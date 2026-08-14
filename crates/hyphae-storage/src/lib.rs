// SPDX-License-Identifier: AGPL-3.0-only

//! Durable log, recovery, snapshot, and materialized-index implementation.

mod backup;
mod data_directory;
mod engine;
mod index;
mod limits;
mod log;
mod manifest;
mod mutation;
mod snapshot;

pub use backup::{BackupError, BackupInfo, RestoreInfo, restore_backup, verify_backup};
pub use data_directory::{DataDirectory, DataDirectoryError};
pub use engine::{
    CompactionOutcome, CompactionReport, KvEntry, KvPage, MAX_SCAN_PAGE_ENTRIES, OpenedStorage,
    ScanPageError, StorageEngine, StorageError, StorageRecoveryReport, VectorEntriesError,
};
pub use index::{MaterializedIndexError, VectorEntry};
pub use limits::{
    MaintenanceLimits, RecoveryLimits, StorageLimitError, StorageLimits, storage_limit_from_io,
};
pub use log::{
    AppendOutcome, CommitReceipt, DurableLog, LogError, OpenedLog, RecoveredTransaction,
    RecoveryReport,
};
pub use manifest::ManifestError;
pub use mutation::{MAX_KEY_BYTES, Mutation, MutationError};
pub use snapshot::{
    SnapshotContents, SnapshotEntry, SnapshotError, SnapshotInfo, SnapshotReadLimits,
    SnapshotReceipts, SnapshotVectorEntry, load_snapshot, load_snapshot_for_migration,
    load_snapshot_with_timeout, open_verified_snapshot_with_limits, verify_snapshot,
    verify_snapshot_with_limits,
};

#[cfg(test)]
mod test_support {
    use std::{fs, io, path::PathBuf};

    pub(crate) struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        pub(crate) fn new(name: &str) -> io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "hyphae-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        pub(crate) fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }
}
