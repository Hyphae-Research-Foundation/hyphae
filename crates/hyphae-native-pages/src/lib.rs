// SPDX-License-Identifier: Apache-2.0

//! Native copy-on-write page codec, page file, and partitioned buffer pool.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use hyphae_native_types::{Csn, PageId};
use thiserror::Error;

/// Fixed native page size.
pub const PAGE_SIZE: usize = 16_384;
/// Native page header size.
pub const PAGE_HEADER_SIZE: usize = 96;
/// Maximum inline payload in one native page.
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

const PAGE_SIZE_U64: u64 = 16_384;
const PAGE_HEADER_SIZE_U16: u16 = 96;
const MAGIC: [u8; 8] = *b"HYPAGE01";
const FORMAT_VERSION: u16 = 1;
const DIGEST_START: usize = 48;
const DIGEST_END: usize = 80;
const CHECKSUM_START: usize = 44;
const CHECKSUM_END: usize = 48;

/// Native page role.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PageKind {
    /// Catalog root.
    CatalogRoot = 1,
    /// Relational heap leaf.
    HeapLeaf = 2,
    /// MVCC version-chain page.
    VersionChain = 3,
    /// B-tree internal node.
    BTreeInternal = 4,
    /// B-tree leaf node.
    BTreeLeaf = 5,
    /// Hash directory.
    HashDirectory = 6,
    /// Hash bucket.
    HashBucket = 7,
    /// Specialized structure node.
    StructureNode = 8,
    /// Bitmap or doc-values page.
    Bitmap = 9,
    /// Transactional search delta.
    SearchDelta = 10,
    /// Vector metadata page.
    VectorMetadata = 11,
    /// Overflow reference page.
    Overflow = 12,
}

impl TryFrom<u8> for PageKind {
    type Error = PageError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::CatalogRoot),
            2 => Ok(Self::HeapLeaf),
            3 => Ok(Self::VersionChain),
            4 => Ok(Self::BTreeInternal),
            5 => Ok(Self::BTreeLeaf),
            6 => Ok(Self::HashDirectory),
            7 => Ok(Self::HashBucket),
            8 => Ok(Self::StructureNode),
            9 => Ok(Self::Bitmap),
            10 => Ok(Self::SearchDelta),
            11 => Ok(Self::VectorMetadata),
            12 => Ok(Self::Overflow),
            other => Err(PageError::UnknownKind(other)),
        }
    }
}

/// Native page codec failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PageError {
    /// Byte length is not exactly one page.
    #[error("native page has length {actual}; expected {PAGE_SIZE}")]
    InvalidLength {
        /// Actual byte length.
        actual: usize,
    },
    /// Magic bytes do not identify a native page.
    #[error("invalid native page magic")]
    InvalidMagic,
    /// Format or header version is unsupported.
    #[error("unsupported native page format {version} or header length {header_length}")]
    UnsupportedFormat {
        /// Found format version.
        version: u16,
        /// Found header length.
        header_length: u16,
    },
    /// Unknown page-kind byte.
    #[error("unknown native page kind {0}")]
    UnknownKind(u8),
    /// Reserved bytes or flags are nonzero.
    #[error("native page reserved bytes or flags are nonzero")]
    ReservedNonzero,
    /// Page identity differs from the requested physical slot.
    #[error("native page ID mismatch: expected {expected}, found {found}")]
    PageIdMismatch {
        /// Requested page ID.
        expected: u64,
        /// Encoded page ID.
        found: u64,
    },
    /// Payload exceeds the inline page capacity.
    #[error("native page payload is {actual} bytes; maximum is {PAGE_PAYLOAD_SIZE}")]
    PayloadTooLarge {
        /// Payload byte length.
        actual: usize,
    },
    /// Bytes after the declared payload are not zero.
    #[error("native page padding is nonzero")]
    NonzeroPadding,
    /// CRC32C integrity check failed.
    #[error("native page CRC32C mismatch")]
    ChecksumMismatch,
    /// BLAKE3 page digest failed.
    #[error("native page BLAKE3 mismatch")]
    DigestMismatch,
}

/// Verified immutable native page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    id: PageId,
    kind: PageKind,
    creating_csn: Option<Csn>,
    next: Option<PageId>,
    payload: Vec<u8>,
}

impl Page {
    /// Constructs a checked native page.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload exceeds one page.
    pub fn new(
        id: PageId,
        kind: PageKind,
        creating_csn: Option<Csn>,
        next: Option<PageId>,
        payload: impl Into<Vec<u8>>,
    ) -> Result<Self, PageError> {
        let payload = payload.into();
        if payload.len() > PAGE_PAYLOAD_SIZE {
            return Err(PageError::PayloadTooLarge {
                actual: payload.len(),
            });
        }
        Ok(Self {
            id,
            kind,
            creating_csn,
            next,
            payload,
        })
    }

    /// Returns the physical page identity.
    pub const fn id(&self) -> PageId {
        self.id
    }

    /// Returns the page role.
    pub const fn kind(&self) -> PageKind {
        self.kind
    }

    /// Returns the creating commit sequence, or `None` before publication.
    pub const fn creating_csn(&self) -> Option<Csn> {
        self.creating_csn
    }

    /// Returns the kind-specific next page.
    pub const fn next(&self) -> Option<PageId> {
        self.next
    }

    /// Returns the verified inline payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Encodes one complete fixed-size native page.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; PAGE_SIZE];
        bytes[0..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[10] = self.kind as u8;
        bytes[12..14].copy_from_slice(&PAGE_HEADER_SIZE_U16.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.id.get().to_le_bytes());
        bytes[24..32].copy_from_slice(&self.creating_csn.map_or(0, Csn::get).to_le_bytes());
        bytes[32..40].copy_from_slice(&self.next.map_or(0, PageId::get).to_le_bytes());
        bytes[40..44].copy_from_slice(
            &u32::try_from(self.payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        bytes[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + self.payload.len()]
            .copy_from_slice(&self.payload);

        let checksum = page_checksum(&bytes);
        bytes[CHECKSUM_START..CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
        let digest = page_digest(&bytes);
        bytes[DIGEST_START..DIGEST_END].copy_from_slice(&digest);
        bytes
    }

    /// Decodes and verifies one fixed-size native page.
    ///
    /// # Errors
    ///
    /// Returns an error for any noncanonical or corrupt field.
    pub fn decode(expected_id: PageId, encoded: &[u8]) -> Result<Self, PageError> {
        if encoded.len() != PAGE_SIZE {
            return Err(PageError::InvalidLength {
                actual: encoded.len(),
            });
        }
        if encoded[0..8] != MAGIC {
            return Err(PageError::InvalidMagic);
        }
        let version = read_u16(&encoded[8..10]);
        let header_length = read_u16(&encoded[12..14]);
        if version != FORMAT_VERSION || usize::from(header_length) != PAGE_HEADER_SIZE {
            return Err(PageError::UnsupportedFormat {
                version,
                header_length,
            });
        }
        if encoded[11] != 0
            || encoded[14..16].iter().any(|byte| *byte != 0)
            || encoded[80..96].iter().any(|byte| *byte != 0)
        {
            return Err(PageError::ReservedNonzero);
        }
        let kind = PageKind::try_from(encoded[10])?;
        let found_id = read_u64(&encoded[16..24]);
        if found_id != expected_id.get() {
            return Err(PageError::PageIdMismatch {
                expected: expected_id.get(),
                found: found_id,
            });
        }
        let payload_length = usize::try_from(read_u32(&encoded[40..44]))
            .map_err(|_| PageError::PayloadTooLarge { actual: usize::MAX })?;
        if payload_length > PAGE_PAYLOAD_SIZE {
            return Err(PageError::PayloadTooLarge {
                actual: payload_length,
            });
        }
        let payload_end = PAGE_HEADER_SIZE + payload_length;
        if encoded[payload_end..].iter().any(|byte| *byte != 0) {
            return Err(PageError::NonzeroPadding);
        }
        let expected_checksum = read_u32(&encoded[CHECKSUM_START..CHECKSUM_END]);
        if page_checksum(encoded) != expected_checksum {
            return Err(PageError::ChecksumMismatch);
        }
        let mut expected_digest = [0_u8; 32];
        expected_digest.copy_from_slice(&encoded[DIGEST_START..DIGEST_END]);
        if page_digest(encoded) != expected_digest {
            return Err(PageError::DigestMismatch);
        }

        let raw_csn = read_u64(&encoded[24..32]);
        let raw_next = read_u64(&encoded[32..40]);
        Ok(Self {
            id: expected_id,
            kind,
            creating_csn: if raw_csn == 0 {
                None
            } else {
                Csn::new(raw_csn).ok()
            },
            next: if raw_next == 0 {
                None
            } else {
                PageId::new(raw_next).ok()
            },
            payload: encoded[PAGE_HEADER_SIZE..payload_end].to_vec(),
        })
    }
}

fn page_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[CHECKSUM_START..DIGEST_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn page_digest(encoded: &[u8]) -> [u8; 32] {
    let mut canonical = encoded.to_vec();
    canonical[DIGEST_START..DIGEST_END].fill(0);
    *blake3::hash(&canonical).as_bytes()
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

/// Native append-only page-file failure.
#[derive(Debug, Error)]
pub enum PageStoreError {
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Page codec rejected bytes.
    #[error(transparent)]
    Page(#[from] PageError),
    /// Existing file length is not a whole number of pages.
    #[error("native page file length {length} is not a multiple of {PAGE_SIZE}")]
    InvalidFileLength {
        /// Physical file length.
        length: u64,
    },
    /// A prior uncertain write or sync poisoned the writer.
    #[error("native page writer is poisoned; reopen before writing")]
    Poisoned,
    /// Page-ID space is exhausted.
    #[error("native page ID space is exhausted")]
    PageIdExhausted,
}

/// Page store reopened after repairing only an incomplete final page.
#[derive(Debug)]
pub struct OpenedPageStore {
    /// Ready append-only page store.
    pub store: PageStore,
    /// Number of incomplete tail bytes removed.
    pub truncated_tail_bytes: u64,
}

/// Append-only file of immutable native pages.
#[derive(Debug)]
pub struct PageStore {
    file: File,
    next_page_id: u64,
    poisoned: bool,
}

impl PageStore {
    /// Creates a new empty page file.
    ///
    /// # Errors
    ///
    /// Returns an error if the path exists or cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, PageStoreError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            file,
            next_page_id: 1,
            poisoned: false,
        })
    }

    /// Opens an existing canonical page file.
    ///
    /// # Errors
    ///
    /// Returns an error when its length is not page aligned.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PageStoreError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        if length % PAGE_SIZE_U64 != 0 {
            return Err(PageStoreError::InvalidFileLength { length });
        }
        let count = length / PAGE_SIZE_U64;
        let next_page_id = count
            .checked_add(1)
            .ok_or(PageStoreError::PageIdExhausted)?;
        Ok(Self {
            file,
            next_page_id,
            poisoned: false,
        })
    }

    /// Opens a page file and removes only an incomplete final physical page.
    ///
    /// Complete pages are never rewritten or silently skipped. Callers must
    /// still verify every page reachable from a recovered committed root.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem or address-space failures.
    pub fn open_repair_tail(path: impl AsRef<Path>) -> Result<OpenedPageStore, PageStoreError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        let truncated_tail_bytes = length % PAGE_SIZE_U64;
        let complete_length = length - truncated_tail_bytes;
        if truncated_tail_bytes != 0 {
            file.set_len(complete_length)?;
            file.sync_data()?;
        }
        let count = complete_length / PAGE_SIZE_U64;
        let next_page_id = count
            .checked_add(1)
            .ok_or(PageStoreError::PageIdExhausted)?;
        Ok(OpenedPageStore {
            store: Self {
                file,
                next_page_id,
                poisoned: false,
            },
            truncated_tail_bytes,
        })
    }

    /// Returns the number of complete physical pages.
    pub const fn page_count(&self) -> u64 {
        self.next_page_id - 1
    }

    /// Appends one unpublished or committed copy-on-write page.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid payload or uncertain filesystem write.
    pub fn append(
        &mut self,
        kind: PageKind,
        creating_csn: Option<Csn>,
        next: Option<PageId>,
        payload: impl Into<Vec<u8>>,
    ) -> Result<PageId, PageStoreError> {
        if self.poisoned {
            return Err(PageStoreError::Poisoned);
        }
        let id = PageId::new(self.next_page_id).map_err(|_| PageStoreError::PageIdExhausted)?;
        let encoded = Page::new(id, kind, creating_csn, next, payload)?.encode();
        self.poisoned = true;
        let write_result = self
            .file
            .seek(SeekFrom::End(0))
            .and_then(|_| self.file.write_all(&encoded));
        if let Err(source) = write_result {
            return Err(PageStoreError::Io(source));
        }
        self.next_page_id = self
            .next_page_id
            .checked_add(1)
            .ok_or(PageStoreError::PageIdExhausted)?;
        self.poisoned = false;
        Ok(id)
    }

    /// Reads and verifies one page by physical identity.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, truncation, corruption, or ID mismatch.
    pub fn read(&self, id: PageId) -> Result<Page, PageStoreError> {
        let offset = id
            .get()
            .checked_sub(1)
            .and_then(|slot| slot.checked_mul(PAGE_SIZE_U64))
            .ok_or(PageStoreError::PageIdExhausted)?;
        let mut bytes = vec![0_u8; PAGE_SIZE];
        read_exact_at(&self.file, &mut bytes, offset)?;
        Ok(Page::decode(id, &bytes)?)
    }

    /// Synchronizes previously appended page bytes.
    ///
    /// # Errors
    ///
    /// Returns an error and poisons the writer when synchronization is
    /// uncertain.
    pub fn sync_data(&mut self) -> Result<(), PageStoreError> {
        if self.poisoned {
            return Err(PageStoreError::Poisoned);
        }
        if let Err(source) = self.file.sync_data() {
            self.poisoned = true;
            return Err(PageStoreError::Io(source));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    while !buffer.is_empty() {
        let read = file.read_at(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated native page",
            ));
        }
        offset = offset.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        buffer = &mut buffer[read..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    while !buffer.is_empty() {
        let read = file.seek_read(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated native page",
            ));
        }
        offset = offset.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        buffer = &mut buffer[read..];
    }
    Ok(())
}

/// Partitioned buffer-pool failure.
#[derive(Debug, Error)]
pub enum BufferPoolError {
    /// Page-file read failed.
    #[error(transparent)]
    Store(#[from] PageStoreError),
    /// Capacity or partition count is invalid.
    #[error("buffer-pool capacity {capacity} must be at least partition count {partitions}")]
    InvalidCapacity {
        /// Requested total frame capacity.
        capacity: usize,
        /// Requested partition count.
        partitions: usize,
    },
    /// A partition mutex was poisoned.
    #[error("buffer-pool partition mutex is poisoned")]
    Poisoned,
    /// Every frame in the selected full partition is pinned.
    #[error("buffer-pool partition is full and every frame is pinned")]
    PartitionExhausted,
}

/// Shared immutable page frame.
#[derive(Debug)]
pub struct PageFrame {
    page: Page,
    last_used: AtomicU64,
}

impl PageFrame {
    /// Returns the verified immutable page.
    pub const fn page(&self) -> &Page {
        &self.page
    }
}

#[derive(Debug)]
struct PoolPartition {
    capacity: usize,
    frames: HashMap<PageId, Arc<PageFrame>>,
}

/// Bounded partitioned cache for verified immutable pages.
#[derive(Debug)]
pub struct BufferPool {
    partitions: Vec<Mutex<PoolPartition>>,
    clock: AtomicU64,
}

impl BufferPool {
    /// Constructs a partitioned buffer pool.
    ///
    /// # Errors
    ///
    /// Returns an error for zero partitions or insufficient total capacity.
    pub fn new(capacity: usize, partition_count: usize) -> Result<Self, BufferPoolError> {
        if partition_count == 0 || capacity < partition_count {
            return Err(BufferPoolError::InvalidCapacity {
                capacity,
                partitions: partition_count,
            });
        }
        let base = capacity / partition_count;
        let remainder = capacity % partition_count;
        let partitions = (0..partition_count)
            .map(|index| {
                Mutex::new(PoolPartition {
                    capacity: base + usize::from(index < remainder),
                    frames: HashMap::new(),
                })
            })
            .collect();
        Ok(Self {
            partitions,
            clock: AtomicU64::new(1),
        })
    }

    /// Returns or loads one verified page frame.
    ///
    /// # Errors
    ///
    /// Returns an error for page I/O/corruption, mutex poisoning, or when all
    /// frames in the target partition are pinned.
    pub fn get_or_load(
        &self,
        store: &PageStore,
        page_id: PageId,
    ) -> Result<Arc<PageFrame>, BufferPoolError> {
        let partition_index = self.partition_index(page_id);
        let stamp = self.clock.fetch_add(1, Ordering::Relaxed);
        {
            let partition = self.partitions[partition_index]
                .lock()
                .map_err(|_| BufferPoolError::Poisoned)?;
            if let Some(frame) = partition.frames.get(&page_id) {
                frame.last_used.store(stamp, Ordering::Relaxed);
                return Ok(Arc::clone(frame));
            }
        }

        let page = store.read(page_id)?;
        let mut partition = self.partitions[partition_index]
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?;
        if let Some(frame) = partition.frames.get(&page_id) {
            frame.last_used.store(stamp, Ordering::Relaxed);
            return Ok(Arc::clone(frame));
        }
        if partition.frames.len() >= partition.capacity {
            let victim = partition
                .frames
                .iter()
                .filter(|(_, frame)| Arc::strong_count(frame) == 1)
                .min_by_key(|(_, frame)| frame.last_used.load(Ordering::Relaxed))
                .map(|(id, _)| *id)
                .ok_or(BufferPoolError::PartitionExhausted)?;
            partition.frames.remove(&victim);
        }
        let frame = Arc::new(PageFrame {
            page,
            last_used: AtomicU64::new(stamp),
        });
        partition.frames.insert(page_id, Arc::clone(&frame));
        Ok(frame)
    }

    fn partition_index(&self, page_id: PageId) -> usize {
        let count = self.partitions.len();
        usize::try_from(page_id.get() % u64::try_from(count).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_types::{Csn, PageId};

    use super::{
        BufferPool, BufferPoolError, PAGE_SIZE, Page, PageError, PageKind, PageStore,
        PageStoreError,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-pages-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self { path })
        }

        fn page_file(&self) -> PathBuf {
            self.path.join("pages.hydb")
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(self.path());
        }
    }

    #[test]
    fn page_codec_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let id = PageId::new(7)?;
        let page = Page::new(
            id,
            PageKind::HeapLeaf,
            Some(Csn::new(3)?),
            None,
            b"row".to_vec(),
        )?;
        assert_eq!(Page::decode(id, &page.encode())?, page);
        Ok(())
    }

    #[test]
    fn golden_page_encoding_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let page = Page::new(
            PageId::new(7)?,
            PageKind::HeapLeaf,
            Some(Csn::new(3)?),
            None,
            b"row".to_vec(),
        )?;
        let encoded = page.encode();
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "d4e3a11874f9bd5b9c4afdc4093466b493de23ac4b80c11843ad9b465bbf7e71"
        );
        assert_eq!(&encoded[0..8], b"HYPAGE01");
        assert_eq!(&encoded[8..10], &1_u16.to_le_bytes());
        assert_eq!(&encoded[16..24], &7_u64.to_le_bytes());
        assert_eq!(&encoded[24..32], &3_u64.to_le_bytes());
        assert_eq!(&encoded[40..44], &3_u32.to_le_bytes());
        Ok(())
    }

    #[test]
    fn page_codec_rejects_corruption() -> Result<(), Box<dyn std::error::Error>> {
        let id = PageId::new(1)?;
        let page = Page::new(id, PageKind::CatalogRoot, None, None, b"catalog".to_vec())?;
        let mut encoded = page.encode();
        encoded[PAGE_SIZE - 1] ^= 1;
        assert_eq!(Page::decode(id, &encoded), Err(PageError::NonzeroPadding));
        encoded = page.encode();
        encoded[100] ^= 1;
        assert_eq!(Page::decode(id, &encoded), Err(PageError::ChecksumMismatch));
        Ok(())
    }

    #[test]
    fn page_store_appends_reopens_and_reads() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.page_file();
        let first_id;
        {
            let mut store = PageStore::create(&path)?;
            first_id = store.append(PageKind::CatalogRoot, None, None, b"root".to_vec())?;
            store.sync_data()?;
        }
        let store = PageStore::open(&path)?;
        assert_eq!(store.page_count(), 1);
        assert_eq!(store.read(first_id)?.payload(), b"root");
        Ok(())
    }

    #[test]
    fn page_store_rejects_unaligned_files() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.page_file();
        fs::write(&path, b"truncated")?;
        assert!(matches!(
            PageStore::open(&path),
            Err(PageStoreError::InvalidFileLength { .. })
        ));
        Ok(())
    }

    #[test]
    fn page_store_repairs_only_an_incomplete_tail() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.page_file();
        let mut store = PageStore::create(&path)?;
        let root = store.append(PageKind::CatalogRoot, None, None, b"root".to_vec())?;
        drop(store);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"torn-page")?;

        let opened = PageStore::open_repair_tail(&path)?;
        assert_eq!(opened.truncated_tail_bytes, 9);
        assert_eq!(opened.store.page_count(), 1);
        assert_eq!(opened.store.read(root)?.payload(), b"root");
        Ok(())
    }

    #[test]
    fn buffer_pool_respects_pins_and_evicts_unpinned_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.page_file();
        let mut store = PageStore::create(&path)?;
        let first = store.append(PageKind::HeapLeaf, None, None, b"one".to_vec())?;
        let second = store.append(PageKind::HeapLeaf, None, None, b"two".to_vec())?;
        store.sync_data()?;
        let pool = BufferPool::new(1, 1)?;
        let pinned = pool.get_or_load(&store, first)?;
        assert!(matches!(
            pool.get_or_load(&store, second),
            Err(BufferPoolError::PartitionExhausted)
        ));
        drop(pinned);
        assert_eq!(pool.get_or_load(&store, second)?.page().payload(), b"two");
        Ok(())
    }
}
