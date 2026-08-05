// SPDX-License-Identifier: Apache-2.0

//! Embedded hybrid retrieval over one immutable native snapshot.

use hyphae_native_types::{Csn, ObjectId};
use hyphae_retrieval::{
    ExactAbstention, ExactAbstentionReason, ExactRetrievalMatch, ExactRetrievalOutcome,
    HybridError, HybridOutcome, HybridRequest, LexicalAbstention, LexicalAbstentionReason,
    LexicalMatch, LexicalOutcome, fuse_hybrid,
};
use thiserror::Error;

use crate::{
    AnnSearchOptions, AnnSearchReceipt, NativeDatabase, NativeRuntimeError, NativeSnapshot, Vector,
    VectorHit,
};

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

/// Complete embedded hybrid request.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridRequest<'request> {
    /// Native lexical collection.
    pub lexical_index: ObjectId,
    /// MATCH query evaluated by the existing native analyzer path.
    pub lexical_query: &'request str,
    /// Maximum lexical candidates passed to RRF.
    pub lexical_limit: usize,
    /// Native vector index.
    pub vector_index: ObjectId,
    /// Validated native query vector.
    pub vector_query: &'request Vector,
    /// Exact or approximate vector execution.
    pub vector_branch: NativeVectorBranch,
    /// Maximum exact-vector candidates; ANN uses `SearchOptions::k`.
    pub vector_limit: usize,
    /// Existing deterministic hybrid weights and final limit.
    pub fusion: HybridRequest,
}

/// Evidence returned with one embedded hybrid execution.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReceipt {
    /// Immutable all-engine CSN shared by both branches.
    pub snapshot_csn: Option<Csn>,
    /// Complete RRF outcome, including per-hit explanations and abstentions.
    pub outcome: HybridOutcome,
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
    /// A branch or final limit is zero or exceeds its hard bound.
    #[error("native hybrid limits are outside their bounded ranges")]
    InvalidLimit,
    /// A lexical document ID is not the canonical 16-byte stable object ID.
    #[error("native hybrid lexical result has a noncanonical stable object ID")]
    InvalidStableId,
    /// A native branch produced a score outside the canonical integer domain.
    #[error("native hybrid branch produced a noncanonical score")]
    InvalidScore,
    /// A native branch failed.
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
    /// Existing deterministic RRF semantics rejected fusion.
    #[error(transparent)]
    Fusion(#[from] HybridError),
}

impl NativeSnapshot {
    /// Executes lexical MATCH and exact vector/ANN against this same immutable snapshot.
    ///
    /// No protocol codec or serialized engine-to-engine request participates in this path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits or identities, native branch failures, or RRF failure.
    pub fn retrieve_hybrid(
        &self,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        validate(request)?;
        let lexical_hits = self.match_text(
            request.lexical_index,
            request.lexical_query,
            request.lexical_limit,
        )?;
        let lexical = lexical_outcome(lexical_hits)?;
        let (vector_hits, ann) = match request.vector_branch {
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
        let vector = vector_outcome(&vector_hits)?;
        let lexical_candidates = outcome_len_lexical(&lexical);
        let vector_candidates = outcome_len_vector(&vector);
        let outcome = fuse_hybrid(&lexical, &vector, &request.fusion)?;
        Ok(NativeHybridReceipt {
            snapshot_csn: self.visible_csn(),
            outcome,
            lexical_candidates,
            vector_candidates,
            ann,
        })
    }
}

impl NativeDatabase {
    /// Captures one all-engine snapshot and executes both hybrid branches inside it.
    ///
    /// # Errors
    ///
    /// Returns snapshot materialization or embedded hybrid execution failures.
    pub fn retrieve_hybrid_latest(
        &self,
        logical_time_micros: i64,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        self.snapshot(logical_time_micros)?.retrieve_hybrid(request)
    }
}

fn validate(request: &NativeHybridRequest<'_>) -> Result<(), NativeHybridError> {
    if !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.lexical_limit)
        || !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.vector_limit)
        || !(1..=MAX_NATIVE_HYBRID_RETURNED).contains(&request.fusion.limit)
    {
        return Err(NativeHybridError::InvalidLimit);
    }
    if let NativeVectorBranch::Ann(options) = request.vector_branch
        && (options.k() > MAX_NATIVE_HYBRID_BRANCH_HITS || options.k() != request.vector_limit)
    {
        return Err(NativeHybridError::InvalidLimit);
    }
    Ok(())
}

fn lexical_outcome(hits: Vec<crate::MatchHit>) -> Result<LexicalOutcome, NativeHybridError> {
    if hits.is_empty() {
        return Ok(LexicalOutcome::Abstained(LexicalAbstention {
            reason: LexicalAbstentionReason::NoCandidates,
            scanned_documents: 0,
            query_tokens: Vec::new(),
        }));
    }
    let matches = hits
        .into_iter()
        .map(|hit| {
            let key = canonical_stable_id(&hit.document_id)?;
            Ok(LexicalMatch {
                key,
                score_nanos: scaled_score(hit.score)?,
                terms: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, NativeHybridError>>()?;
    Ok(LexicalOutcome::Matches {
        matched_documents: matches.len() as u64,
        scanned_documents: matches.len() as u64,
        matches,
        query_tokens: Vec::new(),
    })
}

fn vector_outcome(hits: &[VectorHit]) -> Result<ExactRetrievalOutcome, NativeHybridError> {
    if hits.is_empty() {
        return Ok(ExactRetrievalOutcome::Abstained(ExactAbstention {
            reason: ExactAbstentionReason::NoCandidates,
            best_score_nanos: None,
            runner_up_score_nanos: None,
            scanned_candidates: 0,
        }));
    }
    Ok(ExactRetrievalOutcome::Matches {
        matches: hits
            .iter()
            .map(|hit| {
                Ok(ExactRetrievalMatch {
                    key: hit.object_id.get().to_be_bytes().to_vec(),
                    score_nanos: scaled_score(-hit.distance)?,
                })
            })
            .collect::<Result<Vec<_>, NativeHybridError>>()?,
        scanned_candidates: hits.len() as u64,
    })
}

fn canonical_stable_id(encoded: &[u8]) -> Result<Vec<u8>, NativeHybridError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeHybridError::InvalidStableId)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeHybridError::InvalidStableId)?;
    Ok(bytes.to_vec())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the checked rounded f64 is intentionally canonicalized to hybrid's i64 nanos"
)]
fn scaled_score(score: f64) -> Result<i64, NativeHybridError> {
    let scaled = score * 1_000_000_000.0;
    if !scaled.is_finite() || scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
        return Err(NativeHybridError::InvalidScore);
    }
    Ok(scaled.round() as i64)
}

fn outcome_len_lexical(outcome: &LexicalOutcome) -> usize {
    match outcome {
        LexicalOutcome::Matches { matches, .. } => matches.len(),
        LexicalOutcome::Abstained(_) => 0,
    }
}

fn outcome_len_vector(outcome: &ExactRetrievalOutcome) -> usize {
    match outcome {
        ExactRetrievalOutcome::Matches { matches, .. } => matches.len(),
        ExactRetrievalOutcome::Abstained(_) => 0,
    }
}
