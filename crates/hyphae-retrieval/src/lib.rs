// SPDX-License-Identifier: GPL-3.0-only

//! Exact retrieval and provider-neutral abstention semantics.
//!
//! The reference engine accepts vectors directly. No embedding or model
//! provider is enabled by default.

mod canonical;
mod engine;
mod hybrid;
mod lexical;
mod model;

pub use canonical::{
    DurableVectorRecord, ExactAbstention, ExactAbstentionReason, ExactRetrievalClock,
    ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalMatch, ExactRetrievalOutcome,
    ExactRetrievalRequest, cosine_score_nanos, retrieve_exact, retrieve_exact_with_clock,
};
pub use engine::{
    RetrievalClock, RetrievalError, RetrievalSystemClock, retrieve, retrieve_with_clock,
};
pub use hybrid::{
    HYBRID_RRF_CONSTANT, HybridAbstention, HybridBranchAbsence, HybridError, HybridExplanation,
    HybridMatch, HybridOutcome, HybridRequest, fuse_hybrid,
};
pub use lexical::{
    LexicalAbstention, LexicalAbstentionReason, LexicalError, LexicalField,
    LexicalFieldContribution, LexicalIndexDefinition, LexicalLimits, LexicalMatch,
    LexicalMaterializedCorpus, LexicalMaterializedDocument, LexicalOutcome, LexicalRequest,
    LexicalTermContribution, MAX_LEXICAL_FIELD_WEIGHT_MICROS, MAX_LEXICAL_FIELDS,
    MAX_LEXICAL_PATH_SEGMENT_BYTES, MAX_LEXICAL_PATH_SEGMENTS, MAX_LEXICAL_TOKEN_BYTES,
    retrieve_lexical, retrieve_lexical_materialized, tokenize_v1, tokenize_v1_checked,
};
pub use model::{
    Abstention, AbstentionReason, RetrievalLimits, RetrievalMatch, RetrievalOutcome,
    RetrievalRequest, VectorRecord,
};
