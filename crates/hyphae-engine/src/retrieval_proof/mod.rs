// SPDX-License-Identifier: Apache-2.0

//! Canonical durable-retrieval proofs and complete offline verification.

mod codec;
mod model;
mod verify;

pub use model::{
    EXACT_RETRIEVAL_SEMANTICS_VERSION, ExactRetrievalProof, ExactRetrievalProofArtifact,
    ExactRetrievalVerificationReport, HYBRID_RETRIEVAL_SEMANTICS_VERSION, HybridRetrievalProof,
    HybridRetrievalProofArtifact, HybridRetrievalVerificationReport,
    LEXICAL_RETRIEVAL_SEMANTICS_VERSION, LexicalRetrievalProof, LexicalRetrievalProofArtifact,
    LexicalRetrievalVerificationReport, MAX_RETRIEVAL_PROOF_BYTES, RETRIEVAL_PROOF_FORMAT_VERSION,
    RetrievalProofAnchor, RetrievalProofError, RetrievalVerificationLimits,
};
pub use verify::{
    read_exact_retrieval_proof, read_hybrid_retrieval_proof, read_lexical_retrieval_proof,
    verify_exact_retrieval_proof, verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof,
    write_exact_retrieval_proof, write_hybrid_retrieval_proof, write_lexical_retrieval_proof,
};

use hyphae_retrieval::{
    ExactRetrievalOutcome, ExactRetrievalRequest, HybridOutcome, HybridRequest, LexicalOutcome,
    LexicalRequest,
};
use hyphae_storage::SnapshotInfo;

use self::codec::{
    decode_hybrid_proof, decode_lexical_proof, decode_proof, encode_hybrid_proof,
    encode_lexical_proof, encode_proof, finalize_hybrid_proof, finalize_lexical_proof,
    finalize_proof,
};

impl ExactRetrievalProof {
    /// Creates a canonical exact proof over a format-2 snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a legacy witness, invalid model, or proof bound.
    pub fn new(
        snapshot: &SnapshotInfo,
        request: ExactRetrievalRequest,
        outcome: ExactRetrievalOutcome,
    ) -> Result<Self, RetrievalProofError> {
        if snapshot.disk_format_version != 2 {
            return Err(RetrievalProofError::SnapshotFormatMismatch);
        }
        finalize_proof(
            RetrievalProofAnchor::from_snapshot(snapshot),
            request,
            outcome,
        )
    }

    /// Encodes the complete proof into canonical portable bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid model or exceeded hard bound.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RetrievalProofError> {
        encode_proof(self)
    }

    /// Verifies and decodes canonical proof bytes.
    ///
    /// # Errors
    ///
    /// Returns a framing, version, canonicality, checksum, or digest error.
    pub fn from_bytes(encoded: &[u8]) -> Result<Self, RetrievalProofError> {
        decode_proof(encoded)
    }
}

impl LexicalRetrievalProof {
    /// Creates a canonical lexical proof over a format-2 snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a legacy witness, invalid model, or proof bound.
    pub fn new(
        snapshot: &SnapshotInfo,
        request: LexicalRequest,
        outcome: LexicalOutcome,
    ) -> Result<Self, RetrievalProofError> {
        if snapshot.disk_format_version != 2 {
            return Err(RetrievalProofError::SnapshotFormatMismatch);
        }
        finalize_lexical_proof(
            RetrievalProofAnchor::from_snapshot(snapshot),
            request,
            outcome,
        )
    }

    /// Encodes the complete proof into canonical portable bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid model or exceeded hard bound.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RetrievalProofError> {
        encode_lexical_proof(self)
    }

    /// Verifies and decodes canonical lexical proof bytes.
    ///
    /// # Errors
    ///
    /// Returns a framing, version, canonicality, checksum, or digest error.
    pub fn from_bytes(encoded: &[u8]) -> Result<Self, RetrievalProofError> {
        decode_lexical_proof(encoded)
    }
}

impl HybridRetrievalProof {
    /// Creates a canonical hybrid proof over a format-2 snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a legacy witness, invalid branch/fusion model, or
    /// proof bound.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot: &SnapshotInfo,
        lexical_request: LexicalRequest,
        lexical_outcome: LexicalOutcome,
        vector_request: ExactRetrievalRequest,
        vector_outcome: ExactRetrievalOutcome,
        fusion_request: HybridRequest,
        outcome: HybridOutcome,
    ) -> Result<Self, RetrievalProofError> {
        if snapshot.disk_format_version != 2 {
            return Err(RetrievalProofError::SnapshotFormatMismatch);
        }
        finalize_hybrid_proof(
            RetrievalProofAnchor::from_snapshot(snapshot),
            lexical_request,
            lexical_outcome,
            vector_request,
            vector_outcome,
            fusion_request,
            outcome,
        )
    }

    /// Encodes the complete proof into canonical portable bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid model or exceeded hard bound.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RetrievalProofError> {
        encode_hybrid_proof(self)
    }

    /// Verifies and decodes canonical hybrid proof bytes.
    ///
    /// # Errors
    ///
    /// Returns a framing, version, canonicality, checksum, or digest error.
    pub fn from_bytes(encoded: &[u8]) -> Result<Self, RetrievalProofError> {
        decode_hybrid_proof(encoded)
    }
}
