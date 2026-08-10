// SPDX-License-Identifier: GPL-3.0-only

//! Bounded, catalog-bound integrated native search.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hyphae_native_catalog::{
    CatalogObjectV2, LexicalIndexPolicy, LogicalCatalogObject, SearchCollectionDefinitionV2,
    VectorMetric as CatalogVectorMetric, VectorSearchPolicy,
};
use hyphae_native_runtime::{
    AnnSearchOptions, HnswConfig, NativeRuntimeError, VectorHit,
    VectorMetric as RuntimeVectorMetric, execute_doc_values,
};
use hyphae_native_types::{LogicalType, TransactionId};

use crate::{
    NativeProduct, ProductCommitReceipt, ProductDurability, ProductError, ProductErrorCode,
    ProductSnapshot, SnapshotIdentity,
};

pub use hyphae_native_runtime::{
    DocValue as ProductDocValue, DocValueAggregation as ProductAggregation,
    DocValueAggregationValue as ProductAggregationValue, DocValueFilter as ProductSearchFilter,
    DocValueOperator as ProductSearchOperator, DocValueSort as ProductSearchSort,
    DocValueSortDirection as ProductSortDirection, DocValueSortSource as ProductSortSource,
    FacetBucket as ProductFacetBucket, FacetRequest as ProductFacetRequest,
    FacetResult as ProductFacetResult, MissingPlacement as ProductMissingPlacement,
    NamedDocValueAggregation as ProductNamedAggregation,
    NamedDocValueAggregationValue as ProductNamedAggregationValue, Vector as ProductVector,
};

const MANIFEST_MAGIC: &[u8; 8] = b"HYPSMAN1";
const BINDING_MAGIC: &[u8; 8] = b"HYPSBND1";
const DOCUMENT_MAGIC: &[u8; 8] = b"HYPSDOC1";
const IDEMPOTENCY_MAGIC: &[u8; 8] = b"HYPSIDM1";
const STORAGE_PREFIX: &[u8] = b"\0hyphae.product.search.v1\0";
const RRF_CONSTANT: f64 = 60.0;

/// Maximum documents accepted by one atomic integrated ingestion.
pub const MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS: usize = 256;
/// Maximum logical input bytes accepted by one atomic integrated ingestion.
pub const MAX_PRODUCT_SEARCH_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// Maximum durable documents admitted by one product collection manifest.
pub const MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS: usize = 10_000;
/// Maximum named vector targets in one collection or request.
pub const MAX_PRODUCT_SEARCH_VECTOR_TARGETS: usize = 16;
/// Maximum retrieval candidates requested from one native branch.
pub const MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES: usize = 10_000;
/// Maximum result hits returned by the integrated surface.
pub const MAX_PRODUCT_SEARCH_HITS: usize = hyphae_native_runtime::MAX_DOC_VALUE_HITS;

/// One physical named-vector target owned by a logical Catalog V2 collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductNamedVectorBinding {
    /// Normalized Catalog V2 vector name.
    pub name: String,
    /// Native vector-index object receiving this target's vectors.
    pub index: crate::ObjectId,
}

/// Durable physical binding for one logical Catalog V2 search collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSearchCollectionBinding {
    /// Logical `SearchCollectionDefinitionV2` object.
    pub collection: crate::ObjectId,
    /// Native lexical object receiving the collection's source text.
    pub lexical_index: crate::ObjectId,
    /// Complete named-vector mapping in canonical name order.
    pub vectors: Vec<ProductNamedVectorBinding>,
}

/// One complete product document staged atomically across native engines.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductDocument {
    /// Stable identity shared by lexical and every vector target.
    pub object_id: crate::ObjectId,
    /// Canonical source text indexed by native BM25.
    pub text: String,
    /// Typed filter/sort/facet/aggregation values by normalized field name.
    pub doc_values: BTreeMap<String, ProductDocValue>,
    /// Named vectors by normalized Catalog V2 vector name.
    pub vectors: BTreeMap<String, ProductVector>,
}

/// One bounded, idempotent atomic ingestion request.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSearchIngestBatch {
    /// Stable caller identity for retry suppression.
    pub idempotency_id: u128,
    /// Documents committed together or not at all.
    pub documents: Vec<ProductDocument>,
}

/// Durable result of one integrated ingestion attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSearchIngestReceipt {
    /// All-engine snapshot containing the accepted batch.
    pub snapshot: SnapshotIdentity,
    /// Original commit evidence. Replays preserve the receipt available in the
    /// durable marker rather than describing the retry attempt.
    pub commit: Option<ProductCommitReceipt>,
    /// Number of documents represented by the idempotency record.
    pub documents: usize,
    /// Whether an existing durable idempotency record suppressed publication.
    pub idempotent_replay: bool,
}

/// One idempotent integrated document replacement.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSearchDocumentUpdate {
    /// Stable retry identity.
    pub idempotency_id: u128,
    /// Complete replacement document.
    pub document: ProductDocument,
}

/// One idempotent integrated document deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductSearchDocumentDelete {
    /// Stable retry identity.
    pub idempotency_id: u128,
    /// Existing document identity to remove from every branch.
    pub object_id: crate::ObjectId,
}

/// One native BM25 branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductLexicalBranch {
    /// Analyzer input.
    pub query: String,
    /// Maximum lexical candidates admitted to fusion.
    pub candidate_limit: usize,
    /// Positive deterministic RRF weight.
    pub weight: u32,
}

/// Exact, approximate, or adaptive vector execution requested by the product.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductVectorExecution {
    /// Complete exact distance ranking over the filtered set.
    Exact,
    /// Filter-aware ANN with an exact filtered seed, avoiding post-filter-only execution.
    Ann {
        /// HNSW traversal breadth.
        ef_search: usize,
        /// Optional exact candidate rerank count.
        exact_rerank: Option<usize>,
    },
    /// Exact below the eligible threshold and filter-aware ANN above it.
    Adaptive {
        /// Inclusive eligible-document threshold for exact execution.
        exact_candidate_threshold: usize,
        /// HNSW traversal breadth above the threshold.
        ef_search: usize,
        /// Optional exact candidate rerank count.
        exact_rerank: Option<usize>,
    },
}

/// One named vector retrieval branch.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductVectorBranch {
    /// Catalog V2 named-vector target.
    pub target: String,
    /// Canonical query vector.
    pub query: ProductVector,
    /// Maximum branch candidates admitted to fusion.
    pub candidate_limit: usize,
    /// Positive deterministic RRF weight.
    pub weight: u32,
    /// Optional execution narrowing. `None` executes the catalog-owned policy.
    /// A supplied strategy must agree with the catalog policy and cannot alter
    /// its adaptive threshold or exceed its `ef_search_max`.
    pub execution: Option<ProductVectorExecution>,
}

/// Complete bounded integrated search request.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSearchRequest {
    /// Optional native BM25 branch.
    pub lexical: Option<ProductLexicalBranch>,
    /// Zero or more named vector branches. Repeated targets are rejected.
    pub vectors: Vec<ProductVectorBranch>,
    /// Typed predicate evaluated before every vector strategy.
    pub filter: ProductSearchFilter,
    /// Explicit final sort. Empty means fused relevance descending.
    pub sort: Vec<ProductSearchSort>,
    /// Complete candidate-set facets.
    pub facets: Vec<ProductFacetRequest>,
    /// Complete candidate-set metric aggregations.
    pub aggregations: Vec<ProductNamedAggregation>,
    /// Maximum final hits.
    pub limit: usize,
}

/// Physical vector strategy that actually ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductVectorStrategy {
    /// Caller-selected complete exact filtered oracle.
    ExactFiltered,
    /// Adaptive policy selected the complete exact filtered oracle.
    AdaptiveExactFiltered,
    /// Caller-selected filtered ANN augmented by an exact filtered seed.
    FilterAwareAnn,
    /// Adaptive policy selected filtered ANN augmented by an exact filtered seed.
    AdaptiveFilterAwareAnn,
}

/// Per-target vector execution evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductVectorBranchReceipt {
    /// Named vector target.
    pub target: String,
    /// Physical strategy that ran.
    pub strategy: ProductVectorStrategy,
    /// Whether results remain approximate.
    pub approximate: bool,
    /// Documents admitted by the typed filter.
    pub eligible_documents: usize,
    /// Native candidates observed before fusion.
    pub candidate_count: usize,
    /// Distinct graph nodes evaluated, zero for exact execution.
    pub visited_nodes: usize,
    /// Whether native reranking or the exact filtered seed ran.
    pub exact_reranked: bool,
}

/// One final integrated hit.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductIntegratedSearchHit {
    /// Stable document identity.
    pub object_id: crate::ObjectId,
    /// Nonnegative deterministic fused relevance score.
    pub score: f64,
    /// Persisted typed values used by filtering and sorting.
    pub doc_values: BTreeMap<String, ProductDocValue>,
}

/// Complete integrated result with snapshot, strategy, approximation, and counts.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSearchResult {
    /// Exact immutable all-engine identity shared by every branch and side record.
    pub snapshot: SnapshotIdentity,
    /// Globally sorted, bounded hits.
    pub hits: Vec<ProductIntegratedSearchHit>,
    /// Facets over the complete fused candidate set after the typed filter.
    pub facets: Vec<ProductFacetResult>,
    /// Named metric aggregations over the same set.
    pub aggregations: Vec<ProductNamedAggregationValue>,
    /// Per-target vector strategy evidence.
    pub vector_branches: Vec<ProductVectorBranchReceipt>,
    /// Whether any final branch remains approximate.
    pub approximate: bool,
    /// Durable documents present in the collection snapshot.
    pub total_documents: usize,
    /// Documents admitted before vector strategy selection.
    pub eligible_documents: usize,
    /// BM25 candidates admitted to fusion.
    pub lexical_candidates: usize,
    /// Unique fused candidates inspected by final doc-value execution.
    pub retrieval_candidates: usize,
    /// Candidates matching the final typed predicate before result limiting.
    pub matched_candidates: usize,
}

/// Result of placing an idempotent batch into a bounded stream buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductStreamEnqueueOutcome {
    /// The batch now consumes bounded in-flight capacity.
    Enqueued,
    /// This process-local stream already accepted the idempotency identity.
    Idempotent,
}

/// Bounded streaming-ingestion policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ProductSearchIngestionCoordinator {
    /// Maximum queued logical bytes.
    pub max_in_flight_bytes: usize,
    /// Maximum queued batches.
    pub max_in_flight_batches: usize,
    /// Maximum process-local idempotency identities retained by the stream.
    pub max_tracked_idempotency_ids: usize,
}

impl ProductSearchIngestionCoordinator {
    /// Creates one bounded stream for a logical collection.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error for any zero bound.
    pub fn stream(
        self,
        collection: crate::ObjectId,
    ) -> Result<ProductSearchIngestStream, ProductError> {
        if self.max_in_flight_bytes == 0
            || self.max_in_flight_batches == 0
            || self.max_tracked_idempotency_ids == 0
        {
            return Err(invalid_request());
        }
        Ok(ProductSearchIngestStream {
            collection,
            policy: self,
            queued: VecDeque::new(),
            in_flight_bytes: 0,
            idempotency_ids: BTreeMap::new(),
        })
    }
}

/// Bounded process-local coordinator for atomic native ingestion batches.
#[derive(Clone, Debug)]
pub struct ProductSearchIngestStream {
    collection: crate::ObjectId,
    policy: ProductSearchIngestionCoordinator,
    queued: VecDeque<(ProductSearchIngestBatch, usize)>,
    in_flight_bytes: usize,
    idempotency_ids: BTreeMap<u128, [u8; 32]>,
}

impl ProductSearchIngestStream {
    /// Returns queued logical bytes currently applying backpressure.
    pub const fn in_flight_bytes(&self) -> usize {
        self.in_flight_bytes
    }

    /// Returns the queued batch count.
    pub fn queued_batches(&self) -> usize {
        self.queued.len()
    }

    /// Enqueues a complete atomic batch or leaves the stream unchanged.
    ///
    /// # Errors
    ///
    /// Returns a limit error when byte, batch, or idempotency tracking capacity
    /// would be exceeded. No queue state changes on failure.
    pub fn enqueue(
        &mut self,
        batch: ProductSearchIngestBatch,
    ) -> Result<ProductStreamEnqueueOutcome, ProductError> {
        validate_batch_shape(&batch)?;
        let digest = ingest_digest(&batch)?;
        if let Some(previous) = self.idempotency_ids.get(&batch.idempotency_id) {
            return if previous == &digest {
                Ok(ProductStreamEnqueueOutcome::Idempotent)
            } else {
                Err(idempotency_conflict())
            };
        }
        let bytes = batch_logical_bytes(&batch)?;
        if self.queued.len() == self.policy.max_in_flight_batches
            || self.idempotency_ids.len() == self.policy.max_tracked_idempotency_ids
            || self
                .in_flight_bytes
                .checked_add(bytes)
                .is_none_or(|total| total > self.policy.max_in_flight_bytes)
        {
            return Err(limit_exceeded());
        }
        let idempotency_id = batch.idempotency_id;
        self.queued.push_back((batch, bytes));
        self.idempotency_ids.insert(idempotency_id, digest);
        self.in_flight_bytes += bytes;
        Ok(ProductStreamEnqueueOutcome::Enqueued)
    }

    /// Commits the oldest queued batch and releases capacity only on success.
    ///
    /// # Errors
    ///
    /// Returns a validation, runtime, or durable-state error. A failed batch
    /// remains queued byte-for-byte for explicit retry or stream disposal.
    pub fn flush_next(
        &mut self,
        product: &mut NativeProduct,
        logical_time_micros: i64,
        durability: ProductDurability,
    ) -> Result<Option<ProductSearchIngestReceipt>, ProductError> {
        let Some((batch, bytes)) = self.queued.front().cloned() else {
            return Ok(None);
        };
        let receipt = product.ingest_search_batch(
            self.collection,
            &batch,
            logical_time_micros,
            durability,
        )?;
        self.queued.pop_front();
        self.in_flight_bytes -= bytes;
        Ok(Some(receipt))
    }
}

impl NativeProduct {
    /// Creates and atomically binds native lexical and named-vector storage for
    /// a logical Catalog V2 collection.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid binding, catalog policy, duplicate
    /// physical identity, or failed atomic native commit.
    pub fn provision_search_collection(
        &mut self,
        collection: crate::ObjectId,
        logical_time_micros: i64,
        durability: ProductDurability,
    ) -> Result<ProductSearchIngestReceipt, ProductError> {
        let definition = self.search_definition(collection)?;
        let mut transaction = self
            .database
            .begin(logical_time_micros, durability.into())
            .map_err(map_runtime_error)?;
        let lexical_index = transaction
            .next_catalog_object_id()
            .map_err(map_runtime_error)?;
        transaction
            .create_search_index(
                lexical_index,
                &format!("__product_lexical_{}", collection.get()),
            )
            .map_err(map_runtime_error)?;
        let mut vectors = Vec::with_capacity(definition.vectors.len());
        for vector in &definition.vectors {
            let index = transaction
                .next_catalog_object_id()
                .map_err(map_runtime_error)?;
            let ann = match vector.policy {
                VectorSearchPolicy::Exact => None,
                VectorSearchPolicy::Ann(ann) | VectorSearchPolicy::Adaptive { ann, .. } => {
                    Some(ann)
                }
            };
            let config = if let Some(ann) = ann {
                HnswConfig::new(
                    ann.m(),
                    ann.ef_construction(),
                    ann.ef_search_default(),
                    ann.ef_search_max(),
                    ann.seed(),
                )
            } else {
                HnswConfig::new(8, 32, 16, 4_096, vector.id.get().into())
            }
            .map_err(|_| invalid_request())?;
            transaction
                .create_vector_index_with_lifecycle(
                    index,
                    &format!("__product_vector_{}_{}", collection.get(), vector.id.get()),
                    vector.vector_type.dimension(),
                    runtime_metric(vector.metric),
                    config,
                    vector.lifecycle,
                )
                .map_err(map_runtime_error)?;
            vectors.push(ProductNamedVectorBinding {
                name: vector.name.lookup().to_owned(),
                index,
            });
        }
        let binding = ProductSearchCollectionBinding {
            collection,
            lexical_index,
            vectors,
        };
        transaction
            .set(binding_key(collection), encode_binding(&binding)?, None)
            .map_err(map_runtime_error)?;
        transaction
            .set(manifest_key(collection), MANIFEST_MAGIC.to_vec(), None)
            .map_err(map_runtime_error)?;
        let commit = transaction.commit().map_err(map_runtime_error)?;
        self.observe_commit(&commit);
        let snapshot = self.snapshot_bounded(logical_time_micros)?.identity();
        Ok(ProductSearchIngestReceipt {
            snapshot,
            commit: Some(commit.into()),
            documents: 0,
            idempotent_replay: false,
        })
    }

    /// Validates and atomically ingests one bounded cross-engine document batch.
    ///
    /// # Errors
    ///
    /// Returns without publication for any invalid document, exhausted bound,
    /// duplicate document, unknown target, or native commit failure.
    pub fn ingest_search_batch(
        &mut self,
        collection: crate::ObjectId,
        batch: &ProductSearchIngestBatch,
        logical_time_micros: i64,
        durability: ProductDurability,
    ) -> Result<ProductSearchIngestReceipt, ProductError> {
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let definition = self.search_definition(collection)?;
        validate_documents(&definition, &binding, batch)?;
        let digest = ingest_digest(batch)?;
        let marker_key = idempotency_key(collection, batch.idempotency_id);
        let current = self.snapshot_bounded(logical_time_micros)?;
        if let Some(encoded) = current.structure_get(&marker_key) {
            let marker = decode_idempotency(encoded)?;
            if marker.digest != digest {
                return Err(idempotency_conflict());
            }
            return Ok(ProductSearchIngestReceipt {
                snapshot: current.identity(),
                commit: Some(self.original_search_receipt(marker.transaction_id)?),
                documents: marker.documents,
                idempotent_replay: true,
            });
        }

        let manifest_key = manifest_key(collection);
        let mut transaction = self
            .database
            .begin(logical_time_micros, durability.into())
            .map_err(map_runtime_error)?;
        let existing_manifest = transaction.get(&manifest_key).ok_or_else(corruption)?;
        let mut identities = decode_manifest(existing_manifest)?;
        if identities.len().saturating_add(batch.documents.len())
            > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS
        {
            return Err(limit_exceeded());
        }
        for document in &batch.documents {
            if !identities.insert(document.object_id) {
                return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
            }
        }

        for document in &batch.documents {
            let object_bytes = document.object_id.get().to_be_bytes().to_vec();
            transaction
                .index_document(binding.lexical_index, object_bytes, document.text.clone())
                .map_err(map_runtime_error)?;
            for vector_binding in &binding.vectors {
                if let Some(vector) = document.vectors.get(&vector_binding.name) {
                    transaction
                        .upsert_vector(vector_binding.index, document.object_id, vector.clone())
                        .map_err(map_runtime_error)?;
                }
            }
            transaction
                .set(
                    document_key(collection, document.object_id),
                    encode_document(document)?,
                    None,
                )
                .map_err(map_runtime_error)?;
        }
        transaction
            .set(manifest_key, encode_manifest(&identities)?, None)
            .map_err(map_runtime_error)?;
        let transaction_id = transaction.transaction_id().get();
        transaction
            .set(
                marker_key,
                encode_idempotency(&IdempotencyMarker {
                    digest,
                    documents: batch.documents.len(),
                    transaction_id,
                })?,
                None,
            )
            .map_err(map_runtime_error)?;
        let commit = transaction.commit().map_err(map_runtime_error)?;
        self.observe_commit(&commit);
        let snapshot = self.snapshot_bounded(logical_time_micros)?.identity();
        Ok(ProductSearchIngestReceipt {
            snapshot,
            commit: Some(commit.into()),
            documents: batch.documents.len(),
            idempotent_replay: false,
        })
    }

    /// Atomically replaces one existing integrated document across lexical,
    /// named-vector, and doc-value state.
    ///
    /// # Errors
    ///
    /// Returns a stable request, catalog, limit, conflict, storage, or
    /// durability error. Validation failures publish no partial mutation.
    pub fn update_search_document(
        &mut self,
        collection: crate::ObjectId,
        update: &ProductSearchDocumentUpdate,
        logical_time_micros: i64,
        durability: ProductDurability,
    ) -> Result<ProductSearchIngestReceipt, ProductError> {
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let definition = self.search_definition(collection)?;
        let batch = ProductSearchIngestBatch {
            idempotency_id: update.idempotency_id,
            documents: vec![update.document.clone()],
        };
        validate_documents(&definition, &binding, &batch)?;
        let digest = lifecycle_digest(
            b'U',
            update.idempotency_id,
            &encode_full_document(&update.document)?,
        )?;
        if let Some(receipt) = self.replay_marker(
            collection,
            update.idempotency_id,
            digest,
            logical_time_micros,
        )? {
            return Ok(receipt);
        }
        let mut transaction = self
            .database
            .begin(logical_time_micros, durability.into())
            .map_err(map_runtime_error)?;
        let manifest = decode_manifest(
            transaction
                .get(&manifest_key(collection))
                .ok_or_else(corruption)?,
        )?;
        if !manifest.contains(&update.document.object_id) {
            return Err(ProductError::from_code(ProductErrorCode::ObjectNotFound)
                .with_object_id(update.document.object_id));
        }
        let object_bytes = update.document.object_id.get().to_be_bytes().to_vec();
        transaction
            .replace_document(
                binding.lexical_index,
                object_bytes,
                update.document.text.clone(),
            )
            .map_err(map_runtime_error)?;
        for vector_binding in &binding.vectors {
            if let Some(vector) = update.document.vectors.get(&vector_binding.name) {
                transaction
                    .upsert_vector(
                        vector_binding.index,
                        update.document.object_id,
                        vector.clone(),
                    )
                    .map_err(map_runtime_error)?;
            } else {
                transaction
                    .delete_vector(vector_binding.index, update.document.object_id)
                    .map_err(map_runtime_error)?;
            }
        }
        transaction
            .set(
                document_key(collection, update.document.object_id),
                encode_document(&update.document)?,
                None,
            )
            .map_err(map_runtime_error)?;
        let transaction_id = transaction.transaction_id().get();
        transaction
            .set(
                idempotency_key(collection, update.idempotency_id),
                encode_idempotency(&IdempotencyMarker {
                    digest,
                    documents: 1,
                    transaction_id,
                })?,
                None,
            )
            .map_err(map_runtime_error)?;
        let commit = transaction.commit().map_err(map_runtime_error)?;
        self.observe_commit(&commit);
        Ok(ProductSearchIngestReceipt {
            snapshot: self.snapshot_bounded(logical_time_micros)?.identity(),
            commit: Some(commit.into()),
            documents: 1,
            idempotent_replay: false,
        })
    }

    /// Atomically deletes one existing integrated document from every branch.
    ///
    /// # Errors
    ///
    /// Returns a stable request, catalog, conflict, storage, or durability
    /// error. Validation failures publish no partial mutation.
    pub fn delete_search_document(
        &mut self,
        collection: crate::ObjectId,
        delete: ProductSearchDocumentDelete,
        logical_time_micros: i64,
        durability: ProductDurability,
    ) -> Result<ProductSearchIngestReceipt, ProductError> {
        if delete.idempotency_id == 0 {
            return Err(invalid_request());
        }
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let digest = lifecycle_digest(
            b'D',
            delete.idempotency_id,
            &delete.object_id.get().to_be_bytes(),
        )?;
        if let Some(receipt) = self.replay_marker(
            collection,
            delete.idempotency_id,
            digest,
            logical_time_micros,
        )? {
            return Ok(receipt);
        }
        let mut transaction = self
            .database
            .begin(logical_time_micros, durability.into())
            .map_err(map_runtime_error)?;
        let manifest_key = manifest_key(collection);
        let mut manifest = decode_manifest(transaction.get(&manifest_key).ok_or_else(corruption)?)?;
        if !manifest.remove(&delete.object_id) {
            return Err(ProductError::from_code(ProductErrorCode::ObjectNotFound)
                .with_object_id(delete.object_id));
        }
        transaction
            .delete_document(
                binding.lexical_index,
                delete.object_id.get().to_be_bytes().to_vec(),
            )
            .map_err(map_runtime_error)?;
        for vector in &binding.vectors {
            transaction
                .delete_vector(vector.index, delete.object_id)
                .map_err(map_runtime_error)?;
        }
        transaction
            .delete_structure(document_key(collection, delete.object_id))
            .map_err(map_runtime_error)?;
        transaction
            .set(manifest_key, encode_manifest(&manifest)?, None)
            .map_err(map_runtime_error)?;
        let transaction_id = transaction.transaction_id().get();
        transaction
            .set(
                idempotency_key(collection, delete.idempotency_id),
                encode_idempotency(&IdempotencyMarker {
                    digest,
                    documents: 1,
                    transaction_id,
                })?,
                None,
            )
            .map_err(map_runtime_error)?;
        let commit = transaction.commit().map_err(map_runtime_error)?;
        self.observe_commit(&commit);
        Ok(ProductSearchIngestReceipt {
            snapshot: self.snapshot_bounded(logical_time_micros)?.identity(),
            commit: Some(commit.into()),
            documents: 1,
            idempotent_replay: false,
        })
    }

    fn replay_marker(
        &self,
        collection: crate::ObjectId,
        idempotency_id: u128,
        digest: [u8; 32],
        logical_time_micros: i64,
    ) -> Result<Option<ProductSearchIngestReceipt>, ProductError> {
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let Some(encoded) = snapshot.structure_get(&idempotency_key(collection, idempotency_id))
        else {
            return Ok(None);
        };
        let marker = decode_idempotency(encoded)?;
        if marker.digest != digest {
            return Err(idempotency_conflict());
        }
        Ok(Some(ProductSearchIngestReceipt {
            snapshot: snapshot.identity(),
            commit: Some(self.original_search_receipt(marker.transaction_id)?),
            documents: marker.documents,
            idempotent_replay: true,
        }))
    }

    fn original_search_receipt(
        &self,
        transaction_id: u128,
    ) -> Result<ProductCommitReceipt, ProductError> {
        let transaction_id = TransactionId::new(transaction_id).map_err(|_| corruption())?;
        self.database
            .transaction_commit_receipt(transaction_id)
            .map(Into::into)
            .ok_or_else(corruption)
    }

    #[doc(hidden)]
    pub fn original_search_receipt_for_test(
        &self,
        transaction_id: u128,
    ) -> Result<ProductCommitReceipt, ProductError> {
        self.original_search_receipt(transaction_id)
    }

    /// Executes BM25, named exact/ANN/adaptive vectors, deterministic RRF, and
    /// typed doc-value semantics on one all-engine snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid binding/request, missing durable side
    /// record, exhausted bound, or native lexical/vector execution failure.
    pub fn search_collection(
        &self,
        collection: crate::ObjectId,
        request: &ProductSearchRequest,
        logical_time_micros: i64,
    ) -> Result<ProductSearchResult, ProductError> {
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let definition = self.search_definition(collection)?;
        validate_search_request(&definition, &binding, request)?;
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let documents = load_documents(&snapshot, collection)?;
        let total_documents = documents.len();
        let eligible = filter_documents(&documents, &request.filter)?;
        let eligible_ids = eligible
            .iter()
            .map(|candidate| decode_object_id(&candidate.document_id))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut fused = BTreeMap::<crate::ObjectId, f64>::new();
        let lexical_candidates = execute_lexical_branch(
            &snapshot,
            binding.lexical_index,
            request.lexical.as_ref(),
            &eligible_ids,
            &mut fused,
        )?;
        let vector_receipts = execute_vector_branches(
            &snapshot,
            &binding,
            &definition,
            &request.vectors,
            &eligible_ids,
            &mut fused,
        )?;

        if request.lexical.is_none() && request.vectors.is_empty() {
            for candidate in &eligible {
                fused.insert(decode_object_id(&candidate.document_id)?, 0.0);
            }
        }
        let by_id = documents
            .into_iter()
            .map(|candidate| Ok((decode_object_id(&candidate.document_id)?, candidate)))
            .collect::<Result<BTreeMap<_, _>, ProductError>>()?;
        let candidates = fused
            .into_iter()
            .map(|(object_id, score)| {
                let source = by_id.get(&object_id).ok_or_else(corruption)?;
                Ok(hyphae_native_runtime::DocValueCandidate {
                    document_id: object_id.get().to_be_bytes().to_vec(),
                    score,
                    values: source.values.clone(),
                })
            })
            .collect::<Result<Vec<_>, ProductError>>()?;
        let retrieval_candidates = candidates.len();
        let doc_request = hyphae_native_runtime::DocValueRequest {
            filter: request.filter.clone(),
            sort: request.sort.clone(),
            limit: request.limit,
            facets: request.facets.clone(),
            aggregations: request.aggregations.clone(),
        };
        let result = execute_doc_values(&candidates, &doc_request, &doc_value_limits())
            .map_err(|error| map_doc_value_error(&error))?;
        let approximate = vector_receipts.iter().any(|receipt| receipt.approximate);
        Ok(ProductSearchResult {
            snapshot: snapshot.identity(),
            hits: result
                .hits
                .into_iter()
                .map(|hit| {
                    Ok(ProductIntegratedSearchHit {
                        object_id: decode_object_id(&hit.document_id)?,
                        score: hit.score,
                        doc_values: hit.values,
                    })
                })
                .collect::<Result<_, ProductError>>()?,
            facets: result.facets,
            aggregations: result.aggregations,
            vector_branches: vector_receipts,
            approximate,
            total_documents,
            eligible_documents: eligible_ids.len(),
            lexical_candidates,
            retrieval_candidates,
            matched_candidates: result.matched_candidates,
        })
    }

    /// Executes one integrated search against a caller-owned immutable product
    /// snapshot. This entry point keeps snapshot capture outside a benchmarked
    /// hot path and preserves the exact same search semantics as
    /// [`Self::search_collection`].
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid binding/request, missing durable side
    /// record, exhausted bound, or native lexical/vector execution failure.
    pub fn search_collection_at_snapshot(
        _product: &Self,
        snapshot: &crate::ProductSnapshot,
        collection: crate::ObjectId,
        request: &ProductSearchRequest,
    ) -> Result<ProductSearchResult, ProductError> {
        let binding = Self::search_collection_binding_at_snapshot(snapshot, collection)?;
        let definition = Self::search_definition_at_snapshot(snapshot, collection)?;
        validate_search_request(&definition, &binding, request)?;
        let documents = load_documents(snapshot, collection)?;
        let total_documents = documents.len();
        let eligible = filter_documents(&documents, &request.filter)?;
        let eligible_ids = eligible
            .iter()
            .map(|candidate| decode_object_id(&candidate.document_id))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut fused = BTreeMap::<crate::ObjectId, f64>::new();
        let lexical_candidates = execute_lexical_branch(
            snapshot,
            binding.lexical_index,
            request.lexical.as_ref(),
            &eligible_ids,
            &mut fused,
        )?;
        let vector_receipts = execute_vector_branches(
            snapshot,
            &binding,
            &definition,
            &request.vectors,
            &eligible_ids,
            &mut fused,
        )?;
        if request.lexical.is_none() && request.vectors.is_empty() {
            for candidate in &eligible {
                fused.insert(decode_object_id(&candidate.document_id)?, 0.0);
            }
        }
        let by_id = documents
            .into_iter()
            .map(|candidate| Ok((decode_object_id(&candidate.document_id)?, candidate)))
            .collect::<Result<BTreeMap<_, _>, ProductError>>()?;
        let candidates = fused
            .into_iter()
            .map(|(object_id, score)| {
                let source = by_id.get(&object_id).ok_or_else(corruption)?;
                Ok(hyphae_native_runtime::DocValueCandidate {
                    document_id: object_id.get().to_be_bytes().to_vec(),
                    score,
                    values: source.values.clone(),
                })
            })
            .collect::<Result<Vec<_>, ProductError>>()?;
        let result = execute_doc_values(
            &candidates,
            &hyphae_native_runtime::DocValueRequest {
                filter: request.filter.clone(),
                sort: request.sort.clone(),
                limit: request.limit,
                facets: request.facets.clone(),
                aggregations: request.aggregations.clone(),
            },
            &doc_value_limits(),
        )
        .map_err(|error| map_doc_value_error(&error))?;
        Ok(ProductSearchResult {
            snapshot: snapshot.identity(),
            hits: result
                .hits
                .into_iter()
                .map(|hit| {
                    Ok(ProductIntegratedSearchHit {
                        object_id: decode_object_id(&hit.document_id)?,
                        score: hit.score,
                        doc_values: hit.values,
                    })
                })
                .collect::<Result<_, ProductError>>()?,
            facets: result.facets,
            aggregations: result.aggregations,
            vector_branches: vector_receipts,
            approximate: false,
            total_documents,
            eligible_documents: eligible_ids.len(),
            lexical_candidates,
            retrieval_candidates: candidates.len(),
            matched_candidates: result.matched_candidates,
        })
    }

    fn search_definition(
        &self,
        collection: crate::ObjectId,
    ) -> Result<SearchCollectionDefinitionV2, ProductError> {
        let snapshot = self.catalog_snapshot()?;
        let object = self
            .catalog_describe(&snapshot, collection)?
            .ok_or_else(|| {
                ProductError::from_code(ProductErrorCode::ObjectNotFound).with_object_id(collection)
            })?;
        let LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition)) = object else {
            return Err(invalid_request());
        };
        Ok(definition)
    }

    fn search_definition_at_snapshot(
        product_snapshot: &crate::ProductSnapshot,
        collection: crate::ObjectId,
    ) -> Result<SearchCollectionDefinitionV2, ProductError> {
        let object = product_snapshot
            .inner
            .logical_catalog_object(collection)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        let LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition)) = object else {
            return Err(invalid_request());
        };
        Ok(definition.clone())
    }

    fn search_collection_binding_at_snapshot(
        product_snapshot: &crate::ProductSnapshot,
        collection: crate::ObjectId,
    ) -> Result<ProductSearchCollectionBinding, ProductError> {
        let encoded = product_snapshot
            .structure_get(&binding_key(collection))
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::ObjectNotFound))?;
        decode_binding(encoded)
    }

    /// Resolves a durable catalog-owned physical search binding.
    ///
    /// # Errors
    ///
    /// Returns a stable not-found, catalog, corruption, or snapshot error.
    pub fn resolve_search_collection_binding(
        &self,
        collection: crate::ObjectId,
        logical_time_micros: i64,
    ) -> Result<ProductSearchCollectionBinding, ProductError> {
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let encoded = snapshot
            .structure_get(&binding_key(collection))
            .ok_or_else(|| {
                ProductError::from_code(ProductErrorCode::ObjectNotFound).with_object_id(collection)
            })?;
        let binding = decode_binding(encoded)?;
        let definition = self.search_definition(collection)?;
        validate_binding_definition(&definition, &binding)?;
        Ok(binding)
    }
}

fn execute_lexical_branch(
    snapshot: &ProductSnapshot,
    index: crate::ObjectId,
    lexical: Option<&ProductLexicalBranch>,
    eligible: &BTreeSet<crate::ObjectId>,
    fused: &mut BTreeMap<crate::ObjectId, f64>,
) -> Result<usize, ProductError> {
    let Some(lexical) = lexical else {
        return Ok(0);
    };
    let hits = snapshot
        .inner
        .match_text(index, &lexical.query, lexical.candidate_limit)
        .map_err(map_runtime_error)?;
    let mut admitted = 0;
    for (rank, hit) in hits.into_iter().enumerate() {
        let object_id = decode_object_id(&hit.document_id)?;
        if eligible.contains(&object_id) {
            add_rrf(fused, object_id, lexical.weight, rank)?;
            admitted += 1;
        }
    }
    Ok(admitted)
}

fn execute_vector_branches(
    snapshot: &ProductSnapshot,
    binding: &ProductSearchCollectionBinding,
    definition: &SearchCollectionDefinitionV2,
    branches: &[ProductVectorBranch],
    eligible: &BTreeSet<crate::ObjectId>,
    fused: &mut BTreeMap<crate::ObjectId, f64>,
) -> Result<Vec<ProductVectorBranchReceipt>, ProductError> {
    let mut receipts = Vec::with_capacity(branches.len());
    for branch in branches {
        let vector_binding = binding
            .vectors
            .iter()
            .find(|binding| binding.name == branch.target)
            .ok_or_else(invalid_request)?;
        let vector = definition
            .vectors
            .iter()
            .find(|vector| vector.name.lookup() == branch.target)
            .ok_or_else(invalid_request)?;
        let (hits, receipt) =
            execute_vector_branch(snapshot, vector_binding, vector.policy, branch, eligible)?;
        for (rank, hit) in hits.into_iter().enumerate() {
            add_rrf(fused, hit.object_id, branch.weight, rank)?;
        }
        receipts.push(receipt);
    }
    Ok(receipts)
}

fn execute_vector_branch(
    snapshot: &ProductSnapshot,
    binding: &ProductNamedVectorBinding,
    policy: VectorSearchPolicy,
    branch: &ProductVectorBranch,
    eligible: &BTreeSet<crate::ObjectId>,
) -> Result<(Vec<VectorHit>, ProductVectorBranchReceipt), ProductError> {
    let execution = effective_vector_execution(policy, branch)?;
    let adaptive_exact = matches!(
        execution,
        ProductVectorExecution::Adaptive {
            exact_candidate_threshold,
            ..
        } if eligible.len() <= exact_candidate_threshold
    );
    if execution == ProductVectorExecution::Exact || adaptive_exact {
        let hits = snapshot
            .inner
            .search_vector_exact_filtered(
                binding.index,
                &branch.query,
                branch.candidate_limit,
                eligible,
            )
            .map_err(map_runtime_error)?;
        let strategy = if adaptive_exact {
            ProductVectorStrategy::AdaptiveExactFiltered
        } else {
            ProductVectorStrategy::ExactFiltered
        };
        return Ok((
            hits.clone(),
            ProductVectorBranchReceipt {
                target: branch.target.clone(),
                strategy,
                approximate: false,
                eligible_documents: eligible.len(),
                candidate_count: hits.len(),
                visited_nodes: 0,
                exact_reranked: true,
            },
        ));
    }

    let (ef_search, exact_rerank, adaptive) = match execution {
        ProductVectorExecution::Ann {
            ef_search,
            exact_rerank,
        } => (ef_search, exact_rerank, false),
        ProductVectorExecution::Adaptive {
            ef_search,
            exact_rerank,
            ..
        } => (ef_search, exact_rerank, true),
        ProductVectorExecution::Exact => unreachable!("exact branch returned above"),
    };
    let options = AnnSearchOptions::new(branch.candidate_limit, ef_search, exact_rerank)
        .map_err(|_| invalid_request())?;
    let ann = snapshot
        .inner
        .search_ann_filtered(binding.index, &branch.query, options, eligible)
        .map_err(map_runtime_error)?;

    // The native graph API currently exposes an honest post-filter receipt.
    // Seed from the complete filtered oracle so this product route is not
    // post-filter-only while retaining ANN ranking for the remaining slots.
    let seed_limit = usize::from(branch.candidate_limit > 0 && !eligible.is_empty());
    let exact_seed = snapshot
        .inner
        .search_vector_exact_filtered(binding.index, &branch.query, seed_limit, eligible)
        .map_err(map_runtime_error)?;
    let mut merged = BTreeMap::<crate::ObjectId, f64>::new();
    for hit in ann.hits.iter().chain(&exact_seed) {
        merged
            .entry(hit.object_id)
            .and_modify(|distance| *distance = distance.min(hit.distance))
            .or_insert(hit.distance);
    }
    let mut hits = merged
        .into_iter()
        .map(|(object_id, distance)| VectorHit {
            object_id,
            distance,
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.object_id.cmp(&right.object_id))
    });
    hits.truncate(branch.candidate_limit);
    let approximate = eligible.len() > seed_limit && branch.candidate_limit > seed_limit;
    Ok((
        hits,
        ProductVectorBranchReceipt {
            target: branch.target.clone(),
            strategy: if adaptive {
                ProductVectorStrategy::AdaptiveFilterAwareAnn
            } else {
                ProductVectorStrategy::FilterAwareAnn
            },
            approximate,
            eligible_documents: eligible.len(),
            candidate_count: ann.candidate_count.saturating_add(exact_seed.len()),
            visited_nodes: ann.visited_nodes,
            exact_reranked: ann.exact_reranked || !exact_seed.is_empty(),
        },
    ))
}

fn effective_vector_execution(
    policy: VectorSearchPolicy,
    branch: &ProductVectorBranch,
) -> Result<ProductVectorExecution, ProductError> {
    let execution = match (policy, branch.execution) {
        (VectorSearchPolicy::Exact, None | Some(ProductVectorExecution::Exact)) => {
            ProductVectorExecution::Exact
        }
        (VectorSearchPolicy::Ann(ann), None) => ProductVectorExecution::Ann {
            ef_search: usize::from(ann.ef_search_default()),
            exact_rerank: None,
        },
        (
            VectorSearchPolicy::Ann(ann),
            Some(ProductVectorExecution::Ann {
                ef_search,
                exact_rerank,
            }),
        ) if ef_search <= usize::from(ann.ef_search_max()) => ProductVectorExecution::Ann {
            ef_search,
            exact_rerank,
        },
        (
            VectorSearchPolicy::Adaptive {
                exact_candidate_threshold,
                ann,
            },
            None,
        ) => ProductVectorExecution::Adaptive {
            exact_candidate_threshold: usize::try_from(exact_candidate_threshold)
                .map_err(|_| invalid_request())?,
            ef_search: usize::from(ann.ef_search_default()),
            exact_rerank: None,
        },
        (
            VectorSearchPolicy::Adaptive {
                exact_candidate_threshold,
                ann,
            },
            Some(ProductVectorExecution::Adaptive {
                exact_candidate_threshold: supplied,
                ef_search,
                exact_rerank,
            }),
        ) if supplied
            == usize::try_from(exact_candidate_threshold).map_err(|_| invalid_request())?
            && ef_search <= usize::from(ann.ef_search_max()) =>
        {
            ProductVectorExecution::Adaptive {
                exact_candidate_threshold: supplied,
                ef_search,
                exact_rerank,
            }
        }
        _ => return Err(invalid_request()),
    };
    Ok(execution)
}

fn validate_binding_shape(binding: &ProductSearchCollectionBinding) -> Result<(), ProductError> {
    if binding.vectors.len() > MAX_PRODUCT_SEARCH_VECTOR_TARGETS
        || binding.lexical_index == binding.collection
    {
        return Err(invalid_request());
    }
    let mut names = BTreeSet::new();
    let mut indexes = BTreeSet::from([binding.collection, binding.lexical_index]);
    for vector in &binding.vectors {
        if vector.name.is_empty()
            || !names.insert(vector.name.as_str())
            || !indexes.insert(vector.index)
        {
            return Err(invalid_request());
        }
    }
    Ok(())
}

fn validate_binding_definition(
    definition: &SearchCollectionDefinitionV2,
    binding: &ProductSearchCollectionBinding,
) -> Result<(), ProductError> {
    if definition.header.id != binding.collection
        || !definition
            .fields
            .iter()
            .any(|field| field.options.lexical != LexicalIndexPolicy::None)
        || definition.vectors.len() != binding.vectors.len()
    {
        return Err(invalid_request());
    }
    for (catalog, physical) in definition.vectors.iter().zip(&binding.vectors) {
        if catalog.name.lookup() != physical.name {
            return Err(invalid_request());
        }
    }
    Ok(())
}

fn validate_batch_shape(batch: &ProductSearchIngestBatch) -> Result<(), ProductError> {
    if batch.idempotency_id == 0
        || batch.documents.is_empty()
        || batch.documents.len() > MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS
    {
        return Err(
            if batch.documents.len() > MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS {
                limit_exceeded()
            } else {
                invalid_request()
            },
        );
    }
    if batch_logical_bytes(batch)? > MAX_PRODUCT_SEARCH_BATCH_BYTES {
        return Err(limit_exceeded());
    }
    Ok(())
}

fn validate_documents(
    definition: &SearchCollectionDefinitionV2,
    binding: &ProductSearchCollectionBinding,
    batch: &ProductSearchIngestBatch,
) -> Result<(), ProductError> {
    validate_batch_shape(batch)?;
    let candidates = batch
        .documents
        .iter()
        .map(|document| hyphae_native_runtime::DocValueCandidate {
            document_id: document.object_id.get().to_be_bytes().to_vec(),
            score: 0.0,
            values: document.doc_values.clone(),
        })
        .collect::<Vec<_>>();
    let validation_request = hyphae_native_runtime::DocValueRequest {
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        limit: batch.documents.len(),
        facets: Vec::new(),
        aggregations: Vec::new(),
    };
    execute_doc_values(&candidates, &validation_request, &doc_value_limits())
        .map_err(|error| map_doc_value_error(&error))?;
    for document in &batch.documents {
        for (name, value) in &document.doc_values {
            let field = definition
                .fields
                .iter()
                .find(|field| field.name.lookup() == name && field.options.doc_values)
                .ok_or_else(invalid_request)?;
            if !doc_value_matches_type(value, &field.logical_type) {
                return Err(invalid_request());
            }
        }
        for (name, vector) in &document.vectors {
            let catalog = definition
                .vectors
                .iter()
                .find(|candidate| candidate.name.lookup() == name)
                .ok_or_else(invalid_request)?;
            if vector.dimension() != usize::from(catalog.vector_type.dimension())
                || !binding.vectors.iter().any(|target| target.name == *name)
            {
                return Err(invalid_request());
            }
        }
    }
    Ok(())
}

fn validate_search_request(
    definition: &SearchCollectionDefinitionV2,
    binding: &ProductSearchCollectionBinding,
    request: &ProductSearchRequest,
) -> Result<(), ProductError> {
    if !(1..=MAX_PRODUCT_SEARCH_HITS).contains(&request.limit)
        || request.vectors.len() > MAX_PRODUCT_SEARCH_VECTOR_TARGETS
    {
        return Err(limit_exceeded());
    }
    if let Some(lexical) = &request.lexical
        && (!(1..=MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES).contains(&lexical.candidate_limit)
            || lexical.weight == 0)
    {
        return Err(invalid_request());
    }
    let mut targets = BTreeSet::new();
    for branch in &request.vectors {
        let vector = definition
            .vectors
            .iter()
            .find(|vector| vector.name.lookup() == branch.target)
            .ok_or_else(invalid_request)?;
        if !targets.insert(branch.target.as_str())
            || !binding
                .vectors
                .iter()
                .any(|target| target.name == branch.target)
            || branch.query.dimension() != usize::from(vector.vector_type.dimension())
            || !(1..=MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES).contains(&branch.candidate_limit)
            || branch.weight == 0
        {
            return Err(invalid_request());
        }
        match effective_vector_execution(vector.policy, branch)? {
            ProductVectorExecution::Adaptive {
                exact_candidate_threshold: 0,
                ..
            } => return Err(invalid_request()),
            ProductVectorExecution::Ann { ef_search, .. }
            | ProductVectorExecution::Adaptive { ef_search, .. }
                if ef_search < branch.candidate_limit =>
            {
                return Err(invalid_request());
            }
            _ => {}
        }
    }
    let empty = Vec::new();
    let validation = hyphae_native_runtime::DocValueRequest {
        filter: request.filter.clone(),
        sort: request.sort.clone(),
        limit: request.limit,
        facets: request.facets.clone(),
        aggregations: request.aggregations.clone(),
    };
    execute_doc_values(&empty, &validation, &doc_value_limits())
        .map_err(|error| map_doc_value_error(&error))?;
    Ok(())
}

/// Validates and returns the exact canonical wire round trip for an integrated
/// search request. This keeps transport conformance independent from execution
/// fixtures while exercising the full filter/sort/facet/metric contract.
#[doc(hidden)]
pub fn conformance_validate_integrated_request(
    request: &ProductSearchRequest,
) -> Result<(), ProductError> {
    let empty = Vec::new();
    let validation = hyphae_native_runtime::DocValueRequest {
        filter: request.filter.clone(),
        sort: request.sort.clone(),
        limit: request.limit,
        facets: request.facets.clone(),
        aggregations: request.aggregations.clone(),
    };
    execute_doc_values(&empty, &validation, &doc_value_limits())
        .map(|_| ())
        .map_err(|error| map_doc_value_error(&error))
}

fn load_documents(
    snapshot: &ProductSnapshot,
    collection: crate::ObjectId,
) -> Result<Vec<hyphae_native_runtime::DocValueCandidate>, ProductError> {
    let manifest = snapshot
        .structure_get(&manifest_key(collection))
        .ok_or_else(corruption)?;
    let identities = decode_manifest(manifest)?;
    if identities.len() > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS {
        return Err(corruption());
    }
    identities
        .into_iter()
        .map(|object_id| {
            let encoded = snapshot
                .structure_get(&document_key(collection, object_id))
                .ok_or_else(corruption)?;
            Ok(hyphae_native_runtime::DocValueCandidate {
                document_id: object_id.get().to_be_bytes().to_vec(),
                score: 0.0,
                values: decode_document(encoded, object_id)?,
            })
        })
        .collect()
}

fn filter_documents(
    documents: &[hyphae_native_runtime::DocValueCandidate],
    filter: &ProductSearchFilter,
) -> Result<Vec<hyphae_native_runtime::DocValueCandidate>, ProductError> {
    let request = hyphae_native_runtime::DocValueRequest {
        filter: filter.clone(),
        sort: Vec::new(),
        limit: documents.len().max(1),
        facets: Vec::new(),
        aggregations: Vec::new(),
    };
    execute_doc_values(documents, &request, &doc_value_limits())
        .map(|result| result.hits)
        .map_err(|error| map_doc_value_error(&error))
}

fn doc_value_limits() -> hyphae_native_runtime::DocValueLimits {
    hyphae_native_runtime::DocValueLimits {
        max_candidates: MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS,
        max_matches: MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS,
        max_hits: MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS,
        ..hyphae_native_runtime::DocValueLimits::default()
    }
}

fn doc_value_matches_type(value: &ProductDocValue, logical: &LogicalType) -> bool {
    matches!(
        (value, logical),
        (ProductDocValue::Boolean(_), LogicalType::Boolean)
            | (
                ProductDocValue::Integer(_),
                LogicalType::Signed(_) | LogicalType::Date | LogicalType::Timestamp
            )
            | (ProductDocValue::String(_), LogicalType::Text)
            | (ProductDocValue::Bytes(_), LogicalType::Binary)
    )
}

fn add_rrf(
    fused: &mut BTreeMap<crate::ObjectId, f64>,
    object_id: crate::ObjectId,
    weight: u32,
    zero_based_rank: usize,
) -> Result<(), ProductError> {
    let rank = u32::try_from(zero_based_rank)
        .ok()
        .and_then(|rank| rank.checked_add(1))
        .ok_or_else(limit_exceeded)?;
    let contribution = f64::from(weight) / (RRF_CONSTANT + f64::from(rank));
    let score = fused.entry(object_id).or_default();
    *score += contribution;
    if !score.is_finite() || *score < 0.0 {
        return Err(invalid_request());
    }
    Ok(())
}

fn batch_logical_bytes(batch: &ProductSearchIngestBatch) -> Result<usize, ProductError> {
    batch
        .documents
        .iter()
        .try_fold(16_usize, |total, document| {
            let document_bytes = document.doc_values.iter().try_fold(
                32_usize.saturating_add(document.text.len()),
                |sum, (name, value)| {
                    sum.checked_add(name.len())
                        .and_then(|sum| sum.checked_add(doc_value_bytes(value)))
                        .ok_or_else(limit_exceeded)
                },
            )?;
            let vector_bytes =
                document
                    .vectors
                    .iter()
                    .try_fold(0_usize, |sum, (name, vector)| {
                        sum.checked_add(name.len())
                            .and_then(|sum| sum.checked_add(vector.dimension().saturating_mul(4)))
                            .ok_or_else(limit_exceeded)
                    })?;
            total
                .checked_add(document_bytes)
                .and_then(|sum| sum.checked_add(vector_bytes))
                .ok_or_else(limit_exceeded)
        })
}

const fn doc_value_bytes(value: &ProductDocValue) -> usize {
    match value {
        ProductDocValue::Boolean(_) => 1,
        ProductDocValue::Integer(_) => 8,
        ProductDocValue::String(value) => value.len(),
        ProductDocValue::Bytes(value) => value.len(),
    }
}

fn manifest_key(collection: crate::ObjectId) -> Vec<u8> {
    storage_key(b'M', collection, None)
}

fn binding_key(collection: crate::ObjectId) -> Vec<u8> {
    storage_key(b'B', collection, None)
}

fn document_key(collection: crate::ObjectId, object_id: crate::ObjectId) -> Vec<u8> {
    storage_key(b'D', collection, Some(object_id.get().to_be_bytes()))
}

fn idempotency_key(collection: crate::ObjectId, idempotency_id: u128) -> Vec<u8> {
    storage_key(b'I', collection, Some(idempotency_id.to_be_bytes()))
}

fn storage_key(kind: u8, collection: crate::ObjectId, suffix: Option<[u8; 16]>) -> Vec<u8> {
    let mut key = Vec::with_capacity(STORAGE_PREFIX.len() + 33);
    key.extend_from_slice(STORAGE_PREFIX);
    key.push(kind);
    key.extend_from_slice(&collection.get().to_be_bytes());
    if let Some(suffix) = suffix {
        key.extend_from_slice(&suffix);
    }
    key
}

fn encode_manifest(identities: &BTreeSet<crate::ObjectId>) -> Result<Vec<u8>, ProductError> {
    let count = u32::try_from(identities.len()).map_err(|_| limit_exceeded())?;
    let mut encoded = Vec::with_capacity(12 + identities.len().saturating_mul(16));
    encoded.extend_from_slice(MANIFEST_MAGIC);
    encoded.extend_from_slice(&count.to_le_bytes());
    for object_id in identities {
        encoded.extend_from_slice(&object_id.get().to_be_bytes());
    }
    Ok(encoded)
}

fn decode_manifest(encoded: &[u8]) -> Result<BTreeSet<crate::ObjectId>, ProductError> {
    if encoded == MANIFEST_MAGIC {
        return Ok(BTreeSet::new());
    }
    if encoded.len() < 12 || encoded.get(..8) != Some(MANIFEST_MAGIC.as_slice()) {
        return Err(corruption());
    }
    let count = usize::try_from(u32::from_le_bytes(
        encoded[8..12].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    if encoded.len() != 12_usize.saturating_add(count.saturating_mul(16)) {
        return Err(corruption());
    }
    let mut identities = BTreeSet::new();
    for chunk in encoded[12..].chunks_exact(16) {
        let object_id = crate::ObjectId::new(u128::from_be_bytes(
            chunk.try_into().map_err(|_| corruption())?,
        ))
        .map_err(|_| corruption())?;
        if !identities.insert(object_id) {
            return Err(corruption());
        }
    }
    Ok(identities)
}

fn encode_binding(binding: &ProductSearchCollectionBinding) -> Result<Vec<u8>, ProductError> {
    validate_binding_shape(binding)?;
    let count = u32::try_from(binding.vectors.len()).map_err(|_| limit_exceeded())?;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(BINDING_MAGIC);
    encoded.extend_from_slice(&binding.collection.get().to_be_bytes());
    encoded.extend_from_slice(&binding.lexical_index.get().to_be_bytes());
    encoded.extend_from_slice(&count.to_le_bytes());
    for vector in &binding.vectors {
        put_bytes(&mut encoded, vector.name.as_bytes())?;
        encoded.extend_from_slice(&vector.index.get().to_be_bytes());
    }
    Ok(encoded)
}

fn decode_binding(encoded: &[u8]) -> Result<ProductSearchCollectionBinding, ProductError> {
    if encoded.len() < 44 || encoded.get(..8) != Some(BINDING_MAGIC.as_slice()) {
        return Err(corruption());
    }
    let collection = crate::ObjectId::new(u128::from_be_bytes(
        encoded[8..24].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let lexical_index = crate::ObjectId::new(u128::from_be_bytes(
        encoded[24..40].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let count = usize::try_from(u32::from_le_bytes(
        encoded[40..44].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    if count > MAX_PRODUCT_SEARCH_VECTOR_TARGETS {
        return Err(corruption());
    }
    let mut cursor = 44;
    let mut vectors = Vec::with_capacity(count);
    for _ in 0..count {
        let name = String::from_utf8(read_bytes(encoded, &mut cursor)?.to_vec())
            .map_err(|_| corruption())?;
        let end = cursor.checked_add(16).ok_or_else(corruption)?;
        let index = crate::ObjectId::new(u128::from_be_bytes(
            encoded
                .get(cursor..end)
                .ok_or_else(corruption)?
                .try_into()
                .map_err(|_| corruption())?,
        ))
        .map_err(|_| corruption())?;
        cursor = end;
        vectors.push(ProductNamedVectorBinding { name, index });
    }
    if cursor != encoded.len() {
        return Err(corruption());
    }
    let binding = ProductSearchCollectionBinding {
        collection,
        lexical_index,
        vectors,
    };
    validate_binding_shape(&binding).map_err(|_| corruption())?;
    Ok(binding)
}

fn encode_document(document: &ProductDocument) -> Result<Vec<u8>, ProductError> {
    let count = u32::try_from(document.doc_values.len()).map_err(|_| limit_exceeded())?;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(DOCUMENT_MAGIC);
    encoded.extend_from_slice(&document.object_id.get().to_be_bytes());
    encoded.extend_from_slice(&count.to_le_bytes());
    for (name, value) in &document.doc_values {
        put_bytes(&mut encoded, name.as_bytes())?;
        match value {
            ProductDocValue::Boolean(value) => {
                encoded.push(1);
                put_bytes(&mut encoded, &[u8::from(*value)])?;
            }
            ProductDocValue::Integer(value) => {
                encoded.push(2);
                put_bytes(&mut encoded, &value.to_le_bytes())?;
            }
            ProductDocValue::String(value) => {
                encoded.push(3);
                put_bytes(&mut encoded, value.as_bytes())?;
            }
            ProductDocValue::Bytes(value) => {
                encoded.push(4);
                put_bytes(&mut encoded, value)?;
            }
        }
    }
    Ok(encoded)
}

fn decode_document(
    encoded: &[u8],
    expected: crate::ObjectId,
) -> Result<BTreeMap<String, ProductDocValue>, ProductError> {
    if encoded.len() < 28 || encoded.get(..8) != Some(DOCUMENT_MAGIC.as_slice()) {
        return Err(corruption());
    }
    let object_id = crate::ObjectId::new(u128::from_be_bytes(
        encoded[8..24].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    if object_id != expected {
        return Err(corruption());
    }
    let count = usize::try_from(u32::from_le_bytes(
        encoded[24..28].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let mut cursor = 28;
    let mut values = BTreeMap::new();
    for _ in 0..count {
        let name = String::from_utf8(read_bytes(encoded, &mut cursor)?.to_vec())
            .map_err(|_| corruption())?;
        let tag = *encoded.get(cursor).ok_or_else(corruption)?;
        cursor += 1;
        let value = read_bytes(encoded, &mut cursor)?;
        let value = match tag {
            1 if value.len() == 1 && value[0] <= 1 => ProductDocValue::Boolean(value[0] == 1),
            2 if value.len() == 8 => ProductDocValue::Integer(i64::from_le_bytes(
                value.try_into().map_err(|_| corruption())?,
            )),
            3 => ProductDocValue::String(
                String::from_utf8(value.to_vec()).map_err(|_| corruption())?,
            ),
            4 => ProductDocValue::Bytes(value.to_vec()),
            _ => return Err(corruption()),
        };
        if name.is_empty() || values.insert(name, value).is_some() {
            return Err(corruption());
        }
    }
    if cursor != encoded.len() {
        return Err(corruption());
    }
    Ok(values)
}

struct IdempotencyMarker {
    digest: [u8; 32],
    documents: usize,
    transaction_id: u128,
}

fn encode_idempotency(marker: &IdempotencyMarker) -> Result<Vec<u8>, ProductError> {
    let documents = u32::try_from(marker.documents).map_err(|_| limit_exceeded())?;
    let mut encoded = Vec::with_capacity(60);
    encoded.extend_from_slice(IDEMPOTENCY_MAGIC);
    encoded.extend_from_slice(&marker.digest);
    encoded.extend_from_slice(&documents.to_le_bytes());
    encoded.extend_from_slice(&marker.transaction_id.to_le_bytes());
    Ok(encoded)
}

fn decode_idempotency(encoded: &[u8]) -> Result<IdempotencyMarker, ProductError> {
    if encoded.len() != 60 || encoded.get(..8) != Some(IDEMPOTENCY_MAGIC.as_slice()) {
        return Err(corruption());
    }
    let transaction_id = u128::from_le_bytes(encoded[44..60].try_into().map_err(|_| corruption())?);
    if transaction_id == 0 {
        return Err(corruption());
    }
    Ok(IdempotencyMarker {
        digest: encoded[8..40].try_into().map_err(|_| corruption())?,
        documents: usize::try_from(u32::from_le_bytes(
            encoded[40..44].try_into().map_err(|_| corruption())?,
        ))
        .map_err(|_| corruption())?,
        transaction_id,
    })
}

fn ingest_digest(batch: &ProductSearchIngestBatch) -> Result<[u8; 32], ProductError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"hyphae.product.search.ingest.v1\0");
    encoded.extend_from_slice(&batch.idempotency_id.to_le_bytes());
    let count = u32::try_from(batch.documents.len()).map_err(|_| limit_exceeded())?;
    encoded.extend_from_slice(&count.to_le_bytes());
    for document in &batch.documents {
        put_bytes(&mut encoded, &encode_full_document(document)?)?;
    }
    Ok(*blake3::hash(&encoded).as_bytes())
}

fn lifecycle_digest(kind: u8, idempotency_id: u128, body: &[u8]) -> Result<[u8; 32], ProductError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"hyphae.product.search.lifecycle.v1\0");
    encoded.push(kind);
    encoded.extend_from_slice(&idempotency_id.to_le_bytes());
    put_bytes(&mut encoded, body)?;
    Ok(*blake3::hash(&encoded).as_bytes())
}

fn encode_full_document(document: &ProductDocument) -> Result<Vec<u8>, ProductError> {
    let mut encoded = encode_document(document)?;
    put_bytes(&mut encoded, document.text.as_bytes())?;
    let count = u32::try_from(document.vectors.len()).map_err(|_| limit_exceeded())?;
    encoded.extend_from_slice(&count.to_le_bytes());
    for (name, vector) in &document.vectors {
        put_bytes(&mut encoded, name.as_bytes())?;
        let dimension = u32::try_from(vector.dimension()).map_err(|_| limit_exceeded())?;
        encoded.extend_from_slice(&dimension.to_le_bytes());
        for value in vector.values() {
            encoded.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }
    Ok(encoded)
}

fn put_bytes(encoded: &mut Vec<u8>, value: &[u8]) -> Result<(), ProductError> {
    let length = u32::try_from(value.len()).map_err(|_| limit_exceeded())?;
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(value);
    Ok(())
}

fn read_bytes<'encoded>(
    encoded: &'encoded [u8],
    cursor: &mut usize,
) -> Result<&'encoded [u8], ProductError> {
    let length_end = cursor.checked_add(4).ok_or_else(corruption)?;
    let length = usize::try_from(u32::from_le_bytes(
        encoded
            .get(*cursor..length_end)
            .ok_or_else(corruption)?
            .try_into()
            .map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let end = length_end.checked_add(length).ok_or_else(corruption)?;
    let value = encoded.get(length_end..end).ok_or_else(corruption)?;
    *cursor = end;
    Ok(value)
}

fn decode_object_id(encoded: &[u8]) -> Result<crate::ObjectId, ProductError> {
    let bytes: [u8; 16] = encoded.try_into().map_err(|_| corruption())?;
    crate::ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| corruption())
}

const fn runtime_metric(metric: CatalogVectorMetric) -> RuntimeVectorMetric {
    match metric {
        CatalogVectorMetric::Cosine => RuntimeVectorMetric::Cosine,
        CatalogVectorMetric::NegativeDot => RuntimeVectorMetric::NegativeDot,
        CatalogVectorMetric::SquaredL2 => RuntimeVectorMetric::SquaredL2,
    }
}

fn map_doc_value_error(error: &hyphae_native_runtime::DocValueError) -> ProductError {
    use hyphae_native_runtime::DocValueError;
    match error {
        DocValueError::InvalidHitLimit { .. }
        | DocValueError::CandidateLimit { .. }
        | DocValueError::MatchLimit { .. }
        | DocValueError::ShapeLimit { .. }
        | DocValueError::ValueTooLarge { .. }
        | DocValueError::InvalidFacetLimit { .. }
        | DocValueError::FacetTermLimit { .. } => limit_exceeded(),
        _ => invalid_request(),
    }
}

fn map_runtime_error(error: NativeRuntimeError) -> ProductError {
    match error {
        NativeRuntimeError::Ann(_)
        | NativeRuntimeError::Model(_)
        | NativeRuntimeError::InvalidPreparedMutation => invalid_request(),
        other => other.into(),
    }
}

fn invalid_request() -> ProductError {
    ProductError::from_code(ProductErrorCode::InvalidRequest)
}

fn limit_exceeded() -> ProductError {
    ProductError::from_code(ProductErrorCode::LimitExceeded)
}

fn idempotency_conflict() -> ProductError {
    ProductError::from_code(ProductErrorCode::IdempotencyConflict)
}

fn corruption() -> ProductError {
    ProductError::from_code(ProductErrorCode::Corruption)
}
