// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded, offline backup and restore for the native data directory.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ADMINISTRATIVE_MEMORY_BYTES, CheckpointReceipt, GovernorRequest, NativeDatabase,
    NativeResourceGovernor, NativeRuntimeError, WorkloadClass, admit_governor_work,
};

const MANIFEST_FILE: &str = "NATIVE_BACKUP.json";
const DATA_DIRECTORY: &str = "data";
const BACKUP_KIND: &str = "hyphae-native-directory-backup";
const BACKUP_VERSION: u16 = 1;
static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);

/// Resource limits applied while creating, verifying, and restoring a native backup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct NativeBackupLimits {
    /// Maximum number of regular files in the data directory.
    pub max_files: usize,
    /// Maximum number of subdirectories in the data directory.
    pub max_directories: usize,
    /// Maximum sum of file lengths.
    pub max_total_bytes: u64,
    /// Maximum length of one relative UTF-8 path in bytes.
    pub max_path_bytes: usize,
    /// Maximum encoded manifest length.
    pub max_manifest_bytes: u64,
}

impl Default for NativeBackupLimits {
    fn default() -> Self {
        Self {
            max_files: 16_384,
            max_directories: 16_384,
            max_total_bytes: 256 * 1024 * 1024 * 1024,
            max_path_bytes: 4_096,
            max_manifest_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Failure while creating, verifying, or restoring a native backup.
#[derive(Debug, Error)]
pub enum NativeBackupError {
    /// The requested destination already exists and is never replaced.
    #[error("native backup or restore destination already exists: {0}")]
    DestinationExists(PathBuf),
    /// A destination is nested in its source and cannot be copied safely.
    #[error("native backup or restore destination is inside its source: {0}")]
    DestinationInsideSource(PathBuf),
    /// Backup layout, path, entry type, or manifest semantics are invalid.
    #[error("invalid native backup at {path}: {reason}")]
    Invalid {
        /// Path at which validation failed.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },
    /// A configured resource bound was exceeded.
    #[error("native backup limit exceeded at {path}: {reason}")]
    LimitExceeded {
        /// Path being processed.
        path: PathBuf,
        /// Stable bound description.
        reason: &'static str,
    },
    /// Filesystem I/O failed.
    #[error("native backup I/O failed for {path}: {source}")]
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// Manifest JSON is malformed.
    #[error("native backup manifest is malformed at {path}: {source}")]
    ManifestJson {
        /// Manifest path.
        path: PathBuf,
        /// JSON codec failure.
        #[source]
        source: serde_json::Error,
    },
    /// Checkpoint or logical database validation failed.
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
    /// Opening restored staging for logical recovery failed.
    #[error("restored native staging failed logical validation at {path}: {source}")]
    LogicalValidation {
        /// Staged data-directory path.
        path: PathBuf,
        /// Native open/recovery failure.
        #[source]
        source: Box<NativeRuntimeError>,
    },
}

/// Metadata for a created or offline-verified native backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBackupInfo {
    /// Backup directory.
    pub path: PathBuf,
    /// Checkpoint visible CSN.
    pub visible_csn: u64,
    /// Checkpoint manifest digest.
    pub checkpoint_digest: [u8; 32],
    /// Number of copied data-directory files.
    pub file_count: usize,
    /// Sum of copied file lengths.
    pub total_bytes: u64,
}

/// Evidence for an atomically promoted native restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRestoreInfo {
    /// New native data-directory path.
    pub data_path: PathBuf,
    /// Verified source backup metadata.
    pub backup: NativeBackupInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    kind: String,
    version: u16,
    visible_csn: u64,
    checkpoint_digest: String,
    file_count: usize,
    directory_count: usize,
    total_bytes: u64,
    directories: Vec<String>,
    files: Vec<FileEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileEntry {
    path: String,
    size: u64,
    blake3: String,
}

impl NativeDatabase {
    /// Creates an exact, bounded native data-directory backup at a synchronized checkpoint.
    ///
    /// The database's exclusive directory lock remains held for the complete checkpoint and
    /// copy. The destination is promoted only after its offline verification succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty database, an existing/unsafe destination, unsupported
    /// filesystem entries, changed source files, resource limits, or verification failure.
    pub fn backup(
        &mut self,
        destination: impl AsRef<Path>,
        limits: NativeBackupLimits,
    ) -> Result<NativeBackupInfo, NativeBackupError> {
        let _permit = self.admit_administrative_owned()?;
        let destination = destination.as_ref();
        let parent = prepare_destination(destination)?;
        reject_nested_destination(&self.data_directory, &parent, destination)?;
        let checkpoint = self.checkpoint_at(None)?;
        let staging = staging_path(destination, "backup")?;
        fs::create_dir(&staging).map_err(|source| io_error(&staging, source))?;
        let result = create_staging(self.data_directory(), &staging, checkpoint, limits)
            .and_then(|()| verify_native_backup_bounded(&staging, limits))
            .and_then(|mut info| {
                if destination.exists() {
                    return Err(NativeBackupError::DestinationExists(
                        destination.to_path_buf(),
                    ));
                }
                fs::rename(&staging, destination)
                    .map_err(|source| io_error(destination, source))?;
                sync_directory(&parent)?;
                info.path = destination.to_path_buf();
                Ok(info)
            });
        if result.is_err() {
            let _ignored = fs::remove_dir_all(&staging);
        }
        result
    }
}

/// Verifies a native backup offline using default resource limits.
///
/// # Errors
///
/// Rejects malformed manifests, missing or additional files, noncanonical paths, symlinks,
/// special files, size mismatches, corruption, and exceeded limits.
pub fn verify_native_backup(path: impl AsRef<Path>) -> Result<NativeBackupInfo, NativeBackupError> {
    verify_native_backup_bounded(path.as_ref(), NativeBackupLimits::default())
}

/// Verifies a native backup offline using caller-selected resource limits.
///
/// # Errors
///
/// Returns the same validation and limit failures as [`verify_native_backup`].
pub fn verify_native_backup_with_limits(
    path: impl AsRef<Path>,
    limits: NativeBackupLimits,
) -> Result<NativeBackupInfo, NativeBackupError> {
    verify_native_backup_bounded(path.as_ref(), limits)
}

/// Verifies a native backup while holding administrative governor capacity.
///
/// A zero wait preserves fail-fast admission. A positive wait uses the
/// governor's bounded priority queue. Admission completes before any backup
/// file is opened.
///
/// # Errors
///
/// Returns a resource-admission/queue failure wrapped by
/// [`NativeBackupError::Runtime`], or the same failures as
/// [`verify_native_backup_with_limits`].
pub fn verify_native_backup_with_resource_governor(
    path: impl AsRef<Path>,
    limits: NativeBackupLimits,
    governor: &Arc<NativeResourceGovernor>,
    maximum_wait: Duration,
) -> Result<NativeBackupInfo, NativeBackupError> {
    let _permit = admit_backup_work(governor, maximum_wait)?;
    verify_native_backup_bounded(path.as_ref(), limits)
}

/// Restores a native backup to a new path using default resource limits.
///
/// The copied directory is opened and logically recovered while still in sibling staging. The
/// destination is renamed into place only after its checkpoint identity is validated.
///
/// # Errors
///
/// Returns an error for invalid backup input, an existing/unsafe destination, copy or open
/// failure, logical checkpoint mismatch, or atomic promotion failure.
pub fn restore_native_backup(
    backup: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<NativeRestoreInfo, NativeBackupError> {
    restore_native_backup_with_limits(
        backup.as_ref(),
        destination.as_ref(),
        NativeBackupLimits::default(),
    )
}

/// Restores a native backup to a new path using caller-selected resource limits.
///
/// # Errors
///
/// Returns the same validation, copy, logical-open, and promotion failures as
/// [`restore_native_backup`].
pub fn restore_native_backup_with_limits(
    backup: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    limits: NativeBackupLimits,
) -> Result<NativeRestoreInfo, NativeBackupError> {
    let backup = backup.as_ref();
    let destination = destination.as_ref();
    let verified = verify_native_backup_bounded(backup, limits)?;
    let parent = prepare_destination(destination)?;
    reject_nested_destination(backup, &parent, destination)?;
    let manifest = read_manifest(&backup.join(MANIFEST_FILE), limits)?;
    let staging = staging_path(destination, "restore")?;
    fs::create_dir(&staging).map_err(|source| io_error(&staging, source))?;
    let result = copy_manifest_files(
        &backup.join(DATA_DIRECTORY),
        &staging,
        &manifest.directories,
        &manifest.files,
    )
    .and_then(|()| {
        sync_tree_directories(&staging)?;
        let opened = NativeDatabase::open(&staging).map_err(|source| {
            NativeBackupError::LogicalValidation {
                path: staging.clone(),
                source: Box::new(source),
            }
        })?;
        let recovery = opened.recovery_report();
        if recovery.visible_csn.map(hyphae_native_types::Csn::get) != Some(manifest.visible_csn)
            || opened.last_checkpoint_manifest_digest()
                != decode_digest(&manifest.checkpoint_digest)
        {
            return Err(invalid(
                &staging,
                "restored logical checkpoint does not match manifest",
            ));
        }
        drop(opened);
        if destination.exists() {
            return Err(NativeBackupError::DestinationExists(
                destination.to_path_buf(),
            ));
        }
        fs::rename(&staging, destination).map_err(|source| io_error(destination, source))?;
        sync_directory(&parent)?;
        Ok(NativeRestoreInfo {
            data_path: destination.to_path_buf(),
            backup: verified,
        })
    });
    if result.is_err() {
        let _ignored = fs::remove_dir_all(&staging);
    }
    result
}

/// Restores and logically validates a native backup while holding
/// administrative governor capacity for the complete operation.
///
/// # Errors
///
/// Returns a resource-admission/queue failure wrapped by
/// [`NativeBackupError::Runtime`], or the same failures as
/// [`restore_native_backup_with_limits`].
pub fn restore_native_backup_with_resource_governor(
    backup: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    limits: NativeBackupLimits,
    governor: &Arc<NativeResourceGovernor>,
    maximum_wait: Duration,
) -> Result<NativeRestoreInfo, NativeBackupError> {
    let _permit = admit_backup_work(governor, maximum_wait)?;
    restore_native_backup_with_limits(backup, destination, limits)
}

fn admit_backup_work(
    governor: &Arc<NativeResourceGovernor>,
    maximum_wait: Duration,
) -> Result<crate::DatabaseGovernorPermit, NativeRuntimeError> {
    admit_governor_work(
        governor,
        maximum_wait,
        WorkloadClass::Administrative,
        GovernorRequest {
            compute_threads: 1,
            io_slots: 1,
            memory_bytes: ADMINISTRATIVE_MEMORY_BYTES,
        },
        None,
    )
}

fn create_staging(
    source: &Path,
    staging: &Path,
    checkpoint: CheckpointReceipt,
    limits: NativeBackupLimits,
) -> Result<(), NativeBackupError> {
    let data = staging.join(DATA_DIRECTORY);
    fs::create_dir(&data).map_err(|source| io_error(&data, source))?;
    sync_live_files(source)?;
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut total = 0_u64;
    collect_and_copy(
        source,
        source,
        &data,
        limits,
        &mut directories,
        &mut files,
        &mut total,
    )?;
    directories.sort();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = BackupManifest {
        kind: BACKUP_KIND.to_owned(),
        version: BACKUP_VERSION,
        visible_csn: checkpoint.visible_csn.get(),
        checkpoint_digest: encode_hex(&checkpoint.manifest_digest),
        file_count: files.len(),
        directory_count: directories.len(),
        total_bytes: total,
        directories,
        files,
    };
    write_manifest(&staging.join(MANIFEST_FILE), &manifest, limits)?;
    sync_tree_directories(&data)?;
    sync_directory(staging)
}

fn sync_live_files(source: &Path) -> Result<(), NativeBackupError> {
    for entry in fs::read_dir(source).map_err(|error| io_error(source, error))? {
        let entry = entry.map_err(|error| io_error(source, error))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|error| io_error(&path, error))?;
        if kind.is_symlink() {
            return Err(invalid(&path, "symlinks are forbidden"));
        }
        if kind.is_dir() {
            sync_live_files(&path)?;
        } else if kind.is_file() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| io_error(&path, error))?;
        } else {
            return Err(invalid(&path, "special filesystem entries are forbidden"));
        }
    }
    sync_directory(source)
}

fn collect_and_copy(
    root: &Path,
    directory: &Path,
    destination_root: &Path,
    limits: NativeBackupLimits,
    directories: &mut Vec<String>,
    files: &mut Vec<FileEntry>,
    total: &mut u64,
) -> Result<(), NativeBackupError> {
    let metadata = fs::symlink_metadata(directory).map_err(|source| io_error(directory, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            directory,
            "source entries must be real directories or regular files",
        ));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|source| io_error(directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(directory, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if kind.is_symlink() {
            return Err(invalid(&path, "symlinks are forbidden"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid(&path, "path escaped source"))?;
        validate_relative_path(relative, limits)?;
        let destination = destination_root.join(relative);
        if kind.is_dir() {
            if directories.len() >= limits.max_directories {
                return Err(limit(&path, "directory count exceeds configured maximum"));
            }
            fs::create_dir(&destination).map_err(|source| io_error(&destination, source))?;
            directories.push(slash_path(relative)?);
            collect_and_copy(
                root,
                &path,
                destination_root,
                limits,
                directories,
                files,
                total,
            )?;
        } else if kind.is_file() {
            if files.len() >= limits.max_files {
                return Err(limit(&path, "file count exceeds configured maximum"));
            }
            let (size, digest) = copy_and_hash(&path, &destination)?;
            *total = total
                .checked_add(size)
                .ok_or_else(|| limit(&path, "total bytes overflow"))?;
            if *total > limits.max_total_bytes {
                return Err(limit(&path, "total bytes exceed configured maximum"));
            }
            files.push(FileEntry {
                path: slash_path(relative)?,
                size,
                blake3: encode_hex(&digest),
            });
        } else {
            return Err(invalid(&path, "special filesystem entries are forbidden"));
        }
    }
    Ok(())
}

fn verify_native_backup_bounded(
    path: &Path,
    limits: NativeBackupLimits,
) -> Result<NativeBackupInfo, NativeBackupError> {
    validate_backup_root(path)?;
    let manifest = read_manifest(&path.join(MANIFEST_FILE), limits)?;
    validate_manifest(&manifest, path, limits)?;
    let data = path.join(DATA_DIRECTORY);
    let mut actual_directories = Vec::new();
    let mut actual = Vec::new();
    let mut total = 0_u64;
    collect_entries_for_verify(
        &data,
        &data,
        limits,
        &mut actual_directories,
        &mut actual,
        &mut total,
    )?;
    actual_directories.sort();
    actual.sort_by(|left, right| left.path.cmp(&right.path));
    if actual_directories != manifest.directories
        || actual.len() != manifest.file_count
        || total != manifest.total_bytes
        || actual.iter().zip(&manifest.files).any(|(left, right)| {
            left.path != right.path || left.size != right.size || left.blake3 != right.blake3
        })
    {
        return Err(invalid(
            path,
            "file inventory, size, or BLAKE3 digest mismatch",
        ));
    }
    Ok(NativeBackupInfo {
        path: path.to_path_buf(),
        visible_csn: manifest.visible_csn,
        checkpoint_digest: decode_digest(&manifest.checkpoint_digest)
            .ok_or_else(|| invalid(path, "checkpoint digest is not canonical lowercase hex"))?,
        file_count: actual.len(),
        total_bytes: total,
    })
}

fn collect_entries_for_verify(
    root: &Path,
    directory: &Path,
    limits: NativeBackupLimits,
    directories: &mut Vec<String>,
    files: &mut Vec<FileEntry>,
    total: &mut u64,
) -> Result<(), NativeBackupError> {
    let metadata = fs::symlink_metadata(directory).map_err(|source| io_error(directory, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            directory,
            "data tree contains a symlink or non-directory",
        ));
    }
    for entry in fs::read_dir(directory).map_err(|source| io_error(directory, source))? {
        let entry = entry.map_err(|source| io_error(directory, source))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if kind.is_symlink() {
            return Err(invalid(&path, "symlinks are forbidden"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid(&path, "path escaped data tree"))?;
        validate_relative_path(relative, limits)?;
        if kind.is_dir() {
            if directories.len() >= limits.max_directories {
                return Err(limit(&path, "directory count exceeds configured maximum"));
            }
            directories.push(slash_path(relative)?);
            collect_entries_for_verify(root, &path, limits, directories, files, total)?;
        } else if kind.is_file() {
            if files.len() >= limits.max_files {
                return Err(limit(&path, "file count exceeds configured maximum"));
            }
            let (size, digest) = hash_file(&path)?;
            *total = total
                .checked_add(size)
                .ok_or_else(|| limit(&path, "total bytes overflow"))?;
            if *total > limits.max_total_bytes {
                return Err(limit(&path, "total bytes exceed configured maximum"));
            }
            files.push(FileEntry {
                path: slash_path(relative)?,
                size,
                blake3: encode_hex(&digest),
            });
        } else {
            return Err(invalid(&path, "special filesystem entries are forbidden"));
        }
    }
    Ok(())
}

fn validate_backup_root(path: &Path) -> Result<(), NativeBackupError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(path, "backup root must be a real directory"));
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(path).map_err(|source| io_error(path, source))? {
        let entry = entry.map_err(|source| io_error(path, source))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid(path, "non-UTF-8 root entry"))?;
        let kind = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        let valid =
            (name == MANIFEST_FILE && kind.is_file()) || (name == DATA_DIRECTORY && kind.is_dir());
        if !valid || kind.is_symlink() || !names.insert(name) {
            return Err(invalid(
                &entry.path(),
                "backup root contains an unexpected entry",
            ));
        }
    }
    if names.len() != 2 || !names.contains(MANIFEST_FILE) || !names.contains(DATA_DIRECTORY) {
        return Err(invalid(path, "backup root is missing a required entry"));
    }
    Ok(())
}

fn validate_manifest(
    manifest: &BackupManifest,
    path: &Path,
    limits: NativeBackupLimits,
) -> Result<(), NativeBackupError> {
    if manifest.kind != BACKUP_KIND
        || manifest.version != BACKUP_VERSION
        || manifest.visible_csn == 0
    {
        return Err(invalid(
            path,
            "manifest kind, version, or checkpoint is invalid",
        ));
    }
    if decode_digest(&manifest.checkpoint_digest).is_none()
        || manifest.file_count != manifest.files.len()
        || manifest.directory_count != manifest.directories.len()
        || manifest.file_count > limits.max_files
        || manifest.directory_count > limits.max_directories
        || manifest.total_bytes > limits.max_total_bytes
    {
        return Err(invalid(
            path,
            "manifest totals or checkpoint digest are invalid",
        ));
    }
    let mut previous: Option<&str> = None;
    for directory in &manifest.directories {
        let relative = Path::new(directory);
        validate_relative_path(relative, limits)?;
        if slash_path(relative)? != *directory
            || previous.is_some_and(|value| value >= directory.as_str())
        {
            return Err(invalid(path, "manifest directory path is noncanonical"));
        }
        previous = Some(directory);
    }
    previous = None;
    let mut sum = 0_u64;
    for file in &manifest.files {
        let relative = Path::new(&file.path);
        validate_relative_path(relative, limits)?;
        if slash_path(relative)? != file.path
            || previous.is_some_and(|value| value >= file.path.as_str())
            || decode_digest(&file.blake3).is_none()
        {
            return Err(invalid(
                path,
                "manifest file path or digest is noncanonical",
            ));
        }
        sum = sum
            .checked_add(file.size)
            .ok_or_else(|| invalid(path, "manifest size overflow"))?;
        previous = Some(&file.path);
    }
    if sum != manifest.total_bytes {
        return Err(invalid(
            path,
            "manifest byte total does not match file entries",
        ));
    }
    Ok(())
}

fn copy_manifest_files(
    source: &Path,
    destination: &Path,
    directories: &[String],
    files: &[FileEntry],
) -> Result<(), NativeBackupError> {
    for directory in directories {
        let target = destination.join(directory);
        fs::create_dir(&target).map_err(|source| io_error(&target, source))?;
    }
    for file in files {
        let relative = Path::new(&file.path);
        let target = destination.join(relative);
        let parent = target
            .parent()
            .ok_or_else(|| invalid(&target, "file has no parent"))?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        let (size, digest) = copy_and_hash(&source.join(relative), &target)?;
        if size != file.size || encode_hex(&digest) != file.blake3 {
            return Err(invalid(
                &source.join(relative),
                "source changed while restore copied it",
            ));
        }
    }
    Ok(())
}

fn copy_and_hash(source: &Path, destination: &Path) -> Result<(u64, [u8; 32]), NativeBackupError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_error(source, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid(
            source,
            "copied source must be a regular non-symlink file",
        ));
    }
    let mut input = File::open(source).map_err(|error| io_error(source, error))?;
    let expected = input
        .metadata()
        .map_err(|error| io_error(source, error))?
        .len();
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| io_error(destination, error))?;
    let mut hasher = Hasher::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while copied < expected {
        let remaining = usize::try_from((expected - copied).min(buffer.len() as u64))
            .map_err(|_| limit(source, "file copy length cannot be represented"))?;
        let read = input
            .read(&mut buffer[..remaining])
            .map_err(|error| io_error(source, error))?;
        if read == 0 {
            return Err(invalid(source, "source file shortened while copied"));
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| io_error(destination, error))?;
        hasher.update(&buffer[..read]);
        copied += u64::try_from(read).map_err(|_| limit(source, "file copy length overflow"))?;
    }
    if input
        .metadata()
        .map_err(|error| io_error(source, error))?
        .len()
        != expected
    {
        return Err(invalid(source, "source file changed length while copied"));
    }
    output
        .sync_all()
        .map_err(|error| io_error(destination, error))?;
    Ok((copied, *hasher.finalize().as_bytes()))
}

fn hash_file(path: &Path) -> Result<(u64, [u8; 32]), NativeBackupError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid(
            path,
            "hashed entry must be a regular non-symlink file",
        ));
    }
    let expected = metadata.len();
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut hasher = Hasher::new();
    let mut read_total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        read_total = read_total
            .checked_add(read as u64)
            .ok_or_else(|| limit(path, "file size overflow"))?;
        hasher.update(&buffer[..read]);
    }
    if read_total != expected
        || file
            .metadata()
            .map_err(|source| io_error(path, source))?
            .len()
            != expected
    {
        return Err(invalid(path, "file changed while hashed"));
    }
    Ok((read_total, *hasher.finalize().as_bytes()))
}

fn write_manifest(
    path: &Path,
    manifest: &BackupManifest,
    limits: NativeBackupLimits,
) -> Result<(), NativeBackupError> {
    let mut encoded =
        serde_json::to_vec_pretty(manifest).map_err(|source| NativeBackupError::ManifestJson {
            path: path.to_path_buf(),
            source,
        })?;
    encoded.push(b'\n');
    if encoded.len() as u64 > limits.max_manifest_bytes {
        return Err(limit(path, "manifest exceeds configured maximum"));
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(path, source))
}

fn read_manifest(
    path: &Path,
    limits: NativeBackupLimits,
) -> Result<BackupManifest, NativeBackupError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid(path, "manifest must be a regular non-symlink file"));
    }
    if metadata.len() > limits.max_manifest_bytes {
        return Err(limit(path, "manifest exceeds configured maximum"));
    }
    let mut encoded = Vec::new();
    File::open(path)
        .map_err(|source| io_error(path, source))?
        .take(limits.max_manifest_bytes.saturating_add(1))
        .read_to_end(&mut encoded)
        .map_err(|source| io_error(path, source))?;
    if encoded.len() as u64 > limits.max_manifest_bytes {
        return Err(limit(path, "manifest exceeds configured maximum"));
    }
    serde_json::from_slice(&encoded).map_err(|source| NativeBackupError::ManifestJson {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_relative_path(
    path: &Path,
    limits: NativeBackupLimits,
) -> Result<(), NativeBackupError> {
    if path.as_os_str().is_empty()
        || path.as_os_str().as_encoded_bytes().len() > limits.max_path_bytes
    {
        return Err(limit(
            path,
            "relative path is empty or exceeds configured maximum",
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid(
            path,
            "path traversal or absolute paths are forbidden",
        ));
    }
    if path.to_str().is_none() {
        return Err(invalid(path, "paths must be valid UTF-8"));
    }
    Ok(())
}

fn slash_path(path: &Path) -> Result<String, NativeBackupError> {
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .ok_or_else(|| invalid(path, "paths must be valid UTF-8")),
            _ => Err(invalid(
                path,
                "path traversal or absolute paths are forbidden",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("/"))
}

fn prepare_destination(destination: &Path) -> Result<PathBuf, NativeBackupError> {
    if destination.exists() {
        return Err(NativeBackupError::DestinationExists(
            destination.to_path_buf(),
        ));
    }
    if destination.file_name().is_none() {
        return Err(invalid(
            destination,
            "destination has no final path component",
        ));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    Ok(parent.to_path_buf())
}

fn reject_nested_destination(
    source: &Path,
    destination_parent: &Path,
    destination: &Path,
) -> Result<(), NativeBackupError> {
    let source = fs::canonicalize(source).map_err(|error| io_error(source, error))?;
    let parent = fs::canonicalize(destination_parent)
        .map_err(|error| io_error(destination_parent, error))?;
    if parent.starts_with(source) {
        return Err(NativeBackupError::DestinationInsideSource(
            destination.to_path_buf(),
        ));
    }
    Ok(())
}

fn staging_path(destination: &Path, operation: &str) -> Result<PathBuf, NativeBackupError> {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid(destination, "destination filename must be valid UTF-8"))?;
    let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
    Ok(destination.with_file_name(format!(
        ".{name}.hyphae-native-{operation}-{}-{id}.tmp",
        std::process::id()
    )))
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, output) in digest.iter_mut().enumerate() {
        let high = hex_nibble(value.as_bytes()[index * 2])?;
        let low = hex_nibble(value.as_bytes()[index * 2 + 1])?;
        *output = (high << 4) | low;
    }
    Some(digest)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn sync_tree_directories(path: &Path) -> Result<(), NativeBackupError> {
    for entry in fs::read_dir(path).map_err(|source| io_error(path, source))? {
        let entry = entry.map_err(|source| io_error(path, source))?;
        if entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?
            .is_dir()
        {
            sync_tree_directories(&entry.path())?;
        }
    }
    sync_directory(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), NativeBackupError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), NativeBackupError> {
    Ok(())
}

fn invalid(path: &Path, reason: &'static str) -> NativeBackupError {
    NativeBackupError::Invalid {
        path: path.to_path_buf(),
        reason,
    }
}

fn limit(path: &Path, reason: &'static str) -> NativeBackupError {
    NativeBackupError::LimitExceeded {
        path: path.to_path_buf(),
        reason,
    }
}

fn io_error(path: &Path, source: io::Error) -> NativeBackupError {
    NativeBackupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        io::{Seek, SeekFrom, Write},
        path::PathBuf,
        sync::Arc,
        time::Duration,
    };

    use hyphae_native_types::DurabilityClass;

    use super::{
        NativeBackupError, NativeBackupLimits, restore_native_backup,
        restore_native_backup_with_resource_governor, verify_native_backup,
        verify_native_backup_with_resource_governor,
    };
    use crate::{
        GovernorAdmissionError, GovernorRequest, NativeDatabase, NativeResourceGovernor,
        NativeRuntimeError, WorkloadClass,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Result<Self, Box<dyn Error>> {
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-backup-{name}-{}-{}",
                std::process::id(),
                super::NEXT_STAGING_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn native_backup_round_trip_opens_and_preserves_logical_state() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("roundtrip")?;
        let source = temporary.0.join("source");
        let backup = temporary.0.join("backup");
        let restored = temporary.0.join("restored");
        let mut database = NativeDatabase::create(&source)?;
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.set(b"alpha".to_vec(), b"value".to_vec(), None)?;
        transaction.commit()?;
        let created = database.backup(&backup, NativeBackupLimits::default())?;
        assert_eq!(created, verify_native_backup(&backup)?);
        drop(database);

        let restored_info = restore_native_backup(&backup, &restored)?;
        assert_eq!(restored_info.backup.visible_csn, created.visible_csn);
        let reopened = NativeDatabase::open(&restored)?;
        let snapshot = reopened.snapshot(0)?;
        assert_eq!(snapshot.get(b"alpha"), Some(b"value".as_slice()));
        Ok(())
    }

    #[test]
    fn structure_v3_backup_restore_preserves_format_state_and_scalar_writes()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("structure-v3")?;
        let source = temporary.0.join("source");
        let backup = temporary.0.join("backup");
        let restored = temporary.0.join("restored");
        let mut database = NativeDatabase::create(&source)?;
        let mut transaction = database.begin(10, DurabilityClass::Strict)?;
        transaction.set(b"alpha".to_vec(), b"value".to_vec(), Some(100))?;
        transaction.create_set(b"members".to_vec())?;
        transaction.sadd(b"members".to_vec(), b"one".to_vec())?;
        transaction.commit()?;
        database.migrate_structure_to_v3(DurabilityClass::Strict)?;

        let created = database.backup(&backup, NativeBackupLimits::default())?;
        assert_eq!(created, verify_native_backup(&backup)?);
        drop(database);
        restore_native_backup(&backup, &restored)?;

        let mut reopened = NativeDatabase::open(&restored)?;
        assert_eq!(reopened.structure_format, crate::StructureFormat::BTreeV3);
        let snapshot = reopened.snapshot(11)?;
        assert_eq!(snapshot.get(b"alpha"), Some(b"value".as_slice()));
        assert!(snapshot.sismember(b"members", b"one")?);
        let mut update = reopened.begin(11, DurabilityClass::Strict)?;
        update.set(b"after-restore".to_vec(), b"value".to_vec(), Some(100))?;
        update.commit()?;
        let snapshot = reopened.snapshot(11)?;
        assert_eq!(snapshot.get(b"after-restore"), Some(b"value".as_slice()));
        assert_eq!(
            snapshot.ttl(b"after-restore"),
            crate::Ttl::RemainingMicros(89)
        );
        Ok(())
    }

    #[test]
    fn backup_holds_administrative_capacity_for_the_complete_copy() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("governed")?;
        let source = temporary.0.join("source");
        let backup = temporary.0.join("backup");
        let mut database = NativeDatabase::create(&source)?;
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.set(b"alpha".to_vec(), b"value".to_vec(), None)?;
        transaction.commit()?;
        let governor = Arc::new(NativeResourceGovernor::new(
            crate::tests::engine_admission_test_policy(),
        ));
        database.set_resource_governor(Arc::clone(&governor))?;
        let held = governor.try_admit(
            WorkloadClass::Administrative,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 1,
                memory_bytes: crate::ADMINISTRATIVE_MEMORY_BYTES,
            },
        )?;
        assert!(matches!(
            database.backup(&backup, NativeBackupLimits::default()),
            Err(NativeBackupError::Runtime(
                NativeRuntimeError::ResourceAdmission(
                    GovernorAdmissionError::GlobalCapacity | GovernorAdmissionError::ClassCapacity
                )
            ))
        ));
        assert!(!backup.exists());
        drop(held);
        database.backup(&backup, NativeBackupLimits::default())?;
        assert!(backup.exists());
        assert_eq!(governor.usage_snapshot().compute_threads, 0);

        let held = governor.try_admit(
            WorkloadClass::Administrative,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 1,
                memory_bytes: crate::ADMINISTRATIVE_MEMORY_BYTES,
            },
        )?;
        assert!(matches!(
            verify_native_backup_with_resource_governor(
                &backup,
                NativeBackupLimits::default(),
                &governor,
                Duration::ZERO,
            ),
            Err(NativeBackupError::Runtime(
                NativeRuntimeError::ResourceAdmission(
                    GovernorAdmissionError::GlobalCapacity | GovernorAdmissionError::ClassCapacity
                )
            ))
        ));
        let restored = temporary.0.join("restored");
        assert!(matches!(
            restore_native_backup_with_resource_governor(
                &backup,
                &restored,
                NativeBackupLimits::default(),
                &governor,
                Duration::ZERO,
            ),
            Err(NativeBackupError::Runtime(
                NativeRuntimeError::ResourceAdmission(
                    GovernorAdmissionError::GlobalCapacity | GovernorAdmissionError::ClassCapacity
                )
            ))
        ));
        assert!(!restored.exists());
        drop(held);
        Ok(())
    }

    #[test]
    fn corruption_missing_files_and_existing_destination_never_activate()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("failures")?;
        let source = temporary.0.join("source");
        let backup = temporary.0.join("backup");
        let missing_backup = temporary.0.join("missing-backup");
        let destination = temporary.0.join("destination");
        let mut database = NativeDatabase::create(&source)?;
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.set(b"alpha".to_vec(), b"value".to_vec(), None)?;
        transaction.commit()?;
        database.backup(&backup, NativeBackupLimits::default())?;
        database.backup(&missing_backup, NativeBackupLimits::default())?;
        drop(database);

        fs::create_dir(&destination)?;
        assert!(matches!(
            restore_native_backup(&backup, &destination),
            Err(NativeBackupError::DestinationExists(_))
        ));
        fs::remove_dir(&destination)?;
        fs::remove_file(missing_backup.join("data/FORMAT"))?;
        assert!(verify_native_backup(&missing_backup).is_err());
        assert!(restore_native_backup(&missing_backup, &destination).is_err());
        assert!(!destination.exists());

        let wal = backup.join("data/wal.hywal");
        let mut file = fs::OpenOptions::new().write(true).open(&wal)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&[0xff])?;
        file.sync_all()?;
        assert!(verify_native_backup(&backup).is_err());
        assert!(restore_native_backup(&backup, &destination).is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlink_and_manifest_traversal() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let temporary = TestDirectory::new("unsafe-input")?;
        let source = temporary.0.join("source");
        let backup = temporary.0.join("backup");
        let mut database = NativeDatabase::create(&source)?;
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.set(b"alpha".to_vec(), b"value".to_vec(), None)?;
        transaction.commit()?;
        database.backup(&backup, NativeBackupLimits::default())?;
        drop(database);

        symlink(backup.join("data/FORMAT"), backup.join("data/link"))?;
        assert!(verify_native_backup(&backup).is_err());
        fs::remove_file(backup.join("data/link"))?;
        let manifest = backup.join("NATIVE_BACKUP.json");
        let encoded = fs::read_to_string(&manifest)?.replacen("\"FORMAT\"", "\"../FORMAT\"", 1);
        fs::write(&manifest, encoded)?;
        assert!(verify_native_backup(&backup).is_err());
        Ok(())
    }
}
