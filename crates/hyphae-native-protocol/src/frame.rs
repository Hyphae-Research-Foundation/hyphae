// SPDX-License-Identifier: AGPL-3.0-only

use std::io;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, DecodedFrame, FrameKind, LOCAL_FRAME_HEADER_SIZE,
    LocalProtocolError, decode_frame, encode_frame,
};

/// An owned decoded frame suitable for asynchronous dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedFrame {
    /// Message family.
    pub kind: FrameKind,
    /// Multiplexed stream identity.
    pub stream_id: u32,
    /// Request identity unique while active.
    pub request_id: u64,
    /// Canonical frame payload.
    pub payload: Vec<u8>,
}

/// Asynchronous frame transport failure.
#[derive(Debug, Error)]
pub enum FrameIoError {
    /// Configured payload bound exceeds the protocol-wide maximum.
    #[error("native protocol payload bound exceeds the protocol maximum")]
    MaximumPayloadTooLarge,
    /// The byte stream ended within one frame.
    #[error("native protocol frame is truncated")]
    Truncated,
    /// A complete frame violates `HYPHLCL1`.
    #[error(transparent)]
    Protocol(#[from] LocalProtocolError),
    /// The local byte stream failed.
    #[error("native protocol I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Bounded asynchronous `HYPHLCL1` frame reader/writer.
#[derive(Debug)]
pub struct AsyncFrameIo {
    maximum_payload: usize,
    receive: Vec<u8>,
}

impl AsyncFrameIo {
    /// Creates one frame codec with a strict per-frame payload bound.
    pub fn new(maximum_payload: usize) -> Result<Self, FrameIoError> {
        if maximum_payload > DEFAULT_MAX_FRAME_PAYLOAD {
            return Err(FrameIoError::MaximumPayloadTooLarge);
        }
        Ok(Self {
            maximum_payload,
            receive: Vec::with_capacity(LOCAL_FRAME_HEADER_SIZE),
        })
    }

    /// Returns the configured frame payload bound.
    pub const fn maximum_payload(&self) -> usize {
        self.maximum_payload
    }

    /// Reads one complete frame, returning `None` only for clean frame-boundary EOF.
    pub async fn receive<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<OwnedFrame>, FrameIoError> {
        self.receive.resize(LOCAL_FRAME_HEADER_SIZE, 0);
        let mut offset = 0;
        while offset < LOCAL_FRAME_HEADER_SIZE {
            match reader.read(&mut self.receive[offset..]).await {
                Ok(0) if offset == 0 => return Ok(None),
                Ok(0) => return Err(FrameIoError::Truncated),
                Ok(read) => offset += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        let length = usize::try_from(u32::from_le_bytes(
            self.receive[24..28]
                .try_into()
                .map_err(|_| LocalProtocolError::Truncated)?,
        ))
        .map_err(|_| LocalProtocolError::PayloadTooLarge)?;
        if length > self.maximum_payload {
            return Err(LocalProtocolError::PayloadTooLarge.into());
        }
        let frame_length = LOCAL_FRAME_HEADER_SIZE
            .checked_add(length)
            .ok_or(LocalProtocolError::PayloadTooLarge)?;
        self.receive.resize(frame_length, 0);
        if reader
            .read_exact(&mut self.receive[LOCAL_FRAME_HEADER_SIZE..])
            .await
            .is_err()
        {
            return Err(FrameIoError::Truncated);
        }
        let frame = decode_frame(&self.receive, self.maximum_payload)?;
        Ok(Some(OwnedFrame {
            kind: frame.kind,
            stream_id: frame.stream_id,
            request_id: frame.request_id,
            payload: frame.payload.to_vec(),
        }))
    }

    /// Encodes and writes one complete frame.
    pub async fn send<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        payload: &[u8],
    ) -> Result<(), FrameIoError> {
        let frame = encode_frame(kind, stream_id, request_id, payload, self.maximum_payload)?;
        writer.write_all(&frame).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Shared canonical frame vector used by protocol and transport tests.
pub fn golden_structure_frame() -> Result<Vec<u8>, LocalProtocolError> {
    encode_frame(
        FrameKind::Structure,
        7,
        42,
        b"session",
        DEFAULT_MAX_FRAME_PAYLOAD,
    )
}

/// BLAKE3 identity of [`golden_structure_frame`].
pub const GOLDEN_STRUCTURE_FRAME_BLAKE3: &str =
    "70db9ece6d900078af4565c7c0017ee6de90c08d28f92443a0135d8cb6fb7120";
