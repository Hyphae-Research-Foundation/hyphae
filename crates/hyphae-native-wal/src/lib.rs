// SPDX-License-Identifier: Apache-2.0

//! Block-framed authoritative WAL codec and append/recovery file.

use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::Path,
};

use hyphae_native_types::{EngineKind, Lsn, TransactionId};
use thiserror::Error;

/// Fixed WAL block size.
pub const WAL_BLOCK_SIZE: usize = 65_536;
/// WAL block header size.
pub const WAL_BLOCK_HEADER_SIZE: usize = 112;
/// Maximum encoded records in one WAL block.
pub const WAL_BLOCK_PAYLOAD_SIZE: usize = WAL_BLOCK_SIZE - WAL_BLOCK_HEADER_SIZE;
/// Encoded WAL record header size.
pub const WAL_RECORD_HEADER_SIZE: usize = 44;
/// Maximum body size for one WAL record.
pub const WAL_RECORD_BODY_SIZE: usize = WAL_BLOCK_PAYLOAD_SIZE - WAL_RECORD_HEADER_SIZE;

const WAL_BLOCK_SIZE_U64: u64 = 65_536;
const WAL_BLOCK_HEADER_SIZE_U64: u64 = 112;
const MAGIC: [u8; 8] = *b"HYWAL001";
const FORMAT_VERSION: u16 = 1;
const HEADER_LENGTH_U16: u16 = 112;
const BLOCK_CHECKSUM_START: usize = 44;
const BLOCK_CHECKSUM_END: usize = 48;
const PREVIOUS_DIGEST_START: usize = 48;
const PREVIOUS_DIGEST_END: usize = 80;
const BLOCK_DIGEST_START: usize = 80;
const BLOCK_DIGEST_END: usize = 112;
const RECORD_CHECKSUM_START: usize = 36;
const RECORD_CHECKSUM_END: usize = 40;

/// Native WAL record role.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum RecordKind {
    /// Transaction boundary and snapshot declaration.
    Begin = 1,
    /// One versioned engine mutation.
    Mutation = 2,
    /// Successful transaction commit.
    Commit = 3,
    /// Advisory transaction abort.
    Abort = 4,
    /// Root-set checkpoint.
    Checkpoint = 5,
    /// Catalog change payload.
    Catalog = 6,
}

impl TryFrom<u8> for RecordKind {
    type Error = WalError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Mutation),
            3 => Ok(Self::Commit),
            4 => Ok(Self::Abort),
            5 => Ok(Self::Checkpoint),
            6 => Ok(Self::Catalog),
            other => Err(WalError::UnknownRecordKind(other)),
        }
    }
}

/// WAL framing, integrity, or file failure.
#[derive(Debug, Error)]
pub enum WalError {
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Input is not exactly one complete block.
    #[error("WAL block has length {actual}; expected {WAL_BLOCK_SIZE}")]
    InvalidBlockLength {
        /// Actual input length.
        actual: usize,
    },
    /// Block magic is invalid.
    #[error("invalid WAL block magic")]
    InvalidMagic,
    /// Block version or header length is unsupported.
    #[error("unsupported WAL block format {version} or header length {header_length}")]
    UnsupportedFormat {
        /// Found version.
        version: u16,
        /// Found header length.
        header_length: u16,
    },
    /// Block sequence differs from its physical position.
    #[error("WAL block sequence mismatch: expected {expected}, found {found}")]
    BlockSequenceMismatch {
        /// Expected sequence.
        expected: u64,
        /// Encoded sequence.
        found: u64,
    },
    /// Previous-block digest does not match.
    #[error("WAL previous-block digest mismatch at block {sequence}")]
    PreviousDigestMismatch {
        /// Block sequence.
        sequence: u64,
    },
    /// Block fields reserved by v1 are nonzero.
    #[error("WAL block reserved flags are nonzero")]
    ReservedNonzero,
    /// Block payload length is invalid.
    #[error("WAL block payload is {actual} bytes; maximum is {WAL_BLOCK_PAYLOAD_SIZE}")]
    BlockPayloadTooLarge {
        /// Encoded payload length.
        actual: usize,
    },
    /// Padding after the declared records is nonzero.
    #[error("WAL block padding is nonzero")]
    NonzeroPadding,
    /// Block checksum failed.
    #[error("WAL block CRC32C mismatch")]
    BlockChecksumMismatch,
    /// Block digest failed.
    #[error("WAL block BLAKE3 mismatch")]
    BlockDigestMismatch,
    /// A block has no records.
    #[error("WAL block must contain at least one record")]
    EmptyBlock,
    /// One record body cannot fit in a block.
    #[error("WAL record body is {actual} bytes; maximum is {WAL_RECORD_BODY_SIZE}")]
    RecordBodyTooLarge {
        /// Body byte length.
        actual: usize,
    },
    /// Record lengths are malformed.
    #[error("malformed WAL record length at block payload offset {offset}")]
    InvalidRecordLength {
        /// Record offset inside the block payload.
        offset: usize,
    },
    /// Unknown record kind.
    #[error("unknown WAL record kind {0}")]
    UnknownRecordKind(u8),
    /// Unknown owning engine.
    #[error("unknown WAL engine kind {0}")]
    UnknownEngineKind(u8),
    /// Record reserved bytes are nonzero.
    #[error("WAL record reserved bytes are nonzero")]
    RecordReservedNonzero,
    /// Record LSN differs from its physical location.
    #[error("WAL record LSN mismatch: expected {expected}, found {found}")]
    RecordLsnMismatch {
        /// Expected physical LSN.
        expected: u64,
        /// Encoded LSN.
        found: u64,
    },
    /// Record checksum failed.
    #[error("WAL record CRC32C mismatch at LSN {lsn}")]
    RecordChecksumMismatch {
        /// Record LSN.
        lsn: u64,
    },
    /// A prior uncertain write or sync poisoned the WAL writer.
    #[error("native WAL writer is poisoned; reopen before writing")]
    Poisoned,
    /// Block sequence or physical offset space is exhausted.
    #[error("native WAL address space is exhausted")]
    AddressExhausted,
}

/// Record not yet assigned a physical LSN.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingRecord {
    kind: RecordKind,
    engine: EngineKind,
    flags: u16,
    transaction_id: TransactionId,
    body: Vec<u8>,
}

impl PendingRecord {
    /// Constructs a bounded pending record.
    ///
    /// # Errors
    ///
    /// Returns an error when the body cannot fit in one block.
    pub fn new(
        kind: RecordKind,
        engine: EngineKind,
        flags: u16,
        transaction_id: TransactionId,
        body: impl Into<Vec<u8>>,
    ) -> Result<Self, WalError> {
        let body = body.into();
        if body.len() > WAL_RECORD_BODY_SIZE {
            return Err(WalError::RecordBodyTooLarge { actual: body.len() });
        }
        Ok(Self {
            kind,
            engine,
            flags,
            transaction_id,
            body,
        })
    }

    fn encoded_length(&self) -> usize {
        WAL_RECORD_HEADER_SIZE + self.body.len()
    }
}

/// Verified WAL record with a physical LSN.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalRecord {
    lsn: Lsn,
    kind: RecordKind,
    engine: EngineKind,
    flags: u16,
    transaction_id: TransactionId,
    body: Vec<u8>,
}

impl WalRecord {
    /// Returns the physical record LSN.
    pub const fn lsn(&self) -> Lsn {
        self.lsn
    }

    /// Returns the record role.
    pub const fn kind(&self) -> RecordKind {
        self.kind
    }

    /// Returns the owning native engine.
    pub const fn engine(&self) -> EngineKind {
        self.engine
    }

    /// Returns v1 record flags.
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// Returns the transaction identity.
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the verified body bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    fn encode(&self) -> Result<Vec<u8>, WalError> {
        let total_length = WAL_RECORD_HEADER_SIZE
            .checked_add(self.body.len())
            .ok_or(WalError::AddressExhausted)?;
        let mut encoded = vec![0_u8; total_length];
        encoded[0..4].copy_from_slice(
            &u32::try_from(total_length)
                .map_err(|_| WalError::AddressExhausted)?
                .to_le_bytes(),
        );
        encoded[4..8].copy_from_slice(
            &u32::try_from(self.body.len())
                .map_err(|_| WalError::AddressExhausted)?
                .to_le_bytes(),
        );
        encoded[8] = self.kind as u8;
        encoded[9] = self.engine as u8;
        encoded[10..12].copy_from_slice(&self.flags.to_le_bytes());
        encoded[12..20].copy_from_slice(&self.lsn.get().to_le_bytes());
        encoded[20..36].copy_from_slice(&self.transaction_id.get().to_le_bytes());
        encoded[WAL_RECORD_HEADER_SIZE..].copy_from_slice(&self.body);
        let checksum = record_checksum(&encoded);
        encoded[RECORD_CHECKSUM_START..RECORD_CHECKSUM_END]
            .copy_from_slice(&checksum.to_le_bytes());
        Ok(encoded)
    }

    fn decode(encoded: &[u8], expected_lsn: Lsn) -> Result<Self, WalError> {
        if encoded.len() < WAL_RECORD_HEADER_SIZE {
            return Err(WalError::InvalidRecordLength { offset: 0 });
        }
        let total_length =
            usize::try_from(read_u32(&encoded[0..4])).map_err(|_| WalError::AddressExhausted)?;
        let body_length =
            usize::try_from(read_u32(&encoded[4..8])).map_err(|_| WalError::AddressExhausted)?;
        if total_length != encoded.len()
            || body_length != total_length.saturating_sub(WAL_RECORD_HEADER_SIZE)
        {
            return Err(WalError::InvalidRecordLength { offset: 0 });
        }
        if encoded[40..44].iter().any(|byte| *byte != 0) {
            return Err(WalError::RecordReservedNonzero);
        }
        let found_lsn = read_u64(&encoded[12..20]);
        if found_lsn != expected_lsn.get() {
            return Err(WalError::RecordLsnMismatch {
                expected: expected_lsn.get(),
                found: found_lsn,
            });
        }
        let expected_checksum = read_u32(&encoded[RECORD_CHECKSUM_START..RECORD_CHECKSUM_END]);
        if record_checksum(encoded) != expected_checksum {
            return Err(WalError::RecordChecksumMismatch { lsn: found_lsn });
        }
        let transaction_raw = read_u128(&encoded[20..36]);
        let transaction_id =
            TransactionId::new(transaction_raw).map_err(|_| WalError::RecordReservedNonzero)?;
        Ok(Self {
            lsn: expected_lsn,
            kind: RecordKind::try_from(encoded[8])?,
            engine: decode_engine_kind(encoded[9])?,
            flags: read_u16(&encoded[10..12]),
            transaction_id,
            body: encoded[WAL_RECORD_HEADER_SIZE..].to_vec(),
        })
    }
}

fn record_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[RECORD_CHECKSUM_START..RECORD_CHECKSUM_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn decode_engine_kind(value: u8) -> Result<EngineKind, WalError> {
    match value {
        0 => Ok(EngineKind::Kernel),
        1 => Ok(EngineKind::Relational),
        2 => Ok(EngineKind::Structure),
        3 => Ok(EngineKind::Search),
        other => Err(WalError::UnknownEngineKind(other)),
    }
}

/// Verified fixed-size WAL block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalBlock {
    sequence: u64,
    previous_digest: [u8; 32],
    digest: [u8; 32],
    records: Vec<WalRecord>,
}

impl WalBlock {
    /// Builds one block and assigns physical LSNs to its records.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty block, oversized payload, or exhausted
    /// address space.
    pub fn build(
        sequence: u64,
        previous_digest: [u8; 32],
        pending: Vec<PendingRecord>,
    ) -> Result<Self, WalError> {
        if sequence == 0 {
            return Err(WalError::AddressExhausted);
        }
        if pending.is_empty() {
            return Err(WalError::EmptyBlock);
        }
        let mut payload_offset = 0_usize;
        let block_offset = sequence
            .checked_sub(1)
            .and_then(|value| value.checked_mul(WAL_BLOCK_SIZE_U64))
            .ok_or(WalError::AddressExhausted)?;
        let mut records = Vec::with_capacity(pending.len());
        for record in pending {
            let next_payload_offset = payload_offset
                .checked_add(record.encoded_length())
                .ok_or(WalError::AddressExhausted)?;
            if next_payload_offset > WAL_BLOCK_PAYLOAD_SIZE {
                return Err(WalError::BlockPayloadTooLarge {
                    actual: next_payload_offset,
                });
            }
            let payload_offset_u64 =
                u64::try_from(payload_offset).map_err(|_| WalError::AddressExhausted)?;
            let lsn_raw = block_offset
                .checked_add(WAL_BLOCK_HEADER_SIZE_U64)
                .and_then(|value| value.checked_add(payload_offset_u64))
                .ok_or(WalError::AddressExhausted)?;
            let lsn = Lsn::new(lsn_raw).map_err(|_| WalError::AddressExhausted)?;
            records.push(WalRecord {
                lsn,
                kind: record.kind,
                engine: record.engine,
                flags: record.flags,
                transaction_id: record.transaction_id,
                body: record.body,
            });
            payload_offset = next_payload_offset;
        }
        let mut block = Self {
            sequence,
            previous_digest,
            digest: [0; 32],
            records,
        };
        let encoded = block.encode_with_zero_digest()?;
        block.digest = block_digest(&encoded);
        Ok(block)
    }

    /// Returns the strictly increasing block sequence.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the preceding complete block digest.
    pub const fn previous_digest(&self) -> [u8; 32] {
        self.previous_digest
    }

    /// Returns this block's content digest.
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Returns verified records in physical order.
    pub fn records(&self) -> &[WalRecord] {
        &self.records
    }

    /// Encodes one complete fixed-size WAL block.
    ///
    /// # Errors
    ///
    /// Returns an error if its records no longer fit the canonical block.
    pub fn encode(&self) -> Result<Vec<u8>, WalError> {
        let mut encoded = self.encode_with_zero_digest()?;
        encoded[BLOCK_DIGEST_START..BLOCK_DIGEST_END].copy_from_slice(&self.digest);
        Ok(encoded)
    }

    fn encode_with_zero_digest(&self) -> Result<Vec<u8>, WalError> {
        if self.records.is_empty() {
            return Err(WalError::EmptyBlock);
        }
        let mut payload = Vec::new();
        for record in &self.records {
            payload.extend_from_slice(&record.encode()?);
        }
        if payload.len() > WAL_BLOCK_PAYLOAD_SIZE {
            return Err(WalError::BlockPayloadTooLarge {
                actual: payload.len(),
            });
        }
        let mut encoded = vec![0_u8; WAL_BLOCK_SIZE];
        encoded[0..8].copy_from_slice(&MAGIC);
        encoded[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        encoded[10..12].copy_from_slice(&HEADER_LENGTH_U16.to_le_bytes());
        encoded[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        let first_lsn = self
            .records
            .first()
            .map(WalRecord::lsn)
            .ok_or(WalError::EmptyBlock)?;
        let last_lsn = self
            .records
            .last()
            .map(WalRecord::lsn)
            .ok_or(WalError::EmptyBlock)?;
        encoded[24..32].copy_from_slice(&first_lsn.get().to_le_bytes());
        encoded[32..40].copy_from_slice(&last_lsn.get().to_le_bytes());
        encoded[40..44].copy_from_slice(
            &u32::try_from(payload.len())
                .map_err(|_| WalError::AddressExhausted)?
                .to_le_bytes(),
        );
        encoded[PREVIOUS_DIGEST_START..PREVIOUS_DIGEST_END].copy_from_slice(&self.previous_digest);
        encoded[WAL_BLOCK_HEADER_SIZE..WAL_BLOCK_HEADER_SIZE + payload.len()]
            .copy_from_slice(&payload);
        let checksum = block_checksum(&encoded);
        encoded[BLOCK_CHECKSUM_START..BLOCK_CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
        Ok(encoded)
    }

    /// Decodes and verifies one physical block.
    ///
    /// # Errors
    ///
    /// Returns an error for any corruption, sequence, digest, padding, record,
    /// or LSN divergence.
    pub fn decode(
        expected_sequence: u64,
        expected_previous_digest: [u8; 32],
        encoded: &[u8],
    ) -> Result<Self, WalError> {
        let (payload_length, expected_digest) =
            decode_block_header(expected_sequence, expected_previous_digest, encoded)?;
        let records = decode_block_records(expected_sequence, payload_length, encoded)?;
        validate_block_record_range(&records, encoded)?;
        Ok(Self {
            sequence: expected_sequence,
            previous_digest: expected_previous_digest,
            digest: expected_digest,
            records,
        })
    }
}

fn decode_block_header(
    expected_sequence: u64,
    expected_previous_digest: [u8; 32],
    encoded: &[u8],
) -> Result<(usize, [u8; 32]), WalError> {
    if encoded.len() != WAL_BLOCK_SIZE {
        return Err(WalError::InvalidBlockLength {
            actual: encoded.len(),
        });
    }
    if encoded[0..8] != MAGIC {
        return Err(WalError::InvalidMagic);
    }
    let version = read_u16(&encoded[8..10]);
    let header_length = read_u16(&encoded[10..12]);
    if version != FORMAT_VERSION || usize::from(header_length) != WAL_BLOCK_HEADER_SIZE {
        return Err(WalError::UnsupportedFormat {
            version,
            header_length,
        });
    }
    if encoded[12..16].iter().any(|byte| *byte != 0) {
        return Err(WalError::ReservedNonzero);
    }
    let found_sequence = read_u64(&encoded[16..24]);
    if found_sequence != expected_sequence {
        return Err(WalError::BlockSequenceMismatch {
            expected: expected_sequence,
            found: found_sequence,
        });
    }
    if encoded[PREVIOUS_DIGEST_START..PREVIOUS_DIGEST_END] != expected_previous_digest {
        return Err(WalError::PreviousDigestMismatch {
            sequence: expected_sequence,
        });
    }
    let payload_length =
        usize::try_from(read_u32(&encoded[40..44])).map_err(|_| WalError::AddressExhausted)?;
    if payload_length == 0 {
        return Err(WalError::EmptyBlock);
    }
    if payload_length > WAL_BLOCK_PAYLOAD_SIZE {
        return Err(WalError::BlockPayloadTooLarge {
            actual: payload_length,
        });
    }
    let payload_end = WAL_BLOCK_HEADER_SIZE + payload_length;
    if encoded[payload_end..].iter().any(|byte| *byte != 0) {
        return Err(WalError::NonzeroPadding);
    }
    let expected_checksum = read_u32(&encoded[BLOCK_CHECKSUM_START..BLOCK_CHECKSUM_END]);
    if block_checksum(encoded) != expected_checksum {
        return Err(WalError::BlockChecksumMismatch);
    }
    let mut expected_digest = [0_u8; 32];
    expected_digest.copy_from_slice(&encoded[BLOCK_DIGEST_START..BLOCK_DIGEST_END]);
    if block_digest(encoded) != expected_digest {
        return Err(WalError::BlockDigestMismatch);
    }
    Ok((payload_length, expected_digest))
}

fn decode_block_records(
    expected_sequence: u64,
    payload_length: usize,
    encoded: &[u8],
) -> Result<Vec<WalRecord>, WalError> {
    let block_offset = expected_sequence
        .checked_sub(1)
        .and_then(|value| value.checked_mul(WAL_BLOCK_SIZE_U64))
        .ok_or(WalError::AddressExhausted)?;
    let mut records = Vec::new();
    let mut payload_offset = 0_usize;
    while payload_offset < payload_length {
        let remaining = payload_length - payload_offset;
        if remaining < WAL_RECORD_HEADER_SIZE {
            return Err(WalError::InvalidRecordLength {
                offset: payload_offset,
            });
        }
        let start = WAL_BLOCK_HEADER_SIZE + payload_offset;
        let total_length = usize::try_from(read_u32(&encoded[start..start + 4]))
            .map_err(|_| WalError::AddressExhausted)?;
        if total_length < WAL_RECORD_HEADER_SIZE || total_length > remaining {
            return Err(WalError::InvalidRecordLength {
                offset: payload_offset,
            });
        }
        let payload_offset_u64 =
            u64::try_from(payload_offset).map_err(|_| WalError::AddressExhausted)?;
        let lsn_raw = block_offset
            .checked_add(WAL_BLOCK_HEADER_SIZE_U64)
            .and_then(|value| value.checked_add(payload_offset_u64))
            .ok_or(WalError::AddressExhausted)?;
        let expected_lsn = Lsn::new(lsn_raw).map_err(|_| WalError::AddressExhausted)?;
        records.push(WalRecord::decode(
            &encoded[start..start + total_length],
            expected_lsn,
        )?);
        payload_offset += total_length;
    }
    Ok(records)
}

fn validate_block_record_range(records: &[WalRecord], encoded: &[u8]) -> Result<(), WalError> {
    let first_lsn = records
        .first()
        .map(WalRecord::lsn)
        .ok_or(WalError::EmptyBlock)?;
    let last_lsn = records
        .last()
        .map(WalRecord::lsn)
        .ok_or(WalError::EmptyBlock)?;
    if read_u64(&encoded[24..32]) != first_lsn.get() || read_u64(&encoded[32..40]) != last_lsn.get()
    {
        return Err(WalError::RecordLsnMismatch {
            expected: first_lsn.get(),
            found: read_u64(&encoded[24..32]),
        });
    }
    Ok(())
}

fn block_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[BLOCK_CHECKSUM_START..BLOCK_CHECKSUM_END].fill(0);
    canonical[BLOCK_DIGEST_START..BLOCK_DIGEST_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn block_digest(encoded: &[u8]) -> [u8; 32] {
    let mut canonical = encoded.to_vec();
    canonical[BLOCK_DIGEST_START..BLOCK_DIGEST_END].fill(0);
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

fn read_u128(bytes: &[u8]) -> u128 {
    let mut value = [0_u8; 16];
    value.copy_from_slice(bytes);
    u128::from_le_bytes(value)
}

/// Durable identity of one appended WAL block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockReceipt {
    /// Physical block sequence.
    pub sequence: u64,
    /// First record LSN.
    pub first_lsn: Lsn,
    /// Last record LSN.
    pub last_lsn: Lsn,
    /// Complete block digest.
    pub digest: [u8; 32],
}

/// Verified recovery report for one WAL file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalRecovery {
    /// Complete records in physical order.
    pub records: Vec<WalRecord>,
    /// Verified block receipts in physical order.
    pub blocks: Vec<BlockReceipt>,
    /// Incomplete physical tail removed during open.
    pub truncated_tail_bytes: u64,
    /// Last complete block sequence.
    pub last_sequence: u64,
    /// Last complete block digest, or zero for an empty file.
    pub last_digest: [u8; 32],
}

/// Newly opened WAL and its verified recovery evidence.
#[derive(Debug)]
pub struct OpenedWal {
    /// Ready append-only WAL writer.
    pub wal: WalFile,
    /// Verification and tail-repair evidence.
    pub recovery: WalRecovery,
}

/// Append-only block-framed native WAL file.
#[derive(Debug)]
pub struct WalFile {
    file: File,
    next_sequence: u64,
    previous_digest: [u8; 32],
    poisoned: bool,
}

impl WalFile {
    /// Creates a new empty WAL file.
    ///
    /// # Errors
    ///
    /// Returns an error if the path exists or cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, WalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            file,
            next_sequence: 1,
            previous_digest: [0; 32],
            poisoned: false,
        })
    }

    /// Opens, verifies, and repairs only an incomplete final block.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O or any complete corrupt block.
    pub fn open(path: impl AsRef<Path>) -> Result<OpenedWal, WalError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        let complete_blocks = length / WAL_BLOCK_SIZE_U64;
        let tail_bytes = length % WAL_BLOCK_SIZE_U64;
        let mut previous_digest = [0_u8; 32];
        let mut records = Vec::new();
        let mut blocks = Vec::new();
        for index in 0..complete_blocks {
            let sequence = index.checked_add(1).ok_or(WalError::AddressExhausted)?;
            let mut encoded = vec![0_u8; WAL_BLOCK_SIZE];
            read_exact_at(
                &file,
                &mut encoded,
                index
                    .checked_mul(WAL_BLOCK_SIZE_U64)
                    .ok_or(WalError::AddressExhausted)?,
            )?;
            let block = WalBlock::decode(sequence, previous_digest, &encoded)?;
            let first_lsn = block
                .records()
                .first()
                .map(WalRecord::lsn)
                .ok_or(WalError::EmptyBlock)?;
            let last_lsn = block
                .records()
                .last()
                .map(WalRecord::lsn)
                .ok_or(WalError::EmptyBlock)?;
            blocks.push(BlockReceipt {
                sequence,
                first_lsn,
                last_lsn,
                digest: block.digest(),
            });
            previous_digest = block.digest();
            records.extend_from_slice(block.records());
        }
        if tail_bytes != 0 {
            file.set_len(
                complete_blocks
                    .checked_mul(WAL_BLOCK_SIZE_U64)
                    .ok_or(WalError::AddressExhausted)?,
            )?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;
        let next_sequence = complete_blocks
            .checked_add(1)
            .ok_or(WalError::AddressExhausted)?;
        Ok(OpenedWal {
            wal: Self {
                file,
                next_sequence,
                previous_digest,
                poisoned: false,
            },
            recovery: WalRecovery {
                records,
                blocks,
                truncated_tail_bytes: tail_bytes,
                last_sequence: complete_blocks,
                last_digest: previous_digest,
            },
        })
    }

    /// Appends records in one or more packed fixed-size blocks.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty request, oversized record, exhausted
    /// address space, or uncertain write/synchronization. Reopen a poisoned
    /// writer before further writes.
    pub fn append_records(
        &mut self,
        records: Vec<PendingRecord>,
        synchronize: bool,
    ) -> Result<Vec<BlockReceipt>, WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        if records.is_empty() {
            return Err(WalError::EmptyBlock);
        }
        let mut groups: Vec<Vec<PendingRecord>> = Vec::new();
        let mut current = Vec::new();
        let mut current_bytes = 0_usize;
        for record in records {
            let record_bytes = record.encoded_length();
            if current_bytes
                .checked_add(record_bytes)
                .ok_or(WalError::AddressExhausted)?
                > WAL_BLOCK_PAYLOAD_SIZE
            {
                groups.push(current);
                current = Vec::new();
                current_bytes = 0;
            }
            current_bytes = current_bytes
                .checked_add(record_bytes)
                .ok_or(WalError::AddressExhausted)?;
            current.push(record);
        }
        if !current.is_empty() {
            groups.push(current);
        }

        self.poisoned = true;
        let mut receipts = Vec::with_capacity(groups.len());
        for group in groups {
            let block = WalBlock::build(self.next_sequence, self.previous_digest, group)?;
            let encoded = block.encode()?;
            if let Err(source) = self.file.write_all(&encoded) {
                return Err(WalError::Io(source));
            }
            let first_lsn = block
                .records()
                .first()
                .map(WalRecord::lsn)
                .ok_or(WalError::EmptyBlock)?;
            let last_lsn = block
                .records()
                .last()
                .map(WalRecord::lsn)
                .ok_or(WalError::EmptyBlock)?;
            receipts.push(BlockReceipt {
                sequence: block.sequence(),
                first_lsn,
                last_lsn,
                digest: block.digest(),
            });
            self.previous_digest = block.digest();
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or(WalError::AddressExhausted)?;
        }
        if synchronize {
            self.file.sync_data()?;
        }
        self.poisoned = false;
        Ok(receipts)
    }

    /// Explicitly synchronizes previously appended blocks.
    ///
    /// # Errors
    ///
    /// Returns an error and poisons the writer on uncertain synchronization.
    pub fn sync_data(&mut self) -> Result<(), WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        if let Err(source) = self.file.sync_data() {
            self.poisoned = true;
            return Err(WalError::Io(source));
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
                "truncated native WAL block",
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
                "truncated native WAL block",
            ));
        }
        offset = offset.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        buffer = &mut buffer[read..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_types::{EngineKind, TransactionId};

    use super::{
        PendingRecord, RecordKind, WAL_BLOCK_HEADER_SIZE, WAL_BLOCK_SIZE, WAL_RECORD_HEADER_SIZE,
        WalBlock, WalError, WalFile,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-wal-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self { path })
        }

        fn wal_file(&self) -> PathBuf {
            self.path.join("wal.hylog")
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

    fn pending(
        kind: RecordKind,
        engine: EngineKind,
        transaction: u128,
        body: &[u8],
    ) -> Result<PendingRecord, Box<dyn std::error::Error>> {
        Ok(PendingRecord::new(
            kind,
            engine,
            0,
            TransactionId::new(transaction)?,
            body.to_vec(),
        )?)
    }

    #[test]
    fn block_codec_round_trips_multiple_engines() -> Result<(), Box<dyn std::error::Error>> {
        let block = WalBlock::build(
            1,
            [0; 32],
            vec![
                pending(RecordKind::Begin, EngineKind::Kernel, 7, b"begin")?,
                pending(RecordKind::Mutation, EngineKind::Relational, 7, b"row")?,
                pending(RecordKind::Mutation, EngineKind::Structure, 7, b"key")?,
                pending(RecordKind::Mutation, EngineKind::Search, 7, b"term")?,
                pending(RecordKind::Commit, EngineKind::Kernel, 7, b"commit")?,
            ],
        )?;
        assert_eq!(WalBlock::decode(1, [0; 32], &block.encode()?)?, block);
        Ok(())
    }

    #[test]
    fn golden_wal_block_encoding_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let block = WalBlock::build(
            1,
            [0; 32],
            vec![
                pending(RecordKind::Begin, EngineKind::Kernel, 7, b"begin")?,
                pending(RecordKind::Mutation, EngineKind::Relational, 7, b"row")?,
                pending(RecordKind::Commit, EngineKind::Kernel, 7, b"commit")?,
            ],
        )?;
        let encoded = block.encode()?;
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "01aa068a8ecdde357f3f2c8c9f9addd851a54f6012db680b3fa1154c23f44627"
        );
        assert_eq!(&encoded[0..8], b"HYWAL001");
        assert_eq!(&encoded[8..10], &1_u16.to_le_bytes());
        assert_eq!(&encoded[16..24], &1_u64.to_le_bytes());
        assert_eq!(&encoded[24..32], &112_u64.to_le_bytes());
        Ok(())
    }

    #[test]
    fn complete_block_corruption_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let block = WalBlock::build(
            1,
            [0; 32],
            vec![pending(
                RecordKind::Mutation,
                EngineKind::Relational,
                1,
                b"row",
            )?],
        )?;
        let mut encoded = block.encode()?;
        encoded[WAL_BLOCK_HEADER_SIZE + WAL_RECORD_HEADER_SIZE] ^= 1;
        assert!(matches!(
            WalBlock::decode(1, [0; 32], &encoded),
            Err(WalError::BlockChecksumMismatch)
        ));
        Ok(())
    }

    #[test]
    fn file_append_recovery_preserves_record_order() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.wal_file();
        {
            let mut wal = WalFile::create(&path)?;
            let receipts = wal.append_records(
                vec![
                    pending(RecordKind::Begin, EngineKind::Kernel, 2, b"begin")?,
                    pending(RecordKind::Mutation, EngineKind::Structure, 2, b"set")?,
                    pending(RecordKind::Commit, EngineKind::Kernel, 2, b"commit")?,
                ],
                true,
            )?;
            assert_eq!(receipts.len(), 1);
        }
        let opened = WalFile::open(&path)?;
        assert_eq!(opened.recovery.records.len(), 3);
        assert_eq!(opened.recovery.records[1].body(), b"set");
        assert_eq!(opened.recovery.truncated_tail_bytes, 0);
        Ok(())
    }

    #[test]
    fn open_truncates_only_incomplete_tail() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.wal_file();
        {
            let mut wal = WalFile::create(&path)?;
            wal.append_records(
                vec![pending(
                    RecordKind::Mutation,
                    EngineKind::Search,
                    3,
                    b"index",
                )?],
                true,
            )?;
        }
        let mut bytes = fs::read(&path)?;
        bytes.extend_from_slice(b"partial");
        fs::write(&path, &bytes)?;
        let opened = WalFile::open(&path)?;
        assert_eq!(opened.recovery.truncated_tail_bytes, 7);
        assert_eq!(fs::metadata(&path)?.len(), WAL_BLOCK_SIZE as u64);
        Ok(())
    }

    #[test]
    fn complete_corrupt_block_is_never_truncated() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let path = temporary.wal_file();
        {
            let mut wal = WalFile::create(&path)?;
            wal.append_records(
                vec![pending(
                    RecordKind::Mutation,
                    EngineKind::Relational,
                    4,
                    b"row",
                )?],
                true,
            )?;
        }
        let mut bytes = fs::read(&path)?;
        bytes[WAL_BLOCK_HEADER_SIZE + WAL_RECORD_HEADER_SIZE] ^= 1;
        fs::write(&path, &bytes)?;
        assert!(matches!(
            WalFile::open(&path),
            Err(WalError::BlockChecksumMismatch)
        ));
        assert_eq!(fs::metadata(&path)?.len(), WAL_BLOCK_SIZE as u64);
        Ok(())
    }
}
