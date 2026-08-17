// SPDX-License-Identifier: Apache-2.0

use std::io;

use hyphae_core::{DISK_FORMAT_VERSION, MIN_DISK_FORMAT_VERSION};
use uuid::Uuid;

use super::LogError;

pub(super) const HEADER_LENGTH: usize = 112;
pub(super) const MAX_PAYLOAD_LENGTH: usize = 16 * 1024 * 1024;
const MAGIC: [u8; 8] = *b"HYPHAE01";
const INTEGRITY_PREFIX_LENGTH: usize = 76;
const DIGEST_PREFIX_LENGTH: usize = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum FrameKind {
    Begin = 1,
    Operation = 2,
    Commit = 3,
}

impl TryFrom<u8> for FrameKind {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Operation),
            3 => Ok(Self::Commit),
            other => Err(other),
        }
    }
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) kind: FrameKind,
    pub(super) sequence: u64,
    pub(super) transaction_id: Uuid,
    pub(super) previous_digest: [u8; 32],
    pub(super) digest: [u8; 32],
    pub(super) payload: Vec<u8>,
}

impl Frame {
    pub(super) fn encode(&self, disk_format_version: u16) -> Result<Vec<u8>, LogError> {
        if !(MIN_DISK_FORMAT_VERSION..=DISK_FORMAT_VERSION).contains(&disk_format_version) {
            return Err(LogError::UnsupportedVersion {
                offset: 0,
                found: disk_format_version,
                supported: DISK_FORMAT_VERSION,
            });
        }
        let payload_length =
            u64::try_from(self.payload.len()).map_err(|_| LogError::PayloadTooLarge {
                length: self.payload.len(),
                maximum: MAX_PAYLOAD_LENGTH,
            })?;
        if self.payload.len() > MAX_PAYLOAD_LENGTH {
            return Err(LogError::PayloadTooLarge {
                length: self.payload.len(),
                maximum: MAX_PAYLOAD_LENGTH,
            });
        }

        let mut encoded = vec![0_u8; HEADER_LENGTH + self.payload.len()];
        encoded[0..8].copy_from_slice(&MAGIC);
        encoded[8..10].copy_from_slice(&disk_format_version.to_le_bytes());
        encoded[10] = self.kind as u8;
        encoded[11] = 0;
        encoded[12..20].copy_from_slice(&self.sequence.to_le_bytes());
        encoded[20..36].copy_from_slice(self.transaction_id.as_bytes());
        encoded[36..44].copy_from_slice(&payload_length.to_le_bytes());
        encoded[44..76].copy_from_slice(&self.previous_digest);
        encoded[HEADER_LENGTH..].copy_from_slice(&self.payload);

        let checksum = crc32c_parts(&encoded[..INTEGRITY_PREFIX_LENGTH], &self.payload);
        encoded[76..80].copy_from_slice(&checksum.to_le_bytes());
        let digest = blake3_parts(&encoded[..DIGEST_PREFIX_LENGTH], &self.payload);
        encoded[80..112].copy_from_slice(&digest);
        Ok(encoded)
    }

    pub(super) fn decode(
        header: &[u8; HEADER_LENGTH],
        payload: Vec<u8>,
        offset: u64,
        supported_disk_format_version: u16,
    ) -> Result<Self, LogError> {
        let kind = validate_preamble(header, offset, supported_disk_format_version)?;

        let sequence = u64::from_le_bytes(copy_array(&header[12..20]));
        let transaction_id = Uuid::from_bytes(copy_array(&header[20..36]));
        let previous_digest = copy_array(&header[44..76]);
        let expected_checksum = u32::from_le_bytes(copy_array(&header[76..80]));
        let actual_checksum = crc32c_parts(&header[..INTEGRITY_PREFIX_LENGTH], &payload);
        if actual_checksum != expected_checksum {
            return Err(LogError::ChecksumMismatch { sequence });
        }
        let expected_digest: [u8; 32] = copy_array(&header[80..112]);
        let actual_digest = blake3_parts(&header[..DIGEST_PREFIX_LENGTH], &payload);
        if actual_digest != expected_digest {
            return Err(LogError::DigestMismatch { sequence });
        }

        Ok(Self {
            kind,
            sequence,
            transaction_id,
            previous_digest,
            digest: expected_digest,
            payload,
        })
    }
}

pub(super) fn payload_length(
    header: &[u8; HEADER_LENGTH],
    offset: u64,
    supported_disk_format_version: u16,
) -> Result<usize, LogError> {
    validate_preamble(header, offset, supported_disk_format_version)?;
    let raw = u64::from_le_bytes(copy_array(&header[36..44]));
    let length = usize::try_from(raw).map_err(|_| LogError::PayloadTooLarge {
        length: usize::MAX,
        maximum: MAX_PAYLOAD_LENGTH,
    })?;
    if length > MAX_PAYLOAD_LENGTH {
        return Err(LogError::PayloadTooLarge {
            length,
            maximum: MAX_PAYLOAD_LENGTH,
        });
    }
    Ok(length)
}

fn crc32c_parts(prefix: &[u8], payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(prefix), payload)
}

fn validate_preamble(
    header: &[u8; HEADER_LENGTH],
    offset: u64,
    supported_disk_format_version: u16,
) -> Result<FrameKind, LogError> {
    if header[0..8] != MAGIC {
        return Err(LogError::BadMagic { offset });
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version < MIN_DISK_FORMAT_VERSION || version > supported_disk_format_version {
        return Err(LogError::UnsupportedVersion {
            offset,
            found: version,
            supported: supported_disk_format_version,
        });
    }
    let kind = FrameKind::try_from(header[10])
        .map_err(|kind| LogError::UnknownFrameKind { offset, kind })?;
    if header[11] != 0 {
        return Err(LogError::UnsupportedFlags {
            offset,
            flags: header[11],
        });
    }
    Ok(kind)
}

fn blake3_parts(prefix: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix);
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn copy_array<const N: usize>(source: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(source);
    output
}

pub(super) fn read_exact_or_tail(
    reader: &mut impl io::Read,
    buffer: &mut [u8],
) -> io::Result<ReadStatus> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..])? {
            0 => break,
            count => read += count,
        }
    }
    Ok(if read == buffer.len() {
        ReadStatus::Complete
    } else if read == 0 {
        ReadStatus::End
    } else {
        ReadStatus::Partial
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadStatus {
    Complete,
    End,
    Partial,
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use hyphae_core::DISK_FORMAT_VERSION;

    use super::{Frame, FrameKind, HEADER_LENGTH};

    #[test]
    fn frame_encoding_is_canonical() -> Result<(), Box<dyn Error>> {
        let transaction_id = uuid::Uuid::now_v7();
        let frame = Frame {
            kind: FrameKind::Operation,
            sequence: 7,
            transaction_id,
            previous_digest: [3; 32],
            digest: [0; 32],
            payload: b"value".to_vec(),
        };

        let encoded = frame.encode(DISK_FORMAT_VERSION)?;
        let mut header = [0_u8; HEADER_LENGTH];
        header.copy_from_slice(&encoded[..HEADER_LENGTH]);
        let decoded = Frame::decode(
            &header,
            encoded[HEADER_LENGTH..].to_vec(),
            0,
            DISK_FORMAT_VERSION,
        )?;

        assert_eq!(decoded.kind, FrameKind::Operation);
        assert_eq!(decoded.sequence, 7);
        assert_eq!(decoded.transaction_id, transaction_id);
        assert_eq!(decoded.previous_digest, [3; 32]);
        assert_eq!(decoded.payload, b"value");
        Ok(())
    }
}
