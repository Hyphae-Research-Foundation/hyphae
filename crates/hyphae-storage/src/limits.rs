// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::snapshot::SnapshotReadLimits;

/// Complete finite policy for opening and maintaining one data directory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StorageLimits {
    /// Limits shared by directory validation, log recovery, and index replay.
    pub recovery: RecoveryLimits,
    /// Limits shared by snapshot creation and online compaction.
    pub maintenance: MaintenanceLimits,
}

impl StorageLimits {
    pub(crate) fn compatibility() -> Self {
        Self {
            recovery: RecoveryLimits::compatibility(),
            maintenance: MaintenanceLimits::compatibility(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), StorageLimitError> {
        self.recovery.validate()?;
        self.maintenance.validate()
    }
}

/// Finite policy for one complete open/recovery operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryLimits {
    /// Cooperative end-to-end open/recovery timeout.
    pub timeout: Duration,
    /// Maximum entries inspected in any owned metadata directory.
    pub max_directory_entries: u64,
    /// Maximum complete active log segment length.
    pub max_log_file_bytes: u64,
    /// Maximum complete frames inspected in the active segment.
    pub max_log_frames: u64,
    /// Maximum unique committed transactions retained for replay.
    pub max_transactions: u64,
    /// Maximum operation frames retained across committed transactions.
    pub max_operations: u64,
    /// Maximum aggregate decoded operation payload bytes retained for replay.
    pub max_decoded_operation_bytes: u64,
    /// Snapshot file, logical-record, and decoded-byte limits during restore.
    pub snapshot: SnapshotReadLimits,
    /// Maximum durable documents inspected while rebuilding one lexical index.
    pub max_lexical_documents: u64,
    /// Maximum normalized tokens retained while rebuilding one lexical index.
    pub max_lexical_tokens: u64,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_directory_entries: 1_000_000,
            max_log_file_bytes: 2 * 1024 * 1024 * 1024,
            max_log_frames: 1_000_000,
            max_transactions: 1_000_000,
            max_operations: 1_000_000,
            max_decoded_operation_bytes: 1024 * 1024 * 1024,
            snapshot: SnapshotReadLimits::default(),
            max_lexical_documents: 1_000_000,
            max_lexical_tokens: 10_000_000,
        }
    }
}

impl RecoveryLimits {
    pub(crate) fn compatibility() -> Self {
        Self {
            timeout: Duration::MAX,
            max_directory_entries: u64::MAX,
            max_log_file_bytes: u64::MAX,
            max_log_frames: u64::MAX,
            max_transactions: u64::MAX,
            max_operations: u64::MAX,
            max_decoded_operation_bytes: u64::MAX,
            snapshot: SnapshotReadLimits {
                file_bytes: u64::MAX,
                entries: u64::MAX,
                decoded_bytes: u64::MAX,
            },
            max_lexical_documents: u64::MAX,
            max_lexical_tokens: u64::MAX,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), StorageLimitError> {
        require_nonzero_duration(self.timeout, "recovery.timeout")?;
        for (name, value) in [
            ("recovery.max_directory_entries", self.max_directory_entries),
            ("recovery.max_log_file_bytes", self.max_log_file_bytes),
            ("recovery.max_log_frames", self.max_log_frames),
            ("recovery.max_transactions", self.max_transactions),
            ("recovery.max_operations", self.max_operations),
            (
                "recovery.max_decoded_operation_bytes",
                self.max_decoded_operation_bytes,
            ),
            ("recovery.snapshot.file_bytes", self.snapshot.file_bytes),
            ("recovery.snapshot.entries", self.snapshot.entries),
            (
                "recovery.snapshot.decoded_bytes",
                self.snapshot.decoded_bytes,
            ),
            ("recovery.max_lexical_documents", self.max_lexical_documents),
            ("recovery.max_lexical_tokens", self.max_lexical_tokens),
        ] {
            require_nonzero(value, name)?;
        }
        Ok(())
    }
}

/// Finite policy for one snapshot or online compaction operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceLimits {
    /// Cooperative end-to-end snapshot/compaction timeout.
    pub timeout: Duration,
    /// Maximum snapshot file, logical records, and decoded logical bytes.
    pub snapshot: SnapshotReadLimits,
}

impl Default for MaintenanceLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            snapshot: SnapshotReadLimits::default(),
        }
    }
}

impl MaintenanceLimits {
    fn compatibility() -> Self {
        Self {
            timeout: Duration::MAX,
            snapshot: SnapshotReadLimits {
                file_bytes: u64::MAX,
                entries: u64::MAX,
                decoded_bytes: u64::MAX,
            },
        }
    }

    pub(crate) fn validate(&self) -> Result<(), StorageLimitError> {
        require_nonzero_duration(self.timeout, "maintenance.timeout")?;
        for (name, value) in [
            ("maintenance.snapshot.file_bytes", self.snapshot.file_bytes),
            ("maintenance.snapshot.entries", self.snapshot.entries),
            (
                "maintenance.snapshot.decoded_bytes",
                self.snapshot.decoded_bytes,
            ),
        ] {
            require_nonzero(value, name)?;
        }
        Ok(())
    }
}

/// Failure of a finite storage recovery or maintenance policy.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StorageLimitError {
    /// A configured bound is zero.
    #[error("storage limit must be positive: {name}")]
    ZeroLimit {
        /// Stable field name.
        name: &'static str,
    },
    /// The shared cooperative deadline expired.
    #[error("storage operation timed out")]
    TimedOut,
    /// An owned metadata directory contains too many entries.
    #[error("storage directory entry limit exceeded: {maximum}")]
    DirectoryEntriesExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// The active log file exceeds policy before scan.
    #[error("log file byte limit exceeded: {actual} > {maximum}")]
    LogFileBytesExceeded {
        /// Observed file length.
        actual: u64,
        /// Configured maximum.
        maximum: u64,
    },
    /// The active log contains too many complete frames.
    #[error("log frame limit exceeded: {maximum}")]
    LogFramesExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// Recovery retained too many unique transactions.
    #[error("recovery transaction limit exceeded: {maximum}")]
    TransactionsExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// Recovery retained too many operation frames.
    #[error("recovery operation limit exceeded: {maximum}")]
    OperationsExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// Recovery retained too many aggregate operation payload bytes.
    #[error("recovery decoded operation byte limit exceeded: {maximum}")]
    DecodedOperationBytesExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// A lexical rebuild inspected too many durable documents.
    #[error("lexical rebuild document limit exceeded: {maximum}")]
    LexicalDocumentsExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// A lexical rebuild retained too many normalized tokens.
    #[error("lexical rebuild token limit exceeded: {maximum}")]
    LexicalTokensExceeded {
        /// Configured maximum.
        maximum: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct OperationDeadline {
    started: Instant,
    timeout: Duration,
}

impl OperationDeadline {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    pub(crate) fn check(&self) -> Result<(), StorageLimitError> {
        if self.started.elapsed() >= self.timeout {
            Err(StorageLimitError::TimedOut)
        } else {
            Ok(())
        }
    }
}

pub(crate) fn limit_io_error(source: StorageLimitError) -> io::Error {
    let kind = if matches!(source, StorageLimitError::TimedOut) {
        io::ErrorKind::TimedOut
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, source)
}

/// Recovers a typed finite-policy failure carried through an existing I/O
/// error variant.
///
/// Limited storage entry points preserve the exhaustive public error enums
/// published in 0.2.0 by retaining [`StorageLimitError`] as the I/O error
/// source. Callers that need typed policy handling can use this helper while
/// legacy exhaustive matches remain source-compatible.
pub fn storage_limit_from_io(source: &io::Error) -> Option<&StorageLimitError> {
    source
        .get_ref()
        .and_then(|source| source.downcast_ref::<StorageLimitError>())
}

fn require_nonzero(value: u64, name: &'static str) -> Result<(), StorageLimitError> {
    if value == 0 {
        Err(StorageLimitError::ZeroLimit { name })
    } else {
        Ok(())
    }
}

fn require_nonzero_duration(value: Duration, name: &'static str) -> Result<(), StorageLimitError> {
    if value.is_zero() {
        Err(StorageLimitError::ZeroLimit { name })
    } else {
        Ok(())
    }
}
