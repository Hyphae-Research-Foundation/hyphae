// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use fs4::{FileExt, TryLockError};
use hyphae_core::{DISK_FORMAT_VERSION, MIN_DISK_FORMAT_VERSION};
use thiserror::Error;

use crate::{
    DurableLog, LogError, ManifestError, OpenedLog, RecoveryLimits, StorageLimitError,
    limits::{OperationDeadline, limit_io_error},
    manifest::StorageManifest,
};

const FORMAT_PREFIX: &str = "hyphae-disk-format=";
const MAX_FORMAT_MARKER_BYTES: usize = 64;
const FORMAT_MARKER_READ_LIMIT: u64 = 65;
const REQUIRED_DIRECTORIES: [&str; 6] = ["manifest", "log", "snapshots", "indexes", "blobs", "tmp"];

/// Failure while opening or initializing a Hyphae data directory.
#[derive(Debug, Error)]
pub enum DataDirectoryError {
    /// Another writer already owns the directory lock.
    #[error("data directory is already locked by another writer: {0}")]
    AlreadyLocked(PathBuf),

    /// The `FORMAT` marker does not match the canonical representation.
    #[error("malformed data format marker: {0}")]
    MalformedFormat(PathBuf),

    /// The directory was created by a newer, unsupported disk format.
    #[error("unsupported disk format {found}; this binary supports {supported}")]
    UnsupportedFormat {
        /// Version found in `FORMAT`.
        found: u16,
        /// Highest version understood by this binary.
        supported: u16,
    },

    /// The immutable storage manifest could not be loaded or initialized.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// A filesystem operation failed.
    #[error("failed to {action} {path}: {source}")]
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
}

impl From<StorageLimitError> for DataDirectoryError {
    fn from(source: StorageLimitError) -> Self {
        Self::Manifest(ManifestError::Io(limit_io_error(source)))
    }
}

/// An exclusively owned Hyphae data directory.
///
/// The operating-system lock is held until this value is dropped. Opening the
/// same directory for a second writer fails instead of relying on cooperative
/// process behavior.
#[derive(Debug)]
pub struct DataDirectory {
    root: PathBuf,
    lock: File,
    manifest: StorageManifest,
    disk_format_version: u16,
    #[cfg(test)]
    fail_next_manifest_commit_after_write: bool,
}

impl DataDirectory {
    /// Opens an existing data directory or initializes a new one.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be initialized, is owned by
    /// another writer, has a malformed marker, or uses a future format.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DataDirectoryError> {
        let limits = RecoveryLimits::compatibility();
        let deadline = OperationDeadline::new(limits.timeout);
        Self::open_with_limits_and_deadline(path, &limits, &deadline)
    }

    /// Opens or initializes a data directory under finite recovery limits.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, contention, malformed metadata,
    /// future formats, exhausted directory policy, timeout, or I/O failure.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: &RecoveryLimits,
    ) -> Result<Self, DataDirectoryError> {
        limits.validate()?;
        let deadline = OperationDeadline::new(limits.timeout);
        Self::open_with_limits_and_deadline(path, limits, &deadline)
    }

    pub(crate) fn open_with_limits_and_deadline(
        path: impl AsRef<Path>,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<Self, DataDirectoryError> {
        deadline.check()?;
        let root = path.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|source| DataDirectoryError::Io {
            action: "create data directory",
            path: root.clone(),
            source,
        })?;

        let lock_path = root.join("LOCK");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| DataDirectoryError::Io {
                action: "open lock file",
                path: lock_path.clone(),
                source,
            })?;

        match FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DataDirectoryError::AlreadyLocked(root));
            }
            Err(TryLockError::Error(source)) => {
                return Err(DataDirectoryError::Io {
                    action: "lock data directory",
                    path: lock_path,
                    source,
                });
            }
        }

        let opened_format = initialize_or_validate_format(&root)?;
        for name in REQUIRED_DIRECTORIES {
            let directory = root.join(name);
            fs::create_dir_all(&directory).map_err(|source| DataDirectoryError::Io {
                action: "create data subdirectory",
                path: directory,
                source,
            })?;
        }

        let manifest = StorageManifest::load_or_initialize_with_limits(
            &root,
            limits.max_directory_entries,
            deadline,
        )?;

        Ok(Self {
            root,
            lock,
            manifest,
            disk_format_version: opened_format,
            #[cfg(test)]
            fail_next_manifest_commit_after_write: false,
        })
    }

    /// Returns the canonical root path.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Returns the format currently committed by the directory marker.
    pub fn disk_format_version(&self) -> u16 {
        self.disk_format_version
    }

    /// Returns the path of the active append-only log segment.
    pub fn active_log_path(&self) -> PathBuf {
        self.log_path(self.manifest.active_segment)
    }

    /// Opens the initial durable log while borrowing this directory lock.
    ///
    /// The returned writer cannot outlive the exclusive data-directory owner.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failures or any complete invalid frame.
    pub fn open_log(&self) -> Result<OpenedLog<'_>, LogError> {
        let limits = RecoveryLimits::compatibility();
        let deadline = OperationDeadline::new(limits.timeout);
        self.open_log_with_limits(&limits, &deadline)
    }

    pub(crate) fn open_log_with_limits(
        &self,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<OpenedLog<'_>, LogError> {
        let (log, recovery) = DurableLog::open_file_at_version_with_limits(
            self.active_log_path(),
            self.manifest.base_sequence,
            self.manifest.base_digest,
            self.disk_format_version,
            limits,
            deadline,
        )?;
        Ok(OpenedLog::new(log, recovery))
    }

    pub(crate) fn log_anchor(&self) -> (u64, [u8; 32]) {
        (self.manifest.base_sequence, self.manifest.base_digest)
    }

    pub(crate) fn manifest(&self) -> StorageManifest {
        self.manifest
    }

    pub(crate) fn log_path(&self, segment: u64) -> PathBuf {
        self.root.join("log").join(format!("{segment:020}.hylog"))
    }

    pub(crate) fn snapshot_path(&self, sequence: u64) -> PathBuf {
        self.root
            .join("snapshots")
            .join(format!("snapshot-{sequence:020}.hysnap"))
    }

    pub(crate) fn commit_manifest(
        &mut self,
        manifest: StorageManifest,
    ) -> Result<(), DataDirectoryError> {
        let limits = RecoveryLimits::default();
        let deadline = OperationDeadline::new(limits.timeout);
        self.commit_manifest_with_limits(manifest, &limits, &deadline)
    }

    pub(crate) fn commit_manifest_with_limits(
        &mut self,
        manifest: StorageManifest,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<(), DataDirectoryError> {
        let target = manifest.path(&self.root);
        let target_is_new = self.reserve_target_entry("manifest", &target, limits, deadline)?;
        if target_is_new {
            self.reserve_directory_entries("tmp", 1, limits, deadline)?;
        }
        manifest.write_new(&self.root)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_manifest_commit_after_write) {
            return Err(DataDirectoryError::Io {
                action: "complete injected manifest commit",
                path: target,
                source: io::Error::other("injected post-write manifest failure"),
            });
        }
        self.manifest = manifest;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_manifest_commit_failure_after_write(&mut self) {
        self.fail_next_manifest_commit_after_write = true;
    }

    pub(crate) fn reserve_target_entry(
        &self,
        directory_name: &'static str,
        target: &Path,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<bool, DataDirectoryError> {
        deadline.check()?;
        let target_is_new = !target
            .try_exists()
            .map_err(|source| DataDirectoryError::Io {
                action: "inspect prospective storage entry",
                path: target.to_path_buf(),
                source,
            })?;
        self.reserve_directory_entries(directory_name, u64::from(target_is_new), limits, deadline)?;
        Ok(target_is_new)
    }

    pub(crate) fn reserve_directory_entries(
        &self,
        directory_name: &'static str,
        additional_entries: u64,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<(), DataDirectoryError> {
        deadline.check()?;
        let directory = self.root.join(directory_name);
        let entries = fs::read_dir(&directory).map_err(|source| DataDirectoryError::Io {
            action: "inspect storage directory capacity",
            path: directory.clone(),
            source,
        })?;
        let mut entry_count = 0_u64;
        for entry in entries {
            deadline.check()?;
            entry.map_err(|source| DataDirectoryError::Io {
                action: "inspect storage directory entry",
                path: directory.clone(),
                source,
            })?;
            entry_count =
                entry_count
                    .checked_add(1)
                    .ok_or(StorageLimitError::DirectoryEntriesExceeded {
                        maximum: limits.max_directory_entries,
                    })?;
            if entry_count > limits.max_directory_entries {
                return Err(StorageLimitError::DirectoryEntriesExceeded {
                    maximum: limits.max_directory_entries,
                }
                .into());
            }
        }
        let required = entry_count.checked_add(additional_entries).ok_or(
            StorageLimitError::DirectoryEntriesExceeded {
                maximum: limits.max_directory_entries,
            },
        )?;
        if required > limits.max_directory_entries {
            return Err(StorageLimitError::DirectoryEntriesExceeded {
                maximum: limits.max_directory_entries,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn promote_format(&mut self) -> Result<(), DataDirectoryError> {
        if self.disk_format_version == DISK_FORMAT_VERSION {
            return Ok(());
        }
        write_format_marker(&self.root, DISK_FORMAT_VERSION)?;
        self.disk_format_version = DISK_FORMAT_VERSION;
        Ok(())
    }

    pub(crate) fn cleanup_retired_logs_with_limits(
        &self,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> bool {
        let log_directory = self.root.join("log");
        let Ok(entries) = fs::read_dir(&log_directory) else {
            return false;
        };
        #[cfg(unix)]
        let mut removed = false;
        let mut entry_count = 0_u64;
        for entry in entries.flatten() {
            if deadline.check().is_err() {
                return false;
            }
            entry_count = match entry_count.checked_add(1) {
                Some(count) if count <= limits.max_directory_entries => count,
                _ => return false,
            };
            let path = entry.path();
            let Some(segment) = segment_from_path(&path) else {
                continue;
            };
            if segment < self.manifest.active_segment {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        #[cfg(unix)]
                        {
                            removed = true;
                        }
                    }
                    Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => return false,
                }
            }
        }
        #[cfg(unix)]
        if removed && sync_directory(&log_directory).is_err() {
            return false;
        }
        true
    }
}

impl Drop for DataDirectory {
    fn drop(&mut self) {
        let _ignored = FileExt::unlock(&self.lock);
    }
}

fn initialize_or_validate_format(root: &Path) -> Result<u16, DataDirectoryError> {
    let format_path = root.join("FORMAT");
    if format_path.exists() {
        return validate_format(&format_path);
    }
    write_format_marker(root, DISK_FORMAT_VERSION)?;
    Ok(DISK_FORMAT_VERSION)
}

fn write_format_marker(root: &Path, version: u16) -> Result<(), DataDirectoryError> {
    let format_path = root.join("FORMAT");
    let temporary_path = root.join("FORMAT.new");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary_path)
        .map_err(|source| DataDirectoryError::Io {
            action: "create temporary format marker",
            path: temporary_path.clone(),
            source,
        })?;
    let marker = format!("{FORMAT_PREFIX}{version}\n");
    file.write_all(marker.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| DataDirectoryError::Io {
            action: "initialize temporary format marker",
            path: temporary_path.clone(),
            source,
        })?;
    drop(file);
    fs::rename(&temporary_path, &format_path).map_err(|source| DataDirectoryError::Io {
        action: "promote format marker",
        path: format_path,
        source,
    })?;
    #[cfg(unix)]
    sync_directory(root)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), DataDirectoryError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| DataDirectoryError::Io {
            action: "synchronize data directory",
            path: path.to_path_buf(),
            source,
        })
}

fn validate_format(path: &Path) -> Result<u16, DataDirectoryError> {
    let file = File::open(path).map_err(|source| DataDirectoryError::Io {
        action: "open format marker",
        path: path.to_path_buf(),
        source,
    })?;
    let mut marker_bytes = Vec::with_capacity(MAX_FORMAT_MARKER_BYTES + 1);
    file.take(FORMAT_MARKER_READ_LIMIT)
        .read_to_end(&mut marker_bytes)
        .map_err(|source| DataDirectoryError::Io {
            action: "read format marker",
            path: path.to_path_buf(),
            source,
        })?;
    if marker_bytes.len() > MAX_FORMAT_MARKER_BYTES {
        return Err(DataDirectoryError::MalformedFormat(path.to_path_buf()));
    }
    let marker = String::from_utf8(marker_bytes)
        .map_err(|_| DataDirectoryError::MalformedFormat(path.to_path_buf()))?;

    let Some(raw_version) = marker
        .strip_prefix(FORMAT_PREFIX)
        .and_then(|value| value.strip_suffix('\n'))
    else {
        return Err(DataDirectoryError::MalformedFormat(path.to_path_buf()));
    };
    let version = raw_version
        .parse::<u16>()
        .map_err(|_| DataDirectoryError::MalformedFormat(path.to_path_buf()))?;
    if version > DISK_FORMAT_VERSION {
        return Err(DataDirectoryError::UnsupportedFormat {
            found: version,
            supported: DISK_FORMAT_VERSION,
        });
    }
    if version < MIN_DISK_FORMAT_VERSION {
        return Err(DataDirectoryError::MalformedFormat(path.to_path_buf()));
    }
    Ok(version)
}

fn segment_from_path(path: &Path) -> Option<u64> {
    let filename = path.file_name()?.to_str()?;
    let raw_segment = filename.strip_suffix(".hylog")?;
    if raw_segment.len() != 20 || !raw_segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let segment = raw_segment.parse().ok()?;
    (format!("{segment:020}.hylog") == filename).then_some(segment)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs};

    use uuid::Uuid;

    use super::{DataDirectory, DataDirectoryError, REQUIRED_DIRECTORIES};
    use crate::{DurableLog, test_support::TestDirectory};

    #[test]
    fn initializes_canonical_layout() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("data-layout")?;
        let root = temporary.path().join("data");
        let directory = DataDirectory::open(&root)?;

        assert_eq!(directory.path(), root);
        assert_eq!(
            fs::read_to_string(root.join("FORMAT"))?,
            "hyphae-disk-format=2\n"
        );
        assert!(root.join("LOCK").is_file());
        for name in REQUIRED_DIRECTORIES {
            assert!(root.join(name).is_dir());
        }
        Ok(())
    }

    #[test]
    fn rejects_a_second_writer() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("data-lock")?;
        let first = DataDirectory::open(temporary.path())?;
        let second = DataDirectory::open(temporary.path());

        assert!(matches!(second, Err(DataDirectoryError::AlreadyLocked(_))));
        drop(first);
        DataDirectory::open(temporary.path())?;
        Ok(())
    }

    #[test]
    fn rejects_future_format() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("future-format")?;
        fs::write(temporary.path().join("FORMAT"), "hyphae-disk-format=3\n")?;

        let result = DataDirectory::open(temporary.path());
        assert!(matches!(
            result,
            Err(DataDirectoryError::UnsupportedFormat {
                found: 3,
                supported: 2
            })
        ));
        Ok(())
    }

    #[test]
    fn format_marker_reads_are_bounded_at_sixty_four_bytes() -> Result<(), Box<dyn Error>> {
        for length in [64_usize, 65] {
            let temporary = TestDirectory::new(&format!("bounded-format-{length}"))?;
            fs::write(temporary.path().join("FORMAT"), vec![b'x'; length])?;
            assert!(matches!(
                DataDirectory::open(temporary.path()),
                Err(DataDirectoryError::MalformedFormat(_))
            ));
        }
        Ok(())
    }

    #[test]
    fn durable_log_cannot_outlive_directory_lock() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("locked-log")?;
        let directory = DataDirectory::open(temporary.path())?;
        let mut opened = directory.open_log()?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"operation".to_vec()])?;

        assert_eq!(opened.recovery.transactions.len(), 0);
        assert!(matches!(
            DataDirectory::open(temporary.path()),
            Err(DataDirectoryError::AlreadyLocked(_))
        ));
        Ok(())
    }

    #[test]
    fn migrates_an_existing_format_one_directory_without_a_manifest() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::new("data-manifest-migration")?;
        let root = temporary.path().join("data");
        fs::create_dir_all(root.join("log"))?;
        fs::write(root.join("FORMAT"), "hyphae-disk-format=1\n")?;
        let log_path = root.join("log/00000000000000000001.hylog");
        let (mut legacy_log, _) = DurableLog::open_file_at_version(&log_path, 0, [0; 32], 1)?;
        legacy_log.append_transaction(Uuid::now_v7(), &[b"preserved".to_vec()])?;
        drop(legacy_log);

        let directory = DataDirectory::open(&root)?;
        assert!(
            root.join("manifest/00000000000000000001.hymanifest")
                .is_file()
        );
        let opened = directory.open_log()?;
        assert_eq!(opened.recovery.transactions.len(), 1);
        assert_eq!(opened.recovery.transactions[0].operations[0], b"preserved");
        Ok(())
    }
}
