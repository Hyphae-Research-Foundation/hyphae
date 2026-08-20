// SPDX-License-Identifier: Apache-2.0

//! Stable CLI exit classes and product-error rendering.

use std::{fmt, io};

use hyphae_native_product::proof::NativeProofError;
use hyphae_native_product::{
    BackupProductError, ProductError, ProductErrorCategory, ProductErrorCode,
};
use serde_json::{Value, json};

/// One CLI failure normalized to the native product error registry.
#[derive(Debug)]
pub(crate) struct CliFailure {
    error: Box<ProductError>,
}

impl CliFailure {
    pub(crate) fn invalid() -> Self {
        ProductError::from_code(ProductErrorCode::InvalidRequest).into()
    }

    pub(crate) fn io() -> Self {
        ProductError::from_code(ProductErrorCode::Io).into()
    }

    pub(crate) fn internal() -> Self {
        ProductError::from_code(ProductErrorCode::Internal).into()
    }

    pub(crate) fn error(&self) -> &ProductError {
        &self.error
    }

    pub(crate) const fn exit_class(&self) -> u8 {
        exit_class(self.error.category())
    }
}

impl fmt::Display for CliFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for CliFailure {}

impl From<ProductError> for CliFailure {
    fn from(error: ProductError) -> Self {
        Self {
            error: Box::new(error),
        }
    }
}

impl From<Box<ProductError>> for CliFailure {
    fn from(error: Box<ProductError>) -> Self {
        Self { error }
    }
}

impl From<io::Error> for CliFailure {
    fn from(_error: io::Error) -> Self {
        Self::io()
    }
}

impl From<serde_json::Error> for CliFailure {
    fn from(_error: serde_json::Error) -> Self {
        Self::invalid()
    }
}

impl From<hyphae_native_daemon::DaemonError> for CliFailure {
    fn from(error: hyphae_native_daemon::DaemonError) -> Self {
        match error {
            hyphae_native_daemon::DaemonError::Product(error) => error.into(),
            hyphae_native_daemon::DaemonError::Io(_) => Self::io(),
            _ => Self::internal(),
        }
    }
}

impl From<hyphae_server::NativeHttpV2Error> for CliFailure {
    fn from(error: hyphae_server::NativeHttpV2Error) -> Self {
        match error {
            hyphae_server::NativeHttpV2Error::Product(error) => error.into(),
            hyphae_server::NativeHttpV2Error::Configuration(_) => Self::invalid(),
            hyphae_server::NativeHttpV2Error::Bind { .. }
            | hyphae_server::NativeHttpV2Error::Serve(_) => Self::io(),
        }
    }
}

impl From<BackupProductError> for CliFailure {
    fn from(error: BackupProductError) -> Self {
        match error {
            BackupProductError::InvalidRequest(_) => Self::invalid(),
            BackupProductError::Cancelled => {
                ProductError::from_code(ProductErrorCode::Cancelled).into()
            }
            BackupProductError::Backup { error, .. }
            | BackupProductError::Verification { error, .. }
            | BackupProductError::Restore { error, .. } => (*error).into(),
            BackupProductError::DoctorAfterRestore { .. } => {
                ProductError::from_code(ProductErrorCode::BackupInvalid).into()
            }
        }
    }
}

impl From<crate::migrate_valkey::rdb::RdbError> for CliFailure {
    fn from(error: crate::migrate_valkey::rdb::RdbError) -> Self {
        use crate::migrate_valkey::rdb::RdbError;
        let code = match error {
            RdbError::Limit { .. } => ProductErrorCode::LimitExceeded,
            RdbError::Checksum { .. } => ProductErrorCode::Corruption,
            _ => ProductErrorCode::InvalidRequest,
        };
        ProductError::from_code(code).into()
    }
}

impl From<NativeProofError> for CliFailure {
    fn from(error: NativeProofError) -> Self {
        let code = match error {
            NativeProofError::Io { .. } => ProductErrorCode::Io,
            NativeProofError::Interrupted => ProductErrorCode::Cancelled,
            NativeProofError::DestinationExists(_) => ProductErrorCode::DataDirectoryExists,
            NativeProofError::LimitExceeded { .. } | NativeProofError::LengthOverflow => {
                ProductErrorCode::LimitExceeded
            }
            NativeProofError::OriginNotDirectory(_)
            | NativeProofError::UnsupportedVersion { .. }
            | NativeProofError::TrustedAnchorMismatch => ProductErrorCode::InvalidRequest,
            NativeProofError::Invalid(_)
            | NativeProofError::ChecksumMismatch
            | NativeProofError::DigestMismatch(_)
            | NativeProofError::WitnessAnchorMismatch
            | NativeProofError::WitnessReferenceMismatch => ProductErrorCode::Corruption,
        };
        ProductError::from_code(code).into()
    }
}

/// Stable process exit class derived only from the product error category.
pub(crate) const fn exit_class(category: ProductErrorCategory) -> u8 {
    match category {
        ProductErrorCategory::InvalidRequest => 2,
        ProductErrorCategory::NotFound => 3,
        ProductErrorCategory::Conflict => 4,
        ProductErrorCategory::Limit => 5,
        ProductErrorCategory::Deadline => 6,
        ProductErrorCategory::Cancelled => 7,
        ProductErrorCategory::Authorization => 8,
        ProductErrorCategory::Corruption => 9,
        ProductErrorCategory::Unavailable => 10,
        ProductErrorCategory::Io => 11,
        _ => 12,
    }
}

pub(crate) fn error_json(error: &ProductError) -> Value {
    json!({
        "error": {
            "code": error.code().as_str(),
            "category": error.category().as_str(),
            "message": error.message(),
            "retry": error.retry().as_str(),
            "transaction_state": error.transaction_state().as_str(),
            "request_id": error.request_id().map(|value| value.to_string()),
            "trace_id": error.trace_id().map(|value| value.to_string()),
            "object_id": error.object_id().map(|value| value.get().to_string()),
            "transaction_id": error.details().transaction_id().map(|value| value.get().to_string()),
        },
        "exit_class": exit_class(error.category()),
    })
}

#[cfg(test)]
mod tests {
    use hyphae_native_product::ProductErrorCategory;

    use super::exit_class;

    #[test]
    fn every_product_category_has_a_distinct_stable_exit_class() {
        let classes = [
            ProductErrorCategory::InvalidRequest,
            ProductErrorCategory::NotFound,
            ProductErrorCategory::Conflict,
            ProductErrorCategory::Limit,
            ProductErrorCategory::Deadline,
            ProductErrorCategory::Cancelled,
            ProductErrorCategory::Authorization,
            ProductErrorCategory::Corruption,
            ProductErrorCategory::Unavailable,
            ProductErrorCategory::Io,
            ProductErrorCategory::Internal,
        ]
        .map(exit_class);
        assert_eq!(classes, [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }
}
