// SPDX-License-Identifier: Apache-2.0

//! Snapshot-coherent composition of integrated search and memory lifecycle.

use crate::{
    NativeProduct, ObjectId, ProductError, ProductErrorCode, ProductIntegratedSearchHit,
    ProductSearchRequest, ProductSearchResult, ProductSnapshot, SnapshotIdentity,
};

/// Maximum physical domains participating in one memory read.
pub const MAX_MEMORY_COLLECTIONS: usize = 3;
/// Maximum memories returned by one bounded read.
pub const MAX_MEMORY_RESULTS: usize = 64;
/// Maximum search candidates inspected in each physical domain.
pub const MAX_MEMORY_CANDIDATES: usize = 1_000;
/// Maximum stored lifecycle envelope, including application metadata.
pub const MAX_MEMORY_ENVELOPE_BYTES: usize = 64 * 1024;

/// Versioned composition request; application filters carry project eligibility.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductMemoryRecallRequest {
    /// Distinct physical collections in ascending stable-ID order.
    pub collections: Vec<ObjectId>,
    /// Bounded lexical/vector search shared by the selected collections.
    pub search: ProductSearchRequest,
    /// Final limit applied after lifecycle filtering and cross-domain ordering.
    pub limit: usize,
    /// Bounded caller provenance (for example an attested query embedding).
    /// These bytes are sealed in a proof; they never execute inside the engine.
    pub provenance: Vec<u8>,
}

impl ProductMemoryRecallRequest {
    /// Validates the bounded memory profile without reading or mutating data.
    ///
    /// # Errors
    /// Rejects duplicate/unsorted collections, unsupported result transforms,
    /// and counts outside the versioned profile.
    pub fn validate(&self) -> Result<(), ProductError> {
        if self.collections.is_empty()
            || self.collections.len() > MAX_MEMORY_COLLECTIONS
            || self.collections.windows(2).any(|pair| pair[0] >= pair[1])
            || !(1..=MAX_MEMORY_RESULTS).contains(&self.limit)
            || !(self.limit..=MAX_MEMORY_CANDIDATES).contains(&self.search.limit)
            || self.search.offset != 0
            || !self.search.sort.is_empty()
            || !self.search.facets.is_empty()
            || !self.search.range_facets.is_empty()
            || !self.search.aggregations.is_empty()
            || self.provenance.len() > MAX_MEMORY_ENVELOPE_BYTES
        {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        Ok(())
    }
}

/// Conditional background enrichment of one already-live memory document.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductMemoryEnrichRequest {
    /// Physical memory collection.
    pub collection: ObjectId,
    /// Expected digest of the live lifecycle envelope observed before inference.
    pub expected_envelope_digest: [u8; 32],
    /// Idempotency identity and complete document retaining its original text.
    pub update: crate::ProductSearchDocumentUpdate,
}

/// One selected hit paired with the exact live envelope at the same snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductMemoryHit {
    /// Physical memory domain.
    pub collection: ObjectId,
    /// Search identity, score and typed provenance values.
    pub hit: ProductIntegratedSearchHit,
    /// Bounded application envelope bytes; never decoded by the storage kernel.
    pub envelope: Vec<u8>,
}

/// Complete candidate execution for one memory domain.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductMemorySearchResult {
    /// Physical collection that produced this receipt.
    pub collection: ObjectId,
    /// Complete bounded integrated-search result at the common snapshot.
    pub result: ProductSearchResult,
}

/// Complete memory composition, including evidence for its candidate boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductMemoryRecallResult {
    /// Common snapshot and logical time used by search and lifecycle reads.
    pub snapshot: SnapshotIdentity,
    /// Final canonical order after lifecycle filtering.
    pub memories: Vec<ProductMemoryHit>,
    /// Filter-eligible records excluded before candidate selection because
    /// their lifecycle is absent, expired, or empty.
    pub expired_filtered: usize,
    /// Search receipts in request collection order.
    pub searches: Vec<ProductMemorySearchResult>,
}

impl ProductMemoryRecallResult {
    /// Checks counts, snapshot identity, candidate membership and canonical order.
    ///
    /// # Errors
    /// Rejects a response that could not represent the bounded composition.
    pub fn validate(&self) -> Result<(), ProductError> {
        let invalid = || ProductError::from_code(ProductErrorCode::InvalidRequest);
        if self.searches.is_empty()
            || self.searches.len() > MAX_MEMORY_COLLECTIONS
            || self.memories.len() > MAX_MEMORY_RESULTS
            || self
                .searches
                .windows(2)
                .any(|pair| pair[0].collection >= pair[1].collection)
            || self.searches.iter().any(|search| {
                search.result.snapshot != self.snapshot
                    || search.result.hits.len() > MAX_MEMORY_CANDIDATES
            })
            || self.expired_filtered
                > self.searches.iter().fold(0_usize, |sum, search| {
                    sum.saturating_add(search.result.total_documents)
                })
        {
            return Err(invalid());
        }
        let mut seen = std::collections::BTreeSet::new();
        for memory in &self.memories {
            if memory.envelope.is_empty()
                || memory.envelope.len() > MAX_MEMORY_ENVELOPE_BYTES
                || !seen.insert((memory.collection, memory.hit.object_id))
                || !memory.hit.score.is_finite()
                || memory.hit.score < 0.0
                || !self.searches.iter().any(|search| {
                    search.collection == memory.collection
                        && search.result.hits.contains(&memory.hit)
                })
            {
                return Err(invalid());
            }
        }
        if self
            .memories
            .windows(2)
            .any(|pair| memory_order(&pair[0], &pair[1]).is_gt())
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn memory_order(left: &ProductMemoryHit, right: &ProductMemoryHit) -> std::cmp::Ordering {
    right
        .hit
        .score
        .total_cmp(&left.hit.score)
        .then_with(|| left.collection.cmp(&right.collection))
        .then_with(|| left.hit.object_id.cmp(&right.hit.object_id))
}

/// Canonical public lifecycle key used by all Agent Memory adapters.
pub fn memory_lifecycle_key(collection: ObjectId, identity: ObjectId) -> Vec<u8> {
    let mut key = b"hyphae-memory/".to_vec();
    key.extend_from_slice(&collection.get().to_le_bytes());
    key.extend_from_slice(&identity.get().to_le_bytes());
    key
}

impl NativeProduct {
    /// Publishes an enrichment only while its source lifecycle is unchanged.
    ///
    /// The service owns the product exclusively while this operation executes.
    /// It never creates or extends a lifecycle, so deletion/expiry cannot be
    /// undone by a late inference result.
    ///
    /// # Errors
    /// Rejects stale/absent sources and changes outside the embedding fields.
    pub fn memory_enrich(
        &mut self,
        request: &ProductMemoryEnrichRequest,
        logical_time_micros: i64,
    ) -> Result<crate::ProductSearchIngestReceipt, ProductError> {
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        let key = memory_lifecycle_key(request.collection, request.update.document.object_id);
        let source = snapshot
            .structure_get(&key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
        if blake3::hash(source).as_bytes() != &request.expected_envelope_digest {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let identity = request.update.document.object_id;
        let previous = identity
            .get()
            .checked_sub(1)
            .and_then(|id| ObjectId::new(id).ok());
        let page = Self::search_documents_at_snapshot(&snapshot, request.collection, previous, 1)?;
        let original = page
            .documents
            .first()
            .filter(|document| document.object_id == identity)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
        let is_embedding = |name: &str| {
            matches!(
                name,
                "embedding_model" | "embedding_attestation" | "embedding_manifest"
            )
        };
        if original.text != request.update.document.text
            || original
                .doc_values
                .iter()
                .filter(|(name, _)| !is_embedding(name))
                .ne(request
                    .update
                    .document
                    .doc_values
                    .iter()
                    .filter(|(name, _)| !is_embedding(name)))
            || request
                .update
                .document
                .vectors
                .keys()
                .any(|name| name != "memory")
            || !request.update.document.vectors.contains_key("memory")
        {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest));
        }
        let mut update = request.update.clone();
        let vector = update
            .document
            .vectors
            .get("memory")
            .cloned()
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
        update.document.vectors = original.vectors.clone();
        update.document.vectors.insert("memory".into(), vector);
        drop(snapshot);
        let binding =
            self.resolve_search_collection_binding(request.collection, logical_time_micros)?;
        for vector in &binding.vectors {
            let status = self.administration().ann_maintenance_status(vector.index)?;
            if status.delta_records >= usize::from(status.lifecycle.consolidate_after_deltas) {
                self.administration()
                    .consolidate_ann(crate::AnnConsolidationRequest {
                        index: vector.index,
                        max_vectors: hyphae_native_runtime::MAX_ANN_CONSOLIDATION_VECTORS,
                        max_delta_records: usize::try_from(status.lifecycle.delta_max_entries)
                            .unwrap_or(usize::MAX),
                        durability: crate::ProductDurability::Strict,
                    })?;
            }
        }
        self.update_search_document(
            request.collection,
            &update,
            logical_time_micros,
            crate::ProductDurability::Strict,
        )
    }
    /// Reads candidate search results and lifecycle records on one snapshot.
    ///
    /// # Errors
    /// Returns a typed error for invalid requests, corrupt data or exhausted
    /// execution limits. No partial result is returned.
    pub fn memory_recall(
        &self,
        request: &ProductMemoryRecallRequest,
        logical_time_micros: i64,
    ) -> Result<ProductMemoryRecallResult, ProductError> {
        self.memory_recall_with_checkpoint(request, logical_time_micros, || Ok(()))
    }

    pub(crate) fn memory_recall_with_checkpoint(
        &self,
        request: &ProductMemoryRecallRequest,
        logical_time_micros: i64,
        mut checkpoint: impl FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductMemoryRecallResult, ProductError> {
        request.validate()?;
        checkpoint()?;
        let snapshot = self.snapshot_bounded(logical_time_micros)?;
        self.memory_recall_at_snapshot(request, &snapshot, checkpoint)
    }

    fn memory_recall_at_snapshot(
        &self,
        request: &ProductMemoryRecallRequest,
        snapshot: &ProductSnapshot,
        mut checkpoint: impl FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductMemoryRecallResult, ProductError> {
        let mut memories = Vec::new();
        let mut searches = Vec::new();
        let mut expired_filtered = 0;
        for collection in &request.collections {
            checkpoint()?;
            let (result, excluded) = self.search_memory_collection_at_snapshot(
                snapshot,
                *collection,
                &request.search,
                &mut checkpoint,
            )?;
            expired_filtered += excluded;
            for hit in &result.hits {
                checkpoint()?;
                let key = memory_lifecycle_key(*collection, hit.object_id);
                let Some(envelope) = snapshot
                    .structure_get(&key)
                    .filter(|value| !value.is_empty())
                else {
                    expired_filtered += 1;
                    continue;
                };
                if envelope.len() > MAX_MEMORY_ENVELOPE_BYTES {
                    return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
                }
                memories.push(ProductMemoryHit {
                    collection: *collection,
                    hit: hit.clone(),
                    envelope: envelope.to_vec(),
                });
            }
            searches.push(ProductMemorySearchResult {
                collection: *collection,
                result,
            });
        }
        memories.sort_by(memory_order);
        memories.truncate(request.limit);
        checkpoint()?;
        Ok(ProductMemoryRecallResult {
            snapshot: snapshot.identity(),
            memories,
            expired_filtered,
            searches,
        })
    }
}
