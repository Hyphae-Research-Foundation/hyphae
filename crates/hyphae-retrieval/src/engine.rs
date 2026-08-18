// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{
    Abstention, AbstentionReason, RetrievalLimits, RetrievalMatch, RetrievalOutcome,
    RetrievalRequest, VectorRecord,
};

/// Failure to validate or completely execute exact semantic retrieval.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RetrievalError {
    /// Query vectors must be nonempty.
    #[error("query vector must be nonempty")]
    EmptyQueryVector,

    /// Vector dimensions exceed policy.
    #[error("vector dimension {actual} exceeds maximum {maximum}")]
    DimensionLimitExceeded {
        /// Observed dimension.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Candidate dimension differs from the query.
    #[error("candidate dimension {found} does not match query dimension {expected}")]
    DimensionMismatch {
        /// Query dimension.
        expected: usize,
        /// Candidate dimension.
        found: usize,
    },

    /// Query or candidate contains NaN or infinity.
    #[error("vectors must contain only finite values")]
    NonFiniteVector,

    /// Cosine similarity is undefined for a zero vector.
    #[error("vectors must have nonzero magnitude")]
    ZeroVector,

    /// Candidate keys must be nonempty.
    #[error("candidate key must be nonempty")]
    EmptyCandidateKey,

    /// Candidate keys must be globally unique.
    #[error("duplicate global candidate key")]
    DuplicateCandidateKey,

    /// At least one result must be requested.
    #[error("retrieval limit must be nonzero")]
    ZeroLimit,

    /// Requested result count exceeds policy.
    #[error("retrieval limit {requested} exceeds maximum {maximum}")]
    ResultLimitExceeded {
        /// Requested count.
        requested: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Minimum score is non-finite or outside cosine range.
    #[error("minimum score must be finite and in [-1, 1]")]
    InvalidMinimumScore,

    /// Minimum margin is non-finite or outside cosine range width.
    #[error("minimum margin must be finite and in [0, 2]")]
    InvalidMinimumMargin,

    /// Global candidate budget was exhausted.
    #[error("global candidate budget exceeded: {maximum}")]
    CandidateBudgetExceeded {
        /// Configured maximum.
        maximum: u64,
    },

    /// Cooperative monotonic deadline expired.
    #[error("retrieval execution timed out")]
    TimedOut,
}

/// Injectable monotonic clock for deterministic timeout conformance.
pub trait RetrievalClock {
    /// Returns a nondecreasing duration in an arbitrary local epoch.
    fn now(&mut self) -> Duration;
}

/// Production retrieval clock backed by [`Instant`].
#[derive(Debug)]
pub struct RetrievalSystemClock {
    origin: Instant,
}

impl Default for RetrievalSystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl RetrievalClock for RetrievalSystemClock {
    fn now(&mut self) -> Duration {
        self.origin.elapsed()
    }
}

/// Executes exact global cosine retrieval with the system clock.
///
/// # Errors
///
/// Returns an error for invalid vectors or policies, duplicate keys, budgets,
/// dimensions, or timeout. Threshold and ambiguity rejection are successful
/// abstention outcomes.
pub fn retrieve(
    shards: &[&[VectorRecord]],
    request: &RetrievalRequest,
    limits: &RetrievalLimits,
) -> Result<RetrievalOutcome, RetrievalError> {
    retrieve_with_clock(
        shards,
        request,
        limits,
        &mut RetrievalSystemClock::default(),
    )
}

/// Executes exact retrieval with an injectable monotonic clock.
///
/// # Errors
///
/// Returns an error for invalid vectors or policies, duplicate keys, budgets,
/// dimensions, or timeout.
pub fn retrieve_with_clock(
    shards: &[&[VectorRecord]],
    request: &RetrievalRequest,
    limits: &RetrievalLimits,
    clock: &mut impl RetrievalClock,
) -> Result<RetrievalOutcome, RetrievalError> {
    validate_request(request, limits)?;
    let query = NormalizedVector::new(&request.query)?;
    let started = clock.now();
    let deadline = started.checked_add(limits.timeout).unwrap_or(Duration::MAX);
    check_timeout(clock, deadline)?;

    let mut keys = BTreeSet::new();
    let mut ranked = Vec::new();
    let mut scanned = 0_u64;
    for shard in shards {
        for candidate in *shard {
            check_timeout(clock, deadline)?;
            if scanned >= limits.max_candidates {
                return Err(RetrievalError::CandidateBudgetExceeded {
                    maximum: limits.max_candidates,
                });
            }
            scanned = scanned.saturating_add(1);
            if candidate.key.is_empty() {
                return Err(RetrievalError::EmptyCandidateKey);
            }
            if !keys.insert(candidate.key.clone()) {
                return Err(RetrievalError::DuplicateCandidateKey);
            }
            if candidate.vector.len() != query.len() {
                return Err(RetrievalError::DimensionMismatch {
                    expected: query.len(),
                    found: candidate.vector.len(),
                });
            }
            let score = query.cosine_with(&candidate.vector)?;
            ranked.push(RetrievalMatch {
                key: candidate.key.clone(),
                score,
            });
        }
    }
    ranked.sort_by(compare_matches);
    check_timeout(clock, deadline)?;
    Ok(apply_abstention(ranked, scanned, request))
}

fn validate_request(
    request: &RetrievalRequest,
    limits: &RetrievalLimits,
) -> Result<(), RetrievalError> {
    if request.query.is_empty() {
        return Err(RetrievalError::EmptyQueryVector);
    }
    if request.query.len() > limits.max_dimensions {
        return Err(RetrievalError::DimensionLimitExceeded {
            actual: request.query.len(),
            maximum: limits.max_dimensions,
        });
    }
    if request.limit == 0 {
        return Err(RetrievalError::ZeroLimit);
    }
    if request.limit > limits.max_returned {
        return Err(RetrievalError::ResultLimitExceeded {
            requested: request.limit,
            maximum: limits.max_returned,
        });
    }
    if !request.minimum_score.is_finite() || !(-1.0..=1.0).contains(&request.minimum_score) {
        return Err(RetrievalError::InvalidMinimumScore);
    }
    if !request.minimum_margin.is_finite() || !(0.0..=2.0).contains(&request.minimum_margin) {
        return Err(RetrievalError::InvalidMinimumMargin);
    }
    Ok(())
}

fn check_timeout(
    clock: &mut impl RetrievalClock,
    deadline: Duration,
) -> Result<(), RetrievalError> {
    if clock.now() >= deadline {
        Err(RetrievalError::TimedOut)
    } else {
        Ok(())
    }
}

fn compare_matches(left: &RetrievalMatch, right: &RetrievalMatch) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.key.cmp(&right.key))
}

fn apply_abstention(
    ranked: Vec<RetrievalMatch>,
    scanned: u64,
    request: &RetrievalRequest,
) -> RetrievalOutcome {
    let Some(best) = ranked.first() else {
        return RetrievalOutcome::Abstained(Abstention {
            reason: AbstentionReason::NoCandidates,
            best_score: None,
            runner_up_score: None,
            scanned_candidates: scanned,
        });
    };
    let runner_up = ranked.get(1).map(|candidate| candidate.score);
    if best.score < request.minimum_score {
        return RetrievalOutcome::Abstained(Abstention {
            reason: AbstentionReason::BelowThreshold,
            best_score: Some(best.score),
            runner_up_score: runner_up,
            scanned_candidates: scanned,
        });
    }
    if runner_up.is_some_and(|score| best.score - score < request.minimum_margin) {
        return RetrievalOutcome::Abstained(Abstention {
            reason: AbstentionReason::Ambiguous,
            best_score: Some(best.score),
            runner_up_score: runner_up,
            scanned_candidates: scanned,
        });
    }

    RetrievalOutcome::Matches {
        matches: ranked.into_iter().take(request.limit).collect(),
        scanned_candidates: scanned,
    }
}

struct NormalizedVector {
    values: Vec<f64>,
}

impl NormalizedVector {
    fn new(values: &[f64]) -> Result<Self, RetrievalError> {
        let scale = validate_and_scale(values)?;
        let norm = scaled_norm(values, scale);
        Ok(Self {
            values: values.iter().map(|value| (value / scale) / norm).collect(),
        })
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn cosine_with(&self, candidate: &[f64]) -> Result<f64, RetrievalError> {
        let scale = validate_and_scale(candidate)?;
        let norm = scaled_norm(candidate, scale);
        let mut score = 0.0_f64;
        for (query, candidate) in self.values.iter().zip(candidate) {
            score += query * ((candidate / scale) / norm);
        }
        if !score.is_finite() {
            return Err(RetrievalError::NonFiniteVector);
        }
        Ok(score.clamp(-1.0, 1.0))
    }
}

fn validate_and_scale(values: &[f64]) -> Result<f64, RetrievalError> {
    let mut scale = 0.0_f64;
    for value in values {
        if !value.is_finite() {
            return Err(RetrievalError::NonFiniteVector);
        }
        scale = scale.max(value.abs());
    }
    if scale == 0.0 {
        return Err(RetrievalError::ZeroVector);
    }
    Ok(scale)
}

fn scaled_norm(values: &[f64], scale: f64) -> f64 {
    values
        .iter()
        .map(|value| value / scale)
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RetrievalClock, RetrievalError, retrieve, retrieve_with_clock};
    use crate::{
        AbstentionReason, RetrievalLimits, RetrievalOutcome, RetrievalRequest, VectorRecord,
    };

    fn request(limit: usize) -> RetrievalRequest {
        RetrievalRequest {
            query: vec![1.0, 0.0],
            limit,
            minimum_score: -1.0,
            minimum_margin: 0.0,
        }
    }

    #[test]
    fn exact_global_merge_precedes_limit_with_stable_ties() -> Result<(), RetrievalError> {
        let first = vec![
            VectorRecord::new(b"a", vec![1.0, 0.0]),
            VectorRecord::new(b"z", vec![0.0, 1.0]),
        ];
        let second = vec![
            VectorRecord::new(b"b", vec![0.9, 0.1]),
            VectorRecord::new(b"c", vec![0.8, 0.2]),
            VectorRecord::new(b"d", vec![0.8, 0.2]),
        ];
        let outcome = retrieve(
            &[first.as_slice(), second.as_slice()],
            &request(4),
            &RetrievalLimits::default(),
        )?;
        let RetrievalOutcome::Matches { matches, .. } = outcome else {
            return Err(RetrievalError::TimedOut);
        };
        assert_eq!(
            matches
                .iter()
                .map(|candidate| candidate.key.as_slice())
                .collect::<Vec<_>>(),
            [
                b"a".as_slice(),
                b"b".as_slice(),
                b"c".as_slice(),
                b"d".as_slice()
            ]
        );
        Ok(())
    }

    #[test]
    fn threshold_and_margin_produce_explicit_abstention() -> Result<(), RetrievalError> {
        let candidates = vec![
            VectorRecord::new(b"a", vec![1.0, 0.0]),
            VectorRecord::new(b"b", vec![0.99, 0.01]),
        ];
        let ambiguous = retrieve(
            &[candidates.as_slice()],
            &RetrievalRequest {
                minimum_margin: 0.01,
                ..request(2)
            },
            &RetrievalLimits::default(),
        )?;
        assert!(matches!(
            ambiguous,
            RetrievalOutcome::Abstained(crate::Abstention {
                reason: AbstentionReason::Ambiguous,
                ..
            })
        ));

        let weak = vec![VectorRecord::new(b"weak", vec![0.0, 1.0])];
        let below = retrieve(
            &[weak.as_slice()],
            &RetrievalRequest {
                minimum_score: 0.5,
                ..request(1)
            },
            &RetrievalLimits::default(),
        )?;
        assert!(matches!(
            below,
            RetrievalOutcome::Abstained(crate::Abstention {
                reason: AbstentionReason::BelowThreshold,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn no_candidates_is_a_normal_abstention() -> Result<(), RetrievalError> {
        let empty: [VectorRecord; 0] = [];
        let outcome = retrieve(
            &[empty.as_slice()],
            &request(1),
            &RetrievalLimits::default(),
        )?;
        assert!(matches!(
            outcome,
            RetrievalOutcome::Abstained(crate::Abstention {
                reason: AbstentionReason::NoCandidates,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn dimensions_nonfinite_zero_and_duplicates_fail_loudly() {
        let wrong = vec![VectorRecord::new(b"wrong", vec![1.0])];
        assert_eq!(
            retrieve(
                &[wrong.as_slice()],
                &request(1),
                &RetrievalLimits::default()
            ),
            Err(RetrievalError::DimensionMismatch {
                expected: 2,
                found: 1
            })
        );

        let nonfinite = vec![VectorRecord::new(b"bad", vec![f64::NAN, 0.0])];
        assert_eq!(
            retrieve(
                &[nonfinite.as_slice()],
                &request(1),
                &RetrievalLimits::default()
            ),
            Err(RetrievalError::NonFiniteVector)
        );

        let zero = vec![VectorRecord::new(b"zero", vec![0.0, 0.0])];
        assert_eq!(
            retrieve(&[zero.as_slice()], &request(1), &RetrievalLimits::default()),
            Err(RetrievalError::ZeroVector)
        );

        let first = vec![VectorRecord::new(b"same", vec![1.0, 0.0])];
        let second = vec![VectorRecord::new(b"same", vec![0.0, 1.0])];
        assert_eq!(
            retrieve(
                &[first.as_slice(), second.as_slice()],
                &request(1),
                &RetrievalLimits::default()
            ),
            Err(RetrievalError::DuplicateCandidateKey)
        );
    }

    #[test]
    fn extreme_finite_components_do_not_overflow_cosine() -> Result<(), RetrievalError> {
        let candidates = vec![VectorRecord::new(b"same", vec![f64::MAX, f64::MAX])];
        let outcome = retrieve(
            &[candidates.as_slice()],
            &RetrievalRequest {
                query: vec![f64::MAX, f64::MAX],
                ..request(1)
            },
            &RetrievalLimits::default(),
        )?;
        let RetrievalOutcome::Matches { matches, .. } = outcome else {
            return Err(RetrievalError::TimedOut);
        };
        assert!((matches[0].score - 1.0).abs() <= 1.0e-12);
        Ok(())
    }

    struct StepClock {
        current: Duration,
        step: Duration,
    }

    impl RetrievalClock for StepClock {
        fn now(&mut self) -> Duration {
            let current = self.current;
            self.current = self.current.saturating_add(self.step);
            current
        }
    }

    #[test]
    fn budgets_and_timeout_are_global() {
        let candidates = vec![
            VectorRecord::new(b"a", vec![1.0, 0.0]),
            VectorRecord::new(b"b", vec![0.0, 1.0]),
        ];
        let budget = RetrievalLimits {
            max_candidates: 1,
            ..RetrievalLimits::default()
        };
        assert_eq!(
            retrieve(&[candidates.as_slice()], &request(1), &budget),
            Err(RetrievalError::CandidateBudgetExceeded { maximum: 1 })
        );

        let timeout = RetrievalLimits {
            timeout: Duration::from_millis(3),
            ..RetrievalLimits::default()
        };
        let mut clock = StepClock {
            current: Duration::ZERO,
            step: Duration::from_millis(1),
        };
        assert_eq!(
            retrieve_with_clock(&[candidates.as_slice()], &request(1), &timeout, &mut clock),
            Err(RetrievalError::TimedOut)
        );
    }
}
