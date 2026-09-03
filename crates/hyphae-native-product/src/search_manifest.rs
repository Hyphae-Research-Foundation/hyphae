// SPDX-License-Identifier: Apache-2.0

//! Chunked durable manifest of the documents in one product search collection.
//!
//! The manifest answers three questions on every mutation and query: how many
//! documents the collection holds, whether one identity is a member, and the
//! ordered enumeration of members for pagination. Format 1 (`HYPSMAN1`) kept
//! every identity in one scalar value that was decoded and rewritten in full
//! on every ingest batch: 16 bytes per document, per batch. Format 2 splits the
//! identity space into range chunks so that a mutation touches the header and
//! the chunks holding the affected identities, and nothing else.
//!
//! # Durable records
//!
//! The header stays at the historical manifest key (`HYPSMAN2`):
//!
//! ```text
//! magic(8) ++ u32 LE total_count ++ u32 LE chunk_count
//!          ++ chunk_count × ( u128 BE floor ++ u32 LE entry_count )
//! ```
//!
//! Each chunk lives under its own scalar key derived from its floor
//! (`HYPSCHK1`):
//!
//! ```text
//! magic(8) ++ u128 BE floor ++ u32 LE count ++ count × u128 BE identity
//! ```
//!
//! Floors are immutable lower bounds, strictly ascending across the header,
//! and chunk 0 always carries the sentinel floor `0` (an `ObjectId` is
//! nonzero, so the sentinel never collides). An identity belongs to the chunk
//! with the greatest floor not above it. Every chunk holds between one and
//! [`MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES`] identities, strictly
//! ascending, all at or above its floor and below the next floor. The header
//! total equals the sum of the chunk counts. An empty collection is exactly
//! the 16-byte header and owns no chunk record.
//!
//! # Why inserts only ever `SET`
//!
//! The point-resolved delta ingest path stages scalar `SET`s and nothing else,
//! so an insert can neither rename nor delete a chunk key. Because floors are
//! immutable and chunk 0 starts at zero, an insert only ever appends to an
//! existing chunk or splits an overfull chunk at its midpoint into a fresh
//! key. Chunk keys are only deleted by document deletion, which runs on the
//! materialized transaction path.
//!
//! # Chunk-count bound
//!
//! Maintenance keeps the adjacent-pair invariant: every two neighbouring
//! chunks together hold more than `T = MAX_ENTRIES / 2` identities. Pairs
//! `(0,1), (2,3), …` are disjoint, so `documents ≥ ⌊n/2⌋·(T+1)` and
//! `n ≤ 4·documents / MAX_ENTRIES + 2`, which is
//! [`MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS`] at the collection bound. Inserts
//! only raise counts; a midpoint split of a chunk holding between
//! `MAX_ENTRIES + 1` and `2·MAX_ENTRIES` identities leaves two halves of at
//! least `MAX_ENTRIES / 2`, so every pair involving a half still exceeds `T`.
//! A delete lowers exactly one count by one, so at most the pairs
//! `(i-1, i)` and `(i, i+1)` can fall to `T`; merging one such pair restores
//! the invariant, because the merged chunk with its outer neighbour holds at
//! least what the pre-delete pair held. An emptied chunk is removed (chunk 0
//! absorbs its successor instead, keeping the sentinel floor), and the pair
//! it leaves behind still exceeds `T` because the emptied chunk's neighbour
//! alone held at least `T`. Decoders enforce the derived bound, not the
//! invariant itself: the invariant governs amortization, the bound governs
//! integrity.
//!
//! # Legacy manifests
//!
//! `HYPSMAN1` values, including the bare 8-byte magic that provisioning used
//! to write for an empty collection, decode forever. Read paths serve them
//! as they are and never write. The first accepted mutation repacks the
//! identities into format 2 inside the same transaction, so a directory is
//! either wholly format 1 or wholly format 2 for a given collection.

use std::collections::{BTreeMap, BTreeSet};

use hyphae_native_runtime::NativeWriteBatch;

use crate::search::{
    MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS, corruption, limit_exceeded, manifest_key,
    map_runtime_error, storage_key,
};
use crate::{ObjectId, ProductError, ProductErrorCode, ProductSnapshot};

pub(crate) const LEGACY_MANIFEST_MAGIC: &[u8; 8] = b"HYPSMAN1";
pub(crate) const MANIFEST_HEADER_MAGIC: &[u8; 8] = b"HYPSMAN2";
pub(crate) const MANIFEST_CHUNK_MAGIC: &[u8; 8] = b"HYPSCHK1";
const MANIFEST_HEADER_BYTES: usize = 16;
const MANIFEST_HEADER_ENTRY_BYTES: usize = 20;
const MANIFEST_CHUNK_HEADER_BYTES: usize = 28;
const LEGACY_MANIFEST_HEADER_BYTES: usize = 12;
const IDENTITY_BYTES: usize = 16;
const CHUNK_KIND: u8 = b'C';
const SENTINEL_FLOOR: u128 = 0;

/// Maximum identities held by one manifest chunk record (16 KB of identities).
pub const MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES: usize = 1_024;
/// Adjacent chunks whose combined count is at or below this merge on delete.
const MANIFEST_CHUNK_MERGE_THRESHOLD: usize = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES / 2;
/// Maximum chunk records one manifest may own, derived from the adjacent-pair
/// invariant at the collection document bound.
pub const MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS: usize =
    4 * MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS / MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 2;

const _: () = assert!(
    MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES.is_multiple_of(2)
        && MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES >= 4
);

/// One chunk's position in the header: its floor and how many identities it
/// holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChunkDescriptor {
    pub(crate) floor: u128,
    pub(crate) count: usize,
}

/// Decoded manifest header.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManifestHeader {
    pub(crate) total: usize,
    pub(crate) chunks: Vec<ChunkDescriptor>,
}

/// One durable write the manifest state machine asks its caller to stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManifestWrite {
    Set(Vec<u8>),
    Delete,
}

/// The complete set of key writes one manifest mutation produces, in
/// ascending key order with every key present at most once.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManifestMutations {
    pub(crate) writes: Vec<(Vec<u8>, ManifestWrite)>,
}

/// Point read of one current scalar value in the caller's write context.
pub(crate) type ManifestRead<'r> = &'r dyn Fn(&[u8]) -> Result<Option<Vec<u8>>, ProductError>;

pub(crate) fn manifest_chunk_key(collection: ObjectId, floor: u128) -> Vec<u8> {
    storage_key(CHUNK_KIND, collection, Some(floor.to_be_bytes()))
}

fn catalog_conflict() -> ProductError {
    ProductError::from_code(ProductErrorCode::CatalogConflict)
}

fn read_u32_le(encoded: &[u8], offset: usize) -> Result<usize, ProductError> {
    let bytes: [u8; 4] = encoded
        .get(offset..offset.saturating_add(4))
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(corruption)?;
    usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| corruption())
}

fn read_u128_be(encoded: &[u8], offset: usize) -> Result<u128, ProductError> {
    let bytes: [u8; 16] = encoded
        .get(offset..offset.saturating_add(IDENTITY_BYTES))
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(corruption)?;
    Ok(u128::from_be_bytes(bytes))
}

fn encode_u32_le(value: usize) -> Result<[u8; 4], ProductError> {
    u32::try_from(value)
        .map(u32::to_le_bytes)
        .map_err(|_| limit_exceeded())
}

/// Decodes strictly ascending nonzero identities from a fixed-width tail.
fn decode_identities(tail: &[u8], count: usize) -> Result<Vec<ObjectId>, ProductError> {
    if tail.len() != count.saturating_mul(IDENTITY_BYTES) {
        return Err(corruption());
    }
    let mut identities = Vec::with_capacity(count);
    let mut previous: Option<ObjectId> = None;
    for raw in tail.chunks_exact(IDENTITY_BYTES) {
        let object_id = ObjectId::new(u128::from_be_bytes(
            raw.try_into().map_err(|_| corruption())?,
        ))
        .map_err(|_| corruption())?;
        if previous.is_some_and(|previous| previous >= object_id) {
            return Err(corruption());
        }
        previous = Some(object_id);
        identities.push(object_id);
    }
    Ok(identities)
}

fn validate_header(header: &ManifestHeader) -> Result<(), ProductError> {
    if header.chunks.len() > MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS
        || header.total > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS
    {
        return Err(corruption());
    }
    if header
        .chunks
        .first()
        .is_some_and(|first| first.floor != SENTINEL_FLOOR)
    {
        return Err(corruption());
    }
    let mut sum = 0_usize;
    let mut previous_floor: Option<u128> = None;
    for chunk in &header.chunks {
        if chunk.count == 0 || chunk.count > MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES {
            return Err(corruption());
        }
        if previous_floor.is_some_and(|previous| previous >= chunk.floor) {
            return Err(corruption());
        }
        previous_floor = Some(chunk.floor);
        sum = sum.saturating_add(chunk.count);
    }
    if sum != header.total {
        return Err(corruption());
    }
    Ok(())
}

pub(crate) fn encode_manifest_header(header: &ManifestHeader) -> Result<Vec<u8>, ProductError> {
    validate_header(header)?;
    let mut encoded = Vec::with_capacity(
        MANIFEST_HEADER_BYTES.saturating_add(
            header
                .chunks
                .len()
                .saturating_mul(MANIFEST_HEADER_ENTRY_BYTES),
        ),
    );
    encoded.extend_from_slice(MANIFEST_HEADER_MAGIC);
    encoded.extend_from_slice(&encode_u32_le(header.total)?);
    encoded.extend_from_slice(&encode_u32_le(header.chunks.len())?);
    for chunk in &header.chunks {
        encoded.extend_from_slice(&chunk.floor.to_be_bytes());
        encoded.extend_from_slice(&encode_u32_le(chunk.count)?);
    }
    Ok(encoded)
}

pub(crate) fn decode_manifest_header(encoded: &[u8]) -> Result<ManifestHeader, ProductError> {
    if encoded.len() < MANIFEST_HEADER_BYTES
        || encoded.get(..8) != Some(MANIFEST_HEADER_MAGIC.as_slice())
    {
        return Err(corruption());
    }
    let total = read_u32_le(encoded, 8)?;
    let chunk_count = read_u32_le(encoded, 12)?;
    if chunk_count > MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS
        || encoded.len()
            != MANIFEST_HEADER_BYTES
                .saturating_add(chunk_count.saturating_mul(MANIFEST_HEADER_ENTRY_BYTES))
    {
        return Err(corruption());
    }
    let mut chunks = Vec::with_capacity(chunk_count);
    for index in 0..chunk_count {
        let offset = MANIFEST_HEADER_BYTES.saturating_add(index * MANIFEST_HEADER_ENTRY_BYTES);
        chunks.push(ChunkDescriptor {
            floor: read_u128_be(encoded, offset)?,
            count: read_u32_le(encoded, offset.saturating_add(IDENTITY_BYTES))?,
        });
    }
    let header = ManifestHeader { total, chunks };
    validate_header(&header)?;
    Ok(header)
}

pub(crate) fn encode_manifest_chunk(
    floor: u128,
    identities: &[ObjectId],
) -> Result<Vec<u8>, ProductError> {
    if identities.is_empty() || identities.len() > MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES {
        return Err(corruption());
    }
    let mut encoded = Vec::with_capacity(
        MANIFEST_CHUNK_HEADER_BYTES.saturating_add(identities.len().saturating_mul(IDENTITY_BYTES)),
    );
    encoded.extend_from_slice(MANIFEST_CHUNK_MAGIC);
    encoded.extend_from_slice(&floor.to_be_bytes());
    encoded.extend_from_slice(&encode_u32_le(identities.len())?);
    for object_id in identities {
        encoded.extend_from_slice(&object_id.get().to_be_bytes());
    }
    Ok(encoded)
}

/// Decodes one chunk against the header entry that names it.
pub(crate) fn decode_manifest_chunk(
    encoded: &[u8],
    expected_floor: u128,
    next_floor: Option<u128>,
    expected_count: usize,
) -> Result<Vec<ObjectId>, ProductError> {
    if encoded.len() < MANIFEST_CHUNK_HEADER_BYTES
        || encoded.get(..8) != Some(MANIFEST_CHUNK_MAGIC.as_slice())
    {
        return Err(corruption());
    }
    if read_u128_be(encoded, 8)? != expected_floor {
        return Err(corruption());
    }
    let count = read_u32_le(encoded, 24)?;
    if count == 0 || count > MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES || count != expected_count {
        return Err(corruption());
    }
    let identities = decode_identities(&encoded[MANIFEST_CHUNK_HEADER_BYTES..], count)?;
    let (Some(first), Some(last)) = (identities.first(), identities.last()) else {
        return Err(corruption());
    };
    if first.get() < expected_floor || next_floor.is_some_and(|next| last.get() >= next) {
        return Err(corruption());
    }
    Ok(identities)
}

/// Decodes a format-1 manifest into its strictly ascending identities. The
/// bare magic is the empty collection.
pub(crate) fn decode_legacy_manifest(encoded: &[u8]) -> Result<Vec<ObjectId>, ProductError> {
    if encoded == LEGACY_MANIFEST_MAGIC {
        return Ok(Vec::new());
    }
    if encoded.len() < LEGACY_MANIFEST_HEADER_BYTES
        || encoded.get(..8) != Some(LEGACY_MANIFEST_MAGIC.as_slice())
    {
        return Err(corruption());
    }
    let count = read_u32_le(encoded, 8)?;
    if count > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS {
        return Err(corruption());
    }
    decode_identities(&encoded[LEGACY_MANIFEST_HEADER_BYTES..], count)
}

/// Encodes a format-1 manifest. Retained only so tests can seed legacy
/// directories; no production path writes format 1.
pub(crate) fn encode_legacy_manifest(identities: &[ObjectId]) -> Result<Vec<u8>, ProductError> {
    if identities.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(corruption());
    }
    let mut encoded = Vec::with_capacity(
        LEGACY_MANIFEST_HEADER_BYTES
            .saturating_add(identities.len().saturating_mul(IDENTITY_BYTES)),
    );
    encoded.extend_from_slice(LEGACY_MANIFEST_MAGIC);
    encoded.extend_from_slice(&encode_u32_le(identities.len())?);
    for object_id in identities {
        encoded.extend_from_slice(&object_id.get().to_be_bytes());
    }
    Ok(encoded)
}

/// Whether one durable manifest value is a format-1 record.
pub(crate) fn is_legacy_manifest(encoded: &[u8]) -> bool {
    encoded.get(..8) == Some(LEGACY_MANIFEST_MAGIC.as_slice())
}

/// Mutable manifest state for one write context. Chunks load lazily through
/// the caller's point read; `finish` returns exactly the writes the mutation
/// requires.
pub(crate) struct ManifestState {
    collection: ObjectId,
    header: ManifestHeader,
    loaded: BTreeMap<u128, Vec<ObjectId>>,
    dirty: BTreeSet<u128>,
    deleted: BTreeSet<u128>,
    header_dirty: bool,
}

impl ManifestState {
    /// Opens the current manifest of `collection`. A format-1 value is
    /// repacked into format 2 with every record marked dirty, so the caller's
    /// mutation upgrades the collection in the same transaction.
    pub(crate) fn open(collection: ObjectId, read: ManifestRead<'_>) -> Result<Self, ProductError> {
        let encoded = read(&manifest_key(collection))?.ok_or_else(corruption)?;
        if is_legacy_manifest(&encoded) {
            return Ok(Self::pack_sorted(
                collection,
                &decode_legacy_manifest(&encoded)?,
            ));
        }
        Ok(Self {
            collection,
            header: decode_manifest_header(&encoded)?,
            loaded: BTreeMap::new(),
            dirty: BTreeSet::new(),
            deleted: BTreeSet::new(),
            header_dirty: false,
        })
    }

    /// Packs strictly ascending identities into full chunks, every record
    /// dirty. Chunk `k` covers identities `[k·MAX, (k+1)·MAX)`; the first
    /// floor is the sentinel and every other floor is its chunk's first
    /// identity, which satisfies the adjacent-pair invariant.
    pub(crate) fn pack_sorted(collection: ObjectId, identities: &[ObjectId]) -> Self {
        let mut header = ManifestHeader::default();
        let mut loaded = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        for (index, chunk) in identities
            .chunks(MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES)
            .enumerate()
        {
            let floor = if index == 0 {
                SENTINEL_FLOOR
            } else {
                chunk.first().map_or(SENTINEL_FLOOR, |first| first.get())
            };
            header.chunks.push(ChunkDescriptor {
                floor,
                count: chunk.len(),
            });
            header.total = header.total.saturating_add(chunk.len());
            loaded.insert(floor, chunk.to_vec());
            dirty.insert(floor);
        }
        Self {
            collection,
            header,
            loaded,
            dirty,
            deleted: BTreeSet::new(),
            header_dirty: true,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.header.total == 0
    }

    fn chunk_index(&self, object_id: ObjectId) -> Option<usize> {
        let position = self
            .header
            .chunks
            .partition_point(|chunk| chunk.floor <= object_id.get());
        position.checked_sub(1)
    }

    fn load_chunk(
        &mut self,
        index: usize,
        read: ManifestRead<'_>,
    ) -> Result<&mut Vec<ObjectId>, ProductError> {
        let descriptor = *self.header.chunks.get(index).ok_or_else(corruption)?;
        if !self.loaded.contains_key(&descriptor.floor) {
            let encoded = read(&manifest_chunk_key(self.collection, descriptor.floor))?
                .ok_or_else(corruption)?;
            let next_floor = self.header.chunks.get(index + 1).map(|next| next.floor);
            let identities =
                decode_manifest_chunk(&encoded, descriptor.floor, next_floor, descriptor.count)?;
            self.loaded.insert(descriptor.floor, identities);
        }
        self.loaded
            .get_mut(&descriptor.floor)
            .ok_or_else(corruption)
    }

    /// Whether `object_id` is a member, loading at most one chunk.
    pub(crate) fn contains(
        &mut self,
        object_id: ObjectId,
        read: ManifestRead<'_>,
    ) -> Result<bool, ProductError> {
        let Some(index) = self.chunk_index(object_id) else {
            return Ok(false);
        };
        Ok(self
            .load_chunk(index, read)?
            .binary_search(&object_id)
            .is_ok())
    }

    /// Admits `identities` against the collection bound before loading any
    /// chunk, then inserts each one. A duplicate, in the collection or in the
    /// batch, is a catalog conflict.
    pub(crate) fn insert_batch(
        &mut self,
        identities: &[ObjectId],
        read: ManifestRead<'_>,
    ) -> Result<(), ProductError> {
        if self.header.total.saturating_add(identities.len())
            > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS
        {
            return Err(limit_exceeded());
        }
        for &object_id in identities {
            if self.header.chunks.is_empty() {
                self.header.chunks.push(ChunkDescriptor {
                    floor: SENTINEL_FLOOR,
                    count: 0,
                });
                self.loaded.insert(SENTINEL_FLOOR, Vec::new());
                self.deleted.remove(&SENTINEL_FLOOR);
            }
            let index = self.chunk_index(object_id).ok_or_else(corruption)?;
            let floor = self.header.chunks[index].floor;
            let chunk = self.load_chunk(index, read)?;
            match chunk.binary_search(&object_id) {
                Ok(_) => return Err(catalog_conflict()),
                Err(position) => chunk.insert(position, object_id),
            }
            self.header.chunks[index].count = self.header.chunks[index].count.saturating_add(1);
            self.header.total = self.header.total.saturating_add(1);
            self.dirty.insert(floor);
            self.header_dirty = true;
        }
        self.split_overfull()
    }

    /// Splits every chunk above the entry bound at its midpoint until none
    /// remains. The upper half's first identity becomes the new floor.
    fn split_overfull(&mut self) -> Result<(), ProductError> {
        let mut index = 0;
        while index < self.header.chunks.len() {
            let descriptor = self.header.chunks[index];
            if descriptor.count <= MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES {
                index += 1;
                continue;
            }
            let lower = self
                .loaded
                .get_mut(&descriptor.floor)
                .ok_or_else(corruption)?;
            let upper = lower.split_off(lower.len() / 2);
            let new_floor = upper.first().ok_or_else(corruption)?.get();
            self.header.chunks[index].count = lower.len();
            self.header.chunks.insert(
                index + 1,
                ChunkDescriptor {
                    floor: new_floor,
                    count: upper.len(),
                },
            );
            self.loaded.insert(new_floor, upper);
            self.deleted.remove(&new_floor);
            self.dirty.insert(new_floor);
            self.dirty.insert(descriptor.floor);
        }
        if self.header.chunks.len() > MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS {
            return Err(corruption());
        }
        Ok(())
    }

    /// Removes `object_id`, returning whether it was a member, and restores
    /// the adjacent-pair invariant with at most one merge.
    pub(crate) fn remove(
        &mut self,
        object_id: ObjectId,
        read: ManifestRead<'_>,
    ) -> Result<bool, ProductError> {
        let Some(index) = self.chunk_index(object_id) else {
            return Ok(false);
        };
        let floor = self.header.chunks[index].floor;
        let chunk = self.load_chunk(index, read)?;
        let Ok(position) = chunk.binary_search(&object_id) else {
            return Ok(false);
        };
        chunk.remove(position);
        self.header.chunks[index].count = self.header.chunks[index].count.saturating_sub(1);
        self.header.total = self.header.total.saturating_sub(1);
        self.dirty.insert(floor);
        self.header_dirty = true;
        self.rebalance_after_remove(index, read)?;
        Ok(true)
    }

    fn rebalance_after_remove(
        &mut self,
        index: usize,
        read: ManifestRead<'_>,
    ) -> Result<(), ProductError> {
        let count = self.header.chunks[index].count;
        if count == 0 {
            if index == 0 && self.header.chunks.len() > 1 {
                return self.merge_into(0, read);
            }
            return self.drop_chunk(index);
        }
        if let Some(next) = self.header.chunks.get(index + 1)
            && count.saturating_add(next.count) <= MANIFEST_CHUNK_MERGE_THRESHOLD
        {
            return self.merge_into(index, read);
        }
        if let Some(previous_index) = index.checked_sub(1)
            && self.header.chunks[previous_index]
                .count
                .saturating_add(count)
                <= MANIFEST_CHUNK_MERGE_THRESHOLD
        {
            return self.merge_into(previous_index, read);
        }
        Ok(())
    }

    /// Appends chunk `index + 1` to chunk `index` and deletes the upper key.
    fn merge_into(&mut self, index: usize, read: ManifestRead<'_>) -> Result<(), ProductError> {
        let lower_floor = self.header.chunks[index].floor;
        let upper_floor = self
            .header
            .chunks
            .get(index + 1)
            .ok_or_else(corruption)?
            .floor;
        self.load_chunk(index, read)?;
        self.load_chunk(index + 1, read)?;
        let upper = self.loaded.remove(&upper_floor).ok_or_else(corruption)?;
        let lower = self.loaded.get_mut(&lower_floor).ok_or_else(corruption)?;
        lower.extend(upper);
        self.header.chunks[index].count = lower.len();
        self.header.chunks.remove(index + 1);
        self.dirty.remove(&upper_floor);
        self.deleted.insert(upper_floor);
        self.dirty.insert(lower_floor);
        Ok(())
    }

    fn drop_chunk(&mut self, index: usize) -> Result<(), ProductError> {
        let descriptor = self.header.chunks.get(index).ok_or_else(corruption)?;
        if descriptor.count != 0 {
            return Err(corruption());
        }
        let floor = descriptor.floor;
        self.header.chunks.remove(index);
        self.loaded.remove(&floor);
        self.dirty.remove(&floor);
        self.deleted.insert(floor);
        Ok(())
    }

    /// Emits the writes this state accumulated, in ascending key order.
    pub(crate) fn finish(self) -> Result<ManifestMutations, ProductError> {
        if !self.dirty.is_disjoint(&self.deleted) {
            return Err(corruption());
        }
        let mut writes = BTreeMap::new();
        for floor in &self.dirty {
            let identities = self.loaded.get(floor).ok_or_else(corruption)?;
            writes.insert(
                manifest_chunk_key(self.collection, *floor),
                ManifestWrite::Set(encode_manifest_chunk(*floor, identities)?),
            );
        }
        for floor in &self.deleted {
            writes.insert(
                manifest_chunk_key(self.collection, *floor),
                ManifestWrite::Delete,
            );
        }
        if self.header_dirty {
            writes.insert(
                manifest_key(self.collection),
                ManifestWrite::Set(encode_manifest_header(&self.header)?),
            );
        }
        Ok(ManifestMutations {
            writes: writes.into_iter().collect(),
        })
    }

    /// Loads every chunk and returns all identities ascending.
    #[cfg(test)]
    pub(crate) fn into_sorted_ids(
        mut self,
        read: ManifestRead<'_>,
    ) -> Result<Vec<ObjectId>, ProductError> {
        let mut identities = Vec::with_capacity(self.header.total);
        for index in 0..self.header.chunks.len() {
            identities.extend_from_slice(self.load_chunk(index, read)?);
        }
        Ok(identities)
    }

    /// Writes that replace this manifest with its format-1 encoding: every
    /// chunk deleted, the header key rewritten. Test seeding only.
    pub(crate) fn legacy_rewrite(
        mut self,
        read: ManifestRead<'_>,
    ) -> Result<ManifestMutations, ProductError> {
        let floors: Vec<u128> = self.header.chunks.iter().map(|chunk| chunk.floor).collect();
        let mut identities = Vec::with_capacity(self.header.total);
        for index in 0..floors.len() {
            identities.extend_from_slice(self.load_chunk(index, read)?);
        }
        let mut writes = BTreeMap::new();
        for floor in floors {
            writes.insert(
                manifest_chunk_key(self.collection, floor),
                ManifestWrite::Delete,
            );
        }
        writes.insert(
            manifest_key(self.collection),
            ManifestWrite::Set(encode_legacy_manifest(&identities)?),
        );
        Ok(ManifestMutations {
            writes: writes.into_iter().collect(),
        })
    }
}

/// Stages manifest writes on a materialized write batch.
pub(crate) fn apply_manifest_mutations(
    batch: &mut NativeWriteBatch,
    mutations: ManifestMutations,
) -> Result<(), ProductError> {
    for (key, write) in mutations.writes {
        match write {
            ManifestWrite::Set(value) => batch.set(key, value, None).map_err(map_runtime_error)?,
            ManifestWrite::Delete => {
                batch.delete_structure(key).map_err(map_runtime_error)?;
            }
        }
    }
    Ok(())
}

enum ViewLayout {
    Legacy(Vec<ObjectId>),
    Chunked(ManifestHeader),
}

/// Read-only manifest over one immutable product snapshot. Chunks decode on
/// demand and are never cached across calls, so every answer is verified
/// against the snapshot bytes.
pub(crate) struct ManifestView<'s> {
    snapshot: &'s ProductSnapshot,
    collection: ObjectId,
    layout: ViewLayout,
}

impl<'s> ManifestView<'s> {
    pub(crate) fn open(
        snapshot: &'s ProductSnapshot,
        collection: ObjectId,
    ) -> Result<Self, ProductError> {
        let encoded = snapshot
            .structure_get_internal(&manifest_key(collection))
            .ok_or_else(corruption)?;
        let layout = if is_legacy_manifest(encoded) {
            ViewLayout::Legacy(decode_legacy_manifest(encoded)?)
        } else {
            ViewLayout::Chunked(decode_manifest_header(encoded)?)
        };
        Ok(Self {
            snapshot,
            collection,
            layout,
        })
    }

    pub(crate) fn total(&self) -> usize {
        match &self.layout {
            ViewLayout::Legacy(identities) => identities.len(),
            ViewLayout::Chunked(header) => header.total,
        }
    }

    pub(crate) fn is_legacy(&self) -> bool {
        matches!(self.layout, ViewLayout::Legacy(_))
    }

    pub(crate) fn header(&self) -> Option<&ManifestHeader> {
        match &self.layout {
            ViewLayout::Legacy(_) => None,
            ViewLayout::Chunked(header) => Some(header),
        }
    }

    fn chunk(&self, header: &ManifestHeader, index: usize) -> Result<Vec<ObjectId>, ProductError> {
        let descriptor = header.chunks.get(index).ok_or_else(corruption)?;
        let encoded = self
            .snapshot
            .structure_get_internal(&manifest_chunk_key(self.collection, descriptor.floor))
            .ok_or_else(corruption)?;
        decode_manifest_chunk(
            encoded,
            descriptor.floor,
            header.chunks.get(index + 1).map(|next| next.floor),
            descriptor.count,
        )
    }

    /// Every identity ascending.
    pub(crate) fn sorted_ids(&self) -> Result<Vec<ObjectId>, ProductError> {
        match &self.layout {
            ViewLayout::Legacy(identities) => Ok(identities.clone()),
            ViewLayout::Chunked(header) => {
                let mut identities = Vec::with_capacity(header.total);
                for index in 0..header.chunks.len() {
                    identities.extend(self.chunk(header, index)?);
                }
                if identities.len() != header.total {
                    return Err(corruption());
                }
                Ok(identities)
            }
        }
    }

    /// Every identity as an ordered set, bulk-built from the ascending chunks.
    pub(crate) fn materialize(&self) -> Result<BTreeSet<ObjectId>, ProductError> {
        Ok(self.sorted_ids()?.into_iter().collect())
    }

    /// Whether `object_id` is a member, decoding at most one chunk.
    pub(crate) fn contains(&self, object_id: ObjectId) -> Result<bool, ProductError> {
        match &self.layout {
            ViewLayout::Legacy(identities) => Ok(identities.binary_search(&object_id).is_ok()),
            ViewLayout::Chunked(header) => {
                let position = header
                    .chunks
                    .partition_point(|chunk| chunk.floor <= object_id.get());
                let Some(index) = position.checked_sub(1) else {
                    return Ok(false);
                };
                Ok(self.chunk(header, index)?.binary_search(&object_id).is_ok())
            }
        }
    }

    /// Up to `limit` identities strictly greater than `start_after`,
    /// ascending, decoding only the chunks the page spans.
    pub(crate) fn ids_after(
        &self,
        start_after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ProductError> {
        let admits = |object_id: &ObjectId| start_after.is_none_or(|start| *object_id > start);
        match &self.layout {
            ViewLayout::Legacy(identities) => Ok(identities
                .iter()
                .copied()
                .filter(admits)
                .take(limit)
                .collect()),
            ViewLayout::Chunked(header) => {
                let first = start_after.map_or(0, |start| {
                    header
                        .chunks
                        .partition_point(|chunk| chunk.floor <= start.get())
                        .saturating_sub(1)
                });
                let mut selected = Vec::with_capacity(limit.min(header.total));
                for index in first..header.chunks.len() {
                    if selected.len() >= limit {
                        break;
                    }
                    selected.extend(
                        self.chunk(header, index)?
                            .into_iter()
                            .filter(admits)
                            .take(limit.saturating_sub(selected.len())),
                    );
                }
                Ok(selected)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u128) -> Result<ObjectId, ProductError> {
        ObjectId::new(value).map_err(|_| corruption())
    }

    fn ids(range: std::ops::Range<u128>) -> Result<Vec<ObjectId>, ProductError> {
        range.map(id).collect()
    }

    fn collection() -> Result<ObjectId, ProductError> {
        id(52)
    }

    fn code(result: Result<impl std::fmt::Debug, ProductError>) -> Option<ProductErrorCode> {
        result.err().map(|error| error.code())
    }

    fn is_set(write: &ManifestWrite) -> bool {
        matches!(write, ManifestWrite::Set(_))
    }

    /// Encodes a header without validation, so tests can build invalid bytes.
    fn raw_header(header: &ManifestHeader) -> Result<Vec<u8>, ProductError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MANIFEST_HEADER_MAGIC);
        bytes.extend_from_slice(&encode_u32_le(header.total)?);
        bytes.extend_from_slice(&encode_u32_le(header.chunks.len())?);
        for chunk in &header.chunks {
            bytes.extend_from_slice(&chunk.floor.to_be_bytes());
            bytes.extend_from_slice(&encode_u32_le(chunk.count)?);
        }
        Ok(bytes)
    }

    /// In-memory store standing in for the three production read contexts.
    #[derive(Default)]
    struct Store {
        values: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    impl Store {
        fn empty_v2() -> Result<Self, ProductError> {
            let mut store = Self::default();
            store.values.insert(
                manifest_key(collection()?),
                encode_manifest_header(&ManifestHeader::default())?,
            );
            Ok(store)
        }

        fn legacy(identities: &[ObjectId]) -> Result<Self, ProductError> {
            let mut store = Self::default();
            store.values.insert(
                manifest_key(collection()?),
                encode_legacy_manifest(identities)?,
            );
            Ok(store)
        }

        fn read(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.values.get(key).cloned()
        }

        fn apply(&mut self, mutations: &ManifestMutations) {
            for (key, write) in &mutations.writes {
                match write {
                    ManifestWrite::Set(value) => {
                        self.values.insert(key.clone(), value.clone());
                    }
                    ManifestWrite::Delete => {
                        self.values.remove(key);
                    }
                }
            }
        }

        fn header(&self) -> Result<ManifestHeader, ProductError> {
            decode_manifest_header(
                self.values
                    .get(&manifest_key(collection()?))
                    .ok_or_else(corruption)?,
            )
        }

        fn open(&self) -> Result<ManifestState, ProductError> {
            ManifestState::open(collection()?, &|key| Ok(self.read(key)))
        }

        fn insert(&mut self, identities: &[ObjectId]) -> Result<ManifestMutations, ProductError> {
            let mutations = {
                let read = |key: &[u8]| Ok(self.read(key));
                let mut state = ManifestState::open(collection()?, &read)?;
                state.insert_batch(identities, &read)?;
                state.finish()?
            };
            self.apply(&mutations);
            Ok(mutations)
        }

        fn remove(
            &mut self,
            object_id: ObjectId,
        ) -> Result<(bool, ManifestMutations), ProductError> {
            let (removed, mutations) = {
                let read = |key: &[u8]| Ok(self.read(key));
                let mut state = ManifestState::open(collection()?, &read)?;
                let removed = state.remove(object_id, &read)?;
                (removed, state.finish()?)
            };
            self.apply(&mutations);
            Ok((removed, mutations))
        }

        fn sorted_ids(&self) -> Result<Vec<ObjectId>, ProductError> {
            self.open()?.into_sorted_ids(&|key| Ok(self.read(key)))
        }

        /// Checks the adjacent-pair invariant and the derived chunk bound.
        fn assert_invariant(&self) -> Result<(), ProductError> {
            let header = self.header()?;
            assert!(header.chunks.len() <= MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS);
            for pair in header.chunks.windows(2) {
                assert!(
                    pair[0].count + pair[1].count > MANIFEST_CHUNK_MERGE_THRESHOLD,
                    "adjacent pair {pair:?} violates the invariant"
                );
            }
            Ok(())
        }
    }

    #[test]
    fn header_and_chunk_round_trip() -> Result<(), ProductError> {
        let empty = ManifestHeader::default();
        let encoded = encode_manifest_header(&empty)?;
        assert_eq!(encoded.len(), MANIFEST_HEADER_BYTES);
        assert_eq!(decode_manifest_header(&encoded)?, empty);

        let header = ManifestHeader {
            total: 1_024 + 3 + 1,
            chunks: vec![
                ChunkDescriptor {
                    floor: 0,
                    count: 1_024,
                },
                ChunkDescriptor {
                    floor: 5_000,
                    count: 3,
                },
                ChunkDescriptor {
                    floor: 9_000,
                    count: 1,
                },
            ],
        };
        let encoded = encode_manifest_header(&header)?;
        assert_eq!(
            encoded.len(),
            MANIFEST_HEADER_BYTES + 3 * MANIFEST_HEADER_ENTRY_BYTES
        );
        assert_eq!(decode_manifest_header(&encoded)?, header);

        let one = encode_manifest_chunk(0, &ids(7..8)?)?;
        assert_eq!(decode_manifest_chunk(&one, 0, Some(9), 1)?, ids(7..8)?);
        let full_count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES;
        let full = encode_manifest_chunk(10, &ids(10..10 + full_count as u128)?)?;
        assert_eq!(
            full.len(),
            MANIFEST_CHUNK_HEADER_BYTES + full_count * IDENTITY_BYTES
        );
        assert_eq!(
            decode_manifest_chunk(&full, 10, None, full_count)?.len(),
            full_count
        );
        Ok(())
    }

    #[test]
    fn header_decode_fails_closed() -> Result<(), ProductError> {
        let valid = ManifestHeader {
            total: 3,
            chunks: vec![
                ChunkDescriptor { floor: 0, count: 2 },
                ChunkDescriptor {
                    floor: 40,
                    count: 1,
                },
            ],
        };
        let encoded = encode_manifest_header(&valid)?;
        let mut wrong_magic = encoded.clone();
        wrong_magic[..8].copy_from_slice(b"HYPSMAN9");
        assert_eq!(
            code(decode_manifest_header(&wrong_magic)),
            Some(ProductErrorCode::Corruption)
        );
        let mut short = encoded.clone();
        short.pop();
        assert_eq!(
            code(decode_manifest_header(&short)),
            Some(ProductErrorCode::Corruption)
        );
        let mut long = encoded.clone();
        long.push(0);
        assert_eq!(
            code(decode_manifest_header(&long)),
            Some(ProductErrorCode::Corruption)
        );
        assert_eq!(
            code(decode_manifest_header(LEGACY_MANIFEST_MAGIC)),
            Some(ProductErrorCode::Corruption)
        );

        let mut non_ascending = valid.clone();
        non_ascending.chunks[1].floor = 0;
        let mut bad_first_floor = valid.clone();
        bad_first_floor.chunks[0].floor = 1;
        let mut zero_count = valid.clone();
        zero_count.chunks[1].count = 0;
        zero_count.total = 2;
        let mut over_entries = valid.clone();
        over_entries.chunks[0].count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 1;
        over_entries.total = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 2;
        let mut bad_sum = valid.clone();
        bad_sum.total = 4;
        let mut too_many_chunks = ManifestHeader::default();
        for index in 0..=MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS {
            too_many_chunks.chunks.push(ChunkDescriptor {
                floor: (index as u128) * 2,
                count: 1,
            });
            too_many_chunks.total += 1;
        }
        for case in [
            non_ascending,
            bad_first_floor,
            zero_count,
            over_entries,
            bad_sum,
            too_many_chunks,
        ] {
            assert_eq!(
                code(decode_manifest_header(&raw_header(&case)?)),
                Some(ProductErrorCode::Corruption),
                "{case:?}"
            );
            assert_eq!(
                code(encode_manifest_header(&case)),
                Some(ProductErrorCode::Corruption)
            );
        }
        Ok(())
    }

    #[test]
    fn chunk_decode_fails_closed() -> Result<(), ProductError> {
        let identities = ids(100..104)?;
        let encoded = encode_manifest_chunk(100, &identities)?;
        assert_eq!(
            decode_manifest_chunk(&encoded, 100, Some(200), 4)?,
            identities
        );

        let mut wrong_magic = encoded.clone();
        wrong_magic[..8].copy_from_slice(b"HYPSCHK9");
        assert_eq!(
            code(decode_manifest_chunk(&wrong_magic, 100, Some(200), 4)),
            Some(ProductErrorCode::Corruption)
        );
        let mut short = encoded.clone();
        short.pop();
        assert_eq!(
            code(decode_manifest_chunk(&short, 100, Some(200), 4)),
            Some(ProductErrorCode::Corruption)
        );
        assert_eq!(
            code(decode_manifest_chunk(&encoded, 99, Some(200), 4)),
            Some(ProductErrorCode::Corruption),
            "stored floor must match the header floor"
        );
        assert_eq!(
            code(decode_manifest_chunk(&encoded, 100, Some(200), 3)),
            Some(ProductErrorCode::Corruption),
            "count must match the header count"
        );
        assert_eq!(
            code(decode_manifest_chunk(&encoded, 101, Some(200), 4)),
            Some(ProductErrorCode::Corruption),
            "first identity below the floor"
        );
        assert_eq!(
            code(decode_manifest_chunk(&encoded, 100, Some(103), 4)),
            Some(ProductErrorCode::Corruption),
            "last identity at or above the next floor"
        );

        let first = MANIFEST_CHUNK_HEADER_BYTES;
        let second = first + IDENTITY_BYTES;
        let mut duplicate = encoded.clone();
        duplicate[second..second + IDENTITY_BYTES].copy_from_slice(&100_u128.to_be_bytes());
        assert_eq!(
            code(decode_manifest_chunk(&duplicate, 100, Some(200), 4)),
            Some(ProductErrorCode::Corruption)
        );
        let mut zero = encoded.clone();
        zero[first..second].copy_from_slice(&0_u128.to_be_bytes());
        assert_eq!(
            code(decode_manifest_chunk(&zero, 100, Some(200), 4)),
            Some(ProductErrorCode::Corruption)
        );
        assert_eq!(
            code(encode_manifest_chunk(0, &[])),
            Some(ProductErrorCode::Corruption)
        );
        Ok(())
    }

    #[test]
    fn legacy_manifest_decodes_and_bare_magic_is_empty() -> Result<(), ProductError> {
        assert!(decode_legacy_manifest(LEGACY_MANIFEST_MAGIC)?.is_empty());
        let identities = ids(1..40)?;
        let encoded = encode_legacy_manifest(&identities)?;
        assert_eq!(
            encoded.len(),
            LEGACY_MANIFEST_HEADER_BYTES + 39 * IDENTITY_BYTES
        );
        assert_eq!(decode_legacy_manifest(&encoded)?, identities);
        assert!(is_legacy_manifest(&encoded));
        assert!(!is_legacy_manifest(&encode_manifest_header(
            &ManifestHeader::default()
        )?));
        let mut truncated = encoded.clone();
        truncated.pop();
        assert_eq!(
            code(decode_legacy_manifest(&truncated)),
            Some(ProductErrorCode::Corruption)
        );
        Ok(())
    }

    #[test]
    fn legacy_upgrade_packing_is_deterministic() -> Result<(), ProductError> {
        let identities = ids(1_000..3_600)?;
        let mut store = Store::legacy(&identities)?;
        let state = store.open()?;
        assert!(!state.is_empty());
        let mutations = state.finish()?;
        assert_eq!(mutations.writes.len(), 4);
        assert!(mutations.writes.iter().all(|(_, write)| is_set(write)));
        assert!(
            mutations
                .writes
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );
        store.apply(&mutations);
        let header = store.header()?;
        assert_eq!(header.total, 2_600);
        assert_eq!(
            header.chunks,
            vec![
                ChunkDescriptor {
                    floor: 0,
                    count: 1_024
                },
                ChunkDescriptor {
                    floor: 2_024,
                    count: 1_024
                },
                ChunkDescriptor {
                    floor: 3_048,
                    count: 552
                },
            ]
        );
        assert_eq!(store.sorted_ids()?, identities);
        store.assert_invariant()?;

        // An empty legacy manifest upgrades to the empty header alone.
        let mut empty = Store::default();
        empty
            .values
            .insert(manifest_key(collection()?), LEGACY_MANIFEST_MAGIC.to_vec());
        let state = empty.open()?;
        assert!(state.is_empty());
        let mutations = state.finish()?;
        assert_eq!(mutations.writes.len(), 1);
        empty.apply(&mutations);
        assert_eq!(empty.header()?, ManifestHeader::default());
        Ok(())
    }

    #[test]
    fn legacy_rewrite_round_trips_through_format_two() -> Result<(), ProductError> {
        let identities = ids(1..1_300)?;
        let mut store = Store::empty_v2()?;
        store.insert(&identities)?;
        assert_eq!(store.header()?.chunks.len(), 2);
        let mutations = store.open()?.legacy_rewrite(&|key| Ok(store.read(key)))?;
        store.apply(&mutations);
        assert_eq!(store.values.len(), 1, "only the header key survives");
        let legacy = store
            .values
            .get(&manifest_key(collection()?))
            .ok_or_else(corruption)?;
        assert!(is_legacy_manifest(legacy));
        assert_eq!(decode_legacy_manifest(legacy)?, identities);
        assert_eq!(store.sorted_ids()?, identities);
        Ok(())
    }

    #[test]
    fn insert_split_is_deterministic_and_set_only() -> Result<(), ProductError> {
        let run = || -> Result<(Store, Vec<ManifestMutations>), ProductError> {
            let mut store = Store::empty_v2()?;
            let emitted = vec![
                store.insert(&ids(1..1_025)?)?,
                store.insert(&ids(1_025..1_281)?)?,
            ];
            Ok((store, emitted))
        };
        let (store, first) = run()?;
        let (_, second) = run()?;
        assert_eq!(first, second, "the same sequence yields identical writes");
        assert!(
            first
                .iter()
                .flat_map(|mutations| &mutations.writes)
                .all(|(_, write)| is_set(write))
        );
        let header = store.header()?;
        assert_eq!(header.total, 1_280);
        assert_eq!(
            header.chunks,
            vec![
                ChunkDescriptor {
                    floor: 0,
                    count: 640
                },
                ChunkDescriptor {
                    floor: 641,
                    count: 640
                },
            ]
        );
        assert_eq!(store.sorted_ids()?, ids(1..1_281)?);
        store.assert_invariant()?;
        Ok(())
    }

    #[test]
    fn insert_below_first_identity_stays_in_chunk_zero() -> Result<(), ProductError> {
        let mut store = Store::empty_v2()?;
        store.insert(&ids(500..1_524)?)?;
        store.insert(&ids(2_000..2_010)?)?;
        let before = store.header()?;
        assert_eq!(before.chunks.len(), 2);
        let mutations = store.insert(&ids(1..2)?)?;
        assert_eq!(mutations.writes.len(), 2, "one chunk and the header");
        assert!(mutations.writes.iter().all(|(_, write)| is_set(write)));
        let after = store.header()?;
        assert_eq!(after.chunks.len(), before.chunks.len());
        assert_eq!(after.chunks[0].floor, 0);
        assert_eq!(after.chunks[0].count, before.chunks[0].count + 1);
        assert_eq!(after.chunks[1], before.chunks[1]);
        assert!(store.sorted_ids()?.starts_with(&ids(1..2)?));
        Ok(())
    }

    #[test]
    fn delete_merges_when_a_pair_reaches_the_threshold() -> Result<(), ProductError> {
        let mut store = Store::empty_v2()?;
        store.insert(&ids(1..1_025)?)?;
        store.insert(&ids(1_025..1_027)?)?;
        assert_eq!(
            store
                .header()?
                .chunks
                .iter()
                .map(|chunk| chunk.count)
                .collect::<Vec<_>>(),
            vec![513, 513]
        );
        for value in 514..1_026 {
            let (removed, _) = store.remove(id(value)?)?;
            assert!(removed);
            store.assert_invariant()?;
        }
        assert_eq!(
            store
                .header()?
                .chunks
                .iter()
                .map(|chunk| chunk.count)
                .collect::<Vec<_>>(),
            vec![513, 1]
        );
        // 512 + 1 still exceeds the threshold: no merge yet.
        let (removed, mutations) = store.remove(id(1)?)?;
        assert!(removed);
        assert!(mutations.writes.iter().all(|(_, write)| is_set(write)));
        assert_eq!(store.header()?.chunks.len(), 2);
        // 511 + 1 reaches it: one merge, one deleted key.
        let (removed, mutations) = store.remove(id(2)?)?;
        assert!(removed);
        assert_eq!(
            mutations
                .writes
                .iter()
                .filter(|(_, write)| !is_set(write))
                .count(),
            1
        );
        let header = store.header()?;
        assert_eq!(header.chunks.len(), 1);
        assert_eq!(header.total, 512);
        assert_eq!(store.values.len(), 2, "header and one chunk");
        store.assert_invariant()?;
        Ok(())
    }

    #[test]
    fn delete_removes_empty_chunks() -> Result<(), ProductError> {
        // Emptying a middle chunk deletes only that key.
        let mut store = Store::empty_v2()?;
        store.insert(&ids(1..3_073)?)?;
        let header = store.header()?;
        assert_eq!(header.chunks.len(), 4);
        let middle = header.chunks[1];
        let all = store.sorted_ids()?;
        let middle_ids =
            all[header.chunks[0].count..header.chunks[0].count + middle.count].to_vec();
        let (last, rest) = middle_ids.split_last().ok_or_else(corruption)?;
        for object_id in rest {
            store.remove(*object_id)?;
            store.assert_invariant()?;
        }
        assert!(
            store
                .header()?
                .chunks
                .iter()
                .any(|chunk| chunk.floor == middle.floor)
        );
        let (_, mutations) = store.remove(*last)?;
        let deleted_key = manifest_chunk_key(collection()?, middle.floor);
        assert!(
            mutations
                .writes
                .iter()
                .any(|(key, write)| *key == deleted_key && !is_set(write))
        );
        assert!(
            !store
                .header()?
                .chunks
                .iter()
                .any(|chunk| chunk.floor == middle.floor)
        );
        assert_eq!(store.header()?.total, 3_072 - middle.count);
        store.assert_invariant()?;

        // Emptying chunk 0 absorbs its successor and keeps the sentinel floor.
        let mut store = Store::empty_v2()?;
        store.insert(&ids(1..2_049)?)?;
        let header = store.header()?;
        assert_eq!(header.chunks.len(), 2);
        let first_count = header.chunks[0].count;
        let first_count_id = u128::try_from(first_count).map_err(|_| corruption())?;
        for value in 1..=first_count_id {
            store.remove(id(value)?)?;
            store.assert_invariant()?;
        }
        let header = store.header()?;
        assert_eq!(header.chunks.len(), 1);
        assert_eq!(header.chunks[0].floor, 0);
        assert_eq!(header.total, 2_048 - first_count);
        assert_eq!(store.sorted_ids()?, ids(first_count_id + 1..2_049)?);
        assert_eq!(store.values.len(), 2, "header and the absorbing chunk");

        // Emptying the only chunk leaves the bare header.
        let mut store = Store::empty_v2()?;
        store.insert(&ids(7..9)?)?;
        store.remove(id(7)?)?;
        store.remove(id(8)?)?;
        assert_eq!(store.header()?, ManifestHeader::default());
        assert_eq!(store.values.len(), 1);
        Ok(())
    }

    #[test]
    fn adjacent_pair_invariant_holds_under_random_sequence() -> Result<(), ProductError> {
        let mut store = Store::empty_v2()?;
        let mut members = BTreeSet::new();
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            // Sparse identities over a wide range keep the floors irregular.
            let candidate = id(u128::from(state % 50_000) * 7_919 + 1)?;
            if state.is_multiple_of(3) && members.contains(&candidate) {
                let (removed, _) = store.remove(candidate)?;
                assert!(removed);
                members.remove(&candidate);
            } else if !members.contains(&candidate) {
                store.insert(&[candidate])?;
                members.insert(candidate);
            }
            store.assert_invariant()?;
        }
        assert_eq!(store.header()?.total, members.len());
        assert_eq!(store.sorted_ids()?, members.into_iter().collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn remove_absent_is_false_and_duplicate_is_conflict() -> Result<(), ProductError> {
        let mut store = Store::empty_v2()?;
        let (removed, mutations) = store.remove(id(5)?)?;
        assert!(!removed);
        assert!(mutations.writes.is_empty());
        store.insert(&ids(1..10)?)?;
        let (removed, mutations) = store.remove(id(50)?)?;
        assert!(!removed);
        assert!(mutations.writes.is_empty());
        assert_eq!(
            code(store.insert(&ids(5..6)?)),
            Some(ProductErrorCode::CatalogConflict)
        );
        assert_eq!(
            code(store.insert(&[id(20)?, id(20)?])),
            Some(ProductErrorCode::CatalogConflict),
            "a duplicate within the batch is a conflict"
        );
        assert_eq!(store.header()?.total, 9, "a rejected batch leaves no trace");
        Ok(())
    }

    #[test]
    fn contains_matches_membership_across_chunks() -> Result<(), ProductError> {
        let mut store = Store::empty_v2()?;
        store.insert(&ids(10..2_060)?)?;
        assert!(store.header()?.chunks.len() >= 2);
        let read = |key: &[u8]| Ok(store.read(key));
        let mut state = ManifestState::open(collection()?, &read)?;
        assert!(state.contains(id(10)?, &read)?);
        assert!(state.contains(id(2_059)?, &read)?);
        assert!(!state.contains(id(9)?, &read)?);
        assert!(!state.contains(id(2_060)?, &read)?);
        let boundary = store.header()?.chunks[1].floor;
        assert!(state.contains(id(boundary)?, &read)?);
        assert!(state.contains(id(boundary - 1)?, &read)?);
        Ok(())
    }

    #[test]
    fn insert_rejects_the_bound_before_loading_chunks() -> Result<(), ProductError> {
        let bound =
            u128::try_from(MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS).map_err(|_| corruption())?;
        let identities = ids(1..bound + 1)?;
        let mut store = Store::default();
        store.apply(&ManifestState::pack_sorted(collection()?, &identities).finish()?);
        let header = store.header()?;
        assert_eq!(header.total, MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS);
        assert!(header.chunks.len() <= MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS);
        assert_eq!(
            code(store.insert(&ids(9_000_000..9_000_001)?)),
            Some(ProductErrorCode::LimitExceeded)
        );
        assert_eq!(
            code(store.insert(&[])),
            None,
            "an empty batch at the bound is admitted"
        );
        assert_eq!(
            store.header()?.total,
            MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS
        );
        Ok(())
    }
}
