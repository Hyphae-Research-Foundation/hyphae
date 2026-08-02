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
        if scalar_type_is_unsupported(logical_type) {
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
            LogicalType::Json
            | LogicalType::Array(_)
            | LogicalType::Map(_, _)
            | LogicalType::Vector(_) => Err(NativeTypeError::UnsupportedScalarType),
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
            (
                _,
                LogicalType::Json
                | LogicalType::Array(_)
                | LogicalType::Map(_, _)
                | LogicalType::Vector(_),
            ) => return Err(NativeTypeError::UnsupportedOrderedType),
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
            LogicalType::Json
            | LogicalType::Array(_)
            | LogicalType::Map(_, _)
            | LogicalType::Vector(_) => Err(NativeTypeError::UnsupportedOrderedType),
        }
    }
}

fn scalar_type_is_unsupported(logical_type: &LogicalType) -> bool {
    matches!(
        logical_type,
        LogicalType::Json | LogicalType::Array(_) | LogicalType::Map(_, _) | LogicalType::Vector(_)
    )
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

#[cfg(test)]
mod tests {
    use super::{
        CanonicalF32, CanonicalF64, CatalogVersion, Csn, DecimalType, IntegerWidth, LogicalType,
        MAX_SCALAR_BYTES, NativeTypeError, ObjectId, ScalarValue, VectorElement, VectorType,
    };

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
