// SPDX-License-Identifier: Apache-2.0

//! Deterministic 8-bit scalar quantization for approximate distances.
//!
//! The quantizer folds one exact global `(minimum, range)` pair over the
//! training vectors in input order and encodes each component to one byte
//! plus two retained sums, so asymmetric distances never decode the
//! original floats. All arithmetic is `f64` in deterministic input order;
//! the primitive is audited for a future compressed traversal mode and is
//! not yet part of the durable index format.

use crate::{AnnError, Metric, Vector};

/// Quantization codes per component (8-bit).
const CODES: f64 = 255.0;

/// One trained 8-bit scalar quantizer.
#[derive(Clone, Debug, PartialEq)]
pub struct Sq8Quantizer {
    dimension: u16,
    metric: Metric,
    /// Global minimum across every training component.
    minimum: f64,
    /// Global range across every training component (`> 0`, finite).
    range: f64,
    /// `range² / 255²`.
    a2: f64,
    /// `range · minimum / 255`.
    ab: f64,
    /// `minimum² · dimension`.
    ib2: f64,
}

/// One encoded vector: one byte per component plus retained sums.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sq8Code {
    codes: Vec<u8>,
    /// Σ code.
    sum: u64,
    /// Σ code².
    sum_squared: u64,
}

impl Sq8Code {
    /// Encoded component bytes.
    #[must_use]
    pub fn codes(&self) -> &[u8] {
        &self.codes
    }

    /// Retained code sum.
    #[must_use]
    pub const fn sum(&self) -> u64 {
        self.sum
    }

    /// Retained squared-code sum.
    #[must_use]
    pub const fn sum_squared(&self) -> u64 {
        self.sum_squared
    }
}

impl Sq8Quantizer {
    /// Trains one quantizer over the training vectors in input order.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::InvalidQuantizerTraining`] for empty training
    /// data, a dimension mismatch, or a zero or non-finite global range.
    pub fn train(dimension: u16, metric: Metric, training: &[Vector]) -> Result<Self, AnnError> {
        if training.is_empty() {
            return Err(AnnError::InvalidQuantizerTraining);
        }
        let mut minimum = f64::INFINITY;
        let mut maximum = f64::NEG_INFINITY;
        for vector in training {
            if vector.dimension() != usize::from(dimension) {
                return Err(AnnError::InvalidQuantizerTraining);
            }
            for value in vector.values() {
                let value = f64::from(*value);
                if value < minimum {
                    minimum = value;
                }
                if value > maximum {
                    maximum = value;
                }
            }
        }
        let range = maximum - minimum;
        if !range.is_finite() || range <= 0.0 {
            return Err(AnnError::InvalidQuantizerTraining);
        }
        Ok(Self {
            dimension,
            metric,
            minimum,
            range,
            a2: range * range / (CODES * CODES),
            ab: range * minimum / CODES,
            ib2: minimum * minimum * f64::from(dimension),
        })
    }

    /// Trained dimension.
    #[must_use]
    pub const fn dimension(&self) -> u16 {
        self.dimension
    }

    /// Trained metric.
    #[must_use]
    pub const fn metric(&self) -> Metric {
        self.metric
    }

    /// Encodes one vector to codes plus retained sums.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::DimensionMismatch`] for the wrong dimension.
    pub fn encode(&self, vector: &Vector) -> Result<Sq8Code, AnnError> {
        if vector.dimension() != usize::from(self.dimension) {
            return Err(AnnError::DimensionMismatch);
        }
        let mut codes = Vec::with_capacity(vector.dimension());
        let mut sum = 0_u64;
        let mut sum_squared = 0_u64;
        for value in vector.values() {
            let value = f64::from(*value);
            let code = if value < self.minimum {
                0_u8
            } else if value - self.minimum > self.range {
                u8::MAX
            } else {
                // The clamp keeps the floor inside 0..=255 for every
                // finite admitted component.
                let scaled = ((value - self.minimum) * CODES / self.range).floor();
                if scaled >= CODES {
                    u8::MAX
                } else if scaled <= 0.0 {
                    0
                } else {
                    // Bounded by the branches above.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        scaled as u8
                    }
                }
            };
            codes.push(code);
            sum += u64::from(code);
            sum_squared += u64::from(code) * u64::from(code);
        }
        Ok(Sq8Code {
            codes,
            sum,
            sum_squared,
        })
    }

    /// Approximate distance between two encoded vectors under the trained
    /// metric, without decoding.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::DimensionMismatch`] for misaligned codes or
    /// [`AnnError::ZeroCosineVector`] for a zero approximate cosine norm.
    pub fn distance(&self, left: &Sq8Code, right: &Sq8Code) -> Result<f64, AnnError> {
        if left.codes.len() != right.codes.len() || left.codes.len() != usize::from(self.dimension)
        {
            return Err(AnnError::DimensionMismatch);
        }
        match self.metric {
            Metric::SquaredL2 => {
                let mut total = 0_u64;
                for (left, right) in left.codes.iter().zip(&right.codes) {
                    let delta = i64::from(*left) - i64::from(*right);
                    total += delta.unsigned_abs() * delta.unsigned_abs();
                }
                Ok(self.a2 * sum_as_f64(total))
            }
            Metric::NegativeDot => Ok(-self.approximate_dot(left, right)),
            Metric::Cosine => {
                let left_norm = self.approximate_norm_squared(left);
                let right_norm = self.approximate_norm_squared(right);
                if left_norm <= 0.0 || right_norm <= 0.0 {
                    return Err(AnnError::ZeroCosineVector);
                }
                Ok(
                    1.0 - self.approximate_dot(left, right)
                        / (left_norm.sqrt() * right_norm.sqrt()),
                )
            }
        }
    }

    /// Approximate dot product from codes and retained sums.
    fn approximate_dot(&self, left: &Sq8Code, right: &Sq8Code) -> f64 {
        let mut dot = 0_u64;
        for (left, right) in left.codes.iter().zip(&right.codes) {
            dot += u64::from(*left) * u64::from(*right);
        }
        self.a2 * sum_as_f64(dot)
            + self.ab * (sum_as_f64(left.sum) + sum_as_f64(right.sum))
            + self.ib2
    }

    /// Approximate squared norm from the retained sums alone.
    fn approximate_norm_squared(&self, code: &Sq8Code) -> f64 {
        self.a2 * sum_as_f64(code.sum_squared) + 2.0 * self.ab * sum_as_f64(code.sum) + self.ib2
    }
}

/// Deterministic u64 -> f64 through two u32 halves.
fn sum_as_f64(value: u64) -> f64 {
    let upper = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let lower = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(upper) * 4_294_967_296.0 + f64::from(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(values: &[f32]) -> Result<Vector, AnnError> {
        Vector::new(values.to_vec())
    }

    #[test]
    fn encoding_is_deterministic_and_clamps_out_of_range_queries() -> Result<(), AnnError> {
        let training = [vector(&[0.0, 1.0])?, vector(&[2.0, 3.0])?];
        let quantizer = Sq8Quantizer::train(2, Metric::SquaredL2, &training)?;
        // b = 0, a = 3: 0 -> 0, 3 -> 255, 1.5 -> floor(127.5) = 127.
        let code = quantizer.encode(&vector(&[1.5, 3.0])?)?;
        assert_eq!(code.codes(), &[127, 255]);
        assert_eq!(code.sum(), 382);
        assert_eq!(code.sum_squared(), 127 * 127 + 255 * 255);
        // Out-of-range queries clamp instead of failing: they were never
        // training data.
        let clamped = quantizer.encode(&vector(&[-10.0, 10.0])?)?;
        assert_eq!(clamped.codes(), &[0, 255]);
        Ok(())
    }

    #[test]
    fn squared_l2_approximation_matches_the_exact_scale() -> Result<(), AnnError> {
        let training = [vector(&[0.0, 0.0])?, vector(&[10.0, 10.0])?];
        let quantizer = Sq8Quantizer::train(2, Metric::SquaredL2, &training)?;
        let origin = quantizer.encode(&vector(&[0.0, 0.0])?)?;
        let far = quantizer.encode(&vector(&[10.0, 10.0])?)?;
        let approximate = quantizer.distance(&origin, &far)?;
        // Exact squared L2 is 200; the 8-bit approximation stays within
        // one quantization step per component.
        assert!(
            (approximate - 200.0).abs() < 2.0,
            "approximate {approximate}"
        );
        Ok(())
    }

    #[test]
    fn cosine_distances_are_finite_or_fail_closed() -> Result<(), AnnError> {
        let training = [vector(&[-1.0, -1.0])?, vector(&[1.0, 1.0])?];
        let quantizer = Sq8Quantizer::train(2, Metric::Cosine, &training)?;
        let zeroish = quantizer.encode(&vector(&[0.0, 0.0])?)?;
        let other = quantizer.encode(&vector(&[1.0, 1.0])?)?;
        // Norm is approximately zero only if a2·Σc² + 2ab·Σc + ib2 <= 0.
        match quantizer.distance(&zeroish, &other) {
            Ok(distance) => assert!(distance.is_finite()),
            Err(AnnError::ZeroCosineVector) => {}
            Err(other) => return Err(other),
        }
        Ok(())
    }

    #[test]
    fn training_fails_closed_on_empty_misaligned_or_flat_data() -> Result<(), AnnError> {
        assert!(Sq8Quantizer::train(2, Metric::SquaredL2, &[]).is_err());
        let misaligned = [vector(&[0.0, 1.0])?, vector(&[0.0, 1.0, 2.0])?];
        assert!(Sq8Quantizer::train(2, Metric::SquaredL2, &misaligned).is_err());
        let flat = [vector(&[1.0, 1.0])?, vector(&[1.0, 1.0])?];
        assert!(Sq8Quantizer::train(2, Metric::SquaredL2, &flat).is_err());
        Ok(())
    }

    #[test]
    fn distances_reject_misaligned_codes() -> Result<(), AnnError> {
        let training = [vector(&[0.0, 1.0])?, vector(&[2.0, 3.0])?];
        let quantizer = Sq8Quantizer::train(2, Metric::SquaredL2, &training)?;
        let narrow_training = [vector(&[0.0])?, vector(&[1.0])?];
        let narrow = Sq8Quantizer::train(1, Metric::SquaredL2, &narrow_training)?;
        let wide = quantizer.encode(&vector(&[1.0, 2.0])?)?;
        let short = narrow.encode(&vector(&[0.5])?)?;
        assert!(matches!(
            quantizer.distance(&wide, &short),
            Err(AnnError::DimensionMismatch)
        ));
        Ok(())
    }
}
