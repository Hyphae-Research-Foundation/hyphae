// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use crate::Value;
use thiserror::Error;

const MAGIC: [u8; 8] = *b"HYDOC001";
const DOCUMENT_FORMAT_VERSION: u16 = 1;
const HEADER_LENGTH: usize = 56;
const CHECKSUM_PREFIX_LENGTH: usize = 20;
const DIGEST_PREFIX_LENGTH: usize = 24;
const NULL: u8 = 0;
const FALSE: u8 = 1;
const TRUE: u8 = 2;
const INTEGER: u8 = 3;
const STRING: u8 = 4;
const BYTES: u8 = 5;
const ARRAY: u8 = 6;
const OBJECT: u8 = 7;

/// Maximum canonical document payload bytes.
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum nested array/object depth, with the root at depth zero.
pub const MAX_DOCUMENT_DEPTH: usize = 64;
/// Maximum values decoded from one document.
pub const MAX_DOCUMENT_NODES: usize = 1_000_000;

/// Failure while encoding or verifying one canonical structured document.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DocumentError {
    /// Canonical payload exceeds the hard bound.
    #[error("document payload is {actual} bytes; maximum is {maximum}")]
    TooLarge {
        /// Observed or attempted payload length.
        actual: usize,
        /// Hard maximum.
        maximum: usize,
    },

    /// Array/object nesting exceeds the hard bound.
    #[error("document depth exceeds maximum {maximum}")]
    TooDeep {
        /// Hard maximum.
        maximum: usize,
    },

    /// Value count exceeds the hard bound.
    #[error("document node count exceeds maximum {maximum}")]
    TooManyNodes {
        /// Hard maximum.
        maximum: usize,
    },

    /// Length cannot be represented by the canonical format.
    #[error("document length overflow")]
    LengthOverflow,

    /// Encoded bytes are truncated or structurally noncanonical.
    #[error("invalid canonical document: {reason}")]
    Invalid {
        /// Stable diagnostic reason.
        reason: &'static str,
    },

    /// Document format is newer than this binary.
    #[error("unsupported document format {found}; supported format is {supported}")]
    UnsupportedVersion {
        /// Version found on disk.
        found: u16,
        /// Highest supported version.
        supported: u16,
    },

    /// Fast accidental-corruption check failed.
    #[error("document CRC32C mismatch")]
    ChecksumMismatch,

    /// Canonical content digest failed.
    #[error("document BLAKE3 mismatch")]
    DigestMismatch,

    /// A string or object key is not UTF-8.
    #[error("document contains invalid UTF-8")]
    InvalidUtf8,
}

/// Encodes one structured value into a checksummed canonical binary document.
///
/// # Errors
///
/// Returns an error for depth, node, length, or payload limits.
pub fn encode_document(value: &Value) -> Result<Vec<u8>, DocumentError> {
    let mut encoder = Encoder {
        payload: Vec::new(),
        nodes: 0,
    };
    encoder.value(value, 0)?;
    encode_envelope(&encoder.payload)
}

/// Returns the exact canonical envelope length without allocating the encoded
/// document.
///
/// # Errors
///
/// Returns the same depth, node, payload, or arithmetic-limit errors as
/// [`encode_document`].
pub fn encoded_document_len(value: &Value) -> Result<usize, DocumentError> {
    let mut measurer = DocumentMeasurer {
        payload_bytes: 0,
        nodes: 0,
    };
    measurer.value(value, 0)?;
    HEADER_LENGTH
        .checked_add(measurer.payload_bytes)
        .ok_or(DocumentError::LengthOverflow)
}

/// Verifies and decodes one canonical binary document.
///
/// # Errors
///
/// Returns an error for version, length, integrity, UTF-8, ordering, depth,
/// node count, unknown tags, or trailing bytes.
pub fn decode_document(encoded: &[u8]) -> Result<Value, DocumentError> {
    if encoded.len() < HEADER_LENGTH {
        return Err(DocumentError::Invalid {
            reason: "truncated header",
        });
    }
    if encoded[..8] != MAGIC {
        return Err(DocumentError::Invalid {
            reason: "bad magic",
        });
    }
    let version = u16::from_le_bytes(copy_array(&encoded[8..10]));
    if version != DOCUMENT_FORMAT_VERSION {
        return Err(DocumentError::UnsupportedVersion {
            found: version,
            supported: DOCUMENT_FORMAT_VERSION,
        });
    }
    if u16::from_le_bytes(copy_array(&encoded[10..12])) != 0 {
        return Err(DocumentError::Invalid {
            reason: "unsupported flags",
        });
    }
    let payload_length = usize::try_from(u64::from_le_bytes(copy_array(&encoded[12..20])))
        .map_err(|_| DocumentError::LengthOverflow)?;
    if payload_length > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            actual: payload_length,
            maximum: MAX_DOCUMENT_BYTES,
        });
    }
    let expected_length = HEADER_LENGTH
        .checked_add(payload_length)
        .ok_or(DocumentError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(DocumentError::Invalid {
            reason: "file length mismatch",
        });
    }
    let payload = &encoded[HEADER_LENGTH..];
    let expected_checksum = u32::from_le_bytes(copy_array(&encoded[20..24]));
    let actual_checksum =
        crc32c::crc32c_append(crc32c::crc32c(&encoded[..CHECKSUM_PREFIX_LENGTH]), payload);
    if actual_checksum != expected_checksum {
        return Err(DocumentError::ChecksumMismatch);
    }
    let expected_digest: [u8; 32] = copy_array(&encoded[24..56]);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&encoded[..DIGEST_PREFIX_LENGTH]);
    hasher.update(payload);
    if *hasher.finalize().as_bytes() != expected_digest {
        return Err(DocumentError::DigestMismatch);
    }

    let mut decoder = Decoder {
        payload,
        position: 0,
        nodes: 0,
    };
    let value = decoder.value(0)?;
    if decoder.position != payload.len() {
        return Err(DocumentError::Invalid {
            reason: "trailing payload bytes",
        });
    }
    Ok(value)
}

struct Encoder {
    payload: Vec<u8>,
    nodes: usize,
}

impl Encoder {
    fn value(&mut self, value: &Value, depth: usize) -> Result<(), DocumentError> {
        if depth > MAX_DOCUMENT_DEPTH {
            return Err(DocumentError::TooDeep {
                maximum: MAX_DOCUMENT_DEPTH,
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            })?;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            });
        }
        match value {
            Value::Null => self.append(&[NULL]),
            Value::Boolean(false) => self.append(&[FALSE]),
            Value::Boolean(true) => self.append(&[TRUE]),
            Value::Integer(value) => {
                self.append(&[INTEGER])?;
                self.append(&value.to_le_bytes())
            }
            Value::String(value) => {
                self.append(&[STRING])?;
                self.length_prefixed(value.as_bytes())
            }
            Value::Bytes(value) => {
                self.append(&[BYTES])?;
                self.length_prefixed(value)
            }
            Value::Array(values) => {
                self.append(&[ARRAY])?;
                self.length(values.len())?;
                let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
                    maximum: MAX_DOCUMENT_DEPTH,
                })?;
                for value in values {
                    self.value(value, child_depth)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                self.append(&[OBJECT])?;
                self.length(values.len())?;
                let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
                    maximum: MAX_DOCUMENT_DEPTH,
                })?;
                for (key, value) in values {
                    self.length_prefixed(key.as_bytes())?;
                    self.value(value, child_depth)?;
                }
                Ok(())
            }
        }
    }

    fn length_prefixed(&mut self, value: &[u8]) -> Result<(), DocumentError> {
        self.length(value.len())?;
        self.append(value)
    }

    fn length(&mut self, length: usize) -> Result<(), DocumentError> {
        let encoded = u64::try_from(length).map_err(|_| DocumentError::LengthOverflow)?;
        self.append(&encoded.to_le_bytes())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), DocumentError> {
        let next = self
            .payload
            .len()
            .checked_add(bytes.len())
            .ok_or(DocumentError::LengthOverflow)?;
        if next > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge {
                actual: next,
                maximum: MAX_DOCUMENT_BYTES,
            });
        }
        self.payload.extend_from_slice(bytes);
        Ok(())
    }
}

struct DocumentMeasurer {
    payload_bytes: usize,
    nodes: usize,
}

impl DocumentMeasurer {
    fn value(&mut self, value: &Value, depth: usize) -> Result<(), DocumentError> {
        if depth > MAX_DOCUMENT_DEPTH {
            return Err(DocumentError::TooDeep {
                maximum: MAX_DOCUMENT_DEPTH,
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            })?;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            });
        }
        match value {
            Value::Null | Value::Boolean(_) => self.append(1),
            Value::Integer(_) => {
                self.append(1)?;
                self.append(8)
            }
            Value::String(value) => {
                self.append(1)?;
                self.length_prefixed(value.len())
            }
            Value::Bytes(value) => {
                self.append(1)?;
                self.length_prefixed(value.len())
            }
            Value::Array(values) => {
                self.append(1)?;
                self.append(8)?;
                let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
                    maximum: MAX_DOCUMENT_DEPTH,
                })?;
                for value in values {
                    self.value(value, child_depth)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                self.append(1)?;
                self.append(8)?;
                let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
                    maximum: MAX_DOCUMENT_DEPTH,
                })?;
                for (key, value) in values {
                    self.length_prefixed(key.len())?;
                    self.value(value, child_depth)?;
                }
                Ok(())
            }
        }
    }

    fn length_prefixed(&mut self, length: usize) -> Result<(), DocumentError> {
        self.append(8)?;
        self.append(length)
    }

    fn append(&mut self, bytes: usize) -> Result<(), DocumentError> {
        let next = self
            .payload_bytes
            .checked_add(bytes)
            .ok_or(DocumentError::LengthOverflow)?;
        if next > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge {
                actual: next,
                maximum: MAX_DOCUMENT_BYTES,
            });
        }
        self.payload_bytes = next;
        Ok(())
    }
}

struct Decoder<'payload> {
    payload: &'payload [u8],
    position: usize,
    nodes: usize,
}

impl Decoder<'_> {
    fn value(&mut self, depth: usize) -> Result<Value, DocumentError> {
        if depth > MAX_DOCUMENT_DEPTH {
            return Err(DocumentError::TooDeep {
                maximum: MAX_DOCUMENT_DEPTH,
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            })?;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            });
        }
        let tag = self.read(1)?[0];
        match tag {
            NULL => Ok(Value::Null),
            FALSE => Ok(Value::Boolean(false)),
            TRUE => Ok(Value::Boolean(true)),
            INTEGER => Ok(Value::Integer(i64::from_le_bytes(copy_array(
                self.read(8)?,
            )))),
            STRING => {
                let bytes = self.length_prefixed()?;
                let value = std::str::from_utf8(bytes).map_err(|_| DocumentError::InvalidUtf8)?;
                Ok(Value::String(value.to_owned()))
            }
            BYTES => Ok(Value::Bytes(self.length_prefixed()?.to_vec())),
            ARRAY => self.array(depth),
            OBJECT => self.object(depth),
            _ => Err(DocumentError::Invalid {
                reason: "unknown value tag",
            }),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, DocumentError> {
        let count = self.length()?;
        if count > MAX_DOCUMENT_NODES {
            return Err(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            });
        }
        let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
            maximum: MAX_DOCUMENT_DEPTH,
        })?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.value(child_depth)?);
        }
        Ok(Value::Array(values))
    }

    fn object(&mut self, depth: usize) -> Result<Value, DocumentError> {
        let count = self.length()?;
        if count > MAX_DOCUMENT_NODES {
            return Err(DocumentError::TooManyNodes {
                maximum: MAX_DOCUMENT_NODES,
            });
        }
        let child_depth = depth.checked_add(1).ok_or(DocumentError::TooDeep {
            maximum: MAX_DOCUMENT_DEPTH,
        })?;
        let mut values = BTreeMap::new();
        let mut previous: Option<String> = None;
        for _ in 0..count {
            let bytes = self.length_prefixed()?;
            let key = std::str::from_utf8(bytes)
                .map_err(|_| DocumentError::InvalidUtf8)?
                .to_owned();
            if previous.as_ref().is_some_and(|previous| previous >= &key) {
                return Err(DocumentError::Invalid {
                    reason: "object keys are not strictly sorted",
                });
            }
            let value = self.value(child_depth)?;
            previous = Some(key.clone());
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }

    fn length_prefixed(&mut self) -> Result<&[u8], DocumentError> {
        let length = self.length()?;
        self.read(length)
    }

    fn length(&mut self) -> Result<usize, DocumentError> {
        usize::try_from(u64::from_le_bytes(copy_array(self.read(8)?)))
            .map_err(|_| DocumentError::LengthOverflow)
    }

    fn read(&mut self, length: usize) -> Result<&[u8], DocumentError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(DocumentError::LengthOverflow)?;
        let Some(value) = self.payload.get(self.position..end) else {
            return Err(DocumentError::Invalid {
                reason: "truncated value payload",
            });
        };
        self.position = end;
        Ok(value)
    }
}

fn encode_envelope(payload: &[u8]) -> Result<Vec<u8>, DocumentError> {
    if payload.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            actual: payload.len(),
            maximum: MAX_DOCUMENT_BYTES,
        });
    }
    let payload_length = u64::try_from(payload.len()).map_err(|_| DocumentError::LengthOverflow)?;
    let capacity = HEADER_LENGTH
        .checked_add(payload.len())
        .ok_or(DocumentError::LengthOverflow)?;
    let mut encoded = vec![0_u8; capacity];
    encoded[..8].copy_from_slice(&MAGIC);
    encoded[8..10].copy_from_slice(&DOCUMENT_FORMAT_VERSION.to_le_bytes());
    encoded[10..12].copy_from_slice(&0_u16.to_le_bytes());
    encoded[12..20].copy_from_slice(&payload_length.to_le_bytes());
    encoded[HEADER_LENGTH..].copy_from_slice(payload);
    let checksum =
        crc32c::crc32c_append(crc32c::crc32c(&encoded[..CHECKSUM_PREFIX_LENGTH]), payload);
    encoded[20..24].copy_from_slice(&checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&encoded[..DIGEST_PREFIX_LENGTH]);
    hasher.update(payload);
    encoded[24..56].copy_from_slice(hasher.finalize().as_bytes());
    Ok(encoded)
}

fn copy_array<const N: usize>(source: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(source);
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::Value;

    use super::{
        DOCUMENT_FORMAT_VERSION, DocumentError, MAX_DOCUMENT_DEPTH, NULL, OBJECT, decode_document,
        encode_document, encode_envelope, encoded_document_len,
    };

    #[test]
    fn canonical_document_round_trips_binary_and_nested_values() -> Result<(), DocumentError> {
        let value = Value::Object(BTreeMap::from([
            ("bytes".to_owned(), Value::Bytes(vec![0, 255, 7])),
            (
                "nested".to_owned(),
                Value::Array(vec![Value::Integer(-7), Value::Null]),
            ),
        ]));
        let first = encode_document(&value)?;
        let second = encode_document(&value)?;
        assert_eq!(encoded_document_len(&value)?, first.len());
        assert_eq!(first, second);
        assert_eq!(decode_document(&first)?, value);
        Ok(())
    }

    #[test]
    fn corruption_and_future_versions_fail_before_decoding() -> Result<(), DocumentError> {
        let mut corrupted = encode_document(&Value::Integer(7))?;
        let last = corrupted.len() - 1;
        corrupted[last] ^= 1;
        assert_eq!(
            decode_document(&corrupted),
            Err(DocumentError::ChecksumMismatch)
        );

        let mut future = encode_document(&Value::Null)?;
        future[8..10].copy_from_slice(&(DOCUMENT_FORMAT_VERSION + 1).to_le_bytes());
        assert_eq!(
            decode_document(&future),
            Err(DocumentError::UnsupportedVersion {
                found: DOCUMENT_FORMAT_VERSION + 1,
                supported: DOCUMENT_FORMAT_VERSION
            })
        );
        Ok(())
    }

    #[test]
    fn depth_is_bounded_during_encoding() {
        let mut value = Value::Null;
        for _ in 0..=MAX_DOCUMENT_DEPTH {
            value = Value::Array(vec![value]);
        }
        assert_eq!(
            encode_document(&value),
            Err(DocumentError::TooDeep {
                maximum: MAX_DOCUMENT_DEPTH
            })
        );
    }

    #[test]
    fn decoder_rejects_noncanonical_object_key_order() -> Result<(), DocumentError> {
        let mut payload = vec![OBJECT];
        payload.extend_from_slice(&2_u64.to_le_bytes());
        payload.extend_from_slice(&1_u64.to_le_bytes());
        payload.extend_from_slice(b"b");
        payload.push(NULL);
        payload.extend_from_slice(&1_u64.to_le_bytes());
        payload.extend_from_slice(b"a");
        payload.push(NULL);
        let encoded = encode_envelope(&payload)?;
        assert_eq!(
            decode_document(&encoded),
            Err(DocumentError::Invalid {
                reason: "object keys are not strictly sorted"
            })
        );
        Ok(())
    }
}
