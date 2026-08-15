// SPDX-License-Identifier: AGPL-3.0-only

//! Native identity, role, scope, and API-key primitives.

use std::{fmt, str::FromStr};

use hyphae_native_types::ObjectId;
use subtle::ConstantTimeEq;

use crate::{ProductAuthorization, ProductPermission};

const API_KEY_PREFIX: &[u8] = b"hyp1_";
const API_KEY_SEPARATOR_INDEX: usize = 37;
pub(crate) const API_KEY_BYTES: usize = 102;
const API_KEY_ID_BYTES: usize = 16;
const API_KEY_SECRET_BYTES: usize = 32;
const API_KEY_VERIFIER_DOMAIN: &[u8] = b"hyphae-api-key-v1\0";

/// Maximum UTF-8 bytes in a security display name or credential label.
pub const MAX_SECURITY_DISPLAY_NAME_BYTES: usize = 128;

/// Finite Native access-control v1 authority limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessControlLimits {
    /// Durable principals.
    pub principals: usize,
    /// Mutable custom roles.
    pub custom_roles: usize,
    /// Direct grants in one custom role.
    pub grants_per_role: usize,
    /// Role assignments for one principal.
    pub assignments_per_principal: usize,
    /// Credentials for one principal, including revoked metadata.
    pub keys_per_principal: usize,
    /// UTF-8 bytes in one display name or key label.
    pub display_name_bytes: usize,
    /// Canonical bytes in one durable security event.
    pub audit_event_bytes: usize,
    /// Durable security events retained by the local catalog.
    pub retained_audit_events: usize,
    /// Events returned by one bounded audit read.
    pub audit_result_rows: usize,
    /// Maximum immediate-predecessor key overlap.
    pub maximum_rotation_overlap_seconds: u64,
    /// Verifiers evaluated by one authentication request.
    pub authentication_verifiers_per_request: usize,
    /// Process-local effective-authorization cache entries.
    pub authorization_cache_entries: usize,
}

impl AccessControlLimits {
    /// Native access-control v1 default authority.
    pub const V1: Self = Self {
        principals: 4_096,
        custom_roles: 1_024,
        grants_per_role: 256,
        assignments_per_principal: 128,
        keys_per_principal: 64,
        display_name_bytes: MAX_SECURITY_DISPLAY_NAME_BYTES,
        audit_event_bytes: 4_096,
        retained_audit_events: 100_000,
        audit_result_rows: 1_000,
        maximum_rotation_overlap_seconds: 604_800,
        authentication_verifiers_per_request: 1,
        authorization_cache_entries: 4_096,
    };

    /// Returns whether every bound remains finite and positive.
    pub const fn is_valid(self) -> bool {
        self.principals > 0
            && self.custom_roles > 0
            && self.grants_per_role > 0
            && self.assignments_per_principal > 0
            && self.keys_per_principal > 0
            && self.display_name_bytes > 0
            && self.audit_event_bytes > 0
            && self.retained_audit_events > 0
            && self.audit_result_rows > 0
            && self.maximum_rotation_overlap_seconds > 0
            && self.authentication_verifiers_per_request == 1
            && self.authorization_cache_entries > 0
    }
}

impl Default for AccessControlLimits {
    fn default() -> Self {
        Self::V1
    }
}

/// Monotonic durable generation used to invalidate cached authorization.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AuthorizationEpoch(u64);

impl AuthorizationEpoch {
    /// Compatibility generation for callers not yet backed by durable RBAC.
    pub const UNMANAGED: Self = Self(0);
    /// First generation of a bootstrapped access-control catalog.
    pub const INITIAL: Self = Self(1);

    /// Constructs an epoch from its durable representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the durable integer generation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances the epoch without wrapping.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// One nonzero 128-bit identity for durable security state.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecurityId(u128);

impl SecurityId {
    /// Constructs a checked nonzero identity.
    pub const fn new(value: u128) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Generates an identity with the operating-system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`AccessControlError::EntropyUnavailable`] when the operating
    /// system cannot provide random bytes.
    pub fn generate() -> Result<Self, AccessControlError> {
        loop {
            let mut bytes = [0_u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| AccessControlError::EntropyUnavailable)?;
            if let Some(identity) = Self::new(u128::from_be_bytes(bytes)) {
                return Ok(identity);
            }
        }
    }

    /// Returns the primitive identity.
    pub const fn get(self) -> u128 {
        self.0
    }

    /// Returns the canonical binary representation.
    pub const fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }
}

impl fmt::Debug for SecurityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for SecurityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl FromStr for SecurityId {
    type Err = AccessControlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 || !value.bytes().all(is_lower_hex) {
            return Err(AccessControlError::InvalidIdentity);
        }
        let parsed =
            u128::from_str_radix(value, 16).map_err(|_| AccessControlError::InvalidIdentity)?;
        Self::new(parsed).ok_or(AccessControlError::InvalidIdentity)
    }
}

/// Stable public identity of one API key.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApiKeyId([u8; API_KEY_ID_BYTES]);

impl ApiKeyId {
    pub(crate) fn from_bytes(bytes: [u8; API_KEY_ID_BYTES]) -> Option<Self> {
        bytes.iter().any(|byte| *byte != 0).then_some(Self(bytes))
    }

    /// Returns the canonical binary identity.
    pub const fn as_bytes(&self) -> &[u8; API_KEY_ID_BYTES] {
        &self.0
    }
}

impl fmt::Debug for ApiKeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ApiKeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_lower_hex(formatter, &self.0)
    }
}

/// One-time API-key material returned only to an explicit secret sink.
pub struct IssuedApiKey {
    id: ApiKeyId,
    serialized: Box<str>,
}

impl IssuedApiKey {
    /// Returns the public key identity.
    pub const fn id(&self) -> ApiKeyId {
        self.id
    }

    /// Exposes the complete secret for one-time restricted-file output.
    pub fn expose_secret(&self) -> &str {
        &self.serialized
    }
}

impl fmt::Debug for IssuedApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedApiKey")
            .field("id", &self.id)
            .field("serialized", &"[REDACTED]")
            .finish()
    }
}

/// Durable verifier for one API key. It never retains the raw secret.
#[derive(Clone, Eq, PartialEq)]
pub struct ApiKeyVerifier {
    id: ApiKeyId,
    digest: [u8; 32],
}

impl ApiKeyVerifier {
    /// Generates a fresh key and its durable verifier.
    ///
    /// # Errors
    ///
    /// Returns [`AccessControlError::EntropyUnavailable`] when the operating
    /// system cannot provide random bytes.
    pub fn issue() -> Result<(Self, IssuedApiKey), AccessControlError> {
        let mut id = [0_u8; API_KEY_ID_BYTES];
        let mut secret = [0_u8; API_KEY_SECRET_BYTES];
        getrandom::fill(&mut id).map_err(|_| AccessControlError::EntropyUnavailable)?;
        getrandom::fill(&mut secret).map_err(|_| AccessControlError::EntropyUnavailable)?;
        if id.iter().all(|byte| *byte == 0) {
            return Err(AccessControlError::EntropyUnavailable);
        }
        Ok(Self::from_parts(id, secret))
    }

    /// Imports a canonical key into a verifier and one-time secret value.
    ///
    /// This is intended for deterministic migration and test fixtures. Normal
    /// creation must use [`Self::issue`].
    ///
    /// # Errors
    ///
    /// Returns [`AccessControlError::InvalidApiKey`] when the input is not the
    /// exact canonical v1 representation.
    pub fn import(serialized: &str) -> Result<(Self, IssuedApiKey), AccessControlError> {
        let (id, secret) = parse_api_key(serialized)?;
        let verifier = Self::verifier(id, secret);
        let issued = IssuedApiKey {
            id: ApiKeyId(id),
            serialized: serialized.into(),
        };
        Ok((verifier, issued))
    }

    /// Returns the public key identity.
    pub const fn id(&self) -> ApiKeyId {
        self.id
    }

    /// Verifies one canonical candidate in constant time after strict parsing.
    pub fn verifies(&self, candidate: &str) -> bool {
        let Ok((candidate_id, candidate_secret)) = parse_api_key(candidate) else {
            return false;
        };
        let candidate_digest = key_digest(&candidate_id, &candidate_secret);
        bool::from(self.id.0.ct_eq(&candidate_id) & self.digest.ct_eq(&candidate_digest))
    }

    /// Returns the durable domain-separated verifier bytes.
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn from_digest(id: ApiKeyId, digest: [u8; 32]) -> Self {
        Self { id, digest }
    }

    pub(crate) fn candidate_id(candidate: &str) -> Option<ApiKeyId> {
        parse_api_key(candidate).ok().map(|(id, _)| ApiKeyId(id))
    }

    fn from_parts(
        id: [u8; API_KEY_ID_BYTES],
        secret: [u8; API_KEY_SECRET_BYTES],
    ) -> (Self, IssuedApiKey) {
        let mut serialized = String::with_capacity(API_KEY_BYTES);
        serialized.push_str("hyp1_");
        push_lower_hex(&mut serialized, &id);
        serialized.push('_');
        push_lower_hex(&mut serialized, &secret);
        (
            Self::verifier(id, secret),
            IssuedApiKey {
                id: ApiKeyId(id),
                serialized: serialized.into_boxed_str(),
            },
        )
    }

    fn verifier(id: [u8; API_KEY_ID_BYTES], secret: [u8; API_KEY_SECRET_BYTES]) -> Self {
        Self {
            id: ApiKeyId(id),
            digest: key_digest(&id, &secret),
        }
    }
}

impl fmt::Debug for ApiKeyVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiKeyVerifier")
            .field("id", &self.id)
            .field("digest", &"[REDACTED]")
            .finish()
    }
}

/// One built-in immutable role.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BuiltInRole {
    /// Unique ownership and recovery authority.
    Owner,
    /// Product and security administration without ownership transfer.
    Admin,
    /// Health, maintenance, backup, verification, and audit.
    Operator,
    /// Schema and application development.
    Developer,
    /// Application read/write and search.
    Writer,
    /// Application read, search, and proofs.
    Reader,
    /// Metadata, telemetry, security metadata, audit, and verification.
    Auditor,
}

impl BuiltInRole {
    /// Parses one reserved lowercase role identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "operator" => Some(Self::Operator),
            "developer" => Some(Self::Developer),
            "writer" => Some(Self::Writer),
            "reader" => Some(Self::Reader),
            "auditor" => Some(Self::Auditor),
            _ => None,
        }
    }

    /// Returns the reserved lowercase role identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Operator => "operator",
            Self::Developer => "developer",
            Self::Writer => "writer",
            Self::Reader => "reader",
            Self::Auditor => "auditor",
        }
    }

    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Owner => 0,
            Self::Admin => 1,
            Self::Operator => 2,
            Self::Developer => 3,
            Self::Writer => 4,
            Self::Reader => 5,
            Self::Auditor => 6,
        }
    }

    pub(crate) const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Owner),
            1 => Some(Self::Admin),
            2 => Some(Self::Operator),
            3 => Some(Self::Developer),
            4 => Some(Self::Writer),
            5 => Some(Self::Reader),
            6 => Some(Self::Auditor),
            _ => None,
        }
    }

    /// Returns the exact immutable permission set.
    pub fn authorization(self) -> ProductAuthorization {
        match self {
            Self::Owner => ProductAuthorization::ALL,
            Self::Admin => ProductAuthorization::ALL.without(ProductPermission::OwnershipManage),
            Self::Operator => ProductAuthorization::from_permissions([
                ProductPermission::AuditRead,
                ProductPermission::BackupCreate,
                ProductPermission::BackupVerify,
                ProductPermission::CatalogRead,
                ProductPermission::CredentialSelfManage,
                ProductPermission::Discover,
                ProductPermission::Maintain,
                ProductPermission::Observe,
                ProductPermission::ProofVerify,
            ]),
            Self::Developer => ProductAuthorization::from_permissions([
                ProductPermission::CatalogRead,
                ProductPermission::CatalogWrite,
                ProductPermission::CredentialSelfManage,
                ProductPermission::DataRead,
                ProductPermission::DataWrite,
                ProductPermission::Discover,
                ProductPermission::Observe,
                ProductPermission::ProofGenerate,
                ProductPermission::ProofVerify,
                ProductPermission::SearchExecute,
            ]),
            Self::Writer => ProductAuthorization::from_permissions([
                ProductPermission::CatalogRead,
                ProductPermission::CredentialSelfManage,
                ProductPermission::DataRead,
                ProductPermission::DataWrite,
                ProductPermission::Discover,
                ProductPermission::ProofGenerate,
                ProductPermission::ProofVerify,
                ProductPermission::SearchExecute,
            ]),
            Self::Reader => {
                ProductAuthorization::READ_ONLY.union(ProductAuthorization::from_permissions([
                    ProductPermission::CredentialSelfManage,
                ]))
            }
            Self::Auditor => ProductAuthorization::from_permissions([
                ProductPermission::AuditRead,
                ProductPermission::BackupVerify,
                ProductPermission::CatalogRead,
                ProductPermission::CredentialSelfManage,
                ProductPermission::Discover,
                ProductPermission::Observe,
                ProductPermission::ProofVerify,
                ProductPermission::SecurityRead,
            ]),
        }
    }
}

impl fmt::Display for BuiltInRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable resource boundary attached to one permission grant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProductScope {
    /// Complete local product instance.
    Instance,
    /// One catalog object and every descendant in the bound snapshot.
    CatalogSubtree(ObjectId),
    /// Exactly one catalog object.
    CatalogObject(ObjectId),
}

impl ProductScope {
    /// Returns whether this scope covers `candidate` in the bound catalog.
    pub fn covers_object(
        self,
        candidate: ObjectId,
        is_descendant: impl FnOnce(ObjectId, ObjectId) -> bool,
    ) -> bool {
        match self {
            Self::Instance => true,
            Self::CatalogObject(object) => object == candidate,
            Self::CatalogSubtree(root) => root == candidate || is_descendant(candidate, root),
        }
    }
}

/// Stable access-control construction or parsing error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessControlError {
    /// The operating system did not provide cryptographic entropy.
    EntropyUnavailable,
    /// A stable security identity was zero or not canonical lowercase hex.
    InvalidIdentity,
    /// An API key was not exact canonical v1 syntax.
    InvalidApiKey,
}

impl fmt::Display for AccessControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => formatter.write_str("security entropy is unavailable"),
            Self::InvalidIdentity => formatter.write_str("invalid security identity"),
            Self::InvalidApiKey => formatter.write_str("invalid API key"),
        }
    }
}

impl std::error::Error for AccessControlError {}

impl ProductPermission {
    /// Returns the canonical append-only dotted permission identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuditRead => "audit.read",
            Self::BackupCreate => "backup.create",
            Self::BackupVerify => "backup.verify",
            Self::CatalogRead => "catalog.read",
            Self::CatalogWrite => "catalog.write",
            Self::CredentialSelfManage => "credential.self_manage",
            Self::DataRead => "data.read",
            Self::DataWrite => "data.write",
            Self::Discover => "discover",
            Self::Maintain => "maintain",
            Self::Observe => "observe",
            Self::OwnershipManage => "ownership.manage",
            Self::ProofGenerate => "proof.generate",
            Self::ProofVerify => "proof.verify",
            Self::Restore => "restore",
            Self::SearchExecute => "search.execute",
            Self::SecurityManage => "security.manage",
            Self::SecurityRead => "security.read",
        }
    }

    /// Parses one canonical permission identifier. Unknown values fail closed.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "audit.read" => Some(Self::AuditRead),
            "backup.create" => Some(Self::BackupCreate),
            "backup.verify" => Some(Self::BackupVerify),
            "catalog.read" => Some(Self::CatalogRead),
            "catalog.write" => Some(Self::CatalogWrite),
            "credential.self_manage" => Some(Self::CredentialSelfManage),
            "data.read" => Some(Self::DataRead),
            "data.write" => Some(Self::DataWrite),
            "discover" => Some(Self::Discover),
            "maintain" => Some(Self::Maintain),
            "observe" => Some(Self::Observe),
            "ownership.manage" => Some(Self::OwnershipManage),
            "proof.generate" => Some(Self::ProofGenerate),
            "proof.verify" => Some(Self::ProofVerify),
            "restore" => Some(Self::Restore),
            "search.execute" => Some(Self::SearchExecute),
            "security.manage" => Some(Self::SecurityManage),
            "security.read" => Some(Self::SecurityRead),
            _ => None,
        }
    }

    pub(crate) const fn tag(self) -> u8 {
        self as u8
    }

    pub(crate) const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::AuditRead),
            1 => Some(Self::BackupCreate),
            2 => Some(Self::BackupVerify),
            3 => Some(Self::CatalogRead),
            4 => Some(Self::CatalogWrite),
            5 => Some(Self::CredentialSelfManage),
            6 => Some(Self::DataRead),
            7 => Some(Self::DataWrite),
            8 => Some(Self::Discover),
            9 => Some(Self::Maintain),
            10 => Some(Self::Observe),
            11 => Some(Self::OwnershipManage),
            12 => Some(Self::ProofGenerate),
            13 => Some(Self::ProofVerify),
            14 => Some(Self::Restore),
            15 => Some(Self::SearchExecute),
            16 => Some(Self::SecurityManage),
            17 => Some(Self::SecurityRead),
            _ => None,
        }
    }

    /// Returns whether this permission may be granted at `scope`.
    pub const fn supports_scope(self, scope: ProductScope) -> bool {
        match scope {
            ProductScope::Instance => true,
            ProductScope::CatalogSubtree(_) | ProductScope::CatalogObject(_) => matches!(
                self,
                Self::CatalogRead
                    | Self::CatalogWrite
                    | Self::DataRead
                    | Self::DataWrite
                    | Self::ProofGenerate
                    | Self::SearchExecute
            ),
        }
    }
}

impl ProductAuthorization {
    /// Removes one permission from this set.
    #[must_use]
    pub const fn without(self, permission: ProductPermission) -> Self {
        Self::from_bits(self.bits() & !(1_u64 << permission as u8))
    }

    /// Intersects this set with a credential permission ceiling.
    #[must_use]
    pub const fn intersect(self, ceiling: Self) -> Self {
        Self::from_bits(self.bits() & ceiling.bits())
    }

    /// Returns whether every permission is included in `authority`.
    pub const fn is_subset_of(self, authority: Self) -> bool {
        authority.allows_all(self)
    }

    /// Removes permissions that are invalid at the requested scope kind.
    #[must_use]
    pub fn for_scope(self, scope: ProductScope) -> Self {
        ALL_PRODUCT_PERMISSIONS
            .into_iter()
            .filter(|permission| self.allows(*permission) && permission.supports_scope(scope))
            .fold(Self::NONE, |result, permission| {
                result.union(Self::from_permissions([permission]))
            })
    }
}

const ALL_PRODUCT_PERMISSIONS: [ProductPermission; 18] = [
    ProductPermission::AuditRead,
    ProductPermission::BackupCreate,
    ProductPermission::BackupVerify,
    ProductPermission::CatalogRead,
    ProductPermission::CatalogWrite,
    ProductPermission::CredentialSelfManage,
    ProductPermission::DataRead,
    ProductPermission::DataWrite,
    ProductPermission::Discover,
    ProductPermission::Maintain,
    ProductPermission::Observe,
    ProductPermission::OwnershipManage,
    ProductPermission::ProofGenerate,
    ProductPermission::ProofVerify,
    ProductPermission::Restore,
    ProductPermission::SearchExecute,
    ProductPermission::SecurityManage,
    ProductPermission::SecurityRead,
];

fn parse_api_key(
    serialized: &str,
) -> Result<([u8; API_KEY_ID_BYTES], [u8; API_KEY_SECRET_BYTES]), AccessControlError> {
    let bytes = serialized.as_bytes();
    if bytes.len() != API_KEY_BYTES
        || !bytes.starts_with(API_KEY_PREFIX)
        || bytes[API_KEY_SEPARATOR_INDEX] != b'_'
    {
        return Err(AccessControlError::InvalidApiKey);
    }
    let id = decode_lower_hex::<API_KEY_ID_BYTES>(&bytes[5..API_KEY_SEPARATOR_INDEX])?;
    if id.iter().all(|byte| *byte == 0) {
        return Err(AccessControlError::InvalidApiKey);
    }
    let secret = decode_lower_hex::<API_KEY_SECRET_BYTES>(&bytes[API_KEY_SEPARATOR_INDEX + 1..])?;
    Ok((id, secret))
}

fn key_digest(id: &[u8; API_KEY_ID_BYTES], secret: &[u8; API_KEY_SECRET_BYTES]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(API_KEY_VERIFIER_DOMAIN);
    hasher.update(id);
    hasher.update(secret);
    *hasher.finalize().as_bytes()
}

fn decode_lower_hex<const N: usize>(input: &[u8]) -> Result<[u8; N], AccessControlError> {
    if input.len() != N * 2 || !input.iter().copied().all(is_lower_hex) {
        return Err(AccessControlError::InvalidApiKey);
    }
    let mut output = [0_u8; N];
    for (index, pair) in input.chunks_exact(2).enumerate() {
        output[index] = (decode_lower_nibble(pair[0]) << 4) | decode_lower_nibble(pair[1]);
    }
    Ok(output)
}

const fn is_lower_hex(value: u8) -> bool {
    matches!(value, b'0'..=b'9' | b'a'..=b'f')
}

const fn decode_lower_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => 0,
    }
}

fn push_lower_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

fn write_lower_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_KEY: &str = concat!(
        "hyp1_000102030405060708090a0b0c0d0e0f_",
        "101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f"
    );

    #[test]
    fn security_id_round_trips_canonical_lower_hex() -> Result<(), Box<dyn std::error::Error>> {
        let identity = SecurityId::new(42).ok_or(AccessControlError::InvalidIdentity)?;
        let encoded = identity.to_string();
        assert_eq!(encoded, "0000000000000000000000000000002a");
        assert_eq!(encoded.parse::<SecurityId>(), Ok(identity));
        assert_eq!(
            "00000000000000000000000000000000".parse::<SecurityId>(),
            Err(AccessControlError::InvalidIdentity)
        );
        assert_eq!(
            "0000000000000000000000000000002A".parse::<SecurityId>(),
            Err(AccessControlError::InvalidIdentity)
        );
        Ok(())
    }

    #[test]
    fn api_key_verifier_uses_exact_format_and_redacts_secrets()
    -> Result<(), Box<dyn std::error::Error>> {
        let (verifier, issued) = ApiKeyVerifier::import(FIXTURE_KEY)?;
        assert!(verifier.verifies(FIXTURE_KEY));
        assert_eq!(issued.expose_secret(), FIXTURE_KEY);
        assert!(!format!("{issued:?}").contains(&FIXTURE_KEY[38..]));
        assert!(!format!("{verifier:?}").contains("10111213"));

        let mut wrong = FIXTURE_KEY.to_owned();
        wrong.replace_range(101..102, "0");
        assert!(!verifier.verifies(&wrong));
        assert!(!verifier.verifies(&FIXTURE_KEY.to_uppercase()));
        assert!(!verifier.verifies(&format!(" {FIXTURE_KEY}")));
        assert!(!verifier.verifies(&format!("{FIXTURE_KEY}\n")));
        assert!(!verifier.verifies(&FIXTURE_KEY.replacen("hyp1_", "hyp2_", 1)));
        let zero_id = format!("hyp1_{}_{}", "0".repeat(32), "1".repeat(64));
        assert!(matches!(
            ApiKeyVerifier::import(&zero_id),
            Err(AccessControlError::InvalidApiKey)
        ));
        Ok(())
    }

    #[test]
    fn issued_key_is_canonical_and_self_verifying() -> Result<(), Box<dyn std::error::Error>> {
        let (verifier, issued) = ApiKeyVerifier::issue()?;
        let serialized = issued.expose_secret();
        assert_eq!(serialized.len(), API_KEY_BYTES);
        assert!(verifier.verifies(serialized));
        assert_eq!(verifier.id(), issued.id());
        Ok(())
    }

    #[test]
    fn built_in_roles_preserve_separation_of_duties() {
        let owner = BuiltInRole::Owner.authorization();
        let admin = BuiltInRole::Admin.authorization();
        let operator = BuiltInRole::Operator.authorization();
        let developer = BuiltInRole::Developer.authorization();
        let writer = BuiltInRole::Writer.authorization();
        let reader = BuiltInRole::Reader.authorization();
        let auditor = BuiltInRole::Auditor.authorization();

        assert!(owner.allows(ProductPermission::OwnershipManage));
        assert!(!admin.allows(ProductPermission::OwnershipManage));
        assert!(operator.allows(ProductPermission::Maintain));
        assert!(!operator.allows(ProductPermission::DataRead));
        assert!(developer.allows(ProductPermission::CatalogWrite));
        assert!(!developer.allows(ProductPermission::Maintain));
        assert!(writer.allows(ProductPermission::DataWrite));
        assert!(!writer.allows(ProductPermission::CatalogWrite));
        assert!(reader.allows(ProductPermission::SearchExecute));
        assert!(!reader.allows(ProductPermission::DataWrite));
        assert!(auditor.allows(ProductPermission::SecurityRead));
        assert!(!auditor.allows(ProductPermission::DataRead));
    }

    #[test]
    fn product_scope_uses_stable_object_identity() -> Result<(), Box<dyn std::error::Error>> {
        let root = ObjectId::new(7)?;
        let child = ObjectId::new(8)?;
        let foreign = ObjectId::new(9)?;
        let descendant = |candidate, ancestor| candidate == child && ancestor == root;

        assert!(ProductScope::Instance.covers_object(foreign, descendant));
        assert!(ProductScope::CatalogObject(root).covers_object(root, descendant));
        assert!(!ProductScope::CatalogObject(root).covers_object(child, descendant));
        assert!(ProductScope::CatalogSubtree(root).covers_object(root, descendant));
        assert!(ProductScope::CatalogSubtree(root).covers_object(child, descendant));
        assert!(!ProductScope::CatalogSubtree(root).covers_object(foreign, descendant));
        Ok(())
    }

    #[test]
    fn permission_names_are_exact_and_unknown_values_fail_closed() {
        for permission in [
            ProductPermission::AuditRead,
            ProductPermission::BackupCreate,
            ProductPermission::BackupVerify,
            ProductPermission::CatalogRead,
            ProductPermission::CatalogWrite,
            ProductPermission::CredentialSelfManage,
            ProductPermission::DataRead,
            ProductPermission::DataWrite,
            ProductPermission::Discover,
            ProductPermission::Maintain,
            ProductPermission::Observe,
            ProductPermission::OwnershipManage,
            ProductPermission::ProofGenerate,
            ProductPermission::ProofVerify,
            ProductPermission::Restore,
            ProductPermission::SearchExecute,
            ProductPermission::SecurityManage,
            ProductPermission::SecurityRead,
        ] {
            assert_eq!(
                ProductPermission::parse(permission.as_str()),
                Some(permission)
            );
        }
        assert_eq!(ProductPermission::parse("data.*"), None);
        assert_eq!(ProductPermission::parse("DATA.READ"), None);
    }
}
