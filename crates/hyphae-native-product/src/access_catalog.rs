// SPDX-License-Identifier: Apache-2.0

//! Canonical durable access-control catalog state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    ops::Bound::{Excluded, Unbounded},
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use hyphae_native_types::{ObjectId, TransactionId};
use subtle::ConstantTimeEq;

use crate::{
    AccessControlLimits, ApiKeyConfirmationDigest, ApiKeyId, ApiKeySecretDelivery, ApiKeyVerifier,
    AuthorizationEpoch, BuiltInRole, IssuedApiKey, NativeProduct, ProductAuthorization,
    ProductCommitReceipt, ProductDurability, ProductError, ProductErrorCode, ProductPermission,
    ProductPrincipal, ProductScope, SecurityId,
};

const CATALOG_V1_MAGIC: &[u8; 8] = b"HYACAT01";
const CATALOG_V2_MAGIC: &[u8; 8] = b"HYACAT02";
const CATALOG_V3_MAGIC: &[u8; 8] = b"HYACAT03";
const CATALOG_V4_MAGIC: &[u8; 8] = b"HYACAT04";
const CATALOG_MAGIC: &[u8; 8] = b"HYACAT05";
const CATALOG_DIGEST_BYTES: usize = 32;
const MAX_ACCESS_CATALOG_BYTES: usize = 16 * 1024 * 1024;
/// Maximum redacted records returned by one security metadata list.
pub const MAX_SECURITY_LIST_ROWS: usize = AccessControlLimits::V1.security_result_rows;
const PRODUCT_RESPONSE_ENVELOPE_BYTES: usize = 16;
const SECURITY_METADATA_PAGE_HEADER_BYTES: usize = 56;
const SECURITY_AUDIT_PAGE_HEADER_BYTES: usize = 32;
const BUILT_IN_ROLES: [BuiltInRole; 7] = [
    BuiltInRole::Owner,
    BuiltInRole::Admin,
    BuiltInRole::Operator,
    BuiltInRole::Developer,
    BuiltInRole::Writer,
    BuiltInRole::Reader,
    BuiltInRole::Auditor,
];
const ACCESS_CONTROL_STORAGE_KEY: &[u8] = b"\0hyphae.product.access-control.v1\0catalog";
const AUDIT_EVENT_MAGIC: &[u8; 8] = b"HYAEVT01";
const AUDIT_EVENT_STORAGE_PREFIX: &[u8] = b"\0hyphae.product.access-control.v1\0audit\0";
const SECURITY_MUTATION_MARKER_V1_MAGIC: &[u8; 8] = b"HYASID01";
const SECURITY_MUTATION_MARKER_V2_MAGIC: &[u8; 8] = b"HYASID02";
const SECURITY_MUTATION_MARKER_MAGIC: &[u8; 8] = b"HYASID03";
const SECURITY_MUTATION_INDEX_MAGIC: &[u8; 8] = b"HYASIX01";
const SECURITY_MUTATION_MARKER_PREFIX: &[u8] =
    b"\0hyphae.product.access-control.v1\0idempotency\0marker\0";
const SECURITY_MUTATION_INDEX_PREFIX: &[u8] =
    b"\0hyphae.product.access-control.v1\0idempotency\0index\0";
const SECURITY_MUTATION_IDEMPOTENCY_SHARDS: u8 = 64;
const SECURITY_MUTATION_MARKERS_PER_SHARD: usize = 64;
const SECURITY_MUTATION_MARKER_V1_BYTES: usize = 145;
const SECURITY_MUTATION_MARKER_V2_BYTES: usize = 146;
const SECURITY_MUTATION_MARKER_BYTES: usize = 171;
const SECURITY_MUTATION_REQUEST_DOMAIN: &[u8] = b"hyphae-security-mutation-request-v1\0";
const SECURITY_MUTATION_KEY_DOMAIN: &[u8] = b"hyphae-security-mutation-key-v1\0";
const OFFLINE_OWNER_RECOVERY_MARKER_MAGIC: &[u8; 8] = b"HYAORC01";
const OFFLINE_OWNER_RECOVERY_MARKER_PREFIX: &[u8] =
    b"\0hyphae.product.access-control.v1\0offline-owner-recovery\0";
const OFFLINE_OWNER_RECOVERY_MARKER_BYTES: usize = 105;
const LEGACY_BEARER_MIGRATION_MARKER_MAGIC: &[u8; 8] = b"HYLBRM01";
const LEGACY_BEARER_MIGRATION_MARKER_KEY: &[u8] =
    b"\0hyphae.product.access-control.v1\0legacy-bearer-migration\0activated";
const LEGACY_BEARER_MIGRATION_MARKER_BYTES: usize = 136;
const LEGACY_BEARER_MIGRATION_REQUEST_DOMAIN: &[u8] =
    b"hyphae-legacy-bearer-migration-request-v1\0";

/// Durable principal metadata. Display names are never authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityPrincipalRecord {
    id: SecurityId,
    display_name: Box<str>,
    enabled: bool,
}

impl SecurityPrincipalRecord {
    /// Returns the stable principal identity.
    pub const fn id(&self) -> SecurityId {
        self.id
    }

    /// Returns the mutable display name.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns whether authentication is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

/// One direct immutable built-in role assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInRoleAssignment {
    id: SecurityId,
    principal_id: SecurityId,
    role: BuiltInRole,
    scope: ProductScope,
}

/// One direct custom-role permission grant at one stable scope.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CustomRoleGrant {
    permission: ProductPermission,
    scope: ProductScope,
}

impl CustomRoleGrant {
    /// Constructs one valid direct grant.
    ///
    /// Ownership authority is reserved to the immutable `owner` role.
    pub fn new(permission: ProductPermission, scope: ProductScope) -> Option<Self> {
        (permission != ProductPermission::OwnershipManage && permission.supports_scope(scope))
            .then_some(Self { permission, scope })
    }

    /// Returns the granted permission.
    pub const fn permission(self) -> ProductPermission {
        self.permission
    }

    /// Returns the stable grant scope.
    pub const fn scope(self) -> ProductScope {
        self.scope
    }
}

/// One immutable custom role containing canonical direct grants only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomRoleRecord {
    id: SecurityId,
    display_name: Box<str>,
    grants: Box<[CustomRoleGrant]>,
}

impl CustomRoleRecord {
    /// Returns the stable custom-role identity.
    pub const fn id(&self) -> SecurityId {
        self.id
    }

    /// Returns the non-authoritative role display name.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns the canonical direct grants.
    pub fn grants(&self) -> &[CustomRoleGrant] {
        &self.grants
    }
}

/// One direct custom-role assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustomRoleAssignment {
    id: SecurityId,
    principal_id: SecurityId,
    role_id: SecurityId,
}

impl CustomRoleAssignment {
    /// Returns the stable assignment identity.
    pub const fn id(self) -> SecurityId {
        self.id
    }

    /// Returns the assigned principal.
    pub const fn principal_id(self) -> SecurityId {
        self.principal_id
    }

    /// Returns the assigned custom role.
    pub const fn role_id(self) -> SecurityId {
        self.role_id
    }
}

impl BuiltInRoleAssignment {
    /// Returns the stable assignment identity.
    pub const fn id(self) -> SecurityId {
        self.id
    }

    /// Returns the assigned principal.
    pub const fn principal_id(self) -> SecurityId {
        self.principal_id
    }

    /// Returns the immutable built-in role.
    pub const fn role(self) -> BuiltInRole {
        self.role
    }

    /// Returns the stable resource scope.
    pub const fn scope(self) -> ProductScope {
        self.scope
    }
}

/// Redacted durable API-key metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKeyRecord {
    id: ApiKeyId,
    principal_id: SecurityId,
    label: Box<str>,
    verifier: ApiKeyVerifier,
    active: bool,
    roles: Box<[BuiltInRole]>,
    custom_roles: Box<[SecurityId]>,
    permission_ceiling: ProductAuthorization,
    scope_ceiling: Box<[ProductScope]>,
    created_at_micros: i64,
    expires_at_micros: Option<i64>,
    revoked: bool,
    published_epoch: AuthorizationEpoch,
    predecessor_id: Option<ApiKeyId>,
    successor_id: Option<ApiKeyId>,
    overlap_until_micros: Option<i64>,
    rotation_overlap_micros: Option<u64>,
}

impl ApiKeyRecord {
    /// Returns the public key identity.
    pub const fn id(&self) -> ApiKeyId {
        self.id
    }

    /// Returns the owning principal identity.
    pub const fn principal_id(&self) -> SecurityId {
        self.principal_id
    }

    /// Returns the bounded non-secret label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns whether the key has been durably revoked.
    pub const fn revoked(&self) -> bool {
        self.revoked
    }

    /// Returns whether the restricted output was durably activated.
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Returns the optional exclusive expiry instant.
    pub const fn expires_at_micros(&self) -> Option<i64> {
        self.expires_at_micros
    }

    /// Returns the authorization generation at key publication.
    pub const fn published_epoch(&self) -> AuthorizationEpoch {
        self.published_epoch
    }

    /// Returns the immediate predecessor for a rotated key.
    pub const fn predecessor_id(&self) -> Option<ApiKeyId> {
        self.predecessor_id
    }

    /// Returns the immediate successor when rotation has begun.
    pub const fn successor_id(&self) -> Option<ApiKeyId> {
        self.successor_id
    }

    /// Returns the predecessor's exclusive rotation-overlap deadline.
    pub const fn overlap_until_micros(&self) -> Option<i64> {
        self.overlap_until_micros
    }

    /// Returns the key-selected custom role IDs.
    pub fn custom_roles(&self) -> &[SecurityId] {
        &self.custom_roles
    }

    /// Returns the canonical credential scope ceiling.
    pub fn scope_ceiling(&self) -> &[ProductScope] {
        &self.scope_ceiling
    }
}

/// Authenticated product authority resolved from current durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedAuthority {
    principal_id: SecurityId,
    key_id: ApiKeyId,
    principal: ProductPrincipal,
    authorization: ProductAuthorization,
    authorization_epoch: AuthorizationEpoch,
    directory_lineage: [u8; 24],
    terminal_replay_only: bool,
    valid_until_micros: Option<i64>,
    effective_roles: Box<[BuiltInRole]>,
    effective_custom_roles: Box<[SecurityId]>,
    scope_ceiling: Box<[ProductScope]>,
    scoped_authorization: Box<[ScopedAuthorization]>,
    catalog_cursor_key: [u8; 32],
}

impl AuthenticatedAuthority {
    /// Returns the stable principal identity.
    pub const fn principal_id(&self) -> SecurityId {
        self.principal_id
    }

    /// Returns the public credential identity.
    pub const fn key_id(&self) -> ApiKeyId {
        self.key_id
    }

    /// Returns the transport-independent product principal.
    pub const fn principal(&self) -> &ProductPrincipal {
        &self.principal
    }

    /// Returns the effective global permission projection.
    pub const fn authorization(&self) -> ProductAuthorization {
        self.authorization
    }

    /// Returns the durable authorization generation.
    pub const fn authorization_epoch(&self) -> AuthorizationEpoch {
        self.authorization_epoch
    }

    /// Returns the native directory lineage that issued this capability.
    pub const fn directory_lineage(&self) -> [u8; 24] {
        self.directory_lineage
    }

    /// Returns the earliest expiry or rotation-overlap deadline, if finite.
    pub const fn valid_until_micros(&self) -> Option<i64> {
        self.valid_until_micros
    }

    /// Returns effective immutable built-in roles.
    pub fn effective_roles(&self) -> &[BuiltInRole] {
        &self.effective_roles
    }

    /// Returns effective custom-role identities.
    pub fn effective_custom_roles(&self) -> &[SecurityId] {
        &self.effective_custom_roles
    }

    /// Returns the credential scope ceiling.
    pub fn scope_ceiling(&self) -> &[ProductScope] {
        &self.scope_ceiling
    }

    /// Returns effective scoped authorizations.
    pub fn scoped_authorization(&self) -> &[ScopedAuthorization] {
        &self.scoped_authorization
    }

    pub(crate) const fn catalog_cursor_key(&self) -> [u8; 32] {
        self.catalog_cursor_key
    }

    /// Returns whether both the current principal grant and credential ceiling
    /// authorize one instance-scoped permission.
    #[must_use]
    pub fn allows_instance(&self, permission: ProductPermission) -> bool {
        self.scope_ceiling.contains(&ProductScope::Instance)
            && self.scoped_authorization.iter().any(|scoped| {
                scoped.scope == ProductScope::Instance && scoped.authorization.allows(permission)
            })
    }

    /// Returns whether every permission in `required` is authorized at the
    /// complete product instance.
    #[must_use]
    pub fn allows_instance_authorization(&self, required: ProductAuthorization) -> bool {
        every_required_permission(required, |permission| self.allows_instance(permission))
    }

    /// Returns whether both the current principal grant and credential ceiling
    /// authorize `permission` for one stable catalog object.
    ///
    /// The caller supplies ancestry from the immutable catalog snapshot that
    /// bound the operation. Grants and credential ceilings remain separate so
    /// subtree intersections are evaluated exactly rather than approximated or
    /// widened during authentication.
    #[must_use]
    pub fn allows_object(
        &self,
        permission: ProductPermission,
        target: ObjectId,
        is_descendant: impl Fn(ObjectId, ObjectId) -> bool,
    ) -> bool {
        if !permission.supports_scope(ProductScope::CatalogObject(target)) {
            return false;
        }
        let ceiling_allows = self
            .scope_ceiling
            .iter()
            .copied()
            .any(|scope| scope.covers_object(target, &is_descendant));
        ceiling_allows
            && self.scoped_authorization.iter().any(|scoped| {
                scoped.authorization.allows(permission)
                    && scoped.scope.covers_object(target, &is_descendant)
            })
    }

    /// Returns whether every permission in `required` is authorized for one
    /// stable catalog object in the caller-supplied immutable ancestry.
    #[must_use]
    pub fn allows_object_authorization(
        &self,
        required: ProductAuthorization,
        target: ObjectId,
        is_descendant: impl Fn(ObjectId, ObjectId) -> bool,
    ) -> bool {
        every_required_permission(required, |permission| {
            self.allows_object(permission, target, &is_descendant)
        })
    }
}

fn every_required_permission(
    required: ProductAuthorization,
    mut predicate: impl FnMut(ProductPermission) -> bool,
) -> bool {
    let mut remaining = required.bits();
    while remaining != 0 {
        let Ok(tag) = u8::try_from(remaining.trailing_zeros()) else {
            return false;
        };
        let Some(permission) = ProductPermission::from_tag(tag) else {
            return false;
        };
        if !predicate(permission) {
            return false;
        }
        remaining &= remaining - 1;
    }
    true
}

/// One effective permission set at one stable scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopedAuthorization {
    /// Stable resource boundary.
    pub scope: ProductScope,
    /// Permissions valid within the boundary.
    pub authorization: ProductAuthorization,
}

/// Canonical bounded access-control catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessControlCatalog {
    epoch: AuthorizationEpoch,
    principals: BTreeMap<SecurityId, SecurityPrincipalRecord>,
    assignments: BTreeMap<SecurityId, BuiltInRoleAssignment>,
    custom_roles: BTreeMap<SecurityId, CustomRoleRecord>,
    custom_assignments: BTreeMap<SecurityId, CustomRoleAssignment>,
    keys: BTreeMap<ApiKeyId, ApiKeyRecord>,
    audit_index: Vec<SecurityAuditIndexEntry>,
    pending_owner_recovery: Option<PendingOwnerRecovery>,
    legacy_bearer_state: LegacyBearerState,
    legacy_bearer_migration: Option<LegacyBearerMigration>,
    legacy_bearer_verifier: Option<[u8; 32]>,
}

/// Durable terminal-aware state of Native HTTP legacy-bearer compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyBearerState {
    /// No migration has ever enabled the compatibility credential.
    NeverEnabled,
    /// A canonical Owner key exists but its restricted file is not yet confirmed.
    MigrationPending,
    /// Canonical Owner and explicitly configured legacy bearer are both accepted.
    DualWindow,
    /// Legacy authentication is permanently disabled for this directory.
    Revoked,
}

impl LegacyBearerState {
    /// Returns the stable lowercase contract identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeverEnabled => "never_enabled",
            Self::MigrationPending => "migration_pending",
            Self::DualWindow => "dual_window",
            Self::Revoked => "revoked",
        }
    }

    /// Returns whether 1.2 Native HTTP may use an explicitly configured bearer.
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::MigrationPending | Self::DualWindow)
    }

    const fn tag(self) -> u8 {
        match self {
            Self::NeverEnabled => 0,
            Self::MigrationPending => 1,
            Self::DualWindow => 2,
            Self::Revoked => 3,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::NeverEnabled),
            1 => Some(Self::MigrationPending),
            2 => Some(Self::DualWindow),
            3 => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Product compatibility version used to make the one-minor window testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyBearerCompatibilityVersion {
    /// Product major version.
    pub major: u16,
    /// Product minor version.
    pub minor: u16,
}

impl LegacyBearerCompatibilityVersion {
    /// The sole release line allowed to authenticate a migrated bearer.
    pub const V1_2: Self = Self { major: 1, minor: 2 };

    /// Returns whether this version is exactly the bounded compatibility line.
    pub const fn permits_authentication(self) -> bool {
        self.major == Self::V1_2.major && self.minor == Self::V1_2.minor
    }
}

/// Compatibility version used by this implementation.
pub const LEGACY_BEARER_COMPATIBILITY_VERSION: LegacyBearerCompatibilityVersion =
    LegacyBearerCompatibilityVersion::V1_2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LegacyBearerVerifier([u8; 32]);

impl LegacyBearerVerifier {
    pub(crate) fn verifies(self, candidate: &[u8], product_key: [u8; 32]) -> bool {
        let Ok(candidate) = legacy_bearer_verifier(candidate, product_key) else {
            return false;
        };
        bool::from(self.0.ct_eq(&candidate))
    }

    fn verifies_digest(self, candidate: [u8; 32], product_key: [u8; 32]) -> bool {
        let candidate = legacy_bearer_verifier_from_digest(candidate, product_key);
        bool::from(self.0.ct_eq(&candidate))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LegacyBearerMigration {
    operation_id: SecurityId,
    key_id: ApiKeyId,
    authorization_epoch: AuthorizationEpoch,
    created_at_micros: i64,
    request_digest: [u8; 32],
}

/// Redacted durable legacy-bearer migration state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyBearerMigrationInspection {
    /// Terminal-aware directory state.
    pub state: LegacyBearerState,
    /// Current managed authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Stable offline migration identity, when migration metadata is retained.
    pub operation_id: Option<SecurityId>,
    /// Public canonical Owner key identity, when migration metadata is retained.
    pub key_id: Option<ApiKeyId>,
    /// Phase-one authorization generation, when migration metadata is retained.
    pub migration_authorization_epoch: Option<AuthorizationEpoch>,
    /// Durable phase-one creation instant, when migration metadata is retained.
    pub created_at_micros: Option<i64>,
}

/// Durable provenance for one and only one pending offline owner recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingOwnerRecovery {
    operation_id: SecurityId,
    key_id: ApiKeyId,
    authorization_epoch: AuthorizationEpoch,
    created_at_micros: i64,
    provenance: OwnerRecoveryProvenance,
}

/// Authority source recorded for pending owner recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerRecoveryProvenance {
    /// Exclusive offline data-directory ownership validated by the OS.
    OfflineOperatingSystemOwner,
}

impl PendingOwnerRecovery {
    /// Returns the stable offline operation identity.
    pub const fn operation_id(self) -> SecurityId {
        self.operation_id
    }

    /// Returns the public replacement key identity.
    pub const fn key_id(self) -> ApiKeyId {
        self.key_id
    }

    /// Returns the generation committed by recovery phase one.
    pub const fn authorization_epoch(self) -> AuthorizationEpoch {
        self.authorization_epoch
    }

    /// Returns the durable phase-one creation instant.
    pub const fn created_at_micros(self) -> i64 {
        self.created_at_micros
    }

    /// Returns the authority source that created this pending operation.
    pub const fn provenance(self) -> OwnerRecoveryProvenance {
        self.provenance
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SecurityAuditIndexEntry {
    id: SecurityId,
    commit_csn: u64,
}

/// Stable security mutation kind retained in the durable audit trail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurityAuditAction {
    /// Initial offline owner bootstrap.
    BootstrapOwner,
    /// Restricted-file key activation.
    ActivateKey,
    /// Principal creation.
    CreatePrincipal,
    /// Custom-role creation.
    CreateCustomRole,
    /// Built-in role assignment.
    AssignBuiltInRole,
    /// Custom-role assignment.
    AssignCustomRole,
    /// API-key issue.
    IssueKey,
    /// API-key rotation.
    RotateKey,
    /// Explicit cancellation of one inactive rotation successor.
    AbortKeyRotation,
    /// Explicit cancellation of one inactive issued key.
    AbortKeyIssue,
    /// API-key revocation.
    RevokeKey,
    /// Offline owner recovery.
    RecoverOwner,
    /// Explicit legacy-bearer migration.
    MigrateLegacyBearer,
    /// Principal authentication-state change.
    SetPrincipalEnabled,
    /// Direct role-assignment revocation.
    RevokeAssignment,
    /// Explicit terminal legacy-bearer revocation.
    RevokeLegacyBearer,
}

impl SecurityAuditAction {
    /// Returns the stable append-only wire tag.
    pub const fn tag(self) -> u8 {
        match self {
            Self::BootstrapOwner => 0,
            Self::ActivateKey => 1,
            Self::CreatePrincipal => 2,
            Self::CreateCustomRole => 3,
            Self::AssignBuiltInRole => 4,
            Self::AssignCustomRole => 5,
            Self::IssueKey => 6,
            Self::RotateKey => 7,
            Self::RevokeKey => 8,
            Self::RecoverOwner => 9,
            Self::MigrateLegacyBearer => 10,
            Self::AbortKeyRotation => 11,
            Self::AbortKeyIssue => 12,
            Self::SetPrincipalEnabled => 13,
            Self::RevokeAssignment => 14,
            Self::RevokeLegacyBearer => 15,
        }
    }

    /// Reconstructs one audit action from its stable wire tag.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::BootstrapOwner),
            1 => Some(Self::ActivateKey),
            2 => Some(Self::CreatePrincipal),
            3 => Some(Self::CreateCustomRole),
            4 => Some(Self::AssignBuiltInRole),
            5 => Some(Self::AssignCustomRole),
            6 => Some(Self::IssueKey),
            7 => Some(Self::RotateKey),
            8 => Some(Self::RevokeKey),
            9 => Some(Self::RecoverOwner),
            10 => Some(Self::MigrateLegacyBearer),
            11 => Some(Self::AbortKeyRotation),
            12 => Some(Self::AbortKeyIssue),
            13 => Some(Self::SetPrincipalEnabled),
            14 => Some(Self::RevokeAssignment),
            15 => Some(Self::RevokeLegacyBearer),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecurityMutationOperation {
    CreatePrincipal,
    SetPrincipalEnabled,
    CreateCustomRole,
    AssignBuiltInRole,
    AssignCustomRole,
    RevokeAssignment,
    IssueKeyStart,
    IssueKeyActivate,
    RotateKeyStart,
    RotateKeyActivate,
    AbortKeyIssue,
    AbortKeyRotation,
    RevokeKey,
    RevokeLegacyBearer,
}

impl SecurityMutationOperation {
    const fn tag(self) -> u8 {
        match self {
            Self::CreatePrincipal => 0,
            Self::SetPrincipalEnabled => 1,
            Self::CreateCustomRole => 2,
            Self::AssignBuiltInRole => 3,
            Self::AssignCustomRole => 4,
            Self::RevokeAssignment => 5,
            Self::IssueKeyStart => 6,
            Self::IssueKeyActivate => 7,
            Self::RotateKeyStart => 8,
            Self::RotateKeyActivate => 9,
            Self::AbortKeyIssue => 10,
            Self::AbortKeyRotation => 11,
            Self::RevokeKey => 12,
            Self::RevokeLegacyBearer => 13,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::CreatePrincipal),
            1 => Some(Self::SetPrincipalEnabled),
            2 => Some(Self::CreateCustomRole),
            3 => Some(Self::AssignBuiltInRole),
            4 => Some(Self::AssignCustomRole),
            5 => Some(Self::RevokeAssignment),
            6 => Some(Self::IssueKeyStart),
            7 => Some(Self::IssueKeyActivate),
            8 => Some(Self::RotateKeyStart),
            9 => Some(Self::RotateKeyActivate),
            10 => Some(Self::AbortKeyIssue),
            11 => Some(Self::AbortKeyRotation),
            12 => Some(Self::RevokeKey),
            13 => Some(Self::RevokeLegacyBearer),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecurityMutationResultId {
    None,
    Security(SecurityId),
    ApiKey(ApiKeyId),
}

impl SecurityMutationResultId {
    const fn security_id(self) -> Option<SecurityId> {
        match self {
            Self::Security(id) => Some(id),
            Self::None | Self::ApiKey(_) => None,
        }
    }

    const fn api_key_id(self) -> Option<ApiKeyId> {
        match self {
            Self::ApiKey(id) => Some(id),
            Self::None | Self::Security(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SecurityMutationMarker {
    operation: SecurityMutationOperation,
    request_digest: [u8; 32],
    actor_principal_id: SecurityId,
    actor_key_id: ApiKeyId,
    result_id: SecurityMutationResultId,
    predecessor_key_id: Option<ApiKeyId>,
    overlap_until_micros: Option<i64>,
    authorization_epoch: AuthorizationEpoch,
    transaction_id: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecurityMutationMarkerIndex {
    fingerprints: Vec<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SecurityMutationDraft {
    operation: SecurityMutationOperation,
    request_digest: [u8; 32],
    actor_principal_id: SecurityId,
    actor_key_id: ApiKeyId,
    result_id: SecurityMutationResultId,
    predecessor_key_id: Option<ApiKeyId>,
    overlap_until_micros: Option<i64>,
    authorization_epoch: AuthorizationEpoch,
    fingerprint: [u8; 32],
}

impl SecurityMutationDraft {
    fn new(
        operation: SecurityMutationOperation,
        request_digest: [u8; 32],
        actor: &AuthenticatedAuthority,
        idempotency_token: u128,
        result_id: SecurityMutationResultId,
        authorization_epoch: AuthorizationEpoch,
    ) -> Result<Self, ProductError> {
        if idempotency_token == 0 {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        Ok(Self {
            operation,
            request_digest,
            actor_principal_id: actor.principal_id,
            actor_key_id: actor.key_id,
            result_id,
            predecessor_key_id: None,
            overlap_until_micros: None,
            authorization_epoch,
            fingerprint: security_mutation_fingerprint(actor, idempotency_token),
        })
    }

    fn with_activation(
        mut self,
        predecessor_key_id: Option<ApiKeyId>,
        overlap_until_micros: Option<i64>,
    ) -> Self {
        self.predecessor_key_id = predecessor_key_id;
        self.overlap_until_micros = overlap_until_micros;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SecurityMutationReplay {
    result_id: SecurityMutationResultId,
    predecessor_key_id: Option<ApiKeyId>,
    overlap_until_micros: Option<i64>,
    authorization_epoch: AuthorizationEpoch,
    commit: ProductCommitReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OfflineOwnerRecoveryOutcome {
    Activated,
    Aborted,
}

impl OfflineOwnerRecoveryOutcome {
    const fn tag(self) -> u8 {
        match self {
            Self::Activated => 1,
            Self::Aborted => 2,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Activated),
            2 => Some(Self::Aborted),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OfflineOwnerRecoveryMarker {
    outcome: OfflineOwnerRecoveryOutcome,
    operation_id: SecurityId,
    key_id: ApiKeyId,
    expected_authorization_epoch: AuthorizationEpoch,
    result_authorization_epoch: AuthorizationEpoch,
    transaction_id: u128,
}

/// Redacted public target in one security audit event.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SecurityAuditTarget {
    /// Stable principal identity.
    Principal(SecurityId),
    /// Stable custom-role identity.
    Role(SecurityId),
    /// Stable assignment identity.
    Assignment(SecurityId),
    /// Public API-key identity.
    Key(ApiKeyId),
    /// The directory-scoped synthetic compatibility authority.
    LegacyBearer,
}

/// Typed redacted metadata retained by a security audit event.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SecurityAuditMetadata {
    /// Exclusive key expiry instant.
    ExpiresAtMicros(i64),
    /// Exclusive immediate-predecessor overlap deadline.
    RotationOverlapUntilMicros(i64),
}

/// Durable result of a security audit event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurityAuditResult {
    /// The mutation and its event committed atomically.
    Succeeded,
}

/// One append-only durable security mutation event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityAuditEvent {
    id: SecurityId,
    commit_csn: u64,
    actor_principal_id: Option<SecurityId>,
    actor_key_id: Option<ApiKeyId>,
    action: SecurityAuditAction,
    result: SecurityAuditResult,
    targets: Box<[SecurityAuditTarget]>,
    metadata: Box<[SecurityAuditMetadata]>,
}

impl SecurityAuditEvent {
    /// Reconstructs one bounded canonical redacted event from wire fields.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero CSN, unpaired actor identities, empty or
    /// noncanonical targets, noncanonical metadata, or oversized output.
    #[allow(clippy::too_many_arguments)]
    pub fn try_from_wire(
        id: SecurityId,
        commit_csn: u64,
        actor_principal_id: Option<SecurityId>,
        actor_key_id: Option<ApiKeyId>,
        action: SecurityAuditAction,
        result: SecurityAuditResult,
        targets: Vec<SecurityAuditTarget>,
        metadata: Vec<SecurityAuditMetadata>,
    ) -> Result<Self, AccessCatalogError> {
        if commit_csn == 0
            || actor_principal_id.is_some() != actor_key_id.is_some()
            || targets.is_empty()
            || !strictly_sorted(&targets)
            || !strictly_sorted(&metadata)
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let event = Self {
            id,
            commit_csn,
            actor_principal_id,
            actor_key_id,
            action,
            result,
            targets: targets.into_boxed_slice(),
            metadata: metadata.into_boxed_slice(),
        };
        encode_audit_event(&event)?;
        Ok(event)
    }

    /// Returns the stable event identity.
    pub const fn id(&self) -> SecurityId {
        self.id
    }

    /// Returns the exact native commit sequence containing the event.
    pub const fn commit_csn(&self) -> u64 {
        self.commit_csn
    }

    /// Returns the authenticated actor principal, absent for offline actions.
    pub const fn actor_principal_id(&self) -> Option<SecurityId> {
        self.actor_principal_id
    }

    /// Returns the public actor key, absent for offline actions.
    pub const fn actor_key_id(&self) -> Option<ApiKeyId> {
        self.actor_key_id
    }

    /// Returns the stable mutation action.
    pub const fn action(&self) -> SecurityAuditAction {
        self.action
    }

    /// Returns the durable mutation result.
    pub const fn result(&self) -> SecurityAuditResult {
        self.result
    }

    /// Returns the sorted redacted public targets.
    pub fn targets(&self) -> &[SecurityAuditTarget] {
        &self.targets
    }

    /// Returns typed redacted metadata only.
    pub fn metadata(&self) -> &[SecurityAuditMetadata] {
        &self.metadata
    }

    /// Returns a saturation-safe upper bound for the canonical Native event.
    pub fn encoded_size_bound(&self) -> usize {
        88_usize
            .saturating_add(self.targets.len().saturating_mul(24))
            .saturating_add(self.metadata.len().saturating_mul(16))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecurityAuditDraft {
    actor_principal_id: Option<SecurityId>,
    actor_key_id: Option<ApiKeyId>,
    action: SecurityAuditAction,
    targets: Vec<SecurityAuditTarget>,
    metadata: Vec<SecurityAuditMetadata>,
}

struct SecurityAuditAppend {
    event: SecurityAuditEvent,
    evicted: Option<SecurityAuditIndexEntry>,
}

type OwnerRecoveryStart = (
    SecurityId,
    SecurityId,
    IssuedApiKey,
    AuthorizationEpoch,
    Box<[ApiKeyId]>,
);

/// Definite result of legacy-bearer migration phase one.
#[derive(Debug)]
pub struct LegacyBearerMigrationStartReceipt {
    /// Stable canonical Owner principal.
    pub principal_id: SecurityId,
    /// Stable migration operation identity.
    pub operation_id: SecurityId,
    /// Public identity of the inactive canonical Owner key.
    pub key_id: ApiKeyId,
    /// Authorization generation after publishing pending migration state.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict phase-one commit.
    pub commit: ProductCommitReceipt,
    /// One-time secret for a restricted output file.
    pub secret: IssuedApiKey,
}

/// Definite result of activating the legacy dual window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyBearerMigrationActivationReceipt {
    /// Stable migration operation identity.
    pub operation_id: SecurityId,
    /// Activated canonical Owner key identity.
    pub key_id: ApiKeyId,
    /// Authorization generation after activation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict activation commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of terminal legacy-bearer revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyBearerRevocationReceipt {
    /// Durable authorization generation after revocation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict terminal commit.
    pub commit: ProductCommitReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LegacyBearerMigrationMarker {
    operation_id: SecurityId,
    key_id: ApiKeyId,
    expected_authorization_epoch: AuthorizationEpoch,
    result_authorization_epoch: AuthorizationEpoch,
    request_digest: [u8; 32],
    transaction_id: u128,
}

impl SecurityAuditDraft {
    fn offline(
        action: SecurityAuditAction,
        targets: impl IntoIterator<Item = SecurityAuditTarget>,
    ) -> Self {
        Self {
            actor_principal_id: None,
            actor_key_id: None,
            action,
            targets: targets.into_iter().collect(),
            metadata: Vec::new(),
        }
    }

    fn actor(
        actor: &AuthenticatedAuthority,
        action: SecurityAuditAction,
        targets: impl IntoIterator<Item = SecurityAuditTarget>,
    ) -> Self {
        Self {
            actor_principal_id: Some(actor.principal_id),
            actor_key_id: Some(actor.key_id),
            action,
            targets: targets.into_iter().collect(),
            metadata: Vec::new(),
        }
    }

    fn with_metadata(mut self, metadata: impl IntoIterator<Item = SecurityAuditMetadata>) -> Self {
        self.metadata = metadata.into_iter().collect();
        self
    }
}

/// Redacted bounded status for the durable access-control catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessControlStatus {
    /// Whether at least one principal exists.
    pub bootstrapped: bool,
    /// Current authorization generation.
    pub epoch: AuthorizationEpoch,
    /// Durable principal records.
    pub principals: usize,
    /// Durable built-in role assignments.
    pub assignments: usize,
    /// Durable custom-role definitions.
    pub custom_roles: usize,
    /// Durable custom-role assignments.
    pub custom_assignments: usize,
    /// Durable key records, including revoked metadata.
    pub keys: usize,
    /// Key records awaiting restricted-output activation.
    pub pending_keys: usize,
    /// Retained append-only security audit events.
    pub audit_events: usize,
}

impl AccessControlStatus {
    /// Validates that the aggregate can represent one canonical access-control
    /// catalog snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an inconsistent bootstrap generation or any count
    /// outside the immutable access-control bounds.
    pub fn validate(&self) -> Result<(), AccessCatalogError> {
        let limits = AccessControlLimits::V1;
        let empty = self.principals == 0
            && self.assignments == 0
            && self.custom_roles == 0
            && self.custom_assignments == 0
            && self.keys == 0
            && self.pending_keys == 0
            && self.audit_events == 0;
        let valid_generation = if self.bootstrapped {
            self.epoch != AuthorizationEpoch::UNMANAGED
                && self.principals > 0
                && self.assignments > 0
        } else {
            self.epoch == AuthorizationEpoch::UNMANAGED && empty
        };
        let maximum_assignments = self
            .principals
            .saturating_mul(limits.assignments_per_principal);
        if !valid_generation
            || self.principals > limits.principals
            || self.assignments.saturating_add(self.custom_assignments) > maximum_assignments
            || self.custom_roles > limits.custom_roles
            || self.keys > self.principals.saturating_mul(limits.keys_per_principal)
            || self.pending_keys > self.keys
            || self.audit_events > limits.retained_audit_events
        {
            Err(AccessCatalogError::InvalidRequest)
        } else {
            Ok(())
        }
    }

    /// Returns a saturation-safe upper bound for the canonical Native response.
    pub const fn encoded_size_bound() -> usize {
        PRODUCT_RESPONSE_ENVELOPE_BYTES + 72
    }
}

/// Typed exclusive continuation retained by one security metadata cursor.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SecurityCursorId {
    /// Stable principal continuation.
    Principal(SecurityId),
    /// Immutable built-in role continuation.
    BuiltInRole(BuiltInRole),
    /// Stable custom-role continuation.
    CustomRole(SecurityId),
    /// Stable role-assignment continuation.
    Assignment(SecurityId),
    /// Public API-key continuation.
    Key(ApiKeyId),
}

/// Opaque security metadata cursor bound to one authorization generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecurityCursor {
    authorization_epoch: AuthorizationEpoch,
    after_id: SecurityCursorId,
}

impl SecurityCursor {
    /// Reconstructs one typed cursor from a previously returned generation and
    /// exclusive continuation.
    pub const fn new(authorization_epoch: AuthorizationEpoch, after_id: SecurityCursorId) -> Self {
        Self {
            authorization_epoch,
            after_id,
        }
    }

    /// Returns the exact authorization generation required by this cursor.
    pub const fn authorization_epoch(self) -> AuthorizationEpoch {
        self.authorization_epoch
    }

    /// Returns the typed exclusive continuation.
    pub const fn after_id(self) -> SecurityCursorId {
        self.after_id
    }

    /// Encodes this cursor as one bounded opaque command-line token.
    pub fn to_token(self) -> String {
        let epoch = self.authorization_epoch.get();
        match self.after_id {
            SecurityCursorId::Principal(id) => format!("hysec1:{epoch}:principal:{id}"),
            SecurityCursorId::BuiltInRole(role) => {
                format!("hysec1:{epoch}:built-in-role:{role}")
            }
            SecurityCursorId::CustomRole(id) => {
                format!("hysec1:{epoch}:custom-role:{id}")
            }
            SecurityCursorId::Assignment(id) => format!("hysec1:{epoch}:assignment:{id}"),
            SecurityCursorId::Key(id) => format!("hysec1:{epoch}:key:{id}"),
        }
    }

    /// Decodes one canonical token previously returned by [`Self::to_token`].
    ///
    /// # Errors
    ///
    /// Returns [`AccessCatalogError::InvalidRequest`] for an unknown version,
    /// zero generation, unknown cursor kind, noncanonical identity, or any
    /// missing or trailing field.
    pub fn from_token(token: &str) -> Result<Self, AccessCatalogError> {
        let mut fields = token.split(':');
        let version = fields.next();
        let epoch = fields.next().and_then(|value| value.parse::<u64>().ok());
        let kind = fields.next();
        let value = fields.next();
        if version != Some("hysec1") || epoch == Some(0) || fields.next().is_some() {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let epoch = AuthorizationEpoch::new(epoch.ok_or(AccessCatalogError::InvalidRequest)?);
        let value = value.ok_or(AccessCatalogError::InvalidRequest)?;
        let after_id = match kind {
            Some("principal") => SecurityCursorId::Principal(
                value
                    .parse()
                    .map_err(|_| AccessCatalogError::InvalidRequest)?,
            ),
            Some("built-in-role") => SecurityCursorId::BuiltInRole(
                BuiltInRole::parse(value).ok_or(AccessCatalogError::InvalidRequest)?,
            ),
            Some("custom-role") => SecurityCursorId::CustomRole(
                value
                    .parse()
                    .map_err(|_| AccessCatalogError::InvalidRequest)?,
            ),
            Some("assignment") => SecurityCursorId::Assignment(
                value
                    .parse()
                    .map_err(|_| AccessCatalogError::InvalidRequest)?,
            ),
            Some("key") => SecurityCursorId::Key(
                value
                    .parse()
                    .map_err(|_| AccessCatalogError::InvalidRequest)?,
            ),
            _ => return Err(AccessCatalogError::InvalidRequest),
        };
        let cursor = Self::new(epoch, after_id);
        if cursor.to_token() != token {
            return Err(AccessCatalogError::InvalidRequest);
        }
        Ok(cursor)
    }
}

macro_rules! security_list_request {
    ($name:ident) => {
        /// One bounded security metadata list request.
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name {
            /// Optional exclusive cursor returned by the same list operation.
            pub cursor: Option<SecurityCursor>,
            /// Maximum redacted records to return.
            pub limit: usize,
        }

        impl $name {
            /// Constructs one bounded list request.
            ///
            /// # Errors
            ///
            /// Returns an error when `limit` is zero or exceeds the fixed
            /// security metadata result bound.
            pub fn new(
                cursor: Option<SecurityCursor>,
                limit: usize,
            ) -> Result<Self, AccessCatalogError> {
                let request = Self { cursor, limit };
                request.validate()?;
                Ok(request)
            }

            /// Validates the fixed result bound for this request.
            ///
            /// # Errors
            ///
            /// Returns an error when `limit` is zero or exceeds the fixed
            /// security metadata result bound.
            pub fn validate(self) -> Result<(), AccessCatalogError> {
                validate_security_list_limit(self.limit)
            }

            /// Returns the optional exclusive cursor.
            pub const fn cursor(self) -> Option<SecurityCursor> {
                self.cursor
            }

            /// Returns the maximum records requested.
            pub const fn limit(self) -> usize {
                self.limit
            }
        }
    };
}

security_list_request!(SecurityPrincipalListRequest);
security_list_request!(SecurityRoleListRequest);
security_list_request!(SecurityAssignmentListRequest);
security_list_request!(SecurityKeyListRequest);

/// One bounded security-audit request retaining its v1 event cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecurityAuditReadRequest {
    /// Optional exclusive retained-event identity.
    pub cursor: Option<SecurityId>,
    /// Maximum retained events to return.
    pub limit: usize,
}

impl SecurityAuditReadRequest {
    /// Constructs one bounded security-audit request.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is zero or exceeds the audit result
    /// bound.
    pub fn new(cursor: Option<SecurityId>, limit: usize) -> Result<Self, AccessCatalogError> {
        let request = Self { cursor, limit };
        request.validate()?;
        Ok(request)
    }

    /// Validates the fixed result bound for this request.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is zero or exceeds the audit result
    /// bound.
    pub fn validate(self) -> Result<(), AccessCatalogError> {
        if self.limit == 0 || self.limit > AccessControlLimits::V1.audit_result_rows {
            Err(AccessCatalogError::InvalidRequest)
        } else {
            Ok(())
        }
    }

    /// Returns the optional exclusive retained-event cursor.
    pub const fn cursor(self) -> Option<SecurityId> {
        self.cursor
    }

    /// Returns the maximum events requested.
    pub const fn limit(self) -> usize {
        self.limit
    }
}

/// Redacted principal metadata returned by the security read plane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityPrincipalSummary {
    id: SecurityId,
    display_name: Box<str>,
    enabled: bool,
}

impl SecurityPrincipalSummary {
    /// Reconstructs one redacted principal summary from wire fields.
    ///
    /// # Errors
    ///
    /// Returns an error when the display name violates the bounded canonical
    /// security text contract.
    pub fn new(
        id: SecurityId,
        display_name: impl Into<Box<str>>,
        enabled: bool,
    ) -> Result<Self, AccessCatalogError> {
        let display_name = display_name.into();
        validate_display_name(&display_name)?;
        Ok(Self {
            id,
            display_name,
            enabled,
        })
    }

    /// Returns the stable principal identity.
    pub const fn id(&self) -> SecurityId {
        self.id
    }

    /// Returns the non-authoritative display name.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns whether the principal can authenticate.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns a saturation-safe upper bound for the canonical Native item.
    pub fn encoded_size_bound(&self) -> usize {
        28_usize.saturating_add(self.display_name.len())
    }
}

impl From<&SecurityPrincipalRecord> for SecurityPrincipalSummary {
    fn from(record: &SecurityPrincipalRecord) -> Self {
        Self {
            id: record.id,
            display_name: record.display_name.clone(),
            enabled: record.enabled,
        }
    }
}

/// Redacted immutable or custom role metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityRoleSummary {
    kind: SecurityRoleSummaryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SecurityRoleSummaryKind {
    BuiltIn(BuiltInRole),
    Custom {
        id: SecurityId,
        display_name: Box<str>,
        grants: Box<[CustomRoleGrant]>,
    },
}

impl SecurityRoleSummary {
    /// Constructs one immutable built-in role summary.
    pub const fn built_in(role: BuiltInRole) -> Self {
        Self {
            kind: SecurityRoleSummaryKind::BuiltIn(role),
        }
    }

    /// Constructs one validated custom-role summary from wire fields.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or reserved display text, empty or
    /// oversized grants, or noncanonical grant order.
    pub fn custom(
        id: SecurityId,
        display_name: impl Into<Box<str>>,
        grants: Vec<CustomRoleGrant>,
    ) -> Result<Self, AccessCatalogError> {
        let display_name = display_name.into();
        validate_display_name(&display_name)?;
        if BuiltInRole::parse(&display_name.to_ascii_lowercase()).is_some()
            || grants.is_empty()
            || grants.len() > AccessControlLimits::V1.grants_per_role
            || !strictly_sorted(&grants)
            || grants.iter().any(|grant| {
                grant.permission == ProductPermission::OwnershipManage
                    || !grant.permission.supports_scope(grant.scope)
            })
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        Ok(Self {
            kind: SecurityRoleSummaryKind::Custom {
                id,
                display_name,
                grants: grants.into_boxed_slice(),
            },
        })
    }

    /// Returns the built-in role, when this is immutable metadata.
    pub const fn built_in_role(&self) -> Option<BuiltInRole> {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(role) => Some(*role),
            SecurityRoleSummaryKind::Custom { .. } => None,
        }
    }

    /// Returns the stable custom-role identity, when applicable.
    pub const fn custom_role_id(&self) -> Option<SecurityId> {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(_) => None,
            SecurityRoleSummaryKind::Custom { id, .. } => Some(*id),
        }
    }

    /// Returns the stable display name.
    pub fn display_name(&self) -> &str {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(role) => role.as_str(),
            SecurityRoleSummaryKind::Custom { display_name, .. } => display_name,
        }
    }

    /// Returns direct custom grants. Built-in grants remain represented by
    /// the returned [`BuiltInRole`] and therefore yield an empty slice here.
    pub fn grants(&self) -> &[CustomRoleGrant] {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(_) => &[],
            SecurityRoleSummaryKind::Custom { grants, .. } => grants,
        }
    }

    /// Returns a saturation-safe upper bound for the canonical Native item.
    pub fn encoded_size_bound(&self) -> usize {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(_) => 8,
            SecurityRoleSummaryKind::Custom {
                display_name,
                grants,
                ..
            } => 36_usize
                .saturating_add(display_name.len())
                .saturating_add(grants.len().saturating_mul(32)),
        }
    }

    fn cursor_id(&self) -> SecurityCursorId {
        match &self.kind {
            SecurityRoleSummaryKind::BuiltIn(role) => SecurityCursorId::BuiltInRole(*role),
            SecurityRoleSummaryKind::Custom { id, .. } => SecurityCursorId::CustomRole(*id),
        }
    }
}

/// Redacted direct role-assignment metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecurityAssignmentSummary {
    id: SecurityId,
    principal_id: SecurityId,
    built_in_role: Option<BuiltInRole>,
    custom_role_id: Option<SecurityId>,
    scope: Option<ProductScope>,
}

impl SecurityAssignmentSummary {
    /// Reconstructs one direct assignment summary from wire fields.
    ///
    /// # Errors
    ///
    /// Exactly one role family must be present. Built-in assignments require
    /// a scope; custom-role assignments must not carry one. Owner remains
    /// instance-scoped.
    pub fn new(
        id: SecurityId,
        principal_id: SecurityId,
        built_in_role: Option<BuiltInRole>,
        custom_role_id: Option<SecurityId>,
        scope: Option<ProductScope>,
    ) -> Result<Self, AccessCatalogError> {
        let canonical = matches!(
            (built_in_role, custom_role_id, scope),
            (Some(_), None, Some(_)) | (None, Some(_), None)
        );
        if !canonical
            || built_in_role == Some(BuiltInRole::Owner) && scope != Some(ProductScope::Instance)
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        Ok(Self {
            id,
            principal_id,
            built_in_role,
            custom_role_id,
            scope,
        })
    }

    /// Returns the stable assignment identity.
    pub const fn id(self) -> SecurityId {
        self.id
    }

    /// Returns the assigned principal identity.
    pub const fn principal_id(self) -> SecurityId {
        self.principal_id
    }

    /// Returns the assigned immutable role, when applicable.
    pub const fn built_in_role(self) -> Option<BuiltInRole> {
        self.built_in_role
    }

    /// Returns the assigned custom-role identity, when applicable.
    pub const fn custom_role_id(self) -> Option<SecurityId> {
        self.custom_role_id
    }

    /// Returns the direct built-in assignment scope. Custom assignments use
    /// the grants retained by their custom role and therefore return `None`.
    pub const fn scope(self) -> Option<ProductScope> {
        self.scope
    }

    /// Returns a saturation-safe upper bound for the canonical Native item.
    pub const fn encoded_size_bound(self) -> usize {
        if self.built_in_role.is_some() { 64 } else { 56 }
    }
}

impl From<BuiltInRoleAssignment> for SecurityAssignmentSummary {
    fn from(assignment: BuiltInRoleAssignment) -> Self {
        Self {
            id: assignment.id,
            principal_id: assignment.principal_id,
            built_in_role: Some(assignment.role),
            custom_role_id: None,
            scope: Some(assignment.scope),
        }
    }
}

impl From<CustomRoleAssignment> for SecurityAssignmentSummary {
    fn from(assignment: CustomRoleAssignment) -> Self {
        Self {
            id: assignment.id,
            principal_id: assignment.principal_id,
            built_in_role: None,
            custom_role_id: Some(assignment.role_id),
            scope: None,
        }
    }
}

/// Wire-ready redacted API-key metadata input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityKeySummaryInput {
    /// Public key identity.
    pub id: ApiKeyId,
    /// Owning principal identity.
    pub principal_id: SecurityId,
    /// Bounded non-secret label.
    pub label: String,
    /// Whether restricted output activation completed durably.
    pub active: bool,
    /// Canonically sorted key-selected built-in roles.
    pub roles: Vec<BuiltInRole>,
    /// Canonically sorted key-selected custom-role identities.
    pub custom_roles: Vec<SecurityId>,
    /// Credential permission ceiling containing known bits only.
    pub permission_ceiling: ProductAuthorization,
    /// Canonically sorted credential scope ceiling.
    pub scope_ceiling: Vec<ProductScope>,
    /// Durable creation instant.
    pub created_at_micros: i64,
    /// Optional exclusive expiry instant.
    pub expires_at_micros: Option<i64>,
    /// Whether the credential was durably revoked.
    pub revoked: bool,
    /// Authorization generation at publication.
    pub published_epoch: AuthorizationEpoch,
    /// Immediate predecessor for a rotation successor.
    pub predecessor_id: Option<ApiKeyId>,
    /// Immediate successor when rotation has begun.
    pub successor_id: Option<ApiKeyId>,
    /// Predecessor's exclusive overlap deadline.
    pub overlap_until_micros: Option<i64>,
    /// Configured successor overlap duration.
    pub rotation_overlap_micros: Option<u64>,
}

/// Redacted API-key metadata. The secret and verifier are structurally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityKeySummary {
    id: ApiKeyId,
    principal_id: SecurityId,
    label: Box<str>,
    active: bool,
    roles: Box<[BuiltInRole]>,
    custom_roles: Box<[SecurityId]>,
    permission_ceiling: ProductAuthorization,
    scope_ceiling: Box<[ProductScope]>,
    created_at_micros: i64,
    expires_at_micros: Option<i64>,
    revoked: bool,
    published_epoch: AuthorizationEpoch,
    predecessor_id: Option<ApiKeyId>,
    successor_id: Option<ApiKeyId>,
    overlap_until_micros: Option<i64>,
    rotation_overlap_micros: Option<u64>,
}

impl SecurityKeySummary {
    /// Reconstructs one canonical redacted key summary from wire fields.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid labels, empty or noncanonical roles and
    /// scopes, unknown temporal generations, or incoherent rotation links.
    pub fn try_from_wire(input: SecurityKeySummaryInput) -> Result<Self, AccessCatalogError> {
        validate_display_name(&input.label)?;
        let limits = AccessControlLimits::V1;
        let selected_role_count = input.roles.len().saturating_add(input.custom_roles.len());
        let roles_are_canonical = input.roles.is_empty() || strictly_sorted(&input.roles);
        let custom_roles_are_canonical =
            input.custom_roles.is_empty() || strictly_sorted(&input.custom_roles);
        let scopes_are_canonical = !input.scope_ceiling.is_empty()
            && input.scope_ceiling.len() <= limits.assignments_per_principal
            && strictly_sorted(&input.scope_ceiling)
            && (!input.scope_ceiling.contains(&ProductScope::Instance)
                || input.scope_ceiling.len() == 1);
        let rotation_shape = matches!(
            (
                input.predecessor_id,
                input.successor_id,
                input.overlap_until_micros,
                input.rotation_overlap_micros,
            ),
            (None, None | Some(_), None, None)
                | (Some(_), None, None, Some(_))
                | (None, Some(_), Some(_), None)
        );
        if input.roles.is_empty() && input.custom_roles.is_empty()
            || selected_role_count > limits.assignments_per_principal
            || !roles_are_canonical
            || !custom_roles_are_canonical
            || !scopes_are_canonical
            || input
                .expires_at_micros
                .is_some_and(|expiry| expiry <= input.created_at_micros)
            || input.published_epoch == AuthorizationEpoch::UNMANAGED
            || input.predecessor_id == Some(input.id)
            || input.successor_id == Some(input.id)
            || input
                .overlap_until_micros
                .is_some_and(|deadline| deadline <= input.created_at_micros)
            || input.rotation_overlap_micros.is_some_and(|overlap| {
                overlap
                    > limits
                        .maximum_rotation_overlap_seconds
                        .saturating_mul(1_000_000)
            })
            || !rotation_shape
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        Ok(Self {
            id: input.id,
            principal_id: input.principal_id,
            label: input.label.into_boxed_str(),
            active: input.active,
            roles: input.roles.into_boxed_slice(),
            custom_roles: input.custom_roles.into_boxed_slice(),
            permission_ceiling: input.permission_ceiling,
            scope_ceiling: input.scope_ceiling.into_boxed_slice(),
            created_at_micros: input.created_at_micros,
            expires_at_micros: input.expires_at_micros,
            revoked: input.revoked,
            published_epoch: input.published_epoch,
            predecessor_id: input.predecessor_id,
            successor_id: input.successor_id,
            overlap_until_micros: input.overlap_until_micros,
            rotation_overlap_micros: input.rotation_overlap_micros,
        })
    }

    /// Returns the public key identity.
    pub const fn id(&self) -> ApiKeyId {
        self.id
    }

    /// Returns the owning principal identity.
    pub const fn principal_id(&self) -> SecurityId {
        self.principal_id
    }

    /// Returns the bounded non-secret label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns whether restricted output activation completed durably.
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Returns key-selected built-in roles.
    pub fn roles(&self) -> &[BuiltInRole] {
        &self.roles
    }

    /// Returns key-selected custom-role identities.
    pub fn custom_roles(&self) -> &[SecurityId] {
        &self.custom_roles
    }

    /// Returns the credential permission ceiling.
    pub const fn permission_ceiling(&self) -> ProductAuthorization {
        self.permission_ceiling
    }

    /// Returns the credential scope ceiling.
    pub fn scope_ceiling(&self) -> &[ProductScope] {
        &self.scope_ceiling
    }

    /// Returns the durable creation instant.
    pub const fn created_at_micros(&self) -> i64 {
        self.created_at_micros
    }

    /// Returns the optional exclusive expiry instant.
    pub const fn expires_at_micros(&self) -> Option<i64> {
        self.expires_at_micros
    }

    /// Returns whether the credential was durably revoked.
    pub const fn revoked(&self) -> bool {
        self.revoked
    }

    /// Returns the authorization generation at publication.
    pub const fn published_epoch(&self) -> AuthorizationEpoch {
        self.published_epoch
    }

    /// Returns the immediate predecessor for a rotation successor.
    pub const fn predecessor_id(&self) -> Option<ApiKeyId> {
        self.predecessor_id
    }

    /// Returns the immediate successor when rotation has begun.
    pub const fn successor_id(&self) -> Option<ApiKeyId> {
        self.successor_id
    }

    /// Returns the predecessor's exclusive overlap deadline.
    pub const fn overlap_until_micros(&self) -> Option<i64> {
        self.overlap_until_micros
    }

    /// Returns the configured rotation overlap duration.
    pub const fn rotation_overlap_micros(&self) -> Option<u64> {
        self.rotation_overlap_micros
    }

    /// Returns a saturation-safe upper bound for the canonical Native item.
    pub fn encoded_size_bound(&self) -> usize {
        188_usize
            .saturating_add(self.label.len())
            .saturating_add(self.roles.len())
            .saturating_add(self.custom_roles.len().saturating_mul(16))
            .saturating_add(self.scope_ceiling.len().saturating_mul(24))
    }
}

impl From<&ApiKeyRecord> for SecurityKeySummary {
    fn from(record: &ApiKeyRecord) -> Self {
        Self {
            id: record.id,
            principal_id: record.principal_id,
            label: record.label.clone(),
            active: record.active,
            roles: record.roles.clone(),
            custom_roles: record.custom_roles.clone(),
            permission_ceiling: record.permission_ceiling,
            scope_ceiling: record.scope_ceiling.clone(),
            created_at_micros: record.created_at_micros,
            expires_at_micros: record.expires_at_micros,
            revoked: record.revoked,
            published_epoch: record.published_epoch,
            predecessor_id: record.predecessor_id,
            successor_id: record.successor_id,
            overlap_until_micros: record.overlap_until_micros,
            rotation_overlap_micros: record.rotation_overlap_micros,
        }
    }
}

trait SecurityPageItem {
    fn page_cursor_id(&self) -> SecurityCursorId;

    fn valid_for_page_epoch(&self, _authorization_epoch: AuthorizationEpoch) -> bool {
        true
    }

    fn page_item_size_bound(&self) -> usize;
}

impl SecurityPageItem for SecurityPrincipalSummary {
    fn page_cursor_id(&self) -> SecurityCursorId {
        SecurityCursorId::Principal(self.id)
    }

    fn page_item_size_bound(&self) -> usize {
        self.encoded_size_bound()
    }
}

impl SecurityPageItem for SecurityRoleSummary {
    fn page_cursor_id(&self) -> SecurityCursorId {
        self.cursor_id()
    }

    fn page_item_size_bound(&self) -> usize {
        self.encoded_size_bound()
    }
}

impl SecurityPageItem for SecurityAssignmentSummary {
    fn page_cursor_id(&self) -> SecurityCursorId {
        SecurityCursorId::Assignment(self.id)
    }

    fn page_item_size_bound(&self) -> usize {
        self.encoded_size_bound()
    }
}

impl SecurityPageItem for SecurityKeySummary {
    fn page_cursor_id(&self) -> SecurityCursorId {
        SecurityCursorId::Key(self.id)
    }

    fn valid_for_page_epoch(&self, authorization_epoch: AuthorizationEpoch) -> bool {
        self.published_epoch.get() <= authorization_epoch.get()
    }

    fn page_item_size_bound(&self) -> usize {
        self.encoded_size_bound()
    }
}

fn validate_security_page<T: SecurityPageItem>(
    authorization_epoch: AuthorizationEpoch,
    items: &[T],
    next_cursor: Option<SecurityCursor>,
) -> Result<(), AccessCatalogError> {
    if authorization_epoch == AuthorizationEpoch::UNMANAGED
        || items.len() > MAX_SECURITY_LIST_ROWS
        || items
            .windows(2)
            .any(|pair| pair[0].page_cursor_id() >= pair[1].page_cursor_id())
        || items
            .iter()
            .any(|item| !item.valid_for_page_epoch(authorization_epoch))
    {
        return Err(AccessCatalogError::InvalidRequest);
    }
    if let Some(cursor) = next_cursor {
        let expected = items
            .last()
            .map(|item| SecurityCursor::new(authorization_epoch, item.page_cursor_id()));
        if expected != Some(cursor) {
            return Err(AccessCatalogError::InvalidRequest);
        }
    }
    Ok(())
}

macro_rules! security_page {
    ($name:ident, $item:ty) => {
        /// One bounded generation-consistent security metadata page.
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name {
            /// Authorization generation shared by every returned item.
            pub authorization_epoch: AuthorizationEpoch,
            /// Redacted records in deterministic stable-ID order.
            pub items: Box<[$item]>,
            /// Exclusive continuation when more records remain.
            pub next_cursor: Option<SecurityCursor>,
        }

        impl $name {
            /// Reconstructs one bounded canonical page from wire fields.
            ///
            /// # Errors
            ///
            /// Returns an error for an unmanaged epoch, excessive, duplicated,
            /// or unordered items, a future item epoch, or a cursor that does
            /// not identify the final returned item.
            pub fn try_from_wire(
                authorization_epoch: AuthorizationEpoch,
                items: Vec<$item>,
                next_cursor: Option<SecurityCursor>,
            ) -> Result<Self, AccessCatalogError> {
                validate_security_page(authorization_epoch, &items, next_cursor)?;
                Ok(Self {
                    authorization_epoch,
                    items: items.into_boxed_slice(),
                    next_cursor,
                })
            }

            /// Validates this page against the canonical wire invariants.
            ///
            /// # Errors
            ///
            /// Returns an error for the same noncanonical fields rejected by
            /// [`Self::try_from_wire`].
            pub fn validate(&self) -> Result<(), AccessCatalogError> {
                validate_security_page(self.authorization_epoch, &self.items, self.next_cursor)
            }

            /// Returns the authorization generation shared by this page.
            pub const fn authorization_epoch(&self) -> AuthorizationEpoch {
                self.authorization_epoch
            }

            /// Returns redacted records in deterministic stable-ID order.
            pub fn items(&self) -> &[$item] {
                &self.items
            }

            /// Returns the exclusive continuation when more records remain.
            pub const fn next_cursor(&self) -> Option<SecurityCursor> {
                self.next_cursor
            }

            /// Returns a saturation-safe upper bound for the canonical Native
            /// response containing this page.
            pub fn encoded_size_bound(&self) -> usize {
                self.items.iter().fold(
                    PRODUCT_RESPONSE_ENVELOPE_BYTES + SECURITY_METADATA_PAGE_HEADER_BYTES,
                    |total, item| total.saturating_add(item.page_item_size_bound()),
                )
            }
        }
    };
}

security_page!(SecurityPrincipalPage, SecurityPrincipalSummary);
security_page!(SecurityRolePage, SecurityRoleSummary);
security_page!(SecurityAssignmentPage, SecurityAssignmentSummary);
security_page!(SecurityKeyPage, SecurityKeySummary);

/// Definite result of offline owner bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessControlBootstrapReceipt {
    /// Stable owner principal identity.
    pub principal_id: SecurityId,
    /// Public identity of the issued key.
    pub key_id: ApiKeyId,
    /// Authorization generation after activation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that activated the key.
    pub commit: ProductCommitReceipt,
}

/// Redacted offline owner-recovery state. Verifier and secret are absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerRecoveryInspection {
    /// Current managed authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Exact pending recovery provenance, when present.
    pub pending: Option<PendingOwnerRecovery>,
}

/// Definite result of recovery phase one.
#[derive(Debug)]
pub struct OwnerRecoveryStartReceipt {
    /// Stable owner principal retained by the operation.
    pub principal_id: SecurityId,
    /// Stable operation identity used for redacted provenance.
    pub operation_id: SecurityId,
    /// Public identity of the inactive replacement.
    pub key_id: ApiKeyId,
    /// Authorization generation after phase one.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that published pending provenance and verifier.
    pub commit: ProductCommitReceipt,
    /// One-time secret delivery for the restricted output sink.
    pub secret: IssuedApiKey,
}

/// Definite result of owner recovery activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerRecoveryActivationReceipt {
    /// Stable recovery operation identity.
    pub operation_id: SecurityId,
    /// Activated public key identity.
    pub key_id: ApiKeyId,
    /// Authorization generation after old owner keys were retired.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict activation commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of exact pending-recovery abort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerRecoveryAbortReceipt {
    /// Aborted stable recovery operation identity.
    pub operation_id: SecurityId,
    /// Removed inactive public key identity.
    pub key_id: ApiKeyId,
    /// Authorization generation after abort.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict abort commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one durable access-control mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessControlMutationReceipt {
    /// Authorization generation after publication.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict native commit that published the mutation.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one principal creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecurityPrincipalMutationReceipt {
    /// Stable new principal identity.
    pub principal_id: SecurityId,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict native commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one built-in role assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoleAssignmentMutationReceipt {
    /// Stable assignment identity.
    pub assignment_id: SecurityId,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict native commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one custom-role creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustomRoleMutationReceipt {
    /// Stable custom-role identity.
    pub role_id: SecurityId,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict native commit.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one API-key issue and restricted-file activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiKeyIssueReceipt {
    /// Public key identity.
    pub key_id: ApiKeyId,
    /// Owning principal identity.
    pub principal_id: SecurityId,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that activated the key.
    pub commit: ProductCommitReceipt,
}

/// Definite result of one two-phase API-key rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiKeyRotationReceipt {
    /// Immediate predecessor public identity.
    pub predecessor_key_id: ApiKeyId,
    /// Activated successor public identity.
    pub successor_key_id: ApiKeyId,
    /// Exclusive predecessor overlap deadline, starting at activation.
    pub overlap_until_micros: i64,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that activated the successor and set the deadline.
    pub commit: ProductCommitReceipt,
}

/// Definite first-phase result with one process-wide one-time secret delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKeyStartReceipt {
    /// Public identity of the inactive pending key.
    pub key_id: ApiKeyId,
    /// Owning principal identity.
    pub principal_id: SecurityId,
    /// Public predecessor identity for a rotation.
    pub predecessor_key_id: Option<ApiKeyId>,
    /// Authorization generation after the pending verifier committed.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that published the inactive verifier.
    pub commit: ProductCommitReceipt,
    /// Secret delivery consumable exactly once across cloned responses.
    pub secret: ApiKeySecretDelivery,
}

impl ApiKeyStartReceipt {
    /// Exact canonical Native response bytes required to deliver this receipt.
    pub const fn wire_size_bound() -> usize {
        282
    }
}

/// Definite result of activating an issue or rotation successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiKeyActivationReceipt {
    /// Public identity of the activated key.
    pub key_id: ApiKeyId,
    /// Public predecessor identity for a rotation.
    pub predecessor_key_id: Option<ApiKeyId>,
    /// Exclusive predecessor overlap deadline for a rotation.
    pub overlap_until_micros: Option<i64>,
    /// Published authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict commit that activated the key.
    pub commit: ProductCommitReceipt,
}

/// Redacted durable marker observation exposed only to crash-matrix harnesses.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecurityMutationTestObservation {
    /// Stable security result identity, when the mutation created one.
    pub result_security_id: Option<SecurityId>,
    /// Stable key result identity, when the mutation targeted or created one.
    pub result_key_id: Option<ApiKeyId>,
    /// Authorization generation retained by the idempotency marker.
    pub authorization_epoch: AuthorizationEpoch,
    /// Exact commit retained by the idempotency marker.
    pub commit: ProductCommitReceipt,
}

/// One bounded ordered page of redacted security audit events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityAuditPage {
    /// Events after the exclusive cursor.
    pub events: Box<[SecurityAuditEvent]>,
    /// Last returned event when more retained rows remain.
    pub next_cursor: Option<SecurityId>,
}

impl SecurityAuditPage {
    /// Reconstructs one bounded canonical audit page from wire fields.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive, duplicated, or non-monotonic events, or
    /// a continuation that does not identify the final returned event.
    pub fn try_from_wire(
        events: Vec<SecurityAuditEvent>,
        next_cursor: Option<SecurityId>,
    ) -> Result<Self, AccessCatalogError> {
        let page = Self {
            events: events.into_boxed_slice(),
            next_cursor,
        };
        page.validate()?;
        Ok(page)
    }

    /// Validates this page against the canonical wire invariants.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive, duplicated, or non-monotonic events,
    /// or a continuation that does not identify the final returned event.
    pub fn validate(&self) -> Result<(), AccessCatalogError> {
        let ids: BTreeSet<_> = self.events.iter().map(SecurityAuditEvent::id).collect();
        if self.events.len() > AccessControlLimits::V1.audit_result_rows
            || ids.len() != self.events.len()
            || self
                .events
                .windows(2)
                .any(|pair| pair[0].commit_csn() >= pair[1].commit_csn())
            || self.next_cursor.is_some_and(|cursor| {
                self.events.last().map(SecurityAuditEvent::id) != Some(cursor)
            })
        {
            Err(AccessCatalogError::InvalidRequest)
        } else {
            Ok(())
        }
    }

    /// Returns a saturation-safe upper bound for the canonical Native response.
    pub fn encoded_size_bound(&self) -> usize {
        self.events.iter().fold(
            PRODUCT_RESPONSE_ENVELOPE_BYTES + SECURITY_AUDIT_PAGE_HEADER_BYTES,
            |total, event| total.saturating_add(event.encoded_size_bound()),
        )
    }
}

impl AccessControlCatalog {
    /// Returns an empty unbootstrapped catalog.
    pub fn empty() -> Self {
        Self {
            epoch: AuthorizationEpoch::UNMANAGED,
            principals: BTreeMap::new(),
            assignments: BTreeMap::new(),
            custom_roles: BTreeMap::new(),
            custom_assignments: BTreeMap::new(),
            keys: BTreeMap::new(),
            audit_index: Vec::new(),
            pending_owner_recovery: None,
            legacy_bearer_state: LegacyBearerState::NeverEnabled,
            legacy_bearer_migration: None,
            legacy_bearer_verifier: None,
        }
    }

    /// Creates exactly one owner principal, assignment, and canonical key.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name or label, unavailable entropy, or
    /// an already-bootstrapped catalog.
    pub fn bootstrap_owner(
        &mut self,
        display_name: &str,
        key_label: &str,
        created_at_micros: i64,
    ) -> Result<(SecurityId, IssuedApiKey), AccessCatalogError> {
        if !self.principals.is_empty()
            || !self.assignments.is_empty()
            || !self.custom_roles.is_empty()
            || !self.custom_assignments.is_empty()
            || !self.keys.is_empty()
            || !self.audit_index.is_empty()
        {
            return Err(AccessCatalogError::AlreadyBootstrapped);
        }
        validate_display_name(display_name)?;
        validate_display_name(key_label)?;
        let principal_id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        let assignment_id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        let (verifier, issued) =
            ApiKeyVerifier::issue().map_err(|_| AccessCatalogError::Entropy)?;
        self.epoch = AuthorizationEpoch::INITIAL;
        self.principals.insert(
            principal_id,
            SecurityPrincipalRecord {
                id: principal_id,
                display_name: display_name.into(),
                enabled: true,
            },
        );
        self.assignments.insert(
            assignment_id,
            BuiltInRoleAssignment {
                id: assignment_id,
                principal_id,
                role: BuiltInRole::Owner,
                scope: ProductScope::Instance,
            },
        );
        self.keys.insert(
            verifier.id(),
            ApiKeyRecord {
                id: verifier.id(),
                principal_id,
                label: key_label.into(),
                verifier,
                active: false,
                roles: vec![BuiltInRole::Owner].into_boxed_slice(),
                custom_roles: Box::new([]),
                permission_ceiling: ProductAuthorization::ALL,
                scope_ceiling: vec![ProductScope::Instance].into_boxed_slice(),
                created_at_micros,
                expires_at_micros: None,
                revoked: false,
                published_epoch: self.epoch,
                predecessor_id: None,
                successor_id: None,
                overlap_until_micros: None,
                rotation_overlap_micros: None,
            },
        );
        Ok((principal_id, issued))
    }

    fn begin_legacy_bearer_migration(
        &mut self,
        display_name: &str,
        key_label: &str,
        created_at_micros: i64,
        request_digest: [u8; 32],
        legacy_bearer_verifier: [u8; 32],
    ) -> Result<
        (
            SecurityId,
            SecurityId,
            IssuedApiKey,
            AuthorizationEpoch,
            Option<ApiKeyId>,
        ),
        AccessCatalogError,
    > {
        validate_display_name(display_name)?;
        validate_display_name(key_label)?;
        if self.legacy_bearer_state == LegacyBearerState::MigrationPending {
            let migration = self
                .legacy_bearer_migration
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            if migration.request_digest != request_digest {
                return Err(AccessCatalogError::Conflict);
            }
            if self.legacy_bearer_verifier != Some(legacy_bearer_verifier) {
                return Err(AccessCatalogError::Conflict);
            }
            let previous = self
                .keys
                .get(&migration.key_id)
                .filter(|key| !key.active && !key.revoked)
                .cloned()
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            let (verifier, issued) =
                ApiKeyVerifier::issue().map_err(|_| AccessCatalogError::Entropy)?;
            if self.keys.contains_key(&verifier.id()) {
                return Err(AccessCatalogError::Conflict);
            }
            let epoch = self.next_epoch()?;
            let operation_id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
            self.keys.remove(&migration.key_id);
            self.keys.insert(
                verifier.id(),
                ApiKeyRecord {
                    id: verifier.id(),
                    principal_id: previous.principal_id,
                    label: key_label.into(),
                    verifier,
                    active: false,
                    roles: vec![BuiltInRole::Owner].into_boxed_slice(),
                    custom_roles: Box::new([]),
                    permission_ceiling: ProductAuthorization::ALL,
                    scope_ceiling: vec![ProductScope::Instance].into_boxed_slice(),
                    created_at_micros,
                    expires_at_micros: None,
                    revoked: false,
                    published_epoch: epoch,
                    predecessor_id: None,
                    successor_id: None,
                    overlap_until_micros: None,
                    rotation_overlap_micros: None,
                },
            );
            self.epoch = epoch;
            self.legacy_bearer_migration = Some(LegacyBearerMigration {
                operation_id,
                key_id: issued.id(),
                authorization_epoch: epoch,
                created_at_micros,
                request_digest,
            });
            self.legacy_bearer_verifier = Some(legacy_bearer_verifier);
            return Ok((
                previous.principal_id,
                operation_id,
                issued,
                epoch,
                Some(migration.key_id),
            ));
        }
        if self.legacy_bearer_state != LegacyBearerState::NeverEnabled
            || self.legacy_bearer_migration.is_some()
        {
            return Err(AccessCatalogError::Conflict);
        }
        let (principal_id, issued) =
            self.bootstrap_owner(display_name, key_label, created_at_micros)?;
        let operation_id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        self.legacy_bearer_state = LegacyBearerState::MigrationPending;
        self.legacy_bearer_migration = Some(LegacyBearerMigration {
            operation_id,
            key_id: issued.id(),
            authorization_epoch: self.epoch,
            created_at_micros,
            request_digest,
        });
        self.legacy_bearer_verifier = Some(legacy_bearer_verifier);
        Ok((principal_id, operation_id, issued, self.epoch, None))
    }

    fn activate_legacy_bearer_migration(
        &mut self,
        key_id: ApiKeyId,
        expected_authorization_epoch: AuthorizationEpoch,
        request_digest: [u8; 32],
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        let migration = self
            .legacy_bearer_migration
            .ok_or(AccessCatalogError::NotFound)?;
        if self.legacy_bearer_state != LegacyBearerState::MigrationPending
            || migration.key_id != key_id
            || migration.authorization_epoch != expected_authorization_epoch
            || migration.request_digest != request_digest
            || self.epoch != expected_authorization_epoch
        {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.activate_key(key_id)?;
        self.legacy_bearer_state = LegacyBearerState::DualWindow;
        Ok((migration.operation_id, epoch))
    }

    fn revoke_legacy_bearer(&mut self) -> Result<AuthorizationEpoch, AccessCatalogError> {
        match self.legacy_bearer_state {
            LegacyBearerState::Revoked => Ok(self.epoch),
            LegacyBearerState::MigrationPending | LegacyBearerState::DualWindow => {
                let epoch = self.next_epoch()?;
                self.legacy_bearer_state = LegacyBearerState::Revoked;
                self.legacy_bearer_verifier = None;
                self.epoch = epoch;
                Ok(epoch)
            }
            LegacyBearerState::NeverEnabled => Err(AccessCatalogError::Conflict),
        }
    }

    /// Creates one disabled-by-default durable principal record.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid display text, exhausted limits or epoch,
    /// or unavailable entropy.
    pub fn create_principal(
        &mut self,
        display_name: &str,
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        validate_display_name(display_name)?;
        if self.principals.len() >= AccessControlLimits::V1.principals {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        if self.principals.contains_key(&id) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        self.principals.insert(
            id,
            SecurityPrincipalRecord {
                id,
                display_name: display_name.into(),
                enabled: false,
            },
        );
        self.epoch = epoch;
        Ok((id, epoch))
    }

    /// Changes whether one principal may authenticate.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent principal, a no-op, the owner principal,
    /// or an exhausted authorization epoch.
    pub fn set_principal_enabled(
        &mut self,
        principal_id: SecurityId,
        enabled: bool,
    ) -> Result<AuthorizationEpoch, AccessCatalogError> {
        if self.assignments.values().any(|assignment| {
            assignment.principal_id == principal_id && assignment.role == BuiltInRole::Owner
        }) {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let principal = self
            .principals
            .get(&principal_id)
            .ok_or(AccessCatalogError::NotFound)?;
        if principal.enabled == enabled {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        self.principals
            .get_mut(&principal_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?
            .enabled = enabled;
        self.epoch = epoch;
        Ok(epoch)
    }

    /// Assigns one immutable built-in role at one stable scope.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent principal, duplicate assignment,
    /// exhausted limits or epoch, or unavailable entropy.
    pub fn assign_built_in_role(
        &mut self,
        principal_id: SecurityId,
        role: BuiltInRole,
        scope: ProductScope,
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        if role == BuiltInRole::Owner {
            return Err(AccessCatalogError::InvalidRequest);
        }
        if !self.principals.contains_key(&principal_id) {
            return Err(AccessCatalogError::NotFound);
        }
        let current = self
            .assignments
            .values()
            .filter(|assignment| assignment.principal_id == principal_id)
            .count()
            + self
                .custom_assignments
                .values()
                .filter(|assignment| assignment.principal_id == principal_id)
                .count();
        if current >= AccessControlLimits::V1.assignments_per_principal {
            return Err(AccessCatalogError::LimitExceeded);
        }
        if self.assignments.values().any(|assignment| {
            assignment.principal_id == principal_id
                && assignment.role == role
                && assignment.scope == scope
        }) {
            return Err(AccessCatalogError::Conflict);
        }
        let id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        if self.assignments.contains_key(&id) || self.custom_assignments.contains_key(&id) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        self.assignments.insert(
            id,
            BuiltInRoleAssignment {
                id,
                principal_id,
                role,
                scope,
            },
        );
        self.epoch = epoch;
        Ok((id, epoch))
    }

    /// Creates one custom role containing canonical direct grants only.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, limit, entropy, or epoch error.
    pub fn create_custom_role(
        &mut self,
        display_name: &str,
        grants: impl IntoIterator<Item = CustomRoleGrant>,
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        validate_display_name(display_name)?;
        if BuiltInRole::parse(&display_name.to_ascii_lowercase()).is_some()
            || self
                .custom_roles
                .values()
                .any(|role| role.display_name.eq_ignore_ascii_case(display_name))
        {
            return Err(AccessCatalogError::Conflict);
        }
        if self.custom_roles.len() >= AccessControlLimits::V1.custom_roles {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let grants: BTreeSet<_> = grants.into_iter().collect();
        if grants.is_empty() || grants.len() > AccessControlLimits::V1.grants_per_role {
            return Err(AccessCatalogError::InvalidRequest);
        }
        if grants.iter().any(|grant| {
            grant.permission == ProductPermission::OwnershipManage
                || !grant.permission.supports_scope(grant.scope)
        }) {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        if self.custom_roles.contains_key(&id) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        self.custom_roles.insert(
            id,
            CustomRoleRecord {
                id,
                display_name: display_name.into(),
                grants: grants.into_iter().collect::<Vec<_>>().into_boxed_slice(),
            },
        );
        self.epoch = epoch;
        Ok((id, epoch))
    }

    /// Assigns one custom role directly to one principal.
    ///
    /// # Errors
    ///
    /// Returns a not-found, conflict, limit, entropy, or epoch error.
    pub fn assign_custom_role(
        &mut self,
        principal_id: SecurityId,
        role_id: SecurityId,
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        if !self.principals.contains_key(&principal_id) || !self.custom_roles.contains_key(&role_id)
        {
            return Err(AccessCatalogError::NotFound);
        }
        let assignment_count = self
            .assignments
            .values()
            .filter(|assignment| assignment.principal_id == principal_id)
            .count()
            + self
                .custom_assignments
                .values()
                .filter(|assignment| assignment.principal_id == principal_id)
                .count();
        if assignment_count >= AccessControlLimits::V1.assignments_per_principal {
            return Err(AccessCatalogError::LimitExceeded);
        }
        if self.custom_assignments.values().any(|assignment| {
            assignment.principal_id == principal_id && assignment.role_id == role_id
        }) {
            return Err(AccessCatalogError::Conflict);
        }
        let id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        if self.custom_assignments.contains_key(&id) || self.assignments.contains_key(&id) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        self.custom_assignments.insert(
            id,
            CustomRoleAssignment {
                id,
                principal_id,
                role_id,
            },
        );
        self.epoch = epoch;
        Ok((id, epoch))
    }

    /// Revokes one direct non-owner role assignment.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent assignment, an owner assignment, or an
    /// exhausted authorization epoch.
    pub fn revoke_assignment(
        &mut self,
        assignment_id: SecurityId,
    ) -> Result<(SecurityId, AuthorizationEpoch), AccessCatalogError> {
        if let Some(assignment) = self.assignments.get(&assignment_id) {
            if assignment.role == BuiltInRole::Owner {
                return Err(AccessCatalogError::InvalidRequest);
            }
            let principal_id = assignment.principal_id;
            let epoch = self.next_epoch()?;
            self.assignments
                .remove(&assignment_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            self.epoch = epoch;
            return Ok((principal_id, epoch));
        }
        let assignment = self
            .custom_assignments
            .get(&assignment_id)
            .ok_or(AccessCatalogError::NotFound)?;
        let principal_id = assignment.principal_id;
        let epoch = self.next_epoch()?;
        self.custom_assignments
            .remove(&assignment_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        self.epoch = epoch;
        Ok((principal_id, epoch))
    }

    /// Begins one API-key issue as an inactive durable verifier.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid labels, roles not assigned to the target,
    /// invalid expiry, exhausted limits or epoch, or unavailable entropy.
    pub fn begin_key_issue(
        &mut self,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        permission_ceiling: ProductAuthorization,
        created_at_micros: i64,
        expires_at_micros: Option<i64>,
    ) -> Result<(IssuedApiKey, AuthorizationEpoch), AccessCatalogError> {
        self.begin_key_issue_with_roles(
            principal_id,
            label,
            roles,
            [],
            permission_ceiling,
            [ProductScope::Instance],
            created_at_micros,
            expires_at_micros,
        )
    }

    /// Begins one API-key issue with built-in and custom role narrowing.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, limit, entropy, or epoch error.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_key_issue_with_roles(
        &mut self,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        custom_roles: impl IntoIterator<Item = SecurityId>,
        permission_ceiling: ProductAuthorization,
        scope_ceiling: impl IntoIterator<Item = ProductScope>,
        created_at_micros: i64,
        expires_at_micros: Option<i64>,
    ) -> Result<(IssuedApiKey, AuthorizationEpoch), AccessCatalogError> {
        self.begin_key_issue_with_roles_and_pruning(
            principal_id,
            label,
            roles,
            custom_roles,
            permission_ceiling,
            scope_ceiling,
            created_at_micros,
            expires_at_micros,
        )
        .map(|(issued, epoch, _)| (issued, epoch))
    }

    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::too_many_lines,
        reason = "one fail-atomic key issue keeps validation, retirement, and insertion ordered"
    )]
    fn begin_key_issue_with_roles_and_pruning(
        &mut self,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        custom_roles: impl IntoIterator<Item = SecurityId>,
        permission_ceiling: ProductAuthorization,
        scope_ceiling: impl IntoIterator<Item = ProductScope>,
        created_at_micros: i64,
        expires_at_micros: Option<i64>,
    ) -> Result<(IssuedApiKey, AuthorizationEpoch, Box<[ApiKeyId]>), AccessCatalogError> {
        validate_display_name(label)?;
        if !self.principals.contains_key(&principal_id) {
            return Err(AccessCatalogError::NotFound);
        }
        if expires_at_micros.is_some_and(|expiry| expiry <= created_at_micros) {
            return Err(AccessCatalogError::Conflict);
        }
        if self.keys.values().any(|key| {
            key.principal_id == principal_id
                && key.label.as_ref() == label
                && !key.active
                && !key.revoked
                && key.predecessor_id.is_none()
                && key.successor_id.is_none()
                && key.rotation_overlap_micros.is_none()
        }) {
            return Err(AccessCatalogError::Conflict);
        }
        let current_keys = self
            .keys
            .values()
            .filter(|key| key.principal_id == principal_id)
            .count();
        let retired_keys = self.retired_unlinked_keys_for_capacity(
            principal_id,
            created_at_micros,
            current_keys,
            self.ordinary_key_limit(principal_id),
        )?;
        let roles: BTreeSet<_> = roles.into_iter().collect();
        let custom_roles: BTreeSet<_> = custom_roles.into_iter().collect();
        let scope_ceiling: BTreeSet<_> = scope_ceiling.into_iter().collect();
        if roles.is_empty() && custom_roles.is_empty() {
            return Err(AccessCatalogError::InvalidRequest);
        }
        if scope_ceiling.is_empty()
            || scope_ceiling.len() > AccessControlLimits::V1.assignments_per_principal
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let assigned: BTreeSet<_> = self
            .assignments
            .values()
            .filter(|assignment| assignment.principal_id == principal_id)
            .map(|assignment| assignment.role)
            .collect();
        if !roles.is_subset(&assigned) {
            return Err(AccessCatalogError::Conflict);
        }
        let assigned_custom: BTreeSet<_> = self
            .custom_assignments
            .values()
            .filter(|assignment| assignment.principal_id == principal_id)
            .map(|assignment| assignment.role_id)
            .collect();
        if !custom_roles.is_subset(&assigned_custom) {
            return Err(AccessCatalogError::Conflict);
        }
        let (verifier, issued) =
            ApiKeyVerifier::issue().map_err(|_| AccessCatalogError::Entropy)?;
        if self.keys.contains_key(&verifier.id()) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        for (_, retired_key_id) in &retired_keys {
            self.keys.remove(retired_key_id);
        }
        self.keys.insert(
            verifier.id(),
            ApiKeyRecord {
                id: verifier.id(),
                principal_id,
                label: label.into(),
                verifier,
                active: false,
                roles: roles.into_iter().collect::<Vec<_>>().into_boxed_slice(),
                custom_roles: custom_roles
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                permission_ceiling,
                scope_ceiling: scope_ceiling
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                created_at_micros,
                expires_at_micros,
                revoked: false,
                published_epoch: epoch,
                predecessor_id: None,
                successor_id: None,
                overlap_until_micros: None,
                rotation_overlap_micros: None,
            },
        );
        self.epoch = epoch;
        Ok((
            issued,
            epoch,
            retired_keys
                .into_iter()
                .map(|(_, key_id)| key_id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ))
    }

    fn retired_unlinked_keys_for_capacity(
        &self,
        principal_id: SecurityId,
        now_micros: i64,
        current_keys: usize,
        key_limit: usize,
    ) -> Result<Vec<(i64, ApiKeyId)>, AccessCatalogError> {
        let required = current_keys.saturating_add(1).saturating_sub(key_limit);
        let mut retired: Vec<_> = self
            .keys
            .values()
            .filter(|key| {
                key.principal_id == principal_id
                    && key.predecessor_id.is_none()
                    && key.successor_id.is_none()
                    && (key.revoked
                        || key
                            .expires_at_micros
                            .is_some_and(|deadline| now_micros >= deadline))
            })
            .map(|key| (key.created_at_micros, key.id))
            .collect();
        retired.sort_unstable();
        if retired.len() < required {
            return Err(AccessCatalogError::LimitExceeded);
        }
        retired.truncate(required);
        Ok(retired)
    }

    fn ordinary_key_limit(&self, principal_id: SecurityId) -> usize {
        let is_owner = self.assignments.values().any(|assignment| {
            assignment.principal_id == principal_id
                && assignment.role == BuiltInRole::Owner
                && assignment.scope == ProductScope::Instance
        });
        AccessControlLimits::V1
            .keys_per_principal
            .saturating_sub(usize::from(is_owner))
    }

    /// Returns the current durable authorization generation.
    pub const fn epoch(&self) -> AuthorizationEpoch {
        self.epoch
    }

    /// Returns whether the catalog has durable principals.
    pub fn is_bootstrapped(&self) -> bool {
        !self.principals.is_empty()
    }

    /// Returns the terminal-aware legacy-bearer compatibility state.
    pub const fn legacy_bearer_state(&self) -> LegacyBearerState {
        self.legacy_bearer_state
    }

    /// Returns redacted migration state without a verifier or secret.
    pub fn legacy_bearer_inspection(&self) -> LegacyBearerMigrationInspection {
        LegacyBearerMigrationInspection {
            state: self.legacy_bearer_state,
            authorization_epoch: self.epoch,
            operation_id: self.legacy_bearer_migration.map(|value| value.operation_id),
            key_id: self.legacy_bearer_migration.map(|value| value.key_id),
            migration_authorization_epoch: self
                .legacy_bearer_migration
                .map(|value| value.authorization_epoch),
            created_at_micros: self
                .legacy_bearer_migration
                .map(|value| value.created_at_micros),
        }
    }

    /// Returns one redacted principal record.
    pub fn principal(&self, id: SecurityId) -> Option<&SecurityPrincipalRecord> {
        self.principals.get(&id)
    }

    /// Returns one redacted key record.
    pub fn key(&self, id: ApiKeyId) -> Option<&ApiKeyRecord> {
        self.keys.get(&id)
    }

    /// Returns one redacted custom-role record.
    pub fn custom_role(&self, id: SecurityId) -> Option<&CustomRoleRecord> {
        self.custom_roles.get(&id)
    }

    fn assignment_principal_id(&self, id: SecurityId) -> Option<SecurityId> {
        self.assignments
            .get(&id)
            .map(|assignment| assignment.principal_id)
            .or_else(|| {
                self.custom_assignments
                    .get(&id)
                    .map(|assignment| assignment.principal_id)
            })
    }

    /// Returns redacted aggregate status without credential verifiers.
    pub fn status(&self) -> AccessControlStatus {
        AccessControlStatus {
            bootstrapped: self.is_bootstrapped(),
            epoch: self.epoch,
            principals: self.principals.len(),
            assignments: self.assignments.len(),
            custom_roles: self.custom_roles.len(),
            custom_assignments: self.custom_assignments.len(),
            keys: self.keys.len(),
            pending_keys: self.keys.values().filter(|key| !key.active).count(),
            audit_events: self.audit_index.len(),
        }
    }

    pub(crate) fn list_principals(
        &self,
        request: SecurityPrincipalListRequest,
    ) -> Result<SecurityPrincipalPage, AccessCatalogError> {
        validate_security_list_limit(request.limit)?;
        let after = match validate_security_cursor(request.cursor, self.epoch)? {
            None => None,
            Some(SecurityCursorId::Principal(id)) if self.principals.contains_key(&id) => Some(id),
            Some(SecurityCursorId::Principal(_)) => {
                return Err(AccessCatalogError::InvalidRequest);
            }
            Some(_) => return Err(AccessCatalogError::InvalidRequest),
        };
        let mut items: Vec<_> = match after {
            Some(id) => self
                .principals
                .range((Excluded(id), Unbounded))
                .take(request.limit + 1)
                .map(|(_, record)| SecurityPrincipalSummary::from(record))
                .collect(),
            None => self
                .principals
                .values()
                .take(request.limit + 1)
                .map(SecurityPrincipalSummary::from)
                .collect(),
        };
        let has_more = items.len() > request.limit;
        items.truncate(request.limit);
        let next_cursor = if has_more {
            let last = items.last().ok_or(AccessCatalogError::CorruptCatalog)?;
            Some(SecurityCursor::new(
                self.epoch,
                SecurityCursorId::Principal(last.id),
            ))
        } else {
            None
        };
        SecurityPrincipalPage::try_from_wire(self.epoch, items, next_cursor)
    }

    pub(crate) fn list_roles(
        &self,
        request: SecurityRoleListRequest,
    ) -> Result<SecurityRolePage, AccessCatalogError> {
        validate_security_list_limit(request.limit)?;
        let (built_in_start, custom_after) =
            match validate_security_cursor(request.cursor, self.epoch)? {
                None => (0, None),
                Some(SecurityCursorId::BuiltInRole(role)) => {
                    let position = BUILT_IN_ROLES
                        .iter()
                        .position(|candidate| *candidate == role)
                        .ok_or(AccessCatalogError::InvalidRequest)?;
                    (position + 1, None)
                }
                Some(SecurityCursorId::CustomRole(id)) if self.custom_roles.contains_key(&id) => {
                    (BUILT_IN_ROLES.len(), Some(id))
                }
                Some(SecurityCursorId::CustomRole(_)) => {
                    return Err(AccessCatalogError::InvalidRequest);
                }
                Some(_) => return Err(AccessCatalogError::InvalidRequest),
            };
        let mut items = Vec::with_capacity(request.limit + 1);
        items.extend(
            BUILT_IN_ROLES
                .iter()
                .copied()
                .skip(built_in_start)
                .take(request.limit + 1)
                .map(SecurityRoleSummary::built_in),
        );
        if items.len() <= request.limit {
            let remaining = request.limit + 1 - items.len();
            match custom_after {
                Some(id) => items.extend(
                    self.custom_roles
                        .range((Excluded(id), Unbounded))
                        .take(remaining)
                        .map(|(_, role)| SecurityRoleSummary {
                            kind: SecurityRoleSummaryKind::Custom {
                                id: role.id,
                                display_name: role.display_name.clone(),
                                grants: role.grants.clone(),
                            },
                        }),
                ),
                None => items.extend(self.custom_roles.values().take(remaining).map(|role| {
                    SecurityRoleSummary {
                        kind: SecurityRoleSummaryKind::Custom {
                            id: role.id,
                            display_name: role.display_name.clone(),
                            grants: role.grants.clone(),
                        },
                    }
                })),
            }
        }
        let has_more = items.len() > request.limit;
        items.truncate(request.limit);
        let next_cursor = if has_more {
            let last = items.last().ok_or(AccessCatalogError::CorruptCatalog)?;
            Some(SecurityCursor::new(self.epoch, last.cursor_id()))
        } else {
            None
        };
        SecurityRolePage::try_from_wire(self.epoch, items, next_cursor)
    }

    pub(crate) fn list_assignments(
        &self,
        request: SecurityAssignmentListRequest,
    ) -> Result<SecurityAssignmentPage, AccessCatalogError> {
        validate_security_list_limit(request.limit)?;
        let after = match validate_security_cursor(request.cursor, self.epoch)? {
            None => None,
            Some(SecurityCursorId::Assignment(id)) => {
                let built_in = self.assignments.contains_key(&id);
                let custom = self.custom_assignments.contains_key(&id);
                if built_in == custom {
                    return Err(if built_in {
                        AccessCatalogError::CorruptCatalog
                    } else {
                        AccessCatalogError::InvalidRequest
                    });
                }
                Some(id)
            }
            Some(_) => return Err(AccessCatalogError::InvalidRequest),
        };
        let mut built_in = match after {
            Some(id) => self.assignments.range((Excluded(id), Unbounded)).peekable(),
            None => self.assignments.range::<SecurityId, _>(..).peekable(),
        };
        let mut custom = match after {
            Some(id) => self
                .custom_assignments
                .range((Excluded(id), Unbounded))
                .peekable(),
            None => self
                .custom_assignments
                .range::<SecurityId, _>(..)
                .peekable(),
        };
        let mut items = Vec::with_capacity(request.limit + 1);
        while items.len() <= request.limit {
            let built_in_id = built_in.peek().map(|(id, _)| **id);
            let custom_id = custom.peek().map(|(id, _)| **id);
            let item = match (built_in_id, custom_id) {
                (Some(left), Some(right)) if left == right => {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
                (Some(left), Some(right)) if left < right => built_in
                    .next()
                    .map(|(_, assignment)| SecurityAssignmentSummary::from(*assignment)),
                (Some(_), Some(_)) => custom
                    .next()
                    .map(|(_, assignment)| SecurityAssignmentSummary::from(*assignment)),
                (Some(_), None) => built_in
                    .next()
                    .map(|(_, assignment)| SecurityAssignmentSummary::from(*assignment)),
                (None, Some(_)) => custom
                    .next()
                    .map(|(_, assignment)| SecurityAssignmentSummary::from(*assignment)),
                (None, None) => None,
            };
            let Some(item) = item else {
                break;
            };
            items.push(item);
        }
        let has_more = items.len() > request.limit;
        items.truncate(request.limit);
        let next_cursor = if has_more {
            let last = items.last().ok_or(AccessCatalogError::CorruptCatalog)?;
            Some(SecurityCursor::new(
                self.epoch,
                SecurityCursorId::Assignment(last.id),
            ))
        } else {
            None
        };
        SecurityAssignmentPage::try_from_wire(self.epoch, items, next_cursor)
    }

    pub(crate) fn list_keys(
        &self,
        request: SecurityKeyListRequest,
    ) -> Result<SecurityKeyPage, AccessCatalogError> {
        validate_security_list_limit(request.limit)?;
        let after = match validate_security_cursor(request.cursor, self.epoch)? {
            None => None,
            Some(SecurityCursorId::Key(id)) if self.keys.contains_key(&id) => Some(id),
            Some(_) => return Err(AccessCatalogError::InvalidRequest),
        };
        let mut items: Vec<_> = match after {
            Some(id) => self
                .keys
                .range((Excluded(id), Unbounded))
                .take(request.limit + 1)
                .map(|(_, record)| SecurityKeySummary::from(record))
                .collect(),
            None => self
                .keys
                .values()
                .take(request.limit + 1)
                .map(SecurityKeySummary::from)
                .collect(),
        };
        let has_more = items.len() > request.limit;
        items.truncate(request.limit);
        let next_cursor = if has_more {
            let last = items.last().ok_or(AccessCatalogError::CorruptCatalog)?;
            Some(SecurityCursor::new(
                self.epoch,
                SecurityCursorId::Key(last.id),
            ))
        } else {
            None
        };
        SecurityKeyPage::try_from_wire(self.epoch, items, next_cursor)
    }

    /// Authenticates one canonical key against current durable authority.
    ///
    /// Missing, malformed, unknown, expired, revoked, disabled, and
    /// role-less credentials intentionally return the same error.
    ///
    /// # Errors
    ///
    /// Returns [`AccessCatalogError::Unauthorized`] for every public
    /// authentication failure, or [`AccessCatalogError::CorruptCatalog`] when
    /// a durable principal cannot be represented by the product boundary.
    pub fn authenticate(
        &self,
        candidate: &str,
        logical_time_micros: i64,
    ) -> Result<AuthenticatedAuthority, AccessCatalogError> {
        self.authenticate_for_lineage(candidate, logical_time_micros, [u8::MAX; 24])
    }

    pub(crate) fn authenticate_for_lineage(
        &self,
        candidate: &str,
        logical_time_micros: i64,
        directory_lineage: [u8; 24],
    ) -> Result<AuthenticatedAuthority, AccessCatalogError> {
        let key_id =
            ApiKeyVerifier::candidate_id(candidate).ok_or(AccessCatalogError::Unauthorized)?;
        let key = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::Unauthorized)?;
        if !key.verifier.verifies(candidate) {
            return Err(AccessCatalogError::Unauthorized);
        }
        self.authority_for_key(key_id, logical_time_micros, directory_lineage)
    }

    fn authenticate_terminal_replay_for_lineage(
        &self,
        candidate: &str,
        logical_time_micros: i64,
        directory_lineage: [u8; 24],
    ) -> Result<AuthenticatedAuthority, AccessCatalogError> {
        let key_id =
            ApiKeyVerifier::candidate_id(candidate).ok_or(AccessCatalogError::Unauthorized)?;
        let key = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::Unauthorized)?;
        if !key.verifier.verifies(candidate) || !key.revoked {
            return Err(AccessCatalogError::Unauthorized);
        }
        self.authority_for_key_lifecycle(key_id, logical_time_micros, directory_lineage, false)
    }

    fn authority_for_key(
        &self,
        key_id: ApiKeyId,
        logical_time_micros: i64,
        directory_lineage: [u8; 24],
    ) -> Result<AuthenticatedAuthority, AccessCatalogError> {
        self.authority_for_key_lifecycle(key_id, logical_time_micros, directory_lineage, true)
    }

    #[allow(clippy::too_many_lines)]
    fn authority_for_key_lifecycle(
        &self,
        key_id: ApiKeyId,
        logical_time_micros: i64,
        directory_lineage: [u8; 24],
        require_live: bool,
    ) -> Result<AuthenticatedAuthority, AccessCatalogError> {
        if directory_lineage == [0; 24] {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let key = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::Unauthorized)?;
        if require_live
            && (!key.active
                || key.revoked
                || key
                    .expires_at_micros
                    .is_some_and(|expiry| logical_time_micros >= expiry))
        {
            return Err(AccessCatalogError::Unauthorized);
        }
        if !require_live
            && key
                .expires_at_micros
                .is_some_and(|expiry| logical_time_micros >= expiry)
        {
            return Err(AccessCatalogError::Unauthorized);
        }
        if require_live
            && key.successor_id.zip(key.overlap_until_micros).is_some_and(
                |(successor_id, overlap_until)| {
                    self.keys
                        .get(&successor_id)
                        .is_some_and(|successor| successor.active)
                        && logical_time_micros >= overlap_until
                },
            )
        {
            return Err(AccessCatalogError::Unauthorized);
        }
        let principal = self
            .principals
            .get(&key.principal_id)
            .filter(|principal| principal.enabled)
            .ok_or(AccessCatalogError::Unauthorized)?;
        let requested_roles: BTreeSet<_> = key.roles.iter().copied().collect();
        let matching_assignments: Vec<_> = self
            .assignments
            .values()
            .copied()
            .filter(|assignment| {
                assignment.principal_id == principal.id
                    && requested_roles.contains(&assignment.role)
            })
            .collect();
        let effective_roles: BTreeSet<_> = matching_assignments
            .iter()
            .map(|assignment| assignment.role)
            .collect();
        let requested_custom_roles: BTreeSet<_> = key.custom_roles.iter().copied().collect();
        let effective_custom_roles: BTreeSet<_> = self
            .custom_assignments
            .values()
            .filter(|assignment| {
                assignment.principal_id == principal.id
                    && requested_custom_roles.contains(&assignment.role_id)
                    && self.custom_roles.contains_key(&assignment.role_id)
            })
            .map(|assignment| assignment.role_id)
            .collect();
        let custom_grants = effective_custom_roles
            .iter()
            .filter_map(|role_id| self.custom_roles.get(role_id))
            .flat_map(|role| role.grants.iter().copied());
        let scoped_authorization =
            effective_scopes(&matching_assignments, custom_grants, key.permission_ceiling);
        let authorization = scoped_authorization
            .iter()
            .fold(ProductAuthorization::NONE, |current, scoped| {
                current.union(scoped.authorization)
            });
        if authorization == ProductAuthorization::NONE {
            return Err(AccessCatalogError::Unauthorized);
        }
        let product_principal = ProductPrincipal::new(principal.id.to_string())
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let valid_until_micros = [key.expires_at_micros, key.overlap_until_micros]
            .into_iter()
            .flatten()
            .min();
        Ok(AuthenticatedAuthority {
            principal_id: principal.id,
            key_id,
            principal: product_principal,
            authorization,
            authorization_epoch: self.epoch,
            directory_lineage,
            terminal_replay_only: false,
            valid_until_micros,
            effective_roles: effective_roles
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            effective_custom_roles: effective_custom_roles
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            scope_ceiling: key.scope_ceiling.clone(),
            scoped_authorization: scoped_authorization.into_boxed_slice(),
            catalog_cursor_key: catalog_cursor_key(key, directory_lineage),
        })
    }

    /// Revokes one key and advances the global authorization epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is absent, already revoked, or the epoch
    /// cannot advance.
    pub fn revoke_key(&mut self, id: ApiKeyId) -> Result<AuthorizationEpoch, AccessCatalogError> {
        let current = self.keys.get(&id).ok_or(AccessCatalogError::NotFound)?;
        if !current.active {
            return Err(AccessCatalogError::Conflict);
        }
        if current.revoked {
            return Ok(self.epoch);
        }
        let next_epoch = self
            .epoch
            .checked_next()
            .ok_or(AccessCatalogError::LimitExceeded)?;
        let key = self.keys.get_mut(&id).ok_or(AccessCatalogError::NotFound)?;
        key.revoked = true;
        self.epoch = next_epoch;
        Ok(next_epoch)
    }

    /// Activates one pending key after its restricted output is durable.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is absent, already active/revoked, or the
    /// authorization epoch cannot advance.
    pub fn activate_key(&mut self, id: ApiKeyId) -> Result<AuthorizationEpoch, AccessCatalogError> {
        if self
            .keys
            .get(&id)
            .ok_or(AccessCatalogError::NotFound)?
            .predecessor_id
            .is_some()
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let next_epoch = self
            .epoch
            .checked_next()
            .ok_or(AccessCatalogError::LimitExceeded)?;
        let key = self.keys.get_mut(&id).ok_or(AccessCatalogError::NotFound)?;
        if key.active || key.revoked {
            return Err(AccessCatalogError::Conflict);
        }
        key.active = true;
        key.published_epoch = next_epoch;
        self.epoch = next_epoch;
        Ok(next_epoch)
    }

    /// Activates one rotated successor and starts overlap at activation time.
    ///
    /// # Errors
    ///
    /// Returns a not-found, conflict, corruption, limit, or epoch error.
    pub fn activate_rotated_key(
        &mut self,
        id: ApiKeyId,
        activated_at_micros: i64,
    ) -> Result<(AuthorizationEpoch, i64), AccessCatalogError> {
        let successor = self.keys.get(&id).ok_or(AccessCatalogError::NotFound)?;
        if successor.active
            || successor.revoked
            || activated_at_micros < successor.created_at_micros
            || successor
                .expires_at_micros
                .is_some_and(|expiry| expiry <= activated_at_micros)
        {
            return Err(AccessCatalogError::Conflict);
        }
        let predecessor_id = successor
            .predecessor_id
            .ok_or(AccessCatalogError::InvalidRequest)?;
        let overlap_micros = successor
            .rotation_overlap_micros
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let overlap_micros =
            i64::try_from(overlap_micros).map_err(|_| AccessCatalogError::LimitExceeded)?;
        let overlap_until_micros = activated_at_micros
            .checked_add(overlap_micros)
            .ok_or(AccessCatalogError::LimitExceeded)?;
        let predecessor = self
            .keys
            .get(&predecessor_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        if predecessor.successor_id != Some(id)
            || predecessor.principal_id != successor.principal_id
            || predecessor.overlap_until_micros.is_some()
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let next_epoch = self.next_epoch()?;
        if overlap_micros == 0 {
            let successor = self
                .keys
                .get_mut(&id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            successor.active = true;
            successor.published_epoch = next_epoch;
            let predecessor = self
                .keys
                .get_mut(&predecessor_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            predecessor.revoked = true;
            predecessor.overlap_until_micros = Some(overlap_until_micros);
        } else {
            let successor = self
                .keys
                .get_mut(&id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            successor.active = true;
            successor.published_epoch = next_epoch;
            let predecessor = self
                .keys
                .get_mut(&predecessor_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            predecessor.overlap_until_micros = Some(overlap_until_micros);
        }
        self.epoch = next_epoch;
        Ok((next_epoch, overlap_until_micros))
    }

    /// Begins one successor key and links exactly one immediate predecessor.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, limit, entropy, or epoch error.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_key_rotation(
        &mut self,
        predecessor_id: ApiKeyId,
        label: &str,
        overlap_seconds: u64,
        created_at_micros: i64,
        expires_at_micros: Option<i64>,
    ) -> Result<(IssuedApiKey, AuthorizationEpoch), AccessCatalogError> {
        self.begin_key_rotation_with_pruning(
            predecessor_id,
            label,
            overlap_seconds,
            created_at_micros,
            expires_at_micros,
        )
        .map(|(issued, epoch, _)| (issued, epoch))
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_key_rotation_with_pruning(
        &mut self,
        predecessor_id: ApiKeyId,
        label: &str,
        overlap_seconds: u64,
        created_at_micros: i64,
        expires_at_micros: Option<i64>,
    ) -> Result<(IssuedApiKey, AuthorizationEpoch, Box<[ApiKeyId]>), AccessCatalogError> {
        validate_display_name(label)?;
        if overlap_seconds > AccessControlLimits::V1.maximum_rotation_overlap_seconds
            || expires_at_micros.is_some_and(|expiry| expiry <= created_at_micros)
        {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let predecessor = self
            .keys
            .get(&predecessor_id)
            .filter(|key| key.active && !key.revoked && key.successor_id.is_none())
            .cloned()
            .ok_or(AccessCatalogError::Conflict)?;
        let retired_ancestors =
            self.retired_rotation_ancestors(predecessor_id, created_at_micros)?;
        let key_count = self
            .keys
            .values()
            .filter(|key| key.principal_id == predecessor.principal_id)
            .count()
            .saturating_sub(retired_ancestors.len());
        if key_count >= self.ordinary_key_limit(predecessor.principal_id) {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let overlap_micros = overlap_seconds
            .checked_mul(1_000_000)
            .ok_or(AccessCatalogError::LimitExceeded)?;
        let (verifier, issued) =
            ApiKeyVerifier::issue().map_err(|_| AccessCatalogError::Entropy)?;
        if self.keys.contains_key(&verifier.id()) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        let successor_id = verifier.id();
        for retired_id in &retired_ancestors {
            self.keys.remove(retired_id);
        }
        if !retired_ancestors.is_empty() {
            let current = self
                .keys
                .get_mut(&predecessor_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            current.predecessor_id = None;
            current.rotation_overlap_micros = None;
        }
        self.keys.insert(
            successor_id,
            ApiKeyRecord {
                id: successor_id,
                principal_id: predecessor.principal_id,
                label: label.into(),
                verifier,
                active: false,
                roles: predecessor.roles.clone(),
                custom_roles: predecessor.custom_roles.clone(),
                permission_ceiling: predecessor.permission_ceiling,
                scope_ceiling: predecessor.scope_ceiling.clone(),
                created_at_micros,
                expires_at_micros,
                revoked: false,
                published_epoch: epoch,
                predecessor_id: Some(predecessor_id),
                successor_id: None,
                overlap_until_micros: None,
                rotation_overlap_micros: Some(overlap_micros),
            },
        );
        let predecessor = self
            .keys
            .get_mut(&predecessor_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        predecessor.successor_id = Some(successor_id);
        self.epoch = epoch;
        Ok((issued, epoch, retired_ancestors.into_boxed_slice()))
    }

    fn retired_rotation_ancestors(
        &self,
        key_id: ApiKeyId,
        now_micros: i64,
    ) -> Result<Vec<ApiKeyId>, AccessCatalogError> {
        let mut child_id = key_id;
        let mut cursor = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::NotFound)?
            .predecessor_id;
        let mut retired = Vec::new();
        let mut visited = BTreeSet::new();
        while let Some(ancestor_id) = cursor {
            if !visited.insert(ancestor_id) {
                return Err(AccessCatalogError::CorruptCatalog);
            }
            let ancestor = self
                .keys
                .get(&ancestor_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            if ancestor.successor_id != Some(child_id) {
                return Err(AccessCatalogError::CorruptCatalog);
            }
            let is_retired = ancestor.revoked
                || ancestor
                    .expires_at_micros
                    .is_some_and(|deadline| now_micros >= deadline)
                || ancestor
                    .overlap_until_micros
                    .is_some_and(|deadline| now_micros >= deadline);
            if !is_retired {
                return Err(AccessCatalogError::Conflict);
            }
            retired.push(ancestor_id);
            child_id = ancestor_id;
            cursor = ancestor.predecessor_id;
        }
        Ok(retired)
    }

    /// Aborts the inactive successor linked from one predecessor.
    ///
    /// # Errors
    ///
    /// Returns conflict unless `successor_id` is the exact inactive pending
    /// successor linked from one live predecessor.
    pub fn abort_key_rotation(
        &mut self,
        successor_id: ApiKeyId,
    ) -> Result<AuthorizationEpoch, AccessCatalogError> {
        let successor = self
            .keys
            .get(&successor_id)
            .filter(|key| !key.active && !key.revoked)
            .cloned()
            .ok_or(AccessCatalogError::Conflict)?;
        let predecessor_id = successor
            .predecessor_id
            .ok_or(AccessCatalogError::InvalidRequest)?;
        let predecessor = self
            .keys
            .get(&predecessor_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        if predecessor.successor_id != Some(successor_id) || successor.successor_id.is_some() {
            return Err(AccessCatalogError::Conflict);
        }
        let next_epoch = self.next_epoch()?;
        self.keys.remove(&successor_id);
        self.keys
            .get_mut(&predecessor_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?
            .successor_id = None;
        self.epoch = next_epoch;
        Ok(next_epoch)
    }

    fn pending_key(&self, key_id: ApiKeyId) -> Option<&ApiKeyRecord> {
        self.keys
            .get(&key_id)
            .filter(|key| !key.active && !key.revoked)
    }

    fn pending_key_issue(
        &self,
        principal_id: SecurityId,
        label: &str,
    ) -> Result<&ApiKeyRecord, AccessCatalogError> {
        validate_display_name(label)?;
        let mut matches = self.keys.values().filter(|key| {
            key.principal_id == principal_id
                && key.label.as_ref() == label
                && !key.active
                && !key.revoked
                && key.predecessor_id.is_none()
                && key.successor_id.is_none()
                && key.rotation_overlap_micros.is_none()
        });
        let pending = matches.next().ok_or(AccessCatalogError::NotFound)?;
        if matches.next().is_some() {
            return Err(AccessCatalogError::Conflict);
        }
        Ok(pending)
    }

    /// Aborts one uniquely identified inactive issued key.
    ///
    /// # Errors
    ///
    /// Returns not-found or conflict unless `principal_id` and `label`
    /// identify exactly one inactive, non-rotation key.
    pub fn abort_pending_key_issue(
        &mut self,
        principal_id: SecurityId,
        label: &str,
    ) -> Result<(ApiKeyId, AuthorizationEpoch), AccessCatalogError> {
        let pending_key_id = self.pending_key_issue(principal_id, label)?.id;
        let next_epoch = self.next_epoch()?;
        self.keys.remove(&pending_key_id);
        self.epoch = next_epoch;
        Ok((pending_key_id, next_epoch))
    }

    /// Begins one replacement owner key without disabling current recovery.
    ///
    /// # Errors
    ///
    /// Returns a validation, corruption, limit, entropy, or epoch error.
    pub fn begin_owner_recovery(
        &mut self,
        label: &str,
        created_at_micros: i64,
    ) -> Result<(SecurityId, IssuedApiKey, AuthorizationEpoch), AccessCatalogError> {
        self.begin_owner_recovery_with_retired(label, created_at_micros)
            .map(|(principal_id, _, issued, epoch, _)| (principal_id, issued, epoch))
    }

    fn begin_owner_recovery_with_retired(
        &mut self,
        label: &str,
        created_at_micros: i64,
    ) -> Result<OwnerRecoveryStart, AccessCatalogError> {
        validate_display_name(label)?;
        if self.pending_owner_recovery.is_some() {
            return Err(AccessCatalogError::Conflict);
        }
        let owner_principals: BTreeSet<_> = self
            .assignments
            .values()
            .filter(|assignment| {
                assignment.role == BuiltInRole::Owner && assignment.scope == ProductScope::Instance
            })
            .map(|assignment| assignment.principal_id)
            .collect();
        if owner_principals.len() != 1 {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let owner_principal = *owner_principals
            .first()
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let mut removable: Vec<_> = self
            .keys
            .values()
            .filter(|key| {
                key.principal_id == owner_principal
                    && (!key.active || key.revoked)
                    && key.predecessor_id.is_none()
                    && key.successor_id.is_none()
            })
            .map(|key| (key.created_at_micros, key.id))
            .collect();
        removable.sort_unstable();
        let key_count = self
            .keys
            .values()
            .filter(|key| key.principal_id == owner_principal)
            .count()
            .saturating_sub(removable.len());
        if key_count >= AccessControlLimits::V1.keys_per_principal {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let (verifier, issued) =
            ApiKeyVerifier::issue().map_err(|_| AccessCatalogError::Entropy)?;
        if self.keys.contains_key(&verifier.id()) {
            return Err(AccessCatalogError::Conflict);
        }
        let epoch = self.next_epoch()?;
        let operation_id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        for (_, retired_key_id) in &removable {
            self.keys.remove(retired_key_id);
        }
        self.keys.insert(
            verifier.id(),
            ApiKeyRecord {
                id: verifier.id(),
                principal_id: owner_principal,
                label: label.into(),
                verifier,
                active: false,
                roles: vec![BuiltInRole::Owner].into_boxed_slice(),
                custom_roles: Box::new([]),
                permission_ceiling: ProductAuthorization::ALL,
                scope_ceiling: vec![ProductScope::Instance].into_boxed_slice(),
                created_at_micros,
                expires_at_micros: None,
                revoked: false,
                published_epoch: epoch,
                predecessor_id: None,
                successor_id: None,
                overlap_until_micros: None,
                rotation_overlap_micros: None,
            },
        );
        self.epoch = epoch;
        self.pending_owner_recovery = Some(PendingOwnerRecovery {
            operation_id,
            key_id: issued.id(),
            authorization_epoch: epoch,
            created_at_micros,
            provenance: OwnerRecoveryProvenance::OfflineOperatingSystemOwner,
        });
        if self.legacy_bearer_state == LegacyBearerState::MigrationPending {
            // Recovery supersedes the undelivered migration key in the same
            // catalog generation. Retained migration metadata remains valid
            // terminal provenance and no longer points at required authority.
            self.legacy_bearer_state = LegacyBearerState::Revoked;
            self.legacy_bearer_verifier = None;
        }
        Ok((
            owner_principal,
            operation_id,
            issued,
            epoch,
            removable
                .into_iter()
                .map(|(_, key_id)| key_id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ))
    }

    /// Atomically activates one recovery key and revokes prior owner keys.
    ///
    /// # Errors
    ///
    /// Returns a not-found, conflict, corruption, limit, or epoch error.
    pub fn activate_recovered_owner_key(
        &mut self,
        id: ApiKeyId,
    ) -> Result<AuthorizationEpoch, AccessCatalogError> {
        self.activate_recovered_owner_key_with_retired(id)
            .map(|(epoch, _)| epoch)
    }

    fn activate_recovered_owner_key_with_retired(
        &mut self,
        id: ApiKeyId,
    ) -> Result<(AuthorizationEpoch, Box<[ApiKeyId]>), AccessCatalogError> {
        let pending = self
            .pending_owner_recovery
            .ok_or(AccessCatalogError::NotFound)?;
        if pending.key_id != id {
            return Err(AccessCatalogError::Conflict);
        }
        let replacement = self
            .keys
            .get(&id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        if replacement.active
            || replacement.revoked
            || replacement.roles.as_ref() != [BuiltInRole::Owner]
            || replacement.permission_ceiling != ProductAuthorization::ALL
        {
            return Err(AccessCatalogError::Conflict);
        }
        let principal_id = replacement.principal_id;
        let is_owner = self.assignments.values().any(|assignment| {
            assignment.principal_id == principal_id
                && assignment.role == BuiltInRole::Owner
                && assignment.scope == ProductScope::Instance
        });
        if !is_owner {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let epoch = self.next_epoch()?;
        self.principals
            .get_mut(&principal_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?
            .enabled = true;
        let retired: Vec<_> = self
            .keys
            .values()
            .filter(|key| key.principal_id == principal_id && key.id != id)
            .map(|key| key.id)
            .collect();
        for retired_id in &retired {
            self.keys.remove(retired_id);
        }
        let replacement = self
            .keys
            .get_mut(&id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        replacement.active = true;
        replacement.published_epoch = epoch;
        self.epoch = epoch;
        self.pending_owner_recovery = None;
        if self.legacy_bearer_state.is_enabled() {
            self.legacy_bearer_state = LegacyBearerState::Revoked;
            self.legacy_bearer_verifier = None;
        }
        Ok((self.epoch, retired.into_boxed_slice()))
    }

    fn abort_owner_recovery(
        &mut self,
        key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
    ) -> Result<(PendingOwnerRecovery, AuthorizationEpoch), AccessCatalogError> {
        let pending = self
            .pending_owner_recovery
            .ok_or(AccessCatalogError::NotFound)?;
        if pending.key_id != key_id
            || pending.authorization_epoch != expected_epoch
            || self.epoch != expected_epoch
        {
            return Err(AccessCatalogError::Conflict);
        }
        let key = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        if key.active || key.revoked {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let epoch = self.next_epoch()?;
        self.keys.remove(&key_id);
        self.pending_owner_recovery = None;
        self.epoch = epoch;
        Ok((pending, epoch))
    }

    /// Returns one bounded page after an exclusive retained-event cursor.
    fn security_audit_indices(
        &self,
        cursor: Option<SecurityId>,
        limit: usize,
    ) -> Result<(&[SecurityAuditIndexEntry], Option<SecurityId>), AccessCatalogError> {
        if limit == 0 || limit > AccessControlLimits::V1.audit_result_rows {
            return Err(AccessCatalogError::InvalidRequest);
        }
        let start = match cursor {
            None => 0,
            Some(cursor) => self
                .audit_index
                .iter()
                .position(|event| event.id == cursor)
                .map(|index| index + 1)
                .ok_or(AccessCatalogError::CursorExpired)?,
        };
        let end = start.saturating_add(limit).min(self.audit_index.len());
        let events = &self.audit_index[start..end];
        let next_cursor = (end < self.audit_index.len())
            .then(|| events.last().map(|event| event.id))
            .flatten();
        Ok((events, next_cursor))
    }

    fn append_audit_event(
        &mut self,
        commit_csn: u64,
        mut draft: SecurityAuditDraft,
    ) -> Result<SecurityAuditAppend, AccessCatalogError> {
        if commit_csn == 0 {
            return Err(AccessCatalogError::InvalidRequest);
        }
        draft.targets.sort_unstable();
        draft.targets.dedup();
        draft.metadata.sort_unstable();
        draft.metadata.dedup();
        let id = SecurityId::generate().map_err(|_| AccessCatalogError::Entropy)?;
        if self.audit_index.iter().any(|event| event.id == id)
            || self
                .audit_index
                .last()
                .is_some_and(|event| event.commit_csn >= commit_csn)
        {
            return Err(AccessCatalogError::Conflict);
        }
        let event = SecurityAuditEvent {
            id,
            commit_csn,
            actor_principal_id: draft.actor_principal_id,
            actor_key_id: draft.actor_key_id,
            action: draft.action,
            result: SecurityAuditResult::Succeeded,
            targets: draft.targets.into_boxed_slice(),
            metadata: draft.metadata.into_boxed_slice(),
        };
        if encode_audit_event(&event)?.len() > AccessControlLimits::V1.audit_event_bytes {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let evicted = (self.audit_index.len() == AccessControlLimits::V1.retained_audit_events)
            .then(|| self.audit_index.remove(0));
        self.audit_index
            .push(SecurityAuditIndexEntry { id, commit_csn });
        Ok(SecurityAuditAppend { event, evicted })
    }

    /// Encodes deterministic bounded catalog bytes with a BLAKE3 trailer.
    ///
    /// # Errors
    ///
    /// Returns an error when a count or string cannot be represented within
    /// the v1 codec.
    pub fn encode(&self) -> Result<Vec<u8>, AccessCatalogError> {
        self.validate()?;
        let mut output = Vec::new();
        output.extend_from_slice(CATALOG_MAGIC);
        output.extend_from_slice(&self.epoch.get().to_be_bytes());
        push_count(&mut output, self.principals.len())?;
        push_count(&mut output, self.assignments.len())?;
        push_count(&mut output, self.custom_roles.len())?;
        push_count(&mut output, self.custom_assignments.len())?;
        push_count(&mut output, self.keys.len())?;
        push_count(&mut output, self.audit_index.len())?;
        output.push(u8::from(self.pending_owner_recovery.is_some()));
        output.push(self.legacy_bearer_state.tag());
        output.push(u8::from(self.legacy_bearer_migration.is_some()));
        output.push(u8::from(self.legacy_bearer_verifier.is_some()));
        for record in self.principals.values() {
            output.extend_from_slice(&record.id.to_be_bytes());
            output.push(u8::from(record.enabled));
            push_string(&mut output, &record.display_name)?;
        }
        for assignment in self.assignments.values() {
            output.extend_from_slice(&assignment.id.to_be_bytes());
            output.extend_from_slice(&assignment.principal_id.to_be_bytes());
            output.push(assignment.role.tag());
            encode_scope(&mut output, assignment.scope);
        }
        for role in self.custom_roles.values() {
            output.extend_from_slice(&role.id.to_be_bytes());
            push_string(&mut output, &role.display_name)?;
            output.extend_from_slice(
                &u16::try_from(role.grants.len())
                    .map_err(|_| AccessCatalogError::LimitExceeded)?
                    .to_be_bytes(),
            );
            for grant in role.grants.iter().copied() {
                output.push(grant.permission.tag());
                encode_scope(&mut output, grant.scope);
            }
        }
        for assignment in self.custom_assignments.values() {
            output.extend_from_slice(&assignment.id.to_be_bytes());
            output.extend_from_slice(&assignment.principal_id.to_be_bytes());
            output.extend_from_slice(&assignment.role_id.to_be_bytes());
        }
        for key in self.keys.values() {
            encode_key_v2(&mut output, key)?;
        }
        for event in &self.audit_index {
            output.extend_from_slice(&event.id.to_be_bytes());
            output.extend_from_slice(&event.commit_csn.to_be_bytes());
        }
        if let Some(pending) = self.pending_owner_recovery {
            output.extend_from_slice(&pending.operation_id.to_be_bytes());
            output.extend_from_slice(pending.key_id.as_bytes());
            output.extend_from_slice(&pending.authorization_epoch.get().to_be_bytes());
            output.extend_from_slice(&pending.created_at_micros.to_be_bytes());
            output.push(match pending.provenance {
                OwnerRecoveryProvenance::OfflineOperatingSystemOwner => 1,
            });
        }
        if let Some(migration) = self.legacy_bearer_migration {
            output.extend_from_slice(&migration.operation_id.to_be_bytes());
            output.extend_from_slice(migration.key_id.as_bytes());
            output.extend_from_slice(&migration.authorization_epoch.get().to_be_bytes());
            output.extend_from_slice(&migration.created_at_micros.to_be_bytes());
            output.extend_from_slice(&migration.request_digest);
        }
        if let Some(verifier) = self.legacy_bearer_verifier {
            output.extend_from_slice(&verifier);
        }
        if output.len() > MAX_ACCESS_CATALOG_BYTES - CATALOG_DIGEST_BYTES {
            return Err(AccessCatalogError::LimitExceeded);
        }
        let digest = blake3::hash(&output);
        output.extend_from_slice(digest.as_bytes());
        Ok(output)
    }

    /// Decodes exact canonical catalog bytes and rejects corruption.
    ///
    /// # Errors
    ///
    /// Returns [`AccessCatalogError::CorruptCatalog`] for any invalid,
    /// duplicate, noncanonical, oversized, or digest-mismatched input.
    #[allow(clippy::too_many_lines)]
    pub fn decode(encoded: &[u8]) -> Result<Self, AccessCatalogError> {
        if encoded.len() < CATALOG_MAGIC.len() + 8 + 12 + CATALOG_DIGEST_BYTES
            || encoded.len() > MAX_ACCESS_CATALOG_BYTES
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let body_length = encoded.len() - CATALOG_DIGEST_BYTES;
        let (body, expected_digest) = encoded.split_at(body_length);
        let actual_digest = blake3::hash(body);
        if !bool::from(actual_digest.as_bytes().ct_eq(expected_digest)) {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut decoder = Decoder::new(body);
        let magic = decoder.take(CATALOG_MAGIC.len())?;
        if magic != CATALOG_MAGIC
            && magic != CATALOG_V4_MAGIC
            && magic != CATALOG_V3_MAGIC
            && magic != CATALOG_V2_MAGIC
            && magic != CATALOG_V1_MAGIC
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let epoch = AuthorizationEpoch::new(decoder.u64()?);
        let principal_count = decoder.count(AccessControlLimits::V1.principals)?;
        let assignment_limit = AccessControlLimits::V1
            .principals
            .checked_mul(AccessControlLimits::V1.assignments_per_principal)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let assignment_count = decoder.count(assignment_limit)?;
        let modern = magic == CATALOG_MAGIC
            || magic == CATALOG_V4_MAGIC
            || magic == CATALOG_V3_MAGIC
            || magic == CATALOG_V2_MAGIC;
        let custom_role_count = if modern {
            decoder.count(AccessControlLimits::V1.custom_roles)?
        } else {
            0
        };
        let custom_assignment_count = if modern {
            decoder.count(assignment_limit)?
        } else {
            0
        };
        let key_limit = AccessControlLimits::V1
            .principals
            .checked_mul(AccessControlLimits::V1.keys_per_principal)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let key_count = decoder.count(key_limit)?;
        let audit_count = if modern {
            decoder.count(AccessControlLimits::V1.retained_audit_events)?
        } else {
            0
        };
        let has_pending_owner_recovery =
            if magic == CATALOG_MAGIC || magic == CATALOG_V4_MAGIC || magic == CATALOG_V3_MAGIC {
                decoder.boolean()?
            } else {
                false
            };
        let legacy_bearer_state = if magic == CATALOG_MAGIC || magic == CATALOG_V4_MAGIC {
            LegacyBearerState::from_tag(decoder.byte()?)
                .ok_or(AccessCatalogError::CorruptCatalog)?
        } else {
            LegacyBearerState::NeverEnabled
        };
        let has_legacy_bearer_migration = if magic == CATALOG_MAGIC || magic == CATALOG_V4_MAGIC {
            decoder.boolean()?
        } else {
            false
        };
        let has_legacy_bearer_verifier = if magic == CATALOG_MAGIC {
            decoder.boolean()?
        } else {
            false
        };
        let principals = decode_principals(&mut decoder, principal_count)?;
        let assignments = decode_assignments(&mut decoder, assignment_count)?;
        let custom_roles = decode_custom_roles(&mut decoder, custom_role_count)?;
        let custom_assignments = decode_custom_assignments(&mut decoder, custom_assignment_count)?;
        let keys = if modern {
            decode_keys_v2(&mut decoder, key_count)?
        } else {
            decode_keys_v1(&mut decoder, key_count)?
        };
        let audit_index = decode_audit_index(&mut decoder, audit_count)?;
        let pending_owner_recovery = if has_pending_owner_recovery {
            Some(PendingOwnerRecovery {
                operation_id: decoder.security_id()?,
                key_id: ApiKeyId::from_bytes(decoder.array()?)
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
                authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
                created_at_micros: decoder.i64()?,
                provenance: match decoder.byte()? {
                    1 => OwnerRecoveryProvenance::OfflineOperatingSystemOwner,
                    _ => return Err(AccessCatalogError::CorruptCatalog),
                },
            })
        } else {
            None
        };
        let legacy_bearer_migration = if has_legacy_bearer_migration {
            Some(LegacyBearerMigration {
                operation_id: decoder.security_id()?,
                key_id: ApiKeyId::from_bytes(decoder.array()?)
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
                authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
                created_at_micros: decoder.i64()?,
                request_digest: decoder.array()?,
            })
        } else {
            None
        };
        let legacy_bearer_verifier = if has_legacy_bearer_verifier {
            Some(decoder.array()?)
        } else {
            None
        };
        let catalog = Self {
            epoch,
            principals,
            assignments,
            custom_roles,
            custom_assignments,
            keys,
            audit_index,
            pending_owner_recovery,
            legacy_bearer_state,
            legacy_bearer_migration,
            legacy_bearer_verifier,
        };
        if !decoder.is_empty() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        catalog.validate()?;
        let canonical = if magic == CATALOG_MAGIC {
            catalog.encode()?
        } else if magic == CATALOG_V4_MAGIC {
            catalog.encode_v4()?
        } else if magic == CATALOG_V3_MAGIC {
            catalog.encode_v3()?
        } else if magic == CATALOG_V2_MAGIC {
            catalog.encode_v2()?
        } else {
            catalog.encode_v1()?
        };
        if canonical.as_slice() != encoded {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        Ok(catalog)
    }

    fn encode_v1(&self) -> Result<Vec<u8>, AccessCatalogError> {
        if !self.custom_roles.is_empty()
            || !self.custom_assignments.is_empty()
            || !self.audit_index.is_empty()
            || self.pending_owner_recovery.is_some()
            || self.legacy_bearer_state != LegacyBearerState::NeverEnabled
            || self.legacy_bearer_migration.is_some()
            || self.legacy_bearer_verifier.is_some()
            || self.keys.values().any(|key| {
                !key.custom_roles.is_empty()
                    || key.scope_ceiling.as_ref() != [ProductScope::Instance]
                    || key.predecessor_id.is_some()
                    || key.successor_id.is_some()
                    || key.overlap_until_micros.is_some()
                    || key.rotation_overlap_micros.is_some()
            })
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut output = Vec::new();
        output.extend_from_slice(CATALOG_V1_MAGIC);
        output.extend_from_slice(&self.epoch.get().to_be_bytes());
        push_count(&mut output, self.principals.len())?;
        push_count(&mut output, self.assignments.len())?;
        push_count(&mut output, self.keys.len())?;
        for record in self.principals.values() {
            output.extend_from_slice(&record.id.to_be_bytes());
            output.push(u8::from(record.enabled));
            push_string(&mut output, &record.display_name)?;
        }
        for assignment in self.assignments.values() {
            output.extend_from_slice(&assignment.id.to_be_bytes());
            output.extend_from_slice(&assignment.principal_id.to_be_bytes());
            output.push(assignment.role.tag());
            encode_scope(&mut output, assignment.scope);
        }
        for key in self.keys.values() {
            encode_key_v1(&mut output, key)?;
        }
        let digest = blake3::hash(&output);
        output.extend_from_slice(digest.as_bytes());
        Ok(output)
    }

    fn encode_v2(&self) -> Result<Vec<u8>, AccessCatalogError> {
        if self.pending_owner_recovery.is_some()
            || self.legacy_bearer_state != LegacyBearerState::NeverEnabled
            || self.legacy_bearer_migration.is_some()
            || self.legacy_bearer_verifier.is_some()
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut encoded = self.encode()?;
        encoded[..CATALOG_MAGIC.len()].copy_from_slice(CATALOG_V2_MAGIC);
        let pending_flag = CATALOG_MAGIC.len() + 8 + 6 * std::mem::size_of::<u32>();
        encoded.remove(pending_flag);
        encoded.truncate(encoded.len() - CATALOG_DIGEST_BYTES);
        let digest = blake3::hash(&encoded);
        encoded.extend_from_slice(digest.as_bytes());
        Ok(encoded)
    }

    fn encode_v3(&self) -> Result<Vec<u8>, AccessCatalogError> {
        if self.legacy_bearer_state != LegacyBearerState::NeverEnabled
            || self.legacy_bearer_migration.is_some()
            || self.legacy_bearer_verifier.is_some()
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut encoded = self.encode()?;
        encoded[..CATALOG_MAGIC.len()].copy_from_slice(CATALOG_V3_MAGIC);
        let legacy_state = CATALOG_MAGIC.len() + 8 + 6 * std::mem::size_of::<u32>() + 1;
        encoded.drain(legacy_state..legacy_state + 2);
        encoded.truncate(encoded.len() - CATALOG_DIGEST_BYTES);
        let digest = blake3::hash(&encoded);
        encoded.extend_from_slice(digest.as_bytes());
        Ok(encoded)
    }

    fn encode_v4(&self) -> Result<Vec<u8>, AccessCatalogError> {
        if self.legacy_bearer_verifier.is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut encoded = self.encode()?;
        encoded[..CATALOG_MAGIC.len()].copy_from_slice(CATALOG_V4_MAGIC);
        let verifier_flag = CATALOG_MAGIC.len() + 8 + 6 * std::mem::size_of::<u32>() + 3;
        encoded.remove(verifier_flag);
        encoded.truncate(encoded.len() - CATALOG_DIGEST_BYTES);
        let digest = blake3::hash(&encoded);
        encoded.extend_from_slice(digest.as_bytes());
        Ok(encoded)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one fail-closed audit keeps cross-record catalog invariants visible together"
    )]
    fn validate(&self) -> Result<(), AccessCatalogError> {
        let limits = AccessControlLimits::V1;
        if !limits.is_valid()
            || self.principals.len() > limits.principals
            || self.custom_roles.len() > limits.custom_roles
            || self.audit_index.len() > limits.retained_audit_events
            || self
                .keys
                .values()
                .any(|key| !self.principals.contains_key(&key.principal_id))
            || self
                .assignments
                .values()
                .any(|assignment| !self.principals.contains_key(&assignment.principal_id))
            || self.custom_assignments.values().any(|assignment| {
                !self.principals.contains_key(&assignment.principal_id)
                    || !self.custom_roles.contains_key(&assignment.role_id)
            })
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        for principal in self.principals.values() {
            validate_display_name(&principal.display_name)?;
            let assignments = self
                .assignments
                .values()
                .filter(|assignment| assignment.principal_id == principal.id)
                .count()
                + self
                    .custom_assignments
                    .values()
                    .filter(|assignment| assignment.principal_id == principal.id)
                    .count();
            let keys = self
                .keys
                .values()
                .filter(|key| key.principal_id == principal.id)
                .count();
            if assignments > limits.assignments_per_principal || keys > limits.keys_per_principal {
                return Err(AccessCatalogError::LimitExceeded);
            }
        }
        match (self.legacy_bearer_state, self.legacy_bearer_migration) {
            (LegacyBearerState::NeverEnabled, None) => {}
            (
                LegacyBearerState::MigrationPending | LegacyBearerState::DualWindow,
                Some(migration),
            ) => {
                let key = self
                    .keys
                    .get(&migration.key_id)
                    .ok_or(AccessCatalogError::CorruptCatalog)?;
                let is_owner = self.assignments.values().any(|assignment| {
                    assignment.principal_id == key.principal_id
                        && assignment.role == BuiltInRole::Owner
                        && assignment.scope == ProductScope::Instance
                });
                let pending = self.legacy_bearer_state == LegacyBearerState::MigrationPending;
                if migration.authorization_epoch == AuthorizationEpoch::UNMANAGED
                    || migration.authorization_epoch.get() > self.epoch.get()
                    || migration.created_at_micros != key.created_at_micros
                    || !is_owner
                    || key.revoked
                    || key.roles.as_ref() != [BuiltInRole::Owner]
                    || key.permission_ceiling != ProductAuthorization::ALL
                    || key.scope_ceiling.as_ref() != [ProductScope::Instance]
                    || key.active == pending
                {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
            }
            (LegacyBearerState::Revoked, Some(migration)) => {
                if migration.authorization_epoch == AuthorizationEpoch::UNMANAGED
                    || migration.authorization_epoch.get() > self.epoch.get()
                {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
            }
            _ => return Err(AccessCatalogError::CorruptCatalog),
        }
        if self.legacy_bearer_state.is_enabled() != self.legacy_bearer_verifier.is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        for role in self.custom_roles.values() {
            validate_display_name(&role.display_name)?;
            if BuiltInRole::parse(&role.display_name.to_ascii_lowercase()).is_some()
                || role.grants.is_empty()
                || role.grants.len() > limits.grants_per_role
                || !strictly_sorted(&role.grants)
                || role.grants.iter().any(|grant| {
                    grant.permission == ProductPermission::OwnershipManage
                        || !grant.permission.supports_scope(grant.scope)
                })
            {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        }
        if self
            .assignments
            .keys()
            .any(|id| self.custom_assignments.contains_key(id))
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        for (map_id, key) in &self.keys {
            validate_display_name(&key.label)?;
            if *map_id != key.id
                || key.verifier.id() != key.id
                || (key.roles.is_empty() && key.custom_roles.is_empty())
                || (!key.roles.is_empty() && !strictly_sorted(&key.roles))
                || (!key.custom_roles.is_empty() && !strictly_sorted(&key.custom_roles))
                || key.scope_ceiling.is_empty()
                || key.scope_ceiling.len() > limits.assignments_per_principal
                || !strictly_sorted(&key.scope_ceiling)
                || (key.scope_ceiling.contains(&ProductScope::Instance)
                    && key.scope_ceiling.len() != 1)
                || key
                    .custom_roles
                    .iter()
                    .any(|role_id| !self.custom_roles.contains_key(role_id))
                || key
                    .expires_at_micros
                    .is_some_and(|expiry| expiry <= key.created_at_micros)
                || key.published_epoch == AuthorizationEpoch::UNMANAGED
                || key.published_epoch.get() > self.epoch.get()
                || key.predecessor_id == Some(key.id)
                || key.successor_id == Some(key.id)
                || key.rotation_overlap_micros.is_some_and(|overlap| {
                    overlap
                        > AccessControlLimits::V1
                            .maximum_rotation_overlap_seconds
                            .saturating_mul(1_000_000)
                })
            {
                return Err(AccessCatalogError::CorruptCatalog);
            }
            if let Some(predecessor_id) = key.predecessor_id {
                let predecessor = self
                    .keys
                    .get(&predecessor_id)
                    .ok_or(AccessCatalogError::CorruptCatalog)?;
                if predecessor.successor_id != Some(key.id)
                    || predecessor.principal_id != key.principal_id
                    || key.rotation_overlap_micros.is_none()
                    || predecessor.roles != key.roles
                    || predecessor.custom_roles != key.custom_roles
                    || predecessor.permission_ceiling != key.permission_ceiling
                    || predecessor.scope_ceiling != key.scope_ceiling
                    || predecessor.created_at_micros > key.created_at_micros
                    || predecessor.overlap_until_micros.is_some() != key.active
                {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
            } else if key.rotation_overlap_micros.is_some() {
                return Err(AccessCatalogError::CorruptCatalog);
            }
            if let Some(successor_id) = key.successor_id {
                let successor = self
                    .keys
                    .get(&successor_id)
                    .ok_or(AccessCatalogError::CorruptCatalog)?;
                if successor.predecessor_id != Some(key.id)
                    || successor.principal_id != key.principal_id
                    || key.overlap_until_micros.is_some() != successor.active
                {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
            } else if key.overlap_until_micros.is_some() {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        }
        if let Some(pending) = self.pending_owner_recovery {
            let key = self
                .keys
                .get(&pending.key_id)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            if pending.authorization_epoch == AuthorizationEpoch::UNMANAGED
                || pending.authorization_epoch != self.epoch
                || key.active
                || key.revoked
                || key.published_epoch != pending.authorization_epoch
                || key.created_at_micros != pending.created_at_micros
                || key.roles.as_ref() != [BuiltInRole::Owner]
                || !key.custom_roles.is_empty()
                || key.permission_ceiling != ProductAuthorization::ALL
                || key.scope_ceiling.as_ref() != [ProductScope::Instance]
                || key.predecessor_id.is_some()
                || key.successor_id.is_some()
            {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        }
        self.validate_rotation_acyclic()?;
        let owner_assignments: Vec<_> = self
            .assignments
            .values()
            .filter(|assignment| assignment.role == BuiltInRole::Owner)
            .collect();
        if !self.principals.is_empty()
            && (owner_assignments.len() != 1
                || owner_assignments[0].scope != ProductScope::Instance
                || !self
                    .principals
                    .get(&owner_assignments[0].principal_id)
                    .is_some_and(SecurityPrincipalRecord::enabled))
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        if let Some(owner) = owner_assignments.first() {
            let owner_keys: Vec<_> = self
                .keys
                .values()
                .filter(|key| key.principal_id == owner.principal_id)
                .collect();
            if owner_keys.len() == limits.keys_per_principal {
                let pending_recovery_keys = owner_keys
                    .iter()
                    .filter(|key| {
                        !key.active
                            && !key.revoked
                            && key.roles.as_ref() == [BuiltInRole::Owner]
                            && key.permission_ceiling == ProductAuthorization::ALL
                            && key.scope_ceiling.as_ref() == [ProductScope::Instance]
                            && key.predecessor_id.is_none()
                            && key.successor_id.is_none()
                    })
                    .count();
                if pending_recovery_keys != 1 {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
            }
        }
        if self
            .audit_index
            .windows(2)
            .any(|pair| pair[0].commit_csn >= pair[1].commit_csn)
            || self.audit_index.iter().any(|event| event.commit_csn == 0)
            || self
                .audit_index
                .iter()
                .map(|event| event.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.audit_index.len()
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        if self.principals.is_empty() {
            if self.epoch != AuthorizationEpoch::UNMANAGED
                || !self.assignments.is_empty()
                || !self.custom_roles.is_empty()
                || !self.custom_assignments.is_empty()
                || !self.keys.is_empty()
                || !self.audit_index.is_empty()
                || self.legacy_bearer_state != LegacyBearerState::NeverEnabled
                || self.legacy_bearer_migration.is_some()
                || self.legacy_bearer_verifier.is_some()
            {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        } else if self.epoch == AuthorizationEpoch::UNMANAGED {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        Ok(())
    }

    fn validate_rotation_acyclic(&self) -> Result<(), AccessCatalogError> {
        for start in self.keys.keys().copied() {
            let mut visited = BTreeSet::new();
            let mut cursor = Some(start);
            while let Some(key_id) = cursor {
                if !visited.insert(key_id) {
                    return Err(AccessCatalogError::CorruptCatalog);
                }
                cursor = self
                    .keys
                    .get(&key_id)
                    .ok_or(AccessCatalogError::CorruptCatalog)?
                    .predecessor_id;
            }
        }
        Ok(())
    }

    fn next_epoch(&self) -> Result<AuthorizationEpoch, AccessCatalogError> {
        self.epoch
            .checked_next()
            .ok_or(AccessCatalogError::LimitExceeded)
    }
}

impl Default for AccessControlCatalog {
    fn default() -> Self {
        Self::empty()
    }
}

fn effective_scopes(
    assignments: &[BuiltInRoleAssignment],
    custom_grants: impl IntoIterator<Item = CustomRoleGrant>,
    permission_ceiling: ProductAuthorization,
) -> Vec<ScopedAuthorization> {
    let mut by_scope = BTreeMap::new();
    for assignment in assignments {
        let authorization = assignment
            .role
            .authorization()
            .for_scope(assignment.scope)
            .intersect(permission_ceiling);
        if authorization != ProductAuthorization::NONE {
            by_scope
                .entry(assignment.scope)
                .and_modify(|current: &mut ProductAuthorization| {
                    *current = current.union(authorization);
                })
                .or_insert(authorization);
        }
    }
    for grant in custom_grants {
        let authorization = ProductAuthorization::from_permissions([grant.permission])
            .for_scope(grant.scope)
            .intersect(permission_ceiling);
        if authorization != ProductAuthorization::NONE {
            by_scope
                .entry(grant.scope)
                .and_modify(|current: &mut ProductAuthorization| {
                    *current = current.union(authorization);
                })
                .or_insert(authorization);
        }
    }
    by_scope
        .into_iter()
        .map(|(scope, authorization)| ScopedAuthorization {
            scope,
            authorization,
        })
        .collect()
}

fn catalog_cursor_key(key: &ApiKeyRecord, directory_lineage: [u8; 24]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-catalog-visible-cursor-key-v1\0");
    hasher.update(&directory_lineage);
    hasher.update(key.id.as_bytes());
    hasher.update(key.verifier.digest());
    *hasher.finalize().as_bytes()
}

#[allow(clippy::missing_errors_doc)]
impl NativeProduct {
    /// Injects one deterministic interruption into the next security commit.
    ///
    /// This hook exists only for crash/recovery conformance harnesses. Normal
    /// product paths never arm it.
    #[doc(hidden)]
    pub fn interrupt_next_security_commit_for_test(
        &mut self,
        boundary: hyphae_native_runtime::CommitBoundary,
    ) {
        self.security_commit_interruption =
            Some(crate::SecurityCommitInterruption::returning(boundary));
    }

    /// Installs a synchronous hook at the exact next security commit boundary.
    ///
    /// A process-crash harness uses this to notify its parent and park before
    /// any mutation stack frame or destructor can unwind.
    #[doc(hidden)]
    pub fn hook_next_security_commit_for_test(
        &mut self,
        boundary: hyphae_native_runtime::CommitBoundary,
        hook: fn(hyphae_native_runtime::CommitBoundary),
    ) {
        self.security_commit_interruption =
            Some(crate::SecurityCommitInterruption::hooked(boundary, hook));
    }

    /// Reads one redacted durable idempotency marker for crash-matrix checks.
    #[doc(hidden)]
    pub fn security_mutation_observation_for_test(
        &self,
        actor: &AuthenticatedAuthority,
        idempotency_token: u128,
    ) -> Result<Option<SecurityMutationTestObservation>, ProductError> {
        if idempotency_token == 0 {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let fingerprint = security_mutation_fingerprint(actor, idempotency_token);
        let snapshot = self.snapshot_bounded(0)?;
        let Some(encoded) =
            snapshot.structure_get_internal(&security_mutation_marker_key(fingerprint))
        else {
            return Ok(None);
        };
        let marker = decode_security_mutation_marker(encoded).map_err(map_catalog_error)?;
        if marker.actor_principal_id != actor.principal_id || marker.actor_key_id != actor.key_id {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        let transaction_id = TransactionId::new(marker.transaction_id)
            .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
        let commit = self
            .database
            .transaction_commit_receipt(transaction_id)
            .map(Into::into)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        Ok(Some(SecurityMutationTestObservation {
            result_security_id: marker.result_id.security_id(),
            result_key_id: marker.result_id.api_key_id(),
            authorization_epoch: marker.authorization_epoch,
            commit,
        }))
    }

    /// Reads redacted principals for an embedded crash-matrix harness.
    #[doc(hidden)]
    pub fn read_security_principals_for_test(
        &self,
        actor: &AuthenticatedAuthority,
        request: SecurityPrincipalListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityPrincipalPage, ProductError> {
        self.read_security_principals(actor, &request, logical_time_micros)
    }

    /// Reads redacted keys for an embedded crash-matrix harness.
    #[doc(hidden)]
    pub fn read_security_keys_for_test(
        &self,
        actor: &AuthenticatedAuthority,
        request: SecurityKeyListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityKeyPage, ProductError> {
        self.read_security_keys(actor, &request, logical_time_micros)
    }

    /// Returns the retained audit count without requiring an online actor.
    #[doc(hidden)]
    pub fn security_audit_count_for_test(&self) -> Result<usize, ProductError> {
        self.load_access_control_catalog()
            .map(|catalog| catalog.audit_index.len())
    }

    /// Returns the last redacted audit event for offline crash-matrix checks.
    #[doc(hidden)]
    pub fn last_security_audit_for_test(&self) -> Result<Option<SecurityAuditEvent>, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        let Some(encoded_catalog) = snapshot.structure_get_internal(ACCESS_CONTROL_STORAGE_KEY)
        else {
            return Ok(None);
        };
        let catalog = AccessControlCatalog::decode(encoded_catalog).map_err(map_catalog_error)?;
        let Some(entry) = catalog.audit_index.last().copied() else {
            return Ok(None);
        };
        let encoded = snapshot
            .structure_get_internal(&audit_event_storage_key(entry))
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        let event = decode_audit_event(encoded).map_err(map_catalog_error)?;
        if event.id != entry.id || event.commit_csn != entry.commit_csn {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        Ok(Some(event))
    }

    fn read_authorized_security_catalog<T>(
        &self,
        actor: &AuthenticatedAuthority,
        permission: ProductPermission,
        logical_time_micros: i64,
        query: impl FnOnce(&AccessControlCatalog) -> Result<T, AccessCatalogError>,
    ) -> Result<T, ProductError> {
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let catalog = match snapshot.structure_get_internal(ACCESS_CONTROL_STORAGE_KEY) {
            Some(encoded) => AccessControlCatalog::decode(encoded).map_err(map_catalog_error)?,
            None => AccessControlCatalog::empty(),
        };
        require_current_actor(
            &catalog,
            actor,
            permission,
            snapshot.identity().directory_lineage,
            self.trusted_authorization_time()?,
        )?;
        query(&catalog).map_err(map_catalog_error)
    }

    /// Returns redacted durable access-control status.
    ///
    /// # Errors
    ///
    /// Returns a stable durability or corruption error when the current
    /// catalog snapshot cannot be read or decoded.
    pub fn access_control_status(&self) -> Result<AccessControlStatus, ProductError> {
        self.load_access_control_catalog()
            .map(|catalog| catalog.status())
    }

    /// Returns whether this directory contains durable managed principals.
    pub fn access_control_bootstrapped(&self) -> Result<bool, ProductError> {
        self.load_access_control_catalog()
            .map(|catalog| catalog.is_bootstrapped())
    }

    /// Returns redacted durable legacy-bearer migration state.
    pub fn legacy_bearer_migration_inspection(
        &self,
    ) -> Result<LegacyBearerMigrationInspection, ProductError> {
        self.load_access_control_catalog()
            .map(|catalog| catalog.legacy_bearer_inspection())
    }

    /// Returns the keyed durable verifier while the compatibility bearer is enabled.
    ///
    /// # Errors
    ///
    /// Returns corruption if durable state is not internally coherent.
    pub(crate) fn legacy_bearer_verifier(
        &self,
    ) -> Result<Option<LegacyBearerVerifier>, ProductError> {
        let catalog = self.load_access_control_catalog()?;
        match (
            catalog.legacy_bearer_state.is_enabled(),
            catalog.legacy_bearer_verifier,
        ) {
            (true, Some(verifier)) => Ok(Some(LegacyBearerVerifier(verifier))),
            (false, None) => Ok(None),
            _ => Err(ProductError::from_code(ProductErrorCode::Corruption)),
        }
    }

    pub(crate) fn legacy_bearer_digest_matches(
        &self,
        candidate: [u8; 32],
    ) -> Result<bool, ProductError> {
        Ok(self
            .legacy_bearer_verifier()?
            .is_some_and(|verifier| verifier.verifies_digest(candidate, self.catalog_cursor_key)))
    }

    pub(crate) fn read_security_status(
        &self,
        actor: &AuthenticatedAuthority,
        logical_time_micros: i64,
    ) -> Result<AccessControlStatus, ProductError> {
        self.read_authorized_security_catalog(
            actor,
            ProductPermission::SecurityRead,
            logical_time_micros,
            |catalog| Ok(catalog.status()),
        )
    }

    pub(crate) fn read_security_principals(
        &self,
        actor: &AuthenticatedAuthority,
        request: &SecurityPrincipalListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityPrincipalPage, ProductError> {
        self.read_authorized_security_catalog(
            actor,
            ProductPermission::SecurityRead,
            logical_time_micros,
            |catalog| catalog.list_principals(*request),
        )
    }

    pub(crate) fn read_security_roles(
        &self,
        actor: &AuthenticatedAuthority,
        request: &SecurityRoleListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityRolePage, ProductError> {
        self.read_authorized_security_catalog(
            actor,
            ProductPermission::SecurityRead,
            logical_time_micros,
            |catalog| catalog.list_roles(*request),
        )
    }

    pub(crate) fn read_security_assignments(
        &self,
        actor: &AuthenticatedAuthority,
        request: &SecurityAssignmentListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityAssignmentPage, ProductError> {
        self.read_authorized_security_catalog(
            actor,
            ProductPermission::SecurityRead,
            logical_time_micros,
            |catalog| catalog.list_assignments(*request),
        )
    }

    pub(crate) fn read_security_keys(
        &self,
        actor: &AuthenticatedAuthority,
        request: &SecurityKeyListRequest,
        logical_time_micros: i64,
    ) -> Result<SecurityKeyPage, ProductError> {
        self.read_authorized_security_catalog(
            actor,
            ProductPermission::SecurityRead,
            logical_time_micros,
            |catalog| catalog.list_keys(*request),
        )
    }

    /// Reads one bounded, redacted audit page from one immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization error unless the current durable actor
    /// has `audit.read`. A cursor outside the retained window fails closed, and
    /// a missing or mismatched event is durable corruption.
    pub fn read_security_audit(
        &self,
        actor: &AuthenticatedAuthority,
        cursor: Option<SecurityId>,
        limit: usize,
        logical_time_micros: i64,
    ) -> Result<SecurityAuditPage, ProductError> {
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let catalog = match snapshot.structure_get_internal(ACCESS_CONTROL_STORAGE_KEY) {
            Some(encoded) => AccessControlCatalog::decode(encoded).map_err(map_catalog_error)?,
            None => AccessControlCatalog::empty(),
        };
        let lineage = snapshot.identity().directory_lineage;
        let authorization_time_micros = self.trusted_authorization_time()?;
        require_current_actor(
            &catalog,
            actor,
            ProductPermission::AuditRead,
            lineage,
            authorization_time_micros,
        )?;
        let (indices, next_cursor) = catalog
            .security_audit_indices(cursor, limit)
            .map_err(map_catalog_error)?;
        let mut events = Vec::with_capacity(indices.len());
        for entry in indices.iter().copied() {
            let encoded = snapshot
                .structure_get_internal(&audit_event_storage_key(entry))
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
            let event = decode_audit_event(encoded).map_err(map_catalog_error)?;
            if event.id != entry.id || event.commit_csn != entry.commit_csn {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            events.push(event);
        }
        SecurityAuditPage::try_from_wire(events, next_cursor).map_err(map_catalog_error)
    }

    /// Authenticates one API key against the current durable catalog.
    ///
    /// # Errors
    ///
    /// Returns one uniform authorization error for malformed, unknown,
    /// expired, revoked, disabled, wrong, or role-less credentials. Durable
    /// corruption remains distinguishable to local operators.
    pub fn authenticate_api_key(
        &self,
        candidate: &str,
        _logical_time_micros: i64,
    ) -> Result<AuthenticatedAuthority, ProductError> {
        self.authenticate_api_key_at(candidate, self.trusted_authorization_time()?)
    }

    /// Authenticates a retained revoked key only for exact terminal replay.
    ///
    /// # Errors
    ///
    /// Returns the same uniform authorization denial as normal authentication.
    pub fn authenticate_api_key_for_terminal_replay(
        &self,
        candidate: &str,
    ) -> Result<AuthenticatedAuthority, ProductError> {
        let directory_lineage = self.database.directory_identity().lineage().encode();
        let catalog = self.load_access_control_catalog()?;
        let mut authority = catalog
            .authenticate_terminal_replay_for_lineage(
                candidate,
                self.trusted_authorization_time()?,
                directory_lineage,
            )
            .map_err(map_catalog_error)?;
        authority.authorization_epoch = catalog.epoch;
        authority.terminal_replay_only = true;
        Ok(authority)
    }

    fn authenticate_api_key_at(
        &self,
        candidate: &str,
        authorization_time_micros: i64,
    ) -> Result<AuthenticatedAuthority, ProductError> {
        let directory_lineage = self.database.directory_identity().lineage().encode();
        self.load_access_control_catalog()?
            .authenticate_for_lineage(candidate, authorization_time_micros, directory_lineage)
            .map_err(map_catalog_error)
    }

    #[cfg(test)]
    pub(crate) fn authenticate_api_key_trusted(
        &self,
        candidate: &str,
    ) -> Result<AuthenticatedAuthority, ProductError> {
        self.authenticate_api_key_at(candidate, self.trusted_authorization_time()?)
    }

    pub(crate) fn authenticate_api_key_request_trusted(
        &self,
        candidate: &str,
    ) -> Result<crate::ApiKeyRequestAuthentication, ProductError> {
        let directory_lineage = self.database.directory_identity().lineage().encode();
        let authorization_time_micros = self.trusted_authorization_time()?;
        let catalog = self.load_access_control_catalog()?;
        let key_id = ApiKeyVerifier::candidate_id(candidate);
        let Some(key) = key_id.and_then(|key_id| catalog.keys.get(&key_id)) else {
            return Ok(crate::ApiKeyRequestAuthentication::PendingTerminal(
                crate::PendingTerminalCredential { authority: None },
            ));
        };
        if !key.verifier.verifies(candidate) {
            return Ok(crate::ApiKeyRequestAuthentication::PendingTerminal(
                crate::PendingTerminalCredential { authority: None },
            ));
        }
        match catalog.authority_for_key(key.id, authorization_time_micros, directory_lineage) {
            Ok(authority) => Ok(crate::ApiKeyRequestAuthentication::Authenticated(authority)),
            Err(AccessCatalogError::Unauthorized) if key.revoked => {
                let authority = catalog
                    .authority_for_key_lifecycle(
                        key.id,
                        authorization_time_micros,
                        directory_lineage,
                        false,
                    )
                    .ok()
                    .map(|mut authority| {
                        authority.authorization_epoch = catalog.epoch;
                        authority.terminal_replay_only = true;
                        authority
                    });
                Ok(crate::ApiKeyRequestAuthentication::PendingTerminal(
                    crate::PendingTerminalCredential { authority },
                ))
            }
            Err(AccessCatalogError::Unauthorized) => {
                Ok(crate::ApiKeyRequestAuthentication::PendingTerminal(
                    crate::PendingTerminalCredential { authority: None },
                ))
            }
            Err(error) => Err(map_catalog_error(error)),
        }
    }

    pub(crate) fn revalidate_authenticated_authority(
        &self,
        authority: Arc<AuthenticatedAuthority>,
    ) -> Result<Arc<AuthenticatedAuthority>, ProductError> {
        if authority.directory_lineage != self.database.directory_identity().lineage().encode() {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let now = self.trusted_authorization_time()?;
        if !authority.terminal_replay_only
            && self.access_control_epoch_known.load(Ordering::Acquire)
            && self.access_control_epoch.load(Ordering::Acquire)
                == authority.authorization_epoch.get()
            && authority
                .valid_until_micros
                .is_none_or(|deadline| now < deadline)
        {
            return Ok(authority);
        }
        let catalog = self.load_access_control_catalog()?;
        self.access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        self.access_control_epoch_known
            .store(true, Ordering::Release);
        current_actor(
            &catalog,
            &authority,
            self.database.directory_identity().lineage().encode(),
            now,
        )
        .map(Arc::new)
    }

    fn trusted_authorization_time(&self) -> Result<i64, ProductError> {
        let sampled = trusted_wall_time_micros()?;
        Ok(self
            .authorization_time_watermark
            .fetch_max(sampled, Ordering::AcqRel)
            .max(sampled))
    }

    /// Creates one durable principal under `security.manage` authority.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, limit, entropy, durability,
    /// or corruption error.
    pub fn create_security_principal(
        &mut self,
        actor: &AuthenticatedAuthority,
        display_name: &str,
        logical_time_micros: i64,
    ) -> Result<SecurityPrincipalMutationReceipt, ProductError> {
        self.create_security_principal_idempotent(
            actor,
            display_name,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Creates one disabled principal with an exact durable replay token.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn create_security_principal_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        display_name: &str,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<SecurityPrincipalMutationReceipt, ProductError> {
        self.create_security_principal_idempotent_with_interruption(
            actor,
            display_name,
            idempotency_token,
            logical_time_micros,
            None,
        )
    }

    fn create_security_principal_idempotent_with_interruption(
        &mut self,
        actor: &AuthenticatedAuthority,
        display_name: &str,
        idempotency_token: u128,
        logical_time_micros: i64,
        interruption: Option<crate::SecurityCommitInterruption>,
    ) -> Result<SecurityPrincipalMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::CreatePrincipal,
            &actor,
            idempotency_token,
            display_name.as_bytes(),
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::CreatePrincipal,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(SecurityPrincipalMutationReceipt {
                principal_id: replay
                    .result_id
                    .security_id()
                    .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?,
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        validate_display_name(display_name).map_err(map_catalog_error)?;
        let (principal_id, authorization_epoch) = catalog
            .create_principal(display_name)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::CreatePrincipal,
            [SecurityAuditTarget::Principal(principal_id)],
        );
        let commit = self.commit_access_control_catalog_with_marker(
            &mut catalog,
            logical_time_micros,
            audit,
            Some(SecurityMutationDraft::new(
                SecurityMutationOperation::CreatePrincipal,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::Security(principal_id),
                authorization_epoch,
            )?),
            interruption,
        )?;
        Ok(SecurityPrincipalMutationReceipt {
            principal_id,
            authorization_epoch,
            commit,
        })
    }

    /// Assigns one built-in role at one stable scope.
    ///
    /// Assigning `owner` additionally requires `ownership.manage`.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, not-found, conflict, limit, entropy,
    /// durability, or corruption error.
    pub fn assign_built_in_role(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        role: BuiltInRole,
        scope: ProductScope,
        logical_time_micros: i64,
    ) -> Result<RoleAssignmentMutationReceipt, ProductError> {
        self.assign_built_in_role_idempotent(
            actor,
            principal_id,
            role,
            scope,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Assigns one non-owner built-in role with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn assign_built_in_role_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        role: BuiltInRole,
        scope: ProductScope,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<RoleAssignmentMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let mut body = Vec::with_capacity(34);
        body.extend_from_slice(&principal_id.to_be_bytes());
        body.push(role.tag());
        encode_scope(&mut body, scope);
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::AssignBuiltInRole,
            &actor,
            idempotency_token,
            &body,
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::AssignBuiltInRole,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(RoleAssignmentMutationReceipt {
                assignment_id: replay
                    .result_id
                    .security_id()
                    .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?,
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        if role == BuiltInRole::Owner {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let (assignment_id, authorization_epoch) = catalog
            .assign_built_in_role(principal_id, role, scope)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::AssignBuiltInRole,
            [
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Assignment(assignment_id),
            ],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::AssignBuiltInRole,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::Security(assignment_id),
                authorization_epoch,
            )?,
        )?;
        Ok(RoleAssignmentMutationReceipt {
            assignment_id,
            authorization_epoch,
            commit,
        })
    }

    /// Creates one durable custom role under `security.manage` authority.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, conflict, durability, or
    /// corruption error.
    pub fn create_custom_security_role(
        &mut self,
        actor: &AuthenticatedAuthority,
        display_name: &str,
        grants: impl IntoIterator<Item = CustomRoleGrant>,
        logical_time_micros: i64,
    ) -> Result<CustomRoleMutationReceipt, ProductError> {
        self.create_custom_security_role_idempotent(
            actor,
            display_name,
            grants,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Creates one immutable custom role with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn create_custom_security_role_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        display_name: &str,
        grants: impl IntoIterator<Item = CustomRoleGrant>,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<CustomRoleMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let grants: BTreeSet<_> = grants.into_iter().collect();
        let mut body = Vec::new();
        body.extend_from_slice(
            &u64::try_from(display_name.len())
                .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
                .to_be_bytes(),
        );
        body.extend_from_slice(display_name.as_bytes());
        push_count(&mut body, grants.len()).map_err(map_catalog_error)?;
        for grant in &grants {
            body.push(grant.permission.tag());
            encode_scope(&mut body, grant.scope);
        }
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::CreateCustomRole,
            &actor,
            idempotency_token,
            &body,
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::CreateCustomRole,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(CustomRoleMutationReceipt {
                role_id: replay
                    .result_id
                    .security_id()
                    .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?,
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        validate_display_name(display_name).map_err(map_catalog_error)?;
        let (role_id, authorization_epoch) = catalog
            .create_custom_role(display_name, grants)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::CreateCustomRole,
            [SecurityAuditTarget::Role(role_id)],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::CreateCustomRole,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::Security(role_id),
                authorization_epoch,
            )?,
        )?;
        Ok(CustomRoleMutationReceipt {
            role_id,
            authorization_epoch,
            commit,
        })
    }

    /// Assigns one custom role directly to one principal.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, not-found, conflict, durability, or
    /// corruption error.
    pub fn assign_custom_security_role(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        role_id: SecurityId,
        logical_time_micros: i64,
    ) -> Result<RoleAssignmentMutationReceipt, ProductError> {
        self.assign_custom_security_role_idempotent(
            actor,
            principal_id,
            role_id,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Assigns one custom role with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn assign_custom_security_role_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        role_id: SecurityId,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<RoleAssignmentMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let mut body = Vec::with_capacity(32);
        body.extend_from_slice(&principal_id.to_be_bytes());
        body.extend_from_slice(&role_id.to_be_bytes());
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::AssignCustomRole,
            &actor,
            idempotency_token,
            &body,
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::AssignCustomRole,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(RoleAssignmentMutationReceipt {
                assignment_id: replay
                    .result_id
                    .security_id()
                    .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?,
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        let (assignment_id, authorization_epoch) = catalog
            .assign_custom_role(principal_id, role_id)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::AssignCustomRole,
            [
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Role(role_id),
                SecurityAuditTarget::Assignment(assignment_id),
            ],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::AssignCustomRole,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::Security(assignment_id),
                authorization_epoch,
            )?,
        )?;
        Ok(RoleAssignmentMutationReceipt {
            assignment_id,
            authorization_epoch,
            commit,
        })
    }

    /// Enables or disables one non-owner principal with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn set_security_principal_enabled(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        enabled: bool,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        self.set_security_principal_enabled_idempotent(
            actor,
            principal_id,
            enabled,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Enables or disables one non-owner principal with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn set_security_principal_enabled_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        enabled: bool,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let mut body = Vec::with_capacity(17);
        body.extend_from_slice(&principal_id.to_be_bytes());
        body.push(u8::from(enabled));
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::SetPrincipalEnabled,
            &actor,
            idempotency_token,
            &body,
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::SetPrincipalEnabled,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(AccessControlMutationReceipt {
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        if !enabled && principal_id == actor.principal_id {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let authorization_epoch = catalog
            .set_principal_enabled(principal_id, enabled)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::SetPrincipalEnabled,
            [SecurityAuditTarget::Principal(principal_id)],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::SetPrincipalEnabled,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::None,
                authorization_epoch,
            )?,
        )?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Revokes one non-owner direct assignment with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn revoke_security_assignment(
        &mut self,
        actor: &AuthenticatedAuthority,
        assignment_id: SecurityId,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        self.revoke_security_assignment_idempotent(
            actor,
            assignment_id,
            fresh_security_idempotency_token()?,
            logical_time_micros,
        )
    }

    /// Revokes one non-owner direct assignment with exact durable replay.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, idempotency, durability,
    /// or corruption error.
    pub fn revoke_security_assignment_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        assignment_id: SecurityId,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::SecurityManage,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::RevokeAssignment,
            &actor,
            idempotency_token,
            &assignment_id.to_be_bytes(),
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::RevokeAssignment,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(AccessControlMutationReceipt {
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        if catalog.assignment_principal_id(assignment_id) == Some(actor.principal_id) {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let (principal_id, authorization_epoch) = catalog
            .revoke_assignment(assignment_id)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::RevokeAssignment,
            [
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Assignment(assignment_id),
            ],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::RevokeAssignment,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::None,
                authorization_epoch,
            )?,
        )?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Starts one inactive API-key issue and returns its secret exactly once.
    #[allow(clippy::too_many_arguments)]
    pub fn start_api_key_issue_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        custom_roles: impl IntoIterator<Item = SecurityId>,
        permission_ceiling: ProductAuthorization,
        scope_ceiling: impl IntoIterator<Item = ProductScope>,
        expires_at_micros: Option<i64>,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<ApiKeyStartReceipt, ProductError> {
        self.start_api_key_issue_idempotent_with_interruption(
            actor,
            principal_id,
            label,
            roles,
            custom_roles,
            permission_ceiling,
            scope_ceiling,
            expires_at_micros,
            idempotency_token,
            logical_time_micros,
            self_manage,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_api_key_issue_idempotent_with_interruption(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        custom_roles: impl IntoIterator<Item = SecurityId>,
        permission_ceiling: ProductAuthorization,
        scope_ceiling: impl IntoIterator<Item = ProductScope>,
        expires_at_micros: Option<i64>,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
        interruption: Option<crate::SecurityCommitInterruption>,
    ) -> Result<ApiKeyStartReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let roles: BTreeSet<_> = roles.into_iter().collect();
        let custom_roles: BTreeSet<_> = custom_roles.into_iter().collect();
        let scope_ceiling: BTreeSet<_> = scope_ceiling.into_iter().collect();
        authorize_key_issue_variant(
            self,
            &actor,
            principal_id,
            &roles,
            &custom_roles,
            permission_ceiling,
            &scope_ceiling,
            self_manage,
        )?;
        let body = key_issue_request_body(
            principal_id,
            label,
            &roles,
            &custom_roles,
            permission_ceiling,
            &scope_ceiling,
            expires_at_micros,
            self_manage,
        )?;
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::IssueKeyStart,
            &actor,
            idempotency_token,
            &body,
        )?;
        if self
            .replay_security_mutation(
                &actor,
                idempotency_token,
                SecurityMutationOperation::IssueKeyStart,
                request_digest,
                logical_time_micros,
            )?
            .is_some()
        {
            return Err(ProductError::from_code(
                ProductErrorCode::SecretDeliveryConsumed,
            ));
        }
        let (issued, authorization_epoch, retired_keys) = catalog
            .begin_key_issue_with_roles_and_pruning(
                principal_id,
                label,
                roles,
                custom_roles,
                permission_ceiling,
                scope_ceiling,
                authorization_time_micros,
                expires_at_micros,
            )
            .map_err(map_catalog_error)?;
        let key_id = issued.id();
        let mut targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(key_id),
        ];
        targets.extend(retired_keys.iter().copied().map(SecurityAuditTarget::Key));
        let audit = SecurityAuditDraft::actor(&actor, SecurityAuditAction::IssueKey, targets);
        let commit = self.commit_access_control_catalog_with_marker(
            &mut catalog,
            logical_time_micros,
            audit,
            Some(SecurityMutationDraft::new(
                SecurityMutationOperation::IssueKeyStart,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::ApiKey(key_id),
                authorization_epoch,
            )?),
            interruption,
        )?;
        Ok(ApiKeyStartReceipt {
            key_id,
            principal_id,
            predecessor_key_id: None,
            authorization_epoch,
            commit,
            secret: ApiKeySecretDelivery::new(issued),
        })
    }

    /// Starts one inactive rotation successor and returns its secret once.
    #[allow(clippy::too_many_arguments)]
    pub fn start_api_key_rotation_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        predecessor_key_id: ApiKeyId,
        label: &str,
        overlap_seconds: u64,
        expires_at_micros: Option<i64>,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<ApiKeyStartReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let predecessor = catalog
            .key(predecessor_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
        authorize_key_rotation_target(&catalog, &actor, predecessor, self_manage)?;
        let body = key_rotation_request_body(
            predecessor_key_id,
            label,
            overlap_seconds,
            expires_at_micros,
            self_manage,
        )?;
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::RotateKeyStart,
            &actor,
            idempotency_token,
            &body,
        )?;
        if self
            .replay_security_mutation(
                &actor,
                idempotency_token,
                SecurityMutationOperation::RotateKeyStart,
                request_digest,
                logical_time_micros,
            )?
            .is_some()
        {
            return Err(ProductError::from_code(
                ProductErrorCode::SecretDeliveryConsumed,
            ));
        }
        let principal_id = predecessor.principal_id();
        let (issued, authorization_epoch, retired_ancestors) = catalog
            .begin_key_rotation_with_pruning(
                predecessor_key_id,
                label,
                overlap_seconds,
                authorization_time_micros,
                expires_at_micros,
            )
            .map_err(map_catalog_error)?;
        let successor_key_id = issued.id();
        let mut targets = vec![
            SecurityAuditTarget::Key(predecessor_key_id),
            SecurityAuditTarget::Key(successor_key_id),
        ];
        targets.extend(
            retired_ancestors
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let audit = SecurityAuditDraft::actor(&actor, SecurityAuditAction::RotateKey, targets);
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::RotateKeyStart,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::ApiKey(successor_key_id),
                authorization_epoch,
            )?,
        )?;
        Ok(ApiKeyStartReceipt {
            key_id: successor_key_id,
            principal_id,
            predecessor_key_id: Some(predecessor_key_id),
            authorization_epoch,
            commit,
            secret: ApiKeySecretDelivery::new(issued),
        })
    }

    /// Activates one exact pending issue after client-side durable output.
    pub fn activate_api_key_issue_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        key_id: ApiKeyId,
        confirmation_digest: ApiKeyConfirmationDigest,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<ApiKeyActivationReceipt, ProductError> {
        self.activate_api_key_idempotent(
            actor,
            key_id,
            confirmation_digest,
            idempotency_token,
            logical_time_micros,
            self_manage,
            false,
        )
    }

    /// Activates one exact pending rotation after client-side durable output.
    pub fn activate_api_key_rotation_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        successor_key_id: ApiKeyId,
        confirmation_digest: ApiKeyConfirmationDigest,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<ApiKeyActivationReceipt, ProductError> {
        self.activate_api_key_idempotent(
            actor,
            successor_key_id,
            confirmation_digest,
            idempotency_token,
            logical_time_micros,
            self_manage,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    fn activate_api_key_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        key_id: ApiKeyId,
        confirmation_digest: ApiKeyConfirmationDigest,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
        rotation: bool,
    ) -> Result<ApiKeyActivationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let operation = if rotation {
            SecurityMutationOperation::RotateKeyActivate
        } else {
            SecurityMutationOperation::IssueKeyActivate
        };
        let body = key_target_request_body(key_id, self_manage, Some(confirmation_digest));
        let request_digest =
            security_mutation_request_digest(operation, actor, idempotency_token, &body)?;
        if let Some(replay) = self.replay_security_mutation(
            actor,
            idempotency_token,
            operation,
            request_digest,
            logical_time_micros,
        )? {
            if replay.result_id.api_key_id() != Some(key_id) {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            return Ok(ApiKeyActivationReceipt {
                key_id,
                predecessor_key_id: replay.predecessor_key_id,
                overlap_until_micros: replay.overlap_until_micros,
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let pending = catalog.pending_key(key_id).ok_or_else(|| {
            ProductError::from_code(if rotation {
                ProductErrorCode::AuthorizationDenied
            } else {
                ProductErrorCode::CatalogConflict
            })
        })?;
        if pending.predecessor_id().is_some() != rotation {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        if rotation {
            authorize_key_rotation_target(&catalog, &actor, pending, self_manage)?;
        } else {
            authorize_key_target_variant(&actor, pending, self_manage)?;
        }
        if !bool::from(
            pending
                .verifier
                .confirmation_digest()
                .as_bytes()
                .ct_eq(confirmation_digest.as_bytes()),
        ) {
            return Err(ProductError::from_code(
                ProductErrorCode::ConfirmationDigestMismatch,
            ));
        }
        let predecessor_key_id = pending.predecessor_id();
        let (authorization_epoch, overlap_until_micros) = if rotation {
            let (epoch, overlap) = catalog
                .activate_rotated_key(key_id, authorization_time_micros)
                .map_err(map_catalog_error)?;
            (epoch, Some(overlap))
        } else {
            (
                catalog.activate_key(key_id).map_err(map_catalog_error)?,
                None,
            )
        };
        let mut targets = vec![SecurityAuditTarget::Key(key_id)];
        if let Some(predecessor) = predecessor_key_id {
            targets.push(SecurityAuditTarget::Key(predecessor));
        }
        targets.sort_unstable();
        let mut audit =
            SecurityAuditDraft::actor(&actor, SecurityAuditAction::ActivateKey, targets);
        if let Some(overlap) = overlap_until_micros {
            audit =
                audit.with_metadata([SecurityAuditMetadata::RotationOverlapUntilMicros(overlap)]);
        }
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            authorization_time_micros,
            audit,
            SecurityMutationDraft::new(
                operation,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::ApiKey(key_id),
                authorization_epoch,
            )?
            .with_activation(predecessor_key_id, overlap_until_micros),
        )?;
        Ok(ApiKeyActivationReceipt {
            key_id,
            predecessor_key_id,
            overlap_until_micros,
            authorization_epoch,
            commit,
        })
    }

    /// Aborts one exact inactive issued key with durable idempotency.
    pub fn abort_api_key_issue_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        key_id: ApiKeyId,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        self.abort_api_key_idempotent(
            actor,
            key_id,
            idempotency_token,
            logical_time_micros,
            self_manage,
            false,
        )
    }

    /// Aborts one exact inactive rotation successor with durable idempotency.
    pub fn abort_api_key_rotation_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        successor_key_id: ApiKeyId,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        self.abort_api_key_idempotent(
            actor,
            successor_key_id,
            idempotency_token,
            logical_time_micros,
            self_manage,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn abort_api_key_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        key_id: ApiKeyId,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
        rotation: bool,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let operation = if rotation {
            SecurityMutationOperation::AbortKeyRotation
        } else {
            SecurityMutationOperation::AbortKeyIssue
        };
        let request_digest = security_mutation_request_digest(
            operation,
            &actor,
            idempotency_token,
            &key_target_request_body(key_id, self_manage, None),
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            operation,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(AccessControlMutationReceipt {
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        let pending = catalog.pending_key(key_id).ok_or_else(|| {
            ProductError::from_code(if rotation {
                ProductErrorCode::AuthorizationDenied
            } else {
                ProductErrorCode::CatalogConflict
            })
        })?;
        if pending.predecessor_id().is_some() != rotation {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        if rotation {
            authorize_key_rotation_target(&catalog, &actor, pending, self_manage)?;
        } else {
            authorize_key_target_variant(&actor, pending, self_manage)?;
        }
        let principal_id = pending.principal_id();
        let label = pending.label().to_owned();
        let predecessor_key_id = pending.predecessor_id();
        let authorization_epoch = if rotation {
            catalog
                .abort_key_rotation(key_id)
                .map_err(map_catalog_error)?
        } else {
            let (aborted, epoch) = catalog
                .abort_pending_key_issue(principal_id, &label)
                .map_err(map_catalog_error)?;
            if aborted != key_id {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            epoch
        };
        let mut targets = vec![SecurityAuditTarget::Key(key_id)];
        if !rotation {
            targets.push(SecurityAuditTarget::Principal(principal_id));
        }
        if let Some(predecessor) = predecessor_key_id {
            targets.push(SecurityAuditTarget::Key(predecessor));
        }
        targets.sort_unstable();
        let audit = SecurityAuditDraft::actor(
            &actor,
            if rotation {
                SecurityAuditAction::AbortKeyRotation
            } else {
                SecurityAuditAction::AbortKeyIssue
            },
            targets,
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                operation,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::None,
                authorization_epoch,
            )?,
        )?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Revokes one exact active API key with durable idempotency.
    pub fn revoke_api_key_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        target: ApiKeyId,
        idempotency_token: u128,
        logical_time_micros: i64,
        self_manage: bool,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::RevokeKey,
            actor,
            idempotency_token,
            &key_target_request_body(target, self_manage, None),
        )?;
        if let Some(replay) = self.replay_security_mutation(
            actor,
            idempotency_token,
            SecurityMutationOperation::RevokeKey,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(AccessControlMutationReceipt {
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let target_key = catalog
            .key(target)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        authorize_key_target_variant(&actor, target_key, self_manage)?;
        let authorization_epoch = catalog.revoke_key(target).map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::RevokeKey,
            [SecurityAuditTarget::Key(target)],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            SecurityMutationDraft::new(
                SecurityMutationOperation::RevokeKey,
                request_digest,
                &actor,
                idempotency_token,
                SecurityMutationResultId::None,
                authorization_epoch,
            )?,
        )?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Issues one inactive verifier, writes a new restricted secret file, and
    /// activates it with a second strict commit.
    ///
    /// Self-management cannot widen the caller's effective roles or
    /// permissions. Issuing for another principal requires `security.manage`.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, conflict, limit, I/O,
    /// entropy, durability, or corruption error. The output is never replaced.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_api_key_to_file(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        permission_ceiling: ProductAuthorization,
        expires_at_micros: Option<i64>,
        output_path: impl AsRef<Path>,
        logical_time_micros: i64,
    ) -> Result<ApiKeyIssueReceipt, ProductError> {
        self.issue_scoped_api_key_to_file(
            actor,
            principal_id,
            label,
            roles,
            [],
            permission_ceiling,
            [ProductScope::Instance],
            expires_at_micros,
            output_path,
            logical_time_micros,
        )
    }

    /// Issues one key narrowed by built-in roles, custom roles, permissions,
    /// and a canonical credential scope ceiling.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, conflict, limit, I/O,
    /// entropy, durability, or corruption error. The output is never replaced.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_scoped_api_key_to_file(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        label: &str,
        roles: impl IntoIterator<Item = BuiltInRole>,
        custom_roles: impl IntoIterator<Item = SecurityId>,
        permission_ceiling: ProductAuthorization,
        scope_ceiling: impl IntoIterator<Item = ProductScope>,
        expires_at_micros: Option<i64>,
        output_path: impl AsRef<Path>,
        logical_time_micros: i64,
    ) -> Result<ApiKeyIssueReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let roles: BTreeSet<_> = roles.into_iter().collect();
        let custom_roles: BTreeSet<_> = custom_roles.into_iter().collect();
        let scope_ceiling: BTreeSet<_> = scope_ceiling.into_iter().collect();
        authorize_key_issue(
            self,
            &actor,
            principal_id,
            &roles,
            &custom_roles,
            permission_ceiling,
            &scope_ceiling,
        )?;
        let (issued, _pending_epoch, retired_keys) = catalog
            .begin_key_issue_with_roles_and_pruning(
                principal_id,
                label,
                roles,
                custom_roles,
                permission_ceiling,
                scope_ceiling,
                authorization_time_micros,
                expires_at_micros,
            )
            .map_err(map_catalog_error)?;
        let output_path = output_path.as_ref();
        let mut output = create_restricted_output(output_path)?;
        let mut issue_targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(issued.id()),
        ];
        issue_targets.extend(retired_keys.iter().copied().map(SecurityAuditTarget::Key));
        let pending_audit =
            SecurityAuditDraft::actor(&actor, SecurityAuditAction::IssueKey, issue_targets);
        if let Err(error) =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, pending_audit)
        {
            drop(output);
            remove_empty_output(output_path);
            return Err(error);
        }
        output
            .write_all(issued.expose_secret().as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
        sync_output_parent(output_path)?;
        let authorization_epoch = catalog
            .activate_key(issued.id())
            .map_err(map_catalog_error)?;
        let activation_audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::ActivateKey,
            [SecurityAuditTarget::Key(issued.id())],
        );
        let commit = self.commit_access_control_catalog(
            &mut catalog,
            logical_time_micros,
            activation_audit,
        )?;
        Ok(ApiKeyIssueReceipt {
            key_id: issued.id(),
            principal_id,
            authorization_epoch,
            commit,
        })
    }

    /// Rotates one key through pending verifier, restricted output, and one
    /// atomic activation that starts the bounded predecessor overlap.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, validation, conflict, limit, I/O,
    /// entropy, durability, or corruption error. The output is never replaced.
    #[allow(clippy::too_many_arguments)]
    pub fn rotate_api_key_to_file(
        &mut self,
        actor: &AuthenticatedAuthority,
        predecessor_key_id: ApiKeyId,
        label: &str,
        overlap_seconds: u64,
        expires_at_micros: Option<i64>,
        output_path: impl AsRef<Path>,
        logical_time_micros: i64,
    ) -> Result<ApiKeyRotationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let predecessor = catalog
            .key(predecessor_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
        let self_manage = predecessor.principal_id == actor.principal_id;
        authorize_key_rotation_target(&catalog, &actor, predecessor, self_manage)?;
        let (issued, _pending_epoch, retired_ancestors) = catalog
            .begin_key_rotation_with_pruning(
                predecessor_key_id,
                label,
                overlap_seconds,
                authorization_time_micros,
                expires_at_micros,
            )
            .map_err(map_catalog_error)?;
        let output_path = output_path.as_ref();
        let mut output = create_restricted_output(output_path)?;
        let mut rotation_targets = vec![
            SecurityAuditTarget::Key(predecessor_key_id),
            SecurityAuditTarget::Key(issued.id()),
        ];
        rotation_targets.extend(
            retired_ancestors
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let pending_audit =
            SecurityAuditDraft::actor(&actor, SecurityAuditAction::RotateKey, rotation_targets);
        if let Err(error) =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, pending_audit)
        {
            drop(output);
            remove_empty_output(output_path);
            return Err(error);
        }
        output
            .write_all(issued.expose_secret().as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
        sync_output_parent(output_path)?;
        let activated_at_micros = self.trusted_authorization_time()?;
        let (authorization_epoch, overlap_until_micros) = catalog
            .activate_rotated_key(issued.id(), activated_at_micros)
            .map_err(map_catalog_error)?;
        let activation_audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::ActivateKey,
            [
                SecurityAuditTarget::Key(predecessor_key_id),
                SecurityAuditTarget::Key(issued.id()),
            ],
        )
        .with_metadata([SecurityAuditMetadata::RotationOverlapUntilMicros(
            overlap_until_micros,
        )]);
        let commit = self.commit_access_control_catalog(
            &mut catalog,
            activated_at_micros,
            activation_audit,
        )?;
        Ok(ApiKeyRotationReceipt {
            predecessor_key_id,
            successor_key_id: issued.id(),
            overlap_until_micros,
            authorization_epoch,
            commit,
        })
    }

    /// Aborts one inactive rotation successor and releases its predecessor.
    ///
    /// This is the explicit recovery path when restricted-output I/O or the
    /// activation commit failed after the pending successor became durable.
    /// The caller remains responsible for removing any unactivated secret
    /// file that was already synchronized.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, not-found, conflict, durability, or
    /// corruption error. Active successors cannot be aborted.
    pub fn abort_api_key_rotation(
        &mut self,
        actor: &AuthenticatedAuthority,
        predecessor_key_id: ApiKeyId,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let predecessor = catalog
            .key(predecessor_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        let successor_key_id = predecessor
            .successor_id()
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::CatalogConflict))?;
        let successor = catalog
            .key(successor_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        let principal_id = successor.principal_id();
        let is_owner = successor.roles.contains(&BuiltInRole::Owner);
        if is_owner && !authority_allows_instance(&actor, ProductPermission::OwnershipManage) {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let allowed = if principal_id == actor.principal_id {
            authority_allows_instance(&actor, ProductPermission::CredentialSelfManage)
                || authority_allows_instance(&actor, ProductPermission::SecurityManage)
        } else {
            authority_allows_instance(&actor, ProductPermission::SecurityManage)
        };
        if !allowed {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let authorization_epoch = catalog
            .abort_key_rotation(successor_key_id)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::AbortKeyRotation,
            [
                SecurityAuditTarget::Key(predecessor_key_id),
                SecurityAuditTarget::Key(successor_key_id),
            ],
        );
        let commit =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, audit)?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Aborts one uniquely identified inactive key issue.
    ///
    /// The principal and label remain available to the caller even when the
    /// restricted output file was empty, partial, or lost after phase one.
    /// Active keys and rotation successors are never selected.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, not-found, conflict, durability, or
    /// corruption error. Ambiguous pending labels fail closed.
    pub fn abort_pending_api_key_issue(
        &mut self,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        label: &str,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let mut catalog = self.load_access_control_catalog()?;
        let authorization_time_micros = self.trusted_authorization_time()?;
        let actor = current_actor(
            &catalog,
            actor,
            self.database.directory_identity().lineage().encode(),
            authorization_time_micros,
        )?;
        let pending = catalog
            .pending_key_issue(principal_id, label)
            .map_err(map_catalog_error)?;
        let pending_key_id = pending.id;
        let is_owner = pending.roles.contains(&BuiltInRole::Owner);
        if is_owner && !authority_allows_instance(&actor, ProductPermission::OwnershipManage) {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let allowed = if principal_id == actor.principal_id {
            authority_allows_instance(&actor, ProductPermission::CredentialSelfManage)
                || authority_allows_instance(&actor, ProductPermission::SecurityManage)
        } else {
            authority_allows_instance(&actor, ProductPermission::SecurityManage)
        };
        if !allowed {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let (aborted_key_id, authorization_epoch) = catalog
            .abort_pending_key_issue(principal_id, label)
            .map_err(map_catalog_error)?;
        if aborted_key_id != pending_key_id {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::AbortKeyIssue,
            [
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Key(pending_key_id),
            ],
        );
        let commit =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, audit)?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Revokes one API key under current product authority.
    ///
    /// A principal may revoke its own credential with `credential.self_manage`;
    /// another principal's credential requires `security.manage`.
    ///
    /// # Errors
    ///
    /// Returns a stable authorization, not-found, conflict, limit, durability,
    /// or corruption error.
    pub fn revoke_api_key(
        &mut self,
        actor: &AuthenticatedAuthority,
        target: ApiKeyId,
        logical_time_micros: i64,
    ) -> Result<AccessControlMutationReceipt, ProductError> {
        let self_manage = actor.principal_id
            == self
                .load_access_control_catalog()?
                .key(target)
                .map(ApiKeyRecord::principal_id)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        self.revoke_api_key_idempotent(
            actor,
            target,
            fresh_security_idempotency_token()?,
            logical_time_micros,
            self_manage,
        )
    }

    /// Bootstraps one owner and writes its secret only to a new restricted file.
    ///
    /// The verifier is first committed inactive. The secret file is then
    /// synchronized and the key is activated by a second strict commit. A
    /// crash therefore cannot expose an active verifier without durable secret
    /// output.
    ///
    /// # Errors
    ///
    /// Returns a stable conflict, validation, I/O, entropy, durability, or
    /// corruption error. `output_path` is never overwritten.
    pub fn bootstrap_access_control_to_file(
        &mut self,
        display_name: &str,
        key_label: &str,
        output_path: impl AsRef<Path>,
        logical_time_micros: i64,
    ) -> Result<AccessControlBootstrapReceipt, ProductError> {
        self.ensure_default_scalar_keyspace()?;
        let output_path = output_path.as_ref();
        let mut output = create_restricted_output(output_path)?;
        let mut catalog = self.load_access_control_catalog()?;
        let bootstrap = catalog.bootstrap_owner(display_name, key_label, logical_time_micros);
        let (principal_id, issued) = match bootstrap {
            Ok(value) => value,
            Err(error) => {
                drop(output);
                remove_empty_output(output_path);
                return Err(map_catalog_error(error));
            }
        };
        let pending_audit = SecurityAuditDraft::offline(
            SecurityAuditAction::BootstrapOwner,
            [
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Key(issued.id()),
            ],
        );
        if let Err(error) =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, pending_audit)
        {
            drop(output);
            remove_empty_output(output_path);
            return Err(error);
        }
        output
            .write_all(issued.expose_secret().as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
        sync_output_parent(output_path)?;
        let authorization_epoch = catalog
            .activate_key(issued.id())
            .map_err(map_catalog_error)?;
        let activation_audit = SecurityAuditDraft::offline(
            SecurityAuditAction::ActivateKey,
            [SecurityAuditTarget::Key(issued.id())],
        );
        let commit = self.commit_access_control_catalog(
            &mut catalog,
            logical_time_micros,
            activation_audit,
        )?;
        Ok(AccessControlBootstrapReceipt {
            principal_id,
            key_id: issued.id(),
            authorization_epoch,
            commit,
        })
    }

    /// Publishes one inactive canonical Owner key for legacy-bearer migration.
    ///
    /// The caller must hold offline OS-owner authority. The bearer plaintext is
    /// never persisted or audited; HYACAT05 retains only its product-keyed verifier.
    pub fn start_legacy_bearer_migration_offline(
        &mut self,
        display_name: &str,
        key_label: &str,
        legacy_bearer: &[u8],
        logical_time_micros: i64,
    ) -> Result<LegacyBearerMigrationStartReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        validate_legacy_bearer(legacy_bearer)?;
        self.ensure_default_scalar_keyspace()?;
        self.ensure_unmanaged_catalog_cursor_authority()?;
        let request_digest =
            legacy_bearer_migration_request_digest(display_name, key_label, legacy_bearer)?;
        let legacy_bearer_verifier =
            legacy_bearer_verifier(legacy_bearer, self.catalog_cursor_key)?;
        let mut catalog = self.load_access_control_catalog()?;
        let (principal_id, operation_id, issued, authorization_epoch, retired_key_id) = catalog
            .begin_legacy_bearer_migration(
                display_name,
                key_label,
                logical_time_micros,
                request_digest,
                legacy_bearer_verifier,
            )
            .map_err(map_catalog_error)?;
        let mut targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(issued.id()),
            SecurityAuditTarget::LegacyBearer,
        ];
        if let Some(retired_key_id) = retired_key_id {
            targets.push(SecurityAuditTarget::Key(retired_key_id));
        }
        let audit = SecurityAuditDraft::offline(SecurityAuditAction::MigrateLegacyBearer, targets);
        let commit =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, audit)?;
        Ok(LegacyBearerMigrationStartReceipt {
            principal_id,
            operation_id,
            key_id: issued.id(),
            authorization_epoch,
            commit,
            secret: issued,
        })
    }

    /// Activates the exact canonical Owner key after its restricted file is durable.
    #[allow(clippy::too_many_arguments)]
    pub fn activate_legacy_bearer_migration_offline(
        &mut self,
        key_id: ApiKeyId,
        serialized_key: &str,
        expected_authorization_epoch: AuthorizationEpoch,
        display_name: &str,
        key_label: &str,
        legacy_bearer: &[u8],
        logical_time_micros: i64,
    ) -> Result<LegacyBearerMigrationActivationReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        validate_legacy_bearer(legacy_bearer)?;
        let request_digest =
            legacy_bearer_migration_request_digest(display_name, key_label, legacy_bearer)?;
        if let Some(receipt) = self.replay_legacy_bearer_migration_activation(
            key_id,
            expected_authorization_epoch,
            request_digest,
        )? {
            return Ok(receipt);
        }
        let mut catalog = self.load_access_control_catalog()?;
        let migration = catalog
            .legacy_bearer_migration
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        let verifier = catalog
            .keys
            .get(&key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?
            .verifier
            .clone();
        if !verifier.verifies(serialized_key) {
            return Err(ProductError::from_code(
                ProductErrorCode::ConfirmationDigestMismatch,
            ));
        }
        let (operation_id, authorization_epoch) = catalog
            .activate_legacy_bearer_migration(key_id, expected_authorization_epoch, request_digest)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::offline(
            SecurityAuditAction::ActivateKey,
            [
                SecurityAuditTarget::Key(key_id),
                SecurityAuditTarget::LegacyBearer,
            ],
        );
        let (commit, _) = self.commit_access_control_catalog_recording_legacy_migration(
            &mut catalog,
            logical_time_micros,
            audit,
            migration,
        )?;
        Ok(LegacyBearerMigrationActivationReceipt {
            operation_id,
            key_id,
            authorization_epoch,
            commit,
        })
    }

    /// Permanently disables the synthetic legacy bearer under canonical Owner authority.
    pub fn revoke_legacy_bearer_idempotent(
        &mut self,
        actor: &AuthenticatedAuthority,
        idempotency_token: u128,
        logical_time_micros: i64,
    ) -> Result<LegacyBearerRevocationReceipt, ProductError> {
        if idempotency_token == 0 {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut catalog = self.load_access_control_catalog()?;
        let actor = require_current_actor(
            &catalog,
            actor,
            ProductPermission::OwnershipManage,
            self.database.directory_identity().lineage().encode(),
            self.trusted_authorization_time()?,
        )?;
        if !actor.effective_roles.contains(&BuiltInRole::Owner) {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let request_digest = security_mutation_request_digest(
            SecurityMutationOperation::RevokeLegacyBearer,
            &actor,
            idempotency_token,
            b"legacy-bearer",
        )?;
        if let Some(replay) = self.replay_security_mutation(
            &actor,
            idempotency_token,
            SecurityMutationOperation::RevokeLegacyBearer,
            request_digest,
            logical_time_micros,
        )? {
            return Ok(LegacyBearerRevocationReceipt {
                authorization_epoch: replay.authorization_epoch,
                commit: replay.commit,
            });
        }
        let authorization_epoch = catalog.revoke_legacy_bearer().map_err(map_catalog_error)?;
        let marker = SecurityMutationDraft::new(
            SecurityMutationOperation::RevokeLegacyBearer,
            request_digest,
            &actor,
            idempotency_token,
            SecurityMutationResultId::None,
            authorization_epoch,
        )?;
        let audit = SecurityAuditDraft::actor(
            &actor,
            SecurityAuditAction::RevokeLegacyBearer,
            [SecurityAuditTarget::LegacyBearer],
        );
        let commit = self.commit_access_control_catalog_idempotent(
            &mut catalog,
            logical_time_micros,
            audit,
            marker,
        )?;
        Ok(LegacyBearerRevocationReceipt {
            authorization_epoch,
            commit,
        })
    }

    /// Recovers the unique owner through a two-phase restricted-file swap.
    ///
    /// The caller must own the data directory exclusively. Phase one leaves
    /// every active owner key usable; only after the new secret and parent
    /// directory are synchronized does one strict commit activate the
    /// replacement and revoke every prior owner key.
    ///
    /// # Errors
    ///
    /// Returns a stable validation, conflict, limit, I/O, entropy, durability,
    /// or corruption error. The output is never replaced.
    pub fn recover_owner_access_offline_to_file(
        &mut self,
        key_label: &str,
        output_path: impl AsRef<Path>,
        logical_time_micros: i64,
    ) -> Result<AccessControlBootstrapReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        let output_path = output_path.as_ref();
        let mut catalog = self.load_access_control_catalog()?;
        let (principal_id, _operation_id, issued, _pending_epoch, retired_pending_keys) =
            match catalog.begin_owner_recovery_with_retired(key_label, logical_time_micros) {
                Ok(value) => value,
                Err(error) => return Err(map_catalog_error(error)),
            };
        let mut output = create_restricted_output(output_path)?;
        let mut pending_targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(issued.id()),
        ];
        pending_targets.extend(
            retired_pending_keys
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let pending_audit =
            SecurityAuditDraft::offline(SecurityAuditAction::RecoverOwner, pending_targets);
        if let Err(error) =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, pending_audit)
        {
            drop(output);
            remove_empty_output(output_path);
            return Err(error);
        }
        output
            .write_all(issued.expose_secret().as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
        sync_output_parent(output_path)?;
        let (authorization_epoch, retired_owner_keys) = catalog
            .activate_recovered_owner_key_with_retired(issued.id())
            .map_err(map_catalog_error)?;
        let mut activation_targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(issued.id()),
        ];
        activation_targets.extend(
            retired_owner_keys
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let activation_audit =
            SecurityAuditDraft::offline(SecurityAuditAction::ActivateKey, activation_targets);
        let commit = self.commit_access_control_catalog(
            &mut catalog,
            logical_time_micros,
            activation_audit,
        )?;
        Ok(AccessControlBootstrapReceipt {
            principal_id,
            key_id: issued.id(),
            authorization_epoch,
            commit,
        })
    }

    /// Reads redacted offline owner-recovery state after normal catalog open.
    ///
    /// # Errors
    ///
    /// Returns a durability or corruption error if the current catalog cannot
    /// be read. The verifier and secret are structurally absent.
    pub fn inspect_owner_recovery_offline(&self) -> Result<OwnerRecoveryInspection, ProductError> {
        self.database.revalidate_offline_owner()?;
        let catalog = self.load_access_control_catalog()?;
        Ok(OwnerRecoveryInspection {
            authorization_epoch: catalog.epoch,
            pending: catalog.pending_owner_recovery,
        })
    }

    /// Publishes one inactive owner replacement and returns its one-time secret.
    ///
    /// A caller must write and synchronize the returned secret to a restricted
    /// new file before calling [`Self::resume_owner_recovery_offline`]. Existing
    /// owner keys remain active. A second pending recovery is a catalog conflict.
    ///
    /// # Errors
    ///
    /// Returns validation, conflict, limit, entropy, durability, owner-
    /// authority, or corruption errors.
    pub fn start_owner_recovery_offline(
        &mut self,
        key_label: &str,
        logical_time_micros: i64,
    ) -> Result<OwnerRecoveryStartReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        let mut catalog = self.load_access_control_catalog()?;
        let (principal_id, operation_id, issued, authorization_epoch, retired_pending_keys) =
            catalog
                .begin_owner_recovery_with_retired(key_label, logical_time_micros)
                .map_err(map_catalog_error)?;
        let mut targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(issued.id()),
        ];
        targets.extend(
            retired_pending_keys
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let audit = SecurityAuditDraft::offline(SecurityAuditAction::RecoverOwner, targets);
        let commit =
            self.commit_access_control_catalog(&mut catalog, logical_time_micros, audit)?;
        Ok(OwnerRecoveryStartReceipt {
            principal_id,
            operation_id,
            key_id: issued.id(),
            authorization_epoch,
            commit,
            secret: issued,
        })
    }

    /// Activates one exact pending recovery after restricted-file validation.
    ///
    /// Exact replay after activation returns the retained activation commit.
    /// Empty, partial, malformed, wrong-ID, and wrong-secret files fail before
    /// mutation. Activation atomically removes every prior owner key.
    ///
    /// # Errors
    ///
    /// Returns not-found, conflict, authorization, durability, owner-authority,
    /// I/O, or corruption errors.
    pub fn resume_owner_recovery_offline(
        &mut self,
        pending_key_id: ApiKeyId,
        serialized_key: &str,
        expected_authorization_epoch: AuthorizationEpoch,
        logical_time_micros: i64,
    ) -> Result<OwnerRecoveryActivationReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        if let Some(receipt) =
            self.replay_owner_recovery_activation(pending_key_id, expected_authorization_epoch)?
        {
            return Ok(receipt);
        }
        let mut catalog = self.load_access_control_catalog()?;
        let pending = catalog
            .pending_owner_recovery
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        if pending.key_id != pending_key_id
            || pending.authorization_epoch != expected_authorization_epoch
            || catalog.epoch != expected_authorization_epoch
        {
            return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
        }
        let verifier = catalog
            .keys
            .get(&pending_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?
            .verifier
            .clone();
        if !verifier.verifies(serialized_key) {
            return Err(ProductError::from_code(
                ProductErrorCode::ConfirmationDigestMismatch,
            ));
        }
        let principal_id = catalog
            .keys
            .get(&pending_key_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?
            .principal_id;
        let (authorization_epoch, retired_owner_keys) = catalog
            .activate_recovered_owner_key_with_retired(pending_key_id)
            .map_err(map_catalog_error)?;
        let mut targets = vec![
            SecurityAuditTarget::Principal(principal_id),
            SecurityAuditTarget::Key(pending_key_id),
        ];
        targets.extend(
            retired_owner_keys
                .iter()
                .copied()
                .map(SecurityAuditTarget::Key),
        );
        let audit = SecurityAuditDraft::offline(SecurityAuditAction::ActivateKey, targets);
        let (commit, _) = self.commit_access_control_catalog_recording_offline_recovery(
            &mut catalog,
            logical_time_micros,
            audit,
            OfflineOwnerRecoveryOutcome::Activated,
            pending,
        )?;
        Ok(OwnerRecoveryActivationReceipt {
            operation_id: pending.operation_id,
            key_id: pending_key_id,
            authorization_epoch,
            commit,
        })
    }

    /// Removes one exact inactive owner recovery without touching active keys.
    ///
    /// Exact replay after abort returns the retained abort commit.
    ///
    /// # Errors
    ///
    /// Returns not-found, conflict, durability, owner-authority, or corruption
    /// errors. No caller-supplied filesystem path is removed.
    pub fn abort_owner_recovery_offline(
        &mut self,
        pending_key_id: ApiKeyId,
        expected_authorization_epoch: AuthorizationEpoch,
        logical_time_micros: i64,
    ) -> Result<OwnerRecoveryAbortReceipt, ProductError> {
        self.database.revalidate_offline_owner()?;
        if let Some(receipt) =
            self.replay_owner_recovery_abort(pending_key_id, expected_authorization_epoch)?
        {
            return Ok(receipt);
        }
        let mut catalog = self.load_access_control_catalog()?;
        let (pending, authorization_epoch) = catalog
            .abort_owner_recovery(pending_key_id, expected_authorization_epoch)
            .map_err(map_catalog_error)?;
        let audit = SecurityAuditDraft::offline(
            SecurityAuditAction::AbortKeyIssue,
            [SecurityAuditTarget::Key(pending_key_id)],
        );
        let (commit, _) = self.commit_access_control_catalog_recording_offline_recovery(
            &mut catalog,
            logical_time_micros,
            audit,
            OfflineOwnerRecoveryOutcome::Aborted,
            pending,
        )?;
        Ok(OwnerRecoveryAbortReceipt {
            operation_id: pending.operation_id,
            key_id: pending_key_id,
            authorization_epoch,
            commit,
        })
    }

    pub(crate) fn load_access_control_catalog(&self) -> Result<AccessControlCatalog, ProductError> {
        #[cfg(test)]
        self.access_control_catalog_loads
            .fetch_add(1, Ordering::Relaxed);
        let snapshot = self.snapshot_bounded(0)?;
        match snapshot.structure_get_internal(ACCESS_CONTROL_STORAGE_KEY) {
            Some(encoded) => AccessControlCatalog::decode(encoded).map_err(map_catalog_error),
            None => Ok(AccessControlCatalog::empty()),
        }
    }

    fn replay_security_mutation(
        &self,
        actor: &AuthenticatedAuthority,
        idempotency_token: u128,
        operation: SecurityMutationOperation,
        request_digest: [u8; 32],
        logical_time_micros: i64,
    ) -> Result<Option<SecurityMutationReplay>, ProductError> {
        if idempotency_token == 0 {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let fingerprint = security_mutation_fingerprint(actor, idempotency_token);
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let Some(encoded) =
            snapshot.structure_get_internal(&security_mutation_marker_key(fingerprint))
        else {
            return Ok(None);
        };
        let marker = decode_security_mutation_marker(encoded).map_err(map_catalog_error)?;
        if marker.actor_principal_id != actor.principal_id
            || marker.actor_key_id != actor.key_id
            || marker.operation != operation
            || marker.request_digest != request_digest
        {
            return Err(ProductError::from_code(
                ProductErrorCode::IdempotencyConflict,
            ));
        }
        let transaction_id = TransactionId::new(marker.transaction_id)
            .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
        let commit = self
            .database
            .transaction_commit_receipt(transaction_id)
            .map(Into::into)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        Ok(Some(SecurityMutationReplay {
            result_id: marker.result_id,
            predecessor_key_id: marker.predecessor_key_id,
            overlap_until_micros: marker.overlap_until_micros,
            authorization_epoch: marker.authorization_epoch,
            commit,
        }))
    }

    pub(crate) fn is_exact_terminal_self_replay(
        &self,
        actor: &AuthenticatedAuthority,
        idempotency_token: u128,
        key_id: ApiKeyId,
        confirmation_digest: Option<ApiKeyConfirmationDigest>,
    ) -> Result<bool, ProductError> {
        let (operation, body) = match confirmation_digest {
            Some(digest) => (
                SecurityMutationOperation::RotateKeyActivate,
                key_target_request_body(key_id, true, Some(digest)),
            ),
            None => (
                SecurityMutationOperation::RevokeKey,
                key_target_request_body(key_id, true, None),
            ),
        };
        let request_digest =
            security_mutation_request_digest(operation, actor, idempotency_token, &body)?;
        self.replay_security_mutation(actor, idempotency_token, operation, request_digest, 0)
            .map(|replay| replay.is_some())
    }

    fn replay_owner_recovery_activation(
        &self,
        key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
    ) -> Result<Option<OwnerRecoveryActivationReceipt>, ProductError> {
        let Some((marker, commit)) = self.replay_owner_recovery_outcome(
            key_id,
            expected_epoch,
            OfflineOwnerRecoveryOutcome::Activated,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(OwnerRecoveryActivationReceipt {
            operation_id: marker.operation_id,
            key_id,
            authorization_epoch: marker.result_authorization_epoch,
            commit,
        }))
    }

    fn replay_legacy_bearer_migration_activation(
        &self,
        key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
        request_digest: [u8; 32],
    ) -> Result<Option<LegacyBearerMigrationActivationReceipt>, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        let Some(encoded) = snapshot.structure_get_internal(LEGACY_BEARER_MIGRATION_MARKER_KEY)
        else {
            return Ok(None);
        };
        let marker = decode_legacy_bearer_migration_marker(encoded).map_err(map_catalog_error)?;
        if marker.key_id != key_id
            || marker.expected_authorization_epoch != expected_epoch
            || marker.request_digest != request_digest
        {
            return Err(ProductError::from_code(
                ProductErrorCode::IdempotencyConflict,
            ));
        }
        let transaction_id = TransactionId::new(marker.transaction_id)
            .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
        let commit = self
            .database
            .transaction_commit_receipt(transaction_id)
            .map(Into::into)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        Ok(Some(LegacyBearerMigrationActivationReceipt {
            operation_id: marker.operation_id,
            key_id,
            authorization_epoch: marker.result_authorization_epoch,
            commit,
        }))
    }

    fn replay_owner_recovery_abort(
        &self,
        key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
    ) -> Result<Option<OwnerRecoveryAbortReceipt>, ProductError> {
        let Some((marker, commit)) = self.replay_owner_recovery_outcome(
            key_id,
            expected_epoch,
            OfflineOwnerRecoveryOutcome::Aborted,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(OwnerRecoveryAbortReceipt {
            operation_id: marker.operation_id,
            key_id,
            authorization_epoch: marker.result_authorization_epoch,
            commit,
        }))
    }

    fn replay_owner_recovery_outcome(
        &self,
        key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
        outcome: OfflineOwnerRecoveryOutcome,
    ) -> Result<Option<(OfflineOwnerRecoveryMarker, ProductCommitReceipt)>, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        let Some(encoded) =
            snapshot.structure_get_internal(&offline_owner_recovery_marker_key(key_id))
        else {
            return Ok(None);
        };
        let marker = decode_offline_owner_recovery_marker(encoded).map_err(map_catalog_error)?;
        if marker.key_id != key_id
            || marker.expected_authorization_epoch != expected_epoch
            || marker.outcome != outcome
        {
            return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
        }
        let transaction_id = TransactionId::new(marker.transaction_id)
            .map_err(|_| ProductError::from_code(ProductErrorCode::Corruption))?;
        let commit = self
            .database
            .transaction_commit_receipt(transaction_id)
            .map(Into::into)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Corruption))?;
        Ok(Some((marker, commit)))
    }

    fn commit_access_control_catalog(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
    ) -> Result<ProductCommitReceipt, ProductError> {
        self.commit_access_control_catalog_with_marker(
            catalog,
            logical_time_micros,
            audit,
            None,
            None,
        )
    }

    fn commit_access_control_catalog_recording_offline_recovery(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
        outcome: OfflineOwnerRecoveryOutcome,
        pending: PendingOwnerRecovery,
    ) -> Result<(ProductCommitReceipt, OfflineOwnerRecoveryMarker), ProductError> {
        self.commit_access_control_catalog_recording_offline_recovery_at(
            catalog,
            logical_time_micros,
            audit,
            outcome,
            pending,
            None,
        )
    }

    fn commit_access_control_catalog_recording_legacy_migration(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
        migration: LegacyBearerMigration,
    ) -> Result<(ProductCommitReceipt, LegacyBearerMigrationMarker), ProductError> {
        let mut transaction = self
            .database
            .begin(logical_time_micros, ProductDurability::Strict.into())?;
        let (transaction_id, pending_csn) = transaction.pending_commit_identity()?;
        let appended = catalog
            .append_audit_event(pending_csn.get(), audit)
            .map_err(map_catalog_error)?;
        transaction.set(
            audit_event_storage_key(SecurityAuditIndexEntry {
                id: appended.event.id,
                commit_csn: appended.event.commit_csn,
            }),
            encode_audit_event(&appended.event).map_err(map_catalog_error)?,
            None,
        )?;
        if let Some(evicted) = appended.evicted
            && !transaction.delete_structure(audit_event_storage_key(evicted))?
        {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        transaction.set(
            ACCESS_CONTROL_STORAGE_KEY.to_vec(),
            catalog.encode().map_err(map_catalog_error)?,
            None,
        )?;
        let marker = LegacyBearerMigrationMarker {
            operation_id: migration.operation_id,
            key_id: migration.key_id,
            expected_authorization_epoch: migration.authorization_epoch,
            result_authorization_epoch: catalog.epoch,
            request_digest: migration.request_digest,
            transaction_id: transaction_id.get(),
        };
        transaction.set(
            LEGACY_BEARER_MIGRATION_MARKER_KEY.to_vec(),
            encode_legacy_bearer_migration_marker(marker),
            None,
        )?;
        let commit_result = match self.security_commit_interruption.take() {
            Some(interruption) => interruption.commit(transaction),
            None => transaction.commit(),
        };
        let receipt = match commit_result {
            Ok(receipt) => receipt,
            Err(error) => {
                self.access_control_epoch_known
                    .store(false, Ordering::Release);
                return Err(error.into());
            }
        };
        if receipt.commit_csn != pending_csn {
            self.access_control_epoch_known
                .store(false, Ordering::Release);
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        self.observe_commit(&receipt);
        self.access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        self.access_control_epoch_known
            .store(true, Ordering::Release);
        Ok((receipt.into(), marker))
    }

    fn commit_access_control_catalog_recording_offline_recovery_at(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
        outcome: OfflineOwnerRecoveryOutcome,
        pending: PendingOwnerRecovery,
        interruption: Option<crate::SecurityCommitInterruption>,
    ) -> Result<(ProductCommitReceipt, OfflineOwnerRecoveryMarker), ProductError> {
        let mut transaction = self
            .database
            .begin(logical_time_micros, ProductDurability::Strict.into())?;
        let (transaction_id, pending_csn) = transaction.pending_commit_identity()?;
        let appended = catalog
            .append_audit_event(pending_csn.get(), audit)
            .map_err(map_catalog_error)?;
        transaction.set(
            audit_event_storage_key(SecurityAuditIndexEntry {
                id: appended.event.id,
                commit_csn: appended.event.commit_csn,
            }),
            encode_audit_event(&appended.event).map_err(map_catalog_error)?,
            None,
        )?;
        if let Some(evicted) = appended.evicted
            && !transaction.delete_structure(audit_event_storage_key(evicted))?
        {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        transaction.set(
            ACCESS_CONTROL_STORAGE_KEY.to_vec(),
            catalog.encode().map_err(map_catalog_error)?,
            None,
        )?;
        let marker = OfflineOwnerRecoveryMarker {
            outcome,
            operation_id: pending.operation_id,
            key_id: pending.key_id,
            expected_authorization_epoch: pending.authorization_epoch,
            result_authorization_epoch: catalog.epoch,
            transaction_id: transaction_id.get(),
        };
        transaction.set(
            offline_owner_recovery_marker_key(pending.key_id),
            encode_offline_owner_recovery_marker(marker),
            None,
        )?;
        let commit_result = match interruption.or_else(|| self.security_commit_interruption.take())
        {
            Some(interruption) => interruption.commit(transaction),
            None => transaction.commit(),
        };
        let receipt = match commit_result {
            Ok(receipt) => receipt,
            Err(error) => {
                self.access_control_epoch_known
                    .store(false, Ordering::Release);
                return Err(error.into());
            }
        };
        if receipt.commit_csn != pending_csn {
            self.access_control_epoch_known
                .store(false, Ordering::Release);
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        self.observe_commit(&receipt);
        self.access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        self.access_control_epoch_known
            .store(true, Ordering::Release);
        Ok((receipt.into(), marker))
    }

    fn commit_access_control_catalog_idempotent(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
        marker: SecurityMutationDraft,
    ) -> Result<ProductCommitReceipt, ProductError> {
        self.commit_access_control_catalog_with_marker(
            catalog,
            logical_time_micros,
            audit,
            Some(marker),
            None,
        )
    }

    fn commit_access_control_catalog_with_marker(
        &mut self,
        catalog: &mut AccessControlCatalog,
        logical_time_micros: i64,
        audit: SecurityAuditDraft,
        marker: Option<SecurityMutationDraft>,
        interruption: Option<crate::SecurityCommitInterruption>,
    ) -> Result<ProductCommitReceipt, ProductError> {
        let mut transaction = self
            .database
            .begin(logical_time_micros, ProductDurability::Strict.into())?;
        let (transaction_id, pending_csn) = transaction.pending_commit_identity()?;
        let appended = catalog
            .append_audit_event(pending_csn.get(), audit)
            .map_err(map_catalog_error)?;
        let encoded_catalog = catalog.encode().map_err(map_catalog_error)?;
        let encoded_event = encode_audit_event(&appended.event).map_err(map_catalog_error)?;
        transaction.set(
            audit_event_storage_key(SecurityAuditIndexEntry {
                id: appended.event.id,
                commit_csn: appended.event.commit_csn,
            }),
            encoded_event,
            None,
        )?;
        if let Some(evicted) = appended.evicted
            && !transaction.delete_structure(audit_event_storage_key(evicted))?
        {
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        transaction.set(ACCESS_CONTROL_STORAGE_KEY.to_vec(), encoded_catalog, None)?;
        if let Some(marker) = marker {
            let shard = security_mutation_shard(marker.fingerprint);
            let index_key = security_mutation_index_key(shard);
            let mut index = match transaction.get(&index_key) {
                Some(encoded) => {
                    decode_security_mutation_index(encoded, shard).map_err(map_catalog_error)?
                }
                None => SecurityMutationMarkerIndex {
                    fingerprints: Vec::new(),
                },
            };
            if index.fingerprints.contains(&marker.fingerprint) {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            if index.fingerprints.len() == SECURITY_MUTATION_MARKERS_PER_SHARD {
                let evicted = index.fingerprints.remove(0);
                if !transaction.delete_structure(security_mutation_marker_key(evicted))? {
                    return Err(ProductError::from_code(ProductErrorCode::Corruption));
                }
            }
            index.fingerprints.push(marker.fingerprint);
            transaction.set(
                index_key,
                encode_security_mutation_index(&index, shard).map_err(map_catalog_error)?,
                None,
            )?;
            transaction.set(
                security_mutation_marker_key(marker.fingerprint),
                encode_security_mutation_marker(SecurityMutationMarker {
                    operation: marker.operation,
                    request_digest: marker.request_digest,
                    actor_principal_id: marker.actor_principal_id,
                    actor_key_id: marker.actor_key_id,
                    result_id: marker.result_id,
                    predecessor_key_id: marker.predecessor_key_id,
                    overlap_until_micros: marker.overlap_until_micros,
                    authorization_epoch: marker.authorization_epoch,
                    transaction_id: transaction_id.get(),
                }),
                None,
            )?;
        }
        let commit_result = match interruption.or_else(|| self.security_commit_interruption.take())
        {
            Some(interruption) => interruption.commit(transaction),
            None => transaction.commit(),
        };
        let receipt = match commit_result {
            Ok(receipt) => receipt,
            Err(error) => {
                self.access_control_epoch_known
                    .store(false, Ordering::Release);
                return Err(error.into());
            }
        };
        if receipt.commit_csn != pending_csn {
            self.access_control_epoch_known
                .store(false, Ordering::Release);
            return Err(ProductError::from_code(ProductErrorCode::Corruption));
        }
        self.observe_commit(&receipt);
        self.access_control_epoch
            .store(catalog.epoch().get(), Ordering::Release);
        self.access_control_epoch_known
            .store(true, Ordering::Release);
        Ok(receipt.into())
    }
}

fn fresh_security_idempotency_token() -> Result<u128, ProductError> {
    SecurityId::generate()
        .map(SecurityId::get)
        .map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable))
}

fn security_mutation_fingerprint(
    actor: &AuthenticatedAuthority,
    idempotency_token: u128,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SECURITY_MUTATION_KEY_DOMAIN);
    hasher.update(&actor.principal_id.to_be_bytes());
    hasher.update(actor.key_id.as_bytes());
    hasher.update(&idempotency_token.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn offline_owner_recovery_marker_key(key_id: ApiKeyId) -> Vec<u8> {
    let mut key = Vec::with_capacity(OFFLINE_OWNER_RECOVERY_MARKER_PREFIX.len() + 16);
    key.extend_from_slice(OFFLINE_OWNER_RECOVERY_MARKER_PREFIX);
    key.extend_from_slice(key_id.as_bytes());
    key
}

fn encode_offline_owner_recovery_marker(marker: OfflineOwnerRecoveryMarker) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(OFFLINE_OWNER_RECOVERY_MARKER_BYTES);
    encoded.extend_from_slice(OFFLINE_OWNER_RECOVERY_MARKER_MAGIC);
    encoded.push(marker.outcome.tag());
    encoded.extend_from_slice(&marker.operation_id.to_be_bytes());
    encoded.extend_from_slice(marker.key_id.as_bytes());
    encoded.extend_from_slice(&marker.expected_authorization_epoch.get().to_be_bytes());
    encoded.extend_from_slice(&marker.result_authorization_epoch.get().to_be_bytes());
    encoded.extend_from_slice(&marker.transaction_id.to_be_bytes());
    let digest = blake3::hash(&encoded);
    encoded.extend_from_slice(digest.as_bytes());
    encoded
}

fn encode_legacy_bearer_migration_marker(marker: LegacyBearerMigrationMarker) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(LEGACY_BEARER_MIGRATION_MARKER_BYTES);
    encoded.extend_from_slice(LEGACY_BEARER_MIGRATION_MARKER_MAGIC);
    encoded.extend_from_slice(&marker.operation_id.to_be_bytes());
    encoded.extend_from_slice(marker.key_id.as_bytes());
    encoded.extend_from_slice(&marker.expected_authorization_epoch.get().to_be_bytes());
    encoded.extend_from_slice(&marker.result_authorization_epoch.get().to_be_bytes());
    encoded.extend_from_slice(&marker.request_digest);
    encoded.extend_from_slice(&marker.transaction_id.to_be_bytes());
    let digest = blake3::hash(&encoded);
    encoded.extend_from_slice(digest.as_bytes());
    encoded
}

fn decode_legacy_bearer_migration_marker(
    encoded: &[u8],
) -> Result<LegacyBearerMigrationMarker, AccessCatalogError> {
    if encoded.len() != LEGACY_BEARER_MIGRATION_MARKER_BYTES
        || &encoded[..LEGACY_BEARER_MIGRATION_MARKER_MAGIC.len()]
            != LEGACY_BEARER_MIGRATION_MARKER_MAGIC
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let content_len = encoded.len() - CATALOG_DIGEST_BYTES;
    if blake3::hash(&encoded[..content_len])
        .as_bytes()
        .ct_eq(&encoded[content_len..])
        .unwrap_u8()
        != 1
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut decoder =
        Decoder::new(&encoded[LEGACY_BEARER_MIGRATION_MARKER_MAGIC.len()..content_len]);
    let marker = LegacyBearerMigrationMarker {
        operation_id: decoder.security_id()?,
        key_id: ApiKeyId::from_bytes(decoder.array()?).ok_or(AccessCatalogError::CorruptCatalog)?,
        expected_authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
        result_authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
        request_digest: decoder.array()?,
        transaction_id: u128::from_be_bytes(decoder.array()?),
    };
    if !decoder.is_empty()
        || marker.expected_authorization_epoch == AuthorizationEpoch::UNMANAGED
        || marker.result_authorization_epoch.get()
            != marker.expected_authorization_epoch.get().saturating_add(1)
        || marker.transaction_id == 0
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(marker)
}

fn validate_legacy_bearer(legacy_bearer: &[u8]) -> Result<(), ProductError> {
    if !(32..=4_096).contains(&legacy_bearer.len())
        || !legacy_bearer
            .iter()
            .all(|byte| (0x21..=0x7e).contains(byte))
        || legacy_bearer.starts_with(b"hyp1_")
    {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    Ok(())
}

fn legacy_bearer_migration_request_digest(
    display_name: &str,
    key_label: &str,
    legacy_bearer: &[u8],
) -> Result<[u8; 32], ProductError> {
    validate_display_name(display_name).map_err(map_catalog_error)?;
    validate_display_name(key_label).map_err(map_catalog_error)?;
    validate_legacy_bearer(legacy_bearer)?;
    let bearer_digest = blake3::hash(legacy_bearer);
    let mut hasher = blake3::Hasher::new();
    hasher.update(LEGACY_BEARER_MIGRATION_REQUEST_DOMAIN);
    hasher.update(&(display_name.len() as u64).to_be_bytes());
    hasher.update(display_name.as_bytes());
    hasher.update(&(key_label.len() as u64).to_be_bytes());
    hasher.update(key_label.as_bytes());
    hasher.update(bearer_digest.as_bytes());
    Ok(*hasher.finalize().as_bytes())
}

fn legacy_bearer_verifier(
    legacy_bearer: &[u8],
    product_key: [u8; 32],
) -> Result<[u8; 32], ProductError> {
    validate_legacy_bearer(legacy_bearer)?;
    Ok(legacy_bearer_verifier_from_digest(
        *blake3::hash(legacy_bearer).as_bytes(),
        product_key,
    ))
}

fn legacy_bearer_verifier_from_digest(bearer_digest: [u8; 32], product_key: [u8; 32]) -> [u8; 32] {
    *blake3::keyed_hash(&product_key, &bearer_digest).as_bytes()
}

fn decode_offline_owner_recovery_marker(
    encoded: &[u8],
) -> Result<OfflineOwnerRecoveryMarker, AccessCatalogError> {
    if encoded.len() != OFFLINE_OWNER_RECOVERY_MARKER_BYTES
        || &encoded[..OFFLINE_OWNER_RECOVERY_MARKER_MAGIC.len()]
            != OFFLINE_OWNER_RECOVERY_MARKER_MAGIC
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let content_len = encoded.len() - CATALOG_DIGEST_BYTES;
    let digest = blake3::hash(&encoded[..content_len]);
    if digest.as_bytes().ct_eq(&encoded[content_len..]).unwrap_u8() != 1 {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut decoder =
        Decoder::new(&encoded[OFFLINE_OWNER_RECOVERY_MARKER_MAGIC.len()..content_len]);
    let marker = OfflineOwnerRecoveryMarker {
        outcome: OfflineOwnerRecoveryOutcome::from_tag(decoder.byte()?)
            .ok_or(AccessCatalogError::CorruptCatalog)?,
        operation_id: decoder.security_id()?,
        key_id: ApiKeyId::from_bytes(decoder.array()?).ok_or(AccessCatalogError::CorruptCatalog)?,
        expected_authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
        result_authorization_epoch: AuthorizationEpoch::new(decoder.u64()?),
        transaction_id: u128::from_be_bytes(decoder.array()?),
    };
    if !decoder.is_empty()
        || marker.expected_authorization_epoch == AuthorizationEpoch::UNMANAGED
        || marker.result_authorization_epoch == AuthorizationEpoch::UNMANAGED
        || marker.result_authorization_epoch.get()
            != marker.expected_authorization_epoch.get().saturating_add(1)
        || marker.transaction_id == 0
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(marker)
}

fn security_mutation_request_digest(
    operation: SecurityMutationOperation,
    actor: &AuthenticatedAuthority,
    idempotency_token: u128,
    body: &[u8],
) -> Result<[u8; 32], ProductError> {
    if idempotency_token == 0 {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    let body_len = u64::try_from(body.len())
        .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(SECURITY_MUTATION_REQUEST_DOMAIN);
    hasher.update(&[operation.tag()]);
    hasher.update(&actor.principal_id.to_be_bytes());
    hasher.update(actor.key_id.as_bytes());
    hasher.update(&idempotency_token.to_be_bytes());
    hasher.update(&body_len.to_be_bytes());
    hasher.update(body);
    Ok(*hasher.finalize().as_bytes())
}

const fn security_mutation_shard(fingerprint: [u8; 32]) -> u8 {
    fingerprint[0] % SECURITY_MUTATION_IDEMPOTENCY_SHARDS
}

fn security_mutation_marker_key(fingerprint: [u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(SECURITY_MUTATION_MARKER_PREFIX.len() + 1 + 32);
    key.extend_from_slice(SECURITY_MUTATION_MARKER_PREFIX);
    key.push(security_mutation_shard(fingerprint));
    key.extend_from_slice(&fingerprint);
    key
}

fn security_mutation_index_key(shard: u8) -> Vec<u8> {
    let mut key = Vec::with_capacity(SECURITY_MUTATION_INDEX_PREFIX.len() + 1);
    key.extend_from_slice(SECURITY_MUTATION_INDEX_PREFIX);
    key.push(shard);
    key
}

fn encode_security_mutation_marker(marker: SecurityMutationMarker) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(SECURITY_MUTATION_MARKER_BYTES);
    encoded.extend_from_slice(SECURITY_MUTATION_MARKER_MAGIC);
    encoded.push(marker.operation.tag());
    encoded.extend_from_slice(&marker.request_digest);
    encoded.extend_from_slice(&marker.actor_principal_id.to_be_bytes());
    encoded.extend_from_slice(marker.actor_key_id.as_bytes());
    match marker.result_id {
        SecurityMutationResultId::None => {
            encoded.push(0);
            encoded.extend_from_slice(&[0; 16]);
        }
        SecurityMutationResultId::Security(id) => {
            encoded.push(1);
            encoded.extend_from_slice(&id.to_be_bytes());
        }
        SecurityMutationResultId::ApiKey(id) => {
            encoded.push(2);
            encoded.extend_from_slice(id.as_bytes());
        }
    }
    encoded.extend_from_slice(
        &marker
            .predecessor_key_id
            .map_or([0; 16], |key_id| *key_id.as_bytes()),
    );
    encoded.push(u8::from(marker.overlap_until_micros.is_some()));
    encoded.extend_from_slice(&marker.overlap_until_micros.unwrap_or(0).to_be_bytes());
    encoded.extend_from_slice(&marker.authorization_epoch.get().to_be_bytes());
    encoded.extend_from_slice(&marker.transaction_id.to_be_bytes());
    let digest = blake3::hash(&encoded);
    encoded.extend_from_slice(digest.as_bytes());
    encoded
}

fn decode_security_mutation_marker(
    encoded: &[u8],
) -> Result<SecurityMutationMarker, AccessCatalogError> {
    let v1 = encoded.len() == SECURITY_MUTATION_MARKER_V1_BYTES
        && &encoded[..SECURITY_MUTATION_MARKER_V1_MAGIC.len()] == SECURITY_MUTATION_MARKER_V1_MAGIC;
    let v2 = encoded.len() == SECURITY_MUTATION_MARKER_V2_BYTES
        && &encoded[..SECURITY_MUTATION_MARKER_V2_MAGIC.len()] == SECURITY_MUTATION_MARKER_V2_MAGIC;
    let v3 = encoded.len() == SECURITY_MUTATION_MARKER_BYTES
        && &encoded[..SECURITY_MUTATION_MARKER_MAGIC.len()] == SECURITY_MUTATION_MARKER_MAGIC;
    if !v1 && !v2 && !v3 {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let content_len = encoded.len() - CATALOG_DIGEST_BYTES;
    let expected = blake3::hash(&encoded[..content_len]);
    if expected
        .as_bytes()
        .ct_eq(&encoded[content_len..])
        .unwrap_u8()
        != 1
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut decoder = Decoder::new(&encoded[SECURITY_MUTATION_MARKER_MAGIC.len()..content_len]);
    let operation = SecurityMutationOperation::from_tag(decoder.byte()?)
        .ok_or(AccessCatalogError::CorruptCatalog)?;
    let request_digest = decoder.array::<32>()?;
    let actor_principal_id = SecurityId::new(u128::from_be_bytes(decoder.array()?))
        .ok_or(AccessCatalogError::CorruptCatalog)?;
    let actor_key_id =
        ApiKeyId::from_bytes(decoder.array()?).ok_or(AccessCatalogError::CorruptCatalog)?;
    let result_id = if v1 {
        SecurityMutationResultId::Security(
            SecurityId::new(u128::from_be_bytes(decoder.array()?))
                .ok_or(AccessCatalogError::CorruptCatalog)?,
        )
    } else {
        let kind = decoder.byte()?;
        let bytes = decoder.array()?;
        match kind {
            0 if bytes == [0; 16] => SecurityMutationResultId::None,
            1 => SecurityMutationResultId::Security(
                SecurityId::new(u128::from_be_bytes(bytes))
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            2 => SecurityMutationResultId::ApiKey(
                ApiKeyId::from_bytes(bytes).ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            _ => return Err(AccessCatalogError::CorruptCatalog),
        }
    };
    let (predecessor_key_id, overlap_until_micros) = if v3 {
        let predecessor = decoder.array()?;
        let predecessor_key_id = if predecessor == [0; 16] {
            None
        } else {
            Some(ApiKeyId::from_bytes(predecessor).ok_or(AccessCatalogError::CorruptCatalog)?)
        };
        let overlap_present = decoder.boolean()?;
        let overlap = decoder.i64()?;
        let overlap_until_micros = if overlap_present {
            Some(overlap)
        } else if overlap == 0 {
            None
        } else {
            return Err(AccessCatalogError::CorruptCatalog);
        };
        (predecessor_key_id, overlap_until_micros)
    } else {
        (None, None)
    };
    let authorization_epoch = AuthorizationEpoch::new(u64::from_be_bytes(decoder.array()?));
    let transaction_id = u128::from_be_bytes(decoder.array()?);
    if !decoder.is_empty()
        || authorization_epoch == AuthorizationEpoch::UNMANAGED
        || predecessor_key_id.is_some() != overlap_until_micros.is_some()
        || transaction_id == 0
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(SecurityMutationMarker {
        operation,
        request_digest,
        actor_principal_id,
        actor_key_id,
        result_id,
        predecessor_key_id,
        overlap_until_micros,
        authorization_epoch,
        transaction_id,
    })
}

fn encode_security_mutation_index(
    index: &SecurityMutationMarkerIndex,
    shard: u8,
) -> Result<Vec<u8>, AccessCatalogError> {
    if shard >= SECURITY_MUTATION_IDEMPOTENCY_SHARDS
        || index.fingerprints.len() > SECURITY_MUTATION_MARKERS_PER_SHARD
        || !strictly_unique(&index.fingerprints)
        || index
            .fingerprints
            .iter()
            .any(|fingerprint| security_mutation_shard(*fingerprint) != shard)
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut encoded = Vec::with_capacity(10 + index.fingerprints.len() * 32 + 32);
    encoded.extend_from_slice(SECURITY_MUTATION_INDEX_MAGIC);
    encoded.push(shard);
    encoded.push(
        u8::try_from(index.fingerprints.len()).map_err(|_| AccessCatalogError::LimitExceeded)?,
    );
    for fingerprint in &index.fingerprints {
        encoded.extend_from_slice(fingerprint);
    }
    let digest = blake3::hash(&encoded);
    encoded.extend_from_slice(digest.as_bytes());
    Ok(encoded)
}

fn decode_security_mutation_index(
    encoded: &[u8],
    expected_shard: u8,
) -> Result<SecurityMutationMarkerIndex, AccessCatalogError> {
    let minimum = SECURITY_MUTATION_INDEX_MAGIC.len() + 2 + CATALOG_DIGEST_BYTES;
    if encoded.len() < minimum
        || &encoded[..SECURITY_MUTATION_INDEX_MAGIC.len()] != SECURITY_MUTATION_INDEX_MAGIC
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let content_len = encoded.len() - CATALOG_DIGEST_BYTES;
    let expected_digest = blake3::hash(&encoded[..content_len]);
    if expected_digest
        .as_bytes()
        .ct_eq(&encoded[content_len..])
        .unwrap_u8()
        != 1
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut decoder = Decoder::new(&encoded[SECURITY_MUTATION_INDEX_MAGIC.len()..content_len]);
    let shard = decoder.byte()?;
    let count = usize::from(decoder.byte()?);
    if shard != expected_shard
        || count > SECURITY_MUTATION_MARKERS_PER_SHARD
        || decoder.remaining.len() != count.saturating_mul(32)
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let fingerprints = (0..count)
        .map(|_| decoder.array::<32>())
        .collect::<Result<Vec<_>, _>>()?;
    let index = SecurityMutationMarkerIndex { fingerprints };
    encode_security_mutation_index(&index, shard)?;
    Ok(index)
}

fn strictly_unique<T: Ord + Clone>(values: &[T]) -> bool {
    values.iter().cloned().collect::<BTreeSet<_>>().len() == values.len()
}

fn map_catalog_error(error: AccessCatalogError) -> ProductError {
    let code = match error {
        AccessCatalogError::AlreadyBootstrapped | AccessCatalogError::Conflict => {
            ProductErrorCode::CatalogConflict
        }
        AccessCatalogError::InvalidDisplayName | AccessCatalogError::InvalidRequest => {
            ProductErrorCode::InvalidRequest
        }
        AccessCatalogError::CursorExpired => ProductErrorCode::InvalidRequest,
        AccessCatalogError::Entropy => ProductErrorCode::Unavailable,
        AccessCatalogError::LimitExceeded => ProductErrorCode::LimitExceeded,
        AccessCatalogError::NotFound => ProductErrorCode::ObjectNotFound,
        AccessCatalogError::Unauthorized => ProductErrorCode::AuthorizationDenied,
        AccessCatalogError::CorruptCatalog => ProductErrorCode::Corruption,
    };
    ProductError::from_code(code)
}

fn current_actor(
    catalog: &AccessControlCatalog,
    actor: &AuthenticatedAuthority,
    directory_lineage: [u8; 24],
    logical_time_micros: i64,
) -> Result<AuthenticatedAuthority, ProductError> {
    if actor.directory_lineage != directory_lineage {
        return Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ));
    }
    let current = catalog
        .authority_for_key(actor.key_id, logical_time_micros, directory_lineage)
        .map_err(map_catalog_error)?;
    if current.principal_id != actor.principal_id {
        return Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ));
    }
    Ok(current)
}

fn require_current_actor(
    catalog: &AccessControlCatalog,
    actor: &AuthenticatedAuthority,
    permission: ProductPermission,
    directory_lineage: [u8; 24],
    logical_time_micros: i64,
) -> Result<AuthenticatedAuthority, ProductError> {
    let current = current_actor(catalog, actor, directory_lineage, logical_time_micros)?;
    if !authority_allows_instance(&current, permission) {
        return Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ));
    }
    Ok(current)
}

fn authorize_key_issue(
    product: &NativeProduct,
    actor: &AuthenticatedAuthority,
    principal_id: SecurityId,
    requested_roles: &BTreeSet<BuiltInRole>,
    requested_custom_roles: &BTreeSet<SecurityId>,
    permission_ceiling: ProductAuthorization,
    scope_ceiling: &BTreeSet<ProductScope>,
) -> Result<(), ProductError> {
    let denied = || ProductError::from_code(ProductErrorCode::AuthorizationDenied);
    if requested_roles.contains(&BuiltInRole::Owner)
        && !authority_allows_instance(actor, ProductPermission::OwnershipManage)
    {
        return Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ));
    }
    if principal_id != actor.principal_id {
        return if authority_allows_instance(actor, ProductPermission::SecurityManage) {
            Ok(())
        } else {
            Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ))
        };
    }
    let effective_roles: BTreeSet<_> = actor.effective_roles.iter().copied().collect();
    let effective_custom_roles: BTreeSet<_> =
        actor.effective_custom_roles.iter().copied().collect();
    let scopes_are_covered =
        authority_covers_scope_ceiling(product, actor, permission_ceiling, scope_ceiling).map_err(
            |error| match error.code() {
                ProductErrorCode::Io
                | ProductErrorCode::Unavailable
                | ProductErrorCode::Corruption => error,
                _ => denied(),
            },
        )?;
    if actor
        .authorization
        .allows(ProductPermission::CredentialSelfManage)
        && requested_roles.is_subset(&effective_roles)
        && requested_custom_roles.is_subset(&effective_custom_roles)
        && permission_ceiling.is_subset_of(actor.authorization)
        && scopes_are_covered
    {
        Ok(())
    } else {
        Err(denied())
    }
}

#[allow(clippy::too_many_arguments)]
fn authorize_key_issue_variant(
    product: &NativeProduct,
    actor: &AuthenticatedAuthority,
    principal_id: SecurityId,
    requested_roles: &BTreeSet<BuiltInRole>,
    requested_custom_roles: &BTreeSet<SecurityId>,
    permission_ceiling: ProductAuthorization,
    scope_ceiling: &BTreeSet<ProductScope>,
    self_manage: bool,
) -> Result<(), ProductError> {
    if self_manage && principal_id != actor.principal_id {
        return Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ));
    }
    if self_manage {
        authorize_key_issue(
            product,
            actor,
            principal_id,
            requested_roles,
            requested_custom_roles,
            permission_ceiling,
            scope_ceiling,
        )
    } else if authority_allows_instance(actor, ProductPermission::SecurityManage)
        && (!requested_roles.contains(&BuiltInRole::Owner)
            || authority_allows_instance(actor, ProductPermission::OwnershipManage))
    {
        Ok(())
    } else {
        Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ))
    }
}

fn authority_covers_scope_ceiling(
    product: &NativeProduct,
    actor: &AuthenticatedAuthority,
    permission_ceiling: ProductAuthorization,
    requested: &BTreeSet<ProductScope>,
) -> Result<bool, ProductError> {
    if requested.is_empty() {
        return Ok(false);
    }
    let object_ids = requested
        .iter()
        .chain(actor.scope_ceiling.iter())
        .chain(
            actor
                .scoped_authorization
                .iter()
                .map(|scoped| &scoped.scope),
        )
        .filter_map(|scope| match scope {
            ProductScope::Instance => None,
            ProductScope::CatalogSubtree(object) | ProductScope::CatalogObject(object) => {
                Some(*object)
            }
        })
        .collect::<BTreeSet<_>>();
    let snapshot = product.catalog_snapshot()?;
    let mut ancestry = BTreeMap::new();
    for target in object_ids {
        let mut ancestors = BTreeSet::new();
        let mut current = target;
        while let Some(object) = product.catalog_describe(&snapshot, current)? {
            let Some(parent) = object.parent() else {
                break;
            };
            if !ancestors.insert(parent) {
                return Err(ProductError::from_code(ProductErrorCode::Corruption));
            }
            current = parent;
        }
        if product.catalog_describe(&snapshot, target)?.is_none() {
            return Ok(false);
        }
        ancestry.insert(target, ancestors);
    }
    let is_descendant = |candidate: ObjectId, ancestor: ObjectId| {
        ancestry
            .get(&candidate)
            .is_some_and(|ancestors| ancestors.contains(&ancestor))
    };
    Ok(requested.iter().copied().all(|requested_scope| {
        every_required_permission(permission_ceiling, |permission| {
            if !permission.supports_scope(requested_scope) {
                return true;
            }
            actor
                .scope_ceiling
                .iter()
                .copied()
                .any(|ceiling| scope_covers_scope(ceiling, requested_scope, is_descendant))
                && actor.scoped_authorization.iter().any(|scoped| {
                    scoped.authorization.allows(permission)
                        && scope_covers_scope(scoped.scope, requested_scope, is_descendant)
                })
        })
    }))
}

fn scope_covers_scope(
    available: ProductScope,
    requested: ProductScope,
    is_descendant: impl Fn(ObjectId, ObjectId) -> bool,
) -> bool {
    match (available, requested) {
        (ProductScope::Instance, _) => true,
        (_, ProductScope::Instance)
        | (ProductScope::CatalogObject(_), ProductScope::CatalogSubtree(_)) => false,
        (ProductScope::CatalogObject(available), ProductScope::CatalogObject(requested)) => {
            available == requested
        }
        (
            ProductScope::CatalogSubtree(available),
            ProductScope::CatalogObject(requested) | ProductScope::CatalogSubtree(requested),
        ) => available == requested || is_descendant(requested, available),
    }
}

fn authorize_key_target_variant(
    actor: &AuthenticatedAuthority,
    target: &ApiKeyRecord,
    self_manage: bool,
) -> Result<(), ProductError> {
    let owner = target.roles.contains(&BuiltInRole::Owner);
    let allowed = if self_manage {
        target.principal_id == actor.principal_id
            && authority_allows_instance(actor, ProductPermission::CredentialSelfManage)
    } else {
        authority_allows_instance(actor, ProductPermission::SecurityManage)
    };
    if allowed && (!owner || authority_allows_instance(actor, ProductPermission::OwnershipManage)) {
        Ok(())
    } else {
        Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ))
    }
}

fn authorize_key_rotation_target(
    catalog: &AccessControlCatalog,
    actor: &AuthenticatedAuthority,
    target: &ApiKeyRecord,
    self_manage: bool,
) -> Result<(), ProductError> {
    authorize_key_target_variant(actor, target, self_manage)?;
    if target.id == actor.key_id || actor.effective_roles.contains(&BuiltInRole::Owner) {
        return Ok(());
    }

    let target_authority = catalog
        .authority_for_key_lifecycle(target.id, i64::MIN, actor.directory_lineage, false)
        .map_err(|_| ProductError::from_code(ProductErrorCode::AuthorizationDenied))?;
    let actor_roles = actor
        .effective_roles
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let actor_custom_roles = actor
        .effective_custom_roles
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let target_roles = target_authority
        .effective_roles
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let target_custom_roles = target_authority
        .effective_custom_roles
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let actor_scopes = actor.scope_ceiling.iter().copied().collect::<BTreeSet<_>>();
    let target_scopes = target
        .scope_ceiling
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let scopes_are_subset =
        actor_scopes.contains(&ProductScope::Instance) || target_scopes.is_subset(&actor_scopes);

    if target_authority
        .authorization
        .is_subset_of(actor.authorization)
        && target_roles.is_subset(&actor_roles)
        && target_custom_roles.is_subset(&actor_custom_roles)
        && target.permission_ceiling.is_subset_of(actor.authorization)
        && scopes_are_subset
    {
        Ok(())
    } else {
        Err(ProductError::from_code(
            ProductErrorCode::AuthorizationDenied,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn key_issue_request_body(
    principal_id: SecurityId,
    label: &str,
    roles: &BTreeSet<BuiltInRole>,
    custom_roles: &BTreeSet<SecurityId>,
    permission_ceiling: ProductAuthorization,
    scope_ceiling: &BTreeSet<ProductScope>,
    expires_at_micros: Option<i64>,
    self_manage: bool,
) -> Result<Vec<u8>, ProductError> {
    let mut body = Vec::new();
    body.extend_from_slice(&principal_id.to_be_bytes());
    body.push(u8::from(self_manage));
    body.extend_from_slice(
        &u64::try_from(label.len())
            .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
            .to_be_bytes(),
    );
    body.extend_from_slice(label.as_bytes());
    body.extend_from_slice(
        &u64::try_from(roles.len())
            .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
            .to_be_bytes(),
    );
    body.extend(roles.iter().map(|role| role.tag()));
    body.extend_from_slice(
        &u64::try_from(custom_roles.len())
            .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
            .to_be_bytes(),
    );
    for role_id in custom_roles {
        body.extend_from_slice(&role_id.to_be_bytes());
    }
    body.extend_from_slice(&permission_ceiling.bits().to_be_bytes());
    body.extend_from_slice(
        &u64::try_from(scope_ceiling.len())
            .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
            .to_be_bytes(),
    );
    for scope in scope_ceiling {
        encode_scope(&mut body, *scope);
    }
    encode_optional_i64(&mut body, expires_at_micros);
    Ok(body)
}

fn key_rotation_request_body(
    predecessor_key_id: ApiKeyId,
    label: &str,
    overlap_seconds: u64,
    expires_at_micros: Option<i64>,
    self_manage: bool,
) -> Result<Vec<u8>, ProductError> {
    let mut body = Vec::new();
    body.extend_from_slice(predecessor_key_id.as_bytes());
    body.push(u8::from(self_manage));
    body.extend_from_slice(&overlap_seconds.to_be_bytes());
    body.extend_from_slice(
        &u64::try_from(label.len())
            .map_err(|_| ProductError::from_code(ProductErrorCode::LimitExceeded))?
            .to_be_bytes(),
    );
    body.extend_from_slice(label.as_bytes());
    encode_optional_i64(&mut body, expires_at_micros);
    Ok(body)
}

fn key_target_request_body(
    key_id: ApiKeyId,
    self_manage: bool,
    confirmation_digest: Option<ApiKeyConfirmationDigest>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(49);
    body.extend_from_slice(key_id.as_bytes());
    body.push(u8::from(self_manage));
    if let Some(digest) = confirmation_digest {
        body.extend_from_slice(digest.as_bytes());
    }
    body
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod public_key_lifecycle_tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyphae-public-key-lifecycle-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn public_issue_start_is_one_time_pending_and_exactly_activates()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = directory("issue");
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir(&root)?;
        let data = root.join("data");
        let owner_file = root.join("owner.key");
        let mut product = NativeProduct::create(&data)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_file, 1)?;
        let owner_secret = fs::read_to_string(&owner_file)?;
        let actor = product.authenticate_api_key(&owner_secret, 2)?;
        let started = product.start_api_key_issue_idempotent(
            &actor,
            actor.principal_id(),
            "pending",
            [BuiltInRole::Owner],
            [],
            ProductAuthorization::ALL,
            [ProductScope::Instance],
            None,
            11,
            3,
            true,
        )?;
        let pending = started.secret.take().ok_or("missing one-time secret")?;
        assert!(started.secret.is_consumed());
        assert_eq!(
            product
                .start_api_key_issue_idempotent(
                    &actor,
                    actor.principal_id(),
                    "pending",
                    [BuiltInRole::Owner],
                    [],
                    ProductAuthorization::ALL,
                    [ProductScope::Instance],
                    None,
                    11,
                    3,
                    true,
                )
                .expect_err("start replay returned a secret")
                .code(),
            ProductErrorCode::SecretDeliveryConsumed
        );
        assert_eq!(
            product
                .authenticate_api_key(pending.expose_secret(), 4)
                .expect_err("pending key authenticated")
                .code(),
            ProductErrorCode::AuthorizationDenied
        );
        let mut wrong = *pending.confirmation_digest().as_bytes();
        wrong[0] ^= 1;
        assert_eq!(
            product
                .activate_api_key_issue_idempotent(
                    &actor,
                    started.key_id,
                    ApiKeyConfirmationDigest::from_bytes(wrong),
                    12,
                    5,
                    true,
                )
                .expect_err("wrong digest activated")
                .code(),
            ProductErrorCode::ConfirmationDigestMismatch
        );
        let activated = product.activate_api_key_issue_idempotent(
            &actor,
            started.key_id,
            pending.confirmation_digest(),
            13,
            6,
            true,
        )?;
        let replay = product.activate_api_key_issue_idempotent(
            &actor,
            started.key_id,
            pending.confirmation_digest(),
            13,
            7,
            true,
        )?;
        assert_eq!(activated, replay);
        assert!(
            product
                .authenticate_api_key(pending.expose_secret(), 8)
                .is_ok()
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn public_rotation_supports_zero_and_maximum_overlap_and_exact_abort()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = directory("rotation");
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir(&root)?;
        let data = root.join("data");
        let owner_file = root.join("owner.key");
        let mut product = NativeProduct::create(&data)?;
        let bootstrap =
            product.bootstrap_access_control_to_file("Owner", "owner", &owner_file, 1)?;
        let owner_secret = fs::read_to_string(&owner_file)?;
        let actor = product.authenticate_api_key(&owner_secret, 2)?;
        let pending = product.start_api_key_rotation_idempotent(
            &actor,
            bootstrap.key_id,
            "pending rotation",
            AccessControlLimits::V1.maximum_rotation_overlap_seconds,
            None,
            21,
            3,
            true,
        )?;
        product.abort_api_key_rotation_idempotent(&actor, pending.key_id, 22, 4, true)?;
        let started = product.start_api_key_rotation_idempotent(
            &actor,
            bootstrap.key_id,
            "zero rotation",
            0,
            None,
            23,
            5,
            true,
        )?;
        let secret = started.secret.take().ok_or("missing rotation secret")?;
        let activated = product.activate_api_key_rotation_idempotent(
            &actor,
            started.key_id,
            secret.confirmation_digest(),
            24,
            6,
            true,
        )?;
        assert_eq!(activated.predecessor_key_id, Some(bootstrap.key_id));
        assert!(
            product
                .authenticate_api_key(secret.expose_secret(), 7)
                .is_ok()
        );
        assert_eq!(
            product
                .authenticate_api_key(&owner_secret, 7)
                .expect_err("zero-overlap predecessor remained active")
                .code(),
            ProductErrorCode::AuthorizationDenied
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn reader_cannot_rotate_same_principal_admin_key_but_can_rotate_own_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = directory("rotation-authority");
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir(&root)?;
        let data = root.join("data");
        let owner_file = root.join("owner.key");
        let admin_file = root.join("admin.key");
        let reader_file = root.join("reader.key");
        let mut product = NativeProduct::create(&data)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_file, 1)?;
        let owner_secret = fs::read_to_string(&owner_file)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let principal = product.create_security_principal(&owner, "Mixed authority", 2)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.set_security_principal_enabled_idempotent(
            &owner,
            principal.principal_id,
            true,
            101,
            3,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Admin,
            ProductScope::Instance,
            4,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            5,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let admin = product.issue_api_key_to_file(
            &owner,
            principal.principal_id,
            "admin",
            [BuiltInRole::Admin],
            BuiltInRole::Admin.authorization(),
            None,
            &admin_file,
            6,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let reader = product.issue_api_key_to_file(
            &owner,
            principal.principal_id,
            "reader",
            [BuiltInRole::Reader],
            BuiltInRole::Reader.authorization(),
            None,
            &reader_file,
            7,
        )?;
        let reader_secret = fs::read_to_string(&reader_file)?;
        let actor = product.authenticate_api_key(&reader_secret, i64::MAX)?;

        let denied = product
            .start_api_key_rotation_idempotent(
                &actor,
                admin.key_id,
                "forbidden",
                0,
                None,
                102,
                8,
                true,
            )
            .expect_err("reader rotated a same-principal admin key");
        let unknown = product
            .start_api_key_rotation_idempotent(
                &actor,
                ApiKeyId::from_bytes([0xff; 16]).ok_or("invalid test key")?,
                "unknown",
                0,
                None,
                103,
                8,
                true,
            )
            .expect_err("unknown key disclosed rotation state");
        assert_eq!(denied.code(), ProductErrorCode::AuthorizationDenied);
        assert_eq!(unknown.code(), denied.code());

        let own = product.start_api_key_rotation_idempotent(
            &actor,
            reader.key_id,
            "own",
            0,
            None,
            104,
            9,
            true,
        )?;
        assert_eq!(own.predecessor_key_id, Some(reader.key_id));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn interrupted_start_recovers_as_consumed_without_secret_redelivery()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = directory("interrupted-start");
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir(&root)?;
        let data = root.join("data");
        let owner_file = root.join("owner.key");
        let mut product = NativeProduct::create(&data)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_file, 1)?;
        let owner_secret = fs::read_to_string(&owner_file)?;
        let actor = product.authenticate_api_key(&owner_secret, 2)?;
        assert!(
            product
                .start_api_key_issue_idempotent_with_interruption(
                    &actor,
                    actor.principal_id(),
                    "crashed",
                    [BuiltInRole::Owner],
                    [],
                    ProductAuthorization::ALL,
                    [ProductScope::Instance],
                    None,
                    31,
                    3,
                    true,
                    Some(crate::SecurityCommitInterruption::returning(
                        hyphae_native_runtime::CommitBoundary::RootPublished,
                    )),
                )
                .is_err()
        );
        drop(product);
        let mut reopened = NativeProduct::open(&data)?;
        let actor = reopened.authenticate_api_key(&owner_secret, 4)?;
        assert_eq!(
            reopened
                .start_api_key_issue_idempotent(
                    &actor,
                    actor.principal_id(),
                    "crashed",
                    [BuiltInRole::Owner],
                    [],
                    ProductAuthorization::ALL,
                    [ProductScope::Instance],
                    None,
                    31,
                    5,
                    true,
                )
                .expect_err("interrupted committed start redelivered a secret")
                .code(),
            ProductErrorCode::SecretDeliveryConsumed
        );
        assert_eq!(reopened.access_control_status()?.pending_keys, 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}

fn authority_allows_instance(
    authority: &AuthenticatedAuthority,
    permission: ProductPermission,
) -> bool {
    authority.allows_instance(permission)
}

fn trusted_wall_time_micros() -> Result<i64, ProductError> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable))?
        .as_micros();
    i64::try_from(micros).map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable))
}

fn create_restricted_output(path: &Path) -> Result<File, ProductError> {
    #[cfg(windows)]
    if is_windows_named_stream(path) {
        return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{Foundation::GENERIC_WRITE, Storage::FileSystem::READ_CONTROL};

        // An exclusive handle prevents another process from acquiring the
        // inherited ACL before the protected DACL is installed and verified.
        options
            .access_mode(GENERIC_WRITE | READ_CONTROL)
            .share_mode(0);
    }
    #[allow(unused_mut)]
    let mut file = options
        .open(path)
        .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
    #[cfg(windows)]
    if apply_windows_restricted_acl(path).is_err()
        || validate_windows_restricted_file(&file).is_err()
    {
        drop(file);
        remove_empty_output(path);
        return Err(ProductError::from_code(ProductErrorCode::Io));
    }
    if file.sync_all().is_err() || sync_output_parent(path).is_err() {
        drop(file);
        remove_empty_output(path);
        return Err(ProductError::from_code(ProductErrorCode::Io));
    }
    Ok(file)
}

#[cfg(windows)]
fn is_windows_named_stream(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    path.components().any(|component| match component {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => false,
            _ => prefix.as_os_str().to_string_lossy().contains(':'),
        },
        Component::Normal(value) => value.to_string_lossy().contains(':'),
        Component::RootDir | Component::CurDir | Component::ParentDir => false,
    })
}

#[cfg(windows)]
fn apply_windows_restricted_acl(path: &Path) -> std::io::Result<()> {
    use windows_permissions::{
        LocalBox, SecurityDescriptor,
        constants::{SeObjectType, SecurityInformation},
        utilities, wrappers,
    };

    let current_user_sid = utilities::current_process_sid()?;
    let current_user = current_user_sid.to_string();
    let system = "S-1-5-18";
    let sddl = if current_user == system {
        format!("D:P(A;;FA;;;{system})")
    } else {
        format!("D:P(A;;FA;;;{current_user})(A;;FA;;;{system})")
    };
    let descriptor: LocalBox<SecurityDescriptor> = sddl.parse()?;
    wrappers::SetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        descriptor.dacl(),
        None,
    )?;
    wrappers::SetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner,
        Some(current_user_sid.as_ref()),
        None,
        None,
        None,
    )
}

#[cfg(windows)]
/// Applies and validates the platform account-only DACL for a new credential file.
///
/// # Errors
///
/// Returns an I/O error when the DACL cannot be installed or validated.
pub fn restrict_windows_credential_file(path: &Path, file: &File) -> std::io::Result<()> {
    apply_windows_restricted_acl(path)?;
    validate_windows_restricted_file(file)
}

#[cfg(windows)]
/// Validates the account-only protected DACL on an opened credential file.
///
/// # Errors
///
/// Returns an I/O or permission error when the owner or DACL differs from the
/// current process account plus LocalSystem authority.
pub fn validate_windows_restricted_file(file: &File) -> std::io::Result<()> {
    use windows_permissions::{
        LocalBox, Sid,
        constants::{AccessRights, AceType, SeObjectType, SecurityInformation},
        utilities, wrappers,
    };

    let actual = wrappers::GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    let sddl = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
        &actual,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    let current_user = utilities::current_process_sid()?.to_string();
    let current_user_sid = utilities::current_process_sid()?;
    let system_sid: LocalBox<Sid> = "S-1-5-18".parse()?;
    if actual.owner() != Some(current_user_sid.as_ref())
        || !windows_sddl_has_protected_dacl(&sddl.to_string_lossy())
    {
        return Err(windows_restricted_acl_error());
    }
    let dacl = actual.dacl().ok_or_else(windows_restricted_acl_error)?;
    let expected_aces = if current_user == "S-1-5-18" { 1 } else { 2 };
    if dacl.len() != expected_aces {
        return Err(windows_restricted_acl_error());
    }
    let mut current_user_seen = false;
    let mut system_seen = false;
    for index in 0..dacl.len() {
        let ace = dacl
            .get_ace(index)
            .ok_or_else(windows_restricted_acl_error)?;
        if ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
            || !ace.flags().is_empty()
            || ace.mask() != AccessRights::FileAllAccess
        {
            return Err(windows_restricted_acl_error());
        }
        let sid = ace.sid().ok_or_else(windows_restricted_acl_error)?;
        if sid == current_user_sid.as_ref() {
            if current_user_seen {
                return Err(windows_restricted_acl_error());
            }
            current_user_seen = true;
        } else if sid == system_sid.as_ref() {
            if system_seen {
                return Err(windows_restricted_acl_error());
            }
            system_seen = true;
        } else {
            return Err(windows_restricted_acl_error());
        }
    }
    if !current_user_seen || (current_user != "S-1-5-18" && !system_seen) {
        return Err(windows_restricted_acl_error());
    }
    Ok(())
}

#[cfg(windows)]
fn windows_sddl_has_protected_dacl(actual: &str) -> bool {
    let Some(dacl_start) = actual.find("D:") else {
        return false;
    };
    let dacl = &actual[dacl_start + 2..];
    let Some(aces_start) = dacl.find('(') else {
        return false;
    };
    dacl[..aces_start].contains('P')
}

#[cfg(windows)]
fn windows_restricted_acl_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "credential file ACL is not restricted to the current account and LocalSystem",
    )
}

fn remove_empty_output(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0) {
        let _ignored = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
fn sync_output_parent(path: &Path) -> Result<(), ProductError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ProductError::from_code(ProductErrorCode::Io))
}

#[cfg(not(unix))]
fn sync_output_parent(_path: &Path) -> Result<(), ProductError> {
    Ok(())
}

fn decode_principals(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<SecurityId, SecurityPrincipalRecord>, AccessCatalogError> {
    let mut principals = BTreeMap::new();
    for _ in 0..count {
        let id = decoder.security_id()?;
        let record = SecurityPrincipalRecord {
            id,
            enabled: decoder.boolean()?,
            display_name: decoder.string()?.into_boxed_str(),
        };
        if principals.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(principals)
}

fn decode_assignments(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<SecurityId, BuiltInRoleAssignment>, AccessCatalogError> {
    let mut assignments = BTreeMap::new();
    for _ in 0..count {
        let id = decoder.security_id()?;
        let record = BuiltInRoleAssignment {
            id,
            principal_id: decoder.security_id()?,
            role: BuiltInRole::from_tag(decoder.byte()?)
                .ok_or(AccessCatalogError::CorruptCatalog)?,
            scope: decoder.scope()?,
        };
        if assignments.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(assignments)
}

fn decode_custom_roles(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<SecurityId, CustomRoleRecord>, AccessCatalogError> {
    let mut roles = BTreeMap::new();
    for _ in 0..count {
        let id = decoder.security_id()?;
        let display_name = decoder.string()?.into_boxed_str();
        let grant_count = usize::from(decoder.u16()?);
        if grant_count == 0 || grant_count > AccessControlLimits::V1.grants_per_role {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut grants = Vec::with_capacity(grant_count);
        for _ in 0..grant_count {
            let permission = ProductPermission::from_tag(decoder.byte()?)
                .ok_or(AccessCatalogError::CorruptCatalog)?;
            let scope = decoder.scope()?;
            grants.push(
                CustomRoleGrant::new(permission, scope)
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
            );
        }
        if !strictly_sorted(&grants) {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let record = CustomRoleRecord {
            id,
            display_name,
            grants: grants.into_boxed_slice(),
        };
        if roles.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(roles)
}

fn decode_custom_assignments(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<SecurityId, CustomRoleAssignment>, AccessCatalogError> {
    let mut assignments = BTreeMap::new();
    for _ in 0..count {
        let id = decoder.security_id()?;
        let record = CustomRoleAssignment {
            id,
            principal_id: decoder.security_id()?,
            role_id: decoder.security_id()?,
        };
        if assignments.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(assignments)
}

fn encode_key_v1(output: &mut Vec<u8>, key: &ApiKeyRecord) -> Result<(), AccessCatalogError> {
    output.extend_from_slice(key.id.as_bytes());
    output.extend_from_slice(&key.principal_id.to_be_bytes());
    output.extend_from_slice(key.verifier.digest());
    output.push(u8::from(key.active));
    output.push(u8::from(key.revoked));
    output.extend_from_slice(&key.created_at_micros.to_be_bytes());
    encode_optional_i64(output, key.expires_at_micros);
    output.extend_from_slice(&key.published_epoch.get().to_be_bytes());
    output.extend_from_slice(&key.permission_ceiling.bits().to_be_bytes());
    output.push(u8::try_from(key.roles.len()).map_err(|_| AccessCatalogError::LimitExceeded)?);
    output.extend(key.roles.iter().map(|role| role.tag()));
    push_string(output, &key.label)
}

fn encode_key_v2(output: &mut Vec<u8>, key: &ApiKeyRecord) -> Result<(), AccessCatalogError> {
    encode_key_v1(output, key)?;
    output.extend_from_slice(
        &u16::try_from(key.custom_roles.len())
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    for role_id in key.custom_roles.iter().copied() {
        output.extend_from_slice(&role_id.to_be_bytes());
    }
    output.extend_from_slice(
        &u16::try_from(key.scope_ceiling.len())
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    for scope in key.scope_ceiling.iter().copied() {
        encode_scope(output, scope);
    }
    encode_optional_key_id(output, key.predecessor_id);
    encode_optional_key_id(output, key.successor_id);
    encode_optional_i64(output, key.overlap_until_micros);
    encode_optional_u64(output, key.rotation_overlap_micros);
    Ok(())
}

fn decode_keys_v1(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<ApiKeyId, ApiKeyRecord>, AccessCatalogError> {
    let mut keys = BTreeMap::new();
    for _ in 0..count {
        let id =
            ApiKeyId::from_bytes(decoder.array()?).ok_or(AccessCatalogError::CorruptCatalog)?;
        let principal_id = decoder.security_id()?;
        let verifier = ApiKeyVerifier::from_digest(id, decoder.array()?);
        let active = decoder.boolean()?;
        let revoked = decoder.boolean()?;
        let created_at_micros = decoder.i64()?;
        let expires_at_micros = decode_optional_expiry(decoder)?;
        let published_epoch = AuthorizationEpoch::new(decoder.u64()?);
        let permission_ceiling = ProductAuthorization::from_known_bits(decoder.u64()?)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let roles = decode_roles(decoder)?;
        let record = ApiKeyRecord {
            id,
            principal_id,
            label: decoder.string()?.into_boxed_str(),
            verifier,
            active,
            roles,
            custom_roles: Box::new([]),
            permission_ceiling,
            scope_ceiling: vec![ProductScope::Instance].into_boxed_slice(),
            created_at_micros,
            expires_at_micros,
            revoked,
            published_epoch,
            predecessor_id: None,
            successor_id: None,
            overlap_until_micros: None,
            rotation_overlap_micros: None,
        };
        if keys.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(keys)
}

fn decode_keys_v2(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<ApiKeyId, ApiKeyRecord>, AccessCatalogError> {
    let mut keys = BTreeMap::new();
    for _ in 0..count {
        let id =
            ApiKeyId::from_bytes(decoder.array()?).ok_or(AccessCatalogError::CorruptCatalog)?;
        let principal_id = decoder.security_id()?;
        let verifier = ApiKeyVerifier::from_digest(id, decoder.array()?);
        let active = decoder.boolean()?;
        let revoked = decoder.boolean()?;
        let created_at_micros = decoder.i64()?;
        let expires_at_micros = decode_optional_expiry(decoder)?;
        let published_epoch = AuthorizationEpoch::new(decoder.u64()?);
        let permission_ceiling = ProductAuthorization::from_known_bits(decoder.u64()?)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let roles = decode_roles_allow_empty(decoder)?;
        let label = decoder.string()?.into_boxed_str();
        let custom_role_count = usize::from(decoder.u16()?);
        if custom_role_count > AccessControlLimits::V1.assignments_per_principal {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut custom_roles = Vec::with_capacity(custom_role_count);
        for _ in 0..custom_role_count {
            custom_roles.push(decoder.security_id()?);
        }
        if !strictly_sorted(&custom_roles) {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let scope_count = usize::from(decoder.u16()?);
        if scope_count == 0 || scope_count > AccessControlLimits::V1.assignments_per_principal {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let mut scope_ceiling = Vec::with_capacity(scope_count);
        for _ in 0..scope_count {
            scope_ceiling.push(decoder.scope()?);
        }
        if !strictly_sorted(&scope_ceiling) {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let record = ApiKeyRecord {
            id,
            principal_id,
            label,
            verifier,
            active,
            roles,
            custom_roles: custom_roles.into_boxed_slice(),
            permission_ceiling,
            scope_ceiling: scope_ceiling.into_boxed_slice(),
            created_at_micros,
            expires_at_micros,
            revoked,
            published_epoch,
            predecessor_id: decode_optional_key_id(decoder)?,
            successor_id: decode_optional_key_id(decoder)?,
            overlap_until_micros: decode_optional_expiry(decoder)?,
            rotation_overlap_micros: decode_optional_u64(decoder)?,
        };
        if keys.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(keys)
}

fn decode_audit_index(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<Vec<SecurityAuditIndexEntry>, AccessCatalogError> {
    let mut index = Vec::with_capacity(count);
    let mut ids = BTreeSet::new();
    let mut previous_csn = 0;
    for _ in 0..count {
        let id = decoder.security_id()?;
        let commit_csn = decoder.u64()?;
        if commit_csn == 0 || commit_csn <= previous_csn || !ids.insert(id) {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        index.push(SecurityAuditIndexEntry { id, commit_csn });
        previous_csn = commit_csn;
    }
    Ok(index)
}

fn encode_audit_event(event: &SecurityAuditEvent) -> Result<Vec<u8>, AccessCatalogError> {
    let mut output = Vec::new();
    output.extend_from_slice(AUDIT_EVENT_MAGIC);
    output.extend_from_slice(&event.id.to_be_bytes());
    output.extend_from_slice(&event.commit_csn.to_be_bytes());
    encode_optional_security_id(&mut output, event.actor_principal_id);
    encode_optional_key_id(&mut output, event.actor_key_id);
    output.push(event.action.tag());
    output.push(match event.result {
        SecurityAuditResult::Succeeded => 0,
    });
    output.extend_from_slice(
        &u16::try_from(event.targets.len())
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    for target in event.targets.iter().copied() {
        match target {
            SecurityAuditTarget::Principal(id) => {
                output.push(0);
                output.extend_from_slice(&id.to_be_bytes());
            }
            SecurityAuditTarget::Role(id) => {
                output.push(1);
                output.extend_from_slice(&id.to_be_bytes());
            }
            SecurityAuditTarget::Assignment(id) => {
                output.push(2);
                output.extend_from_slice(&id.to_be_bytes());
            }
            SecurityAuditTarget::Key(id) => {
                output.push(3);
                output.extend_from_slice(id.as_bytes());
            }
            SecurityAuditTarget::LegacyBearer => {
                output.push(4);
                output.extend_from_slice(&[0; 16]);
            }
        }
    }
    output.extend_from_slice(
        &u16::try_from(event.metadata.len())
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    for metadata in event.metadata.iter().copied() {
        match metadata {
            SecurityAuditMetadata::ExpiresAtMicros(value) => {
                output.push(0);
                output.extend_from_slice(&value.to_be_bytes());
            }
            SecurityAuditMetadata::RotationOverlapUntilMicros(value) => {
                output.push(1);
                output.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
    if output.len() > AccessControlLimits::V1.audit_event_bytes - CATALOG_DIGEST_BYTES {
        return Err(AccessCatalogError::LimitExceeded);
    }
    let digest = blake3::hash(&output);
    output.extend_from_slice(digest.as_bytes());
    Ok(output)
}

fn decode_audit_event(encoded: &[u8]) -> Result<SecurityAuditEvent, AccessCatalogError> {
    if encoded.len() < AUDIT_EVENT_MAGIC.len() + 16 + 8 + 1 + 1 + 1 + 2 + 2 + CATALOG_DIGEST_BYTES
        || encoded.len() > AccessControlLimits::V1.audit_event_bytes
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let body_length = encoded.len() - CATALOG_DIGEST_BYTES;
    let (body, expected_digest) = encoded.split_at(body_length);
    if !bool::from(blake3::hash(body).as_bytes().ct_eq(expected_digest)) {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut decoder = Decoder::new(body);
    if decoder.take(AUDIT_EVENT_MAGIC.len())? != AUDIT_EVENT_MAGIC {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let id = decoder.security_id()?;
    let commit_csn = decoder.u64()?;
    let actor_principal_id = decode_optional_security_id(&mut decoder)?;
    let actor_key_id = decode_optional_key_id(&mut decoder)?;
    let action =
        SecurityAuditAction::from_tag(decoder.byte()?).ok_or(AccessCatalogError::CorruptCatalog)?;
    let result = match decoder.byte()? {
        0 => SecurityAuditResult::Succeeded,
        _ => return Err(AccessCatalogError::CorruptCatalog),
    };
    let target_count = usize::from(decoder.u16()?);
    let mut targets = Vec::with_capacity(target_count);
    for _ in 0..target_count {
        let tag = decoder.byte()?;
        let bytes = decoder.array::<16>()?;
        targets.push(match tag {
            0 => SecurityAuditTarget::Principal(
                SecurityId::new(u128::from_be_bytes(bytes))
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            1 => SecurityAuditTarget::Role(
                SecurityId::new(u128::from_be_bytes(bytes))
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            2 => SecurityAuditTarget::Assignment(
                SecurityId::new(u128::from_be_bytes(bytes))
                    .ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            3 => SecurityAuditTarget::Key(
                ApiKeyId::from_bytes(bytes).ok_or(AccessCatalogError::CorruptCatalog)?,
            ),
            4 if bytes == [0; 16] => SecurityAuditTarget::LegacyBearer,
            _ => return Err(AccessCatalogError::CorruptCatalog),
        });
    }
    let metadata_count = usize::from(decoder.u16()?);
    let mut metadata = Vec::with_capacity(metadata_count);
    for _ in 0..metadata_count {
        metadata.push(match decoder.byte()? {
            0 => SecurityAuditMetadata::ExpiresAtMicros(decoder.i64()?),
            1 => SecurityAuditMetadata::RotationOverlapUntilMicros(decoder.i64()?),
            _ => return Err(AccessCatalogError::CorruptCatalog),
        });
    }
    let event = SecurityAuditEvent {
        id,
        commit_csn,
        actor_principal_id,
        actor_key_id,
        action,
        result,
        targets: targets.into_boxed_slice(),
        metadata: metadata.into_boxed_slice(),
    };
    if !decoder.is_empty()
        || event.commit_csn == 0
        || !strictly_sorted(&event.targets)
        || !strictly_sorted(&event.metadata)
        || encode_audit_event(&event)?.as_slice() != encoded
    {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(event)
}

fn decode_optional_expiry(decoder: &mut Decoder<'_>) -> Result<Option<i64>, AccessCatalogError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => decoder.i64().map(Some),
        _ => Err(AccessCatalogError::CorruptCatalog),
    }
}

fn encode_optional_i64(output: &mut Vec<u8>, value: Option<i64>) {
    match value {
        Some(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_be_bytes());
        }
        None => output.push(0),
    }
}

fn encode_optional_u64(output: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_be_bytes());
        }
        None => output.push(0),
    }
}

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, AccessCatalogError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => decoder.u64().map(Some),
        _ => Err(AccessCatalogError::CorruptCatalog),
    }
}

fn encode_optional_security_id(output: &mut Vec<u8>, value: Option<SecurityId>) {
    match value {
        Some(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_be_bytes());
        }
        None => output.push(0),
    }
}

fn decode_optional_security_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<SecurityId>, AccessCatalogError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => decoder.security_id().map(Some),
        _ => Err(AccessCatalogError::CorruptCatalog),
    }
}

fn encode_optional_key_id(output: &mut Vec<u8>, value: Option<ApiKeyId>) {
    match value {
        Some(value) => {
            output.push(1);
            output.extend_from_slice(value.as_bytes());
        }
        None => output.push(0),
    }
}

fn decode_optional_key_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<ApiKeyId>, AccessCatalogError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => ApiKeyId::from_bytes(decoder.array()?)
            .map(Some)
            .ok_or(AccessCatalogError::CorruptCatalog),
        _ => Err(AccessCatalogError::CorruptCatalog),
    }
}

fn decode_roles(decoder: &mut Decoder<'_>) -> Result<Box<[BuiltInRole]>, AccessCatalogError> {
    let count = usize::from(decoder.byte()?);
    if count == 0 || count > 7 {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut roles = Vec::with_capacity(count);
    for _ in 0..count {
        roles.push(
            BuiltInRole::from_tag(decoder.byte()?).ok_or(AccessCatalogError::CorruptCatalog)?,
        );
    }
    if !strictly_sorted(&roles) {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(roles.into_boxed_slice())
}

fn decode_roles_allow_empty(
    decoder: &mut Decoder<'_>,
) -> Result<Box<[BuiltInRole]>, AccessCatalogError> {
    let count = usize::from(decoder.byte()?);
    if count > 7 {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    let mut roles = Vec::with_capacity(count);
    for _ in 0..count {
        roles.push(
            BuiltInRole::from_tag(decoder.byte()?).ok_or(AccessCatalogError::CorruptCatalog)?,
        );
    }
    if !roles.is_empty() && !strictly_sorted(&roles) {
        return Err(AccessCatalogError::CorruptCatalog);
    }
    Ok(roles.into_boxed_slice())
}

fn audit_event_storage_key(entry: SecurityAuditIndexEntry) -> Vec<u8> {
    let mut key = Vec::with_capacity(AUDIT_EVENT_STORAGE_PREFIX.len() + 8 + 16);
    key.extend_from_slice(AUDIT_EVENT_STORAGE_PREFIX);
    key.extend_from_slice(&entry.commit_csn.to_be_bytes());
    key.extend_from_slice(&entry.id.to_be_bytes());
    key
}

/// Stable catalog construction, authentication, or codec error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessCatalogError {
    /// Access control already has durable principals.
    AlreadyBootstrapped,
    /// Security display text is empty, oversized, or contains control bytes.
    InvalidDisplayName,
    /// A security mutation request is structurally invalid.
    InvalidRequest,
    /// The operating-system CSPRNG failed.
    Entropy,
    /// A bounded v1 limit was exceeded.
    LimitExceeded,
    /// The target record is absent.
    NotFound,
    /// An audit cursor is older than or outside the retained event window.
    CursorExpired,
    /// Current durable state conflicts with the mutation.
    Conflict,
    /// Authentication failed without disclosing why.
    Unauthorized,
    /// Durable catalog bytes are corrupt or noncanonical.
    CorruptCatalog,
}

impl std::fmt::Display for AccessCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyBootstrapped => {
                formatter.write_str("access control is already bootstrapped")
            }
            Self::InvalidDisplayName => formatter.write_str("invalid security display name"),
            Self::InvalidRequest => formatter.write_str("invalid access-control request"),
            Self::Entropy => formatter.write_str("security entropy is unavailable"),
            Self::LimitExceeded => formatter.write_str("access-control limit exceeded"),
            Self::NotFound => formatter.write_str("security record not found"),
            Self::CursorExpired => formatter.write_str("security audit cursor is not retained"),
            Self::Conflict => formatter.write_str("security state conflicts with the request"),
            Self::Unauthorized => formatter.write_str("unauthorized"),
            Self::CorruptCatalog => formatter.write_str("access-control catalog is corrupt"),
        }
    }
}

impl std::error::Error for AccessCatalogError {}

fn validate_security_list_limit(limit: usize) -> Result<(), AccessCatalogError> {
    if limit == 0 || limit > MAX_SECURITY_LIST_ROWS {
        return Err(AccessCatalogError::InvalidRequest);
    }
    Ok(())
}

fn validate_security_cursor(
    cursor: Option<SecurityCursor>,
    current_epoch: AuthorizationEpoch,
) -> Result<Option<SecurityCursorId>, AccessCatalogError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.authorization_epoch != current_epoch {
        return Err(AccessCatalogError::Conflict);
    }
    Ok(Some(cursor.after_id))
}

fn validate_display_name(value: &str) -> Result<(), AccessCatalogError> {
    if value.is_empty()
        || value.len() > AccessControlLimits::V1.display_name_bytes
        || value.chars().any(char::is_control)
    {
        Err(AccessCatalogError::InvalidDisplayName)
    } else {
        Ok(())
    }
}

fn push_count(output: &mut Vec<u8>, count: usize) -> Result<(), AccessCatalogError> {
    output.extend_from_slice(
        &u32::try_from(count)
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    Ok(())
}

fn push_string(output: &mut Vec<u8>, value: &str) -> Result<(), AccessCatalogError> {
    validate_display_name(value)?;
    output.extend_from_slice(
        &u16::try_from(value.len())
            .map_err(|_| AccessCatalogError::LimitExceeded)?
            .to_be_bytes(),
    );
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_scope(output: &mut Vec<u8>, scope: ProductScope) {
    match scope {
        ProductScope::Instance => output.push(0),
        ProductScope::CatalogSubtree(object) => {
            output.push(1);
            output.extend_from_slice(&object.get().to_be_bytes());
        }
        ProductScope::CatalogObject(object) => {
            output.push(2);
            output.extend_from_slice(&object.get().to_be_bytes());
        }
    }
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    const fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], AccessCatalogError> {
        if self.remaining.len() < count {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let (value, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], AccessCatalogError> {
        self.take(N)?
            .try_into()
            .map_err(|_| AccessCatalogError::CorruptCatalog)
    }

    fn byte(&mut self) -> Result<u8, AccessCatalogError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(AccessCatalogError::CorruptCatalog)
    }

    fn boolean(&mut self) -> Result<bool, AccessCatalogError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(AccessCatalogError::CorruptCatalog),
        }
    }

    fn u16(&mut self) -> Result<u16, AccessCatalogError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, AccessCatalogError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, AccessCatalogError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, AccessCatalogError> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    fn count(&mut self, maximum: usize) -> Result<usize, AccessCatalogError> {
        let count = usize::try_from(self.u32()?).map_err(|_| AccessCatalogError::CorruptCatalog)?;
        (count <= maximum)
            .then_some(count)
            .ok_or(AccessCatalogError::CorruptCatalog)
    }

    fn string(&mut self) -> Result<String, AccessCatalogError> {
        let length = usize::from(self.u16()?);
        let value = std::str::from_utf8(self.take(length)?)
            .map_err(|_| AccessCatalogError::CorruptCatalog)?;
        validate_display_name(value)?;
        Ok(value.to_owned())
    }

    fn security_id(&mut self) -> Result<SecurityId, AccessCatalogError> {
        SecurityId::new(u128::from_be_bytes(self.array()?))
            .ok_or(AccessCatalogError::CorruptCatalog)
    }

    fn scope(&mut self) -> Result<ProductScope, AccessCatalogError> {
        match self.byte()? {
            0 => Ok(ProductScope::Instance),
            1 => ObjectId::new(u128::from_be_bytes(self.array()?))
                .map(ProductScope::CatalogSubtree)
                .map_err(|_| AccessCatalogError::CorruptCatalog),
            2 => ObjectId::new(u128::from_be_bytes(self.array()?))
                .map(ProductScope::CatalogObject)
                .map_err(|_| AccessCatalogError::CorruptCatalog),
            _ => Err(AccessCatalogError::CorruptCatalog),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt::Write as _,
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hyphae-access-catalog-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_test_files(paths: &[&Path]) {
        for path in paths {
            let _ignored = fs::remove_file(path);
        }
    }

    #[test]
    fn legacy_bearer_migration_is_durable_terminal_and_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let key_path = path.with_extension("legacy-owner-key");
        remove_test_files(&[&key_path]);
        let _ignored = fs::remove_dir_all(&path);
        let bearer = b"legacy-bearer-canary-0123456789abcdef";
        let mut product = NativeProduct::create(&path)?;
        drop(product);

        product = NativeProduct::open_offline_owner(&path)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical-owner",
            bearer,
            1,
        )?;
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::MigrationPending
        );
        let secret = started.secret.expose_secret().to_owned();
        fs::write(&key_path, &secret)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        }
        let activated = product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &secret,
            started.authorization_epoch,
            "Migrated owner",
            "canonical-owner",
            bearer,
            2,
        )?;
        let replay = product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &secret,
            started.authorization_epoch,
            "Migrated owner",
            "canonical-owner",
            bearer,
            3,
        )?;
        assert_eq!(activated, replay);
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::DualWindow
        );
        drop(product);

        let mut product = NativeProduct::open(&path)?;
        let owner = product.authenticate_api_key(&secret, 0)?;
        let revoked = product.revoke_legacy_bearer_idempotent(&owner, 77, 4)?;
        let replay = product.revoke_legacy_bearer_idempotent(&owner, 77, 5)?;
        assert_eq!(revoked, replay);
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        drop(product);
        let reopened = NativeProduct::open(&path)?;
        assert_eq!(
            reopened.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        drop(reopened);
        remove_test_files(&[&key_path]);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn owner_recovery_activation_terminally_revokes_legacy_in_the_same_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let legacy = b"legacy-recovery-canary-0123456789abcdef";
        let _ignored = fs::remove_dir_all(&path);
        let mut product = NativeProduct::create(&path)?;
        drop(product);
        product = NativeProduct::open_offline_owner(&path)?;
        let migrated = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy,
            1,
        )?;
        let canonical = migrated.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            migrated.key_id,
            &canonical,
            migrated.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy,
            2,
        )?;
        let recovery = product.start_owner_recovery_offline("recovered", 3)?;
        let recovered = recovery.secret.expose_secret().to_owned();
        let receipt = product.resume_owner_recovery_offline(
            recovery.key_id,
            &recovered,
            recovery.authorization_epoch,
            4,
        )?;
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        assert_eq!(
            receipt.authorization_epoch,
            product.access_control_status()?.epoch
        );
        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn owner_recovery_supersedes_pending_legacy_migration_without_invalid_catalog()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let legacy = b"legacy-pending-recovery-canary-0123456789abcdef";
        let _ignored = fs::remove_dir_all(&path);
        drop(NativeProduct::create(&path)?);
        let mut product = NativeProduct::open_offline_owner(&path)?;
        let migrated = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy,
            1,
        )?;
        let retired_migration_secret = migrated.secret.expose_secret().to_owned();
        let recovery = product.start_owner_recovery_offline("recovered", 2)?;
        let recovered_secret = recovery.secret.expose_secret().to_owned();
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        drop(product);

        let mut reopened = NativeProduct::open_offline_owner(&path)?;
        assert_eq!(
            reopened.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        let receipt = reopened.resume_owner_recovery_offline(
            recovery.key_id,
            &recovered_secret,
            recovery.authorization_epoch,
            3,
        )?;
        assert_eq!(receipt.key_id, recovery.key_id);
        assert!(
            reopened
                .authenticate_api_key(&retired_migration_secret, 0)
                .is_err()
        );
        assert!(reopened.authenticate_api_key(&recovered_secret, 0).is_ok());
        drop(reopened);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn owner_recovery_with_legacy_recovers_terminally_from_wal_appended()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let legacy = b"legacy-recovery-wal-canary-0123456789abcdef";
        let _ignored = fs::remove_dir_all(&path);
        drop(NativeProduct::create(&path)?);
        let mut product = NativeProduct::open_offline_owner(&path)?;
        let migrated = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy,
            1,
        )?;
        let canonical = migrated.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            migrated.key_id,
            &canonical,
            migrated.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy,
            2,
        )?;
        let recovery = product.start_owner_recovery_offline("recovered", 3)?;
        let recovered = recovery.secret.expose_secret().to_owned();
        product.interrupt_next_security_commit_for_test(
            hyphae_native_runtime::CommitBoundary::WalAppended,
        );
        assert!(
            product
                .resume_owner_recovery_offline(
                    recovery.key_id,
                    &recovered,
                    recovery.authorization_epoch,
                    4,
                )
                .is_err()
        );
        drop(product);

        let mut reopened = NativeProduct::open_offline_owner(&path)?;
        assert_eq!(
            reopened.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::Revoked
        );
        assert!(reopened.legacy_bearer_verifier()?.is_none());
        let receipt = reopened.resume_owner_recovery_offline(
            recovery.key_id,
            &recovered,
            recovery.authorization_epoch,
            5,
        )?;
        assert_eq!(
            receipt.authorization_epoch,
            reopened.access_control_status()?.epoch
        );
        drop(reopened);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn pending_migration_can_be_restarted_after_secret_file_loss()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let _ignored = fs::remove_dir_all(&path);
        let legacy = b"legacy-restart-canary-0123456789abcdef";
        let mut product = NativeProduct::create(&path)?;
        drop(product);
        product = NativeProduct::open_offline_owner(&path)?;
        let first = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy,
            1,
        )?;
        let first_id = first.key_id;
        drop(first);
        drop(product);

        let mut product = NativeProduct::open_offline_owner(&path)?;
        let replacement = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy,
            2,
        )?;
        assert_ne!(replacement.key_id, first_id);
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.key_id,
            Some(replacement.key_id)
        );
        let secret = replacement.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            replacement.key_id,
            &secret,
            replacement.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy,
            3,
        )?;
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            LegacyBearerState::DualWindow
        );
        drop(product);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn hyacat05_persists_only_product_keyed_legacy_verifier_and_v4_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let legacy = b"legacy-durable-verifier-canary-0123456789abcdef";
        let product_key = [0x5a; 32];
        let bare_digest = *blake3::hash(legacy).as_bytes();
        let keyed_verifier = legacy_bearer_verifier(legacy, product_key)?;
        assert_ne!(keyed_verifier, bare_digest);

        let mut catalog = AccessControlCatalog::empty();
        let request_digest = legacy_bearer_migration_request_digest("Owner", "owner", legacy)?;
        catalog.begin_legacy_bearer_migration(
            "Owner",
            "owner",
            1,
            request_digest,
            keyed_verifier,
        )?;
        let encoded = catalog.encode()?;
        assert_eq!(&encoded[..CATALOG_MAGIC.len()], CATALOG_MAGIC);
        assert!(!encoded.windows(legacy.len()).any(|window| window == legacy));
        assert_eq!(AccessControlCatalog::decode(&encoded)?, catalog);
        assert!(catalog.encode_v4().is_err());
        assert!(LegacyBearerVerifier(keyed_verifier).verifies(legacy, product_key));
        assert!(!LegacyBearerVerifier(keyed_verifier).verifies_digest(bare_digest, [0; 32]));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn restricted_output_has_current_account_and_system_dacl()
    -> Result<(), Box<dyn std::error::Error>> {
        use windows_permissions::{
            constants::{SeObjectType, SecurityInformation},
            utilities, wrappers,
        };

        let path = temporary_directory().with_extension("key");
        remove_test_files(&[&path]);
        let file = create_restricted_output(&path)?;
        validate_windows_restricted_file(&file)?;
        let descriptor = wrappers::GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Owner | SecurityInformation::Dacl,
        )?;
        let sddl = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
            &descriptor,
            SecurityInformation::Owner | SecurityInformation::Dacl,
        )?;
        let current_user = utilities::current_process_sid()?.to_string();
        let sddl = sddl.to_string_lossy();
        assert!(sddl.contains("D:P"));
        assert!(windows_sddl_has_protected_dacl(&sddl));
        if current_user != "S-1-5-18" {
            assert!(sddl.contains("(A;;FA;;;SY)") || sddl.contains("(A;;FA;;;S-1-5-18)"));
        }
        assert!(!sddl.contains(";;;WD)"));
        assert!(!sddl.contains(";;;AU)"));
        drop(file);
        remove_test_files(&[&path]);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn restricted_output_rejects_an_extra_trustee() -> Result<(), Box<dyn std::error::Error>> {
        use windows_permissions::{
            LocalBox, SecurityDescriptor,
            constants::{SeObjectType, SecurityInformation},
            utilities, wrappers,
        };

        let path = temporary_directory().with_extension("key");
        remove_test_files(&[&path]);
        let file = create_restricted_output(&path)?;
        let current_user = utilities::current_process_sid()?.to_string();
        let descriptor: LocalBox<SecurityDescriptor> =
            format!("O:{current_user}D:P(A;;FA;;;{current_user})(A;;FA;;;SY)(A;;FR;;;WD)")
                .parse()?;
        wrappers::SetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            descriptor.dacl(),
            None,
        )?;
        assert!(validate_windows_restricted_file(&file).is_err());
        drop(file);
        remove_test_files(&[&path]);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn restricted_output_rejects_named_streams() -> Result<(), Box<dyn std::error::Error>> {
        let base = temporary_directory().with_extension("carrier");
        let stream = std::path::PathBuf::from(format!("{}:owner.key", base.display()));
        remove_test_files(&[&base, &stream]);
        fs::write(&base, b"carrier")?;
        let error = create_restricted_output(&stream)
            .err()
            .ok_or("named-stream credential output was accepted")?;
        assert_eq!(error.code(), ProductErrorCode::InvalidRequest);
        assert!(!stream.exists());
        remove_test_files(&[&base, &stream]);
        Ok(())
    }

    #[test]
    fn windows_dacl_parser_requires_protected_dacl() {
        #[cfg(windows)]
        {
            assert!(windows_sddl_has_protected_dacl("O:SYD:P(A;;FA;;;SY)"));
            assert!(windows_sddl_has_protected_dacl(
                "O:LSD:PAI(A;;FA;;;LS)(A;;FA;;;SY)"
            ));
            assert!(!windows_sddl_has_protected_dacl(
                "O:NSD:AI(A;;FA;;;NS)(A;;FA;;;SY)"
            ));
        }
    }

    fn scoped_authority(
        permission: ProductPermission,
        grant_scope: ProductScope,
        ceiling_scope: ProductScope,
    ) -> Result<AuthenticatedAuthority, Box<dyn std::error::Error>> {
        Ok(AuthenticatedAuthority {
            principal_id: SecurityId::new(1).ok_or("invalid principal")?,
            key_id: ApiKeyId::from_bytes([1; 16]).ok_or("invalid key")?,
            principal: ProductPrincipal::new("scope-test").ok_or("invalid product principal")?,
            authorization: ProductAuthorization::from_permissions([permission]),
            authorization_epoch: AuthorizationEpoch::INITIAL,
            directory_lineage: [1; 24],
            terminal_replay_only: false,
            valid_until_micros: None,
            effective_roles: Box::new([]),
            effective_custom_roles: Box::new([]),
            scope_ceiling: vec![ceiling_scope].into_boxed_slice(),
            scoped_authorization: vec![ScopedAuthorization {
                scope: grant_scope,
                authorization: ProductAuthorization::from_permissions([permission]),
            }]
            .into_boxed_slice(),
            catalog_cursor_key: [9; 32],
        })
    }

    fn valid_key_summary_input() -> Result<SecurityKeySummaryInput, Box<dyn std::error::Error>> {
        Ok(SecurityKeySummaryInput {
            id: ApiKeyId::from_bytes([1; 16]).ok_or("invalid key")?,
            principal_id: SecurityId::new(1).ok_or("invalid principal")?,
            label: "reader-key".to_owned(),
            active: true,
            roles: vec![BuiltInRole::Reader],
            custom_roles: Vec::new(),
            permission_ceiling: BuiltInRole::Reader.authorization(),
            scope_ceiling: vec![ProductScope::Instance],
            created_at_micros: 1,
            expires_at_micros: Some(2),
            revoked: false,
            published_epoch: AuthorizationEpoch::INITIAL,
            predecessor_id: None,
            successor_id: None,
            overlap_until_micros: None,
            rotation_overlap_micros: None,
        })
    }

    fn principal_summary(id: u128) -> Result<SecurityPrincipalSummary, Box<dyn std::error::Error>> {
        Ok(SecurityPrincipalSummary::new(
            SecurityId::new(id).ok_or("invalid principal")?,
            format!("principal-{id}"),
            true,
        )?)
    }

    fn audit_event(
        id: u128,
        commit_csn: u64,
    ) -> Result<SecurityAuditEvent, Box<dyn std::error::Error>> {
        let id = SecurityId::new(id).ok_or("invalid event")?;
        Ok(SecurityAuditEvent::try_from_wire(
            id,
            commit_csn,
            None,
            None,
            SecurityAuditAction::RecoverOwner,
            SecurityAuditResult::Succeeded,
            vec![SecurityAuditTarget::Principal(id)],
            Vec::new(),
        )?)
    }

    #[test]
    fn wire_summary_constructors_reject_noncanonical_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let principal_id = SecurityId::new(1).ok_or("invalid principal")?;
        let assignment_id = SecurityId::new(2).ok_or("invalid assignment")?;
        let role_id = SecurityId::new(3).ok_or("invalid role")?;
        assert!(SecurityPrincipalSummary::new(principal_id, "Reader", true).is_ok());
        assert_eq!(
            SecurityPrincipalSummary::new(principal_id, "", true),
            Err(AccessCatalogError::InvalidDisplayName)
        );

        assert!(
            SecurityAssignmentSummary::new(
                assignment_id,
                principal_id,
                Some(BuiltInRole::Reader),
                None,
                Some(ProductScope::Instance),
            )
            .is_ok()
        );
        assert!(
            SecurityAssignmentSummary::new(assignment_id, principal_id, None, Some(role_id), None,)
                .is_ok()
        );
        for invalid in [
            SecurityAssignmentSummary::new(assignment_id, principal_id, None, None, None),
            SecurityAssignmentSummary::new(
                assignment_id,
                principal_id,
                Some(BuiltInRole::Reader),
                Some(role_id),
                Some(ProductScope::Instance),
            ),
            SecurityAssignmentSummary::new(
                assignment_id,
                principal_id,
                Some(BuiltInRole::Reader),
                None,
                None,
            ),
            SecurityAssignmentSummary::new(
                assignment_id,
                principal_id,
                None,
                Some(role_id),
                Some(ProductScope::Instance),
            ),
        ] {
            assert_eq!(invalid, Err(AccessCatalogError::InvalidRequest));
        }

        let catalog_read =
            CustomRoleGrant::new(ProductPermission::CatalogRead, ProductScope::Instance)
                .ok_or("invalid catalog grant")?;
        let data_read = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
            .ok_or("invalid data grant")?;
        assert!(
            SecurityRoleSummary::custom(role_id, "tenant reader", vec![catalog_read, data_read],)
                .is_ok()
        );
        assert_eq!(
            SecurityRoleSummary::custom(role_id, "reader", vec![catalog_read]),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityRoleSummary::custom(role_id, "tenant reader", vec![data_read, catalog_read],),
            Err(AccessCatalogError::InvalidRequest)
        );
        Ok(())
    }

    #[test]
    fn wire_key_and_audit_constructors_round_trip_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let input = valid_key_summary_input()?;
        let summary = SecurityKeySummary::try_from_wire(input.clone())?;
        assert_eq!(summary.id(), input.id);
        assert_eq!(summary.principal_id(), input.principal_id);
        assert_eq!(summary.label(), input.label);
        assert_eq!(summary.roles(), input.roles);
        assert_eq!(summary.scope_ceiling(), input.scope_ceiling);
        assert_eq!(summary.expires_at_micros(), input.expires_at_micros);

        let mut invalid = input.clone();
        invalid.roles.clear();
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );
        let mut invalid = input.clone();
        invalid.roles = vec![BuiltInRole::Reader, BuiltInRole::Admin];
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );
        let mut invalid = input.clone();
        invalid.scope_ceiling.clear();
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );
        let mut invalid = input.clone();
        invalid.expires_at_micros = Some(input.created_at_micros);
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );
        let mut invalid = input.clone();
        invalid.predecessor_id = Some(input.id);
        invalid.rotation_overlap_micros = Some(1);
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );
        let mut invalid = input.clone();
        invalid.custom_roles = (1_u128..=128)
            .map(|value| SecurityId::new(value).ok_or("invalid custom role"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            SecurityKeySummary::try_from_wire(invalid),
            Err(AccessCatalogError::InvalidRequest)
        );

        let event_id = SecurityId::new(4).ok_or("invalid event")?;
        let key_id = ApiKeyId::from_bytes([4; 16]).ok_or("invalid key")?;
        let targets = vec![
            SecurityAuditTarget::Principal(input.principal_id),
            SecurityAuditTarget::Key(key_id),
        ];
        let event = SecurityAuditEvent::try_from_wire(
            event_id,
            7,
            Some(input.principal_id),
            Some(key_id),
            SecurityAuditAction::IssueKey,
            SecurityAuditResult::Succeeded,
            targets.clone(),
            vec![SecurityAuditMetadata::ExpiresAtMicros(2)],
        )?;
        assert_eq!(event.id(), event_id);
        assert_eq!(event.targets(), targets);
        assert_eq!(
            SecurityAuditAction::from_tag(event.action().tag()),
            Some(event.action())
        );
        assert_write_audit_action_tags();
        assert_eq!(
            SecurityAuditEvent::try_from_wire(
                event_id,
                7,
                Some(input.principal_id),
                None,
                SecurityAuditAction::IssueKey,
                SecurityAuditResult::Succeeded,
                targets.clone(),
                Vec::new(),
            ),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditEvent::try_from_wire(
                event_id,
                0,
                None,
                None,
                SecurityAuditAction::RecoverOwner,
                SecurityAuditResult::Succeeded,
                targets.into_iter().rev().collect(),
                Vec::new(),
            ),
            Err(AccessCatalogError::InvalidRequest)
        );
        Ok(())
    }

    fn assert_write_audit_action_tags() {
        assert_eq!(
            SecurityAuditAction::from_tag(13),
            Some(SecurityAuditAction::SetPrincipalEnabled)
        );
        assert_eq!(
            SecurityAuditAction::from_tag(14),
            Some(SecurityAuditAction::RevokeAssignment)
        );
        assert_eq!(
            SecurityAuditAction::from_tag(15),
            Some(SecurityAuditAction::RevokeLegacyBearer)
        );
        assert_eq!(SecurityAuditAction::from_tag(16), None);
    }

    #[test]
    fn metadata_page_constructors_reject_order_cursor_and_epoch_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let epoch = AuthorizationEpoch::INITIAL;
        let first = principal_summary(1)?;
        let second = principal_summary(2)?;
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(epoch, vec![second.clone(), first.clone()], None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(epoch, vec![first.clone(), first.clone()], None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(
                epoch,
                vec![first.clone()],
                Some(SecurityCursor::new(
                    epoch,
                    SecurityCursorId::Principal(second.id()),
                )),
            ),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(
                epoch,
                Vec::new(),
                Some(SecurityCursor::new(
                    epoch,
                    SecurityCursorId::Principal(first.id()),
                )),
            ),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(AuthorizationEpoch::UNMANAGED, vec![first], None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        Ok(())
    }

    #[test]
    fn every_metadata_page_family_enforces_its_canonical_item_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let epoch = AuthorizationEpoch::INITIAL;
        let role_id = SecurityId::new(3).ok_or("invalid role")?;
        let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
            .ok_or("invalid grant")?;
        let custom = SecurityRoleSummary::custom(role_id, "custom-reader", vec![grant])?;
        assert_eq!(
            SecurityRolePage::try_from_wire(
                epoch,
                vec![custom, SecurityRoleSummary::built_in(BuiltInRole::Reader)],
                None,
            ),
            Err(AccessCatalogError::InvalidRequest)
        );

        let assignment_id = SecurityId::new(4).ok_or("invalid assignment")?;
        let principal_id = SecurityId::new(5).ok_or("invalid principal")?;
        let assignment = SecurityAssignmentSummary::new(
            assignment_id,
            principal_id,
            Some(BuiltInRole::Reader),
            None,
            Some(ProductScope::Instance),
        )?;
        assert!(SecurityAssignmentPage::try_from_wire(epoch, vec![assignment], None).is_ok());

        let mut input = valid_key_summary_input()?;
        input.published_epoch = AuthorizationEpoch::new(epoch.get() + 1);
        let future_key = SecurityKeySummary::try_from_wire(input)?;
        assert_eq!(
            SecurityKeyPage::try_from_wire(epoch, vec![future_key], None),
            Err(AccessCatalogError::InvalidRequest)
        );
        Ok(())
    }

    #[test]
    fn audit_page_constructor_rejects_nonmonotonic_duplicate_and_wrong_cursor()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = audit_event(1, 10)?;
        let second = audit_event(2, 11)?;
        assert_eq!(
            SecurityAuditPage::try_from_wire(vec![second.clone(), first.clone()], None),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditPage::try_from_wire(vec![first.clone(), audit_event(1, 11)?], None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditPage::try_from_wire(vec![first.clone(), audit_event(3, 10)?], None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditPage::try_from_wire(vec![first.clone()], Some(second.id())),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditPage::try_from_wire(Vec::new(), Some(first.id())),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditPage::try_from_wire(vec![first.clone(), second.clone()], Some(first.id()),),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert!(
            SecurityAuditPage::try_from_wire(vec![first, second.clone()], Some(second.id()),)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn metadata_page_constructors_reject_one_past_the_result_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let items = (1..=MAX_SECURITY_LIST_ROWS + 1)
            .map(|id| principal_summary(id as u128))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            SecurityPrincipalPage::try_from_wire(AuthorizationEpoch::INITIAL, items, None,),
            Err(AccessCatalogError::InvalidRequest)
        );
        Ok(())
    }

    #[test]
    fn security_status_validates_combined_assignment_and_principal_bounds() {
        let valid = AccessControlStatus {
            bootstrapped: true,
            epoch: AuthorizationEpoch::INITIAL,
            principals: 1,
            assignments: 1,
            custom_roles: 1,
            custom_assignments: AccessControlLimits::V1.assignments_per_principal - 1,
            keys: AccessControlLimits::V1.keys_per_principal,
            pending_keys: 1,
            audit_events: 1,
        };
        assert_eq!(valid.validate(), Ok(()));
        assert_eq!(
            AccessControlStatus {
                custom_assignments: AccessControlLimits::V1.assignments_per_principal,
                ..valid
            }
            .validate(),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            AccessControlStatus {
                assignments: 0,
                custom_assignments: 0,
                ..valid
            }
            .validate(),
            Err(AccessCatalogError::InvalidRequest)
        );
    }

    #[test]
    fn security_response_size_bounds_are_exact_for_canonical_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let epoch = AuthorizationEpoch::INITIAL;
        let principal = principal_summary(1)?;
        let principal_item_bytes = 28 + principal.display_name().len();
        let principal_page = SecurityPrincipalPage::try_from_wire(epoch, vec![principal], None)?;
        assert_eq!(
            principal_page.encoded_size_bound(),
            72 + principal_item_bytes
        );

        let role_page = SecurityRolePage::try_from_wire(
            epoch,
            vec![SecurityRoleSummary::built_in(BuiltInRole::Reader)],
            None,
        )?;
        assert_eq!(role_page.encoded_size_bound(), 80);

        let role_id = SecurityId::new(3).ok_or("invalid role")?;
        let custom_role = SecurityRoleSummary::custom(
            role_id,
            "custom-reader",
            vec![
                CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
                    .ok_or("invalid grant")?,
            ],
        )?;
        let custom_role_bytes = 36 + custom_role.display_name().len() + 32;
        let custom_role_page = SecurityRolePage::try_from_wire(epoch, vec![custom_role], None)?;
        assert_eq!(
            custom_role_page.encoded_size_bound(),
            72 + custom_role_bytes
        );

        let assignment = SecurityAssignmentSummary::new(
            SecurityId::new(4).ok_or("invalid assignment")?,
            SecurityId::new(5).ok_or("invalid principal")?,
            Some(BuiltInRole::Reader),
            None,
            Some(ProductScope::Instance),
        )?;
        let assignment_page = SecurityAssignmentPage::try_from_wire(epoch, vec![assignment], None)?;
        assert_eq!(assignment_page.encoded_size_bound(), 72 + 64);

        let key = SecurityKeySummary::try_from_wire(valid_key_summary_input()?)?;
        let expected_key_bytes = 188
            + key.label().len()
            + key.roles().len()
            + 16 * key.custom_roles().len()
            + 24 * key.scope_ceiling().len();
        let key_page = SecurityKeyPage::try_from_wire(epoch, vec![key], None)?;
        assert_eq!(key_page.encoded_size_bound(), 72 + expected_key_bytes);

        let event = audit_event(2, 10)?;
        let event_bytes = 88 + 24 * event.targets().len() + 16 * event.metadata().len();
        let audit_page = SecurityAuditPage::try_from_wire(vec![event], None)?;
        assert_eq!(audit_page.encoded_size_bound(), 48 + event_bytes);
        assert_eq!(AccessControlStatus::encoded_size_bound(), 88);
        Ok(())
    }

    #[test]
    fn security_list_requests_reject_zero_and_one_past_the_limit() {
        assert_eq!(
            SecurityPrincipalListRequest::new(None, 0),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityRoleListRequest::new(None, MAX_SECURITY_LIST_ROWS + 1),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAssignmentListRequest::new(None, 0),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityKeyListRequest::new(None, MAX_SECURITY_LIST_ROWS + 1),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditReadRequest::new(None, 0),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert_eq!(
            SecurityAuditReadRequest::new(None, AccessControlLimits::V1.audit_result_rows + 1,),
            Err(AccessCatalogError::InvalidRequest)
        );
        assert!(SecurityPrincipalListRequest::new(None, 1).is_ok());
        assert!(SecurityRoleListRequest::new(None, MAX_SECURITY_LIST_ROWS).is_ok());
        assert!(SecurityAssignmentListRequest::new(None, 1).is_ok());
        assert!(SecurityKeyListRequest::new(None, MAX_SECURITY_LIST_ROWS).is_ok());
        assert!(
            SecurityAuditReadRequest::new(None, AccessControlLimits::V1.audit_result_rows).is_ok()
        );
    }

    #[test]
    fn security_principal_pages_are_ordered_and_epoch_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (owner_id, owner_key) = catalog.bootstrap_owner("Owner", "owner-key", 1)?;
        catalog.activate_key(owner_key.id())?;
        catalog.create_principal("Reader service")?;
        catalog.create_principal("Writer service")?;

        let principal_page =
            catalog.list_principals(SecurityPrincipalListRequest::new(None, 2)?)?;
        assert_eq!(principal_page.authorization_epoch(), catalog.epoch());
        assert_eq!(principal_page.items().len(), 2);
        let principal_cursor = principal_page
            .next_cursor()
            .ok_or("missing principal cursor")?;
        assert_eq!(principal_cursor.authorization_epoch(), catalog.epoch());
        let remaining_principals = catalog.list_principals(SecurityPrincipalListRequest::new(
            Some(principal_cursor),
            2,
        )?)?;
        let principal_ids: Vec<_> = principal_page
            .items()
            .iter()
            .chain(remaining_principals.items())
            .map(SecurityPrincipalSummary::id)
            .collect();
        assert_eq!(principal_ids.len(), 3);
        assert!(principal_ids.windows(2).all(|ids| ids[0] < ids[1]));
        assert!(principal_ids.contains(&owner_id));

        let stale_request = SecurityPrincipalListRequest::new(Some(principal_cursor), 1)?;
        assert_eq!(
            catalog.list_keys(SecurityKeyListRequest::new(Some(principal_cursor), 1)?),
            Err(AccessCatalogError::InvalidRequest)
        );
        catalog.create_principal("Epoch change")?;
        assert_eq!(
            catalog.list_principals(stale_request),
            Err(AccessCatalogError::Conflict)
        );
        Ok(())
    }

    #[test]
    fn security_cursor_tokens_round_trip_canonically_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let principal = SecurityId::new(1).ok_or("missing principal")?;
        let key = ApiKeyId::from_bytes([7; 16]).ok_or("missing key")?;
        let cursors = [
            SecurityCursor::new(
                AuthorizationEpoch::new(9),
                SecurityCursorId::Principal(principal),
            ),
            SecurityCursor::new(
                AuthorizationEpoch::new(9),
                SecurityCursorId::BuiltInRole(BuiltInRole::Auditor),
            ),
            SecurityCursor::new(
                AuthorizationEpoch::new(9),
                SecurityCursorId::CustomRole(principal),
            ),
            SecurityCursor::new(
                AuthorizationEpoch::new(9),
                SecurityCursorId::Assignment(principal),
            ),
            SecurityCursor::new(AuthorizationEpoch::new(9), SecurityCursorId::Key(key)),
        ];
        for cursor in cursors {
            let token = cursor.to_token();
            assert_eq!(SecurityCursor::from_token(&token)?, cursor);
        }
        for token in [
            "",
            "hysec2:9:principal:00000000000000000000000000000001",
            "hysec1:0:principal:00000000000000000000000000000001",
            "hysec1:09:principal:00000000000000000000000000000001",
            "hysec1:9:principal:00000000000000000000000000000000",
            "hysec1:9:built-in-role:unknown",
            "hysec1:9:key:00",
            "hysec1:9:principal:00000000000000000000000000000001:extra",
        ] {
            assert_eq!(
                SecurityCursor::from_token(token),
                Err(AccessCatalogError::InvalidRequest),
                "unexpected token acceptance: {token}",
            );
        }
        Ok(())
    }

    #[test]
    fn security_role_and_assignment_pages_are_globally_ordered()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_, owner_key) = catalog.bootstrap_owner("Owner", "owner-key", 1)?;
        catalog.activate_key(owner_key.id())?;
        let (reader_id, _) = catalog.create_principal("Reader service")?;
        let (writer_id, _) = catalog.create_principal("Writer service")?;
        let (custom_role_id, _) = catalog.create_custom_role(
            "tenant reader",
            [
                CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
                    .ok_or("invalid grant")?,
            ],
        )?;
        let (second_custom_role_id, _) = catalog.create_custom_role(
            "tenant writer",
            [
                CustomRoleGrant::new(ProductPermission::DataWrite, ProductScope::Instance)
                    .ok_or("invalid grant")?,
            ],
        )?;
        catalog.assign_built_in_role(reader_id, BuiltInRole::Reader, ProductScope::Instance)?;
        catalog.assign_custom_role(writer_id, custom_role_id)?;
        catalog.assign_custom_role(reader_id, second_custom_role_id)?;

        let role_page = catalog.list_roles(SecurityRoleListRequest::new(None, 8)?)?;
        assert_eq!(role_page.items().len(), 8);
        let built_ins: Vec<_> = role_page
            .items()
            .iter()
            .take(7)
            .map(SecurityRoleSummary::built_in_role)
            .collect();
        assert_eq!(
            built_ins,
            vec![
                Some(BuiltInRole::Owner),
                Some(BuiltInRole::Admin),
                Some(BuiltInRole::Operator),
                Some(BuiltInRole::Developer),
                Some(BuiltInRole::Writer),
                Some(BuiltInRole::Reader),
                Some(BuiltInRole::Auditor),
            ]
        );
        let role_cursor = role_page.next_cursor().ok_or("missing role cursor")?;
        let remaining_roles =
            catalog.list_roles(SecurityRoleListRequest::new(Some(role_cursor), 8)?)?;
        let custom_role_ids: Vec<_> = role_page
            .items()
            .iter()
            .chain(remaining_roles.items())
            .filter_map(SecurityRoleSummary::custom_role_id)
            .collect();
        let mut expected_custom_role_ids = vec![custom_role_id, second_custom_role_id];
        expected_custom_role_ids.sort_unstable();
        assert_eq!(custom_role_ids, expected_custom_role_ids);
        assert!(
            role_page
                .items()
                .iter()
                .chain(remaining_roles.items())
                .filter(|role| role.custom_role_id().is_some())
                .all(|role| role.grants().len() == 1)
        );

        let assignment_page =
            catalog.list_assignments(SecurityAssignmentListRequest::new(None, 2)?)?;
        let assignment_cursor = assignment_page
            .next_cursor()
            .ok_or("missing assignment cursor")?;
        let remaining_assignments = catalog.list_assignments(
            SecurityAssignmentListRequest::new(Some(assignment_cursor), 2)?,
        )?;
        let assignments: Vec<_> = assignment_page
            .items()
            .iter()
            .chain(remaining_assignments.items())
            .copied()
            .collect();
        assert_eq!(assignments.len(), 4);
        assert!(
            assignments
                .windows(2)
                .all(|items| items[0].id() < items[1].id())
        );
        assert!(assignments.iter().any(|item| {
            item.principal_id() == writer_id && item.custom_role_id() == Some(custom_role_id)
        }));
        Ok(())
    }

    #[test]
    fn security_key_pages_are_structurally_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (owner_id, owner_key) = catalog.bootstrap_owner("Owner", "owner-key", 1)?;
        catalog.activate_key(owner_key.id())?;
        let (second_key, _) = catalog.begin_key_issue(
            owner_id,
            "second-owner-key",
            [BuiltInRole::Owner],
            ProductAuthorization::ALL,
            2,
            None,
        )?;
        catalog.activate_key(second_key.id())?;
        let key_page = catalog.list_keys(SecurityKeyListRequest::new(None, 1)?)?;
        let cursor = key_page.next_cursor().ok_or("missing key cursor")?;
        let remaining = catalog.list_keys(SecurityKeyListRequest::new(Some(cursor), 1)?)?;
        let keys: Vec<_> = key_page.items().iter().chain(remaining.items()).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.windows(2).all(|items| items[0].id() < items[1].id()));
        let key = keys
            .iter()
            .copied()
            .find(|summary| summary.id() == owner_key.id())
            .ok_or("missing primary key summary")?;
        assert_eq!(key.id(), owner_key.id());
        assert_eq!(key.principal_id(), owner_id);
        assert_eq!(key.label(), "owner-key");
        assert_eq!(key.roles(), &[BuiltInRole::Owner]);
        assert_eq!(key.permission_ceiling(), ProductAuthorization::ALL);
        assert_eq!(key.scope_ceiling(), &[ProductScope::Instance]);
        assert_eq!(key.created_at_micros(), 1);

        let key_record = catalog.key(owner_key.id()).ok_or("missing key")?;
        let debug_pages = format!("{key_page:?}{remaining:?}");
        assert!(!debug_pages.contains(owner_key.expose_secret()));
        assert!(!debug_pages.contains(second_key.expose_secret()));
        let digest_hex =
            key_record
                .verifier
                .digest()
                .iter()
                .try_fold(String::new(), |mut output, byte| {
                    write!(&mut output, "{byte:02x}")?;
                    Ok::<_, std::fmt::Error>(output)
                })?;
        assert!(!debug_pages.contains(&digest_hex));
        Ok(())
    }

    #[test]
    fn security_catalog_pages_survive_reopen_and_corruption_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let key_path = path.with_extension("owner-security-list-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&key_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &key_path, 1)?;
        let secret = fs::read_to_string(&key_path)?;
        let owner = product.authenticate_api_key(&secret, 2)?;
        product.create_security_principal(&owner, "Durable reader", 2)?;
        let before = product
            .load_access_control_catalog()?
            .list_principals(SecurityPrincipalListRequest::new(None, 8)?)?;
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        let catalog = reopened.load_access_control_catalog()?;
        let after = catalog.list_principals(SecurityPrincipalListRequest::new(None, 8)?)?;
        assert_eq!(after, before);
        let mut corrupt = catalog.encode()?;
        corrupt[CATALOG_MAGIC.len()] ^= 1;
        assert_eq!(
            AccessControlCatalog::decode(&corrupt),
            Err(AccessCatalogError::CorruptCatalog)
        );

        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(key_path)?;
        Ok(())
    }

    #[test]
    fn authority_scope_intersection_table_requires_grant_and_key_ceiling()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = ObjectId::new(10)?;
        let child = ObjectId::new(11)?;
        let grandchild = ObjectId::new(12)?;
        let foreign = ObjectId::new(13)?;
        let is_descendant = |candidate, ancestor| {
            matches!(
                (candidate, ancestor),
                (value, parent) if (value == child && parent == root)
                    || (value == grandchild && (parent == child || parent == root))
            )
        };
        let cases = [
            (ProductScope::Instance, ProductScope::Instance, root, true),
            (
                ProductScope::Instance,
                ProductScope::CatalogSubtree(root),
                child,
                true,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::Instance,
                child,
                true,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::CatalogObject(child),
                child,
                true,
            ),
            (
                ProductScope::CatalogObject(child),
                ProductScope::CatalogSubtree(root),
                child,
                true,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::CatalogSubtree(child),
                grandchild,
                true,
            ),
            (
                ProductScope::CatalogObject(child),
                ProductScope::CatalogObject(child),
                child,
                true,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::CatalogObject(child),
                root,
                false,
            ),
            (
                ProductScope::CatalogObject(child),
                ProductScope::CatalogSubtree(root),
                grandchild,
                false,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::CatalogSubtree(child),
                child,
                true,
            ),
            (
                ProductScope::CatalogSubtree(root),
                ProductScope::CatalogSubtree(child),
                foreign,
                false,
            ),
            (
                ProductScope::CatalogObject(child),
                ProductScope::CatalogObject(foreign),
                child,
                false,
            ),
        ];

        for (grant, ceiling, target, expected) in cases {
            let authority = scoped_authority(ProductPermission::DataRead, grant, ceiling)?;
            assert_eq!(
                authority.allows_object(ProductPermission::DataRead, target, is_descendant),
                expected,
                "grant={grant:?}, ceiling={ceiling:?}, target={target:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn authority_scope_intersection_never_widens_instance_or_permission()
    -> Result<(), Box<dyn std::error::Error>> {
        let object = ObjectId::new(21)?;
        for grant in [
            ProductScope::Instance,
            ProductScope::CatalogSubtree(object),
            ProductScope::CatalogObject(object),
        ] {
            for ceiling in [
                ProductScope::Instance,
                ProductScope::CatalogSubtree(object),
                ProductScope::CatalogObject(object),
            ] {
                let authority = scoped_authority(ProductPermission::DataRead, grant, ceiling)?;
                assert_eq!(
                    authority.allows_instance(ProductPermission::DataRead),
                    grant == ProductScope::Instance && ceiling == ProductScope::Instance
                );
                assert!(!authority.allows_instance(ProductPermission::DataWrite));
                assert!(!authority.allows_object(
                    ProductPermission::DataWrite,
                    object,
                    |_candidate, _ancestor| false,
                ));
            }
        }
        let instance_only = scoped_authority(
            ProductPermission::SecurityManage,
            ProductScope::Instance,
            ProductScope::CatalogObject(object),
        )?;
        assert!(!instance_only.allows_object(
            ProductPermission::SecurityManage,
            object,
            |_candidate, _ancestor| false,
        ));
        Ok(())
    }

    #[test]
    fn bootstrap_authentication_codec_and_revocation_are_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (principal_id, issued) = catalog.bootstrap_owner("Local owner", "bootstrap", 17)?;
        let key_id = issued.id();
        let secret = issued.expose_secret().to_owned();
        assert_eq!(
            catalog.authenticate(&secret, 18),
            Err(AccessCatalogError::Unauthorized)
        );
        let active_epoch = catalog.activate_key(key_id)?;
        let authenticated = catalog.authenticate(&secret, 18)?;
        assert_eq!(authenticated.principal_id, principal_id);
        assert_eq!(authenticated.key_id, key_id);
        assert_eq!(authenticated.authorization, ProductAuthorization::ALL);
        assert_eq!(authenticated.authorization_epoch, active_epoch);

        let encoded = catalog.encode()?;
        let mut reopened = AccessControlCatalog::decode(&encoded)?;
        assert_eq!(reopened, catalog);
        assert_eq!(
            reopened.authenticate(&secret, 18)?.principal_id,
            principal_id
        );
        let next = reopened.revoke_key(key_id)?;
        assert_eq!(next, AuthorizationEpoch::new(3));
        assert_eq!(
            reopened.authenticate(&secret, 18),
            Err(AccessCatalogError::Unauthorized)
        );
        Ok(())
    }

    #[test]
    fn decoder_rejects_every_truncation_and_corruption() -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let _issued = catalog.bootstrap_owner("Owner", "primary", 0)?;
        let encoded = catalog.encode()?;
        for length in 0..encoded.len() {
            assert_eq!(
                AccessControlCatalog::decode(&encoded[..length]),
                Err(AccessCatalogError::CorruptCatalog),
                "truncation {length}"
            );
        }
        let mut corrupt = encoded;
        corrupt[24] ^= 0x01;
        assert_eq!(
            AccessControlCatalog::decode(&corrupt),
            Err(AccessCatalogError::CorruptCatalog)
        );
        Ok(())
    }

    #[test]
    fn malformed_unknown_and_wrong_keys_share_one_public_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_principal_id, issued) = catalog.bootstrap_owner("Owner", "primary", 0)?;
        let mut wrong = issued.expose_secret().to_owned();
        wrong.replace_range(101..102, "0");
        for candidate in ["", "not-a-key", wrong.as_str()] {
            assert_eq!(
                catalog.authenticate(candidate, 0),
                Err(AccessCatalogError::Unauthorized)
            );
        }
        Ok(())
    }

    #[test]
    fn catalog_commit_survives_strict_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let key_path = path.with_extension("owner-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&key_path);
        let mut product = NativeProduct::create(&path)?;
        assert_eq!(
            product.access_control_status()?,
            AccessControlCatalog::empty().status()
        );
        let receipt =
            product.bootstrap_access_control_to_file("Owner", "bootstrap", &key_path, 7)?;
        assert_eq!(receipt.commit.durability, ProductDurability::Strict);
        assert_eq!(receipt.authorization_epoch, AuthorizationEpoch::new(2));
        let secret = fs::read_to_string(&key_path)?;
        assert_eq!(secret.len(), 102);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&key_path)?.permissions().mode() & 0o777, 0o600);
        }
        let catalog = product.load_access_control_catalog()?;
        drop(product);

        let mut reopened = NativeProduct::open(&path)?;
        let loaded = reopened.load_access_control_catalog()?;
        assert_eq!(loaded, catalog);
        let authority = reopened.authenticate_api_key(&secret, 8)?;
        assert_eq!(authority.authorization, ProductAuthorization::ALL);
        let revoked = reopened.revoke_api_key(&authority, receipt.key_id, 9)?;
        assert_eq!(revoked.authorization_epoch, AuthorizationEpoch::new(3));
        assert_eq!(
            reopened.authenticate_api_key(&secret, 10).map(|_| ()),
            Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied
            ))
        );
        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(key_path)?;
        Ok(())
    }

    #[test]
    fn principal_role_and_narrow_key_mutations_reauthorize_each_epoch()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_key_path = path.with_extension("owner-key");
        let reader_key_path = path.with_extension("reader-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_key_path);
        let _ignored = fs::remove_file(&reader_key_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_key_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_key_path)?;
        let owner = product.authenticate_api_key(&owner_secret, 2)?;
        let created = product.create_security_principal(&owner, "Read service", 2)?;

        let owner = product.authenticate_api_key(&owner_secret, 3)?;
        product.set_security_principal_enabled_idempotent(
            &owner,
            created.principal_id,
            true,
            1_001,
            3,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, 4)?;
        let assignment = product.assign_built_in_role(
            &owner,
            created.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            4,
        )?;
        assert!(assignment.authorization_epoch > created.authorization_epoch);

        let owner = product.authenticate_api_key(&owner_secret, 5)?;
        let issued = product.issue_api_key_to_file(
            &owner,
            created.principal_id,
            "reader",
            [BuiltInRole::Reader],
            BuiltInRole::Reader.authorization(),
            None,
            &reader_key_path,
            5,
        )?;
        let reader_secret = fs::read_to_string(&reader_key_path)?;
        let reader = product.authenticate_api_key(&reader_secret, 6)?;
        assert_eq!(reader.principal_id, created.principal_id);
        assert_eq!(reader.effective_roles.as_ref(), &[BuiltInRole::Reader]);
        assert!(reader.authorization.allows(ProductPermission::DataRead));
        assert!(!reader.authorization.allows(ProductPermission::DataWrite));
        assert_eq!(reader.authorization_epoch, issued.authorization_epoch);

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_key_path)?;
        fs::remove_file(reader_key_path)?;
        Ok(())
    }

    #[test]
    fn admin_cannot_issue_rotate_or_revoke_owner_credentials()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-separation-key");
        let admin_path = path.with_extension("admin-separation-key");
        let issue_path = path.with_extension("forbidden-owner-issue-key");
        let rotate_path = path.with_extension("forbidden-owner-rotation-key");
        let _ignored = fs::remove_dir_all(&path);
        remove_test_files(&[&owner_path, &admin_path, &issue_path, &rotate_path]);
        let mut product = NativeProduct::create(&path)?;
        let bootstrap =
            product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, 2)?;
        let admin_principal = product.create_security_principal(&owner, "Admin", 2)?;
        let owner = product.authenticate_api_key(&owner_secret, 3)?;
        enable_principal(&mut product, &owner, admin_principal.principal_id, 1_002, 3)?;
        let owner = product.authenticate_api_key(&owner_secret, 4)?;
        let admin_assignment = product.assign_built_in_role(
            &owner,
            admin_principal.principal_id,
            BuiltInRole::Admin,
            ProductScope::Instance,
            4,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, 5)?;
        product.issue_api_key_to_file(
            &owner,
            admin_principal.principal_id,
            "admin",
            [BuiltInRole::Admin],
            BuiltInRole::Admin.authorization(),
            None,
            &admin_path,
            5,
        )?;
        let admin_secret = fs::read_to_string(&admin_path)?;
        let admin = product.authenticate_api_key(&admin_secret, 6)?;
        assert_self_assignment_revoke_denied(&mut product, &admin, admin_assignment.assignment_id);

        let Err(issue_error) = product.issue_api_key_to_file(
            &admin,
            bootstrap.principal_id,
            "owner-copy",
            [BuiltInRole::Owner],
            ProductAuthorization::ALL,
            None,
            &issue_path,
            5,
        ) else {
            return Err("Admin unexpectedly minted Owner authority".into());
        };
        assert_eq!(issue_error.code(), ProductErrorCode::AuthorizationDenied);
        assert!(!issue_path.exists());

        let Err(rotate_error) = product.rotate_api_key_to_file(
            &admin,
            bootstrap.key_id,
            "owner-rotation",
            0,
            None,
            &rotate_path,
            6,
        ) else {
            return Err("Admin unexpectedly rotated Owner authority".into());
        };
        assert_eq!(rotate_error.code(), ProductErrorCode::AuthorizationDenied);
        assert!(!rotate_path.exists());

        let Err(revoke_error) = product.revoke_api_key(&admin, bootstrap.key_id, 7) else {
            return Err("Admin unexpectedly revoked Owner authority".into());
        };
        assert_eq!(revoke_error.code(), ProductErrorCode::AuthorizationDenied);
        assert!(product.authenticate_api_key(&owner_secret, 8).is_ok());

        let owner = product.authenticate_api_key(&owner_secret, i64::MIN)?;
        let mut catalog = product.load_access_control_catalog()?;
        let (pending_owner, _) = catalog.begin_key_rotation(
            bootstrap.key_id,
            "pending-owner",
            0,
            product.trusted_authorization_time()?,
            None,
        )?;
        let pending_owner_id = pending_owner.id();
        let audit = SecurityAuditDraft::actor(
            &owner,
            SecurityAuditAction::RotateKey,
            [
                SecurityAuditTarget::Key(bootstrap.key_id),
                SecurityAuditTarget::Key(pending_owner_id),
            ],
        );
        product.commit_access_control_catalog(&mut catalog, 9, audit)?;
        let Err(abort_error) = product.abort_api_key_rotation(&admin, bootstrap.key_id, 10) else {
            return Err("Admin unexpectedly aborted an Owner rotation".into());
        };
        assert_eq!(abort_error.code(), ProductErrorCode::AuthorizationDenied);
        assert_eq!(product.access_control_status()?.pending_keys, 1);
        product.abort_api_key_rotation(&owner, bootstrap.key_id, 11)?;
        assert_eq!(product.access_control_status()?.pending_keys, 0);

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(admin_path)?;
        Ok(())
    }

    fn assert_self_assignment_revoke_denied(
        product: &mut NativeProduct,
        actor: &AuthenticatedAuthority,
        assignment_id: SecurityId,
    ) {
        assert_eq!(
            product
                .revoke_security_assignment_idempotent(actor, assignment_id, 1_005, 6)
                .map(|_| ()),
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest))
        );
    }

    fn enable_principal(
        product: &mut NativeProduct,
        actor: &AuthenticatedAuthority,
        principal_id: SecurityId,
        token: u128,
        logical_time_micros: i64,
    ) -> Result<(), ProductError> {
        product
            .set_security_principal_enabled_idempotent(
                actor,
                principal_id,
                true,
                token,
                logical_time_micros,
            )
            .map(|_| ())
    }

    #[test]
    fn security_mutation_and_audit_event_share_exact_strict_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let key_path = path.with_extension("owner-audit-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&key_path);
        let mut product = NativeProduct::create(&path)?;
        let bootstrap = product.bootstrap_access_control_to_file("Owner", "owner", &key_path, 1)?;
        let secret = fs::read_to_string(&key_path)?;
        let owner = product.authenticate_api_key(&secret, 2)?;
        let created = product.create_security_principal(&owner, "Service", 2)?;
        let owner = product.authenticate_api_key(&secret, 3)?;
        let page = product.read_security_audit(&owner, None, 16, 3)?;
        assert_eq!(page.events.len(), 3);
        assert_eq!(page.next_cursor, None);
        let event = page.events.last().ok_or("missing audit event")?;
        assert_eq!(event.commit_csn(), created.commit.commit_csn);
        assert_eq!(event.actor_principal_id(), Some(owner.principal_id()));
        assert_eq!(event.actor_key_id(), Some(owner.key_id()));
        assert_eq!(event.action(), SecurityAuditAction::CreatePrincipal);
        assert_eq!(event.result(), SecurityAuditResult::Succeeded);
        assert_eq!(
            event.targets(),
            &[SecurityAuditTarget::Principal(created.principal_id)]
        );
        let first_cursor = page.events[0].id();
        let after_first = product.read_security_audit(&owner, Some(first_cursor), 16, 3)?;
        assert_eq!(after_first.events.len(), 2);
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        let owner = reopened.authenticate_api_key(&secret, 4)?;
        let reopened_page = reopened.read_security_audit(&owner, None, 16, 4)?;
        assert_eq!(reopened_page, page);
        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(key_path)?;
        assert_eq!(bootstrap.commit.commit_csn, page.events[1].commit_csn());
        Ok(())
    }

    #[test]
    fn rotation_overlap_starts_at_activation_and_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_principal_id, predecessor) = catalog.bootstrap_owner("Owner", "primary", 1)?;
        let predecessor_id = predecessor.id();
        let predecessor_secret = predecessor.expose_secret().to_owned();
        catalog.activate_key(predecessor_id)?;
        let (successor, _) = catalog.begin_key_rotation(predecessor_id, "rotated", 5, 10, None)?;
        let successor_id = successor.id();
        let successor_secret = successor.expose_secret().to_owned();
        assert!(
            catalog
                .authenticate(&predecessor_secret, 99_999_999)
                .is_ok()
        );
        assert_eq!(
            catalog.authenticate(&successor_secret, 99_999_999),
            Err(AccessCatalogError::Unauthorized)
        );
        let (_, deadline) = catalog.activate_rotated_key(successor_id, 100_000_000)?;
        assert_eq!(deadline, 105_000_000);
        assert!(
            catalog
                .authenticate(&predecessor_secret, deadline - 1)
                .is_ok()
        );
        assert_eq!(
            catalog.authenticate(&predecessor_secret, deadline),
            Err(AccessCatalogError::Unauthorized)
        );
        assert!(catalog.authenticate(&successor_secret, deadline).is_ok());
        let encoded = catalog.encode()?;
        assert_eq!(AccessControlCatalog::decode(&encoded)?, catalog);

        let mut expired = AccessControlCatalog::empty();
        let (_, predecessor) = expired.bootstrap_owner("Owner", "primary", 1)?;
        let predecessor_id = predecessor.id();
        let predecessor_secret = predecessor.expose_secret().to_owned();
        expired.activate_key(predecessor_id)?;
        let (successor, _) =
            expired.begin_key_rotation(predecessor_id, "expired", 0, 10, Some(20))?;
        let expired_successor_id = successor.id();
        assert_eq!(
            expired.activate_rotated_key(expired_successor_id, 20),
            Err(AccessCatalogError::Conflict)
        );
        assert!(expired.authenticate(&predecessor_secret, 21).is_ok());
        expired.abort_key_rotation(expired_successor_id)?;
        assert!(expired.key(expired_successor_id).is_none());
        assert_eq!(
            expired
                .key(predecessor_id)
                .ok_or("missing predecessor after abort")?
                .successor_id(),
            None
        );
        let (retry, _) = expired.begin_key_rotation(predecessor_id, "retry", 0, 22, None)?;
        expired.activate_rotated_key(retry.id(), 23)?;
        Ok(())
    }

    #[test]
    fn rotation_retirement_is_bounded_and_pending_revoke_is_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_, initial) = catalog.bootstrap_owner("Owner", "primary", 1)?;
        let mut current_id = initial.id();
        catalog.activate_key(current_id)?;
        for instant in 2_i64..102 {
            let (successor, _) =
                catalog.begin_key_rotation(current_id, "rotated", 0, instant, None)?;
            let successor_id = successor.id();
            catalog.activate_rotated_key(successor_id, instant)?;
            assert!(catalog.keys.len() <= 2);
            assert!(catalog.key(current_id).is_some_and(ApiKeyRecord::revoked));
            current_id = successor_id;
        }
        let (_, _recovery, _) = catalog.begin_owner_recovery("recovery", 103)?;
        assert_eq!(catalog.keys.len(), 3);

        let mut overlapping = AccessControlCatalog::empty();
        let (_, initial) = overlapping.bootstrap_owner("Owner", "primary", 1)?;
        let initial_id = initial.id();
        overlapping.activate_key(initial_id)?;
        let (current, _) = overlapping.begin_key_rotation(initial_id, "current", 1, 2, None)?;
        let current_id = current.id();
        let (_, deadline) = overlapping.activate_rotated_key(current_id, 3)?;
        assert!(matches!(
            overlapping.begin_key_rotation(current_id, "too-early", 0, deadline - 1, None),
            Err(AccessCatalogError::Conflict)
        ));
        let (pending, _) =
            overlapping.begin_key_rotation(current_id, "after-overlap", 0, deadline, None)?;
        let pending_id = pending.id();
        assert!(overlapping.key(initial_id).is_none());
        assert_eq!(
            overlapping.revoke_key(pending_id),
            Err(AccessCatalogError::Conflict)
        );
        overlapping.revoke_key(current_id)?;
        overlapping.abort_key_rotation(pending_id)?;
        assert_eq!(
            overlapping
                .key(current_id)
                .ok_or("missing revoked predecessor")?
                .successor_id(),
            None
        );
        Ok(())
    }

    #[test]
    fn rotation_cycles_fail_closed_without_traversal_loops()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_, first) = catalog.bootstrap_owner("Owner", "first", 1)?;
        let first_id = first.id();
        catalog.activate_key(first_id)?;
        let (second, _) = catalog.begin_key_rotation(first_id, "second", 1, 1, None)?;
        let second_id = second.id();
        {
            let first = catalog
                .keys
                .get_mut(&first_id)
                .ok_or("missing first cycle key")?;
            first.predecessor_id = Some(second_id);
            first.successor_id = Some(second_id);
            first.overlap_until_micros = Some(10);
            first.rotation_overlap_micros = Some(1);
        }
        {
            let second = catalog
                .keys
                .get_mut(&second_id)
                .ok_or("missing second cycle key")?;
            second.active = true;
            second.predecessor_id = Some(first_id);
            second.successor_id = Some(first_id);
            second.overlap_until_micros = Some(10);
            second.rotation_overlap_micros = Some(1);
        }
        assert_eq!(catalog.validate(), Err(AccessCatalogError::CorruptCatalog));
        assert_eq!(
            catalog.retired_rotation_ancestors(first_id, 10),
            Err(AccessCatalogError::CorruptCatalog)
        );
        Ok(())
    }

    #[test]
    fn revoked_unlinked_keys_do_not_exhaust_the_lifetime_quota()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (owner_principal, primary) = catalog.bootstrap_owner("Owner", "primary", 1)?;
        catalog.activate_key(primary.id())?;
        for instant in 2_i64..82 {
            let label = format!("retired-{instant}");
            let (issued, _) = catalog.begin_key_issue(
                owner_principal,
                &label,
                [BuiltInRole::Owner],
                ProductAuthorization::ALL,
                instant,
                None,
            )?;
            catalog.activate_key(issued.id())?;
            catalog.revoke_key(issued.id())?;
            assert!(catalog.keys.len() <= AccessControlLimits::V1.keys_per_principal);
        }
        assert!(catalog.authenticate(primary.expose_secret(), 82).is_ok());
        let encoded = catalog.encode()?;
        assert_eq!(AccessControlCatalog::decode(&encoded)?, catalog);
        Ok(())
    }

    #[test]
    fn owner_principal_reserves_one_offline_recovery_slot() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut catalog = AccessControlCatalog::empty();
        let (owner_principal, primary) = catalog.bootstrap_owner("Owner", "primary", 1)?;
        catalog.activate_key(primary.id())?;
        for instant in 2_i64..64 {
            let label = format!("active-{instant}");
            let (issued, _) = catalog.begin_key_issue(
                owner_principal,
                &label,
                [BuiltInRole::Owner],
                ProductAuthorization::ALL,
                instant,
                None,
            )?;
            catalog.activate_key(issued.id())?;
        }
        assert_eq!(catalog.keys.len(), 63);
        assert!(matches!(
            catalog.begin_key_issue(
                owner_principal,
                "must-reserve-recovery",
                [BuiltInRole::Owner],
                ProductAuthorization::ALL,
                64,
                None,
            ),
            Err(AccessCatalogError::LimitExceeded)
        ));
        let (_, recovery, _) = catalog.begin_owner_recovery("recovery", 65)?;
        assert_eq!(catalog.keys.len(), 64);
        catalog
            .keys
            .get_mut(&recovery.id())
            .ok_or("missing pending recovery key")?
            .active = true;
        assert_eq!(catalog.validate(), Err(AccessCatalogError::CorruptCatalog));
        catalog
            .keys
            .get_mut(&recovery.id())
            .ok_or("missing pending recovery key")?
            .active = false;
        catalog.activate_recovered_owner_key(recovery.id())?;
        assert_eq!(catalog.keys.len(), 1);
        assert!(catalog.authenticate(recovery.expose_secret(), 66).is_ok());
        Ok(())
    }

    #[test]
    fn pending_rotation_can_be_aborted_by_predecessor_and_retried()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-abort-key");
        let retry_path = path.with_extension("retry-rotation-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let _ignored = fs::remove_file(&retry_path);
        let mut product = NativeProduct::create(&path)?;
        let bootstrap =
            product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;

        let mut catalog = product.load_access_control_catalog()?;
        let (pending, _) = catalog.begin_key_rotation(
            bootstrap.key_id,
            "interrupted",
            0,
            product.trusted_authorization_time()?,
            None,
        )?;
        let pending_id = pending.id();
        let audit = SecurityAuditDraft::actor(
            &owner,
            SecurityAuditAction::RotateKey,
            [
                SecurityAuditTarget::Key(bootstrap.key_id),
                SecurityAuditTarget::Key(pending_id),
            ],
        );
        product.commit_access_control_catalog(&mut catalog, 2, audit)?;
        assert_eq!(product.access_control_status()?.pending_keys, 1);

        product.abort_api_key_rotation(&owner, bootstrap.key_id, 3)?;
        assert_eq!(product.access_control_status()?.pending_keys, 0);
        let owner = product.authenticate_api_key(&owner_secret, i64::MIN)?;
        let retry = product.rotate_api_key_to_file(
            &owner,
            bootstrap.key_id,
            "retry",
            0,
            None,
            &retry_path,
            4,
        )?;
        assert_ne!(retry.successor_key_id, pending_id);
        let successor_secret = fs::read_to_string(&retry_path)?;
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        assert_eq!(reopened.access_control_status()?.pending_keys, 0);
        let successor = reopened.authenticate_api_key(&successor_secret, i64::MIN)?;
        let audit_page = reopened.read_security_audit(&successor, None, 32, 5)?;
        assert!(audit_page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::AbortKeyRotation
                && event.targets().len() == 2
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Key(bootstrap.key_id))
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Key(pending_id))
        }));

        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(retry_path)?;
        Ok(())
    }

    #[test]
    fn pending_key_issue_can_be_aborted_by_principal_and_label()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-pending-issue-key");
        let reader_path = path.with_extension("reader-retry-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let _ignored = fs::remove_file(&reader_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let reader = product.create_security_principal(&owner, "Reader", 2)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MIN)?;
        product.set_security_principal_enabled_idempotent(
            &owner,
            reader.principal_id,
            true,
            1_003,
            3,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.assign_built_in_role(
            &owner,
            reader.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            4,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;

        let mut catalog = product.load_access_control_catalog()?;
        let created_at = product.trusted_authorization_time()?;
        let (pending, _) = catalog.begin_key_issue_with_roles(
            reader.principal_id,
            "reader",
            [BuiltInRole::Reader],
            [],
            BuiltInRole::Reader.authorization(),
            [ProductScope::Instance],
            created_at,
            None,
        )?;
        let pending_id = pending.id();
        assert!(matches!(
            catalog.begin_key_issue_with_roles(
                reader.principal_id,
                "reader",
                [BuiltInRole::Reader],
                [],
                BuiltInRole::Reader.authorization(),
                [ProductScope::Instance],
                created_at,
                None,
            ),
            Err(AccessCatalogError::Conflict)
        ));
        let audit = SecurityAuditDraft::actor(
            &owner,
            SecurityAuditAction::IssueKey,
            [
                SecurityAuditTarget::Principal(reader.principal_id),
                SecurityAuditTarget::Key(pending_id),
            ],
        );
        product.commit_access_control_catalog(&mut catalog, 4, audit)?;
        assert_eq!(product.access_control_status()?.pending_keys, 1);

        product.abort_pending_api_key_issue(&owner, reader.principal_id, "reader", 5)?;
        assert_eq!(product.access_control_status()?.pending_keys, 0);
        drop(product);

        let mut product = NativeProduct::open(&path)?;
        assert_eq!(product.access_control_status()?.pending_keys, 0);
        let owner = product.authenticate_api_key(&owner_secret, i64::MIN)?;
        let retry = product.issue_api_key_to_file(
            &owner,
            reader.principal_id,
            "reader",
            [BuiltInRole::Reader],
            BuiltInRole::Reader.authorization(),
            None,
            &reader_path,
            6,
        )?;
        assert_ne!(retry.key_id, pending_id);
        let reader_secret = fs::read_to_string(&reader_path)?;
        let reader_authority = product.authenticate_api_key(&reader_secret, i64::MAX)?;
        let audit_page = product.read_security_audit(&owner, None, 32, 7)?;
        assert!(audit_page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::AbortKeyIssue
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Principal(reader.principal_id))
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Key(pending_id))
        }));
        assert_eq!(reader_authority.principal_id(), reader.principal_id);

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(reader_path)?;
        Ok(())
    }

    #[test]
    fn custom_only_key_scope_ceiling_cannot_authorize_instance_admin()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-custom-key");
        let scoped_path = path.with_extension("scoped-custom-key");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let _ignored = fs::remove_file(&scoped_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, 2)?;
        let principal = product.create_security_principal(&owner, "Scoped reader", 2)?;
        let object = ObjectId::new(42)?;
        let owner = product.authenticate_api_key(&owner_secret, 3)?;
        product.set_security_principal_enabled_idempotent(
            &owner,
            principal.principal_id,
            true,
            1_004,
            3,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, 4)?;
        let role = product.create_custom_security_role(
            &owner,
            "tenant reader",
            [CustomRoleGrant::new(
                ProductPermission::DataRead,
                ProductScope::CatalogObject(object),
            )
            .ok_or("invalid custom grant")?],
            4,
        )?;
        let owner = product.authenticate_api_key(&owner_secret, 5)?;
        product.assign_custom_security_role(&owner, principal.principal_id, role.role_id, 5)?;
        let owner = product.authenticate_api_key(&owner_secret, 6)?;
        product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            "scoped",
            [],
            [role.role_id],
            ProductAuthorization::from_permissions([ProductPermission::DataRead]),
            [ProductScope::CatalogObject(object)],
            None,
            &scoped_path,
            6,
        )?;
        let scoped_secret = fs::read_to_string(&scoped_path)?;
        let scoped = product.authenticate_api_key(&scoped_secret, 7)?;
        assert_eq!(scoped.effective_roles(), &[]);
        assert_eq!(scoped.effective_custom_roles(), &[role.role_id]);
        assert_eq!(
            scoped.scope_ceiling(),
            &[ProductScope::CatalogObject(object)]
        );
        assert!(scoped.authorization().allows(ProductPermission::DataRead));
        assert!(!authority_allows_instance(
            &scoped,
            ProductPermission::DataRead
        ));
        assert_eq!(
            product
                .create_security_principal(&scoped, "Escalation", 6)
                .map(|_| ()),
            Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied
            ))
        );
        let mut session = crate::ProductSession::new_authenticated(
            crate::ProductSessionId::new(1).ok_or("invalid session")?,
            scoped,
        );
        let context = crate::ProductRequestContext::new(
            1,
            session.id(),
            i64::MAX,
            session.principal().clone(),
            session.authorization(),
        )
        .with_authorization_epoch(session.authorization_epoch());
        let Err(error) = product.dispatch(
            &mut session,
            &context,
            crate::ProductOperation::StructureGet {
                key: b"must-not-escape-scope".to_vec(),
            },
        ) else {
            return Err("object scope escaped into the default keyspace".into());
        };
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(scoped_path)?;
        Ok(())
    }

    #[test]
    fn owner_recovery_keeps_old_key_until_atomic_activation()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-before-recovery");
        let recovery_path = path.with_extension("owner-after-recovery");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let _ignored = fs::remove_file(&recovery_path);
        let mut created = NativeProduct::create(&path)?;
        created.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let old_secret = fs::read_to_string(&owner_path)?;
        assert!(created.authenticate_api_key(&old_secret, 2).is_ok());
        drop(created);
        let mut product = NativeProduct::open_offline_owner(&path)?;
        assert!(product.authenticate_api_key(&old_secret, 2).is_ok());
        let recovered =
            product.recover_owner_access_offline_to_file("recovered", &recovery_path, 3)?;
        let new_secret = fs::read_to_string(&recovery_path)?;
        assert_eq!(
            product.authenticate_api_key(&old_secret, 4).map(|_| ()),
            Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied
            ))
        );
        let replacement = product.authenticate_api_key(&new_secret, 4)?;
        assert_eq!(replacement.principal_id(), recovered.principal_id);
        assert_eq!(replacement.key_id(), recovered.key_id);
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        assert!(reopened.authenticate_api_key(&new_secret, 5).is_ok());
        assert_eq!(
            reopened.authenticate_api_key(&old_secret, 5).map(|_| ()),
            Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied
            ))
        );
        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(recovery_path)?;
        Ok(())
    }

    #[test]
    fn owner_recovery_rejects_replacement_of_pending_key() -> Result<(), Box<dyn std::error::Error>>
    {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-recovery-retry");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;

        let mut catalog = product.load_access_control_catalog()?;
        let (principal_id, _, first_pending, _, first_retired) =
            catalog.begin_owner_recovery_with_retired("recovery-first", 2)?;
        assert!(first_retired.is_empty());
        let first_pending_id = first_pending.id();
        product.commit_access_control_catalog(
            &mut catalog,
            2,
            SecurityAuditDraft::offline(
                SecurityAuditAction::RecoverOwner,
                [
                    SecurityAuditTarget::Principal(principal_id),
                    SecurityAuditTarget::Key(first_pending_id),
                ],
            ),
        )?;

        let mut catalog = product.load_access_control_catalog()?;
        assert_eq!(
            catalog
                .begin_owner_recovery_with_retired("recovery-retry", 3)
                .map(|_| ()),
            Err(AccessCatalogError::Conflict)
        );

        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let audit_page = product.read_security_audit(&owner, None, 32, 4)?;
        assert!(audit_page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::RecoverOwner
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Key(first_pending_id))
        }));
        assert_eq!(product.access_control_status()?.pending_keys, 1);

        drop(product);
        let reopened = NativeProduct::open(&path)?;
        assert!(
            reopened
                .authenticate_api_key(&owner_secret, i64::MIN)
                .is_ok()
        );
        assert_eq!(reopened.access_control_status()?.pending_keys, 1);
        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        Ok(())
    }

    #[test]
    fn offline_owner_recovery_activation_and_abort_survive_unknown_ack()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-offline-replay");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let mut created = NativeProduct::create(&path)?;
        created.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        drop(created);

        let mut product = NativeProduct::open_offline_owner(&path)?;
        let started = product.start_owner_recovery_offline("activate", 2)?;
        let secret = started.secret.expose_secret().to_owned();
        let pending_key = started.key_id;
        let pending_epoch = started.authorization_epoch;
        let activated =
            product.resume_owner_recovery_offline(pending_key, &secret, pending_epoch, 3)?;
        drop(product);
        let mut reopened = NativeProduct::open_offline_owner(&path)?;
        let replay =
            reopened.resume_owner_recovery_offline(pending_key, &secret, pending_epoch, 4)?;
        assert_eq!(activated, replay);

        let started = reopened.start_owner_recovery_offline("abort", 5)?;
        let aborted = reopened.abort_owner_recovery_offline(
            started.key_id,
            started.authorization_epoch,
            6,
        )?;
        drop(reopened);
        let mut reopened = NativeProduct::open_offline_owner(&path)?;
        let replay = reopened.abort_owner_recovery_offline(
            started.key_id,
            started.authorization_epoch,
            7,
        )?;
        assert_eq!(aborted, replay);
        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        Ok(())
    }

    #[test]
    fn offline_owner_recovery_pending_catalog_round_trips_with_redacted_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = AccessControlCatalog::empty();
        let (_, owner) = catalog.bootstrap_owner("Owner", "owner", 1)?;
        catalog.activate_key(owner.id())?;
        let (_, operation_id, pending, epoch, _) =
            catalog.begin_owner_recovery_with_retired("pending", 2)?;
        let pending_key = pending.id();
        let encoded = catalog.encode()?;
        assert!(!encoded.windows(5).any(|window| window == b"hyp1_"));
        let decoded = AccessControlCatalog::decode(&encoded)?;
        let provenance = decoded
            .pending_owner_recovery
            .ok_or("missing pending provenance")?;
        assert_eq!(provenance.operation_id(), operation_id);
        assert_eq!(provenance.key_id(), pending_key);
        assert_eq!(provenance.authorization_epoch(), epoch);
        assert_eq!(
            provenance.provenance(),
            OwnerRecoveryProvenance::OfflineOperatingSystemOwner
        );
        Ok(())
    }

    #[test]
    fn every_offline_owner_activation_boundary_recovers_pending_or_complete()
    -> Result<(), Box<dyn std::error::Error>> {
        use hyphae_native_runtime::CommitBoundary;

        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let path = temporary_directory().with_extension(format!("owner-{boundary:?}"));
            let owner_path = path.with_extension("owner-key");
            let _ignored = fs::remove_dir_all(&path);
            let _ignored = fs::remove_file(&owner_path);
            let mut created = NativeProduct::create(&path)?;
            created.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
            let old_secret = fs::read_to_string(&owner_path)?;
            drop(created);
            let mut product = NativeProduct::open_offline_owner(&path)?;
            let started = product.start_owner_recovery_offline("pending", 2)?;
            let new_secret = started.secret.expose_secret().to_owned();
            let pending = product
                .load_access_control_catalog()?
                .pending_owner_recovery
                .ok_or("missing pending recovery")?;
            let mut catalog = product.load_access_control_catalog()?;
            let principal_id = catalog
                .key(started.key_id)
                .ok_or("missing pending key")?
                .principal_id();
            let (authorization_epoch, retired) =
                catalog.activate_recovered_owner_key_with_retired(started.key_id)?;
            let mut targets = vec![
                SecurityAuditTarget::Principal(principal_id),
                SecurityAuditTarget::Key(started.key_id),
            ];
            targets.extend(retired.iter().copied().map(SecurityAuditTarget::Key));
            let result = product.commit_access_control_catalog_recording_offline_recovery_at(
                &mut catalog,
                3,
                SecurityAuditDraft::offline(SecurityAuditAction::ActivateKey, targets),
                OfflineOwnerRecoveryOutcome::Activated,
                pending,
                Some(crate::SecurityCommitInterruption::returning(boundary)),
            );
            assert!(result.is_err());
            drop(product);

            let mut reopened = NativeProduct::open_offline_owner(&path)?;
            let inspection = reopened.inspect_owner_recovery_offline()?;
            if inspection.pending.is_some() {
                assert!(reopened.authenticate_api_key(&old_secret, 4).is_ok());
                assert!(reopened.authenticate_api_key(&new_secret, 4).is_err());
                let receipt = reopened.resume_owner_recovery_offline(
                    started.key_id,
                    &new_secret,
                    started.authorization_epoch,
                    4,
                )?;
                assert_eq!(receipt.authorization_epoch, authorization_epoch);
            } else {
                assert!(reopened.authenticate_api_key(&old_secret, 4).is_err());
                assert!(reopened.authenticate_api_key(&new_secret, 4).is_ok());
                let receipt = reopened.resume_owner_recovery_offline(
                    started.key_id,
                    &new_secret,
                    started.authorization_epoch,
                    4,
                )?;
                assert_eq!(receipt.authorization_epoch, authorization_epoch);
            }
            drop(reopened);
            fs::remove_dir_all(path)?;
            fs::remove_file(owner_path)?;
        }
        Ok(())
    }

    #[test]
    fn security_mutation_marker_codecs_are_bounded_redacted_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let marker = SecurityMutationMarker {
            operation: SecurityMutationOperation::CreatePrincipal,
            request_digest: [7; 32],
            actor_principal_id: SecurityId::new(1).ok_or("invalid principal")?,
            actor_key_id: ApiKeyId::from_bytes([2; 16]).ok_or("invalid key")?,
            result_id: SecurityMutationResultId::Security(
                SecurityId::new(3).ok_or("invalid result")?,
            ),
            predecessor_key_id: None,
            overlap_until_micros: None,
            authorization_epoch: AuthorizationEpoch::new(4),
            transaction_id: 5,
        };
        let encoded = encode_security_mutation_marker(marker);
        assert_eq!(encoded.len(), SECURITY_MUTATION_MARKER_BYTES);
        assert_eq!(decode_security_mutation_marker(&encoded)?, marker);
        let mut corrupt = encoded.clone();
        corrupt[12] ^= 1;
        assert_eq!(
            decode_security_mutation_marker(&corrupt),
            Err(AccessCatalogError::CorruptCatalog)
        );
        let mut unknown_operation = encoded.clone();
        unknown_operation[SECURITY_MUTATION_MARKER_MAGIC.len()] = u8::MAX;
        let content_len = unknown_operation.len() - CATALOG_DIGEST_BYTES;
        let digest = blake3::hash(&unknown_operation[..content_len]);
        unknown_operation[content_len..].copy_from_slice(digest.as_bytes());
        assert_eq!(
            decode_security_mutation_marker(&unknown_operation),
            Err(AccessCatalogError::CorruptCatalog)
        );

        let mut fingerprints = Vec::new();
        for suffix in 1..=SECURITY_MUTATION_MARKERS_PER_SHARD {
            let mut fingerprint = [0_u8; 32];
            fingerprint[31] = u8::try_from(suffix)?;
            fingerprints.push(fingerprint);
        }
        let index = SecurityMutationMarkerIndex { fingerprints };
        let encoded_index = encode_security_mutation_index(&index, 0)?;
        assert_eq!(decode_security_mutation_index(&encoded_index, 0)?, index);
        assert_eq!(
            decode_security_mutation_index(&encoded_index, 1),
            Err(AccessCatalogError::CorruptCatalog)
        );
        let duplicate = SecurityMutationMarkerIndex {
            fingerprints: vec![[0; 32], [0; 32]],
        };
        assert_eq!(
            encode_security_mutation_index(&duplicate, 0),
            Err(AccessCatalogError::CorruptCatalog)
        );
        Ok(())
    }

    #[test]
    fn create_principal_idempotency_replays_exact_receipt_and_conflicts_on_reuse()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-write-idempotency");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;

        let first =
            product.create_security_principal_idempotent(&owner, "Disabled service", 41, 2)?;
        let replay =
            product.create_security_principal_idempotent(&owner, "Disabled service", 41, 3)?;
        assert_eq!(replay, first);
        let catalog = product.load_access_control_catalog()?;
        assert!(
            !catalog
                .principal(first.principal_id)
                .ok_or("missing principal")?
                .enabled()
        );
        let fingerprint = security_mutation_fingerprint(&owner, 41);
        let snapshot = product.snapshot_bounded(i64::MAX)?;
        let marker = decode_security_mutation_marker(
            snapshot
                .structure_get_internal(&security_mutation_marker_key(fingerprint))
                .ok_or("missing mutation marker")?,
        )?;
        assert_eq!(
            marker.result_id,
            SecurityMutationResultId::Security(first.principal_id)
        );
        assert_eq!(marker.authorization_epoch, first.authorization_epoch);
        assert_eq!(marker.transaction_id, first.commit.transaction_id.get());
        drop(snapshot);
        let event = product
            .read_security_audit(&owner, None, 16, 3)?
            .events
            .into_iter()
            .find(|event| {
                event.action() == SecurityAuditAction::CreatePrincipal
                    && event
                        .targets()
                        .contains(&SecurityAuditTarget::Principal(first.principal_id))
            })
            .ok_or("missing create-principal audit")?;
        assert_eq!(event.commit_csn(), first.commit.commit_csn);
        let Err(conflict) =
            product.create_security_principal_idempotent(&owner, "Different request", 41, 4)
        else {
            return Err("token reuse did not conflict".into());
        };
        assert_eq!(conflict.code(), ProductErrorCode::IdempotencyConflict);
        let Err(operation_conflict) = product.set_security_principal_enabled_idempotent(
            &owner,
            first.principal_id,
            true,
            41,
            4,
        ) else {
            return Err("cross-operation token reuse did not conflict".into());
        };
        assert_eq!(
            operation_conflict.code(),
            ProductErrorCode::IdempotencyConflict
        );
        let Err(zero) = product.create_security_principal_idempotent(&owner, "Zero token", 0, 5)
        else {
            return Err("zero idempotency token succeeded".into());
        };
        assert_eq!(zero.code(), ProductErrorCode::InvalidRequest);

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        Ok(())
    }

    #[test]
    fn security_write_plane_enforces_owner_self_and_epoch_invariants()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-write-plane");
        let reader_path = path.with_extension("reader-write-plane");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let _ignored = fs::remove_file(&reader_path);
        let mut product = NativeProduct::create(&path)?;
        let bootstrap =
            product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let (principal_id, built_in_assignment, custom_assignment) =
            prepare_security_write_subject(&mut product, &owner_secret, bootstrap)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.issue_api_key_to_file(
            &owner,
            principal_id,
            "reader",
            [BuiltInRole::Reader],
            BuiltInRole::Reader.authorization(),
            None,
            &reader_path,
            8,
        )?;
        let reader_secret = fs::read_to_string(&reader_path)?;
        let reader = product.authenticate_api_key(&reader_secret, i64::MAX)?;

        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let revoked =
            product.revoke_security_assignment_idempotent(&owner, custom_assignment, 56, 9)?;
        assert_eq!(
            product.revoke_security_assignment_idempotent(&owner, custom_assignment, 56, 10,)?,
            revoked
        );
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        product.set_security_principal_enabled_idempotent(&owner, principal_id, false, 57, 11)?;
        let mut reader_session = crate::ProductSession::new_authenticated(
            crate::ProductSessionId::new(701).ok_or("invalid reader session")?,
            reader,
        );
        let reader_context = crate::ProductRequestContext::new(
            702,
            reader_session.id(),
            i64::MAX,
            reader_session.principal().clone(),
            reader_session.authorization(),
        )
        .with_authorization_epoch(reader_session.authorization_epoch());
        let Err(session_error) = product.dispatch(
            &mut reader_session,
            &reader_context,
            crate::ProductOperation::StructureGet {
                key: b"epoch-invalidated".to_vec(),
            },
        ) else {
            return Err("disabled principal retained a managed session".into());
        };
        assert_eq!(session_error.code(), ProductErrorCode::AuthorizationDenied);
        assert_security_write_audits(&product, &owner, built_in_assignment, 12)?;

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        fs::remove_file(reader_path)?;
        Ok(())
    }

    fn prepare_security_write_subject(
        product: &mut NativeProduct,
        owner_secret: &str,
        bootstrap: AccessControlBootstrapReceipt,
    ) -> Result<(SecurityId, SecurityId, SecurityId), Box<dyn std::error::Error>> {
        let owner = product.authenticate_api_key(owner_secret, i64::MAX)?;
        let principal = product.create_security_principal_idempotent(&owner, "Reader", 51, 2)?;
        let owner = product.authenticate_api_key(owner_secret, i64::MAX)?;
        let enabled = product.set_security_principal_enabled_idempotent(
            &owner,
            principal.principal_id,
            true,
            52,
            3,
        )?;
        assert_eq!(
            product.set_security_principal_enabled_idempotent(
                &owner,
                principal.principal_id,
                true,
                52,
                4,
            )?,
            enabled
        );
        assert_write_mutation_owner_guards(product, &owner, bootstrap, 4)?;
        let owner = product.authenticate_api_key(owner_secret, i64::MAX)?;
        let built_in = product.assign_built_in_role_idempotent(
            &owner,
            principal.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            53,
            5,
        )?;
        assert_eq!(
            product.assign_built_in_role_idempotent(
                &owner,
                principal.principal_id,
                BuiltInRole::Reader,
                ProductScope::Instance,
                53,
                5,
            )?,
            built_in
        );
        let owner = product.authenticate_api_key(owner_secret, i64::MAX)?;
        let audit_grant =
            CustomRoleGrant::new(ProductPermission::AuditRead, ProductScope::Instance)
                .ok_or("invalid grant")?;
        let role = product.create_custom_security_role_idempotent(
            &owner,
            "Audited reader",
            [audit_grant],
            54,
            6,
        )?;
        assert_eq!(
            product.create_custom_security_role_idempotent(
                &owner,
                "Audited reader",
                [audit_grant],
                54,
                6,
            )?,
            role
        );
        let owner = product.authenticate_api_key(owner_secret, i64::MAX)?;
        let custom = product.assign_custom_security_role_idempotent(
            &owner,
            principal.principal_id,
            role.role_id,
            55,
            7,
        )?;
        assert_eq!(
            product.assign_custom_security_role_idempotent(
                &owner,
                principal.principal_id,
                role.role_id,
                55,
                7,
            )?,
            custom
        );
        Ok((
            principal.principal_id,
            built_in.assignment_id,
            custom.assignment_id,
        ))
    }

    fn assert_write_mutation_owner_guards(
        product: &mut NativeProduct,
        owner: &AuthenticatedAuthority,
        bootstrap: AccessControlBootstrapReceipt,
        logical_time_micros: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let owner_assignment = product
            .load_access_control_catalog()?
            .assignments
            .values()
            .find(|assignment| assignment.principal_id == bootstrap.principal_id)
            .ok_or("missing owner assignment")?
            .id;
        assert_eq!(
            product
                .assign_built_in_role_idempotent(
                    owner,
                    bootstrap.principal_id,
                    BuiltInRole::Owner,
                    ProductScope::Instance,
                    91,
                    logical_time_micros,
                )
                .map(|_| ()),
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest))
        );
        assert_eq!(
            product
                .set_security_principal_enabled_idempotent(
                    owner,
                    bootstrap.principal_id,
                    false,
                    92,
                    logical_time_micros,
                )
                .map(|_| ()),
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest))
        );
        assert_eq!(
            product
                .revoke_security_assignment_idempotent(
                    owner,
                    owner_assignment,
                    93,
                    logical_time_micros,
                )
                .map(|_| ()),
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest))
        );
        Ok(())
    }

    fn assert_security_write_audits(
        product: &NativeProduct,
        owner: &AuthenticatedAuthority,
        built_in_assignment: SecurityId,
        logical_time_micros: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let page = product.read_security_audit(owner, None, 32, logical_time_micros)?;
        assert!(page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::SetPrincipalEnabled
                && event.actor_key_id() == Some(owner.key_id())
        }));
        assert!(page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::RevokeAssignment
                && event
                    .targets()
                    .iter()
                    .any(|target| matches!(target, SecurityAuditTarget::Assignment(_)))
        }));
        assert!(page.events.iter().any(|event| {
            event.action() == SecurityAuditAction::AssignBuiltInRole
                && event
                    .targets()
                    .contains(&SecurityAuditTarget::Assignment(built_in_assignment))
        }));
        Ok(())
    }

    #[test]
    fn security_idempotency_fifo_evicts_only_oldest_marker_in_one_shard()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension("owner-idempotency-fifo");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        let tokens = same_shard_tokens(&owner, SECURITY_MUTATION_MARKERS_PER_SHARD + 1);
        let first_shard = security_mutation_shard(security_mutation_fingerprint(&owner, tokens[0]));
        let mut first_receipt = None;
        for (index, token) in tokens.iter().copied().enumerate() {
            let receipt = product.create_security_principal_idempotent(
                &owner,
                &format!("retained-{index}"),
                token,
                i64::try_from(index + 2)?,
            )?;
            first_receipt.get_or_insert(receipt);
        }
        let snapshot = product.snapshot_bounded(i64::MAX)?;
        let index = decode_security_mutation_index(
            snapshot
                .structure_get_internal(&security_mutation_index_key(first_shard))
                .ok_or("missing marker index")?,
            first_shard,
        )?;
        assert_eq!(
            index.fingerprints.len(),
            SECURITY_MUTATION_MARKERS_PER_SHARD
        );
        assert!(
            snapshot
                .structure_get_internal(&security_mutation_marker_key(
                    security_mutation_fingerprint(&owner, tokens[0],)
                ))
                .is_none()
        );
        let outside_window =
            product.create_security_principal_idempotent(&owner, "retained-0", tokens[0], 100)?;
        assert_ne!(
            outside_window,
            first_receipt.ok_or("missing first receipt")?
        );

        drop(product);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        Ok(())
    }

    fn same_shard_tokens(actor: &AuthenticatedAuthority, count: usize) -> Vec<u128> {
        let target_shard = security_mutation_shard(security_mutation_fingerprint(actor, 1));
        (1_u128..)
            .filter(|token| {
                security_mutation_shard(security_mutation_fingerprint(actor, *token))
                    == target_shard
            })
            .take(count)
            .collect()
    }

    #[test]
    fn create_principal_ack_unknown_reopens_to_one_exact_replay()
    -> Result<(), Box<dyn std::error::Error>> {
        for (index, boundary) in [
            hyphae_native_runtime::CommitBoundary::WalAppended,
            hyphae_native_runtime::CommitBoundary::WalSynchronized,
            hyphae_native_runtime::CommitBoundary::RootPublished,
        ]
        .into_iter()
        .enumerate()
        {
            assert_create_principal_ack_unknown_replay(boundary, u128::try_from(index + 101)?)?;
        }
        Ok(())
    }

    fn assert_create_principal_ack_unknown_replay(
        boundary: hyphae_native_runtime::CommitBoundary,
        token: u128,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary_directory();
        let owner_path = path.with_extension(format!("owner-crash-{token}"));
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner_path);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_path)?;
        let owner = product.authenticate_api_key(&owner_secret, i64::MAX)?;
        assert!(
            product
                .create_security_principal_idempotent_with_interruption(
                    &owner,
                    "Crash-safe principal",
                    token,
                    2,
                    Some(crate::SecurityCommitInterruption::returning(boundary)),
                )
                .is_err()
        );
        drop(product);

        let mut reopened = NativeProduct::open(&path)?;
        let owner = reopened.authenticate_api_key(&owner_secret, i64::MAX)?;
        let receipt = reopened.create_security_principal_idempotent(
            &owner,
            "Crash-safe principal",
            token,
            3,
        )?;
        let catalog = reopened.load_access_control_catalog()?;
        assert_eq!(
            catalog
                .principals
                .values()
                .filter(|principal| principal.display_name() == "Crash-safe principal")
                .count(),
            1
        );
        assert_eq!(
            catalog
                .principal(receipt.principal_id)
                .map(SecurityPrincipalRecord::enabled),
            Some(false)
        );
        let events = reopened.read_security_audit(&owner, None, 16, 4)?;
        let matching = events
            .events
            .iter()
            .filter(|event| {
                event.action() == SecurityAuditAction::CreatePrincipal
                    && event
                        .targets()
                        .contains(&SecurityAuditTarget::Principal(receipt.principal_id))
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].commit_csn(), receipt.commit.commit_csn);

        drop(reopened);
        fs::remove_dir_all(path)?;
        fs::remove_file(owner_path)?;
        Ok(())
    }
}
