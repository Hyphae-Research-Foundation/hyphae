// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic Hyphae-owned HNSW kernel and exact vector-search oracle.
//!
//! This crate owns graph construction and traversal. It deliberately exposes
//! canonical records so the search engine can persist them in Hyphae pages
//! without serializing an opaque third-party index.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
};

use hyphae_native_types::{Csn, ObjectId};
use thiserror::Error;

const MIN_M: u16 = 2;
const MAX_M: u16 = 64;
/// Maximum deterministic HNSW layer used for validation and build budgeting.
pub const MAX_HNSW_LEVEL: u16 = 32;
const BUILD_IDENTITY_VERSION: u16 = 1;

/// Native vector metric. Smaller distances always rank first.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Metric {
    /// One minus cosine similarity.
    Cosine = 1,
    /// Negated dot product.
    NegativeDot = 2,
    /// Squared Euclidean distance.
    SquaredL2 = 3,
}

/// Versioned HNSW construction and query bounds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct HnswConfig {
    m: u16,
    ef_construction: u16,
    ef_search_default: u16,
    ef_search_max: u16,
    seed: u64,
}

impl HnswConfig {
    /// Constructs a checked HNSW configuration.
    ///
    /// # Errors
    ///
    /// Returns an error unless `M` is in 2 through 64, construction breadth
    /// is at least `M`, and the default search breadth is nonzero and no
    /// larger than the maximum.
    pub fn new(
        m: u16,
        ef_construction: u16,
        ef_search_default: u16,
        ef_search_max: u16,
        seed: u64,
    ) -> Result<Self, AnnError> {
        if !(MIN_M..=MAX_M).contains(&m) {
            return Err(AnnError::InvalidM);
        }
        if ef_construction < m {
            return Err(AnnError::InvalidEfConstruction);
        }
        if ef_search_default == 0 || ef_search_default > ef_search_max {
            return Err(AnnError::InvalidEfSearch);
        }
        Ok(Self {
            m,
            ef_construction,
            ef_search_default,
            ef_search_max,
            seed,
        })
    }

    /// Maximum retained neighbors per node and layer.
    pub const fn m(self) -> u16 {
        self.m
    }

    /// Candidate breadth used during deterministic rebuild.
    pub const fn ef_construction(self) -> u16 {
        self.ef_construction
    }

    /// Default query breadth.
    pub const fn ef_search_default(self) -> u16 {
        self.ef_search_default
    }

    /// Maximum admitted query breadth.
    pub const fn ef_search_max(self) -> u16 {
        self.ef_search_max
    }

    /// Definition-pinned deterministic seed.
    pub const fn seed(self) -> u64 {
        self.seed
    }
}

/// Immutable vector-index definition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VectorIndexDefinition {
    index_id: ObjectId,
    dimension: u16,
    metric: Metric,
    config: HnswConfig,
}

impl VectorIndexDefinition {
    /// Constructs a checked vector-index definition.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero dimension.
    pub fn new(
        index_id: ObjectId,
        dimension: u16,
        metric: Metric,
        config: HnswConfig,
    ) -> Result<Self, AnnError> {
        if dimension == 0 {
            return Err(AnnError::InvalidDimension);
        }
        Ok(Self {
            index_id,
            dimension,
            metric,
            config,
        })
    }

    /// Stable catalog identity of this index.
    pub const fn index_id(self) -> ObjectId {
        self.index_id
    }

    /// Fixed vector dimension.
    pub const fn dimension(self) -> u16 {
        self.dimension
    }

    /// Fixed metric.
    pub const fn metric(self) -> Metric {
        self.metric
    }

    /// HNSW configuration.
    pub const fn config(self) -> HnswConfig {
        self.config
    }

    /// Content digest used by deterministic level derivation.
    pub fn digest(self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }

    fn canonical_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(35);
        bytes.extend_from_slice(&self.index_id.get().to_be_bytes());
        bytes.extend_from_slice(&self.dimension.to_le_bytes());
        bytes.push(self.metric as u8);
        bytes.extend_from_slice(&self.config.m.to_le_bytes());
        bytes.extend_from_slice(&self.config.ef_construction.to_le_bytes());
        bytes.extend_from_slice(&self.config.ef_search_default.to_le_bytes());
        bytes.extend_from_slice(&self.config.ef_search_max.to_le_bytes());
        bytes.extend_from_slice(&self.config.seed.to_le_bytes());
        bytes
    }
}

/// Canonical finite `f32` vector.
#[derive(Clone, Debug, PartialEq)]
pub struct Vector {
    values: Box<[f32]>,
    norm_squared: f64,
}

impl Vector {
    /// Collects finite components and canonicalizes both signed zero values to
    /// positive zero.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty vector, excessive dimension, NaN, or
    /// infinity.
    pub fn new(values: impl IntoIterator<Item = f32>) -> Result<Self, AnnError> {
        let values = values
            .into_iter()
            .map(|value| {
                if !value.is_finite() {
                    Err(AnnError::NonFiniteComponent)
                } else if value == 0.0 {
                    Ok(0.0)
                } else {
                    Ok(value)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if values.is_empty() || values.len() > usize::from(u16::MAX) {
            return Err(AnnError::InvalidDimension);
        }
        let norm_squared = values
            .iter()
            .map(|value| {
                let value = f64::from(*value);
                value * value
            })
            .sum();
        Ok(Self {
            values: values.into_boxed_slice(),
            norm_squared,
        })
    }

    /// Returns canonical components.
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Returns the vector dimension.
    pub fn dimension(&self) -> usize {
        self.values.len()
    }
}

/// One persisted vector and its canonical rebuild ordering CSN.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorRecord {
    /// Stable object identity.
    pub object_id: ObjectId,
    /// CSN that created the current visible vector version.
    pub creating_csn: Csn,
    /// Canonical vector.
    pub vector: Vector,
}

/// One canonical directed HNSW node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphNodeRecord {
    /// Stable object identity.
    pub object_id: ObjectId,
    /// Highest layer occupied by the node.
    pub level: u16,
    /// Object-ID-sorted neighbor identities for layers zero through `level`.
    pub neighbors: Vec<Vec<ObjectId>>,
}

/// Complete persistence-facing HNSW generation.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexSnapshot {
    /// Immutable index definition.
    pub definition: VectorIndexDefinition,
    /// Vectors in object-ID order.
    pub vectors: Vec<VectorRecord>,
    /// Graph nodes in object-ID order.
    pub nodes: Vec<GraphNodeRecord>,
    /// Greedy traversal entry point.
    pub entry_point: Option<ObjectId>,
    /// Highest graph layer.
    pub max_level: u16,
    /// Digest of the complete logical build.
    pub build_identity: [u8; 32],
}

/// Scalar persistence metadata for one immutable HNSW generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HnswGenerationDescriptor {
    /// Digest of the complete logical build.
    pub build_identity: [u8; 32],
    /// Number of canonical vectors.
    pub vector_count: usize,
    /// Number of canonical graph nodes.
    pub graph_node_count: usize,
    /// Greedy traversal entry point.
    pub entry_point: Option<ObjectId>,
    /// Highest graph layer.
    pub max_level: u16,
}

/// Complete persistence-facing experimental partitioned HNSW generation.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionedIndexSnapshot {
    /// Immutable definition shared by every child generation.
    pub definition: VectorIndexDefinition,
    /// Canonical recursive partition plan identity.
    pub input_identity: [u8; 32],
    /// Aggregate child generation identity.
    pub build_identity: [u8; 32],
    /// Child snapshots in canonical partition order.
    pub partitions: Vec<IndexSnapshot>,
}

/// One exact-distance vector result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorHit {
    /// Stable object identity.
    pub object_id: ObjectId,
    /// Canonical metric distance. Smaller values rank first.
    pub distance: f64,
}

/// Physical strategy used to produce one ANN result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnnSearchStrategy {
    /// Ordinary bounded graph traversal without a filter.
    GraphTraversal,
    /// Layer-zero graph navigation retains a separate eligible allowlist set.
    StableIdEligibilityTraversal,
    /// The admitted set was restrictive enough for complete exact scoring.
    StableIdAdaptiveExact,
}

/// Honest recall qualification for one ANN execution strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnnRecallRisk {
    /// Results remain subject to ordinary bounded graph-traversal recall.
    ApproximateTraversal,
    /// Filter eligibility participates in bounded layer-zero traversal, while
    /// navigation may still visit disallowed connector nodes.
    FilteredApproximateTraversal,
    /// The complete admitted set was scored exactly.
    ExactFilteredCandidates,
}

/// Per-query ANN bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    k: usize,
    ef_search: usize,
    exact_rerank: Option<usize>,
}

impl SearchOptions {
    /// Constructs checked query bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when `k` is zero, `ef_search` is smaller than `k`, or
    /// an exact-rerank count falls outside `k..=ef_search`.
    pub fn new(k: usize, ef_search: usize, exact_rerank: Option<usize>) -> Result<Self, AnnError> {
        if k == 0 || ef_search < k {
            return Err(AnnError::InvalidSearchOptions);
        }
        if exact_rerank.is_some_and(|count| count < k || count > ef_search) {
            return Err(AnnError::InvalidSearchOptions);
        }
        Ok(Self {
            k,
            ef_search,
            exact_rerank,
        })
    }

    /// Requested result count.
    pub const fn k(self) -> usize {
        self.k
    }

    /// Graph-search breadth.
    pub const fn ef_search(self) -> usize {
        self.ef_search
    }

    /// Optional candidate count rescored by the exact metric.
    pub const fn exact_rerank(self) -> Option<usize> {
        self.exact_rerank
    }
}

/// Execution receipt for one approximate query.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnSearchResult {
    /// Explicit approximation label.
    pub approximate: bool,
    /// Physical graph generation identity.
    pub build_identity: [u8; 32],
    /// Fixed index metric.
    pub metric: Metric,
    /// Effective graph breadth.
    pub ef_search: usize,
    /// Number of layer-zero candidates retained before final truncation.
    pub candidate_count: usize,
    /// Candidates retained after applying the selected filter strategy.
    pub eligible_candidate_count: usize,
    /// Physical filtering and traversal strategy that ran.
    pub strategy: AnnSearchStrategy,
    /// Recall qualification implied by the selected strategy.
    pub recall_risk: AnnRecallRisk,
    /// Whether candidates were explicitly rescored.
    pub exact_reranked: bool,
    /// Number of distinct nodes whose distance was evaluated.
    pub visited_nodes: usize,
    /// Ordered result hits.
    pub hits: Vec<VectorHit>,
}

/// Native ANN validation, mutation, or query failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum AnnError {
    /// `M` falls outside the versioned range.
    #[error("native HNSW M must be in 2 through 64")]
    InvalidM,
    /// Construction breadth is smaller than `M`.
    #[error("native HNSW ef_construction must be at least M")]
    InvalidEfConstruction,
    /// Default search breadth is zero or exceeds the maximum.
    #[error("native HNSW default ef_search is outside its configured maximum")]
    InvalidEfSearch,
    /// Index or vector dimension is zero or exceeds the canonical field.
    #[error("native ANN vector dimension is invalid")]
    InvalidDimension,
    /// Vector components contain NaN or infinity.
    #[error("native ANN vectors require finite f32 components")]
    NonFiniteComponent,
    /// A vector does not match the index dimension.
    #[error("native ANN vector dimension does not match the index")]
    DimensionMismatch,
    /// Cosine distance cannot admit a zero vector.
    #[error("native cosine ANN vectors cannot be zero")]
    ZeroCosineVector,
    /// A canonical build input repeats one object identity.
    #[error("native ANN build contains a duplicate object ID")]
    DuplicateObjectId,
    /// Query breadth or reranking bounds are invalid.
    #[error("native ANN search options are invalid")]
    InvalidSearchOptions,
    /// Query breadth exceeds the index definition.
    #[error("native ANN ef_search exceeds the configured maximum")]
    SearchBreadthExceeded,
    /// Persisted graph records are malformed or not the canonical rebuild.
    #[error("native HNSW graph is corrupt or noncanonical")]
    CorruptGraph,
    /// A canonical count cannot fit its versioned field.
    #[error("native HNSW canonical count exceeds its versioned field")]
    LengthOverflow,
    /// A deterministic bulk plan requires at least one bounded partition.
    #[error("native ANN bulk partition count is invalid")]
    InvalidPartitionCount,
    /// Cooperative cancellation stopped a canonical build before publication.
    #[error("native HNSW build was cancelled")]
    BuildCancelled,
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    creating_csn: Csn,
    vector: Vector,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraphNode {
    level: u16,
    neighbors: Vec<BTreeSet<ObjectId>>,
}

impl GraphNode {
    fn new(level: u16) -> Self {
        Self {
            level,
            neighbors: (0..=level).map(|_| BTreeSet::new()).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    object_id: ObjectId,
    distance: f64,
}

/// Deterministic in-memory HNSW generation.
#[derive(Clone, Debug, PartialEq)]
pub struct HnswIndex {
    definition: VectorIndexDefinition,
    definition_digest: [u8; 32],
    entries: BTreeMap<ObjectId, Entry>,
    nodes: BTreeMap<ObjectId, GraphNode>,
    entry_point: Option<ObjectId>,
    max_level: u16,
    build_identity: [u8; 32],
}

/// Deterministic geometrically ordered input partitions for an experimental
/// bulk HNSW build.
#[derive(Clone, Debug, PartialEq)]
pub struct HnswPartitionPlan {
    definition: VectorIndexDefinition,
    partitions: Vec<Vec<VectorRecord>>,
    input_identity: [u8; 32],
}

impl HnswPartitionPlan {
    /// Creates balanced recursive geometric partitions. Every split uses a
    /// distinct definition-derived projection and stable object-ID tie break.
    ///
    /// # Errors
    ///
    /// Returns an error for zero partitions, duplicate identities, or invalid
    /// vectors.
    pub fn build(
        definition: VectorIndexDefinition,
        records: impl IntoIterator<Item = VectorRecord>,
        partition_count: usize,
    ) -> Result<Self, AnnError> {
        Self::build_cancellable(definition, records, partition_count, || {
            ControlFlow::Continue(())
        })
    }

    /// Creates balanced recursive geometric partitions with cooperative
    /// cancellation throughout validation, projection, sorting, and identity
    /// construction.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::BuildCancelled`] when the controller requests
    /// cancellation, or the same validation errors as [`Self::build`].
    pub fn build_cancellable(
        definition: VectorIndexDefinition,
        records: impl IntoIterator<Item = VectorRecord>,
        partition_count: usize,
        mut control: impl FnMut() -> ControlFlow<()>,
    ) -> Result<Self, AnnError> {
        if partition_count == 0 {
            return Err(AnnError::InvalidPartitionCount);
        }
        let mut identities = BTreeSet::new();
        let mut admitted_records = Vec::new();
        for record in records {
            check_build_control(&mut control)?;
            validate_vector(definition, &record.vector)?;
            if !identities.insert(record.object_id) {
                return Err(AnnError::DuplicateObjectId);
            }
            admitted_records.push(record);
        }
        check_build_control(&mut control)?;
        let mut records = admitted_records;
        records.sort_by_key(|record| record.object_id);
        check_build_control(&mut control)?;
        let effective_partitions = partition_count.min(records.len()).max(1);
        let partitions = if records.is_empty() {
            Vec::new()
        } else {
            recursive_projection_partitions(
                definition,
                records,
                effective_partitions,
                &mut control,
            )?
        };
        let input_identity =
            partition_plan_identity_with_control(definition, &partitions, &mut control)?;
        Ok(Self {
            definition,
            partitions,
            input_identity,
        })
    }

    /// Fixed native vector definition shared by every partition.
    pub const fn definition(&self) -> VectorIndexDefinition {
        self.definition
    }

    /// Exact number of planned partitions.
    pub fn partition_count(&self) -> usize {
        self.partitions.len()
    }

    /// Canonical source and boundary identity for this plan.
    pub const fn input_identity(&self) -> [u8; 32] {
        self.input_identity
    }

    /// Immutable canonically ordered partition records.
    pub fn partitions(&self) -> &[Vec<VectorRecord>] {
        &self.partitions
    }

    /// Transfers canonically ordered partitions to an external governed
    /// executor.
    pub fn into_partitions(self) -> Vec<Vec<VectorRecord>> {
        self.partitions
    }
}

/// Experimental fan-out index built from independent deterministic HNSW
/// partitions. It is not a durable production format until P4 qualification.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionedHnswIndex {
    definition: VectorIndexDefinition,
    partitions: Vec<HnswIndex>,
    summaries: Vec<HnswPartitionSummary>,
    input_identity: [u8; 32],
    build_identity: [u8; 32],
}

/// Deterministic routing summary for one child HNSW generation.
#[derive(Clone, Debug, PartialEq)]
pub struct HnswPartitionSummary {
    partition_index: usize,
    vector_count: usize,
    centroid: Box<[f64]>,
    squared_l2_radius: f64,
    projection_minimum: f64,
    projection_maximum: f64,
    representative_object_id: ObjectId,
    routing_vector: Vector,
}

impl HnswPartitionSummary {
    /// Stable child position committed by the partition plan.
    pub const fn partition_index(&self) -> usize {
        self.partition_index
    }

    /// Complete vector count in this child generation.
    pub const fn vector_count(&self) -> usize {
        self.vector_count
    }

    /// Canonical f64 centroid components in index-dimension order.
    pub fn centroid(&self) -> &[f64] {
        &self.centroid
    }

    /// Maximum squared-L2 distance from the centroid to a child vector.
    pub const fn squared_l2_radius(&self) -> f64 {
        self.squared_l2_radius
    }

    /// Inclusive minimum definition-derived projection in this partition.
    pub const fn projection_minimum(&self) -> f64 {
        self.projection_minimum
    }

    /// Inclusive maximum definition-derived projection in this partition.
    pub const fn projection_maximum(&self) -> f64 {
        self.projection_maximum
    }

    /// Stable vector nearest the centroid, used if cosine centroid norm is zero.
    pub const fn representative_object_id(&self) -> ObjectId {
        self.representative_object_id
    }
}

/// Approximate partition-routing evidence plus the ordinary ANN result.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionedAnnSearchResult {
    /// Canonically merged child result.
    pub result: AnnSearchResult,
    /// Partitions selected in deterministic routing-score order.
    pub selected_partitions: Vec<usize>,
    /// Complete available partition count.
    pub total_partitions: usize,
}

/// Monotonic canonical-node progress for one complete HNSW generation build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HnswBuildProgress {
    completed: usize,
    total: usize,
}

impl HnswBuildProgress {
    const fn new(completed: usize, total: usize) -> Self {
        debug_assert!(completed <= total);
        Self { completed, total }
    }

    /// Canonical nodes inserted into the new generation.
    pub const fn completed(self) -> usize {
        self.completed
    }

    /// Complete admitted node count for the new generation.
    pub const fn total(self) -> usize {
        self.total
    }
}

impl HnswIndex {
    /// Creates an empty index.
    ///
    /// # Errors
    ///
    /// Returns an error if the empty canonical build identity cannot be
    /// represented.
    pub fn new(definition: VectorIndexDefinition) -> Result<Self, AnnError> {
        let mut index = Self {
            definition,
            definition_digest: definition.digest(),
            entries: BTreeMap::new(),
            nodes: BTreeMap::new(),
            entry_point: None,
            max_level: 0,
            build_identity: [0; 32],
        };
        index.refresh_build_identity()?;
        Ok(index)
    }

    /// Builds one canonical generation from an unordered record collection.
    ///
    /// Rebuild order is always `(creating_csn, object_id)`, independent of
    /// input arrival order.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate identities or invalid vector admission.
    pub fn build(
        definition: VectorIndexDefinition,
        records: impl IntoIterator<Item = VectorRecord>,
    ) -> Result<Self, AnnError> {
        Self::build_with_progress(definition, records, |_| {})
    }

    /// Builds one canonical generation and observes deterministic node progress.
    ///
    /// The observer receives zero admitted nodes before graph construction and
    /// one observation after every node is inserted in canonical rebuild order.
    /// Observing progress cannot alter the graph or its build identity.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::build`].
    pub fn build_with_progress(
        definition: VectorIndexDefinition,
        records: impl IntoIterator<Item = VectorRecord>,
        mut progress: impl FnMut(HnswBuildProgress),
    ) -> Result<Self, AnnError> {
        Self::build_cancellable(definition, records, |value| {
            progress(value);
            ControlFlow::Continue(())
        })
    }

    /// Builds one canonical generation with cooperative cancellation checkpoints.
    ///
    /// The controller receives the same deterministic observations as
    /// [`Self::build_with_progress`]. Returning [`ControlFlow::Break`] stops the
    /// build at that checkpoint and discards the private partial generation.
    /// No index is returned unless construction and validation both complete.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::BuildCancelled`] when the controller requests
    /// cancellation, or the same validation errors as [`Self::build`].
    pub fn build_cancellable(
        definition: VectorIndexDefinition,
        records: impl IntoIterator<Item = VectorRecord>,
        control: impl FnMut(HnswBuildProgress) -> ControlFlow<()>,
    ) -> Result<Self, AnnError> {
        let mut entries = BTreeMap::new();
        for record in records {
            validate_vector(definition, &record.vector)?;
            if entries
                .insert(
                    record.object_id,
                    Entry {
                        creating_csn: record.creating_csn,
                        vector: record.vector,
                    },
                )
                .is_some()
            {
                return Err(AnnError::DuplicateObjectId);
            }
        }
        Self::from_entries_with_control(definition, entries, control)
    }

    /// Builds one canonical generation from an owned partition while checking
    /// cancellation before every validation, insertion, and graph checkpoint.
    ///
    /// This ownership-oriented surface is intended for governed bulk workers:
    /// cancellation never waits for the complete partition to be ingested.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::BuildCancelled`] when the controller requests
    /// cancellation, or the same validation errors as [`Self::build`].
    pub fn build_owned_cancellable(
        definition: VectorIndexDefinition,
        records: Vec<VectorRecord>,
        mut control: impl FnMut() -> ControlFlow<()>,
    ) -> Result<Self, AnnError> {
        let mut entries = BTreeMap::new();
        for record in records {
            check_build_control(&mut control)?;
            validate_vector(definition, &record.vector)?;
            if entries
                .insert(
                    record.object_id,
                    Entry {
                        creating_csn: record.creating_csn,
                        vector: record.vector,
                    },
                )
                .is_some()
            {
                return Err(AnnError::DuplicateObjectId);
            }
        }
        Self::from_entries_with_control(definition, entries, |_| control())
    }

    /// Restores one persisted generation after validating its canonical
    /// ordering, structural bounds, deterministic levels, and complete build
    /// identity.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate, missing, or out-of-order identities,
    /// malformed vectors, invalid graph edges, or any build-identity mismatch.
    pub fn restore(snapshot: &IndexSnapshot) -> Result<Self, AnnError> {
        Self::restore_owned(snapshot.clone())
    }

    /// Restores one owned persisted generation without cloning its vectors or
    /// graph records.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::restore`].
    pub fn restore_owned(snapshot: IndexSnapshot) -> Result<Self, AnnError> {
        let mut entries = BTreeMap::new();
        let mut previous_object_id = None;
        for record in snapshot.vectors {
            if previous_object_id.is_some_and(|previous| previous >= record.object_id) {
                return Err(AnnError::CorruptGraph);
            }
            validate_vector(snapshot.definition, &record.vector)?;
            if entries
                .insert(
                    record.object_id,
                    Entry {
                        creating_csn: record.creating_csn,
                        vector: record.vector,
                    },
                )
                .is_some()
            {
                return Err(AnnError::CorruptGraph);
            }
            previous_object_id = Some(record.object_id);
        }

        let mut nodes = BTreeMap::new();
        previous_object_id = None;
        for record in snapshot.nodes {
            if previous_object_id.is_some_and(|previous| previous >= record.object_id)
                || record.neighbors.len() != usize::from(record.level) + 1
            {
                return Err(AnnError::CorruptGraph);
            }
            let neighbors = record
                .neighbors
                .into_iter()
                .map(|layer| {
                    if layer.windows(2).any(|pair| pair[0] >= pair[1]) {
                        return Err(AnnError::CorruptGraph);
                    }
                    Ok(layer.into_iter().collect())
                })
                .collect::<Result<Vec<BTreeSet<_>>, AnnError>>()?;
            if nodes
                .insert(
                    record.object_id,
                    GraphNode {
                        level: record.level,
                        neighbors,
                    },
                )
                .is_some()
            {
                return Err(AnnError::CorruptGraph);
            }
            previous_object_id = Some(record.object_id);
        }

        let restored = Self {
            definition: snapshot.definition,
            definition_digest: snapshot.definition.digest(),
            entries,
            nodes,
            entry_point: snapshot.entry_point,
            max_level: snapshot.max_level,
            build_identity: snapshot.build_identity,
        };
        restored.validate()?;
        Ok(restored)
    }

    /// Rebuilds a restored generation and proves byte-for-byte canonical graph
    /// equivalence. This intentionally expensive path is for offline proofs;
    /// ordinary recovery uses [`Self::restore`].
    ///
    /// # Errors
    ///
    /// Returns an error when ordinary restore fails or the deterministic
    /// rebuild differs from the persisted generation.
    pub fn restore_canonical(snapshot: &IndexSnapshot) -> Result<Self, AnnError> {
        let restored = Self::restore(snapshot)?;
        let expected = Self::from_entries(snapshot.definition, restored.entries)?;
        if expected.export_snapshot() != *snapshot {
            return Err(AnnError::CorruptGraph);
        }
        Ok(expected)
    }

    fn from_entries(
        definition: VectorIndexDefinition,
        entries: BTreeMap<ObjectId, Entry>,
    ) -> Result<Self, AnnError> {
        Self::from_entries_with_progress(definition, entries, |_| {})
    }

    fn from_entries_with_progress(
        definition: VectorIndexDefinition,
        entries: BTreeMap<ObjectId, Entry>,
        mut progress: impl FnMut(HnswBuildProgress),
    ) -> Result<Self, AnnError> {
        Self::from_entries_with_control(definition, entries, |value| {
            progress(value);
            ControlFlow::Continue(())
        })
    }

    fn from_entries_with_control(
        definition: VectorIndexDefinition,
        entries: BTreeMap<ObjectId, Entry>,
        mut control: impl FnMut(HnswBuildProgress) -> ControlFlow<()>,
    ) -> Result<Self, AnnError> {
        let mut index = Self::new(definition)?;
        index.entries = entries;
        index.rebuild_with_control(&mut control)?;
        Ok(index)
    }

    /// Returns the immutable definition.
    pub const fn definition(&self) -> VectorIndexDefinition {
        self.definition
    }

    /// Returns the number of current vectors.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns one immutable vector version by stable object identity.
    pub fn vector_record(&self, object_id: ObjectId) -> Option<(Csn, &Vector)> {
        self.entries
            .get(&object_id)
            .map(|entry| (entry.creating_csn, &entry.vector))
    }

    /// Visits every immutable vector version in stable object-ID order without
    /// exporting or cloning the graph generation.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by `visitor`.
    pub fn try_for_each_vector_record<E>(
        &self,
        mut visitor: impl FnMut(ObjectId, Csn, &Vector) -> Result<(), E>,
    ) -> Result<(), E> {
        for (object_id, entry) in &self.entries {
            visitor(*object_id, entry.creating_csn, &entry.vector)?;
        }
        Ok(())
    }

    /// Returns the canonical graph generation digest.
    pub const fn build_identity(&self) -> [u8; 32] {
        self.build_identity
    }

    /// Returns scalar persistence metadata without exporting the generation.
    pub fn generation_descriptor(&self) -> HnswGenerationDescriptor {
        HnswGenerationDescriptor {
            build_identity: self.build_identity,
            vector_count: self.entries.len(),
            graph_node_count: self.nodes.len(),
            entry_point: self.entry_point,
            max_level: self.max_level,
        }
    }

    /// Inserts or replaces one vector and deterministically rebuilds the
    /// generation in `(creating_csn, object_id)` order.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid vector admission.
    pub fn upsert(
        &mut self,
        object_id: ObjectId,
        creating_csn: Csn,
        vector: Vector,
    ) -> Result<(), AnnError> {
        validate_vector(self.definition, &vector)?;
        let mut entries = self.entries.clone();
        entries.insert(
            object_id,
            Entry {
                creating_csn,
                vector,
            },
        );
        *self = Self::from_entries(self.definition, entries)?;
        Ok(())
    }

    /// Deletes one current vector and deterministically rebuilds the graph.
    ///
    /// # Errors
    ///
    /// Returns an error if rebuilding the remaining canonical state fails.
    pub fn delete(&mut self, object_id: ObjectId) -> Result<bool, AnnError> {
        if !self.entries.contains_key(&object_id) {
            return Ok(false);
        }
        let mut entries = self.entries.clone();
        entries.remove(&object_id);
        *self = Self::from_entries(self.definition, entries)?;
        Ok(true)
    }

    /// Executes the complete exact ranking oracle.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid query admission.
    pub fn search_exact(&self, query: &Vector, k: usize) -> Result<Vec<VectorHit>, AnnError> {
        self.search_exact_allowlist(query, k, None)
    }

    /// Executes the complete exact ranking oracle over a stable-ID allowlist.
    ///
    /// IDs absent from the current generation are ignored. Unlike filtered
    /// ANN traversal, this scans every current vector admitted by the filter.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid query admission.
    pub fn search_exact_filtered(
        &self,
        query: &Vector,
        k: usize,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<Vec<VectorHit>, AnnError> {
        self.search_exact_allowlist(query, k, Some(allowlist))
    }

    fn search_exact_allowlist(
        &self,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<Vec<VectorHit>, AnnError> {
        validate_vector(self.definition, query)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut hits = self
            .entries
            .iter()
            .filter(|(object_id, _)| allowlist.is_none_or(|ids| ids.contains(object_id)))
            .map(|(object_id, entry)| {
                Ok(VectorHit {
                    object_id: *object_id,
                    distance: distance(self.definition.metric, query, &entry.vector)?,
                })
            })
            .collect::<Result<Vec<_>, AnnError>>()?;
        hits.sort_by(compare_hits);
        hits.truncate(k);
        Ok(hits)
    }

    /// Traverses the HNSW graph and explicitly labels the result approximate.
    ///
    /// Distances are always computed from canonical `f32` vectors with `f64`
    /// accumulation. Exact reranking repeats that calculation over the
    /// declared candidate count so the receipt records the physical choice.
    ///
    /// # Errors
    ///
    /// Returns an error for query admission or breadth above the configured
    /// maximum.
    pub fn search(
        &self,
        query: &Vector,
        options: SearchOptions,
    ) -> Result<AnnSearchResult, AnnError> {
        self.search_allowlist(query, options, None)
    }

    /// Traverses the graph with separate navigation and allowlist eligibility.
    ///
    /// Disallowed graph nodes remain available as connectors, but do not
    /// consume the bounded eligible candidate set. Restrictive admitted sets
    /// no larger than `ef_search` use the complete exact filtered path.
    ///
    /// # Errors
    ///
    /// Returns an error for query admission or breadth above the configured
    /// maximum.
    pub fn search_filtered(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<AnnSearchResult, AnnError> {
        validate_vector(self.definition, query)?;
        if options.ef_search > usize::from(self.definition.config.ef_search_max) {
            return Err(AnnError::SearchBreadthExceeded);
        }
        let eligible_count = allowlist
            .iter()
            .filter(|object_id| self.entries.contains_key(object_id))
            .count();
        if eligible_count <= options.ef_search {
            let hits = self.search_exact_filtered(query, options.k, allowlist)?;
            return Ok(AnnSearchResult {
                approximate: false,
                build_identity: self.build_identity,
                metric: self.definition.metric,
                ef_search: options.ef_search,
                candidate_count: eligible_count,
                eligible_candidate_count: eligible_count,
                strategy: AnnSearchStrategy::StableIdAdaptiveExact,
                recall_risk: AnnRecallRisk::ExactFilteredCandidates,
                exact_reranked: true,
                visited_nodes: eligible_count,
                hits,
            });
        }
        self.search_allowlist(query, options, Some(allowlist))
    }

    fn search_allowlist(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<AnnSearchResult, AnnError> {
        validate_vector(self.definition, query)?;
        if options.ef_search > usize::from(self.definition.config.ef_search_max) {
            return Err(AnnError::SearchBreadthExceeded);
        }
        let Some(mut current) = self.entry_point else {
            return Ok(AnnSearchResult {
                approximate: true,
                build_identity: self.build_identity,
                metric: self.definition.metric,
                ef_search: options.ef_search,
                candidate_count: 0,
                eligible_candidate_count: 0,
                strategy: strategy(allowlist),
                recall_risk: recall_risk(allowlist),
                exact_reranked: options.exact_rerank.is_some(),
                visited_nodes: 0,
                hits: Vec::new(),
            });
        };
        let mut visited = BTreeSet::new();
        visited.insert(current);
        for layer in (1..=self.max_level).rev() {
            current = self.greedy_at_layer(query, current, layer, &mut visited)?;
        }
        let breadth = options
            .exact_rerank
            .unwrap_or(options.ef_search)
            .max(options.k);
        let mut candidates = if let Some(allowlist) = allowlist {
            self.search_layer_filtered(
                query,
                &[current],
                options.ef_search.max(breadth),
                allowlist,
                &mut visited,
            )?
        } else {
            self.search_layer(
                query,
                &[current],
                0,
                options.ef_search.max(breadth),
                &mut visited,
            )?
        };
        let candidate_count = visited.len();
        let eligible_candidate_count = candidates.len();
        if let Some(rerank_count) = options.exact_rerank {
            candidates.truncate(rerank_count);
            for candidate in &mut candidates {
                candidate.distance = distance(
                    self.definition.metric,
                    query,
                    &self.entry(candidate.object_id)?.vector,
                )?;
            }
            candidates.sort_by(compare_candidates);
        }
        candidates.truncate(options.k);
        Ok(AnnSearchResult {
            approximate: true,
            build_identity: self.build_identity,
            metric: self.definition.metric,
            ef_search: options.ef_search,
            candidate_count,
            eligible_candidate_count,
            strategy: strategy(allowlist),
            recall_risk: recall_risk(allowlist),
            exact_reranked: options.exact_rerank.is_some(),
            visited_nodes: visited.len(),
            hits: candidates
                .into_iter()
                .map(|candidate| VectorHit {
                    object_id: candidate.object_id,
                    distance: candidate.distance,
                })
                .collect(),
        })
    }

    /// Exports canonical persistence records.
    pub fn export_snapshot(&self) -> IndexSnapshot {
        IndexSnapshot {
            definition: self.definition,
            vectors: self
                .entries
                .iter()
                .map(|(object_id, entry)| VectorRecord {
                    object_id: *object_id,
                    creating_csn: entry.creating_csn,
                    vector: entry.vector.clone(),
                })
                .collect(),
            nodes: self.export_graph(),
            entry_point: self.entry_point,
            max_level: self.max_level,
            build_identity: self.build_identity,
        }
    }

    /// Transfers this complete generation into canonical persistence records
    /// without cloning vector or graph payloads.
    pub fn into_snapshot(self) -> IndexSnapshot {
        let Self {
            definition,
            entries,
            nodes,
            entry_point,
            max_level,
            build_identity,
            ..
        } = self;
        IndexSnapshot {
            definition,
            vectors: entries
                .into_iter()
                .map(|(object_id, entry)| VectorRecord {
                    object_id,
                    creating_csn: entry.creating_csn,
                    vector: entry.vector,
                })
                .collect(),
            nodes: nodes
                .into_iter()
                .map(|(object_id, node)| GraphNodeRecord {
                    object_id,
                    level: node.level,
                    neighbors: node
                        .neighbors
                        .into_iter()
                        .map(|neighbors| neighbors.into_iter().collect())
                        .collect(),
                })
                .collect(),
            entry_point,
            max_level,
            build_identity,
        }
    }

    /// Exports graph nodes in canonical object-ID order.
    pub fn export_graph(&self) -> Vec<GraphNodeRecord> {
        self.nodes
            .iter()
            .map(|(object_id, node)| GraphNodeRecord {
                object_id: *object_id,
                level: node.level,
                neighbors: node
                    .neighbors
                    .iter()
                    .map(|neighbors| neighbors.iter().copied().collect())
                    .collect(),
            })
            .collect()
    }

    /// Verifies structural bounds and the complete build identity.
    ///
    /// # Errors
    ///
    /// Returns an error for any malformed graph state.
    pub fn validate(&self) -> Result<(), AnnError> {
        if self.entries.len() != self.nodes.len()
            || self.entries.keys().ne(self.nodes.keys())
            || self.calculate_build_identity()? != self.build_identity
        {
            return Err(AnnError::CorruptGraph);
        }
        if self.entries.is_empty() {
            if self.entry_point.is_some() || self.max_level != 0 {
                return Err(AnnError::CorruptGraph);
            }
            return Ok(());
        }
        let entry_point = self.entry_point.ok_or(AnnError::CorruptGraph)?;
        let entry_node = self.nodes.get(&entry_point).ok_or(AnnError::CorruptGraph)?;
        if entry_node.level != self.max_level {
            return Err(AnnError::CorruptGraph);
        }
        for (object_id, node) in &self.nodes {
            if node.level > MAX_HNSW_LEVEL
                || node.level > self.max_level
                || node.level != self.level_for(*object_id)
                || node.neighbors.len() != usize::from(node.level) + 1
            {
                return Err(AnnError::CorruptGraph);
            }
            for (layer, neighbors) in node.neighbors.iter().enumerate() {
                if neighbors.len() > usize::from(self.definition.config.m)
                    || neighbors.contains(object_id)
                {
                    return Err(AnnError::CorruptGraph);
                }
                for neighbor in neighbors {
                    let neighbor_node = self.nodes.get(neighbor).ok_or(AnnError::CorruptGraph)?;
                    if usize::from(neighbor_node.level) < layer {
                        return Err(AnnError::CorruptGraph);
                    }
                }
            }
        }
        Ok(())
    }

    fn rebuild_with_control(
        &mut self,
        control: &mut impl FnMut(HnswBuildProgress) -> ControlFlow<()>,
    ) -> Result<(), AnnError> {
        self.nodes.clear();
        self.entry_point = None;
        self.max_level = 0;
        let mut order = self
            .entries
            .iter()
            .map(|(object_id, entry)| (entry.creating_csn, *object_id))
            .collect::<Vec<_>>();
        order.sort_by_key(|(creating_csn, object_id)| (creating_csn.get(), object_id.get()));
        let total = order.len();
        if control(HnswBuildProgress::new(0, total)).is_break() {
            return Err(AnnError::BuildCancelled);
        }
        for (offset, (_, object_id)) in order.into_iter().enumerate() {
            self.insert_graph_node(object_id)?;
            if control(HnswBuildProgress::new(offset + 1, total)).is_break() {
                return Err(AnnError::BuildCancelled);
            }
        }
        self.refresh_build_identity()?;
        self.validate()
    }

    fn insert_graph_node(&mut self, object_id: ObjectId) -> Result<(), AnnError> {
        let level = self.level_for(object_id);
        let Some(mut entry_point) = self.entry_point else {
            self.nodes.insert(object_id, GraphNode::new(level));
            self.entry_point = Some(object_id);
            self.max_level = level;
            return Ok(());
        };
        let query = self.entry(object_id)?.vector.clone();
        let previous_max_level = self.max_level;
        self.nodes.insert(object_id, GraphNode::new(level));

        let mut visited = BTreeSet::new();
        visited.insert(entry_point);
        if previous_max_level > level {
            for layer in ((level + 1)..=previous_max_level).rev() {
                entry_point = self.greedy_at_layer(&query, entry_point, layer, &mut visited)?;
            }
        }
        let highest_shared_layer = level.min(previous_max_level);
        for layer in (0..=highest_shared_layer).rev() {
            let candidates = self.search_layer(
                &query,
                &[entry_point],
                layer,
                usize::from(self.definition.config.ef_construction),
                &mut visited,
            )?;
            let selected = candidates
                .iter()
                .filter(|candidate| candidate.object_id != object_id)
                .take(usize::from(self.definition.config.m))
                .map(|candidate| candidate.object_id)
                .collect::<Vec<_>>();
            self.connect(object_id, layer, &selected)?;
            if let Some(next) = candidates
                .iter()
                .find(|candidate| candidate.object_id != object_id)
            {
                entry_point = next.object_id;
            }
        }
        if level > previous_max_level {
            self.entry_point = Some(object_id);
            self.max_level = level;
        }
        Ok(())
    }

    fn connect(
        &mut self,
        object_id: ObjectId,
        layer: u16,
        neighbors: &[ObjectId],
    ) -> Result<(), AnnError> {
        let layer_index = usize::from(layer);
        let node = self
            .nodes
            .get_mut(&object_id)
            .ok_or(AnnError::CorruptGraph)?;
        let node_neighbors = node
            .neighbors
            .get_mut(layer_index)
            .ok_or(AnnError::CorruptGraph)?;
        node_neighbors.extend(neighbors.iter().copied());
        for neighbor in neighbors {
            let neighbor_node = self.nodes.get_mut(neighbor).ok_or(AnnError::CorruptGraph)?;
            neighbor_node
                .neighbors
                .get_mut(layer_index)
                .ok_or(AnnError::CorruptGraph)?
                .insert(object_id);
            self.prune(*neighbor, layer)?;
        }
        self.prune(object_id, layer)
    }

    fn prune(&mut self, object_id: ObjectId, layer: u16) -> Result<(), AnnError> {
        let layer_index = usize::from(layer);
        let current = self
            .nodes
            .get(&object_id)
            .and_then(|node| node.neighbors.get(layer_index))
            .ok_or(AnnError::CorruptGraph)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        if current.len() <= usize::from(self.definition.config.m) {
            return Ok(());
        }
        let source = self.entry(object_id)?.vector.clone();
        let mut ranked = current
            .into_iter()
            .map(|neighbor| {
                Ok(Candidate {
                    object_id: neighbor,
                    distance: distance(
                        self.definition.metric,
                        &source,
                        &self.entry(neighbor)?.vector,
                    )?,
                })
            })
            .collect::<Result<Vec<_>, AnnError>>()?;
        ranked.sort_by(compare_candidates);
        ranked.truncate(usize::from(self.definition.config.m));
        let retained = ranked
            .into_iter()
            .map(|candidate| candidate.object_id)
            .collect();
        self.nodes
            .get_mut(&object_id)
            .and_then(|node| node.neighbors.get_mut(layer_index))
            .ok_or(AnnError::CorruptGraph)
            .map(|neighbors| *neighbors = retained)
    }

    fn greedy_at_layer(
        &self,
        query: &Vector,
        mut current: ObjectId,
        layer: u16,
        visited: &mut BTreeSet<ObjectId>,
    ) -> Result<ObjectId, AnnError> {
        let mut current_distance =
            distance(self.definition.metric, query, &self.entry(current)?.vector)?;
        loop {
            let mut best = Candidate {
                object_id: current,
                distance: current_distance,
            };
            let neighbors = self
                .nodes
                .get(&current)
                .and_then(|node| node.neighbors.get(usize::from(layer)))
                .ok_or(AnnError::CorruptGraph)?;
            for neighbor in neighbors {
                visited.insert(*neighbor);
                let candidate = Candidate {
                    object_id: *neighbor,
                    distance: distance(
                        self.definition.metric,
                        query,
                        &self.entry(*neighbor)?.vector,
                    )?,
                };
                if compare_candidates(&candidate, &best) == Ordering::Less {
                    best = candidate;
                }
            }
            if best.object_id == current {
                return Ok(current);
            }
            current = best.object_id;
            current_distance = best.distance;
        }
    }

    fn search_layer(
        &self,
        query: &Vector,
        entry_points: &[ObjectId],
        layer: u16,
        breadth: usize,
        visited: &mut BTreeSet<ObjectId>,
    ) -> Result<Vec<Candidate>, AnnError> {
        let mut layer_visited = BTreeSet::new();
        let mut frontier = Vec::new();
        let mut best = Vec::new();
        for entry_point in entry_points {
            if !self.nodes.contains_key(entry_point) {
                return Err(AnnError::CorruptGraph);
            }
            layer_visited.insert(*entry_point);
            visited.insert(*entry_point);
            let candidate = Candidate {
                object_id: *entry_point,
                distance: distance(
                    self.definition.metric,
                    query,
                    &self.entry(*entry_point)?.vector,
                )?,
            };
            frontier.push(candidate);
            best.push(candidate);
        }
        sort_and_deduplicate_candidates(&mut frontier);
        sort_and_deduplicate_candidates(&mut best);
        best.truncate(breadth);
        while !frontier.is_empty() {
            frontier.sort_by(compare_candidates);
            let candidate = frontier.remove(0);
            if best.len() >= breadth
                && best
                    .last()
                    .is_some_and(|worst| compare_candidates(&candidate, worst) == Ordering::Greater)
            {
                break;
            }
            let neighbors = self
                .nodes
                .get(&candidate.object_id)
                .and_then(|node| node.neighbors.get(usize::from(layer)))
                .ok_or(AnnError::CorruptGraph)?;
            for neighbor in neighbors {
                if !layer_visited.insert(*neighbor) {
                    continue;
                }
                visited.insert(*neighbor);
                let neighbor = Candidate {
                    object_id: *neighbor,
                    distance: distance(
                        self.definition.metric,
                        query,
                        &self.entry(*neighbor)?.vector,
                    )?,
                };
                let admitted = best.len() < breadth
                    || best.last().is_some_and(|worst| {
                        compare_candidates(&neighbor, worst) == Ordering::Less
                    });
                if admitted {
                    frontier.push(neighbor);
                    best.push(neighbor);
                    best.sort_by(compare_candidates);
                    best.dedup_by_key(|candidate| candidate.object_id);
                    best.truncate(breadth);
                }
            }
        }
        Ok(best)
    }

    fn search_layer_filtered(
        &self,
        query: &Vector,
        entry_points: &[ObjectId],
        breadth: usize,
        allowlist: &BTreeSet<ObjectId>,
        visited: &mut BTreeSet<ObjectId>,
    ) -> Result<Vec<Candidate>, AnnError> {
        let mut layer_visited = BTreeSet::new();
        let mut frontier = Vec::new();
        let mut navigation = Vec::new();
        let mut eligible = Vec::new();
        for entry_point in entry_points {
            if !self.nodes.contains_key(entry_point) {
                return Err(AnnError::CorruptGraph);
            }
            layer_visited.insert(*entry_point);
            visited.insert(*entry_point);
            let candidate = Candidate {
                object_id: *entry_point,
                distance: distance(
                    self.definition.metric,
                    query,
                    &self.entry(*entry_point)?.vector,
                )?,
            };
            frontier.push(candidate);
            navigation.push(candidate);
            if allowlist.contains(entry_point) {
                eligible.push(candidate);
            }
        }
        sort_and_deduplicate_candidates(&mut frontier);
        sort_and_deduplicate_candidates(&mut navigation);
        sort_and_deduplicate_candidates(&mut eligible);
        navigation.truncate(breadth);
        eligible.truncate(breadth);
        while !frontier.is_empty() {
            frontier.sort_by(compare_candidates);
            let candidate = frontier.remove(0);
            if navigation.len() >= breadth
                && navigation
                    .last()
                    .is_some_and(|worst| compare_candidates(&candidate, worst) == Ordering::Greater)
                && eligible.len() >= breadth
            {
                break;
            }
            let neighbors = self
                .nodes
                .get(&candidate.object_id)
                .and_then(|node| node.neighbors.first())
                .ok_or(AnnError::CorruptGraph)?;
            for neighbor in neighbors {
                if !layer_visited.insert(*neighbor) {
                    continue;
                }
                visited.insert(*neighbor);
                let neighbor = Candidate {
                    object_id: *neighbor,
                    distance: distance(
                        self.definition.metric,
                        query,
                        &self.entry(*neighbor)?.vector,
                    )?,
                };
                let navigation_admitted = navigation.len() < breadth
                    || navigation.last().is_some_and(|worst| {
                        compare_candidates(&neighbor, worst) == Ordering::Less
                    });
                if navigation_admitted || allowlist.contains(&neighbor.object_id) {
                    frontier.push(neighbor);
                }
                if navigation_admitted {
                    navigation.push(neighbor);
                    navigation.sort_by(compare_candidates);
                    navigation.dedup_by_key(|candidate| candidate.object_id);
                    navigation.truncate(breadth);
                }
                if allowlist.contains(&neighbor.object_id) {
                    eligible.push(neighbor);
                    eligible.sort_by(compare_candidates);
                    eligible.dedup_by_key(|candidate| candidate.object_id);
                    eligible.truncate(breadth);
                }
            }
        }
        Ok(eligible)
    }

    fn level_for(&self, object_id: ObjectId) -> u16 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hyphae-hnsw-level-v1");
        hasher.update(&self.definition_digest);
        hasher.update(&object_id.get().to_be_bytes());
        let digest = hasher.finalize();
        let mut random_bytes = [0_u8; 8];
        random_bytes.copy_from_slice(&digest.as_bytes()[..8]);
        let mut random = u64::from_le_bytes(random_bytes);
        let divisor = u64::from(self.definition.config.m);
        let mut level = 0_u16;
        while level < MAX_HNSW_LEVEL && random % divisor == 0 {
            level += 1;
            random /= divisor;
        }
        level
    }

    fn entry(&self, object_id: ObjectId) -> Result<&Entry, AnnError> {
        self.entries.get(&object_id).ok_or(AnnError::CorruptGraph)
    }

    fn refresh_build_identity(&mut self) -> Result<(), AnnError> {
        self.build_identity = self.calculate_build_identity()?;
        Ok(())
    }

    fn calculate_build_identity(&self) -> Result<[u8; 32], AnnError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hyphae-hnsw-build");
        hasher.update(&BUILD_IDENTITY_VERSION.to_le_bytes());
        hasher.update(&self.definition.canonical_bytes());
        hasher.update(
            &u64::try_from(self.entries.len())
                .map_err(|_| AnnError::CorruptGraph)?
                .to_le_bytes(),
        );
        for (object_id, entry) in &self.entries {
            hasher.update(&object_id.get().to_be_bytes());
            hasher.update(&entry.creating_csn.get().to_le_bytes());
            for value in entry.vector.values() {
                hasher.update(&value.to_bits().to_le_bytes());
            }
        }
        hasher.update(&self.entry_point.map_or(0, ObjectId::get).to_be_bytes());
        hasher.update(&self.max_level.to_le_bytes());
        for (object_id, node) in &self.nodes {
            hasher.update(&object_id.get().to_be_bytes());
            hasher.update(&node.level.to_le_bytes());
            for neighbors in &node.neighbors {
                hasher.update(
                    &u16::try_from(neighbors.len())
                        .map_err(|_| AnnError::CorruptGraph)?
                        .to_le_bytes(),
                );
                for neighbor in neighbors {
                    hasher.update(&neighbor.get().to_be_bytes());
                }
            }
        }
        Ok(*hasher.finalize().as_bytes())
    }
}

impl PartitionedHnswIndex {
    /// Builds every planned partition with the canonical HNSW baseline.
    /// This serial convenience path is an oracle for the governed runtime
    /// executor, not the production bulk scheduler.
    ///
    /// # Errors
    ///
    /// Returns an error when a child build or plan validation fails.
    pub fn build(plan: &HnswPartitionPlan) -> Result<Self, AnnError> {
        let partitions = plan
            .partitions
            .iter()
            .cloned()
            .map(|records| HnswIndex::build(plan.definition, records))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_built_partitions(plan, partitions)
    }

    /// Validates externally built partitions against their immutable plan and
    /// assembles one deterministic fan-out index.
    ///
    /// # Errors
    ///
    /// Rejects reordered, missing, duplicated, or definition-mismatched child
    /// generations.
    pub fn from_built_partitions(
        plan: &HnswPartitionPlan,
        partitions: Vec<HnswIndex>,
    ) -> Result<Self, AnnError> {
        if partitions.len() != plan.partitions.len() {
            return Err(AnnError::CorruptGraph);
        }
        for (planned, built) in plan.partitions.iter().zip(&partitions) {
            if built.definition != plan.definition || built.entries.len() != planned.len() {
                return Err(AnnError::CorruptGraph);
            }
            for record in planned {
                let entry = built
                    .entries
                    .get(&record.object_id)
                    .ok_or(AnnError::CorruptGraph)?;
                if entry.creating_csn != record.creating_csn || entry.vector != record.vector {
                    return Err(AnnError::CorruptGraph);
                }
            }
        }
        let build_identity =
            partitioned_build_identity(plan.definition, plan.input_identity, &partitions)?;
        let summaries = summarize_partitions(&partitions)?;
        Ok(Self {
            definition: plan.definition,
            partitions,
            summaries,
            input_identity: plan.input_identity,
            build_identity,
        })
    }

    /// Assembles externally built child generations after independently
    /// reconstructing their geometric partition boundaries and plan identity.
    /// This path lets a governed executor transfer each partition into one
    /// worker without retaining a second full vector copy.
    ///
    /// # Errors
    ///
    /// Rejects an empty child, repeated identity, definition mismatch,
    /// noncanonical boundary, or input identity mismatch.
    pub fn from_governed_partitions(
        definition: VectorIndexDefinition,
        input_identity: [u8; 32],
        partitions: Vec<HnswIndex>,
    ) -> Result<Self, AnnError> {
        Self::from_governed_partitions_cancellable(definition, input_identity, partitions, || {
            ControlFlow::Continue(())
        })
    }

    /// Assembles governed child generations with cooperative cancellation
    /// throughout boundary reconstruction, identity validation, and routing
    /// summary construction.
    ///
    /// No fan-out index is returned unless every child and aggregate identity
    /// has been validated and every routing summary is complete.
    ///
    /// # Errors
    ///
    /// Returns [`AnnError::BuildCancelled`] when the controller requests
    /// cancellation, or the same validation errors as
    /// [`Self::from_governed_partitions`].
    pub fn from_governed_partitions_cancellable(
        definition: VectorIndexDefinition,
        input_identity: [u8; 32],
        partitions: Vec<HnswIndex>,
        mut control: impl FnMut() -> ControlFlow<()>,
    ) -> Result<Self, AnnError> {
        validate_governed_partitions(definition, input_identity, &partitions, &mut control)?;
        let build_identity = partitioned_build_identity_with_control(
            definition,
            input_identity,
            &partitions,
            &mut control,
        )?;
        let summaries = summarize_partitions_with_control(&partitions, &mut control)?;
        check_build_control(&mut control)?;
        Ok(Self {
            definition,
            partitions,
            summaries,
            input_identity,
            build_identity,
        })
    }

    /// Fixed definition shared by every child generation.
    pub const fn definition(&self) -> VectorIndexDefinition {
        self.definition
    }

    /// Exact vector count across disjoint partitions.
    pub fn len(&self) -> usize {
        self.partitions.iter().map(HnswIndex::len).sum()
    }

    /// Returns whether the fan-out index has no vectors.
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// Deterministic geometric input and boundary identity.
    pub const fn input_identity(&self) -> [u8; 32] {
        self.input_identity
    }

    /// Aggregate identity over the plan and every canonical child graph.
    pub const fn build_identity(&self) -> [u8; 32] {
        self.build_identity
    }

    /// Complete child generation count.
    pub fn partition_count(&self) -> usize {
        self.partitions.len()
    }

    /// Returns child persistence metadata without exporting any generation.
    pub fn generation_descriptors(&self) -> Vec<HnswGenerationDescriptor> {
        self.partitions
            .iter()
            .map(HnswIndex::generation_descriptor)
            .collect()
    }

    /// Returns one immutable vector version and its owning child partition.
    pub fn vector_record(&self, object_id: ObjectId) -> Option<(usize, Csn, &Vector)> {
        self.partitions
            .iter()
            .enumerate()
            .find_map(|(partition, index)| {
                index
                    .vector_record(object_id)
                    .map(|(creating_csn, vector)| (partition, creating_csn, vector))
            })
    }

    /// Returns one immutable vector version from an exact child partition.
    pub fn partition_vector_record(
        &self,
        partition: usize,
        object_id: ObjectId,
    ) -> Option<(Csn, &Vector)> {
        self.partitions
            .get(partition)
            .and_then(|index| index.vector_record(object_id))
    }

    /// Visits every immutable vector version in child order and stable
    /// object-ID order without exporting or cloning any child graph.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by `visitor`.
    pub fn try_for_each_vector_record<E>(
        &self,
        mut visitor: impl FnMut(usize, ObjectId, Csn, &Vector) -> Result<(), E>,
    ) -> Result<(), E> {
        for (partition, index) in self.partitions.iter().enumerate() {
            index.try_for_each_vector_record(|object_id, creating_csn, vector| {
                visitor(partition, object_id, creating_csn, vector)
            })?;
        }
        Ok(())
    }

    /// Canonical centroid/radius summaries in partition order.
    pub fn partition_summaries(&self) -> &[HnswPartitionSummary] {
        &self.summaries
    }

    /// Exports complete child generations in canonical partition order.
    pub fn export_snapshot(&self) -> PartitionedIndexSnapshot {
        PartitionedIndexSnapshot {
            definition: self.definition,
            input_identity: self.input_identity,
            build_identity: self.build_identity,
            partitions: self
                .partitions
                .iter()
                .map(HnswIndex::export_snapshot)
                .collect(),
        }
    }

    /// Transfers complete child generations into canonical persistence
    /// records without retaining or cloning the in-memory indexes.
    pub fn into_snapshot(self) -> PartitionedIndexSnapshot {
        let Self {
            definition,
            partitions,
            input_identity,
            build_identity,
            ..
        } = self;
        PartitionedIndexSnapshot {
            definition,
            input_identity,
            build_identity,
            partitions: partitions
                .into_iter()
                .map(HnswIndex::into_snapshot)
                .collect(),
        }
    }

    /// Restores and independently revalidates every child generation,
    /// recursive geometric boundary, plan identity, and aggregate identity.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed child state, reordered partitions, or
    /// any identity mismatch.
    pub fn restore_snapshot(snapshot: PartitionedIndexSnapshot) -> Result<Self, AnnError> {
        let partitions = snapshot
            .partitions
            .into_iter()
            .map(HnswIndex::restore_owned)
            .collect::<Result<Vec<_>, _>>()?;
        let restored = Self::from_governed_partitions(
            snapshot.definition,
            snapshot.input_identity,
            partitions,
        )?;
        if restored.build_identity != snapshot.build_identity {
            return Err(AnnError::CorruptGraph);
        }
        Ok(restored)
    }

    /// Executes exact fan-out and canonical top-k merge.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid query admission.
    pub fn search_exact(&self, query: &Vector, k: usize) -> Result<Vec<VectorHit>, AnnError> {
        let mut hits = self
            .partitions
            .iter()
            .map(|partition| partition.search_exact(query, k))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        hits.sort_by(compare_hits);
        hits.truncate(k);
        Ok(hits)
    }

    /// Fans one approximate query to every experimental partition and merges
    /// child top-k results canonically.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid query or search bounds.
    pub fn search(
        &self,
        query: &Vector,
        options: SearchOptions,
    ) -> Result<AnnSearchResult, AnnError> {
        Ok(self
            .search_selected(query, options, self.partitions.len().max(1))?
            .result)
    }

    /// Routes an approximate query to no more than `maximum_partitions`
    /// centroid-ranked children and merges their results canonically.
    ///
    /// # Errors
    ///
    /// Returns an error for zero selected capacity, invalid query, routing
    /// summary corruption, or ordinary child-search bounds.
    pub fn search_selected(
        &self,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
    ) -> Result<PartitionedAnnSearchResult, AnnError> {
        if maximum_partitions == 0 {
            return Err(AnnError::InvalidPartitionCount);
        }
        validate_vector(self.definition, query)?;
        if options.ef_search > usize::from(self.definition.config.ef_search_max) {
            return Err(AnnError::SearchBreadthExceeded);
        }
        let query_projection = deterministic_projection(self.definition.digest(), query);
        let mut selected = self
            .summaries
            .iter()
            .map(|summary| {
                Ok((
                    partition_routing_distance(self.definition.metric, query, summary)?,
                    projection_interval_distance(query_projection, summary),
                    summary.partition_index,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        selected.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.total_cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        selected.truncate(maximum_partitions.min(selected.len()));
        let children = selected
            .iter()
            .map(|(_, _, partition)| {
                self.partitions
                    .get(*partition)
                    .ok_or(AnnError::CorruptGraph)?
                    .search(query, options)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = merge_partitioned_search(self, options, &children)?;
        Ok(PartitionedAnnSearchResult {
            result,
            selected_partitions: selected
                .into_iter()
                .map(|(_, _, partition)| partition)
                .collect(),
            total_partitions: self.partitions.len(),
        })
    }
}

fn merge_partitioned_search(
    index: &PartitionedHnswIndex,
    options: SearchOptions,
    children: &[AnnSearchResult],
) -> Result<AnnSearchResult, AnnError> {
    let mut hits = children
        .iter()
        .flat_map(|child| child.hits.iter().copied())
        .collect::<Vec<_>>();
    hits.sort_by(compare_hits);
    hits.dedup_by_key(|hit| hit.object_id);
    hits.truncate(options.k);
    Ok(AnnSearchResult {
        approximate: true,
        build_identity: index.build_identity,
        metric: index.definition.metric,
        ef_search: options.ef_search,
        candidate_count: checked_child_sum(children, |child| child.candidate_count)?,
        eligible_candidate_count: checked_child_sum(children, |child| {
            child.eligible_candidate_count
        })?,
        strategy: AnnSearchStrategy::GraphTraversal,
        recall_risk: AnnRecallRisk::ApproximateTraversal,
        exact_reranked: options.exact_rerank.is_some(),
        visited_nodes: checked_child_sum(children, |child| child.visited_nodes)?,
        hits,
    })
}

fn summarize_partitions(partitions: &[HnswIndex]) -> Result<Vec<HnswPartitionSummary>, AnnError> {
    summarize_partitions_with_control(partitions, &mut || ControlFlow::Continue(()))
}

fn summarize_partitions_with_control(
    partitions: &[HnswIndex],
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<Vec<HnswPartitionSummary>, AnnError> {
    let mut summaries = Vec::with_capacity(partitions.len());
    for (partition, index) in partitions.iter().enumerate() {
        check_build_control(control)?;
        summaries.push(summarize_partition_with_control(partition, index, control)?);
    }
    Ok(summaries)
}

fn summarize_partition_with_control(
    partition_index: usize,
    index: &HnswIndex,
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<HnswPartitionSummary, AnnError> {
    check_build_control(control)?;
    let count = u32::try_from(index.entries.len()).map_err(|_| AnnError::LengthOverflow)?;
    if count == 0 {
        return Err(AnnError::CorruptGraph);
    }
    let mut centroid = vec![0.0_f64; usize::from(index.definition.dimension)];
    for entry in index.entries.values() {
        check_build_control(control)?;
        for (total, value) in centroid.iter_mut().zip(entry.vector.values()) {
            *total += f64::from(*value);
        }
    }
    let divisor = f64::from(count);
    for value in &mut centroid {
        *value /= divisor;
    }
    let mut representative = None::<(f64, ObjectId, &Vector)>;
    let mut squared_l2_radius = 0.0_f64;
    let projection_identity = index.definition.digest();
    let projection_weights = deterministic_projection_weights(
        projection_identity,
        0,
        usize::from(index.definition.dimension),
    );
    let mut projection_minimum = f64::INFINITY;
    let mut projection_maximum = f64::NEG_INFINITY;
    for (object_id, entry) in &index.entries {
        check_build_control(control)?;
        let projection = project_with_weights(&entry.vector, &projection_weights);
        projection_minimum = projection_minimum.min(projection);
        projection_maximum = projection_maximum.max(projection);
        let squared_l2 = entry
            .vector
            .values()
            .iter()
            .zip(&centroid)
            .map(|(value, center)| {
                let difference = f64::from(*value) - center;
                difference * difference
            })
            .sum::<f64>();
        squared_l2_radius = squared_l2_radius.max(squared_l2);
        if representative.as_ref().is_none_or(|current| {
            squared_l2
                .total_cmp(&current.0)
                .then_with(|| object_id.cmp(&current.1))
                == Ordering::Less
        }) {
            representative = Some((squared_l2, *object_id, &entry.vector));
        }
    }
    let (_, representative_object_id, routing_vector) =
        representative.ok_or(AnnError::CorruptGraph)?;
    check_build_control(control)?;
    Ok(HnswPartitionSummary {
        partition_index,
        vector_count: index.entries.len(),
        centroid: centroid.into_boxed_slice(),
        squared_l2_radius,
        projection_minimum,
        projection_maximum,
        representative_object_id,
        routing_vector: routing_vector.clone(),
    })
}

fn projection_interval_distance(query_projection: f64, summary: &HnswPartitionSummary) -> f64 {
    if query_projection < summary.projection_minimum {
        summary.projection_minimum - query_projection
    } else if query_projection > summary.projection_maximum {
        query_projection - summary.projection_maximum
    } else {
        0.0
    }
}

fn partition_routing_distance(
    metric: Metric,
    query: &Vector,
    summary: &HnswPartitionSummary,
) -> Result<f64, AnnError> {
    if query.dimension() != summary.centroid.len() {
        return Err(AnnError::CorruptGraph);
    }
    match metric {
        Metric::SquaredL2 => Ok(query
            .values()
            .iter()
            .zip(&summary.centroid)
            .map(|(query, center)| {
                let difference = f64::from(*query) - center;
                difference * difference
            })
            .sum()),
        Metric::NegativeDot => Ok(-query
            .values()
            .iter()
            .zip(&summary.centroid)
            .map(|(query, center)| f64::from(*query) * center)
            .sum::<f64>()),
        Metric::Cosine => {
            let centroid_norm = summary
                .centroid
                .iter()
                .map(|value| value * value)
                .sum::<f64>();
            if centroid_norm == 0.0 {
                return distance(metric, query, &summary.routing_vector);
            }
            let dot = query
                .values()
                .iter()
                .zip(&summary.centroid)
                .map(|(query, center)| f64::from(*query) * center)
                .sum::<f64>();
            Ok(1.0 - dot / (query.norm_squared.sqrt() * centroid_norm.sqrt()))
        }
    }
}

fn recursive_projection_partitions(
    definition: VectorIndexDefinition,
    records: Vec<VectorRecord>,
    partition_count: usize,
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<Vec<Vec<VectorRecord>>, AnnError> {
    check_build_control(control)?;
    let base_records = records.len() / partition_count;
    let remainder = records.len() % partition_count;
    let target_sizes = (0..partition_count)
        .map(|partition| base_records + usize::from(partition < remainder))
        .collect::<Vec<_>>();
    let mut partitions = Vec::with_capacity(partition_count);
    let mut split_sequence = 0_u64;
    split_projection_partition(
        definition.digest(),
        records,
        &target_sizes,
        &mut split_sequence,
        &mut partitions,
        control,
    )?;
    Ok(partitions)
}

fn split_projection_partition(
    projection_identity: [u8; 32],
    records: Vec<VectorRecord>,
    target_sizes: &[usize],
    split_sequence: &mut u64,
    partitions: &mut Vec<Vec<VectorRecord>>,
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<(), AnnError> {
    check_build_control(control)?;
    if target_sizes.len() == 1 {
        if records.len() != target_sizes[0] {
            return Err(AnnError::CorruptGraph);
        }
        partitions.push(records);
        return Ok(());
    }
    let axis = *split_sequence;
    *split_sequence = split_sequence
        .checked_add(1)
        .ok_or(AnnError::LengthOverflow)?;
    let mut records = sort_records_by_projection(records, projection_identity, axis, control)?;
    let left_partitions = target_sizes.len() / 2;
    let left_records = target_sizes[..left_partitions]
        .iter()
        .try_fold(0_usize, |total, count| total.checked_add(*count))
        .ok_or(AnnError::LengthOverflow)?;
    if left_records == 0 || left_records >= records.len() {
        return Err(AnnError::CorruptGraph);
    }
    let right = records.split_off(left_records);
    split_projection_partition(
        projection_identity,
        records,
        &target_sizes[..left_partitions],
        split_sequence,
        partitions,
        control,
    )?;
    split_projection_partition(
        projection_identity,
        right,
        &target_sizes[left_partitions..],
        split_sequence,
        partitions,
        control,
    )
}

fn check_build_control(control: &mut impl FnMut() -> ControlFlow<()>) -> Result<(), AnnError> {
    if control().is_break() {
        Err(AnnError::BuildCancelled)
    } else {
        Ok(())
    }
}

fn deterministic_projection(projection_identity: [u8; 32], vector: &Vector) -> f64 {
    deterministic_projection_axis(projection_identity, 0, vector)
}

fn deterministic_projection_axis(projection_identity: [u8; 32], axis: u64, vector: &Vector) -> f64 {
    let weights = deterministic_projection_weights(projection_identity, axis, vector.dimension());
    project_with_weights(vector, &weights)
}

fn deterministic_projection_weights(
    projection_identity: [u8; 32],
    axis: u64,
    dimension: usize,
) -> Vec<f64> {
    (0..dimension)
        .map(|component| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"hyphae-ann-balanced-projection-v1");
            hasher.update(&projection_identity);
            hasher.update(&axis.to_le_bytes());
            hasher.update(&u64::try_from(component).unwrap_or(u64::MAX).to_le_bytes());
            let digest = hasher.finalize();
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(&digest.as_bytes()[..8]);
            let bits = u64::from_le_bytes(bytes);
            f64::from((bits >> 32) as u32) - f64::from(u32::MAX) / 2.0
        })
        .collect()
}

fn project_with_weights(vector: &Vector, weights: &[f64]) -> f64 {
    debug_assert_eq!(vector.dimension(), weights.len());
    vector
        .values()
        .iter()
        .zip(weights)
        .map(|(value, weight)| f64::from(*value) * weight)
        .sum()
}

fn partition_plan_identity_with_control(
    definition: VectorIndexDefinition,
    partitions: &[Vec<VectorRecord>],
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<[u8; 32], AnnError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-partition-plan-v1");
    hasher.update(&definition.canonical_bytes());
    hasher.update(
        &u64::try_from(partitions.len())
            .map_err(|_| AnnError::LengthOverflow)?
            .to_le_bytes(),
    );
    for (partition, records) in partitions.iter().enumerate() {
        check_build_control(control)?;
        hasher.update(
            &u64::try_from(partition)
                .map_err(|_| AnnError::LengthOverflow)?
                .to_le_bytes(),
        );
        hasher.update(
            &u64::try_from(records.len())
                .map_err(|_| AnnError::LengthOverflow)?
                .to_le_bytes(),
        );
        for record in records {
            check_build_control(control)?;
            hasher.update(&record.object_id.get().to_be_bytes());
            hasher.update(&record.creating_csn.get().to_le_bytes());
            for value in record.vector.values() {
                hasher.update(&value.to_bits().to_le_bytes());
            }
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

fn partitioned_build_identity(
    definition: VectorIndexDefinition,
    input_identity: [u8; 32],
    partitions: &[HnswIndex],
) -> Result<[u8; 32], AnnError> {
    partitioned_build_identity_with_control(definition, input_identity, partitions, &mut || {
        ControlFlow::Continue(())
    })
}

fn partitioned_build_identity_with_control(
    definition: VectorIndexDefinition,
    input_identity: [u8; 32],
    partitions: &[HnswIndex],
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<[u8; 32], AnnError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-partitioned-hnsw-build-v1");
    hasher.update(&definition.canonical_bytes());
    hasher.update(&input_identity);
    for partition in partitions {
        check_build_control(control)?;
        hasher.update(&partition.build_identity);
        hasher.update(
            &u64::try_from(partition.len())
                .map_err(|_| AnnError::LengthOverflow)?
                .to_le_bytes(),
        );
    }
    check_build_control(control)?;
    Ok(*hasher.finalize().as_bytes())
}

#[derive(Clone, Copy)]
struct BuiltRecordRef<'a> {
    object_id: ObjectId,
    actual_partition: usize,
    entry: &'a Entry,
}

trait GeometricPartitionRecord {
    fn object_id(&self) -> ObjectId;

    fn vector(&self) -> &Vector;
}

impl GeometricPartitionRecord for VectorRecord {
    fn object_id(&self) -> ObjectId {
        self.object_id
    }

    fn vector(&self) -> &Vector {
        &self.vector
    }
}

impl GeometricPartitionRecord for BuiltRecordRef<'_> {
    fn object_id(&self) -> ObjectId {
        self.object_id
    }

    fn vector(&self) -> &Vector {
        &self.entry.vector
    }
}

fn sort_records_by_projection<T: GeometricPartitionRecord>(
    records: Vec<T>,
    projection_identity: [u8; 32],
    axis: u64,
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<Vec<T>, AnnError> {
    let dimension = records
        .first()
        .map(|record| record.vector().dimension())
        .ok_or(AnnError::CorruptGraph)?;
    let projection_weights = deterministic_projection_weights(projection_identity, axis, dimension);
    let mut projected = Vec::with_capacity(records.len());
    for record in records {
        check_build_control(control)?;
        projected.push((
            project_with_weights(record.vector(), &projection_weights),
            record,
        ));
    }
    projected.sort_by(|(left_projection, left), (right_projection, right)| {
        left_projection
            .total_cmp(right_projection)
            .then_with(|| left.object_id().cmp(&right.object_id()))
    });
    check_build_control(control)?;
    Ok(projected.into_iter().map(|(_, record)| record).collect())
}

fn split_built_record_refs<'a>(
    projection_identity: [u8; 32],
    records: Vec<BuiltRecordRef<'a>>,
    target_sizes: &[usize],
    split_sequence: &mut u64,
    partitions: &mut Vec<Vec<BuiltRecordRef<'a>>>,
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<(), AnnError> {
    check_build_control(control)?;
    if target_sizes.len() == 1 {
        if records.len() != target_sizes[0] {
            return Err(AnnError::CorruptGraph);
        }
        partitions.push(records);
        return Ok(());
    }
    let axis = *split_sequence;
    *split_sequence = split_sequence
        .checked_add(1)
        .ok_or(AnnError::LengthOverflow)?;
    let mut records = sort_records_by_projection(records, projection_identity, axis, control)?;
    let left_partitions = target_sizes.len() / 2;
    let left_records = target_sizes[..left_partitions]
        .iter()
        .try_fold(0_usize, |total, count| total.checked_add(*count))
        .ok_or(AnnError::LengthOverflow)?;
    if left_records == 0 || left_records >= records.len() {
        return Err(AnnError::CorruptGraph);
    }
    let right = records.split_off(left_records);
    split_built_record_refs(
        projection_identity,
        records,
        &target_sizes[..left_partitions],
        split_sequence,
        partitions,
        control,
    )?;
    split_built_record_refs(
        projection_identity,
        right,
        &target_sizes[left_partitions..],
        split_sequence,
        partitions,
        control,
    )
}

fn validate_governed_partitions(
    definition: VectorIndexDefinition,
    input_identity: [u8; 32],
    partitions: &[HnswIndex],
    control: &mut impl FnMut() -> ControlFlow<()>,
) -> Result<(), AnnError> {
    check_build_control(control)?;
    if partitions.is_empty() {
        return if input_identity == partition_plan_identity_with_control(definition, &[], control)?
        {
            Ok(())
        } else {
            Err(AnnError::CorruptGraph)
        };
    }
    let projection_identity = definition.digest();
    let mut identities = BTreeSet::new();
    let mut records = Vec::new();
    for (partition, index) in partitions.iter().enumerate() {
        check_build_control(control)?;
        if index.definition != definition || index.entries.is_empty() {
            return Err(AnnError::CorruptGraph);
        }
        for (object_id, entry) in &index.entries {
            check_build_control(control)?;
            if !identities.insert(*object_id) {
                return Err(AnnError::CorruptGraph);
            }
            records.push(BuiltRecordRef {
                object_id: *object_id,
                actual_partition: partition,
                entry,
            });
        }
    }
    records.sort_by_key(|record| record.object_id);
    let base_records = records.len() / partitions.len();
    let remainder = records.len() % partitions.len();
    let target_sizes = (0..partitions.len())
        .map(|partition| base_records + usize::from(partition < remainder))
        .collect::<Vec<_>>();
    let mut expected_partitions = Vec::with_capacity(partitions.len());
    let mut split_sequence = 0_u64;
    split_built_record_refs(
        projection_identity,
        records,
        &target_sizes,
        &mut split_sequence,
        &mut expected_partitions,
        control,
    )?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-partition-plan-v1");
    hasher.update(&definition.canonical_bytes());
    hasher.update(
        &u64::try_from(partitions.len())
            .map_err(|_| AnnError::LengthOverflow)?
            .to_le_bytes(),
    );
    for (partition, records) in expected_partitions.iter().enumerate() {
        check_build_control(control)?;
        if records
            .iter()
            .any(|record| record.actual_partition != partition)
        {
            return Err(AnnError::CorruptGraph);
        }
        hasher.update(
            &u64::try_from(partition)
                .map_err(|_| AnnError::LengthOverflow)?
                .to_le_bytes(),
        );
        hasher.update(
            &u64::try_from(records.len())
                .map_err(|_| AnnError::LengthOverflow)?
                .to_le_bytes(),
        );
        for record in records {
            check_build_control(control)?;
            hasher.update(&record.object_id.get().to_be_bytes());
            hasher.update(&record.entry.creating_csn.get().to_le_bytes());
            for value in record.entry.vector.values() {
                hasher.update(&value.to_bits().to_le_bytes());
            }
        }
    }
    if input_identity != *hasher.finalize().as_bytes() {
        return Err(AnnError::CorruptGraph);
    }
    check_build_control(control)?;
    Ok(())
}

fn checked_child_sum(
    children: &[AnnSearchResult],
    value: impl Fn(&AnnSearchResult) -> usize,
) -> Result<usize, AnnError> {
    children.iter().try_fold(0_usize, |total, child| {
        total
            .checked_add(value(child))
            .ok_or(AnnError::LengthOverflow)
    })
}

fn validate_vector(definition: VectorIndexDefinition, vector: &Vector) -> Result<(), AnnError> {
    if vector.dimension() != usize::from(definition.dimension) {
        return Err(AnnError::DimensionMismatch);
    }
    if definition.metric == Metric::Cosine
        && vector.values().iter().all(|component| *component == 0.0)
    {
        return Err(AnnError::ZeroCosineVector);
    }
    Ok(())
}

fn distance(metric: Metric, left: &Vector, right: &Vector) -> Result<f64, AnnError> {
    if left.dimension() != right.dimension() {
        return Err(AnnError::DimensionMismatch);
    }
    match metric {
        Metric::Cosine => {
            let mut dot = 0.0_f64;
            for (left, right) in left.values().iter().zip(right.values()) {
                let left = f64::from(*left);
                let right = f64::from(*right);
                dot += left * right;
            }
            if left.norm_squared == 0.0 || right.norm_squared == 0.0 {
                return Err(AnnError::ZeroCosineVector);
            }
            Ok(1.0 - dot / (left.norm_squared.sqrt() * right.norm_squared.sqrt()))
        }
        Metric::NegativeDot => Ok(-left
            .values()
            .iter()
            .zip(right.values())
            .map(|(left, right)| f64::from(*left) * f64::from(*right))
            .sum::<f64>()),
        Metric::SquaredL2 => Ok(left
            .values()
            .iter()
            .zip(right.values())
            .map(|(left, right)| {
                let difference = f64::from(*left) - f64::from(*right);
                difference * difference
            })
            .sum()),
    }
}

fn compare_candidates(left: &Candidate, right: &Candidate) -> Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.object_id.cmp(&right.object_id))
}

fn strategy(allowlist: Option<&BTreeSet<ObjectId>>) -> AnnSearchStrategy {
    if allowlist.is_some() {
        AnnSearchStrategy::StableIdEligibilityTraversal
    } else {
        AnnSearchStrategy::GraphTraversal
    }
}

fn recall_risk(allowlist: Option<&BTreeSet<ObjectId>>) -> AnnRecallRisk {
    if allowlist.is_some() {
        AnnRecallRisk::FilteredApproximateTraversal
    } else {
        AnnRecallRisk::ApproximateTraversal
    }
}

fn compare_hits(left: &VectorHit, right: &VectorHit) -> Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.object_id.cmp(&right.object_id))
}

fn sort_and_deduplicate_candidates(candidates: &mut Vec<Candidate>) {
    candidates.sort_by(compare_candidates);
    candidates.dedup_by_key(|candidate| candidate.object_id);
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, ops::ControlFlow};

    use hyphae_native_types::{Csn, ObjectId};

    use super::{
        AnnError, AnnRecallRisk, AnnSearchStrategy, HnswConfig, HnswIndex, HnswPartitionPlan,
        HnswPartitionSummary, Metric, PartitionedHnswIndex, SearchOptions, Vector,
        VectorIndexDefinition, VectorRecord,
    };

    fn object(value: u128) -> Result<ObjectId, Box<dyn std::error::Error>> {
        Ok(ObjectId::new(value)?)
    }

    fn csn(value: u64) -> Result<Csn, Box<dyn std::error::Error>> {
        Ok(Csn::new(value)?)
    }

    fn definition(metric: Metric) -> Result<VectorIndexDefinition, Box<dyn std::error::Error>> {
        Ok(VectorIndexDefinition::new(
            object(91)?,
            2,
            metric,
            HnswConfig::new(8, 32, 16, 128, 0x5eed)?,
        )?)
    }

    fn deterministic_vector(
        object_id: u16,
        dimension: u16,
    ) -> Result<Vector, Box<dyn std::error::Error>> {
        let mut state = u64::from(object_id) ^ 0x9e37_79b9_7f4a_7c15;
        let mut values = Vec::with_capacity(usize::from(dimension));
        for component in 0..dimension {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state ^= u64::from(component).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let raw = u16::try_from((state >> 16) & u64::from(u16::MAX))?;
            values.push(f32::from(raw) / 32_767.5 - 1.0);
        }
        Ok(Vector::new(values)?)
    }

    fn restore_fixture() -> Result<HnswIndex, Box<dyn std::error::Error>> {
        let mut index = HnswIndex::new(definition(Metric::Cosine)?)?;
        for value in 1..=32_u16 {
            index.upsert(
                object(u128::from(value))?,
                csn(u64::from(value))?,
                deterministic_vector(value, 2)?,
            )?;
        }
        Ok(index)
    }

    #[test]
    fn every_metric_matches_the_exact_oracle_for_axis_vectors()
    -> Result<(), Box<dyn std::error::Error>> {
        for metric in [Metric::Cosine, Metric::NegativeDot, Metric::SquaredL2] {
            let mut index = HnswIndex::new(definition(metric)?)?;
            index.upsert(object(1)?, csn(1)?, Vector::new([1.0, 0.0])?)?;
            index.upsert(object(2)?, csn(2)?, Vector::new([0.0, 1.0])?)?;
            index.upsert(object(3)?, csn(3)?, Vector::new([-1.0, 0.0])?)?;

            let query = Vector::new([1.0, 0.0])?;
            let exact = index.search_exact(&query, 3)?;
            let approximate = index.search(&query, SearchOptions::new(3, 16, None)?)?;
            assert_eq!(approximate.hits, exact);
            assert!(approximate.approximate);
            assert_eq!(exact[0].object_id, object(1)?);
        }
        Ok(())
    }

    #[test]
    fn rebuild_is_deterministic_across_mutation_arrival_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::Cosine)?;
        let entries = [
            (object(4)?, csn(4)?, Vector::new([0.8, 0.2])?),
            (object(2)?, csn(2)?, Vector::new([0.2, 0.8])?),
            (object(3)?, csn(3)?, Vector::new([0.6, 0.4])?),
            (object(1)?, csn(1)?, Vector::new([1.0, 0.0])?),
        ];
        let mut forward = HnswIndex::new(definition)?;
        for (id, creating_csn, vector) in &entries {
            forward.upsert(*id, *creating_csn, vector.clone())?;
        }
        let mut reverse = HnswIndex::new(definition)?;
        for (id, creating_csn, vector) in entries.iter().rev() {
            reverse.upsert(*id, *creating_csn, vector.clone())?;
        }

        assert_eq!(forward.build_identity(), reverse.build_identity());
        assert_eq!(forward.export_graph(), reverse.export_graph());
        Ok(())
    }

    #[test]
    fn build_progress_is_monotonic_and_preserves_the_canonical_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::Cosine)?;
        let records = (1..=64_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 2)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let expected = HnswIndex::build(definition, records.clone())?;
        let mut progress = Vec::new();

        let observed = HnswIndex::build_with_progress(definition, records, |value| {
            progress.push(value);
        })?;

        assert_eq!(observed.export_snapshot(), expected.export_snapshot());
        assert_eq!(progress.len(), 65);
        assert_eq!(
            progress
                .first()
                .map(|value| (value.completed(), value.total())),
            Some((0, 64))
        );
        assert_eq!(
            progress
                .last()
                .map(|value| (value.completed(), value.total())),
            Some((64, 64))
        );
        assert!(progress.windows(2).all(|values| {
            values[1].completed() == values[0].completed() + 1
                && values[1].total() == values[0].total()
        }));
        Ok(())
    }

    #[test]
    fn cancellable_build_success_matches_the_canonical_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::Cosine)?;
        let records = (1..=32_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 2)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let expected = HnswIndex::build(definition, records.clone())?;
        let mut observations = Vec::new();

        let built = HnswIndex::build_cancellable(definition, records, |progress| {
            observations.push((progress.completed(), progress.total()));
            ControlFlow::Continue(())
        })?;

        assert_eq!(built.export_snapshot(), expected.export_snapshot());
        assert_eq!(observations.first(), Some(&(0, 32)));
        assert_eq!(observations.last(), Some(&(32, 32)));
        Ok(())
    }

    #[test]
    fn cancellable_build_stops_at_the_requested_checkpoint_without_returning_an_index()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::SquaredL2)?;
        let records = (1..=32_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 2)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let mut observations = Vec::new();

        let result = HnswIndex::build_cancellable(definition, records, |progress| {
            observations.push((progress.completed(), progress.total()));
            if progress.completed() == 7 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });

        assert_eq!(result, Err(AnnError::BuildCancelled));
        assert_eq!(observations.first(), Some(&(0, 32)));
        assert_eq!(observations.last(), Some(&(7, 32)));
        assert_eq!(observations.len(), 8);
        Ok(())
    }

    #[test]
    fn owned_cancellable_build_stops_during_partition_ingestion()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::SquaredL2)?;
        let records = (1..=1_024_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 2)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let mut checkpoints = 0_usize;

        let result = HnswIndex::build_owned_cancellable(definition, records, || {
            checkpoints += 1;
            if checkpoints == 7 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });

        assert_eq!(result, Err(AnnError::BuildCancelled));
        assert_eq!(checkpoints, 7);
        Ok(())
    }

    #[test]
    fn canonical_build_identity_is_frozen() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            restore_fixture()?.build_identity(),
            [
                167, 22, 41, 248, 199, 113, 23, 225, 118, 147, 152, 216, 175, 136, 150, 106, 67,
                221, 74, 124, 123, 13, 145, 10, 59, 203, 190, 237, 196, 46, 220, 30,
            ]
        );
        Ok(())
    }

    #[test]
    fn updates_and_deletes_rebuild_without_stale_neighbors()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut index = HnswIndex::new(definition(Metric::SquaredL2)?)?;
        index.upsert(object(1)?, csn(1)?, Vector::new([1.0, 0.0])?)?;
        index.upsert(object(2)?, csn(2)?, Vector::new([0.0, 1.0])?)?;
        index.upsert(object(1)?, csn(3)?, Vector::new([-1.0, 0.0])?)?;
        assert_eq!(
            index.search_exact(&Vector::new([1.0, 0.0])?, 1)?[0].object_id,
            object(2)?
        );
        assert!(index.delete(object(2)?)?);
        assert!(!index.delete(object(2)?)?);
        index.validate()?;
        assert_eq!(index.len(), 1);
        Ok(())
    }

    #[test]
    fn admission_rejects_nonfinite_wrong_dimension_and_zero_cosine_vectors()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Vector::new([f32::NAN]), Err(AnnError::NonFiniteComponent));
        let mut index = HnswIndex::new(definition(Metric::Cosine)?)?;
        index.upsert(object(1)?, csn(1)?, Vector::new([1.0, 0.0])?)?;
        let original_identity = index.build_identity();
        assert_eq!(
            index.upsert(object(1)?, csn(2)?, Vector::new([1.0])?),
            Err(AnnError::DimensionMismatch)
        );
        assert_eq!(
            index.upsert(object(1)?, csn(2)?, Vector::new([-0.0, 0.0])?),
            Err(AnnError::ZeroCosineVector)
        );
        assert_eq!(index.build_identity(), original_identity);
        Ok(())
    }

    #[test]
    fn equal_distances_use_object_id_as_the_final_tie_breaker()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut index = HnswIndex::new(definition(Metric::SquaredL2)?)?;
        index.upsert(object(2)?, csn(2)?, Vector::new([-1.0, 0.0])?)?;
        index.upsert(object(1)?, csn(1)?, Vector::new([1.0, 0.0])?)?;
        let hits = index.search_exact(&Vector::new([0.0, 0.0])?, 2)?;
        assert_eq!(hits[0].object_id, object(1)?);
        assert_eq!(hits[1].object_id, object(2)?);
        Ok(())
    }

    #[test]
    fn restore_rejects_any_noncanonical_graph_record() -> Result<(), Box<dyn std::error::Error>> {
        let index = restore_fixture()?;
        let mut snapshot = index.export_snapshot();
        snapshot.build_identity[0] ^= 0xff;
        assert_eq!(HnswIndex::restore(&snapshot), Err(AnnError::CorruptGraph));

        let mut graph_corruption = index.export_snapshot();
        let neighbors = graph_corruption
            .nodes
            .iter_mut()
            .flat_map(|node| node.neighbors.iter_mut())
            .find(|neighbors| !neighbors.is_empty())
            .ok_or("quality fixture did not produce a graph edge")?;
        neighbors.clear();
        assert_eq!(
            HnswIndex::restore(&graph_corruption),
            Err(AnnError::CorruptGraph)
        );
        Ok(())
    }

    #[test]
    fn restore_preserves_canonical_generation_and_search_results()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = restore_fixture()?;
        let snapshot = index.export_snapshot();
        let restored = HnswIndex::restore(&snapshot)?;
        let restored_owned = HnswIndex::restore_owned(snapshot.clone())?;
        let canonical = HnswIndex::restore_canonical(&snapshot)?;
        let query = deterministic_vector(91, 2)?;
        let options = SearchOptions::new(10, 32, Some(10))?;

        assert_eq!(restored.export_snapshot(), snapshot);
        assert_eq!(restored_owned.export_snapshot(), snapshot);
        assert_eq!(canonical.export_snapshot(), snapshot);
        assert_eq!(index.clone().into_snapshot(), snapshot);
        assert_eq!(
            restored.search(&query, options)?,
            index.search(&query, options)?
        );
        Ok(())
    }

    #[test]
    fn restore_rejects_noncanonical_record_order() -> Result<(), Box<dyn std::error::Error>> {
        let index = restore_fixture()?;

        let mut vectors = index.export_snapshot();
        vectors.vectors.swap(0, 1);
        assert_eq!(HnswIndex::restore(&vectors), Err(AnnError::CorruptGraph));

        let mut nodes = index.export_snapshot();
        nodes.nodes.swap(0, 1);
        assert_eq!(HnswIndex::restore(&nodes), Err(AnnError::CorruptGraph));

        let mut neighbors = index.export_snapshot();
        let layer = neighbors
            .nodes
            .iter_mut()
            .flat_map(|node| node.neighbors.iter_mut())
            .find(|layer| layer.len() >= 2)
            .ok_or("quality fixture did not produce two graph neighbors")?;
        layer.swap(0, 1);
        assert_eq!(HnswIndex::restore(&neighbors), Err(AnnError::CorruptGraph));
        Ok(())
    }

    #[test]
    fn canonical_build_rejects_duplicate_object_ids() -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition(Metric::SquaredL2)?;
        let duplicate = object(1)?;
        let records = [
            VectorRecord {
                object_id: duplicate,
                creating_csn: csn(1)?,
                vector: Vector::new([1.0, 0.0])?,
            },
            VectorRecord {
                object_id: duplicate,
                creating_csn: csn(2)?,
                vector: Vector::new([0.0, 1.0])?,
            },
        ];
        assert_eq!(
            HnswIndex::build(definition, records),
            Err(AnnError::DuplicateObjectId)
        );
        Ok(())
    }

    #[test]
    fn search_bounds_are_enforced_before_traversal() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            SearchOptions::new(3, 2, None),
            Err(AnnError::InvalidSearchOptions)
        );
        let index = HnswIndex::new(definition(Metric::Cosine)?)?;
        assert_eq!(
            index.search(&Vector::new([1.0, 0.0])?, SearchOptions::new(1, 129, None)?),
            Err(AnnError::SearchBreadthExceeded)
        );
        Ok(())
    }

    #[test]
    fn exact_and_ann_filtered_use_the_same_stable_id_allowlist()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut index = HnswIndex::new(definition(Metric::SquaredL2)?)?;
        for value in 1..=32_u16 {
            index.upsert(
                object(u128::from(value))?,
                csn(u64::from(value))?,
                Vector::new([f32::from(value), 1.0])?,
            )?;
        }
        let allowlist = [object(2)?, object(7)?, object(19)?]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let query = Vector::new([6.5, 1.0])?;
        let exact = index.search_exact_filtered(&query, 3, &allowlist)?;
        let approximate =
            index.search_filtered(&query, SearchOptions::new(3, 32, Some(32))?, &allowlist)?;

        assert_eq!(approximate.hits, exact);
        assert_eq!(
            approximate.strategy,
            AnnSearchStrategy::StableIdAdaptiveExact
        );
        assert_eq!(
            approximate.recall_risk,
            AnnRecallRisk::ExactFilteredCandidates
        );
        assert_eq!(approximate.eligible_candidate_count, 3);
        assert!(approximate.candidate_count >= approximate.eligible_candidate_count);
        assert!(
            approximate
                .hits
                .iter()
                .all(|hit| allowlist.contains(&hit.object_id))
        );
        Ok(())
    }

    #[test]
    fn filtered_ann_is_bounded_and_fails_closed_for_empty_or_unknown_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut index = HnswIndex::new(definition(Metric::SquaredL2)?)?;
        for value in 1..=16_u16 {
            index.upsert(
                object(u128::from(value))?,
                csn(u64::from(value))?,
                Vector::new([f32::from(value), 1.0])?,
            )?;
        }
        let query = Vector::new([1.0, 1.0])?;
        for allowlist in [BTreeSet::new(), BTreeSet::from([object(999)?])] {
            let result =
                index.search_filtered(&query, SearchOptions::new(4, 4, None)?, &allowlist)?;
            assert!(result.hits.is_empty());
            assert_eq!(result.eligible_candidate_count, 0);
            assert!(result.candidate_count <= 4);
        }
        assert!(
            index
                .search_exact_filtered(&query, 4, &BTreeSet::new())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn filtered_layer_zero_eligibility_does_not_spend_candidates_on_connectors()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = VectorIndexDefinition::new(
            object(93)?,
            2,
            Metric::SquaredL2,
            HnswConfig::new(4, 24, 4, 32, 0xfeed)?,
        )?;
        let records = (1..=96_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: Vector::new([f32::from(value), 0.0])?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let index = HnswIndex::build(definition, records)?;
        let allowlist = (1..=96_u16)
            .filter(|value| value % 3 == 0)
            .map(|value| object(u128::from(value)))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let query = Vector::new([47.5, 0.0])?;
        let exact = index.search_exact_filtered(&query, 4, &allowlist)?;
        let filtered =
            index.search_filtered(&query, SearchOptions::new(4, 8, Some(8))?, &allowlist)?;

        assert_eq!(filtered.hits, exact);
        assert_eq!(
            filtered.strategy,
            AnnSearchStrategy::StableIdEligibilityTraversal
        );
        assert_eq!(
            filtered.recall_risk,
            AnnRecallRisk::FilteredApproximateTraversal
        );
        assert!(filtered.eligible_candidate_count >= 4);
        assert!(filtered.eligible_candidate_count <= 8);
        assert!(filtered.candidate_count >= filtered.eligible_candidate_count);
        Ok(())
    }

    #[test]
    fn geometric_partition_plan_is_balanced_and_input_order_independent()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = VectorIndexDefinition::new(
            object(94)?,
            8,
            Metric::Cosine,
            HnswConfig::new(8, 48, 32, 64, 0xbeef)?,
        )?;
        let records = (1..=257_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 8)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let forward = HnswPartitionPlan::build(definition, records.clone(), 8)?;
        let reverse = HnswPartitionPlan::build(definition, records.into_iter().rev(), 8)?;
        assert_eq!(forward, reverse);
        assert_eq!(forward.partition_count(), 8);
        let sizes = forward
            .partitions()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>();
        assert_eq!(sizes.iter().sum::<usize>(), 257);
        let maximum = sizes.iter().copied().max().ok_or("missing partition")?;
        let minimum = sizes.iter().copied().min().ok_or("missing partition")?;
        assert!(maximum - minimum <= 1);
        assert!(matches!(
            HnswPartitionPlan::build(definition, Vec::<VectorRecord>::new(), 0),
            Err(AnnError::InvalidPartitionCount)
        ));
        let cancellable_records = (1..=1_024_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 8)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let mut checkpoints = 0_usize;
        assert!(matches!(
            HnswPartitionPlan::build_cancellable(definition, cancellable_records, 8, || {
                checkpoints += 1;
                if checkpoints == 7 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },),
            Err(AnnError::BuildCancelled)
        ));
        assert_eq!(checkpoints, 7);
        Ok(())
    }

    fn assert_partitioned_snapshot_validation(
        index: &PartitionedHnswIndex,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = index.export_snapshot();
        assert_eq!(index.clone().into_snapshot(), snapshot);
        assert_eq!(
            PartitionedHnswIndex::restore_snapshot(snapshot.clone())?,
            *index
        );
        let mut reordered = snapshot.clone();
        reordered.partitions.swap(0, 1);
        assert!(matches!(
            PartitionedHnswIndex::restore_snapshot(reordered),
            Err(AnnError::CorruptGraph)
        ));
        let mut wrong_identity = snapshot;
        wrong_identity.build_identity[0] ^= 1;
        assert!(matches!(
            PartitionedHnswIndex::restore_snapshot(wrong_identity),
            Err(AnnError::CorruptGraph)
        ));
        Ok(())
    }

    #[test]
    fn partitioned_hnsw_exact_merge_and_build_identity_are_canonical()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = VectorIndexDefinition::new(
            object(95)?,
            8,
            Metric::Cosine,
            HnswConfig::new(16, 96, 64, 128, 0xcafe)?,
        )?;
        let records = (1..=256_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 8)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let canonical = HnswIndex::build(definition, records.clone())?;
        let first_plan = HnswPartitionPlan::build(definition, records.clone(), 4)?;
        let first = PartitionedHnswIndex::build(&first_plan)?;
        let second_plan = HnswPartitionPlan::build(definition, records.into_iter().rev(), 4)?;
        let second = PartitionedHnswIndex::build(&second_plan)?;
        let governed_plan =
            HnswPartitionPlan::build(definition, canonical.export_snapshot().vectors, 4)?;
        let governed_identity = governed_plan.input_identity();
        let governed_children = governed_plan
            .into_partitions()
            .into_iter()
            .map(|partition| HnswIndex::build(definition, partition))
            .collect::<Result<Vec<_>, _>>()?;
        let governed = PartitionedHnswIndex::from_governed_partitions(
            definition,
            governed_identity,
            governed_children.clone(),
        )?;
        assert_eq!(first, second);
        assert_eq!(first, governed);
        assert_partitioned_snapshot_validation(&first)?;
        assert_eq!(first.len(), 256);
        assert_eq!(first.partition_count(), 4);
        assert_eq!(first.partition_summaries().len(), 4);
        assert_eq!(
            first
                .partition_summaries()
                .iter()
                .map(HnswPartitionSummary::vector_count)
                .sum::<usize>(),
            256
        );
        assert!(first.partition_summaries().iter().all(|summary| {
            summary.centroid().len() == 8
                && summary.squared_l2_radius().is_finite()
                && summary.projection_minimum() <= summary.projection_maximum()
        }));
        let mut reordered = governed_children;
        reordered.swap(0, 1);
        assert!(matches!(
            PartitionedHnswIndex::from_governed_partitions(
                definition,
                governed_identity,
                reordered
            ),
            Err(AnnError::CorruptGraph)
        ));
        let mut recalled = 0_usize;
        let mut expected = 0_usize;
        for query_id in 1..=16_u16 {
            let query = deterministic_vector(query_id, 8)?;
            assert_eq!(
                first.search_exact(&query, 10)?,
                canonical.search_exact(&query, 10)?
            );
            let approximate = first.search(&query, SearchOptions::new(10, 64, Some(64))?)?;
            let all_partitions =
                first.search_selected(&query, SearchOptions::new(10, 64, Some(64))?, 4)?;
            assert_eq!(all_partitions.result, approximate);
            assert_eq!(all_partitions.selected_partitions.len(), 4);
            assert_eq!(all_partitions.total_partitions, 4);
            let selected =
                first.search_selected(&query, SearchOptions::new(10, 64, Some(64))?, 2)?;
            assert_eq!(selected.selected_partitions.len(), 2);
            assert_eq!(selected.total_partitions, 4);
            assert!(matches!(
                first.search_selected(&query, SearchOptions::new(10, 64, Some(64))?, 0),
                Err(AnnError::InvalidPartitionCount)
            ));
            let exact_ids = first
                .search_exact(&query, 10)?
                .into_iter()
                .map(|hit| hit.object_id)
                .collect::<BTreeSet<_>>();
            recalled += approximate
                .hits
                .iter()
                .filter(|hit| exact_ids.contains(&hit.object_id))
                .count();
            expected += exact_ids.len();
        }
        assert!(recalled * 100 >= expected * 95);
        Ok(())
    }

    #[test]
    fn cancellable_governed_assembly_preserves_the_exact_canonical_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = VectorIndexDefinition::new(
            object(97)?,
            8,
            Metric::Cosine,
            HnswConfig::new(8, 48, 32, 64, 0xfade)?,
        )?;
        let records = (1..=64_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 8)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let plan = HnswPartitionPlan::build(definition, records, 4)?;
        let input_identity = plan.input_identity();
        let children = plan
            .into_partitions()
            .into_iter()
            .map(|partition| HnswIndex::build(definition, partition))
            .collect::<Result<Vec<_>, _>>()?;

        let expected = PartitionedHnswIndex::from_governed_partitions(
            definition,
            input_identity,
            children.clone(),
        )?;
        let observed = PartitionedHnswIndex::from_governed_partitions_cancellable(
            definition,
            input_identity,
            children,
            || ControlFlow::Continue(()),
        )?;

        assert_eq!(observed, expected);
        assert_eq!(observed.build_identity(), expected.build_identity());
        assert_eq!(observed.input_identity(), expected.input_identity());
        Ok(())
    }

    #[test]
    fn governed_partition_assembly_cancels_during_boundary_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = VectorIndexDefinition::new(
            object(97)?,
            8,
            Metric::Cosine,
            HnswConfig::new(8, 48, 32, 64, 0xfade)?,
        )?;
        let records = (1..=64_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, 8)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let plan = HnswPartitionPlan::build(definition, records, 4)?;
        let input_identity = plan.input_identity();
        let children = plan
            .into_partitions()
            .into_iter()
            .map(|partition| HnswIndex::build(definition, partition))
            .collect::<Result<Vec<_>, _>>()?;
        let records = children.iter().map(HnswIndex::len).sum::<usize>();
        let cancel_after_collection = records + children.len() + 8;
        let mut checkpoints = 0_usize;

        let result = PartitionedHnswIndex::from_governed_partitions_cancellable(
            definition,
            input_identity,
            children,
            || {
                checkpoints += 1;
                if checkpoints == cancel_after_collection {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        );

        assert_eq!(result, Err(AnnError::BuildCancelled));
        assert_eq!(checkpoints, cancel_after_collection);
        assert!(checkpoints > records);
        Ok(())
    }

    #[test]
    fn deterministic_quality_corpus_meets_the_bounded_recall_floor()
    -> Result<(), Box<dyn std::error::Error>> {
        const VECTOR_COUNT: u16 = 512;
        const DIMENSION: u16 = 8;
        const QUERY_COUNT: u16 = 32;
        const K: usize = 10;

        let definition = VectorIndexDefinition::new(
            object(92)?,
            DIMENSION,
            Metric::Cosine,
            HnswConfig::new(16, 96, 64, 128, 0xc0de)?,
        )?;
        let records = (1..=VECTOR_COUNT)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: object(u128::from(value))?,
                    creating_csn: csn(u64::from(value))?,
                    vector: deterministic_vector(value, DIMENSION)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let index = HnswIndex::build(definition, records)?;
        let mut recalled = 0_usize;
        for query_id in 1..=QUERY_COUNT {
            let query = deterministic_vector(query_id, DIMENSION)?;
            let exact = index.search_exact(&query, K)?;
            let approximate = index.search(&query, SearchOptions::new(K, 64, Some(64))?)?;
            let exact_ids = exact
                .iter()
                .map(|hit| hit.object_id)
                .collect::<std::collections::BTreeSet<_>>();
            recalled = recalled.saturating_add(
                approximate
                    .hits
                    .iter()
                    .filter(|hit| exact_ids.contains(&hit.object_id))
                    .count(),
            );
        }
        let opportunities = usize::from(QUERY_COUNT).saturating_mul(K);
        assert!(recalled.saturating_mul(100) >= opportunities.saturating_mul(95));
        Ok(())
    }
}
