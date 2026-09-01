// SPDX-License-Identifier: Apache-2.0

//! Transport-independent product operations and embedded dispatcher.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use hyphae_native_catalog::{CatalogObjectV2, LogicalCatalogObject, StructureKind};
use hyphae_native_runtime::{
    BoundedSearchError, BoundedSearchLimits, BoundedSearchQuery, NativeDatabase,
    NativeExecutionPool, NativeResourceGovernor, NativeTransaction, NativeWriteBatch,
    SetAlgebraRequest, SqlStatementClass, classify_sql_statement,
};
use hyphae_native_types::{CanonicalF64, TransactionId};

pub use hyphae_native_runtime::CommitBoundary;

fn sql_mutation_checkpoint(context: &ProductRequestContext) -> bool {
    #[cfg(test)]
    if let Some(remaining) = &context.cancel_sql_mutation_after_checkpoints
        && remaining.fetch_sub(1, Ordering::AcqRel) == 1
    {
        context.cancellation.cancel();
    }
    context.checkpoint().is_ok()
}

use crate::proof::{NativeOperationProofArtifact, NativeProofGenerationLimits};

use crate::session::{ProductAuthorizationRequirement, ProductSessionSqlBindingKey};
use crate::{
    AccessControlMutationReceipt, AccessControlStatus, AdminStatus, ApiKeyActivationReceipt,
    ApiKeyConfirmationDigest, ApiKeyId, ApiKeyStartReceipt, AuthorizationEpoch, BackupInfo,
    BackupPhase, BackupProductError, BackupRequest, BuiltInRole, CatalogDependencyRequest,
    CatalogListRequest, CatalogObject, CatalogObjectSummary, CatalogPage,
    CatalogVisibleListRequest, CatalogVisiblePage, CustomRoleGrant, CustomRoleMutationReceipt,
    DoctorReport, DoctorRequest, MetricId, NativeProduct, ObjectId, ProductAuthorization,
    ProductCancellationToken, ProductCapabilities, ProductCheckpointReceipt, ProductCommitReceipt,
    ProductDurability, ProductError, ProductErrorCode, ProductExplain,
    ProductExplicitCommitReceipt, ProductExplicitTransactionStatus, ProductFailureBoundary,
    ProductHashEntry, ProductHashScanStop, ProductKeyEntry, ProductLimits, ProductListSide,
    ProductPermission, ProductPreparedHandle, ProductPrincipal, ProductRead,
    ProductRollbackReceipt, ProductScope, ProductScoreBound, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchIngestBatch, ProductSearchIngestReceipt,
    ProductSearchRequest, ProductSearchResult, ProductSession, ProductSessionId,
    ProductSortedSetEntry, ProductSortedSetOrder, ProductSqlResult, ProductStreamEntry,
    ProductStructureKey, ProductStructureMutation, ProductStructureMutationResult,
    ProductStructureRead, ProductStructureReadRequest, ProductStructureReadResult,
    ProductTransactionHandle, ProductTransactionId, ProductTransactionSearchMutation,
    ProductTransactionSqlMutation, ProductTransactionStageReceipt, ProductTransactionStageResult,
    ProductTransactionStatus, ProductTransactionVectorMutation, ProductTtl, ProductValue,
    ProgressControl, QualifiedName, RestoreRequest, RoleAssignmentMutationReceipt,
    SecurityAssignmentListRequest, SecurityAssignmentPage, SecurityAuditPage,
    SecurityAuditReadRequest, SecurityId, SecurityKeyListRequest, SecurityKeyPage,
    SecurityPrincipalListRequest, SecurityPrincipalMutationReceipt, SecurityPrincipalPage,
    SecurityRoleListRequest, SecurityRolePage, SnapshotIdentity, StatusRequest, TelemetryEvent,
    TelemetryEventKind, TelemetryRegistry, TimingClass,
};

/// Product-owned durability policy applied to every mutation in one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductDurabilityPolicy {
    /// Minimum acknowledgement selected by the caller.
    pub durability: ProductDurability,
}

impl ProductDurabilityPolicy {
    /// Requires strict physical synchronization.
    pub const STRICT: Self = Self {
        durability: ProductDurability::Strict,
    };
    /// Selects native group durability.
    pub const GROUP: Self = Self {
        durability: ProductDurability::Group,
    };
    /// Allows publication without crash-durability acknowledgement.
    pub const MEMORY: Self = Self {
        durability: ProductDurability::Memory,
    };
}

impl Default for ProductDurabilityPolicy {
    fn default() -> Self {
        Self::STRICT
    }
}

/// Complete execution authority and resource envelope for one request.
#[derive(Clone, Debug)]
pub struct ProductRequestContext {
    /// Caller-assigned diagnostic correlation identity.
    pub request_id: u128,
    /// Optional explicit mutation idempotency token, independent of request ID.
    pub idempotency_token: Option<u128>,
    /// Session that owns prepared handles and outcome evidence.
    pub session_id: ProductSessionId,
    /// Deterministic logical time used by TTL and snapshot reads.
    pub logical_time_micros: i64,
    /// Absolute Unix-time deadline in microseconds, when bounded.
    pub deadline_micros: Option<i64>,
    /// Cooperative cancellation observed before publication.
    pub cancellation: ProductCancellationToken,
    /// Central count, byte, work, and memory envelope.
    pub limits: ProductLimits,
    /// Authenticated caller identity.
    pub principal: ProductPrincipal,
    /// Authorization grants fixed by the authentication boundary.
    pub authorization: ProductAuthorization,
    /// Durable authorization generation fixed by the authentication boundary.
    pub authorization_epoch: AuthorizationEpoch,
    /// Durability policy for every mutating operation.
    pub durability: ProductDurabilityPolicy,
    /// Test-only deterministic SQL mutation cancellation countdown.
    #[cfg(test)]
    pub(crate) cancel_sql_mutation_after_checkpoints: Option<Arc<AtomicUsize>>,
    /// Test-only cancellation immediately after the commit reports publication.
    #[cfg(test)]
    pub(crate) cancel_sql_mutation_at_commit: bool,
}

impl ProductRequestContext {
    /// Creates a context with strict durability and default central limits.
    pub fn new(
        request_id: u128,
        session_id: ProductSessionId,
        logical_time_micros: i64,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
    ) -> Self {
        Self {
            request_id,
            idempotency_token: None,
            session_id,
            logical_time_micros,
            deadline_micros: None,
            cancellation: ProductCancellationToken::new(),
            limits: ProductLimits::default(),
            principal,
            authorization,
            authorization_epoch: AuthorizationEpoch::UNMANAGED,
            durability: ProductDurabilityPolicy::default(),
            #[cfg(test)]
            cancel_sql_mutation_after_checkpoints: None,
            #[cfg(test)]
            cancel_sql_mutation_at_commit: false,
        }
    }

    /// Binds this request to a durable authorization generation.
    #[must_use]
    pub const fn with_authorization_epoch(mut self, epoch: AuthorizationEpoch) -> Self {
        self.authorization_epoch = epoch;
        self
    }

    /// Attaches a stable nonzero idempotency token for one mutation attempt.
    #[must_use]
    pub const fn with_idempotency_token(mut self, token: u128) -> Self {
        self.idempotency_token = if token == 0 { None } else { Some(token) };
        self
    }

    /// Rejects cancellation or an elapsed deadline at a cooperative checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a request-bound cancelled or deadline error.
    pub fn checkpoint(&self) -> Result<(), ProductError> {
        if self.cancellation.is_cancelled() {
            return Err(self.error(ProductErrorCode::Cancelled));
        }
        if self
            .deadline_micros
            .is_some_and(|deadline| unix_time_micros() >= deadline)
        {
            return Err(self.error(ProductErrorCode::DeadlineExceeded));
        }
        Ok(())
    }

    pub(crate) fn deadline_elapsed(&self) -> bool {
        self.deadline_micros
            .is_some_and(|deadline| unix_time_micros() >= deadline)
    }

    fn error(&self, code: ProductErrorCode) -> ProductError {
        ProductError::from_code(code).with_request_id(self.request_id)
    }
}

/// Proven or unresolved commit outcome returned by mutation operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductCommitOutcome {
    /// Commit and selected durability are proven.
    Committed(ProductCommitReceipt),
    /// Publication may have occurred and status resolution is required.
    OutcomeUnknown {
        /// Resolution identity attached to the corresponding error.
        transaction_id: ProductTransactionId,
    },
}

/// Product-owned bounded lexical-search hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSearchHit {
    /// Stable caller-supplied document identity.
    pub document_id: Vec<u8>,
    /// Canonical score.
    pub score: crate::CanonicalF64,
}

/// Product-owned bounded lexical-search result and work receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSearchResults {
    /// Deterministically ordered matches.
    pub hits: Vec<ProductSearchHit>,
    /// Source documents examined.
    pub documents_examined: usize,
    /// Aggregate source bytes analyzed.
    pub source_bytes: usize,
    /// Aggregate clause-level token visits.
    pub token_visits: usize,
    /// Aggregate exact/prefix/phrase comparisons.
    pub token_comparisons: usize,
    /// Aggregate fuzzy dynamic-programming cells evaluated.
    pub fuzzy_steps: usize,
}

/// Closed operation set shared by the direct facade and local service.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProductOperation {
    /// Discover versions and hard limits.
    Capabilities,
    /// Resolve a current V1-compatible catalog object by ID.
    CatalogObject {
        /// Stable catalog identity.
        id: ObjectId,
    },
    /// Resolve a current V1-compatible catalog object by name.
    CatalogObjectNamed {
        /// Normalized qualified name.
        name: QualifiedName,
    },
    /// List one bounded current catalog page.
    CatalogList(CatalogListRequest),
    /// List only catalog objects visible under the current durable authority.
    CatalogVisibleList(CatalogVisibleListRequest),
    /// List one bounded current dependency page.
    CatalogDependencies(CatalogDependencyRequest),
    /// Describe one current logical V2 object.
    CatalogDescribe {
        /// Stable logical catalog identity.
        id: ObjectId,
    },
    /// Resolve one current logical V2 object by name.
    CatalogResolve {
        /// Normalized qualified name.
        name: QualifiedName,
    },
    /// Create one logical V2 catalog object.
    CatalogCreate {
        /// Complete validated logical definition.
        object: LogicalCatalogObject,
    },
    /// Bind and retain one prepared SQL read in the session.
    PrepareSql {
        /// Bounded SQL `SELECT` text.
        statement: String,
    },
    /// Release one session-local prepared SQL plan.
    DeallocatePrepared {
        /// Session-local retained plan.
        handle: ProductPreparedHandle,
    },
    /// Execute a session-local prepared SQL read.
    ExecutePrepared {
        /// Session-local retained plan.
        handle: ProductPreparedHandle,
        /// Canonical typed parameter values.
        parameters: Vec<ProductValue>,
    },
    /// Execute one admitted direct SQL DDL, DML, or query statement.
    ExecuteSql {
        /// Bounded SQL statement text.
        statement: String,
        /// Canonical typed parameter values.
        parameters: Vec<ProductValue>,
    },
    /// Read one scalar structure value.
    StructureGet {
        /// Exact binary scalar key.
        key: Vec<u8>,
    },
    /// Set one scalar structure value and optional absolute expiry.
    StructureSet {
        /// Exact binary scalar key.
        key: Vec<u8>,
        /// Exact binary scalar value.
        value: Vec<u8>,
        /// Optional absolute logical expiry.
        expires_at_micros: Option<i64>,
    },
    /// Read one scalar structure TTL.
    StructureTtl {
        /// Exact binary scalar key.
        key: Vec<u8>,
    },
    /// Atomically applies a nonempty batch across native structure families.
    StructureMutate {
        /// Ordered mutations committed under one CSN.
        mutations: Vec<ProductStructureMutation>,
    },
    /// Reads one non-scalar structure family at an immutable snapshot.
    StructureRead(ProductStructureReadRequest),
    /// Begins one detached explicit all-engine transaction.
    TransactionBegin,
    /// Stages one SQL DML statement in an explicit transaction.
    TransactionStageSql {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
        /// Complete SQL mutation.
        mutation: ProductTransactionSqlMutation,
    },
    /// Stages one structure mutation in an explicit transaction.
    TransactionStageStructure {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
        /// Complete structure mutation.
        mutation: ProductStructureMutation,
    },
    /// Stages one lexical document mutation in an explicit transaction.
    TransactionStageSearch {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
        /// Complete lexical mutation.
        mutation: ProductTransactionSearchMutation,
    },
    /// Stages one native vector mutation in an explicit transaction.
    TransactionStageVector {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
        /// Complete vector mutation.
        mutation: ProductTransactionVectorMutation,
    },
    /// Commits one explicit transaction under a single CSN.
    TransactionCommit {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
    },
    /// Discards one complete explicit transaction.
    TransactionRollback {
        /// Owning session-local transaction.
        handle: ProductTransactionHandle,
    },
    /// Reads one explicit transaction lifecycle.
    ExplicitTransactionStatus {
        /// Session-local transaction identity.
        handle: ProductTransactionHandle,
    },
    /// Resolve retained commit evidence.
    TransactionStatus {
        /// Product transaction identity from a prior outcome.
        transaction_id: ProductTransactionId,
    },
    /// Resolve durable outcome evidence by an explicit idempotency token.
    TransactionStatusByIdempotency {
        /// Caller-selected token bound to the authenticated principal.
        idempotency_token: u128,
    },
    /// Execute bounded lexical search through the native search engine.
    Search {
        /// Native search index identity.
        index: ObjectId,
        /// Bounded lexical expression.
        query: BoundedSearchQuery,
        /// Maximum returned hits.
        limit: usize,
    },
    /// Execute integrated catalog-bound lexical, vector, and doc-value search.
    SearchCollection {
        /// Logical Catalog V2 collection identity.
        collection: ObjectId,
        /// Complete bounded integrated request.
        request: ProductSearchRequest,
    },
    /// Atomically ingest integrated documents across native engines.
    SearchIngest {
        /// Logical Catalog V2 collection identity.
        collection: ObjectId,
        /// Complete bounded idempotent batch.
        batch: ProductSearchIngestBatch,
    },
    /// Atomically replaces one integrated document across every branch.
    SearchDocumentUpdate {
        /// Logical Catalog V2 collection identity.
        collection: ObjectId,
        /// Complete idempotent replacement.
        update: ProductSearchDocumentUpdate,
    },
    /// Atomically deletes one integrated document from every branch.
    SearchDocumentDelete {
        /// Logical Catalog V2 collection identity.
        collection: ObjectId,
        /// Complete idempotent deletion.
        delete: ProductSearchDocumentDelete,
    },
    /// Capture current administrative status.
    AdminStatus,
    /// Publish one synchronized checkpoint.
    AdminCheckpoint,
    /// Explain one admitted SQL statement without exposing a private plan type.
    AdminExplainSql {
        /// SQL statement to explain.
        statement: String,
    },
    /// Run exclusive directory diagnosis.
    Doctor(DoctorRequest),
    /// Create and verify a native backup.
    Backup(BackupRequest),
    /// Verifies and restores a backup to a separate new native directory.
    Restore(RestoreRequest),
    /// Capture the bounded process-local telemetry registry.
    Telemetry,
    /// Verify canonical native proof and witness artifacts offline.
    VerifyProof {
        /// Complete encoded `HYNPRF02` proof.
        proof: Vec<u8>,
        /// Complete encoded native witness.
        witness: Vec<u8>,
        /// Independently obtained trusted anchor digest.
        trusted_anchor: [u8; 32],
    },
    /// Read redacted access-control catalog status.
    SecurityStatus,
    /// List one redacted principal page.
    SecurityPrincipalList(SecurityPrincipalListRequest),
    /// List one redacted built-in and custom role page.
    SecurityRoleList(SecurityRoleListRequest),
    /// List one redacted built-in and custom assignment page.
    SecurityAssignmentList(SecurityAssignmentListRequest),
    /// List one redacted API-key metadata page.
    SecurityKeyList(SecurityKeyListRequest),
    /// Read one bounded security-audit page.
    SecurityAuditRead(SecurityAuditReadRequest),
    /// Create one disabled durable principal.
    SecurityPrincipalCreate {
        /// Bounded display name.
        display_name: String,
    },
    /// Enable or disable one durable principal.
    SecurityPrincipalSetEnabled {
        /// Stable principal identity.
        principal_id: SecurityId,
        /// New authentication state.
        enabled: bool,
    },
    /// Create one immutable custom role.
    SecurityCustomRoleCreate {
        /// Bounded display name.
        display_name: String,
        /// Canonical nonempty grants.
        grants: Vec<CustomRoleGrant>,
    },
    /// Create one non-owner built-in role assignment.
    SecurityBuiltInAssignmentCreate {
        /// Stable target principal.
        principal_id: SecurityId,
        /// Built-in role other than owner.
        role: BuiltInRole,
        /// Stable assignment scope.
        scope: ProductScope,
    },
    /// Create one custom-role assignment.
    SecurityCustomAssignmentCreate {
        /// Stable target principal.
        principal_id: SecurityId,
        /// Stable custom-role identity.
        role_id: SecurityId,
    },
    /// Revoke one non-owner role assignment.
    SecurityAssignmentRevoke {
        /// Stable assignment identity.
        assignment_id: SecurityId,
    },
    /// Starts one self-managed inactive API key.
    SecurityApiKeyIssueSelfStart {
        /// Stable target principal, which must equal the actor principal.
        principal_id: SecurityId,
        /// Non-secret bounded label.
        label: String,
        /// Canonically selected built-in roles.
        roles: Vec<BuiltInRole>,
        /// Canonically selected custom-role identities.
        custom_roles: Vec<SecurityId>,
        /// Permission ceiling no wider than the actor.
        permission_ceiling: ProductAuthorization,
        /// Scope ceiling no wider than the actor.
        scope_ceiling: Vec<ProductScope>,
        /// Optional exclusive expiry.
        expires_at_micros: Option<i64>,
    },
    /// Starts one administratively managed inactive API key.
    SecurityApiKeyIssueStart {
        /// Stable target principal.
        principal_id: SecurityId,
        /// Non-secret bounded label.
        label: String,
        /// Canonically selected built-in roles.
        roles: Vec<BuiltInRole>,
        /// Canonically selected custom-role identities.
        custom_roles: Vec<SecurityId>,
        /// Credential permission ceiling.
        permission_ceiling: ProductAuthorization,
        /// Credential scope ceiling.
        scope_ceiling: Vec<ProductScope>,
        /// Optional exclusive expiry.
        expires_at_micros: Option<i64>,
    },
    /// Activates one exact self-managed pending issue.
    SecurityApiKeyIssueSelfActivate {
        /// Public pending key identity.
        key_id: ApiKeyId,
        /// Confirmation digest derived from the delivered secret.
        confirmation_digest: ApiKeyConfirmationDigest,
    },
    /// Activates one exact administratively managed pending issue.
    SecurityApiKeyIssueActivate {
        /// Public pending key identity.
        key_id: ApiKeyId,
        /// Confirmation digest derived from the delivered secret.
        confirmation_digest: ApiKeyConfirmationDigest,
    },
    /// Starts one self-managed inactive rotation successor.
    SecurityApiKeyRotateSelfStart {
        /// Active predecessor identity.
        predecessor_key_id: ApiKeyId,
        /// Non-secret successor label.
        label: String,
        /// Bounded predecessor overlap after activation.
        overlap_seconds: u64,
        /// Optional successor expiry.
        expires_at_micros: Option<i64>,
    },
    /// Starts one administratively managed inactive rotation successor.
    SecurityApiKeyRotateStart {
        /// Active predecessor identity.
        predecessor_key_id: ApiKeyId,
        /// Non-secret successor label.
        label: String,
        /// Bounded predecessor overlap after activation.
        overlap_seconds: u64,
        /// Optional successor expiry.
        expires_at_micros: Option<i64>,
    },
    /// Activates one exact self-managed rotation successor.
    SecurityApiKeyRotateSelfActivate {
        /// Public successor identity.
        successor_key_id: ApiKeyId,
        /// Confirmation digest derived from the delivered secret.
        confirmation_digest: ApiKeyConfirmationDigest,
    },
    /// Activates one exact administratively managed rotation successor.
    SecurityApiKeyRotateActivate {
        /// Public successor identity.
        successor_key_id: ApiKeyId,
        /// Confirmation digest derived from the delivered secret.
        confirmation_digest: ApiKeyConfirmationDigest,
    },
    /// Aborts one exact self-managed pending issue.
    SecurityApiKeyIssueSelfAbort {
        /// Exact pending issue identity.
        key_id: ApiKeyId,
    },
    /// Aborts one exact administratively managed pending issue.
    SecurityApiKeyIssueAbort {
        /// Exact pending issue identity.
        key_id: ApiKeyId,
    },
    /// Aborts one exact self-managed pending rotation.
    SecurityApiKeyRotateSelfAbort {
        /// Exact pending successor identity.
        successor_key_id: ApiKeyId,
    },
    /// Aborts one exact administratively managed pending rotation.
    SecurityApiKeyRotateAbort {
        /// Exact pending successor identity.
        successor_key_id: ApiKeyId,
    },
    /// Revokes one exact self-managed active key.
    SecurityApiKeyRevokeSelf {
        /// Exact active key identity.
        key_id: ApiKeyId,
    },
    /// Revokes one exact administratively managed active key.
    SecurityApiKeyRevoke {
        /// Exact active key identity.
        key_id: ApiKeyId,
    },
    /// Permanently revokes the migrated Native HTTP legacy bearer.
    SecurityLegacyBearerRevoke,
    /// Execute one eligible read and retain an offline-verifiable semantic proof.
    Prove {
        /// Read operation to execute exactly once for proof generation.
        operation: Box<ProductOperation>,
        /// Explicit proof, witness, and semantic bounds.
        limits: NativeProofGenerationLimits,
    },
}

/// Closed transport-independent response set.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProductResponse {
    /// Capability discovery.
    Capabilities(ProductCapabilities),
    /// Current V1-compatible catalog object.
    CatalogObject(ProductRead<CatalogObject>),
    /// One bounded logical catalog page.
    CatalogPage(CatalogPage<CatalogObjectSummary>),
    /// Scope-visible logical catalog page with an opaque continuation.
    CatalogVisiblePage(CatalogVisiblePage),
    /// One bounded logical dependency page.
    CatalogDependencyPage(CatalogPage<hyphae_native_catalog::DependencyEdge>),
    /// Optional complete logical catalog definition.
    CatalogDefinition(Option<LogicalCatalogObject>),
    /// Logical catalog creation commit.
    CatalogCreated(ProductCommitOutcome),
    /// Retained prepared-plan metadata.
    PreparedSql {
        /// Session-local retained plan.
        handle: ProductPreparedHandle,
        /// Catalog version used to bind the plan.
        catalog_version: crate::CatalogVersion,
        /// Exact parameter arity.
        parameter_count: usize,
        /// Admitted maximum result rows.
        maximum_result_rows: usize,
    },
    /// Prepared plan was released.
    Deallocated,
    /// SQL result with read identity or mutation outcome.
    Sql {
        /// Statement result.
        result: ProductSqlResult,
        /// Exact read snapshot, absent for mutations.
        snapshot: Option<SnapshotIdentity>,
        /// Mutation commit evidence, absent for reads.
        commit: Option<ProductCommitOutcome>,
    },
    /// Scalar structure value.
    StructureValue(Option<Vec<u8>>),
    /// Scalar structure mutation outcome.
    StructureSet(ProductCommitOutcome),
    /// Scalar structure TTL.
    StructureTtl(ProductTtl),
    /// Atomic non-scalar structure mutation outcome.
    StructureMutated(ProductCommitOutcome),
    /// Snapshot-bound non-scalar structure read.
    StructureRead(ProductStructureRead),
    /// Explicit transaction began or its current status was requested.
    ExplicitTransactionStatus(ProductExplicitTransactionStatus),
    /// One explicit transaction mutation was staged.
    TransactionStaged(ProductTransactionStageReceipt),
    /// One explicit all-engine transaction committed.
    TransactionCommitted(ProductExplicitCommitReceipt),
    /// One explicit transaction rolled back.
    TransactionRolledBack(ProductRollbackReceipt),
    /// Retained transaction evidence.
    TransactionStatus(ProductTransactionStatus),
    /// Bounded lexical-search result.
    Search(ProductSearchResults),
    /// Integrated lexical/vector/doc-value result.
    IntegratedSearch(ProductSearchResult),
    /// Atomic integrated ingestion outcome.
    SearchIngested(ProductSearchIngestReceipt),
    /// Current administration status.
    AdminStatus(AdminStatus),
    /// Synchronized checkpoint receipt.
    AdminCheckpoint(ProductCheckpointReceipt),
    /// Product-owned typed explanation.
    Explain(ProductExplain),
    /// Typed doctor report.
    Doctor(DoctorReport),
    /// Verified promoted backup metadata.
    Backup(BackupInfo),
    /// Verified restored-directory metadata and mandatory doctor report.
    Restore(crate::RestoreInfo),
    /// Bounded telemetry snapshot.
    Telemetry(crate::TelemetrySnapshot),
    /// Origin-independent proof verification report.
    ProofVerification(crate::proof::NativeProofVerificationReport),
    /// Redacted access-control catalog status.
    SecurityStatus(AccessControlStatus),
    /// One bounded principal page.
    SecurityPrincipalPage(SecurityPrincipalPage),
    /// One bounded role page.
    SecurityRolePage(SecurityRolePage),
    /// One bounded role-assignment page.
    SecurityAssignmentPage(SecurityAssignmentPage),
    /// One bounded API-key metadata page.
    SecurityKeyPage(SecurityKeyPage),
    /// One bounded security-audit page.
    SecurityAuditPage(SecurityAuditPage),
    /// One durable principal creation receipt.
    SecurityPrincipalMutated(SecurityPrincipalMutationReceipt),
    /// One durable custom-role creation receipt.
    SecurityCustomRoleMutated(CustomRoleMutationReceipt),
    /// One durable role-assignment creation receipt.
    SecurityAssignmentMutated(RoleAssignmentMutationReceipt),
    /// One durable access-control state-change receipt.
    SecurityMutated(AccessControlMutationReceipt),
    /// One-time first-phase API-key secret delivery.
    SecurityApiKeyStarted(ApiKeyStartReceipt),
    /// Definite issue or rotation activation receipt.
    SecurityApiKeyActivated(ApiKeyActivationReceipt),
    /// Actual read response paired with its complete portable proof artifacts.
    Proven {
        /// Response produced by the operation integrated with proof generation.
        response: Box<ProductResponse>,
        /// Proof, retained witness, and external anchor value.
        artifact: Box<NativeOperationProofArtifact>,
    },
}

impl NativeProduct {
    /// Installs one verified governor and persistent execution pool in the
    /// product's embedded database.
    ///
    /// # Errors
    ///
    /// Returns an error when the pool does not match the governor policy.
    pub fn set_resource_governor_with_execution_pool(
        &mut self,
        governor: Arc<NativeResourceGovernor>,
        execution_pool: Arc<NativeExecutionPool>,
        maximum_wait: Duration,
    ) -> Result<(), ProductError> {
        self.database
            .set_resource_governor_with_execution_pool(governor, execution_pool, maximum_wait)
            .map_err(Into::into)
    }

    /// Returns whether both halves of the embedded execution authority exist.
    pub fn has_execution_authority(&self) -> bool {
        self.database.resource_governor().is_some() && self.database.execution_pool().is_some()
    }

    /// Executes one product operation directly through the shared dispatcher.
    ///
    /// All admission, authorization, cancellation, durability, and failure
    /// semantics are identical to [`crate::NativeProductService`].
    ///
    /// # Errors
    ///
    /// Returns a request-bound product error for admission or execution failure.
    pub fn dispatch(
        &mut self,
        session: &mut ProductSession,
        context: &ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<ProductResponse, ProductError> {
        dispatch(self, session, context, operation)
    }
}

pub(crate) fn dispatch(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    operation: ProductOperation,
) -> Result<ProductResponse, ProductError> {
    let telemetry = product.telemetry.clone();
    let rollback_on_authority_loss = session.is_managed()
        && validate_context(session, context).is_ok()
        && matches!(operation, ProductOperation::TransactionCommit { .. });
    let admitted = record_admission(product, &telemetry, session, context, &operation);
    if rollback_on_authority_loss
        && admitted
            .as_ref()
            .is_err_and(|error| error.code() == ProductErrorCode::AuthorizationDenied)
        && let ProductOperation::TransactionCommit { handle } = &operation
    {
        session.rollback_active_transaction_after_authority_loss(*handle);
    }
    let sql_binding_key = request_sql_binding_key(context, &operation);
    let result = admitted.and_then(|()| {
        let execution_started = Instant::now();
        let result = dispatch_inner(product, session, context, operation);
        telemetry.record_timing(TimingClass::EngineExecution, execution_started.elapsed());
        result
    });
    if let Some(key) = sql_binding_key {
        let _ = session.take_sql_binding(key);
    }
    record_dispatch_result(&telemetry, context, result)
}

fn request_sql_binding_key(
    context: &ProductRequestContext,
    operation: &ProductOperation,
) -> Option<ProductSessionSqlBindingKey> {
    match operation {
        ProductOperation::ExecuteSql { .. } => {
            Some(ProductSessionSqlBindingKey::Execute(context.request_id))
        }
        ProductOperation::AdminExplainSql { .. } => {
            Some(ProductSessionSqlBindingKey::Explain(context.request_id))
        }
        ProductOperation::TransactionStageSql { handle, .. } => {
            Some(ProductSessionSqlBindingKey::Stage {
                request_id: context.request_id,
                handle: *handle,
            })
        }
        ProductOperation::Prove { operation, .. }
            if matches!(operation.as_ref(), ProductOperation::ExecuteSql { .. }) =>
        {
            Some(ProductSessionSqlBindingKey::Execute(context.request_id))
        }
        _ => None,
    }
}

fn record_admission(
    product: &NativeProduct,
    telemetry: &TelemetryRegistry,
    session: &ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
) -> Result<(), ProductError> {
    let admission_started = Instant::now();
    telemetry.increment(MetricId::Requests, 1);
    let admitted = admit_operation(product, session, context, operation);
    telemetry.record_timing(TimingClass::Admission, admission_started.elapsed());
    admitted
}

fn record_dispatch_result(
    telemetry: &TelemetryRegistry,
    context: &ProductRequestContext,
    result: Result<ProductResponse, ProductError>,
) -> Result<ProductResponse, ProductError> {
    let result = result.map_err(|error| {
        if error.request_id().is_some() {
            error
        } else {
            error.with_request_id(context.request_id)
        }
    });
    if let Err(error) = &result {
        telemetry.increment(MetricId::Errors, 1);
        let kind = match error.code() {
            ProductErrorCode::Cancelled => {
                telemetry.increment(MetricId::Cancellations, 1);
                TelemetryEventKind::Cancelled
            }
            ProductErrorCode::DeadlineExceeded => {
                telemetry.increment(MetricId::Deadlines, 1);
                TelemetryEventKind::Deadline
            }
            _ => TelemetryEventKind::Error(error.category()),
        };
        telemetry.record_event(TelemetryEvent {
            captured_at_micros: context.logical_time_micros,
            kind,
        });
    }
    result
}

pub(crate) fn dispatch_structure_get_read_only(
    product: &NativeProduct,
    session: &ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
) -> Result<ProductResponse, ProductError> {
    let ProductOperation::StructureGet { key } = &operation else {
        return Err(context.error(ProductErrorCode::InvalidRequest));
    };
    let telemetry = product.telemetry.clone();
    let admitted = record_admission(product, &telemetry, session, context, operation);
    let result = admitted.and_then(|()| {
        let execution_started = Instant::now();
        let result = (|| {
            let response = ProductResponse::StructureValue(
                product
                    .database
                    .get_latest_structure(key, context.logical_time_micros)?,
            );
            admit_response(context, &response)?;
            Ok(response)
        })();
        telemetry.record_timing(TimingClass::EngineExecution, execution_started.elapsed());
        result
    });
    record_dispatch_result(&telemetry, context, result)
}

#[allow(clippy::too_many_lines)]
fn dispatch_inner(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    operation: ProductOperation,
) -> Result<ProductResponse, ProductError> {
    let read_only = !operation.is_mutating();
    let key_self_manage = operation.is_self_key_lifecycle();
    let mut response_limit_already_enforced = false;
    let response = match operation {
        ProductOperation::Capabilities => ProductResponse::Capabilities(product.capabilities()),
        ProductOperation::CatalogObject { id } => {
            ProductResponse::CatalogObject(product.catalog_object(id)?)
        }
        ProductOperation::CatalogObjectNamed { name } => {
            ProductResponse::CatalogObject(product.catalog_object_named(&name)?)
        }
        ProductOperation::CatalogList(request) => {
            let snapshot = product.catalog_snapshot()?;
            let response = ProductResponse::CatalogPage(product.catalog_list(&snapshot, request)?);
            admit_response(context, &response)?;
            response_limit_already_enforced = true;
            response
        }
        ProductOperation::CatalogVisibleList(request) => {
            let authority = session.authenticated_authority()?;
            if authority.is_none() {
                product.ensure_unmanaged_catalog_cursor_authority()?;
            }
            let snapshot = product.catalog_snapshot()?;
            let (scopes, cursor_key, authorization_epoch) = match authority {
                None => (
                    vec![ProductScope::Instance],
                    unmanaged_catalog_cursor_key(product.catalog_cursor_key, session.principal()),
                    context.authorization_epoch,
                ),
                Some(authority) => {
                    let scopes = exact_catalog_visible_scopes(product, &authority, &snapshot)?;
                    (
                        scopes,
                        authority.catalog_cursor_key(),
                        authority.authorization_epoch(),
                    )
                }
            };
            let response = ProductResponse::CatalogVisiblePage(product.catalog_visible_list(
                &snapshot,
                &scopes,
                cursor_key,
                authorization_epoch,
                &request,
            )?);
            admit_response(context, &response)?;
            response_limit_already_enforced = true;
            response
        }
        ProductOperation::CatalogDependencies(request) => {
            let snapshot = product.catalog_snapshot()?;
            let response = ProductResponse::CatalogDependencyPage(
                product.catalog_dependencies(&snapshot, request)?,
            );
            admit_response(context, &response)?;
            response_limit_already_enforced = true;
            response
        }
        ProductOperation::CatalogDescribe { id } => {
            let snapshot = product.catalog_snapshot()?;
            ProductResponse::CatalogDefinition(product.catalog_describe(&snapshot, id)?)
        }
        ProductOperation::CatalogResolve { name } => {
            let snapshot = product.catalog_snapshot()?;
            ProductResponse::CatalogDefinition(product.catalog_resolve(&snapshot, &name)?)
        }
        ProductOperation::CatalogCreate { object } => {
            let mut transaction = product.database.begin(
                context.logical_time_micros,
                context.durability.durability.into(),
            )?;
            transaction.create_catalog_object_v2(object)?;
            context.checkpoint()?;
            let telemetry = product.telemetry.clone();
            let receipt = commit(&telemetry, transaction, session, context)?;
            ProductResponse::CatalogCreated(ProductCommitOutcome::Committed(receipt))
        }
        ProductOperation::PrepareSql { statement } => {
            let planning_started = Instant::now();
            let prepared = product.prepare_sql(&statement)?;
            product
                .telemetry
                .record_timing(TimingClass::Planning, planning_started.elapsed());
            let catalog_version = prepared.catalog_version();
            let parameter_count = prepared.parameter_count();
            let maximum_result_rows = prepared.maximum_result_rows();
            let handle = session
                .retain_prepared(prepared)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
            ProductResponse::PreparedSql {
                handle,
                catalog_version,
                parameter_count,
                maximum_result_rows,
            }
        }
        ProductOperation::DeallocatePrepared { handle } => {
            if !session.deallocate(handle) {
                return Err(ProductError::from_code(ProductErrorCode::SqlInvalidValue));
            }
            ProductResponse::Deallocated
        }
        ProductOperation::ExecutePrepared { handle, parameters } => {
            let prepared = session
                .prepared(handle)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::SqlInvalidValue))?;
            if prepared.maximum_result_rows() > context.limits.max_count {
                return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
            }
            context.checkpoint()?;
            let result = product
                .execute_prepared_with_checkpoint(prepared, &parameters, || {
                    context.checkpoint().is_ok()
                })
                .map_err(|error| map_execution_interruption(error, context))?;
            context.checkpoint()?;
            let response = ProductResponse::Sql {
                result: result.value,
                snapshot: Some(result.snapshot),
                commit: None,
            };
            admit_response(context, &response)?;
            response_limit_already_enforced = true;
            response
        }
        ProductOperation::ExecuteSql {
            statement,
            parameters,
        } => {
            let bound = session
                .take_sql_binding(ProductSessionSqlBindingKey::Execute(context.request_id))?
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
            context.checkpoint()?;
            let response = execute_sql(product, session, context, &statement, &parameters, &bound)?;
            if bound.class() == SqlStatementClass::Read {
                context.checkpoint()?;
            }
            response
        }
        ProductOperation::StructureGet { key } => ProductResponse::StructureValue(
            product
                .database
                .get_latest_structure(&key, context.logical_time_micros)?,
        ),
        ProductOperation::StructureSet {
            key,
            value,
            expires_at_micros,
        } => {
            let mut transaction = product.database.begin(
                context.logical_time_micros,
                context.durability.durability.into(),
            )?;
            transaction.set(key, value, expires_at_micros)?;
            context.checkpoint()?;
            let telemetry = product.telemetry.clone();
            let receipt = commit(&telemetry, transaction, session, context)?;
            ProductResponse::StructureSet(ProductCommitOutcome::Committed(receipt))
        }
        ProductOperation::StructureTtl { key } => ProductResponse::StructureTtl(
            product
                .database
                .ttl_latest_structure(&key, context.logical_time_micros)?
                .into(),
        ),
        ProductOperation::StructureMutate { mutations } => {
            if mutations.is_empty() {
                return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
            }
            let mut transaction = product.database.begin(
                context.logical_time_micros,
                context.durability.durability.into(),
            )?;
            for mutation in mutations {
                let _result = apply_structure_mutation(&mut transaction, mutation)?;
            }
            context.checkpoint()?;
            let telemetry = product.telemetry.clone();
            let receipt = commit(&telemetry, transaction, session, context)?;
            ProductResponse::StructureMutated(ProductCommitOutcome::Committed(receipt))
        }
        ProductOperation::StructureRead(request) => {
            let snapshot = product.snapshot_bounded(context.logical_time_micros)?;
            let identity = snapshot.identity();
            let value = read_structure(&snapshot, request)?;
            ProductResponse::StructureRead(ProductRead {
                snapshot: identity,
                value,
            })
        }
        ProductOperation::TransactionBegin => {
            context.checkpoint()?;
            let batch = product.database.begin_optimistic(
                context.logical_time_micros,
                context.durability.durability.into(),
            )?;
            let status = session
                .begin_transaction(batch, context.durability.durability)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
            ProductResponse::ExplicitTransactionStatus(status)
        }
        ProductOperation::TransactionStageSql { handle, mutation } => {
            context.checkpoint()?;
            let bound = session
                .take_sql_binding(ProductSessionSqlBindingKey::Stage {
                    request_id: context.request_id,
                    handle,
                })?
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
            let authorization = sql_binding_requirement(bound.class(), &bound);
            ProductResponse::TransactionStaged(stage_transaction(
                &product.database,
                session,
                handle,
                &authorization,
                context.limits,
                |batch| {
                    validate_transaction_sql(&mutation)?;
                    let result = batch
                        .execute_bound_sql_with_checkpoint(&bound, &mutation.parameters, || {
                            sql_mutation_checkpoint(context)
                        })
                        .map_err(ProductError::from)
                        .map_err(|error| map_execution_interruption(error, context))?;
                    context.checkpoint()?;
                    Ok(ProductTransactionStageResult::Sql(result.into()))
                },
            )?)
        }
        ProductOperation::TransactionStageStructure { handle, mutation } => {
            context.checkpoint()?;
            let authorization = ProductAuthorizationRequirement::object(
                authorization([ProductPermission::DataWrite]),
                mutation.structure_key().keyspace,
            );
            ProductResponse::TransactionStaged(stage_transaction(
                &product.database,
                session,
                handle,
                &authorization,
                context.limits,
                |batch| {
                    let result = apply_structure_mutation(batch, mutation)?;
                    Ok(ProductTransactionStageResult::Structure(result))
                },
            )?)
        }
        ProductOperation::TransactionStageSearch { handle, mutation } => {
            context.checkpoint()?;
            let authorization = ProductAuthorizationRequirement::object(
                authorization([ProductPermission::CatalogRead, ProductPermission::DataWrite]),
                mutation.index(),
            );
            ProductResponse::TransactionStaged(stage_transaction(
                &product.database,
                session,
                handle,
                &authorization,
                context.limits,
                |batch| {
                    apply_search_mutation(product, batch, mutation, context.logical_time_micros)?;
                    Ok(ProductTransactionStageResult::Search)
                },
            )?)
        }
        ProductOperation::TransactionStageVector { handle, mutation } => {
            context.checkpoint()?;
            let authorization = ProductAuthorizationRequirement::object(
                authorization([ProductPermission::CatalogRead, ProductPermission::DataWrite]),
                mutation.index(),
            );
            ProductResponse::TransactionStaged(stage_transaction(
                &product.database,
                session,
                handle,
                &authorization,
                context.limits,
                |batch| {
                    let changed = apply_vector_mutation(batch, mutation)?;
                    Ok(ProductTransactionStageResult::Vector(changed))
                },
            )?)
        }
        ProductOperation::TransactionCommit { handle } => {
            context.checkpoint()?;
            let transaction = session
                .take_active_transaction(handle)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
            if transaction.staged_operations == 0 || transaction.batch.mutation_count() == 0 {
                session.replace_active_transaction(handle, transaction);
                return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
            }
            let staged_operations = transaction.staged_operations;
            match product.database.commit_optimistic_resolved(
                transaction.batch,
                principal_hash(context.principal.identity()),
                explicit_idempotency_token(context, handle),
            ) {
                Ok((resolution, receipt)) => {
                    product.observe_commit(&receipt);
                    let transaction_id = ProductTransactionId::from(resolution.resolution_id);
                    let receipt = ProductCommitReceipt::from_runtime(receipt, transaction_id);
                    session.record_transaction(
                        receipt.transaction_id,
                        ProductTransactionStatus::Committed(receipt),
                    );
                    session.record_explicit_status(
                        handle,
                        ProductExplicitTransactionStatus::Committed {
                            handle,
                            staged_operations,
                            receipt,
                        },
                    );
                    ProductResponse::TransactionCommitted(ProductExplicitCommitReceipt {
                        handle,
                        staged_operations,
                        commit: receipt,
                    })
                }
                Err(error) => {
                    let resolution_id = error
                        .resolution()
                        .map(|resolution| ProductTransactionId::from(resolution.resolution_id));
                    return Err(handle_explicit_commit_error(
                        session,
                        handle,
                        staged_operations,
                        resolution_id,
                        error.into_source(),
                    ));
                }
            }
        }
        ProductOperation::TransactionRollback { handle } => {
            context.checkpoint()?;
            let transaction = session
                .take_active_transaction(handle)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
            let discarded_operations = transaction.staged_operations;
            transaction.batch.rollback();
            let status = ProductExplicitTransactionStatus::RolledBack {
                handle,
                discarded_operations,
            };
            session.record_explicit_status(handle, status);
            ProductResponse::TransactionRolledBack(ProductRollbackReceipt {
                handle,
                discarded_operations,
            })
        }
        ProductOperation::ExplicitTransactionStatus { handle } => {
            ProductResponse::ExplicitTransactionStatus(session.explicit_transaction_status(handle))
        }
        ProductOperation::SearchCollection {
            collection,
            request,
        } => ProductResponse::IntegratedSearch(product.search_collection_with_checkpoint(
            collection,
            &request,
            context.logical_time_micros,
            || context.checkpoint(),
        )?),
        ProductOperation::SearchIngest { collection, batch } => {
            ProductResponse::SearchIngested(product.ingest_search_batch(
                collection,
                &batch,
                context.logical_time_micros,
                context.durability.durability,
            )?)
        }
        ProductOperation::SearchDocumentUpdate { collection, update } => {
            ProductResponse::SearchIngested(product.update_search_document(
                collection,
                &update,
                context.logical_time_micros,
                context.durability.durability,
            )?)
        }
        ProductOperation::SearchDocumentDelete { collection, delete } => {
            ProductResponse::SearchIngested(product.delete_search_document(
                collection,
                delete,
                context.logical_time_micros,
                context.durability.durability,
            )?)
        }
        ProductOperation::TransactionStatus { transaction_id } => {
            ProductResponse::TransactionStatus(resolve_transaction_status(
                product,
                session,
                context,
                transaction_id,
            ))
        }
        ProductOperation::TransactionStatusByIdempotency { idempotency_token } => {
            ProductResponse::TransactionStatus(resolve_transaction_status_by_token(
                product,
                context,
                idempotency_token,
            ))
        }
        ProductOperation::Search {
            index,
            query,
            limit,
        } => {
            context.checkpoint()?;
            let snapshot = product.snapshot_bounded(context.logical_time_micros)?;
            let result = snapshot
                .inner
                .search_bounded_with_checkpoint(
                    index,
                    &query,
                    limit,
                    search_limits(context.limits, limit),
                    || !context.cancellation.is_cancelled() && !context.deadline_elapsed(),
                )
                .map_err(|error| map_search_error(&error, context))?;
            context.checkpoint()?;
            let response = ProductResponse::Search(ProductSearchResults {
                hits: result
                    .hits
                    .into_iter()
                    .map(|hit| ProductSearchHit {
                        document_id: hit.document_id,
                        score: crate::CanonicalF64::new(hit.score),
                    })
                    .collect(),
                documents_examined: result.documents_examined,
                source_bytes: result.source_bytes,
                token_visits: result.token_visits,
                token_comparisons: result.token_comparisons,
                fuzzy_steps: result.fuzzy_steps,
            });
            admit_response(context, &response)?;
            response_limit_already_enforced = true;
            response
        }
        ProductOperation::AdminStatus => {
            ProductResponse::AdminStatus(product.administration().status(StatusRequest {
                logical_time_micros: context.logical_time_micros,
            })?)
        }
        ProductOperation::AdminCheckpoint => {
            context.checkpoint()?;
            let receipt = product.administration().checkpoint()?;
            ProductResponse::AdminCheckpoint(receipt)
        }
        ProductOperation::AdminExplainSql { statement } => {
            let bound = session
                .take_sql_binding(ProductSessionSqlBindingKey::Explain(context.request_id))?
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
            ProductResponse::Explain(
                product
                    .administration()
                    .explain_bound_sql(&bound, &statement)?,
            )
        }
        ProductOperation::Doctor(request) => {
            ProductResponse::Doctor(product.doctor_opened(request.logical_time_micros))
        }
        ProductOperation::Backup(request) => {
            context.checkpoint()?;
            let context_cancellation = context.cancellation.clone();
            let deadline = context.deadline_micros;
            let info = product
                .administration()
                .backup(&request, move |_phase: BackupPhase| {
                    if context_cancellation.is_cancelled()
                        || deadline.is_some_and(|value| unix_time_micros() >= value)
                    {
                        ProgressControl::Cancel
                    } else {
                        ProgressControl::Continue
                    }
                })
                .map_err(|error| map_backup_error(error, context))?;
            ProductResponse::Backup(info)
        }
        ProductOperation::Restore(request) => {
            context.checkpoint()?;
            let context_cancellation = context.cancellation.clone();
            let deadline = context.deadline_micros;
            let info = product
                .administration()
                .restore(&request, move |_phase| {
                    if context_cancellation.is_cancelled()
                        || deadline.is_some_and(|value| unix_time_micros() >= value)
                    {
                        ProgressControl::Cancel
                    } else {
                        ProgressControl::Continue
                    }
                })
                .map_err(|error| map_backup_error(error, context))?;
            ProductResponse::Restore(info)
        }
        ProductOperation::Telemetry => {
            ProductResponse::Telemetry(product.telemetry_snapshot(context.logical_time_micros)?)
        }
        ProductOperation::VerifyProof {
            proof,
            witness,
            trusted_anchor,
        } => {
            context.checkpoint()?;
            let started = Instant::now();
            let result = crate::proof::verify_native_proof_offline_with_checkpoint(
                &proof,
                &witness,
                crate::proof::ExternalTrustedAnchor::new(trusted_anchor),
                &crate::proof::NativeVerificationLimits::default(),
                || context.checkpoint().is_ok(),
            );
            product
                .telemetry
                .record_timing(TimingClass::ProofVerification, started.elapsed());
            context.checkpoint()?;
            ProductResponse::ProofVerification(result.map_err(|error| {
                if matches!(error, crate::proof::NativeProofError::Interrupted) {
                    context
                        .checkpoint()
                        .err()
                        .unwrap_or_else(|| map_proof_error(&error))
                } else {
                    map_proof_error(&error)
                }
            })?)
        }
        ProductOperation::SecurityStatus => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityStatus(
                product.read_security_status(&actor, context.logical_time_micros)?,
            )
        }
        ProductOperation::SecurityPrincipalList(request) => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityPrincipalPage(product.read_security_principals(
                &actor,
                &request,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityRoleList(request) => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityRolePage(product.read_security_roles(
                &actor,
                &request,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityAssignmentList(request) => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityAssignmentPage(product.read_security_assignments(
                &actor,
                &request,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityKeyList(request) => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityKeyPage(product.read_security_keys(
                &actor,
                &request,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityAuditRead(request) => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityAuditPage(product.read_security_audit(
                &actor,
                request.cursor,
                request.limit,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityPrincipalCreate { display_name } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityPrincipalMutated(
                product.create_security_principal_idempotent(
                    &actor,
                    &display_name,
                    required_idempotency_token(context)?,
                    context.logical_time_micros,
                )?,
            )
        }
        ProductOperation::SecurityPrincipalSetEnabled {
            principal_id,
            enabled,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityMutated(product.set_security_principal_enabled_idempotent(
                &actor,
                principal_id,
                enabled,
                required_idempotency_token(context)?,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityCustomRoleCreate {
            display_name,
            grants,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityCustomRoleMutated(
                product.create_custom_security_role_idempotent(
                    &actor,
                    &display_name,
                    grants,
                    required_idempotency_token(context)?,
                    context.logical_time_micros,
                )?,
            )
        }
        ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id,
            role,
            scope,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityAssignmentMutated(product.assign_built_in_role_idempotent(
                &actor,
                principal_id,
                role,
                scope,
                required_idempotency_token(context)?,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityCustomAssignmentCreate {
            principal_id,
            role_id,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityAssignmentMutated(
                product.assign_custom_security_role_idempotent(
                    &actor,
                    principal_id,
                    role_id,
                    required_idempotency_token(context)?,
                    context.logical_time_micros,
                )?,
            )
        }
        ProductOperation::SecurityAssignmentRevoke { assignment_id } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityMutated(product.revoke_security_assignment_idempotent(
                &actor,
                assignment_id,
                required_idempotency_token(context)?,
                context.logical_time_micros,
            )?)
        }
        ProductOperation::SecurityApiKeyIssueSelfStart {
            principal_id,
            label,
            roles,
            custom_roles,
            permission_ceiling,
            scope_ceiling,
            expires_at_micros,
        }
        | ProductOperation::SecurityApiKeyIssueStart {
            principal_id,
            label,
            roles,
            custom_roles,
            permission_ceiling,
            scope_ceiling,
            expires_at_micros,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityApiKeyStarted(product.start_api_key_issue_idempotent(
                &actor,
                principal_id,
                &label,
                roles,
                custom_roles,
                permission_ceiling,
                scope_ceiling,
                expires_at_micros,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id,
            confirmation_digest,
        }
        | ProductOperation::SecurityApiKeyIssueActivate {
            key_id,
            confirmation_digest,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityApiKeyActivated(product.activate_api_key_issue_idempotent(
                &actor,
                key_id,
                confirmation_digest,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyRotateSelfStart {
            predecessor_key_id,
            label,
            overlap_seconds,
            expires_at_micros,
        }
        | ProductOperation::SecurityApiKeyRotateStart {
            predecessor_key_id,
            label,
            overlap_seconds,
            expires_at_micros,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityApiKeyStarted(product.start_api_key_rotation_idempotent(
                &actor,
                predecessor_key_id,
                &label,
                overlap_seconds,
                expires_at_micros,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id,
            confirmation_digest,
        }
        | ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id,
            confirmation_digest,
        } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityApiKeyActivated(product.activate_api_key_rotation_idempotent(
                &actor,
                successor_key_id,
                confirmation_digest,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
        | ProductOperation::SecurityApiKeyIssueAbort { key_id } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityMutated(product.abort_api_key_issue_idempotent(
                &actor,
                key_id,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyRotateSelfAbort { successor_key_id }
        | ProductOperation::SecurityApiKeyRotateAbort { successor_key_id } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityMutated(product.abort_api_key_rotation_idempotent(
                &actor,
                successor_key_id,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityApiKeyRevokeSelf { key_id }
        | ProductOperation::SecurityApiKeyRevoke { key_id } => {
            let actor = managed_actor(session, context)?;
            ProductResponse::SecurityMutated(product.revoke_api_key_idempotent(
                &actor,
                key_id,
                required_idempotency_token(context)?,
                context.logical_time_micros,
                key_self_manage,
            )?)
        }
        ProductOperation::SecurityLegacyBearerRevoke => {
            let actor = managed_actor(session, context)?;
            let receipt = product.revoke_legacy_bearer_idempotent(
                &actor,
                required_idempotency_token(context)?,
                context.logical_time_micros,
            )?;
            ProductResponse::SecurityMutated(AccessControlMutationReceipt {
                authorization_epoch: receipt.authorization_epoch,
                commit: receipt.commit,
            })
        }
        ProductOperation::Prove { operation, limits } => {
            if operation.requires_managed_authority() || operation.is_key_lifecycle() {
                return Err(context.error(ProductErrorCode::InvalidRequest));
            }
            let started = Instant::now();
            context.checkpoint()?;
            let result = crate::proof::dispatch_proven_operation(
                product, session, context, &operation, limits,
            );
            product
                .telemetry
                .record_timing(TimingClass::ProofConstruction, started.elapsed());
            context.checkpoint()?;
            return result.map_err(|error| map_proof_error(&error));
        }
    };

    if read_only && !response_limit_already_enforced {
        admit_response(context, &response)?;
    }
    Ok(response)
}

fn admit_operation(
    product: &NativeProduct,
    session: &ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
) -> Result<(), ProductError> {
    validate_context(session, context)?;
    if session.is_legacy_owner()
        && (!product
            .legacy_bearer_migration_inspection()?
            .state
            .is_enabled()
            || operation.requires_managed_authority()
            || operation
                .required_permissions()?
                .allows(ProductPermission::SecurityManage)
            || operation
                .required_permissions()?
                .allows(ProductPermission::OwnershipManage))
    {
        return Err(context.error(ProductErrorCode::AuthorizationDenied));
    }
    let mut terminal_replay = false;
    let current_authority = match validate_durable_authority(product, session) {
        Ok(authority) => authority,
        Err(error)
            if error.code() == ProductErrorCode::AuthorizationDenied
                && exact_terminal_self_replay(product, session, context, operation)? =>
        {
            terminal_replay = true;
            let actor = session
                .authenticated_authority()?
                .ok_or_else(|| context.error(ProductErrorCode::AuthorizationDenied))?;
            Some(actor)
        }
        Err(error) => return Err(error),
    };
    if operation.requires_managed_authority() && current_authority.is_none() {
        return Err(context.error(ProductErrorCode::AuthorizationDenied));
    }
    let coarse_permissions = operation.required_permissions().map_err(|error| {
        if current_authority
            .as_deref()
            .is_some_and(|authority| mask_sql_classification_error(operation, authority))
        {
            ProductError::from_code(ProductErrorCode::AuthorizationDenied)
        } else {
            error
        }
    })?;
    let has_coarse_permissions = current_authority.as_ref().map_or_else(
        || session.authorization().allows_all(coarse_permissions),
        |authority| authority.authorization().allows_all(coarse_permissions),
    );
    if !has_coarse_permissions {
        return Err(context.error(ProductErrorCode::AuthorizationDenied));
    }
    if operation.requires_idempotency_token() && context.idempotency_token.is_none() {
        return Err(context.error(ProductErrorCode::InvalidRequest));
    }
    if operation.is_key_lifecycle() && context.durability != ProductDurabilityPolicy::STRICT {
        return Err(context.error(ProductErrorCode::InvalidRequest));
    }
    if let Some((count, bytes)) = operation.mutation_response_cost() {
        // A mutation is never published if its canonical wire response cannot be retained.
        context.limits.admit_response(count, bytes, bytes)?;
    }
    let requirement = operation_authorization_requirement(
        product,
        session,
        context,
        operation,
        current_authority.as_deref(),
    )?;
    if !terminal_replay {
        context.checkpoint()?;
        let authorized = match current_authority.as_ref() {
            Some(authority) => authority_satisfies_requirement(product, authority, &requirement)?,
            None => session
                .authorization()
                .allows_all(requirement.permissions()),
        };
        if !authorized {
            return Err(context.error(ProductErrorCode::AuthorizationDenied));
        }
    }
    if operation_uses_internal_structure_namespace(operation) {
        return Err(context.error(ProductErrorCode::InvalidRequest));
    }
    let (count, bytes, work, memory) = operation.request_cost();
    context.limits.admit_request(count, bytes, work, memory)?;
    operation.validate_limits(context.limits)?;
    Ok(())
}

fn exact_terminal_self_replay(
    product: &NativeProduct,
    session: &ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
) -> Result<bool, ProductError> {
    let Some(actor) = session.authenticated_authority()? else {
        return Ok(false);
    };
    let Some(token) = context.idempotency_token else {
        return Ok(false);
    };
    match operation {
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id,
            confirmation_digest,
        } => product.is_exact_terminal_self_replay(
            &actor,
            token,
            *successor_key_id,
            Some(*confirmation_digest),
        ),
        ProductOperation::SecurityApiKeyRevokeSelf { key_id } if *key_id == actor.key_id() => {
            product.is_exact_terminal_self_replay(&actor, token, *key_id, None)
        }
        _ => Ok(false),
    }
}

fn managed_actor(
    session: &ProductSession,
    context: &ProductRequestContext,
) -> Result<Arc<crate::AuthenticatedAuthority>, ProductError> {
    session
        .authenticated_authority()?
        .ok_or_else(|| context.error(ProductErrorCode::AuthorizationDenied))
}

fn required_idempotency_token(context: &ProductRequestContext) -> Result<u128, ProductError> {
    context
        .idempotency_token
        .ok_or_else(|| context.error(ProductErrorCode::InvalidRequest))
}

fn mask_sql_classification_error(
    operation: &ProductOperation,
    authority: &crate::AuthenticatedAuthority,
) -> bool {
    let (is_sql, additional) = match operation {
        ProductOperation::ExecuteSql { .. } => (true, ProductAuthorization::NONE),
        ProductOperation::Prove { operation, .. }
            if matches!(operation.as_ref(), ProductOperation::ExecuteSql { .. }) =>
        {
            (
                true,
                ProductAuthorization::from_permissions([ProductPermission::ProofGenerate]),
            )
        }
        _ => (false, ProductAuthorization::NONE),
    };
    is_sql
        && ![
            ProductAuthorization::from_permissions([
                ProductPermission::CatalogRead,
                ProductPermission::DataRead,
            ]),
            ProductAuthorization::from_permissions([
                ProductPermission::CatalogRead,
                ProductPermission::DataWrite,
            ]),
            ProductAuthorization::from_permissions([ProductPermission::CatalogWrite]),
        ]
        .into_iter()
        .any(|permissions| authority.allows_instance_authorization(permissions.union(additional)))
}

fn validate_durable_authority(
    product: &NativeProduct,
    session: &ProductSession,
) -> Result<Option<Arc<crate::AuthenticatedAuthority>>, ProductError> {
    let Some(bound) = session.authenticated_authority()? else {
        return Ok(None);
    };
    let current = product.revalidate_authenticated_authority(Arc::clone(&bound))?;
    if !Arc::ptr_eq(&bound, &current) {
        session.refresh_authenticated_authority(Arc::clone(&current))?;
    }
    Ok(Some(current))
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive operation-to-scope mapping is the fail-closed authorization boundary"
)]
fn operation_authorization_requirement(
    product: &NativeProduct,
    session: &ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
    authority: Option<&crate::AuthenticatedAuthority>,
) -> Result<ProductAuthorizationRequirement, ProductError> {
    let permissions = operation.required_permissions()?;
    let requirement = match operation {
        ProductOperation::CatalogObject { id } | ProductOperation::CatalogDescribe { id } => {
            ProductAuthorizationRequirement::object(permissions, *id)
        }
        ProductOperation::CatalogDependencies(request) => {
            if authority.is_some() {
                ProductAuthorizationRequirement::instance(permissions)
            } else {
                ProductAuthorizationRequirement::object(permissions, request.object)
            }
        }
        ProductOperation::CatalogCreate { object } => object.parent().map_or_else(
            || ProductAuthorizationRequirement::instance(permissions),
            |parent| ProductAuthorizationRequirement::object(permissions, parent),
        ),
        ProductOperation::CatalogObjectNamed { name }
        | ProductOperation::CatalogResolve { name } => {
            let snapshot = product.catalog_snapshot()?;
            product.catalog_resolve(&snapshot, name)?.map_or_else(
                || ProductAuthorizationRequirement::instance(permissions),
                |object| ProductAuthorizationRequirement::object(permissions, object.id()),
            )
        }
        ProductOperation::PrepareSql { statement } => {
            let prepared = product.prepare_sql(statement).map_err(|error| {
                if authority
                    .is_some_and(|current| !current.allows_instance_authorization(permissions))
                {
                    ProductError::from_code(ProductErrorCode::AuthorizationDenied)
                } else {
                    error
                }
            })?;
            requirement_for_objects(permissions, prepared.referenced_object_ids())
        }
        ProductOperation::ExecuteSql {
            statement,
            parameters,
        } => {
            let bound = product
                .database
                .bind_sql_latest(statement, parameters)
                .map_err(|error| mask_scoped_sql_bind_error(error, authority, permissions))?;
            let requirement = sql_binding_requirement(bound.class(), &bound);
            session.retain_sql_binding(
                ProductSessionSqlBindingKey::Execute(context.request_id),
                bound,
            )?;
            requirement
        }
        ProductOperation::AdminExplainSql { statement } => {
            let catalog_read = authorization([ProductPermission::CatalogRead]);
            let explain_permissions =
                authorization([ProductPermission::CatalogRead, ProductPermission::Observe]);
            let bound = product
                .database
                .bind_sql_latest(statement, &[])
                .map_err(|error| {
                    mask_scoped_sql_bind_error(error, authority, explain_permissions)
                })?;
            if bound.class() != SqlStatementClass::Read {
                return Err(ProductError::from_code(ProductErrorCode::SqlInvalidSyntax));
            }
            let mut requirement = ProductAuthorizationRequirement::instance(authorization([
                ProductPermission::Observe,
            ]));
            requirement.union(&requirement_for_objects(
                catalog_read,
                bound.referenced_object_ids().iter().copied(),
            ));
            session.retain_sql_binding(
                ProductSessionSqlBindingKey::Explain(context.request_id),
                bound,
            )?;
            requirement
        }
        ProductOperation::DeallocatePrepared { handle }
        | ProductOperation::ExecutePrepared { handle, .. } => {
            session.prepared(*handle).map_or_else(
                || ProductAuthorizationRequirement::instance(permissions),
                |prepared| requirement_for_objects(permissions, prepared.referenced_object_ids()),
            )
        }
        ProductOperation::StructureGet { .. }
        | ProductOperation::StructureSet { .. }
        | ProductOperation::StructureTtl { .. } => ProductAuthorizationRequirement::object(
            permissions,
            product.default_scalar_keyspace_id()?,
        ),
        ProductOperation::StructureMutate { mutations } => requirement_for_objects(
            permissions,
            mutations
                .iter()
                .map(|mutation| mutation.structure_key().keyspace),
        ),
        ProductOperation::StructureRead(request) => request.keyspace().map_or_else(
            || ProductAuthorizationRequirement::instance(permissions),
            |keyspace| ProductAuthorizationRequirement::object(permissions, keyspace),
        ),
        ProductOperation::Search { index, .. } => {
            ProductAuthorizationRequirement::object(permissions, *index)
        }
        ProductOperation::SearchCollection { collection, .. }
        | ProductOperation::SearchIngest { collection, .. }
        | ProductOperation::SearchDocumentUpdate { collection, .. }
        | ProductOperation::SearchDocumentDelete { collection, .. } => {
            ProductAuthorizationRequirement::object(permissions, *collection)
        }
        ProductOperation::TransactionStageStructure { mutation, .. } => {
            ProductAuthorizationRequirement::object(permissions, mutation.structure_key().keyspace)
        }
        ProductOperation::TransactionStageSearch { mutation, .. } => {
            ProductAuthorizationRequirement::object(permissions, mutation.index())
        }
        ProductOperation::TransactionStageVector { mutation, .. } => {
            ProductAuthorizationRequirement::object(permissions, mutation.index())
        }
        ProductOperation::TransactionStageSql { handle, mutation } => {
            let transaction = session
                .active_transaction(*handle)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
            let bound = transaction
                .batch
                .bind_sql(&mutation.statement, &mutation.parameters)
                .map_err(|error| mask_scoped_sql_bind_error(error, authority, permissions))?;
            if bound.class() != SqlStatementClass::DataMutation {
                return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
            }
            let requirement = sql_binding_requirement(bound.class(), &bound);
            session.retain_sql_binding(
                ProductSessionSqlBindingKey::Stage {
                    request_id: context.request_id,
                    handle: *handle,
                },
                bound,
            )?;
            requirement
        }
        ProductOperation::TransactionCommit { handle } => {
            session.active_transaction(*handle).map_or_else(
                || ProductAuthorizationRequirement::unscoped(permissions),
                |transaction| transaction.authorization.clone(),
            )
        }
        ProductOperation::Prove { operation, .. } => {
            let mut inner = operation_authorization_requirement(
                product, session, context, operation, authority,
            )?;
            inner.add_permission_to_bound_targets(ProductPermission::ProofGenerate);
            inner
        }
        ProductOperation::TransactionBegin
        | ProductOperation::TransactionRollback { .. }
        | ProductOperation::ExplicitTransactionStatus { .. }
        | ProductOperation::TransactionStatus { .. }
        | ProductOperation::TransactionStatusByIdempotency { .. }
        | ProductOperation::CatalogVisibleList(_)
        | ProductOperation::SecurityApiKeyIssueSelfStart { .. } => {
            ProductAuthorizationRequirement::unscoped(permissions)
        }
        // Historical CatalogList remains instance-only. Other unbound surfaces
        // also fail closed at the instance boundary.
        _ => ProductAuthorizationRequirement::instance(permissions),
    };
    Ok(requirement)
}

fn mask_scoped_sql_bind_error(
    error: hyphae_native_runtime::SqlError,
    authority: Option<&crate::AuthenticatedAuthority>,
    permissions: ProductAuthorization,
) -> ProductError {
    if authority.is_some_and(|current| !current.allows_instance_authorization(permissions)) {
        ProductError::from_code(ProductErrorCode::AuthorizationDenied)
    } else {
        ProductError::from(error)
    }
}

fn sql_binding_requirement(
    class: SqlStatementClass,
    bound: &hyphae_native_runtime::BoundSqlStatement,
) -> ProductAuthorizationRequirement {
    let permissions = match class {
        SqlStatementClass::Read => {
            authorization([ProductPermission::CatalogRead, ProductPermission::DataRead])
        }
        SqlStatementClass::DataMutation => {
            authorization([ProductPermission::CatalogRead, ProductPermission::DataWrite])
        }
        SqlStatementClass::CatalogMutation => authorization([ProductPermission::CatalogWrite]),
    };
    if class == SqlStatementClass::CatalogMutation && bound.requires_instance_catalog_write() {
        ProductAuthorizationRequirement::instance(permissions)
    } else {
        requirement_for_objects(permissions, bound.referenced_object_ids().iter().copied())
    }
}

fn requirement_for_objects(
    permissions: ProductAuthorization,
    objects: impl IntoIterator<Item = ObjectId>,
) -> ProductAuthorizationRequirement {
    let mut objects = objects.into_iter();
    let Some(first) = objects.next() else {
        return ProductAuthorizationRequirement::instance(permissions);
    };
    let mut requirement = ProductAuthorizationRequirement::object(permissions, first);
    for object in objects {
        requirement.union(&ProductAuthorizationRequirement::object(
            permissions,
            object,
        ));
    }
    requirement
}

fn unmanaged_catalog_cursor_key(base: [u8; 32], principal: &ProductPrincipal) -> [u8; 32] {
    *blake3::keyed_hash(&base, principal.identity().as_bytes()).as_bytes()
}

fn authority_satisfies_requirement(
    product: &NativeProduct,
    authority: &crate::AuthenticatedAuthority,
    requirement: &ProductAuthorizationRequirement,
) -> Result<bool, ProductError> {
    let snapshot = product.catalog_snapshot()?;
    authority_satisfies_requirement_at_snapshot(product, authority, requirement, &snapshot)
}

fn authority_satisfies_requirement_at_snapshot(
    product: &NativeProduct,
    authority: &crate::AuthenticatedAuthority,
    requirement: &ProductAuthorizationRequirement,
    snapshot: &crate::ProductCatalogSnapshot,
) -> Result<bool, ProductError> {
    if !authority.authorization().allows_all(requirement.unscoped) {
        return Ok(false);
    }
    if !authority.allows_instance_authorization(requirement.instance) {
        return Ok(false);
    }
    if requirement.objects.is_empty() {
        return Ok(true);
    }
    let objects = requirement.objects.keys().copied().collect::<BTreeSet<_>>();
    let ancestry = catalog_ancestry_at(product, snapshot, &objects)?;
    Ok(requirement.objects.iter().all(|(object, permissions)| {
        authority.allows_object_authorization(*permissions, *object, |candidate, ancestor| {
            ancestry
                .get(&candidate)
                .is_some_and(|ancestors| ancestors.contains(&ancestor))
        })
    }))
}

fn catalog_ancestry_at(
    product: &NativeProduct,
    snapshot: &crate::ProductCatalogSnapshot,
    objects: &BTreeSet<ObjectId>,
) -> Result<BTreeMap<ObjectId, BTreeSet<ObjectId>>, ProductError> {
    let mut ancestry = BTreeMap::new();
    for target in objects {
        let mut ancestors = BTreeSet::new();
        let mut current = *target;
        while let Some(object) = product.catalog_describe(snapshot, current)? {
            let Some(parent) = object.parent() else {
                break;
            };
            if !ancestors.insert(parent) {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            current = parent;
        }
        ancestry.insert(*target, ancestors);
    }
    Ok(ancestry)
}

fn exact_catalog_visible_scopes(
    product: &NativeProduct,
    authority: &crate::AuthenticatedAuthority,
    snapshot: &crate::ProductCatalogSnapshot,
) -> Result<Vec<crate::ProductScope>, ProductError> {
    let grants = authority
        .scoped_authorization()
        .iter()
        .filter(|scoped| scoped.authorization.allows(ProductPermission::CatalogRead))
        .map(|scoped| scoped.scope)
        .collect::<Vec<_>>();
    let object_ids = grants
        .iter()
        .chain(authority.scope_ceiling())
        .filter_map(|scope| match scope {
            crate::ProductScope::Instance => None,
            crate::ProductScope::CatalogSubtree(id) | crate::ProductScope::CatalogObject(id) => {
                Some(*id)
            }
        })
        .collect::<BTreeSet<_>>();
    let ancestry = catalog_ancestry_at(product, snapshot, &object_ids)?;
    let is_descendant = |candidate: ObjectId, ancestor: ObjectId| {
        ancestry
            .get(&candidate)
            .is_some_and(|ancestors| ancestors.contains(&ancestor))
    };
    let mut intersections = BTreeSet::new();
    for grant in grants {
        for ceiling in authority.scope_ceiling() {
            if let Some(scope) = intersect_catalog_scopes(grant, *ceiling, is_descendant) {
                intersections.insert(scope);
            }
        }
    }
    Ok(intersections.into_iter().collect())
}

fn intersect_catalog_scopes(
    left: crate::ProductScope,
    right: crate::ProductScope,
    is_descendant: impl Fn(ObjectId, ObjectId) -> bool,
) -> Option<crate::ProductScope> {
    use crate::ProductScope::{CatalogObject, CatalogSubtree, Instance};

    match (left, right) {
        (Instance, scope) | (scope, Instance) => Some(scope),
        (CatalogObject(left), CatalogObject(right)) => {
            (left == right).then_some(CatalogObject(left))
        }
        (CatalogObject(object), CatalogSubtree(root))
        | (CatalogSubtree(root), CatalogObject(object)) => {
            (object == root || is_descendant(object, root)).then_some(CatalogObject(object))
        }
        (CatalogSubtree(left), CatalogSubtree(right)) if left == right => {
            Some(CatalogSubtree(left))
        }
        (CatalogSubtree(left), CatalogSubtree(right)) if is_descendant(left, right) => {
            Some(CatalogSubtree(left))
        }
        (CatalogSubtree(left), CatalogSubtree(right)) if is_descendant(right, left) => {
            Some(CatalogSubtree(right))
        }
        (CatalogSubtree(_), CatalogSubtree(_)) => None,
    }
}

fn operation_uses_internal_structure_namespace(operation: &ProductOperation) -> bool {
    match operation {
        ProductOperation::StructureGet { key }
        | ProductOperation::StructureSet { key, .. }
        | ProductOperation::StructureTtl { key } => crate::is_internal_structure_key(key),
        ProductOperation::StructureMutate { mutations } => mutations
            .iter()
            .any(|mutation| crate::is_internal_structure_key(&mutation.structure_key().key)),
        ProductOperation::StructureRead(request) => request.uses_internal_structure_namespace(),
        ProductOperation::TransactionStageStructure { mutation, .. } => {
            crate::is_internal_structure_key(&mutation.structure_key().key)
        }
        ProductOperation::Prove { operation, .. } => {
            operation_uses_internal_structure_namespace(operation)
        }
        _ => false,
    }
}

fn validate_context(
    session: &ProductSession,
    context: &ProductRequestContext,
) -> Result<(), ProductError> {
    if context.request_id == 0
        || context.idempotency_token == Some(0)
        || context.session_id != session.id()
        || &context.principal != session.principal()
        || context.authorization != session.authorization()
    {
        return Err(context.error(ProductErrorCode::InvalidRequest));
    }
    if context.authorization_epoch != session.authorization_epoch() {
        return Err(context.error(ProductErrorCode::AuthorizationDenied));
    }
    context.limits.validate()
}

fn execute_sql(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    statement: &str,
    parameters: &[ProductValue],
    bound: &hyphae_native_runtime::BoundSqlStatement,
) -> Result<ProductResponse, ProductError> {
    if statement.len() > crate::MAX_PRODUCT_SQL_STATEMENT_BYTES {
        return Err(ProductError::sql_statement_limit(
            crate::MAX_PRODUCT_SQL_STATEMENT_BYTES,
            statement.len(),
        ));
    }
    if parameters.len() > crate::MAX_PRODUCT_SQL_PARAMETERS {
        return Err(ProductError::sql_parameter_limit(
            crate::MAX_PRODUCT_SQL_PARAMETERS,
            parameters.len(),
        ));
    }
    if bound.class() == SqlStatementClass::Read {
        if let Some(prepared) = bound.prepared_statement() {
            if prepared
                .maximum_result_rows()
                .is_none_or(|rows| rows > context.limits.max_count)
            {
                return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
            }
            let result = product
                .execute_bound_sql_with_checkpoint(bound, parameters, || {
                    context.checkpoint().is_ok()
                })
                .map_err(|error| map_execution_interruption(error, context))?;
            let response = ProductResponse::Sql {
                result: result.value,
                snapshot: Some(result.snapshot),
                commit: None,
            };
            admit_response(context, &response)?;
            return Ok(response);
        }

        let identity = product
            .snapshot_bounded(context.logical_time_micros)?
            .identity();
        let mut transaction = product.database.begin_sql(
            context.logical_time_micros,
            context.durability.durability.into(),
        )?;
        if identity.catalog_version != bound.catalog_version() {
            return Err(hyphae_native_runtime::SqlError::CatalogChanged.into());
        }
        let result = transaction
            .execute_bound_sql_with_checkpoint(bound, parameters, || context.checkpoint().is_ok())
            .map_err(ProductError::from)
            .map_err(|error| map_execution_interruption(error, context))?;
        transaction.rollback();
        let response = ProductResponse::Sql {
            result: result.into(),
            snapshot: Some(identity),
            commit: None,
        };
        admit_response(context, &response)?;
        return Ok(response);
    }

    let mut transaction = product.database.begin_sql(
        context.logical_time_micros,
        context.durability.durability.into(),
    )?;
    let result = transaction
        .execute_bound_sql_with_checkpoint(bound, parameters, || sql_mutation_checkpoint(context))
        .map_err(ProductError::from)
        .map_err(|error| map_execution_interruption(error, context))?;
    let no_mutation = sql_command_can_be_noop(bound.class(), statement)
        && matches!(
            result,
            hyphae_native_runtime::SqlResult::Command {
                rows_affected: 0,
                object_id: None,
            }
        );
    let product_result = ProductSqlResult::from(result);
    context.checkpoint()?;
    if no_mutation {
        transaction.rollback();
        return Ok(ProductResponse::Sql {
            result: product_result,
            snapshot: None,
            commit: None,
        });
    }
    let telemetry = product.telemetry.clone();
    let receipt = commit(&telemetry, transaction, session, context)?;
    Ok(ProductResponse::Sql {
        result: product_result,
        snapshot: None,
        commit: Some(ProductCommitOutcome::Committed(receipt)),
    })
}

fn admit_response(
    context: &ProductRequestContext,
    response: &ProductResponse,
) -> Result<(), ProductError> {
    let (count, bytes) = response.cost();
    context.limits.admit_response(count, bytes, bytes)
}

fn commit(
    telemetry: &crate::TelemetryRegistry,
    mut transaction: NativeTransaction<'_>,
    session: &mut ProductSession,
    context: &ProductRequestContext,
) -> Result<ProductCommitReceipt, ProductError> {
    let runtime_transaction_id = transaction.transaction_id();
    let principal_hash = principal_hash(context.principal.identity());
    let idempotency_token = idempotency_token(context, runtime_transaction_id);
    let resolution = transaction
        .begin_resolution(principal_hash, idempotency_token)
        .map_err(|error| {
            if matches!(
                error,
                hyphae_native_runtime::NativeRuntimeError::InvalidPreparedMutation
            ) && context.idempotency_token.is_some()
            {
                ProductError::from_code(ProductErrorCode::IdempotencyConflict)
            } else {
                ProductError::from(error)
            }
        })?;
    let resolution_id = ProductTransactionId::from(resolution.resolution_id);
    match transaction.commit() {
        Ok(receipt) => {
            #[cfg(test)]
            if context.cancel_sql_mutation_at_commit {
                context.cancellation.cancel();
            }
            telemetry.record_timing(TimingClass::WalAppend, receipt.wal_append_time);
            telemetry.record_timing(
                TimingClass::PageSynchronization,
                receipt.page_synchronization_time,
            );
            telemetry.record_timing(
                TimingClass::WalSynchronization,
                receipt.wal_synchronization_time,
            );
            telemetry.record_timing(TimingClass::Durability, receipt.execution_time);
            let receipt = ProductCommitReceipt::from_runtime(receipt, resolution_id);
            session.record_transaction(resolution_id, ProductTransactionStatus::Committed(receipt));
            Ok(receipt)
        }
        Err(error) if commit_publication_may_be_unknown(&error) => {
            session.record_transaction(
                resolution_id,
                ProductTransactionStatus::OutcomeUnknown {
                    transaction_id: resolution_id,
                },
            );
            Err(
                ProductFailureBoundary::publication_unknown(resolution_id.native())
                    .apply(ProductError::from(error)),
            )
        }
        Err(error) => {
            let error = ProductError::from(error);
            if commit_failure_proves_rollback(&error) {
                session.record_transaction(
                    resolution_id,
                    ProductTransactionStatus::RolledBack {
                        transaction_id: resolution_id,
                    },
                );
                Err(ProductFailureBoundary::rolled_back(resolution_id.native()).apply(error))
            } else {
                Err(error)
            }
        }
    }
}

fn principal_hash(principal: &str) -> [u8; 32] {
    *blake3::hash(principal.as_bytes()).as_bytes()
}

fn resolve_transaction_status(
    product: &NativeProduct,
    session: &ProductSession,
    context: &ProductRequestContext,
    transaction_id: ProductTransactionId,
) -> ProductTransactionStatus {
    if let status @ (ProductTransactionStatus::Committed(_)
    | ProductTransactionStatus::RolledBack { .. }) = session.transaction_status(transaction_id)
    {
        return status;
    }
    let Some(resolution) = product
        .database
        .transaction_resolution(transaction_id.native())
        .filter(|resolution| {
            resolution.principal_hash == principal_hash(context.principal.identity())
        })
    else {
        return ProductTransactionStatus::Unknown;
    };
    resolve_durable_transaction(product, transaction_id, resolution)
}

fn resolve_transaction_status_by_token(
    product: &NativeProduct,
    context: &ProductRequestContext,
    idempotency_token: u128,
) -> ProductTransactionStatus {
    if idempotency_token == 0 {
        return ProductTransactionStatus::Unknown;
    }
    let principal_hash = principal_hash(context.principal.identity());
    let token_hash = supplied_idempotency_token(context.principal.identity(), idempotency_token);
    let Some(resolution) = product
        .database
        .transaction_resolution_for_token(principal_hash, token_hash)
    else {
        return ProductTransactionStatus::Unknown;
    };
    resolve_durable_transaction(
        product,
        ProductTransactionId::from(resolution.resolution_id),
        resolution,
    )
}

fn resolve_durable_transaction(
    product: &NativeProduct,
    transaction_id: ProductTransactionId,
    resolution: hyphae_native_runtime::DurableTransactionResolution,
) -> ProductTransactionStatus {
    match resolution.outcome {
        hyphae_native_runtime::DurableTransactionOutcome::Committed {
            runtime_transaction_id,
            ..
        } => product
            .database
            .transaction_commit_receipt(runtime_transaction_id)
            .map_or(
                ProductTransactionStatus::OutcomeUnknown { transaction_id },
                |receipt| {
                    ProductTransactionStatus::Committed(ProductCommitReceipt::from_runtime(
                        receipt,
                        transaction_id,
                    ))
                },
            ),
        hyphae_native_runtime::DurableTransactionOutcome::RolledBack { .. } => {
            ProductTransactionStatus::RolledBack { transaction_id }
        }
        hyphae_native_runtime::DurableTransactionOutcome::OutcomeUnknown { .. } => {
            ProductTransactionStatus::OutcomeUnknown { transaction_id }
        }
    }
}

fn idempotency_token(context: &ProductRequestContext, transaction_id: TransactionId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-product-idempotency-v1");
    hasher.update(context.principal.identity().as_bytes());
    if let Some(token) = context.idempotency_token {
        return supplied_idempotency_token(context.principal.identity(), token);
    }
    hasher.update(&[0]);
    hasher.update(&context.session_id.get().to_le_bytes());
    hasher.update(&transaction_id.get().to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn supplied_idempotency_token(principal: &str, token: u128) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-product-idempotency-v1");
    hasher.update(principal.as_bytes());
    hasher.update(&[1]);
    hasher.update(&token.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn explicit_idempotency_token(
    context: &ProductRequestContext,
    handle: ProductTransactionHandle,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-product-explicit-idempotency-v1");
    hasher.update(context.principal.identity().as_bytes());
    if let Some(token) = context.idempotency_token {
        return supplied_idempotency_token(context.principal.identity(), token);
    }
    hasher.update(&[0]);
    hasher.update(&context.session_id.get().to_le_bytes());
    hasher.update(&handle.get().to_le_bytes());
    *hasher.finalize().as_bytes()
}

pub(crate) fn apply_structure_mutation(
    transaction: &mut NativeWriteBatch,
    mutation: ProductStructureMutation,
) -> Result<ProductStructureMutationResult, ProductError> {
    validate_structure_key_for_batch(transaction, mutation.structure_key(), mutation.family())?;
    match mutation {
        ProductStructureMutation::StringSet {
            key,
            value,
            expires_at_micros,
        } => {
            transaction.set(key.key, value, expires_at_micros)?;
            Ok(ProductStructureMutationResult::Unit)
        }
        ProductStructureMutation::StringDelete { key } => Ok(
            ProductStructureMutationResult::Boolean(transaction.delete_structure(key.key)?),
        ),
        ProductStructureMutation::CounterAdd { key, delta } => Ok(
            ProductStructureMutationResult::Integer(transaction.increment_i64(key.key, delta)?),
        ),
        ProductStructureMutation::Create { key, family } => {
            create_structure(transaction, key, family)
        }
        ProductStructureMutation::Delete { key, family } => {
            delete_structure(transaction, key, family)
        }
        ProductStructureMutation::Expire {
            key,
            family,
            expires_at_micros,
        } => {
            let updated = match family {
                StructureKind::String | StructureKind::Counter => {
                    transaction.expire_structure(key.key, expires_at_micros)?
                }
                StructureKind::Hash => transaction.expire_hash(key.key, expires_at_micros)?,
                StructureKind::List => transaction.expire_list(key.key, expires_at_micros)?,
                StructureKind::Set => transaction.expire_set(key.key, expires_at_micros)?,
                StructureKind::SortedSet => {
                    transaction.expire_sorted_set(key.key, expires_at_micros)?
                }
                StructureKind::Stream => transaction.expire_stream(key.key, expires_at_micros)?,
            };
            Ok(ProductStructureMutationResult::Boolean(updated))
        }
        ProductStructureMutation::HashSet { key, field, value } => {
            transaction.hset(key.key, field, value)?;
            Ok(ProductStructureMutationResult::Unit)
        }
        ProductStructureMutation::HashDelete { key, field } => Ok(
            ProductStructureMutationResult::Boolean(transaction.hdelete(key.key, field)?),
        ),
        ProductStructureMutation::HashCounterAdd { key, field, delta } => {
            Ok(ProductStructureMutationResult::Integer(
                transaction.hincrement_i64(key.key, field, delta)?,
            ))
        }
        ProductStructureMutation::HashExpireField {
            key,
            field,
            expires_at_micros,
        } => Ok(ProductStructureMutationResult::Boolean(
            transaction.expire_hash_field(key.key, field, expires_at_micros)?,
        )),
        ProductStructureMutation::ListPush { key, side, value } => {
            let count = match side {
                ProductListSide::Left => transaction.lpush(key.key, value)?,
                ProductListSide::Right => transaction.rpush(key.key, value)?,
            };
            Ok(ProductStructureMutationResult::Count(count))
        }
        ProductStructureMutation::ListPop { key, side } => {
            let value = match side {
                ProductListSide::Left => transaction.lpop(key.key)?,
                ProductListSide::Right => transaction.rpop(key.key)?,
            };
            Ok(ProductStructureMutationResult::Value(value))
        }
        ProductStructureMutation::SetAdd { key, member } => Ok(
            ProductStructureMutationResult::Boolean(transaction.sadd(key.key, member)?),
        ),
        ProductStructureMutation::SetRemove { key, member } => Ok(
            ProductStructureMutationResult::Boolean(transaction.srem(key.key, member)?),
        ),
        ProductStructureMutation::SortedSetAdd { key, member, score } => {
            transaction.zadd(key.key, score.get(), member)?;
            Ok(ProductStructureMutationResult::Unit)
        }
        mutation @ (ProductStructureMutation::SortedSetIncrement { .. }
        | ProductStructureMutation::SortedSetPop { .. }) => {
            apply_sorted_set_value_mutation(transaction, mutation)
        }
        ProductStructureMutation::SortedSetRemove { key, member } => Ok(
            ProductStructureMutationResult::Boolean(transaction.zrem(key.key, member)?),
        ),
        ProductStructureMutation::StreamAdd { key, fields } => {
            apply_stream_add(transaction, key, fields)
        }
    }
}

/// Applies one bounded stream append.
fn apply_stream_add(
    transaction: &mut NativeWriteBatch,
    key: ProductStructureKey,
    fields: Vec<ProductHashEntry>,
) -> Result<ProductStructureMutationResult, ProductError> {
    if fields.is_empty() || fields.len() > crate::MAX_PRODUCT_STREAM_FIELDS {
        return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
    }
    let fields = fields
        .into_iter()
        .map(|entry| (entry.field, entry.value))
        .collect::<Vec<_>>();
    Ok(ProductStructureMutationResult::StreamId(
        transaction.xadd(key.key, &fields)?,
    ))
}

fn create_structure(
    transaction: &mut NativeWriteBatch,
    key: ProductStructureKey,
    family: StructureKind,
) -> Result<ProductStructureMutationResult, ProductError> {
    match family {
        StructureKind::Hash => transaction.create_hash(key.key)?,
        StructureKind::List => transaction.create_list(key.key)?,
        StructureKind::Set => transaction.create_set(key.key)?,
        StructureKind::SortedSet => transaction.create_sorted_set(key.key)?,
        StructureKind::Stream => transaction.create_stream(key.key)?,
        StructureKind::String | StructureKind::Counter => {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
    }
    Ok(ProductStructureMutationResult::Unit)
}

fn delete_structure(
    transaction: &mut NativeWriteBatch,
    key: ProductStructureKey,
    family: StructureKind,
) -> Result<ProductStructureMutationResult, ProductError> {
    let deleted = match family {
        StructureKind::String | StructureKind::Counter => transaction.delete_structure(key.key)?,
        StructureKind::Hash => transaction.delete_hash(key.key)?,
        StructureKind::List => transaction.delete_list(key.key)?,
        StructureKind::Set => transaction.delete_set(key.key)?,
        StructureKind::SortedSet => transaction.delete_sorted_set(key.key)?,
        StructureKind::Stream => transaction.delete_stream(key.key)?,
    };
    Ok(ProductStructureMutationResult::Boolean(deleted))
}

// Exhaustive read dispatch: one arm per request shape, cohesive by design.
#[allow(clippy::too_many_lines)]
pub(crate) fn read_structure(
    snapshot: &crate::ProductSnapshot,
    request: ProductStructureReadRequest,
) -> Result<ProductStructureReadResult, ProductError> {
    validate_structure_key_for_snapshot(snapshot, request.structure_key(), request.family())?;
    match request {
        ProductStructureReadRequest::StringGet { key } => Ok(ProductStructureReadResult::Value(
            snapshot.structure_get(&key.key).map(<[u8]>::to_vec),
        )),
        ProductStructureReadRequest::CounterGet { key } => snapshot
            .structure_get(&key.key)
            .map(parse_product_counter)
            .transpose()
            .map(ProductStructureReadResult::Counter),
        ProductStructureReadRequest::Ttl { key, family } => Ok(ProductStructureReadResult::Ttl(
            structure_ttl(snapshot, &key.key, family),
        )),
        ProductStructureReadRequest::HashGet { key, field } => snapshot
            .inner
            .hget(&key.key, &field)
            .map(|value| ProductStructureReadResult::Value(value.map(<[u8]>::to_vec)))
            .map_err(Into::into),
        ProductStructureReadRequest::HashFieldTtl { key, field } => Ok(
            ProductStructureReadResult::Ttl(snapshot.inner.ttl_hash_field(&key.key, &field).into()),
        ),
        ProductStructureReadRequest::HashScan {
            key,
            start_after,
            limit,
        } => read_hash_scan(snapshot, &key.key, start_after.as_deref(), limit),
        ProductStructureReadRequest::HashLength { key } => snapshot
            .inner
            .hlen(&key.key)
            .map(ProductStructureReadResult::Count)
            .map_err(Into::into),
        ProductStructureReadRequest::SetMembers {
            key,
            start_after,
            limit,
        } => snapshot
            .inner
            .sscan(&key.key, start_after.as_deref(), limit)
            .map(ProductStructureReadResult::Values)
            .map_err(Into::into),
        ProductStructureReadRequest::ListRange { key, start, stop } => snapshot
            .inner
            .lrange(&key.key, start, stop)
            .map(ProductStructureReadResult::Values)
            .map_err(Into::into),
        ProductStructureReadRequest::ListLength { key } => snapshot
            .inner
            .llen(&key.key)
            .map(ProductStructureReadResult::Count)
            .map_err(Into::into),
        ProductStructureReadRequest::SetContains { key, member } => snapshot
            .inner
            .sismember(&key.key, &member)
            .map(ProductStructureReadResult::Boolean)
            .map_err(Into::into),
        ProductStructureReadRequest::SetCardinality { key } => snapshot
            .inner
            .scard(&key.key)
            .map(ProductStructureReadResult::Count)
            .map_err(Into::into),
        ProductStructureReadRequest::SetAlgebra {
            operation,
            keys,
            output_member_limit,
            visit_limit,
            ..
        } => read_set_algebra(snapshot, operation, keys, output_member_limit, visit_limit),
        ProductStructureReadRequest::SortedSetScore { key, member } => snapshot
            .inner
            .zscore(&key.key, &member)
            .map(|score| {
                ProductStructureReadResult::SortedSetScore(score.map(crate::CanonicalF64::new))
            })
            .map_err(Into::into),
        ProductStructureReadRequest::SortedSetRank { key, member, order } => {
            read_sorted_set_rank(snapshot, &key.key, &member, order)
        }
        ProductStructureReadRequest::SortedSetRange {
            key,
            start,
            stop,
            order,
        } => read_sorted_set_range(snapshot, &key.key, start, stop, order),
        ProductStructureReadRequest::SortedSetCardinality { key } => snapshot
            .inner
            .zcard(&key.key)
            .map(ProductStructureReadResult::Count)
            .map_err(Into::into),
        ProductStructureReadRequest::StreamRange {
            key,
            start,
            end,
            limit,
        } => read_stream_range(snapshot, &key.key, start, end, limit),
        ProductStructureReadRequest::SortedSetScoreRange {
            key,
            lower,
            upper,
            offset,
            limit,
            order,
        } => read_sorted_set_score_range(snapshot, &key.key, lower, upper, offset, limit, order),
        ProductStructureReadRequest::HashScanReverse {
            key,
            start_before,
            limit,
        } => snapshot
            .inner
            .hscan_reverse(&key.key, start_before.as_deref(), limit)
            .map(|entries| {
                ProductStructureReadResult::HashEntries(
                    entries
                        .into_iter()
                        .map(|entry| ProductHashEntry {
                            field: entry.field().to_vec(),
                            value: entry.value().to_vec(),
                        })
                        .collect(),
                )
            })
            .map_err(Into::into),
        ProductStructureReadRequest::HashScanMatch {
            key,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => read_hash_scan_match(
            snapshot,
            &key.key,
            &pattern,
            start_after.as_deref(),
            output_limit,
            visit_limit,
            match_step_limit,
        ),
        ProductStructureReadRequest::KeyScanMatch {
            keyspace: _,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => read_key_scan_match(
            snapshot,
            &pattern,
            start_after.as_deref(),
            output_limit,
            visit_limit,
            match_step_limit,
        ),
    }
}

/// Executes one bounded cross-family key glob scan, hiding the reserved
/// internal namespace from visits, outputs, and continuations.
fn read_key_scan_match(
    snapshot: &crate::ProductSnapshot,
    pattern: &[u8],
    start_after: Option<&[u8]>,
    output_limit: usize,
    visit_limit: usize,
    match_step_limit: usize,
) -> Result<ProductStructureReadResult, ProductError> {
    if pattern.starts_with(crate::INTERNAL_STRUCTURE_KEY_PREFIX)
        || start_after
            .is_some_and(|cursor| cursor.starts_with(crate::INTERNAL_STRUCTURE_KEY_PREFIX))
    {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    let request = hyphae_native_runtime::HashPatternScanRequest::try_new(
        pattern,
        start_after.map(<[u8]>::to_vec),
        output_limit,
        visit_limit,
        match_step_limit,
    )
    .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    let page = snapshot.inner.key_scan_match(&request)?;
    let stop = match page.stop {
        hyphae_native_runtime::HashPatternScanStop::Exhausted => ProductHashScanStop::Exhausted,
        hyphae_native_runtime::HashPatternScanStop::OutputLimit => ProductHashScanStop::OutputLimit,
        hyphae_native_runtime::HashPatternScanStop::VisitLimit => ProductHashScanStop::VisitLimit,
    };
    Ok(ProductStructureReadResult::KeyPage {
        entries: page
            .entries
            .into_iter()
            .filter(|(key, _)| !crate::is_internal_structure_key(key))
            .map(|(key, family)| ProductKeyEntry { key, family })
            .collect(),
        continuation: page.continuation,
        stop,
        visited: page.visited,
        match_steps: page.match_steps,
    })
}

/// Applies the minor-6 sorted-set mutations that return typed values.
fn apply_sorted_set_value_mutation(
    transaction: &mut NativeWriteBatch,
    mutation: ProductStructureMutation,
) -> Result<ProductStructureMutationResult, ProductError> {
    match mutation {
        ProductStructureMutation::SortedSetIncrement { key, member, delta } => {
            let score = transaction.zincrby(key.key, delta.get(), member)?;
            Ok(ProductStructureMutationResult::Score(CanonicalF64::new(
                score,
            )))
        }
        ProductStructureMutation::SortedSetPop { key, highest } => {
            let popped = transaction.zpop(key.key, highest)?;
            Ok(ProductStructureMutationResult::PoppedEntry(popped.map(
                |(member, score)| ProductSortedSetEntry {
                    member,
                    score: CanonicalF64::new(score),
                },
            )))
        }
        _ => Err(ProductError::from_code(ProductErrorCode::InvalidRequest)),
    }
}

/// Executes one bounded canonical score range in the requested direction.
fn read_sorted_set_score_range(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    lower: ProductScoreBound,
    upper: ProductScoreBound,
    offset: usize,
    limit: usize,
    order: ProductSortedSetOrder,
) -> Result<ProductStructureReadResult, ProductError> {
    let lower = score_bound(lower)?;
    let upper = score_bound(upper)?;
    let entries = match order {
        ProductSortedSetOrder::Ascending => snapshot
            .inner
            .zrange_by_score(key, lower, upper, offset, limit),
        ProductSortedSetOrder::Descending => snapshot
            .inner
            .zrevrange_by_score(key, lower, upper, offset, limit),
    }?;
    Ok(ProductStructureReadResult::SortedSetEntries(
        entries
            .into_iter()
            .map(|entry| ProductSortedSetEntry {
                member: entry.member().to_vec(),
                score: crate::CanonicalF64::new(entry.score()),
            })
            .collect(),
    ))
}

/// Maps one product score endpoint onto the runtime bound, rejecting NaN.
fn score_bound(bound: ProductScoreBound) -> Result<std::ops::Bound<f64>, ProductError> {
    Ok(match bound {
        ProductScoreBound::Unbounded => std::ops::Bound::Unbounded,
        ProductScoreBound::Inclusive(score) if !score.is_nan() => std::ops::Bound::Included(score),
        ProductScoreBound::Exclusive(score) if !score.is_nan() => std::ops::Bound::Excluded(score),
        _ => return Err(ProductError::from_code(ProductErrorCode::InvalidRequest)),
    })
}

/// Executes one bounded binary-glob page over a hash.
fn read_hash_scan_match(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    pattern: &[u8],
    start_after: Option<&[u8]>,
    output_limit: usize,
    visit_limit: usize,
    match_step_limit: usize,
) -> Result<ProductStructureReadResult, ProductError> {
    let request = hyphae_native_runtime::HashPatternScanRequest::try_new(
        pattern,
        start_after.map(<[u8]>::to_vec),
        output_limit,
        visit_limit,
        match_step_limit,
    )
    .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    let page = snapshot.inner.hscan_match(key, &request)?;
    let stop = match page.stop() {
        hyphae_native_runtime::HashPatternScanStop::Exhausted => ProductHashScanStop::Exhausted,
        hyphae_native_runtime::HashPatternScanStop::OutputLimit => ProductHashScanStop::OutputLimit,
        hyphae_native_runtime::HashPatternScanStop::VisitLimit => ProductHashScanStop::VisitLimit,
    };
    let visited = page.visited();
    let match_steps = page.match_steps();
    let continuation = page.continuation().map(<[u8]>::to_vec);
    Ok(ProductStructureReadResult::HashPage {
        entries: page
            .into_entries()
            .into_iter()
            .map(|entry| ProductHashEntry {
                field: entry.field().to_vec(),
                value: entry.value().to_vec(),
            })
            .collect(),
        continuation,
        stop,
        visited,
        match_steps,
    })
}

fn read_sorted_set_rank(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    member: &[u8],
    order: ProductSortedSetOrder,
) -> Result<ProductStructureReadResult, ProductError> {
    let result = match order {
        ProductSortedSetOrder::Ascending => snapshot.inner.zrank(key, member),
        ProductSortedSetOrder::Descending => snapshot.inner.zrevrank(key, member),
    }?;
    Ok(ProductStructureReadResult::SortedSetRank(result))
}

fn read_sorted_set_range(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    start: i64,
    stop: i64,
    order: ProductSortedSetOrder,
) -> Result<ProductStructureReadResult, ProductError> {
    let entries = match order {
        ProductSortedSetOrder::Ascending => snapshot.inner.zrange(key, start, stop),
        ProductSortedSetOrder::Descending => snapshot.inner.zrevrange(key, start, stop),
    }?;
    Ok(ProductStructureReadResult::SortedSetEntries(
        entries
            .into_iter()
            .map(|entry| ProductSortedSetEntry {
                member: entry.member().to_vec(),
                score: crate::CanonicalF64::new(entry.score()),
            })
            .collect(),
    ))
}

fn read_stream_range(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    start: u64,
    end: u64,
    limit: usize,
) -> Result<ProductStructureReadResult, ProductError> {
    let entries = snapshot.inner.xrange_stream(key, start, end, limit)?;
    Ok(ProductStructureReadResult::StreamEntries(
        entries
            .into_iter()
            .map(|(id, fields)| ProductStreamEntry {
                id,
                fields: fields
                    .into_iter()
                    .map(|(field, value)| ProductHashEntry { field, value })
                    .collect(),
            })
            .collect(),
    ))
}

fn read_hash_scan(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    start_after: Option<&[u8]>,
    limit: usize,
) -> Result<ProductStructureReadResult, ProductError> {
    snapshot
        .inner
        .hscan(key, start_after, limit)
        .map(|entries| {
            ProductStructureReadResult::HashEntries(
                entries
                    .into_iter()
                    .map(|entry| ProductHashEntry {
                        field: entry.field().to_vec(),
                        value: entry.value().to_vec(),
                    })
                    .collect(),
            )
        })
        .map_err(Into::into)
}

fn read_set_algebra(
    snapshot: &crate::ProductSnapshot,
    operation: hyphae_native_runtime::SetAlgebraOperation,
    keys: Vec<Vec<u8>>,
    output_member_limit: usize,
    visit_limit: usize,
) -> Result<ProductStructureReadResult, ProductError> {
    let request = SetAlgebraRequest::try_new(operation, keys, output_member_limit, visit_limit)
        .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
    let result = snapshot.inner.set_algebra(&request)?;
    Ok(ProductStructureReadResult::SetAlgebra {
        members: result.members().to_vec(),
        visited: result.visited(),
    })
}

impl ProductStructureMutation {
    fn structure_key(&self) -> &ProductStructureKey {
        match self {
            Self::StringSet { key, .. }
            | Self::StringDelete { key }
            | Self::CounterAdd { key, .. }
            | Self::Create { key, .. }
            | Self::Delete { key, .. }
            | Self::Expire { key, .. }
            | Self::HashSet { key, .. }
            | Self::HashDelete { key, .. }
            | Self::HashCounterAdd { key, .. }
            | Self::HashExpireField { key, .. }
            | Self::ListPush { key, .. }
            | Self::ListPop { key, .. }
            | Self::SetAdd { key, .. }
            | Self::SetRemove { key, .. }
            | Self::SortedSetAdd { key, .. }
            | Self::SortedSetIncrement { key, .. }
            | Self::SortedSetPop { key, .. }
            | Self::SortedSetRemove { key, .. }
            | Self::StreamAdd { key, .. } => key,
        }
    }

    fn family(&self) -> StructureKind {
        match self {
            Self::StringSet { .. } | Self::StringDelete { .. } => StructureKind::String,
            Self::CounterAdd { .. } => StructureKind::Counter,
            Self::Create { family, .. }
            | Self::Delete { family, .. }
            | Self::Expire { family, .. } => *family,
            Self::HashSet { .. }
            | Self::HashDelete { .. }
            | Self::HashCounterAdd { .. }
            | Self::HashExpireField { .. } => StructureKind::Hash,
            Self::ListPush { .. } | Self::ListPop { .. } => StructureKind::List,
            Self::SetAdd { .. } | Self::SetRemove { .. } => StructureKind::Set,
            Self::SortedSetAdd { .. }
            | Self::SortedSetIncrement { .. }
            | Self::SortedSetPop { .. }
            | Self::SortedSetRemove { .. } => StructureKind::SortedSet,
            Self::StreamAdd { .. } => StructureKind::Stream,
        }
    }
}

impl ProductStructureReadRequest {
    fn uses_internal_structure_namespace(&self) -> bool {
        match self {
            Self::SetAlgebra { keys, .. } => {
                keys.iter().any(|key| crate::is_internal_structure_key(key))
            }
            _ => self
                .structure_key()
                .is_some_and(|key| crate::is_internal_structure_key(&key.key)),
        }
    }

    fn structure_key(&self) -> Option<&ProductStructureKey> {
        match self {
            Self::SetAlgebra { .. } | Self::KeyScanMatch { .. } => None,
            Self::StringGet { key }
            | Self::CounterGet { key }
            | Self::Ttl { key, .. }
            | Self::HashGet { key, .. }
            | Self::HashFieldTtl { key, .. }
            | Self::HashScan { key, .. }
            | Self::HashLength { key }
            | Self::ListRange { key, .. }
            | Self::ListLength { key }
            | Self::SetContains { key, .. }
            | Self::SetMembers { key, .. }
            | Self::SetCardinality { key }
            | Self::SortedSetScore { key, .. }
            | Self::SortedSetRank { key, .. }
            | Self::SortedSetRange { key, .. }
            | Self::SortedSetCardinality { key }
            | Self::StreamRange { key, .. }
            | Self::SortedSetScoreRange { key, .. }
            | Self::HashScanReverse { key, .. }
            | Self::HashScanMatch { key, .. } => Some(key),
        }
    }

    fn keyspace(&self) -> Option<ObjectId> {
        match self {
            Self::SetAlgebra { keyspace, .. } | Self::KeyScanMatch { keyspace, .. } => {
                Some(*keyspace)
            }
            _ => self.structure_key().map(|key| key.keyspace),
        }
    }

    fn family(&self) -> StructureKind {
        match self {
            Self::StringGet { .. } | Self::KeyScanMatch { .. } => StructureKind::String,
            Self::CounterGet { .. } => StructureKind::Counter,
            Self::Ttl { family, .. } => *family,
            Self::HashGet { .. }
            | Self::HashFieldTtl { .. }
            | Self::HashScan { .. }
            | Self::HashScanReverse { .. }
            | Self::HashScanMatch { .. }
            | Self::HashLength { .. } => StructureKind::Hash,
            Self::ListRange { .. } | Self::ListLength { .. } => StructureKind::List,
            Self::SetContains { .. }
            | Self::SetMembers { .. }
            | Self::SetCardinality { .. }
            | Self::SetAlgebra { .. } => StructureKind::Set,
            Self::SortedSetScore { .. }
            | Self::SortedSetRank { .. }
            | Self::SortedSetRange { .. }
            | Self::SortedSetScoreRange { .. }
            | Self::SortedSetCardinality { .. } => StructureKind::SortedSet,
            Self::StreamRange { .. } => StructureKind::Stream,
        }
    }
}

impl ProductTransactionSearchMutation {
    fn index(&self) -> ObjectId {
        match self {
            Self::Index { index, .. }
            | Self::Replace { index, .. }
            | Self::Delete { index, .. } => *index,
            Self::Document { collection, .. } => *collection,
        }
    }
}

impl ProductTransactionVectorMutation {
    fn index(&self) -> ObjectId {
        match self {
            Self::Upsert { index, .. } | Self::Delete { index, .. } => *index,
        }
    }
}

fn validate_structure_key_for_snapshot(
    snapshot: &crate::ProductSnapshot,
    key: Option<&ProductStructureKey>,
    family: StructureKind,
) -> Result<(), ProductError> {
    let keyspace = key.map_or_else(
        || None,
        |key| snapshot.inner.logical_catalog_object(key.keyspace),
    );
    if let Some(key) = key {
        validate_catalogued_structure(
            key.keyspace,
            family,
            snapshot.inner.catalog_object(key.keyspace),
            keyspace,
        )
    } else {
        Ok(())
    }
}

fn validate_structure_key_for_batch(
    batch: &NativeWriteBatch,
    key: &ProductStructureKey,
    family: StructureKind,
) -> Result<(), ProductError> {
    validate_catalogued_structure(
        key.keyspace,
        family,
        batch.catalog_object(key.keyspace),
        batch.logical_catalog_object(key.keyspace),
    )
}

fn validate_catalogued_structure(
    keyspace: ObjectId,
    family: StructureKind,
    legacy: Option<&CatalogObject>,
    logical: Option<&LogicalCatalogObject>,
) -> Result<(), ProductError> {
    let found = match (legacy, logical) {
        (Some(CatalogObject::Structure(definition)), _) => Some(definition.kind),
        (_, Some(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(definition)))) => {
            Some(definition.kind)
        }
        (_, Some(LogicalCatalogObject::Compatible(definition))) => match &definition.object {
            CatalogObject::Structure(definition) => Some(definition.kind),
            _ => None,
        },
        _ => None,
    };
    match found {
        Some(found) if found == family => Ok(()),
        Some(_) => Err(ProductError::from_code(ProductErrorCode::InvalidRequest)),
        None => {
            let _ = keyspace;
            Err(ProductError::from_code(ProductErrorCode::ObjectNotFound))
        }
    }
}

fn structure_ttl(
    snapshot: &crate::ProductSnapshot,
    key: &[u8],
    family: StructureKind,
) -> ProductTtl {
    match family {
        StructureKind::String | StructureKind::Counter => snapshot.inner.ttl(key),
        StructureKind::Hash => snapshot.inner.ttl_hash(key),
        StructureKind::List => snapshot.inner.ttl_list(key),
        StructureKind::Set => snapshot.inner.ttl_set(key),
        StructureKind::SortedSet => snapshot.inner.ttl_sorted_set(key),
        StructureKind::Stream => snapshot.inner.ttl_stream(key),
    }
    .into()
}

fn parse_product_counter(value: &[u8]) -> Result<i64, ProductError> {
    let text = std::str::from_utf8(value)
        .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    let parsed = text
        .parse::<i64>()
        .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    if parsed.to_string().as_bytes() != value {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    Ok(parsed)
}

fn stage_transaction(
    database: &NativeDatabase,
    session: &mut ProductSession,
    handle: ProductTransactionHandle,
    authorization: &ProductAuthorizationRequirement,
    limits: ProductLimits,
    stage: impl FnOnce(&mut NativeWriteBatch) -> Result<ProductTransactionStageResult, ProductError>,
) -> Result<ProductTransactionStageReceipt, ProductError> {
    let current = session
        .active_transaction(handle)
        .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    if current.staged_operations >= crate::MAX_PRODUCT_TRANSACTION_OPERATIONS {
        return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
    }
    let staged_operations = current.staged_operations;
    let durability = current.durability;
    let mut candidate_batch = database.clone_legacy_materialized_write_batch(&current.batch)?;
    let before = candidate_batch.mutation_count();
    let result = stage(&mut candidate_batch)?;
    let changed = candidate_batch.mutation_count() > before;
    let operation_ordinal = staged_operations + 1;
    let receipt = ProductTransactionStageReceipt {
        handle,
        operation_ordinal,
        changed,
        result,
    };
    let response_bytes = transaction_stage_response_bytes(&receipt);
    limits.admit_response(1, response_bytes, response_bytes)?;
    let source = session
        .take_active_transaction(handle)
        .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
    let mut retained_authorization = source.authorization;
    retained_authorization.union(authorization);
    let candidate = crate::session::ActiveProductTransaction {
        batch: candidate_batch.finish(source.batch)?,
        staged_operations: operation_ordinal,
        durability,
        authorization: retained_authorization,
    };
    session.replace_active_transaction(handle, candidate);
    Ok(receipt)
}

fn transaction_stage_response_bytes(receipt: &ProductTransactionStageReceipt) -> usize {
    // Product limits count the complete canonical Native response envelope.
    const ENVELOPE_AND_STAGE_HEADER_BYTES: usize = 16 + 8 + 8 + 1 + 1;
    ENVELOPE_AND_STAGE_HEADER_BYTES.saturating_add(match &receipt.result {
        ProductTransactionStageResult::Sql(result) => sql_result_wire_bytes(result),
        ProductTransactionStageResult::Structure(result) => {
            structure_mutation_result_wire_bytes(result)
        }
        ProductTransactionStageResult::Search => 0,
        ProductTransactionStageResult::Vector(_) => 1,
    })
}

fn structure_mutation_result_wire_bytes(result: &ProductStructureMutationResult) -> usize {
    match result {
        ProductStructureMutationResult::Unit => 1,
        ProductStructureMutationResult::Integer(_)
        | ProductStructureMutationResult::Count(_)
        | ProductStructureMutationResult::StreamId(_)
        | ProductStructureMutationResult::Score(_) => 9,
        ProductStructureMutationResult::Boolean(_) => 2,
        ProductStructureMutationResult::PoppedEntry(entry) => entry
            .as_ref()
            .map_or(2, |entry| 14_usize.saturating_add(entry.member.len())),
        ProductStructureMutationResult::Value(value) => value
            .as_ref()
            .map_or(2, |value| 6_usize.saturating_add(value.len())),
    }
}

fn sql_result_wire_bytes(result: &ProductSqlResult) -> usize {
    match result {
        ProductSqlResult::Command { object_id, .. } => {
            16_usize.saturating_add(usize::from(object_id.is_some()).saturating_mul(16))
        }
        ProductSqlResult::Rows { columns, rows } => {
            let columns = columns.iter().fold(0_usize, |total, column| {
                total.saturating_add(4).saturating_add(column.len())
            });
            let values = rows
                .iter()
                .flat_map(|row| row.iter())
                .fold(0_usize, |total, value| {
                    total.saturating_add(value_wire_bytes(value, 0))
                });
            16_usize.saturating_add(columns).saturating_add(values)
        }
    }
}

fn value_wire_bytes(value: &ProductValue, depth: usize) -> usize {
    if depth > 8 {
        return usize::MAX;
    }
    match value {
        ProductValue::Null => 1,
        ProductValue::Boolean(_) => 2,
        ProductValue::Signed(_)
        | ProductValue::Unsigned(_)
        | ProductValue::Float64(_)
        | ProductValue::Time(_)
        | ProductValue::Timestamp(_) => 9,
        ProductValue::Decimal(_) | ProductValue::Uuid(_) | ProductValue::Interval { .. } => 17,
        ProductValue::Float32(_) | ProductValue::Date(_) => 5,
        ProductValue::Text(value) | ProductValue::Json(value) => {
            5_usize.saturating_add(value.len())
        }
        ProductValue::Binary(value) => 5_usize.saturating_add(value.len()),
        ProductValue::Array(values) => values.iter().fold(5_usize, |total, value| {
            total.saturating_add(value_wire_bytes(value, depth + 1))
        }),
        ProductValue::Map(entries) => entries.iter().fold(5_usize, |total, (key, value)| {
            total
                .saturating_add(value_wire_bytes(key, depth + 1))
                .saturating_add(value_wire_bytes(value, depth + 1))
        }),
        ProductValue::Vector(values) => 5_usize.saturating_add(values.len().saturating_mul(4)),
        _ => usize::MAX,
    }
}

fn restore_success_response_bytes(request: &RestoreRequest) -> usize {
    // A successful restore always returns both request paths, a healthy doctor
    // report with recovery evidence, and all six ordered restore phases.
    222_usize
        .saturating_add(request.backup.as_os_str().as_encoded_bytes().len())
        .saturating_add(request.destination.as_os_str().as_encoded_bytes().len())
}

fn restore_response_bytes(info: &crate::RestoreInfo) -> usize {
    let doctor_bytes = 44_usize
        .saturating_add(usize::from(info.doctor.directory_lineage.is_some()).saturating_mul(24))
        .saturating_add(usize::from(info.doctor.recovery.is_some()).saturating_mul(64));
    16_usize
        .saturating_add(4)
        .saturating_add(info.data_path.as_os_str().as_encoded_bytes().len())
        .saturating_add(4)
        .saturating_add(info.backup.path.as_os_str().as_encoded_bytes().len())
        .saturating_add(56)
        .saturating_add(doctor_bytes)
        .saturating_add(4)
        .saturating_add(info.phases.len())
}

fn validate_transaction_sql(mutation: &ProductTransactionSqlMutation) -> Result<(), ProductError> {
    if mutation.statement.len() > crate::MAX_PRODUCT_SQL_STATEMENT_BYTES
        || mutation.parameters.len() > crate::MAX_PRODUCT_SQL_PARAMETERS
        || sql_statement_class(&mutation.statement)? != SqlStatementClass::DataMutation
    {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    Ok(())
}

fn apply_search_mutation(
    product: &NativeProduct,
    batch: &mut NativeWriteBatch,
    mutation: ProductTransactionSearchMutation,
    logical_time_micros: i64,
) -> Result<(), ProductError> {
    match mutation {
        ProductTransactionSearchMutation::Index {
            index,
            document_id,
            text,
        } => batch.index_document(index, document_id, text)?,
        ProductTransactionSearchMutation::Replace {
            index,
            document_id,
            text,
        } => batch.replace_document(index, document_id, text)?,
        ProductTransactionSearchMutation::Delete { index, document_id } => {
            batch.delete_document(index, document_id)?;
        }
        ProductTransactionSearchMutation::Document {
            collection,
            document,
        } => product.stage_document_in_batch(batch, collection, &document, logical_time_micros)?,
    }
    Ok(())
}

fn apply_vector_mutation(
    batch: &mut NativeWriteBatch,
    mutation: ProductTransactionVectorMutation,
) -> Result<bool, ProductError> {
    match mutation {
        ProductTransactionVectorMutation::Upsert {
            index,
            object_id,
            vector,
        } => {
            batch.upsert_vector(index, object_id, vector)?;
            Ok(true)
        }
        ProductTransactionVectorMutation::Delete { index, object_id } => {
            batch.delete_vector(index, object_id).map_err(Into::into)
        }
    }
}

fn handle_explicit_commit_error(
    session: &mut ProductSession,
    handle: ProductTransactionHandle,
    staged_operations: usize,
    resolution_id: Option<ProductTransactionId>,
    error: hyphae_native_runtime::NativeRuntimeError,
) -> ProductError {
    if commit_publication_may_be_unknown(&error) {
        if let Some(resolution_id) = resolution_id {
            session.record_transaction(
                resolution_id,
                ProductTransactionStatus::OutcomeUnknown {
                    transaction_id: resolution_id,
                },
            );
            session.record_explicit_status(
                handle,
                ProductExplicitTransactionStatus::OutcomeUnknown {
                    handle,
                    transaction_id: resolution_id,
                    staged_operations,
                },
            );
            return ProductFailureBoundary::publication_unknown(resolution_id.native())
                .apply(ProductError::from(error));
        }
        return ProductError::from(error);
    }
    let product_error = ProductError::from(error);
    if commit_failure_proves_rollback(&product_error) {
        session.record_explicit_status(
            handle,
            ProductExplicitTransactionStatus::RolledBack {
                handle,
                discarded_operations: staged_operations,
            },
        );
        if let Some(resolution_id) = resolution_id {
            session.record_transaction(
                resolution_id,
                ProductTransactionStatus::RolledBack {
                    transaction_id: resolution_id,
                },
            );
            return ProductFailureBoundary::rolled_back(resolution_id.native())
                .apply(product_error);
        }
    }
    product_error
}

fn commit_failure_proves_rollback(error: &ProductError) -> bool {
    matches!(
        error.code(),
        ProductErrorCode::WriteConflict
            | ProductErrorCode::SqlUniqueViolation
            | ProductErrorCode::SqlCheckViolation
            | ProductErrorCode::SqlForeignKeyViolation
            | ProductErrorCode::CatalogConflict
            | ProductErrorCode::IdempotencyConflict
            | ProductErrorCode::InvalidRequest
            | ProductErrorCode::LimitExceeded
    )
}

fn commit_publication_may_be_unknown(error: &hyphae_native_runtime::NativeRuntimeError) -> bool {
    use hyphae_native_runtime::{CommitBoundary, NativeRuntimeError};

    error.is_io()
        || matches!(
            error,
            NativeRuntimeError::Wal(_) | NativeRuntimeError::Mvcc(_)
        )
        || matches!(
            error,
            NativeRuntimeError::InjectedCrash(
                CommitBoundary::WalAppended
                    | CommitBoundary::WalSynchronized
                    | CommitBoundary::RootPublished
            )
        )
}

impl ProductOperation {
    /// Returns the stable registry name for a durable security mutation.
    ///
    /// Offline owner-recovery and legacy migration calls are intentionally not
    /// `ProductOperation` variants and are inventoried separately.
    #[doc(hidden)]
    pub const fn security_mutation_registry_name(&self) -> Option<&'static str> {
        match self {
            Self::SecurityPrincipalCreate { .. } => Some("SecurityPrincipalCreate"),
            Self::SecurityPrincipalSetEnabled { .. } => Some("SecurityPrincipalSetEnabled"),
            Self::SecurityCustomRoleCreate { .. } => Some("SecurityCustomRoleCreate"),
            Self::SecurityBuiltInAssignmentCreate { .. } => Some("SecurityBuiltInAssignmentCreate"),
            Self::SecurityCustomAssignmentCreate { .. } => Some("SecurityCustomAssignmentCreate"),
            Self::SecurityAssignmentRevoke { .. } => Some("SecurityAssignmentRevoke"),
            Self::SecurityApiKeyIssueSelfStart { .. } => Some("SecurityApiKeyIssueSelfStart"),
            Self::SecurityApiKeyIssueStart { .. } => Some("SecurityApiKeyIssueStart"),
            Self::SecurityApiKeyIssueSelfActivate { .. } => Some("SecurityApiKeyIssueSelfActivate"),
            Self::SecurityApiKeyIssueActivate { .. } => Some("SecurityApiKeyIssueActivate"),
            Self::SecurityApiKeyRotateSelfStart { .. } => Some("SecurityApiKeyRotateSelfStart"),
            Self::SecurityApiKeyRotateStart { .. } => Some("SecurityApiKeyRotateStart"),
            Self::SecurityApiKeyRotateSelfActivate { .. } => {
                Some("SecurityApiKeyRotateSelfActivate")
            }
            Self::SecurityApiKeyRotateActivate { .. } => Some("SecurityApiKeyRotateActivate"),
            Self::SecurityApiKeyIssueSelfAbort { .. } => Some("SecurityApiKeyIssueSelfAbort"),
            Self::SecurityApiKeyIssueAbort { .. } => Some("SecurityApiKeyIssueAbort"),
            Self::SecurityApiKeyRotateSelfAbort { .. } => Some("SecurityApiKeyRotateSelfAbort"),
            Self::SecurityApiKeyRotateAbort { .. } => Some("SecurityApiKeyRotateAbort"),
            Self::SecurityApiKeyRevokeSelf { .. } => Some("SecurityApiKeyRevokeSelf"),
            Self::SecurityApiKeyRevoke { .. } => Some("SecurityApiKeyRevoke"),
            Self::SecurityLegacyBearerRevoke => Some("SecurityLegacyBearerRevoke"),
            _ => None,
        }
    }

    /// Returns whether this is a strict managed credential-lifecycle write.
    #[must_use]
    pub fn is_key_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::SecurityApiKeyIssueSelfStart { .. }
                | Self::SecurityApiKeyIssueStart { .. }
                | Self::SecurityApiKeyIssueSelfActivate { .. }
                | Self::SecurityApiKeyIssueActivate { .. }
                | Self::SecurityApiKeyRotateSelfStart { .. }
                | Self::SecurityApiKeyRotateStart { .. }
                | Self::SecurityApiKeyRotateSelfActivate { .. }
                | Self::SecurityApiKeyRotateActivate { .. }
                | Self::SecurityApiKeyIssueSelfAbort { .. }
                | Self::SecurityApiKeyIssueAbort { .. }
                | Self::SecurityApiKeyRotateSelfAbort { .. }
                | Self::SecurityApiKeyRotateAbort { .. }
                | Self::SecurityApiKeyRevokeSelf { .. }
                | Self::SecurityApiKeyRevoke { .. }
                | Self::SecurityLegacyBearerRevoke
        )
    }

    fn is_self_key_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::SecurityApiKeyIssueSelfStart { .. }
                | Self::SecurityApiKeyIssueSelfActivate { .. }
                | Self::SecurityApiKeyRotateSelfStart { .. }
                | Self::SecurityApiKeyRotateSelfActivate { .. }
                | Self::SecurityApiKeyIssueSelfAbort { .. }
                | Self::SecurityApiKeyRotateSelfAbort { .. }
                | Self::SecurityApiKeyRevokeSelf { .. }
        )
    }

    fn validate_limits(&self, limits: ProductLimits) -> Result<(), ProductError> {
        let valid = match self {
            Self::CatalogList(request) => {
                request.byte_limit <= limits.max_response_bytes
                    && request.byte_limit <= limits.max_memory_bytes
            }
            Self::CatalogVisibleList(request) => {
                request.cursor.as_ref().is_none_or(|cursor| {
                    cursor.encoded_len() <= crate::MAX_CATALOG_VISIBLE_CURSOR_BYTES
                }) && request.byte_limit <= limits.max_response_bytes
                    && request.byte_limit <= limits.max_memory_bytes
            }
            Self::CatalogDependencies(request) => {
                request.byte_limit <= limits.max_response_bytes
                    && request.byte_limit <= limits.max_memory_bytes
            }
            Self::StructureMutate { mutations } => {
                !mutations.is_empty()
                    && mutations.len() <= crate::MAX_PRODUCT_TRANSACTION_OPERATIONS
            }
            Self::StructureRead(
                ProductStructureReadRequest::HashScan { limit, .. }
                | ProductStructureReadRequest::SetMembers { limit, .. }
                | ProductStructureReadRequest::StreamRange { limit, .. },
            ) => *limit <= limits.max_count,
            Self::StructureRead(ProductStructureReadRequest::SetAlgebra {
                keys,
                output_member_limit,
                visit_limit,
                ..
            }) => {
                keys.len() <= limits.max_count
                    && *output_member_limit <= limits.max_count
                    && *visit_limit <= limits.max_work_units
            }
            Self::SecurityPrincipalList(request) => request.limit <= limits.max_count,
            Self::SecurityRoleList(request) => request.limit <= limits.max_count,
            Self::SecurityAssignmentList(request) => request.limit <= limits.max_count,
            Self::SecurityKeyList(request) => request.limit <= limits.max_count,
            Self::SecurityAuditRead(request) => request.limit <= limits.max_count,
            Self::Prove {
                operation,
                limits: _,
            } => operation.validate_limits(limits).is_ok(),
            _ => true,
        };
        if !valid
            && matches!(self, Self::CatalogVisibleList(request) if request.cursor.as_ref().is_some_and(|cursor| cursor.encoded_len() > crate::MAX_CATALOG_VISIBLE_CURSOR_BYTES))
        {
            Err(ProductError::from_code(ProductErrorCode::CatalogConflict))
        } else if valid {
            Ok(())
        } else {
            Err(ProductError::from_code(ProductErrorCode::LimitExceeded))
        }
    }

    #[allow(clippy::match_same_arms, clippy::too_many_lines)]
    fn required_permissions(&self) -> Result<ProductAuthorization, ProductError> {
        let required = match self {
            Self::Capabilities => authorization([ProductPermission::Discover]),
            Self::CatalogObject { .. }
            | Self::CatalogObjectNamed { .. }
            | Self::CatalogList(_)
            | Self::CatalogVisibleList(_)
            | Self::CatalogDependencies(_)
            | Self::CatalogDescribe { .. }
            | Self::CatalogResolve { .. } => authorization([ProductPermission::CatalogRead]),
            Self::CatalogCreate { .. } => authorization([ProductPermission::CatalogWrite]),
            Self::PrepareSql { .. } | Self::ExecutePrepared { .. } => {
                authorization([ProductPermission::CatalogRead, ProductPermission::DataRead])
            }
            Self::DeallocatePrepared { .. }
            | Self::StructureGet { .. }
            | Self::StructureTtl { .. }
            | Self::StructureRead(_)
            | Self::TransactionStatus { .. }
            | Self::TransactionStatusByIdempotency { .. }
            | Self::ExplicitTransactionStatus { .. } => {
                authorization([ProductPermission::DataRead])
            }
            Self::ExecuteSql { statement, .. } => match sql_statement_class(statement)? {
                SqlStatementClass::Read => {
                    authorization([ProductPermission::CatalogRead, ProductPermission::DataRead])
                }
                SqlStatementClass::DataMutation => {
                    authorization([ProductPermission::CatalogRead, ProductPermission::DataWrite])
                }
                SqlStatementClass::CatalogMutation => {
                    authorization([ProductPermission::CatalogWrite])
                }
            },
            Self::StructureSet { .. }
            | Self::StructureMutate { .. }
            | Self::TransactionBegin
            | Self::TransactionStageStructure { .. }
            | Self::TransactionCommit { .. }
            | Self::TransactionRollback { .. } => authorization([ProductPermission::DataWrite]),
            Self::TransactionStageSql { .. }
            | Self::TransactionStageSearch { .. }
            | Self::TransactionStageVector { .. }
            | Self::SearchIngest { .. }
            | Self::SearchDocumentUpdate { .. }
            | Self::SearchDocumentDelete { .. } => {
                authorization([ProductPermission::CatalogRead, ProductPermission::DataWrite])
            }
            Self::Search { .. } | Self::SearchCollection { .. } => authorization([
                ProductPermission::CatalogRead,
                ProductPermission::SearchExecute,
            ]),
            Self::AdminStatus | Self::Telemetry => authorization([ProductPermission::Observe]),
            Self::AdminCheckpoint | Self::Doctor(_) => authorization([ProductPermission::Maintain]),
            Self::AdminExplainSql { .. } => {
                authorization([ProductPermission::CatalogRead, ProductPermission::Observe])
            }
            Self::Backup(_) => authorization([ProductPermission::BackupCreate]),
            Self::Restore(_) => authorization([ProductPermission::Restore]),
            Self::SecurityStatus
            | Self::SecurityPrincipalList(_)
            | Self::SecurityRoleList(_)
            | Self::SecurityAssignmentList(_)
            | Self::SecurityKeyList(_) => authorization([ProductPermission::SecurityRead]),
            Self::SecurityPrincipalCreate { .. }
            | Self::SecurityPrincipalSetEnabled { .. }
            | Self::SecurityCustomRoleCreate { .. }
            | Self::SecurityBuiltInAssignmentCreate { .. }
            | Self::SecurityCustomAssignmentCreate { .. }
            | Self::SecurityAssignmentRevoke { .. } => {
                authorization([ProductPermission::SecurityManage])
            }
            Self::SecurityApiKeyIssueSelfStart { .. }
            | Self::SecurityApiKeyIssueSelfActivate { .. }
            | Self::SecurityApiKeyRotateSelfStart { .. }
            | Self::SecurityApiKeyRotateSelfActivate { .. }
            | Self::SecurityApiKeyIssueSelfAbort { .. }
            | Self::SecurityApiKeyRotateSelfAbort { .. }
            | Self::SecurityApiKeyRevokeSelf { .. } => {
                authorization([ProductPermission::CredentialSelfManage])
            }
            Self::SecurityApiKeyIssueStart { roles, .. } if roles.contains(&BuiltInRole::Owner) => {
                authorization([
                    ProductPermission::SecurityManage,
                    ProductPermission::OwnershipManage,
                ])
            }
            Self::SecurityApiKeyIssueStart { .. }
            | Self::SecurityApiKeyIssueActivate { .. }
            | Self::SecurityApiKeyRotateStart { .. }
            | Self::SecurityApiKeyRotateActivate { .. }
            | Self::SecurityApiKeyIssueAbort { .. }
            | Self::SecurityApiKeyRotateAbort { .. }
            | Self::SecurityApiKeyRevoke { .. } => {
                authorization([ProductPermission::SecurityManage])
            }
            Self::SecurityAuditRead(_) => authorization([ProductPermission::AuditRead]),
            Self::SecurityLegacyBearerRevoke => authorization([ProductPermission::OwnershipManage]),
            Self::VerifyProof { .. } => authorization([ProductPermission::ProofVerify]),
            Self::Prove { operation, .. } => operation
                .required_permissions()?
                .union(authorization([ProductPermission::ProofGenerate])),
        };
        Ok(required)
    }

    /// Returns whether this operation is a side-effect-free read eligible for
    /// provisional read streaming.
    ///
    /// Session mutations such as preparing SQL or staging and rolling back an
    /// explicit transaction are deliberately excluded even when they do not
    /// publish durable product state.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn is_read_only(&self) -> bool {
        match self {
            Self::Capabilities
            | Self::CatalogObject { .. }
            | Self::CatalogObjectNamed { .. }
            | Self::CatalogList(_)
            | Self::CatalogVisibleList(_)
            | Self::CatalogDependencies(_)
            | Self::CatalogDescribe { .. }
            | Self::CatalogResolve { .. }
            | Self::ExecutePrepared { .. }
            | Self::StructureGet { .. }
            | Self::StructureTtl { .. }
            | Self::StructureRead(_)
            | Self::ExplicitTransactionStatus { .. }
            | Self::TransactionStatus { .. }
            | Self::TransactionStatusByIdempotency { .. }
            | Self::Search { .. }
            | Self::SearchCollection { .. }
            | Self::AdminStatus
            | Self::AdminExplainSql { .. }
            | Self::Doctor(_)
            | Self::Telemetry
            | Self::VerifyProof { .. }
            | Self::SecurityStatus
            | Self::SecurityPrincipalList(_)
            | Self::SecurityRoleList(_)
            | Self::SecurityAssignmentList(_)
            | Self::SecurityKeyList(_)
            | Self::SecurityAuditRead(_) => true,
            Self::ExecuteSql { statement, .. } => {
                matches!(
                    classify_sql_statement(statement),
                    Ok(SqlStatementClass::Read)
                )
            }
            Self::Prove { operation, .. } => operation.is_read_only(),
            Self::CatalogCreate { .. }
            | Self::PrepareSql { .. }
            | Self::DeallocatePrepared { .. }
            | Self::StructureSet { .. }
            | Self::StructureMutate { .. }
            | Self::TransactionBegin
            | Self::TransactionStageSql { .. }
            | Self::TransactionStageStructure { .. }
            | Self::TransactionStageSearch { .. }
            | Self::TransactionStageVector { .. }
            | Self::TransactionCommit { .. }
            | Self::TransactionRollback { .. }
            | Self::SearchIngest { .. }
            | Self::SearchDocumentUpdate { .. }
            | Self::SearchDocumentDelete { .. }
            | Self::AdminCheckpoint
            | Self::Backup(_)
            | Self::Restore(_)
            | Self::SecurityPrincipalCreate { .. }
            | Self::SecurityPrincipalSetEnabled { .. }
            | Self::SecurityCustomRoleCreate { .. }
            | Self::SecurityBuiltInAssignmentCreate { .. }
            | Self::SecurityCustomAssignmentCreate { .. }
            | Self::SecurityAssignmentRevoke { .. } => false,
            Self::SecurityApiKeyIssueSelfStart { .. }
            | Self::SecurityApiKeyIssueStart { .. }
            | Self::SecurityApiKeyIssueSelfActivate { .. }
            | Self::SecurityApiKeyIssueActivate { .. }
            | Self::SecurityApiKeyRotateSelfStart { .. }
            | Self::SecurityApiKeyRotateStart { .. }
            | Self::SecurityApiKeyRotateSelfActivate { .. }
            | Self::SecurityApiKeyRotateActivate { .. }
            | Self::SecurityApiKeyIssueSelfAbort { .. }
            | Self::SecurityApiKeyIssueAbort { .. }
            | Self::SecurityApiKeyRotateSelfAbort { .. }
            | Self::SecurityApiKeyRotateAbort { .. }
            | Self::SecurityApiKeyRevokeSelf { .. }
            | Self::SecurityApiKeyRevoke { .. } => false,
            Self::SecurityLegacyBearerRevoke => false,
        }
    }

    #[allow(clippy::match_same_arms)]
    fn requires_managed_authority(&self) -> bool {
        match self {
            Self::SecurityStatus
            | Self::SecurityPrincipalList(_)
            | Self::SecurityRoleList(_)
            | Self::SecurityAssignmentList(_)
            | Self::SecurityKeyList(_)
            | Self::SecurityAuditRead(_)
            | Self::SecurityPrincipalCreate { .. }
            | Self::SecurityPrincipalSetEnabled { .. }
            | Self::SecurityCustomRoleCreate { .. }
            | Self::SecurityBuiltInAssignmentCreate { .. }
            | Self::SecurityCustomAssignmentCreate { .. }
            | Self::SecurityAssignmentRevoke { .. } => true,
            Self::SecurityApiKeyIssueSelfStart { .. }
            | Self::SecurityApiKeyIssueStart { .. }
            | Self::SecurityApiKeyIssueSelfActivate { .. }
            | Self::SecurityApiKeyIssueActivate { .. }
            | Self::SecurityApiKeyRotateSelfStart { .. }
            | Self::SecurityApiKeyRotateStart { .. }
            | Self::SecurityApiKeyRotateSelfActivate { .. }
            | Self::SecurityApiKeyRotateActivate { .. }
            | Self::SecurityApiKeyIssueSelfAbort { .. }
            | Self::SecurityApiKeyIssueAbort { .. }
            | Self::SecurityApiKeyRotateSelfAbort { .. }
            | Self::SecurityApiKeyRotateAbort { .. }
            | Self::SecurityApiKeyRevokeSelf { .. }
            | Self::SecurityApiKeyRevoke { .. } => true,
            Self::SecurityLegacyBearerRevoke => true,
            Self::Prove { operation, .. } => operation.requires_managed_authority(),
            _ => false,
        }
    }

    fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::CatalogCreate { .. }
                | Self::StructureSet { .. }
                | Self::StructureMutate { .. }
                | Self::TransactionCommit { .. }
                | Self::SearchIngest { .. }
                | Self::SearchDocumentUpdate { .. }
                | Self::SearchDocumentDelete { .. }
                | Self::AdminCheckpoint
                | Self::Backup(_)
                | Self::Restore(_)
                | Self::SecurityPrincipalCreate { .. }
                | Self::SecurityPrincipalSetEnabled { .. }
                | Self::SecurityCustomRoleCreate { .. }
                | Self::SecurityBuiltInAssignmentCreate { .. }
                | Self::SecurityCustomAssignmentCreate { .. }
                | Self::SecurityAssignmentRevoke { .. }
                | Self::SecurityApiKeyIssueSelfStart { .. }
                | Self::SecurityApiKeyIssueStart { .. }
                | Self::SecurityApiKeyIssueSelfActivate { .. }
                | Self::SecurityApiKeyIssueActivate { .. }
                | Self::SecurityApiKeyRotateSelfStart { .. }
                | Self::SecurityApiKeyRotateStart { .. }
                | Self::SecurityApiKeyRotateSelfActivate { .. }
                | Self::SecurityApiKeyRotateActivate { .. }
                | Self::SecurityApiKeyIssueSelfAbort { .. }
                | Self::SecurityApiKeyIssueAbort { .. }
                | Self::SecurityApiKeyRotateSelfAbort { .. }
                | Self::SecurityApiKeyRotateAbort { .. }
                | Self::SecurityApiKeyRevokeSelf { .. }
                | Self::SecurityApiKeyRevoke { .. }
                | Self::SecurityLegacyBearerRevoke
        ) || matches!(
            self,
            Self::ExecuteSql { statement, .. }
                if matches!(
                    classify_sql_statement(statement),
                    Ok(SqlStatementClass::DataMutation | SqlStatementClass::CatalogMutation)
                )
        ) || matches!(self, Self::Prove { operation, .. } if operation.is_mutating())
    }

    fn requires_idempotency_token(&self) -> bool {
        matches!(
            self,
            Self::SecurityPrincipalCreate { .. }
                | Self::SecurityPrincipalSetEnabled { .. }
                | Self::SecurityCustomRoleCreate { .. }
                | Self::SecurityBuiltInAssignmentCreate { .. }
                | Self::SecurityCustomAssignmentCreate { .. }
                | Self::SecurityAssignmentRevoke { .. }
                | Self::SecurityApiKeyIssueSelfStart { .. }
                | Self::SecurityApiKeyIssueStart { .. }
                | Self::SecurityApiKeyIssueSelfActivate { .. }
                | Self::SecurityApiKeyIssueActivate { .. }
                | Self::SecurityApiKeyRotateSelfStart { .. }
                | Self::SecurityApiKeyRotateStart { .. }
                | Self::SecurityApiKeyRotateSelfActivate { .. }
                | Self::SecurityApiKeyRotateActivate { .. }
                | Self::SecurityApiKeyIssueSelfAbort { .. }
                | Self::SecurityApiKeyIssueAbort { .. }
                | Self::SecurityApiKeyRotateSelfAbort { .. }
                | Self::SecurityApiKeyRotateAbort { .. }
                | Self::SecurityApiKeyRevokeSelf { .. }
                | Self::SecurityApiKeyRevoke { .. }
                | Self::SecurityLegacyBearerRevoke
        )
    }

    fn request_cost(&self) -> (usize, usize, usize, usize) {
        let (count, bytes, work) = self.request_cost_parts();
        (count, bytes, work, bytes)
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::match_same_arms)]
    fn request_cost_parts(&self) -> (usize, usize, usize) {
        match self {
            Self::Capabilities
            | Self::AdminStatus
            | Self::AdminCheckpoint
            | Self::TransactionStatus { .. }
            | Self::TransactionStatusByIdempotency { .. }
            | Self::TransactionBegin
            | Self::ExplicitTransactionStatus { .. }
            | Self::TransactionCommit { .. }
            | Self::TransactionRollback { .. }
            | Self::Telemetry
            | Self::SecurityStatus => (1, 0, 1),
            Self::DeallocatePrepared { .. } => (1, 8, 1),
            Self::AdminExplainSql { statement } | Self::PrepareSql { statement } => {
                (1, statement.len(), statement.len())
            }
            Self::CatalogObject { .. }
            | Self::CatalogDescribe { .. }
            | Self::SecurityPrincipalSetEnabled { .. }
            | Self::SecurityAssignmentRevoke { .. } => (1, 16, 1),
            Self::CatalogObjectNamed { name } | Self::CatalogResolve { name } => {
                (1, qualified_name_bytes(name), 1)
            }
            Self::CatalogList(request) => (
                request.item_limit,
                0,
                request.visit_limit.max(request.item_limit),
            ),
            Self::CatalogVisibleList(request) => (
                request.item_limit,
                request
                    .cursor
                    .as_ref()
                    .map_or(0, crate::CatalogVisibleCursor::encoded_len),
                request.visit_limit.max(request.item_limit),
            ),
            Self::CatalogDependencies(request) => (
                request.item_limit,
                0,
                request.visit_limit.max(request.item_limit),
            ),
            Self::CatalogCreate { object } => (1, format!("{object:?}").len(), 1),
            Self::ExecutePrepared { parameters, .. } => (
                parameters.len(),
                values_bytes(parameters),
                parameters.len().max(1),
            ),
            Self::ExecuteSql {
                statement,
                parameters,
            } => (
                parameters.len().max(1),
                statement.len().saturating_add(values_bytes(parameters)),
                statement.len().saturating_add(parameters.len()),
            ),
            Self::StructureGet { key } | Self::StructureTtl { key } => (1, key.len(), 1),
            Self::StructureSet { key, value, .. } => (1, key.len().saturating_add(value.len()), 1),
            Self::StructureMutate { .. }
            | Self::StructureRead(_)
            | Self::TransactionStageSql { .. }
            | Self::TransactionStageStructure { .. }
            | Self::TransactionStageSearch { .. }
            | Self::TransactionStageVector { .. } => complex_operation_cost(self),
            Self::Search { query, limit, .. } => {
                let bytes = search_query_bytes(query);
                (*limit, bytes, bytes.max(*limit))
            }
            Self::SearchCollection { request, .. } => {
                let bytes = format!("{request:?}").len().saturating_add(16);
                (request.limit, bytes, bytes.max(request.limit))
            }
            Self::SearchIngest { batch, .. } => {
                let bytes = format!("{batch:?}").len().saturating_add(16);
                (
                    batch.documents.len(),
                    bytes,
                    bytes.max(batch.documents.len()),
                )
            }
            Self::SearchDocumentUpdate { update, .. } => {
                let bytes = format!("{update:?}").len().saturating_add(16);
                (1, bytes, bytes)
            }
            Self::SearchDocumentDelete { .. } => (1, 48, 1),
            Self::Doctor(request) => (1, request.path.as_os_str().as_encoded_bytes().len(), 1),
            Self::Backup(request) => (
                1,
                request.destination.as_os_str().as_encoded_bytes().len(),
                1,
            ),
            Self::SecurityPrincipalList(request) => (request.limit, 0, request.limit.max(1)),
            Self::SecurityRoleList(request) => (request.limit, 0, request.limit.max(1)),
            Self::SecurityAssignmentList(request) => (request.limit, 0, request.limit.max(1)),
            Self::SecurityKeyList(request) => (request.limit, 0, request.limit.max(1)),
            Self::SecurityAuditRead(request) => (request.limit, 0, request.limit.max(1)),
            Self::SecurityPrincipalCreate { display_name } => {
                (1, display_name.len(), display_name.len().max(1))
            }
            Self::SecurityCustomRoleCreate {
                display_name,
                grants,
            } => {
                let bytes = display_name
                    .len()
                    .saturating_add(grants.len().saturating_mul(32));
                (grants.len().max(1), bytes, bytes.max(1))
            }
            Self::SecurityBuiltInAssignmentCreate { .. } => (1, 64, 1),
            Self::SecurityCustomAssignmentCreate { .. } => (1, 32, 1),
            Self::SecurityApiKeyIssueSelfStart {
                label,
                roles,
                custom_roles,
                scope_ceiling,
                ..
            }
            | Self::SecurityApiKeyIssueStart {
                label,
                roles,
                custom_roles,
                scope_ceiling,
                ..
            } => {
                let bytes = label
                    .len()
                    .saturating_add(roles.len())
                    .saturating_add(custom_roles.len().saturating_mul(16))
                    .saturating_add(scope_ceiling.len().saturating_mul(24));
                (1, bytes, bytes.max(1))
            }
            Self::SecurityApiKeyRotateSelfStart { label, .. }
            | Self::SecurityApiKeyRotateStart { label, .. } => {
                (1, label.len().saturating_add(40), label.len().max(1))
            }
            Self::SecurityApiKeyIssueSelfActivate { .. }
            | Self::SecurityApiKeyIssueActivate { .. }
            | Self::SecurityApiKeyRotateSelfActivate { .. }
            | Self::SecurityApiKeyRotateActivate { .. } => (1, 48, 1),
            Self::SecurityApiKeyIssueSelfAbort { .. }
            | Self::SecurityApiKeyIssueAbort { .. }
            | Self::SecurityApiKeyRotateSelfAbort { .. }
            | Self::SecurityApiKeyRotateAbort { .. }
            | Self::SecurityApiKeyRevokeSelf { .. }
            | Self::SecurityApiKeyRevoke { .. } => (1, 16, 1),
            Self::SecurityLegacyBearerRevoke => (1, 0, 1),
            Self::Restore(request) => (
                1,
                request
                    .backup
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_add(request.destination.as_os_str().as_encoded_bytes().len()),
                1,
            ),
            Self::VerifyProof { proof, witness, .. } => {
                let bytes = proof.len().saturating_add(witness.len());
                (1, bytes, bytes)
            }
            Self::Prove { operation, .. } => {
                let (count, bytes, work, _) = operation.request_cost();
                (count, bytes, work)
            }
        }
    }

    fn mutation_response_cost(&self) -> Option<(usize, usize)> {
        match self {
            Self::CatalogCreate { .. }
            | Self::PrepareSql { .. }
            | Self::StructureSet { .. }
            | Self::StructureMutate { .. }
            | Self::TransactionBegin
            | Self::TransactionCommit { .. }
            | Self::TransactionRollback { .. }
            | Self::SearchIngest { .. }
            | Self::SearchDocumentUpdate { .. }
            | Self::SearchDocumentDelete { .. }
            | Self::AdminCheckpoint
            | Self::SecurityPrincipalCreate { .. }
            | Self::SecurityPrincipalSetEnabled { .. }
            | Self::SecurityCustomRoleCreate { .. }
            | Self::SecurityBuiltInAssignmentCreate { .. }
            | Self::SecurityCustomAssignmentCreate { .. }
            | Self::SecurityAssignmentRevoke { .. }
            | Self::SecurityApiKeyIssueSelfActivate { .. }
            | Self::SecurityApiKeyIssueActivate { .. }
            | Self::SecurityApiKeyRotateSelfActivate { .. }
            | Self::SecurityApiKeyRotateActivate { .. }
            | Self::SecurityApiKeyIssueSelfAbort { .. }
            | Self::SecurityApiKeyIssueAbort { .. }
            | Self::SecurityApiKeyRotateSelfAbort { .. }
            | Self::SecurityApiKeyRotateAbort { .. }
            | Self::SecurityApiKeyRevokeSelf { .. }
            | Self::SecurityApiKeyRevoke { .. }
            | Self::SecurityLegacyBearerRevoke => Some((1, 256)),
            Self::SecurityApiKeyIssueSelfStart { .. }
            | Self::SecurityApiKeyIssueStart { .. }
            | Self::SecurityApiKeyRotateSelfStart { .. }
            | Self::SecurityApiKeyRotateStart { .. } => {
                Some((1, ApiKeyStartReceipt::wire_size_bound()))
            }
            Self::ExecuteSql { .. } => self.is_mutating().then_some((1, 256)),
            Self::Backup(request) => Some((
                1,
                request
                    .destination
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_add(128),
            )),
            Self::Restore(request) => Some((1, restore_success_response_bytes(request))),
            _ => None,
        }
    }
}

fn complex_operation_cost(operation: &ProductOperation) -> (usize, usize, usize) {
    match operation {
        ProductOperation::StructureMutate { mutations } => {
            let bytes = mutations.iter().fold(0_usize, |total, mutation| {
                total.saturating_add(format!("{mutation:?}").len())
            });
            (mutations.len(), bytes, mutations.len())
        }
        ProductOperation::StructureRead(request) => {
            let bytes = format!("{request:?}").len();
            (1, bytes, bytes.max(1))
        }
        ProductOperation::TransactionStageSql { mutation, .. } => (
            mutation.parameters.len().max(1),
            mutation
                .statement
                .len()
                .saturating_add(values_bytes(&mutation.parameters)),
            mutation
                .statement
                .len()
                .saturating_add(mutation.parameters.len()),
        ),
        ProductOperation::TransactionStageStructure { mutation, .. } => debug_cost(mutation),
        ProductOperation::TransactionStageSearch { mutation, .. } => debug_cost(mutation),
        ProductOperation::TransactionStageVector { mutation, .. } => debug_cost(mutation),
        _ => (usize::MAX, usize::MAX, usize::MAX),
    }
}

fn debug_cost(value: &impl std::fmt::Debug) -> (usize, usize, usize) {
    let bytes = format!("{value:?}").len();
    (1, bytes, bytes.max(1))
}

impl ProductResponse {
    fn cost(&self) -> (usize, usize) {
        match self {
            Self::Capabilities(_) => (1, 64),
            Self::CatalogObject(read) => (1, catalog_object_bytes(&read.value)),
            Self::CatalogPage(page) => (page.items.len(), page.returned_bytes),
            Self::CatalogVisiblePage(page) => (
                page.items.len(),
                page.items
                    .iter()
                    .map(|item| qualified_name_bytes(&item.name).saturating_add(40))
                    .sum::<usize>()
                    .saturating_add(
                        page.cursor
                            .as_ref()
                            .map_or(0, crate::CatalogVisibleCursor::encoded_len),
                    ),
            ),
            Self::CatalogDependencyPage(page) => (page.items.len(), page.returned_bytes),
            Self::CatalogDefinition(None) => (0, 0),
            Self::CatalogDefinition(Some(object)) => (1, qualified_name_bytes(object.name())),
            Self::PreparedSql { .. }
            | Self::Deallocated
            | Self::CatalogCreated(_)
            | Self::StructureSet(_)
            | Self::StructureMutated(_)
            | Self::StructureTtl(_)
            | Self::TransactionStatus(_)
            | Self::ExplicitTransactionStatus(_)
            | Self::TransactionCommitted(_)
            | Self::TransactionRolledBack(_)
            | Self::AdminCheckpoint(_)
            | Self::SearchIngested(_)
            | Self::ProofVerification(_)
            | Self::SecurityPrincipalMutated(_)
            | Self::SecurityCustomRoleMutated(_)
            | Self::SecurityAssignmentMutated(_)
            | Self::SecurityMutated(_)
            | Self::SecurityApiKeyStarted(_)
            | Self::SecurityApiKeyActivated(_) => (1, 256),
            Self::TransactionStaged(receipt) => (1, transaction_stage_response_bytes(receipt)),
            Self::Explain(explanation) => (1, format!("{explanation:?}").len()),
            Self::Sql { result, .. } => sql_result_cost(result),
            Self::StructureValue(value) => (
                usize::from(value.is_some()),
                value.as_ref().map_or(0, Vec::len),
            ),
            Self::StructureRead(read) => (1, format!("{:?}", read.value).len()),
            Self::Search(result) => (
                result.hits.len(),
                result
                    .hits
                    .iter()
                    .map(|hit| hit.document_id.len().saturating_add(8))
                    .sum(),
            ),
            Self::IntegratedSearch(result) => (result.hits.len(), format!("{result:?}").len()),
            Self::AdminStatus(_) | Self::Doctor(_) => (1, 512),
            Self::SecurityStatus(_) => (1, AccessControlStatus::encoded_size_bound()),
            Self::Backup(info) => (1, info.path.as_os_str().as_encoded_bytes().len() + 128),
            Self::SecurityPrincipalPage(page) => (page.items.len(), page.encoded_size_bound()),
            Self::SecurityRolePage(page) => (page.items.len(), page.encoded_size_bound()),
            Self::SecurityAssignmentPage(page) => (page.items.len(), page.encoded_size_bound()),
            Self::SecurityKeyPage(page) => (page.items.len(), page.encoded_size_bound()),
            Self::SecurityAuditPage(page) => (page.events.len(), page.encoded_size_bound()),
            Self::Restore(info) => (1, restore_response_bytes(info)),
            Self::Telemetry(snapshot) => (
                snapshot.metrics.len().saturating_add(snapshot.events.len()),
                snapshot
                    .metrics
                    .len()
                    .saturating_mul(128)
                    .saturating_add(snapshot.events.len().saturating_mul(24)),
            ),
            Self::Proven { response, artifact } => {
                let (count, bytes) = response.cost();
                (
                    count,
                    bytes
                        .saturating_add(artifact.proof_bytes.len())
                        .saturating_add(artifact.witness_bytes.len()),
                )
            }
        }
    }
}

fn authorization<const N: usize>(permissions: [ProductPermission; N]) -> ProductAuthorization {
    ProductAuthorization::from_permissions(permissions)
}

fn sql_statement_class(statement: &str) -> Result<SqlStatementClass, ProductError> {
    classify_sql_statement(statement).map_err(ProductError::from)
}

fn sql_command_can_be_noop(class: SqlStatementClass, statement: &str) -> bool {
    if class != SqlStatementClass::DataMutation {
        return false;
    }
    let first = statement
        .trim_start()
        .split(|character: char| character.is_ascii_whitespace() || character == '(')
        .next()
        .unwrap_or_default();
    first.eq_ignore_ascii_case("update") || first.eq_ignore_ascii_case("delete")
}

fn search_limits(limits: ProductLimits, requested_hits: usize) -> BoundedSearchLimits {
    debug_assert!(requested_hits <= limits.max_count);
    BoundedSearchLimits {
        max_hits: limits.max_count,
        max_documents: limits.max_work_units,
        max_matches: limits.max_count,
        max_source_bytes: limits.max_memory_bytes,
        max_token_visits: limits.max_work_units,
        max_token_comparisons: limits.max_work_units,
        max_fuzzy_steps: limits.max_work_units,
        max_clauses: limits.max_count,
        max_query_bytes: limits.max_request_bytes,
    }
}

fn map_search_error(error: &BoundedSearchError, context: &ProductRequestContext) -> ProductError {
    match error {
        BoundedSearchError::ExecutionInterrupted => context
            .checkpoint()
            .err()
            .unwrap_or_else(|| context.error(ProductErrorCode::Cancelled)),
        BoundedSearchError::UnknownIndex => {
            ProductError::from_code(ProductErrorCode::ObjectNotFound)
        }
        BoundedSearchError::ClauseBudgetExceeded { .. }
        | BoundedSearchError::QueryByteBudgetExceeded { .. }
        | BoundedSearchError::DocumentBudgetExceeded { .. }
        | BoundedSearchError::MatchBudgetExceeded { .. }
        | BoundedSearchError::SourceByteBudgetExceeded { .. }
        | BoundedSearchError::TokenVisitBudgetExceeded { .. }
        | BoundedSearchError::ComparisonBudgetExceeded { .. }
        | BoundedSearchError::FuzzyStepBudgetExceeded { .. } => {
            ProductError::from_code(ProductErrorCode::LimitExceeded)
        }
        _ => ProductError::from_code(ProductErrorCode::InvalidRequest),
    }
}

fn map_execution_interruption(
    error: ProductError,
    context: &ProductRequestContext,
) -> ProductError {
    if error.code() == ProductErrorCode::Cancelled {
        context.checkpoint().err().unwrap_or(error)
    } else {
        error
    }
}

fn map_backup_error(error: BackupProductError, context: &ProductRequestContext) -> ProductError {
    match error {
        BackupProductError::Cancelled if context.cancellation.is_cancelled() => {
            context.error(ProductErrorCode::Cancelled)
        }
        BackupProductError::Cancelled => context.error(ProductErrorCode::DeadlineExceeded),
        BackupProductError::InvalidRequest(_) => {
            ProductError::from_code(ProductErrorCode::InvalidRequest)
        }
        BackupProductError::Backup { error, .. }
        | BackupProductError::Verification { error, .. }
        | BackupProductError::Restore { error, .. } => *error,
        BackupProductError::DoctorAfterRestore { .. } => {
            ProductError::from_code(ProductErrorCode::BackupInvalid)
        }
    }
}

fn map_proof_error(error: &crate::proof::NativeProofError) -> ProductError {
    match error {
        crate::proof::NativeProofError::Interrupted => {
            ProductError::from_code(ProductErrorCode::Cancelled)
        }
        crate::proof::NativeProofError::LimitExceeded { .. }
        | crate::proof::NativeProofError::LengthOverflow => {
            ProductError::from_code(ProductErrorCode::LimitExceeded)
        }
        crate::proof::NativeProofError::Io { .. } => ProductError::from_code(ProductErrorCode::Io),
        _ => ProductError::from_code(ProductErrorCode::Corruption),
    }
}

fn values_bytes(values: &[ProductValue]) -> usize {
    values
        .iter()
        .fold(0, |total, value| total.saturating_add(value_bytes(value)))
}

fn value_bytes(value: &ProductValue) -> usize {
    value_bytes_at(value, 0)
}

fn value_bytes_at(value: &ProductValue, depth: usize) -> usize {
    if depth > 8 {
        return usize::MAX;
    }
    match value {
        ProductValue::Null => 0,
        ProductValue::Boolean(_) => 1,
        ProductValue::Signed(_)
        | ProductValue::Unsigned(_)
        | ProductValue::Float64(_)
        | ProductValue::Time(_)
        | ProductValue::Timestamp(_) => 8,
        ProductValue::Decimal(_) | ProductValue::Interval { .. } | ProductValue::Uuid(_) => 16,
        ProductValue::Float32(_) | ProductValue::Date(_) => 4,
        ProductValue::Text(value) | ProductValue::Json(value) => value.len(),
        ProductValue::Binary(value) => value.len(),
        ProductValue::Array(values) => values.iter().fold(0, |total, value| {
            total.saturating_add(value_bytes_at(value, depth + 1))
        }),
        ProductValue::Map(entries) => entries.iter().fold(0, |total, (key, value)| {
            total
                .saturating_add(value_bytes_at(key, depth + 1))
                .saturating_add(value_bytes_at(value, depth + 1))
        }),
        ProductValue::Vector(values) => values.len().saturating_mul(4),
        _ => usize::MAX,
    }
}

fn sql_result_cost(result: &ProductSqlResult) -> (usize, usize) {
    match result {
        ProductSqlResult::Command { .. } => (1, 32),
        ProductSqlResult::Rows { columns, rows } => {
            let bytes = columns
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(
                    rows.iter()
                        .flat_map(|row| row.iter())
                        .map(value_bytes)
                        .sum(),
                );
            (rows.len(), bytes)
        }
    }
}

fn search_query_bytes(query: &BoundedSearchQuery) -> usize {
    search_query_bytes_at(query, 0)
}

fn search_query_bytes_at(query: &BoundedSearchQuery, depth: usize) -> usize {
    if depth > hyphae_native_runtime::MAX_BOUNDED_SEARCH_DEPTH {
        return usize::MAX;
    }
    match query {
        BoundedSearchQuery::Term(value)
        | BoundedSearchQuery::Phrase(value)
        | BoundedSearchQuery::Prefix(value) => value.len(),
        BoundedSearchQuery::Fuzzy { term, .. } => term.len(),
        BoundedSearchQuery::Boolean {
            must,
            should,
            must_not,
        } => must
            .iter()
            .chain(should)
            .chain(must_not)
            .fold(0, |total, query| {
                total.saturating_add(search_query_bytes_at(query, depth + 1))
            }),
    }
}

fn qualified_name_bytes(name: &QualifiedName) -> usize {
    format!("{name:?}").len()
}

fn catalog_object_bytes(object: &CatalogObject) -> usize {
    format!("{object:?}").len()
}

fn unix_time_micros() -> i64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_micros());
    i64::try_from(micros).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod security_response_cost_tests {
    use super::*;

    #[test]
    fn security_response_cost_uses_canonical_encoded_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let epoch = AuthorizationEpoch::INITIAL;
        let principals = SecurityPrincipalPage::try_from_wire(epoch, Vec::new(), None)?;
        let roles = SecurityRolePage::try_from_wire(epoch, Vec::new(), None)?;
        let assignments = SecurityAssignmentPage::try_from_wire(epoch, Vec::new(), None)?;
        let keys = SecurityKeyPage::try_from_wire(epoch, Vec::new(), None)?;
        let audit = SecurityAuditPage::try_from_wire(Vec::new(), None)?;
        let status = AccessControlStatus {
            bootstrapped: false,
            epoch: AuthorizationEpoch::UNMANAGED,
            principals: 0,
            assignments: 0,
            custom_roles: 0,
            custom_assignments: 0,
            keys: 0,
            pending_keys: 0,
            audit_events: 0,
        };

        assert_eq!(ProductResponse::SecurityStatus(status).cost(), (1, 88));
        assert_eq!(
            ProductResponse::SecurityPrincipalPage(principals).cost(),
            (0, 72)
        );
        assert_eq!(ProductResponse::SecurityRolePage(roles).cost(), (0, 72));
        assert_eq!(
            ProductResponse::SecurityAssignmentPage(assignments).cost(),
            (0, 72)
        );
        assert_eq!(ProductResponse::SecurityKeyPage(keys).cost(), (0, 72));
        assert_eq!(ProductResponse::SecurityAuditPage(audit).cost(), (0, 48));
        Ok(())
    }

    #[test]
    #[allow(clippy::panic)]
    fn api_key_lifecycle_is_always_rejected_from_prove() {
        let Some(key_id) = ApiKeyId::from_bytes([1; 16]) else {
            panic!("nonzero key id");
        };
        let Some(principal_id) = SecurityId::new(1) else {
            panic!("nonzero principal");
        };
        assert!(ProductOperation::SecurityApiKeyRevoke { key_id }.is_key_lifecycle());
        assert!(
            ProductOperation::SecurityApiKeyIssueSelfStart {
                principal_id,
                label: "pending".to_owned(),
                roles: vec![BuiltInRole::Owner],
                custom_roles: Vec::new(),
                permission_ceiling: ProductAuthorization::ALL,
                scope_ceiling: vec![ProductScope::Instance],
                expires_at_micros: None,
            }
            .is_key_lifecycle()
        );
    }
}
