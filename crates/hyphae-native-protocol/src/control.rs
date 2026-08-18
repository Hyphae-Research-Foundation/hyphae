// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

/// Canonical `WINDOW_UPDATE` payload bytes.
pub const WINDOW_UPDATE_PAYLOAD_SIZE: usize = 16;
/// Canonical `CANCEL` payload bytes.
pub const CANCEL_PAYLOAD_SIZE: usize = 16;
/// Canonical request deadline payload bytes.
pub const DEADLINE_PAYLOAD_SIZE: usize = 16;
/// Canonical successful `END` payload bytes.
pub const END_PAYLOAD_SIZE: usize = 56;

const WINDOW_MAGIC: &[u8; 8] = b"HYPWIN01";
const CANCEL_MAGIC: &[u8; 8] = b"HYPCAN01";
const DEADLINE_MAGIC: &[u8; 8] = b"HYPDDL01";
const END_MAGIC: &[u8; 8] = b"HYPEND01";

/// Flow-control, cancellation, deadline, or completion validation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ControlError {
    /// Payload has the wrong exact shape or magic.
    #[error("native protocol control payload is malformed")]
    Malformed,
    /// A zero increment or invalid deadline was supplied.
    #[error("native protocol control value is invalid")]
    InvalidValue,
    /// A flow-control update exceeds the configured window maximum.
    #[error("native protocol flow-control window overflow")]
    WindowOverflow,
    /// A producer attempted to exceed its byte credit.
    #[error("native protocol flow-control window is exhausted")]
    WindowExhausted,
    /// A provisional stream ended without a valid completion frame.
    #[error("native protocol stream is missing successful completion")]
    MissingCompletion,
    /// Completion length or digest differs from provisional data.
    #[error("native protocol stream completion does not match its data")]
    CompletionMismatch,
}

/// Bounded byte-credit state for one protocol stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowWindow {
    available: u64,
    maximum: u64,
}

impl FlowWindow {
    /// Creates a nonzero bounded window.
    pub const fn new(initial: u64, maximum: u64) -> Result<Self, ControlError> {
        if initial == 0 || maximum == 0 || initial > maximum {
            Err(ControlError::InvalidValue)
        } else {
            Ok(Self {
                available: initial,
                maximum,
            })
        }
    }

    /// Returns currently available byte credit.
    pub const fn available(self) -> u64 {
        self.available
    }

    /// Debits payload bytes before they are sent.
    pub fn consume(&mut self, bytes: usize) -> Result<(), ControlError> {
        let bytes = u64::try_from(bytes).map_err(|_| ControlError::WindowExhausted)?;
        if bytes > self.available {
            return Err(ControlError::WindowExhausted);
        }
        self.available -= bytes;
        Ok(())
    }

    /// Applies one positive peer credit update.
    pub fn update(&mut self, increment: u64) -> Result<(), ControlError> {
        if increment == 0 {
            return Err(ControlError::InvalidValue);
        }
        let updated = self
            .available
            .checked_add(increment)
            .ok_or(ControlError::WindowOverflow)?;
        if updated > self.maximum {
            return Err(ControlError::WindowOverflow);
        }
        self.available = updated;
        Ok(())
    }
}

/// Encodes a positive stream-window increment.
pub fn encode_window_update(
    increment: u64,
) -> Result<[u8; WINDOW_UPDATE_PAYLOAD_SIZE], ControlError> {
    if increment == 0 {
        return Err(ControlError::InvalidValue);
    }
    let mut encoded = [0; WINDOW_UPDATE_PAYLOAD_SIZE];
    encoded[..8].copy_from_slice(WINDOW_MAGIC);
    encoded[8..].copy_from_slice(&increment.to_le_bytes());
    Ok(encoded)
}

/// Decodes a positive stream-window increment.
pub fn decode_window_update(encoded: &[u8]) -> Result<u64, ControlError> {
    if encoded.len() != WINDOW_UPDATE_PAYLOAD_SIZE || &encoded[..8] != WINDOW_MAGIC {
        return Err(ControlError::Malformed);
    }
    let increment = read_u64(&encoded[8..]);
    if increment == 0 {
        Err(ControlError::InvalidValue)
    } else {
        Ok(increment)
    }
}

/// Encodes an idempotent cancellation reason code.
pub fn encode_cancel(reason: u32) -> [u8; CANCEL_PAYLOAD_SIZE] {
    let mut encoded = [0; CANCEL_PAYLOAD_SIZE];
    encoded[..8].copy_from_slice(CANCEL_MAGIC);
    encoded[8..12].copy_from_slice(&reason.to_le_bytes());
    encoded
}

/// Decodes a cancellation reason code.
pub fn decode_cancel(encoded: &[u8]) -> Result<u32, ControlError> {
    if encoded.len() != CANCEL_PAYLOAD_SIZE
        || &encoded[..8] != CANCEL_MAGIC
        || encoded[12..] != [0; 4]
    {
        return Err(ControlError::Malformed);
    }
    Ok(u32::from_le_bytes(
        encoded[8..12]
            .try_into()
            .map_err(|_| ControlError::Malformed)?,
    ))
}

/// Encodes one absolute Unix-time request deadline in microseconds.
pub fn encode_deadline(deadline_micros: i64) -> Result<[u8; DEADLINE_PAYLOAD_SIZE], ControlError> {
    if deadline_micros <= 0 {
        return Err(ControlError::InvalidValue);
    }
    let mut encoded = [0; DEADLINE_PAYLOAD_SIZE];
    encoded[..8].copy_from_slice(DEADLINE_MAGIC);
    encoded[8..].copy_from_slice(&deadline_micros.to_le_bytes());
    Ok(encoded)
}

/// Decodes one absolute Unix-time request deadline in microseconds.
pub fn decode_deadline(encoded: &[u8]) -> Result<i64, ControlError> {
    if encoded.len() != DEADLINE_PAYLOAD_SIZE || &encoded[..8] != DEADLINE_MAGIC {
        return Err(ControlError::Malformed);
    }
    let deadline = i64::from_le_bytes(
        encoded[8..]
            .try_into()
            .map_err(|_| ControlError::Malformed)?,
    );
    if deadline <= 0 {
        Err(ControlError::InvalidValue)
    } else {
        Ok(deadline)
    }
}

/// Canonical successful completion for provisional response data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamCompletion {
    /// Aggregate unframed response bytes.
    pub total_bytes: u64,
    /// BLAKE3 digest over response bytes in frame order.
    pub digest: [u8; 32],
}

impl StreamCompletion {
    /// Computes completion over one bounded response.
    pub fn for_data(data: &[u8]) -> Result<Self, ControlError> {
        Ok(Self {
            total_bytes: u64::try_from(data.len()).map_err(|_| ControlError::InvalidValue)?,
            digest: *blake3::hash(data).as_bytes(),
        })
    }
}

/// Encodes one successful stream completion.
pub fn encode_end(completion: StreamCompletion) -> [u8; END_PAYLOAD_SIZE] {
    let mut encoded = [0; END_PAYLOAD_SIZE];
    encoded[..8].copy_from_slice(END_MAGIC);
    encoded[8..12].copy_from_slice(&56_u32.to_le_bytes());
    encoded[12] = 1;
    encoded[16..24].copy_from_slice(&completion.total_bytes.to_le_bytes());
    encoded[24..].copy_from_slice(&completion.digest);
    encoded
}

/// Decodes one successful stream completion.
pub fn decode_end(encoded: &[u8]) -> Result<StreamCompletion, ControlError> {
    if encoded.len() != END_PAYLOAD_SIZE
        || &encoded[..8] != END_MAGIC
        || u32::from_le_bytes(
            encoded[8..12]
                .try_into()
                .map_err(|_| ControlError::Malformed)?,
        ) as usize
            != END_PAYLOAD_SIZE
        || encoded[12] != 1
        || encoded[13..16] != [0; 3]
    {
        return Err(ControlError::Malformed);
    }
    let mut digest = [0; 32];
    digest.copy_from_slice(&encoded[24..]);
    Ok(StreamCompletion {
        total_bytes: read_u64(&encoded[16..24]),
        digest,
    })
}

/// Client-side accumulator that never exposes provisional bytes as complete.
#[derive(Debug, Default)]
pub struct ProvisionalStream {
    data: Vec<u8>,
    completed: bool,
}

impl ProvisionalStream {
    /// Creates an empty provisional stream.
    pub const fn new() -> Self {
        Self {
            data: Vec::new(),
            completed: false,
        }
    }

    /// Appends one `DATA` payload before completion.
    pub fn push(&mut self, data: &[u8], maximum_bytes: usize) -> Result<(), ControlError> {
        if self.completed {
            return Err(ControlError::CompletionMismatch);
        }
        let length = self
            .data
            .len()
            .checked_add(data.len())
            .ok_or(ControlError::InvalidValue)?;
        if length > maximum_bytes {
            return Err(ControlError::InvalidValue);
        }
        self.data.extend_from_slice(data);
        Ok(())
    }

    /// Validates `END` and returns the now-definitive response bytes.
    pub fn complete(mut self, completion: StreamCompletion) -> Result<Vec<u8>, ControlError> {
        let expected = StreamCompletion::for_data(&self.data)?;
        if completion != expected {
            return Err(ControlError::CompletionMismatch);
        }
        self.completed = true;
        Ok(self.data)
    }

    /// Rejects EOF or disconnect before successful `END`.
    pub fn reject_incomplete(self) -> Result<Vec<u8>, ControlError> {
        drop(self);
        Err(ControlError::MissingCompletion)
    }
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))
}
