// SPDX-License-Identifier: Apache-2.0

//! Stable product identity, compatibility constants, and canonical vector
//! domain values shared by Hyphae surfaces.

use std::{error::Error, fmt};

/// Canonical product and executable name.
pub const PRODUCT_NAME: &str = "hyphae";

/// Current public HTTP API version.
pub const API_VERSION: &str = "v1";

/// Current on-disk format version.
pub const DISK_FORMAT_VERSION: u16 = 2;

/// Oldest on-disk format this binary can open and migrate.
pub const MIN_DISK_FORMAT_VERSION: u16 = 1;

/// Disk format introduced by Hyphae 0.2 durable retrieval.
pub const DURABLE_RETRIEVAL_DISK_FORMAT_VERSION: u16 = 2;

/// Maximum canonical vector-space identifier length.
pub const MAX_VECTOR_SPACE_NAME_BYTES: usize = 128;

/// Maximum canonical vector dimension.
pub const MAX_VECTOR_DIMENSIONS: usize = 4_096;

/// Scale used by canonical cosine scores.
pub const SCORE_NANOS_SCALE: i64 = 1_000_000_000;

/// Failure to construct a canonical shared vector value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorValueError {
    /// The vector-space name is empty.
    EmptySpaceName,
    /// The vector-space name exceeds its byte limit.
    SpaceNameTooLong,
    /// The vector-space name does not match the canonical ASCII grammar.
    InvalidSpaceName,
    /// A vector has no elements.
    EmptyVector,
    /// A vector exceeds the maximum dimension.
    DimensionTooLarge,
    /// Signed Q15 reserves `i16::MIN` and therefore rejects it.
    InvalidQ15Element,
    /// Cosine is undefined for an all-zero vector.
    ZeroVector,
    /// A space dimension and vector dimension differ.
    DimensionMismatch,
}

impl fmt::Display for VectorValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptySpaceName => "vector-space name must be nonempty",
            Self::SpaceNameTooLong => "vector-space name exceeds 128 bytes",
            Self::InvalidSpaceName => {
                "vector-space name does not match the canonical ASCII grammar"
            }
            Self::EmptyVector => "vector must be nonempty",
            Self::DimensionTooLarge => "vector dimension exceeds 4096",
            Self::InvalidQ15Element => "Q15 vector elements cannot equal -32768",
            Self::ZeroVector => "vector must have nonzero magnitude",
            Self::DimensionMismatch => "vector dimension does not match the named space",
        };
        formatter.write_str(message)
    }
}

impl Error for VectorValueError {}

/// Canonical bounded ASCII name for one vector space.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VectorSpaceName(String);

impl VectorSpaceName {
    /// Validates and constructs a canonical vector-space name.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` matches
    /// `[A-Za-z][A-Za-z0-9._-]{0,127}`.
    pub fn new(value: impl Into<String>) -> Result<Self, VectorValueError> {
        let value = value.into();
        if value.is_empty() {
            return Err(VectorValueError::EmptySpaceName);
        }
        if value.len() > MAX_VECTOR_SPACE_NAME_BYTES {
            return Err(VectorValueError::SpaceNameTooLong);
        }
        let mut bytes = value.bytes();
        let first = bytes.next().ok_or(VectorValueError::EmptySpaceName)?;
        if !first.is_ascii_alphabetic()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(VectorValueError::InvalidSpaceName);
        }
        Ok(Self(value))
    }

    /// Returns the canonical UTF-8/ASCII representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the name and returns its canonical string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for VectorSpaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Canonical nonzero signed-Q15 vector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Q15Vector(Vec<i16>);

impl Q15Vector {
    /// Validates and constructs a canonical Q15 vector.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty, oversized, all-zero vector or an element
    /// equal to `i16::MIN`.
    pub fn new(values: impl Into<Vec<i16>>) -> Result<Self, VectorValueError> {
        let values = values.into();
        if values.is_empty() {
            return Err(VectorValueError::EmptyVector);
        }
        if values.len() > MAX_VECTOR_DIMENSIONS {
            return Err(VectorValueError::DimensionTooLarge);
        }
        if values.contains(&i16::MIN) {
            return Err(VectorValueError::InvalidQ15Element);
        }
        if values.iter().all(|value| *value == 0) {
            return Err(VectorValueError::ZeroVector);
        }
        Ok(Self(values))
    }

    /// Returns the signed Q15 elements.
    pub fn as_slice(&self) -> &[i16] {
        &self.0
    }

    /// Returns the canonical dimension.
    pub fn dimension(&self) -> u16 {
        u16::try_from(self.0.len()).unwrap_or(u16::MAX)
    }

    /// Consumes the vector and returns its elements.
    pub fn into_vec(self) -> Vec<i16> {
        self.0
    }
}

/// Supported canonical vector metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum VectorMetric {
    /// Integer cosine-nanos semantics from ADR-0015.
    Cosine = 1,
}

/// Immutable definition of one named vector space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorSpaceDefinition {
    /// Canonical vector-space identifier.
    pub name: VectorSpaceName,
    /// Fixed vector dimension.
    pub dimension: u16,
    /// Canonical metric.
    pub metric: VectorMetric,
}

impl VectorSpaceDefinition {
    /// Constructs a cosine vector space with a fixed dimension.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or oversized dimension.
    pub fn cosine(name: VectorSpaceName, dimension: u16) -> Result<Self, VectorValueError> {
        if dimension == 0 {
            return Err(VectorValueError::EmptyVector);
        }
        if usize::from(dimension) > MAX_VECTOR_DIMENSIONS {
            return Err(VectorValueError::DimensionTooLarge);
        }
        Ok(Self {
            name,
            dimension,
            metric: VectorMetric::Cosine,
        })
    }

    /// Checks that a vector belongs to this space.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions differ.
    pub fn validate_vector(&self, vector: &Q15Vector) -> Result<(), VectorValueError> {
        if vector.dimension() == self.dimension {
            Ok(())
        } else {
            Err(VectorValueError::DimensionMismatch)
        }
    }
}

/// Product version information that can be reported without opening a data directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionInfo {
    /// Product name.
    pub product: &'static str,
    /// Cargo package version for the running binary.
    pub engine: &'static str,
    /// Public HTTP API version.
    pub api: &'static str,
    /// On-disk format version.
    pub disk_format: u16,
}

/// Returns the version information compiled into this build.
pub const fn current_version() -> VersionInfo {
    VersionInfo {
        product: PRODUCT_NAME,
        engine: env!("CARGO_PKG_VERSION"),
        api: API_VERSION,
        disk_format: DISK_FORMAT_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        API_VERSION, DISK_FORMAT_VERSION, PRODUCT_NAME, Q15Vector, VectorSpaceDefinition,
        VectorSpaceName, VectorValueError, current_version,
    };

    #[test]
    fn current_version_matches_public_constants() {
        let version = current_version();
        assert_eq!(version.product, PRODUCT_NAME);
        assert_eq!(version.api, API_VERSION);
        assert_eq!(version.disk_format, DISK_FORMAT_VERSION);
        assert!(!version.engine.is_empty());
    }

    #[test]
    fn vector_space_names_follow_the_canonical_ascii_grammar() -> Result<(), VectorValueError> {
        let name = VectorSpaceName::new("semantic.v1")?;
        assert_eq!(name.as_str(), "semantic.v1");
        assert_eq!(
            VectorSpaceName::new("1semantic"),
            Err(VectorValueError::InvalidSpaceName)
        );
        assert_eq!(
            VectorSpaceName::new("semántica"),
            Err(VectorValueError::InvalidSpaceName)
        );
        Ok(())
    }

    #[test]
    fn q15_vectors_are_nonzero_bounded_and_dimension_checked() -> Result<(), VectorValueError> {
        let vector = Q15Vector::new(vec![32_767, 0])?;
        let space = VectorSpaceDefinition::cosine(VectorSpaceName::new("semantic")?, 2)?;
        assert_eq!(space.validate_vector(&vector), Ok(()));
        assert_eq!(
            Q15Vector::new(vec![i16::MIN]),
            Err(VectorValueError::InvalidQ15Element)
        );
        assert_eq!(
            Q15Vector::new(vec![0, 0]),
            Err(VectorValueError::ZeroVector)
        );
        Ok(())
    }
}
