// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded transport-independent logical catalog contracts.

use hyphae_native_catalog::{
    CatalogObjectKind, DependencyDirection, DependencyEdge, LogicalCatalogObject, QualifiedName,
};
use hyphae_native_runtime::{
    CatalogDependencyRequest as RuntimeDependencyRequest, CatalogListRequest as RuntimeListRequest,
    CatalogPageStop, NativeCatalogSnapshot, NativeRuntimeError,
};
use hyphae_native_types::ObjectId;

use crate::{NativeProduct, ProductError, ProductErrorCode, SnapshotIdentity};

/// Maximum summaries or edges returned by one product catalog request.
pub const MAX_PRODUCT_CATALOG_ITEMS: usize = hyphae_native_runtime::MAX_CATALOG_READ_ITEMS;
/// Maximum physical catalog entries visited by one product catalog request.
pub const MAX_PRODUCT_CATALOG_VISITS: usize = hyphae_native_runtime::MAX_CATALOG_READ_VISITS;
/// Maximum canonical output bytes returned by one product catalog request.
pub const MAX_PRODUCT_CATALOG_BYTES: usize = hyphae_native_runtime::MAX_CATALOG_READ_BYTES;

/// Opaque catalog pagination cursor bound to one immutable root identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogCursor {
    snapshot: SnapshotIdentity,
    after: ObjectId,
}

impl CatalogCursor {
    /// Reconstructs a cursor from a previously returned exact snapshot and
    /// exclusive stable-ID continuation.
    pub const fn new(snapshot: SnapshotIdentity, after: ObjectId) -> Self {
        Self { snapshot, after }
    }

    /// Returns the immutable snapshot identity required by this cursor.
    pub const fn snapshot(&self) -> SnapshotIdentity {
        self.snapshot
    }

    /// Returns the exclusive stable-ID continuation.
    pub const fn after(&self) -> ObjectId {
        self.after
    }
}

/// Bounded catalog object-list request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogListRequest {
    /// Optional hierarchy parent filter.
    pub parent: Option<ObjectId>,
    /// Optional stable object-kind filter.
    pub kind: Option<CatalogObjectKind>,
    /// Optional exclusive cursor from a prior page.
    pub cursor: Option<CatalogCursor>,
    /// Maximum returned summaries.
    pub item_limit: usize,
    /// Maximum physical object definitions visited.
    pub visit_limit: usize,
    /// Maximum canonical summary bytes returned.
    pub byte_limit: usize,
}

/// Bounded catalog dependency-list request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogDependencyRequest {
    /// Object whose dependency namespace is selected.
    pub object: ObjectId,
    /// Outgoing dependencies or incoming dependents.
    pub direction: DependencyDirection,
    /// Optional exclusive adjacent-object cursor from a prior page.
    pub cursor: Option<CatalogCursor>,
    /// Maximum returned edges.
    pub item_limit: usize,
    /// Maximum physical dependency entries visited.
    pub visit_limit: usize,
    /// Maximum canonical edge bytes returned.
    pub byte_limit: usize,
}

/// Lightweight object summary returned by bounded catalog listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogObjectSummary {
    /// Stable object identity.
    pub id: ObjectId,
    /// Stable logical object family.
    pub kind: CatalogObjectKind,
    /// Qualified display and lookup name.
    pub name: QualifiedName,
    /// Stable hierarchy parent.
    pub parent: Option<ObjectId>,
}

/// One bounded page with exact snapshot and work accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPage<T> {
    /// Immutable snapshot identity used by all returned items.
    pub snapshot: SnapshotIdentity,
    /// Returned summaries or edges.
    pub items: Vec<T>,
    /// Exclusive continuation bound to the same snapshot.
    pub cursor: Option<CatalogCursor>,
    /// Why traversal stopped.
    pub stop: CatalogPageStop,
    /// Physical namespace entries visited.
    pub visited: usize,
    /// Canonical output bytes returned.
    pub returned_bytes: usize,
}

/// Lightweight immutable catalog-only product snapshot.
#[derive(Clone, Debug)]
pub struct ProductCatalogSnapshot {
    identity: SnapshotIdentity,
    inner: NativeCatalogSnapshot,
}

impl ProductCatalogSnapshot {
    /// Returns the exact immutable snapshot identity.
    pub const fn identity(&self) -> SnapshotIdentity {
        self.identity
    }
}

impl NativeProduct {
    /// Captures one immutable catalog-only snapshot without materializing data
    /// from any engine.
    ///
    /// # Errors
    ///
    /// Returns a stable catalog or storage error when the current catalog root
    /// cannot be opened and validated.
    pub fn catalog_snapshot(&self) -> Result<ProductCatalogSnapshot, ProductError> {
        let inner = self.database.catalog_snapshot()?;
        let runtime = inner.identity();
        let identity = SnapshotIdentity {
            directory_lineage: self.database.directory_identity().lineage().encode(),
            visible_csn: runtime.visible_csn,
            catalog_version: runtime.catalog_version,
            root_digest: runtime.root_digest,
            logical_time_micros: 0,
        };
        Ok(ProductCatalogSnapshot { identity, inner })
    }

    /// Durably creates one generic logical catalog V2 object.
    ///
    /// # Errors
    ///
    /// Returns a stable request, conflict, I/O, or corruption error.
    pub fn create_catalog_object_v2(
        &mut self,
        object: LogicalCatalogObject,
        durability: crate::ProductDurability,
    ) -> Result<crate::ProductCommitReceipt, ProductError> {
        self.create_catalog_objects_v2(vec![object], durability)
    }

    /// Durably creates one ordered batch of generic logical catalog V2 objects.
    ///
    /// The batch is one atomic catalog commit. Parent objects must precede
    /// their dependents; an invalid object publishes none of the batch.
    ///
    /// # Errors
    ///
    /// Returns a stable request, limit, conflict, I/O, or corruption error.
    pub fn create_catalog_objects_v2(
        &mut self,
        objects: Vec<LogicalCatalogObject>,
        durability: crate::ProductDurability,
    ) -> Result<crate::ProductCommitReceipt, ProductError> {
        if objects.is_empty() {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        if objects.len() > crate::MAX_PRODUCT_TRANSACTION_OPERATIONS {
            return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
        }
        let receipt = self
            .database
            .create_catalog_objects_v2(objects, durability.into())
            .map_err(map_catalog_mutation_error)?;
        self.observe_commit(&receipt);
        Ok(receipt.into())
    }

    /// Returns one bounded page of stable-ID ordered logical V2 summaries.
    ///
    /// # Errors
    ///
    /// Returns a stable limit, cursor-conflict, unsupported-format, or
    /// corruption error.
    pub fn catalog_list(
        &self,
        snapshot: &ProductCatalogSnapshot,
        request: CatalogListRequest,
    ) -> Result<CatalogPage<CatalogObjectSummary>, ProductError> {
        validate_cursor(snapshot, request.cursor)?;
        let page = self
            .database
            .catalog_list(
                &snapshot.inner,
                RuntimeListRequest {
                    parent: request.parent,
                    kind: request.kind,
                    start_after: request.cursor.map(|cursor| cursor.after),
                    item_limit: request.item_limit,
                    visit_limit: request.visit_limit,
                    byte_limit: request.byte_limit,
                },
            )
            .map_err(map_catalog_error)?;
        Ok(CatalogPage {
            snapshot: snapshot.identity,
            items: page
                .items
                .into_iter()
                .map(|item| CatalogObjectSummary {
                    id: item.id,
                    kind: item.kind,
                    name: item.name,
                    parent: item.parent,
                })
                .collect(),
            cursor: page.continuation.map(|after| CatalogCursor {
                snapshot: snapshot.identity,
                after,
            }),
            stop: page.stop,
            visited: page.visited,
            returned_bytes: page.returned_bytes,
        })
    }

    /// Describes one complete logical V2 definition at an immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns a stable unsupported-format, I/O, or corruption error.
    pub fn catalog_describe(
        &self,
        snapshot: &ProductCatalogSnapshot,
        id: ObjectId,
    ) -> Result<Option<LogicalCatalogObject>, ProductError> {
        Ok(self
            .database
            .catalog_describe(&snapshot.inner, id)
            .map_err(map_catalog_error)?
            .object)
    }

    /// Resolves one qualified name at an immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns a stable unsupported-format, I/O, or corruption error.
    pub fn catalog_resolve(
        &self,
        snapshot: &ProductCatalogSnapshot,
        name: &QualifiedName,
    ) -> Result<Option<LogicalCatalogObject>, ProductError> {
        Ok(self
            .database
            .catalog_resolve(&snapshot.inner, name)
            .map_err(map_catalog_error)?
            .object)
    }

    /// Returns one bounded page of outgoing dependencies or incoming dependents.
    ///
    /// # Errors
    ///
    /// Returns a stable limit, cursor-conflict, unsupported-format, or
    /// corruption error.
    pub fn catalog_dependencies(
        &self,
        snapshot: &ProductCatalogSnapshot,
        request: CatalogDependencyRequest,
    ) -> Result<CatalogPage<DependencyEdge>, ProductError> {
        validate_cursor(snapshot, request.cursor)?;
        let page = self
            .database
            .catalog_dependencies(
                &snapshot.inner,
                RuntimeDependencyRequest {
                    object: request.object,
                    direction: request.direction,
                    start_after: request.cursor.map(|cursor| cursor.after),
                    item_limit: request.item_limit,
                    visit_limit: request.visit_limit,
                    byte_limit: request.byte_limit,
                },
            )
            .map_err(map_catalog_error)?;
        Ok(CatalogPage {
            snapshot: snapshot.identity,
            items: page.items,
            cursor: page.continuation.map(|after| CatalogCursor {
                snapshot: snapshot.identity,
                after,
            }),
            stop: page.stop,
            visited: page.visited,
            returned_bytes: page.returned_bytes,
        })
    }
}

fn validate_cursor(
    snapshot: &ProductCatalogSnapshot,
    cursor: Option<CatalogCursor>,
) -> Result<(), ProductError> {
    if cursor.is_some_and(|cursor| cursor.snapshot != snapshot.identity) {
        return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
    }
    Ok(())
}

fn map_catalog_error(error: NativeRuntimeError) -> ProductError {
    match error {
        NativeRuntimeError::InvalidCatalogReadLimit => {
            ProductError::from_code(ProductErrorCode::LimitExceeded)
        }
        NativeRuntimeError::CatalogV2Unavailable => {
            ProductError::from_code(ProductErrorCode::InvalidRequest)
        }
        NativeRuntimeError::CatalogSnapshotMismatch => {
            ProductError::from_code(ProductErrorCode::CatalogConflict)
        }
        other => other.into(),
    }
}

fn map_catalog_mutation_error(error: NativeRuntimeError) -> ProductError {
    match error {
        NativeRuntimeError::Catalog(_) | NativeRuntimeError::Model(_) => {
            ProductError::from_code(ProductErrorCode::InvalidRequest)
        }
        other => other.into(),
    }
}
