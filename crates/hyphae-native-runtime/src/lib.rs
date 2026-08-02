// SPDX-License-Identifier: Apache-2.0

//! First executable convergence slice for Hyphae's native local data engine.
//!
//! This crate proves one data directory, one page store, one WAL authority,
//! one MVCC publication, and independently owned relational, structure, and
//! lexical-search state. Its deliberately small operation surface is not a
//! claim of complete SQL, Valkey, or `OpenSearch` compatibility.

mod ann_store;
mod local_protocol;
mod model;
mod sql;
mod wal_codec;

pub use hyphae_native_ann::{
    HnswConfig, Metric as VectorMetric, SearchOptions as AnnSearchOptions, Vector, VectorHit,
};
pub use local_protocol::{
    DEFAULT_MAX_FRAME_PAYLOAD, DecodedFrame, FrameKind, LOCAL_FRAME_HEADER_SIZE,
    LocalProtocolError, decode_frame, encode_frame,
};
pub use sql::{PreparedStatement, SqlError, SqlResult, SqlValue};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    ops::{Bound, ControlFlow, Deref, DerefMut},
    path::{Path, PathBuf},
};

use hyphae_native_blobs::{BlobError, BlobStore, StagedBlob};
use hyphae_native_btree::{BTREE_MAX_KEY_SIZE, BTree, BTreeError};
use hyphae_native_catalog::{
    AnnIndexDefinition, CatalogError, CatalogName, CatalogObject, ColumnDefinition, ObjectHeader,
    QualifiedName, RelationDefinition, SearchCollectionDefinition, SearchFieldDefinition,
    SecondaryIndexDefinition, VectorMetric as CatalogVectorMetric,
};
use hyphae_native_manifest::{ManifestError, RootManifest, RootManifestStore};
use hyphae_native_mvcc::{
    CommitCoordinator, ConflictTable, MvccError, RootSet, RootSlot, RootTransaction, Snapshot,
    WalAnchor, WriteConflict, WriteKey,
};
use hyphae_native_pages::{BufferPool, BufferPoolError, PageKind, PageStore, PageStoreError};
use hyphae_native_records::{
    BlobReference, ColumnValueRef, RecordError, RowRecord, RowRecordView, RowTupleView,
    RowVersionPointer,
};
use hyphae_native_types::{
    CatalogVersion, ColumnId, Csn, DurabilityClass, EngineKind, FieldId, LogicalType, Lsn,
    ManifestGeneration, ObjectId, PageId, RowId, ScalarValue, TransactionId, VectorElement,
    VectorType,
};
use hyphae_native_wal::{WalError, WalFile, WalRecovery};
use thiserror::Error;

use crate::{
    model::{
        CatalogState, ModelError, RelationState, SearchState, StructureEntry, StructureState,
        TtlValue, analyze, bm25_idf, bm25_term_score,
    },
    wal_codec::{
        Mutation, Opcode, RecoveredWal, TransactionPlan, WalSemanticError, encode_checkpoint,
        encode_transaction, recover_wal,
    },
};

const PAGE_FILE: &str = "pages.hydb";
const WAL_FILE: &str = "wal.hywal";
const RELATIONAL_FORMAT_KEY: &[u8] = b"\x00";
const RELATIONAL_FORMAT_VALUE_V1: &[u8] = b"HYRELBT1";
const RELATIONAL_FORMAT_VALUE_V2: &[u8] = b"HYRELBT2";
const RELATIONAL_TABLE_PREFIX: u8 = 1;
const RELATIONAL_ROW_PREFIX: u8 = 2;
const RELATIONAL_SECONDARY_INDEX_PREFIX: u8 = 3;
const RELATIONAL_SECONDARY_ENTRY_PREFIX: u8 = 4;
const RELATIONAL_SECONDARY_INDEX_MAGIC: &[u8; 8] = b"HYRIDX01";
const RELATIONAL_SECONDARY_INDEX_META_SIZE: usize = 32;
const RELATIONAL_SECONDARY_ENTRY_TOMBSTONE: u8 = 0;
const RELATIONAL_SECONDARY_ENTRY_LIVE: u8 = 1;
const RELATIONAL_VALUE_INLINE: u8 = 0;
const RELATIONAL_VALUE_BLOB: u8 = 1;
const RELATIONAL_INLINE_VALUE_LIMIT: usize = 8_192;
const STRUCTURE_FORMAT_KEY: &[u8] = b"\x00";
const STRUCTURE_FORMAT_VALUE_V1: &[u8] = b"HYSTRBT1";
const STRUCTURE_ENTRY_PREFIX: u8 = 1;
const STRUCTURE_HASH_META_PREFIX: u8 = 2;
const STRUCTURE_HASH_FIELD_PREFIX: u8 = 3;
const STRUCTURE_SET_META_PREFIX: u8 = 4;
const STRUCTURE_SET_MEMBER_PREFIX: u8 = 5;
const STRUCTURE_VALUE_MAGIC: &[u8; 8] = b"HYSTRV01";
const STRUCTURE_HASH_META_MAGIC: &[u8; 8] = b"HYHSHM01";
const STRUCTURE_SET_META_MAGIC: &[u8; 8] = b"HYSETM01";
const STRUCTURE_HASH_META_SIZE: usize = 16;
const STRUCTURE_SET_META_SIZE: usize = 16;
const STRUCTURE_VALUE_HEADER_SIZE: usize = 24;
const STRUCTURE_VALUE_HAS_EXPIRY: u8 = 1;
const STRUCTURE_VALUE_TOMBSTONE: u8 = 2;
const STRUCTURE_VALUE_INLINE: u8 = 0;
const STRUCTURE_VALUE_BLOB: u8 = 1;
const STRUCTURE_INLINE_VALUE_LIMIT: usize = 8_192;
const SEARCH_FORMAT_KEY: &[u8] = b"\x00";
const SEARCH_FORMAT_VALUE_V1: &[u8] = b"HYSEABT1";
const SEARCH_INDEX_META_PREFIX: u8 = 1;
const SEARCH_DOCUMENT_PREFIX: u8 = 2;
const SEARCH_TERM_META_PREFIX: u8 = 3;
const SEARCH_POSTING_PREFIX: u8 = 4;
const SEARCH_INDEX_META_MAGIC: &[u8; 8] = b"HYIDX001";
const SEARCH_DOCUMENT_MAGIC: &[u8; 8] = b"HYDOCS01";
const SEARCH_TERM_META_MAGIC: &[u8; 8] = b"HYTERM01";
const SEARCH_POSTING_MAGIC: &[u8; 8] = b"HYPOST01";
const SEARCH_INDEX_META_SIZE: usize = 24;
const SEARCH_DOCUMENT_HEADER_SIZE: usize = 24;
const SEARCH_TERM_META_SIZE: usize = 16;
const SEARCH_POSTING_SIZE: usize = 16;
const SEARCH_DOCUMENT_INLINE: u8 = 0;
const SEARCH_DOCUMENT_BLOB: u8 = 1;
const SEARCH_INLINE_VALUE_LIMIT: usize = 8_192;
const DEFAULT_BUFFER_POOL_FRAMES: usize = 1_024;
const DEFAULT_BUFFER_POOL_PARTITIONS: usize = 16;
const SLOT_CATALOG: RootSlot = RootSlot {
    engine: EngineKind::Kernel,
    partition: 0,
};
const SLOT_RELATIONAL: RootSlot = RootSlot {
    engine: EngineKind::Relational,
    partition: 0,
};
const SLOT_STRUCTURE: RootSlot = RootSlot {
    engine: EngineKind::Structure,
    partition: 0,
};
const SLOT_SEARCH: RootSlot = RootSlot {
    engine: EngineKind::Search,
    partition: 0,
};
const ROOT_SLOTS: [RootSlot; 4] = [SLOT_CATALOG, SLOT_RELATIONAL, SLOT_STRUCTURE, SLOT_SEARCH];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelationalFormat {
    InlineRowV1,
    VersionChainV2,
}

impl RelationalFormat {
    const fn marker(self) -> &'static [u8] {
        match self {
            Self::InlineRowV1 => RELATIONAL_FORMAT_VALUE_V1,
            Self::VersionChainV2 => RELATIONAL_FORMAT_VALUE_V2,
        }
    }

    fn decode(marker: &[u8]) -> Result<Self, NativeRuntimeError> {
        match marker {
            RELATIONAL_FORMAT_VALUE_V1 => Ok(Self::InlineRowV1),
            RELATIONAL_FORMAT_VALUE_V2 => Ok(Self::VersionChainV2),
            _ => Err(NativeRuntimeError::InvalidRelationalTree),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructureFormat {
    InlineStateV1,
    BTreeV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchFormat {
    InlineStateV1,
    InvertedBTreeV1,
}

/// Native runtime or recovery failure.
#[derive(Debug, Error)]
pub enum NativeRuntimeError {
    /// Native ANN validation, construction, or query failed.
    #[error(transparent)]
    Ann(#[from] hyphae_native_ann::AnnError),
    /// Filesystem operation outside the page/WAL files failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Native page storage failed.
    #[error(transparent)]
    Page(#[from] PageStoreError),
    /// Native buffer-pool access failed.
    #[error(transparent)]
    BufferPool(#[from] BufferPoolError),
    /// Native immutable blob storage failed.
    #[error(transparent)]
    Blob(#[from] BlobError),
    /// Native relational B+tree storage failed.
    #[error(transparent)]
    BTree(#[from] BTreeError),
    /// Native canonical row-record handling failed.
    #[error(transparent)]
    Record(#[from] RecordError),
    /// Native WAL framing failed.
    #[error(transparent)]
    Wal(#[from] WalError),
    /// Native WAL transaction semantics failed.
    #[error("native transaction WAL semantics failed: {0}")]
    WalSemantic(String),
    /// Native MVCC coordination failed.
    #[error(transparent)]
    Mvcc(#[from] MvccError),
    /// First-committer-wins rejected a stale logical write.
    #[error(transparent)]
    WriteConflict(#[from] WriteConflict),
    /// Native immutable root-manifest handling failed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// Catalog definition validation failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// Engine-state codec or semantic validation failed.
    #[error("native engine state failed: {0}")]
    Model(String),
    /// A non-null native unique secondary-index key already identifies a row.
    #[error("native relational unique secondary index is violated")]
    UniqueSecondaryIndexViolation,
    /// The requested native secondary index does not exist in the current root.
    #[error("native relational secondary index {index} does not exist")]
    UnknownSecondaryIndex {
        /// Stable catalog identity requested by the caller.
        index: ObjectId,
    },
    /// The requested native relation does not exist in the current root.
    #[error("native relation {table} does not exist")]
    UnknownRelation {
        /// Stable catalog identity requested by the caller.
        table: ObjectId,
    },
    /// The requested data directory already exists.
    #[error("native data directory already exists")]
    DataDirectoryExists,
    /// A committed root is missing or has the wrong page kind.
    #[error("committed native root is missing or has the wrong page kind")]
    InvalidCommittedRoot,
    /// A committed page was created after the commit that references it.
    #[error("committed native page has a future creating CSN")]
    FuturePage,
    /// The relational B+tree contains malformed namespace keys or markers.
    #[error("native relational B+tree namespace is invalid")]
    InvalidRelationalTree,
    /// The structure B+tree contains malformed namespace keys or values.
    #[error("native structure B+tree namespace is invalid")]
    InvalidStructureTree,
    /// The lexical-search B+tree contains malformed metadata or postings.
    #[error("native lexical-search B+tree namespace is invalid")]
    InvalidSearchTree,
    /// The ANN B+tree namespace or canonical graph generation is invalid.
    #[error("native ANN tree is invalid")]
    InvalidAnnTree,
    /// The requested native ANN index does not exist.
    #[error("native ANN index {index} does not exist")]
    UnknownVectorIndex {
        /// Stable search-index identity requested by the caller.
        index: ObjectId,
    },
    /// A detached write mutation cannot be reapplied to the admitted base.
    #[error("native optimistic write batch contains an invalid mutation")]
    InvalidPreparedMutation,
    /// A structure value is not one canonical signed decimal integer.
    #[error("native structure value is not a canonical signed 64-bit integer")]
    StructureValueNotInteger,
    /// A signed structure counter operation exceeded the i64 domain.
    #[error("native signed 64-bit structure counter overflow")]
    StructureIntegerOverflow,
    /// One structure key was addressed through a different family.
    #[error("native structure key belongs to a different family")]
    StructureKindMismatch,
    /// The requested native hash does not exist.
    #[error("native structure hash does not exist")]
    UnknownStructureHash,
    /// The requested native set does not exist.
    #[error("native structure set does not exist")]
    UnknownStructureSet,
    /// The requested structure key already exists.
    #[error("native structure key already exists")]
    StructureKeyExists,
    /// Collection families are unavailable in a legacy whole-state directory.
    #[error("native collection families require the B+tree structure format")]
    LegacyStructureFamilyUnsupported,
    /// A composite structure identity cannot fit its canonical u32 length.
    #[error("native structure identity exceeds its canonical length field")]
    StructureIdentityTooLarge,
    /// A search document or analyzed term cannot fit one canonical B+tree key.
    #[error("native search identity exceeds the canonical B+tree key limit")]
    SearchIdentityTooLarge,
    /// Recovered commit sequences are not contiguous.
    #[error("recovered native commit sequence is not contiguous")]
    NoncontiguousCommitSequence,
    /// A checkpoint does not match its manifest, WAL commit, or root set.
    #[error("native checkpoint does not match the verified manifest/WAL chain")]
    InvalidCheckpoint,
    /// No committed root set exists to checkpoint.
    #[error("native checkpoint requires at least one committed transaction")]
    NoCommittedState,
    /// Transaction identity space is exhausted.
    #[error("native transaction identity space is exhausted")]
    TransactionIdExhausted,
    /// A deterministic crash-matrix interruption was requested.
    #[error("native commit interrupted at {0:?}; reopen the data directory")]
    InjectedCrash(CommitBoundary),
    /// A deterministic checkpoint interruption was requested.
    #[error("native checkpoint interrupted at {0:?}; reopen the data directory")]
    InjectedCheckpointCrash(CheckpointBoundary),
}

impl From<WalSemanticError> for NativeRuntimeError {
    fn from(source: WalSemanticError) -> Self {
        Self::WalSemantic(source.to_string())
    }
}

impl From<ModelError> for NativeRuntimeError {
    fn from(source: ModelError) -> Self {
        match source {
            ModelError::UniqueSecondaryIndexViolation => Self::UniqueSecondaryIndexViolation,
            source => Self::Model(source.to_string()),
        }
    }
}

/// Deterministic commit boundary used by the native crash matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitBoundary {
    /// Large values are synchronized only under temporary blob names.
    BlobStaged,
    /// Immutable blobs are promoted but no pages or WAL transaction exist.
    BlobPromoted,
    /// New copy-on-write pages exist but have not been synchronized.
    PageAppended,
    /// New pages are synchronized but no WAL transaction exists.
    PageSynchronized,
    /// A complete WAL transaction is appended but not explicitly synchronized.
    WalAppended,
    /// The WAL transaction is synchronized but roots are not published in memory.
    WalSynchronized,
    /// The complete root set has been published.
    RootPublished,
}

/// Deterministic root-checkpoint boundary used by the native crash matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointBoundary {
    /// A synchronized create-new manifest exists only under its temporary name.
    ManifestStaged,
    /// The immutable manifest is published but not WAL-anchored.
    ManifestPublished,
    /// The WAL checkpoint record is appended but not explicitly synchronized.
    WalAppended,
    /// The manifest and its WAL checkpoint record are synchronized.
    WalSynchronized,
}

/// TTL state for one native structure key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ttl {
    /// No value exists for this key.
    Missing,
    /// The value exists without an expiry.
    Persistent,
    /// The value exists with this nonnegative remaining duration.
    RemainingMicros(i64),
}

/// Predicate evaluated by one native scalar `SET`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetCondition {
    /// Always replace the visible value.
    Always,
    /// Apply only when no unexpired value exists.
    IfAbsent,
    /// Apply only when an unexpired value exists.
    IfPresent,
}

/// Result of evaluating one conditional scalar `SET`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetOutcome {
    /// The private write set now contains the mutation.
    Applied,
    /// The condition was false and no mutation was added.
    NotApplied,
}

/// Result of one native hash-field upsert.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashSetOutcome {
    /// The field did not exist and increased the hash cardinality.
    Added,
    /// The field existed and its value was replaced.
    Updated,
}

/// Reopen evidence for the native data directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Incomplete page-file tail removed during open.
    pub page_tail_bytes_removed: u64,
    /// Incomplete WAL-block tail removed during open.
    pub wal_tail_bytes_removed: u64,
    /// Number of semantically verified committed transactions.
    pub committed_transactions: usize,
    /// Latest recovered visible CSN.
    pub visible_csn: Option<Csn>,
    /// Number of verified immutable root manifests.
    pub manifest_count: usize,
    /// Number of semantically verified WAL checkpoint anchors.
    pub checkpoint_count: usize,
    /// Number of interrupted temporary manifests removed during open.
    pub recovered_temporary_manifests: usize,
    /// Complete manifest suffix not yet referenced by a WAL checkpoint.
    pub unanchored_manifest_suffix: usize,
    /// Latest verified checkpoint generation.
    pub latest_checkpoint_generation: Option<ManifestGeneration>,
    /// Number of verified immutable blob files.
    pub blob_count: usize,
    /// Physical blob generation derived from the verified namespace.
    pub blob_generation: u64,
    /// Interrupted temporary blob files removed during open.
    pub recovered_temporary_blobs: usize,
}

/// Receipt for one cross-engine native commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    /// Transaction identity in the WAL.
    pub transaction_id: TransactionId,
    /// CSN shared by every affected native engine.
    pub commit_csn: Csn,
    /// Catalog snapshot published by the commit.
    pub catalog_version: CatalogVersion,
    /// Physical LSN of the terminal commit record.
    pub commit_lsn: hyphae_native_types::Lsn,
    /// Digest of the complete WAL block containing the commit record.
    pub wal_block_digest: [u8; 32],
    /// Durability promise used for acknowledgement.
    pub durability: DurabilityClass,
}

/// Receipt for one synchronized immutable root checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointReceipt {
    /// Transaction identity used by the standalone WAL checkpoint record.
    pub transaction_id: TransactionId,
    /// Visible all-engine commit captured by the manifest.
    pub visible_csn: Csn,
    /// Immutable manifest generation.
    pub manifest_generation: ManifestGeneration,
    /// Digest of the complete immutable manifest.
    pub manifest_digest: [u8; 32],
    /// Physical LSN of the checkpoint record.
    pub checkpoint_lsn: Lsn,
    /// Whether this platform implements strict parent-directory synchronization.
    pub parent_directory_sync_supported: bool,
}
/// One lexical match result ordered by descending BM25 score.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchHit {
    /// Stable document ID supplied by the caller.
    pub document_id: Vec<u8>,
    /// Native BM25 score.
    pub score: f64,
}

/// Runtime receipt for one approximate native vector query.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnSearchReceipt {
    /// Stable catalog identity of the queried vector index.
    pub index_id: ObjectId,
    /// Immutable all-engine CSN used by the query.
    pub snapshot_csn: Option<Csn>,
    /// Explicit approximation label.
    pub approximate: bool,
    /// Canonical physical graph-generation identity.
    pub build_identity: [u8; 32],
    /// Fixed index metric.
    pub metric: VectorMetric,
    /// Effective graph-search breadth.
    pub ef_search: usize,
    /// Layer-zero candidates retained before final truncation.
    pub candidate_count: usize,
    /// Whether candidates were exactly rescored.
    pub exact_reranked: bool,
    /// Distinct graph nodes whose distance was evaluated.
    pub visited_nodes: usize,
    /// Ordered vector hits.
    pub hits: Vec<VectorHit>,
}

/// One row reached through an exact native secondary-index key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondaryIndexRow {
    /// Relation identified by the secondary-index metadata.
    pub table: ObjectId,
    /// Canonical physical primary-key bytes.
    pub primary_key: Vec<u8>,
    /// Visible canonical row bytes.
    pub row: Vec<u8>,
}

/// One row emitted by a bounded native relational primary-key scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationalScanRow {
    /// Relation selected by the scan.
    pub table: ObjectId,
    /// Canonical physical primary-key bytes and exclusive resume cursor.
    pub primary_key: Vec<u8>,
    /// Visible canonical row bytes.
    pub row: Vec<u8>,
}

pub(crate) enum RelationalVisitError<E> {
    Runtime(NativeRuntimeError),
    Visitor(E),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct MaterializedState {
    catalog: CatalogState,
    relational: RelationState,
    structures: StructureState,
    search: SearchState,
    ann: ann_store::AnnState,
}

/// Immutable logical snapshot spanning all three native engines.
#[derive(Clone, Debug)]
pub struct NativeSnapshot {
    metadata: Snapshot,
    state: MaterializedState,
}

impl NativeSnapshot {
    /// Returns the latest commit visible to every engine in this snapshot.
    pub const fn visible_csn(&self) -> Option<Csn> {
        self.metadata.visible_csn
    }

    /// Returns the catalog version pinned by this snapshot.
    pub const fn catalog_version(&self) -> CatalogVersion {
        self.metadata.catalog_version
    }

    /// Returns one immutable catalog object definition pinned by this
    /// snapshot.
    pub fn catalog_object(&self, id: ObjectId) -> Option<&CatalogObject> {
        self.state.catalog.object(id)
    }

    /// Performs a relational primary-key lookup.
    pub fn select(&self, table: ObjectId, primary_key: &[u8]) -> Option<&[u8]> {
        self.state.relational.select(table, primary_key)
    }

    /// Returns a structure value unless it is expired at snapshot logical time.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.state
            .structures
            .get(key, self.metadata.logical_time_micros)
    }

    /// Returns the key's TTL state at snapshot logical time.
    pub fn ttl(&self, key: &[u8]) -> Ttl {
        match self
            .state
            .structures
            .ttl_micros(key, self.metadata.logical_time_micros)
        {
            None => Ttl::Missing,
            Some(TtlValue::Persistent) => Ttl::Persistent,
            Some(TtlValue::Remaining(value)) => Ttl::RemainingMicros(value),
        }
    }

    /// Reads one field from an existing native hash in this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar key or a missing hash.
    pub fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.sets.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        if !self.state.structures.hashes.contains_key(key) {
            return Err(NativeRuntimeError::UnknownStructureHash);
        }
        Ok(self.state.structures.hget(key, field))
    }

    /// Returns one native hash's field count in this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar key or a missing hash.
    pub fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.sets.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .hlen(key)
            .ok_or(NativeRuntimeError::UnknownStructureHash)
    }

    /// Tests exact membership in an existing native set in this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar/hash key or a missing set.
    pub fn sismember(&self, key: &[u8], member: &[u8]) -> Result<bool, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.hashes.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .sismember(key, member)
            .ok_or(NativeRuntimeError::UnknownStructureSet)
    }

    /// Returns one native set's exact member count in this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar/hash key or a missing set.
    pub fn scard(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.hashes.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .scard(key)
            .ok_or(NativeRuntimeError::UnknownStructureSet)
    }

    /// Executes deterministic native lexical matching.
    ///
    /// # Errors
    ///
    /// Returns an error when the search collection does not exist.
    pub fn match_text(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MatchHit>, NativeRuntimeError> {
        Ok(self
            .state
            .search
            .search(index, query, limit)?
            .into_iter()
            .map(|(document_id, score)| MatchHit { document_id, score })
            .collect())
    }

    /// Executes approximate native vector search against this immutable
    /// all-engine snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index, invalid query vector, or
    /// query breadth outside the catalog definition.
    pub fn search_ann(
        &self,
        index: ObjectId,
        query: &Vector,
        options: AnnSearchOptions,
    ) -> Result<AnnSearchReceipt, NativeRuntimeError> {
        let result = self.state.ann.search(index, query, options)?;
        Ok(ann_search_receipt(index, self.metadata.visible_csn, result))
    }

    /// Executes the complete exact vector-ranking oracle against this
    /// immutable all-engine snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index or invalid query vector.
    pub fn search_vector_exact(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        self.state.ann.search_exact(index, query, k)
    }

    /// Lexes, parses, and catalog-binds one parameterized native SQL `SELECT`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported syntax or an unknown relation.
    pub fn prepare_sql(&self, statement: &str) -> Result<PreparedStatement, SqlError> {
        sql::prepare(self, statement)
    }

    /// Executes one catalog-bound native SQL plan.
    ///
    /// # Errors
    ///
    /// Returns an error for stale catalog binding, invalid parameters, or
    /// native execution failure.
    pub fn execute_prepared(
        &self,
        prepared: &PreparedStatement,
        parameters: &[SqlValue],
    ) -> Result<SqlResult, SqlError> {
        sql::execute_prepared(self, prepared, parameters)
    }

    /// Executes an allocation-free binary primary-key prepared lookup.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog binding is stale.
    pub fn execute_prepared_binary<'snapshot>(
        &'snapshot self,
        prepared: &PreparedStatement,
        primary_key: &[u8],
    ) -> Result<Option<&'snapshot [u8]>, SqlError> {
        sql::execute_prepared_binary(self, prepared, primary_key)
    }
}

fn ann_search_receipt(
    index_id: ObjectId,
    snapshot_csn: Option<Csn>,
    result: hyphae_native_ann::AnnSearchResult,
) -> AnnSearchReceipt {
    AnnSearchReceipt {
        index_id,
        snapshot_csn,
        approximate: result.approximate,
        build_identity: result.build_identity,
        metric: result.metric,
        ef_search: result.ef_search,
        candidate_count: result.candidate_count,
        exact_reranked: result.exact_reranked,
        visited_nodes: result.visited_nodes,
        hits: result.hits,
    }
}

/// One Hyphae-owned native data directory.
#[derive(Debug)]
pub struct NativeDatabase {
    data_directory: PathBuf,
    pages: PageStore,
    buffer_pool: BufferPool,
    blobs: BlobStore,
    wal: WalFile,
    manifests: RootManifestStore,
    coordinator: CommitCoordinator,
    conflicts: ConflictTable,
    relational_format: RelationalFormat,
    structure_format: StructureFormat,
    search_format: SearchFormat,
    next_transaction_id: u128,
    last_checkpoint_lsn: Option<Lsn>,
    recovery: RecoveryReport,
}

impl NativeDatabase {
    /// Creates a new empty native data directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the target already exists or cannot be initialized.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, NativeRuntimeError> {
        let path = path.as_ref();
        if path.exists() {
            return Err(NativeRuntimeError::DataDirectoryExists);
        }
        fs::create_dir(path)?;
        let pages = PageStore::create(path.join(PAGE_FILE))?;
        let buffer_pool =
            BufferPool::new(DEFAULT_BUFFER_POOL_FRAMES, DEFAULT_BUFFER_POOL_PARTITIONS)?;
        let blobs = BlobStore::create(path)?;
        let wal = WalFile::create(path.join(WAL_FILE))?;
        let manifests = RootManifestStore::create(path)?;
        let coordinator = CommitCoordinator::new(
            CatalogVersion::new(1).map_err(|_| NativeRuntimeError::InvalidCommittedRoot)?,
        );
        Ok(Self {
            data_directory: path.to_path_buf(),
            pages,
            buffer_pool,
            blobs,
            wal,
            manifests,
            coordinator,
            conflicts: ConflictTable::default(),
            relational_format: RelationalFormat::VersionChainV2,
            structure_format: StructureFormat::BTreeV1,
            search_format: SearchFormat::InvertedBTreeV1,
            next_transaction_id: 1,
            last_checkpoint_lsn: None,
            recovery: RecoveryReport {
                page_tail_bytes_removed: 0,
                wal_tail_bytes_removed: 0,
                committed_transactions: 0,
                visible_csn: None,
                manifest_count: 0,
                checkpoint_count: 0,
                recovered_temporary_manifests: 0,
                unanchored_manifest_suffix: 0,
                latest_checkpoint_generation: None,
                blob_count: 0,
                blob_generation: 0,
                recovered_temporary_blobs: 0,
            },
        })
    }

    /// Opens, verifies, and recovers an existing native data directory.
    ///
    /// # Errors
    ///
    /// Returns an error for any complete corruption, malformed committed
    /// transaction, missing referenced page, or noncontiguous CSN.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, NativeRuntimeError> {
        let path = path.as_ref();
        let opened_pages = PageStore::open_repair_tail(path.join(PAGE_FILE))?;
        let buffer_pool =
            BufferPool::new(DEFAULT_BUFFER_POOL_FRAMES, DEFAULT_BUFFER_POOL_PARTITIONS)?;
        let blobs = BlobStore::open(path)?;
        let blob_recovery = blobs.recovery()?;
        let opened_wal = WalFile::open(path.join(WAL_FILE))?;
        let recovered_wal = recover_wal(&opened_wal.recovery.records)?;
        let commits = &recovered_wal.commits;
        validate_commit_sequence(commits)?;
        let mut conflicts = ConflictTable::default();
        for recovered in commits {
            let keys = mutation_write_keys(&recovered.mutations);
            conflicts.validate(recovered.manifest.read_csn, &keys)?;
            conflicts.publish_committed(recovered.manifest.commit_csn, keys);
        }
        let manifests = RootManifestStore::open(path)?;
        let manifest_recovery = manifests.recovery();
        let mut latest_root = None;
        let mut committed_roots = BTreeMap::new();
        for recovered in commits {
            let anchor_digest = digest_for_lsn(&opened_wal.recovery, recovered.commit_lsn)?;
            let roots = root_map(recovered.manifest.roots);
            let root = RootSet::committed(
                recovered.manifest.commit_csn,
                recovered.manifest.catalog_version,
                WalAnchor::new(recovered.commit_lsn, anchor_digest)?,
                roots,
                recovered.manifest.blob_generation,
            )?;
            if root.blob_generation() > blob_recovery.generation {
                return Err(NativeRuntimeError::InvalidCommittedRoot);
            }
            validate_roots(
                &opened_pages.store,
                &blobs,
                &root,
                recovered.manifest.commit_csn,
            )?;
            committed_roots.insert(recovered.manifest.commit_csn, root.clone());
            latest_root = Some(root);
        }
        let checkpoint_validation = validate_checkpoints(
            &recovered_wal,
            &manifest_recovery.manifests,
            &committed_roots,
        )?;
        let (relational_format, structure_format, search_format) =
            formats_for_latest_root(&opened_pages.store, latest_root.as_ref())?;
        let coordinator = if let Some(root) = latest_root {
            CommitCoordinator::restore(root)?
        } else {
            CommitCoordinator::new(
                CatalogVersion::new(1).map_err(|_| NativeRuntimeError::InvalidCommittedRoot)?,
            )
        };
        let next_transaction_id = opened_wal
            .recovery
            .records
            .iter()
            .map(|record| record.transaction_id().get())
            .max()
            .map_or(Ok(1_u128), |transaction_id| {
                transaction_id
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::TransactionIdExhausted)
            })?;
        let visible_csn = commits.last().map(|commit| commit.manifest.commit_csn);
        Ok(Self {
            data_directory: path.to_path_buf(),
            pages: opened_pages.store,
            buffer_pool,
            blobs,
            wal: opened_wal.wal,
            manifests,
            coordinator,
            conflicts,
            relational_format,
            structure_format,
            search_format,
            next_transaction_id,
            last_checkpoint_lsn: checkpoint_validation.last_checkpoint_lsn,
            recovery: RecoveryReport {
                page_tail_bytes_removed: opened_pages.truncated_tail_bytes,
                wal_tail_bytes_removed: opened_wal.recovery.truncated_tail_bytes,
                committed_transactions: commits.len(),
                visible_csn,
                manifest_count: manifest_recovery.manifests.len(),
                checkpoint_count: recovered_wal.checkpoints.len(),
                recovered_temporary_manifests: manifest_recovery.ignored_temporary_files,
                unanchored_manifest_suffix: checkpoint_validation.unanchored_manifest_suffix,
                latest_checkpoint_generation: checkpoint_validation.latest_generation,
                blob_count: blob_recovery.blob_count,
                blob_generation: blob_recovery.generation,
                recovered_temporary_blobs: blob_recovery.recovered_temporary_files,
            },
        })
    }

    /// Returns the owned data-directory path.
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    /// Returns recovery evidence produced when this handle was opened.
    pub const fn recovery_report(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// Materializes one immutable all-engine read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for synchronization, page, root, or state corruption.
    pub fn snapshot(&self, logical_time_micros: i64) -> Result<NativeSnapshot, NativeRuntimeError> {
        let metadata = self.coordinator.snapshot(logical_time_micros)?;
        let state = load_state(&self.pages, &self.blobs, metadata.roots())?;
        Ok(NativeSnapshot { metadata, state })
    }

    /// Binds one parameterized SQL `SELECT` against only the current catalog.
    ///
    /// Unlike [`Self::snapshot`], this does not materialize any relational,
    /// structure, or search state.
    ///
    /// # Errors
    ///
    /// Returns an error for snapshot coordination, catalog corruption,
    /// unsupported syntax, or an unknown relation.
    pub fn prepare_sql_latest(&self, statement: &str) -> Result<PreparedStatement, SqlError> {
        let metadata = self
            .coordinator
            .snapshot(0)
            .map_err(NativeRuntimeError::from)?;
        let catalog = load_catalog_state(&self.pages, metadata.roots())?;
        sql::prepare_catalog(metadata.catalog_version, &catalog, statement)
    }

    /// Executes one current catalog-bound SQL plan through physical roots.
    ///
    /// The execution captures one root-set snapshot and does not materialize
    /// complete engine state.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale catalog binding, invalid parameters,
    /// missing index metadata, or physical storage corruption.
    pub fn execute_prepared_latest(
        &self,
        prepared: &PreparedStatement,
        parameters: &[SqlValue],
    ) -> Result<SqlResult, SqlError> {
        let metadata = self
            .coordinator
            .snapshot(0)
            .map_err(NativeRuntimeError::from)?;
        sql::execute_prepared_latest(self, &metadata, prepared, parameters)
    }

    /// Performs an owned primary-key lookup through the current relational
    /// B+tree root without materializing the complete relation state.
    ///
    /// # Errors
    ///
    /// Returns an error for page or B+tree corruption.
    pub fn select_latest_relational(
        &self,
        table: ObjectId,
        primary_key: &[u8],
    ) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        self.select_relational_at(&snapshot, table, primary_key)
    }

    fn select_relational_at(
        &self,
        snapshot: &Snapshot,
        table: ObjectId,
        primary_key: &[u8],
    ) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        let Some(root) = snapshot.roots().root(SLOT_RELATIONAL) else {
            return Ok(None);
        };
        let encoded = BTree::from_root(root).get_cached_pinned(
            &self.pages,
            &self.buffer_pool,
            &relational_row_key(table, primary_key),
        )?;
        encoded
            .map(|encoded| {
                let context = RelationalReadContext {
                    pages: &self.pages,
                    pool: &self.buffer_pool,
                    blobs: &self.blobs,
                    format: self.relational_format,
                    visible_csn: snapshot.visible_csn,
                };
                decode_relational_value_cached(&context, table, primary_key, encoded.bytes())
            })
            .transpose()
            .map(Option::flatten)
    }

    /// Scans visible current rows in canonical primary-key order without
    /// materializing the complete relation.
    ///
    /// `start_after` is an exclusive canonical primary-key cursor. A zero
    /// `limit` validates relation identity and returns no rows.
    ///
    /// # Errors
    ///
    /// Returns an error when the relation does not exist or when a reached
    /// physical key, row, page, blob, or version chain is malformed.
    pub fn scan_latest_relational(
        &self,
        table: ObjectId,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<RelationalScanRow>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        self.scan_relational_at(&snapshot, table, start_after, limit)
    }

    /// Scans one bounded current primary-key range without materializing the
    /// complete relation.
    ///
    /// Bounds contain canonical primary-key bytes, not full physical keys.
    /// A zero `limit` validates relation identity and returns no rows.
    ///
    /// # Errors
    ///
    /// Returns an error when the relation does not exist or when a reached
    /// physical key, row, page, blob, or version chain is malformed.
    pub fn scan_latest_relational_range(
        &self,
        table: ObjectId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        limit: usize,
    ) -> Result<Vec<RelationalScanRow>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        self.scan_relational_range_at(&snapshot, table, lower, upper, limit)
    }

    fn scan_relational_at(
        &self,
        snapshot: &Snapshot,
        table: ObjectId,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<RelationalScanRow>, NativeRuntimeError> {
        let lower = start_after.map_or(Bound::Unbounded, Bound::Excluded);
        self.scan_relational_range_at(snapshot, table, lower, Bound::Unbounded, limit)
    }

    fn scan_relational_range_at(
        &self,
        snapshot: &Snapshot,
        table: ObjectId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        limit: usize,
    ) -> Result<Vec<RelationalScanRow>, NativeRuntimeError> {
        let (tree, prefix) = self.relational_table_tree_at(snapshot, table)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let lower = map_primary_key_bound(table, lower);
        let upper = map_primary_key_bound(table, upper);
        let context = RelationalReadContext {
            pages: &self.pages,
            pool: &self.buffer_pool,
            blobs: &self.blobs,
            format: self.relational_format,
            visible_csn: snapshot.visible_csn,
        };
        let mut rows = Vec::with_capacity(limit.min(256));
        let mut scan_error = None;
        let _outcome = tree.visit_prefix_range_cached(
            &self.pages,
            &self.buffer_pool,
            &prefix,
            bound_as_slice(&lower),
            bound_as_slice(&upper),
            |physical_key, encoded| {
                let Some(primary_key) = physical_key.get(prefix.len()..) else {
                    scan_error = Some(NativeRuntimeError::InvalidRelationalTree);
                    return ControlFlow::Break(());
                };
                match decode_relational_value_cached(&context, table, primary_key, encoded) {
                    Ok(Some(row)) => {
                        rows.push(RelationalScanRow {
                            table,
                            primary_key: primary_key.to_vec(),
                            row,
                        });
                        if rows.len() == limit {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    }
                    Ok(None) => ControlFlow::Continue(()),
                    Err(error) => {
                        scan_error = Some(error);
                        ControlFlow::Break(())
                    }
                }
            },
        )?;
        if let Some(error) = scan_error {
            return Err(error);
        }
        Ok(rows)
    }

    fn relational_table_tree_at(
        &self,
        snapshot: &Snapshot,
        table: ObjectId,
    ) -> Result<(BTree, Vec<u8>), NativeRuntimeError> {
        let root = snapshot
            .roots()
            .root(SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::UnknownRelation { table })?;
        let tree = BTree::from_root(root);
        let table_marker = tree
            .get_cached_pinned(&self.pages, &self.buffer_pool, &relational_table_key(table))?
            .ok_or(NativeRuntimeError::UnknownRelation { table })?;
        if !table_marker.bytes().is_empty() {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        let prefix = relational_row_key(table, &[]);
        Ok((tree, prefix))
    }

    pub(crate) fn visit_relational_range_at<E>(
        &self,
        snapshot: &Snapshot,
        table: ObjectId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        mut visitor: impl FnMut(&[u8], &[u8]) -> Result<ControlFlow<()>, E>,
    ) -> Result<(), RelationalVisitError<E>> {
        let (tree, prefix) = self
            .relational_table_tree_at(snapshot, table)
            .map_err(RelationalVisitError::Runtime)?;
        let lower = map_primary_key_bound(table, lower);
        let upper = map_primary_key_bound(table, upper);
        let context = RelationalReadContext {
            pages: &self.pages,
            pool: &self.buffer_pool,
            blobs: &self.blobs,
            format: self.relational_format,
            visible_csn: snapshot.visible_csn,
        };
        let mut failure = None;
        let _outcome = tree
            .visit_prefix_range_cached(
                &self.pages,
                &self.buffer_pool,
                &prefix,
                bound_as_slice(&lower),
                bound_as_slice(&upper),
                |physical_key, encoded| {
                    let Some(primary_key) = physical_key.get(prefix.len()..) else {
                        failure = Some(RelationalVisitError::Runtime(
                            NativeRuntimeError::InvalidRelationalTree,
                        ));
                        return ControlFlow::Break(());
                    };
                    match decode_relational_value_cached(&context, table, primary_key, encoded) {
                        Ok(Some(row)) => match visitor(primary_key, &row) {
                            Ok(control) => control,
                            Err(error) => {
                                failure = Some(RelationalVisitError::Visitor(error));
                                ControlFlow::Break(())
                            }
                        },
                        Ok(None) => ControlFlow::Continue(()),
                        Err(error) => {
                            failure = Some(RelationalVisitError::Runtime(error));
                            ControlFlow::Break(())
                        }
                    }
                },
            )
            .map_err(NativeRuntimeError::from)
            .map_err(RelationalVisitError::Runtime)?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    /// Performs an exact current secondary-index lookup through the relational
    /// B+tree without materializing the complete relation state.
    ///
    /// Results are ordered by canonical primary-key bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the index does not exist or when physical
    /// metadata, entries, rows, pages, or version chains are malformed.
    pub fn select_latest_secondary_index(
        &self,
        index: ObjectId,
        index_key: &[u8],
    ) -> Result<Vec<SecondaryIndexRow>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        self.select_secondary_index_at(&snapshot, index, index_key)
    }

    fn select_secondary_index_at(
        &self,
        snapshot: &Snapshot,
        index: ObjectId,
        index_key: &[u8],
    ) -> Result<Vec<SecondaryIndexRow>, NativeRuntimeError> {
        let Some(root) = snapshot.roots().root(SLOT_RELATIONAL) else {
            return Err(NativeRuntimeError::UnknownSecondaryIndex { index });
        };
        let tree = BTree::from_root(root);
        let table = {
            let metadata = tree
                .get_cached_pinned(
                    &self.pages,
                    &self.buffer_pool,
                    &relational_secondary_index_key(index),
                )?
                .ok_or(NativeRuntimeError::UnknownSecondaryIndex { index })?;
            let (table, _, _) = decode_secondary_index_metadata(metadata.bytes())?;
            table
        };
        let prefix = relational_secondary_entry_prefix(index, index_key)?;
        let entries = tree.scan_prefix_cached(&self.pages, &self.buffer_pool, &prefix)?;
        let context = RelationalReadContext {
            pages: &self.pages,
            pool: &self.buffer_pool,
            blobs: &self.blobs,
            format: self.relational_format,
            visible_csn: snapshot.visible_csn,
        };
        let mut rows = Vec::with_capacity(entries.len());
        for (physical_key, marker) in entries {
            let identity = physical_key
                .get(17..)
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
            let (stored_index_key, primary_key) = decode_secondary_index_entry_identity(identity)?;
            if stored_index_key != index_key {
                return Err(NativeRuntimeError::InvalidRelationalTree);
            }
            match marker.as_slice() {
                [RELATIONAL_SECONDARY_ENTRY_TOMBSTONE] => continue,
                [RELATIONAL_SECONDARY_ENTRY_LIVE] => {}
                _ => return Err(NativeRuntimeError::InvalidRelationalTree),
            }
            let encoded_row = tree
                .get_cached_pinned(
                    &self.pages,
                    &self.buffer_pool,
                    &relational_row_key(table, primary_key),
                )?
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
            let row =
                decode_relational_value_cached(&context, table, primary_key, encoded_row.bytes())?
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
            rows.push(SecondaryIndexRow {
                table,
                primary_key: primary_key.to_vec(),
                row,
            });
        }
        Ok(rows)
    }

    /// Reads one current structure value through its physical root.
    ///
    /// Expiry is evaluated against the supplied deterministic logical time.
    ///
    /// # Errors
    ///
    /// Returns an error for page, B+tree, value-envelope, or blob corruption.
    pub fn get_latest_structure(
        &self,
        key: &[u8],
        logical_time_micros: i64,
    ) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        Ok(self.latest_structure_entry(key)?.and_then(|entry| {
            entry
                .expires_at_micros
                .is_none_or(|expiry| expiry > logical_time_micros)
                .then_some(entry.value)
        }))
    }

    /// Returns current physical TTL state at deterministic logical time.
    ///
    /// # Errors
    ///
    /// Returns an error for page, B+tree, value-envelope, or blob corruption.
    pub fn ttl_latest_structure(
        &self,
        key: &[u8],
        logical_time_micros: i64,
    ) -> Result<Ttl, NativeRuntimeError> {
        Ok(match self.latest_structure_entry(key)? {
            None => Ttl::Missing,
            Some(entry)
                if entry
                    .expires_at_micros
                    .is_some_and(|expiry| expiry <= logical_time_micros) =>
            {
                Ttl::Missing
            }
            Some(entry) => entry.expires_at_micros.map_or(Ttl::Persistent, |expiry| {
                Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
            }),
        })
    }

    fn latest_structure_entry(
        &self,
        key: &[u8],
    ) -> Result<Option<StructureEntry>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        let Some(root) = snapshot.roots().root(SLOT_STRUCTURE) else {
            return Ok(None);
        };
        match self.structure_format {
            StructureFormat::InlineStateV1 => {
                let state = load_structure_state(&self.pages, &self.blobs, snapshot.roots())?;
                Ok(state.entries.get(key).cloned())
            }
            StructureFormat::BTreeV1 => BTree::from_root(root)
                .get_cached_pinned(&self.pages, &self.buffer_pool, &structure_key(key))?
                .map(|encoded| decode_structure_value(encoded.bytes(), &self.blobs))
                .transpose()
                .map(Option::flatten),
        }
    }

    /// Reads one field directly through the current physical hash namespace.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar key, a missing hash, or
    /// any malformed B+tree, metadata, field envelope, or blob.
    pub fn hget_latest_hash(
        &self,
        key: &[u8],
        field: &[u8],
    ) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let root = snapshot
            .roots()
            .root(SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = BTree::from_root(root);
        let Some(metadata) = tree.get_cached_pinned(
            &self.pages,
            &self.buffer_pool,
            &structure_hash_meta_key(key),
        )?
        else {
            if tree
                .get_cached_pinned(&self.pages, &self.buffer_pool, &structure_key(key))?
                .map(|encoded| decode_structure_value(encoded.bytes(), &self.blobs))
                .transpose()?
                .flatten()
                .is_some()
                || tree
                    .get_cached_pinned(
                        &self.pages,
                        &self.buffer_pool,
                        &structure_set_meta_key(key),
                    )?
                    .is_some()
            {
                return Err(NativeRuntimeError::StructureKindMismatch);
            }
            return Err(NativeRuntimeError::UnknownStructureHash);
        };
        decode_hash_metadata(metadata.bytes())?;
        tree.get_cached_pinned(
            &self.pages,
            &self.buffer_pool,
            &structure_hash_field_key(key, field)?,
        )?
        .map(|encoded| decode_hash_field_value(encoded.bytes(), &self.blobs))
        .transpose()
        .map(Option::flatten)
    }

    /// Reads one hash cardinality directly from its physical metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar key, a missing hash, or
    /// malformed B+tree metadata.
    pub fn hlen_latest_hash(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let root = snapshot
            .roots()
            .root(SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = BTree::from_root(root);
        let Some(metadata) = tree.get_cached_pinned(
            &self.pages,
            &self.buffer_pool,
            &structure_hash_meta_key(key),
        )?
        else {
            if tree
                .get_cached_pinned(&self.pages, &self.buffer_pool, &structure_key(key))?
                .map(|encoded| decode_structure_value(encoded.bytes(), &self.blobs))
                .transpose()?
                .flatten()
                .is_some()
                || tree
                    .get_cached_pinned(
                        &self.pages,
                        &self.buffer_pool,
                        &structure_set_meta_key(key),
                    )?
                    .is_some()
            {
                return Err(NativeRuntimeError::StructureKindMismatch);
            }
            return Err(NativeRuntimeError::UnknownStructureHash);
        };
        usize::try_from(decode_hash_metadata(metadata.bytes())?)
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)
    }

    /// Tests membership directly through the current physical set namespace.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar/hash key, a missing set,
    /// or malformed B+tree metadata/member data.
    pub fn sismember_latest_set(
        &self,
        key: &[u8],
        member: &[u8],
    ) -> Result<bool, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let root = snapshot
            .roots()
            .root(SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = BTree::from_root(root);
        let Some(metadata) =
            tree.get_cached_pinned(&self.pages, &self.buffer_pool, &structure_set_meta_key(key))?
        else {
            let scalar_exists = tree
                .get_cached_pinned(&self.pages, &self.buffer_pool, &structure_key(key))?
                .map(|encoded| decode_structure_value(encoded.bytes(), &self.blobs))
                .transpose()?
                .flatten()
                .is_some();
            let hash_exists = tree
                .get_cached_pinned(
                    &self.pages,
                    &self.buffer_pool,
                    &structure_hash_meta_key(key),
                )?
                .is_some();
            if scalar_exists || hash_exists {
                return Err(NativeRuntimeError::StructureKindMismatch);
            }
            return Err(NativeRuntimeError::UnknownStructureSet);
        };
        decode_set_metadata(metadata.bytes())?;
        tree.get_cached_pinned(
            &self.pages,
            &self.buffer_pool,
            &structure_set_member_key(key, member)?,
        )?
        .map(|encoded| decode_set_member_value(encoded.bytes()))
        .transpose()
        .map(Option::unwrap_or_default)
    }

    /// Reads one set cardinality directly from its physical metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar/hash key, a missing set,
    /// or malformed B+tree metadata.
    pub fn scard_latest_set(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let root = snapshot
            .roots()
            .root(SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = BTree::from_root(root);
        let Some(metadata) =
            tree.get_cached_pinned(&self.pages, &self.buffer_pool, &structure_set_meta_key(key))?
        else {
            let scalar_exists = tree
                .get_cached_pinned(&self.pages, &self.buffer_pool, &structure_key(key))?
                .map(|encoded| decode_structure_value(encoded.bytes(), &self.blobs))
                .transpose()?
                .flatten()
                .is_some();
            let hash_exists = tree
                .get_cached_pinned(
                    &self.pages,
                    &self.buffer_pool,
                    &structure_hash_meta_key(key),
                )?
                .is_some();
            if scalar_exists || hash_exists {
                return Err(NativeRuntimeError::StructureKindMismatch);
            }
            return Err(NativeRuntimeError::UnknownStructureSet);
        };
        usize::try_from(decode_set_metadata(metadata.bytes())?)
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)
    }

    /// Executes BM25 matching through the current physical inverted index.
    ///
    /// Native B+tree directories resolve only the query terms' posting
    /// ranges. Legacy inline directories retain their historical materialized
    /// fallback.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown collection, oversized query term, or
    /// malformed B+tree metadata, posting, document envelope, page, or blob.
    pub fn match_latest_text(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MatchHit>, NativeRuntimeError> {
        if self.search_format == SearchFormat::InlineStateV1 {
            return self.snapshot(0)?.match_text(index, query, limit);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let root = snapshot
            .roots()
            .root(SLOT_SEARCH)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = BTree::from_root(root);
        let metadata = tree
            .get_cached_pinned(
                &self.pages,
                &self.buffer_pool,
                &search_index_meta_key(index),
            )?
            .ok_or(ModelError::UnknownObject)?;
        let (document_count, total_document_terms) =
            decode_search_index_metadata(metadata.bytes())?;
        let query_tokens: BTreeSet<String> = analyze(query).into_iter().collect();
        if query_tokens.is_empty() || limit == 0 || document_count == 0 {
            return Ok(Vec::new());
        }
        let document_count_f64 = search_count_f64(document_count)?;
        let average_length = search_count_f64(total_document_terms)? / document_count_f64;
        let mut scores = BTreeMap::<Vec<u8>, f64>::new();
        let mut document_lengths = BTreeMap::<Vec<u8>, u64>::new();
        for term in query_tokens {
            let term_key = search_term_meta_key(index, term.as_bytes())?;
            let Some(term_metadata) =
                tree.get_cached_pinned(&self.pages, &self.buffer_pool, &term_key)?
            else {
                continue;
            };
            let document_frequency = decode_search_term_metadata(term_metadata.bytes())?;
            if document_frequency == 0 || document_frequency > document_count {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let idf = bm25_idf(document_count_f64, search_count_f64(document_frequency)?);
            let posting_prefix = search_posting_prefix(index, term.as_bytes())?;
            let postings =
                tree.scan_prefix_cached(&self.pages, &self.buffer_pool, &posting_prefix)?;
            if u64::try_from(postings.len()).map_err(|_| NativeRuntimeError::InvalidSearchTree)?
                != document_frequency
            {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            for (key, encoded_frequency) in postings {
                let document_id = key
                    .strip_prefix(posting_prefix.as_slice())
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?
                    .to_vec();
                let term_frequency = decode_search_posting(&encoded_frequency)?;
                let document_length = if let Some(length) = document_lengths.get(&document_id) {
                    *length
                } else {
                    let document = tree
                        .get_cached_pinned(
                            &self.pages,
                            &self.buffer_pool,
                            &search_document_key(index, &document_id)?,
                        )?
                        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                    let (length, _, _) = decode_search_document_header(document.bytes())?;
                    document_lengths.insert(document_id.clone(), length);
                    length
                };
                *scores.entry(document_id).or_default() += bm25_term_score(
                    idf,
                    f64::from(term_frequency),
                    search_count_f64(document_length)?,
                    average_length,
                );
            }
        }
        let mut hits = scores
            .into_iter()
            .filter(|(_, score)| *score > 0.0)
            .map(|(document_id, score)| MatchHit { document_id, score })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.document_id.cmp(&right.document_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Executes approximate vector search against the current committed
    /// all-engine root set.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index, invalid query, malformed
    /// catalog/root state, or query breadth outside the catalog definition.
    pub fn search_ann_latest(
        &self,
        index: ObjectId,
        query: &Vector,
        options: AnnSearchOptions,
    ) -> Result<AnnSearchReceipt, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        let state = load_state(&self.pages, &self.blobs, snapshot.roots())?;
        let result = state.ann.search(index, query, options)?;
        Ok(ann_search_receipt(index, snapshot.visible_csn, result))
    }

    /// Executes the complete exact vector-ranking oracle against the current
    /// committed all-engine root set.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index, invalid query, or malformed
    /// catalog/root state.
    pub fn search_vector_exact_latest(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        load_state(&self.pages, &self.blobs, snapshot.roots())?
            .ann
            .search_exact(index, query, k)
    }

    /// Verifies the current relational B+tree and returns its node height.
    ///
    /// # Errors
    ///
    /// Returns an error for snapshot coordination, a missing committed root,
    /// or any reachable B+tree/page corruption.
    pub fn latest_relational_tree_height(&self) -> Result<usize, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        let Some(root) = snapshot.roots().root(SLOT_RELATIONAL) else {
            return Ok(0);
        };
        Ok(BTree::from_root(root).height(&self.pages)?)
    }

    /// Verifies the current structure B+tree and returns its node height.
    ///
    /// Legacy inline-state roots report zero because they are not a B+tree.
    ///
    /// # Errors
    ///
    /// Returns an error for snapshot, page, or B+tree corruption.
    pub fn latest_structure_tree_height(&self) -> Result<usize, NativeRuntimeError> {
        if self.structure_format == StructureFormat::InlineStateV1 {
            return Ok(0);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let Some(root) = snapshot.roots().root(SLOT_STRUCTURE) else {
            return Ok(0);
        };
        Ok(BTree::from_root(root).height(&self.pages)?)
    }

    /// Verifies the current inverted-index B+tree and returns its node height.
    ///
    /// Legacy inline search roots report zero.
    ///
    /// # Errors
    ///
    /// Returns an error for snapshot, page, or B+tree corruption.
    pub fn latest_search_tree_height(&self) -> Result<usize, NativeRuntimeError> {
        if self.search_format == SearchFormat::InlineStateV1 {
            return Ok(0);
        }
        let snapshot = self.coordinator.snapshot(0)?;
        let Some(root) = snapshot.roots().root(SLOT_SEARCH) else {
            return Ok(0);
        };
        Ok(BTree::from_root(root).height(&self.pages)?)
    }

    /// Prepares one detached optimistic write transaction.
    ///
    /// Preparation captures and materializes an immutable snapshot without
    /// acquiring the serialized writer guard. Multiple callers may therefore
    /// prepare private write sets concurrently. Publication remains a short,
    /// explicitly serialized operation through [`Self::commit_optimistic`].
    ///
    /// # Errors
    ///
    /// Returns an error for snapshot, page, root, blob, or state corruption.
    pub fn begin_optimistic(
        &self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeWriteBatch, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(logical_time_micros)?;
        let state = load_state(&self.pages, &self.blobs, snapshot.roots())?;
        Ok(NativeWriteBatch {
            snapshot,
            state,
            mutations: Vec::new(),
            dirty: [false; 4],
            durability,
            structure_format: self.structure_format,
            search_format: self.search_format,
        })
    }

    /// Begins one serialized native write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid state, exhausted IDs, or poisoned
    /// synchronization.
    pub fn begin(
        &mut self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeTransaction<'_>, NativeRuntimeError> {
        let batch = self.begin_optimistic(logical_time_micros, durability)?;
        let transaction_id = TransactionId::new(self.next_transaction_id)
            .map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let root_transaction = self.coordinator.begin_write()?;
        Ok(NativeTransaction {
            pages: &mut self.pages,
            blobs: &mut self.blobs,
            wal: &mut self.wal,
            conflicts: &mut self.conflicts,
            relational_format: self.relational_format,
            structure_format: self.structure_format,
            search_format: self.search_format,
            root_transaction,
            conflict_read_csn: batch.snapshot.visible_csn,
            transaction_id,
            next_transaction_id: &mut self.next_transaction_id,
            batch,
        })
    }

    /// Begins the transaction used for native SQL `BEGIN`.
    ///
    /// # Errors
    ///
    /// Returns the same state and synchronization errors as [`Self::begin`].
    pub fn begin_sql(
        &mut self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeTransaction<'_>, NativeRuntimeError> {
        self.begin(logical_time_micros, durability)
    }

    /// Validates, rebases, persists, and publishes one detached write batch.
    ///
    /// First-committer-wins validation uses the batch's original read CSN.
    /// Disjoint writes are reapplied to the root set current at writer
    /// admission so a stale prepared batch cannot replace intervening state.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty batch, a write conflict, invalid rebased
    /// semantics, persistence, synchronization, codec, or MVCC publication.
    pub fn commit_optimistic(
        &mut self,
        batch: NativeWriteBatch,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        self.commit_optimistic_at(batch, None)
    }

    /// Publishes a detached batch with one deterministic crash interruption.
    ///
    /// After an injected interruption the caller must drop the database handle
    /// and reopen the data directory.
    ///
    /// # Errors
    ///
    /// Returns [`NativeRuntimeError::InjectedCrash`] at the requested boundary,
    /// or the same errors as [`Self::commit_optimistic`].
    pub fn commit_optimistic_with_interruption(
        &mut self,
        batch: NativeWriteBatch,
        boundary: CommitBoundary,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        self.commit_optimistic_at(batch, Some(boundary))
    }

    fn commit_optimistic_at(
        &mut self,
        mut batch: NativeWriteBatch,
        interruption: Option<CommitBoundary>,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        if batch.mutations.is_empty() {
            return Err(WalSemanticError::InvalidSequence.into());
        }
        let conflict_read_csn = batch.snapshot.visible_csn;
        let logical_time_micros = batch.snapshot.logical_time_micros;
        let root_transaction = self.coordinator.begin_write()?;
        let write_keys = mutation_write_keys(&batch.mutations);
        self.conflicts.validate(conflict_read_csn, &write_keys)?;

        let mut state = load_state(&self.pages, &self.blobs, root_transaction.base_roots())?;
        apply_mutations_to_state(&mut state, &batch.mutations)?;
        batch.snapshot = root_transaction.base_snapshot(logical_time_micros);
        batch.state = state;
        let transaction_id = TransactionId::new(self.next_transaction_id)
            .map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        NativeTransaction {
            pages: &mut self.pages,
            blobs: &mut self.blobs,
            wal: &mut self.wal,
            conflicts: &mut self.conflicts,
            relational_format: self.relational_format,
            structure_format: self.structure_format,
            search_format: self.search_format,
            root_transaction,
            conflict_read_csn,
            transaction_id,
            next_transaction_id: &mut self.next_transaction_id,
            batch,
        }
        .commit_at(interruption)
    }

    /// Publishes and WAL-anchors one synchronized immutable root checkpoint.
    ///
    /// The checkpoint does not advance the visible CSN. It records the exact
    /// all-engine root set already committed at that CSN.
    ///
    /// # Errors
    ///
    /// Returns an error before the first commit, on identity exhaustion, or
    /// for manifest/WAL publication and synchronization failures.
    pub fn checkpoint(&mut self) -> Result<CheckpointReceipt, NativeRuntimeError> {
        self.checkpoint_at(None)
    }

    /// Checkpoints with one deterministic interruption for crash-matrix tests.
    ///
    /// After an injected interruption the caller must drop the database handle
    /// and reopen the data directory.
    ///
    /// # Errors
    ///
    /// Returns [`NativeRuntimeError::InjectedCheckpointCrash`] at the requested
    /// boundary, or another manifest/WAL failure.
    pub fn checkpoint_with_interruption(
        &mut self,
        boundary: CheckpointBoundary,
    ) -> Result<CheckpointReceipt, NativeRuntimeError> {
        self.checkpoint_at(Some(boundary))
    }

    fn checkpoint_at(
        &mut self,
        interruption: Option<CheckpointBoundary>,
    ) -> Result<CheckpointReceipt, NativeRuntimeError> {
        let snapshot = self.coordinator.snapshot(0)?;
        let visible_csn = snapshot
            .visible_csn
            .ok_or(NativeRuntimeError::NoCommittedState)?;
        let (next_generation, previous_digest) = if let Some(current) = self.manifests.current() {
            (
                current
                    .generation()
                    .get()
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidCheckpoint)?,
                current.digest(),
            )
        } else {
            (1, [0; 32])
        };
        let generation = ManifestGeneration::new(next_generation)
            .map_err(|_| NativeRuntimeError::InvalidCheckpoint)?;
        let manifest = RootManifest::from_root_set(generation, previous_digest, snapshot.roots())?;
        let staged = self.manifests.stage(manifest, true)?;
        interrupt_checkpoint(interruption, CheckpointBoundary::ManifestStaged)?;
        let manifest = self.manifests.publish(staged, true)?;
        interrupt_checkpoint(interruption, CheckpointBoundary::ManifestPublished)?;

        let transaction_id = TransactionId::new(self.next_transaction_id)
            .map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let checkpoint = encode_checkpoint(
            transaction_id,
            visible_csn,
            manifest.generation(),
            manifest.digest(),
            self.last_checkpoint_lsn,
        )?;
        let receipts = self.wal.append_records(vec![checkpoint], false)?;
        let block = receipts.last().ok_or(WalError::EmptyBlock)?;
        interrupt_checkpoint(interruption, CheckpointBoundary::WalAppended)?;
        self.wal.sync_data()?;
        interrupt_checkpoint(interruption, CheckpointBoundary::WalSynchronized)?;

        self.next_transaction_id = transaction_id
            .get()
            .checked_add(1)
            .ok_or(NativeRuntimeError::TransactionIdExhausted)?;
        self.last_checkpoint_lsn = Some(block.last_lsn);
        Ok(CheckpointReceipt {
            transaction_id,
            visible_csn,
            manifest_generation: manifest.generation(),
            manifest_digest: manifest.digest(),
            checkpoint_lsn: block.last_lsn,
            parent_directory_sync_supported: hyphae_native_manifest::parent_sync_supported(),
        })
    }
}

/// Detached private write set over one immutable all-engine snapshot.
///
/// A batch owns no file handle and holds no writer guard. It may be prepared
/// concurrently and later submitted to [`NativeDatabase::commit_optimistic`].
#[derive(Debug)]
pub struct NativeWriteBatch {
    snapshot: Snapshot,
    state: MaterializedState,
    mutations: Vec<Mutation>,
    dirty: [bool; 4],
    durability: DurabilityClass,
    structure_format: StructureFormat,
    search_format: SearchFormat,
}

/// Legacy serialized transaction retaining writer admission for its lifetime.
///
/// This compatibility path delegates its public mutation surface to
/// [`NativeWriteBatch`]. New concurrency-sensitive callers should prepare a
/// detached batch and submit it explicitly.
#[derive(Debug)]
pub struct NativeTransaction<'database> {
    pages: &'database mut PageStore,
    blobs: &'database mut BlobStore,
    wal: &'database mut WalFile,
    conflicts: &'database mut ConflictTable,
    relational_format: RelationalFormat,
    structure_format: StructureFormat,
    search_format: SearchFormat,
    root_transaction: RootTransaction<'database>,
    conflict_read_csn: Option<Csn>,
    transaction_id: TransactionId,
    next_transaction_id: &'database mut u128,
    batch: NativeWriteBatch,
}

impl Deref for NativeTransaction<'_> {
    type Target = NativeWriteBatch;

    fn deref(&self) -> &Self::Target {
        &self.batch
    }
}

impl DerefMut for NativeTransaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.batch
    }
}

impl NativeWriteBatch {
    /// Executes one statement in the current native SQL slice.
    ///
    /// The supported grammar includes typed `CREATE TABLE`, `CREATE INDEX`
    /// with optional `UNIQUE`, named-column `INSERT`, exact-primary-key typed
    /// `UPDATE`/`DELETE`, exact primary/secondary-key `SELECT`, and bounded
    /// `EXPLAIN SELECT`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported syntax, binding, parameters, or
    /// engine semantics.
    pub fn execute_sql(
        &mut self,
        statement: &str,
        parameters: &[SqlValue],
    ) -> Result<SqlResult, SqlError> {
        sql::execute_transaction(self, statement, parameters)
    }

    /// Creates one fixed-schema binary relation with a binary primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid names or duplicate catalog identity/name.
    pub fn create_relation(&mut self, id: ObjectId, name: &str) -> Result<(), NativeRuntimeError> {
        self.create_relation_definition(binary_relation_definition(id, name)?)
    }

    fn create_relation_definition(
        &mut self,
        definition: RelationDefinition,
    ) -> Result<(), NativeRuntimeError> {
        let id = definition.header.id;
        let object = CatalogObject::Relation(definition);
        let encoded_definition = object.encode_definition()?;
        let name_identity = catalog_name_identity(object.header())?;
        self.state.catalog.create(object)?;
        self.state.relational.create_table(id)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::CreateTable,
            target: Some(id),
            key: name_identity,
            value: encoded_definition,
            expires_at_micros: None,
        });
        self.dirty[0] = true;
        self.dirty[1] = true;
        Ok(())
    }

    fn create_secondary_index_definition(
        &mut self,
        definition: &SecondaryIndexDefinition,
    ) -> Result<(), NativeRuntimeError> {
        let id = definition.header.id;
        let relation = definition.relation;
        let object = CatalogObject::SecondaryIndex(definition.clone());
        let encoded_definition = object.encode_definition()?;
        let name_identity = catalog_name_identity(object.header())?;
        let mut catalog = self.state.catalog.clone();
        let mut relational = self.state.relational.clone();
        catalog.create(object)?;
        relational.create_secondary_index(
            id,
            relation,
            definition.unique,
            definition.nulls_distinct,
        )?;
        let rows = relational
            .tables
            .get(&relation)
            .ok_or(NativeRuntimeError::InvalidPreparedMutation)?
            .iter()
            .map(|(primary_key, row)| (primary_key.clone(), row.clone()))
            .collect::<Vec<_>>();
        for (primary_key, row) in rows {
            let projection = secondary_index_projection(&catalog, definition, &row)?;
            relational.insert_secondary_index(
                id,
                projection.key.clone(),
                primary_key.clone(),
                projection.contains_null,
            )?;
        }
        self.state.catalog = catalog;
        self.state.relational = relational;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::CreateSecondaryIndex,
            target: Some(id),
            key: name_identity,
            value: encoded_definition,
            expires_at_micros: None,
        });
        self.dirty[0] = true;
        self.dirty[1] = true;
        Ok(())
    }

    /// Inserts one binary row addressed by its primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or duplicate primary key.
    pub fn insert(
        &mut self,
        table: ObjectId,
        primary_key: impl Into<Vec<u8>>,
        row: impl Into<Vec<u8>>,
    ) -> Result<(), NativeRuntimeError> {
        self.state.catalog.require(table, EngineKind::Relational)?;
        let primary_key = primary_key.into();
        let row = row.into();
        let projections = secondary_index_projections(&self.state.catalog, table, &row)?;
        let mut relational = self.state.relational.clone();
        relational.insert(table, primary_key.clone(), row.clone())?;
        for projection in &projections {
            relational.insert_secondary_index(
                projection.index,
                projection.key.clone(),
                primary_key.clone(),
                projection.contains_null,
            )?;
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::InsertRow,
            target: Some(table),
            key: primary_key.clone(),
            value: row,
            expires_at_micros: None,
        });
        self.state.relational = relational;
        self.dirty[1] = true;
        Ok(())
    }

    /// Replaces one binary row addressed by its primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or missing primary key.
    pub fn update(
        &mut self,
        table: ObjectId,
        primary_key: impl Into<Vec<u8>>,
        row: impl Into<Vec<u8>>,
    ) -> Result<(), NativeRuntimeError> {
        self.state.catalog.require(table, EngineKind::Relational)?;
        let primary_key = primary_key.into();
        let row = row.into();
        let old_row = self
            .state
            .relational
            .select(table, &primary_key)
            .ok_or(ModelError::MissingPrimaryKey)?
            .to_vec();
        let old_projections = secondary_index_projections(&self.state.catalog, table, &old_row)?;
        let new_projections = secondary_index_projections(&self.state.catalog, table, &row)?;
        let mut relational = self.state.relational.clone();
        remove_secondary_index_projections(&mut relational, &old_projections, &primary_key)?;
        relational.update(table, &primary_key, row.clone())?;
        insert_secondary_index_projections(&mut relational, &new_projections, &primary_key)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::UpdateRow,
            target: Some(table),
            key: primary_key.clone(),
            value: row,
            expires_at_micros: None,
        });
        self.state.relational = relational;
        self.dirty[1] = true;
        Ok(())
    }

    /// Deletes one binary row by publishing a canonical tombstone.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown table or missing primary key.
    pub fn delete(
        &mut self,
        table: ObjectId,
        primary_key: impl Into<Vec<u8>>,
    ) -> Result<(), NativeRuntimeError> {
        self.state.catalog.require(table, EngineKind::Relational)?;
        let primary_key = primary_key.into();
        let old_row = self
            .state
            .relational
            .select(table, &primary_key)
            .ok_or(ModelError::MissingPrimaryKey)?
            .to_vec();
        let projections = secondary_index_projections(&self.state.catalog, table, &old_row)?;
        let mut relational = self.state.relational.clone();
        remove_secondary_index_projections(&mut relational, &projections, &primary_key)?;
        relational.delete(table, &primary_key)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::DeleteRow,
            target: Some(table),
            key: primary_key.clone(),
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.state.relational = relational;
        self.dirty[1] = true;
        Ok(())
    }

    /// Reads a relation from the snapshot plus this transaction's writes.
    pub fn select(&self, table: ObjectId, primary_key: &[u8]) -> Option<&[u8]> {
        self.state.relational.select(table, primary_key)
    }

    /// Sets one native structure value and optional absolute expiry.
    ///
    /// # Errors
    ///
    /// Returns an error when the key belongs to a collection.
    pub fn set(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
        expires_at_micros: Option<i64>,
    ) -> Result<(), NativeRuntimeError> {
        let outcome = self.set_conditional(key, value, expires_at_micros, SetCondition::Always)?;
        debug_assert_eq!(outcome, SetOutcome::Applied);
        Ok(())
    }

    /// Sets one scalar value only when its snapshot-time predicate is true.
    ///
    /// A rejected condition adds no write key and therefore needs no commit.
    /// Concurrent `IfAbsent` writers over the same missing key may both prepare,
    /// but first-committer-wins admits only one publication.
    ///
    /// # Errors
    ///
    /// Returns an error when the key belongs to a collection.
    pub fn set_conditional(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
        expires_at_micros: Option<i64>,
        condition: SetCondition,
    ) -> Result<SetOutcome, NativeRuntimeError> {
        let key = key.into();
        let value = value.into();
        if self.state.structures.hashes.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let exists = self
            .state
            .structures
            .visible_entry(&key, self.snapshot.logical_time_micros)
            .is_some();
        let applies = match condition {
            SetCondition::Always => true,
            SetCondition::IfAbsent => !exists,
            SetCondition::IfPresent => exists,
        };
        if !applies {
            return Ok(SetOutcome::NotApplied);
        }
        self.state
            .structures
            .set(key.clone(), value.clone(), expires_at_micros);
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::SetValue,
            target: None,
            key,
            value,
            expires_at_micros,
        });
        self.dirty[2] = true;
        Ok(SetOutcome::Applied)
    }

    /// Deletes one unexpired scalar value from this transaction.
    ///
    /// Returns `false` without adding a mutation when the key is missing or
    /// expired at the transaction's deterministic logical time.
    ///
    /// # Errors
    ///
    /// Returns an error when the key belongs to a collection.
    pub fn delete_structure(
        &mut self,
        key: impl Into<Vec<u8>>,
    ) -> Result<bool, NativeRuntimeError> {
        let key = key.into();
        if self.state.structures.hashes.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        if self
            .state
            .structures
            .visible_entry(&key, self.snapshot.logical_time_micros)
            .is_none()
        {
            return Ok(false);
        }
        let removed = self.state.structures.delete(&key);
        debug_assert!(removed.is_some());
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::DeleteValue,
            target: None,
            key,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(true)
    }

    /// Replaces one visible scalar value's absolute expiry.
    ///
    /// The value bytes are retained in the WAL mutation so recovery can verify
    /// the exact new physical envelope without consulting an older root.
    ///
    /// # Errors
    ///
    /// Returns an error when the key belongs to a collection.
    pub fn expire_structure(
        &mut self,
        key: impl Into<Vec<u8>>,
        expires_at_micros: i64,
    ) -> Result<bool, NativeRuntimeError> {
        let key = key.into();
        if self.state.structures.hashes.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let Some(value) = self
            .state
            .structures
            .visible_entry(&key, self.snapshot.logical_time_micros)
            .map(|entry| entry.value.clone())
        else {
            return Ok(false);
        };
        self.state
            .structures
            .set(key.clone(), value.clone(), Some(expires_at_micros));
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::ExpireValue,
            target: None,
            key,
            value,
            expires_at_micros: Some(expires_at_micros),
        });
        self.dirty[2] = true;
        Ok(true)
    }

    /// Atomically adds `delta` to one canonical signed-decimal scalar.
    ///
    /// Missing or expired keys start at zero. Existing expiry is preserved.
    /// Noncanonical decimal bytes and signed overflow fail without adding a
    /// mutation.
    ///
    /// # Errors
    ///
    /// Returns [`NativeRuntimeError::StructureValueNotInteger`] for a
    /// noncanonical integer and
    /// [`NativeRuntimeError::StructureIntegerOverflow`] on overflow.
    pub fn increment_i64(
        &mut self,
        key: impl Into<Vec<u8>>,
        delta: i64,
    ) -> Result<i64, NativeRuntimeError> {
        let key = key.into();
        if self.state.structures.hashes.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let existing = self
            .state
            .structures
            .visible_entry(&key, self.snapshot.logical_time_micros)
            .cloned();
        let (base, expires_at_micros) = match existing {
            None => (0, None),
            Some(entry) => (parse_canonical_i64(&entry.value)?, entry.expires_at_micros),
        };
        let value = base
            .checked_add(delta)
            .ok_or(NativeRuntimeError::StructureIntegerOverflow)?;
        let outcome = self.set_conditional(
            key,
            value.to_string().into_bytes(),
            expires_at_micros,
            SetCondition::Always,
        )?;
        debug_assert_eq!(outcome, SetOutcome::Applied);
        Ok(value)
    }

    /// Reads a structure value from the snapshot plus private writes.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.state
            .structures
            .get(key, self.snapshot.logical_time_micros)
    }

    /// Creates one explicitly typed empty native hash.
    ///
    /// Hashes are not encoded in legacy whole-state structure roots.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage or an existing scalar/hash key.
    pub fn create_hash(&mut self, key: impl Into<Vec<u8>>) -> Result<(), NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if !self.state.structures.create_hash(key.clone()) {
            return Err(NativeRuntimeError::StructureKeyExists);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::CreateHash,
            target: None,
            key,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(())
    }

    /// Inserts or replaces one field in an existing native hash.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar key, or a missing hash.
    pub fn hset(
        &mut self,
        key: impl Into<Vec<u8>>,
        field: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> Result<HashSetOutcome, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if self.state.structures.entries.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let field = field.into();
        let value = value.into();
        let added = self
            .state
            .structures
            .hset(&key, field.clone(), value.clone())
            .ok_or(NativeRuntimeError::UnknownStructureHash)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::SetHashField,
            target: None,
            key: hash_field_identity(&key, &field)?,
            value,
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(if added {
            HashSetOutcome::Added
        } else {
            HashSetOutcome::Updated
        })
    }

    /// Reads one field from an existing native hash.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar key or a missing hash.
    pub fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.sets.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        if !self.state.structures.hashes.contains_key(key) {
            return Err(NativeRuntimeError::UnknownStructureHash);
        }
        Ok(self.state.structures.hget(key, field))
    }

    /// Deletes one field without deleting the typed hash itself.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar key, or a missing hash.
    pub fn hdelete(
        &mut self,
        key: impl Into<Vec<u8>>,
        field: impl Into<Vec<u8>>,
    ) -> Result<bool, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if self.state.structures.entries.contains_key(&key)
            || self.state.structures.sets.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let field = field.into();
        let deleted = self
            .state
            .structures
            .hdelete(&key, &field)
            .ok_or(NativeRuntimeError::UnknownStructureHash)?;
        if !deleted {
            return Ok(false);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::DeleteHashField,
            target: None,
            key: hash_field_identity(&key, &field)?,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(true)
    }

    /// Returns the current private field count for an existing native hash.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar key or a missing hash.
    pub fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.sets.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .hlen(key)
            .ok_or(NativeRuntimeError::UnknownStructureHash)
    }

    /// Creates one explicitly typed empty native set.
    ///
    /// Sets are not encoded in legacy whole-state structure roots.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage or an existing structure key.
    pub fn create_set(&mut self, key: impl Into<Vec<u8>>) -> Result<(), NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if !self.state.structures.create_set(key.clone()) {
            return Err(NativeRuntimeError::StructureKeyExists);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::CreateSet,
            target: None,
            key,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(())
    }

    /// Adds one exact binary member to an existing native set.
    ///
    /// Returns `true` only when the member was absent.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar/hash key, or a missing
    /// set.
    pub fn sadd(
        &mut self,
        key: impl Into<Vec<u8>>,
        member: impl Into<Vec<u8>>,
    ) -> Result<bool, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if self.state.structures.entries.contains_key(&key)
            || self.state.structures.hashes.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let member = member.into();
        let added = self
            .state
            .structures
            .sadd(&key, member.clone())
            .ok_or(NativeRuntimeError::UnknownStructureSet)?;
        if !added {
            return Ok(false);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::AddSetMember,
            target: None,
            key: set_member_identity(&key, &member)?,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(true)
    }

    /// Tests exact binary membership in an existing native set.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar/hash key or a missing set.
    pub fn sismember(&self, key: &[u8], member: &[u8]) -> Result<bool, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.hashes.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .sismember(key, member)
            .ok_or(NativeRuntimeError::UnknownStructureSet)
    }

    /// Removes one exact binary member without deleting the typed set.
    ///
    /// Returns `true` only when the member existed.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, a scalar/hash key, or a missing
    /// set.
    pub fn srem(
        &mut self,
        key: impl Into<Vec<u8>>,
        member: impl Into<Vec<u8>>,
    ) -> Result<bool, NativeRuntimeError> {
        if self.structure_format != StructureFormat::BTreeV1 {
            return Err(NativeRuntimeError::LegacyStructureFamilyUnsupported);
        }
        let key = key.into();
        if self.state.structures.entries.contains_key(&key)
            || self.state.structures.hashes.contains_key(&key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        let member = member.into();
        let deleted = self
            .state
            .structures
            .srem(&key, &member)
            .ok_or(NativeRuntimeError::UnknownStructureSet)?;
        if !deleted {
            return Ok(false);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::DeleteSetMember,
            target: None,
            key: set_member_identity(&key, &member)?,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[2] = true;
        Ok(true)
    }

    /// Returns the current private exact cardinality of one native set.
    ///
    /// # Errors
    ///
    /// Returns an error for a scalar/hash key or a missing set.
    pub fn scard(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        if self.state.structures.entries.contains_key(key)
            || self.state.structures.hashes.contains_key(key)
        {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        self.state
            .structures
            .scard(key)
            .ok_or(NativeRuntimeError::UnknownStructureSet)
    }

    /// Creates one native text search collection.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid names or duplicate catalog identity/name.
    pub fn create_search_index(
        &mut self,
        id: ObjectId,
        name: &str,
    ) -> Result<(), NativeRuntimeError> {
        let object = CatalogObject::Search(text_search_definition(id, name)?);
        let encoded_definition = object.encode_definition()?;
        let name_identity = catalog_name_identity(object.header())?;
        self.state.catalog.create(object)?;
        self.state.search.create_index(id)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::CreateIndex,
            target: Some(id),
            key: name_identity,
            value: encoded_definition,
            expires_at_micros: None,
        });
        self.dirty[0] = true;
        self.dirty[3] = true;
        Ok(())
    }

    /// Inserts one immutable text document.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown collection, invalid ownership, invalid
    /// UTF-8 state, or duplicate document ID.
    pub fn index_document(
        &mut self,
        index: ObjectId,
        document_id: impl Into<Vec<u8>>,
        text: impl Into<String>,
    ) -> Result<(), NativeRuntimeError> {
        self.state.catalog.require(index, EngineKind::Search)?;
        let document_id = document_id.into();
        let text = text.into();
        validate_search_document_identity(&document_id, &text)?;
        self.state
            .search
            .index_document(index, document_id.clone(), text.clone())?;
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::IndexDocument,
            target: Some(index),
            key: document_id,
            value: text.into_bytes(),
            expires_at_micros: None,
        });
        self.dirty[3] = true;
        Ok(())
    }

    /// Matches text against the snapshot plus private writes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown collection.
    pub fn match_text(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MatchHit>, NativeRuntimeError> {
        Ok(self
            .state
            .search
            .search(index, query, limit)?
            .into_iter()
            .map(|(document_id, score)| MatchHit { document_id, score })
            .collect())
    }

    /// Creates one catalog-bound native HNSW vector index.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid catalog identity/name, vector dimension,
    /// HNSW bounds, or a duplicate index.
    pub fn create_vector_index(
        &mut self,
        id: ObjectId,
        name: &str,
        dimension: u16,
        metric: VectorMetric,
        config: HnswConfig,
    ) -> Result<(), NativeRuntimeError> {
        if self.search_format == SearchFormat::InlineStateV1 {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let definition = vector_search_definition(id, name, dimension, metric, config)?;
        let native_definition = ann_store::definition_from_search(&definition)?;
        let object = CatalogObject::Search(definition);
        let encoded_definition = object.encode_definition()?;
        let name_identity = catalog_name_identity(object.header())?;
        self.state.catalog.create(object)?;
        self.state.ann.create(native_definition)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::CreateAnnIndex,
            target: Some(id),
            key: name_identity,
            value: encoded_definition,
            expires_at_micros: None,
        });
        self.dirty[0] = true;
        self.dirty[3] = true;
        Ok(())
    }

    /// Inserts or replaces one vector in the transaction-private ANN
    /// generation.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown index, invalid vector, dimension
    /// mismatch, or metric-specific admission failure.
    pub fn upsert_vector(
        &mut self,
        index: ObjectId,
        object_id: ObjectId,
        vector: Vector,
    ) -> Result<(), NativeRuntimeError> {
        let encoded = ann_store::encode_vector_mutation(&vector);
        self.state
            .ann
            .upsert(index, object_id, ann_store::private_mutation_csn()?, vector)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::UpsertVector,
            target: Some(index),
            key: ann_store::encode_object_identity(object_id),
            value: encoded,
            expires_at_micros: None,
        });
        self.dirty[3] = true;
        Ok(())
    }

    /// Inserts or replaces a duplicate-free vector batch through one
    /// canonical graph rebuild.
    ///
    /// An empty batch is a no-op. The complete batch is admitted before the
    /// transaction-private generation or WAL mutation set changes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown index, duplicate object ID, invalid
    /// vector, dimension mismatch, or metric-specific admission failure.
    pub fn upsert_vectors(
        &mut self,
        index: ObjectId,
        vectors: impl IntoIterator<Item = (ObjectId, Vector)>,
    ) -> Result<usize, NativeRuntimeError> {
        let vectors = vectors.into_iter().collect::<Vec<_>>();
        if vectors.is_empty() {
            return Ok(0);
        }
        let mut identities = BTreeSet::new();
        if vectors
            .iter()
            .any(|(object_id, _)| !identities.insert(*object_id))
        {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        self.state
            .ann
            .upsert_many(index, ann_store::private_mutation_csn()?, &vectors)?;
        let count = vectors.len();
        self.mutations
            .extend(vectors.into_iter().map(|(object_id, vector)| Mutation {
                engine: EngineKind::Search,
                opcode: Opcode::UpsertVector,
                target: Some(index),
                key: ann_store::encode_object_identity(object_id),
                value: ann_store::encode_vector_mutation(&vector),
                expires_at_micros: None,
            }));
        self.dirty[3] = true;
        Ok(count)
    }

    /// Deletes one vector from the transaction-private ANN generation.
    ///
    /// Returns `true` only when the object currently has a visible vector.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index or failed canonical rebuild.
    pub fn delete_vector(
        &mut self,
        index: ObjectId,
        object_id: ObjectId,
    ) -> Result<bool, NativeRuntimeError> {
        if !self.state.ann.delete(index, object_id)? {
            return Ok(false);
        }
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::DeleteVector,
            target: Some(index),
            key: ann_store::encode_object_identity(object_id),
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[3] = true;
        Ok(true)
    }

    /// Executes approximate vector search over the snapshot plus private
    /// writes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index or invalid query options.
    pub fn search_ann(
        &self,
        index: ObjectId,
        query: &Vector,
        options: AnnSearchOptions,
    ) -> Result<AnnSearchReceipt, NativeRuntimeError> {
        let result = self.state.ann.search(index, query, options)?;
        Ok(ann_search_receipt(index, self.snapshot.visible_csn, result))
    }

    /// Executes the complete exact vector-ranking oracle over the snapshot
    /// plus private writes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown ANN index or invalid query vector.
    pub fn search_vector_exact(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        self.state.ann.search_exact(index, query, k)
    }

    /// Returns the snapshot CSN captured before private preparation.
    pub fn read_csn(&self) -> Option<Csn> {
        self.snapshot.visible_csn
    }

    /// Explicitly discards this detached write batch.
    pub fn rollback(self) {
        drop(self);
    }
}

impl NativeTransaction<'_> {
    /// Commits through the normal native path.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty write set, persistence, synchronization,
    /// codec, or MVCC publication failure.
    pub fn commit(self) -> Result<CommitReceipt, NativeRuntimeError> {
        self.commit_at(None)
    }

    /// Explicitly rolls back all private engine and catalog changes.
    pub fn rollback(self) {
        drop(self);
    }

    /// Commits with one deterministic interruption for crash-matrix testing.
    ///
    /// After an injected interruption the caller must drop the database handle
    /// and reopen the data directory.
    ///
    /// # Errors
    ///
    /// Returns [`NativeRuntimeError::InjectedCrash`] at the requested boundary,
    /// or another persistence/validation error.
    pub fn commit_with_interruption(
        self,
        boundary: CommitBoundary,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        self.commit_at(Some(boundary))
    }

    fn validated_write_keys(&self) -> Result<Vec<WriteKey>, WriteConflict> {
        let write_keys = mutation_write_keys(&self.batch.mutations);
        self.conflicts
            .validate(self.conflict_read_csn, &write_keys)?;
        Ok(write_keys)
    }

    fn commit_catalog_version(&self) -> Result<CatalogVersion, CatalogError> {
        if self.batch.dirty[0] {
            self.batch
                .snapshot
                .catalog_version
                .checked_next()
                .ok_or(CatalogError::VersionExhausted)
        } else {
            Ok(self.batch.snapshot.catalog_version)
        }
    }

    fn commit_at(
        mut self,
        interruption: Option<CommitBoundary>,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        if self.batch.mutations.is_empty() {
            return Err(WalSemanticError::InvalidSequence.into());
        }
        let commit_csn = self.root_transaction.commit_csn()?;
        let write_keys = self.validated_write_keys()?;
        let catalog_version = self.commit_catalog_version()?;
        let batch = self.batch;
        let synchronize = batch.durability != DurabilityClass::Memory;
        let staged_blobs = stage_large_values(self.blobs, &batch.mutations, synchronize)?;
        interrupt(interruption, CommitBoundary::BlobStaged)?;
        let blob_references = publish_staged_blobs(self.blobs, staged_blobs, synchronize)?;
        let blob_generation = self.blobs.generation()?;
        interrupt(interruption, CommitBoundary::BlobPromoted)?;

        let roots = commit_engine_roots(
            self.pages,
            self.blobs,
            roots_from_snapshot(batch.snapshot.roots()),
            self.relational_format,
            self.structure_format,
            self.search_format,
            commit_csn,
            &batch,
            &blob_references,
        )?;
        interrupt(interruption, CommitBoundary::PageAppended)?;
        if synchronize {
            self.pages.sync_data()?;
        }
        interrupt(interruption, CommitBoundary::PageSynchronized)?;

        let concrete_roots = require_roots(roots)?;
        let wal_mutations = wal_mutations(&batch.mutations, &blob_references)?;
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: self.transaction_id,
            read_csn: self.conflict_read_csn,
            catalog_version,
            logical_time_micros: batch.snapshot.logical_time_micros,
            durability: batch.durability,
            mutations: &wal_mutations,
            commit_csn,
            roots: concrete_roots,
            blob_generation,
        })?;
        let receipts = self.wal.append_records(pending, false)?;
        interrupt(interruption, CommitBoundary::WalAppended)?;
        if synchronize {
            self.wal.sync_data()?;
        }
        interrupt(interruption, CommitBoundary::WalSynchronized)?;

        let block = receipts.last().ok_or(WalError::EmptyBlock)?;
        for (slot, page) in ROOT_SLOTS.into_iter().zip(concrete_roots) {
            self.root_transaction.set_root(slot, page);
        }
        self.root_transaction.set_blob_generation(blob_generation);
        self.root_transaction.commit(
            catalog_version,
            WalAnchor::new(block.last_lsn, block.digest)?,
        )?;
        self.conflicts.publish_committed(commit_csn, write_keys);
        *self.next_transaction_id = self
            .transaction_id
            .get()
            .checked_add(1)
            .ok_or(NativeRuntimeError::TransactionIdExhausted)?;
        interrupt(interruption, CommitBoundary::RootPublished)?;
        Ok(CommitReceipt {
            transaction_id: self.transaction_id,
            commit_csn,
            catalog_version,
            commit_lsn: block.last_lsn,
            wal_block_digest: block.digest,
            durability: batch.durability,
        })
    }
}

fn apply_structure_mutation(
    state: &mut StructureState,
    mutation: &Mutation,
) -> Result<(), NativeRuntimeError> {
    match mutation.opcode {
        Opcode::SetValue => {
            if mutation.target.is_some()
                || state.hashes.contains_key(&mutation.key)
                || state.sets.contains_key(&mutation.key)
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            state.set(
                mutation.key.clone(),
                mutation.value.clone(),
                mutation.expires_at_micros,
            );
        }
        Opcode::DeleteValue => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
                || state.hashes.contains_key(&mutation.key)
                || state.sets.contains_key(&mutation.key)
                || state.delete(&mutation.key).is_none()
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::ExpireValue => {
            if mutation.target.is_some()
                || mutation.expires_at_micros.is_none()
                || state.hashes.contains_key(&mutation.key)
                || state.sets.contains_key(&mutation.key)
                || !state.entries.contains_key(&mutation.key)
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            state.set(
                mutation.key.clone(),
                mutation.value.clone(),
                mutation.expires_at_micros,
            );
        }
        Opcode::CreateHash => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
                || !state.create_hash(mutation.key.clone())
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::SetHashField => {
            if mutation.target.is_some() || mutation.expires_at_micros.is_some() {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            let (key, field) = decode_hash_field_identity(&mutation.key)?;
            if state
                .hset(key, field.to_vec(), mutation.value.clone())
                .is_none()
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::DeleteHashField => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            let (key, field) = decode_hash_field_identity(&mutation.key)?;
            if state.hdelete(key, field) != Some(true) {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::CreateSet | Opcode::AddSetMember | Opcode::DeleteSetMember => {
            apply_set_mutation(state, mutation)?;
        }
        _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
    }
    Ok(())
}

fn apply_set_mutation(
    state: &mut StructureState,
    mutation: &Mutation,
) -> Result<(), NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateSet => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
                || !state.create_set(mutation.key.clone())
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::AddSetMember => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            let (key, member) = decode_set_member_identity(&mutation.key)?;
            if state.sadd(key, member.to_vec()) != Some(true) {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        Opcode::DeleteSetMember => {
            if mutation.target.is_some()
                || !mutation.value.is_empty()
                || mutation.expires_at_micros.is_some()
            {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            let (key, member) = decode_set_member_identity(&mutation.key)?;
            if state.srem(key, member) != Some(true) {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
        }
        _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
    }
    Ok(())
}

fn apply_mutations_to_state(
    state: &mut MaterializedState,
    mutations: &[Mutation],
) -> Result<(), NativeRuntimeError> {
    for mutation in mutations {
        match mutation.opcode {
            Opcode::CreateTable
            | Opcode::InsertRow
            | Opcode::UpdateRow
            | Opcode::DeleteRow
            | Opcode::CreateSecondaryIndex => {
                apply_relational_mutation(state, mutation)?;
            }
            Opcode::SetValue
            | Opcode::DeleteValue
            | Opcode::ExpireValue
            | Opcode::CreateHash
            | Opcode::SetHashField
            | Opcode::DeleteHashField
            | Opcode::CreateSet
            | Opcode::AddSetMember
            | Opcode::DeleteSetMember => {
                apply_structure_mutation(&mut state.structures, mutation)?;
            }
            Opcode::CreateIndex => {
                let index = mutation
                    .target
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
                let object = decode_search_creation(index, mutation)?;
                state.catalog.create(object)?;
                state.search.create_index(index)?;
            }
            Opcode::IndexDocument => {
                let index = mutation
                    .target
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
                let text = std::str::from_utf8(&mutation.value)
                    .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
                validate_search_document_identity(&mutation.key, text)
                    .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
                state.catalog.require(index, EngineKind::Search)?;
                state
                    .search
                    .index_document(index, mutation.key.clone(), text.to_owned())?;
            }
            Opcode::CreateAnnIndex => {
                let index = mutation
                    .target
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
                let object = decode_ann_creation(index, mutation)?;
                let CatalogObject::Search(definition) = &object else {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                };
                let definition = ann_store::definition_from_search(definition)?;
                state.catalog.create(object)?;
                state.ann.create(definition)?;
            }
            Opcode::UpsertVector => {
                let index = mutation
                    .target
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
                state.ann.upsert(
                    index,
                    ann_store::decode_object_identity(&mutation.key)?,
                    ann_store::private_mutation_csn()?,
                    ann_store::decode_vector_mutation(&mutation.value)?,
                )?;
            }
            Opcode::DeleteVector => {
                let index = mutation
                    .target
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
                if !state
                    .ann
                    .delete(index, ann_store::decode_object_identity(&mutation.key)?)?
                {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                }
            }
        }
    }
    Ok(())
}

fn apply_relational_mutation(
    state: &mut MaterializedState,
    mutation: &Mutation,
) -> Result<(), NativeRuntimeError> {
    let target = mutation
        .target
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
    match mutation.opcode {
        Opcode::CreateTable => {
            let object = decode_relation_creation(target, mutation)?;
            state.catalog.create(object)?;
            state.relational.create_table(target)?;
        }
        Opcode::InsertRow => {
            state.catalog.require(target, EngineKind::Relational)?;
            let projections = secondary_index_projections(&state.catalog, target, &mutation.value)?;
            state
                .relational
                .insert(target, mutation.key.clone(), mutation.value.clone())?;
            for projection in projections {
                state.relational.insert_secondary_index(
                    projection.index,
                    projection.key,
                    mutation.key.clone(),
                    projection.contains_null,
                )?;
            }
        }
        Opcode::UpdateRow | Opcode::DeleteRow => {
            state.catalog.require(target, EngineKind::Relational)?;
            let old_row = state
                .relational
                .select(target, &mutation.key)
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?
                .to_vec();
            let old_projections = secondary_index_projections(&state.catalog, target, &old_row)?;
            remove_secondary_index_projections(
                &mut state.relational,
                &old_projections,
                &mutation.key,
            )?;
            if mutation.opcode == Opcode::UpdateRow {
                let new_projections =
                    secondary_index_projections(&state.catalog, target, &mutation.value)?;
                state
                    .relational
                    .update(target, &mutation.key, mutation.value.clone())?;
                insert_secondary_index_projections(
                    &mut state.relational,
                    &new_projections,
                    &mutation.key,
                )?;
            } else {
                if !mutation.value.is_empty() {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                }
                state.relational.delete(target, &mutation.key)?;
            }
        }
        Opcode::CreateSecondaryIndex => {
            apply_secondary_index_creation(state, target, mutation)?;
        }
        _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
    }
    Ok(())
}

fn apply_secondary_index_creation(
    state: &mut MaterializedState,
    index: ObjectId,
    mutation: &Mutation,
) -> Result<(), NativeRuntimeError> {
    let object = decode_secondary_index_creation(index, mutation)?;
    let CatalogObject::SecondaryIndex(definition) = &object else {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    };
    let definition = definition.clone();
    state.catalog.create(object)?;
    state.relational.create_secondary_index(
        index,
        definition.relation,
        definition.unique,
        definition.nulls_distinct,
    )?;
    let rows = state
        .relational
        .tables
        .get(&definition.relation)
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?
        .iter()
        .map(|(primary_key, row)| (primary_key.clone(), row.clone()))
        .collect::<Vec<_>>();
    for (primary_key, row) in rows {
        let projection = secondary_index_projection(&state.catalog, &definition, &row)?;
        state.relational.insert_secondary_index(
            index,
            projection.key,
            primary_key,
            projection.contains_null,
        )?;
    }
    Ok(())
}

fn mutation_write_keys(mutations: &[Mutation]) -> Vec<WriteKey> {
    let mut keys = Vec::with_capacity(mutations.len().saturating_mul(3));
    for mutation in mutations {
        let identity = match mutation.opcode {
            Opcode::SetValue
            | Opcode::DeleteValue
            | Opcode::ExpireValue
            | Opcode::CreateHash
            | Opcode::CreateSet => {
                let mut identity = Vec::with_capacity(mutation.key.len().saturating_add(1));
                identity.push(0);
                identity.extend_from_slice(&mutation.key);
                identity
            }
            Opcode::SetHashField | Opcode::DeleteHashField => {
                let mut identity = Vec::with_capacity(mutation.key.len().saturating_add(1));
                identity.push(1);
                identity.extend_from_slice(&mutation.key);
                identity
            }
            Opcode::AddSetMember | Opcode::DeleteSetMember => {
                let mut identity = Vec::with_capacity(mutation.key.len().saturating_add(1));
                identity.push(2);
                identity.extend_from_slice(&mutation.key);
                identity
            }
            Opcode::UpsertVector | Opcode::DeleteVector => {
                let mut identity = Vec::with_capacity(mutation.key.len().saturating_add(1));
                identity.push(3);
                identity.extend_from_slice(&mutation.key);
                identity
            }
            _ => mutation.key.clone(),
        };
        keys.push(WriteKey::new(mutation.engine, mutation.target, identity));
        if matches!(
            mutation.opcode,
            Opcode::CreateTable
                | Opcode::CreateSecondaryIndex
                | Opcode::CreateIndex
                | Opcode::CreateAnnIndex
        ) {
            if let Some(object) = mutation.target {
                let mut object_key = Vec::with_capacity(17);
                object_key.push(1);
                object_key.extend_from_slice(&object.get().to_be_bytes());
                keys.push(WriteKey::new(EngineKind::Kernel, None, object_key));
            }
            let name_identity = if mutation.key.is_empty() {
                mutation.value.as_slice()
            } else {
                mutation.key.as_slice()
            };
            let mut name_key = Vec::with_capacity(name_identity.len().saturating_add(2));
            name_key.extend_from_slice(&[2, mutation.engine as u8]);
            name_key.extend_from_slice(name_identity);
            keys.push(WriteKey::new(EngineKind::Kernel, None, name_key));
        }
    }
    keys
}

fn interrupt(
    requested: Option<CommitBoundary>,
    current: CommitBoundary,
) -> Result<(), NativeRuntimeError> {
    if requested == Some(current) {
        Err(NativeRuntimeError::InjectedCrash(current))
    } else {
        Ok(())
    }
}

fn interrupt_checkpoint(
    requested: Option<CheckpointBoundary>,
    current: CheckpointBoundary,
) -> Result<(), NativeRuntimeError> {
    if requested == Some(current) {
        Err(NativeRuntimeError::InjectedCheckpointCrash(current))
    } else {
        Ok(())
    }
}

struct CheckpointValidation {
    last_checkpoint_lsn: Option<Lsn>,
    latest_generation: Option<ManifestGeneration>,
    unanchored_manifest_suffix: usize,
}

fn validate_checkpoints(
    recovered: &RecoveredWal,
    manifests: &[RootManifest],
    committed_roots: &BTreeMap<Csn, RootSet>,
) -> Result<CheckpointValidation, NativeRuntimeError> {
    for checkpoint in &recovered.checkpoints {
        let generation_index = checkpoint
            .manifest_generation
            .get()
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or(NativeRuntimeError::InvalidCheckpoint)?;
        let manifest = manifests
            .get(generation_index)
            .ok_or(NativeRuntimeError::InvalidCheckpoint)?;
        let committed_root = committed_roots
            .get(&checkpoint.visible_csn)
            .ok_or(NativeRuntimeError::InvalidCheckpoint)?;
        if manifest.generation() != checkpoint.manifest_generation
            || manifest.visible_csn() != checkpoint.visible_csn
            || manifest.digest() != checkpoint.manifest_digest
            || manifest.to_root_set()? != *committed_root
        {
            return Err(NativeRuntimeError::InvalidCheckpoint);
        }
    }
    let latest = recovered.checkpoints.last();
    let anchored_manifest_count = latest
        .map(|checkpoint| {
            usize::try_from(checkpoint.manifest_generation.get())
                .map_err(|_| NativeRuntimeError::InvalidCheckpoint)
        })
        .transpose()?
        .unwrap_or(0);
    let unanchored_manifest_suffix = manifests
        .len()
        .checked_sub(anchored_manifest_count)
        .ok_or(NativeRuntimeError::InvalidCheckpoint)?;
    Ok(CheckpointValidation {
        last_checkpoint_lsn: latest.map(|checkpoint| checkpoint.checkpoint_lsn),
        latest_generation: latest.map(|checkpoint| checkpoint.manifest_generation),
        unanchored_manifest_suffix,
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_engine_roots(
    pages: &mut PageStore,
    blobs: &BlobStore,
    mut roots: [Option<PageId>; 4],
    relational_format: RelationalFormat,
    structure_format: StructureFormat,
    search_format: SearchFormat,
    commit_csn: Csn,
    batch: &NativeWriteBatch,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<[Option<PageId>; 4], NativeRuntimeError> {
    if batch.dirty[0] || roots[0].is_none() {
        roots[0] = Some(pages.append(
            PageKind::CatalogRoot,
            Some(commit_csn),
            None,
            batch.state.catalog.encode()?,
        )?);
    }
    if batch.dirty[1] || roots[1].is_none() {
        roots[1] = relational_tree_after_mutations(
            pages,
            blobs,
            roots[1],
            relational_format,
            commit_csn,
            &batch.state.catalog,
            &batch.state.relational,
            &batch.mutations,
            blob_references,
        )?
        .root();
    }
    if batch.dirty[2] || roots[2].is_none() {
        roots[2] = structure_root_after_mutations(
            pages,
            roots[2],
            structure_format,
            commit_csn,
            &batch.state.structures,
            &batch.mutations,
            blob_references,
        )?;
    }
    if batch.dirty[3] || roots[3].is_none() {
        roots[3] = search_root_after_mutations(
            pages,
            roots[3],
            commit_csn,
            &SearchMutationContext {
                format: search_format,
                catalog: &batch.state.catalog,
                state: &batch.state.search,
                mutations: &batch.mutations,
                blob_references,
            },
        )?;
    }
    Ok(roots)
}

fn stage_large_values(
    blobs: &BlobStore,
    mutations: &[Mutation],
    synchronize: bool,
) -> Result<BTreeMap<[u8; 32], StagedBlob>, NativeRuntimeError> {
    let mut staged = BTreeMap::new();
    for mutation in mutations.iter().filter(|mutation| {
        (mutation.engine == EngineKind::Relational
            && matches!(mutation.opcode, Opcode::InsertRow | Opcode::UpdateRow)
            && mutation.value.len() > RELATIONAL_INLINE_VALUE_LIMIT)
            || (mutation.engine == EngineKind::Structure
                && matches!(
                    mutation.opcode,
                    Opcode::SetValue | Opcode::ExpireValue | Opcode::SetHashField
                )
                && mutation.value.len() > STRUCTURE_INLINE_VALUE_LIMIT)
            || (mutation.engine == EngineKind::Search
                && mutation.opcode == Opcode::IndexDocument
                && mutation.value.len() > SEARCH_INLINE_VALUE_LIMIT)
    }) {
        let digest = *blake3::hash(&mutation.value).as_bytes();
        if let std::collections::btree_map::Entry::Vacant(entry) = staged.entry(digest) {
            entry.insert(blobs.stage(&mutation.value, synchronize)?);
        }
    }
    Ok(staged)
}

fn publish_staged_blobs(
    blobs: &mut BlobStore,
    staged: BTreeMap<[u8; 32], StagedBlob>,
    synchronize: bool,
) -> Result<BTreeMap<[u8; 32], BlobReference>, NativeRuntimeError> {
    staged
        .into_iter()
        .map(|(digest, staged)| Ok((digest, blobs.publish(staged, synchronize)?)))
        .collect()
}

fn relational_storage_value(
    value: &[u8],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<Vec<u8>, NativeRuntimeError> {
    if value.len() <= RELATIONAL_INLINE_VALUE_LIMIT {
        let mut encoded = Vec::with_capacity(value.len() + 1);
        encoded.push(RELATIONAL_VALUE_INLINE);
        encoded.extend_from_slice(value);
        return Ok(encoded);
    }
    let digest = *blake3::hash(value).as_bytes();
    let reference = blob_references
        .get(&digest)
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    let mut encoded = Vec::with_capacity(1 + hyphae_native_records::BLOB_REFERENCE_SIZE);
    encoded.push(RELATIONAL_VALUE_BLOB);
    encoded.extend_from_slice(&reference.encode());
    Ok(encoded)
}

fn structure_storage_value(
    value: &[u8],
    expires_at_micros: Option<i64>,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let uses_blob = value.len() > STRUCTURE_INLINE_VALUE_LIMIT;
    let payload_length = if uses_blob {
        hyphae_native_records::BLOB_REFERENCE_SIZE
    } else {
        value.len()
    };
    let mut encoded = Vec::with_capacity(STRUCTURE_VALUE_HEADER_SIZE + payload_length);
    encoded.extend_from_slice(STRUCTURE_VALUE_MAGIC);
    encoded.push(u8::from(expires_at_micros.is_some()) * STRUCTURE_VALUE_HAS_EXPIRY);
    encoded.push(if uses_blob {
        STRUCTURE_VALUE_BLOB
    } else {
        STRUCTURE_VALUE_INLINE
    });
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(&expires_at_micros.unwrap_or(0).to_le_bytes());
    if uses_blob {
        let digest = *blake3::hash(value).as_bytes();
        let reference = blob_references
            .get(&digest)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        encoded.extend_from_slice(&reference.encode());
    } else {
        encoded.extend_from_slice(value);
    }
    Ok(encoded)
}

fn search_document_storage_value(
    text: &[u8],
    token_count: u64,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let uses_blob = text.len() > SEARCH_INLINE_VALUE_LIMIT;
    let payload_length = if uses_blob {
        hyphae_native_records::BLOB_REFERENCE_SIZE
    } else {
        text.len()
    };
    let mut encoded = Vec::with_capacity(SEARCH_DOCUMENT_HEADER_SIZE + payload_length);
    encoded.extend_from_slice(SEARCH_DOCUMENT_MAGIC);
    encoded.push(if uses_blob {
        SEARCH_DOCUMENT_BLOB
    } else {
        SEARCH_DOCUMENT_INLINE
    });
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&token_count.to_le_bytes());
    if uses_blob {
        let digest = *blake3::hash(text).as_bytes();
        let reference = blob_references
            .get(&digest)
            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
        encoded.extend_from_slice(&reference.encode());
    } else {
        encoded.extend_from_slice(text);
    }
    Ok(encoded)
}

fn structure_tombstone_value() -> Vec<u8> {
    let mut encoded = Vec::with_capacity(STRUCTURE_VALUE_HEADER_SIZE);
    encoded.extend_from_slice(STRUCTURE_VALUE_MAGIC);
    encoded.push(STRUCTURE_VALUE_TOMBSTONE);
    encoded.push(STRUCTURE_VALUE_INLINE);
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(&0_i64.to_le_bytes());
    encoded
}

fn is_structure_tombstone(encoded: &[u8]) -> bool {
    encoded.len() == STRUCTURE_VALUE_HEADER_SIZE
        && encoded.get(..8) == Some(STRUCTURE_VALUE_MAGIC.as_slice())
        && encoded[8] == STRUCTURE_VALUE_TOMBSTONE
        && encoded[9] == STRUCTURE_VALUE_INLINE
        && encoded[10..24].iter().all(|byte| *byte == 0)
}

fn decode_structure_value(
    encoded: &[u8],
    blobs: &BlobStore,
) -> Result<Option<StructureEntry>, NativeRuntimeError> {
    if encoded.len() < STRUCTURE_VALUE_HEADER_SIZE
        || encoded.get(..8) != Some(STRUCTURE_VALUE_MAGIC.as_slice())
        || !matches!(
            encoded[8],
            0 | STRUCTURE_VALUE_HAS_EXPIRY | STRUCTURE_VALUE_TOMBSTONE
        )
        || encoded[10..16] != [0; 6]
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut expiry_bytes = [0_u8; 8];
    expiry_bytes.copy_from_slice(&encoded[16..24]);
    let raw_expiry = i64::from_le_bytes(expiry_bytes);
    let payload = &encoded[STRUCTURE_VALUE_HEADER_SIZE..];
    if encoded[8] == STRUCTURE_VALUE_TOMBSTONE {
        if !is_structure_tombstone(encoded) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        return Ok(None);
    }
    let expires_at_micros = if encoded[8] == STRUCTURE_VALUE_HAS_EXPIRY {
        Some(raw_expiry)
    } else if raw_expiry == 0 {
        None
    } else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    let value = match encoded[9] {
        STRUCTURE_VALUE_INLINE if payload.len() <= STRUCTURE_INLINE_VALUE_LIMIT => payload.to_vec(),
        STRUCTURE_VALUE_BLOB if payload.len() == hyphae_native_records::BLOB_REFERENCE_SIZE => {
            let reference = BlobReference::decode(payload)?;
            if reference.logical_length <= STRUCTURE_INLINE_VALUE_LIMIT as u64 {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            blobs.read(reference)?
        }
        _ => return Err(NativeRuntimeError::InvalidStructureTree),
    };
    Ok(Some(StructureEntry {
        value,
        expires_at_micros,
    }))
}

fn wal_mutations(
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<Vec<Mutation>, NativeRuntimeError> {
    mutations
        .iter()
        .cloned()
        .map(|mut mutation| {
            if mutation.engine == EngineKind::Relational
                && matches!(mutation.opcode, Opcode::InsertRow | Opcode::UpdateRow)
            {
                mutation.value = relational_storage_value(&mutation.value, blob_references)?;
            } else if mutation.engine == EngineKind::Structure
                && matches!(
                    mutation.opcode,
                    Opcode::SetValue | Opcode::ExpireValue | Opcode::SetHashField
                )
            {
                mutation.value = structure_storage_value(
                    &mutation.value,
                    mutation.expires_at_micros,
                    blob_references,
                )?;
            } else if mutation.engine == EngineKind::Search
                && mutation.opcode == Opcode::IndexDocument
            {
                let text = std::str::from_utf8(&mutation.value)
                    .map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
                mutation.value = search_document_storage_value(
                    &mutation.value,
                    u64::try_from(analyze(text).len())
                        .map_err(|_| NativeRuntimeError::InvalidSearchTree)?,
                    blob_references,
                )?;
            }
            Ok(mutation)
        })
        .collect()
}

fn structure_key(key: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(key.len().saturating_add(1));
    encoded.push(STRUCTURE_ENTRY_PREFIX);
    encoded.extend_from_slice(key);
    encoded
}

fn structure_hash_meta_key(key: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(key.len().saturating_add(1));
    encoded.push(STRUCTURE_HASH_META_PREFIX);
    encoded.extend_from_slice(key);
    encoded
}

fn structure_set_meta_key(key: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(key.len().saturating_add(1));
    encoded.push(STRUCTURE_SET_META_PREFIX);
    encoded.extend_from_slice(key);
    encoded
}

fn collection_member_identity(key: &[u8], member: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let key_length =
        u32::try_from(key.len()).map_err(|_| NativeRuntimeError::StructureIdentityTooLarge)?;
    let mut encoded = Vec::with_capacity(
        4_usize
            .saturating_add(key.len())
            .saturating_add(member.len()),
    );
    encoded.extend_from_slice(&key_length.to_be_bytes());
    encoded.extend_from_slice(key);
    encoded.extend_from_slice(member);
    Ok(encoded)
}

fn decode_collection_member_identity(encoded: &[u8]) -> Result<(&[u8], &[u8]), NativeRuntimeError> {
    let length_bytes = encoded
        .get(..4)
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
    let mut length = [0_u8; 4];
    length.copy_from_slice(length_bytes);
    let key_length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
    let member_start = 4_usize
        .checked_add(key_length)
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
    if member_start > encoded.len() {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok((&encoded[4..member_start], &encoded[member_start..]))
}

fn hash_field_identity(key: &[u8], field: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    collection_member_identity(key, field)
}

fn decode_hash_field_identity(encoded: &[u8]) -> Result<(&[u8], &[u8]), NativeRuntimeError> {
    decode_collection_member_identity(encoded)
}

fn set_member_identity(key: &[u8], member: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    collection_member_identity(key, member)
}

fn decode_set_member_identity(encoded: &[u8]) -> Result<(&[u8], &[u8]), NativeRuntimeError> {
    decode_collection_member_identity(encoded)
}

fn structure_hash_field_key(key: &[u8], field: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let identity = hash_field_identity(key, field)?;
    let mut encoded = Vec::with_capacity(identity.len().saturating_add(1));
    encoded.push(STRUCTURE_HASH_FIELD_PREFIX);
    encoded.extend_from_slice(&identity);
    Ok(encoded)
}

fn structure_set_member_key(key: &[u8], member: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let identity = set_member_identity(key, member)?;
    let mut encoded = Vec::with_capacity(identity.len().saturating_add(1));
    encoded.push(STRUCTURE_SET_MEMBER_PREFIX);
    encoded.extend_from_slice(&identity);
    Ok(encoded)
}

fn encode_hash_metadata(field_count: u64) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(STRUCTURE_HASH_META_SIZE);
    encoded.extend_from_slice(STRUCTURE_HASH_META_MAGIC);
    encoded.extend_from_slice(&field_count.to_le_bytes());
    encoded
}

fn decode_hash_metadata(encoded: &[u8]) -> Result<u64, NativeRuntimeError> {
    if encoded.len() != STRUCTURE_HASH_META_SIZE
        || encoded.get(..8) != Some(STRUCTURE_HASH_META_MAGIC.as_slice())
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut count = [0_u8; 8];
    count.copy_from_slice(&encoded[8..16]);
    Ok(u64::from_le_bytes(count))
}

fn decode_hash_field_value(
    encoded: &[u8],
    blobs: &BlobStore,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    match decode_structure_value(encoded, blobs)? {
        Some(entry) if entry.expires_at_micros.is_none() => Ok(Some(entry.value)),
        Some(_) => Err(NativeRuntimeError::InvalidStructureTree),
        None => Ok(None),
    }
}

fn encode_set_metadata(member_count: u64) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(STRUCTURE_SET_META_SIZE);
    encoded.extend_from_slice(STRUCTURE_SET_META_MAGIC);
    encoded.extend_from_slice(&member_count.to_le_bytes());
    encoded
}

fn decode_set_metadata(encoded: &[u8]) -> Result<u64, NativeRuntimeError> {
    if encoded.len() != STRUCTURE_SET_META_SIZE
        || encoded.get(..8) != Some(STRUCTURE_SET_META_MAGIC.as_slice())
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut count = [0_u8; 8];
    count.copy_from_slice(&encoded[8..16]);
    Ok(u64::from_le_bytes(count))
}

fn set_member_live_value() -> Vec<u8> {
    let mut encoded = Vec::with_capacity(STRUCTURE_VALUE_HEADER_SIZE);
    encoded.extend_from_slice(STRUCTURE_VALUE_MAGIC);
    encoded.push(0);
    encoded.push(STRUCTURE_VALUE_INLINE);
    encoded.extend_from_slice(&[0; 14]);
    encoded
}

fn decode_set_member_value(encoded: &[u8]) -> Result<bool, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        Ok(false)
    } else if encoded == set_member_live_value() {
        Ok(true)
    } else {
        Err(NativeRuntimeError::InvalidStructureTree)
    }
}

fn validate_search_document_identity(
    document_id: &[u8],
    text: &str,
) -> Result<(), NativeRuntimeError> {
    if 17_usize
        .checked_add(document_id.len())
        .is_none_or(|length| length > BTREE_MAX_KEY_SIZE)
    {
        return Err(NativeRuntimeError::SearchIdentityTooLarge);
    }
    for term in analyze(text) {
        let term_length = term.len();
        if 17_usize
            .checked_add(term_length)
            .is_none_or(|length| length > BTREE_MAX_KEY_SIZE)
            || 21_usize
                .checked_add(term_length)
                .and_then(|length| length.checked_add(document_id.len()))
                .is_none_or(|length| length > BTREE_MAX_KEY_SIZE)
        {
            return Err(NativeRuntimeError::SearchIdentityTooLarge);
        }
    }
    Ok(())
}

fn search_index_meta_key(index: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(SEARCH_INDEX_META_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key
}

fn search_document_key(index: ObjectId, document_id: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut key = Vec::with_capacity(17_usize.saturating_add(document_id.len()));
    key.push(SEARCH_DOCUMENT_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key.extend_from_slice(document_id);
    if key.len() > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::SearchIdentityTooLarge);
    }
    Ok(key)
}

fn search_term_meta_key(index: ObjectId, term: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    if term.is_empty() {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut key = Vec::with_capacity(17_usize.saturating_add(term.len()));
    key.push(SEARCH_TERM_META_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key.extend_from_slice(term);
    if key.len() > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::SearchIdentityTooLarge);
    }
    Ok(key)
}

fn search_posting_prefix(index: ObjectId, term: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    if term.is_empty() {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let term_length =
        u32::try_from(term.len()).map_err(|_| NativeRuntimeError::SearchIdentityTooLarge)?;
    let mut key = Vec::with_capacity(21_usize.saturating_add(term.len()));
    key.push(SEARCH_POSTING_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key.extend_from_slice(&term_length.to_be_bytes());
    key.extend_from_slice(term);
    if key.len() > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::SearchIdentityTooLarge);
    }
    Ok(key)
}

fn search_posting_key(
    index: ObjectId,
    term: &[u8],
    document_id: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut key = search_posting_prefix(index, term)?;
    key.extend_from_slice(document_id);
    if key.len() > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::SearchIdentityTooLarge);
    }
    Ok(key)
}

fn decode_search_object_key(
    key: &[u8],
    prefix: u8,
) -> Result<(ObjectId, &[u8]), NativeRuntimeError> {
    if key.len() < 17 || key[0] != prefix {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&key[1..17]);
    let index = ObjectId::new(u128::from_be_bytes(encoded))
        .map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
    Ok((index, &key[17..]))
}

fn decode_search_posting_key(key: &[u8]) -> Result<(ObjectId, &[u8], &[u8]), NativeRuntimeError> {
    let (index, identity) = decode_search_object_key(key, SEARCH_POSTING_PREFIX)?;
    let encoded_length = identity
        .get(..4)
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    let mut length = [0_u8; 4];
    length.copy_from_slice(encoded_length);
    let term_length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
    let term_end = 4_usize
        .checked_add(term_length)
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    if term_length == 0 || term_end > identity.len() {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    Ok((index, &identity[4..term_end], &identity[term_end..]))
}

fn encode_search_index_metadata(document_count: u64, total_document_terms: u64) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(SEARCH_INDEX_META_SIZE);
    encoded.extend_from_slice(SEARCH_INDEX_META_MAGIC);
    encoded.extend_from_slice(&document_count.to_le_bytes());
    encoded.extend_from_slice(&total_document_terms.to_le_bytes());
    encoded
}

fn decode_search_index_metadata(encoded: &[u8]) -> Result<(u64, u64), NativeRuntimeError> {
    if encoded.len() != SEARCH_INDEX_META_SIZE
        || encoded.get(..8) != Some(SEARCH_INDEX_META_MAGIC.as_slice())
    {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut document_count = [0_u8; 8];
    document_count.copy_from_slice(&encoded[8..16]);
    let mut total_document_terms = [0_u8; 8];
    total_document_terms.copy_from_slice(&encoded[16..24]);
    Ok((
        u64::from_le_bytes(document_count),
        u64::from_le_bytes(total_document_terms),
    ))
}

fn decode_search_document_header(encoded: &[u8]) -> Result<(u64, u8, &[u8]), NativeRuntimeError> {
    if encoded.len() < SEARCH_DOCUMENT_HEADER_SIZE
        || encoded.get(..8) != Some(SEARCH_DOCUMENT_MAGIC.as_slice())
        || !matches!(encoded[8], SEARCH_DOCUMENT_INLINE | SEARCH_DOCUMENT_BLOB)
        || encoded[9..16].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut token_count = [0_u8; 8];
    token_count.copy_from_slice(&encoded[16..24]);
    Ok((
        u64::from_le_bytes(token_count),
        encoded[8],
        &encoded[SEARCH_DOCUMENT_HEADER_SIZE..],
    ))
}

fn decode_search_document(
    encoded: &[u8],
    blobs: &BlobStore,
) -> Result<(String, u64), NativeRuntimeError> {
    let (token_count, storage, payload) = decode_search_document_header(encoded)?;
    let text = match storage {
        SEARCH_DOCUMENT_INLINE if payload.len() <= SEARCH_INLINE_VALUE_LIMIT => payload.to_vec(),
        SEARCH_DOCUMENT_BLOB if payload.len() == hyphae_native_records::BLOB_REFERENCE_SIZE => {
            let reference = BlobReference::decode(payload)?;
            if reference.logical_length <= SEARCH_INLINE_VALUE_LIMIT as u64 {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            blobs.read(reference)?
        }
        _ => return Err(NativeRuntimeError::InvalidSearchTree),
    };
    let text = String::from_utf8(text).map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
    if u64::try_from(analyze(&text).len()).map_err(|_| NativeRuntimeError::InvalidSearchTree)?
        != token_count
    {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    Ok((text, token_count))
}

fn encode_search_term_metadata(document_frequency: u64) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(SEARCH_TERM_META_SIZE);
    encoded.extend_from_slice(SEARCH_TERM_META_MAGIC);
    encoded.extend_from_slice(&document_frequency.to_le_bytes());
    encoded
}

fn decode_search_term_metadata(encoded: &[u8]) -> Result<u64, NativeRuntimeError> {
    if encoded.len() != SEARCH_TERM_META_SIZE
        || encoded.get(..8) != Some(SEARCH_TERM_META_MAGIC.as_slice())
    {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut document_frequency = [0_u8; 8];
    document_frequency.copy_from_slice(&encoded[8..16]);
    Ok(u64::from_le_bytes(document_frequency))
}

fn encode_search_posting(term_frequency: u32) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(SEARCH_POSTING_SIZE);
    encoded.extend_from_slice(SEARCH_POSTING_MAGIC);
    encoded.extend_from_slice(&term_frequency.to_le_bytes());
    encoded.extend_from_slice(&[0; 4]);
    encoded
}

fn decode_search_posting(encoded: &[u8]) -> Result<u32, NativeRuntimeError> {
    if encoded.len() != SEARCH_POSTING_SIZE
        || encoded.get(..8) != Some(SEARCH_POSTING_MAGIC.as_slice())
        || encoded[12..16].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut term_frequency = [0_u8; 4];
    term_frequency.copy_from_slice(&encoded[8..12]);
    let term_frequency = u32::from_le_bytes(term_frequency);
    if term_frequency == 0 {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    Ok(term_frequency)
}

fn search_count_f64(value: u64) -> Result<f64, NativeRuntimeError> {
    u32::try_from(value)
        .map(f64::from)
        .map_err(|_| NativeRuntimeError::InvalidSearchTree)
}

fn parse_canonical_i64(value: &[u8]) -> Result<i64, NativeRuntimeError> {
    let text =
        std::str::from_utf8(value).map_err(|_| NativeRuntimeError::StructureValueNotInteger)?;
    let parsed = text
        .parse::<i64>()
        .map_err(|_| NativeRuntimeError::StructureValueNotInteger)?;
    if parsed.to_string().as_bytes() != value {
        return Err(NativeRuntimeError::StructureValueNotInteger);
    }
    Ok(parsed)
}

fn create_hash_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    if !mutation.value.is_empty() || mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let metadata_key = structure_hash_meta_key(&mutation.key);
    if tree.get(pages, &metadata_key)?.is_some()
        || tree
            .get(pages, &structure_set_meta_key(&mutation.key))?
            .is_some()
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if tree
        .get(pages, &structure_key(&mutation.key))?
        .is_some_and(|value| !is_structure_tombstone(&value))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(tree
        .insert_unique(pages, creating_csn, metadata_key, encode_hash_metadata(0))?
        .tree)
}

fn create_set_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    if !mutation.value.is_empty() || mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let metadata_key = structure_set_meta_key(&mutation.key);
    if tree.get(pages, &metadata_key)?.is_some()
        || tree
            .get(pages, &structure_hash_meta_key(&mutation.key))?
            .is_some()
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if tree
        .get(pages, &structure_key(&mutation.key))?
        .is_some_and(|value| !is_structure_tombstone(&value))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(tree
        .insert_unique(pages, creating_csn, metadata_key, encode_set_metadata(0))?
        .tree)
}

fn set_hash_field_in_tree(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    if mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, field) = decode_hash_field_identity(&mutation.key)?;
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let count = decode_hash_metadata(&metadata)?;
    let field_key = structure_hash_field_key(key, field)?;
    let added = tree
        .get(pages, &field_key)?
        .is_none_or(|value| is_structure_tombstone(&value));
    let value = structure_storage_value(&mutation.value, None, blob_references)?;
    tree = tree.upsert(pages, creating_csn, field_key, value)?.tree;
    if added {
        let count = count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        tree = tree
            .upsert(
                pages,
                creating_csn,
                metadata_key,
                encode_hash_metadata(count),
            )?
            .tree;
    }
    Ok(tree)
}

fn delete_hash_field_in_tree(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    if !mutation.value.is_empty() || mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, field) = decode_hash_field_identity(&mutation.key)?;
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let count = decode_hash_metadata(&metadata)?;
    let field_key = structure_hash_field_key(key, field)?;
    let field_value = tree
        .get(pages, &field_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&field_value) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    tree = tree
        .upsert(pages, creating_csn, field_key, structure_tombstone_value())?
        .tree;
    let count = count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    Ok(tree
        .upsert(
            pages,
            creating_csn,
            metadata_key,
            encode_hash_metadata(count),
        )?
        .tree)
}

fn add_set_member_in_tree(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    if !mutation.value.is_empty() || mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, member) = decode_set_member_identity(&mutation.key)?;
    let metadata_key = structure_set_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let count = decode_set_metadata(&metadata)?;
    let member_key = structure_set_member_key(key, member)?;
    if let Some(existing) = tree.get(pages, &member_key)?
        && decode_set_member_value(&existing)?
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    tree = tree
        .upsert(pages, creating_csn, member_key, set_member_live_value())?
        .tree;
    let count = count
        .checked_add(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    Ok(tree
        .upsert(
            pages,
            creating_csn,
            metadata_key,
            encode_set_metadata(count),
        )?
        .tree)
}

fn delete_set_member_in_tree(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    if !mutation.value.is_empty() || mutation.expires_at_micros.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, member) = decode_set_member_identity(&mutation.key)?;
    let metadata_key = structure_set_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let count = decode_set_metadata(&metadata)?;
    let member_key = structure_set_member_key(key, member)?;
    let member_value = tree
        .get(pages, &member_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if !decode_set_member_value(&member_value)? {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    tree = tree
        .upsert(pages, creating_csn, member_key, structure_tombstone_value())?
        .tree;
    let count = count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    Ok(tree
        .upsert(
            pages,
            creating_csn,
            metadata_key,
            encode_set_metadata(count),
        )?
        .tree)
}

fn structure_tree_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let mut tree = root.map_or_else(BTree::empty, BTree::from_root);
    if tree.root().is_none() {
        tree = tree
            .insert_unique(
                pages,
                creating_csn,
                STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V1.to_vec(),
            )?
            .tree;
    }
    for mutation in mutations
        .iter()
        .filter(|mutation| mutation.engine == EngineKind::Structure)
    {
        if mutation.target.is_some() {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let value = match mutation.opcode {
            Opcode::SetValue => structure_storage_value(
                &mutation.value,
                mutation.expires_at_micros,
                blob_references,
            )?,
            Opcode::ExpireValue if mutation.expires_at_micros.is_some() => structure_storage_value(
                &mutation.value,
                mutation.expires_at_micros,
                blob_references,
            )?,
            Opcode::DeleteValue
                if mutation.value.is_empty() && mutation.expires_at_micros.is_none() =>
            {
                structure_tombstone_value()
            }
            Opcode::CreateHash => {
                tree = create_hash_in_tree(pages, tree, creating_csn, mutation)?;
                continue;
            }
            Opcode::SetHashField => {
                tree =
                    set_hash_field_in_tree(pages, tree, creating_csn, mutation, blob_references)?;
                continue;
            }
            Opcode::DeleteHashField => {
                tree = delete_hash_field_in_tree(pages, tree, creating_csn, mutation)?;
                continue;
            }
            Opcode::CreateSet => {
                tree = create_set_in_tree(pages, tree, creating_csn, mutation)?;
                continue;
            }
            Opcode::AddSetMember => {
                tree = add_set_member_in_tree(pages, tree, creating_csn, mutation)?;
                continue;
            }
            Opcode::DeleteSetMember => {
                tree = delete_set_member_in_tree(pages, tree, creating_csn, mutation)?;
                continue;
            }
            _ => return Err(NativeRuntimeError::InvalidStructureTree),
        };
        if tree
            .get(pages, &structure_hash_meta_key(&mutation.key))?
            .is_some()
            || tree
                .get(pages, &structure_set_meta_key(&mutation.key))?
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        tree = tree
            .upsert(pages, creating_csn, structure_key(&mutation.key), value)?
            .tree;
    }
    Ok(tree)
}

fn structure_root_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    format: StructureFormat,
    creating_csn: Csn,
    state: &StructureState,
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<Option<PageId>, NativeRuntimeError> {
    match format {
        StructureFormat::InlineStateV1 => Ok(Some(pages.append(
            PageKind::StructureNode,
            Some(creating_csn),
            None,
            state.encode()?,
        )?)),
        StructureFormat::BTreeV1 => Ok(structure_tree_after_mutations(
            pages,
            root,
            creating_csn,
            mutations,
            blob_references,
        )?
        .root()),
    }
}

fn search_tree_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    catalog: &CatalogState,
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let mut tree = root.map_or_else(BTree::empty, BTree::from_root);
    if tree.root().is_none() {
        tree = tree
            .insert_unique(
                pages,
                creating_csn,
                SEARCH_FORMAT_KEY.to_vec(),
                SEARCH_FORMAT_VALUE_V1.to_vec(),
            )?
            .tree;
    }
    for mutation in mutations.iter().filter(|mutation| {
        mutation.engine == EngineKind::Search
            && matches!(mutation.opcode, Opcode::CreateIndex | Opcode::IndexDocument)
    }) {
        tree = apply_search_tree_mutation(pages, tree, creating_csn, mutation, blob_references)?;
    }
    ann_store::apply_tree_mutations(pages, tree, creating_csn, catalog, mutations)
}

fn apply_search_tree_mutation(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let index = mutation
        .target
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    match mutation.opcode {
        Opcode::CreateIndex => {
            if mutation.expires_at_micros.is_some()
                || decode_search_creation(index, mutation).is_err()
            {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            Ok(tree
                .insert_unique(
                    pages,
                    creating_csn,
                    search_index_meta_key(index),
                    encode_search_index_metadata(0, 0),
                )?
                .tree)
        }
        Opcode::IndexDocument if mutation.expires_at_micros.is_none() => {
            index_document_in_search_tree(
                pages,
                tree,
                creating_csn,
                index,
                mutation,
                blob_references,
            )
        }
        _ => Err(NativeRuntimeError::InvalidSearchTree),
    }
}

fn index_document_in_search_tree(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    index: ObjectId,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let text =
        std::str::from_utf8(&mutation.value).map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
    validate_search_document_identity(&mutation.key, text)?;
    let metadata_key = search_index_meta_key(index);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    let (document_count, total_document_terms) = decode_search_index_metadata(&metadata)?;
    let tokens = analyze(text);
    let token_count =
        u64::try_from(tokens.len()).map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
    tree = tree
        .insert_unique(
            pages,
            creating_csn,
            search_document_key(index, &mutation.key)?,
            search_document_storage_value(&mutation.value, token_count, blob_references)?,
        )?
        .tree;
    let mut frequencies = BTreeMap::<String, u32>::new();
    for token in tokens {
        let frequency = frequencies.entry(token).or_default();
        *frequency = frequency
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    }
    for (term, term_frequency) in frequencies {
        tree = insert_search_posting(
            pages,
            tree,
            creating_csn,
            index,
            term.as_bytes(),
            &mutation.key,
            term_frequency,
        )?;
    }
    Ok(tree
        .upsert(
            pages,
            creating_csn,
            metadata_key,
            encode_search_index_metadata(
                document_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?,
                total_document_terms
                    .checked_add(token_count)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?,
            ),
        )?
        .tree)
}

fn insert_search_posting(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    index: ObjectId,
    term: &[u8],
    document_id: &[u8],
    term_frequency: u32,
) -> Result<BTree, NativeRuntimeError> {
    let term_key = search_term_meta_key(index, term)?;
    let document_frequency = tree
        .get(pages, &term_key)?
        .map(|encoded| decode_search_term_metadata(&encoded))
        .transpose()?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    tree = tree
        .upsert(
            pages,
            creating_csn,
            term_key,
            encode_search_term_metadata(document_frequency),
        )?
        .tree;
    Ok(tree
        .insert_unique(
            pages,
            creating_csn,
            search_posting_key(index, term, document_id)?,
            encode_search_posting(term_frequency),
        )?
        .tree)
}

struct SearchMutationContext<'a> {
    format: SearchFormat,
    catalog: &'a CatalogState,
    state: &'a SearchState,
    mutations: &'a [Mutation],
    blob_references: &'a BTreeMap<[u8; 32], BlobReference>,
}

fn search_root_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    context: &SearchMutationContext<'_>,
) -> Result<Option<PageId>, NativeRuntimeError> {
    match context.format {
        SearchFormat::InlineStateV1 => {
            if context.mutations.iter().any(|mutation| {
                matches!(
                    mutation.opcode,
                    Opcode::CreateAnnIndex | Opcode::UpsertVector | Opcode::DeleteVector
                )
            }) {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            Ok(Some(pages.append(
                PageKind::SearchDelta,
                Some(creating_csn),
                None,
                context.state.encode()?,
            )?))
        }
        SearchFormat::InvertedBTreeV1 => Ok(search_tree_after_mutations(
            pages,
            root,
            creating_csn,
            context.catalog,
            context.mutations,
            context.blob_references,
        )?
        .root()),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn relational_tree_after_mutations(
    pages: &mut PageStore,
    blobs: &BlobStore,
    root: Option<PageId>,
    format: RelationalFormat,
    creating_csn: Csn,
    catalog: &CatalogState,
    relational: &RelationState,
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let mut tree = root.map_or_else(BTree::empty, BTree::from_root);
    if tree.root().is_none() {
        tree = tree
            .insert_unique(
                pages,
                creating_csn,
                RELATIONAL_FORMAT_KEY.to_vec(),
                format.marker().to_vec(),
            )?
            .tree;
    }
    for mutation in mutations
        .iter()
        .filter(|mutation| mutation.engine == EngineKind::Relational)
    {
        let target = mutation
            .target
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        let key = match mutation.opcode {
            Opcode::CreateTable => {
                tree = tree
                    .insert_unique(
                        pages,
                        creating_csn,
                        relational_table_key(target),
                        Vec::new(),
                    )?
                    .tree;
                continue;
            }
            Opcode::CreateSecondaryIndex => {
                let object = decode_secondary_index_creation(target, mutation)
                    .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?;
                let CatalogObject::SecondaryIndex(definition) = object else {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                };
                tree = tree
                    .insert_unique(
                        pages,
                        creating_csn,
                        relational_secondary_index_key(target),
                        encode_secondary_index_metadata(&definition).to_vec(),
                    )?
                    .tree;
                let index_state = relational
                    .indexes
                    .get(&target)
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                for (index_key, primary_keys) in &index_state.entries {
                    for primary_key in primary_keys {
                        let identity = secondary_index_entry_identity(index_key, primary_key)?;
                        tree = tree
                            .upsert(
                                pages,
                                creating_csn,
                                relational_secondary_entry_key(target, &identity)?,
                                vec![RELATIONAL_SECONDARY_ENTRY_LIVE],
                            )?
                            .tree;
                    }
                }
                continue;
            }
            Opcode::InsertRow | Opcode::UpdateRow | Opcode::DeleteRow => {
                relational_row_key(target, &mutation.key)
            }
            _ => return Err(NativeRuntimeError::InvalidRelationalTree),
        };
        let old_projections = if mutation.opcode == Opcode::InsertRow {
            Vec::new()
        } else {
            let old_row = relational_tree_row(
                pages,
                blobs,
                tree,
                format,
                target,
                &mutation.key,
                Some(creating_csn),
            )?
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
            secondary_index_projections(catalog, target, &old_row)?
        };
        let row = if mutation.opcode == Opcode::DeleteRow {
            RowRecord::tombstone(
                relational_row_id(target, &mutation.key)?,
                creating_csn,
                None,
            )?
        } else {
            RowRecord::new(
                relational_row_id(target, &mutation.key)?,
                creating_csn,
                None,
                vec![
                    Some(mutation.key.clone()),
                    Some(relational_storage_value(&mutation.value, blob_references)?),
                ],
            )?
        };
        let value = match format {
            RelationalFormat::InlineRowV1 => row.encode()?,
            RelationalFormat::VersionChainV2 => {
                let previous = tree.get(pages, &key)?;
                append_row_version(pages, previous.as_deref(), &row, creating_csn)?
            }
        };
        tree = tree.upsert(pages, creating_csn, key, value)?.tree;
        tree = write_secondary_index_projection_markers(
            pages,
            tree,
            creating_csn,
            &old_projections,
            &mutation.key,
            RELATIONAL_SECONDARY_ENTRY_TOMBSTONE,
        )?;
        if mutation.opcode != Opcode::DeleteRow {
            let new_projections = secondary_index_projections(catalog, target, &mutation.value)?;
            tree = write_secondary_index_projection_markers(
                pages,
                tree,
                creating_csn,
                &new_projections,
                &mutation.key,
                RELATIONAL_SECONDARY_ENTRY_LIVE,
            )?;
        }
    }
    Ok(tree)
}

fn relational_tree_row(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    format: RelationalFormat,
    table: ObjectId,
    primary_key: &[u8],
    visible_csn: Option<Csn>,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let Some(encoded) = tree.get(pages, &relational_row_key(table, primary_key))? else {
        return Ok(None);
    };
    match format {
        RelationalFormat::InlineRowV1 => {
            decode_relational_row(table, primary_key, &encoded, visible_csn, blobs)
        }
        RelationalFormat::VersionChainV2 => {
            decode_relational_chain(pages, table, primary_key, &encoded, visible_csn, blobs)
        }
    }
}

fn write_secondary_index_projection_markers(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    projections: &[SecondaryIndexProjection],
    primary_key: &[u8],
    marker: u8,
) -> Result<BTree, NativeRuntimeError> {
    if !matches!(
        marker,
        RELATIONAL_SECONDARY_ENTRY_TOMBSTONE | RELATIONAL_SECONDARY_ENTRY_LIVE
    ) {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    for projection in projections {
        let identity = secondary_index_entry_identity(&projection.key, primary_key)?;
        tree = tree
            .upsert(
                pages,
                creating_csn,
                relational_secondary_entry_key(projection.index, &identity)?,
                vec![marker],
            )?
            .tree;
    }
    Ok(tree)
}

fn append_row_version(
    pages: &mut PageStore,
    previous_pointer: Option<&[u8]>,
    row: &RowRecord,
    creating_csn: Csn,
) -> Result<Vec<u8>, NativeRuntimeError> {
    if row.begin_csn() != creating_csn || row.end_csn().is_some() {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let next = if let Some(previous_pointer) = previous_pointer {
        let pointer = RowVersionPointer::decode(previous_pointer)?;
        let previous_page = pages.read(pointer.page_id)?;
        if previous_page.kind() != PageKind::VersionChain
            || previous_page
                .creating_csn()
                .is_none_or(|created| created > creating_csn)
        {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        let previous = RowRecord::decode(previous_page.payload())?;
        if previous.row_id() != row.row_id() || previous.end_csn().is_some() {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        if previous.begin_csn() == creating_csn {
            previous_page.next()
        } else {
            Some(pages.append(
                PageKind::VersionChain,
                Some(creating_csn),
                previous_page.next(),
                previous.close_at(creating_csn)?.encode()?,
            )?)
        }
    } else {
        None
    };
    let latest = pages.append(
        PageKind::VersionChain,
        Some(creating_csn),
        next,
        row.encode()?,
    )?;
    Ok(RowVersionPointer { page_id: latest }.encode().to_vec())
}

fn relational_table_key(table: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(RELATIONAL_TABLE_PREFIX);
    key.extend_from_slice(&table.get().to_be_bytes());
    key
}

fn relational_row_key(table: ObjectId, primary_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(17 + primary_key.len());
    key.push(RELATIONAL_ROW_PREFIX);
    key.extend_from_slice(&table.get().to_be_bytes());
    key.extend_from_slice(primary_key);
    key
}

fn map_primary_key_bound(table: ObjectId, bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(primary_key) => Bound::Included(relational_row_key(table, primary_key)),
        Bound::Excluded(primary_key) => Bound::Excluded(relational_row_key(table, primary_key)),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn bound_as_slice(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(key) => Bound::Included(key.as_slice()),
        Bound::Excluded(key) => Bound::Excluded(key.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecondaryIndexProjection {
    index: ObjectId,
    key: Vec<u8>,
    contains_null: bool,
}

fn insert_secondary_index_projections(
    relational: &mut RelationState,
    projections: &[SecondaryIndexProjection],
    primary_key: &[u8],
) -> Result<(), NativeRuntimeError> {
    for projection in projections {
        relational.insert_secondary_index(
            projection.index,
            projection.key.clone(),
            primary_key.to_vec(),
            projection.contains_null,
        )?;
    }
    Ok(())
}

fn remove_secondary_index_projections(
    relational: &mut RelationState,
    projections: &[SecondaryIndexProjection],
    primary_key: &[u8],
) -> Result<(), NativeRuntimeError> {
    for projection in projections {
        relational.remove_secondary_index(projection.index, &projection.key, primary_key)?;
    }
    Ok(())
}

fn secondary_index_projections(
    catalog: &CatalogState,
    relation: ObjectId,
    row: &[u8],
) -> Result<Vec<SecondaryIndexProjection>, NativeRuntimeError> {
    catalog
        .objects
        .values()
        .filter_map(|object| match object {
            CatalogObject::SecondaryIndex(definition) if definition.relation == relation => {
                Some(definition)
            }
            CatalogObject::Relation(_)
            | CatalogObject::SecondaryIndex(_)
            | CatalogObject::Structure(_)
            | CatalogObject::Search(_) => None,
        })
        .map(|definition| secondary_index_projection(catalog, definition, row))
        .collect()
}

fn secondary_index_projection(
    catalog: &CatalogState,
    definition: &SecondaryIndexDefinition,
    row: &[u8],
) -> Result<SecondaryIndexProjection, NativeRuntimeError> {
    let Some(CatalogObject::Relation(relation)) = catalog.object(definition.relation) else {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    };
    let tuple = RowTupleView::decode(row)?;
    if tuple.column_count() != relation.columns.len() {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let mut key = Vec::new();
    let mut contains_null = false;
    for column_id in &definition.columns {
        let index = relation
            .columns
            .iter()
            .position(|column| column.id == *column_id)
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        let column = &relation.columns[index];
        let value = match tuple
            .value(index)
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?
        {
            ColumnValueRef::Null => {
                contains_null = true;
                ScalarValue::Null
            }
            ColumnValueRef::Bytes(encoded) => {
                ScalarValue::decode_storage(&column.logical_type, encoded)
                    .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?
            }
        };
        key.extend_from_slice(
            &value
                .encode_ordered_component(&column.logical_type)
                .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?,
        );
    }
    Ok(SecondaryIndexProjection {
        index: definition.header.id,
        key,
        contains_null,
    })
}

fn secondary_index_entry_identity(
    index_key: &[u8],
    primary_key: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    let index_length =
        u32::try_from(index_key.len()).map_err(|_| NativeRuntimeError::InvalidRelationalTree)?;
    let capacity = 4_usize
        .checked_add(index_key.len())
        .and_then(|length| length.checked_add(primary_key.len()))
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    if capacity
        .checked_add(17)
        .is_none_or(|physical| physical > BTREE_MAX_KEY_SIZE)
    {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&index_length.to_be_bytes());
    encoded.extend_from_slice(index_key);
    encoded.extend_from_slice(primary_key);
    Ok(encoded)
}

fn decode_secondary_index_entry_identity(
    encoded: &[u8],
) -> Result<(&[u8], &[u8]), NativeRuntimeError> {
    let length = encoded
        .get(..4)
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    let index_length = usize::try_from(u32::from_be_bytes(
        length
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?,
    ))
    .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?;
    let index_end = 4_usize
        .checked_add(index_length)
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    let index_key = encoded
        .get(4..index_end)
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    let primary_key = encoded
        .get(index_end..)
        .filter(|key| !key.is_empty())
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    Ok((index_key, primary_key))
}

fn relational_secondary_index_key(index: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(RELATIONAL_SECONDARY_INDEX_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key
}

fn relational_secondary_entry_key(
    index: ObjectId,
    identity: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    let capacity = 17_usize
        .checked_add(identity.len())
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    if capacity > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let mut key = Vec::with_capacity(capacity);
    key.push(RELATIONAL_SECONDARY_ENTRY_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key.extend_from_slice(identity);
    Ok(key)
}

fn relational_secondary_entry_prefix(
    index: ObjectId,
    index_key: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    let index_length =
        u32::try_from(index_key.len()).map_err(|_| NativeRuntimeError::InvalidRelationalTree)?;
    let capacity = 21_usize
        .checked_add(index_key.len())
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    if capacity >= BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let mut prefix = Vec::with_capacity(capacity);
    prefix.push(RELATIONAL_SECONDARY_ENTRY_PREFIX);
    prefix.extend_from_slice(&index.get().to_be_bytes());
    prefix.extend_from_slice(&index_length.to_be_bytes());
    prefix.extend_from_slice(index_key);
    Ok(prefix)
}

fn encode_secondary_index_metadata(
    definition: &SecondaryIndexDefinition,
) -> [u8; RELATIONAL_SECONDARY_INDEX_META_SIZE] {
    let mut encoded = [0_u8; RELATIONAL_SECONDARY_INDEX_META_SIZE];
    encoded[..8].copy_from_slice(RELATIONAL_SECONDARY_INDEX_MAGIC);
    encoded[8..24].copy_from_slice(&definition.relation.get().to_be_bytes());
    encoded[24] = u8::from(definition.unique);
    encoded[25] = u8::from(definition.nulls_distinct);
    encoded
}

fn decode_secondary_index_metadata(
    encoded: &[u8],
) -> Result<(ObjectId, bool, bool), NativeRuntimeError> {
    if encoded.len() != RELATIONAL_SECONDARY_INDEX_META_SIZE
        || encoded.get(..8) != Some(RELATIONAL_SECONDARY_INDEX_MAGIC.as_slice())
        || !matches!(encoded[24], 0 | 1)
        || !matches!(encoded[25], 0 | 1)
        || encoded[26..].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let relation = ObjectId::new(u128::from_be_bytes(
        encoded[8..24]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?,
    ))
    .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?;
    Ok((relation, encoded[24] == 1, encoded[25] == 1))
}

fn relational_row_id(table: ObjectId, primary_key: &[u8]) -> Result<RowId, NativeRuntimeError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-relational-row-id-v1");
    hasher.update(&table.get().to_be_bytes());
    hasher.update(
        &u64::try_from(primary_key.len())
            .map_err(|_| NativeRuntimeError::InvalidRelationalTree)?
            .to_be_bytes(),
    );
    hasher.update(primary_key);
    let digest = hasher.finalize();
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&digest.as_bytes()[..16]);
    let mut value = u128::from_be_bytes(encoded);
    if value == 0 {
        value = 1;
    }
    RowId::new(value).map_err(|_| NativeRuntimeError::InvalidRelationalTree)
}

fn decode_relational_row(
    table: ObjectId,
    primary_key: &[u8],
    encoded: &[u8],
    visible_csn: Option<Csn>,
    blobs: &BlobStore,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let row = RowRecordView::decode(encoded)?;
    if row.row_id() != relational_row_id(table, primary_key)? {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    if !row.is_visible_at(visible_csn) {
        return Ok(None);
    }
    decode_relational_row_value(row, primary_key, blobs)
}

fn decode_relational_row_value(
    row: RowRecordView<'_>,
    primary_key: &[u8],
    blobs: &BlobStore,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    if row.is_tombstone() {
        return Ok(None);
    }
    if row.column_count() != 2 {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    match (row.value(0), row.value(1)) {
        (
            Some(ColumnValueRef::Bytes(encoded_primary_key)),
            Some(ColumnValueRef::Bytes(stored_value)),
        ) if encoded_primary_key == primary_key => {
            let Some((&storage, value)) = stored_value.split_first() else {
                return Err(NativeRuntimeError::InvalidRelationalTree);
            };
            match storage {
                RELATIONAL_VALUE_INLINE => Ok(Some(value.to_vec())),
                RELATIONAL_VALUE_BLOB => {
                    let reference = BlobReference::decode(value)?;
                    Ok(Some(blobs.read(reference)?))
                }
                _ => Err(NativeRuntimeError::InvalidRelationalTree),
            }
        }
        _ => Err(NativeRuntimeError::InvalidRelationalTree),
    }
}

struct RelationalReadContext<'a> {
    pages: &'a PageStore,
    pool: &'a BufferPool,
    blobs: &'a BlobStore,
    format: RelationalFormat,
    visible_csn: Option<Csn>,
}

fn decode_relational_value_cached(
    context: &RelationalReadContext<'_>,
    table: ObjectId,
    primary_key: &[u8],
    encoded: &[u8],
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    match context.format {
        RelationalFormat::InlineRowV1 => decode_relational_row(
            table,
            primary_key,
            encoded,
            context.visible_csn,
            context.blobs,
        ),
        RelationalFormat::VersionChainV2 => {
            let mut page_id = RowVersionPointer::decode(encoded)?.page_id;
            let mut stack_visited = [0_u64; 64];
            let mut overflow_visited = None;
            let mut depth = 0_usize;
            let mut newer_begin = None;
            loop {
                track_version_page(page_id, depth, &mut stack_visited, &mut overflow_visited)?;
                let frame = context.pool.get_or_load(context.pages, page_id)?;
                let page = frame.page();
                if page.kind() != PageKind::VersionChain {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
                let row = RowRecordView::decode(page.payload())?;
                validate_version_page(
                    page,
                    row,
                    table,
                    primary_key,
                    context.visible_csn,
                    newer_begin,
                )?;
                if row.is_visible_at(context.visible_csn) {
                    return decode_relational_row_value(row, primary_key, context.blobs);
                }
                newer_begin = Some(row.begin_csn());
                page_id = page
                    .next()
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                depth = depth
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
            }
        }
    }
}

fn track_version_page(
    page_id: PageId,
    depth: usize,
    stack_visited: &mut [u64; 64],
    overflow_visited: &mut Option<BTreeSet<u64>>,
) -> Result<(), NativeRuntimeError> {
    if depth < stack_visited.len() {
        if stack_visited[..depth].contains(&page_id.get()) {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        stack_visited[depth] = page_id.get();
        return Ok(());
    }
    let visited = overflow_visited.get_or_insert_with(|| stack_visited.iter().copied().collect());
    if !visited.insert(page_id.get()) {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    Ok(())
}

fn validate_version_page(
    page: &hyphae_native_pages::Page,
    row: RowRecordView<'_>,
    table: ObjectId,
    primary_key: &[u8],
    visible_csn: Option<Csn>,
    newer_begin: Option<Csn>,
) -> Result<(), NativeRuntimeError> {
    let created = page
        .creating_csn()
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    if visible_csn.is_none_or(|visible| created > visible)
        || row.row_id() != relational_row_id(table, primary_key)?
    {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    match newer_begin {
        None if row.end_csn().is_some() || created != row.begin_csn() => {
            Err(NativeRuntimeError::InvalidRelationalTree)
        }
        Some(newer) if row.end_csn() != Some(newer) || created != newer => {
            Err(NativeRuntimeError::InvalidRelationalTree)
        }
        _ => Ok(()),
    }
}

fn decode_relational_chain(
    pages: &PageStore,
    table: ObjectId,
    primary_key: &[u8],
    encoded: &[u8],
    visible_csn: Option<Csn>,
    blobs: &BlobStore,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let mut page_id = RowVersionPointer::decode(encoded)?.page_id;
    let mut visited = BTreeSet::new();
    let mut newer_begin = None;
    let mut selected = None;
    loop {
        if !visited.insert(page_id) {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        let page = pages.read(page_id)?;
        if page.kind() != PageKind::VersionChain {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        let row = RowRecordView::decode(page.payload())?;
        validate_version_page(&page, row, table, primary_key, visible_csn, newer_begin)?;
        let value = decode_relational_row_value(row, primary_key, blobs)?;
        if selected.is_none() && row.is_visible_at(visible_csn) {
            selected = Some(value);
        }
        newer_begin = Some(row.begin_csn());
        let Some(next) = page.next() else {
            break;
        };
        page_id = next;
    }
    selected.ok_or(NativeRuntimeError::InvalidRelationalTree)
}

fn decode_relational_table(key: &[u8], prefix: u8) -> Result<ObjectId, NativeRuntimeError> {
    if key.len() < 17 || key[0] != prefix {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&key[1..17]);
    ObjectId::new(u128::from_be_bytes(encoded))
        .map_err(|_| NativeRuntimeError::InvalidRelationalTree)
}

fn roots_from_snapshot(root_set: &RootSet) -> [Option<PageId>; 4] {
    ROOT_SLOTS.map(|slot| root_set.root(slot))
}

fn require_roots(roots: [Option<PageId>; 4]) -> Result<[PageId; 4], NativeRuntimeError> {
    let [
        Some(catalog),
        Some(relational),
        Some(structures),
        Some(search),
    ] = roots
    else {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    };
    Ok([catalog, relational, structures, search])
}

fn root_map(roots: [PageId; 4]) -> BTreeMap<RootSlot, PageId> {
    ROOT_SLOTS.into_iter().zip(roots).collect()
}

fn validate_commit_sequence(
    commits: &[wal_codec::RecoveredCommit],
) -> Result<(), NativeRuntimeError> {
    let mut expected = 1_u64;
    let mut prior = None;
    for commit in commits {
        if commit.manifest.commit_csn.get() != expected
            || commit
                .manifest
                .read_csn
                .is_some_and(|read| prior.is_none_or(|published| read > published))
        {
            return Err(NativeRuntimeError::NoncontiguousCommitSequence);
        }
        prior = Some(commit.manifest.commit_csn);
        expected = expected
            .checked_add(1)
            .ok_or(NativeRuntimeError::NoncontiguousCommitSequence)?;
    }
    Ok(())
}

fn digest_for_lsn(
    recovery: &WalRecovery,
    lsn: hyphae_native_types::Lsn,
) -> Result<[u8; 32], NativeRuntimeError> {
    recovery
        .blocks
        .iter()
        .find(|block| block.first_lsn <= lsn && lsn <= block.last_lsn)
        .map(|block| block.digest)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)
}

fn validate_roots(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
    visible_csn: Csn,
) -> Result<(), NativeRuntimeError> {
    let catalog_root = roots
        .root(SLOT_CATALOG)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let catalog_page = pages.read(catalog_root)?;
    if catalog_page.kind() != PageKind::CatalogRoot {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    }
    if catalog_page
        .creating_csn()
        .is_none_or(|creating| creating > visible_csn)
    {
        return Err(NativeRuntimeError::FuturePage);
    }
    let relational_root = roots
        .root(SLOT_RELATIONAL)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let relational_page = pages.read(relational_root)?;
    if !matches!(
        relational_page.kind(),
        PageKind::BTreeLeaf | PageKind::BTreeInternal
    ) {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    }
    if relational_page
        .creating_csn()
        .is_none_or(|creating| creating > visible_csn)
    {
        return Err(NativeRuntimeError::FuturePage);
    }
    BTree::from_root(relational_root).validate_visible(pages, visible_csn)?;

    let structure_root = roots
        .root(SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let structure_page = pages.read(structure_root)?;
    if !matches!(
        structure_page.kind(),
        PageKind::StructureNode | PageKind::BTreeLeaf | PageKind::BTreeInternal
    ) {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    }
    if structure_page
        .creating_csn()
        .is_none_or(|creating| creating > visible_csn)
    {
        return Err(NativeRuntimeError::FuturePage);
    }
    if matches!(
        structure_page.kind(),
        PageKind::BTreeLeaf | PageKind::BTreeInternal
    ) {
        BTree::from_root(structure_root).validate_visible(pages, visible_csn)?;
    }
    let search_root = roots
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let search_page = pages.read(search_root)?;
    if !matches!(
        search_page.kind(),
        PageKind::SearchDelta | PageKind::BTreeLeaf | PageKind::BTreeInternal
    ) {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    }
    if search_page
        .creating_csn()
        .is_none_or(|creating| creating > visible_csn)
    {
        return Err(NativeRuntimeError::FuturePage);
    }
    if matches!(
        search_page.kind(),
        PageKind::BTreeLeaf | PageKind::BTreeInternal
    ) {
        BTree::from_root(search_root).validate_visible(pages, visible_csn)?;
    }
    load_state(pages, blobs, roots)?;
    Ok(())
}

fn load_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
) -> Result<MaterializedState, NativeRuntimeError> {
    let catalog = load_catalog_state(pages, roots)?;
    let relational = load_relational_state(pages, blobs, roots, &catalog)?;
    let search = load_search_state(pages, blobs, roots)?;
    let ann = ann_store::load(pages, roots.root(SLOT_SEARCH), &catalog)?;
    Ok(MaterializedState {
        catalog,
        relational,
        structures: load_structure_state(pages, blobs, roots)?,
        search,
        ann,
    })
}

fn load_catalog_state(
    pages: &PageStore,
    roots: &RootSet,
) -> Result<CatalogState, NativeRuntimeError> {
    Ok(load_root(
        pages,
        roots,
        SLOT_CATALOG,
        PageKind::CatalogRoot,
        CatalogState::decode,
    )?
    .unwrap_or_default())
}

#[allow(clippy::too_many_lines)]
fn load_relational_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
    catalog: &CatalogState,
) -> Result<RelationState, NativeRuntimeError> {
    let Some(root) = roots.root(SLOT_RELATIONAL) else {
        return Ok(RelationState::default());
    };
    let entries = BTree::from_root(root).scan(pages)?;
    let mut iterator = entries.into_iter();
    let Some((format_key, format_value)) = iterator.next() else {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    };
    if format_key != RELATIONAL_FORMAT_KEY {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    let format = RelationalFormat::decode(&format_value)?;
    let mut tables = BTreeMap::new();
    let mut indexes = BTreeMap::new();
    for (key, value) in iterator {
        match key.first().copied() {
            Some(RELATIONAL_TABLE_PREFIX) if key.len() == 17 && value.is_empty() => {
                let table = decode_relational_table(&key, RELATIONAL_TABLE_PREFIX)?;
                if tables.insert(table, BTreeMap::new()).is_some() {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
            }
            Some(RELATIONAL_ROW_PREFIX) if key.len() >= 17 => {
                let table = decode_relational_table(&key, RELATIONAL_ROW_PREFIX)?;
                let rows = tables
                    .get_mut(&table)
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                let primary_key = &key[17..];
                let decoded = match format {
                    RelationalFormat::InlineRowV1 => decode_relational_row(
                        table,
                        primary_key,
                        &value,
                        roots.visible_csn(),
                        blobs,
                    )?,
                    RelationalFormat::VersionChainV2 => decode_relational_chain(
                        pages,
                        table,
                        primary_key,
                        &value,
                        roots.visible_csn(),
                        blobs,
                    )?,
                };
                let Some(value) = decoded else {
                    continue;
                };
                if rows.insert(primary_key.to_vec(), value).is_some() {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
            }
            Some(RELATIONAL_SECONDARY_INDEX_PREFIX) if key.len() == 17 => {
                let index = decode_relational_table(&key, RELATIONAL_SECONDARY_INDEX_PREFIX)?;
                let (relation, unique, nulls_distinct) = decode_secondary_index_metadata(&value)?;
                let Some(CatalogObject::SecondaryIndex(definition)) = catalog.object(index) else {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                };
                if definition.relation != relation
                    || definition.unique != unique
                    || definition.nulls_distinct != nulls_distinct
                    || !tables.contains_key(&relation)
                    || indexes
                        .insert(
                            index,
                            model::SecondaryIndexState {
                                relation,
                                unique,
                                nulls_distinct,
                                entries: BTreeMap::new(),
                            },
                        )
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
            }
            Some(RELATIONAL_SECONDARY_ENTRY_PREFIX) if key.len() > 21 => {
                let index = decode_relational_table(&key, RELATIONAL_SECONDARY_ENTRY_PREFIX)?;
                let (index_key, primary_key) = decode_secondary_index_entry_identity(&key[17..])?;
                let marker = match value.as_slice() {
                    [RELATIONAL_SECONDARY_ENTRY_TOMBSTONE] => false,
                    [RELATIONAL_SECONDARY_ENTRY_LIVE] => true,
                    _ => return Err(NativeRuntimeError::InvalidRelationalTree),
                };
                let index_state = indexes
                    .get(&index)
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                if !marker {
                    continue;
                }
                let row = tables
                    .get(&index_state.relation)
                    .and_then(|rows| rows.get(primary_key))
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                let Some(CatalogObject::SecondaryIndex(definition)) = catalog.object(index) else {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                };
                let projection = secondary_index_projection(catalog, definition, row)?;
                if projection.key != index_key {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
                let index_state = indexes
                    .get_mut(&index)
                    .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
                let existing = index_state
                    .entries
                    .entry(index_key.to_vec())
                    .or_insert_with(BTreeSet::new);
                if !existing.insert(primary_key.to_vec())
                    || (index_state.unique
                        && !(projection.contains_null && index_state.nulls_distinct)
                        && existing.len() != 1)
                {
                    return Err(NativeRuntimeError::InvalidRelationalTree);
                }
            }
            _ => return Err(NativeRuntimeError::InvalidRelationalTree),
        }
    }
    let state = RelationState { tables, indexes };
    validate_secondary_index_projection(catalog, &state)?;
    Ok(state)
}

fn validate_secondary_index_projection(
    catalog: &CatalogState,
    relational: &RelationState,
) -> Result<(), NativeRuntimeError> {
    for object in catalog.objects.values() {
        let CatalogObject::SecondaryIndex(definition) = object else {
            continue;
        };
        let index = relational
            .indexes
            .get(&definition.header.id)
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        if index.relation != definition.relation
            || index.unique != definition.unique
            || index.nulls_distinct != definition.nulls_distinct
        {
            return Err(NativeRuntimeError::InvalidRelationalTree);
        }
        let rows = relational
            .tables
            .get(&definition.relation)
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        for (primary_key, row) in rows {
            let projection = secondary_index_projection(catalog, definition, row)?;
            if !index
                .entries
                .get(&projection.key)
                .is_some_and(|entries| entries.contains(primary_key))
            {
                return Err(NativeRuntimeError::InvalidRelationalTree);
            }
        }
    }
    Ok(())
}

fn formats_for_latest_root(
    pages: &PageStore,
    root: Option<&RootSet>,
) -> Result<(RelationalFormat, StructureFormat, SearchFormat), NativeRuntimeError> {
    root.map_or(
        Ok((
            RelationalFormat::VersionChainV2,
            StructureFormat::BTreeV1,
            SearchFormat::InvertedBTreeV1,
        )),
        |root| {
            Ok((
                relational_format_for_root(pages, root)?,
                structure_format_for_root(pages, root)?,
                search_format_for_root(pages, root)?,
            ))
        },
    )
}

fn relational_format_for_root(
    pages: &PageStore,
    roots: &RootSet,
) -> Result<RelationalFormat, NativeRuntimeError> {
    let root = roots
        .root(SLOT_RELATIONAL)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let marker = BTree::from_root(root)
        .get(pages, RELATIONAL_FORMAT_KEY)?
        .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
    RelationalFormat::decode(&marker)
}

fn load_structure_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
) -> Result<StructureState, NativeRuntimeError> {
    let Some(root) = roots.root(SLOT_STRUCTURE) else {
        return Ok(StructureState::default());
    };
    let page = pages.read(root)?;
    if page.kind() == PageKind::StructureNode {
        return Ok(StructureState::decode(page.payload())?);
    }
    if !matches!(page.kind(), PageKind::BTreeLeaf | PageKind::BTreeInternal) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let entries = BTree::from_root(root).scan(pages)?;
    let mut iterator = entries.into_iter();
    let Some((format_key, format_value)) = iterator.next() else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if format_key != STRUCTURE_FORMAT_KEY || format_value != STRUCTURE_FORMAT_VALUE_V1 {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut decoded = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    let mut hash_counts = BTreeMap::new();
    let mut sets = BTreeMap::new();
    let mut set_counts = BTreeMap::new();
    for (key, value) in iterator {
        match key.first().copied() {
            Some(STRUCTURE_ENTRY_PREFIX) => {
                if let Some(entry) = decode_structure_value(&value, blobs)?
                    && decoded.insert(key[1..].to_vec(), entry).is_some()
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Some(STRUCTURE_HASH_META_PREFIX) => {
                let hash = key[1..].to_vec();
                if decoded.contains_key(&hash)
                    || hashes.insert(hash.clone(), BTreeMap::new()).is_some()
                    || hash_counts
                        .insert(hash, decode_hash_metadata(&value)?)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Some(STRUCTURE_HASH_FIELD_PREFIX) => {
                let (hash, field) = decode_hash_field_identity(&key[1..])
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
                let fields = hashes
                    .get_mut(hash)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if let Some(value) = decode_hash_field_value(&value, blobs)?
                    && fields.insert(field.to_vec(), value).is_some()
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Some(STRUCTURE_SET_META_PREFIX) => {
                let set = key[1..].to_vec();
                if decoded.contains_key(&set)
                    || hashes.contains_key(&set)
                    || sets.insert(set.clone(), BTreeSet::new()).is_some()
                    || set_counts
                        .insert(set, decode_set_metadata(&value)?)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Some(STRUCTURE_SET_MEMBER_PREFIX) => {
                let (set, member) = decode_set_member_identity(&key[1..])
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
                let members = sets
                    .get_mut(set)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if decode_set_member_value(&value)? && !members.insert(member.to_vec()) {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            _ => return Err(NativeRuntimeError::InvalidStructureTree),
        }
    }
    validate_hash_counts(&hashes, hash_counts)?;
    validate_set_counts(&sets, set_counts)?;
    Ok(StructureState {
        entries: decoded,
        hashes,
        sets,
    })
}

fn validate_hash_counts(
    hashes: &BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>,
    expected_counts: BTreeMap<Vec<u8>, u64>,
) -> Result<(), NativeRuntimeError> {
    for (key, expected) in expected_counts {
        let actual = hashes
            .get(&key)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
            .len();
        if u64::try_from(actual).map_err(|_| NativeRuntimeError::InvalidStructureTree)? != expected
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(())
}

fn validate_set_counts(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    expected_counts: BTreeMap<Vec<u8>, u64>,
) -> Result<(), NativeRuntimeError> {
    for (key, expected) in expected_counts {
        let actual = sets
            .get(&key)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
            .len();
        if u64::try_from(actual).map_err(|_| NativeRuntimeError::InvalidStructureTree)? != expected
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(())
}

fn load_search_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
) -> Result<SearchState, NativeRuntimeError> {
    let Some(root) = roots.root(SLOT_SEARCH) else {
        return Ok(SearchState::default());
    };
    let page = pages.read(root)?;
    if page.kind() == PageKind::SearchDelta {
        return Ok(SearchState::decode(page.payload())?);
    }
    if !matches!(page.kind(), PageKind::BTreeLeaf | PageKind::BTreeInternal) {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let entries = BTree::from_root(root).scan(pages)?;
    let mut iterator = entries.into_iter();
    let Some((format_key, format_value)) = iterator.next() else {
        return Err(NativeRuntimeError::InvalidSearchTree);
    };
    if format_key != SEARCH_FORMAT_KEY || format_value != SEARCH_FORMAT_VALUE_V1 {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    let mut indexes = BTreeMap::<ObjectId, BTreeMap<Vec<u8>, String>>::new();
    let mut index_metadata = BTreeMap::<ObjectId, (u64, u64)>::new();
    let mut term_metadata = BTreeMap::<(ObjectId, Vec<u8>), u64>::new();
    let mut postings = BTreeMap::<(ObjectId, Vec<u8>, Vec<u8>), u32>::new();
    for (key, value) in iterator {
        match key.first().copied() {
            Some(SEARCH_INDEX_META_PREFIX) if key.len() == 17 => {
                let (index, suffix) = decode_search_object_key(&key, SEARCH_INDEX_META_PREFIX)?;
                if !suffix.is_empty()
                    || indexes.insert(index, BTreeMap::new()).is_some()
                    || index_metadata
                        .insert(index, decode_search_index_metadata(&value)?)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
            Some(SEARCH_DOCUMENT_PREFIX) => {
                let (index, document_id) = decode_search_object_key(&key, SEARCH_DOCUMENT_PREFIX)?;
                let documents = indexes
                    .get_mut(&index)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                let (text, _) = decode_search_document(&value, blobs)?;
                validate_search_document_identity(document_id, &text)
                    .map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
                if documents.insert(document_id.to_vec(), text).is_some() {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
            Some(SEARCH_TERM_META_PREFIX) => {
                let (index, term) = decode_search_object_key(&key, SEARCH_TERM_META_PREFIX)?;
                if !indexes.contains_key(&index)
                    || !is_canonical_search_term(term)
                    || term_metadata
                        .insert((index, term.to_vec()), decode_search_term_metadata(&value)?)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
            Some(SEARCH_POSTING_PREFIX) => {
                let (index, term, document_id) = decode_search_posting_key(&key)?;
                if !indexes
                    .get(&index)
                    .is_some_and(|documents| documents.contains_key(document_id))
                    || !is_canonical_search_term(term)
                    || postings
                        .insert(
                            (index, term.to_vec(), document_id.to_vec()),
                            decode_search_posting(&value)?,
                        )
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
            _ if ann_store::is_ann_physical_key(&key) => {}
            _ => return Err(NativeRuntimeError::InvalidSearchTree),
        }
    }
    validate_search_projection(&indexes, &index_metadata, &term_metadata, &postings)?;
    Ok(SearchState { indexes })
}

fn validate_search_projection(
    indexes: &BTreeMap<ObjectId, BTreeMap<Vec<u8>, String>>,
    index_metadata: &BTreeMap<ObjectId, (u64, u64)>,
    term_metadata: &BTreeMap<(ObjectId, Vec<u8>), u64>,
    postings: &BTreeMap<(ObjectId, Vec<u8>, Vec<u8>), u32>,
) -> Result<(), NativeRuntimeError> {
    let mut expected_terms = BTreeMap::<(ObjectId, Vec<u8>), u64>::new();
    let mut expected_postings = BTreeMap::<(ObjectId, Vec<u8>, Vec<u8>), u32>::new();
    for (index, documents) in indexes {
        let (expected_document_count, expected_total_terms) = index_metadata
            .get(index)
            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
        let actual_document_count =
            u64::try_from(documents.len()).map_err(|_| NativeRuntimeError::InvalidSearchTree)?;
        let mut actual_total_terms = 0_u64;
        for (document_id, text) in documents {
            let tokens = analyze(text);
            actual_total_terms = actual_total_terms
                .checked_add(
                    u64::try_from(tokens.len())
                        .map_err(|_| NativeRuntimeError::InvalidSearchTree)?,
                )
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            let mut frequencies = BTreeMap::<Vec<u8>, u32>::new();
            for token in tokens {
                let frequency = frequencies.entry(token.into_bytes()).or_default();
                *frequency = frequency
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            }
            for (term, frequency) in frequencies {
                let document_frequency = expected_terms.entry((*index, term.clone())).or_default();
                *document_frequency = (*document_frequency)
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                if expected_postings
                    .insert((*index, term, document_id.clone()), frequency)
                    .is_some()
                {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
        }
        if *expected_document_count != actual_document_count
            || *expected_total_terms != actual_total_terms
        {
            return Err(NativeRuntimeError::InvalidSearchTree);
        }
    }
    if *term_metadata != expected_terms || *postings != expected_postings {
        return Err(NativeRuntimeError::InvalidSearchTree);
    }
    Ok(())
}

fn is_canonical_search_term(term: &[u8]) -> bool {
    std::str::from_utf8(term)
        .ok()
        .is_some_and(|term| analyze(term) == [term])
}

fn structure_format_for_root(
    pages: &PageStore,
    roots: &RootSet,
) -> Result<StructureFormat, NativeRuntimeError> {
    let root = roots
        .root(SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let page = pages.read(root)?;
    match page.kind() {
        PageKind::StructureNode => Ok(StructureFormat::InlineStateV1),
        PageKind::BTreeLeaf | PageKind::BTreeInternal => {
            let marker = BTree::from_root(root)
                .get(pages, STRUCTURE_FORMAT_KEY)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            if marker == STRUCTURE_FORMAT_VALUE_V1 {
                Ok(StructureFormat::BTreeV1)
            } else {
                Err(NativeRuntimeError::InvalidStructureTree)
            }
        }
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn search_format_for_root(
    pages: &PageStore,
    roots: &RootSet,
) -> Result<SearchFormat, NativeRuntimeError> {
    let root = roots
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let page = pages.read(root)?;
    match page.kind() {
        PageKind::SearchDelta => Ok(SearchFormat::InlineStateV1),
        PageKind::BTreeLeaf | PageKind::BTreeInternal => {
            let marker = BTree::from_root(root)
                .get(pages, SEARCH_FORMAT_KEY)?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            if marker == SEARCH_FORMAT_VALUE_V1 {
                Ok(SearchFormat::InvertedBTreeV1)
            } else {
                Err(NativeRuntimeError::InvalidSearchTree)
            }
        }
        _ => Err(NativeRuntimeError::InvalidSearchTree),
    }
}

fn load_root<T>(
    pages: &PageStore,
    roots: &RootSet,
    slot: RootSlot,
    expected_kind: PageKind,
    decode: impl FnOnce(&[u8]) -> Result<T, ModelError>,
) -> Result<Option<T>, NativeRuntimeError> {
    let Some(page_id) = roots.root(slot) else {
        return Ok(None);
    };
    let page = pages.read(page_id)?;
    if page.kind() != expected_kind {
        return Err(NativeRuntimeError::InvalidCommittedRoot);
    }
    Ok(Some(decode(page.payload())?))
}

fn decode_relation_creation(
    id: ObjectId,
    mutation: &Mutation,
) -> Result<CatalogObject, NativeRuntimeError> {
    let encoded_definition = mutation.value.starts_with(b"HYCOBJ01");
    let object = if encoded_definition {
        CatalogObject::decode_definition(&mutation.value)?
    } else {
        if !mutation.key.is_empty() {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let name = std::str::from_utf8(&mutation.value)
            .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
        CatalogObject::Relation(binary_relation_definition(id, name)?)
    };
    if !matches!(&object, CatalogObject::Relation(_)) || object.header().id != id {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    if encoded_definition && mutation.key.is_empty() {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    validate_catalog_creation_identity(&object, mutation)?;
    Ok(object)
}

fn decode_secondary_index_creation(
    id: ObjectId,
    mutation: &Mutation,
) -> Result<CatalogObject, NativeRuntimeError> {
    if mutation.key.is_empty()
        || mutation.expires_at_micros.is_some()
        || !mutation.value.starts_with(b"HYCOBJ01")
    {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let object = CatalogObject::decode_definition(&mutation.value)?;
    if !matches!(&object, CatalogObject::SecondaryIndex(_)) || object.header().id != id {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    validate_catalog_creation_identity(&object, mutation)?;
    Ok(object)
}

fn decode_search_creation(
    id: ObjectId,
    mutation: &Mutation,
) -> Result<CatalogObject, NativeRuntimeError> {
    let encoded_definition = mutation.value.starts_with(b"HYCOBJ01");
    let object = if encoded_definition {
        CatalogObject::decode_definition(&mutation.value)?
    } else {
        if !mutation.key.is_empty() {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let name = std::str::from_utf8(&mutation.value)
            .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
        CatalogObject::Search(text_search_definition(id, name)?)
    };
    if !matches!(&object, CatalogObject::Search(_)) || object.header().id != id {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    if encoded_definition && mutation.key.is_empty() {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    validate_catalog_creation_identity(&object, mutation)?;
    Ok(object)
}

fn decode_ann_creation(
    id: ObjectId,
    mutation: &Mutation,
) -> Result<CatalogObject, NativeRuntimeError> {
    if mutation.key.is_empty()
        || mutation.expires_at_micros.is_some()
        || !mutation.value.starts_with(b"HYCOBJ01")
    {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let object = CatalogObject::decode_definition(&mutation.value)?;
    let CatalogObject::Search(definition) = &object else {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    };
    if definition.header.id != id || definition.vector.is_none() || definition.ann.is_none() {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    validate_catalog_creation_identity(&object, mutation)?;
    Ok(object)
}

fn validate_catalog_creation_identity(
    object: &CatalogObject,
    mutation: &Mutation,
) -> Result<(), NativeRuntimeError> {
    if !mutation.key.is_empty() && mutation.key != catalog_name_identity(object.header())? {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(())
}

fn catalog_name_identity(header: &ObjectHeader) -> Result<Vec<u8>, CatalogError> {
    let mut encoded = Vec::new();
    for component in [
        &header.name.database,
        &header.name.schema,
        &header.name.object,
    ] {
        let lookup = component.lookup().as_bytes();
        let length = u32::try_from(lookup.len()).map_err(|_| CatalogError::NameTooLong)?;
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.extend_from_slice(lookup);
    }
    Ok(encoded)
}

fn binary_relation_definition(
    id: ObjectId,
    name: &str,
) -> Result<RelationDefinition, CatalogError> {
    let definition = RelationDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Relational,
            name: qualified_name(name)?,
        },
        columns: vec![
            ColumnDefinition {
                id: ColumnId::new(1).map_err(|_| CatalogError::EmptyName)?,
                name: CatalogName::unquoted("primary_key")?,
                logical_type: LogicalType::Binary,
                nullable: false,
            },
            ColumnDefinition {
                id: ColumnId::new(2).map_err(|_| CatalogError::EmptyName)?,
                name: CatalogName::unquoted("row")?,
                logical_type: LogicalType::Binary,
                nullable: false,
            },
        ],
        primary_key: vec![ColumnId::new(1).map_err(|_| CatalogError::EmptyName)?],
    };
    definition.validate()?;
    Ok(definition)
}

fn text_search_definition(
    id: ObjectId,
    name: &str,
) -> Result<SearchCollectionDefinition, CatalogError> {
    let definition = SearchCollectionDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Search,
            name: qualified_name(name)?,
        },
        fields: vec![SearchFieldDefinition {
            id: FieldId::new(1).map_err(|_| CatalogError::EmptyName)?,
            name: CatalogName::unquoted("text")?,
            logical_type: LogicalType::Text,
            analyzer: None,
            doc_values: false,
        }],
        vector: None,
        ann: None,
    };
    definition.validate()?;
    Ok(definition)
}

fn vector_search_definition(
    id: ObjectId,
    name: &str,
    dimension: u16,
    metric: VectorMetric,
    config: HnswConfig,
) -> Result<SearchCollectionDefinition, CatalogError> {
    let metric = match metric {
        VectorMetric::Cosine => CatalogVectorMetric::Cosine,
        VectorMetric::NegativeDot => CatalogVectorMetric::NegativeDot,
        VectorMetric::SquaredL2 => CatalogVectorMetric::SquaredL2,
    };
    let definition = SearchCollectionDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Search,
            name: qualified_name(name)?,
        },
        fields: Vec::new(),
        vector: Some(VectorType::new(VectorElement::Float32, dimension)?),
        ann: Some(AnnIndexDefinition::new(
            metric,
            config.m(),
            config.ef_construction(),
            config.ef_search_default(),
            config.ef_search_max(),
            config.seed(),
        )?),
    };
    definition.validate()?;
    Ok(definition)
}

fn qualified_name(name: &str) -> Result<QualifiedName, CatalogError> {
    Ok(QualifiedName::new(
        CatalogName::unquoted("main")?,
        CatalogName::unquoted("public")?,
        CatalogName::unquoted(name)?,
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
        ops::Bound,
        path::{Path, PathBuf},
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, Ordering},
        },
    };

    use hyphae_native_mvcc::WriteKey;
    use hyphae_native_types::{
        CanonicalF64, ColumnId, Csn, DurabilityClass, IntegerWidth, LogicalType,
        ManifestGeneration, ObjectId, PageId,
    };

    use super::{
        AnnSearchOptions, CatalogObject, CheckpointBoundary, CommitBoundary, HashSetOutcome,
        HnswConfig, NativeDatabase, NativeRuntimeError, NativeWriteBatch, PAGE_FILE,
        RelationalScanRow, SetCondition, SetOutcome, SqlError, SqlResult, SqlValue, Vector,
        VectorMetric,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            Self {
                path: std::env::temp_dir().join(format!(
                    "hyphae-native-runtime-{}-{sequence}",
                    std::process::id()
                )),
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }

    fn stage_vertical(
        database: &mut NativeDatabase,
    ) -> Result<super::NativeTransaction<'_>, NativeRuntimeError> {
        let table = ObjectId::new(1).map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let index = ObjectId::new(2).map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let mut transaction = database.begin(100, DurabilityClass::Strict)?;
        transaction.create_relation(table, "accounts")?;
        transaction.insert(table, b"mario".to_vec(), b"active".to_vec())?;
        transaction.set(b"session".to_vec(), b"open".to_vec(), Some(200))?;
        transaction.create_search_index(index, "notes")?;
        transaction.index_document(index, b"doc-1".to_vec(), "native rust search")?;
        assert_eq!(
            transaction.select(table, b"mario"),
            Some(b"active".as_slice())
        );
        assert_eq!(transaction.get(b"session"), Some(b"open".as_slice()));
        assert_eq!(
            transaction.match_text(index, "rust", 10)?[0].document_id,
            b"doc-1"
        );
        Ok(transaction)
    }

    fn assert_vertical(database: &NativeDatabase) -> Result<(), NativeRuntimeError> {
        let table = ObjectId::new(1).map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let index = ObjectId::new(2).map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let snapshot = database.snapshot(150)?;
        assert_eq!(
            database.select_latest_relational(table, b"mario")?,
            Some(b"active".to_vec())
        );
        assert_eq!(
            snapshot.visible_csn().map(hyphae_native_types::Csn::get),
            Some(1)
        );
        assert_eq!(snapshot.select(table, b"mario"), Some(b"active".as_slice()));
        assert_eq!(snapshot.get(b"session"), Some(b"open".as_slice()));
        assert_eq!(snapshot.ttl(b"session"), super::Ttl::RemainingMicros(50));
        assert_eq!(
            database.get_latest_structure(b"session", 150)?,
            Some(b"open".to_vec())
        );
        assert_eq!(
            database.ttl_latest_structure(b"session", 150)?,
            super::Ttl::RemainingMicros(50)
        );
        assert_eq!(
            snapshot.match_text(index, "rust", 10)?[0].document_id,
            b"doc-1"
        );
        assert_eq!(
            database.match_latest_text(index, "rust", 10)?[0].document_id,
            b"doc-1"
        );
        let expired = database.snapshot(200)?;
        assert_eq!(expired.get(b"session"), None);
        assert_eq!(database.get_latest_structure(b"session", 200)?, None);
        assert_eq!(
            database.ttl_latest_structure(b"session", 200)?,
            super::Ttl::Missing
        );
        Ok(())
    }

    fn assert_typed_sql_insert_rejections(transaction: &mut super::NativeTransaction<'_>) {
        assert!(matches!(
            transaction.execute_sql(
                "INSERT INTO people (id, name) VALUES (?, ?)",
                &[SqlValue::Signed(8), SqlValue::Null],
            ),
            Err(SqlError::NullViolation)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "INSERT INTO people (id, name) VALUES (?, ?)",
                &[
                    SqlValue::Text("wrong key type".to_owned()),
                    SqlValue::Text("rejected".to_owned()),
                ],
            ),
            Err(SqlError::TypeMismatch)
        ));
    }

    #[test]
    fn one_csn_commits_relational_structure_and_search_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let receipt = stage_vertical(&mut database)?.commit()?;
        assert_eq!(receipt.commit_csn.get(), 1);
        assert_vertical(&database)?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 1);
        assert_vertical(&reopened)?;
        Ok(())
    }

    #[test]
    fn native_inverted_index_matches_reference_bm25_across_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(10)?;
        let mut seed = database.begin(10, DurabilityClass::Memory)?;
        seed.create_search_index(index, "native_search")?;
        for document in 0..512_u32 {
            let text = if document % 2 == 0 {
                format!("rust engine common document{document}")
            } else {
                format!("sql engine common document{document}")
            };
            seed.index_document(index, document.to_be_bytes().to_vec(), text)?;
        }
        seed.commit()?;
        assert!(database.latest_search_tree_height()? >= 2);

        let historical = database.snapshot(11)?;
        for query in ["rust", "sql engine", "common", "missing"] {
            assert_eq!(
                database.match_latest_text(index, query, 25)?,
                historical.match_text(index, query, 25)?
            );
        }

        let mut later = database.begin(12, DurabilityClass::Strict)?;
        later.index_document(
            index,
            b"exclusive-document".to_vec(),
            "exclusive exclusive rust",
        )?;
        later.commit()?;
        assert!(historical.match_text(index, "exclusive", 10)?.is_empty());
        assert_eq!(
            database.match_latest_text(index, "exclusive", 10)?[0].document_id,
            b"exclusive-document"
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert!(reopened.latest_search_tree_height()? >= 2);
        assert_eq!(
            reopened.match_latest_text(index, "exclusive", 10)?[0].document_id,
            b"exclusive-document"
        );
        assert_eq!(
            reopened.match_latest_text(index, "rust engine", 25)?,
            reopened
                .snapshot(13)?
                .match_text(index, "rust engine", 25)?
        );
        Ok(())
    }

    #[test]
    fn search_metadata_mismatch_fails_complete_state_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(13)?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_search_index(index, "corruption_probe")?;
        seed.index_document(index, b"doc".to_vec(), "rust search")?;
        seed.commit()?;

        let root_set = database.coordinator.snapshot(11)?.roots().clone();
        let root = root_set
            .root(super::SLOT_SEARCH)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let bad_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(
                &mut database.pages,
                Csn::new(1)?,
                super::search_index_meta_key(index),
                super::encode_search_index_metadata(2, 2),
            )?
            .tree;
        let mut roots = root_set
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        roots.insert(
            super::SLOT_SEARCH,
            bad_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidSearchTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots,
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            super::load_search_state(&database.pages, &database.blobs, &forged),
            Err(NativeRuntimeError::InvalidSearchTree)
        ));
        Ok(())
    }

    #[test]
    fn inline_search_directories_remain_readable_and_writable()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        database.search_format = super::SearchFormat::InlineStateV1;
        let index = ObjectId::new(14)?;
        let mut create = database.begin(10, DurabilityClass::Strict)?;
        create.create_search_index(index, "legacy_search")?;
        create.index_document(index, b"first".to_vec(), "legacy rust")?;
        create.commit()?;
        drop(database);

        let mut reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.search_format, super::SearchFormat::InlineStateV1);
        assert_eq!(reopened.latest_search_tree_height()?, 0);
        assert_eq!(
            reopened.match_latest_text(index, "rust", 10)?[0].document_id,
            b"first"
        );
        let mut update = reopened.begin(11, DurabilityClass::Strict)?;
        update.index_document(index, b"second".to_vec(), "legacy sql")?;
        update.commit()?;
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            recovered.match_latest_text(index, "sql", 10)?[0].document_id,
            b"second"
        );
        Ok(())
    }

    #[test]
    fn search_rejects_identities_that_cannot_fit_native_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(15)?;
        let mut transaction = database.begin(10, DurabilityClass::Strict)?;
        transaction.create_search_index(index, "bounded_search")?;
        assert!(matches!(
            transaction.index_document(
                index,
                vec![b'd'; hyphae_native_btree::BTREE_MAX_KEY_SIZE],
                "rust",
            ),
            Err(NativeRuntimeError::SearchIdentityTooLarge)
        ));
        assert!(matches!(
            transaction.index_document(
                index,
                b"doc".to_vec(),
                "x".repeat(hyphae_native_btree::BTREE_MAX_KEY_SIZE),
            ),
            Err(NativeRuntimeError::SearchIdentityTooLarge)
        ));
        transaction.index_document(index, b"valid".to_vec(), "rust")?;
        transaction.commit()?;
        assert_eq!(
            database.match_latest_text(index, "rust", 10)?[0].document_id,
            b"valid"
        );
        Ok(())
    }

    #[test]
    fn large_cross_engine_value_uses_one_verified_blob() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = ObjectId::new(11)?;
        let index = ObjectId::new(12)?;
        let value = b"m "
            .iter()
            .copied()
            .cycle()
            .take(super::RELATIONAL_INLINE_VALUE_LIMIT + 4_096)
            .collect::<Vec<_>>();
        let text = String::from_utf8(value.clone())?;
        let mut transaction = database.begin(100, DurabilityClass::Strict)?;
        transaction.create_relation(table, "large_rows")?;
        transaction.insert(table, b"blob-row".to_vec(), value.clone())?;
        transaction.set(b"blob-key".to_vec(), value.clone(), None)?;
        transaction.create_hash(b"blob-hash".to_vec())?;
        assert_eq!(
            transaction.hset(b"blob-hash".to_vec(), b"payload".to_vec(), value.clone())?,
            HashSetOutcome::Added
        );
        transaction.create_search_index(index, "large_documents")?;
        transaction.index_document(index, b"blob-document".to_vec(), text)?;
        transaction.commit()?;
        assert_eq!(
            database.select_latest_relational(table, b"blob-row")?,
            Some(value.clone())
        );
        assert_eq!(
            database.get_latest_structure(b"blob-key", 101)?,
            Some(value.clone())
        );
        assert_eq!(
            database.hget_latest_hash(b"blob-hash", b"payload")?,
            Some(value.clone())
        );
        assert_eq!(
            database.match_latest_text(index, "m", 1)?[0].document_id,
            b"blob-document"
        );
        database.checkpoint()?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().blob_count, 1);
        assert_eq!(reopened.recovery_report().blob_generation, 1);
        assert_eq!(reopened.recovery_report().checkpoint_count, 1);
        assert_eq!(
            reopened.snapshot(101)?.select(table, b"blob-row"),
            Some(value.as_slice())
        );
        assert_eq!(
            reopened.snapshot(101)?.get(b"blob-key"),
            Some(value.as_slice())
        );
        assert_eq!(
            reopened.get_latest_structure(b"blob-key", 101)?,
            Some(value.clone())
        );
        assert_eq!(
            reopened.hget_latest_hash(b"blob-hash", b"payload")?,
            Some(value.clone())
        );
        assert_eq!(
            reopened.match_latest_text(index, "m", 1)?[0].document_id,
            b"blob-document"
        );
        assert_eq!(
            reopened.select_latest_relational(table, b"blob-row")?,
            Some(value)
        );
        Ok(())
    }

    #[test]
    fn multilevel_structure_tree_preserves_history_ttl_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let target = 1_024_u32.to_be_bytes();
        let original = vec![0x41; 96];
        let mut seed = database.begin(100, DurabilityClass::Memory)?;
        for index in 0..2_048_u32 {
            let value = if index == 1_024 {
                original.clone()
            } else {
                vec![u8::try_from(index % 251)?; 96]
            };
            seed.set(index.to_be_bytes().to_vec(), value, Some(1_000))?;
        }
        seed.commit()?;

        let root_snapshot = database.coordinator.snapshot(101)?;
        let root = root_snapshot
            .roots()
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        assert!(hyphae_native_btree::BTree::from_root(root).height(&database.pages)? >= 2);
        assert_eq!(
            database.get_latest_structure(&target, 200)?,
            Some(original.clone())
        );
        assert_eq!(
            database.ttl_latest_structure(&target, 200)?,
            super::Ttl::RemainingMicros(800)
        );
        let historical = database.snapshot(200)?;

        let updated = vec![0x42; 96];
        let mut update = database.begin(201, DurabilityClass::Strict)?;
        update.set(target.to_vec(), updated.clone(), None)?;
        update.commit()?;
        assert_eq!(historical.get(&target), Some(original.as_slice()));
        assert_eq!(
            database.get_latest_structure(&target, 202)?,
            Some(updated.clone())
        );
        assert_eq!(
            database.ttl_latest_structure(&target, 202)?,
            super::Ttl::Persistent
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert_eq!(
            reopened.get_latest_structure(&target, 203)?,
            Some(updated.clone())
        );
        assert_eq!(
            reopened.snapshot(203)?.get(&target),
            Some(updated.as_slice())
        );
        Ok(())
    }

    fn stage_scalar_semantics(
        database: &mut NativeDatabase,
    ) -> Result<super::NativeSnapshot, NativeRuntimeError> {
        let mut seed = database.begin(100, DurabilityClass::Strict)?;
        assert_eq!(
            seed.set_conditional(
                b"condition".to_vec(),
                b"v1".to_vec(),
                None,
                SetCondition::IfAbsent,
            )?,
            SetOutcome::Applied
        );
        assert_eq!(
            seed.set_conditional(
                b"condition".to_vec(),
                b"rejected".to_vec(),
                None,
                SetCondition::IfAbsent,
            )?,
            SetOutcome::NotApplied
        );
        assert_eq!(
            seed.set_conditional(
                b"condition".to_vec(),
                b"v2".to_vec(),
                None,
                SetCondition::IfPresent,
            )?,
            SetOutcome::Applied
        );
        seed.set(b"delete".to_vec(), b"old".to_vec(), None)?;
        seed.set(b"expire".to_vec(), b"alive".to_vec(), None)?;
        seed.set(b"max-expiry".to_vec(), b"alive".to_vec(), None)?;
        seed.set(b"counter".to_vec(), b"41".to_vec(), Some(1_000))?;
        seed.commit()?;
        database.snapshot(101)
    }

    fn commit_scalar_semantics(database: &mut NativeDatabase) -> Result<(), NativeRuntimeError> {
        let mut mutate = database.begin(200, DurabilityClass::Strict)?;
        assert_eq!(
            mutate.set_conditional(
                b"condition".to_vec(),
                b"still-rejected".to_vec(),
                None,
                SetCondition::IfAbsent,
            )?,
            SetOutcome::NotApplied
        );
        assert_eq!(
            mutate.set_conditional(
                b"condition".to_vec(),
                b"v3".to_vec(),
                None,
                SetCondition::IfPresent,
            )?,
            SetOutcome::Applied
        );
        assert!(mutate.delete_structure(b"delete".to_vec())?);
        assert!(!mutate.delete_structure(b"delete".to_vec())?);
        assert!(mutate.expire_structure(b"expire".to_vec(), 250)?);
        assert!(mutate.expire_structure(b"max-expiry".to_vec(), i64::MAX)?);
        assert!(!mutate.expire_structure(b"missing".to_vec(), 250)?);
        assert_eq!(mutate.increment_i64(b"counter".to_vec(), 1)?, 42);
        mutate.commit()?;
        Ok(())
    }

    #[test]
    fn scalar_conditions_delete_expire_and_counter_versions_recover()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let historical = stage_scalar_semantics(&mut database)?;
        commit_scalar_semantics(&mut database)?;

        assert_eq!(historical.get(b"condition"), Some(b"v2".as_slice()));
        assert_eq!(historical.get(b"delete"), Some(b"old".as_slice()));
        assert_eq!(historical.get(b"expire"), Some(b"alive".as_slice()));
        assert_eq!(historical.get(b"counter"), Some(b"41".as_slice()));
        let current = database.snapshot(201)?;
        assert_eq!(current.get(b"condition"), Some(b"v3".as_slice()));
        assert_eq!(current.get(b"delete"), None);
        assert_eq!(current.get(b"expire"), Some(b"alive".as_slice()));
        assert_eq!(current.ttl(b"expire"), super::Ttl::RemainingMicros(49));
        assert_eq!(current.get(b"counter"), Some(b"42".as_slice()));
        assert_eq!(current.ttl(b"counter"), super::Ttl::RemainingMicros(799));
        assert_eq!(database.get_latest_structure(b"delete", 201)?, None);
        assert_eq!(
            database.ttl_latest_structure(b"delete", 201)?,
            super::Ttl::Missing
        );
        assert_eq!(
            database.ttl_latest_structure(b"expire", 250)?,
            super::Ttl::Missing
        );

        let root = database
            .coordinator
            .snapshot(201)?
            .roots()
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tombstone = hyphae_native_btree::BTree::from_root(root)
            .get(&database.pages, &super::structure_key(b"delete"))?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        assert_eq!(
            super::decode_structure_value(&tombstone, &database.blobs)?,
            None
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert_eq!(
            reopened.get_latest_structure(b"condition", 202)?,
            Some(b"v3".to_vec())
        );
        assert_eq!(reopened.get_latest_structure(b"delete", 202)?, None);
        assert_eq!(
            reopened.get_latest_structure(b"expire", 249)?,
            Some(b"alive".to_vec())
        );
        assert_eq!(reopened.get_latest_structure(b"expire", 250)?, None);
        assert_eq!(
            reopened.get_latest_structure(b"counter", 202)?,
            Some(b"42".to_vec())
        );
        assert_eq!(
            reopened.ttl_latest_structure(b"max-expiry", 202)?,
            super::Ttl::RemainingMicros(i64::MAX - 202)
        );
        Ok(())
    }

    #[test]
    fn counters_reject_noncanonical_values_and_overflow_without_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.set(b"noncanonical".to_vec(), b"01".to_vec(), None)?;
        seed.set(
            b"overflow".to_vec(),
            i64::MAX.to_string().into_bytes(),
            None,
        )?;
        seed.commit()?;

        let mut rejected = database.begin_optimistic(11, DurabilityClass::Strict)?;
        assert!(matches!(
            rejected.increment_i64(b"noncanonical".to_vec(), 1),
            Err(NativeRuntimeError::StructureValueNotInteger)
        ));
        assert!(matches!(
            rejected.increment_i64(b"overflow".to_vec(), 1),
            Err(NativeRuntimeError::StructureIntegerOverflow)
        ));
        assert_eq!(rejected.get(b"noncanonical"), Some(b"01".as_slice()));
        assert_eq!(
            rejected.get(b"overflow"),
            Some(i64::MAX.to_string().as_bytes())
        );
        assert_eq!(
            rejected.increment_i64(b"minimum".to_vec(), i64::MIN)?,
            i64::MIN
        );
        database.commit_optimistic(rejected)?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened.get_latest_structure(b"noncanonical", 12)?,
            Some(b"01".to_vec())
        );
        assert_eq!(
            reopened.get_latest_structure(b"overflow", 12)?,
            Some(i64::MAX.to_string().into_bytes())
        );
        assert_eq!(
            reopened.get_latest_structure(b"minimum", 12)?,
            Some(i64::MIN.to_string().into_bytes())
        );
        Ok(())
    }

    #[test]
    fn racing_if_absent_batches_admit_one_writer() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut first = database.begin_optimistic(10, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(10, DurabilityClass::Strict)?;
        assert_eq!(
            first.set_conditional(
                b"race".to_vec(),
                b"first".to_vec(),
                None,
                SetCondition::IfAbsent,
            )?,
            SetOutcome::Applied
        );
        assert_eq!(
            second.set_conditional(
                b"race".to_vec(),
                b"second".to_vec(),
                None,
                SetCondition::IfAbsent,
            )?,
            SetOutcome::Applied
        );
        database.commit_optimistic(first)?;
        assert!(matches!(
            database.commit_optimistic(second),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(
            database.get_latest_structure(b"race", 11)?,
            Some(b"first".to_vec())
        );
        Ok(())
    }

    #[test]
    fn native_hash_fields_preserve_history_cardinality_tombstones_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_hash(b"profile".to_vec())?;
        assert_eq!(
            seed.hset(b"profile".to_vec(), b"name".to_vec(), b"Mario".to_vec())?,
            HashSetOutcome::Added
        );
        assert_eq!(
            seed.hset(b"profile".to_vec(), b"age".to_vec(), b"40".to_vec())?,
            HashSetOutcome::Added
        );
        assert_eq!(
            seed.hset(b"profile".to_vec(), b"name".to_vec(), b"mario".to_vec())?,
            HashSetOutcome::Updated
        );
        assert_eq!(seed.hlen(b"profile")?, 2);
        assert_eq!(seed.hget(b"profile", b"name")?, Some(b"mario".as_slice()));
        assert!(matches!(
            seed.set(b"profile".to_vec(), b"scalar".to_vec(), None),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
        seed.commit()?;
        let historical = database.snapshot(11)?;

        let mut mutate = database.begin(12, DurabilityClass::Strict)?;
        assert!(mutate.hdelete(b"profile".to_vec(), b"age".to_vec())?);
        assert!(!mutate.hdelete(b"profile".to_vec(), b"age".to_vec())?);
        assert_eq!(
            mutate.hset(b"profile".to_vec(), b"name".to_vec(), b"Mario".to_vec())?,
            HashSetOutcome::Updated
        );
        mutate.commit()?;

        assert_eq!(
            historical.hget(b"profile", b"name")?,
            Some(b"mario".as_slice())
        );
        assert_eq!(historical.hget(b"profile", b"age")?, Some(b"40".as_slice()));
        assert_eq!(database.hlen_latest_hash(b"profile")?, 1);
        assert_eq!(
            database.hget_latest_hash(b"profile", b"name")?,
            Some(b"Mario".to_vec())
        );
        assert_eq!(database.hget_latest_hash(b"profile", b"age")?, None);

        let root = database
            .coordinator
            .snapshot(13)?
            .roots()
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let age = hyphae_native_btree::BTree::from_root(root)
            .get(
                &database.pages,
                &super::structure_hash_field_key(b"profile", b"age")?,
            )?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        assert!(super::is_structure_tombstone(&age));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert_eq!(reopened.hlen_latest_hash(b"profile")?, 1);
        assert_eq!(
            reopened.hget_latest_hash(b"profile", b"name")?,
            Some(b"Mario".to_vec())
        );
        assert_eq!(reopened.hget_latest_hash(b"profile", b"age")?, None);
        Ok(())
    }

    #[test]
    fn multilevel_hash_fields_retain_snapshots_and_reopen() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let target = 1_024_u32.to_be_bytes();
        let removed = 1_025_u32.to_be_bytes();
        let original = vec![0x41; 96];
        let mut seed = database.begin(100, DurabilityClass::Memory)?;
        seed.create_hash(b"large-map".to_vec())?;
        for index in 0..2_048_u32 {
            let value = if index == 1_024 {
                original.clone()
            } else {
                vec![u8::try_from(index % 251)?; 96]
            };
            seed.hset(b"large-map".to_vec(), index.to_be_bytes().to_vec(), value)?;
        }
        seed.commit()?;
        assert!(database.latest_structure_tree_height()? >= 2);
        assert_eq!(database.hlen_latest_hash(b"large-map")?, 2_048);
        assert_eq!(
            database.hget_latest_hash(b"large-map", &target)?,
            Some(original.clone())
        );
        let historical = database.snapshot(101)?;

        let updated = vec![0x42; 96];
        let mut mutate = database.begin(102, DurabilityClass::Strict)?;
        mutate.hset(b"large-map".to_vec(), target.to_vec(), updated.clone())?;
        assert!(mutate.hdelete(b"large-map".to_vec(), removed.to_vec())?);
        mutate.commit()?;
        assert_eq!(
            historical.hget(b"large-map", &target)?,
            Some(original.as_slice())
        );
        assert!(historical.hget(b"large-map", &removed)?.is_some());
        assert_eq!(database.hlen_latest_hash(b"large-map")?, 2_047);
        assert_eq!(
            database.hget_latest_hash(b"large-map", &target)?,
            Some(updated.clone())
        );
        assert_eq!(database.hget_latest_hash(b"large-map", &removed)?, None);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.hlen_latest_hash(b"large-map")?, 2_047);
        assert_eq!(
            reopened.hget_latest_hash(b"large-map", &target)?,
            Some(updated)
        );
        assert_eq!(reopened.hget_latest_hash(b"large-map", &removed)?, None);
        Ok(())
    }

    #[test]
    fn optimistic_hash_writers_rebase_disjoint_fields_and_conflict_per_field()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_hash(b"map".to_vec())?;
        seed.commit()?;

        let mut first = database.begin_optimistic(11, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(11, DurabilityClass::Strict)?;
        first.hset(b"map".to_vec(), b"alpha".to_vec(), b"1".to_vec())?;
        second.hset(b"map".to_vec(), b"beta".to_vec(), b"2".to_vec())?;
        database.commit_optimistic(first)?;
        database.commit_optimistic(second)?;
        assert_eq!(database.hlen_latest_hash(b"map")?, 2);

        let historical = database.snapshot(12)?;
        let mut winner = database.begin_optimistic(13, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(13, DurabilityClass::Strict)?;
        winner.hset(b"map".to_vec(), b"race".to_vec(), b"winner".to_vec())?;
        loser.hset(b"map".to_vec(), b"race".to_vec(), b"loser".to_vec())?;
        database.commit_optimistic(winner)?;
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(historical.hlen(b"map")?, 2);
        assert_eq!(database.hlen_latest_hash(b"map")?, 3);
        assert_eq!(
            database.hget_latest_hash(b"map", b"race")?,
            Some(b"winner".to_vec())
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 4);
        assert_eq!(reopened.hlen_latest_hash(b"map")?, 3);
        Ok(())
    }

    #[test]
    fn hash_metadata_count_mismatch_fails_complete_state_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_hash(b"map".to_vec())?;
        seed.hset(b"map".to_vec(), b"field".to_vec(), b"value".to_vec())?;
        seed.commit()?;

        let root_set = database.coordinator.snapshot(11)?.roots().clone();
        let root = root_set
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let bad_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(
                &mut database.pages,
                Csn::new(1)?,
                super::structure_hash_meta_key(b"map"),
                super::encode_hash_metadata(2),
            )?
            .tree;
        let mut roots = root_set
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        roots.insert(
            super::SLOT_STRUCTURE,
            bad_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidStructureTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots,
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            super::load_structure_state(&database.pages, &database.blobs, &forged),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn native_set_members_preserve_history_cardinality_tombstones_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"colors".to_vec())?;
        assert!(seed.sadd(b"colors".to_vec(), b"blue".to_vec())?);
        assert!(seed.sadd(b"colors".to_vec(), b"red".to_vec())?);
        assert!(!seed.sadd(b"colors".to_vec(), b"blue".to_vec())?);
        assert!(seed.sismember(b"colors", b"blue")?);
        assert_eq!(seed.scard(b"colors")?, 2);
        assert!(matches!(
            seed.set(b"colors".to_vec(), b"scalar".to_vec(), None),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
        seed.commit()?;
        let historical = database.snapshot(11)?;

        let mut mutate = database.begin(12, DurabilityClass::Strict)?;
        assert!(mutate.srem(b"colors".to_vec(), b"red".to_vec())?);
        assert!(!mutate.srem(b"colors".to_vec(), b"red".to_vec())?);
        assert!(mutate.sadd(b"colors".to_vec(), b"green".to_vec())?);
        mutate.commit()?;

        assert!(historical.sismember(b"colors", b"red")?);
        assert!(!historical.sismember(b"colors", b"green")?);
        assert_eq!(historical.scard(b"colors")?, 2);
        assert!(database.sismember_latest_set(b"colors", b"blue")?);
        assert!(!database.sismember_latest_set(b"colors", b"red")?);
        assert!(database.sismember_latest_set(b"colors", b"green")?);
        assert_eq!(database.scard_latest_set(b"colors")?, 2);

        let root = database
            .coordinator
            .snapshot(13)?
            .roots()
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let red = hyphae_native_btree::BTree::from_root(root)
            .get(
                &database.pages,
                &super::structure_set_member_key(b"colors", b"red")?,
            )?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        assert!(super::is_structure_tombstone(&red));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert_eq!(reopened.scard_latest_set(b"colors")?, 2);
        assert!(reopened.sismember_latest_set(b"colors", b"blue")?);
        assert!(reopened.sismember_latest_set(b"colors", b"green")?);
        assert!(!reopened.sismember_latest_set(b"colors", b"red")?);
        Ok(())
    }

    #[test]
    fn optimistic_set_writers_rebase_disjoint_members_and_conflict_per_member()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.commit()?;

        let mut first = database.begin_optimistic(11, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(11, DurabilityClass::Strict)?;
        assert!(first.sadd(b"members".to_vec(), b"alpha".to_vec())?);
        assert!(second.sadd(b"members".to_vec(), b"beta".to_vec())?);
        database.commit_optimistic(first)?;
        database.commit_optimistic(second)?;
        assert_eq!(database.scard_latest_set(b"members")?, 2);

        let historical = database.snapshot(12)?;
        let mut winner = database.begin_optimistic(13, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(13, DurabilityClass::Strict)?;
        assert!(winner.sadd(b"members".to_vec(), b"race".to_vec())?);
        assert!(loser.sadd(b"members".to_vec(), b"race".to_vec())?);
        database.commit_optimistic(winner)?;
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(historical.scard(b"members")?, 2);
        assert_eq!(database.scard_latest_set(b"members")?, 3);
        assert!(database.sismember_latest_set(b"members", b"race")?);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 4);
        assert_eq!(reopened.scard_latest_set(b"members")?, 3);
        Ok(())
    }

    #[test]
    fn multilevel_set_members_retain_snapshots_and_reopen() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let removed = 1_024_u32.to_be_bytes();
        let added = 2_048_u32.to_be_bytes();
        let mut seed = database.begin(100, DurabilityClass::Memory)?;
        seed.create_set(b"large-set".to_vec())?;
        for index in 0..2_048_u32 {
            assert!(seed.sadd(b"large-set".to_vec(), index.to_be_bytes().to_vec())?);
        }
        seed.commit()?;
        assert!(database.latest_structure_tree_height()? >= 2);
        assert_eq!(database.scard_latest_set(b"large-set")?, 2_048);
        assert!(database.sismember_latest_set(b"large-set", &removed)?);
        let historical = database.snapshot(101)?;

        let mut mutate = database.begin(102, DurabilityClass::Strict)?;
        assert!(mutate.srem(b"large-set".to_vec(), removed.to_vec())?);
        assert!(mutate.sadd(b"large-set".to_vec(), added.to_vec())?);
        mutate.commit()?;
        assert!(historical.sismember(b"large-set", &removed)?);
        assert!(!historical.sismember(b"large-set", &added)?);
        assert_eq!(database.scard_latest_set(b"large-set")?, 2_048);
        assert!(!database.sismember_latest_set(b"large-set", &removed)?);
        assert!(database.sismember_latest_set(b"large-set", &added)?);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.scard_latest_set(b"large-set")?, 2_048);
        assert!(!reopened.sismember_latest_set(b"large-set", &removed)?);
        assert!(reopened.sismember_latest_set(b"large-set", &added)?);
        Ok(())
    }

    #[test]
    fn set_metadata_count_mismatch_fails_complete_state_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.sadd(b"members".to_vec(), b"one".to_vec())?;
        seed.commit()?;

        let root_set = database.coordinator.snapshot(11)?.roots().clone();
        let root = root_set
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let bad_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(
                &mut database.pages,
                Csn::new(1)?,
                super::structure_set_meta_key(b"members"),
                super::encode_set_metadata(2),
            )?
            .tree;
        let mut roots = root_set
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        roots.insert(
            super::SLOT_STRUCTURE,
            bad_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidStructureTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots,
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            super::load_structure_state(&database.pages, &database.blobs, &forged),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn malformed_and_orphan_set_members_fail_complete_state_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.commit()?;

        let roots = database.coordinator.snapshot(11)?.roots().clone();
        let root = roots
            .root(super::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let malformed =
            super::structure_storage_value(b"not-empty", None, &std::collections::BTreeMap::new())?;
        for (key, value) in [
            (
                super::structure_set_member_key(b"members", b"malformed")?,
                malformed,
            ),
            (
                super::structure_set_member_key(b"orphan", b"member")?,
                super::set_member_live_value(),
            ),
        ] {
            let forged_tree = hyphae_native_btree::BTree::from_root(root)
                .upsert(&mut database.pages, Csn::new(1)?, key, value)?
                .tree;
            let mut forged_roots = roots
                .iter_roots()
                .collect::<std::collections::BTreeMap<_, _>>();
            forged_roots.insert(
                super::SLOT_STRUCTURE,
                forged_tree
                    .root()
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?,
            );
            let forged = hyphae_native_mvcc::RootSet::committed(
                roots
                    .visible_csn()
                    .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
                roots.catalog_version(),
                roots
                    .wal_anchor()
                    .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
                forged_roots,
                roots.blob_generation(),
            )?;
            assert!(matches!(
                super::load_structure_state(&database.pages, &database.blobs, &forged),
                Err(NativeRuntimeError::InvalidStructureTree)
            ));
        }
        Ok(())
    }

    #[test]
    fn scalar_hash_and_set_creation_race_on_one_typed_key() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut scalar = database.begin_optimistic(10, DurabilityClass::Strict)?;
        let mut hash = database.begin_optimistic(10, DurabilityClass::Strict)?;
        let mut set = database.begin_optimistic(10, DurabilityClass::Strict)?;
        scalar.set(b"typed".to_vec(), b"scalar".to_vec(), None)?;
        hash.create_hash(b"typed".to_vec())?;
        set.create_set(b"typed".to_vec())?;
        database.commit_optimistic(set)?;
        assert!(matches!(
            database.commit_optimistic(scalar),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert!(matches!(
            database.commit_optimistic(hash),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(database.scard_latest_set(b"typed")?, 0);
        assert!(matches!(
            database.hlen_latest_hash(b"typed"),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
        assert!(database.get_latest_structure(b"typed", 11)?.is_none());
        Ok(())
    }

    #[test]
    fn inline_structure_directories_remain_readable_and_writable()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        database.structure_format = super::StructureFormat::InlineStateV1;
        let mut create = database.begin(10, DurabilityClass::Strict)?;
        create.set(b"legacy".to_vec(), b"v1".to_vec(), Some(50))?;
        create.commit()?;
        drop(database);

        let mut reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened.structure_format,
            super::StructureFormat::InlineStateV1
        );
        assert_eq!(
            reopened.get_latest_structure(b"legacy", 20)?,
            Some(b"v1".to_vec())
        );
        let mut update = reopened.begin(21, DurabilityClass::Strict)?;
        assert!(matches!(
            update.create_hash(b"legacy-hash".to_vec()),
            Err(NativeRuntimeError::LegacyStructureFamilyUnsupported)
        ));
        assert!(matches!(
            update.create_set(b"legacy-set".to_vec()),
            Err(NativeRuntimeError::LegacyStructureFamilyUnsupported)
        ));
        update.set(b"legacy".to_vec(), b"v2".to_vec(), None)?;
        update.commit()?;
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            recovered.get_latest_structure(b"legacy", 60)?,
            Some(b"v2".to_vec())
        );
        assert_eq!(
            recovered.ttl_latest_structure(b"legacy", 60)?,
            super::Ttl::Persistent
        );
        Ok(())
    }

    #[test]
    fn structure_value_envelope_is_canonical_and_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        fs::create_dir_all(temporary.path())?;
        let blobs = hyphae_native_blobs::BlobStore::create(temporary.path())?;
        let encoded = super::structure_storage_value(
            b"value",
            Some(i64::MAX),
            &std::collections::BTreeMap::new(),
        )?;
        let decoded = super::decode_structure_value(&encoded, &blobs)?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        assert_eq!(decoded.value, b"value");
        assert_eq!(decoded.expires_at_micros, Some(i64::MAX));

        assert_eq!(
            super::decode_structure_value(&super::structure_tombstone_value(), &blobs)?,
            None
        );
        let mut noncanonical_tombstone = super::structure_tombstone_value();
        noncanonical_tombstone.push(0);
        assert!(matches!(
            super::decode_structure_value(&noncanonical_tombstone, &blobs),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        let mut reserved = encoded.clone();
        reserved[10] = 1;
        assert!(matches!(
            super::decode_structure_value(&reserved, &blobs),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        let mut noncanonical_persistent = encoded;
        noncanonical_persistent[8] = 0;
        assert!(matches!(
            super::decode_structure_value(&noncanonical_persistent, &blobs),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        let metadata = super::encode_hash_metadata(42);
        assert_eq!(super::decode_hash_metadata(&metadata)?, 42);
        let mut trailing_metadata = metadata;
        trailing_metadata.push(0);
        assert!(matches!(
            super::decode_hash_metadata(&trailing_metadata),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert!(matches!(
            super::decode_hash_field_value(&reserved, &blobs),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        let set_metadata = super::encode_set_metadata(7);
        assert_eq!(super::decode_set_metadata(&set_metadata)?, 7);
        let mut trailing_set_metadata = set_metadata;
        trailing_set_metadata.push(0);
        assert!(matches!(
            super::decode_set_metadata(&trailing_set_metadata),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert!(super::decode_set_member_value(
            &super::set_member_live_value()
        )?);
        assert!(!super::decode_set_member_value(
            &super::structure_tombstone_value()
        )?);
        let nonempty_member =
            super::structure_storage_value(b"value", None, &std::collections::BTreeMap::new())?;
        assert!(matches!(
            super::decode_set_member_value(&nonempty_member),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        let identity = super::hash_field_identity(b"hash", b"field")?;
        assert_eq!(
            super::decode_hash_field_identity(&identity)?,
            (b"hash".as_slice(), b"field".as_slice())
        );
        assert!(matches!(
            super::decode_hash_field_identity(&[0, 0, 0, 8, 1]),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        let member_identity = super::set_member_identity(b"set", b"member")?;
        assert_eq!(
            super::decode_set_member_identity(&member_identity)?,
            (b"set".as_slice(), b"member".as_slice())
        );
        Ok(())
    }

    fn assert_three_version_row_chain(
        database: &NativeDatabase,
        table: ObjectId,
    ) -> Result<(), NativeRuntimeError> {
        let current = database.coordinator.snapshot(16)?;
        let relational_root = current
            .roots()
            .root(super::SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let pointer_bytes = hyphae_native_btree::BTree::from_root(relational_root)
            .get(&database.pages, &super::relational_row_key(table, b"mario"))?
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        let pointer = hyphae_native_records::RowVersionPointer::decode(&pointer_bytes)?;
        let latest_page = database.pages.read(pointer.page_id)?;
        let latest = hyphae_native_records::RowRecord::decode(latest_page.payload())?;
        assert_eq!(latest.begin_csn().get(), 3);
        assert_eq!(latest.end_csn(), None);
        assert!(latest.is_tombstone());

        let updated_page = database.pages.read(
            latest_page
                .next()
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?,
        )?;
        let updated = hyphae_native_records::RowRecord::decode(updated_page.payload())?;
        assert_eq!(updated.begin_csn().get(), 2);
        assert_eq!(
            updated.end_csn().map(hyphae_native_types::Csn::get),
            Some(3)
        );
        assert!(!updated.is_tombstone());

        let inserted_page = database.pages.read(
            updated_page
                .next()
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?,
        )?;
        let inserted = hyphae_native_records::RowRecord::decode(inserted_page.payload())?;
        assert_eq!(inserted.begin_csn().get(), 1);
        assert_eq!(
            inserted.end_csn().map(hyphae_native_types::Csn::get),
            Some(2)
        );
        assert!(!inserted.is_tombstone());
        assert_eq!(inserted_page.next(), None);
        Ok(())
    }

    #[test]
    fn blob_stage_and_promotion_interruptions_recover_explicitly()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [CommitBoundary::BlobStaged, CommitBoundary::BlobPromoted] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let table = ObjectId::new(12)?;
            let mut transaction = database.begin(100, DurabilityClass::Strict)?;
            transaction.create_relation(table, "interrupted_blobs")?;
            transaction.insert(
                table,
                b"blob-row".to_vec(),
                vec![0x5a; super::RELATIONAL_INLINE_VALUE_LIMIT + 1],
            )?;
            assert!(matches!(
                transaction.commit_with_interruption(boundary),
                Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            assert_eq!(reopened.recovery_report().visible_csn, None);
            match boundary {
                CommitBoundary::BlobStaged => {
                    assert_eq!(reopened.recovery_report().blob_count, 0);
                    assert_eq!(reopened.recovery_report().recovered_temporary_blobs, 1);
                }
                CommitBoundary::BlobPromoted => {
                    assert_eq!(reopened.recovery_report().blob_count, 1);
                    assert_eq!(reopened.recovery_report().blob_generation, 1);
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[test]
    fn immutable_checkpoint_round_trips_without_advancing_csn()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        stage_vertical(&mut database)?.commit()?;
        let checkpoint = database.checkpoint()?;
        assert_eq!(checkpoint.visible_csn.get(), 1);
        assert_eq!(checkpoint.manifest_generation.get(), 1);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let recovery = reopened.recovery_report();
        assert_eq!(recovery.checkpoint_count, 1);
        assert_eq!(recovery.manifest_count, 1);
        assert_eq!(recovery.unanchored_manifest_suffix, 0);
        assert_eq!(
            recovery
                .latest_checkpoint_generation
                .map(ManifestGeneration::get),
            Some(1)
        );
        assert_vertical(&reopened)?;
        Ok(())
    }

    #[test]
    fn every_checkpoint_boundary_recovers_anchored_or_unanchored_state()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CheckpointBoundary::ManifestStaged,
            CheckpointBoundary::ManifestPublished,
            CheckpointBoundary::WalAppended,
            CheckpointBoundary::WalSynchronized,
        ] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            stage_vertical(&mut database)?.commit()?;
            let result = database.checkpoint_with_interruption(boundary);
            assert!(matches!(
                result,
                Err(NativeRuntimeError::InjectedCheckpointCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            assert_vertical(&reopened)?;
            match boundary {
                CheckpointBoundary::ManifestStaged => {
                    assert_eq!(reopened.recovery_report().manifest_count, 0);
                    assert_eq!(reopened.recovery_report().checkpoint_count, 0);
                    assert_eq!(reopened.recovery_report().recovered_temporary_manifests, 1);
                }
                CheckpointBoundary::ManifestPublished => {
                    assert_eq!(reopened.recovery_report().manifest_count, 1);
                    assert_eq!(reopened.recovery_report().checkpoint_count, 0);
                    assert_eq!(reopened.recovery_report().unanchored_manifest_suffix, 1);
                }
                CheckpointBoundary::WalAppended | CheckpointBoundary::WalSynchronized => {
                    assert_eq!(reopened.recovery_report().manifest_count, 1);
                    assert_eq!(reopened.recovery_report().checkpoint_count, 1);
                    assert_eq!(reopened.recovery_report().unanchored_manifest_suffix, 0);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn later_checkpoint_can_anchor_a_verified_unanchored_manifest_chain()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        stage_vertical(&mut database)?.commit()?;
        assert!(matches!(
            database.checkpoint_with_interruption(CheckpointBoundary::ManifestPublished),
            Err(NativeRuntimeError::InjectedCheckpointCrash(
                CheckpointBoundary::ManifestPublished
            ))
        ));
        drop(database);

        let mut reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().unanchored_manifest_suffix, 1);
        let receipt = reopened.checkpoint()?;
        assert_eq!(receipt.manifest_generation.get(), 2);
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        assert_eq!(recovered.recovery_report().manifest_count, 2);
        assert_eq!(recovered.recovery_report().checkpoint_count, 1);
        assert_eq!(recovered.recovery_report().unanchored_manifest_suffix, 0);
        assert_eq!(
            recovered
                .recovery_report()
                .latest_checkpoint_generation
                .map(ManifestGeneration::get),
            Some(2)
        );
        assert_vertical(&recovered)?;
        Ok(())
    }

    #[test]
    fn every_commit_boundary_recovers_prior_or_complete_state()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let result = stage_vertical(&mut database)?.commit_with_interruption(boundary);
            assert!(matches!(
                result,
                Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            match reopened.recovery_report().visible_csn {
                None => {
                    assert!(matches!(
                        boundary,
                        CommitBoundary::BlobStaged
                            | CommitBoundary::BlobPromoted
                            | CommitBoundary::PageAppended
                            | CommitBoundary::PageSynchronized
                    ));
                }
                Some(csn) => {
                    assert_eq!(csn.get(), 1);
                    assert_vertical(&reopened)?;
                }
            }
        }
        Ok(())
    }

    #[test]
    fn optimistic_commit_boundaries_recover_prior_or_complete_state()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let mut seed = stage_vertical(&mut database)?;
            seed.set(b"remove-on-commit".to_vec(), b"present".to_vec(), None)?;
            seed.create_hash(b"crash-hash".to_vec())?;
            seed.hset(
                b"crash-hash".to_vec(),
                b"before".to_vec(),
                b"present".to_vec(),
            )?;
            seed.create_set(b"crash-set".to_vec())?;
            seed.sadd(b"crash-set".to_vec(), b"before".to_vec())?;
            seed.commit()?;
            let table = ObjectId::new(1)?;
            let mut batch = database.begin_optimistic(151, DurabilityClass::Strict)?;
            batch.update(table, b"mario".to_vec(), b"optimistic".to_vec())?;
            assert!(batch.delete_structure(b"remove-on-commit".to_vec())?);
            assert!(batch.expire_structure(b"session".to_vec(), 300)?);
            assert!(batch.hdelete(b"crash-hash".to_vec(), b"before".to_vec())?);
            batch.hset(
                b"crash-hash".to_vec(),
                b"after".to_vec(),
                b"present".to_vec(),
            )?;
            assert!(batch.srem(b"crash-set".to_vec(), b"before".to_vec())?);
            assert!(batch.sadd(b"crash-set".to_vec(), b"after".to_vec())?);
            assert!(matches!(
                database.commit_optimistic_with_interruption(batch, boundary),
                Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            let snapshot = reopened.snapshot(152)?;
            match reopened
                .recovery_report()
                .visible_csn
                .map(hyphae_native_types::Csn::get)
            {
                Some(1) => {
                    assert!(matches!(
                        boundary,
                        CommitBoundary::BlobStaged
                            | CommitBoundary::BlobPromoted
                            | CommitBoundary::PageAppended
                            | CommitBoundary::PageSynchronized
                    ));
                    assert_eq!(snapshot.select(table, b"mario"), Some(b"active".as_slice()));
                    assert_eq!(
                        snapshot.get(b"remove-on-commit"),
                        Some(b"present".as_slice())
                    );
                    assert_eq!(snapshot.ttl(b"session"), super::Ttl::RemainingMicros(48));
                    assert_eq!(
                        snapshot.hget(b"crash-hash", b"before")?,
                        Some(b"present".as_slice())
                    );
                    assert_eq!(snapshot.hget(b"crash-hash", b"after")?, None);
                    assert!(snapshot.sismember(b"crash-set", b"before")?);
                    assert!(!snapshot.sismember(b"crash-set", b"after")?);
                    assert_eq!(snapshot.scard(b"crash-set")?, 1);
                }
                Some(2) => {
                    assert!(matches!(
                        boundary,
                        CommitBoundary::WalAppended
                            | CommitBoundary::WalSynchronized
                            | CommitBoundary::RootPublished
                    ));
                    assert_eq!(
                        snapshot.select(table, b"mario"),
                        Some(b"optimistic".as_slice())
                    );
                    assert_eq!(snapshot.get(b"remove-on-commit"), None);
                    assert_eq!(snapshot.ttl(b"session"), super::Ttl::RemainingMicros(148));
                    assert_eq!(snapshot.hget(b"crash-hash", b"before")?, None);
                    assert_eq!(
                        snapshot.hget(b"crash-hash", b"after")?,
                        Some(b"present".as_slice())
                    );
                    assert!(!snapshot.sismember(b"crash-set", b"before")?);
                    assert!(snapshot.sismember(b"crash-set", b"after")?);
                    assert_eq!(snapshot.scard(b"crash-set")?, 1);
                }
                found => return Err(format!("unexpected recovered CSN: {found:?}").into()),
            }
        }
        Ok(())
    }

    #[test]
    fn concurrent_optimistic_preparation_rebases_disjoint_writes_and_rejects_conflicts()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        stage_vertical(&mut database)?.commit()?;
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;
        let barrier = Arc::new(Barrier::new(2));

        let ((first, second), (first_read_csn, second_read_csn)) = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_database = &database;
            let first = scope.spawn(move || -> Result<_, NativeRuntimeError> {
                let mut batch = first_database.begin_optimistic(151, DurabilityClass::Strict)?;
                let read_csn = batch.read_csn();
                first_barrier.wait();
                batch.update(table, b"mario".to_vec(), b"first".to_vec())?;
                batch.set(b"first".to_vec(), b"structure".to_vec(), None)?;
                batch.index_document(index, b"doc-first".to_vec(), "first writer")?;
                Ok((batch, read_csn))
            });
            let second_barrier = Arc::clone(&barrier);
            let second_database = &database;
            let second = scope.spawn(move || -> Result<_, NativeRuntimeError> {
                let mut batch = second_database.begin_optimistic(151, DurabilityClass::Strict)?;
                let read_csn = batch.read_csn();
                second_barrier.wait();
                batch.insert(table, b"second".to_vec(), b"row".to_vec())?;
                batch.set(b"second".to_vec(), b"structure".to_vec(), None)?;
                batch.index_document(index, b"doc-second".to_vec(), "second writer")?;
                Ok((batch, read_csn))
            });
            let (first, first_read_csn) = first
                .join()
                .map_err(|_| std::io::Error::other("first writer thread panicked"))??;
            let (second, second_read_csn) = second
                .join()
                .map_err(|_| std::io::Error::other("second writer thread panicked"))??;
            Ok::<_, Box<dyn std::error::Error>>((
                (first, second),
                (first_read_csn, second_read_csn),
            ))
        })?;
        assert_eq!(first_read_csn.map(Csn::get), Some(1));
        assert_eq!(second_read_csn.map(Csn::get), Some(1));
        assert_eq!(database.commit_optimistic(first)?.commit_csn.get(), 2);
        assert_eq!(database.commit_optimistic(second)?.commit_csn.get(), 3);

        let rebased = database.snapshot(152)?;
        assert_eq!(rebased.select(table, b"mario"), Some(b"first".as_slice()));
        assert_eq!(rebased.select(table, b"second"), Some(b"row".as_slice()));
        assert_eq!(rebased.get(b"first"), Some(b"structure".as_slice()));
        assert_eq!(rebased.get(b"second"), Some(b"structure".as_slice()));
        assert_eq!(
            rebased.match_text(index, "first", 10)?[0].document_id,
            b"doc-first"
        );
        assert_eq!(
            rebased.match_text(index, "second", 10)?[0].document_id,
            b"doc-second"
        );

        let mut winner = database.begin_optimistic(153, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(153, DurabilityClass::Strict)?;
        assert_eq!(winner.read_csn().map(Csn::get), Some(3));
        assert_eq!(loser.read_csn().map(Csn::get), Some(3));
        winner.update(table, b"mario".to_vec(), b"winner".to_vec())?;
        loser.update(table, b"mario".to_vec(), b"loser".to_vec())?;
        assert_eq!(database.commit_optimistic(winner)?.commit_csn.get(), 4);
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(
            database.snapshot(154)?.select(table, b"mario"),
            Some(b"winner".as_slice())
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 4);
        let recovered = reopened.snapshot(155)?;
        assert_eq!(
            recovered.select(table, b"mario"),
            Some(b"winner".as_slice())
        );
        assert_eq!(recovered.select(table, b"second"), Some(b"row".as_slice()));
        assert_eq!(recovered.get(b"first"), Some(b"structure".as_slice()));
        assert_eq!(recovered.get(b"second"), Some(b"structure".as_slice()));
        assert_eq!(
            recovered.match_text(index, "first", 10)?[0].document_id,
            b"doc-first"
        );
        assert_eq!(
            recovered.match_text(index, "second", 10)?[0].document_id,
            b"doc-second"
        );
        Ok(())
    }

    #[test]
    fn disjoint_genesis_batches_recover_with_their_original_read_csn()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut first = database.begin_optimistic(10, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(10, DurabilityClass::Strict)?;
        assert_eq!(first.read_csn(), None);
        assert_eq!(second.read_csn(), None);
        first.set(b"first".to_vec(), b"one".to_vec(), None)?;
        second.set(b"second".to_vec(), b"two".to_vec(), None)?;
        assert_eq!(database.commit_optimistic(first)?.commit_csn.get(), 1);
        assert_eq!(database.commit_optimistic(second)?.commit_csn.get(), 2);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        let recovered = reopened.snapshot(11)?;
        assert_eq!(recovered.get(b"first"), Some(b"one".as_slice()));
        assert_eq!(recovered.get(b"second"), Some(b"two".as_slice()));
        Ok(())
    }

    #[test]
    fn native_sql_create_insert_prepare_select_and_rollback_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE accounts (primary_key BINARY PRIMARY KEY, row BINARY)",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        transaction.execute_sql(
            "INSERT INTO accounts (primary_key, row) VALUES (?, ?)",
            &[
                SqlValue::Binary(b"mario".to_vec()),
                SqlValue::Binary(b"active".to_vec()),
            ],
        )?;
        let private = transaction.execute_sql(
            "SELECT row FROM accounts WHERE primary_key = ?",
            &[SqlValue::Binary(b"mario".to_vec())],
        )?;
        assert_eq!(
            private,
            SqlResult::Rows {
                columns: vec!["row".to_owned()],
                rows: vec![vec![SqlValue::Binary(b"active".to_vec())]],
            }
        );
        transaction.commit()?;

        let snapshot = database.snapshot(11)?;
        let Some(CatalogObject::Relation(definition)) = snapshot.catalog_object(table) else {
            return Err("missing persisted relation definition".into());
        };
        assert_eq!(definition.header.name.object.lookup(), "accounts");
        assert_eq!(definition.columns.len(), 2);
        assert!(definition.columns.iter().all(|column| !column.nullable));
        assert_eq!(definition.primary_key, [ColumnId::new(1)?]);
        let prepared = snapshot.prepare_sql("SELECT row FROM accounts WHERE primary_key = ?")?;
        assert_eq!(
            snapshot.execute_prepared(&prepared, &[SqlValue::Binary(b"mario".to_vec())],)?,
            private
        );

        let mut rolled_back = database.begin_sql(12, DurabilityClass::Strict)?;
        rolled_back.execute_sql(
            "INSERT INTO accounts (primary_key, row) VALUES (?, ?)",
            &[
                SqlValue::Binary(b"discarded".to_vec()),
                SqlValue::Binary(b"row".to_vec()),
            ],
        )?;
        rolled_back.rollback();
        let after_rollback = database.snapshot(13)?;
        assert_eq!(
            after_rollback
                .execute_prepared(&prepared, &[SqlValue::Binary(b"discarded".to_vec())],)?,
            SqlResult::Rows {
                columns: vec!["row".to_owned()],
                rows: Vec::new(),
            }
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let recovered = reopened.snapshot(14)?;
        let Some(CatalogObject::Relation(definition)) = recovered.catalog_object(table) else {
            return Err("relation definition did not survive reopen".into());
        };
        assert_eq!(definition.header.name.object.lookup(), "accounts");
        assert_eq!(definition.columns.len(), 2);
        assert_eq!(definition.primary_key, [ColumnId::new(1)?]);
        Ok(())
    }

    #[test]
    fn typed_sql_rows_bind_to_catalog_and_survive_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                name TEXT NOT NULL,
                active BOOLEAN,
                score DOUBLE
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing typed table identity".into());
        };
        let score = SqlValue::Float64(CanonicalF64::new(1.5));
        transaction.execute_sql(
            "INSERT INTO people (id, name, active, score) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::Signed(7),
                SqlValue::Text("Mario".to_owned()),
                SqlValue::Boolean(true),
                score.clone(),
            ],
        )?;
        assert_typed_sql_insert_rejections(&mut transaction);
        let private = transaction.execute_sql(
            "SELECT name, active, score FROM people WHERE id = ?",
            &[SqlValue::Signed(7)],
        )?;
        assert_eq!(
            private,
            SqlResult::Rows {
                columns: vec!["name".to_owned(), "active".to_owned(), "score".to_owned()],
                rows: vec![vec![
                    SqlValue::Text("Mario".to_owned()),
                    SqlValue::Boolean(true),
                    score.clone(),
                ]],
            }
        );
        transaction.commit()?;

        let snapshot = database.snapshot(11)?;
        let Some(CatalogObject::Relation(definition)) = snapshot.catalog_object(table) else {
            return Err("missing typed relation definition".into());
        };
        assert_eq!(
            definition.columns[0].logical_type,
            LogicalType::Signed(IntegerWidth::Bits64)
        );
        assert_eq!(definition.columns[1].logical_type, LogicalType::Text);
        assert_eq!(definition.columns[2].logical_type, LogicalType::Boolean);
        assert_eq!(definition.columns[3].logical_type, LogicalType::Float64);
        assert_eq!(definition.primary_key, [ColumnId::new(1)?]);
        let prepared =
            snapshot.prepare_sql("SELECT name, active, score FROM people WHERE id = ?")?;
        assert_eq!(
            snapshot.execute_prepared(&prepared, &[SqlValue::Signed(7)])?,
            private
        );
        assert_eq!(
            snapshot.execute_prepared(&prepared, &[SqlValue::Signed(8)])?,
            SqlResult::Rows {
                columns: vec!["name".to_owned(), "active".to_owned(), "score".to_owned()],
                rows: Vec::new(),
            }
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let recovered = reopened.snapshot(12)?;
        let recovered_plan = recovered.prepare_sql("SELECT * FROM people WHERE id = ?")?;
        assert_eq!(
            recovered.execute_prepared(&recovered_plan, &[SqlValue::Signed(7)])?,
            SqlResult::Rows {
                columns: vec![
                    "id".to_owned(),
                    "name".to_owned(),
                    "active".to_owned(),
                    "score".to_owned(),
                ],
                rows: vec![vec![
                    SqlValue::Signed(7),
                    SqlValue::Text("Mario".to_owned()),
                    SqlValue::Boolean(true),
                    score,
                ]],
            }
        );
        Ok(())
    }

    #[test]
    fn typed_sql_composite_key_binds_predicates_independent_of_text_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE events (
                tenant TEXT NOT NULL,
                sequence BIGINT NOT NULL,
                payload TEXT,
                PRIMARY KEY (tenant, sequence)
            )",
            &[],
        )?;
        transaction.execute_sql(
            "INSERT INTO events (payload, sequence, tenant) VALUES (?, ?, ?)",
            &[
                SqlValue::Text("native".to_owned()),
                SqlValue::Signed(42),
                SqlValue::Text("celiums".to_owned()),
            ],
        )?;
        assert_eq!(
            transaction.execute_sql(
                "SELECT payload FROM events WHERE sequence = ? AND tenant = ?",
                &[SqlValue::Signed(42), SqlValue::Text("celiums".to_owned()),],
            )?,
            SqlResult::Rows {
                columns: vec!["payload".to_owned()],
                rows: vec![vec![SqlValue::Text("native".to_owned())]],
            }
        );
        assert!(matches!(
            transaction.execute_sql(
                "SELECT payload FROM events WHERE tenant = ?",
                &[SqlValue::Text("celiums".to_owned())],
            ),
            Err(SqlError::InvalidPrimaryKey)
        ));
        transaction.commit()?;

        let snapshot = database.snapshot(11)?;
        let prepared =
            snapshot.prepare_sql("SELECT payload FROM events WHERE sequence = ? AND tenant = ?")?;
        assert_eq!(
            snapshot.execute_prepared(
                &prepared,
                &[SqlValue::Signed(42), SqlValue::Text("celiums".to_owned()),],
            )?,
            SqlResult::Rows {
                columns: vec!["payload".to_owned()],
                rows: vec![vec![SqlValue::Text("native".to_owned())]],
            }
        );
        Ok(())
    }

    #[test]
    fn secondary_index_backfill_insert_explain_and_recovery_are_equivalent()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL,
                name TEXT NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        for (id, name) in [(1, "Mario"), (2, "Romina")] {
            transaction.execute_sql(
                "INSERT INTO people (id, email, name) VALUES (?, ?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text("family@celiums.test".to_owned()),
                    SqlValue::Text(name.to_owned()),
                ],
            )?;
        }
        let created =
            transaction.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing index identity".into());
        };
        transaction.execute_sql(
            "INSERT INTO people (id, email, name) VALUES (?, ?, ?)",
            &[
                SqlValue::Signed(3),
                SqlValue::Text("family@celiums.test".to_owned()),
                SqlValue::Text("Luciana".to_owned()),
            ],
        )?;
        let expected = SqlResult::Rows {
            columns: vec!["id".to_owned(), "name".to_owned()],
            rows: vec![
                vec![SqlValue::Signed(1), SqlValue::Text("Mario".to_owned())],
                vec![SqlValue::Signed(2), SqlValue::Text("Romina".to_owned())],
                vec![SqlValue::Signed(3), SqlValue::Text("Luciana".to_owned())],
            ],
        };
        assert_eq!(
            transaction.execute_sql(
                "SELECT id, name FROM people WHERE email = ?",
                &[SqlValue::Text("family@celiums.test".to_owned())],
            )?,
            expected
        );
        assert_eq!(
            transaction.execute_sql("EXPLAIN SELECT id, name FROM people WHERE email = ?", &[],)?,
            SqlResult::Rows {
                columns: vec!["plan".to_owned()],
                rows: vec![vec![SqlValue::Text(format!(
                    "SecondaryIndexLookup(table={table},index={index})"
                ))]],
            }
        );
        transaction.commit()?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let snapshot = reopened.snapshot(11)?;
        let Some(CatalogObject::SecondaryIndex(definition)) = snapshot.catalog_object(index) else {
            return Err("secondary-index definition did not survive reopen".into());
        };
        assert_eq!(definition.relation, table);
        assert!(!definition.unique);
        let prepared = snapshot.prepare_sql("SELECT id, name FROM people WHERE email = ?")?;
        assert_eq!(
            snapshot.execute_prepared(
                &prepared,
                &[SqlValue::Text("family@celiums.test".to_owned())],
            )?,
            expected
        );
        Ok(())
    }

    fn seed_scaled_secondary_index(
        database: &mut NativeDatabase,
    ) -> Result<(ObjectId, ObjectId), Box<dyn std::error::Error>> {
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL,
                name TEXT NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        for id in 0_i64..512 {
            let email = if matches!(id, 7 | 23 | 511) {
                "family@celiums.test".to_owned()
            } else {
                format!("person-{id:04}@celiums.test")
            };
            transaction.execute_sql(
                "INSERT INTO people (id, email, name) VALUES (?, ?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(email),
                    SqlValue::Text(format!("person-{id:04}")),
                ],
            )?;
        }
        let created =
            transaction.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing secondary-index identity".into());
        };
        transaction.commit()?;
        Ok((table, index))
    }

    #[test]
    fn latest_sql_uses_exact_physical_secondary_index_without_materialized_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let (table, index) = seed_scaled_secondary_index(&mut database)?;
        assert!(database.latest_relational_tree_height()? >= 2);

        let query = "SELECT id, name FROM people WHERE email = ?";
        let parameters = [SqlValue::Text("family@celiums.test".to_owned())];
        let materialized = database.snapshot(11)?;
        let materialized_plan = materialized.prepare_sql(query)?;
        let expected = materialized.execute_prepared(&materialized_plan, &parameters)?;
        let physical_plan = database.prepare_sql_latest(query)?;
        assert_eq!(
            database.execute_prepared_latest(&physical_plan, &parameters)?,
            expected
        );
        assert_eq!(
            database.execute_prepared_latest(&physical_plan, &[SqlValue::Null])?,
            SqlResult::Rows {
                columns: vec!["id".to_owned(), "name".to_owned()],
                rows: Vec::new(),
            }
        );

        let index_key = parameters[0].encode_ordered_component(&LogicalType::Text)?;
        let physical_rows = database.select_latest_secondary_index(index, &index_key)?;
        assert_eq!(physical_rows.len(), 3);
        assert!(physical_rows.iter().all(|row| row.table == table));
        assert!(
            physical_rows
                .windows(2)
                .all(|rows| { rows[0].primary_key.as_slice() < rows[1].primary_key.as_slice() })
        );
        for row in &physical_rows {
            assert_eq!(
                database.select_latest_relational(table, &row.primary_key)?,
                Some(row.row.clone())
            );
        }
        assert!(matches!(
            database.select_latest_secondary_index(ObjectId::new(9_999)?, &index_key),
            Err(NativeRuntimeError::UnknownSecondaryIndex { index: missing })
                if missing == ObjectId::new(9_999)?
        ));

        let historical = database.snapshot(12)?;
        let mut catalog_change = database.begin_sql(13, DurabilityClass::Strict)?;
        catalog_change.execute_sql("CREATE TABLE audit (id BIGINT PRIMARY KEY, note TEXT)", &[])?;
        catalog_change.commit()?;
        assert!(matches!(
            database.execute_prepared_latest(&physical_plan, &parameters),
            Err(SqlError::CatalogChanged)
        ));
        assert_eq!(
            historical.execute_prepared(&physical_plan, &parameters)?,
            expected
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(query)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &parameters)?,
            expected
        );
        assert_eq!(
            reopened.select_latest_secondary_index(index, &index_key)?,
            physical_rows
        );
        Ok(())
    }

    fn assert_latest_relational_range_contract(
        database: &NativeDatabase,
        table: ObjectId,
    ) -> Result<Vec<RelationalScanRow>, Box<dyn std::error::Error>> {
        let lower = 2_u32.to_be_bytes();
        let upper = 10_u32.to_be_bytes();
        let half_open = database.scan_latest_relational_range(
            table,
            Bound::Included(lower.as_slice()),
            Bound::Excluded(upper.as_slice()),
            16,
        )?;
        assert_eq!(
            half_open
                .iter()
                .map(|row| row.primary_key.clone())
                .collect::<Vec<_>>(),
            [2_u32, 4, 5, 6]
                .iter()
                .map(|index| index.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        let mixed = database.scan_latest_relational_range(
            table,
            Bound::Excluded(lower.as_slice()),
            Bound::Included(upper.as_slice()),
            16,
        )?;
        assert_eq!(
            mixed
                .iter()
                .map(|row| row.primary_key.clone())
                .collect::<Vec<_>>(),
            [4_u32, 5, 6, 10]
                .iter()
                .map(|index| index.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        let reversed = database.scan_latest_relational_range(
            table,
            Bound::Included(upper.as_slice()),
            Bound::Excluded(lower.as_slice()),
            16,
        )?;
        assert!(reversed.is_empty());
        Ok(half_open)
    }

    #[test]
    fn latest_relational_scan_is_bounded_resumable_and_skips_tombstones()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = ObjectId::new(77)?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_relation(table, "events")?;
        for index in 0..512_u32 {
            seed.insert(
                table,
                index.to_be_bytes().to_vec(),
                format!("event-{index:04}").into_bytes(),
            )?;
        }
        seed.commit()?;
        assert!(database.latest_relational_tree_height()? >= 2);

        let mut mutation = database.begin(11, DurabilityClass::Strict)?;
        mutation.update(table, 2_u32.to_be_bytes().to_vec(), b"updated".to_vec())?;
        for index in [1_u32, 3, 7, 8, 9] {
            mutation.delete(table, index.to_be_bytes().to_vec())?;
        }
        mutation.commit()?;

        let first = database.scan_latest_relational(table, None, 5)?;
        assert_eq!(
            first
                .iter()
                .map(|row| row.primary_key.clone())
                .collect::<Vec<_>>(),
            [0_u32, 2, 4, 5, 6]
                .iter()
                .map(|index| index.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        assert!(first.iter().all(|row| row.table == table));
        assert_eq!(first[1].row, b"updated");

        let cursor = first
            .last()
            .ok_or("missing first scan page")?
            .primary_key
            .clone();
        let second = database.scan_latest_relational(table, Some(&cursor), 4)?;
        assert_eq!(
            second
                .iter()
                .map(|row| row.primary_key.clone())
                .collect::<Vec<_>>(),
            [10_u32, 11, 12, 13]
                .iter()
                .map(|index| index.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        assert!(database.scan_latest_relational(table, None, 0)?.is_empty());
        let half_open = assert_latest_relational_range_contract(&database, table)?;
        assert!(matches!(
            database.scan_latest_relational(ObjectId::new(9_999)?, None, 1),
            Err(NativeRuntimeError::UnknownRelation { table: missing })
                if missing == ObjectId::new(9_999)?
        ));

        drop(database);
        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.scan_latest_relational(table, None, 5)?, first);
        assert_eq!(
            reopened.scan_latest_relational(table, Some(&cursor), 4)?,
            second
        );
        assert_eq!(
            reopened.scan_latest_relational_range(
                table,
                Bound::Included(2_u32.to_be_bytes().as_slice()),
                Bound::Excluded(10_u32.to_be_bytes().as_slice()),
                16,
            )?,
            half_open
        );
        Ok(())
    }

    fn seed_bounded_sql_scan(
        database: &mut NativeDatabase,
    ) -> Result<ObjectId, Box<dyn std::error::Error>> {
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = seed.execute_sql(
            "CREATE TABLE events (
                tenant BIGINT NOT NULL,
                sequence BIGINT NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY (tenant, sequence)
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing events table identity".into());
        };
        for sequence in 0_i64..512 {
            seed.execute_sql(
                "INSERT INTO events (tenant, sequence, payload) VALUES (?, ?, ?)",
                &[
                    SqlValue::Signed(1),
                    SqlValue::Signed(sequence),
                    SqlValue::Text(format!("event-{sequence:04}")),
                ],
            )?;
        }
        assert_eq!(
            seed.execute_sql(
                "SELECT sequence, payload FROM events ORDER BY tenant, sequence LIMIT 2",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["sequence".to_owned(), "payload".to_owned()],
                rows: vec![
                    vec![SqlValue::Signed(0), SqlValue::Text("event-0000".to_owned()),],
                    vec![SqlValue::Signed(1), SqlValue::Text("event-0001".to_owned()),],
                ],
            }
        );
        assert_eq!(
            seed.execute_sql(
                "EXPLAIN SELECT payload FROM events ORDER BY tenant, sequence LIMIT 5",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["plan".to_owned()],
                rows: vec![vec![SqlValue::Text(format!(
                    "PrimaryKeyScan(table={table},limit=5)"
                ))]],
            }
        );
        seed.commit()?;
        Ok(table)
    }

    fn mutate_bounded_sql_scan(
        database: &mut NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut mutation = database.begin_sql(11, DurabilityClass::Strict)?;
        mutation.execute_sql(
            "UPDATE events SET payload = ? WHERE tenant = ? AND sequence = ?",
            &[
                SqlValue::Text("updated".to_owned()),
                SqlValue::Signed(1),
                SqlValue::Signed(2),
            ],
        )?;
        for sequence in [1_i64, 3, 7, 8, 9] {
            mutation.execute_sql(
                "DELETE FROM events WHERE tenant = ? AND sequence = ?",
                &[SqlValue::Signed(1), SqlValue::Signed(sequence)],
            )?;
        }
        mutation.commit()?;
        Ok(())
    }

    fn expected_bounded_sql_scan() -> SqlResult {
        SqlResult::Rows {
            columns: vec!["sequence".to_owned(), "payload".to_owned()],
            rows: vec![
                vec![SqlValue::Signed(0), SqlValue::Text("event-0000".to_owned())],
                vec![SqlValue::Signed(2), SqlValue::Text("updated".to_owned())],
                vec![SqlValue::Signed(4), SqlValue::Text("event-0004".to_owned())],
                vec![SqlValue::Signed(5), SqlValue::Text("event-0005".to_owned())],
                vec![SqlValue::Signed(6), SqlValue::Text("event-0006".to_owned())],
            ],
        }
    }

    #[test]
    fn bounded_sql_scan_matches_transaction_snapshot_and_physical_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let _table = seed_bounded_sql_scan(&mut database)?;
        let query = "SELECT sequence, payload FROM events ORDER BY tenant, sequence LIMIT 5";
        assert!(database.latest_relational_tree_height()? >= 2);
        mutate_bounded_sql_scan(&mut database)?;
        let expected = expected_bounded_sql_scan();
        let materialized = database.snapshot(12)?;
        let materialized_plan = materialized.prepare_sql(query)?;
        assert_eq!(
            materialized.execute_prepared(&materialized_plan, &[])?,
            expected
        );
        let physical_plan = database.prepare_sql_latest(query)?;
        assert_eq!(
            database.execute_prepared_latest(&physical_plan, &[])?,
            expected
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest("SELECT sequence, payload FROM events LIMIT 5")?,
                &[],
            )?,
            expected
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest("SELECT payload FROM events LIMIT 0")?,
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["payload".to_owned()],
                rows: Vec::new(),
            }
        );
        assert!(matches!(
            database
                .prepare_sql_latest("SELECT payload FROM events ORDER BY sequence, tenant LIMIT 5"),
            Err(SqlError::InvalidPrimaryKey)
        ));
        assert!(matches!(
            database.execute_prepared_latest(&physical_plan, &[SqlValue::Signed(1)]),
            Err(SqlError::ParameterMismatch)
        ));

        let mut catalog_change = database.begin_sql(13, DurabilityClass::Strict)?;
        catalog_change.execute_sql("CREATE TABLE audit (id BIGINT PRIMARY KEY)", &[])?;
        catalog_change.commit()?;
        assert!(matches!(
            database.execute_prepared_latest(&physical_plan, &[]),
            Err(SqlError::CatalogChanged)
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(query)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &[])?,
            expected
        );
        Ok(())
    }

    const COMPOSITE_RANGE_QUERY: &str = "SELECT sequence, payload
        FROM events
        WHERE (tenant, sequence) >= (?, ?)
          AND (tenant, sequence) < (?, ?)
        ORDER BY tenant, sequence
        LIMIT 16";

    fn composite_range_parameters() -> [SqlValue; 4] {
        [
            SqlValue::Signed(1),
            SqlValue::Signed(2),
            SqlValue::Signed(1),
            SqlValue::Signed(12),
        ]
    }

    fn reversed_composite_range_parameters() -> [SqlValue; 4] {
        [
            SqlValue::Signed(1),
            SqlValue::Signed(12),
            SqlValue::Signed(1),
            SqlValue::Signed(2),
        ]
    }

    fn expected_composite_range() -> SqlResult {
        SqlResult::Rows {
            columns: vec!["sequence".to_owned(), "payload".to_owned()],
            rows: vec![
                vec![SqlValue::Signed(2), SqlValue::Text("updated".to_owned())],
                vec![SqlValue::Signed(4), SqlValue::Text("event-0004".to_owned())],
                vec![SqlValue::Signed(5), SqlValue::Text("event-0005".to_owned())],
                vec![SqlValue::Signed(6), SqlValue::Text("event-0006".to_owned())],
                vec![
                    SqlValue::Signed(10),
                    SqlValue::Text("event-0010".to_owned()),
                ],
                vec![
                    SqlValue::Signed(11),
                    SqlValue::Text("event-0011".to_owned()),
                ],
            ],
        }
    }

    fn empty_composite_range() -> SqlResult {
        SqlResult::Rows {
            columns: vec!["sequence".to_owned(), "payload".to_owned()],
            rows: Vec::new(),
        }
    }

    fn assert_transaction_composite_range(
        database: &mut NativeDatabase,
        table: ObjectId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut transaction = database.begin_sql(12, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "INSERT INTO events (tenant, sequence, payload) VALUES (?, ?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Signed(512),
                SqlValue::Text("private".to_owned()),
            ],
        )?;
        assert_eq!(
            transaction.execute_sql(
                "EXPLAIN SELECT sequence FROM events
                 WHERE (tenant, sequence) > (?, ?)
                   AND (tenant, sequence) <= (?, ?)
                 ORDER BY tenant, sequence
                 LIMIT 3",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["plan".to_owned()],
                rows: vec![vec![SqlValue::Text(format!(
                    "PrimaryKeyRangeScan(table={table},lower=exclusive,\
                     upper=inclusive,limit=3)"
                ))]],
            }
        );
        assert_eq!(
            transaction.execute_sql(
                "SELECT sequence, payload
                 FROM events
                 WHERE (tenant, sequence) > (?, ?)
                   AND (tenant, sequence) <= (?, ?)
                 ORDER BY tenant, sequence
                 LIMIT 3",
                &[
                    SqlValue::Signed(1),
                    SqlValue::Signed(510),
                    SqlValue::Signed(1),
                    SqlValue::Signed(512),
                ],
            )?,
            SqlResult::Rows {
                columns: vec!["sequence".to_owned(), "payload".to_owned()],
                rows: vec![
                    vec![
                        SqlValue::Signed(511),
                        SqlValue::Text("event-0511".to_owned()),
                    ],
                    vec![SqlValue::Signed(512), SqlValue::Text("private".to_owned()),],
                ],
            }
        );
        assert_eq!(
            transaction.execute_sql(
                COMPOSITE_RANGE_QUERY,
                &reversed_composite_range_parameters(),
            )?,
            empty_composite_range()
        );
        transaction.rollback();
        Ok(())
    }

    fn assert_materialized_composite_range(
        database: &NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let materialized = database.snapshot(13)?;
        let materialized_plan = materialized.prepare_sql(COMPOSITE_RANGE_QUERY)?;
        assert_eq!(
            materialized.execute_prepared(&materialized_plan, &composite_range_parameters())?,
            expected_composite_range()
        );
        assert_eq!(
            materialized
                .execute_prepared(&materialized_plan, &reversed_composite_range_parameters(),)?,
            empty_composite_range()
        );
        Ok(())
    }

    fn assert_latest_composite_range(
        database: &NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let physical_plan = database.prepare_sql_latest(COMPOSITE_RANGE_QUERY)?;
        assert_eq!(
            database.execute_prepared_latest(&physical_plan, &composite_range_parameters())?,
            expected_composite_range()
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest(
                    "SELECT sequence FROM events
                     WHERE (tenant, sequence) > (?, ?)
                       AND (tenant, sequence) <= (?, ?)
                     ORDER BY tenant, sequence
                     LIMIT 16"
                )?,
                &[
                    SqlValue::Signed(1),
                    SqlValue::Signed(2),
                    SqlValue::Signed(1),
                    SqlValue::Signed(10),
                ],
            )?,
            SqlResult::Rows {
                columns: vec!["sequence".to_owned()],
                rows: vec![
                    vec![SqlValue::Signed(4)],
                    vec![SqlValue::Signed(5)],
                    vec![SqlValue::Signed(6)],
                    vec![SqlValue::Signed(10)],
                ],
            }
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest(
                    "SELECT sequence FROM events
                     WHERE (tenant, sequence) >= (?, ?)
                     ORDER BY tenant, sequence
                     LIMIT 4"
                )?,
                &[SqlValue::Signed(1), SqlValue::Signed(510)],
            )?,
            SqlResult::Rows {
                columns: vec!["sequence".to_owned()],
                rows: vec![vec![SqlValue::Signed(510)], vec![SqlValue::Signed(511)],],
            }
        );
        assert_eq!(
            database
                .execute_prepared_latest(&physical_plan, &reversed_composite_range_parameters(),)?,
            empty_composite_range()
        );
        assert!(matches!(
            database.prepare_sql_latest(
                "SELECT payload FROM events
                 WHERE (tenant, sequence) >= (?, ?)
                 ORDER BY tenant, sequence"
            ),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            database.prepare_sql_latest(
                "SELECT payload FROM events
                 WHERE (tenant, sequence) >= (?, ?)
                 ORDER BY sequence, tenant
                 LIMIT 5"
            ),
            Err(SqlError::InvalidPrimaryKey)
        ));
        assert!(matches!(
            database.execute_prepared_latest(
                &physical_plan,
                &[
                    SqlValue::Signed(1),
                    SqlValue::Null,
                    SqlValue::Signed(1),
                    SqlValue::Signed(12),
                ],
            ),
            Err(SqlError::InvalidPrimaryKey)
        ));
        let parameters = composite_range_parameters();
        assert!(matches!(
            database.execute_prepared_latest(&physical_plan, &parameters[..3]),
            Err(SqlError::ParameterMismatch)
        ));
        Ok(())
    }

    #[test]
    fn bounded_composite_primary_key_range_matches_every_executor()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = seed_bounded_sql_scan(&mut database)?;
        mutate_bounded_sql_scan(&mut database)?;
        assert_transaction_composite_range(&mut database, table)?;
        assert_materialized_composite_range(&database)?;
        assert_latest_composite_range(&database)?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(COMPOSITE_RANGE_QUERY)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &composite_range_parameters())?,
            expected_composite_range()
        );
        Ok(())
    }

    const RESIDUAL_RANGE_QUERY: &str = "SELECT id, note
        FROM events
        WHERE id >= ?
          AND active = ?
          AND (note IS NULL OR score > ?)
        ORDER BY id
        LIMIT 2";

    fn expected_residual_range() -> SqlResult {
        SqlResult::Rows {
            columns: vec!["id".to_owned(), "note".to_owned()],
            rows: vec![
                vec![SqlValue::Signed(3), SqlValue::Null],
                vec![SqlValue::Signed(5), SqlValue::Null],
            ],
        }
    }

    fn seed_residual_filter_rows(
        database: &mut NativeDatabase,
    ) -> Result<(ObjectId, ObjectId), Box<dyn std::error::Error>> {
        let mut seed = database.begin_sql(20, DurabilityClass::Strict)?;
        let created = seed.execute_sql(
            "CREATE TABLE events (
                id BIGINT PRIMARY KEY,
                tenant TEXT NOT NULL,
                score BIGINT NOT NULL,
                note TEXT NULL,
                active BOOLEAN NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing residual-filter table identity".into());
        };
        let rows = [
            (1_i64, "alpha", 10_i64, SqlValue::Null, false),
            (2, "alpha", 20, SqlValue::Text("Mario's".to_owned()), false),
            (3, "alpha", 30, SqlValue::Null, true),
            (4, "beta", 40, SqlValue::Text("keep".to_owned()), false),
            (5, "beta", 50, SqlValue::Null, true),
            (6, "beta", 60, SqlValue::Text("keep".to_owned()), true),
            (7, "beta", 70, SqlValue::Null, false),
        ];
        for (id, tenant, score, note, active) in rows {
            seed.execute_sql(
                "INSERT INTO events (id, tenant, score, note, active)
                 VALUES (?, ?, ?, ?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(tenant.to_owned()),
                    SqlValue::Signed(score),
                    note,
                    SqlValue::Boolean(active),
                ],
            )?;
        }
        let created = seed.execute_sql("CREATE INDEX events_tenant ON events (tenant)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing residual-filter index identity".into());
        };
        seed.commit()?;
        Ok((table, index))
    }

    fn residual_range_parameters() -> [SqlValue; 3] {
        [
            SqlValue::Signed(2),
            SqlValue::Boolean(true),
            SqlValue::Signed(45),
        ]
    }

    fn assert_private_residual_filters(
        database: &mut NativeDatabase,
        table: ObjectId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut private = database.begin_sql(21, DurabilityClass::Strict)?;
        private.execute_sql(
            "INSERT INTO events (id, tenant, score, note, active)
             VALUES (?, ?, ?, ?, ?)",
            &[
                SqlValue::Signed(8),
                SqlValue::Text("beta".to_owned()),
                SqlValue::Signed(80),
                SqlValue::Null,
                SqlValue::Boolean(true),
            ],
        )?;
        assert_eq!(
            private.execute_sql(RESIDUAL_RANGE_QUERY, &residual_range_parameters())?,
            expected_residual_range()
        );
        for (statement, expected) in [
            (
                "EXPLAIN SELECT id FROM events
                 WHERE id >= ? AND active = ?
                 ORDER BY id LIMIT 2",
                format!(
                    "PrimaryKeyRangeScan(table={table},lower=inclusive,\
                     upper=unbounded,limit=2,residual=true)"
                ),
            ),
            (
                "EXPLAIN SELECT id FROM events WHERE id = ? AND active = ?",
                format!("PrimaryKeyLookup(table={table},residual=true)"),
            ),
        ] {
            assert_eq!(
                private.execute_sql(statement, &[])?,
                SqlResult::Rows {
                    columns: vec!["plan".to_owned()],
                    rows: vec![vec![SqlValue::Text(expected)]],
                }
            );
        }
        private.rollback();
        Ok(())
    }

    fn assert_latest_residual_semantics(
        database: &NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (
                "SELECT id FROM events
                 WHERE NOT active = ? AND note IS NOT NULL
                 ORDER BY id LIMIT 2",
                vec![SqlValue::Boolean(true)],
                vec![vec![SqlValue::Signed(2)], vec![SqlValue::Signed(4)]],
            ),
            (
                "SELECT id FROM events WHERE note = ? OR note IS NULL LIMIT 10",
                vec![SqlValue::Null],
                vec![
                    vec![SqlValue::Signed(1)],
                    vec![SqlValue::Signed(3)],
                    vec![SqlValue::Signed(5)],
                    vec![SqlValue::Signed(7)],
                ],
            ),
            (
                "SELECT id FROM events WHERE tenant = ? AND active = ?",
                vec![SqlValue::Text("alpha".to_owned()), SqlValue::Boolean(true)],
                vec![vec![SqlValue::Signed(3)]],
            ),
            (
                "SELECT id FROM events WHERE id = ? AND note = ?",
                vec![SqlValue::Signed(3), SqlValue::Null],
                Vec::new(),
            ),
        ];
        for (statement, parameters, rows) in cases {
            let plan = database.prepare_sql_latest(statement)?;
            assert_eq!(
                database.execute_prepared_latest(&plan, &parameters)?,
                SqlResult::Rows {
                    columns: vec!["id".to_owned()],
                    rows,
                }
            );
        }
        Ok(())
    }

    fn assert_residual_filter_failures(
        database: &NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let plan = database.prepare_sql_latest(RESIDUAL_RANGE_QUERY)?;
        assert!(matches!(
            database.execute_prepared_latest(
                &plan,
                &[
                    SqlValue::Signed(2),
                    SqlValue::Boolean(true),
                    SqlValue::Text("wrong".to_owned()),
                ],
            ),
            Err(SqlError::TypeMismatch)
        ));
        assert!(matches!(
            database.execute_prepared_latest(&plan, &residual_range_parameters()[..2]),
            Err(SqlError::ParameterMismatch)
        ));
        assert!(matches!(
            database
                .prepare_sql_latest("SELECT id FROM events WHERE missing = ? ORDER BY id LIMIT 1"),
            Err(SqlError::UnknownColumn)
        ));
        Ok(())
    }

    #[test]
    fn typed_residual_filters_preserve_three_valued_logic_across_executors()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let (table, _) = seed_residual_filter_rows(&mut database)?;
        let parameters = residual_range_parameters();
        let expected = expected_residual_range();

        assert_private_residual_filters(&mut database, table)?;

        let materialized = database.snapshot(22)?;
        let materialized_plan = materialized.prepare_sql(RESIDUAL_RANGE_QUERY)?;
        assert_eq!(
            materialized.execute_prepared(&materialized_plan, &parameters)?,
            expected
        );
        let latest_plan = database.prepare_sql_latest(RESIDUAL_RANGE_QUERY)?;
        assert_eq!(
            database.execute_prepared_latest(&latest_plan, &parameters)?,
            expected
        );
        assert_latest_residual_semantics(&database)?;
        assert_residual_filter_failures(&database)?;

        drop(database);
        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(RESIDUAL_RANGE_QUERY)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &parameters)?,
            expected
        );
        Ok(())
    }

    fn assert_literal_access_plans(
        database: &mut NativeDatabase,
        table: ObjectId,
        index: ObjectId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut explain = database.begin_sql(21, DurabilityClass::Strict)?;
        for (statement, expected) in [
            (
                "EXPLAIN SELECT id FROM events
                 WHERE id >= 2 AND active = TRUE
                 ORDER BY id LIMIT 2",
                format!(
                    "PrimaryKeyRangeScan(table={table},lower=inclusive,\
                     upper=unbounded,limit=2,residual=true)"
                ),
            ),
            (
                "EXPLAIN SELECT id FROM events
                 WHERE tenant = 'alpha' AND active = TRUE",
                format!("SecondaryIndexLookup(table={table},index={index},residual=true)"),
            ),
        ] {
            assert_eq!(
                explain.execute_sql(statement, &[])?,
                SqlResult::Rows {
                    columns: vec!["plan".to_owned()],
                    rows: vec![vec![SqlValue::Text(expected)]],
                }
            );
        }
        explain.rollback();
        Ok(())
    }

    #[test]
    fn typed_filter_literals_bind_to_catalog_and_reuse_physical_access()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let (table, index) = seed_residual_filter_rows(&mut database)?;

        let literal_range = database.prepare_sql_latest(
            "SELECT id, note FROM events
             WHERE id >= 2
               AND active = TRUE
               AND (note IS NULL OR score > 45)
             ORDER BY id LIMIT 2",
        )?;
        assert_eq!(
            database.execute_prepared_latest(&literal_range, &[])?,
            expected_residual_range()
        );
        let materialized = database.snapshot(22)?;
        let materialized_plan = materialized.prepare_sql(
            "SELECT id, note FROM events
             WHERE id >= 2
               AND active = TRUE
               AND (note IS NULL OR score > 45)
             ORDER BY id LIMIT 2",
        )?;
        assert_eq!(
            materialized.execute_prepared(&materialized_plan, &[])?,
            expected_residual_range()
        );
        assert_literal_access_plans(&mut database, table, index)?;
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest(
                    "SELECT id FROM events WHERE tenant = 'alpha' AND active = TRUE"
                )?,
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(3)]],
            }
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest(
                    "SELECT id FROM events WHERE note = 'Mario''s' ORDER BY id LIMIT 1"
                )?,
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(2)]],
            }
        );
        assert_eq!(
            database.execute_prepared_latest(
                &database.prepare_sql_latest("SELECT id FROM events WHERE id = NULL")?,
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: Vec::new(),
            }
        );
        assert!(matches!(
            database.prepare_sql_latest("SELECT id FROM events WHERE active = 1 LIMIT 1"),
            Err(SqlError::TypeMismatch)
        ));
        assert!(matches!(
            database.prepare_sql_latest("SELECT id FROM events WHERE id = '2'"),
            Err(SqlError::TypeMismatch)
        ));
        assert!(matches!(
            database.prepare_sql_latest("SELECT id FROM events WHERE id = 9223372036854775808"),
            Err(SqlError::TypeMismatch)
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(
            "SELECT id FROM events
             WHERE id >= -2 AND active = TRUE
             ORDER BY id LIMIT 2",
        )?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &[])?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(3)], vec![SqlValue::Signed(5)],],
            }
        );
        Ok(())
    }

    #[test]
    fn physical_secondary_index_lookup_rejects_malformed_live_marker()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL
            )",
            &[],
        )?;
        transaction.execute_sql(
            "INSERT INTO people (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Text("mario@celiums.test".to_owned()),
            ],
        )?;
        let created =
            transaction.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing secondary-index identity".into());
        };
        transaction.commit()?;

        let index_key = SqlValue::Text("mario@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let primary_key = SqlValue::Signed(1)
            .encode_ordered_component(&LogicalType::Signed(IntegerWidth::Bits64))?;
        let identity = super::secondary_index_entry_identity(&index_key, &primary_key)?;
        let physical_key = super::relational_secondary_entry_key(index, &identity)?;
        let roots = database.coordinator.snapshot(11)?.roots().clone();
        let root = roots
            .root(super::SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let visible_csn = roots
            .visible_csn()
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let forged_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(&mut database.pages, visible_csn, physical_key, vec![9])?
            .tree;
        let mut forged_roots = roots
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        forged_roots.insert(
            super::SLOT_RELATIONAL,
            forged_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            visible_csn,
            roots.catalog_version(),
            roots
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            forged_roots,
            roots.blob_generation(),
        )?;
        database.coordinator = hyphae_native_mvcc::CommitCoordinator::restore(forged)?;

        assert!(matches!(
            database.select_latest_secondary_index(index, &index_key),
            Err(NativeRuntimeError::InvalidRelationalTree)
        ));
        Ok(())
    }

    #[test]
    fn composite_secondary_index_binds_predicates_in_catalog_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE events (
                id BIGINT PRIMARY KEY,
                tenant TEXT NOT NULL,
                sequence BIGINT NOT NULL,
                payload TEXT NOT NULL
            )",
            &[],
        )?;
        transaction.execute_sql(
            "INSERT INTO events (id, tenant, sequence, payload) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Text("celiums".to_owned()),
                SqlValue::Signed(42),
                SqlValue::Text("native".to_owned()),
            ],
        )?;
        transaction.execute_sql(
            "CREATE INDEX events_tenant_sequence ON events (tenant, sequence)",
            &[],
        )?;
        let query = "SELECT payload FROM events WHERE sequence = ? AND tenant = ?";
        let parameters = [SqlValue::Signed(42), SqlValue::Text("celiums".to_owned())];
        let expected = SqlResult::Rows {
            columns: vec!["payload".to_owned()],
            rows: vec![vec![SqlValue::Text("native".to_owned())]],
        };
        assert_eq!(transaction.execute_sql(query, &parameters)?, expected);
        assert!(matches!(
            transaction.execute_sql(query, &[SqlValue::Null, SqlValue::Signed(7)]),
            Err(SqlError::TypeMismatch)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "SELECT payload FROM events WHERE tenant = ?",
                &[SqlValue::Text("celiums".to_owned())],
            ),
            Err(SqlError::NoAccessPath)
        ));
        transaction.commit()?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let snapshot = reopened.snapshot(11)?;
        let prepared = snapshot.prepare_sql(query)?;
        assert_eq!(snapshot.execute_prepared(&prepared, &parameters)?, expected);
        Ok(())
    }

    #[test]
    fn unique_secondary_index_is_statement_atomic_and_nulls_are_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT
            )",
            &[],
        )?;
        for id in [1, 2] {
            transaction.execute_sql(
                "INSERT INTO users (id, email) VALUES (?, ?)",
                &[SqlValue::Signed(id), SqlValue::Null],
            )?;
        }
        transaction.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        assert_eq!(
            transaction.execute_sql("SELECT id FROM users WHERE email = ?", &[SqlValue::Null],)?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: Vec::new(),
            }
        );
        transaction.execute_sql(
            "INSERT INTO users (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(3),
                SqlValue::Text("mario@celiums.test".to_owned()),
            ],
        )?;
        assert!(matches!(
            transaction.execute_sql(
                "INSERT INTO users (id, email) VALUES (?, ?)",
                &[
                    SqlValue::Signed(4),
                    SqlValue::Text("mario@celiums.test".to_owned()),
                ],
            ),
            Err(SqlError::UniqueViolation)
        ));
        assert_eq!(
            transaction.execute_sql(
                "SELECT id FROM users WHERE email = ?",
                &[SqlValue::Text("mario@celiums.test".to_owned())],
            )?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(3)]],
            }
        );
        transaction.commit()?;
        drop(database);
        NativeDatabase::open(temporary.path())?;
        Ok(())
    }

    #[test]
    fn unique_secondary_index_backfill_failure_is_statement_atomic()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        assert_eq!(table, ObjectId::new(1)?);
        for id in [1, 2] {
            transaction.execute_sql(
                "INSERT INTO users (id, email) VALUES (?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text("duplicate@celiums.test".to_owned()),
                ],
            )?;
        }
        assert!(matches!(
            transaction.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[]),
            Err(SqlError::UniqueViolation)
        ));
        assert!(matches!(
            transaction.execute_sql("EXPLAIN SELECT id FROM users WHERE email = ?", &[]),
            Err(SqlError::NoAccessPath)
        ));
        transaction.commit()?;

        let missing_index = ObjectId::new(2)?;
        let snapshot = database.snapshot(11)?;
        assert!(snapshot.catalog_object(missing_index).is_none());
        assert!(matches!(
            snapshot.prepare_sql("SELECT id FROM users WHERE email = ?"),
            Err(SqlError::NoAccessPath)
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let recovered = reopened.snapshot(11)?;
        assert!(recovered.catalog_object(missing_index).is_none());
        assert!(matches!(
            recovered.prepare_sql("SELECT id FROM users WHERE email = ?"),
            Err(SqlError::NoAccessPath)
        ));
        Ok(())
    }

    fn seed_mutable_secondary_indexes(
        database: &mut NativeDatabase,
    ) -> Result<(ObjectId, ObjectId, ObjectId), Box<dyn std::error::Error>> {
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT,
                city TEXT NOT NULL,
                name TEXT NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing mutable table identity".into());
        };
        for (id, email, name) in [
            (1, "mario@celiums.test", "Mario"),
            (2, "romina@celiums.test", "Romina"),
        ] {
            transaction.execute_sql(
                "INSERT INTO people (id, email, city, name) VALUES (?, ?, ?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(email.to_owned()),
                    SqlValue::Text("Medellin".to_owned()),
                    SqlValue::Text(name.to_owned()),
                ],
            )?;
        }
        let created =
            transaction.execute_sql("CREATE UNIQUE INDEX people_email ON people (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(email_index),
            ..
        } = created
        else {
            return Err("missing email index identity".into());
        };
        let created = transaction.execute_sql("CREATE INDEX people_city ON people (city)", &[])?;
        let SqlResult::Command {
            object_id: Some(city_index),
            ..
        } = created
        else {
            return Err("missing city index identity".into());
        };
        transaction.commit()?;
        Ok((table, email_index, city_index))
    }

    fn command_rows(rows_affected: u64) -> SqlResult {
        SqlResult::Command {
            rows_affected,
            object_id: None,
        }
    }

    fn ordered_i64(value: i64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        Ok(SqlValue::Signed(value)
            .encode_ordered_component(&LogicalType::Signed(IntegerWidth::Bits64))?)
    }

    fn assert_indexed_update_rejections(
        database: &mut NativeDatabase,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut transaction = database.begin_sql(13, DurabilityClass::Strict)?;
        assert!(matches!(
            transaction.execute_sql(
                "UPDATE people SET email = ? WHERE id = ?",
                &[
                    SqlValue::Text("romina@celiums.test".to_owned()),
                    SqlValue::Signed(1),
                ],
            ),
            Err(SqlError::UniqueViolation)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "UPDATE people SET id = ? WHERE id = ?",
                &[SqlValue::Signed(3), SqlValue::Signed(1)],
            ),
            Err(SqlError::PrimaryKeyMutationUnsupported)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "UPDATE people SET city = ? WHERE id = ?",
                &[SqlValue::Null, SqlValue::Signed(1)],
            ),
            Err(SqlError::NullViolation)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "UPDATE people SET name = ?, name = ? WHERE id = ?",
                &[
                    SqlValue::Text("one".to_owned()),
                    SqlValue::Text("two".to_owned()),
                    SqlValue::Signed(1),
                ],
            ),
            Err(SqlError::DuplicateColumn)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "UPDATE people SET email = ? WHERE id = ?",
                &[SqlValue::Signed(7), SqlValue::Signed(1)],
            ),
            Err(SqlError::TypeMismatch)
        ));
        assert_eq!(
            transaction.execute_sql(
                "UPDATE people SET name = ? WHERE id = ?",
                &[SqlValue::Text("missing".to_owned()), SqlValue::Signed(99),],
            )?,
            command_rows(0)
        );
        assert_eq!(
            transaction.execute_sql(
                "SELECT name FROM people WHERE email = ?",
                &[SqlValue::Text("founder@celiums.test".to_owned())],
            )?,
            SqlResult::Rows {
                columns: vec!["name".to_owned()],
                rows: vec![vec![SqlValue::Text("Mr. Mario".to_owned())]],
            }
        );
        transaction.rollback();
        Ok(())
    }

    #[test]
    fn typed_update_delete_maintain_secondary_indexes_history_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let (table, email_index, city_index) = seed_mutable_secondary_indexes(&mut database)?;
        let historical = database.snapshot(11)?;
        let historical_plan = historical.prepare_sql("SELECT name FROM people WHERE email = ?")?;

        let mut update = database.begin_sql(12, DurabilityClass::Strict)?;
        assert_eq!(
            update.execute_sql(
                "UPDATE people SET name = ?, email = ?, city = ? WHERE id = ?",
                &[
                    SqlValue::Text("Mr. Mario".to_owned()),
                    SqlValue::Text("founder@celiums.test".to_owned()),
                    SqlValue::Text("Bogota".to_owned()),
                    SqlValue::Signed(1),
                ],
            )?,
            SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            }
        );
        update.commit()?;

        let old_email = SqlValue::Text("mario@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let new_email = SqlValue::Text("founder@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let medellin =
            SqlValue::Text("Medellin".to_owned()).encode_ordered_component(&LogicalType::Text)?;
        let bogota =
            SqlValue::Text("Bogota".to_owned()).encode_ordered_component(&LogicalType::Text)?;
        assert!(
            database
                .select_latest_secondary_index(email_index, &old_email)?
                .is_empty()
        );
        assert_eq!(
            database
                .select_latest_secondary_index(email_index, &new_email)?
                .len(),
            1
        );
        assert_eq!(
            database
                .select_latest_secondary_index(city_index, &medellin)?
                .len(),
            1
        );
        assert_eq!(
            database
                .select_latest_secondary_index(city_index, &bogota)?
                .len(),
            1
        );
        assert_eq!(
            historical.execute_prepared(
                &historical_plan,
                &[SqlValue::Text("mario@celiums.test".to_owned())],
            )?,
            SqlResult::Rows {
                columns: vec!["name".to_owned()],
                rows: vec![vec![SqlValue::Text("Mario".to_owned())]],
            }
        );

        assert_indexed_update_rejections(&mut database)?;
        let mut delete = database.begin_sql(14, DurabilityClass::Strict)?;
        assert_eq!(
            delete.execute_sql("DELETE FROM people WHERE id = ?", &[SqlValue::Signed(1)])?,
            command_rows(1)
        );
        delete.commit()?;
        assert!(
            database
                .select_latest_secondary_index(email_index, &new_email)?
                .is_empty()
        );
        assert!(
            database
                .select_latest_secondary_index(city_index, &bogota)?
                .is_empty()
        );
        assert_eq!(
            database.select_latest_relational(table, &ordered_i64(1)?)?,
            None
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert!(
            reopened
                .select_latest_secondary_index(email_index, &new_email)?
                .is_empty()
        );
        assert_eq!(
            reopened
                .select_latest_secondary_index(city_index, &medellin)?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn optimistic_indexed_updates_recheck_unique_keys_on_admitted_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let (_, email_index, _) = seed_mutable_secondary_indexes(&mut database)?;
        let mut first = database.begin_optimistic(12, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(12, DurabilityClass::Strict)?;
        for (batch, id) in [(&mut first, 1), (&mut second, 2)] {
            assert_eq!(
                batch.execute_sql(
                    "UPDATE people SET email = ? WHERE id = ?",
                    &[
                        SqlValue::Text("shared@celiums.test".to_owned()),
                        SqlValue::Signed(id),
                    ],
                )?,
                command_rows(1)
            );
        }
        database.commit_optimistic(first)?;
        assert!(matches!(
            database.commit_optimistic(second),
            Err(NativeRuntimeError::UniqueSecondaryIndexViolation)
        ));

        let shared = SqlValue::Text("shared@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let first_old = SqlValue::Text("mario@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let second_old = SqlValue::Text("romina@celiums.test".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        assert_eq!(
            database
                .select_latest_secondary_index(email_index, &shared)?
                .len(),
            1
        );
        assert!(
            database
                .select_latest_secondary_index(email_index, &first_old)?
                .is_empty()
        );
        assert_eq!(
            database
                .select_latest_secondary_index(email_index, &second_old)?
                .len(),
            1
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened
                .select_latest_secondary_index(email_index, &shared)?
                .len(),
            1
        );
        assert_eq!(
            reopened
                .select_latest_secondary_index(email_index, &second_old)?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn typed_mutation_binds_composite_primary_key_in_predicate_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE events (
                tenant TEXT NOT NULL,
                sequence BIGINT NOT NULL,
                code TEXT NOT NULL,
                payload TEXT,
                PRIMARY KEY (tenant, sequence)
            )",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO events (tenant, sequence, code, payload) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::Text("celiums".to_owned()),
                SqlValue::Signed(42),
                SqlValue::Text("old".to_owned()),
                SqlValue::Text("before".to_owned()),
            ],
        )?;
        let created = seed.execute_sql("CREATE UNIQUE INDEX events_code ON events (code)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing composite mutation index".into());
        };
        seed.commit()?;

        let mut update = database.begin_sql(11, DurabilityClass::Strict)?;
        assert_eq!(
            update.execute_sql(
                "UPDATE events SET code = ?, payload = ?
                 WHERE sequence = ? AND tenant = ?",
                &[
                    SqlValue::Text("new".to_owned()),
                    SqlValue::Null,
                    SqlValue::Signed(42),
                    SqlValue::Text("celiums".to_owned()),
                ],
            )?,
            command_rows(1)
        );
        update.commit()?;
        let old = SqlValue::Text("old".to_owned()).encode_ordered_component(&LogicalType::Text)?;
        let new = SqlValue::Text("new".to_owned()).encode_ordered_component(&LogicalType::Text)?;
        assert!(
            database
                .select_latest_secondary_index(index, &old)?
                .is_empty()
        );
        assert_eq!(
            database.select_latest_secondary_index(index, &new)?.len(),
            1
        );

        let mut delete = database.begin_sql(12, DurabilityClass::Strict)?;
        assert_eq!(
            delete.execute_sql(
                "DELETE FROM events WHERE sequence = ? AND tenant = ?",
                &[SqlValue::Signed(42), SqlValue::Text("celiums".to_owned()),],
            )?,
            command_rows(1)
        );
        delete.commit()?;
        assert!(
            database
                .select_latest_secondary_index(index, &new)?
                .is_empty()
        );
        drop(database);
        NativeDatabase::open(temporary.path())?;
        Ok(())
    }

    fn seed_literal_mutation_users(
        database: &mut NativeDatabase,
    ) -> Result<ObjectId, Box<dyn std::error::Error>> {
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL,
                note TEXT,
                active BOOLEAN NOT NULL
            )",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO users (id, email, note, active)
             VALUES (1, 'old@hyphae.local', NULL, FALSE)",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO users (id, email, note, active)
             VALUES (?, 'two@hyphae.local', 'second', TRUE)",
            &[SqlValue::Signed(2)],
        )?;
        let created = seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing literal-mutation index".into());
        };
        seed.commit()?;
        Ok(index)
    }

    fn assert_literal_mutation_failures(
        mutation: &mut super::NativeWriteBatch,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            mutation.execute_sql(
                "UPDATE users
                 SET email = 'must-not-persist@hyphae.local', active = 1
                 WHERE id = 1",
                &[],
            ),
            Err(SqlError::TypeMismatch)
        ));
        assert_eq!(
            mutation.execute_sql("SELECT id FROM users WHERE email = 'old@hyphae.local'", &[],)?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(1)]],
            }
        );
        assert!(matches!(
            mutation.execute_sql(
                "INSERT INTO users (id, email, note, active)
                 VALUES (?, 'bad@hyphae.local', NULL, TRUE)",
                &[],
            ),
            Err(SqlError::ParameterMismatch)
        ));
        assert!(matches!(
            mutation.execute_sql(
                "INSERT INTO users (id, email, note, active)
                 VALUES (3, NULL, NULL, TRUE)",
                &[],
            ),
            Err(SqlError::NullViolation)
        ));
        assert!(matches!(
            mutation.execute_sql(
                "INSERT INTO users (id, email, note, active)
                 VALUES (9223372036854775808, 'overflow@hyphae.local', NULL, TRUE)",
                &[],
            ),
            Err(SqlError::TypeMismatch)
        ));
        Ok(())
    }

    #[test]
    fn typed_mutation_literals_preserve_indexes_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = seed_literal_mutation_users(&mut database)?;

        let mut mutation = database.begin_sql(11, DurabilityClass::Strict)?;
        assert_literal_mutation_failures(&mut mutation)?;
        assert_eq!(
            mutation.execute_sql(
                "UPDATE users
                 SET email = 'new@hyphae.local', note = 'Mario''s', active = TRUE
                 WHERE id = ?",
                &[SqlValue::Signed(1)],
            )?,
            command_rows(1)
        );
        assert_eq!(
            mutation.execute_sql("DELETE FROM users WHERE id = 2", &[])?,
            command_rows(1)
        );
        mutation.commit()?;

        let old = SqlValue::Text("old@hyphae.local".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let new = SqlValue::Text("new@hyphae.local".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        assert!(
            database
                .select_latest_secondary_index(index, &old)?
                .is_empty()
        );
        assert_eq!(
            database.select_latest_secondary_index(index, &new)?.len(),
            1
        );
        let expected = SqlResult::Rows {
            columns: vec!["id".to_owned(), "note".to_owned()],
            rows: vec![vec![
                SqlValue::Signed(1),
                SqlValue::Text("Mario's".to_owned()),
            ]],
        };
        let plan = database.prepare_sql_latest(
            "SELECT id, note FROM users
             WHERE email = 'new@hyphae.local' AND active = TRUE",
        )?;
        assert_eq!(database.execute_prepared_latest(&plan, &[])?, expected);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(
            "SELECT id, note FROM users
             WHERE email = 'new@hyphae.local' AND active = TRUE",
        )?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &[])?,
            expected
        );
        assert_eq!(
            reopened.select_latest_secondary_index(index, &new)?.len(),
            1
        );
        Ok(())
    }

    fn indexed_join_query() -> &'static str {
        "SELECT users.id, users.name, profiles.city
         FROM users
         INNER JOIN profiles ON users.profile_id = profiles.id
         WHERE email = ?"
    }

    fn indexed_join_columns() -> Vec<String> {
        vec![
            "users.id".to_owned(),
            "users.name".to_owned(),
            "profiles.city".to_owned(),
        ]
    }

    fn seed_indexed_join(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL,
                name TEXT NOT NULL,
                profile_id BIGINT
            )",
            &[],
        )?;
        seed.execute_sql(
            "CREATE TABLE profiles (
                id BIGINT PRIMARY KEY,
                city TEXT NOT NULL
            )",
            &[],
        )?;
        for values in [
            "(1, 'one@hyphae.local', 'Mario', 100)",
            "(2, 'two@hyphae.local', 'Romina', NULL)",
            "(3, 'three@hyphae.local', 'Luciana', 999)",
            "(4, 'four@hyphae.local', 'Genesis', 400)",
            "(5, 'five@hyphae.local', 'Suli', 500)",
        ] {
            seed.execute_sql(
                &format!("INSERT INTO users (id, email, name, profile_id) VALUES {values}"),
                &[],
            )?;
        }
        seed.execute_sql(
            "INSERT INTO profiles (id, city) VALUES (100, 'Medellin')",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO profiles (id, city) VALUES (400, 'Caracas')",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO profiles (id, city) VALUES (500, 'San Cristobal')",
            &[],
        )?;
        seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        seed.commit()?;
        Ok(())
    }

    #[test]
    fn indexed_inner_join_matches_private_snapshot_latest_and_reopen()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        seed_indexed_join(&mut database)?;
        let historical = database.snapshot(11)?;
        let historical_plan = historical.prepare_sql(indexed_join_query())?;
        let expected_medellin = SqlResult::Rows {
            columns: indexed_join_columns(),
            rows: vec![vec![
                SqlValue::Signed(1),
                SqlValue::Text("Mario".to_owned()),
                SqlValue::Text("Medellin".to_owned()),
            ]],
        };
        let mario = [SqlValue::Text("one@hyphae.local".to_owned())];
        assert_eq!(
            historical.execute_prepared(&historical_plan, &mario)?,
            expected_medellin
        );
        let latest_plan = database.prepare_sql_latest(indexed_join_query())?;
        assert_eq!(
            database.execute_prepared_latest(&latest_plan, &mario)?,
            expected_medellin
        );
        for email in [
            "two@hyphae.local",
            "three@hyphae.local",
            "absent@hyphae.local",
        ] {
            assert_eq!(
                database
                    .execute_prepared_latest(&latest_plan, &[SqlValue::Text(email.to_owned())],)?,
                SqlResult::Rows {
                    columns: indexed_join_columns(),
                    rows: Vec::new(),
                }
            );
        }

        let mut update = database.begin_sql(12, DurabilityClass::Strict)?;
        update.execute_sql("UPDATE profiles SET city = 'Envigado' WHERE id = 100", &[])?;
        let expected_envigado = SqlResult::Rows {
            columns: indexed_join_columns(),
            rows: vec![vec![
                SqlValue::Signed(1),
                SqlValue::Text("Mario".to_owned()),
                SqlValue::Text("Envigado".to_owned()),
            ]],
        };
        assert_eq!(
            update.execute_sql(indexed_join_query(), &mario)?,
            expected_envigado
        );
        update.commit()?;
        assert_eq!(
            historical.execute_prepared(&historical_plan, &mario)?,
            expected_medellin
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(indexed_join_query())?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &mario)?,
            expected_envigado
        );
        Ok(())
    }

    #[test]
    fn bounded_inner_join_applies_limit_after_missing_right_rows()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        seed_indexed_join(&mut database)?;
        let query = "SELECT users.id, profiles.city
                     FROM users
                     INNER JOIN profiles ON users.profile_id = profiles.id
                     WHERE id >= ?
                     ORDER BY id
                     LIMIT 2";
        let parameters = [SqlValue::Signed(1)];
        let expected = SqlResult::Rows {
            columns: vec!["users.id".to_owned(), "profiles.city".to_owned()],
            rows: vec![
                vec![SqlValue::Signed(1), SqlValue::Text("Medellin".to_owned())],
                vec![SqlValue::Signed(4), SqlValue::Text("Caracas".to_owned())],
            ],
        };
        let snapshot = database.snapshot(11)?;
        let snapshot_plan = snapshot.prepare_sql(query)?;
        assert_eq!(
            snapshot.execute_prepared(&snapshot_plan, &parameters)?,
            expected
        );
        let latest_plan = database.prepare_sql_latest(query)?;
        assert_eq!(
            database.execute_prepared_latest(&latest_plan, &parameters)?,
            expected
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let reopened_plan = reopened.prepare_sql_latest(query)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &parameters)?,
            expected
        );
        Ok(())
    }

    fn assert_bounded_join_plan_and_full_scan(
        transaction: &mut NativeWriteBatch,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            transaction.execute_sql(
                "EXPLAIN SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 WHERE id >= ?
                 ORDER BY id
                 LIMIT 2",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["plan".to_owned()],
                rows: vec![vec![SqlValue::Text(
                    "IndexedInnerJoin(left_table=1,left_access=primary-key-range(lower=inclusive,upper=unbounded,limit=2),right_table=2,right_access=primary-key)"
                        .to_owned(),
                )]],
            }
        );
        assert_eq!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 ORDER BY id
                 LIMIT 2",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["users.id".to_owned(), "profiles.city".to_owned()],
                rows: vec![
                    vec![SqlValue::Signed(1), SqlValue::Text("Medellin".to_owned()),],
                    vec![SqlValue::Signed(4), SqlValue::Text("Caracas".to_owned()),],
                ],
            }
        );
        Ok(())
    }

    #[test]
    fn bounded_inner_join_reads_private_rows_and_explains_access()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        seed_indexed_join(&mut database)?;
        let mut transaction = database.begin_sql(12, DurabilityClass::Strict)?;
        assert_bounded_join_plan_and_full_scan(&mut transaction)?;
        let range_query = "SELECT users.id, profiles.city
                           FROM users
                           INNER JOIN profiles ON users.profile_id = profiles.id
                           WHERE id >= ?
                           ORDER BY id
                           LIMIT 3";
        transaction.execute_sql(
            "INSERT INTO profiles (id, city) VALUES (600, 'Maracaibo')",
            &[],
        )?;
        transaction.execute_sql(
            "INSERT INTO users (id, email, name, profile_id)
             VALUES (6, 'six@hyphae.local', 'Danny', 600)",
            &[],
        )?;
        assert_eq!(
            transaction.execute_sql(range_query, &[SqlValue::Signed(4)])?,
            SqlResult::Rows {
                columns: vec!["users.id".to_owned(), "profiles.city".to_owned()],
                rows: vec![
                    vec![SqlValue::Signed(4), SqlValue::Text("Caracas".to_owned()),],
                    vec![
                        SqlValue::Signed(5),
                        SqlValue::Text("San Cristobal".to_owned()),
                    ],
                    vec![SqlValue::Signed(6), SqlValue::Text("Maracaibo".to_owned()),],
                ],
            }
        );
        assert_eq!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 ORDER BY id
                 LIMIT 0",
                &[],
            )?,
            SqlResult::Rows {
                columns: vec!["users.id".to_owned(), "profiles.city".to_owned()],
                rows: Vec::new(),
            }
        );
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 WHERE id >= ?",
                &[SqlValue::Signed(1)],
            ),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 ORDER BY profile_id
                 LIMIT 2",
                &[],
            ),
            Err(SqlError::InvalidPrimaryKey)
        ));
        Ok(())
    }

    #[test]
    fn indexed_inner_join_explains_and_rejects_unsupported_access_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        seed_indexed_join(&mut database)?;
        let mut transaction = database.begin_sql(11, DurabilityClass::Strict)?;
        assert_eq!(
            transaction.execute_sql(&format!("EXPLAIN {}", indexed_join_query()), &[])?,
            SqlResult::Rows {
                columns: vec!["plan".to_owned()],
                rows: vec![vec![SqlValue::Text(
                    "IndexedInnerJoin(left_table=1,left_access=unique-secondary(index=3),right_table=2,right_access=primary-key)"
                        .to_owned()
                )]]
            }
        );
        assert_eq!(
            transaction.execute_sql(
                "SELECT users.name, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 WHERE id = ?",
                &[SqlValue::Signed(1)],
            )?,
            SqlResult::Rows {
                columns: vec!["users.name".to_owned(), "profiles.city".to_owned()],
                rows: vec![vec![
                    SqlValue::Text("Mario".to_owned()),
                    SqlValue::Text("Medellin".to_owned()),
                ]]
            }
        );
        transaction.execute_sql("CREATE INDEX users_name ON users (name)", &[])?;
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 WHERE name = ?",
                &[SqlValue::Text("Mario".to_owned())],
            ),
            Err(SqlError::NoAccessPath)
        ));
        let mario = [SqlValue::Text("one@hyphae.local".to_owned())];
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.name = profiles.city
                 WHERE email = ?",
                &mario,
            ),
            Err(SqlError::NoAccessPath)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, city
                 FROM users
                 INNER JOIN profiles ON users.profile_id = profiles.id
                 WHERE email = ?",
                &mario,
            ),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            transaction.execute_sql(
                "SELECT users.id, profiles.city
                 FROM users
                 INNER JOIN profiles ON users.email = profiles.id
                 WHERE email = ?",
                &mario,
            ),
            Err(SqlError::TypeMismatch)
        ));
        assert!(matches!(
            transaction.execute_sql(
                indexed_join_query(),
                &[
                    SqlValue::Text("one@hyphae.local".to_owned()),
                    SqlValue::Signed(1)
                ]
            ),
            Err(SqlError::ParameterMismatch)
        ));
        Ok(())
    }

    #[test]
    fn indexed_updates_preserve_unique_nulls_distinct() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT)",
            &[],
        )?;
        for (id, email) in [(1, "one@hyphae.local"), (2, "two@hyphae.local")] {
            seed.execute_sql(
                "INSERT INTO users (id, email) VALUES (?, ?)",
                &[SqlValue::Signed(id), SqlValue::Text(email.to_owned())],
            )?;
        }
        let created = seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing nullable unique index".into());
        };
        seed.commit()?;

        let mut update = database.begin_sql(11, DurabilityClass::Strict)?;
        for id in [1, 2] {
            assert_eq!(
                update.execute_sql(
                    "UPDATE users SET email = ? WHERE id = ?",
                    &[SqlValue::Null, SqlValue::Signed(id)],
                )?,
                command_rows(1)
            );
        }
        assert_eq!(
            update.execute_sql("SELECT id FROM users WHERE email = ?", &[SqlValue::Null])?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: Vec::new(),
            }
        );
        update.commit()?;

        let null_key = SqlValue::Null.encode_ordered_component(&LogicalType::Text)?;
        assert_eq!(
            database
                .select_latest_secondary_index(index, &null_key)?
                .len(),
            2
        );
        drop(database);
        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened
                .select_latest_secondary_index(index, &null_key)?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn indexed_update_delete_crash_boundaries_recover_prior_or_complete_projections()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let (_, email_index, city_index) = seed_mutable_secondary_indexes(&mut database)?;
            let mut mutate = database.begin_sql(12, DurabilityClass::Strict)?;
            mutate.execute_sql(
                "UPDATE people SET email = ?, city = ? WHERE id = ?",
                &[
                    SqlValue::Text("new@celiums.test".to_owned()),
                    SqlValue::Text("Bogota".to_owned()),
                    SqlValue::Signed(1),
                ],
            )?;
            mutate.execute_sql("DELETE FROM people WHERE id = ?", &[SqlValue::Signed(2)])?;
            assert!(matches!(
                mutate.commit_with_interruption(boundary),
                Err(NativeRuntimeError::InjectedCrash(reached)) if reached == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            let old = SqlValue::Text("mario@celiums.test".to_owned())
                .encode_ordered_component(&LogicalType::Text)?;
            let new = SqlValue::Text("new@celiums.test".to_owned())
                .encode_ordered_component(&LogicalType::Text)?;
            let medellin = SqlValue::Text("Medellin".to_owned())
                .encode_ordered_component(&LogicalType::Text)?;
            let bogota =
                SqlValue::Text("Bogota".to_owned()).encode_ordered_component(&LogicalType::Text)?;
            let complete = matches!(
                boundary,
                CommitBoundary::WalAppended
                    | CommitBoundary::WalSynchronized
                    | CommitBoundary::RootPublished
            );
            assert_eq!(
                reopened
                    .select_latest_secondary_index(email_index, &old)?
                    .len(),
                usize::from(!complete)
            );
            assert_eq!(
                reopened
                    .select_latest_secondary_index(email_index, &new)?
                    .len(),
                usize::from(complete)
            );
            assert_eq!(
                reopened
                    .select_latest_secondary_index(city_index, &medellin)?
                    .len(),
                if complete { 0 } else { 2 }
            );
            assert_eq!(
                reopened
                    .select_latest_secondary_index(city_index, &bogota)?
                    .len(),
                usize::from(complete)
            );
        }
        Ok(())
    }

    #[test]
    fn optimistic_unique_secondary_index_rejects_disjoint_primary_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL
            )",
            &[],
        )?;
        seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        seed.commit()?;

        let mut winner = database.begin_optimistic(11, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(11, DurabilityClass::Strict)?;
        winner.execute_sql(
            "INSERT INTO users (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Text("winner@celiums.test".to_owned()),
            ],
        )?;
        loser.execute_sql(
            "INSERT INTO users (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(2),
                SqlValue::Text("winner@celiums.test".to_owned()),
            ],
        )?;
        database.commit_optimistic(winner)?;
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::UniqueSecondaryIndexViolation)
        ));

        let snapshot = database.snapshot(12)?;
        let prepared = snapshot.prepare_sql("SELECT id FROM users WHERE email = ?")?;
        assert_eq!(
            snapshot.execute_prepared(
                &prepared,
                &[SqlValue::Text("winner@celiums.test".to_owned())],
            )?,
            SqlResult::Rows {
                columns: vec!["id".to_owned()],
                rows: vec![vec![SqlValue::Signed(1)]],
            }
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        Ok(())
    }

    #[test]
    fn optimistic_index_creation_and_insert_rebase_preserve_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        for index_commits_first in [true, false] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
            seed.execute_sql(
                "CREATE TABLE people (
                    id BIGINT PRIMARY KEY,
                    email TEXT NOT NULL
                )",
                &[],
            )?;
            seed.commit()?;

            let mut create_index = database.begin_optimistic(11, DurabilityClass::Strict)?;
            let mut insert = database.begin_optimistic(11, DurabilityClass::Strict)?;
            create_index.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
            insert.execute_sql(
                "INSERT INTO people (id, email) VALUES (?, ?)",
                &[
                    SqlValue::Signed(1),
                    SqlValue::Text("mario@celiums.test".to_owned()),
                ],
            )?;
            if index_commits_first {
                database.commit_optimistic(create_index)?;
                database.commit_optimistic(insert)?;
            } else {
                database.commit_optimistic(insert)?;
                database.commit_optimistic(create_index)?;
            }

            let snapshot = database.snapshot(12)?;
            let prepared = snapshot.prepare_sql("SELECT id FROM people WHERE email = ?")?;
            assert_eq!(
                snapshot.execute_prepared(
                    &prepared,
                    &[SqlValue::Text("mario@celiums.test".to_owned())],
                )?,
                SqlResult::Rows {
                    columns: vec!["id".to_owned()],
                    rows: vec![vec![SqlValue::Signed(1)]],
                }
            );
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            assert_eq!(reopened.recovery_report().committed_transactions, 3);
            let recovered = reopened.snapshot(12)?;
            let recovered_plan = recovered.prepare_sql("SELECT id FROM people WHERE email = ?")?;
            assert_eq!(
                recovered.execute_prepared(
                    &recovered_plan,
                    &[SqlValue::Text("mario@celiums.test".to_owned())],
                )?,
                SqlResult::Rows {
                    columns: vec!["id".to_owned()],
                    rows: vec![vec![SqlValue::Signed(1)]],
                }
            );
        }
        Ok(())
    }

    #[test]
    fn missing_secondary_index_projection_fails_complete_state_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL
            )",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        transaction.execute_sql(
            "INSERT INTO people (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Text("mario@celiums.test".to_owned()),
            ],
        )?;
        let created =
            transaction.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing index identity".into());
        };
        transaction.commit()?;

        let roots = database.coordinator.snapshot(11)?.roots().clone();
        let state = super::load_state(&database.pages, &database.blobs, &roots)?;
        let index_state = state
            .relational
            .indexes
            .get(&index)
            .ok_or("missing materialized secondary index")?;
        assert_eq!(index_state.relation, table);
        let (index_key, primary_keys) = index_state
            .entries
            .first_key_value()
            .ok_or("missing secondary-index key")?;
        let primary_key = primary_keys
            .first()
            .ok_or("missing secondary-index primary key")?;
        let identity = super::secondary_index_entry_identity(index_key, primary_key)?;
        let physical_key = super::relational_secondary_entry_key(index, &identity)?;
        let root = roots
            .root(super::SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let forged_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(
                &mut database.pages,
                Csn::new(1)?,
                physical_key,
                vec![super::RELATIONAL_SECONDARY_ENTRY_TOMBSTONE],
            )?
            .tree;
        let mut forged_roots = roots
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        forged_roots.insert(
            super::SLOT_RELATIONAL,
            forged_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidRelationalTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            roots
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots.catalog_version(),
            roots
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            forged_roots,
            roots.blob_generation(),
        )?;
        assert!(matches!(
            super::load_relational_state(&database.pages, &database.blobs, &forged, &state.catalog,),
            Err(NativeRuntimeError::InvalidRelationalTree)
        ));
        Ok(())
    }

    #[test]
    fn sql_update_delete_publish_new_roots_and_retain_old_snapshots()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut create = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = create.execute_sql(
            "CREATE TABLE accounts (primary_key BINARY PRIMARY KEY, row BINARY)",
            &[],
        )?;
        let SqlResult::Command {
            object_id: Some(table),
            ..
        } = created
        else {
            return Err("missing table identity".into());
        };
        create.execute_sql(
            "INSERT INTO accounts (primary_key, row) VALUES (?, ?)",
            &[
                SqlValue::Binary(b"mario".to_vec()),
                SqlValue::Binary(b"v1".to_vec()),
            ],
        )?;
        create.commit()?;
        let version_one = database.snapshot(11)?;

        let mut update = database.begin_sql(12, DurabilityClass::Strict)?;
        assert_eq!(
            update.execute_sql(
                "UPDATE accounts SET row = ? WHERE primary_key = ?",
                &[
                    SqlValue::Binary(b"v2".to_vec()),
                    SqlValue::Binary(b"mario".to_vec()),
                ],
            )?,
            SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            }
        );
        update.commit()?;
        let version_two = database.snapshot(13)?;
        assert_eq!(version_one.select(table, b"mario"), Some(b"v1".as_slice()));
        assert_eq!(version_two.select(table, b"mario"), Some(b"v2".as_slice()));

        let mut delete = database.begin_sql(14, DurabilityClass::Strict)?;
        delete.execute_sql(
            "DELETE FROM accounts WHERE primary_key = ?",
            &[SqlValue::Binary(b"mario".to_vec())],
        )?;
        delete.commit()?;
        assert_eq!(database.snapshot(15)?.select(table, b"mario"), None);
        assert_eq!(database.select_latest_relational(table, b"mario")?, None);
        assert_eq!(version_one.select(table, b"mario"), Some(b"v1".as_slice()));
        assert_eq!(version_two.select(table, b"mario"), Some(b"v2".as_slice()));
        assert_three_version_row_chain(&database, table)?;
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 3);
        assert_eq!(reopened.snapshot(16)?.select(table, b"mario"), None);
        assert_eq!(reopened.conflicts.len(), 4);
        assert_eq!(
            reopened
                .conflicts
                .latest_commit(&WriteKey::new(
                    hyphae_native_types::EngineKind::Relational,
                    Some(table),
                    b"mario",
                ))
                .map(hyphae_native_types::Csn::get),
            Some(3)
        );
        Ok(())
    }

    #[test]
    fn same_transaction_row_rewrites_coalesce_into_one_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = ObjectId::new(21)?;
        let mut transaction = database.begin(10, DurabilityClass::Strict)?;
        transaction.create_relation(table, "coalesced_rows")?;
        transaction.insert(table, b"key".to_vec(), b"one".to_vec())?;
        transaction.update(table, b"key".to_vec(), b"two".to_vec())?;
        transaction.delete(table, b"key".to_vec())?;
        transaction.insert(table, b"key".to_vec(), b"final".to_vec())?;
        transaction.commit()?;
        assert_eq!(
            database.select_latest_relational(table, b"key")?,
            Some(b"final".to_vec())
        );

        let snapshot = database.coordinator.snapshot(11)?;
        let root = snapshot
            .roots()
            .root(super::SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let pointer_bytes = hyphae_native_btree::BTree::from_root(root)
            .get(&database.pages, &super::relational_row_key(table, b"key"))?
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        let pointer = hyphae_native_records::RowVersionPointer::decode(&pointer_bytes)?;
        let page = database.pages.read(pointer.page_id)?;
        let row = hyphae_native_records::RowRecord::decode(page.payload())?;
        assert_eq!(row.begin_csn().get(), 1);
        assert_eq!(row.end_csn(), None);
        assert_eq!(page.next(), None);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened.select_latest_relational(table, b"key")?,
            Some(b"final".to_vec())
        );
        Ok(())
    }

    #[test]
    fn inline_row_v1_directories_remain_readable_and_writable()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        database.relational_format = super::RelationalFormat::InlineRowV1;
        let table = ObjectId::new(22)?;
        let mut create = database.begin(10, DurabilityClass::Strict)?;
        create.create_relation(table, "legacy_rows")?;
        create.insert(table, b"key".to_vec(), b"v1".to_vec())?;
        create.commit()?;
        drop(database);

        let mut reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened.relational_format,
            super::RelationalFormat::InlineRowV1
        );
        assert_eq!(
            reopened.select_latest_relational(table, b"key")?,
            Some(b"v1".to_vec())
        );
        assert_eq!(
            reopened.scan_latest_relational(table, None, 1)?[0].row,
            b"v1"
        );
        let mut update = reopened.begin(11, DurabilityClass::Strict)?;
        update.update(table, b"key".to_vec(), b"v2".to_vec())?;
        update.commit()?;
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            recovered.select_latest_relational(table, b"key")?,
            Some(b"v2".to_vec())
        );
        assert_eq!(
            recovered.scan_latest_relational(table, None, 1)?[0].row,
            b"v2"
        );
        Ok(())
    }

    #[test]
    fn inline_row_v1_updates_preserve_secondary_index_projections()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        database.relational_format = super::RelationalFormat::InlineRowV1;
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL
            )",
            &[],
        )?;
        seed.execute_sql(
            "INSERT INTO users (id, email) VALUES (?, ?)",
            &[
                SqlValue::Signed(1),
                SqlValue::Text("old@hyphae.local".to_owned()),
            ],
        )?;
        let created = seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
        let SqlResult::Command {
            object_id: Some(index),
            ..
        } = created
        else {
            return Err("missing V1 secondary index".into());
        };
        seed.commit()?;
        drop(database);

        let mut reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            reopened.relational_format,
            super::RelationalFormat::InlineRowV1
        );
        let mut update = reopened.begin_sql(11, DurabilityClass::Strict)?;
        update.execute_sql(
            "UPDATE users SET email = ? WHERE id = ?",
            &[
                SqlValue::Text("new@hyphae.local".to_owned()),
                SqlValue::Signed(1),
            ],
        )?;
        update.commit()?;
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        let old = SqlValue::Text("old@hyphae.local".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        let new = SqlValue::Text("new@hyphae.local".to_owned())
            .encode_ordered_component(&LogicalType::Text)?;
        assert!(
            recovered
                .select_latest_secondary_index(index, &old)?
                .is_empty()
        );
        assert_eq!(
            recovered.select_latest_secondary_index(index, &new)?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn relational_version_chain_cycles_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        fs::create_dir_all(temporary.path())?;
        let mut pages =
            hyphae_native_pages::PageStore::create(temporary.path().join("cycle-pages.hydb"))?;
        let blobs = hyphae_native_blobs::BlobStore::create(temporary.path())?;
        let table = ObjectId::new(23)?;
        let primary_key = b"key";
        let row = hyphae_native_records::RowRecord::new(
            super::relational_row_id(table, primary_key)?,
            Csn::new(1)?,
            None,
            vec![
                Some(primary_key.to_vec()),
                Some([super::RELATIONAL_VALUE_INLINE, b'v'].to_vec()),
            ],
        )?;
        let cyclic_page = pages.append(
            hyphae_native_pages::PageKind::VersionChain,
            Some(Csn::new(1)?),
            Some(PageId::new(1)?),
            row.encode()?,
        )?;
        assert_eq!(cyclic_page, PageId::new(1)?);
        let pointer = hyphae_native_records::RowVersionPointer {
            page_id: cyclic_page,
        }
        .encode();
        assert!(matches!(
            super::decode_relational_chain(
                &pages,
                table,
                primary_key,
                &pointer,
                Some(Csn::new(1)?),
                &blobs,
            ),
            Err(NativeRuntimeError::InvalidRelationalTree)
        ));
        Ok(())
    }

    #[test]
    fn later_commit_preserves_historical_snapshot_and_recovery_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        stage_vertical(&mut database)?.commit()?;
        let historical = database.snapshot(150)?;

        let mut second = database.begin(151, DurabilityClass::Strict)?;
        second.set(b"second".to_vec(), b"value".to_vec(), None)?;
        let receipt = second.commit()?;
        assert_eq!(receipt.commit_csn.get(), 2);
        assert_eq!(historical.get(b"second"), None);
        assert_eq!(
            database.snapshot(152)?.get(b"second"),
            Some(b"value".as_slice())
        );
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        assert_eq!(
            reopened
                .recovery_report()
                .visible_csn
                .map(hyphae_native_types::Csn::get),
            Some(2)
        );
        assert_eq!(
            reopened.snapshot(153)?.get(b"second"),
            Some(b"value".as_slice())
        );
        Ok(())
    }

    #[test]
    fn recovery_verifies_superseded_committed_roots() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        stage_vertical(&mut database)?.commit()?;
        let superseded_root = database
            .coordinator
            .snapshot(150)?
            .roots()
            .root(super::SLOT_RELATIONAL)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let table = ObjectId::new(1).map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let mut second = database.begin(151, DurabilityClass::Strict)?;
        second.insert(table, b"luciana".to_vec(), b"active".to_vec())?;
        second.commit()?;
        drop(database);

        let page_path = temporary.path().join(PAGE_FILE);
        let mut page_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(page_path)?;
        page_file.seek(SeekFrom::Start(
            superseded_root
                .get()
                .checked_sub(1)
                .and_then(|page| {
                    page.checked_mul(u64::try_from(hyphae_native_pages::PAGE_SIZE).ok()?)
                })
                .and_then(|offset| offset.checked_add(100))
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
        ))?;
        let mut byte = [0_u8; 1];
        page_file.read_exact(&mut byte)?;
        page_file.seek(SeekFrom::Current(-1))?;
        byte[0] ^= 1;
        page_file.write_all(&byte)?;
        page_file.sync_data()?;

        assert!(NativeDatabase::open(temporary.path()).is_err());
        Ok(())
    }

    fn ann_config() -> Result<HnswConfig, NativeRuntimeError> {
        Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
    }

    fn ann_options() -> Result<AnnSearchOptions, NativeRuntimeError> {
        Ok(AnnSearchOptions::new(2, 8, Some(4))?)
    }

    #[test]
    fn durable_ann_generation_shares_the_global_csn_and_recovers()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = ObjectId::new(1)?;
        let lexical = ObjectId::new(2)?;
        let vectors = ObjectId::new(3)?;
        let mario = ObjectId::new(101)?;
        let romina = ObjectId::new(102)?;
        let luciana = ObjectId::new(103)?;

        let mut create = database.begin(10, DurabilityClass::Strict)?;
        create.create_relation(table, "accounts")?;
        create.insert(table, b"mario".to_vec(), b"active".to_vec())?;
        create.set(b"session".to_vec(), b"open".to_vec(), Some(100))?;
        create.create_search_index(lexical, "notes")?;
        create.index_document(lexical, b"doc-1".to_vec(), "native vector search")?;
        create.create_vector_index(
            vectors,
            "embeddings",
            3,
            VectorMetric::Cosine,
            ann_config()?,
        )?;
        assert_eq!(
            create.upsert_vectors(
                vectors,
                [
                    (mario, Vector::new([1.0, 0.0, 0.0])?),
                    (romina, Vector::new([0.0, 1.0, 0.0])?),
                ],
            )?,
            2
        );
        let first = create.commit()?;
        assert_eq!(first.commit_csn, Csn::new(1)?);

        let historical = database.snapshot(11)?;
        assert_eq!(
            historical.select(table, b"mario"),
            Some(b"active".as_slice())
        );
        assert_eq!(historical.get(b"session"), Some(b"open".as_slice()));
        assert_eq!(
            historical.match_text(lexical, "vector", 1)?[0].document_id,
            b"doc-1"
        );
        let first_ann =
            historical.search_ann(vectors, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)?;
        assert!(first_ann.approximate);
        assert_eq!(first_ann.index_id, vectors);
        assert_eq!(first_ann.snapshot_csn, Some(Csn::new(1)?));
        assert_eq!(first_ann.hits[0].object_id, mario);
        assert_eq!(
            historical.search_vector_exact(vectors, &Vector::new([1.0, 0.0, 0.0])?, 2,)?[0]
                .object_id,
            mario
        );

        let mut mutate = database.begin(12, DurabilityClass::Strict)?;
        mutate.upsert_vector(vectors, mario, Vector::new([0.0, 1.0, 0.0])?)?;
        assert!(mutate.delete_vector(vectors, romina)?);
        mutate.upsert_vector(vectors, luciana, Vector::new([1.0, 0.0, 0.0])?)?;
        let second = mutate.commit()?;
        assert_eq!(second.commit_csn, Csn::new(2)?);

        assert_eq!(
            historical.search_vector_exact(vectors, &Vector::new([1.0, 0.0, 0.0])?, 2,)?[0]
                .object_id,
            mario
        );
        let current =
            database.search_ann_latest(vectors, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)?;
        assert_eq!(current.snapshot_csn, Some(Csn::new(2)?));
        assert_eq!(current.hits[0].object_id, luciana);
        assert_ne!(current.build_identity, first_ann.build_identity);
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        assert_eq!(reopened.recovery_report().committed_transactions, 2);
        let recovered =
            reopened.search_ann_latest(vectors, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)?;
        assert_eq!(recovered, current);
        assert_eq!(
            reopened
                .search_vector_exact_latest(vectors, &Vector::new([1.0, 0.0, 0.0])?, 3,)?
                .iter()
                .map(|hit| hit.object_id)
                .collect::<Vec<_>>(),
            vec![luciana, mario]
        );
        Ok(())
    }

    #[test]
    fn ann_batch_ingest_rejects_the_complete_invalid_batch()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(10)?;
        let first = ObjectId::new(100)?;
        let second = ObjectId::new(200)?;
        let mut transaction = database.begin(1, DurabilityClass::Strict)?;
        transaction.create_vector_index(
            index,
            "embeddings",
            3,
            VectorMetric::Cosine,
            ann_config()?,
        )?;
        assert_eq!(
            transaction.upsert_vectors(
                index,
                [
                    (first, Vector::new([1.0, 0.0, 0.0])?),
                    (second, Vector::new([0.0, 1.0, 0.0])?),
                ],
            )?,
            2
        );
        let before =
            transaction.search_ann(index, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)?;
        assert!(
            transaction
                .upsert_vectors(
                    index,
                    [
                        (first, Vector::new([0.0, 0.0, 1.0])?),
                        (first, Vector::new([0.0, 1.0, 1.0])?),
                    ],
                )
                .is_err()
        );
        assert!(
            transaction
                .upsert_vectors(index, [(first, Vector::new([1.0, 0.0])?)])
                .is_err()
        );
        assert_eq!(transaction.upsert_vectors(index, std::iter::empty())?, 0);
        assert_eq!(
            transaction.search_ann(index, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)?,
            before
        );
        Ok(())
    }

    #[test]
    fn ann_same_vector_conflicts_and_disjoint_vectors_rebase()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(10)?;
        let first_id = ObjectId::new(100)?;
        let second_id = ObjectId::new(200)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "embeddings",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.commit()?;

        let mut first = database.begin_optimistic(2, DurabilityClass::Strict)?;
        let mut second = database.begin_optimistic(2, DurabilityClass::Strict)?;
        first.upsert_vector(index, first_id, Vector::new([1.0, 0.0])?)?;
        second.upsert_vector(index, second_id, Vector::new([0.0, 1.0])?)?;
        database.commit_optimistic(first)?;
        database.commit_optimistic(second)?;

        let mut winner = database.begin_optimistic(3, DurabilityClass::Strict)?;
        let mut loser = database.begin_optimistic(3, DurabilityClass::Strict)?;
        winner.upsert_vector(index, first_id, Vector::new([2.0, 0.0])?)?;
        loser.upsert_vector(index, first_id, Vector::new([3.0, 0.0])?)?;
        database.commit_optimistic(winner)?;
        assert!(matches!(
            database.commit_optimistic(loser),
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        assert_eq!(
            database
                .search_vector_exact_latest(index, &Vector::new([0.0, 1.0])?, 2)?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn ann_commit_crash_matrix_recovers_none_or_the_complete_cross_engine_csn()
    -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let table = ObjectId::new(1)?;
            let lexical = ObjectId::new(2)?;
            let vectors = ObjectId::new(3)?;
            let object = ObjectId::new(4)?;
            let mut transaction = database.begin(10, DurabilityClass::Strict)?;
            transaction.create_relation(table, "accounts")?;
            transaction.insert(table, b"mario".to_vec(), b"active".to_vec())?;
            transaction.set(b"session".to_vec(), b"open".to_vec(), None)?;
            transaction.create_search_index(lexical, "notes")?;
            transaction.index_document(lexical, b"doc-1".to_vec(), "atomic vector")?;
            transaction.create_vector_index(
                vectors,
                "embeddings",
                2,
                VectorMetric::NegativeDot,
                ann_config()?,
            )?;
            transaction.upsert_vector(vectors, object, Vector::new([1.0, 0.0])?)?;
            assert!(matches!(
                transaction.commit_with_interruption(boundary),
                Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
            ));
            drop(database);

            let reopened = NativeDatabase::open(temporary.path())?;
            match reopened.recovery_report().visible_csn {
                None => {
                    let snapshot = reopened.snapshot(11)?;
                    assert_eq!(snapshot.select(table, b"mario"), None);
                    assert_eq!(snapshot.get(b"session"), None);
                    assert!(snapshot.match_text(lexical, "vector", 1).is_err());
                    assert!(
                        snapshot
                            .search_ann(vectors, &Vector::new([1.0, 0.0])?, ann_options()?,)
                            .is_err()
                    );
                }
                Some(csn) if csn == Csn::new(1)? => {
                    let snapshot = reopened.snapshot(11)?;
                    assert_eq!(snapshot.select(table, b"mario"), Some(b"active".as_slice()));
                    assert_eq!(snapshot.get(b"session"), Some(b"open".as_slice()));
                    assert_eq!(
                        snapshot.match_text(lexical, "vector", 1)?[0].document_id,
                        b"doc-1"
                    );
                    assert_eq!(
                        snapshot
                            .search_ann(vectors, &Vector::new([1.0, 0.0])?, ann_options()?,)?
                            .hits[0]
                            .object_id,
                        object
                    );
                }
                found => return Err(format!("unexpected recovered CSN: {found:?}").into()),
            }
        }
        Ok(())
    }

    #[test]
    fn corrupted_ann_generation_fails_complete_root_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let index = ObjectId::new(10)?;
        let object = ObjectId::new(20)?;
        let mut transaction = database.begin(1, DurabilityClass::Strict)?;
        transaction.create_vector_index(
            index,
            "embeddings",
            2,
            VectorMetric::Cosine,
            ann_config()?,
        )?;
        transaction.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
        transaction.commit()?;
        let receipt =
            database.search_ann_latest(index, &Vector::new([1.0, 0.0])?, ann_options()?)?;

        let root_set = database.coordinator.snapshot(2)?.roots().clone();
        let search_root = root_set
            .root(super::SLOT_SEARCH)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let vector_key = super::ann_store::vector_key(index, receipt.build_identity, object);
        let encoded_vector = hyphae_native_btree::BTree::from_root(search_root)
            .get(&database.pages, &vector_key)?
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let corrupted = hyphae_native_btree::BTree::from_root(search_root)
            .upsert(
                &mut database.pages,
                Csn::new(1)?,
                vector_key.clone(),
                vec![0; 32],
            )?
            .tree;
        let mut roots = root_set
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        roots.insert(
            super::SLOT_SEARCH,
            corrupted.root().ok_or(NativeRuntimeError::InvalidAnnTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots.clone(),
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            super::load_state(&database.pages, &database.blobs, &forged),
            Err(NativeRuntimeError::InvalidAnnTree)
        ));

        let mut orphaned = hyphae_native_btree::BTree::empty()
            .insert_unique(
                &mut database.pages,
                Csn::new(1)?,
                super::SEARCH_FORMAT_KEY.to_vec(),
                super::SEARCH_FORMAT_VALUE_V1.to_vec(),
            )?
            .tree;
        orphaned = orphaned
            .insert_unique(
                &mut database.pages,
                Csn::new(1)?,
                vector_key,
                encoded_vector,
            )?
            .tree;
        roots.insert(
            super::SLOT_SEARCH,
            orphaned.root().ok_or(NativeRuntimeError::InvalidAnnTree)?,
        );
        let forged_orphan = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots,
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            super::load_state(&database.pages, &database.blobs, &forged_orphan),
            Err(NativeRuntimeError::InvalidAnnTree)
        ));
        Ok(())
    }
}
