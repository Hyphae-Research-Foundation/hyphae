// SPDX-License-Identifier: Apache-2.0

//! Bounded transport-independent logical catalog contracts.

use hyphae_native_catalog::{
    CatalogObjectKind, DependencyDirection, DependencyEdge, LogicalCatalogObject, QualifiedName,
};
use hyphae_native_runtime::{
    CatalogDependencyRequest as RuntimeDependencyRequest, CatalogListRequest as RuntimeListRequest,
    CatalogPageStop, CatalogVisibleListRequest as RuntimeVisibleListRequest,
    CatalogVisibleScope as RuntimeVisibleScope, NativeCatalogSnapshot, NativeRuntimeError,
};
use hyphae_native_types::ObjectId;

use crate::{NativeProduct, ProductError, ProductErrorCode, SnapshotIdentity};

/// Maximum summaries or edges returned by one product catalog request.
pub const MAX_PRODUCT_CATALOG_ITEMS: usize = hyphae_native_runtime::MAX_CATALOG_READ_ITEMS;
/// Maximum physical catalog entries visited by one product catalog request.
pub const MAX_PRODUCT_CATALOG_VISITS: usize = hyphae_native_runtime::MAX_CATALOG_READ_VISITS;
/// Maximum canonical output bytes returned by one product catalog request.
pub const MAX_PRODUCT_CATALOG_BYTES: usize = hyphae_native_runtime::MAX_CATALOG_READ_BYTES;
/// Maximum canonical opaque cursor bytes accepted from public callers.
pub const MAX_CATALOG_VISIBLE_CURSOR_BYTES: usize = 256;

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

/// Opaque authenticated continuation for scope-visible catalog listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogVisibleCursor(Vec<u8>);

impl CatalogVisibleCursor {
    /// Constructs a bounded opaque cursor without parsing private fields.
    ///
    /// # Errors
    ///
    /// Returns `catalog_conflict` for empty input or input above the complete
    /// product request bound. Canonical cursor length and authentication are
    /// intentionally checked only at dispatch so every malformed token has the
    /// same public `catalog_conflict` result.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, ProductError> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_PRODUCT_CATALOG_BYTES {
            return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
        }
        Ok(Self(bytes))
    }

    /// Returns the complete opaque bytes for transport or later continuation.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn encoded_len(&self) -> usize {
        self.0.len()
    }
}

/// Scope-visible list filter bound into every opaque continuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogVisibleListFilter {
    /// Optional visible parent filter.
    pub parent: Option<ObjectId>,
    /// Optional stable object-kind filter.
    pub kind: Option<CatalogObjectKind>,
}

/// Bounded scope-visible list request introduced in protocol minor 3.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogVisibleListRequest {
    /// Filter cryptographically bound to the continuation.
    pub filter: CatalogVisibleListFilter,
    /// Opaque continuation returned by the preceding page.
    pub cursor: Option<CatalogVisibleCursor>,
    /// Maximum visible summaries to return.
    pub item_limit: usize,
    /// Maximum visible candidates to consider.
    pub visit_limit: usize,
    /// Maximum canonical summary bytes to return.
    pub byte_limit: usize,
}

/// Scope-visible page with no physical snapshot or traversal accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogVisiblePage {
    /// Visible summaries in stable `ObjectId` order.
    pub items: Vec<CatalogObjectSummary>,
    /// Opaque continuation, absent when authorized scopes are exhausted.
    pub cursor: Option<CatalogVisibleCursor>,
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

    /// Explicitly upgrades the logical catalog to the scope-index format.
    ///
    /// This is a strict mutating maintenance operation. Catalog reads never
    /// invoke it implicitly.
    ///
    /// # Errors
    ///
    /// Returns a storage, corruption, or durability error if migration fails.
    pub fn upgrade_catalog_scope_index(
        &mut self,
    ) -> Result<Option<crate::ProductCommitReceipt>, ProductError> {
        if let Some(receipt) = self
            .database
            .ensure_catalog_scope_index(hyphae_native_types::DurabilityClass::Strict)?
        {
            self.observe_commit(&receipt);
            return Ok(Some(receipt.into()));
        }
        Ok(None)
    }

    pub(crate) fn catalog_visible_list(
        &self,
        snapshot: &ProductCatalogSnapshot,
        scopes: &[crate::ProductScope],
        authority_key: [u8; 32],
        authorization_epoch: crate::AuthorizationEpoch,
        request: &CatalogVisibleListRequest,
    ) -> Result<CatalogVisiblePage, ProductError> {
        let runtime_scopes = canonical_visible_scopes(scopes);
        if runtime_scopes.is_empty() {
            return Err(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            ));
        }
        let filter_digest = visible_filter_digest(request.filter, &runtime_scopes);
        let start_after = request.cursor.as_ref().map_or(Ok(None), |cursor| {
            decode_visible_cursor(
                cursor,
                authority_key,
                authorization_epoch,
                snapshot.identity,
                filter_digest,
            )
            .map(Some)
        })?;
        let page = self
            .database
            .catalog_visible_list(
                &snapshot.inner,
                &RuntimeVisibleListRequest {
                    scopes: runtime_scopes,
                    parent: request.filter.parent,
                    kind: request.filter.kind,
                    start_after,
                    item_limit: request.item_limit,
                    visit_limit: request.visit_limit,
                    byte_limit: request.byte_limit,
                },
            )
            .map_err(map_catalog_error)?;
        let items = page
            .items
            .into_iter()
            .map(|item| CatalogObjectSummary {
                id: item.id,
                kind: item.kind,
                name: item.name,
                parent: item.parent,
            })
            .collect();
        let cursor = (!page.exhausted)
            .then_some(page.continuation)
            .flatten()
            .map(|after| {
                encode_visible_cursor(
                    authority_key,
                    authorization_epoch,
                    snapshot.identity,
                    filter_digest,
                    after,
                )
            })
            .transpose()?;
        Ok(CatalogVisiblePage { items, cursor })
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

fn canonical_visible_scopes(scopes: &[crate::ProductScope]) -> Vec<RuntimeVisibleScope> {
    let scopes = scopes
        .iter()
        .copied()
        .map(|scope| match scope {
            crate::ProductScope::Instance => RuntimeVisibleScope::Instance,
            crate::ProductScope::CatalogObject(object) => RuntimeVisibleScope::Object(object),
            crate::ProductScope::CatalogSubtree(object) => RuntimeVisibleScope::Subtree(object),
        })
        .collect::<std::collections::BTreeSet<_>>();
    if scopes.contains(&RuntimeVisibleScope::Instance) {
        return vec![RuntimeVisibleScope::Instance];
    }
    scopes.iter().copied().collect()
}

fn visible_filter_digest(
    filter: CatalogVisibleListFilter,
    scopes: &[RuntimeVisibleScope],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-catalog-visible-filter-v1\0");
    match filter.parent {
        Some(parent) => {
            hasher.update(&[1]);
            hasher.update(&parent.get().to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.update(&[filter.kind.map_or(0, |kind| kind as u8)]);
    for scope in scopes {
        match scope {
            RuntimeVisibleScope::Instance => {
                hasher.update(&[0]);
            }
            RuntimeVisibleScope::Object(object) => {
                hasher.update(&[1]);
                hasher.update(&object.get().to_le_bytes());
            }
            RuntimeVisibleScope::Subtree(object) => {
                hasher.update(&[2]);
                hasher.update(&object.get().to_le_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
}

const VISIBLE_CURSOR_MAGIC: &[u8; 8] = b"HYCVIS01";
const VISIBLE_CURSOR_CONTENT_BYTES: usize = 144;
const VISIBLE_CURSOR_BYTES: usize = VISIBLE_CURSOR_CONTENT_BYTES + 32;

fn encode_visible_cursor(
    key: [u8; 32],
    epoch: crate::AuthorizationEpoch,
    snapshot: SnapshotIdentity,
    filter_digest: [u8; 32],
    after: ObjectId,
) -> Result<CatalogVisibleCursor, ProductError> {
    let mut bytes = Vec::with_capacity(VISIBLE_CURSOR_BYTES);
    bytes.extend_from_slice(VISIBLE_CURSOR_MAGIC);
    bytes.extend_from_slice(&epoch.get().to_le_bytes());
    bytes.extend_from_slice(&snapshot.directory_lineage);
    bytes.extend_from_slice(
        &snapshot
            .visible_csn
            .map_or(0, hyphae_native_types::Csn::get)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&snapshot.catalog_version.get().to_le_bytes());
    bytes.extend_from_slice(&snapshot.root_digest);
    bytes.extend_from_slice(&filter_digest);
    bytes.extend_from_slice(&after.get().to_le_bytes());
    bytes.extend_from_slice(&[1; 8]);
    let mac = blake3::keyed_hash(&key, &bytes);
    bytes.extend_from_slice(mac.as_bytes());
    CatalogVisibleCursor::new(bytes)
}

fn decode_visible_cursor(
    cursor: &CatalogVisibleCursor,
    key: [u8; 32],
    epoch: crate::AuthorizationEpoch,
    snapshot: SnapshotIdentity,
    filter_digest: [u8; 32],
) -> Result<ObjectId, ProductError> {
    let bytes = cursor.as_bytes();
    let conflict = || ProductError::from_code(ProductErrorCode::CatalogConflict);
    if bytes.len() != VISIBLE_CURSOR_BYTES {
        return Err(conflict());
    }
    if &bytes[..8] != VISIBLE_CURSOR_MAGIC
        || bytes[136..144] != [1; 8]
        || blake3::keyed_hash(&key, &bytes[..VISIBLE_CURSOR_CONTENT_BYTES]).as_bytes()
            != &bytes[VISIBLE_CURSOR_CONTENT_BYTES..]
        || u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| conflict())?) != epoch.get()
        || bytes[16..40] != snapshot.directory_lineage
        || u64::from_le_bytes(bytes[40..48].try_into().map_err(|_| conflict())?)
            != snapshot
                .visible_csn
                .map_or(0, hyphae_native_types::Csn::get)
        || u64::from_le_bytes(bytes[48..56].try_into().map_err(|_| conflict())?)
            != snapshot.catalog_version.get()
        || bytes[56..88] != snapshot.root_digest
        || bytes[88..120] != filter_digest
    {
        return Err(conflict());
    }
    ObjectId::new(u128::from_le_bytes(
        bytes[120..136].try_into().map_err(|_| conflict())?,
    ))
    .map_err(|_| conflict())
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
