// SPDX-License-Identifier: Apache-2.0

use std::{io, time::Duration};

use hyphae_query::{ExecutionLimits, Query, QueryError, QueryResult, Record};
use hyphae_storage::{SnapshotError, SnapshotInfo, SnapshotReadLimits};
use thiserror::Error;

use crate::DocumentError;

/// Version of the canonical result-proof envelope.
pub const RESULT_PROOF_FORMAT_VERSION: u16 = 1;

/// Hard maximum canonical proof file length.
pub const MAX_RESULT_PROOF_BYTES: u64 = 64 * 1024 * 1024;

/// Snapshot and log checkpoint identity trusted by one result proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofAnchor {
    /// Materialized commit sequence captured by the snapshot.
    pub checkpoint_sequence: u64,
    /// Commit digest captured by the snapshot, absent only for empty history.
    pub checkpoint_digest: Option<[u8; 32]>,
    /// Canonical logical snapshot digest.
    pub snapshot_digest: [u8; 32],
}

impl ProofAnchor {
    /// Creates an anchor from already verified snapshot metadata.
    pub fn from_snapshot(snapshot: &SnapshotInfo) -> Self {
        Self {
            checkpoint_sequence: snapshot.checkpoint_sequence,
            checkpoint_digest: snapshot.checkpoint_digest,
            snapshot_digest: snapshot.snapshot_digest,
        }
    }

    /// Computes the caller-pinnable domain-separated anchor digest.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hyphae-proof-anchor-v1");
        hasher.update(&self.checkpoint_sequence.to_le_bytes());
        hasher.update(&self.checkpoint_digest.unwrap_or([0; 32]));
        hasher.update(&self.snapshot_digest);
        *hasher.finalize().as_bytes()
    }
}

/// Durable operation embedded in a result proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvenOperation {
    /// Exact binary-key lookup.
    Get {
        /// Requested nonempty key.
        key: Vec<u8>,
    },
    /// Complete deterministic structured query.
    Query(Query),
}

/// Complete logical result embedded in a result proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvenResult {
    /// Exact lookup result, including verifiable absence.
    Get(Option<Record>),
    /// Structured query result including cursor and aggregation.
    Query(QueryResult),
}

/// Canonical result proof with an embedded request and result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultProof {
    pub(crate) anchor: ProofAnchor,
    pub(crate) operation: ProvenOperation,
    pub(crate) result: ProvenResult,
    pub(crate) proof_digest: [u8; 32],
}

/// Newly created proof and the local canonical snapshot witness it references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultProofArtifact {
    /// Portable canonical result proof.
    pub proof: ResultProof,
    /// Verified local snapshot witness metadata and path.
    pub snapshot: SnapshotInfo,
}

impl ResultProof {
    /// Returns the exact snapshot/log anchor.
    pub fn anchor(&self) -> &ProofAnchor {
        &self.anchor
    }

    /// Returns the caller-pinnable anchor digest.
    pub fn anchor_digest(&self) -> [u8; 32] {
        self.anchor.digest()
    }

    /// Returns the operation whose result is proven.
    pub fn operation(&self) -> &ProvenOperation {
        &self.operation
    }

    /// Returns the complete proven result.
    pub fn result(&self) -> &ProvenResult {
        &self.result
    }

    /// Returns the digest of the complete canonical proof bytes.
    pub fn proof_digest(&self) -> [u8; 32] {
        self.proof_digest
    }
}

/// Resource limits for complete offline result verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationLimits {
    /// Maximum proof file bytes accepted before allocation.
    pub proof_bytes: u64,
    /// Limits for loading the canonical snapshot witness.
    pub snapshot: SnapshotReadLimits,
    /// Reference structured-query limits used during reexecution.
    pub query: ExecutionLimits,
    /// End-to-end cooperative verification deadline.
    pub timeout: Duration,
}

impl Default for VerificationLimits {
    fn default() -> Self {
        Self {
            proof_bytes: MAX_RESULT_PROOF_BYTES,
            snapshot: SnapshotReadLimits::default(),
            query: ExecutionLimits::default(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Successful offline verification evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    /// Trusted anchor accepted by the verifier.
    pub anchor: ProofAnchor,
    /// Caller-pinnable anchor digest that matched expectation.
    pub anchor_digest: [u8; 32],
    /// Digest of the verified canonical proof file.
    pub proof_digest: [u8; 32],
    /// Complete reexecuted and verified result.
    pub result: ProvenResult,
}

/// Failure while encoding, reading, or verifying a result proof.
#[derive(Debug, Error)]
pub enum ProofError {
    /// Proof file I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// Canonical structured document verification failed.
    #[error(transparent)]
    Document(#[from] DocumentError),

    /// Snapshot witness verification failed.
    #[error("snapshot witness failed: {source}")]
    Snapshot {
        /// Underlying snapshot failure.
        #[source]
        source: Box<SnapshotError>,
    },

    /// Structured query reexecution failed.
    #[error("query reexecution failed: {source}")]
    Query {
        /// Underlying deterministic query failure.
        #[source]
        source: Box<QueryError>,
    },

    /// Proof bytes violate the canonical format.
    #[error("invalid result proof: {reason}")]
    Invalid {
        /// Stable diagnostic reason.
        reason: &'static str,
    },

    /// Proof format is newer than this verifier.
    #[error("unsupported result-proof format {found}; supported format is {supported}")]
    UnsupportedVersion {
        /// Version found in proof bytes.
        found: u16,
        /// Highest supported version.
        supported: u16,
    },

    /// Proof file exceeds caller policy.
    #[error("result proof is {actual} bytes; verification limit is {maximum}")]
    ProofLimitExceeded {
        /// Observed proof bytes.
        actual: u64,
        /// Configured maximum.
        maximum: u64,
    },

    /// A canonical length or count cannot be represented safely.
    #[error("result-proof length overflow")]
    LengthOverflow,

    /// Fast accidental-corruption check failed.
    #[error("result-proof CRC32C mismatch")]
    ChecksumMismatch,

    /// Canonical proof content digest failed.
    #[error("result-proof BLAKE3 mismatch")]
    DigestMismatch,

    /// Proof anchor did not match caller-pinned trust state.
    #[error("result-proof anchor does not match the trusted anchor digest")]
    AnchorMismatch,

    /// Snapshot metadata does not match the proof anchor.
    #[error("snapshot witness does not match the result-proof anchor")]
    SnapshotAnchorMismatch,

    /// Embedded operation and result variants disagree.
    #[error("result-proof operation and result variants do not match")]
    OperationResultMismatch,

    /// Deterministic replay did not reproduce the embedded result.
    #[error("offline reexecution does not match the result proof")]
    ReexecutionMismatch,

    /// End-to-end cooperative verification deadline expired.
    #[error("result-proof verification timed out")]
    TimedOut,
}

impl From<SnapshotError> for ProofError {
    fn from(source: SnapshotError) -> Self {
        Self::Snapshot {
            source: Box::new(source),
        }
    }
}

impl From<QueryError> for ProofError {
    fn from(source: QueryError) -> Self {
        Self::Query {
            source: Box::new(source),
        }
    }
}
