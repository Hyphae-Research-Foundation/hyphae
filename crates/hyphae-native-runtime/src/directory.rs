// SPDX-License-Identifier: Apache-2.0

//! Native data-directory identity and single-writer ownership.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use blake3::Hasher;
use hyphae_native_types::{DirectoryUuid, HistoryEpoch, LineageIdentity};
use same_file::Handle as FileIdentityHandle;
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

/// Deterministic pending-marker promotion boundary used by crash matrices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionBoundary {
    /// The pending marker is validated but has not been renamed.
    BeforeRename,
    /// The marker has its authoritative name but the parent is not synchronized.
    MarkerRenamed,
    /// The marker rename and parent-directory synchronization are complete.
    ParentSynchronized,
}

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
    /// The current OS account does not own one stable, protected directory.
    #[error("native data directory owner authority is invalid")]
    OfflineOwnerAuthorityDenied,
    /// A deterministic migration-promotion interruption was requested.
    #[error("native migration promotion interrupted at {0:?}; reopen the data directory")]
    InjectedPromotionCrash(PromotionBoundary),
}

#[derive(Debug)]
pub(crate) struct NativeDirectoryGuard {
    identity: NativeDirectoryIdentity,
    pending: bool,
    _lock: File,
    offline_owner: Option<OfflineOwnerAuthority>,
}

#[derive(Debug)]
struct OfflineOwnerAuthority {
    identity: FileIdentityHandle,
}

impl NativeDirectoryGuard {
    pub(crate) fn initialize(path: &Path) -> Result<Self, NativeDirectoryError> {
        Self::initialize_with_marker(path, false)
    }

    pub(crate) fn initialize_pending(path: &Path) -> Result<Self, NativeDirectoryError> {
        Self::initialize_with_marker(path, true)
    }

    fn initialize_with_marker(path: &Path, pending: bool) -> Result<Self, NativeDirectoryError> {
        #[cfg(windows)]
        restrict_windows_data_directory(path)?;
        let lock = acquire_lock(path, true)?;
        let identity =
            NativeDirectoryIdentity::new(generate_directory_id(path)?, HistoryEpoch::FIRST);
        write_format_marker(path, &identity, pending)?;
        Ok(Self {
            identity,
            pending,
            _lock: lock,
            offline_owner: None,
        })
    }

    pub(crate) fn open(path: &Path) -> Result<Self, NativeDirectoryError> {
        Self::open_with_pending(path, false, false)
    }

    pub(crate) fn open_pending(path: &Path) -> Result<Self, NativeDirectoryError> {
        Self::open_with_pending(path, true, false)
    }

    pub(crate) fn open_offline_owner(path: &Path) -> Result<Self, NativeDirectoryError> {
        Self::open_with_pending(path, false, true)
    }

    fn open_with_pending(
        path: &Path,
        allow_pending: bool,
        require_offline_owner: bool,
    ) -> Result<Self, NativeDirectoryError> {
        let offline_owner = require_offline_owner
            .then(|| OfflineOwnerAuthority::validate(path))
            .transpose()?;
        let lock = acquire_lock(path, false)?;
        if let Some(authority) = &offline_owner {
            authority.revalidate(path)?;
        }
        let identity = read_and_validate_marker(path, allow_pending)?;
        reject_mixed_format_families(path)?;
        Ok(Self {
            identity,
            pending: !path
                .join(FORMAT_FILE)
                .try_exists()
                .map_err(|source| io_error(&path.join(FORMAT_FILE), source))?,
            _lock: lock,
            offline_owner,
        })
    }

    pub(crate) fn promote_pending(&mut self, path: &Path) -> Result<(), NativeDirectoryError> {
        self.promote_pending_at(path, None)
    }

    pub(crate) fn promote_pending_with_interruption(
        &mut self,
        path: &Path,
        boundary: PromotionBoundary,
    ) -> Result<(), NativeDirectoryError> {
        self.promote_pending_at(path, Some(boundary))
    }

    fn promote_pending_at(
        &mut self,
        path: &Path,
        interruption: Option<PromotionBoundary>,
    ) -> Result<(), NativeDirectoryError> {
        if !self.pending {
            return Ok(());
        }
        let pending_path = path.join(PENDING_FORMAT_FILE);
        let format_path = path.join(FORMAT_FILE);
        if format_path
            .try_exists()
            .map_err(|source| io_error(&format_path, source))?
        {
            return Err(NativeDirectoryError::ConflictingFormatMarkers(
                path.to_path_buf(),
            ));
        }
        interrupt_promotion(interruption, PromotionBoundary::BeforeRename)?;
        fs::rename(&pending_path, &format_path)
            .map_err(|source| io_error(&pending_path, source))?;
        self.pending = false;
        interrupt_promotion(interruption, PromotionBoundary::MarkerRenamed)?;
        sync_parent_directory(path)?;
        interrupt_promotion(interruption, PromotionBoundary::ParentSynchronized)
    }

    pub(crate) const fn identity(&self) -> &NativeDirectoryIdentity {
        &self.identity
    }

    pub(crate) fn revalidate_offline_owner(&self, path: &Path) -> Result<(), NativeDirectoryError> {
        self.offline_owner
            .as_ref()
            .ok_or_else(owner_authority_error)?
            .revalidate(path)
    }
}

impl OfflineOwnerAuthority {
    fn validate(path: &Path) -> Result<Self, NativeDirectoryError> {
        validate_offline_owner_path(path).map_err(|_| owner_authority_error())
    }

    fn revalidate(&self, path: &Path) -> Result<(), NativeDirectoryError> {
        let current = Self::validate(path)?;
        if self.identity != current.identity {
            return Err(owner_authority_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn validate_offline_owner_path(path: &Path) -> io::Result<OfflineOwnerAuthority> {
    use std::os::unix::fs::MetadataExt;

    let before = fs::symlink_metadata(path)?;
    if before.file_type().is_symlink()
        || !before.file_type().is_dir()
        || before.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data directory owner authority is invalid",
        ));
    }
    let identity = FileIdentityHandle::from_path(path)?;
    let opened = identity.as_file().metadata()?;
    let current = fs::symlink_metadata(path)?;
    let expected_identity = (before.dev(), before.ino());
    if !opened.file_type().is_dir()
        || !current.file_type().is_dir()
        || current.file_type().is_symlink()
        || opened.uid() != before.uid()
        || current.uid() != before.uid()
        || (opened.dev(), opened.ino()) != expected_identity
        || (current.dev(), current.ino()) != expected_identity
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data directory identity changed",
        ));
    }
    Ok(OfflineOwnerAuthority { identity })
}

#[cfg(windows)]
fn validate_offline_owner_path(path: &Path) -> io::Result<OfflineOwnerAuthority> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if is_windows_named_stream(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named streams cannot carry directory authority",
        ));
    }
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_dir() || before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data directory is not a regular directory",
        ));
    }
    let identity = FileIdentityHandle::from_path(path)?;
    validate_windows_data_directory(identity.as_file())?;
    let current = fs::symlink_metadata(path)?;
    if !current.file_type().is_dir()
        || current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || identity != FileIdentityHandle::from_path(path)?
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data directory identity changed",
        ));
    }
    Ok(OfflineOwnerAuthority { identity })
}

#[cfg(windows)]
fn is_windows_named_stream(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    path.components().any(|component| match component {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => false,
            _ => prefix.as_os_str().to_string_lossy().contains(':'),
        },
        Component::Normal(value) => value.to_string_lossy().contains(':'),
        Component::RootDir | Component::CurDir | Component::ParentDir => false,
    })
}

#[cfg(windows)]
fn validate_windows_data_directory(file: &File) -> io::Result<()> {
    use windows_permissions::{
        LocalBox, Sid,
        constants::{AccessRights, AceFlags, AceType, SeObjectType, SecurityInformation},
        utilities, wrappers,
    };

    let actual = wrappers::GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    let sddl = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
        &actual,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    let current_sid = utilities::current_process_sid()?;
    let current = current_sid.to_string();
    let system_sid: LocalBox<Sid> = "S-1-5-18".parse()?;
    if actual.owner() != Some(current_sid.as_ref())
        || !windows_sddl_has_protected_dacl(&sddl.to_string_lossy())
    {
        return Err(windows_directory_acl_error());
    }
    let dacl = actual.dacl().ok_or_else(windows_directory_acl_error)?;
    let expected = if current == "S-1-5-18" { 1 } else { 2 };
    if dacl.len() != expected {
        return Err(windows_directory_acl_error());
    }
    let mut current_seen = false;
    let mut system_seen = false;
    for index in 0..dacl.len() {
        let ace = dacl
            .get_ace(index)
            .ok_or_else(windows_directory_acl_error)?;
        let allowed_flags = AceFlags::ObjectInherit | AceFlags::ContainerInherit;
        if ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
            || !(ace.flags().is_empty() || ace.flags() == allowed_flags)
            || ace.mask() != AccessRights::FileAllAccess
        {
            return Err(windows_directory_acl_error());
        }
        let sid = ace.sid().ok_or_else(windows_directory_acl_error)?;
        if sid == current_sid.as_ref() && !current_seen {
            current_seen = true;
        } else if sid == system_sid.as_ref() && !system_seen {
            system_seen = true;
        } else {
            return Err(windows_directory_acl_error());
        }
    }
    if !current_seen || (current != "S-1-5-18" && !system_seen) {
        return Err(windows_directory_acl_error());
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_data_directory(path: &Path) -> Result<(), NativeDirectoryError> {
    use std::os::windows::fs::MetadataExt;
    use windows_permissions::{
        LocalBox, SecurityDescriptor,
        constants::{SeObjectType, SecurityInformation},
        utilities, wrappers,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if is_windows_named_stream(path) {
        return Err(owner_authority_error());
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(owner_authority_error());
    }
    let current_sid = utilities::current_process_sid().map_err(|source| io_error(path, source))?;
    let current = current_sid.to_string();
    let system = "S-1-5-18";
    let sddl = if current == system {
        format!("D:P(A;OICI;FA;;;{system})")
    } else {
        format!("D:P(A;OICI;FA;;;{current})(A;OICI;FA;;;{system})")
    };
    let descriptor: LocalBox<SecurityDescriptor> =
        sddl.parse().map_err(|source| io_error(path, source))?;
    wrappers::SetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        descriptor.dacl(),
        None,
    )
    .map_err(|source| io_error(path, source))?;
    wrappers::SetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner,
        Some(current_sid.as_ref()),
        None,
        None,
        None,
    )
    .map_err(|source| io_error(path, source))?;
    let identity = FileIdentityHandle::from_path(path).map_err(|source| io_error(path, source))?;
    validate_windows_data_directory(identity.as_file()).map_err(|source| io_error(path, source))
}

#[cfg(windows)]
fn windows_sddl_has_protected_dacl(actual: &str) -> bool {
    actual
        .find("D:")
        .and_then(|start| actual[start + 2..].find('(').map(|aces| (start, aces)))
        .is_some_and(|(start, aces)| actual[start + 2..start + 2 + aces].contains('P'))
}

#[cfg(windows)]
fn windows_directory_acl_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "data directory ACL is not restricted to the current account and LocalSystem",
    )
}

fn owner_authority_error() -> NativeDirectoryError {
    NativeDirectoryError::OfflineOwnerAuthorityDenied
}

fn interrupt_promotion(
    requested: Option<PromotionBoundary>,
    current: PromotionBoundary,
) -> Result<(), NativeDirectoryError> {
    if requested == Some(current) {
        Err(NativeDirectoryError::InjectedPromotionCrash(current))
    } else {
        Ok(())
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
    pending: bool,
) -> Result<(), NativeDirectoryError> {
    let format_path = path.join(if pending {
        PENDING_FORMAT_FILE
    } else {
        FORMAT_FILE
    });
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

fn read_and_validate_marker(
    path: &Path,
    allow_pending: bool,
) -> Result<NativeDirectoryIdentity, NativeDirectoryError> {
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
            if !allow_pending {
                return Err(NativeDirectoryError::PendingMigration(path.to_path_buf()));
            }
        }
        (false, false) => {
            return Err(NativeDirectoryError::MissingFormat(path.to_path_buf()));
        }
        (true, false) if allow_pending => {
            return Err(NativeDirectoryError::ConflictingFormatMarkers(
                path.to_path_buf(),
            ));
        }
        (true, false) => {}
    }

    let marker_path = if has_format {
        format_path.clone()
    } else {
        pending_path
    };
    let mut marker = Vec::new();
    File::open(&marker_path)
        .map_err(|source| io_error(&marker_path, source))?
        .take(MAX_FORMAT_READ_BYTES)
        .read_to_end(&mut marker)
        .map_err(|source| io_error(&marker_path, source))?;
    parse_marker(&marker_path, &marker)
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

    #[cfg(unix)]
    #[test]
    fn offline_owner_authority_rejects_a_symlink() -> Result<(), Box<dyn std::error::Error>> {
        use std::{fs, os::unix::fs::symlink};

        let root = std::env::temp_dir().join(format!(
            "hyphae-offline-owner-path-{}-{}",
            std::process::id(),
            super::NEXT_DIRECTORY_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let directory = root.join("data");
        let link = root.join("link");
        fs::create_dir_all(&directory)?;
        symlink(&directory, &link)?;
        assert!(matches!(
            super::validate_offline_owner_path(&link),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn offline_owner_authority_requires_effective_uid_and_stable_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::{fs, os::unix::fs::MetadataExt};

        let root = std::env::temp_dir().join(format!(
            "hyphae-offline-owner-identity-{}-{}",
            std::process::id(),
            super::NEXT_DIRECTORY_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let authority = super::OfflineOwnerAuthority::validate(&root)?;
        assert_eq!(
            fs::symlink_metadata(&root)?.uid(),
            rustix::process::geteuid().as_raw()
        );
        authority.revalidate(&root)?;
        let moved = root.with_extension("moved");
        fs::rename(&root, &moved)?;
        fs::create_dir(&root)?;
        assert!(matches!(
            authority.revalidate(&root),
            Err(NativeDirectoryError::OfflineOwnerAuthorityDenied)
        ));
        fs::remove_dir_all(root)?;
        fs::remove_dir_all(moved)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn offline_owner_authority_rejects_a_named_stream_path() {
        assert!(super::is_windows_named_stream(Path::new(r"C:\\data:owner")));
    }

    #[cfg(windows)]
    #[test]
    fn normal_directory_creation_installs_offline_owner_dacl()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-windows-directory-dacl-{}-{}",
            std::process::id(),
            super::NEXT_DIRECTORY_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ignored = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path)?;
        super::restrict_windows_data_directory(&path)?;
        super::validate_offline_owner_path(&path)?;
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
