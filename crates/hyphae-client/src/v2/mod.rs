// SPDX-License-Identifier: Apache-2.0

//! Equivalent high-level API over HTTP `/v2` and exact `HYPHLCL1` local transport.

mod client;
mod http;
mod local;

pub use client::*;
pub use http::*;
pub use local::*;

pub use hyphae_native_product::{
    AccessControlMutationReceipt, ApiKeyActivationReceipt, ApiKeyConfirmationDigest, ApiKeyId,
    ApiKeySecretDelivery, ApiKeyStartReceipt, BackupLimits, BoundedSearchQuery, BuiltInRole,
    CatalogDependencyRequest, CatalogListRequest, CatalogVisibleCursor, CatalogVisibleListFilter,
    CatalogVisibleListRequest, CatalogVisiblePage, CustomRoleGrant, CustomRoleMutationReceipt,
    DoctorRequest, ProductAuthorization, ProductCapabilities, ProductCommitOutcome,
    ProductDurabilityPolicy, ProductError, ProductErrorCategory, ProductErrorCode, ProductLimits,
    ProductOperation, ProductPermission, ProductPreparedHandle, ProductResponse, ProductRetry,
    ProductScope, ProductSearchRequest, ProductSqlResult, ProductTransactionId,
    ProductTransactionState, ProductTransactionStatus, ProductTtl, ProductValue,
    ProductVectorBranch, ProductVectorExecution, RoleAssignmentMutationReceipt,
    SecurityAssignmentListRequest, SecurityAuditReadRequest, SecurityCursor, SecurityCursorId,
    SecurityId, SecurityKeyListRequest, SecurityPrincipalListRequest,
    SecurityPrincipalMutationReceipt, SecurityRoleListRequest,
};
