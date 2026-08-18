// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroU64;

use hyphae_native_types::{Csn, DurabilityClass, TransactionId};
use thiserror::Error;

/// Fixed payload width for one explicit local `BEGIN`.
pub const LOCAL_TRANSACTION_BEGIN_SIZE: usize = 8;
/// Fixed payload width for one explicit local `BEGUN` receipt.
pub const LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE: usize = 32;
/// Fixed payload width for one transaction-bound mutation receipt.
pub const LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE: usize = 32;
/// Fixed payload width for one explicit local `COMMIT`.
pub const LOCAL_TRANSACTION_COMMIT_SIZE: usize = 24;
/// Fixed payload width for one explicit local `COMMITTED` receipt.
pub const LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE: usize = 40;
/// Fixed payload width for one explicit local `ROLLBACK`.
pub const LOCAL_TRANSACTION_ROLLBACK_SIZE: usize = 16;
/// Fixed payload width for one explicit local `ROLLED_BACK` receipt.
pub const LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE: usize = 24;
/// Maximum successfully staged operations in one local transaction.
pub const MAX_LOCAL_TRANSACTION_OPERATIONS: usize = 1_024;

const TRANSACTION_VERSION: u8 = 1;
const CONTROL_OPCODE: u8 = 1;
const BEGUN_RECEIPT_TAG: u8 = 1;
const STAGED_RECEIPT_TAG: u8 = 2;
const COMMITTED_RECEIPT_TAG: u8 = 3;
const ROLLED_BACK_RECEIPT_TAG: u8 = 4;

/// Native engine family affected by one staged local operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LocalTransactionEngine {
    /// Native relational engine.
    Relational = 1,
    /// Native structure engine.
    Structure = 2,
    /// Native lexical/search engine.
    Search = 3,
}

impl TryFrom<u8> for LocalTransactionEngine {
    type Error = LocalTransactionCodecError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Relational),
            2 => Ok(Self::Structure),
            3 => Ok(Self::Search),
            _ => Err(LocalTransactionCodecError::UnknownEngine(value)),
        }
    }
}

/// Stable receipt returned after opening one detached native batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTransactionBeginReceipt {
    /// Durability fixed for the complete transaction.
    pub durability: DurabilityClass,
    /// Nonzero connection-local transaction handle.
    pub handle: NonZeroU64,
    /// Immutable read CSN, or `None` before the first commit.
    pub read_csn: Option<Csn>,
    /// Server logical time sampled once at `BEGIN`.
    pub logical_time_micros: i64,
}

/// Stable acknowledgement for one transaction-bound engine mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTransactionStageReceipt {
    /// Native engine family affected by the operation.
    pub engine: LocalTransactionEngine,
    /// Matching connection-local transaction handle.
    pub handle: NonZeroU64,
    /// One-based successful operation ordinal.
    pub operation_ordinal: u64,
    /// Logical SQL rows affected, or one for structure/search.
    pub rows_affected: u64,
}

/// Stable receipt returned after one all-engine commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTransactionCommitReceipt {
    /// Durability satisfied before acknowledgement.
    pub durability: DurabilityClass,
    /// Connection-local transaction handle.
    pub handle: NonZeroU64,
    /// Durable native transaction identity allocated at writer admission.
    pub transaction_id: TransactionId,
    /// Single CSN shared by every affected engine.
    pub commit_csn: Csn,
    /// Number of successful client operations committed.
    pub staged_operations: u32,
}

/// Stable receipt returned after discarding one detached batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTransactionRollbackReceipt {
    /// Connection-local transaction handle.
    pub handle: NonZeroU64,
    /// Number of successful staged operations discarded.
    pub discarded_operations: u64,
}

/// Canonical explicit-transaction payload failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalTransactionCodecError {
    /// A fixed transaction payload is incomplete.
    #[error("native local transaction payload is truncated")]
    Truncated,
    /// The transaction payload version is unsupported.
    #[error("native local transaction payload version {0} is unsupported")]
    UnsupportedVersion(u8),
    /// Reserved payload bytes are nonzero.
    #[error("native local transaction reserved bytes are nonzero")]
    ReservedBytes,
    /// A fixed payload carries trailing bytes.
    #[error("native local transaction payload length mismatch")]
    LengthMismatch,
    /// A control opcode is unknown.
    #[error("native local transaction opcode {0} is unknown")]
    UnknownOpcode(u8),
    /// A receipt tag is unknown.
    #[error("native local transaction receipt tag {0} is unknown")]
    UnknownReceiptTag(u8),
    /// A staged engine tag is unknown.
    #[error("native local transaction engine tag {0} is unknown")]
    UnknownEngine(u8),
    /// A durability byte is unknown.
    #[error("native local transaction durability {0} is unknown")]
    UnknownDurability(u8),
    /// A known durability class is not implemented by this session.
    #[error("native local transaction durability {0} is unsupported")]
    UnsupportedDurability(u8),
    /// A handle, WAL transaction, or commit CSN identity is zero.
    #[error("native local transaction identity is invalid")]
    InvalidIdentity,
    /// A staged-operation count is outside its canonical bound.
    #[error("native local transaction operation count is invalid")]
    InvalidOperationCount,
}

/// Encodes one fixed explicit local `BEGIN`.
///
/// # Errors
///
/// Returns an error for group or unknown durability.
pub fn encode_local_transaction_begin(
    buffer: &mut Vec<u8>,
    durability: DurabilityClass,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_durability(durability)?;
    buffer.resize(LOCAL_TRANSACTION_BEGIN_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = CONTROL_OPCODE;
    buffer[2] = durability as u8;
    Ok(buffer)
}

/// Decodes one fixed explicit local `BEGIN`.
///
/// # Errors
///
/// Returns an error for length, version, opcode, reserved, or durability
/// violations.
pub fn decode_local_transaction_begin(
    payload: &[u8],
) -> Result<DurabilityClass, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_BEGIN_SIZE)?;
    validate_control(payload)?;
    require_zero(&payload[3..LOCAL_TRANSACTION_BEGIN_SIZE])?;
    decode_durability(payload[2])
}

/// Encodes one fixed `BEGUN` receipt.
///
/// # Errors
///
/// Returns an error for unsupported durability.
pub fn encode_local_transaction_begin_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalTransactionBeginReceipt,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_durability(receipt.durability)?;
    buffer.resize(LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = BEGUN_RECEIPT_TAG;
    buffer[2] = receipt.durability as u8;
    buffer[4..12].copy_from_slice(&receipt.handle.get().to_le_bytes());
    buffer[12..20].copy_from_slice(&receipt.read_csn.map_or(0, Csn::get).to_le_bytes());
    buffer[20..28].copy_from_slice(&receipt.logical_time_micros.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed `BEGUN` receipt.
///
/// # Errors
///
/// Returns an error for malformed receipt identity, durability, or reserved
/// bytes.
pub fn decode_local_transaction_begin_receipt(
    payload: &[u8],
) -> Result<LocalTransactionBeginReceipt, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE)?;
    validate_receipt(payload, BEGUN_RECEIPT_TAG)?;
    require_zero(&payload[3..4])?;
    require_zero(&payload[28..32])?;
    let durability = decode_durability(payload[2])?;
    let handle = decode_handle(payload, 4)?;
    let read_csn = match read_u64(payload, 12)? {
        0 => None,
        value => Some(Csn::new(value).map_err(|_| LocalTransactionCodecError::InvalidIdentity)?),
    };
    Ok(LocalTransactionBeginReceipt {
        durability,
        handle,
        read_csn,
        logical_time_micros: read_i64(payload, 20)?,
    })
}

/// Encodes one fixed engine mutation acknowledgement.
///
/// # Errors
///
/// Returns an error when the operation ordinal exceeds the local bound.
pub fn encode_local_transaction_stage_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalTransactionStageReceipt,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_operation_count(receipt.operation_ordinal, false)?;
    buffer.resize(LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = STAGED_RECEIPT_TAG;
    buffer[2] = receipt.engine as u8;
    buffer[4..12].copy_from_slice(&receipt.handle.get().to_le_bytes());
    buffer[12..20].copy_from_slice(&receipt.operation_ordinal.to_le_bytes());
    buffer[20..28].copy_from_slice(&receipt.rows_affected.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed engine mutation acknowledgement.
///
/// # Errors
///
/// Returns an error for malformed engine, handle, ordinal, or reserved bytes.
pub fn decode_local_transaction_stage_receipt(
    payload: &[u8],
) -> Result<LocalTransactionStageReceipt, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE)?;
    validate_receipt(payload, STAGED_RECEIPT_TAG)?;
    require_zero(&payload[3..4])?;
    require_zero(&payload[28..32])?;
    let operation_ordinal = read_u64(payload, 12)?;
    validate_operation_count(operation_ordinal, false)?;
    Ok(LocalTransactionStageReceipt {
        engine: LocalTransactionEngine::try_from(payload[2])?,
        handle: decode_handle(payload, 4)?,
        operation_ordinal,
        rows_affected: read_u64(payload, 20)?,
    })
}

/// Encodes one fixed explicit local `COMMIT`.
///
/// # Errors
///
/// Returns an error when the expected count is zero or exceeds the bound.
pub fn encode_local_transaction_commit(
    buffer: &mut Vec<u8>,
    handle: NonZeroU64,
    expected_operations: u64,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_operation_count(expected_operations, false)?;
    buffer.resize(LOCAL_TRANSACTION_COMMIT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = CONTROL_OPCODE;
    buffer[4..12].copy_from_slice(&handle.get().to_le_bytes());
    buffer[12..20].copy_from_slice(&expected_operations.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed explicit local `COMMIT`.
///
/// # Errors
///
/// Returns an error for malformed identity, count, or reserved bytes.
pub fn decode_local_transaction_commit(
    payload: &[u8],
) -> Result<(NonZeroU64, u64), LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_COMMIT_SIZE)?;
    validate_control(payload)?;
    require_zero(&payload[2..4])?;
    require_zero(&payload[20..24])?;
    let expected_operations = read_u64(payload, 12)?;
    validate_operation_count(expected_operations, true)?;
    Ok((decode_handle(payload, 4)?, expected_operations))
}

/// Encodes one fixed all-engine `COMMITTED` receipt.
///
/// # Errors
///
/// Returns an error for unsupported durability or operation count.
pub fn encode_local_transaction_commit_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalTransactionCommitReceipt,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_durability(receipt.durability)?;
    validate_operation_count(u64::from(receipt.staged_operations), false)?;
    buffer.resize(LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = COMMITTED_RECEIPT_TAG;
    buffer[2] = receipt.durability as u8;
    buffer[4..12].copy_from_slice(&receipt.handle.get().to_le_bytes());
    buffer[12..28].copy_from_slice(&receipt.transaction_id.get().to_le_bytes());
    buffer[28..36].copy_from_slice(&receipt.commit_csn.get().to_le_bytes());
    buffer[36..40].copy_from_slice(&receipt.staged_operations.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed all-engine `COMMITTED` receipt.
///
/// # Errors
///
/// Returns an error for malformed durability, identities, or operation count.
pub fn decode_local_transaction_commit_receipt(
    payload: &[u8],
) -> Result<LocalTransactionCommitReceipt, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE)?;
    validate_receipt(payload, COMMITTED_RECEIPT_TAG)?;
    require_zero(&payload[3..4])?;
    let staged_operations = read_u32(payload, 36)?;
    validate_operation_count(u64::from(staged_operations), false)?;
    Ok(LocalTransactionCommitReceipt {
        durability: decode_durability(payload[2])?,
        handle: decode_handle(payload, 4)?,
        transaction_id: TransactionId::new(read_u128(payload, 12)?)
            .map_err(|_| LocalTransactionCodecError::InvalidIdentity)?,
        commit_csn: Csn::new(read_u64(payload, 28)?)
            .map_err(|_| LocalTransactionCodecError::InvalidIdentity)?,
        staged_operations,
    })
}

/// Encodes one fixed explicit local `ROLLBACK`.
pub fn encode_local_transaction_rollback(buffer: &mut Vec<u8>, handle: NonZeroU64) -> &[u8] {
    buffer.resize(LOCAL_TRANSACTION_ROLLBACK_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = CONTROL_OPCODE;
    buffer[4..12].copy_from_slice(&handle.get().to_le_bytes());
    buffer
}

/// Decodes one fixed explicit local `ROLLBACK`.
///
/// # Errors
///
/// Returns an error for malformed identity or reserved bytes.
pub fn decode_local_transaction_rollback(
    payload: &[u8],
) -> Result<NonZeroU64, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_ROLLBACK_SIZE)?;
    validate_control(payload)?;
    require_zero(&payload[2..4])?;
    require_zero(&payload[12..16])?;
    decode_handle(payload, 4)
}

/// Encodes one fixed `ROLLED_BACK` receipt.
///
/// # Errors
///
/// Returns an error when the discarded count exceeds the local bound.
pub fn encode_local_transaction_rollback_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalTransactionRollbackReceipt,
) -> Result<&[u8], LocalTransactionCodecError> {
    validate_operation_count(receipt.discarded_operations, true)?;
    buffer.resize(LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = TRANSACTION_VERSION;
    buffer[1] = ROLLED_BACK_RECEIPT_TAG;
    buffer[4..12].copy_from_slice(&receipt.handle.get().to_le_bytes());
    buffer[12..20].copy_from_slice(&receipt.discarded_operations.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed `ROLLED_BACK` receipt.
///
/// # Errors
///
/// Returns an error for malformed identity, count, or reserved bytes.
pub fn decode_local_transaction_rollback_receipt(
    payload: &[u8],
) -> Result<LocalTransactionRollbackReceipt, LocalTransactionCodecError> {
    require_fixed(payload, LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE)?;
    validate_receipt(payload, ROLLED_BACK_RECEIPT_TAG)?;
    require_zero(&payload[2..4])?;
    require_zero(&payload[20..24])?;
    let discarded_operations = read_u64(payload, 12)?;
    validate_operation_count(discarded_operations, true)?;
    Ok(LocalTransactionRollbackReceipt {
        handle: decode_handle(payload, 4)?,
        discarded_operations,
    })
}

fn validate_control(payload: &[u8]) -> Result<(), LocalTransactionCodecError> {
    validate_version(payload)?;
    if payload[1] != CONTROL_OPCODE {
        return Err(LocalTransactionCodecError::UnknownOpcode(payload[1]));
    }
    Ok(())
}

fn validate_receipt(payload: &[u8], tag: u8) -> Result<(), LocalTransactionCodecError> {
    validate_version(payload)?;
    if payload[1] != tag {
        return Err(LocalTransactionCodecError::UnknownReceiptTag(payload[1]));
    }
    Ok(())
}

fn validate_version(payload: &[u8]) -> Result<(), LocalTransactionCodecError> {
    if payload[0] != TRANSACTION_VERSION {
        return Err(LocalTransactionCodecError::UnsupportedVersion(payload[0]));
    }
    Ok(())
}

fn validate_durability(durability: DurabilityClass) -> Result<(), LocalTransactionCodecError> {
    match durability {
        DurabilityClass::Memory | DurabilityClass::Strict => Ok(()),
        DurabilityClass::Group => Err(LocalTransactionCodecError::UnsupportedDurability(
            durability as u8,
        )),
    }
}

fn decode_durability(value: u8) -> Result<DurabilityClass, LocalTransactionCodecError> {
    match value {
        value if value == DurabilityClass::Memory as u8 => Ok(DurabilityClass::Memory),
        value if value == DurabilityClass::Strict as u8 => Ok(DurabilityClass::Strict),
        value if value == DurabilityClass::Group as u8 => {
            Err(LocalTransactionCodecError::UnsupportedDurability(value))
        }
        value => Err(LocalTransactionCodecError::UnknownDurability(value)),
    }
}

fn validate_operation_count(
    count: u64,
    allow_zero: bool,
) -> Result<(), LocalTransactionCodecError> {
    let maximum = u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS)
        .map_err(|_| LocalTransactionCodecError::InvalidOperationCount)?;
    if (!allow_zero && count == 0) || count > maximum {
        return Err(LocalTransactionCodecError::InvalidOperationCount);
    }
    Ok(())
}

fn decode_handle(payload: &[u8], offset: usize) -> Result<NonZeroU64, LocalTransactionCodecError> {
    NonZeroU64::new(read_u64(payload, offset)?).ok_or(LocalTransactionCodecError::InvalidIdentity)
}

fn require_fixed(payload: &[u8], expected: usize) -> Result<(), LocalTransactionCodecError> {
    if payload.len() < expected {
        return Err(LocalTransactionCodecError::Truncated);
    }
    if payload.len() != expected {
        return Err(LocalTransactionCodecError::LengthMismatch);
    }
    Ok(())
}

fn require_zero(bytes: &[u8]) -> Result<(), LocalTransactionCodecError> {
    if bytes.iter().any(|byte| *byte != 0) {
        return Err(LocalTransactionCodecError::ReservedBytes);
    }
    Ok(())
}

fn read_u32(payload: &[u8], offset: usize) -> Result<u32, LocalTransactionCodecError> {
    let bytes = payload
        .get(offset..offset + 4)
        .ok_or(LocalTransactionCodecError::Truncated)?
        .try_into()
        .map_err(|_| LocalTransactionCodecError::Truncated)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(payload: &[u8], offset: usize) -> Result<u64, LocalTransactionCodecError> {
    let bytes = payload
        .get(offset..offset + 8)
        .ok_or(LocalTransactionCodecError::Truncated)?
        .try_into()
        .map_err(|_| LocalTransactionCodecError::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i64(payload: &[u8], offset: usize) -> Result<i64, LocalTransactionCodecError> {
    let bytes = payload
        .get(offset..offset + 8)
        .ok_or(LocalTransactionCodecError::Truncated)?
        .try_into()
        .map_err(|_| LocalTransactionCodecError::Truncated)?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_u128(payload: &[u8], offset: usize) -> Result<u128, LocalTransactionCodecError> {
    let bytes = payload
        .get(offset..offset + 16)
        .ok_or(LocalTransactionCodecError::Truncated)?
        .try_into()
        .map_err(|_| LocalTransactionCodecError::Truncated)?;
    Ok(u128::from_le_bytes(bytes))
}
