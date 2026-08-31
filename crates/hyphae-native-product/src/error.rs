// SPDX-License-Identifier: Apache-2.0

//! Transport-independent product errors and engine-to-product mappings.

use std::{fmt, io};

use hyphae_native_runtime::{
    MAX_SQL_JOIN_CANDIDATES, MAX_SQL_SCAN_CANDIDATES, NativeDirectoryError, NativeRuntimeError,
    SqlError,
};
use hyphae_native_types::{ObjectId, TransactionId};

/// Maximum bytes in a stable product error code or limit name.
pub const MAX_PRODUCT_ERROR_IDENTIFIER_BYTES: usize = 64;
/// Maximum bytes in a safe product error message.
pub const MAX_PRODUCT_ERROR_MESSAGE_BYTES: usize = 256;
/// Maximum unknown detail fields retained by one error.
pub const MAX_PRODUCT_ERROR_UNKNOWN_DETAILS: usize = 16;
/// Maximum bytes retained by one unknown detail field.
pub const MAX_PRODUCT_ERROR_DETAIL_BYTES: usize = 256;

const PRODUCT_MAX_EXPIRY_SWEEP_KEYS: usize = 4_096;

/// A checked, bounded lowercase ASCII product identifier.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductErrorIdentifier {
    bytes: [u8; MAX_PRODUCT_ERROR_IDENTIFIER_BYTES],
    length: u8,
}

impl ProductErrorIdentifier {
    /// Constructs a bounded identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty, oversized, or noncanonical identifier.
    pub fn new(value: &str) -> Result<Self, ProductErrorValidationError> {
        let raw = value.as_bytes();
        if raw.is_empty() || raw.len() > MAX_PRODUCT_ERROR_IDENTIFIER_BYTES {
            return Err(ProductErrorValidationError::InvalidIdentifier);
        }
        if !raw[0].is_ascii_lowercase()
            || !raw
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        {
            return Err(ProductErrorValidationError::InvalidIdentifier);
        }
        let mut bytes = [0_u8; MAX_PRODUCT_ERROR_IDENTIFIER_BYTES];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self {
            bytes,
            length: u8::try_from(raw.len())
                .map_err(|_| ProductErrorValidationError::InvalidIdentifier)?,
        })
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.length)]).unwrap_or_default()
    }
}

impl fmt::Debug for ProductErrorIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ProductErrorIdentifier")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for ProductErrorIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable public product error category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductErrorCategory {
    /// The request or its value domain is invalid.
    InvalidRequest,
    /// The requested catalog or data object does not exist.
    NotFound,
    /// Snapshot, uniqueness, or transaction admission rejected the request.
    Conflict,
    /// A configured resource bound rejected the request.
    Limit,
    /// A request deadline elapsed.
    Deadline,
    /// The caller cancelled the request.
    Cancelled,
    /// The caller lacks permission for the operation.
    Authorization,
    /// Durable or logical authority is malformed.
    Corruption,
    /// The directory or service cannot currently accept the request.
    Unavailable,
    /// A filesystem or device operation failed.
    Io,
    /// An engine failure has not yet received a narrower public mapping.
    Internal,
}

impl ProductErrorCategory {
    /// Returns the stable lowercase ASCII wire identity.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid-request",
            Self::NotFound => "not-found",
            Self::Conflict => "conflict",
            Self::Limit => "limit",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::Authorization => "authorization",
            Self::Corruption => "corruption",
            Self::Unavailable => "unavailable",
            Self::Io => "io",
            Self::Internal => "internal",
        }
    }

    pub(crate) const fn wire_tag(self) -> u8 {
        match self {
            Self::InvalidRequest => 0,
            Self::NotFound => 1,
            Self::Conflict => 2,
            Self::Limit => 3,
            Self::Deadline => 4,
            Self::Cancelled => 5,
            Self::Authorization => 6,
            Self::Corruption => 7,
            Self::Unavailable => 8,
            Self::Io => 9,
            Self::Internal => 10,
        }
    }

    pub(crate) const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::InvalidRequest),
            1 => Some(Self::NotFound),
            2 => Some(Self::Conflict),
            3 => Some(Self::Limit),
            4 => Some(Self::Deadline),
            5 => Some(Self::Cancelled),
            6 => Some(Self::Authorization),
            7 => Some(Self::Corruption),
            8 => Some(Self::Unavailable),
            9 => Some(Self::Io),
            10 => Some(Self::Internal),
            _ => None,
        }
    }
}

/// Stable caller retry classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductRetry {
    /// Retrying the same request cannot make it valid.
    Never,
    /// The same idempotent request may be retried unchanged.
    SameRequest,
    /// Rebind against a fresh catalog or snapshot before retrying.
    NewSnapshot,
    /// Retry after the conflicting owner or temporary condition clears.
    AfterBackoff,
    /// Recovery or operator intervention is required before retrying.
    AfterRecovery,
    /// Resolve the transaction identity before deciding whether to retry.
    UnknownCommit,
}

impl ProductRetry {
    /// Returns the stable lowercase ASCII wire identity.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::SameRequest => "same-request",
            Self::NewSnapshot => "new-snapshot",
            Self::AfterBackoff => "after-backoff",
            Self::AfterRecovery => "after-recovery",
            Self::UnknownCommit => "unknown-commit",
        }
    }

    pub(crate) const fn wire_tag(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::SameRequest => 1,
            Self::NewSnapshot => 2,
            Self::AfterBackoff => 3,
            Self::AfterRecovery => 4,
            Self::UnknownCommit => 5,
        }
    }

    pub(crate) const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Never),
            1 => Some(Self::SameRequest),
            2 => Some(Self::NewSnapshot),
            3 => Some(Self::AfterBackoff),
            4 => Some(Self::AfterRecovery),
            5 => Some(Self::UnknownCommit),
            _ => None,
        }
    }
}

/// Public transaction state attached to a product error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductTransactionState {
    /// No transaction is associated with the error.
    None,
    /// The transaction remains active.
    Active,
    /// Rollback is proven.
    RolledBack,
    /// Commit is proven.
    Committed,
    /// Publication may be recovered as committed and must be resolved.
    OutcomeUnknown,
}

impl ProductTransactionState {
    /// Returns the stable lowercase ASCII wire identity.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Active => "active",
            Self::RolledBack => "rolled-back",
            Self::Committed => "committed",
            Self::OutcomeUnknown => "outcome-unknown",
        }
    }

    pub(crate) const fn wire_tag(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Active => 1,
            Self::RolledBack => 2,
            Self::Committed => 3,
            Self::OutcomeUnknown => 4,
        }
    }

    pub(crate) const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::None),
            1 => Some(Self::Active),
            2 => Some(Self::RolledBack),
            3 => Some(Self::Committed),
            4 => Some(Self::OutcomeUnknown),
            _ => None,
        }
    }
}

/// Stable product error code, including an unknown future v1 code.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ProductErrorCode {
    /// The target path already exists.
    DataDirectoryExists,
    /// Another handle owns the data directory.
    DataDirectoryLocked,
    /// A native directory marker is missing or malformed.
    InvalidDataDirectory,
    /// A format-2 directory cannot be opened as native.
    Format2Directory,
    /// The requested catalog object is absent.
    CatalogObjectNotFound,
    /// SQL text is invalid or outside the bounded grammar.
    SqlInvalidSyntax,
    /// SQL parameter count or values differ from the prepared plan.
    SqlParameterMismatch,
    /// A prepared statement must be rebound to the current catalog.
    SqlCatalogChanged,
    /// A prepared statement belongs to another native directory lineage.
    SqlForeignPrepared,
    /// A SQL catalog identity is absent or invalid.
    SqlUnknownObject,
    /// A SQL value or nullability rule is invalid.
    SqlInvalidValue,
    /// No admitted physical SQL access path exists.
    SqlNoAccessPath,
    /// A SQL uniqueness rule rejected the write.
    SqlUniqueViolation,
    /// A SQL check constraint rejected the write.
    SqlCheckViolation,
    /// A SQL foreign key rejected the write.
    SqlForeignKeyViolation,
    /// Snapshot isolation rejected a stale writer.
    WriteConflict,
    /// A requested native object is absent.
    ObjectNotFound,
    /// A request exceeds a bounded product limit.
    LimitExceeded,
    /// Durable native state is corrupt or inconsistent.
    Corruption,
    /// A filesystem or device operation failed.
    Io,
    /// A failure has not yet received a narrower stable mapping.
    Internal,
    /// A request is malformed or violates a general product precondition.
    InvalidRequest,
    /// Current catalog authority conflicts with the requested change.
    CatalogConflict,
    /// An idempotency identity was previously committed for another request.
    IdempotencyConflict,
    /// A one-time key-start response was already delivered for this token.
    SecretDeliveryConsumed,
    /// API-key activation confirmation did not match the pending verifier.
    ConfirmationDigestMismatch,
    /// The request deadline elapsed before a definite result.
    DeadlineExceeded,
    /// The caller cancelled the request before a definite result.
    Cancelled,
    /// The authenticated caller is not authorized for the operation.
    AuthorizationDenied,
    /// The product cannot currently accept the request.
    Unavailable,
    /// Publication may have committed and must be resolved by transaction ID.
    UnknownCommit,
    /// Backup authority is malformed or fails verification.
    BackupInvalid,
    /// Durable metadata is valid but requires an explicit local upgrade.
    UpgradeRequired,
    /// A future v1 code not recognized by this build.
    Unknown(ProductErrorIdentifier),
}

impl ProductErrorCode {
    /// Returns the stable lowercase ASCII wire identity.
    pub fn as_str(&self) -> &str {
        match self {
            Self::DataDirectoryExists => "data_directory_exists",
            Self::DataDirectoryLocked => "data_directory_locked",
            Self::InvalidDataDirectory => "invalid_data_directory",
            Self::Format2Directory => "format2_directory",
            Self::CatalogObjectNotFound => "catalog_object_not_found",
            Self::SqlInvalidSyntax => "sql_invalid_syntax",
            Self::SqlParameterMismatch => "sql_parameter_mismatch",
            Self::SqlCatalogChanged => "sql_catalog_changed",
            Self::SqlForeignPrepared => "sql_foreign_prepared",
            Self::SqlUnknownObject => "sql_unknown_object",
            Self::SqlInvalidValue => "sql_invalid_value",
            Self::SqlNoAccessPath => "sql_no_access_path",
            Self::SqlUniqueViolation => "sql_unique_violation",
            Self::SqlCheckViolation => "sql_check_violation",
            Self::SqlForeignKeyViolation => "sql_foreign_key_violation",
            Self::WriteConflict => "write_conflict",
            Self::ObjectNotFound => "object_not_found",
            Self::LimitExceeded => "limit_exceeded",
            Self::Corruption => "corruption",
            Self::Io => "io",
            Self::Internal => "internal",
            Self::InvalidRequest => "invalid_request",
            Self::CatalogConflict => "catalog_conflict",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::SecretDeliveryConsumed => "secret_delivery_consumed",
            Self::ConfirmationDigestMismatch => "confirmation_digest_mismatch",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Cancelled => "cancelled",
            Self::AuthorizationDenied => "authorization_denied",
            Self::Unavailable => "unavailable",
            Self::UnknownCommit => "unknown_commit",
            Self::BackupInvalid => "backup_invalid",
            Self::UpgradeRequired => "upgrade_required",
            Self::Unknown(raw) => raw.as_str(),
        }
    }

    /// Parses a known code or retains one checked unknown code verbatim.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` is not a bounded canonical code.
    pub fn from_raw(raw: &str) -> Result<Self, ProductErrorValidationError> {
        Ok(match raw {
            "data_directory_exists" => Self::DataDirectoryExists,
            "data_directory_locked" => Self::DataDirectoryLocked,
            "invalid_data_directory" => Self::InvalidDataDirectory,
            "format2_directory" => Self::Format2Directory,
            "catalog_object_not_found" => Self::CatalogObjectNotFound,
            "sql_invalid_syntax" => Self::SqlInvalidSyntax,
            "sql_parameter_mismatch" => Self::SqlParameterMismatch,
            "sql_catalog_changed" => Self::SqlCatalogChanged,
            "sql_foreign_prepared" => Self::SqlForeignPrepared,
            "sql_unknown_object" => Self::SqlUnknownObject,
            "sql_invalid_value" => Self::SqlInvalidValue,
            "sql_no_access_path" => Self::SqlNoAccessPath,
            "sql_unique_violation" => Self::SqlUniqueViolation,
            "sql_check_violation" => Self::SqlCheckViolation,
            "sql_foreign_key_violation" => Self::SqlForeignKeyViolation,
            "write_conflict" => Self::WriteConflict,
            "object_not_found" => Self::ObjectNotFound,
            "limit_exceeded" => Self::LimitExceeded,
            "corruption" => Self::Corruption,
            "io" => Self::Io,
            "internal" => Self::Internal,
            "invalid_request" => Self::InvalidRequest,
            "catalog_conflict" => Self::CatalogConflict,
            "idempotency_conflict" => Self::IdempotencyConflict,
            "secret_delivery_consumed" => Self::SecretDeliveryConsumed,
            "confirmation_digest_mismatch" => Self::ConfirmationDigestMismatch,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "cancelled" => Self::Cancelled,
            "authorization_denied" => Self::AuthorizationDenied,
            "unavailable" => Self::Unavailable,
            "unknown_commit" => Self::UnknownCommit,
            "backup_invalid" => Self::BackupInvalid,
            "upgrade_required" => Self::UpgradeRequired,
            _ => Self::Unknown(ProductErrorIdentifier::new(raw)?),
        })
    }

    /// Returns whether this build recognizes the code.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

/// Stable definition of one registered v1 product error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductErrorDefinition {
    code: ProductErrorCode,
    category: ProductErrorCategory,
    default_retry: Option<ProductRetry>,
    message: &'static str,
}

impl ProductErrorDefinition {
    /// Returns the stable product error code.
    pub const fn code(self) -> ProductErrorCode {
        self.code
    }

    /// Returns the stable category for this code.
    pub const fn category(self) -> ProductErrorCategory {
        self.category
    }

    /// Returns the stable retry default, or `None` when failure details decide it.
    pub const fn default_retry(self) -> Option<ProductRetry> {
        self.default_retry
    }

    /// Returns the fixed redaction-safe message for this code.
    pub const fn message(self) -> &'static str {
        self.message
    }
}

const fn definition(
    code: ProductErrorCode,
    category: ProductErrorCategory,
    default_retry: Option<ProductRetry>,
    message: &'static str,
) -> ProductErrorDefinition {
    ProductErrorDefinition {
        code,
        category,
        default_retry,
        message,
    }
}

/// Complete append-only v1 registry and deterministic defaults.
pub const PRODUCT_ERROR_REGISTRY_V1: &[ProductErrorDefinition] = &[
    definition(
        ProductErrorCode::DataDirectoryExists,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native data directory already exists",
    ),
    definition(
        ProductErrorCode::DataDirectoryLocked,
        ProductErrorCategory::Unavailable,
        Some(ProductRetry::AfterBackoff),
        "native data directory is already locked",
    ),
    definition(
        ProductErrorCode::InvalidDataDirectory,
        ProductErrorCategory::Corruption,
        Some(ProductRetry::AfterRecovery),
        "native data directory is invalid",
    ),
    definition(
        ProductErrorCode::Format2Directory,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "format-2 directory cannot be opened as native",
    ),
    definition(
        ProductErrorCode::CatalogObjectNotFound,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
        "native catalog object does not exist",
    ),
    definition(
        ProductErrorCode::SqlInvalidSyntax,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "native SQL syntax is invalid or unsupported",
    ),
    definition(
        ProductErrorCode::SqlParameterMismatch,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "native SQL parameters do not match the prepared plan",
    ),
    definition(
        ProductErrorCode::SqlCatalogChanged,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::NewSnapshot),
        "native SQL prepared plan requires rebind",
    ),
    definition(
        ProductErrorCode::SqlForeignPrepared,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native SQL prepared plan belongs to another directory",
    ),
    definition(
        ProductErrorCode::SqlUnknownObject,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
        "native SQL catalog object does not exist",
    ),
    definition(
        ProductErrorCode::SqlInvalidValue,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "native SQL value or binding is invalid",
    ),
    definition(
        ProductErrorCode::SqlNoAccessPath,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "native SQL query has no admitted access path",
    ),
    definition(
        ProductErrorCode::SqlUniqueViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native SQL unique constraint failed",
    ),
    definition(
        ProductErrorCode::SqlCheckViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native SQL check constraint failed",
    ),
    definition(
        ProductErrorCode::SqlForeignKeyViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native SQL foreign key constraint failed",
    ),
    definition(
        ProductErrorCode::WriteConflict,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::NewSnapshot),
        "native write conflicts with a committed transaction",
    ),
    definition(
        ProductErrorCode::ObjectNotFound,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
        "native object does not exist",
    ),
    definition(
        ProductErrorCode::LimitExceeded,
        ProductErrorCategory::Limit,
        Some(ProductRetry::Never),
        "native request exceeds a product limit",
    ),
    definition(
        ProductErrorCode::Corruption,
        ProductErrorCategory::Corruption,
        Some(ProductRetry::AfterRecovery),
        "native durable state is invalid",
    ),
    definition(
        ProductErrorCode::Io,
        ProductErrorCategory::Io,
        None,
        "native filesystem operation failed",
    ),
    definition(
        ProductErrorCode::Internal,
        ProductErrorCategory::Internal,
        Some(ProductRetry::Never),
        "native product operation failed",
    ),
    definition(
        ProductErrorCode::InvalidRequest,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
        "native product request is invalid",
    ),
    definition(
        ProductErrorCode::CatalogConflict,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::NewSnapshot),
        "native catalog authority conflicts with the request",
    ),
    definition(
        ProductErrorCode::DeadlineExceeded,
        ProductErrorCategory::Deadline,
        Some(ProductRetry::SameRequest),
        "native product request deadline exceeded",
    ),
    definition(
        ProductErrorCode::Cancelled,
        ProductErrorCategory::Cancelled,
        Some(ProductRetry::SameRequest),
        "native product request was cancelled",
    ),
    definition(
        ProductErrorCode::AuthorizationDenied,
        ProductErrorCategory::Authorization,
        Some(ProductRetry::Never),
        "native product operation is not authorized",
    ),
    definition(
        ProductErrorCode::Unavailable,
        ProductErrorCategory::Unavailable,
        Some(ProductRetry::AfterBackoff),
        "native product is temporarily unavailable",
    ),
    definition(
        ProductErrorCode::UnknownCommit,
        ProductErrorCategory::Unavailable,
        Some(ProductRetry::UnknownCommit),
        "native transaction publication outcome is unknown",
    ),
    definition(
        ProductErrorCode::BackupInvalid,
        ProductErrorCategory::Corruption,
        Some(ProductRetry::AfterRecovery),
        "native backup authority is invalid",
    ),
    definition(
        ProductErrorCode::IdempotencyConflict,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "native idempotency identity conflicts with the request",
    ),
    definition(
        ProductErrorCode::SecretDeliveryConsumed,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
        "API key secret delivery was already consumed",
    ),
    definition(
        ProductErrorCode::ConfirmationDigestMismatch,
        ProductErrorCategory::Authorization,
        Some(ProductRetry::Never),
        "API key activation confirmation does not match",
    ),
    definition(
        ProductErrorCode::UpgradeRequired,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::AfterRecovery),
        "native durable metadata requires explicit upgrade",
    ),
];

fn registered_definition(code: ProductErrorCode) -> Option<ProductErrorDefinition> {
    PRODUCT_ERROR_REGISTRY_V1
        .iter()
        .copied()
        .find(|definition| definition.code == code)
}

/// Typed identity for a configured product limit.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ProductLimitKind {
    /// SQL statement UTF-8 bytes.
    SqlStatementBytes,
    /// SQL parameter positions.
    SqlParameters,
    /// Materialized SQL result rows.
    SqlResultRows,
    /// Visible left candidates consumed by one bounded SQL join.
    SqlJoinCandidates,
    /// Physical candidates consumed by one bounded SQL scan.
    SqlScanCandidates,
    /// Request payload bytes.
    RequestBytes,
    /// Response payload bytes.
    ResponseBytes,
    /// Hash field positions in one operation.
    HashFieldBatchItems,
    /// Set member positions in one operation.
    SetMemberBatchItems,
    /// Keys admitted by one expiry sweep.
    ExpirySweepKeys,
    /// Transactions admitted by one group commit.
    GroupCommitTransactions,
    /// A future bounded limit not recognized by this build.
    Unknown(ProductErrorIdentifier),
}

impl ProductLimitKind {
    /// Returns the stable lowercase ASCII limit identity.
    pub fn as_str(&self) -> &str {
        match self {
            Self::SqlStatementBytes => "sql_statement_bytes",
            Self::SqlParameters => "sql_parameters",
            Self::SqlResultRows => "sql_result_rows",
            Self::SqlJoinCandidates => "sql_join_candidates",
            Self::SqlScanCandidates => "sql_scan_candidates",
            Self::RequestBytes => "request_bytes",
            Self::ResponseBytes => "response_bytes",
            Self::HashFieldBatchItems => "hash_field_batch_items",
            Self::SetMemberBatchItems => "set_member_batch_items",
            Self::ExpirySweepKeys => "expiry_sweep_keys",
            Self::GroupCommitTransactions => "group_commit_transactions",
            Self::Unknown(raw) => raw.as_str(),
        }
    }

    /// Parses a known kind or retains a checked unknown kind.
    ///
    /// # Errors
    ///
    /// Returns an error when an unknown kind is not a canonical bounded
    /// lowercase product identifier.
    pub fn from_raw(raw: &str) -> Result<Self, ProductErrorValidationError> {
        Ok(match raw {
            "sql_statement_bytes" => Self::SqlStatementBytes,
            "sql_parameters" => Self::SqlParameters,
            "sql_result_rows" => Self::SqlResultRows,
            "sql_join_candidates" => Self::SqlJoinCandidates,
            "sql_scan_candidates" => Self::SqlScanCandidates,
            "request_bytes" => Self::RequestBytes,
            "response_bytes" => Self::ResponseBytes,
            "hash_field_batch_items" => Self::HashFieldBatchItems,
            "set_member_batch_items" => Self::SetMemberBatchItems,
            "expiry_sweep_keys" => Self::ExpirySweepKeys,
            "group_commit_transactions" => Self::GroupCommitTransactions,
            _ => Self::Unknown(ProductErrorIdentifier::new(raw)?),
        })
    }
}

/// Configured and observed values for one rejected product limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductLimit {
    kind: ProductLimitKind,
    configured: u64,
    observed: u64,
}

impl ProductLimit {
    /// Constructs typed limit evidence.
    pub const fn new(kind: ProductLimitKind, configured: u64, observed: u64) -> Self {
        Self {
            kind,
            configured,
            observed,
        }
    }

    /// Returns the rejected limit identity.
    pub const fn kind(self) -> ProductLimitKind {
        self.kind
    }

    /// Returns the configured maximum.
    pub const fn configured(self) -> u64 {
        self.configured
    }

    /// Returns the observed request value.
    pub const fn observed(self) -> u64 {
        self.observed
    }
}

/// Byte offsets into caller-supplied text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductSourceSpan {
    start: u32,
    end: u32,
}

impl ProductSourceSpan {
    /// Constructs a half-open source span.
    ///
    /// # Errors
    ///
    /// Returns an error when `end` precedes `start`.
    pub const fn new(start: u32, end: u32) -> Result<Self, ProductErrorValidationError> {
        if end < start {
            return Err(ProductErrorValidationError::InvalidSourceSpan);
        }
        Ok(Self { start, end })
    }

    /// Returns the inclusive start byte offset.
    pub const fn start(self) -> u32 {
        self.start
    }

    /// Returns the exclusive end byte offset.
    pub const fn end(self) -> u32 {
        self.end
    }
}

/// Native SQL diagnostic subcode retained as typed product detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductSqlSubcode {
    /// `HYSQL001`: invalid or unsupported syntax.
    Hysql001,
    /// `HYSQL002`: parameter mismatch.
    Hysql002,
    /// `HYSQL003`: prepared-plan catalog change.
    Hysql003,
    /// `HYSQL004`: unknown column.
    Hysql004,
    /// `HYSQL005`: duplicate column.
    Hysql005,
    /// `HYSQL006`: type mismatch.
    Hysql006,
    /// `HYSQL007`: nullability violation.
    Hysql007,
    /// `HYSQL008`: invalid primary-key binding.
    Hysql008,
    /// `HYSQL009`: invalid stored row.
    Hysql009,
    /// `HYSQL010`: invalid catalog object.
    Hysql010,
    /// `HYSQL011`: no admitted access path.
    Hysql011,
    /// `HYSQL012`: uniqueness violation.
    Hysql012,
    /// `HYSQL013`: unsupported primary-key mutation.
    Hysql013,
    /// `HYSQL014`: invalid secondary-index range.
    Hysql014,
    /// `HYSQL015`: check-constraint violation.
    Hysql015,
    /// `HYSQL016`: foreign-key violation.
    Hysql016,
    /// `HYSQL017`: unknown relation.
    Hysql017,
    /// `HYSQL018`: bounded join candidate budget exhausted.
    Hysql018,
    /// `HYSQL019`: bounded scan candidate budget exhausted.
    Hysql019,
    /// `HYSQL020`: multi-row INSERT row budget exhausted.
    Hysql020,
    /// `HYSQL021`: invalid aggregate binding.
    Hysql021,
    /// `HYSQL022`: aggregate accumulator overflow.
    Hysql022,
}

impl ProductSqlSubcode {
    /// Returns the exact native SQL subcode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hysql001 => "HYSQL001",
            Self::Hysql002 => "HYSQL002",
            Self::Hysql003 => "HYSQL003",
            Self::Hysql004 => "HYSQL004",
            Self::Hysql005 => "HYSQL005",
            Self::Hysql006 => "HYSQL006",
            Self::Hysql007 => "HYSQL007",
            Self::Hysql008 => "HYSQL008",
            Self::Hysql009 => "HYSQL009",
            Self::Hysql010 => "HYSQL010",
            Self::Hysql011 => "HYSQL011",
            Self::Hysql012 => "HYSQL012",
            Self::Hysql013 => "HYSQL013",
            Self::Hysql014 => "HYSQL014",
            Self::Hysql015 => "HYSQL015",
            Self::Hysql016 => "HYSQL016",
            Self::Hysql017 => "HYSQL017",
            Self::Hysql018 => "HYSQL018",
            Self::Hysql019 => "HYSQL019",
            Self::Hysql020 => "HYSQL020",
            Self::Hysql021 => "HYSQL021",
            Self::Hysql022 => "HYSQL022",
        }
    }

    pub(crate) const fn from_raw(raw: &str) -> Option<Self> {
        match raw.as_bytes() {
            b"HYSQL001" => Some(Self::Hysql001),
            b"HYSQL002" => Some(Self::Hysql002),
            b"HYSQL003" => Some(Self::Hysql003),
            b"HYSQL004" => Some(Self::Hysql004),
            b"HYSQL005" => Some(Self::Hysql005),
            b"HYSQL006" => Some(Self::Hysql006),
            b"HYSQL007" => Some(Self::Hysql007),
            b"HYSQL008" => Some(Self::Hysql008),
            b"HYSQL009" => Some(Self::Hysql009),
            b"HYSQL010" => Some(Self::Hysql010),
            b"HYSQL011" => Some(Self::Hysql011),
            b"HYSQL012" => Some(Self::Hysql012),
            b"HYSQL013" => Some(Self::Hysql013),
            b"HYSQL014" => Some(Self::Hysql014),
            b"HYSQL015" => Some(Self::Hysql015),
            b"HYSQL016" => Some(Self::Hysql016),
            b"HYSQL017" => Some(Self::Hysql017),
            b"HYSQL018" => Some(Self::Hysql018),
            b"HYSQL019" => Some(Self::Hysql019),
            b"HYSQL020" => Some(Self::Hysql020),
            b"HYSQL021" => Some(Self::Hysql021),
            b"HYSQL022" => Some(Self::Hysql022),
            _ => None,
        }
    }
}

/// One bounded unknown future detail TLV.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductUnknownDetail {
    tag: u16,
    value: Box<[u8]>,
}

impl ProductUnknownDetail {
    /// Constructs one unknown detail field.
    ///
    /// Tags `1` and `2` are reserved for SQL subcode and transaction ID.
    ///
    /// # Errors
    ///
    /// Returns an error for a reserved tag or oversized value.
    pub fn new(tag: u16, value: &[u8]) -> Result<Self, ProductErrorValidationError> {
        if tag <= 2 {
            return Err(ProductErrorValidationError::ReservedDetailTag);
        }
        if value.len() > MAX_PRODUCT_ERROR_DETAIL_BYTES {
            return Err(ProductErrorValidationError::DetailTooLarge);
        }
        Ok(Self {
            tag,
            value: value.into(),
        })
    }

    /// Returns the unrecognized detail tag.
    pub const fn tag(&self) -> u16 {
        self.tag
    }

    /// Returns the exact retained detail payload.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// Bounded typed code-specific details.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProductErrorDetails {
    sql_subcode: Option<ProductSqlSubcode>,
    transaction_id: Option<TransactionId>,
    unknown: Vec<ProductUnknownDetail>,
}

impl ProductErrorDetails {
    /// Returns the native SQL subcode, when applicable.
    pub const fn sql_subcode(&self) -> Option<ProductSqlSubcode> {
        self.sql_subcode
    }

    /// Returns the transaction identity needed for status resolution.
    pub const fn transaction_id(&self) -> Option<TransactionId> {
        self.transaction_id
    }

    /// Returns unknown future details in ascending tag order.
    pub fn unknown(&self) -> &[ProductUnknownDetail] {
        &self.unknown
    }

    pub(crate) fn set_sql_subcode(&mut self, subcode: ProductSqlSubcode) {
        self.sql_subcode = Some(subcode);
    }

    pub(crate) fn set_transaction_id(&mut self, transaction_id: TransactionId) {
        self.transaction_id = Some(transaction_id);
    }

    pub(crate) fn insert_unknown(
        &mut self,
        detail: ProductUnknownDetail,
    ) -> Result<(), ProductErrorValidationError> {
        if self.unknown.len() >= MAX_PRODUCT_ERROR_UNKNOWN_DETAILS {
            return Err(ProductErrorValidationError::TooManyDetails);
        }
        match self
            .unknown
            .binary_search_by_key(&detail.tag, ProductUnknownDetail::tag)
        {
            Ok(_) => Err(ProductErrorValidationError::DuplicateDetail),
            Err(position) => {
                self.unknown.insert(position, detail);
                Ok(())
            }
        }
    }

    pub(crate) fn field_count(&self) -> usize {
        usize::from(self.sql_subcode.is_some())
            + usize::from(self.transaction_id.is_some())
            + self.unknown.len()
    }
}

/// Failure while constructing a bounded product error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductErrorValidationError {
    /// A code, limit name, or other identifier is not canonical.
    InvalidIdentifier,
    /// A message is empty, oversized, or contains control characters.
    InvalidMessage,
    /// A source span ends before it starts.
    InvalidSourceSpan,
    /// An unknown detail attempts to use a known typed tag.
    ReservedDetailTag,
    /// One detail payload exceeds its fixed byte bound.
    DetailTooLarge,
    /// The number of retained unknown details exceeds its fixed bound.
    TooManyDetails,
    /// A detail tag is present more than once.
    DuplicateDetail,
    /// Category, retry, or message conflicts with a registered code.
    InconsistentKnownCode,
    /// Transaction state, retry, code, and transaction ID conflict.
    InconsistentTransactionState,
}

impl fmt::Display for ProductErrorValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentifier => "product error identifier is invalid",
            Self::InvalidMessage => "product error message is invalid",
            Self::InvalidSourceSpan => "product error source span is invalid",
            Self::ReservedDetailTag => "product error detail tag is reserved",
            Self::DetailTooLarge => "product error detail is too large",
            Self::TooManyDetails => "product error has too many details",
            Self::DuplicateDetail => "product error detail tag is duplicated",
            Self::InconsistentKnownCode => "product error fields conflict with the code registry",
            Self::InconsistentTransactionState => {
                "product error transaction fields are inconsistent"
            }
        })
    }
}

impl std::error::Error for ProductErrorValidationError {}

/// Transport-independent bounded product error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductError {
    code: ProductErrorCode,
    category: ProductErrorCategory,
    retry: ProductRetry,
    message: Box<str>,
    object_id: Option<ObjectId>,
    request_id: Option<u128>,
    trace_id: Option<u128>,
    transaction_state: ProductTransactionState,
    limit: Option<ProductLimit>,
    source_span: Option<ProductSourceSpan>,
    details: ProductErrorDetails,
}

impl ProductError {
    /// Constructs an error from one registered code and its fixed defaults.
    ///
    pub fn from_code(code: ProductErrorCode) -> Self {
        let definition = registered_definition(code).unwrap_or(ProductErrorDefinition {
            code: ProductErrorCode::Internal,
            category: ProductErrorCategory::Internal,
            message: "internal product error",
            default_retry: Some(ProductRetry::AfterRecovery),
        });
        let retry = definition
            .default_retry
            .unwrap_or(ProductRetry::AfterRecovery);
        Self::from_parts(definition, retry)
    }

    fn from_parts(definition: ProductErrorDefinition, retry: ProductRetry) -> Self {
        Self {
            code: definition.code,
            category: definition.category,
            retry,
            message: definition.message.into(),
            object_id: None,
            request_id: None,
            trace_id: None,
            transaction_state: ProductTransactionState::None,
            limit: None,
            source_span: None,
            details: ProductErrorDetails::default(),
        }
    }

    /// Constructs a bounded error while enforcing known-code registry fields.
    ///
    /// This is primarily for adapters decoding errors produced by a newer
    /// build. Product producers should normally use [`Self::from_code`].
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe message or fields inconsistent with a
    /// known code.
    pub fn try_new(
        code: ProductErrorCode,
        category: ProductErrorCategory,
        retry: ProductRetry,
        message: &str,
    ) -> Result<Self, ProductErrorValidationError> {
        validate_message(message)?;
        if let Some(definition) = registered_definition(code)
            && (category != definition.category
                || definition.default_retry.is_some_and(|value| value != retry)
                || message != definition.message)
        {
            return Err(ProductErrorValidationError::InconsistentKnownCode);
        }
        Ok(Self {
            code,
            category,
            retry,
            message: message.into(),
            object_id: None,
            request_id: None,
            trace_id: None,
            transaction_state: ProductTransactionState::None,
            limit: None,
            source_span: None,
            details: ProductErrorDetails::default(),
        })
    }

    /// Returns the stable product code.
    pub const fn code(&self) -> ProductErrorCode {
        self.code
    }

    /// Returns the broad error category.
    pub const fn category(&self) -> ProductErrorCategory {
        self.category
    }

    /// Returns the caller retry classification.
    pub const fn retry(&self) -> ProductRetry {
        self.retry
    }

    /// Returns one bounded, safe message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the affected catalog object when the mapping can prove it.
    pub const fn object_id(&self) -> Option<ObjectId> {
        self.object_id
    }

    /// Returns the stable product request identity when assigned.
    pub const fn request_id(&self) -> Option<u128> {
        self.request_id
    }

    /// Returns the local diagnostic trace identity when assigned.
    pub const fn trace_id(&self) -> Option<u128> {
        self.trace_id
    }

    /// Returns the transaction outcome known at the failure boundary.
    pub const fn transaction_state(&self) -> ProductTransactionState {
        self.transaction_state
    }

    /// Returns typed configured-limit evidence.
    pub const fn limit(&self) -> Option<ProductLimit> {
        self.limit
    }

    /// Returns a source span into caller-supplied text.
    pub const fn source_span(&self) -> Option<ProductSourceSpan> {
        self.source_span
    }

    /// Returns bounded code-specific details.
    pub const fn details(&self) -> &ProductErrorDetails {
        &self.details
    }

    /// Attaches the stable request identity.
    #[must_use]
    pub const fn with_request_id(mut self, request_id: u128) -> Self {
        self.request_id = Some(request_id);
        self
    }

    /// Attaches the local diagnostic trace identity.
    #[must_use]
    pub const fn with_trace_id(mut self, trace_id: u128) -> Self {
        self.trace_id = Some(trace_id);
        self
    }

    /// Attaches the affected catalog object identity.
    #[must_use]
    pub const fn with_object_id(mut self, object_id: ObjectId) -> Self {
        self.object_id = Some(object_id);
        self
    }

    /// Attaches typed configured-limit evidence.
    #[must_use]
    pub const fn with_limit(mut self, limit: ProductLimit) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Attaches a checked source span.
    #[must_use]
    pub const fn with_source_span(mut self, source_span: ProductSourceSpan) -> Self {
        self.source_span = Some(source_span);
        self
    }

    /// Attaches a native SQL subcode.
    #[must_use]
    pub fn with_sql_subcode(mut self, subcode: ProductSqlSubcode) -> Self {
        self.details.set_sql_subcode(subcode);
        self
    }

    /// Attaches a transaction identity.
    #[must_use]
    pub fn with_transaction_id(mut self, transaction_id: TransactionId) -> Self {
        self.details.set_transaction_id(transaction_id);
        self
    }

    /// Retains one bounded unknown future detail.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate fields or when the detail bound is full.
    pub fn with_unknown_detail(
        mut self,
        detail: ProductUnknownDetail,
    ) -> Result<Self, ProductErrorValidationError> {
        self.details.insert_unknown(detail)?;
        Ok(self)
    }

    /// Applies transaction outcome semantics at a failure boundary.
    #[must_use]
    pub fn at_failure_boundary(mut self, boundary: ProductFailureBoundary) -> Self {
        self.transaction_state = boundary.transaction_state;
        if let Some(transaction_id) = boundary.transaction_id {
            self.details.set_transaction_id(transaction_id);
        }
        if boundary.transaction_state == ProductTransactionState::OutcomeUnknown {
            self.code = ProductErrorCode::UnknownCommit;
            self.category = ProductErrorCategory::Unavailable;
            self.retry = ProductRetry::UnknownCommit;
            self.message = "native transaction publication outcome is unknown".into();
        }
        self
    }

    /// Validates cross-field transaction invariants used by canonical codecs.
    pub(crate) fn validate_transaction_fields(&self) -> Result<(), ProductErrorValidationError> {
        let has_transaction = self.details.transaction_id.is_some();
        if (self.transaction_state == ProductTransactionState::None && has_transaction)
            || (self.transaction_state != ProductTransactionState::None && !has_transaction)
            || (self.transaction_state == ProductTransactionState::OutcomeUnknown
                && (self.retry != ProductRetry::UnknownCommit
                    || (self.code.is_known() && self.code != ProductErrorCode::UnknownCommit)))
            || (self.transaction_state != ProductTransactionState::OutcomeUnknown
                && self.retry == ProductRetry::UnknownCommit)
        {
            return Err(ProductErrorValidationError::InconsistentTransactionState);
        }
        Ok(())
    }

    pub(crate) fn set_transaction_state(
        &mut self,
        state: ProductTransactionState,
    ) -> Result<(), ProductErrorValidationError> {
        self.transaction_state = state;
        self.validate_transaction_fields()
    }

    pub(crate) fn set_details(&mut self, details: ProductErrorDetails) {
        self.details = details;
    }
}

fn validate_message(message: &str) -> Result<(), ProductErrorValidationError> {
    if message.is_empty()
        || message.len() > MAX_PRODUCT_ERROR_MESSAGE_BYTES
        || message.chars().any(char::is_control)
    {
        return Err(ProductErrorValidationError::InvalidMessage);
    }
    Ok(())
}

impl fmt::Display for ProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for ProductError {}

/// Proven transaction state at the point an error leaves a product surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductFailureBoundary {
    transaction_state: ProductTransactionState,
    transaction_id: Option<TransactionId>,
}

impl ProductFailureBoundary {
    /// Constructs a failure unrelated to a transaction.
    pub const fn request() -> Self {
        Self {
            transaction_state: ProductTransactionState::None,
            transaction_id: None,
        }
    }

    /// Constructs a failure that leaves a transaction active.
    pub const fn active(transaction_id: TransactionId) -> Self {
        Self {
            transaction_state: ProductTransactionState::Active,
            transaction_id: Some(transaction_id),
        }
    }

    /// Constructs a failure after rollback is proven.
    pub const fn rolled_back(transaction_id: TransactionId) -> Self {
        Self {
            transaction_state: ProductTransactionState::RolledBack,
            transaction_id: Some(transaction_id),
        }
    }

    /// Constructs a response-delivery failure after commit is proven.
    pub const fn committed(transaction_id: TransactionId) -> Self {
        Self {
            transaction_state: ProductTransactionState::Committed,
            transaction_id: Some(transaction_id),
        }
    }

    /// Constructs a failure after publication may have happened.
    ///
    /// Applying this boundary always produces `unknown_commit`, retry
    /// `unknown-commit`, and transaction state `outcome-unknown`.
    pub const fn publication_unknown(transaction_id: TransactionId) -> Self {
        Self {
            transaction_state: ProductTransactionState::OutcomeUnknown,
            transaction_id: Some(transaction_id),
        }
    }

    /// Applies this boundary to an error.
    pub fn apply(self, error: ProductError) -> ProductError {
        error.at_failure_boundary(self)
    }
}

impl From<NativeRuntimeError> for ProductError {
    #[allow(clippy::too_many_lines)]
    fn from(source: NativeRuntimeError) -> Self {
        if let Some(kind) = source.io_error_kind() {
            let mut error = Self::from_code(ProductErrorCode::Io);
            error.retry = io_retry(kind);
            return error;
        }
        if source.is_corruption() {
            return Self::from_code(ProductErrorCode::Corruption);
        }
        match source {
            NativeRuntimeError::DataDirectoryExists => {
                Self::from_code(ProductErrorCode::DataDirectoryExists)
            }
            NativeRuntimeError::Directory(NativeDirectoryError::AlreadyLocked(_)) => {
                Self::from_code(ProductErrorCode::DataDirectoryLocked)
            }
            NativeRuntimeError::Directory(NativeDirectoryError::OfflineOwnerAuthorityDenied) => {
                Self::from_code(ProductErrorCode::AuthorizationDenied)
            }
            NativeRuntimeError::Directory(NativeDirectoryError::Format2Directory(_)) => {
                Self::from_code(ProductErrorCode::Format2Directory)
            }
            NativeRuntimeError::Directory(_) => {
                Self::from_code(ProductErrorCode::InvalidDataDirectory)
            }
            NativeRuntimeError::WriteConflict(_) => {
                Self::from_code(ProductErrorCode::WriteConflict)
            }
            NativeRuntimeError::UnknownRelation { table } => Self::not_found(table),
            NativeRuntimeError::UnknownSecondaryIndex { index }
            | NativeRuntimeError::UnknownVectorIndex { index } => Self::not_found(index),
            NativeRuntimeError::UnknownStructureHash
            | NativeRuntimeError::UnknownStructureSet
            | NativeRuntimeError::UnknownStructureList
            | NativeRuntimeError::UnknownStructureStream
            | NativeRuntimeError::UnknownStructureSortedSet
            | NativeRuntimeError::UnknownSnapshotPin => {
                Self::from_code(ProductErrorCode::ObjectNotFound)
            }
            NativeRuntimeError::HashFieldBatchTooLarge { requested } => Self::limit_exceeded(
                ProductLimitKind::HashFieldBatchItems,
                hyphae_native_runtime::MAX_HASH_FIELD_BATCH_SIZE,
                requested,
            ),
            NativeRuntimeError::SetMemberBatchTooLarge { requested } => Self::limit_exceeded(
                ProductLimitKind::SetMemberBatchItems,
                hyphae_native_runtime::MAX_SET_MEMBER_BATCH_SIZE,
                requested,
            ),
            NativeRuntimeError::InvalidExpirySweepLimit { requested } => Self::limit_exceeded(
                ProductLimitKind::ExpirySweepKeys,
                PRODUCT_MAX_EXPIRY_SWEEP_KEYS,
                requested,
            ),
            NativeRuntimeError::InvalidGroupCommitBatchSize { requested } => Self::limit_exceeded(
                ProductLimitKind::GroupCommitTransactions,
                hyphae_native_runtime::MAX_GROUP_COMMIT_BATCH_SIZE,
                requested,
            ),
            NativeRuntimeError::StructureIdentityTooLarge
            | NativeRuntimeError::SearchIdentityTooLarge => {
                Self::from_code(ProductErrorCode::LimitExceeded)
            }
            NativeRuntimeError::UniqueSecondaryIndexViolation => {
                Self::from_code(ProductErrorCode::SqlUniqueViolation)
            }
            NativeRuntimeError::CheckConstraintViolation => {
                Self::from_code(ProductErrorCode::SqlCheckViolation)
            }
            NativeRuntimeError::ForeignKeyConstraintViolation => {
                Self::from_code(ProductErrorCode::SqlForeignKeyViolation)
            }
            NativeRuntimeError::SnapshotBelowRetentionFloor { .. }
            | NativeRuntimeError::StructureKeyExists
            | NativeRuntimeError::SnapshotPinExists => {
                Self::from_code(ProductErrorCode::CatalogConflict)
            }
            NativeRuntimeError::InvalidPreparedMutation
            | NativeRuntimeError::StructureValueNotInteger
            | NativeRuntimeError::StructureIntegerOverflow
            | NativeRuntimeError::StructureKindMismatch
            | NativeRuntimeError::StructureStreamEntryNotCanonical
            | NativeRuntimeError::StructureScoreNotCanonical
            | NativeRuntimeError::DuplicateHashField
            | NativeRuntimeError::DuplicateSetMember
            | NativeRuntimeError::LegacyStructureFamilyUnsupported
            | NativeRuntimeError::StructureCompactionUnsupported
            | NativeRuntimeError::SearchCompactionUnsupported
            | NativeRuntimeError::NoCommittedState
            | NativeRuntimeError::GroupCommitRequiresGroupDurability => {
                Self::from_code(ProductErrorCode::InvalidRequest)
            }
            NativeRuntimeError::InvalidCatalogTree
            | NativeRuntimeError::InvalidRelationalTree
            | NativeRuntimeError::InvalidStructureTree
            | NativeRuntimeError::InvalidSearchTree
            | NativeRuntimeError::InvalidAnnTree
            | NativeRuntimeError::InvalidCommittedRoot
            | NativeRuntimeError::InvalidCheckpoint
            | NativeRuntimeError::NoncontiguousCommitSequence
            | NativeRuntimeError::FuturePage => Self::from_code(ProductErrorCode::Corruption),
            _ => Self::from_code(ProductErrorCode::Internal),
        }
    }
}

fn io_retry(kind: io::ErrorKind) -> ProductRetry {
    match kind {
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            ProductRetry::AfterBackoff
        }
        _ => ProductRetry::AfterRecovery,
    }
}

impl From<SqlError> for ProductError {
    #[allow(clippy::too_many_lines)]
    fn from(source: SqlError) -> Self {
        match source {
            SqlError::InvalidSyntax => Self::sql(
                ProductErrorCode::SqlInvalidSyntax,
                ProductSqlSubcode::Hysql001,
            ),
            SqlError::ParameterMismatch => Self::sql(
                ProductErrorCode::SqlParameterMismatch,
                ProductSqlSubcode::Hysql002,
            ),
            SqlError::CatalogChanged => Self::sql(
                ProductErrorCode::SqlCatalogChanged,
                ProductSqlSubcode::Hysql003,
            ),
            SqlError::UnknownColumn => Self::sql(
                ProductErrorCode::SqlUnknownObject,
                ProductSqlSubcode::Hysql004,
            ),
            SqlError::DuplicateColumn => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql005,
            ),
            SqlError::TypeMismatch => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql006,
            ),
            SqlError::NullViolation => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql007,
            ),
            SqlError::InvalidPrimaryKey => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql008,
            ),
            SqlError::InvalidStoredRow => {
                Self::sql(ProductErrorCode::Corruption, ProductSqlSubcode::Hysql009)
            }
            SqlError::InvalidCatalogObject => {
                Self::sql(ProductErrorCode::Corruption, ProductSqlSubcode::Hysql010)
            }
            SqlError::NoAccessPath => Self::sql(
                ProductErrorCode::SqlNoAccessPath,
                ProductSqlSubcode::Hysql011,
            ),
            SqlError::UniqueViolation => Self::sql(
                ProductErrorCode::SqlUniqueViolation,
                ProductSqlSubcode::Hysql012,
            ),
            SqlError::PrimaryKeyMutationUnsupported => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql013,
            ),
            SqlError::InvalidSecondaryIndexRange => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql014,
            ),
            SqlError::CheckViolation => Self::sql(
                ProductErrorCode::SqlCheckViolation,
                ProductSqlSubcode::Hysql015,
            ),
            SqlError::ForeignKeyViolation => Self::sql(
                ProductErrorCode::SqlForeignKeyViolation,
                ProductSqlSubcode::Hysql016,
            ),
            SqlError::UnknownRelation => Self::sql(
                ProductErrorCode::SqlUnknownObject,
                ProductSqlSubcode::Hysql017,
            ),
            SqlError::JoinCandidateBudgetExceeded => {
                Self::sql(ProductErrorCode::LimitExceeded, ProductSqlSubcode::Hysql018).with_limit(
                    ProductLimit::new(
                        ProductLimitKind::SqlJoinCandidates,
                        usize_to_u64(MAX_SQL_JOIN_CANDIDATES),
                        usize_to_u64(MAX_SQL_JOIN_CANDIDATES.saturating_add(1)),
                    ),
                )
            }
            SqlError::ScanCandidateBudgetExceeded => {
                Self::sql(ProductErrorCode::LimitExceeded, ProductSqlSubcode::Hysql019).with_limit(
                    ProductLimit::new(
                        ProductLimitKind::SqlScanCandidates,
                        usize_to_u64(MAX_SQL_SCAN_CANDIDATES),
                        usize_to_u64(MAX_SQL_SCAN_CANDIDATES.saturating_add(1)),
                    ),
                )
            }
            SqlError::InsertRowBudgetExceeded => {
                Self::sql(ProductErrorCode::LimitExceeded, ProductSqlSubcode::Hysql020).with_limit(
                    ProductLimit::new(
                        ProductLimitKind::SqlStatementBytes,
                        usize_to_u64(hyphae_native_runtime::MAX_SQL_INSERT_ROWS),
                        usize_to_u64(hyphae_native_runtime::MAX_SQL_INSERT_ROWS.saturating_add(1)),
                    ),
                )
            }
            SqlError::InvalidAggregate => Self::sql(
                ProductErrorCode::SqlInvalidSyntax,
                ProductSqlSubcode::Hysql021,
            ),
            SqlError::AggregateOverflow => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductSqlSubcode::Hysql022,
            ),
            SqlError::ExecutionInterrupted => Self::from_code(ProductErrorCode::Cancelled),
            SqlError::Runtime(source) => source.into(),
        }
    }
}

impl ProductError {
    pub(crate) fn catalog_object_not_found(object_id: Option<ObjectId>) -> Self {
        let error = Self::from_code(ProductErrorCode::CatalogObjectNotFound);
        match object_id {
            Some(object_id) => error.with_object_id(object_id),
            None => error,
        }
    }

    pub(crate) fn foreign_prepared() -> Self {
        Self::from_code(ProductErrorCode::SqlForeignPrepared)
    }

    pub(crate) fn sql_statement_limit(configured: usize, observed: usize) -> Self {
        Self::limit_exceeded(ProductLimitKind::SqlStatementBytes, configured, observed)
    }

    pub(crate) fn sql_parameter_limit(configured: usize, observed: usize) -> Self {
        Self::limit_exceeded(ProductLimitKind::SqlParameters, configured, observed)
    }

    pub(crate) fn sql_row_limit(configured: usize, observed: usize) -> Self {
        Self::limit_exceeded(ProductLimitKind::SqlResultRows, configured, observed)
    }

    fn not_found(object_id: ObjectId) -> Self {
        Self::from_code(ProductErrorCode::ObjectNotFound).with_object_id(object_id)
    }

    fn sql(code: ProductErrorCode, subcode: ProductSqlSubcode) -> Self {
        Self::from_code(code).with_sql_subcode(subcode)
    }

    fn limit_exceeded(kind: ProductLimitKind, configured: usize, observed: usize) -> Self {
        Self::from_code(ProductErrorCode::LimitExceeded).with_limit(ProductLimit::new(
            kind,
            usize_to_u64(configured),
            usize_to_u64(observed),
        ))
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_codes_are_bounded_and_preserved() -> Result<(), Box<dyn std::error::Error>> {
        let code = ProductErrorCode::from_raw("future_failure")?;
        assert_eq!(code.as_str(), "future_failure");
        assert!(!code.is_known());
        assert_eq!(
            ProductErrorCode::from_raw("UPPER_CASE"),
            Err(ProductErrorValidationError::InvalidIdentifier)
        );
        Ok(())
    }

    #[test]
    fn sql_mapping_retains_exact_subcode() {
        let error = ProductError::from(SqlError::ForeignKeyViolation);
        assert_eq!(error.code(), ProductErrorCode::SqlForeignKeyViolation);
        assert_eq!(
            error.details().sql_subcode(),
            Some(ProductSqlSubcode::Hysql016)
        );

        let bounded = ProductError::from(SqlError::JoinCandidateBudgetExceeded);
        assert_eq!(bounded.code(), ProductErrorCode::LimitExceeded);
        assert_eq!(bounded.category(), ProductErrorCategory::Limit);
        assert_eq!(
            bounded.details().sql_subcode(),
            Some(ProductSqlSubcode::Hysql018)
        );
        assert_eq!(
            bounded.limit().map(ProductLimit::kind),
            Some(ProductLimitKind::SqlJoinCandidates)
        );

        let scan = ProductError::from(SqlError::ScanCandidateBudgetExceeded);
        assert_eq!(scan.code(), ProductErrorCode::LimitExceeded);
        assert_eq!(
            scan.details().sql_subcode(),
            Some(ProductSqlSubcode::Hysql019)
        );
        assert_eq!(
            scan.limit().map(ProductLimit::kind),
            Some(ProductLimitKind::SqlScanCandidates)
        );
    }

    #[test]
    fn publication_unknown_replaces_broad_failure_semantics()
    -> Result<(), Box<dyn std::error::Error>> {
        let transaction_id = TransactionId::new(41)?;
        let error = ProductFailureBoundary::publication_unknown(transaction_id)
            .apply(ProductError::from_code(ProductErrorCode::Unavailable).with_request_id(7));
        assert_eq!(error.code(), ProductErrorCode::UnknownCommit);
        assert_eq!(error.retry(), ProductRetry::UnknownCommit);
        assert_eq!(
            error.transaction_state(),
            ProductTransactionState::OutcomeUnknown
        );
        assert_eq!(error.details().transaction_id(), Some(transaction_id));
        assert_eq!(error.request_id(), Some(7));
        Ok(())
    }

    #[test]
    fn runtime_messages_never_include_source_diagnostics() {
        let error = ProductError::from(NativeRuntimeError::WalSemantic(
            "/secret/path token=document-value".to_owned(),
        ));
        assert_eq!(error.message(), "native durable state is invalid");
        assert!(!error.to_string().contains("secret"));
    }
}
