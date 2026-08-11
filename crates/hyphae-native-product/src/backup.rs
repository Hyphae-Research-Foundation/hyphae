// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded product backup, verification, and restore wrappers.

#![allow(clippy::result_large_err)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use hyphae_native_runtime::{
    NativeBackupError, NativeBackupInfo, NativeBackupLimits, NativeDatabase,
    restore_native_backup_with_limits, verify_native_backup_with_limits,
};

use crate::{DoctorReport, DoctorRequest, DoctorStatus, ProductError};

/// Maximum encoded backup or restore path accepted by the product wrapper.
pub const MAX_BACKUP_PATH_BYTES: usize = 4_096;

/// Cooperative product cancellation flag.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: std::sync::Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates an uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Already-running runtime phases remain atomic.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Product backup/restore limits independent of runtime defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct BackupLimits {
    /// Maximum regular files.
    pub max_files: usize,
    /// Maximum subdirectories.
    pub max_directories: usize,
    /// Maximum sum of file lengths.
    pub max_total_bytes: u64,
    /// Maximum relative path bytes.
    pub max_path_bytes: usize,
    /// Maximum encoded manifest bytes.
    pub max_manifest_bytes: u64,
}

impl Default for BackupLimits {
    fn default() -> Self {
        let limits = NativeBackupLimits::default();
        Self::from_runtime(limits)
    }
}

impl BackupLimits {
    const fn from_runtime(limits: NativeBackupLimits) -> Self {
        Self {
            max_files: limits.max_files,
            max_directories: limits.max_directories,
            max_total_bytes: limits.max_total_bytes,
            max_path_bytes: limits.max_path_bytes,
            max_manifest_bytes: limits.max_manifest_bytes,
        }
    }

    fn runtime(self) -> NativeBackupLimits {
        NativeBackupLimits {
            max_files: self.max_files,
            max_directories: self.max_directories,
            max_total_bytes: self.max_total_bytes,
            max_path_bytes: self.max_path_bytes,
            max_manifest_bytes: self.max_manifest_bytes,
        }
    }

    const fn valid(self) -> bool {
        self.max_files > 0
            && self.max_directories > 0
            && self.max_total_bytes > 0
            && self.max_path_bytes > 0
            && self.max_path_bytes <= MAX_BACKUP_PATH_BYTES
            && self.max_manifest_bytes > 0
    }
}

/// Bounded backup request.
#[derive(Clone, Debug)]
pub struct BackupRequest {
    /// New backup destination.
    pub destination: PathBuf,
    /// Complete resource limits.
    pub limits: BackupLimits,
    /// Optional cooperative cancellation flag.
    pub cancellation: Option<CancellationToken>,
}

impl BackupRequest {
    /// Constructs a bounded backup request with default resource limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized destination path.
    pub fn new(destination: impl Into<PathBuf>) -> Result<Self, BackupRequestError> {
        let destination = destination.into();
        validate_path(&destination)?;
        Ok(Self {
            destination,
            limits: BackupLimits::default(),
            cancellation: None,
        })
    }
}

/// Bounded offline verification request.
#[derive(Clone, Debug)]
pub struct VerifyBackupRequest {
    /// Backup directory.
    pub backup: PathBuf,
    /// Complete resource limits.
    pub limits: BackupLimits,
    /// Optional cooperative cancellation flag.
    pub cancellation: Option<CancellationToken>,
}

impl VerifyBackupRequest {
    /// Constructs a bounded verification request with default resource limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized backup path.
    pub fn new(backup: impl Into<PathBuf>) -> Result<Self, BackupRequestError> {
        let backup = backup.into();
        validate_path(&backup)?;
        Ok(Self {
            backup,
            limits: BackupLimits::default(),
            cancellation: None,
        })
    }
}

/// Bounded restore request.
#[derive(Clone, Debug)]
pub struct RestoreRequest {
    /// Verified source backup.
    pub backup: PathBuf,
    /// New native data-directory destination.
    pub destination: PathBuf,
    /// Complete resource limits.
    pub limits: BackupLimits,
    /// Logical time used by mandatory doctor-after-restore.
    pub doctor_logical_time_micros: i64,
    /// Optional cooperative cancellation flag.
    pub cancellation: Option<CancellationToken>,
}

impl RestoreRequest {
    /// Constructs a bounded restore request with default resource limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized path.
    pub fn new(
        backup: impl Into<PathBuf>,
        destination: impl Into<PathBuf>,
    ) -> Result<Self, BackupRequestError> {
        let backup = backup.into();
        let destination = destination.into();
        validate_path(&backup)?;
        validate_path(&destination)?;
        Ok(Self {
            backup,
            destination,
            limits: BackupLimits::default(),
            doctor_logical_time_micros: 0,
            cancellation: None,
        })
    }
}

/// Invalid bounded backup request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupRequestError {
    /// One supplied path is empty.
    EmptyPath,
    /// One supplied path exceeds [`MAX_BACKUP_PATH_BYTES`].
    PathTooLong,
    /// A configured resource bound is zero or outside the product maximum.
    InvalidLimits,
}

impl std::fmt::Display for BackupRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyPath => "native backup path is empty",
            Self::PathTooLong => "native backup path exceeds the product limit",
            Self::InvalidLimits => "native backup limits are invalid",
        })
    }
}

impl std::error::Error for BackupRequestError {}

/// Typed backup lifecycle phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupPhase {
    /// Request and cancellation admission.
    ValidatingRequest,
    /// Runtime checkpoint, copy, and staging verification.
    CheckpointingAndCopying,
    /// Destination was atomically promoted by the runtime.
    Promoted,
    /// Final promoted backup verification.
    VerifyingPromotedBackup,
    /// Operation completed successfully.
    Complete,
}

/// Typed offline backup verification phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyBackupPhase {
    /// Request and cancellation admission.
    ValidatingRequest,
    /// Manifest, inventory, sizes, and digests are being verified.
    Verifying,
    /// Verification completed successfully.
    Complete,
}

/// Typed restore lifecycle phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestorePhase {
    /// Request and cancellation admission.
    ValidatingRequest,
    /// Source backup verification.
    VerifyingBackup,
    /// Runtime copy and logical staging validation. The runtime checks caller
    /// cancellation immediately before this phase, which contains promotion.
    RestoringAndPromoting,
    /// Destination promotion completed.
    Promoted,
    /// Mandatory verified doctor-after-restore.
    DoctorAfterRestore,
    /// Restore and doctor completed successfully.
    Complete,
}

/// Progress callback control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressControl {
    /// Continue to the next phase.
    Continue,
    /// Cancel before entering the next cancellable phase.
    Cancel,
}

/// Product-owned backup metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupInfo {
    /// Promoted or verified backup path.
    pub path: PathBuf,
    /// Checkpoint visible CSN as a primitive stable value.
    pub visible_csn: u64,
    /// Checkpoint manifest digest.
    pub checkpoint_digest: [u8; 32],
    /// Verified regular-file count.
    pub file_count: usize,
    /// Verified sum of file lengths.
    pub total_bytes: u64,
}

impl From<NativeBackupInfo> for BackupInfo {
    fn from(value: NativeBackupInfo) -> Self {
        Self {
            path: value.path,
            visible_csn: value.visible_csn,
            checkpoint_digest: value.checkpoint_digest,
            file_count: value.file_count,
            total_bytes: value.total_bytes,
        }
    }
}

/// Product restore evidence including mandatory post-restore diagnosis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreInfo {
    /// Promoted native data-directory path.
    pub data_path: PathBuf,
    /// Verified source backup metadata.
    pub backup: BackupInfo,
    /// Healthy report from the promoted directory.
    pub doctor: DoctorReport,
    /// Complete ordered progress observed by the product operation.
    pub phases: Vec<RestorePhase>,
}

/// Backup, verification, or restore failure with a safe typed phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupProductError {
    /// Bounded request validation failed.
    InvalidRequest(BackupRequestError),
    /// Caller cancellation was observed before promotion began.
    Cancelled,
    /// Runtime backup failure mapped to the stable product registry.
    Backup {
        /// Last product phase entered.
        phase: BackupPhase,
        /// Stable product error.
        error: Box<ProductError>,
    },
    /// Runtime verification failure mapped to the stable product registry.
    Verification {
        /// Last product phase entered.
        phase: VerifyBackupPhase,
        /// Stable product error.
        error: Box<ProductError>,
    },
    /// Runtime restore failure mapped to the stable product registry.
    Restore {
        /// Last product phase entered.
        phase: RestorePhase,
        /// Stable product error.
        error: Box<ProductError>,
    },
    /// Promoted restore did not pass mandatory doctor.
    DoctorAfterRestore {
        /// Typed failed doctor report.
        report: DoctorReport,
    },
}

impl std::fmt::Display for BackupProductError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(error) => error.fmt(formatter),
            Self::Cancelled => formatter.write_str("native backup operation was cancelled"),
            Self::Backup { error, .. }
            | Self::Verification { error, .. }
            | Self::Restore { error, .. } => error.fmt(formatter),
            Self::DoctorAfterRestore { .. } => {
                formatter.write_str("restored native directory failed doctor verification")
            }
        }
    }
}

impl std::error::Error for BackupProductError {}

/// Creates and then independently verifies one promoted native backup.
///
/// # Errors
///
/// Returns typed validation, cancellation, runtime, or verification failures.
pub fn backup(
    database: &mut NativeDatabase,
    request: &BackupRequest,
    mut progress: impl FnMut(BackupPhase) -> ProgressControl,
) -> Result<BackupInfo, BackupProductError> {
    validate_request(&request.destination, request.limits)?;
    checkpoint(
        BackupPhase::ValidatingRequest,
        request.cancellation.as_ref(),
        &mut progress,
    )?;
    checkpoint(
        BackupPhase::CheckpointingAndCopying,
        request.cancellation.as_ref(),
        &mut progress,
    )?;
    let created = database
        .backup(&request.destination, request.limits.runtime())
        .map_err(|error| BackupProductError::Backup {
            phase: BackupPhase::CheckpointingAndCopying,
            error: Box::new(map_backup_error(error)),
        })?;
    let _ignored = progress(BackupPhase::Promoted);
    let _ignored = progress(BackupPhase::VerifyingPromotedBackup);
    let verified = verify_native_backup_with_limits(&request.destination, request.limits.runtime())
        .map_err(|error| BackupProductError::Backup {
            phase: BackupPhase::VerifyingPromotedBackup,
            error: Box::new(map_backup_error(error)),
        })?;
    if created.visible_csn != verified.visible_csn
        || created.checkpoint_digest != verified.checkpoint_digest
    {
        return Err(BackupProductError::Backup {
            phase: BackupPhase::VerifyingPromotedBackup,
            error: Box::new(ProductError::from_code(
                crate::ProductErrorCode::BackupInvalid,
            )),
        });
    }
    let _ignored = progress(BackupPhase::Complete);
    Ok(verified.into())
}

/// Verifies one native backup without opening a live database.
///
/// # Errors
///
/// Returns typed validation, cancellation, or runtime verification failures.
pub fn verify_backup(
    request: &VerifyBackupRequest,
    mut progress: impl FnMut(VerifyBackupPhase) -> ProgressControl,
) -> Result<BackupInfo, BackupProductError> {
    validate_request(&request.backup, request.limits)?;
    verify_checkpoint(
        VerifyBackupPhase::ValidatingRequest,
        request.cancellation.as_ref(),
        &mut progress,
    )?;
    verify_checkpoint(
        VerifyBackupPhase::Verifying,
        request.cancellation.as_ref(),
        &mut progress,
    )?;
    let verified = verify_native_backup_with_limits(&request.backup, request.limits.runtime())
        .map_err(|error| BackupProductError::Verification {
            phase: VerifyBackupPhase::Verifying,
            error: Box::new(map_backup_error(error)),
        })?;
    let _ignored = progress(VerifyBackupPhase::Complete);
    Ok(verified.into())
}

/// Verifies and restores to a new path, then mandates doctor-after-restore.
///
/// Cancellation is checked after source verification and immediately before
/// entering the runtime restore call. The runtime performs staging validation
/// and atomic promotion as one non-cancellable integrity phase.
///
/// # Errors
///
/// Returns typed validation, cancellation, restore, or doctor failures.
pub fn restore(
    request: &RestoreRequest,
    mut progress: impl FnMut(RestorePhase) -> ProgressControl,
) -> Result<RestoreInfo, BackupProductError> {
    let mut phases = Vec::new();
    validate_request(&request.backup, request.limits)?;
    validate_request(&request.destination, request.limits)?;
    restore_checkpoint(
        RestorePhase::ValidatingRequest,
        request.cancellation.as_ref(),
        &mut |phase| {
            phases.push(phase);
            progress(phase)
        },
    )?;
    restore_checkpoint(
        RestorePhase::VerifyingBackup,
        request.cancellation.as_ref(),
        &mut |phase| {
            phases.push(phase);
            progress(phase)
        },
    )?;
    let verified = verify_native_backup_with_limits(&request.backup, request.limits.runtime())
        .map_err(|error| BackupProductError::Restore {
            phase: RestorePhase::VerifyingBackup,
            error: Box::new(map_backup_error(error)),
        })?;
    restore_checkpoint(
        RestorePhase::RestoringAndPromoting,
        request.cancellation.as_ref(),
        &mut |phase| {
            phases.push(phase);
            progress(phase)
        },
    )?;
    let restored = restore_native_backup_with_limits(
        &request.backup,
        &request.destination,
        request.limits.runtime(),
    )
    .map_err(|error| BackupProductError::Restore {
        phase: RestorePhase::RestoringAndPromoting,
        error: Box::new(map_backup_error(error)),
    })?;
    phases.push(RestorePhase::Promoted);
    let _ignored = progress(RestorePhase::Promoted);
    phases.push(RestorePhase::DoctorAfterRestore);
    let _ignored = progress(RestorePhase::DoctorAfterRestore);
    let doctor_request =
        DoctorRequest::new(&restored.data_path, request.doctor_logical_time_micros)
            .map_err(|_| BackupProductError::InvalidRequest(BackupRequestError::PathTooLong))?;
    let report = crate::doctor(&doctor_request);
    if report.status != DoctorStatus::Healthy {
        return Err(BackupProductError::DoctorAfterRestore { report });
    }
    phases.push(RestorePhase::Complete);
    let _ignored = progress(RestorePhase::Complete);
    Ok(RestoreInfo {
        data_path: restored.data_path,
        backup: verified.into(),
        doctor: report,
        phases,
    })
}

fn checkpoint(
    phase: BackupPhase,
    cancellation: Option<&CancellationToken>,
    progress: &mut impl FnMut(BackupPhase) -> ProgressControl,
) -> Result<(), BackupProductError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled)
        || progress(phase) == ProgressControl::Cancel
    {
        Err(BackupProductError::Cancelled)
    } else {
        Ok(())
    }
}

fn verify_checkpoint(
    phase: VerifyBackupPhase,
    cancellation: Option<&CancellationToken>,
    progress: &mut impl FnMut(VerifyBackupPhase) -> ProgressControl,
) -> Result<(), BackupProductError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled)
        || progress(phase) == ProgressControl::Cancel
    {
        Err(BackupProductError::Cancelled)
    } else {
        Ok(())
    }
}

fn restore_checkpoint(
    phase: RestorePhase,
    cancellation: Option<&CancellationToken>,
    progress: &mut impl FnMut(RestorePhase) -> ProgressControl,
) -> Result<(), BackupProductError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled)
        || progress(phase) == ProgressControl::Cancel
    {
        Err(BackupProductError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_request(path: &Path, limits: BackupLimits) -> Result<(), BackupProductError> {
    validate_path(path).map_err(BackupProductError::InvalidRequest)?;
    if limits.valid() {
        Ok(())
    } else {
        Err(BackupProductError::InvalidRequest(
            BackupRequestError::InvalidLimits,
        ))
    }
}

fn validate_path(path: &Path) -> Result<(), BackupRequestError> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length == 0 {
        Err(BackupRequestError::EmptyPath)
    } else if length > MAX_BACKUP_PATH_BYTES {
        Err(BackupRequestError::PathTooLong)
    } else {
        Ok(())
    }
}

fn map_backup_error(error: NativeBackupError) -> ProductError {
    match error {
        NativeBackupError::DestinationExists(_) | NativeBackupError::DestinationInsideSource(_) => {
            ProductError::from_code(crate::ProductErrorCode::DataDirectoryExists)
        }
        NativeBackupError::LimitExceeded { .. } => {
            ProductError::from_code(crate::ProductErrorCode::LimitExceeded)
        }
        NativeBackupError::Io { .. } => ProductError::from_code(crate::ProductErrorCode::Io),
        NativeBackupError::Runtime(source) => source.into(),
        NativeBackupError::LogicalValidation { source, .. } => (*source).into(),
        NativeBackupError::Invalid { .. } | NativeBackupError::ManifestJson { .. } => {
            ProductError::from_code(crate::ProductErrorCode::BackupInvalid)
        }
    }
}
