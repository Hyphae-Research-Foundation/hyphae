// SPDX-License-Identifier: Apache-2.0

//! Canonical identities and logical types for Hyphae's native substrate.

use std::{
    error::Error,
    fmt,
    num::{NonZeroU32, NonZeroU64, NonZeroU128},
};

/// Canonical positive identity or type construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTypeError {
    /// A stable identity or sequence number was zero.
    ZeroIdentity(&'static str),
    /// A decimal precision was outside 1 through 38.
    InvalidDecimalPrecision,
    /// A decimal scale exceeded its precision.
    InvalidDecimalScale,
    /// A vector dimension was zero.
    EmptyVector,
    /// A logical-type descriptor was malformed or contained trailing bytes.
    InvalidTypeDescriptor,
    /// A logical type exceeded the maximum recursive nesting depth.
    TypeNestingDepthExceeded,
    /// A scalar value does not belong to the declared logical type.
    ScalarTypeMismatch,
    /// SQL null must be represented by the containing row's null bitmap.
    NullRequiresRowBitmap,
    /// A scalar byte representation was malformed or noncanonical.
    InvalidScalarEncoding,
    /// A scalar value exceeded its declared precision or domain.
    ScalarOutOfRange,
    /// A directory UUID was not a canonical RFC 9562 `UUIDv7` identity.
    InvalidDirectoryUuid,
    /// A lineage byte representation had the wrong length or invalid fields.
    InvalidLineageEncoding,
    /// An ordered-key codec is not yet defined for this logical type.
    UnsupportedOrderedType,
    /// A canonical scalar codec is not yet defined for this logical type.
    UnsupportedScalarType,
    /// One scalar value exceeded the native 16 MiB format maximum.
    ScalarLengthExceeded,
}

impl fmt::Display for NativeTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroIdentity(name) => write!(formatter, "{name} must be nonzero"),
            Self::InvalidDecimalPrecision => {
                formatter.write_str("decimal precision must be in 1..=38")
            }
            Self::InvalidDecimalScale => {
                formatter.write_str("decimal scale cannot exceed precision")
            }
            Self::EmptyVector => formatter.write_str("vector dimension must be nonzero"),
            Self::InvalidTypeDescriptor => {
                formatter.write_str("logical-type descriptor is invalid")
            }
            Self::TypeNestingDepthExceeded => {
                formatter.write_str("logical-type nesting exceeds 64")
            }
            Self::ScalarTypeMismatch => {
                formatter.write_str("scalar value does not match its logical type")
            }
            Self::NullRequiresRowBitmap => {
                formatter.write_str("SQL null must be represented by the row null bitmap")
            }
            Self::InvalidScalarEncoding => {
                formatter.write_str("scalar encoding is malformed or noncanonical")
            }
            Self::ScalarOutOfRange => {
                formatter.write_str("scalar value is outside its declared domain")
            }
            Self::InvalidDirectoryUuid => {
                formatter.write_str("directory UUID must be a canonical RFC 9562 UUIDv7")
            }
            Self::InvalidLineageEncoding => {
                formatter.write_str("directory lineage encoding is invalid")
            }
            Self::UnsupportedOrderedType => {
                formatter.write_str("logical type has no native ordered-key codec")
            }
            Self::UnsupportedScalarType => {
                formatter.write_str("logical type has no native scalar codec")
            }
            Self::ScalarLengthExceeded => {
                formatter.write_str("scalar value exceeds the 16 MiB format maximum")
            }
        }
    }
}

impl Error for NativeTypeError {}

/// Stable RFC 9562 `UUIDv7` identity for one native data directory.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DirectoryUuid([u8; 16]);

impl DirectoryUuid {
    /// Constructs a checked `UUIDv7` identity from network-order bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the version or RFC variant bits are invalid.
    pub fn new(bytes: [u8; 16]) -> Result<Self, NativeTypeError> {
        if bytes[6] >> 4 != 7 || bytes[8] & 0xc0 != 0x80 {
            return Err(NativeTypeError::InvalidDirectoryUuid);
        }
        Ok(Self(bytes))
    }

    /// Parses the exact lowercase hyphenated `UUIDv7` representation.
    ///
    /// # Errors
    ///
    /// Returns an error for noncanonical text, non-v7 bytes, or a non-RFC
    /// variant.
    pub fn parse_canonical(value: &str) -> Result<Self, NativeTypeError> {
        let encoded = value.as_bytes();
        if encoded.len() != 36
            || !matches!(
                (encoded[8], encoded[13], encoded[18], encoded[23]),
                (b'-', b'-', b'-', b'-')
            )
        {
            return Err(NativeTypeError::InvalidDirectoryUuid);
        }
        let mut bytes = [0_u8; 16];
        let mut source = 0_usize;
        let mut target = 0_usize;
        while source < encoded.len() {
            if matches!(source, 8 | 13 | 18 | 23) {
                source += 1;
                continue;
            }
            let high =
                decode_lower_hex(encoded[source]).ok_or(NativeTypeError::InvalidDirectoryUuid)?;
            let low = decode_lower_hex(encoded[source + 1])
                .ok_or(NativeTypeError::InvalidDirectoryUuid)?;
            bytes[target] = (high << 4) | low;
            source += 2;
            target += 1;
        }
        Self::new(bytes)
    }

    /// Returns the UUID bytes in RFC network order.
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for DirectoryUuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        write!(
            formatter,
            concat!(
                "{:02x}{:02x}{:02x}{:02x}-",
                "{:02x}{:02x}-",
                "{:02x}{:02x}-",
                "{:02x}{:02x}-",
                "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}"
            ),
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15],
        )
    }
}

const fn decode_lower_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

macro_rules! nonzero_identity {
    ($name:ident, $inner:ty, $nonzero:ty, $label:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name($nonzero);

        impl $name {
            /// Constructs a checked nonzero identity.
            ///
            /// # Errors
            ///
            /// Returns an error when `value` is zero.
            pub fn new(value: $inner) -> Result<Self, NativeTypeError> {
                <$nonzero>::new(value)
                    .map(Self)
                    .ok_or(NativeTypeError::ZeroIdentity($label))
            }

            /// Returns the primitive nonzero value.
            pub const fn get(self) -> $inner {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.get().fmt(formatter)
            }
        }
    };
}

nonzero_identity!(
    ObjectId,
    u128,
    NonZeroU128,
    "object ID",
    "Stable identity for a catalogued object."
);
nonzero_identity!(
    RowId,
    u128,
    NonZeroU128,
    "row ID",
    "Stable identity for one relational row."
);
nonzero_identity!(
    BlobId,
    u128,
    NonZeroU128,
    "blob ID",
    "Stable identity for one immutable blob."
);
nonzero_identity!(
    TransactionId,
    u128,
    NonZeroU128,
    "transaction ID",
    "Stable identity for one transaction attempt."
);
nonzero_identity!(
    ColumnId,
    u32,
    NonZeroU32,
    "column ID",
    "Stable identity for one column inside a relation."
);
nonzero_identity!(
    FieldId,
    u32,
    NonZeroU32,
    "field ID",
    "Stable identity for one field inside a search collection."
);
nonzero_identity!(
    PageId,
    u64,
    NonZeroU64,
    "page ID",
    "Physical page slot identity."
);
nonzero_identity!(
    PageGeneration,
    u64,
    NonZeroU64,
    "page generation",
    "Immutable page-file generation."
);
nonzero_identity!(
    HistoryEpoch,
    u64,
    NonZeroU64,
    "history epoch",
    "Monotonic identity for one nondivergent native directory history."
);

impl HistoryEpoch {
    /// Initial history epoch for a newly created native directory.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Returns the next history epoch before overflow.
    pub fn checked_next(self) -> Option<Self> {
        self.get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

/// Exact directory and history identity carried by native authority records.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LineageIdentity {
    directory_uuid: DirectoryUuid,
    history_epoch: HistoryEpoch,
}

impl LineageIdentity {
    /// Exact encoded lineage width.
    pub const ENCODED_SIZE: usize = 24;

    /// Constructs one checked lineage identity.
    pub const fn new(directory_uuid: DirectoryUuid, history_epoch: HistoryEpoch) -> Self {
        Self {
            directory_uuid,
            history_epoch,
        }
    }

    /// Returns the stable directory UUID.
    pub const fn directory_uuid(self) -> DirectoryUuid {
        self.directory_uuid
    }

    /// Returns the nonzero history epoch.
    pub const fn history_epoch(self) -> HistoryEpoch {
        self.history_epoch
    }

    /// Encodes UUID bytes followed by a little-endian history epoch.
    pub fn encode(self) -> [u8; Self::ENCODED_SIZE] {
        let mut encoded = [0_u8; Self::ENCODED_SIZE];
        encoded[..16].copy_from_slice(&self.directory_uuid.to_bytes());
        encoded[16..].copy_from_slice(&self.history_epoch.get().to_le_bytes());
        encoded
    }

    /// Decodes one exact lineage identity.
    ///
    /// # Errors
    ///
    /// Returns an error for the wrong length, invalid `UUIDv7`, or zero epoch.
    pub fn decode(encoded: &[u8]) -> Result<Self, NativeTypeError> {
        if encoded.len() != Self::ENCODED_SIZE {
            return Err(NativeTypeError::InvalidLineageEncoding);
        }
        let mut uuid = [0_u8; 16];
        uuid.copy_from_slice(&encoded[..16]);
        let mut epoch = [0_u8; 8];
        epoch.copy_from_slice(&encoded[16..]);
        let directory_uuid =
            DirectoryUuid::new(uuid).map_err(|_| NativeTypeError::InvalidLineageEncoding)?;
        let history_epoch = HistoryEpoch::new(u64::from_le_bytes(epoch))
            .map_err(|_| NativeTypeError::InvalidLineageEncoding)?;
        Ok(Self::new(directory_uuid, history_epoch))
    }
}

impl PageGeneration {
    /// Historical first page-file generation.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Returns the next immutable page-file generation.
    pub fn checked_next(self) -> Option<Self> {
        self.get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}
nonzero_identity!(
    CatalogVersion,
    u64,
    NonZeroU64,
    "catalog version",
    "Immutable catalog snapshot identity."
);
nonzero_identity!(
    ManifestGeneration,
    u64,
    NonZeroU64,
    "manifest generation",
    "Immutable root-manifest generation."
);
nonzero_identity!(
    Csn,
    u64,
    NonZeroU64,
    "CSN",
    "Committed transaction sequence number."
);
nonzero_identity!(Lsn, u64, NonZeroU64, "LSN", "WAL record byte position.");

impl Csn {
    /// First committed transaction sequence.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Returns the next commit sequence number.
    pub fn checked_next(self) -> Option<Self> {
        self.get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

impl CatalogVersion {
    /// Returns the next immutable catalog version.
    pub fn checked_next(self) -> Option<Self> {
        self.get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

/// Native engine owning an operation or catalog object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum EngineKind {
    /// Shared-kernel metadata.
    Kernel = 0,
    /// Relational data and indexes.
    Relational = 1,
    /// Keyspace and specialized structures.
    Structure = 2,
    /// Lexical, document, and vector search.
    Search = 3,
}

/// A transaction's physical durability promise.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum DurabilityClass {
    /// Acknowledge after the transaction's synchronization completes.
    Strict = 1,
    /// Synchronize and acknowledge a group of commits together.
    Group = 2,
    /// Publish without crash-durability acknowledgement.
    Memory = 3,
}

/// Supported fixed integer widths.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum IntegerWidth {
    /// Eight bits.
    Bits8 = 8,
    /// Sixteen bits.
    Bits16 = 16,
    /// Thirty-two bits.
    Bits32 = 32,
    /// Sixty-four bits.
    Bits64 = 64,
}

/// Checked fixed-precision decimal declaration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecimalType {
    precision: u8,
    scale: u8,
}

impl DecimalType {
    /// Constructs a decimal declaration.
    ///
    /// # Errors
    ///
    /// Returns an error unless precision is in 1 through 38 and scale does not
    /// exceed precision.
    pub fn new(precision: u8, scale: u8) -> Result<Self, NativeTypeError> {
        if !(1..=38).contains(&precision) {
            return Err(NativeTypeError::InvalidDecimalPrecision);
        }
        if scale > precision {
            return Err(NativeTypeError::InvalidDecimalScale);
        }
        Ok(Self { precision, scale })
    }

    /// Returns the decimal precision.
    pub const fn precision(self) -> u8 {
        self.precision
    }

    /// Returns the decimal scale.
    pub const fn scale(self) -> u8 {
        self.scale
    }
}

/// Vector element representation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum VectorElement {
    /// Canonical IEEE-754 binary32.
    Float32 = 1,
}

/// Checked fixed-dimension vector declaration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VectorType {
    element: VectorElement,
    dimension: u16,
}

impl VectorType {
    /// Constructs a checked vector declaration.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero dimension.
    pub fn new(element: VectorElement, dimension: u16) -> Result<Self, NativeTypeError> {
        if dimension == 0 {
            return Err(NativeTypeError::EmptyVector);
        }
        Ok(Self { element, dimension })
    }

    /// Returns the vector element representation.
    pub const fn element(self) -> VectorElement {
        self.element
    }

    /// Returns the fixed vector dimension.
    pub const fn dimension(self) -> u16 {
        self.dimension
    }
}

/// Canonical logical type shared by native engines.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LogicalType {
    /// Boolean.
    Boolean,
    /// Signed fixed-width integer.
    Signed(IntegerWidth),
    /// Unsigned fixed-width integer.
    Unsigned(IntegerWidth),
    /// Fixed precision decimal.
    Decimal(DecimalType),
    /// Canonical IEEE-754 binary32.
    Float32,
    /// Canonical IEEE-754 binary64.
    Float64,
    /// Valid UTF-8 text.
    Text,
    /// Arbitrary binary bytes.
    Binary,
    /// Signed days from 1970-01-01.
    Date,
    /// Nanoseconds from midnight.
    Time,
    /// Signed microseconds from Unix epoch.
    Timestamp,
    /// Signed months, days, and nanoseconds.
    Interval,
    /// UUID bits.
    Uuid,
    /// Canonical JSON.
    Json,
    /// Homogeneous ordered array.
    Array(Box<Self>),
    /// Canonically ordered typed map.
    Map(Box<Self>, Box<Self>),
    /// Fixed-dimension float vector.
    Vector(VectorType),
}

/// Maximum encoded byte length for one native scalar value.
pub const MAX_SCALAR_BYTES: usize = 16 * 1024 * 1024;
const MAX_TYPE_NESTING: usize = 64;
const MAX_ORDERED_SCALAR_BYTES: usize = (MAX_SCALAR_BYTES * 2) + 3;
const NANOS_PER_DAY: u64 = 86_400_000_000_000;
const ORDERED_NULL: u8 = 0;
const ORDERED_VALUE: u8 = 1;

const TYPE_BOOLEAN: u8 = 1;
const TYPE_SIGNED: u8 = 2;
const TYPE_UNSIGNED: u8 = 3;
const TYPE_DECIMAL: u8 = 4;
const TYPE_FLOAT32: u8 = 5;
const TYPE_FLOAT64: u8 = 6;
const TYPE_TEXT: u8 = 7;
const TYPE_BINARY: u8 = 8;
const TYPE_DATE: u8 = 9;
const TYPE_TIME: u8 = 10;
const TYPE_TIMESTAMP: u8 = 11;
const TYPE_INTERVAL: u8 = 12;
const TYPE_UUID: u8 = 13;
const TYPE_JSON: u8 = 14;
const TYPE_ARRAY: u8 = 15;
const TYPE_MAP: u8 = 16;
const TYPE_VECTOR: u8 = 17;

impl LogicalType {
    /// Encodes one self-delimiting canonical logical-type descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error when recursive array/map nesting exceeds 64.
    pub fn encode_descriptor(&self) -> Result<Vec<u8>, NativeTypeError> {
        let mut encoded = Vec::new();
        encode_type_descriptor(self, 0, &mut encoded)?;
        Ok(encoded)
    }

    /// Decodes one complete canonical logical-type descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tag, invalid parameter, excessive
    /// nesting, truncation, or trailing bytes.
    pub fn decode_descriptor(encoded: &[u8]) -> Result<Self, NativeTypeError> {
        let mut offset = 0;
        let decoded = decode_type_descriptor(encoded, &mut offset, 0)?;
        if offset != encoded.len() {
            return Err(NativeTypeError::InvalidTypeDescriptor);
        }
        Ok(decoded)
    }
}

fn encode_type_descriptor(
    logical_type: &LogicalType,
    depth: usize,
    output: &mut Vec<u8>,
) -> Result<(), NativeTypeError> {
    if depth > MAX_TYPE_NESTING {
        return Err(NativeTypeError::TypeNestingDepthExceeded);
    }
    match logical_type {
        LogicalType::Boolean => output.push(TYPE_BOOLEAN),
        LogicalType::Signed(width) => {
            output.extend_from_slice(&[TYPE_SIGNED, *width as u8]);
        }
        LogicalType::Unsigned(width) => {
            output.extend_from_slice(&[TYPE_UNSIGNED, *width as u8]);
        }
        LogicalType::Decimal(decimal) => {
            output.extend_from_slice(&[TYPE_DECIMAL, decimal.precision(), decimal.scale()]);
        }
        LogicalType::Float32 => output.push(TYPE_FLOAT32),
        LogicalType::Float64 => output.push(TYPE_FLOAT64),
        LogicalType::Text => output.push(TYPE_TEXT),
        LogicalType::Binary => output.push(TYPE_BINARY),
        LogicalType::Date => output.push(TYPE_DATE),
        LogicalType::Time => output.push(TYPE_TIME),
        LogicalType::Timestamp => output.push(TYPE_TIMESTAMP),
        LogicalType::Interval => output.push(TYPE_INTERVAL),
        LogicalType::Uuid => output.push(TYPE_UUID),
        LogicalType::Json => output.push(TYPE_JSON),
        LogicalType::Array(element) => {
            output.push(TYPE_ARRAY);
            encode_type_descriptor(element, depth + 1, output)?;
        }
        LogicalType::Map(key, value) => {
            output.push(TYPE_MAP);
            encode_type_descriptor(key, depth + 1, output)?;
            encode_type_descriptor(value, depth + 1, output)?;
        }
        LogicalType::Vector(vector) => {
            output.extend_from_slice(&[
                TYPE_VECTOR,
                vector.element() as u8,
                vector.dimension().to_le_bytes()[0],
                vector.dimension().to_le_bytes()[1],
            ]);
        }
    }
    Ok(())
}

fn decode_type_descriptor(
    encoded: &[u8],
    offset: &mut usize,
    depth: usize,
) -> Result<LogicalType, NativeTypeError> {
    if depth > MAX_TYPE_NESTING {
        return Err(NativeTypeError::TypeNestingDepthExceeded);
    }
    let tag = take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?;
    match tag {
        TYPE_BOOLEAN => Ok(LogicalType::Boolean),
        TYPE_SIGNED => Ok(LogicalType::Signed(decode_integer_width(
            take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?,
        )?)),
        TYPE_UNSIGNED => Ok(LogicalType::Unsigned(decode_integer_width(
            take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?,
        )?)),
        TYPE_DECIMAL => {
            let precision =
                take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?;
            let scale = take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?;
            DecimalType::new(precision, scale)
                .map(LogicalType::Decimal)
                .map_err(|_| NativeTypeError::InvalidTypeDescriptor)
        }
        TYPE_FLOAT32 => Ok(LogicalType::Float32),
        TYPE_FLOAT64 => Ok(LogicalType::Float64),
        TYPE_TEXT => Ok(LogicalType::Text),
        TYPE_BINARY => Ok(LogicalType::Binary),
        TYPE_DATE => Ok(LogicalType::Date),
        TYPE_TIME => Ok(LogicalType::Time),
        TYPE_TIMESTAMP => Ok(LogicalType::Timestamp),
        TYPE_INTERVAL => Ok(LogicalType::Interval),
        TYPE_UUID => Ok(LogicalType::Uuid),
        TYPE_JSON => Ok(LogicalType::Json),
        TYPE_ARRAY => Ok(LogicalType::Array(Box::new(decode_type_descriptor(
            encoded,
            offset,
            depth + 1,
        )?))),
        TYPE_MAP => Ok(LogicalType::Map(
            Box::new(decode_type_descriptor(encoded, offset, depth + 1)?),
            Box::new(decode_type_descriptor(encoded, offset, depth + 1)?),
        )),
        TYPE_VECTOR => {
            let element =
                take_byte(encoded, offset).ok_or(NativeTypeError::InvalidTypeDescriptor)?;
            if element != VectorElement::Float32 as u8 {
                return Err(NativeTypeError::InvalidTypeDescriptor);
            }
            let dimension = take_exact::<2>(encoded, offset)
                .map(u16::from_le_bytes)
                .ok_or(NativeTypeError::InvalidTypeDescriptor)?;
            VectorType::new(VectorElement::Float32, dimension)
                .map(LogicalType::Vector)
                .map_err(|_| NativeTypeError::InvalidTypeDescriptor)
        }
        _ => Err(NativeTypeError::InvalidTypeDescriptor),
    }
}

fn decode_integer_width(encoded: u8) -> Result<IntegerWidth, NativeTypeError> {
    match encoded {
        8 => Ok(IntegerWidth::Bits8),
        16 => Ok(IntegerWidth::Bits16),
        32 => Ok(IntegerWidth::Bits32),
        64 => Ok(IntegerWidth::Bits64),
        _ => Err(NativeTypeError::InvalidTypeDescriptor),
    }
}

fn take_byte(encoded: &[u8], offset: &mut usize) -> Option<u8> {
    let value = *encoded.get(*offset)?;
    *offset = offset.checked_add(1)?;
    Some(value)
}

fn take_exact<const LENGTH: usize>(encoded: &[u8], offset: &mut usize) -> Option<[u8; LENGTH]> {
    let end = offset.checked_add(LENGTH)?;
    let mut value = [0; LENGTH];
    value.copy_from_slice(encoded.get(*offset..end)?);
    *offset = end;
    Some(value)
}

const CANONICAL_F32_NAN_BITS: u32 = 0x7fc0_0000;
const CANONICAL_F64_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

/// Canonicalized IEEE-754 binary32 suitable for hashing and total ordering.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalF32(u32);

impl CanonicalF32 {
    /// Canonicalizes NaN payloads and signed zero.
    pub fn new(value: f32) -> Self {
        if value.is_nan() {
            Self(CANONICAL_F32_NAN_BITS)
        } else if value == 0.0 {
            Self(0)
        } else {
            Self(value.to_bits())
        }
    }

    /// Reconstructs the canonical floating-point value.
    pub const fn get(self) -> f32 {
        f32::from_bits(self.0)
    }

    /// Returns the canonical bit representation.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl Ord for CanonicalF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.get().total_cmp(&other.get())
    }
}

impl PartialOrd for CanonicalF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Canonicalized IEEE-754 binary64 suitable for hashing and total ordering.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalF64(u64);

impl CanonicalF64 {
    /// Canonicalizes NaN payloads and signed zero.
    pub fn new(value: f64) -> Self {
        if value.is_nan() {
            Self(CANONICAL_F64_NAN_BITS)
        } else if value == 0.0 {
            Self(0)
        } else {
            Self(value.to_bits())
        }
    }

    /// Reconstructs the canonical floating-point value.
    pub const fn get(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Returns the canonical bit representation.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

impl Ord for CanonicalF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.get().total_cmp(&other.get())
    }
}

impl PartialOrd for CanonicalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One canonical primitive scalar shared by row and ordered-index codecs.
///
/// SQL `NULL` is represented here so ordered indexes can encode it, but row
/// storage must place nullness in the containing row's null bitmap. Nested,
/// JSON, and vector values deliberately remain unsupported until their
/// canonical validators and resource bounds exist.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ScalarValue {
    /// SQL null.
    Null,
    /// Boolean.
    Boolean(bool),
    /// Signed integer, checked against the declared width.
    Signed(i64),
    /// Unsigned integer, checked against the declared width.
    Unsigned(u64),
    /// Fixed-scale decimal coefficient.
    Decimal(i128),
    /// Canonical binary32.
    Float32(CanonicalF32),
    /// Canonical binary64.
    Float64(CanonicalF64),
    /// Valid UTF-8 text.
    Text(String),
    /// Arbitrary bytes.
    Binary(Vec<u8>),
    /// Signed days from 1970-01-01.
    Date(i32),
    /// Nanoseconds from midnight.
    Time(u64),
    /// Signed microseconds from the Unix epoch.
    Timestamp(i64),
    /// Signed calendar months, days, and nanoseconds.
    Interval {
        /// Calendar-month component.
        months: i32,
        /// Calendar-day component.
        days: i32,
        /// Nanosecond component.
        nanoseconds: i64,
    },
    /// UUID bits in network byte order.
    Uuid([u8; 16]),
    /// Ordered homogeneous nested values.
    Array(Vec<Self>),
    /// Canonically key-ordered nested entries.
    Map(Vec<(Self, Self)>),
    /// Fixed-dimension canonical float32 vector.
    Vector(Vec<CanonicalF32>),
}

impl ScalarValue {
    /// Encodes one non-null primitive scalar for native row storage.
    ///
    /// Fixed-width numeric values use little-endian bytes. Text and binary
    /// values contain their bytes without a length prefix because the row
    /// field directory supplies the boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for SQL null, a value/type mismatch, an unsupported
    /// logical type, a noncanonical/out-of-domain value, or a value larger
    /// than 16 MiB.
    pub fn encode_storage(&self, logical_type: &LogicalType) -> Result<Vec<u8>, NativeTypeError> {
        if matches!(self, Self::Null) {
            return Err(NativeTypeError::NullRequiresRowBitmap);
        }
        if matches!(logical_type, LogicalType::Json) {
            return Err(NativeTypeError::UnsupportedScalarType);
        }
        let encoded = match (self, logical_type) {
            (Self::Boolean(value), LogicalType::Boolean) => vec![u8::from(*value)],
            (Self::Signed(value), LogicalType::Signed(width)) => {
                encode_signed_storage(*value, *width)?
            }
            (Self::Unsigned(value), LogicalType::Unsigned(width)) => {
                encode_unsigned_storage(*value, *width)?
            }
            (Self::Decimal(value), LogicalType::Decimal(decimal)) => {
                validate_decimal(*value, *decimal)?;
                value.to_le_bytes().to_vec()
            }
            (Self::Float32(value), LogicalType::Float32) => value.bits().to_le_bytes().to_vec(),
            (Self::Float64(value), LogicalType::Float64) => value.bits().to_le_bytes().to_vec(),
            (Self::Text(value), LogicalType::Text) => {
                ensure_scalar_length(value.len())?;
                value.as_bytes().to_vec()
            }
            (Self::Binary(value), LogicalType::Binary) => {
                ensure_scalar_length(value.len())?;
                value.clone()
            }
            (Self::Date(value), LogicalType::Date) => value.to_le_bytes().to_vec(),
            (Self::Time(value), LogicalType::Time) => {
                validate_time(*value)?;
                value.to_le_bytes().to_vec()
            }
            (Self::Timestamp(value), LogicalType::Timestamp) => value.to_le_bytes().to_vec(),
            (
                Self::Interval {
                    months,
                    days,
                    nanoseconds,
                },
                LogicalType::Interval,
            ) => {
                let mut encoded = Vec::with_capacity(16);
                encoded.extend_from_slice(&months.to_le_bytes());
                encoded.extend_from_slice(&days.to_le_bytes());
                encoded.extend_from_slice(&nanoseconds.to_le_bytes());
                encoded
            }
            (Self::Uuid(value), LogicalType::Uuid) => value.to_vec(),
            (Self::Array(values), LogicalType::Array(element_type)) => {
                encode_array_storage(values, element_type)?
            }
            (Self::Map(entries), LogicalType::Map(key_type, value_type)) => {
                encode_map_storage(entries, key_type, value_type)?
            }
            (Self::Vector(values), LogicalType::Vector(vector_type)) => {
                encode_vector_storage(values, *vector_type)?
            }
            _ => return Err(NativeTypeError::ScalarTypeMismatch),
        };
        ensure_scalar_length(encoded.len())?;
        Ok(encoded)
    }

    /// Decodes one complete non-null primitive scalar from native row bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported logical type, malformed or
    /// noncanonical bytes, an out-of-domain value, or trailing bytes on a
    /// fixed-width value.
    pub fn decode_storage(
        logical_type: &LogicalType,
        encoded: &[u8],
    ) -> Result<Self, NativeTypeError> {
        ensure_scalar_length(encoded.len())?;
        match logical_type {
            LogicalType::Boolean => match encoded {
                [0] => Ok(Self::Boolean(false)),
                [1] => Ok(Self::Boolean(true)),
                _ => Err(NativeTypeError::InvalidScalarEncoding),
            },
            LogicalType::Signed(width) => decode_signed_storage(encoded, *width).map(Self::Signed),
            LogicalType::Unsigned(width) => {
                decode_unsigned_storage(encoded, *width).map(Self::Unsigned)
            }
            LogicalType::Decimal(decimal) => {
                let value = i128::from_le_bytes(exact_scalar_bytes(encoded)?);
                validate_decimal(value, *decimal)?;
                Ok(Self::Decimal(value))
            }
            LogicalType::Float32 => {
                let bits = u32::from_le_bytes(exact_scalar_bytes(encoded)?);
                decode_canonical_f32(bits).map(Self::Float32)
            }
            LogicalType::Float64 => {
                let bits = u64::from_le_bytes(exact_scalar_bytes(encoded)?);
                decode_canonical_f64(bits).map(Self::Float64)
            }
            LogicalType::Text => String::from_utf8(encoded.to_vec())
                .map(Self::Text)
                .map_err(|_| NativeTypeError::InvalidScalarEncoding),
            LogicalType::Binary => Ok(Self::Binary(encoded.to_vec())),
            LogicalType::Date => Ok(Self::Date(i32::from_le_bytes(exact_scalar_bytes(encoded)?))),
            LogicalType::Time => {
                let value = u64::from_le_bytes(exact_scalar_bytes(encoded)?);
                validate_time(value)?;
                Ok(Self::Time(value))
            }
            LogicalType::Timestamp => Ok(Self::Timestamp(i64::from_le_bytes(exact_scalar_bytes(
                encoded,
            )?))),
            LogicalType::Interval => {
                if encoded.len() != 16 {
                    return Err(NativeTypeError::InvalidScalarEncoding);
                }
                let months = i32::from_le_bytes(
                    encoded[0..4]
                        .try_into()
                        .map_err(|_| NativeTypeError::InvalidScalarEncoding)?,
                );
                let days = i32::from_le_bytes(
                    encoded[4..8]
                        .try_into()
                        .map_err(|_| NativeTypeError::InvalidScalarEncoding)?,
                );
                let nanoseconds = i64::from_le_bytes(
                    encoded[8..16]
                        .try_into()
                        .map_err(|_| NativeTypeError::InvalidScalarEncoding)?,
                );
                Ok(Self::Interval {
                    months,
                    days,
                    nanoseconds,
                })
            }
            LogicalType::Uuid => Ok(Self::Uuid(exact_scalar_bytes(encoded)?)),
            LogicalType::Array(element_type) => {
                decode_array_storage(encoded, element_type).map(Self::Array)
            }
            LogicalType::Map(key_type, value_type) => {
                decode_map_storage(encoded, key_type, value_type).map(Self::Map)
            }
            LogicalType::Vector(vector_type) => {
                decode_vector_storage(encoded, *vector_type).map(Self::Vector)
            }
            LogicalType::Json => Err(NativeTypeError::UnsupportedScalarType),
        }
    }

    /// Encodes one scalar as a self-delimiting memcomparable index component.
    ///
    /// The first byte sorts SQL null before non-null values. The remaining
    /// bytes preserve the declared type's total order under ordinary unsigned
    /// byte comparison.
    ///
    /// # Errors
    ///
    /// Returns an error for a value/type mismatch, unsupported ordered type,
    /// out-of-domain value, or overlong scalar.
    pub fn encode_ordered_component(
        &self,
        logical_type: &LogicalType,
    ) -> Result<Vec<u8>, NativeTypeError> {
        if matches!(self, Self::Null) {
            return Ok(vec![ORDERED_NULL]);
        }
        let payload = match (self, logical_type) {
            (Self::Boolean(value), LogicalType::Boolean) => vec![u8::from(*value)],
            (Self::Signed(value), LogicalType::Signed(width)) => {
                encode_signed_ordered(*value, *width)?
            }
            (Self::Unsigned(value), LogicalType::Unsigned(width)) => {
                encode_unsigned_ordered(*value, *width)?
            }
            (Self::Decimal(value), LogicalType::Decimal(decimal)) => {
                validate_decimal(*value, *decimal)?;
                encode_i128_ordered(*value).to_vec()
            }
            (Self::Float32(value), LogicalType::Float32) => {
                sortable_f32_bits(value.bits()).to_be_bytes().to_vec()
            }
            (Self::Float64(value), LogicalType::Float64) => {
                sortable_f64_bits(value.bits()).to_be_bytes().to_vec()
            }
            (Self::Text(value), LogicalType::Text) => {
                ensure_scalar_length(value.len())?;
                encode_memcomparable_bytes(value.as_bytes())?
            }
            (Self::Binary(value), LogicalType::Binary) => {
                ensure_scalar_length(value.len())?;
                encode_memcomparable_bytes(value)?
            }
            (Self::Date(value), LogicalType::Date) => encode_i32_ordered(*value).to_vec(),
            (Self::Time(value), LogicalType::Time) => {
                validate_time(*value)?;
                value.to_be_bytes().to_vec()
            }
            (Self::Timestamp(value), LogicalType::Timestamp) => encode_i64_ordered(*value).to_vec(),
            (
                Self::Interval {
                    months,
                    days,
                    nanoseconds,
                },
                LogicalType::Interval,
            ) => {
                let mut encoded = Vec::with_capacity(16);
                encoded.extend_from_slice(&encode_i32_ordered(*months));
                encoded.extend_from_slice(&encode_i32_ordered(*days));
                encoded.extend_from_slice(&encode_i64_ordered(*nanoseconds));
                encoded
            }
            (Self::Uuid(value), LogicalType::Uuid) => value.to_vec(),
            (Self::Array(values), LogicalType::Array(element_type)) => {
                encode_array_ordered(values, element_type)?
            }
            (_, LogicalType::Json | LogicalType::Map(_, _) | LogicalType::Vector(_)) => {
                return Err(NativeTypeError::UnsupportedOrderedType);
            }
            _ => return Err(NativeTypeError::ScalarTypeMismatch),
        };
        let output_length = payload
            .len()
            .checked_add(1)
            .ok_or(NativeTypeError::ScalarLengthExceeded)?;
        if output_length > MAX_ORDERED_SCALAR_BYTES {
            return Err(NativeTypeError::ScalarLengthExceeded);
        }
        let mut encoded = Vec::with_capacity(output_length);
        encoded.push(ORDERED_VALUE);
        encoded.extend_from_slice(&payload);
        Ok(encoded)
    }

    /// Decodes one complete memcomparable index component.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, noncanonical, out-of-domain, trailing,
    /// or unsupported bytes.
    pub fn decode_ordered_component(
        logical_type: &LogicalType,
        encoded: &[u8],
    ) -> Result<Self, NativeTypeError> {
        if encoded.is_empty() {
            return Err(NativeTypeError::InvalidScalarEncoding);
        }
        if encoded.len() > MAX_ORDERED_SCALAR_BYTES {
            return Err(NativeTypeError::ScalarLengthExceeded);
        }
        let (marker, payload) = encoded
            .split_first()
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        match *marker {
            ORDERED_NULL if payload.is_empty() => return Ok(Self::Null),
            ORDERED_VALUE => {}
            _ => return Err(NativeTypeError::InvalidScalarEncoding),
        }
        match logical_type {
            LogicalType::Boolean => match payload {
                [0] => Ok(Self::Boolean(false)),
                [1] => Ok(Self::Boolean(true)),
                _ => Err(NativeTypeError::InvalidScalarEncoding),
            },
            LogicalType::Signed(width) => decode_signed_ordered(payload, *width).map(Self::Signed),
            LogicalType::Unsigned(width) => {
                decode_unsigned_ordered(payload, *width).map(Self::Unsigned)
            }
            LogicalType::Decimal(decimal) => {
                let mut bytes = exact_scalar_bytes::<16>(payload)?;
                bytes[0] ^= 0x80;
                let value = i128::from_be_bytes(bytes);
                validate_decimal(value, *decimal)?;
                Ok(Self::Decimal(value))
            }
            LogicalType::Float32 => {
                let sortable = u32::from_be_bytes(exact_scalar_bytes(payload)?);
                decode_canonical_f32(unsortable_f32_bits(sortable)).map(Self::Float32)
            }
            LogicalType::Float64 => {
                let sortable = u64::from_be_bytes(exact_scalar_bytes(payload)?);
                decode_canonical_f64(unsortable_f64_bits(sortable)).map(Self::Float64)
            }
            LogicalType::Text => {
                let decoded = decode_memcomparable_bytes(payload)?;
                String::from_utf8(decoded)
                    .map(Self::Text)
                    .map_err(|_| NativeTypeError::InvalidScalarEncoding)
            }
            LogicalType::Binary => decode_memcomparable_bytes(payload).map(Self::Binary),
            LogicalType::Date => {
                let mut bytes = exact_scalar_bytes::<4>(payload)?;
                bytes[0] ^= 0x80;
                Ok(Self::Date(i32::from_be_bytes(bytes)))
            }
            LogicalType::Time => {
                let value = u64::from_be_bytes(exact_scalar_bytes(payload)?);
                validate_time(value)?;
                Ok(Self::Time(value))
            }
            LogicalType::Timestamp => {
                let mut bytes = exact_scalar_bytes::<8>(payload)?;
                bytes[0] ^= 0x80;
                Ok(Self::Timestamp(i64::from_be_bytes(bytes)))
            }
            LogicalType::Interval => {
                if payload.len() != 16 {
                    return Err(NativeTypeError::InvalidScalarEncoding);
                }
                let mut month_bytes: [u8; 4] = payload[0..4]
                    .try_into()
                    .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
                month_bytes[0] ^= 0x80;
                let mut day_bytes: [u8; 4] = payload[4..8]
                    .try_into()
                    .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
                day_bytes[0] ^= 0x80;
                let mut nanos_bytes: [u8; 8] = payload[8..16]
                    .try_into()
                    .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
                nanos_bytes[0] ^= 0x80;
                Ok(Self::Interval {
                    months: i32::from_be_bytes(month_bytes),
                    days: i32::from_be_bytes(day_bytes),
                    nanoseconds: i64::from_be_bytes(nanos_bytes),
                })
            }
            LogicalType::Uuid => Ok(Self::Uuid(exact_scalar_bytes(payload)?)),
            LogicalType::Array(element_type) => {
                decode_array_ordered(payload, element_type).map(Self::Array)
            }
            LogicalType::Json | LogicalType::Map(_, _) | LogicalType::Vector(_) => {
                Err(NativeTypeError::UnsupportedOrderedType)
            }
        }
    }
}

fn encode_array_ordered(
    values: &[ScalarValue],
    element_type: &LogicalType,
) -> Result<Vec<u8>, NativeTypeError> {
    let mut encoded = Vec::new();
    for value in values {
        let component = value.encode_ordered_component(element_type)?;
        encoded.extend_from_slice(&encode_memcomparable_bytes(&component)?);
    }
    encoded.extend_from_slice(&[0, 0]);
    ensure_scalar_length(encoded.len())?;
    Ok(encoded)
}

fn decode_array_ordered(
    encoded: &[u8],
    element_type: &LogicalType,
) -> Result<Vec<ScalarValue>, NativeTypeError> {
    let mut offset = 0_usize;
    let mut values = Vec::new();
    loop {
        let remaining = encoded
            .get(offset..)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        if remaining == [0, 0] {
            return Ok(values);
        }
        if remaining.starts_with(&[0, 0]) {
            return Err(NativeTypeError::InvalidScalarEncoding);
        }
        if values.len() >= 100_000 {
            return Err(NativeTypeError::ScalarLengthExceeded);
        }
        let end = memcomparable_component_end(remaining)?;
        let component = decode_memcomparable_bytes(&remaining[..end])?;
        values.push(ScalarValue::decode_ordered_component(
            element_type,
            &component,
        )?);
        offset = offset
            .checked_add(end)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
    }
}

fn memcomparable_component_end(encoded: &[u8]) -> Result<usize, NativeTypeError> {
    let mut offset = 0_usize;
    while offset < encoded.len() {
        if encoded[offset] != 0 {
            offset += 1;
            continue;
        }
        let escape = *encoded
            .get(offset + 1)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        match escape {
            0 => return Ok(offset + 2),
            0xff => offset += 2,
            _ => return Err(NativeTypeError::InvalidScalarEncoding),
        }
    }
    Err(NativeTypeError::InvalidScalarEncoding)
}

fn encode_vector_storage(
    values: &[CanonicalF32],
    vector_type: VectorType,
) -> Result<Vec<u8>, NativeTypeError> {
    if vector_type.element() != VectorElement::Float32
        || values.len() != usize::from(vector_type.dimension())
    {
        return Err(NativeTypeError::ScalarOutOfRange);
    }
    let mut encoded = Vec::with_capacity(values.len().saturating_mul(4));
    for value in values {
        encoded.extend_from_slice(&value.bits().to_le_bytes());
    }
    ensure_scalar_length(encoded.len())?;
    Ok(encoded)
}

fn decode_vector_storage(
    encoded: &[u8],
    vector_type: VectorType,
) -> Result<Vec<CanonicalF32>, NativeTypeError> {
    if vector_type.element() != VectorElement::Float32
        || encoded.len() != usize::from(vector_type.dimension()).saturating_mul(4)
    {
        return Err(NativeTypeError::InvalidScalarEncoding);
    }
    encoded
        .chunks_exact(4)
        .map(|bytes| {
            let bits = u32::from_le_bytes(exact_scalar_bytes(bytes)?);
            decode_canonical_f32(bits)
        })
        .collect()
}

fn encode_map_storage(
    entries: &[(ScalarValue, ScalarValue)],
    key_type: &LogicalType,
    value_type: &LogicalType,
) -> Result<Vec<u8>, NativeTypeError> {
    let count = u32::try_from(entries.len()).map_err(|_| NativeTypeError::ScalarLengthExceeded)?;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&count.to_le_bytes());
    let mut previous_key: Option<Vec<u8>> = None;
    for (key, value) in entries {
        if matches!(key, ScalarValue::Null) {
            return Err(NativeTypeError::InvalidScalarEncoding);
        }
        let key_payload = key.encode_storage(key_type)?;
        let key_ordered = key.encode_ordered_component(key_type)?;
        if previous_key
            .as_ref()
            .is_some_and(|previous| previous >= &key_ordered)
        {
            return Err(NativeTypeError::InvalidScalarEncoding);
        }
        previous_key = Some(key_ordered);
        put_nested_payload(&mut encoded, &key_payload)?;
        if matches!(value, ScalarValue::Null) {
            encoded.push(0);
        } else {
            encoded.push(1);
            let value_payload = value.encode_storage(value_type)?;
            put_nested_payload(&mut encoded, &value_payload)?;
        }
        ensure_scalar_length(encoded.len())?;
    }
    Ok(encoded)
}

fn put_nested_payload(encoded: &mut Vec<u8>, payload: &[u8]) -> Result<(), NativeTypeError> {
    let length = u32::try_from(payload.len()).map_err(|_| NativeTypeError::ScalarLengthExceeded)?;
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(payload);
    Ok(())
}

fn take_nested_payload<'a>(
    encoded: &'a [u8],
    offset: &mut usize,
) -> Result<&'a [u8], NativeTypeError> {
    let length_end = offset
        .checked_add(4)
        .ok_or(NativeTypeError::InvalidScalarEncoding)?;
    let length = usize::try_from(u32::from_le_bytes(exact_scalar_bytes(
        encoded
            .get(*offset..length_end)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?,
    )?))
    .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
    *offset = length_end;
    let end = offset
        .checked_add(length)
        .ok_or(NativeTypeError::InvalidScalarEncoding)?;
    let payload = encoded
        .get(*offset..end)
        .ok_or(NativeTypeError::InvalidScalarEncoding)?;
    *offset = end;
    Ok(payload)
}

fn decode_map_storage(
    encoded: &[u8],
    key_type: &LogicalType,
    value_type: &LogicalType,
) -> Result<Vec<(ScalarValue, ScalarValue)>, NativeTypeError> {
    let count = encoded
        .get(..4)
        .ok_or(NativeTypeError::InvalidScalarEncoding)
        .and_then(|bytes| exact_scalar_bytes(bytes).map(u32::from_le_bytes))?;
    let count = usize::try_from(count).map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
    if count > 100_000 {
        return Err(NativeTypeError::ScalarLengthExceeded);
    }
    let mut offset = 4_usize;
    let mut previous_key: Option<Vec<u8>> = None;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key =
            ScalarValue::decode_storage(key_type, take_nested_payload(encoded, &mut offset)?)?;
        let key_ordered = key.encode_ordered_component(key_type)?;
        if matches!(key, ScalarValue::Null)
            || previous_key
                .as_ref()
                .is_some_and(|previous| previous >= &key_ordered)
        {
            return Err(NativeTypeError::InvalidScalarEncoding);
        }
        previous_key = Some(key_ordered);
        let marker = *encoded
            .get(offset)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        offset += 1;
        let value = match marker {
            0 => ScalarValue::Null,
            1 => {
                ScalarValue::decode_storage(value_type, take_nested_payload(encoded, &mut offset)?)?
            }
            _ => return Err(NativeTypeError::InvalidScalarEncoding),
        };
        entries.push((key, value));
    }
    if offset != encoded.len() {
        return Err(NativeTypeError::InvalidScalarEncoding);
    }
    Ok(entries)
}

fn encode_array_storage(
    values: &[ScalarValue],
    element_type: &LogicalType,
) -> Result<Vec<u8>, NativeTypeError> {
    let count = u32::try_from(values.len()).map_err(|_| NativeTypeError::ScalarLengthExceeded)?;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&count.to_le_bytes());
    for value in values {
        if matches!(value, ScalarValue::Null) {
            encoded.push(0);
            continue;
        }
        let payload = value.encode_storage(element_type)?;
        let length =
            u32::try_from(payload.len()).map_err(|_| NativeTypeError::ScalarLengthExceeded)?;
        encoded.push(1);
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.extend_from_slice(&payload);
        ensure_scalar_length(encoded.len())?;
    }
    Ok(encoded)
}

fn decode_array_storage(
    encoded: &[u8],
    element_type: &LogicalType,
) -> Result<Vec<ScalarValue>, NativeTypeError> {
    let count_bytes = encoded
        .get(..4)
        .ok_or(NativeTypeError::InvalidScalarEncoding)?;
    let count = usize::try_from(u32::from_le_bytes(exact_scalar_bytes(count_bytes)?))
        .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
    if count > 100_000 {
        return Err(NativeTypeError::ScalarLengthExceeded);
    }
    let mut offset = 4_usize;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let marker = *encoded
            .get(offset)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        offset += 1;
        match marker {
            0 => values.push(ScalarValue::Null),
            1 => {
                let length_end = offset
                    .checked_add(4)
                    .ok_or(NativeTypeError::InvalidScalarEncoding)?;
                let length = usize::try_from(u32::from_le_bytes(exact_scalar_bytes(
                    encoded
                        .get(offset..length_end)
                        .ok_or(NativeTypeError::InvalidScalarEncoding)?,
                )?))
                .map_err(|_| NativeTypeError::InvalidScalarEncoding)?;
                offset = length_end;
                let end = offset
                    .checked_add(length)
                    .ok_or(NativeTypeError::InvalidScalarEncoding)?;
                let payload = encoded
                    .get(offset..end)
                    .ok_or(NativeTypeError::InvalidScalarEncoding)?;
                values.push(ScalarValue::decode_storage(element_type, payload)?);
                offset = end;
            }
            _ => return Err(NativeTypeError::InvalidScalarEncoding),
        }
    }
    if offset != encoded.len() {
        return Err(NativeTypeError::InvalidScalarEncoding);
    }
    Ok(values)
}

fn ensure_scalar_length(length: usize) -> Result<(), NativeTypeError> {
    if length > MAX_SCALAR_BYTES {
        return Err(NativeTypeError::ScalarLengthExceeded);
    }
    Ok(())
}

fn exact_scalar_bytes<const LENGTH: usize>(
    encoded: &[u8],
) -> Result<[u8; LENGTH], NativeTypeError> {
    encoded
        .try_into()
        .map_err(|_| NativeTypeError::InvalidScalarEncoding)
}

fn validate_decimal(value: i128, decimal: DecimalType) -> Result<(), NativeTypeError> {
    let maximum = 10_u128.pow(u32::from(decimal.precision())) - 1;
    if value.unsigned_abs() > maximum {
        return Err(NativeTypeError::ScalarOutOfRange);
    }
    Ok(())
}

fn validate_time(value: u64) -> Result<(), NativeTypeError> {
    if value >= NANOS_PER_DAY {
        return Err(NativeTypeError::ScalarOutOfRange);
    }
    Ok(())
}

fn decode_canonical_f32(bits: u32) -> Result<CanonicalF32, NativeTypeError> {
    let value = CanonicalF32::new(f32::from_bits(bits));
    if value.bits() != bits {
        return Err(NativeTypeError::InvalidScalarEncoding);
    }
    Ok(value)
}

fn decode_canonical_f64(bits: u64) -> Result<CanonicalF64, NativeTypeError> {
    let value = CanonicalF64::new(f64::from_bits(bits));
    if value.bits() != bits {
        return Err(NativeTypeError::InvalidScalarEncoding);
    }
    Ok(value)
}

fn encode_signed_storage(value: i64, width: IntegerWidth) -> Result<Vec<u8>, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => i8::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits16 => i16::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits32 => i32::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits64 => Ok(value.to_le_bytes().to_vec()),
    }
}

fn decode_signed_storage(encoded: &[u8], width: IntegerWidth) -> Result<i64, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => Ok(i64::from(i8::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits16 => Ok(i64::from(i16::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits32 => Ok(i64::from(i32::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits64 => Ok(i64::from_le_bytes(exact_scalar_bytes(encoded)?)),
    }
}

fn encode_unsigned_storage(value: u64, width: IntegerWidth) -> Result<Vec<u8>, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => u8::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits16 => u16::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits32 => u32::try_from(value)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits64 => Ok(value.to_le_bytes().to_vec()),
    }
}

fn decode_unsigned_storage(encoded: &[u8], width: IntegerWidth) -> Result<u64, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => Ok(u64::from(u8::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits16 => Ok(u64::from(u16::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits32 => Ok(u64::from(u32::from_le_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits64 => Ok(u64::from_le_bytes(exact_scalar_bytes(encoded)?)),
    }
}

fn encode_signed_ordered(value: i64, width: IntegerWidth) -> Result<Vec<u8>, NativeTypeError> {
    let mut bytes = match width {
        IntegerWidth::Bits8 => i8::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange)?,
        IntegerWidth::Bits16 => i16::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange)?,
        IntegerWidth::Bits32 => i32::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange)?,
        IntegerWidth::Bits64 => value.to_be_bytes().to_vec(),
    };
    bytes[0] ^= 0x80;
    Ok(bytes)
}

fn decode_signed_ordered(encoded: &[u8], width: IntegerWidth) -> Result<i64, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => {
            let mut bytes = exact_scalar_bytes::<1>(encoded)?;
            bytes[0] ^= 0x80;
            Ok(i64::from(i8::from_be_bytes(bytes)))
        }
        IntegerWidth::Bits16 => {
            let mut bytes = exact_scalar_bytes::<2>(encoded)?;
            bytes[0] ^= 0x80;
            Ok(i64::from(i16::from_be_bytes(bytes)))
        }
        IntegerWidth::Bits32 => {
            let mut bytes = exact_scalar_bytes::<4>(encoded)?;
            bytes[0] ^= 0x80;
            Ok(i64::from(i32::from_be_bytes(bytes)))
        }
        IntegerWidth::Bits64 => {
            let mut bytes = exact_scalar_bytes::<8>(encoded)?;
            bytes[0] ^= 0x80;
            Ok(i64::from_be_bytes(bytes))
        }
    }
}

fn encode_unsigned_ordered(value: u64, width: IntegerWidth) -> Result<Vec<u8>, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => u8::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits16 => u16::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits32 => u32::try_from(value)
            .map(|value| value.to_be_bytes().to_vec())
            .map_err(|_| NativeTypeError::ScalarOutOfRange),
        IntegerWidth::Bits64 => Ok(value.to_be_bytes().to_vec()),
    }
}

fn decode_unsigned_ordered(encoded: &[u8], width: IntegerWidth) -> Result<u64, NativeTypeError> {
    match width {
        IntegerWidth::Bits8 => Ok(u64::from(u8::from_be_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits16 => Ok(u64::from(u16::from_be_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits32 => Ok(u64::from(u32::from_be_bytes(exact_scalar_bytes(encoded)?))),
        IntegerWidth::Bits64 => Ok(u64::from_be_bytes(exact_scalar_bytes(encoded)?)),
    }
}

fn encode_i32_ordered(value: i32) -> [u8; 4] {
    let mut bytes = value.to_be_bytes();
    bytes[0] ^= 0x80;
    bytes
}

fn encode_i64_ordered(value: i64) -> [u8; 8] {
    let mut bytes = value.to_be_bytes();
    bytes[0] ^= 0x80;
    bytes
}

fn encode_i128_ordered(value: i128) -> [u8; 16] {
    let mut bytes = value.to_be_bytes();
    bytes[0] ^= 0x80;
    bytes
}

fn sortable_f32_bits(bits: u32) -> u32 {
    if bits & (1 << 31) == 0 {
        bits ^ (1 << 31)
    } else {
        !bits
    }
}

fn unsortable_f32_bits(bits: u32) -> u32 {
    if bits & (1 << 31) == 0 {
        !bits
    } else {
        bits ^ (1 << 31)
    }
}

fn sortable_f64_bits(bits: u64) -> u64 {
    if bits & (1 << 63) == 0 {
        bits ^ (1 << 63)
    } else {
        !bits
    }
}

fn unsortable_f64_bits(bits: u64) -> u64 {
    if bits & (1 << 63) == 0 {
        !bits
    } else {
        bits ^ (1 << 63)
    }
}

fn encode_memcomparable_bytes(value: &[u8]) -> Result<Vec<u8>, NativeTypeError> {
    let encoded_length = value.iter().try_fold(2_usize, |length, byte| {
        length.checked_add(if *byte == 0 { 2 } else { 1 })
    });
    let encoded_length = encoded_length.ok_or(NativeTypeError::ScalarLengthExceeded)?;
    if encoded_length >= MAX_ORDERED_SCALAR_BYTES {
        return Err(NativeTypeError::ScalarLengthExceeded);
    }
    let mut encoded = Vec::with_capacity(encoded_length);
    for byte in value {
        if *byte == 0 {
            encoded.extend_from_slice(&[0, 0xff]);
        } else {
            encoded.push(*byte);
        }
    }
    encoded.extend_from_slice(&[0, 0]);
    Ok(encoded)
}

fn decode_memcomparable_bytes(encoded: &[u8]) -> Result<Vec<u8>, NativeTypeError> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut offset = 0;
    while offset < encoded.len() {
        let byte = encoded[offset];
        offset += 1;
        if byte != 0 {
            decoded.push(byte);
            continue;
        }
        let escape = *encoded
            .get(offset)
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        offset += 1;
        match escape {
            0 if offset == encoded.len() => {
                ensure_scalar_length(decoded.len())?;
                return Ok(decoded);
            }
            0xff => decoded.push(0),
            _ => return Err(NativeTypeError::InvalidScalarEncoding),
        }
        if decoded.len() > MAX_SCALAR_BYTES {
            return Err(NativeTypeError::ScalarLengthExceeded);
        }
    }
    Err(NativeTypeError::InvalidScalarEncoding)
}

/// One frozen primitive scalar fixture consumed by native storage layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimitiveScalarGolden {
    /// Declared logical type.
    pub logical_type: LogicalType,
    /// Canonical logical value.
    pub value: ScalarValue,
    /// Canonical row/storage payload.
    pub storage: Vec<u8>,
    /// Canonical ordered-index component including its non-null marker.
    pub ordered: Vec<u8>,
}

/// Returns the frozen cross-crate primitive scalar corpus.
///
/// # Errors
///
/// Returns an error only if an internal fixture declaration violates a checked
/// logical-type bound.
pub fn primitive_scalar_golden_fixtures() -> Result<Vec<PrimitiveScalarGolden>, NativeTypeError> {
    let decimal = DecimalType::new(6, 2)?;
    let declarations = [
        (LogicalType::Boolean, ScalarValue::Boolean(true)),
        (
            LogicalType::Signed(IntegerWidth::Bits16),
            ScalarValue::Signed(-2),
        ),
        (
            LogicalType::Unsigned(IntegerWidth::Bits16),
            ScalarValue::Unsigned(0x1234),
        ),
        (LogicalType::Decimal(decimal), ScalarValue::Decimal(-12_345)),
        (
            LogicalType::Float32,
            ScalarValue::Float32(CanonicalF32::new(-1.5)),
        ),
        (
            LogicalType::Float64,
            ScalarValue::Float64(CanonicalF64::new(f64::NAN)),
        ),
        (LogicalType::Text, ScalarValue::Text("A\0B".to_owned())),
        (LogicalType::Binary, ScalarValue::Binary(vec![0, 0xff])),
        (LogicalType::Date, ScalarValue::Date(-1)),
        (LogicalType::Time, ScalarValue::Time(1)),
        (LogicalType::Timestamp, ScalarValue::Timestamp(-2)),
        (
            LogicalType::Interval,
            ScalarValue::Interval {
                months: -1,
                days: 2,
                nanoseconds: -3,
            },
        ),
        (LogicalType::Uuid, ScalarValue::Uuid([0x11; 16])),
    ];
    declarations
        .into_iter()
        .map(|(logical_type, value)| {
            Ok(PrimitiveScalarGolden {
                storage: value.encode_storage(&logical_type)?,
                ordered: value.encode_ordered_component(&logical_type)?,
                logical_type,
                value,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalF32, CanonicalF64, CatalogVersion, Csn, DecimalType, DirectoryUuid, HistoryEpoch,
        IntegerWidth, LineageIdentity, LogicalType, MAX_SCALAR_BYTES, NativeTypeError, ObjectId,
        ScalarValue, VectorElement, VectorType, primitive_scalar_golden_fixtures,
    };
    use proptest::prelude::*;

    #[test]
    fn lineage_identity_has_canonical_text_and_binary_forms() -> Result<(), NativeTypeError> {
        let directory_uuid =
            DirectoryUuid::parse_canonical("018f4e9d-3d7a-7b6c-8f12-123456789abc")?;
        let lineage = LineageIdentity::new(directory_uuid, HistoryEpoch::new(42)?);
        let encoded = lineage.encode();

        assert_eq!(
            directory_uuid.to_string(),
            "018f4e9d-3d7a-7b6c-8f12-123456789abc"
        );
        assert_eq!(
            encoded,
            [
                0x01, 0x8f, 0x4e, 0x9d, 0x3d, 0x7a, 0x7b, 0x6c, 0x8f, 0x12, 0x12, 0x34, 0x56, 0x78,
                0x9a, 0xbc, 0x2a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );
        assert_eq!(LineageIdentity::decode(&encoded)?, lineage);
        Ok(())
    }

    #[test]
    fn lineage_identity_rejects_noncanonical_or_invalid_values() {
        for value in [
            "018f4e9d-3d7a-6b6c-8f12-123456789abc",
            "018f4e9d-3d7a-7b6c-7f12-123456789abc",
            "018F4E9D-3D7A-7B6C-8F12-123456789ABC",
            "018f4e9d3d7a7b6c8f12123456789abc",
        ] {
            assert_eq!(
                DirectoryUuid::parse_canonical(value),
                Err(NativeTypeError::InvalidDirectoryUuid)
            );
        }
        let mut zero_epoch = [
            0x01, 0x8f, 0x4e, 0x9d, 0x3d, 0x7a, 0x7b, 0x6c, 0x8f, 0x12, 0x12, 0x34, 0x56, 0x78,
            0x9a, 0xbc, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            LineageIdentity::decode(&zero_epoch),
            Err(NativeTypeError::InvalidLineageEncoding)
        );
        zero_epoch[6] = 0x6b;
        assert_eq!(
            LineageIdentity::decode(&zero_epoch),
            Err(NativeTypeError::InvalidLineageEncoding)
        );
    }

    #[test]
    fn stable_identities_reject_zero() {
        assert_eq!(
            ObjectId::new(0),
            Err(NativeTypeError::ZeroIdentity("object ID"))
        );
        assert_eq!(ObjectId::new(7).map(ObjectId::get), Ok(7));
    }

    #[test]
    fn sequence_numbers_advance_without_wrapping() -> Result<(), NativeTypeError> {
        assert_eq!(Csn::new(1)?.checked_next().map(Csn::get), Some(2));
        assert_eq!(
            CatalogVersion::new(u64::MAX)?
                .checked_next()
                .map(CatalogVersion::get),
            None
        );
        Ok(())
    }

    #[test]
    fn decimal_declarations_are_bounded() {
        assert_eq!(
            DecimalType::new(0, 0),
            Err(NativeTypeError::InvalidDecimalPrecision)
        );
        assert_eq!(
            DecimalType::new(5, 6),
            Err(NativeTypeError::InvalidDecimalScale)
        );
        assert_eq!(DecimalType::new(38, 18).map(DecimalType::scale), Ok(18));
    }

    proptest! {
        #[test]
        fn unsigned_storage_and_ordered_codecs_round_trip(value in any::<u64>()) {
            let logical_type = LogicalType::Unsigned(IntegerWidth::Bits64);
            let scalar = ScalarValue::Unsigned(value);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_storage(&logical_type, &storage)?,
                scalar.clone()
            );
            let ordered = scalar.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_ordered_component(&logical_type, &ordered)?,
                scalar
            );
        }

        #[test]
        fn unsigned_ordered_bytes_preserve_value_order(left in any::<u64>(), right in any::<u64>()) {
            let logical_type = LogicalType::Unsigned(IntegerWidth::Bits64);
            let left_bytes = ScalarValue::Unsigned(left).encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Unsigned(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
        }

        #[test]
        fn signed_storage_and_ordered_codecs_round_trip(value in any::<i64>()) {
            let logical_type = LogicalType::Signed(IntegerWidth::Bits64);
            let scalar = ScalarValue::Signed(value);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_storage(&logical_type, &storage)?,
                scalar.clone()
            );
            let ordered = scalar.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_ordered_component(&logical_type, &ordered)?,
                scalar
            );
        }

        #[test]
        fn signed_ordered_bytes_preserve_value_order(left in any::<i64>(), right in any::<i64>()) {
            let logical_type = LogicalType::Signed(IntegerWidth::Bits64);
            let left_bytes = ScalarValue::Signed(left).encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Signed(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
        }

        #[test]
        fn text_storage_and_ordered_codecs_round_trip(value in ".{0,128}") {
            let logical_type = LogicalType::Text;
            let scalar = ScalarValue::Text(value);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_storage(&logical_type, &storage)?,
                scalar.clone()
            );
            let ordered = scalar.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_ordered_component(&logical_type, &ordered)?,
                scalar
            );
        }

        #[test]
        fn text_ordered_bytes_preserve_value_order(left in ".{0,64}", right in ".{0,64}") {
            let logical_type = LogicalType::Text;
            let left_bytes = ScalarValue::Text(left.clone()).encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Text(right.clone()).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.as_bytes().cmp(right.as_bytes()), left_bytes.cmp(&right_bytes));
        }

        #[test]
        fn decimal_storage_and_ordered_codecs_round_trip(value in -999_999_999_999_i128..=999_999_999_999_i128) {
            let logical_type = LogicalType::Decimal(DecimalType::new(12, 4)?);
            let scalar = ScalarValue::Decimal(value);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, scalar.clone());
            let ordered = scalar.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &ordered)?, scalar);
        }

        #[test]
        fn decimal_ordered_bytes_preserve_coefficient_order(
            left in -999_999_999_999_i128..=999_999_999_999_i128,
            right in -999_999_999_999_i128..=999_999_999_999_i128,
        ) {
            let logical_type = LogicalType::Decimal(DecimalType::new(12, 4)?);
            let left_bytes = ScalarValue::Decimal(left).encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Decimal(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
        }

        #[test]
        fn date_and_timestamp_codecs_round_trip_and_preserve_order(
            left_date in any::<i32>(), right_date in any::<i32>(),
            left_timestamp in any::<i64>(), right_timestamp in any::<i64>(),
        ) {
            for (logical_type, left, right, value_order) in [
                (
                    LogicalType::Date,
                    ScalarValue::Date(left_date),
                    ScalarValue::Date(right_date),
                    left_date.cmp(&right_date),
                ),
                (
                    LogicalType::Timestamp,
                    ScalarValue::Timestamp(left_timestamp),
                    ScalarValue::Timestamp(right_timestamp),
                    left_timestamp.cmp(&right_timestamp),
                ),
            ] {
                let storage = left.encode_storage(&logical_type)?;
                prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, left.clone());
                let left_bytes = left.encode_ordered_component(&logical_type)?;
                let right_bytes = right.encode_ordered_component(&logical_type)?;
                prop_assert_eq!(value_order, left_bytes.cmp(&right_bytes));
                prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, left);
            }
        }

        #[test]
        fn time_codecs_round_trip_and_preserve_order(
            left in 0_u64..86_400_000_000_000_u64,
            right in 0_u64..86_400_000_000_000_u64,
        ) {
            let logical_type = LogicalType::Time;
            let scalar = ScalarValue::Time(left);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, scalar.clone());
            let left_bytes = scalar.encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Time(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, scalar);
        }

        #[test]
        fn uuid_codecs_round_trip_and_preserve_network_order(
            left in any::<[u8; 16]>(), right in any::<[u8; 16]>(),
        ) {
            let logical_type = LogicalType::Uuid;
            let scalar = ScalarValue::Uuid(left);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, scalar.clone());
            let left_bytes = scalar.encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Uuid(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, scalar);
        }

        #[test]
        fn float32_codecs_canonicalize_round_trip_and_preserve_total_order(
            left_bits in any::<u32>(), right_bits in any::<u32>(),
        ) {
            let logical_type = LogicalType::Float32;
            let left = CanonicalF32::new(f32::from_bits(left_bits));
            let right = CanonicalF32::new(f32::from_bits(right_bits));
            let scalar = ScalarValue::Float32(left);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, scalar.clone());
            let left_bytes = scalar.encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Float32(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, scalar);
        }

        #[test]
        fn float64_codecs_canonicalize_round_trip_and_preserve_total_order(
            left_bits in any::<u64>(), right_bits in any::<u64>(),
        ) {
            let logical_type = LogicalType::Float64;
            let left = CanonicalF64::new(f64::from_bits(left_bits));
            let right = CanonicalF64::new(f64::from_bits(right_bits));
            let scalar = ScalarValue::Float64(left);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, scalar.clone());
            let left_bytes = scalar.encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Float64(right).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, scalar);
        }

        #[test]
        fn interval_codecs_round_trip_and_preserve_lexicographic_order(
            left_months in any::<i32>(), left_days in any::<i32>(), left_nanos in any::<i64>(),
            right_months in any::<i32>(), right_days in any::<i32>(), right_nanos in any::<i64>(),
        ) {
            let logical_type = LogicalType::Interval;
            let left = ScalarValue::Interval {
                months: left_months,
                days: left_days,
                nanoseconds: left_nanos,
            };
            let right = ScalarValue::Interval {
                months: right_months,
                days: right_days,
                nanoseconds: right_nanos,
            };
            let storage = left.encode_storage(&logical_type)?;
            prop_assert_eq!(ScalarValue::decode_storage(&logical_type, &storage)?, left.clone());
            let left_bytes = left.encode_ordered_component(&logical_type)?;
            let right_bytes = right.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(
                (left_months, left_days, left_nanos).cmp(&(right_months, right_days, right_nanos)),
                left_bytes.cmp(&right_bytes)
            );
            prop_assert_eq!(ScalarValue::decode_ordered_component(&logical_type, &left_bytes)?, left);
        }

        #[test]
        fn binary_storage_and_ordered_codecs_round_trip(value in proptest::collection::vec(any::<u8>(), 0..512)) {
            let logical_type = LogicalType::Binary;
            let scalar = ScalarValue::Binary(value);
            let storage = scalar.encode_storage(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_storage(&logical_type, &storage)?,
                scalar.clone()
            );
            let ordered = scalar.encode_ordered_component(&logical_type)?;
            prop_assert_eq!(
                ScalarValue::decode_ordered_component(&logical_type, &ordered)?,
                scalar
            );
        }

        #[test]
        fn binary_ordered_bytes_preserve_value_order(
            left in proptest::collection::vec(any::<u8>(), 0..128),
            right in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let logical_type = LogicalType::Binary;
            let left_bytes = ScalarValue::Binary(left.clone()).encode_ordered_component(&logical_type)?;
            let right_bytes = ScalarValue::Binary(right.clone()).encode_ordered_component(&logical_type)?;
            prop_assert_eq!(left.cmp(&right), left_bytes.cmp(&right_bytes));
        }
    }

    #[test]
    fn primitive_scalar_golden_corpus_is_frozen() -> Result<(), NativeTypeError> {
        let fixtures = primitive_scalar_golden_fixtures()?;
        assert_eq!(fixtures.len(), 13);
        assert_eq!(fixtures[0].storage, [1]);
        assert_eq!(fixtures[0].ordered, [1, 1]);
        assert_eq!(fixtures[1].storage, [0xfe, 0xff]);
        assert_eq!(fixtures[1].ordered, [1, 0x7f, 0xfe]);
        assert_eq!(fixtures[6].storage, b"A\0B");
        assert_eq!(fixtures[6].ordered, [1, b'A', 0, 0xff, b'B', 0, 0]);
        assert_eq!(fixtures[12].storage, [0x11; 16]);
        assert_eq!(fixtures[12].ordered, [vec![1], vec![0x11; 16]].concat());
        Ok(())
    }

    #[test]
    fn array_storage_codec_round_trips_nested_null_and_empty_values() -> Result<(), NativeTypeError>
    {
        let logical_type = LogicalType::Array(Box::new(LogicalType::Array(Box::new(
            LogicalType::Signed(IntegerWidth::Bits16),
        ))));
        let value = ScalarValue::Array(vec![
            ScalarValue::Array(vec![ScalarValue::Signed(-2), ScalarValue::Null]),
            ScalarValue::Array(Vec::new()),
        ]);

        let encoded = value.encode_storage(&logical_type)?;
        assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        assert_eq!(
            encoded,
            [
                2, 0, 0, 0, 1, 12, 0, 0, 0, 2, 0, 0, 0, 1, 2, 0, 0, 0, 0xfe, 0xff, 0, 1, 4, 0, 0,
                0, 0, 0, 0, 0,
            ]
        );
        Ok(())
    }

    #[test]
    fn array_storage_codec_rejects_truncation_trailing_and_type_mismatch()
    -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Array(Box::new(LogicalType::Unsigned(IntegerWidth::Bits8)));
        let value = ScalarValue::Array(vec![ScalarValue::Unsigned(7)]);
        let encoded = value.encode_storage(&logical_type)?;
        for length in 0..encoded.len() {
            assert!(ScalarValue::decode_storage(&logical_type, &encoded[..length]).is_err());
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            ScalarValue::decode_storage(&logical_type, &trailing),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::Array(vec![ScalarValue::Text("bad".to_owned())])
                .encode_storage(&logical_type),
            Err(NativeTypeError::ScalarTypeMismatch)
        );
        Ok(())
    }

    #[test]
    fn array_ordered_codec_round_trips_and_preserves_lexicographic_order()
    -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Array(Box::new(LogicalType::Signed(IntegerWidth::Bits16)));
        let values = [
            ScalarValue::Array(vec![]),
            ScalarValue::Array(vec![ScalarValue::Null]),
            ScalarValue::Array(vec![ScalarValue::Signed(-1)]),
            ScalarValue::Array(vec![ScalarValue::Signed(-1), ScalarValue::Null]),
            ScalarValue::Array(vec![ScalarValue::Signed(0)]),
        ];
        let mut previous: Option<Vec<u8>> = None;
        for value in values {
            let encoded = value.encode_ordered_component(&logical_type)?;
            assert_eq!(
                ScalarValue::decode_ordered_component(&logical_type, &encoded)?,
                value
            );
            if let Some(previous) = previous {
                assert!(previous < encoded);
            }
            previous = Some(encoded);
        }
        Ok(())
    }

    #[test]
    fn array_ordered_codec_rejects_truncation_invalid_escape_and_trailing_bytes()
    -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Array(Box::new(LogicalType::Text));
        let value = ScalarValue::Array(vec![
            ScalarValue::Text("a\0b".to_owned()),
            ScalarValue::Null,
        ]);
        let encoded = value.encode_ordered_component(&logical_type)?;
        for length in 0..encoded.len() {
            assert!(
                ScalarValue::decode_ordered_component(&logical_type, &encoded[..length]).is_err()
            );
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            ScalarValue::decode_ordered_component(&logical_type, &trailing),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        let mut invalid = encoded;
        let escape = invalid
            .windows(2)
            .position(|window| window == [0, 0xff])
            .ok_or(NativeTypeError::InvalidScalarEncoding)?;
        invalid[escape + 1] = 0x7f;
        assert_eq!(
            ScalarValue::decode_ordered_component(&logical_type, &invalid),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        Ok(())
    }

    #[test]
    fn map_storage_codec_round_trips_canonical_order_and_nullable_values()
    -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Map(
            Box::new(LogicalType::Text),
            Box::new(LogicalType::Array(Box::new(LogicalType::Unsigned(
                IntegerWidth::Bits8,
            )))),
        );
        let value = ScalarValue::Map(vec![
            (
                ScalarValue::Text("a".to_owned()),
                ScalarValue::Array(vec![ScalarValue::Unsigned(1), ScalarValue::Null]),
            ),
            (ScalarValue::Text("b".to_owned()), ScalarValue::Null),
        ]);

        let encoded = value.encode_storage(&logical_type)?;
        assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        assert_eq!(
            encoded,
            [
                2, 0, 0, 0, 1, 0, 0, 0, b'a', 1, 11, 0, 0, 0, 2, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1,
                0, 0, 0, b'b', 0,
            ]
        );
        Ok(())
    }

    #[test]
    fn map_storage_codec_rejects_null_duplicate_and_unsorted_keys() {
        let logical_type = LogicalType::Map(
            Box::new(LogicalType::Text),
            Box::new(LogicalType::Unsigned(IntegerWidth::Bits8)),
        );
        for value in [
            ScalarValue::Map(vec![(ScalarValue::Null, ScalarValue::Unsigned(1))]),
            ScalarValue::Map(vec![
                (ScalarValue::Text("a".to_owned()), ScalarValue::Unsigned(1)),
                (ScalarValue::Text("a".to_owned()), ScalarValue::Unsigned(2)),
            ]),
            ScalarValue::Map(vec![
                (ScalarValue::Text("b".to_owned()), ScalarValue::Unsigned(1)),
                (ScalarValue::Text("a".to_owned()), ScalarValue::Unsigned(2)),
            ]),
        ] {
            assert_eq!(
                value.encode_storage(&logical_type),
                Err(NativeTypeError::InvalidScalarEncoding)
            );
        }
    }

    #[test]
    fn map_storage_codec_rejects_truncation_and_trailing_bytes() -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Map(
            Box::new(LogicalType::Unsigned(IntegerWidth::Bits8)),
            Box::new(LogicalType::Text),
        );
        let value = ScalarValue::Map(vec![(
            ScalarValue::Unsigned(1),
            ScalarValue::Text("one".to_owned()),
        )]);
        let encoded = value.encode_storage(&logical_type)?;
        for length in 0..encoded.len() {
            assert!(ScalarValue::decode_storage(&logical_type, &encoded[..length]).is_err());
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            ScalarValue::decode_storage(&logical_type, &trailing),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        Ok(())
    }

    #[test]
    fn vector_storage_codec_round_trips_canonical_float_elements() -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Vector(VectorType::new(VectorElement::Float32, 4)?);
        let value = ScalarValue::Vector(vec![
            CanonicalF32::new(-0.0),
            CanonicalF32::new(1.5),
            CanonicalF32::new(f32::NEG_INFINITY),
            CanonicalF32::new(f32::NAN),
        ]);
        let encoded = value.encode_storage(&logical_type)?;
        assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        assert_eq!(
            encoded,
            [
                0, 0, 0, 0, 0, 0, 0xc0, 0x3f, 0, 0, 0x80, 0xff, 0, 0, 0xc0, 0x7f,
            ]
        );
        Ok(())
    }

    #[test]
    fn vector_storage_codec_rejects_dimensions_noncanonical_bits_and_mismatch()
    -> Result<(), NativeTypeError> {
        let logical_type = LogicalType::Vector(VectorType::new(VectorElement::Float32, 2)?);
        assert_eq!(
            ScalarValue::Vector(vec![CanonicalF32::new(1.0)]).encode_storage(&logical_type),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Vector(vec![CanonicalF32::new(1.0), CanonicalF32::new(2.0)])
                .encode_storage(&LogicalType::Array(Box::new(LogicalType::Float32))),
            Err(NativeTypeError::ScalarTypeMismatch)
        );
        assert_eq!(
            ScalarValue::decode_storage(&logical_type, &[0; 4]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        let mut negative_zero = Vec::new();
        negative_zero.extend_from_slice(&(-0.0_f32).to_bits().to_le_bytes());
        negative_zero.extend_from_slice(&1.0_f32.to_bits().to_le_bytes());
        assert_eq!(
            ScalarValue::decode_storage(&logical_type, &negative_zero),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        let mut noncanonical_nan = Vec::new();
        noncanonical_nan.extend_from_slice(&0x7fc0_0001_u32.to_le_bytes());
        noncanonical_nan.extend_from_slice(&1.0_f32.to_bits().to_le_bytes());
        assert_eq!(
            ScalarValue::decode_storage(&logical_type, &noncanonical_nan),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        Ok(())
    }

    #[test]
    fn vector_dimensions_are_nonzero() {
        assert_eq!(
            VectorType::new(VectorElement::Float32, 0),
            Err(NativeTypeError::EmptyVector)
        );
        assert_eq!(
            VectorType::new(VectorElement::Float32, 384).map(VectorType::dimension),
            Ok(384)
        );
    }

    #[test]
    fn floats_canonicalize_nan_and_signed_zero() {
        assert_eq!(CanonicalF32::new(-0.0).bits(), 0);
        assert_eq!(CanonicalF64::new(-0.0).bits(), 0);
        assert_eq!(
            CanonicalF32::new(f32::from_bits(0x7fc0_1234)),
            CanonicalF32::new(f32::NAN)
        );
        assert_eq!(
            CanonicalF64::new(f64::from_bits(0x7ff8_0000_0000_1234)),
            CanonicalF64::new(f64::NAN)
        );
        assert!(CanonicalF32::new(f32::NAN) > CanonicalF32::new(f32::INFINITY));
    }

    #[test]
    fn logical_type_descriptors_round_trip_with_golden_bytes() -> Result<(), NativeTypeError> {
        let logical_types = vec![
            LogicalType::Boolean,
            LogicalType::Signed(IntegerWidth::Bits8),
            LogicalType::Signed(IntegerWidth::Bits16),
            LogicalType::Signed(IntegerWidth::Bits32),
            LogicalType::Signed(IntegerWidth::Bits64),
            LogicalType::Unsigned(IntegerWidth::Bits8),
            LogicalType::Unsigned(IntegerWidth::Bits16),
            LogicalType::Unsigned(IntegerWidth::Bits32),
            LogicalType::Unsigned(IntegerWidth::Bits64),
            LogicalType::Decimal(DecimalType::new(38, 18)?),
            LogicalType::Float32,
            LogicalType::Float64,
            LogicalType::Text,
            LogicalType::Binary,
            LogicalType::Date,
            LogicalType::Time,
            LogicalType::Timestamp,
            LogicalType::Interval,
            LogicalType::Uuid,
            LogicalType::Json,
            LogicalType::Array(Box::new(LogicalType::Text)),
            LogicalType::Map(
                Box::new(LogicalType::Text),
                Box::new(LogicalType::Unsigned(IntegerWidth::Bits64)),
            ),
            LogicalType::Vector(VectorType::new(VectorElement::Float32, 384)?),
        ];
        for logical_type in logical_types {
            let encoded = logical_type.encode_descriptor()?;
            assert_eq!(LogicalType::decode_descriptor(&encoded)?, logical_type);
        }

        assert_eq!(
            LogicalType::Signed(IntegerWidth::Bits16).encode_descriptor()?,
            [2, 16]
        );
        assert_eq!(
            LogicalType::Decimal(DecimalType::new(10, 2)?).encode_descriptor()?,
            [4, 10, 2]
        );
        assert_eq!(
            LogicalType::Vector(VectorType::new(VectorElement::Float32, 384)?)
                .encode_descriptor()?,
            [17, 1, 0x80, 1]
        );
        Ok(())
    }

    #[test]
    fn logical_type_descriptors_enforce_depth_and_canonical_form() {
        let mut at_limit = LogicalType::Boolean;
        for _ in 0..64 {
            at_limit = LogicalType::Array(Box::new(at_limit));
        }
        let encoded = at_limit.encode_descriptor();
        assert!(encoded.is_ok());
        assert_eq!(
            encoded.and_then(|bytes| LogicalType::decode_descriptor(&bytes)),
            Ok(at_limit.clone())
        );

        let too_deep = LogicalType::Array(Box::new(at_limit));
        assert_eq!(
            too_deep.encode_descriptor(),
            Err(NativeTypeError::TypeNestingDepthExceeded)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[0xff]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[2]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[2, 7]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[4, 0, 0]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[17, 1, 0, 0]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
        assert_eq!(
            LogicalType::decode_descriptor(&[1, 0]),
            Err(NativeTypeError::InvalidTypeDescriptor)
        );
    }

    #[test]
    fn primitive_storage_scalars_round_trip() -> Result<(), NativeTypeError> {
        let decimal = DecimalType::new(12, 4)?;
        let values = vec![
            (LogicalType::Boolean, ScalarValue::Boolean(true)),
            (
                LogicalType::Signed(IntegerWidth::Bits64),
                ScalarValue::Signed(i64::MIN),
            ),
            (
                LogicalType::Unsigned(IntegerWidth::Bits64),
                ScalarValue::Unsigned(u64::MAX),
            ),
            (
                LogicalType::Decimal(decimal),
                ScalarValue::Decimal(-123_456),
            ),
            (
                LogicalType::Float32,
                ScalarValue::Float32(CanonicalF32::new(f32::NAN)),
            ),
            (
                LogicalType::Float64,
                ScalarValue::Float64(CanonicalF64::new(-123.5)),
            ),
            (
                LogicalType::Text,
                ScalarValue::Text("Hyphae\0SQL".to_owned()),
            ),
            (LogicalType::Binary, ScalarValue::Binary(vec![0, 1, 0xff])),
            (LogicalType::Date, ScalarValue::Date(-20_000)),
            (LogicalType::Time, ScalarValue::Time(43_200_000_000_001)),
            (LogicalType::Timestamp, ScalarValue::Timestamp(-1_234_567)),
            (
                LogicalType::Interval,
                ScalarValue::Interval {
                    months: -2,
                    days: 3,
                    nanoseconds: -4,
                },
            ),
            (LogicalType::Uuid, ScalarValue::Uuid([0xab; 16])),
        ];

        for (logical_type, value) in values {
            let encoded = value.encode_storage(&logical_type)?;
            assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        }

        for (width, value) in [
            (IntegerWidth::Bits8, -128),
            (IntegerWidth::Bits16, -32_768),
            (IntegerWidth::Bits32, i64::from(i32::MIN)),
            (IntegerWidth::Bits64, i64::MIN),
        ] {
            let logical_type = LogicalType::Signed(width);
            let value = ScalarValue::Signed(value);
            let encoded = value.encode_storage(&logical_type)?;
            assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        }
        for (width, value) in [
            (IntegerWidth::Bits8, u64::from(u8::MAX)),
            (IntegerWidth::Bits16, u64::from(u16::MAX)),
            (IntegerWidth::Bits32, u64::from(u32::MAX)),
            (IntegerWidth::Bits64, u64::MAX),
        ] {
            let logical_type = LogicalType::Unsigned(width);
            let value = ScalarValue::Unsigned(value);
            let encoded = value.encode_storage(&logical_type)?;
            assert_eq!(ScalarValue::decode_storage(&logical_type, &encoded)?, value);
        }

        assert_eq!(
            ScalarValue::Signed(-2).encode_storage(&LogicalType::Signed(IntegerWidth::Bits16))?,
            [0xfe, 0xff]
        );
        assert_eq!(
            ScalarValue::Text("abc".to_owned()).encode_storage(&LogicalType::Text)?,
            b"abc"
        );
        Ok(())
    }

    #[test]
    fn storage_scalars_reject_noncanonical_and_out_of_domain_values() -> Result<(), NativeTypeError>
    {
        assert_eq!(
            ScalarValue::Null.encode_storage(&LogicalType::Boolean),
            Err(NativeTypeError::NullRequiresRowBitmap)
        );
        assert_eq!(
            ScalarValue::Null.encode_storage(&LogicalType::Json),
            Err(NativeTypeError::NullRequiresRowBitmap)
        );
        assert_eq!(
            ScalarValue::Signed(128).encode_storage(&LogicalType::Signed(IntegerWidth::Bits8)),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Unsigned(256).encode_storage(&LogicalType::Unsigned(IntegerWidth::Bits8)),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Decimal(1_000)
                .encode_storage(&LogicalType::Decimal(DecimalType::new(3, 0)?)),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Time(86_400_000_000_000).encode_storage(&LogicalType::Time),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Boolean(true).encode_storage(&LogicalType::Text),
            Err(NativeTypeError::ScalarTypeMismatch)
        );
        assert_eq!(
            ScalarValue::Text("{}".to_owned()).encode_storage(&LogicalType::Json),
            Err(NativeTypeError::UnsupportedScalarType)
        );
        assert_eq!(
            ScalarValue::decode_storage(&LogicalType::Boolean, &[2]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_storage(&LogicalType::Signed(IntegerWidth::Bits16), &[0]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_storage(&LogicalType::Float32, &(-0.0_f32).to_bits().to_le_bytes()),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_storage(
                &LogicalType::Float64,
                &0x7ff8_0000_0000_0001_u64.to_le_bytes()
            ),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_storage(&LogicalType::Time, &[0; 7]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::Binary(vec![0; MAX_SCALAR_BYTES + 1]).encode_storage(&LogicalType::Binary),
            Err(NativeTypeError::ScalarLengthExceeded)
        );
        Ok(())
    }

    fn assert_ordered_round_trip(
        logical_type: &LogicalType,
        values: &[ScalarValue],
    ) -> Result<(), NativeTypeError> {
        let mut encodings = Vec::with_capacity(values.len());
        for value in values {
            let encoded = value.encode_ordered_component(logical_type)?;
            assert_eq!(
                ScalarValue::decode_ordered_component(logical_type, &encoded)?,
                *value
            );
            encodings.push(encoded);
        }
        for pair in encodings.windows(2) {
            assert!(
                pair[0] < pair[1],
                "ordered encodings are not increasing: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        Ok(())
    }

    #[test]
    fn ordered_numeric_components_preserve_total_order() -> Result<(), NativeTypeError> {
        assert_ordered_round_trip(
            &LogicalType::Boolean,
            &[
                ScalarValue::Null,
                ScalarValue::Boolean(false),
                ScalarValue::Boolean(true),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Signed(IntegerWidth::Bits64),
            &[
                ScalarValue::Null,
                ScalarValue::Signed(i64::MIN),
                ScalarValue::Signed(-1),
                ScalarValue::Signed(0),
                ScalarValue::Signed(1),
                ScalarValue::Signed(i64::MAX),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Unsigned(IntegerWidth::Bits64),
            &[
                ScalarValue::Null,
                ScalarValue::Unsigned(0),
                ScalarValue::Unsigned(1),
                ScalarValue::Unsigned(u64::MAX),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Decimal(DecimalType::new(4, 2)?),
            &[
                ScalarValue::Null,
                ScalarValue::Decimal(-9_999),
                ScalarValue::Decimal(-1),
                ScalarValue::Decimal(0),
                ScalarValue::Decimal(1),
                ScalarValue::Decimal(9_999),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Float32,
            &[
                ScalarValue::Null,
                ScalarValue::Float32(CanonicalF32::new(f32::NEG_INFINITY)),
                ScalarValue::Float32(CanonicalF32::new(-1.0)),
                ScalarValue::Float32(CanonicalF32::new(0.0)),
                ScalarValue::Float32(CanonicalF32::new(1.0)),
                ScalarValue::Float32(CanonicalF32::new(f32::INFINITY)),
                ScalarValue::Float32(CanonicalF32::new(f32::NAN)),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Float64,
            &[
                ScalarValue::Null,
                ScalarValue::Float64(CanonicalF64::new(f64::NEG_INFINITY)),
                ScalarValue::Float64(CanonicalF64::new(-1.0)),
                ScalarValue::Float64(CanonicalF64::new(0.0)),
                ScalarValue::Float64(CanonicalF64::new(1.0)),
                ScalarValue::Float64(CanonicalF64::new(f64::INFINITY)),
                ScalarValue::Float64(CanonicalF64::new(f64::NAN)),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn ordered_variable_components_preserve_binary_order() -> Result<(), NativeTypeError> {
        assert_ordered_round_trip(
            &LogicalType::Text,
            &[
                ScalarValue::Null,
                ScalarValue::Text(String::new()),
                ScalarValue::Text("\0".to_owned()),
                ScalarValue::Text("\0a".to_owned()),
                ScalarValue::Text("a".to_owned()),
                ScalarValue::Text("a\0".to_owned()),
                ScalarValue::Text("aa".to_owned()),
                ScalarValue::Text("b".to_owned()),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Binary,
            &[
                ScalarValue::Null,
                ScalarValue::Binary(vec![]),
                ScalarValue::Binary(vec![0]),
                ScalarValue::Binary(vec![0, 1]),
                ScalarValue::Binary(vec![1]),
                ScalarValue::Binary(vec![1, 0]),
                ScalarValue::Binary(vec![1, 1]),
                ScalarValue::Binary(vec![0xff]),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn ordered_temporal_and_uuid_components_preserve_total_order() -> Result<(), NativeTypeError> {
        assert_ordered_round_trip(
            &LogicalType::Date,
            &[
                ScalarValue::Null,
                ScalarValue::Date(i32::MIN),
                ScalarValue::Date(-1),
                ScalarValue::Date(0),
                ScalarValue::Date(i32::MAX),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Time,
            &[
                ScalarValue::Null,
                ScalarValue::Time(0),
                ScalarValue::Time(1),
                ScalarValue::Time(86_399_999_999_999),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Timestamp,
            &[
                ScalarValue::Null,
                ScalarValue::Timestamp(i64::MIN),
                ScalarValue::Timestamp(-1),
                ScalarValue::Timestamp(0),
                ScalarValue::Timestamp(i64::MAX),
            ],
        )?;
        assert_ordered_round_trip(
            &LogicalType::Interval,
            &[
                ScalarValue::Null,
                ScalarValue::Interval {
                    months: -1,
                    days: 5,
                    nanoseconds: 9,
                },
                ScalarValue::Interval {
                    months: 0,
                    days: -1,
                    nanoseconds: 9,
                },
                ScalarValue::Interval {
                    months: 0,
                    days: 0,
                    nanoseconds: -1,
                },
                ScalarValue::Interval {
                    months: 0,
                    days: 0,
                    nanoseconds: 0,
                },
            ],
        )?;
        let mut uuid_tail = [0; 16];
        uuid_tail[15] = 1;
        let mut uuid_head = [0; 16];
        uuid_head[0] = 1;
        assert_ordered_round_trip(
            &LogicalType::Uuid,
            &[
                ScalarValue::Null,
                ScalarValue::Uuid([0; 16]),
                ScalarValue::Uuid(uuid_tail),
                ScalarValue::Uuid(uuid_head),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn ordered_components_reject_malformed_or_noncanonical_bytes() -> Result<(), NativeTypeError> {
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Text, &[0, 0]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Text, &[2]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Text, &[1, b'a']),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Text, &[1, 0, 1]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Text, &[1, 0, 0, 1]),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(
                &LogicalType::Signed(IntegerWidth::Bits8),
                &[1, 0x80, 0]
            ),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(
                &LogicalType::Float32,
                &[1, 0x7f, 0xff, 0xff, 0xff]
            ),
            Err(NativeTypeError::InvalidScalarEncoding)
        );
        let mut invalid_time = vec![1];
        invalid_time.extend_from_slice(&86_400_000_000_000_u64.to_be_bytes());
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Time, &invalid_time),
            Err(NativeTypeError::ScalarOutOfRange)
        );
        assert_eq!(
            ScalarValue::Boolean(false).encode_ordered_component(&LogicalType::Json),
            Err(NativeTypeError::UnsupportedOrderedType)
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Json, &[1]),
            Err(NativeTypeError::UnsupportedOrderedType)
        );
        assert_eq!(
            ScalarValue::Null.encode_ordered_component(&LogicalType::Json)?,
            [0]
        );
        assert_eq!(
            ScalarValue::decode_ordered_component(&LogicalType::Json, &[0])?,
            ScalarValue::Null
        );
        Ok(())
    }
}
