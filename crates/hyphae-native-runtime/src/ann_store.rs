// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Bound, ControlFlow},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use hyphae_native_ann::{
    AnnError, AnnRecallRisk, AnnSearchResult, AnnSearchStrategy, GraphNodeRecord,
    HnswBuildProgress, HnswConfig, HnswIndex, HnswPartitionPlan, IndexSnapshot, MAX_HNSW_LEVEL,
    Metric, PartitionedAnnChildSearchResult, PartitionedAnnRoutedSearchResult,
    PartitionedAnnRoutingOutcome, PartitionedAnnSearchPlan, PartitionedHnswIndex,
    PartitionedIndexSnapshot, SearchOptions, Vector, VectorHit, VectorIndexDefinition,
    VectorRecord,
};
use hyphae_native_btree::{
    BTree, BTreeError, BorrowedVisitError, BorrowedVisitLimits, KeyValue, PrefixReplacementBatch,
    PrefixReplacementStructuralLimits, PrefixReplacementStructuralPlan,
};
use hyphae_native_catalog::{
    CatalogObject, IncrementalVectorLifecycle, SearchCollectionDefinition, VectorMetric,
};
use hyphae_native_pages::{BufferPool, PAGE_PAYLOAD_SIZE, PageKind, PageStore, UnpublishedTail};
use hyphae_native_types::{Csn, ObjectId, PageId};

use crate::{
    GovernorCancellation, GovernorQueueError, NativeExecutionError, NativeExecutionPool,
    NativeRuntimeError, OwnedGovernorPermit,
    execution::{TargetedSingleExecutionError, TargetedSingleExecutionRoute},
    model::CatalogState,
    wal_codec::{Mutation, Opcode},
};

pub(crate) struct ExactSearchExecution {
    pub(crate) hits: Vec<VectorHit>,
    pub(crate) planned_vectors: usize,
    pub(crate) planned_batches: usize,
    pub(crate) worker_batches: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnRoutingExecutionMode {
    SelectedPartitions,
    FullFanout,
    FullFanoutBudgetFallback,
    SingleGenerationFallback,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AnnRoutedSearchExecution {
    pub(crate) result: AnnSearchResult,
    pub(crate) base_build_identity: [u8; 32],
    pub(crate) view_identity: [u8; 32],
    pub(crate) exact_delta_candidates: usize,
    pub(crate) selected_partitions: Vec<usize>,
    pub(crate) total_partitions: usize,
    pub(crate) routing_mode: AnnRoutingExecutionMode,
    pub(crate) next_partition_lower_bound: Option<f64>,
    pub(crate) execution_workers: usize,
    pub(crate) execution_worker_batches: usize,
    pub(crate) execution_waves: usize,
    pub(crate) targeted_single_batches: usize,
    pub(crate) generic_single_fallback_batches: usize,
}

pub(crate) const ANN_INDEX_META_PREFIX: u8 = 5;
pub(crate) const ANN_VECTOR_PREFIX: u8 = 6;
pub(crate) const ANN_GRAPH_LAYER_PREFIX: u8 = 7;
pub(crate) const ANN_DELTA_PREFIX: u8 = 8;
const ANN_OVERLAY_MANIFEST_PREFIX: u8 = 9;
const ANN_OVERLAY_DELTA_PREFIX: u8 = 10;
const ANN_OVERLAY_NODE_PREFIX: u8 = 11;

/// Maximum object-keyed mutations retained above one ANN base generation.
pub const MAX_ANN_DELTA_RECORDS: usize = 4_096;
/// Maximum encoded bytes retained by one ANN delta.
pub const MAX_ANN_DELTA_BYTES: usize = 64 * 1024 * 1024;
/// Maximum effective vectors admitted by one bounded consolidation plan.
pub const MAX_ANN_CONSOLIDATION_VECTORS: usize = 1_000_000;

const ANN_INDEX_META_MAGIC_V1: &[u8; 8] = b"HYANNM01";
const ANN_INDEX_META_MAGIC_V2: &[u8; 8] = b"HYANNM02";
const ANN_INDEX_META_MAGIC_V3: &[u8; 8] = b"HYANNM03";
const ANN_INDEX_META_MAGIC_V4: &[u8; 8] = b"HYANNM04";
const ANN_INDEX_META_MAGIC_V5: &[u8; 8] = b"HYANNM05";
const ANN_VECTOR_MAGIC: &[u8; 8] = b"HYANNV01";
const ANN_GRAPH_LAYER_MAGIC: &[u8; 8] = b"HYANNG01";
const ANN_DELTA_MAGIC: &[u8; 8] = b"HYANND01";
const ANN_OVERLAY_MANIFEST_MAGIC: &[u8; 8] = b"HYANNO01";
const ANN_OVERLAY_DELTA_MAGIC: &[u8; 8] = b"HYANND02";
const ANN_OVERLAY_NODE_MAGIC: &[u8; 8] = b"HYANNN01";
const ANN_INDEX_META_V1_SIZE: usize = 80;
const ANN_INDEX_META_V2_SIZE: usize = 144;
const ANN_INDEX_META_V3_SIZE: usize = 160;
const ANN_INDEX_META_V4_HEADER_SIZE: usize = 160;
const ANN_INDEX_META_V5_HEADER_SIZE: usize = 280;
const ANN_INDEX_META_V4_CHILD_SIZE: usize = 72;
const ANN_INDEX_META_V4_RETAINED_HEADER_SIZE: usize = 40;
const ANN_INDEX_META_KEY_SIZE: usize = 17;
const BTREE_LEAF_HEADER_SIZE: usize = 16;
const BTREE_LEAF_ENTRY_HEADER_SIZE: usize = 8;
const ANN_VECTOR_HEADER_SIZE: usize = 24;
const ANN_GRAPH_LAYER_HEADER_SIZE: usize = 16;
const ANN_DELTA_HEADER_SIZE: usize = 40;
const ANN_OVERLAY_MANIFEST_SIZE: usize = 184;
const ANN_OVERLAY_NODE_HEADER_SIZE: usize = 56;
const ANN_GENERATION_KEY_SIZE: usize = 65;
const ANN_GRAPH_LAYER_KEY_SIZE: usize = 67;
const ANN_DELTA_KEY_SIZE: usize = 33;
const ANN_OVERLAY_NODE_KEY_SIZE: usize = 34;
const ANN_OVERLAY_TREE_DEPTH: u8 = 32;
const ANN_OVERLAY_FANOUT: usize = 16;
const ANN_OVERLAY_MAX_NODES: u64 = (MAX_ANN_DELTA_RECORDS as u64) * 32;
const ANN_DELTA_UPSERT: u8 = 1;
const ANN_DELTA_TOMBSTONE: u8 = 2;
const ANN_BASE_SINGLE: u8 = 1;
const ANN_BASE_PARTITIONED: u8 = 2;
const PRIVATE_MUTATION_CSN: u64 = u64::MAX;

static ANN_INDEX_SCOPED_RESTORES_PROCESS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static ANN_BASE_SNAPSHOT_EXPORTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANN_INDEX_SCOPED_RESTORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANN_INDEX_SCOPED_PEAK_PHYSICAL_ENTRIES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static ANN_SEARCH_CANCEL_POINT: std::cell::Cell<Option<AnnSearchCancellationPoint>> =
        const { std::cell::Cell::new(None) };
    static ANN_FULL_STREAM_PHYSICAL_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANN_FULL_STREAM_NODE_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANN_FULL_STREAM_OVERLAY_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANN_FULL_STREAM_PEAK_FRONTIER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnSearchCancellationPoint {
    AfterFirstWave,
    AfterGeometricWidening,
    AfterFallbackWave,
    BeforeDeltaMerge,
}

pub(crate) const DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE: IncrementalVectorLifecycle =
    IncrementalVectorLifecycle {
        delta_max_entries: 4_096,
        consolidate_after_deltas: 1_024,
        retain_generations: 1,
    };

pub(crate) fn maximum_initial_ann_bulk_partitions(retain_generations: u16) -> usize {
    let metadata_value_limit = PAGE_PAYLOAD_SIZE
        .saturating_sub(BTREE_LEAF_HEADER_SIZE)
        .saturating_sub(BTREE_LEAF_ENTRY_HEADER_SIZE)
        .saturating_sub(ANN_INDEX_META_KEY_SIZE);
    let retained_generations = usize::from(retain_generations);
    let fixed_bytes = ANN_INDEX_META_V4_HEADER_SIZE.saturating_add(
        retained_generations.saturating_mul(ANN_INDEX_META_V4_RETAINED_HEADER_SIZE),
    );
    let child_copies = retained_generations.saturating_add(1);
    metadata_value_limit.saturating_sub(fixed_bytes)
        / child_copies.saturating_mul(ANN_INDEX_META_V4_CHILD_SIZE)
}

fn maximum_consolidation_replacement_partitions(
    selected_children: usize,
    retained_children: impl IntoIterator<Item = usize>,
    retain_generations: u16,
    selected_base_is_empty: bool,
) -> usize {
    let mut future_retained = retained_children.into_iter().collect::<Vec<_>>();
    if !selected_base_is_empty {
        future_retained.push(selected_children);
    }
    let retain = usize::from(retain_generations);
    if future_retained.len() > retain {
        future_retained.drain(..future_retained.len() - retain);
    }
    let metadata_value_limit = PAGE_PAYLOAD_SIZE
        .saturating_sub(BTREE_LEAF_HEADER_SIZE)
        .saturating_sub(BTREE_LEAF_ENTRY_HEADER_SIZE)
        .saturating_sub(ANN_INDEX_META_KEY_SIZE);
    let retained_bytes = future_retained
        .into_iter()
        .fold(0_usize, |bytes, children| {
            bytes.saturating_add(
                ANN_INDEX_META_V4_RETAINED_HEADER_SIZE
                    .saturating_add(children.saturating_mul(ANN_INDEX_META_V4_CHILD_SIZE)),
            )
        });
    metadata_value_limit
        .saturating_sub(ANN_INDEX_META_V4_HEADER_SIZE)
        .saturating_sub(retained_bytes)
        / ANN_INDEX_META_V4_CHILD_SIZE
}

fn consolidation_replacement_partitions(
    base_is_partitioned: bool,
    selected_children: usize,
    effective_vectors: usize,
) -> usize {
    if base_is_partitioned && effective_vectors != 0 {
        selected_children.min(effective_vectors)
    } else {
        1
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DeltaRecord {
    Upsert { sequence: u64, record: VectorRecord },
    Tombstone { sequence: u64, mutation_csn: Csn },
}

impl DeltaRecord {
    const fn sequence(&self) -> u64 {
        match self {
            Self::Upsert { sequence, .. } | Self::Tombstone { sequence, .. } => *sequence,
        }
    }

    fn encoded_len(&self) -> usize {
        ANN_DELTA_HEADER_SIZE
            + match self {
                Self::Upsert { record, .. } => record.vector.dimension().saturating_mul(4),
                Self::Tombstone { .. } => 0,
            }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum AnnBase {
    Single(HnswIndex),
    Partitioned(PartitionedHnswIndex),
}

#[derive(Clone, Copy, Debug)]
enum AnnBaseRecordLocator {
    Single(ObjectId),
    Partitioned {
        partition: usize,
        object_id: ObjectId,
    },
}

impl AnnBase {
    fn definition(&self) -> VectorIndexDefinition {
        match self {
            Self::Single(index) => index.definition(),
            Self::Partitioned(index) => index.definition(),
        }
    }

    fn build_identity(&self) -> [u8; 32] {
        match self {
            Self::Single(index) => index.build_identity(),
            Self::Partitioned(index) => index.build_identity(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Single(index) => index.len(),
            Self::Partitioned(index) => index.len(),
        }
    }

    fn is_partitioned(&self) -> bool {
        matches!(self, Self::Partitioned(_))
    }

    fn export_snapshots(&self) -> Vec<IndexSnapshot> {
        #[cfg(test)]
        ANN_BASE_SNAPSHOT_EXPORTS.set(ANN_BASE_SNAPSHOT_EXPORTS.get().saturating_add(1));
        match self {
            Self::Single(index) => vec![index.export_snapshot()],
            Self::Partitioned(index) => index.export_snapshot().partitions,
        }
    }

    fn input_identity(&self) -> Option<[u8; 32]> {
        match self {
            Self::Single(_) => None,
            Self::Partitioned(index) => Some(index.input_identity()),
        }
    }

    fn vector_records(&self) -> Vec<VectorRecord> {
        let mut records = Vec::with_capacity(self.len());
        let result = self.try_for_each_vector_record(|object_id, creating_csn, vector| {
            records.push(VectorRecord {
                object_id,
                creating_csn,
                vector: vector.clone(),
            });
            Ok::<_, std::convert::Infallible>(())
        });
        if let Err(never) = result {
            match never {}
        }
        records
    }

    fn vector_record(&self, object_id: ObjectId) -> Option<(Csn, &Vector)> {
        match self {
            Self::Single(index) => index.vector_record(object_id),
            Self::Partitioned(index) => index
                .vector_record(object_id)
                .map(|(_, creating_csn, vector)| (creating_csn, vector)),
        }
    }

    fn try_for_each_vector_record<E>(
        &self,
        mut visitor: impl FnMut(ObjectId, Csn, &Vector) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Single(index) => index.try_for_each_vector_record(visitor),
            Self::Partitioned(index) => {
                index.try_for_each_vector_record(|_, object_id, creating_csn, vector| {
                    visitor(object_id, creating_csn, vector)
                })
            }
        }
    }

    fn try_for_each_record_locator<E>(
        &self,
        mut visitor: impl FnMut(AnnBaseRecordLocator, ObjectId) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Single(index) => index.try_for_each_vector_record(|object_id, _, _| {
                visitor(AnnBaseRecordLocator::Single(object_id), object_id)
            }),
            Self::Partitioned(index) => {
                index.try_for_each_vector_record(|partition, object_id, _, _| {
                    visitor(
                        AnnBaseRecordLocator::Partitioned {
                            partition,
                            object_id,
                        },
                        object_id,
                    )
                })
            }
        }
    }

    fn vector_at(&self, locator: AnnBaseRecordLocator) -> Option<(ObjectId, &Vector)> {
        match (self, locator) {
            (Self::Single(index), AnnBaseRecordLocator::Single(object_id)) => index
                .vector_record(object_id)
                .map(|(_, vector)| (object_id, vector)),
            (
                Self::Partitioned(index),
                AnnBaseRecordLocator::Partitioned {
                    partition,
                    object_id,
                },
            ) => index
                .partition_vector_record(partition, object_id)
                .map(|(_, vector)| (object_id, vector)),
            _ => None,
        }
    }

    fn search(
        &self,
        query: &Vector,
        options: SearchOptions,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        match self {
            Self::Single(index) => Ok(index.search(query, options)?),
            Self::Partitioned(index) => Ok(index.search(query, options)?),
        }
    }

    fn search_routed(
        &self,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
    ) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
        if maximum_partitions == 0 {
            return Err(AnnError::InvalidPartitionCount.into());
        }
        match self {
            Self::Single(index) => Ok(AnnRoutedSearchExecution {
                result: index.search(query, options)?,
                base_build_identity: index.build_identity(),
                view_identity: index.build_identity(),
                exact_delta_candidates: 0,
                selected_partitions: vec![0],
                total_partitions: 1,
                routing_mode: AnnRoutingExecutionMode::SingleGenerationFallback,
                next_partition_lower_bound: None,
                execution_workers: 1,
                execution_worker_batches: 1,
                execution_waves: 1,
                targeted_single_batches: 0,
                generic_single_fallback_batches: 0,
            }),
            Self::Partitioned(index) => {
                let selected = index.search_routed(query, options, maximum_partitions)?;
                let routing_mode = match selected.outcome {
                    PartitionedAnnRoutingOutcome::SelectedCertified => {
                        AnnRoutingExecutionMode::SelectedPartitions
                    }
                    PartitionedAnnRoutingOutcome::FullFanoutRequested => {
                        AnnRoutingExecutionMode::FullFanout
                    }
                    PartitionedAnnRoutingOutcome::FullFanoutBudgetFallback => {
                        AnnRoutingExecutionMode::FullFanoutBudgetFallback
                    }
                };
                Ok(AnnRoutedSearchExecution {
                    result: selected.result,
                    base_build_identity: index.build_identity(),
                    view_identity: index.build_identity(),
                    exact_delta_candidates: 0,
                    selected_partitions: selected.selected_partitions,
                    total_partitions: selected.total_partitions,
                    routing_mode,
                    next_partition_lower_bound: selected.next_partition_lower_bound,
                    execution_workers: 1,
                    execution_worker_batches: 1,
                    execution_waves: 1,
                    targeted_single_batches: 0,
                    generic_single_fallback_batches: 0,
                })
            }
        }
    }

    fn retention_descriptor(&self) -> RetainedGeneration {
        RetainedGeneration {
            build_identity: self.build_identity(),
            children: self.child_descriptors(),
        }
    }

    fn child_descriptors(&self) -> Vec<PersistedChildDescriptor> {
        match self {
            Self::Single(index) => vec![PersistedChildDescriptor::from_generation(
                index.generation_descriptor(),
            )],
            Self::Partitioned(index) => index
                .generation_descriptors()
                .into_iter()
                .map(PersistedChildDescriptor::from_generation)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetainedGeneration {
    build_identity: [u8; 32],
    children: Vec<PersistedChildDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
struct AnnIndexState {
    base: AnnBase,
    deltas: BTreeMap<ObjectId, DeltaRecord>,
    next_sequence: u64,
    view_identity: [u8; 32],
    lifecycle: IncrementalVectorLifecycle,
    retained_generations: Vec<RetainedGeneration>,
    persisted_version: u8,
}

#[derive(Clone, Copy, Debug)]
enum ExactRecordLocator {
    Base(AnnBaseRecordLocator),
    Delta(ObjectId),
}

impl AnnIndexState {
    fn new(base: HnswIndex, lifecycle: IncrementalVectorLifecycle) -> Self {
        let mut state = Self {
            base: AnnBase::Single(base),
            deltas: BTreeMap::new(),
            next_sequence: 1,
            view_identity: [0; 32],
            lifecycle,
            retained_generations: Vec::new(),
            persisted_version: 4,
        };
        state.refresh_view_identity();
        state
    }

    fn definition(&self) -> VectorIndexDefinition {
        self.base.definition()
    }

    fn delta_bytes(&self) -> usize {
        self.deltas.values().map(DeltaRecord::encoded_len).sum()
    }

    fn allocate_sequence(&mut self) -> Result<u64, NativeRuntimeError> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or(NativeRuntimeError::AnnDeltaLimitExceeded)?;
        Ok(sequence)
    }

    fn contains_effective_record(&self, object_id: ObjectId) -> bool {
        match self.deltas.get(&object_id) {
            Some(DeltaRecord::Upsert { .. }) => true,
            Some(DeltaRecord::Tombstone { .. }) => false,
            None => self.base.vector_record(object_id).is_some(),
        }
    }

    fn effective_vector_count(&self) -> usize {
        self.deltas
            .iter()
            .fold(self.base.len(), |count, (object_id, delta)| match delta {
                DeltaRecord::Upsert { .. } if self.base.vector_record(*object_id).is_none() => {
                    count.saturating_add(1)
                }
                DeltaRecord::Tombstone { .. } if self.base.vector_record(*object_id).is_some() => {
                    count.saturating_sub(1)
                }
                _ => count,
            })
    }

    fn effective_vectors(&self) -> Vec<VectorRecord> {
        let mut vectors = self
            .base
            .vector_records()
            .into_iter()
            .map(|record| (record.object_id, record))
            .collect::<BTreeMap<_, _>>();
        for (object_id, delta) in &self.deltas {
            match delta {
                DeltaRecord::Upsert { record, .. } => {
                    vectors.insert(*object_id, record.clone());
                }
                DeltaRecord::Tombstone { .. } => {
                    vectors.remove(object_id);
                }
            }
        }
        vectors.into_values().collect()
    }

    fn effective_vectors_with_cancellation(
        &self,
        cancellation: Option<&GovernorCancellation>,
    ) -> Result<Vec<VectorRecord>, NativeRuntimeError> {
        self.effective_vectors_with_control(|| reject_cancelled_ann_search(cancellation))
    }

    fn effective_vectors_with_control(
        &self,
        mut check: impl FnMut() -> Result<(), NativeRuntimeError>,
    ) -> Result<Vec<VectorRecord>, NativeRuntimeError> {
        check()?;
        let mut vectors = BTreeMap::new();
        self.base.try_for_each_vector_record(
            |object_id, creating_csn, vector| -> Result<(), NativeRuntimeError> {
                check()?;
                #[cfg(test)]
                ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS.set(
                    ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS
                        .get()
                        .saturating_add(1),
                );
                vectors.insert(
                    object_id,
                    VectorRecord {
                        object_id,
                        creating_csn,
                        vector: vector.clone(),
                    },
                );
                Ok(())
            },
        )?;
        for (object_id, delta) in &self.deltas {
            check()?;
            #[cfg(test)]
            ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS.set(
                ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS
                    .get()
                    .saturating_add(1),
            );
            match delta {
                DeltaRecord::Upsert { record, .. } => {
                    vectors.insert(*object_id, record.clone());
                }
                DeltaRecord::Tombstone { .. } => {
                    vectors.remove(object_id);
                }
            }
        }
        check()?;
        Ok(vectors.into_values().collect())
    }

    fn upsert(
        &mut self,
        object_id: ObjectId,
        creating_csn: Csn,
        vector: Vector,
    ) -> Result<(), NativeRuntimeError> {
        if self.persisted_version == 5 {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        validate_vector(self.definition(), &vector)?;
        let sequence = self.allocate_sequence()?;
        let previous = self.deltas.insert(
            object_id,
            DeltaRecord::Upsert {
                sequence,
                record: VectorRecord {
                    object_id,
                    creating_csn,
                    vector,
                },
            },
        );
        if let Err(error) = self.validate_delta_bounds() {
            if let Some(previous) = previous {
                self.deltas.insert(object_id, previous);
            } else {
                self.deltas.remove(&object_id);
            }
            self.next_sequence = sequence;
            return Err(error);
        }
        self.refresh_view_identity();
        Ok(())
    }

    fn delete(
        &mut self,
        object_id: ObjectId,
        mutation_csn: Csn,
    ) -> Result<bool, NativeRuntimeError> {
        if self.persisted_version == 5 {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        if !self.contains_effective_record(object_id) {
            return Ok(false);
        }
        let sequence = self.allocate_sequence()?;
        let previous = self.deltas.insert(
            object_id,
            DeltaRecord::Tombstone {
                sequence,
                mutation_csn,
            },
        );
        if let Err(error) = self.validate_delta_bounds() {
            if let Some(previous) = previous {
                self.deltas.insert(object_id, previous);
            } else {
                self.deltas.remove(&object_id);
            }
            self.next_sequence = sequence;
            return Err(error);
        }
        self.refresh_view_identity();
        Ok(true)
    }

    fn validate_delta_bounds(&self) -> Result<(), NativeRuntimeError> {
        if self.deltas.len()
            > usize::try_from(self.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
            || self.delta_bytes() > MAX_ANN_DELTA_BYTES
        {
            Err(NativeRuntimeError::AnnDeltaLimitExceeded)
        } else {
            Ok(())
        }
    }

    fn refresh_view_identity(&mut self) {
        self.view_identity =
            calculate_view_identity(self.base.build_identity(), self.next_sequence, &self.deltas);
    }

    fn search_exact(
        &self,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        self.search_exact_borrowed(query, k, allowlist)
            .map(|(hits, _)| hits)
    }

    fn search_exact_profiled(
        &self,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<ExactSearchExecution, NativeRuntimeError> {
        let (hits, planned_vectors) = self.search_exact_borrowed(query, k, allowlist)?;
        Ok(ExactSearchExecution {
            hits,
            planned_vectors,
            planned_batches: usize::from(planned_vectors > 0 && k > 0),
            worker_batches: 0,
        })
    }

    fn search_exact_borrowed(
        &self,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<(Vec<VectorHit>, usize), NativeRuntimeError> {
        validate_vector(self.definition(), query)?;
        let mut planned_vectors = 0_usize;
        let mut hits = Vec::new();
        self.base.try_for_each_vector_record(
            |object_id, _, vector| -> Result<(), NativeRuntimeError> {
                if self.deltas.contains_key(&object_id)
                    || allowlist.is_some_and(|ids| !ids.contains(&object_id))
                {
                    return Ok(());
                }
                planned_vectors = planned_vectors
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                if k != 0 {
                    hits.push(VectorHit {
                        object_id,
                        distance: distance(self.definition().metric(), query, vector)?,
                    });
                }
                Ok(())
            },
        )?;
        for (object_id, delta) in &self.deltas {
            let DeltaRecord::Upsert { record, .. } = delta else {
                continue;
            };
            if allowlist.is_some_and(|ids| !ids.contains(object_id)) {
                continue;
            }
            planned_vectors = planned_vectors
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            if k != 0 {
                hits.push(VectorHit {
                    object_id: *object_id,
                    distance: distance(self.definition().metric(), query, &record.vector)?,
                });
            }
        }
        sort_hits(&mut hits);
        hits.truncate(k);
        Ok((hits, planned_vectors))
    }

    fn search_exact_parallel(
        self,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
        execution_pool: &NativeExecutionPool,
        permit: &OwnedGovernorPermit,
    ) -> Result<ExactSearchExecution, NativeRuntimeError> {
        validate_vector(self.definition(), query)?;
        if k == 0 {
            return Ok(ExactSearchExecution {
                hits: Vec::new(),
                planned_vectors: 0,
                planned_batches: 0,
                worker_batches: 0,
            });
        }
        let mut locators = Vec::with_capacity(self.effective_vector_count());
        self.base.try_for_each_record_locator(
            |locator, object_id| -> Result<(), NativeRuntimeError> {
                if !self.deltas.contains_key(&object_id)
                    && allowlist.is_none_or(|ids| ids.contains(&object_id))
                {
                    locators.push(ExactRecordLocator::Base(locator));
                }
                Ok(())
            },
        )?;
        locators.extend(self.deltas.iter().filter_map(|(object_id, delta)| {
            matches!(delta, DeltaRecord::Upsert { .. })
                .then_some(*object_id)
                .filter(|object_id| allowlist.is_none_or(|ids| ids.contains(object_id)))
                .map(ExactRecordLocator::Delta)
        }));
        if locators.is_empty() {
            return Ok(ExactSearchExecution {
                hits: Vec::new(),
                planned_vectors: 0,
                planned_batches: 0,
                worker_batches: 0,
            });
        }
        let planned_vectors = locators.len();
        let batch_count = usize::try_from(permit.request().compute_threads)
            .unwrap_or(usize::MAX)
            .min(planned_vectors);
        let mut batches = std::iter::repeat_with(Vec::new)
            .take(batch_count)
            .collect::<Vec<_>>();
        for (position, locator) in locators.into_iter().enumerate() {
            batches[position % batch_count].push(locator);
        }
        let planned_batches = batches.len();
        let metric = self.definition().metric();
        let query = query.clone();
        let state = std::sync::Arc::new(self);
        let (batch_results, worker_batches) =
            execution_pool.execute_ordered_profiled(permit, batches, move |locators| {
                let mut hits = locators
                    .into_iter()
                    .map(|locator| {
                        let (object_id, vector) = state
                            .vector_at(locator)
                            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                        Ok(VectorHit {
                            object_id,
                            distance: distance(metric, &query, vector)?,
                        })
                    })
                    .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
                sort_hits(&mut hits);
                hits.truncate(k);
                Ok(hits)
            })?;
        let mut hits = batch_results
            .into_iter()
            .collect::<Result<Vec<_>, NativeRuntimeError>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        sort_hits(&mut hits);
        hits.truncate(k);
        Ok(ExactSearchExecution {
            hits,
            planned_vectors,
            planned_batches,
            worker_batches,
        })
    }

    fn vector_at(&self, locator: ExactRecordLocator) -> Option<(ObjectId, &Vector)> {
        match locator {
            ExactRecordLocator::Base(locator) => self.base.vector_at(locator),
            ExactRecordLocator::Delta(object_id) => match self.deltas.get(&object_id) {
                Some(DeltaRecord::Upsert { record, .. }) => Some((object_id, &record.vector)),
                _ => None,
            },
        }
    }

    fn search(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        if let Some(allowlist) = allowlist {
            let eligible_count = self.search_exact_borrowed(query, 0, Some(allowlist))?.1;
            if eligible_count <= options.ef_search() || self.base.is_partitioned() {
                return self.exact_filtered_search_result(
                    query,
                    options,
                    allowlist,
                    eligible_count,
                );
            }
        }
        let base_result = match (&self.base, allowlist) {
            (AnnBase::Single(base), Some(allowlist)) => {
                base.search_filtered(query, options, allowlist)?
            }
            (_, None) => self.base.search(query, options)?,
            (AnnBase::Partitioned(_), Some(allowlist)) => {
                let eligible_count = self.search_exact_borrowed(query, 0, Some(allowlist))?.1;
                return self.exact_filtered_search_result(
                    query,
                    options,
                    allowlist,
                    eligible_count,
                );
            }
        };
        self.merge_delta_search_result(query, options, allowlist, base_result)
    }

    fn search_selected(
        &self,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
    ) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
        let mut execution = self
            .base
            .search_routed(query, options, maximum_partitions)?;
        let exact_delta_candidates = self
            .deltas
            .values()
            .filter(|delta| matches!(delta, DeltaRecord::Upsert { .. }))
            .count();
        execution.result =
            self.merge_delta_search_result(query, options, None, execution.result)?;
        execution.base_build_identity = self.base.build_identity();
        execution.view_identity = self.view_identity;
        execution.exact_delta_candidates = exact_delta_candidates;
        Ok(execution)
    }

    fn search_selected_parallel(
        self: Arc<Self>,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
        execution: AnnParallelSearchExecution<'_>,
    ) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
        reject_cancelled_ann_search(execution.cancellation)?;
        let routing_plan = match &self.base {
            AnnBase::Single(_) => {
                return self.search_selected(query, options, maximum_partitions);
            }
            AnnBase::Partitioned(index) => {
                index.plan_routed_search(query, options, maximum_partitions)?
            }
        };
        let state = self;
        let routing_plan = Arc::new(routing_plan);
        let routed =
            execute_adaptive_routed_base(&state, &routing_plan, query, options, execution)?;
        let exact_delta_candidates = state
            .deltas
            .values()
            .filter(|delta| matches!(delta, DeltaRecord::Upsert { .. }))
            .count();
        let PartitionedAnnRoutedSearchResult {
            result,
            selected_partitions,
            total_partitions,
            outcome,
            next_partition_lower_bound,
        } = routed.result;
        let routing_mode = routing_execution_mode(outcome);
        Ok(AnnRoutedSearchExecution {
            result,
            base_build_identity: state.base.build_identity(),
            view_identity: state.view_identity,
            exact_delta_candidates,
            selected_partitions,
            total_partitions,
            routing_mode,
            next_partition_lower_bound,
            execution_workers: routed.workers,
            execution_worker_batches: routed.worker_batches,
            execution_waves: routed.waves,
            targeted_single_batches: routed.targeted_single_batches,
            generic_single_fallback_batches: routed.generic_single_fallback_batches,
        })
    }

    fn merge_delta_search_result(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: Option<&BTreeSet<ObjectId>>,
        base_result: AnnSearchResult,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        self.merge_delta_search_result_controlled(query, options, allowlist, base_result, None)
    }
    fn merge_delta_search_result_controlled(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: Option<&BTreeSet<ObjectId>>,
        base_result: AnnSearchResult,
        cancellation: Option<&GovernorCancellation>,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        reject_cancelled_ann_search(cancellation)?;
        let overridden = self.deltas.keys().copied().collect::<BTreeSet<_>>();
        let mut hits = base_result
            .hits
            .into_iter()
            .filter(|hit| !overridden.contains(&hit.object_id))
            .collect::<Vec<_>>();
        let mut exact_delta_candidates = 0_usize;
        for (object_id, delta) in &self.deltas {
            reject_cancelled_ann_search(cancellation)?;
            let DeltaRecord::Upsert { record, .. } = delta else {
                continue;
            };
            if allowlist.is_some_and(|ids| !ids.contains(object_id)) {
                continue;
            }
            exact_delta_candidates = exact_delta_candidates
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            hits.push(VectorHit {
                object_id: *object_id,
                distance: distance(self.definition().metric(), query, &record.vector)?,
            });
        }
        reject_cancelled_ann_search(cancellation)?;
        sort_hits(&mut hits);
        hits.truncate(options.k());
        Ok(AnnSearchResult {
            approximate: base_result.approximate,
            build_identity: self.view_identity,
            metric: self.definition().metric(),
            ef_search: base_result.ef_search,
            candidate_count: base_result
                .candidate_count
                .saturating_add(exact_delta_candidates),
            eligible_candidate_count: base_result
                .eligible_candidate_count
                .saturating_add(exact_delta_candidates),
            strategy: base_result.strategy,
            recall_risk: base_result.recall_risk,
            exact_reranked: base_result.exact_reranked,
            visited_nodes: base_result.visited_nodes,
            hits,
        })
    }

    fn exact_filtered_search_result(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: &BTreeSet<ObjectId>,
        eligible_count: usize,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        let hits = self.search_exact(query, options.k(), Some(allowlist))?;
        Ok(AnnSearchResult {
            approximate: false,
            build_identity: self.view_identity,
            metric: self.definition().metric(),
            ef_search: options.ef_search(),
            candidate_count: eligible_count,
            eligible_candidate_count: eligible_count,
            strategy: AnnSearchStrategy::StableIdAdaptiveExact,
            recall_risk: AnnRecallRisk::ExactFilteredCandidates,
            exact_reranked: true,
            visited_nodes: eligible_count,
            hits,
        })
    }
}

fn routing_execution_mode(outcome: PartitionedAnnRoutingOutcome) -> AnnRoutingExecutionMode {
    match outcome {
        PartitionedAnnRoutingOutcome::SelectedCertified => {
            AnnRoutingExecutionMode::SelectedPartitions
        }
        PartitionedAnnRoutingOutcome::FullFanoutRequested => AnnRoutingExecutionMode::FullFanout,
        PartitionedAnnRoutingOutcome::FullFanoutBudgetFallback => {
            AnnRoutingExecutionMode::FullFanoutBudgetFallback
        }
    }
}

struct AdaptiveRoutedBaseExecution {
    result: PartitionedAnnRoutedSearchResult,
    workers: usize,
    worker_batches: usize,
    waves: usize,
    targeted_single_batches: usize,
    generic_single_fallback_batches: usize,
}

#[derive(Default)]
struct AdaptiveRoutingStats {
    workers: usize,
    worker_batches: usize,
    waves: usize,
    targeted_single_batches: usize,
    generic_single_fallback_batches: usize,
}

impl AdaptiveRoutingStats {
    fn record_wave(&mut self, wave: &RoutedWaveExecution) -> Result<(), NativeRuntimeError> {
        self.worker_batches = self
            .worker_batches
            .checked_add(wave.worker_batches)
            .ok_or(NativeExecutionError::Synchronization)?;
        self.workers = self.workers.max(wave.worker_batches);
        self.waves = self
            .waves
            .checked_add(1)
            .ok_or(NativeExecutionError::Synchronization)?;
        self.targeted_single_batches = self
            .targeted_single_batches
            .checked_add(wave.targeted_single_batches)
            .ok_or(NativeExecutionError::Synchronization)?;
        self.generic_single_fallback_batches = self
            .generic_single_fallback_batches
            .checked_add(wave.generic_single_fallback_batches)
            .ok_or(NativeExecutionError::Synchronization)?;
        Ok(())
    }

    fn validate(&self) -> Result<(), NativeRuntimeError> {
        validate_single_route_counts(
            self.targeted_single_batches,
            self.generic_single_fallback_batches,
            self.worker_batches,
            self.waves,
        )
    }
}

fn execute_adaptive_routed_base(
    state: &Arc<AnnIndexState>,
    plan: &Arc<PartitionedAnnSearchPlan>,
    query: &Vector,
    options: SearchOptions,
    execution: AnnParallelSearchExecution<'_>,
) -> Result<AdaptiveRoutedBaseExecution, NativeRuntimeError> {
    let mut children = Vec::new();
    let mut routing_counts = AdaptiveRoutingStats::default();
    let mut routed = None;
    for prefix in plan.geometric_prefixes() {
        reject_cancelled_ann_search(execution.cancellation)?;
        let wave = execute_routed_wave(
            state,
            plan,
            children.len()..prefix,
            execution.pool,
            execution.permit,
            execution.cancellation,
        )?;
        routing_counts.record_wave(&wave)?;
        children.extend(wave.children);
        let cancellation_point = if routing_counts.waves == 1 {
            AnnSearchCancellationPoint::AfterFirstWave
        } else {
            AnnSearchCancellationPoint::AfterGeometricWidening
        };
        cancel_ann_search_at_test_point(cancellation_point, execution.cancellation);
        reject_cancelled_ann_search(execution.cancellation)?;
        match partitioned_base(state)?.merge_routed_search(plan, &children) {
            Ok(result) => {
                let result = merge_routed_candidate_with_deltas(
                    state,
                    query,
                    options,
                    result,
                    execution.cancellation,
                )?;
                if selected_certificate_survives_deltas(&result, options) {
                    routed = Some(result);
                    break;
                }
            }
            Err(AnnError::RoutingBudgetInsufficient) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let result = if let Some(result) = routed {
        result
    } else {
        reject_cancelled_ann_search(execution.cancellation)?;
        let fallback = execute_routed_wave(
            state,
            plan,
            children.len()..plan.total_partitions(),
            execution.pool,
            execution.permit,
            execution.cancellation,
        )?;
        cancel_ann_search_at_test_point(
            AnnSearchCancellationPoint::AfterFallbackWave,
            execution.cancellation,
        );
        reject_cancelled_ann_search(execution.cancellation)?;
        routing_counts.record_wave(&fallback)?;
        children.extend(fallback.children);
        let result = partitioned_base(state)?.merge_routed_search(plan, &children)?;
        merge_routed_candidate_with_deltas(state, query, options, result, execution.cancellation)?
    };
    routing_counts.validate()?;
    Ok(AdaptiveRoutedBaseExecution {
        result,
        workers: routing_counts.workers,
        worker_batches: routing_counts.worker_batches,
        waves: routing_counts.waves,
        targeted_single_batches: routing_counts.targeted_single_batches,
        generic_single_fallback_batches: routing_counts.generic_single_fallback_batches,
    })
}

fn validate_single_route_counts(
    targeted_single_batches: usize,
    generic_single_fallback_batches: usize,
    worker_batches: usize,
    waves: usize,
) -> Result<(), NativeRuntimeError> {
    let single_route_batches = targeted_single_batches
        .checked_add(generic_single_fallback_batches)
        .ok_or(NativeExecutionError::Synchronization)?;
    if single_route_batches > worker_batches || single_route_batches > waves {
        return Err(NativeExecutionError::Synchronization.into());
    }
    Ok(())
}

struct RoutedWaveExecution {
    children: Vec<PartitionedAnnChildSearchResult>,
    worker_batches: usize,
    targeted_single_batches: usize,
    generic_single_fallback_batches: usize,
}

fn merge_routed_candidate_with_deltas(
    state: &AnnIndexState,
    query: &Vector,
    options: SearchOptions,
    mut routed: PartitionedAnnRoutedSearchResult,
    cancellation: Option<&GovernorCancellation>,
) -> Result<PartitionedAnnRoutedSearchResult, NativeRuntimeError> {
    cancel_ann_search_at_test_point(AnnSearchCancellationPoint::BeforeDeltaMerge, cancellation);
    reject_cancelled_ann_search(cancellation)?;
    routed.result = state.merge_delta_search_result_controlled(
        query,
        options,
        None,
        routed.result,
        cancellation,
    )?;
    Ok(routed)
}

fn selected_certificate_survives_deltas(
    routed: &PartitionedAnnRoutedSearchResult,
    options: SearchOptions,
) -> bool {
    routed.outcome != PartitionedAnnRoutingOutcome::SelectedCertified
        || (routed.result.hits.len() == options.k()
            && routed.next_partition_lower_bound.is_some_and(|bound| {
                bound.total_cmp(&routed.result.hits[options.k() - 1].distance)
                    == std::cmp::Ordering::Greater
            }))
}

fn partitioned_base(state: &AnnIndexState) -> Result<&PartitionedHnswIndex, NativeRuntimeError> {
    match &state.base {
        AnnBase::Partitioned(index) => Ok(index),
        AnnBase::Single(_) => Err(NativeRuntimeError::InvalidAnnTree),
    }
}

fn execute_routed_wave(
    state: &Arc<AnnIndexState>,
    plan: &Arc<PartitionedAnnSearchPlan>,
    positions: std::ops::Range<usize>,
    execution_pool: &NativeExecutionPool,
    permit: &OwnedGovernorPermit,
    cancellation: Option<&GovernorCancellation>,
) -> Result<RoutedWaveExecution, NativeRuntimeError> {
    reject_cancelled_ann_search(cancellation)?;
    let work = positions.collect::<Vec<_>>();
    if work.is_empty() {
        return Err(AnnError::InvalidPartitionCount.into());
    }
    if work.len() == 1 {
        let position = work[0];
        let stable_hint = plan
            .ranked_partitions()
            .nth(position)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let state = Arc::clone(state);
        let plan = Arc::clone(plan);
        let waiter_cancellation = cancellation.cloned();
        let operation_cancellation = waiter_cancellation.clone();
        let (child, receipt) = execution_pool
            .execute_single_targeted_profiled(
                permit,
                stable_hint,
                waiter_cancellation.as_ref(),
                move || {
                    reject_cancelled_ann_search(operation_cancellation.as_ref())?;
                    Ok::<_, NativeRuntimeError>(
                        partitioned_base(&state)?.search_planned_partition(&plan, position)?,
                    )
                },
            )
            .map_err(map_targeted_ann_execution_error)?;
        let child = child?;
        let (targeted_single_batches, generic_single_fallback_batches) = match receipt.route {
            TargetedSingleExecutionRoute::Targeted => (1, 0),
            TargetedSingleExecutionRoute::GenericFallbackBusy => (0, 1),
        };
        return Ok(RoutedWaveExecution {
            children: vec![child],
            worker_batches: 1,
            targeted_single_batches,
            generic_single_fallback_batches,
        });
    }
    let state = Arc::clone(state);
    let plan = Arc::clone(plan);
    let cancellation = cancellation.cloned();
    let (children, worker_batches) = execution_pool.execute_ordered_profiled(
        permit,
        work,
        move |position| -> Result<PartitionedAnnChildSearchResult, NativeRuntimeError> {
            reject_cancelled_ann_search(cancellation.as_ref())?;
            Ok(partitioned_base(&state)?.search_planned_partition(&plan, position)?)
        },
    )?;
    Ok(RoutedWaveExecution {
        children: children
            .into_iter()
            .collect::<Result<Vec<_>, NativeRuntimeError>>()?,
        worker_batches,
        targeted_single_batches: 0,
        generic_single_fallback_batches: 0,
    })
}

fn map_targeted_ann_execution_error(error: TargetedSingleExecutionError) -> NativeRuntimeError {
    match error {
        TargetedSingleExecutionError::Execution(error) => error.into(),
        TargetedSingleExecutionError::Cancelled => GovernorQueueError::Cancelled.into(),
        TargetedSingleExecutionError::ForeignCancellation => {
            GovernorQueueError::ForeignCancellation.into()
        }
        TargetedSingleExecutionError::Closed
        | TargetedSingleExecutionError::GenerationExhausted => {
            NativeExecutionError::Synchronization.into()
        }
    }
}

fn reject_cancelled_ann_search(
    cancellation: Option<&GovernorCancellation>,
) -> Result<(), NativeRuntimeError> {
    if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
        Err(GovernorQueueError::Cancelled.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn cancel_ann_search_at_test_point(
    point: AnnSearchCancellationPoint,
    cancellation: Option<&GovernorCancellation>,
) {
    if ANN_SEARCH_CANCEL_POINT.get() == Some(point) {
        ANN_SEARCH_CANCEL_POINT.set(None);
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
        }
    }
}

#[cfg(not(test))]
fn cancel_ann_search_at_test_point(
    _point: AnnSearchCancellationPoint,
    _cancellation: Option<&GovernorCancellation>,
) {
}

#[cfg(test)]
pub(crate) fn cancel_next_search_at_for_test(point: AnnSearchCancellationPoint) {
    ANN_SEARCH_CANCEL_POINT.set(Some(point));
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AnnState {
    indexes: BTreeMap<ObjectId, AnnIndexState>,
}

impl AnnState {
    pub(crate) fn vector_records(
        &self,
        index: ObjectId,
    ) -> Result<Vec<VectorRecord>, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })
            .map(AnnIndexState::effective_vectors)
    }

    pub(crate) fn create(
        &mut self,
        definition: VectorIndexDefinition,
        lifecycle: IncrementalVectorLifecycle,
    ) -> Result<(), NativeRuntimeError> {
        if self
            .indexes
            .insert(
                definition.index_id(),
                AnnIndexState::new(HnswIndex::new(definition)?, lifecycle),
            )
            .is_some()
        {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        Ok(())
    }

    pub(crate) fn upsert(
        &mut self,
        index: ObjectId,
        object_id: ObjectId,
        creating_csn: Csn,
        vector: Vector,
    ) -> Result<(), NativeRuntimeError> {
        self.indexes
            .get_mut(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .upsert(object_id, creating_csn, vector)
    }

    pub(crate) fn upsert_many(
        &mut self,
        index: ObjectId,
        creating_csn: Csn,
        vectors: &[(ObjectId, Vector)],
    ) -> Result<(), NativeRuntimeError> {
        let mut identities = BTreeSet::new();
        if vectors
            .iter()
            .any(|(object_id, _)| !identities.insert(*object_id))
        {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let current = self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        if current.persisted_version == 5 {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let mut prospective = current.deltas.clone();
        let mut sequence = current.next_sequence;
        for (object_id, vector) in vectors {
            validate_vector(current.definition(), vector)?;
            prospective.insert(
                *object_id,
                DeltaRecord::Upsert {
                    sequence,
                    record: VectorRecord {
                        object_id: *object_id,
                        creating_csn,
                        vector: vector.clone(),
                    },
                },
            );
            sequence = sequence
                .checked_add(1)
                .ok_or(NativeRuntimeError::AnnDeltaLimitExceeded)?;
        }
        let prospective_bytes = prospective
            .values()
            .map(DeltaRecord::encoded_len)
            .sum::<usize>();
        if prospective.len()
            > usize::try_from(current.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
            || prospective_bytes > MAX_ANN_DELTA_BYTES
        {
            return Err(NativeRuntimeError::AnnDeltaLimitExceeded);
        }
        let mut replacement = self
            .indexes
            .get(&index)
            .cloned()
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        for (object_id, vector) in vectors {
            replacement.upsert(*object_id, creating_csn, vector.clone())?;
        }
        self.indexes.insert(index, replacement);
        Ok(())
    }

    pub(crate) fn upsert_initial_many(
        &mut self,
        index: ObjectId,
        creating_csn: Csn,
        vectors: &[(ObjectId, Vector)],
    ) -> Result<(), NativeRuntimeError> {
        self.upsert_initial_many_with_progress(index, creating_csn, vectors, |_| {})
    }

    pub(crate) fn upsert_initial_many_with_progress(
        &mut self,
        index: ObjectId,
        creating_csn: Csn,
        vectors: &[(ObjectId, Vector)],
        progress: impl FnMut(HnswBuildProgress),
    ) -> Result<(), NativeRuntimeError> {
        let current = self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        if current.persisted_version == 5
            || !current.deltas.is_empty()
            || !current.retained_generations.is_empty()
        {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let mut records = current
            .base
            .vector_records()
            .into_iter()
            .map(|record| (record.object_id, record))
            .collect::<BTreeMap<_, _>>();
        let mut identities = BTreeSet::new();
        for (object_id, vector) in vectors {
            if !identities.insert(*object_id) {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            validate_vector(current.definition(), vector)?;
            records.insert(
                *object_id,
                VectorRecord {
                    object_id: *object_id,
                    creating_csn,
                    vector: vector.clone(),
                },
            );
        }
        let replacement =
            HnswIndex::build_with_progress(current.definition(), records.into_values(), progress)?;
        let current = self
            .indexes
            .get_mut(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        current.base = AnnBase::Single(replacement);
        current.next_sequence = 1;
        current.refresh_view_identity();
        Ok(())
    }

    pub(crate) fn delete(
        &mut self,
        index: ObjectId,
        object_id: ObjectId,
    ) -> Result<bool, NativeRuntimeError> {
        self.indexes
            .get_mut(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .delete(object_id, private_mutation_csn()?)
    }

    pub(crate) fn search(
        &self,
        index: ObjectId,
        query: &Vector,
        options: SearchOptions,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search(query, options, None)
    }

    pub(crate) fn search_selected(
        &self,
        index: ObjectId,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
    ) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_selected(query, options, maximum_partitions)
    }

    pub(crate) fn search_exact(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact(query, k, None)
    }

    pub(crate) fn search_filtered(
        &self,
        index: ObjectId,
        query: &Vector,
        options: SearchOptions,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search(query, options, Some(allowlist))
    }

    pub(crate) fn search_exact_filtered(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact(query, k, Some(allowlist))
    }

    pub(crate) fn search_exact_parallel(
        mut self,
        index: ObjectId,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
        execution_pool: &NativeExecutionPool,
        permit: &OwnedGovernorPermit,
    ) -> Result<ExactSearchExecution, NativeRuntimeError> {
        self.indexes
            .remove(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact_parallel(query, k, allowlist, execution_pool, permit)
    }

    pub(crate) fn search_exact_profiled(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<ExactSearchExecution, NativeRuntimeError> {
        self.indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact_profiled(query, k, allowlist)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistedBaseKind {
    Single,
    Partitioned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistedChildDescriptor {
    build_identity: [u8; 32],
    vector_count: u64,
    graph_node_count: u64,
    entry_point: Option<ObjectId>,
    max_level: u16,
    complete: bool,
}

impl PersistedChildDescriptor {
    fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        Self {
            build_identity: snapshot.build_identity,
            vector_count: u64::try_from(snapshot.vectors.len()).unwrap_or(u64::MAX),
            graph_node_count: u64::try_from(snapshot.nodes.len()).unwrap_or(u64::MAX),
            entry_point: snapshot.entry_point,
            max_level: snapshot.max_level,
            complete: true,
        }
    }

    fn from_generation(descriptor: hyphae_native_ann::HnswGenerationDescriptor) -> Self {
        Self {
            build_identity: descriptor.build_identity,
            vector_count: u64::try_from(descriptor.vector_count).unwrap_or(u64::MAX),
            graph_node_count: u64::try_from(descriptor.graph_node_count).unwrap_or(u64::MAX),
            entry_point: descriptor.entry_point,
            max_level: descriptor.max_level,
            complete: true,
        }
    }

    const fn legacy(build_identity: [u8; 32]) -> Self {
        Self {
            build_identity,
            vector_count: 0,
            graph_node_count: 0,
            entry_point: None,
            max_level: 0,
            complete: false,
        }
    }
}

#[derive(Clone, Debug)]
struct PersistedIndexMetadata {
    build_identity: [u8; 32],
    vector_count: u64,
    base_kind: PersistedBaseKind,
    input_identity: Option<[u8; 32]>,
    children: Vec<PersistedChildDescriptor>,
    view_identity: [u8; 32],
    delta_count: u64,
    delta_bytes: u64,
    next_sequence: u64,
    lifecycle: IncrementalVectorLifecycle,
    retained_generations: Vec<RetainedGeneration>,
    version: u8,
    overlay: Option<PersistedOverlayMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PersistedOverlayMetadata {
    legacy_view_identity: [u8; 32],
    overlay_root: [u8; 32],
    overlay_count: u64,
    overlay_bytes: u64,
    overlay_node_count: u64,
    effective_count: u64,
    effective_bytes: u64,
    legacy_next_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverlayManifest {
    legacy_view_identity: [u8; 32],
    overlay_root: [u8; 32],
    view_identity: [u8; 32],
    legacy_count: u64,
    legacy_bytes: u64,
    overlay_count: u64,
    overlay_bytes: u64,
    overlay_node_count: u64,
    effective_count: u64,
    effective_bytes: u64,
    legacy_next_sequence: u64,
    next_sequence: u64,
}

impl PersistedIndexMetadata {
    fn current_child_identities(&self) -> BTreeSet<[u8; 32]> {
        self.children
            .iter()
            .map(|child| child.build_identity)
            .collect()
    }

    fn retained_child_identities(&self) -> BTreeSet<[u8; 32]> {
        self.retained_generations
            .iter()
            .flat_map(|generation| generation.children.iter().map(|child| child.build_identity))
            .collect()
    }

    fn owns_physical_identity(&self, identity: [u8; 32]) -> bool {
        self.children
            .iter()
            .any(|child| child.build_identity == identity)
            || self.retained_generations.iter().any(|generation| {
                generation
                    .children
                    .iter()
                    .any(|child| child.build_identity == identity)
            })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConsolidationPlan {
    index: ObjectId,
    base_identity: [u8; 32],
    captured_view_identity: [u8; 32],
    captured_deltas: BTreeMap<ObjectId, u64>,
    replacement: Arc<ConsolidationReplacement>,
}

#[derive(Clone, Copy)]
pub(crate) struct ConsolidationBuildExecution<'a> {
    pub(crate) pool: Option<&'a NativeExecutionPool>,
    pub(crate) permit: Option<&'a OwnedGovernorPermit>,
    pub(crate) cancellation: Option<&'a GovernorCancellation>,
}

#[derive(Clone, Debug)]
enum ConsolidationReplacement {
    Single(IndexSnapshot),
    Partitioned(PartitionedIndexSnapshot),
}

impl ConsolidationReplacement {
    fn definition(&self) -> VectorIndexDefinition {
        match self {
            Self::Single(snapshot) => snapshot.definition,
            Self::Partitioned(snapshot) => snapshot.definition,
        }
    }

    fn build_identity(&self) -> [u8; 32] {
        match self {
            Self::Single(snapshot) => snapshot.build_identity,
            Self::Partitioned(snapshot) => snapshot.build_identity,
        }
    }

    fn input_identity(&self) -> Option<[u8; 32]> {
        match self {
            Self::Single(_) => None,
            Self::Partitioned(snapshot) => Some(snapshot.input_identity),
        }
    }

    fn snapshots(&self) -> &[IndexSnapshot] {
        match self {
            Self::Single(snapshot) => std::slice::from_ref(snapshot),
            Self::Partitioned(snapshot) => &snapshot.partitions,
        }
    }

    fn len(&self) -> usize {
        self.snapshots()
            .iter()
            .map(|snapshot| snapshot.vectors.len())
            .sum()
    }

    fn base_kind(&self) -> PersistedBaseKind {
        match self {
            Self::Single(_) => PersistedBaseKind::Single,
            Self::Partitioned(_) => PersistedBaseKind::Partitioned,
        }
    }

    fn child_descriptors(&self) -> Vec<PersistedChildDescriptor> {
        self.snapshots()
            .iter()
            .map(PersistedChildDescriptor::from_snapshot)
            .collect()
    }
}

impl ConsolidationPlan {
    pub(crate) const fn index(&self) -> ObjectId {
        self.index
    }

    pub(crate) const fn base_identity(&self) -> [u8; 32] {
        self.base_identity
    }

    pub(crate) const fn captured_view_identity(&self) -> [u8; 32] {
        self.captured_view_identity
    }

    pub(crate) fn definition(&self) -> VectorIndexDefinition {
        self.replacement.definition()
    }

    pub(crate) fn captured_delta_count(&self) -> usize {
        self.captured_deltas.len()
    }

    pub(crate) fn effective_vector_count(&self) -> usize {
        self.replacement.len()
    }

    pub(crate) fn replacement_identity(&self) -> [u8; 32] {
        self.replacement.build_identity()
    }
}

pub(crate) fn consolidation_prefix_replacement_limits(
    load_plan: &AnnIndexLoadPlan,
    plan: &ConsolidationPlan,
) -> Result<PrefixReplacementStructuralLimits, NativeRuntimeError> {
    let candidate_entries =
        plan.replacement
            .snapshots()
            .iter()
            .try_fold(0_usize, |total, snapshot| {
                let graph_entries = snapshot.nodes.iter().try_fold(0_usize, |nodes, node| {
                    nodes
                        .checked_add(node.neighbors.len())
                        .ok_or(NativeRuntimeError::InvalidAnnTree)
                })?;
                total
                    .checked_add(snapshot.vectors.len())
                    .and_then(|entries| entries.checked_add(graph_entries))
                    .ok_or(NativeRuntimeError::InvalidAnnTree)
            })?;
    let maximum_entries = 2_usize
        .checked_add(load_plan.planned_physical_entries())
        .and_then(|entries| entries.checked_add(candidate_entries))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    PrefixReplacementStructuralLimits::new(maximum_entries, ANN_GRAPH_LAYER_KEY_SIZE)
        .map_err(Into::into)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IndexObservation {
    pub(crate) base_identity: [u8; 32],
    pub(crate) view_identity: [u8; 32],
    pub(crate) base_vector_count: usize,
    pub(crate) effective_vector_count: usize,
    pub(crate) delta_records: usize,
    pub(crate) delta_bytes: usize,
    pub(crate) generation_records: usize,
    pub(crate) selected_generation_records: usize,
    pub(crate) lifecycle: IncrementalVectorLifecycle,
    pub(crate) maintenance_due: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MaintenanceStatus {
    pub(crate) lifecycle: IncrementalVectorLifecycle,
    pub(crate) delta_records: usize,
    pub(crate) delta_bytes: usize,
    pub(crate) due: bool,
}

/// Immutable authority for loading one ANN index from one committed search root.
///
/// Planning decodes only the target metadata. The potentially large physical
/// generation is not materialized until the caller has admitted
/// [`Self::hydration_memory_bytes`].
pub(crate) struct AnnIndexLoadPlan {
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
    encoded_metadata: Vec<u8>,
    hydration_memory_bytes: u64,
    physical_limits: AnnPhysicalLimits,
}

impl AnnIndexLoadPlan {
    pub(crate) const fn hydration_memory_bytes(&self) -> u64 {
        self.hydration_memory_bytes
    }

    pub(crate) fn planned_physical_entries(&self) -> usize {
        self.physical_limits.total_entries()
    }

    pub(crate) fn planned_physical_bytes(&self) -> u64 {
        self.physical_limits.total_bytes()
    }

    #[cfg(test)]
    pub(crate) fn physical_entry_limit(&self) -> usize {
        self.physical_limits.total_entries()
    }
}

/// Bounded authority for mutating one index's durable object delta without
/// restoring its immutable HNSW base generation.
pub(crate) struct AnnDeltaMutationPlan {
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
    expected_metadata: Vec<u8>,
    metadata: PersistedIndexMetadata,
    retained_memory_bytes: u64,
    delta_limit: AnnPhysicalRangeLimit,
}

impl AnnDeltaMutationPlan {
    pub(crate) const fn retained_memory_bytes(&self) -> u64 {
        self.retained_memory_bytes
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AnnDeltaMutationState {
    definition: VectorIndexDefinition,
    expected_metadata: Vec<u8>,
    metadata: PersistedIndexMetadata,
    deltas: BTreeMap<ObjectId, DeltaRecord>,
    staged_objects: BTreeSet<ObjectId>,
    retained_memory_bytes: u64,
}

impl AnnDeltaMutationState {
    pub(crate) const fn retained_memory_bytes(&self) -> u64 {
        self.retained_memory_bytes
    }

    pub(crate) fn upsert_retained_memory_bytes(vector: &Vector) -> u64 {
        u64::try_from(vector.dimension())
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap_or(u64::MAX))
            .saturating_add(512)
    }

    pub(crate) fn upsert(
        &mut self,
        object_id: ObjectId,
        vector: Vector,
    ) -> Result<(), NativeRuntimeError> {
        if self.staged_objects.contains(&object_id) {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        validate_vector(self.definition, &vector)?;
        let sequence = self.metadata.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(NativeRuntimeError::AnnDeltaLimitExceeded)?;
        let previous = self.deltas.insert(
            object_id,
            DeltaRecord::Upsert {
                sequence,
                record: VectorRecord {
                    object_id,
                    creating_csn: private_mutation_csn()?,
                    vector,
                },
            },
        );
        if let Err(error) = validate_delta_mutation_bounds(&self.metadata, &self.deltas) {
            if let Some(previous) = previous {
                self.deltas.insert(object_id, previous);
            } else {
                self.deltas.remove(&object_id);
            }
            return Err(error);
        }
        self.metadata.next_sequence = next_sequence;
        self.staged_objects.insert(object_id);
        self.retained_memory_bytes =
            self.retained_memory_bytes
                .saturating_add(Self::upsert_retained_memory_bytes(
                    match self.deltas.get(&object_id) {
                        Some(DeltaRecord::Upsert { record, .. }) => &record.vector,
                        _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
                    },
                ));
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct AnnPhysicalRangeLimit {
    entries: usize,
    bytes: u64,
}

#[derive(Clone, Copy)]
struct AnnPhysicalLimits {
    vectors: AnnPhysicalRangeLimit,
    graph_layers: AnnPhysicalRangeLimit,
    deltas: AnnPhysicalRangeLimit,
    overlay_manifest: AnnPhysicalRangeLimit,
    overlay_deltas: AnnPhysicalRangeLimit,
    overlay_nodes: AnnPhysicalRangeLimit,
}

impl AnnPhysicalLimits {
    fn total_entries(self) -> usize {
        self.vectors
            .entries
            .saturating_add(self.graph_layers.entries)
            .saturating_add(self.deltas.entries)
            .saturating_add(self.overlay_manifest.entries)
            .saturating_add(self.overlay_deltas.entries)
            .saturating_add(self.overlay_nodes.entries)
    }

    fn total_bytes(self) -> u64 {
        self.vectors
            .bytes
            .saturating_add(self.graph_layers.bytes)
            .saturating_add(self.deltas.bytes)
            .saturating_add(self.overlay_manifest.bytes)
            .saturating_add(self.overlay_deltas.bytes)
            .saturating_add(self.overlay_nodes.bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnOwnedBaseKind {
    Single,
    Partitioned,
}

#[derive(Clone, Debug)]
pub(crate) struct AnnOwnedReadAuthority {
    pub(crate) definition_digest: [u8; 32],
    pub(crate) dimension: u16,
    pub(crate) base_kind: AnnOwnedBaseKind,
    pub(crate) child_identities: Vec<[u8; 32]>,
    pub(crate) base_build_identity: [u8; 32],
    pub(crate) view_identity: [u8; 32],
    pub(crate) logical_partitions: usize,
    pub(crate) base_vector_count: usize,
    pub(crate) delta_records: usize,
    pub(crate) delta_bytes: usize,
    pub(crate) next_sequence: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct AnnOwnedReadState {
    state: Arc<AnnIndexState>,
    authority: AnnOwnedReadAuthority,
}

impl AnnOwnedReadState {
    pub(crate) fn authority(&self) -> &AnnOwnedReadAuthority {
        &self.authority
    }

    pub(crate) fn search_selected_parallel(
        &self,
        query: &Vector,
        options: SearchOptions,
        maximum_partitions: usize,
        execution: AnnParallelSearchExecution<'_>,
    ) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
        Arc::clone(&self.state).search_selected_parallel(
            query,
            options,
            maximum_partitions,
            execution,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnnHydrationObservation {
    pub(crate) physical_entries: usize,
    pub(crate) physical_bytes: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct AnnParallelSearchExecution<'a> {
    pub(crate) pool: &'a NativeExecutionPool,
    pub(crate) permit: &'a OwnedGovernorPermit,
    pub(crate) cancellation: Option<&'a GovernorCancellation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InitialBulkAuthority {
    pub(crate) definition: VectorIndexDefinition,
    pub(crate) lifecycle: IncrementalVectorLifecycle,
    pub(crate) base_identity: [u8; 32],
    pub(crate) view_identity: [u8; 32],
}

pub(crate) fn definition_from_search(
    definition: &SearchCollectionDefinition,
) -> Result<VectorIndexDefinition, NativeRuntimeError> {
    let vector = definition
        .vector
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let ann = definition.ann.ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let metric = match ann.metric() {
        VectorMetric::Cosine => Metric::Cosine,
        VectorMetric::NegativeDot => Metric::NegativeDot,
        VectorMetric::SquaredL2 => Metric::SquaredL2,
    };
    let config = HnswConfig::new(
        ann.m(),
        ann.ef_construction(),
        ann.ef_search_default(),
        ann.ef_search_max(),
        ann.seed(),
    )?;
    Ok(VectorIndexDefinition::new(
        definition.header.id,
        vector.dimension(),
        metric,
        config,
    )?)
}

pub(crate) fn encode_vector_mutation(vector: &Vector) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(vector.values().len().saturating_mul(4));
    for component in vector.values() {
        encoded.extend_from_slice(&component.to_bits().to_le_bytes());
    }
    encoded
}

pub(crate) fn decode_vector_mutation(encoded: &[u8]) -> Result<Vector, NativeRuntimeError> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(4) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Vector::new(encoded.chunks_exact(4).map(|component| {
        let mut bits = [0_u8; 4];
        bits.copy_from_slice(component);
        f32::from_bits(u32::from_le_bytes(bits))
    }))
    .map_err(NativeRuntimeError::from)
}

pub(crate) fn encode_object_identity(object_id: ObjectId) -> Vec<u8> {
    object_id.get().to_be_bytes().to_vec()
}

pub(crate) fn decode_object_identity(encoded: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
    ObjectId::new(u128::from_be_bytes(bytes))
        .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)
}

pub(crate) fn private_mutation_csn() -> Result<Csn, NativeRuntimeError> {
    Csn::new(PRIVATE_MUTATION_CSN).map_err(|_| NativeRuntimeError::InvalidPreparedMutation)
}

pub(crate) fn is_ann_physical_key(key: &[u8]) -> bool {
    matches!(
        key.first().copied(),
        Some(
            ANN_INDEX_META_PREFIX
                | ANN_VECTOR_PREFIX
                | ANN_GRAPH_LAYER_PREFIX
                | ANN_DELTA_PREFIX
                | ANN_OVERLAY_MANIFEST_PREFIX
                | ANN_OVERLAY_DELTA_PREFIX
                | ANN_OVERLAY_NODE_PREFIX
        )
    )
}

pub(crate) fn capture_initial_bulk_authority(
    pages: &PageStore,
    root: PageId,
    catalog: &CatalogState,
    index: ObjectId,
) -> Result<InitialBulkAuthority, NativeRuntimeError> {
    let state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state
        .indexes
        .get(&index)
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
    if current.persisted_version == 5
        || !matches!(current.base, AnnBase::Single(_))
        || current.base.len() != 0
        || !current.deltas.is_empty()
        || !current.retained_generations.is_empty()
        || current.next_sequence != 1
        || current.view_identity != current.base.build_identity()
    {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(InitialBulkAuthority {
        definition: current.definition(),
        lifecycle: current.lifecycle,
        base_identity: current.base.build_identity(),
        view_identity: current.view_identity,
    })
}

pub(crate) fn encode_initial_bulk_publication(
    publication: &crate::InitialAnnBulkPublication,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let snapshot = &publication.candidate;
    if publication.expected_base_identity == [0; 32]
        || publication.expected_view_identity == [0; 32]
        || snapshot.definition.index_id() != publication.index
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_initial_bulk_candidate(publication.candidate_csn, snapshot)?;
    let mut encoded = Vec::with_capacity(160);
    encoded.extend_from_slice(b"HYANNP01");
    encoded.extend_from_slice(&publication.expected_base_identity);
    encoded.extend_from_slice(&publication.expected_view_identity);
    encoded.extend_from_slice(&snapshot.input_identity);
    encoded.extend_from_slice(&snapshot.build_identity);
    encoded.extend_from_slice(
        &u64::try_from(snapshot.partitions.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &snapshot
            .partitions
            .iter()
            .try_fold(0_u64, |count, child| {
                count.checked_add(u64::try_from(child.vectors.len()).ok()?)
            })
            .ok_or(NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&publication.candidate_csn.get().to_le_bytes());
    Ok(encoded)
}

pub(crate) fn publish_initial_bulk_tree(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    catalog: &CatalogState,
    publication: &crate::InitialAnnBulkPublication,
) -> Result<BTree, NativeRuntimeError> {
    let root = root.ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if creating_csn != publication.candidate_csn {
        return Err(NativeRuntimeError::InitialAnnBulkStale);
    }
    let mut state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state.indexes.get_mut(&publication.index).ok_or(
        NativeRuntimeError::UnknownVectorIndex {
            index: publication.index,
        },
    )?;
    if current.persisted_version == 5
        || !matches!(current.base, AnnBase::Single(_))
        || current.base.len() != 0
        || !current.deltas.is_empty()
        || !current.retained_generations.is_empty()
        || current.next_sequence != 1
        || current.base.build_identity() != publication.expected_base_identity
        || current.view_identity != publication.expected_view_identity
    {
        return Err(NativeRuntimeError::InitialAnnBulkStale);
    }
    let snapshot = &publication.candidate;
    if snapshot.definition.index_id() != publication.index {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_initial_bulk_candidate(publication.candidate_csn, snapshot)?;
    if snapshot.definition != current.definition() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }

    let tree = BTree::from_root(root);
    let mut entries = tree.scan(pages)?;
    entries.retain(|(key, _)| !ann_key_targets_index(key, publication.index));
    let mut replacement_entries = BTreeMap::new();
    replacement_entries.insert(
        meta_key(publication.index),
        encode_initial_bulk_metadata(current, snapshot)?,
    );
    for child in &snapshot.partitions {
        append_generation_entries(&mut replacement_entries, child)?;
    }
    entries.extend(replacement_entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let replacement = BTree::empty()
        .upsert_sorted_batch(pages, creating_csn, entries)?
        .tree;
    validate_published_initial_bulk_tree(pages, replacement, catalog, publication)?;
    Ok(replacement)
}

fn validate_published_initial_bulk_tree(
    pages: &PageStore,
    tree: BTree,
    catalog: &CatalogState,
    publication: &crate::InitialAnnBulkPublication,
) -> Result<(), NativeRuntimeError> {
    let entries = tree.scan(pages)?;
    let metadata = decode_metadata_entries(&entries)?;
    let persisted = metadata
        .get(&publication.index)
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let expected_children = publication
        .candidate
        .partitions
        .iter()
        .map(PersistedChildDescriptor::from_snapshot)
        .collect::<Vec<_>>();
    if persisted.base_kind != PersistedBaseKind::Partitioned
        || persisted.build_identity != publication.candidate.build_identity
        || persisted.input_identity != Some(publication.candidate.input_identity)
        || persisted.children != expected_children
        || persisted.view_identity != publication.candidate.build_identity
        || persisted.delta_count != 0
        || persisted.delta_bytes != 0
        || persisted.next_sequence != 1
        || !persisted.retained_generations.is_empty()
        || catalog_ann_definition(catalog, publication.index)? != publication.candidate.definition
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let selected = persisted.current_child_identities();
    let physical_entries = PhysicalEntryIndex::build(&entries)?;
    if physical_entries.has_unselected_child(publication.index, &selected)
        || physical_entries.has_deltas(publication.index)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    for descriptor in &persisted.children {
        validate_initial_bulk_child_entries(
            &entries,
            &physical_entries,
            publication.index,
            publication.candidate.definition,
            descriptor,
        )?;
    }
    Ok(())
}

fn validate_initial_bulk_child_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    physical_entries: &PhysicalEntryIndex,
    index: ObjectId,
    definition: VectorIndexDefinition,
    descriptor: &PersistedChildDescriptor,
) -> Result<(), NativeRuntimeError> {
    let child =
        physical_entries.restore_child(entries, index, definition, descriptor.build_identity)?;
    let snapshot = restore_child_snapshot(definition, descriptor, child)?;
    let restored =
        HnswIndex::restore_owned(snapshot).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if restored.definition() != definition
        || restored.build_identity() != descriptor.build_identity
        || u64::try_from(restored.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != descriptor.vector_count
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn validate_initial_bulk_candidate(
    candidate_csn: Csn,
    snapshot: &PartitionedIndexSnapshot,
) -> Result<(), NativeRuntimeError> {
    if snapshot.partitions.is_empty()
        || snapshot.input_identity == [0; 32]
        || snapshot.build_identity == [0; 32]
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut object_ids = BTreeSet::new();
    let mut vector_count = 0_usize;
    for child in &snapshot.partitions {
        if child.definition != snapshot.definition
            || child.build_identity == [0; 32]
            || child.vectors.is_empty()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        vector_count = vector_count
            .checked_add(child.vectors.len())
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        for record in &child.vectors {
            if record.creating_csn != candidate_csn || !object_ids.insert(record.object_id) {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
        }
    }
    if vector_count == 0 || snapshot.partitions.len() > vector_count {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn apply_tree_mutations(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    catalog: &CatalogState,
    mutations: &[Mutation],
    delta_mutations: Option<&BTreeMap<ObjectId, AnnDeltaMutationState>>,
) -> Result<BTree, NativeRuntimeError> {
    let ann_mutations = mutations
        .iter()
        .filter(|mutation| {
            matches!(
                mutation.opcode,
                Opcode::CreateAnnIndex | Opcode::UpsertVector | Opcode::DeleteVector
            )
        })
        .collect::<Vec<_>>();
    if ann_mutations.is_empty() {
        return Ok(tree);
    }
    if let Some(delta_mutations) = delta_mutations.filter(|mutations| !mutations.is_empty()) {
        return apply_delta_tree_mutations(
            pages,
            tree,
            creating_csn,
            catalog,
            &ann_mutations,
            delta_mutations,
        );
    }

    crate::record_full_state_materialization()?;
    let mut state = load_from_tree(pages, tree.root(), catalog, false)?;
    let mut changed = BTreeMap::<ObjectId, BTreeSet<ObjectId>>::new();
    let mut created = BTreeSet::new();
    let mut initial_vectors = BTreeMap::<ObjectId, BTreeMap<ObjectId, Vector>>::new();
    for mutation in ann_mutations {
        let index = mutation
            .target
            .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
        match mutation.opcode {
            Opcode::CreateAnnIndex => {
                let (object, lifecycle) = crate::decode_ann_creation(index, mutation)?;
                let CatalogObject::Search(definition) = object else {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                };
                if definition.header.id != index || !created.insert(index) {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                }
                state.create(definition_from_search(&definition)?, lifecycle)?;
                initial_vectors.insert(index, BTreeMap::new());
            }
            Opcode::UpsertVector => {
                let object_id = decode_object_identity(&mutation.key)?;
                let vector = decode_vector_mutation(&mutation.value)?;
                if let Some(vectors) = initial_vectors.get_mut(&index) {
                    vectors.insert(object_id, vector);
                } else {
                    state.upsert(index, object_id, creating_csn, vector)?;
                    changed.entry(index).or_default().insert(object_id);
                }
            }
            Opcode::DeleteVector => {
                let object_id = decode_object_identity(&mutation.key)?;
                if let Some(vectors) = initial_vectors.get_mut(&index) {
                    if !mutation.value.is_empty() || vectors.remove(&object_id).is_none() {
                        return Err(NativeRuntimeError::InvalidPreparedMutation);
                    }
                } else {
                    if !state
                        .indexes
                        .get_mut(&index)
                        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
                        .delete(object_id, creating_csn)?
                    {
                        return Err(NativeRuntimeError::InvalidPreparedMutation);
                    }
                    changed.entry(index).or_default().insert(object_id);
                }
            }
            _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
        }
    }

    for (index, vectors) in initial_vectors {
        state.upsert_initial_many(
            index,
            creating_csn,
            &vectors.into_iter().collect::<Vec<_>>(),
        )?;
        changed.remove(&index);
    }

    validate_catalog_coverage(catalog, &state)?;
    let mut entries = BTreeMap::new();
    entries.insert(
        crate::SEARCH_FORMAT_KEY.to_vec(),
        crate::SEARCH_FORMAT_VALUE_V3.to_vec(),
    );
    for index in created.iter().chain(changed.keys()) {
        let index_state = state
            .indexes
            .get(index)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        entries.insert(meta_key(*index), encode_metadata(index_state)?);
        if created.contains(index) {
            append_base_generation_entries(&mut entries, &index_state.base)?;
        }
        if let Some(objects) = changed.get(index) {
            for object_id in objects {
                let delta = index_state
                    .deltas
                    .get(object_id)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                entries.insert(delta_key(*index, *object_id), encode_delta(delta)?);
            }
        }
    }
    tree = tree
        .upsert_sorted_batch(pages, creating_csn, entries.into_iter().collect())?
        .tree;
    Ok(tree)
}

fn apply_delta_tree_mutations(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    catalog: &CatalogState,
    mutations: &[&Mutation],
    authorities: &BTreeMap<ObjectId, AnnDeltaMutationState>,
) -> Result<BTree, NativeRuntimeError> {
    validate_delta_mutation_shape(authorities, mutations.iter().copied())?;
    let mut entries = BTreeMap::new();
    for (index, authority) in authorities {
        if catalog_ann_definition(catalog, *index)? != authority.definition {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let current_metadata = tree
            .get(pages, &meta_key(*index))?
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if current_metadata != authority.expected_metadata {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let view_identity = calculate_view_identity_at_csn(
            authority.metadata.build_identity,
            authority.metadata.next_sequence,
            &authority.deltas,
            Some((&authority.staged_objects, creating_csn)),
        );
        entries.insert(
            meta_key(*index),
            encode_delta_metadata(
                &authority.metadata,
                &authority.expected_metadata,
                &authority.deltas,
                view_identity,
            )?,
        );
        for object_id in &authority.staged_objects {
            let delta = authority
                .deltas
                .get(object_id)
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
            entries.insert(
                delta_key(*index, *object_id),
                encode_delta_at_csn(delta, creating_csn)?,
            );
        }
    }
    Ok(tree
        .upsert_sorted_batch(pages, creating_csn, entries.into_iter().collect())?
        .tree)
}

pub(crate) fn validate_delta_mutation_batch(
    authorities: &BTreeMap<ObjectId, AnnDeltaMutationState>,
    mutations: &[Mutation],
) -> Result<(), NativeRuntimeError> {
    validate_delta_mutation_shape(
        authorities,
        mutations.iter().filter(|mutation| {
            matches!(mutation.opcode, Opcode::UpsertVector | Opcode::DeleteVector)
        }),
    )
}

fn validate_delta_mutation_shape<'a>(
    authorities: &BTreeMap<ObjectId, AnnDeltaMutationState>,
    mutations: impl IntoIterator<Item = &'a Mutation>,
) -> Result<(), NativeRuntimeError> {
    let mut represented = BTreeMap::<ObjectId, BTreeSet<ObjectId>>::new();
    for mutation in mutations {
        let index = mutation
            .target
            .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
        let object_id = decode_object_identity(&mutation.key)?;
        let authority = authorities
            .get(&index)
            .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
        let vector = decode_vector_mutation(&mutation.value)?;
        let valid = mutation.engine == hyphae_native_types::EngineKind::Search
            && mutation.opcode == Opcode::UpsertVector
            && mutation.expires_at_micros.is_none()
            && matches!(authority.deltas.get(&object_id),
                Some(DeltaRecord::Upsert { record, .. }) if record.vector == vector);
        if !valid {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        represented.entry(index).or_default().insert(object_id);
    }
    if represented.len() != authorities.len()
        || authorities.iter().any(|(index, authority)| {
            represented.get(index) != Some(&authority.staged_objects)
                || validate_delta_mutation_bounds(&authority.metadata, &authority.deltas).is_err()
        })
    {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(())
}

pub(crate) fn load(
    pages: &PageStore,
    root: Option<PageId>,
    catalog: &CatalogState,
) -> Result<AnnState, NativeRuntimeError> {
    load_with_memory_limit(pages, root, catalog, crate::RECOVERY_MEMORY_BYTES)
}

pub(crate) fn load_with_memory_limit(
    pages: &PageStore,
    root: Option<PageId>,
    catalog: &CatalogState,
    memory_limit: u64,
) -> Result<AnnState, NativeRuntimeError> {
    load_from_tree_with_memory_limit(pages, root, catalog, true, memory_limit)
}

/// Plans a bounded load of exactly one ANN index without restoring HNSW.
///
/// The returned memory bound includes current and retained physical
/// generations because target validation is fail-closed for every identity
/// owned by the target metadata.
pub(crate) fn plan_index_load(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<AnnIndexLoadPlan, NativeRuntimeError> {
    plan_index_load_with_cancellation(pages, buffer_pool, root, index, definition, None)
}

pub(crate) fn plan_delta_mutation(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<AnnDeltaMutationPlan, NativeRuntimeError> {
    if definition.index_id() != index
        || !matches!(
            pages.read(root)?.kind(),
            PageKind::BTreeLeaf | PageKind::BTreeInternal
        )
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let tree = BTree::from_root(root);
    let marker = tree
        .get_cached_pinned(pages, buffer_pool, crate::SEARCH_FORMAT_KEY)?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if marker.bytes() != crate::SEARCH_FORMAT_VALUE_V1
        && marker.bytes() != crate::SEARCH_FORMAT_VALUE_V2
        && marker.bytes() != crate::SEARCH_FORMAT_VALUE_V3
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let expected_metadata = tree
        .get_cached_pinned(pages, buffer_pool, &meta_key(index))?
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
        .bytes()
        .to_vec();
    let metadata = decode_metadata(&expected_metadata)?;
    if metadata.version == 5 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let delta_entries =
        usize::try_from(metadata.delta_count).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let delta_limit = AnnPhysicalRangeLimit {
        entries: delta_entries,
        bytes: metadata
            .delta_bytes
            .checked_add(
                metadata
                    .delta_count
                    .saturating_mul(u64::try_from(ANN_DELTA_KEY_SIZE).unwrap_or(u64::MAX)),
            )
            .ok_or(NativeRuntimeError::InvalidAnnTree)?,
    };
    let retained_memory_bytes = delta_mutation_memory_bytes(&metadata, expected_metadata.len())?;
    Ok(AnnDeltaMutationPlan {
        root,
        index,
        definition,
        expected_metadata,
        metadata,
        retained_memory_bytes,
        delta_limit,
    })
}

pub(crate) fn load_delta_mutation(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: AnnDeltaMutationPlan,
) -> Result<AnnDeltaMutationState, NativeRuntimeError> {
    let tree = BTree::from_root(plan.root);
    let current_metadata = tree
        .get_cached_pinned(pages, buffer_pool, &meta_key(plan.index))?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if current_metadata.bytes() != plan.expected_metadata {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut entries = Vec::with_capacity(plan.delta_limit.entries);
    visit_bounded_physical_range(
        tree,
        pages,
        buffer_pool,
        &object_prefix(ANN_DELTA_PREFIX, plan.index),
        plan.delta_limit,
        None,
        &mut entries,
    )?;
    let mut deltas = BTreeMap::new();
    for (key, value) in entries {
        let (index, object_id) = decode_delta_key(&key)?;
        if index != plan.index
            || deltas
                .insert(object_id, decode_delta(&value, object_id, plan.definition)?)
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    validate_restored_deltas(&plan.metadata, &deltas)?;
    let maximum_sequence = deltas
        .values()
        .map(DeltaRecord::sequence)
        .max()
        .unwrap_or(0);
    if plan.metadata.next_sequence == 0
        || plan.metadata.next_sequence <= maximum_sequence
        || plan.metadata.view_identity
            != calculate_view_identity(
                plan.metadata.build_identity,
                plan.metadata.next_sequence,
                &deltas,
            )
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(AnnDeltaMutationState {
        definition: plan.definition,
        expected_metadata: plan.expected_metadata,
        metadata: plan.metadata,
        deltas,
        staged_objects: BTreeSet::new(),
        retained_memory_bytes: plan.retained_memory_bytes,
    })
}

#[cfg(test)]
pub(crate) fn delta_record_identity_for_test(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
    object_id: ObjectId,
) -> Result<(Csn, [u8; 32], [u8; 32]), NativeRuntimeError> {
    let plan = plan_delta_mutation(pages, buffer_pool, root, index, definition)?;
    let state = load_delta_mutation(pages, buffer_pool, plan)?;
    let creating_csn = match state.deltas.get(&object_id) {
        Some(DeltaRecord::Upsert { record, .. }) => record.creating_csn,
        _ => return Err(NativeRuntimeError::InvalidAnnTree),
    };
    let calculated = calculate_view_identity(
        state.metadata.build_identity,
        state.metadata.next_sequence,
        &state.deltas,
    );
    Ok((creating_csn, state.metadata.view_identity, calculated))
}

fn delta_mutation_memory_bytes(
    metadata: &PersistedIndexMetadata,
    metadata_bytes: usize,
) -> Result<u64, NativeRuntimeError> {
    const FIXED_BYTES: u64 = 1024 * 1024;
    const RECORD_OVERHEAD_BYTES: u64 = 512;
    metadata
        .delta_bytes
        .checked_mul(2)
        .and_then(|bytes| {
            bytes.checked_add(metadata.delta_count.saturating_mul(RECORD_OVERHEAD_BYTES))
        })
        .and_then(|bytes| {
            bytes.checked_add(
                u64::try_from(metadata_bytes)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(2),
            )
        })
        .and_then(|bytes| bytes.checked_add(FIXED_BYTES))
        .ok_or(NativeRuntimeError::InvalidAnnTree)
}

pub(crate) fn plan_index_load_with_cancellation(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    root: PageId,
    index: ObjectId,
    definition: VectorIndexDefinition,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnIndexLoadPlan, NativeRuntimeError> {
    reject_cancelled_ann_search(cancellation)?;
    if definition.index_id() != index
        || !matches!(
            pages.read(root)?.kind(),
            PageKind::BTreeLeaf | PageKind::BTreeInternal
        )
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    reject_cancelled_ann_search(cancellation)?;
    let tree = BTree::from_root(root);
    let marker = tree
        .get_cached_pinned(pages, buffer_pool, crate::SEARCH_FORMAT_KEY)?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if marker.bytes() != crate::SEARCH_FORMAT_VALUE_V1
        && marker.bytes() != crate::SEARCH_FORMAT_VALUE_V2
        && marker.bytes() != crate::SEARCH_FORMAT_VALUE_V3
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    reject_cancelled_ann_search(cancellation)?;
    let encoded_metadata = tree
        .get_cached_pinned(pages, buffer_pool, &meta_key(index))?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?
        .bytes()
        .to_vec();
    reject_cancelled_ann_search(cancellation)?;
    let metadata = decode_metadata(&encoded_metadata)?;
    let physical_limits = index_physical_limits(definition, &metadata)?;
    let hydration_memory_bytes = index_hydration_memory_bytes(definition, &metadata)?;
    Ok(AnnIndexLoadPlan {
        root,
        index,
        definition,
        encoded_metadata,
        hydration_memory_bytes,
        physical_limits,
    })
}

/// Restores and queries the exact index bound by `plan`.
///
/// Callers must hold the governor admission described by the plan before
/// entering this function.
#[cfg(test)]
pub(crate) fn search_selected_planned(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    query: &Vector,
    options: SearchOptions,
    maximum_partitions: usize,
) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
    search_selected_planned_with_cancellation(
        pages,
        buffer_pool,
        plan,
        query,
        options,
        maximum_partitions,
        None,
    )
}

pub(crate) fn search_selected_planned_with_cancellation(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    query: &Vector,
    options: SearchOptions,
    maximum_partitions: usize,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
    let state = load_planned_index(pages, buffer_pool, plan, cancellation)?;
    reject_cancelled_ann_search(cancellation)?;
    state.search_selected(query, options, maximum_partitions)
}

pub(crate) fn search_selected_planned_parallel(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    query: &Vector,
    options: SearchOptions,
    maximum_partitions: usize,
    execution: AnnParallelSearchExecution<'_>,
) -> Result<AnnRoutedSearchExecution, NativeRuntimeError> {
    reject_cancelled_ann_search(execution.cancellation)?;
    let state = load_planned_index(pages, buffer_pool, plan, execution.cancellation)?;
    Arc::new(state).search_selected_parallel(query, options, maximum_partitions, execution)
}

pub(crate) fn hydrate_owned_read_state(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    cancellation: Option<&GovernorCancellation>,
) -> Result<(AnnOwnedReadState, AnnHydrationObservation), NativeRuntimeError> {
    let (state, entries, streamed_nodes) =
        load_planned_index_with_entries(pages, buffer_pool, plan, cancellation)?;
    reject_cancelled_ann_search(cancellation)?;
    let physical_bytes = entries.iter().try_fold(0_u64, |total, (key, value)| {
        let entry_bytes = u64::try_from(key.len().saturating_add(value.len()))
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        total
            .checked_add(entry_bytes)
            .ok_or(NativeRuntimeError::InvalidAnnTree)
    })?;
    let child_identities = state
        .base
        .child_descriptors()
        .into_iter()
        .map(|child| child.build_identity)
        .collect::<Vec<_>>();
    let logical_partitions = child_identities.len().max(1);
    let authority = AnnOwnedReadAuthority {
        definition_digest: state.definition().digest(),
        dimension: state.definition().dimension(),
        base_kind: if state.base.is_partitioned() {
            AnnOwnedBaseKind::Partitioned
        } else {
            AnnOwnedBaseKind::Single
        },
        child_identities,
        base_build_identity: state.base.build_identity(),
        view_identity: state.view_identity,
        logical_partitions,
        base_vector_count: state.base.len(),
        delta_records: state.deltas.len(),
        delta_bytes: state.delta_bytes(),
        next_sequence: state.next_sequence,
    };
    Ok((
        AnnOwnedReadState {
            state: Arc::new(state),
            authority,
        },
        AnnHydrationObservation {
            physical_entries: entries
                .len()
                .saturating_add(streamed_nodes.physical_entries),
            physical_bytes: physical_bytes
                .checked_add(streamed_nodes.physical_bytes)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
    ))
}

fn load_planned_index(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnIndexState, NativeRuntimeError> {
    load_planned_index_with_entries(pages, buffer_pool, plan, cancellation)
        .map(|(state, _, _)| state)
}

fn load_planned_index_with_entries(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    cancellation: Option<&GovernorCancellation>,
) -> Result<(AnnIndexState, Vec<KeyValue>, AnnHydrationObservation), NativeRuntimeError> {
    let tree = BTree::from_root(plan.root);
    let encoded_metadata = tree
        .get(pages, &meta_key(plan.index))?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if encoded_metadata != plan.encoded_metadata {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut metadata = decode_metadata(&encoded_metadata)?;
    let entries = scan_index_physical_entries(
        tree,
        pages,
        buffer_pool,
        plan.index,
        plan.physical_limits,
        cancellation,
    )?;
    enrich_target_legacy_retained_generations(
        &entries,
        plan.index,
        plan.definition,
        &mut metadata,
    )?;
    validate_target_physical_entries(&entries, plan.index, plan.definition, &metadata)?;
    let streamed_nodes = validate_overlay_nodes_in_tree(
        tree,
        pages,
        buffer_pool,
        plan.index,
        &metadata,
        &entries,
        plan.physical_limits.overlay_nodes,
        cancellation,
    )?;
    #[cfg(test)]
    ANN_INDEX_SCOPED_RESTORES.set(ANN_INDEX_SCOPED_RESTORES.get().saturating_add(1));
    ANN_INDEX_SCOPED_RESTORES_PROCESS.fetch_add(1, Ordering::Relaxed);
    let state = restore_index_with_definition_controlled(
        &entries,
        plan.index,
        plan.definition,
        metadata,
        cancellation,
        true,
    )?;
    Ok((state, entries, streamed_nodes))
}

fn scan_index_physical_entries(
    tree: BTree,
    pages: &PageStore,
    buffer_pool: &BufferPool,
    index: ObjectId,
    limits: AnnPhysicalLimits,
    cancellation: Option<&GovernorCancellation>,
) -> Result<Vec<KeyValue>, NativeRuntimeError> {
    let mut entries = Vec::new();
    for (prefix, limit) in [
        (ANN_VECTOR_PREFIX, limits.vectors),
        (ANN_GRAPH_LAYER_PREFIX, limits.graph_layers),
        (ANN_DELTA_PREFIX, limits.deltas),
        (ANN_OVERLAY_MANIFEST_PREFIX, limits.overlay_manifest),
        (ANN_OVERLAY_DELTA_PREFIX, limits.overlay_deltas),
    ] {
        visit_bounded_physical_range(
            tree,
            pages,
            buffer_pool,
            &object_prefix(prefix, index),
            limit,
            cancellation,
            &mut entries,
        )?;
    }
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(entries)
}

fn visit_bounded_physical_range(
    tree: BTree,
    pages: &PageStore,
    buffer_pool: &BufferPool,
    prefix: &[u8],
    limit: AnnPhysicalRangeLimit,
    cancellation: Option<&GovernorCancellation>,
    entries: &mut Vec<KeyValue>,
) -> Result<(), NativeRuntimeError> {
    enum Stop {
        Limit,
        Cancelled,
    }

    let starting_entries = entries.len();
    let mut visited_entries = 0_usize;
    let mut visited_bytes = 0_u64;
    let mut stop = None;
    let outcome = tree.visit_prefix_cached(pages, buffer_pool, prefix, None, |key, value| {
        if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
            stop = Some(Stop::Cancelled);
            return ControlFlow::Break(());
        }
        let Some(next_entries) = visited_entries.checked_add(1) else {
            stop = Some(Stop::Limit);
            return ControlFlow::Break(());
        };
        let encoded_bytes =
            u64::try_from(key.len().saturating_add(value.len())).unwrap_or(u64::MAX);
        let Some(next_bytes) = visited_bytes.checked_add(encoded_bytes) else {
            stop = Some(Stop::Limit);
            return ControlFlow::Break(());
        };
        if next_entries > limit.entries || next_bytes > limit.bytes {
            stop = Some(Stop::Limit);
            return ControlFlow::Break(());
        }
        visited_entries = next_entries;
        visited_bytes = next_bytes;
        entries.push((key.to_vec(), value.to_vec()));
        #[cfg(test)]
        ANN_INDEX_SCOPED_PEAK_PHYSICAL_ENTRIES.set(
            ANN_INDEX_SCOPED_PEAK_PHYSICAL_ENTRIES
                .get()
                .max(entries.len()),
        );
        ControlFlow::Continue(())
    })?;
    match (outcome, stop) {
        (ControlFlow::Continue(()), None) => Ok(()),
        (ControlFlow::Break(()), Some(Stop::Cancelled)) => {
            entries.truncate(starting_entries);
            Err(GovernorQueueError::Cancelled.into())
        }
        (ControlFlow::Break(()), Some(Stop::Limit)) => {
            entries.truncate(starting_entries);
            Err(NativeRuntimeError::InvalidAnnTree)
        }
        _ => Err(NativeRuntimeError::InvalidAnnTree),
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn validate_overlay_nodes_in_tree(
    tree: BTree,
    pages: &PageStore,
    buffer_pool: &BufferPool,
    index: ObjectId,
    metadata: &PersistedIndexMetadata,
    entries: &[KeyValue],
    limit: AnnPhysicalRangeLimit,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnHydrationObservation, NativeRuntimeError> {
    let mut physical_entries = 0_usize;
    let mut physical_bytes = 0_u64;
    let mut failed = false;
    let outcome = tree.visit_prefix_cached(
        pages,
        buffer_pool,
        &object_prefix(ANN_OVERLAY_NODE_PREFIX, index),
        None,
        |key, value| {
            if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
                return ControlFlow::Break(());
            }
            let encoded_bytes =
                u64::try_from(key.len().saturating_add(value.len())).unwrap_or(u64::MAX);
            let Some(next_entries) = physical_entries.checked_add(1) else {
                failed = true;
                return ControlFlow::Break(());
            };
            let Some(next_bytes) = physical_bytes.checked_add(encoded_bytes) else {
                failed = true;
                return ControlFlow::Break(());
            };
            if next_entries > limit.entries
                || next_bytes > limit.bytes
                || decode_overlay_node_key(key)
                    .and_then(|(found, depth, _)| {
                        if found != index {
                            return Err(NativeRuntimeError::InvalidAnnTree);
                        }
                        decode_overlay_node(value, depth).map(|_| ())
                    })
                    .is_err()
            {
                failed = true;
                return ControlFlow::Break(());
            }
            physical_entries = next_entries;
            physical_bytes = next_bytes;
            ControlFlow::Continue(())
        },
    )?;
    if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
        return Err(GovernorQueueError::Cancelled.into());
    }
    if failed || matches!(outcome, ControlFlow::Break(())) || physical_entries != limit.entries {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let Some(overlay) = metadata.overlay else {
        if physical_entries == 0 {
            return Ok(AnnHydrationObservation {
                physical_entries,
                physical_bytes,
            });
        }
        return Err(NativeRuntimeError::InvalidAnnTree);
    };
    let mut frontier = BTreeMap::new();
    for (key, value) in entries {
        if key.first() != Some(&ANN_OVERLAY_DELTA_PREFIX) {
            continue;
        }
        let (found, object_id) = decode_overlay_delta_key(key)?;
        if found != index
            || frontier
                .insert(object_id.get(), overlay_leaf_hash(object_id, value))
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    if frontier.is_empty() {
        if overlay.overlay_root != overlay_empty_hash(0) || overlay.overlay_node_count != 0 {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        return Ok(AnnHydrationObservation {
            physical_entries,
            physical_bytes,
        });
    }
    let mut expected_node_count = 0_u64;
    for depth in (0..ANN_OVERLAY_TREE_DEPTH).rev() {
        reject_cancelled_ann_search(cancellation)?;
        let (parents, expected) = expected_overlay_level(&frontier, depth)?;
        let mut position = 0_usize;
        let mut invalid = false;
        let outcome = tree.visit_prefix_cached(
            pages,
            buffer_pool,
            &overlay_node_prefix(index, depth),
            None,
            |key, value| {
                if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
                    return ControlFlow::Break(());
                }
                let valid = expected.get(position).is_some_and(|expected_node| {
                    decode_overlay_node_key(key).is_ok_and(|(found, found_depth, path)| {
                        found == index
                            && found_depth == depth
                            && path == expected_node.path
                            && decode_overlay_node(value, depth)
                                .is_ok_and(|node| node == expected_node.node)
                    })
                });
                if !valid {
                    invalid = true;
                    return ControlFlow::Break(());
                }
                position = position.saturating_add(1);
                ControlFlow::Continue(())
            },
        )?;
        if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
            return Err(GovernorQueueError::Cancelled.into());
        }
        if invalid || matches!(outcome, ControlFlow::Break(())) || position != expected.len() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        expected_node_count = expected_node_count
            .checked_add(
                u64::try_from(expected.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            )
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        frontier = parents;
    }
    if frontier.len() != 1
        || frontier.get(&0) != Some(&overlay.overlay_root)
        || expected_node_count != overlay.overlay_node_count
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(AnnHydrationObservation {
        physical_entries,
        physical_bytes,
    })
}

fn index_hydration_memory_bytes(
    definition: VectorIndexDefinition,
    metadata: &PersistedIndexMetadata,
) -> Result<u64, NativeRuntimeError> {
    const FIXED_BYTES: u64 = 2 * 1_024 * 1_024;
    const VECTOR_RECORD_OVERHEAD_BYTES: u64 = 256;
    const GRAPH_NODE_OVERHEAD_BYTES: u64 = 256;
    const EDGE_COPIES: u64 = 2;

    let (vectors, graph_layer_nodes) = index_physical_cardinality_bounds(metadata)?;
    let vector_bytes = u64::from(definition.dimension())
        .checked_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap_or(u64::MAX))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(VECTOR_RECORD_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_mul(vectors))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_bytes = u64::from(definition.config().m())
        .checked_mul(u64::try_from(std::mem::size_of::<ObjectId>()).unwrap_or(u64::MAX))
        .and_then(|bytes| bytes.checked_mul(EDGE_COPIES))
        .and_then(|bytes| bytes.checked_add(GRAPH_NODE_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_mul(graph_layer_nodes))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    FIXED_BYTES
        .checked_add(vector_bytes)
        .and_then(|bytes| bytes.checked_add(graph_bytes))
        .and_then(|bytes| bytes.checked_add(metadata.delta_bytes.saturating_mul(2)))
        .and_then(|bytes| {
            bytes.checked_add(
                metadata
                    .overlay
                    .map_or(0, |overlay| overlay.overlay_bytes.saturating_mul(2)),
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(
                metadata
                    .overlay
                    .map_or(0, |overlay| overlay.overlay_count.saturating_mul(256)),
            )
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)
}

fn index_physical_cardinality_bounds(
    metadata: &PersistedIndexMetadata,
) -> Result<(u64, u64), NativeRuntimeError> {
    let legacy_retained_vector_bound = metadata.vector_count.saturating_add(
        u64::from(metadata.lifecycle.delta_max_entries)
            .saturating_mul(u64::try_from(metadata.retained_generations.len()).unwrap_or(u64::MAX)),
    );
    let retained_vectors = metadata
        .retained_generations
        .iter()
        .flat_map(|generation| &generation.children)
        .try_fold(0_u64, |count, child| {
            count.checked_add(if child.complete {
                child.vector_count
            } else {
                legacy_retained_vector_bound
            })
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_layer_nodes = metadata
        .children
        .iter()
        .chain(
            metadata
                .retained_generations
                .iter()
                .flat_map(|generation| &generation.children),
        )
        .try_fold(0_u64, |count, child| {
            let graph_node_count = if child.complete {
                child.graph_node_count
            } else {
                legacy_retained_vector_bound
            };
            let max_level = if child.complete {
                child.max_level
            } else {
                MAX_HNSW_LEVEL
            };
            graph_node_count
                .checked_mul(u64::from(max_level).saturating_add(1))
                .and_then(|layer_nodes| count.checked_add(layer_nodes))
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let vectors = metadata
        .vector_count
        .checked_add(retained_vectors)
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    Ok((vectors, graph_layer_nodes))
}

fn index_physical_limits(
    definition: VectorIndexDefinition,
    metadata: &PersistedIndexMetadata,
) -> Result<AnnPhysicalLimits, NativeRuntimeError> {
    let (vector_entries, graph_entries) = index_physical_cardinality_bounds(metadata)?;
    let vector_record_bytes = u64::try_from(ANN_GENERATION_KEY_SIZE)
        .unwrap_or(u64::MAX)
        .checked_add(u64::try_from(ANN_VECTOR_HEADER_SIZE).unwrap_or(u64::MAX))
        .and_then(|bytes| {
            bytes.checked_add(
                u64::from(definition.dimension())
                    .saturating_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap_or(u64::MAX)),
            )
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_record_bytes = u64::try_from(ANN_GRAPH_LAYER_KEY_SIZE)
        .unwrap_or(u64::MAX)
        .checked_add(u64::try_from(ANN_GRAPH_LAYER_HEADER_SIZE).unwrap_or(u64::MAX))
        .and_then(|bytes| {
            bytes.checked_add(
                u64::from(definition.config().m())
                    .saturating_mul(2)
                    .saturating_mul(
                        u64::try_from(std::mem::size_of::<ObjectId>()).unwrap_or(u64::MAX),
                    ),
            )
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let delta_entries =
        usize::try_from(metadata.delta_count).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let (manifest_entries, overlay_entries, overlay_bytes, overlay_node_entries) =
        metadata.overlay.map_or((0, 0, 0, 0), |overlay| {
            (
                1,
                overlay.overlay_count,
                overlay.overlay_bytes,
                overlay.overlay_node_count,
            )
        });
    let overlay_entries =
        usize::try_from(overlay_entries).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let overlay_node_entries =
        usize::try_from(overlay_node_entries).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    Ok(AnnPhysicalLimits {
        vectors: AnnPhysicalRangeLimit {
            entries: usize::try_from(vector_entries)
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            bytes: vector_entries
                .checked_mul(vector_record_bytes)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
        graph_layers: AnnPhysicalRangeLimit {
            entries: usize::try_from(graph_entries)
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            bytes: graph_entries
                .checked_mul(graph_record_bytes)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
        deltas: AnnPhysicalRangeLimit {
            entries: delta_entries,
            bytes: metadata
                .delta_bytes
                .checked_add(
                    metadata
                        .delta_count
                        .saturating_mul(u64::try_from(ANN_DELTA_KEY_SIZE).unwrap_or(u64::MAX)),
                )
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
        overlay_manifest: AnnPhysicalRangeLimit {
            entries: manifest_entries,
            bytes: u64::try_from(ANN_INDEX_META_KEY_SIZE + ANN_OVERLAY_MANIFEST_SIZE)
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                .saturating_mul(u64::try_from(manifest_entries).unwrap_or(u64::MAX)),
        },
        overlay_deltas: AnnPhysicalRangeLimit {
            entries: overlay_entries,
            bytes: overlay_bytes
                .checked_add(
                    u64::try_from(overlay_entries)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(u64::try_from(ANN_DELTA_KEY_SIZE).unwrap_or(u64::MAX)),
                )
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
        overlay_nodes: AnnPhysicalRangeLimit {
            entries: overlay_node_entries,
            bytes: u64::try_from(ANN_OVERLAY_NODE_KEY_SIZE)
                .unwrap_or(u64::MAX)
                .saturating_add(u64::try_from(ANN_OVERLAY_NODE_HEADER_SIZE).unwrap_or(u64::MAX))
                .saturating_add(
                    u64::try_from(ANN_OVERLAY_FANOUT.saturating_mul(32)).unwrap_or(u64::MAX),
                )
                .checked_mul(u64::try_from(overlay_node_entries).unwrap_or(u64::MAX))
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        },
    })
}

#[cfg(test)]
pub(crate) fn reset_index_scoped_restore_count_for_test() {
    ANN_INDEX_SCOPED_RESTORES.set(0);
    ANN_INDEX_SCOPED_PEAK_PHYSICAL_ENTRIES.set(0);
}

#[cfg(test)]
pub(crate) fn index_scoped_restore_count_for_test() -> usize {
    ANN_INDEX_SCOPED_RESTORES.get()
}

pub(crate) fn process_index_scoped_restore_count() -> u64 {
    ANN_INDEX_SCOPED_RESTORES_PROCESS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn index_scoped_peak_physical_entries_for_test() -> usize {
    ANN_INDEX_SCOPED_PEAK_PHYSICAL_ENTRIES.get()
}

fn load_from_tree(
    pages: &PageStore,
    root: Option<PageId>,
    catalog: &CatalogState,
    require_complete: bool,
) -> Result<AnnState, NativeRuntimeError> {
    load_from_tree_with_memory_limit(
        pages,
        root,
        catalog,
        require_complete,
        crate::RECOVERY_MEMORY_BYTES,
    )
}

fn load_from_tree_with_memory_limit(
    pages: &PageStore,
    root: Option<PageId>,
    catalog: &CatalogState,
    require_complete: bool,
    memory_limit: u64,
) -> Result<AnnState, NativeRuntimeError> {
    if let Some(root) = root
        && pages.read(root)?.kind() == PageKind::SearchDelta
    {
        let state = AnnState::default();
        if require_complete {
            validate_catalog_coverage(catalog, &state)?;
        }
        return Ok(state);
    }
    let tree = root.map_or_else(BTree::empty, BTree::from_root);
    if tree_contains_metadata_v5(tree, pages)? {
        return load_layered_from_tree(tree, pages, catalog, require_complete, memory_limit);
    }
    let entries = tree.scan(pages)?;
    let mut metadata = decode_metadata_entries(&entries)?;
    enrich_legacy_retained_generations(&entries, catalog, &mut metadata)?;
    validate_physical_entries(&entries, catalog, &metadata)?;

    let mut state = AnnState::default();
    for (index, metadata) in metadata {
        let restored = restore_index(&entries, catalog, index, metadata)?;
        if state.indexes.insert(index, restored).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }

    if require_complete {
        validate_catalog_coverage(catalog, &state)?;
    }
    Ok(state)
}

fn tree_contains_metadata_v5(tree: BTree, pages: &PageStore) -> Result<bool, NativeRuntimeError> {
    let mut found = false;
    tree.visit_range_borrowed_with_control(
        pages,
        Bound::Included(&[ANN_INDEX_META_PREFIX]),
        Bound::Excluded(&[ANN_VECTOR_PREFIX]),
        metadata_borrowed_visit_limits(crate::RECOVERY_MEMORY_BYTES),
        || ControlFlow::Continue(()),
        |_, value| {
            if value.get(..8) == Some(ANN_INDEX_META_MAGIC_V5.as_slice()) {
                found = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )
    .map_err(map_borrowed_ann_visit_error)?;
    Ok(found)
}

fn load_layered_from_tree(
    tree: BTree,
    pages: &PageStore,
    catalog: &CatalogState,
    require_complete: bool,
    memory_limit: u64,
) -> Result<AnnState, NativeRuntimeError> {
    let mut planned = BTreeMap::new();
    let mut aggregate_memory = 0_u64;
    let mut failure = None;
    let metadata_stats = tree
        .visit_range_borrowed_with_control(
            pages,
            Bound::Included(&[ANN_INDEX_META_PREFIX]),
            Bound::Excluded(&[ANN_VECTOR_PREFIX]),
            metadata_borrowed_visit_limits(memory_limit),
            || ControlFlow::Continue(()),
            |key, value| {
                let result = (|| {
                    let index = decode_meta_key(key)?;
                    let metadata = decode_metadata(value)?;
                    let definition = catalog_ann_definition(catalog, index)?;
                    let limits = index_physical_limits(definition, &metadata)?;
                    let memory = index_hydration_memory_bytes(definition, &metadata)?;
                    aggregate_memory = aggregate_memory
                        .checked_add(memory)
                        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                    if aggregate_memory > memory_limit
                        || planned
                            .insert(
                                index,
                                StreamingIndexRestore::new(definition, metadata, limits),
                            )
                            .is_some()
                    {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failure = Some(error);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .map_err(map_borrowed_ann_visit_error)?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !metadata_stats.complete {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }

    let physical_limits = aggregate_streaming_physical_limits(planned.values())?;
    let mut failure = None;
    let physical_stats = tree
        .visit_range_borrowed_with_control(
            pages,
            Bound::Included(&[ANN_VECTOR_PREFIX]),
            Bound::Excluded(&[ANN_OVERLAY_NODE_PREFIX + 1]),
            physical_limits,
            || ControlFlow::Continue(()),
            |key, value| {
                let result = streamed_index_for_key(&mut planned, key)
                    .and_then(|index| index.accept_physical(key, value));
                if let Err(error) = result {
                    failure = Some(error);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .map_err(map_borrowed_ann_visit_error)?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !physical_stats.complete {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }

    let mut state = AnnState::default();
    for (index, streamed) in planned {
        if state.indexes.insert(index, streamed.finish()?).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    if require_complete {
        validate_catalog_coverage(catalog, &state)?;
    }
    Ok(state)
}

fn metadata_borrowed_visit_limits(memory_limit: u64) -> BorrowedVisitLimits {
    let maximum_bytes = memory_limit.min(crate::RECOVERY_MEMORY_BYTES);
    BorrowedVisitLimits {
        maximum_entries: usize::try_from(
            maximum_bytes / u64::try_from(ANN_INDEX_META_V1_SIZE).unwrap_or(1),
        )
        .unwrap_or(usize::MAX),
        maximum_bytes,
    }
}

fn aggregate_streaming_physical_limits<'a>(
    indexes: impl IntoIterator<Item = &'a StreamingIndexRestore>,
) -> Result<BorrowedVisitLimits, NativeRuntimeError> {
    indexes.into_iter().try_fold(
        BorrowedVisitLimits {
            maximum_entries: 0,
            maximum_bytes: 0,
        },
        |aggregate, index| {
            Ok(BorrowedVisitLimits {
                maximum_entries: aggregate
                    .maximum_entries
                    .checked_add(index.limits.total_entries())
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?,
                maximum_bytes: aggregate
                    .maximum_bytes
                    .checked_add(index.limits.total_bytes())
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?,
            })
        },
    )
}

fn map_borrowed_ann_visit_error(error: BorrowedVisitError) -> NativeRuntimeError {
    match error {
        BorrowedVisitError::Tree(error) => error.into(),
        BorrowedVisitError::Cancelled | BorrowedVisitError::LimitExceeded => {
            NativeRuntimeError::InvalidAnnTree
        }
    }
}

fn streamed_index_for_key<'a>(
    planned: &'a mut BTreeMap<ObjectId, StreamingIndexRestore>,
    key: &[u8],
) -> Result<&'a mut StreamingIndexRestore, NativeRuntimeError> {
    let encoded_index = key.get(1..17).ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let index = decode_index(encoded_index)?;
    planned
        .get_mut(&index)
        .ok_or(NativeRuntimeError::InvalidAnnTree)
}

#[derive(Default)]
struct AnnPhysicalRangeObservation {
    entries: usize,
    bytes: u64,
}

impl AnnPhysicalRangeObservation {
    fn admit(
        &mut self,
        limit: AnnPhysicalRangeLimit,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), NativeRuntimeError> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let entry_bytes = u64::try_from(key.len().saturating_add(value.len()))
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        let bytes = self
            .bytes
            .checked_add(entry_bytes)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if entries > limit.entries || bytes > limit.bytes {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        self.entries = entries;
        self.bytes = bytes;
        Ok(())
    }
}

struct StreamingIndexRestore {
    definition: VectorIndexDefinition,
    metadata: PersistedIndexMetadata,
    limits: AnnPhysicalLimits,
    children: BTreeMap<[u8; 32], RestoredChildEntries>,
    legacy_deltas: BTreeMap<ObjectId, DeltaRecord>,
    overlay_manifest: Option<OverlayManifest>,
    overlay_deltas: BTreeMap<ObjectId, DeltaRecord>,
    overlay_nodes: StreamingOverlayNodes,
    vector_observation: AnnPhysicalRangeObservation,
    graph_observation: AnnPhysicalRangeObservation,
    legacy_observation: AnnPhysicalRangeObservation,
    manifest_observation: AnnPhysicalRangeObservation,
    overlay_observation: AnnPhysicalRangeObservation,
    node_observation: AnnPhysicalRangeObservation,
}

impl StreamingIndexRestore {
    fn new(
        definition: VectorIndexDefinition,
        metadata: PersistedIndexMetadata,
        limits: AnnPhysicalLimits,
    ) -> Self {
        Self {
            definition,
            overlay_nodes: StreamingOverlayNodes::new(metadata.overlay),
            metadata,
            limits,
            children: BTreeMap::new(),
            legacy_deltas: BTreeMap::new(),
            overlay_manifest: None,
            overlay_deltas: BTreeMap::new(),
            vector_observation: AnnPhysicalRangeObservation::default(),
            graph_observation: AnnPhysicalRangeObservation::default(),
            legacy_observation: AnnPhysicalRangeObservation::default(),
            manifest_observation: AnnPhysicalRangeObservation::default(),
            overlay_observation: AnnPhysicalRangeObservation::default(),
            node_observation: AnnPhysicalRangeObservation::default(),
        }
    }

    fn accept_physical(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        #[cfg(test)]
        ANN_FULL_STREAM_PHYSICAL_VISITS
            .set(ANN_FULL_STREAM_PHYSICAL_VISITS.get().saturating_add(1));
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => self.accept_vector(key, value),
            Some(ANN_GRAPH_LAYER_PREFIX) => self.accept_graph_layer(key, value),
            Some(ANN_DELTA_PREFIX) => self.accept_legacy_delta(key, value),
            Some(ANN_OVERLAY_MANIFEST_PREFIX) => self.accept_manifest(key, value),
            Some(ANN_OVERLAY_DELTA_PREFIX) => self.accept_overlay_delta(key, value),
            Some(ANN_OVERLAY_NODE_PREFIX) => self.accept_overlay_node(key, value),
            _ => Err(NativeRuntimeError::InvalidAnnTree),
        }
    }

    fn accept_vector(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.vector_observation
            .admit(self.limits.vectors, key, value)?;
        let expected = ANN_VECTOR_HEADER_SIZE
            .checked_add(usize::from(self.definition.dimension()).saturating_mul(4))
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if value.len() != expected {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let (index, build_identity, object_id) = decode_vector_key(key)?;
        if index != self.definition.index_id()
            || !self.metadata.owns_physical_identity(build_identity)
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let child = self.children.entry(build_identity).or_default();
        if !child.vector_ids.insert(object_id) {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        child
            .vectors
            .push(decode_vector_record(value, object_id, self.definition)?);
        Ok(())
    }

    fn accept_graph_layer(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.graph_observation
            .admit(self.limits.graph_layers, key, value)?;
        let maximum = ANN_GRAPH_LAYER_HEADER_SIZE
            .checked_add(
                usize::from(self.definition.config().m())
                    .saturating_mul(2)
                    .saturating_mul(16),
            )
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if value.len() > maximum {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let (index, build_identity, object_id, layer) = decode_graph_layer_key(key)?;
        if index != self.definition.index_id()
            || !self.metadata.owns_physical_identity(build_identity)
            || self
                .children
                .entry(build_identity)
                .or_default()
                .layers
                .entry(object_id)
                .or_default()
                .insert(layer, decode_graph_layer(value)?)
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(())
    }

    fn accept_legacy_delta(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.legacy_observation
            .admit(self.limits.deltas, key, value)?;
        validate_delta_size_before_decode(value, self.definition, *ANN_DELTA_MAGIC)?;
        let (index, object_id) = decode_delta_key(key)?;
        if index != self.definition.index_id()
            || self
                .legacy_deltas
                .insert(object_id, decode_delta(value, object_id, self.definition)?)
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(())
    }

    fn accept_manifest(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.manifest_observation
            .admit(self.limits.overlay_manifest, key, value)?;
        if self.metadata.overlay.is_none()
            || decode_overlay_manifest_key(key)? != self.definition.index_id()
            || self
                .overlay_manifest
                .replace(decode_overlay_manifest(value)?)
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(())
    }

    fn accept_overlay_delta(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.overlay_observation
            .admit(self.limits.overlay_deltas, key, value)?;
        validate_delta_size_before_decode(value, self.definition, *ANN_OVERLAY_DELTA_MAGIC)?;
        let (index, object_id) = decode_overlay_delta_key(key)?;
        if self.metadata.overlay.is_none() || index != self.definition.index_id() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let leaf_hash = overlay_leaf_hash(object_id, value);
        #[cfg(test)]
        ANN_FULL_STREAM_OVERLAY_DECODES
            .set(ANN_FULL_STREAM_OVERLAY_DECODES.get().saturating_add(1));
        if self
            .overlay_deltas
            .insert(
                object_id,
                decode_overlay_delta(value, object_id, self.definition)?,
            )
            .is_some()
            || self
                .overlay_nodes
                .insert_leaf(object_id.get(), leaf_hash)
                .is_err()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(())
    }

    fn accept_overlay_node(&mut self, key: &[u8], value: &[u8]) -> Result<(), NativeRuntimeError> {
        self.node_observation
            .admit(self.limits.overlay_nodes, key, value)?;
        if value.len()
            > ANN_OVERLAY_NODE_HEADER_SIZE.saturating_add(ANN_OVERLAY_FANOUT.saturating_mul(32))
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let (index, depth, path) = decode_overlay_node_key(key)?;
        if self.metadata.overlay.is_none() || index != self.definition.index_id() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        #[cfg(test)]
        ANN_FULL_STREAM_NODE_DECODES.set(ANN_FULL_STREAM_NODE_DECODES.get().saturating_add(1));
        self.overlay_nodes.accept(depth, path, value)
    }

    fn finish(mut self) -> Result<AnnIndexState, NativeRuntimeError> {
        self.overlay_nodes.finish()?;
        enrich_streamed_retained_generations(&mut self.metadata, self.definition, &self.children)?;
        validate_streamed_retained_generations(&self.metadata, &self.children)?;
        let current_identities = self.metadata.current_child_identities();
        let mut selected = BTreeMap::new();
        for identity in current_identities {
            if let Some(child) = self.children.remove(&identity) {
                selected.insert(identity, child);
            }
        }
        let base = restore_base_with_cancellation(&self.metadata, self.definition, selected, None)?;
        let deltas = if let Some(overlay) = self.metadata.overlay {
            validate_layered_deltas(
                &self.metadata,
                overlay,
                self.overlay_manifest
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?,
                self.legacy_deltas,
                self.overlay_deltas,
            )?
        } else {
            if self.overlay_manifest.is_some() || !self.overlay_deltas.is_empty() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            validate_restored_deltas(&self.metadata, &self.legacy_deltas)?;
            self.legacy_deltas
        };
        let mut restored = AnnIndexState {
            base,
            deltas,
            next_sequence: self.metadata.next_sequence,
            view_identity: self.metadata.view_identity,
            lifecycle: self.metadata.lifecycle,
            retained_generations: self.metadata.retained_generations,
            persisted_version: self.metadata.version,
        };
        restored.validate_delta_bounds()?;
        let maximum_sequence = restored
            .deltas
            .values()
            .map(DeltaRecord::sequence)
            .max()
            .unwrap_or(0);
        if restored.next_sequence == 0 || restored.next_sequence <= maximum_sequence {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        if self.metadata.version == 1 {
            restored.view_identity = restored.base.build_identity();
        } else if self.metadata.version != 5
            && restored.view_identity
                != calculate_view_identity(
                    restored.base.build_identity(),
                    restored.next_sequence,
                    &restored.deltas,
                )
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(restored)
    }
}

fn validate_delta_size_before_decode(
    encoded: &[u8],
    definition: VectorIndexDefinition,
    magic: [u8; 8],
) -> Result<(), NativeRuntimeError> {
    if encoded.len() < ANN_DELTA_HEADER_SIZE || encoded.get(..8) != Some(magic.as_slice()) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let expected = match encoded[8] {
        ANN_DELTA_UPSERT => ANN_DELTA_HEADER_SIZE
            .checked_add(usize::from(definition.dimension()).saturating_mul(4))
            .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        ANN_DELTA_TOMBSTONE => ANN_DELTA_HEADER_SIZE,
        _ => return Err(NativeRuntimeError::InvalidAnnTree),
    };
    if encoded.len() != expected {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

struct StreamingOverlayNodes {
    overlay: Option<PersistedOverlayMetadata>,
    leaves: BTreeMap<u128, [u8; 32]>,
    current_depth: Option<u8>,
    expected: BTreeMap<u128, [u8; 32]>,
    next_expected: BTreeMap<u128, [u8; 32]>,
    observed_nodes: u64,
}

impl StreamingOverlayNodes {
    fn new(overlay: Option<PersistedOverlayMetadata>) -> Self {
        Self {
            overlay,
            leaves: BTreeMap::new(),
            current_depth: None,
            expected: BTreeMap::new(),
            next_expected: BTreeMap::new(),
            observed_nodes: 0,
        }
    }

    fn insert_leaf(&mut self, path: u128, hash: [u8; 32]) -> Result<(), NativeRuntimeError> {
        if self.current_depth.is_some() || self.leaves.insert(path, hash).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        #[cfg(test)]
        ANN_FULL_STREAM_PEAK_FRONTIER
            .set(ANN_FULL_STREAM_PEAK_FRONTIER.get().max(self.leaves.len()));
        Ok(())
    }

    fn accept(&mut self, depth: u8, path: u128, value: &[u8]) -> Result<(), NativeRuntimeError> {
        let overlay = self.overlay.ok_or(NativeRuntimeError::InvalidAnnTree)?;
        self.observed_nodes = self
            .observed_nodes
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if self.observed_nodes > overlay.overlay_node_count || overlay.overlay_count == 0 {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        if self.current_depth.is_none() {
            if u64::try_from(self.leaves.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                != overlay.overlay_count
            {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            self.current_depth = Some(0);
            self.expected.insert(0, overlay.overlay_root);
        }
        let current_depth = self
            .current_depth
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if depth != current_depth {
            if depth != current_depth.saturating_add(1) || !self.expected.is_empty() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            self.expected = std::mem::take(&mut self.next_expected);
            self.current_depth = Some(depth);
        }
        let expected_hash = self
            .expected
            .remove(&path)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let node = decode_overlay_node(value, depth)?;
        if node.node_hash != expected_hash {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let shift = u32::from(ANN_OVERLAY_TREE_DEPTH - depth - 1) * 4;
        let mut child_hashes = node.child_hashes.iter();
        for position in 0..ANN_OVERLAY_FANOUT {
            if node.bitmap & (1_u16 << position) == 0 {
                continue;
            }
            let child_hash = *child_hashes
                .next()
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            let child_path = path
                | (u128::try_from(position).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                    << shift);
            if depth + 1 == ANN_OVERLAY_TREE_DEPTH {
                if self.leaves.remove(&child_path) != Some(child_hash) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            } else if self.next_expected.insert(child_path, child_hash).is_some() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            #[cfg(test)]
            ANN_FULL_STREAM_PEAK_FRONTIER.set(
                ANN_FULL_STREAM_PEAK_FRONTIER
                    .get()
                    .max(self.next_expected.len()),
            );
        }
        if child_hashes.next().is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(())
    }

    fn finish(self) -> Result<(), NativeRuntimeError> {
        let Some(overlay) = self.overlay else {
            if self.current_depth.is_none() && self.leaves.is_empty() && self.observed_nodes == 0 {
                return Ok(());
            }
            return Err(NativeRuntimeError::InvalidAnnTree);
        };
        let valid_empty = overlay.overlay_count == 0
            && self.current_depth.is_none()
            && self.leaves.is_empty()
            && self.observed_nodes == 0
            && overlay.overlay_node_count == 0
            && overlay.overlay_root == overlay_empty_hash(0);
        let valid_nonempty = overlay.overlay_count != 0
            && self.current_depth == Some(ANN_OVERLAY_TREE_DEPTH - 1)
            && self.expected.is_empty()
            && self.next_expected.is_empty()
            && self.leaves.is_empty()
            && self.observed_nodes == overlay.overlay_node_count;
        if valid_empty || valid_nonempty {
            Ok(())
        } else {
            Err(NativeRuntimeError::InvalidAnnTree)
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_full_stream_observation_for_test() {
    ANN_FULL_STREAM_PHYSICAL_VISITS.set(0);
    ANN_FULL_STREAM_NODE_DECODES.set(0);
    ANN_FULL_STREAM_OVERLAY_DECODES.set(0);
    ANN_FULL_STREAM_PEAK_FRONTIER.set(0);
}

#[cfg(test)]
pub(crate) fn full_stream_observation_for_test() -> (usize, usize, usize) {
    (
        ANN_FULL_STREAM_PHYSICAL_VISITS.get(),
        ANN_FULL_STREAM_NODE_DECODES.get(),
        ANN_FULL_STREAM_PEAK_FRONTIER.get(),
    )
}

#[cfg(test)]
pub(crate) fn full_stream_overlay_decodes_for_test() -> usize {
    ANN_FULL_STREAM_OVERLAY_DECODES.get()
}

fn enrich_streamed_retained_generations(
    metadata: &mut PersistedIndexMetadata,
    definition: VectorIndexDefinition,
    children: &BTreeMap<[u8; 32], RestoredChildEntries>,
) -> Result<(), NativeRuntimeError> {
    for child in metadata
        .retained_generations
        .iter_mut()
        .flat_map(|generation| &mut generation.children)
        .filter(|child| !child.complete)
    {
        let entries = children
            .get(&child.build_identity)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?
            .clone();
        let descriptor = infer_legacy_retained_descriptor(child.build_identity, &entries)?;
        let snapshot = restore_child_snapshot(definition, &descriptor, entries)?;
        let restored =
            HnswIndex::restore_owned(snapshot).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        if restored.build_identity() != child.build_identity {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        *child = descriptor;
    }
    Ok(())
}

fn validate_streamed_retained_generations(
    metadata: &PersistedIndexMetadata,
    children: &BTreeMap<[u8; 32], RestoredChildEntries>,
) -> Result<(), NativeRuntimeError> {
    for child in metadata
        .retained_generations
        .iter()
        .flat_map(|generation| &generation.children)
    {
        let entries = children
            .get(&child.build_identity)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let summary = PhysicalGenerationSummary {
            vector_ids: entries.vector_ids.clone(),
            graph_layers: entries
                .layers
                .iter()
                .map(|(object_id, layers)| (*object_id, layers.keys().copied().collect()))
                .collect(),
        };
        validate_retained_child_entries(child, &summary)?;
    }
    Ok(())
}

fn decode_metadata_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Result<BTreeMap<ObjectId, PersistedIndexMetadata>, NativeRuntimeError> {
    let mut metadata = BTreeMap::new();
    for (key, value) in entries {
        if key.first() == Some(&ANN_INDEX_META_PREFIX) {
            let index = decode_meta_key(key)?;
            if metadata.insert(index, decode_metadata(value)?).is_some() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
        }
    }
    Ok(metadata)
}

fn enrich_legacy_retained_generations(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    metadata: &mut BTreeMap<ObjectId, PersistedIndexMetadata>,
) -> Result<(), NativeRuntimeError> {
    let has_incomplete_children = metadata.values().any(|persisted| {
        persisted
            .retained_generations
            .iter()
            .flat_map(|generation| &generation.children)
            .any(|child| !child.complete)
    });
    if !has_incomplete_children {
        return Ok(());
    }
    let physical_entries = PhysicalEntryIndex::build(entries)?;
    for (index, persisted) in metadata {
        let definition = catalog_ann_definition(catalog, *index)?;
        for child in persisted
            .retained_generations
            .iter_mut()
            .flat_map(|generation| &mut generation.children)
            .filter(|child| !child.complete)
        {
            *child = restore_legacy_retained_descriptor(
                entries,
                &physical_entries,
                *index,
                definition,
                child.build_identity,
            )?;
        }
    }
    Ok(())
}

fn enrich_target_legacy_retained_generations(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: &mut PersistedIndexMetadata,
) -> Result<(), NativeRuntimeError> {
    if !metadata
        .retained_generations
        .iter()
        .flat_map(|generation| &generation.children)
        .any(|child| !child.complete)
    {
        return Ok(());
    }
    let physical_entries = PhysicalEntryIndex::build(entries)?;
    for child in metadata
        .retained_generations
        .iter_mut()
        .flat_map(|generation| &mut generation.children)
        .filter(|child| !child.complete)
    {
        *child = restore_legacy_retained_descriptor(
            entries,
            &physical_entries,
            index,
            definition,
            child.build_identity,
        )?;
    }
    Ok(())
}

fn restore_legacy_retained_descriptor(
    entries: &[(Vec<u8>, Vec<u8>)],
    physical_entries: &PhysicalEntryIndex,
    index: ObjectId,
    definition: VectorIndexDefinition,
    build_identity: [u8; 32],
) -> Result<PersistedChildDescriptor, NativeRuntimeError> {
    let mut child = physical_entries.restore_child(entries, index, definition, build_identity)?;
    child.vectors.sort_by_key(|record| record.object_id);
    let descriptor = infer_legacy_retained_descriptor(build_identity, &child)?;
    let snapshot = restore_child_snapshot(definition, &descriptor, child)?;
    let restored =
        HnswIndex::restore_owned(snapshot).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if restored.build_identity() != build_identity {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(descriptor)
}

fn infer_legacy_retained_descriptor(
    build_identity: [u8; 32],
    child: &RestoredChildEntries,
) -> Result<PersistedChildDescriptor, NativeRuntimeError> {
    if child.vectors.is_empty()
        || child.vector_ids != child.layers.keys().copied().collect()
        || child.layers.values().any(|layers| {
            layers
                .keys()
                .next_back()
                .is_none_or(|maximum| layers.keys().copied().ne(0..=*maximum))
        })
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let levels = child
        .layers
        .iter()
        .map(|(object_id, layers)| {
            layers
                .keys()
                .next_back()
                .copied()
                .map(|level| (*object_id, level))
                .ok_or(NativeRuntimeError::InvalidAnnTree)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut entry_point = None;
    let mut max_level = 0_u16;
    let mut order = child
        .vectors
        .iter()
        .map(|record| (record.creating_csn, record.object_id))
        .collect::<Vec<_>>();
    order.sort_by_key(|(creating_csn, object_id)| (creating_csn.get(), object_id.get()));
    for (_, object_id) in order {
        let level = *levels
            .get(&object_id)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if entry_point.is_none() || level > max_level {
            entry_point = Some(object_id);
            max_level = level;
        }
    }
    Ok(PersistedChildDescriptor {
        build_identity,
        vector_count: u64::try_from(child.vectors.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        graph_node_count: u64::try_from(child.layers.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        entry_point,
        max_level,
        complete: true,
    })
}

fn restore_index(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    index: ObjectId,
    metadata: PersistedIndexMetadata,
) -> Result<AnnIndexState, NativeRuntimeError> {
    let definition = catalog_ann_definition(catalog, index)?;
    restore_index_with_definition(entries, index, definition, metadata)
}

fn restore_index_with_definition(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: PersistedIndexMetadata,
) -> Result<AnnIndexState, NativeRuntimeError> {
    restore_index_with_definition_controlled(entries, index, definition, metadata, None, false)
}

#[allow(clippy::too_many_lines)]
fn restore_index_with_definition_controlled(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: PersistedIndexMetadata,
    cancellation: Option<&GovernorCancellation>,
    overlay_nodes_prevalidated: bool,
) -> Result<AnnIndexState, NativeRuntimeError> {
    let delta_prefix = object_prefix(ANN_DELTA_PREFIX, index);
    let current_identities = metadata.current_child_identities();
    let retained_identities = metadata.retained_child_identities();
    let mut children = BTreeMap::<[u8; 32], RestoredChildEntries>::new();
    let mut deltas = BTreeMap::new();
    let mut overlay_manifest = None;
    let mut overlay_deltas = BTreeMap::new();
    let mut overlay_leaf_hashes = BTreeMap::new();
    let mut has_overlay_nodes = false;
    for (key, value) in entries {
        reject_cancelled_ann_search(cancellation)?;
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (found_index, build_identity, object_id) = decode_vector_key(key)?;
                if found_index == index && current_identities.contains(&build_identity) {
                    let child = children.entry(build_identity).or_default();
                    if !child.vector_ids.insert(object_id) {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                    child
                        .vectors
                        .push(decode_vector_record(value, object_id, definition)?);
                } else if found_index == index && !retained_identities.contains(&build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (found_index, build_identity, object_id, layer) = decode_graph_layer_key(key)?;
                if found_index == index && current_identities.contains(&build_identity) {
                    if children
                        .entry(build_identity)
                        .or_default()
                        .layers
                        .entry(object_id)
                        .or_default()
                        .insert(layer, decode_graph_layer(value)?)
                        .is_some()
                    {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                } else if found_index == index && !retained_identities.contains(&build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_DELTA_PREFIX) => {
                let (found_index, object_id) = decode_delta_key(key)?;
                if found_index == index
                    && (!key.starts_with(&delta_prefix)
                        || deltas
                            .insert(object_id, decode_delta(value, object_id, definition)?)
                            .is_some())
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_OVERLAY_MANIFEST_PREFIX) => {
                let found_index = decode_overlay_manifest_key(key)?;
                if found_index == index
                    && overlay_manifest
                        .replace(decode_overlay_manifest(value)?)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_OVERLAY_DELTA_PREFIX) => {
                let (found_index, object_id) = decode_overlay_delta_key(key)?;
                if found_index == index {
                    let delta = decode_overlay_delta(value, object_id, definition)?;
                    if overlay_deltas.insert(object_id, delta).is_some()
                        || overlay_leaf_hashes
                            .insert(object_id.get(), overlay_leaf_hash(object_id, value))
                            .is_some()
                    {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                }
            }
            Some(ANN_OVERLAY_NODE_PREFIX) => {
                let (found_index, depth, _) = decode_overlay_node_key(key)?;
                if found_index == index {
                    has_overlay_nodes = true;
                    if !overlay_nodes_prevalidated {
                        decode_overlay_node(value, depth)?;
                    }
                }
            }
            _ => {}
        }
    }
    let deltas = if let Some(overlay) = metadata.overlay {
        let manifest = overlay_manifest.ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if !overlay_nodes_prevalidated {
            validate_overlay_nodes_in_entries(entries, index, &overlay_leaf_hashes, overlay)?;
        }
        validate_layered_deltas(&metadata, overlay, manifest, deltas, overlay_deltas)?
    } else {
        if overlay_manifest.is_some() || !overlay_deltas.is_empty() || has_overlay_nodes {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        validate_restored_deltas(&metadata, &deltas)?;
        deltas
    };
    let base = restore_base_with_cancellation(&metadata, definition, children, cancellation)?;
    let mut restored = AnnIndexState {
        base,
        deltas,
        next_sequence: metadata.next_sequence,
        view_identity: metadata.view_identity,
        lifecycle: metadata.lifecycle,
        retained_generations: metadata.retained_generations,
        persisted_version: metadata.version,
    };
    restored.validate_delta_bounds()?;
    let max_sequence = restored
        .deltas
        .values()
        .map(DeltaRecord::sequence)
        .max()
        .unwrap_or(0);
    if restored.next_sequence == 0 || restored.next_sequence <= max_sequence {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    if metadata.version == 1 {
        restored.view_identity = restored.base.build_identity();
    } else if metadata.version != 5
        && restored.view_identity
            != calculate_view_identity(
                restored.base.build_identity(),
                restored.next_sequence,
                &restored.deltas,
            )
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(restored)
}

fn validate_layered_deltas(
    metadata: &PersistedIndexMetadata,
    overlay: PersistedOverlayMetadata,
    manifest: OverlayManifest,
    legacy_deltas: BTreeMap<ObjectId, DeltaRecord>,
    overlay_deltas: BTreeMap<ObjectId, DeltaRecord>,
) -> Result<BTreeMap<ObjectId, DeltaRecord>, NativeRuntimeError> {
    let expected_manifest = OverlayManifest {
        legacy_view_identity: overlay.legacy_view_identity,
        overlay_root: overlay.overlay_root,
        view_identity: metadata.view_identity,
        legacy_count: metadata.delta_count,
        legacy_bytes: metadata.delta_bytes,
        overlay_count: overlay.overlay_count,
        overlay_bytes: overlay.overlay_bytes,
        overlay_node_count: overlay.overlay_node_count,
        effective_count: overlay.effective_count,
        effective_bytes: overlay.effective_bytes,
        legacy_next_sequence: overlay.legacy_next_sequence,
        next_sequence: metadata.next_sequence,
    };
    if manifest != expected_manifest {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_restored_deltas(metadata, &legacy_deltas)?;
    let overlay_count =
        u64::try_from(overlay_deltas.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let overlay_bytes = delta_map_bytes(&overlay_deltas)?;
    if overlay_count != overlay.overlay_count || overlay_bytes != overlay.overlay_bytes {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    if calculate_view_identity(
        metadata.build_identity,
        overlay.legacy_next_sequence,
        &legacy_deltas,
    ) != overlay.legacy_view_identity
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut sequences = BTreeSet::new();
    if legacy_deltas.values().any(|delta| {
        delta.sequence() >= overlay.legacy_next_sequence || !sequences.insert(delta.sequence())
    }) || overlay_deltas.values().any(|delta| {
        delta.sequence() < overlay.legacy_next_sequence
            || delta.sequence() >= metadata.next_sequence
            || !sequences.insert(delta.sequence())
    }) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut effective = legacy_deltas;
    for (object_id, delta) in overlay_deltas {
        effective.insert(object_id, delta);
    }
    if u64::try_from(effective.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
        != overlay.effective_count
        || delta_map_bytes(&effective)? != overlay.effective_bytes
        || overlay_view_identity(metadata.build_identity, manifest) != metadata.view_identity
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(effective)
}

fn delta_map_bytes(deltas: &BTreeMap<ObjectId, DeltaRecord>) -> Result<u64, NativeRuntimeError> {
    deltas.values().try_fold(0_u64, |bytes, delta| {
        bytes
            .checked_add(
                u64::try_from(delta.encoded_len())
                    .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            )
            .ok_or(NativeRuntimeError::InvalidAnnTree)
    })
}

fn validate_overlay_nodes_in_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    leaf_hashes: &BTreeMap<u128, [u8; 32]>,
    overlay: PersistedOverlayMetadata,
) -> Result<(), NativeRuntimeError> {
    let mut frontier = leaf_hashes.clone();
    let mut node_count = 0_u64;
    if frontier.is_empty() {
        if entries.iter().any(|(key, _)| {
            key.first() == Some(&ANN_OVERLAY_NODE_PREFIX)
                && decode_overlay_node_key(key).is_ok_and(|(found, _, _)| found == index)
        }) || overlay.overlay_node_count != 0
            || overlay.overlay_root != overlay_empty_hash(0)
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        return Ok(());
    }
    for depth in (0..ANN_OVERLAY_TREE_DEPTH).rev() {
        let (parents, expected) = expected_overlay_level(&frontier, depth)?;
        let mut observed = entries
            .iter()
            .filter(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
            .filter_map(|(key, value)| match decode_overlay_node_key(key) {
                Ok((found, found_depth, path)) if found == index && found_depth == depth => {
                    Some(Ok((path, value)))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            });
        for expected_node in &expected {
            let (path, value) = observed
                .next()
                .ok_or(NativeRuntimeError::InvalidAnnTree)??;
            if path != expected_node.path
                || decode_overlay_node(value, depth)? != expected_node.node
            {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
        }
        if observed.next().is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        node_count = node_count
            .checked_add(
                u64::try_from(expected.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            )
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        frontier = parents;
    }
    if frontier.len() != 1
        || frontier.get(&0) != Some(&overlay.overlay_root)
        || node_count != overlay.overlay_node_count
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

#[derive(Clone, Default)]
struct RestoredChildEntries {
    vectors: Vec<VectorRecord>,
    vector_ids: BTreeSet<ObjectId>,
    layers: BTreeMap<ObjectId, BTreeMap<u16, Vec<ObjectId>>>,
}

#[derive(Default)]
struct PhysicalChildPositions {
    vectors: Option<PhysicalEntrySpan>,
    graph_layers: Option<PhysicalEntrySpan>,
}

#[derive(Clone, Copy)]
struct PhysicalEntrySpan {
    start: usize,
    end: usize,
}

impl PhysicalEntrySpan {
    fn extend(span: &mut Option<Self>, position: usize) -> Result<(), NativeRuntimeError> {
        let end = position
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        match span {
            Some(current) if current.end == position => current.end = end,
            Some(_) => return Err(NativeRuntimeError::InvalidAnnTree),
            None => {
                *span = Some(Self {
                    start: position,
                    end,
                });
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct PhysicalEntryIndex {
    children: BTreeMap<(ObjectId, [u8; 32]), PhysicalChildPositions>,
    indexes_with_deltas: BTreeSet<ObjectId>,
    #[cfg(test)]
    source_entry_visits: usize,
}

impl PhysicalEntryIndex {
    fn build(entries: &[(Vec<u8>, Vec<u8>)]) -> Result<Self, NativeRuntimeError> {
        let mut physical = Self::default();
        for (position, (key, _)) in entries.iter().enumerate() {
            #[cfg(test)]
            {
                physical.source_entry_visits = physical.source_entry_visits.saturating_add(1);
            }
            match key.first().copied() {
                Some(ANN_VECTOR_PREFIX) => {
                    let (index, build_identity, _) = decode_vector_key(key)?;
                    PhysicalEntrySpan::extend(
                        &mut physical
                            .children
                            .entry((index, build_identity))
                            .or_default()
                            .vectors,
                        position,
                    )?;
                }
                Some(ANN_GRAPH_LAYER_PREFIX) => {
                    let (index, build_identity, _, _) = decode_graph_layer_key(key)?;
                    PhysicalEntrySpan::extend(
                        &mut physical
                            .children
                            .entry((index, build_identity))
                            .or_default()
                            .graph_layers,
                        position,
                    )?;
                }
                Some(ANN_DELTA_PREFIX) => {
                    physical
                        .indexes_with_deltas
                        .insert(decode_delta_key(key)?.0);
                }
                _ => {}
            }
        }
        Ok(physical)
    }

    fn has_unselected_child(&self, index: ObjectId, selected: &BTreeSet<[u8; 32]>) -> bool {
        self.children
            .keys()
            .any(|(found_index, identity)| *found_index == index && !selected.contains(identity))
    }

    fn has_deltas(&self, index: ObjectId) -> bool {
        self.indexes_with_deltas.contains(&index)
    }

    fn restore_child(
        &self,
        entries: &[(Vec<u8>, Vec<u8>)],
        index: ObjectId,
        definition: VectorIndexDefinition,
        build_identity: [u8; 32],
    ) -> Result<RestoredChildEntries, NativeRuntimeError> {
        let mut child = RestoredChildEntries::default();
        let Some(positions) = self.children.get(&(index, build_identity)) else {
            return Ok(child);
        };
        let vector_positions = positions.vectors.map_or(0..0, |span| span.start..span.end);
        for position in vector_positions {
            let (key, value) = entries
                .get(position)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            let (found_index, found_identity, object_id) = decode_vector_key(key)?;
            if found_index != index
                || found_identity != build_identity
                || !child.vector_ids.insert(object_id)
            {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            child
                .vectors
                .push(decode_vector_record(value, object_id, definition)?);
        }
        let graph_positions = positions
            .graph_layers
            .map_or(0..0, |span| span.start..span.end);
        for position in graph_positions {
            let (key, value) = entries
                .get(position)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            let (found_index, found_identity, object_id, layer) = decode_graph_layer_key(key)?;
            if found_index != index
                || found_identity != build_identity
                || child
                    .layers
                    .entry(object_id)
                    .or_default()
                    .insert(layer, decode_graph_layer(value)?)
                    .is_some()
            {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
        }
        Ok(child)
    }
}

#[cfg(test)]
fn restore_base(
    metadata: &PersistedIndexMetadata,
    definition: VectorIndexDefinition,
    entries: BTreeMap<[u8; 32], RestoredChildEntries>,
) -> Result<AnnBase, NativeRuntimeError> {
    restore_base_with_cancellation(metadata, definition, entries, None)
}

fn restore_base_with_cancellation(
    metadata: &PersistedIndexMetadata,
    definition: VectorIndexDefinition,
    mut entries: BTreeMap<[u8; 32], RestoredChildEntries>,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnBase, NativeRuntimeError> {
    let mut snapshots = Vec::with_capacity(metadata.children.len());
    for descriptor in &metadata.children {
        reject_cancelled_ann_search(cancellation)?;
        let child = entries
            .remove(&descriptor.build_identity)
            .unwrap_or_default();
        snapshots.push(restore_child_snapshot(definition, descriptor, child)?);
    }
    if !entries.is_empty() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let base = match metadata.base_kind {
        PersistedBaseKind::Single => {
            let snapshot = snapshots.pop().ok_or(NativeRuntimeError::InvalidAnnTree)?;
            if !snapshots.is_empty() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            AnnBase::Single(
                HnswIndex::restore_owned_with_control(snapshot, || {
                    ann_restore_control(cancellation)
                })
                .map_err(|error| map_ann_restore_error(&error, cancellation))?,
            )
        }
        PersistedBaseKind::Partitioned => {
            let input_identity = metadata
                .input_identity
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            AnnBase::Partitioned(
                PartitionedHnswIndex::restore_snapshot_with_control(
                    PartitionedIndexSnapshot {
                        definition,
                        input_identity,
                        build_identity: metadata.build_identity,
                        partitions: snapshots,
                    },
                    || ann_restore_control(cancellation),
                )
                .map_err(|error| map_ann_restore_error(&error, cancellation))?,
            )
        }
    };
    if base.build_identity() != metadata.build_identity
        || u64::try_from(base.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != metadata.vector_count
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(base)
}

fn ann_restore_control(cancellation: Option<&GovernorCancellation>) -> ControlFlow<()> {
    if cancellation.is_some_and(GovernorCancellation::is_cancelled) {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

fn map_ann_restore_error(
    error: &AnnError,
    cancellation: Option<&GovernorCancellation>,
) -> NativeRuntimeError {
    if *error == AnnError::BuildCancelled
        && cancellation.is_some_and(GovernorCancellation::is_cancelled)
    {
        GovernorQueueError::Cancelled.into()
    } else {
        NativeRuntimeError::InvalidAnnTree
    }
}

fn restore_child_snapshot(
    definition: VectorIndexDefinition,
    descriptor: &PersistedChildDescriptor,
    mut entries: RestoredChildEntries,
) -> Result<IndexSnapshot, NativeRuntimeError> {
    entries.vectors.sort_by_key(|record| record.object_id);
    if u64::try_from(entries.vectors.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
        != descriptor.vector_count
        || u64::try_from(entries.layers.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != descriptor.graph_node_count
        || entries.vector_ids != entries.layers.keys().copied().collect()
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let nodes = entries
        .layers
        .into_iter()
        .map(|(object_id, layers)| graph_node(object_id, layers))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IndexSnapshot {
        definition,
        vectors: entries.vectors,
        nodes,
        entry_point: descriptor.entry_point,
        max_level: descriptor.max_level,
        build_identity: descriptor.build_identity,
    })
}

fn validate_restored_deltas(
    metadata: &PersistedIndexMetadata,
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
) -> Result<(), NativeRuntimeError> {
    if u64::try_from(deltas.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
        != metadata.delta_count
        || u64::try_from(deltas.values().map(DeltaRecord::encoded_len).sum::<usize>())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != metadata.delta_bytes
        || metadata.version == 1 && !deltas.is_empty()
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn validate_delta_mutation_bounds(
    metadata: &PersistedIndexMetadata,
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
) -> Result<(), NativeRuntimeError> {
    if deltas.len() > usize::try_from(metadata.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
        || deltas.values().map(DeltaRecord::encoded_len).sum::<usize>() > MAX_ANN_DELTA_BYTES
    {
        Err(NativeRuntimeError::AnnDeltaLimitExceeded)
    } else {
        Ok(())
    }
}

pub(crate) fn plan_consolidation(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    load_plan: &AnnIndexLoadPlan,
    max_vectors: usize,
    max_delta_records: usize,
    execution: ConsolidationBuildExecution<'_>,
) -> Result<ConsolidationPlan, NativeRuntimeError> {
    if max_vectors == 0
        || max_vectors > MAX_ANN_CONSOLIDATION_VECTORS
        || max_delta_records == 0
        || max_delta_records > MAX_ANN_DELTA_RECORDS
    {
        return Err(NativeRuntimeError::InvalidAnnConsolidationLimit);
    }
    reject_cancelled_ann_search(execution.cancellation)?;
    let current = load_planned_index(pages, buffer_pool, load_plan, execution.cancellation)?;
    if current.persisted_version == 5 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    if max_delta_records
        > usize::try_from(current.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
    {
        return Err(NativeRuntimeError::InvalidAnnConsolidationLimit);
    }
    if current.deltas.is_empty() {
        return Err(NativeRuntimeError::AnnConsolidationNotNeeded);
    }
    let effective_vector_count = current.effective_vector_count();
    if effective_vector_count > max_vectors || current.deltas.len() > max_delta_records {
        return Err(NativeRuntimeError::AnnConsolidationLimitExceeded);
    }
    let vectors = current.effective_vectors_with_cancellation(execution.cancellation)?;
    if vectors.len() != effective_vector_count {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let selected_children = current.base.child_descriptors().len();
    let partition_count = consolidation_replacement_partitions(
        current.base.is_partitioned(),
        selected_children,
        effective_vector_count,
    );
    let maximum_partitions = maximum_consolidation_replacement_partitions(
        selected_children,
        current
            .retained_generations
            .iter()
            .map(|generation| generation.children.len()),
        current.lifecycle.retain_generations,
        current.base.len() == 0,
    );
    if partition_count > maximum_partitions {
        return Err(NativeRuntimeError::AnnConsolidationLimitExceeded);
    }
    reject_cancelled_ann_search(execution.cancellation)?;
    let replacement = if current.base.is_partitioned() && !vectors.is_empty() {
        ConsolidationReplacement::Partitioned(build_partitioned_consolidation(
            current.definition(),
            vectors,
            partition_count,
            execution.pool,
            execution.permit,
            execution.cancellation,
        )?)
    } else {
        let cancellation = execution.cancellation.cloned();
        ConsolidationReplacement::Single(
            HnswIndex::build_owned_cancellable(current.definition(), vectors, move || {
                if cancellation
                    .as_ref()
                    .is_some_and(GovernorCancellation::is_cancelled)
                {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?
            .into_snapshot(),
        )
    };
    Ok(ConsolidationPlan {
        index: load_plan.index,
        base_identity: current.base.build_identity(),
        captured_view_identity: current.view_identity,
        captured_deltas: current
            .deltas
            .iter()
            .map(|(object_id, delta)| (*object_id, delta.sequence()))
            .collect(),
        replacement: Arc::new(replacement),
    })
}

fn build_partitioned_consolidation(
    definition: VectorIndexDefinition,
    vectors: Vec<VectorRecord>,
    partition_count: usize,
    execution_pool: Option<&NativeExecutionPool>,
    permit: Option<&OwnedGovernorPermit>,
    cancellation: Option<&GovernorCancellation>,
) -> Result<PartitionedIndexSnapshot, NativeRuntimeError> {
    let cancellation_for_plan = cancellation.cloned();
    let plan =
        HnswPartitionPlan::build_cancellable(definition, vectors, partition_count, move || {
            if cancellation_for_plan
                .as_ref()
                .is_some_and(GovernorCancellation::is_cancelled)
            {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
    let input_identity = plan.input_identity();
    let partitions = plan.into_partitions();
    let children = if let (Some(execution_pool), Some(permit)) = (execution_pool, permit)
        && permit.request().compute_threads > 1
        && partitions.len() > 1
    {
        let cancellation = cancellation.cloned();
        let (children, _) =
            execution_pool.execute_ordered_profiled(permit, partitions, move |partition| {
                let cancellation = cancellation.clone();
                HnswIndex::build_owned_cancellable(definition, partition, move || {
                    if cancellation
                        .as_ref()
                        .is_some_and(GovernorCancellation::is_cancelled)
                    {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
            })?;
        children.into_iter().collect::<Result<Vec<_>, _>>()?
    } else {
        partitions
            .into_iter()
            .map(|partition| {
                let cancellation = cancellation.cloned();
                HnswIndex::build_owned_cancellable(definition, partition, move || {
                    if cancellation
                        .as_ref()
                        .is_some_and(GovernorCancellation::is_cancelled)
                    {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let cancellation_for_assembly = cancellation.cloned();
    Ok(PartitionedHnswIndex::from_governed_partitions_cancellable(
        definition,
        input_identity,
        children,
        move || {
            if cancellation_for_assembly
                .as_ref()
                .is_some_and(GovernorCancellation::is_cancelled)
            {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?
    .into_snapshot())
}

pub(crate) fn encode_consolidation_mutation(plan: &ConsolidationPlan) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(112);
    encoded.extend_from_slice(b"HYANNC01");
    encoded.extend_from_slice(&plan.base_identity);
    encoded.extend_from_slice(&plan.captured_view_identity);
    encoded.extend_from_slice(&plan.replacement.build_identity());
    encoded.extend_from_slice(
        &u64::try_from(plan.captured_deltas.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    encoded
}

pub(crate) fn consolidate_tree(
    pages: &mut PageStore,
    buffer_pool: &BufferPool,
    root: Option<PageId>,
    creating_csn: Csn,
    plan: &ConsolidationPlan,
    structural_plan: &PrefixReplacementStructuralPlan,
) -> Result<BTree, NativeRuntimeError> {
    let root = root.ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let tree = BTree::from_root(root);
    let definition = plan.definition();
    let load_plan = plan_index_load(pages, buffer_pool, root, plan.index, definition)?;
    let (mut current, physical_entries, _) =
        load_planned_index_with_entries(pages, buffer_pool, &load_plan, None)?;
    if current.persisted_version == 5 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    if current.base.build_identity() != plan.base_identity {
        return Err(NativeRuntimeError::AnnConsolidationStale);
    }
    for (object_id, captured_sequence) in &plan.captured_deltas {
        if current
            .deltas
            .get(object_id)
            .is_some_and(|delta| delta.sequence() == *captured_sequence)
        {
            current.deltas.remove(object_id);
        }
    }
    let previous = current.base.retention_descriptor();
    if current.base.len() != 0
        && previous.build_identity != plan.replacement.build_identity()
        && !current
            .retained_generations
            .iter()
            .any(|generation| generation.build_identity == previous.build_identity)
    {
        current.retained_generations.push(previous);
    }
    let retain = usize::from(current.lifecycle.retain_generations);
    if current.retained_generations.len() > retain {
        current
            .retained_generations
            .drain(..current.retained_generations.len() - retain);
    }
    current.validate_delta_bounds()?;
    let replacement_view_identity = calculate_view_identity(
        plan.replacement.build_identity(),
        current.next_sequence,
        &current.deltas,
    );

    let physical = prepare_consolidation_physical_replacement(
        pages,
        tree,
        &current,
        physical_entries,
        plan,
        replacement_view_identity,
    )?;
    let mut unpublished = pages.begin_unpublished_tail()?;
    let mutation = tree.replace_prefixes_sorted_batch_in_unpublished_tail_with_control(
        &mut unpublished,
        structural_plan,
        PrefixReplacementBatch {
            creating_csn,
            prefixes: &physical.prefixes,
            expected_keys: &physical.expected_keys,
            replacements: physical.replacements,
        },
        || ControlFlow::Continue(()),
    );
    let replacement = match mutation {
        Ok(result) => result.tree,
        Err(error) => {
            unpublished.rollback()?;
            return Err(match error {
                BTreeError::PrefixContentsChanged => NativeRuntimeError::AnnConsolidationStale,
                BTreeError::Cancelled => NativeRuntimeError::InvalidAnnTree,
                error => error.into(),
            });
        }
    };
    if let Err(error) = validate_consolidated_tree_unpublished(
        &unpublished,
        replacement,
        plan,
        replacement_view_identity,
        &current.deltas,
    ) {
        unpublished.rollback()?;
        return Err(error);
    }
    unpublished.finalize();
    Ok(replacement)
}

struct ConsolidationPhysicalReplacement {
    prefixes: Vec<Vec<u8>>,
    expected_keys: Vec<Vec<u8>>,
    replacements: Vec<KeyValue>,
}

fn prepare_consolidation_physical_replacement(
    pages: &PageStore,
    tree: BTree,
    current: &AnnIndexState,
    physical_entries: Vec<KeyValue>,
    plan: &ConsolidationPlan,
    replacement_view_identity: [u8; 32],
) -> Result<ConsolidationPhysicalReplacement, NativeRuntimeError> {
    let marker = tree
        .get(pages, crate::SEARCH_FORMAT_KEY)?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let mut replacements = BTreeMap::new();
    replacements.insert(
        crate::SEARCH_FORMAT_KEY.to_vec(),
        crate::SEARCH_FORMAT_VALUE_V3.to_vec(),
    );
    replacements.insert(
        meta_key(plan.index),
        encode_consolidated_metadata(current, &plan.replacement, replacement_view_identity)?,
    );
    for snapshot in plan.replacement.snapshots() {
        append_generation_entries(&mut replacements, snapshot)?;
    }
    let mut expected_keys = Vec::with_capacity(physical_entries.len().saturating_add(2));
    expected_keys.push(crate::SEARCH_FORMAT_KEY.to_vec());
    expected_keys.push(meta_key(plan.index));
    for (key, value) in physical_entries {
        expected_keys.push(key.clone());
        if ann_generation_identity(&key).is_some_and(|identity| {
            current.retained_generations.iter().any(|generation| {
                generation
                    .children
                    .iter()
                    .any(|child| child.build_identity == identity)
            })
        }) {
            replacements.insert(key, value);
        }
    }
    for (object_id, delta) in &current.deltas {
        replacements.insert(delta_key(plan.index, *object_id), encode_delta(delta)?);
    }
    if marker != crate::SEARCH_FORMAT_VALUE_V1
        && marker != crate::SEARCH_FORMAT_VALUE_V2
        && marker != crate::SEARCH_FORMAT_VALUE_V3
        || expected_keys.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(ConsolidationPhysicalReplacement {
        prefixes: vec![
            crate::SEARCH_FORMAT_KEY.to_vec(),
            object_prefix(ANN_INDEX_META_PREFIX, plan.index),
            object_prefix(ANN_VECTOR_PREFIX, plan.index),
            object_prefix(ANN_GRAPH_LAYER_PREFIX, plan.index),
            object_prefix(ANN_DELTA_PREFIX, plan.index),
        ],
        expected_keys,
        replacements: replacements.into_iter().collect(),
    })
}

fn validate_consolidated_tree_unpublished(
    unpublished: &UnpublishedTail<'_>,
    tree: BTree,
    plan: &ConsolidationPlan,
    expected_view_identity: [u8; 32],
    expected_deltas: &BTreeMap<ObjectId, DeltaRecord>,
) -> Result<(), NativeRuntimeError> {
    let definition = plan.definition();
    let marker = tree
        .get_unpublished(unpublished, crate::SEARCH_FORMAT_KEY)?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if marker != crate::SEARCH_FORMAT_VALUE_V1
        && marker != crate::SEARCH_FORMAT_VALUE_V2
        && marker != crate::SEARCH_FORMAT_VALUE_V3
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let encoded_metadata = tree
        .get_unpublished(unpublished, &meta_key(plan.index))?
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let metadata = decode_metadata(&encoded_metadata)?;
    let limits = index_physical_limits(definition, &metadata)?;
    let entries = scan_index_physical_entries_unpublished(tree, unpublished, plan.index, limits)?;
    validate_target_physical_entries(&entries, plan.index, definition, &metadata)?;
    let current = restore_index_with_definition_controlled(
        &entries, plan.index, definition, metadata, None, false,
    )?;
    if current.base.build_identity() != plan.replacement.build_identity()
        || current.base.definition() != plan.replacement.definition()
        || current.base.len() != plan.replacement.len()
        || current.base.input_identity() != plan.replacement.input_identity()
        || current.base.is_partitioned()
            != matches!(
                plan.replacement.as_ref(),
                ConsolidationReplacement::Partitioned(_)
            )
        || current.view_identity != expected_view_identity
        || current.deltas != *expected_deltas
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn scan_index_physical_entries_unpublished(
    tree: BTree,
    unpublished: &UnpublishedTail<'_>,
    index: ObjectId,
    limits: AnnPhysicalLimits,
) -> Result<Vec<KeyValue>, NativeRuntimeError> {
    let mut entries = Vec::new();
    for (prefix, limit) in [
        (ANN_VECTOR_PREFIX, limits.vectors),
        (ANN_GRAPH_LAYER_PREFIX, limits.graph_layers),
        (ANN_DELTA_PREFIX, limits.deltas),
        (ANN_OVERLAY_MANIFEST_PREFIX, limits.overlay_manifest),
        (ANN_OVERLAY_DELTA_PREFIX, limits.overlay_deltas),
        (ANN_OVERLAY_NODE_PREFIX, limits.overlay_nodes),
    ] {
        let mut visited_entries = 0_usize;
        let mut visited_bytes = 0_u64;
        let mut exceeded = false;
        let outcome = tree.visit_prefix_unpublished(
            unpublished,
            &object_prefix(prefix, index),
            |key, value| {
                let Some(next_entries) = visited_entries.checked_add(1) else {
                    exceeded = true;
                    return ControlFlow::Break(());
                };
                let encoded_bytes =
                    u64::try_from(key.len().saturating_add(value.len())).unwrap_or(u64::MAX);
                let Some(next_bytes) = visited_bytes.checked_add(encoded_bytes) else {
                    exceeded = true;
                    return ControlFlow::Break(());
                };
                if next_entries > limit.entries || next_bytes > limit.bytes {
                    exceeded = true;
                    return ControlFlow::Break(());
                }
                visited_entries = next_entries;
                visited_bytes = next_bytes;
                entries.push((key.to_vec(), value.to_vec()));
                ControlFlow::Continue(())
            },
        )?;
        if exceeded || matches!(outcome, ControlFlow::Break(())) {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(entries)
}

pub(crate) fn observe(
    pages: &PageStore,
    root: PageId,
    catalog: &CatalogState,
    index: ObjectId,
) -> Result<IndexObservation, NativeRuntimeError> {
    let state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state
        .indexes
        .get(&index)
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
    let entries = BTree::from_root(root).scan(pages)?;
    Ok(index_observation(current, &entries, index))
}

pub(crate) fn inspect_consolidation_publication(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    load_plan: &AnnIndexLoadPlan,
    plan: &ConsolidationPlan,
) -> Result<(IndexObservation, usize), NativeRuntimeError> {
    let (current, entries, _) =
        load_planned_index_with_entries(pages, buffer_pool, load_plan, None)?;
    if current.base.build_identity() != plan.base_identity {
        return Err(NativeRuntimeError::AnnConsolidationStale);
    }
    let consumed = plan
        .captured_deltas
        .iter()
        .filter(|(object_id, captured_sequence)| {
            current
                .deltas
                .get(object_id)
                .is_some_and(|delta| delta.sequence() == **captured_sequence)
        })
        .count();
    Ok((index_observation(&current, &entries, plan.index), consumed))
}

pub(crate) fn observe_planned_index(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    load_plan: &AnnIndexLoadPlan,
) -> Result<IndexObservation, NativeRuntimeError> {
    let (current, entries, _) =
        load_planned_index_with_entries(pages, buffer_pool, load_plan, None)?;
    Ok(index_observation(&current, &entries, load_plan.index))
}

fn index_observation(
    current: &AnnIndexState,
    entries: &[KeyValue],
    index: ObjectId,
) -> IndexObservation {
    let selected_identities = current
        .base
        .retention_descriptor()
        .children
        .into_iter()
        .map(|child| child.build_identity)
        .collect::<Vec<_>>();
    let generation_records = entries
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.first().copied(),
                Some(ANN_VECTOR_PREFIX | ANN_GRAPH_LAYER_PREFIX)
            ) && key
                .get(1..17)
                .is_some_and(|encoded| encoded == index.get().to_be_bytes().as_slice())
        })
        .count();
    let selected_generation_records = entries
        .iter()
        .filter(|(key, _)| match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                decode_vector_key(key).is_ok_and(|(found_index, build_identity, _)| {
                    found_index == index && selected_identities.contains(&build_identity)
                })
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                decode_graph_layer_key(key).is_ok_and(|(found_index, build_identity, _, _)| {
                    found_index == index && selected_identities.contains(&build_identity)
                })
            }
            _ => false,
        })
        .count();
    IndexObservation {
        base_identity: current.base.build_identity(),
        view_identity: current.view_identity,
        base_vector_count: current.base.len(),
        effective_vector_count: current.effective_vector_count(),
        delta_records: current.deltas.len(),
        delta_bytes: current.delta_bytes(),
        generation_records,
        selected_generation_records,
        lifecycle: current.lifecycle,
        maintenance_due: maintenance_due(current),
    }
}

pub(crate) fn maintenance_status(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
) -> Result<MaintenanceStatus, NativeRuntimeError> {
    let metadata = decode_metadata(&plan.encoded_metadata)?;
    if metadata.version == 5 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let mut entries = Vec::new();
    visit_bounded_physical_range(
        BTree::from_root(plan.root),
        pages,
        buffer_pool,
        &object_prefix(ANN_DELTA_PREFIX, plan.index),
        plan.physical_limits.deltas,
        None,
        &mut entries,
    )?;
    let mut deltas = BTreeMap::new();
    for (key, value) in entries {
        let (found_index, object_id) = decode_delta_key(&key)?;
        if found_index != plan.index
            || deltas
                .insert(object_id, decode_delta(&value, object_id, plan.definition)?)
                .is_some()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    validate_restored_deltas(&metadata, &deltas)?;
    if deltas
        .values()
        .map(DeltaRecord::sequence)
        .max()
        .is_some_and(|maximum| maximum >= metadata.next_sequence)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(MaintenanceStatus {
        lifecycle: metadata.lifecycle,
        delta_records: deltas.len(),
        delta_bytes: deltas.values().map(DeltaRecord::encoded_len).sum(),
        due: maintenance_due_counts(metadata.lifecycle, deltas.len()),
    })
}

fn maintenance_due(state: &AnnIndexState) -> bool {
    maintenance_due_counts(state.lifecycle, state.deltas.len())
}

fn maintenance_due_counts(lifecycle: IncrementalVectorLifecycle, delta_records: usize) -> bool {
    delta_records >= usize::from(lifecycle.consolidate_after_deltas)
        || delta_records >= usize::try_from(lifecycle.delta_max_entries).unwrap_or(usize::MAX)
}

#[allow(clippy::too_many_lines)]
fn validate_physical_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    metadata: &BTreeMap<ObjectId, PersistedIndexMetadata>,
) -> Result<(), NativeRuntimeError> {
    let mut indexes_with_records = BTreeSet::new();
    let mut generations = BTreeMap::<(ObjectId, [u8; 32]), PhysicalGenerationSummary>::new();
    for (key, value) in entries {
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (index, build_identity, object_id) = decode_vector_key(key)?;
                let definition = catalog_ann_definition(catalog, index)?;
                decode_vector_record(value, object_id, definition)?;
                let persisted = metadata
                    .get(&index)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                if !persisted.owns_physical_identity(build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                if !generations
                    .entry((index, build_identity))
                    .or_default()
                    .vector_ids
                    .insert(object_id)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (index, build_identity, object_id, layer) = decode_graph_layer_key(key)?;
                catalog_ann_definition(catalog, index)?;
                decode_graph_layer(value)?;
                let persisted = metadata
                    .get(&index)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                if !persisted.owns_physical_identity(build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                if !generations
                    .entry((index, build_identity))
                    .or_default()
                    .graph_layers
                    .entry(object_id)
                    .or_default()
                    .insert(layer)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            Some(ANN_DELTA_PREFIX) => {
                let (index, object_id) = decode_delta_key(key)?;
                let definition = catalog_ann_definition(catalog, index)?;
                decode_delta(value, object_id, definition)?;
                indexes_with_records.insert(index);
            }
            Some(ANN_OVERLAY_MANIFEST_PREFIX) => {
                let index = decode_overlay_manifest_key(key)?;
                decode_overlay_manifest(value)?;
                if metadata
                    .get(&index)
                    .is_none_or(|persisted| persisted.overlay.is_none())
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            Some(ANN_OVERLAY_DELTA_PREFIX) => {
                let (index, object_id) = decode_overlay_delta_key(key)?;
                let definition = catalog_ann_definition(catalog, index)?;
                decode_overlay_delta(value, object_id, definition)?;
                if metadata
                    .get(&index)
                    .is_none_or(|persisted| persisted.overlay.is_none())
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            Some(ANN_OVERLAY_NODE_PREFIX) => {
                let (index, depth, _) = decode_overlay_node_key(key)?;
                decode_overlay_node(value, depth)?;
                catalog_ann_definition(catalog, index)?;
                if metadata
                    .get(&index)
                    .is_none_or(|persisted| persisted.overlay.is_none())
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            _ => {}
        }
    }
    if indexes_with_records
        .iter()
        .any(|index| !metadata.contains_key(index))
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    for (index, persisted) in metadata {
        for child in persisted
            .retained_generations
            .iter()
            .flat_map(|generation| &generation.children)
            .filter(|child| child.complete)
        {
            let summary = generations
                .get(&(*index, child.build_identity))
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            validate_retained_child_entries(child, summary)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_target_physical_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: &PersistedIndexMetadata,
) -> Result<(), NativeRuntimeError> {
    let mut generations = BTreeMap::<[u8; 32], PhysicalGenerationSummary>::new();
    let mut delta_count = 0_u64;
    let mut delta_bytes = 0_u64;
    let mut overlay_manifest_count = 0_u64;
    let mut overlay_delta_count = 0_u64;
    let mut overlay_delta_bytes = 0_u64;
    for (key, value) in entries {
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (found_index, build_identity, object_id) = decode_vector_key(key)?;
                if found_index != index || !metadata.owns_physical_identity(build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                decode_vector_record(value, object_id, definition)?;
                if !generations
                    .entry(build_identity)
                    .or_default()
                    .vector_ids
                    .insert(object_id)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (found_index, build_identity, object_id, layer) = decode_graph_layer_key(key)?;
                if found_index != index || !metadata.owns_physical_identity(build_identity) {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                decode_graph_layer(value)?;
                if !generations
                    .entry(build_identity)
                    .or_default()
                    .graph_layers
                    .entry(object_id)
                    .or_default()
                    .insert(layer)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_DELTA_PREFIX) => {
                let (found_index, object_id) = decode_delta_key(key)?;
                if found_index != index {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                let delta = decode_delta(value, object_id, definition)?;
                delta_count = delta_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                delta_bytes = delta_bytes
                    .checked_add(
                        u64::try_from(delta.encoded_len())
                            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
                    )
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            }
            Some(ANN_OVERLAY_MANIFEST_PREFIX) => {
                let found_index = decode_overlay_manifest_key(key)?;
                if found_index != index || metadata.overlay.is_none() {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                decode_overlay_manifest(value)?;
                overlay_manifest_count = overlay_manifest_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            }
            Some(ANN_OVERLAY_DELTA_PREFIX) => {
                let (found_index, object_id) = decode_overlay_delta_key(key)?;
                if found_index != index || metadata.overlay.is_none() {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                let delta = decode_overlay_delta(value, object_id, definition)?;
                overlay_delta_count = overlay_delta_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                overlay_delta_bytes = overlay_delta_bytes
                    .checked_add(
                        u64::try_from(delta.encoded_len())
                            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
                    )
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            }
            Some(ANN_OVERLAY_NODE_PREFIX) => {
                let (found_index, depth, _) = decode_overlay_node_key(key)?;
                if found_index != index || metadata.overlay.is_none() {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                decode_overlay_node(value, depth)?;
            }
            _ => return Err(NativeRuntimeError::InvalidAnnTree),
        }
    }
    if delta_count != metadata.delta_count || delta_bytes != metadata.delta_bytes {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    match metadata.overlay {
        Some(overlay)
            if overlay_manifest_count == 1
                && overlay_delta_count == overlay.overlay_count
                && overlay_delta_bytes == overlay.overlay_bytes => {}
        None if overlay_manifest_count == 0
            && overlay_delta_count == 0
            && overlay_delta_bytes == 0 => {}
        _ => return Err(NativeRuntimeError::InvalidAnnTree),
    }
    for child in metadata
        .retained_generations
        .iter()
        .flat_map(|generation| &generation.children)
        .filter(|child| child.complete)
    {
        let summary = generations
            .get(&child.build_identity)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        validate_retained_child_entries(child, summary)?;
    }
    Ok(())
}

#[derive(Default)]
struct PhysicalGenerationSummary {
    vector_ids: BTreeSet<ObjectId>,
    graph_layers: BTreeMap<ObjectId, BTreeSet<u16>>,
}

fn validate_retained_child_entries(
    descriptor: &PersistedChildDescriptor,
    summary: &PhysicalGenerationSummary,
) -> Result<(), NativeRuntimeError> {
    let vector_count =
        u64::try_from(summary.vector_ids.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let graph_node_count = u64::try_from(summary.graph_layers.len())
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let maximum_level = summary
        .graph_layers
        .values()
        .filter_map(|layers| layers.last().copied())
        .max()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let entry_point_level = descriptor
        .entry_point
        .and_then(|entry_point| summary.graph_layers.get(&entry_point))
        .and_then(|layers| layers.last().copied());
    let contiguous_layers = summary.graph_layers.values().all(|layers| {
        layers
            .last()
            .is_some_and(|maximum| layers.iter().copied().eq(0..=*maximum))
    });
    if vector_count != descriptor.vector_count
        || graph_node_count != descriptor.graph_node_count
        || summary.vector_ids != summary.graph_layers.keys().copied().collect()
        || maximum_level != descriptor.max_level
        || entry_point_level != Some(descriptor.max_level)
        || !contiguous_layers
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn validate_catalog_coverage(
    catalog: &CatalogState,
    state: &AnnState,
) -> Result<(), NativeRuntimeError> {
    let expected = catalog
        .objects
        .iter()
        .filter_map(|(id, object)| match object {
            CatalogObject::Search(definition) if definition.ann.is_some() => Some(*id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let actual = state.indexes.keys().copied().collect::<BTreeSet<_>>();
    if expected != actual {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    for (index, persisted) in &state.indexes {
        if persisted.definition() != catalog_ann_definition(catalog, *index)? {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    Ok(())
}

fn catalog_ann_definition(
    catalog: &CatalogState,
    index: ObjectId,
) -> Result<VectorIndexDefinition, NativeRuntimeError> {
    let Some(CatalogObject::Search(definition)) = catalog.object(index) else {
        return Err(NativeRuntimeError::InvalidAnnTree);
    };
    definition_from_search(definition)
}

fn append_generation_entries(
    entries: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    snapshot: &IndexSnapshot,
) -> Result<(), NativeRuntimeError> {
    for record in &snapshot.vectors {
        entries.insert(
            vector_key(
                snapshot.definition.index_id(),
                snapshot.build_identity,
                record.object_id,
            ),
            encode_vector_record(record)?,
        );
    }
    for node in &snapshot.nodes {
        for (layer, neighbors) in node.neighbors.iter().enumerate() {
            entries.insert(
                graph_layer_key(
                    snapshot.definition.index_id(),
                    snapshot.build_identity,
                    node.object_id,
                    u16::try_from(layer).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
                ),
                encode_graph_layer(neighbors)?,
            );
        }
    }
    Ok(())
}

fn append_base_generation_entries(
    entries: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    base: &AnnBase,
) -> Result<(), NativeRuntimeError> {
    for snapshot in base.export_snapshots() {
        append_generation_entries(entries, &snapshot)?;
    }
    Ok(())
}

pub(crate) fn meta_key(index: ObjectId) -> Vec<u8> {
    object_prefix(ANN_INDEX_META_PREFIX, index)
}

pub(crate) fn vector_key(
    index: ObjectId,
    build_identity: [u8; 32],
    object_id: ObjectId,
) -> Vec<u8> {
    let mut key = generation_prefix(ANN_VECTOR_PREFIX, index, build_identity);
    key.extend_from_slice(&object_id.get().to_be_bytes());
    key
}

fn graph_layer_key(
    index: ObjectId,
    build_identity: [u8; 32],
    object_id: ObjectId,
    layer: u16,
) -> Vec<u8> {
    let mut key = generation_prefix(ANN_GRAPH_LAYER_PREFIX, index, build_identity);
    key.extend_from_slice(&object_id.get().to_be_bytes());
    key.extend_from_slice(&layer.to_be_bytes());
    key
}

fn delta_key(index: ObjectId, object_id: ObjectId) -> Vec<u8> {
    let mut key = object_prefix(ANN_DELTA_PREFIX, index);
    key.extend_from_slice(&object_id.get().to_be_bytes());
    key
}

#[cfg(test)]
fn overlay_manifest_key(index: ObjectId) -> Vec<u8> {
    object_prefix(ANN_OVERLAY_MANIFEST_PREFIX, index)
}

#[cfg(test)]
fn overlay_delta_key(index: ObjectId, object_id: ObjectId) -> Vec<u8> {
    let mut key = object_prefix(ANN_OVERLAY_DELTA_PREFIX, index);
    key.extend_from_slice(&object_id.get().to_be_bytes());
    key
}

fn overlay_node_prefix(index: ObjectId, depth: u8) -> Vec<u8> {
    let mut key = object_prefix(ANN_OVERLAY_NODE_PREFIX, index);
    key.push(depth);
    key
}

#[cfg(test)]
fn overlay_node_key(index: ObjectId, depth: u8, path: u128) -> Vec<u8> {
    let mut key = overlay_node_prefix(index, depth);
    key.extend_from_slice(&path.to_be_bytes());
    key
}

fn object_prefix(prefix: u8, index: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(prefix);
    key.extend_from_slice(&index.get().to_be_bytes());
    key
}

fn generation_prefix(prefix: u8, index: ObjectId, build_identity: [u8; 32]) -> Vec<u8> {
    let mut key = object_prefix(prefix, index);
    key.extend_from_slice(&build_identity);
    key
}

fn ann_key_targets_index(key: &[u8], index: ObjectId) -> bool {
    is_ann_physical_key(key)
        && key
            .get(1..17)
            .is_some_and(|encoded| encoded == index.get().to_be_bytes().as_slice())
}

fn ann_generation_identity(key: &[u8]) -> Option<[u8; 32]> {
    if !matches!(
        key.first().copied(),
        Some(ANN_VECTOR_PREFIX | ANN_GRAPH_LAYER_PREFIX)
    ) {
        return None;
    }
    key.get(17..49)?.try_into().ok()
}

fn decode_meta_key(key: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    if key.len() != 17 || key.first() != Some(&ANN_INDEX_META_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    decode_index(&key[1..17])
}

fn decode_vector_key(key: &[u8]) -> Result<(ObjectId, [u8; 32], ObjectId), NativeRuntimeError> {
    if key.len() != ANN_GENERATION_KEY_SIZE || key.first() != Some(&ANN_VECTOR_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok((
        decode_index(&key[1..17])?,
        key[17..49]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        decode_index(&key[49..65])?,
    ))
}

fn decode_graph_layer_key(
    key: &[u8],
) -> Result<(ObjectId, [u8; 32], ObjectId, u16), NativeRuntimeError> {
    if key.len() != ANN_GRAPH_LAYER_KEY_SIZE || key.first() != Some(&ANN_GRAPH_LAYER_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok((
        decode_index(&key[1..17])?,
        key[17..49]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        decode_index(&key[49..65])?,
        u16::from_be_bytes(
            key[65..67]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
    ))
}

fn decode_delta_key(key: &[u8]) -> Result<(ObjectId, ObjectId), NativeRuntimeError> {
    if key.len() != ANN_DELTA_KEY_SIZE || key.first() != Some(&ANN_DELTA_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok((decode_index(&key[1..17])?, decode_index(&key[17..33])?))
}

fn decode_overlay_manifest_key(key: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    if key.len() != ANN_INDEX_META_KEY_SIZE || key.first() != Some(&ANN_OVERLAY_MANIFEST_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    decode_index(&key[1..17])
}

fn decode_overlay_delta_key(key: &[u8]) -> Result<(ObjectId, ObjectId), NativeRuntimeError> {
    if key.len() != ANN_DELTA_KEY_SIZE || key.first() != Some(&ANN_OVERLAY_DELTA_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok((decode_index(&key[1..17])?, decode_index(&key[17..33])?))
}

fn decode_overlay_node_key(key: &[u8]) -> Result<(ObjectId, u8, u128), NativeRuntimeError> {
    if key.len() != ANN_OVERLAY_NODE_KEY_SIZE || key.first() != Some(&ANN_OVERLAY_NODE_PREFIX) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let depth = key[17];
    if depth >= ANN_OVERLAY_TREE_DEPTH {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let path = u128::from_be_bytes(
        key[18..34]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    if path != overlay_path_prefix(path, depth) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok((decode_index(&key[1..17])?, depth, path))
}

fn decode_index(encoded: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeRuntimeError::InvalidAnnTree)
}

fn decode_overlay_manifest(encoded: &[u8]) -> Result<OverlayManifest, NativeRuntimeError> {
    if encoded.len() != ANN_OVERLAY_MANIFEST_SIZE
        || encoded.get(..8) != Some(ANN_OVERLAY_MANIFEST_MAGIC.as_slice())
        || encoded[176..184].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let manifest = OverlayManifest {
        legacy_view_identity: encoded[8..40]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        overlay_root: encoded[40..72]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        view_identity: encoded[72..104]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        legacy_count: read_u64(&encoded[104..112]),
        legacy_bytes: read_u64(&encoded[112..120]),
        overlay_count: read_u64(&encoded[120..128]),
        overlay_bytes: read_u64(&encoded[128..136]),
        overlay_node_count: read_u64(&encoded[136..144]),
        effective_count: read_u64(&encoded[144..152]),
        effective_bytes: read_u64(&encoded[152..160]),
        legacy_next_sequence: read_u64(&encoded[160..168]),
        next_sequence: read_u64(&encoded[168..176]),
    };
    if [
        manifest.legacy_view_identity,
        manifest.overlay_root,
        manifest.view_identity,
    ]
    .contains(&[0; 32])
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(manifest)
}

fn encode_initial_bulk_metadata(
    current: &AnnIndexState,
    snapshot: &PartitionedIndexSnapshot,
) -> Result<Vec<u8>, NativeRuntimeError> {
    if current.persisted_version == 5 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    current
        .lifecycle
        .validate()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let child_count =
        u16::try_from(snapshot.partitions.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let vector_count = snapshot
        .partitions
        .iter()
        .try_fold(0_u64, |count, child| {
            count.checked_add(u64::try_from(child.vectors.len()).ok()?)
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_node_count = snapshot
        .partitions
        .iter()
        .try_fold(0_u64, |count, child| {
            count.checked_add(u64::try_from(child.nodes.len()).ok()?)
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let capacity = ANN_INDEX_META_V4_HEADER_SIZE
        .checked_add(
            snapshot
                .partitions
                .len()
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V4);
    encoded.extend_from_slice(&snapshot.build_identity);
    encoded.extend_from_slice(&snapshot.build_identity);
    encoded.extend_from_slice(&snapshot.input_identity);
    encoded.extend_from_slice(&vector_count.to_le_bytes());
    encoded.extend_from_slice(&graph_node_count.to_le_bytes());
    encoded.extend_from_slice(&0_u64.to_le_bytes());
    encoded.extend_from_slice(&0_u64.to_le_bytes());
    encoded.extend_from_slice(&1_u64.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.delta_max_entries.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.consolidate_after_deltas.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.retain_generations.to_le_bytes());
    encoded.extend_from_slice(&[ANN_BASE_PARTITIONED, 0]);
    encoded.extend_from_slice(&child_count.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    encoded.extend_from_slice(&[0; 2]);
    for child in &snapshot.partitions {
        encode_child_descriptor(&mut encoded, child)?;
    }
    if encoded.len() != capacity {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(encoded)
}

fn encode_consolidated_metadata(
    current: &AnnIndexState,
    replacement: &ConsolidationReplacement,
    view_identity: [u8; 32],
) -> Result<Vec<u8>, NativeRuntimeError> {
    current
        .lifecycle
        .validate()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if current.next_sequence == 0
        || view_identity == [0; 32]
        || replacement.definition() != current.definition()
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let retained_count = u16::try_from(current.retained_generations.len())
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if usize::from(retained_count) > usize::from(current.lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_retained_generations(&current.retained_generations, replacement.build_identity())?;
    let children = replacement.child_descriptors();
    let child_count =
        u16::try_from(children.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let retained_size = current
        .retained_generations
        .iter()
        .try_fold(0_usize, |size, generation| {
            ANN_INDEX_META_V4_RETAINED_HEADER_SIZE
                .checked_add(
                    generation
                        .children
                        .len()
                        .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)?,
                )
                .and_then(|generation_size| size.checked_add(generation_size))
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let capacity = ANN_INDEX_META_V4_HEADER_SIZE
        .checked_add(
            children
                .len()
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .and_then(|size| size.checked_add(retained_size))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let vector_count = children
        .iter()
        .try_fold(0_u64, |count, child| count.checked_add(child.vector_count))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_node_count = children
        .iter()
        .try_fold(0_u64, |count, child| {
            count.checked_add(child.graph_node_count)
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V4);
    encoded.extend_from_slice(&replacement.build_identity());
    encoded.extend_from_slice(&view_identity);
    encoded.extend_from_slice(&replacement.input_identity().unwrap_or([0; 32]));
    encoded.extend_from_slice(&vector_count.to_le_bytes());
    encoded.extend_from_slice(&graph_node_count.to_le_bytes());
    encoded.extend_from_slice(
        &u64::try_from(current.deltas.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &u64::try_from(current.delta_bytes())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&current.next_sequence.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.delta_max_entries.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.consolidate_after_deltas.to_le_bytes());
    encoded.extend_from_slice(&current.lifecycle.retain_generations.to_le_bytes());
    encoded.push(match replacement.base_kind() {
        PersistedBaseKind::Single => ANN_BASE_SINGLE,
        PersistedBaseKind::Partitioned => ANN_BASE_PARTITIONED,
    });
    encoded.push(0);
    encoded.extend_from_slice(&child_count.to_le_bytes());
    encoded.extend_from_slice(&retained_count.to_le_bytes());
    encoded.extend_from_slice(&[0; 2]);
    for child in &children {
        encode_persisted_child_descriptor(&mut encoded, child)?;
    }
    for generation in &current.retained_generations {
        let child_count = u16::try_from(generation.children.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        encoded.extend_from_slice(&generation.build_identity);
        encoded.extend_from_slice(&child_count.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        for child in &generation.children {
            encode_persisted_child_descriptor(&mut encoded, child)?;
        }
    }
    if encoded.len() != capacity {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(encoded)
}

fn encode_metadata(state: &AnnIndexState) -> Result<Vec<u8>, NativeRuntimeError> {
    state
        .lifecycle
        .validate()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if state.persisted_version == 5 || state.next_sequence == 0 || state.view_identity == [0; 32] {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let children = state.base.child_descriptors();
    let child_count =
        u16::try_from(children.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let retained_count = u16::try_from(state.retained_generations.len())
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if usize::from(retained_count) > usize::from(state.lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_retained_generations(&state.retained_generations, state.base.build_identity())?;
    let retained_size = state
        .retained_generations
        .iter()
        .try_fold(0_usize, |size, generation| {
            ANN_INDEX_META_V4_RETAINED_HEADER_SIZE
                .checked_add(
                    generation
                        .children
                        .len()
                        .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)?,
                )
                .and_then(|generation_size| size.checked_add(generation_size))
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let capacity = ANN_INDEX_META_V4_HEADER_SIZE
        .checked_add(
            children
                .len()
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .and_then(|size| size.checked_add(retained_size))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let vector_count = children
        .iter()
        .try_fold(0_u64, |count, child| count.checked_add(child.vector_count))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let graph_node_count = children
        .iter()
        .try_fold(0_u64, |count, child| {
            count.checked_add(child.graph_node_count)
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V4);
    encoded.extend_from_slice(&state.base.build_identity());
    encoded.extend_from_slice(&state.view_identity);
    encoded.extend_from_slice(&state.base.input_identity().unwrap_or([0; 32]));
    encoded.extend_from_slice(&vector_count.to_le_bytes());
    encoded.extend_from_slice(&graph_node_count.to_le_bytes());
    encoded.extend_from_slice(
        &u64::try_from(state.deltas.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &u64::try_from(state.delta_bytes())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&state.next_sequence.to_le_bytes());
    encoded.extend_from_slice(&state.lifecycle.delta_max_entries.to_le_bytes());
    encoded.extend_from_slice(&state.lifecycle.consolidate_after_deltas.to_le_bytes());
    encoded.extend_from_slice(&state.lifecycle.retain_generations.to_le_bytes());
    encoded.push(match state.base {
        AnnBase::Single(_) => ANN_BASE_SINGLE,
        AnnBase::Partitioned(_) => ANN_BASE_PARTITIONED,
    });
    encoded.push(0);
    encoded.extend_from_slice(&child_count.to_le_bytes());
    encoded.extend_from_slice(&retained_count.to_le_bytes());
    encoded.extend_from_slice(&[0; 2]);
    for child in &children {
        encode_persisted_child_descriptor(&mut encoded, child)?;
    }
    for generation in &state.retained_generations {
        let child_count = u16::try_from(generation.children.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        encoded.extend_from_slice(&generation.build_identity);
        encoded.extend_from_slice(&child_count.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        for child in &generation.children {
            encode_persisted_child_descriptor(&mut encoded, child)?;
        }
    }
    debug_assert_eq!(encoded.len(), capacity);
    Ok(encoded)
}

#[allow(clippy::too_many_lines)]
fn encode_delta_metadata(
    metadata: &PersistedIndexMetadata,
    expected_metadata: &[u8],
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
    view_identity: [u8; 32],
) -> Result<Vec<u8>, NativeRuntimeError> {
    metadata
        .lifecycle
        .validate()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    validate_delta_mutation_bounds(metadata, deltas)?;
    if metadata.next_sequence == 0 || view_identity == [0; 32] {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let delta_count =
        u64::try_from(deltas.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let delta_bytes = deltas
        .values()
        .try_fold(0_u64, |bytes, delta| {
            bytes.checked_add(u64::try_from(delta.encoded_len()).ok()?)
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if matches!(metadata.version, 2 | 3) {
        let mut encoded = expected_metadata.to_vec();
        encoded[88..120].copy_from_slice(&view_identity);
        encoded[120..128].copy_from_slice(&delta_count.to_le_bytes());
        encoded[128..136].copy_from_slice(&delta_bytes.to_le_bytes());
        encoded[136..144].copy_from_slice(&metadata.next_sequence.to_le_bytes());
        decode_metadata(&encoded)?;
        return Ok(encoded);
    }
    let child_count =
        u16::try_from(metadata.children.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let retained_count = u16::try_from(metadata.retained_generations.len())
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if usize::from(retained_count) > usize::from(metadata.lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    validate_current_base_metadata(
        metadata.base_kind,
        metadata.build_identity,
        metadata.input_identity.unwrap_or([0; 32]),
        metadata.vector_count,
        metadata
            .children
            .iter()
            .try_fold(0_u64, |count, child| {
                count.checked_add(child.graph_node_count)
            })
            .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        &metadata.children,
    )?;
    validate_retained_generations(&metadata.retained_generations, metadata.build_identity)?;
    let retained_size = metadata
        .retained_generations
        .iter()
        .try_fold(0_usize, |size, generation| {
            ANN_INDEX_META_V4_RETAINED_HEADER_SIZE
                .checked_add(
                    generation
                        .children
                        .len()
                        .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)?,
                )
                .and_then(|generation_size| size.checked_add(generation_size))
        })
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let capacity = ANN_INDEX_META_V4_HEADER_SIZE
        .checked_add(
            metadata
                .children
                .len()
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .and_then(|size| size.checked_add(retained_size))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V4);
    encoded.extend_from_slice(&metadata.build_identity);
    encoded.extend_from_slice(&view_identity);
    encoded.extend_from_slice(&metadata.input_identity.unwrap_or([0; 32]));
    encoded.extend_from_slice(&metadata.vector_count.to_le_bytes());
    encoded.extend_from_slice(
        &metadata
            .children
            .iter()
            .try_fold(0_u64, |count, child| {
                count.checked_add(child.graph_node_count)
            })
            .ok_or(NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&delta_count.to_le_bytes());
    encoded.extend_from_slice(&delta_bytes.to_le_bytes());
    encoded.extend_from_slice(&metadata.next_sequence.to_le_bytes());
    encoded.extend_from_slice(&metadata.lifecycle.delta_max_entries.to_le_bytes());
    encoded.extend_from_slice(&metadata.lifecycle.consolidate_after_deltas.to_le_bytes());
    encoded.extend_from_slice(&metadata.lifecycle.retain_generations.to_le_bytes());
    encoded.push(match metadata.base_kind {
        PersistedBaseKind::Single => ANN_BASE_SINGLE,
        PersistedBaseKind::Partitioned => ANN_BASE_PARTITIONED,
    });
    encoded.push(0);
    encoded.extend_from_slice(&child_count.to_le_bytes());
    encoded.extend_from_slice(&retained_count.to_le_bytes());
    encoded.extend_from_slice(&[0; 2]);
    for child in &metadata.children {
        encode_persisted_child_descriptor(&mut encoded, child)?;
    }
    for generation in &metadata.retained_generations {
        encoded.extend_from_slice(&generation.build_identity);
        encoded.extend_from_slice(
            &u16::try_from(generation.children.len())
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&[0; 6]);
        for child in &generation.children {
            encode_persisted_child_descriptor(&mut encoded, child)?;
        }
    }
    if encoded.len() != capacity {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(encoded)
}

fn decode_metadata(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
    if encoded.len() >= ANN_INDEX_META_V5_HEADER_SIZE
        && encoded.get(..8) == Some(ANN_INDEX_META_MAGIC_V5.as_slice())
    {
        return decode_metadata_v5(encoded);
    }
    if encoded.len() >= ANN_INDEX_META_V4_HEADER_SIZE
        && encoded.get(..8) == Some(ANN_INDEX_META_MAGIC_V4.as_slice())
    {
        return decode_metadata_v4(encoded);
    }
    let version = if encoded.len() == ANN_INDEX_META_V1_SIZE
        && encoded.get(..8) == Some(ANN_INDEX_META_MAGIC_V1.as_slice())
    {
        1
    } else if encoded.len() == ANN_INDEX_META_V2_SIZE
        && encoded.get(..8) == Some(ANN_INDEX_META_MAGIC_V2.as_slice())
    {
        2
    } else if encoded.len() >= ANN_INDEX_META_V3_SIZE
        && encoded.get(..8) == Some(ANN_INDEX_META_MAGIC_V3.as_slice())
    {
        3
    } else {
        return Err(NativeRuntimeError::InvalidAnnTree);
    };
    if encoded[74..80].iter().any(|byte| *byte != 0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let build_identity = encoded[8..40]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if build_identity == [0; 32] {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let raw_entry = u128::from_be_bytes(
        encoded[56..72]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    let (view_identity, delta_count, delta_bytes, next_sequence) = if version == 1 {
        (build_identity, 0, 0, 1)
    } else {
        if encoded[80..88] != *ANN_INDEX_META_MAGIC_V1 {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let view_identity = encoded[88..120]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        if view_identity == [0; 32] {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        (
            view_identity,
            read_u64(&encoded[120..128]),
            read_u64(&encoded[128..136]),
            read_u64(&encoded[136..144]),
        )
    };
    let lifecycle = decode_lifecycle(encoded, version)?;
    let retained_generations =
        decode_legacy_retained_generations(encoded, version, lifecycle, build_identity)?;
    let entry_point = if raw_entry == 0 {
        None
    } else {
        Some(ObjectId::new(raw_entry).map_err(|_| NativeRuntimeError::InvalidAnnTree)?)
    };
    let max_level = u16::from_le_bytes(
        encoded[72..74]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    let vector_count = read_u64(&encoded[40..48]);
    let graph_node_count = read_u64(&encoded[48..56]);
    Ok(PersistedIndexMetadata {
        build_identity,
        vector_count,
        base_kind: PersistedBaseKind::Single,
        input_identity: None,
        children: vec![PersistedChildDescriptor {
            build_identity,
            vector_count,
            graph_node_count,
            entry_point,
            max_level,
            complete: true,
        }],
        view_identity,
        delta_count,
        delta_bytes,
        next_sequence,
        lifecycle,
        retained_generations,
        version,
        overlay: None,
    })
}

fn decode_metadata_v4(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
    if encoded[153] != 0 || encoded[158..160].iter().any(|byte| *byte != 0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let build_identity: [u8; 32] = encoded[8..40]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let view_identity: [u8; 32] = encoded[40..72]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let raw_input_identity: [u8; 32] = encoded[72..104]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if build_identity == [0; 32] || view_identity == [0; 32] {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let vector_count = read_u64(&encoded[104..112]);
    let graph_node_count = read_u64(&encoded[112..120]);
    let delta_count = read_u64(&encoded[120..128]);
    let delta_bytes = read_u64(&encoded[128..136]);
    let next_sequence = read_u64(&encoded[136..144]);
    let lifecycle = decode_lifecycle(encoded, 4)?;
    let base_kind = match encoded[152] {
        ANN_BASE_SINGLE => PersistedBaseKind::Single,
        ANN_BASE_PARTITIONED => PersistedBaseKind::Partitioned,
        _ => return Err(NativeRuntimeError::InvalidAnnTree),
    };
    let child_count = usize::from(u16::from_le_bytes(
        encoded[154..156]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    let retained_count = usize::from(u16::from_le_bytes(
        encoded[156..158]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    if child_count == 0 || retained_count > usize::from(lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let children_end = ANN_INDEX_META_V4_HEADER_SIZE
        .checked_add(
            child_count
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if children_end > encoded.len() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let children = encoded[ANN_INDEX_META_V4_HEADER_SIZE..children_end]
        .chunks_exact(ANN_INDEX_META_V4_CHILD_SIZE)
        .map(decode_child_descriptor)
        .collect::<Result<Vec<_>, _>>()?;
    let retained_generations =
        decode_v4_retained_generations(encoded, children_end, retained_count)?;
    validate_current_base_metadata(
        base_kind,
        build_identity,
        raw_input_identity,
        vector_count,
        graph_node_count,
        &children,
    )?;
    validate_retained_generations(&retained_generations, build_identity)?;
    Ok(PersistedIndexMetadata {
        build_identity,
        vector_count,
        base_kind,
        input_identity: (base_kind == PersistedBaseKind::Partitioned).then_some(raw_input_identity),
        children,
        view_identity,
        delta_count,
        delta_bytes,
        next_sequence,
        lifecycle,
        retained_generations,
        version: 4,
        overlay: None,
    })
}

#[allow(clippy::too_many_lines)]
fn decode_metadata_v5(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
    if encoded[153] != 0
        || encoded[158..160].iter().any(|byte| *byte != 0)
        || encoded[272..280].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let build_identity: [u8; 32] = encoded[8..40]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let view_identity: [u8; 32] = encoded[40..72]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let raw_input_identity: [u8; 32] = encoded[72..104]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let legacy_view_identity: [u8; 32] = encoded[160..192]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let overlay_root: [u8; 32] = encoded[192..224]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if [
        build_identity,
        view_identity,
        legacy_view_identity,
        overlay_root,
    ]
    .contains(&[0; 32])
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let vector_count = read_u64(&encoded[104..112]);
    let graph_node_count = read_u64(&encoded[112..120]);
    let legacy_count = read_u64(&encoded[120..128]);
    let legacy_bytes = read_u64(&encoded[128..136]);
    let next_sequence = read_u64(&encoded[136..144]);
    let lifecycle = decode_lifecycle(encoded, 5)?;
    let base_kind = match encoded[152] {
        ANN_BASE_SINGLE => PersistedBaseKind::Single,
        ANN_BASE_PARTITIONED => PersistedBaseKind::Partitioned,
        _ => return Err(NativeRuntimeError::InvalidAnnTree),
    };
    let child_count = usize::from(u16::from_le_bytes(
        encoded[154..156]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    let retained_count = usize::from(u16::from_le_bytes(
        encoded[156..158]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    let overlay = PersistedOverlayMetadata {
        legacy_view_identity,
        overlay_root,
        overlay_count: read_u64(&encoded[224..232]),
        overlay_bytes: read_u64(&encoded[232..240]),
        overlay_node_count: read_u64(&encoded[240..248]),
        effective_count: read_u64(&encoded[248..256]),
        effective_bytes: read_u64(&encoded[256..264]),
        legacy_next_sequence: read_u64(&encoded[264..272]),
    };
    validate_overlay_metadata(
        legacy_count,
        legacy_bytes,
        next_sequence,
        lifecycle,
        overlay,
    )?;
    if child_count == 0 || retained_count > usize::from(lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let children_end = ANN_INDEX_META_V5_HEADER_SIZE
        .checked_add(
            child_count
                .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if children_end > encoded.len() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let children = encoded[ANN_INDEX_META_V5_HEADER_SIZE..children_end]
        .chunks_exact(ANN_INDEX_META_V4_CHILD_SIZE)
        .map(decode_child_descriptor)
        .collect::<Result<Vec<_>, _>>()?;
    let retained_generations =
        decode_v4_retained_generations(encoded, children_end, retained_count)?;
    validate_current_base_metadata(
        base_kind,
        build_identity,
        raw_input_identity,
        vector_count,
        graph_node_count,
        &children,
    )?;
    validate_retained_generations(&retained_generations, build_identity)?;
    Ok(PersistedIndexMetadata {
        build_identity,
        vector_count,
        base_kind,
        input_identity: (base_kind == PersistedBaseKind::Partitioned).then_some(raw_input_identity),
        children,
        view_identity,
        delta_count: legacy_count,
        delta_bytes: legacy_bytes,
        next_sequence,
        lifecycle,
        retained_generations,
        version: 5,
        overlay: Some(overlay),
    })
}

fn validate_overlay_metadata(
    legacy_count: u64,
    legacy_bytes: u64,
    next_sequence: u64,
    lifecycle: IncrementalVectorLifecycle,
    overlay: PersistedOverlayMetadata,
) -> Result<(), NativeRuntimeError> {
    let lifecycle_limit = u64::from(lifecycle.delta_max_entries);
    let valid_empty_overlay = overlay.overlay_count != 0
        || (overlay.overlay_bytes == 0
            && overlay.overlay_node_count == 0
            && overlay.overlay_root == overlay_empty_hash(0)
            && next_sequence == overlay.legacy_next_sequence);
    let valid_nonempty_overlay = overlay.overlay_count == 0
        || (overlay.overlay_bytes
            >= overlay
                .overlay_count
                .saturating_mul(ANN_DELTA_HEADER_SIZE as u64)
            && overlay.overlay_node_count >= u64::from(ANN_OVERLAY_TREE_DEPTH)
            && overlay.overlay_node_count
                <= overlay
                    .overlay_count
                    .saturating_mul(u64::from(ANN_OVERLAY_TREE_DEPTH))
            && next_sequence > overlay.legacy_next_sequence);
    if legacy_count > lifecycle_limit
        || legacy_bytes > MAX_ANN_DELTA_BYTES as u64
        || legacy_bytes < legacy_count.saturating_mul(ANN_DELTA_HEADER_SIZE as u64)
        || overlay.overlay_count > lifecycle_limit
        || overlay.overlay_bytes > MAX_ANN_DELTA_BYTES as u64
        || overlay.overlay_node_count > ANN_OVERLAY_MAX_NODES
        || overlay.effective_count > lifecycle_limit
        || overlay.effective_bytes > MAX_ANN_DELTA_BYTES as u64
        || overlay.effective_count < legacy_count.max(overlay.overlay_count)
        || overlay.effective_count > legacy_count.saturating_add(overlay.overlay_count)
        || overlay.legacy_next_sequence == 0
        || next_sequence < overlay.legacy_next_sequence
        || !valid_empty_overlay
        || !valid_nonempty_overlay
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn decode_v4_retained_generations(
    encoded: &[u8],
    mut offset: usize,
    count: usize,
) -> Result<Vec<RetainedGeneration>, NativeRuntimeError> {
    let mut retained_generations = Vec::with_capacity(count);
    for _ in 0..count {
        let header_end = offset
            .checked_add(ANN_INDEX_META_V4_RETAINED_HEADER_SIZE)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let header = encoded
            .get(offset..header_end)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        if header[34..40].iter().any(|byte| *byte != 0) {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        let retained_build_identity = header[..32]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        let retained_child_count = usize::from(u16::from_le_bytes(
            header[32..34]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ));
        let children_bytes = retained_child_count
            .checked_mul(ANN_INDEX_META_V4_CHILD_SIZE)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let generation_end = header_end
            .checked_add(children_bytes)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
        let children = encoded
            .get(header_end..generation_end)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?
            .chunks_exact(ANN_INDEX_META_V4_CHILD_SIZE)
            .map(decode_child_descriptor)
            .collect::<Result<Vec<_>, _>>()?;
        retained_generations.push(RetainedGeneration {
            build_identity: retained_build_identity,
            children,
        });
        offset = generation_end;
    }
    if offset != encoded.len() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(retained_generations)
}

fn decode_lifecycle(
    encoded: &[u8],
    version: u8,
) -> Result<IncrementalVectorLifecycle, NativeRuntimeError> {
    if version < 3 {
        return Ok(DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
    }
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: u32::from_le_bytes(
            encoded[144..148]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
        consolidate_after_deltas: u16::from_le_bytes(
            encoded[148..150]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
        retain_generations: u16::from_le_bytes(
            encoded[150..152]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
    };
    lifecycle
        .validate()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    Ok(lifecycle)
}

fn encode_child_descriptor(
    encoded: &mut Vec<u8>,
    snapshot: &IndexSnapshot,
) -> Result<(), NativeRuntimeError> {
    encode_persisted_child_descriptor(encoded, &PersistedChildDescriptor::from_snapshot(snapshot))
}

fn encode_persisted_child_descriptor(
    encoded: &mut Vec<u8>,
    descriptor: &PersistedChildDescriptor,
) -> Result<(), NativeRuntimeError> {
    if !descriptor.complete {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    encoded.extend_from_slice(&descriptor.build_identity);
    encoded.extend_from_slice(&descriptor.vector_count.to_le_bytes());
    encoded.extend_from_slice(&descriptor.graph_node_count.to_le_bytes());
    encoded.extend_from_slice(
        &descriptor
            .entry_point
            .map_or(0, ObjectId::get)
            .to_be_bytes(),
    );
    encoded.extend_from_slice(&descriptor.max_level.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    Ok(())
}

fn decode_child_descriptor(encoded: &[u8]) -> Result<PersistedChildDescriptor, NativeRuntimeError> {
    if encoded.len() != ANN_INDEX_META_V4_CHILD_SIZE
        || encoded[66..72].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let build_identity = encoded[..32]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    if build_identity == [0; 32] {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let raw_entry = u128::from_be_bytes(
        encoded[48..64]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    Ok(PersistedChildDescriptor {
        build_identity,
        vector_count: read_u64(&encoded[32..40]),
        graph_node_count: read_u64(&encoded[40..48]),
        entry_point: if raw_entry == 0 {
            None
        } else {
            Some(ObjectId::new(raw_entry).map_err(|_| NativeRuntimeError::InvalidAnnTree)?)
        },
        max_level: u16::from_le_bytes(
            encoded[64..66]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
        complete: true,
    })
}

fn validate_current_base_metadata(
    base_kind: PersistedBaseKind,
    build_identity: [u8; 32],
    input_identity: [u8; 32],
    vector_count: u64,
    graph_node_count: u64,
    children: &[PersistedChildDescriptor],
) -> Result<(), NativeRuntimeError> {
    let child_vector_count = children
        .iter()
        .try_fold(0_u64, |count, child| count.checked_add(child.vector_count));
    let child_graph_count = children.iter().try_fold(0_u64, |count, child| {
        count.checked_add(child.graph_node_count)
    });
    let unique_children = children
        .iter()
        .map(|child| child.build_identity)
        .collect::<BTreeSet<_>>();
    let valid_shape = match base_kind {
        PersistedBaseKind::Single => {
            input_identity == [0; 32]
                && matches!(children, [child] if child.build_identity == build_identity)
        }
        PersistedBaseKind::Partitioned => input_identity != [0; 32] && !children.is_empty(),
    };
    if !valid_shape
        || unique_children.len() != children.len()
        || child_vector_count != Some(vector_count)
        || child_graph_count != Some(graph_node_count)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

fn validate_retained_generations(
    generations: &[RetainedGeneration],
    current_build_identity: [u8; 32],
) -> Result<(), NativeRuntimeError> {
    let mut generation_identities = BTreeSet::new();
    for generation in generations {
        let child_identities = generation
            .children
            .iter()
            .map(|child| child.build_identity)
            .collect::<BTreeSet<_>>();
        if generation.build_identity == [0; 32]
            || generation.build_identity == current_build_identity
            || !generation_identities.insert(generation.build_identity)
            || generation.children.is_empty()
            || child_identities.len() != generation.children.len()
            || child_identities.contains(&[0; 32])
            || generation.children.iter().any(|child| {
                child.complete
                    && (child.vector_count == 0
                        || child.graph_node_count == 0
                        || child.vector_count != child.graph_node_count
                        || child.entry_point.is_none())
            })
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    Ok(())
}

fn decode_legacy_retained_generations(
    encoded: &[u8],
    version: u8,
    lifecycle: IncrementalVectorLifecycle,
    build_identity: [u8; 32],
) -> Result<Vec<RetainedGeneration>, NativeRuntimeError> {
    if version < 3 {
        return Ok(Vec::new());
    }
    if encoded[154..160].iter().any(|byte| *byte != 0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let count = usize::from(u16::from_le_bytes(
        encoded[152..154]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    let expected = ANN_INDEX_META_V3_SIZE
        .checked_add(count.saturating_mul(32))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if encoded.len() != expected || count > usize::from(lifecycle.retain_generations) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let identities = encoded[ANN_INDEX_META_V3_SIZE..]
        .chunks_exact(32)
        .map(|identity| {
            identity
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)
        })
        .collect::<Result<Vec<[u8; 32]>, _>>()?;
    let retained = identities
        .into_iter()
        .map(|identity| RetainedGeneration {
            build_identity: identity,
            children: vec![PersistedChildDescriptor::legacy(identity)],
        })
        .collect::<Vec<_>>();
    validate_retained_generations(&retained, build_identity)?;
    Ok(retained)
}

fn encode_vector_record(record: &VectorRecord) -> Result<Vec<u8>, NativeRuntimeError> {
    let dimension =
        u16::try_from(record.vector.dimension()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(
        ANN_VECTOR_HEADER_SIZE.saturating_add(usize::from(dimension).saturating_mul(4)),
    );
    encoded.extend_from_slice(ANN_VECTOR_MAGIC);
    encoded.extend_from_slice(&record.creating_csn.get().to_le_bytes());
    encoded.extend_from_slice(&dimension.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(&encode_vector_mutation(&record.vector));
    Ok(encoded)
}

fn decode_vector_record(
    encoded: &[u8],
    object_id: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<VectorRecord, NativeRuntimeError> {
    if encoded.len() < ANN_VECTOR_HEADER_SIZE
        || encoded.get(..8) != Some(ANN_VECTOR_MAGIC.as_slice())
        || encoded[18..24].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let dimension = u16::from_le_bytes(
        encoded[16..18]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    let expected = ANN_VECTOR_HEADER_SIZE
        .checked_add(usize::from(dimension).saturating_mul(4))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if dimension != definition.dimension() || encoded.len() != expected {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let record = VectorRecord {
        object_id,
        creating_csn: Csn::new(read_u64(&encoded[8..16]))
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        vector: decode_vector_mutation(&encoded[ANN_VECTOR_HEADER_SIZE..])
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    };
    validate_vector(definition, &record.vector).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    Ok(record)
}

fn encode_delta(delta: &DeltaRecord) -> Result<Vec<u8>, NativeRuntimeError> {
    let (kind, sequence, mutation_csn, dimension, vector) = match delta {
        DeltaRecord::Upsert { sequence, record } => (
            ANN_DELTA_UPSERT,
            *sequence,
            record.creating_csn,
            u16::try_from(record.vector.dimension())
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            Some(&record.vector),
        ),
        DeltaRecord::Tombstone {
            sequence,
            mutation_csn,
        } => (ANN_DELTA_TOMBSTONE, *sequence, *mutation_csn, 0, None),
    };
    let mut encoded = Vec::with_capacity(delta.encoded_len());
    encoded.extend_from_slice(ANN_DELTA_MAGIC);
    encoded.push(kind);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&sequence.to_le_bytes());
    encoded.extend_from_slice(&mutation_csn.get().to_le_bytes());
    encoded.extend_from_slice(&dimension.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    if let Some(vector) = vector {
        encoded.extend_from_slice(&encode_vector_mutation(vector));
    }
    Ok(encoded)
}

fn encode_delta_at_csn(
    delta: &DeltaRecord,
    mutation_csn: Csn,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let (kind, sequence, dimension, vector) = match delta {
        DeltaRecord::Upsert { sequence, record } => (
            ANN_DELTA_UPSERT,
            *sequence,
            u16::try_from(record.vector.dimension())
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
            Some(&record.vector),
        ),
        DeltaRecord::Tombstone { sequence, .. } => (ANN_DELTA_TOMBSTONE, *sequence, 0, None),
    };
    let mut encoded = Vec::with_capacity(delta.encoded_len());
    encoded.extend_from_slice(ANN_DELTA_MAGIC);
    encoded.push(kind);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&sequence.to_le_bytes());
    encoded.extend_from_slice(&mutation_csn.get().to_le_bytes());
    encoded.extend_from_slice(&dimension.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    if let Some(vector) = vector {
        encoded.extend_from_slice(&encode_vector_mutation(vector));
    }
    Ok(encoded)
}

fn decode_delta(
    encoded: &[u8],
    object_id: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<DeltaRecord, NativeRuntimeError> {
    decode_delta_with_magic(encoded, object_id, definition, *ANN_DELTA_MAGIC)
}

fn decode_overlay_delta(
    encoded: &[u8],
    object_id: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<DeltaRecord, NativeRuntimeError> {
    decode_delta_with_magic(encoded, object_id, definition, *ANN_OVERLAY_DELTA_MAGIC)
}

fn decode_delta_with_magic(
    encoded: &[u8],
    object_id: ObjectId,
    definition: VectorIndexDefinition,
    magic: [u8; 8],
) -> Result<DeltaRecord, NativeRuntimeError> {
    if encoded.len() < ANN_DELTA_HEADER_SIZE
        || encoded.get(..8) != Some(magic.as_slice())
        || encoded[9..16].iter().any(|byte| *byte != 0)
        || encoded[34..40].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let sequence = read_u64(&encoded[16..24]);
    let mutation_csn =
        Csn::new(read_u64(&encoded[24..32])).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let dimension = u16::from_le_bytes(
        encoded[32..34]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    if sequence == 0 {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    match encoded[8] {
        ANN_DELTA_UPSERT => {
            let expected = ANN_DELTA_HEADER_SIZE
                .checked_add(usize::from(dimension).saturating_mul(4))
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            if dimension != definition.dimension() || encoded.len() != expected {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            let vector = decode_vector_mutation(&encoded[ANN_DELTA_HEADER_SIZE..])
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
            validate_vector(definition, &vector).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
            Ok(DeltaRecord::Upsert {
                sequence,
                record: VectorRecord {
                    object_id,
                    creating_csn: mutation_csn,
                    vector,
                },
            })
        }
        ANN_DELTA_TOMBSTONE if dimension == 0 && encoded.len() == ANN_DELTA_HEADER_SIZE => {
            Ok(DeltaRecord::Tombstone {
                sequence,
                mutation_csn,
            })
        }
        _ => Err(NativeRuntimeError::InvalidAnnTree),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverlayNode {
    depth: u8,
    bitmap: u16,
    node_hash: [u8; 32],
    child_hashes: Vec<[u8; 32]>,
}

fn decode_overlay_node(encoded: &[u8], key_depth: u8) -> Result<OverlayNode, NativeRuntimeError> {
    if encoded.len() < ANN_OVERLAY_NODE_HEADER_SIZE
        || encoded.get(..8) != Some(ANN_OVERLAY_NODE_MAGIC.as_slice())
        || encoded[10..16].iter().any(|byte| *byte != 0)
        || encoded[8] != key_depth
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let child_count = usize::from(encoded[9]);
    let bitmap = u16::from_le_bytes(
        encoded[16..18]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    if child_count == 0
        || child_count > ANN_OVERLAY_FANOUT
        || bitmap.count_ones() as usize != child_count
        || encoded[18..24].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let expected = ANN_OVERLAY_NODE_HEADER_SIZE
        .checked_add(
            child_count
                .checked_mul(32)
                .ok_or(NativeRuntimeError::InvalidAnnTree)?,
        )
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if encoded.len() != expected {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let node_hash = encoded[24..56]
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let child_hashes = encoded[ANN_OVERLAY_NODE_HEADER_SIZE..]
        .chunks_exact(32)
        .map(|hash| {
            hash.try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)
        })
        .collect::<Result<Vec<[u8; 32]>, _>>()?;
    if child_hashes
        .iter()
        .any(|hash| *hash == overlay_empty_hash(key_depth.saturating_add(1)))
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let calculated = overlay_node_hash(key_depth, bitmap, &child_hashes)?;
    if node_hash != calculated {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(OverlayNode {
        depth: key_depth,
        bitmap,
        node_hash,
        child_hashes,
    })
}

fn encode_graph_layer(neighbors: &[ObjectId]) -> Result<Vec<u8>, NativeRuntimeError> {
    let count = u16::try_from(neighbors.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(
        ANN_GRAPH_LAYER_HEADER_SIZE.saturating_add(neighbors.len().saturating_mul(16)),
    );
    encoded.extend_from_slice(ANN_GRAPH_LAYER_MAGIC);
    encoded.extend_from_slice(&count.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    for neighbor in neighbors {
        encoded.extend_from_slice(&neighbor.get().to_be_bytes());
    }
    Ok(encoded)
}

fn decode_graph_layer(encoded: &[u8]) -> Result<Vec<ObjectId>, NativeRuntimeError> {
    if encoded.len() < ANN_GRAPH_LAYER_HEADER_SIZE
        || encoded.get(..8) != Some(ANN_GRAPH_LAYER_MAGIC.as_slice())
        || encoded[10..16].iter().any(|byte| *byte != 0)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let count = usize::from(u16::from_le_bytes(
        encoded[8..10]
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    ));
    let expected = ANN_GRAPH_LAYER_HEADER_SIZE
        .checked_add(count.saturating_mul(16))
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    if encoded.len() != expected {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    encoded[ANN_GRAPH_LAYER_HEADER_SIZE..]
        .chunks_exact(16)
        .map(decode_index)
        .collect()
}

fn graph_node(
    object_id: ObjectId,
    layers: BTreeMap<u16, Vec<ObjectId>>,
) -> Result<GraphNodeRecord, NativeRuntimeError> {
    let Some((&level, _)) = layers.last_key_value() else {
        return Err(NativeRuntimeError::InvalidAnnTree);
    };
    if layers.keys().copied().ne(0..=level) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(GraphNodeRecord {
        object_id,
        level,
        neighbors: layers.into_values().collect(),
    })
}

fn calculate_view_identity(
    base_identity: [u8; 32],
    next_sequence: u64,
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
) -> [u8; 32] {
    calculate_view_identity_at_csn(base_identity, next_sequence, deltas, None)
}

fn calculate_view_identity_at_csn(
    base_identity: [u8; 32],
    next_sequence: u64,
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
    staged: Option<(&BTreeSet<ObjectId>, Csn)>,
) -> [u8; 32] {
    if deltas.is_empty() {
        return base_identity;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-base-delta-view-v1");
    hasher.update(&base_identity);
    hasher.update(&next_sequence.to_le_bytes());
    for (object_id, delta) in deltas {
        hasher.update(&object_id.get().to_be_bytes());
        hasher.update(&delta.sequence().to_le_bytes());
        match delta {
            DeltaRecord::Upsert { record, .. } => {
                hasher.update(&[ANN_DELTA_UPSERT]);
                let creating_csn = staged
                    .filter(|(objects, _)| objects.contains(object_id))
                    .map_or(record.creating_csn, |(_, creating_csn)| creating_csn);
                hasher.update(&creating_csn.get().to_le_bytes());
                hasher.update(&encode_vector_mutation(&record.vector));
            }
            DeltaRecord::Tombstone { mutation_csn, .. } => {
                hasher.update(&[ANN_DELTA_TOMBSTONE]);
                let mutation_csn = staged
                    .filter(|(objects, _)| objects.contains(object_id))
                    .map_or(*mutation_csn, |(_, creating_csn)| creating_csn);
                hasher.update(&mutation_csn.get().to_le_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
}

fn overlay_empty_hash(depth: u8) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-overlay-empty-v1");
    hasher.update(&[depth]);
    *hasher.finalize().as_bytes()
}

fn overlay_leaf_hash(object_id: ObjectId, encoded: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-overlay-leaf-v1");
    hasher.update(&object_id.get().to_be_bytes());
    hasher.update(
        &u64::try_from(encoded.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(encoded);
    *hasher.finalize().as_bytes()
}

fn overlay_node_hash(
    depth: u8,
    bitmap: u16,
    child_hashes: &[[u8; 32]],
) -> Result<[u8; 32], NativeRuntimeError> {
    if depth >= ANN_OVERLAY_TREE_DEPTH
        || bitmap == 0
        || bitmap.count_ones() as usize != child_hashes.len()
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let empty = overlay_empty_hash(depth.saturating_add(1));
    let mut present = child_hashes.iter();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-overlay-node-v1");
    hasher.update(&[depth]);
    hasher.update(&bitmap.to_le_bytes());
    for position in 0..ANN_OVERLAY_FANOUT {
        if bitmap & (1_u16 << position) == 0 {
            hasher.update(&empty);
        } else {
            let child = present.next().ok_or(NativeRuntimeError::InvalidAnnTree)?;
            if *child == empty {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
            hasher.update(child);
        }
    }
    if present.next().is_some() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn overlay_view_identity(base_identity: [u8; 32], manifest: OverlayManifest) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-overlay-view-v1");
    hasher.update(&base_identity);
    hasher.update(&manifest.legacy_view_identity);
    hasher.update(&manifest.overlay_root);
    hasher.update(&manifest.legacy_count.to_le_bytes());
    hasher.update(&manifest.legacy_bytes.to_le_bytes());
    hasher.update(&manifest.overlay_count.to_le_bytes());
    hasher.update(&manifest.overlay_bytes.to_le_bytes());
    hasher.update(&manifest.overlay_node_count.to_le_bytes());
    hasher.update(&manifest.effective_count.to_le_bytes());
    hasher.update(&manifest.effective_bytes.to_le_bytes());
    hasher.update(&manifest.legacy_next_sequence.to_le_bytes());
    hasher.update(&manifest.next_sequence.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn overlay_path_prefix(path: u128, depth: u8) -> u128 {
    if depth == 0 {
        0
    } else {
        path & (u128::MAX << (u32::from(ANN_OVERLAY_TREE_DEPTH - depth) * 4))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedOverlayNode {
    path: u128,
    node: OverlayNode,
}

type OverlayFrontier = BTreeMap<u128, [u8; 32]>;
type ExpectedOverlayLevel = (OverlayFrontier, Vec<ExpectedOverlayNode>);

fn expected_overlay_level(
    frontier: &OverlayFrontier,
    depth: u8,
) -> Result<ExpectedOverlayLevel, NativeRuntimeError> {
    if depth >= ANN_OVERLAY_TREE_DEPTH || frontier.is_empty() {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let mut grouped = BTreeMap::<u128, [Option<[u8; 32]>; ANN_OVERLAY_FANOUT]>::new();
    let shift = u32::from(ANN_OVERLAY_TREE_DEPTH - depth - 1) * 4;
    for (path, hash) in frontier {
        let parent = overlay_path_prefix(*path, depth);
        let position = usize::try_from((path >> shift) & 0x0f)
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        let child = &mut grouped.entry(parent).or_insert([None; ANN_OVERLAY_FANOUT])[position];
        if child.replace(*hash).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }
    let mut parents = BTreeMap::new();
    let mut expected = Vec::with_capacity(grouped.len());
    for (path, children) in grouped {
        let mut bitmap = 0_u16;
        let mut child_hashes = Vec::new();
        for (position, child) in children.into_iter().enumerate() {
            if let Some(child) = child {
                bitmap |= 1_u16 << position;
                child_hashes.push(child);
            }
        }
        let node_hash = overlay_node_hash(depth, bitmap, &child_hashes)?;
        if parents.insert(path, node_hash).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        expected.push(ExpectedOverlayNode {
            path,
            node: OverlayNode {
                depth,
                bitmap,
                node_hash,
                child_hashes,
            },
        });
    }
    Ok((parents, expected))
}

fn validate_vector(
    definition: VectorIndexDefinition,
    vector: &Vector,
) -> Result<(), NativeRuntimeError> {
    if vector.dimension() != usize::from(definition.dimension()) {
        return Err(hyphae_native_ann::AnnError::DimensionMismatch.into());
    }
    if definition.metric() == Metric::Cosine
        && vector.values().iter().all(|component| *component == 0.0)
    {
        return Err(hyphae_native_ann::AnnError::ZeroCosineVector.into());
    }
    Ok(())
}

fn distance(metric: Metric, left: &Vector, right: &Vector) -> Result<f64, NativeRuntimeError> {
    if left.dimension() != right.dimension() {
        return Err(hyphae_native_ann::AnnError::DimensionMismatch.into());
    }
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    let mut squared_l2 = 0.0_f64;
    for (left, right) in left.values().iter().zip(right.values()) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
        let difference = left - right;
        squared_l2 += difference * difference;
    }
    match metric {
        Metric::Cosine if left_norm == 0.0 || right_norm == 0.0 => {
            Err(hyphae_native_ann::AnnError::ZeroCosineVector.into())
        }
        Metric::Cosine => Ok(1.0 - dot / (left_norm.sqrt() * right_norm.sqrt())),
        Metric::NegativeDot => Ok(-dot),
        Metric::SquaredL2 => Ok(squared_l2),
    }
}

fn sort_hits(hits: &mut [VectorHit]) {
    hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.object_id.cmp(&right.object_id))
    });
}

fn read_u64(encoded: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(encoded);
    u64::from_le_bytes(value)
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) enum TestOverlayMutation {
    Upsert(ObjectId, Vector),
    Tombstone(ObjectId),
}

#[cfg(test)]
pub(crate) struct TestM05Installation {
    pub(crate) tree: BTree,
    pub(crate) view_identity: [u8; 32],
    pub(crate) node_count: usize,
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
pub(crate) fn install_test_m05_tree(
    pages: &mut PageStore,
    buffer_pool: &BufferPool,
    root: PageId,
    definition: VectorIndexDefinition,
    mutation_csn: Csn,
    mutations: &[TestOverlayMutation],
) -> Result<TestM05Installation, NativeRuntimeError> {
    let plan = plan_index_load(pages, buffer_pool, root, definition.index_id(), definition)?;
    let (state, _, _) = load_planned_index_with_entries(pages, buffer_pool, &plan, None)?;
    if state.persisted_version != 4 {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let legacy_next_sequence = state.next_sequence;
    let legacy_view_identity = calculate_view_identity(
        state.base.build_identity(),
        legacy_next_sequence,
        &state.deltas,
    );
    let mut sequence = legacy_next_sequence;
    let mut overlay_deltas = BTreeMap::new();
    for mutation in mutations {
        let (object_id, delta) = match mutation {
            TestOverlayMutation::Upsert(object_id, vector) => {
                validate_vector(definition, vector)?;
                (
                    *object_id,
                    DeltaRecord::Upsert {
                        sequence,
                        record: VectorRecord {
                            object_id: *object_id,
                            creating_csn: mutation_csn,
                            vector: vector.clone(),
                        },
                    },
                )
            }
            TestOverlayMutation::Tombstone(object_id) => (
                *object_id,
                DeltaRecord::Tombstone {
                    sequence,
                    mutation_csn,
                },
            ),
        };
        if overlay_deltas.insert(object_id, delta).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        sequence = sequence
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    }
    let mut replacements = BTreeMap::new();
    let mut frontier = BTreeMap::new();
    for (object_id, delta) in &overlay_deltas {
        let encoded = test_encode_overlay_delta(delta)?;
        frontier.insert(object_id.get(), overlay_leaf_hash(*object_id, &encoded));
        replacements.insert(
            overlay_delta_key(definition.index_id(), *object_id),
            encoded,
        );
    }
    let mut node_count = 0_u64;
    if !frontier.is_empty() {
        for depth in (0..ANN_OVERLAY_TREE_DEPTH).rev() {
            let (parents, nodes) = expected_overlay_level(&frontier, depth)?;
            for node in &nodes {
                replacements.insert(
                    overlay_node_key(definition.index_id(), depth, node.path),
                    test_encode_overlay_node(&node.node)?,
                );
            }
            node_count = node_count
                .checked_add(
                    u64::try_from(nodes.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
                )
                .ok_or(NativeRuntimeError::InvalidAnnTree)?;
            frontier = parents;
        }
    }
    let overlay_root = if overlay_deltas.is_empty() {
        overlay_empty_hash(0)
    } else {
        *frontier.get(&0).ok_or(NativeRuntimeError::InvalidAnnTree)?
    };
    let mut effective = state.deltas.clone();
    effective.extend(overlay_deltas.clone());
    let mut manifest = OverlayManifest {
        legacy_view_identity,
        overlay_root,
        view_identity: [0; 32],
        legacy_count: u64::try_from(state.deltas.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        legacy_bytes: delta_map_bytes(&state.deltas)?,
        overlay_count: u64::try_from(overlay_deltas.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        overlay_bytes: delta_map_bytes(&overlay_deltas)?,
        overlay_node_count: node_count,
        effective_count: u64::try_from(effective.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        effective_bytes: delta_map_bytes(&effective)?,
        legacy_next_sequence,
        next_sequence: sequence,
    };
    manifest.view_identity = overlay_view_identity(state.base.build_identity(), manifest);
    replacements.insert(
        meta_key(definition.index_id()),
        test_encode_metadata_v5(&state, manifest)?,
    );
    replacements.insert(
        overlay_manifest_key(definition.index_id()),
        test_encode_overlay_manifest(manifest),
    );
    let tree = BTree::from_root(root)
        .upsert_sorted_batch(pages, mutation_csn, replacements.into_iter().collect())?
        .tree;
    Ok(TestM05Installation {
        tree,
        view_identity: manifest.view_identity,
        node_count: usize::try_from(node_count).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    })
}

#[cfg(test)]
pub(crate) fn verify_test_m05_rejects_initial_bulk(
    pages: &mut PageStore,
    root: PageId,
    catalog: &CatalogState,
    definition: VectorIndexDefinition,
    creating_csn: Csn,
) -> Result<(), NativeRuntimeError> {
    match capture_initial_bulk_authority(pages, root, catalog, definition.index_id()) {
        Err(NativeRuntimeError::InvalidPreparedMutation) => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(NativeRuntimeError::InvalidAnnTree),
    }
    let current = load_from_tree(pages, Some(root), catalog, true)?
        .indexes
        .remove(&definition.index_id())
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let record = VectorRecord {
        object_id: ObjectId::new(u128::MAX).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        creating_csn,
        vector: Vector::new(std::iter::repeat_n(
            1.0,
            usize::from(definition.dimension()),
        ))?,
    };
    let plan = HnswPartitionPlan::build(definition, [record], 1)?;
    let candidate = PartitionedHnswIndex::build(&plan)?.export_snapshot();
    let publication = crate::InitialAnnBulkPublication {
        index: definition.index_id(),
        expected_base_identity: current.base.build_identity(),
        expected_view_identity: current.view_identity,
        candidate_csn: creating_csn,
        candidate,
    };
    let pages_before = pages.page_count();
    match publish_initial_bulk_tree(pages, Some(root), creating_csn, catalog, &publication) {
        Err(NativeRuntimeError::InitialAnnBulkStale) => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(NativeRuntimeError::InvalidAnnTree),
    }
    if pages.page_count() != pages_before {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    Ok(())
}

#[cfg(test)]
fn test_encode_overlay_delta(delta: &DeltaRecord) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut encoded = encode_delta(delta)?;
    encoded[..8].copy_from_slice(ANN_OVERLAY_DELTA_MAGIC);
    Ok(encoded)
}

#[cfg(test)]
fn test_encode_overlay_node(node: &OverlayNode) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut encoded = Vec::with_capacity(
        ANN_OVERLAY_NODE_HEADER_SIZE.saturating_add(node.child_hashes.len().saturating_mul(32)),
    );
    encoded.extend_from_slice(ANN_OVERLAY_NODE_MAGIC);
    encoded.push(node.depth);
    encoded.push(
        u8::try_from(node.child_hashes.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
    );
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(&node.bitmap.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(&node.node_hash);
    for child in &node.child_hashes {
        encoded.extend_from_slice(child);
    }
    Ok(encoded)
}

#[cfg(test)]
fn test_encode_overlay_manifest(manifest: OverlayManifest) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(ANN_OVERLAY_MANIFEST_SIZE);
    encoded.extend_from_slice(ANN_OVERLAY_MANIFEST_MAGIC);
    encoded.extend_from_slice(&manifest.legacy_view_identity);
    encoded.extend_from_slice(&manifest.overlay_root);
    encoded.extend_from_slice(&manifest.view_identity);
    encoded.extend_from_slice(&manifest.legacy_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.legacy_bytes.to_le_bytes());
    encoded.extend_from_slice(&manifest.overlay_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.overlay_bytes.to_le_bytes());
    encoded.extend_from_slice(&manifest.overlay_node_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.effective_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.effective_bytes.to_le_bytes());
    encoded.extend_from_slice(&manifest.legacy_next_sequence.to_le_bytes());
    encoded.extend_from_slice(&manifest.next_sequence.to_le_bytes());
    encoded.extend_from_slice(&[0; 8]);
    encoded
}

#[cfg(test)]
fn test_encode_metadata_v5(
    state: &AnnIndexState,
    manifest: OverlayManifest,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let current = encode_metadata(state)?;
    let mut encoded = current[..ANN_INDEX_META_V4_HEADER_SIZE].to_vec();
    encoded[..8].copy_from_slice(ANN_INDEX_META_MAGIC_V5);
    encoded[40..72].copy_from_slice(&manifest.view_identity);
    encoded[120..128].copy_from_slice(&manifest.legacy_count.to_le_bytes());
    encoded[128..136].copy_from_slice(&manifest.legacy_bytes.to_le_bytes());
    encoded[136..144].copy_from_slice(&manifest.next_sequence.to_le_bytes());
    encoded.extend_from_slice(&manifest.legacy_view_identity);
    encoded.extend_from_slice(&manifest.overlay_root);
    encoded.extend_from_slice(&manifest.overlay_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.overlay_bytes.to_le_bytes());
    encoded.extend_from_slice(&manifest.overlay_node_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.effective_count.to_le_bytes());
    encoded.extend_from_slice(&manifest.effective_bytes.to_le_bytes());
    encoded.extend_from_slice(&manifest.legacy_next_sequence.to_le_bytes());
    encoded.extend_from_slice(&[0; 8]);
    encoded.extend_from_slice(&current[ANN_INDEX_META_V4_HEADER_SIZE..]);
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use hyphae_native_ann::HnswPartitionPlan;

    use super::*;

    fn definition() -> Result<VectorIndexDefinition, Box<dyn std::error::Error>> {
        Ok(VectorIndexDefinition::new(
            ObjectId::new(11)?,
            2,
            Metric::SquaredL2,
            HnswConfig::new(4, 16, 4, 32, 7)?,
        )?)
    }

    fn partitioned_index() -> Result<PartitionedHnswIndex, Box<dyn std::error::Error>> {
        let definition = definition()?;
        let creating_csn = Csn::new(3)?;
        let records = [[0.0, 0.0], [0.0, 1.0], [10.0, 10.0], [10.0, 11.0]]
            .into_iter()
            .enumerate()
            .map(|(position, values)| {
                Ok(VectorRecord {
                    object_id: ObjectId::new(u128::try_from(position)? + 1)?,
                    creating_csn,
                    vector: Vector::new(values)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let plan = HnswPartitionPlan::build(definition, records, 2)?;
        Ok(PartitionedHnswIndex::build(&plan)?)
    }

    fn partitioned_state() -> Result<AnnIndexState, Box<dyn std::error::Error>> {
        let base = AnnBase::Partitioned(partitioned_index()?);
        Ok(AnnIndexState {
            view_identity: base.build_identity(),
            base,
            deltas: BTreeMap::new(),
            next_sequence: 1,
            lifecycle: DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE,
            retained_generations: Vec::new(),
            persisted_version: 4,
        })
    }

    fn encode_legacy_metadata(
        snapshot: &IndexSnapshot,
        version: u8,
    ) -> Result<Vec<u8>, NativeRuntimeError> {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(match version {
            1 => ANN_INDEX_META_MAGIC_V1,
            2 => ANN_INDEX_META_MAGIC_V2,
            3 => ANN_INDEX_META_MAGIC_V3,
            _ => return Err(NativeRuntimeError::InvalidAnnTree),
        });
        encoded.extend_from_slice(&snapshot.build_identity);
        encoded.extend_from_slice(
            &u64::try_from(snapshot.vectors.len())
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &u64::try_from(snapshot.nodes.len())
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&snapshot.entry_point.map_or(0, ObjectId::get).to_be_bytes());
        encoded.extend_from_slice(&snapshot.max_level.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        if version >= 2 {
            encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V1);
            encoded.extend_from_slice(&snapshot.build_identity);
            encoded.extend_from_slice(&0_u64.to_le_bytes());
            encoded.extend_from_slice(&0_u64.to_le_bytes());
            encoded.extend_from_slice(&1_u64.to_le_bytes());
        }
        if version >= 3 {
            encoded.extend_from_slice(
                &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                    .delta_max_entries
                    .to_le_bytes(),
            );
            encoded.extend_from_slice(
                &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                    .consolidate_after_deltas
                    .to_le_bytes(),
            );
            encoded.extend_from_slice(
                &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                    .retain_generations
                    .to_le_bytes(),
            );
            encoded.extend_from_slice(&0_u16.to_le_bytes());
            encoded.extend_from_slice(&[0; 6]);
        }
        Ok(encoded)
    }

    #[derive(Clone)]
    struct OverlayFixture {
        definition: VectorIndexDefinition,
        metadata: Vec<u8>,
        entries: Vec<KeyValue>,
        legacy_objects: BTreeSet<ObjectId>,
        overlay_objects: BTreeSet<ObjectId>,
    }

    fn encode_overlay_delta_for_test(delta: &DeltaRecord) -> Result<Vec<u8>, NativeRuntimeError> {
        let mut encoded = encode_delta(delta)?;
        encoded[..8].copy_from_slice(ANN_OVERLAY_DELTA_MAGIC);
        Ok(encoded)
    }

    fn encode_overlay_node_for_test(node: &OverlayNode) -> Result<Vec<u8>, NativeRuntimeError> {
        let child_count = u8::try_from(node.child_hashes.len())
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        let mut encoded = Vec::with_capacity(
            ANN_OVERLAY_NODE_HEADER_SIZE.saturating_add(node.child_hashes.len().saturating_mul(32)),
        );
        encoded.extend_from_slice(ANN_OVERLAY_NODE_MAGIC);
        encoded.push(node.depth);
        encoded.push(child_count);
        encoded.extend_from_slice(&[0; 6]);
        encoded.extend_from_slice(&node.bitmap.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        encoded.extend_from_slice(&node.node_hash);
        for child in &node.child_hashes {
            encoded.extend_from_slice(child);
        }
        if encoded.len()
            != ANN_OVERLAY_NODE_HEADER_SIZE.saturating_add(node.child_hashes.len() * 32)
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
        Ok(encoded)
    }

    fn encode_overlay_manifest_for_test(manifest: OverlayManifest) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(ANN_OVERLAY_MANIFEST_SIZE);
        encoded.extend_from_slice(ANN_OVERLAY_MANIFEST_MAGIC);
        encoded.extend_from_slice(&manifest.legacy_view_identity);
        encoded.extend_from_slice(&manifest.overlay_root);
        encoded.extend_from_slice(&manifest.view_identity);
        encoded.extend_from_slice(&manifest.legacy_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.legacy_bytes.to_le_bytes());
        encoded.extend_from_slice(&manifest.overlay_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.overlay_bytes.to_le_bytes());
        encoded.extend_from_slice(&manifest.overlay_node_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.effective_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.effective_bytes.to_le_bytes());
        encoded.extend_from_slice(&manifest.legacy_next_sequence.to_le_bytes());
        encoded.extend_from_slice(&manifest.next_sequence.to_le_bytes());
        encoded.extend_from_slice(&[0; 8]);
        encoded
    }

    fn encode_metadata_v5_for_test(
        state: &AnnIndexState,
        manifest: OverlayManifest,
    ) -> Result<Vec<u8>, NativeRuntimeError> {
        let current = encode_metadata(state)?;
        let mut encoded = current[..ANN_INDEX_META_V4_HEADER_SIZE].to_vec();
        encoded[..8].copy_from_slice(ANN_INDEX_META_MAGIC_V5);
        encoded[40..72].copy_from_slice(&manifest.view_identity);
        encoded[120..128].copy_from_slice(&manifest.legacy_count.to_le_bytes());
        encoded[128..136].copy_from_slice(&manifest.legacy_bytes.to_le_bytes());
        encoded[136..144].copy_from_slice(&manifest.next_sequence.to_le_bytes());
        encoded.extend_from_slice(&manifest.legacy_view_identity);
        encoded.extend_from_slice(&manifest.overlay_root);
        encoded.extend_from_slice(&manifest.overlay_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.overlay_bytes.to_le_bytes());
        encoded.extend_from_slice(&manifest.overlay_node_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.effective_count.to_le_bytes());
        encoded.extend_from_slice(&manifest.effective_bytes.to_le_bytes());
        encoded.extend_from_slice(&manifest.legacy_next_sequence.to_le_bytes());
        encoded.extend_from_slice(&[0; 8]);
        encoded.extend_from_slice(&current[ANN_INDEX_META_V4_HEADER_SIZE..]);
        Ok(encoded)
    }

    #[allow(clippy::too_many_lines)]
    fn overlay_fixture() -> Result<OverlayFixture, Box<dyn std::error::Error>> {
        let definition = definition()?;
        let base = HnswIndex::build(
            definition,
            [
                VectorRecord {
                    object_id: ObjectId::new(1)?,
                    creating_csn: Csn::new(1)?,
                    vector: Vector::new([1.0, 1.0])?,
                },
                VectorRecord {
                    object_id: ObjectId::new(2)?,
                    creating_csn: Csn::new(1)?,
                    vector: Vector::new([2.0, 2.0])?,
                },
                VectorRecord {
                    object_id: ObjectId::new(3)?,
                    creating_csn: Csn::new(1)?,
                    vector: Vector::new([3.0, 3.0])?,
                },
            ],
        )?;
        let state = AnnIndexState::new(base, DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
        let legacy_deltas = BTreeMap::from([
            (
                ObjectId::new(1)?,
                DeltaRecord::Upsert {
                    sequence: 1,
                    record: VectorRecord {
                        object_id: ObjectId::new(1)?,
                        creating_csn: Csn::new(2)?,
                        vector: Vector::new([4.0, 4.0])?,
                    },
                },
            ),
            (
                ObjectId::new(4)?,
                DeltaRecord::Upsert {
                    sequence: 2,
                    record: VectorRecord {
                        object_id: ObjectId::new(4)?,
                        creating_csn: Csn::new(2)?,
                        vector: Vector::new([5.0, 5.0])?,
                    },
                },
            ),
        ]);
        let overlay_deltas = BTreeMap::from([
            (
                ObjectId::new(1)?,
                DeltaRecord::Tombstone {
                    sequence: 3,
                    mutation_csn: Csn::new(3)?,
                },
            ),
            (
                ObjectId::new(4)?,
                DeltaRecord::Upsert {
                    sequence: 4,
                    record: VectorRecord {
                        object_id: ObjectId::new(4)?,
                        creating_csn: Csn::new(3)?,
                        vector: Vector::new([0.5, 0.5])?,
                    },
                },
            ),
            (
                ObjectId::new(5)?,
                DeltaRecord::Upsert {
                    sequence: 5,
                    record: VectorRecord {
                        object_id: ObjectId::new(5)?,
                        creating_csn: Csn::new(3)?,
                        vector: Vector::new([6.0, 6.0])?,
                    },
                },
            ),
        ]);
        let legacy_next_sequence = 3;
        let next_sequence = 6;
        let legacy_view_identity = calculate_view_identity(
            state.base.build_identity(),
            legacy_next_sequence,
            &legacy_deltas,
        );
        let mut entries = BTreeMap::new();
        append_base_generation_entries(&mut entries, &state.base)?;
        for (object_id, delta) in &legacy_deltas {
            entries.insert(
                delta_key(definition.index_id(), *object_id),
                encode_delta(delta)?,
            );
        }
        let mut frontier = BTreeMap::new();
        for (object_id, delta) in &overlay_deltas {
            let encoded = encode_overlay_delta_for_test(delta)?;
            frontier.insert(object_id.get(), overlay_leaf_hash(*object_id, &encoded));
            entries.insert(
                overlay_delta_key(definition.index_id(), *object_id),
                encoded,
            );
        }
        let mut overlay_node_count = 0_u64;
        for depth in (0..ANN_OVERLAY_TREE_DEPTH).rev() {
            let (parents, nodes) = expected_overlay_level(&frontier, depth)?;
            for node in &nodes {
                entries.insert(
                    overlay_node_key(definition.index_id(), depth, node.path),
                    encode_overlay_node_for_test(&node.node)?,
                );
            }
            overlay_node_count = overlay_node_count
                .checked_add(u64::try_from(nodes.len())?)
                .ok_or("overlay node count overflow")?;
            frontier = parents;
        }
        let overlay_root = *frontier.get(&0).ok_or("missing overlay root")?;
        let mut effective = legacy_deltas.clone();
        effective.extend(overlay_deltas.clone());
        let mut manifest = OverlayManifest {
            legacy_view_identity,
            overlay_root,
            view_identity: [0; 32],
            legacy_count: u64::try_from(legacy_deltas.len())?,
            legacy_bytes: delta_map_bytes(&legacy_deltas)?,
            overlay_count: u64::try_from(overlay_deltas.len())?,
            overlay_bytes: delta_map_bytes(&overlay_deltas)?,
            overlay_node_count,
            effective_count: u64::try_from(effective.len())?,
            effective_bytes: delta_map_bytes(&effective)?,
            legacy_next_sequence,
            next_sequence,
        };
        manifest.view_identity = overlay_view_identity(state.base.build_identity(), manifest);
        entries.insert(
            overlay_manifest_key(definition.index_id()),
            encode_overlay_manifest_for_test(manifest),
        );
        Ok(OverlayFixture {
            definition,
            metadata: encode_metadata_v5_for_test(&state, manifest)?,
            entries: entries.into_iter().collect(),
            legacy_objects: legacy_deltas.keys().copied().collect(),
            overlay_objects: overlay_deltas.keys().copied().collect(),
        })
    }

    fn restore_overlay_fixture(
        fixture: &OverlayFixture,
    ) -> Result<AnnIndexState, NativeRuntimeError> {
        restore_index_with_definition(
            &fixture.entries,
            fixture.definition.index_id(),
            fixture.definition,
            decode_metadata(&fixture.metadata)?,
        )
    }

    #[test]
    fn metadata_v5_composes_frozen_legacy_overlay_and_base_for_both_query_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        let metadata = decode_metadata(&fixture.metadata)?;
        let overlay = metadata.overlay.ok_or("missing overlay metadata")?;
        assert_eq!(metadata.version, 5);
        assert_eq!(metadata.delta_count, 2);
        assert_eq!(overlay.overlay_count, 3);
        assert_eq!(overlay.effective_count, 3);
        assert_eq!(
            fixture.legacy_objects,
            [ObjectId::new(1)?, ObjectId::new(4)?].into()
        );
        assert_eq!(
            fixture.overlay_objects,
            [ObjectId::new(1)?, ObjectId::new(4)?, ObjectId::new(5)?].into()
        );

        let mut restored = restore_overlay_fixture(&fixture)?;
        let records = restored.effective_vectors();
        assert_eq!(
            records
                .iter()
                .map(|record| record.object_id)
                .collect::<Vec<_>>(),
            [
                ObjectId::new(2)?,
                ObjectId::new(3)?,
                ObjectId::new(4)?,
                ObjectId::new(5)?,
            ]
        );
        assert_eq!(records[2].vector, Vector::new([0.5, 0.5])?);
        let query = Vector::new([0.0, 0.0])?;
        let exact = restored.search_exact(&query, 4, None)?;
        let approximate = restored.search(&query, SearchOptions::new(4, 32, Some(32))?, None)?;
        assert_eq!(approximate.hits, exact);
        assert_eq!(approximate.build_identity, metadata.view_identity);

        assert!(matches!(
            restored.upsert(ObjectId::new(9)?, Csn::new(9)?, Vector::new([9.0, 9.0])?),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        assert!(matches!(
            restored.delete(ObjectId::new(2)?, Csn::new(9)?),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        assert!(encode_metadata(&restored).is_err());
        Ok(())
    }

    #[test]
    fn overlay_codecs_reject_every_header_path_truncation_and_trailing_byte()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        for length in 0..fixture.metadata.len() {
            assert!(decode_metadata(&fixture.metadata[..length]).is_err());
        }
        let mut trailing_metadata = fixture.metadata.clone();
        trailing_metadata.push(0);
        assert!(decode_metadata(&trailing_metadata).is_err());
        let mut unsupported_metadata = fixture.metadata.clone();
        unsupported_metadata[7] = b'6';
        assert!(decode_metadata(&unsupported_metadata).is_err());

        let (_, manifest) = fixture
            .entries
            .iter()
            .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_MANIFEST_PREFIX))
            .ok_or("missing manifest")?;
        for length in 0..manifest.len() {
            assert!(decode_overlay_manifest(&manifest[..length]).is_err());
        }
        let mut trailing_manifest = manifest.clone();
        trailing_manifest.push(0);
        assert!(decode_overlay_manifest(&trailing_manifest).is_err());

        let (leaf_key, leaf) = fixture
            .entries
            .iter()
            .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_DELTA_PREFIX))
            .ok_or("missing overlay leaf")?;
        let (_, object_id) = decode_overlay_delta_key(leaf_key)?;
        for length in 0..leaf.len() {
            assert!(decode_overlay_delta(&leaf[..length], object_id, fixture.definition).is_err());
        }
        let mut trailing_leaf = leaf.clone();
        trailing_leaf.push(0);
        assert!(decode_overlay_delta(&trailing_leaf, object_id, fixture.definition).is_err());

        let (node_key, node) = fixture
            .entries
            .iter()
            .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
            .ok_or("missing overlay node")?;
        let (_, depth, _) = decode_overlay_node_key(node_key)?;
        for length in 0..node.len() {
            assert!(decode_overlay_node(&node[..length], depth).is_err());
        }
        let mut trailing_node = node.clone();
        trailing_node.push(0);
        assert!(decode_overlay_node(&trailing_node, depth).is_err());
        Ok(())
    }

    #[test]
    fn metadata_and_manifest_corruption_matrix_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        for offset in [
            0, 8, 40, 72, 104, 112, 120, 128, 136, 144, 148, 150, 152, 153, 154, 156, 158, 160,
            192, 224, 232, 240, 248, 256, 264, 272, 280, 312, 320, 328, 344, 346,
        ] {
            let mut corrupted = fixture.clone();
            if offset == 148 {
                corrupted.metadata[148..150].fill(0);
            } else {
                corrupted.metadata[offset] ^= 1;
            }
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "metadata offset {offset}"
            );
        }
        for offset in [
            0, 8, 40, 72, 104, 112, 120, 128, 136, 144, 152, 160, 168, 176,
        ] {
            let mut corrupted = fixture.clone();
            let manifest = corrupted
                .entries
                .iter_mut()
                .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_MANIFEST_PREFIX))
                .ok_or("missing manifest")?;
            manifest.1[offset] ^= 1;
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "manifest offset {offset}"
            );
        }
        let mut missing = fixture.clone();
        missing
            .entries
            .retain(|(key, _)| key.first() != Some(&ANN_OVERLAY_MANIFEST_PREFIX));
        assert!(restore_overlay_fixture(&missing).is_err());
        Ok(())
    }

    #[test]
    fn overlay_leaf_corruption_matrix_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        for key_offset in [0, 1, 32] {
            let mut corrupted = fixture.clone();
            let leaf = corrupted
                .entries
                .iter_mut()
                .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_DELTA_PREFIX))
                .ok_or("missing overlay leaf")?;
            leaf.0[key_offset] ^= 1;
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "leaf key {key_offset}"
            );
        }
        for value_offset in [0, 8, 9, 16, 24, 32, 34, 40] {
            let mut corrupted = fixture.clone();
            let leaf = corrupted
                .entries
                .iter_mut()
                .find(|(key, _)| {
                    key.first() == Some(&ANN_OVERLAY_DELTA_PREFIX) && key.last() == Some(&4)
                })
                .ok_or("missing overlay upsert")?;
            leaf.1[value_offset] ^= 1;
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "leaf value {value_offset}"
            );
        }
        let mut duplicate_sequence = fixture.clone();
        let leaf = duplicate_sequence
            .entries
            .iter_mut()
            .find(|(key, _)| {
                key.first() == Some(&ANN_OVERLAY_DELTA_PREFIX) && key.last() == Some(&4)
            })
            .ok_or("missing overlay upsert")?;
        leaf.1[16..24].copy_from_slice(&3_u64.to_le_bytes());
        assert!(restore_overlay_fixture(&duplicate_sequence).is_err());
        Ok(())
    }

    #[test]
    fn overlay_node_corruption_reachability_and_explicit_empty_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        for key_offset in [0, 1, 17, 33] {
            let mut corrupted = fixture.clone();
            let node = corrupted
                .entries
                .iter_mut()
                .find(|(key, _)| {
                    key.first() == Some(&ANN_OVERLAY_NODE_PREFIX) && key.get(17) == Some(&31)
                })
                .ok_or("missing depth-31 node")?;
            node.0[key_offset] ^= 1;
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "node key {key_offset}"
            );
        }
        for value_offset in [0, 8, 9, 10, 16, 18, 24, 56] {
            let mut corrupted = fixture.clone();
            let node = corrupted
                .entries
                .iter_mut()
                .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
                .ok_or("missing node")?;
            node.1[value_offset] ^= 1;
            assert!(
                restore_overlay_fixture(&corrupted).is_err(),
                "node value {value_offset}"
            );
        }
        let mut explicit_empty = fixture.clone();
        let node = explicit_empty
            .entries
            .iter_mut()
            .find(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
            .ok_or("missing node")?;
        let (_, depth, _) = decode_overlay_node_key(&node.0)?;
        node.1[56..88].copy_from_slice(&overlay_empty_hash(depth.saturating_add(1)));
        assert!(restore_overlay_fixture(&explicit_empty).is_err());

        let mut reordered = fixture.clone();
        let node = reordered
            .entries
            .iter_mut()
            .find(|(key, _)| {
                key.first() == Some(&ANN_OVERLAY_NODE_PREFIX) && key.get(17) == Some(&31)
            })
            .ok_or("missing branching node")?;
        let mut decoded = decode_overlay_node(&node.1, 31)?;
        decoded.child_hashes.swap(0, 1);
        decoded.node_hash = overlay_node_hash(31, decoded.bitmap, &decoded.child_hashes)?;
        node.1 = encode_overlay_node_for_test(&decoded)?;
        assert!(restore_overlay_fixture(&reordered).is_err());

        let mut missing = fixture.clone();
        let position = missing
            .entries
            .iter()
            .position(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
            .ok_or("missing node")?;
        missing.entries.remove(position);
        assert!(restore_overlay_fixture(&missing).is_err());

        let mut unreachable = fixture.clone();
        let mut extra = unreachable
            .entries
            .iter()
            .find(|(key, _)| {
                key.first() == Some(&ANN_OVERLAY_NODE_PREFIX) && key.get(17) == Some(&31)
            })
            .cloned()
            .ok_or("missing depth-31 node")?;
        extra.0[33] = extra.0[33].wrapping_add(0x10);
        unreachable.entries.push(extra);
        unreachable
            .entries
            .sort_by(|left, right| left.0.cmp(&right.0));
        assert!(restore_overlay_fixture(&unreachable).is_err());
        Ok(())
    }

    #[test]
    fn legacy_metadata_rejects_every_overlay_namespace() -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition()?;
        let base = HnswIndex::build(
            definition,
            [VectorRecord {
                object_id: ObjectId::new(2)?,
                creating_csn: Csn::new(1)?,
                vector: Vector::new([2.0, 2.0])?,
            }],
        )?;
        let snapshot = base.export_snapshot();
        let state = AnnIndexState::new(base, DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
        let mut metadata_versions = [1, 2, 3]
            .into_iter()
            .map(|version| encode_legacy_metadata(&snapshot, version))
            .collect::<Result<Vec<_>, _>>()?;
        metadata_versions.push(encode_metadata(&state)?);
        let mut base_entries = BTreeMap::new();
        append_base_generation_entries(&mut base_entries, &state.base)?;
        let fixture = overlay_fixture()?;
        for encoded_metadata in metadata_versions {
            let metadata = decode_metadata(&encoded_metadata)?;
            for prefix in [
                ANN_OVERLAY_MANIFEST_PREFIX,
                ANN_OVERLAY_DELTA_PREFIX,
                ANN_OVERLAY_NODE_PREFIX,
            ] {
                let extra = fixture
                    .entries
                    .iter()
                    .find(|(key, _)| key.first() == Some(&prefix))
                    .cloned()
                    .ok_or("missing overlay namespace")?;
                let mut entries = base_entries.clone();
                entries.insert(extra.0, extra.1);
                assert!(
                    restore_index_with_definition(
                        &entries.into_iter().collect::<Vec<_>>(),
                        state.definition().index_id(),
                        state.definition(),
                        metadata.clone(),
                    )
                    .is_err(),
                    "metadata version {} overlay prefix {prefix}",
                    metadata.version
                );
            }
        }
        Ok(())
    }

    #[test]
    fn overlay_metadata_and_frontier_enforce_existing_hard_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = overlay_fixture()?;
        for (offset, value) in [
            (
                120,
                u64::from(DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE.delta_max_entries) + 1,
            ),
            (128, u64::try_from(MAX_ANN_DELTA_BYTES)? + 1),
            (
                224,
                u64::from(DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE.delta_max_entries) + 1,
            ),
            (232, u64::try_from(MAX_ANN_DELTA_BYTES)? + 1),
            (240, ANN_OVERLAY_MAX_NODES + 1),
            (
                248,
                u64::from(DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE.delta_max_entries) + 1,
            ),
            (256, u64::try_from(MAX_ANN_DELTA_BYTES)? + 1),
        ] {
            let mut corrupted = fixture.metadata.clone();
            corrupted[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(
                decode_metadata(&corrupted).is_err(),
                "bound offset {offset}"
            );
        }
        let mut zero_sequence = fixture.metadata;
        zero_sequence[264..272].fill(0);
        assert!(decode_metadata(&zero_sequence).is_err());

        let mut frontier = BTreeMap::new();
        let mut value = 0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03_u128;
        for position in 0..MAX_ANN_DELTA_RECORDS {
            value ^= value << 17;
            value ^= value >> 29;
            value ^= value << 41;
            value = value.wrapping_add(u128::try_from(position)? + 1);
            frontier.insert(value.max(1), [u8::try_from(position % 251)? + 1; 32]);
        }
        assert_eq!(frontier.len(), MAX_ANN_DELTA_RECORDS);
        let mut total_nodes = 0_usize;
        let mut peak_level_nodes = 0_usize;
        for depth in (0..ANN_OVERLAY_TREE_DEPTH).rev() {
            let (parents, expected) = expected_overlay_level(&frontier, depth)?;
            peak_level_nodes = peak_level_nodes.max(expected.len());
            total_nodes = total_nodes
                .checked_add(expected.len())
                .ok_or("node count overflow")?;
            frontier = parents;
        }
        assert!(total_nodes > 100_000);
        assert!(total_nodes <= usize::try_from(ANN_OVERLAY_MAX_NODES)?);
        assert!(peak_level_nodes <= MAX_ANN_DELTA_RECORDS);
        Ok(())
    }

    #[test]
    fn current_ann_writers_emit_only_m04_d01_and_allocated_legacy_prefixes()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let metadata = encode_metadata(&state)?;
        assert_eq!(&metadata[..8], ANN_INDEX_META_MAGIC_V4);
        let delta = DeltaRecord::Upsert {
            sequence: 1,
            record: VectorRecord {
                object_id: ObjectId::new(90)?,
                creating_csn: Csn::new(2)?,
                vector: Vector::new([9.0, 0.0])?,
            },
        };
        let encoded_delta = encode_delta(&delta)?;
        assert_eq!(&encoded_delta[..8], ANN_DELTA_MAGIC);
        assert_eq!(
            meta_key(state.definition().index_id())[0],
            ANN_INDEX_META_PREFIX
        );
        assert_eq!(
            delta_key(state.definition().index_id(), ObjectId::new(90)?)[0],
            ANN_DELTA_PREFIX
        );
        for forbidden in [
            ANN_INDEX_META_MAGIC_V5,
            ANN_OVERLAY_MANIFEST_MAGIC,
            ANN_OVERLAY_DELTA_MAGIC,
            ANN_OVERLAY_NODE_MAGIC,
            b"HYANNA02",
        ] {
            assert!(!metadata.windows(8).any(|window| window == forbidden));
            assert!(!encoded_delta.windows(8).any(|window| window == forbidden));
        }
        Ok(())
    }

    #[test]
    fn historical_m04_bytes_restore_and_reencode_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let encoded = encode_metadata(&state)?;
        let mut entries = BTreeMap::new();
        append_base_generation_entries(&mut entries, &state.base)?;
        let restored = restore_index_with_definition(
            &entries.into_iter().collect::<Vec<_>>(),
            state.definition().index_id(),
            state.definition(),
            decode_metadata(&encoded)?,
        )?;
        assert_eq!(restored, state);
        assert_eq!(encode_metadata(&restored)?, encoded);
        Ok(())
    }

    #[test]
    fn historical_m01_through_m04_restore_to_the_same_read_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition()?;
        let base = HnswIndex::build(
            definition,
            [
                VectorRecord {
                    object_id: ObjectId::new(1)?,
                    creating_csn: Csn::new(1)?,
                    vector: Vector::new([1.0, 0.0])?,
                },
                VectorRecord {
                    object_id: ObjectId::new(2)?,
                    creating_csn: Csn::new(1)?,
                    vector: Vector::new([2.0, 0.0])?,
                },
            ],
        )?;
        let snapshot = base.export_snapshot();
        let state = AnnIndexState::new(base, DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
        let mut entries = BTreeMap::new();
        append_base_generation_entries(&mut entries, &state.base)?;
        let entries = entries.into_iter().collect::<Vec<_>>();
        let expected = state.search_exact(&Vector::new([0.0, 0.0])?, 2, None)?;
        let mut versions = [1, 2, 3]
            .into_iter()
            .map(|version| encode_legacy_metadata(&snapshot, version))
            .collect::<Result<Vec<_>, _>>()?;
        versions.push(encode_metadata(&state)?);
        for (position, encoded) in versions.into_iter().enumerate() {
            let restored = restore_index_with_definition(
                &entries,
                definition.index_id(),
                definition,
                decode_metadata(&encoded)?,
            )?;
            assert_eq!(restored.effective_vectors(), state.effective_vectors());
            assert_eq!(
                restored.search_exact(&Vector::new([0.0, 0.0])?, 2, None)?,
                expected,
                "metadata version {}",
                position + 1
            );
        }
        Ok(())
    }

    #[test]
    fn overlay_codecs_and_domain_hashes_match_fixed_goldens()
    -> Result<(), Box<dyn std::error::Error>> {
        let delta = DeltaRecord::Tombstone {
            sequence: 3,
            mutation_csn: Csn::new(3)?,
        };
        let encoded = encode_overlay_delta_for_test(&delta)?;
        assert_eq!(
            encoded,
            [
                72, 89, 65, 78, 78, 68, 48, 50, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 3,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
        let leaf = overlay_leaf_hash(ObjectId::new(1)?, &encoded);
        let node = overlay_node_hash(31, 0x0012, &[leaf, [7; 32]])?;
        let manifest = OverlayManifest {
            legacy_view_identity: [1; 32],
            overlay_root: node,
            view_identity: [3; 32],
            legacy_count: 2,
            legacy_bytes: 96,
            overlay_count: 3,
            overlay_bytes: 136,
            overlay_node_count: 34,
            effective_count: 3,
            effective_bytes: 136,
            legacy_next_sequence: 3,
            next_sequence: 6,
        };
        assert_eq!(
            overlay_empty_hash(0),
            [
                228, 39, 167, 233, 20, 15, 12, 228, 127, 240, 182, 196, 102, 136, 197, 213, 229, 3,
                34, 51, 95, 74, 121, 161, 197, 241, 218, 244, 167, 6, 201, 104,
            ]
        );
        assert_eq!(
            leaf,
            [
                145, 14, 98, 7, 25, 101, 165, 41, 183, 69, 138, 249, 90, 145, 248, 124, 104, 82,
                186, 27, 193, 135, 124, 130, 204, 215, 137, 126, 173, 121, 254, 107,
            ]
        );
        assert_eq!(
            node,
            [
                85, 117, 77, 105, 7, 54, 207, 21, 253, 29, 64, 47, 152, 124, 109, 98, 13, 231, 84,
                183, 76, 202, 140, 198, 189, 146, 228, 249, 8, 148, 14, 100,
            ]
        );
        assert_eq!(
            overlay_view_identity([9; 32], manifest),
            [
                200, 245, 167, 198, 238, 12, 195, 246, 87, 144, 171, 48, 194, 66, 163, 203, 108,
                219, 182, 127, 10, 170, 76, 168, 232, 61, 171, 230, 9, 182, 126, 53,
            ]
        );
        assert_eq!(
            decode_overlay_manifest(&encode_overlay_manifest_for_test(manifest))?,
            manifest
        );
        let encoded_node = encode_overlay_node_for_test(&OverlayNode {
            depth: 31,
            bitmap: 0x0012,
            node_hash: node,
            child_hashes: vec![leaf, [7; 32]],
        })?;
        assert_eq!(decode_overlay_node(&encoded_node, 31)?.node_hash, node);

        for (mut unsupported, decoder) in [
            (encode_overlay_manifest_for_test(manifest), 0_u8),
            (encoded.clone(), 1),
            (encoded_node, 2),
        ] {
            unsupported[7] = unsupported[7].wrapping_add(1);
            let rejected = match decoder {
                0 => decode_overlay_manifest(&unsupported).is_err(),
                1 => decode_overlay_delta(&unsupported, ObjectId::new(1)?, definition()?).is_err(),
                _ => decode_overlay_node(&unsupported, 31).is_err(),
            };
            assert!(rejected);
        }
        Ok(())
    }

    #[test]
    fn frozen_legacy_identity_preserves_historical_empty_and_nonempty_rules()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = [9; 32];
        let empty = BTreeMap::new();
        assert_eq!(calculate_view_identity(base, 1, &empty), base);
        assert_eq!(calculate_view_identity(base, u64::MAX, &empty), base);

        let object_id = ObjectId::new(1)?;
        let nonempty = BTreeMap::from([(
            object_id,
            DeltaRecord::Tombstone {
                sequence: 3,
                mutation_csn: Csn::new(3)?,
            },
        )]);
        assert_eq!(
            calculate_view_identity(base, 4, &nonempty),
            [
                155, 81, 196, 85, 49, 180, 231, 15, 64, 90, 121, 142, 224, 214, 2, 108, 65, 137,
                14, 163, 227, 109, 79, 252, 121, 23, 223, 239, 66, 201, 150, 82,
            ]
        );
        Ok(())
    }

    #[test]
    fn planned_overlay_hydration_streams_sparse_nodes_without_retaining_them()
    -> Result<(), Box<dyn std::error::Error>> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let fixture = overlay_fixture()?;
        let path = std::env::temp_dir().join(format!(
            "hyphae-ann-overlay-{}-{}.pages",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut pages = PageStore::create(&path)?;
        let pool = BufferPool::new(64, 4)?;
        let mut entries = fixture.entries.clone();
        entries.push((
            crate::SEARCH_FORMAT_KEY.to_vec(),
            crate::SEARCH_FORMAT_VALUE_V3.to_vec(),
        ));
        entries.push((
            meta_key(fixture.definition.index_id()),
            fixture.metadata.clone(),
        ));
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let tree = BTree::empty()
            .upsert_sorted_batch(&mut pages, Csn::new(1)?, entries)?
            .tree;
        let root = tree.root().ok_or("missing root")?;
        let plan = plan_index_load(
            &pages,
            &pool,
            root,
            fixture.definition.index_id(),
            fixture.definition,
        )?;
        assert!(matches!(
            plan_delta_mutation(
                &pages,
                &pool,
                root,
                fixture.definition.index_id(),
                fixture.definition,
            ),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        assert!(matches!(
            maintenance_status(&pages, &pool, &plan),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        assert!(matches!(
            plan_consolidation(
                &pages,
                &pool,
                &plan,
                16,
                MAX_ANN_DELTA_RECORDS,
                ConsolidationBuildExecution {
                    pool: None,
                    permit: None,
                    cancellation: None,
                },
            ),
            Err(NativeRuntimeError::InvalidPreparedMutation)
        ));
        reset_index_scoped_restore_count_for_test();
        let (owned, observed) = hydrate_owned_read_state(&pages, &pool, &plan, None)?;
        let node_count = fixture
            .entries
            .iter()
            .filter(|(key, _)| key.first() == Some(&ANN_OVERLAY_NODE_PREFIX))
            .count();
        assert_eq!(observed.physical_entries, fixture.entries.len());
        assert_eq!(plan.physical_entry_limit(), fixture.entries.len());
        assert!(index_scoped_peak_physical_entries_for_test() < node_count);
        assert_eq!(owned.authority().delta_records, 3);
        drop(pages);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn delta_metadata_preserves_v2_v3_and_safely_upgrades_v1()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = HnswIndex::new(definition()?)?.export_snapshot();
        let object_id = ObjectId::new(91)?;
        let delta = DeltaRecord::Upsert {
            sequence: 1,
            record: VectorRecord {
                object_id,
                creating_csn: Csn::new(2)?,
                vector: Vector::new([1.0, 2.0])?,
            },
        };
        let deltas = BTreeMap::from([(object_id, delta)]);

        for (input_version, output_version, encoded_size) in [
            (1, 4, ANN_INDEX_META_V1_SIZE),
            (2, 2, ANN_INDEX_META_V2_SIZE),
            (3, 3, ANN_INDEX_META_V3_SIZE),
        ] {
            let expected = encode_legacy_metadata(&snapshot, input_version)?;
            assert_eq!(expected.len(), encoded_size);
            let mut trailing = expected.clone();
            trailing.push(0);
            assert!(decode_metadata(&trailing).is_err());

            let mut metadata = decode_metadata(&expected)?;
            metadata.next_sequence = 2;
            let view_identity =
                calculate_view_identity(metadata.build_identity, metadata.next_sequence, &deltas);
            let encoded = encode_delta_metadata(&metadata, &expected, &deltas, view_identity)?;
            let decoded = decode_metadata(&encoded)?;
            assert_eq!(decoded.version, output_version);
            assert_eq!(decoded.view_identity, view_identity);
            assert_eq!(decoded.delta_count, 1);
            assert_eq!(
                decoded.delta_bytes,
                u64::try_from(deltas[&object_id].encoded_len())?
            );
            assert_eq!(decoded.next_sequence, 2);
            assert_eq!(decoded.lifecycle, DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
            assert_eq!(decoded.build_identity, snapshot.build_identity);
        }
        Ok(())
    }

    #[test]
    fn serial_and_single_generation_routes_never_claim_targeted_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let query = Vector::new([0.0, 0.0])?;
        let options = SearchOptions::new(1, 16, Some(16))?;
        let partitioned = partitioned_state()?.search_selected(&query, options, 2)?;
        assert_eq!(partitioned.targeted_single_batches, 0);
        assert_eq!(partitioned.generic_single_fallback_batches, 0);

        let definition = definition()?;
        let base = HnswIndex::build(definition, Vec::new())?;
        let single = AnnBase::Single(base).search_routed(&query, options, 1)?;
        assert_eq!(
            single.routing_mode,
            AnnRoutingExecutionMode::SingleGenerationFallback
        );
        assert_eq!(single.targeted_single_batches, 0);
        assert_eq!(single.generic_single_fallback_batches, 0);
        Ok(())
    }

    #[test]
    fn single_route_counts_are_bounded_by_batches_and_waves() {
        assert!(validate_single_route_counts(1, 1, 2, 2).is_ok());
        assert!(validate_single_route_counts(usize::MAX, 1, usize::MAX, usize::MAX).is_err());
        assert!(validate_single_route_counts(2, 0, 1, 2).is_err());
        assert!(validate_single_route_counts(2, 0, 2, 1).is_err());
    }

    #[test]
    fn targeted_foreign_cancellation_preserves_governor_causality() {
        assert!(matches!(
            map_targeted_ann_execution_error(TargetedSingleExecutionError::ForeignCancellation),
            NativeRuntimeError::ResourceQueue(GovernorQueueError::ForeignCancellation)
        ));
        assert!(matches!(
            map_targeted_ann_execution_error(TargetedSingleExecutionError::Cancelled),
            NativeRuntimeError::ResourceQueue(GovernorQueueError::Cancelled)
        ));
        for error in [
            TargetedSingleExecutionError::Closed,
            TargetedSingleExecutionError::GenerationExhausted,
        ] {
            assert!(matches!(
                map_targeted_ann_execution_error(error),
                NativeRuntimeError::Execution(NativeExecutionError::Synchronization)
            ));
        }
    }

    fn restored_entries(base: &AnnBase) -> BTreeMap<[u8; 32], RestoredChildEntries> {
        base.export_snapshots()
            .into_iter()
            .map(|snapshot| {
                let vector_ids = snapshot
                    .vectors
                    .iter()
                    .map(|record| record.object_id)
                    .collect();
                let layers = snapshot
                    .nodes
                    .iter()
                    .map(|node| {
                        (
                            node.object_id,
                            node.neighbors
                                .iter()
                                .cloned()
                                .enumerate()
                                .map(|(layer, neighbors)| {
                                    (u16::try_from(layer).unwrap_or(u16::MAX), neighbors)
                                })
                                .collect(),
                        )
                    })
                    .collect();
                (
                    snapshot.build_identity,
                    RestoredChildEntries {
                        vectors: snapshot.vectors,
                        vector_ids,
                        layers,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn metadata_v4_round_trips_partitioned_children_in_canonical_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let expected = match &state.base {
            AnnBase::Partitioned(index) => index.export_snapshot(),
            AnnBase::Single(_) => return Err("expected partitioned base".into()),
        };
        let encoded = encode_metadata(&state)?;
        assert_eq!(&encoded[..8], ANN_INDEX_META_MAGIC_V4);
        let metadata = decode_metadata(&encoded)?;
        assert_eq!(metadata.base_kind, PersistedBaseKind::Partitioned);
        assert_eq!(metadata.input_identity, Some(expected.input_identity));
        assert_eq!(metadata.build_identity, expected.build_identity);
        assert_eq!(
            metadata
                .children
                .iter()
                .map(|child| child.build_identity)
                .collect::<Vec<_>>(),
            expected
                .partitions
                .iter()
                .map(|child| child.build_identity)
                .collect::<Vec<_>>()
        );
        let restored = restore_base(
            &metadata,
            expected.definition,
            restored_entries(&state.base),
        )?;
        let AnnBase::Partitioned(restored) = restored else {
            return Err("restored wrong base kind".into());
        };
        assert_eq!(restored.export_snapshot(), expected);
        Ok(())
    }

    #[test]
    fn metadata_v4_rejects_reordered_partition_descriptors()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let mut encoded = encode_metadata(&state)?;
        let first = ANN_INDEX_META_V4_HEADER_SIZE;
        let children = &mut encoded[first..];
        let (left, right) = children.split_at_mut(ANN_INDEX_META_V4_CHILD_SIZE);
        left.swap_with_slice(&mut right[..ANN_INDEX_META_V4_CHILD_SIZE]);
        let metadata = decode_metadata(&encoded)?;
        assert!(restore_base(&metadata, definition()?, restored_entries(&state.base)).is_err());
        Ok(())
    }

    #[test]
    fn metadata_v4_rejects_identity_drift_and_incomplete_children()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let definition = state.definition();
        let encoded = encode_metadata(&state)?;
        for offset in [8, 72, ANN_INDEX_META_V4_HEADER_SIZE] {
            let mut corrupted = encoded.clone();
            corrupted[offset] ^= 1;
            let metadata = decode_metadata(&corrupted)?;
            assert!(restore_base(&metadata, definition, restored_entries(&state.base)).is_err());
        }

        let metadata = decode_metadata(&encoded)?;
        let mut missing_vector = restored_entries(&state.base);
        let child = missing_vector
            .values_mut()
            .next()
            .ok_or("missing restored child")?;
        let removed = child.vectors.pop().ok_or("missing restored vector")?;
        child.vector_ids.remove(&removed.object_id);
        assert!(restore_base(&metadata, definition, missing_vector).is_err());

        let mut missing_graph = restored_entries(&state.base);
        let child = missing_graph
            .values_mut()
            .next()
            .ok_or("missing restored child")?;
        let object_id = *child.layers.keys().next().ok_or("missing restored graph")?;
        child.layers.remove(&object_id);
        assert!(restore_base(&metadata, definition, missing_graph).is_err());
        Ok(())
    }

    #[test]
    fn metadata_v4_retains_partition_children_as_one_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = partitioned_state()?;
        let retained = state.base.retention_descriptor();
        let replacement = HnswIndex::build(state.definition(), state.effective_vectors())?;
        state.base = AnnBase::Single(replacement);
        state.refresh_view_identity();
        state.retained_generations.push(retained.clone());
        let encoded = encode_metadata(&state)?;
        let metadata = decode_metadata(&encoded)?;
        assert_eq!(metadata.retained_generations, vec![retained.clone()]);
        for child in retained.children {
            assert!(metadata.owns_physical_identity(child.build_identity));
        }
        assert!(!metadata.owns_physical_identity([0xB3; 32]));

        let mut truncated = encoded;
        truncated.pop();
        assert!(decode_metadata(&truncated).is_err());
        Ok(())
    }

    #[test]
    fn retained_v4_children_fail_closed_when_any_physical_record_is_missing()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let retained = state.base.retention_descriptor();
        let snapshots = state.base.export_snapshots();
        assert_eq!(retained.children.len(), snapshots.len());
        for (descriptor, snapshot) in retained.children.iter().zip(&snapshots) {
            let summary = PhysicalGenerationSummary {
                vector_ids: snapshot
                    .vectors
                    .iter()
                    .map(|record| record.object_id)
                    .collect(),
                graph_layers: snapshot
                    .nodes
                    .iter()
                    .map(|node| {
                        (
                            node.object_id,
                            (0..node.neighbors.len())
                                .map(u16::try_from)
                                .collect::<Result<BTreeSet<_>, _>>(),
                        )
                    })
                    .map(|(object_id, layers)| Ok((object_id, layers?)))
                    .collect::<Result<_, std::num::TryFromIntError>>()?,
            };
            validate_retained_child_entries(descriptor, &summary)?;

            let mut missing_vector = PhysicalGenerationSummary {
                vector_ids: summary.vector_ids.clone(),
                graph_layers: summary.graph_layers.clone(),
            };
            let vector_id = *missing_vector
                .vector_ids
                .first()
                .ok_or("retained child had no vector")?;
            missing_vector.vector_ids.remove(&vector_id);
            assert!(validate_retained_child_entries(descriptor, &missing_vector).is_err());

            let mut missing_graph = PhysicalGenerationSummary {
                vector_ids: summary.vector_ids.clone(),
                graph_layers: summary.graph_layers.clone(),
            };
            missing_graph.graph_layers.remove(&vector_id);
            assert!(validate_retained_child_entries(descriptor, &missing_graph).is_err());

            let mut missing_layer = summary;
            missing_layer
                .graph_layers
                .get_mut(&vector_id)
                .ok_or("retained child had no graph node")?
                .remove(&0);
            assert!(validate_retained_child_entries(descriptor, &missing_layer).is_err());
        }
        Ok(())
    }

    #[test]
    fn durable_partition_limit_accounts_for_every_retained_generation() {
        assert_eq!(maximum_initial_ann_bulk_partitions(1), 111);
        assert_eq!(maximum_initial_ann_bulk_partitions(2), 74);
        assert_eq!(maximum_initial_ann_bulk_partitions(64), 2);
        assert_eq!(
            maximum_consolidation_replacement_partitions(111, [], 1, false),
            111
        );
        assert_eq!(
            maximum_consolidation_replacement_partitions(74, [74], 2, false),
            74
        );
        assert_eq!(
            maximum_consolidation_replacement_partitions(2, [2; 63], 64, false),
            59
        );
    }

    #[test]
    fn consolidation_partition_count_is_bounded_by_effective_membership() {
        let selected_children = 221;
        assert_eq!(
            consolidation_replacement_partitions(true, selected_children, 1),
            1
        );
        assert_eq!(
            consolidation_replacement_partitions(true, selected_children, 2),
            2
        );
        assert_eq!(
            consolidation_replacement_partitions(true, selected_children, selected_children - 1),
            selected_children - 1
        );
    }

    #[test]
    fn effective_vector_capture_cancels_between_records_without_returning_partial_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS.set(0);
        let mut checks = 0_usize;
        let result = state.effective_vectors_with_control(|| {
            checks = checks.saturating_add(1);
            if checks == 4 {
                Err(GovernorQueueError::Cancelled.into())
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            result,
            Err(NativeRuntimeError::ResourceQueue(
                GovernorQueueError::Cancelled
            ))
        ));
        assert_eq!(ANN_CONSOLIDATION_EFFECTIVE_VECTOR_VISITS.get(), 2);
        Ok(())
    }

    #[test]
    fn physical_entry_index_validates_all_children_after_one_source_pass()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let definition = state.definition();
        let snapshots = state.base.export_snapshots();
        let mut physical = BTreeMap::new();
        for snapshot in &snapshots {
            append_generation_entries(&mut physical, snapshot)?;
        }
        let entries = physical.into_iter().collect::<Vec<_>>();
        let physical_entries = PhysicalEntryIndex::build(&entries)?;
        assert_eq!(physical_entries.source_entry_visits, entries.len());
        assert_eq!(physical_entries.children.len(), snapshots.len());

        for snapshot in &snapshots {
            validate_initial_bulk_child_entries(
                &entries,
                &physical_entries,
                definition.index_id(),
                definition,
                &PersistedChildDescriptor::from_snapshot(snapshot),
            )?;
        }
        assert_eq!(physical_entries.source_entry_visits, entries.len());
        Ok(())
    }

    #[test]
    fn initial_bulk_bounded_child_validation_rejects_corrupt_physical_records()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = partitioned_state()?;
        let definition = state.definition();
        for snapshot in state.base.export_snapshots() {
            let descriptor = PersistedChildDescriptor::from_snapshot(&snapshot);
            let mut physical = BTreeMap::new();
            append_generation_entries(&mut physical, &snapshot)?;
            let entries = physical.into_iter().collect::<Vec<_>>();
            let physical_entries = PhysicalEntryIndex::build(&entries)?;
            validate_initial_bulk_child_entries(
                &entries,
                &physical_entries,
                definition.index_id(),
                definition,
                &descriptor,
            )?;

            for prefix in [ANN_VECTOR_PREFIX, ANN_GRAPH_LAYER_PREFIX] {
                let mut truncated = entries.clone();
                let position = truncated
                    .iter()
                    .position(|(key, _)| key.first() == Some(&prefix))
                    .ok_or("missing initial bulk physical record")?;
                truncated.remove(position);
                let truncated_index = PhysicalEntryIndex::build(&truncated)?;
                assert!(
                    validate_initial_bulk_child_entries(
                        &truncated,
                        &truncated_index,
                        definition.index_id(),
                        definition,
                        &descriptor,
                    )
                    .is_err()
                );
            }

            let mut corrupted = entries;
            let graph = corrupted
                .iter_mut()
                .find(|(key, _)| key.first() == Some(&ANN_GRAPH_LAYER_PREFIX))
                .ok_or("missing initial bulk graph record")?;
            graph.1[0] ^= 1;
            let corrupted_index = PhysicalEntryIndex::build(&corrupted)?;
            assert!(
                validate_initial_bulk_child_entries(
                    &corrupted,
                    &corrupted_index,
                    definition.index_id(),
                    definition,
                    &descriptor,
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn partitioned_base_applies_deltas_and_filters_with_an_exact_receipt()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = partitioned_state()?;
        let deleted = ObjectId::new(1)?;
        let inserted = ObjectId::new(9)?;
        assert!(state.delete(deleted, Csn::new(4)?)?);
        state.upsert(inserted, Csn::new(4)?, Vector::new([5.0, 5.0])?)?;
        let allowlist = [deleted, inserted].into_iter().collect();
        let result = state.search(
            &Vector::new([5.0, 5.0])?,
            SearchOptions::new(1, 4, None)?,
            Some(&allowlist),
        )?;
        assert!(!result.approximate);
        assert_eq!(result.strategy, AnnSearchStrategy::StableIdAdaptiveExact);
        assert_eq!(result.recall_risk, AnnRecallRisk::ExactFilteredCandidates);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].object_id, inserted);
        assert_eq!(result.build_identity, state.view_identity);
        Ok(())
    }

    #[test]
    fn partitioned_lifecycle_uses_borrowed_records_without_exporting_the_corpus()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition()?;
        let creating_csn = Csn::new(3)?;
        let records = (1..=2_048_u16)
            .map(|value| {
                Ok(VectorRecord {
                    object_id: ObjectId::new(u128::from(value))?,
                    creating_csn,
                    vector: Vector::new([f32::from(value), f32::from(value % 17)])?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let plan = HnswPartitionPlan::build(definition, records, 8)?;
        let base = AnnBase::Partitioned(PartitionedHnswIndex::build(&plan)?);
        let mut state = AnnIndexState {
            view_identity: base.build_identity(),
            base,
            deltas: BTreeMap::new(),
            next_sequence: 1,
            lifecycle: DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE,
            retained_generations: Vec::new(),
            persisted_version: 4,
        };
        ANN_BASE_SNAPSHOT_EXPORTS.set(0);

        assert!(state.delete(ObjectId::new(1)?, Csn::new(4)?)?);
        state.upsert(
            ObjectId::new(3_000)?,
            Csn::new(4)?,
            Vector::new([3_000.0, 1.0])?,
        )?;
        let exact = state.search_exact_profiled(&Vector::new([1_024.0, 1.0])?, 10, None)?;
        assert_eq!(exact.planned_vectors, 2_048);
        assert_eq!(state.effective_vector_count(), 2_048);
        let vectors = state.effective_vectors();
        assert_eq!(vectors.len(), 2_048);
        let replacement = HnswIndex::build(definition, vectors)?.into_snapshot();
        assert_eq!(replacement.vectors.len(), 2_048);
        let retained = state.base.retention_descriptor();
        assert_eq!(retained.children.len(), 8);
        let encoded = encode_metadata(&state)?;
        assert_eq!(decode_metadata(&encoded)?.children.len(), 8);
        let replacement_view_identity = calculate_view_identity(
            replacement.build_identity,
            state.next_sequence,
            &state.deltas,
        );
        let replacement = ConsolidationReplacement::Single(replacement);
        let encoded =
            encode_consolidated_metadata(&state, &replacement, replacement_view_identity)?;
        assert_eq!(decode_metadata(&encoded)?.children.len(), 1);
        assert_eq!(ANN_BASE_SNAPSHOT_EXPORTS.get(), 0);
        Ok(())
    }

    #[test]
    fn selected_certificate_is_revoked_when_a_tombstone_removes_the_selected_kth_hit()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = partitioned_state()?;
        let query = Vector::new([0.0, 0.0])?;
        let options = SearchOptions::new(1, 4, Some(4))?;
        let AnnBase::Partitioned(index) = &state.base else {
            return Err("expected partitioned base".into());
        };
        let plan = index.plan_routed_search(&query, options, 1)?;
        let child = index.search_planned_partition(&plan, 0)?;
        let routed = index.merge_routed_search(&plan, &[child])?;
        assert_eq!(
            routed.outcome,
            PartitionedAnnRoutingOutcome::SelectedCertified
        );
        let selected_hit = routed.result.hits[0].object_id;

        assert!(state.delete(selected_hit, Csn::new(4)?)?);
        let merged = merge_routed_candidate_with_deltas(&state, &query, options, routed, None)?;

        assert!(!selected_certificate_survives_deltas(&merged, options));
        assert!(merged.result.hits.is_empty());
        Ok(())
    }

    #[test]
    fn metadata_v3_decodes_as_a_single_base_with_scalar_retention()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = HnswIndex::new(definition()?)?.export_snapshot();
        let retained_identity = [0xC1; 32];
        let mut encoded = Vec::with_capacity(ANN_INDEX_META_V3_SIZE + 32);
        encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V3);
        encoded.extend_from_slice(&snapshot.build_identity);
        encoded.extend_from_slice(&0_u64.to_le_bytes());
        encoded.extend_from_slice(&0_u64.to_le_bytes());
        encoded.extend_from_slice(&0_u128.to_be_bytes());
        encoded.extend_from_slice(&0_u16.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V1);
        encoded.extend_from_slice(&snapshot.build_identity);
        encoded.extend_from_slice(&0_u64.to_le_bytes());
        encoded.extend_from_slice(&0_u64.to_le_bytes());
        encoded.extend_from_slice(&1_u64.to_le_bytes());
        encoded.extend_from_slice(
            &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                .delta_max_entries
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                .consolidate_after_deltas
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE
                .retain_generations
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&1_u16.to_le_bytes());
        encoded.extend_from_slice(&[0; 6]);
        encoded.extend_from_slice(&retained_identity);

        let metadata = decode_metadata(&encoded)?;
        assert_eq!(metadata.version, 3);
        assert_eq!(metadata.base_kind, PersistedBaseKind::Single);
        assert_eq!(metadata.children.len(), 1);
        assert_eq!(
            metadata.retained_generations,
            vec![RetainedGeneration {
                build_identity: retained_identity,
                children: vec![PersistedChildDescriptor::legacy(retained_identity)],
            }]
        );
        Ok(())
    }

    #[test]
    fn legacy_retained_physical_records_enrich_to_v4_and_fail_closed_when_truncated()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = definition()?;
        let legacy = HnswIndex::build(
            definition,
            [
                VectorRecord {
                    object_id: ObjectId::new(21)?,
                    creating_csn: Csn::new(2)?,
                    vector: Vector::new([1.0, 2.0])?,
                },
                VectorRecord {
                    object_id: ObjectId::new(22)?,
                    creating_csn: Csn::new(2)?,
                    vector: Vector::new([2.0, 3.0])?,
                },
            ],
        )?
        .export_snapshot();
        let mut physical = BTreeMap::new();
        append_generation_entries(&mut physical, &legacy)?;
        let entries = physical.into_iter().collect::<Vec<_>>();
        let physical_entries = PhysicalEntryIndex::build(&entries)?;
        let descriptor = restore_legacy_retained_descriptor(
            &entries,
            &physical_entries,
            definition.index_id(),
            definition,
            legacy.build_identity,
        )?;
        assert!(descriptor.complete);
        assert_eq!(descriptor.vector_count, 2);
        assert_eq!(descriptor.graph_node_count, 2);

        let current = HnswIndex::build(
            definition,
            [VectorRecord {
                object_id: ObjectId::new(31)?,
                creating_csn: Csn::new(3)?,
                vector: Vector::new([8.0, 9.0])?,
            }],
        )?;
        let mut state = AnnIndexState::new(current, DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE);
        state.retained_generations.push(RetainedGeneration {
            build_identity: legacy.build_identity,
            children: vec![descriptor],
        });
        let upgraded = decode_metadata(&encode_metadata(&state)?)?;
        assert_eq!(upgraded.version, 4);
        assert!(upgraded.retained_generations[0].children[0].complete);

        for prefix in [ANN_VECTOR_PREFIX, ANN_GRAPH_LAYER_PREFIX] {
            let mut truncated = entries.clone();
            let position = truncated
                .iter()
                .position(|(key, _)| key.first() == Some(&prefix))
                .ok_or("missing retained physical record")?;
            truncated.remove(position);
            let truncated_index = PhysicalEntryIndex::build(&truncated)?;
            assert!(
                restore_legacy_retained_descriptor(
                    &truncated,
                    &truncated_index,
                    definition.index_id(),
                    definition,
                    legacy.build_identity,
                )
                .is_err()
            );
        }
        Ok(())
    }
}
