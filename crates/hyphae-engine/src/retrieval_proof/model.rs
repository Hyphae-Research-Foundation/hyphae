// SPDX-License-Identifier: Apache-2.0

use std::{io, time::Duration};

use hyphae_core::VectorValueError;
use hyphae_retrieval::{
    ExactRetrievalError, ExactRetrievalOutcome, ExactRetrievalRequest, HybridError, HybridOutcome,
    HybridRequest, LexicalError, LexicalOutcome, LexicalRequest,
};
use hyphae_storage::{SnapshotError, SnapshotInfo, SnapshotReadLimits};
use thiserror::Error;

use crate::DocumentError;

/// Version of the canonical retrieval-proof envelope.
pub const RETRIEVAL_PROOF_FORMAT_VERSION: u16 = 1;

/// Durable exact-retrieval semantics version bound into proofs.
pub const EXACT_RETRIEVAL_SEMANTICS_VERSION: u16 = 2;

/// Durable lexical-retrieval semantics version bound into proofs.
pub const LEXICAL_RETRIEVAL_SEMANTICS_VERSION: u16 = 1;

/// Deterministic hybrid-retrieval semantics version bound into proofs.
pub const HYBRID_RETRIEVAL_SEMANTICS_VERSION: u16 = 1;

/// Hard maximum canonical retrieval-proof file length.
pub const MAX_RETRIEVAL_PROOF_BYTES: u64 = 64 * 1024 * 1024;

/// Snapshot and log checkpoint identity trusted by one retrieval proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalProofAnchor {
    /// Materialized commit sequence captured by the snapshot.
    pub checkpoint_sequence: u64,
    /// Commit digest captured by the snapshot, absent only for empty history.
    pub checkpoint_digest: Option<[u8; 32]>,
    /// Canonical logical snapshot digest.
    pub snapshot_digest: [u8; 32],
}

impl RetrievalProofAnchor {
    /// Creates an anchor from already verified snapshot metadata.
    pub fn from_snapshot(snapshot: &SnapshotInfo) -> Self {
        Self {
            checkpoint_sequence: snapshot.checkpoint_sequence,
            checkpoint_digest: snapshot.checkpoint_digest,
            snapshot_digest: snapshot.snapshot_digest,
        }
    }

    /// Computes the caller-pinnable retrieval-specific anchor digest.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hyphae-retrieval-anchor-v1");
        hasher.update(&self.checkpoint_sequence.to_le_bytes());
        hasher.update(&self.checkpoint_digest.unwrap_or([0; 32]));
        hasher.update(&self.snapshot_digest);
        *hasher.finalize().as_bytes()
    }
}

/// Canonical exact-retrieval proof with embedded request and outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalProof {
    pub(crate) anchor: RetrievalProofAnchor,
    pub(crate) semantics_version: u16,
    pub(crate) request: ExactRetrievalRequest,
    pub(crate) outcome: ExactRetrievalOutcome,
    pub(crate) proof_digest: [u8; 32],
}

/// Newly created proof and the canonical snapshot witness it references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalProofArtifact {
    /// Portable canonical proof.
    pub proof: ExactRetrievalProof,
    /// Verified local snapshot witness metadata and path.
    pub snapshot: SnapshotInfo,
}

/// Canonical lexical-retrieval proof with embedded request and outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalRetrievalProof {
    pub(crate) anchor: RetrievalProofAnchor,
    pub(crate) semantics_version: u16,
    pub(crate) request: LexicalRequest,
    pub(crate) outcome: LexicalOutcome,
    pub(crate) proof_digest: [u8; 32],
}

/// Newly created lexical proof and canonical snapshot witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalRetrievalProofArtifact {
    /// Portable canonical proof.
    pub proof: LexicalRetrievalProof,
    /// Verified local snapshot witness metadata and path.
    pub snapshot: SnapshotInfo,
}

/// Canonical hybrid-retrieval proof with both complete branch executions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridRetrievalProof {
    pub(crate) anchor: RetrievalProofAnchor,
    pub(crate) semantics_version: u16,
    pub(crate) lexical_request: LexicalRequest,
    pub(crate) lexical_outcome: LexicalOutcome,
    pub(crate) vector_request: ExactRetrievalRequest,
    pub(crate) vector_outcome: ExactRetrievalOutcome,
    pub(crate) fusion_request: HybridRequest,
    pub(crate) outcome: HybridOutcome,
    pub(crate) proof_digest: [u8; 32],
}

/// Newly created hybrid proof and canonical snapshot witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridRetrievalProofArtifact {
    /// Portable canonical proof.
    pub proof: HybridRetrievalProof,
    /// Verified local snapshot witness metadata and path.
    pub snapshot: SnapshotInfo,
}

impl ExactRetrievalProof {
    /// Returns the exact snapshot/log anchor.
    pub fn anchor(&self) -> &RetrievalProofAnchor {
        &self.anchor
    }

    /// Returns the caller-pinnable anchor digest.
    pub fn anchor_digest(&self) -> [u8; 32] {
        self.anchor.digest()
    }

    /// Returns the bound exact-retrieval semantics version.
    pub fn semantics_version(&self) -> u16 {
        self.semantics_version
    }

    /// Returns the complete proven request.
    pub fn request(&self) -> &ExactRetrievalRequest {
        &self.request
    }

    /// Returns the complete proven outcome.
    pub fn outcome(&self) -> &ExactRetrievalOutcome {
        &self.outcome
    }

    /// Returns the digest of the complete canonical proof bytes.
    pub fn proof_digest(&self) -> [u8; 32] {
        self.proof_digest
    }
}

impl LexicalRetrievalProof {
    /// Returns the exact snapshot/log anchor.
    pub fn anchor(&self) -> &RetrievalProofAnchor {
        &self.anchor
    }

    /// Returns the caller-pinnable anchor digest.
    pub fn anchor_digest(&self) -> [u8; 32] {
        self.anchor.digest()
    }

    /// Returns the bound lexical semantics version.
    pub fn semantics_version(&self) -> u16 {
        self.semantics_version
    }

    /// Returns the complete proven lexical request.
    pub fn request(&self) -> &LexicalRequest {
        &self.request
    }

    /// Returns the complete proven lexical outcome.
    pub fn outcome(&self) -> &LexicalOutcome {
        &self.outcome
    }

    /// Returns the digest of the canonical proof bytes.
    pub fn proof_digest(&self) -> [u8; 32] {
        self.proof_digest
    }
}

impl HybridRetrievalProof {
    /// Returns the exact snapshot/log anchor.
    pub fn anchor(&self) -> &RetrievalProofAnchor {
        &self.anchor
    }

    /// Returns the caller-pinnable anchor digest.
    pub fn anchor_digest(&self) -> [u8; 32] {
        self.anchor.digest()
    }

    /// Returns the bound hybrid semantics version.
    pub fn semantics_version(&self) -> u16 {
        self.semantics_version
    }

    /// Returns the complete proven lexical branch request.
    pub fn lexical_request(&self) -> &LexicalRequest {
        &self.lexical_request
    }

    /// Returns the complete proven lexical branch outcome.
    pub fn lexical_outcome(&self) -> &LexicalOutcome {
        &self.lexical_outcome
    }

    /// Returns the complete proven exact-vector branch request.
    pub fn vector_request(&self) -> &ExactRetrievalRequest {
        &self.vector_request
    }

    /// Returns the complete proven exact-vector branch outcome.
    pub fn vector_outcome(&self) -> &ExactRetrievalOutcome {
        &self.vector_outcome
    }

    /// Returns the deterministic fusion request.
    pub fn fusion_request(&self) -> &HybridRequest {
        &self.fusion_request
    }

    /// Returns the complete proven hybrid outcome.
    pub fn outcome(&self) -> &HybridOutcome {
        &self.outcome
    }

    /// Returns the digest of the canonical proof bytes.
    pub fn proof_digest(&self) -> [u8; 32] {
        self.proof_digest
    }
}

/// Resource limits for complete offline exact-retrieval verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalVerificationLimits {
    /// Maximum proof file bytes accepted before allocation.
    pub proof_bytes: u64,
    /// Limits for loading the canonical snapshot witness.
    pub snapshot: SnapshotReadLimits,
    /// Maximum candidates accepted by exact replay.
    pub max_candidates: u64,
    /// Maximum aggregate key and vector bytes accepted by exact replay.
    pub max_candidate_bytes: u64,
    /// Maximum result count accepted by exact replay.
    pub max_returned: usize,
    /// Maximum durable documents accepted by lexical replay.
    pub max_documents: u64,
    /// Maximum normalized tokens accepted by lexical replay.
    pub max_tokens: u64,
    /// Maximum matching documents retained by lexical replay.
    pub max_lexical_candidates: u64,
    /// Maximum lexical result count accepted by replay.
    pub max_lexical_returned: usize,
    /// Maximum hybrid result count accepted by replay.
    pub max_hybrid_returned: usize,
    /// End-to-end cooperative verification deadline.
    pub timeout: Duration,
}

impl Default for RetrievalVerificationLimits {
    fn default() -> Self {
        Self {
            proof_bytes: MAX_RETRIEVAL_PROOF_BYTES,
            snapshot: SnapshotReadLimits::default(),
            max_candidates: 100_000,
            max_candidate_bytes: 1024 * 1024 * 1024,
            max_returned: 1_000,
            max_documents: 1_000_000,
            max_tokens: 10_000_000,
            max_lexical_candidates: 100_000,
            max_lexical_returned: 1_000,
            max_hybrid_returned: 1_000,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Successful offline exact-retrieval verification evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRetrievalVerificationReport {
    /// Trusted anchor accepted by the verifier.
    pub anchor: RetrievalProofAnchor,
    /// Caller-pinnable anchor digest that matched expectation.
    pub anchor_digest: [u8; 32],
    /// Digest of the verified canonical proof file.
    pub proof_digest: [u8; 32],
    /// Complete reexecuted and verified outcome.
    pub outcome: ExactRetrievalOutcome,
}

/// Successful offline lexical-retrieval verification evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalRetrievalVerificationReport {
    /// Trusted anchor accepted by the verifier.
    pub anchor: RetrievalProofAnchor,
    /// Caller-pinnable anchor digest that matched expectation.
    pub anchor_digest: [u8; 32],
    /// Digest of the verified canonical proof file.
    pub proof_digest: [u8; 32],
    /// Complete reexecuted lexical outcome.
    pub outcome: LexicalOutcome,
}

/// Successful offline hybrid-retrieval verification evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridRetrievalVerificationReport {
    /// Trusted anchor accepted by the verifier.
    pub anchor: RetrievalProofAnchor,
    /// Caller-pinnable anchor digest that matched expectation.
    pub anchor_digest: [u8; 32],
    /// Digest of the verified canonical proof file.
    pub proof_digest: [u8; 32],
    /// Complete reexecuted hybrid outcome.
    pub outcome: HybridOutcome,
}

/// Failure while encoding, reading, or verifying a retrieval proof.
#[derive(Debug, Error)]
pub enum RetrievalProofError {
    /// Proof file I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// Snapshot witness verification failed.
    #[error("snapshot witness failed: {source}")]
    Snapshot {
        /// Underlying snapshot failure.
        #[source]
        source: Box<SnapshotError>,
    },

    /// Canonical vector value is invalid.
    #[error(transparent)]
    Vector(#[from] VectorValueError),

    /// Exact-retrieval replay failed.
    #[error("exact retrieval replay failed: {source}")]
    Retrieval {
        /// Underlying deterministic retrieval failure.
        #[source]
        source: Box<ExactRetrievalError>,
    },

    /// Lexical-retrieval replay failed.
    #[error("lexical retrieval replay failed: {source}")]
    Lexical {
        /// Underlying deterministic lexical failure.
        #[source]
        source: Box<LexicalError>,
    },

    /// Hybrid fusion replay failed.
    #[error("hybrid retrieval replay failed: {source}")]
    Hybrid {
        /// Underlying deterministic hybrid failure.
        #[source]
        source: Box<HybridError>,
    },

    /// Canonical durable document decoding failed.
    #[error("snapshot document decoding failed: {source}")]
    Document {
        /// Underlying canonical document failure.
        #[source]
        source: Box<DocumentError>,
    },

    /// Proof bytes violate the canonical format.
    #[error("invalid retrieval proof: {reason}")]
    Invalid {
        /// Stable diagnostic reason.
        reason: &'static str,
    },

    /// Proof format is newer than this verifier.
    #[error("unsupported retrieval-proof format {found}; supported format is {supported}")]
    UnsupportedVersion {
        /// Version found in proof bytes.
        found: u16,
        /// Highest supported version.
        supported: u16,
    },

    /// Retrieval operation is not supported.
    #[error("unsupported retrieval-proof operation {found}")]
    UnsupportedOperation {
        /// Operation tag found in proof bytes.
        found: u16,
    },

    /// Retrieval semantics version is not supported.
    #[error("unsupported exact-retrieval semantics {found}; supported semantics is {supported}")]
    UnsupportedSemantics {
        /// Semantics version found in proof bytes.
        found: u16,
        /// Supported semantics version.
        supported: u16,
    },

    /// Proof file exceeds caller policy.
    #[error("retrieval proof is {actual} bytes; verification limit is {maximum}")]
    ProofLimitExceeded {
        /// Observed proof bytes.
        actual: u64,
        /// Configured maximum.
        maximum: u64,
    },

    /// A canonical length or count cannot be represented safely.
    #[error("retrieval-proof length overflow")]
    LengthOverflow,

    /// Fast accidental-corruption check failed.
    #[error("retrieval-proof CRC32C mismatch")]
    ChecksumMismatch,

    /// Canonical proof content digest failed.
    #[error("retrieval-proof BLAKE3 mismatch")]
    DigestMismatch,

    /// Proof anchor did not match caller-pinned trust state.
    #[error("retrieval-proof anchor does not match the trusted anchor digest")]
    AnchorMismatch,

    /// Snapshot metadata does not match the proof anchor.
    #[error("snapshot witness does not match the retrieval-proof anchor")]
    SnapshotAnchorMismatch,

    /// Snapshot uses a format that cannot witness durable vectors.
    #[error("retrieval proofs require a disk-format-2 snapshot witness")]
    SnapshotFormatMismatch,

    /// Deterministic replay did not reproduce the embedded outcome.
    #[error("offline reexecution does not match the retrieval proof")]
    ReexecutionMismatch,

    /// End-to-end cooperative verification deadline expired.
    #[error("retrieval-proof verification timed out")]
    TimedOut,
}

impl From<SnapshotError> for RetrievalProofError {
    fn from(source: SnapshotError) -> Self {
        Self::Snapshot {
            source: Box::new(source),
        }
    }
}

impl From<ExactRetrievalError> for RetrievalProofError {
    fn from(source: ExactRetrievalError) -> Self {
        Self::Retrieval {
            source: Box::new(source),
        }
    }
}

impl From<LexicalError> for RetrievalProofError {
    fn from(source: LexicalError) -> Self {
        Self::Lexical {
            source: Box::new(source),
        }
    }
}

impl From<HybridError> for RetrievalProofError {
    fn from(source: HybridError) -> Self {
        Self::Hybrid {
            source: Box::new(source),
        }
    }
}

impl From<DocumentError> for RetrievalProofError {
    fn from(source: DocumentError) -> Self {
        Self::Document {
            source: Box::new(source),
        }
    }
}
