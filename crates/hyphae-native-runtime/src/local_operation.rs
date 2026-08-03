// SPDX-License-Identifier: Apache-2.0

use hyphae_native_btree::BTREE_MAX_KEY_SIZE;
use hyphae_native_types::{Csn, DurabilityClass, TransactionId};
use thiserror::Error;

/// Canonical header width shared by local point requests and scalar values.
pub const LOCAL_OPERATION_HEADER_SIZE: usize = 8;
/// Canonical header width for one local scalar `SET`.
pub const LOCAL_STRUCTURE_SET_HEADER_SIZE: usize = 20;
/// Canonical fixed width for one local TTL response.
pub const LOCAL_TTL_PAYLOAD_SIZE: usize = 12;
/// Canonical fixed width for one local mutation commit receipt.
pub const LOCAL_COMMIT_RECEIPT_SIZE: usize = 28;
/// Maximum binary scalar key after reserving its physical namespace byte.
pub const MAX_LOCAL_STRUCTURE_KEY_BYTES: usize = BTREE_MAX_KEY_SIZE - 1;

const OPERATION_VERSION: u8 = 1;
const STRUCTURE_GET_OPCODE: u8 = 1;
const STRUCTURE_SET_OPCODE: u8 = 2;
const STRUCTURE_TTL_OPCODE: u8 = 3;
const PERSISTENT_EXPIRY_MODE: u8 = 0;
const RELATIVE_EXPIRY_MODE: u8 = 1;
const MISSING_VALUE_TAG: u8 = 0;
const PRESENT_VALUE_TAG: u8 = 1;
const MISSING_TTL_TAG: u8 = 0;
const PERSISTENT_TTL_TAG: u8 = 1;
const REMAINING_TTL_TAG: u8 = 2;
const SET_COMMITTED_RECEIPT_TAG: u8 = 1;
const FAILURE_PAYLOAD_SIZE: usize = 4;

/// Stable request-local failure exposed by the engine-bearing local session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LocalFailureCode {
    /// The operation payload is malformed or unsupported.
    InvalidRequest = 1,
    /// The binary structure key exceeds the canonical physical limit.
    KeyTooLarge = 2,
    /// The native physical operation or commit failed.
    EngineFailure = 3,
    /// The complete response cannot fit the connection frame bound.
    ResponseTooLarge = 4,
    /// The frame kind is invalid in the current session state.
    UnexpectedFrame = 5,
    /// The requested durability is not implemented by this session.
    UnsupportedDurability = 6,
    /// Relative TTL addition overflowed absolute server time.
    ExpiryOverflow = 7,
    /// SQL text, binding, or physical access path is invalid.
    SqlInvalid = 8,
    /// SQL parameters do not satisfy the prepared plan.
    SqlParameters = 9,
    /// A prepared SQL plan's catalog version is stale.
    SqlCatalogChanged = 10,
    /// A bounded SQL session resource limit was reached.
    SqlResourceLimit = 11,
    /// A session-local prepared SQL plan does not exist.
    UnknownPrepared = 12,
}

impl TryFrom<u8> for LocalFailureCode {
    type Error = LocalOperationCodecError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidRequest),
            2 => Ok(Self::KeyTooLarge),
            3 => Ok(Self::EngineFailure),
            4 => Ok(Self::ResponseTooLarge),
            5 => Ok(Self::UnexpectedFrame),
            6 => Ok(Self::UnsupportedDurability),
            7 => Ok(Self::ExpiryOverflow),
            8 => Ok(Self::SqlInvalid),
            9 => Ok(Self::SqlParameters),
            10 => Ok(Self::SqlCatalogChanged),
            11 => Ok(Self::SqlResourceLimit),
            12 => Ok(Self::UnknownPrepared),
            _ => Err(LocalOperationCodecError::UnknownFailureCode(value)),
        }
    }
}

/// Borrowed scalar value preserving missing versus present-empty semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalValue<'payload> {
    /// No scalar value is visible at the server's logical time.
    Missing,
    /// One visible scalar value, including a valid empty value.
    Present(&'payload [u8]),
}

/// Borrowed canonical scalar `SET` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalStructureSetRequest<'payload> {
    /// Binary scalar key.
    pub key: &'payload [u8],
    /// Binary scalar value, including a valid empty value.
    pub value: &'payload [u8],
    /// Positive relative TTL sampled against server time, or persistent.
    pub relative_ttl_micros: Option<i64>,
    /// Explicit acknowledgement promise.
    pub durability: DurabilityClass,
}

/// Decoded local structure request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalStructureRequest<'payload> {
    /// Read one scalar value.
    Get(&'payload [u8]),
    /// Set one scalar value and optional relative TTL.
    Set(LocalStructureSetRequest<'payload>),
    /// Read one scalar TTL state.
    Ttl(&'payload [u8]),
}

/// Canonical local TTL response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalTtlValue {
    /// No scalar value is visible.
    Missing,
    /// The scalar value has no expiry.
    Persistent,
    /// The scalar value has this strictly positive remaining duration.
    RemainingMicros(i64),
}

/// Stable subset of one native structure commit receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalStructureCommitReceipt {
    /// Nonzero native transaction identity.
    pub transaction_id: TransactionId,
    /// Nonzero all-engine commit sequence number.
    pub commit_csn: Csn,
    /// Durability promise satisfied before acknowledgement.
    pub durability: DurabilityClass,
}

/// Canonical local operation payload failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalOperationCodecError {
    /// The payload is shorter than its fixed header.
    #[error("native local operation payload is truncated")]
    Truncated,
    /// The payload version is unsupported.
    #[error("native local operation payload version {0} is unsupported")]
    UnsupportedVersion(u8),
    /// Reserved payload bytes are nonzero.
    #[error("native local operation payload reserved bytes are nonzero")]
    ReservedBytes,
    /// The structure operation is unknown.
    #[error("native local structure opcode {0} is unknown")]
    UnknownStructureOpcode(u8),
    /// The binary structure key exceeds its canonical limit.
    #[error("native local structure key exceeds its canonical limit")]
    KeyTooLarge,
    /// The declared and physical payload lengths diverge.
    #[error("native local operation payload length mismatch")]
    LengthMismatch,
    /// The requested durability byte is unknown.
    #[error("native local durability {0} is unknown")]
    UnknownDurability(u8),
    /// The requested durability is known but unsupported by this session.
    #[error("native local durability {0} is unsupported")]
    UnsupportedDurability(u8),
    /// The scalar SET expiry mode is unknown.
    #[error("native local SET expiry mode {0} is unknown")]
    UnknownExpiryMode(u8),
    /// The relative TTL and expiry mode are noncanonical.
    #[error("native local SET relative TTL is noncanonical")]
    NoncanonicalRelativeTtl,
    /// The scalar value tag is unknown.
    #[error("native local value tag {0} is unknown")]
    UnknownValueTag(u8),
    /// A missing value declared bytes or carried trailing data.
    #[error("native local missing value is noncanonical")]
    NoncanonicalMissing,
    /// The TTL response tag is unknown.
    #[error("native local TTL tag {0} is unknown")]
    UnknownTtlTag(u8),
    /// The TTL tag and encoded duration are noncanonical.
    #[error("native local TTL response is noncanonical")]
    NoncanonicalTtl,
    /// The mutation receipt tag is unknown.
    #[error("native local receipt tag {0} is unknown")]
    UnknownReceiptTag(u8),
    /// A receipt transaction or CSN identity is zero.
    #[error("native local receipt identity is invalid")]
    InvalidIdentity,
    /// The complete encoded payload exceeds its configured frame bound.
    #[error("native local operation payload exceeds the configured frame bound")]
    PayloadTooLarge,
    /// The request-local failure code is unknown.
    #[error("native local failure code {0} is unknown")]
    UnknownFailureCode(u8),
}

/// Encodes one canonical binary `STRUCTURE GET` payload into a reusable buffer.
///
/// # Errors
///
/// Returns [`LocalOperationCodecError::KeyTooLarge`] when the supplied key
/// cannot fit the native scalar namespace.
pub fn encode_local_structure_get<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    key: &[u8],
) -> Result<&'buffer [u8], LocalOperationCodecError> {
    encode_point_request(buffer, STRUCTURE_GET_OPCODE, key)
}

/// Encodes one canonical binary `STRUCTURE TTL` payload into a reusable buffer.
///
/// # Errors
///
/// Returns [`LocalOperationCodecError::KeyTooLarge`] when the supplied key
/// cannot fit the native scalar namespace.
pub fn encode_local_structure_ttl<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    key: &[u8],
) -> Result<&'buffer [u8], LocalOperationCodecError> {
    encode_point_request(buffer, STRUCTURE_TTL_OPCODE, key)
}

fn encode_point_request<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    opcode: u8,
    key: &[u8],
) -> Result<&'buffer [u8], LocalOperationCodecError> {
    validate_key_length(key.len())?;
    let key_length = u32::try_from(key.len()).map_err(|_| LocalOperationCodecError::KeyTooLarge)?;
    let encoded_length = LOCAL_OPERATION_HEADER_SIZE
        .checked_add(key.len())
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    buffer.resize(encoded_length, 0);
    buffer[..LOCAL_OPERATION_HEADER_SIZE].fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = opcode;
    buffer[4..8].copy_from_slice(&key_length.to_le_bytes());
    buffer[LOCAL_OPERATION_HEADER_SIZE..].copy_from_slice(key);
    Ok(buffer)
}

/// Encodes one canonical scalar `STRUCTURE SET` payload.
///
/// # Errors
///
/// Returns a typed error for key, durability, TTL, length, or frame-bound
/// violations before growing the reusable buffer.
pub fn encode_local_structure_set<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    key: &[u8],
    value: &[u8],
    relative_ttl_micros: Option<i64>,
    durability: DurabilityClass,
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalOperationCodecError> {
    validate_key_length(key.len())?;
    let durability = encode_durability(durability)?;
    let (expiry_mode, relative_ttl_micros) = encode_expiry(relative_ttl_micros)?;
    let key_length = u32::try_from(key.len()).map_err(|_| LocalOperationCodecError::KeyTooLarge)?;
    let value_length =
        u32::try_from(value.len()).map_err(|_| LocalOperationCodecError::PayloadTooLarge)?;
    let encoded_length = LOCAL_STRUCTURE_SET_HEADER_SIZE
        .checked_add(key.len())
        .and_then(|length| length.checked_add(value.len()))
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    if encoded_length > maximum_payload {
        return Err(LocalOperationCodecError::PayloadTooLarge);
    }
    buffer.resize(encoded_length, 0);
    buffer[..LOCAL_STRUCTURE_SET_HEADER_SIZE].fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = STRUCTURE_SET_OPCODE;
    buffer[2] = durability;
    buffer[3] = expiry_mode;
    buffer[4..8].copy_from_slice(&key_length.to_le_bytes());
    buffer[8..12].copy_from_slice(&value_length.to_le_bytes());
    buffer[12..20].copy_from_slice(&relative_ttl_micros.to_le_bytes());
    let value_start = LOCAL_STRUCTURE_SET_HEADER_SIZE + key.len();
    buffer[LOCAL_STRUCTURE_SET_HEADER_SIZE..value_start].copy_from_slice(key);
    buffer[value_start..].copy_from_slice(value);
    Ok(buffer)
}

/// Decodes and validates one canonical binary `STRUCTURE GET` payload.
///
/// # Errors
///
/// Returns a typed error for truncation, version, reserved bytes, opcode,
/// key bound, or length divergence.
pub fn decode_local_structure_get(payload: &[u8]) -> Result<&[u8], LocalOperationCodecError> {
    decode_point_request(payload, STRUCTURE_GET_OPCODE)
}

/// Decodes any implemented canonical local structure request.
///
/// # Errors
///
/// Returns a typed error for every noncanonical header, identity, length,
/// durability, or TTL boundary.
pub fn decode_local_structure_request(
    payload: &[u8],
) -> Result<LocalStructureRequest<'_>, LocalOperationCodecError> {
    if payload.len() < 2 {
        return Err(LocalOperationCodecError::Truncated);
    }
    validate_version(payload)?;
    match payload[1] {
        STRUCTURE_GET_OPCODE => {
            decode_point_request(payload, STRUCTURE_GET_OPCODE).map(LocalStructureRequest::Get)
        }
        STRUCTURE_SET_OPCODE => decode_structure_set(payload).map(LocalStructureRequest::Set),
        STRUCTURE_TTL_OPCODE => {
            decode_point_request(payload, STRUCTURE_TTL_OPCODE).map(LocalStructureRequest::Ttl)
        }
        opcode => Err(LocalOperationCodecError::UnknownStructureOpcode(opcode)),
    }
}

fn decode_point_request(
    payload: &[u8],
    expected_opcode: u8,
) -> Result<&[u8], LocalOperationCodecError> {
    validate_operation_header(payload)?;
    if payload[1] != expected_opcode {
        return Err(LocalOperationCodecError::UnknownStructureOpcode(payload[1]));
    }
    let key_length = read_u32(payload, 4)?;
    validate_key_length(key_length)?;
    require_payload_length(payload, LOCAL_OPERATION_HEADER_SIZE, key_length)?;
    Ok(&payload[LOCAL_OPERATION_HEADER_SIZE..])
}

fn decode_structure_set(
    payload: &[u8],
) -> Result<LocalStructureSetRequest<'_>, LocalOperationCodecError> {
    if payload.len() < LOCAL_STRUCTURE_SET_HEADER_SIZE {
        return Err(LocalOperationCodecError::Truncated);
    }
    validate_version(payload)?;
    if payload[1] != STRUCTURE_SET_OPCODE {
        return Err(LocalOperationCodecError::UnknownStructureOpcode(payload[1]));
    }
    let durability = decode_durability(payload[2])?;
    let relative_ttl_micros = decode_expiry(payload[3], read_i64(payload, 12))?;
    let key_length = read_u32(payload, 4)?;
    validate_key_length(key_length)?;
    let value_length = read_u32(payload, 8)?;
    let body_length = key_length
        .checked_add(value_length)
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    require_payload_length(payload, LOCAL_STRUCTURE_SET_HEADER_SIZE, body_length)?;
    let value_start = LOCAL_STRUCTURE_SET_HEADER_SIZE + key_length;
    Ok(LocalStructureSetRequest {
        key: &payload[LOCAL_STRUCTURE_SET_HEADER_SIZE..value_start],
        value: &payload[value_start..],
        relative_ttl_micros,
        durability,
    })
}

/// Encodes a canonical missing or present scalar value into a reusable buffer.
///
/// # Errors
///
/// Returns [`LocalOperationCodecError::PayloadTooLarge`] when the fixed header
/// plus complete value exceeds `maximum_payload`.
pub fn encode_local_value<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    value: Option<&[u8]>,
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalOperationCodecError> {
    let value_length = value.map_or(0, <[u8]>::len);
    let encoded_length = LOCAL_OPERATION_HEADER_SIZE
        .checked_add(value_length)
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    if encoded_length > maximum_payload {
        return Err(LocalOperationCodecError::PayloadTooLarge);
    }
    let value_length =
        u32::try_from(value_length).map_err(|_| LocalOperationCodecError::PayloadTooLarge)?;
    buffer.resize(encoded_length, 0);
    buffer[..LOCAL_OPERATION_HEADER_SIZE].fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = u8::from(value.is_some());
    buffer[4..8].copy_from_slice(&value_length.to_le_bytes());
    if let Some(value) = value {
        buffer[LOCAL_OPERATION_HEADER_SIZE..].copy_from_slice(value);
    }
    Ok(buffer)
}

/// Decodes a canonical missing or present scalar value.
///
/// # Errors
///
/// Returns a typed error for truncation, version, reserved bytes, tag,
/// noncanonical missing values, or length divergence.
pub fn decode_local_value(payload: &[u8]) -> Result<LocalValue<'_>, LocalOperationCodecError> {
    validate_operation_header(payload)?;
    let value_length = read_u32(payload, 4)?;
    require_payload_length(payload, LOCAL_OPERATION_HEADER_SIZE, value_length)?;
    match payload[1] {
        MISSING_VALUE_TAG if value_length == 0 => Ok(LocalValue::Missing),
        MISSING_VALUE_TAG => Err(LocalOperationCodecError::NoncanonicalMissing),
        PRESENT_VALUE_TAG => Ok(LocalValue::Present(&payload[LOCAL_OPERATION_HEADER_SIZE..])),
        tag => Err(LocalOperationCodecError::UnknownValueTag(tag)),
    }
}

/// Encodes one canonical TTL response.
///
/// # Errors
///
/// Returns [`LocalOperationCodecError::NoncanonicalTtl`] for a nonpositive
/// remaining duration.
pub fn encode_local_ttl(
    buffer: &mut Vec<u8>,
    ttl: LocalTtlValue,
) -> Result<&[u8], LocalOperationCodecError> {
    let (tag, remaining_micros) = match ttl {
        LocalTtlValue::Missing => (MISSING_TTL_TAG, 0),
        LocalTtlValue::Persistent => (PERSISTENT_TTL_TAG, 0),
        LocalTtlValue::RemainingMicros(value) if value > 0 => (REMAINING_TTL_TAG, value),
        LocalTtlValue::RemainingMicros(_) => {
            return Err(LocalOperationCodecError::NoncanonicalTtl);
        }
    };
    buffer.resize(LOCAL_TTL_PAYLOAD_SIZE, 0);
    buffer.fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = tag;
    buffer[4..12].copy_from_slice(&remaining_micros.to_le_bytes());
    Ok(buffer)
}

/// Decodes one canonical fixed-width TTL response.
///
/// # Errors
///
/// Returns a typed error for size, version, reserved bytes, tag, or duration
/// canonicality.
pub fn decode_local_ttl(payload: &[u8]) -> Result<LocalTtlValue, LocalOperationCodecError> {
    require_fixed_payload(payload, LOCAL_TTL_PAYLOAD_SIZE)?;
    validate_version_and_reserved(payload)?;
    let remaining_micros = read_i64(payload, 4);
    match payload[1] {
        MISSING_TTL_TAG if remaining_micros == 0 => Ok(LocalTtlValue::Missing),
        PERSISTENT_TTL_TAG if remaining_micros == 0 => Ok(LocalTtlValue::Persistent),
        REMAINING_TTL_TAG if remaining_micros > 0 => {
            Ok(LocalTtlValue::RemainingMicros(remaining_micros))
        }
        MISSING_TTL_TAG | PERSISTENT_TTL_TAG | REMAINING_TTL_TAG => {
            Err(LocalOperationCodecError::NoncanonicalTtl)
        }
        tag => Err(LocalOperationCodecError::UnknownTtlTag(tag)),
    }
}

/// Encodes one canonical fixed-width structure commit receipt.
///
/// # Errors
///
/// Returns a typed error when the receipt claims group durability, which this
/// serialized session cannot acknowledge.
pub fn encode_local_structure_commit_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalStructureCommitReceipt,
) -> Result<&[u8], LocalOperationCodecError> {
    let durability = encode_durability(receipt.durability)?;
    buffer.resize(LOCAL_COMMIT_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = SET_COMMITTED_RECEIPT_TAG;
    buffer[2] = durability;
    buffer[4..20].copy_from_slice(&receipt.transaction_id.get().to_le_bytes());
    buffer[20..28].copy_from_slice(&receipt.commit_csn.get().to_le_bytes());
    Ok(buffer)
}

/// Decodes one canonical fixed-width structure commit receipt.
///
/// # Errors
///
/// Returns a typed error for size, version, receipt tag, durability, reserved
/// byte, or zero identities.
pub fn decode_local_structure_commit_receipt(
    payload: &[u8],
) -> Result<LocalStructureCommitReceipt, LocalOperationCodecError> {
    require_fixed_payload(payload, LOCAL_COMMIT_RECEIPT_SIZE)?;
    validate_version(payload)?;
    if payload[1] != SET_COMMITTED_RECEIPT_TAG {
        return Err(LocalOperationCodecError::UnknownReceiptTag(payload[1]));
    }
    let durability = decode_durability(payload[2])?;
    if payload[3] != 0 {
        return Err(LocalOperationCodecError::ReservedBytes);
    }
    let transaction_id = TransactionId::new(read_u128(payload, 4))
        .map_err(|_| LocalOperationCodecError::InvalidIdentity)?;
    let commit_csn =
        Csn::new(read_u64(payload, 20)).map_err(|_| LocalOperationCodecError::InvalidIdentity)?;
    Ok(LocalStructureCommitReceipt {
        transaction_id,
        commit_csn,
        durability,
    })
}

/// Encodes one fixed canonical request-local failure payload.
pub fn encode_local_failure(buffer: &mut Vec<u8>, code: LocalFailureCode) -> &[u8] {
    buffer.resize(FAILURE_PAYLOAD_SIZE, 0);
    buffer.fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = code as u8;
    buffer
}

/// Decodes one fixed canonical request-local failure payload.
///
/// # Errors
///
/// Returns a typed error for truncation, trailing bytes, version, reserved
/// bytes, or an unknown failure code.
pub fn decode_local_failure(payload: &[u8]) -> Result<LocalFailureCode, LocalOperationCodecError> {
    require_fixed_payload(payload, FAILURE_PAYLOAD_SIZE)?;
    validate_version_and_reserved(payload)?;
    LocalFailureCode::try_from(payload[1])
}

fn validate_operation_header(payload: &[u8]) -> Result<(), LocalOperationCodecError> {
    if payload.len() < LOCAL_OPERATION_HEADER_SIZE {
        return Err(LocalOperationCodecError::Truncated);
    }
    validate_version_and_reserved(payload)
}

fn validate_version_and_reserved(payload: &[u8]) -> Result<(), LocalOperationCodecError> {
    validate_version(payload)?;
    if payload[2..4] != [0, 0] {
        return Err(LocalOperationCodecError::ReservedBytes);
    }
    Ok(())
}

fn validate_version(payload: &[u8]) -> Result<(), LocalOperationCodecError> {
    if payload[0] != OPERATION_VERSION {
        return Err(LocalOperationCodecError::UnsupportedVersion(payload[0]));
    }
    Ok(())
}

fn validate_key_length(key_length: usize) -> Result<(), LocalOperationCodecError> {
    if key_length > MAX_LOCAL_STRUCTURE_KEY_BYTES {
        return Err(LocalOperationCodecError::KeyTooLarge);
    }
    Ok(())
}

fn encode_durability(durability: DurabilityClass) -> Result<u8, LocalOperationCodecError> {
    match durability {
        DurabilityClass::Strict | DurabilityClass::Memory => Ok(durability as u8),
        DurabilityClass::Group => Err(LocalOperationCodecError::UnsupportedDurability(
            durability as u8,
        )),
    }
}

fn decode_durability(value: u8) -> Result<DurabilityClass, LocalOperationCodecError> {
    match value {
        value if value == DurabilityClass::Strict as u8 => Ok(DurabilityClass::Strict),
        value if value == DurabilityClass::Group as u8 => {
            Err(LocalOperationCodecError::UnsupportedDurability(value))
        }
        value if value == DurabilityClass::Memory as u8 => Ok(DurabilityClass::Memory),
        value => Err(LocalOperationCodecError::UnknownDurability(value)),
    }
}

fn encode_expiry(relative_ttl_micros: Option<i64>) -> Result<(u8, i64), LocalOperationCodecError> {
    match relative_ttl_micros {
        None => Ok((PERSISTENT_EXPIRY_MODE, 0)),
        Some(value) if value > 0 => Ok((RELATIVE_EXPIRY_MODE, value)),
        Some(_) => Err(LocalOperationCodecError::NoncanonicalRelativeTtl),
    }
}

fn decode_expiry(
    expiry_mode: u8,
    relative_ttl_micros: i64,
) -> Result<Option<i64>, LocalOperationCodecError> {
    match (expiry_mode, relative_ttl_micros) {
        (PERSISTENT_EXPIRY_MODE, 0) => Ok(None),
        (RELATIVE_EXPIRY_MODE, value) if value > 0 => Ok(Some(value)),
        (PERSISTENT_EXPIRY_MODE | RELATIVE_EXPIRY_MODE, _) => {
            Err(LocalOperationCodecError::NoncanonicalRelativeTtl)
        }
        (mode, _) => Err(LocalOperationCodecError::UnknownExpiryMode(mode)),
    }
}

fn read_u32(payload: &[u8], offset: usize) -> Result<usize, LocalOperationCodecError> {
    let mut encoded = [0_u8; 4];
    encoded.copy_from_slice(&payload[offset..offset + 4]);
    usize::try_from(u32::from_le_bytes(encoded))
        .map_err(|_| LocalOperationCodecError::PayloadTooLarge)
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    let mut encoded = [0_u8; 8];
    encoded.copy_from_slice(&payload[offset..offset + 8]);
    u64::from_le_bytes(encoded)
}

fn read_u128(payload: &[u8], offset: usize) -> u128 {
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&payload[offset..offset + 16]);
    u128::from_le_bytes(encoded)
}

fn read_i64(payload: &[u8], offset: usize) -> i64 {
    let mut encoded = [0_u8; 8];
    encoded.copy_from_slice(&payload[offset..offset + 8]);
    i64::from_le_bytes(encoded)
}

fn require_payload_length(
    payload: &[u8],
    header_length: usize,
    body_length: usize,
) -> Result<(), LocalOperationCodecError> {
    let encoded_length = header_length
        .checked_add(body_length)
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    if payload.len() != encoded_length {
        return Err(LocalOperationCodecError::LengthMismatch);
    }
    Ok(())
}

fn require_fixed_payload(payload: &[u8], expected: usize) -> Result<(), LocalOperationCodecError> {
    if payload.len() < expected {
        return Err(LocalOperationCodecError::Truncated);
    }
    if payload.len() != expected {
        return Err(LocalOperationCodecError::LengthMismatch);
    }
    Ok(())
}
