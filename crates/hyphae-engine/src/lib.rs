// SPDX-License-Identifier: Apache-2.0

//! Embeddable Hyphae facade over durable storage, structured query, and exact
//! provider-neutral retrieval.

mod facade;
pub mod proof;
pub mod retrieval_proof;

pub use facade::{BoundedEngineQueryError, EngineError, HyphaeEngine, OpenedEngine};
pub use hyphae_query::{
    DocumentError, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_NODES, decode_document,
    encode_document,
};
pub use hyphae_storage::{MaintenanceLimits, RecoveryLimits, StorageLimitError, StorageLimits};
pub use proof::{
    MAX_RESULT_PROOF_BYTES, ProofAnchor, ProofError, ProvenOperation, ProvenResult,
    RESULT_PROOF_FORMAT_VERSION, ResultProof, ResultProofArtifact, VerificationLimits,
    VerificationReport, read_result_proof, verify_result_proof, write_result_proof,
};
pub use retrieval_proof::{
    EXACT_RETRIEVAL_SEMANTICS_VERSION, ExactRetrievalProof, ExactRetrievalProofArtifact,
    ExactRetrievalVerificationReport, HYBRID_RETRIEVAL_SEMANTICS_VERSION, HybridRetrievalProof,
    HybridRetrievalProofArtifact, HybridRetrievalVerificationReport,
    LEXICAL_RETRIEVAL_SEMANTICS_VERSION, LexicalRetrievalProof, LexicalRetrievalProofArtifact,
    LexicalRetrievalVerificationReport, MAX_RETRIEVAL_PROOF_BYTES, RETRIEVAL_PROOF_FORMAT_VERSION,
    RetrievalProofAnchor, RetrievalProofError, RetrievalVerificationLimits,
    read_exact_retrieval_proof, read_hybrid_retrieval_proof, read_lexical_retrieval_proof,
    verify_exact_retrieval_proof, verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof,
    write_exact_retrieval_proof, write_hybrid_retrieval_proof, write_lexical_retrieval_proof,
};
