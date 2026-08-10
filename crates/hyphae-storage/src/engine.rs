// SPDX-License-Identifier: GPL-3.0-only

use std::path::{Path, PathBuf};

use hyphae_core::{VectorSpaceDefinition, VectorSpaceName};
use hyphae_retrieval::{ExactRetrievalError, LexicalIndexDefinition, LexicalMaterializedCorpus};
use thiserror::Error;
use uuid::Uuid;

/// Maximum KV entries returned by one ordered storage scan page.
pub const MAX_SCAN_PAGE_ENTRIES: usize = 4_096;

use crate::log::transaction_digest;
use crate::{
    AppendOutcome, BackupError, BackupInfo, CommitReceipt, DataDirectory, DataDirectoryError,
    DurableLog, LogError, MaintenanceLimits, MaterializedIndexError, Mutation, MutationError,
    RecoveredTransaction, RecoveryLimits, RecoveryReport, SnapshotError, SnapshotInfo,
    SnapshotReadLimits, StorageLimitError, StorageLimits,
    index::{KvScanError, MaterializedIndex, VectorEntry, VectorScanError},
    limits::OperationDeadline,
    manifest::StorageManifest,
    mutation::validate_key,
    snapshot::{create_snapshot, verify_snapshot_with_policy},
};

/// Failure while opening or operating the durable embedded storage engine.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The data directory could not be initialized or exclusively locked.
    #[error(transparent)]
    DataDirectory(#[from] DataDirectoryError),

    /// The authoritative log rejected or failed an operation.
    #[error(transparent)]
    Log(#[from] LogError),

    /// The rebuildable materialized index failed before a new log commit.
    #[error("materialized index failure: {source}")]
    Index {
        /// Rebuildable-index failure.
        #[source]
        source: Box<MaterializedIndexError>,
    },

    /// A mutation violates the stable binary codec.
    #[error(transparent)]
    Mutation(#[from] MutationError),

    /// The log commit is durable but its index update failed.
    #[error("transaction {receipt:?} is durable but not materialized; reopen to recover")]
    CommittedButNotIndexed {
        /// Receipt proving that the log commit succeeded.
        receipt: CommitReceipt,
        /// Rebuildable-index failure.
        #[source]
        source: Box<MaterializedIndexError>,
    },

    /// Reads and further writes are blocked after an index update failure.
    #[error("materialized index is stale; reopen storage to replay the durable log")]
    StaleIndex,

    /// Snapshot creation or verification failed.
    #[error("snapshot failure: {source}")]
    Snapshot {
        /// Underlying snapshot failure.
        #[source]
        source: Box<SnapshotError>,
    },

    /// The immutable manifest generation space is exhausted.
    #[error("storage manifest generation space is exhausted")]
    ManifestGenerationExhausted,

    /// A prepared compaction segment unexpectedly contains complete frames.
    #[error("prepared compaction segment is not empty: {path}")]
    PreparedSegmentNotEmpty {
        /// Unexpected nonempty segment path.
        path: PathBuf,
    },

    /// A KV scan page size is zero or exceeds the hard storage bound.
    #[error("scan page size {requested} is outside 1..={maximum}")]
    InvalidScanLimit {
        /// Requested page size.
        requested: usize,
        /// Hard maximum page size.
        maximum: usize,
    },
}

impl From<StorageLimitError> for StorageError {
    fn from(source: StorageLimitError) -> Self {
        Self::Snapshot {
            source: Box::new(SnapshotError::from(source)),
        }
    }
}

/// Failure while reading a vector space under an explicit end-to-end
/// retrieval deadline.
#[derive(Debug, Error)]
pub enum VectorEntriesError {
    /// Ordinary durable storage failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The exact-retrieval deadline expired.
    #[error(transparent)]
    ExactRetrieval(#[from] ExactRetrievalError),
}

impl From<MaterializedIndexError> for VectorEntriesError {
    fn from(source: MaterializedIndexError) -> Self {
        Self::Storage(StorageError::from(source))
    }
}

/// Failure while scanning one page under an explicit aggregate byte budget.
#[derive(Debug, Error)]
pub enum ScanPageError {
    /// Ordinary durable storage failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Aggregate key and value bytes exceeded caller policy.
    #[error("scan page byte budget exceeded: {maximum}")]
    ByteBudgetExceeded {
        /// Maximum aggregate key and value bytes permitted.
        maximum: u64,
    },
}

/// Recovery evidence returned when the complete embedded storage layer opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageRecoveryReport {
    /// Authoritative log verification and tail-repair evidence.
    pub log: RecoveryReport,
    /// Durable transactions newly applied to the materialized index.
    pub replayed_transactions: u64,
}

/// A newly opened storage engine and its recovery evidence.
#[derive(Debug)]
pub struct OpenedStorage {
    /// Ready-to-use storage engine.
    pub storage: StorageEngine,
    /// Evidence from log validation and index replay.
    pub recovery: StorageRecoveryReport,
}

/// Evidence for one successfully committed compaction generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionReport {
    /// Newly active immutable manifest generation.
    pub generation: u64,
    /// Snapshot anchoring the retired log prefix.
    pub snapshot: SnapshotInfo,
    /// Segment that became inactive after the manifest commit.
    pub retired_segment: PathBuf,
    /// Whether best-effort physical cleanup removed the retired segment.
    pub retired_segment_removed: bool,
}

/// One binary KV entry in canonical key order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KvEntry {
    /// Binary key.
    pub key: Vec<u8>,
    /// Opaque binary value.
    pub value: Vec<u8>,
}

/// One bounded ordered KV scan page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KvPage {
    /// Entries strictly after the requested cursor.
    pub entries: Vec<KvEntry>,
    /// Last emitted key when more entries remain.
    pub next_after: Option<Vec<u8>>,
}

/// Result of an online compaction request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionOutcome {
    /// No committed frames exist beyond the already active snapshot anchor.
    NoChanges {
        /// Current verified snapshot.
        snapshot: SnapshotInfo,
    },
    /// A new manifest and anchored segment were committed.
    Compacted(CompactionReport),
}

/// Single-writer durable KV storage composed from the log and rebuildable redb index.
#[derive(Debug)]
pub struct StorageEngine {
    log: DurableLog,
    index: MaterializedIndex,
    index_stale: bool,
    directory: DataDirectory,
    limits: StorageLimits,
}

impl StorageEngine {
    /// Opens a data directory, verifies its log, and catches the index up before use.
    ///
    /// # Errors
    ///
    /// Returns an error for directory contention, log corruption, invalid committed
    /// mutations, a divergent index checkpoint, or filesystem failures.
    pub fn open(path: impl AsRef<Path>) -> Result<OpenedStorage, StorageError> {
        Self::open_with_limits(path, StorageLimits::compatibility())
    }

    /// Opens and fully recovers a data directory under finite shared limits.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, directory contention, corruption,
    /// exhausted recovery work/bytes, timeout, divergence, or I/O failure.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: StorageLimits,
    ) -> Result<OpenedStorage, StorageError> {
        limits.validate()?;
        let deadline = OperationDeadline::new(limits.recovery.timeout);
        let directory =
            DataDirectory::open_with_limits_and_deadline(path, &limits.recovery, &deadline)?;
        let index_path = directory.path().join("indexes").join("primary.redb");
        ensure_snapshot_base(&directory, &index_path, &limits.recovery, &deadline)?;
        let (base_sequence, base_digest) = directory.log_anchor();
        let (log, log_recovery) = DurableLog::open_file_at_version_with_limits(
            directory.active_log_path(),
            base_sequence,
            base_digest,
            directory.disk_format_version(),
            &limits.recovery,
            &deadline,
        )?;
        let index = MaterializedIndex::open(index_path)?;
        let replayed_transactions =
            index.replay_with_limits(&log_recovery, &limits.recovery, &deadline)?;
        let _cleanup_complete =
            directory.cleanup_retired_logs_with_limits(&limits.recovery, &deadline);
        deadline.check()?;
        let storage = Self {
            log,
            index,
            index_stale: false,
            directory,
            limits,
        };
        Ok(OpenedStorage {
            storage,
            recovery: StorageRecoveryReport {
                log: log_recovery,
                replayed_transactions,
            },
        })
    }

    /// Returns the owned data-directory path.
    pub fn data_path(&self) -> &Path {
        self.directory.path()
    }

    /// Creates an atomic, independently verifiable backup at the current checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be created, the destination
    /// exists or is inside the live data directory, or synchronized promotion
    /// of the complete backup fails.
    pub fn backup(&self, destination: impl AsRef<Path>) -> Result<BackupInfo, BackupError> {
        crate::backup::create_backup(self, destination.as_ref())
    }

    /// Durably commits an atomic batch and then materializes it.
    ///
    /// The log is synchronized before redb is updated. If redb fails, the error
    /// includes the durable commit receipt and this handle blocks reads and writes
    /// until reopen replays the log.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid mutations, idempotency conflicts, log I/O,
    /// or materialized-index failures.
    pub fn write(
        &mut self,
        transaction_id: Uuid,
        mutations: &[Mutation],
    ) -> Result<AppendOutcome, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        self.index.validate_mutations(mutations)?;
        let operations = mutations
            .iter()
            .map(Mutation::encode)
            .collect::<Result<Vec<_>, _>>()?;
        let operation_count =
            u32::try_from(operations.len()).map_err(|_| LogError::TooManyOperations)?;
        let requested_digest = transaction_digest(&operations, operation_count)?;
        if let Some(receipt) = self.index.receipt(transaction_id)? {
            return if receipt.transaction_digest == requested_digest {
                Ok(AppendOutcome::Existing(receipt))
            } else {
                Err(LogError::IdempotencyConflict { transaction_id }.into())
            };
        }
        let lexical_deadline = OperationDeadline::new(self.limits.recovery.timeout);
        self.index.preflight_lexical_mutations(
            mutations,
            &self.limits.recovery,
            &lexical_deadline,
        )?;
        if mutations.iter().any(|mutation| {
            matches!(
                mutation,
                Mutation::DefineVectorSpace { .. }
                    | Mutation::UpsertVector { .. }
                    | Mutation::DeleteVector { .. }
                    | Mutation::DefineLexicalIndex { .. }
            )
        }) {
            self.directory.promote_format()?;
            self.log
                .set_disk_format_version(self.directory.disk_format_version())?;
        }
        let outcome = match self.log.append_transaction(transaction_id, &operations) {
            Ok(outcome) => outcome,
            Err(source) => {
                if self.log.is_poisoned() {
                    self.index_stale = true;
                }
                return Err(source.into());
            }
        };
        let AppendOutcome::Committed(receipt) = outcome else {
            return Ok(outcome);
        };

        let transaction = RecoveredTransaction {
            receipt,
            operations,
        };
        if let Err(source) =
            self.index
                .apply_with_limits(&transaction, &self.limits.recovery, &lexical_deadline)
        {
            self.index_stale = true;
            return Err(StorageError::CommittedButNotIndexed {
                receipt,
                source: Box::new(source),
            });
        }
        Ok(outcome)
    }

    /// Reads a binary value from the caught-up materialized index.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized key, an index read failure,
    /// or a handle made stale by a prior post-commit index failure.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        validate_key(key)?;
        Ok(self.index.get(key)?)
    }

    /// Returns an immutable vector-space definition when present.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle or malformed materialized state.
    pub fn vector_space(
        &self,
        name: &VectorSpaceName,
    ) -> Result<Option<VectorSpaceDefinition>, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        Ok(self.index.vector_space(name)?)
    }

    /// Returns an immutable lexical-index definition when present.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle or malformed materialized state.
    pub fn lexical_index(
        &self,
        name: &VectorSpaceName,
    ) -> Result<Option<LexicalIndexDefinition>, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        Ok(self.index.lexical_index(name)?)
    }

    /// Reads bounded query-relevant statistics from the rebuildable lexical
    /// projection.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, malformed projection, exhausted
    /// candidate budget, or elapsed deadline.
    pub fn lexical_corpus(
        &self,
        definition: &LexicalIndexDefinition,
        query_tokens: &[String],
        max_candidates: u64,
        timeout: std::time::Duration,
    ) -> Result<LexicalMaterializedCorpus, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        Ok(self
            .index
            .lexical_corpus(definition, query_tokens, max_candidates, timeout)?)
    }

    /// Reads one vector space in strict binary-key order under explicit
    /// candidate and decoded-byte budgets.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, malformed state, or an exhausted
    /// count/byte budget. No partial list is returned.
    pub fn vector_entries(
        &self,
        name: &VectorSpaceName,
        max_candidates: u64,
        max_bytes: u64,
    ) -> Result<Vec<VectorEntry>, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        Ok(self.index.scan_vectors(name, max_candidates, max_bytes)?)
    }

    /// Reads one vector space under count, decoded-byte, and wall-clock bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, malformed state, exhausted
    /// count/byte budget, or elapsed exact-retrieval deadline. No partial list
    /// is returned.
    pub fn vector_entries_with_timeout(
        &self,
        name: &VectorSpaceName,
        max_candidates: u64,
        max_bytes: u64,
        timeout: std::time::Duration,
    ) -> Result<Vec<VectorEntry>, VectorEntriesError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex.into());
        }
        match self
            .index
            .scan_vectors_with_timeout(name, max_candidates, max_bytes, timeout)
        {
            Ok(entries) => Ok(entries),
            Err(VectorScanError::Index(source)) => Err(source.into()),
            Err(VectorScanError::ExactRetrieval(source)) => Err(source.into()),
        }
    }

    /// Scans one bounded page in strict binary-key order.
    ///
    /// `after` is exclusive. A returned `next_after` is present only when at
    /// least one additional entry exists.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, invalid cursor key, invalid page
    /// size, or materialized-index failure.
    pub fn scan_page(&self, after: Option<&[u8]>, limit: usize) -> Result<KvPage, StorageError> {
        match self.scan_page_with_byte_limit(after, limit, u64::MAX) {
            Ok(page) => Ok(page),
            Err(ScanPageError::Storage(source)) => Err(source),
            Err(ScanPageError::ByteBudgetExceeded { .. }) => {
                unreachable!("an in-memory page cannot exceed the u64 byte limit")
            }
        }
    }

    /// Scans one ordered page under an aggregate key/value byte budget.
    ///
    /// The byte limit is checked inside the materialized-index iterator before
    /// a key or value is cloned into the returned page.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, invalid cursor or page size,
    /// exhausted byte budget, or materialized-index failure.
    pub fn scan_page_with_byte_limit(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        max_bytes: u64,
    ) -> Result<KvPage, ScanPageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex.into());
        }
        if let Some(key) = after {
            validate_key(key).map_err(StorageError::from)?;
        }
        if limit == 0 || limit > MAX_SCAN_PAGE_ENTRIES {
            return Err(StorageError::InvalidScanLimit {
                requested: limit,
                maximum: MAX_SCAN_PAGE_ENTRIES,
            }
            .into());
        }
        let (raw, has_more) = match self
            .index
            .scan_after_with_byte_limit(after, limit, max_bytes)
        {
            Err(KvScanError::ByteBudgetExceeded { maximum }) => {
                return Err(ScanPageError::ByteBudgetExceeded { maximum });
            }
            Err(KvScanError::Index(source)) => return Err(StorageError::from(source).into()),
            Ok(raw) => raw,
        };
        let next_after = has_more
            .then(|| raw.last().map(|(key, _)| key.clone()))
            .flatten();
        Ok(KvPage {
            entries: raw
                .into_iter()
                .map(|(key, value)| KvEntry { key, value })
                .collect(),
            next_after,
        })
    }

    /// Returns the internal materialized-index path for diagnostics.
    pub fn index_path(&self) -> PathBuf {
        self.directory.path().join("indexes").join("primary.redb")
    }

    /// Creates or reuses a verified logical snapshot at the current index checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the live index is stale, cannot be streamed, or the
    /// snapshot cannot be synchronized, verified, and atomically promoted.
    pub fn snapshot(&self) -> Result<SnapshotInfo, StorageError> {
        let limits = self.limits.maintenance.clone();
        self.snapshot_with_limits(&limits)
    }

    /// Creates or reuses a verified snapshot under explicit finite limits.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, stale state, exhausted records or
    /// bytes, timeout, verification failure, or I/O failure.
    pub fn snapshot_with_limits(
        &self,
        limits: &MaintenanceLimits,
    ) -> Result<SnapshotInfo, StorageError> {
        limits.validate()?;
        let deadline = OperationDeadline::new(limits.timeout);
        self.snapshot_with_deadline(limits, &deadline)
    }

    fn snapshot_with_deadline(
        &self,
        limits: &MaintenanceLimits,
        deadline: &OperationDeadline,
    ) -> Result<SnapshotInfo, StorageError> {
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        let snapshots = self.directory.path().join("snapshots");
        let temporary = self.directory.path().join("tmp");
        let checkpoint = self.index.checkpoint()?;
        let snapshot_target = self.directory.snapshot_path(checkpoint.sequence);
        let snapshot_is_new = self.directory.reserve_target_entry(
            "snapshots",
            &snapshot_target,
            &self.limits.recovery,
            deadline,
        )?;
        if snapshot_is_new {
            self.directory
                .reserve_directory_entries("tmp", 1, &self.limits.recovery, deadline)?;
        }
        Ok(create_snapshot(
            &self.index,
            &snapshots,
            &temporary,
            self.directory.disk_format_version(),
            &limits.snapshot,
            deadline,
        )?)
    }

    /// Retires the active log prefix behind a verified logical snapshot.
    ///
    /// The new empty segment is synchronized before an immutable manifest
    /// generation selects it. Physical deletion of the retired segment happens
    /// only after that commit and is reported independently.
    ///
    /// # Errors
    ///
    /// Returns an error while preparing the snapshot, segment, or manifest. A
    /// poisoned or stale handle must be reopened before compaction.
    pub fn compact(&mut self) -> Result<CompactionOutcome, StorageError> {
        let limits = self.limits.maintenance.clone();
        self.compact_with_limits(&limits)
    }

    /// Compacts the active generation under one shared finite deadline.
    ///
    /// # Errors
    ///
    /// Returns an error before the manifest commit for invalid limits, stale
    /// state, exhausted snapshot policy, timeout, corruption, or I/O failure.
    pub fn compact_with_limits(
        &mut self,
        limits: &MaintenanceLimits,
    ) -> Result<CompactionOutcome, StorageError> {
        limits.validate()?;
        let deadline = OperationDeadline::new(limits.timeout);
        if self.index_stale {
            return Err(StorageError::StaleIndex);
        }
        let effective_limits = MaintenanceLimits {
            timeout: limits.timeout,
            snapshot: intersect_snapshot_limits(&limits.snapshot, &self.limits.recovery.snapshot),
        };
        let current = self.directory.manifest();
        let checkpoint = self.index.checkpoint()?;
        if checkpoint.sequence == 0 || checkpoint.sequence == current.base_sequence {
            let snapshot = self.snapshot_with_deadline(&effective_limits, &deadline)?;
            return Ok(CompactionOutcome::NoChanges { snapshot });
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StorageError::ManifestGenerationExhausted)?;
        let prospective_manifest = StorageManifest {
            generation,
            active_segment: generation,
            base_sequence: checkpoint.sequence,
            base_digest: checkpoint.digest.unwrap_or([0; 32]),
            snapshot_digest: [0; 32],
        };
        let snapshot_target = self.directory.snapshot_path(checkpoint.sequence);
        let snapshot_is_new = self.directory.reserve_target_entry(
            "snapshots",
            &snapshot_target,
            &self.limits.recovery,
            &deadline,
        )?;
        let next_segment = self.directory.log_path(generation);
        self.directory.reserve_target_entry(
            "log",
            &next_segment,
            &self.limits.recovery,
            &deadline,
        )?;
        let manifest_target = prospective_manifest.path(self.directory.path());
        let manifest_is_new = self.directory.reserve_target_entry(
            "manifest",
            &manifest_target,
            &self.limits.recovery,
            &deadline,
        )?;
        if snapshot_is_new || manifest_is_new {
            self.directory
                .reserve_directory_entries("tmp", 1, &self.limits.recovery, &deadline)?;
        }
        let snapshot = self.snapshot_with_deadline(&effective_limits, &deadline)?;
        let Some(base_digest) = snapshot.checkpoint_digest else {
            return Err(SnapshotError::Invalid {
                reason: "compaction snapshot lacks a checkpoint digest",
            }
            .into());
        };
        let next = StorageManifest {
            generation,
            active_segment: generation,
            base_sequence: snapshot.checkpoint_sequence,
            base_digest,
            snapshot_digest: snapshot.snapshot_digest,
        };
        let (next_log, prepared) = DurableLog::open_file_at_version_with_limits(
            &next_segment,
            next.base_sequence,
            next.base_digest,
            self.directory.disk_format_version(),
            &self.limits.recovery,
            &deadline,
        )?;
        if prepared.valid_bytes != 0 {
            return Err(StorageError::PreparedSegmentNotEmpty { path: next_segment });
        }

        let retired_segment = self.directory.active_log_path();
        deadline.check()?;
        if let Err(source) =
            self.directory
                .commit_manifest_with_limits(next, &self.limits.recovery, &deadline)
        {
            self.index_stale = true;
            return Err(source.into());
        }
        let retired_log = std::mem::replace(&mut self.log, next_log);
        drop(retired_log);
        let retired_segment_removed = remove_retired_segment(&retired_segment);
        Ok(CompactionOutcome::Compacted(CompactionReport {
            generation,
            snapshot,
            retired_segment,
            retired_segment_removed,
        }))
    }
}

fn intersect_snapshot_limits(
    maintenance: &SnapshotReadLimits,
    recovery: &SnapshotReadLimits,
) -> SnapshotReadLimits {
    SnapshotReadLimits {
        file_bytes: maintenance.file_bytes.min(recovery.file_bytes),
        entries: maintenance.entries.min(recovery.entries),
        decoded_bytes: maintenance.decoded_bytes.min(recovery.decoded_bytes),
    }
}

fn remove_retired_segment(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            #[cfg(unix)]
            if let Some(parent) = path.parent()
                && sync_directory(parent).is_err()
            {
                return false;
            }
            true
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn ensure_snapshot_base(
    directory: &DataDirectory,
    index_path: &Path,
    limits: &RecoveryLimits,
    deadline: &OperationDeadline,
) -> Result<(), StorageError> {
    deadline.check()?;
    let manifest = directory.manifest();
    if manifest.base_sequence == 0 {
        return Ok(());
    }
    let snapshot_path = directory.snapshot_path(manifest.base_sequence);
    let verified = verify_snapshot_with_policy(&snapshot_path, &limits.snapshot, deadline)?;
    if verified.checkpoint_sequence != manifest.base_sequence
        || verified.checkpoint_digest != Some(manifest.base_digest)
        || verified.snapshot_digest != manifest.snapshot_digest
    {
        return Err(SnapshotError::Invalid {
            reason: "snapshot does not match active storage manifest",
        }
        .into());
    }
    if index_path.exists() {
        return Ok(());
    }

    let temporary_path = directory
        .path()
        .join("tmp")
        .join(format!("index-restore-{}.redb.tmp", Uuid::now_v7()));
    let mut temporary_guard = TemporaryFileGuard::new(temporary_path.clone());
    let restored = MaterializedIndex::restore_from_snapshot_with_limits(
        &temporary_path,
        &snapshot_path,
        &limits.snapshot,
        limits,
        deadline,
    )?;
    if restored != verified {
        return Err(SnapshotError::Invalid {
            reason: "snapshot changed while rebuilding the materialized index",
        }
        .into());
    }
    deadline.check()?;
    std::fs::rename(&temporary_path, index_path)
        .map_err(SnapshotError::from)
        .map_err(StorageError::from)?;
    temporary_guard.disarm();
    #[cfg(unix)]
    sync_directory(index_path.parent().ok_or(SnapshotError::Invalid {
        reason: "materialized index path has no parent",
    })?)?;
    Ok(())
}

struct TemporaryFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ignored = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StorageError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(SnapshotError::from)
        .map_err(StorageError::from)
}

impl From<MaterializedIndexError> for StorageError {
    fn from(source: MaterializedIndexError) -> Self {
        Self::Index {
            source: Box::new(source),
        }
    }
}

impl From<SnapshotError> for StorageError {
    fn from(source: SnapshotError) -> Self {
        Self::Snapshot {
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        error::Error,
        fs::{self, OpenOptions},
        io::{Seek, SeekFrom, Write},
        time::Duration,
    };

    use hyphae_core::VectorSpaceName;
    use hyphae_query::{FieldPath, Value, encode_document};
    use hyphae_retrieval::{LexicalError, LexicalField, LexicalIndexDefinition};
    use uuid::Uuid;

    use super::{
        CompactionOutcome, DurableLog, ScanPageError, StorageEngine, StorageError, StorageManifest,
    };
    use crate::{
        AppendOutcome, DataDirectory, MaintenanceLimits, ManifestError, Mutation, SnapshotError,
        SnapshotReadLimits, StorageLimitError, StorageLimits, index::MaterializedIndex,
        load_snapshot, load_snapshot_for_migration, load_snapshot_with_timeout,
        storage_limit_from_io, test_support::TestDirectory, verify_snapshot,
    };

    fn lexical_document(text: &str) -> Result<Vec<u8>, hyphae_query::DocumentError> {
        encode_document(&Value::Object(BTreeMap::from([(
            "body".to_owned(),
            Value::String(text.to_owned()),
        )])))
    }

    fn lexical_definition(
        name: &str,
    ) -> Result<LexicalIndexDefinition, Box<dyn std::error::Error>> {
        Ok(LexicalIndexDefinition::new(
            VectorSpaceName::new(name)?,
            vec![LexicalField {
                path: FieldPath::field("body"),
                weight_micros: 1_000_000,
            }],
        )?)
    }

    #[test]
    fn atomic_batches_persist_and_delete() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-kv")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"a", b"one"), Mutation::put(b"b", b"two")],
        )?;
        assert_eq!(opened.storage.get(b"a")?, Some(b"one".to_vec()));
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::delete(b"a")])?;
        drop(opened);

        let reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.storage.get(b"a")?, None);
        assert_eq!(reopened.storage.get(b"b")?, Some(b"two".to_vec()));
        assert_eq!(reopened.recovery.replayed_transactions, 0);
        Ok(())
    }

    #[test]
    fn exact_retry_does_not_reapply_a_batch() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-idempotency")?;
        let transaction_id = Uuid::now_v7();
        let mutations = [Mutation::put(b"key", b"value")];
        let mut opened = StorageEngine::open(temporary.path())?;

        let first = opened.storage.write(transaction_id, &mutations)?;
        let second = opened.storage.write(transaction_id, &mutations)?;
        assert!(matches!(first, AppendOutcome::Committed(_)));
        assert!(matches!(second, AppendOutcome::Existing(_)));
        assert_eq!(opened.storage.get(b"key")?, Some(b"value".to_vec()));

        let conflict = opened
            .storage
            .write(transaction_id, &[Mutation::put(b"key", b"different")]);
        assert!(matches!(
            conflict,
            Err(super::StorageError::Log(
                crate::LogError::IdempotencyConflict { .. }
            ))
        ));
        Ok(())
    }

    #[test]
    fn reopen_replays_a_commit_missing_from_the_index() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-index-replay")?;
        let root = temporary.path().join("data");
        let directory = DataDirectory::open(&root)?;
        let mutation = Mutation::put(b"recovered", b"yes");
        let mut log = directory.open_log()?;
        log.log
            .append_transaction(Uuid::now_v7(), &[mutation.encode()?])?;
        drop(log);
        drop(directory);

        let reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.recovery.replayed_transactions, 1);
        assert_eq!(reopened.storage.get(b"recovered")?, Some(b"yes".to_vec()));
        Ok(())
    }

    #[test]
    fn logical_snapshot_is_stable_and_detects_payload_corruption() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-snapshot")?;
        let mut opened = StorageEngine::open(temporary.path().join("data"))?;
        opened.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::put(b"beta", b"second"),
                Mutation::put(b"alpha", b"first"),
            ],
        )?;

        let created = opened.storage.snapshot()?;
        assert_eq!(created.checkpoint_sequence, 4);
        assert!(created.checkpoint_digest.is_some());
        assert_eq!(created.entry_count, 2);
        assert_eq!(created.receipt_count, 1);
        assert_eq!(verify_snapshot(&created.path)?, created);
        assert_eq!(opened.storage.snapshot()?, created);
        let (witness, receipts) =
            load_snapshot_for_migration(&created.path, &SnapshotReadLimits::default())?;
        assert_eq!(witness.info, created);
        assert_eq!(witness.entries.len(), 2);
        assert_eq!(receipts.0.len(), 1);
        assert_eq!(witness.entries[0].key, b"alpha");
        assert_eq!(witness.entries[0].value, b"first");
        assert!(matches!(
            load_snapshot_with_timeout(
                &created.path,
                &SnapshotReadLimits::default(),
                Duration::ZERO
            ),
            Err(source) if source.is_timeout()
        ));
        assert!(matches!(
            load_snapshot(
                &created.path,
                &SnapshotReadLimits {
                    entries: 1,
                    ..SnapshotReadLimits::default()
                }
            ),
            Err(SnapshotError::EntryLimitExceeded {
                actual: 2,
                maximum: 1
            })
        ));

        let corrupted_path = temporary.path().join("corrupted.hysnap");
        fs::copy(&created.path, &corrupted_path)?;
        let mut corrupted = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&corrupted_path)?;
        corrupted.seek(SeekFrom::End(-1))?;
        corrupted.write_all(&[0xff])?;
        corrupted.sync_all()?;
        drop(corrupted);

        assert!(matches!(
            load_snapshot(
                &corrupted_path,
                &SnapshotReadLimits {
                    entries: 1,
                    ..SnapshotReadLimits::default()
                }
            ),
            Err(SnapshotError::EntryLimitExceeded {
                actual: 2,
                maximum: 1
            })
        ));
        assert!(matches!(
            load_snapshot(
                &corrupted_path,
                &SnapshotReadLimits {
                    decoded_bytes: 0,
                    ..SnapshotReadLimits::default()
                }
            ),
            Err(SnapshotError::DecodedBytesLimitExceeded { maximum: 0 })
        ));
        assert!(matches!(
            verify_snapshot(&corrupted_path),
            Err(SnapshotError::Invalid {
                reason: "CRC32C mismatch"
            })
        ));
        Ok(())
    }

    #[test]
    fn empty_storage_has_a_canonical_empty_snapshot() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-empty-snapshot")?;
        let opened = StorageEngine::open(temporary.path().join("data"))?;

        let snapshot = opened.storage.snapshot()?;
        assert_eq!(snapshot.checkpoint_sequence, 0);
        assert_eq!(snapshot.checkpoint_digest, None);
        assert_eq!(snapshot.entry_count, 0);
        assert_eq!(snapshot.vector_space_count, 0);
        assert_eq!(snapshot.vector_count, 0);
        assert_eq!(snapshot.lexical_index_count, 0);
        assert_eq!(snapshot.receipt_count, 0);
        assert_eq!(snapshot.file_bytes, 136);
        assert_eq!(verify_snapshot(&snapshot.path)?, snapshot);
        Ok(())
    }

    #[test]
    fn snapshot_rebuilds_kv_and_idempotency_state() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-snapshot-restore")?;
        let root = temporary.path().join("data");
        let transaction_id = Uuid::now_v7();
        let mut opened = StorageEngine::open(&root)?;
        let outcome = opened.storage.write(
            transaction_id,
            &[
                Mutation::put(b"alpha", b"one"),
                Mutation::put(b"beta", b"two"),
            ],
        )?;
        let AppendOutcome::Committed(receipt) = outcome else {
            return Err("new transaction was not committed".into());
        };
        let snapshot = opened.storage.snapshot()?;
        drop(opened);

        let restored_path = root.join("tmp/restored.redb");
        assert_eq!(
            MaterializedIndex::restore_from_snapshot(&restored_path, &snapshot.path)?,
            snapshot
        );
        let restored = MaterializedIndex::open(&restored_path)?;
        assert_eq!(restored.get(b"alpha")?, Some(b"one".to_vec()));
        assert_eq!(restored.get(b"beta")?, Some(b"two".to_vec()));
        assert_eq!(restored.receipt(transaction_id)?, Some(receipt));
        Ok(())
    }

    #[test]
    fn compaction_retires_history_and_snapshot_rebuilds_the_index() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-compaction")?;
        let root = temporary.path().join("data");
        let first_id = Uuid::now_v7();
        let mut opened = StorageEngine::open(&root)?;
        let first = opened
            .storage
            .write(first_id, &[Mutation::put(b"before", b"one")])?;
        let AppendOutcome::Committed(first_receipt) = first else {
            return Err("first transaction was not committed".into());
        };

        let compacted = opened.storage.compact()?;
        let CompactionOutcome::Compacted(report) = compacted else {
            return Err("committed history was not compacted".into());
        };
        assert_eq!(report.generation, 2);
        assert!(report.retired_segment_removed);
        assert!(!report.retired_segment.exists());
        assert!(root.join("log/00000000000000000002.hylog").is_file());
        assert!(matches!(
            opened.storage.compact()?,
            CompactionOutcome::NoChanges { .. }
        ));

        assert_eq!(
            opened
                .storage
                .write(first_id, &[Mutation::put(b"before", b"one")])?,
            AppendOutcome::Existing(first_receipt)
        );
        let second_id = Uuid::now_v7();
        let second = opened
            .storage
            .write(second_id, &[Mutation::put(b"after", b"two")])?;
        let AppendOutcome::Committed(second_receipt) = second else {
            return Err("second transaction was not committed".into());
        };
        assert_eq!(
            second_receipt.commit_sequence,
            first_receipt.commit_sequence + 3
        );
        drop(opened);

        fs::remove_file(root.join("indexes/primary.redb"))?;
        let mut rebuilt = StorageEngine::open(&root)?;
        assert_eq!(rebuilt.recovery.replayed_transactions, 1);
        assert_eq!(rebuilt.storage.get(b"before")?, Some(b"one".to_vec()));
        assert_eq!(rebuilt.storage.get(b"after")?, Some(b"two".to_vec()));
        assert_eq!(
            rebuilt
                .storage
                .write(first_id, &[Mutation::put(b"before", b"one")])?,
            AppendOutcome::Existing(first_receipt)
        );
        Ok(())
    }

    #[test]
    fn open_and_compaction_limits_fail_without_advancing_durable_state()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("bounded-open-compaction")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"a", b"one"), Mutation::put(b"b", b"two")],
        )?;
        let active_log = opened.storage.directory.active_log_path();
        let log_bytes = fs::metadata(&active_log)?.len();
        let generation = opened.storage.directory.manifest().generation;

        let too_few_records = MaintenanceLimits {
            snapshot: SnapshotReadLimits {
                entries: 1,
                ..SnapshotReadLimits::default()
            },
            ..MaintenanceLimits::default()
        };
        assert!(matches!(
            opened.storage.compact_with_limits(&too_few_records),
            Err(StorageError::Snapshot { source })
                if matches!(
                    source.as_ref(),
                    SnapshotError::EntryLimitExceeded {
                        actual: 2,
                        maximum: 1
                    }
                )
        ));
        assert_eq!(opened.storage.directory.manifest().generation, generation);
        assert!(active_log.is_file());
        drop(opened);

        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_log_file_bytes: log_bytes - 1,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        assert!(matches!(
            StorageEngine::open_with_limits(&root, limits),
            Err(StorageError::Log(crate::LogError::Io(source)))
                if matches!(
                    storage_limit_from_io(&source),
                    Some(StorageLimitError::LogFileBytesExceeded { actual, maximum })
                        if *actual == log_bytes && *maximum == log_bytes - 1
                )
        ));
        assert_eq!(fs::metadata(&active_log)?.len(), log_bytes);
        Ok(())
    }

    #[test]
    fn compaction_snapshot_policy_is_reopenable_at_the_exact_recovery_limit()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("compaction-recovery-snapshot-intersection")?;

        let exact_root = temporary.path().join("exact");
        let exact_limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                snapshot: SnapshotReadLimits {
                    entries: 2,
                    ..SnapshotReadLimits::default()
                },
                ..crate::RecoveryLimits::default()
            },
            maintenance: MaintenanceLimits {
                snapshot: SnapshotReadLimits {
                    entries: 10,
                    ..SnapshotReadLimits::default()
                },
                ..MaintenanceLimits::default()
            },
        };
        let mut exact = StorageEngine::open_with_limits(&exact_root, exact_limits.clone())?;
        exact.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"a", b"one"), Mutation::put(b"b", b"two")],
        )?;
        assert!(matches!(
            exact.storage.compact()?,
            CompactionOutcome::Compacted(_)
        ));
        drop(exact);
        let reopened = StorageEngine::open_with_limits(&exact_root, exact_limits)?;
        assert_eq!(reopened.storage.get(b"a")?, Some(b"one".to_vec()));
        assert_eq!(reopened.storage.get(b"b")?, Some(b"two".to_vec()));
        drop(reopened);

        let rejected_root = temporary.path().join("exact-plus-one");
        let rejected_limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                snapshot: SnapshotReadLimits {
                    entries: 1,
                    ..SnapshotReadLimits::default()
                },
                ..crate::RecoveryLimits::default()
            },
            maintenance: MaintenanceLimits {
                snapshot: SnapshotReadLimits {
                    entries: 2,
                    ..SnapshotReadLimits::default()
                },
                ..MaintenanceLimits::default()
            },
        };
        let mut rejected =
            StorageEngine::open_with_limits(&rejected_root, rejected_limits.clone())?;
        rejected.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"a", b"one"), Mutation::put(b"b", b"two")],
        )?;
        assert!(matches!(
            rejected.storage.compact(),
            Err(StorageError::Snapshot { source })
                if matches!(
                    source.as_ref(),
                    SnapshotError::EntryLimitExceeded {
                        actual: 2,
                        maximum: 1
                    }
                )
        ));
        assert_eq!(rejected.storage.directory.manifest().generation, 1);
        drop(rejected);
        let reopened = StorageEngine::open_with_limits(&rejected_root, rejected_limits)?;
        assert_eq!(reopened.storage.get(b"a")?, Some(b"one".to_vec()));
        assert_eq!(reopened.storage.get(b"b")?, Some(b"two".to_vec()));
        Ok(())
    }

    #[test]
    fn compaction_reserves_directory_entries_before_creating_any_artifact()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("compaction-directory-reservation")?;
        let root = temporary.path().join("data");
        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_directory_entries: 1,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let mut opened = StorageEngine::open_with_limits(&root, limits.clone())?;
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"key", b"value")])?;

        assert!(matches!(
            opened.storage.compact(),
            Err(StorageError::DataDirectory(
                crate::DataDirectoryError::Manifest(ManifestError::Io(source))
            ))
                if matches!(
                    storage_limit_from_io(&source),
                    Some(StorageLimitError::DirectoryEntriesExceeded { maximum: 1 })
                )
        ));
        assert_eq!(fs::read_dir(root.join("snapshots"))?.count(), 0);
        assert_eq!(fs::read_dir(root.join("log"))?.count(), 1);
        assert_eq!(fs::read_dir(root.join("manifest"))?.count(), 1);
        assert_eq!(fs::read_dir(root.join("tmp"))?.count(), 0);
        assert_eq!(opened.storage.directory.manifest().generation, 1);
        drop(opened);

        let reopened = StorageEngine::open_with_limits(&root, limits)?;
        assert_eq!(reopened.storage.get(b"key")?, Some(b"value".to_vec()));
        Ok(())
    }

    #[test]
    fn existing_compaction_targets_do_not_consume_another_directory_entry()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("compaction-existing-targets")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"key", b"value")])?;
        let snapshot = opened.storage.snapshot()?;
        let base_digest = snapshot
            .checkpoint_digest
            .ok_or("snapshot checkpoint digest is absent")?;
        let (prepared, recovery) = DurableLog::open_file_at(
            opened.storage.directory.log_path(2),
            snapshot.checkpoint_sequence,
            base_digest,
        )?;
        assert_eq!(recovery.valid_bytes, 0);
        drop(prepared);
        drop(opened);

        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_directory_entries: 2,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let mut reopened = StorageEngine::open_with_limits(&root, limits.clone())?;
        assert!(matches!(
            reopened.storage.compact()?,
            CompactionOutcome::Compacted(_)
        ));
        drop(reopened);

        let reopened = StorageEngine::open_with_limits(&root, limits)?;
        assert_eq!(reopened.storage.get(b"key")?, Some(b"value".to_vec()));
        assert_eq!(reopened.storage.directory.manifest().generation, 2);
        Ok(())
    }

    #[test]
    fn orphan_prepared_segment_is_ignored_until_manifest_commit() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-compaction-orphan")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"key", b"value")])?;
        let snapshot = opened.storage.snapshot()?;
        let base_digest = snapshot
            .checkpoint_digest
            .ok_or("snapshot checkpoint digest is absent")?;
        let orphan_path = opened.storage.directory.log_path(2);
        let (orphan, recovery) =
            DurableLog::open_file_at(&orphan_path, snapshot.checkpoint_sequence, base_digest)?;
        assert_eq!(recovery.valid_bytes, 0);
        drop(orphan);
        drop(opened);

        let mut reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.storage.directory.manifest().generation, 1);
        assert_eq!(reopened.storage.get(b"key")?, Some(b"value".to_vec()));
        assert!(matches!(
            reopened.storage.compact()?,
            CompactionOutcome::Compacted(_)
        ));
        assert_eq!(reopened.storage.directory.manifest().generation, 2);
        Ok(())
    }

    #[test]
    fn committed_manifest_wins_before_retired_log_cleanup() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-compaction-committed")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"key", b"value")])?;
        let snapshot = opened.storage.snapshot()?;
        let base_digest = snapshot
            .checkpoint_digest
            .ok_or("snapshot checkpoint digest is absent")?;
        let next = StorageManifest {
            generation: 2,
            active_segment: 2,
            base_sequence: snapshot.checkpoint_sequence,
            base_digest,
            snapshot_digest: snapshot.snapshot_digest,
        };
        let (prepared, recovery) = DurableLog::open_file_at(
            opened.storage.directory.log_path(2),
            next.base_sequence,
            next.base_digest,
        )?;
        assert_eq!(recovery.valid_bytes, 0);
        drop(prepared);
        opened.storage.directory.commit_manifest(next)?;
        let retired_path = opened.storage.directory.log_path(1);
        assert!(retired_path.is_file());
        drop(opened);

        let reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.storage.directory.manifest().generation, 2);
        assert_eq!(reopened.storage.get(b"key")?, Some(b"value".to_vec()));
        assert!(!retired_path.exists());
        Ok(())
    }

    #[test]
    fn uncertain_log_sync_blocks_the_handle_until_recovery() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-injected-log-sync")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened.storage.log.inject_sync_failure();

        let result = opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"recovered", b"yes")]);
        assert!(matches!(
            result,
            Err(StorageError::Log(crate::LogError::Io(_)))
        ));
        assert!(matches!(
            opened.storage.get(b"recovered"),
            Err(StorageError::StaleIndex)
        ));
        assert!(matches!(
            opened.storage.snapshot(),
            Err(StorageError::StaleIndex)
        ));
        assert!(matches!(
            opened.storage.compact(),
            Err(StorageError::StaleIndex)
        ));
        drop(opened);

        let reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.recovery.replayed_transactions, 1);
        assert_eq!(reopened.storage.get(b"recovered")?, Some(b"yes".to_vec()));
        Ok(())
    }

    #[test]
    fn uncertain_manifest_commit_blocks_every_operation_until_reopen() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::new("storage-injected-manifest-commit")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"durable", b"yes")])?;
        opened
            .storage
            .directory
            .inject_manifest_commit_failure_after_write();

        assert!(matches!(
            opened.storage.compact(),
            Err(StorageError::DataDirectory(crate::DataDirectoryError::Io {
                action: "complete injected manifest commit",
                ..
            }))
        ));
        assert!(matches!(
            opened.storage.get(b"durable"),
            Err(StorageError::StaleIndex)
        ));
        assert!(matches!(
            opened
                .storage
                .write(Uuid::now_v7(), &[Mutation::put(b"blocked", b"yes")]),
            Err(StorageError::StaleIndex)
        ));
        assert!(matches!(
            opened.storage.snapshot(),
            Err(StorageError::StaleIndex)
        ));
        assert!(matches!(
            opened.storage.compact(),
            Err(StorageError::StaleIndex)
        ));
        drop(opened);

        let mut reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.storage.directory.manifest().generation, 2);
        assert_eq!(reopened.storage.get(b"durable")?, Some(b"yes".to_vec()));
        reopened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"after", b"reopen")])?;
        assert_eq!(reopened.storage.get(b"after")?, Some(b"reopen".to_vec()));
        Ok(())
    }

    #[test]
    fn post_commit_index_failure_recovers_from_the_log() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-injected-index")?;
        let root = temporary.path().join("data");
        let mut opened = StorageEngine::open(&root)?;
        opened.storage.index.inject_apply_failure();

        let result = opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"durable", b"yes")]);
        assert!(matches!(
            result,
            Err(StorageError::CommittedButNotIndexed { .. })
        ));
        assert!(matches!(
            opened.storage.get(b"durable"),
            Err(StorageError::StaleIndex)
        ));
        drop(opened);

        let reopened = StorageEngine::open(&root)?;
        assert_eq!(reopened.recovery.replayed_transactions, 1);
        assert_eq!(reopened.storage.get(b"durable")?, Some(b"yes".to_vec()));
        Ok(())
    }

    #[test]
    fn lexical_document_limit_rejects_exact_plus_one_before_append_and_rebuilds()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-lexical-document-limit")?;
        let root = temporary.path().join("data");
        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_lexical_documents: 2,
                max_lexical_tokens: 100,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let first = lexical_document("one")?;
        let second = lexical_document("two")?;
        let rejected = lexical_document("three")?;
        let mut opened = StorageEngine::open_with_limits(&root, limits.clone())?;
        opened.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::put(b"first", first.clone()),
                Mutation::put(b"second", second.clone()),
            ],
        )?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::define_lexical_index(lexical_definition(
                "documents.limit",
            )?)],
        )?;

        let active_log = opened.storage.directory.active_log_path();
        let accepted_log_bytes = fs::metadata(&active_log)?.len();
        let result = opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"rejected", rejected)]);
        assert!(matches!(
            result,
            Err(StorageError::Index { source })
                if matches!(
                    source.as_ref(),
                    crate::MaterializedIndexError::Lexical(
                        LexicalError::DocumentBudgetExceeded { maximum: 2 }
                    )
                )
        ));
        assert_eq!(opened.storage.get(b"rejected")?, None);
        assert_eq!(opened.storage.get(b"first")?, Some(first));
        assert_eq!(opened.storage.get(b"second")?, Some(second));
        assert_eq!(fs::metadata(&active_log)?.len(), accepted_log_bytes);
        drop(opened);

        fs::remove_file(root.join("indexes/primary.redb"))?;
        let rebuilt = StorageEngine::open_with_limits(&root, limits)?;
        let rebuilt_definition = lexical_definition("documents.limit")?;
        assert_eq!(rebuilt.storage.get(b"rejected")?, None);
        assert_eq!(
            rebuilt
                .storage
                .lexical_corpus(&rebuilt_definition, &[], 10, Duration::from_secs(1))?
                .document_count,
            2
        );
        Ok(())
    }

    #[test]
    fn lexical_token_limit_rejects_exact_plus_one_before_append_and_rebuilds()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-lexical-token-limit")?;
        let root = temporary.path().join("data");
        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_lexical_documents: 10,
                max_lexical_tokens: 2,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let accepted = lexical_document("one two")?;
        let rejected = lexical_document("one two three")?;
        let mut opened = StorageEngine::open_with_limits(&root, limits.clone())?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"document", accepted.clone())],
        )?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::define_lexical_index(lexical_definition(
                "tokens.limit",
            )?)],
        )?;

        let active_log = opened.storage.directory.active_log_path();
        let accepted_log_bytes = fs::metadata(&active_log)?.len();
        let result = opened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"document", rejected)]);
        assert!(matches!(
            result,
            Err(StorageError::Index { source })
                if matches!(
                    source.as_ref(),
                    crate::MaterializedIndexError::Lexical(
                        LexicalError::TokenBudgetExceeded { maximum: 2 }
                    )
            )
        ));
        assert_eq!(opened.storage.get(b"document")?, Some(accepted.clone()));
        assert_eq!(fs::metadata(&active_log)?.len(), accepted_log_bytes);
        drop(opened);

        fs::remove_file(root.join("indexes/primary.redb"))?;
        let rebuilt = StorageEngine::open_with_limits(&root, limits)?;
        let rebuilt_definition = lexical_definition("tokens.limit")?;
        assert_eq!(rebuilt.storage.get(b"document")?, Some(accepted));
        assert_eq!(
            rebuilt
                .storage
                .lexical_corpus(&rebuilt_definition, &[], 10, Duration::from_secs(1))?
                .token_count,
            2
        );
        Ok(())
    }

    #[test]
    fn lexical_define_over_limit_is_read_only_before_append() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-lexical-define-limit")?;
        let root = temporary.path().join("data");
        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_lexical_documents: 1,
                max_lexical_tokens: 100,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let mut opened = StorageEngine::open_with_limits(&root, limits)?;
        opened.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::put(b"first", lexical_document("one")?),
                Mutation::put(b"second", lexical_document("two")?),
            ],
        )?;

        let active_log = opened.storage.directory.active_log_path();
        let format_path = root.join("FORMAT");
        let accepted_log_bytes = fs::metadata(&active_log)?.len();
        let accepted_format = fs::read(&format_path)?;

        let definition = lexical_definition("define.limit")?;
        let result = opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::define_lexical_index(definition.clone())],
        );
        assert!(matches!(
            result,
            Err(StorageError::Index { source })
                if matches!(
                    source.as_ref(),
                    crate::MaterializedIndexError::Lexical(
                        LexicalError::DocumentBudgetExceeded { maximum: 1 }
                    )
            )
        ));
        assert_eq!(opened.storage.lexical_index(&definition.name)?, None);
        assert_eq!(fs::metadata(&active_log)?.len(), accepted_log_bytes);
        assert_eq!(fs::read(&format_path)?, accepted_format);
        Ok(())
    }

    #[test]
    fn lexical_preflight_simulates_ordered_batches_and_idempotent_retries()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-lexical-ordered-preflight")?;
        let root = temporary.path().join("put-before-define");
        let limits = StorageLimits {
            recovery: crate::RecoveryLimits {
                max_lexical_documents: 1,
                max_lexical_tokens: 2,
                ..crate::RecoveryLimits::default()
            },
            ..StorageLimits::default()
        };
        let definition = lexical_definition("ordered.limit")?;
        let first_transaction = Uuid::now_v7();
        let first_batch = [
            Mutation::put(b"first", lexical_document("one two")?),
            Mutation::define_lexical_index(definition.clone()),
        ];
        let mut opened = StorageEngine::open_with_limits(&root, limits.clone())?;
        let first_outcome = opened.storage.write(first_transaction, &first_batch)?;
        assert!(matches!(first_outcome, AppendOutcome::Committed(_)));

        opened.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::delete(b"first"),
                Mutation::put(b"second", lexical_document("three")?),
            ],
        )?;
        assert_eq!(opened.storage.get(b"first")?, None);
        assert!(opened.storage.get(b"second")?.is_some());
        assert!(matches!(
            opened.storage.write(
                Uuid::now_v7(),
                &[
                    Mutation::put(b"third", lexical_document("four")?),
                    Mutation::delete(b"second"),
                ],
            ),
            Err(StorageError::Index { source })
                if matches!(
                    source.as_ref(),
                    crate::MaterializedIndexError::Lexical(
                        LexicalError::DocumentBudgetExceeded { maximum: 1 }
                    )
                )
        ));
        assert!(matches!(
            opened.storage.write(first_transaction, &first_batch)?,
            AppendOutcome::Existing(_)
        ));
        drop(opened);

        let define_first_root = temporary.path().join("define-before-put");
        let mut define_first = StorageEngine::open_with_limits(&define_first_root, limits)?;
        define_first.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::define_lexical_index(lexical_definition("ordered.second")?),
                Mutation::put(b"document", lexical_document("one two")?),
            ],
        )?;
        assert!(define_first.storage.get(b"document")?.is_some());
        Ok(())
    }

    #[test]
    fn kv_scan_pages_are_strictly_ordered_and_exclusive() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("storage-scan-page")?;
        let mut opened = StorageEngine::open(temporary.path().join("data"))?;
        opened.storage.write(
            Uuid::now_v7(),
            &[
                Mutation::put(b"c", b"three"),
                Mutation::put(b"a", b"one"),
                Mutation::put(b"b", b"two"),
            ],
        )?;

        let first = opened.storage.scan_page(None, 2)?;
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.key.as_slice())
                .collect::<Vec<_>>(),
            [b"a".as_slice(), b"b".as_slice()]
        );
        assert_eq!(first.next_after, Some(b"b".to_vec()));

        let second = opened.storage.scan_page(first.next_after.as_deref(), 2)?;
        assert_eq!(second.entries[0].key, b"c");
        assert_eq!(second.next_after, None);

        let total_bytes = u64::try_from(
            b"a".len() + b"one".len() + b"b".len() + b"two".len() + b"c".len() + b"three".len(),
        )?;
        assert_eq!(
            opened
                .storage
                .scan_page_with_byte_limit(None, 3, total_bytes)?
                .entries
                .len(),
            3
        );
        assert!(matches!(
            opened
                .storage
                .scan_page_with_byte_limit(None, 3, total_bytes - 1),
            Err(ScanPageError::ByteBudgetExceeded { maximum })
                if maximum == total_bytes - 1
        ));
        let first_bytes = u64::try_from(b"a".len() + b"one".len())?;
        let first_only = opened
            .storage
            .scan_page_with_byte_limit(None, 1, first_bytes)?;
        assert_eq!(first_only.entries.len(), 1);
        assert_eq!(first_only.next_after, Some(b"a".to_vec()));
        Ok(())
    }
}
