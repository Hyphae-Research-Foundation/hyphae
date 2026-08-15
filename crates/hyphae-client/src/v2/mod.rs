// SPDX-License-Identifier: AGPL-3.0-only

//! Equivalent high-level API over HTTP `/v2` and exact `HYPHLCL1` local transport.

mod client;
mod http;
mod local;

pub use client::*;
pub use http::*;
pub use local::*;

pub use hyphae_native_product::{
    AccessControlMutationReceipt, BackupLimits, BoundedSearchQuery, BuiltInRole,
    CatalogDependencyRequest, CatalogListRequest, CustomRoleGrant, CustomRoleMutationReceipt,
    DoctorRequest, ProductCapabilities, ProductCommitOutcome, ProductDurabilityPolicy,
    ProductError, ProductErrorCategory, ProductErrorCode, ProductLimits, ProductOperation,
    ProductPermission, ProductPreparedHandle, ProductResponse, ProductRetry, ProductScope,
    ProductSearchRequest, ProductSqlResult, ProductTransactionId, ProductTransactionState,
    ProductTransactionStatus, ProductTtl, ProductValue, ProductVectorBranch,
    ProductVectorExecution, RoleAssignmentMutationReceipt, SecurityAssignmentListRequest,
    SecurityAuditReadRequest, SecurityCursor, SecurityCursorId, SecurityId, SecurityKeyListRequest,
    SecurityPrincipalListRequest, SecurityPrincipalMutationReceipt, SecurityRoleListRequest,
};
