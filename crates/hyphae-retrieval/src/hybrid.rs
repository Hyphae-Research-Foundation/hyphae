// SPDX-License-Identifier: Apache-2.0

//! Deterministic reciprocal-rank fusion under hybrid semantics v1.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::{
    ExactAbstentionReason, ExactRetrievalOutcome, LexicalAbstentionReason, LexicalOutcome,
};

/// Fixed RRF rank constant.
pub const HYBRID_RRF_CONSTANT: u64 = 60;
const CONTRIBUTION_SCALE: u64 = 1_000_000_000;
const MAX_WEIGHT: u32 = 1_000_000;

/// Complete fusion request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridRequest {
    /// Positive lexical branch weight.
    pub lexical_weight: u32,
    /// Positive exact-vector branch weight.
    pub vector_weight: u32,
    /// Maximum returned fused matches.
    pub limit: usize,
}

/// Preserved reason for an absent branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HybridBranchAbsence {
    /// Lexical branch had no candidates.
    LexicalNoCandidates,
    /// Exact branch had no candidates.
    VectorNoCandidates,
    /// Exact branch was below its threshold.
    VectorBelowThreshold,
    /// Exact branch was ambiguous under margin policy.
    VectorAmbiguous,
}

/// Full per-result fusion explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridExplanation {
    /// One-based lexical rank.
    pub lexical_rank: Option<u64>,
    /// Canonical BM25F score.
    pub lexical_score_nanos: Option<i64>,
    /// One-based exact-vector rank.
    pub vector_rank: Option<u64>,
    /// Canonical integer cosine score.
    pub vector_score_nanos: Option<i64>,
    /// Integer lexical contribution.
    pub lexical_contribution: u64,
    /// Integer vector contribution.
    pub vector_contribution: u64,
    /// Checked contribution sum.
    pub fusion_score: u64,
    /// One-based final rank.
    pub final_rank: u64,
}

/// One canonical fused match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridMatch {
    /// Binary object key.
    pub key: Vec<u8>,
    /// Explainable fusion components.
    pub explanation: HybridExplanation,
}

/// Both branches abstained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridAbstention {
    /// Lexical reason.
    pub lexical: HybridBranchAbsence,
    /// Exact-vector reason.
    pub vector: HybridBranchAbsence,
}

/// Complete hybrid outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HybridOutcome {
    /// Fused or explicit single-modality matches.
    Matches {
        /// Final matches.
        matches: Vec<HybridMatch>,
        /// Preserved lexical absence.
        lexical_absence: Option<HybridBranchAbsence>,
        /// Preserved vector absence.
        vector_absence: Option<HybridBranchAbsence>,
    },
    /// Both branches abstained.
    Abstained(HybridAbstention),
}

/// Fusion failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HybridError {
    /// Weights must be positive and bounded.
    #[error("hybrid branch weights must be in 1..=1000000")]
    InvalidWeight,
    /// At least one result must be requested.
    #[error("hybrid result limit must be nonzero")]
    ZeroLimit,
    /// A branch unexpectedly repeats one key.
    #[error("hybrid branch contains a duplicate key")]
    DuplicateBranchKey,
    /// Checked integer arithmetic failed.
    #[error("hybrid contribution arithmetic overflow")]
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Default)]
struct Accumulator {
    lexical_rank: Option<u64>,
    lexical_score_nanos: Option<i64>,
    vector_rank: Option<u64>,
    vector_score_nanos: Option<i64>,
}

/// Fuses complete branch outcomes with deterministic RRF.
///
/// # Errors
///
/// Returns invalid-input, duplicate-key, or arithmetic failure with no
/// partial result.
pub fn fuse_hybrid(
    lexical: &LexicalOutcome,
    vector: &ExactRetrievalOutcome,
    request: &HybridRequest,
) -> Result<HybridOutcome, HybridError> {
    validate_request(request)?;
    let (lexical_matches, lexical_absence) = lexical_branch(lexical);
    let (vector_matches, vector_absence) = vector_branch(vector);
    if let (Some(lexical), Some(vector)) = (lexical_absence, vector_absence) {
        return Ok(HybridOutcome::Abstained(HybridAbstention {
            lexical,
            vector,
        }));
    }
    let mut combined = BTreeMap::<Vec<u8>, Accumulator>::new();
    if let Some(matches) = lexical_matches {
        for (index, matched) in matches.iter().enumerate() {
            let rank = one_based(index)?;
            let entry = combined.entry(matched.key.clone()).or_default();
            if entry.lexical_rank.replace(rank).is_some() {
                return Err(HybridError::DuplicateBranchKey);
            }
            entry.lexical_score_nanos = Some(matched.score_nanos);
        }
    }
    if let Some(matches) = vector_matches {
        for (index, matched) in matches.iter().enumerate() {
            let rank = one_based(index)?;
            let entry = combined.entry(matched.key.clone()).or_default();
            if entry.vector_rank.replace(rank).is_some() {
                return Err(HybridError::DuplicateBranchKey);
            }
            entry.vector_score_nanos = Some(matched.score_nanos);
        }
    }
    let mut matches = combined
        .into_iter()
        .map(|(key, entry)| build_match(key, entry, request))
        .collect::<Result<Vec<_>, HybridError>>()?;
    matches.sort_by(|left, right| {
        right
            .explanation
            .fusion_score
            .cmp(&left.explanation.fusion_score)
            .then_with(|| left.key.cmp(&right.key))
    });
    matches.truncate(request.limit);
    for (index, matched) in matches.iter_mut().enumerate() {
        matched.explanation.final_rank = one_based(index)?;
    }
    Ok(HybridOutcome::Matches {
        matches,
        lexical_absence,
        vector_absence,
    })
}

fn validate_request(request: &HybridRequest) -> Result<(), HybridError> {
    if !(1..=MAX_WEIGHT).contains(&request.lexical_weight)
        || !(1..=MAX_WEIGHT).contains(&request.vector_weight)
    {
        return Err(HybridError::InvalidWeight);
    }
    if request.limit == 0 {
        return Err(HybridError::ZeroLimit);
    }
    Ok(())
}

fn lexical_branch(
    lexical: &LexicalOutcome,
) -> (Option<&[crate::LexicalMatch]>, Option<HybridBranchAbsence>) {
    match lexical {
        LexicalOutcome::Matches { matches, .. } => (Some(matches.as_slice()), None),
        LexicalOutcome::Abstained(abstention) => (
            None,
            Some(match abstention.reason {
                LexicalAbstentionReason::NoCandidates => HybridBranchAbsence::LexicalNoCandidates,
            }),
        ),
    }
}

fn vector_branch(
    vector: &ExactRetrievalOutcome,
) -> (
    Option<&[crate::ExactRetrievalMatch]>,
    Option<HybridBranchAbsence>,
) {
    match vector {
        ExactRetrievalOutcome::Matches { matches, .. } => (Some(matches.as_slice()), None),
        ExactRetrievalOutcome::Abstained(abstention) => (
            None,
            Some(match abstention.reason {
                ExactAbstentionReason::NoCandidates => HybridBranchAbsence::VectorNoCandidates,
                ExactAbstentionReason::BelowThreshold => HybridBranchAbsence::VectorBelowThreshold,
                ExactAbstentionReason::Ambiguous => HybridBranchAbsence::VectorAmbiguous,
            }),
        ),
    }
}

fn build_match(
    key: Vec<u8>,
    entry: Accumulator,
    request: &HybridRequest,
) -> Result<HybridMatch, HybridError> {
    let lexical_contribution = contribution(request.lexical_weight, entry.lexical_rank)?;
    let vector_contribution = contribution(request.vector_weight, entry.vector_rank)?;
    let fusion_score = lexical_contribution
        .checked_add(vector_contribution)
        .ok_or(HybridError::ArithmeticOverflow)?;
    Ok(HybridMatch {
        key,
        explanation: HybridExplanation {
            lexical_rank: entry.lexical_rank,
            lexical_score_nanos: entry.lexical_score_nanos,
            vector_rank: entry.vector_rank,
            vector_score_nanos: entry.vector_score_nanos,
            lexical_contribution,
            vector_contribution,
            fusion_score,
            final_rank: 0,
        },
    })
}

fn one_based(index: usize) -> Result<u64, HybridError> {
    u64::try_from(index)
        .ok()
        .and_then(|rank| rank.checked_add(1))
        .ok_or(HybridError::ArithmeticOverflow)
}

fn contribution(weight: u32, rank: Option<u64>) -> Result<u64, HybridError> {
    let Some(rank) = rank else {
        return Ok(0);
    };
    let denominator = HYBRID_RRF_CONSTANT
        .checked_add(rank)
        .ok_or(HybridError::ArithmeticOverflow)?;
    u64::from(weight)
        .checked_mul(CONTRIBUTION_SCALE)
        .and_then(|numerator| numerator.checked_div(denominator))
        .ok_or(HybridError::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use crate::{ExactAbstention, ExactRetrievalMatch, LexicalMatch};

    use super::*;

    fn lexical(keys: &[&[u8]]) -> LexicalOutcome {
        LexicalOutcome::Matches {
            matches: keys
                .iter()
                .enumerate()
                .map(|(index, key)| LexicalMatch {
                    key: key.to_vec(),
                    score_nanos: 100 - i64::try_from(index).unwrap_or(0),
                    terms: Vec::new(),
                })
                .collect(),
            scanned_documents: keys.len() as u64,
            matched_documents: keys.len() as u64,
            query_tokens: vec!["x".into()],
        }
    }

    fn vector(keys: &[&[u8]]) -> ExactRetrievalOutcome {
        ExactRetrievalOutcome::Matches {
            matches: keys
                .iter()
                .enumerate()
                .map(|(index, key)| ExactRetrievalMatch {
                    key: key.to_vec(),
                    score_nanos: 100 - i64::try_from(index).unwrap_or(0),
                })
                .collect(),
            scanned_candidates: keys.len() as u64,
        }
    }

    #[test]
    fn rrf_deduplicates_and_explains_rank() -> Result<(), HybridError> {
        let outcome = fuse_hybrid(
            &lexical(&[b"a", b"b"]),
            &vector(&[b"b", b"c"]),
            &HybridRequest {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 3,
            },
        )?;
        let HybridOutcome::Matches { matches, .. } = outcome else {
            return Err(HybridError::ArithmeticOverflow);
        };
        assert_eq!(matches[0].key, b"b");
        assert_eq!(matches[0].explanation.lexical_rank, Some(2));
        assert_eq!(matches[0].explanation.vector_rank, Some(1));
        assert_eq!(matches[0].explanation.final_rank, 1);
        Ok(())
    }

    #[test]
    fn one_abstaining_branch_is_explicit() -> Result<(), HybridError> {
        let lexical = LexicalOutcome::Abstained(crate::LexicalAbstention {
            reason: LexicalAbstentionReason::NoCandidates,
            scanned_documents: 1,
            query_tokens: vec!["x".into()],
        });
        let outcome = fuse_hybrid(
            &lexical,
            &vector(&[b"a"]),
            &HybridRequest {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 1,
            },
        )?;
        assert!(matches!(
            outcome,
            HybridOutcome::Matches {
                lexical_absence: Some(HybridBranchAbsence::LexicalNoCandidates),
                ..
            }
        ));
        let both = fuse_hybrid(
            &lexical,
            &ExactRetrievalOutcome::Abstained(ExactAbstention {
                reason: ExactAbstentionReason::NoCandidates,
                best_score_nanos: None,
                runner_up_score_nanos: None,
                scanned_candidates: 0,
            }),
            &HybridRequest {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 1,
            },
        )?;
        assert!(matches!(both, HybridOutcome::Abstained(_)));
        Ok(())
    }
}
