// SPDX-License-Identifier: Apache-2.0

//! Immutable digest-chained root manifests and staged checkpoint publication.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use hyphae_native_mvcc::{MvccError, RootSet, RootSlot, WalAnchor};
use hyphae_native_types::{
    CatalogVersion, Csn, EngineKind, Lsn, ManifestGeneration, PageGeneration, PageId,
};
use thiserror::Error;

/// Fixed root-manifest header size.
pub const MANIFEST_HEADER_SIZE: usize = 176;
/// Root-manifest header size when page-retention state is present.
pub const MANIFEST_V2_HEADER_SIZE: usize = 192;
/// Maximum root slots in one manifest.
pub const MAX_MANIFEST_ROOTS: usize = 4_096;

const MAGIC_V1: &[u8; 8] = b"HYROOT01";
const MAGIC_V2: &[u8; 8] = b"HYROOT02";
const FORMAT_VERSION_V1: u16 = 1;
const FORMAT_VERSION_V2: u16 = 2;
const HEADER_SIZE_V1_U16: u16 = 176;
const HEADER_SIZE_V2_U16: u16 = 192;
const ROOT_ENTRY_SIZE: usize = 12;
const CHECKSUM_START: usize = 64;
const CHECKSUM_END: usize = 68;
const MANIFEST_DIGEST_START: usize = 144;
const MANIFEST_DIGEST_END: usize = 176;
const DIRECTORY_NAME: &str = "roots";

/// Root-manifest codec, chain, or publication failure.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// MVCC root reconstruction failed.
    #[error(transparent)]
    Mvcc(#[from] MvccError),
    /// Root set is not committed or has no physical roots.
    #[error("native root manifest requires a committed nonempty root set")]
    UncommittedRootSet,
    /// Manifest length or payload count is invalid.
    #[error("native root manifest length or root count is invalid")]
    InvalidLength,
    /// Magic, version, flags, or reserved bytes are invalid.
    #[error("native root manifest preamble is invalid")]
    InvalidPreamble,
    /// Manifest checksum failed.
    #[error("native root manifest CRC32C mismatch")]
    ChecksumMismatch,
    /// Manifest content digest failed.
    #[error("native root manifest BLAKE3 mismatch")]
    DigestMismatch,
    /// Stable identity is zero or an engine value is unknown.
    #[error("native root manifest contains an invalid identity")]
    InvalidIdentity,
    /// Page generation and retention floor are not a valid storage state.
    #[error("native root manifest contains an invalid page retention state")]
    InvalidStorageState,
    /// Root slots are duplicated or not in canonical order.
    #[error("native root manifest slots are not strictly ordered")]
    NoncanonicalRoots,
    /// Generation or predecessor digest does not continue the chain.
    #[error("native root manifest chain is not contiguous")]
    InvalidChain,
    /// The owned roots directory contains an unexpected material entry.
    #[error("native roots directory contains an unexpected entry")]
    UnexpectedDirectoryEntry,
    /// Temporary or final publication target already exists.
    #[error("native root manifest publication target already exists")]
    PublicationTargetExists,
}

/// One immutable committed root manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootManifest {
    generation: ManifestGeneration,
    visible_csn: Csn,
    catalog_version: CatalogVersion,
    wal_anchor: WalAnchor,
    blob_generation: u64,
    page_generation: PageGeneration,
    retention_floor_csn: Csn,
    roots: BTreeMap<RootSlot, PageId>,
    previous_digest: [u8; 32],
    digest: [u8; 32],
}

impl RootManifest {
    /// Builds a manifest from one committed immutable root set.
    ///
    /// # Errors
    ///
    /// Returns an error when the root set is uncommitted, empty, or exceeds
    /// the manifest root bound.
    pub fn from_root_set(
        generation: ManifestGeneration,
        previous_digest: [u8; 32],
        root_set: &RootSet,
    ) -> Result<Self, ManifestError> {
        let visible_csn = root_set
            .visible_csn()
            .ok_or(ManifestError::UncommittedRootSet)?;
        let wal_anchor = root_set
            .wal_anchor()
            .ok_or(ManifestError::UncommittedRootSet)?;
        let roots: BTreeMap<_, _> = root_set.iter_roots().collect();
        if roots.is_empty() || roots.len() > MAX_MANIFEST_ROOTS {
            return Err(ManifestError::UncommittedRootSet);
        }
        let mut manifest = Self {
            generation,
            visible_csn,
            catalog_version: root_set.catalog_version(),
            wal_anchor,
            blob_generation: root_set.blob_generation(),
            page_generation: root_set.page_generation(),
            retention_floor_csn: root_set
                .retention_floor_csn()
                .ok_or(ManifestError::UncommittedRootSet)?,
            roots,
            previous_digest,
            digest: [0; 32],
        };
        manifest.digest = manifest.compute_digest()?;
        Ok(manifest)
    }

    /// Returns the immutable generation.
    pub const fn generation(&self) -> ManifestGeneration {
        self.generation
    }

    /// Returns the visible committed CSN.
    pub const fn visible_csn(&self) -> Csn {
        self.visible_csn
    }

    /// Returns the catalog version.
    pub const fn catalog_version(&self) -> CatalogVersion {
        self.catalog_version
    }

    /// Returns the committed WAL anchor.
    pub const fn wal_anchor(&self) -> WalAnchor {
        self.wal_anchor
    }

    /// Returns the immutable page-file generation.
    pub const fn page_generation(&self) -> PageGeneration {
        self.page_generation
    }

    /// Returns the earliest CSN whose physical roots remain retained.
    pub const fn retention_floor_csn(&self) -> Csn {
        self.retention_floor_csn
    }

    /// Returns the prior manifest digest, or zero for generation one.
    pub const fn previous_digest(&self) -> [u8; 32] {
        self.previous_digest
    }

    /// Returns this complete manifest digest.
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Returns one physical root.
    pub fn root(&self, slot: RootSlot) -> Option<PageId> {
        self.roots.get(&slot).copied()
    }

    /// Reconstructs the committed MVCC root set.
    ///
    /// # Errors
    ///
    /// Returns an error if the WAL anchor is invalid.
    pub fn to_root_set(&self) -> Result<RootSet, ManifestError> {
        Ok(RootSet::committed_with_storage(
            self.visible_csn,
            self.catalog_version,
            self.wal_anchor,
            self.roots.clone(),
            self.blob_generation,
            self.page_generation,
            self.retention_floor_csn,
        )?)
    }

    /// Encodes one exact manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if lengths exceed canonical fields.
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let mut encoded = self.encode_without_integrity()?;
        let checksum = manifest_checksum(&encoded);
        encoded[CHECKSUM_START..CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
        encoded[MANIFEST_DIGEST_START..MANIFEST_DIGEST_END].copy_from_slice(&self.digest);
        Ok(encoded)
    }

    /// Decodes and verifies one exact manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for any format, identity, order, checksum, or digest
    /// divergence.
    pub fn decode(encoded: &[u8]) -> Result<Self, ManifestError> {
        if encoded.len() < MANIFEST_HEADER_SIZE {
            return Err(ManifestError::InvalidLength);
        }
        let (header_size, page_generation, retention_floor_csn) = match (
            &encoded[0..8],
            read_u16(&encoded[8..10]),
            read_u16(&encoded[10..12]),
        ) {
            (magic, FORMAT_VERSION_V1, HEADER_SIZE_V1_U16) if magic == MAGIC_V1 => {
                (MANIFEST_HEADER_SIZE, PageGeneration::FIRST, Csn::FIRST)
            }
            (magic, FORMAT_VERSION_V2, HEADER_SIZE_V2_U16) if magic == MAGIC_V2 => {
                if encoded.len() < MANIFEST_V2_HEADER_SIZE {
                    return Err(ManifestError::InvalidLength);
                }
                (
                    MANIFEST_V2_HEADER_SIZE,
                    PageGeneration::new(read_u64(&encoded[176..184]))
                        .map_err(|_| ManifestError::InvalidIdentity)?,
                    Csn::new(read_u64(&encoded[184..192]))
                        .map_err(|_| ManifestError::InvalidIdentity)?,
                )
            }
            _ => return Err(ManifestError::InvalidPreamble),
        };
        if encoded[12..16].iter().any(|byte| *byte != 0)
            || encoded[68..80].iter().any(|byte| *byte != 0)
        {
            return Err(ManifestError::InvalidPreamble);
        }
        let root_count = usize::try_from(read_u32(&encoded[56..60]))
            .map_err(|_| ManifestError::InvalidLength)?;
        let payload_length = usize::try_from(read_u32(&encoded[60..64]))
            .map_err(|_| ManifestError::InvalidLength)?;
        if root_count == 0
            || root_count > MAX_MANIFEST_ROOTS
            || payload_length != root_count * ROOT_ENTRY_SIZE
            || encoded.len() != header_size + payload_length
        {
            return Err(ManifestError::InvalidLength);
        }
        if manifest_checksum(encoded) != read_u32(&encoded[CHECKSUM_START..CHECKSUM_END]) {
            return Err(ManifestError::ChecksumMismatch);
        }
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(&encoded[MANIFEST_DIGEST_START..MANIFEST_DIGEST_END]);
        if manifest_digest(encoded) != digest {
            return Err(ManifestError::DigestMismatch);
        }
        let generation = ManifestGeneration::new(read_u64(&encoded[16..24]))
            .map_err(|_| ManifestError::InvalidIdentity)?;
        let visible_csn =
            Csn::new(read_u64(&encoded[24..32])).map_err(|_| ManifestError::InvalidIdentity)?;
        validate_storage_state(page_generation, retention_floor_csn, visible_csn)?;
        let catalog_version = CatalogVersion::new(read_u64(&encoded[32..40]))
            .map_err(|_| ManifestError::InvalidIdentity)?;
        let wal_lsn =
            Lsn::new(read_u64(&encoded[40..48])).map_err(|_| ManifestError::InvalidIdentity)?;
        let blob_generation = read_u64(&encoded[48..56]);
        let mut wal_digest = [0_u8; 32];
        wal_digest.copy_from_slice(&encoded[80..112]);
        let wal_anchor = WalAnchor::new(wal_lsn, wal_digest)?;
        let mut previous_digest = [0_u8; 32];
        previous_digest.copy_from_slice(&encoded[112..144]);
        let roots = decode_roots(&encoded[header_size..], root_count)?;
        Ok(Self {
            generation,
            visible_csn,
            catalog_version,
            wal_anchor,
            blob_generation,
            page_generation,
            retention_floor_csn,
            roots,
            previous_digest,
            digest,
        })
    }

    fn encode_without_integrity(&self) -> Result<Vec<u8>, ManifestError> {
        if self.roots.is_empty() || self.roots.len() > MAX_MANIFEST_ROOTS {
            return Err(ManifestError::InvalidLength);
        }
        let payload_length = self
            .roots
            .len()
            .checked_mul(ROOT_ENTRY_SIZE)
            .ok_or(ManifestError::InvalidLength)?;
        validate_storage_state(
            self.page_generation,
            self.retention_floor_csn,
            self.visible_csn,
        )?;
        let is_v1 =
            self.page_generation == PageGeneration::FIRST && self.retention_floor_csn == Csn::FIRST;
        let header_size = if is_v1 {
            MANIFEST_HEADER_SIZE
        } else {
            MANIFEST_V2_HEADER_SIZE
        };
        let total_length = header_size
            .checked_add(payload_length)
            .ok_or(ManifestError::InvalidLength)?;
        let mut encoded = vec![0_u8; total_length];
        encoded[0..8].copy_from_slice(if is_v1 { MAGIC_V1 } else { MAGIC_V2 });
        encoded[8..10].copy_from_slice(
            &(if is_v1 {
                FORMAT_VERSION_V1
            } else {
                FORMAT_VERSION_V2
            })
            .to_le_bytes(),
        );
        encoded[10..12].copy_from_slice(
            &(if is_v1 {
                HEADER_SIZE_V1_U16
            } else {
                HEADER_SIZE_V2_U16
            })
            .to_le_bytes(),
        );
        encoded[16..24].copy_from_slice(&self.generation.get().to_le_bytes());
        encoded[24..32].copy_from_slice(&self.visible_csn.get().to_le_bytes());
        encoded[32..40].copy_from_slice(&self.catalog_version.get().to_le_bytes());
        encoded[40..48].copy_from_slice(&self.wal_anchor.lsn.get().to_le_bytes());
        encoded[48..56].copy_from_slice(&self.blob_generation.to_le_bytes());
        encoded[56..60].copy_from_slice(
            &u32::try_from(self.roots.len())
                .map_err(|_| ManifestError::InvalidLength)?
                .to_le_bytes(),
        );
        encoded[60..64].copy_from_slice(
            &u32::try_from(payload_length)
                .map_err(|_| ManifestError::InvalidLength)?
                .to_le_bytes(),
        );
        encoded[80..112].copy_from_slice(&self.wal_anchor.digest);
        encoded[112..144].copy_from_slice(&self.previous_digest);
        if !is_v1 {
            encoded[176..184].copy_from_slice(&self.page_generation.get().to_le_bytes());
            encoded[184..192].copy_from_slice(&self.retention_floor_csn.get().to_le_bytes());
        }
        let mut offset = header_size;
        for (slot, page) in &self.roots {
            encoded[offset] = slot.engine as u8;
            encoded[offset + 2..offset + 4].copy_from_slice(&slot.partition.to_le_bytes());
            encoded[offset + 4..offset + 12].copy_from_slice(&page.get().to_le_bytes());
            offset += ROOT_ENTRY_SIZE;
        }
        Ok(encoded)
    }

    fn compute_digest(&self) -> Result<[u8; 32], ManifestError> {
        let mut encoded = self.encode_without_integrity()?;
        let checksum = manifest_checksum(&encoded);
        encoded[CHECKSUM_START..CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
        Ok(manifest_digest(&encoded))
    }
}

fn validate_storage_state(
    page_generation: PageGeneration,
    retention_floor_csn: Csn,
    visible_csn: Csn,
) -> Result<(), ManifestError> {
    let first_generation = page_generation == PageGeneration::FIRST;
    let first_floor = retention_floor_csn == Csn::FIRST;
    if retention_floor_csn > visible_csn || first_generation != first_floor {
        return Err(ManifestError::InvalidStorageState);
    }
    Ok(())
}

fn decode_roots(
    payload: &[u8],
    root_count: usize,
) -> Result<BTreeMap<RootSlot, PageId>, ManifestError> {
    let mut roots = BTreeMap::new();
    let mut prior = None;
    for index in 0..root_count {
        let offset = index * ROOT_ENTRY_SIZE;
        if payload[offset + 1] != 0 {
            return Err(ManifestError::InvalidPreamble);
        }
        let engine = decode_engine(payload[offset])?;
        let partition = read_u16(&payload[offset + 2..offset + 4]);
        let page = PageId::new(read_u64(&payload[offset + 4..offset + 12]))
            .map_err(|_| ManifestError::InvalidIdentity)?;
        let slot = RootSlot { engine, partition };
        if prior.is_some_and(|previous| previous >= slot) || roots.insert(slot, page).is_some() {
            return Err(ManifestError::NoncanonicalRoots);
        }
        prior = Some(slot);
    }
    Ok(roots)
}

fn decode_engine(value: u8) -> Result<EngineKind, ManifestError> {
    match value {
        0 => Ok(EngineKind::Kernel),
        1 => Ok(EngineKind::Relational),
        2 => Ok(EngineKind::Structure),
        3 => Ok(EngineKind::Search),
        _ => Err(ManifestError::InvalidIdentity),
    }
}

fn manifest_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[CHECKSUM_START..CHECKSUM_END].fill(0);
    canonical[MANIFEST_DIGEST_START..MANIFEST_DIGEST_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn manifest_digest(encoded: &[u8]) -> [u8; 32] {
    let mut canonical = encoded.to_vec();
    canonical[MANIFEST_DIGEST_START..MANIFEST_DIGEST_END].fill(0);
    *blake3::hash(&canonical).as_bytes()
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

/// One staged manifest not yet visible under its final immutable filename.
#[derive(Debug)]
pub struct StagedManifest {
    manifest: RootManifest,
    temporary_path: PathBuf,
    final_path: PathBuf,
}

impl StagedManifest {
    /// Returns the staged immutable manifest.
    pub const fn manifest(&self) -> &RootManifest {
        &self.manifest
    }
}

/// Verified manifest-chain recovery report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestRecovery {
    /// Complete verified manifests in generation order.
    pub manifests: Vec<RootManifest>,
    /// Recovered and removed create-new temporary files.
    pub ignored_temporary_files: usize,
    /// Whether strict parent-directory synchronization is supported here.
    pub parent_sync_supported: bool,
}

/// Root-manifest directory and current verified chain.
#[derive(Debug)]
pub struct RootManifestStore {
    directory: PathBuf,
    manifests: Vec<RootManifest>,
    ignored_temporary_files: usize,
}

impl RootManifestStore {
    /// Creates the owned empty roots directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory exists or cannot be created.
    pub fn create(data_directory: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let directory = data_directory.as_ref().join(DIRECTORY_NAME);
        fs::create_dir(&directory)?;
        sync_directory(data_directory.as_ref())?;
        Ok(Self {
            directory,
            manifests: Vec::new(),
            ignored_temporary_files: 0,
        })
    }

    /// Opens and verifies the complete immutable manifest chain.
    ///
    /// Incomplete canonical `.tmp` stages are counted and removed after the
    /// complete manifest chain verifies. Every other material entry must be a
    /// canonical manifest filename.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, unexpected entries, corrupt manifests, or a
    /// noncontiguous generation/digest chain.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let directory = data_directory.as_ref().join(DIRECTORY_NAME);
        let mut manifest_paths = Vec::new();
        let mut temporary_paths = Vec::new();
        let mut ignored_temporary_files = 0;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(ManifestError::UnexpectedDirectoryEntry);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ManifestError::UnexpectedDirectoryEntry)?;
            if parse_temporary_filename(&name).is_some() {
                ignored_temporary_files += 1;
                temporary_paths.push(entry.path());
            } else if parse_generation_filename(&name).is_some() {
                manifest_paths.push((name, entry.path()));
            } else {
                return Err(ManifestError::UnexpectedDirectoryEntry);
            }
        }
        manifest_paths.sort_by(|left, right| left.0.cmp(&right.0));
        let mut manifests = Vec::with_capacity(manifest_paths.len());
        let mut expected_generation = 1_u64;
        let mut previous_digest = [0_u8; 32];
        for (name, path) in manifest_paths {
            let generation =
                parse_generation_filename(&name).ok_or(ManifestError::UnexpectedDirectoryEntry)?;
            if generation != expected_generation {
                return Err(ManifestError::InvalidChain);
            }
            let encoded = fs::read(path)?;
            let manifest = RootManifest::decode(&encoded)?;
            if manifest.generation.get() != generation
                || manifest.previous_digest != previous_digest
            {
                return Err(ManifestError::InvalidChain);
            }
            previous_digest = manifest.digest;
            manifests.push(manifest);
            expected_generation = expected_generation
                .checked_add(1)
                .ok_or(ManifestError::InvalidChain)?;
        }
        for temporary_path in temporary_paths {
            fs::remove_file(temporary_path)?;
        }
        if ignored_temporary_files != 0 {
            sync_directory(&directory)?;
        }
        Ok(Self {
            directory,
            manifests,
            ignored_temporary_files,
        })
    }

    /// Returns current recovery evidence.
    pub fn recovery(&self) -> ManifestRecovery {
        ManifestRecovery {
            manifests: self.manifests.clone(),
            ignored_temporary_files: self.ignored_temporary_files,
            parent_sync_supported: parent_sync_supported(),
        }
    }

    /// Returns the latest verified manifest.
    pub fn current(&self) -> Option<&RootManifest> {
        self.manifests.last()
    }

    /// Stages one create-new manifest under a non-visible temporary name.
    ///
    /// # Errors
    ///
    /// Returns an error for a chain mismatch, existing target, or uncertain
    /// write/synchronization.
    pub fn stage(
        &self,
        manifest: RootManifest,
        synchronize: bool,
    ) -> Result<StagedManifest, ManifestError> {
        self.validate_next(&manifest)?;
        let stem = generation_stem(manifest.generation);
        let temporary_path = self.directory.join(format!("{stem}.tmp"));
        let final_path = self.directory.join(format!("{stem}.hyroot"));
        if temporary_path.exists() || final_path.exists() {
            return Err(ManifestError::PublicationTargetExists);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(&manifest.encode()?)?;
        if synchronize {
            file.sync_all()?;
        }
        drop(file);
        Ok(StagedManifest {
            manifest,
            temporary_path,
            final_path,
        })
    }

    /// Renames one staged manifest into its immutable visible generation.
    ///
    /// # Errors
    ///
    /// Returns an error for chain races, target collision, rename, or
    /// directory synchronization failure.
    pub fn publish(
        &mut self,
        staged: StagedManifest,
        synchronize: bool,
    ) -> Result<RootManifest, ManifestError> {
        self.validate_next(&staged.manifest)?;
        if staged.final_path.exists() {
            return Err(ManifestError::PublicationTargetExists);
        }
        fs::rename(&staged.temporary_path, &staged.final_path)?;
        if synchronize {
            sync_directory(&self.directory)?;
        }
        self.manifests.push(staged.manifest.clone());
        Ok(staged.manifest)
    }

    /// Stages and publishes one manifest.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::stage`] and [`Self::publish`].
    pub fn append(
        &mut self,
        manifest: RootManifest,
        synchronize: bool,
    ) -> Result<RootManifest, ManifestError> {
        let staged = self.stage(manifest, synchronize)?;
        self.publish(staged, synchronize)
    }

    fn validate_next(&self, manifest: &RootManifest) -> Result<(), ManifestError> {
        let (generation, digest) = if let Some(current) = self.current() {
            (
                current
                    .generation
                    .get()
                    .checked_add(1)
                    .ok_or(ManifestError::InvalidChain)?,
                current.digest,
            )
        } else {
            (1_u64, [0; 32])
        };
        if manifest.generation.get() != generation || manifest.previous_digest != digest {
            return Err(ManifestError::InvalidChain);
        }
        Ok(())
    }
}

fn generation_stem(generation: ManifestGeneration) -> String {
    format!("manifest-{:016x}", generation.get())
}

fn parse_generation_filename(name: &str) -> Option<u64> {
    let value = name.strip_prefix("manifest-")?.strip_suffix(".hyroot")?;
    parse_generation_hex(value)
}

fn parse_temporary_filename(name: &str) -> Option<u64> {
    let value = name.strip_prefix("manifest-")?.strip_suffix(".tmp")?;
    parse_generation_hex(value)
}

fn parse_generation_hex(value: &str) -> Option<u64> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    u64::from_str_radix(value, 16)
        .ok()
        .filter(|value| *value != 0)
}

/// Returns whether strict parent-directory synchronization is implemented.
pub const fn parent_sync_supported() -> bool {
    cfg!(unix)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> Result<(), std::io::Error> {
    fs::metadata(directory).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_mvcc::{RootSet, RootSlot, WalAnchor};
    use hyphae_native_types::{
        CatalogVersion, Csn, EngineKind, Lsn, ManifestGeneration, PageGeneration, PageId,
    };

    use super::{ManifestError, RootManifest, RootManifestStore};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-manifest-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(self.path());
        }
    }

    fn root_set(csn: u64, marker: u8) -> Result<RootSet, Box<dyn std::error::Error>> {
        root_set_with_storage(csn, marker, PageGeneration::FIRST, Csn::FIRST)
    }

    fn root_set_with_storage(
        csn: u64,
        marker: u8,
        page_generation: PageGeneration,
        retention_floor_csn: Csn,
    ) -> Result<RootSet, Box<dyn std::error::Error>> {
        let roots = [
            (EngineKind::Kernel, 1_u64),
            (EngineKind::Relational, 2),
            (EngineKind::Structure, 3),
            (EngineKind::Search, 4),
        ]
        .into_iter()
        .map(|(engine, page)| {
            Ok((
                RootSlot {
                    engine,
                    partition: 0,
                },
                PageId::new(page)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, hyphae_native_types::NativeTypeError>>()?;
        Ok(RootSet::committed_with_storage(
            Csn::new(csn)?,
            CatalogVersion::new(2)?,
            WalAnchor::new(Lsn::new(112 + (csn - 1) * 65_536)?, [marker; 32])?,
            roots,
            0,
            page_generation,
            retention_floor_csn,
        )?)
    }

    #[test]
    fn manifest_codec_and_root_reconstruction_are_stable() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = root_set(1, 0xa5)?;
        let manifest = RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root)?;
        let encoded = manifest.encode()?;
        assert_eq!(RootManifest::decode(&encoded)?, manifest);
        assert_eq!(manifest.to_root_set()?, root);
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "b6211d9d373a4d01f5768126895aaaa4281bbba128929992259f9da7a4df047a"
        );
        Ok(())
    }

    #[test]
    fn vacuum_manifest_round_trips_v2_and_continues_a_v1_chain()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = RootManifestStore::create(temporary.path())?;
        let first =
            RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root_set(1, 1)?)?;
        store.append(first.clone(), true)?;

        let vacuum_roots = root_set_with_storage(2, 2, PageGeneration::new(2)?, Csn::new(2)?)?;
        let vacuum = RootManifest::from_root_set(
            ManifestGeneration::new(2)?,
            first.digest(),
            &vacuum_roots,
        )?;
        let encoded = vacuum.encode()?;
        assert_eq!(encoded.len(), 240);
        assert_eq!(&encoded[..8], b"HYROOT02");
        assert_eq!(&encoded[176..184], &2_u64.to_le_bytes());
        assert_eq!(&encoded[184..192], &2_u64.to_le_bytes());
        assert_eq!(RootManifest::decode(&encoded)?, vacuum);
        assert_eq!(vacuum.to_root_set()?, vacuum_roots);
        store.append(vacuum.clone(), true)?;
        drop(store);

        let reopened = RootManifestStore::open(temporary.path())?;
        assert_eq!(reopened.recovery().manifests, vec![first, vacuum.clone()]);
        assert_eq!(reopened.current(), Some(&vacuum));
        Ok(())
    }

    #[test]
    fn malformed_v2_storage_state_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let roots = root_set_with_storage(2, 2, PageGeneration::new(2)?, Csn::new(2)?)?;
        let manifest = RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &roots)?;
        let mut encoded = manifest.encode()?;
        encoded.truncate(191);
        assert!(matches!(
            RootManifest::decode(&encoded),
            Err(ManifestError::InvalidLength)
        ));

        let invalid_roots = root_set_with_storage(2, 2, PageGeneration::FIRST, Csn::new(2)?)?;
        assert!(matches!(
            RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &invalid_roots),
            Err(ManifestError::InvalidStorageState)
        ));
        Ok(())
    }

    #[test]
    fn staged_manifest_is_ignored_until_atomic_publish() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let store = RootManifestStore::create(temporary.path())?;
        let root = root_set(1, 1)?;
        let manifest = RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root)?;
        let staged = store.stage(manifest, true)?;
        assert_eq!(staged.manifest().visible_csn().get(), 1);
        drop(staged);
        drop(store);

        let reopened = RootManifestStore::open(temporary.path())?;
        assert!(reopened.current().is_none());
        assert_eq!(reopened.recovery().ignored_temporary_files, 1);
        let replacement = RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root)?;
        let _replacement = reopened.stage(replacement, false)?;
        Ok(())
    }

    #[test]
    fn published_chain_reopens_and_rejects_generation_gaps()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = RootManifestStore::create(temporary.path())?;
        let first =
            RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root_set(1, 1)?)?;
        store.append(first.clone(), true)?;
        let second = RootManifest::from_root_set(
            ManifestGeneration::new(2)?,
            first.digest(),
            &root_set(2, 2)?,
        )?;
        store.append(second.clone(), true)?;
        drop(store);

        let reopened = RootManifestStore::open(temporary.path())?;
        assert_eq!(reopened.current(), Some(&second));

        let invalid = RootManifest::from_root_set(
            ManifestGeneration::new(4)?,
            second.digest(),
            &root_set(3, 3)?,
        )?;
        assert!(matches!(
            reopened.stage(invalid, false),
            Err(ManifestError::InvalidChain)
        ));
        Ok(())
    }

    #[test]
    fn complete_corrupt_manifest_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = RootManifestStore::create(temporary.path())?;
        let manifest =
            RootManifest::from_root_set(ManifestGeneration::new(1)?, [0; 32], &root_set(1, 1)?)?;
        store.append(manifest, true)?;
        drop(store);

        let path = temporary
            .path()
            .join("roots")
            .join("manifest-0000000000000001.hyroot");
        let mut encoded = fs::read(&path)?;
        encoded[200] ^= 1;
        fs::write(path, encoded)?;
        assert!(matches!(
            RootManifestStore::open(temporary.path()),
            Err(ManifestError::ChecksumMismatch)
        ));
        Ok(())
    }
}
