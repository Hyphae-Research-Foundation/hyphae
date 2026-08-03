// SPDX-License-Identifier: Apache-2.0

//! Durable identities and immutable records for restartable native snapshots.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    num::NonZeroU128,
    path::{Path, PathBuf},
};

use hyphae_native_manifest::RootManifest;
use hyphae_native_mvcc::RootSet;
use hyphae_native_types::{Csn, LineageIdentity, Lsn, ManifestGeneration, PageGeneration};
use thiserror::Error;

const DIRECTORY_NAME: &str = "pins";
const MAGIC: &[u8; 8] = b"HYPIN001";
const FORMAT_VERSION: u16 = 1;
const RECORD_SIZE: usize = 240;
const RECORD_SIZE_U16: u16 = 240;
const CHECKSUM_START: usize = 208;
const CHECKSUM_END: usize = 240;

/// Stable identity for one durable native snapshot pin.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotPinId(NonZeroU128);

impl SnapshotPinId {
    /// Constructs one nonzero snapshot-pin identity.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero.
    pub fn new(value: u128) -> Result<Self, SnapshotPinError> {
        NonZeroU128::new(value)
            .map(Self)
            .ok_or(SnapshotPinError::InvalidIdentity)
    }

    /// Returns the underlying nonzero integer.
    pub const fn get(self) -> u128 {
        self.0.get()
    }

    fn canonical_text(self) -> String {
        format!("{:032x}", self.get())
    }
}

impl fmt::Display for SnapshotPinId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.get())
    }
}

/// Snapshot-pin codec, namespace, or publication failure.
#[derive(Debug, Error)]
pub enum SnapshotPinError {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A required identity, CSN, generation, LSN, or digest is invalid.
    #[error("native snapshot pin contains an invalid identity")]
    InvalidIdentity,
    /// The record is not exactly the canonical fixed width.
    #[error("native snapshot pin length is invalid")]
    InvalidLength,
    /// Magic, version, size, flags, or reserved bytes are invalid.
    #[error("native snapshot pin preamble is invalid")]
    InvalidPreamble,
    /// The record checksum does not match its content.
    #[error("native snapshot pin BLAKE3 checksum mismatch")]
    ChecksumMismatch,
    /// The payload identity does not match the canonical filename.
    #[error("native snapshot pin filename and payload identity differ")]
    FilenameIdentityMismatch,
    /// The pin belongs to another data-directory history.
    #[error("native snapshot pin lineage does not match the data directory")]
    LineageMismatch,
    /// The pins namespace contains a noncanonical material entry.
    #[error("native snapshot pin directory contains an unexpected entry")]
    UnexpectedDirectoryEntry,
    /// A stable or temporary publication target already exists.
    #[error("native snapshot pin publication target already exists")]
    PublicationTargetExists,
}

/// One immutable durable snapshot retention claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotPin {
    id: SnapshotPinId,
    lineage: LineageIdentity,
    visible_csn: Csn,
    logical_time_micros: i64,
    manifest_generation: ManifestGeneration,
    manifest_digest: [u8; 32],
    root_digest: [u8; 32],
    page_generation: PageGeneration,
    blob_generation: u64,
    retention_floor_csn: Csn,
    wal_lsn: Lsn,
    wal_digest: [u8; 32],
}

impl SnapshotPin {
    pub(crate) fn from_manifest(
        id: SnapshotPinId,
        logical_time_micros: i64,
        lineage: LineageIdentity,
        manifest: &RootManifest,
        roots: &RootSet,
    ) -> Result<Self, SnapshotPinError> {
        let visible_csn = roots
            .visible_csn()
            .ok_or(SnapshotPinError::InvalidIdentity)?;
        let retention_floor_csn = roots
            .retention_floor_csn()
            .ok_or(SnapshotPinError::InvalidIdentity)?;
        let wal_anchor = roots
            .wal_anchor()
            .ok_or(SnapshotPinError::InvalidIdentity)?;
        let root_digest = roots.digest();
        if manifest.lineage() != Some(lineage)
            || manifest.visible_csn() != visible_csn
            || manifest.page_generation() != roots.page_generation()
            || manifest.retention_floor_csn() != retention_floor_csn
            || manifest.wal_anchor() != wal_anchor
            || manifest.digest() == [0; 32]
            || root_digest == [0; 32]
            || wal_anchor.digest == [0; 32]
        {
            return Err(SnapshotPinError::InvalidIdentity);
        }
        Ok(Self {
            id,
            lineage,
            visible_csn,
            logical_time_micros,
            manifest_generation: manifest.generation(),
            manifest_digest: manifest.digest(),
            root_digest,
            page_generation: roots.page_generation(),
            blob_generation: roots.blob_generation(),
            retention_floor_csn,
            wal_lsn: wal_anchor.lsn,
            wal_digest: wal_anchor.digest,
        })
    }

    pub(crate) const fn id(&self) -> SnapshotPinId {
        self.id
    }

    pub(crate) const fn visible_csn(&self) -> Csn {
        self.visible_csn
    }

    pub(crate) const fn logical_time_micros(&self) -> i64 {
        self.logical_time_micros
    }

    pub(crate) const fn manifest_generation(&self) -> ManifestGeneration {
        self.manifest_generation
    }

    pub(crate) const fn manifest_digest(&self) -> [u8; 32] {
        self.manifest_digest
    }

    pub(crate) const fn root_digest(&self) -> [u8; 32] {
        self.root_digest
    }

    pub(crate) const fn page_generation(&self) -> PageGeneration {
        self.page_generation
    }

    pub(crate) const fn blob_generation(&self) -> u64 {
        self.blob_generation
    }

    pub(crate) const fn wal_lsn(&self) -> Lsn {
        self.wal_lsn
    }

    pub(crate) const fn wal_digest(&self) -> [u8; 32] {
        self.wal_digest
    }

    pub(crate) fn matches_manifest(&self, manifest: &RootManifest) -> bool {
        let Ok(roots) = manifest.to_root_set() else {
            return false;
        };
        manifest.generation() == self.manifest_generation
            && manifest.digest() == self.manifest_digest
            && manifest.lineage() == Some(self.lineage)
            && manifest.visible_csn() == self.visible_csn
            && manifest.page_generation() == self.page_generation
            && manifest.retention_floor_csn() == self.retention_floor_csn
            && manifest.wal_anchor().lsn == self.wal_lsn
            && manifest.wal_anchor().digest == self.wal_digest
            && roots.digest() == self.root_digest
            && roots.blob_generation() == self.blob_generation
    }

    fn encode(&self) -> [u8; RECORD_SIZE] {
        let mut encoded = [0_u8; RECORD_SIZE];
        encoded[0..8].copy_from_slice(MAGIC);
        encoded[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        encoded[10..12].copy_from_slice(&RECORD_SIZE_U16.to_le_bytes());
        encoded[16..40].copy_from_slice(&self.lineage.encode());
        encoded[40..56].copy_from_slice(&self.id.get().to_be_bytes());
        encoded[56..64].copy_from_slice(&self.visible_csn.get().to_le_bytes());
        encoded[64..72].copy_from_slice(&self.logical_time_micros.to_le_bytes());
        encoded[72..80].copy_from_slice(&self.manifest_generation.get().to_le_bytes());
        encoded[80..112].copy_from_slice(&self.manifest_digest);
        encoded[112..144].copy_from_slice(&self.root_digest);
        encoded[144..152].copy_from_slice(&self.page_generation.get().to_le_bytes());
        encoded[152..160].copy_from_slice(&self.blob_generation.to_le_bytes());
        encoded[160..168].copy_from_slice(&self.retention_floor_csn.get().to_le_bytes());
        encoded[168..176].copy_from_slice(&self.wal_lsn.get().to_le_bytes());
        encoded[176..208].copy_from_slice(&self.wal_digest);
        let checksum = blake3::hash(&encoded[..CHECKSUM_START]);
        encoded[CHECKSUM_START..CHECKSUM_END].copy_from_slice(checksum.as_bytes());
        encoded
    }

    fn decode(encoded: &[u8]) -> Result<Self, SnapshotPinError> {
        if encoded.len() != RECORD_SIZE {
            return Err(SnapshotPinError::InvalidLength);
        }
        if encoded.get(0..8) != Some(MAGIC.as_slice())
            || read_u16(&encoded[8..10]) != FORMAT_VERSION
            || read_u16(&encoded[10..12]) != RECORD_SIZE_U16
            || encoded[12..16].iter().any(|byte| *byte != 0)
        {
            return Err(SnapshotPinError::InvalidPreamble);
        }
        let expected_checksum = blake3::hash(&encoded[..CHECKSUM_START]);
        if encoded[CHECKSUM_START..CHECKSUM_END] != expected_checksum.as_bytes()[..] {
            return Err(SnapshotPinError::ChecksumMismatch);
        }
        let lineage = LineageIdentity::decode(&encoded[16..40])
            .map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let id = SnapshotPinId::new(read_u128_be(&encoded[40..56]))?;
        let visible_csn =
            Csn::new(read_u64(&encoded[56..64])).map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let logical_time_micros = read_i64(&encoded[64..72]);
        let manifest_generation = ManifestGeneration::new(read_u64(&encoded[72..80]))
            .map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let manifest_digest = read_digest(&encoded[80..112])?;
        let root_digest = read_digest(&encoded[112..144])?;
        let page_generation = PageGeneration::new(read_u64(&encoded[144..152]))
            .map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let blob_generation = read_u64(&encoded[152..160]);
        let retention_floor_csn = Csn::new(read_u64(&encoded[160..168]))
            .map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let wal_lsn = Lsn::new(read_u64(&encoded[168..176]))
            .map_err(|_| SnapshotPinError::InvalidIdentity)?;
        let wal_digest = read_digest(&encoded[176..208])?;
        if retention_floor_csn > visible_csn {
            return Err(SnapshotPinError::InvalidIdentity);
        }
        Ok(Self {
            id,
            lineage,
            visible_csn,
            logical_time_micros,
            manifest_generation,
            manifest_digest,
            root_digest,
            page_generation,
            blob_generation,
            retention_floor_csn,
            wal_lsn,
            wal_digest,
        })
    }
}

#[derive(Debug)]
pub(crate) struct StagedSnapshotPin {
    pin: SnapshotPin,
    temporary_path: PathBuf,
    final_path: PathBuf,
}

/// Verified durable snapshot-pin namespace.
#[derive(Debug)]
pub(crate) struct SnapshotPinStore {
    directory: PathBuf,
    pins: BTreeMap<SnapshotPinId, SnapshotPin>,
    temporary_paths: Vec<PathBuf>,
}

impl SnapshotPinStore {
    pub(crate) fn create(data_directory: &Path) -> Result<Self, SnapshotPinError> {
        let directory = data_directory.join(DIRECTORY_NAME);
        fs::create_dir(&directory)?;
        sync_directory(data_directory)?;
        Ok(Self {
            directory,
            pins: BTreeMap::new(),
            temporary_paths: Vec::new(),
        })
    }

    pub(crate) fn open_or_create(
        data_directory: &Path,
        expected_lineage: LineageIdentity,
    ) -> Result<Self, SnapshotPinError> {
        let directory = data_directory.join(DIRECTORY_NAME);
        if !directory.exists() {
            fs::create_dir(&directory)?;
            sync_directory(data_directory)?;
        }
        if !directory.is_dir() {
            return Err(SnapshotPinError::UnexpectedDirectoryEntry);
        }
        let mut pins = BTreeMap::new();
        let mut temporary_paths = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(SnapshotPinError::UnexpectedDirectoryEntry);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| SnapshotPinError::UnexpectedDirectoryEntry)?;
            if parse_temporary_filename(&name).is_some() {
                temporary_paths.push(entry.path());
                continue;
            }
            let filename_id =
                parse_stable_filename(&name).ok_or(SnapshotPinError::UnexpectedDirectoryEntry)?;
            let encoded = fs::read(entry.path())?;
            let pin = SnapshotPin::decode(&encoded)?;
            if pin.id != filename_id {
                return Err(SnapshotPinError::FilenameIdentityMismatch);
            }
            if pin.lineage != expected_lineage {
                return Err(SnapshotPinError::LineageMismatch);
            }
            if pins.insert(pin.id, pin).is_some() {
                return Err(SnapshotPinError::UnexpectedDirectoryEntry);
            }
        }
        temporary_paths.sort();
        Ok(Self {
            directory,
            pins,
            temporary_paths,
        })
    }

    pub(crate) fn get(&self, id: SnapshotPinId) -> Option<&SnapshotPin> {
        self.pins.get(&id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &SnapshotPin> {
        self.pins.values()
    }

    pub(crate) fn len(&self) -> usize {
        self.pins.len()
    }

    pub(crate) fn pinned_generations(&self) -> BTreeSet<PageGeneration> {
        self.pins
            .values()
            .map(SnapshotPin::page_generation)
            .collect()
    }

    pub(crate) fn references_generation(&self, generation: PageGeneration) -> bool {
        self.pins
            .values()
            .any(|pin| pin.page_generation == generation)
    }

    pub(crate) fn stage(
        &self,
        pin: SnapshotPin,
        synchronize: bool,
    ) -> Result<StagedSnapshotPin, SnapshotPinError> {
        let stem = pin_stem(pin.id);
        let temporary_path = self.directory.join(format!("{stem}.hypin.tmp"));
        let final_path = self.directory.join(format!("{stem}.hypin"));
        if self.pins.contains_key(&pin.id) || temporary_path.exists() || final_path.exists() {
            return Err(SnapshotPinError::PublicationTargetExists);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(&pin.encode())?;
        if synchronize {
            file.sync_all()?;
        }
        drop(file);
        Ok(StagedSnapshotPin {
            pin,
            temporary_path,
            final_path,
        })
    }

    pub(crate) fn publish(
        &mut self,
        staged: StagedSnapshotPin,
        synchronize: bool,
    ) -> Result<SnapshotPin, SnapshotPinError> {
        if self.pins.contains_key(&staged.pin.id) || staged.final_path.exists() {
            return Err(SnapshotPinError::PublicationTargetExists);
        }
        fs::rename(&staged.temporary_path, &staged.final_path)?;
        if synchronize {
            sync_directory(&self.directory)?;
        }
        let pin = staged.pin;
        self.pins.insert(pin.id, pin.clone());
        Ok(pin)
    }

    pub(crate) fn remove(
        &mut self,
        id: SnapshotPinId,
        synchronize: bool,
    ) -> Result<Option<SnapshotPin>, SnapshotPinError> {
        let Some(pin) = self.pins.get(&id).cloned() else {
            return Ok(None);
        };
        fs::remove_file(self.directory.join(format!("{}.hypin", pin_stem(id))))?;
        if synchronize {
            sync_directory(&self.directory)?;
        }
        self.pins.remove(&id);
        Ok(Some(pin))
    }

    pub(crate) fn cleanup_temporaries(&mut self) -> Result<usize, SnapshotPinError> {
        for path in &self.temporary_paths {
            fs::remove_file(path)?;
        }
        let removed = self.temporary_paths.len();
        if removed != 0 {
            sync_directory(&self.directory)?;
        }
        self.temporary_paths.clear();
        Ok(removed)
    }
}

fn pin_stem(id: SnapshotPinId) -> String {
    format!("pin-{}", id.canonical_text())
}

fn parse_stable_filename(name: &str) -> Option<SnapshotPinId> {
    parse_filename(name, ".hypin")
}

fn parse_temporary_filename(name: &str) -> Option<SnapshotPinId> {
    parse_filename(name, ".hypin.tmp")
}

fn parse_filename(name: &str, suffix: &str) -> Option<SnapshotPinId> {
    let value = name.strip_prefix("pin-")?.strip_suffix(suffix)?;
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    u128::from_str_radix(value, 16)
        .ok()
        .and_then(|value| SnapshotPinId::new(value).ok())
}

fn read_u16(encoded: &[u8]) -> u16 {
    u16::from_le_bytes(encoded.try_into().unwrap_or([0; 2]))
}

fn read_u64(encoded: &[u8]) -> u64 {
    u64::from_le_bytes(encoded.try_into().unwrap_or([0; 8]))
}

fn read_i64(encoded: &[u8]) -> i64 {
    i64::from_le_bytes(encoded.try_into().unwrap_or([0; 8]))
}

fn read_u128_be(encoded: &[u8]) -> u128 {
    u128::from_be_bytes(encoded.try_into().unwrap_or([0; 16]))
}

fn read_digest(encoded: &[u8]) -> Result<[u8; 32], SnapshotPinError> {
    let digest: [u8; 32] = encoded
        .try_into()
        .map_err(|_| SnapshotPinError::InvalidLength)?;
    if digest == [0; 32] {
        return Err(SnapshotPinError::InvalidIdentity);
    }
    Ok(digest)
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

    use hyphae_native_manifest::RootManifest;
    use hyphae_native_mvcc::{RootSet, RootSlot, WalAnchor};
    use hyphae_native_types::{
        CatalogVersion, Csn, DirectoryUuid, EngineKind, HistoryEpoch, LineageIdentity, Lsn,
        ManifestGeneration, PageGeneration, PageId,
    };

    use super::{
        CHECKSUM_END, CHECKSUM_START, MAGIC, SnapshotPin, SnapshotPinError, SnapshotPinId,
        SnapshotPinStore, parse_stable_filename, parse_temporary_filename,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-snapshot-pins-{}-{sequence}",
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

    fn lineage(marker: u8) -> Result<LineageIdentity, Box<dyn std::error::Error>> {
        let mut uuid = [marker; 16];
        uuid[6] = (uuid[6] & 0x0f) | 0x70;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        Ok(LineageIdentity::new(
            DirectoryUuid::new(uuid)?,
            HistoryEpoch::FIRST,
        ))
    }

    fn root_set(marker: u8) -> Result<RootSet, Box<dyn std::error::Error>> {
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
            Csn::new(7)?,
            CatalogVersion::new(3)?,
            WalAnchor::new(Lsn::new(131_184)?, [marker; 32])?,
            roots,
            0,
            PageGeneration::new(2)?,
            Csn::new(5)?,
        )?)
    }

    fn pin(marker: u8) -> Result<SnapshotPin, Box<dyn std::error::Error>> {
        let lineage = lineage(marker)?;
        let roots = root_set(marker)?;
        let manifest = RootManifest::from_root_set_with_lineage(
            ManifestGeneration::new(9)?,
            [marker.wrapping_add(1); 32],
            &roots,
            lineage,
        )?;
        Ok(SnapshotPin::from_manifest(
            SnapshotPinId::new(u128::from(marker))?,
            -42,
            lineage,
            &manifest,
            &roots,
        )?)
    }

    #[test]
    fn pin_identity_and_filenames_are_canonical() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            SnapshotPinId::new(0),
            Err(SnapshotPinError::InvalidIdentity)
        ));
        let id = SnapshotPinId::new(0xabc)?;
        assert_eq!(id.to_string(), "00000000000000000000000000000abc");
        assert_eq!(
            parse_stable_filename("pin-00000000000000000000000000000abc.hypin"),
            Some(id)
        );
        assert_eq!(
            parse_temporary_filename("pin-00000000000000000000000000000abc.hypin.tmp"),
            Some(id)
        );
        assert_eq!(
            parse_stable_filename("pin-00000000000000000000000000000ABC.hypin"),
            None
        );
        assert_eq!(parse_stable_filename("pin-0abc.hypin"), None);
        Ok(())
    }

    #[test]
    fn pin_record_round_trips_and_rejects_noncanonical_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let pin = pin(7)?;
        let encoded = pin.encode();
        assert_eq!(&encoded[..8], MAGIC);
        assert_eq!(encoded.len(), 240);
        assert_eq!(SnapshotPin::decode(&encoded)?, pin);

        let mut extended = encoded.to_vec();
        extended.push(0);
        assert!(matches!(
            SnapshotPin::decode(&extended),
            Err(SnapshotPinError::InvalidLength)
        ));

        let mut reserved = encoded;
        reserved[12] = 1;
        assert!(matches!(
            SnapshotPin::decode(&reserved),
            Err(SnapshotPinError::InvalidPreamble)
        ));

        let mut checksum = encoded;
        checksum[CHECKSUM_END - 1] ^= 1;
        assert!(matches!(
            SnapshotPin::decode(&checksum),
            Err(SnapshotPinError::ChecksumMismatch)
        ));

        let mut zero_digest = encoded;
        zero_digest[112..144].fill(0);
        let checksum = blake3::hash(&zero_digest[..CHECKSUM_START]);
        zero_digest[CHECKSUM_START..CHECKSUM_END].copy_from_slice(checksum.as_bytes());
        assert!(matches!(
            SnapshotPin::decode(&zero_digest),
            Err(SnapshotPinError::InvalidIdentity)
        ));
        Ok(())
    }

    #[test]
    fn pin_store_ignores_stages_publishes_once_and_enforces_lineage()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let pin = pin(9)?;
        let store = SnapshotPinStore::create(temporary.path())?;
        let _stage = store.stage(pin.clone(), true)?;
        drop(store);

        let mut reopened = SnapshotPinStore::open_or_create(temporary.path(), lineage(9)?)?;
        assert_eq!(reopened.len(), 0);
        assert_eq!(reopened.cleanup_temporaries()?, 1);
        assert_eq!(reopened.cleanup_temporaries()?, 0);

        let stage = reopened.stage(pin.clone(), true)?;
        assert_eq!(reopened.publish(stage, true)?, pin);
        assert!(matches!(
            reopened.stage(pin.clone(), true),
            Err(SnapshotPinError::PublicationTargetExists)
        ));
        drop(reopened);

        let stable = SnapshotPinStore::open_or_create(temporary.path(), lineage(9)?)?;
        assert_eq!(stable.iter().cloned().collect::<Vec<_>>(), vec![pin]);
        assert!(matches!(
            SnapshotPinStore::open_or_create(temporary.path(), lineage(8)?),
            Err(SnapshotPinError::LineageMismatch)
        ));
        Ok(())
    }
}
