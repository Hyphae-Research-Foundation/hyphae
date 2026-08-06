// SPDX-License-Identifier: Apache-2.0

//! Transport-independent contracts and a curated embedded facade for Hyphae Native.
//!
//! This crate is the first G6 product slice. It deliberately exposes only
//! directory lifecycle, snapshots, catalog point lookup, prepared SQL reads,
//! and scalar structure access. Later G6 slices add the remaining admitted
//! operation families without making transport types product authority.

use std::path::Path;

pub use hyphae_native_catalog::{CatalogName, CatalogObject, QualifiedName};
use hyphae_native_runtime::{
    NativeDatabase, NativeDirectoryError, NativeRuntimeError, NativeSnapshot, PreparedStatement,
    SqlError, SqlResult,
};
pub use hyphae_native_types::{
    CanonicalF32, CanonicalF64, CatalogVersion, Csn, ObjectId, ScalarValue as ProductValue,
};

/// Product-visible TTL state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductTtl {
    /// No visible scalar value exists.
    Missing,
    /// The value has no expiry.
    Persistent,
    /// The value expires after the positive remaining duration.
    RemainingMicros(i64),
}

impl From<hyphae_native_runtime::Ttl> for ProductTtl {
    fn from(value: hyphae_native_runtime::Ttl) -> Self {
        match value {
            hyphae_native_runtime::Ttl::Missing => Self::Missing,
            hyphae_native_runtime::Ttl::Persistent => Self::Persistent,
            hyphae_native_runtime::Ttl::RemainingMicros(remaining) => {
                Self::RemainingMicros(remaining)
            }
        }
    }
}

/// Product-owned wrapper around one catalog-bound SQL plan.
#[derive(Clone, Debug)]
pub struct ProductPreparedStatement {
    directory_lineage: [u8; 24],
    maximum_result_rows: usize,
    inner: PreparedStatement,
}

impl ProductPreparedStatement {
    /// Returns the catalog version used by the binder.
    pub const fn catalog_version(&self) -> CatalogVersion {
        self.inner.catalog_version()
    }

    /// Returns the exact parameter count required by this plan.
    pub fn parameter_count(&self) -> usize {
        self.inner.parameter_count()
    }

    /// Returns the admitted maximum materialized row count.
    pub const fn maximum_result_rows(&self) -> usize {
        self.maximum_result_rows
    }
}

/// Maximum UTF-8 statement bytes admitted by the current embedded product slice.
pub const MAX_PRODUCT_SQL_STATEMENT_BYTES: usize = 64 * 1024;
/// Maximum SQL parameters admitted by the current embedded product slice.
pub const MAX_PRODUCT_SQL_PARAMETERS: usize = 1_024;
/// Maximum rows materialized by the current embedded product slice.
pub const MAX_PRODUCT_SQL_ROWS: usize = 1_024;

/// Transport-independent result of one native SQL execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductSqlResult {
    /// DDL or DML completion.
    Command {
        /// Number of logical rows affected.
        rows_affected: u64,
        /// Stable object identity created by DDL, when applicable.
        object_id: Option<ObjectId>,
    },
    /// Materialized result rows.
    Rows {
        /// Stable output column names.
        columns: Vec<String>,
        /// Rows in executor order.
        rows: Vec<Vec<ProductValue>>,
    },
}

impl From<SqlResult> for ProductSqlResult {
    fn from(value: SqlResult) -> Self {
        match value {
            SqlResult::Command {
                rows_affected,
                object_id,
            } => Self::Command {
                rows_affected,
                object_id,
            },
            SqlResult::Rows { columns, rows } => Self::Rows { columns, rows },
        }
    }
}

/// Product result paired with the exact snapshot identity used to produce it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductRead<T> {
    /// Immutable catalog and CSN identity for this result.
    pub snapshot: SnapshotIdentity,
    /// Logical product value.
    pub value: T,
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

/// Stable product error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
}

/// Stable definition of one registered v1 product error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductErrorDefinition {
    code: ProductErrorCode,
    category: ProductErrorCategory,
    default_retry: Option<ProductRetry>,
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
}

const fn error_definition(
    code: ProductErrorCode,
    category: ProductErrorCategory,
    default_retry: Option<ProductRetry>,
) -> ProductErrorDefinition {
    ProductErrorDefinition {
        code,
        category,
        default_retry,
    }
}

/// Complete v1 registry of stable product error codes and deterministic defaults.
pub const PRODUCT_ERROR_REGISTRY_V1: &[ProductErrorDefinition] = &[
    error_definition(
        ProductErrorCode::DataDirectoryExists,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::DataDirectoryLocked,
        ProductErrorCategory::Unavailable,
        Some(ProductRetry::AfterBackoff),
    ),
    error_definition(
        ProductErrorCode::InvalidDataDirectory,
        ProductErrorCategory::Corruption,
        Some(ProductRetry::AfterRecovery),
    ),
    error_definition(
        ProductErrorCode::Format2Directory,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::CatalogObjectNotFound,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlInvalidSyntax,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlParameterMismatch,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlCatalogChanged,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::NewSnapshot),
    ),
    error_definition(
        ProductErrorCode::SqlForeignPrepared,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlUnknownObject,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlInvalidValue,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlNoAccessPath,
        ProductErrorCategory::InvalidRequest,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlUniqueViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlCheckViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::SqlForeignKeyViolation,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::WriteConflict,
        ProductErrorCategory::Conflict,
        Some(ProductRetry::NewSnapshot),
    ),
    error_definition(
        ProductErrorCode::ObjectNotFound,
        ProductErrorCategory::NotFound,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::LimitExceeded,
        ProductErrorCategory::Limit,
        Some(ProductRetry::Never),
    ),
    error_definition(
        ProductErrorCode::Corruption,
        ProductErrorCategory::Corruption,
        Some(ProductRetry::AfterRecovery),
    ),
    error_definition(ProductErrorCode::Io, ProductErrorCategory::Io, None),
    error_definition(
        ProductErrorCode::Internal,
        ProductErrorCategory::Internal,
        Some(ProductRetry::Never),
    ),
];

impl ProductErrorCode {
    /// Returns the stable lowercase ASCII wire identity.
    pub const fn as_str(self) -> &'static str {
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
        }
    }
}

/// Transport-independent product error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductError {
    code: ProductErrorCode,
    category: ProductErrorCategory,
    retry: ProductRetry,
    message: &'static str,
    object_id: Option<ObjectId>,
    request_id: Option<u128>,
    transaction_state: ProductTransactionState,
}

impl ProductError {
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
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Returns the affected catalog object when the mapping can prove it.
    pub const fn object_id(&self) -> Option<ObjectId> {
        self.object_id
    }

    /// Returns the stable product request identity when assigned.
    pub const fn request_id(&self) -> Option<u128> {
        self.request_id
    }

    /// Returns the transaction outcome known at the failure boundary.
    pub const fn transaction_state(&self) -> ProductTransactionState {
        self.transaction_state
    }

    const fn new(
        code: ProductErrorCode,
        category: ProductErrorCategory,
        retry: ProductRetry,
        message: &'static str,
        object_id: Option<ObjectId>,
    ) -> Self {
        Self {
            code,
            category,
            retry,
            message,
            object_id,
            request_id: None,
            transaction_state: ProductTransactionState::None,
        }
    }
}

impl std::fmt::Display for ProductError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProductError {}

impl From<NativeRuntimeError> for ProductError {
    #[allow(clippy::too_many_lines)]
    fn from(source: NativeRuntimeError) -> Self {
        if let Some(kind) = source.io_error_kind() {
            return Self::new(
                ProductErrorCode::Io,
                ProductErrorCategory::Io,
                io_retry(kind),
                "native filesystem operation failed",
                None,
            );
        }
        if source.is_corruption() {
            return Self::new(
                ProductErrorCode::Corruption,
                ProductErrorCategory::Corruption,
                ProductRetry::AfterRecovery,
                "native durable state is invalid",
                None,
            );
        }
        match source {
            NativeRuntimeError::DataDirectoryExists => Self::new(
                ProductErrorCode::DataDirectoryExists,
                ProductErrorCategory::Conflict,
                ProductRetry::Never,
                "native data directory already exists",
                None,
            ),
            NativeRuntimeError::Directory(NativeDirectoryError::AlreadyLocked(_)) => Self::new(
                ProductErrorCode::DataDirectoryLocked,
                ProductErrorCategory::Unavailable,
                ProductRetry::AfterBackoff,
                "native data directory is already locked",
                None,
            ),
            NativeRuntimeError::Directory(NativeDirectoryError::Format2Directory(_)) => Self::new(
                ProductErrorCode::Format2Directory,
                ProductErrorCategory::InvalidRequest,
                ProductRetry::Never,
                "format-2 directory cannot be opened as native",
                None,
            ),
            NativeRuntimeError::Directory(_) => Self::new(
                ProductErrorCode::InvalidDataDirectory,
                ProductErrorCategory::Corruption,
                ProductRetry::AfterRecovery,
                "native data directory is invalid",
                None,
            ),
            NativeRuntimeError::WriteConflict(_) => Self::new(
                ProductErrorCode::WriteConflict,
                ProductErrorCategory::Conflict,
                ProductRetry::NewSnapshot,
                "native write conflicts with a committed transaction",
                None,
            ),
            NativeRuntimeError::UnknownRelation { table } => Self::not_found(table),
            NativeRuntimeError::UnknownSecondaryIndex { index }
            | NativeRuntimeError::UnknownVectorIndex { index } => Self::not_found(index),
            NativeRuntimeError::UnknownStructureHash
            | NativeRuntimeError::UnknownStructureSet
            | NativeRuntimeError::UnknownStructureList
            | NativeRuntimeError::UnknownStructureStream
            | NativeRuntimeError::UnknownStructureSortedSet
            | NativeRuntimeError::UnknownSnapshotPin => Self::new(
                ProductErrorCode::ObjectNotFound,
                ProductErrorCategory::NotFound,
                ProductRetry::Never,
                "native object does not exist",
                None,
            ),
            NativeRuntimeError::HashFieldBatchTooLarge { .. }
            | NativeRuntimeError::SetMemberBatchTooLarge { .. }
            | NativeRuntimeError::InvalidExpirySweepLimit { .. }
            | NativeRuntimeError::InvalidGroupCommitBatchSize { .. }
            | NativeRuntimeError::StructureIdentityTooLarge
            | NativeRuntimeError::SearchIdentityTooLarge => Self::new(
                ProductErrorCode::LimitExceeded,
                ProductErrorCategory::Limit,
                ProductRetry::Never,
                "native request exceeds a product limit",
                None,
            ),
            NativeRuntimeError::InvalidCatalogTree
            | NativeRuntimeError::InvalidRelationalTree
            | NativeRuntimeError::InvalidStructureTree
            | NativeRuntimeError::InvalidSearchTree
            | NativeRuntimeError::InvalidAnnTree
            | NativeRuntimeError::InvalidCommittedRoot
            | NativeRuntimeError::InvalidCheckpoint
            | NativeRuntimeError::NoncontiguousCommitSequence
            | NativeRuntimeError::FuturePage => Self::new(
                ProductErrorCode::Corruption,
                ProductErrorCategory::Corruption,
                ProductRetry::AfterRecovery,
                "native durable state is invalid",
                None,
            ),
            _ => Self::new(
                ProductErrorCode::Internal,
                ProductErrorCategory::Internal,
                ProductRetry::Never,
                "native product operation failed",
                None,
            ),
        }
    }
}

fn io_retry(kind: std::io::ErrorKind) -> ProductRetry {
    use std::io::ErrorKind;

    match kind {
        ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            ProductRetry::AfterBackoff
        }
        _ => ProductRetry::AfterRecovery,
    }
}

impl From<SqlError> for ProductError {
    fn from(source: SqlError) -> Self {
        match source {
            SqlError::InvalidSyntax => Self::sql(
                ProductErrorCode::SqlInvalidSyntax,
                ProductErrorCategory::InvalidRequest,
                ProductRetry::Never,
                "native SQL syntax is invalid or unsupported",
            ),
            SqlError::ParameterMismatch => Self::sql(
                ProductErrorCode::SqlParameterMismatch,
                ProductErrorCategory::InvalidRequest,
                ProductRetry::Never,
                "native SQL parameters do not match the prepared plan",
            ),
            SqlError::CatalogChanged => Self::sql(
                ProductErrorCode::SqlCatalogChanged,
                ProductErrorCategory::Conflict,
                ProductRetry::NewSnapshot,
                "native SQL prepared plan requires rebind",
            ),
            SqlError::UnknownColumn | SqlError::UnknownRelation => Self::sql(
                ProductErrorCode::SqlUnknownObject,
                ProductErrorCategory::NotFound,
                ProductRetry::Never,
                "native SQL catalog object does not exist",
            ),
            SqlError::InvalidCatalogObject => Self::new(
                ProductErrorCode::Corruption,
                ProductErrorCategory::Corruption,
                ProductRetry::AfterRecovery,
                "native SQL catalog object is invalid",
                None,
            ),
            SqlError::DuplicateColumn
            | SqlError::TypeMismatch
            | SqlError::NullViolation
            | SqlError::InvalidPrimaryKey
            | SqlError::InvalidSecondaryIndexRange
            | SqlError::PrimaryKeyMutationUnsupported => Self::sql(
                ProductErrorCode::SqlInvalidValue,
                ProductErrorCategory::InvalidRequest,
                ProductRetry::Never,
                "native SQL value or binding is invalid",
            ),
            SqlError::NoAccessPath => Self::sql(
                ProductErrorCode::SqlNoAccessPath,
                ProductErrorCategory::InvalidRequest,
                ProductRetry::Never,
                "native SQL query has no admitted access path",
            ),
            SqlError::UniqueViolation => Self::sql(
                ProductErrorCode::SqlUniqueViolation,
                ProductErrorCategory::Conflict,
                ProductRetry::Never,
                "native SQL unique constraint failed",
            ),
            SqlError::CheckViolation => Self::sql(
                ProductErrorCode::SqlCheckViolation,
                ProductErrorCategory::Conflict,
                ProductRetry::Never,
                "native SQL check constraint failed",
            ),
            SqlError::ForeignKeyViolation => Self::sql(
                ProductErrorCode::SqlForeignKeyViolation,
                ProductErrorCategory::Conflict,
                ProductRetry::Never,
                "native SQL foreign key constraint failed",
            ),
            SqlError::InvalidStoredRow => Self::new(
                ProductErrorCode::Corruption,
                ProductErrorCategory::Corruption,
                ProductRetry::AfterRecovery,
                "native SQL stored row is invalid",
                None,
            ),
            SqlError::Runtime(source) => source.into(),
        }
    }
}

impl ProductError {
    const fn not_found(object_id: ObjectId) -> Self {
        Self::new(
            ProductErrorCode::ObjectNotFound,
            ProductErrorCategory::NotFound,
            ProductRetry::Never,
            "native object does not exist",
            Some(object_id),
        )
    }

    const fn sql(
        code: ProductErrorCode,
        category: ProductErrorCategory,
        retry: ProductRetry,
        message: &'static str,
    ) -> Self {
        Self::new(code, category, retry, message, None)
    }
}

/// Stable metadata for one immutable product snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotIdentity {
    /// Stable native data-directory lineage.
    pub directory_lineage: [u8; 24],
    /// Latest commit visible to every engine, absent for an empty directory.
    pub visible_csn: Option<Csn>,
    /// Catalog version pinned by the snapshot.
    pub catalog_version: CatalogVersion,
    /// Complete immutable root-set digest.
    pub root_digest: [u8; 32],
    /// Logical time used by temporal and TTL reads.
    pub logical_time_micros: i64,
}

/// Curated immutable embedded read facade.
#[derive(Clone, Debug)]
pub struct ProductSnapshot {
    directory_lineage: [u8; 24],
    inner: NativeSnapshot,
}

impl ProductSnapshot {
    /// Returns snapshot identity shared by all engines.
    pub fn identity(&self) -> SnapshotIdentity {
        SnapshotIdentity {
            directory_lineage: self.directory_lineage,
            visible_csn: self.inner.visible_csn(),
            catalog_version: self.inner.catalog_version(),
            root_digest: self.inner.root_digest(),
            logical_time_micros: self.inner.logical_time_micros(),
        }
    }

    /// Looks up one catalog object by stable identity.
    ///
    /// # Errors
    ///
    /// Returns a stable not-found error when the object is absent.
    pub fn catalog_object(&self, id: ObjectId) -> Result<&CatalogObject, ProductError> {
        self.inner.catalog_object(id).ok_or_else(|| {
            ProductError::new(
                ProductErrorCode::CatalogObjectNotFound,
                ProductErrorCategory::NotFound,
                ProductRetry::Never,
                "native catalog object does not exist",
                Some(id),
            )
        })
    }

    /// Returns one scalar structure value at the snapshot logical time.
    pub fn structure_get(&self, key: &[u8]) -> Option<&[u8]> {
        self.inner.get(key)
    }

    /// Returns one scalar structure TTL state.
    pub fn structure_ttl(&self, key: &[u8]) -> ProductTtl {
        self.inner.ttl(key).into()
    }

    /// Executes one catalog-bound prepared SQL read.
    ///
    /// # Errors
    ///
    /// Returns a stable SQL or durable-state error.
    pub fn execute_prepared(
        &self,
        prepared: &ProductPreparedStatement,
        parameters: &[ProductValue],
    ) -> Result<ProductSqlResult, ProductError> {
        if prepared.directory_lineage != self.directory_lineage {
            return Err(foreign_prepared_error());
        }
        self.inner
            .execute_prepared(&prepared.inner, parameters)
            .map(ProductSqlResult::from)
            .map_err(Into::into)
    }
}

/// Curated embedded Native product facade.
#[derive(Debug)]
pub struct NativeProduct {
    database: NativeDatabase,
}

impl NativeProduct {
    /// Creates a new native product directory.
    ///
    /// # Errors
    ///
    /// Returns a stable product error when the directory cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        NativeDatabase::create(path)
            .map(|database| Self { database })
            .map_err(Into::into)
    }

    /// Opens and verifies an existing native product directory.
    ///
    /// # Errors
    ///
    /// Returns a stable product error for ownership, I/O, or corruption.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        NativeDatabase::open(path)
            .map(|database| Self { database })
            .map_err(Into::into)
    }

    /// Captures one immutable snapshot for a caller-supplied bounded dataset.
    ///
    /// This first G6 slice materializes the admitted engine state. It remains
    /// partial evidence until a later slice adds explicit snapshot work and
    /// memory limits.
    ///
    /// # Errors
    ///
    /// Returns a stable product error for snapshot or durable-state failure.
    pub fn snapshot_bounded(
        &self,
        logical_time_micros: i64,
    ) -> Result<ProductSnapshot, ProductError> {
        self.database
            .snapshot(logical_time_micros)
            .map(|inner| ProductSnapshot {
                directory_lineage: self.database.directory_identity().lineage().encode(),
                inner,
            })
            .map_err(Into::into)
    }

    /// Resolves one current catalog object by stable identity.
    ///
    /// # Errors
    ///
    /// Returns a stable not-found or durable-state error.
    pub fn catalog_object(&self, id: ObjectId) -> Result<ProductRead<CatalogObject>, ProductError> {
        let identified = self
            .database
            .catalog_object_latest_identified(id)
            .map_err(ProductError::from)?;
        let value = identified.object.ok_or_else(|| {
            ProductError::new(
                ProductErrorCode::CatalogObjectNotFound,
                ProductErrorCategory::NotFound,
                ProductRetry::Never,
                "native catalog object does not exist",
                Some(id),
            )
        })?;
        let snapshot = SnapshotIdentity {
            directory_lineage: self.database.directory_identity().lineage().encode(),
            visible_csn: identified.visible_csn,
            catalog_version: identified.catalog_version,
            root_digest: identified.root_digest,
            logical_time_micros: 0,
        };
        Ok(ProductRead { snapshot, value })
    }

    /// Resolves one current catalog object by normalized qualified name.
    ///
    /// # Errors
    ///
    /// Returns a stable not-found or durable-state error.
    pub fn catalog_object_named(
        &self,
        name: &QualifiedName,
    ) -> Result<ProductRead<CatalogObject>, ProductError> {
        let identified = self
            .database
            .catalog_object_named_latest_identified(name)
            .map_err(ProductError::from)?;
        let value = identified.object.ok_or_else(|| {
            ProductError::new(
                ProductErrorCode::CatalogObjectNotFound,
                ProductErrorCategory::NotFound,
                ProductRetry::Never,
                "native catalog object does not exist",
                None,
            )
        })?;
        let snapshot = SnapshotIdentity {
            directory_lineage: self.database.directory_identity().lineage().encode(),
            visible_csn: identified.visible_csn,
            catalog_version: identified.catalog_version,
            root_digest: identified.root_digest,
            logical_time_micros: 0,
        };
        Ok(ProductRead { snapshot, value })
    }

    /// Binds one current-catalog prepared SQL read.
    ///
    /// # Errors
    ///
    /// Returns a stable SQL or durable-state error.
    pub fn prepare_sql(&self, statement: &str) -> Result<ProductPreparedStatement, ProductError> {
        if statement.len() > MAX_PRODUCT_SQL_STATEMENT_BYTES {
            return Err(limit_error(
                "native SQL statement exceeds the product limit",
            ));
        }
        let inner = self
            .database
            .prepare_sql_latest(statement)
            .map_err(ProductError::from)?;
        if inner.parameter_count() > MAX_PRODUCT_SQL_PARAMETERS
            || inner
                .maximum_result_rows()
                .is_none_or(|rows| rows > MAX_PRODUCT_SQL_ROWS)
        {
            return Err(limit_error("native SQL plan exceeds the product limit"));
        }
        let maximum_result_rows = inner.maximum_result_rows().unwrap_or(0);
        Ok(ProductPreparedStatement {
            directory_lineage: self.database.directory_identity().lineage().encode(),
            maximum_result_rows,
            inner,
        })
    }

    /// Executes one current prepared SQL read.
    ///
    /// # Errors
    ///
    /// Returns a stable SQL or durable-state error.
    pub fn execute_prepared(
        &self,
        prepared: &ProductPreparedStatement,
        parameters: &[ProductValue],
    ) -> Result<ProductRead<ProductSqlResult>, ProductError> {
        if prepared.directory_lineage != self.database.directory_identity().lineage().encode() {
            return Err(foreign_prepared_error());
        }
        if parameters.len() > MAX_PRODUCT_SQL_PARAMETERS {
            return Err(limit_error(
                "native SQL parameters exceed the product limit",
            ));
        }
        if parameters.len() != prepared.parameter_count() {
            return Err(ProductError::from(SqlError::ParameterMismatch));
        }
        let (visible_csn, catalog_version, root_digest, value) = self
            .database
            .execute_prepared_latest_identified(&prepared.inner, parameters)
            .map_err(ProductError::from)?;
        let snapshot = SnapshotIdentity {
            directory_lineage: self.database.directory_identity().lineage().encode(),
            visible_csn: Some(visible_csn),
            catalog_version,
            root_digest,
            logical_time_micros: 0,
        };
        Ok(ProductRead {
            snapshot,
            value: ProductSqlResult::from(value),
        })
    }
}

const fn foreign_prepared_error() -> ProductError {
    ProductError::new(
        ProductErrorCode::SqlForeignPrepared,
        ProductErrorCategory::Conflict,
        ProductRetry::Never,
        "native SQL prepared plan belongs to another directory",
        None,
    )
}

const fn limit_error(message: &'static str) -> ProductError {
    ProductError::new(
        ProductErrorCode::LimitExceeded,
        ProductErrorCategory::Limit,
        ProductRetry::Never,
        message,
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, io, path::PathBuf};

    use super::{
        MAX_PRODUCT_SQL_STATEMENT_BYTES, NativeDatabase, NativeProduct, ObjectId, ProductError,
        ProductErrorCategory, ProductErrorCode, ProductRetry, ProductTransactionState,
        ProductValue,
    };
    use hyphae_native_blobs::BlobError;
    use hyphae_native_btree::BTreeError;
    use hyphae_native_catalog::CatalogError;
    use hyphae_native_manifest::ManifestError;
    use hyphae_native_pages::{BufferPoolError, PageError, PageStoreError};
    use hyphae_native_records::RecordError;
    use hyphae_native_runtime::{NativeRuntimeError, SnapshotPinError, SqlError};
    use hyphae_native_wal::WalError;

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyphae-native-product-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn facade_creates_empty_directory_and_reports_safe_not_found()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("snapshot");
        let _ = fs::remove_dir_all(&path);
        let product = NativeProduct::create(&path)?;
        let missing = ObjectId::new(1)?;
        let error = product
            .catalog_object(missing)
            .err()
            .ok_or("missing catalog object unexpectedly resolved")?;
        assert_eq!(error.code(), ProductErrorCode::CatalogObjectNotFound);
        assert_eq!(error.category(), ProductErrorCategory::NotFound);
        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn directory_lock_and_sql_errors_have_stable_mappings() -> Result<(), Box<dyn std::error::Error>>
    {
        let path = temporary("errors");
        let _ = fs::remove_dir_all(&path);
        let product = NativeProduct::create(&path)?;
        let Err(error) = NativeProduct::open(&path) else {
            return Err("second handle was not rejected".into());
        };
        assert_eq!(error.code(), ProductErrorCode::DataDirectoryLocked);
        assert_eq!(error.category(), ProductErrorCategory::Unavailable);
        assert_eq!(error.retry(), ProductRetry::AfterBackoff);
        let sql = super::ProductError::from(SqlError::CatalogChanged);
        assert_eq!(sql.code(), ProductErrorCode::SqlCatalogChanged);
        assert_eq!(sql.code().as_str(), "sql_catalog_changed");
        assert_eq!(sql.retry(), ProductRetry::NewSnapshot);
        assert_eq!(sql.transaction_state(), ProductTransactionState::None);
        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn stable_error_registry_matches_accepted_contract() {
        let contract = include_str!("../../../docs/native/product-error-v1.md")
            .lines()
            .filter_map(|line| {
                let columns = line
                    .strip_prefix("| `")?
                    .strip_suffix("` |")?
                    .split("` | `")
                    .collect::<Vec<_>>();
                match columns.as_slice() {
                    [code, category, retry] => Some((*code, *category, *retry)),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let implementation = super::PRODUCT_ERROR_REGISTRY_V1
            .iter()
            .copied()
            .map(|definition| {
                (
                    definition.code().as_str(),
                    definition.category().as_str(),
                    definition
                        .default_retry()
                        .map_or("failure-dependent", ProductRetry::as_str),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(implementation, contract);
        let unique = implementation
            .iter()
            .map(|(code, _, _)| *code)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), implementation.len());
    }

    #[test]
    fn nested_runtime_io_and_corruption_have_honest_categories() {
        let io_errors = [
            NativeRuntimeError::Wal(WalError::Io(io::ErrorKind::TimedOut.into())),
            NativeRuntimeError::Page(PageStoreError::Io(io::ErrorKind::PermissionDenied.into())),
            NativeRuntimeError::BufferPool(BufferPoolError::Store(PageStoreError::Io(
                io::ErrorKind::WouldBlock.into(),
            ))),
            NativeRuntimeError::Blob(BlobError::Io(io::ErrorKind::NotFound.into())),
            NativeRuntimeError::BTree(BTreeError::BufferPool(BufferPoolError::Store(
                PageStoreError::Io(io::ErrorKind::Interrupted.into()),
            ))),
            NativeRuntimeError::Manifest(ManifestError::Io(io::ErrorKind::StorageFull.into())),
            NativeRuntimeError::SnapshotPin(SnapshotPinError::Io(
                io::ErrorKind::ReadOnlyFilesystem.into(),
            )),
        ];
        for source in io_errors {
            assert!(source.is_io());
            let error = ProductError::from(source);
            assert_eq!(error.code(), ProductErrorCode::Io);
            assert_eq!(error.category(), ProductErrorCategory::Io);
        }

        let corruption_errors = [
            NativeRuntimeError::Wal(WalError::BlockChecksumMismatch),
            NativeRuntimeError::Page(PageStoreError::Page(PageError::PayloadTooLarge {
                actual: usize::MAX,
            })),
            NativeRuntimeError::BufferPool(BufferPoolError::Store(PageStoreError::Page(
                PageError::DigestMismatch,
            ))),
            NativeRuntimeError::Blob(BlobError::Identity(
                hyphae_native_types::NativeTypeError::ZeroIdentity("blob ID"),
            )),
            NativeRuntimeError::BTree(BTreeError::InvalidPreamble),
            NativeRuntimeError::Record(RecordError::EmptyRegularRow),
            NativeRuntimeError::Manifest(ManifestError::DigestMismatch),
            NativeRuntimeError::SnapshotPin(SnapshotPinError::ChecksumMismatch),
            NativeRuntimeError::Catalog(CatalogError::WrongObjectOwner),
        ];
        for source in corruption_errors {
            assert!(!source.is_io());
            let error = ProductError::from(source);
            assert_eq!(error.code(), ProductErrorCode::Corruption);
            assert_eq!(error.category(), ProductErrorCategory::Corruption);
            assert_eq!(error.retry(), ProductRetry::AfterRecovery);
        }

        for source in [
            NativeRuntimeError::BTree(BTreeError::KeyTooLarge),
            NativeRuntimeError::Catalog(CatalogError::VersionExhausted),
        ] {
            let error = ProductError::from(source);
            assert_eq!(error.code(), ProductErrorCode::Internal);
            assert_eq!(error.category(), ProductErrorCategory::Internal);
        }
    }

    #[test]
    fn missing_relation_is_not_an_internal_product_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("missing-relation");
        let _ = fs::remove_dir_all(&path);
        let product = NativeProduct::create(&path)?;
        let error = product
            .prepare_sql("SELECT id FROM missing WHERE id = ?")
            .err()
            .ok_or("missing relation unexpectedly prepared")?;
        assert_eq!(error.code(), ProductErrorCode::SqlUnknownObject);
        assert_eq!(error.category(), ProductErrorCategory::NotFound);
        assert_eq!(error.retry(), ProductRetry::Never);
        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn prepared_sql_enforces_product_row_and_statement_limits()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("sql-limits");
        let _ = fs::remove_dir_all(&path);
        let mut runtime = NativeDatabase::create(&path)?;
        let mut transaction = runtime.begin(1, hyphae_native_types::DurabilityClass::Memory)?;
        transaction.execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)", &[])?;
        transaction.commit()?;
        drop(runtime);
        let product = NativeProduct::open(&path)?;

        let prepared = product.prepare_sql("SELECT id FROM items WHERE id = ?")?;
        assert_eq!(prepared.parameter_count(), 1);
        assert_eq!(prepared.maximum_result_rows(), 1);

        let oversized = format!("SELECT{}", " ".repeat(MAX_PRODUCT_SQL_STATEMENT_BYTES));
        let statement_error = product
            .prepare_sql(&oversized)
            .err()
            .ok_or("oversized statement unexpectedly prepared")?;
        assert_eq!(statement_error.code(), ProductErrorCode::LimitExceeded);

        let row_error = product
            .prepare_sql("SELECT id FROM items LIMIT 1025")
            .err()
            .ok_or("oversized row plan unexpectedly prepared")?;
        assert_eq!(row_error.code(), ProductErrorCode::LimitExceeded);
        assert_eq!(row_error.category(), ProductErrorCategory::Limit);

        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn non_relation_name_is_not_reported_as_corruption() {
        let error = super::ProductError::from(SqlError::UnknownRelation);
        assert_eq!(error.code(), ProductErrorCode::SqlUnknownObject);
        assert_eq!(error.category(), ProductErrorCategory::NotFound);
        assert_eq!(error.retry(), ProductRetry::Never);
    }

    #[test]
    fn prepared_statement_cannot_cross_directory_lineage() -> Result<(), Box<dyn std::error::Error>>
    {
        let left_path = temporary("prepared-left");
        let right_path = temporary("prepared-right");
        let _ = fs::remove_dir_all(&left_path);
        let _ = fs::remove_dir_all(&right_path);
        let mut left_runtime = NativeDatabase::create(&left_path)?;
        let mut transaction =
            left_runtime.begin(1, hyphae_native_types::DurabilityClass::Memory)?;
        transaction.execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)", &[])?;
        transaction.commit()?;
        drop(left_runtime);
        let left = NativeProduct::open(&left_path)?;
        let right = NativeProduct::create(&right_path)?;
        let prepared = left.prepare_sql("SELECT id FROM items WHERE id = ?")?;
        let error = right
            .execute_prepared(&prepared, &[ProductValue::Signed(1)])
            .err()
            .ok_or("foreign prepared statement unexpectedly executed")?;
        assert_eq!(error.code(), ProductErrorCode::SqlForeignPrepared);
        assert_eq!(error.category(), ProductErrorCategory::Conflict);
        drop(left);
        drop(right);
        fs::remove_dir_all(left_path)?;
        fs::remove_dir_all(right_path)?;
        Ok(())
    }

    #[test]
    fn prepared_statement_snapshot_cannot_cross_directory_lineage()
    -> Result<(), Box<dyn std::error::Error>> {
        let left_path = temporary("snapshot-prepared-left");
        let right_path = temporary("snapshot-prepared-right");
        let _ = fs::remove_dir_all(&left_path);
        let _ = fs::remove_dir_all(&right_path);
        let mut left_runtime = NativeDatabase::create(&left_path)?;
        let mut transaction =
            left_runtime.begin(1, hyphae_native_types::DurabilityClass::Memory)?;
        transaction.execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)", &[])?;
        transaction.commit()?;
        drop(left_runtime);
        let left = NativeProduct::open(&left_path)?;
        let right = NativeProduct::create(&right_path)?;
        let prepared = left.prepare_sql("SELECT id FROM items WHERE id = ?")?;
        let snapshot = right.snapshot_bounded(0)?;
        let error = snapshot
            .execute_prepared(&prepared, &[ProductValue::Signed(1)])
            .err()
            .ok_or("foreign prepared statement unexpectedly executed on snapshot")?;
        assert_eq!(error.code(), ProductErrorCode::SqlForeignPrepared);
        assert_eq!(error.category(), ProductErrorCategory::Conflict);
        assert_eq!(error.retry(), ProductRetry::Never);
        drop(left);
        drop(right);
        fs::remove_dir_all(left_path)?;
        fs::remove_dir_all(right_path)?;
        Ok(())
    }
}
