// SPDX-License-Identifier: AGPL-3.0-only

//! Block-framed authoritative WAL codec and append/recovery file.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use hyphae_native_types::{
    Csn, EngineKind, LineageIdentity, Lsn, ManifestGeneration, TransactionId,
};
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
/// Exact encoded retention-anchor size.
pub const WAL_RETENTION_ANCHOR_SIZE: usize = 256;
/// Exact encoded lineage-bearing retention-anchor size.
pub const WAL_RETENTION_ANCHOR_V2_SIZE: usize = 280;

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
const RETENTION_MAGIC_V1: [u8; 8] = *b"HYWAR001";
const RETENTION_MAGIC_V2: [u8; 8] = *b"HYWAR002";
const RETENTION_FORMAT_VERSION_V1: u16 = 1;
const RETENTION_FORMAT_VERSION_V2: u16 = 2;
const RETENTION_HEADER_LENGTH_V1: u16 = 256;
const RETENTION_HEADER_LENGTH_V2: u16 = 280;
const RETENTION_CHECKSUM_START: usize = 12;
const RETENTION_CHECKSUM_END: usize = 16;
const RETENTION_DIGEST_START: usize = 224;
const RETENTION_DIGEST_END: usize = 256;
const RETENTION_V2_DIGEST_START: usize = 248;
const RETENTION_V2_DIGEST_END: usize = 280;

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
    /// A retention anchor has the wrong exact length.
    #[error("native WAL retention anchor has unsupported length {actual}")]
    InvalidRetentionAnchorLength {
        /// Actual encoded length.
        actual: usize,
    },
    /// Retention-anchor magic, version, or header length is unsupported.
    #[error("invalid native WAL retention anchor preamble")]
    InvalidRetentionAnchorPreamble,
    /// Retention-anchor identity or continuity fields are invalid.
    #[error("invalid native WAL retention anchor identity")]
    InvalidRetentionAnchorIdentity,
    /// Retention-anchor CRC32C failed.
    #[error("native WAL retention anchor CRC32C mismatch")]
    RetentionAnchorChecksumMismatch,
    /// Retention-anchor BLAKE3 digest failed.
    #[error("native WAL retention anchor BLAKE3 mismatch")]
    RetentionAnchorDigestMismatch,
    /// A physical WAL base sequence and digest do not form a valid pair.
    #[error("invalid native WAL physical base")]
    InvalidPhysicalBase,
    /// A WAL-anchor-like data-directory entry is not canonical.
    #[error("unexpected native WAL retention entry")]
    UnexpectedRetentionEntry,
    /// WAL retention-anchor generations or digest links diverge.
    #[error("invalid native WAL retention anchor chain")]
    InvalidRetentionAnchorChain,
    /// One retention-anchor authority chain carries mixed directory lineage.
    #[error("native WAL retention anchor lineage does not match its authority chain")]
    RetentionAnchorLineageMismatch,
    /// A staged or final anchor publication target already exists.
    #[error("native WAL retention publication target already exists")]
    RetentionPublicationTargetExists,
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

/// Validated identity fields for one compacted native WAL prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalRetentionAnchorFields {
    /// Monotonically increasing anchor generation.
    pub epoch: u64,
    /// Last retired absolute WAL block sequence.
    pub retired_through_sequence: u64,
    /// LSN of the synchronized checkpoint record that closes the prefix.
    pub retired_checkpoint_lsn: Lsn,
    /// Digest of the block containing that checkpoint record.
    pub retired_block_digest: [u8; 32],
    /// Complete logical base CSN reconstructed from the root manifest.
    pub base_visible_csn: Csn,
    /// Immutable root-manifest generation referenced by the checkpoint.
    pub manifest_generation: ManifestGeneration,
    /// Complete referenced root-manifest digest.
    pub manifest_digest: [u8; 32],
    /// Committed root LSN carried by the referenced manifest.
    pub root_commit_lsn: Lsn,
    /// Committed root block digest carried by the referenced manifest.
    pub root_commit_block_digest: [u8; 32],
    /// First transaction ID not consumed by the retired prefix.
    pub next_transaction_id: u128,
    /// Cumulative checkpoint count through this anchor.
    pub checkpoint_count: u64,
    /// Cumulative committed transaction count through this anchor.
    pub committed_transaction_count: u64,
    /// Prior anchor digest, or zero for epoch one.
    pub previous_anchor_digest: [u8; 32],
}

/// Fixed-size, self-verifying trust root for one retired WAL prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalRetentionAnchor {
    fields: WalRetentionAnchorFields,
    lineage: Option<LineageIdentity>,
}

impl WalRetentionAnchor {
    /// Validates fields and constructs one canonical anchor.
    ///
    /// # Errors
    ///
    /// Returns an error when identities, counters, digests, or LSN/block
    /// relationships cannot describe one retained prefix.
    pub fn new(fields: WalRetentionAnchorFields) -> Result<Self, WalError> {
        Self::new_internal(fields, None)
    }

    /// Validates fields and constructs one lineage-bearing canonical anchor.
    ///
    /// # Errors
    ///
    /// Returns an error when identities, counters, digests, or LSN/block
    /// relationships cannot describe one retained prefix.
    pub fn new_with_lineage(
        fields: WalRetentionAnchorFields,
        lineage: LineageIdentity,
    ) -> Result<Self, WalError> {
        Self::new_internal(fields, Some(lineage))
    }

    fn new_internal(
        fields: WalRetentionAnchorFields,
        lineage: Option<LineageIdentity>,
    ) -> Result<Self, WalError> {
        validate_retention_anchor_fields(&fields)?;
        Ok(Self { fields, lineage })
    }

    /// Returns the validated identity fields.
    pub const fn fields(&self) -> WalRetentionAnchorFields {
        self.fields
    }

    /// Returns the directory lineage carried by v2 anchors.
    pub const fn lineage(&self) -> Option<LineageIdentity> {
        self.lineage
    }

    /// Returns the complete canonical anchor digest.
    pub fn digest(&self) -> [u8; 32] {
        encode_retention_anchor(self.fields, self.lineage).1
    }

    /// Encodes the exact canonical v1 or lineage-bearing v2 anchor.
    pub fn encode(&self) -> Vec<u8> {
        encode_retention_anchor(self.fields, self.lineage).0
    }

    /// Decodes and verifies one exact canonical anchor.
    ///
    /// # Errors
    ///
    /// Returns an error for length, preamble, checksum, digest, identity,
    /// counter, or LSN/block divergence.
    pub fn decode(encoded: &[u8]) -> Result<Self, WalError> {
        let (digest_start, digest_end, lineage) = match encoded.len() {
            WAL_RETENTION_ANCHOR_SIZE
                if encoded[0..8] == RETENTION_MAGIC_V1
                    && read_u16(&encoded[8..10]) == RETENTION_FORMAT_VERSION_V1
                    && read_u16(&encoded[10..12]) == RETENTION_HEADER_LENGTH_V1 =>
            {
                (RETENTION_DIGEST_START, RETENTION_DIGEST_END, None)
            }
            WAL_RETENTION_ANCHOR_V2_SIZE
                if encoded[0..8] == RETENTION_MAGIC_V2
                    && read_u16(&encoded[8..10]) == RETENTION_FORMAT_VERSION_V2
                    && read_u16(&encoded[10..12]) == RETENTION_HEADER_LENGTH_V2 =>
            {
                (
                    RETENTION_V2_DIGEST_START,
                    RETENTION_V2_DIGEST_END,
                    Some(
                        LineageIdentity::decode(&encoded[224..248])
                            .map_err(|_| WalError::InvalidRetentionAnchorIdentity)?,
                    ),
                )
            }
            WAL_RETENTION_ANCHOR_SIZE | WAL_RETENTION_ANCHOR_V2_SIZE => {
                return Err(WalError::InvalidRetentionAnchorPreamble);
            }
            _ => {
                return Err(WalError::InvalidRetentionAnchorLength {
                    actual: encoded.len(),
                });
            }
        };
        if retention_anchor_checksum(encoded, digest_start, digest_end)
            != read_u32(&encoded[RETENTION_CHECKSUM_START..RETENTION_CHECKSUM_END])
        {
            return Err(WalError::RetentionAnchorChecksumMismatch);
        }
        let mut stored_digest = [0_u8; 32];
        stored_digest.copy_from_slice(&encoded[digest_start..digest_end]);
        if retention_anchor_digest(encoded, digest_start, digest_end) != stored_digest {
            return Err(WalError::RetentionAnchorDigestMismatch);
        }
        let mut retired_block_digest = [0_u8; 32];
        retired_block_digest.copy_from_slice(&encoded[40..72]);
        let mut manifest_digest = [0_u8; 32];
        manifest_digest.copy_from_slice(&encoded[88..120]);
        let mut root_commit_block_digest = [0_u8; 32];
        root_commit_block_digest.copy_from_slice(&encoded[128..160]);
        let mut previous_anchor_digest = [0_u8; 32];
        previous_anchor_digest.copy_from_slice(&encoded[192..224]);
        let fields = WalRetentionAnchorFields {
            epoch: read_u64(&encoded[16..24]),
            retired_through_sequence: read_u64(&encoded[24..32]),
            retired_checkpoint_lsn: Lsn::new(read_u64(&encoded[32..40]))
                .map_err(|_| WalError::InvalidRetentionAnchorIdentity)?,
            retired_block_digest,
            base_visible_csn: Csn::new(read_u64(&encoded[72..80]))
                .map_err(|_| WalError::InvalidRetentionAnchorIdentity)?,
            manifest_generation: ManifestGeneration::new(read_u64(&encoded[80..88]))
                .map_err(|_| WalError::InvalidRetentionAnchorIdentity)?,
            manifest_digest,
            root_commit_lsn: Lsn::new(read_u64(&encoded[120..128]))
                .map_err(|_| WalError::InvalidRetentionAnchorIdentity)?,
            root_commit_block_digest,
            next_transaction_id: read_u128(&encoded[160..176]),
            checkpoint_count: read_u64(&encoded[176..184]),
            committed_transaction_count: read_u64(&encoded[184..192]),
            previous_anchor_digest,
        };
        let anchor = Self::new_internal(fields, lineage)?;
        if anchor.digest() != stored_digest {
            return Err(WalError::RetentionAnchorDigestMismatch);
        }
        Ok(anchor)
    }
}

fn validate_retention_anchor_fields(fields: &WalRetentionAnchorFields) -> Result<(), WalError> {
    let checkpoint_sequence = sequence_for_record_lsn(fields.retired_checkpoint_lsn)?;
    let root_sequence = sequence_for_record_lsn(fields.root_commit_lsn)?;
    let previous_digest_is_valid = if fields.epoch == 1 {
        fields.previous_anchor_digest == [0; 32]
    } else {
        fields.previous_anchor_digest != [0; 32]
    };
    if fields.epoch == 0
        || fields.retired_through_sequence == 0
        || checkpoint_sequence != fields.retired_through_sequence
        || root_sequence > fields.retired_through_sequence
        || fields.root_commit_lsn >= fields.retired_checkpoint_lsn
        || fields.retired_block_digest == [0; 32]
        || fields.manifest_digest == [0; 32]
        || fields.root_commit_block_digest == [0; 32]
        || fields.next_transaction_id == 0
        || fields.checkpoint_count == 0
        || fields.committed_transaction_count != fields.base_visible_csn.get()
        || !previous_digest_is_valid
    {
        return Err(WalError::InvalidRetentionAnchorIdentity);
    }
    Ok(())
}

fn sequence_for_record_lsn(lsn: Lsn) -> Result<u64, WalError> {
    let raw = lsn.get();
    let offset = raw % WAL_BLOCK_SIZE_U64;
    if offset < WAL_BLOCK_HEADER_SIZE_U64 {
        return Err(WalError::InvalidRetentionAnchorIdentity);
    }
    raw.checked_div(WAL_BLOCK_SIZE_U64)
        .and_then(|sequence| sequence.checked_add(1))
        .ok_or(WalError::InvalidRetentionAnchorIdentity)
}

fn encode_retention_anchor(
    fields: WalRetentionAnchorFields,
    lineage: Option<LineageIdentity>,
) -> (Vec<u8>, [u8; 32]) {
    let (encoded_size, magic, format_version, header_length, digest_start, digest_end) =
        if lineage.is_some() {
            (
                WAL_RETENTION_ANCHOR_V2_SIZE,
                RETENTION_MAGIC_V2,
                RETENTION_FORMAT_VERSION_V2,
                RETENTION_HEADER_LENGTH_V2,
                RETENTION_V2_DIGEST_START,
                RETENTION_V2_DIGEST_END,
            )
        } else {
            (
                WAL_RETENTION_ANCHOR_SIZE,
                RETENTION_MAGIC_V1,
                RETENTION_FORMAT_VERSION_V1,
                RETENTION_HEADER_LENGTH_V1,
                RETENTION_DIGEST_START,
                RETENTION_DIGEST_END,
            )
        };
    let mut encoded = vec![0_u8; encoded_size];
    encoded[0..8].copy_from_slice(&magic);
    encoded[8..10].copy_from_slice(&format_version.to_le_bytes());
    encoded[10..12].copy_from_slice(&header_length.to_le_bytes());
    encoded[16..24].copy_from_slice(&fields.epoch.to_le_bytes());
    encoded[24..32].copy_from_slice(&fields.retired_through_sequence.to_le_bytes());
    encoded[32..40].copy_from_slice(&fields.retired_checkpoint_lsn.get().to_le_bytes());
    encoded[40..72].copy_from_slice(&fields.retired_block_digest);
    encoded[72..80].copy_from_slice(&fields.base_visible_csn.get().to_le_bytes());
    encoded[80..88].copy_from_slice(&fields.manifest_generation.get().to_le_bytes());
    encoded[88..120].copy_from_slice(&fields.manifest_digest);
    encoded[120..128].copy_from_slice(&fields.root_commit_lsn.get().to_le_bytes());
    encoded[128..160].copy_from_slice(&fields.root_commit_block_digest);
    encoded[160..176].copy_from_slice(&fields.next_transaction_id.to_le_bytes());
    encoded[176..184].copy_from_slice(&fields.checkpoint_count.to_le_bytes());
    encoded[184..192].copy_from_slice(&fields.committed_transaction_count.to_le_bytes());
    encoded[192..224].copy_from_slice(&fields.previous_anchor_digest);
    if let Some(lineage) = lineage {
        encoded[224..248].copy_from_slice(&lineage.encode());
    }
    let checksum = retention_anchor_checksum(&encoded, digest_start, digest_end);
    encoded[RETENTION_CHECKSUM_START..RETENTION_CHECKSUM_END]
        .copy_from_slice(&checksum.to_le_bytes());
    let digest = retention_anchor_digest(&encoded, digest_start, digest_end);
    encoded[digest_start..digest_end].copy_from_slice(&digest);
    (encoded, digest)
}

fn retention_anchor_checksum(encoded: &[u8], digest_start: usize, digest_end: usize) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[RETENTION_CHECKSUM_START..RETENTION_CHECKSUM_END].fill(0);
    canonical[digest_start..digest_end].fill(0);
    crc32c::crc32c(&canonical)
}

fn retention_anchor_digest(encoded: &[u8], digest_start: usize, digest_end: usize) -> [u8; 32] {
    let mut canonical = encoded.to_vec();
    canonical[digest_start..digest_end].fill(0);
    *blake3::hash(&canonical).as_bytes()
}

/// One create-new retention anchor not yet visible under its final name.
#[derive(Debug)]
pub struct StagedWalRetentionAnchor {
    anchor: WalRetentionAnchor,
    temporary_path: PathBuf,
    candidate_path: PathBuf,
}

/// Verified retention-anchor directory state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalRetentionRecovery {
    /// Stable anchors in increasing epoch order.
    pub anchors: Vec<WalRetentionAnchor>,
    /// Synchronized candidate that authorizes an in-progress WAL reset.
    pub candidate: Option<WalRetentionAnchor>,
    /// Abandoned canonical create-new stages removed during open.
    pub ignored_temporary_files: usize,
    /// Whether strict parent-directory synchronization is supported.
    pub parent_sync_supported: bool,
}

/// Create-new publication and recovery for native WAL retention anchors.
#[derive(Debug)]
pub struct WalRetentionStore {
    directory: PathBuf,
    anchors: Vec<WalRetentionAnchor>,
    candidate: Option<WalRetentionAnchor>,
    ignored_temporary_files: usize,
}

impl WalRetentionStore {
    /// Creates empty retention state in an existing data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be inspected or already
    /// contains a WAL retention entry.
    pub fn create(data_directory: impl AsRef<Path>) -> Result<Self, WalError> {
        let directory = data_directory.as_ref();
        for entry in fs::read_dir(directory)? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| WalError::UnexpectedRetentionEntry)?;
            if name.starts_with("wal-anchor-") {
                return Err(WalError::RetentionPublicationTargetExists);
            }
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            anchors: Vec::new(),
            candidate: None,
            ignored_temporary_files: 0,
        })
    }

    /// Opens and verifies stable anchors and removes abandoned stages.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, noncanonical lookalikes, corrupt anchors, or
    /// more than one prior/candidate transition.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, WalError> {
        let directory = data_directory.as_ref();
        let mut finals = Vec::new();
        let mut candidates = Vec::new();
        let mut temporary_paths = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| WalError::UnexpectedRetentionEntry)?;
            if !name.starts_with("wal-anchor-") {
                continue;
            }
            if !entry.file_type()?.is_file() {
                return Err(WalError::UnexpectedRetentionEntry);
            }
            if let Some(epoch) = parse_retention_anchor_filename(&name) {
                finals.push((epoch, entry.path()));
            } else if parse_retention_anchor_temporary_filename(&name).is_some() {
                temporary_paths.push(entry.path());
            } else if let Some(epoch) = parse_retention_anchor_candidate_filename(&name) {
                candidates.push((epoch, entry.path()));
            } else {
                return Err(WalError::UnexpectedRetentionEntry);
            }
        }
        finals.sort_by_key(|(epoch, _)| *epoch);
        if finals.len() > 2 || candidates.len() > 1 || (finals.len() == 2 && !candidates.is_empty())
        {
            return Err(WalError::InvalidRetentionAnchorChain);
        }
        let mut anchors = Vec::with_capacity(finals.len());
        for (epoch, path) in finals {
            let anchor = WalRetentionAnchor::decode(&fs::read(path)?)?;
            if anchor.fields().epoch != epoch {
                return Err(WalError::InvalidRetentionAnchorChain);
            }
            if anchors
                .last()
                .is_some_and(|prior: &WalRetentionAnchor| prior.lineage != anchor.lineage)
            {
                return Err(WalError::RetentionAnchorLineageMismatch);
            }
            anchors.push(anchor);
        }
        let candidate = if let Some((epoch, path)) = candidates.pop() {
            let candidate = WalRetentionAnchor::decode(&fs::read(path)?)?;
            let expected = anchors.last().map_or(Ok((1, [0; 32])), |prior| {
                prior
                    .fields()
                    .epoch
                    .checked_add(1)
                    .map(|next| (next, prior.digest()))
                    .ok_or(WalError::InvalidRetentionAnchorChain)
            })?;
            if candidate.fields().epoch != epoch
                || (
                    candidate.fields().epoch,
                    candidate.fields().previous_anchor_digest,
                ) != expected
            {
                return Err(WalError::InvalidRetentionAnchorChain);
            }
            if anchors
                .last()
                .is_some_and(|prior| prior.lineage != candidate.lineage)
            {
                return Err(WalError::RetentionAnchorLineageMismatch);
            }
            Some(candidate)
        } else {
            None
        };
        if let [prior, candidate] = anchors.as_slice()
            && (prior.fields().epoch.checked_add(1) != Some(candidate.fields().epoch)
                || candidate.fields().previous_anchor_digest != prior.digest())
        {
            return Err(WalError::InvalidRetentionAnchorChain);
        }
        for temporary_path in &temporary_paths {
            fs::remove_file(temporary_path)?;
        }
        if !temporary_paths.is_empty() {
            sync_directory(directory)?;
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            anchors,
            candidate,
            ignored_temporary_files: temporary_paths.len(),
        })
    }

    /// Returns stable anchor recovery evidence.
    pub fn recovery(&self) -> WalRetentionRecovery {
        WalRetentionRecovery {
            anchors: self.anchors.clone(),
            candidate: self.candidate,
            ignored_temporary_files: self.ignored_temporary_files,
            parent_sync_supported: parent_sync_supported(),
        }
    }

    /// Returns the latest stable anchor.
    pub fn current(&self) -> Option<&WalRetentionAnchor> {
        self.anchors.last()
    }

    /// Requires every stable and pending anchor to carry one exact lineage.
    ///
    /// # Errors
    ///
    /// Returns an error for a legacy anchor or any different identity.
    pub fn validate_lineage(&self, expected: LineageIdentity) -> Result<(), WalError> {
        let stable_mismatch = self
            .anchors
            .iter()
            .any(|anchor| anchor.lineage != Some(expected));
        let candidate_mismatch = self
            .candidate
            .is_some_and(|anchor| anchor.lineage != Some(expected));
        if stable_mismatch || candidate_mismatch {
            return Err(WalError::RetentionAnchorLineageMismatch);
        }
        Ok(())
    }

    /// Writes one synchronized create-new anchor stage.
    ///
    /// # Errors
    ///
    /// Returns an error for a noncontiguous anchor, unresolved prior
    /// transition, existing target, or uncertain I/O.
    pub fn stage(
        &self,
        anchor: WalRetentionAnchor,
        synchronize: bool,
    ) -> Result<StagedWalRetentionAnchor, WalError> {
        self.validate_next(&anchor)?;
        let stem = retention_anchor_stem(anchor.fields().epoch);
        let temporary_path = self.directory.join(format!("{stem}.hywa.tmp"));
        let candidate_path = self.directory.join(format!("{stem}.hywa.pending"));
        let final_path = self.directory.join(format!("{stem}.hywa"));
        if temporary_path.exists() || candidate_path.exists() || final_path.exists() {
            return Err(WalError::RetentionPublicationTargetExists);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(&anchor.encode())?;
        if synchronize {
            file.sync_all()?;
        }
        drop(file);
        Ok(StagedWalRetentionAnchor {
            anchor,
            temporary_path,
            candidate_path,
        })
    }

    /// Publishes one staged candidate before the destructive WAL reset.
    ///
    /// # Errors
    ///
    /// Returns an error for chain races, target collision, rename, or
    /// directory synchronization failure.
    pub fn publish_candidate(
        &mut self,
        staged: StagedWalRetentionAnchor,
        synchronize: bool,
    ) -> Result<WalRetentionAnchor, WalError> {
        let StagedWalRetentionAnchor {
            anchor,
            temporary_path,
            candidate_path,
        } = staged;
        self.validate_next(&anchor)?;
        if candidate_path.exists() {
            return Err(WalError::RetentionPublicationTargetExists);
        }
        fs::rename(temporary_path, candidate_path)?;
        if synchronize {
            sync_directory(&self.directory)?;
        }
        self.candidate = Some(anchor);
        Ok(anchor)
    }

    /// Promotes a published candidate after the WAL reset is synchronized.
    ///
    /// # Errors
    ///
    /// Returns an error when the candidate identity diverges, a final target
    /// exists, or rename/directory synchronization fails.
    pub fn stabilize_candidate(
        &mut self,
        epoch: u64,
        synchronize: bool,
    ) -> Result<WalRetentionAnchor, WalError> {
        let candidate = self
            .candidate
            .filter(|candidate| candidate.fields().epoch == epoch)
            .ok_or(WalError::InvalidRetentionAnchorChain)?;
        let stem = retention_anchor_stem(epoch);
        let candidate_path = self.directory.join(format!("{stem}.hywa.pending"));
        let final_path = self.directory.join(format!("{stem}.hywa"));
        if final_path.exists() {
            return Err(WalError::RetentionPublicationTargetExists);
        }
        fs::rename(candidate_path, final_path)?;
        if synchronize {
            sync_directory(&self.directory)?;
        }
        self.anchors.push(candidate);
        self.candidate = None;
        Ok(candidate)
    }

    /// Removes stable anchors older than the retained epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when the retained epoch is absent or file/directory
    /// synchronization fails.
    pub fn remove_before(
        &mut self,
        retained_epoch: u64,
        synchronize: bool,
    ) -> Result<usize, WalError> {
        if !self
            .anchors
            .iter()
            .any(|anchor| anchor.fields().epoch == retained_epoch)
        {
            return Err(WalError::InvalidRetentionAnchorChain);
        }
        let obsolete = self
            .anchors
            .iter()
            .filter(|anchor| anchor.fields().epoch < retained_epoch)
            .copied()
            .collect::<Vec<_>>();
        for anchor in &obsolete {
            fs::remove_file(self.directory.join(format!(
                "{}.hywa",
                retention_anchor_stem(anchor.fields().epoch)
            )))?;
        }
        self.anchors
            .retain(|anchor| anchor.fields().epoch >= retained_epoch);
        if synchronize && !obsolete.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(obsolete.len())
    }

    fn validate_next(&self, anchor: &WalRetentionAnchor) -> Result<(), WalError> {
        if self.anchors.len() >= 2 || self.candidate.is_some() {
            return Err(WalError::InvalidRetentionAnchorChain);
        }
        let (expected_epoch, expected_previous_digest) = if let Some(current) = self.current() {
            if current.lineage != anchor.lineage {
                return Err(WalError::RetentionAnchorLineageMismatch);
            }
            (
                current
                    .fields()
                    .epoch
                    .checked_add(1)
                    .ok_or(WalError::InvalidRetentionAnchorChain)?,
                current.digest(),
            )
        } else {
            (1, [0; 32])
        };
        if anchor.fields().epoch != expected_epoch
            || anchor.fields().previous_anchor_digest != expected_previous_digest
        {
            return Err(WalError::InvalidRetentionAnchorChain);
        }
        Ok(())
    }
}

fn retention_anchor_stem(epoch: u64) -> String {
    format!("wal-anchor-{epoch:020}")
}

fn parse_retention_anchor_filename(name: &str) -> Option<u64> {
    let value = name.strip_prefix("wal-anchor-")?.strip_suffix(".hywa")?;
    parse_retention_anchor_epoch(value)
}

fn parse_retention_anchor_temporary_filename(name: &str) -> Option<u64> {
    let value = name
        .strip_prefix("wal-anchor-")?
        .strip_suffix(".hywa.tmp")?;
    parse_retention_anchor_epoch(value)
}

fn parse_retention_anchor_candidate_filename(name: &str) -> Option<u64> {
    let value = name
        .strip_prefix("wal-anchor-")?
        .strip_suffix(".hywa.pending")?;
    parse_retention_anchor_epoch(value)
}

fn parse_retention_anchor_epoch(value: &str) -> Option<u64> {
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|epoch| *epoch != 0)
}

/// Returns whether strict parent-directory synchronization is implemented.
pub const fn parent_sync_supported() -> bool {
    cfg!(unix)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), io::Error> {
    File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> Result<(), io::Error> {
    fs::metadata(directory).map(|_| ())
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
    /// Absolute block sequence immediately preceding this physical file.
    pub base_sequence: u64,
    /// Digest of that preceding block, or zero at genesis.
    pub base_digest: [u8; 32],
    /// Complete records in physical order.
    pub records: Vec<WalRecord>,
    /// Verified block receipts in physical order.
    pub blocks: Vec<BlockReceipt>,
    /// Incomplete physical tail removed during open.
    pub truncated_tail_bytes: u64,
    /// Last complete absolute block sequence, or the base for an empty file.
    pub last_sequence: u64,
    /// Last complete block digest, or the base digest for an empty file.
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
    /// Returns the last absolute block sequence known by this writer.
    pub const fn last_sequence(&self) -> u64 {
        self.next_sequence - 1
    }

    /// Returns the digest preceding the next block append.
    pub const fn last_digest(&self) -> [u8; 32] {
        self.previous_digest
    }

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
        Self::open_after(path, 0, [0; 32])
    }

    /// Opens a retained WAL suffix after one verified absolute block base.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid base pair, I/O, any complete corrupt
    /// block, or exhausted absolute block address space.
    pub fn open_after(
        path: impl AsRef<Path>,
        base_sequence: u64,
        base_digest: [u8; 32],
    ) -> Result<OpenedWal, WalError> {
        if (base_sequence == 0) != (base_digest == [0; 32]) {
            return Err(WalError::InvalidPhysicalBase);
        }
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        let complete_blocks = length / WAL_BLOCK_SIZE_U64;
        let tail_bytes = length % WAL_BLOCK_SIZE_U64;
        let mut previous_digest = base_digest;
        let mut records = Vec::new();
        let mut blocks = Vec::new();
        for index in 0..complete_blocks {
            let sequence = base_sequence
                .checked_add(index)
                .and_then(|value| value.checked_add(1))
                .ok_or(WalError::AddressExhausted)?;
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
        let last_sequence = base_sequence
            .checked_add(complete_blocks)
            .ok_or(WalError::AddressExhausted)?;
        let next_sequence = last_sequence
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
                base_sequence,
                base_digest,
                records,
                blocks,
                truncated_tail_bytes: tail_bytes,
                last_sequence,
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

    /// Retires the complete current physical file and resumes after one
    /// verified absolute block base.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid base, exhausted address space, or
    /// uncertain truncate, seek, or synchronization. Any physical failure
    /// poisons the writer and requires reopen.
    pub fn reset_after(
        &mut self,
        retired_through_sequence: u64,
        retired_block_digest: [u8; 32],
        synchronize: bool,
    ) -> Result<(), WalError> {
        if retired_through_sequence == 0 || retired_block_digest == [0; 32] {
            return Err(WalError::InvalidPhysicalBase);
        }
        let next_sequence = retired_through_sequence
            .checked_add(1)
            .ok_or(WalError::AddressExhausted)?;
        self.poisoned = true;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        if synchronize {
            self.file.sync_data()?;
        }
        self.next_sequence = next_sequence;
        self.previous_digest = retired_block_digest;
        self.poisoned = false;
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

    use hyphae_native_types::{
        Csn, DirectoryUuid, EngineKind, HistoryEpoch, LineageIdentity, Lsn, ManifestGeneration,
        TransactionId,
    };

    use super::{
        PendingRecord, RecordKind, WAL_BLOCK_HEADER_SIZE, WAL_BLOCK_SIZE, WAL_RECORD_HEADER_SIZE,
        WAL_RETENTION_ANCHOR_SIZE, WAL_RETENTION_ANCHOR_V2_SIZE, WalBlock, WalError, WalFile,
        WalRetentionAnchor, WalRetentionAnchorFields, WalRetentionStore,
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

    fn retention_anchor_fields() -> Result<WalRetentionAnchorFields, Box<dyn std::error::Error>> {
        Ok(WalRetentionAnchorFields {
            epoch: 2,
            retired_through_sequence: 7,
            retired_checkpoint_lsn: Lsn::new(6 * 65_536 + 112)?,
            retired_block_digest: [7; 32],
            base_visible_csn: Csn::new(41)?,
            manifest_generation: ManifestGeneration::new(3)?,
            manifest_digest: [3; 32],
            root_commit_lsn: Lsn::new(5 * 65_536 + 112)?,
            root_commit_block_digest: [5; 32],
            next_transaction_id: 99,
            checkpoint_count: 3,
            committed_transaction_count: 41,
            previous_anchor_digest: [1; 32],
        })
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

    #[test]
    fn retention_anchor_codec_is_exact_and_rejects_every_truncated_prefix()
    -> Result<(), Box<dyn std::error::Error>> {
        let anchor = WalRetentionAnchor::new(retention_anchor_fields()?)?;
        let encoded = anchor.encode();
        assert_eq!(encoded.len(), WAL_RETENTION_ANCHOR_SIZE);
        assert_eq!(&encoded[0..8], b"HYWAR001");
        assert_eq!(WalRetentionAnchor::decode(&encoded)?, anchor);

        for truncated_length in 0..WAL_RETENTION_ANCHOR_SIZE {
            assert!(WalRetentionAnchor::decode(&encoded[..truncated_length]).is_err());
        }

        for offset in [0, 8, 10, 16, 24, 32, 72, 80, 120, 160, 176, 184, 224] {
            let mut corrupt = encoded.clone();
            corrupt[offset] ^= 1;
            assert!(WalRetentionAnchor::decode(&corrupt).is_err());
        }
        Ok(())
    }

    #[test]
    fn lineage_retention_anchor_v2_has_golden_offsets_and_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        let lineage = LineageIdentity::new(
            DirectoryUuid::parse_canonical("018f4e9d-3d7a-7b6c-8f12-123456789abc")?,
            HistoryEpoch::new(42)?,
        );
        let anchor = WalRetentionAnchor::new_with_lineage(retention_anchor_fields()?, lineage)?;
        let encoded = anchor.encode();

        assert_eq!(encoded.len(), WAL_RETENTION_ANCHOR_V2_SIZE);
        assert_eq!(&encoded[..8], b"HYWAR002");
        assert_eq!(&encoded[8..10], &2_u16.to_le_bytes());
        assert_eq!(&encoded[10..12], &280_u16.to_le_bytes());
        assert_eq!(&encoded[224..248], &lineage.encode());
        assert_eq!(WalRetentionAnchor::decode(&encoded)?, anchor);
        assert_eq!(anchor.lineage(), Some(lineage));
        Ok(())
    }

    #[test]
    fn lineage_retention_anchor_v2_rejects_every_truncated_prefix_and_single_byte_corruption()
    -> Result<(), Box<dyn std::error::Error>> {
        let lineage = LineageIdentity::new(
            DirectoryUuid::parse_canonical("018f4e9d-3d7a-7b6c-8f12-123456789abc")?,
            HistoryEpoch::new(42)?,
        );
        let anchor = WalRetentionAnchor::new_with_lineage(retention_anchor_fields()?, lineage)?;
        let encoded = anchor.encode();

        for truncated_length in 0..encoded.len() {
            assert!(WalRetentionAnchor::decode(&encoded[..truncated_length]).is_err());
        }
        for offset in 0..encoded.len() {
            let mut corrupt = encoded.clone();
            corrupt[offset] ^= 1;
            assert!(WalRetentionAnchor::decode(&corrupt).is_err());
        }
        Ok(())
    }

    #[test]
    fn retention_store_rejects_mixed_lineage() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let mut store = WalRetentionStore::create(temporary.path())?;
        let mut first_fields = retention_anchor_fields()?;
        first_fields.epoch = 1;
        first_fields.previous_anchor_digest = [0; 32];
        let first = WalRetentionAnchor::new_with_lineage(first_fields, lineage(1)?)?;
        store.publish_candidate(store.stage(first, false)?, false)?;
        store.stabilize_candidate(1, false)?;

        let mut second_fields = retention_anchor_fields()?;
        second_fields.previous_anchor_digest = first.digest();
        let divergent = WalRetentionAnchor::new_with_lineage(second_fields, lineage(2)?)?;
        assert!(matches!(
            store.stage(divergent, false),
            Err(WalError::RetentionAnchorLineageMismatch)
        ));
        assert!(matches!(
            store.validate_lineage(lineage(2)?),
            Err(WalError::RetentionAnchorLineageMismatch)
        ));
        Ok(())
    }

    #[test]
    fn anchored_wal_preserves_absolute_sequences_lsns_and_digest_chain()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        fs::write(temporary.wal_file(), [])?;
        let mut opened = WalFile::open_after(temporary.wal_file(), 7, [7; 32])?;
        assert_eq!(opened.recovery.base_sequence, 7);
        assert_eq!(opened.recovery.base_digest, [7; 32]);
        assert_eq!(opened.recovery.last_sequence, 7);

        let receipt = opened.wal.append_records(
            vec![pending(
                RecordKind::Begin,
                EngineKind::Kernel,
                99,
                b"anchored",
            )?],
            true,
        )?;
        assert_eq!(receipt[0].sequence, 8);
        assert_eq!(receipt[0].first_lsn, Lsn::new(7 * 65_536 + 112)?);
        drop(opened);

        let reopened = WalFile::open_after(temporary.wal_file(), 7, [7; 32])?;
        assert_eq!(reopened.recovery.blocks[0], receipt[0]);
        assert_eq!(reopened.recovery.records[0].body(), b"anchored");
        assert_eq!(reopened.recovery.last_sequence, 8);
        Ok(())
    }

    #[test]
    fn retention_store_publishes_recovers_and_prunes_anchor_generations()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let store = WalRetentionStore::create(temporary.path())?;

        let mut first_fields = retention_anchor_fields()?;
        first_fields.epoch = 1;
        first_fields.previous_anchor_digest = [0; 32];
        let first = WalRetentionAnchor::new(first_fields)?;
        let abandoned = store.stage(first, true)?;
        drop(abandoned);
        drop(store);

        let mut reopened = WalRetentionStore::open(temporary.path())?;
        assert!(reopened.current().is_none());
        assert_eq!(reopened.recovery().ignored_temporary_files, 1);
        let first = reopened.publish_candidate(reopened.stage(first, true)?, true)?;
        assert_eq!(reopened.recovery().candidate, Some(first));
        drop(reopened);

        let mut reopened = WalRetentionStore::open(temporary.path())?;
        assert!(reopened.current().is_none());
        assert_eq!(reopened.recovery().candidate, Some(first));
        reopened.stabilize_candidate(first.fields().epoch, true)?;
        assert_eq!(reopened.recovery().candidate, None);

        let mut second_fields = retention_anchor_fields()?;
        second_fields.previous_anchor_digest = first.digest();
        let second = WalRetentionAnchor::new(second_fields)?;
        let second = reopened.publish_candidate(reopened.stage(second, true)?, true)?;
        assert_eq!(reopened.recovery().anchors, vec![first]);
        assert_eq!(reopened.recovery().candidate, Some(second));
        reopened.stabilize_candidate(second.fields().epoch, true)?;
        assert_eq!(reopened.recovery().anchors, vec![first, second]);
        assert_eq!(reopened.recovery().candidate, None);

        assert_eq!(reopened.remove_before(second.fields().epoch, true)?, 1);
        drop(reopened);
        let stable = WalRetentionStore::open(temporary.path())?;
        assert_eq!(stable.recovery().anchors, vec![second]);
        Ok(())
    }

    #[test]
    fn wal_reset_reopens_at_the_retired_absolute_base() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new()?;
        let mut wal = WalFile::create(temporary.wal_file())?;
        let first = wal.append_records(
            vec![pending(RecordKind::Begin, EngineKind::Kernel, 1, b"first")?],
            true,
        )?[0];
        wal.reset_after(first.sequence, first.digest, true)?;
        let second = wal.append_records(
            vec![pending(
                RecordKind::Begin,
                EngineKind::Kernel,
                2,
                b"second",
            )?],
            true,
        )?[0];
        assert_eq!(second.sequence, 2);
        drop(wal);

        let reopened = WalFile::open_after(temporary.wal_file(), first.sequence, first.digest)?;
        assert_eq!(reopened.recovery.records.len(), 1);
        assert_eq!(reopened.recovery.records[0].body(), b"second");
        assert_eq!(reopened.recovery.blocks[0], second);
        Ok(())
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
