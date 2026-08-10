// SPDX-License-Identifier: GPL-3.0-only

use std::{fmt, io, path::PathBuf};

/// Version of the native proof and witness contracts.
pub const NATIVE_PROOF_VERSION: u16 = 2;
/// Hard implementation ceiling for one encoded native proof.
pub const MAX_NATIVE_PROOF_BYTES: u64 = 64 * 1024 * 1024;
/// Hard implementation ceiling for one encoded native witness.
pub const MAX_NATIVE_WITNESS_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// A canonical byte string and its domain-separated BLAKE3 digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalBytes {
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: [u8; 32],
}

impl CanonicalBytes {
    /// Canonicalizes an already deterministic byte string for proof binding.
    pub fn new(bytes: Vec<u8>) -> Self {
        let digest = canonical_bytes_digest(&bytes);
        Self { bytes, digest }
    }

    /// Canonicalizes a deterministic byte slice with allocation failure reporting.
    ///
    /// # Errors
    ///
    /// Returns [`NativeProofError::LengthOverflow`] when the canonical bytes cannot be reserved.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, NativeProofError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| NativeProofError::LengthOverflow)?;
        owned.extend_from_slice(bytes);
        Ok(Self::new(owned))
    }

    /// Returns the exact canonical bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the domain-separated digest of the canonical bytes.
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

pub(crate) fn canonical_bytes_digest(bytes: &[u8]) -> [u8; 32] {
    super::crypto::blake3_parts(&[
        b"hyphae-native-canonical-bytes-v1",
        &u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes(),
        bytes,
    ])
}

/// Immutable native state identity which must be pinned through a trusted channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProofAnchor {
    /// Stable 192-bit native directory lineage.
    pub directory_lineage: [u8; 24],
    /// Nonzero history epoch within the directory lineage.
    pub history_epoch: u64,
    /// Visible commit sequence, or zero for empty history.
    pub visible_csn: u64,
    /// Nonzero immutable catalog version.
    pub catalog_version: u64,
    /// Digest of the complete immutable root set.
    pub root_digest: [u8; 32],
    /// Sequence of the durable checkpoint bound by this anchor.
    pub checkpoint_sequence: u64,
    /// Digest of the durable checkpoint.
    pub checkpoint_digest: [u8; 32],
}

impl NativeProofAnchor {
    /// Computes the caller-pinnable, domain-separated anchor digest.
    pub fn digest(self) -> [u8; 32] {
        super::crypto::blake3_parts(&[
            b"hyphae-native-proof-anchor-v1",
            &self.directory_lineage,
            &self.history_epoch.to_le_bytes(),
            &self.visible_csn.to_le_bytes(),
            &self.catalog_version.to_le_bytes(),
            &self.root_digest,
            &self.checkpoint_sequence.to_le_bytes(),
            &self.checkpoint_digest,
        ])
    }
}

/// Anchor digest obtained independently of the proof and witness artifacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalTrustedAnchor {
    digest: [u8; 32],
}

impl ExternalTrustedAnchor {
    /// Wraps an anchor digest received through a caller-trusted channel.
    pub const fn new(digest: [u8; 32]) -> Self {
        Self { digest }
    }

    /// Returns the externally pinned digest.
    pub const fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Native operation family proven by an artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeProofKind {
    /// Exact point read, including absence.
    Point = 1,
    /// Bounded deterministic SQL result.
    Sql = 2,
    /// Bounded lexical-search result.
    Lexical = 3,
    /// Exact or filtered-exact vector result.
    ExactVector = 4,
    /// Approximate nearest-neighbor execution.
    Ann = 5,
    /// Fusion of independently digested branches.
    Hybrid = 6,
    /// Catalog inspection.
    Catalog = 7,
}

/// Whether the admitted operation completed its declared result semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CompletionStatus {
    /// The result is complete within the declared semantics.
    Complete = 1,
    /// A declared bound stopped execution and the result is a prefix/partial result.
    Truncated = 2,
}

/// Bounds admitted by the producer and committed into the proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmittedProofLimits {
    /// Maximum logical result items.
    pub result_items: u64,
    /// Maximum search candidates, or zero for operation families without candidates.
    pub candidate_items: u64,
    /// Maximum canonical evidence bytes.
    pub evidence_bytes: u64,
}

/// Stable catalog object and definition used by execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProofObjectBinding {
    /// Nonzero stable native object identity.
    pub object_id: u128,
    /// Digest of the canonical object definition.
    pub definition_digest: [u8; 32],
}

/// Vector distance or similarity metric bound by ANN execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum VectorMetric {
    /// Cosine distance.
    Cosine = 1,
    /// Negative dot-product distance.
    NegativeDot = 2,
    /// Squared Euclidean distance.
    SquaredL2 = 3,
}

/// Filtering strategy used by an ANN search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AnnFilterStrategy {
    /// No filter was applied.
    None = 0,
    /// Eligibility was tested before graph admission.
    PreFilter = 1,
    /// Results were filtered after graph traversal.
    PostFilter = 2,
    /// Traversal expanded iteratively until the filtered candidate target was met.
    Iterative = 3,
    /// Post-filter graph traversal augmented by one exact eligible seed.
    ExactSeededPostFilter = 4,
}

/// Honest approximation claim made by an ANN proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ApproximationLabel {
    /// Provenance and declared approximate execution are bound; exactness is not claimed.
    Approximate = 1,
    /// A separately digested exact-oracle receipt is included.
    ApproximateWithExactOracle = 2,
}

/// ANN execution metadata required for an approximate proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnProofMetadata {
    /// Vector metric used by the index and query.
    pub metric: VectorMetric,
    /// Digest of the canonical ANN index definition.
    pub index_definition_digest: [u8; 32],
    /// Digest of the immutable graph generation searched.
    pub graph_generation_digest: [u8; 32],
    /// Declared graph search breadth.
    pub search_breadth: u32,
    /// Filter execution strategy.
    pub filter_strategy: AnnFilterStrategy,
    /// Digest of the anchored eligibility predicate and observed eligible count.
    pub eligible_set_digest: [u8; 32],
    /// Number of graph nodes visited.
    pub visited_count: u64,
    /// Number of candidates retained before reranking.
    pub candidate_count: u64,
    /// Number of candidates reranked with source vectors.
    pub rerank_count: u64,
    /// Explicit approximation label.
    pub approximation: ApproximationLabel,
    /// Digest of a canonical exact-oracle receipt when declared by the label.
    pub exact_oracle_digest: Option<[u8; 32]>,
}

/// Hybrid branch failure behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HybridFailurePolicy {
    /// Any branch failure fails the hybrid operation.
    FailClosed = 1,
    /// Failed branches may be omitted as declared partial execution.
    AllowPartial = 2,
}

/// Deterministic hybrid fusion algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HybridFusionMethod {
    /// Weighted reciprocal-rank fusion.
    WeightedReciprocalRank = 1,
    /// Weighted normalized-score fusion.
    WeightedScore = 2,
}

/// Duplicate handling across hybrid branches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HybridDuplicatePolicy {
    /// Merge equal stable object IDs before final ordering.
    MergeByObjectId = 1,
    /// Preserve branch-qualified occurrences.
    PreserveBranches = 2,
}

/// One ordered branch committed into a hybrid proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HybridBranchBinding {
    /// Digest of the complete canonical branch execution contract.
    pub proof_digest: [u8; 32],
    /// Fixed-point branch weight in millionths.
    pub weight_millionths: u32,
    /// Maximum candidates admitted from this branch.
    pub candidate_limit: u32,
}

/// Hybrid fusion metadata and ordered branch proof digests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridProofMetadata {
    /// Ordered branch bindings.
    pub branches: Vec<HybridBranchBinding>,
    /// Branch failure behavior.
    pub failure_policy: HybridFailurePolicy,
    /// Fusion algorithm.
    pub fusion_method: HybridFusionMethod,
    /// Cross-branch duplicate handling.
    pub duplicate_policy: HybridDuplicatePolicy,
}

/// Exact witness artifact referenced by a proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeWitnessReference {
    /// Digest from the canonical `HYNWIT02` envelope.
    pub digest: [u8; 32],
    /// Exact encoded witness length.
    pub file_bytes: u64,
}

/// All canonical content committed into one native proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProofContent {
    /// Operation family.
    pub kind: NativeProofKind,
    /// Trusted immutable native state identity.
    pub anchor: NativeProofAnchor,
    /// Version of operation semantics.
    pub semantics_version: u16,
    /// Version of canonical result ordering.
    pub ordering_version: u16,
    /// Stable object bindings sorted by object ID.
    pub objects: Vec<ProofObjectBinding>,
    /// Complete canonical request bytes and digest.
    pub request: CanonicalBytes,
    /// Complete ordered canonical result bytes and digest.
    pub result: CanonicalBytes,
    /// Complete canonical evidence bytes and digest.
    pub evidence: CanonicalBytes,
    /// Producer-admitted execution bounds.
    pub limits: AdmittedProofLimits,
    /// Completion state.
    pub completion: CompletionStatus,
    /// Required complete witness artifact.
    pub witness: NativeWitnessReference,
    /// Required metadata for ANN proofs and absent otherwise.
    pub ann: Option<AnnProofMetadata>,
    /// Required metadata for hybrid proofs and absent otherwise.
    pub hybrid: Option<HybridProofMetadata>,
}

/// Canonical `HYNPRF02` proof model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProof {
    pub(crate) content: NativeProofContent,
    pub(crate) proof_digest: [u8; 32],
}

impl NativeProof {
    /// Validates and finalizes canonical proof content under default codec limits.
    ///
    /// # Errors
    ///
    /// Returns an error when content is noncanonical, inconsistent, or too large.
    pub fn new(content: NativeProofContent) -> Result<Self, NativeProofError> {
        super::codec::finalize_proof(content, &ProofCodecLimits::default())
    }

    /// Returns all content committed by the proof digest.
    pub const fn content(&self) -> &NativeProofContent {
        &self.content
    }

    /// Returns the digest stored in the canonical proof envelope.
    pub const fn proof_digest(&self) -> [u8; 32] {
        self.proof_digest
    }
}

/// Resource limits for encoding and decoding `HYNPRF02`.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProofCodecLimits {
    /// Maximum complete encoded proof bytes.
    pub max_proof_bytes: u64,
    /// Maximum bytes in each canonical request, result, or evidence section.
    pub max_section_bytes: u64,
    /// Maximum sum of decoded canonical section bytes.
    pub max_decoded_bytes: u64,
    /// Maximum stable object bindings.
    pub max_objects: usize,
    /// Maximum hybrid branch bindings.
    pub max_hybrid_branches: usize,
}

impl Default for ProofCodecLimits {
    fn default() -> Self {
        Self {
            max_proof_bytes: 64 * 1024 * 1024,
            max_section_bytes: 32 * 1024 * 1024,
            max_decoded_bytes: 48 * 1024 * 1024,
            max_objects: 4_096,
            max_hybrid_branches: 64,
        }
    }
}

/// Resource limits for creating and decoding `HYNWIT02`.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WitnessCodecLimits {
    /// Maximum complete encoded witness bytes.
    pub max_witness_bytes: u64,
    /// Maximum inventory entries.
    pub max_entries: usize,
    /// Maximum regular files.
    pub max_files: usize,
    /// Maximum directories below the witness root.
    pub max_directories: usize,
    /// Maximum canonical UTF-8 relative-path bytes.
    pub max_path_bytes: usize,
    /// Maximum bytes in one regular file.
    pub max_file_bytes: u64,
    /// Maximum sum of regular-file bytes.
    pub max_total_file_bytes: u64,
    /// Maximum sum of decoded path and file-content bytes.
    pub max_decoded_bytes: u64,
}

impl Default for WitnessCodecLimits {
    fn default() -> Self {
        Self {
            max_witness_bytes: MAX_NATIVE_WITNESS_BYTES,
            max_entries: 65_536,
            max_files: 32_768,
            max_directories: 32_768,
            max_path_bytes: 4_096,
            max_file_bytes: 1024 * 1024 * 1024,
            max_total_file_bytes: 3 * 1024 * 1024 * 1024,
            max_decoded_bytes: 3 * 1024 * 1024 * 1024,
        }
    }
}

/// One entry in the sorted complete witness inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeWitnessEntry {
    /// One directory below the inventory root, including empty directories.
    Directory {
        /// Canonical slash-separated relative UTF-8 path.
        path: String,
    },
    /// One complete regular file.
    File {
        /// Canonical slash-separated relative UTF-8 path.
        path: String,
        /// BLAKE3 digest of `bytes`.
        digest: [u8; 32],
        /// Complete file bytes.
        bytes: Vec<u8>,
    },
}

impl NativeWitnessEntry {
    /// Returns the canonical relative path.
    pub fn path(&self) -> &str {
        match self {
            Self::Directory { path } | Self::File { path, .. } => path,
        }
    }
}

/// Canonical complete directory witness decoded from `HYNWIT02`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDirectoryWitness {
    pub(crate) anchor: NativeProofAnchor,
    pub(crate) entries: Vec<NativeWitnessEntry>,
    pub(crate) witness_digest: [u8; 32],
}

impl NativeDirectoryWitness {
    /// Returns the state identity committed into the witness.
    pub const fn anchor(&self) -> NativeProofAnchor {
        self.anchor
    }

    /// Returns the complete sorted directory/file inventory.
    pub fn entries(&self) -> &[NativeWitnessEntry] {
        &self.entries
    }

    /// Returns the canonical witness envelope digest.
    pub const fn witness_digest(&self) -> [u8; 32] {
        self.witness_digest
    }
}

/// Successful creation of one portable single-file witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWitnessArtifact {
    /// Validated witness model.
    pub witness: NativeDirectoryWitness,
    /// Complete canonical `HYNWIT02` bytes.
    pub bytes: Vec<u8>,
}

impl NativeWitnessArtifact {
    /// Returns the reference to commit into a native proof.
    ///
    /// # Errors
    ///
    /// Returns an error if the complete witness byte length cannot be
    /// represented by the canonical unsigned 64-bit field.
    pub fn reference(&self) -> Result<NativeWitnessReference, NativeProofError> {
        Ok(NativeWitnessReference {
            digest: self.witness.witness_digest,
            file_bytes: u64::try_from(self.bytes.len())
                .map_err(|_| NativeProofError::LengthOverflow)?,
        })
    }
}

/// Scope of an offline native proof verification report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVerificationScope {
    /// Canonical proof/witness integrity and externally trusted anchor binding only.
    ArtifactIntegrity,
    /// Integrity plus native operation reexecution against the retained authority.
    SemanticReexecution,
}

/// Explicit result of origin-independent artifact verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProofVerificationReport {
    /// Verification scope established by this invocation.
    pub scope: NativeVerificationScope,
    /// Operation family accepted by the verifier.
    pub kind: NativeProofKind,
    /// Externally pinned anchor digest which matched both artifacts.
    pub anchor_digest: [u8; 32],
    /// Verified canonical proof digest.
    pub proof_digest: [u8; 32],
    /// Verified canonical witness digest.
    pub witness_digest: [u8; 32],
    /// Canonical request digest.
    pub request_digest: [u8; 32],
    /// Canonical ordered-result digest.
    pub result_digest: [u8; 32],
    /// Canonical evidence digest.
    pub evidence_digest: [u8; 32],
    /// Number of verified regular files.
    pub file_count: usize,
    /// Number of verified directories below the witness root.
    pub directory_count: usize,
    /// Sum of verified regular-file bytes.
    pub total_file_bytes: u64,
    /// True only after successful native authority reopen and operation reexecution.
    pub semantic_reexecution_performed: bool,
}

/// Combined resource limits for origin-independent verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeVerificationLimits {
    /// Proof codec bounds.
    pub proof: ProofCodecLimits,
    /// Witness codec bounds.
    pub witness: WitnessCodecLimits,
    /// Maximum materialized result items during semantic reexecution.
    pub max_reexecution_result_items: usize,
    /// Maximum candidate/work items during semantic reexecution.
    pub max_reexecution_candidate_items: usize,
    /// Maximum canonical request/result/evidence bytes rebuilt by reexecution.
    pub max_reexecution_bytes: u64,
}

impl Default for NativeVerificationLimits {
    fn default() -> Self {
        Self {
            proof: ProofCodecLimits::default(),
            witness: WitnessCodecLimits::default(),
            max_reexecution_result_items: 10_000,
            max_reexecution_candidate_items: 100_000,
            max_reexecution_bytes: 48 * 1024 * 1024,
        }
    }
}

/// Explicit resource envelope used while generating one operation proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProofGenerationLimits {
    /// Limits committed into the generated proof.
    pub admitted: AdmittedProofLimits,
    /// Proof codec bounds.
    pub proof: ProofCodecLimits,
    /// Witness capture bounds.
    pub witness: WitnessCodecLimits,
}

impl Default for NativeProofGenerationLimits {
    fn default() -> Self {
        Self {
            admitted: AdmittedProofLimits {
                result_items: 10_000,
                candidate_items: 100_000,
                evidence_bytes: 32 * 1024 * 1024,
            },
            proof: ProofCodecLimits::default(),
            witness: WitnessCodecLimits::default(),
        }
    }
}

/// Complete portable output of one operation-integrated proof generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeOperationProofArtifact {
    /// Validated proof model.
    pub proof: NativeProof,
    /// Complete canonical `HYNPRF02` bytes.
    pub proof_bytes: Vec<u8>,
    /// Complete canonical retained native authority.
    pub witness_bytes: Vec<u8>,
    /// Anchor to pin independently before offline verification.
    pub trusted_anchor: ExternalTrustedAnchor,
}

/// Failure while building, coding, or verifying a native proof artifact.
#[derive(Debug)]
pub enum NativeProofError {
    /// Filesystem operation failed.
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Operating-system failure.
        source: io::Error,
    },
    /// An output destination already exists and was not replaced.
    DestinationExists(PathBuf),
    /// The requested witness origin is not a directory.
    OriginNotDirectory(PathBuf),
    /// Canonical format or model invariant failed.
    Invalid(&'static str),
    /// A newer format version was encountered.
    UnsupportedVersion {
        /// Version in the artifact.
        found: u16,
        /// Highest accepted version.
        supported: u16,
    },
    /// A caller or implementation resource bound was exceeded.
    LimitExceeded {
        /// Stable resource identity.
        resource: &'static str,
        /// Observed amount.
        actual: u64,
        /// Admitted maximum.
        maximum: u64,
    },
    /// A length or count cannot be represented safely.
    LengthOverflow,
    /// Fast envelope corruption check failed.
    ChecksumMismatch,
    /// Canonical BLAKE3 digest verification failed.
    DigestMismatch(&'static str),
    /// Neither artifact matches the caller's external trusted anchor.
    TrustedAnchorMismatch,
    /// Proof and witness carry different native state identities.
    WitnessAnchorMismatch,
    /// The supplied witness differs from the exact artifact referenced by the proof.
    WitnessReferenceMismatch,
}

impl fmt::Display for NativeProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "native proof I/O failed for {}: {source}",
                    path.display()
                )
            }
            Self::DestinationExists(path) => write!(
                formatter,
                "native witness destination already exists: {}",
                path.display()
            ),
            Self::OriginNotDirectory(path) => write!(
                formatter,
                "native witness origin is not a directory: {}",
                path.display()
            ),
            Self::Invalid(reason) => write!(formatter, "invalid native proof artifact: {reason}"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported native proof artifact version {found}; supported version is {supported}"
            ),
            Self::LimitExceeded {
                resource,
                actual,
                maximum,
            } => write!(
                formatter,
                "native proof limit exceeded for {resource}: {actual} > {maximum}"
            ),
            Self::LengthOverflow => formatter.write_str("native proof length overflow"),
            Self::ChecksumMismatch => formatter.write_str("native proof artifact CRC32C mismatch"),
            Self::DigestMismatch(resource) => {
                write!(
                    formatter,
                    "native proof artifact digest mismatch: {resource}"
                )
            }
            Self::TrustedAnchorMismatch => formatter
                .write_str("native proof anchor does not match the external trusted anchor"),
            Self::WitnessAnchorMismatch => {
                formatter.write_str("native proof and witness anchors differ")
            }
            Self::WitnessReferenceMismatch => {
                formatter.write_str("native witness does not match the proof reference")
            }
        }
    }
}

impl std::error::Error for NativeProofError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub(crate) fn io_error(path: impl Into<PathBuf>, source: io::Error) -> NativeProofError {
    NativeProofError::Io {
        path: path.into(),
        source,
    }
}

pub(crate) fn limit(
    resource: &'static str,
    actual: impl TryInto<u64>,
    maximum: impl TryInto<u64>,
) -> NativeProofError {
    NativeProofError::LimitExceeded {
        resource,
        actual: actual.try_into().unwrap_or(u64::MAX),
        maximum: maximum.try_into().unwrap_or(u64::MAX),
    }
}
