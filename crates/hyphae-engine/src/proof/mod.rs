// SPDX-License-Identifier: Apache-2.0

//! Canonical snapshot-witness result proofs and offline verification.

mod codec;
mod model;
mod verify;

pub use model::{
    MAX_RESULT_PROOF_BYTES, ProofAnchor, ProofError, ProvenOperation, ProvenResult,
    RESULT_PROOF_FORMAT_VERSION, ResultProof, ResultProofArtifact, VerificationLimits,
    VerificationReport,
};
pub use verify::{read_result_proof, verify_result_proof, write_result_proof};

use hyphae_query::{Query, QueryResult, Record};
use hyphae_storage::SnapshotInfo;

use self::codec::{decode_proof, encode_proof, finalize_proof};

impl ResultProof {
    /// Creates a canonical proof for one exact KV lookup.
    ///
    /// # Errors
    ///
    /// Returns an error for a noncanonical key/result pair or proof bounds.
    pub fn for_get(
        snapshot: &SnapshotInfo,
        key: Vec<u8>,
        result: Option<Record>,
    ) -> Result<Self, ProofError> {
        finalize_proof(
            ProofAnchor::from_snapshot(snapshot),
            ProvenOperation::Get { key },
            ProvenResult::Get(result),
        )
    }

    /// Creates a canonical proof for one complete structured query result.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported lengths, document bounds, or total
    /// proof size.
    pub fn for_query(
        snapshot: &SnapshotInfo,
        query: Query,
        result: QueryResult,
    ) -> Result<Self, ProofError> {
        finalize_proof(
            ProofAnchor::from_snapshot(snapshot),
            ProvenOperation::Query(query),
            ProvenResult::Query(result),
        )
    }

    /// Encodes the complete proof into canonical portable bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the proof exceeds a hard bound or contains a
    /// noncanonical model value.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ProofError> {
        encode_proof(self)
    }

    /// Verifies and decodes canonical proof bytes.
    ///
    /// # Errors
    ///
    /// Returns a framing, version, canonicality, bound, checksum, or digest
    /// error.
    pub fn from_bytes(encoded: &[u8]) -> Result<Self, ProofError> {
        decode_proof(encoded)
    }
}
