// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use hyphae_core::{DISK_FORMAT_VERSION, MIN_DISK_FORMAT_VERSION};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    DataDirectory, DurableLog, SnapshotError, SnapshotInfo, StorageEngine, StorageError,
    manifest::StorageManifest, verify_snapshot,
};

const BACKUP_MANIFEST: &str = "BACKUP.json";
const BACKUP_SNAPSHOT: &str = "snapshot.hysnap";
const BACKUP_KIND: &str = "hyphae-backup";
const BACKUP_FORMAT_VERSION: u16 = 1;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Failure while creating, verifying, or restoring a portable backup.
#[derive(Debug, Error)]
pub enum BackupError {
    /// The requested destination already exists and is never replaced.
    #[error("backup or restore destination already exists: {0}")]
    DestinationExists(PathBuf),

    /// A backup destination inside the live directory would couple lifecycles.
    #[error("backup destination must be outside the live data directory: {0}")]
    DestinationInsideDataDirectory(PathBuf),

    /// A restore destination inside its source backup is unsafe.
    #[error("restore destination must be outside the backup directory: {0}")]
    RestoreInsideBackup(PathBuf),

    /// The backup directory does not contain exactly the canonical two files.
    #[error("invalid backup layout at {path}: {reason}")]
    InvalidLayout {
        /// Backup path being validated.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// The bounded JSON manifest is malformed or disagrees with its snapshot.
    #[error("invalid backup manifest at {path}: {reason}")]
    InvalidManifest {
        /// Manifest path being validated.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Backup JSON could not be decoded.
    #[error("failed to decode backup manifest {path}: {source}")]
    ManifestJson {
        /// Manifest path being decoded.
        path: PathBuf,
        /// JSON decoding failure.
        #[source]
        source: serde_json::Error,
    },

    /// Snapshot creation or validation failed.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    /// Opening or validating restored storage failed before activation.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A filesystem operation failed.
    #[error("failed to {action} {path}: {source}")]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
}

/// Verified metadata for one portable backup directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupInfo {
    /// Canonical backup directory.
    pub path: PathBuf,
    /// Verified logical snapshot stored by the backup.
    pub snapshot: SnapshotInfo,
}

/// Evidence that a backup was fully verified before destination activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreInfo {
    /// Newly activated data directory.
    pub data_path: PathBuf,
    /// Logical snapshot verified after index reconstruction and reopen.
    pub snapshot: SnapshotInfo,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    kind: String,
    backup_format_version: u16,
    disk_format_version: u16,
    snapshot_file: String,
    checkpoint_sequence: u64,
    checkpoint_digest: Option<String>,
    entry_count: u64,
    #[serde(default)]
    vector_space_count: u64,
    #[serde(default)]
    vector_count: u64,
    #[serde(default)]
    lexical_index_count: u64,
    receipt_count: u64,
    snapshot_digest: String,
    snapshot_file_bytes: u64,
}

impl BackupManifest {
    fn from_snapshot(snapshot: &SnapshotInfo) -> Self {
        Self {
            kind: BACKUP_KIND.to_owned(),
            backup_format_version: BACKUP_FORMAT_VERSION,
            disk_format_version: snapshot.disk_format_version,
            snapshot_file: BACKUP_SNAPSHOT.to_owned(),
            checkpoint_sequence: snapshot.checkpoint_sequence,
            checkpoint_digest: snapshot.checkpoint_digest.map(|digest| encode_hex(&digest)),
            entry_count: snapshot.entry_count,
            vector_space_count: snapshot.vector_space_count,
            vector_count: snapshot.vector_count,
            lexical_index_count: snapshot.lexical_index_count,
            receipt_count: snapshot.receipt_count,
            snapshot_digest: encode_hex(&snapshot.snapshot_digest),
            snapshot_file_bytes: snapshot.file_bytes,
        }
    }

    fn matches(&self, snapshot: &SnapshotInfo) -> bool {
        self.kind == BACKUP_KIND
            && self.backup_format_version == BACKUP_FORMAT_VERSION
            && (MIN_DISK_FORMAT_VERSION..=DISK_FORMAT_VERSION).contains(&self.disk_format_version)
            && self.disk_format_version == snapshot.disk_format_version
            && self.snapshot_file == BACKUP_SNAPSHOT
            && self.checkpoint_sequence == snapshot.checkpoint_sequence
            && self.checkpoint_digest
                == snapshot.checkpoint_digest.map(|digest| encode_hex(&digest))
            && self.entry_count == snapshot.entry_count
            && self.vector_space_count == snapshot.vector_space_count
            && self.vector_count == snapshot.vector_count
            && self.lexical_index_count == snapshot.lexical_index_count
            && self.receipt_count == snapshot.receipt_count
            && self.snapshot_digest == encode_hex(&snapshot.snapshot_digest)
            && self.snapshot_file_bytes == snapshot.file_bytes
    }
}

pub(crate) fn create_backup(
    storage: &StorageEngine,
    destination: &Path,
) -> Result<BackupInfo, BackupError> {
    let parent = prepare_destination_parent(destination)?;
    let source_root = fs::canonicalize(storage.data_path()).map_err(|source| BackupError::Io {
        action: "canonicalize live data directory",
        path: storage.data_path().to_path_buf(),
        source,
    })?;
    let destination_parent = fs::canonicalize(&parent).map_err(|source| BackupError::Io {
        action: "canonicalize backup parent",
        path: parent.clone(),
        source,
    })?;
    if destination_parent.starts_with(&source_root) {
        return Err(BackupError::DestinationInsideDataDirectory(
            destination.to_path_buf(),
        ));
    }

    let snapshot = storage.snapshot().map_err(|source| match source {
        StorageError::Snapshot { source } => BackupError::Snapshot(*source),
        other => BackupError::Storage(other),
    })?;
    let staging = staging_path(destination, "backup")?;
    fs::create_dir(&staging).map_err(|source| BackupError::Io {
        action: "create backup staging directory",
        path: staging.clone(),
        source,
    })?;
    let result = write_backup_staging(&staging, &snapshot).and_then(|()| {
        let staged = verify_backup(&staging)?;
        fs::rename(&staging, destination).map_err(|source| BackupError::Io {
            action: "atomically promote verified backup",
            path: destination.to_path_buf(),
            source,
        })?;
        sync_directory(&parent)?;
        Ok(BackupInfo {
            path: destination.to_path_buf(),
            snapshot: SnapshotInfo {
                path: destination.join(BACKUP_SNAPSHOT),
                ..staged.snapshot
            },
        })
    });
    if result.is_err() {
        let _ignored = fs::remove_dir_all(&staging);
    }
    result
}

/// Verifies a backup layout, bounded manifest, and complete snapshot.
///
/// # Errors
///
/// Returns an error for unexpected files, symlinks, malformed metadata,
/// snapshot corruption, or any manifest/snapshot mismatch.
pub fn verify_backup(path: impl AsRef<Path>) -> Result<BackupInfo, BackupError> {
    let path = path.as_ref();
    validate_layout(path)?;
    let manifest_path = path.join(BACKUP_MANIFEST);
    let manifest = read_manifest(&manifest_path)?;
    let snapshot_path = path.join(BACKUP_SNAPSHOT);
    let snapshot = verify_snapshot(&snapshot_path)?;
    if !manifest.matches(&snapshot) {
        return Err(BackupError::InvalidManifest {
            path: manifest_path,
            reason: "manifest fields do not match the verified snapshot",
        });
    }
    Ok(BackupInfo {
        path: path.to_path_buf(),
        snapshot,
    })
}

/// Restores a verified backup to a new data directory.
///
/// The destination name becomes visible only after the snapshot is installed,
/// the materialized index is rebuilt, and the complete storage engine reopens
/// at the expected checkpoint.
///
/// # Errors
///
/// Returns an error when verification fails, the destination exists, or any
/// staging, index-rebuild, reopen, or atomic-promotion operation fails.
pub fn restore_backup(
    backup: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<RestoreInfo, BackupError> {
    let backup = backup.as_ref();
    let destination = destination.as_ref();
    let verified = verify_backup(backup)?;
    let parent = prepare_destination_parent(destination)?;
    let backup_root = fs::canonicalize(backup).map_err(|source| BackupError::Io {
        action: "canonicalize backup directory",
        path: backup.to_path_buf(),
        source,
    })?;
    let destination_parent = fs::canonicalize(&parent).map_err(|source| BackupError::Io {
        action: "canonicalize restore parent",
        path: parent.clone(),
        source,
    })?;
    if destination_parent.starts_with(&backup_root) {
        return Err(BackupError::RestoreInsideBackup(destination.to_path_buf()));
    }

    let staging = staging_path(destination, "restore")?;
    fs::create_dir(&staging).map_err(|source| BackupError::Io {
        action: "create restore staging directory",
        path: staging.clone(),
        source,
    })?;
    let result = restore_into_staging(&verified, &staging).and_then(|snapshot| {
        fs::rename(&staging, destination).map_err(|source| BackupError::Io {
            action: "atomically activate restored data directory",
            path: destination.to_path_buf(),
            source,
        })?;
        sync_directory(&parent)?;
        Ok(RestoreInfo {
            data_path: destination.to_path_buf(),
            snapshot: SnapshotInfo {
                path: destination
                    .join("snapshots")
                    .join(snapshot_filename(snapshot.checkpoint_sequence)),
                ..snapshot
            },
        })
    });
    if result.is_err() {
        let _ignored = fs::remove_dir_all(&staging);
    }
    result
}

fn write_backup_staging(staging: &Path, snapshot: &SnapshotInfo) -> Result<(), BackupError> {
    let copied_path = staging.join(BACKUP_SNAPSHOT);
    copy_new_file(&snapshot.path, &copied_path, "copy backup snapshot")?;
    let copied = verify_snapshot(&copied_path)?;
    if !same_snapshot_identity(snapshot, &copied) {
        return Err(BackupError::InvalidManifest {
            path: copied_path,
            reason: "snapshot changed while backup was copied",
        });
    }
    let mut encoded =
        serde_json::to_vec_pretty(&BackupManifest::from_snapshot(&copied)).map_err(|source| {
            BackupError::ManifestJson {
                path: staging.join(BACKUP_MANIFEST),
                source,
            }
        })?;
    encoded.push(b'\n');
    let manifest_path = staging.join(BACKUP_MANIFEST);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&manifest_path)
        .map_err(|source| BackupError::Io {
            action: "create backup manifest",
            path: manifest_path.clone(),
            source,
        })?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|source| BackupError::Io {
            action: "synchronize backup manifest",
            path: manifest_path,
            source,
        })?;
    sync_directory(staging)
}

fn restore_into_staging(backup: &BackupInfo, staging: &Path) -> Result<SnapshotInfo, BackupError> {
    let mut directory = DataDirectory::open(staging).map_err(StorageError::from)?;
    if backup.snapshot.disk_format_version != directory.disk_format_version() {
        write_format_marker_for_restore(staging, backup.snapshot.disk_format_version)?;
        drop(directory);
        directory = DataDirectory::open(staging).map_err(StorageError::from)?;
    }
    let checkpoint = backup.snapshot.checkpoint_sequence;
    if checkpoint > 0 {
        let snapshot_path = staging
            .join("snapshots")
            .join(snapshot_filename(checkpoint));
        copy_new_file(
            &backup.snapshot.path,
            &snapshot_path,
            "copy restored snapshot",
        )?;
        let restored = verify_snapshot(&snapshot_path)?;
        if !same_snapshot_identity(&backup.snapshot, &restored) {
            return Err(BackupError::InvalidManifest {
                path: snapshot_path,
                reason: "restored snapshot differs from verified backup",
            });
        }
        let base_digest = restored
            .checkpoint_digest
            .ok_or(BackupError::InvalidManifest {
                path: backup.path.join(BACKUP_MANIFEST),
                reason: "nonempty backup lacks a checkpoint digest",
            })?;
        let manifest = StorageManifest {
            generation: 2,
            active_segment: 2,
            base_sequence: checkpoint,
            base_digest,
            snapshot_digest: restored.snapshot_digest,
        };
        let (active_log, recovery) = DurableLog::open_file_at_version(
            staging.join("log/00000000000000000002.hylog"),
            checkpoint,
            base_digest,
            backup.snapshot.disk_format_version,
        )
        .map_err(StorageError::from)?;
        if recovery.valid_bytes != 0 {
            return Err(BackupError::InvalidLayout {
                path: staging.to_path_buf(),
                reason: "new restore log segment is not empty",
            });
        }
        drop(active_log);
        directory
            .commit_manifest(manifest)
            .map_err(StorageError::from)?;
    }
    drop(directory);

    let opened = StorageEngine::open(staging)?;
    let rebuilt = opened.storage.snapshot().map_err(|source| match source {
        StorageError::Snapshot { source } => BackupError::Snapshot(*source),
        other => BackupError::Storage(other),
    })?;
    if !same_snapshot_identity(&backup.snapshot, &rebuilt) {
        return Err(BackupError::InvalidManifest {
            path: backup.path.join(BACKUP_MANIFEST),
            reason: "restored engine checkpoint differs from backup",
        });
    }
    drop(opened);
    sync_directory(staging)?;
    Ok(rebuilt)
}

fn prepare_destination_parent(destination: &Path) -> Result<PathBuf, BackupError> {
    if destination.exists() {
        return Err(BackupError::DestinationExists(destination.to_path_buf()));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    if destination.file_name().is_none() {
        return Err(BackupError::InvalidLayout {
            path: destination.to_path_buf(),
            reason: "destination has no final path component",
        });
    }
    fs::create_dir_all(&parent).map_err(|source| BackupError::Io {
        action: "create destination parent",
        path: parent.clone(),
        source,
    })?;
    Ok(parent)
}

fn staging_path(destination: &Path, operation: &str) -> Result<PathBuf, BackupError> {
    let filename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| BackupError::InvalidLayout {
            path: destination.to_path_buf(),
            reason: "destination filename is not valid Unicode",
        })?;
    Ok(destination.with_file_name(format!(
        ".{filename}.hyphae-{operation}-{}.tmp",
        Uuid::now_v7()
    )))
}

fn validate_layout(path: &Path) -> Result<(), BackupError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| BackupError::Io {
        action: "inspect backup directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BackupError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "backup root must be a real directory",
        });
    }
    let mut manifest_seen = false;
    let mut snapshot_seen = false;
    for entry in fs::read_dir(path).map_err(|source| BackupError::Io {
        action: "list backup directory",
        path: path.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| BackupError::Io {
            action: "read backup directory entry",
            path: path.to_path_buf(),
            source,
        })?;
        if !entry
            .file_type()
            .map_err(|source| BackupError::Io {
                action: "inspect backup file",
                path: entry.path(),
                source,
            })?
            .is_file()
        {
            return Err(BackupError::InvalidLayout {
                path: entry.path(),
                reason: "backup entries must be regular files",
            });
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(BackupError::InvalidLayout {
                path: entry.path(),
                reason: "backup filename is not valid Unicode",
            });
        };
        match name.as_str() {
            BACKUP_MANIFEST => manifest_seen = true,
            BACKUP_SNAPSHOT => snapshot_seen = true,
            _ => {
                return Err(BackupError::InvalidLayout {
                    path: entry.path(),
                    reason: "backup contains an unexpected file",
                });
            }
        }
    }
    if !manifest_seen || !snapshot_seen {
        return Err(BackupError::InvalidLayout {
            path: path.to_path_buf(),
            reason: "backup must contain exactly BACKUP.json and snapshot.hysnap",
        });
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<BackupManifest, BackupError> {
    let metadata = fs::metadata(path).map_err(|source| BackupError::Io {
        action: "inspect backup manifest",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(BackupError::InvalidManifest {
            path: path.to_path_buf(),
            reason: "manifest exceeds 64 KiB",
        });
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| BackupError::InvalidManifest {
        path: path.to_path_buf(),
        reason: "manifest length does not fit memory limits",
    })?;
    let mut encoded = Vec::with_capacity(capacity);
    File::open(path)
        .map(|file| file.take(MAX_MANIFEST_BYTES.saturating_add(1)))
        .and_then(|mut bounded| bounded.read_to_end(&mut encoded))
        .map_err(|source| BackupError::Io {
            action: "read backup manifest",
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
        return Err(BackupError::InvalidManifest {
            path: path.to_path_buf(),
            reason: "manifest exceeds 64 KiB",
        });
    }
    serde_json::from_slice(&encoded).map_err(|source| BackupError::ManifestJson {
        path: path.to_path_buf(),
        source,
    })
}

fn copy_new_file(
    source: &Path,
    destination: &Path,
    action: &'static str,
) -> Result<(), BackupError> {
    let metadata = fs::symlink_metadata(source).map_err(|source_error| BackupError::Io {
        action: "inspect source file",
        path: source.to_path_buf(),
        source: source_error,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(BackupError::InvalidLayout {
            path: source.to_path_buf(),
            reason: "snapshot must be a regular file",
        });
    }
    let mut input = File::open(source).map_err(|source_error| BackupError::Io {
        action,
        path: source.to_path_buf(),
        source: source_error,
    })?;
    let opened_metadata = input.metadata().map_err(|source_error| BackupError::Io {
        action: "inspect opened source file",
        path: source.to_path_buf(),
        source: source_error,
    })?;
    if !opened_metadata.is_file() {
        return Err(BackupError::InvalidLayout {
            path: source.to_path_buf(),
            reason: "opened snapshot must be a regular file",
        });
    }
    let expected_bytes = opened_metadata.len();
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|source_error| BackupError::Io {
            action,
            path: destination.to_path_buf(),
            source: source_error,
        })?;
    let copied_bytes =
        io::copy(&mut (&mut input).take(expected_bytes), &mut output).map_err(|source_error| {
            BackupError::Io {
                action,
                path: destination.to_path_buf(),
                source: source_error,
            }
        })?;
    let final_bytes = input
        .metadata()
        .map_err(|source_error| BackupError::Io {
            action: "reinspect copied source file",
            path: source.to_path_buf(),
            source: source_error,
        })?
        .len();
    if copied_bytes != expected_bytes || final_bytes != expected_bytes {
        return Err(BackupError::InvalidLayout {
            path: source.to_path_buf(),
            reason: "snapshot changed length while it was copied",
        });
    }
    output.sync_all().map_err(|source_error| BackupError::Io {
        action,
        path: destination.to_path_buf(),
        source: source_error,
    })?;
    Ok(())
}

fn same_snapshot_identity(left: &SnapshotInfo, right: &SnapshotInfo) -> bool {
    left.disk_format_version == right.disk_format_version
        && left.checkpoint_sequence == right.checkpoint_sequence
        && left.checkpoint_digest == right.checkpoint_digest
        && left.entry_count == right.entry_count
        && left.vector_space_count == right.vector_space_count
        && left.vector_count == right.vector_count
        && left.lexical_index_count == right.lexical_index_count
        && left.receipt_count == right.receipt_count
        && left.snapshot_digest == right.snapshot_digest
        && left.file_bytes == right.file_bytes
}

fn write_format_marker_for_restore(path: &Path, version: u16) -> Result<(), BackupError> {
    if !(MIN_DISK_FORMAT_VERSION..=DISK_FORMAT_VERSION).contains(&version) {
        return Err(BackupError::InvalidManifest {
            path: path.join(BACKUP_MANIFEST),
            reason: "backup uses an unsupported disk format",
        });
    }
    let marker = path.join("FORMAT");
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&marker)
        .map_err(|source| BackupError::Io {
            action: "open restored format marker",
            path: marker.clone(),
            source,
        })?;
    writeln!(file, "hyphae-disk-format={version}")
        .and_then(|()| file.sync_all())
        .map_err(|source| BackupError::Io {
            action: "write restored format marker",
            path: marker,
            source,
        })
}

fn snapshot_filename(sequence: u64) -> String {
    format!("snapshot-{sequence:020}.hysnap")
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), BackupError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| BackupError::Io {
            action: "synchronize directory",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keep the fallible directory-sync interface shared with Unix callers"
)]
fn sync_directory(_path: &Path) -> Result<(), BackupError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        io::{Seek, SeekFrom, Write},
    };

    use uuid::Uuid;

    use super::{BackupError, restore_backup, verify_backup};
    use crate::{AppendOutcome, Mutation, StorageEngine, test_support::TestDirectory};

    #[test]
    fn backup_restore_preserves_values_receipts_and_sequence() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("backup-round-trip")?;
        let source = temporary.path().join("source");
        let backup = temporary.path().join("backup");
        let restored = temporary.path().join("restored");
        let transaction_id = Uuid::now_v7();
        let mutation = Mutation::put(b"alpha", b"value".to_vec());
        let mut opened = StorageEngine::open(&source)?;
        let committed = opened
            .storage
            .write(transaction_id, std::slice::from_ref(&mutation))?;
        let AppendOutcome::Committed(receipt) = committed else {
            return Err("initial write was not committed".into());
        };
        let created = opened.storage.backup(&backup)?;
        assert_eq!(created, verify_backup(&backup)?);
        drop(opened);

        let activated = restore_backup(&backup, &restored)?;
        assert_eq!(
            activated.snapshot.snapshot_digest,
            created.snapshot.snapshot_digest
        );
        let mut reopened = StorageEngine::open(&restored)?;
        assert_eq!(reopened.storage.get(b"alpha")?, Some(b"value".to_vec()));
        assert!(matches!(
            reopened.storage.write(transaction_id, std::slice::from_ref(&mutation))?,
            AppendOutcome::Existing(existing) if existing == receipt
        ));
        let next = reopened
            .storage
            .write(Uuid::now_v7(), &[Mutation::put(b"beta", b"next".to_vec())])?;
        let next_receipt = match next {
            AppendOutcome::Committed(next_receipt) | AppendOutcome::Existing(next_receipt) => {
                next_receipt
            }
        };
        assert!(next_receipt.commit_sequence > receipt.commit_sequence);
        Ok(())
    }

    #[test]
    fn corrupt_backup_never_activates_destination() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("backup-corruption")?;
        let source = temporary.path().join("source");
        let backup = temporary.path().join("backup");
        let destination = temporary.path().join("destination");
        let mut opened = StorageEngine::open(&source)?;
        opened.storage.write(
            Uuid::now_v7(),
            &[Mutation::put(b"alpha", b"value".to_vec())],
        )?;
        opened.storage.backup(&backup)?;
        drop(opened);

        let snapshot = backup.join("snapshot.hysnap");
        let mut file = fs::OpenOptions::new().write(true).open(&snapshot)?;
        file.seek(SeekFrom::Start(16))?;
        file.write_all(&[0xff])?;
        file.sync_all()?;
        assert!(restore_backup(&backup, &destination).is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn backup_refuses_existing_and_live_directory_destinations() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("backup-destinations")?;
        let source = temporary.path().join("source");
        let existing = temporary.path().join("existing");
        fs::create_dir(&existing)?;
        let opened = StorageEngine::open(&source)?;
        assert!(matches!(
            opened.storage.backup(&existing),
            Err(BackupError::DestinationExists(_))
        ));
        assert!(matches!(
            opened.storage.backup(source.join("nested-backup")),
            Err(BackupError::DestinationInsideDataDirectory(_))
        ));
        Ok(())
    }

    #[test]
    fn backup_layout_manifest_and_restore_location_are_bounded() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("backup-input-bounds")?;
        let source = temporary.path().join("source");
        let backup = temporary.path().join("backup");
        let opened = StorageEngine::open(&source)?;
        opened.storage.backup(&backup)?;
        drop(opened);

        let extra = backup.join("unexpected");
        fs::write(&extra, b"unexpected")?;
        assert!(matches!(
            verify_backup(&backup),
            Err(BackupError::InvalidLayout { .. })
        ));
        fs::remove_file(extra)?;

        assert!(matches!(
            restore_backup(&backup, backup.join("nested")),
            Err(BackupError::RestoreInsideBackup(_))
        ));

        let manifest = backup.join("BACKUP.json");
        fs::OpenOptions::new()
            .write(true)
            .open(&manifest)?
            .set_len(64 * 1024 + 1)?;
        assert!(matches!(
            verify_backup(&backup),
            Err(BackupError::InvalidManifest { .. })
        ));
        Ok(())
    }

    #[test]
    fn empty_backup_restores_as_an_empty_writable_engine() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("backup-empty")?;
        let source = temporary.path().join("source");
        let backup = temporary.path().join("backup");
        let restored = temporary.path().join("restored");
        let opened = StorageEngine::open(&source)?;
        let created = opened.storage.backup(&backup)?;
        assert_eq!(created.snapshot.checkpoint_sequence, 0);
        drop(opened);

        restore_backup(&backup, &restored)?;
        let mut reopened = StorageEngine::open(&restored)?;
        assert_eq!(reopened.storage.get(b"missing")?, None);
        assert!(matches!(
            reopened.storage.write(
                Uuid::now_v7(),
                &[Mutation::put(b"first", b"value".to_vec())]
            )?,
            AppendOutcome::Committed(_)
        ));
        Ok(())
    }
}
