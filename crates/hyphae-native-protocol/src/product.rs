// SPDX-License-Identifier: Apache-2.0

use hyphae_native_catalog::{
    CatalogName, CatalogObject, CatalogObjectKind, DependencyDirection, DependencyEdge,
    DependencyKind, LogicalCatalogObject, QualifiedName,
};
use hyphae_native_product::proof::{
    AdmittedProofLimits, ExternalTrustedAnchor, NativeOperationProofArtifact,
    NativeProofGenerationLimits, ProofCodecLimits, WitnessCodecLimits, decode_native_proof,
};
use hyphae_native_product::{
    AccessControlLimits, AccessControlMutationReceipt, AccessControlStatus, AdminStatus,
    ApiKeyActivationReceipt, ApiKeyConfirmationDigest, ApiKeyId, ApiKeySecretDelivery,
    ApiKeyStartReceipt, AuthorizationEpoch, BackupInfo, BackupLimits, BackupRequest,
    BoundedSearchQuery, BuiltInRole, CatalogCursor, CatalogDependencyRequest, CatalogListRequest,
    CatalogObjectSummary, CatalogPage, CatalogVersion, CatalogVisibleCursor,
    CatalogVisibleListFilter, CatalogVisibleListRequest, CatalogVisiblePage, CustomRoleGrant,
    CustomRoleMutationReceipt, DoctorRecovery, DoctorReport, DoctorRequest, DoctorStatus,
    MAX_SECURITY_LIST_ROWS, MetricKind, MetricValue, ObjectId, ProductAnnExplanation,
    ProductAnnRecallRisk, ProductAnnStrategy, ProductAuthorization, ProductCheckpointReceipt,
    ProductCommitOutcome, ProductCommitReceipt, ProductConvergenceExplanation,
    ProductConvergenceStrategy, ProductDocValue, ProductDocument, ProductDurability,
    ProductDurabilityPolicy, ProductError, ProductErrorCodecError, ProductExplain,
    ProductExplicitCommitReceipt, ProductExplicitTransactionStatus, ProductHashEntry,
    ProductHashScanStop, ProductHybridExplanation, ProductHybridVectorStrategy,
    ProductIntegratedSearchHit, ProductLexicalBranch, ProductLimits, ProductListSide,
    ProductMissingPlacement, ProductNamedAggregation, ProductNamedAggregationValue,
    ProductOperation, ProductPermission, ProductPhysicalObservation, ProductPreparedHandle,
    ProductRead, ProductResponse, ProductRollbackReceipt, ProductScope, ProductScoreBound,
    ProductSearchDocumentDelete, ProductSearchDocumentUpdate, ProductSearchFilter,
    ProductSearchHit, ProductSearchIngestBatch, ProductSearchIngestReceipt, ProductSearchOperator,
    ProductSearchRequest, ProductSearchResult, ProductSearchResults, ProductSearchSort,
    ProductSetAlgebraOperation, ProductSortDirection, ProductSortSource, ProductSortedSetEntry,
    ProductSortedSetOrder, ProductSqlResult, ProductStreamEntry, ProductStructureKey,
    ProductStructureMutation, ProductStructureMutationResult, ProductStructureReadRequest,
    ProductStructureReadResult, ProductTransactionHandle, ProductTransactionId,
    ProductTransactionSearchMutation, ProductTransactionSqlMutation,
    ProductTransactionStageReceipt, ProductTransactionStageResult, ProductTransactionStatus,
    ProductTransactionVectorMutation, ProductTtl, ProductValue, ProductVectorBranch,
    ProductVectorBranchReceipt, ProductVectorExecution, ProductVectorStrategy, RestoreRequest,
    RoleAssignmentMutationReceipt, SecurityAssignmentListRequest, SecurityAssignmentPage,
    SecurityAssignmentSummary, SecurityAuditAction, SecurityAuditEvent, SecurityAuditMetadata,
    SecurityAuditPage, SecurityAuditReadRequest, SecurityAuditResult, SecurityAuditTarget,
    SecurityCursor, SecurityCursorId, SecurityId, SecurityKeyListRequest, SecurityKeyPage,
    SecurityKeySummary, SecurityKeySummaryInput, SecurityPrincipalListRequest,
    SecurityPrincipalMutationReceipt, SecurityPrincipalPage, SecurityPrincipalSummary,
    SecurityRoleListRequest, SecurityRolePage, SecurityRoleSummary, SnapshotIdentity, SqlPlanText,
    TelemetryEvent, TelemetryEventKind, TelemetrySnapshot, decode_product_error,
    encode_product_error,
};
use hyphae_native_runtime::CatalogPageStop;
use thiserror::Error;

/// Request envelope magic.
pub const PRODUCT_REQUEST_MAGIC: [u8; 8] = *b"HYPREQ01";
/// Response envelope magic.
pub const PRODUCT_RESPONSE_MAGIC: [u8; 8] = *b"HYPRSP01";
/// Maximum complete request or response body.
pub const MAX_PRODUCT_WIRE_BYTES: usize = 16 * 1024 * 1024;

const HEADER_SIZE: usize = 16;
const REQUEST_CAPABILITIES: u16 = 1;
const REQUEST_PREPARE_SQL: u16 = 2;
const REQUEST_EXECUTE_PREPARED: u16 = 3;
const REQUEST_EXECUTE_SQL: u16 = 4;
const REQUEST_STRUCTURE_GET: u16 = 5;
const REQUEST_STRUCTURE_SET: u16 = 6;
const REQUEST_STRUCTURE_TTL: u16 = 7;
const REQUEST_TRANSACTION_STATUS: u16 = 8;
const REQUEST_SEARCH: u16 = 9;
const REQUEST_ADMIN_STATUS: u16 = 10;
const REQUEST_ADMIN_CHECKPOINT: u16 = 11;
const REQUEST_DEALLOCATE_PREPARED: u16 = 12;
const REQUEST_CATALOG_OBJECT: u16 = 13;
const REQUEST_CATALOG_OBJECT_NAMED: u16 = 14;
const REQUEST_CATALOG_LIST: u16 = 15;
const REQUEST_CATALOG_DEPENDENCIES: u16 = 16;
const REQUEST_CATALOG_DESCRIBE: u16 = 17;
const REQUEST_CATALOG_RESOLVE: u16 = 18;
const REQUEST_CATALOG_CREATE: u16 = 19;
const REQUEST_ADMIN_EXPLAIN_SQL: u16 = 20;
const REQUEST_DOCTOR: u16 = 21;
const REQUEST_BACKUP: u16 = 22;
const REQUEST_TELEMETRY: u16 = 23;
const REQUEST_VERIFY_PROOF: u16 = 24;
const REQUEST_SEARCH_COLLECTION: u16 = 25;
const REQUEST_TRANSACTION_STATUS_BY_IDEMPOTENCY: u16 = 39;
const REQUEST_STRUCTURE_MUTATE: u16 = 26;
const REQUEST_STRUCTURE_READ: u16 = 27;
const REQUEST_RESTORE: u16 = 28;
const REQUEST_SEARCH_INGEST: u16 = 29;
const REQUEST_SEARCH_DOCUMENT_UPDATE: u16 = 30;
const REQUEST_SEARCH_DOCUMENT_DELETE: u16 = 31;
const REQUEST_TRANSACTION_BEGIN: u16 = 32;
const REQUEST_TRANSACTION_STAGE_SQL: u16 = 33;
const REQUEST_TRANSACTION_STAGE_STRUCTURE: u16 = 34;
const REQUEST_TRANSACTION_STAGE_SEARCH: u16 = 35;
const REQUEST_TRANSACTION_STAGE_VECTOR: u16 = 36;
const REQUEST_TRANSACTION_COMMIT: u16 = 37;
const REQUEST_TRANSACTION_ROLLBACK: u16 = 38;
const REQUEST_EXPLICIT_TRANSACTION_STATUS: u16 = 40;
const REQUEST_PROVE: u16 = 41;
const REQUEST_SECURITY_STATUS: u16 = 42;
const REQUEST_SECURITY_PRINCIPAL_LIST: u16 = 43;
const REQUEST_SECURITY_ROLE_LIST: u16 = 44;
const REQUEST_SECURITY_ASSIGNMENT_LIST: u16 = 45;
const REQUEST_SECURITY_KEY_LIST: u16 = 46;
const REQUEST_SECURITY_AUDIT_READ: u16 = 47;
const REQUEST_SECURITY_PRINCIPAL_CREATE: u16 = 48;
const REQUEST_SECURITY_PRINCIPAL_SET_ENABLED: u16 = 49;
const REQUEST_SECURITY_CUSTOM_ROLE_CREATE: u16 = 50;
const REQUEST_SECURITY_BUILT_IN_ASSIGNMENT_CREATE: u16 = 51;
const REQUEST_SECURITY_CUSTOM_ASSIGNMENT_CREATE: u16 = 52;
const REQUEST_SECURITY_ASSIGNMENT_REVOKE: u16 = 53;
const REQUEST_CATALOG_VISIBLE_LIST: u16 = 54;
const REQUEST_SECURITY_API_KEY_ISSUE_SELF_START: u16 = 55;
const REQUEST_SECURITY_API_KEY_ISSUE_START: u16 = 56;
const REQUEST_SECURITY_API_KEY_ISSUE_SELF_ACTIVATE: u16 = 57;
const REQUEST_SECURITY_API_KEY_ISSUE_ACTIVATE: u16 = 58;
const REQUEST_SECURITY_API_KEY_ROTATE_SELF_START: u16 = 59;
const REQUEST_SECURITY_API_KEY_ROTATE_START: u16 = 60;
const REQUEST_SECURITY_API_KEY_ROTATE_SELF_ACTIVATE: u16 = 61;
const REQUEST_SECURITY_API_KEY_ROTATE_ACTIVATE: u16 = 62;
const REQUEST_SECURITY_API_KEY_ISSUE_SELF_ABORT: u16 = 63;
const REQUEST_SECURITY_API_KEY_ISSUE_ABORT: u16 = 64;
const REQUEST_SECURITY_API_KEY_ROTATE_SELF_ABORT: u16 = 65;
const REQUEST_SECURITY_API_KEY_ROTATE_ABORT: u16 = 66;
const REQUEST_SECURITY_API_KEY_REVOKE_SELF: u16 = 67;
const REQUEST_SECURITY_API_KEY_REVOKE: u16 = 68;
const REQUEST_SECURITY_LEGACY_BEARER_REVOKE: u16 = 70;

const RESPONSE_CAPABILITIES: u16 = 1;
const RESPONSE_PREPARED_SQL: u16 = 2;
const RESPONSE_SQL: u16 = 3;
const RESPONSE_STRUCTURE_VALUE: u16 = 4;
const RESPONSE_STRUCTURE_SET: u16 = 5;
const RESPONSE_STRUCTURE_TTL: u16 = 6;
const RESPONSE_TRANSACTION_STATUS: u16 = 7;
const RESPONSE_SEARCH: u16 = 8;
const RESPONSE_ADMIN_STATUS: u16 = 9;
const RESPONSE_ADMIN_CHECKPOINT: u16 = 10;
const RESPONSE_DEALLOCATED: u16 = 11;
const RESPONSE_CATALOG_OBJECT: u16 = 12;
const RESPONSE_CATALOG_PAGE: u16 = 13;
const RESPONSE_CATALOG_DEPENDENCY_PAGE: u16 = 14;
const RESPONSE_CATALOG_DEFINITION: u16 = 15;
const RESPONSE_CATALOG_CREATED: u16 = 16;
const RESPONSE_EXPLAIN: u16 = 17;
const RESPONSE_DOCTOR: u16 = 18;
const RESPONSE_BACKUP: u16 = 19;
const RESPONSE_TELEMETRY: u16 = 20;
const RESPONSE_PROOF_VERIFICATION: u16 = 21;
const RESPONSE_INTEGRATED_SEARCH: u16 = 22;
const RESPONSE_STRUCTURE_MUTATED: u16 = 23;
const RESPONSE_STRUCTURE_READ: u16 = 24;
const RESPONSE_RESTORE: u16 = 25;
const RESPONSE_SEARCH_INGESTED: u16 = 26;
const RESPONSE_EXPLICIT_TRANSACTION_STATUS: u16 = 27;
const RESPONSE_TRANSACTION_STAGED: u16 = 28;
const RESPONSE_TRANSACTION_COMMITTED: u16 = 29;
const RESPONSE_TRANSACTION_ROLLED_BACK: u16 = 30;
const RESPONSE_PROVEN: u16 = 31;
const RESPONSE_SECURITY_STATUS: u16 = 32;
const RESPONSE_SECURITY_PRINCIPAL_PAGE: u16 = 33;
const RESPONSE_SECURITY_ROLE_PAGE: u16 = 34;
const RESPONSE_SECURITY_ASSIGNMENT_PAGE: u16 = 35;
const RESPONSE_SECURITY_KEY_PAGE: u16 = 36;
const RESPONSE_SECURITY_AUDIT_PAGE: u16 = 37;
const RESPONSE_SECURITY_PRINCIPAL_MUTATED: u16 = 38;
const RESPONSE_SECURITY_CUSTOM_ROLE_MUTATED: u16 = 39;
const RESPONSE_SECURITY_ASSIGNMENT_MUTATED: u16 = 40;
const RESPONSE_SECURITY_MUTATED: u16 = 41;
const RESPONSE_CATALOG_VISIBLE_PAGE: u16 = 42;
const RESPONSE_SECURITY_API_KEY_STARTED: u16 = 43;
const RESPONSE_SECURITY_API_KEY_ACTIVATED: u16 = 44;

/// Decoded product request and execution metadata.
#[derive(Clone, Debug)]
pub struct WireRequest {
    /// Transport-independent product operation.
    pub operation: ProductOperation,
    /// Logical time selected for snapshot and TTL behavior.
    pub logical_time_micros: i64,
    /// Absolute Unix-time deadline in microseconds, absent when not requested.
    pub deadline_micros: Option<i64>,
    /// Optional explicit mutation idempotency token.
    pub idempotency_token: Option<u128>,
    /// Central request and response limits.
    pub limits: ProductLimits,
    /// Mutation durability policy.
    pub durability: ProductDurabilityPolicy,
}

/// Product wire codec failure.
#[derive(Debug, Error)]
pub enum ProductCodecError {
    /// Input ends before a declared field.
    #[error("native product wire payload is truncated")]
    Truncated,
    /// Magic, exact length, reserved fields, or discriminants are invalid.
    #[error("native product wire payload is malformed")]
    Malformed,
    /// Payload exceeds a fixed product protocol bound.
    #[error("native product wire payload exceeds a bound")]
    LimitExceeded,
    /// UTF-8 or nested canonical product data is invalid.
    #[error("native product wire payload contains an invalid value")]
    InvalidValue,
    /// This operation or response is not in the current portable subset.
    #[error("native product wire operation is unsupported")]
    Unsupported,
    /// Canonical `HYPERR01` codec failed.
    #[error(transparent)]
    ErrorEnvelope(#[from] ProductErrorCodecError),
}

/// Encodes one request and its complete execution envelope.
pub fn encode_product_request(request: &WireRequest) -> Result<Vec<u8>, ProductCodecError> {
    if request.idempotency_token == Some(0)
        || (operation_requires_idempotency(&request.operation)
            && request.idempotency_token.is_none())
    {
        return Err(ProductCodecError::InvalidValue);
    }
    if operation_is_key_lifecycle(&request.operation)
        && request.durability != ProductDurabilityPolicy::STRICT
    {
        return Err(ProductCodecError::InvalidValue);
    }
    request
        .limits
        .validate()
        .map_err(|_| ProductCodecError::LimitExceeded)?;
    let (kind, body) = encode_operation(&request.operation)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&request.logical_time_micros.to_le_bytes());
    payload.extend_from_slice(&request.deadline_micros.unwrap_or(0).to_le_bytes());
    let extended = request.idempotency_token.is_some();
    if let Some(token) = request.idempotency_token {
        payload.extend_from_slice(&token.to_le_bytes());
    }
    put_u64(&mut payload, request.limits.max_count)?;
    put_u64(&mut payload, request.limits.max_request_bytes)?;
    put_u64(&mut payload, request.limits.max_response_bytes)?;
    put_u64(&mut payload, request.limits.max_work_units)?;
    put_u64(&mut payload, request.limits.max_memory_bytes)?;
    payload.push(durability_tag(request.durability.durability));
    payload.extend_from_slice(if extended {
        &[1, 0, 0, 0, 0, 0, 0]
    } else {
        &[0; 7]
    });
    payload.extend_from_slice(&body);
    envelope(PRODUCT_REQUEST_MAGIC, kind, &payload)
}

/// Encodes one request only when every operation is available in the
/// negotiated protocol minor.
pub fn encode_product_request_for_minor(
    request: &WireRequest,
    negotiated_minor: u16,
) -> Result<Vec<u8>, ProductCodecError> {
    ensure_operation_minor(&request.operation, negotiated_minor)?;
    encode_product_request(request)
}

/// Decodes one exact request and execution envelope.
pub fn decode_product_request(encoded: &[u8]) -> Result<WireRequest, ProductCodecError> {
    let (kind, payload) = decode_envelope(encoded, PRODUCT_REQUEST_MAGIC)?;
    if payload.len() < 64 {
        return Err(ProductCodecError::Malformed);
    }
    let deadline = read_i64(&payload[8..16]);
    let legacy_reserved = payload[57..64] == [0; 7];
    let extended = payload.len() >= 80 && payload[73..80] == [1, 0, 0, 0, 0, 0, 0];
    let (operation, idempotency_token, limits_offset, durability_offset) = if extended {
        match decode_operation(kind, &payload[80..]) {
            Ok(operation) => (
                operation,
                Some(
                    ProductTransactionId::new(read_u128(&payload[16..32]))
                        .ok_or(ProductCodecError::InvalidValue)?
                        .get(),
                ),
                32,
                72,
            ),
            Err(_) if legacy_reserved => (decode_operation(kind, &payload[64..])?, None, 16, 56),
            Err(error) => return Err(error),
        }
    } else if legacy_reserved {
        (decode_operation(kind, &payload[64..])?, None, 16, 56)
    } else {
        return Err(ProductCodecError::Malformed);
    };
    let request = WireRequest {
        operation,
        logical_time_micros: read_i64(&payload[..8]),
        deadline_micros: (deadline != 0).then_some(deadline),
        idempotency_token,
        limits: ProductLimits {
            max_count: read_usize(&payload[limits_offset..limits_offset + 8])?,
            max_request_bytes: read_usize(&payload[limits_offset + 8..limits_offset + 16])?,
            max_response_bytes: read_usize(&payload[limits_offset + 16..limits_offset + 24])?,
            max_work_units: read_usize(&payload[limits_offset + 24..limits_offset + 32])?,
            max_memory_bytes: read_usize(&payload[limits_offset + 32..limits_offset + 40])?,
        },
        durability: ProductDurabilityPolicy {
            durability: decode_durability(payload[durability_offset])?,
        },
    };
    request
        .limits
        .validate()
        .map_err(|_| ProductCodecError::LimitExceeded)?;
    if request
        .deadline_micros
        .is_some_and(|deadline| deadline <= 0)
    {
        return Err(ProductCodecError::InvalidValue);
    }
    if operation_requires_idempotency(&request.operation) && request.idempotency_token.is_none() {
        return Err(ProductCodecError::InvalidValue);
    }
    if operation_is_key_lifecycle(&request.operation)
        && request.durability != ProductDurabilityPolicy::STRICT
    {
        return Err(ProductCodecError::InvalidValue);
    }
    Ok(request)
}

/// Decodes one request while rejecting operations introduced after the
/// negotiated protocol minor.
pub fn decode_product_request_for_minor(
    encoded: &[u8],
    negotiated_minor: u16,
) -> Result<WireRequest, ProductCodecError> {
    let request = decode_product_request(encoded)?;
    ensure_operation_minor(&request.operation, negotiated_minor)?;
    Ok(request)
}

/// Encodes one transport-independent product response.
#[allow(clippy::too_many_lines)]
pub fn encode_product_response(response: &ProductResponse) -> Result<Vec<u8>, ProductCodecError> {
    let (kind, body) = match response {
        ProductResponse::Capabilities(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(&value.product_api_version.to_le_bytes());
            body.extend_from_slice(&value.native_directory_format.to_le_bytes());
            body.extend_from_slice(&value.logical_catalog_codec_version.to_le_bytes());
            body.extend_from_slice(&value.catalog_tree_format_version.to_le_bytes());
            put_u64(&mut body, value.max_catalog_items)?;
            put_u64(&mut body, value.max_catalog_visits)?;
            put_u64(&mut body, value.max_catalog_bytes)?;
            put_u64(&mut body, value.max_sql_statement_bytes)?;
            put_u64(&mut body, value.max_sql_parameters)?;
            put_u64(&mut body, value.max_sql_rows)?;
            (RESPONSE_CAPABILITIES, body)
        }
        ProductResponse::PreparedSql {
            handle,
            catalog_version,
            parameter_count,
            maximum_result_rows,
        } => {
            let mut body = Vec::new();
            body.extend_from_slice(&handle.get().to_le_bytes());
            body.extend_from_slice(&catalog_version.get().to_le_bytes());
            put_u64(&mut body, *parameter_count)?;
            put_u64(&mut body, *maximum_result_rows)?;
            (RESPONSE_PREPARED_SQL, body)
        }
        ProductResponse::Deallocated => (RESPONSE_DEALLOCATED, Vec::new()),
        ProductResponse::CatalogObject(value) => {
            let mut body = Vec::new();
            encode_snapshot(&mut body, value.snapshot);
            put_bytes(
                &mut body,
                &value
                    .value
                    .encode_definition()
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            )?;
            (RESPONSE_CATALOG_OBJECT, body)
        }
        ProductResponse::CatalogPage(value) => {
            let mut body = Vec::new();
            encode_catalog_page(&mut body, value)?;
            (RESPONSE_CATALOG_PAGE, body)
        }
        ProductResponse::CatalogVisiblePage(value) => {
            let mut body = Vec::new();
            encode_catalog_visible_page(&mut body, value)?;
            (RESPONSE_CATALOG_VISIBLE_PAGE, body)
        }
        ProductResponse::CatalogDependencyPage(value) => {
            let mut body = Vec::new();
            encode_dependency_page(&mut body, value)?;
            (RESPONSE_CATALOG_DEPENDENCY_PAGE, body)
        }
        ProductResponse::CatalogDefinition(value) => {
            let mut body = Vec::new();
            body.push(u8::from(value.is_some()));
            body.extend_from_slice(&[0; 3]);
            if let Some(value) = value {
                put_bytes(
                    &mut body,
                    &value
                        .encode_definition_v2()
                        .map_err(|_| ProductCodecError::InvalidValue)?,
                )?;
            }
            (RESPONSE_CATALOG_DEFINITION, body)
        }
        ProductResponse::CatalogCreated(value) => {
            let mut body = Vec::new();
            encode_commit_outcome(&mut body, *value)?;
            (RESPONSE_CATALOG_CREATED, body)
        }
        ProductResponse::Sql {
            result,
            snapshot,
            commit,
        } => {
            let mut body = Vec::new();
            body.push(u8::from(snapshot.is_some()) | (u8::from(commit.is_some()) << 1));
            body.extend_from_slice(&[0; 7]);
            if let Some(snapshot) = snapshot {
                encode_snapshot(&mut body, *snapshot);
            }
            if let Some(commit) = commit {
                encode_commit_outcome(&mut body, *commit)?;
            }
            encode_sql_result(&mut body, result)?;
            (RESPONSE_SQL, body)
        }
        ProductResponse::StructureValue(value) => {
            let mut body = Vec::new();
            body.push(u8::from(value.is_some()));
            body.extend_from_slice(&[0; 3]);
            if let Some(value) = value {
                put_bytes(&mut body, value)?;
            }
            (RESPONSE_STRUCTURE_VALUE, body)
        }
        ProductResponse::StructureSet(outcome) => {
            let mut body = Vec::new();
            encode_commit_outcome(&mut body, *outcome)?;
            (RESPONSE_STRUCTURE_SET, body)
        }
        ProductResponse::StructureTtl(value) => {
            let mut body = Vec::new();
            match value {
                ProductTtl::Missing => body.push(0),
                ProductTtl::Persistent => body.push(1),
                ProductTtl::RemainingMicros(remaining) => {
                    body.push(2);
                    body.extend_from_slice(&remaining.to_le_bytes());
                }
            }
            (RESPONSE_STRUCTURE_TTL, body)
        }
        ProductResponse::TransactionStatus(value) => {
            let mut body = Vec::new();
            encode_transaction_status(&mut body, *value)?;
            (RESPONSE_TRANSACTION_STATUS, body)
        }
        ProductResponse::Search(value) => {
            let mut body = Vec::new();
            put_u32(&mut body, value.hits.len())?;
            body.extend_from_slice(&[0; 4]);
            put_u64(&mut body, value.documents_examined)?;
            put_u64(&mut body, value.source_bytes)?;
            put_u64(&mut body, value.token_visits)?;
            put_u64(&mut body, value.token_comparisons)?;
            put_u64(&mut body, value.fuzzy_steps)?;
            for hit in &value.hits {
                put_bytes(&mut body, &hit.document_id)?;
                body.extend_from_slice(&hit.score.bits().to_le_bytes());
            }
            (RESPONSE_SEARCH, body)
        }
        ProductResponse::AdminStatus(value) => {
            let mut body = Vec::new();
            encode_snapshot(&mut body, value.snapshot);
            put_u64(&mut body, value.snapshot_pin_count)?;
            body.extend_from_slice(&value.physical.page_count.to_le_bytes());
            body.extend_from_slice(&value.physical.physical_page_reads.to_le_bytes());
            body.extend_from_slice(&value.physical.wal_bytes.to_le_bytes());
            body.extend_from_slice(&value.physical.process_full_state_loads.to_le_bytes());
            body.extend_from_slice(&value.physical.process_full_catalog_loads.to_le_bytes());
            body.extend_from_slice(&value.retained_wal_bytes.to_le_bytes());
            put_u64(&mut body, value.replayed_transactions)?;
            put_u64(&mut body, value.manifest_count)?;
            put_u64(&mut body, value.blob_count)?;
            (RESPONSE_ADMIN_STATUS, body)
        }
        ProductResponse::AdminCheckpoint(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(&value.transaction_id.to_le_bytes());
            body.extend_from_slice(&value.visible_csn.to_le_bytes());
            body.extend_from_slice(&value.manifest_generation.to_le_bytes());
            body.extend_from_slice(&value.manifest_digest);
            body.extend_from_slice(&value.checkpoint_lsn.to_le_bytes());
            body.push(u8::from(value.parent_directory_sync_supported));
            (RESPONSE_ADMIN_CHECKPOINT, body)
        }
        ProductResponse::Explain(value) => {
            let mut body = Vec::new();
            encode_explain(&mut body, value)?;
            (RESPONSE_EXPLAIN, body)
        }
        ProductResponse::Doctor(value) => {
            let mut body = Vec::new();
            encode_doctor_report(&mut body, value)?;
            (RESPONSE_DOCTOR, body)
        }
        ProductResponse::Backup(value) => {
            let mut body = Vec::new();
            put_path(&mut body, &value.path)?;
            body.extend_from_slice(&value.visible_csn.to_le_bytes());
            body.extend_from_slice(&value.checkpoint_digest);
            put_u64(&mut body, value.file_count)?;
            body.extend_from_slice(&value.total_bytes.to_le_bytes());
            (RESPONSE_BACKUP, body)
        }
        ProductResponse::Telemetry(value) => {
            let mut body = Vec::new();
            encode_telemetry(&mut body, value)?;
            (RESPONSE_TELEMETRY, body)
        }
        ProductResponse::ProofVerification(value) => {
            let mut body = Vec::new();
            body.push(value.kind as u8);
            body.push(u8::from(value.semantic_reexecution_performed));
            body.extend_from_slice(&[0; 6]);
            body.extend_from_slice(&value.anchor_digest);
            body.extend_from_slice(&value.proof_digest);
            body.extend_from_slice(&value.witness_digest);
            body.extend_from_slice(&value.request_digest);
            body.extend_from_slice(&value.result_digest);
            body.extend_from_slice(&value.evidence_digest);
            put_u64(&mut body, value.file_count)?;
            put_u64(&mut body, value.directory_count)?;
            body.extend_from_slice(&value.total_file_bytes.to_le_bytes());
            (RESPONSE_PROOF_VERIFICATION, body)
        }
        ProductResponse::IntegratedSearch(value) => {
            let mut body = Vec::new();
            encode_integrated_search(&mut body, value)?;
            (RESPONSE_INTEGRATED_SEARCH, body)
        }
        ProductResponse::SearchIngested(value) => {
            let mut body = Vec::new();
            encode_search_ingest_receipt(&mut body, value)?;
            (RESPONSE_SEARCH_INGESTED, body)
        }
        ProductResponse::ExplicitTransactionStatus(value) => {
            let mut body = Vec::new();
            encode_explicit_transaction_status(&mut body, *value)?;
            (RESPONSE_EXPLICIT_TRANSACTION_STATUS, body)
        }
        ProductResponse::TransactionStaged(value) => {
            let mut body = Vec::new();
            encode_transaction_stage_receipt(&mut body, value)?;
            (RESPONSE_TRANSACTION_STAGED, body)
        }
        ProductResponse::TransactionCommitted(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(&value.handle.get().to_le_bytes());
            put_u64(&mut body, value.staged_operations)?;
            encode_receipt(&mut body, value.commit)?;
            (RESPONSE_TRANSACTION_COMMITTED, body)
        }
        ProductResponse::TransactionRolledBack(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(&value.handle.get().to_le_bytes());
            put_u64(&mut body, value.discarded_operations)?;
            (RESPONSE_TRANSACTION_ROLLED_BACK, body)
        }
        ProductResponse::StructureMutated(value) => {
            let mut body = Vec::new();
            encode_commit_outcome(&mut body, *value)?;
            (RESPONSE_STRUCTURE_MUTATED, body)
        }
        ProductResponse::StructureRead(value) => {
            let mut body = Vec::new();
            encode_snapshot(&mut body, value.snapshot);
            encode_structure_read_result(&mut body, &value.value)?;
            (RESPONSE_STRUCTURE_READ, body)
        }
        ProductResponse::Restore(value) => {
            let mut body = Vec::new();
            put_path(&mut body, &value.data_path)?;
            put_path(&mut body, &value.backup.path)?;
            body.extend_from_slice(&value.backup.visible_csn.to_le_bytes());
            body.extend_from_slice(&value.backup.checkpoint_digest);
            put_u64(&mut body, value.backup.file_count)?;
            body.extend_from_slice(&value.backup.total_bytes.to_le_bytes());
            encode_doctor_report(&mut body, &value.doctor)?;
            put_u32(&mut body, value.phases.len())?;
            for phase in &value.phases {
                body.push(restore_phase_tag(*phase));
            }
            (RESPONSE_RESTORE, body)
        }
        ProductResponse::SecurityStatus(value) => {
            let mut body = Vec::new();
            encode_security_status(&mut body, *value)?;
            (RESPONSE_SECURITY_STATUS, body)
        }
        ProductResponse::SecurityPrincipalPage(value) => {
            let mut body = Vec::new();
            encode_security_principal_page(&mut body, value)?;
            (RESPONSE_SECURITY_PRINCIPAL_PAGE, body)
        }
        ProductResponse::SecurityRolePage(value) => {
            let mut body = Vec::new();
            encode_security_role_page(&mut body, value)?;
            (RESPONSE_SECURITY_ROLE_PAGE, body)
        }
        ProductResponse::SecurityAssignmentPage(value) => {
            let mut body = Vec::new();
            encode_security_assignment_page(&mut body, value)?;
            (RESPONSE_SECURITY_ASSIGNMENT_PAGE, body)
        }
        ProductResponse::SecurityKeyPage(value) => {
            let mut body = Vec::new();
            encode_security_key_page(&mut body, value)?;
            (RESPONSE_SECURITY_KEY_PAGE, body)
        }
        ProductResponse::SecurityAuditPage(value) => {
            let mut body = Vec::new();
            encode_security_audit_page(&mut body, value)?;
            (RESPONSE_SECURITY_AUDIT_PAGE, body)
        }
        ProductResponse::SecurityPrincipalMutated(value) => {
            let mut body = Vec::new();
            encode_security_mutation_receipt(
                &mut body,
                value.principal_id,
                value.authorization_epoch,
                value.commit,
            )?;
            (RESPONSE_SECURITY_PRINCIPAL_MUTATED, body)
        }
        ProductResponse::SecurityCustomRoleMutated(value) => {
            let mut body = Vec::new();
            encode_security_mutation_receipt(
                &mut body,
                value.role_id,
                value.authorization_epoch,
                value.commit,
            )?;
            (RESPONSE_SECURITY_CUSTOM_ROLE_MUTATED, body)
        }
        ProductResponse::SecurityAssignmentMutated(value) => {
            let mut body = Vec::new();
            encode_security_mutation_receipt(
                &mut body,
                value.assignment_id,
                value.authorization_epoch,
                value.commit,
            )?;
            (RESPONSE_SECURITY_ASSIGNMENT_MUTATED, body)
        }
        ProductResponse::SecurityMutated(value) => {
            let mut body = Vec::new();
            encode_authorization_epoch(&mut body, value.authorization_epoch)?;
            encode_receipt(&mut body, value.commit)?;
            (RESPONSE_SECURITY_MUTATED, body)
        }
        ProductResponse::SecurityApiKeyStarted(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(value.key_id.as_bytes());
            body.extend_from_slice(&value.principal_id.to_be_bytes());
            encode_optional_api_key_id(&mut body, value.predecessor_key_id);
            encode_authorization_epoch(&mut body, value.authorization_epoch)?;
            encode_receipt(&mut body, value.commit)?;
            let secret = value.secret.take().ok_or(ProductCodecError::Unsupported)?;
            put_bytes(&mut body, secret.expose_secret_bytes())?;
            (RESPONSE_SECURITY_API_KEY_STARTED, body)
        }
        ProductResponse::SecurityApiKeyActivated(value) => {
            let mut body = Vec::new();
            body.extend_from_slice(value.key_id.as_bytes());
            encode_optional_api_key_id(&mut body, value.predecessor_key_id);
            encode_fixed_optional_i64(&mut body, value.overlap_until_micros);
            encode_authorization_epoch(&mut body, value.authorization_epoch)?;
            encode_receipt(&mut body, value.commit)?;
            (RESPONSE_SECURITY_API_KEY_ACTIVATED, body)
        }
        ProductResponse::Proven { response, artifact } => {
            let mut body = Vec::new();
            put_bytes(&mut body, &encode_product_response(response)?)?;
            put_bytes(&mut body, &artifact.proof_bytes)?;
            put_bytes(&mut body, &artifact.witness_bytes)?;
            body.extend_from_slice(&artifact.trusted_anchor.digest());
            (RESPONSE_PROVEN, body)
        }
        _ => return Err(ProductCodecError::Unsupported),
    };
    envelope(PRODUCT_RESPONSE_MAGIC, kind, &body)
}

/// Encodes one response only when its variant is available in the negotiated
/// protocol minor.
pub fn encode_product_response_for_minor(
    response: &ProductResponse,
    negotiated_minor: u16,
) -> Result<Vec<u8>, ProductCodecError> {
    ensure_response_minor(response, negotiated_minor)?;
    encode_product_response(response)
}

/// Decodes one exact transport-independent product response.
#[allow(clippy::too_many_lines)]
pub fn decode_product_response(encoded: &[u8]) -> Result<ProductResponse, ProductCodecError> {
    let (kind, payload) = decode_envelope(encoded, PRODUCT_RESPONSE_MAGIC)?;
    let mut decoder = Decoder::new(payload);
    let response = match kind {
        RESPONSE_CAPABILITIES => {
            ProductResponse::Capabilities(hyphae_native_product::ProductCapabilities {
                product_api_version: decoder.u16()?,
                native_directory_format: decoder.u16()?,
                logical_catalog_codec_version: decoder.u16()?,
                catalog_tree_format_version: decoder.u16()?,
                max_catalog_items: decoder.usize()?,
                max_catalog_visits: decoder.usize()?,
                max_catalog_bytes: decoder.usize()?,
                max_sql_statement_bytes: decoder.usize()?,
                max_sql_parameters: decoder.usize()?,
                max_sql_rows: decoder.usize()?,
            })
        }
        RESPONSE_PREPARED_SQL => ProductResponse::PreparedSql {
            handle: ProductPreparedHandle::new(decoder.u64()?)
                .ok_or(ProductCodecError::InvalidValue)?,
            catalog_version: CatalogVersion::new(decoder.u64()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            parameter_count: decoder.usize()?,
            maximum_result_rows: decoder.usize()?,
        },
        RESPONSE_DEALLOCATED => ProductResponse::Deallocated,
        RESPONSE_CATALOG_OBJECT => ProductResponse::CatalogObject(ProductRead {
            snapshot: decode_snapshot(&mut decoder)?,
            value: CatalogObject::decode_definition(&decoder.owned_bytes()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
        }),
        RESPONSE_CATALOG_PAGE => ProductResponse::CatalogPage(decode_catalog_page(&mut decoder)?),
        RESPONSE_CATALOG_VISIBLE_PAGE => {
            ProductResponse::CatalogVisiblePage(decode_catalog_visible_page(&mut decoder)?)
        }
        RESPONSE_CATALOG_DEPENDENCY_PAGE => {
            ProductResponse::CatalogDependencyPage(decode_dependency_page(&mut decoder)?)
        }
        RESPONSE_CATALOG_DEFINITION => {
            let present = decoder.u8()?;
            if present > 1 || decoder.bytes(3)? != [0; 3] {
                return Err(ProductCodecError::Malformed);
            }
            ProductResponse::CatalogDefinition(if present == 1 {
                Some(
                    LogicalCatalogObject::decode_definition_v2(&decoder.owned_bytes()?)
                        .map_err(|_| ProductCodecError::InvalidValue)?,
                )
            } else {
                None
            })
        }
        RESPONSE_CATALOG_CREATED => {
            ProductResponse::CatalogCreated(decode_commit_outcome(&mut decoder)?)
        }
        RESPONSE_SQL => {
            let flags = decoder.u8()?;
            if flags & !3 != 0 || decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            let snapshot = if flags & 1 != 0 {
                Some(decode_snapshot(&mut decoder)?)
            } else {
                None
            };
            let commit = if flags & 2 != 0 {
                Some(decode_commit_outcome(&mut decoder)?)
            } else {
                None
            };
            ProductResponse::Sql {
                result: decode_sql_result(&mut decoder)?,
                snapshot,
                commit,
            }
        }
        RESPONSE_STRUCTURE_VALUE => {
            let present = decoder.u8()?;
            if decoder.bytes(3)? != [0; 3] || present > 1 {
                return Err(ProductCodecError::Malformed);
            }
            ProductResponse::StructureValue(if present == 1 {
                Some(decoder.owned_bytes()?)
            } else {
                None
            })
        }
        RESPONSE_STRUCTURE_SET => {
            ProductResponse::StructureSet(decode_commit_outcome(&mut decoder)?)
        }
        RESPONSE_STRUCTURE_TTL => ProductResponse::StructureTtl(match decoder.u8()? {
            0 => ProductTtl::Missing,
            1 => ProductTtl::Persistent,
            2 => ProductTtl::RemainingMicros(decoder.i64()?),
            _ => return Err(ProductCodecError::Malformed),
        }),
        RESPONSE_TRANSACTION_STATUS => {
            ProductResponse::TransactionStatus(decode_transaction_status(&mut decoder)?)
        }
        RESPONSE_SEARCH => {
            let count = decoder.usize_u32()?;
            if decoder.bytes(4)? != [0; 4] {
                return Err(ProductCodecError::Malformed);
            }
            let documents_examined = decoder.usize()?;
            let source_bytes = decoder.usize()?;
            let token_visits = decoder.usize()?;
            let token_comparisons = decoder.usize()?;
            let fuzzy_steps = decoder.usize()?;
            let mut hits = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                hits.push(ProductSearchHit {
                    document_id: decoder.owned_bytes()?,
                    score: hyphae_native_product::CanonicalF64::new(f64::from_bits(decoder.u64()?)),
                });
            }
            ProductResponse::Search(ProductSearchResults {
                hits,
                documents_examined,
                source_bytes,
                token_visits,
                token_comparisons,
                fuzzy_steps,
            })
        }
        RESPONSE_ADMIN_STATUS => ProductResponse::AdminStatus(AdminStatus {
            snapshot: decode_snapshot(&mut decoder)?,
            snapshot_pin_count: decoder.usize()?,
            physical: ProductPhysicalObservation {
                page_count: decoder.u64()?,
                physical_page_reads: decoder.u64()?,
                wal_bytes: decoder.u64()?,
                process_full_state_loads: decoder.u64()?,
                process_full_catalog_loads: decoder.u64()?,
            },
            retained_wal_bytes: decoder.u64()?,
            replayed_transactions: decoder.usize()?,
            manifest_count: decoder.usize()?,
            blob_count: decoder.usize()?,
        }),
        RESPONSE_ADMIN_CHECKPOINT => {
            let transaction_id = decoder.u128()?;
            let visible_csn = decoder.u64()?;
            let manifest_generation = decoder.u64()?;
            let manifest_digest = decoder.array()?;
            let checkpoint_lsn = decoder.u64()?;
            let parent_directory_sync_supported = match decoder.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ProductCodecError::InvalidValue),
            };
            ProductResponse::AdminCheckpoint(ProductCheckpointReceipt {
                transaction_id,
                visible_csn,
                manifest_generation,
                manifest_digest,
                checkpoint_lsn,
                parent_directory_sync_supported,
            })
        }
        RESPONSE_EXPLAIN => ProductResponse::Explain(decode_explain(&mut decoder)?),
        RESPONSE_DOCTOR => ProductResponse::Doctor(decode_doctor_report(&mut decoder)?),
        RESPONSE_BACKUP => ProductResponse::Backup(BackupInfo {
            path: decoder.path()?,
            visible_csn: decoder.u64()?,
            checkpoint_digest: decoder.array()?,
            file_count: decoder.usize()?,
            total_bytes: decoder.u64()?,
        }),
        RESPONSE_TELEMETRY => ProductResponse::Telemetry(decode_telemetry(&mut decoder)?),
        RESPONSE_PROOF_VERIFICATION => {
            let kind = decode_proof_kind(decoder.u8()?)?;
            let semantic_reexecution_performed = decoder.boolean()?;
            if decoder.bytes(6)? != [0; 6] {
                return Err(ProductCodecError::Malformed);
            }
            ProductResponse::ProofVerification(
                hyphae_native_product::proof::NativeProofVerificationReport {
                    scope: hyphae_native_product::proof::NativeVerificationScope::ArtifactIntegrity,
                    kind,
                    anchor_digest: decoder.array()?,
                    proof_digest: decoder.array()?,
                    witness_digest: decoder.array()?,
                    request_digest: decoder.array()?,
                    result_digest: decoder.array()?,
                    evidence_digest: decoder.array()?,
                    file_count: decoder.usize()?,
                    directory_count: decoder.usize()?,
                    total_file_bytes: decoder.u64()?,
                    semantic_reexecution_performed,
                },
            )
        }
        RESPONSE_INTEGRATED_SEARCH => {
            ProductResponse::IntegratedSearch(decode_integrated_search(&mut decoder)?)
        }
        RESPONSE_STRUCTURE_MUTATED => {
            ProductResponse::StructureMutated(decode_commit_outcome(&mut decoder)?)
        }
        RESPONSE_STRUCTURE_READ => ProductResponse::StructureRead(ProductRead {
            snapshot: decode_snapshot(&mut decoder)?,
            value: decode_structure_read_result(&mut decoder)?,
        }),
        RESPONSE_RESTORE => ProductResponse::Restore(hyphae_native_product::RestoreInfo {
            data_path: decoder.path()?,
            backup: BackupInfo {
                path: decoder.path()?,
                visible_csn: decoder.u64()?,
                checkpoint_digest: decoder.array()?,
                file_count: decoder.usize()?,
                total_bytes: decoder.u64()?,
            },
            doctor: decode_doctor_report(&mut decoder)?,
            phases: {
                let count = decoder.usize_u32()?;
                let mut phases = Vec::with_capacity(count);
                for _ in 0..count {
                    phases.push(decode_restore_phase(decoder.u8()?)?);
                }
                phases
            },
        }),
        RESPONSE_SECURITY_STATUS => {
            ProductResponse::SecurityStatus(decode_security_status(&mut decoder)?)
        }
        RESPONSE_SECURITY_PRINCIPAL_PAGE => {
            ProductResponse::SecurityPrincipalPage(decode_security_principal_page(&mut decoder)?)
        }
        RESPONSE_SECURITY_ROLE_PAGE => {
            ProductResponse::SecurityRolePage(decode_security_role_page(&mut decoder)?)
        }
        RESPONSE_SECURITY_ASSIGNMENT_PAGE => {
            ProductResponse::SecurityAssignmentPage(decode_security_assignment_page(&mut decoder)?)
        }
        RESPONSE_SECURITY_KEY_PAGE => {
            ProductResponse::SecurityKeyPage(decode_security_key_page(&mut decoder)?)
        }
        RESPONSE_SECURITY_AUDIT_PAGE => {
            ProductResponse::SecurityAuditPage(decode_security_audit_page(&mut decoder)?)
        }
        RESPONSE_SECURITY_PRINCIPAL_MUTATED => {
            let (principal_id, authorization_epoch, commit) =
                decode_security_mutation_receipt(&mut decoder)?;
            ProductResponse::SecurityPrincipalMutated(SecurityPrincipalMutationReceipt {
                principal_id,
                authorization_epoch,
                commit,
            })
        }
        RESPONSE_SECURITY_CUSTOM_ROLE_MUTATED => {
            let (role_id, authorization_epoch, commit) =
                decode_security_mutation_receipt(&mut decoder)?;
            ProductResponse::SecurityCustomRoleMutated(CustomRoleMutationReceipt {
                role_id,
                authorization_epoch,
                commit,
            })
        }
        RESPONSE_SECURITY_ASSIGNMENT_MUTATED => {
            let (assignment_id, authorization_epoch, commit) =
                decode_security_mutation_receipt(&mut decoder)?;
            ProductResponse::SecurityAssignmentMutated(RoleAssignmentMutationReceipt {
                assignment_id,
                authorization_epoch,
                commit,
            })
        }
        RESPONSE_SECURITY_MUTATED => {
            ProductResponse::SecurityMutated(AccessControlMutationReceipt {
                authorization_epoch: decode_authorization_epoch(&mut decoder)?,
                commit: decode_receipt(&mut decoder)?,
            })
        }
        RESPONSE_SECURITY_API_KEY_STARTED => {
            let key_id = decode_api_key_id(decoder.array()?)?;
            let principal_id = decode_security_id(decoder.array()?)?;
            let predecessor_key_id = decode_optional_api_key_id(&mut decoder)?;
            let authorization_epoch = decode_authorization_epoch(&mut decoder)?;
            let commit = decode_receipt(&mut decoder)?;
            let secret = ApiKeySecretDelivery::from_bytes(&decoder.owned_bytes()?)
                .map_err(|_| ProductCodecError::InvalidValue)?;
            if secret.id() != key_id {
                return Err(ProductCodecError::InvalidValue);
            }
            ProductResponse::SecurityApiKeyStarted(ApiKeyStartReceipt {
                key_id,
                principal_id,
                predecessor_key_id,
                authorization_epoch,
                commit,
                secret,
            })
        }
        RESPONSE_SECURITY_API_KEY_ACTIVATED => {
            ProductResponse::SecurityApiKeyActivated(ApiKeyActivationReceipt {
                key_id: decode_api_key_id(decoder.array()?)?,
                predecessor_key_id: decode_optional_api_key_id(&mut decoder)?,
                overlap_until_micros: decode_fixed_optional_i64(&mut decoder)?,
                authorization_epoch: decode_authorization_epoch(&mut decoder)?,
                commit: decode_receipt(&mut decoder)?,
            })
        }
        RESPONSE_SEARCH_INGESTED => {
            ProductResponse::SearchIngested(decode_search_ingest_receipt(&mut decoder)?)
        }
        RESPONSE_EXPLICIT_TRANSACTION_STATUS => ProductResponse::ExplicitTransactionStatus(
            decode_explicit_transaction_status(&mut decoder)?,
        ),
        RESPONSE_TRANSACTION_STAGED => {
            ProductResponse::TransactionStaged(decode_transaction_stage_receipt(&mut decoder)?)
        }
        RESPONSE_TRANSACTION_COMMITTED => {
            ProductResponse::TransactionCommitted(ProductExplicitCommitReceipt {
                handle: decode_transaction_handle(&mut decoder)?,
                staged_operations: decoder.usize()?,
                commit: decode_receipt(&mut decoder)?,
            })
        }
        RESPONSE_TRANSACTION_ROLLED_BACK => {
            ProductResponse::TransactionRolledBack(ProductRollbackReceipt {
                handle: decode_transaction_handle(&mut decoder)?,
                discarded_operations: decoder.usize()?,
            })
        }
        RESPONSE_PROVEN => {
            let response = Box::new(decode_product_response(&decoder.owned_bytes()?)?);
            let proof_bytes = decoder.owned_bytes()?;
            let witness_bytes = decoder.owned_bytes()?;
            let trusted_anchor = ExternalTrustedAnchor::new(decoder.array()?);
            let proof = decode_native_proof(&proof_bytes, &ProofCodecLimits::default())
                .map_err(|_| ProductCodecError::InvalidValue)?;
            ProductResponse::Proven {
                response,
                artifact: Box::new(NativeOperationProofArtifact {
                    proof,
                    proof_bytes,
                    witness_bytes,
                    trusted_anchor,
                }),
            }
        }
        _ => return Err(ProductCodecError::Unsupported),
    };
    decoder.finish()?;
    Ok(response)
}

/// Decodes one response while rejecting variants introduced after the
/// negotiated protocol minor.
pub fn decode_product_response_for_minor(
    encoded: &[u8],
    negotiated_minor: u16,
) -> Result<ProductResponse, ProductCodecError> {
    let response = decode_product_response(encoded)?;
    ensure_response_minor(&response, negotiated_minor)?;
    Ok(response)
}

/// Lowest protocol minor whose codec admits this doc value.
fn doc_value_required_minor(value: &ProductDocValue) -> u16 {
    match value {
        ProductDocValue::Boolean(_)
        | ProductDocValue::Integer(_)
        | ProductDocValue::String(_)
        | ProductDocValue::Bytes(_) => 0,
    }
}

/// Lowest protocol minor whose codec admits this comparison operator.
fn operator_required_minor(operator: ProductSearchOperator) -> u16 {
    match operator {
        ProductSearchOperator::Equal
        | ProductSearchOperator::NotEqual
        | ProductSearchOperator::Less
        | ProductSearchOperator::LessOrEqual
        | ProductSearchOperator::Greater
        | ProductSearchOperator::GreaterOrEqual => 0,
    }
}

/// Lowest protocol minor whose codec admits every node of this filter.
/// Depth is already bounded by the strict decoder before this walk runs.
fn filter_required_minor(filter: &ProductSearchFilter) -> u16 {
    match filter {
        ProductSearchFilter::MatchAll | ProductSearchFilter::Exists(_) => 0,
        ProductSearchFilter::Compare {
            operator, value, ..
        } => operator_required_minor(*operator).max(doc_value_required_minor(value)),
        ProductSearchFilter::All(children) | ProductSearchFilter::Any(children) => children
            .iter()
            .map(filter_required_minor)
            .max()
            .unwrap_or(0),
        ProductSearchFilter::Not(child) => filter_required_minor(child),
        ProductSearchFilter::In { values, .. } => values
            .iter()
            .map(doc_value_required_minor)
            .max()
            .unwrap_or(0)
            .max(4),
        ProductSearchFilter::IsNull(_) | ProductSearchFilter::Like { .. } => 4,
    }
}

/// Lowest protocol minor whose codec admits every doc value of this document.
fn document_required_minor(document: &ProductDocument) -> u16 {
    document
        .doc_values
        .values()
        .map(doc_value_required_minor)
        .max()
        .unwrap_or(0)
}

/// Lowest protocol minor whose codec admits every part of this search
/// request. Future request content (fusion methods, new operators, new
/// doc-value types) raises the requirement here rather than adding a new
/// operation variant.
fn search_request_required_minor(request: &ProductSearchRequest) -> u16 {
    let fusion = match request.fusion {
        None => 0,
        Some(hyphae_native_product::ProductFusionMethod::WeightedScore) => 4,
    };
    let dedupe = if request.parent_dedupe.is_some() {
        4
    } else {
        0
    };
    let rerank = if request.rerank.is_some() { 4 } else { 0 };
    let highlight = if request.highlight.is_some() { 5 } else { 0 };
    filter_required_minor(&request.filter)
        .max(fusion)
        .max(dedupe)
        .max(rerank)
        .max(highlight)
}

fn ensure_operation_minor(
    operation: &ProductOperation,
    negotiated_minor: u16,
) -> Result<(), ProductCodecError> {
    let required_minor = match operation {
        ProductOperation::SecurityStatus
        | ProductOperation::SecurityPrincipalList(_)
        | ProductOperation::SecurityRoleList(_)
        | ProductOperation::SecurityAssignmentList(_)
        | ProductOperation::SecurityKeyList(_)
        | ProductOperation::SecurityAuditRead(_) => 1,
        ProductOperation::SecurityPrincipalCreate { .. }
        | ProductOperation::SecurityPrincipalSetEnabled { .. }
        | ProductOperation::SecurityCustomRoleCreate { .. }
        | ProductOperation::SecurityBuiltInAssignmentCreate { .. }
        | ProductOperation::SecurityCustomAssignmentCreate { .. }
        | ProductOperation::SecurityAssignmentRevoke { .. } => 2,
        ProductOperation::CatalogVisibleList(_)
        | ProductOperation::SecurityApiKeyIssueSelfStart { .. }
        | ProductOperation::SecurityApiKeyIssueStart { .. }
        | ProductOperation::SecurityApiKeyIssueSelfActivate { .. }
        | ProductOperation::SecurityApiKeyIssueActivate { .. }
        | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
        | ProductOperation::SecurityApiKeyRotateStart { .. }
        | ProductOperation::SecurityApiKeyRotateSelfActivate { .. }
        | ProductOperation::SecurityApiKeyRotateActivate { .. }
        | ProductOperation::SecurityApiKeyIssueSelfAbort { .. }
        | ProductOperation::SecurityApiKeyIssueAbort { .. }
        | ProductOperation::SecurityApiKeyRotateSelfAbort { .. }
        | ProductOperation::SecurityApiKeyRotateAbort { .. }
        | ProductOperation::SecurityApiKeyRevokeSelf { .. }
        | ProductOperation::SecurityApiKeyRevoke { .. }
        | ProductOperation::SecurityLegacyBearerRevoke => 3,
        ProductOperation::Prove { operation, .. } => {
            return ensure_operation_minor(operation, negotiated_minor);
        }
        ProductOperation::SearchCollection { request, .. } => {
            search_request_required_minor(request)
        }
        ProductOperation::SearchIngest { batch, .. } => batch
            .documents
            .iter()
            .map(document_required_minor)
            .max()
            .unwrap_or(0),
        ProductOperation::SearchDocumentUpdate { update, .. } => {
            document_required_minor(&update.document)
        }
        ProductOperation::StructureRead(
            ProductStructureReadRequest::SortedSetScoreRange { .. }
            | ProductStructureReadRequest::HashScanReverse { .. }
            | ProductStructureReadRequest::HashScanMatch { .. }
            | ProductStructureReadRequest::KeyScanMatch { .. }
            | ProductStructureReadRequest::StringRange { .. }
            | ProductStructureReadRequest::SetRandomMembers { .. },
        ) => 6,
        ProductOperation::StructureMutate { mutations }
            if mutations.iter().any(structure_mutation_requires_minor_six) =>
        {
            6
        }
        ProductOperation::TransactionStageStructure { mutation, .. }
            if structure_mutation_requires_minor_six(mutation) =>
        {
            6
        }
        _ => 0,
    };
    if negotiated_minor < required_minor {
        Err(ProductCodecError::Unsupported)
    } else {
        Ok(())
    }
}

fn ensure_response_minor(
    response: &ProductResponse,
    negotiated_minor: u16,
) -> Result<(), ProductCodecError> {
    let required_minor = match response {
        ProductResponse::SecurityStatus(_)
        | ProductResponse::SecurityPrincipalPage(_)
        | ProductResponse::SecurityRolePage(_)
        | ProductResponse::SecurityAssignmentPage(_)
        | ProductResponse::SecurityKeyPage(_)
        | ProductResponse::SecurityAuditPage(_) => 1,
        ProductResponse::SecurityPrincipalMutated(_)
        | ProductResponse::SecurityCustomRoleMutated(_)
        | ProductResponse::SecurityAssignmentMutated(_)
        | ProductResponse::SecurityMutated(_) => 2,
        ProductResponse::CatalogVisiblePage(_)
        | ProductResponse::SecurityApiKeyStarted(_)
        | ProductResponse::SecurityApiKeyActivated(_) => 3,
        ProductResponse::Proven { response, .. } => {
            return ensure_response_minor(response, negotiated_minor);
        }
        ProductResponse::IntegratedSearch(result) => {
            let values = result
                .hits
                .iter()
                .flat_map(|hit| hit.doc_values.values())
                .map(doc_value_required_minor)
                .max()
                .unwrap_or(0);
            let fragments = if result.hits.iter().any(|hit| !hit.fragments.is_empty()) {
                5
            } else {
                0
            };
            values.max(fragments)
        }
        ProductResponse::StructureRead(read) => match &read.value {
            ProductStructureReadResult::HashPage { .. }
            | ProductStructureReadResult::KeyPage { .. } => 6,
            _ => 0,
        },
        ProductResponse::TransactionStaged(receipt) => match &receipt.result {
            ProductTransactionStageResult::Structure(
                ProductStructureMutationResult::Score(_)
                | ProductStructureMutationResult::PoppedEntry(_),
            ) => 6,
            _ => 0,
        },
        _ => 0,
    };
    if negotiated_minor < required_minor {
        Err(ProductCodecError::Unsupported)
    } else {
        Ok(())
    }
}

fn structure_mutation_requires_minor_six(mutation: &ProductStructureMutation) -> bool {
    matches!(
        mutation,
        ProductStructureMutation::SortedSetIncrement { .. }
            | ProductStructureMutation::SortedSetPop { .. }
            | ProductStructureMutation::StringSetConditional { .. }
            | ProductStructureMutation::StringAppend { .. }
            | ProductStructureMutation::StringSetRange { .. }
            | ProductStructureMutation::HashSetIfAbsent { .. }
            | ProductStructureMutation::SetPop { .. }
    )
}

fn operation_requires_idempotency(operation: &ProductOperation) -> bool {
    matches!(
        operation,
        ProductOperation::SecurityPrincipalCreate { .. }
            | ProductOperation::SecurityPrincipalSetEnabled { .. }
            | ProductOperation::SecurityCustomRoleCreate { .. }
            | ProductOperation::SecurityBuiltInAssignmentCreate { .. }
            | ProductOperation::SecurityCustomAssignmentCreate { .. }
            | ProductOperation::SecurityAssignmentRevoke { .. }
            | ProductOperation::SecurityApiKeyIssueSelfStart { .. }
            | ProductOperation::SecurityApiKeyIssueStart { .. }
            | ProductOperation::SecurityApiKeyIssueSelfActivate { .. }
            | ProductOperation::SecurityApiKeyIssueActivate { .. }
            | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
            | ProductOperation::SecurityApiKeyRotateStart { .. }
            | ProductOperation::SecurityApiKeyRotateSelfActivate { .. }
            | ProductOperation::SecurityApiKeyRotateActivate { .. }
            | ProductOperation::SecurityApiKeyIssueSelfAbort { .. }
            | ProductOperation::SecurityApiKeyIssueAbort { .. }
            | ProductOperation::SecurityApiKeyRotateSelfAbort { .. }
            | ProductOperation::SecurityApiKeyRotateAbort { .. }
            | ProductOperation::SecurityApiKeyRevokeSelf { .. }
            | ProductOperation::SecurityApiKeyRevoke { .. }
            | ProductOperation::SecurityLegacyBearerRevoke
    )
}

fn operation_is_key_lifecycle(operation: &ProductOperation) -> bool {
    matches!(
        operation,
        ProductOperation::SecurityApiKeyIssueSelfStart { .. }
            | ProductOperation::SecurityApiKeyIssueStart { .. }
            | ProductOperation::SecurityApiKeyIssueSelfActivate { .. }
            | ProductOperation::SecurityApiKeyIssueActivate { .. }
            | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
            | ProductOperation::SecurityApiKeyRotateStart { .. }
            | ProductOperation::SecurityApiKeyRotateSelfActivate { .. }
            | ProductOperation::SecurityApiKeyRotateActivate { .. }
            | ProductOperation::SecurityApiKeyIssueSelfAbort { .. }
            | ProductOperation::SecurityApiKeyIssueAbort { .. }
            | ProductOperation::SecurityApiKeyRotateSelfAbort { .. }
            | ProductOperation::SecurityApiKeyRotateAbort { .. }
            | ProductOperation::SecurityApiKeyRevokeSelf { .. }
            | ProductOperation::SecurityApiKeyRevoke { .. }
            | ProductOperation::SecurityLegacyBearerRevoke
    )
}

/// Encodes a canonical `HYPERR01` product error payload.
pub fn encode_failure(error: &ProductError) -> Result<Vec<u8>, ProductCodecError> {
    encode_product_error(error).map_err(Into::into)
}

/// Decodes a canonical `HYPERR01` product error payload.
pub fn decode_failure(encoded: &[u8]) -> Result<ProductError, ProductCodecError> {
    decode_product_error(encoded).map_err(Into::into)
}

#[allow(clippy::too_many_lines)]
fn encode_operation(operation: &ProductOperation) -> Result<(u16, Vec<u8>), ProductCodecError> {
    let mut body = Vec::new();
    let kind = match operation {
        ProductOperation::Capabilities => REQUEST_CAPABILITIES,
        ProductOperation::CatalogObject { id } => {
            body.extend_from_slice(&id.get().to_le_bytes());
            REQUEST_CATALOG_OBJECT
        }
        ProductOperation::CatalogObjectNamed { name } => {
            encode_qualified_name(&mut body, name)?;
            REQUEST_CATALOG_OBJECT_NAMED
        }
        ProductOperation::CatalogList(request) => {
            body.push(u8::from(request.parent.is_some()));
            body.push(request.kind.map_or(0, |kind| kind as u8));
            body.extend_from_slice(&[0; 6]);
            if let Some(parent) = request.parent {
                body.extend_from_slice(&parent.get().to_le_bytes());
            }
            encode_cursor(&mut body, request.cursor);
            put_u64(&mut body, request.item_limit)?;
            put_u64(&mut body, request.visit_limit)?;
            put_u64(&mut body, request.byte_limit)?;
            REQUEST_CATALOG_LIST
        }
        ProductOperation::CatalogVisibleList(request) => {
            body.push(u8::from(request.filter.parent.is_some()));
            body.push(request.filter.kind.map_or(0, |kind| kind as u8));
            body.extend_from_slice(&[0; 6]);
            if let Some(parent) = request.filter.parent {
                body.extend_from_slice(&parent.get().to_le_bytes());
            }
            put_bytes(
                &mut body,
                request
                    .cursor
                    .as_ref()
                    .map_or(&[], CatalogVisibleCursor::as_bytes),
            )?;
            put_u64(&mut body, request.item_limit)?;
            put_u64(&mut body, request.visit_limit)?;
            put_u64(&mut body, request.byte_limit)?;
            REQUEST_CATALOG_VISIBLE_LIST
        }
        ProductOperation::CatalogDependencies(request) => {
            body.extend_from_slice(&request.object.get().to_le_bytes());
            body.push(match request.direction {
                DependencyDirection::Outgoing => 0,
                DependencyDirection::Incoming => 1,
            });
            body.extend_from_slice(&[0; 7]);
            encode_cursor(&mut body, request.cursor);
            put_u64(&mut body, request.item_limit)?;
            put_u64(&mut body, request.visit_limit)?;
            put_u64(&mut body, request.byte_limit)?;
            REQUEST_CATALOG_DEPENDENCIES
        }
        ProductOperation::CatalogDescribe { id } => {
            body.extend_from_slice(&id.get().to_le_bytes());
            REQUEST_CATALOG_DESCRIBE
        }
        ProductOperation::CatalogResolve { name } => {
            encode_qualified_name(&mut body, name)?;
            REQUEST_CATALOG_RESOLVE
        }
        ProductOperation::CatalogCreate { object } => {
            put_bytes(
                &mut body,
                &object
                    .encode_definition_v2()
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            )?;
            REQUEST_CATALOG_CREATE
        }
        ProductOperation::PrepareSql { statement } => {
            put_text(&mut body, statement)?;
            REQUEST_PREPARE_SQL
        }
        ProductOperation::ExecutePrepared { handle, parameters } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            encode_values(&mut body, parameters)?;
            REQUEST_EXECUTE_PREPARED
        }
        ProductOperation::DeallocatePrepared { handle } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            REQUEST_DEALLOCATE_PREPARED
        }
        ProductOperation::ExecuteSql {
            statement,
            parameters,
        } => {
            put_text(&mut body, statement)?;
            encode_values(&mut body, parameters)?;
            REQUEST_EXECUTE_SQL
        }
        ProductOperation::StructureGet { key } => {
            put_bytes(&mut body, key)?;
            REQUEST_STRUCTURE_GET
        }
        ProductOperation::StructureSet {
            key,
            value,
            expires_at_micros,
        } => {
            put_bytes(&mut body, key)?;
            put_bytes(&mut body, value)?;
            body.push(u8::from(expires_at_micros.is_some()));
            body.extend_from_slice(&[0; 7]);
            if let Some(expiry) = expires_at_micros {
                body.extend_from_slice(&expiry.to_le_bytes());
            }
            REQUEST_STRUCTURE_SET
        }
        ProductOperation::StructureTtl { key } => {
            put_bytes(&mut body, key)?;
            REQUEST_STRUCTURE_TTL
        }
        ProductOperation::TransactionStatus { transaction_id } => {
            body.extend_from_slice(&transaction_id.get().to_le_bytes());
            REQUEST_TRANSACTION_STATUS
        }
        ProductOperation::TransactionStatusByIdempotency { idempotency_token } => {
            if *idempotency_token == 0 {
                return Err(ProductCodecError::InvalidValue);
            }
            body.extend_from_slice(&idempotency_token.to_le_bytes());
            REQUEST_TRANSACTION_STATUS_BY_IDEMPOTENCY
        }
        ProductOperation::Search {
            index,
            query,
            limit,
        } => {
            body.extend_from_slice(&index.get().to_le_bytes());
            put_u64(&mut body, *limit)?;
            encode_query(&mut body, query, 0)?;
            REQUEST_SEARCH
        }
        ProductOperation::AdminStatus => REQUEST_ADMIN_STATUS,
        ProductOperation::AdminCheckpoint => REQUEST_ADMIN_CHECKPOINT,
        ProductOperation::AdminExplainSql { statement } => {
            put_text(&mut body, statement)?;
            REQUEST_ADMIN_EXPLAIN_SQL
        }
        ProductOperation::Doctor(_request) => {
            // Transport-facing diagnosis is always scoped to the serving
            // product directory; carrying an arbitrary host path would make
            // the HTTP/local contract both nonportable and over-privileged.
            REQUEST_DOCTOR
        }
        ProductOperation::Backup(request) => {
            put_path(&mut body, &request.destination)?;
            put_u64(&mut body, request.limits.max_files)?;
            put_u64(&mut body, request.limits.max_directories)?;
            body.extend_from_slice(&request.limits.max_total_bytes.to_le_bytes());
            put_u64(&mut body, request.limits.max_path_bytes)?;
            body.extend_from_slice(&request.limits.max_manifest_bytes.to_le_bytes());
            REQUEST_BACKUP
        }
        ProductOperation::Telemetry => REQUEST_TELEMETRY,
        ProductOperation::VerifyProof {
            proof,
            witness,
            trusted_anchor,
        } => {
            put_bytes(&mut body, proof)?;
            put_bytes(&mut body, witness)?;
            body.extend_from_slice(trusted_anchor);
            REQUEST_VERIFY_PROOF
        }
        ProductOperation::SearchCollection {
            collection,
            request,
        } => {
            encode_search_collection(&mut body, *collection, request)?;
            REQUEST_SEARCH_COLLECTION
        }
        ProductOperation::SearchIngest { collection, batch } => {
            body.extend_from_slice(&collection.get().to_le_bytes());
            encode_search_ingest_batch(&mut body, batch)?;
            REQUEST_SEARCH_INGEST
        }
        ProductOperation::SearchDocumentUpdate { collection, update } => {
            body.extend_from_slice(&collection.get().to_le_bytes());
            body.extend_from_slice(&update.idempotency_id.to_le_bytes());
            encode_product_document(&mut body, &update.document)?;
            REQUEST_SEARCH_DOCUMENT_UPDATE
        }
        ProductOperation::SearchDocumentDelete { collection, delete } => {
            body.extend_from_slice(&collection.get().to_le_bytes());
            body.extend_from_slice(&delete.idempotency_id.to_le_bytes());
            body.extend_from_slice(&delete.object_id.get().to_le_bytes());
            REQUEST_SEARCH_DOCUMENT_DELETE
        }
        ProductOperation::StructureMutate { mutations } => {
            put_u32(&mut body, mutations.len())?;
            for mutation in mutations {
                encode_structure_mutation(&mut body, mutation)?;
            }
            REQUEST_STRUCTURE_MUTATE
        }
        ProductOperation::StructureRead(request) => {
            encode_structure_read_request(&mut body, request)?;
            REQUEST_STRUCTURE_READ
        }
        ProductOperation::Restore(request) => {
            put_path(&mut body, &request.backup)?;
            put_path(&mut body, &request.destination)?;
            put_u64(&mut body, request.limits.max_files)?;
            put_u64(&mut body, request.limits.max_directories)?;
            body.extend_from_slice(&request.limits.max_total_bytes.to_le_bytes());
            put_u64(&mut body, request.limits.max_path_bytes)?;
            body.extend_from_slice(&request.limits.max_manifest_bytes.to_le_bytes());
            body.extend_from_slice(&request.doctor_logical_time_micros.to_le_bytes());
            REQUEST_RESTORE
        }
        ProductOperation::SecurityStatus => REQUEST_SECURITY_STATUS,
        ProductOperation::SecurityPrincipalList(request) => {
            request
                .validate()
                .map_err(|_| ProductCodecError::LimitExceeded)?;
            encode_security_cursor(&mut body, request.cursor(), SecurityCursorFamily::Principal)?;
            put_u64(&mut body, request.limit())?;
            REQUEST_SECURITY_PRINCIPAL_LIST
        }
        ProductOperation::SecurityRoleList(request) => {
            request
                .validate()
                .map_err(|_| ProductCodecError::LimitExceeded)?;
            encode_security_cursor(&mut body, request.cursor(), SecurityCursorFamily::Role)?;
            put_u64(&mut body, request.limit())?;
            REQUEST_SECURITY_ROLE_LIST
        }
        ProductOperation::SecurityAssignmentList(request) => {
            request
                .validate()
                .map_err(|_| ProductCodecError::LimitExceeded)?;
            encode_security_cursor(
                &mut body,
                request.cursor(),
                SecurityCursorFamily::Assignment,
            )?;
            put_u64(&mut body, request.limit())?;
            REQUEST_SECURITY_ASSIGNMENT_LIST
        }
        ProductOperation::SecurityKeyList(request) => {
            request
                .validate()
                .map_err(|_| ProductCodecError::LimitExceeded)?;
            encode_security_cursor(&mut body, request.cursor(), SecurityCursorFamily::Key)?;
            put_u64(&mut body, request.limit())?;
            REQUEST_SECURITY_KEY_LIST
        }
        ProductOperation::SecurityAuditRead(request) => {
            request
                .validate()
                .map_err(|_| ProductCodecError::LimitExceeded)?;
            encode_optional_security_id(&mut body, request.cursor());
            put_u64(&mut body, request.limit())?;
            REQUEST_SECURITY_AUDIT_READ
        }
        ProductOperation::SecurityPrincipalCreate { display_name } => {
            put_security_text(&mut body, display_name)?;
            REQUEST_SECURITY_PRINCIPAL_CREATE
        }
        ProductOperation::SecurityPrincipalSetEnabled {
            principal_id,
            enabled,
        } => {
            body.extend_from_slice(&principal_id.to_be_bytes());
            body.push(u8::from(*enabled));
            body.extend_from_slice(&[0; 7]);
            REQUEST_SECURITY_PRINCIPAL_SET_ENABLED
        }
        ProductOperation::SecurityCustomRoleCreate {
            display_name,
            grants,
        } => {
            put_security_text(&mut body, display_name)?;
            encode_custom_role_grants(&mut body, grants)?;
            REQUEST_SECURITY_CUSTOM_ROLE_CREATE
        }
        ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id,
            role,
            scope,
        } => {
            body.extend_from_slice(&principal_id.to_be_bytes());
            body.push(role.tag());
            body.extend_from_slice(&[0; 7]);
            encode_product_scope(&mut body, *scope);
            REQUEST_SECURITY_BUILT_IN_ASSIGNMENT_CREATE
        }
        ProductOperation::SecurityCustomAssignmentCreate {
            principal_id,
            role_id,
        } => {
            body.extend_from_slice(&principal_id.to_be_bytes());
            body.extend_from_slice(&role_id.to_be_bytes());
            REQUEST_SECURITY_CUSTOM_ASSIGNMENT_CREATE
        }
        ProductOperation::SecurityAssignmentRevoke { assignment_id } => {
            body.extend_from_slice(&assignment_id.to_be_bytes());
            REQUEST_SECURITY_ASSIGNMENT_REVOKE
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
            body.extend_from_slice(&principal_id.to_be_bytes());
            put_security_text(&mut body, label)?;
            encode_built_in_roles(&mut body, roles)?;
            encode_security_ids(&mut body, custom_roles)?;
            body.extend_from_slice(&permission_ceiling.bits().to_le_bytes());
            encode_product_scopes(&mut body, scope_ceiling)?;
            encode_fixed_optional_i64(&mut body, *expires_at_micros);
            if matches!(
                operation,
                ProductOperation::SecurityApiKeyIssueSelfStart { .. }
            ) {
                REQUEST_SECURITY_API_KEY_ISSUE_SELF_START
            } else {
                REQUEST_SECURITY_API_KEY_ISSUE_START
            }
        }
        ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id,
            confirmation_digest,
        }
        | ProductOperation::SecurityApiKeyIssueActivate {
            key_id,
            confirmation_digest,
        } => {
            body.extend_from_slice(key_id.as_bytes());
            body.extend_from_slice(confirmation_digest.as_bytes());
            if matches!(
                operation,
                ProductOperation::SecurityApiKeyIssueSelfActivate { .. }
            ) {
                REQUEST_SECURITY_API_KEY_ISSUE_SELF_ACTIVATE
            } else {
                REQUEST_SECURITY_API_KEY_ISSUE_ACTIVATE
            }
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
            body.extend_from_slice(predecessor_key_id.as_bytes());
            put_security_text(&mut body, label)?;
            body.extend_from_slice(&overlap_seconds.to_le_bytes());
            encode_fixed_optional_i64(&mut body, *expires_at_micros);
            if matches!(
                operation,
                ProductOperation::SecurityApiKeyRotateSelfStart { .. }
            ) {
                REQUEST_SECURITY_API_KEY_ROTATE_SELF_START
            } else {
                REQUEST_SECURITY_API_KEY_ROTATE_START
            }
        }
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id,
            confirmation_digest,
        }
        | ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id,
            confirmation_digest,
        } => {
            body.extend_from_slice(successor_key_id.as_bytes());
            body.extend_from_slice(confirmation_digest.as_bytes());
            if matches!(
                operation,
                ProductOperation::SecurityApiKeyRotateSelfActivate { .. }
            ) {
                REQUEST_SECURITY_API_KEY_ROTATE_SELF_ACTIVATE
            } else {
                REQUEST_SECURITY_API_KEY_ROTATE_ACTIVATE
            }
        }
        ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
        | ProductOperation::SecurityApiKeyIssueAbort { key_id }
        | ProductOperation::SecurityApiKeyRevokeSelf { key_id }
        | ProductOperation::SecurityApiKeyRevoke { key_id } => {
            body.extend_from_slice(key_id.as_bytes());
            match operation {
                ProductOperation::SecurityApiKeyIssueSelfAbort { .. } => {
                    REQUEST_SECURITY_API_KEY_ISSUE_SELF_ABORT
                }
                ProductOperation::SecurityApiKeyIssueAbort { .. } => {
                    REQUEST_SECURITY_API_KEY_ISSUE_ABORT
                }
                ProductOperation::SecurityApiKeyRevokeSelf { .. } => {
                    REQUEST_SECURITY_API_KEY_REVOKE_SELF
                }
                ProductOperation::SecurityApiKeyRevoke { .. } => REQUEST_SECURITY_API_KEY_REVOKE,
                _ => return Err(ProductCodecError::InvalidValue),
            }
        }
        ProductOperation::SecurityApiKeyRotateSelfAbort { successor_key_id }
        | ProductOperation::SecurityApiKeyRotateAbort { successor_key_id } => {
            body.extend_from_slice(successor_key_id.as_bytes());
            if matches!(
                operation,
                ProductOperation::SecurityApiKeyRotateSelfAbort { .. }
            ) {
                REQUEST_SECURITY_API_KEY_ROTATE_SELF_ABORT
            } else {
                REQUEST_SECURITY_API_KEY_ROTATE_ABORT
            }
        }
        ProductOperation::SecurityLegacyBearerRevoke => REQUEST_SECURITY_LEGACY_BEARER_REVOKE,
        ProductOperation::TransactionBegin => REQUEST_TRANSACTION_BEGIN,
        ProductOperation::TransactionStageSql { handle, mutation } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            put_text(&mut body, &mutation.statement)?;
            encode_values(&mut body, &mutation.parameters)?;
            REQUEST_TRANSACTION_STAGE_SQL
        }
        ProductOperation::TransactionStageStructure { handle, mutation } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            encode_structure_mutation(&mut body, mutation)?;
            REQUEST_TRANSACTION_STAGE_STRUCTURE
        }
        ProductOperation::TransactionStageSearch { handle, mutation } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            encode_transaction_search_mutation(&mut body, mutation)?;
            REQUEST_TRANSACTION_STAGE_SEARCH
        }
        ProductOperation::TransactionStageVector { handle, mutation } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            encode_transaction_vector_mutation(&mut body, mutation)?;
            REQUEST_TRANSACTION_STAGE_VECTOR
        }
        ProductOperation::TransactionCommit { handle } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            REQUEST_TRANSACTION_COMMIT
        }
        ProductOperation::TransactionRollback { handle } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            REQUEST_TRANSACTION_ROLLBACK
        }
        ProductOperation::ExplicitTransactionStatus { handle } => {
            body.extend_from_slice(&handle.get().to_le_bytes());
            REQUEST_EXPLICIT_TRANSACTION_STATUS
        }
        ProductOperation::Prove { operation, limits } => {
            if matches!(operation.as_ref(), ProductOperation::Prove { .. })
                || operation.is_key_lifecycle()
            {
                return Err(ProductCodecError::InvalidValue);
            }
            let (operation_kind, operation_body) = encode_operation(operation)?;
            body.extend_from_slice(&operation_kind.to_le_bytes());
            body.extend_from_slice(&0_u16.to_le_bytes());
            put_bytes(&mut body, &operation_body)?;
            encode_proof_generation_limits(&mut body, *limits)?;
            REQUEST_PROVE
        }
        _ => return Err(ProductCodecError::Unsupported),
    };
    Ok((kind, body))
}

#[allow(clippy::too_many_lines)]
fn decode_operation(kind: u16, encoded: &[u8]) -> Result<ProductOperation, ProductCodecError> {
    let mut decoder = Decoder::new(encoded);
    let operation = match kind {
        REQUEST_CAPABILITIES => ProductOperation::Capabilities,
        REQUEST_CATALOG_OBJECT => ProductOperation::CatalogObject {
            id: ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?,
        },
        REQUEST_CATALOG_OBJECT_NAMED => ProductOperation::CatalogObjectNamed {
            name: decode_qualified_name(&mut decoder)?,
        },
        REQUEST_CATALOG_LIST => {
            let has_parent = decoder.u8()?;
            let kind = decoder.u8()?;
            if has_parent > 1 || decoder.bytes(6)? != [0; 6] {
                return Err(ProductCodecError::Malformed);
            }
            ProductOperation::CatalogList(CatalogListRequest {
                parent: if has_parent == 1 {
                    Some(
                        ObjectId::new(decoder.u128()?)
                            .map_err(|_| ProductCodecError::InvalidValue)?,
                    )
                } else {
                    None
                },
                kind: if kind == 0 {
                    None
                } else {
                    Some(decode_catalog_kind(kind)?)
                },
                cursor: decode_cursor(&mut decoder)?,
                item_limit: decoder.usize()?,
                visit_limit: decoder.usize()?,
                byte_limit: decoder.usize()?,
            })
        }
        REQUEST_CATALOG_VISIBLE_LIST => {
            let has_parent = decoder.u8()?;
            let kind = decoder.u8()?;
            if has_parent > 1 || decoder.bytes(6)? != [0; 6] {
                return Err(ProductCodecError::Malformed);
            }
            let parent = if has_parent == 1 {
                Some(ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?)
            } else {
                None
            };
            let cursor = decoder.owned_bytes()?;
            ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
                filter: CatalogVisibleListFilter {
                    parent,
                    kind: if kind == 0 {
                        None
                    } else {
                        Some(decode_catalog_kind(kind)?)
                    },
                },
                cursor: if cursor.is_empty() {
                    None
                } else {
                    Some(
                        CatalogVisibleCursor::new(cursor)
                            .map_err(|_| ProductCodecError::InvalidValue)?,
                    )
                },
                item_limit: decoder.usize()?,
                visit_limit: decoder.usize()?,
                byte_limit: decoder.usize()?,
            })
        }
        REQUEST_CATALOG_DEPENDENCIES => {
            let object =
                ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
            let direction = match decoder.u8()? {
                0 => DependencyDirection::Outgoing,
                1 => DependencyDirection::Incoming,
                _ => return Err(ProductCodecError::InvalidValue),
            };
            if decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            ProductOperation::CatalogDependencies(CatalogDependencyRequest {
                object,
                direction,
                cursor: decode_cursor(&mut decoder)?,
                item_limit: decoder.usize()?,
                visit_limit: decoder.usize()?,
                byte_limit: decoder.usize()?,
            })
        }
        REQUEST_CATALOG_DESCRIBE => ProductOperation::CatalogDescribe {
            id: ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?,
        },
        REQUEST_CATALOG_RESOLVE => ProductOperation::CatalogResolve {
            name: decode_qualified_name(&mut decoder)?,
        },
        REQUEST_CATALOG_CREATE => ProductOperation::CatalogCreate {
            object: LogicalCatalogObject::decode_definition_v2(&decoder.owned_bytes()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
        },
        REQUEST_PREPARE_SQL => ProductOperation::PrepareSql {
            statement: decoder.text()?,
        },
        REQUEST_EXECUTE_PREPARED => ProductOperation::ExecutePrepared {
            handle: ProductPreparedHandle::new(decoder.u64()?)
                .ok_or(ProductCodecError::InvalidValue)?,
            parameters: decode_values(&mut decoder)?,
        },
        REQUEST_DEALLOCATE_PREPARED => ProductOperation::DeallocatePrepared {
            handle: ProductPreparedHandle::new(decoder.u64()?)
                .ok_or(ProductCodecError::InvalidValue)?,
        },
        REQUEST_EXECUTE_SQL => ProductOperation::ExecuteSql {
            statement: decoder.text()?,
            parameters: decode_values(&mut decoder)?,
        },
        REQUEST_STRUCTURE_GET => ProductOperation::StructureGet {
            key: decoder.owned_bytes()?,
        },
        REQUEST_STRUCTURE_SET => {
            let key = decoder.owned_bytes()?;
            let value = decoder.owned_bytes()?;
            let has_expiry = decoder.u8()?;
            if has_expiry > 1 || decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            ProductOperation::StructureSet {
                key,
                value,
                expires_at_micros: if has_expiry == 1 {
                    Some(decoder.i64()?)
                } else {
                    None
                },
            }
        }
        REQUEST_STRUCTURE_TTL => ProductOperation::StructureTtl {
            key: decoder.owned_bytes()?,
        },
        REQUEST_TRANSACTION_STATUS => ProductOperation::TransactionStatus {
            transaction_id: ProductTransactionId::new(decoder.u128()?)
                .ok_or(ProductCodecError::InvalidValue)?,
        },
        REQUEST_TRANSACTION_STATUS_BY_IDEMPOTENCY => {
            let idempotency_token = decoder.u128()?;
            if idempotency_token == 0 {
                return Err(ProductCodecError::InvalidValue);
            }
            ProductOperation::TransactionStatusByIdempotency { idempotency_token }
        }
        REQUEST_SEARCH => ProductOperation::Search {
            index: ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?,
            limit: decoder.usize()?,
            query: decode_query(&mut decoder, 0)?,
        },
        REQUEST_ADMIN_STATUS => ProductOperation::AdminStatus,
        REQUEST_ADMIN_CHECKPOINT => ProductOperation::AdminCheckpoint,
        REQUEST_ADMIN_EXPLAIN_SQL => ProductOperation::AdminExplainSql {
            statement: decoder.text()?,
        },
        REQUEST_DOCTOR => ProductOperation::Doctor(
            DoctorRequest::new(std::path::PathBuf::from("."), 0)
                .map_err(|_| ProductCodecError::InvalidValue)?,
        ),
        REQUEST_BACKUP => {
            let mut request =
                BackupRequest::new(decoder.path()?).map_err(|_| ProductCodecError::InvalidValue)?;
            request.limits = BackupLimits {
                max_files: decoder.usize()?,
                max_directories: decoder.usize()?,
                max_total_bytes: decoder.u64()?,
                max_path_bytes: decoder.usize()?,
                max_manifest_bytes: decoder.u64()?,
            };
            ProductOperation::Backup(request)
        }
        REQUEST_TELEMETRY => ProductOperation::Telemetry,
        REQUEST_VERIFY_PROOF => ProductOperation::VerifyProof {
            proof: decoder.owned_bytes()?,
            witness: decoder.owned_bytes()?,
            trusted_anchor: decoder.array()?,
        },
        REQUEST_SEARCH_COLLECTION => {
            let (collection, request) = decode_search_collection(&mut decoder)?;
            ProductOperation::SearchCollection {
                collection,
                request,
            }
        }
        REQUEST_SEARCH_INGEST => ProductOperation::SearchIngest {
            collection: ObjectId::new(decoder.u128()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            batch: decode_search_ingest_batch(&mut decoder)?,
        },
        REQUEST_SEARCH_DOCUMENT_UPDATE => ProductOperation::SearchDocumentUpdate {
            collection: ObjectId::new(decoder.u128()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            update: ProductSearchDocumentUpdate {
                idempotency_id: decoder.u128()?,
                document: decode_product_document(&mut decoder)?,
            },
        },
        REQUEST_SEARCH_DOCUMENT_DELETE => ProductOperation::SearchDocumentDelete {
            collection: ObjectId::new(decoder.u128()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            delete: ProductSearchDocumentDelete {
                idempotency_id: decoder.u128()?,
                object_id: ObjectId::new(decoder.u128()?)
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            },
        },
        REQUEST_STRUCTURE_MUTATE => {
            let count = decoder.usize_u32()?;
            if count == 0 || count > 4096 {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut mutations = Vec::with_capacity(count);
            for _ in 0..count {
                mutations.push(decode_structure_mutation(&mut decoder)?);
            }
            ProductOperation::StructureMutate { mutations }
        }
        REQUEST_STRUCTURE_READ => {
            ProductOperation::StructureRead(decode_structure_read_request(&mut decoder)?)
        }
        REQUEST_RESTORE => {
            let backup = decoder.path()?;
            let destination = decoder.path()?;
            let mut request = RestoreRequest::new(backup, destination)
                .map_err(|_| ProductCodecError::InvalidValue)?;
            request.limits = BackupLimits {
                max_files: decoder.usize()?,
                max_directories: decoder.usize()?,
                max_total_bytes: decoder.u64()?,
                max_path_bytes: decoder.usize()?,
                max_manifest_bytes: decoder.u64()?,
            };
            request.doctor_logical_time_micros = decoder.i64()?;
            ProductOperation::Restore(request)
        }
        REQUEST_SECURITY_STATUS => ProductOperation::SecurityStatus,
        REQUEST_SECURITY_PRINCIPAL_LIST => ProductOperation::SecurityPrincipalList(
            SecurityPrincipalListRequest::new(
                decode_security_cursor(&mut decoder, SecurityCursorFamily::Principal)?,
                decoder.usize()?,
            )
            .map_err(|_| ProductCodecError::LimitExceeded)?,
        ),
        REQUEST_SECURITY_ROLE_LIST => ProductOperation::SecurityRoleList(
            SecurityRoleListRequest::new(
                decode_security_cursor(&mut decoder, SecurityCursorFamily::Role)?,
                decoder.usize()?,
            )
            .map_err(|_| ProductCodecError::LimitExceeded)?,
        ),
        REQUEST_SECURITY_ASSIGNMENT_LIST => ProductOperation::SecurityAssignmentList(
            SecurityAssignmentListRequest::new(
                decode_security_cursor(&mut decoder, SecurityCursorFamily::Assignment)?,
                decoder.usize()?,
            )
            .map_err(|_| ProductCodecError::LimitExceeded)?,
        ),
        REQUEST_SECURITY_KEY_LIST => ProductOperation::SecurityKeyList(
            SecurityKeyListRequest::new(
                decode_security_cursor(&mut decoder, SecurityCursorFamily::Key)?,
                decoder.usize()?,
            )
            .map_err(|_| ProductCodecError::LimitExceeded)?,
        ),
        REQUEST_SECURITY_AUDIT_READ => ProductOperation::SecurityAuditRead(
            SecurityAuditReadRequest::new(
                decode_optional_security_id(&mut decoder)?,
                decoder.usize()?,
            )
            .map_err(|_| ProductCodecError::LimitExceeded)?,
        ),
        REQUEST_SECURITY_PRINCIPAL_CREATE => ProductOperation::SecurityPrincipalCreate {
            display_name: decode_security_text(&mut decoder)?,
        },
        REQUEST_SECURITY_PRINCIPAL_SET_ENABLED => {
            let principal_id = decode_security_id(decoder.array()?)?;
            let enabled = decoder.u8()?;
            if enabled > 1 || decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            ProductOperation::SecurityPrincipalSetEnabled {
                principal_id,
                enabled: enabled == 1,
            }
        }
        REQUEST_SECURITY_CUSTOM_ROLE_CREATE => ProductOperation::SecurityCustomRoleCreate {
            display_name: decode_security_text(&mut decoder)?,
            grants: decode_custom_role_grants(&mut decoder)?,
        },
        REQUEST_SECURITY_BUILT_IN_ASSIGNMENT_CREATE => {
            let principal_id = decode_security_id(decoder.array()?)?;
            let role =
                BuiltInRole::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?;
            if decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            ProductOperation::SecurityBuiltInAssignmentCreate {
                principal_id,
                role,
                scope: decode_product_scope(&mut decoder)?,
            }
        }
        REQUEST_SECURITY_CUSTOM_ASSIGNMENT_CREATE => {
            ProductOperation::SecurityCustomAssignmentCreate {
                principal_id: decode_security_id(decoder.array()?)?,
                role_id: decode_security_id(decoder.array()?)?,
            }
        }
        REQUEST_SECURITY_ASSIGNMENT_REVOKE => ProductOperation::SecurityAssignmentRevoke {
            assignment_id: decode_security_id(decoder.array()?)?,
        },
        REQUEST_SECURITY_API_KEY_ISSUE_SELF_START | REQUEST_SECURITY_API_KEY_ISSUE_START => {
            let principal_id = decode_security_id(decoder.array()?)?;
            let label = decode_security_text(&mut decoder)?;
            let roles = decode_built_in_roles(&mut decoder)?;
            let custom_roles = decode_security_ids(&mut decoder)?;
            let permission_ceiling = ProductAuthorization::from_known_bits(decoder.u64()?)
                .ok_or(ProductCodecError::InvalidValue)?;
            let scope_ceiling = decode_product_scopes(&mut decoder)?;
            let expires_at_micros = decode_fixed_optional_i64(&mut decoder)?;
            if kind == REQUEST_SECURITY_API_KEY_ISSUE_SELF_START {
                ProductOperation::SecurityApiKeyIssueSelfStart {
                    principal_id,
                    label,
                    roles,
                    custom_roles,
                    permission_ceiling,
                    scope_ceiling,
                    expires_at_micros,
                }
            } else {
                ProductOperation::SecurityApiKeyIssueStart {
                    principal_id,
                    label,
                    roles,
                    custom_roles,
                    permission_ceiling,
                    scope_ceiling,
                    expires_at_micros,
                }
            }
        }
        REQUEST_SECURITY_API_KEY_ISSUE_SELF_ACTIVATE | REQUEST_SECURITY_API_KEY_ISSUE_ACTIVATE => {
            let key_id = decode_api_key_id(decoder.array()?)?;
            let confirmation_digest = ApiKeyConfirmationDigest::from_bytes(decoder.array()?);
            if kind == REQUEST_SECURITY_API_KEY_ISSUE_SELF_ACTIVATE {
                ProductOperation::SecurityApiKeyIssueSelfActivate {
                    key_id,
                    confirmation_digest,
                }
            } else {
                ProductOperation::SecurityApiKeyIssueActivate {
                    key_id,
                    confirmation_digest,
                }
            }
        }
        REQUEST_SECURITY_API_KEY_ROTATE_SELF_START | REQUEST_SECURITY_API_KEY_ROTATE_START => {
            let predecessor_key_id = decode_api_key_id(decoder.array()?)?;
            let label = decode_security_text(&mut decoder)?;
            let overlap_seconds = decoder.u64()?;
            let expires_at_micros = decode_fixed_optional_i64(&mut decoder)?;
            if kind == REQUEST_SECURITY_API_KEY_ROTATE_SELF_START {
                ProductOperation::SecurityApiKeyRotateSelfStart {
                    predecessor_key_id,
                    label,
                    overlap_seconds,
                    expires_at_micros,
                }
            } else {
                ProductOperation::SecurityApiKeyRotateStart {
                    predecessor_key_id,
                    label,
                    overlap_seconds,
                    expires_at_micros,
                }
            }
        }
        REQUEST_SECURITY_API_KEY_ROTATE_SELF_ACTIVATE
        | REQUEST_SECURITY_API_KEY_ROTATE_ACTIVATE => {
            let successor_key_id = decode_api_key_id(decoder.array()?)?;
            let confirmation_digest = ApiKeyConfirmationDigest::from_bytes(decoder.array()?);
            if kind == REQUEST_SECURITY_API_KEY_ROTATE_SELF_ACTIVATE {
                ProductOperation::SecurityApiKeyRotateSelfActivate {
                    successor_key_id,
                    confirmation_digest,
                }
            } else {
                ProductOperation::SecurityApiKeyRotateActivate {
                    successor_key_id,
                    confirmation_digest,
                }
            }
        }
        REQUEST_SECURITY_API_KEY_ISSUE_SELF_ABORT
        | REQUEST_SECURITY_API_KEY_ISSUE_ABORT
        | REQUEST_SECURITY_API_KEY_REVOKE_SELF
        | REQUEST_SECURITY_API_KEY_REVOKE => {
            let key_id = decode_api_key_id(decoder.array()?)?;
            match kind {
                REQUEST_SECURITY_API_KEY_ISSUE_SELF_ABORT => {
                    ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
                }
                REQUEST_SECURITY_API_KEY_ISSUE_ABORT => {
                    ProductOperation::SecurityApiKeyIssueAbort { key_id }
                }
                REQUEST_SECURITY_API_KEY_REVOKE_SELF => {
                    ProductOperation::SecurityApiKeyRevokeSelf { key_id }
                }
                REQUEST_SECURITY_API_KEY_REVOKE => {
                    ProductOperation::SecurityApiKeyRevoke { key_id }
                }
                _ => return Err(ProductCodecError::InvalidValue),
            }
        }
        REQUEST_SECURITY_API_KEY_ROTATE_SELF_ABORT | REQUEST_SECURITY_API_KEY_ROTATE_ABORT => {
            let successor_key_id = decode_api_key_id(decoder.array()?)?;
            if kind == REQUEST_SECURITY_API_KEY_ROTATE_SELF_ABORT {
                ProductOperation::SecurityApiKeyRotateSelfAbort { successor_key_id }
            } else {
                ProductOperation::SecurityApiKeyRotateAbort { successor_key_id }
            }
        }
        REQUEST_SECURITY_LEGACY_BEARER_REVOKE => ProductOperation::SecurityLegacyBearerRevoke,
        REQUEST_TRANSACTION_BEGIN => ProductOperation::TransactionBegin,
        REQUEST_TRANSACTION_STAGE_SQL => ProductOperation::TransactionStageSql {
            handle: decode_transaction_handle(&mut decoder)?,
            mutation: ProductTransactionSqlMutation {
                statement: decoder.text()?,
                parameters: decode_values(&mut decoder)?,
            },
        },
        REQUEST_TRANSACTION_STAGE_STRUCTURE => ProductOperation::TransactionStageStructure {
            handle: decode_transaction_handle(&mut decoder)?,
            mutation: decode_structure_mutation(&mut decoder)?,
        },
        REQUEST_TRANSACTION_STAGE_SEARCH => ProductOperation::TransactionStageSearch {
            handle: decode_transaction_handle(&mut decoder)?,
            mutation: decode_transaction_search_mutation(&mut decoder)?,
        },
        REQUEST_TRANSACTION_STAGE_VECTOR => ProductOperation::TransactionStageVector {
            handle: decode_transaction_handle(&mut decoder)?,
            mutation: decode_transaction_vector_mutation(&mut decoder)?,
        },
        REQUEST_TRANSACTION_COMMIT => ProductOperation::TransactionCommit {
            handle: decode_transaction_handle(&mut decoder)?,
        },
        REQUEST_TRANSACTION_ROLLBACK => ProductOperation::TransactionRollback {
            handle: decode_transaction_handle(&mut decoder)?,
        },
        REQUEST_EXPLICIT_TRANSACTION_STATUS => ProductOperation::ExplicitTransactionStatus {
            handle: decode_transaction_handle(&mut decoder)?,
        },
        REQUEST_PROVE => {
            let operation_kind = decoder.u16()?;
            if decoder.u16()? != 0 {
                return Err(ProductCodecError::Malformed);
            }
            let operation_body = decoder.owned_bytes()?;
            let operation = decode_operation(operation_kind, &operation_body)?;
            if matches!(operation, ProductOperation::Prove { .. }) || operation.is_key_lifecycle() {
                return Err(ProductCodecError::InvalidValue);
            }
            ProductOperation::Prove {
                operation: Box::new(operation),
                limits: decode_proof_generation_limits(&mut decoder)?,
            }
        }
        _ => return Err(ProductCodecError::Unsupported),
    };
    decoder.finish()?;
    Ok(operation)
}

#[derive(Clone, Copy)]
enum SecurityCursorFamily {
    Principal,
    Role,
    Assignment,
    Key,
}

fn encode_security_cursor(
    encoded: &mut Vec<u8>,
    cursor: Option<SecurityCursor>,
    family: SecurityCursorFamily,
) -> Result<(), ProductCodecError> {
    let Some(cursor) = cursor else {
        encoded.extend_from_slice(&[0; 40]);
        return Ok(());
    };
    if cursor.authorization_epoch().get() == 0 {
        return Err(ProductCodecError::InvalidValue);
    }
    encoded.push(1);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&cursor.authorization_epoch().get().to_le_bytes());
    let (kind, payload) = encode_security_cursor_id(cursor.after_id(), family)?;
    encoded.push(kind);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&payload);
    Ok(())
}

fn encode_security_cursor_id(
    id: SecurityCursorId,
    family: SecurityCursorFamily,
) -> Result<(u8, [u8; 16]), ProductCodecError> {
    match (family, id) {
        (SecurityCursorFamily::Principal, SecurityCursorId::Principal(id)) => {
            Ok((1, id.to_be_bytes()))
        }
        (SecurityCursorFamily::Role, SecurityCursorId::BuiltInRole(role)) => {
            let mut encoded = [0; 16];
            encoded[0] = role.tag();
            Ok((2, encoded))
        }
        (SecurityCursorFamily::Role, SecurityCursorId::CustomRole(id)) => Ok((3, id.to_be_bytes())),
        (SecurityCursorFamily::Assignment, SecurityCursorId::Assignment(id)) => {
            Ok((4, id.to_be_bytes()))
        }
        (SecurityCursorFamily::Key, SecurityCursorId::Key(id)) => Ok((5, *id.as_bytes())),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn decode_security_cursor(
    decoder: &mut Decoder<'_>,
    family: SecurityCursorFamily,
) -> Result<Option<SecurityCursor>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let epoch = decoder.u64()?;
    let kind = decoder.u8()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let payload = decoder.array()?;
    if present == 0 {
        if epoch != 0 || kind != 0 || payload != [0; 16] {
            return Err(ProductCodecError::Malformed);
        }
        return Ok(None);
    }
    if epoch == 0 {
        return Err(ProductCodecError::InvalidValue);
    }
    let after_id = decode_security_cursor_id(kind, payload, family)?;
    Ok(Some(SecurityCursor::new(
        AuthorizationEpoch::new(epoch),
        after_id,
    )))
}

fn decode_security_cursor_id(
    kind: u8,
    payload: [u8; 16],
    family: SecurityCursorFamily,
) -> Result<SecurityCursorId, ProductCodecError> {
    match (family, kind) {
        (SecurityCursorFamily::Principal, 1) => {
            Ok(SecurityCursorId::Principal(decode_security_id(payload)?))
        }
        (SecurityCursorFamily::Role, 2) => {
            if payload[1..] != [0; 15] {
                return Err(ProductCodecError::Malformed);
            }
            Ok(SecurityCursorId::BuiltInRole(
                BuiltInRole::from_tag(payload[0]).ok_or(ProductCodecError::InvalidValue)?,
            ))
        }
        (SecurityCursorFamily::Role, 3) => {
            Ok(SecurityCursorId::CustomRole(decode_security_id(payload)?))
        }
        (SecurityCursorFamily::Assignment, 4) => {
            Ok(SecurityCursorId::Assignment(decode_security_id(payload)?))
        }
        (SecurityCursorFamily::Key, 5) => Ok(SecurityCursorId::Key(
            ApiKeyId::from_bytes(payload).ok_or(ProductCodecError::InvalidValue)?,
        )),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_optional_security_id(encoded: &mut Vec<u8>, id: Option<SecurityId>) {
    encoded.push(u8::from(id.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&id.map_or([0; 16], SecurityId::to_be_bytes));
}

fn decode_optional_security_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<SecurityId>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let payload = decoder.array()?;
    if present == 0 {
        return if payload == [0; 16] {
            Ok(None)
        } else {
            Err(ProductCodecError::Malformed)
        };
    }
    decode_security_id(payload).map(Some)
}

fn decode_security_id(payload: [u8; 16]) -> Result<SecurityId, ProductCodecError> {
    SecurityId::new(u128::from_be_bytes(payload)).ok_or(ProductCodecError::InvalidValue)
}

fn encode_authorization_epoch(
    encoded: &mut Vec<u8>,
    epoch: AuthorizationEpoch,
) -> Result<(), ProductCodecError> {
    if epoch == AuthorizationEpoch::UNMANAGED {
        return Err(ProductCodecError::InvalidValue);
    }
    encoded.extend_from_slice(&epoch.get().to_le_bytes());
    Ok(())
}

fn decode_authorization_epoch(
    decoder: &mut Decoder<'_>,
) -> Result<AuthorizationEpoch, ProductCodecError> {
    let epoch = AuthorizationEpoch::new(decoder.u64()?);
    if epoch == AuthorizationEpoch::UNMANAGED {
        return Err(ProductCodecError::InvalidValue);
    }
    Ok(epoch)
}

fn encode_security_mutation_receipt(
    encoded: &mut Vec<u8>,
    id: SecurityId,
    authorization_epoch: AuthorizationEpoch,
    commit: ProductCommitReceipt,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&id.to_be_bytes());
    encode_authorization_epoch(encoded, authorization_epoch)?;
    encode_receipt(encoded, commit)
}

fn decode_security_mutation_receipt(
    decoder: &mut Decoder<'_>,
) -> Result<(SecurityId, AuthorizationEpoch, ProductCommitReceipt), ProductCodecError> {
    Ok((
        decode_security_id(decoder.array()?)?,
        decode_authorization_epoch(decoder)?,
        decode_receipt(decoder)?,
    ))
}

fn encode_security_status(
    encoded: &mut Vec<u8>,
    value: AccessControlStatus,
) -> Result<(), ProductCodecError> {
    value
        .validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    encoded.push(u8::from(value.bootstrapped));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&value.epoch.get().to_le_bytes());
    for count in [
        value.principals,
        value.assignments,
        value.custom_roles,
        value.custom_assignments,
        value.keys,
        value.pending_keys,
        value.audit_events,
    ] {
        put_u64(encoded, count)?;
    }
    Ok(())
}

fn decode_security_status(
    decoder: &mut Decoder<'_>,
) -> Result<AccessControlStatus, ProductCodecError> {
    let bootstrapped = decoder.boolean()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let value = AccessControlStatus {
        bootstrapped,
        epoch: AuthorizationEpoch::new(decoder.u64()?),
        principals: decoder.usize()?,
        assignments: decoder.usize()?,
        custom_roles: decoder.usize()?,
        custom_assignments: decoder.usize()?,
        keys: decoder.usize()?,
        pending_keys: decoder.usize()?,
        audit_events: decoder.usize()?,
    };
    value
        .validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    Ok(value)
}

fn encode_security_page_header(
    encoded: &mut Vec<u8>,
    authorization_epoch: AuthorizationEpoch,
    item_count: usize,
    next_cursor: Option<SecurityCursor>,
    family: SecurityCursorFamily,
) -> Result<(), ProductCodecError> {
    if authorization_epoch == AuthorizationEpoch::UNMANAGED
        || item_count > MAX_SECURITY_LIST_ROWS
        || next_cursor.is_some_and(|cursor| cursor.authorization_epoch() != authorization_epoch)
    {
        return Err(ProductCodecError::InvalidValue);
    }
    encoded.extend_from_slice(&authorization_epoch.get().to_le_bytes());
    put_u32(encoded, item_count)?;
    encoded.extend_from_slice(&[0; 4]);
    encode_security_cursor(encoded, next_cursor, family)
}

fn decode_security_page_header(
    decoder: &mut Decoder<'_>,
    family: SecurityCursorFamily,
) -> Result<(AuthorizationEpoch, usize, Option<SecurityCursor>), ProductCodecError> {
    let authorization_epoch = AuthorizationEpoch::new(decoder.u64()?);
    let item_count = decoder.usize_u32()?;
    if authorization_epoch == AuthorizationEpoch::UNMANAGED
        || item_count > MAX_SECURITY_LIST_ROWS
        || decoder.bytes(4)? != [0; 4]
    {
        return Err(ProductCodecError::InvalidValue);
    }
    let next_cursor = decode_security_cursor(decoder, family)?;
    if next_cursor.is_some_and(|cursor| cursor.authorization_epoch() != authorization_epoch) {
        return Err(ProductCodecError::InvalidValue);
    }
    Ok((authorization_epoch, item_count, next_cursor))
}

fn encode_security_principal_page(
    encoded: &mut Vec<u8>,
    page: &SecurityPrincipalPage,
) -> Result<(), ProductCodecError> {
    page.validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    encode_security_page_header(
        encoded,
        page.authorization_epoch(),
        page.items().len(),
        page.next_cursor(),
        SecurityCursorFamily::Principal,
    )?;
    for item in page.items() {
        encoded.extend_from_slice(&item.id().to_be_bytes());
        encoded.push(u8::from(item.enabled()));
        encoded.extend_from_slice(&[0; 7]);
        put_security_text(encoded, item.display_name())?;
    }
    Ok(())
}

fn decode_security_principal_page(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityPrincipalPage, ProductCodecError> {
    let (authorization_epoch, item_count, next_cursor) =
        decode_security_page_header(decoder, SecurityCursorFamily::Principal)?;
    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        let id = decode_security_id(decoder.array()?)?;
        let enabled = decoder.boolean()?;
        if decoder.bytes(7)? != [0; 7] {
            return Err(ProductCodecError::Malformed);
        }
        items.push(
            SecurityPrincipalSummary::new(id, decode_security_text(decoder)?, enabled)
                .map_err(|_| ProductCodecError::InvalidValue)?,
        );
    }
    SecurityPrincipalPage::try_from_wire(authorization_epoch, items, next_cursor)
        .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_security_role_page(
    encoded: &mut Vec<u8>,
    page: &SecurityRolePage,
) -> Result<(), ProductCodecError> {
    page.validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    encode_security_page_header(
        encoded,
        page.authorization_epoch(),
        page.items().len(),
        page.next_cursor(),
        SecurityCursorFamily::Role,
    )?;
    for item in page.items() {
        if let Some(role) = item.built_in_role() {
            encoded.push(0);
            encoded.push(role.tag());
            encoded.extend_from_slice(&[0; 6]);
        } else {
            encoded.push(1);
            encoded.extend_from_slice(&[0; 7]);
            encoded.extend_from_slice(
                &item
                    .custom_role_id()
                    .ok_or(ProductCodecError::InvalidValue)?
                    .to_be_bytes(),
            );
            put_security_text(encoded, item.display_name())?;
            encode_custom_role_grants(encoded, item.grants())?;
        }
    }
    Ok(())
}

fn decode_security_role_page(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityRolePage, ProductCodecError> {
    let (authorization_epoch, item_count, next_cursor) =
        decode_security_page_header(decoder, SecurityCursorFamily::Role)?;
    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        items.push(match decoder.u8()? {
            0 => {
                let role =
                    BuiltInRole::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?;
                if decoder.bytes(6)? != [0; 6] {
                    return Err(ProductCodecError::Malformed);
                }
                SecurityRoleSummary::built_in(role)
            }
            1 => {
                if decoder.bytes(7)? != [0; 7] {
                    return Err(ProductCodecError::Malformed);
                }
                let id = decode_security_id(decoder.array()?)?;
                let display_name = decode_security_text(decoder)?;
                let grants = decode_custom_role_grants(decoder)?;
                SecurityRoleSummary::custom(id, display_name, grants)
                    .map_err(|_| ProductCodecError::InvalidValue)?
            }
            _ => return Err(ProductCodecError::InvalidValue),
        });
    }
    SecurityRolePage::try_from_wire(authorization_epoch, items, next_cursor)
        .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_custom_role_grants(
    encoded: &mut Vec<u8>,
    grants: &[CustomRoleGrant],
) -> Result<(), ProductCodecError> {
    if grants.is_empty()
        || grants.len() > AccessControlLimits::V1.grants_per_role
        || !grants.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, grants.len())?;
    encoded.extend_from_slice(&[0; 4]);
    for grant in grants {
        encoded.push(grant.permission().tag());
        encoded.extend_from_slice(&[0; 7]);
        encode_product_scope(encoded, grant.scope());
    }
    Ok(())
}

fn decode_custom_role_grants(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<CustomRoleGrant>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count == 0 || count > AccessControlLimits::V1.grants_per_role || decoder.bytes(4)? != [0; 4]
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut grants = Vec::with_capacity(count);
    for _ in 0..count {
        let permission =
            ProductPermission::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?;
        if decoder.bytes(7)? != [0; 7] {
            return Err(ProductCodecError::Malformed);
        }
        grants.push(
            CustomRoleGrant::new(permission, decode_product_scope(decoder)?)
                .ok_or(ProductCodecError::InvalidValue)?,
        );
    }
    if !grants.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(ProductCodecError::InvalidValue);
    }
    Ok(grants)
}

fn encode_security_assignment_page(
    encoded: &mut Vec<u8>,
    page: &SecurityAssignmentPage,
) -> Result<(), ProductCodecError> {
    page.validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    encode_security_page_header(
        encoded,
        page.authorization_epoch(),
        page.items().len(),
        page.next_cursor(),
        SecurityCursorFamily::Assignment,
    )?;
    for item in page.items() {
        encoded.extend_from_slice(&item.id().to_be_bytes());
        encoded.extend_from_slice(&item.principal_id().to_be_bytes());
        match (item.built_in_role(), item.custom_role_id(), item.scope()) {
            (Some(role), None, Some(scope)) => {
                encoded.push(0);
                encoded.push(role.tag());
                encoded.extend_from_slice(&[0; 6]);
                encode_product_scope(encoded, scope);
            }
            (None, Some(role_id), None) => {
                encoded.push(1);
                encoded.extend_from_slice(&[0; 7]);
                encoded.extend_from_slice(&role_id.to_be_bytes());
            }
            _ => return Err(ProductCodecError::InvalidValue),
        }
    }
    Ok(())
}

fn decode_security_assignment_page(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityAssignmentPage, ProductCodecError> {
    let (authorization_epoch, item_count, next_cursor) =
        decode_security_page_header(decoder, SecurityCursorFamily::Assignment)?;
    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        let id = decode_security_id(decoder.array()?)?;
        let principal_id = decode_security_id(decoder.array()?)?;
        let (built_in_role, custom_role_id, scope) = match decoder.u8()? {
            0 => {
                let role =
                    BuiltInRole::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?;
                if decoder.bytes(6)? != [0; 6] {
                    return Err(ProductCodecError::Malformed);
                }
                (Some(role), None, Some(decode_product_scope(decoder)?))
            }
            1 => {
                if decoder.bytes(7)? != [0; 7] {
                    return Err(ProductCodecError::Malformed);
                }
                (None, Some(decode_security_id(decoder.array()?)?), None)
            }
            _ => return Err(ProductCodecError::InvalidValue),
        };
        items.push(
            SecurityAssignmentSummary::new(id, principal_id, built_in_role, custom_role_id, scope)
                .map_err(|_| ProductCodecError::InvalidValue)?,
        );
    }
    SecurityAssignmentPage::try_from_wire(authorization_epoch, items, next_cursor)
        .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_security_key_page(
    encoded: &mut Vec<u8>,
    page: &SecurityKeyPage,
) -> Result<(), ProductCodecError> {
    page.validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    encode_security_page_header(
        encoded,
        page.authorization_epoch(),
        page.items().len(),
        page.next_cursor(),
        SecurityCursorFamily::Key,
    )?;
    for item in page.items() {
        encode_security_key_summary(encoded, item)?;
    }
    Ok(())
}

fn decode_security_key_page(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityKeyPage, ProductCodecError> {
    let (authorization_epoch, item_count, next_cursor) =
        decode_security_page_header(decoder, SecurityCursorFamily::Key)?;
    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        items.push(decode_security_key_summary(decoder)?);
    }
    SecurityKeyPage::try_from_wire(authorization_epoch, items, next_cursor)
        .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_security_key_summary(
    encoded: &mut Vec<u8>,
    item: &SecurityKeySummary,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(item.id().as_bytes());
    encoded.extend_from_slice(&item.principal_id().to_be_bytes());
    encoded.push(u8::from(item.active()) | (u8::from(item.revoked()) << 1));
    encoded.extend_from_slice(&[0; 7]);
    put_security_text(encoded, item.label())?;
    encode_built_in_roles(encoded, item.roles())?;
    encode_security_ids(encoded, item.custom_roles())?;
    encoded.extend_from_slice(&item.permission_ceiling().bits().to_le_bytes());
    encode_product_scopes(encoded, item.scope_ceiling())?;
    encoded.extend_from_slice(&item.created_at_micros().to_le_bytes());
    encode_fixed_optional_i64(encoded, item.expires_at_micros());
    encoded.extend_from_slice(&item.published_epoch().get().to_le_bytes());
    encode_optional_api_key_id(encoded, item.predecessor_id());
    encode_optional_api_key_id(encoded, item.successor_id());
    encode_fixed_optional_i64(encoded, item.overlap_until_micros());
    encode_optional_u64(encoded, item.rotation_overlap_micros());
    Ok(())
}

fn decode_security_key_summary(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityKeySummary, ProductCodecError> {
    let id = decode_api_key_id(decoder.array()?)?;
    let principal_id = decode_security_id(decoder.array()?)?;
    let flags = decoder.u8()?;
    if flags & !3 != 0 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let label = decode_security_text(decoder)?;
    let roles = decode_built_in_roles(decoder)?;
    let custom_roles = decode_security_ids(decoder)?;
    let permission_ceiling = ProductAuthorization::from_known_bits(decoder.u64()?)
        .ok_or(ProductCodecError::InvalidValue)?;
    let scope_ceiling = decode_product_scopes(decoder)?;
    let created_at_micros = decoder.i64()?;
    let expires_at_micros = decode_fixed_optional_i64(decoder)?;
    let published_epoch = AuthorizationEpoch::new(decoder.u64()?);
    let predecessor_id = decode_optional_api_key_id(decoder)?;
    let successor_id = decode_optional_api_key_id(decoder)?;
    let overlap_until_micros = decode_fixed_optional_i64(decoder)?;
    let rotation_overlap_micros = decode_optional_u64(decoder)?;
    SecurityKeySummary::try_from_wire(SecurityKeySummaryInput {
        id,
        principal_id,
        label,
        active: flags & 1 != 0,
        roles,
        custom_roles,
        permission_ceiling,
        scope_ceiling,
        created_at_micros,
        expires_at_micros,
        revoked: flags & 2 != 0,
        published_epoch,
        predecessor_id,
        successor_id,
        overlap_until_micros,
        rotation_overlap_micros,
    })
    .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_built_in_roles(
    encoded: &mut Vec<u8>,
    roles: &[BuiltInRole],
) -> Result<(), ProductCodecError> {
    if roles.len() > 7 {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, roles.len())?;
    encoded.extend_from_slice(&[0; 4]);
    encoded.extend(roles.iter().map(|role| role.tag()));
    Ok(())
}

fn decode_built_in_roles(decoder: &mut Decoder<'_>) -> Result<Vec<BuiltInRole>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count > 7 || decoder.bytes(4)? != [0; 4] {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut roles = Vec::with_capacity(count);
    for _ in 0..count {
        roles.push(BuiltInRole::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?);
    }
    Ok(roles)
}

fn encode_security_ids(encoded: &mut Vec<u8>, ids: &[SecurityId]) -> Result<(), ProductCodecError> {
    if ids.len() > AccessControlLimits::V1.assignments_per_principal {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, ids.len())?;
    encoded.extend_from_slice(&[0; 4]);
    for id in ids {
        encoded.extend_from_slice(&id.to_be_bytes());
    }
    Ok(())
}

fn decode_security_ids(decoder: &mut Decoder<'_>) -> Result<Vec<SecurityId>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count > AccessControlLimits::V1.assignments_per_principal || decoder.bytes(4)? != [0; 4] {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        ids.push(decode_security_id(decoder.array()?)?);
    }
    Ok(ids)
}

fn encode_product_scopes(
    encoded: &mut Vec<u8>,
    scopes: &[ProductScope],
) -> Result<(), ProductCodecError> {
    if scopes.is_empty() || scopes.len() > AccessControlLimits::V1.assignments_per_principal {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, scopes.len())?;
    encoded.extend_from_slice(&[0; 4]);
    for scope in scopes {
        encode_product_scope(encoded, *scope);
    }
    Ok(())
}

fn decode_product_scopes(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<ProductScope>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count == 0
        || count > AccessControlLimits::V1.assignments_per_principal
        || decoder.bytes(4)? != [0; 4]
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut scopes = Vec::with_capacity(count);
    for _ in 0..count {
        scopes.push(decode_product_scope(decoder)?);
    }
    Ok(scopes)
}

fn encode_product_scope(encoded: &mut Vec<u8>, scope: ProductScope) {
    let (kind, id) = match scope {
        ProductScope::Instance => (0, [0; 16]),
        ProductScope::CatalogSubtree(id) => (1, id.get().to_le_bytes()),
        ProductScope::CatalogObject(id) => (2, id.get().to_le_bytes()),
    };
    encoded.push(kind);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&id);
}

fn decode_product_scope(decoder: &mut Decoder<'_>) -> Result<ProductScope, ProductCodecError> {
    let kind = decoder.u8()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let payload = decoder.array()?;
    match kind {
        0 if payload == [0; 16] => Ok(ProductScope::Instance),
        0 => Err(ProductCodecError::Malformed),
        1 => Ok(ProductScope::CatalogSubtree(
            ObjectId::new(u128::from_le_bytes(payload))
                .map_err(|_| ProductCodecError::InvalidValue)?,
        )),
        2 => Ok(ProductScope::CatalogObject(
            ObjectId::new(u128::from_le_bytes(payload))
                .map_err(|_| ProductCodecError::InvalidValue)?,
        )),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_security_audit_page(
    encoded: &mut Vec<u8>,
    page: &SecurityAuditPage,
) -> Result<(), ProductCodecError> {
    page.validate()
        .map_err(|_| ProductCodecError::InvalidValue)?;
    if page.events.len() > AccessControlLimits::V1.audit_result_rows {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, page.events.len())?;
    encoded.extend_from_slice(&[0; 4]);
    encode_optional_security_id(encoded, page.next_cursor);
    for event in &page.events {
        encode_security_audit_event(encoded, event)?;
    }
    Ok(())
}

fn decode_security_audit_page(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityAuditPage, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count > AccessControlLimits::V1.audit_result_rows || decoder.bytes(4)? != [0; 4] {
        return Err(ProductCodecError::LimitExceeded);
    }
    let next_cursor = decode_optional_security_id(decoder)?;
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        events.push(decode_security_audit_event(decoder)?);
    }
    SecurityAuditPage::try_from_wire(events, next_cursor)
        .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_security_audit_event(
    encoded: &mut Vec<u8>,
    event: &SecurityAuditEvent,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&event.id().to_be_bytes());
    encoded.extend_from_slice(&event.commit_csn().to_le_bytes());
    let has_actor = event.actor_principal_id().is_some() && event.actor_key_id().is_some();
    if event.actor_principal_id().is_some() != event.actor_key_id().is_some() {
        return Err(ProductCodecError::InvalidValue);
    }
    encoded.push(u8::from(has_actor));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(
        &event
            .actor_principal_id()
            .map_or([0; 16], SecurityId::to_be_bytes),
    );
    encoded.extend_from_slice(&event.actor_key_id().map_or([0; 16], |id| *id.as_bytes()));
    encoded.push(event.action().tag());
    encoded.push(match event.result() {
        SecurityAuditResult::Succeeded => 0,
    });
    encoded.extend_from_slice(&[0; 6]);
    encode_security_audit_targets(encoded, event.targets())?;
    encode_security_audit_metadata(encoded, event.metadata())?;
    Ok(())
}

fn decode_security_audit_event(
    decoder: &mut Decoder<'_>,
) -> Result<SecurityAuditEvent, ProductCodecError> {
    let id = decode_security_id(decoder.array()?)?;
    let commit_csn = decoder.u64()?;
    let has_actor = decoder.boolean()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let actor_principal = decoder.array()?;
    let actor_key = decoder.array()?;
    let (actor_principal_id, actor_key_id) = if has_actor {
        (
            Some(decode_security_id(actor_principal)?),
            Some(decode_api_key_id(actor_key)?),
        )
    } else if actor_principal == [0; 16] && actor_key == [0; 16] {
        (None, None)
    } else {
        return Err(ProductCodecError::Malformed);
    };
    let action =
        SecurityAuditAction::from_tag(decoder.u8()?).ok_or(ProductCodecError::InvalidValue)?;
    let result = match decoder.u8()? {
        0 => SecurityAuditResult::Succeeded,
        _ => return Err(ProductCodecError::InvalidValue),
    };
    if decoder.bytes(6)? != [0; 6] {
        return Err(ProductCodecError::Malformed);
    }
    let targets = decode_security_audit_targets(decoder)?;
    let metadata = decode_security_audit_metadata(decoder)?;
    SecurityAuditEvent::try_from_wire(
        id,
        commit_csn,
        actor_principal_id,
        actor_key_id,
        action,
        result,
        targets,
        metadata,
    )
    .map_err(|_| ProductCodecError::InvalidValue)
}

fn encode_security_audit_targets(
    encoded: &mut Vec<u8>,
    targets: &[SecurityAuditTarget],
) -> Result<(), ProductCodecError> {
    if targets.is_empty() || targets.len() > AccessControlLimits::V1.assignments_per_principal {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, targets.len())?;
    encoded.extend_from_slice(&[0; 4]);
    for target in targets {
        let (kind, id) = match target {
            SecurityAuditTarget::Principal(id) => (0, id.to_be_bytes()),
            SecurityAuditTarget::Role(id) => (1, id.to_be_bytes()),
            SecurityAuditTarget::Assignment(id) => (2, id.to_be_bytes()),
            SecurityAuditTarget::Key(id) => (3, *id.as_bytes()),
            SecurityAuditTarget::LegacyBearer => (4, [0; 16]),
        };
        encoded.push(kind);
        encoded.extend_from_slice(&[0; 7]);
        encoded.extend_from_slice(&id);
    }
    Ok(())
}

fn decode_security_audit_targets(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<SecurityAuditTarget>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count == 0
        || count > AccessControlLimits::V1.assignments_per_principal
        || decoder.bytes(4)? != [0; 4]
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut targets = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = decoder.u8()?;
        if decoder.bytes(7)? != [0; 7] {
            return Err(ProductCodecError::Malformed);
        }
        let id = decoder.array()?;
        targets.push(match kind {
            0 => SecurityAuditTarget::Principal(decode_security_id(id)?),
            1 => SecurityAuditTarget::Role(decode_security_id(id)?),
            2 => SecurityAuditTarget::Assignment(decode_security_id(id)?),
            3 => SecurityAuditTarget::Key(decode_api_key_id(id)?),
            4 if id == [0; 16] => SecurityAuditTarget::LegacyBearer,
            _ => return Err(ProductCodecError::InvalidValue),
        });
    }
    Ok(targets)
}

fn encode_security_audit_metadata(
    encoded: &mut Vec<u8>,
    metadata: &[SecurityAuditMetadata],
) -> Result<(), ProductCodecError> {
    if metadata.len() > AccessControlLimits::V1.assignments_per_principal {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_u32(encoded, metadata.len())?;
    encoded.extend_from_slice(&[0; 4]);
    for value in metadata {
        let (kind, instant) = match value {
            SecurityAuditMetadata::ExpiresAtMicros(instant) => (0, *instant),
            SecurityAuditMetadata::RotationOverlapUntilMicros(instant) => (1, *instant),
        };
        encoded.push(kind);
        encoded.extend_from_slice(&[0; 7]);
        encoded.extend_from_slice(&instant.to_le_bytes());
    }
    Ok(())
}

fn decode_security_audit_metadata(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<SecurityAuditMetadata>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count > AccessControlLimits::V1.assignments_per_principal || decoder.bytes(4)? != [0; 4] {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut metadata = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = decoder.u8()?;
        if decoder.bytes(7)? != [0; 7] {
            return Err(ProductCodecError::Malformed);
        }
        let instant = decoder.i64()?;
        metadata.push(match kind {
            0 => SecurityAuditMetadata::ExpiresAtMicros(instant),
            1 => SecurityAuditMetadata::RotationOverlapUntilMicros(instant),
            _ => return Err(ProductCodecError::InvalidValue),
        });
    }
    Ok(metadata)
}

fn put_security_text(encoded: &mut Vec<u8>, value: &str) -> Result<(), ProductCodecError> {
    if value.is_empty()
        || value.len() > hyphae_native_product::MAX_SECURITY_DISPLAY_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    put_text(encoded, value)
}

fn decode_security_text(decoder: &mut Decoder<'_>) -> Result<String, ProductCodecError> {
    let value = decoder.text()?;
    if value.is_empty()
        || value.len() > hyphae_native_product::MAX_SECURITY_DISPLAY_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProductCodecError::LimitExceeded);
    }
    Ok(value)
}

fn decode_api_key_id(payload: [u8; 16]) -> Result<ApiKeyId, ProductCodecError> {
    ApiKeyId::from_bytes(payload).ok_or(ProductCodecError::InvalidValue)
}

fn encode_optional_api_key_id(encoded: &mut Vec<u8>, id: Option<ApiKeyId>) {
    encoded.push(u8::from(id.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&id.map_or([0; 16], |id| *id.as_bytes()));
}

fn decode_optional_api_key_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<ApiKeyId>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let payload = decoder.array()?;
    if present == 0 {
        return if payload == [0; 16] {
            Ok(None)
        } else {
            Err(ProductCodecError::Malformed)
        };
    }
    decode_api_key_id(payload).map(Some)
}

fn encode_fixed_optional_i64(encoded: &mut Vec<u8>, value: Option<i64>) {
    encoded.push(u8::from(value.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&value.unwrap_or(0).to_le_bytes());
}

fn decode_fixed_optional_i64(decoder: &mut Decoder<'_>) -> Result<Option<i64>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let value = decoder.i64()?;
    if present == 0 {
        return if value == 0 {
            Ok(None)
        } else {
            Err(ProductCodecError::Malformed)
        };
    }
    Ok(Some(value))
}

fn encode_optional_u64(encoded: &mut Vec<u8>, value: Option<u64>) {
    encoded.push(u8::from(value.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&value.unwrap_or(0).to_le_bytes());
}

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let value = decoder.u64()?;
    if present == 0 {
        return if value == 0 {
            Ok(None)
        } else {
            Err(ProductCodecError::Malformed)
        };
    }
    Ok(Some(value))
}

fn encode_proof_generation_limits(
    encoded: &mut Vec<u8>,
    limits: NativeProofGenerationLimits,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&limits.admitted.result_items.to_le_bytes());
    encoded.extend_from_slice(&limits.admitted.candidate_items.to_le_bytes());
    encoded.extend_from_slice(&limits.admitted.evidence_bytes.to_le_bytes());
    encoded.extend_from_slice(&limits.proof.max_proof_bytes.to_le_bytes());
    encoded.extend_from_slice(&limits.proof.max_section_bytes.to_le_bytes());
    encoded.extend_from_slice(&limits.proof.max_decoded_bytes.to_le_bytes());
    put_u64(encoded, limits.proof.max_objects)?;
    put_u64(encoded, limits.proof.max_hybrid_branches)?;
    encoded.extend_from_slice(&limits.witness.max_witness_bytes.to_le_bytes());
    put_u64(encoded, limits.witness.max_entries)?;
    put_u64(encoded, limits.witness.max_files)?;
    put_u64(encoded, limits.witness.max_directories)?;
    put_u64(encoded, limits.witness.max_path_bytes)?;
    encoded.extend_from_slice(&limits.witness.max_file_bytes.to_le_bytes());
    encoded.extend_from_slice(&limits.witness.max_total_file_bytes.to_le_bytes());
    encoded.extend_from_slice(&limits.witness.max_decoded_bytes.to_le_bytes());
    Ok(())
}

fn decode_proof_generation_limits(
    decoder: &mut Decoder<'_>,
) -> Result<NativeProofGenerationLimits, ProductCodecError> {
    Ok(NativeProofGenerationLimits {
        admitted: AdmittedProofLimits {
            result_items: decoder.u64()?,
            candidate_items: decoder.u64()?,
            evidence_bytes: decoder.u64()?,
        },
        proof: ProofCodecLimits {
            max_proof_bytes: decoder.u64()?,
            max_section_bytes: decoder.u64()?,
            max_decoded_bytes: decoder.u64()?,
            max_objects: decoder.usize()?,
            max_hybrid_branches: decoder.usize()?,
        },
        witness: WitnessCodecLimits {
            max_witness_bytes: decoder.u64()?,
            max_entries: decoder.usize()?,
            max_files: decoder.usize()?,
            max_directories: decoder.usize()?,
            max_path_bytes: decoder.usize()?,
            max_file_bytes: decoder.u64()?,
            max_total_file_bytes: decoder.u64()?,
            max_decoded_bytes: decoder.u64()?,
        },
    })
}

fn encode_values(encoded: &mut Vec<u8>, values: &[ProductValue]) -> Result<(), ProductCodecError> {
    put_u32(encoded, values.len())?;
    for value in values {
        encode_value(encoded, value, 0)?;
    }
    Ok(())
}

fn decode_values(decoder: &mut Decoder<'_>) -> Result<Vec<ProductValue>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    if count > 4096 {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decode_value(decoder, 0)?);
    }
    Ok(values)
}

fn encode_value(
    encoded: &mut Vec<u8>,
    value: &ProductValue,
    depth: usize,
) -> Result<(), ProductCodecError> {
    if depth > 8 {
        return Err(ProductCodecError::LimitExceeded);
    }
    match value {
        ProductValue::Null => encoded.push(0),
        ProductValue::Boolean(value) => {
            encoded.push(1);
            encoded.push(u8::from(*value));
        }
        ProductValue::Signed(value) => {
            encoded.push(2);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Unsigned(value) => {
            encoded.push(3);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Decimal(value) => {
            encoded.push(4);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Float32(value) => {
            encoded.push(5);
            encoded.extend_from_slice(&value.bits().to_le_bytes());
        }
        ProductValue::Float64(value) => {
            encoded.push(6);
            encoded.extend_from_slice(&value.bits().to_le_bytes());
        }
        ProductValue::Text(value) => {
            encoded.push(7);
            put_text(encoded, value)?;
        }
        ProductValue::Binary(value) => {
            encoded.push(8);
            put_bytes(encoded, value)?;
        }
        ProductValue::Date(value) => {
            encoded.push(9);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Time(value) => {
            encoded.push(10);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Timestamp(value) => {
            encoded.push(11);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductValue::Interval {
            months,
            days,
            nanoseconds,
        } => {
            encoded.push(12);
            encoded.extend_from_slice(&months.to_le_bytes());
            encoded.extend_from_slice(&days.to_le_bytes());
            encoded.extend_from_slice(&nanoseconds.to_le_bytes());
        }
        ProductValue::Uuid(value) => {
            encoded.push(13);
            encoded.extend_from_slice(value);
        }
        ProductValue::Array(values) => {
            encoded.push(14);
            encode_values(encoded, values)?;
        }
        ProductValue::Map(entries) => {
            encoded.push(15);
            put_u32(encoded, entries.len())?;
            for (key, value) in entries {
                encode_value(encoded, key, depth + 1)?;
                encode_value(encoded, value, depth + 1)?;
            }
        }
        ProductValue::Vector(values) => {
            encoded.push(16);
            put_u32(encoded, values.len())?;
            for value in values {
                encoded.extend_from_slice(&value.bits().to_le_bytes());
            }
        }
        ProductValue::Json(value) => {
            encoded.push(17);
            put_text(encoded, value)?;
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

fn decode_value(
    decoder: &mut Decoder<'_>,
    depth: usize,
) -> Result<ProductValue, ProductCodecError> {
    if depth > 8 {
        return Err(ProductCodecError::LimitExceeded);
    }
    Ok(match decoder.u8()? {
        0 => ProductValue::Null,
        1 => match decoder.u8()? {
            0 => ProductValue::Boolean(false),
            1 => ProductValue::Boolean(true),
            _ => return Err(ProductCodecError::InvalidValue),
        },
        2 => ProductValue::Signed(decoder.i64()?),
        3 => ProductValue::Unsigned(decoder.u64()?),
        4 => ProductValue::Decimal(decoder.i128()?),
        5 => ProductValue::Float32(hyphae_native_product::CanonicalF32::new(f32::from_bits(
            decoder.u32()?,
        ))),
        6 => ProductValue::Float64(hyphae_native_product::CanonicalF64::new(f64::from_bits(
            decoder.u64()?,
        ))),
        7 => ProductValue::Text(decoder.text()?),
        8 => ProductValue::Binary(decoder.owned_bytes()?),
        9 => ProductValue::Date(decoder.i32()?),
        10 => ProductValue::Time(decoder.u64()?),
        11 => ProductValue::Timestamp(decoder.i64()?),
        12 => ProductValue::Interval {
            months: decoder.i32()?,
            days: decoder.i32()?,
            nanoseconds: decoder.i64()?,
        },
        13 => ProductValue::Uuid(decoder.array()?),
        14 => ProductValue::Array(decode_values(decoder)?),
        15 => {
            let count = decoder.usize_u32()?;
            if count > 4096 {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push((
                    decode_value(decoder, depth + 1)?,
                    decode_value(decoder, depth + 1)?,
                ));
            }
            ProductValue::Map(entries)
        }
        16 => {
            let count = decoder.usize_u32()?;
            if count > 4096 {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(hyphae_native_product::CanonicalF32::new(f32::from_bits(
                    decoder.u32()?,
                )));
            }
            ProductValue::Vector(values)
        }
        17 => ProductValue::Json(decoder.text()?),
        _ => return Err(ProductCodecError::Unsupported),
    })
}

#[allow(clippy::too_many_lines)]
fn encode_structure_mutation(
    encoded: &mut Vec<u8>,
    mutation: &ProductStructureMutation,
) -> Result<(), ProductCodecError> {
    match mutation {
        ProductStructureMutation::StringSet {
            key,
            value,
            expires_at_micros,
        } => {
            encoded.push(0);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, value)?;
            encode_optional_i64(encoded, *expires_at_micros);
        }
        ProductStructureMutation::StringDelete { key } => {
            encoded.push(1);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureMutation::CounterAdd { key, delta } => {
            encoded.push(2);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&delta.to_le_bytes());
        }
        ProductStructureMutation::Create { key, family } => {
            encoded.push(3);
            encode_structure_key(encoded, key)?;
            encoded.push(*family as u8);
        }
        ProductStructureMutation::Delete { key, family } => {
            encoded.push(4);
            encode_structure_key(encoded, key)?;
            encoded.push(*family as u8);
        }
        ProductStructureMutation::Expire {
            key,
            family,
            expires_at_micros,
        } => {
            encoded.push(5);
            encode_structure_key(encoded, key)?;
            encoded.push(*family as u8);
            encoded.extend_from_slice(&expires_at_micros.to_le_bytes());
        }
        ProductStructureMutation::HashSet { key, field, value } => {
            encoded.push(6);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
            put_bytes(encoded, value)?;
        }
        ProductStructureMutation::HashDelete { key, field } => {
            encoded.push(7);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
        }
        ProductStructureMutation::HashCounterAdd { key, field, delta } => {
            encoded.push(8);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
            encoded.extend_from_slice(&delta.to_le_bytes());
        }
        ProductStructureMutation::HashExpireField {
            key,
            field,
            expires_at_micros,
        } => {
            encoded.push(9);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
            encoded.extend_from_slice(&expires_at_micros.to_le_bytes());
        }
        ProductStructureMutation::ListPush { key, side, value } => {
            encoded.push(10);
            encode_structure_key(encoded, key)?;
            encoded.push(list_side_tag(*side));
            put_bytes(encoded, value)?;
        }
        ProductStructureMutation::ListPop { key, side } => {
            encoded.push(11);
            encode_structure_key(encoded, key)?;
            encoded.push(list_side_tag(*side));
        }
        ProductStructureMutation::SetAdd { key, member } => {
            encoded.push(12);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
        }
        ProductStructureMutation::SetRemove { key, member } => {
            encoded.push(13);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
        }
        ProductStructureMutation::SortedSetAdd { key, score, member } => {
            encoded.push(14);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&score.bits().to_le_bytes());
            put_bytes(encoded, member)?;
        }
        ProductStructureMutation::SortedSetRemove { key, member } => {
            encoded.push(15);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
        }
        ProductStructureMutation::SortedSetIncrement { key, delta, member } => {
            encoded.push(17);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&delta.bits().to_le_bytes());
            put_bytes(encoded, member)?;
        }
        ProductStructureMutation::SortedSetPop { key, highest } => {
            encoded.push(18);
            encode_structure_key(encoded, key)?;
            encoded.push(u8::from(*highest));
        }
        ProductStructureMutation::StringSetConditional {
            key,
            value,
            expires_at_micros,
            if_present,
        } => {
            encoded.push(19);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, value)?;
            encode_optional_i64(encoded, *expires_at_micros);
            encoded.push(u8::from(*if_present));
        }
        ProductStructureMutation::StringAppend { key, suffix } => {
            encoded.push(20);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, suffix)?;
        }
        ProductStructureMutation::StringSetRange { key, offset, patch } => {
            encoded.push(21);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&offset.to_le_bytes());
            put_bytes(encoded, patch)?;
        }
        ProductStructureMutation::HashSetIfAbsent { key, field, value } => {
            encoded.push(22);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
            put_bytes(encoded, value)?;
        }
        ProductStructureMutation::SetPop { key, seed } => {
            encoded.push(23);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&seed.to_le_bytes());
        }
        ProductStructureMutation::StreamAdd { key, fields } => {
            encoded.push(16);
            encode_structure_key(encoded, key)?;
            put_u32(encoded, fields.len())?;
            for entry in fields {
                put_bytes(encoded, &entry.field)?;
                put_bytes(encoded, &entry.value)?;
            }
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

fn decode_structure_mutation(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureMutation, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => ProductStructureMutation::StringSet {
            key: decode_structure_key(decoder)?,
            value: decoder.owned_bytes()?,
            expires_at_micros: decode_optional_i64(decoder)?,
        },
        1 => ProductStructureMutation::StringDelete {
            key: decode_structure_key(decoder)?,
        },
        2 => ProductStructureMutation::CounterAdd {
            key: decode_structure_key(decoder)?,
            delta: decoder.i64()?,
        },
        3 => ProductStructureMutation::Create {
            key: decode_structure_key(decoder)?,
            family: decode_structure_kind(decoder.u8()?)?,
        },
        4 => ProductStructureMutation::Delete {
            key: decode_structure_key(decoder)?,
            family: decode_structure_kind(decoder.u8()?)?,
        },
        5 => ProductStructureMutation::Expire {
            key: decode_structure_key(decoder)?,
            family: decode_structure_kind(decoder.u8()?)?,
            expires_at_micros: decoder.i64()?,
        },
        6 => ProductStructureMutation::HashSet {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
            value: decoder.owned_bytes()?,
        },
        7 => ProductStructureMutation::HashDelete {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
        },
        8 => ProductStructureMutation::HashCounterAdd {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
            delta: decoder.i64()?,
        },
        9 => ProductStructureMutation::HashExpireField {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
            expires_at_micros: decoder.i64()?,
        },
        10 => ProductStructureMutation::ListPush {
            key: decode_structure_key(decoder)?,
            side: decode_list_side(decoder.u8()?)?,
            value: decoder.owned_bytes()?,
        },
        11 => ProductStructureMutation::ListPop {
            key: decode_structure_key(decoder)?,
            side: decode_list_side(decoder.u8()?)?,
        },
        12 => ProductStructureMutation::SetAdd {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
        },
        13 => ProductStructureMutation::SetRemove {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
        },
        14 => ProductStructureMutation::SortedSetAdd {
            key: decode_structure_key(decoder)?,
            score: hyphae_native_product::CanonicalF64::new(f64::from_bits(decoder.u64()?)),
            member: decoder.owned_bytes()?,
        },
        15 => ProductStructureMutation::SortedSetRemove {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
        },
        16 => {
            let key = decode_structure_key(decoder)?;
            let count = decoder.usize_u32()?;
            if count == 0 || count > 4096 {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                fields.push(ProductHashEntry {
                    field: decoder.owned_bytes()?,
                    value: decoder.owned_bytes()?,
                });
            }
            ProductStructureMutation::StreamAdd { key, fields }
        }
        tag @ (17 | 18) => decode_sorted_set_value_mutation(decoder, tag)?,
        tag @ 19..=23 => decode_conditional_value_mutation(decoder, tag)?,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

/// Decodes the minor-6 conditional and range mutations (tags 19-23).
fn decode_conditional_value_mutation(
    decoder: &mut Decoder<'_>,
    tag: u8,
) -> Result<ProductStructureMutation, ProductCodecError> {
    Ok(match tag {
        19 => ProductStructureMutation::StringSetConditional {
            key: decode_structure_key(decoder)?,
            value: decoder.owned_bytes()?,
            expires_at_micros: decode_optional_i64(decoder)?,
            if_present: match decoder.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ProductCodecError::InvalidValue),
            },
        },
        20 => ProductStructureMutation::StringAppend {
            key: decode_structure_key(decoder)?,
            suffix: decoder.owned_bytes()?,
        },
        21 => ProductStructureMutation::StringSetRange {
            key: decode_structure_key(decoder)?,
            offset: decoder.u32()?,
            patch: decoder.owned_bytes()?,
        },
        22 => ProductStructureMutation::HashSetIfAbsent {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
            value: decoder.owned_bytes()?,
        },
        23 => ProductStructureMutation::SetPop {
            key: decode_structure_key(decoder)?,
            seed: decoder.u64()?,
        },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

/// Decodes the minor-6 sorted-set mutations that return typed values.
fn decode_sorted_set_value_mutation(
    decoder: &mut Decoder<'_>,
    tag: u8,
) -> Result<ProductStructureMutation, ProductCodecError> {
    Ok(match tag {
        17 => ProductStructureMutation::SortedSetIncrement {
            key: decode_structure_key(decoder)?,
            delta: hyphae_native_product::CanonicalF64::new(f64::from_bits(decoder.u64()?)),
            member: decoder.owned_bytes()?,
        },
        18 => ProductStructureMutation::SortedSetPop {
            key: decode_structure_key(decoder)?,
            highest: match decoder.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ProductCodecError::InvalidValue),
            },
        },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn encode_structure_key(
    encoded: &mut Vec<u8>,
    key: &ProductStructureKey,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&key.keyspace.get().to_le_bytes());
    put_bytes(encoded, &key.key)
}

fn decode_structure_key(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureKey, ProductCodecError> {
    Ok(ProductStructureKey {
        keyspace: ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?,
        key: decoder.owned_bytes()?,
    })
}

fn encode_optional_i64(encoded: &mut Vec<u8>, value: Option<i64>) {
    encoded.push(u8::from(value.is_some()));
    if let Some(value) = value {
        encoded.extend_from_slice(&value.to_le_bytes());
    }
}

fn decode_optional_i64(decoder: &mut Decoder<'_>) -> Result<Option<i64>, ProductCodecError> {
    decoder.boolean()?.then(|| decoder.i64()).transpose()
}

const fn list_side_tag(side: ProductListSide) -> u8 {
    match side {
        ProductListSide::Left => 0,
        ProductListSide::Right => 1,
    }
}

fn decode_list_side(value: u8) -> Result<ProductListSide, ProductCodecError> {
    match value {
        0 => Ok(ProductListSide::Left),
        1 => Ok(ProductListSide::Right),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn decode_structure_kind(
    value: u8,
) -> Result<hyphae_native_catalog::StructureKind, ProductCodecError> {
    use hyphae_native_catalog::StructureKind;
    match value {
        1 => Ok(StructureKind::String),
        2 => Ok(StructureKind::Counter),
        3 => Ok(StructureKind::Hash),
        4 => Ok(StructureKind::List),
        5 => Ok(StructureKind::Set),
        6 => Ok(StructureKind::SortedSet),
        7 => Ok(StructureKind::Stream),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn decode_transaction_handle(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionHandle, ProductCodecError> {
    ProductTransactionHandle::new(decoder.u64()?).ok_or(ProductCodecError::InvalidValue)
}

fn encode_transaction_search_mutation(
    encoded: &mut Vec<u8>,
    mutation: &ProductTransactionSearchMutation,
) -> Result<(), ProductCodecError> {
    match mutation {
        ProductTransactionSearchMutation::Index {
            index,
            document_id,
            text,
        } => {
            encoded.push(0);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            put_bytes(encoded, document_id)?;
            put_text(encoded, text)?;
        }
        ProductTransactionSearchMutation::Replace {
            index,
            document_id,
            text,
        } => {
            encoded.push(1);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            put_bytes(encoded, document_id)?;
            put_text(encoded, text)?;
        }
        ProductTransactionSearchMutation::Delete { index, document_id } => {
            encoded.push(2);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            put_bytes(encoded, document_id)?;
        }
        ProductTransactionSearchMutation::Document {
            collection,
            document,
        } => {
            encoded.push(3);
            encoded.extend_from_slice(&collection.get().to_le_bytes());
            encode_product_document(encoded, document)?;
        }
    }
    Ok(())
}

fn decode_transaction_search_mutation(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionSearchMutation, ProductCodecError> {
    let tag = decoder.u8()?;
    if tag == 3 {
        let collection =
            ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        return Ok(ProductTransactionSearchMutation::Document {
            collection,
            document: decode_product_document(decoder)?,
        });
    }
    let index = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let document_id = decoder.owned_bytes()?;
    match tag {
        0 => Ok(ProductTransactionSearchMutation::Index {
            index,
            document_id,
            text: decoder.text()?,
        }),
        1 => Ok(ProductTransactionSearchMutation::Replace {
            index,
            document_id,
            text: decoder.text()?,
        }),
        2 => Ok(ProductTransactionSearchMutation::Delete { index, document_id }),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_transaction_vector_mutation(
    encoded: &mut Vec<u8>,
    mutation: &ProductTransactionVectorMutation,
) -> Result<(), ProductCodecError> {
    match mutation {
        ProductTransactionVectorMutation::Upsert {
            index,
            object_id,
            vector,
        } => {
            encoded.push(0);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            encoded.extend_from_slice(&object_id.get().to_le_bytes());
            put_u32(encoded, vector.dimension())?;
            for value in vector.values() {
                encoded.extend_from_slice(&value.to_bits().to_le_bytes());
            }
        }
        ProductTransactionVectorMutation::Delete { index, object_id } => {
            encoded.push(1);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            encoded.extend_from_slice(&object_id.get().to_le_bytes());
        }
    }
    Ok(())
}

fn decode_transaction_vector_mutation(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionVectorMutation, ProductCodecError> {
    let tag = decoder.u8()?;
    let index = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let object_id = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    match tag {
        0 => {
            let dimension = decoder.usize_u32()?;
            let mut values = Vec::with_capacity(dimension);
            for _ in 0..dimension {
                values.push(f32::from_bits(decoder.u32()?));
            }
            Ok(ProductTransactionVectorMutation::Upsert {
                index,
                object_id,
                vector: hyphae_native_product::ProductVector::new(values)
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            })
        }
        1 => Ok(ProductTransactionVectorMutation::Delete { index, object_id }),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

#[cfg(any())]
fn encode_transaction_search_mutation_duplicate(
    encoded: &mut Vec<u8>,
    mutation: &ProductTransactionSearchMutation,
) -> Result<(), ProductCodecError> {
    let (tag, index, document_id, text) = match mutation {
        ProductTransactionSearchMutation::Index {
            index,
            document_id,
            text,
        } => (0, index, document_id, Some(text)),
        ProductTransactionSearchMutation::Replace {
            index,
            document_id,
            text,
        } => (1, index, document_id, Some(text)),
        ProductTransactionSearchMutation::Delete { index, document_id } => {
            (2, index, document_id, None)
        }
    };
    encoded.push(tag);
    encoded.extend_from_slice(&index.get().to_le_bytes());
    put_bytes(encoded, document_id)?;
    if let Some(text) = text {
        put_text(encoded, text)?;
    }
    Ok(())
}

#[cfg(any())]
fn decode_transaction_search_mutation_duplicate(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionSearchMutation, ProductCodecError> {
    let tag = decoder.u8()?;
    let index = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let document_id = decoder.owned_bytes()?;
    Ok(match tag {
        0 => ProductTransactionSearchMutation::Index {
            index,
            document_id,
            text: decoder.text()?,
        },
        1 => ProductTransactionSearchMutation::Replace {
            index,
            document_id,
            text: decoder.text()?,
        },
        2 => ProductTransactionSearchMutation::Delete { index, document_id },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

#[cfg(any())]
fn encode_transaction_vector_mutation_duplicate(
    encoded: &mut Vec<u8>,
    mutation: &ProductTransactionVectorMutation,
) -> Result<(), ProductCodecError> {
    match mutation {
        ProductTransactionVectorMutation::Upsert {
            index,
            object_id,
            vector,
        } => {
            encoded.push(0);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            encoded.extend_from_slice(&object_id.get().to_le_bytes());
            put_u32(encoded, vector.values().len())?;
            for value in vector.values() {
                encoded.extend_from_slice(&value.to_bits().to_le_bytes());
            }
        }
        ProductTransactionVectorMutation::Delete { index, object_id } => {
            encoded.push(1);
            encoded.extend_from_slice(&index.get().to_le_bytes());
            encoded.extend_from_slice(&object_id.get().to_le_bytes());
        }
    }
    Ok(())
}

#[cfg(any())]
fn decode_transaction_vector_mutation_duplicate(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionVectorMutation, ProductCodecError> {
    let tag = decoder.u8()?;
    let index = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let object_id = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    Ok(match tag {
        0 => {
            let count = decoder.usize_u32()?;
            if count == 0 || count > usize::from(u16::MAX) {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(f32::from_bits(decoder.u32()?));
            }
            ProductTransactionVectorMutation::Upsert {
                index,
                object_id,
                vector: hyphae_native_product::ProductVector::new(values)
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            }
        }
        1 => ProductTransactionVectorMutation::Delete { index, object_id },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

#[allow(clippy::too_many_lines)]
fn encode_structure_read_request(
    encoded: &mut Vec<u8>,
    request: &ProductStructureReadRequest,
) -> Result<(), ProductCodecError> {
    match request {
        ProductStructureReadRequest::StringGet { key } => {
            encoded.push(0);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::CounterGet { key } => {
            encoded.push(1);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::Ttl { key, family } => {
            encoded.push(2);
            encode_structure_key(encoded, key)?;
            encoded.push(*family as u8);
        }
        ProductStructureReadRequest::HashGet { key, field } => {
            encoded.push(3);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
        }
        ProductStructureReadRequest::HashFieldTtl { key, field } => {
            encoded.push(4);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, field)?;
        }
        ProductStructureReadRequest::HashScan {
            key,
            start_after,
            limit,
        } => {
            encoded.push(5);
            encode_structure_key(encoded, key)?;
            encoded.push(u8::from(start_after.is_some()));
            if let Some(start_after) = start_after {
                put_bytes(encoded, start_after)?;
            }
            put_u64(encoded, *limit)?;
        }
        ProductStructureReadRequest::HashLength { key } => {
            encoded.push(6);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::ListRange { key, start, stop } => {
            encoded.push(7);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&start.to_le_bytes());
            encoded.extend_from_slice(&stop.to_le_bytes());
        }
        ProductStructureReadRequest::ListLength { key } => {
            encoded.push(8);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::SetContains { key, member } => {
            encoded.push(9);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
        }
        ProductStructureReadRequest::SetMembers {
            key,
            start_after,
            limit,
        } => {
            encoded.push(10);
            encode_structure_key(encoded, key)?;
            encoded.push(u8::from(start_after.is_some()));
            if let Some(start_after) = start_after {
                put_bytes(encoded, start_after)?;
            }
            put_u64(encoded, *limit)?;
        }
        ProductStructureReadRequest::SetCardinality { key } => {
            encoded.push(11);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::SetAlgebra {
            keyspace,
            operation,
            keys,
            output_member_limit,
            visit_limit,
        } => {
            encoded.push(12);
            encoded.extend_from_slice(&keyspace.get().to_le_bytes());
            encoded.push(match operation {
                ProductSetAlgebraOperation::Union => 0,
                ProductSetAlgebraOperation::Intersection => 1,
                ProductSetAlgebraOperation::Difference => 2,
            });
            put_u32(encoded, keys.len())?;
            for key in keys {
                put_bytes(encoded, key)?;
            }
            put_u64(encoded, *output_member_limit)?;
            put_u64(encoded, *visit_limit)?;
        }
        ProductStructureReadRequest::SortedSetScore { key, member } => {
            encoded.push(13);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
        }
        ProductStructureReadRequest::SortedSetRank { key, member, order } => {
            encoded.push(14);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, member)?;
            encoded.push(sorted_order_tag(*order));
        }
        ProductStructureReadRequest::SortedSetRange {
            key,
            start,
            stop,
            order,
        } => {
            encoded.push(15);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&start.to_le_bytes());
            encoded.extend_from_slice(&stop.to_le_bytes());
            encoded.push(sorted_order_tag(*order));
        }
        ProductStructureReadRequest::SortedSetCardinality { key } => {
            encoded.push(16);
            encode_structure_key(encoded, key)?;
        }
        ProductStructureReadRequest::StreamRange {
            key,
            start,
            end,
            limit,
        } => {
            encoded.push(17);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&start.to_le_bytes());
            encoded.extend_from_slice(&end.to_le_bytes());
            put_u64(encoded, *limit)?;
        }
        ProductStructureReadRequest::SortedSetScoreRange {
            key,
            lower,
            upper,
            offset,
            limit,
            order,
        } => {
            encoded.push(18);
            encode_structure_key(encoded, key)?;
            encode_score_bound(encoded, *lower)?;
            encode_score_bound(encoded, *upper)?;
            put_u64(encoded, *offset)?;
            put_u64(encoded, *limit)?;
            encoded.push(sorted_order_tag(*order));
        }
        ProductStructureReadRequest::HashScanReverse {
            key,
            start_before,
            limit,
        } => {
            encoded.push(19);
            encode_structure_key(encoded, key)?;
            encoded.push(u8::from(start_before.is_some()));
            if let Some(cursor) = start_before {
                put_bytes(encoded, cursor)?;
            }
            put_u64(encoded, *limit)?;
        }
        ProductStructureReadRequest::HashScanMatch {
            key,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => {
            encoded.push(20);
            encode_structure_key(encoded, key)?;
            put_bytes(encoded, pattern)?;
            encoded.push(u8::from(start_after.is_some()));
            if let Some(cursor) = start_after {
                put_bytes(encoded, cursor)?;
            }
            put_u64(encoded, *output_limit)?;
            put_u64(encoded, *visit_limit)?;
            put_u64(encoded, *match_step_limit)?;
        }
        ProductStructureReadRequest::KeyScanMatch {
            keyspace,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => {
            encoded.push(21);
            encoded.extend_from_slice(&keyspace.get().to_le_bytes());
            put_bytes(encoded, pattern)?;
            encoded.push(u8::from(start_after.is_some()));
            if let Some(cursor) = start_after {
                put_bytes(encoded, cursor)?;
            }
            put_u64(encoded, *output_limit)?;
            put_u64(encoded, *visit_limit)?;
            put_u64(encoded, *match_step_limit)?;
        }
        ProductStructureReadRequest::StringRange { key, start, end } => {
            encoded.push(22);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&start.to_le_bytes());
            encoded.extend_from_slice(&end.to_le_bytes());
        }
        ProductStructureReadRequest::SetRandomMembers { key, seed, count } => {
            encoded.push(23);
            encode_structure_key(encoded, key)?;
            encoded.extend_from_slice(&seed.to_le_bytes());
            put_u64(encoded, *count)?;
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn decode_structure_read_request(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureReadRequest, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => ProductStructureReadRequest::StringGet {
            key: decode_structure_key(decoder)?,
        },
        1 => ProductStructureReadRequest::CounterGet {
            key: decode_structure_key(decoder)?,
        },
        2 => ProductStructureReadRequest::Ttl {
            key: decode_structure_key(decoder)?,
            family: decode_structure_kind(decoder.u8()?)?,
        },
        3 => ProductStructureReadRequest::HashGet {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
        },
        4 => ProductStructureReadRequest::HashFieldTtl {
            key: decode_structure_key(decoder)?,
            field: decoder.owned_bytes()?,
        },
        5 => {
            let key = decode_structure_key(decoder)?;
            let present = decoder.boolean()?;
            let start_after = present.then(|| decoder.owned_bytes()).transpose()?;
            ProductStructureReadRequest::HashScan {
                key,
                start_after,
                limit: decoder.usize()?,
            }
        }
        6 => ProductStructureReadRequest::HashLength {
            key: decode_structure_key(decoder)?,
        },
        7 => ProductStructureReadRequest::ListRange {
            key: decode_structure_key(decoder)?,
            start: decoder.i64()?,
            stop: decoder.i64()?,
        },
        8 => ProductStructureReadRequest::ListLength {
            key: decode_structure_key(decoder)?,
        },
        9 => ProductStructureReadRequest::SetContains {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
        },
        10 => {
            let key = decode_structure_key(decoder)?;
            let present = decoder.boolean()?;
            let start_after = present.then(|| decoder.owned_bytes()).transpose()?;
            ProductStructureReadRequest::SetMembers {
                key,
                start_after,
                limit: decoder.usize()?,
            }
        }
        11 => ProductStructureReadRequest::SetCardinality {
            key: decode_structure_key(decoder)?,
        },
        12 => {
            let keyspace =
                ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
            let operation = match decoder.u8()? {
                0 => ProductSetAlgebraOperation::Union,
                1 => ProductSetAlgebraOperation::Intersection,
                2 => ProductSetAlgebraOperation::Difference,
                _ => return Err(ProductCodecError::InvalidValue),
            };
            let count = decoder.usize_u32()?;
            let mut keys = Vec::with_capacity(count);
            for _ in 0..count {
                keys.push(decoder.owned_bytes()?);
            }
            ProductStructureReadRequest::SetAlgebra {
                keyspace,
                operation,
                keys,
                output_member_limit: decoder.usize()?,
                visit_limit: decoder.usize()?,
            }
        }
        13 => ProductStructureReadRequest::SortedSetScore {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
        },
        14 => ProductStructureReadRequest::SortedSetRank {
            key: decode_structure_key(decoder)?,
            member: decoder.owned_bytes()?,
            order: decode_sorted_order(decoder.u8()?)?,
        },
        15 => ProductStructureReadRequest::SortedSetRange {
            key: decode_structure_key(decoder)?,
            start: decoder.i64()?,
            stop: decoder.i64()?,
            order: decode_sorted_order(decoder.u8()?)?,
        },
        16 => ProductStructureReadRequest::SortedSetCardinality {
            key: decode_structure_key(decoder)?,
        },
        17 => ProductStructureReadRequest::StreamRange {
            key: decode_structure_key(decoder)?,
            start: decoder.u64()?,
            end: decoder.u64()?,
            limit: decoder.usize()?,
        },
        18 => ProductStructureReadRequest::SortedSetScoreRange {
            key: decode_structure_key(decoder)?,
            lower: decode_score_bound(decoder)?,
            upper: decode_score_bound(decoder)?,
            offset: decoder.usize()?,
            limit: decoder.usize()?,
            order: decode_sorted_order(decoder.u8()?)?,
        },
        19 => ProductStructureReadRequest::HashScanReverse {
            key: decode_structure_key(decoder)?,
            start_before: if decoder.boolean()? {
                Some(decoder.owned_bytes()?)
            } else {
                None
            },
            limit: decoder.usize()?,
        },
        20 => ProductStructureReadRequest::HashScanMatch {
            key: decode_structure_key(decoder)?,
            pattern: decoder.owned_bytes()?,
            start_after: if decoder.boolean()? {
                Some(decoder.owned_bytes()?)
            } else {
                None
            },
            output_limit: decoder.usize()?,
            visit_limit: decoder.usize()?,
            match_step_limit: decoder.usize()?,
        },
        21 => ProductStructureReadRequest::KeyScanMatch {
            keyspace: ObjectId::new(decoder.u128()?)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            pattern: decoder.owned_bytes()?,
            start_after: if decoder.boolean()? {
                Some(decoder.owned_bytes()?)
            } else {
                None
            },
            output_limit: decoder.usize()?,
            visit_limit: decoder.usize()?,
            match_step_limit: decoder.usize()?,
        },
        22 => ProductStructureReadRequest::StringRange {
            key: decode_structure_key(decoder)?,
            start: decoder.i64()?,
            end: decoder.i64()?,
        },
        23 => ProductStructureReadRequest::SetRandomMembers {
            key: decode_structure_key(decoder)?,
            seed: decoder.u64()?,
            count: decoder.usize()?,
        },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn encode_score_bound(
    encoded: &mut Vec<u8>,
    bound: ProductScoreBound,
) -> Result<(), ProductCodecError> {
    match bound {
        ProductScoreBound::Unbounded => encoded.push(0),
        ProductScoreBound::Inclusive(score) => {
            encoded.push(1);
            encoded.extend_from_slice(&score.to_bits().to_le_bytes());
        }
        ProductScoreBound::Exclusive(score) => {
            encoded.push(2);
            encoded.extend_from_slice(&score.to_bits().to_le_bytes());
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

fn decode_score_bound(decoder: &mut Decoder<'_>) -> Result<ProductScoreBound, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => ProductScoreBound::Unbounded,
        1 => ProductScoreBound::Inclusive(f64::from_bits(decoder.u64()?)),
        2 => ProductScoreBound::Exclusive(f64::from_bits(decoder.u64()?)),
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

const fn sorted_order_tag(order: ProductSortedSetOrder) -> u8 {
    match order {
        ProductSortedSetOrder::Ascending => 0,
        ProductSortedSetOrder::Descending => 1,
    }
}

fn decode_sorted_order(value: u8) -> Result<ProductSortedSetOrder, ProductCodecError> {
    match value {
        0 => Ok(ProductSortedSetOrder::Ascending),
        1 => Ok(ProductSortedSetOrder::Descending),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

// Exhaustive result dispatch: one arm per result shape, cohesive by design.
#[allow(clippy::too_many_lines)]
fn encode_structure_read_result(
    encoded: &mut Vec<u8>,
    result: &ProductStructureReadResult,
) -> Result<(), ProductCodecError> {
    match result {
        ProductStructureReadResult::Value(value) => {
            encoded.push(0);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                put_bytes(encoded, value)?;
            }
        }
        ProductStructureReadResult::Values(values) => {
            encoded.push(1);
            put_byte_values(encoded, values)?;
        }
        ProductStructureReadResult::Counter(value) => {
            encoded.push(2);
            encode_optional_i64(encoded, *value);
        }
        ProductStructureReadResult::Ttl(value) => {
            encoded.push(3);
            encode_product_ttl(encoded, *value);
        }
        ProductStructureReadResult::HashEntries(entries) => {
            encoded.push(4);
            put_u32(encoded, entries.len())?;
            for entry in entries {
                put_bytes(encoded, &entry.field)?;
                put_bytes(encoded, &entry.value)?;
            }
        }
        ProductStructureReadResult::Count(value) => {
            encoded.push(5);
            put_u64(encoded, *value)?;
        }
        ProductStructureReadResult::Boolean(value) => {
            encoded.push(6);
            encoded.push(u8::from(*value));
        }
        ProductStructureReadResult::SetAlgebra { members, visited } => {
            encoded.push(7);
            put_byte_values(encoded, members)?;
            put_u64(encoded, *visited)?;
        }
        ProductStructureReadResult::SortedSetScore(value) => {
            encoded.push(8);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                encoded.extend_from_slice(&value.bits().to_le_bytes());
            }
        }
        ProductStructureReadResult::SortedSetRank(value) => {
            encoded.push(9);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                put_u64(encoded, *value)?;
            }
        }
        ProductStructureReadResult::SortedSetEntries(entries) => {
            encoded.push(10);
            put_u32(encoded, entries.len())?;
            for entry in entries {
                put_bytes(encoded, &entry.member)?;
                encoded.extend_from_slice(&entry.score.bits().to_le_bytes());
            }
        }
        ProductStructureReadResult::StreamEntries(entries) => {
            encoded.push(11);
            put_u32(encoded, entries.len())?;
            for entry in entries {
                encoded.extend_from_slice(&entry.id.to_le_bytes());
                put_u32(encoded, entry.fields.len())?;
                for field in &entry.fields {
                    put_bytes(encoded, &field.field)?;
                    put_bytes(encoded, &field.value)?;
                }
            }
        }
        ProductStructureReadResult::HashPage {
            entries,
            continuation,
            stop,
            visited,
            match_steps,
        } => {
            encoded.push(12);
            put_u32(encoded, entries.len())?;
            for entry in entries {
                put_bytes(encoded, &entry.field)?;
                put_bytes(encoded, &entry.value)?;
            }
            encoded.push(u8::from(continuation.is_some()));
            if let Some(cursor) = continuation {
                put_bytes(encoded, cursor)?;
            }
            encoded.push(match stop {
                ProductHashScanStop::Exhausted => 0,
                ProductHashScanStop::OutputLimit => 1,
                ProductHashScanStop::VisitLimit => 2,
                _ => return Err(ProductCodecError::Unsupported),
            });
            put_u64(encoded, *visited)?;
            put_u64(encoded, *match_steps)?;
        }
        ProductStructureReadResult::KeyPage {
            entries,
            continuation,
            stop,
            visited,
            match_steps,
        } => {
            encoded.push(13);
            put_u32(encoded, entries.len())?;
            for entry in entries {
                put_bytes(encoded, &entry.key)?;
                encoded.push(entry.family as u8);
            }
            encoded.push(u8::from(continuation.is_some()));
            if let Some(cursor) = continuation {
                put_bytes(encoded, cursor)?;
            }
            encoded.push(match stop {
                ProductHashScanStop::Exhausted => 0,
                ProductHashScanStop::OutputLimit => 1,
                ProductHashScanStop::VisitLimit => 2,
                _ => return Err(ProductCodecError::Unsupported),
            });
            put_u64(encoded, *visited)?;
            put_u64(encoded, *match_steps)?;
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

fn decode_structure_read_result(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureReadResult, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => ProductStructureReadResult::Value(if decoder.boolean()? {
            Some(decoder.owned_bytes()?)
        } else {
            None
        }),
        1 => ProductStructureReadResult::Values(read_byte_values(decoder)?),
        2 => ProductStructureReadResult::Counter(decode_optional_i64(decoder)?),
        3 => ProductStructureReadResult::Ttl(decode_product_ttl(decoder)?),
        4 => {
            let count = decoder.usize_u32()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(ProductHashEntry {
                    field: decoder.owned_bytes()?,
                    value: decoder.owned_bytes()?,
                });
            }
            ProductStructureReadResult::HashEntries(entries)
        }
        5 => ProductStructureReadResult::Count(decoder.usize()?),
        6 => ProductStructureReadResult::Boolean(decoder.boolean()?),
        7 => ProductStructureReadResult::SetAlgebra {
            members: read_byte_values(decoder)?,
            visited: decoder.usize()?,
        },
        8 => ProductStructureReadResult::SortedSetScore(if decoder.boolean()? {
            Some(hyphae_native_product::CanonicalF64::new(f64::from_bits(
                decoder.u64()?,
            )))
        } else {
            None
        }),
        9 => ProductStructureReadResult::SortedSetRank(if decoder.boolean()? {
            Some(decoder.usize()?)
        } else {
            None
        }),
        10 => {
            let count = decoder.usize_u32()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(ProductSortedSetEntry {
                    member: decoder.owned_bytes()?,
                    score: hyphae_native_product::CanonicalF64::new(f64::from_bits(decoder.u64()?)),
                });
            }
            ProductStructureReadResult::SortedSetEntries(entries)
        }
        11 => {
            let count = decoder.usize_u32()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let id = decoder.u64()?;
                let field_count = decoder.usize_u32()?;
                let mut fields = Vec::with_capacity(field_count);
                for _ in 0..field_count {
                    fields.push(ProductHashEntry {
                        field: decoder.owned_bytes()?,
                        value: decoder.owned_bytes()?,
                    });
                }
                entries.push(ProductStreamEntry { id, fields });
            }
            ProductStructureReadResult::StreamEntries(entries)
        }
        12 => {
            let count = decoder.usize_u32()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(ProductHashEntry {
                    field: decoder.owned_bytes()?,
                    value: decoder.owned_bytes()?,
                });
            }
            let continuation = if decoder.boolean()? {
                Some(decoder.owned_bytes()?)
            } else {
                None
            };
            let stop = match decoder.u8()? {
                0 => ProductHashScanStop::Exhausted,
                1 => ProductHashScanStop::OutputLimit,
                2 => ProductHashScanStop::VisitLimit,
                _ => return Err(ProductCodecError::InvalidValue),
            };
            ProductStructureReadResult::HashPage {
                entries,
                continuation,
                stop,
                visited: decoder.usize()?,
                match_steps: decoder.usize()?,
            }
        }
        13 => decode_key_page(decoder)?,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

/// Decodes one bounded cross-family key glob page (result tag 13).
fn decode_key_page(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureReadResult, ProductCodecError> {
    let count = decoder.usize_u32()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(hyphae_native_product::ProductKeyEntry {
            key: decoder.owned_bytes()?,
            family: decode_structure_kind(decoder.u8()?)?,
        });
    }
    let continuation = decoder
        .boolean()?
        .then(|| decoder.owned_bytes())
        .transpose()?;
    let stop = match decoder.u8()? {
        0 => ProductHashScanStop::Exhausted,
        1 => ProductHashScanStop::OutputLimit,
        2 => ProductHashScanStop::VisitLimit,
        _ => return Err(ProductCodecError::InvalidValue),
    };
    Ok(ProductStructureReadResult::KeyPage {
        entries,
        continuation,
        stop,
        visited: decoder.usize()?,
        match_steps: decoder.usize()?,
    })
}

fn encode_product_ttl(encoded: &mut Vec<u8>, value: ProductTtl) {
    match value {
        ProductTtl::Missing => encoded.push(0),
        ProductTtl::Persistent => encoded.push(1),
        ProductTtl::RemainingMicros(value) => {
            encoded.push(2);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn decode_product_ttl(decoder: &mut Decoder<'_>) -> Result<ProductTtl, ProductCodecError> {
    match decoder.u8()? {
        0 => Ok(ProductTtl::Missing),
        1 => Ok(ProductTtl::Persistent),
        2 => Ok(ProductTtl::RemainingMicros(decoder.i64()?)),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

const fn restore_phase_tag(value: hyphae_native_product::RestorePhase) -> u8 {
    use hyphae_native_product::RestorePhase;
    match value {
        RestorePhase::ValidatingRequest => 0,
        RestorePhase::VerifyingBackup => 1,
        RestorePhase::RestoringAndPromoting => 2,
        RestorePhase::Promoted => 3,
        RestorePhase::DoctorAfterRestore => 4,
        RestorePhase::Complete => 5,
    }
}

fn decode_restore_phase(
    value: u8,
) -> Result<hyphae_native_product::RestorePhase, ProductCodecError> {
    use hyphae_native_product::RestorePhase;
    match value {
        0 => Ok(RestorePhase::ValidatingRequest),
        1 => Ok(RestorePhase::VerifyingBackup),
        2 => Ok(RestorePhase::RestoringAndPromoting),
        3 => Ok(RestorePhase::Promoted),
        4 => Ok(RestorePhase::DoctorAfterRestore),
        5 => Ok(RestorePhase::Complete),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_explicit_transaction_status(
    encoded: &mut Vec<u8>,
    value: ProductExplicitTransactionStatus,
) -> Result<(), ProductCodecError> {
    match value {
        ProductExplicitTransactionStatus::Unknown => encoded.push(0),
        ProductExplicitTransactionStatus::Active {
            handle,
            read_csn,
            staged_operations,
            durability,
        } => {
            encoded.push(1);
            encoded.extend_from_slice(&handle.get().to_le_bytes());
            encoded.extend_from_slice(&read_csn.unwrap_or(0).to_le_bytes());
            put_u64(encoded, staged_operations)?;
            encoded.push(durability_tag(durability));
        }
        ProductExplicitTransactionStatus::Committed {
            handle,
            staged_operations,
            receipt,
        } => {
            encoded.push(2);
            encoded.extend_from_slice(&handle.get().to_le_bytes());
            put_u64(encoded, staged_operations)?;
            encode_receipt(encoded, receipt)?;
        }
        ProductExplicitTransactionStatus::RolledBack {
            handle,
            discarded_operations,
        } => {
            encoded.push(3);
            encoded.extend_from_slice(&handle.get().to_le_bytes());
            put_u64(encoded, discarded_operations)?;
        }
        ProductExplicitTransactionStatus::OutcomeUnknown {
            handle,
            transaction_id,
            staged_operations,
        } => {
            encoded.push(4);
            encoded.extend_from_slice(&handle.get().to_le_bytes());
            encoded.extend_from_slice(&transaction_id.get().to_le_bytes());
            put_u64(encoded, staged_operations)?;
        }
    }
    Ok(())
}

fn decode_explicit_transaction_status(
    decoder: &mut Decoder<'_>,
) -> Result<ProductExplicitTransactionStatus, ProductCodecError> {
    match decoder.u8()? {
        0 => Ok(ProductExplicitTransactionStatus::Unknown),
        1 => {
            let handle = decode_transaction_handle(decoder)?;
            let read_csn = decoder.u64()?;
            Ok(ProductExplicitTransactionStatus::Active {
                handle,
                read_csn: (read_csn != 0).then_some(read_csn),
                staged_operations: decoder.usize()?,
                durability: decode_durability(decoder.u8()?)?,
            })
        }
        2 => Ok(ProductExplicitTransactionStatus::Committed {
            handle: decode_transaction_handle(decoder)?,
            staged_operations: decoder.usize()?,
            receipt: decode_receipt(decoder)?,
        }),
        3 => Ok(ProductExplicitTransactionStatus::RolledBack {
            handle: decode_transaction_handle(decoder)?,
            discarded_operations: decoder.usize()?,
        }),
        4 => Ok(ProductExplicitTransactionStatus::OutcomeUnknown {
            handle: decode_transaction_handle(decoder)?,
            transaction_id: ProductTransactionId::new(decoder.u128()?)
                .ok_or(ProductCodecError::InvalidValue)?,
            staged_operations: decoder.usize()?,
        }),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_transaction_stage_receipt(
    encoded: &mut Vec<u8>,
    value: &ProductTransactionStageReceipt,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&value.handle.get().to_le_bytes());
    put_u64(encoded, value.operation_ordinal)?;
    encoded.push(u8::from(value.changed));
    match &value.result {
        ProductTransactionStageResult::Sql(result) => {
            encoded.push(0);
            encode_sql_result(encoded, result)?;
        }
        ProductTransactionStageResult::Structure(result) => {
            encoded.push(1);
            encode_structure_mutation_result(encoded, result)?;
        }
        ProductTransactionStageResult::Search => encoded.push(2),
        ProductTransactionStageResult::Vector(changed) => {
            encoded.push(3);
            encoded.push(u8::from(*changed));
        }
    }
    Ok(())
}

fn decode_transaction_stage_receipt(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionStageReceipt, ProductCodecError> {
    let handle = decode_transaction_handle(decoder)?;
    let operation_ordinal = decoder.usize()?;
    let changed = decoder.boolean()?;
    let result = match decoder.u8()? {
        0 => ProductTransactionStageResult::Sql(decode_sql_result(decoder)?),
        1 => ProductTransactionStageResult::Structure(decode_structure_mutation_result(decoder)?),
        2 => ProductTransactionStageResult::Search,
        3 => ProductTransactionStageResult::Vector(decoder.boolean()?),
        _ => return Err(ProductCodecError::InvalidValue),
    };
    Ok(ProductTransactionStageReceipt {
        handle,
        operation_ordinal,
        changed,
        result,
    })
}

fn encode_structure_mutation_result(
    encoded: &mut Vec<u8>,
    value: &ProductStructureMutationResult,
) -> Result<(), ProductCodecError> {
    match value {
        ProductStructureMutationResult::Unit => encoded.push(0),
        ProductStructureMutationResult::Integer(value) => {
            encoded.push(1);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductStructureMutationResult::Boolean(value) => {
            encoded.push(2);
            encoded.push(u8::from(*value));
        }
        ProductStructureMutationResult::Count(value) => {
            encoded.push(3);
            put_u64(encoded, *value)?;
        }
        ProductStructureMutationResult::Value(value) => {
            encoded.push(4);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                put_bytes(encoded, value)?;
            }
        }
        ProductStructureMutationResult::StreamId(value) => {
            encoded.push(5);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductStructureMutationResult::Score(value) => {
            encoded.push(6);
            encoded.extend_from_slice(&value.bits().to_le_bytes());
        }
        ProductStructureMutationResult::PoppedEntry(entry) => {
            encoded.push(7);
            encoded.push(u8::from(entry.is_some()));
            if let Some(entry) = entry {
                put_bytes(encoded, &entry.member)?;
                encoded.extend_from_slice(&entry.score.bits().to_le_bytes());
            }
        }
        _ => return Err(ProductCodecError::Unsupported),
    }
    Ok(())
}

fn decode_structure_mutation_result(
    decoder: &mut Decoder<'_>,
) -> Result<ProductStructureMutationResult, ProductCodecError> {
    match decoder.u8()? {
        0 => Ok(ProductStructureMutationResult::Unit),
        1 => Ok(ProductStructureMutationResult::Integer(decoder.i64()?)),
        2 => Ok(ProductStructureMutationResult::Boolean(decoder.boolean()?)),
        3 => Ok(ProductStructureMutationResult::Count(decoder.usize()?)),
        4 => Ok(ProductStructureMutationResult::Value(
            decoder
                .boolean()?
                .then(|| decoder.owned_bytes())
                .transpose()?,
        )),
        5 => Ok(ProductStructureMutationResult::StreamId(decoder.u64()?)),
        6 => Ok(ProductStructureMutationResult::Score(
            hyphae_native_product::CanonicalF64::new(f64::from_bits(decoder.u64()?)),
        )),
        7 => Ok(ProductStructureMutationResult::PoppedEntry(
            decoder
                .boolean()?
                .then(|| -> Result<_, ProductCodecError> {
                    Ok(hyphae_native_product::ProductSortedSetEntry {
                        member: decoder.owned_bytes()?,
                        score: hyphae_native_product::CanonicalF64::new(f64::from_bits(
                            decoder.u64()?,
                        )),
                    })
                })
                .transpose()?,
        )),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn put_byte_values(encoded: &mut Vec<u8>, values: &[Vec<u8>]) -> Result<(), ProductCodecError> {
    put_u32(encoded, values.len())?;
    for value in values {
        put_bytes(encoded, value)?;
    }
    Ok(())
}

fn read_byte_values(decoder: &mut Decoder<'_>) -> Result<Vec<Vec<u8>>, ProductCodecError> {
    let count = decoder.usize_u32()?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decoder.owned_bytes()?);
    }
    Ok(values)
}

fn encode_sql_result(
    encoded: &mut Vec<u8>,
    result: &ProductSqlResult,
) -> Result<(), ProductCodecError> {
    match result {
        ProductSqlResult::Command {
            rows_affected,
            object_id,
        } => {
            encoded.push(0);
            encoded.push(u8::from(object_id.is_some()));
            encoded.extend_from_slice(&[0; 6]);
            encoded.extend_from_slice(&rows_affected.to_le_bytes());
            if let Some(object_id) = object_id {
                encoded.extend_from_slice(&object_id.get().to_le_bytes());
            }
        }
        ProductSqlResult::Rows { columns, rows } => {
            encoded.push(1);
            encoded.extend_from_slice(&[0; 7]);
            put_u32(encoded, columns.len())?;
            put_u32(encoded, rows.len())?;
            for column in columns {
                put_text(encoded, column)?;
            }
            for row in rows {
                if row.len() != columns.len() {
                    return Err(ProductCodecError::InvalidValue);
                }
                for value in row {
                    encode_value(encoded, value, 0)?;
                }
            }
        }
    }
    Ok(())
}

fn decode_sql_result(decoder: &mut Decoder<'_>) -> Result<ProductSqlResult, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => {
            let has_object = decoder.u8()?;
            if has_object > 1 || decoder.bytes(6)? != [0; 6] {
                return Err(ProductCodecError::Malformed);
            }
            ProductSqlResult::Command {
                rows_affected: decoder.u64()?,
                object_id: if has_object == 1 {
                    Some(
                        ObjectId::new(decoder.u128()?)
                            .map_err(|_| ProductCodecError::InvalidValue)?,
                    )
                } else {
                    None
                },
            }
        }
        1 => {
            if decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            let column_count = decoder.usize_u32()?;
            let row_count = decoder.usize_u32()?;
            if column_count > 4096 || row_count > 4096 {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut columns = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                columns.push(decoder.text()?);
            }
            let mut rows = Vec::with_capacity(row_count);
            for _ in 0..row_count {
                let mut row = Vec::with_capacity(column_count);
                for _ in 0..column_count {
                    row.push(decode_value(decoder, 0)?);
                }
                rows.push(row);
            }
            ProductSqlResult::Rows { columns, rows }
        }
        _ => return Err(ProductCodecError::Unsupported),
    })
}

fn encode_query(
    encoded: &mut Vec<u8>,
    query: &BoundedSearchQuery,
    depth: usize,
) -> Result<(), ProductCodecError> {
    if depth > 8 {
        return Err(ProductCodecError::LimitExceeded);
    }
    match query {
        BoundedSearchQuery::Term(value) => {
            encoded.push(0);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Phrase(value) => {
            encoded.push(1);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Prefix(value) => {
            encoded.push(2);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Fuzzy { term, max_distance } => {
            encoded.push(3);
            encoded.push(*max_distance);
            put_text(encoded, term)?;
        }
        BoundedSearchQuery::Boolean {
            must,
            should,
            must_not,
        } => {
            encoded.push(4);
            put_u32(encoded, must.len())?;
            put_u32(encoded, should.len())?;
            put_u32(encoded, must_not.len())?;
            for clause in must.iter().chain(should).chain(must_not) {
                encode_query(encoded, clause, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn decode_query(
    decoder: &mut Decoder<'_>,
    depth: usize,
) -> Result<BoundedSearchQuery, ProductCodecError> {
    if depth > 8 {
        return Err(ProductCodecError::LimitExceeded);
    }
    Ok(match decoder.u8()? {
        0 => BoundedSearchQuery::Term(decoder.text()?),
        1 => BoundedSearchQuery::Phrase(decoder.text()?),
        2 => BoundedSearchQuery::Prefix(decoder.text()?),
        3 => BoundedSearchQuery::Fuzzy {
            max_distance: decoder.u8()?,
            term: decoder.text()?,
        },
        4 => {
            let must_count = decoder.usize_u32()?;
            let should_count = decoder.usize_u32()?;
            let must_not_count = decoder.usize_u32()?;
            if must_count
                .checked_add(should_count)
                .and_then(|count| count.checked_add(must_not_count))
                .is_none_or(|count| count > 4096)
            {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut read = |count| -> Result<Vec<BoundedSearchQuery>, ProductCodecError> {
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(decode_query(decoder, depth + 1)?);
                }
                Ok(values)
            };
            let must = read(must_count)?;
            let should = read(should_count)?;
            let must_not = read(must_not_count)?;
            BoundedSearchQuery::boolean(must, should, must_not)
        }
        _ => return Err(ProductCodecError::Unsupported),
    })
}

#[allow(clippy::too_many_lines)]
fn encode_search_collection(
    encoded: &mut Vec<u8>,
    collection: ObjectId,
    request: &ProductSearchRequest,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&collection.get().to_le_bytes());
    encoded.push(u8::from(request.lexical.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    if let Some(lexical) = &request.lexical {
        put_text(encoded, &lexical.query)?;
        put_u64(encoded, lexical.candidate_limit)?;
        encoded.extend_from_slice(&lexical.weight.to_le_bytes());
    }
    put_u32(encoded, request.vectors.len())?;
    for vector in &request.vectors {
        put_text(encoded, &vector.target)?;
        put_u32(encoded, vector.query.dimension())?;
        for value in vector.query.values() {
            encoded.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        put_u64(encoded, vector.candidate_limit)?;
        encoded.extend_from_slice(&vector.weight.to_le_bytes());
        match vector.execution {
            None => {
                encoded.push(0);
                encoded.extend_from_slice(&[0; 7]);
            }
            Some(ProductVectorExecution::Exact) => {
                encoded.push(1);
                encoded.extend_from_slice(&[0; 7]);
            }
            Some(ProductVectorExecution::Ann {
                ef_search,
                exact_rerank,
            }) => {
                encoded.push(2);
                encoded.push(u8::from(exact_rerank.is_some()));
                encoded.extend_from_slice(&[0; 6]);
                put_u64(encoded, ef_search)?;
                put_u64(encoded, exact_rerank.unwrap_or(0))?;
            }
            Some(ProductVectorExecution::Adaptive {
                exact_candidate_threshold,
                ef_search,
                exact_rerank,
            }) => {
                encoded.push(3);
                encoded.push(u8::from(exact_rerank.is_some()));
                encoded.extend_from_slice(&[0; 6]);
                put_u64(encoded, exact_candidate_threshold)?;
                put_u64(encoded, ef_search)?;
                put_u64(encoded, exact_rerank.unwrap_or(0))?;
            }
        }
    }
    encode_search_filter(encoded, &request.filter, 0)?;
    put_u32(encoded, request.sort.len())?;
    for sort in &request.sort {
        match &sort.source {
            ProductSortSource::Score => encoded.push(0),
            ProductSortSource::Field(field) => {
                encoded.push(1);
                put_text(encoded, field)?;
            }
        }
        encoded.push(match sort.direction {
            ProductSortDirection::Ascending => 0,
            ProductSortDirection::Descending => 1,
        });
        encoded.push(match sort.missing {
            ProductMissingPlacement::First => 0,
            ProductMissingPlacement::Last => 1,
        });
    }
    put_u32(encoded, request.facets.len())?;
    for facet in &request.facets {
        put_text(encoded, &facet.field)?;
        put_u64(encoded, facet.limit)?;
    }
    put_u32(encoded, request.aggregations.len())?;
    for aggregation in &request.aggregations {
        put_text(encoded, &aggregation.name)?;
        match &aggregation.aggregation {
            hyphae_native_product::ProductAggregation::Count => encoded.push(0),
            hyphae_native_product::ProductAggregation::Sum(field) => {
                encoded.push(1);
                put_text(encoded, field)?;
            }
            hyphae_native_product::ProductAggregation::Min(field) => {
                encoded.push(2);
                put_text(encoded, field)?;
            }
            hyphae_native_product::ProductAggregation::Max(field) => {
                encoded.push(3);
                put_text(encoded, field)?;
            }
        }
    }
    put_u64(encoded, request.limit)?;
    // Content-derived tagged sections in ascending tag order: an absent
    // section is the default and keeps the exact historical bytes.
    if let Some(fusion) = request.fusion {
        encoded.push(1);
        encoded.push(match fusion {
            hyphae_native_product::ProductFusionMethod::WeightedScore => 1,
        });
    }
    if let Some(dedupe) = &request.parent_dedupe {
        encoded.push(2);
        put_text(encoded, &dedupe.field)?;
        put_u32(encoded, dedupe.first_k)?;
    }
    if let Some(stage) = &request.rerank {
        encoded.push(3);
        put_bytes(encoded, &stage.attestation)?;
        put_u32(encoded, stage.scores.len())?;
        for (object_id, score) in &stage.scores {
            encoded.extend_from_slice(&object_id.get().to_le_bytes());
            encoded.extend_from_slice(&score.to_le_bytes());
        }
    }
    if let Some(highlight) = &request.highlight {
        encoded.push(4);
        put_u32(encoded, highlight.max_fragments)?;
        put_u32(encoded, highlight.fragment_bytes)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn decode_search_collection(
    decoder: &mut Decoder<'_>,
) -> Result<(ObjectId, ProductSearchRequest), ProductCodecError> {
    let collection = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let has_lexical = decoder.boolean()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let lexical = has_lexical
        .then(|| -> Result<ProductLexicalBranch, ProductCodecError> {
            Ok(ProductLexicalBranch {
                query: decoder.text()?,
                candidate_limit: decoder.usize()?,
                weight: decoder.u32()?,
            })
        })
        .transpose()?;
    let vector_count = decoder.usize_u32()?;
    if vector_count > hyphae_native_product::MAX_PRODUCT_SEARCH_VECTOR_TARGETS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut vectors = Vec::with_capacity(vector_count);
    for _ in 0..vector_count {
        let target = decoder.text()?;
        let dimension = decoder.usize_u32()?;
        let mut values = Vec::with_capacity(dimension);
        for _ in 0..dimension {
            values.push(f32::from_bits(decoder.u32()?));
        }
        let candidate_limit = decoder.usize()?;
        let weight = decoder.u32()?;
        let execution = match decoder.u8()? {
            0 => {
                if decoder.bytes(7)? != [0; 7] {
                    return Err(ProductCodecError::Malformed);
                }
                None
            }
            1 => {
                if decoder.bytes(7)? != [0; 7] {
                    return Err(ProductCodecError::Malformed);
                }
                Some(ProductVectorExecution::Exact)
            }
            tag @ (2 | 3) => {
                let has_rerank = decoder.boolean()?;
                if decoder.bytes(6)? != [0; 6] {
                    return Err(ProductCodecError::Malformed);
                }
                Some(if tag == 2 {
                    let ef_search = decoder.usize()?;
                    let rerank = decoder.usize()?;
                    ProductVectorExecution::Ann {
                        ef_search,
                        exact_rerank: has_rerank.then_some(rerank),
                    }
                } else {
                    let exact_candidate_threshold = decoder.usize()?;
                    let ef_search = decoder.usize()?;
                    let rerank = decoder.usize()?;
                    ProductVectorExecution::Adaptive {
                        exact_candidate_threshold,
                        ef_search,
                        exact_rerank: has_rerank.then_some(rerank),
                    }
                })
            }
            _ => return Err(ProductCodecError::InvalidValue),
        };
        vectors.push(ProductVectorBranch {
            target,
            query: hyphae_native_product::ProductVector::new(values)
                .map_err(|_| ProductCodecError::InvalidValue)?,
            candidate_limit,
            weight,
            execution,
        });
    }
    let filter = decode_search_filter(decoder, 0)?;
    let sort_count = decoder.usize_u32()?;
    if sort_count > hyphae_native_runtime::MAX_DOC_VALUE_SORTS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut sort = Vec::with_capacity(sort_count);
    for _ in 0..sort_count {
        let source = match decoder.u8()? {
            0 => ProductSortSource::Score,
            1 => ProductSortSource::Field(decoder.text()?),
            _ => return Err(ProductCodecError::InvalidValue),
        };
        let direction = match decoder.u8()? {
            0 => ProductSortDirection::Ascending,
            1 => ProductSortDirection::Descending,
            _ => return Err(ProductCodecError::InvalidValue),
        };
        let missing = match decoder.u8()? {
            0 => ProductMissingPlacement::First,
            1 => ProductMissingPlacement::Last,
            _ => return Err(ProductCodecError::InvalidValue),
        };
        sort.push(ProductSearchSort {
            source,
            direction,
            missing,
        });
    }
    let facet_count = decoder.usize_u32()?;
    if facet_count > hyphae_native_runtime::MAX_DOC_VALUE_FACETS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut facets = Vec::with_capacity(facet_count);
    for _ in 0..facet_count {
        facets.push(hyphae_native_product::ProductFacetRequest {
            field: decoder.text()?,
            limit: decoder.usize()?,
        });
    }
    let aggregation_count = decoder.usize_u32()?;
    if aggregation_count > hyphae_native_runtime::MAX_DOC_VALUE_AGGREGATIONS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut aggregations = Vec::with_capacity(aggregation_count);
    for _ in 0..aggregation_count {
        let name = decoder.text()?;
        let aggregation = match decoder.u8()? {
            0 => hyphae_native_product::ProductAggregation::Count,
            1 => hyphae_native_product::ProductAggregation::Sum(decoder.text()?),
            2 => hyphae_native_product::ProductAggregation::Min(decoder.text()?),
            3 => hyphae_native_product::ProductAggregation::Max(decoder.text()?),
            _ => return Err(ProductCodecError::InvalidValue),
        };
        aggregations.push(ProductNamedAggregation { name, aggregation });
    }
    let limit = decoder.usize()?;
    let mut fusion = None;
    let mut parent_dedupe = None;
    let mut rerank = None;
    let mut highlight = None;
    let mut previous = 0_u8;
    while decoder.has_remaining() {
        let tag = decoder.u8()?;
        if tag <= previous {
            return Err(ProductCodecError::InvalidValue);
        }
        previous = tag;
        match tag {
            1 => {
                fusion = Some(match decoder.u8()? {
                    1 => hyphae_native_product::ProductFusionMethod::WeightedScore,
                    _ => return Err(ProductCodecError::InvalidValue),
                });
            }
            2 => {
                let field = decoder.text()?;
                let first_k = decoder.usize_u32()?;
                if !(1..=hyphae_native_product::MAX_PARENT_DEDUPE_FIRST_K).contains(&first_k) {
                    return Err(ProductCodecError::InvalidValue);
                }
                parent_dedupe = Some(hyphae_native_product::ProductParentDedupe { field, first_k });
            }
            3 => {
                let attestation = decoder.owned_bytes()?;
                if hyphae_native_product::proof::attestation::ModelAttestation::decode(&attestation)
                    .is_err()
                {
                    return Err(ProductCodecError::InvalidValue);
                }
                let count = decoder.usize_u32()?;
                if !(1..=hyphae_native_product::MAX_RERANK_ENTRIES).contains(&count) {
                    return Err(ProductCodecError::InvalidValue);
                }
                let mut scores = Vec::with_capacity(count);
                for _ in 0..count {
                    let object_id = ObjectId::new(decoder.u128()?)
                        .map_err(|_| ProductCodecError::InvalidValue)?;
                    let score = f64::from_le_bytes(
                        decoder
                            .bytes(8)?
                            .try_into()
                            .map_err(|_| ProductCodecError::Malformed)?,
                    );
                    scores.push((object_id, score));
                }
                rerank = Some(hyphae_native_product::ProductRerankStage {
                    attestation,
                    scores,
                });
            }
            4 => {
                let max_fragments = decoder.usize_u32()?;
                let fragment_bytes = decoder.usize_u32()?;
                if !(1..=hyphae_native_product::MAX_HIGHLIGHT_FRAGMENTS).contains(&max_fragments)
                    || !(hyphae_native_product::MIN_HIGHLIGHT_FRAGMENT_BYTES
                        ..=hyphae_native_product::MAX_HIGHLIGHT_FRAGMENT_BYTES)
                        .contains(&fragment_bytes)
                {
                    return Err(ProductCodecError::InvalidValue);
                }
                highlight = Some(hyphae_native_product::ProductHighlight {
                    max_fragments,
                    fragment_bytes,
                });
            }
            _ => return Err(ProductCodecError::InvalidValue),
        }
    }
    Ok((
        collection,
        ProductSearchRequest {
            lexical,
            vectors,
            filter,
            sort,
            facets,
            aggregations,
            limit,
            fusion,
            parent_dedupe,
            rerank,
            highlight,
        },
    ))
}

fn encode_search_filter(
    encoded: &mut Vec<u8>,
    filter: &ProductSearchFilter,
    depth: usize,
) -> Result<(), ProductCodecError> {
    if depth > hyphae_native_runtime::MAX_DOC_VALUE_FILTER_DEPTH {
        return Err(ProductCodecError::LimitExceeded);
    }
    match filter {
        ProductSearchFilter::MatchAll => encoded.push(0),
        ProductSearchFilter::Exists(field) => {
            encoded.push(1);
            put_text(encoded, field)?;
        }
        ProductSearchFilter::Compare {
            field,
            operator,
            value,
        } => {
            encoded.push(2);
            put_text(encoded, field)?;
            encoded.push(match operator {
                ProductSearchOperator::Equal => 0,
                ProductSearchOperator::NotEqual => 1,
                ProductSearchOperator::Less => 2,
                ProductSearchOperator::LessOrEqual => 3,
                ProductSearchOperator::Greater => 4,
                ProductSearchOperator::GreaterOrEqual => 5,
            });
            encode_doc_value(encoded, value)?;
        }
        ProductSearchFilter::All(filters) | ProductSearchFilter::Any(filters) => {
            encoded.push(if matches!(filter, ProductSearchFilter::All(_)) {
                3
            } else {
                4
            });
            put_u32(encoded, filters.len())?;
            for filter in filters {
                encode_search_filter(encoded, filter, depth + 1)?;
            }
        }
        ProductSearchFilter::Not(filter) => {
            encoded.push(5);
            encode_search_filter(encoded, filter, depth + 1)?;
        }
        ProductSearchFilter::In { field, values } => {
            encoded.push(6);
            put_text(encoded, field)?;
            put_u32(encoded, values.len())?;
            for value in values {
                encode_doc_value(encoded, value)?;
            }
        }
        ProductSearchFilter::IsNull(field) => {
            encoded.push(7);
            put_text(encoded, field)?;
        }
        ProductSearchFilter::Like { field, pattern } => {
            encoded.push(8);
            put_text(encoded, field)?;
            put_text(encoded, pattern)?;
        }
    }
    Ok(())
}

fn decode_search_filter(
    decoder: &mut Decoder<'_>,
    depth: usize,
) -> Result<ProductSearchFilter, ProductCodecError> {
    if depth > hyphae_native_runtime::MAX_DOC_VALUE_FILTER_DEPTH {
        return Err(ProductCodecError::LimitExceeded);
    }
    Ok(match decoder.u8()? {
        0 => ProductSearchFilter::MatchAll,
        1 => ProductSearchFilter::Exists(decoder.text()?),
        2 => {
            let field = decoder.text()?;
            let operator = match decoder.u8()? {
                0 => ProductSearchOperator::Equal,
                1 => ProductSearchOperator::NotEqual,
                2 => ProductSearchOperator::Less,
                3 => ProductSearchOperator::LessOrEqual,
                4 => ProductSearchOperator::Greater,
                5 => ProductSearchOperator::GreaterOrEqual,
                _ => return Err(ProductCodecError::InvalidValue),
            };
            ProductSearchFilter::Compare {
                field,
                operator,
                value: decode_doc_value(decoder)?,
            }
        }
        tag @ (3 | 4) => {
            let count = decoder.usize_u32()?;
            if count > hyphae_native_runtime::MAX_DOC_VALUE_FILTER_NODES {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut filters = Vec::with_capacity(count);
            for _ in 0..count {
                filters.push(decode_search_filter(decoder, depth + 1)?);
            }
            if tag == 3 {
                ProductSearchFilter::All(filters)
            } else {
                ProductSearchFilter::Any(filters)
            }
        }
        5 => ProductSearchFilter::Not(Box::new(decode_search_filter(decoder, depth + 1)?)),
        6 => {
            let field = decoder.text()?;
            let count = decoder.usize_u32()?;
            if count > hyphae_native_runtime::MAX_DOC_VALUE_IN_MEMBERS {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(decode_doc_value(decoder)?);
            }
            ProductSearchFilter::In { field, values }
        }
        7 => ProductSearchFilter::IsNull(decoder.text()?),
        8 => ProductSearchFilter::Like {
            field: decoder.text()?,
            pattern: decoder.text()?,
        },
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn encode_search_ingest_batch(
    encoded: &mut Vec<u8>,
    batch: &ProductSearchIngestBatch,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&batch.idempotency_id.to_le_bytes());
    put_u32(encoded, batch.documents.len())?;
    for document in &batch.documents {
        encode_product_document(encoded, document)?;
    }
    Ok(())
}

fn decode_search_ingest_batch(
    decoder: &mut Decoder<'_>,
) -> Result<ProductSearchIngestBatch, ProductCodecError> {
    let idempotency_id = decoder.u128()?;
    let count = decoder.usize_u32()?;
    if idempotency_id == 0
        || count == 0
        || count > hyphae_native_product::MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS
    {
        return Err(ProductCodecError::InvalidValue);
    }
    let mut documents = Vec::with_capacity(count);
    for _ in 0..count {
        documents.push(decode_product_document(decoder)?);
    }
    Ok(ProductSearchIngestBatch {
        idempotency_id,
        documents,
    })
}

fn encode_product_document(
    encoded: &mut Vec<u8>,
    document: &ProductDocument,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&document.object_id.get().to_le_bytes());
    put_text(encoded, &document.text)?;
    put_u32(encoded, document.doc_values.len())?;
    for (name, value) in &document.doc_values {
        put_text(encoded, name)?;
        encode_doc_value(encoded, value)?;
    }
    put_u32(encoded, document.vectors.len())?;
    for (name, vector) in &document.vectors {
        put_text(encoded, name)?;
        put_u32(encoded, vector.dimension())?;
        for value in vector.values() {
            encoded.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }
    Ok(())
}

fn decode_product_document(
    decoder: &mut Decoder<'_>,
) -> Result<ProductDocument, ProductCodecError> {
    let object_id = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    let text = decoder.text()?;
    let value_count = decoder.usize_u32()?;
    if value_count > hyphae_native_runtime::MAX_DOC_VALUES_PER_CANDIDATE {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut doc_values = std::collections::BTreeMap::new();
    for _ in 0..value_count {
        let name = decoder.text()?;
        if doc_values
            .insert(name, decode_doc_value(decoder)?)
            .is_some()
        {
            return Err(ProductCodecError::InvalidValue);
        }
    }
    let vector_count = decoder.usize_u32()?;
    if vector_count > hyphae_native_product::MAX_PRODUCT_SEARCH_VECTOR_TARGETS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut vectors = std::collections::BTreeMap::new();
    for _ in 0..vector_count {
        let name = decoder.text()?;
        let dimension = decoder.usize_u32()?;
        let mut values = Vec::with_capacity(dimension);
        for _ in 0..dimension {
            values.push(f32::from_bits(decoder.u32()?));
        }
        let vector = hyphae_native_product::ProductVector::new(values)
            .map_err(|_| ProductCodecError::InvalidValue)?;
        if vectors.insert(name, vector).is_some() {
            return Err(ProductCodecError::InvalidValue);
        }
    }
    Ok(ProductDocument {
        object_id,
        text,
        doc_values,
        vectors,
    })
}

fn encode_search_ingest_receipt(
    encoded: &mut Vec<u8>,
    receipt: &ProductSearchIngestReceipt,
) -> Result<(), ProductCodecError> {
    encode_snapshot(encoded, receipt.snapshot);
    encoded.push(u8::from(receipt.commit.is_some()));
    encoded.push(u8::from(receipt.idempotent_replay));
    encoded.extend_from_slice(&[0; 6]);
    put_u64(encoded, receipt.documents)?;
    if let Some(commit) = receipt.commit {
        encode_receipt(encoded, commit)?;
    }
    Ok(())
}

fn decode_search_ingest_receipt(
    decoder: &mut Decoder<'_>,
) -> Result<ProductSearchIngestReceipt, ProductCodecError> {
    let snapshot = decode_snapshot(decoder)?;
    let has_commit = decoder.boolean()?;
    let idempotent_replay = decoder.boolean()?;
    if decoder.bytes(6)? != [0; 6] {
        return Err(ProductCodecError::Malformed);
    }
    let documents = decoder.usize()?;
    let commit = has_commit.then(|| decode_receipt(decoder)).transpose()?;
    Ok(ProductSearchIngestReceipt {
        snapshot,
        commit,
        documents,
        idempotent_replay,
    })
}

fn encode_aggregation_value(
    encoded: &mut Vec<u8>,
    value: &hyphae_native_product::ProductAggregationValue,
) -> Result<(), ProductCodecError> {
    match value {
        hyphae_native_product::ProductAggregationValue::Count(value) => {
            encoded.push(0);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        hyphae_native_product::ProductAggregationValue::Integer(value) => {
            encoded.push(1);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                encoded.extend_from_slice(&value.to_le_bytes());
            }
        }
        hyphae_native_product::ProductAggregationValue::Value(value) => {
            encoded.push(2);
            encoded.push(u8::from(value.is_some()));
            if let Some(value) = value {
                encode_doc_value(encoded, value)?;
            }
        }
    }
    Ok(())
}

fn decode_aggregation_value(
    decoder: &mut Decoder<'_>,
) -> Result<hyphae_native_product::ProductAggregationValue, ProductCodecError> {
    Ok(match decoder.u8()? {
        0 => hyphae_native_product::ProductAggregationValue::Count(decoder.u64()?),
        1 => {
            let present = decoder.boolean()?;
            hyphae_native_product::ProductAggregationValue::Integer(
                present.then(|| decoder.i128()).transpose()?,
            )
        }
        2 => {
            let present = decoder.boolean()?;
            hyphae_native_product::ProductAggregationValue::Value(
                present.then(|| decode_doc_value(decoder)).transpose()?,
            )
        }
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn encode_integrated_search(
    encoded: &mut Vec<u8>,
    result: &ProductSearchResult,
) -> Result<(), ProductCodecError> {
    encode_snapshot(encoded, result.snapshot);
    put_u32(encoded, result.hits.len())?;
    for hit in &result.hits {
        encoded.extend_from_slice(&hit.object_id.get().to_le_bytes());
        encoded.extend_from_slice(&hit.score.to_bits().to_le_bytes());
        put_u32(encoded, hit.doc_values.len())?;
        for (name, value) in &hit.doc_values {
            put_text(encoded, name)?;
            encode_doc_value(encoded, value)?;
        }
    }
    put_u32(encoded, result.facets.len())?;
    for facet in &result.facets {
        put_text(encoded, &facet.field)?;
        put_u32(encoded, facet.buckets.len())?;
        for bucket in &facet.buckets {
            encode_doc_value(encoded, &bucket.value)?;
            encoded.extend_from_slice(&bucket.count.to_le_bytes());
        }
    }
    put_u32(encoded, result.aggregations.len())?;
    for aggregation in &result.aggregations {
        put_text(encoded, &aggregation.name)?;
        encode_aggregation_value(encoded, &aggregation.value)?;
    }
    put_u32(encoded, result.vector_branches.len())?;
    for branch in &result.vector_branches {
        put_text(encoded, &branch.target)?;
        encoded.push(match branch.strategy {
            ProductVectorStrategy::ExactFiltered => 0,
            ProductVectorStrategy::AdaptiveExactFiltered => 1,
            ProductVectorStrategy::FilterAwareAnn => 2,
            ProductVectorStrategy::AdaptiveFilterAwareAnn => 3,
        });
        encoded.push(u8::from(branch.approximate));
        encoded.push(u8::from(branch.exact_reranked));
        encoded.extend_from_slice(&[0; 5]);
        put_u64(encoded, branch.eligible_documents)?;
        put_u64(encoded, branch.candidate_count)?;
        put_u64(encoded, branch.visited_nodes)?;
    }
    encoded.push(u8::from(result.approximate));
    encoded.extend_from_slice(&[0; 7]);
    for count in [
        result.total_documents,
        result.eligible_documents,
        result.lexical_candidates,
        result.retrieval_candidates,
        result.matched_candidates,
    ] {
        put_u64(encoded, count)?;
    }
    // Content-derived tagged tail: a result without fragments keeps the
    // exact historical bytes, and fragments only exist when the request
    // carried a highlight budget — which minor gating already bounds.
    if result.hits.iter().any(|hit| !hit.fragments.is_empty()) {
        encoded.push(1);
        for hit in &result.hits {
            put_u32(encoded, hit.fragments.len())?;
            for fragment in &hit.fragments {
                put_text(encoded, fragment)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn decode_integrated_search(
    decoder: &mut Decoder<'_>,
) -> Result<ProductSearchResult, ProductCodecError> {
    let snapshot = decode_snapshot(decoder)?;
    let hit_count = decoder.usize_u32()?;
    if hit_count > hyphae_native_product::MAX_PRODUCT_SEARCH_HITS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut hits = Vec::with_capacity(hit_count);
    for _ in 0..hit_count {
        let object_id =
            ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        let score = f64::from_bits(decoder.u64()?);
        if !score.is_finite() || score < 0.0 {
            return Err(ProductCodecError::InvalidValue);
        }
        let value_count = decoder.usize_u32()?;
        if value_count > hyphae_native_runtime::MAX_DOC_VALUES_PER_CANDIDATE {
            return Err(ProductCodecError::LimitExceeded);
        }
        let mut doc_values = std::collections::BTreeMap::new();
        for _ in 0..value_count {
            let name = decoder.text()?;
            if doc_values
                .insert(name, decode_doc_value(decoder)?)
                .is_some()
            {
                return Err(ProductCodecError::InvalidValue);
            }
        }
        hits.push(ProductIntegratedSearchHit {
            object_id,
            score,
            doc_values,
            fragments: Vec::new(),
        });
    }
    let facet_count = decoder.usize_u32()?;
    if facet_count > hyphae_native_runtime::MAX_DOC_VALUE_FACETS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut facets = Vec::with_capacity(facet_count);
    for _ in 0..facet_count {
        let field = decoder.text()?;
        let bucket_count = decoder.usize_u32()?;
        if bucket_count > hyphae_native_runtime::MAX_DOC_VALUE_FACET_TERMS {
            return Err(ProductCodecError::LimitExceeded);
        }
        let mut buckets = Vec::with_capacity(bucket_count);
        for _ in 0..bucket_count {
            buckets.push(hyphae_native_product::ProductFacetBucket {
                value: decode_doc_value(decoder)?,
                count: decoder.u64()?,
            });
        }
        facets.push(hyphae_native_product::ProductFacetResult { field, buckets });
    }
    let aggregation_count = decoder.usize_u32()?;
    if aggregation_count > hyphae_native_runtime::MAX_DOC_VALUE_AGGREGATIONS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut aggregations = Vec::with_capacity(aggregation_count);
    for _ in 0..aggregation_count {
        aggregations.push(ProductNamedAggregationValue {
            name: decoder.text()?,
            value: decode_aggregation_value(decoder)?,
        });
    }
    let branch_count = decoder.usize_u32()?;
    if branch_count > hyphae_native_product::MAX_PRODUCT_SEARCH_VECTOR_TARGETS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut vector_branches = Vec::with_capacity(branch_count);
    for _ in 0..branch_count {
        let target = decoder.text()?;
        let strategy = match decoder.u8()? {
            0 => ProductVectorStrategy::ExactFiltered,
            1 => ProductVectorStrategy::AdaptiveExactFiltered,
            2 => ProductVectorStrategy::FilterAwareAnn,
            3 => ProductVectorStrategy::AdaptiveFilterAwareAnn,
            _ => return Err(ProductCodecError::InvalidValue),
        };
        let approximate = decoder.boolean()?;
        let exact_reranked = decoder.boolean()?;
        if decoder.bytes(5)? != [0; 5] {
            return Err(ProductCodecError::Malformed);
        }
        vector_branches.push(ProductVectorBranchReceipt {
            target,
            strategy,
            approximate,
            eligible_documents: decoder.usize()?,
            candidate_count: decoder.usize()?,
            visited_nodes: decoder.usize()?,
            exact_reranked,
        });
    }
    let approximate = decoder.boolean()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    let total_documents = decoder.usize()?;
    let eligible_documents = decoder.usize()?;
    let lexical_candidates = decoder.usize()?;
    let retrieval_candidates = decoder.usize()?;
    let matched_candidates = decoder.usize()?;
    if decoder.has_remaining() {
        if decoder.u8()? != 1 {
            return Err(ProductCodecError::InvalidValue);
        }
        let mut any = false;
        for hit in &mut hits {
            let fragment_count = decoder.usize_u32()?;
            if fragment_count > hyphae_native_product::MAX_HIGHLIGHT_FRAGMENTS {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut fragments = Vec::with_capacity(fragment_count);
            for _ in 0..fragment_count {
                let fragment = decoder.text()?;
                if fragment.len() > hyphae_native_product::MAX_HIGHLIGHT_FRAGMENT_BYTES {
                    return Err(ProductCodecError::LimitExceeded);
                }
                fragments.push(fragment);
            }
            any |= !fragments.is_empty();
            hit.fragments = fragments;
        }
        if !any {
            return Err(ProductCodecError::InvalidValue);
        }
    }
    Ok(ProductSearchResult {
        snapshot,
        hits,
        facets,
        aggregations,
        vector_branches,
        approximate,
        total_documents,
        eligible_documents,
        lexical_candidates,
        retrieval_candidates,
        matched_candidates,
    })
}

fn encode_doc_value(
    encoded: &mut Vec<u8>,
    value: &ProductDocValue,
) -> Result<(), ProductCodecError> {
    match value {
        ProductDocValue::Boolean(value) => {
            encoded.push(0);
            encoded.push(u8::from(*value));
        }
        ProductDocValue::Integer(value) => {
            encoded.push(1);
            encoded.extend_from_slice(&value.to_le_bytes());
        }
        ProductDocValue::String(value) => {
            encoded.push(2);
            put_text(encoded, value)?;
        }
        ProductDocValue::Bytes(value) => {
            encoded.push(3);
            put_bytes(encoded, value)?;
        }
    }
    Ok(())
}

fn decode_doc_value(decoder: &mut Decoder<'_>) -> Result<ProductDocValue, ProductCodecError> {
    match decoder.u8()? {
        0 => Ok(ProductDocValue::Boolean(decoder.boolean()?)),
        1 => Ok(ProductDocValue::Integer(decoder.i64()?)),
        2 => Ok(ProductDocValue::String(decoder.text()?)),
        3 => Ok(ProductDocValue::Bytes(decoder.owned_bytes()?)),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_qualified_name(
    encoded: &mut Vec<u8>,
    name: &QualifiedName,
) -> Result<(), ProductCodecError> {
    put_text(encoded, name.database.display())?;
    put_text(encoded, name.database.lookup())?;
    put_text(encoded, name.schema.display())?;
    put_text(encoded, name.schema.lookup())?;
    put_text(encoded, name.object.display())?;
    put_text(encoded, name.object.lookup())?;
    Ok(())
}

fn decode_qualified_name(decoder: &mut Decoder<'_>) -> Result<QualifiedName, ProductCodecError> {
    fn component(decoder: &mut Decoder<'_>) -> Result<CatalogName, ProductCodecError> {
        let display = decoder.text()?;
        let lookup = decoder.text()?;
        let unquoted =
            CatalogName::unquoted(display.clone()).map_err(|_| ProductCodecError::InvalidValue)?;
        if unquoted.lookup() == lookup {
            return Ok(unquoted);
        }
        let quoted = CatalogName::quoted(display).map_err(|_| ProductCodecError::InvalidValue)?;
        if quoted.lookup() == lookup {
            Ok(quoted)
        } else {
            Err(ProductCodecError::InvalidValue)
        }
    }
    Ok(QualifiedName::new(
        component(decoder)?,
        component(decoder)?,
        component(decoder)?,
    ))
}

fn encode_cursor(encoded: &mut Vec<u8>, cursor: Option<CatalogCursor>) {
    encoded.push(u8::from(cursor.is_some()));
    encoded.extend_from_slice(&[0; 7]);
    if let Some(cursor) = cursor {
        encode_snapshot(encoded, cursor.snapshot());
        encoded.extend_from_slice(&cursor.after().get().to_le_bytes());
    }
}

fn decode_cursor(decoder: &mut Decoder<'_>) -> Result<Option<CatalogCursor>, ProductCodecError> {
    let present = decoder.u8()?;
    if present > 1 || decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    if present == 0 {
        return Ok(None);
    }
    let snapshot = decode_snapshot(decoder)?;
    let after = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
    Ok(Some(CatalogCursor::new(snapshot, after)))
}

fn decode_catalog_kind(tag: u8) -> Result<CatalogObjectKind, ProductCodecError> {
    match tag {
        1 => Ok(CatalogObjectKind::Database),
        2 => Ok(CatalogObjectKind::Schema),
        3 => Ok(CatalogObjectKind::Relation),
        4 => Ok(CatalogObjectKind::SecondaryIndex),
        5 => Ok(CatalogObjectKind::Keyspace),
        6 => Ok(CatalogObjectKind::Structure),
        7 => Ok(CatalogObjectKind::SearchCollection),
        8 => Ok(CatalogObjectKind::Analyzer),
        9 => Ok(CatalogObjectKind::CrossEngineLink),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn encode_catalog_page(
    encoded: &mut Vec<u8>,
    page: &CatalogPage<CatalogObjectSummary>,
) -> Result<(), ProductCodecError> {
    encode_page_header(encoded, page)?;
    put_u32(encoded, page.items.len())?;
    for item in &page.items {
        encoded.extend_from_slice(&item.id.get().to_le_bytes());
        encoded.push(item.kind as u8);
        encoded.push(u8::from(item.parent.is_some()));
        encoded.extend_from_slice(&[0; 6]);
        if let Some(parent) = item.parent {
            encoded.extend_from_slice(&parent.get().to_le_bytes());
        }
        encode_qualified_name(encoded, &item.name)?;
    }
    Ok(())
}

fn decode_catalog_page(
    decoder: &mut Decoder<'_>,
) -> Result<CatalogPage<CatalogObjectSummary>, ProductCodecError> {
    let (snapshot, cursor, stop, visited, returned_bytes) = decode_page_header(decoder)?;
    let count = decoder.usize_u32()?;
    if count > 4096 {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let id = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        let kind = decode_catalog_kind(decoder.u8()?)?;
        let has_parent = decoder.u8()?;
        if has_parent > 1 || decoder.bytes(6)? != [0; 6] {
            return Err(ProductCodecError::Malformed);
        }
        items.push(CatalogObjectSummary {
            id,
            kind,
            parent: if has_parent == 1 {
                Some(ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?)
            } else {
                None
            },
            name: decode_qualified_name(decoder)?,
        });
    }
    Ok(CatalogPage {
        snapshot,
        items,
        cursor,
        stop,
        visited,
        returned_bytes,
    })
}

fn encode_catalog_visible_page(
    encoded: &mut Vec<u8>,
    page: &CatalogVisiblePage,
) -> Result<(), ProductCodecError> {
    put_bytes(
        encoded,
        page.cursor
            .as_ref()
            .map_or(&[], CatalogVisibleCursor::as_bytes),
    )?;
    put_u32(encoded, page.items.len())?;
    for item in &page.items {
        encoded.extend_from_slice(&item.id.get().to_le_bytes());
        encoded.push(item.kind as u8);
        encoded.push(u8::from(item.parent.is_some()));
        encoded.extend_from_slice(&[0; 6]);
        if let Some(parent) = item.parent {
            encoded.extend_from_slice(&parent.get().to_le_bytes());
        }
        encode_qualified_name(encoded, &item.name)?;
    }
    Ok(())
}

fn decode_catalog_visible_page(
    decoder: &mut Decoder<'_>,
) -> Result<CatalogVisiblePage, ProductCodecError> {
    let cursor = decoder.owned_bytes()?;
    let count = decoder.usize_u32()?;
    if count > 4096 {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let id = ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        let kind = decode_catalog_kind(decoder.u8()?)?;
        let has_parent = decoder.u8()?;
        if has_parent > 1 || decoder.bytes(6)? != [0; 6] {
            return Err(ProductCodecError::Malformed);
        }
        items.push(CatalogObjectSummary {
            id,
            kind,
            parent: if has_parent == 1 {
                Some(ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?)
            } else {
                None
            },
            name: decode_qualified_name(decoder)?,
        });
    }
    Ok(CatalogVisiblePage {
        items,
        cursor: if cursor.is_empty() {
            None
        } else {
            Some(CatalogVisibleCursor::new(cursor).map_err(|_| ProductCodecError::InvalidValue)?)
        },
    })
}

fn encode_dependency_page(
    encoded: &mut Vec<u8>,
    page: &CatalogPage<DependencyEdge>,
) -> Result<(), ProductCodecError> {
    encode_page_header(encoded, page)?;
    put_u32(encoded, page.items.len())?;
    for item in &page.items {
        encoded.extend_from_slice(&item.dependent.get().to_le_bytes());
        encoded.extend_from_slice(&item.prerequisite.get().to_le_bytes());
        encoded.push(item.kind as u8);
    }
    Ok(())
}

fn decode_dependency_page(
    decoder: &mut Decoder<'_>,
) -> Result<CatalogPage<DependencyEdge>, ProductCodecError> {
    let (snapshot, cursor, stop, visited, returned_bytes) = decode_page_header(decoder)?;
    let count = decoder.usize_u32()?;
    if count > 4096 {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let dependent =
            ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        let prerequisite =
            ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
        let kind = match decoder.u8()? {
            1 => DependencyKind::Parent,
            2 => DependencyKind::SecondaryIndexRelation,
            3 => DependencyKind::ForeignKey,
            4 => DependencyKind::Analyzer,
            5 => DependencyKind::LinkEndpoint,
            6 => DependencyKind::RelationSchema,
            _ => return Err(ProductCodecError::InvalidValue),
        };
        items.push(DependencyEdge::new(dependent, prerequisite, kind));
    }
    Ok(CatalogPage {
        snapshot,
        items,
        cursor,
        stop,
        visited,
        returned_bytes,
    })
}

fn encode_page_header<T>(
    encoded: &mut Vec<u8>,
    page: &CatalogPage<T>,
) -> Result<(), ProductCodecError> {
    encode_snapshot(encoded, page.snapshot);
    encode_cursor(encoded, page.cursor);
    encoded.push(match page.stop {
        CatalogPageStop::Exhausted => 0,
        CatalogPageStop::ItemLimit => 1,
        CatalogPageStop::VisitLimit => 2,
        CatalogPageStop::ByteLimit => 3,
    });
    encoded.extend_from_slice(&[0; 7]);
    put_u64(encoded, page.visited)?;
    put_u64(encoded, page.returned_bytes)?;
    Ok(())
}

type PageHeader = (
    SnapshotIdentity,
    Option<CatalogCursor>,
    CatalogPageStop,
    usize,
    usize,
);

fn decode_page_header(decoder: &mut Decoder<'_>) -> Result<PageHeader, ProductCodecError> {
    let snapshot = decode_snapshot(decoder)?;
    let cursor = decode_cursor(decoder)?;
    let stop = match decoder.u8()? {
        0 => CatalogPageStop::Exhausted,
        1 => CatalogPageStop::ItemLimit,
        2 => CatalogPageStop::VisitLimit,
        3 => CatalogPageStop::ByteLimit,
        _ => return Err(ProductCodecError::InvalidValue),
    };
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    Ok((snapshot, cursor, stop, decoder.usize()?, decoder.usize()?))
}

fn encode_doctor_report(
    encoded: &mut Vec<u8>,
    report: &DoctorReport,
) -> Result<(), ProductCodecError> {
    encoded.push(match report.status {
        DoctorStatus::Healthy => 0,
        DoctorStatus::Busy => 1,
        DoctorStatus::Corrupt => 2,
        DoctorStatus::Io => 3,
    });
    encoded.push(u8::from(report.verified_open));
    encoded.push(u8::from(report.snapshot_verified));
    encoded.push(u8::from(report.directory_lineage.is_some()));
    encoded.push(u8::from(report.recovery.is_some()));
    encoded.extend_from_slice(&[0; 3]);
    encoded.extend_from_slice(&report.telemetry_registry_version.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    encoded.extend_from_slice(&report.process_start_identity.to_le_bytes());
    encoded.extend_from_slice(&report.session_start_identity.to_le_bytes());
    if let Some(lineage) = report.directory_lineage {
        encoded.extend_from_slice(&lineage);
    }
    if let Some(recovery) = &report.recovery {
        encoded.extend_from_slice(
            &recovery
                .visible_csn
                .map_or(0, hyphae_native_product::Csn::get)
                .to_le_bytes(),
        );
        put_u64(encoded, recovery.replayed_transactions)?;
        encoded.extend_from_slice(&recovery.page_tail_bytes_removed.to_le_bytes());
        encoded.extend_from_slice(&recovery.wal_tail_bytes_removed.to_le_bytes());
        encoded.extend_from_slice(&recovery.retained_wal_bytes.to_le_bytes());
        put_u64(encoded, recovery.manifest_count)?;
        put_u64(encoded, recovery.blob_count)?;
        encoded.extend_from_slice(&recovery.open_time_micros.to_le_bytes());
    }
    Ok(())
}

fn decode_doctor_report(decoder: &mut Decoder<'_>) -> Result<DoctorReport, ProductCodecError> {
    let status = match decoder.u8()? {
        0 => DoctorStatus::Healthy,
        1 => DoctorStatus::Busy,
        2 => DoctorStatus::Corrupt,
        3 => DoctorStatus::Io,
        _ => return Err(ProductCodecError::InvalidValue),
    };
    let verified_open = decoder.boolean()?;
    let snapshot_verified = decoder.boolean()?;
    let has_lineage = decoder.boolean()?;
    let has_recovery = decoder.boolean()?;
    if decoder.bytes(3)? != [0; 3] {
        return Err(ProductCodecError::Malformed);
    }
    let telemetry_registry_version = decoder.u16()?;
    if decoder.u16()? != 0 {
        return Err(ProductCodecError::Malformed);
    }
    let process_start_identity = decoder.u128()?;
    let session_start_identity = decoder.u128()?;
    let directory_lineage = has_lineage.then(|| decoder.array()).transpose()?;
    let recovery = if has_recovery {
        let visible = decoder.u64()?;
        Some(DoctorRecovery {
            visible_csn: if visible == 0 {
                None
            } else {
                Some(
                    hyphae_native_product::Csn::new(visible)
                        .map_err(|_| ProductCodecError::InvalidValue)?,
                )
            },
            replayed_transactions: decoder.usize()?,
            page_tail_bytes_removed: decoder.u64()?,
            wal_tail_bytes_removed: decoder.u64()?,
            retained_wal_bytes: decoder.u64()?,
            manifest_count: decoder.usize()?,
            blob_count: decoder.usize()?,
            open_time_micros: decoder.u64()?,
        })
    } else {
        None
    };
    Ok(DoctorReport {
        status,
        verified_open,
        snapshot_verified,
        directory_lineage,
        recovery,
        telemetry_registry_version,
        process_start_identity,
        session_start_identity,
    })
}

fn encode_telemetry(
    encoded: &mut Vec<u8>,
    snapshot: &TelemetrySnapshot,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&snapshot.registry_version.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    encoded.extend_from_slice(&snapshot.process_start_identity.to_le_bytes());
    encoded.extend_from_slice(&snapshot.session_start_identity.to_le_bytes());
    encoded.extend_from_slice(&snapshot.captured_at_micros.to_le_bytes());
    encoded.extend_from_slice(
        &snapshot
            .catalog_version
            .map_or(0, CatalogVersion::get)
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&snapshot.dropped_events.to_le_bytes());
    put_u32(encoded, snapshot.metrics.len())?;
    put_u32(encoded, snapshot.events.len())?;
    for row in &snapshot.metrics {
        put_text(encoded, row.descriptor.name)?;
        match &row.value {
            MetricValue::Counter(value) => {
                encoded.push(0);
                encoded.extend_from_slice(&value.to_le_bytes());
            }
            MetricValue::Gauge(value) => {
                encoded.push(1);
                encoded.extend_from_slice(&value.to_le_bytes());
            }
            MetricValue::Histogram {
                count,
                sum_micros,
                buckets,
            } => {
                encoded.push(2);
                encoded.extend_from_slice(&count.to_le_bytes());
                encoded.extend_from_slice(&sum_micros.to_le_bytes());
                for bucket in buckets {
                    encoded.extend_from_slice(&bucket.to_le_bytes());
                }
            }
        }
    }
    for event in &snapshot.events {
        encoded.extend_from_slice(&event.captured_at_micros.to_le_bytes());
        let (kind, category) = match event.kind {
            TelemetryEventKind::Backup => (0, 0),
            TelemetryEventKind::Restore => (1, 0),
            TelemetryEventKind::Doctor => (2, 0),
            TelemetryEventKind::Cancelled => (3, 0),
            TelemetryEventKind::Deadline => (4, 0),
            TelemetryEventKind::Error(category) => (5, product_error_category_tag(category)),
        };
        encoded.push(kind);
        encoded.push(category);
        encoded.extend_from_slice(&[0; 6]);
    }
    Ok(())
}

fn encode_explain(
    encoded: &mut Vec<u8>,
    explanation: &ProductExplain,
) -> Result<(), ProductCodecError> {
    match explanation {
        ProductExplain::SqlPlanText(value) => {
            encoded.push(0);
            encoded.extend_from_slice(&[0; 3]);
            encoded.extend_from_slice(&value.version.to_le_bytes());
            encoded.extend_from_slice(&0_u16.to_le_bytes());
            encoded.extend_from_slice(&value.visible_csn.unwrap_or(0).to_le_bytes());
            encoded.extend_from_slice(&value.catalog_version.to_le_bytes());
            encoded.push(u8::from(value.executed));
            encoded.extend_from_slice(&[0; 7]);
            put_text(encoded, &value.text)?;
        }
        ProductExplain::Convergence(value) => {
            encoded.push(1);
            encoded.extend_from_slice(&[0; 3]);
            encoded.extend_from_slice(&value.snapshot_csn.unwrap_or(0).to_le_bytes());
            encoded.push(u8::from(value.inner_join_by_object_id));
            encoded.push(u8::from(value.stable_object_id_order));
            encoded.extend_from_slice(&[0; 2]);
            put_u32(encoded, value.strategies.len())?;
            for strategy in &value.strategies {
                encoded.push(convergence_strategy_tag(*strategy));
            }
        }
        ProductExplain::Ann(value) => {
            encoded.push(2);
            encoded.extend_from_slice(&[0; 3]);
            encoded.extend_from_slice(&value.index.get().to_le_bytes());
            encoded.extend_from_slice(&value.snapshot_csn.unwrap_or(0).to_le_bytes());
            encoded.push(u8::from(value.approximate));
            encoded.push(ann_strategy_tag(value.strategy));
            encoded.push(ann_recall_risk_tag(value.recall_risk));
            encoded.push(u8::from(value.exact_reranked));
            encoded.extend_from_slice(&value.build_identity);
            for count in [
                value.ef_search,
                value.candidate_count,
                value.eligible_candidate_count,
                value.visited_nodes,
            ] {
                put_u64(encoded, count)?;
            }
        }
        ProductExplain::Hybrid(value) => {
            encoded.push(3);
            encoded.extend_from_slice(&[0; 3]);
            encoded.extend_from_slice(&value.lexical_index.get().to_le_bytes());
            put_u64(encoded, value.lexical_limit)?;
            encoded.extend_from_slice(&value.vector_index.get().to_le_bytes());
            match value.vector_strategy {
                ProductHybridVectorStrategy::Exact => {
                    encoded.push(0);
                    encoded.extend_from_slice(&[0; 7]);
                }
                ProductHybridVectorStrategy::Ann {
                    k,
                    ef_search,
                    exact_rerank,
                } => {
                    encoded.push(1);
                    encoded.push(u8::from(exact_rerank.is_some()));
                    encoded.extend_from_slice(&[0; 6]);
                    put_u64(encoded, k)?;
                    put_u64(encoded, ef_search)?;
                    put_u64(encoded, exact_rerank.unwrap_or(0))?;
                }
            }
            put_u64(encoded, value.vector_limit)?;
            encoded.extend_from_slice(&value.lexical_weight.to_le_bytes());
            encoded.extend_from_slice(&value.vector_weight.to_le_bytes());
            put_u64(encoded, value.fusion_limit)?;
            encoded.extend_from_slice(&value.rrf_constant.to_le_bytes());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn decode_explain(decoder: &mut Decoder<'_>) -> Result<ProductExplain, ProductCodecError> {
    let kind = decoder.u8()?;
    if decoder.bytes(3)? != [0; 3] {
        return Err(ProductCodecError::Malformed);
    }
    Ok(match kind {
        0 => {
            let version = decoder.u16()?;
            if decoder.u16()? != 0 {
                return Err(ProductCodecError::Malformed);
            }
            let visible_csn = optional_u64(decoder.u64()?);
            let catalog_version = decoder.u64()?;
            let executed = decoder.boolean()?;
            if decoder.bytes(7)? != [0; 7] {
                return Err(ProductCodecError::Malformed);
            }
            ProductExplain::SqlPlanText(SqlPlanText {
                version,
                text: decoder.text()?,
                visible_csn,
                catalog_version,
                executed,
            })
        }
        1 => {
            let snapshot_csn = optional_u64(decoder.u64()?);
            let inner_join_by_object_id = decoder.boolean()?;
            let stable_object_id_order = decoder.boolean()?;
            if decoder.bytes(2)? != [0; 2] {
                return Err(ProductCodecError::Malformed);
            }
            let count = decoder.usize_u32()?;
            if count > hyphae_native_runtime::MAX_CONVERGENCE_SOURCES {
                return Err(ProductCodecError::LimitExceeded);
            }
            let mut strategies = Vec::with_capacity(count);
            for _ in 0..count {
                strategies.push(decode_convergence_strategy(decoder.u8()?)?);
            }
            ProductExplain::Convergence(ProductConvergenceExplanation {
                snapshot_csn,
                strategies,
                inner_join_by_object_id,
                stable_object_id_order,
            })
        }
        2 => ProductExplain::Ann(ProductAnnExplanation {
            index: ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?,
            snapshot_csn: optional_u64(decoder.u64()?),
            approximate: decoder.boolean()?,
            strategy: decode_ann_strategy(decoder.u8()?)?,
            recall_risk: decode_ann_recall_risk(decoder.u8()?)?,
            exact_reranked: decoder.boolean()?,
            build_identity: decoder.array()?,
            ef_search: decoder.usize()?,
            candidate_count: decoder.usize()?,
            eligible_candidate_count: decoder.usize()?,
            visited_nodes: decoder.usize()?,
        }),
        3 => {
            let lexical_index =
                ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
            let lexical_limit = decoder.usize()?;
            let vector_index =
                ObjectId::new(decoder.u128()?).map_err(|_| ProductCodecError::InvalidValue)?;
            let vector_strategy = match decoder.u8()? {
                0 => {
                    if decoder.bytes(7)? != [0; 7] {
                        return Err(ProductCodecError::Malformed);
                    }
                    ProductHybridVectorStrategy::Exact
                }
                1 => {
                    let has_rerank = decoder.boolean()?;
                    if decoder.bytes(6)? != [0; 6] {
                        return Err(ProductCodecError::Malformed);
                    }
                    let k = decoder.usize()?;
                    let ef_search = decoder.usize()?;
                    let exact_rerank_value = decoder.usize()?;
                    ProductHybridVectorStrategy::Ann {
                        k,
                        ef_search,
                        exact_rerank: has_rerank.then_some(exact_rerank_value),
                    }
                }
                _ => return Err(ProductCodecError::InvalidValue),
            };
            ProductExplain::Hybrid(ProductHybridExplanation {
                lexical_index,
                lexical_limit,
                vector_index,
                vector_strategy,
                vector_limit: decoder.usize()?,
                lexical_weight: decoder.u32()?,
                vector_weight: decoder.u32()?,
                fusion_limit: decoder.usize()?,
                rrf_constant: decoder.u64()?,
            })
        }
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

const fn convergence_strategy_tag(value: ProductConvergenceStrategy) -> u8 {
    match value {
        ProductConvergenceStrategy::ScalarLookup => 0,
        ProductConvergenceStrategy::HashRange => 1,
        ProductConvergenceStrategy::SetRange => 2,
        ProductConvergenceStrategy::ListRange => 3,
        ProductConvergenceStrategy::SortedSetRange => 4,
        ProductConvergenceStrategy::StreamRange => 5,
        ProductConvergenceStrategy::LexicalTopK => 6,
        ProductConvergenceStrategy::ExactVectorTopK => 7,
        ProductConvergenceStrategy::AnnTopK => 8,
        ProductConvergenceStrategy::HybridRrf => 9,
    }
}

fn decode_convergence_strategy(value: u8) -> Result<ProductConvergenceStrategy, ProductCodecError> {
    Ok(match value {
        0 => ProductConvergenceStrategy::ScalarLookup,
        1 => ProductConvergenceStrategy::HashRange,
        2 => ProductConvergenceStrategy::SetRange,
        3 => ProductConvergenceStrategy::ListRange,
        4 => ProductConvergenceStrategy::SortedSetRange,
        5 => ProductConvergenceStrategy::StreamRange,
        6 => ProductConvergenceStrategy::LexicalTopK,
        7 => ProductConvergenceStrategy::ExactVectorTopK,
        8 => ProductConvergenceStrategy::AnnTopK,
        9 => ProductConvergenceStrategy::HybridRrf,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

const fn ann_strategy_tag(value: ProductAnnStrategy) -> u8 {
    match value {
        ProductAnnStrategy::GraphTraversal => 0,
        ProductAnnStrategy::StableIdAllowlistPostFilter => 1,
        ProductAnnStrategy::StableIdEligibilityTraversal => 2,
        ProductAnnStrategy::StableIdAdaptiveExact => 3,
    }
}

fn decode_ann_strategy(value: u8) -> Result<ProductAnnStrategy, ProductCodecError> {
    Ok(match value {
        0 => ProductAnnStrategy::GraphTraversal,
        1 => ProductAnnStrategy::StableIdAllowlistPostFilter,
        2 => ProductAnnStrategy::StableIdEligibilityTraversal,
        3 => ProductAnnStrategy::StableIdAdaptiveExact,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

const fn ann_recall_risk_tag(value: ProductAnnRecallRisk) -> u8 {
    match value {
        ProductAnnRecallRisk::ApproximateTraversal => 0,
        ProductAnnRecallRisk::PostFilterMayMissAllowedNeighbors => 1,
        ProductAnnRecallRisk::FilteredApproximateTraversal => 2,
        ProductAnnRecallRisk::ExactFilteredCandidates => 3,
    }
}

fn decode_ann_recall_risk(value: u8) -> Result<ProductAnnRecallRisk, ProductCodecError> {
    Ok(match value {
        0 => ProductAnnRecallRisk::ApproximateTraversal,
        1 => ProductAnnRecallRisk::PostFilterMayMissAllowedNeighbors,
        2 => ProductAnnRecallRisk::FilteredApproximateTraversal,
        3 => ProductAnnRecallRisk::ExactFilteredCandidates,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

const fn optional_u64(value: u64) -> Option<u64> {
    if value == 0 { None } else { Some(value) }
}

fn decode_telemetry(decoder: &mut Decoder<'_>) -> Result<TelemetrySnapshot, ProductCodecError> {
    let registry_version = decoder.u16()?;
    if decoder.u16()? != 0 {
        return Err(ProductCodecError::Malformed);
    }
    let process_start_identity = decoder.u128()?;
    let session_start_identity = decoder.u128()?;
    let captured_at_micros = decoder.i64()?;
    let catalog_version = match decoder.u64()? {
        0 => None,
        value => Some(CatalogVersion::new(value).map_err(|_| ProductCodecError::InvalidValue)?),
    };
    let dropped_events = decoder.u64()?;
    let metric_count = decoder.usize_u32()?;
    let event_count = decoder.usize_u32()?;
    if metric_count > 256 || event_count > hyphae_native_product::MAX_TELEMETRY_EVENTS {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut metrics = Vec::with_capacity(metric_count);
    for _ in 0..metric_count {
        let name = decoder.text()?;
        let descriptor = hyphae_native_product::METRIC_REGISTRY_V1
            .iter()
            .copied()
            .find(|candidate| candidate.name == name)
            .ok_or(ProductCodecError::InvalidValue)?;
        let value = match decoder.u8()? {
            0 if descriptor.kind == MetricKind::Counter => MetricValue::Counter(decoder.u64()?),
            1 if descriptor.kind == MetricKind::Gauge => MetricValue::Gauge(decoder.u64()?),
            2 if descriptor.kind == MetricKind::Histogram => {
                let count = decoder.u64()?;
                let sum_micros = decoder.u64()?;
                let mut buckets =
                    [0_u64; hyphae_native_product::TELEMETRY_HISTOGRAM_BOUNDS_MICROS.len() + 1];
                for bucket in &mut buckets {
                    *bucket = decoder.u64()?;
                }
                MetricValue::Histogram {
                    count,
                    sum_micros,
                    buckets,
                }
            }
            _ => return Err(ProductCodecError::InvalidValue),
        };
        metrics.push(hyphae_native_product::MetricRow { descriptor, value });
    }
    let mut events = Vec::with_capacity(event_count);
    for _ in 0..event_count {
        let captured_at_micros = decoder.i64()?;
        let kind = decoder.u8()?;
        let category = decoder.u8()?;
        if decoder.bytes(6)? != [0; 6] {
            return Err(ProductCodecError::Malformed);
        }
        let kind = match (kind, category) {
            (0, 0) => TelemetryEventKind::Backup,
            (1, 0) => TelemetryEventKind::Restore,
            (2, 0) => TelemetryEventKind::Doctor,
            (3, 0) => TelemetryEventKind::Cancelled,
            (4, 0) => TelemetryEventKind::Deadline,
            (5, category) => TelemetryEventKind::Error(decode_product_error_category(category)?),
            _ => return Err(ProductCodecError::InvalidValue),
        };
        events.push(TelemetryEvent {
            captured_at_micros,
            kind,
        });
    }
    Ok(TelemetrySnapshot {
        registry_version,
        process_start_identity,
        session_start_identity,
        captured_at_micros,
        catalog_version,
        metrics,
        events,
        dropped_events,
    })
}

fn product_error_category_tag(value: hyphae_native_product::ProductErrorCategory) -> u8 {
    match value {
        hyphae_native_product::ProductErrorCategory::InvalidRequest => 0,
        hyphae_native_product::ProductErrorCategory::NotFound => 1,
        hyphae_native_product::ProductErrorCategory::Conflict => 2,
        hyphae_native_product::ProductErrorCategory::Limit => 3,
        hyphae_native_product::ProductErrorCategory::Deadline => 4,
        hyphae_native_product::ProductErrorCategory::Cancelled => 5,
        hyphae_native_product::ProductErrorCategory::Authorization => 6,
        hyphae_native_product::ProductErrorCategory::Corruption => 7,
        hyphae_native_product::ProductErrorCategory::Unavailable => 8,
        hyphae_native_product::ProductErrorCategory::Io => 9,
        hyphae_native_product::ProductErrorCategory::Internal => 10,
        _ => u8::MAX,
    }
}

fn decode_product_error_category(
    value: u8,
) -> Result<hyphae_native_product::ProductErrorCategory, ProductCodecError> {
    Ok(match value {
        0 => hyphae_native_product::ProductErrorCategory::InvalidRequest,
        1 => hyphae_native_product::ProductErrorCategory::NotFound,
        2 => hyphae_native_product::ProductErrorCategory::Conflict,
        3 => hyphae_native_product::ProductErrorCategory::Limit,
        4 => hyphae_native_product::ProductErrorCategory::Deadline,
        5 => hyphae_native_product::ProductErrorCategory::Cancelled,
        6 => hyphae_native_product::ProductErrorCategory::Authorization,
        7 => hyphae_native_product::ProductErrorCategory::Corruption,
        8 => hyphae_native_product::ProductErrorCategory::Unavailable,
        9 => hyphae_native_product::ProductErrorCategory::Io,
        10 => hyphae_native_product::ProductErrorCategory::Internal,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn decode_proof_kind(
    value: u8,
) -> Result<hyphae_native_product::proof::NativeProofKind, ProductCodecError> {
    Ok(match value {
        1 => hyphae_native_product::proof::NativeProofKind::Point,
        2 => hyphae_native_product::proof::NativeProofKind::Sql,
        3 => hyphae_native_product::proof::NativeProofKind::Lexical,
        4 => hyphae_native_product::proof::NativeProofKind::ExactVector,
        5 => hyphae_native_product::proof::NativeProofKind::Ann,
        6 => hyphae_native_product::proof::NativeProofKind::Hybrid,
        7 => hyphae_native_product::proof::NativeProofKind::Catalog,
        _ => return Err(ProductCodecError::InvalidValue),
    })
}

fn encode_snapshot(encoded: &mut Vec<u8>, value: SnapshotIdentity) {
    encoded.extend_from_slice(&value.directory_lineage);
    encoded.extend_from_slice(
        &value
            .visible_csn
            .map_or(0, hyphae_native_product::Csn::get)
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&value.catalog_version.get().to_le_bytes());
    encoded.extend_from_slice(&value.root_digest);
    encoded.extend_from_slice(&value.logical_time_micros.to_le_bytes());
}

fn decode_snapshot(decoder: &mut Decoder<'_>) -> Result<SnapshotIdentity, ProductCodecError> {
    let directory_lineage = decoder.array::<24>()?;
    let visible = decoder.u64()?;
    Ok(SnapshotIdentity {
        directory_lineage,
        visible_csn: if visible == 0 {
            None
        } else {
            Some(
                hyphae_native_product::Csn::new(visible)
                    .map_err(|_| ProductCodecError::InvalidValue)?,
            )
        },
        catalog_version: CatalogVersion::new(decoder.u64()?)
            .map_err(|_| ProductCodecError::InvalidValue)?,
        root_digest: decoder.array()?,
        logical_time_micros: decoder.i64()?,
    })
}

fn encode_commit_outcome(
    encoded: &mut Vec<u8>,
    value: ProductCommitOutcome,
) -> Result<(), ProductCodecError> {
    match value {
        ProductCommitOutcome::Committed(receipt) => {
            encoded.push(0);
            encoded.extend_from_slice(&[0; 7]);
            encode_receipt(encoded, receipt)?;
        }
        ProductCommitOutcome::OutcomeUnknown { transaction_id } => {
            encoded.push(1);
            encoded.extend_from_slice(&[0; 7]);
            encoded.extend_from_slice(&transaction_id.get().to_le_bytes());
        }
    }
    Ok(())
}

fn decode_commit_outcome(
    decoder: &mut Decoder<'_>,
) -> Result<ProductCommitOutcome, ProductCodecError> {
    let tag = decoder.u8()?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    match tag {
        0 => Ok(ProductCommitOutcome::Committed(decode_receipt(decoder)?)),
        1 => Ok(ProductCommitOutcome::OutcomeUnknown {
            transaction_id: ProductTransactionId::new(decoder.u128()?)
                .ok_or(ProductCodecError::InvalidValue)?,
        }),
        _ => Err(ProductCodecError::Malformed),
    }
}

fn encode_receipt(
    encoded: &mut Vec<u8>,
    value: ProductCommitReceipt,
) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(&value.transaction_id.get().to_le_bytes());
    encoded.extend_from_slice(&value.commit_csn.to_le_bytes());
    encoded.extend_from_slice(&value.catalog_version.to_le_bytes());
    encoded.extend_from_slice(&value.commit_lsn.to_le_bytes());
    encoded.extend_from_slice(&value.wal_block_digest);
    encoded.push(durability_tag(value.durability));
    encoded.extend_from_slice(&[0; 7]);
    put_u64(encoded, value.durability_cohort_size)?;
    put_u64(encoded, value.durability_cohort_position)?;
    Ok(())
}

fn decode_receipt(decoder: &mut Decoder<'_>) -> Result<ProductCommitReceipt, ProductCodecError> {
    let transaction_id =
        ProductTransactionId::new(decoder.u128()?).ok_or(ProductCodecError::InvalidValue)?;
    let committed_sequence = decoder.u64()?;
    let catalog_version = decoder.u64()?;
    let commit_lsn = decoder.u64()?;
    let wal_block_digest = decoder.array()?;
    let durability = decode_durability(decoder.u8()?)?;
    if decoder.bytes(7)? != [0; 7] {
        return Err(ProductCodecError::Malformed);
    }
    Ok(ProductCommitReceipt {
        transaction_id,
        commit_csn: committed_sequence,
        catalog_version,
        commit_lsn,
        wal_block_digest,
        durability,
        durability_cohort_size: decoder.usize()?,
        durability_cohort_position: decoder.usize()?,
    })
}

fn encode_transaction_status(
    encoded: &mut Vec<u8>,
    value: ProductTransactionStatus,
) -> Result<(), ProductCodecError> {
    match value {
        ProductTransactionStatus::Unknown => encoded.push(0),
        ProductTransactionStatus::Committed(receipt) => {
            encoded.push(1);
            encode_receipt(encoded, receipt)?;
        }
        ProductTransactionStatus::RolledBack { transaction_id } => {
            encoded.push(2);
            encoded.extend_from_slice(&transaction_id.get().to_le_bytes());
        }
        ProductTransactionStatus::OutcomeUnknown { transaction_id } => {
            encoded.push(3);
            encoded.extend_from_slice(&transaction_id.get().to_le_bytes());
        }
    }
    Ok(())
}

fn decode_transaction_status(
    decoder: &mut Decoder<'_>,
) -> Result<ProductTransactionStatus, ProductCodecError> {
    match decoder.u8()? {
        0 => Ok(ProductTransactionStatus::Unknown),
        1 => Ok(ProductTransactionStatus::Committed(decode_receipt(
            decoder,
        )?)),
        2 => Ok(ProductTransactionStatus::RolledBack {
            transaction_id: ProductTransactionId::new(decoder.u128()?)
                .ok_or(ProductCodecError::InvalidValue)?,
        }),
        3 => Ok(ProductTransactionStatus::OutcomeUnknown {
            transaction_id: ProductTransactionId::new(decoder.u128()?)
                .ok_or(ProductCodecError::InvalidValue)?,
        }),
        _ => Err(ProductCodecError::Malformed),
    }
}

fn envelope(magic: [u8; 8], kind: u16, payload: &[u8]) -> Result<Vec<u8>, ProductCodecError> {
    let length = HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(ProductCodecError::LimitExceeded)?;
    if length > MAX_PRODUCT_WIRE_BYTES {
        return Err(ProductCodecError::LimitExceeded);
    }
    let mut encoded = Vec::with_capacity(length);
    encoded.extend_from_slice(&magic);
    encoded.extend_from_slice(
        &u32::try_from(length)
            .map_err(|_| ProductCodecError::LimitExceeded)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&kind.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

fn decode_envelope(encoded: &[u8], magic: [u8; 8]) -> Result<(u16, &[u8]), ProductCodecError> {
    if encoded.len() < HEADER_SIZE {
        return Err(ProductCodecError::Truncated);
    }
    if encoded.len() > MAX_PRODUCT_WIRE_BYTES
        || encoded[..8] != magic
        || read_u32(&encoded[8..12]) as usize != encoded.len()
        || read_u16(&encoded[14..16]) != 0
    {
        return Err(ProductCodecError::Malformed);
    }
    Ok((read_u16(&encoded[12..14]), &encoded[HEADER_SIZE..]))
}

fn durability_tag(value: ProductDurability) -> u8 {
    match value {
        ProductDurability::Strict => 0,
        ProductDurability::Group => 1,
        ProductDurability::Memory => 2,
    }
}

fn decode_durability(tag: u8) -> Result<ProductDurability, ProductCodecError> {
    match tag {
        0 => Ok(ProductDurability::Strict),
        1 => Ok(ProductDurability::Group),
        2 => Ok(ProductDurability::Memory),
        _ => Err(ProductCodecError::InvalidValue),
    }
}

fn put_bytes(encoded: &mut Vec<u8>, value: &[u8]) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(
        &u32::try_from(value.len())
            .map_err(|_| ProductCodecError::LimitExceeded)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(value);
    Ok(())
}

fn put_text(encoded: &mut Vec<u8>, value: &str) -> Result<(), ProductCodecError> {
    put_bytes(encoded, value.as_bytes())
}

fn put_path(encoded: &mut Vec<u8>, value: &std::path::Path) -> Result<(), ProductCodecError> {
    put_bytes(encoded, value.as_os_str().as_encoded_bytes())
}

fn put_u32(encoded: &mut Vec<u8>, value: usize) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(
        &u32::try_from(value)
            .map_err(|_| ProductCodecError::LimitExceeded)?
            .to_le_bytes(),
    );
    Ok(())
}

fn put_u64(encoded: &mut Vec<u8>, value: usize) -> Result<(), ProductCodecError> {
    encoded.extend_from_slice(
        &u64::try_from(value)
            .map_err(|_| ProductCodecError::LimitExceeded)?
            .to_le_bytes(),
    );
    Ok(())
}

fn read_usize(encoded: &[u8]) -> Result<usize, ProductCodecError> {
    usize::try_from(read_u64(encoded)).map_err(|_| ProductCodecError::LimitExceeded)
}

fn read_u16(encoded: &[u8]) -> u16 {
    u16::from_le_bytes(encoded.try_into().unwrap_or([0; 2]))
}

fn read_u128(encoded: &[u8]) -> u128 {
    u128::from_le_bytes(encoded[..16].try_into().unwrap_or([0; 16]))
}

fn read_u32(encoded: &[u8]) -> u32 {
    u32::from_le_bytes(encoded.try_into().unwrap_or([0; 4]))
}

fn read_u64(encoded: &[u8]) -> u64 {
    u64::from_le_bytes(encoded.try_into().unwrap_or([0; 8]))
}

fn read_i64(encoded: &[u8]) -> i64 {
    i64::from_le_bytes(encoded.try_into().unwrap_or([0; 8]))
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    const fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    const fn has_remaining(&self) -> bool {
        !self.remaining.is_empty()
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], ProductCodecError> {
        if length > self.remaining.len() {
            return Err(ProductCodecError::Truncated);
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn owned_bytes(&mut self) -> Result<Vec<u8>, ProductCodecError> {
        let length = self.usize_u32()?;
        if length > MAX_PRODUCT_WIRE_BYTES {
            return Err(ProductCodecError::LimitExceeded);
        }
        Ok(self.bytes(length)?.to_vec())
    }

    fn text(&mut self) -> Result<String, ProductCodecError> {
        String::from_utf8(self.owned_bytes()?).map_err(|_| ProductCodecError::InvalidValue)
    }

    fn path(&mut self) -> Result<std::path::PathBuf, ProductCodecError> {
        let encoded = self.owned_bytes()?;
        let text = std::str::from_utf8(&encoded).map_err(|_| ProductCodecError::InvalidValue)?;
        Ok(text.into())
    }

    fn boolean(&mut self) -> Result<bool, ProductCodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProductCodecError::InvalidValue),
        }
    }

    fn u8(&mut self) -> Result<u8, ProductCodecError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProductCodecError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ProductCodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ProductCodecError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ProductCodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, ProductCodecError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn i128(&mut self) -> Result<i128, ProductCodecError> {
        Ok(i128::from_le_bytes(self.array()?))
    }

    fn u128(&mut self) -> Result<u128, ProductCodecError> {
        Ok(u128::from_le_bytes(self.array()?))
    }

    fn usize(&mut self) -> Result<usize, ProductCodecError> {
        usize::try_from(self.u64()?).map_err(|_| ProductCodecError::LimitExceeded)
    }

    fn usize_u32(&mut self) -> Result<usize, ProductCodecError> {
        usize::try_from(self.u32()?).map_err(|_| ProductCodecError::LimitExceeded)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProductCodecError> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| ProductCodecError::Truncated)
    }

    fn finish(self) -> Result<(), ProductCodecError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(ProductCodecError::Malformed)
        }
    }
}
