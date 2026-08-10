// SPDX-License-Identifier: GPL-3.0-only

//! Equivalent high-level API over HTTP `/v2` and exact `HYPHLCL1` local transport.

mod client;
mod http;
mod local;

pub use client::*;
pub use http::*;
pub use local::*;

pub use hyphae_native_product::{
    BackupLimits, BoundedSearchQuery, CatalogDependencyRequest, CatalogListRequest, DoctorRequest,
    ProductCapabilities, ProductCommitOutcome, ProductDurabilityPolicy, ProductError,
    ProductErrorCategory, ProductErrorCode, ProductLimits, ProductOperation, ProductPreparedHandle,
    ProductResponse, ProductRetry, ProductSearchRequest, ProductSqlResult, ProductTransactionId,
    ProductTransactionState, ProductTransactionStatus, ProductTtl, ProductValue,
    ProductVectorBranch, ProductVectorExecution,
};
