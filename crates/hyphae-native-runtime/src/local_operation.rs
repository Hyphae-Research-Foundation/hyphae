// SPDX-License-Identifier: Apache-2.0

use hyphae_native_btree::BTREE_MAX_KEY_SIZE;
use thiserror::Error;

/// Canonical header width shared by local structure requests and values.
pub const LOCAL_OPERATION_HEADER_SIZE: usize = 8;
/// Maximum binary scalar key after reserving its physical namespace byte.
pub const MAX_LOCAL_STRUCTURE_KEY_BYTES: usize = BTREE_MAX_KEY_SIZE - 1;

const OPERATION_VERSION: u8 = 1;
const STRUCTURE_GET_OPCODE: u8 = 1;
const MISSING_VALUE_TAG: u8 = 0;
const PRESENT_VALUE_TAG: u8 = 1;
const FAILURE_PAYLOAD_SIZE: usize = 4;

/// Stable request-local failure exposed by the first engine-bearing session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LocalFailureCode {
    /// The operation payload is malformed or unsupported.
    InvalidRequest = 1,
    /// The binary structure key exceeds the canonical physical limit.
    KeyTooLarge = 2,
    /// The native physical read failed.
    EngineFailure = 3,
    /// The complete value cannot fit the connection frame bound.
    ResponseTooLarge = 4,
    /// The frame kind is invalid in the current session state.
    UnexpectedFrame = 5,
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
    /// The scalar value tag is unknown.
    #[error("native local value tag {0} is unknown")]
    UnknownValueTag(u8),
    /// A missing value declared bytes or carried trailing data.
    #[error("native local missing value is noncanonical")]
    NoncanonicalMissing,
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
    if key.len() > MAX_LOCAL_STRUCTURE_KEY_BYTES {
        return Err(LocalOperationCodecError::KeyTooLarge);
    }
    let key_length = u32::try_from(key.len()).map_err(|_| LocalOperationCodecError::KeyTooLarge)?;
    buffer.resize(LOCAL_OPERATION_HEADER_SIZE + key.len(), 0);
    buffer[..LOCAL_OPERATION_HEADER_SIZE].fill(0);
    buffer[0] = OPERATION_VERSION;
    buffer[1] = STRUCTURE_GET_OPCODE;
    buffer[4..8].copy_from_slice(&key_length.to_le_bytes());
    buffer[LOCAL_OPERATION_HEADER_SIZE..].copy_from_slice(key);
    Ok(buffer)
}

/// Decodes and validates one canonical binary `STRUCTURE GET` payload.
///
/// # Errors
///
/// Returns a typed error for truncation, version, reserved bytes, opcode,
/// key bound, or length divergence.
pub fn decode_local_structure_get(payload: &[u8]) -> Result<&[u8], LocalOperationCodecError> {
    validate_operation_header(payload)?;
    if payload[1] != STRUCTURE_GET_OPCODE {
        return Err(LocalOperationCodecError::UnknownStructureOpcode(payload[1]));
    }
    let key_length = read_payload_length(payload)?;
    if key_length > MAX_LOCAL_STRUCTURE_KEY_BYTES {
        return Err(LocalOperationCodecError::KeyTooLarge);
    }
    require_payload_length(payload, key_length)?;
    Ok(&payload[LOCAL_OPERATION_HEADER_SIZE..])
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
    let value_length = read_payload_length(payload)?;
    require_payload_length(payload, value_length)?;
    match payload[1] {
        MISSING_VALUE_TAG if value_length == 0 => Ok(LocalValue::Missing),
        MISSING_VALUE_TAG => Err(LocalOperationCodecError::NoncanonicalMissing),
        PRESENT_VALUE_TAG => Ok(LocalValue::Present(&payload[LOCAL_OPERATION_HEADER_SIZE..])),
        tag => Err(LocalOperationCodecError::UnknownValueTag(tag)),
    }
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
    if payload.len() < FAILURE_PAYLOAD_SIZE {
        return Err(LocalOperationCodecError::Truncated);
    }
    if payload.len() != FAILURE_PAYLOAD_SIZE {
        return Err(LocalOperationCodecError::LengthMismatch);
    }
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
    if payload[0] != OPERATION_VERSION {
        return Err(LocalOperationCodecError::UnsupportedVersion(payload[0]));
    }
    if payload[2..4] != [0, 0] {
        return Err(LocalOperationCodecError::ReservedBytes);
    }
    Ok(())
}

fn read_payload_length(payload: &[u8]) -> Result<usize, LocalOperationCodecError> {
    let mut length = [0_u8; 4];
    length.copy_from_slice(&payload[4..8]);
    usize::try_from(u32::from_le_bytes(length))
        .map_err(|_| LocalOperationCodecError::PayloadTooLarge)
}

fn require_payload_length(
    payload: &[u8],
    body_length: usize,
) -> Result<(), LocalOperationCodecError> {
    let encoded_length = LOCAL_OPERATION_HEADER_SIZE
        .checked_add(body_length)
        .ok_or(LocalOperationCodecError::PayloadTooLarge)?;
    if payload.len() != encoded_length {
        return Err(LocalOperationCodecError::LengthMismatch);
    }
    Ok(())
}
