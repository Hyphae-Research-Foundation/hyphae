// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
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
    BTree, BTreeError, KeyValue, PrefixReplacementBatch, PrefixReplacementStructuralLimits,
    PrefixReplacementStructuralPlan,
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
const ANN_VECTOR_MAGIC: &[u8; 8] = b"HYANNV01";
const ANN_GRAPH_LAYER_MAGIC: &[u8; 8] = b"HYANNG01";
const ANN_DELTA_MAGIC: &[u8; 8] = b"HYANND01";
const ANN_INDEX_META_V1_SIZE: usize = 80;
const ANN_INDEX_META_V2_SIZE: usize = 144;
const ANN_INDEX_META_V3_SIZE: usize = 160;
const ANN_INDEX_META_V4_HEADER_SIZE: usize = 160;
const ANN_INDEX_META_V4_CHILD_SIZE: usize = 72;
const ANN_INDEX_META_V4_RETAINED_HEADER_SIZE: usize = 40;
const ANN_INDEX_META_KEY_SIZE: usize = 17;
const BTREE_LEAF_HEADER_SIZE: usize = 16;
const BTREE_LEAF_ENTRY_HEADER_SIZE: usize = 8;
const ANN_VECTOR_HEADER_SIZE: usize = 24;
const ANN_GRAPH_LAYER_HEADER_SIZE: usize = 16;
const ANN_DELTA_HEADER_SIZE: usize = 40;
const ANN_GENERATION_KEY_SIZE: usize = 65;
const ANN_GRAPH_LAYER_KEY_SIZE: usize = 67;
const ANN_DELTA_KEY_SIZE: usize = 33;
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
        if !current.deltas.is_empty() || !current.retained_generations.is_empty() {
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

#[derive(Clone)]
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
}

impl AnnPhysicalLimits {
    fn total_entries(self) -> usize {
        self.vectors
            .entries
            .saturating_add(self.graph_layers.entries)
            .saturating_add(self.deltas.entries)
    }

    fn total_bytes(self) -> u64 {
        self.vectors
            .bytes
            .saturating_add(self.graph_layers.bytes)
            .saturating_add(self.deltas.bytes)
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
        Some(ANN_INDEX_META_PREFIX | ANN_VECTOR_PREFIX | ANN_GRAPH_LAYER_PREFIX | ANN_DELTA_PREFIX)
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
    if !matches!(current.base, AnnBase::Single(_))
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
    if !matches!(current.base, AnnBase::Single(_))
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

pub(crate) fn apply_tree_mutations(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    catalog: &CatalogState,
    mutations: &[Mutation],
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

pub(crate) fn load(
    pages: &PageStore,
    root: Option<PageId>,
    catalog: &CatalogState,
) -> Result<AnnState, NativeRuntimeError> {
    load_from_tree(pages, root, catalog, true)
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
    let (state, entries) = load_planned_index_with_entries(pages, buffer_pool, plan, cancellation)?;
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
            physical_entries: entries.len(),
            physical_bytes,
        },
    ))
}

fn load_planned_index(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnIndexState, NativeRuntimeError> {
    load_planned_index_with_entries(pages, buffer_pool, plan, cancellation).map(|(state, _)| state)
}

fn load_planned_index_with_entries(
    pages: &PageStore,
    buffer_pool: &BufferPool,
    plan: &AnnIndexLoadPlan,
    cancellation: Option<&GovernorCancellation>,
) -> Result<(AnnIndexState, Vec<KeyValue>), NativeRuntimeError> {
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
    #[cfg(test)]
    ANN_INDEX_SCOPED_RESTORES.set(ANN_INDEX_SCOPED_RESTORES.get().saturating_add(1));
    ANN_INDEX_SCOPED_RESTORES_PROCESS.fetch_add(1, Ordering::Relaxed);
    let state = restore_index_with_definition_controlled(
        &entries,
        plan.index,
        plan.definition,
        metadata,
        cancellation,
    )?;
    Ok((state, entries))
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
    restore_index_with_definition_controlled(entries, index, definition, metadata, None)
}

fn restore_index_with_definition_controlled(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: PersistedIndexMetadata,
    cancellation: Option<&GovernorCancellation>,
) -> Result<AnnIndexState, NativeRuntimeError> {
    let delta_prefix = object_prefix(ANN_DELTA_PREFIX, index);
    let current_identities = metadata.current_child_identities();
    let retained_identities = metadata.retained_child_identities();
    let mut children = BTreeMap::<[u8; 32], RestoredChildEntries>::new();
    let mut deltas = BTreeMap::new();
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
            _ => {}
        }
    }
    validate_restored_deltas(&metadata, &deltas)?;
    let base = restore_base_with_cancellation(&metadata, definition, children, cancellation)?;
    let mut restored = AnnIndexState {
        base,
        deltas,
        next_sequence: metadata.next_sequence,
        view_identity: metadata.view_identity,
        lifecycle: metadata.lifecycle,
        retained_generations: metadata.retained_generations,
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
    } else if restored.view_identity
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

#[derive(Default)]
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
    let (mut current, physical_entries) =
        load_planned_index_with_entries(pages, buffer_pool, &load_plan, None)?;
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
    let current =
        restore_index_with_definition_controlled(&entries, plan.index, definition, metadata, None)?;
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
    let (current, entries) = load_planned_index_with_entries(pages, buffer_pool, load_plan, None)?;
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
    let (current, entries) = load_planned_index_with_entries(pages, buffer_pool, load_plan, None)?;
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

fn validate_target_physical_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    index: ObjectId,
    definition: VectorIndexDefinition,
    metadata: &PersistedIndexMetadata,
) -> Result<(), NativeRuntimeError> {
    let mut generations = BTreeMap::<[u8; 32], PhysicalGenerationSummary>::new();
    let mut delta_count = 0_u64;
    let mut delta_bytes = 0_u64;
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
            _ => return Err(NativeRuntimeError::InvalidAnnTree),
        }
    }
    if delta_count != metadata.delta_count || delta_bytes != metadata.delta_bytes {
        return Err(NativeRuntimeError::InvalidAnnTree);
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

fn decode_index(encoded: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeRuntimeError::InvalidAnnTree)
}

fn encode_initial_bulk_metadata(
    current: &AnnIndexState,
    snapshot: &PartitionedIndexSnapshot,
) -> Result<Vec<u8>, NativeRuntimeError> {
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
    if state.next_sequence == 0 || state.view_identity == [0; 32] {
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

fn decode_metadata(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
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
    })
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

fn decode_delta(
    encoded: &[u8],
    object_id: ObjectId,
    definition: VectorIndexDefinition,
) -> Result<DeltaRecord, NativeRuntimeError> {
    if encoded.len() < ANN_DELTA_HEADER_SIZE
        || encoded.get(..8) != Some(ANN_DELTA_MAGIC.as_slice())
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
                hasher.update(&record.creating_csn.get().to_le_bytes());
                hasher.update(&encode_vector_mutation(&record.vector));
            }
            DeltaRecord::Tombstone { mutation_csn, .. } => {
                hasher.update(&[ANN_DELTA_TOMBSTONE]);
                hasher.update(&mutation_csn.get().to_le_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
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
        })
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
