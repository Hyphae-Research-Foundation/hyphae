// SPDX-License-Identifier: AGPL-3.0-only

//! Product principals, authorization, sessions, and prepared handles.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, RwLock},
};

use hyphae_native_runtime::NativeWriteBatch;
use hyphae_native_types::TransactionId;

use crate::{
    AuthenticatedAuthority, AuthorizationEpoch, ProductCommitReceipt, ProductDurability,
    ProductError, ProductErrorCode, ProductExplicitTransactionStatus, ProductPreparedStatement,
    ProductTransactionHandle,
};

/// Maximum UTF-8 bytes in one product principal identity.
pub const MAX_PRODUCT_PRINCIPAL_BYTES: usize = 256;
/// Default retained prepared plans in one product session.
pub const DEFAULT_PRODUCT_PREPARED_HANDLES: usize = 128;
/// Default retained commit outcomes in one product session.
pub const DEFAULT_PRODUCT_TRANSACTION_STATUSES: usize = 1_024;
/// Default simultaneous explicit all-engine transactions in one session.
pub const DEFAULT_PRODUCT_ACTIVE_TRANSACTIONS: usize = 16;

#[derive(Debug)]
pub(crate) struct ActiveProductTransaction {
    pub(crate) batch: NativeWriteBatch,
    pub(crate) staged_operations: usize,
    pub(crate) durability: ProductDurability,
}

/// Stable identity for one process-local product session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductSessionId(u128);

impl ProductSessionId {
    /// Constructs a nonzero session identity.
    pub const fn new(value: u128) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the primitive session identity.
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Authenticated caller identity retained without transport-specific claims.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductPrincipal {
    identity: Box<str>,
}

impl ProductPrincipal {
    /// Constructs a bounded nonempty principal identity.
    pub fn new(identity: impl Into<Box<str>>) -> Option<Self> {
        let identity = identity.into();
        (!identity.is_empty() && identity.len() <= MAX_PRODUCT_PRINCIPAL_BYTES)
            .then_some(Self { identity })
    }

    /// Returns the exact authenticated identity.
    pub fn identity(&self) -> &str {
        &self.identity
    }
}

/// Product permission checked before operation execution.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ProductPermission {
    /// Read bounded durable security events.
    AuditRead = 0,
    /// Create and verify a new backup.
    BackupCreate = 1,
    /// Verify a backup without activating it.
    BackupVerify = 2,
    /// Catalog reads.
    CatalogRead = 3,
    /// Catalog mutation.
    CatalogWrite = 4,
    /// Create, rotate, or revoke the caller's narrowed credentials.
    CredentialSelfManage = 5,
    /// SQL and structure reads.
    DataRead = 6,
    /// SQL, structure, and search-data mutation.
    DataWrite = 7,
    /// Capability discovery.
    Discover = 8,
    /// Checkpoint, doctor, compaction, vacuum, and retention operations.
    Maintain = 9,
    /// Status, telemetry, and bounded explain observation.
    Observe = 10,
    /// Ownership transfer and recovery policy.
    OwnershipManage = 11,
    /// Generate a proof for an otherwise-authorized read.
    ProofGenerate = 12,
    /// Offline proof verification.
    ProofVerify = 13,
    /// Restore a verified backup into a new directory.
    Restore = 14,
    /// Lexical, vector, ANN, and hybrid search execution.
    SearchExecute = 15,
    /// Mutate principals, roles, assignments, and credentials.
    SecurityManage = 16,
    /// Read redacted security metadata.
    SecurityRead = 17,
}

/// Closed permission set supplied by the authentication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductAuthorization(u64);

impl ProductAuthorization {
    /// No permissions.
    pub const NONE: Self = Self(0);
    /// Every permission known to this product version.
    pub const ALL: Self = Self((1 << 18) - 1);
    /// Discovery plus ordinary application read, search, and proof operations.
    pub const READ_ONLY: Self = Self(
        (1 << ProductPermission::Discover as u8)
            | (1 << ProductPermission::CatalogRead as u8)
            | (1 << ProductPermission::DataRead as u8)
            | (1 << ProductPermission::SearchExecute as u8)
            | (1 << ProductPermission::ProofGenerate as u8)
            | (1 << ProductPermission::ProofVerify as u8),
    );

    /// Builds an authorization set from explicit permissions.
    pub fn from_permissions(permissions: impl IntoIterator<Item = ProductPermission>) -> Self {
        let mut bits = 0_u64;
        for permission in permissions {
            bits |= 1_u64 << permission as u8;
        }
        Self(bits)
    }

    /// Returns whether the permission is granted.
    pub const fn allows(self, permission: ProductPermission) -> bool {
        self.0 & (1_u64 << permission as u8) != 0
    }

    /// Returns whether every permission in `required` is granted.
    pub const fn allows_all(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Combines two additive permission sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn bits(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_bits(bits: u64) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub(crate) const fn from_known_bits(bits: u64) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

/// Opaque session-local prepared SQL handle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductPreparedHandle(u64);

impl ProductPreparedHandle {
    /// Constructs a nonzero session-local prepared handle from its wire value.
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the primitive process-local handle.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Opaque non-guessable identity used to resolve one durable mutation outcome.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductTransactionId(TransactionId);

impl ProductTransactionId {
    /// Constructs a nonzero resolution identity from its wire value.
    pub fn new(value: u128) -> Option<Self> {
        TransactionId::new(value).ok().map(Self)
    }

    /// Returns the primitive wire value.
    pub const fn get(self) -> u128 {
        self.0.get()
    }

    /// Returns the canonical little-endian wire representation.
    pub const fn to_le_bytes(self) -> [u8; 16] {
        self.get().to_le_bytes()
    }

    pub(crate) const fn native(self) -> TransactionId {
        self.0
    }
}

impl From<TransactionId> for ProductTransactionId {
    fn from(value: TransactionId) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ProductTransactionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(formatter)
    }
}

/// Retained transaction outcome used by status resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductTransactionStatus {
    /// The session has no evidence for this identity.
    Unknown,
    /// Commit and its durability acknowledgement are proven.
    Committed(ProductCommitReceipt),
    /// Private mutation was discarded before publication.
    RolledBack {
        /// Product resolution identity.
        transaction_id: ProductTransactionId,
    },
    /// Publication may have completed and must be resolved after reopen.
    OutcomeUnknown {
        /// Resolution identity also attached to the product error.
        transaction_id: ProductTransactionId,
    },
}

/// Direct or service-owned state for prepared plans and commit evidence.
#[derive(Debug)]
pub struct ProductSession {
    id: ProductSessionId,
    principal: ProductPrincipal,
    authorization: ProductAuthorization,
    authorization_epoch: AuthorizationEpoch,
    authority: ProductSessionAuthority,
    prepared: BTreeMap<ProductPreparedHandle, ProductPreparedStatement>,
    next_prepared: u64,
    maximum_prepared: usize,
    transactions: BTreeMap<ProductTransactionId, ProductTransactionStatus>,
    transaction_order: VecDeque<ProductTransactionId>,
    maximum_transactions: usize,
    active_transactions: BTreeMap<ProductTransactionHandle, ActiveProductTransaction>,
    explicit_statuses: BTreeMap<ProductTransactionHandle, ProductExplicitTransactionStatus>,
    explicit_status_order: VecDeque<ProductTransactionHandle>,
    next_transaction_handle: u64,
    maximum_active_transactions: usize,
}

#[derive(Debug)]
enum ProductSessionAuthority {
    Unmanaged,
    Managed(RwLock<Arc<AuthenticatedAuthority>>),
}

impl ProductSession {
    /// Creates one explicitly unmanaged embedded session with default bounds.
    ///
    /// The caller is the trusted local authority. Remote adapters must use an
    /// authenticated session and must not derive permissions from peer input.
    pub fn new(
        id: ProductSessionId,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
    ) -> Self {
        Self::new_at_epoch(id, principal, authorization, AuthorizationEpoch::UNMANAGED)
    }

    /// Creates one explicitly unmanaged embedded session at a caller epoch.
    pub fn new_at_epoch(
        id: ProductSessionId,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
        authorization_epoch: AuthorizationEpoch,
    ) -> Self {
        Self::with_limits(
            id,
            principal,
            authorization,
            authorization_epoch,
            DEFAULT_PRODUCT_PREPARED_HANDLES,
            DEFAULT_PRODUCT_TRANSACTION_STATUSES,
            DEFAULT_PRODUCT_ACTIVE_TRANSACTIONS,
        )
    }

    /// Creates one managed embedded session from an unforgeable authority.
    ///
    /// Every dispatched operation revalidates the authority against the
    /// current durable catalog and trusted wall clock.
    pub fn new_authenticated(id: ProductSessionId, authority: AuthenticatedAuthority) -> Self {
        Self::with_authenticated_limits(
            id,
            authority,
            DEFAULT_PRODUCT_PREPARED_HANDLES,
            DEFAULT_PRODUCT_TRANSACTION_STATUSES,
            DEFAULT_PRODUCT_ACTIVE_TRANSACTIONS,
        )
    }

    pub(crate) fn with_limits(
        id: ProductSessionId,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
        authorization_epoch: AuthorizationEpoch,
        maximum_prepared: usize,
        maximum_transactions: usize,
        maximum_active_transactions: usize,
    ) -> Self {
        Self::with_limits_and_authority(
            id,
            principal,
            authorization,
            authorization_epoch,
            None,
            maximum_prepared,
            maximum_transactions,
            maximum_active_transactions,
        )
    }

    pub(crate) fn with_authenticated_limits(
        id: ProductSessionId,
        authority: AuthenticatedAuthority,
        maximum_prepared: usize,
        maximum_transactions: usize,
        maximum_active_transactions: usize,
    ) -> Self {
        Self::with_limits_and_authority(
            id,
            authority.principal().clone(),
            authority.authorization(),
            authority.authorization_epoch(),
            Some(authority),
            maximum_prepared,
            maximum_transactions,
            maximum_active_transactions,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one constructor centralizes every bounded session field"
    )]
    fn with_limits_and_authority(
        id: ProductSessionId,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
        authorization_epoch: AuthorizationEpoch,
        authenticated_authority: Option<AuthenticatedAuthority>,
        maximum_prepared: usize,
        maximum_transactions: usize,
        maximum_active_transactions: usize,
    ) -> Self {
        Self {
            id,
            principal,
            authorization,
            authorization_epoch,
            authority: authenticated_authority
                .map_or(ProductSessionAuthority::Unmanaged, |value| {
                    ProductSessionAuthority::Managed(RwLock::new(Arc::new(value)))
                }),
            prepared: BTreeMap::new(),
            next_prepared: 1,
            maximum_prepared,
            transactions: BTreeMap::new(),
            transaction_order: VecDeque::new(),
            maximum_transactions,
            active_transactions: BTreeMap::new(),
            explicit_statuses: BTreeMap::new(),
            explicit_status_order: VecDeque::new(),
            next_transaction_handle: 1,
            maximum_active_transactions,
        }
    }

    /// Returns this session's identity.
    pub const fn id(&self) -> ProductSessionId {
        self.id
    }

    /// Returns the authenticated principal bound at session creation.
    pub const fn principal(&self) -> &ProductPrincipal {
        &self.principal
    }

    /// Returns immutable authorization bound at session creation.
    pub const fn authorization(&self) -> ProductAuthorization {
        self.authorization
    }

    /// Returns the durable authorization generation bound at authentication.
    pub const fn authorization_epoch(&self) -> AuthorizationEpoch {
        self.authorization_epoch
    }

    pub(crate) fn authenticated_authority(
        &self,
    ) -> Result<Option<Arc<AuthenticatedAuthority>>, ProductError> {
        match &self.authority {
            ProductSessionAuthority::Unmanaged => Ok(None),
            ProductSessionAuthority::Managed(authority) => authority
                .read()
                .map(|authority| Some(Arc::clone(&authority)))
                .map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable)),
        }
    }

    pub(crate) fn refresh_authenticated_authority(
        &self,
        authority: Arc<AuthenticatedAuthority>,
    ) -> Result<(), ProductError> {
        match &self.authority {
            ProductSessionAuthority::Unmanaged => {
                Err(ProductError::from_code(ProductErrorCode::Internal))
            }
            ProductSessionAuthority::Managed(cached) => {
                *cached
                    .write()
                    .map_err(|_| ProductError::from_code(ProductErrorCode::Unavailable))? =
                    authority;
                Ok(())
            }
        }
    }

    pub(crate) fn is_managed(&self) -> bool {
        matches!(self.authority, ProductSessionAuthority::Managed(_))
    }

    pub(crate) fn retain_prepared(
        &mut self,
        prepared: ProductPreparedStatement,
    ) -> Option<ProductPreparedHandle> {
        if self.prepared.len() >= self.maximum_prepared || self.next_prepared == u64::MAX {
            return None;
        }
        let handle = ProductPreparedHandle(self.next_prepared);
        self.next_prepared += 1;
        self.prepared.insert(handle, prepared);
        Some(handle)
    }

    pub(crate) fn prepared(
        &self,
        handle: ProductPreparedHandle,
    ) -> Option<&ProductPreparedStatement> {
        self.prepared.get(&handle)
    }

    pub(crate) fn deallocate(&mut self, handle: ProductPreparedHandle) -> bool {
        self.prepared.remove(&handle).is_some()
    }

    pub(crate) fn record_transaction(
        &mut self,
        id: ProductTransactionId,
        status: ProductTransactionStatus,
    ) {
        if self.maximum_transactions == 0 {
            return;
        }
        if !self.transactions.contains_key(&id) {
            while self.transactions.len() >= self.maximum_transactions {
                if let Some(expired) = self.transaction_order.pop_front() {
                    self.transactions.remove(&expired);
                }
            }
            self.transaction_order.push_back(id);
        }
        self.transactions.insert(id, status);
    }

    pub(crate) fn transaction_status(&self, id: ProductTransactionId) -> ProductTransactionStatus {
        self.transactions
            .get(&id)
            .copied()
            .unwrap_or(ProductTransactionStatus::Unknown)
    }

    pub(crate) fn begin_transaction(
        &mut self,
        batch: NativeWriteBatch,
        durability: ProductDurability,
    ) -> Option<ProductExplicitTransactionStatus> {
        if self.active_transactions.len() >= self.maximum_active_transactions
            || self.next_transaction_handle == u64::MAX
        {
            return None;
        }
        let handle = ProductTransactionHandle::new(self.next_transaction_handle)?;
        self.next_transaction_handle += 1;
        let status = ProductExplicitTransactionStatus::Active {
            handle,
            read_csn: batch.read_csn().map(hyphae_native_types::Csn::get),
            staged_operations: 0,
            durability,
        };
        self.active_transactions.insert(
            handle,
            ActiveProductTransaction {
                batch,
                staged_operations: 0,
                durability,
            },
        );
        self.record_explicit_status(handle, status);
        Some(status)
    }

    pub(crate) fn active_transaction(
        &self,
        handle: ProductTransactionHandle,
    ) -> Option<&ActiveProductTransaction> {
        self.active_transactions.get(&handle)
    }

    pub(crate) fn has_active_transactions(&self) -> bool {
        !self.active_transactions.is_empty()
    }

    pub(crate) fn replace_active_transaction(
        &mut self,
        handle: ProductTransactionHandle,
        transaction: ActiveProductTransaction,
    ) {
        let status = ProductExplicitTransactionStatus::Active {
            handle,
            read_csn: transaction
                .batch
                .read_csn()
                .map(hyphae_native_types::Csn::get),
            staged_operations: transaction.staged_operations,
            durability: transaction.durability,
        };
        self.active_transactions.insert(handle, transaction);
        self.record_explicit_status(handle, status);
    }

    pub(crate) fn take_active_transaction(
        &mut self,
        handle: ProductTransactionHandle,
    ) -> Option<ActiveProductTransaction> {
        self.active_transactions.remove(&handle)
    }

    pub(crate) fn rollback_active_transaction_after_authority_loss(
        &mut self,
        handle: ProductTransactionHandle,
    ) {
        let Some(transaction) = self.active_transactions.remove(&handle) else {
            return;
        };
        let discarded_operations = transaction.staged_operations;
        transaction.batch.rollback();
        self.record_explicit_status(
            handle,
            ProductExplicitTransactionStatus::RolledBack {
                handle,
                discarded_operations,
            },
        );
    }

    pub(crate) fn record_explicit_status(
        &mut self,
        handle: ProductTransactionHandle,
        status: ProductExplicitTransactionStatus,
    ) {
        if !self.explicit_statuses.contains_key(&handle) {
            while self.explicit_statuses.len() >= self.maximum_transactions {
                if let Some(expired) = self.explicit_status_order.pop_front() {
                    if self.active_transactions.contains_key(&expired) {
                        self.explicit_status_order.push_back(expired);
                        break;
                    }
                    self.explicit_statuses.remove(&expired);
                }
            }
            self.explicit_status_order.push_back(handle);
        }
        self.explicit_statuses.insert(handle, status);
    }

    pub(crate) fn explicit_transaction_status(
        &self,
        handle: ProductTransactionHandle,
    ) -> ProductExplicitTransactionStatus {
        self.explicit_statuses
            .get(&handle)
            .copied()
            .unwrap_or(ProductExplicitTransactionStatus::Unknown)
    }
}
