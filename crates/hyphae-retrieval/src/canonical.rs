// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    time::{Duration, Instant},
};

use hyphae_core::{Q15Vector, SCORE_NANOS_SCALE, VectorSpaceName};
use thiserror::Error;

/// One durable exact-retrieval candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableVectorRecord {
    /// Globally unique nonempty object key within the selected space.
    pub key: Vec<u8>,
    /// Canonical signed-Q15 vector.
    pub vector: Q15Vector,
}

/// Durable exact-retrieval request under semantics v2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalRequest {
    /// Canonical named vector space.
    pub vector_space: VectorSpaceName,
    /// Canonical signed-Q15 query.
    pub query: Q15Vector,
    /// Maximum returned matches.
    pub limit: usize,
    /// Inclusive minimum canonical score.
    pub minimum_score_nanos: i64,
    /// Minimum canonical best/runner-up margin.
    pub minimum_margin_nanos: u64,
}

/// Runtime and shape limits for durable exact retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalLimits {
    /// Maximum candidates inspected.
    pub max_candidates: u64,
    /// Maximum aggregate candidate key and vector bytes loaded.
    pub max_candidate_bytes: u64,
    /// Maximum requested results.
    pub max_returned: usize,
    /// Cooperative timeout.
    pub timeout: Duration,
}

impl Default for ExactRetrievalLimits {
    fn default() -> Self {
        Self {
            max_candidates: 100_000,
            max_candidate_bytes: 256 * 1024 * 1024,
            max_returned: 1_000,
            timeout: Duration::from_secs(30),
        }
    }
}

/// One canonical exact match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalMatch {
    /// Binary object key.
    pub key: Vec<u8>,
    /// Canonical integer cosine score.
    pub score_nanos: i64,
}

/// Complete durable exact-retrieval outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExactRetrievalOutcome {
    /// Accepted globally ranked matches.
    Matches {
        /// Final matches.
        matches: Vec<ExactRetrievalMatch>,
        /// Global candidates inspected.
        scanned_candidates: u64,
    },
    /// Typed normal abstention.
    Abstained(ExactAbstention),
}

/// Stable durable exact-retrieval abstention evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactAbstention {
    /// Stable reason.
    pub reason: ExactAbstentionReason,
    /// Best canonical score when present.
    pub best_score_nanos: Option<i64>,
    /// Runner-up canonical score when present.
    pub runner_up_score_nanos: Option<i64>,
    /// Global candidates inspected.
    pub scanned_candidates: u64,
}

/// Stable reason why durable exact retrieval declined matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactAbstentionReason {
    /// The selected space contains no vectors.
    NoCandidates,
    /// The best score is below the request threshold.
    BelowThreshold,
    /// The best/runner-up gap is below the request margin.
    Ambiguous,
}

/// Failure to execute durable exact retrieval completely.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ExactRetrievalError {
    /// Candidate keys must be nonempty.
    #[error("candidate key must be nonempty")]
    EmptyCandidateKey,
    /// Candidate keys must be unique in one space.
    #[error("duplicate candidate key")]
    DuplicateCandidateKey,
    /// Candidate and query dimensions must match.
    #[error("candidate dimension {found} does not match query dimension {expected}")]
    DimensionMismatch {
        /// Query dimension.
        expected: u16,
        /// Candidate dimension.
        found: u16,
    },
    /// At least one match must be requested.
    #[error("retrieval limit must be nonzero")]
    ZeroLimit,
    /// The requested result count exceeds policy.
    #[error("retrieval limit {requested} exceeds maximum {maximum}")]
    ResultLimitExceeded {
        /// Requested result count.
        requested: usize,
        /// Maximum result count.
        maximum: usize,
    },
    /// The score threshold is outside the canonical cosine range.
    #[error("minimum score nanos must be in [-1000000000, 1000000000]")]
    InvalidMinimumScore,
    /// The margin is outside the canonical cosine range width.
    #[error("minimum margin nanos must be in [0, 2000000000]")]
    InvalidMinimumMargin,
    /// The global candidate budget was exhausted.
    #[error("global candidate budget exceeded: {maximum}")]
    CandidateBudgetExceeded {
        /// Maximum candidates.
        maximum: u64,
    },
    /// The aggregate candidate key/vector byte budget was exhausted.
    #[error("global candidate byte budget exceeded: {maximum}")]
    CandidateByteBudgetExceeded {
        /// Maximum aggregate candidate bytes.
        maximum: u64,
    },
    /// The cooperative timeout elapsed.
    #[error("retrieval execution timed out")]
    TimedOut,
    /// Checked integer arithmetic failed.
    #[error("canonical score arithmetic overflow")]
    ArithmeticOverflow,
}

/// Monotonic clock used by the canonical executor.
pub trait ExactRetrievalClock {
    /// Returns a nondecreasing local duration.
    fn now(&mut self) -> Duration;
}

/// Executes durable exact retrieval using the process monotonic clock.
///
/// # Errors
///
/// Returns the same complete-or-error outcomes as
/// [`retrieve_exact_with_clock`].
pub fn retrieve_exact(
    candidates: &[DurableVectorRecord],
    request: &ExactRetrievalRequest,
    limits: &ExactRetrievalLimits,
) -> Result<ExactRetrievalOutcome, ExactRetrievalError> {
    retrieve_exact_with_clock(
        candidates,
        request,
        limits,
        &mut SystemClock {
            started: Instant::now(),
        },
    )
}

struct SystemClock {
    started: Instant,
}

impl ExactRetrievalClock for SystemClock {
    fn now(&mut self) -> Duration {
        self.started.elapsed()
    }
}

/// Executes durable exact retrieval using a caller-supplied monotonic clock.
///
/// # Errors
///
/// Returns an error for invalid input, dimensions, budget, timeout, duplicate
/// keys, or arithmetic overflow. Errors return no partial ranking.
pub fn retrieve_exact_with_clock(
    candidates: &[DurableVectorRecord],
    request: &ExactRetrievalRequest,
    limits: &ExactRetrievalLimits,
    clock: &mut impl ExactRetrievalClock,
) -> Result<ExactRetrievalOutcome, ExactRetrievalError> {
    validate_request(request, limits)?;
    let started = clock.now();
    let deadline = started.checked_add(limits.timeout).unwrap_or(Duration::MAX);
    check_timeout(clock, deadline)?;

    let mut keys = BTreeSet::new();
    let mut ranked = Vec::with_capacity(candidates.len().min(request.limit));
    let mut scanned = 0_u64;
    let mut scanned_bytes = 0_u64;
    for candidate in candidates {
        check_timeout(clock, deadline)?;
        if scanned >= limits.max_candidates {
            return Err(ExactRetrievalError::CandidateBudgetExceeded {
                maximum: limits.max_candidates,
            });
        }
        scanned = scanned.saturating_add(1);
        let candidate_bytes = u64::try_from(candidate.key.len())
            .ok()
            .and_then(|key_bytes| {
                u64::try_from(candidate.vector.as_slice().len())
                    .ok()
                    .and_then(|elements| elements.checked_mul(2))
                    .and_then(|vector_bytes| key_bytes.checked_add(vector_bytes))
            })
            .ok_or(ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            })?;
        scanned_bytes = scanned_bytes.checked_add(candidate_bytes).ok_or(
            ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            },
        )?;
        if scanned_bytes > limits.max_candidate_bytes {
            return Err(ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            });
        }
        if candidate.key.is_empty() {
            return Err(ExactRetrievalError::EmptyCandidateKey);
        }
        if !keys.insert(candidate.key.as_slice()) {
            return Err(ExactRetrievalError::DuplicateCandidateKey);
        }
        if candidate.vector.dimension() != request.query.dimension() {
            return Err(ExactRetrievalError::DimensionMismatch {
                expected: request.query.dimension(),
                found: candidate.vector.dimension(),
            });
        }
        ranked.push(ExactRetrievalMatch {
            key: candidate.key.clone(),
            score_nanos: cosine_score_nanos(&request.query, &candidate.vector)?,
        });
    }
    ranked.sort_by(compare_matches);
    check_timeout(clock, deadline)?;
    Ok(apply_policy(ranked, scanned, request))
}

/// Computes the canonical integer cosine score from ADR-0015.
///
/// # Errors
///
/// Returns an arithmetic error if checked accumulation exceeds `u128`/`i128`.
pub fn cosine_score_nanos(left: &Q15Vector, right: &Q15Vector) -> Result<i64, ExactRetrievalError> {
    if left.dimension() != right.dimension() {
        return Err(ExactRetrievalError::DimensionMismatch {
            expected: left.dimension(),
            found: right.dimension(),
        });
    }
    let mut dot = 0_i128;
    let mut left_squared = 0_u128;
    let mut right_squared = 0_u128;
    for (left_value, right_value) in left.as_slice().iter().zip(right.as_slice()) {
        let left_value = i128::from(*left_value);
        let right_value = i128::from(*right_value);
        dot = dot
            .checked_add(
                left_value
                    .checked_mul(right_value)
                    .ok_or(ExactRetrievalError::ArithmeticOverflow)?,
            )
            .ok_or(ExactRetrievalError::ArithmeticOverflow)?;
        left_squared = left_squared
            .checked_add(
                u128::try_from(left_value * left_value)
                    .map_err(|_| ExactRetrievalError::ArithmeticOverflow)?,
            )
            .ok_or(ExactRetrievalError::ArithmeticOverflow)?;
        right_squared = right_squared
            .checked_add(
                u128::try_from(right_value * right_value)
                    .map_err(|_| ExactRetrievalError::ArithmeticOverflow)?,
            )
            .ok_or(ExactRetrievalError::ArithmeticOverflow)?;
    }
    let norm_product = left_squared
        .checked_mul(right_squared)
        .ok_or(ExactRetrievalError::ArithmeticOverflow)?;
    let denominator = integer_sqrt(norm_product);
    if denominator == 0 {
        return Err(ExactRetrievalError::ArithmeticOverflow);
    }
    let absolute_dot = dot.unsigned_abs();
    let score_scale =
        u128::try_from(SCORE_NANOS_SCALE).map_err(|_| ExactRetrievalError::ArithmeticOverflow)?;
    let numerator = absolute_dot
        .checked_mul(score_scale)
        .and_then(|value| value.checked_add(denominator / 2))
        .ok_or(ExactRetrievalError::ArithmeticOverflow)?;
    let magnitude = (numerator / denominator).min(score_scale);
    let magnitude =
        i64::try_from(magnitude).map_err(|_| ExactRetrievalError::ArithmeticOverflow)?;
    Ok(if dot < 0 { -magnitude } else { magnitude })
}

fn validate_request(
    request: &ExactRetrievalRequest,
    limits: &ExactRetrievalLimits,
) -> Result<(), ExactRetrievalError> {
    if request.limit == 0 {
        return Err(ExactRetrievalError::ZeroLimit);
    }
    if request.limit > limits.max_returned {
        return Err(ExactRetrievalError::ResultLimitExceeded {
            requested: request.limit,
            maximum: limits.max_returned,
        });
    }
    if !(-SCORE_NANOS_SCALE..=SCORE_NANOS_SCALE).contains(&request.minimum_score_nanos) {
        return Err(ExactRetrievalError::InvalidMinimumScore);
    }
    let maximum_margin = u64::try_from(SCORE_NANOS_SCALE.saturating_mul(2))
        .map_err(|_| ExactRetrievalError::InvalidMinimumMargin)?;
    if request.minimum_margin_nanos > maximum_margin {
        return Err(ExactRetrievalError::InvalidMinimumMargin);
    }
    Ok(())
}

fn check_timeout(
    clock: &mut impl ExactRetrievalClock,
    deadline: Duration,
) -> Result<(), ExactRetrievalError> {
    if clock.now() >= deadline {
        Err(ExactRetrievalError::TimedOut)
    } else {
        Ok(())
    }
}

fn compare_matches(left: &ExactRetrievalMatch, right: &ExactRetrievalMatch) -> Ordering {
    right
        .score_nanos
        .cmp(&left.score_nanos)
        .then_with(|| left.key.cmp(&right.key))
}

fn apply_policy(
    ranked: Vec<ExactRetrievalMatch>,
    scanned: u64,
    request: &ExactRetrievalRequest,
) -> ExactRetrievalOutcome {
    let Some(best) = ranked.first() else {
        return ExactRetrievalOutcome::Abstained(ExactAbstention {
            reason: ExactAbstentionReason::NoCandidates,
            best_score_nanos: None,
            runner_up_score_nanos: None,
            scanned_candidates: scanned,
        });
    };
    let runner_up = ranked.get(1).map(|candidate| candidate.score_nanos);
    if best.score_nanos < request.minimum_score_nanos {
        return ExactRetrievalOutcome::Abstained(ExactAbstention {
            reason: ExactAbstentionReason::BelowThreshold,
            best_score_nanos: Some(best.score_nanos),
            runner_up_score_nanos: runner_up,
            scanned_candidates: scanned,
        });
    }
    if runner_up.is_some_and(|score| {
        let margin = best.score_nanos.saturating_sub(score);
        u64::try_from(margin).unwrap_or_default() < request.minimum_margin_nanos
    }) {
        return ExactRetrievalOutcome::Abstained(ExactAbstention {
            reason: ExactAbstentionReason::Ambiguous,
            best_score_nanos: Some(best.score_nanos),
            runner_up_score_nanos: runner_up,
            scanned_candidates: scanned,
        });
    }
    ExactRetrievalOutcome::Matches {
        matches: ranked.into_iter().take(request.limit).collect(),
        scanned_candidates: scanned,
    }
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut lower = 1_u128;
    let mut upper = (value >> 1).saturating_add(1);
    while lower <= upper {
        let middle = lower + ((upper - lower) >> 1);
        match middle.checked_mul(middle) {
            Some(square) if square == value => return middle,
            Some(square) if square < value => lower = middle.saturating_add(1),
            _ => upper = middle.saturating_sub(1),
        }
    }
    upper
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hyphae_core::{Q15Vector, VectorSpaceName};

    use super::{
        DurableVectorRecord, ExactAbstentionReason, ExactRetrievalClock, ExactRetrievalLimits,
        ExactRetrievalOutcome, ExactRetrievalRequest, cosine_score_nanos,
        retrieve_exact_with_clock,
    };

    struct StepClock(Duration);

    impl ExactRetrievalClock for StepClock {
        fn now(&mut self) -> Duration {
            let current = self.0;
            self.0 = self.0.saturating_add(Duration::from_millis(1));
            current
        }
    }

    fn request() -> Result<ExactRetrievalRequest, hyphae_core::VectorValueError> {
        Ok(ExactRetrievalRequest {
            vector_space: VectorSpaceName::new("semantic")?,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 3,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        })
    }

    #[test]
    fn canonical_scores_cover_same_orthogonal_and_opposite()
    -> Result<(), Box<dyn std::error::Error>> {
        let query = Q15Vector::new(vec![32_767, 0])?;
        assert_eq!(
            cosine_score_nanos(&query, &Q15Vector::new(vec![32_767, 0])?)?,
            1_000_000_000
        );
        assert_eq!(
            cosine_score_nanos(&query, &Q15Vector::new(vec![0, 32_767])?)?,
            0
        );
        assert_eq!(
            cosine_score_nanos(&query, &Q15Vector::new(vec![-32_767, 0])?)?,
            -1_000_000_000
        );
        Ok(())
    }

    #[test]
    fn exact_ranking_uses_score_then_binary_key() -> Result<(), Box<dyn std::error::Error>> {
        let candidates = vec![
            DurableVectorRecord {
                key: vec![0xff],
                vector: Q15Vector::new(vec![7, 7])?,
            },
            DurableVectorRecord {
                key: vec![0],
                vector: Q15Vector::new(vec![2, 2])?,
            },
        ];
        let mut tied = request()?;
        tied.query = Q15Vector::new(vec![1, 1])?;
        tied.limit = 2;
        let outcome = retrieve_exact_with_clock(
            &candidates,
            &tied,
            &ExactRetrievalLimits::default(),
            &mut StepClock(Duration::ZERO),
        )?;
        let ExactRetrievalOutcome::Matches { matches, .. } = outcome else {
            return Err("unexpected abstention".into());
        };
        assert_eq!(matches[0].key, vec![0]);
        assert_eq!(matches[1].key, vec![0xff]);
        Ok(())
    }

    #[test]
    fn empty_space_is_typed_abstention() -> Result<(), Box<dyn std::error::Error>> {
        let outcome = retrieve_exact_with_clock(
            &[],
            &request()?,
            &ExactRetrievalLimits::default(),
            &mut StepClock(Duration::ZERO),
        )?;
        assert!(matches!(
            outcome,
            ExactRetrievalOutcome::Abstained(super::ExactAbstention {
                reason: ExactAbstentionReason::NoCandidates,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn sparse_near_ties_are_distinct_and_obey_margin_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let candidates = vec![
            DurableVectorRecord {
                key: b"near-a".to_vec(),
                vector: Q15Vector::new(vec![32_767, 16, 0, 0, 0, 0, 0, 0])?,
            },
            DurableVectorRecord {
                key: b"near-b".to_vec(),
                vector: Q15Vector::new(vec![32_767, 32, 0, 0, 0, 0, 0, 0])?,
            },
        ];
        let mut near_tie = request()?;
        near_tie.query = Q15Vector::new(vec![32_767, 0, 0, 0, 0, 0, 0, 0])?;
        near_tie.limit = 2;
        let ranked = retrieve_exact_with_clock(
            &candidates,
            &near_tie,
            &ExactRetrievalLimits::default(),
            &mut StepClock(Duration::ZERO),
        )?;
        let ExactRetrievalOutcome::Matches { matches, .. } = ranked else {
            return Err("unexpected abstention".into());
        };
        assert_eq!(matches[0].key, b"near-a");
        assert_eq!(matches[1].key, b"near-b");
        let margin = u64::try_from(matches[0].score_nanos - matches[1].score_nanos)?;
        assert!(margin > 0);

        near_tie.minimum_margin_nanos = margin.saturating_add(1);
        let abstained = retrieve_exact_with_clock(
            &candidates,
            &near_tie,
            &ExactRetrievalLimits::default(),
            &mut StepClock(Duration::ZERO),
        )?;
        assert!(matches!(
            abstained,
            ExactRetrievalOutcome::Abstained(super::ExactAbstention {
                reason: ExactAbstentionReason::Ambiguous,
                ..
            })
        ));
        Ok(())
    }
}
