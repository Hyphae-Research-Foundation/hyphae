// SPDX-License-Identifier: Apache-2.0

//! Native data-directory identity and single-writer ownership.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use blake3::Hasher;
use hyphae_native_types::{DirectoryUuid, HistoryEpoch, LineageIdentity};
use thiserror::Error;

const FORMAT_FILE: &str = "FORMAT";
const PENDING_FORMAT_FILE: &str = "FORMAT.pending";
const LOCK_FILE: &str = "LOCK";
const NATIVE_FORMAT_PREFIX: &str = "hyphae-native-format=";
const FORMAT2_PREFIX: &str = "hyphae-disk-format=";
const SUPPORTED_FORMAT_VERSION: u64 = 1;
const MAX_FORMAT_BYTES: usize = 128;
const MAX_FORMAT_READ_BYTES: u64 = 129;
const FORMAT2_ONLY_ENTRIES: [&str; 2] = ["indexes", "log"];

static NEXT_DIRECTORY_NONCE: AtomicU64 = AtomicU64::new(1);
const MAX_UUID_V7_MILLISECONDS: u64 = 0x0000_ffff_ffff_ffff;

/// Stable identity of one native data-directory history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDirectoryIdentity {
    directory_id: String,
    lineage: LineageIdentity,
}

impl NativeDirectoryIdentity {
    /// Returns the canonical lowercase, hyphenated `UUIDv7` directory identity.
    pub fn directory_id(&self) -> &str {
        &self.directory_id
    }

    /// Returns the nonzero history epoch.
    pub const fn history_epoch(&self) -> u64 {
        self.lineage.history_epoch().get()
    }

    /// Returns the canonical binary lineage carried by native authority records.
    pub const fn lineage(&self) -> LineageIdentity {
        self.lineage
    }

    fn new(directory_uuid: DirectoryUuid, history_epoch: HistoryEpoch) -> Self {
        Self {
            directory_id: directory_uuid.to_string(),
            lineage: LineageIdentity::new(directory_uuid, history_epoch),
        }
    }

    fn encode(&self) -> String {
        format!(
            "{NATIVE_FORMAT_PREFIX}{SUPPORTED_FORMAT_VERSION} directory={} epoch={}\n",
            self.directory_id,
            self.history_epoch()
        )
    }
}

/// Fail-closed native directory validation or ownership failure.
#[derive(Debug, Error)]
pub enum NativeDirectoryError {
    /// Another live handle owns the exclusive writer lock.
    #[error("native data directory is already locked: {0}")]
    AlreadyLocked(PathBuf),
    /// The mandatory single-writer lock file is missing.
    #[error("native data directory is missing LOCK: {0}")]
    MissingLock(PathBuf),
    /// The mandatory native format marker is missing.
    #[error("native data directory is missing FORMAT: {0}")]
    MissingFormat(PathBuf),
    /// The directory is an unpromoted migration target.
    #[error("native data directory has pending migration marker: {0}")]
    PendingMigration(PathBuf),
    /// Both authoritative and pending markers exist.
    #[error("native data directory has conflicting FORMAT markers: {0}")]
    ConflictingFormatMarkers(PathBuf),
    /// The marker identifies a disk-format-2 directory.
    #[error("disk-format-2 directory cannot be opened as native: {0}")]
    Format2Directory(PathBuf),
    /// The native marker uses an unsupported format version.
    #[error("unsupported native directory format {found}; supported version is {supported}")]
    UnsupportedFormat {
        /// Version found in the marker.
        found: u64,
        /// Version supported by this runtime.
        supported: u64,
    },
    /// The native marker is not byte-for-byte canonical.
    #[error("native FORMAT marker is malformed: {0}")]
    MalformedFormat(PathBuf),
    /// Native and disk-format-2-only entries occur in one directory.
    #[error("native data directory mixes incompatible format families: {0}")]
    MixedFormatFamilies(PathBuf),
    /// The system clock cannot be represented by `UUIDv7`.
    #[error("system clock cannot produce a native UUIDv7 directory identity")]
    InvalidSystemClock,
    /// A filesystem operation required by the directory contract failed.
    #[error("native directory I/O failed for {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub(crate) struct NativeDirectoryGuard {
    identity: NativeDirectoryIdentity,
    _lock: File,
}

impl NativeDirectoryGuard {
    pub(crate) fn initialize(path: &Path) -> Result<Self, NativeDirectoryError> {
        let lock = acquire_lock(path, true)?;
        let identity =
            NativeDirectoryIdentity::new(generate_directory_id(path)?, HistoryEpoch::FIRST);
        write_format_marker(path, &identity)?;
        Ok(Self {
            identity,
            _lock: lock,
        })
    }

    pub(crate) fn open(path: &Path) -> Result<Self, NativeDirectoryError> {
        let lock = acquire_lock(path, false)?;
        let identity = read_and_validate_marker(path)?;
        reject_mixed_format_families(path)?;
        Ok(Self {
            identity,
            _lock: lock,
        })
    }

    pub(crate) const fn identity(&self) -> &NativeDirectoryIdentity {
        &self.identity
    }
}

fn acquire_lock(path: &Path, create: bool) -> Result<File, NativeDirectoryError> {
    let lock_path = path.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .open(&lock_path)
        .map_err(|source| {
            if !create && source.kind() == io::ErrorKind::NotFound {
                NativeDirectoryError::MissingLock(lock_path.clone())
            } else {
                io_error(&lock_path, source)
            }
        })?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(source) => {
            let source = io::Error::from(source);
            if source.kind() == io::ErrorKind::WouldBlock {
                Err(NativeDirectoryError::AlreadyLocked(lock_path))
            } else {
                Err(io_error(&lock_path, source))
            }
        }
    }
}

fn write_format_marker(
    path: &Path,
    identity: &NativeDirectoryIdentity,
) -> Result<(), NativeDirectoryError> {
    let format_path = path.join(FORMAT_FILE);
    let mut format = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&format_path)
        .map_err(|source| io_error(&format_path, source))?;
    format
        .write_all(identity.encode().as_bytes())
        .map_err(|source| io_error(&format_path, source))?;
    format
        .sync_all()
        .map_err(|source| io_error(&format_path, source))?;
    sync_parent_directory(path)?;
    Ok(())
}

fn read_and_validate_marker(path: &Path) -> Result<NativeDirectoryIdentity, NativeDirectoryError> {
    let format_path = path.join(FORMAT_FILE);
    let pending_path = path.join(PENDING_FORMAT_FILE);
    let has_format = format_path
        .try_exists()
        .map_err(|source| io_error(&format_path, source))?;
    let has_pending = pending_path
        .try_exists()
        .map_err(|source| io_error(&pending_path, source))?;

    match (has_format, has_pending) {
        (true, true) => {
            return Err(NativeDirectoryError::ConflictingFormatMarkers(
                path.to_path_buf(),
            ));
        }
        (false, true) => {
            return Err(NativeDirectoryError::PendingMigration(path.to_path_buf()));
        }
        (false, false) => {
            return Err(NativeDirectoryError::MissingFormat(path.to_path_buf()));
        }
        (true, false) => {}
    }

    let mut marker = Vec::new();
    File::open(&format_path)
        .map_err(|source| io_error(&format_path, source))?
        .take(MAX_FORMAT_READ_BYTES)
        .read_to_end(&mut marker)
        .map_err(|source| io_error(&format_path, source))?;
    parse_marker(&format_path, &marker)
}

fn parse_marker(
    format_path: &Path,
    marker: &[u8],
) -> Result<NativeDirectoryIdentity, NativeDirectoryError> {
    if marker.len() > MAX_FORMAT_BYTES {
        return Err(NativeDirectoryError::MalformedFormat(
            format_path.to_path_buf(),
        ));
    }
    let marker = std::str::from_utf8(marker)
        .map_err(|_| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    if marker.starts_with(FORMAT2_PREFIX) {
        return Err(NativeDirectoryError::Format2Directory(
            format_path.to_path_buf(),
        ));
    }

    let line = marker
        .strip_suffix('\n')
        .ok_or_else(|| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    let fields = line.split(' ').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(NativeDirectoryError::MalformedFormat(
            format_path.to_path_buf(),
        ));
    }
    let version_text = fields[0]
        .strip_prefix(NATIVE_FORMAT_PREFIX)
        .ok_or_else(|| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    let version = version_text
        .parse::<u64>()
        .map_err(|_| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    if version_text != version.to_string() {
        return Err(NativeDirectoryError::MalformedFormat(
            format_path.to_path_buf(),
        ));
    }
    if version != SUPPORTED_FORMAT_VERSION {
        return Err(NativeDirectoryError::UnsupportedFormat {
            found: version,
            supported: SUPPORTED_FORMAT_VERSION,
        });
    }

    let directory_text = fields[1]
        .strip_prefix("directory=")
        .ok_or_else(|| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    let directory_uuid = DirectoryUuid::parse_canonical(directory_text)
        .map_err(|_| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;

    let epoch_text = fields[2]
        .strip_prefix("epoch=")
        .ok_or_else(|| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    let history_epoch = epoch_text
        .parse::<u64>()
        .map_err(|_| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    let history_epoch = HistoryEpoch::new(history_epoch)
        .map_err(|_| NativeDirectoryError::MalformedFormat(format_path.to_path_buf()))?;
    if epoch_text != history_epoch.to_string() {
        return Err(NativeDirectoryError::MalformedFormat(
            format_path.to_path_buf(),
        ));
    }

    let identity = NativeDirectoryIdentity::new(directory_uuid, history_epoch);
    if identity.encode() != marker {
        return Err(NativeDirectoryError::MalformedFormat(
            format_path.to_path_buf(),
        ));
    }
    Ok(identity)
}

fn generate_directory_id(path: &Path) -> Result<DirectoryUuid, NativeDirectoryError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NativeDirectoryError::InvalidSystemClock)?;
    let milliseconds =
        u64::try_from(elapsed.as_millis()).map_err(|_| NativeDirectoryError::InvalidSystemClock)?;
    if milliseconds > MAX_UUID_V7_MILLISECONDS {
        return Err(NativeDirectoryError::InvalidSystemClock);
    }

    let mut bytes = [0_u8; 16];
    let timestamp = milliseconds.to_be_bytes();
    bytes[..6].copy_from_slice(&timestamp[2..]);

    let mut hasher = Hasher::new();
    hasher.update(&elapsed.as_nanos().to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(
        &NEXT_DIRECTORY_NONCE
            .fetch_add(1, Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.update(path.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    bytes[6..].copy_from_slice(&digest.as_bytes()[..10]);

    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    DirectoryUuid::new(bytes).map_err(|_| NativeDirectoryError::InvalidSystemClock)
}

fn reject_mixed_format_families(path: &Path) -> Result<(), NativeDirectoryError> {
    for entry in FORMAT2_ONLY_ENTRIES {
        let entry_path = path.join(entry);
        if entry_path
            .try_exists()
            .map_err(|source| io_error(&entry_path, source))?
        {
            return Err(NativeDirectoryError::MixedFormatFamilies(entry_path));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), NativeDirectoryError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), NativeDirectoryError> {
    Ok(())
}

fn io_error(path: &Path, source: io::Error) -> NativeDirectoryError {
    NativeDirectoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeDirectoryError, NativeDirectoryIdentity, parse_marker};
    use hyphae_native_types::{DirectoryUuid, HistoryEpoch};
    use std::path::Path;

    #[test]
    fn canonical_marker_has_golden_bytes() -> Result<(), hyphae_native_types::NativeTypeError> {
        let identity = NativeDirectoryIdentity::new(
            DirectoryUuid::parse_canonical("018f4e9d-3d7a-7b6c-8f12-123456789abc")?,
            HistoryEpoch::new(42)?,
        );
        assert_eq!(
            identity.encode(),
            "hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=42\n"
        );
        Ok(())
    }

    #[test]
    fn malformed_marker_matrix_fails_closed() {
        let path = Path::new("FORMAT");
        let malformed = [
            b"".as_slice(),
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=1",
            b"hyphae-native-format=01 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=1\n",
            b"hyphae-native-format=1  directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=1\n",
            b"hyphae-native-format=1 epoch=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc\n",
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=0\n",
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=01\n",
            b"hyphae-native-format=1 directory=018F4E9D-3D7A-7B6C-8F12-123456789ABC epoch=1\n",
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-6b6c-8f12-123456789abc epoch=1\n",
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-:f12-123456789abc epoch=1\n",
            b"hyphae-native-format=1 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=1\nextra",
            b"hyphae-native-format=02 directory=018f4e9d-3d7a-7b6c-8f12-123456789abc epoch=1\n",
        ];
        for marker in malformed {
            assert!(matches!(
                parse_marker(path, marker),
                Err(NativeDirectoryError::MalformedFormat(_))
            ));
        }
        let oversized = vec![b'x'; 129];
        assert!(matches!(
            parse_marker(path, &oversized),
            Err(NativeDirectoryError::MalformedFormat(_))
        ));
    }
}
