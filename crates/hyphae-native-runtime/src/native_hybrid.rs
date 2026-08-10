// SPDX-License-Identifier: GPL-3.0-only

//! Embedded deterministic RRF over one immutable native snapshot.

use std::collections::BTreeMap;

use hyphae_native_types::{Csn, ObjectId};
use thiserror::Error;

use crate::{
    AnnSearchOptions, AnnSearchReceipt, NativeDatabase, NativeRuntimeError, NativeSnapshot, Vector,
    VectorHit,
};

/// Fixed reciprocal-rank fusion constant.
pub const NATIVE_HYBRID_RRF_CONSTANT: u64 = 60;
const CONTRIBUTION_SCALE: u64 = 1_000_000_000;
const MAX_WEIGHT: u32 = 1_000_000;
/// Maximum candidates admitted from either native branch.
pub const MAX_NATIVE_HYBRID_BRANCH_HITS: usize = 10_000;
/// Maximum fused matches returned by one embedded request.
pub const MAX_NATIVE_HYBRID_RETURNED: usize = 1_000;

/// Native vector branch selected for hybrid execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVectorBranch {
    /// Complete exact ranking over the vector generation.
    Exact,
    /// HNSW traversal with its explicit native search options.
    Ann(AnnSearchOptions),
}

/// Complete native fusion settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridFusion {
    /// Positive lexical branch weight.
    pub lexical_weight: u32,
    /// Positive vector branch weight.
    pub vector_weight: u32,
    /// Maximum returned fused matches.
    pub limit: usize,
}

/// Complete embedded hybrid request.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridRequest<'request> {
    /// Native lexical collection.
    pub lexical_index: ObjectId,
    /// MATCH query evaluated by the native analyzer.
    pub lexical_query: &'request str,
    /// Maximum lexical candidates passed to RRF.
    pub lexical_limit: usize,
    /// Native vector index.
    pub vector_index: ObjectId,
    /// Validated native query vector.
    pub vector_query: &'request Vector,
    /// Exact or approximate vector execution.
    pub vector_branch: NativeVectorBranch,
    /// Maximum exact-vector candidates; ANN uses `AnnSearchOptions::k`.
    pub vector_limit: usize,
    /// Deterministic native RRF settings.
    pub fusion: NativeHybridFusion,
}

/// Full per-result native fusion explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridExplanation {
    /// One-based lexical rank.
    pub lexical_rank: Option<u64>,
    /// Canonical lexical score nanos.
    pub lexical_score_nanos: Option<i64>,
    /// One-based vector rank.
    pub vector_rank: Option<u64>,
    /// Canonical vector score nanos.
    pub vector_score_nanos: Option<i64>,
    /// Integer lexical RRF contribution.
    pub lexical_contribution: u64,
    /// Integer vector RRF contribution.
    pub vector_contribution: u64,
    /// Checked contribution sum.
    pub fusion_score: u64,
    /// One-based final rank.
    pub final_rank: u64,
}

/// One canonical native hybrid match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridMatch {
    /// Stable object identity.
    pub object_id: ObjectId,
    /// Explainable fusion components.
    pub explanation: NativeHybridExplanation,
}

/// Complete native hybrid outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHybridOutcome {
    /// At least one branch produced candidates.
    Matches(Vec<NativeHybridMatch>),
    /// Both branches had no candidates.
    Abstained,
}

/// Evidence returned with one embedded hybrid execution.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReceipt {
    /// Immutable all-engine CSN shared by both branches.
    pub snapshot_csn: Option<Csn>,
    /// Complete native RRF outcome.
    pub outcome: NativeHybridOutcome,
    /// Lexical candidates admitted to fusion.
    pub lexical_candidates: usize,
    /// Vector candidates admitted to fusion.
    pub vector_candidates: usize,
    /// ANN execution evidence, present only for an ANN branch.
    pub ann: Option<AnnSearchReceipt>,
}

/// Embedded hybrid validation or execution failure.
#[derive(Debug, Error)]
pub enum NativeHybridError {
    /// A weight or limit is outside its hard bound.
    #[error("native hybrid weights or limits are outside their bounded ranges")]
    InvalidRequest,
    /// A lexical document ID is not one canonical stable object ID.
    #[error("native hybrid lexical result has a noncanonical stable object ID")]
    InvalidStableId,
    /// A score cannot be represented canonically.
    #[error("native hybrid branch produced a noncanonical score")]
    InvalidScore,
    /// A branch repeats one stable ID.
    #[error("native hybrid branch contains a duplicate stable ID")]
    DuplicateBranchId,
    /// Checked fusion arithmetic failed.
    #[error("native hybrid contribution arithmetic overflow")]
    ArithmeticOverflow,
    /// A native branch failed.
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
}

#[derive(Clone, Copy, Default)]
struct Accumulator {
    lexical_rank: Option<u64>,
    lexical_score_nanos: Option<i64>,
    vector_rank: Option<u64>,
    vector_score_nanos: Option<i64>,
}

impl NativeSnapshot {
    /// Executes native lexical and exact vector/ANN branches on this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inputs, branch failures, or fusion overflow.
    pub fn retrieve_hybrid(
        &self,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        validate(request)?;
        let lexical = self.match_text(
            request.lexical_index,
            request.lexical_query,
            request.lexical_limit,
        )?;
        let (vector, ann) = match request.vector_branch {
            NativeVectorBranch::Exact => (
                self.search_vector_exact(
                    request.vector_index,
                    request.vector_query,
                    request.vector_limit,
                )?,
                None,
            ),
            NativeVectorBranch::Ann(options) => {
                let receipt =
                    self.search_ann(request.vector_index, request.vector_query, options)?;
                (receipt.hits.clone(), Some(receipt))
            }
        };
        let outcome = fuse(&lexical, &vector, &request.fusion)?;
        Ok(NativeHybridReceipt {
            snapshot_csn: self.visible_csn(),
            lexical_candidates: lexical.len(),
            vector_candidates: vector.len(),
            outcome,
            ann,
        })
    }
}

impl NativeDatabase {
    /// Captures one all-engine snapshot and executes both hybrid branches.
    ///
    /// # Errors
    ///
    /// Returns snapshot materialization or native hybrid failures.
    pub fn retrieve_hybrid_latest(
        &self,
        logical_time_micros: i64,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        let _permit = self.admit_foreground_bounded()?;
        self.snapshot(logical_time_micros)?.retrieve_hybrid(request)
    }
}

fn validate(request: &NativeHybridRequest<'_>) -> Result<(), NativeHybridError> {
    if !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.lexical_limit)
        || !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.vector_limit)
        || !(1..=MAX_NATIVE_HYBRID_RETURNED).contains(&request.fusion.limit)
        || !(1..=MAX_WEIGHT).contains(&request.fusion.lexical_weight)
        || !(1..=MAX_WEIGHT).contains(&request.fusion.vector_weight)
    {
        return Err(NativeHybridError::InvalidRequest);
    }
    if let NativeVectorBranch::Ann(options) = request.vector_branch
        && (options.k() > MAX_NATIVE_HYBRID_BRANCH_HITS || options.k() != request.vector_limit)
    {
        return Err(NativeHybridError::InvalidRequest);
    }
    Ok(())
}

fn fuse(
    lexical: &[crate::MatchHit],
    vector: &[VectorHit],
    request: &NativeHybridFusion,
) -> Result<NativeHybridOutcome, NativeHybridError> {
    if lexical.is_empty() && vector.is_empty() {
        return Ok(NativeHybridOutcome::Abstained);
    }
    let mut combined = BTreeMap::<ObjectId, Accumulator>::new();
    for (index, hit) in lexical.iter().enumerate() {
        let object_id = decode_object_id(&hit.document_id)?;
        let entry = combined.entry(object_id).or_default();
        if entry.lexical_rank.replace(one_based(index)?).is_some() {
            return Err(NativeHybridError::DuplicateBranchId);
        }
        entry.lexical_score_nanos = Some(scaled_score(hit.score)?);
    }
    for (index, hit) in vector.iter().enumerate() {
        let entry = combined.entry(hit.object_id).or_default();
        if entry.vector_rank.replace(one_based(index)?).is_some() {
            return Err(NativeHybridError::DuplicateBranchId);
        }
        entry.vector_score_nanos = Some(scaled_score(-hit.distance)?);
    }
    let mut matches = combined
        .into_iter()
        .map(|(object_id, entry)| build_match(object_id, entry, request))
        .collect::<Result<Vec<_>, NativeHybridError>>()?;
    matches.sort_by(|left, right| {
        right
            .explanation
            .fusion_score
            .cmp(&left.explanation.fusion_score)
            .then_with(|| left.object_id.cmp(&right.object_id))
    });
    matches.truncate(request.limit);
    for (index, matched) in matches.iter_mut().enumerate() {
        matched.explanation.final_rank = one_based(index)?;
    }
    Ok(NativeHybridOutcome::Matches(matches))
}

fn build_match(
    object_id: ObjectId,
    entry: Accumulator,
    request: &NativeHybridFusion,
) -> Result<NativeHybridMatch, NativeHybridError> {
    let lexical_contribution = contribution(request.lexical_weight, entry.lexical_rank)?;
    let vector_contribution = contribution(request.vector_weight, entry.vector_rank)?;
    let fusion_score = lexical_contribution
        .checked_add(vector_contribution)
        .ok_or(NativeHybridError::ArithmeticOverflow)?;
    Ok(NativeHybridMatch {
        object_id,
        explanation: NativeHybridExplanation {
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

fn decode_object_id(encoded: &[u8]) -> Result<ObjectId, NativeHybridError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeHybridError::InvalidStableId)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeHybridError::InvalidStableId)
}

fn one_based(index: usize) -> Result<u64, NativeHybridError> {
    u64::try_from(index)
        .ok()
        .and_then(|rank| rank.checked_add(1))
        .ok_or(NativeHybridError::ArithmeticOverflow)
}

fn contribution(weight: u32, rank: Option<u64>) -> Result<u64, NativeHybridError> {
    let Some(rank) = rank else { return Ok(0) };
    NATIVE_HYBRID_RRF_CONSTANT
        .checked_add(rank)
        .and_then(|denominator| {
            u64::from(weight)
                .checked_mul(CONTRIBUTION_SCALE)
                .and_then(|numerator| numerator.checked_div(denominator))
        })
        .ok_or(NativeHybridError::ArithmeticOverflow)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "checked rounded f64 is intentionally canonicalized to i64 nanos"
)]
fn scaled_score(score: f64) -> Result<i64, NativeHybridError> {
    let scaled = score * 1_000_000_000.0;
    if !scaled.is_finite() || scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
        return Err(NativeHybridError::InvalidScore);
    }
    Ok(scaled.round() as i64)
}
