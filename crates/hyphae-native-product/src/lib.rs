// SPDX-License-Identifier: Apache-2.0

//! Transport-independent contracts and a curated embedded facade for Hyphae Native.
//!
//! This crate is the first G6 product slice. It deliberately exposes only
//! directory lifecycle, snapshots, catalog point lookup, prepared SQL reads,
//! and scalar structure access. Later G6 slices add the remaining admitted
//! operation families without making transport types product authority.

#![allow(clippy::result_large_err)]

mod access_catalog;
mod access_control;
mod admin;
mod backup;
mod cancellation;
mod capabilities;
mod catalog;
pub mod chunker;
mod default_scalar_keyspace;
mod doctor;
pub mod error;
pub mod error_codec;
mod lexical_analyzer;
mod limits;
mod operation;
/// Canonical, bounded native proof and directory-witness artifacts.
pub mod proof;
mod search;
mod service;
mod session;
mod structures;
mod telemetry;

use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};

pub use access_catalog::*;
pub use access_control::*;
pub use admin::*;
pub use backup::*;
pub use cancellation::*;
pub use capabilities::*;
pub use catalog::*;
pub use doctor::*;
pub use error::*;
pub use error_codec::*;
pub use hyphae_native_catalog::{
    CatalogName, CatalogObject, LogicalCatalogObject, QualifiedName, StructureKind,
};
pub use hyphae_native_runtime::{BoundSqlStatement, BoundedSearchQuery};
use hyphae_native_runtime::{
    HnswConfig, NativeDatabase, NativeSnapshot, PreparedStatement, SqlError, SqlResult,
    Vector as RuntimeVector, VectorMetric as RuntimeVectorMetric,
};
pub use hyphae_native_types::{
    CanonicalF32, CanonicalF64, CatalogVersion, Csn, ObjectId, ScalarValue as ProductValue,
};
pub use limits::*;
pub use operation::*;
pub use search::*;
pub use service::*;
pub use session::*;
pub use structures::*;
pub use telemetry::*;

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

    pub(crate) fn referenced_object_ids(&self) -> std::collections::BTreeSet<ObjectId> {
        self.inner.referenced_object_ids()
    }
}

/// Maximum UTF-8 statement bytes admitted by the current embedded product slice.
pub const MAX_PRODUCT_SQL_STATEMENT_BYTES: usize = 64 * 1024;
/// Maximum SQL parameters admitted by the current embedded product slice.
pub const MAX_PRODUCT_SQL_PARAMETERS: usize = 1_024;
/// Maximum rows materialized by the current embedded product slice.
pub const MAX_PRODUCT_SQL_ROWS: usize = 1_024;

const MIGRATION_STORAGE_PREFIX: &[u8] = b"\0hyphae.migration.v1\0";
const CATALOG_CURSOR_AUTHORITY_KEY: &[u8] = b"\0hyphae.catalog-visible.v1\0cursor-key";
const INTERNAL_STRUCTURE_KEY_PREFIX: &[u8] = b"\0hyphae.";
const MIGRATION_SEARCH_BATCH_SIZE: usize = 512;

/// One source lexical index prepared for offline migration.
#[derive(Clone, Debug)]
pub struct MigrationLexicalIndexInput {
    /// Stable Native physical index identity.
    pub index: ObjectId,
    /// Physical index name.
    pub name: String,
    /// Source documents and their weighted lexical text.
    pub documents: Vec<(Vec<u8>, String)>,
}

/// One source vector space prepared for offline migration.
#[derive(Clone, Debug)]
pub struct MigrationVectorIndexInput {
    /// Stable Native physical index identity.
    pub index: ObjectId,
    /// Physical index name.
    pub name: String,
    /// Fixed source dimension.
    pub dimension: u16,
    /// Source document identities and finite vector components.
    pub vectors: Vec<(ObjectId, Vec<f32>)>,
}

/// Expected lexical documents for migration verification.
pub type MigrationLexicalVerification = (ObjectId, Vec<(Vec<u8>, String)>);
/// Expected vector records for migration verification.
pub type MigrationVectorVerification = (ObjectId, Vec<(ObjectId, Vec<f32>)>);

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
    pub(crate) inner: NativeSnapshot,
}

impl ProductSnapshot {
    /// Times the retained-model scorer alone. Diagnostic surface for the
    /// cap-ladder evidence harness; not a public contract.
    #[doc(hidden)]
    pub fn match_text_for_diagnostics(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<usize, ProductError> {
        self.match_text_hits_for_diagnostics(index, query, limit)
            .map(|hits| hits.len())
    }

    /// Returns the retained-model scorer's ranked hits so the evidence
    /// harness can compare them bit-for-bit against the durable scorer.
    /// Diagnostic surface; not a public contract.
    #[doc(hidden)]
    pub fn match_text_hits_for_diagnostics(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<hyphae_native_runtime::MatchHit>, ProductError> {
        self.inner
            .match_text(index, query, limit)
            .map_err(ProductError::from)
    }

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
        self.inner
            .catalog_object(id)
            .ok_or_else(|| ProductError::catalog_object_not_found(Some(id)))
    }

    /// Returns one scalar structure value at the snapshot logical time.
    pub fn structure_get(&self, key: &[u8]) -> Option<&[u8]> {
        (!is_internal_structure_key(key))
            .then(|| self.inner.get(key))
            .flatten()
    }

    /// Returns one scalar structure TTL state.
    pub fn structure_ttl(&self, key: &[u8]) -> ProductTtl {
        if is_internal_structure_key(key) {
            ProductTtl::Missing
        } else {
            self.inner.ttl(key).into()
        }
    }

    /// Returns visible public scalar keys inside `[start, end)`, ascending, or
    /// `None` fail-closed above `limit`.
    pub fn structure_keys_in_range(
        &self,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<Vec<u8>>> {
        if start.starts_with(INTERNAL_STRUCTURE_KEY_PREFIX)
            || end.starts_with(INTERNAL_STRUCTURE_KEY_PREFIX)
        {
            return Some(Vec::new());
        }
        self.inner.structure_keys_in_range(start, end, limit)
    }

    /// Returns visible internal scalar keys inside `[start, end)`, ascending,
    /// or `None` fail-closed above `limit`. Both bounds must carry the
    /// reserved internal prefix; anything else returns no keys.
    pub(crate) fn visit_structure_keys_in_range_internal(
        &self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        visitor: impl FnMut(&[u8]),
    ) -> Option<()> {
        if !start.starts_with(INTERNAL_STRUCTURE_KEY_PREFIX)
            || !end.starts_with(INTERNAL_STRUCTURE_KEY_PREFIX)
        {
            return Some(());
        }
        self.inner
            .visit_structure_keys_in_range(start, end, limit, visitor)
    }

    pub(crate) fn structure_get_internal(&self, key: &[u8]) -> Option<&[u8]> {
        self.inner.get(key)
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
    pub(crate) database: NativeDatabase,
    pub(crate) default_scalar_keyspace_id: Option<ObjectId>,
    pub(crate) telemetry: TelemetryRegistry,
    pub(crate) access_control_epoch: AtomicU64,
    pub(crate) access_control_epoch_known: AtomicBool,
    pub(crate) authorization_time_watermark: AtomicI64,
    pub(crate) catalog_cursor_key: [u8; 32],
    security_commit_interruption: Option<SecurityCommitInterruption>,
    #[cfg(test)]
    pub(crate) access_control_catalog_loads: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SecurityCommitInterruption {
    boundary: hyphae_native_runtime::CommitBoundary,
    hook: Option<fn(hyphae_native_runtime::CommitBoundary)>,
}

impl SecurityCommitInterruption {
    pub(crate) const fn returning(boundary: hyphae_native_runtime::CommitBoundary) -> Self {
        Self {
            boundary,
            hook: None,
        }
    }

    pub(crate) const fn hooked(
        boundary: hyphae_native_runtime::CommitBoundary,
        hook: fn(hyphae_native_runtime::CommitBoundary),
    ) -> Self {
        Self {
            boundary,
            hook: Some(hook),
        }
    }

    pub(crate) fn commit(
        self,
        transaction: hyphae_native_runtime::NativeTransaction<'_>,
    ) -> Result<hyphae_native_runtime::CommitReceipt, hyphae_native_runtime::NativeRuntimeError>
    {
        match self.hook {
            Some(hook) => transaction.commit_with_boundary_hook_for_test(self.boundary, hook),
            None => transaction.commit_with_interruption(self.boundary),
        }
    }
}

fn catalog_cursor_process_key() -> Result<[u8; 32], ProductError> {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key)
        .map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable))?;
    Ok(key)
}

/// Deterministic explicit-transaction interruption used only by focused recovery tests.
#[doc(hidden)]
pub fn commit_explicit_transaction_with_interruption_for_test(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    handle: ProductTransactionHandle,
    boundary: hyphae_native_runtime::CommitBoundary,
) -> Result<ProductCommitReceipt, ProductError> {
    use hyphae_native_runtime::NativeRuntimeError;

    context.checkpoint()?;
    let transaction = session
        .take_active_transaction(handle)
        .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    let staged_operations = transaction.staged_operations;
    if staged_operations == 0 || transaction.batch.mutation_count() == 0 {
        session.replace_active_transaction(handle, transaction);
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    let principal_hash = *blake3::hash(context.principal.identity().as_bytes()).as_bytes();
    let mut token = blake3::Hasher::new();
    token.update(b"hyphae-product-explicit-interruption-v1");
    token.update(&context.session_id.get().to_le_bytes());
    token.update(&handle.get().to_le_bytes());
    let idempotency_token = *token.finalize().as_bytes();
    match product
        .database
        .commit_optimistic_resolved_with_interruption(
            transaction.batch,
            principal_hash,
            idempotency_token,
            boundary,
        ) {
        Ok((resolution, receipt)) => {
            let transaction_id = ProductTransactionId::from(resolution.resolution_id);
            let receipt = ProductCommitReceipt::from_runtime(receipt, transaction_id);
            session
                .record_transaction(transaction_id, ProductTransactionStatus::Committed(receipt));
            Ok(receipt)
        }
        Err(error)
            if matches!(
                error.source(),
                NativeRuntimeError::InjectedCrash(
                    hyphae_native_runtime::CommitBoundary::WalAppended
                        | hyphae_native_runtime::CommitBoundary::WalSynchronized
                        | hyphae_native_runtime::CommitBoundary::RootPublished
                )
            ) =>
        {
            let resolution = error
                .resolution()
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?;
            let transaction_id = ProductTransactionId::from(resolution.resolution_id);
            session.record_transaction(
                transaction_id,
                ProductTransactionStatus::OutcomeUnknown { transaction_id },
            );
            session.record_explicit_status(
                handle,
                ProductExplicitTransactionStatus::OutcomeUnknown {
                    handle,
                    transaction_id,
                    staged_operations,
                },
            );
            Err(
                ProductFailureBoundary::publication_unknown(transaction_id.native())
                    .apply(ProductError::from(error.into_source())),
            )
        }
        Err(error) => Err(error.into_source().into()),
    }
}

impl NativeProduct {
    pub(crate) fn ensure_unmanaged_catalog_cursor_authority(&mut self) -> Result<(), ProductError> {
        if let Some(encoded) = self
            .database
            .get_latest_structure(CATALOG_CURSOR_AUTHORITY_KEY, 0)?
        {
            self.catalog_cursor_key = encoded
                .as_slice()
                .try_into()
                .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
            return Ok(());
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        transaction.set(
            CATALOG_CURSOR_AUTHORITY_KEY.to_vec(),
            self.catalog_cursor_key.to_vec(),
            None,
        )?;
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(())
    }

    fn initialize_catalog_cursor_authority(&mut self) -> Result<(), ProductError> {
        if let Some(encoded) = self
            .database
            .get_latest_structure(CATALOG_CURSOR_AUTHORITY_KEY, 0)?
        {
            self.catalog_cursor_key = encoded
                .as_slice()
                .try_into()
                .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
        }
        Ok(())
    }

    /// Returns the owned native data-directory path.
    pub fn data_directory(&self) -> &Path {
        self.database.data_directory()
    }

    /// Returns the stable native directory identity used by migration evidence.
    pub fn directory_identity(&self) -> &hyphae_native_runtime::NativeDirectoryIdentity {
        self.database.directory_identity()
    }

    /// Stores exact migration witness entries under one strict native commit.
    ///
    /// # Errors
    ///
    /// Returns a product or durability error when the witness cannot be stored.
    pub fn migration_store_entries(
        &mut self,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<ProductCommitReceipt, ProductError> {
        if entries.is_empty() {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        for (key, value) in entries {
            let mut storage_key = MIGRATION_STORAGE_PREFIX.to_vec();
            storage_key.extend_from_slice(&(key.len() as u64).to_be_bytes());
            storage_key.extend_from_slice(key);
            transaction.set(storage_key, value.clone(), None)?;
        }
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Stores exact legacy values in the public scalar structure namespace.
    ///
    /// # Errors
    ///
    /// Returns a product or durability error when the values cannot be stored.
    pub fn migration_store_public_entries(
        &mut self,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<ProductCommitReceipt, ProductError> {
        if entries.is_empty()
            || entries
                .iter()
                .any(|(key, _)| is_internal_structure_key(key))
        {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        for (key, value) in entries {
            transaction.set(key.clone(), value.clone(), None)?;
        }
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Stores one exact public scalar value with its original absolute expiry
    /// under a strict native commit.
    ///
    /// # Errors
    ///
    /// Returns a request, product, or durability error when the value cannot
    /// be stored.
    pub fn migration_store_public_entry(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        expires_at_micros: Option<i64>,
    ) -> Result<ProductCommitReceipt, ProductError> {
        if is_internal_structure_key(&key) {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        transaction.set(key, value, expires_at_micros)?;
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Deletes one public scalar value under a strict native commit.
    ///
    /// # Errors
    ///
    /// Returns a request, product, or durability error when the value cannot
    /// be deleted.
    pub fn migration_delete_public_entry(
        &mut self,
        key: Vec<u8>,
    ) -> Result<ProductCommitReceipt, ProductError> {
        if is_internal_structure_key(&key) {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        transaction.delete_structure(key)?;
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Checks exact migration witness entries at one immutable native snapshot.
    ///
    /// # Errors
    ///
    /// Returns a snapshot or corruption error when verification cannot run.
    pub fn migration_verify_entries(
        &self,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<bool, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        Ok(entries.iter().all(|(key, value)| {
            let mut storage_key = MIGRATION_STORAGE_PREFIX.to_vec();
            storage_key.extend_from_slice(&(key.len() as u64).to_be_bytes());
            storage_key.extend_from_slice(key);
            snapshot
                .structure_get_internal(&storage_key)
                .is_some_and(|actual| actual == value)
        }))
    }

    /// Verifies exact values in the public scalar structure namespace.
    ///
    /// # Errors
    ///
    /// Returns a snapshot or corruption error when verification cannot run.
    pub fn migration_verify_public_entries(
        &self,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<bool, ProductError> {
        if entries
            .iter()
            .any(|(key, _)| is_internal_structure_key(key))
        {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let snapshot = self.snapshot_bounded(0)?;
        Ok(entries.iter().all(|(key, value)| {
            snapshot
                .structure_get(key)
                .is_some_and(|actual| actual == value)
        }))
    }

    /// Imports source lexical and vector payloads into independent Native
    /// physical indexes under one strict commit.
    ///
    /// # Errors
    ///
    /// Returns a product, validation, or durability error when import fails.
    pub fn migration_store_search(
        &mut self,
        lexical_indexes: &[MigrationLexicalIndexInput],
        vector_indexes: &[MigrationVectorIndexInput],
    ) -> Result<ProductCommitReceipt, ProductError> {
        if lexical_indexes.is_empty() && vector_indexes.is_empty() {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut last_receipt = None;
        for index in lexical_indexes {
            let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
            transaction
                .create_search_index(index.index, &index.name)
                .map_err(|error| {
                    eprintln!("migration create search runtime error: {error:?}");
                    error
                })?;
            let receipt = transaction.commit().map_err(|error| {
                eprintln!("migration search commit error: {error:?}");
                error
            })?;
            self.observe_commit(&receipt);
            last_receipt = Some(receipt.into());
            for documents in index.documents.chunks(MIGRATION_SEARCH_BATCH_SIZE) {
                let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
                for (document_id, text) in documents {
                    transaction.index_document(index.index, document_id.clone(), text.clone())?;
                }
                let receipt = transaction.commit().map_err(|error| {
                    eprintln!("migration search commit error: {error:?}");
                    error
                })?;
                self.observe_commit(&receipt);
                last_receipt = Some(receipt.into());
            }
        }
        for index in vector_indexes {
            let config = HnswConfig::new(8, 32, 16, 4_096, migration_hnsw_seed(index.index))
                .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
            let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
            transaction
                .create_vector_index(
                    index.index,
                    &index.name,
                    index.dimension,
                    RuntimeVectorMetric::Cosine,
                    config,
                )
                .map_err(|error| {
                    eprintln!("migration create vector runtime error: {error:?}");
                    error
                })?;
            let receipt = transaction.commit().map_err(|error| {
                eprintln!("migration search commit error: {error:?}");
                error
            })?;
            self.observe_commit(&receipt);
            last_receipt = Some(receipt.into());
            for records in index.vectors.chunks(MIGRATION_SEARCH_BATCH_SIZE) {
                let vectors = records
                    .iter()
                    .map(|(document_id, values)| {
                        Ok((
                            *document_id,
                            RuntimeVector::new(values.iter().copied()).map_err(|_| {
                                ProductError::from_code(ProductErrorCode::InvalidRequest)
                            })?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ProductError>>()?;
                let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
                transaction
                    .upsert_vectors(index.index, vectors)
                    .map_err(|error| {
                        eprintln!("migration upsert vector batch error: {error:?}");
                        error
                    })?;
                let receipt = transaction.commit().map_err(|error| {
                    eprintln!("migration search commit error: {error:?}");
                    error
                })?;
                self.observe_commit(&receipt);
                last_receipt = Some(receipt.into());
            }
        }
        last_receipt.ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))
    }

    /// Verifies migrated physical lexical and vector indexes against source
    /// payloads without changing the target.
    ///
    /// # Errors
    ///
    /// Returns a snapshot or corruption error when verification cannot run.
    pub fn migration_verify_search(
        &self,
        lexical_indexes: &[MigrationLexicalVerification],
        vector_indexes: &[MigrationVectorVerification],
    ) -> Result<bool, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        for (index, expected) in lexical_indexes {
            let Some(mut actual) = snapshot.inner.search_documents(*index) else {
                return Ok(false);
            };
            actual.sort_by(|left, right| left.0.cmp(&right.0));
            let mut expected = expected.clone();
            expected.sort_by(|left, right| left.0.cmp(&right.0));
            if actual != expected {
                return Ok(false);
            }
        }
        for (index, expected) in vector_indexes {
            let mut actual = snapshot.inner.vector_records(*index)?;
            actual.sort_by_key(|record| record.object_id);
            let mut expected = expected.clone();
            expected.sort_by_key(|record| record.0);
            if actual.len() != expected.len()
                || actual.iter().zip(expected).any(|(actual, expected)| {
                    actual.object_id != expected.0
                        || actual.vector.values().len() != expected.1.len()
                        || actual
                            .vector
                            .values()
                            .iter()
                            .zip(expected.1)
                            .any(|(actual, expected)| actual.to_bits() != expected.to_bits())
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Creates catalogued binary structure keyspaces for an external
    /// migration import, allocating stable identities inside one strict
    /// atomic commit.
    ///
    /// # Errors
    ///
    /// Returns a product, catalog, or durability error when the keyspaces
    /// cannot be created.
    pub fn migration_create_structure_keyspaces(
        &mut self,
        keyspaces: &[(String, StructureKind)],
    ) -> Result<Vec<(String, ObjectId)>, ProductError> {
        use hyphae_native_catalog::{
            CatalogObjectV2, DefinitionVersion, KeyspaceDefinition, KeyspaceEvictionPolicy,
            KeyspaceMemoryClass, KeyspaceTtlPolicy, ObjectHeaderV2, StructureOwnership,
        };
        use hyphae_native_types::{EngineKind, LogicalType};
        if keyspaces.is_empty() || keyspaces.len() > MAX_PRODUCT_TRANSACTION_OPERATIONS {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let invalid = || ProductError::from_code(ProductErrorCode::InvalidRequest);
        let qualified = |object: &str| -> Result<QualifiedName, ProductError> {
            Ok(QualifiedName::new(
                CatalogName::unquoted("main").map_err(|_| invalid())?,
                CatalogName::unquoted("public").map_err(|_| invalid())?,
                CatalogName::unquoted(object).map_err(|_| invalid())?,
            ))
        };
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        let database = transaction.next_catalog_object_id()?;
        transaction.create_catalog_object_v2(LogicalCatalogObject::V2(
            CatalogObjectV2::Database(ObjectHeaderV2 {
                id: database,
                owner: EngineKind::Kernel,
                name: qualified("database")?,
                parent: None,
                definition_version: DefinitionVersion::FIRST,
            }),
        ))?;
        let schema = transaction.next_catalog_object_id()?;
        transaction.create_catalog_object_v2(LogicalCatalogObject::V2(CatalogObjectV2::Schema(
            ObjectHeaderV2 {
                id: schema,
                owner: EngineKind::Kernel,
                name: qualified("schema")?,
                parent: Some(database),
                definition_version: DefinitionVersion::FIRST,
            },
        )))?;
        let mut created = Vec::with_capacity(keyspaces.len());
        for (name, kind) in keyspaces {
            let id = transaction.next_catalog_object_id()?;
            transaction.create_catalog_object_v2(LogicalCatalogObject::V2(
                CatalogObjectV2::Keyspace(KeyspaceDefinition {
                    header: ObjectHeaderV2 {
                        id,
                        owner: EngineKind::Structure,
                        name: qualified(name)?,
                        parent: Some(schema),
                        definition_version: DefinitionVersion::FIRST,
                    },
                    kind: *kind,
                    key_type: LogicalType::Binary,
                    value_type: LogicalType::Binary,
                    ownership: StructureOwnership::Canonical,
                    ttl_policy: KeyspaceTtlPolicy::PerValue,
                    default_ttl_millis: None,
                    memory_class: KeyspaceMemoryClass::Durable,
                    eviction: KeyspaceEvictionPolicy::None,
                    relation_schema: None,
                }),
            ))?;
            created.push((name.clone(), id));
        }
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(created)
    }

    /// Applies one bounded ordered batch of structure mutations under one
    /// strict native commit for an external migration import.
    ///
    /// # Errors
    ///
    /// Returns a product, validation, or durability error when the batch
    /// cannot be applied.
    pub fn migration_store_structures(
        &mut self,
        mutations: Vec<ProductStructureMutation>,
    ) -> Result<ProductCommitReceipt, ProductError> {
        if mutations.is_empty() {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        if mutations.len() > MAX_PRODUCT_TRANSACTION_OPERATIONS {
            return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
        }
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        for mutation in mutations {
            operation::apply_structure_mutation(&mut transaction, mutation)?;
        }
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Evaluates one bounded ordered batch of structure reads at one
    /// immutable snapshot pinned to the supplied logical time.
    ///
    /// # Errors
    ///
    /// Returns a product or snapshot error when any read cannot run.
    pub fn migration_read_structures(
        &self,
        logical_time_micros: i64,
        requests: Vec<ProductStructureReadRequest>,
    ) -> Result<Vec<ProductStructureReadResult>, ProductError> {
        if requests.is_empty() || requests.len() > MAX_PRODUCT_TRANSACTION_OPERATIONS {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        requests
            .into_iter()
            .map(|request| operation::read_structure(&snapshot, request))
            .collect()
    }

    /// Returns admitted product, catalog-format, and hard-limit capabilities.
    #[expect(
        clippy::unused_self,
        reason = "capabilities are exposed by the product facade"
    )]
    pub const fn capabilities(&self) -> ProductCapabilities {
        capabilities()
    }

    /// Creates a new native product directory.
    ///
    /// # Errors
    ///
    /// Returns a stable product error when the directory cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let database = NativeDatabase::create(path)?;
        let catalog_cursor_key = catalog_cursor_process_key()?;
        let mut product = Self {
            database,
            default_scalar_keyspace_id: None,
            telemetry: TelemetryRegistry::default(),
            access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
            access_control_epoch_known: AtomicBool::new(true),
            authorization_time_watermark: AtomicI64::new(i64::MIN),
            catalog_cursor_key,
            security_commit_interruption: None,
            #[cfg(test)]
            access_control_catalog_loads: AtomicU64::new(0),
        };
        product.ensure_default_scalar_keyspace()?;
        Ok(product)
    }

    /// Creates a Native migration target that is not authoritative until
    /// [`Self::promote_pending`] is called.
    ///
    /// # Errors
    ///
    /// Returns an error when the target cannot be initialized.
    pub fn create_pending(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let catalog_cursor_key = catalog_cursor_process_key()?;
        NativeDatabase::create_pending(path)
            .map(|database| Self {
                database,
                default_scalar_keyspace_id: None,
                telemetry: TelemetryRegistry::default(),
                access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
                access_control_epoch_known: AtomicBool::new(true),
                authorization_time_watermark: AtomicI64::new(i64::MIN),
                catalog_cursor_key,
                security_commit_interruption: None,
                #[cfg(test)]
                access_control_catalog_loads: AtomicU64::new(0),
            })
            .map_err(Into::into)
    }

    /// Opens an importer-owned pending migration target.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not an unpromoted migration target.
    pub fn open_pending(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let catalog_cursor_key = catalog_cursor_process_key()?;
        NativeDatabase::open_pending(path)
            .map(|database| Self {
                database,
                default_scalar_keyspace_id: None,
                telemetry: TelemetryRegistry::default(),
                access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
                access_control_epoch_known: AtomicBool::new(false),
                authorization_time_watermark: AtomicI64::new(i64::MIN),
                catalog_cursor_key,
                security_commit_interruption: None,
                #[cfg(test)]
                access_control_catalog_loads: AtomicU64::new(0),
            })
            .map_err(Into::into)
            .and_then(Self::initialize_pending_internal_state)
    }

    /// Publishes a pending migration target after the importer has validated it.
    ///
    /// # Errors
    ///
    /// Returns an error when marker promotion or directory synchronization fails.
    pub fn promote_pending(&mut self) -> Result<(), ProductError> {
        self.ensure_default_scalar_keyspace()?;
        self.database.promote_pending().map_err(Into::into)
    }

    /// Opens and verifies an existing native product directory.
    ///
    /// # Errors
    ///
    /// Returns a stable product error for ownership, I/O, or corruption.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let catalog_cursor_key = catalog_cursor_process_key()?;
        NativeDatabase::open(path)
            .map(|database| {
                let telemetry = TelemetryRegistry::default();
                telemetry.increment(MetricId::Recoveries, 1);
                Self {
                    database,
                    default_scalar_keyspace_id: None,
                    telemetry,
                    access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
                    access_control_epoch_known: AtomicBool::new(false),
                    authorization_time_watermark: AtomicI64::new(i64::MIN),
                    catalog_cursor_key,
                    security_commit_interruption: None,
                    #[cfg(test)]
                    access_control_catalog_loads: AtomicU64::new(0),
                }
            })
            .map_err(Into::into)
            .and_then(Self::initialize_internal_state)
    }

    /// Opens an existing directory for an explicit, lock-held metadata upgrade.
    ///
    /// A missing pre-1.2 default scalar binding is accepted only by this
    /// constructor. Any present malformed binding still fails as corruption.
    ///
    /// # Errors
    ///
    /// Returns ownership, lock, recovery, or durable corruption errors.
    pub fn open_for_upgrade(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let catalog_cursor_key = catalog_cursor_process_key()?;
        let database = NativeDatabase::open(path)?;
        let telemetry = TelemetryRegistry::default();
        telemetry.increment(MetricId::Recoveries, 1);
        let mut product = Self {
            database,
            default_scalar_keyspace_id: None,
            telemetry,
            access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
            access_control_epoch_known: AtomicBool::new(false),
            authorization_time_watermark: AtomicI64::new(i64::MIN),
            catalog_cursor_key,
            security_commit_interruption: None,
            #[cfg(test)]
            access_control_catalog_loads: AtomicU64::new(0),
        };
        let catalog = product.load_access_control_catalog()?;
        product.initialize_upgrade_default_scalar_keyspace()?;
        product.initialize_catalog_cursor_authority()?;
        product
            .access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        product
            .access_control_epoch_known
            .store(true, Ordering::Release);
        Ok(product)
    }

    /// Opens a directory exclusively after validating offline OS-owner authority.
    ///
    /// This constructor is reserved for bounded owner-recovery operations. It
    /// does not authenticate a managed credential or create a listener.
    ///
    /// # Errors
    ///
    /// Returns a stable owner-authority, lock, recovery, or corruption error.
    pub fn open_offline_owner(path: impl AsRef<Path>) -> Result<Self, ProductError> {
        let catalog_cursor_key = catalog_cursor_process_key()?;
        NativeDatabase::open_offline_owner(path)
            .map(|database| Self {
                database,
                default_scalar_keyspace_id: None,
                telemetry: TelemetryRegistry::default(),
                access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
                access_control_epoch_known: AtomicBool::new(false),
                authorization_time_watermark: AtomicI64::new(i64::MIN),
                catalog_cursor_key,
                security_commit_interruption: None,
                #[cfg(test)]
                access_control_catalog_loads: AtomicU64::new(0),
            })
            .map_err(Into::into)
            .and_then(Self::initialize_internal_state)
    }

    /// Explicitly migrates an unbootstrapped preview directory to the durable
    /// default scalar keyspace binding and returns the opened product.
    ///
    /// The directory lock is held from verified open through the strict atomic
    /// binding commit. A directory with any durable principal or an existing
    /// malformed binding fails closed.
    ///
    /// # Errors
    ///
    /// Returns a stable ownership, corruption, durability, or bootstrap-state
    /// error. It never adopts catalog objects by name.
    pub fn open_with_preview_default_scalar_migration(
        path: impl AsRef<Path>,
    ) -> Result<Self, ProductError> {
        let database = NativeDatabase::open(path)?;
        let catalog_cursor_key = catalog_cursor_process_key()?;
        let telemetry = TelemetryRegistry::default();
        telemetry.increment(MetricId::Recoveries, 1);
        let mut product = Self {
            database,
            default_scalar_keyspace_id: None,
            telemetry,
            access_control_epoch: AtomicU64::new(AuthorizationEpoch::UNMANAGED.get()),
            access_control_epoch_known: AtomicBool::new(false),
            authorization_time_watermark: AtomicI64::new(i64::MIN),
            catalog_cursor_key,
            security_commit_interruption: None,
            #[cfg(test)]
            access_control_catalog_loads: AtomicU64::new(0),
        };
        let catalog = product.load_access_control_catalog()?;
        if catalog.is_bootstrapped() {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        if product.has_default_scalar_binding()? {
            return product.initialize_internal_state();
        }
        product.ensure_default_scalar_keyspace()?;
        product.initialize_internal_state()
    }

    fn initialize_internal_state(mut self) -> Result<Self, ProductError> {
        let catalog = self.load_access_control_catalog()?;
        self.initialize_default_scalar_keyspace(catalog.is_bootstrapped())?;
        self.initialize_catalog_cursor_authority()?;
        self.access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        self.access_control_epoch_known
            .store(true, Ordering::Release);
        Ok(self)
    }

    fn initialize_pending_internal_state(mut self) -> Result<Self, ProductError> {
        let catalog = self.load_access_control_catalog()?;
        self.initialize_pending_default_scalar_keyspace(catalog.is_bootstrapped())?;
        self.initialize_catalog_cursor_authority()?;
        Ok(self)
    }

    /// Returns this instance's bounded process-local telemetry registry.
    pub const fn telemetry(&self) -> &TelemetryRegistry {
        &self.telemetry
    }

    /// Runs exclusive verified doctor against a directory that is not open by
    /// this product handle and records the classified attempt.
    pub fn doctor(&self, request: &DoctorRequest) -> DoctorReport {
        self.telemetry.increment(MetricId::DoctorRuns, 1);
        self.telemetry.record_event(TelemetryEvent {
            captured_at_micros: request.logical_time_micros,
            kind: TelemetryEventKind::Doctor,
        });
        let mut report = doctor(request);
        report.telemetry_registry_version = TELEMETRY_REGISTRY_VERSION;
        report.process_start_identity = self.telemetry.process_start_identity();
        report.session_start_identity = self.telemetry.session_start_identity();
        report
    }

    pub(crate) fn doctor_opened(&self, logical_time_micros: i64) -> DoctorReport {
        self.telemetry.increment(MetricId::DoctorRuns, 1);
        self.telemetry.record_event(TelemetryEvent {
            captured_at_micros: logical_time_micros,
            kind: TelemetryEventKind::Doctor,
        });
        let mut report = doctor::doctor_opened(&self.database, logical_time_micros);
        report.telemetry_registry_version = TELEMETRY_REGISTRY_VERSION;
        report.process_start_identity = self.telemetry.process_start_identity();
        report.session_start_identity = self.telemetry.session_start_identity();
        report
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
        let value = identified
            .object
            .ok_or_else(|| ProductError::catalog_object_not_found(Some(id)))?;
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
        let value = identified
            .object
            .ok_or_else(|| ProductError::catalog_object_not_found(None))?;
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
            return Err(ProductError::sql_statement_limit(
                MAX_PRODUCT_SQL_STATEMENT_BYTES,
                statement.len(),
            ));
        }
        let inner = self
            .database
            .prepare_sql_latest(statement)
            .map_err(ProductError::from)?;
        if inner.parameter_count() > MAX_PRODUCT_SQL_PARAMETERS {
            return Err(ProductError::sql_parameter_limit(
                MAX_PRODUCT_SQL_PARAMETERS,
                inner.parameter_count(),
            ));
        }
        if inner
            .maximum_result_rows()
            .is_none_or(|rows| rows > MAX_PRODUCT_SQL_ROWS)
        {
            return Err(ProductError::sql_row_limit(
                MAX_PRODUCT_SQL_ROWS,
                inner.maximum_result_rows().unwrap_or(usize::MAX),
            ));
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
        self.execute_prepared_with_checkpoint(prepared, parameters, || true)
    }

    pub(crate) fn execute_prepared_with_checkpoint(
        &self,
        prepared: &ProductPreparedStatement,
        parameters: &[ProductValue],
        checkpoint: impl FnMut() -> bool,
    ) -> Result<ProductRead<ProductSqlResult>, ProductError> {
        if prepared.directory_lineage != self.database.directory_identity().lineage().encode() {
            return Err(foreign_prepared_error());
        }
        if parameters.len() > MAX_PRODUCT_SQL_PARAMETERS {
            return Err(ProductError::sql_parameter_limit(
                MAX_PRODUCT_SQL_PARAMETERS,
                parameters.len(),
            ));
        }
        if parameters.len() != prepared.parameter_count() {
            return Err(ProductError::from(SqlError::ParameterMismatch));
        }
        let (visible_csn, catalog_version, root_digest, value) = self
            .database
            .execute_prepared_latest_identified_with_checkpoint(
                &prepared.inner,
                parameters,
                checkpoint,
            )
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

    /// Executes the prepared plan retained by one exact SQL read binding.
    ///
    /// # Errors
    ///
    /// Returns a stable SQL error for a non-read binding, stale catalog, or
    /// invalid parameter set.
    pub fn execute_bound_sql(
        &self,
        bound: &BoundSqlStatement,
        parameters: &[ProductValue],
    ) -> Result<ProductRead<ProductSqlResult>, ProductError> {
        self.execute_bound_sql_with_checkpoint(bound, parameters, || true)
    }

    pub(crate) fn execute_bound_sql_with_checkpoint(
        &self,
        bound: &BoundSqlStatement,
        parameters: &[ProductValue],
        checkpoint: impl FnMut() -> bool,
    ) -> Result<ProductRead<ProductSqlResult>, ProductError> {
        let prepared = bound
            .prepared_statement()
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
        if parameters.len() > MAX_PRODUCT_SQL_PARAMETERS {
            return Err(ProductError::sql_parameter_limit(
                MAX_PRODUCT_SQL_PARAMETERS,
                parameters.len(),
            ));
        }
        if parameters.len() != prepared.parameter_count() {
            return Err(ProductError::from(SqlError::ParameterMismatch));
        }
        let (visible_csn, catalog_version, root_digest, value) = self
            .database
            .execute_prepared_latest_identified_with_checkpoint(prepared, parameters, checkpoint)
            .map_err(ProductError::from)?;
        Ok(ProductRead {
            snapshot: SnapshotIdentity {
                directory_lineage: self.database.directory_identity().lineage().encode(),
                visible_csn: Some(visible_csn),
                catalog_version,
                root_digest,
                logical_time_micros: 0,
            },
            value: ProductSqlResult::from(value),
        })
    }
}

pub(crate) fn is_internal_structure_key(key: &[u8]) -> bool {
    key.starts_with(INTERNAL_STRUCTURE_KEY_PREFIX)
}

const fn migration_hnsw_seed(index: ObjectId) -> u64 {
    let bytes = index.get().to_le_bytes();
    let lower = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let upper = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    lower ^ upper
}

fn foreign_prepared_error() -> ProductError {
    ProductError::foreign_prepared()
}

#[cfg(test)]
mod tests {
    use std::{fs, io, path::PathBuf};

    use super::{
        MAX_PRODUCT_SQL_STATEMENT_BYTES, MigrationLexicalIndexInput, MigrationVectorIndexInput,
        NativeDatabase, NativeProduct, ObjectId, ProductError, ProductErrorCategory,
        ProductErrorCode, ProductRetry, ProductTransactionState, ProductValue,
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
    fn migration_search_imports_every_record_and_survives_reopen()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("migration-search-complete");
        let _ = fs::remove_dir_all(&path);
        let lexical_index = ObjectId::new(101)?;
        let vector_index = ObjectId::new(102)?;
        let documents = (0_u8..17)
            .map(|value| (vec![value], format!("document {value}")))
            .collect::<Vec<_>>();
        let vectors = (0_u8..17)
            .map(|value| -> Result<_, Box<dyn std::error::Error>> {
                Ok((
                    ObjectId::new(u128::from(value) + 1)?,
                    vec![f32::from(value), f32::from(value) + 0.5],
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let lexical = vec![MigrationLexicalIndexInput {
            index: lexical_index,
            name: "migration_lexical".to_owned(),
            documents: documents.clone(),
        }];
        let vector = vec![MigrationVectorIndexInput {
            index: vector_index,
            name: "migration_vector".to_owned(),
            dimension: 2,
            vectors: vectors.clone(),
        }];
        let expected_lexical = vec![(lexical_index, documents)];
        let expected_vectors = vec![(vector_index, vectors)];

        let mut product = NativeProduct::create(&path)?;
        product.migration_store_search(&lexical, &vector)?;
        assert!(product.migration_verify_search(&expected_lexical, &expected_vectors)?);
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        assert!(reopened.migration_verify_search(&expected_lexical, &expected_vectors)?);
        let mut incomplete = expected_vectors.clone();
        incomplete[0].1.pop();
        assert!(!reopened.migration_verify_search(&expected_lexical, &incomplete)?);
        drop(reopened);
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
        let contract = include_str!("../assets/product-error-v1.md")
            .lines()
            .filter_map(|line| {
                let columns = line
                    .strip_prefix("| `")?
                    .strip_suffix("` |")?
                    .split("` | `")
                    .collect::<Vec<_>>();
                match columns.as_slice() {
                    [code, category, retry] => Some(((*code).to_owned(), *category, *retry)),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let implementation = super::PRODUCT_ERROR_REGISTRY_V1
            .iter()
            .copied()
            .map(|definition| {
                (
                    definition.code().as_str().to_owned(),
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
            .map(|(code, _, _)| code)
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
        let product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;

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
        let left = NativeProduct::open_with_preview_default_scalar_migration(&left_path)?;
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
        let left = NativeProduct::open_with_preview_default_scalar_migration(&left_path)?;
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
