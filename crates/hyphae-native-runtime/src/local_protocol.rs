// SPDX-License-Identifier: AGPL-3.0-only

use std::io::{self, Read, Write};

use thiserror::Error;

/// Native local frame header size.
pub const LOCAL_FRAME_HEADER_SIZE: usize = 32;
/// Default bounded local frame payload.
pub const DEFAULT_MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"HYPHLCL1";
const MAJOR_VERSION: u16 = 1;
const CHECKSUM_START: usize = 28;
const CHECKSUM_END: usize = 32;

/// Native local protocol message family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum FrameKind {
    /// Client version/capability negotiation.
    Hello = 1,
    /// Server version/capability selection.
    Welcome = 2,
    /// Liveness request.
    Ping = 3,
    /// Prepare one SQL statement.
    Prepare = 4,
    /// Execute one prepared operation.
    Execute = 5,
    /// Begin one all-engine transaction.
    Begin = 6,
    /// Commit one all-engine transaction.
    Commit = 7,
    /// Roll back one transaction.
    Rollback = 8,
    /// Native structure point operation.
    Structure = 9,
    /// Native search operation.
    Search = 10,
    /// Typed scalar response.
    Value = 11,
    /// Transaction receipt.
    Receipt = 12,
    /// Stable native error.
    Failure = 13,
    /// Cooperative cancellation.
    Cancel = 14,
    /// Clean connection close.
    Close = 15,
    /// Release one prepared plan.
    Deallocate = 16,
    /// Explain one prepared or textual operation.
    Explain = 17,
    /// Create or address one transaction savepoint.
    Savepoint = 18,
    /// Bounded stream data fragment.
    Data = 19,
    /// End one bounded result or data stream.
    End = 20,
    /// Increase one stream's available byte window.
    WindowUpdate = 21,
    /// Typed row-schema result.
    RowSchema = 22,
    /// Bounded typed row batch.
    RowBatch = 23,
    /// Absolute request deadline control.
    Deadline = 24,
}

impl TryFrom<u8> for FrameKind {
    type Error = LocalProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Welcome),
            3 => Ok(Self::Ping),
            4 => Ok(Self::Prepare),
            5 => Ok(Self::Execute),
            6 => Ok(Self::Begin),
            7 => Ok(Self::Commit),
            8 => Ok(Self::Rollback),
            9 => Ok(Self::Structure),
            10 => Ok(Self::Search),
            11 => Ok(Self::Value),
            12 => Ok(Self::Receipt),
            13 => Ok(Self::Failure),
            14 => Ok(Self::Cancel),
            15 => Ok(Self::Close),
            16 => Ok(Self::Deallocate),
            17 => Ok(Self::Explain),
            18 => Ok(Self::Savepoint),
            19 => Ok(Self::Data),
            20 => Ok(Self::End),
            21 => Ok(Self::WindowUpdate),
            22 => Ok(Self::RowSchema),
            23 => Ok(Self::RowBatch),
            24 => Ok(Self::Deadline),
            _ => Err(LocalProtocolError::UnknownKind(value)),
        }
    }
}

/// Native local frame validation failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalProtocolError {
    /// Frame is shorter than its canonical header.
    #[error("native local frame is truncated")]
    Truncated,
    /// Magic or major version is unsupported.
    #[error("native local frame has invalid magic or major version")]
    InvalidPreamble,
    /// Frame flags reserved by v1 are nonzero.
    #[error("native local frame reserved flags are nonzero")]
    ReservedFlags,
    /// Frame kind is unknown.
    #[error("native local frame kind {0} is unknown")]
    UnknownKind(u8),
    /// Declared payload is too large.
    #[error("native local frame payload exceeds the configured maximum")]
    PayloadTooLarge,
    /// Declared and physical frame lengths differ.
    #[error("native local frame length mismatch")]
    LengthMismatch,
    /// Frame CRC32C failed.
    #[error("native local frame CRC32C mismatch")]
    ChecksumMismatch,
}

/// Native local framed-I/O failure.
#[derive(Debug, Error)]
pub enum LocalTransportError {
    /// Configured payload bound exceeds the protocol-wide maximum.
    #[error("native local transport payload bound exceeds the protocol maximum")]
    MaximumPayloadTooLarge,
    /// The byte stream ended within one frame.
    #[error("native local transport frame is truncated")]
    Truncated,
    /// A complete frame violates the native local protocol.
    #[error(transparent)]
    Protocol(#[from] LocalProtocolError),
    /// A local byte-stream operation failed.
    #[error("native local transport I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The requested local endpoint path already exists.
    #[error("native local transport endpoint already exists")]
    EndpointExists,
    /// The bound endpoint path no longer names the socket created here.
    #[error("native local transport endpoint identity changed")]
    EndpointReplaced,
}

/// Borrowed, checksum-verified native local frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedFrame<'frame> {
    /// Message family.
    pub kind: FrameKind,
    /// Multiplexed stream identity.
    pub stream_id: u32,
    /// Request identity unique while active.
    pub request_id: u64,
    /// Canonical borrowed payload.
    pub payload: &'frame [u8],
}

/// Reusable bounded buffers for one ordered local byte stream.
#[derive(Debug)]
pub struct LocalFrameIo {
    maximum_payload: usize,
    receive_buffer: Vec<u8>,
    send_buffer: Vec<u8>,
}

impl LocalFrameIo {
    /// Creates one framed-I/O state with a strict per-frame payload bound.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested bound exceeds the protocol-wide
    /// maximum.
    pub fn new(maximum_payload: usize) -> Result<Self, LocalTransportError> {
        if maximum_payload > DEFAULT_MAX_FRAME_PAYLOAD {
            return Err(LocalTransportError::MaximumPayloadTooLarge);
        }
        Ok(Self {
            maximum_payload,
            receive_buffer: Vec::with_capacity(LOCAL_FRAME_HEADER_SIZE),
            send_buffer: Vec::with_capacity(LOCAL_FRAME_HEADER_SIZE),
        })
    }

    /// Returns the strict per-frame payload bound for this byte stream.
    pub const fn maximum_payload(&self) -> usize {
        self.maximum_payload
    }

    /// Reads and validates one complete frame.
    ///
    /// Clean EOF is returned only when no header byte was read. The returned
    /// payload borrows this I/O state's reusable receive buffer.
    ///
    /// # Errors
    ///
    /// Returns an error for partial frames, configured-bound violations,
    /// protocol failures, or byte-stream I/O failures.
    pub fn receive_from<R: Read>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<DecodedFrame<'_>>, LocalTransportError> {
        self.receive_buffer.resize(LOCAL_FRAME_HEADER_SIZE, 0);
        if !read_header(reader, &mut self.receive_buffer)? {
            return Ok(None);
        }
        let payload_length = usize::try_from(read_u32(&self.receive_buffer[24..28]))
            .map_err(|_| LocalProtocolError::PayloadTooLarge)?;
        if payload_length > self.maximum_payload {
            return Err(LocalProtocolError::PayloadTooLarge.into());
        }
        let frame_length = LOCAL_FRAME_HEADER_SIZE
            .checked_add(payload_length)
            .ok_or(LocalProtocolError::PayloadTooLarge)?;
        self.receive_buffer.resize(frame_length, 0);
        read_frame_tail(reader, &mut self.receive_buffer[LOCAL_FRAME_HEADER_SIZE..])?;
        decode_frame(&self.receive_buffer, self.maximum_payload)
            .map(Some)
            .map_err(Into::into)
    }

    /// Encodes and writes one complete frame through the reusable send buffer.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized payload or byte-stream write failure.
    pub fn send_to<W: Write>(
        &mut self,
        writer: &mut W,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        payload: &[u8],
    ) -> Result<(), LocalTransportError> {
        let encoded = encode_frame_into(
            &mut self.send_buffer,
            kind,
            stream_id,
            request_id,
            payload,
            self.maximum_payload,
        )?;
        writer.write_all(encoded)?;
        Ok(())
    }
}

/// Encodes one complete native local frame.
///
/// # Errors
///
/// Returns an error if the payload exceeds the configured maximum or `u32`.
pub fn encode_frame(
    kind: FrameKind,
    stream_id: u32,
    request_id: u64,
    payload: &[u8],
    maximum_payload: usize,
) -> Result<Vec<u8>, LocalProtocolError> {
    let mut frame = Vec::new();
    encode_frame_into(
        &mut frame,
        kind,
        stream_id,
        request_id,
        payload,
        maximum_payload,
    )?;
    Ok(frame)
}

fn encode_frame_into<'buffer>(
    frame: &'buffer mut Vec<u8>,
    kind: FrameKind,
    stream_id: u32,
    request_id: u64,
    payload: &[u8],
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalProtocolError> {
    if payload.len() > maximum_payload {
        return Err(LocalProtocolError::PayloadTooLarge);
    }
    let payload_length =
        u32::try_from(payload.len()).map_err(|_| LocalProtocolError::PayloadTooLarge)?;
    frame.resize(LOCAL_FRAME_HEADER_SIZE + payload.len(), 0);
    frame[..LOCAL_FRAME_HEADER_SIZE].fill(0);
    frame[0..8].copy_from_slice(MAGIC);
    frame[8..10].copy_from_slice(&MAJOR_VERSION.to_le_bytes());
    frame[10] = kind as u8;
    frame[12..16].copy_from_slice(&stream_id.to_le_bytes());
    frame[16..24].copy_from_slice(&request_id.to_le_bytes());
    frame[24..28].copy_from_slice(&payload_length.to_le_bytes());
    frame[LOCAL_FRAME_HEADER_SIZE..].copy_from_slice(payload);
    let checksum = frame_checksum(frame);
    frame[CHECKSUM_START..CHECKSUM_END].copy_from_slice(&checksum.to_le_bytes());
    Ok(frame)
}

/// Decodes one complete frame without allocating its payload.
///
/// # Errors
///
/// Returns an error for truncation, unknown versions/kinds, reserved flags,
/// length divergence, oversized payload, or checksum failure.
pub fn decode_frame(
    frame: &[u8],
    maximum_payload: usize,
) -> Result<DecodedFrame<'_>, LocalProtocolError> {
    if frame.len() < LOCAL_FRAME_HEADER_SIZE {
        return Err(LocalProtocolError::Truncated);
    }
    if &frame[0..8] != MAGIC || read_u16(&frame[8..10]) != MAJOR_VERSION {
        return Err(LocalProtocolError::InvalidPreamble);
    }
    if frame[11] != 0 {
        return Err(LocalProtocolError::ReservedFlags);
    }
    let payload_length = usize::try_from(read_u32(&frame[24..28]))
        .map_err(|_| LocalProtocolError::PayloadTooLarge)?;
    if payload_length > maximum_payload {
        return Err(LocalProtocolError::PayloadTooLarge);
    }
    if frame.len() != LOCAL_FRAME_HEADER_SIZE + payload_length {
        return Err(LocalProtocolError::LengthMismatch);
    }
    let expected_checksum = read_u32(&frame[CHECKSUM_START..CHECKSUM_END]);
    if frame_checksum(frame) != expected_checksum {
        return Err(LocalProtocolError::ChecksumMismatch);
    }
    Ok(DecodedFrame {
        kind: FrameKind::try_from(frame[10])?,
        stream_id: read_u32(&frame[12..16]),
        request_id: read_u64(&frame[16..24]),
        payload: &frame[LOCAL_FRAME_HEADER_SIZE..],
    })
}

fn frame_checksum(frame: &[u8]) -> u32 {
    let mut checksum = crc32c::crc32c(&frame[..CHECKSUM_START]);
    checksum = crc32c::crc32c_append(checksum, &[0; CHECKSUM_END - CHECKSUM_START]);
    crc32c::crc32c_append(checksum, &frame[CHECKSUM_END..])
}

fn read_header<R: Read>(reader: &mut R, header: &mut [u8]) -> Result<bool, LocalTransportError> {
    let mut offset = 0;
    while offset < header.len() {
        match reader.read(&mut header[offset..]) {
            Ok(0) if offset == 0 => return Ok(false),
            Ok(0) => return Err(LocalTransportError::Truncated),
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

fn read_frame_tail<R: Read>(reader: &mut R, payload: &mut [u8]) -> Result<(), LocalTransportError> {
    let mut offset = 0;
    while offset < payload.len() {
        match reader.read(&mut payload[offset..]) {
            Ok(0) => return Err(LocalTransportError::Truncated),
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, LocalFrameIo, LocalProtocolError, decode_frame,
        encode_frame,
    };

    #[test]
    fn local_frame_round_trips_without_payload_allocation() -> Result<(), Box<dyn std::error::Error>>
    {
        let encoded = encode_frame(
            FrameKind::Structure,
            7,
            42,
            b"session",
            DEFAULT_MAX_FRAME_PAYLOAD,
        )?;
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "70db9ece6d900078af4565c7c0017ee6de90c08d28f92443a0135d8cb6fb7120"
        );
        let decoded = decode_frame(&encoded, DEFAULT_MAX_FRAME_PAYLOAD)?;
        assert_eq!(decoded.kind, FrameKind::Structure);
        assert_eq!(decoded.stream_id, 7);
        assert_eq!(decoded.request_id, 42);
        assert_eq!(decoded.payload, b"session");
        Ok(())
    }

    #[test]
    fn local_frame_rejects_corruption_and_unknown_kinds() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut encoded = encode_frame(FrameKind::Ping, 0, 1, b"", 0)?;
        encoded[10] = 255;
        let checksum = super::frame_checksum(&encoded);
        encoded[28..32].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            decode_frame(&encoded, 0),
            Err(LocalProtocolError::UnknownKind(255))
        );

        let mut corrupt = encode_frame(FrameKind::Ping, 0, 1, b"", 0)?;
        corrupt[16] ^= 1;
        assert_eq!(
            decode_frame(&corrupt, 0),
            Err(LocalProtocolError::ChecksumMismatch)
        );
        Ok(())
    }

    #[test]
    fn framed_io_reuses_buffers_without_retaining_prior_frame_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut framed_io = LocalFrameIo::new(64)?;
        let mut large_output = Vec::new();
        framed_io.send_to(&mut large_output, FrameKind::Ping, 7, 41, &[0xa5; 64])?;
        let send_capacity = framed_io.send_buffer.capacity();

        let mut empty_output = Vec::new();
        framed_io.send_to(&mut empty_output, FrameKind::Close, 0, 42, b"")?;
        assert_eq!(framed_io.send_buffer.capacity(), send_capacity);
        let decoded = decode_frame(&empty_output, 64)?;
        assert_eq!(decoded.kind, FrameKind::Close);
        assert_eq!(decoded.stream_id, 0);
        assert_eq!(decoded.request_id, 42);
        assert!(decoded.payload.is_empty());

        let mut large_input = Cursor::new(large_output);
        let decoded = framed_io
            .receive_from(&mut large_input)?
            .ok_or("large frame was treated as EOF")?;
        assert_eq!(decoded.payload, &[0xa5; 64]);
        let receive_capacity = framed_io.receive_buffer.capacity();

        let mut empty_input = Cursor::new(empty_output);
        let decoded = framed_io
            .receive_from(&mut empty_input)?
            .ok_or("empty frame was treated as EOF")?;
        assert_eq!(decoded.kind, FrameKind::Close);
        assert!(decoded.payload.is_empty());
        assert_eq!(framed_io.receive_buffer.capacity(), receive_capacity);
        Ok(())
    }
}
