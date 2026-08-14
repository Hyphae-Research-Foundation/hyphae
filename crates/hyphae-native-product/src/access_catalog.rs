// SPDX-License-Identifier: AGPL-3.0-only

//! Canonical durable access-control catalog state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use hyphae_native_types::ObjectId;
use subtle::ConstantTimeEq;

use crate::{
    AccessControlLimits, ApiKeyId, ApiKeyVerifier, AuthorizationEpoch, BuiltInRole, IssuedApiKey,
    NativeProduct, ProductAuthorization, ProductCommitReceipt, ProductDurability, ProductError,
    ProductErrorCode, ProductPermission, ProductPrincipal, ProductScope, SecurityId,
};

const CATALOG_MAGIC: &[u8; 8] = b"HYACAT01";
const CATALOG_DIGEST_BYTES: usize = 32;
const MAX_ACCESS_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const ACCESS_CONTROL_STORAGE_KEY: &[u8] = b"\0hyphae.product.access-control.v1\0catalog";

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
    permission_ceiling: ProductAuthorization,
    created_at_micros: i64,
    expires_at_micros: Option<i64>,
    revoked: bool,
    published_epoch: AuthorizationEpoch,
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
}

/// Authenticated product authority resolved from current durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedAuthority {
    /// Stable principal identity.
    pub principal_id: SecurityId,
    /// Public key identity used for authentication.
    pub key_id: ApiKeyId,
    /// Transport-independent product principal.
    pub principal: ProductPrincipal,
    /// Effective permission intersection.
    pub authorization: ProductAuthorization,
    /// Current durable authorization generation.
    pub authorization_epoch: AuthorizationEpoch,
}

/// Canonical bounded access-control catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessControlCatalog {
    epoch: AuthorizationEpoch,
    principals: BTreeMap<SecurityId, SecurityPrincipalRecord>,
    assignments: BTreeMap<SecurityId, BuiltInRoleAssignment>,
    keys: BTreeMap<ApiKeyId, ApiKeyRecord>,
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
    /// Durable key records, including revoked metadata.
    pub keys: usize,
    /// Key records awaiting restricted-output activation.
    pub pending_keys: usize,
}

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

/// Definite result of one durable access-control mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessControlMutationReceipt {
    /// Authorization generation after publication.
    pub authorization_epoch: AuthorizationEpoch,
    /// Strict native commit that published the mutation.
    pub commit: ProductCommitReceipt,
}

impl AccessControlCatalog {
    /// Returns an empty unbootstrapped catalog.
    pub fn empty() -> Self {
        Self {
            epoch: AuthorizationEpoch::UNMANAGED,
            principals: BTreeMap::new(),
            assignments: BTreeMap::new(),
            keys: BTreeMap::new(),
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
        if !self.principals.is_empty() || !self.assignments.is_empty() || !self.keys.is_empty() {
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
                permission_ceiling: ProductAuthorization::ALL,
                created_at_micros,
                expires_at_micros: None,
                revoked: false,
                published_epoch: self.epoch,
            },
        );
        Ok((principal_id, issued))
    }

    /// Returns the current durable authorization generation.
    pub const fn epoch(&self) -> AuthorizationEpoch {
        self.epoch
    }

    /// Returns whether the catalog has durable principals.
    pub fn is_bootstrapped(&self) -> bool {
        !self.principals.is_empty()
    }

    /// Returns one redacted principal record.
    pub fn principal(&self, id: SecurityId) -> Option<&SecurityPrincipalRecord> {
        self.principals.get(&id)
    }

    /// Returns one redacted key record.
    pub fn key(&self, id: ApiKeyId) -> Option<&ApiKeyRecord> {
        self.keys.get(&id)
    }

    /// Returns redacted aggregate status without credential verifiers.
    pub fn status(&self) -> AccessControlStatus {
        AccessControlStatus {
            bootstrapped: self.is_bootstrapped(),
            epoch: self.epoch,
            principals: self.principals.len(),
            assignments: self.assignments.len(),
            keys: self.keys.len(),
            pending_keys: self.keys.values().filter(|key| !key.active).count(),
        }
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
        let key_id =
            ApiKeyVerifier::candidate_id(candidate).ok_or(AccessCatalogError::Unauthorized)?;
        let key = self
            .keys
            .get(&key_id)
            .ok_or(AccessCatalogError::Unauthorized)?;
        if !key.active
            || key.revoked
            || key
                .expires_at_micros
                .is_some_and(|expiry| logical_time_micros >= expiry)
            || !key.verifier.verifies(candidate)
        {
            return Err(AccessCatalogError::Unauthorized);
        }
        let principal = self
            .principals
            .get(&key.principal_id)
            .filter(|principal| principal.enabled)
            .ok_or(AccessCatalogError::Unauthorized)?;
        let assigned_roles: BTreeSet<_> = self
            .assignments
            .values()
            .filter(|assignment| assignment.principal_id == principal.id)
            .map(|assignment| assignment.role)
            .collect();
        let authorization = key
            .roles
            .iter()
            .copied()
            .filter(|role| assigned_roles.contains(role))
            .fold(ProductAuthorization::NONE, |current, role| {
                current.union(role.authorization())
            })
            .intersect(key.permission_ceiling);
        if authorization == ProductAuthorization::NONE {
            return Err(AccessCatalogError::Unauthorized);
        }
        let product_principal = ProductPrincipal::new(principal.id.to_string())
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        Ok(AuthenticatedAuthority {
            principal_id: principal.id,
            key_id,
            principal: product_principal,
            authorization,
            authorization_epoch: self.epoch,
        })
    }

    /// Revokes one key and advances the global authorization epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is absent, already revoked, or the epoch
    /// cannot advance.
    pub fn revoke_key(&mut self, id: ApiKeyId) -> Result<AuthorizationEpoch, AccessCatalogError> {
        let next_epoch = self
            .epoch
            .checked_next()
            .ok_or(AccessCatalogError::LimitExceeded)?;
        let key = self.keys.get_mut(&id).ok_or(AccessCatalogError::NotFound)?;
        if key.revoked {
            return Err(AccessCatalogError::Conflict);
        }
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
            output.extend_from_slice(key.id.as_bytes());
            output.extend_from_slice(&key.principal_id.to_be_bytes());
            output.extend_from_slice(key.verifier.digest());
            output.push(u8::from(key.active));
            output.push(u8::from(key.revoked));
            output.extend_from_slice(&key.created_at_micros.to_be_bytes());
            match key.expires_at_micros {
                Some(expiry) => {
                    output.push(1);
                    output.extend_from_slice(&expiry.to_be_bytes());
                }
                None => output.push(0),
            }
            output.extend_from_slice(&key.published_epoch.get().to_be_bytes());
            output.extend_from_slice(&key.permission_ceiling.bits().to_be_bytes());
            output.push(
                u8::try_from(key.roles.len()).map_err(|_| AccessCatalogError::LimitExceeded)?,
            );
            output.extend(key.roles.iter().map(|role| role.tag()));
            push_string(&mut output, &key.label)?;
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
        if decoder.take(CATALOG_MAGIC.len())? != CATALOG_MAGIC {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        let epoch = AuthorizationEpoch::new(decoder.u64()?);
        let principal_count = decoder.count(AccessControlLimits::V1.principals)?;
        let assignment_limit = AccessControlLimits::V1
            .principals
            .checked_mul(AccessControlLimits::V1.assignments_per_principal)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let assignment_count = decoder.count(assignment_limit)?;
        let key_limit = AccessControlLimits::V1
            .principals
            .checked_mul(AccessControlLimits::V1.keys_per_principal)
            .ok_or(AccessCatalogError::CorruptCatalog)?;
        let key_count = decoder.count(key_limit)?;
        let catalog = Self {
            epoch,
            principals: decode_principals(&mut decoder, principal_count)?,
            assignments: decode_assignments(&mut decoder, assignment_count)?,
            keys: decode_keys(&mut decoder, key_count)?,
        };
        if !decoder.is_empty() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        catalog.validate()?;
        if catalog.encode()?.as_slice() != encoded {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        Ok(catalog)
    }

    fn validate(&self) -> Result<(), AccessCatalogError> {
        let limits = AccessControlLimits::V1;
        if !limits.is_valid()
            || self.principals.len() > limits.principals
            || self
                .keys
                .values()
                .any(|key| !self.principals.contains_key(&key.principal_id))
            || self
                .assignments
                .values()
                .any(|assignment| !self.principals.contains_key(&assignment.principal_id))
        {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        for principal in self.principals.values() {
            validate_display_name(&principal.display_name)?;
            let assignments = self
                .assignments
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
        for key in self.keys.values() {
            validate_display_name(&key.label)?;
            if key.roles.is_empty() || !strictly_sorted(&key.roles) {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        }
        if self.principals.is_empty() {
            if self.epoch != AuthorizationEpoch::UNMANAGED
                || !self.assignments.is_empty()
                || !self.keys.is_empty()
            {
                return Err(AccessCatalogError::CorruptCatalog);
            }
        } else if self.epoch == AuthorizationEpoch::UNMANAGED {
            return Err(AccessCatalogError::CorruptCatalog);
        }
        Ok(())
    }
}

impl Default for AccessControlCatalog {
    fn default() -> Self {
        Self::empty()
    }
}

impl NativeProduct {
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
        logical_time_micros: i64,
    ) -> Result<AuthenticatedAuthority, ProductError> {
        self.load_access_control_catalog()?
            .authenticate(candidate, logical_time_micros)
            .map_err(map_catalog_error)
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
        let mut catalog = self.load_access_control_catalog()?;
        if actor.authorization_epoch != catalog.epoch() {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let target_principal = catalog
            .key(target)
            .map(ApiKeyRecord::principal_id)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        let allowed = if target_principal == actor.principal_id {
            actor
                .authorization
                .allows(ProductPermission::CredentialSelfManage)
                || actor
                    .authorization
                    .allows(ProductPermission::SecurityManage)
        } else {
            actor
                .authorization
                .allows(ProductPermission::SecurityManage)
        };
        if !allowed {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let authorization_epoch = catalog.revoke_key(target).map_err(map_catalog_error)?;
        let commit = self.commit_access_control_catalog(&catalog, logical_time_micros)?;
        Ok(AccessControlMutationReceipt {
            authorization_epoch,
            commit,
        })
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
        if let Err(error) = self.commit_access_control_catalog(&catalog, logical_time_micros) {
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
        let commit = self.commit_access_control_catalog(&catalog, logical_time_micros)?;
        Ok(AccessControlBootstrapReceipt {
            principal_id,
            key_id: issued.id(),
            authorization_epoch,
            commit,
        })
    }

    pub(crate) fn load_access_control_catalog(&self) -> Result<AccessControlCatalog, ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        match snapshot.structure_get(ACCESS_CONTROL_STORAGE_KEY) {
            Some(encoded) => AccessControlCatalog::decode(encoded).map_err(map_catalog_error),
            None => Ok(AccessControlCatalog::empty()),
        }
    }

    pub(crate) fn commit_access_control_catalog(
        &mut self,
        catalog: &AccessControlCatalog,
        logical_time_micros: i64,
    ) -> Result<ProductCommitReceipt, ProductError> {
        let encoded = catalog.encode().map_err(map_catalog_error)?;
        let mut transaction = self
            .database
            .begin(logical_time_micros, ProductDurability::Strict.into())?;
        transaction.set(ACCESS_CONTROL_STORAGE_KEY.to_vec(), encoded, None)?;
        let receipt = transaction.commit()?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }
}

fn map_catalog_error(error: AccessCatalogError) -> ProductError {
    let code = match error {
        AccessCatalogError::AlreadyBootstrapped | AccessCatalogError::Conflict => {
            ProductErrorCode::CatalogConflict
        }
        AccessCatalogError::InvalidDisplayName => ProductErrorCode::InvalidRequest,
        AccessCatalogError::Entropy => ProductErrorCode::Unavailable,
        AccessCatalogError::LimitExceeded => ProductErrorCode::LimitExceeded,
        AccessCatalogError::NotFound => ProductErrorCode::ObjectNotFound,
        AccessCatalogError::Unauthorized => ProductErrorCode::AuthorizationDenied,
        AccessCatalogError::CorruptCatalog => ProductErrorCode::Corruption,
    };
    ProductError::from_code(code)
}

fn create_restricted_output(path: &Path) -> Result<File, ProductError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(path)
        .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
    file.sync_all()
        .map_err(|_| ProductError::from_code(ProductErrorCode::Io))?;
    sync_output_parent(path)?;
    Ok(file)
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

fn decode_keys(
    decoder: &mut Decoder<'_>,
    count: usize,
) -> Result<BTreeMap<ApiKeyId, ApiKeyRecord>, AccessCatalogError> {
    let mut keys = BTreeMap::new();
    for _ in 0..count {
        let id = ApiKeyId::from_bytes(decoder.array()?);
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
            permission_ceiling,
            created_at_micros,
            expires_at_micros,
            revoked,
            published_epoch,
        };
        if keys.insert(id, record).is_some() {
            return Err(AccessCatalogError::CorruptCatalog);
        }
    }
    Ok(keys)
}

fn decode_optional_expiry(decoder: &mut Decoder<'_>) -> Result<Option<i64>, AccessCatalogError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => decoder.i64().map(Some),
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

/// Stable catalog construction, authentication, or codec error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessCatalogError {
    /// Access control already has durable principals.
    AlreadyBootstrapped,
    /// Security display text is empty, oversized, or contains control bytes.
    InvalidDisplayName,
    /// The operating-system CSPRNG failed.
    Entropy,
    /// A bounded v1 limit was exceeded.
    LimitExceeded,
    /// The target record is absent.
    NotFound,
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
            Self::Entropy => formatter.write_str("security entropy is unavailable"),
            Self::LimitExceeded => formatter.write_str("access-control limit exceeded"),
            Self::NotFound => formatter.write_str("security record not found"),
            Self::Conflict => formatter.write_str("security state conflicts with the request"),
            Self::Unauthorized => formatter.write_str("unauthorized"),
            Self::CorruptCatalog => formatter.write_str("access-control catalog is corrupt"),
        }
    }
}

impl std::error::Error for AccessCatalogError {}

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
}
