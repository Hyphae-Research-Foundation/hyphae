// SPDX-License-Identifier: Apache-2.0

//! First executable convergence slice for Hyphae's native local data engine.
//!
//! This crate proves one data directory, one page store, one WAL authority,
//! one MVCC publication, and independently owned relational, structure, and
//! lexical-search state. Its deliberately small operation surface is not a
//! claim of complete SQL, Valkey, or `OpenSearch` compatibility.

mod local_protocol;
mod model;
mod sql;
mod wal_codec;

pub use local_protocol::{
    DEFAULT_MAX_FRAME_PAYLOAD, DecodedFrame, FrameKind, LOCAL_FRAME_HEADER_SIZE,
    LocalProtocolError, decode_frame, encode_frame,
};
pub use sql::{PreparedStatement, SqlError, SqlResult, SqlValue};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use hyphae_native_blobs::{BlobError, BlobStore, StagedBlob};
use hyphae_native_btree::{BTree, BTreeError};
use hyphae_native_catalog::{
    CatalogError, CatalogName, ColumnDefinition, ObjectHeader, QualifiedName, RelationDefinition,
    SearchCollectionDefinition, SearchFieldDefinition,
};
use hyphae_native_manifest::{ManifestError, RootManifest, RootManifestStore};
use hyphae_native_mvcc::{
    CommitCoordinator, ConflictTable, MvccError, RootSet, RootSlot, RootTransaction, Snapshot,
    WalAnchor, WriteConflict, WriteKey,
};
use hyphae_native_pages::{BufferPool, BufferPoolError, PageKind, PageStore, PageStoreError};
use hyphae_native_records::{
    BlobReference, ColumnValueRef, RecordError, RowRecord, RowRecordView, RowVersionPointer,
};
use hyphae_native_types::{
    CatalogVersion, ColumnId, Csn, DurabilityClass, EngineKind, FieldId, LogicalType, Lsn,
    ManifestGeneration, ObjectId, PageId, RowId, TransactionId,
};
use hyphae_native_wal::{WalError, WalFile, WalRecovery};
use thiserror::Error;

use crate::{
    model::{CatalogState, ModelError, RelationState, SearchState, StructureState, TtlValue},
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
const RELATIONAL_VALUE_INLINE: u8 = 0;
const RELATIONAL_VALUE_BLOB: u8 = 1;
const RELATIONAL_INLINE_VALUE_LIMIT: usize = 8_192;
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

/// Native runtime or recovery failure.
#[derive(Debug, Error)]
pub enum NativeRuntimeError {
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
        Self::Model(source.to_string())
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct MaterializedState {
    catalog: CatalogState,
    relational: RelationState,
    structures: StructureState,
    search: SearchState,
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
        let relational_format = latest_root
            .as_ref()
            .map(|root| relational_format_for_root(&opened_pages.store, root))
            .transpose()?
            .unwrap_or(RelationalFormat::VersionChainV2);
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
        let snapshot = self.coordinator.snapshot(logical_time_micros)?;
        let state = load_state(&self.pages, &self.blobs, snapshot.roots())?;
        let transaction_id = TransactionId::new(self.next_transaction_id)
            .map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let root_transaction = self.coordinator.begin_write()?;
        Ok(NativeTransaction {
            pages: &mut self.pages,
            blobs: &mut self.blobs,
            wal: &mut self.wal,
            conflicts: &mut self.conflicts,
            relational_format: self.relational_format,
            root_transaction,
            transaction_id,
            next_transaction_id: &mut self.next_transaction_id,
            snapshot,
            state,
            mutations: Vec::new(),
            dirty: [false; 4],
            durability,
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

/// Private write set for one all-engine native transaction.
#[derive(Debug)]
pub struct NativeTransaction<'database> {
    pages: &'database mut PageStore,
    blobs: &'database mut BlobStore,
    wal: &'database mut WalFile,
    conflicts: &'database mut ConflictTable,
    relational_format: RelationalFormat,
    root_transaction: RootTransaction<'database>,
    transaction_id: TransactionId,
    next_transaction_id: &'database mut u128,
    snapshot: Snapshot,
    state: MaterializedState,
    mutations: Vec<Mutation>,
    dirty: [bool; 4],
    durability: DurabilityClass,
}

impl NativeTransaction<'_> {
    /// Executes one statement in the exact first native SQL slice.
    ///
    /// The supported grammar is `CREATE TABLE`, binary primary-key `INSERT`,
    /// `UPDATE`/`DELETE`, and parameterized primary-key `SELECT`.
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
        validate_relation_definition(id, name)?;
        self.state
            .catalog
            .create(id, EngineKind::Relational, name.to_owned())?;
        self.state.relational.create_table(id)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::CreateTable,
            target: Some(id),
            key: Vec::new(),
            value: name.as_bytes().to_vec(),
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
        self.state
            .relational
            .insert(table, primary_key.clone(), row.clone())?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::InsertRow,
            target: Some(table),
            key: primary_key,
            value: row,
            expires_at_micros: None,
        });
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
        self.state
            .relational
            .update(table, &primary_key, row.clone())?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::UpdateRow,
            target: Some(table),
            key: primary_key,
            value: row,
            expires_at_micros: None,
        });
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
        self.state.relational.delete(table, &primary_key)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Relational,
            opcode: Opcode::DeleteRow,
            target: Some(table),
            key: primary_key,
            value: Vec::new(),
            expires_at_micros: None,
        });
        self.dirty[1] = true;
        Ok(())
    }

    /// Reads a relation from the snapshot plus this transaction's writes.
    pub fn select(&self, table: ObjectId, primary_key: &[u8]) -> Option<&[u8]> {
        self.state.relational.select(table, primary_key)
    }

    /// Sets one native structure value and optional absolute expiry.
    pub fn set(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
        expires_at_micros: Option<i64>,
    ) {
        let key = key.into();
        let value = value.into();
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
    }

    /// Reads a structure value from the snapshot plus private writes.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.state
            .structures
            .get(key, self.snapshot.logical_time_micros)
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
        validate_search_definition(id, name)?;
        self.state
            .catalog
            .create(id, EngineKind::Search, name.to_owned())?;
        self.state.search.create_index(id)?;
        self.mutations.push(Mutation {
            engine: EngineKind::Search,
            opcode: Opcode::CreateIndex,
            target: Some(id),
            key: Vec::new(),
            value: name.as_bytes().to_vec(),
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
        let write_keys = mutation_write_keys(&self.mutations);
        self.conflicts
            .validate(self.root_transaction.read_csn(), &write_keys)?;
        Ok(write_keys)
    }

    fn commit_catalog_version(&self) -> Result<CatalogVersion, CatalogError> {
        if self.dirty[0] {
            self.snapshot
                .catalog_version
                .checked_next()
                .ok_or(CatalogError::VersionExhausted)
        } else {
            Ok(self.snapshot.catalog_version)
        }
    }

    fn commit_at(
        mut self,
        interruption: Option<CommitBoundary>,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        if self.mutations.is_empty() {
            return Err(WalSemanticError::InvalidSequence.into());
        }
        let commit_csn = self.root_transaction.commit_csn()?;
        let write_keys = self.validated_write_keys()?;
        let catalog_version = self.commit_catalog_version()?;
        let synchronize = self.durability != DurabilityClass::Memory;
        let staged_blobs = stage_large_relational_values(self.blobs, &self.mutations, synchronize)?;
        interrupt(interruption, CommitBoundary::BlobStaged)?;
        let blob_references = publish_staged_blobs(self.blobs, staged_blobs, synchronize)?;
        let blob_generation = self.blobs.generation()?;
        interrupt(interruption, CommitBoundary::BlobPromoted)?;

        let mut roots = roots_from_snapshot(self.snapshot.roots());
        if self.dirty[0] || roots[0].is_none() {
            roots[0] = Some(self.pages.append(
                PageKind::CatalogRoot,
                Some(commit_csn),
                None,
                self.state.catalog.encode()?,
            )?);
        }
        if self.dirty[1] || roots[1].is_none() {
            roots[1] = relational_tree_after_mutations(
                self.pages,
                roots[1],
                self.relational_format,
                commit_csn,
                &self.mutations,
                &blob_references,
            )?
            .root();
        }
        if self.dirty[2] || roots[2].is_none() {
            roots[2] = Some(self.pages.append(
                PageKind::StructureNode,
                Some(commit_csn),
                None,
                self.state.structures.encode()?,
            )?);
        }
        if self.dirty[3] || roots[3].is_none() {
            roots[3] = Some(self.pages.append(
                PageKind::SearchDelta,
                Some(commit_csn),
                None,
                self.state.search.encode()?,
            )?);
        }
        interrupt(interruption, CommitBoundary::PageAppended)?;
        if synchronize {
            self.pages.sync_data()?;
        }
        interrupt(interruption, CommitBoundary::PageSynchronized)?;

        let concrete_roots = require_roots(roots)?;
        let wal_mutations = wal_mutations(&self.mutations, &blob_references)?;
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: self.transaction_id,
            read_csn: self.root_transaction.read_csn(),
            catalog_version,
            logical_time_micros: self.snapshot.logical_time_micros,
            durability: self.durability,
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
            durability: self.durability,
        })
    }
}

fn mutation_write_keys(mutations: &[Mutation]) -> Vec<WriteKey> {
    let mut keys = Vec::with_capacity(mutations.len().saturating_mul(3));
    for mutation in mutations {
        keys.push(WriteKey::new(
            mutation.engine,
            mutation.target,
            mutation.key.clone(),
        ));
        if matches!(mutation.opcode, Opcode::CreateTable | Opcode::CreateIndex) {
            if let Some(object) = mutation.target {
                let mut object_key = Vec::with_capacity(17);
                object_key.push(1);
                object_key.extend_from_slice(&object.get().to_be_bytes());
                keys.push(WriteKey::new(EngineKind::Kernel, None, object_key));
            }
            let mut name_key = Vec::with_capacity(mutation.value.len().saturating_add(2));
            name_key.extend_from_slice(&[2, mutation.engine as u8]);
            name_key.extend_from_slice(&mutation.value);
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

fn stage_large_relational_values(
    blobs: &BlobStore,
    mutations: &[Mutation],
    synchronize: bool,
) -> Result<BTreeMap<[u8; 32], StagedBlob>, NativeRuntimeError> {
    let mut staged = BTreeMap::new();
    for mutation in mutations.iter().filter(|mutation| {
        mutation.engine == EngineKind::Relational
            && matches!(mutation.opcode, Opcode::InsertRow | Opcode::UpdateRow)
            && mutation.value.len() > RELATIONAL_INLINE_VALUE_LIMIT
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
            }
            Ok(mutation)
        })
        .collect()
}

fn relational_tree_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    format: RelationalFormat,
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
                RELATIONAL_FORMAT_KEY.to_vec(),
                format.marker().to_vec(),
            )?
            .tree;
    }
    for mutation in mutations
        .iter()
        .filter(|mutation| mutation.engine == EngineKind::Relational)
    {
        let table = mutation
            .target
            .ok_or(NativeRuntimeError::InvalidRelationalTree)?;
        let key = match mutation.opcode {
            Opcode::CreateTable => {
                tree = tree
                    .insert_unique(pages, creating_csn, relational_table_key(table), Vec::new())?
                    .tree;
                continue;
            }
            Opcode::InsertRow | Opcode::UpdateRow | Opcode::DeleteRow => {
                relational_row_key(table, &mutation.key)
            }
            _ => return Err(NativeRuntimeError::InvalidRelationalTree),
        };
        let row = if mutation.opcode == Opcode::DeleteRow {
            RowRecord::tombstone(relational_row_id(table, &mutation.key)?, creating_csn, None)?
        } else {
            RowRecord::new(
                relational_row_id(table, &mutation.key)?,
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
        if commit.manifest.commit_csn.get() != expected || commit.manifest.read_csn != prior {
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
    for (slot, expected_kind) in [
        (SLOT_CATALOG, PageKind::CatalogRoot),
        (SLOT_STRUCTURE, PageKind::StructureNode),
        (SLOT_SEARCH, PageKind::SearchDelta),
    ] {
        let page_id = roots
            .root(slot)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let page = pages.read(page_id)?;
        if page.kind() != expected_kind {
            return Err(NativeRuntimeError::InvalidCommittedRoot);
        }
        if page
            .creating_csn()
            .is_none_or(|creating| creating > visible_csn)
        {
            return Err(NativeRuntimeError::FuturePage);
        }
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
    load_state(pages, blobs, roots)?;
    Ok(())
}

fn load_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
) -> Result<MaterializedState, NativeRuntimeError> {
    Ok(MaterializedState {
        catalog: load_root(pages, roots, SLOT_CATALOG, PageKind::CatalogRoot, |bytes| {
            CatalogState::decode(bytes)
        })?
        .unwrap_or_default(),
        relational: load_relational_state(pages, blobs, roots)?,
        structures: load_root(
            pages,
            roots,
            SLOT_STRUCTURE,
            PageKind::StructureNode,
            StructureState::decode,
        )?
        .unwrap_or_default(),
        search: load_root(pages, roots, SLOT_SEARCH, PageKind::SearchDelta, |bytes| {
            SearchState::decode(bytes)
        })?
        .unwrap_or_default(),
    })
}

fn load_relational_state(
    pages: &PageStore,
    blobs: &BlobStore,
    roots: &RootSet,
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
            _ => return Err(NativeRuntimeError::InvalidRelationalTree),
        }
    }
    Ok(RelationState { tables })
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

fn validate_relation_definition(id: ObjectId, name: &str) -> Result<(), CatalogError> {
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
    definition.validate()
}

fn validate_search_definition(id: ObjectId, name: &str) -> Result<(), CatalogError> {
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
    };
    definition.validate()
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
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_mvcc::WriteKey;
    use hyphae_native_types::{Csn, DurabilityClass, ManifestGeneration, ObjectId, PageId};

    use super::{
        CheckpointBoundary, CommitBoundary, NativeDatabase, NativeRuntimeError, PAGE_FILE,
        SqlResult, SqlValue,
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
        transaction.set(b"session".to_vec(), b"open".to_vec(), Some(200));
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
            snapshot.match_text(index, "rust", 10)?[0].document_id,
            b"doc-1"
        );
        let expired = database.snapshot(200)?;
        assert_eq!(expired.get(b"session"), None);
        Ok(())
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
    fn large_relational_value_uses_verified_blob_storage() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let table = ObjectId::new(11)?;
        let value = vec![0x6d; super::RELATIONAL_INLINE_VALUE_LIMIT + 4_096];
        let mut transaction = database.begin(100, DurabilityClass::Strict)?;
        transaction.create_relation(table, "large_rows")?;
        transaction.insert(table, b"blob-row".to_vec(), value.clone())?;
        transaction.commit()?;
        assert_eq!(
            database.select_latest_relational(table, b"blob-row")?,
            Some(value.clone())
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
            reopened.select_latest_relational(table, b"blob-row")?,
            Some(value)
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
    fn native_sql_create_insert_prepare_select_and_rollback_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut transaction = database.begin_sql(10, DurabilityClass::Strict)?;
        let created = transaction.execute_sql(
            "CREATE TABLE accounts (primary_key BINARY PRIMARY KEY, row BINARY)",
            &[],
        )?;
        assert!(matches!(
            created,
            SqlResult::Command {
                object_id: Some(_),
                ..
            }
        ));
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
        let mut update = reopened.begin(11, DurabilityClass::Strict)?;
        update.update(table, b"key".to_vec(), b"v2".to_vec())?;
        update.commit()?;
        drop(reopened);

        let recovered = NativeDatabase::open(temporary.path())?;
        assert_eq!(
            recovered.select_latest_relational(table, b"key")?,
            Some(b"v2".to_vec())
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
        second.set(b"second".to_vec(), b"value".to_vec(), None);
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
}
