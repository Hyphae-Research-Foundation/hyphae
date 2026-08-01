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
    CatalogVersion,
    u64,
    NonZeroU64,
    "catalog version",
    "Immutable catalog snapshot identity."
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

#[cfg(test)]
mod tests {
    use super::{
        CanonicalF32, CanonicalF64, CatalogVersion, Csn, DecimalType, NativeTypeError, ObjectId,
        VectorElement, VectorType,
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
}
