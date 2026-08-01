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
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use hyphae_native_btree::{BTree, BTreeError};
use hyphae_native_catalog::{
    CatalogError, CatalogName, ColumnDefinition, ObjectHeader, QualifiedName, RelationDefinition,
    SearchCollectionDefinition, SearchFieldDefinition,
};
use hyphae_native_manifest::{ManifestError, RootManifest, RootManifestStore};
use hyphae_native_mvcc::{
    CommitCoordinator, MvccError, RootSet, RootSlot, RootTransaction, Snapshot, WalAnchor,
};
use hyphae_native_pages::{BufferPool, BufferPoolError, PageKind, PageStore, PageStoreError};
use hyphae_native_records::{ColumnValueRef, RecordError, RowRecord};
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
const RELATIONAL_FORMAT_VALUE: &[u8] = b"HYRELBT1";
const RELATIONAL_TABLE_PREFIX: u8 = 1;
const RELATIONAL_ROW_PREFIX: u8 = 2;
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
    wal: WalFile,
    manifests: RootManifestStore,
    coordinator: CommitCoordinator,
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
        let wal = WalFile::create(path.join(WAL_FILE))?;
        let manifests = RootManifestStore::create(path)?;
        let coordinator = CommitCoordinator::new(
            CatalogVersion::new(1).map_err(|_| NativeRuntimeError::InvalidCommittedRoot)?,
        );
        Ok(Self {
            data_directory: path.to_path_buf(),
            pages,
            buffer_pool,
            wal,
            manifests,
            coordinator,
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
        let opened_wal = WalFile::open(path.join(WAL_FILE))?;
        let recovered_wal = recover_wal(&opened_wal.recovery.records)?;
        let commits = &recovered_wal.commits;
        validate_commit_sequence(commits)?;
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
                0,
            )?;
            validate_roots(&opened_pages.store, &root, recovered.manifest.commit_csn)?;
            committed_roots.insert(recovered.manifest.commit_csn, root.clone());
            latest_root = Some(root);
        }
        let checkpoint_validation = validate_checkpoints(
            &recovered_wal,
            &manifest_recovery.manifests,
            &committed_roots,
        )?;
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
            wal: opened_wal.wal,
            manifests,
            coordinator,
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
        let state = load_state(&self.pages, metadata.roots())?;
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
        let encoded = BTree::from_root(root).get_cached(
            &self.pages,
            &self.buffer_pool,
            &relational_row_key(table, primary_key),
        )?;
        encoded
            .map(|encoded| {
                decode_relational_row(table, primary_key, &encoded, snapshot.visible_csn)
            })
            .transpose()
            .map(Option::flatten)
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
        let state = load_state(&self.pages, snapshot.roots())?;
        let transaction_id = TransactionId::new(self.next_transaction_id)
            .map_err(|_| NativeRuntimeError::TransactionIdExhausted)?;
        let root_transaction = self.coordinator.begin_write()?;
        Ok(NativeTransaction {
            pages: &mut self.pages,
            wal: &mut self.wal,
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
    wal: &'database mut WalFile,
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
    /// and parameterized primary-key `SELECT`.
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

    fn commit_at(
        mut self,
        interruption: Option<CommitBoundary>,
    ) -> Result<CommitReceipt, NativeRuntimeError> {
        if self.mutations.is_empty() {
            return Err(WalSemanticError::InvalidSequence.into());
        }
        let commit_csn = self.root_transaction.commit_csn()?;
        let catalog_version = if self.dirty[0] {
            self.snapshot
                .catalog_version
                .checked_next()
                .ok_or(CatalogError::VersionExhausted)?
        } else {
            self.snapshot.catalog_version
        };
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
            roots[1] =
                relational_tree_after_mutations(self.pages, roots[1], commit_csn, &self.mutations)?
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
        if self.durability != DurabilityClass::Memory {
            self.pages.sync_data()?;
        }
        interrupt(interruption, CommitBoundary::PageSynchronized)?;

        let concrete_roots = require_roots(roots)?;
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: self.transaction_id,
            read_csn: self.root_transaction.read_csn(),
            catalog_version,
            logical_time_micros: self.snapshot.logical_time_micros,
            durability: self.durability,
            mutations: &self.mutations,
            commit_csn,
            roots: concrete_roots,
        })?;
        let receipts = self.wal.append_records(pending, false)?;
        interrupt(interruption, CommitBoundary::WalAppended)?;
        if self.durability != DurabilityClass::Memory {
            self.wal.sync_data()?;
        }
        interrupt(interruption, CommitBoundary::WalSynchronized)?;

        let block = receipts.last().ok_or(WalError::EmptyBlock)?;
        for (slot, page) in ROOT_SLOTS.into_iter().zip(concrete_roots) {
            self.root_transaction.set_root(slot, page);
        }
        self.root_transaction.commit(
            catalog_version,
            WalAnchor::new(block.last_lsn, block.digest)?,
        )?;
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

fn relational_tree_after_mutations(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    mutations: &[Mutation],
) -> Result<BTree, NativeRuntimeError> {
    let mut tree = root.map_or_else(BTree::empty, BTree::from_root);
    if tree.root().is_none() {
        tree = tree
            .insert_unique(
                pages,
                creating_csn,
                RELATIONAL_FORMAT_KEY.to_vec(),
                RELATIONAL_FORMAT_VALUE.to_vec(),
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
        let (key, value) = match mutation.opcode {
            Opcode::CreateTable => (relational_table_key(table), Vec::new()),
            Opcode::InsertRow => (
                relational_row_key(table, &mutation.key),
                RowRecord::new(
                    relational_row_id(table, &mutation.key)?,
                    creating_csn,
                    None,
                    vec![Some(mutation.key.clone()), Some(mutation.value.clone())],
                )?
                .encode()?,
            ),
            _ => return Err(NativeRuntimeError::InvalidRelationalTree),
        };
        tree = tree.insert_unique(pages, creating_csn, key, value)?.tree;
    }
    Ok(tree)
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
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let row = RowRecord::decode(encoded)?;
    if row.row_id() != relational_row_id(table, primary_key)? {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    if !row.is_visible_at(visible_csn) || row.is_tombstone() {
        return Ok(None);
    }
    if row.column_count() != 2 {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
    match (row.value(0), row.value(1)) {
        (Some(ColumnValueRef::Bytes(encoded_primary_key)), Some(ColumnValueRef::Bytes(value)))
            if encoded_primary_key == primary_key =>
        {
            Ok(Some(value.to_vec()))
        }
        _ => Err(NativeRuntimeError::InvalidRelationalTree),
    }
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
    load_state(pages, roots)?;
    Ok(())
}

fn load_state(pages: &PageStore, roots: &RootSet) -> Result<MaterializedState, NativeRuntimeError> {
    Ok(MaterializedState {
        catalog: load_root(pages, roots, SLOT_CATALOG, PageKind::CatalogRoot, |bytes| {
            CatalogState::decode(bytes)
        })?
        .unwrap_or_default(),
        relational: load_relational_state(pages, roots)?,
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
    if format_key != RELATIONAL_FORMAT_KEY || format_value != RELATIONAL_FORMAT_VALUE {
        return Err(NativeRuntimeError::InvalidRelationalTree);
    }
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
                let Some(value) =
                    decode_relational_row(table, primary_key, &value, roots.visible_csn())?
                else {
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

    use hyphae_native_types::{DurabilityClass, ManifestGeneration, ObjectId};

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
                        CommitBoundary::PageAppended | CommitBoundary::PageSynchronized
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
