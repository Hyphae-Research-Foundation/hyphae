// SPDX-License-Identifier: Apache-2.0

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
use hyphae_native_types::{CanonicalF64, LogicalType, TransactionId};

use crate::{
    NativeProduct, ProductCommitReceipt, ProductDurability, ProductError, ProductErrorCode,
    ProductSnapshot, SnapshotIdentity,
};

pub use hyphae_native_runtime::{
    DocValue as ProductDocValue, DocValueAggregation as ProductAggregation,
    DocValueAggregationValue as ProductAggregationValue, DocValueFilter as ProductSearchFilter,
    DocValueOperator as ProductSearchOperator, DocValueSort as ProductSearchSort,
    DocValueSortDirection as ProductSortDirection, DocValueSortSource as ProductSortSource,
    FacetBucket as ProductFacetBucket, FacetRange as ProductFacetRange,
    FacetRequest as ProductFacetRequest, FacetResult as ProductFacetResult,
    MissingPlacement as ProductMissingPlacement,
    NamedDocValueAggregation as ProductNamedAggregation,
    NamedDocValueAggregationValue as ProductNamedAggregationValue,
    RangeFacetRequest as ProductRangeFacetRequest, Vector as ProductVector,
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
/// Raised from 10,000 on the R-track evidence chain (posting-index
/// eligibility, pinned posting scorer, cached snapshot state, and the
/// sealed `FiQA` relevance receipt); the next rung is evidence-gated.
pub const MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS: usize = 100_000;
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

/// One stable-ID ordered page of complete integrated search documents.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductDocumentPage {
    /// Immutable snapshot shared by every returned document.
    pub snapshot: SnapshotIdentity,
    /// Complete documents in ascending object-ID order.
    pub documents: Vec<ProductDocument>,
    /// Last returned object ID when another page remains.
    pub continuation: Option<crate::ObjectId>,
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
    /// Optional term-admission operator. Absent keeps pure OR semantics.
    pub operator: Option<ProductLexicalOperator>,
    /// Treat the final analyzed query term as a prefix and expand it to
    /// every distinct indexed term starting with it (bounded). Mutually
    /// exclusive with `operator`.
    pub prefix: bool,
    /// Ordered weighted field boosts switching the branch to versioned
    /// BM25F. Empty keeps single-field BM25. Mutually exclusive with
    /// `operator` and `prefix`.
    pub fields: Vec<ProductLexicalFieldBoost>,
    /// Optional Levenshtein character-edit distance (`1..=2`) expanding
    /// every analyzed query term over the bounded committed vocabulary.
    /// Mutually exclusive with `operator`, `prefix`, and `fields`.
    pub fuzzy: Option<usize>,
}

/// Maximum Levenshtein distance one fuzzy expansion may declare.
pub const MAX_LEXICAL_FUZZY_DISTANCE: usize = 2;

/// One weighted BM25F field boost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductLexicalFieldBoost {
    /// `body` for the canonical indexed text, or a string doc-value field.
    pub field: String,
    /// Positive weight in micros; one million is a whole weight.
    pub weight_micros: u32,
}

/// Reserved field name scoring the canonical indexed source text.
pub const LEXICAL_BODY_FIELD: &str = "body";

/// Maximum distinct terms one prefix may expand to.
pub const MAX_LEXICAL_PREFIX_TERMS: usize = 64;

/// Maximum `minimum_match` distinct terms in one lexical operator.
pub const MAX_LEXICAL_MINIMUM_MATCH: usize = 64;

/// Term-admission operator over the analyzed query terms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductLexicalOperator {
    /// Admit only candidates containing every distinct analyzed term.
    And,
    /// Admit candidates containing at least this many distinct analyzed
    /// terms (`1..=64`).
    Or {
        /// Minimum distinct-term matches required.
        minimum_match: usize,
    },
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
    /// Optional finite nonnegative distance cutoff: hits strictly farther
    /// than this canonical metric distance are discarded before fusion.
    pub max_distance: Option<CanonicalF64>,
}

/// Branch-combination method for the fused relevance score.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductFusionMethod {
    /// Normalized score blend: a lexical candidate contributes its branch
    /// weight times its score divided by the branch's top score, and a
    /// vector candidate contributes its branch weight times the bounded
    /// similarity `1 / (1 + distance)`.
    WeightedScore,
    /// Min-max relative-score blend: each branch normalizes its admitted
    /// candidates to `[0, 1]` over that branch's own score range before
    /// weighting, so the best admitted candidate contributes exactly the
    /// branch weight and the worst contributes zero regardless of scale.
    /// A branch whose candidates all share one score contributes the full
    /// weight for each.
    RelativeScore,
}

/// Bounded first-k-per-parent deduplication over the final ranking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductParentDedupe {
    /// Doc-value field holding the parent identity. Hits missing the field
    /// are never deduplicated.
    pub field: String,
    /// Hits retained per distinct parent value, within `1..=100`.
    pub first_k: usize,
}

/// Maximum hits retained per parent by deduplication.
pub const MAX_PARENT_DEDUPE_FIRST_K: usize = 100;

/// Maximum externally reranked entries in one request.
pub const MAX_RERANK_ENTRIES: usize = 256;

/// Maximum highlighted fragments per hit.
pub const MAX_HIGHLIGHT_FRAGMENTS: usize = 4;
/// Maximum normalized-text bytes per highlighted fragment.
pub const MAX_HIGHLIGHT_FRAGMENT_BYTES: usize = 512;
/// Minimum normalized-text bytes per highlighted fragment.
pub const MIN_HIGHLIGHT_FRAGMENT_BYTES: usize = 16;

/// Budgeted deterministic highlighting over the final hits.
///
/// Fragments are cut from the canonical analyzer's normalized text of each
/// hit's indexed source, around tokens equal to the analyzed query terms.
/// Extraction is a pure function of committed text, the query, and this
/// budget — it never touches the wire encoding of proofs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductHighlight {
    /// Fragments retained per hit, within `1..=4`.
    pub max_fragments: usize,
    /// Normalized-text byte budget per fragment, within `16..=512`.
    pub fragment_bytes: usize,
}

/// An attested external rerank applied over the final ranking.
///
/// The scores come from a model stage outside the engine — the attested
/// local tool or a declared provider — together with the attestation
/// envelope that binds how they were produced. The engine reorders
/// deterministically and seals the attestation class in the proof; it never
/// runs the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductRerankStage {
    /// Canonical `HYATTS01` attestation envelope for the score source.
    pub attestation: Vec<u8>,
    /// Externally computed scores by document identity.
    pub scores: Vec<(crate::ObjectId, f64)>,
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
    /// Complete candidate-set range facets over numeric fields.
    pub range_facets: Vec<ProductRangeFacetRequest>,
    /// Complete candidate-set metric aggregations.
    pub aggregations: Vec<ProductNamedAggregation>,
    /// Maximum final hits.
    pub limit: usize,
    /// Branch-combination method. Absent means the deterministic
    /// rank-based weighted reciprocal-rank fusion.
    pub fusion: Option<ProductFusionMethod>,
    /// Optional first-k-per-parent deduplication over the final ranking.
    pub parent_dedupe: Option<ProductParentDedupe>,
    /// Optional attested external rerank over the final ranking.
    pub rerank: Option<ProductRerankStage>,
    /// Optional budgeted highlighting over the final hits.
    pub highlight: Option<ProductHighlight>,
    /// Optional knee-detection truncation of the score-ordered ranking:
    /// cut immediately before the N-th strict local maximum of the
    /// normalized score curve's deviation from uniform linear decay.
    /// Runs after reranking/deduplication and before the final limit.
    pub autocut: Option<usize>,
    /// Leading hits skipped from the final ranking before the limit
    /// window. `offset + limit` must stay within the bounded ranking
    /// ceiling.
    pub offset: usize,
}

/// Maximum autocut extremum count in one request.
pub const MAX_AUTOCUT_STEEPNESS: usize = 16;

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
    /// Budgeted normalized-text fragments, present only when requested.
    pub fragments: Vec<String>,
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
    /// Range facets in request order, one bucket per declared range.
    pub range_facets: Vec<ProductFacetResult>,
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
    #[allow(clippy::too_many_lines)]
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
        if let Some(encoded) = current.structure_get_internal(&marker_key) {
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
        let collection_was_empty = identities.is_empty();
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

        let transform = collection_lexical_transform(&definition, |id| {
            current.inner.logical_catalog_object(id).cloned()
        })?;
        for document in &batch.documents {
            let object_bytes = document.object_id.get().to_be_bytes().to_vec();
            let text = match &transform {
                None => document.text.clone(),
                Some(transform) => transform.apply(&document.text),
            };
            transaction
                .index_document(binding.lexical_index, object_bytes, text)
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
        let (covered, newly_covered) =
            posting_coverage(&transaction, collection, collection_was_empty);
        if covered {
            for document in &batch.documents {
                write_document_postings(&mut transaction, collection, document)?;
            }
        }
        if newly_covered {
            transaction
                .set(
                    posting_coverage_key(collection),
                    POSTING_COVERAGE_MAGIC.to_vec(),
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

    /// Validates and applies one complete product document inside a
    /// caller-owned write batch, so the lexical text, doc-values, vectors,
    /// manifest, and postings commit under the batch's single CSN alongside
    /// SQL and structure stages.
    ///
    /// # Errors
    ///
    /// Returns without mutation for any invalid document, unknown target,
    /// duplicate identity, or exhausted bound.
    pub fn stage_document_in_batch(
        &self,
        batch: &mut hyphae_native_runtime::NativeWriteBatch,
        collection: crate::ObjectId,
        document: &crate::ProductDocument,
        logical_time_micros: i64,
    ) -> Result<(), ProductError> {
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let definition = self.search_definition(collection)?;
        let batch_shape = ProductSearchIngestBatch {
            idempotency_id: 1,
            documents: vec![document.clone()],
        };
        validate_documents(&definition, &binding, &batch_shape)?;
        let manifest_key = manifest_key(collection);
        let existing = batch.get(&manifest_key).ok_or_else(corruption)?;
        let mut identities = decode_manifest(existing)?;
        if identities.len().saturating_add(1) > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS {
            return Err(limit_exceeded());
        }
        let replace = identities.contains(&document.object_id);
        if !replace && !identities.insert(document.object_id) {
            return Err(ProductError::from_code(ProductErrorCode::CatalogConflict));
        }
        let object_bytes = document.object_id.get().to_be_bytes().to_vec();
        let transform = collection_lexical_transform(&definition, |id| {
            batch.logical_catalog_object(id).cloned()
        })?;
        let text = match &transform {
            None => document.text.clone(),
            Some(transform) => transform.apply(&document.text),
        };
        let lexical = binding.lexical_index;
        if replace {
            batch
                .replace_document(lexical, object_bytes.clone(), text)
                .map_err(map_runtime_error)?;
            let encoded_previous = batch
                .get(&document_key(collection, document.object_id))
                .ok_or_else(corruption)?
                .to_vec();
            delete_document_postings(batch, collection, document.object_id, &encoded_previous)?;
        } else {
            batch
                .index_document(lexical, object_bytes.clone(), text)
                .map_err(map_runtime_error)?;
        }
        for vector_binding in &binding.vectors {
            if let Some(vector) = document.vectors.get(&vector_binding.name) {
                batch
                    .upsert_vector(vector_binding.index, document.object_id, vector.clone())
                    .map_err(map_runtime_error)?;
            }
        }
        batch
            .set(
                document_key(collection, document.object_id),
                encode_document(document)?,
                None,
            )
            .map_err(map_runtime_error)?;
        write_document_postings(batch, collection, document)?;
        batch
            .set(manifest_key, encode_manifest(&identities)?, None)
            .map_err(map_runtime_error)?;
        Ok(())
    }

    /// Atomically replaces one existing integrated document across lexical,
    /// named-vector, and doc-value state.
    ///
    /// # Errors
    ///
    /// Returns a stable request, catalog, limit, conflict, storage, or
    /// durability error. Validation failures publish no partial mutation.
    #[allow(clippy::too_many_lines)]
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
        let catalog = self.catalog_snapshot()?;
        let transform = collection_lexical_transform(&definition, |id| {
            self.catalog_describe(&catalog, id).ok().flatten()
        })?;
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
        let (postings_covered, _) = posting_coverage(&transaction, collection, false);
        if postings_covered {
            let previous = transaction
                .get(&document_key(collection, update.document.object_id))
                .ok_or_else(corruption)?
                .to_vec();
            delete_document_postings(
                &mut transaction,
                collection,
                update.document.object_id,
                &previous,
            )?;
        }
        let object_bytes = update.document.object_id.get().to_be_bytes().to_vec();
        let text = match &transform {
            None => update.document.text.clone(),
            Some(transform) => transform.apply(&update.document.text),
        };
        transaction
            .replace_document(binding.lexical_index, object_bytes, text)
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
        if postings_covered {
            write_document_postings(&mut transaction, collection, &update.document)?;
        }
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
        let (postings_covered, _) = posting_coverage(&transaction, collection, false);
        if postings_covered {
            let previous = transaction
                .get(&document_key(collection, delete.object_id))
                .ok_or_else(corruption)?
                .to_vec();
            delete_document_postings(&mut transaction, collection, delete.object_id, &previous)?;
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
        let Some(encoded) =
            snapshot.structure_get_internal(&idempotency_key(collection, idempotency_id))
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
        self.search_collection_with_checkpoint(collection, request, logical_time_micros, || Ok(()))
    }

    /// Reads one bounded, stable-ID ordered page of complete integrated
    /// documents from a caller-owned immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing binding, an invalid limit, corrupt side
    /// records, or inconsistent lexical/vector state.
    pub fn search_documents_at_snapshot(
        snapshot: &ProductSnapshot,
        collection: crate::ObjectId,
        start_after: Option<crate::ObjectId>,
        limit: usize,
    ) -> Result<ProductDocumentPage, ProductError> {
        if limit == 0 || limit > MAX_PRODUCT_SEARCH_HITS {
            return Err(limit_exceeded());
        }
        let binding = Self::search_collection_binding_at_snapshot(snapshot, collection)?;
        let manifest = load_manifest_ids(snapshot, collection)?;
        let mut selected = manifest
            .iter()
            .copied()
            .filter(|object_id| start_after.is_none_or(|start| *object_id > start))
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>();
        let continuation = (selected.len() > limit)
            .then(|| selected.get(limit.saturating_sub(1)).copied())
            .flatten();
        selected.truncate(limit);

        let mut vector_values = BTreeMap::new();
        for vector in &binding.vectors {
            let records = snapshot
                .inner
                .vector_records(vector.index)
                .map_err(map_runtime_error)?;
            vector_values.insert(
                vector.name.clone(),
                records
                    .into_iter()
                    .map(|record| (record.object_id, record.vector))
                    .collect::<BTreeMap<_, _>>(),
            );
        }

        let documents = selected
            .into_iter()
            .map(|object_id| {
                let text = snapshot
                    .inner
                    .search_document_text(binding.lexical_index, &object_id.get().to_be_bytes())
                    .ok_or_else(corruption)?
                    .to_owned();
                let encoded = snapshot
                    .structure_get_internal(&document_key(collection, object_id))
                    .ok_or_else(corruption)?;
                let doc_values = decode_document(encoded, object_id)?;
                let vectors = vector_values
                    .iter()
                    .filter_map(|(name, records)| {
                        records
                            .get(&object_id)
                            .cloned()
                            .map(|vector| (name.clone(), vector))
                    })
                    .collect();
                Ok(ProductDocument {
                    object_id,
                    text,
                    doc_values,
                    vectors,
                })
            })
            .collect::<Result<Vec<_>, ProductError>>()?;

        Ok(ProductDocumentPage {
            snapshot: snapshot.identity(),
            documents,
            continuation,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn search_collection_with_checkpoint(
        &self,
        collection: crate::ObjectId,
        request: &ProductSearchRequest,
        logical_time_micros: i64,
        mut checkpoint: impl FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductSearchResult, ProductError> {
        checkpoint()?;
        let binding = self.resolve_search_collection_binding(collection, logical_time_micros)?;
        let definition = self.search_definition(collection)?;
        validate_search_request(&definition, &binding, request)?;
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let manifest_ids = load_manifest_ids(&snapshot, collection)?;
        let total_documents = manifest_ids.len();
        let (eligible_ids, source) = resolve_eligibility_with_checkpoint(
            &snapshot,
            collection,
            &request.filter,
            &manifest_ids,
            &mut checkpoint,
        )?;
        let transform = collection_lexical_transform(&definition, |id| {
            snapshot.inner.logical_catalog_object(id).cloned()
        })?;
        let mut fused = BTreeMap::<crate::ObjectId, f64>::new();
        let lexical_candidates = execute_lexical_branch(
            &self.database,
            &snapshot,
            collection,
            &source,
            binding.lexical_index,
            request.lexical.as_ref(),
            collection_bm25_parameters(&definition),
            request.fusion,
            transform.as_ref(),
            &eligible_ids,
            &mut fused,
            &mut checkpoint,
        )?;
        let vector_receipts = execute_vector_branches(
            &snapshot,
            &binding,
            &definition,
            &request.vectors,
            request.fusion,
            &eligible_ids,
            &mut fused,
            &mut checkpoint,
        )?;

        if request.lexical.is_none() && request.vectors.is_empty() {
            for object_id in &eligible_ids {
                checkpoint()?;
                fused.insert(*object_id, 0.0);
            }
        }
        let mut candidates = Vec::with_capacity(fused.len());
        for (object_id, score) in fused {
            checkpoint()?;
            candidates.push(hyphae_native_runtime::DocValueCandidate {
                document_id: object_id.get().to_be_bytes().to_vec(),
                score,
                values: source.values_of(&snapshot, collection, object_id)?,
            });
        }
        let retrieval_candidates = candidates.len();
        let doc_request = hyphae_native_runtime::DocValueRequest {
            filter: request.filter.clone(),
            sort: request.sort.clone(),
            limit: if request.parent_dedupe.is_some() || request.rerank.is_some() {
                // Deduplication and reranking need the complete bounded
                // ranking before the final truncation.
                hyphae_native_runtime::MAX_DOC_VALUE_HITS
            } else {
                request.offset.saturating_add(request.limit)
            },
            facets: request.facets.clone(),
            range_facets: request.range_facets.clone(),
            aggregations: request.aggregations.clone(),
        };
        let mut result = execute_doc_values(&candidates, &doc_request, &doc_value_limits())
            .map_err(|error| map_doc_value_error(&error))?;
        let window = request.offset.saturating_add(request.limit);
        if let Some(stage) = &request.rerank {
            apply_rerank(&mut result.hits, stage)?;
        }
        if let Some(dedupe) = &request.parent_dedupe {
            result.hits = apply_parent_dedupe(result.hits, dedupe, window)?;
        } else if request.rerank.is_some() {
            result.hits.truncate(window);
        }
        if let Some(steepness) = request.autocut
            && request.sort.is_empty()
        {
            let cut = autocut_extremum(
                &result.hits.iter().map(|hit| hit.score).collect::<Vec<_>>(),
                steepness,
            );
            result.hits.truncate(cut);
        }
        if request.offset > 0 {
            result.hits = result.hits.split_off(request.offset.min(result.hits.len()));
        }
        result.hits.truncate(request.limit);
        checkpoint()?;
        let approximate = vector_receipts.iter().any(|receipt| receipt.approximate);
        Ok(ProductSearchResult {
            snapshot: snapshot.identity(),
            hits: integrated_hits(
                result.hits,
                &snapshot,
                binding.lexical_index,
                request,
                transform.as_ref(),
            )?,
            facets: result.facets,
            range_facets: result.range_facets,
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
        product: &Self,
        snapshot: &crate::ProductSnapshot,
        collection: crate::ObjectId,
        request: &ProductSearchRequest,
    ) -> Result<ProductSearchResult, ProductError> {
        let binding = Self::search_collection_binding_at_snapshot(snapshot, collection)?;
        let definition = Self::search_definition_at_snapshot(snapshot, collection)?;
        validate_search_request(&definition, &binding, request)?;
        let manifest_ids = load_manifest_ids(snapshot, collection)?;
        let total_documents = manifest_ids.len();
        let (eligible_ids, source) = resolve_eligibility_with_checkpoint(
            snapshot,
            collection,
            &request.filter,
            &manifest_ids,
            &mut || Ok(()),
        )?;
        let transform = collection_lexical_transform(&definition, |id| {
            snapshot.inner.logical_catalog_object(id).cloned()
        })?;
        let mut fused = BTreeMap::<crate::ObjectId, f64>::new();
        let lexical_candidates = execute_lexical_branch(
            &product.database,
            snapshot,
            collection,
            &source,
            binding.lexical_index,
            request.lexical.as_ref(),
            collection_bm25_parameters(&definition),
            request.fusion,
            transform.as_ref(),
            &eligible_ids,
            &mut fused,
            &mut || Ok(()),
        )?;
        let vector_receipts = execute_vector_branches(
            snapshot,
            &binding,
            &definition,
            &request.vectors,
            request.fusion,
            &eligible_ids,
            &mut fused,
            &mut || Ok(()),
        )?;
        if request.lexical.is_none() && request.vectors.is_empty() {
            for object_id in &eligible_ids {
                fused.insert(*object_id, 0.0);
            }
        }
        let candidates = fused
            .into_iter()
            .map(|(object_id, score)| {
                Ok(hyphae_native_runtime::DocValueCandidate {
                    document_id: object_id.get().to_be_bytes().to_vec(),
                    score,
                    values: source.values_of(snapshot, collection, object_id)?,
                })
            })
            .collect::<Result<Vec<_>, ProductError>>()?;
        let mut result = execute_doc_values(
            &candidates,
            &hyphae_native_runtime::DocValueRequest {
                filter: request.filter.clone(),
                sort: request.sort.clone(),
                limit: if request.parent_dedupe.is_some() || request.rerank.is_some() {
                    hyphae_native_runtime::MAX_DOC_VALUE_HITS
                } else {
                    request.limit
                },
                facets: request.facets.clone(),
                range_facets: Vec::new(),
                aggregations: request.aggregations.clone(),
            },
            &doc_value_limits(),
        )
        .map_err(|error| map_doc_value_error(&error))?;
        if let Some(stage) = &request.rerank {
            apply_rerank(&mut result.hits, stage)?;
        }
        if let Some(dedupe) = &request.parent_dedupe {
            result.hits = apply_parent_dedupe(result.hits, dedupe, request.limit)?;
        } else if request.rerank.is_some() {
            result.hits.truncate(request.limit);
        }
        Ok(ProductSearchResult {
            snapshot: snapshot.identity(),
            hits: integrated_hits(
                result.hits,
                snapshot,
                binding.lexical_index,
                request,
                transform.as_ref(),
            )?,
            facets: result.facets,
            range_facets: result.range_facets,
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
            .structure_get_internal(&binding_key(collection))
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
            .structure_get_internal(&binding_key(collection))
            .ok_or_else(|| {
                ProductError::from_code(ProductErrorCode::ObjectNotFound).with_object_id(collection)
            })?;
        let binding = decode_binding(encoded)?;
        let definition = self.search_definition(collection)?;
        validate_binding_definition(&definition, &binding)?;
        Ok(binding)
    }
}

/// Scores the boosted branch with versioned BM25F over the bounded
/// committed corpus: `body` reads the canonical indexed text, every
/// other declared field reads its exact string doc value (missing or
/// non-string reads as the empty field). Nano scores map to
/// `score_nanos / 1e9` so fusion sees ordinary nonnegative floats.
fn execute_bm25f_branch(
    snapshot: &ProductSnapshot,
    collection: crate::ObjectId,
    source: &DocumentSource,
    index: crate::ObjectId,
    lexical: &ProductLexicalBranch,
    query: &str,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<Vec<hyphae_native_runtime::MatchHit>, ProductError> {
    let corpus = snapshot
        .inner
        .search_documents(index)
        .ok_or_else(invalid_request)?;
    let mut documents = Vec::with_capacity(corpus.len());
    for (document_id, body) in corpus {
        checkpoint()?;
        let object_id = decode_object_id(&document_id)?;
        let values = source.values_of(snapshot, collection, object_id)?;
        let fields = lexical
            .fields
            .iter()
            .map(|boost| {
                if boost.field == LEXICAL_BODY_FIELD {
                    body.clone()
                } else {
                    match values.get(&boost.field) {
                        Some(ProductDocValue::String(value)) => value.clone(),
                        _ => String::new(),
                    }
                }
            })
            .collect();
        documents.push(hyphae_native_runtime::bm25f::Bm25fDocument {
            key: document_id,
            fields,
        });
    }
    let fields = lexical
        .fields
        .iter()
        .map(|boost| hyphae_native_runtime::bm25f::Bm25fField {
            weight_micros: boost.weight_micros,
        })
        .collect::<Vec<_>>();
    if hyphae_native_runtime::CanonicalAnalyzer::analyze(query)
        .tokens
        .is_empty()
    {
        return Ok(Vec::new());
    }
    let matches = hyphae_native_runtime::bm25f::score_bm25f(
        &documents,
        &fields,
        query,
        lexical.candidate_limit,
    )
    .map_err(|_| invalid_request())?;
    Ok(matches
        .into_iter()
        .map(|entry| hyphae_native_runtime::MatchHit {
            document_id: entry.key,
            score: integer_nanos_as_f64(entry.score_nanos),
        })
        .collect())
}

/// Deterministic nanos -> f64 through halved integer arithmetic.
fn integer_nanos_as_f64(nanos: i64) -> f64 {
    let negative = nanos < 0;
    let magnitude = nanos.unsigned_abs();
    let upper = u32::try_from(magnitude >> 32).unwrap_or(u32::MAX);
    let lower = u32::try_from(magnitude & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    let mut value = f64::from(upper) * 4_294_967_296.0 + f64::from(lower);
    if negative {
        value = -value;
    }
    value / 1_000_000_000.0
}

/// Rewrites the final analyzed query term as its bounded prefix
/// expansion; earlier terms stay exact. No expansion leaves the branch
/// query empty (scoring nothing); overflow fails closed.
fn expand_prefix_query(
    snapshot: &ProductSnapshot,
    index: crate::ObjectId,
    query: &str,
) -> Result<String, ProductError> {
    let analysis = hyphae_native_runtime::CanonicalAnalyzer::analyze(query);
    let Some(last) = analysis.tokens.last() else {
        return Ok(String::new());
    };
    let expansion =
        match snapshot
            .inner
            .search_expand_term_prefix(index, &last.term, MAX_LEXICAL_PREFIX_TERMS)
        {
            hyphae_native_runtime::TermPrefixExpansion::UnknownIndex => {
                return Err(invalid_request());
            }
            hyphae_native_runtime::TermPrefixExpansion::Overflow => return Err(limit_exceeded()),
            hyphae_native_runtime::TermPrefixExpansion::Terms(terms) => terms,
        };
    let mut terms: Vec<&str> = analysis.tokens[..analysis.tokens.len() - 1]
        .iter()
        .map(|token| token.term.as_str())
        .collect();
    terms.extend(expansion.iter().map(String::as_str));
    Ok(terms.join(" "))
}

/// Rewrites every analyzed query term as its bounded fuzzy expansion
/// over the committed vocabulary; empty expansions contribute nothing.
fn expand_fuzzy_query(
    snapshot: &ProductSnapshot,
    index: crate::ObjectId,
    query: &str,
    max_distance: usize,
) -> Result<String, ProductError> {
    let analysis = hyphae_native_runtime::CanonicalAnalyzer::analyze(query);
    let mut collected = BTreeSet::new();
    for token in &analysis.tokens {
        match snapshot.inner.search_expand_term_fuzzy(
            index,
            &token.term,
            max_distance,
            MAX_LEXICAL_PREFIX_TERMS,
            &mut collected,
        ) {
            hyphae_native_runtime::TermPrefixExpansion::UnknownIndex => {
                return Err(invalid_request());
            }
            hyphae_native_runtime::TermPrefixExpansion::Overflow => {
                return Err(limit_exceeded());
            }
            hyphae_native_runtime::TermPrefixExpansion::Terms(_) => {}
        }
    }
    Ok(collected.into_iter().collect::<Vec<_>>().join(" "))
}

/// Per-term membership sets and the required distinct-match minimum.
type LexicalMembership = (Vec<BTreeSet<crate::ObjectId>>, usize);

/// Complete bounded per-term membership sets for the lexical operator.
///
/// `None` when no operator applies. Each distinct analyzed term's full
/// match set must fit the branch candidate bound; a set that reaches the
/// bound fails closed as limit-exceeded rather than answering from a
/// truncated set.
fn lexical_operator_membership(
    database: &hyphae_native_runtime::NativeDatabase,
    snapshot: &ProductSnapshot,
    index: crate::ObjectId,
    query: &str,
    operator: Option<ProductLexicalOperator>,
    parameters: hyphae_native_runtime::Bm25ScoreParameters,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<Option<LexicalMembership>, ProductError> {
    let Some(operator) = operator else {
        return Ok(None);
    };
    let analysis = hyphae_native_runtime::CanonicalAnalyzer::analyze(query);
    let terms: BTreeSet<&str> = analysis
        .tokens
        .iter()
        .map(|token| token.term.as_str())
        .collect();
    if terms.is_empty() {
        return Ok(Some((Vec::new(), usize::from(!terms.is_empty()))));
    }
    let minimum = match operator {
        ProductLexicalOperator::And => terms.len(),
        ProductLexicalOperator::Or { minimum_match } => minimum_match,
    };
    let mut memberships = Vec::with_capacity(terms.len());
    for term in terms {
        checkpoint()?;
        let hits = match database.match_text_at_snapshot(
            &snapshot.inner,
            index,
            term,
            MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES,
            parameters,
        ) {
            Ok(hits) => hits,
            Err(_) => snapshot
                .inner
                .match_text_with_parameters(
                    index,
                    term,
                    MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES,
                    parameters,
                )
                .map_err(map_runtime_error)?,
        };
        if hits.len() >= MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES {
            return Err(limit_exceeded());
        }
        let members = hits
            .into_iter()
            .map(|hit| decode_object_id(&hit.document_id))
            .collect::<Result<BTreeSet<_>, ProductError>>()?;
        memberships.push(members);
    }
    Ok(Some((memberships, minimum)))
}

#[allow(clippy::too_many_arguments)]
fn execute_lexical_branch(
    database: &hyphae_native_runtime::NativeDatabase,
    snapshot: &ProductSnapshot,
    collection: crate::ObjectId,
    source: &DocumentSource,
    index: crate::ObjectId,
    lexical: Option<&ProductLexicalBranch>,
    parameters: hyphae_native_runtime::Bm25ScoreParameters,
    fusion: Option<ProductFusionMethod>,
    transform: Option<&crate::lexical_analyzer::LexicalTransform>,
    eligible: &BTreeSet<crate::ObjectId>,
    fused: &mut BTreeMap<crate::ObjectId, f64>,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<usize, ProductError> {
    let Some(lexical) = lexical else {
        return Ok(0);
    };
    let query = match transform {
        None => lexical.query.clone(),
        Some(transform) => transform.apply(&lexical.query),
    };
    let query = if lexical.prefix {
        expand_prefix_query(snapshot, index, &query)?
    } else if let Some(distance) = lexical.fuzzy {
        expand_fuzzy_query(snapshot, index, &query, distance)?
    } else {
        query
    };
    let query = query.as_str();
    // The durable posting scorer is bit-identical to the retained model; a
    // reclaimed page generation or inline-format directory falls open to
    // the model, never to a different answer.
    let hits = if lexical.fields.is_empty() {
        match database.match_text_at_snapshot(
            &snapshot.inner,
            index,
            query,
            lexical.candidate_limit,
            parameters,
        ) {
            Ok(hits) => hits,
            Err(_) => snapshot
                .inner
                .match_text_with_parameters(index, query, lexical.candidate_limit, parameters)
                .map_err(map_runtime_error)?,
        }
    } else {
        execute_bm25f_branch(
            snapshot, collection, source, index, lexical, query, checkpoint,
        )?
    };
    let required = lexical_operator_membership(
        database,
        snapshot,
        index,
        query,
        lexical.operator,
        parameters,
        checkpoint,
    )?;
    let mut admitted_hits = Vec::new();
    for (rank, hit) in hits.into_iter().enumerate() {
        checkpoint()?;
        let object_id = decode_object_id(&hit.document_id)?;
        let operator_admits = match &required {
            None => true,
            Some((memberships, minimum)) => {
                memberships
                    .iter()
                    .filter(|members| members.contains(&object_id))
                    .count()
                    >= *minimum
            }
        };
        if operator_admits && eligible.contains(&object_id) {
            admitted_hits.push((rank, object_id, hit.score));
        }
    }
    let admitted = admitted_hits.len();
    let top_score = admitted_hits.first().map_or(0.0, |(_, _, score)| *score);
    let (branch_min, branch_max) = score_bounds(admitted_hits.iter().map(|(_, _, score)| *score));
    for (rank, object_id, score) in admitted_hits {
        checkpoint()?;
        match fusion {
            None => add_rrf(fused, object_id, lexical.weight, rank)?,
            Some(ProductFusionMethod::WeightedScore) => {
                let normalized = if top_score > 0.0 && score >= 0.0 {
                    score / top_score
                } else {
                    0.0
                };
                add_weighted_score(fused, object_id, lexical.weight, normalized)?;
            }
            Some(ProductFusionMethod::RelativeScore) => {
                let normalized = min_max_normalized(score, branch_min, branch_max);
                add_weighted_score(fused, object_id, lexical.weight, normalized)?;
            }
        }
    }
    Ok(admitted)
}

/// Finite `(min, max)` bounds over one branch's admitted scores.
fn score_bounds(scores: impl Iterator<Item = f64>) -> (f64, f64) {
    let mut bounds: Option<(f64, f64)> = None;
    for score in scores {
        bounds = Some(match bounds {
            None => (score, score),
            Some((low, high)) => (low.min(score), high.max(score)),
        });
    }
    bounds.unwrap_or((0.0, 0.0))
}

/// Min-max normalization to `[0, 1]`; a degenerate range contributes `1.0`
/// so an all-equal branch still contributes its full weight.
fn min_max_normalized(score: f64, min: f64, max: f64) -> f64 {
    if max > min {
        ((score - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_vector_branches(
    snapshot: &ProductSnapshot,
    binding: &ProductSearchCollectionBinding,
    definition: &SearchCollectionDefinitionV2,
    branches: &[ProductVectorBranch],
    fusion: Option<ProductFusionMethod>,
    eligible: &BTreeSet<crate::ObjectId>,
    fused: &mut BTreeMap<crate::ObjectId, f64>,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<Vec<ProductVectorBranchReceipt>, ProductError> {
    let mut receipts = Vec::with_capacity(branches.len());
    for branch in branches {
        checkpoint()?;
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
        let (mut hits, receipt) =
            execute_vector_branch(snapshot, vector_binding, vector.policy, branch, eligible)?;
        if hits
            .iter()
            .any(|hit| !hit.distance.is_finite() || hit.distance < 0.0)
        {
            return Err(invalid_request());
        }
        if let Some(cutoff) = branch.max_distance {
            hits.retain(|hit| hit.distance <= cutoff.get());
        }
        let (branch_min, branch_max) = score_bounds(hits.iter().map(|hit| hit.distance));
        for (rank, hit) in hits.into_iter().enumerate() {
            checkpoint()?;
            match fusion {
                None => add_rrf(fused, hit.object_id, branch.weight, rank)?,
                Some(ProductFusionMethod::WeightedScore) => {
                    let normalized = 1.0 / (1.0 + hit.distance);
                    add_weighted_score(fused, hit.object_id, branch.weight, normalized)?;
                }
                Some(ProductFusionMethod::RelativeScore) => {
                    // Distances rank ascending: the nearest admitted hit
                    // contributes the full weight, the farthest zero.
                    let normalized =
                        min_max_normalized(branch_max - hit.distance, 0.0, branch_max - branch_min);
                    add_weighted_score(fused, hit.object_id, branch.weight, normalized)?;
                }
            }
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
        // The exact branch returns earlier in this function; treat an
        // impossible residue as an invalid request instead of panicking.
        ProductVectorExecution::Exact => return Err(invalid_request()),
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
        range_facets: Vec::new(),
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

/// Reorders the final ranking by the externally attested scores: scored
/// hits sort by score descending (ties break on the stable identity),
/// unscored hits follow in their existing order. Deterministic and bounded.
fn apply_rerank(
    hits: &mut [hyphae_native_runtime::DocValueCandidate],
    stage: &ProductRerankStage,
) -> Result<(), ProductError> {
    let mut scores = BTreeMap::new();
    for (object_id, score) in &stage.scores {
        if !score.is_finite() || scores.insert(*object_id, *score).is_some() {
            return Err(invalid_request());
        }
    }
    hits.sort_by(|left, right| {
        let left_score = decode_object_id(&left.document_id)
            .ok()
            .and_then(|id| scores.get(&id).copied());
        let right_score = decode_object_id(&right.document_id)
            .ok()
            .and_then(|id| scores.get(&id).copied());
        match (left_score, right_score) {
            (Some(left_value), Some(right_value)) => right_value
                .total_cmp(&left_value)
                .then_with(|| left.document_id.cmp(&right.document_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    Ok(())
}

fn validate_rerank(request: &ProductSearchRequest) -> Result<(), ProductError> {
    if let Some(stage) = &request.rerank
        && (stage.scores.is_empty()
            || stage.scores.len() > MAX_RERANK_ENTRIES
            || crate::proof::attestation::ModelAttestation::decode(&stage.attestation).is_err())
    {
        return Err(invalid_request());
    }
    Ok(())
}

fn validate_autocut(request: &ProductSearchRequest) -> Result<(), ProductError> {
    if let Some(steepness) = request.autocut
        && !(1..=MAX_AUTOCUT_STEEPNESS).contains(&steepness)
    {
        return Err(invalid_request());
    }
    Ok(())
}

fn validate_highlight(request: &ProductSearchRequest) -> Result<(), ProductError> {
    if let Some(highlight) = &request.highlight
        && (!(1..=MAX_HIGHLIGHT_FRAGMENTS).contains(&highlight.max_fragments)
            || !(MIN_HIGHLIGHT_FRAGMENT_BYTES..=MAX_HIGHLIGHT_FRAGMENT_BYTES)
                .contains(&highlight.fragment_bytes)
            || request.lexical.is_none())
    {
        return Err(invalid_request());
    }
    Ok(())
}

fn validate_parent_dedupe(request: &ProductSearchRequest) -> Result<(), ProductError> {
    if let Some(dedupe) = &request.parent_dedupe
        && (dedupe.field.is_empty()
            || dedupe.field.len() > 1_024
            || !(1..=MAX_PARENT_DEDUPE_FIRST_K).contains(&dedupe.first_k))
    {
        return Err(invalid_request());
    }
    Ok(())
}

/// Validates the lexical branch's bounds and mode exclusivity.
fn validate_lexical_branch(
    definition: &SearchCollectionDefinitionV2,
    request: &ProductSearchRequest,
) -> Result<(), ProductError> {
    if let Some(lexical) = &request.lexical {
        let boosted = !lexical.fields.is_empty();
        let fuzzy = lexical.fuzzy.is_some();
        let modes = usize::from(lexical.prefix)
            + usize::from(lexical.operator.is_some())
            + usize::from(boosted)
            + usize::from(fuzzy);
        let mut boost_names = BTreeSet::new();
        if !(1..=MAX_PRODUCT_SEARCH_BRANCH_CANDIDATES).contains(&lexical.candidate_limit)
            || lexical.weight == 0
            || modes > 1
            || matches!(
                lexical.fuzzy,
                Some(distance) if !(1..=MAX_LEXICAL_FUZZY_DISTANCE).contains(&distance)
            )
            || (lexical.prefix && lexical.operator.is_some())
            || (boosted && (lexical.prefix || lexical.operator.is_some()))
            || lexical.fields.len() > hyphae_native_runtime::bm25f::MAX_BM25F_FIELDS
            || lexical.fields.iter().any(|boost| {
                boost.field.is_empty()
                    || !(1..=hyphae_native_runtime::bm25f::MAX_BM25F_FIELD_WEIGHT_MICROS)
                        .contains(&boost.weight_micros)
                    || !boost_names.insert(boost.field.as_str())
                    || (boost.field != LEXICAL_BODY_FIELD
                        && !definition.fields.iter().any(|field| {
                            field.name.lookup() == boost.field
                                && field.logical_type == LogicalType::Text
                        }))
            })
            || matches!(
                lexical.operator,
                Some(ProductLexicalOperator::Or { minimum_match })
                    if !(1..=MAX_LEXICAL_MINIMUM_MATCH).contains(&minimum_match)
            )
        {
            return Err(invalid_request());
        }
    }
    Ok(())
}

fn validate_search_request(
    definition: &SearchCollectionDefinitionV2,
    binding: &ProductSearchCollectionBinding,
    request: &ProductSearchRequest,
) -> Result<(), ProductError> {
    validate_parent_dedupe(request)?;
    validate_rerank(request)?;
    validate_highlight(request)?;
    validate_autocut(request)?;
    if request.range_facets.len() > hyphae_native_runtime::MAX_DOC_VALUE_RANGE_FACETS
        || request.range_facets.iter().any(|facet| {
            facet.field.is_empty()
                || facet.ranges.is_empty()
                || facet.ranges.len() > hyphae_native_runtime::MAX_DOC_VALUE_FACET_RANGES
        })
    {
        return Err(invalid_request());
    }
    if !(1..=MAX_PRODUCT_SEARCH_HITS).contains(&request.limit)
        || request
            .offset
            .checked_add(request.limit)
            .is_none_or(|window| window > MAX_PRODUCT_SEARCH_HITS)
        || request.vectors.len() > MAX_PRODUCT_SEARCH_VECTOR_TARGETS
    {
        return Err(limit_exceeded());
    }
    validate_lexical_branch(definition, request)?;
    let mut targets = BTreeSet::new();
    for branch in &request.vectors {
        if let Some(cutoff) = branch.max_distance
            && (!cutoff.get().is_finite() || cutoff.get() < 0.0)
        {
            return Err(invalid_request());
        }
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
        range_facets: Vec::new(),
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
        range_facets: Vec::new(),
        aggregations: request.aggregations.clone(),
    };
    execute_doc_values(&empty, &validation, &doc_value_limits())
        .map(|_| ())
        .map_err(|error| map_doc_value_error(&error))
}

fn load_documents_with_checkpoint(
    snapshot: &ProductSnapshot,
    collection: crate::ObjectId,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<Vec<hyphae_native_runtime::DocValueCandidate>, ProductError> {
    let manifest = snapshot
        .structure_get_internal(&manifest_key(collection))
        .ok_or_else(corruption)?;
    let identities = decode_manifest(manifest)?;
    if identities.len() > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS {
        return Err(corruption());
    }
    let mut documents = Vec::with_capacity(identities.len());
    for object_id in identities {
        checkpoint()?;
        let encoded = snapshot
            .structure_get_internal(&document_key(collection, object_id))
            .ok_or_else(corruption)?;
        documents.push(hyphae_native_runtime::DocValueCandidate {
            document_id: object_id.get().to_be_bytes().to_vec(),
            score: 0.0,
            values: decode_document(encoded, object_id)?,
        });
    }
    Ok(documents)
}

fn filter_documents_with_checkpoint(
    documents: &[hyphae_native_runtime::DocValueCandidate],
    filter: &ProductSearchFilter,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<Vec<hyphae_native_runtime::DocValueCandidate>, ProductError> {
    checkpoint()?;
    let request = hyphae_native_runtime::DocValueRequest {
        filter: filter.clone(),
        sort: Vec::new(),
        limit: documents.len().max(1),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
    };
    let result = execute_doc_values(documents, &request, &doc_value_limits())
        .map_err(|error| map_doc_value_error(&error))?;
    checkpoint()?;
    Ok(result.hits)
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
            | (
                ProductDocValue::Float(_),
                LogicalType::Float32 | LogicalType::Float64
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
    add_contribution(
        fused,
        object_id,
        f64::from(weight) / (RRF_CONSTANT + f64::from(rank)),
    )
}

/// Adds one weighted normalized-score contribution. Lexical candidates
/// normalize by the branch's top score; vector candidates map a canonical
/// distance to the bounded similarity `1 / (1 + distance)`.
fn add_weighted_score(
    fused: &mut BTreeMap<crate::ObjectId, f64>,
    object_id: crate::ObjectId,
    weight: u32,
    normalized: f64,
) -> Result<(), ProductError> {
    if !normalized.is_finite() || !(0.0..=1.0).contains(&normalized) {
        return Err(invalid_request());
    }
    add_contribution(fused, object_id, f64::from(weight) * normalized)
}

fn add_contribution(
    fused: &mut BTreeMap<crate::ObjectId, f64>,
    object_id: crate::ObjectId,
    contribution: f64,
) -> Result<(), ProductError> {
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
        ProductDocValue::Integer(_) | ProductDocValue::Float(_) => 8,
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

/// Magic sealing a collection's doc-value posting coverage marker.
const POSTING_COVERAGE_MAGIC: &[u8; 8] = b"HYPSPST1";
/// Posting keys longer than this are not written; the field falls back.
const MAX_POSTING_KEY_BYTES: usize = 3_900;

fn posting_coverage_key(collection: crate::ObjectId) -> Vec<u8> {
    storage_key(b'V', collection, None)
}

fn posting_field_prefix(collection: crate::ObjectId, field: &str) -> Vec<u8> {
    let mut key = storage_key(b'P', collection, None);
    key.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
    key.extend_from_slice(field.as_bytes());
    key
}

fn unindexed_field_key(collection: crate::ObjectId, field: &str) -> Vec<u8> {
    let mut key = storage_key(b'U', collection, None);
    key.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
    key.extend_from_slice(field.as_bytes());
    key
}

/// Pinned posting type tags in the doc-value total order.
/// Retains the first `first_k` hits per distinct parent value over the
/// sorted ranking, then truncates to the requested limit. Hits without the
/// parent field are never deduplicated. Grouping keys use the canonical
/// posting component encoding so equality is exact and type-bound.
/// Maps final doc-value hits to integrated hits, cutting budgeted
/// highlight fragments when the request carries a budget. Both search
/// twins share this exact mapping.
fn integrated_hits(
    hits: Vec<hyphae_native_runtime::DocValueCandidate>,
    snapshot: &ProductSnapshot,
    lexical_index: crate::ObjectId,
    request: &ProductSearchRequest,
    transform: Option<&crate::lexical_analyzer::LexicalTransform>,
) -> Result<Vec<ProductIntegratedSearchHit>, ProductError> {
    let terms = highlight_terms(request, transform);
    hits.into_iter()
        .map(|hit| {
            let fragments = match (&terms, &request.highlight) {
                (Some(terms), Some(highlight)) => snapshot
                    .inner
                    .search_document_text(lexical_index, &hit.document_id)
                    .map_or_else(Vec::new, |text| extract_fragments(text, terms, highlight)),
                _ => Vec::new(),
            };
            Ok(ProductIntegratedSearchHit {
                object_id: decode_object_id(&hit.document_id)?,
                score: hit.score,
                doc_values: hit.values,
                fragments,
            })
        })
        .collect()
}

/// Analyzed query terms for highlighting, derived from exactly the
/// transformed query string the lexical branch scores with.
fn highlight_terms(
    request: &ProductSearchRequest,
    transform: Option<&crate::lexical_analyzer::LexicalTransform>,
) -> Option<BTreeSet<String>> {
    request.highlight.as_ref()?;
    let lexical = request.lexical.as_ref()?;
    let query = match transform {
        None => lexical.query.clone(),
        Some(transform) => transform.apply(&lexical.query),
    };
    let analysis = hyphae_native_runtime::CanonicalAnalyzer::analyze(&query);
    Some(
        analysis
            .tokens
            .into_iter()
            .map(|token| token.term)
            .collect(),
    )
}

/// Cuts budgeted fragments from the canonical analyzer's normalized text
/// around tokens equal to the analyzed query terms.
///
/// Extraction is a pure deterministic function of the stored text, the
/// term set, and the budget: fragments start one quarter of the budget
/// before the first unconsumed matching token (clipped to a character
/// boundary), extend to the byte budget, and never overlap.
fn extract_fragments(
    text: &str,
    terms: &BTreeSet<String>,
    highlight: &ProductHighlight,
) -> Vec<String> {
    let analysis = hyphae_native_runtime::CanonicalAnalyzer::analyze(text);
    let normalized = analysis.normalized_text.as_str();
    let mut fragments = Vec::new();
    let mut cursor = 0_usize;
    for token in &analysis.tokens {
        if fragments.len() == highlight.max_fragments {
            break;
        }
        if token.start_offset < cursor || !terms.contains(&token.term) {
            continue;
        }
        let mut start = token
            .start_offset
            .saturating_sub(highlight.fragment_bytes / 4)
            .max(cursor);
        while start > 0 && !normalized.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = start
            .saturating_add(highlight.fragment_bytes)
            .min(normalized.len());
        while end < normalized.len() && !normalized.is_char_boundary(end) {
            end -= 1;
        }
        if end <= start {
            continue;
        }
        fragments.push(normalized[start..end].to_owned());
        cursor = end;
    }
    fragments
}

/// Knee-detection cut point over a descending score curve: normalize
/// scores to `[0, 1]` against uniform linear decay and cut immediately
/// before the `steepness`-th strict local maximum of the deviation.
/// Degenerate inputs (one hit, equal extremes) return the whole length.
fn autocut_extremum(scores: &[f64], steepness: usize) -> usize {
    if scores.len() <= 1 {
        return scores.len();
    }
    let first = scores[0];
    let last = scores[scores.len() - 1];
    let range = last - first;
    if range == 0.0 || !range.is_finite() {
        return scores.len();
    }
    let count = scores.len();
    // Hit counts are bounded far below 2^32; the lossless u32->f64 path
    // keeps the normalization exact.
    let denominator = f64::from(u32::try_from(count - 1).unwrap_or(u32::MAX));
    let step = 1.0 / denominator;
    let deviation: Vec<f64> = scores
        .iter()
        .enumerate()
        .map(|(index, score)| {
            let position = f64::from(u32::try_from(index).unwrap_or(u32::MAX));
            (score - first) / range - position * step
        })
        .collect();
    let mut extrema = 0_usize;
    for index in 1..count {
        let is_maximum = if index == count - 1 {
            count > 2
                && deviation[index] > deviation[index - 1]
                && deviation[index] > deviation[index - 2]
        } else {
            deviation[index] > deviation[index - 1] && deviation[index] > deviation[index + 1]
        };
        if is_maximum {
            extrema += 1;
            if extrema >= steepness {
                return index;
            }
        }
    }
    count
}

fn apply_parent_dedupe(
    hits: Vec<hyphae_native_runtime::DocValueCandidate>,
    dedupe: &ProductParentDedupe,
    limit: usize,
) -> Result<Vec<hyphae_native_runtime::DocValueCandidate>, ProductError> {
    let mut counts: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut retained = Vec::new();
    for hit in hits {
        let keep = match hit.values.get(&dedupe.field) {
            None => true,
            Some(value) => {
                let (tag, component) = posting_component(value)?;
                let mut key = Vec::with_capacity(1 + component.len());
                key.push(tag);
                key.extend_from_slice(&component);
                let count = counts.entry(key).or_insert(0);
                *count += 1;
                *count <= dedupe.first_k
            }
        };
        if keep {
            retained.push(hit);
            if retained.len() == limit {
                break;
            }
        }
    }
    Ok(retained)
}

fn posting_component(value: &ProductDocValue) -> Result<(u8, Vec<u8>), ProductError> {
    Ok(match value {
        ProductDocValue::Boolean(value) => (1, vec![u8::from(*value)]),
        ProductDocValue::Integer(value) => {
            (2, hyphae_native_types::encode_i64_ordered(*value).to_vec())
        }
        ProductDocValue::String(value) => (
            3,
            hyphae_native_types::encode_memcomparable_bytes(value.as_bytes())
                .map_err(|_| limit_exceeded())?,
        ),
        ProductDocValue::Bytes(value) => (
            4,
            hyphae_native_types::encode_memcomparable_bytes(value).map_err(|_| limit_exceeded())?,
        ),
        ProductDocValue::Float(value) => {
            // Sign-flip trick maps the IEEE total order onto unsigned
            // byte order: non-negative values flip the sign bit, negative
            // values flip every bit.
            let bits = value.bits();
            let ordered = if bits & (1 << 63) == 0 {
                bits ^ (1 << 63)
            } else {
                !bits
            };
            (5, ordered.to_be_bytes().to_vec())
        }
    })
}

fn posting_value_prefix(
    collection: crate::ObjectId,
    field: &str,
    value: &ProductDocValue,
) -> Result<Vec<u8>, ProductError> {
    let (tag, component) = posting_component(value)?;
    let mut key = posting_field_prefix(collection, field);
    key.push(tag);
    key.extend_from_slice(&component);
    Ok(key)
}

fn posting_key(
    collection: crate::ObjectId,
    field: &str,
    value: &ProductDocValue,
    object_id: crate::ObjectId,
) -> Result<Vec<u8>, ProductError> {
    let mut key = posting_value_prefix(collection, field, value)?;
    key.extend_from_slice(&object_id.get().to_be_bytes());
    Ok(key)
}

/// Returns the exclusive upper bound sharing `prefix`, or `None` when no
/// byte string is strictly greater (all bytes are 0xFF).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut bound = prefix.to_vec();
    while let Some(last) = bound.pop() {
        if last < u8::MAX {
            bound.push(last + 1);
            return Some(bound);
        }
    }
    None
}

fn posting_object_id(key: &[u8]) -> Result<crate::ObjectId, ProductError> {
    let suffix = key.len().checked_sub(16).ok_or_else(corruption)?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&key[suffix..]);
    crate::ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| corruption())
}

/// Emits the posting mutations for one document: keys to set live and the
/// fields whose encoded posting would exceed the bounded key budget.
fn document_posting_keys(
    collection: crate::ObjectId,
    object_id: crate::ObjectId,
    doc_values: &BTreeMap<String, ProductDocValue>,
) -> Result<(Vec<Vec<u8>>, Vec<String>), ProductError> {
    let mut keys = Vec::new();
    let mut oversized = Vec::new();
    for (field, value) in doc_values {
        let key = posting_key(collection, field, value, object_id)?;
        if key.len() > MAX_POSTING_KEY_BYTES {
            oversized.push(field.clone());
        } else {
            keys.push(key);
        }
    }
    Ok((keys, oversized))
}

/// Fields one filter references, or `None` when a node shape has none.
fn filter_fields(filter: &ProductSearchFilter, fields: &mut BTreeSet<String>) {
    match filter {
        ProductSearchFilter::MatchAll => {}
        ProductSearchFilter::Exists(field)
        | ProductSearchFilter::Compare { field, .. }
        | ProductSearchFilter::In { field, .. }
        | ProductSearchFilter::IsNull(field)
        | ProductSearchFilter::Like { field, .. } => {
            fields.insert(field.clone());
        }
        ProductSearchFilter::All(children) | ProductSearchFilter::Any(children) => {
            for child in children {
                filter_fields(child, fields);
            }
        }
        ProductSearchFilter::Not(child) => filter_fields(child, fields),
    }
}

/// Collects the visible posting document identities inside `[start, end)`.
fn posting_scan(
    snapshot: &crate::ProductSnapshot,
    start: &[u8],
    end: &[u8],
) -> Option<BTreeSet<crate::ObjectId>> {
    let keys = snapshot.structure_keys_in_range_internal(
        start,
        end,
        MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS.saturating_mul(2),
    )?;
    let mut identities = BTreeSet::new();
    for key in keys {
        identities.insert(posting_object_id(&key).ok()?);
    }
    Some(identities)
}

/// Evaluates one filter against the posting index, producing exactly the
/// eligible set the linear scan would produce, or `None` when any node
/// cannot be answered from postings (the caller falls back fail-open to the
/// scan, never to a wrong answer).
fn posting_filter_ids(
    snapshot: &crate::ProductSnapshot,
    collection: crate::ObjectId,
    filter: &ProductSearchFilter,
    manifest: &BTreeSet<crate::ObjectId>,
) -> Option<BTreeSet<crate::ObjectId>> {
    match filter {
        ProductSearchFilter::MatchAll => Some(manifest.clone()),
        ProductSearchFilter::Exists(field) => {
            let start = posting_field_prefix(collection, field);
            let end = prefix_successor(&start)?;
            posting_scan(snapshot, &start, &end)
        }
        ProductSearchFilter::Compare {
            field,
            operator,
            value,
        } => {
            let field_start = posting_field_prefix(collection, field);
            let (tag, component) = posting_component(value).ok()?;
            let mut tag_start = field_start.clone();
            tag_start.push(tag);
            let tag_end = prefix_successor(&tag_start)?;
            let mut value_start = tag_start.clone();
            value_start.extend_from_slice(&component);
            let value_end = prefix_successor(&value_start)?;
            match operator {
                ProductSearchOperator::Equal => posting_scan(snapshot, &value_start, &value_end),
                ProductSearchOperator::NotEqual => {
                    let same_type = posting_scan(snapshot, &tag_start, &tag_end)?;
                    let equal = posting_scan(snapshot, &value_start, &value_end)?;
                    Some(same_type.difference(&equal).copied().collect())
                }
                ProductSearchOperator::Less => posting_scan(snapshot, &tag_start, &value_start),
                ProductSearchOperator::LessOrEqual => {
                    posting_scan(snapshot, &tag_start, &value_end)
                }
                ProductSearchOperator::Greater => posting_scan(snapshot, &value_end, &tag_end),
                ProductSearchOperator::GreaterOrEqual => {
                    posting_scan(snapshot, &value_start, &tag_end)
                }
            }
        }
        ProductSearchFilter::All(children) => {
            let mut result = manifest.clone();
            for child in children {
                let ids = posting_filter_ids(snapshot, collection, child, manifest)?;
                result = result.intersection(&ids).copied().collect();
            }
            Some(result)
        }
        ProductSearchFilter::Any(children) => {
            let mut result = BTreeSet::new();
            for child in children {
                let ids = posting_filter_ids(snapshot, collection, child, manifest)?;
                result.extend(ids);
            }
            Some(result)
        }
        ProductSearchFilter::Not(child) => {
            let ids = posting_filter_ids(snapshot, collection, child, manifest)?;
            Some(manifest.difference(&ids).copied().collect())
        }
        // A bounded membership set is the union of its members' point scans.
        ProductSearchFilter::In { field, values } => {
            let field_start = posting_field_prefix(collection, field);
            let mut result = BTreeSet::new();
            for value in values {
                let (tag, component) = posting_component(value).ok()?;
                let mut value_start = field_start.clone();
                value_start.push(tag);
                value_start.extend_from_slice(&component);
                let value_end = prefix_successor(&value_start)?;
                result.extend(posting_scan(snapshot, &value_start, &value_end)?);
            }
            Some(result)
        }
        // Missing-field membership is the manifest minus every posting for
        // the field, mirroring the reference's negated Exists exactly.
        ProductSearchFilter::IsNull(field) => {
            let start = posting_field_prefix(collection, field);
            let end = prefix_successor(&start)?;
            let present = posting_scan(snapshot, &start, &end)?;
            Some(manifest.difference(&present).copied().collect())
        }
        // Substring shapes cannot be answered from ordered postings; the
        // caller falls back fail-open to the exact scan.
        ProductSearchFilter::Like { .. } => None,
    }
}

/// Answers eligibility from the posting index when the collection is
/// posting-covered and every referenced field is fully indexed.
fn posting_eligible_ids(
    snapshot: &crate::ProductSnapshot,
    collection: crate::ObjectId,
    filter: &ProductSearchFilter,
    manifest: &BTreeSet<crate::ObjectId>,
) -> Option<BTreeSet<crate::ObjectId>> {
    let coverage = snapshot.structure_get_internal(&posting_coverage_key(collection))?;
    if coverage != POSTING_COVERAGE_MAGIC {
        return None;
    }
    let mut fields = BTreeSet::new();
    filter_fields(filter, &mut fields);
    for field in &fields {
        if snapshot
            .structure_get_internal(&unindexed_field_key(collection, field))
            .is_some()
        {
            return None;
        }
    }
    posting_filter_ids(snapshot, collection, filter, manifest)
}

/// Writes the live postings and oversized-field markers for one document.
fn write_document_postings(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    collection: crate::ObjectId,
    document: &ProductDocument,
) -> Result<(), ProductError> {
    let (keys, oversized) =
        document_posting_keys(collection, document.object_id, &document.doc_values)?;
    for key in keys {
        transaction
            .set(key, vec![1], None)
            .map_err(map_runtime_error)?;
    }
    for field in oversized {
        transaction
            .set(unindexed_field_key(collection, &field), vec![1], None)
            .map_err(map_runtime_error)?;
    }
    Ok(())
}

/// Deletes the postings of one previously stored document encoding.
fn delete_document_postings(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    collection: crate::ObjectId,
    object_id: crate::ObjectId,
    encoded: &[u8],
) -> Result<(), ProductError> {
    let previous = decode_document(encoded, object_id)?;
    let (keys, _oversized) = document_posting_keys(collection, object_id, &previous)?;
    for key in keys {
        let _removed = transaction
            .delete_structure(key)
            .map_err(map_runtime_error)?;
    }
    Ok(())
}

/// Whether the collection maintains postings inside this transaction, and
/// whether this transaction is the one that turns coverage on.
fn posting_coverage(
    transaction: &hyphae_native_runtime::NativeWriteBatch,
    collection: crate::ObjectId,
    collection_was_empty: bool,
) -> (bool, bool) {
    if transaction.get(&posting_coverage_key(collection)).is_some() {
        return (true, false);
    }
    (collection_was_empty, collection_was_empty)
}

/// Where per-candidate doc-values come from after eligibility resolution.
enum DocumentSource {
    /// Postings answered eligibility; values load per fused candidate.
    Postings,
    /// The linear scan already materialized every document.
    Scan(BTreeMap<crate::ObjectId, hyphae_native_runtime::DocValueCandidate>),
}

impl DocumentSource {
    fn values_of(
        &self,
        snapshot: &crate::ProductSnapshot,
        collection: crate::ObjectId,
        object_id: crate::ObjectId,
    ) -> Result<BTreeMap<String, ProductDocValue>, ProductError> {
        match self {
            Self::Scan(by_id) => Ok(by_id.get(&object_id).ok_or_else(corruption)?.values.clone()),
            Self::Postings => {
                let encoded = snapshot
                    .structure_get_internal(&document_key(collection, object_id))
                    .ok_or_else(corruption)?;
                decode_document(encoded, object_id)
            }
        }
    }
}

/// Resolves eligibility from postings when possible, otherwise through the
/// materializing linear scan, preserving byte-identical semantics.
fn resolve_eligibility_with_checkpoint(
    snapshot: &crate::ProductSnapshot,
    collection: crate::ObjectId,
    filter: &ProductSearchFilter,
    manifest_ids: &BTreeSet<crate::ObjectId>,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<(BTreeSet<crate::ObjectId>, DocumentSource), ProductError> {
    if let Some(eligible) = posting_eligible_ids(snapshot, collection, filter, manifest_ids) {
        checkpoint()?;
        return Ok((eligible, DocumentSource::Postings));
    }
    let documents = load_documents_with_checkpoint(snapshot, collection, checkpoint)?;
    let eligible = filter_documents_with_checkpoint(&documents, filter, checkpoint)?;
    let mut eligible_ids = BTreeSet::new();
    for candidate in &eligible {
        checkpoint()?;
        eligible_ids.insert(decode_object_id(&candidate.document_id)?);
    }
    let mut by_id = BTreeMap::new();
    for candidate in documents {
        checkpoint()?;
        by_id.insert(decode_object_id(&candidate.document_id)?, candidate);
    }
    Ok((eligible_ids, DocumentSource::Scan(by_id)))
}

/// Loads the collection manifest identities under the read-side cap.
/// Tuned BM25 parameters from the collection definition, or the canonical
/// defaults when the definition predates tuning.
/// Resolves the configured lexical transform for one collection, or `None`
/// for the canonical identity pipeline. Fails closed on analyzer shapes the
/// transform cannot honor exactly.
fn collection_lexical_transform(
    definition: &SearchCollectionDefinitionV2,
    resolve: impl Fn(crate::ObjectId) -> Option<hyphae_native_catalog::LogicalCatalogObject>,
) -> Result<Option<crate::lexical_analyzer::LexicalTransform>, ProductError> {
    let analyzer = definition
        .fields
        .iter()
        .find(|field| {
            field.options.lexical != hyphae_native_catalog::LexicalIndexPolicy::None
                && field.analyzer.is_some()
        })
        .and_then(|field| field.analyzer);
    let Some(analyzer) = analyzer else {
        return Ok(None);
    };
    let Some(hyphae_native_catalog::LogicalCatalogObject::V2(
        hyphae_native_catalog::CatalogObjectV2::Analyzer(analyzer),
    )) = resolve(analyzer)
    else {
        return Err(corruption());
    };
    crate::lexical_analyzer::LexicalTransform::from_definition(&analyzer)
        .map_err(|_| invalid_request())
}

fn collection_bm25_parameters(
    definition: &SearchCollectionDefinitionV2,
) -> hyphae_native_runtime::Bm25ScoreParameters {
    definition.bm25.map_or_else(
        hyphae_native_runtime::Bm25ScoreParameters::default,
        |bm25| hyphae_native_runtime::Bm25ScoreParameters {
            k1_micros: bm25.k1_micros,
            b_micros: bm25.b_micros,
        },
    )
}

fn load_manifest_ids(
    snapshot: &crate::ProductSnapshot,
    collection: crate::ObjectId,
) -> Result<BTreeSet<crate::ObjectId>, ProductError> {
    let identities = decode_manifest(
        snapshot
            .structure_get_internal(&manifest_key(collection))
            .ok_or_else(corruption)?,
    )?;
    if identities.len() > MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS {
        return Err(corruption());
    }
    Ok(identities)
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
            ProductDocValue::Float(value) => {
                encoded.push(5);
                put_bytes(&mut encoded, &value.bits().to_le_bytes())?;
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
            5 if value.len() == 8 => {
                let bits = u64::from_le_bytes(value.try_into().map_err(|_| corruption())?);
                let float = hyphae_native_types::CanonicalF64::new(f64::from_bits(bits));
                if float.bits() != bits {
                    return Err(corruption());
                }
                ProductDocValue::Float(float)
            }
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
