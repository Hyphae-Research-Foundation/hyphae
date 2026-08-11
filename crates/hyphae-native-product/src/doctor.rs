// SPDX-License-Identifier: AGPL-3.0-only

//! Verified native data-directory diagnosis.

use std::path::{Path, PathBuf};

use hyphae_native_runtime::{NativeDatabase, NativeDirectoryError, NativeRuntimeError};
use hyphae_native_types::Csn;

/// Maximum encoded path bytes accepted by doctor.
pub const MAX_DOCTOR_PATH_BYTES: usize = 4_096;

/// Stable high-level outcome of one native doctor attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorStatus {
    /// Verified open and logical snapshot validation succeeded.
    Healthy,
    /// Another process currently owns the native data directory.
    Busy,
    /// Durable or logical native authority is malformed.
    Corrupt,
    /// A filesystem or device operation failed.
    Io,
}

/// Bounded native doctor request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRequest {
    /// Native data-directory path.
    pub path: PathBuf,
    /// Logical time used for the final all-engine snapshot check.
    pub logical_time_micros: i64,
}

impl DoctorRequest {
    /// Constructs a bounded request.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is empty or exceeds the fixed byte bound.
    pub fn new(
        path: impl Into<PathBuf>,
        logical_time_micros: i64,
    ) -> Result<Self, DoctorRequestError> {
        let path = path.into();
        validate_path(&path)?;
        Ok(Self {
            path,
            logical_time_micros,
        })
    }
}

/// Invalid bounded doctor request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorRequestError {
    /// The supplied path has no encoded bytes.
    EmptyPath,
    /// The supplied path exceeds [`MAX_DOCTOR_PATH_BYTES`].
    PathTooLong,
}

impl std::fmt::Display for DoctorRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyPath => "native doctor path is empty",
            Self::PathTooLong => "native doctor path exceeds the product limit",
        })
    }
}

impl std::error::Error for DoctorRequestError {}

/// Bounded recovery evidence emitted by a successful verified open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRecovery {
    /// Latest all-engine commit reconstructed by open.
    pub visible_csn: Option<Csn>,
    /// Replayed committed suffix transactions.
    pub replayed_transactions: usize,
    /// Incomplete page tail removed by recovery.
    pub page_tail_bytes_removed: u64,
    /// Incomplete WAL tail removed by recovery.
    pub wal_tail_bytes_removed: u64,
    /// Verified retained WAL bytes.
    pub retained_wal_bytes: u64,
    /// Verified immutable manifests.
    pub manifest_count: usize,
    /// Verified immutable blobs.
    pub blob_count: usize,
    /// Complete open and recovery duration.
    pub open_time_micros: u64,
}

/// Typed native doctor report. Paths and engine error strings are never exposed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    /// Classified health state.
    pub status: DoctorStatus,
    /// Whether an exclusive runtime open completed.
    pub verified_open: bool,
    /// Whether a complete all-engine snapshot was materialized and validated.
    pub snapshot_verified: bool,
    /// Stable directory lineage, only after verified open.
    pub directory_lineage: Option<[u8; 24]>,
    /// Recovery evidence, only after verified open.
    pub recovery: Option<DoctorRecovery>,
    /// Stable registry version captured for this doctor process.
    pub telemetry_registry_version: u16,
    /// Process identity shared by all registries in this process.
    pub process_start_identity: u128,
    /// Doctor-attempt identity unique within this process.
    pub session_start_identity: u128,
}

impl DoctorReport {
    /// Returns whether the complete doctor attempt was healthy.
    pub const fn is_healthy(&self) -> bool {
        matches!(self.status, DoctorStatus::Healthy)
    }
}

/// Exclusively opens, recovers, and validates a native data directory.
///
/// All operational outcomes are represented by [`DoctorStatus`]. Only invalid
/// request construction is fallible before this function is called.
pub fn doctor(request: &DoctorRequest) -> DoctorReport {
    let telemetry =
        crate::TelemetryRegistry::new(crate::TelemetryConfig::new(0).unwrap_or_default());
    let identity = telemetry.snapshot(request.logical_time_micros, None);
    match NativeDatabase::open(&request.path) {
        Ok(database) => {
            let recovery = database.recovery_report();
            let evidence = DoctorRecovery {
                visible_csn: recovery.visible_csn,
                replayed_transactions: recovery.replayed_transactions,
                page_tail_bytes_removed: recovery.page_tail_bytes_removed,
                wal_tail_bytes_removed: recovery.wal_tail_bytes_removed,
                retained_wal_bytes: recovery.retained_wal_bytes,
                manifest_count: recovery.manifest_count,
                blob_count: recovery.blob_count,
                open_time_micros: duration_micros(recovery.open_time),
            };
            let lineage = database.directory_identity().lineage().encode();
            match database.snapshot(request.logical_time_micros) {
                Ok(_) => DoctorReport {
                    status: DoctorStatus::Healthy,
                    verified_open: true,
                    snapshot_verified: true,
                    directory_lineage: Some(lineage),
                    recovery: Some(evidence),
                    telemetry_registry_version: identity.registry_version,
                    process_start_identity: identity.process_start_identity,
                    session_start_identity: identity.session_start_identity,
                },
                Err(error) => classified(&error, true, Some(lineage), Some(evidence), &identity),
            }
        }
        Err(error) => classified(&error, false, None, None, &identity),
    }
}

fn classified(
    error: &NativeRuntimeError,
    verified_open: bool,
    directory_lineage: Option<[u8; 24]>,
    recovery: Option<DoctorRecovery>,
    identity: &crate::TelemetrySnapshot,
) -> DoctorReport {
    let status = if matches!(
        error,
        NativeRuntimeError::Directory(NativeDirectoryError::AlreadyLocked(_))
    ) {
        DoctorStatus::Busy
    } else if error.is_io() {
        DoctorStatus::Io
    } else {
        DoctorStatus::Corrupt
    };
    DoctorReport {
        status,
        verified_open,
        snapshot_verified: false,
        directory_lineage,
        recovery,
        telemetry_registry_version: identity.registry_version,
        process_start_identity: identity.process_start_identity,
        session_start_identity: identity.session_start_identity,
    }
}

fn validate_path(path: &Path) -> Result<(), DoctorRequestError> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length == 0 {
        Err(DoctorRequestError::EmptyPath)
    } else if length > MAX_DOCTOR_PATH_BYTES {
        Err(DoctorRequestError::PathTooLong)
    } else {
        Ok(())
    }
}

fn duration_micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
