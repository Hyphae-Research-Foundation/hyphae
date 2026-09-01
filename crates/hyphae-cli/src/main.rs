// SPDX-License-Identifier: Apache-2.0

//! Command-line entry point for the single native Hyphae executable.

mod agent;
mod agent_hooks;
mod compatibility;
mod exit;
mod json_value;
mod mcp;
mod migrate_valkey;
mod native;
mod native_client;
mod native_service;
mod tui;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufWriter, Write, stderr, stdout},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use compatibility::{BackupFamily, DirectoryFamily};
use exit::CliFailure;
use hyphae_core::current_version;
use hyphae_engine::decode_document;
use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogObjectKind,
    CatalogObjectV2, DefinitionVersion, DependencyDirection, DependencyKind, FieldSourcePolicy,
    IncrementalVectorLifecycle, KeyspaceDefinition, KeyspaceEvictionPolicy, KeyspaceMemoryClass,
    KeyspaceTtlPolicy, LexicalIndexPolicy, LogicalCatalogObject, NamedVectorDefinition,
    ObjectHeaderV2, QualifiedName, SearchCollectionDefinitionV2, SearchFieldDefinitionV2,
    SearchFieldOptions, StructureOwnership, VectorMetric, VectorSearchPolicy,
};
use hyphae_native_product::proof::{
    ExternalTrustedAnchor, NativeProofGenerationLimits, NativeProofKind, NativeVerificationLimits,
    verify_native_proof_offline,
};
use hyphae_native_product::{
    AccessControlLimits, AccessControlStatus, ApiKeyId, AuthorizationEpoch, BackupInfo,
    BackupRequest, BuiltInRole, CatalogDependencyRequest, CatalogListRequest, CatalogVisibleCursor,
    CatalogVisibleListFilter, CatalogVisibleListRequest, CompactionRequest, CompactionTarget,
    CustomRoleGrant, DoctorRequest, DoctorStatus, MetricValue, MigrationLexicalIndexInput,
    MigrationVectorIndexInput, NativeProduct, ObjectId, ProductAggregation, ProductAuthorization,
    ProductCommitOutcome, ProductCommitReceipt, ProductDocValue, ProductDocument,
    ProductDurability, ProductError, ProductErrorCode, ProductExplain,
    ProductExplicitTransactionStatus, ProductFacetRequest, ProductHashEntry, ProductLexicalBranch,
    ProductListSide, ProductMissingPlacement, ProductNamedAggregation, ProductOperation,
    ProductPermission, ProductResponse, ProductScope, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchOperator, ProductSearchRequest, ProductSearchResults, ProductSearchSort,
    ProductSetAlgebraOperation, ProductSortDirection, ProductSortSource, ProductSqlResult,
    ProductStructureKey, ProductStructureMutation, ProductStructureMutationResult,
    ProductStructureReadRequest, ProductStructureReadResult, ProductTransactionHandle,
    ProductTransactionSearchMutation, ProductTransactionSqlMutation, ProductTransactionStageResult,
    ProductTransactionStatus, ProductTransactionVectorMutation, ProductTtl, ProductValue,
    ProductVector, ProductVectorBranch, ProductVectorExecution, ProductVectorStrategy,
    ProgressControl, RestorePhase, RestoreRequest, SecurityAssignmentListRequest,
    SecurityAssignmentPage, SecurityAuditAction, SecurityAuditMetadata, SecurityAuditPage,
    SecurityAuditReadRequest, SecurityAuditResult, SecurityAuditTarget, SecurityCursor, SecurityId,
    SecurityKeyListRequest, SecurityKeyPage, SecurityPrincipalListRequest, SecurityPrincipalPage,
    SecurityRoleListRequest, SecurityRolePage, SecurityRoleSummary, SnapshotIdentity,
    StructureKind, VerifyBackupRequest, capabilities, verify_backup,
};
use hyphae_native_runtime::{
    CalibrationMode, CalibrationRequest, GovernorMode, GovernorPolicyError, HardwareCalibration,
    HardwareProfile, MigrationDocument, MigrationLexicalField, MigrationLexicalIndex,
    MigrationManifest, MigrationObject, MigrationProofAnchor, MigrationReceipt, MigrationSource,
    MigrationTarget, MigrationVectorSpace, NativeExecutionTopology, NativeGovernorPolicy,
};
use hyphae_query::Value as LegacyValue;
use hyphae_storage::{
    SnapshotContents, SnapshotReadLimits, load_snapshot, load_snapshot_for_migration,
};
use native_client::{
    EmbeddedClient, OfflineOwnerClient, ensure_key_output_outside_data_dir,
    read_legacy_bearer_file, reserve_restricted_api_key_file,
};
use serde::{Deserialize, Deserializer, de::Visitor};
use serde_json::{Value, json};
use uuid::Uuid;

use hyphae_native_types::{
    EngineKind, FieldId, IntegerWidth, LogicalType, VectorElement, VectorType,
};

#[derive(Debug, Parser)]
#[command(
    name = "hyphae",
    version,
    about = "Autonomous native SQL, structure, and search data engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print independently versioned release and native product surfaces.
    Version {
        /// Emit a machine-readable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Atomically store one format-2 structured JSON document.
    Put {
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long = "json")]
        value: String,
        #[arg(long)]
        transaction_id: Option<Uuid>,
    },
    /// Read one format-2 structured document.
    Get {
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        proof_out: Option<PathBuf>,
    },
    /// Atomically delete one format-2 structured document.
    Delete {
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        transaction_id: Option<Uuid>,
    },
    /// Execute one deterministic format-2 structured query.
    Query {
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        #[arg(long, requires = "equals")]
        field: Option<String>,
        #[arg(long, requires = "field")]
        equals: Option<String>,
        #[arg(long)]
        sort: Option<String>,
        #[arg(long)]
        descending: bool,
        #[arg(long)]
        nulls_first: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        proof_out: Option<PathBuf>,
    },
    /// Create or reuse a verified format-2 logical snapshot.
    Snapshot {
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Inspect a format-2 source without mutating it.
    Migrate {
        #[command(subcommand)]
        operation: MigrationCommand,
    },
    /// Initialize a new native data directory, failing if the path exists.
    Init(LocalDirectory),
    /// Explicitly upgrade durable native metadata under the directory lock.
    Upgrade {
        /// Native data directory to upgrade.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Discover native product capabilities and hard limits.
    Capabilities(LocalDirectory),
    /// Inspect the native logical catalog.
    Catalog {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: CatalogCommand,
    },
    /// Execute native SQL.
    Sql {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: SqlCommand,
    },
    /// Read or mutate native scalar structures.
    Structure {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: StructureCommand,
    },
    /// Execute bounded native lexical search.
    Search {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: SearchCommand,
    },
    /// Resolve native transaction outcome evidence.
    Transaction {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: TransactionCommand,
    },
    /// Explain an admitted native operation.
    Explain {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: ExplainCommand,
    },
    /// Discover read-only hardware capabilities for Native scheduling.
    Hardware {
        #[command(subcommand)]
        operation: HardwareCommand,
    },
    /// Report current all-engine native status.
    Status(LocalDirectory),
    /// Capture bounded process-local native telemetry.
    Telemetry(LocalDirectory),
    /// Open the interactive native operator console.
    Console(LocalDirectory),
    /// Bootstrap or inspect security status, principals, roles, assignments, keys, and audit.
    Security {
        #[command(flatten)]
        local: LocalDirectory,
        #[command(subcommand)]
        operation: SecurityCommand,
    },
    /// Open, recover, and validate a native directory.
    Doctor(LocalDirectory),
    /// Publish a synchronized all-engine checkpoint.
    Checkpoint(LocalDirectory),
    /// Compact one native root family.
    Compact {
        #[command(flatten)]
        local: LocalDirectory,
        /// Root family to compact.
        #[arg(long, value_enum, default_value_t = CompactTarget::Structures)]
        target: CompactTarget,
    },
    /// Rebuild live native roots into a smaller page generation.
    Vacuum(LocalDirectory),
    /// Create or verify a native backup.
    Backup {
        #[command(subcommand)]
        operation: Option<BackupCommand>,
        /// Format-2 compatibility source. Native backups use `backup create`.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: Option<PathBuf>,
        /// New format-2 compatibility backup directory.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify a shipped format-2 portable backup offline.
    BackupVerify {
        #[arg(long)]
        backup: PathBuf,
    },
    /// Restore a verified native backup to a new directory.
    Restore {
        /// Native backup directory.
        #[arg(long)]
        backup: PathBuf,
        /// New native data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Work with canonical native proof artifacts.
    Proof {
        #[command(subcommand)]
        operation: ProofCommand,
    },
    /// Verify a shipped format-2 result proof completely offline.
    Verify {
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        anchor: String,
    },
    /// Verify a shipped format-2 retrieval proof completely offline.
    VerifyRetrieval {
        #[arg(long, value_enum)]
        kind: compatibility::RetrievalKind,
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        anchor: String,
    },
    /// Start the native local daemon and optional HTTP v2 edge.
    Serve {
        /// Existing native data directory to serve.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// UDS path on Unix or named-pipe identity on Windows.
        #[arg(long)]
        endpoint: Option<String>,
        /// Optional loopback native HTTP v2 listener.
        #[arg(long)]
        http_bind: Option<SocketAddr>,
        /// Require durable Native API keys on local and HTTP v2 transports.
        #[arg(long)]
        native_api_key_auth: bool,
        /// Restricted 1.2-only legacy bearer file for Native HTTP only.
        #[arg(long, requires = "http_bind")]
        native_legacy_bearer_file: Option<PathBuf>,
        /// Format-2 `/v1` listener. Native HTTP uses `--http-bind`.
        #[arg(long)]
        bind: Option<SocketAddr>,
        /// Restricted bearer-token file for the format-2 `/v1` listener.
        #[arg(long, env = "HYPHAE_BEARER_TOKEN_FILE")]
        bearer_token_file: Option<PathBuf>,
    },
    /// Call a running shipped `/v1` server through its public HTTP contract.
    Remote {
        #[arg(long, env = "HYPHAE_BASE_URL")]
        base_url: String,
        #[arg(long, env = "HYPHAE_BEARER_TOKEN_FILE")]
        bearer_token_file: Option<PathBuf>,
        #[command(subcommand)]
        operation: compatibility::RemoteCommand,
    },
    /// Run the bounded read-only MCP adapter over managed Native HTTP v2.
    /// Agent Memory lifecycle: setup, status, doctor, backup, remove,
    /// and purge-data over the user's dedicated memory directory.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Mcp {
        /// Managed Native HTTP v2 root origin.
        #[arg(
            long,
            env = "HYPHAE_BASE_URL",
            conflicts_with = "endpoint",
            required_unless_present = "endpoint"
        )]
        base_url: Option<String>,
        /// Native local UDS path or Windows named-pipe identity.
        #[arg(
            long,
            env = "HYPHAE_NATIVE_ENDPOINT",
            conflicts_with = "base_url",
            required_unless_present = "base_url"
        )]
        endpoint: Option<String>,
        /// Restricted file containing one durable Native API key.
        #[arg(
            long,
            env = "HYPHAE_NATIVE_API_KEY_FILE",
            conflicts_with = "native_api_key_stdin"
        )]
        native_api_key_file: Option<PathBuf>,
        /// Read the Native API key from the first stdin line, then MCP messages.
        #[arg(long, conflicts_with = "native_api_key_file")]
        native_api_key_stdin: bool,
        /// Explicitly expose the bounded write-scoped ingest tool.
        #[arg(long)]
        allow_ingest: bool,
        /// Tool profile: the full native registry, or the Agent Memory
        /// five-tool surface.
        #[arg(long, value_enum, default_value_t = McpProfile::Full)]
        profile: McpProfile,
        /// Expose the write-scoped memory tools (store, journal, forget) on the
        /// memory profile.
        #[arg(long)]
        allow_write: bool,
        /// Personal-memory collection identity.
        #[arg(long, default_value_t = 21)]
        personal_memory_collection: u128,
        /// Shared work-memory collection identity.
        #[arg(long, default_value_t = 22)]
        work_memory_collection: u128,
        /// Model-journal collection identity.
        #[arg(long, default_value_t = 23)]
        journal_memory_collection: u128,
    },
}

#[derive(Debug, Subcommand)]
enum HardwareCommand {
    /// Emit a stable hardware fingerprint and current resource snapshot.
    Discover {
        /// Data path whose filesystem and block device should be resolved.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Measure bounded CPU, memory, engine, storage, and WAL primitives.
    Calibrate {
        /// Data path whose static hardware profile identifies the calibration.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: Option<PathBuf>,
        /// Calibration duration and sample policy.
        #[arg(long, value_enum, default_value_t = HardwareCalibrationMode::Quick)]
        mode: HardwareCalibrationMode,
        /// Override the per-user immutable calibration cache directory.
        #[arg(long, conflicts_with = "no_cache")]
        cache_dir: Option<PathBuf>,
        /// Run without reading or writing the calibration cache.
        #[arg(long)]
        no_cache: bool,
    },
    /// Derive an inspectable resource policy from one calibration receipt.
    GovernorPolicy {
        /// Data path whose current static profile must match the calibration.
        #[arg(long, env = "HYPHAE_DATA_DIR", conflicts_with = "profile")]
        data_dir: Option<PathBuf>,
        /// Exact discovery receipt used as immutable policy authority.
        #[arg(long, conflicts_with = "data_dir")]
        profile: Option<PathBuf>,
        /// Hardware calibration receipt used as the decision evidence.
        #[arg(long)]
        calibration: PathBuf,
        /// Scheduler objective used for class limits.
        #[arg(long, value_enum, default_value_t = HardwareGovernorMode::Mixed)]
        mode: HardwareGovernorMode,
    },
    /// Derive inspectable persistent worker and NUMA placement.
    ExecutionTopology {
        /// Data path whose current static profile must match the calibration.
        #[arg(long, env = "HYPHAE_DATA_DIR", conflicts_with = "profile")]
        data_dir: Option<PathBuf>,
        /// Exact discovery receipt used as immutable topology authority.
        #[arg(long, conflicts_with = "data_dir")]
        profile: Option<PathBuf>,
        /// Hardware calibration receipt used to derive the governor budget.
        #[arg(long)]
        calibration: PathBuf,
        /// Scheduler objective used for the worker budget.
        #[arg(long, value_enum, default_value_t = HardwareGovernorMode::Mixed)]
        mode: HardwareGovernorMode,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum HardwareCalibrationMode {
    Quick,
    Thorough,
}

impl From<HardwareCalibrationMode> for CalibrationMode {
    fn from(value: HardwareCalibrationMode) -> Self {
        match value {
            HardwareCalibrationMode::Quick => Self::Quick,
            HardwareCalibrationMode::Thorough => Self::Thorough,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum HardwareGovernorMode {
    Latency,
    Bulk,
    Mixed,
}

impl From<HardwareGovernorMode> for GovernorMode {
    fn from(value: HardwareGovernorMode) -> Self {
        match value {
            HardwareGovernorMode::Latency => Self::Latency,
            HardwareGovernorMode::Bulk => Self::Bulk,
            HardwareGovernorMode::Mixed => Self::Mixed,
        }
    }
}

/// Selectable migration source kind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
enum MigrationSourceKind {
    /// Existing format-2 Hyphae data directory.
    #[default]
    Format2,
    /// Offline Valkey/Redis RDB file.
    ValkeyRdb,
}

#[derive(Debug, Subcommand)]
enum MigrationCommand {
    /// Verify and report the source logical snapshot.
    Inspect {
        #[arg(long)]
        source: PathBuf,
        #[arg(long, value_enum, default_value_t)]
        source_kind: MigrationSourceKind,
        /// Explicitly waive one degraded or rejected source construct.
        #[arg(long = "waive")]
        waived: Vec<String>,
    },
    /// Import a verified source into a separate pending Native target.
    Run {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t)]
        source_kind: MigrationSourceKind,
        /// Explicitly waive one degraded or rejected source construct.
        #[arg(long = "waive")]
        waived: Vec<String>,
    },
    /// Verify a migration manifest and its pending or promoted target.
    Verify {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t)]
        source_kind: MigrationSourceKind,
    },
    /// Promote a validated pending Native target.
    Promote {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t)]
        source_kind: MigrationSourceKind,
    },
    /// Remove a pending migration target while retaining the source.
    Rollback {
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, clap::Args)]
struct LocalDirectory {
    /// Native data directory.
    #[arg(long, env = "HYPHAE_DATA_DIR")]
    data_dir: PathBuf,
    /// Restricted API-key file for a bootstrapped Native directory.
    #[arg(
        long,
        env = "HYPHAE_NATIVE_API_KEY_FILE",
        conflicts_with = "native_api_key_stdin"
    )]
    native_api_key_file: Option<PathBuf>,
    /// Read the Native API key from standard input without exposing it in argv.
    #[arg(long, conflicts_with = "native_api_key_file")]
    native_api_key_stdin: bool,
}

#[derive(Debug, Subcommand)]
enum SecurityCommand {
    /// Report redacted access-control catalog status.
    Status,
    /// Inspect redacted principal metadata.
    Principal {
        #[command(subcommand)]
        operation: SecurityPrincipalCommand,
    },
    /// Inspect immutable and custom role metadata.
    Role {
        #[command(subcommand)]
        operation: SecurityRoleCommand,
    },
    /// Inspect direct role assignments.
    Assignment {
        #[command(subcommand)]
        operation: SecurityAssignmentCommand,
    },
    /// Inspect redacted API-key metadata.
    Key {
        #[command(subcommand)]
        operation: SecurityKeyCommand,
    },
    /// Inspect retained durable security events.
    Audit {
        #[command(subcommand)]
        operation: SecurityListCommand,
    },
    /// Create the unique initial owner and a restricted API-key file.
    Bootstrap {
        /// Human-readable owner name; never used as authority.
        #[arg(long)]
        name: String,
        /// Non-secret credential label.
        #[arg(long, default_value = "bootstrap")]
        label: String,
        /// New owner-only API-key file. Existing paths are never overwritten.
        #[arg(long)]
        key_out: PathBuf,
    },
    /// Inspect or resolve pending owner recovery while the directory is offline.
    Owner {
        #[command(subcommand)]
        operation: SecurityOwnerCommand,
    },
    /// Migrate or terminally revoke Native HTTP legacy-bearer compatibility.
    LegacyBearer {
        #[command(subcommand)]
        operation: SecurityLegacyBearerCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityLegacyBearerCommand {
    /// Create canonical Owner/key state, durably write the key, then activate dual-window.
    Migrate {
        /// Human-readable canonical Owner name.
        #[arg(long)]
        name: String,
        /// Non-secret canonical Owner-key label.
        #[arg(long)]
        label: String,
        /// Existing restricted legacy bearer file.
        #[arg(long)]
        legacy_bearer_file: PathBuf,
        /// New restricted canonical Owner key file.
        #[arg(long)]
        key_out: PathBuf,
    },
    /// Permanently disable the legacy bearer using a canonical Owner key.
    Revoke {
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityOwnerCommand {
    /// Inspect redacted pending owner-recovery provenance.
    Inspect,
    /// Start recovery, write a new restricted key, and leave activation pending.
    Recover {
        /// Non-secret credential label.
        #[arg(long)]
        label: String,
        /// New restricted key file outside the data directory.
        #[arg(long)]
        key_out: PathBuf,
    },
    /// Validate the exact restricted key file and atomically activate it.
    Resume {
        /// Public pending key identity.
        #[arg(long)]
        pending_key_id: String,
        /// Existing restricted file containing the complete pending key.
        #[arg(long)]
        key_file: PathBuf,
        /// Exact authorization generation reported by recover or inspect.
        #[arg(long)]
        expected_authorization_epoch: u64,
    },
    /// Delete only the exact pending recovery record and inactive key.
    AbortPending {
        /// Public pending key identity.
        #[arg(long)]
        pending_key_id: String,
        /// Exact authorization generation reported by recover or inspect.
        #[arg(long)]
        expected_authorization_epoch: u64,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityPrincipalCommand {
    /// Return one bounded page in stable order.
    List {
        /// Opaque continuation emitted by the preceding page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum redacted rows to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Create one disabled principal.
    Create {
        /// Bounded human-readable display name.
        #[arg(long)]
        name: String,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Enable or disable one existing principal.
    SetEnabled {
        /// Canonical 32-hex principal identity.
        #[arg(long)]
        principal_id: String,
        /// New authentication state.
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityRoleCommand {
    /// Return one bounded page in stable order.
    List {
        /// Opaque continuation emitted by the preceding page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum redacted rows to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Create one immutable custom role.
    Create {
        /// Bounded human-readable display name.
        #[arg(long)]
        name: String,
        /// Canonical `PERMISSION@SCOPE`; repeat for each grant.
        #[arg(long, required = true)]
        grant: Vec<String>,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AssignableBuiltInRole {
    Admin,
    Operator,
    Developer,
    Writer,
    Reader,
    Auditor,
}

impl AssignableBuiltInRole {
    const fn product(self) -> BuiltInRole {
        match self {
            Self::Admin => BuiltInRole::Admin,
            Self::Operator => BuiltInRole::Operator,
            Self::Developer => BuiltInRole::Developer,
            Self::Writer => BuiltInRole::Writer,
            Self::Reader => BuiltInRole::Reader,
            Self::Auditor => BuiltInRole::Auditor,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SecurityAssignmentCommand {
    /// Return one bounded page in stable order.
    List {
        /// Opaque continuation emitted by the preceding page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum redacted rows to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Assign one non-owner built-in role.
    CreateBuiltIn {
        /// Canonical 32-hex principal identity.
        #[arg(long)]
        principal_id: String,
        /// Assignable built-in role; `owner` is never accepted.
        #[arg(long, value_enum)]
        role: AssignableBuiltInRole,
        /// `instance`, `catalog_subtree:ID`, or `catalog_object:ID`.
        #[arg(long)]
        scope: String,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Assign one immutable custom role at its exact grant scopes.
    CreateCustom {
        /// Canonical 32-hex principal identity.
        #[arg(long)]
        principal_id: String,
        /// Canonical 32-hex custom-role identity.
        #[arg(long)]
        role_id: String,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Revoke one non-owner assignment.
    Revoke {
        /// Canonical 32-hex assignment identity.
        #[arg(long)]
        assignment_id: String,
        /// Nonzero replay-or-conflict token.
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityListCommand {
    /// Return one bounded page in stable order.
    List {
        /// Opaque continuation emitted by the preceding page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum redacted rows to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum SecurityKeyCommand {
    /// Return one bounded page in stable order.
    List {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Issue one inactive key, write it locally, then activate it.
    Issue {
        #[arg(long)]
        principal_id: String,
        #[arg(long)]
        label: String,
        #[arg(long, value_enum, required = true)]
        role: Vec<AssignableBuiltInRole>,
        #[arg(long)]
        custom_role: Vec<String>,
        #[arg(long, required = true)]
        permission: Vec<String>,
        #[arg(long, required = true)]
        scope: Vec<String>,
        #[arg(long)]
        expires_at_micros: Option<i64>,
        #[arg(long)]
        self_manage: bool,
        #[arg(long)]
        key_out: PathBuf,
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Rotate one key, write the successor locally, then activate it.
    Rotate {
        #[arg(long)]
        predecessor_key_id: String,
        #[arg(long)]
        label: String,
        #[arg(long, default_value_t = 0)]
        overlap_seconds: u64,
        #[arg(long)]
        expires_at_micros: Option<i64>,
        #[arg(long)]
        self_manage: bool,
        #[arg(long)]
        key_out: PathBuf,
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Revoke one exact active key.
    Revoke {
        #[arg(long)]
        key_id: String,
        #[arg(long)]
        self_manage: bool,
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
    /// Abort one exact inactive issue or rotation successor.
    Abort {
        #[arg(long)]
        key_id: String,
        #[arg(long)]
        rotation: bool,
        #[arg(long)]
        self_manage: bool,
        #[arg(long, value_parser = parse_nonzero_idempotency_token)]
        idempotency_token: u128,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// List one bounded stable-ID ordered catalog page.
    List {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 1_000)]
        visit_limit: usize,
        #[arg(long, default_value_t = 1_048_576)]
        byte_limit: usize,
        #[arg(long, value_enum)]
        kind: Option<CatalogKind>,
        #[arg(long)]
        parent: Option<u128>,
        /// Opaque continuation token emitted by the preceding visible page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Describe one logical catalog object by stable ID.
    Describe {
        #[arg(long)]
        id: u128,
    },
    /// Resolve one `database.schema.object` name.
    Resolve {
        #[arg(long)]
        name: String,
    },
    /// List one bounded page of object dependencies or dependents.
    Dependencies {
        #[arg(long)]
        id: u128,
        #[arg(long, value_enum, default_value_t = DependencyDirectionArgument::Outgoing)]
        direction: DependencyDirectionArgument,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 1_000)]
        visit_limit: usize,
        #[arg(long, default_value_t = 1_048_576)]
        byte_limit: usize,
    },
    /// Create a catalogued native structure keyspace.
    CreateKeyspace {
        #[arg(long)]
        id: u128,
        #[arg(long)]
        parent: u128,
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        family: StructureFamily,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Create a catalogued integrated search collection and its analyzer.
    CreateSearchCollection {
        #[arg(long)]
        database: u128,
        #[arg(long)]
        schema: u128,
        #[arg(long)]
        collection: u128,
        #[arg(long)]
        analyzer: u128,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 2)]
        dimension: u16,
        /// Adds frozen Latin diacritic folding to the collection analyzer.
        #[arg(long)]
        analyzer_ascii_folding: bool,
        /// Adds frozen English stop-word removal to the collection analyzer.
        #[arg(long)]
        analyzer_english_stop: bool,
        /// Adds frozen English Porter stemming to the collection analyzer.
        #[arg(long)]
        analyzer_english_stem: bool,
        /// Tuned BM25 k1 in micros (defaults keep the canonical 1.2).
        /// Replace the sample doc-value fields with the Agent Memory
        /// schema: project and kind string doc-values.
        #[arg(long)]
        memory_schema: bool,
        /// Reuse an existing database, schema, and analyzer; create only the collection.
        #[arg(long)]
        reuse_schema: bool,
        #[arg(long, requires = "bm25_b_micros")]
        bm25_k1_micros: Option<u64>,
        /// Tuned BM25 b in micros (defaults keep the canonical 0.75).
        #[arg(long, requires = "bm25_k1_micros")]
        bm25_b_micros: Option<u64>,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
}

#[derive(Debug, Subcommand)]
enum SqlCommand {
    /// Execute one bounded native DDL, DML, or query statement.
    Execute {
        #[arg(long)]
        statement: String,
        /// JSON scalar parameter in statement order. May be repeated.
        #[arg(long = "parameter")]
        parameters: Vec<String>,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Prepare, execute, and deallocate one bounded SQL query in one session.
    Prepared {
        #[arg(long)]
        statement: String,
        /// JSON scalar parameter in statement order. May be repeated.
        #[arg(long = "parameter")]
        parameters: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum StructureCommand {
    /// Read one UTF-8 key.
    Get {
        #[arg(long)]
        key: String,
    },
    /// Set one UTF-8 key and value.
    Set {
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        /// Optional absolute Unix expiry in microseconds.
        #[arg(long)]
        expires_at_micros: Option<i64>,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Read one UTF-8 key's TTL state.
    Ttl {
        #[arg(long)]
        key: String,
    },
    /// Atomically apply one JSON array of catalogued structure mutations.
    Batch {
        /// JSON array of typed mutation objects.
        #[arg(long)]
        mutations_json: String,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Read one catalogued structure with a typed JSON request.
    Read {
        #[arg(long)]
        request_json: String,
    },
}

#[derive(Debug, Subcommand)]
enum SearchCommand {
    /// Provision catalog-owned physical search storage for one logical collection.
    Provision {
        #[arg(long)]
        collection: u128,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Execute a bounded exact-term, phrase, prefix, or fuzzy query.
    Query {
        #[arg(long)]
        index: u128,
        #[arg(long)]
        query: String,
        #[arg(long, value_enum, default_value_t = SearchQueryKind::Term)]
        kind: SearchQueryKind,
        #[arg(long, default_value_t = 1)]
        max_distance: u8,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Execute integrated lexical and named-vector search.
    Integrated {
        #[arg(long)]
        collection: u128,
        #[arg(long)]
        lexical: Option<String>,
        #[arg(long)]
        vector_target: Option<String>,
        /// Query vector component; may be repeated.
        #[arg(long = "vector")]
        vector: Vec<f32>,
        #[arg(long, value_enum, default_value_t = IntegratedVectorStrategy::Exact)]
        vector_strategy: IntegratedVectorStrategy,
        #[arg(long, default_value_t = 8)]
        ef_search: usize,
        #[arg(long, default_value_t = 8)]
        candidate_limit: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Typed doc-value filter JSON. Defaults to match-all.
        #[arg(long)]
        filter_json: Option<String>,
        /// JSON array of typed sort components.
        #[arg(long)]
        sort_json: Option<String>,
        /// JSON array of `{field,limit}` facet requests.
        #[arg(long)]
        facets_json: Option<String>,
        /// JSON array of named metric aggregation requests.
        #[arg(long)]
        metrics_json: Option<String>,
        /// Branch-combination method. Defaults to weighted reciprocal-rank.
        #[arg(long, value_enum)]
        fusion: Option<FusionMethodInput>,
        /// Doc-value field for first-k-per-parent deduplication.
        #[arg(long, requires = "dedupe_first_k")]
        dedupe_field: Option<String>,
        /// Hits retained per distinct parent value (1..=100).
        #[arg(long, requires = "dedupe_field")]
        dedupe_first_k: Option<usize>,
        /// Budgeted highlighted fragments per hit (1..=4).
        #[arg(long)]
        highlight_fragments: Option<usize>,
        /// Normalized-text byte budget per fragment (16..=512).
        #[arg(long, default_value_t = 128, requires = "highlight_fragments")]
        highlight_bytes: usize,
    },
    /// Consolidates every vector index of one collection into a fresh
    /// generation, draining accumulated deltas.
    Consolidate {
        #[arg(long)]
        collection: u128,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Deterministically chunk one document into ingest-ready JSON.
    Chunk {
        /// Parent document identity carried by every chunk.
        #[arg(long)]
        parent: u128,
        /// UTF-8 source text. Mutually exclusive with --file.
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// UTF-8 source file. Mutually exclusive with --text.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Fixed window size in bytes.
        #[arg(long, default_value_t = 1024)]
        size: usize,
        /// Fixed window overlap in bytes.
        #[arg(long, default_value_t = 0)]
        overlap: usize,
        /// Pack whole sentences up to --size, never beyond --sentence-max.
        #[arg(long)]
        sentence: bool,
        /// Hard sentence-mode window bound in bytes.
        #[arg(long, default_value_t = 2048)]
        sentence_max: usize,
    },
    /// Atomically ingest integrated documents from one JSON array.
    Ingest {
        #[arg(long)]
        collection: u128,
        #[arg(long)]
        idempotency_id: u128,
        /// JSON array of `{id,text,doc_values,vectors}` documents.
        #[arg(long)]
        documents_json: String,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Replace one integrated document from JSON.
    Update {
        #[arg(long)]
        collection: u128,
        #[arg(long)]
        idempotency_id: u128,
        #[arg(long)]
        document_json: String,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Delete one integrated document from every branch.
    Delete {
        #[arg(long)]
        collection: u128,
        #[arg(long)]
        idempotency_id: u128,
        #[arg(long)]
        document: u128,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
}

#[derive(Debug, Deserialize)]
struct IngestDocument {
    id: JsonU128,
    text: String,
    #[serde(default)]
    doc_values: BTreeMap<String, Value>,
    #[serde(default)]
    vectors: BTreeMap<String, Vec<f32>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum StructureMutationInput {
    StringSet {
        keyspace: JsonU128,
        key: String,
        value: String,
        expires_at_micros: Option<i64>,
    },
    StringDelete {
        keyspace: JsonU128,
        key: String,
    },
    CounterAdd {
        keyspace: JsonU128,
        key: String,
        delta: i64,
    },
    Create {
        keyspace: JsonU128,
        key: String,
        family: StructureFamily,
    },
    Delete {
        keyspace: JsonU128,
        key: String,
        family: StructureFamily,
    },
    Expire {
        keyspace: JsonU128,
        key: String,
        family: StructureFamily,
        expires_at_micros: i64,
    },
    HashSet {
        keyspace: JsonU128,
        key: String,
        field: String,
        value: String,
    },
    HashDelete {
        keyspace: JsonU128,
        key: String,
        field: String,
    },
    HashCounterAdd {
        keyspace: JsonU128,
        key: String,
        field: String,
        delta: i64,
    },
    HashExpireField {
        keyspace: JsonU128,
        key: String,
        field: String,
        expires_at_micros: i64,
    },
    ListPush {
        keyspace: JsonU128,
        key: String,
        side: ListSideInput,
        value: String,
    },
    ListPop {
        keyspace: JsonU128,
        key: String,
        side: ListSideInput,
    },
    SetAdd {
        keyspace: JsonU128,
        key: String,
        member: String,
    },
    SetRemove {
        keyspace: JsonU128,
        key: String,
        member: String,
    },
    SortedSetAdd {
        keyspace: JsonU128,
        key: String,
        member: String,
        score: f64,
    },
    SortedSetRemove {
        keyspace: JsonU128,
        key: String,
        member: String,
    },
    SortedSetIncrement {
        keyspace: JsonU128,
        key: String,
        member: String,
        delta: f64,
    },
    SortedSetPop {
        keyspace: JsonU128,
        key: String,
        #[serde(default)]
        end: SortedSetEndInput,
    },
    StringSetConditional {
        keyspace: JsonU128,
        key: String,
        value: String,
        expires_at_micros: Option<i64>,
        #[serde(default)]
        condition: SetConditionInput,
    },
    StringAppend {
        keyspace: JsonU128,
        key: String,
        suffix: String,
    },
    StringSetRange {
        keyspace: JsonU128,
        key: String,
        offset: u32,
        patch: String,
    },
    HashSetIfAbsent {
        keyspace: JsonU128,
        key: String,
        field: String,
        value: String,
    },
    SetPop {
        keyspace: JsonU128,
        key: String,
        seed: u64,
    },
    StreamAdd {
        keyspace: JsonU128,
        key: String,
        fields: BTreeMap<String, String>,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SetConditionInput {
    #[default]
    IfAbsent,
    IfPresent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortedSetEndInput {
    #[default]
    Lowest,
    Highest,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ListSideInput {
    Left,
    Right,
}

impl From<ListSideInput> for ProductListSide {
    fn from(value: ListSideInput) -> Self {
        match value {
            ListSideInput::Left => Self::Left,
            ListSideInput::Right => Self::Right,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum StructureReadInput {
    StringGet {
        keyspace: JsonU128,
        key: String,
    },
    CounterGet {
        keyspace: JsonU128,
        key: String,
    },
    Ttl {
        keyspace: JsonU128,
        key: String,
        family: StructureFamily,
    },
    HashGet {
        keyspace: JsonU128,
        key: String,
        field: String,
    },
    HashFieldTtl {
        keyspace: JsonU128,
        key: String,
        field: String,
    },
    HashScan {
        keyspace: JsonU128,
        key: String,
        start_after: Option<String>,
        limit: usize,
    },
    HashLength {
        keyspace: JsonU128,
        key: String,
    },
    ListRange {
        keyspace: JsonU128,
        key: String,
        start: i64,
        stop: i64,
    },
    ListLength {
        keyspace: JsonU128,
        key: String,
    },
    SetContains {
        keyspace: JsonU128,
        key: String,
        member: String,
    },
    SetMembers {
        keyspace: JsonU128,
        key: String,
        start_after: Option<String>,
        limit: usize,
    },
    SetCardinality {
        keyspace: JsonU128,
        key: String,
    },
    SetAlgebra {
        keyspace: JsonU128,
        operation_kind: SetAlgebraInput,
        keys: Vec<String>,
        output_member_limit: usize,
        visit_limit: usize,
    },
    SortedSetScore {
        keyspace: JsonU128,
        key: String,
        member: String,
    },
    SortedSetRank {
        keyspace: JsonU128,
        key: String,
        member: String,
        order: SortOrderInput,
    },
    SortedSetRange {
        keyspace: JsonU128,
        key: String,
        start: i64,
        stop: i64,
        order: SortOrderInput,
    },
    SortedSetCardinality {
        keyspace: JsonU128,
        key: String,
    },
    StreamRange {
        keyspace: JsonU128,
        key: String,
        start: u64,
        end: u64,
        limit: usize,
    },
    SortedSetScoreRange {
        keyspace: JsonU128,
        key: String,
        #[serde(default)]
        lower: ScoreBoundInput,
        #[serde(default)]
        upper: ScoreBoundInput,
        #[serde(default)]
        offset: usize,
        limit: usize,
        order: SortOrderInput,
    },
    HashScanReverse {
        keyspace: JsonU128,
        key: String,
        start_before: Option<String>,
        limit: usize,
    },
    HashScanMatch {
        keyspace: JsonU128,
        key: String,
        pattern: String,
        start_after: Option<String>,
        output_limit: usize,
        visit_limit: usize,
        match_step_limit: usize,
    },
    KeyScanMatch {
        keyspace: JsonU128,
        pattern: String,
        start_after: Option<String>,
        output_limit: usize,
        visit_limit: usize,
        match_step_limit: usize,
    },
    StringRange {
        keyspace: JsonU128,
        key: String,
        start: i64,
        end: i64,
    },
    SetRandomMembers {
        keyspace: JsonU128,
        key: String,
        seed: u64,
        count: usize,
    },
}

/// One canonical score endpoint: unbounded by default.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScoreBoundInput {
    #[default]
    Unbounded,
    #[serde(untagged)]
    Bounded {
        #[serde(default)]
        exclusive: bool,
        score: f64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SetAlgebraInput {
    Union,
    Intersection,
    Difference,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortOrderInput {
    Ascending,
    Descending,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum TransactionStepInput {
    Status,
    StageSql {
        statement: String,
        #[serde(default)]
        parameters: Vec<Value>,
    },
    StageStructure {
        mutation: StructureMutationInput,
    },
    StageSearch {
        action: SearchMutationAction,
        index: JsonU128,
        document_id: String,
        text: Option<String>,
    },
    StageVector {
        action: VectorMutationAction,
        index: JsonU128,
        object_id: JsonU128,
        #[serde(default)]
        vector: Vec<f32>,
    },
    Commit,
    Rollback,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchMutationAction {
    Index,
    Replace,
    Delete,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VectorMutationAction {
    Upsert,
    Delete,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ProofOperationInput {
    CatalogList {
        #[serde(default = "default_catalog_limit")]
        limit: usize,
    },
    CatalogDescribe {
        id: JsonU128,
    },
    Sql {
        statement: String,
        #[serde(default)]
        parameters: Vec<Value>,
    },
}

const fn default_catalog_limit() -> usize {
    100
}

#[derive(Clone, Copy, Debug)]
struct JsonU128(u128);

impl<'de> Deserialize<'de> for JsonU128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct JsonU128Visitor;

        impl Visitor<'_> for JsonU128Visitor {
            type Value = JsonU128;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an unsigned integer or canonical decimal string")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(JsonU128(u128::from(value)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value.parse::<u128>().map(JsonU128).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(JsonU128Visitor)
    }
}

#[derive(Debug, Subcommand)]
enum TransactionCommand {
    /// Execute one explicit-transaction script in a single retained session.
    Execute {
        /// JSON array containing `status`, `stage_sql`, `stage_structure`,
        /// `stage_search`, `stage_vector`, and final `commit` or `rollback` steps.
        #[arg(long)]
        steps_json: String,
        #[arg(long, value_enum, default_value_t = Durability::Strict)]
        durability: Durability,
    },
    /// Resolve retained commit evidence by request or transaction identity.
    Status {
        #[arg(long)]
        id: u128,
    },
}

#[derive(Debug, Subcommand)]
enum ExplainCommand {
    /// Return bounded native SQL plan text.
    Sql {
        #[arg(long)]
        statement: String,
    },
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    /// Create and independently verify a native backup.
    Create {
        #[command(flatten)]
        local: LocalDirectory,
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify a native backup without opening it as live state.
    Verify {
        #[arg(long)]
        backup: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ProofCommand {
    /// Execute an eligible read and write canonical proof and witness artifacts.
    Generate {
        #[command(flatten)]
        local: LocalDirectory,
        /// JSON proof operation (`catalog_list`, `catalog_describe`, or `sql`).
        #[arg(long)]
        operation_json: String,
        #[arg(long)]
        proof_out: PathBuf,
        #[arg(long)]
        witness_out: PathBuf,
    },
    /// Verify a canonical native proof and complete witness offline.
    Verify {
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        witness: PathBuf,
        /// Independently trusted 32-byte native anchor digest.
        #[arg(long)]
        anchor: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Durability {
    Strict,
    Group,
    Memory,
}

impl From<Durability> for ProductDurability {
    fn from(value: Durability) -> Self {
        match value {
            Durability::Strict => Self::Strict,
            Durability::Group => Self::Group,
            Durability::Memory => Self::Memory,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CatalogKind {
    Database,
    Schema,
    Relation,
    SecondaryIndex,
    Keyspace,
    Structure,
    SearchCollection,
    Analyzer,
    CrossEngineLink,
}

impl From<CatalogKind> for CatalogObjectKind {
    fn from(value: CatalogKind) -> Self {
        match value {
            CatalogKind::Database => Self::Database,
            CatalogKind::Schema => Self::Schema,
            CatalogKind::Relation => Self::Relation,
            CatalogKind::SecondaryIndex => Self::SecondaryIndex,
            CatalogKind::Keyspace => Self::Keyspace,
            CatalogKind::Structure => Self::Structure,
            CatalogKind::SearchCollection => Self::SearchCollection,
            CatalogKind::Analyzer => Self::Analyzer,
            CatalogKind::CrossEngineLink => Self::CrossEngineLink,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum CompactTarget {
    #[default]
    Structures,
    Search,
}

impl From<CompactTarget> for CompactionTarget {
    fn from(value: CompactTarget) -> Self {
        match value {
            CompactTarget::Structures => Self::Structures,
            CompactTarget::Search => Self::Search,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum SearchQueryKind {
    #[default]
    Term,
    Phrase,
    Prefix,
    Fuzzy,
}

#[derive(Debug, clap::Subcommand)]
enum AgentCommand {
    /// Create every Agent Memory resource and smoke test the surface.
    Setup {
        /// Enable and start the user service without asking.
        #[arg(long, conflicts_with = "no_service")]
        enable_service: bool,
        /// Skip service installation entirely.
        #[arg(long)]
        no_service: bool,
    },
    /// Redacted local status: paths, initialization, credentials.
    Status,
    /// Engine doctor over the Agent Memory directory.
    Doctor,
    /// Write one verified backup archive under the backups directory.
    Backup,
    /// Remove generated credentials while preserving data and backups.
    Remove,
    /// Restore one verified backup into the data directory.
    Restore {
        /// Backup directory produced by `hyphae agent backup`.
        #[arg(long)]
        backup: PathBuf,
    },
    /// Stop, back up, doctor, and restart the service around an upgrade.
    Upgrade,
    /// Offline copy legacy mixed memories into physical domain collections.
    MigrateDomains,
    /// Generate one agent host's MCP configuration.
    Configure {
        #[arg(value_enum)]
        host: AgentHost,
        /// Agent Memory authority granted to this host.
        #[arg(long, value_enum, default_value_t = AgentAccess::Read)]
        access: AgentAccess,
        /// Apply the configuration through the host's supported interface.
        #[arg(long)]
        apply: bool,
    },
    /// Process one proactive host lifecycle event from standard input.
    Hook {
        #[arg(long, value_enum)]
        host: AgentHost,
    },
    /// Permanently delete the data directory after confirmation.
    PurgeData {
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AgentHost {
    Claude,
    Codex,
    Opencode,
    Pi,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum AgentAccess {
    #[default]
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum McpProfile {
    /// Full native tool registry.
    Full,
    /// Agent Memory five-tool surface.
    Memory,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FusionMethodInput {
    /// Normalized weighted score blend across branches.
    WeightedScore,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum IntegratedVectorStrategy {
    #[default]
    Exact,
    Ann,
    Adaptive,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DependencyDirectionArgument {
    Outgoing,
    Incoming,
}

impl From<DependencyDirectionArgument> for DependencyDirection {
    fn from(value: DependencyDirectionArgument) -> Self {
        match value {
            DependencyDirectionArgument::Outgoing => Self::Outgoing,
            DependencyDirectionArgument::Incoming => Self::Incoming,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum StructureFamily {
    String,
    Counter,
    Hash,
    List,
    Set,
    SortedSet,
    Stream,
}

impl From<StructureFamily> for StructureKind {
    fn from(value: StructureFamily) -> Self {
        match value {
            StructureFamily::String => Self::String,
            StructureFamily::Counter => Self::Counter,
            StructureFamily::Hash => Self::Hash,
            StructureFamily::List => Self::List,
            StructureFamily::Set => Self::Set,
            StructureFamily::SortedSet => Self::SortedSet,
            StructureFamily::Stream => Self::Stream,
        }
    }
}

fn main() {
    // The dispatch future and the engine's open path outgrow the 1 MiB
    // Windows main-thread stack. A dedicated worker with an explicit stack
    // owns the runtime, and the dispatch state machine lives on the heap.
    let Ok(worker) = std::thread::Builder::new()
        .name("hyphae-cli".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(cli_main)
    else {
        let failure = CliFailure::internal();
        let _ignored = print_error(&failure);
        std::process::exit(i32::from(failure.exit_class()));
    };
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

fn cli_main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.use_stderr() => {
            let failure = CliFailure::invalid();
            let _ignored = print_error(&failure);
            std::process::exit(i32::from(failure.exit_class()));
        }
        Err(error) => error.exit(),
    };
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        let failure = CliFailure::internal();
        let _ignored = print_error(&failure);
        std::process::exit(i32::from(failure.exit_class()));
    };
    if let Err(failure) = runtime.block_on(Box::pin(run(cli))) {
        match failure {
            RunFailure::Native(failure) => {
                let _ignored = print_error(&failure);
                std::process::exit(i32::from(failure.exit_class()));
            }
            RunFailure::Compatibility(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

enum RunFailure {
    Native(CliFailure),
    Compatibility(Box<dyn std::error::Error>),
}

impl From<CliFailure> for RunFailure {
    fn from(failure: CliFailure) -> Self {
        Self::Native(failure)
    }
}

fn compatibility<T>(result: Result<T, Box<dyn std::error::Error>>) -> Result<T, RunFailure> {
    result.map_err(RunFailure::Compatibility)
}

#[allow(clippy::too_many_lines)]
async fn run(cli: Cli) -> Result<(), RunFailure> {
    match cli.command {
        Command::Version { json } => print_version(json).map_err(Into::into),
        Command::Put {
            data_dir,
            key,
            value,
            transaction_id,
        } => compatibility(compatibility::put(&data_dir, key, &value, transaction_id)),
        Command::Get {
            data_dir,
            key,
            proof_out,
        } => compatibility(compatibility::get(
            &data_dir,
            key.as_bytes(),
            proof_out.as_deref(),
        )),
        Command::Delete {
            data_dir,
            key,
            transaction_id,
        } => compatibility(compatibility::delete(
            &data_dir,
            key.as_bytes(),
            transaction_id,
        )),
        Command::Query {
            data_dir,
            field,
            equals,
            sort,
            descending,
            nulls_first,
            limit,
            proof_out,
        } => compatibility(compatibility::query(
            &data_dir,
            compatibility::QueryArguments {
                field,
                equals,
                sort,
                descending,
                nulls_first,
                limit,
                proof_out,
            },
        )),
        Command::Snapshot { data_dir } => compatibility(compatibility::snapshot(&data_dir)),
        Command::Migrate { operation } => migration(operation).map_err(Into::into),
        Command::Init(local) => init(&local).map_err(Into::into),
        Command::Upgrade { data_dir } => upgrade(&data_dir).map_err(Into::into),
        Command::Capabilities(local) => {
            dispatch(&local, ProductOperation::Capabilities).map_err(Into::into)
        }
        Command::Catalog { local, operation } => catalog(&local, operation).map_err(Into::into),
        Command::Sql { local, operation } => sql(&local, operation).map_err(Into::into),
        Command::Structure { local, operation } => structure(&local, operation).map_err(Into::into),
        Command::Search { local, operation } => search(&local, operation).map_err(Into::into),
        Command::Transaction { local, operation } => {
            transaction(&local, operation).map_err(Into::into)
        }
        Command::Explain { local, operation } => explain(&local, operation).map_err(Into::into),
        Command::Hardware { operation } => hardware(operation).map_err(Into::into),
        Command::Status(local) => {
            dispatch(&local, ProductOperation::AdminStatus).map_err(Into::into)
        }
        Command::Telemetry(local) => telemetry(&local).map_err(Into::into),
        Command::Console(local) => {
            let data_dir = local.data_dir.clone();
            let client = open_client(&local)?;
            tui::run(data_dir, client).map_err(Into::into)
        }
        Command::Security { local, operation } => security(&local, operation).map_err(Into::into),
        Command::Doctor(local) => {
            if compatibility(compatibility::directory_family(&local.data_dir))?
                == DirectoryFamily::Format2
            {
                compatibility(compatibility::doctor(&local.data_dir))
            } else {
                doctor(&local).map_err(Into::into)
            }
        }
        Command::Checkpoint(local) => {
            dispatch(&local, ProductOperation::AdminCheckpoint).map_err(Into::into)
        }
        Command::Compact { local, target } => {
            if compatibility(compatibility::directory_family(&local.data_dir))?
                == DirectoryFamily::Format2
            {
                compatibility(compatibility::compact(&local.data_dir))
            } else {
                compact(&local, target).map_err(Into::into)
            }
        }
        Command::Vacuum(local) => vacuum(&local).map_err(Into::into),
        Command::Backup {
            operation,
            data_dir,
            out,
        } => match (operation, data_dir, out) {
            (Some(operation), None, None) => backup(operation).map_err(Into::into),
            (None, Some(data_dir), Some(out)) => {
                compatibility(compatibility::backup(&data_dir, &out))
            }
            _ => Err(RunFailure::Native(CliFailure::invalid())),
        },
        Command::BackupVerify { backup } => compatibility(compatibility::backup_verify(&backup)),
        Command::Restore { backup, data_dir } => {
            if compatibility(compatibility::backup_family(&backup))? == BackupFamily::Format2 {
                compatibility(compatibility::restore(&backup, &data_dir))
            } else {
                restore(&backup, &data_dir).map_err(Into::into)
            }
        }
        Command::Proof { operation } => proof(operation).map_err(Into::into),
        Command::Verify {
            proof,
            snapshot,
            anchor,
        } => compatibility(compatibility::verify(&proof, &snapshot, &anchor)),
        Command::VerifyRetrieval {
            kind,
            proof,
            snapshot,
            anchor,
        } => compatibility(compatibility::verify_retrieval(
            kind, &proof, &snapshot, &anchor,
        )),
        Command::Serve {
            data_dir,
            endpoint,
            http_bind,
            native_api_key_auth,
            native_legacy_bearer_file,
            bind,
            bearer_token_file,
        } => {
            let family = compatibility(compatibility::directory_family(&data_dir))?;
            if family == DirectoryFamily::Native
                || (family == DirectoryFamily::Other
                    && bind.is_none()
                    && bearer_token_file.is_none())
            {
                if bind.is_some() || bearer_token_file.is_some() {
                    return Err(RunFailure::Native(CliFailure::invalid()));
                }
                native_service::serve(
                    data_dir,
                    endpoint,
                    http_bind,
                    native_api_key_auth,
                    native_legacy_bearer_file,
                )
                .await
                .map_err(Into::into)
            } else {
                if endpoint.is_some()
                    || http_bind.is_some()
                    || native_api_key_auth
                    || native_legacy_bearer_file.is_some()
                {
                    return Err(RunFailure::Compatibility(
                        "native serve options cannot be used with a format-2 directory".into(),
                    ));
                }
                compatibility(
                    compatibility::serve(data_dir, bind, bearer_token_file.as_deref()).await,
                )
            }
        }
        Command::Remote {
            base_url,
            bearer_token_file,
            operation,
        } => compatibility(
            compatibility::remote(&base_url, bearer_token_file.as_deref(), operation).await,
        ),
        Command::Agent { command } => match command {
            AgentCommand::Setup {
                enable_service,
                no_service,
            } => agent::setup(enable_service, no_service).map_err(Into::into),
            AgentCommand::Status => agent::status().map_err(Into::into),
            AgentCommand::Doctor => agent::doctor().await.map_err(Into::into),
            AgentCommand::Backup => agent::backup().await.map_err(Into::into),
            AgentCommand::Remove => agent::remove().map_err(Into::into),
            AgentCommand::Restore { backup } => agent::restore(&backup).map_err(Into::into),
            AgentCommand::Upgrade => agent::upgrade().await.map_err(Into::into),
            AgentCommand::MigrateDomains => agent::migrate_domains().map_err(Into::into),
            AgentCommand::Configure {
                host,
                access,
                apply,
            } => agent::configure(
                match host {
                    AgentHost::Claude => agent::Host::Claude,
                    AgentHost::Codex => agent::Host::Codex,
                    AgentHost::Opencode => agent::Host::Opencode,
                    AgentHost::Pi => agent::Host::Pi,
                },
                match access {
                    AgentAccess::Read => agent::Access::Read,
                    AgentAccess::Write => agent::Access::Write,
                },
                apply,
            )
            .map_err(Into::into),
            AgentCommand::Hook { host } => agent_hooks::handle(match host {
                AgentHost::Claude => agent_hooks::Host::Claude,
                AgentHost::Codex => agent_hooks::Host::Codex,
                AgentHost::Opencode => agent_hooks::Host::Opencode,
                AgentHost::Pi => agent_hooks::Host::Pi,
            })
            .await
            .map_err(Into::into),
            AgentCommand::PurgeData { yes } => agent::purge_data(yes).map_err(Into::into),
        },
        Command::Mcp {
            base_url,
            endpoint,
            native_api_key_file,
            native_api_key_stdin,
            allow_ingest,
            profile,
            allow_write,
            personal_memory_collection,
            work_memory_collection,
            journal_memory_collection,
        } => mcp::run(
            base_url.as_deref(),
            endpoint.as_deref(),
            native_api_key_file.as_deref(),
            native_api_key_stdin,
            allow_ingest,
            match profile {
                McpProfile::Full => mcp::Profile::Full,
                McpProfile::Memory => mcp::Profile::Memory {
                    allow_write,
                    collections: mcp::MemoryCollections {
                        personal: personal_memory_collection,
                        work: work_memory_collection,
                        journal: journal_memory_collection,
                    },
                },
            },
        )
        .await
        .map_err(Into::into),
    }
}

fn upgrade(data_dir: &Path) -> Result<(), CliFailure> {
    let mut product = NativeProduct::open_for_upgrade(data_dir)?;
    let default_scalar_keyspace = product.upgrade_default_scalar_keyspace_binding()?;
    let catalog_scope_index = product.upgrade_catalog_scope_index()?.is_some();
    print_json(&json!({
        "schema": "hyphae-native-upgrade-v1",
        "status": "upgraded",
        "data_dir": data_dir,
        "default_scalar_keyspace_created": default_scalar_keyspace,
        "catalog_scope_index_created": catalog_scope_index,
    }))
}

fn hardware(command: HardwareCommand) -> Result<(), CliFailure> {
    let mut output = BufWriter::new(stdout().lock());
    let mut diagnostic = BufWriter::new(stderr().lock());
    hardware_with_writers(command, &mut output, &mut diagnostic)
}

fn hardware_with_writers(
    command: HardwareCommand,
    output: &mut impl Write,
    diagnostic: &mut impl Write,
) -> Result<(), CliFailure> {
    match command {
        HardwareCommand::Discover { data_dir } => {
            let data_path = data_dir.map_or_else(std::env::current_dir, Ok)?;
            let profile = HardwareProfile::discover(data_path).map_err(|_| CliFailure::io())?;
            write_json(output, &serde_json::to_value(profile)?)
        }
        HardwareCommand::Calibrate {
            data_dir,
            mode,
            cache_dir,
            no_cache,
        } => {
            let data_path = data_dir.map_or_else(std::env::current_dir, Ok)?;
            let profile = HardwareProfile::discover(data_path).map_err(|_| CliFailure::io())?;
            let request = CalibrationRequest::for_current_executable(
                mode.into(),
                env!("HYPHAE_RUSTC_IDENTITY"),
                concat!("hyphae-cli/", env!("CARGO_PKG_VERSION")),
            )
            .map_err(|_| CliFailure::io())?;
            let calibration = if no_cache {
                HardwareCalibration::run(&profile, &request)
            } else {
                let cache_directory =
                    cache_dir.map_or_else(default_hardware_cache_directory, Ok)?;
                HardwareCalibration::run_cached(&profile, &request, cache_directory)
            }
            .map_err(|_| CliFailure::io())?;
            write_json(output, &serde_json::to_value(calibration)?)
        }
        HardwareCommand::GovernorPolicy {
            data_dir,
            profile,
            calibration,
            mode,
        } => hardware_governor_policy(data_dir, profile, &calibration, mode, output, diagnostic),
        HardwareCommand::ExecutionTopology {
            data_dir,
            profile,
            calibration,
            mode,
        } => hardware_execution_topology(data_dir, profile, &calibration, mode, output, diagnostic),
    }
}

fn hardware_governor_policy(
    data_dir: Option<PathBuf>,
    profile_path: Option<PathBuf>,
    calibration_path: &Path,
    mode: HardwareGovernorMode,
    output: &mut impl Write,
    diagnostic: &mut impl Write,
) -> Result<(), CliFailure> {
    let profile = load_or_discover_hardware_profile(data_dir, profile_path)?;
    let calibration = read_hardware_calibration(calibration_path, diagnostic)?;
    let policy = NativeGovernorPolicy::derive(&profile, &calibration, mode.into())
        .map_err(|error| governor_policy_failure(&calibration, error, diagnostic))?;
    write_json(output, &serde_json::to_value(policy)?)
}

fn hardware_execution_topology(
    data_dir: Option<PathBuf>,
    profile_path: Option<PathBuf>,
    calibration_path: &Path,
    mode: HardwareGovernorMode,
    output: &mut impl Write,
    diagnostic: &mut impl Write,
) -> Result<(), CliFailure> {
    let profile = load_or_discover_hardware_profile(data_dir, profile_path)?;
    let calibration = read_hardware_calibration(calibration_path, diagnostic)?;
    let policy = NativeGovernorPolicy::derive(&profile, &calibration, mode.into())
        .map_err(|error| governor_policy_failure(&calibration, error, diagnostic))?;
    let topology =
        NativeExecutionTopology::derive_with_calibration(&profile, &policy, &calibration).map_err(
            |error| {
                let _ignored = writeln!(
                    diagnostic,
                    "execution topology derivation rejected: {error}"
                );
                CliFailure::invalid()
            },
        )?;
    write_json(output, &serde_json::to_value(topology)?)
}

fn read_hardware_calibration(
    path: &Path,
    diagnostic: &mut impl Write,
) -> Result<HardwareCalibration, CliFailure> {
    let encoded = fs::read(path).map_err(|error| {
        let _ignored = writeln!(
            diagnostic,
            "cannot read hardware calibration receipt {}: {error}",
            path.display()
        );
        CliFailure::io()
    })?;
    serde_json::from_slice(&encoded).map_err(|error| {
        let _ignored = writeln!(
            diagnostic,
            "hardware calibration receipt {} is malformed: {error}",
            path.display()
        );
        CliFailure::invalid()
    })
}

fn governor_policy_failure(
    calibration: &HardwareCalibration,
    error: GovernorPolicyError,
    diagnostic: &mut impl Write,
) -> CliFailure {
    let _ignored = writeln!(
        diagnostic,
        "{}",
        governor_policy_diagnostic(calibration, error)
    );
    CliFailure::invalid()
}

fn governor_policy_diagnostic(
    calibration: &HardwareCalibration,
    error: GovernorPolicyError,
) -> String {
    let scaling = &calibration.thread_scaling;
    format!(
        "governor policy derivation rejected: {error}; thread_scaling.status={} \
         recommended_worker_count={:?}; unstable measurements: {}",
        scaling.status,
        scaling.recommended_worker_count,
        unstable_measurement_summaries(calibration).join(", ")
    )
}

fn unstable_measurement_summaries(calibration: &HardwareCalibration) -> Vec<String> {
    let unstable = calibration
        .measurements
        .iter()
        .filter(|measurement| measurement.status != "stable")
        .map(|measurement| {
            format!(
                "{}@{} ({}, mad={}ppm range={}ppm)",
                measurement.primitive,
                measurement.input_size,
                measurement.status,
                measurement.statistics.relative_mad_ppm,
                measurement.statistics.relative_range_ppm
            )
        })
        .collect::<Vec<_>>();
    if unstable.is_empty() {
        vec!["none".to_owned()]
    } else {
        unstable
    }
}

fn load_or_discover_hardware_profile(
    data_dir: Option<PathBuf>,
    profile: Option<PathBuf>,
) -> Result<HardwareProfile, CliFailure> {
    if let Some(profile) = profile {
        let encoded = fs::read(profile)?;
        return HardwareProfile::from_json_slice(&encoded).map_err(|_| CliFailure::invalid());
    }
    let data_path = data_dir.map_or_else(std::env::current_dir, Ok)?;
    HardwareProfile::discover(data_path).map_err(|_| CliFailure::io())
}

fn default_hardware_cache_directory() -> Result<PathBuf, CliFailure> {
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Caches"));

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        });

    root.map(|root| root.join("hyphae").join("calibration"))
        .ok_or_else(CliFailure::io)
}

#[allow(clippy::too_many_lines)]
fn migration(command: MigrationCommand) -> Result<(), CliFailure> {
    match command {
        MigrationCommand::Inspect {
            source,
            source_kind,
            waived,
        } => match source_kind {
            MigrationSourceKind::Format2 => {
                if !waived.is_empty() {
                    return Err(CliFailure::invalid());
                }
                let snapshot = migration_snapshot(&source)?;
                print_json(&migration_snapshot_json(&snapshot))
            }
            MigrationSourceKind::ValkeyRdb => {
                let inventory = migrate_valkey::inspect_valkey_rdb(
                    &source,
                    &migrate_valkey::rdb::RdbReadLimits::default(),
                )
                .map_err(|error| {
                    eprintln!("valkey-rdb inspection failed: {error}");
                    CliFailure::from(error)
                })?;
                let unwaived: Vec<&String> = inventory
                    .required_waivers
                    .iter()
                    .filter(|construct| !waived.contains(construct))
                    .collect();
                let mut report = migrate_valkey::inventory_json(&inventory);
                if let Some(object) = report.as_object_mut() {
                    object.insert(
                        "unwaived_constructs".to_owned(),
                        serde_json::json!(unwaived),
                    );
                }
                print_json(&report)
            }
        },
        MigrationCommand::Run {
            source,
            target,
            manifest,
            source_kind,
            waived,
        } => {
            reject_migration_path_overlap(&source, &target, &manifest)?;
            if source_kind == MigrationSourceKind::ValkeyRdb {
                let outcome =
                    migrate_valkey::import::run_valkey_rdb(&source, &target, &manifest, &waived)?;
                return print_json(&json!({
                    "status": "imported",
                    "source": source,
                    "source_kind": "valkey-rdb",
                    "target": target,
                    "manifest": manifest,
                    "pending": true,
                    "directory_id": outcome.receipt.target.directory_id,
                    "history_epoch": outcome.receipt.target.history_epoch,
                    "imported_keys": outcome.imported_keys,
                    "skipped_expired": outcome.skipped_expired,
                    "logical_digest": outcome.receipt.target.logical_digest,
                    "content_digest": outcome.receipt.content_digest,
                }));
            }
            if !waived.is_empty() {
                return Err(CliFailure::invalid());
            }
            let snapshot = migration_snapshot(&source)?;
            let mut product = NativeProduct::create_pending(&target)?;
            let result = match import_migration_snapshot(&mut product, &snapshot) {
                Ok(result) => result,
                Err(error) => {
                    drop(product);
                    let _ignored = fs::remove_dir_all(&target);
                    return Err(error);
                }
            };
            let target_identity = (
                product.directory_identity().directory_id().to_owned(),
                product.directory_identity().history_epoch(),
            );
            let migration_entries = snapshot
                .entries
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect::<Vec<_>>();
            if !product.migration_verify_public_entries(&migration_entries)? {
                drop(product);
                let _ignored = fs::remove_dir_all(&target);
                return Err(CliFailure::invalid());
            }
            let encoded = result.encode().map_err(|_| CliFailure::internal())?;
            if let Err(error) = write_new_file(&manifest, &encoded) {
                drop(product);
                let _ignored = fs::remove_dir_all(&target);
                return Err(error);
            }
            print_json(&json!({
                "status": "imported",
                "source": source,
                "target": target,
                "manifest": manifest,
                "pending": true,
                "directory_id": target_identity.0,
                "history_epoch": target_identity.1,
                "snapshot": migration_snapshot_json(&snapshot),
                "documents": result.documents.len(),
                "receipts": result.receipts.len(),
            }))
        }
        MigrationCommand::Verify {
            source,
            target,
            manifest,
            source_kind,
        } => {
            reject_migration_path_overlap(&source, &target, &manifest)?;
            if source_kind == MigrationSourceKind::ValkeyRdb {
                let outcome =
                    migrate_valkey::import::verify_valkey_rdb(&source, &target, &manifest)?;
                return print_json(&json!({
                    "status": "verified",
                    "source": source,
                    "source_kind": "valkey-rdb",
                    "target": target,
                    "manifest": manifest,
                    "pending": outcome.pending,
                    "logical_digest": outcome.receipt.target.logical_digest,
                    "content_digest": outcome.receipt.content_digest,
                }));
            }
            let snapshot = migration_snapshot(&source)?;
            let manifest_bytes = fs::read(&manifest)?;
            let migration = hyphae_native_runtime::MigrationManifest::decode(
                &manifest_bytes,
                &hyphae_native_runtime::MigrationManifestLimits::default(),
            )
            .map_err(|_| CliFailure::invalid())?;
            verify_migration_target(&target, &snapshot, &migration)?;
            print_json(&json!({
                "status": "verified",
                "source": source,
                "target": target,
                "manifest": manifest,
                "pending": !target.join("FORMAT").try_exists().map_err(|_| CliFailure::io())?,
            }))
        }
        MigrationCommand::Promote {
            source,
            target,
            manifest,
            source_kind,
        } => {
            reject_migration_path_overlap(&source, &target, &manifest)?;
            if source_kind == MigrationSourceKind::ValkeyRdb {
                let receipt =
                    migrate_valkey::import::promote_valkey_rdb(&source, &target, &manifest)?;
                return print_json(&json!({
                    "status": "promoted",
                    "source_kind": "valkey-rdb",
                    "target": target,
                    "manifest": manifest,
                    "content_digest": receipt.content_digest,
                }));
            }
            let snapshot = migration_snapshot(&source)?;
            let bytes = fs::read(&manifest)?;
            let migration = hyphae_native_runtime::MigrationManifest::decode(
                &bytes,
                &hyphae_native_runtime::MigrationManifestLimits::default(),
            )
            .map_err(|_| CliFailure::invalid())?;
            if migration.source.snapshot_digest != encode_hex(&snapshot.info.snapshot_digest) {
                return Err(CliFailure::invalid());
            }
            let pending = target.join("FORMAT.pending").try_exists()?;
            if !pending {
                return Err(CliFailure::invalid());
            }
            let mut product = NativeProduct::open_pending(&target)?;
            let lineage = product.directory_identity();
            if migration.target.directory_id != lineage.directory_id()
                || migration.target.history_epoch != lineage.history_epoch()
            {
                return Err(CliFailure::invalid());
            }
            verify_migration_product(&product, &snapshot, &migration)?;
            product.promote_pending()?;
            print_json(&json!({ "status": "promoted", "target": target, "manifest": manifest }))
        }
        MigrationCommand::Rollback { target, manifest } => {
            let pending = target.join("FORMAT.pending").try_exists()?;
            if !pending {
                return Err(CliFailure::invalid());
            }
            let pending_product = NativeProduct::open_pending(&target)?;
            drop(pending_product);
            fs::remove_dir_all(&target)?;
            sync_parent_directory(target.parent().unwrap_or_else(|| Path::new(".")))?;
            print_json(&json!({
                "status": "rolled_back",
                "target": target,
                "manifest": manifest,
                "source_retained": true,
            }))
        }
    }
}

fn reject_migration_path_overlap(
    source: &Path,
    target: &Path,
    manifest: &Path,
) -> Result<(), CliFailure> {
    let source = fs::canonicalize(source).map_err(|_| CliFailure::invalid())?;
    let target = canonicalize_for_output(target)?;
    let manifest = canonicalize_for_output(manifest)?;
    if target.starts_with(&source) || manifest.starts_with(&source) || source == target {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn canonicalize_for_output(path: &Path) -> Result<PathBuf, CliFailure> {
    if path.exists() {
        fs::canonicalize(path).map_err(|_| CliFailure::invalid())
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        Ok(fs::canonicalize(parent)
            .map_err(|_| CliFailure::invalid())?
            .join(path.file_name().ok_or_else(CliFailure::invalid)?))
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), CliFailure> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn migration_snapshot(source: &Path) -> Result<SnapshotContents, CliFailure> {
    let marker = fs::read(source.join("FORMAT"))?;
    if marker != b"hyphae-disk-format=2\n" {
        return Err(CliFailure::invalid());
    }
    let snapshot = latest_existing_snapshot(source)?;
    load_snapshot(snapshot, &SnapshotReadLimits::default()).map_err(|_| CliFailure::invalid())
}

fn source_receipts(
    snapshot: &SnapshotContents,
) -> Result<Vec<hyphae_storage::CommitReceipt>, CliFailure> {
    load_snapshot_for_migration(&snapshot.info.path, &SnapshotReadLimits::default())
        .map(|(_, receipts)| receipts.0)
        .map_err(|_| CliFailure::invalid())
}

fn latest_existing_snapshot(source: &Path) -> Result<PathBuf, CliFailure> {
    let mut candidates = fs::read_dir(source.join("snapshots"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let sequence = name
                .strip_prefix("snapshot-")?
                .strip_suffix(".hysnap")?
                .parse::<u64>()
                .ok()?;
            entry
                .file_type()
                .ok()?
                .is_file()
                .then_some((sequence, path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(sequence, _)| *sequence);
    candidates
        .pop()
        .map(|(_, path)| path)
        .ok_or_else(CliFailure::invalid)
}

fn migration_snapshot_json(snapshot: &SnapshotContents) -> Value {
    json!({
        "disk_format_version": snapshot.info.disk_format_version,
        "checkpoint_sequence": snapshot.info.checkpoint_sequence,
        "checkpoint_digest": snapshot.info.checkpoint_digest.map(|value| encode_hex(&value)),
        "snapshot_digest": encode_hex(&snapshot.info.snapshot_digest),
        "entry_count": snapshot.entries.len(),
        "vector_space_count": snapshot.vector_spaces.len(),
        "vector_count": snapshot.vectors.len(),
        "lexical_index_count": snapshot.lexical_indexes.len(),
        "receipt_count": snapshot.info.receipt_count,
    })
}

#[allow(clippy::too_many_lines)]
fn import_migration_snapshot(
    product: &mut NativeProduct,
    snapshot: &SnapshotContents,
) -> Result<MigrationManifest, CliFailure> {
    let catalog_objects = vec![
        LogicalCatalogObject::V2(CatalogObjectV2::Database(catalog_header(
            1,
            EngineKind::Kernel,
            "main.public.migration_database",
            None,
        )?)),
        LogicalCatalogObject::V2(CatalogObjectV2::Schema(catalog_header(
            2,
            EngineKind::Kernel,
            "main.public.migration_schema",
            Some(1),
        )?)),
        LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(KeyspaceDefinition {
            header: catalog_header(
                3,
                EngineKind::Structure,
                "main.public.legacy_records",
                Some(2),
            )?,
            kind: StructureKind::String,
            key_type: LogicalType::Binary,
            value_type: LogicalType::Binary,
            ownership: StructureOwnership::Canonical,
            ttl_policy: KeyspaceTtlPolicy::PerValue,
            default_ttl_millis: None,
            memory_class: KeyspaceMemoryClass::Durable,
            eviction: KeyspaceEvictionPolicy::None,
            relation_schema: None,
        })),
    ];
    product
        .create_catalog_objects_v2(catalog_objects, ProductDurability::Memory)
        .map_err(|error| {
            eprintln!("migration catalog create failed: {error:?}");
            error
        })?;

    let mut documents = Vec::with_capacity(snapshot.entries.len());
    let mut records = Vec::with_capacity(snapshot.entries.len());
    let mut document_ids = BTreeMap::new();
    for entry in &snapshot.entries {
        let object_id = migration_object_id(b"document", &entry.key)?;
        document_ids.insert(entry.key.clone(), object_id);
        records.push((entry.key.clone(), entry.value.clone()));
        documents.push(MigrationDocument {
            source_key: encode_hex(&entry.key),
            object_id: object_id.get().to_string(),
        });
    }
    if !records.is_empty() {
        for chunk in records.chunks(512) {
            product
                .migration_store_public_entries(chunk)
                .map_err(|error| {
                    eprintln!("migration public entries failed: {error:?}");
                    error
                })?;
        }
    }

    let (lexical_inputs, vector_inputs) = migration_search_inputs(snapshot, &document_ids)?;
    if !lexical_inputs.is_empty() || !vector_inputs.is_empty() {
        product
            .migration_store_search(&lexical_inputs, &vector_inputs)
            .map_err(|error| {
                eprintln!("migration search import failed: {error:?}");
                error
            })?;
    }

    let mut objects = vec![MigrationObject {
        kind: "legacy-records".to_owned(),
        source_identity: "format-2".to_owned(),
        target_id: "3".to_owned(),
    }];
    objects.sort();
    let source_receipts = source_receipts(snapshot)?;
    let receipts = source_receipts
        .iter()
        .map(|receipt| MigrationReceipt {
            transaction_id: encode_hex(receipt.transaction_id.as_bytes()),
            commit_sequence: receipt.commit_sequence,
            commit_digest: encode_hex(&receipt.commit_digest),
            transaction_digest: encode_hex(&receipt.transaction_digest),
            idempotency_identity: encode_hex(receipt.transaction_id.as_bytes()),
        })
        .collect::<Vec<_>>();
    let logical_digest = migration_logical_digest(snapshot);
    let lineage = product.directory_identity();
    let source = MigrationSource {
        disk_format_version: snapshot.info.disk_format_version,
        checkpoint_sequence: snapshot.info.checkpoint_sequence,
        checkpoint_digest: snapshot
            .info
            .checkpoint_digest
            .map(|value| encode_hex(&value)),
        snapshot_digest: encode_hex(&snapshot.info.snapshot_digest),
        entry_count: snapshot.info.entry_count,
        vector_space_count: snapshot.info.vector_space_count,
        vector_count: snapshot.info.vector_count,
        lexical_index_count: snapshot.info.lexical_index_count,
        receipt_count: snapshot.info.receipt_count,
        vector_spaces: snapshot
            .vector_spaces
            .iter()
            .map(|space| MigrationVectorSpace {
                name: space.name.to_string(),
                dimension: space.dimension,
                metric: space.metric as u8,
            })
            .collect(),
        lexical_indexes: snapshot
            .lexical_indexes
            .iter()
            .map(|index| MigrationLexicalIndex {
                name: index.name.to_string(),
                fields: index
                    .fields
                    .iter()
                    .map(|field| MigrationLexicalField {
                        path: field.path.segments().to_vec(),
                        weight_micros: field.weight_micros,
                    })
                    .collect(),
            })
            .collect(),
    };
    Ok(MigrationManifest {
        version: hyphae_native_runtime::NATIVE_MIGRATION_MANIFEST_VERSION,
        kind: hyphae_native_runtime::NATIVE_MIGRATION_MANIFEST_KIND.to_owned(),
        source,
        target: MigrationTarget {
            directory_id: lineage.directory_id().to_owned(),
            history_epoch: lineage.history_epoch(),
            entry_count: documents.len() as u64,
            vector_space_count: snapshot.vector_spaces.len() as u64,
            vector_count: snapshot.vectors.len() as u64,
            lexical_index_count: snapshot.lexical_indexes.len() as u64,
            receipt_count: receipts.len() as u64,
            logical_digest: logical_digest.clone(),
        },
        objects,
        documents,
        receipts,
        proof_anchors: vec![MigrationProofAnchor {
            kind: "format-2-snapshot".to_owned(),
            source_digest: encode_hex(&snapshot.info.snapshot_digest),
            target_digest: logical_digest.clone(),
        }],
    })
}

fn migration_object_id(prefix: &[u8], key: &[u8]) -> Result<ObjectId, CliFailure> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-migration-object-v1");
    hasher.update(prefix);
    hasher.update(key);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    object_id(u128::from_be_bytes(bytes) | 1)
}

fn migration_logical_digest(snapshot: &SnapshotContents) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-migration-logical-v1");
    for entry in &snapshot.entries {
        hasher.update(&(entry.key.len() as u64).to_le_bytes());
        hasher.update(&entry.key);
        hasher.update(&(entry.value.len() as u64).to_le_bytes());
        hasher.update(&entry.value);
    }
    for space in &snapshot.vector_spaces {
        hasher.update(b"vector-space");
        hasher.update(space.name.as_str().as_bytes());
        hasher.update(&space.dimension.to_le_bytes());
        hasher.update(&[space.metric as u8]);
    }
    for vector in &snapshot.vectors {
        hasher.update(b"vector");
        hasher.update(vector.space.as_str().as_bytes());
        hasher.update(&(vector.key.len() as u64).to_le_bytes());
        hasher.update(&vector.key);
        for value in vector.vector.as_slice() {
            hasher.update(&value.to_le_bytes());
        }
    }
    for index in &snapshot.lexical_indexes {
        hasher.update(b"lexical-index");
        hasher.update(index.name.as_str().as_bytes());
        for field in &index.fields {
            for segment in field.path.segments() {
                hasher.update(&(segment.len() as u64).to_le_bytes());
                hasher.update(segment.as_bytes());
            }
            hasher.update(&field.weight_micros.to_le_bytes());
        }
    }
    for receipt in &snapshot.info.receipt_count.to_le_bytes() {
        hasher.update(b"receipt");
        hasher.update(&[*receipt]);
    }
    encode_hex(hasher.finalize().as_bytes())
}

fn decode_legacy_document(encoded: &[u8]) -> Result<LegacyValue, CliFailure> {
    decode_document(encoded).map_err(|_| CliFailure::invalid())
}

fn migration_search_inputs(
    snapshot: &SnapshotContents,
    document_ids: &BTreeMap<Vec<u8>, ObjectId>,
) -> Result<
    (
        Vec<MigrationLexicalIndexInput>,
        Vec<MigrationVectorIndexInput>,
    ),
    CliFailure,
> {
    let mut lexical_inputs = Vec::with_capacity(snapshot.lexical_indexes.len());
    for definition in &snapshot.lexical_indexes {
        let index = migration_object_id(b"lexical-index", definition.name.as_str().as_bytes())?;
        let mut documents = Vec::with_capacity(snapshot.entries.len());
        for entry in &snapshot.entries {
            let value = decode_legacy_document(&entry.value)?;
            let document_id = document_ids
                .get(&entry.key)
                .ok_or_else(CliFailure::internal)?
                .get()
                .to_be_bytes()
                .to_vec();
            documents.push((document_id, lexical_text(&value, definition)));
        }
        lexical_inputs.push(MigrationLexicalIndexInput {
            index,
            name: format!("__migration_lexical_{}", definition.name),
            documents,
        });
    }
    let mut vector_inputs = Vec::with_capacity(snapshot.vector_spaces.len());
    for definition in &snapshot.vector_spaces {
        let index = migration_object_id(b"vector-space", definition.name.as_str().as_bytes())?;
        let mut vectors = Vec::new();
        for vector in snapshot
            .vectors
            .iter()
            .filter(|vector| vector.space == definition.name)
        {
            let object_id = document_ids
                .get(&vector.key)
                .ok_or_else(CliFailure::internal)?;
            vectors.push((
                *object_id,
                vector
                    .vector
                    .as_slice()
                    .iter()
                    .map(|value| f32::from(*value) / 32_767.0)
                    .collect(),
            ));
        }
        vector_inputs.push(MigrationVectorIndexInput {
            index,
            name: format!("__migration_vector_{}", definition.name),
            dimension: definition.dimension,
            vectors,
        });
    }
    Ok((lexical_inputs, vector_inputs))
}

fn lexical_text(
    value: &LegacyValue,
    definition: &hyphae_retrieval::LexicalIndexDefinition,
) -> String {
    definition
        .fields
        .iter()
        .filter_map(|field| field.path.resolve(value))
        .filter_map(|value| match value {
            LegacyValue::String(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn verify_migration_target(
    target: &Path,
    snapshot: &SnapshotContents,
    manifest: &MigrationManifest,
) -> Result<(), CliFailure> {
    if manifest.source.snapshot_digest != encode_hex(&snapshot.info.snapshot_digest)
        || manifest.documents.len() != snapshot.entries.len()
        || manifest.receipts.len()
            != usize::try_from(snapshot.info.receipt_count).map_err(|_| CliFailure::invalid())?
    {
        return Err(CliFailure::invalid());
    }
    let product = match NativeProduct::open_pending(target) {
        Ok(product) => product,
        Err(_) => NativeProduct::open(target)?,
    };
    let entries = snapshot
        .entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<Vec<_>>();
    if !product
        .migration_verify_public_entries(&entries)
        .map_err(|error| {
            eprintln!("migration public verification error: {error:?}");
            error
        })?
    {
        return Err(CliFailure::invalid());
    }
    verify_migration_product(&product, snapshot, manifest)?;
    let lineage = product.directory_identity();
    if manifest.target.directory_id != lineage.directory_id()
        || manifest.target.history_epoch != lineage.history_epoch()
    {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn verify_migration_product(
    product: &NativeProduct,
    snapshot: &SnapshotContents,
    manifest: &MigrationManifest,
) -> Result<(), CliFailure> {
    if manifest.target.entry_count != snapshot.entries.len() as u64
        || manifest.target.vector_space_count != snapshot.vector_spaces.len() as u64
        || manifest.target.vector_count != snapshot.vectors.len() as u64
        || manifest.target.lexical_index_count != snapshot.lexical_indexes.len() as u64
        || manifest.target.receipt_count != snapshot.info.receipt_count
        || manifest.source.entry_count != snapshot.info.entry_count
        || manifest.source.vector_space_count != snapshot.info.vector_space_count
        || manifest.source.vector_count != snapshot.info.vector_count
        || manifest.source.lexical_index_count != snapshot.info.lexical_index_count
        || manifest.source.receipt_count != snapshot.info.receipt_count
    {
        return Err(CliFailure::invalid());
    }
    let target_digest = migration_logical_digest(snapshot);
    if manifest.target.logical_digest != target_digest {
        return Err(CliFailure::invalid());
    }
    let mut expected_documents = Vec::with_capacity(snapshot.entries.len());
    let mut document_ids = BTreeMap::new();
    for entry in &snapshot.entries {
        let object_id = migration_object_id(b"document", &entry.key)?;
        document_ids.insert(entry.key.clone(), object_id);
        expected_documents.push(MigrationDocument {
            source_key: encode_hex(&entry.key),
            object_id: object_id.get().to_string(),
        });
    }
    expected_documents.sort();
    if manifest.documents != expected_documents {
        return Err(CliFailure::invalid());
    }
    let source_receipts = source_receipts(snapshot)?;
    let expected_receipts = source_receipts
        .iter()
        .map(|receipt| MigrationReceipt {
            transaction_id: encode_hex(receipt.transaction_id.as_bytes()),
            commit_sequence: receipt.commit_sequence,
            commit_digest: encode_hex(&receipt.commit_digest),
            transaction_digest: encode_hex(&receipt.transaction_digest),
            idempotency_identity: encode_hex(receipt.transaction_id.as_bytes()),
        })
        .collect::<Vec<_>>();
    if manifest.receipts != expected_receipts {
        return Err(CliFailure::invalid());
    }
    let (lexical_inputs, vector_inputs) = migration_search_inputs(snapshot, &document_ids)?;
    let lexical_expected = lexical_inputs
        .into_iter()
        .map(|input| (input.index, input.documents))
        .collect::<Vec<_>>();
    let vector_expected = vector_inputs
        .into_iter()
        .map(|input| (input.index, input.vectors))
        .collect::<Vec<_>>();
    if !product
        .migration_verify_search(&lexical_expected, &vector_expected)
        .map_err(|error| {
            eprintln!("migration search verification error: {error:?}");
            error
        })?
    {
        return Err(CliFailure::invalid());
    }
    let mut expected_objects = vec![MigrationObject {
        kind: "legacy-records".to_owned(),
        source_identity: "format-2".to_owned(),
        target_id: "3".to_owned(),
    }];
    expected_objects.sort();
    if manifest.objects != expected_objects {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn open_client(local: &LocalDirectory) -> Result<EmbeddedClient, CliFailure> {
    EmbeddedClient::open(
        NativeProduct::open(&local.data_dir).map_err(Box::new)?,
        local.native_api_key_file.as_deref(),
        local.native_api_key_stdin,
    )
}

fn init(local: &LocalDirectory) -> Result<(), CliFailure> {
    let product = NativeProduct::create(&local.data_dir)?;
    let native_directory_format = product.capabilities().native_directory_format;
    drop(product);
    print_json(&json!({
        "status": "initialized",
        "data_path": local.data_dir,
        "native_directory_format": native_directory_format,
    }))
}

fn dispatch(local: &LocalDirectory, operation: ProductOperation) -> Result<(), CliFailure> {
    let response = open_client(local)?.dispatch(operation)?;
    print_json(&response_json(response))
}

#[allow(clippy::too_many_lines)]
fn catalog(local: &LocalDirectory, command: CatalogCommand) -> Result<(), CliFailure> {
    let operation = match command {
        CatalogCommand::List {
            limit,
            visit_limit,
            byte_limit,
            kind,
            parent,
            cursor,
        } => ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
            filter: CatalogVisibleListFilter {
                parent: parent.map(object_id).transpose()?,
                kind: kind.map(Into::into),
            },
            cursor: cursor
                .map(|token| decode_catalog_cursor_token(&token))
                .transpose()?,
            item_limit: limit,
            visit_limit,
            byte_limit,
        }),
        CatalogCommand::Describe { id } => ProductOperation::CatalogDescribe { id: object_id(id)? },
        CatalogCommand::Resolve { name } => ProductOperation::CatalogResolve {
            name: qualified_name(&name)?,
        },
        CatalogCommand::Dependencies {
            id,
            direction,
            limit,
            visit_limit,
            byte_limit,
        } => ProductOperation::CatalogDependencies(CatalogDependencyRequest {
            object: object_id(id)?,
            direction: direction.into(),
            cursor: None,
            item_limit: limit,
            visit_limit,
            byte_limit,
        }),
        CatalogCommand::CreateKeyspace {
            id,
            parent,
            name,
            family,
            durability,
        } => {
            let object = LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(KeyspaceDefinition {
                header: catalog_header(id, EngineKind::Structure, &name, Some(parent))?,
                kind: family.into(),
                key_type: LogicalType::Binary,
                value_type: LogicalType::Binary,
                ownership: StructureOwnership::Canonical,
                ttl_policy: KeyspaceTtlPolicy::PerValue,
                default_ttl_millis: None,
                memory_class: KeyspaceMemoryClass::Durable,
                eviction: KeyspaceEvictionPolicy::None,
                relation_schema: None,
            }));
            let response = open_client(local)?.dispatch_with_durability(
                ProductOperation::CatalogCreate { object },
                durability.into(),
            )?;
            return print_json(&response_json(response));
        }
        CatalogCommand::CreateSearchCollection {
            database,
            schema,
            collection,
            analyzer,
            name,
            dimension,
            analyzer_ascii_folding,
            analyzer_english_stop,
            analyzer_english_stem,
            memory_schema,
            reuse_schema,
            bm25_k1_micros,
            bm25_b_micros,
            durability,
        } => {
            let mut objects = Vec::new();
            if !reuse_schema {
                objects.extend([
                    LogicalCatalogObject::V2(CatalogObjectV2::Database(catalog_header(
                        database,
                        EngineKind::Kernel,
                        "main.public.database",
                        None,
                    )?)),
                    LogicalCatalogObject::V2(CatalogObjectV2::Schema(catalog_header(
                        schema,
                        EngineKind::Kernel,
                        "main.public.schema",
                        Some(database),
                    )?)),
                ]);
                let analyzer_name = format!("{name}_analyzer");
                objects.push(LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(
                    AnalyzerDefinition {
                        header: catalog_header(
                            analyzer,
                            EngineKind::Search,
                            &analyzer_name,
                            Some(schema),
                        )?,
                        tokenizer: AnalyzerTokenizer::UnicodeWord,
                        filters: {
                            let mut filters = vec![AnalyzerFilter::Lowercase];
                            if analyzer_ascii_folding {
                                filters.push(AnalyzerFilter::AsciiFolding);
                            }
                            if analyzer_english_stop {
                                filters.push(AnalyzerFilter::EnglishStopV1);
                            }
                            if analyzer_english_stem {
                                filters.push(AnalyzerFilter::EnglishStemV1);
                            }
                            filters
                        },
                    },
                )));
            }
            let ann = AnnIndexDefinition::new(VectorMetric::SquaredL2, 8, 32, 16, 256, 7)
                .map_err(|_| CliFailure::invalid())?;
            let lifecycle = IncrementalVectorLifecycle {
                delta_max_entries: 1_000,
                consolidate_after_deltas: 4,
                retain_generations: 2,
            };
            let object = LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
                SearchCollectionDefinitionV2 {
                    header: catalog_header(collection, EngineKind::Search, &name, Some(schema))?,
                    bm25: match (bm25_k1_micros, bm25_b_micros) {
                        (Some(k1_micros), Some(b_micros)) => {
                            Some(hyphae_native_catalog::Bm25Parameters {
                                k1_micros,
                                b_micros,
                            })
                        }
                        _ => None,
                    },
                    fields: {
                        let mut fields = vec![SearchFieldDefinitionV2 {
                            id: field_id(1)?,
                            name: catalog_name("body")?,
                            logical_type: LogicalType::Text,
                            analyzer: Some(object_id(analyzer)?),
                            options: SearchFieldOptions {
                                stored: true,
                                doc_values: false,
                                source: FieldSourcePolicy::Retained,
                                lexical: LexicalIndexPolicy::Frequencies,
                            },
                        }];
                        if memory_schema {
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(2)?,
                                name: catalog_name("project")?,
                                logical_type: LogicalType::Text,
                                analyzer: None,
                                options: doc_value_options(),
                            });
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(3)?,
                                name: catalog_name("kind")?,
                                logical_type: LogicalType::Text,
                                analyzer: None,
                                options: doc_value_options(),
                            });
                            for (id, field) in [(4, "layer"), (5, "harness"), (6, "model")] {
                                fields.push(SearchFieldDefinitionV2 {
                                    id: field_id(id)?,
                                    name: catalog_name(field)?,
                                    logical_type: LogicalType::Text,
                                    analyzer: None,
                                    options: doc_value_options(),
                                });
                            }
                            for (id, field) in [(9, "session"), (10, "actor"), (11, "date_anchor")]
                            {
                                fields.push(SearchFieldDefinitionV2 {
                                    id: field_id(id)?,
                                    name: catalog_name(field)?,
                                    logical_type: LogicalType::Text,
                                    analyzer: None,
                                    options: doc_value_options(),
                                });
                            }
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(12)?,
                                name: catalog_name("session_ts")?,
                                logical_type: LogicalType::Signed(IntegerWidth::Bits64),
                                analyzer: None,
                                options: doc_value_options(),
                            });
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(13)?,
                                name: catalog_name("turn_ord")?,
                                logical_type: LogicalType::Signed(IntegerWidth::Bits64),
                                analyzer: None,
                                options: doc_value_options(),
                            });
                        } else {
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(2)?,
                                name: catalog_name("category")?,
                                logical_type: LogicalType::Text,
                                analyzer: None,
                                options: doc_value_options(),
                            });
                            fields.push(SearchFieldDefinitionV2 {
                                id: field_id(3)?,
                                name: catalog_name("price")?,
                                logical_type: LogicalType::Signed(IntegerWidth::Bits64),
                                analyzer: None,
                                options: doc_value_options(),
                            });
                        }
                        fields
                    },
                    vectors: vec![
                        NamedVectorDefinition {
                            id: field_id(7)?,
                            name: catalog_name("exact")?,
                            vector_type: vector_type(dimension)?,
                            metric: VectorMetric::SquaredL2,
                            policy: VectorSearchPolicy::Exact,
                            lifecycle,
                        },
                        NamedVectorDefinition {
                            id: field_id(8)?,
                            name: catalog_name("ann")?,
                            vector_type: vector_type(dimension)?,
                            metric: VectorMetric::SquaredL2,
                            policy: VectorSearchPolicy::Ann(ann),
                            lifecycle,
                        },
                    ],
                },
            ));
            objects.push(object);
            let mut client = open_client(local)?;
            let receipt = client
                .unmanaged_product_mut()?
                .create_catalog_objects_v2(objects, durability.into())?;
            return print_json(&response_json(ProductResponse::CatalogCreated(
                ProductCommitOutcome::Committed(receipt),
            )));
        }
    };
    dispatch(local, operation)
}

fn encode_catalog_cursor_token(bytes: &[u8]) -> String {
    let mut token = String::with_capacity(7 + bytes.len().saturating_mul(2));
    token.push_str("hycatv1:");
    token.push_str(&encode_hex(bytes));
    token
}

fn decode_catalog_cursor_token(token: &str) -> Result<CatalogVisibleCursor, CliFailure> {
    let conflict = || CliFailure::from(ProductError::from_code(ProductErrorCode::CatalogConflict));
    let encoded = token.strip_prefix("hycatv1:").ok_or_else(&conflict)?;
    if encoded.is_empty() || encoded.len() % 2 != 0 {
        return Err(conflict());
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = decode_hex_nibble(pair[0]).ok_or_else(&conflict)?;
            let low = decode_hex_nibble(pair[1]).ok_or_else(&conflict)?;
            Ok((high << 4) | low)
        })
        .collect::<Result<Vec<_>, CliFailure>>()?;
    let cursor = CatalogVisibleCursor::new(bytes).map_err(CliFailure::from)?;
    if encode_catalog_cursor_token(cursor.as_bytes()) != token {
        return Err(conflict());
    }
    Ok(cursor)
}

const fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn sql(local: &LocalDirectory, command: SqlCommand) -> Result<(), CliFailure> {
    match command {
        SqlCommand::Execute {
            statement,
            parameters,
            durability,
        } => {
            let parameters = parameter_strings(&parameters)?;
            let response = open_client(local)?.dispatch_with_durability(
                ProductOperation::ExecuteSql {
                    statement,
                    parameters,
                },
                durability.into(),
            )?;
            print_json(&response_json(response))
        }
        SqlCommand::Prepared {
            statement,
            parameters,
        } => {
            let mut client = open_client(local)?;
            let prepared = client.dispatch(ProductOperation::PrepareSql { statement })?;
            let ProductResponse::PreparedSql { handle, .. } = prepared else {
                return Err(CliFailure::internal());
            };
            let response = client.dispatch(ProductOperation::ExecutePrepared {
                handle,
                parameters: parameter_strings(&parameters)?,
            })?;
            client.dispatch(ProductOperation::DeallocatePrepared { handle })?;
            print_json(&json!({
                "handle": handle.get(),
                "result": response_json(response),
                "deallocated": true,
            }))
        }
    }
}

fn structure(local: &LocalDirectory, command: StructureCommand) -> Result<(), CliFailure> {
    let (operation, durability) = match command {
        StructureCommand::Get { key } => (
            ProductOperation::StructureGet {
                key: key.into_bytes(),
            },
            ProductDurability::Strict,
        ),
        StructureCommand::Set {
            key,
            value,
            expires_at_micros,
            durability,
        } => (
            ProductOperation::StructureSet {
                key: key.into_bytes(),
                value: value.into_bytes(),
                expires_at_micros,
            },
            durability.into(),
        ),
        StructureCommand::Ttl { key } => (
            ProductOperation::StructureTtl {
                key: key.into_bytes(),
            },
            ProductDurability::Strict,
        ),
        StructureCommand::Batch {
            mutations_json,
            durability,
        } => (
            ProductOperation::StructureMutate {
                mutations: serde_json::from_str::<Vec<StructureMutationInput>>(&mutations_json)?
                    .into_iter()
                    .map(structure_mutation)
                    .collect::<Result<_, _>>()?,
            },
            durability.into(),
        ),
        StructureCommand::Read { request_json } => (
            ProductOperation::StructureRead(structure_read(serde_json::from_str(&request_json)?)?),
            ProductDurability::Strict,
        ),
    };
    let response = open_client(local)?.dispatch_with_durability(operation, durability)?;
    print_json(&response_json(response))
}

#[allow(clippy::too_many_lines)]
fn search(local: &LocalDirectory, command: SearchCommand) -> Result<(), CliFailure> {
    match command {
        SearchCommand::Consolidate {
            collection,
            durability,
        } => {
            let mut client = open_client(local)?;
            let collection = object_id(collection)?;
            let binding = client
                .unmanaged_product_mut()?
                .resolve_search_collection_binding(collection, native::logical_time_micros())?;
            let mut receipts = Vec::new();
            for vector in &binding.vectors {
                let status = client
                    .unmanaged_product_mut()?
                    .administration()
                    .ann_maintenance_status(vector.index)?;
                // An index without deltas has nothing to consolidate, and the
                // capture bound must stay inside the index's own delta limit.
                if status.delta_records == 0 {
                    receipts.push(json!({
                        "target": vector.name,
                        "consumed_delta_records": 0,
                        "skipped": true,
                    }));
                    continue;
                }
                let max_delta_records = hyphae_native_runtime::MAX_ANN_DELTA_RECORDS
                    .min(usize::try_from(status.lifecycle.delta_max_entries).unwrap_or(usize::MAX))
                    .max(1);
                let receipt = client
                    .unmanaged_product_mut()?
                    .administration()
                    .consolidate_ann(hyphae_native_product::AnnConsolidationRequest {
                        index: vector.index,
                        max_vectors: hyphae_native_runtime::MAX_ANN_CONSOLIDATION_VECTORS,
                        max_delta_records,
                        durability: durability.into(),
                    })?;
                receipts.push(json!({
                    "target": vector.name,
                    "consumed_delta_records": receipt.consumed_delta_records,
                    "effective_vector_count": receipt.effective_vector_count,
                }));
            }
            print_json(&json!({
                "schema": "hyphae-search-consolidation-v1",
                "collection": collection.get().to_string(),
                "consolidations": receipts,
            }))
        }
        SearchCommand::Provision {
            collection,
            durability,
        } => {
            let mut client = open_client(local)?;
            let collection = object_id(collection)?;
            let receipt = client
                .unmanaged_product_mut()?
                .provision_search_collection(
                    collection,
                    native::logical_time_micros(),
                    durability.into(),
                )?;
            let binding = client
                .unmanaged_product_mut()?
                .resolve_search_collection_binding(collection, native::logical_time_micros())?;
            print_json(&json!({
                "result": response_json(ProductResponse::SearchIngested(receipt)),
                "binding": {
                    "collection": binding.collection.get().to_string(),
                    "lexical_index": binding.lexical_index.get().to_string(),
                    "vectors": binding.vectors.into_iter().map(|vector| json!({
                        "name": vector.name,
                        "index": vector.index.get().to_string(),
                    })).collect::<Vec<_>>(),
                },
            }))
        }
        SearchCommand::Query {
            index,
            query,
            kind,
            max_distance,
            limit,
        } => {
            let query = match kind {
                SearchQueryKind::Term => hyphae_native_product::BoundedSearchQuery::Term(query),
                SearchQueryKind::Phrase => hyphae_native_product::BoundedSearchQuery::Phrase(query),
                SearchQueryKind::Prefix => hyphae_native_product::BoundedSearchQuery::Prefix(query),
                SearchQueryKind::Fuzzy => hyphae_native_product::BoundedSearchQuery::Fuzzy {
                    term: query,
                    max_distance,
                },
            };
            dispatch(
                local,
                ProductOperation::Search {
                    index: object_id(index)?,
                    query,
                    limit,
                },
            )
        }
        SearchCommand::Integrated {
            collection,
            lexical,
            vector_target,
            vector,
            vector_strategy,
            ef_search,
            candidate_limit,
            limit,
            filter_json,
            sort_json,
            facets_json,
            metrics_json,
            fusion,
            dedupe_field,
            dedupe_first_k,
            highlight_fragments,
            highlight_bytes,
        } => {
            let vectors = match vector_target {
                Some(target) => vec![ProductVectorBranch {
                    target,
                    query: ProductVector::new(vector).map_err(|_| CliFailure::invalid())?,
                    candidate_limit,
                    weight: 1,
                    execution: Some(match vector_strategy {
                        IntegratedVectorStrategy::Exact => ProductVectorExecution::Exact,
                        IntegratedVectorStrategy::Ann => ProductVectorExecution::Ann {
                            ef_search,
                            exact_rerank: Some(candidate_limit),
                        },
                        IntegratedVectorStrategy::Adaptive => ProductVectorExecution::Adaptive {
                            exact_candidate_threshold: 2,
                            ef_search,
                            exact_rerank: Some(candidate_limit),
                        },
                    }),
                }],
                None if vector.is_empty() => Vec::new(),
                None => return Err(CliFailure::invalid()),
            };
            dispatch(
                local,
                ProductOperation::SearchCollection {
                    collection: object_id(collection)?,
                    request: ProductSearchRequest {
                        lexical: lexical.map(|query| ProductLexicalBranch {
                            query,
                            candidate_limit,
                            weight: 1,
                        }),
                        vectors,
                        filter: filter_json
                            .map(|value| serde_json::from_str::<Value>(&value))
                            .transpose()?
                            .map(product_search_filter)
                            .transpose()?
                            .unwrap_or(ProductSearchFilter::MatchAll),
                        sort: sort_json
                            .map(|value| serde_json::from_str::<Vec<Value>>(&value))
                            .transpose()?
                            .unwrap_or_default()
                            .into_iter()
                            .map(product_search_sort)
                            .collect::<Result<_, _>>()?,
                        facets: facets_json
                            .map(|value| serde_json::from_str::<Vec<Value>>(&value))
                            .transpose()?
                            .unwrap_or_default()
                            .into_iter()
                            .map(product_facet)
                            .collect::<Result<_, _>>()?,
                        aggregations: metrics_json
                            .map(|value| serde_json::from_str::<Vec<Value>>(&value))
                            .transpose()?
                            .unwrap_or_default()
                            .into_iter()
                            .map(product_aggregation)
                            .collect::<Result<_, _>>()?,
                        limit,
                        fusion: fusion.map(|method| match method {
                            FusionMethodInput::WeightedScore => {
                                hyphae_native_product::ProductFusionMethod::WeightedScore
                            }
                        }),
                        parent_dedupe: match (dedupe_field, dedupe_first_k) {
                            (Some(field), Some(first_k)) => {
                                Some(hyphae_native_product::ProductParentDedupe { field, first_k })
                            }
                            _ => None,
                        },
                        rerank: None,
                        highlight: highlight_fragments.map(|max_fragments| {
                            hyphae_native_product::ProductHighlight {
                                max_fragments,
                                fragment_bytes: highlight_bytes,
                            }
                        }),
                        autocut: None,
                    },
                },
            )
        }
        SearchCommand::Chunk {
            parent,
            text,
            file,
            size,
            overlap,
            sentence,
            sentence_max,
        } => {
            let source = match (text, file) {
                (Some(text), None) => text,
                (None, Some(path)) => {
                    std::fs::read_to_string(path).map_err(|_| CliFailure::invalid())?
                }
                _ => return Err(CliFailure::invalid()),
            };
            let mode = if sentence {
                hyphae_native_product::chunker::ChunkerMode::SentenceBounded {
                    target: size,
                    maximum: sentence_max,
                }
            } else {
                hyphae_native_product::chunker::ChunkerMode::FixedBytes { size, overlap }
            };
            let documents = hyphae_native_product::chunker::chunk_documents(
                parent,
                &source,
                hyphae_native_product::chunker::ChunkerConfig { mode },
            )
            .map_err(|_| CliFailure::invalid())?;
            let rendered = documents
                .into_iter()
                .map(|document| {
                    json!({
                        "id": document.object_id.get().to_string(),
                        "text": document.text,
                        "doc_values": document
                            .doc_values
                            .into_iter()
                            .map(|(name, value)| (name, doc_value_json(value)))
                            .collect::<serde_json::Map<_, _>>(),
                    })
                })
                .collect::<Vec<_>>();
            print_json(&json!({
                "schema": "hyphae-chunk-documents-v1",
                "parent": parent.to_string(),
                "documents": rendered,
            }))
        }
        SearchCommand::Ingest {
            collection,
            idempotency_id,
            documents_json,
            durability,
        } => {
            if idempotency_id == 0 {
                return Err(CliFailure::invalid());
            }
            let documents = serde_json::from_str::<Vec<IngestDocument>>(&documents_json)?
                .into_iter()
                .map(product_document)
                .collect::<Result<Vec<_>, _>>()?;
            let response = open_client(local)?.dispatch_with_durability(
                ProductOperation::SearchIngest {
                    collection: object_id(collection)?,
                    batch: ProductSearchIngestBatch {
                        idempotency_id,
                        documents,
                    },
                },
                durability.into(),
            )?;
            print_json(&response_json(response))
        }
        SearchCommand::Update {
            collection,
            idempotency_id,
            document_json,
            durability,
        } => {
            let document = product_document(serde_json::from_str(&document_json)?)?;
            let response = open_client(local)?.dispatch_with_durability(
                ProductOperation::SearchDocumentUpdate {
                    collection: object_id(collection)?,
                    update: ProductSearchDocumentUpdate {
                        idempotency_id,
                        document,
                    },
                },
                durability.into(),
            )?;
            print_json(&response_json(response))
        }
        SearchCommand::Delete {
            collection,
            idempotency_id,
            document,
            durability,
        } => {
            let response = open_client(local)?.dispatch_with_durability(
                ProductOperation::SearchDocumentDelete {
                    collection: object_id(collection)?,
                    delete: ProductSearchDocumentDelete {
                        idempotency_id,
                        object_id: object_id(document)?,
                    },
                },
                durability.into(),
            )?;
            print_json(&response_json(response))
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn transaction(local: &LocalDirectory, command: TransactionCommand) -> Result<(), CliFailure> {
    match command {
        TransactionCommand::Status { id } => dispatch(
            local,
            ProductOperation::TransactionStatus {
                transaction_id: hyphae_native_product::ProductTransactionId::new(id)
                    .ok_or_else(CliFailure::invalid)?,
            },
        ),
        TransactionCommand::Execute {
            steps_json,
            durability,
        } => execute_transaction(local, &steps_json, durability.into()),
    }
}

fn execute_transaction(
    local: &LocalDirectory,
    steps_json: &str,
    durability: ProductDurability,
) -> Result<(), CliFailure> {
    let steps = serde_json::from_str::<Vec<TransactionStepInput>>(steps_json)?;
    if steps.is_empty()
        || !matches!(
            steps.last(),
            Some(TransactionStepInput::Commit | TransactionStepInput::Rollback)
        )
        || steps[..steps.len() - 1].iter().any(|step| {
            matches!(
                step,
                TransactionStepInput::Commit | TransactionStepInput::Rollback
            )
        })
    {
        return Err(CliFailure::invalid());
    }
    let mut client = open_client(local)?;
    let began = client.dispatch_with_durability(ProductOperation::TransactionBegin, durability)?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = began
    else {
        return Err(CliFailure::internal());
    };
    let mut results = vec![response_json(began)];
    for step in steps {
        let operation = transaction_step(handle, step)?;
        results.push(response_json(
            client.dispatch_with_durability(operation, durability)?,
        ));
    }
    print_json(&json!({ "handle": handle.get(), "steps": results }))
}

fn transaction_step(
    handle: ProductTransactionHandle,
    step: TransactionStepInput,
) -> Result<ProductOperation, CliFailure> {
    Ok(match step {
        TransactionStepInput::Status => ProductOperation::ExplicitTransactionStatus { handle },
        TransactionStepInput::StageSql {
            statement,
            parameters,
        } => ProductOperation::TransactionStageSql {
            handle,
            mutation: ProductTransactionSqlMutation {
                statement,
                parameters: parameters
                    .into_iter()
                    .map(product_value)
                    .collect::<Result<_, _>>()?,
            },
        },
        TransactionStepInput::StageStructure { mutation } => {
            ProductOperation::TransactionStageStructure {
                handle,
                mutation: structure_mutation(mutation)?,
            }
        }
        TransactionStepInput::StageSearch {
            action,
            index,
            document_id,
            text,
        } => {
            let index = object_id(index.0)?;
            let document_id = document_id.into_bytes();
            let mutation = match action {
                SearchMutationAction::Index => ProductTransactionSearchMutation::Index {
                    index,
                    document_id,
                    text: text.ok_or_else(CliFailure::invalid)?,
                },
                SearchMutationAction::Replace => ProductTransactionSearchMutation::Replace {
                    index,
                    document_id,
                    text: text.ok_or_else(CliFailure::invalid)?,
                },
                SearchMutationAction::Delete if text.is_none() => {
                    ProductTransactionSearchMutation::Delete { index, document_id }
                }
                SearchMutationAction::Delete => return Err(CliFailure::invalid()),
            };
            ProductOperation::TransactionStageSearch { handle, mutation }
        }
        TransactionStepInput::StageVector {
            action,
            index,
            object_id: vector_object_id,
            vector,
        } => {
            let mutation = match action {
                VectorMutationAction::Upsert => ProductTransactionVectorMutation::Upsert {
                    index: object_id(index.0)?,
                    object_id: object_id(vector_object_id.0)?,
                    vector: ProductVector::new(vector).map_err(|_| CliFailure::invalid())?,
                },
                VectorMutationAction::Delete if vector.is_empty() => {
                    ProductTransactionVectorMutation::Delete {
                        index: object_id(index.0)?,
                        object_id: object_id(vector_object_id.0)?,
                    }
                }
                VectorMutationAction::Delete => return Err(CliFailure::invalid()),
            };
            ProductOperation::TransactionStageVector { handle, mutation }
        }
        TransactionStepInput::Commit => ProductOperation::TransactionCommit { handle },
        TransactionStepInput::Rollback => ProductOperation::TransactionRollback { handle },
    })
}

fn explain(local: &LocalDirectory, command: ExplainCommand) -> Result<(), CliFailure> {
    let ExplainCommand::Sql { statement } = command;
    dispatch(local, ProductOperation::AdminExplainSql { statement })
}

fn telemetry(local: &LocalDirectory) -> Result<(), CliFailure> {
    dispatch(local, ProductOperation::Telemetry)
}

fn security(local: &LocalDirectory, command: SecurityCommand) -> Result<(), CliFailure> {
    match command {
        SecurityCommand::Status => dispatch(local, ProductOperation::SecurityStatus),
        SecurityCommand::Principal { operation } => security_principal(local, operation),
        SecurityCommand::Role { operation } => security_role(local, operation),
        SecurityCommand::Assignment { operation } => security_assignment(local, operation),
        SecurityCommand::Key { operation } => security_key(local, operation),
        SecurityCommand::Audit { operation } => security_audit(local, operation),
        SecurityCommand::Bootstrap {
            name,
            label,
            key_out,
        } => {
            let mut product = NativeProduct::open(&local.data_dir)?;
            let receipt = product.bootstrap_access_control_to_file(
                &name,
                &label,
                &key_out,
                native::logical_time_micros(),
            )?;
            print_json(&json!({
                "schema": "hyphae-native-access-control-bootstrap-v1",
                "status": "bootstrapped",
                "principal_id": receipt.principal_id.to_string(),
                "key_id": receipt.key_id.to_string(),
                "authorization_epoch": receipt.authorization_epoch.get(),
                "key_file": key_out,
                "commit": commit_json(receipt.commit),
            }))
        }
        SecurityCommand::Owner { operation } => security_owner(&local.data_dir, operation),
        SecurityCommand::LegacyBearer { operation } => security_legacy_bearer(local, operation),
    }
}

fn security_legacy_bearer(
    local: &LocalDirectory,
    command: SecurityLegacyBearerCommand,
) -> Result<(), CliFailure> {
    match command {
        SecurityLegacyBearerCommand::Migrate {
            name,
            label,
            legacy_bearer_file,
            key_out,
        } => {
            if local.native_api_key_file.is_some() || local.native_api_key_stdin {
                return Err(CliFailure::invalid());
            }
            validate_security_display_name(&name)?;
            validate_security_display_name(&label)?;
            ensure_key_output_outside_data_dir(&local.data_dir, &key_out)?;
            let legacy_bearer = read_legacy_bearer_file(&legacy_bearer_file)?;
            let mut output = reserve_restricted_api_key_file(&key_out)?;
            let mut client = OfflineOwnerClient::open(&local.data_dir)?;
            let started = client.start_legacy(&name, &label, &legacy_bearer)?;
            output.write_secret(started.secret.expose_secret_bytes())?;
            let activated = client.activate_legacy(
                started.key_id,
                started.secret.expose_secret(),
                started.authorization_epoch,
                &name,
                &label,
                &legacy_bearer,
            )?;
            print_json(&json!({
                "schema": "hyphae-native-legacy-bearer-migration-v1",
                "operation": "security.legacy_bearer_migrate",
                "status": "dual_window",
                "principal_id": started.principal_id.to_string(),
                "operation_id": activated.operation_id.to_string(),
                "key_id": activated.key_id.to_string(),
                "authorization_epoch": activated.authorization_epoch.get(),
                "key_file": key_out,
                "commit": commit_json(activated.commit),
            }))
        }
        SecurityLegacyBearerCommand::Revoke { idempotency_token } => {
            let mut client = open_client(local)?;
            let response = client.dispatch_with_idempotency(
                ProductOperation::SecurityLegacyBearerRevoke,
                idempotency_token,
            )?;
            let ProductResponse::SecurityMutated(receipt) = response else {
                return Err(CliFailure::internal());
            };
            print_json(&json!({
                "schema": "hyphae-native-legacy-bearer-revocation-v1",
                "operation": "security.legacy_bearer_revoke",
                "status": "revoked",
                "authorization_epoch": receipt.authorization_epoch.get(),
                "commit": commit_json(receipt.commit),
            }))
        }
    }
}

fn security_owner(data_dir: &Path, command: SecurityOwnerCommand) -> Result<(), CliFailure> {
    match command {
        SecurityOwnerCommand::Inspect => {
            let inspection = OfflineOwnerClient::open(data_dir)?.inspect()?;
            let pending = inspection.pending.map(|pending| {
                json!({
                    "operation_id": pending.operation_id().to_string(),
                    "pending_key_id": pending.key_id().to_string(),
                    "authorization_epoch": pending.authorization_epoch().get(),
                    "created_at_micros": pending.created_at_micros(),
                    "provenance": "offline_os_owner",
                })
            });
            print_json(&json!({
                "schema": "hyphae-native-owner-recovery-v1",
                "operation": "security.owner_inspect",
                "authorization_epoch": inspection.authorization_epoch.get(),
                "pending": pending,
            }))
        }
        SecurityOwnerCommand::Recover { label, key_out } => {
            validate_security_display_name(&label)?;
            ensure_key_output_outside_data_dir(data_dir, &key_out)?;
            let mut output = reserve_restricted_api_key_file(&key_out)?;
            let mut client = OfflineOwnerClient::open(data_dir)?;
            let receipt = client.start(&label)?;
            output.write_secret(receipt.secret.expose_secret_bytes())?;
            print_json(&json!({
                "schema": "hyphae-native-owner-recovery-v1",
                "operation": "security.owner_recover",
                "status": "pending",
                "operation_id": receipt.operation_id.to_string(),
                "pending_key_id": receipt.key_id.to_string(),
                "authorization_epoch": receipt.authorization_epoch.get(),
                "key_file": key_out,
                "commit": commit_json(receipt.commit),
            }))
        }
        SecurityOwnerCommand::Resume {
            pending_key_id,
            key_file,
            expected_authorization_epoch,
        } => {
            let pending_key_id = parse_api_key_id(&pending_key_id)?;
            let expected_epoch = parse_managed_authorization_epoch(expected_authorization_epoch)?;
            let mut client = OfflineOwnerClient::open(data_dir)?;
            let receipt = client.resume(pending_key_id, &key_file, expected_epoch)?;
            print_json(&json!({
                "schema": "hyphae-native-owner-recovery-v1",
                "operation": "security.owner_resume",
                "status": "activated",
                "operation_id": receipt.operation_id.to_string(),
                "key_id": receipt.key_id.to_string(),
                "authorization_epoch": receipt.authorization_epoch.get(),
                "commit": commit_json(receipt.commit),
            }))
        }
        SecurityOwnerCommand::AbortPending {
            pending_key_id,
            expected_authorization_epoch,
        } => {
            let pending_key_id = parse_api_key_id(&pending_key_id)?;
            let expected_epoch = parse_managed_authorization_epoch(expected_authorization_epoch)?;
            let mut client = OfflineOwnerClient::open(data_dir)?;
            let receipt = client.abort(pending_key_id, expected_epoch)?;
            print_json(&json!({
                "schema": "hyphae-native-owner-recovery-v1",
                "operation": "security.owner_abort_pending",
                "status": "aborted",
                "operation_id": receipt.operation_id.to_string(),
                "pending_key_id": receipt.key_id.to_string(),
                "authorization_epoch": receipt.authorization_epoch.get(),
                "commit": commit_json(receipt.commit),
            }))
        }
    }
}

fn parse_managed_authorization_epoch(value: u64) -> Result<AuthorizationEpoch, CliFailure> {
    (value != 0)
        .then_some(AuthorizationEpoch::new(value))
        .ok_or_else(CliFailure::invalid)
}

#[allow(clippy::too_many_lines)]
fn security_key(local: &LocalDirectory, command: SecurityKeyCommand) -> Result<(), CliFailure> {
    match command {
        SecurityKeyCommand::List { cursor, limit } => security_metadata(
            local,
            SecurityListKind::Key,
            SecurityListCommand::List { cursor, limit },
        ),
        SecurityKeyCommand::Issue {
            principal_id,
            label,
            role,
            custom_role,
            permission,
            scope,
            expires_at_micros,
            self_manage,
            key_out,
            idempotency_token,
        } => {
            validate_security_display_name(&label)?;
            let principal_id = parse_security_id(&principal_id)?;
            let custom_roles = custom_role
                .iter()
                .map(|value| parse_security_id(value))
                .collect::<Result<Vec<_>, _>>()?;
            let permissions = permission
                .iter()
                .map(|value| ProductPermission::parse(value).ok_or_else(CliFailure::invalid))
                .collect::<Result<Vec<_>, _>>()?;
            let operation = if self_manage {
                ProductOperation::SecurityApiKeyIssueSelfStart {
                    principal_id,
                    label,
                    roles: role
                        .into_iter()
                        .map(AssignableBuiltInRole::product)
                        .collect(),
                    custom_roles,
                    permission_ceiling: ProductAuthorization::from_permissions(permissions),
                    scope_ceiling: scope
                        .iter()
                        .map(|value| parse_product_scope(value))
                        .collect::<Result<Vec<_>, _>>()?,
                    expires_at_micros,
                }
            } else {
                ProductOperation::SecurityApiKeyIssueStart {
                    principal_id,
                    label,
                    roles: role
                        .into_iter()
                        .map(AssignableBuiltInRole::product)
                        .collect(),
                    custom_roles,
                    permission_ceiling: ProductAuthorization::from_permissions(permissions),
                    scope_ceiling: scope
                        .iter()
                        .map(|value| parse_product_scope(value))
                        .collect::<Result<Vec<_>, _>>()?,
                    expires_at_micros,
                }
            };
            key_start_write_activate(
                local,
                operation,
                false,
                self_manage,
                &key_out,
                idempotency_token,
            )
        }
        SecurityKeyCommand::Rotate {
            predecessor_key_id,
            label,
            overlap_seconds,
            expires_at_micros,
            self_manage,
            key_out,
            idempotency_token,
        } => {
            validate_security_display_name(&label)?;
            let predecessor_key_id = parse_api_key_id(&predecessor_key_id)?;
            let operation = if self_manage {
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
            };
            key_start_write_activate(
                local,
                operation,
                true,
                self_manage,
                &key_out,
                idempotency_token,
            )
        }
        SecurityKeyCommand::Revoke {
            key_id,
            self_manage,
            idempotency_token,
        } => {
            let key_id = parse_api_key_id(&key_id)?;
            let operation = if self_manage {
                ProductOperation::SecurityApiKeyRevokeSelf { key_id }
            } else {
                ProductOperation::SecurityApiKeyRevoke { key_id }
            };
            let response = dispatch_security_mutation(local, operation, idempotency_token)?;
            print_key_mutation("security.key_revoke", key_id, &response)
        }
        SecurityKeyCommand::Abort {
            key_id,
            rotation,
            self_manage,
            idempotency_token,
        } => {
            let key_id = parse_api_key_id(&key_id)?;
            let operation = match (rotation, self_manage) {
                (false, true) => ProductOperation::SecurityApiKeyIssueSelfAbort { key_id },
                (false, false) => ProductOperation::SecurityApiKeyIssueAbort { key_id },
                (true, true) => ProductOperation::SecurityApiKeyRotateSelfAbort {
                    successor_key_id: key_id,
                },
                (true, false) => ProductOperation::SecurityApiKeyRotateAbort {
                    successor_key_id: key_id,
                },
            };
            let response = dispatch_security_mutation(local, operation, idempotency_token)?;
            print_key_mutation("security.key_abort", key_id, &response)
        }
    }
}

fn key_start_write_activate(
    local: &LocalDirectory,
    start: ProductOperation,
    rotation: bool,
    self_manage: bool,
    key_out: &Path,
    idempotency_token: u128,
) -> Result<(), CliFailure> {
    ensure_key_output_outside_data_dir(&local.data_dir, key_out)?;
    let mut output = reserve_restricted_api_key_file(key_out)?;
    let mut client = open_client(local)?;
    let response = client.dispatch_with_idempotency(start, idempotency_token)?;
    let ProductResponse::SecurityApiKeyStarted(started) = response else {
        return Err(CliFailure::internal());
    };
    let secret = started.secret.take().ok_or_else(CliFailure::internal)?;
    let confirmation_digest = secret.confirmation_digest();
    if let Err(error) = output.write_secret(secret.expose_secret_bytes()) {
        let abort_token = lifecycle_abort_token(idempotency_token);
        let abort = match (rotation, self_manage) {
            (false, true) => ProductOperation::SecurityApiKeyIssueSelfAbort {
                key_id: started.key_id,
            },
            (false, false) => ProductOperation::SecurityApiKeyIssueAbort {
                key_id: started.key_id,
            },
            (true, true) => ProductOperation::SecurityApiKeyRotateSelfAbort {
                successor_key_id: started.key_id,
            },
            (true, false) => ProductOperation::SecurityApiKeyRotateAbort {
                successor_key_id: started.key_id,
            },
        };
        match client.dispatch_with_idempotency(abort, abort_token) {
            Ok(ProductResponse::SecurityMutated(_)) => {}
            Ok(_) | Err(_) => {
                eprintln!("pending_key_id={}", started.key_id);
            }
        }
        return Err(error);
    }
    let activation_token = lifecycle_activation_token(idempotency_token);
    let activate = match (rotation, self_manage) {
        (false, true) => ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id: started.key_id,
            confirmation_digest,
        },
        (false, false) => ProductOperation::SecurityApiKeyIssueActivate {
            key_id: started.key_id,
            confirmation_digest,
        },
        (true, true) => ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id: started.key_id,
            confirmation_digest,
        },
        (true, false) => ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id: started.key_id,
            confirmation_digest,
        },
    };
    let response = client.dispatch_with_idempotency(activate, activation_token)?;
    let ProductResponse::SecurityApiKeyActivated(receipt) = response else {
        return Err(CliFailure::internal());
    };
    print_json(&json!({
        "schema": "hyphae-native-api-key-lifecycle-v1",
        "operation": if rotation { "security.key_rotate" } else { "security.key_issue" },
        "key_id": receipt.key_id.to_string(),
        "predecessor_key_id": receipt.predecessor_key_id.map(|id| id.to_string()),
        "overlap_until_micros": receipt.overlap_until_micros,
        "authorization_epoch": receipt.authorization_epoch.get(),
        "commit": commit_json(receipt.commit),
    }))
}

fn lifecycle_activation_token(start_token: u128) -> u128 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-cli-api-key-activation-idempotency-v1\0");
    hasher.update(&start_token.to_be_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    u128::from_be_bytes(bytes).max(1)
}

fn lifecycle_abort_token(start_token: u128) -> u128 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-cli-api-key-abort-idempotency-v1\0");
    hasher.update(&start_token.to_be_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    u128::from_be_bytes(bytes).max(1)
}

fn print_key_mutation(
    operation: &str,
    key_id: ApiKeyId,
    response: &ProductResponse,
) -> Result<(), CliFailure> {
    let ProductResponse::SecurityMutated(receipt) = response else {
        return Err(CliFailure::internal());
    };
    print_json(&json!({
        "schema": "hyphae-native-api-key-lifecycle-v1",
        "operation": operation,
        "key_id": key_id.to_string(),
        "authorization_epoch": receipt.authorization_epoch.get(),
        "commit": commit_json(receipt.commit),
    }))
}

fn parse_api_key_id(value: &str) -> Result<ApiKeyId, CliFailure> {
    value.parse().map_err(|_| CliFailure::invalid())
}

fn security_principal(
    local: &LocalDirectory,
    command: SecurityPrincipalCommand,
) -> Result<(), CliFailure> {
    match command {
        SecurityPrincipalCommand::List { cursor, limit } => security_metadata(
            local,
            SecurityListKind::Principal,
            SecurityListCommand::List { cursor, limit },
        ),
        SecurityPrincipalCommand::Create {
            name,
            idempotency_token,
        } => {
            validate_security_display_name(&name)?;
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityPrincipalCreate { display_name: name },
                idempotency_token,
            )?;
            let ProductResponse::SecurityPrincipalMutated(receipt) = response else {
                return Err(CliFailure::internal());
            };
            print_security_mutation(
                "security.principal_create",
                receipt.principal_id,
                receipt.authorization_epoch.get(),
                receipt.commit,
            )
        }
        SecurityPrincipalCommand::SetEnabled {
            principal_id,
            enabled,
            idempotency_token,
        } => {
            let principal_id = parse_security_id(&principal_id)?;
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityPrincipalSetEnabled {
                    principal_id,
                    enabled,
                },
                idempotency_token,
            )?;
            let ProductResponse::SecurityMutated(receipt) = response else {
                return Err(CliFailure::internal());
            };
            print_security_mutation(
                "security.principal_set_enabled",
                principal_id,
                receipt.authorization_epoch.get(),
                receipt.commit,
            )
        }
    }
}

fn security_role(local: &LocalDirectory, command: SecurityRoleCommand) -> Result<(), CliFailure> {
    match command {
        SecurityRoleCommand::List { cursor, limit } => security_metadata(
            local,
            SecurityListKind::Role,
            SecurityListCommand::List { cursor, limit },
        ),
        SecurityRoleCommand::Create {
            name,
            grant,
            idempotency_token,
        } => {
            validate_security_display_name(&name)?;
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityCustomRoleCreate {
                    display_name: name,
                    grants: parse_security_grants(&grant)?,
                },
                idempotency_token,
            )?;
            let ProductResponse::SecurityCustomRoleMutated(receipt) = response else {
                return Err(CliFailure::internal());
            };
            print_security_mutation(
                "security.custom_role_create",
                receipt.role_id,
                receipt.authorization_epoch.get(),
                receipt.commit,
            )
        }
    }
}

fn security_assignment(
    local: &LocalDirectory,
    command: SecurityAssignmentCommand,
) -> Result<(), CliFailure> {
    match command {
        SecurityAssignmentCommand::List { cursor, limit } => security_metadata(
            local,
            SecurityListKind::Assignment,
            SecurityListCommand::List { cursor, limit },
        ),
        SecurityAssignmentCommand::CreateBuiltIn {
            principal_id,
            role,
            scope,
            idempotency_token,
        } => {
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityBuiltInAssignmentCreate {
                    principal_id: parse_security_id(&principal_id)?,
                    role: role.product(),
                    scope: parse_product_scope(&scope)?,
                },
                idempotency_token,
            )?;
            print_security_assignment_receipt(&response, "security.assignment_create_built_in")
        }
        SecurityAssignmentCommand::CreateCustom {
            principal_id,
            role_id,
            idempotency_token,
        } => {
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityCustomAssignmentCreate {
                    principal_id: parse_security_id(&principal_id)?,
                    role_id: parse_security_id(&role_id)?,
                },
                idempotency_token,
            )?;
            print_security_assignment_receipt(&response, "security.assignment_create_custom")
        }
        SecurityAssignmentCommand::Revoke {
            assignment_id,
            idempotency_token,
        } => {
            let assignment_id = parse_security_id(&assignment_id)?;
            let response = dispatch_security_mutation(
                local,
                ProductOperation::SecurityAssignmentRevoke { assignment_id },
                idempotency_token,
            )?;
            let ProductResponse::SecurityMutated(receipt) = response else {
                return Err(CliFailure::internal());
            };
            print_security_mutation(
                "security.assignment_revoke",
                assignment_id,
                receipt.authorization_epoch.get(),
                receipt.commit,
            )
        }
    }
}

fn dispatch_security_mutation(
    local: &LocalDirectory,
    operation: ProductOperation,
    idempotency_token: u128,
) -> Result<ProductResponse, CliFailure> {
    open_client(local)?
        .dispatch_with_idempotency(operation, idempotency_token)
        .map_err(Into::into)
}

fn print_security_assignment_receipt(
    response: &ProductResponse,
    operation: &str,
) -> Result<(), CliFailure> {
    let ProductResponse::SecurityAssignmentMutated(receipt) = response else {
        return Err(CliFailure::internal());
    };
    print_security_mutation(
        operation,
        receipt.assignment_id,
        receipt.authorization_epoch.get(),
        receipt.commit,
    )
}

fn print_security_mutation(
    operation: &str,
    result_id: SecurityId,
    authorization_epoch: u64,
    commit: ProductCommitReceipt,
) -> Result<(), CliFailure> {
    print_json(&json!({
        "schema": "hyphae-native-security-mutation-v1",
        "operation": operation,
        "result_id": result_id.to_string(),
        "authorization_epoch": authorization_epoch,
        "commit": commit_json(commit),
    }))
}

fn parse_nonzero_idempotency_token(value: &str) -> Result<u128, String> {
    let token = value
        .parse::<u128>()
        .map_err(|_| "idempotency token must be a nonzero u128".to_owned())?;
    if token == 0 {
        return Err("idempotency token must be nonzero".to_owned());
    }
    Ok(token)
}

fn validate_security_display_name(value: &str) -> Result<(), CliFailure> {
    if value.is_empty()
        || value.len() > AccessControlLimits::V1.display_name_bytes
        || value.chars().any(char::is_control)
    {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn parse_security_id(value: &str) -> Result<SecurityId, CliFailure> {
    value.parse().map_err(|_| CliFailure::invalid())
}

fn parse_security_grants(values: &[String]) -> Result<Vec<CustomRoleGrant>, CliFailure> {
    if values.is_empty() || values.len() > AccessControlLimits::V1.grants_per_role {
        return Err(CliFailure::invalid());
    }
    let mut grants = BTreeSet::new();
    for value in values {
        if value.len() > MAX_SECURITY_GRANT_BYTES {
            return Err(CliFailure::invalid());
        }
        let Some((permission, scope)) = value.split_once('@') else {
            return Err(CliFailure::invalid());
        };
        if scope.contains('@') {
            return Err(CliFailure::invalid());
        }
        let permission = ProductPermission::parse(permission).ok_or_else(CliFailure::invalid)?;
        let grant = CustomRoleGrant::new(permission, parse_product_scope(scope)?)
            .ok_or_else(CliFailure::invalid)?;
        if !grants.insert(grant) {
            return Err(CliFailure::invalid());
        }
    }
    Ok(grants.into_iter().collect())
}

fn parse_product_scope(value: &str) -> Result<ProductScope, CliFailure> {
    if value == "instance" {
        return Ok(ProductScope::Instance);
    }
    if let Some(object) = value.strip_prefix("catalog_subtree:") {
        return parse_scope_object(object).map(ProductScope::CatalogSubtree);
    }
    if let Some(object) = value.strip_prefix("catalog_object:") {
        return parse_scope_object(object).map(ProductScope::CatalogObject);
    }
    Err(CliFailure::invalid())
}

fn parse_scope_object(value: &str) -> Result<ObjectId, CliFailure> {
    if value.is_empty()
        || value.len() > MAX_DECIMAL_OBJECT_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(CliFailure::invalid());
    }
    let parsed = value.parse::<u128>().map_err(|_| CliFailure::invalid())?;
    if parsed.to_string() != value {
        return Err(CliFailure::invalid());
    }
    object_id(parsed)
}

const MAX_DECIMAL_OBJECT_ID_BYTES: usize = 39;
const MAX_SECURITY_GRANT_BYTES: usize = 128;

#[derive(Clone, Copy)]
enum SecurityListKind {
    Principal,
    Role,
    Assignment,
    Key,
}

impl SecurityListKind {
    fn operation(
        self,
        cursor: Option<SecurityCursor>,
        limit: usize,
    ) -> Result<ProductOperation, CliFailure> {
        let operation = match self {
            Self::Principal => ProductOperation::SecurityPrincipalList(
                SecurityPrincipalListRequest::new(cursor, limit)
                    .map_err(|_| CliFailure::invalid())?,
            ),
            Self::Role => ProductOperation::SecurityRoleList(
                SecurityRoleListRequest::new(cursor, limit).map_err(|_| CliFailure::invalid())?,
            ),
            Self::Assignment => ProductOperation::SecurityAssignmentList(
                SecurityAssignmentListRequest::new(cursor, limit)
                    .map_err(|_| CliFailure::invalid())?,
            ),
            Self::Key => ProductOperation::SecurityKeyList(
                SecurityKeyListRequest::new(cursor, limit).map_err(|_| CliFailure::invalid())?,
            ),
        };
        Ok(operation)
    }
}

const MAX_SECURITY_CURSOR_BYTES: usize = 128;

fn security_metadata(
    local: &LocalDirectory,
    kind: SecurityListKind,
    command: SecurityListCommand,
) -> Result<(), CliFailure> {
    let SecurityListCommand::List { cursor, limit } = command;
    let cursor = parse_security_cursor(cursor.as_deref())?;
    let mut client = open_client(local)?;
    let response = client.dispatch(kind.operation(cursor, limit)?)?;
    print_json(&response_json(response))
}

fn parse_security_cursor(requested: Option<&str>) -> Result<Option<SecurityCursor>, CliFailure> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    if requested.len() > MAX_SECURITY_CURSOR_BYTES {
        return Err(CliFailure::invalid());
    }
    SecurityCursor::from_token(requested)
        .map(Some)
        .map_err(|_| CliFailure::invalid())
}

fn security_audit(local: &LocalDirectory, command: SecurityListCommand) -> Result<(), CliFailure> {
    let SecurityListCommand::List { cursor, limit } = command;
    let cursor = cursor
        .map(|value| {
            value
                .parse::<SecurityId>()
                .map_err(|_| CliFailure::invalid())
        })
        .transpose()?;
    let request =
        SecurityAuditReadRequest::new(cursor, limit).map_err(|_| CliFailure::invalid())?;
    dispatch(local, ProductOperation::SecurityAuditRead(request))
}

fn doctor(local: &LocalDirectory) -> Result<(), CliFailure> {
    let request = DoctorRequest::new(&local.data_dir, native::logical_time_micros())
        .map_err(|_| CliFailure::invalid())?;
    let product = match NativeProduct::open(&local.data_dir) {
        Ok(product) => product,
        Err(open_error)
            if matches!(
                open_error.code(),
                hyphae_native_product::ProductErrorCode::DataDirectoryLocked
                    | hyphae_native_product::ProductErrorCode::InvalidDataDirectory
                    | hyphae_native_product::ProductErrorCode::Io
            ) =>
        {
            return print_json(&doctor_json(hyphae_native_product::doctor(&request)));
        }
        Err(open_error) => return Err(open_error.into()),
    };
    let mut client = EmbeddedClient::open(
        product,
        local.native_api_key_file.as_deref(),
        local.native_api_key_stdin,
    )?;
    let response = client.dispatch(ProductOperation::Doctor(request))?;
    print_json(&response_json(response))
}

fn compact(local: &LocalDirectory, target: CompactTarget) -> Result<(), CliFailure> {
    let mut client = open_client(local)?;
    let receipt = client
        .unmanaged_product_mut()?
        .administration()
        .compact(CompactionRequest {
            target: target.into(),
            durability: ProductDurability::Strict,
        })?;
    print_json(&json!({
        "status": if receipt.commit.is_some() { "compacted" } else { "no_changes" },
        "target": compaction_target(receipt.target),
        "scanned_entries": receipt.scanned_entries,
        "retained_entries": receipt.retained_entries,
        "dropped_tombstones": receipt.dropped_tombstones,
        "reachable_pages_before": receipt.reachable_pages_before,
        "reachable_pages_after": receipt.reachable_pages_after,
        "pages_appended": receipt.pages_appended,
        "commit": receipt.commit.map(commit_json),
    }))
}

fn vacuum(local: &LocalDirectory) -> Result<(), CliFailure> {
    let mut client = open_client(local)?;
    let receipt = client
        .unmanaged_product_mut()?
        .administration()
        .vacuum_pages()?;
    print_json(&json!({
        "status": if receipt.applied { "vacuumed" } else { "no_changes" },
        "previous_generation": receipt.previous_generation,
        "active_generation": receipt.active_generation,
        "previous_page_count": receipt.previous_page_count,
        "active_page_count": receipt.active_page_count,
        "reclaimed_pages": receipt.reclaimed_pages,
        "commit": receipt.commit.map(commit_json),
    }))
}

fn backup(command: BackupCommand) -> Result<(), CliFailure> {
    match command {
        BackupCommand::Create { local, out } => {
            let request = BackupRequest::new(out).map_err(|_| CliFailure::invalid())?;
            dispatch(&local, ProductOperation::Backup(request))
        }
        BackupCommand::Verify { backup } => {
            let request = VerifyBackupRequest::new(backup).map_err(|_| CliFailure::invalid())?;
            let info = verify_backup(&request, |_| ProgressControl::Continue)?;
            print_json(&backup_json("verified", &info))
        }
    }
}

fn restore(backup: &Path, data_dir: &Path) -> Result<(), CliFailure> {
    let request = RestoreRequest::new(backup, data_dir).map_err(|_| CliFailure::invalid())?;
    let restored = hyphae_native_product::restore(&request, |_| ProgressControl::Continue)?;
    print_json(&json!({
        "status": "restored",
        "data_path": restored.data_path,
        "backup": backup_json("verified", &restored.backup),
        "doctor": {
            "status": doctor_status(restored.doctor.status),
            "verified_open": restored.doctor.verified_open,
            "snapshot_verified": restored.doctor.snapshot_verified,
        },
    }))
}

fn proof(command: ProofCommand) -> Result<(), CliFailure> {
    match command {
        ProofCommand::Generate {
            local,
            operation_json,
            proof_out,
            witness_out,
        } => {
            if proof_out == witness_out {
                return Err(CliFailure::invalid());
            }
            let operation = parse_proof_operation(&operation_json)?;
            let response = open_client(&local)?.dispatch(ProductOperation::Prove {
                operation: Box::new(operation),
                limits: NativeProofGenerationLimits::default(),
            })?;
            let ProductResponse::Proven { response, artifact } = response else {
                return Err(CliFailure::internal());
            };
            write_new_file(&proof_out, &artifact.proof_bytes)?;
            if let Err(error) = write_new_file(&witness_out, &artifact.witness_bytes) {
                let _ignored = fs::remove_file(&proof_out);
                return Err(error);
            }
            print_json(&json!({
                "status": "generated",
                "response": response_json(*response),
                "kind": proof_kind(artifact.proof.content().kind),
                "proof_path": proof_out,
                "witness_path": witness_out,
                "anchor": encode_hex(&artifact.trusted_anchor.digest()),
                "proof_bytes": artifact.proof_bytes.len(),
                "witness_bytes": artifact.witness_bytes.len(),
            }))
        }
        ProofCommand::Verify {
            proof,
            witness,
            anchor,
        } => {
            let proof = fs::read(proof)?;
            let witness = fs::read(witness)?;
            let anchor = decode_hex::<32>(&anchor)?;
            let report = verify_native_proof_offline(
                &proof,
                &witness,
                ExternalTrustedAnchor::new(anchor),
                &NativeVerificationLimits::default(),
            )?;
            let scope = if report.semantic_reexecution_performed {
                "semantic_reexecution"
            } else {
                "artifact_integrity"
            };
            print_json(&json!({
                "status": "verified",
                "scope": scope,
                "kind": proof_kind(report.kind),
                "anchor_digest": encode_hex(&report.anchor_digest),
                "proof_digest": encode_hex(&report.proof_digest),
                "witness_digest": encode_hex(&report.witness_digest),
                "request_digest": encode_hex(&report.request_digest),
                "result_digest": encode_hex(&report.result_digest),
                "evidence_digest": encode_hex(&report.evidence_digest),
                "file_count": report.file_count,
                "directory_count": report.directory_count,
                "total_file_bytes": report.total_file_bytes,
                "semantic_reexecution_performed": report.semantic_reexecution_performed,
            }))
        }
    }
}

fn proof_operation(input: ProofOperationInput) -> Result<ProductOperation, CliFailure> {
    Ok(match input {
        ProofOperationInput::CatalogList { limit } => {
            ProductOperation::CatalogList(CatalogListRequest {
                parent: None,
                kind: None,
                cursor: None,
                item_limit: limit,
                visit_limit: 1_000,
                byte_limit: 1_048_576,
            })
        }
        ProofOperationInput::CatalogDescribe { id } => ProductOperation::CatalogDescribe {
            id: object_id(id.0)?,
        },
        ProofOperationInput::Sql {
            statement,
            parameters,
        } => ProductOperation::ExecuteSql {
            statement,
            parameters: parameters
                .into_iter()
                .map(product_value)
                .collect::<Result<_, _>>()?,
        },
    })
}

/// Parses one proof operation document, admitting the search-collection
/// shape next to the tagged catalog and SQL shapes.
fn parse_proof_operation(operation_json: &str) -> Result<ProductOperation, CliFailure> {
    let value: Value = serde_json::from_str(operation_json)?;
    if value.get("operation").and_then(Value::as_str) == Some("search_collection") {
        let Value::Object(mut object) = value else {
            return Err(CliFailure::invalid());
        };
        object.remove("operation");
        let input: mcp::CollectionSearchInput =
            serde_json::from_value(Value::Object(object)).map_err(|_| CliFailure::invalid())?;
        let collection = object_id(u128::from(input.collection))?;
        return Ok(ProductOperation::SearchCollection {
            collection,
            request: mcp::collection_search_request(input)
                .map_err(|error| CliFailure::from(*error))?,
        });
    }
    proof_operation(serde_json::from_value(value)?)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), CliFailure> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ignored = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn response_json(response: ProductResponse) -> Value {
    match response {
        ProductResponse::Capabilities(value) => json!({
            "product_api_version": value.product_api_version,
            "native_directory_format": value.native_directory_format,
            "logical_catalog_codec_version": value.logical_catalog_codec_version,
            "catalog_tree_format_version": value.catalog_tree_format_version,
            "limits": {
                "catalog_items": value.max_catalog_items,
                "catalog_visits": value.max_catalog_visits,
                "catalog_bytes": value.max_catalog_bytes,
                "sql_statement_bytes": value.max_sql_statement_bytes,
                "sql_parameters": value.max_sql_parameters,
                "sql_rows": value.max_sql_rows,
            }
        }),
        ProductResponse::CatalogObject(read) => json!({
            "snapshot": snapshot_json(read.snapshot),
            "object": {
                "id": read.value.header().id.get().to_string(),
                "kind": catalog_kind(read.value.kind()),
                "name": read.value.header().name.to_string(),
            }
        }),
        ProductResponse::CatalogPage(page) => json!({
            "snapshot": snapshot_json(page.snapshot),
            "items": page.items.into_iter().map(|item| json!({
                "id": item.id.get().to_string(),
                "kind": catalog_kind(item.kind),
                "name": item.name.to_string(),
                "parent": item.parent.map(|value| value.get().to_string()),
            })).collect::<Vec<_>>(),
            "cursor": page.cursor.map(|cursor| cursor.after().get().to_string()),
            "stop": catalog_page_stop(page.stop),
            "visited": page.visited,
            "returned_bytes": page.returned_bytes,
        }),
        ProductResponse::CatalogVisiblePage(page) => json!({
            "items": page.items.into_iter().map(|item| json!({
                "id": item.id.get().to_string(),
                "kind": catalog_kind(item.kind),
                "name": item.name.to_string(),
                "parent": item.parent.map(|parent| parent.get().to_string()),
            })).collect::<Vec<_>>(),
            "cursor": page.cursor.map(|cursor| encode_catalog_cursor_token(cursor.as_bytes())),
        }),
        ProductResponse::CatalogDefinition(definition) => json!({
            "found": definition.is_some(),
            "object": definition.map(|object| json!({
                "id": object.id().get().to_string(),
                "kind": catalog_kind(object.kind()),
                "name": object.name().to_string(),
                "parent": object.parent().map(|value| value.get().to_string()),
                "definition_version": object.definition_version().get(),
            })),
        }),
        ProductResponse::Sql {
            result,
            snapshot,
            commit,
        } => json!({
            "result": sql_result_json(result),
            "snapshot": snapshot.map(snapshot_json),
            "commit": commit.map(commit_outcome_json),
        }),
        ProductResponse::StructureValue(value) => json!({
            "found": value.is_some(),
            "value": value.as_ref().and_then(|bytes| std::str::from_utf8(bytes).ok()),
            "value_hex": value.map(|bytes| encode_hex(&bytes)),
        }),
        ProductResponse::StructureSet(outcome)
        | ProductResponse::StructureMutated(outcome)
        | ProductResponse::CatalogCreated(outcome) => commit_outcome_json(outcome),
        ProductResponse::StructureTtl(ttl) => ttl_json(ttl),
        ProductResponse::StructureRead(read) => json!({
            "snapshot": snapshot_json(read.snapshot),
            "result": structure_read_json(read.value),
        }),
        ProductResponse::ExplicitTransactionStatus(status) => explicit_transaction_json(status),
        ProductResponse::TransactionStaged(receipt) => json!({
            "status": "staged",
            "handle": receipt.handle.get(),
            "operation_ordinal": receipt.operation_ordinal,
            "changed": receipt.changed,
            "result": transaction_stage_result_json(receipt.result),
        }),
        ProductResponse::TransactionCommitted(receipt) => json!({
            "status": "committed",
            "handle": receipt.handle.get(),
            "staged_operations": receipt.staged_operations,
            "commit": commit_json(receipt.commit),
        }),
        ProductResponse::TransactionRolledBack(receipt) => json!({
            "status": "rolled_back",
            "handle": receipt.handle.get(),
            "discarded_operations": receipt.discarded_operations,
        }),
        ProductResponse::TransactionStatus(status) => transaction_status_json(status),
        ProductResponse::Search(results) => search_result_json(results),
        ProductResponse::AdminStatus(status) => json!({
            "status": "ready",
            "snapshot": snapshot_json(status.snapshot),
            "snapshot_pin_count": status.snapshot_pin_count,
            "physical": {
                "page_count": status.physical.page_count,
                "physical_page_reads": status.physical.physical_page_reads,
                "wal_bytes": status.physical.wal_bytes,
                "process_full_state_loads": status.physical.process_full_state_loads,
                "process_full_catalog_loads": status.physical.process_full_catalog_loads,
            },
            "retained_wal_bytes": status.retained_wal_bytes,
            "replayed_transactions": status.replayed_transactions,
            "manifest_count": status.manifest_count,
            "blob_count": status.blob_count,
        }),
        ProductResponse::AdminCheckpoint(receipt) => json!({
            "status": "checkpointed",
            "transaction_id": receipt.transaction_id.to_string(),
            "visible_csn": receipt.visible_csn,
            "manifest_generation": receipt.manifest_generation,
            "manifest_digest": encode_hex(&receipt.manifest_digest),
            "checkpoint_lsn": receipt.checkpoint_lsn,
            "parent_directory_sync_supported": receipt.parent_directory_sync_supported,
        }),
        ProductResponse::Explain(explanation) => explain_json(explanation),
        ProductResponse::Backup(info) => backup_json("created", &info),
        ProductResponse::Doctor(report) => json!({
            "status": doctor_status(report.status),
            "verified_open": report.verified_open,
            "snapshot_verified": report.snapshot_verified,
        }),
        ProductResponse::SecurityStatus(status) => security_status_json(status),
        ProductResponse::SecurityPrincipalPage(page) => security_principal_page_json(page),
        ProductResponse::SecurityRolePage(page) => security_role_page_json(&page),
        ProductResponse::SecurityAssignmentPage(page) => security_assignment_page_json(page),
        ProductResponse::SecurityKeyPage(page) => security_key_page_json(page),
        ProductResponse::SecurityAuditPage(page) => security_audit_page_json(page),
        ProductResponse::PreparedSql {
            handle,
            catalog_version,
            parameter_count,
            maximum_result_rows,
        } => json!({
            "handle": handle.get(),
            "catalog_version": catalog_version.get(),
            "parameter_count": parameter_count,
            "maximum_result_rows": maximum_result_rows,
        }),
        ProductResponse::Deallocated => json!({ "status": "deallocated" }),
        ProductResponse::CatalogDependencyPage(page) => json!({
            "snapshot": snapshot_json(page.snapshot),
            "items": page.items.into_iter().map(|edge| json!({
                "dependent": edge.dependent.get().to_string(),
                "prerequisite": edge.prerequisite.get().to_string(),
                "kind": dependency_kind(edge.kind),
            })).collect::<Vec<_>>(),
            "cursor": page.cursor.map(|cursor| cursor.after().get().to_string()),
            "stop": catalog_page_stop(page.stop),
            "visited": page.visited,
            "returned_bytes": page.returned_bytes,
        }),
        ProductResponse::Telemetry(snapshot) => json!({
            "registry_version": snapshot.registry_version,
            "process_start_identity": snapshot.process_start_identity.to_string(),
            "session_start_identity": snapshot.session_start_identity.to_string(),
            "captured_at_micros": snapshot.captured_at_micros,
            "catalog_version": snapshot.catalog_version.map(hyphae_native_product::CatalogVersion::get),
            "dropped_events": snapshot.dropped_events,
            "metrics": snapshot.metrics.into_iter().map(|row| json!({
                "name": row.descriptor.name,
                "value": metric_kind(row.value),
            })).collect::<Vec<_>>(),
        }),
        ProductResponse::ProofVerification(report) => json!({
            "status": "verified",
            "scope": "artifact_integrity",
            "kind": proof_kind(report.kind),
            "anchor_digest": encode_hex(&report.anchor_digest),
            "proof_digest": encode_hex(&report.proof_digest),
            "witness_digest": encode_hex(&report.witness_digest),
            "semantic_reexecution_performed": report.semantic_reexecution_performed,
        }),
        ProductResponse::IntegratedSearch(result) => json!({
            "snapshot": snapshot_json(result.snapshot),
            "hits": result.hits.into_iter().map(|hit| json!({
                "object_id": hit.object_id.get().to_string(),
                "score": hit.score,
                "doc_values": hit.doc_values.into_iter().map(|(name, value)| (name, doc_value_json(value))).collect::<serde_json::Map<_, _>>(),
                "fragments": hit.fragments,
            })).collect::<Vec<_>>(),
            "facets": result.facets.into_iter().map(|facet| json!({
                "field": facet.field,
                "buckets": facet.buckets.into_iter().map(|bucket| json!({
                    "value": doc_value_json(bucket.value),
                    "count": bucket.count,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "aggregations": result.aggregations.into_iter().map(|aggregation| json!({
                "name": aggregation.name,
                "value": aggregation_value_json(aggregation.value),
            })).collect::<Vec<_>>(),
            "vector_branches": result.vector_branches.into_iter().map(|branch| json!({
                "target": branch.target,
                "strategy": vector_strategy(branch.strategy),
                "approximate": branch.approximate,
                "eligible_documents": branch.eligible_documents,
                "candidate_count": branch.candidate_count,
                "visited_nodes": branch.visited_nodes,
                "exact_reranked": branch.exact_reranked,
            })).collect::<Vec<_>>(),
            "approximate": result.approximate,
            "total_documents": result.total_documents,
            "eligible_documents": result.eligible_documents,
            "lexical_candidates": result.lexical_candidates,
            "retrieval_candidates": result.retrieval_candidates,
            "matched_candidates": result.matched_candidates,
        }),
        ProductResponse::SearchIngested(receipt) => json!({
            "status": if receipt.idempotent_replay { "existing" } else { "committed" },
            "snapshot": snapshot_json(receipt.snapshot),
            "commit": receipt.commit.map(commit_json),
            "documents": receipt.documents,
            "idempotent_replay": receipt.idempotent_replay,
        }),
        ProductResponse::Restore(restored) => json!({
            "status": "restored",
            "data_path": restored.data_path,
            "backup": backup_json("verified", &restored.backup),
            "doctor": doctor_json(restored.doctor),
            "phases": restored.phases.into_iter().map(restore_phase).collect::<Vec<_>>(),
        }),
        ProductResponse::Proven { response, artifact } => json!({
            "response": response_json(*response),
            "proof": {
                "anchor_digest": encode_hex(&artifact.trusted_anchor.digest()),
                "proof_bytes": artifact.proof_bytes.len(),
                "witness_bytes": artifact.witness_bytes.len(),
            },
        }),
        _ => json!({ "status": "ok" }),
    }
}

fn security_status_json(status: AccessControlStatus) -> Value {
    json!({
        "schema": "hyphae-native-access-control-status-v1",
        "bootstrapped": status.bootstrapped,
        "authorization_epoch": status.epoch.get(),
        "principals": status.principals,
        "assignments": status.assignments,
        "custom_roles": status.custom_roles,
        "custom_assignments": status.custom_assignments,
        "keys": status.keys,
        "pending_keys": status.pending_keys,
        "audit_events": status.audit_events,
    })
}

fn security_principal_page_json(page: SecurityPrincipalPage) -> Value {
    json!({
        "schema": "hyphae-native-security-principals-v1",
        "authorization_epoch": page.authorization_epoch.get(),
        "items": page.items.into_vec().into_iter().map(|principal| json!({
            "id": principal.id().to_string(),
            "display_name": principal.display_name(),
            "enabled": principal.enabled(),
        })).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor.map(SecurityCursor::to_token),
    })
}

fn security_role_page_json(page: &SecurityRolePage) -> Value {
    json!({
        "schema": "hyphae-native-security-roles-v1",
        "authorization_epoch": page.authorization_epoch.get(),
        "items": page.items.iter().map(security_role_json).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor.map(SecurityCursor::to_token),
    })
}

fn security_role_json(role: &SecurityRoleSummary) -> Value {
    if let Some(built_in) = role.built_in_role() {
        json!({
            "kind": "built_in",
            "id": built_in.as_str(),
            "display_name": role.display_name(),
            "permissions": authorization_permissions(built_in.authorization()),
            "grants": [],
        })
    } else {
        json!({
            "kind": "custom",
            "id": role.custom_role_id().map(|id| id.to_string()),
            "display_name": role.display_name(),
            "permissions": [],
            "grants": role.grants().iter().copied().map(|grant| json!({
                "permission": grant.permission().as_str(),
                "scope": security_scope_json(grant.scope()),
            })).collect::<Vec<_>>(),
        })
    }
}

fn security_assignment_page_json(page: SecurityAssignmentPage) -> Value {
    json!({
        "schema": "hyphae-native-security-assignments-v1",
        "authorization_epoch": page.authorization_epoch.get(),
        "items": page.items.into_vec().into_iter().map(|assignment| json!({
            "id": assignment.id().to_string(),
            "principal_id": assignment.principal_id().to_string(),
            "built_in_role": assignment.built_in_role().map(BuiltInRole::as_str),
            "custom_role_id": assignment.custom_role_id().map(|id| id.to_string()),
            "scope": assignment.scope().map(security_scope_json),
        })).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor.map(SecurityCursor::to_token),
    })
}

fn security_key_page_json(page: SecurityKeyPage) -> Value {
    json!({
        "schema": "hyphae-native-security-keys-v1",
        "authorization_epoch": page.authorization_epoch.get(),
        "items": page.items.into_vec().into_iter().map(|key| json!({
            "id": key.id().to_string(),
            "principal_id": key.principal_id().to_string(),
            "label": key.label(),
            "active": key.active(),
            "roles": key.roles().iter().map(|role| role.as_str()).collect::<Vec<_>>(),
            "custom_roles": key.custom_roles().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "permission_ceiling": authorization_permissions(key.permission_ceiling()),
            "scope_ceiling": key.scope_ceiling().iter().copied().map(security_scope_json).collect::<Vec<_>>(),
            "created_at_micros": key.created_at_micros(),
            "expires_at_micros": key.expires_at_micros(),
            "revoked": key.revoked(),
            "published_epoch": key.published_epoch().get(),
            "predecessor_id": key.predecessor_id().map(|id| id.to_string()),
            "successor_id": key.successor_id().map(|id| id.to_string()),
            "overlap_until_micros": key.overlap_until_micros(),
            "rotation_overlap_micros": key.rotation_overlap_micros(),
        })).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor.map(SecurityCursor::to_token),
    })
}

fn security_audit_page_json(page: SecurityAuditPage) -> Value {
    json!({
        "schema": "hyphae-native-security-audit-v1",
        "items": page.events.into_vec().into_iter().map(|event| json!({
            "id": event.id().to_string(),
            "commit_csn": event.commit_csn(),
            "actor_principal_id": event.actor_principal_id().map(|id| id.to_string()),
            "actor_key_id": event.actor_key_id().map(|id| id.to_string()),
            "action": security_audit_action(event.action()),
            "result": security_audit_result(event.result()),
            "targets": event.targets().iter().copied().map(security_audit_target_json).collect::<Vec<_>>(),
            "metadata": event.metadata().iter().copied().map(security_audit_metadata_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor.map(|cursor| cursor.to_string()),
    })
}

fn authorization_permissions(authorization: ProductAuthorization) -> Vec<&'static str> {
    (0_u8..=u8::MAX)
        .filter_map(ProductPermission::from_tag)
        .filter(|permission| authorization.allows(*permission))
        .map(ProductPermission::as_str)
        .collect()
}

fn security_scope_json(scope: ProductScope) -> Value {
    match scope {
        ProductScope::Instance => json!({ "kind": "instance" }),
        ProductScope::CatalogSubtree(object) => json!({
            "kind": "catalog_subtree",
            "object_id": object.get().to_string(),
        }),
        ProductScope::CatalogObject(object) => json!({
            "kind": "catalog_object",
            "object_id": object.get().to_string(),
        }),
    }
}

const fn security_audit_action(action: SecurityAuditAction) -> &'static str {
    match action {
        SecurityAuditAction::BootstrapOwner => "bootstrap_owner",
        SecurityAuditAction::ActivateKey => "activate_key",
        SecurityAuditAction::CreatePrincipal => "create_principal",
        SecurityAuditAction::CreateCustomRole => "create_custom_role",
        SecurityAuditAction::AssignBuiltInRole => "assign_built_in_role",
        SecurityAuditAction::AssignCustomRole => "assign_custom_role",
        SecurityAuditAction::IssueKey => "issue_key",
        SecurityAuditAction::RotateKey => "rotate_key",
        SecurityAuditAction::AbortKeyRotation => "abort_key_rotation",
        SecurityAuditAction::AbortKeyIssue => "abort_key_issue",
        SecurityAuditAction::RevokeKey => "revoke_key",
        SecurityAuditAction::RecoverOwner => "recover_owner",
        SecurityAuditAction::MigrateLegacyBearer => "migrate_legacy_bearer",
        SecurityAuditAction::SetPrincipalEnabled => "set_principal_enabled",
        SecurityAuditAction::RevokeAssignment => "revoke_assignment",
        SecurityAuditAction::RevokeLegacyBearer => "revoke_legacy_bearer",
    }
}

const fn security_audit_result(result: SecurityAuditResult) -> &'static str {
    match result {
        SecurityAuditResult::Succeeded => "succeeded",
    }
}

fn security_audit_target_json(target: SecurityAuditTarget) -> Value {
    match target {
        SecurityAuditTarget::Principal(id) => {
            json!({ "kind": "principal", "id": id.to_string() })
        }
        SecurityAuditTarget::Role(id) => json!({ "kind": "role", "id": id.to_string() }),
        SecurityAuditTarget::Assignment(id) => {
            json!({ "kind": "assignment", "id": id.to_string() })
        }
        SecurityAuditTarget::Key(id) => json!({ "kind": "key", "id": id.to_string() }),
        SecurityAuditTarget::LegacyBearer => json!({ "kind": "legacy_bearer" }),
    }
}

fn security_audit_metadata_json(metadata: SecurityAuditMetadata) -> Value {
    match metadata {
        SecurityAuditMetadata::ExpiresAtMicros(value) => {
            json!({ "kind": "expires_at_micros", "value": value })
        }
        SecurityAuditMetadata::RotationOverlapUntilMicros(value) => {
            json!({ "kind": "rotation_overlap_until_micros", "value": value })
        }
    }
}

fn structure_read_json(result: ProductStructureReadResult) -> Value {
    match result {
        ProductStructureReadResult::Value(value) => json!({
            "type": "value",
            "found": value.is_some(),
            "value": value.as_ref().and_then(|bytes| std::str::from_utf8(bytes).ok()),
            "value_hex": value.map(|bytes| encode_hex(&bytes)),
        }),
        ProductStructureReadResult::Counter(value) => {
            json!({ "type": "counter", "value": value })
        }
        ProductStructureReadResult::Ttl(value) => {
            json!({ "type": "ttl", "value": ttl_json(value) })
        }
        ProductStructureReadResult::HashEntries(entries) => json!({
            "type": "hash_entries",
            "entries": entries.into_iter().map(|entry| json!({
                "field_hex": encode_hex(&entry.field),
                "field": std::str::from_utf8(&entry.field).ok(),
                "value_hex": encode_hex(&entry.value),
                "value": std::str::from_utf8(&entry.value).ok(),
            })).collect::<Vec<_>>(),
        }),
        ProductStructureReadResult::Count(value) => json!({ "type": "count", "value": value }),
        ProductStructureReadResult::Boolean(value) => {
            json!({ "type": "boolean", "value": value })
        }
        ProductStructureReadResult::Values(values) => json!({
            "type": "values",
            "values": values.into_iter().map(|value| json!({
                "value_hex": encode_hex(&value),
                "value": std::str::from_utf8(&value).ok(),
            })).collect::<Vec<_>>(),
        }),
        ProductStructureReadResult::SetAlgebra { members, visited } => json!({
            "type": "set_algebra",
            "members": members.into_iter().map(|value| json!({
                "value_hex": encode_hex(&value),
                "value": std::str::from_utf8(&value).ok(),
            })).collect::<Vec<_>>(),
            "visited": visited,
        }),
        ProductStructureReadResult::SortedSetScore(value) => json!({
            "type": "sorted_set_score",
            "value": value.map(hyphae_native_product::CanonicalF64::get),
        }),
        ProductStructureReadResult::SortedSetRank(value) => {
            json!({ "type": "sorted_set_rank", "value": value })
        }
        ProductStructureReadResult::SortedSetEntries(entries) => json!({
            "type": "sorted_set_entries",
            "entries": entries.into_iter().map(|entry| json!({
                "member_hex": encode_hex(&entry.member),
                "member": std::str::from_utf8(&entry.member).ok(),
                "score": entry.score.get(),
            })).collect::<Vec<_>>(),
        }),
        ProductStructureReadResult::StreamEntries(entries) => json!({
            "type": "stream_entries",
            "entries": entries.into_iter().map(|entry| json!({
                "id": entry.id,
                "fields": entry.fields.into_iter().map(|field| json!({
                    "field_hex": encode_hex(&field.field),
                    "field": std::str::from_utf8(&field.field).ok(),
                    "value_hex": encode_hex(&field.value),
                    "value": std::str::from_utf8(&field.value).ok(),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        ProductStructureReadResult::HashPage {
            entries,
            continuation,
            stop,
            visited,
            match_steps,
        } => json!({
            "type": "hash_page",
            "entries": entries.into_iter().map(|entry| json!({
                "field_hex": encode_hex(&entry.field),
                "field": std::str::from_utf8(&entry.field).ok(),
                "value_hex": encode_hex(&entry.value),
                "value": std::str::from_utf8(&entry.value).ok(),
            })).collect::<Vec<_>>(),
            "continuation_hex": continuation.map(|cursor| encode_hex(&cursor)),
            "stop": match stop {
                hyphae_native_product::ProductHashScanStop::Exhausted => "exhausted",
                hyphae_native_product::ProductHashScanStop::OutputLimit => "output_limit",
                hyphae_native_product::ProductHashScanStop::VisitLimit => "visit_limit",
                _ => "unknown",
            },
            "visited": visited,
            "match_steps": match_steps,
        }),
        result @ ProductStructureReadResult::KeyPage { .. } => key_page_json(result),
        _ => json!({ "type": "unsupported" }),
    }
}

fn key_page_json(result: ProductStructureReadResult) -> Value {
    let ProductStructureReadResult::KeyPage {
        entries,
        continuation,
        stop,
        visited,
        match_steps,
    } = result
    else {
        return json!({ "type": "unsupported" });
    };
    json!({
        "type": "key_page",
        "entries": entries.into_iter().map(|entry| json!({
            "key_hex": encode_hex(&entry.key),
            "key": std::str::from_utf8(&entry.key).ok(),
            "family": structure_family_name(entry.family),
        })).collect::<Vec<_>>(),
        "continuation_hex": continuation.map(|cursor| encode_hex(&cursor)),
        "stop": match stop {
            hyphae_native_product::ProductHashScanStop::Exhausted => "exhausted",
            hyphae_native_product::ProductHashScanStop::OutputLimit => "output_limit",
            hyphae_native_product::ProductHashScanStop::VisitLimit => "visit_limit",
            _ => "unknown",
        },
        "visited": visited,
        "match_steps": match_steps,
    })
}

const fn structure_family_name(family: hyphae_native_product::StructureKind) -> &'static str {
    match family {
        hyphae_native_product::StructureKind::String => "string",
        hyphae_native_product::StructureKind::Counter => "counter",
        hyphae_native_product::StructureKind::Hash => "hash",
        hyphae_native_product::StructureKind::List => "list",
        hyphae_native_product::StructureKind::Set => "set",
        hyphae_native_product::StructureKind::SortedSet => "sorted_set",
        hyphae_native_product::StructureKind::Stream => "stream",
    }
}

fn explicit_transaction_json(status: ProductExplicitTransactionStatus) -> Value {
    match status {
        ProductExplicitTransactionStatus::Unknown => json!({ "status": "unknown" }),
        ProductExplicitTransactionStatus::Active {
            handle,
            read_csn,
            staged_operations,
            durability: value,
        } => json!({
            "status": "active",
            "handle": handle.get(),
            "read_csn": read_csn,
            "staged_operations": staged_operations,
            "durability": durability(value),
        }),
        ProductExplicitTransactionStatus::Committed {
            handle,
            staged_operations,
            receipt,
        } => json!({
            "status": "committed",
            "handle": handle.get(),
            "staged_operations": staged_operations,
            "commit": commit_json(receipt),
        }),
        ProductExplicitTransactionStatus::RolledBack {
            handle,
            discarded_operations,
        } => json!({
            "status": "rolled_back",
            "handle": handle.get(),
            "discarded_operations": discarded_operations,
        }),
        ProductExplicitTransactionStatus::OutcomeUnknown {
            handle,
            transaction_id,
            staged_operations,
        } => json!({
            "status": "outcome_unknown",
            "handle": handle.get(),
            "transaction_id": transaction_id.to_string(),
            "staged_operations": staged_operations,
        }),
    }
}

fn transaction_stage_result_json(result: ProductTransactionStageResult) -> Value {
    match result {
        ProductTransactionStageResult::Sql(result) => {
            json!({ "type": "sql", "result": sql_result_json(result) })
        }
        ProductTransactionStageResult::Structure(result) => {
            json!({ "type": "structure", "result": structure_mutation_result_json(result) })
        }
        ProductTransactionStageResult::Search => json!({ "type": "search" }),
        ProductTransactionStageResult::Vector(changed) => {
            json!({ "type": "vector", "changed": changed })
        }
    }
}

fn structure_mutation_result_json(result: ProductStructureMutationResult) -> Value {
    match result {
        ProductStructureMutationResult::Unit => json!({ "type": "unit" }),
        ProductStructureMutationResult::Integer(value) => {
            json!({ "type": "integer", "value": value })
        }
        ProductStructureMutationResult::Boolean(value) => {
            json!({ "type": "boolean", "value": value })
        }
        ProductStructureMutationResult::Count(value) => {
            json!({ "type": "count", "value": value })
        }
        ProductStructureMutationResult::Value(value) => json!({
            "type": "value",
            "found": value.is_some(),
            "value": value.as_ref().and_then(|bytes| std::str::from_utf8(bytes).ok()),
            "value_hex": value.map(|bytes| encode_hex(&bytes)),
        }),
        ProductStructureMutationResult::StreamId(value) => {
            json!({ "type": "stream_id", "value": value })
        }
        ProductStructureMutationResult::Score(value) => {
            json!({ "type": "score", "value": value.get() })
        }
        ProductStructureMutationResult::PoppedEntry(entry) => json!({
            "type": "popped_entry",
            "found": entry.is_some(),
            "member": entry
                .as_ref()
                .and_then(|entry| std::str::from_utf8(&entry.member).ok()),
            "member_hex": entry.as_ref().map(|entry| encode_hex(&entry.member)),
            "score": entry.map(|entry| entry.score.get()),
        }),
        _ => json!({ "type": "unsupported" }),
    }
}

fn print_version(json_output: bool) -> Result<(), CliFailure> {
    let version = current_version();
    let native = capabilities();
    if json_output {
        print_json(&json!({
            "product": version.product,
            "engine_version": version.engine,
            "api_version": version.api,
            "disk_format_version": version.disk_format,
            "product_api_version": native.product_api_version,
            "native_directory_format": native.native_directory_format,
        }))
    } else {
        let mut output = BufWriter::new(stdout().lock());
        writeln!(
            output,
            "{} {} (native product API {}, native directory format {})",
            version.product,
            version.engine,
            native.product_api_version,
            native.native_directory_format
        )?;
        Ok(())
    }
}

fn product_document(document: IngestDocument) -> Result<ProductDocument, CliFailure> {
    Ok(ProductDocument {
        object_id: object_id(document.id.0)?,
        text: document.text,
        doc_values: document
            .doc_values
            .into_iter()
            .map(|(name, value)| Ok((name, product_doc_value(value)?)))
            .collect::<Result<_, CliFailure>>()?,
        vectors: document
            .vectors
            .into_iter()
            .map(|(name, value)| {
                Ok((
                    name,
                    ProductVector::new(value).map_err(|_| CliFailure::invalid())?,
                ))
            })
            .collect::<Result<_, CliFailure>>()?,
    })
}

fn parameter_strings(values: &[String]) -> Result<Vec<ProductValue>, CliFailure> {
    values
        .iter()
        .map(|value| {
            serde_json::from_str(value)
                .map_err(CliFailure::from)
                .and_then(product_value)
        })
        .collect()
}

fn catalog_name(value: &str) -> Result<hyphae_native_product::CatalogName, CliFailure> {
    hyphae_native_product::CatalogName::unquoted(value).map_err(|_| CliFailure::invalid())
}

fn catalog_header(
    id: u128,
    owner: EngineKind,
    name: &str,
    parent: Option<u128>,
) -> Result<ObjectHeaderV2, CliFailure> {
    Ok(ObjectHeaderV2 {
        id: object_id(id)?,
        owner,
        name: qualified_name(name)?,
        parent: parent.map(object_id).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn field_id(value: u32) -> Result<FieldId, CliFailure> {
    FieldId::new(value).map_err(|_| CliFailure::invalid())
}

fn vector_type(dimension: u16) -> Result<VectorType, CliFailure> {
    VectorType::new(VectorElement::Float32, dimension).map_err(|_| CliFailure::invalid())
}

const fn doc_value_options() -> SearchFieldOptions {
    SearchFieldOptions {
        stored: true,
        doc_values: true,
        source: FieldSourcePolicy::Retained,
        lexical: LexicalIndexPolicy::None,
    }
}

fn structure_key(keyspace: JsonU128, key: String) -> Result<ProductStructureKey, CliFailure> {
    Ok(ProductStructureKey {
        keyspace: object_id(keyspace.0)?,
        key: key.into_bytes(),
    })
}

#[allow(clippy::too_many_lines)]
fn structure_mutation(
    input: StructureMutationInput,
) -> Result<ProductStructureMutation, CliFailure> {
    Ok(match input {
        StructureMutationInput::StringSet {
            keyspace,
            key,
            value,
            expires_at_micros,
        } => ProductStructureMutation::StringSet {
            key: structure_key(keyspace, key)?,
            value: value.into_bytes(),
            expires_at_micros,
        },
        StructureMutationInput::StringDelete { keyspace, key } => {
            ProductStructureMutation::StringDelete {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureMutationInput::CounterAdd {
            keyspace,
            key,
            delta,
        } => ProductStructureMutation::CounterAdd {
            key: structure_key(keyspace, key)?,
            delta,
        },
        StructureMutationInput::Create {
            keyspace,
            key,
            family,
        } => ProductStructureMutation::Create {
            key: structure_key(keyspace, key)?,
            family: family.into(),
        },
        StructureMutationInput::Delete {
            keyspace,
            key,
            family,
        } => ProductStructureMutation::Delete {
            key: structure_key(keyspace, key)?,
            family: family.into(),
        },
        StructureMutationInput::Expire {
            keyspace,
            key,
            family,
            expires_at_micros,
        } => ProductStructureMutation::Expire {
            key: structure_key(keyspace, key)?,
            family: family.into(),
            expires_at_micros,
        },
        StructureMutationInput::HashSet {
            keyspace,
            key,
            field,
            value,
        } => ProductStructureMutation::HashSet {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
            value: value.into_bytes(),
        },
        StructureMutationInput::HashDelete {
            keyspace,
            key,
            field,
        } => ProductStructureMutation::HashDelete {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
        },
        StructureMutationInput::HashCounterAdd {
            keyspace,
            key,
            field,
            delta,
        } => ProductStructureMutation::HashCounterAdd {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
            delta,
        },
        StructureMutationInput::HashExpireField {
            keyspace,
            key,
            field,
            expires_at_micros,
        } => ProductStructureMutation::HashExpireField {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
            expires_at_micros,
        },
        StructureMutationInput::ListPush {
            keyspace,
            key,
            side,
            value,
        } => ProductStructureMutation::ListPush {
            key: structure_key(keyspace, key)?,
            side: side.into(),
            value: value.into_bytes(),
        },
        StructureMutationInput::ListPop {
            keyspace,
            key,
            side,
        } => ProductStructureMutation::ListPop {
            key: structure_key(keyspace, key)?,
            side: side.into(),
        },
        StructureMutationInput::SetAdd {
            keyspace,
            key,
            member,
        } => ProductStructureMutation::SetAdd {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
        },
        StructureMutationInput::SetRemove {
            keyspace,
            key,
            member,
        } => ProductStructureMutation::SetRemove {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
        },
        StructureMutationInput::SortedSetAdd {
            keyspace,
            key,
            member,
            score,
        } => ProductStructureMutation::SortedSetAdd {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
            score: hyphae_native_product::CanonicalF64::new(score),
        },
        StructureMutationInput::SortedSetRemove {
            keyspace,
            key,
            member,
        } => ProductStructureMutation::SortedSetRemove {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
        },
        StructureMutationInput::SortedSetIncrement {
            keyspace,
            key,
            member,
            delta,
        } => ProductStructureMutation::SortedSetIncrement {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
            delta: hyphae_native_product::CanonicalF64::new(delta),
        },
        StructureMutationInput::SortedSetPop { keyspace, key, end } => {
            ProductStructureMutation::SortedSetPop {
                key: structure_key(keyspace, key)?,
                highest: matches!(end, SortedSetEndInput::Highest),
            }
        }
        StructureMutationInput::StringSetConditional {
            keyspace,
            key,
            value,
            expires_at_micros,
            condition,
        } => ProductStructureMutation::StringSetConditional {
            key: structure_key(keyspace, key)?,
            value: value.into_bytes(),
            expires_at_micros,
            if_present: matches!(condition, SetConditionInput::IfPresent),
        },
        StructureMutationInput::StringAppend {
            keyspace,
            key,
            suffix,
        } => ProductStructureMutation::StringAppend {
            key: structure_key(keyspace, key)?,
            suffix: suffix.into_bytes(),
        },
        StructureMutationInput::StringSetRange {
            keyspace,
            key,
            offset,
            patch,
        } => ProductStructureMutation::StringSetRange {
            key: structure_key(keyspace, key)?,
            offset,
            patch: patch.into_bytes(),
        },
        StructureMutationInput::HashSetIfAbsent {
            keyspace,
            key,
            field,
            value,
        } => ProductStructureMutation::HashSetIfAbsent {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
            value: value.into_bytes(),
        },
        StructureMutationInput::SetPop {
            keyspace,
            key,
            seed,
        } => ProductStructureMutation::SetPop {
            key: structure_key(keyspace, key)?,
            seed,
        },
        StructureMutationInput::StreamAdd {
            keyspace,
            key,
            fields,
        } => ProductStructureMutation::StreamAdd {
            key: structure_key(keyspace, key)?,
            fields: fields
                .into_iter()
                .map(|(field, value)| ProductHashEntry {
                    field: field.into_bytes(),
                    value: value.into_bytes(),
                })
                .collect(),
        },
    })
}

#[allow(clippy::too_many_lines)]
fn structure_read(input: StructureReadInput) -> Result<ProductStructureReadRequest, CliFailure> {
    Ok(match input {
        StructureReadInput::StringGet { keyspace, key } => ProductStructureReadRequest::StringGet {
            key: structure_key(keyspace, key)?,
        },
        StructureReadInput::CounterGet { keyspace, key } => {
            ProductStructureReadRequest::CounterGet {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureReadInput::Ttl {
            keyspace,
            key,
            family,
        } => ProductStructureReadRequest::Ttl {
            key: structure_key(keyspace, key)?,
            family: family.into(),
        },
        StructureReadInput::HashGet {
            keyspace,
            key,
            field,
        } => ProductStructureReadRequest::HashGet {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
        },
        StructureReadInput::HashFieldTtl {
            keyspace,
            key,
            field,
        } => ProductStructureReadRequest::HashFieldTtl {
            key: structure_key(keyspace, key)?,
            field: field.into_bytes(),
        },
        StructureReadInput::HashScan {
            keyspace,
            key,
            start_after,
            limit,
        } => ProductStructureReadRequest::HashScan {
            key: structure_key(keyspace, key)?,
            start_after: start_after.map(String::into_bytes),
            limit,
        },
        StructureReadInput::HashLength { keyspace, key } => {
            ProductStructureReadRequest::HashLength {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureReadInput::ListRange {
            keyspace,
            key,
            start,
            stop,
        } => ProductStructureReadRequest::ListRange {
            key: structure_key(keyspace, key)?,
            start,
            stop,
        },
        StructureReadInput::ListLength { keyspace, key } => {
            ProductStructureReadRequest::ListLength {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureReadInput::SetContains {
            keyspace,
            key,
            member,
        } => ProductStructureReadRequest::SetContains {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
        },
        StructureReadInput::SetMembers {
            keyspace,
            key,
            start_after,
            limit,
        } => ProductStructureReadRequest::SetMembers {
            key: structure_key(keyspace, key)?,
            start_after: start_after.map(String::into_bytes),
            limit,
        },
        StructureReadInput::SetCardinality { keyspace, key } => {
            ProductStructureReadRequest::SetCardinality {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureReadInput::SetAlgebra {
            keyspace,
            operation_kind,
            keys,
            output_member_limit,
            visit_limit,
        } => ProductStructureReadRequest::SetAlgebra {
            keyspace: object_id(keyspace.0)?,
            operation: match operation_kind {
                SetAlgebraInput::Union => ProductSetAlgebraOperation::Union,
                SetAlgebraInput::Intersection => ProductSetAlgebraOperation::Intersection,
                SetAlgebraInput::Difference => ProductSetAlgebraOperation::Difference,
            },
            keys: keys.into_iter().map(String::into_bytes).collect(),
            output_member_limit,
            visit_limit,
        },
        StructureReadInput::SortedSetScore {
            keyspace,
            key,
            member,
        } => ProductStructureReadRequest::SortedSetScore {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
        },
        StructureReadInput::SortedSetRank {
            keyspace,
            key,
            member,
            order,
        } => ProductStructureReadRequest::SortedSetRank {
            key: structure_key(keyspace, key)?,
            member: member.into_bytes(),
            order: sorted_set_order(order),
        },
        StructureReadInput::SortedSetRange {
            keyspace,
            key,
            start,
            stop,
            order,
        } => ProductStructureReadRequest::SortedSetRange {
            key: structure_key(keyspace, key)?,
            start,
            stop,
            order: sorted_set_order(order),
        },
        StructureReadInput::SortedSetCardinality { keyspace, key } => {
            ProductStructureReadRequest::SortedSetCardinality {
                key: structure_key(keyspace, key)?,
            }
        }
        StructureReadInput::StreamRange {
            keyspace,
            key,
            start,
            end,
            limit,
        } => ProductStructureReadRequest::StreamRange {
            key: structure_key(keyspace, key)?,
            start,
            end,
            limit,
        },
        StructureReadInput::SortedSetScoreRange {
            keyspace,
            key,
            lower,
            upper,
            offset,
            limit,
            order,
        } => ProductStructureReadRequest::SortedSetScoreRange {
            key: structure_key(keyspace, key)?,
            lower: score_bound_input(lower),
            upper: score_bound_input(upper),
            offset,
            limit,
            order: sorted_set_order(order),
        },
        StructureReadInput::HashScanReverse {
            keyspace,
            key,
            start_before,
            limit,
        } => ProductStructureReadRequest::HashScanReverse {
            key: structure_key(keyspace, key)?,
            start_before: start_before.map(String::into_bytes),
            limit,
        },
        StructureReadInput::HashScanMatch {
            keyspace,
            key,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => ProductStructureReadRequest::HashScanMatch {
            key: structure_key(keyspace, key)?,
            pattern: pattern.into_bytes(),
            start_after: start_after.map(String::into_bytes),
            output_limit,
            visit_limit,
            match_step_limit,
        },
        StructureReadInput::KeyScanMatch {
            keyspace,
            pattern,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        } => ProductStructureReadRequest::KeyScanMatch {
            keyspace: object_id(keyspace.0)?,
            pattern: pattern.into_bytes(),
            start_after: start_after.map(String::into_bytes),
            output_limit,
            visit_limit,
            match_step_limit,
        },
        StructureReadInput::StringRange {
            keyspace,
            key,
            start,
            end,
        } => ProductStructureReadRequest::StringRange {
            key: structure_key(keyspace, key)?,
            start,
            end,
        },
        StructureReadInput::SetRandomMembers {
            keyspace,
            key,
            seed,
            count,
        } => ProductStructureReadRequest::SetRandomMembers {
            key: structure_key(keyspace, key)?,
            seed,
            count,
        },
    })
}

const fn score_bound_input(input: ScoreBoundInput) -> hyphae_native_product::ProductScoreBound {
    match input {
        ScoreBoundInput::Unbounded => hyphae_native_product::ProductScoreBound::Unbounded,
        ScoreBoundInput::Bounded {
            exclusive: false,
            score,
        } => hyphae_native_product::ProductScoreBound::Inclusive(score),
        ScoreBoundInput::Bounded {
            exclusive: true,
            score,
        } => hyphae_native_product::ProductScoreBound::Exclusive(score),
    }
}

const fn sorted_set_order(order: SortOrderInput) -> hyphae_native_product::ProductSortedSetOrder {
    match order {
        SortOrderInput::Ascending => hyphae_native_product::ProductSortedSetOrder::Ascending,
        SortOrderInput::Descending => hyphae_native_product::ProductSortedSetOrder::Descending,
    }
}

fn product_doc_value(value: Value) -> Result<ProductDocValue, CliFailure> {
    match value {
        Value::Bool(value) => Ok(ProductDocValue::Boolean(value)),
        Value::Number(value) => value
            .as_i64()
            .map(ProductDocValue::Integer)
            .ok_or_else(CliFailure::invalid),
        Value::String(value) => Ok(ProductDocValue::String(value)),
        Value::Object(mut object) if object.len() == 1 && object.contains_key("bytes_hex") => {
            let encoded = object
                .remove("bytes_hex")
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(CliFailure::invalid)?;
            Ok(ProductDocValue::Bytes(decode_hex_bytes(&encoded)?))
        }
        _ => Err(CliFailure::invalid()),
    }
}

fn product_search_filter(value: Value) -> Result<ProductSearchFilter, CliFailure> {
    let Value::Object(mut object) = value else {
        return Err(CliFailure::invalid());
    };
    let operation = take_string(&mut object, "operation")?;
    let filter = match operation.as_str() {
        "match_all" => ProductSearchFilter::MatchAll,
        "exists" => ProductSearchFilter::Exists(take_string(&mut object, "field")?),
        "compare" => ProductSearchFilter::Compare {
            field: take_string(&mut object, "field")?,
            operator: match take_string(&mut object, "operator")?.as_str() {
                "equal" => ProductSearchOperator::Equal,
                "not_equal" => ProductSearchOperator::NotEqual,
                "less" => ProductSearchOperator::Less,
                "less_or_equal" => ProductSearchOperator::LessOrEqual,
                "greater" => ProductSearchOperator::Greater,
                "greater_or_equal" => ProductSearchOperator::GreaterOrEqual,
                _ => return Err(CliFailure::invalid()),
            },
            value: product_doc_value(object.remove("value").ok_or_else(CliFailure::invalid)?)?,
        },
        "all" | "any" => {
            let children = object
                .remove("filters")
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(CliFailure::invalid)?
                .into_iter()
                .map(product_search_filter)
                .collect::<Result<Vec<_>, _>>()?;
            if operation == "all" {
                ProductSearchFilter::All(children)
            } else {
                ProductSearchFilter::Any(children)
            }
        }
        "not" => ProductSearchFilter::Not(Box::new(product_search_filter(
            object.remove("filter").ok_or_else(CliFailure::invalid)?,
        )?)),
        "in" => ProductSearchFilter::In {
            field: take_string(&mut object, "field")?,
            values: object
                .remove("values")
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(CliFailure::invalid)?
                .into_iter()
                .map(product_doc_value)
                .collect::<Result<Vec<_>, _>>()?,
        },
        "is_null" => ProductSearchFilter::IsNull(take_string(&mut object, "field")?),
        "like" => ProductSearchFilter::Like {
            field: take_string(&mut object, "field")?,
            pattern: take_string(&mut object, "pattern")?,
        },
        _ => return Err(CliFailure::invalid()),
    };
    if object.is_empty() {
        Ok(filter)
    } else {
        Err(CliFailure::invalid())
    }
}

fn product_search_sort(value: Value) -> Result<ProductSearchSort, CliFailure> {
    let Value::Object(mut object) = value else {
        return Err(CliFailure::invalid());
    };
    let source = match take_string(&mut object, "source")?.as_str() {
        "score" => ProductSortSource::Score,
        "field" => ProductSortSource::Field(take_string(&mut object, "field")?),
        _ => return Err(CliFailure::invalid()),
    };
    let direction = match take_string(&mut object, "direction")?.as_str() {
        "ascending" => ProductSortDirection::Ascending,
        "descending" => ProductSortDirection::Descending,
        _ => return Err(CliFailure::invalid()),
    };
    let missing = match take_string(&mut object, "missing")?.as_str() {
        "first" => ProductMissingPlacement::First,
        "last" => ProductMissingPlacement::Last,
        _ => return Err(CliFailure::invalid()),
    };
    if !object.is_empty() {
        return Err(CliFailure::invalid());
    }
    Ok(ProductSearchSort {
        source,
        direction,
        missing,
    })
}

fn product_facet(value: Value) -> Result<ProductFacetRequest, CliFailure> {
    let Value::Object(mut object) = value else {
        return Err(CliFailure::invalid());
    };
    let field = take_string(&mut object, "field")?;
    let limit = take_usize(&mut object, "limit")?;
    if !object.is_empty() {
        return Err(CliFailure::invalid());
    }
    Ok(ProductFacetRequest { field, limit })
}

fn product_aggregation(value: Value) -> Result<ProductNamedAggregation, CliFailure> {
    let Value::Object(mut object) = value else {
        return Err(CliFailure::invalid());
    };
    let name = take_string(&mut object, "name")?;
    let aggregation = match take_string(&mut object, "operation")?.as_str() {
        "count" => ProductAggregation::Count,
        "sum" => ProductAggregation::Sum(take_string(&mut object, "field")?),
        "min" => ProductAggregation::Min(take_string(&mut object, "field")?),
        "max" => ProductAggregation::Max(take_string(&mut object, "field")?),
        _ => return Err(CliFailure::invalid()),
    };
    if !object.is_empty() {
        return Err(CliFailure::invalid());
    }
    Ok(ProductNamedAggregation { name, aggregation })
}

fn take_string(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, CliFailure> {
    object
        .remove(key)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(CliFailure::invalid)
}

fn take_usize(object: &mut serde_json::Map<String, Value>, key: &str) -> Result<usize, CliFailure> {
    object
        .remove(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(CliFailure::invalid)
}

fn product_value(value: Value) -> Result<ProductValue, CliFailure> {
    match value {
        Value::Null => Ok(ProductValue::Null),
        Value::Bool(value) => Ok(ProductValue::Boolean(value)),
        Value::Number(value) => value
            .as_i64()
            .map(ProductValue::Signed)
            .or_else(|| value.as_u64().map(ProductValue::Unsigned))
            .or_else(|| {
                value.as_f64().map(|value| {
                    ProductValue::Float64(hyphae_native_product::CanonicalF64::new(value))
                })
            })
            .ok_or_else(CliFailure::invalid),
        Value::String(value) => Ok(ProductValue::Text(value)),
        Value::Array(values) => values
            .into_iter()
            .map(product_value)
            .collect::<Result<Vec<_>, _>>()
            .map(ProductValue::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((ProductValue::Text(key), product_value(value)?)))
            .collect::<Result<Vec<_>, CliFailure>>()
            .map(ProductValue::Map),
    }
}

fn value_json(value: ProductValue) -> Value {
    match value {
        ProductValue::Null => Value::Null,
        ProductValue::Boolean(value) => json!(value),
        ProductValue::Signed(value) => json!(value),
        ProductValue::Unsigned(value) => json!(value),
        ProductValue::Decimal(value) => Value::String(value.to_string()),
        ProductValue::Float32(value) => json!(value.get()),
        ProductValue::Float64(value) => json!(value.get()),
        ProductValue::Text(value) | ProductValue::Json(value) => Value::String(value),
        ProductValue::Binary(value) => json!({ "binary_hex": encode_hex(&value) }),
        ProductValue::Date(value) => json!({ "date_days": value }),
        ProductValue::Time(value) => json!({ "time_nanos": value }),
        ProductValue::Timestamp(value) => json!({ "timestamp_micros": value }),
        ProductValue::Interval {
            months,
            days,
            nanoseconds,
        } => json!({
            "months": months, "days": days, "nanoseconds": nanoseconds,
        }),
        ProductValue::Uuid(value) => json!({ "uuid_hex": encode_hex(&value) }),
        ProductValue::Array(values) => Value::Array(values.into_iter().map(value_json).collect()),
        ProductValue::Map(entries) => Value::Array(
            entries
                .into_iter()
                .map(|(key, value)| json!([value_json(key), value_json(value)]))
                .collect(),
        ),
        ProductValue::Vector(values) => {
            Value::Array(values.into_iter().map(|value| json!(value.get())).collect())
        }
        _ => Value::String("unsupported_value".to_owned()),
    }
}

fn sql_result_json(result: ProductSqlResult) -> Value {
    match result {
        ProductSqlResult::Command {
            rows_affected,
            object_id,
        } => json!({
            "type": "command",
            "rows_affected": rows_affected,
            "object_id": object_id.map(|value| value.get().to_string()),
        }),
        ProductSqlResult::Rows { columns, rows } => json!({
            "type": "rows",
            "columns": columns,
            "rows": rows.into_iter().map(|row| row.into_iter().map(value_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
    }
}

fn snapshot_json(snapshot: SnapshotIdentity) -> Value {
    json!({
        "directory_lineage": encode_hex(&snapshot.directory_lineage),
        "visible_csn": snapshot.visible_csn.map(hyphae_native_product::Csn::get),
        "catalog_version": snapshot.catalog_version.get(),
        "root_digest": encode_hex(&snapshot.root_digest),
        "logical_time_micros": snapshot.logical_time_micros,
    })
}

fn commit_outcome_json(outcome: ProductCommitOutcome) -> Value {
    match outcome {
        ProductCommitOutcome::Committed(receipt) => {
            let mut value = commit_json(receipt);
            value["status"] = json!("committed");
            value
        }
        ProductCommitOutcome::OutcomeUnknown { transaction_id } => json!({
            "status": "outcome_unknown",
            "transaction_id": transaction_id.to_string(),
        }),
    }
}

fn commit_json(receipt: ProductCommitReceipt) -> Value {
    json!({
        "transaction_id": receipt.transaction_id.to_string(),
        "commit_csn": receipt.commit_csn,
        "catalog_version": receipt.catalog_version,
        "commit_lsn": receipt.commit_lsn,
        "wal_block_digest": encode_hex(&receipt.wal_block_digest),
        "durability": durability(receipt.durability),
        "durability_cohort_size": receipt.durability_cohort_size,
        "durability_cohort_position": receipt.durability_cohort_position,
    })
}

fn transaction_status_json(status: ProductTransactionStatus) -> Value {
    match status {
        ProductTransactionStatus::Unknown => json!({ "status": "unknown" }),
        ProductTransactionStatus::Committed(receipt) => {
            let mut value = commit_json(receipt);
            value["status"] = json!("committed");
            value
        }
        ProductTransactionStatus::RolledBack { transaction_id } => json!({
            "status": "rolled_back", "transaction_id": transaction_id.to_string(),
        }),
        ProductTransactionStatus::OutcomeUnknown { transaction_id } => json!({
            "status": "outcome_unknown", "transaction_id": transaction_id.to_string(),
        }),
    }
}

fn ttl_json(ttl: ProductTtl) -> Value {
    match ttl {
        ProductTtl::Missing => json!({ "status": "missing" }),
        ProductTtl::Persistent => json!({ "status": "persistent" }),
        ProductTtl::RemainingMicros(value) => json!({
            "status": "remaining", "remaining_micros": value,
        }),
    }
}

fn search_result_json(results: ProductSearchResults) -> Value {
    json!({
        "hits": results.hits.into_iter().map(|hit| json!({
            "document_id_hex": encode_hex(&hit.document_id),
            "score": hit.score.get(),
        })).collect::<Vec<_>>(),
        "documents_examined": results.documents_examined,
        "source_bytes": results.source_bytes,
        "token_visits": results.token_visits,
        "token_comparisons": results.token_comparisons,
        "fuzzy_steps": results.fuzzy_steps,
    })
}

fn doc_value_json(value: ProductDocValue) -> Value {
    match value {
        ProductDocValue::Boolean(value) => json!(value),
        ProductDocValue::Integer(value) => json!(value),
        ProductDocValue::Float(value) => json!(value.get()),
        ProductDocValue::String(value) => json!(value),
        ProductDocValue::Bytes(value) => json!({ "bytes_hex": encode_hex(&value) }),
    }
}

fn aggregation_value_json(value: hyphae_native_product::ProductAggregationValue) -> Value {
    match value {
        hyphae_native_product::ProductAggregationValue::Count(value) => json!(value),
        hyphae_native_product::ProductAggregationValue::Integer(value) => {
            value.map_or(Value::Null, |value| Value::String(value.to_string()))
        }
        hyphae_native_product::ProductAggregationValue::Value(value) => {
            value.map_or(Value::Null, doc_value_json)
        }
        hyphae_native_product::ProductAggregationValue::Float(value) => {
            value.map_or(Value::Null, |value| json!(value.get()))
        }
    }
}

fn doctor_json(report: hyphae_native_product::DoctorReport) -> Value {
    json!({
        "status": doctor_status(report.status),
        "verified_open": report.verified_open,
        "snapshot_verified": report.snapshot_verified,
        "directory_lineage": report.directory_lineage.map(|value| encode_hex(&value)),
        "telemetry_registry_version": report.telemetry_registry_version,
        "process_start_identity": report.process_start_identity.to_string(),
        "session_start_identity": report.session_start_identity.to_string(),
        "recovery": report.recovery.map(|recovery| json!({
            "visible_csn": recovery.visible_csn.map(hyphae_native_product::Csn::get),
            "replayed_transactions": recovery.replayed_transactions,
            "page_tail_bytes_removed": recovery.page_tail_bytes_removed,
            "wal_tail_bytes_removed": recovery.wal_tail_bytes_removed,
            "retained_wal_bytes": recovery.retained_wal_bytes,
            "manifest_count": recovery.manifest_count,
            "blob_count": recovery.blob_count,
            "open_time_micros": recovery.open_time_micros,
        })),
    })
}

fn explain_json(explanation: ProductExplain) -> Value {
    match explanation {
        ProductExplain::SqlPlanText(plan) => json!({
            "type": "sql_plan_text", "version": plan.version, "text": plan.text,
            "visible_csn": plan.visible_csn, "catalog_version": plan.catalog_version,
            "executed": plan.executed,
        }),
        ProductExplain::Convergence(value) => json!({
            "type": "convergence", "snapshot_csn": value.snapshot_csn,
            "strategies": format!("{:?}", value.strategies),
            "inner_join_by_object_id": value.inner_join_by_object_id,
            "stable_object_id_order": value.stable_object_id_order,
        }),
        ProductExplain::Ann(value) => json!({
            "type": "ann", "index": value.index.get().to_string(),
            "snapshot_csn": value.snapshot_csn, "approximate": value.approximate,
            "build_identity": encode_hex(&value.build_identity), "ef_search": value.ef_search,
            "candidate_count": value.candidate_count,
            "eligible_candidate_count": value.eligible_candidate_count,
            "exact_reranked": value.exact_reranked, "visited_nodes": value.visited_nodes,
        }),
        ProductExplain::Hybrid(value) => json!({
            "type": "hybrid", "lexical_index": value.lexical_index.get().to_string(),
            "lexical_limit": value.lexical_limit,
            "vector_index": value.vector_index.get().to_string(),
            "vector_limit": value.vector_limit, "lexical_weight": value.lexical_weight,
            "vector_weight": value.vector_weight, "fusion_limit": value.fusion_limit,
            "rrf_constant": value.rrf_constant,
        }),
    }
}

fn backup_json(status: &str, info: &BackupInfo) -> Value {
    json!({
        "status": status,
        "backup_path": info.path,
        "visible_csn": info.visible_csn,
        "checkpoint_digest": encode_hex(&info.checkpoint_digest),
        "file_count": info.file_count,
        "total_bytes": info.total_bytes,
    })
}

#[allow(clippy::needless_pass_by_value)]
fn metric_kind(value: MetricValue) -> Value {
    match value {
        MetricValue::Counter(value) => json!({ "type": "counter", "value": value }),
        MetricValue::Gauge(value) => json!({ "type": "gauge", "value": value }),
        MetricValue::Histogram {
            count,
            sum_micros,
            buckets,
        } => json!({
            "type": "histogram", "count": count, "sum_micros": sum_micros,
            "buckets": buckets,
        }),
    }
}

fn object_id(value: u128) -> Result<ObjectId, CliFailure> {
    ObjectId::new(value).map_err(|_| CliFailure::invalid())
}

fn qualified_name(value: &str) -> Result<QualifiedName, CliFailure> {
    let parts = value.split('.').collect::<Vec<_>>();
    let [database, schema, object] = parts.as_slice() else {
        return Err(CliFailure::invalid());
    };
    Ok(QualifiedName::new(
        hyphae_native_product::CatalogName::unquoted(*database)
            .map_err(|_| CliFailure::invalid())?,
        hyphae_native_product::CatalogName::unquoted(*schema).map_err(|_| CliFailure::invalid())?,
        hyphae_native_product::CatalogName::unquoted(*object).map_err(|_| CliFailure::invalid())?,
    ))
}

fn decode_hex<const N: usize>(encoded: &str) -> Result<[u8; N], CliFailure> {
    if encoded.len() != N.saturating_mul(2) {
        return Err(CliFailure::invalid());
    }
    let mut bytes = [0_u8; N];
    for (index, chunk) in encoded.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
    }
    Ok(bytes)
}

fn decode_hex_bytes(encoded: &str) -> Result<Vec<u8>, CliFailure> {
    if !encoded.len().is_multiple_of(2) {
        return Err(CliFailure::invalid());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Ok((hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?))
        .collect()
}

fn hex_nibble(value: u8) -> Result<u8, CliFailure> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CliFailure::invalid()),
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn catalog_kind(kind: CatalogObjectKind) -> &'static str {
    match kind {
        CatalogObjectKind::Database => "database",
        CatalogObjectKind::Schema => "schema",
        CatalogObjectKind::Relation => "relation",
        CatalogObjectKind::SecondaryIndex => "secondary_index",
        CatalogObjectKind::Keyspace => "keyspace",
        CatalogObjectKind::Structure => "structure",
        CatalogObjectKind::SearchCollection => "search_collection",
        CatalogObjectKind::Analyzer => "analyzer",
        CatalogObjectKind::CrossEngineLink => "cross_engine_link",
    }
}

const fn durability(value: ProductDurability) -> &'static str {
    match value {
        ProductDurability::Strict => "strict",
        ProductDurability::Group => "group",
        ProductDurability::Memory => "memory",
    }
}

const fn compaction_target(value: CompactionTarget) -> &'static str {
    match value {
        CompactionTarget::Structures => "structures",
        CompactionTarget::Search => "search",
    }
}

const fn doctor_status(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Healthy => "healthy",
        DoctorStatus::Busy => "busy",
        DoctorStatus::Corrupt => "corrupt",
        DoctorStatus::Io => "io",
    }
}

const fn proof_kind(kind: NativeProofKind) -> &'static str {
    match kind {
        NativeProofKind::Point => "point",
        NativeProofKind::Sql => "sql",
        NativeProofKind::Lexical => "lexical",
        NativeProofKind::ExactVector => "exact_vector",
        NativeProofKind::Ann => "ann",
        NativeProofKind::Hybrid => "hybrid",
        NativeProofKind::Catalog => "catalog",
    }
}

const fn dependency_kind(kind: DependencyKind) -> &'static str {
    match kind {
        DependencyKind::Parent => "parent",
        DependencyKind::SecondaryIndexRelation => "secondary_index_relation",
        DependencyKind::ForeignKey => "foreign_key",
        DependencyKind::Analyzer => "analyzer",
        DependencyKind::LinkEndpoint => "link_endpoint",
        DependencyKind::RelationSchema => "relation_schema",
    }
}

const fn vector_strategy(strategy: ProductVectorStrategy) -> &'static str {
    match strategy {
        ProductVectorStrategy::ExactFiltered => "exact_filtered",
        ProductVectorStrategy::AdaptiveExactFiltered => "adaptive_exact_filtered",
        ProductVectorStrategy::FilterAwareAnn => "filter_aware_ann",
        ProductVectorStrategy::AdaptiveFilterAwareAnn => "adaptive_filter_aware_ann",
    }
}

const fn catalog_page_stop(stop: hyphae_native_runtime::CatalogPageStop) -> &'static str {
    match stop {
        hyphae_native_runtime::CatalogPageStop::Exhausted => "exhausted",
        hyphae_native_runtime::CatalogPageStop::ItemLimit => "item_limit",
        hyphae_native_runtime::CatalogPageStop::VisitLimit => "visit_limit",
        hyphae_native_runtime::CatalogPageStop::ByteLimit => "byte_limit",
    }
}

const fn restore_phase(phase: RestorePhase) -> &'static str {
    match phase {
        RestorePhase::ValidatingRequest => "validating_request",
        RestorePhase::VerifyingBackup => "verifying_backup",
        RestorePhase::RestoringAndPromoting => "restoring_and_promoting",
        RestorePhase::Promoted => "promoted",
        RestorePhase::DoctorAfterRestore => "doctor_after_restore",
        RestorePhase::Complete => "complete",
    }
}

fn print_json(value: &Value) -> Result<(), CliFailure> {
    let mut output = BufWriter::new(stdout().lock());
    write_json(&mut output, value)
}

fn write_json(output: &mut impl Write, value: &Value) -> Result<(), CliFailure> {
    serde_json::to_writer_pretty(&mut *output, value)?;
    writeln!(output)?;
    Ok(())
}

fn print_error(failure: &CliFailure) -> Result<(), std::io::Error> {
    let mut output = BufWriter::new(stderr().lock());
    serde_json::to_writer(&mut output, &exit::error_json(failure.error()))?;
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        Cli, Command, HardwareCalibrationMode, HardwareCommand, HardwareGovernorMode,
        authorization_permissions, decode_hex, encode_hex, hardware_with_writers, qualified_name,
    };
    use clap::Parser;
    use hyphae_native_product::{ProductAuthorization, ProductPermission};
    use hyphae_native_runtime::{
        CalibrationCacheStatus, CalibrationCorrectness, CalibrationCoverage,
        CalibrationFeatureDetection, CalibrationIdentity, CalibrationIoScaling,
        CalibrationMeasurement, CalibrationMode, CalibrationPolicy, CalibrationStatistics,
        CalibrationThreadScaling, GovernorMode, HardwareCalibration, HardwareProfile,
        NativeGovernorPolicy, SelectedCalibrationKernel, UnsupportedCalibration,
    };

    struct TemporaryDirectory(PathBuf);

    static TEMPORARY_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    impl TemporaryDirectory {
        fn create(label: &str) -> Result<Self, std::io::Error> {
            let sequence = TEMPORARY_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-cli-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn native_cli_hex_and_names_are_canonical() {
        let bytes = [0, 1, 15, 16, 255];
        assert_eq!(decode_hex::<5>(&encode_hex(&bytes)).ok(), Some(bytes));
        assert!(decode_hex::<1>("FF").is_err());
        assert_eq!(
            qualified_name("main.public.items")
                .map(|name| name.to_string())
                .ok()
                .as_deref(),
            Some("main.public.items")
        );
        assert!(qualified_name("items").is_err());
    }

    #[test]
    fn authorization_output_discovers_every_known_tag_and_omits_unknown_tags() {
        let permissions = authorization_permissions(ProductAuthorization::ALL);
        let known = (0_u8..=u8::MAX)
            .filter_map(ProductPermission::from_tag)
            .map(ProductPermission::as_str)
            .collect::<Vec<_>>();
        assert_eq!(permissions, known);
        assert!(ProductPermission::from_tag(u8::MAX).is_none());
    }

    #[test]
    fn hardware_discovery_does_not_require_a_data_directory() {
        let cli = Cli::try_parse_from(["hyphae", "hardware", "discover"]);
        assert!(matches!(
            cli.map(|value| value.command),
            Ok(Command::Hardware {
                operation: HardwareCommand::Discover { data_dir: None }
            })
        ));
    }

    #[test]
    fn hardware_calibration_defaults_to_quick_mode() {
        let cli = Cli::try_parse_from(["hyphae", "hardware", "calibrate"]);
        assert!(matches!(
            cli.map(|value| value.command),
            Ok(Command::Hardware {
                operation: HardwareCommand::Calibrate {
                    data_dir: None,
                    mode: HardwareCalibrationMode::Quick,
                    cache_dir: None,
                    no_cache: false
                }
            })
        ));
    }

    fn effective_processor_boundaries(profile: &HardwareProfile) -> (u64, u64) {
        let logical = profile.cpu.quota_millicores.map_or(
            profile.cpu.logical_processors_available.max(1),
            |quota| {
                profile.cpu.logical_processors_available.max(1).min(
                    usize::try_from(quota.div_ceil(1_000))
                        .unwrap_or(usize::MAX)
                        .max(1),
                )
            },
        );
        let physical = profile
            .cpu
            .physical_cores_visible
            .unwrap_or(logical)
            .min(logical)
            .max(1);
        (
            u64::try_from(physical).unwrap_or(u64::MAX),
            u64::try_from(logical).unwrap_or(u64::MAX),
        )
    }

    fn stable_thread_measurement(digest: &str) -> CalibrationMeasurement {
        CalibrationMeasurement {
            primitive: "thread-scaling-memory-scan".to_owned(),
            variant: "persistent-workers-physical-range-unbound".to_owned(),
            input_size: 1,
            input_unit: "threads".to_owned(),
            bytes_per_operation: 1_048_576,
            operations_per_sample: 500,
            maximum_operations_per_sample: 1 << 20,
            sample_count: 31,
            statistics: CalibrationStatistics {
                unit: "picoseconds_per_operation".to_owned(),
                minimum: 450_000_000,
                median: 450_000_000,
                maximum: 450_000_000,
                median_absolute_deviation: 0,
                relative_mad_ppm: 0,
                relative_range_ppm: 0,
                median_bytes_per_second: Some(2_330_168_888),
            },
            correctness: CalibrationCorrectness {
                status: "passed".to_owned(),
                result_digest_blake3: digest.to_owned(),
                reference_digest_blake3: digest.to_owned(),
            },
            status: "stable".to_owned(),
            retry_history: Vec::new(),
        }
    }

    fn stable_io_measurement(digest: &str) -> CalibrationMeasurement {
        CalibrationMeasurement {
            primitive: "queue-depth-random-read".to_owned(),
            variant: "buffered-sync-workers".to_owned(),
            input_size: 1,
            input_unit: "outstanding_reads".to_owned(),
            bytes_per_operation: 4_096,
            operations_per_sample: 64,
            maximum_operations_per_sample: 64,
            sample_count: 31,
            statistics: CalibrationStatistics {
                unit: "picoseconds_per_operation".to_owned(),
                minimum: 3_515_625_000,
                median: 3_515_625_000,
                maximum: 3_515_625_000,
                median_absolute_deviation: 0,
                relative_mad_ppm: 0,
                relative_range_ppm: 0,
                median_bytes_per_second: Some(1_165_084),
            },
            correctness: CalibrationCorrectness {
                status: "passed".to_owned(),
                result_digest_blake3: digest.to_owned(),
                reference_digest_blake3: digest.to_owned(),
            },
            status: "stable".to_owned(),
            retry_history: Vec::new(),
        }
    }

    fn current_valid_calibration_receipt(profile: &HardwareProfile) -> HardwareCalibration {
        let digest = "ab".repeat(32);
        let thread_measurement = stable_thread_measurement(&digest);
        let io_measurement = stable_io_measurement(&digest);
        let (physical_core_boundary, logical_processor_boundary) =
            effective_processor_boundaries(profile);
        HardwareCalibration {
            schema: "hyphae-native-hardware-calibration-v1".to_owned(),
            mode: CalibrationMode::Thorough,
            status: "stable".to_owned(),
            accepted_for_scheduling: true,
            cache_status: CalibrationCacheStatus::Disabled,
            elapsed_ms: 180_000,
            identity: CalibrationIdentity {
                hardware_fingerprint: profile.fingerprint.clone(),
                kernel_release: profile.operating_system.kernel_release.clone(),
                filesystem: profile.storage.filesystem.clone(),
                compiler_identity: "rustc-test".to_owned(),
                hyphae_build_identity: "hyphae-cli-test".to_owned(),
                executable_blake3: digest,
                cache_key: "cd".repeat(32),
            },
            policy: CalibrationPolicy {
                minimum_duration_ms: 180_000,
                maximum_duration_ms: 600_000,
                warmup_batches: 4,
                samples_per_measurement: 31,
                target_sample_duration_ms: 225,
                maximum_relative_mad_ppm: 40_000,
                maximum_relative_range_ppm: 300_000,
                measurement_retry_limit: 3,
            },
            feature_detection: CalibrationFeatureDetection {
                instruction_sets: profile.cpu.instruction_sets.clone(),
                differential_tests_passed: true,
            },
            measurements: vec![thread_measurement, io_measurement],
            selected_kernels: vec![
                SelectedCalibrationKernel {
                    primitive: "thread-scaling-memory-scan".to_owned(),
                    input_size: 1,
                    input_unit: "threads".to_owned(),
                    variant: "persistent-workers-physical-range-unbound".to_owned(),
                    reason: "candidate passed correctness and variance policy".to_owned(),
                },
                SelectedCalibrationKernel {
                    primitive: "queue-depth-random-read".to_owned(),
                    input_size: 1,
                    input_unit: "outstanding_reads".to_owned(),
                    variant: "buffered-sync-workers".to_owned(),
                    reason: "candidate passed correctness and variance policy".to_owned(),
                },
            ],
            thread_scaling: CalibrationThreadScaling {
                binding: "unbound".to_owned(),
                physical_core_boundary,
                logical_processor_boundary,
                measured_thread_counts: vec![1],
                status: "stable".to_owned(),
                physical_peak_threads: Some(1),
                physical_peak_bytes_per_second: Some(2_330_168_888),
                smt_peak_threads: None,
                smt_peak_bytes_per_second: None,
                smt_to_physical_throughput_ppm: None,
                smt_recommended: false,
                recommended_worker_count: Some(1),
                recommendation: "SMT did not clear the frozen five-percent throughput-gain threshold; use the measured physical-range peak for the recorded placement adapter".to_owned(),
            },
            io_scaling: CalibrationIoScaling {
                binding: "buffered-sync-workers".to_owned(),
                measured_queue_depths: vec![1],
                status: "stable".to_owned(),
                peak_queue_depth: Some(1),
                peak_bytes_per_second: Some(1_165_084),
                recommended_io_slots: Some(1),
                recommendation: "use the smallest measured outstanding-read depth within five percent of peak buffered-read throughput".to_owned(),
            },
            coverage: CalibrationCoverage {
                measured: vec![
                    "queue-depth-random-read".to_owned(),
                    "thread-scaling-memory-scan".to_owned(),
                ],
                unsupported: vec![
                    UnsupportedCalibration {
                        primitive: "simd-vector-kernels".to_owned(),
                        reason: "safe instruction-specific candidates and differential tests are not implemented".to_owned(),
                    },
                    UnsupportedCalibration {
                        primitive: "asynchronous-io-adapters".to_owned(),
                        reason: "io_uring, IOCP, and equivalent platform-specific adapters are pending".to_owned(),
                    },
                ],
            },
            claims: Vec::new(),
        }
    }

    fn make_thread_scaling_unavailable(
        mut calibration: HardwareCalibration,
    ) -> Option<HardwareCalibration> {
        let thread_measurement = calibration
            .measurements
            .iter_mut()
            .find(|measurement| measurement.primitive == "thread-scaling-memory-scan")?;
        thread_measurement.statistics.minimum = 427_500_000;
        thread_measurement.statistics.maximum = 477_000_000;
        thread_measurement.statistics.median_absolute_deviation = 22_500_000;
        thread_measurement.statistics.relative_mad_ppm = 50_000;
        thread_measurement.statistics.relative_range_ppm = 110_000;
        thread_measurement.status = "unstable".to_owned();
        calibration.status = "unstable".to_owned();
        calibration.accepted_for_scheduling = false;
        calibration.selected_kernels.clear();
        calibration.thread_scaling.status = "unavailable".to_owned();
        calibration.thread_scaling.physical_peak_threads = None;
        calibration.thread_scaling.physical_peak_bytes_per_second = None;
        calibration.thread_scaling.smt_peak_threads = None;
        calibration.thread_scaling.smt_peak_bytes_per_second = None;
        calibration.thread_scaling.smt_to_physical_throughput_ppm = None;
        calibration.thread_scaling.smt_recommended = false;
        calibration.thread_scaling.recommended_worker_count = None;
        calibration.thread_scaling.recommendation = "thread scaling is unavailable because at least one curve point is missing, incorrect, or unstable".to_owned();
        Some(calibration)
    }

    #[test]
    fn hardware_governor_policy_reports_scaling_rejection_without_stdout()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("governor-scaling-unavailable")?;
        let profile = HardwareProfile::discover(&directory.0)?;
        let profile_path = directory.join("profile.json");
        let calibration_path = directory.join("calibration.json");
        fs::write(&profile_path, serde_json::to_vec_pretty(&profile)?)?;
        let valid_calibration = current_valid_calibration_receipt(&profile);
        NativeGovernorPolicy::derive(&profile, &valid_calibration, GovernorMode::Mixed)?;
        let rejected_calibration = make_thread_scaling_unavailable(valid_calibration)
            .ok_or("current receipt has no thread-scaling measurement")?;
        fs::write(
            &calibration_path,
            serde_json::to_vec_pretty(&rejected_calibration)?,
        )?;

        let cli = Cli::try_parse_from([
            OsString::from("hyphae"),
            OsString::from("hardware"),
            OsString::from("governor-policy"),
            OsString::from("--profile"),
            profile_path.into_os_string(),
            OsString::from("--calibration"),
            calibration_path.into_os_string(),
        ])?;
        let Command::Hardware { operation } = cli.command else {
            return Err("expected hardware command".into());
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let Err(failure) = hardware_with_writers(operation, &mut stdout, &mut stderr) else {
            return Err("unstable thread scaling did not fail closed".into());
        };

        assert_eq!(failure.exit_class(), 2);
        assert_eq!(failure.error().code().as_str(), "invalid_request");
        assert!(stdout.is_empty(), "rejected policy emitted stdout");
        let diagnostic = String::from_utf8(stderr)?;
        assert!(diagnostic.contains("stable thread-scaling recommendation is unavailable"));
        assert!(diagnostic.contains("thread_scaling.status=unavailable"));
        assert!(
            diagnostic
                .contains("thread-scaling-memory-scan@1 (unstable, mad=50000ppm range=110000ppm)")
        );
        Ok(())
    }

    #[test]
    fn hardware_governor_policy_defaults_to_mixed_mode() {
        let cli = Cli::try_parse_from([
            "hyphae",
            "hardware",
            "governor-policy",
            "--calibration",
            "receipt.json",
        ]);
        assert!(matches!(
            cli.map(|value| value.command),
            Ok(Command::Hardware {
                operation: HardwareCommand::GovernorPolicy {
                    data_dir: None,
                    profile: None,
                    calibration,
                    mode: HardwareGovernorMode::Mixed,
                }
            }) if calibration == Path::new("receipt.json")
        ));
    }

    #[test]
    fn hardware_execution_topology_defaults_to_mixed_mode() {
        let cli = Cli::try_parse_from([
            "hyphae",
            "hardware",
            "execution-topology",
            "--calibration",
            "receipt.json",
        ]);
        assert!(matches!(
            cli.map(|value| value.command),
            Ok(Command::Hardware {
                operation: HardwareCommand::ExecutionTopology {
                    data_dir: None,
                    profile: None,
                    calibration,
                    mode: HardwareGovernorMode::Mixed,
                }
            }) if calibration == Path::new("receipt.json")
        ));
    }

    #[test]
    fn hardware_policy_profile_conflicts_with_live_discovery() {
        let cli = Cli::try_parse_from([
            "hyphae",
            "hardware",
            "governor-policy",
            "--data-dir",
            "data",
            "--profile",
            "profile.json",
            "--calibration",
            "receipt.json",
        ]);
        assert!(cli.is_err());
    }
}
