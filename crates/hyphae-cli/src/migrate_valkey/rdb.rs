// SPDX-License-Identifier: Apache-2.0

//! Bounded offline parser for Valkey/Redis RDB files.
//!
//! The parser reads one complete RDB byte payload, verifies the running
//! CRC-64 trailer, decodes every supported value family, and collects the
//! constructs it cannot represent so the importer can classify them and fail
//! closed unless the operator explicitly waives them. Unknown opcodes, value
//! types, and encodings always fail closed naming the offending byte.

use std::collections::BTreeMap;

use thiserror::Error;

use super::lzf::{self, LzfError};

/// Lowest accepted RDB format version (Redis 4.0).
pub(crate) const MIN_RDB_VERSION: u32 = 8;
/// Highest accepted RDB format version (Redis 7.x / Valkey 8.x).
pub(crate) const MAX_RDB_VERSION: u32 = 12;

/// Bounded reader policy, mirroring the snapshot reader discipline.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RdbReadLimits {
    /// Maximum RDB file bytes.
    pub max_file_bytes: u64,
    /// Maximum total keys across all databases.
    pub max_total_keys: u64,
    /// Maximum key bytes.
    pub max_key_bytes: usize,
    /// Maximum decoded value bytes for one scalar.
    pub max_value_bytes: usize,
    /// Maximum members in one container value.
    pub max_container_members: usize,
    /// Maximum entries in one stream value.
    pub max_stream_entries: usize,
    /// Maximum logical databases.
    pub max_databases: u32,
    /// Maximum auxiliary fields.
    pub max_aux_fields: usize,
}

impl Default for RdbReadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024 * 1024,
            max_total_keys: 1_000_000,
            max_key_bytes: 64 * 1024,
            max_value_bytes: 64 * 1024 * 1024,
            max_container_members: 1_000_000,
            max_stream_entries: 1_000_000,
            max_databases: 16,
            max_aux_fields: 128,
        }
    }
}

/// Failure while parsing one bounded RDB payload.
#[derive(Debug, Error)]
pub(crate) enum RdbError {
    /// The payload did not start with the RDB magic.
    #[error("RDB magic is invalid")]
    Magic,
    /// The RDB version is outside the supported window.
    #[error("RDB version {found} is outside the supported {MIN_RDB_VERSION}..={MAX_RDB_VERSION}")]
    Version {
        /// Declared version.
        found: u32,
    },
    /// The payload ended inside a structure.
    #[error("RDB payload is truncated at offset {offset}")]
    Truncated {
        /// Offset where more bytes were required.
        offset: usize,
    },
    /// An opcode is unknown to this parser.
    #[error("RDB opcode 0x{opcode:02x} at offset {offset} is unsupported")]
    UnknownOpcode {
        /// Opcode byte.
        opcode: u8,
        /// Offset of the opcode.
        offset: usize,
    },
    /// A value type is unknown to this parser.
    #[error("RDB value type {value_type} at offset {offset} is unsupported")]
    UnknownValueType {
        /// Value-type byte.
        value_type: u8,
        /// Offset of the value type.
        offset: usize,
    },
    /// A construct cannot be skipped safely and aborts the parse.
    #[error("RDB construct {construct} cannot be represented or skipped")]
    Unskippable {
        /// Construct identifier.
        construct: &'static str,
    },
    /// A bound was exceeded.
    #[error("RDB payload exceeds {field} limit {maximum}")]
    Limit {
        /// Bounded field.
        field: &'static str,
        /// Maximum admitted value.
        maximum: u64,
    },
    /// The CRC-64 trailer did not match the payload.
    #[error("RDB checksum differs: declared {declared:016x}, computed {computed:016x}")]
    Checksum {
        /// Declared trailer value.
        declared: u64,
        /// Computed running value.
        computed: u64,
    },
    /// An inner encoding violated its own format.
    #[error("RDB encoding is invalid: {0}")]
    Encoding(&'static str),
    /// LZF decompression failed.
    #[error("RDB LZF payload is invalid: {0}")]
    Lzf(#[from] LzfError),
}

/// Byte field/value pairs in stored order.
pub(crate) type BytePairs = Vec<(Vec<u8>, Vec<u8>)>;

/// One decoded stream entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RdbStreamEntry {
    /// Entry identifier milliseconds component.
    pub id_ms: u64,
    /// Entry identifier sequence component.
    pub id_seq: u64,
    /// Entry field/value pairs in stored order.
    pub fields: BytePairs,
}

/// One decoded stream value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RdbStream {
    /// Live entries in identifier order.
    pub entries: Vec<RdbStreamEntry>,
    /// Number of consumer groups attached to the stream.
    pub group_count: u64,
}

/// One decoded value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RdbValue {
    /// Scalar byte string.
    String(Vec<u8>),
    /// List members in order.
    List(Vec<Vec<u8>>),
    /// Set members in stored order.
    Set(Vec<Vec<u8>>),
    /// Hash field/value pairs in stored order.
    Hash(BytePairs),
    /// Sorted-set member/score pairs in stored order.
    SortedSet(Vec<(Vec<u8>, f64)>),
    /// Stream entries and group metadata.
    Stream(RdbStream),
}

impl RdbValue {
    /// Stable family identifier used in classifications and keyspace names.
    pub(crate) fn family(&self) -> &'static str {
        match self {
            Self::String(_) => "strings",
            Self::List(_) => "lists",
            Self::Set(_) => "sets",
            Self::Hash(_) => "hashes",
            Self::SortedSet(_) => "sorted_sets",
            Self::Stream(_) => "streams",
        }
    }
}

/// One decoded key record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RdbRecord {
    /// Logical database index.
    pub db_index: u32,
    /// Key bytes.
    pub key: Vec<u8>,
    /// Absolute expiry in unix milliseconds, when present.
    pub expires_at_ms: Option<u64>,
    /// Decoded value.
    pub value: RdbValue,
}

/// Complete parsed RDB payload.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RdbFile {
    /// Declared RDB format version.
    pub version: u32,
    /// Auxiliary fields in encounter order (later sorted for receipts).
    pub aux_fields: Vec<(String, String)>,
    /// Every decoded record.
    pub records: Vec<RdbRecord>,
    /// Whether the trailer carried a verified checksum.
    pub checksum_present: bool,
    /// Trailer checksum value when present.
    pub checksum: u64,
    /// Counts of encountered constructs that have no exact mapping.
    pub encountered: BTreeMap<&'static str, u64>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], RdbError> {
        let end = self.offset.checked_add(count).ok_or(RdbError::Truncated {
            offset: self.offset,
        })?;
        if end > self.bytes.len() {
            return Err(RdbError::Truncated {
                offset: self.offset,
            });
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, RdbError> {
        Ok(self.take(1)?[0])
    }

    fn u16_le(&mut self) -> Result<u16, RdbError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32_le(&mut self) -> Result<u32, RdbError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64_le(&mut self) -> Result<u64, RdbError> {
        let bytes = self.take(8)?;
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(raw))
    }

    fn u64_be(&mut self) -> Result<u64, RdbError> {
        let bytes = self.take(8)?;
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(raw))
    }

    fn done(&self) -> bool {
        self.offset >= self.bytes.len()
    }
}

enum Length {
    Value(u64),
    Encoded(u8),
}

fn read_length(cursor: &mut Cursor<'_>) -> Result<Length, RdbError> {
    let first = cursor.u8()?;
    match first >> 6 {
        0 => Ok(Length::Value(u64::from(first & 0x3f))),
        1 => {
            let second = cursor.u8()?;
            Ok(Length::Value(
                (u64::from(first & 0x3f) << 8) | u64::from(second),
            ))
        }
        2 => {
            if first == 0x80 {
                let bytes = cursor.take(4)?;
                Ok(Length::Value(u64::from(u32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))))
            } else if first == 0x81 {
                Ok(Length::Value(cursor.u64_be()?))
            } else {
                Err(RdbError::Encoding("length prefix is invalid"))
            }
        }
        _ => Ok(Length::Encoded(first & 0x3f)),
    }
}

fn read_exact_length(cursor: &mut Cursor<'_>) -> Result<u64, RdbError> {
    match read_length(cursor)? {
        Length::Value(value) => Ok(value),
        Length::Encoded(_) => Err(RdbError::Encoding("expected a plain length")),
    }
}

fn bounded_len(value: u64, field: &'static str, maximum: usize) -> Result<usize, RdbError> {
    let length = usize::try_from(value).map_err(|_| RdbError::Limit {
        field,
        maximum: maximum as u64,
    })?;
    if length > maximum {
        return Err(RdbError::Limit {
            field,
            maximum: maximum as u64,
        });
    }
    Ok(length)
}

fn read_string(cursor: &mut Cursor<'_>, limits: &RdbReadLimits) -> Result<Vec<u8>, RdbError> {
    match read_length(cursor)? {
        Length::Value(value) => {
            let length = bounded_len(value, "value_bytes", limits.max_value_bytes)?;
            Ok(cursor.take(length)?.to_vec())
        }
        Length::Encoded(0) => {
            let value = cursor.u8()?.cast_signed();
            Ok(value.to_string().into_bytes())
        }
        Length::Encoded(1) => {
            let bytes = cursor.take(2)?;
            let value = i16::from_le_bytes([bytes[0], bytes[1]]);
            Ok(value.to_string().into_bytes())
        }
        Length::Encoded(2) => {
            let bytes = cursor.take(4)?;
            let value = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            Ok(value.to_string().into_bytes())
        }
        Length::Encoded(3) => {
            let compressed = bounded_len(
                read_exact_length(cursor)?,
                "value_bytes",
                limits.max_value_bytes,
            )?;
            let declared = bounded_len(
                read_exact_length(cursor)?,
                "value_bytes",
                limits.max_value_bytes,
            )?;
            let payload = cursor.take(compressed)?;
            Ok(lzf::decompress(payload, declared, limits.max_value_bytes)?)
        }
        Length::Encoded(_) => Err(RdbError::Encoding("string special encoding is unknown")),
    }
}

fn ziplist_entry(cursor: &mut Cursor<'_>) -> Result<Vec<u8>, RdbError> {
    let prev = cursor.u8()?;
    if prev == 0xfe {
        cursor.take(4)?;
    }
    let encoding = cursor.u8()?;
    match encoding >> 6 {
        0 => {
            let length = usize::from(encoding & 0x3f);
            Ok(cursor.take(length)?.to_vec())
        }
        1 => {
            let second = cursor.u8()?;
            let length = (usize::from(encoding & 0x3f) << 8) | usize::from(second);
            Ok(cursor.take(length)?.to_vec())
        }
        2 => {
            let bytes = cursor.take(4)?;
            let length =
                usize::try_from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .map_err(|_| RdbError::Encoding("ziplist string length overflows"))?;
            Ok(cursor.take(length)?.to_vec())
        }
        _ => ziplist_integer(cursor, encoding),
    }
}

fn ziplist_integer(cursor: &mut Cursor<'_>, encoding: u8) -> Result<Vec<u8>, RdbError> {
    let value: i64 = match encoding {
        0xc0 => {
            let bytes = cursor.take(2)?;
            i64::from(i16::from_le_bytes([bytes[0], bytes[1]]))
        }
        0xd0 => {
            let bytes = cursor.take(4)?;
            i64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        0xe0 => {
            let bytes = cursor.take(8)?;
            let mut raw = [0_u8; 8];
            raw.copy_from_slice(bytes);
            i64::from_le_bytes(raw)
        }
        0xf0 => {
            let bytes = cursor.take(3)?;
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) << 8 >> 8;
            i64::from(raw)
        }
        0xfe => {
            let byte = cursor.u8()?;
            i64::from(byte.cast_signed())
        }
        0xf1..=0xfd => i64::from(encoding & 0x0f) - 1,
        _ => return Err(RdbError::Encoding("ziplist entry encoding is unknown")),
    };
    Ok(value.to_string().into_bytes())
}

fn ziplist_entries(bytes: &[u8], limits: &RdbReadLimits) -> Result<Vec<Vec<u8>>, RdbError> {
    let mut cursor = Cursor { bytes, offset: 0 };
    cursor.take(4)?;
    cursor.take(4)?;
    cursor.u16_le()?;
    let mut entries = Vec::new();
    loop {
        if entries.len() > limits.max_container_members {
            return Err(RdbError::Limit {
                field: "container_members",
                maximum: limits.max_container_members as u64,
            });
        }
        let next = *cursor.bytes.get(cursor.offset).ok_or(RdbError::Truncated {
            offset: cursor.offset,
        })?;
        if next == 0xff {
            return Ok(entries);
        }
        entries.push(ziplist_entry(&mut cursor)?);
    }
}

fn listpack_entry(cursor: &mut Cursor<'_>) -> Result<Vec<u8>, RdbError> {
    let start = cursor.offset;
    let first = cursor.u8()?;
    let value: Vec<u8> = if first & 0x80 == 0 {
        u64::from(first & 0x7f).to_string().into_bytes()
    } else if first & 0xc0 == 0x80 {
        let length = usize::from(first & 0x3f);
        cursor.take(length)?.to_vec()
    } else if first & 0xe0 == 0xc0 {
        let second = cursor.u8()?;
        let raw = ((i64::from(first & 0x1f) << 8) | i64::from(second)) << 51 >> 51;
        raw.to_string().into_bytes()
    } else if first & 0xf0 == 0xe0 {
        let second = cursor.u8()?;
        let length = (usize::from(first & 0x0f) << 8) | usize::from(second);
        cursor.take(length)?.to_vec()
    } else {
        listpack_wide_entry(cursor, first)?
    };
    let consumed = cursor.offset - start;
    let backlen_size = match consumed {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        2_097_152..=268_435_455 => 4,
        _ => 5,
    };
    cursor.take(backlen_size)?;
    Ok(value)
}

fn listpack_wide_entry(cursor: &mut Cursor<'_>, first: u8) -> Result<Vec<u8>, RdbError> {
    match first {
        0xf1 => {
            let bytes = cursor.take(2)?;
            Ok(i64::from(i16::from_le_bytes([bytes[0], bytes[1]]))
                .to_string()
                .into_bytes())
        }
        0xf2 => {
            let bytes = cursor.take(3)?;
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) << 8 >> 8;
            Ok(i64::from(raw).to_string().into_bytes())
        }
        0xf3 => {
            let bytes = cursor.take(4)?;
            Ok(
                i64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .to_string()
                    .into_bytes(),
            )
        }
        0xf4 => {
            let bytes = cursor.take(8)?;
            let mut raw = [0_u8; 8];
            raw.copy_from_slice(bytes);
            Ok(i64::from_le_bytes(raw).to_string().into_bytes())
        }
        0xf0 => {
            let bytes = cursor.take(4)?;
            let length =
                usize::try_from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .map_err(|_| RdbError::Encoding("listpack string length overflows"))?;
            Ok(cursor.take(length)?.to_vec())
        }
        _ => Err(RdbError::Encoding("listpack entry encoding is unknown")),
    }
}

fn listpack_entries(bytes: &[u8], limits: &RdbReadLimits) -> Result<Vec<Vec<u8>>, RdbError> {
    let mut cursor = Cursor { bytes, offset: 0 };
    cursor.take(4)?;
    cursor.u16_le()?;
    let mut entries = Vec::new();
    loop {
        if entries.len() > limits.max_container_members {
            return Err(RdbError::Limit {
                field: "container_members",
                maximum: limits.max_container_members as u64,
            });
        }
        let next = *cursor.bytes.get(cursor.offset).ok_or(RdbError::Truncated {
            offset: cursor.offset,
        })?;
        if next == 0xff {
            return Ok(entries);
        }
        entries.push(listpack_entry(&mut cursor)?);
    }
}

fn intset_entries(bytes: &[u8], limits: &RdbReadLimits) -> Result<Vec<Vec<u8>>, RdbError> {
    let mut cursor = Cursor { bytes, offset: 0 };
    let encoding = cursor.u32_le()?;
    let count = bounded_len(
        u64::from(cursor.u32_le()?),
        "container_members",
        limits.max_container_members,
    )?;
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        let value: i64 = match encoding {
            2 => {
                let raw = cursor.take(2)?;
                i64::from(i16::from_le_bytes([raw[0], raw[1]]))
            }
            4 => {
                let raw = cursor.take(4)?;
                i64::from(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
            }
            8 => {
                let raw = cursor.take(8)?;
                let mut wide = [0_u8; 8];
                wide.copy_from_slice(raw);
                i64::from_le_bytes(wide)
            }
            _ => return Err(RdbError::Encoding("intset encoding is unknown")),
        };
        members.push(value.to_string().into_bytes());
    }
    Ok(members)
}

fn pairs(entries: Vec<Vec<u8>>, construct: &'static str) -> Result<BytePairs, RdbError> {
    if !entries.len().is_multiple_of(2) {
        return Err(RdbError::Encoding(construct));
    }
    let mut paired = Vec::with_capacity(entries.len() / 2);
    let mut iterator = entries.into_iter();
    while let (Some(first), Some(second)) = (iterator.next(), iterator.next()) {
        paired.push((first, second));
    }
    Ok(paired)
}

fn parse_double_string(bytes: &[u8]) -> Result<f64, RdbError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RdbError::Encoding("sorted-set score is not UTF-8"))?;
    text.parse::<f64>()
        .map_err(|_| RdbError::Encoding("sorted-set score is not numeric"))
}

fn read_legacy_double(cursor: &mut Cursor<'_>) -> Result<f64, RdbError> {
    let length = cursor.u8()?;
    match length {
        253 => Err(RdbError::Encoding("sorted-set score is NaN")),
        254 => Ok(f64::INFINITY),
        255 => Ok(f64::NEG_INFINITY),
        _ => parse_double_string(cursor.take(usize::from(length))?),
    }
}

fn score_from_bytes(bytes: &[u8]) -> Result<f64, RdbError> {
    let score = parse_double_string(bytes)?;
    if score.is_nan() {
        return Err(RdbError::Encoding("sorted-set score is NaN"));
    }
    Ok(score)
}

/// Parses one complete RDB payload under the configured bounds.
#[allow(clippy::too_many_lines)]
pub(crate) fn parse(bytes: &[u8], limits: &RdbReadLimits) -> Result<RdbFile, RdbError> {
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(RdbError::Limit {
            field: "file_bytes",
            maximum: limits.max_file_bytes,
        });
    }
    let mut cursor = Cursor { bytes, offset: 0 };
    let magic = cursor.take(9)?;
    if &magic[..5] != b"REDIS" {
        return Err(RdbError::Magic);
    }
    let version = std::str::from_utf8(&magic[5..])
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
        .ok_or(RdbError::Magic)?;
    if !(MIN_RDB_VERSION..=MAX_RDB_VERSION).contains(&version) {
        return Err(RdbError::Version { found: version });
    }
    let mut file = RdbFile {
        version,
        aux_fields: Vec::new(),
        records: Vec::new(),
        checksum_present: false,
        checksum: 0,
        encountered: BTreeMap::new(),
    };
    let mut db_index = 0_u32;
    let mut databases_seen = 0_u32;
    let mut expires_at_ms: Option<u64> = None;
    loop {
        let opcode_offset = cursor.offset;
        let opcode = cursor.u8()?;
        match opcode {
            0xff => {
                let declared = cursor.u64_le()?;
                if !cursor.done() {
                    return Err(RdbError::Encoding("bytes follow the RDB trailer"));
                }
                if declared == 0 {
                    *file.encountered.entry("checksum-absent").or_insert(0) += 1;
                } else {
                    let computed = super::crc64::update(0, &bytes[..=opcode_offset]);
                    if computed != declared {
                        return Err(RdbError::Checksum { declared, computed });
                    }
                    file.checksum_present = true;
                    file.checksum = declared;
                }
                return Ok(file);
            }
            0xfe => {
                db_index = u32::try_from(read_exact_length(&mut cursor)?)
                    .map_err(|_| RdbError::Encoding("database index overflows"))?;
                databases_seen += 1;
                if databases_seen > limits.max_databases {
                    return Err(RdbError::Limit {
                        field: "databases",
                        maximum: u64::from(limits.max_databases),
                    });
                }
            }
            0xfb => {
                read_exact_length(&mut cursor)?;
                read_exact_length(&mut cursor)?;
            }
            0xfd => {
                expires_at_ms = Some(u64::from(cursor.u32_le()?).saturating_mul(1000));
                continue;
            }
            0xfc => {
                expires_at_ms = Some(cursor.u64_le()?);
                continue;
            }
            0xfa => {
                if file.aux_fields.len() >= limits.max_aux_fields {
                    return Err(RdbError::Limit {
                        field: "aux_fields",
                        maximum: limits.max_aux_fields as u64,
                    });
                }
                let name = read_string(&mut cursor, limits)?;
                let value = read_string(&mut cursor, limits)?;
                file.aux_fields.push((
                    String::from_utf8_lossy(&name).into_owned(),
                    String::from_utf8_lossy(&value).into_owned(),
                ));
            }
            0xf8 => {
                read_exact_length(&mut cursor)?;
                continue;
            }
            0xf9 => {
                cursor.u8()?;
                continue;
            }
            0xf5 => {
                read_string(&mut cursor, limits)?;
                *file.encountered.entry("functions").or_insert(0) += 1;
            }
            0xf4 => {
                read_exact_length(&mut cursor)?;
                read_exact_length(&mut cursor)?;
                read_exact_length(&mut cursor)?;
                *file.encountered.entry("cluster-slot-info").or_insert(0) += 1;
            }
            0xf7 | 0xf6 => {
                return Err(RdbError::Unskippable {
                    construct: "module-aux",
                });
            }
            value_type => {
                if value_type >= 0x80 {
                    return Err(RdbError::UnknownOpcode {
                        opcode: value_type,
                        offset: opcode_offset,
                    });
                }
                if file.records.len() as u64 >= limits.max_total_keys {
                    return Err(RdbError::Limit {
                        field: "total_keys",
                        maximum: limits.max_total_keys,
                    });
                }
                let key = read_string(&mut cursor, limits)?;
                if key.len() > limits.max_key_bytes {
                    return Err(RdbError::Limit {
                        field: "key_bytes",
                        maximum: limits.max_key_bytes as u64,
                    });
                }
                let value = read_value(&mut cursor, value_type, opcode_offset, limits, &mut file)?;
                if let Some(value) = value {
                    file.records.push(RdbRecord {
                        db_index,
                        key,
                        expires_at_ms: expires_at_ms.take(),
                        value,
                    });
                }
            }
        }
        expires_at_ms = None;
    }
}

fn read_value(
    cursor: &mut Cursor<'_>,
    value_type: u8,
    offset: usize,
    limits: &RdbReadLimits,
    file: &mut RdbFile,
) -> Result<Option<RdbValue>, RdbError> {
    match value_type {
        0 => Ok(Some(RdbValue::String(read_string(cursor, limits)?))),
        1 | 2 => {
            let count = bounded_len(
                read_exact_length(cursor)?,
                "container_members",
                limits.max_container_members,
            )?;
            let mut members = Vec::with_capacity(count);
            for _ in 0..count {
                members.push(read_string(cursor, limits)?);
            }
            Ok(Some(if value_type == 1 {
                RdbValue::List(members)
            } else {
                RdbValue::Set(members)
            }))
        }
        3 | 5 => read_sorted_set(cursor, value_type, limits).map(Some),
        4 => {
            let count = bounded_len(
                read_exact_length(cursor)?,
                "container_members",
                limits.max_container_members,
            )?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let field = read_string(cursor, limits)?;
                let value = read_string(cursor, limits)?;
                entries.push((field, value));
            }
            Ok(Some(RdbValue::Hash(entries)))
        }
        9 => {
            read_string(cursor, limits)?;
            *file.encountered.entry("zipmap").or_insert(0) += 1;
            Ok(None)
        }
        10 => Ok(Some(RdbValue::List(ziplist_entries(
            &read_string(cursor, limits)?,
            limits,
        )?))),
        11 => Ok(Some(RdbValue::Set(intset_entries(
            &read_string(cursor, limits)?,
            limits,
        )?))),
        12 | 17 => {
            let payload = read_string(cursor, limits)?;
            let entries = if value_type == 12 {
                ziplist_entries(&payload, limits)?
            } else {
                listpack_entries(&payload, limits)?
            };
            let mut members = Vec::with_capacity(entries.len() / 2);
            for (member, score) in pairs(entries, "sorted-set element count is odd")? {
                members.push((member, score_from_bytes(&score)?));
            }
            Ok(Some(RdbValue::SortedSet(members)))
        }
        13 | 16 => {
            let payload = read_string(cursor, limits)?;
            let entries = if value_type == 13 {
                ziplist_entries(&payload, limits)?
            } else {
                listpack_entries(&payload, limits)?
            };
            Ok(Some(RdbValue::Hash(pairs(
                entries,
                "hash element count is odd",
            )?)))
        }
        14 | 18 => read_quicklist(cursor, value_type, limits).map(Some),
        20 => Ok(Some(RdbValue::Set(listpack_entries(
            &read_string(cursor, limits)?,
            limits,
        )?))),
        15 | 19 | 21 => read_stream(cursor, value_type, limits, file).map(Some),
        6 | 7 => Err(RdbError::Unskippable {
            construct: "modules",
        }),
        22..=25 => {
            *file.encountered.entry("hash-field-ttl").or_insert(0) += 1;
            Err(RdbError::Unskippable {
                construct: "hash-field-ttl",
            })
        }
        _ => Err(RdbError::UnknownValueType { value_type, offset }),
    }
}

fn read_sorted_set(
    cursor: &mut Cursor<'_>,
    value_type: u8,
    limits: &RdbReadLimits,
) -> Result<RdbValue, RdbError> {
    let count = bounded_len(
        read_exact_length(cursor)?,
        "container_members",
        limits.max_container_members,
    )?;
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        let member = read_string(cursor, limits)?;
        let score = if value_type == 5 {
            let raw = cursor.take(8)?;
            let mut wide = [0_u8; 8];
            wide.copy_from_slice(raw);
            let score = f64::from_le_bytes(wide);
            if score.is_nan() {
                return Err(RdbError::Encoding("sorted-set score is NaN"));
            }
            score
        } else {
            read_legacy_double(cursor)?
        };
        members.push((member, score));
    }
    Ok(RdbValue::SortedSet(members))
}

fn read_quicklist(
    cursor: &mut Cursor<'_>,
    value_type: u8,
    limits: &RdbReadLimits,
) -> Result<RdbValue, RdbError> {
    let nodes = bounded_len(
        read_exact_length(cursor)?,
        "container_members",
        limits.max_container_members,
    )?;
    let mut members = Vec::new();
    for _ in 0..nodes {
        if value_type == 18 {
            let container = read_exact_length(cursor)?;
            let payload = read_string(cursor, limits)?;
            match container {
                1 => members.push(payload),
                2 => members.extend(listpack_entries(&payload, limits)?),
                _ => return Err(RdbError::Encoding("quicklist container is unknown")),
            }
        } else {
            let payload = read_string(cursor, limits)?;
            members.extend(ziplist_entries(&payload, limits)?);
        }
        if members.len() > limits.max_container_members {
            return Err(RdbError::Limit {
                field: "container_members",
                maximum: limits.max_container_members as u64,
            });
        }
    }
    Ok(RdbValue::List(members))
}

fn stream_id_from_key(key: &[u8]) -> Result<(u64, u64), RdbError> {
    if key.len() != 16 {
        return Err(RdbError::Encoding("stream master key is not 16 bytes"));
    }
    let mut ms = [0_u8; 8];
    ms.copy_from_slice(&key[..8]);
    let mut seq = [0_u8; 8];
    seq.copy_from_slice(&key[8..]);
    Ok((u64::from_be_bytes(ms), u64::from_be_bytes(seq)))
}

fn listpack_integer(bytes: &[u8], label: &'static str) -> Result<i64, RdbError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .ok_or(RdbError::Encoding(label))
}

#[allow(clippy::too_many_lines)]
fn read_stream(
    cursor: &mut Cursor<'_>,
    value_type: u8,
    limits: &RdbReadLimits,
    file: &mut RdbFile,
) -> Result<RdbValue, RdbError> {
    let listpacks = bounded_len(
        read_exact_length(cursor)?,
        "stream_entries",
        limits.max_stream_entries,
    )?;
    let mut entries = Vec::new();
    for _ in 0..listpacks {
        let master_key = read_string(cursor, limits)?;
        let (master_ms, master_seq) = stream_id_from_key(&master_key)?;
        let payload = read_string(cursor, limits)?;
        let elements = listpack_entries(&payload, limits)?;
        decode_stream_listpack(&elements, master_ms, master_seq, &mut entries, limits)?;
    }
    read_exact_length(cursor)?;
    read_exact_length(cursor)?;
    read_exact_length(cursor)?;
    if value_type >= 19 {
        read_exact_length(cursor)?;
        read_exact_length(cursor)?;
        read_exact_length(cursor)?;
        read_exact_length(cursor)?;
        read_exact_length(cursor)?;
    }
    let groups = read_exact_length(cursor)?;
    for _ in 0..groups {
        skip_stream_group(cursor, value_type, limits)?;
    }
    if groups > 0 {
        *file
            .encountered
            .entry("stream-consumer-groups")
            .or_insert(0) += groups;
    }
    Ok(RdbValue::Stream(RdbStream {
        entries,
        group_count: groups,
    }))
}

fn decode_stream_listpack(
    elements: &[Vec<u8>],
    master_ms: u64,
    master_seq: u64,
    entries: &mut Vec<RdbStreamEntry>,
    limits: &RdbReadLimits,
) -> Result<(), RdbError> {
    let mut index = 0_usize;
    let next = |index: &mut usize| -> Result<&Vec<u8>, RdbError> {
        let element = elements
            .get(*index)
            .ok_or(RdbError::Encoding("stream listpack is truncated"))?;
        *index += 1;
        Ok(element)
    };
    let count = listpack_integer(next(&mut index)?, "stream count is invalid")?;
    let deleted = listpack_integer(next(&mut index)?, "stream deleted count is invalid")?;
    let master_fields =
        listpack_integer(next(&mut index)?, "stream master field count is invalid")?;
    let master_fields = usize::try_from(master_fields)
        .map_err(|_| RdbError::Encoding("stream master field count is negative"))?;
    let mut master_names = Vec::with_capacity(master_fields);
    for _ in 0..master_fields {
        master_names.push(next(&mut index)?.clone());
    }
    next(&mut index)?;
    let total = count
        .checked_add(deleted)
        .ok_or(RdbError::Encoding("stream entry count overflows"))?;
    for _ in 0..total {
        let flags = listpack_integer(next(&mut index)?, "stream entry flags are invalid")?;
        let ms_diff = u64::try_from(listpack_integer(
            next(&mut index)?,
            "stream ms diff is invalid",
        )?)
        .map_err(|_| RdbError::Encoding("stream ms diff is negative"))?;
        let seq_diff = u64::try_from(listpack_integer(
            next(&mut index)?,
            "stream seq diff is invalid",
        )?)
        .map_err(|_| RdbError::Encoding("stream seq diff is negative"))?;
        let same_fields = flags & 2 != 0;
        let deleted_entry = flags & 1 != 0;
        let mut fields = Vec::new();
        if same_fields {
            for name in &master_names {
                let value = next(&mut index)?.clone();
                fields.push((name.clone(), value));
            }
        } else {
            let field_count = usize::try_from(listpack_integer(
                next(&mut index)?,
                "stream field count is invalid",
            )?)
            .map_err(|_| RdbError::Encoding("stream field count is negative"))?;
            for _ in 0..field_count {
                let name = next(&mut index)?.clone();
                let value = next(&mut index)?.clone();
                fields.push((name, value));
            }
        }
        next(&mut index)?;
        if !deleted_entry {
            if entries.len() >= limits.max_stream_entries {
                return Err(RdbError::Limit {
                    field: "stream_entries",
                    maximum: limits.max_stream_entries as u64,
                });
            }
            entries.push(RdbStreamEntry {
                id_ms: master_ms.saturating_add(ms_diff),
                id_seq: master_seq.saturating_add(seq_diff),
                fields,
            });
        }
    }
    Ok(())
}

fn skip_stream_group(
    cursor: &mut Cursor<'_>,
    value_type: u8,
    limits: &RdbReadLimits,
) -> Result<(), RdbError> {
    read_string(cursor, limits)?;
    read_exact_length(cursor)?;
    read_exact_length(cursor)?;
    if value_type >= 19 {
        read_exact_length(cursor)?;
    }
    let pending = read_exact_length(cursor)?;
    for _ in 0..pending {
        cursor.take(16)?;
        cursor.u64_le()?;
        read_exact_length(cursor)?;
    }
    let consumers = read_exact_length(cursor)?;
    for _ in 0..consumers {
        read_string(cursor, limits)?;
        cursor.u64_le()?;
        if value_type >= 21 {
            cursor.u64_le()?;
        }
        let consumer_pending = read_exact_length(cursor)?;
        for _ in 0..consumer_pending {
            cursor.take(16)?;
        }
    }
    Ok(())
}
