// SPDX-License-Identifier: AGPL-3.0-only

//! Immutable content-addressed blob files and staged publication.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use hyphae_native_records::BlobReference;
use hyphae_native_types::{BlobId, NativeTypeError};
use thiserror::Error;

/// Fixed blob-file header before content bytes.
pub const BLOB_HEADER_SIZE: usize = 80;
/// Maximum blob accepted by the first bounded implementation.
pub const MAX_BLOB_SIZE: usize = 1_073_741_824;

const MAGIC: &[u8; 8] = b"HYBLOB01";
const FORMAT_VERSION: u16 = 1;
const HEADER_SIZE_U16: u16 = 80;
const CHECKSUM_START: usize = 40;
const CHECKSUM_END: usize = 44;
const BLOBS_DIRECTORY: &str = "blobs";
const TEMP_DIRECTORY: &str = "tmp";
const TEMP_BLOBS_DIRECTORY: &str = "blobs";

/// Complete verified references keyed by their content digest.
pub type BlobReferenceSet = BTreeMap<[u8; 32], BlobReference>;

/// Blob format, staging, publication, or recovery failure.
#[derive(Debug, Error)]
pub enum BlobError {
    /// Filesystem access failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Blob identity construction failed.
    #[error(transparent)]
    Identity(#[from] NativeTypeError),
    /// Blob bytes exceed the first bounded implementation.
    #[error("native blob is {actual} bytes; maximum is {MAX_BLOB_SIZE}")]
    TooLarge {
        /// Supplied content length.
        actual: usize,
    },
    /// Blob magic, version, header, flags, or reserved bytes are invalid.
    #[error("native blob preamble is invalid")]
    InvalidPreamble,
    /// Encoded file length differs from its declared logical length.
    #[error("native blob file length is invalid")]
    InvalidLength,
    /// Blob CRC32C does not match.
    #[error("native blob CRC32C mismatch")]
    ChecksumMismatch,
    /// Blob content digest, identity, filename, or supplied reference differs.
    #[error("native blob content identity mismatch")]
    IdentityMismatch,
    /// The owned directory contains an unexpected entry.
    #[error("native blob directory contains an unexpected entry")]
    UnexpectedDirectoryEntry,
    /// A temporary or final publication target already exists.
    #[error("native blob publication target already exists")]
    PublicationTargetExists,
    /// Two distinct digests map to the same stable blob identity.
    #[error("native blob identity collision")]
    IdentityCollision,
    /// A read reference is absent from this handle's verified namespace snapshot.
    #[error("native blob {0:?} is outside the captured blob namespace")]
    ReferenceOutsideBoundary(BlobId),
    /// Blob generation cannot be represented as u64.
    #[error("native blob generation is exhausted")]
    GenerationExhausted,
    /// Effective generation is lower than the verified physical file count.
    #[error("native blob generation is inconsistent with physical inventory")]
    InconsistentGeneration,
    /// Only one authoritative reference trace may be active at a time.
    #[error("native blob reference trace is already active")]
    ReferenceTraceActive,
    /// Reference tracing state cannot be trusted after synchronization poison.
    #[error("native blob reference trace state is poisoned")]
    ReferenceTracePoisoned,
}

/// Verified blob recovery evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobRecovery {
    /// Number of immutable verified blobs.
    pub blob_count: usize,
    /// Encoded bytes occupied by immutable verified blobs.
    pub blob_bytes: u64,
    /// Minimum generation supplied by retained committed roots.
    pub committed_generation_floor: u64,
    /// Effective generation after applying the committed floor.
    pub generation: u64,
    /// Interrupted canonical temporary files removed during open.
    pub recovered_temporary_files: usize,
    /// Whether strict parent-directory synchronization is supported.
    pub parent_sync_supported: bool,
}

/// Exact verified physical blob inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobInventory {
    /// Number of complete immutable files.
    pub blob_count: usize,
    /// Exact encoded bytes occupied by complete immutable files.
    pub blob_bytes: u64,
}

/// One deterministic immutable-blob pruning attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobPruneReceipt {
    /// Complete unreferenced candidates at the beginning of the attempt.
    pub candidate_files: usize,
    /// Exact encoded bytes in all initial candidates.
    pub candidate_bytes: u64,
    /// Candidate files removed by this attempt.
    pub removed_files: usize,
    /// Exact encoded bytes removed by this attempt.
    pub removed_bytes: u64,
    /// Complete immutable files retained after this attempt.
    pub retained_files: usize,
    /// Exact encoded bytes retained after this attempt.
    pub retained_bytes: u64,
    /// Whether strict namespace-directory synchronization is supported.
    pub parent_sync_supported: bool,
}

/// A create-new blob stage that is not visible from the final namespace.
#[derive(Debug)]
pub struct StagedBlob {
    reference: BlobReference,
    temporary_path: Option<PathBuf>,
    final_path: PathBuf,
}

impl StagedBlob {
    /// Returns the immutable content reference.
    pub const fn reference(&self) -> BlobReference {
        self.reference
    }

    /// Returns whether publication can reuse an existing verified blob.
    pub const fn already_present(&self) -> bool {
        self.temporary_path.is_none()
    }
}

/// Scoped authoritative collection of successfully resolved blob references.
#[derive(Debug)]
pub struct BlobReferenceTrace<'store> {
    store: &'store BlobStore,
    active: bool,
}

impl BlobReferenceTrace<'_> {
    /// Finishes the trace and returns references keyed by complete digest.
    ///
    /// # Errors
    ///
    /// Returns an error if trace synchronization state is poisoned.
    pub fn finish(mut self) -> Result<BlobReferenceSet, BlobError> {
        let references = self
            .store
            .reference_trace_lock()?
            .take()
            .ok_or(BlobError::ReferenceTracePoisoned)?;
        self.active = false;
        Ok(references)
    }
}

impl Drop for BlobReferenceTrace<'_> {
    fn drop(&mut self) {
        if self.active
            && let Ok(mut trace) = self.store.reference_trace.lock()
        {
            *trace = None;
        }
    }
}

/// One Hyphae-owned immutable blob namespace.
#[derive(Debug)]
pub struct BlobStore {
    blobs_directory: PathBuf,
    temporary_directory: PathBuf,
    blobs: BTreeMap<BlobId, BlobReference>,
    committed_generation_floor: u64,
    generation: u64,
    recovered_temporary_files: usize,
    reference_trace: Mutex<Option<BlobReferenceSet>>,
}

/// Shareable immutable view of one verified blob namespace snapshot.
///
/// The wrapper exposes no mutable publication or collection authority.
#[derive(Debug)]
pub struct BlobStoreReader(BlobStore);

impl Deref for BlobStoreReader {
    type Target = BlobStore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl BlobStore {
    /// Captures an immutable worker view of the current verified references.
    pub fn read_handle(&self) -> BlobStoreReader {
        BlobStoreReader(BlobStore {
            blobs_directory: self.blobs_directory.clone(),
            temporary_directory: self.temporary_directory.clone(),
            blobs: self.blobs.clone(),
            committed_generation_floor: self.committed_generation_floor,
            generation: self.generation,
            recovered_temporary_files: self.recovered_temporary_files,
            reference_trace: Mutex::new(None),
        })
    }

    /// Creates the owned blob and temporary directories.
    ///
    /// # Errors
    ///
    /// Returns an error if an owned target already exists or cannot be
    /// initialized.
    pub fn create(data_directory: impl AsRef<Path>) -> Result<Self, BlobError> {
        let data_directory = data_directory.as_ref();
        let blobs_directory = data_directory.join(BLOBS_DIRECTORY);
        fs::create_dir(&blobs_directory)?;
        let temporary_root = data_directory.join(TEMP_DIRECTORY);
        if !temporary_root.exists() {
            fs::create_dir(&temporary_root)?;
            sync_directory(data_directory)?;
        }
        let temporary_directory = temporary_root.join(TEMP_BLOBS_DIRECTORY);
        fs::create_dir(&temporary_directory)?;
        sync_directory(&temporary_root)?;
        sync_directory(data_directory)?;
        Ok(Self {
            blobs_directory,
            temporary_directory,
            blobs: BTreeMap::new(),
            committed_generation_floor: 0,
            generation: 0,
            recovered_temporary_files: 0,
            reference_trace: Mutex::new(None),
        })
    }

    /// Opens and verifies every immutable blob, then removes interrupted
    /// canonical temporary files.
    ///
    /// # Errors
    ///
    /// Returns an error for unexpected entries, complete corruption, identity
    /// collisions, I/O, or generation exhaustion.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, BlobError> {
        Self::open_with_generation_floor(data_directory, 0)
    }

    /// Opens every immutable blob with a retained committed generation floor.
    ///
    /// Physical collection may reduce the verified file count below a
    /// generation previously bound by a committed root. The effective
    /// generation is therefore the maximum of both values.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::open`].
    pub fn open_with_generation_floor(
        data_directory: impl AsRef<Path>,
        committed_generation_floor: u64,
    ) -> Result<Self, BlobError> {
        let data_directory = data_directory.as_ref();
        let blobs_directory = data_directory.join(BLOBS_DIRECTORY);
        let temporary_directory = data_directory
            .join(TEMP_DIRECTORY)
            .join(TEMP_BLOBS_DIRECTORY);
        let mut blobs = BTreeMap::new();
        for entry in fs::read_dir(&blobs_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(BlobError::UnexpectedDirectoryEntry);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| BlobError::UnexpectedDirectoryEntry)?;
            let digest = parse_final_filename(&name).ok_or(BlobError::UnexpectedDirectoryEntry)?;
            let encoded = fs::read(entry.path())?;
            let (reference, _) = decode_blob(&encoded)?;
            if reference.digest != digest {
                return Err(BlobError::IdentityMismatch);
            }
            let _inserted = insert_reference(&mut blobs, reference)?;
        }

        let mut temporary_paths = Vec::new();
        for entry in fs::read_dir(&temporary_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(BlobError::UnexpectedDirectoryEntry);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| BlobError::UnexpectedDirectoryEntry)?;
            parse_temporary_filename(&name).ok_or(BlobError::UnexpectedDirectoryEntry)?;
            temporary_paths.push(entry.path());
        }
        let recovered_temporary_files = temporary_paths.len();
        for path in temporary_paths {
            fs::remove_file(path)?;
        }
        if recovered_temporary_files != 0 {
            sync_directory(&temporary_directory)?;
        }
        let physical_generation = generation_for_count(blobs.len())?;
        Ok(Self {
            blobs_directory,
            temporary_directory,
            blobs,
            committed_generation_floor,
            generation: committed_generation_floor.max(physical_generation),
            recovered_temporary_files,
            reference_trace: Mutex::new(None),
        })
    }

    /// Returns verified recovery and generation evidence.
    ///
    /// # Errors
    ///
    /// Returns an error only if the blob count exceeds u64.
    pub fn recovery(&self) -> Result<BlobRecovery, BlobError> {
        Ok(BlobRecovery {
            blob_count: self.blobs.len(),
            blob_bytes: self.inventory()?.blob_bytes,
            committed_generation_floor: self.committed_generation_floor,
            generation: self.generation,
            recovered_temporary_files: self.recovered_temporary_files,
            parent_sync_supported: parent_sync_supported(),
        })
    }

    /// Returns the effective committed blob generation.
    ///
    /// # Errors
    ///
    /// Returns an error only if the blob count exceeds u64.
    pub fn generation(&self) -> Result<u64, BlobError> {
        if self.generation < generation_for_count(self.blobs.len())? {
            return Err(BlobError::InconsistentGeneration);
        }
        Ok(self.generation)
    }

    /// Raises the effective generation to a verified committed-root floor.
    ///
    /// This is used after the runtime has independently selected and verified
    /// its retained WAL/root authority. The generation never decreases.
    ///
    /// # Errors
    ///
    /// Returns an error if physical inventory cannot be represented by the
    /// effective generation.
    pub fn apply_committed_generation_floor(&mut self, floor: u64) -> Result<(), BlobError> {
        let physical = generation_for_count(self.blobs.len())?;
        self.committed_generation_floor = self.committed_generation_floor.max(floor);
        self.generation = self.generation.max(floor).max(physical);
        self.generation()?;
        Ok(())
    }

    /// Returns exact verified physical inventory.
    ///
    /// # Errors
    ///
    /// Returns an error if encoded byte totals overflow u64.
    pub fn inventory(&self) -> Result<BlobInventory, BlobError> {
        let mut blob_bytes = 0_u64;
        for reference in self.blobs.values() {
            blob_bytes = blob_bytes
                .checked_add(encoded_size(*reference)?)
                .ok_or(BlobError::GenerationExhausted)?;
        }
        Ok(BlobInventory {
            blob_count: self.blobs.len(),
            blob_bytes,
        })
    }

    /// Begins one scoped authoritative reference trace.
    ///
    /// Every successful [`Self::read`] records the exact resolved reference
    /// until the returned scope is finished or dropped.
    ///
    /// # Errors
    ///
    /// Returns an error for a nested trace or poisoned trace state.
    pub fn begin_reference_trace(&self) -> Result<BlobReferenceTrace<'_>, BlobError> {
        let mut trace = self.reference_trace_lock()?;
        if trace.is_some() {
            return Err(BlobError::ReferenceTraceActive);
        }
        *trace = Some(BTreeMap::new());
        drop(trace);
        Ok(BlobReferenceTrace {
            store: self,
            active: true,
        })
    }

    /// Stages content under a create-new temporary path.
    ///
    /// Existing final content is fully verified and returned as a reusable
    /// stage.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized content, collision, uncertain I/O, or
    /// an existing temporary target.
    pub fn stage(&self, content: &[u8], synchronize: bool) -> Result<StagedBlob, BlobError> {
        let reference = reference_for(content)?;
        let stem = digest_hex(reference.digest);
        let final_path = self.blobs_directory.join(format!("{stem}.hyblob"));
        if final_path.exists() {
            let encoded = fs::read(&final_path)?;
            let (found, _) = decode_blob(&encoded)?;
            if found != reference {
                return Err(BlobError::IdentityMismatch);
            }
            return Ok(StagedBlob {
                reference,
                temporary_path: None,
                final_path,
            });
        }
        if self
            .blobs
            .get(&reference.id)
            .is_some_and(|current| *current != reference)
        {
            return Err(BlobError::IdentityCollision);
        }
        let temporary_path = self.temporary_directory.join(format!("{stem}.tmp"));
        if temporary_path.exists() {
            return Err(BlobError::PublicationTargetExists);
        }
        let encoded = encode_blob(reference, content)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(&encoded)?;
        if synchronize {
            file.sync_all()?;
        }
        drop(file);
        Ok(StagedBlob {
            reference,
            temporary_path: Some(temporary_path),
            final_path,
        })
    }

    /// Promotes one staged blob into the immutable content namespace.
    ///
    /// # Errors
    ///
    /// Returns an error for a target race, collision, rename, or directory
    /// synchronization failure.
    pub fn publish(
        &mut self,
        staged: StagedBlob,
        synchronize: bool,
    ) -> Result<BlobReference, BlobError> {
        let is_new = match self.blobs.get(&staged.reference.id) {
            Some(current) if *current != staged.reference => {
                return Err(BlobError::IdentityCollision);
            }
            Some(_) => false,
            None => true,
        };
        let next_generation = if is_new {
            self.generation
                .checked_add(1)
                .ok_or(BlobError::GenerationExhausted)?
        } else {
            self.generation
        };
        if let Some(temporary_path) = staged.temporary_path {
            if staged.final_path.exists() {
                return Err(BlobError::PublicationTargetExists);
            }
            fs::rename(temporary_path, &staged.final_path)?;
            if synchronize {
                sync_directory(&self.blobs_directory)?;
                sync_directory(&self.temporary_directory)?;
            }
        }
        let inserted = insert_reference(&mut self.blobs, staged.reference)?;
        if inserted != is_new {
            return Err(BlobError::IdentityCollision);
        }
        self.generation = next_generation;
        Ok(staged.reference)
    }

    /// Stages and publishes one immutable blob.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::stage`] and [`Self::publish`].
    pub fn put(&mut self, content: &[u8], synchronize: bool) -> Result<BlobReference, BlobError> {
        let staged = self.stage(content, synchronize)?;
        self.publish(staged, synchronize)
    }

    /// Reads and re-verifies one immutable blob by exact reference.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing file, corruption, or reference mismatch.
    pub fn read(&self, reference: BlobReference) -> Result<Vec<u8>, BlobError> {
        if self.blobs.get(&reference.id) != Some(&reference) {
            return Err(BlobError::ReferenceOutsideBoundary(reference.id));
        }
        let path = self
            .blobs_directory
            .join(format!("{}.hyblob", digest_hex(reference.digest)));
        let encoded = fs::read(path)?;
        let (found, content) = decode_blob(&encoded)?;
        if found != reference {
            return Err(BlobError::IdentityMismatch);
        }
        self.record_reference(reference)?;
        Ok(content.to_vec())
    }

    /// Removes a deterministic prefix of verified references absent from the
    /// supplied authoritative live set.
    ///
    /// `limit = None` removes every candidate. Directory synchronization is
    /// performed after removal when `synchronize` is true, including for an
    /// idempotent zero-candidate retry.
    ///
    /// # Errors
    ///
    /// Returns an error for a forged live reference, uncertain file removal,
    /// byte overflow, or directory synchronization failure.
    pub fn prune_unreferenced(
        &mut self,
        live: &BlobReferenceSet,
        limit: Option<usize>,
        synchronize: bool,
    ) -> Result<BlobPruneReceipt, BlobError> {
        self.validate_live_references(live)?;
        let mut candidates = self
            .blobs
            .values()
            .copied()
            .filter(|reference| !live.contains_key(&reference.digest))
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|reference| reference.digest);
        let candidate_bytes = sum_encoded_bytes(&candidates)?;
        let removal_count = limit.unwrap_or(candidates.len()).min(candidates.len());
        let mut removed_bytes = 0_u64;
        for reference in candidates.iter().take(removal_count).copied() {
            let current = self
                .blobs
                .get(&reference.id)
                .copied()
                .ok_or(BlobError::IdentityMismatch)?;
            if current != reference {
                return Err(BlobError::IdentityMismatch);
            }
            let path = self
                .blobs_directory
                .join(format!("{}.hyblob", digest_hex(reference.digest)));
            fs::remove_file(path)?;
            let removed = self
                .blobs
                .remove(&reference.id)
                .ok_or(BlobError::IdentityMismatch)?;
            if removed != reference {
                return Err(BlobError::IdentityMismatch);
            }
            removed_bytes = removed_bytes
                .checked_add(encoded_size(reference)?)
                .ok_or(BlobError::GenerationExhausted)?;
        }
        if synchronize {
            self.synchronize_namespace()?;
        }
        let retained = self.inventory()?;
        Ok(BlobPruneReceipt {
            candidate_files: candidates.len(),
            candidate_bytes,
            removed_files: removal_count,
            removed_bytes,
            retained_files: retained.blob_count,
            retained_bytes: retained.blob_bytes,
            parent_sync_supported: parent_sync_supported(),
        })
    }

    /// Synchronizes the immutable blob namespace where supported.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace cannot be synchronized or
    /// inspected.
    pub fn synchronize_namespace(&self) -> Result<(), BlobError> {
        sync_directory(&self.blobs_directory)?;
        Ok(())
    }

    fn validate_live_references(&self, live: &BlobReferenceSet) -> Result<(), BlobError> {
        for (digest, reference) in live {
            if *digest != reference.digest
                || self.blobs.get(&reference.id).copied() != Some(*reference)
            {
                return Err(BlobError::IdentityMismatch);
            }
        }
        Ok(())
    }

    fn record_reference(&self, reference: BlobReference) -> Result<(), BlobError> {
        let mut trace = self.reference_trace_lock()?;
        let Some(references) = trace.as_mut() else {
            return Ok(());
        };
        match references.get(&reference.digest) {
            Some(current) if *current != reference => Err(BlobError::IdentityMismatch),
            Some(_) => Ok(()),
            None => {
                references.insert(reference.digest, reference);
                Ok(())
            }
        }
    }

    fn reference_trace_lock(&self) -> Result<MutexGuard<'_, Option<BlobReferenceSet>>, BlobError> {
        self.reference_trace
            .lock()
            .map_err(|_| BlobError::ReferenceTracePoisoned)
    }
}

fn reference_for(content: &[u8]) -> Result<BlobReference, BlobError> {
    if content.len() > MAX_BLOB_SIZE {
        return Err(BlobError::TooLarge {
            actual: content.len(),
        });
    }
    let digest = *blake3::hash(content).as_bytes();
    let mut identity_bytes = [0_u8; 16];
    identity_bytes.copy_from_slice(&digest[..16]);
    let mut identity = u128::from_le_bytes(identity_bytes);
    if identity == 0 {
        identity = 1;
    }
    Ok(BlobReference {
        id: BlobId::new(identity)?,
        logical_length: u64::try_from(content.len()).map_err(|_| BlobError::TooLarge {
            actual: content.len(),
        })?,
        digest,
    })
}

fn encode_blob(reference: BlobReference, content: &[u8]) -> Result<Vec<u8>, BlobError> {
    if content.len() > MAX_BLOB_SIZE
        || u64::try_from(content.len()).ok() != Some(reference.logical_length)
        || *blake3::hash(content).as_bytes() != reference.digest
        || reference_for(content)? != reference
    {
        return Err(BlobError::IdentityMismatch);
    }
    let mut encoded = vec![0_u8; BLOB_HEADER_SIZE + content.len()];
    encoded[0..8].copy_from_slice(MAGIC);
    encoded[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    encoded[10..12].copy_from_slice(&HEADER_SIZE_U16.to_le_bytes());
    encoded[16..32].copy_from_slice(&reference.id.get().to_le_bytes());
    encoded[32..40].copy_from_slice(&reference.logical_length.to_le_bytes());
    encoded[48..80].copy_from_slice(&reference.digest);
    encoded[BLOB_HEADER_SIZE..].copy_from_slice(content);
    let checksum = blob_checksum(&encoded);
    encoded[CHECKSUM_START..CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
    Ok(encoded)
}

fn decode_blob(encoded: &[u8]) -> Result<(BlobReference, &[u8]), BlobError> {
    if encoded.len() < BLOB_HEADER_SIZE {
        return Err(BlobError::InvalidLength);
    }
    if &encoded[0..8] != MAGIC
        || read_u16(&encoded[8..10]) != FORMAT_VERSION
        || read_u16(&encoded[10..12]) != HEADER_SIZE_U16
        || encoded[12..16].iter().any(|byte| *byte != 0)
        || encoded[44..48].iter().any(|byte| *byte != 0)
    {
        return Err(BlobError::InvalidPreamble);
    }
    let logical_length =
        usize::try_from(read_u64(&encoded[32..40])).map_err(|_| BlobError::InvalidLength)?;
    if logical_length > MAX_BLOB_SIZE || encoded.len() != BLOB_HEADER_SIZE + logical_length {
        return Err(BlobError::InvalidLength);
    }
    if blob_checksum(encoded) != read_u32(&encoded[CHECKSUM_START..CHECKSUM_END]) {
        return Err(BlobError::ChecksumMismatch);
    }
    let id = BlobId::new(read_u128(&encoded[16..32]))?;
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&encoded[48..80]);
    let content = &encoded[BLOB_HEADER_SIZE..];
    let reference = BlobReference {
        id,
        logical_length: u64::try_from(logical_length).map_err(|_| BlobError::InvalidLength)?,
        digest,
    };
    if reference_for(content)? != reference {
        return Err(BlobError::IdentityMismatch);
    }
    Ok((reference, content))
}

fn blob_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[CHECKSUM_START..CHECKSUM_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn insert_reference(
    blobs: &mut BTreeMap<BlobId, BlobReference>,
    reference: BlobReference,
) -> Result<bool, BlobError> {
    match blobs.get(&reference.id) {
        Some(current) if *current != reference => Err(BlobError::IdentityCollision),
        Some(_) => Ok(false),
        None => {
            blobs.insert(reference.id, reference);
            Ok(true)
        }
    }
}

fn generation_for_count(count: usize) -> Result<u64, BlobError> {
    u64::try_from(count).map_err(|_| BlobError::GenerationExhausted)
}

fn encoded_size(reference: BlobReference) -> Result<u64, BlobError> {
    u64::try_from(BLOB_HEADER_SIZE)
        .ok()
        .and_then(|header| header.checked_add(reference.logical_length))
        .ok_or(BlobError::GenerationExhausted)
}

fn sum_encoded_bytes(references: &[BlobReference]) -> Result<u64, BlobError> {
    references.iter().try_fold(0_u64, |total, reference| {
        total
            .checked_add(encoded_size(*reference)?)
            .ok_or(BlobError::GenerationExhausted)
    })
}

fn digest_hex(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn parse_final_filename(name: &str) -> Option<[u8; 32]> {
    parse_digest_hex(name.strip_suffix(".hyblob")?)
}

fn parse_temporary_filename(name: &str) -> Option<[u8; 32]> {
    parse_digest_hex(name.strip_suffix(".tmp")?)
}

fn parse_digest_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = decode_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(decode_nibble(pair[1])?)?;
    }
    Some(digest)
}

const fn decode_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
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

fn read_u128(bytes: &[u8]) -> u128 {
    let mut value = [0_u8; 16];
    value.copy_from_slice(bytes);
    u128::from_le_bytes(value)
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
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{BlobError, BlobStore, encode_blob, reference_for};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-blobs-{}-{sequence}",
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

    #[test]
    fn blob_round_trips_deduplicates_and_reopens() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let content = vec![0x5a; 32_768];
        let reference = store.put(&content, true)?;
        assert_eq!(store.read(reference)?, content);
        assert_eq!(store.put(&content, true)?, reference);
        assert_eq!(store.generation()?, 1);
        drop(store);

        let reopened = BlobStore::open(temporary.path())?;
        assert_eq!(reopened.recovery()?.blob_count, 1);
        assert_eq!(reopened.read(reference)?, content);
        Ok(())
    }

    #[test]
    fn immutable_read_handle_freezes_the_verified_blob_namespace()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let first = store.put(b"first", false)?;
        let reader = store.read_handle();
        let second = store.put(b"second", false)?;

        assert_eq!(reader.read(first)?, b"first");
        assert!(matches!(
            reader.read(second),
            Err(BlobError::ReferenceOutsideBoundary(id)) if id == second.id
        ));
        assert_eq!(store.read(second)?, b"second");
        Ok(())
    }

    #[test]
    fn temporary_stage_is_removed_and_can_be_retried() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let store = BlobStore::create(temporary.path())?;
        let staged = store.stage(b"interrupted", true)?;
        assert!(!staged.already_present());
        drop(staged);
        drop(store);

        let reopened = BlobStore::open(temporary.path())?;
        assert_eq!(reopened.recovery()?.recovered_temporary_files, 1);
        let _replacement = reopened.stage(b"interrupted", false)?;
        Ok(())
    }

    #[test]
    fn complete_corruption_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let reference = store.put(b"durable", true)?;
        drop(store);
        let path = temporary
            .path()
            .join("blobs")
            .join(format!("{}.hyblob", super::digest_hex(reference.digest)));
        let mut encoded = fs::read(&path)?;
        encoded[super::BLOB_HEADER_SIZE] ^= 1;
        fs::write(path, encoded)?;
        assert!(matches!(
            BlobStore::open(temporary.path()),
            Err(BlobError::ChecksumMismatch)
        ));
        Ok(())
    }

    #[test]
    fn exact_blob_file_encoding_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let content = b"hyphae-native-blob-golden";
        let reference = reference_for(content)?;
        let encoded = encode_blob(reference, content)?;
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "9a0f3fe1cb0a72d63a0e4aa9bf00dd22c99e8439f5bd18f18414f09a405e42da"
        );
        Ok(())
    }

    #[test]
    fn traced_liveness_prunes_dead_blobs_without_lowering_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let live = store.put(b"live-content", true)?;
        let _dead_one = store.put(b"dead-content-one", true)?;
        let _dead_two = store.put(b"dead-content-two", true)?;
        assert_eq!(store.generation()?, 3);

        let trace = store.begin_reference_trace()?;
        assert_eq!(store.read(live)?, b"live-content");
        let live_references = trace.finish()?;
        let receipt = store.prune_unreferenced(&live_references, None, false)?;
        assert_eq!(receipt.candidate_files, 2);
        assert_eq!(receipt.removed_files, 2);
        assert_eq!(receipt.retained_files, 1);
        assert_eq!(store.generation()?, 3);
        store.synchronize_namespace()?;
        drop(store);

        let mut reopened = BlobStore::open_with_generation_floor(temporary.path(), 3)?;
        assert_eq!(reopened.recovery()?.blob_count, 1);
        assert_eq!(reopened.generation()?, 3);
        assert_eq!(reopened.read(live)?, b"live-content");
        let _new = reopened.put(b"new-live-content", false)?;
        assert_eq!(reopened.generation()?, 4);
        Ok(())
    }

    #[test]
    fn partial_pruning_is_deterministic_and_idempotent() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let live = store.put(b"live", false)?;
        let _dead_one = store.put(b"dead-one", false)?;
        let _dead_two = store.put(b"dead-two", false)?;

        let trace = store.begin_reference_trace()?;
        let _content = store.read(live)?;
        let live_references = trace.finish()?;
        let partial = store.prune_unreferenced(&live_references, Some(1), false)?;
        assert_eq!(partial.candidate_files, 2);
        assert_eq!(partial.removed_files, 1);
        assert_eq!(partial.retained_files, 2);
        drop(store);

        let mut reopened = BlobStore::open_with_generation_floor(temporary.path(), 3)?;
        let trace = reopened.begin_reference_trace()?;
        let _content = reopened.read(live)?;
        let live_references = trace.finish()?;
        let completed = reopened.prune_unreferenced(&live_references, None, true)?;
        assert_eq!(completed.candidate_files, 1);
        assert_eq!(completed.removed_files, 1);
        assert_eq!(completed.retained_files, 1);
        let retry = reopened.prune_unreferenced(&live_references, None, true)?;
        assert_eq!(retry.candidate_files, 0);
        assert_eq!(retry.removed_files, 0);
        assert_eq!(retry.retained_files, 1);
        Ok(())
    }

    #[test]
    fn nested_trace_and_forged_live_reference_fail_closed() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::create()?;
        let mut store = BlobStore::create(temporary.path())?;
        let live = store.put(b"live", false)?;
        let trace = store.begin_reference_trace()?;
        assert!(matches!(
            store.begin_reference_trace(),
            Err(BlobError::ReferenceTraceActive)
        ));
        let _content = store.read(live)?;
        let mut live_references = trace.finish()?;
        let forged = super::reference_for(b"forged")?;
        live_references.insert(forged.digest, forged);
        assert!(matches!(
            store.prune_unreferenced(&live_references, None, false),
            Err(BlobError::IdentityMismatch)
        ));
        Ok(())
    }
}
