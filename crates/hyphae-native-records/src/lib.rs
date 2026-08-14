// SPDX-License-Identifier: AGPL-3.0-only

//! Canonical committed-row and immutable blob-reference codecs.

use hyphae_native_types::{BlobId, Csn, PageId, RowId};
use thiserror::Error;

mod tuple;

pub use tuple::{ROW_TUPLE_HEADER_SIZE, RowTuple, RowTupleView};

/// Fixed committed-row header size before null bits and offsets.
pub const ROW_HEADER_SIZE: usize = 40;
/// Encoded immutable blob-reference size.
pub const BLOB_REFERENCE_SIZE: usize = 56;
/// Encoded B+tree locator for one latest row-version page.
pub const ROW_VERSION_POINTER_SIZE: usize = 16;

const TOMBSTONE_FLAG: u16 = 1;
const OPEN_END_CSN: u64 = u64::MAX;
const ROW_VERSION_POINTER_MAGIC: &[u8; 8] = b"HYROWP01";

/// Row or blob-reference codec failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RecordError {
    /// Encoded bytes are shorter than the fixed header.
    #[error("native row record is truncated")]
    Truncated,
    /// The total-length field differs from the supplied bytes.
    #[error("native row record length does not match its canonical total")]
    LengthMismatch,
    /// Flags not defined by v1 are nonzero.
    #[error("native row record contains unknown flags")]
    UnknownFlags,
    /// A regular row has no catalog columns.
    #[error("native regular row must contain at least one column")]
    EmptyRegularRow,
    /// Column count cannot be represented canonically.
    #[error("native row column count exceeds u16")]
    ColumnCountOverflow,
    /// Encoded length or offset cannot be represented canonically.
    #[error("native row byte length exceeds u32")]
    LengthOverflow,
    /// Row identity or CSN is zero.
    #[error("native row contains a zero identity or CSN")]
    ZeroIdentity,
    /// The MVCC interval is empty, inverted, or collides with the open marker.
    #[error("native row MVCC interval is invalid")]
    InvalidVersionWindow,
    /// Tombstone bytes contain columns or values.
    #[error("native row tombstone contains column data")]
    InvalidTombstone,
    /// Null bitmap contains noncanonical unused bits.
    #[error("native row null bitmap has nonzero unused bits")]
    NoncanonicalNullBitmap,
    /// Value offsets are not exact, monotonic, or in bounds.
    #[error("native row value offsets are invalid")]
    InvalidOffsets,
    /// A null value consumes physical value bytes.
    #[error("native row null value has nonempty physical bytes")]
    NullHasBytes,
    /// Blob reference does not have its exact fixed length.
    #[error("native blob reference must be exactly {BLOB_REFERENCE_SIZE} bytes")]
    InvalidBlobReferenceLength,
    /// Row-version pointer has the wrong length, magic, or page identity.
    #[error("native row-version pointer is invalid")]
    InvalidRowVersionPointer,
    /// A typed row tuple has invalid magic or reserved header bytes.
    #[error("native typed row tuple header is invalid")]
    InvalidTupleHeader,
}

/// Borrowed logical value for one catalog-ordered row column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnValueRef<'value> {
    /// SQL null.
    Null,
    /// Canonical catalog-typed bytes, including a possible empty value.
    Bytes(&'value [u8]),
}

/// One immutable committed MVCC row version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowRecord {
    row_id: RowId,
    begin_csn: Csn,
    end_csn: Option<Csn>,
    tombstone: bool,
    values: Vec<Option<Vec<u8>>>,
}

/// Fixed B+tree value pointing to the latest immutable row-version page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowVersionPointer {
    /// Latest version-chain page.
    pub page_id: PageId,
}

impl RowVersionPointer {
    /// Encodes the exact 16-byte row-version pointer.
    pub fn encode(self) -> [u8; ROW_VERSION_POINTER_SIZE] {
        let mut encoded = [0_u8; ROW_VERSION_POINTER_SIZE];
        encoded[0..8].copy_from_slice(ROW_VERSION_POINTER_MAGIC);
        encoded[8..16].copy_from_slice(&self.page_id.get().to_le_bytes());
        encoded
    }

    /// Decodes one exact row-version pointer.
    ///
    /// # Errors
    ///
    /// Returns an error for a wrong length, magic, or zero page identity.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() != ROW_VERSION_POINTER_SIZE
            || bytes.get(..8) != Some(ROW_VERSION_POINTER_MAGIC.as_slice())
        {
            return Err(RecordError::InvalidRowVersionPointer);
        }
        let page_id = PageId::new(read_u64(&bytes[8..16]))
            .map_err(|_| RecordError::InvalidRowVersionPointer)?;
        Ok(Self { page_id })
    }
}

/// Borrowed validated view over one canonical MVCC row record.
#[derive(Clone, Copy, Debug)]
pub struct RowRecordView<'record> {
    bytes: &'record [u8],
    row_id: RowId,
    begin_csn: Csn,
    end_csn: Option<Csn>,
    tombstone: bool,
    column_count: usize,
    offsets_start: usize,
}

impl<'record> RowRecordView<'record> {
    /// Decodes and validates a row without allocating owned column values.
    ///
    /// # Errors
    ///
    /// Returns an error for any malformed length, flag, identity, MVCC window,
    /// null bit, offset, or tombstone representation.
    pub fn decode(bytes: &'record [u8]) -> Result<Self, RecordError> {
        if bytes.len() < ROW_HEADER_SIZE {
            return Err(RecordError::Truncated);
        }
        let total_length =
            usize::try_from(read_u32(&bytes[0..4])).map_err(|_| RecordError::LengthOverflow)?;
        if total_length != bytes.len() {
            return Err(RecordError::LengthMismatch);
        }
        let flags = read_u16(&bytes[4..6]);
        if flags & !TOMBSTONE_FLAG != 0 {
            return Err(RecordError::UnknownFlags);
        }
        let column_count = usize::from(read_u16(&bytes[6..8]));
        let row_id = RowId::new(read_u128(&bytes[8..24])).map_err(|_| RecordError::ZeroIdentity)?;
        let begin_csn =
            Csn::new(read_u64(&bytes[24..32])).map_err(|_| RecordError::ZeroIdentity)?;
        let raw_end = read_u64(&bytes[32..40]);
        let end_csn = if raw_end == OPEN_END_CSN {
            None
        } else {
            Some(Csn::new(raw_end).map_err(|_| RecordError::ZeroIdentity)?)
        };
        validate_window(begin_csn, end_csn)?;

        let tombstone = flags & TOMBSTONE_FLAG != 0;
        if tombstone {
            if column_count != 0 || bytes.len() != ROW_HEADER_SIZE {
                return Err(RecordError::InvalidTombstone);
            }
            return Ok(Self {
                bytes,
                row_id,
                begin_csn,
                end_csn,
                tombstone,
                column_count,
                offsets_start: 0,
            });
        }
        if column_count == 0 {
            return Err(RecordError::EmptyRegularRow);
        }
        let offsets_start = validate_regular_layout(bytes, column_count)?;

        Ok(Self {
            bytes,
            row_id,
            begin_csn,
            end_csn,
            tombstone,
            column_count,
            offsets_start,
        })
    }

    /// Returns the stable row identity.
    pub const fn row_id(self) -> RowId {
        self.row_id
    }

    /// Returns the first CSN where this version is visible.
    pub const fn begin_csn(self) -> Csn {
        self.begin_csn
    }

    /// Returns the first CSN where this version is no longer visible.
    pub const fn end_csn(self) -> Option<Csn> {
        self.end_csn
    }

    /// Returns whether this version is a deletion marker.
    pub const fn is_tombstone(self) -> bool {
        self.tombstone
    }

    /// Returns the catalog column count.
    pub const fn column_count(self) -> usize {
        self.column_count
    }

    /// Returns one borrowed catalog-ordered logical column value.
    pub fn value(self, index: usize) -> Option<ColumnValueRef<'record>> {
        if self.tombstone || index >= self.column_count {
            return None;
        }
        let is_null = self.bytes[ROW_HEADER_SIZE + index / 8] & (1_u8 << (index % 8)) != 0;
        if is_null {
            return Some(ColumnValueRef::Null);
        }
        let offset = |position: usize| -> Option<usize> {
            let start = self.offsets_start + position * 4;
            usize::try_from(read_u32(self.bytes.get(start..start + 4)?)).ok()
        };
        let start = offset(index)?;
        let end = offset(index + 1)?;
        self.bytes.get(start..end).map(ColumnValueRef::Bytes)
    }

    /// Returns whether this version is visible at one snapshot CSN.
    pub fn is_visible_at(self, visible_csn: Option<Csn>) -> bool {
        let Some(visible) = visible_csn else {
            return false;
        };
        self.begin_csn <= visible && self.end_csn.is_none_or(|end| visible < end)
    }

    /// Returns the exact validated canonical bytes.
    pub const fn bytes(self) -> &'record [u8] {
        self.bytes
    }
}

fn validate_regular_layout(bytes: &[u8], column_count: usize) -> Result<usize, RecordError> {
    let null_bytes = null_bitmap_length(column_count);
    let offsets_bytes = column_count
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or(RecordError::LengthOverflow)?;
    let offsets_start = ROW_HEADER_SIZE
        .checked_add(null_bytes)
        .ok_or(RecordError::LengthOverflow)?;
    let value_start = offsets_start
        .checked_add(offsets_bytes)
        .ok_or(RecordError::LengthOverflow)?;
    if value_start > bytes.len() {
        return Err(RecordError::Truncated);
    }
    if !column_count.is_multiple_of(8) {
        let used_bits = column_count % 8;
        let invalid_mask = !((1_u8 << used_bits) - 1);
        if bytes[ROW_HEADER_SIZE + null_bytes - 1] & invalid_mask != 0 {
            return Err(RecordError::NoncanonicalNullBitmap);
        }
    }

    let mut previous_offset = None;
    for offset_index in 0..=column_count {
        let start = offsets_start
            .checked_add(
                offset_index
                    .checked_mul(4)
                    .ok_or(RecordError::LengthOverflow)?,
            )
            .ok_or(RecordError::LengthOverflow)?;
        let encoded = bytes.get(start..start + 4).ok_or(RecordError::Truncated)?;
        let offset = usize::try_from(read_u32(encoded)).map_err(|_| RecordError::LengthOverflow)?;
        if offset < value_start || offset > bytes.len() {
            return Err(RecordError::InvalidOffsets);
        }
        if offset_index == 0 && offset != value_start {
            return Err(RecordError::InvalidOffsets);
        }
        if let Some(previous) = previous_offset {
            if previous > offset {
                return Err(RecordError::InvalidOffsets);
            }
            let column = offset_index - 1;
            let is_null = bytes[ROW_HEADER_SIZE + column / 8] & (1_u8 << (column % 8)) != 0;
            if is_null && previous != offset {
                return Err(RecordError::NullHasBytes);
            }
        }
        previous_offset = Some(offset);
    }
    if previous_offset != Some(bytes.len()) {
        return Err(RecordError::InvalidOffsets);
    }
    Ok(offsets_start)
}

impl RowRecord {
    /// Constructs one regular committed row version.
    ///
    /// # Errors
    ///
    /// Returns an error for no columns, too many columns, or an invalid MVCC
    /// interval.
    pub fn new(
        row_id: RowId,
        begin_csn: Csn,
        end_csn: Option<Csn>,
        values: Vec<Option<Vec<u8>>>,
    ) -> Result<Self, RecordError> {
        if values.is_empty() {
            return Err(RecordError::EmptyRegularRow);
        }
        u16::try_from(values.len()).map_err(|_| RecordError::ColumnCountOverflow)?;
        validate_window(begin_csn, end_csn)?;
        Ok(Self {
            row_id,
            begin_csn,
            end_csn,
            tombstone: false,
            values,
        })
    }

    /// Constructs one committed tombstone version.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid MVCC interval.
    pub fn tombstone(
        row_id: RowId,
        begin_csn: Csn,
        end_csn: Option<Csn>,
    ) -> Result<Self, RecordError> {
        validate_window(begin_csn, end_csn)?;
        Ok(Self {
            row_id,
            begin_csn,
            end_csn,
            tombstone: true,
            values: Vec::new(),
        })
    }

    /// Returns the stable row identity.
    pub const fn row_id(&self) -> RowId {
        self.row_id
    }

    /// Returns the first CSN where this version is visible.
    pub const fn begin_csn(&self) -> Csn {
        self.begin_csn
    }

    /// Returns the first CSN where this version is no longer visible.
    pub const fn end_csn(&self) -> Option<Csn> {
        self.end_csn
    }

    /// Returns whether this version is a deletion marker.
    pub const fn is_tombstone(&self) -> bool {
        self.tombstone
    }

    /// Returns the catalog column count.
    pub fn column_count(&self) -> usize {
        self.values.len()
    }

    /// Returns one catalog-ordered logical column value.
    pub fn value(&self, index: usize) -> Option<ColumnValueRef<'_>> {
        self.values.get(index).map(|value| {
            value
                .as_deref()
                .map_or(ColumnValueRef::Null, ColumnValueRef::Bytes)
        })
    }

    /// Returns whether this version is visible at one snapshot CSN.
    pub fn is_visible_at(&self, visible_csn: Option<Csn>) -> bool {
        let Some(visible) = visible_csn else {
            return false;
        };
        self.begin_csn <= visible && self.end_csn.is_none_or(|end| visible < end)
    }

    /// Closes an open immutable version at the supplied later CSN.
    ///
    /// The returned record is a new value; the source record is consumed and
    /// no published bytes are changed in place.
    ///
    /// # Errors
    ///
    /// Returns an error when this version is already closed or `end_csn` does
    /// not form a valid half-open interval.
    pub fn close_at(mut self, end_csn: Csn) -> Result<Self, RecordError> {
        if self.end_csn.is_some() {
            return Err(RecordError::InvalidVersionWindow);
        }
        validate_window(self.begin_csn, Some(end_csn))?;
        self.end_csn = Some(end_csn);
        Ok(self)
    }

    /// Encodes one exact canonical row record.
    ///
    /// # Errors
    ///
    /// Returns an error when its length exceeds canonical offset fields.
    pub fn encode(&self) -> Result<Vec<u8>, RecordError> {
        if self.tombstone {
            let mut bytes = vec![0_u8; ROW_HEADER_SIZE];
            bytes[0..4].copy_from_slice(
                &u32::try_from(ROW_HEADER_SIZE)
                    .map_err(|_| RecordError::LengthOverflow)?
                    .to_le_bytes(),
            );
            bytes[4..6].copy_from_slice(&TOMBSTONE_FLAG.to_le_bytes());
            encode_identity_and_window(&mut bytes, self)?;
            return Ok(bytes);
        }

        let column_count =
            u16::try_from(self.values.len()).map_err(|_| RecordError::ColumnCountOverflow)?;
        let null_bytes = null_bitmap_length(self.values.len());
        let offsets_bytes = self
            .values
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_mul(4))
            .ok_or(RecordError::LengthOverflow)?;
        let value_start = ROW_HEADER_SIZE
            .checked_add(null_bytes)
            .and_then(|size| size.checked_add(offsets_bytes))
            .ok_or(RecordError::LengthOverflow)?;
        let value_bytes = self.values.iter().try_fold(0_usize, |total, value| {
            total
                .checked_add(value.as_ref().map_or(0, Vec::len))
                .ok_or(RecordError::LengthOverflow)
        })?;
        let total_length = value_start
            .checked_add(value_bytes)
            .ok_or(RecordError::LengthOverflow)?;
        let total_u32 = u32::try_from(total_length).map_err(|_| RecordError::LengthOverflow)?;
        let mut bytes = vec![0_u8; total_length];
        bytes[0..4].copy_from_slice(&total_u32.to_le_bytes());
        bytes[6..8].copy_from_slice(&column_count.to_le_bytes());
        encode_identity_and_window(&mut bytes, self)?;

        let null_start = ROW_HEADER_SIZE;
        let offsets_start = null_start + null_bytes;
        let mut value_offset = value_start;
        write_u32(&mut bytes[offsets_start..offsets_start + 4], value_offset)?;
        for (index, value) in self.values.iter().enumerate() {
            if let Some(value) = value {
                let end = value_offset
                    .checked_add(value.len())
                    .ok_or(RecordError::LengthOverflow)?;
                bytes[value_offset..end].copy_from_slice(value);
                value_offset = end;
            } else {
                bytes[null_start + index / 8] |= 1_u8 << (index % 8);
            }
            let offset_position = offsets_start + (index + 1) * 4;
            write_u32(
                &mut bytes[offset_position..offset_position + 4],
                value_offset,
            )?;
        }
        Ok(bytes)
    }

    /// Decodes and validates one exact canonical row record.
    ///
    /// # Errors
    ///
    /// Returns an error for any malformed length, flag, identity, MVCC window,
    /// null bit, offset, or tombstone representation.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let view = RowRecordView::decode(bytes)?;
        if view.is_tombstone() {
            return Ok(Self {
                row_id: view.row_id(),
                begin_csn: view.begin_csn(),
                end_csn: view.end_csn(),
                tombstone: true,
                values: Vec::new(),
            });
        }
        let mut values = Vec::with_capacity(view.column_count());
        for index in 0..view.column_count() {
            match view.value(index).ok_or(RecordError::InvalidOffsets)? {
                ColumnValueRef::Null => values.push(None),
                ColumnValueRef::Bytes(value) => values.push(Some(value.to_vec())),
            }
        }
        Ok(Self {
            row_id: view.row_id(),
            begin_csn: view.begin_csn(),
            end_csn: view.end_csn(),
            tombstone: false,
            values,
        })
    }
}

fn encode_identity_and_window(bytes: &mut [u8], row: &RowRecord) -> Result<(), RecordError> {
    validate_window(row.begin_csn, row.end_csn)?;
    bytes[8..24].copy_from_slice(&row.row_id.get().to_le_bytes());
    bytes[24..32].copy_from_slice(&row.begin_csn.get().to_le_bytes());
    bytes[32..40].copy_from_slice(&row.end_csn.map_or(OPEN_END_CSN, Csn::get).to_le_bytes());
    Ok(())
}

fn validate_window(begin_csn: Csn, end_csn: Option<Csn>) -> Result<(), RecordError> {
    if end_csn.is_some_and(|end| end.get() == OPEN_END_CSN || end <= begin_csn) {
        Err(RecordError::InvalidVersionWindow)
    } else {
        Ok(())
    }
}

fn null_bitmap_length(column_count: usize) -> usize {
    column_count.saturating_add(7) / 8
}

fn write_u32(target: &mut [u8], value: usize) -> Result<(), RecordError> {
    let value = u32::try_from(value).map_err(|_| RecordError::LengthOverflow)?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
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

/// Immutable content-addressed blob reference.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlobReference {
    /// Stable blob identity.
    pub id: BlobId,
    /// Logical byte length.
    pub logical_length: u64,
    /// BLAKE3 digest of immutable content.
    pub digest: [u8; 32],
}

impl BlobReference {
    /// Encodes the exact fixed-width blob reference.
    pub fn encode(self) -> [u8; BLOB_REFERENCE_SIZE] {
        let mut bytes = [0_u8; BLOB_REFERENCE_SIZE];
        bytes[0..16].copy_from_slice(&self.id.get().to_le_bytes());
        bytes[16..24].copy_from_slice(&self.logical_length.to_le_bytes());
        bytes[24..56].copy_from_slice(&self.digest);
        bytes
    }

    /// Decodes one exact fixed-width blob reference.
    ///
    /// # Errors
    ///
    /// Returns an error for a wrong length or zero blob identity.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() != BLOB_REFERENCE_SIZE {
            return Err(RecordError::InvalidBlobReferenceLength);
        }
        let id = BlobId::new(read_u128(&bytes[0..16])).map_err(|_| RecordError::ZeroIdentity)?;
        let logical_length = read_u64(&bytes[16..24]);
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(&bytes[24..56]);
        Ok(Self {
            id,
            logical_length,
            digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use hyphae_native_types::{BlobId, Csn, RowId};

    use super::{
        BLOB_REFERENCE_SIZE, BlobReference, ColumnValueRef, ROW_HEADER_SIZE,
        ROW_VERSION_POINTER_SIZE, RecordError, RowRecord, RowRecordView, RowVersionPointer,
    };

    #[test]
    fn canonical_row_round_trips_null_empty_and_binary() -> Result<(), Box<dyn std::error::Error>> {
        let row = RowRecord::new(
            RowId::new(7)?,
            Csn::new(3)?,
            Some(Csn::new(9)?),
            vec![
                Some(b"pk".to_vec()),
                None,
                Some(Vec::new()),
                Some(vec![0, 255]),
            ],
        )?;
        let encoded = row.encode()?;
        assert_eq!(RowRecord::decode(&encoded)?, row);
        let view = RowRecordView::decode(&encoded)?;
        assert_eq!(view.row_id(), RowId::new(7)?);
        assert_eq!(view.begin_csn(), Csn::new(3)?);
        assert_eq!(view.end_csn(), Some(Csn::new(9)?));
        assert_eq!(view.column_count(), 4);
        assert_eq!(view.value(0), Some(ColumnValueRef::Bytes(b"pk")));
        assert_eq!(view.value(1), Some(ColumnValueRef::Null));
        assert_eq!(view.value(2), Some(ColumnValueRef::Bytes(b"")));
        assert_eq!(view.value(3), Some(ColumnValueRef::Bytes(&[0, 255])));
        assert_eq!(view.bytes(), encoded);
        assert_eq!(row.value(0), Some(ColumnValueRef::Bytes(b"pk")));
        assert_eq!(row.value(1), Some(ColumnValueRef::Null));
        assert_eq!(row.value(2), Some(ColumnValueRef::Bytes(b"")));
        assert_eq!(row.value(3), Some(ColumnValueRef::Bytes(&[0, 255])));
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "ace9babb642187aae288a9d8823d20801341d271b4f83f417514270c88514d04"
        );
        Ok(())
    }

    #[test]
    fn tombstone_is_header_only_and_visibility_is_half_open()
    -> Result<(), Box<dyn std::error::Error>> {
        let tombstone = RowRecord::tombstone(RowId::new(8)?, Csn::new(5)?, None)?;
        let encoded = tombstone.encode()?;
        assert_eq!(encoded.len(), ROW_HEADER_SIZE);
        assert_eq!(RowRecord::decode(&encoded)?, tombstone);
        let view = RowRecordView::decode(&encoded)?;
        assert!(view.is_tombstone());
        assert_eq!(view.value(0), None);
        assert!(!tombstone.is_visible_at(Some(Csn::new(4)?)));
        assert!(tombstone.is_visible_at(Some(Csn::new(5)?)));
        Ok(())
    }

    #[test]
    fn invalid_offsets_null_bytes_and_unused_bits_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let row = RowRecord::new(
            RowId::new(1)?,
            Csn::new(1)?,
            None,
            vec![None, Some(b"value".to_vec())],
        )?;
        let encoded = row.encode()?;

        let mut invalid_unused = encoded.clone();
        invalid_unused[ROW_HEADER_SIZE] |= 0x80;
        assert_eq!(
            RowRecord::decode(&invalid_unused),
            Err(RecordError::NoncanonicalNullBitmap)
        );

        let mut null_has_bytes = encoded.clone();
        let first_offset = u32::from_le_bytes(
            null_has_bytes[ROW_HEADER_SIZE + 1..ROW_HEADER_SIZE + 5].try_into()?,
        );
        null_has_bytes[ROW_HEADER_SIZE + 5..ROW_HEADER_SIZE + 9]
            .copy_from_slice(&(first_offset + 1).to_le_bytes());
        assert_eq!(
            RowRecord::decode(&null_has_bytes),
            Err(RecordError::NullHasBytes)
        );

        let mut backwards = encoded;
        backwards[ROW_HEADER_SIZE + 9..ROW_HEADER_SIZE + 13].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            RowRecord::decode(&backwards),
            Err(RecordError::InvalidOffsets)
        );
        Ok(())
    }

    #[test]
    fn invalid_windows_and_tombstone_payloads_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            RowRecord::new(
                RowId::new(1)?,
                Csn::new(3)?,
                Some(Csn::new(3)?),
                vec![Some(Vec::new())],
            ),
            Err(RecordError::InvalidVersionWindow)
        );
        let mut tombstone = RowRecord::tombstone(RowId::new(1)?, Csn::new(1)?, None)?.encode()?;
        tombstone.extend_from_slice(b"x");
        let tombstone_length =
            u32::try_from(tombstone.len()).map_err(|_| RecordError::LengthOverflow)?;
        tombstone[0..4].copy_from_slice(&tombstone_length.to_le_bytes());
        assert_eq!(
            RowRecord::decode(&tombstone),
            Err(RecordError::InvalidTombstone)
        );
        Ok(())
    }

    #[test]
    fn blob_reference_is_exact_and_fixed_width() -> Result<(), Box<dyn std::error::Error>> {
        let reference = BlobReference {
            id: BlobId::new(11)?,
            logical_length: 65_537,
            digest: [0xa5; 32],
        };
        let encoded = reference.encode();
        assert_eq!(encoded.len(), BLOB_REFERENCE_SIZE);
        assert_eq!(BlobReference::decode(&encoded)?, reference);
        assert_eq!(
            BlobReference::decode(&encoded[..BLOB_REFERENCE_SIZE - 1]),
            Err(RecordError::InvalidBlobReferenceLength)
        );
        Ok(())
    }

    #[test]
    fn row_version_pointer_and_closed_copy_are_canonical() -> Result<(), Box<dyn std::error::Error>>
    {
        let pointer = RowVersionPointer {
            page_id: hyphae_native_types::PageId::new(42)?,
        };
        let encoded = pointer.encode();
        assert_eq!(encoded.len(), ROW_VERSION_POINTER_SIZE);
        assert_eq!(&encoded[..8], b"HYROWP01");
        assert_eq!(&encoded[8..], &42_u64.to_le_bytes());
        assert_eq!(RowVersionPointer::decode(&encoded)?, pointer);

        let open = RowRecord::new(
            RowId::new(2)?,
            Csn::new(3)?,
            None,
            vec![Some(b"row".to_vec())],
        )?;
        let closed = open.close_at(Csn::new(5)?)?;
        assert_eq!(closed.begin_csn(), Csn::new(3)?);
        assert_eq!(closed.end_csn(), Some(Csn::new(5)?));
        assert_eq!(
            closed.clone().close_at(Csn::new(6)?),
            Err(RecordError::InvalidVersionWindow)
        );
        Ok(())
    }
}
