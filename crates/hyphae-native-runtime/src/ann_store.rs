// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use hyphae_native_ann::{
    AnnRecallRisk, AnnSearchResult, AnnSearchStrategy, GraphNodeRecord, HnswConfig, HnswIndex,
    IndexSnapshot, Metric, SearchOptions, Vector, VectorHit, VectorIndexDefinition, VectorRecord,
};
use hyphae_native_btree::BTree;
use hyphae_native_catalog::{
    CatalogObject, IncrementalVectorLifecycle, SearchCollectionDefinition, VectorMetric,
};
use hyphae_native_pages::{PageKind, PageStore};
use hyphae_native_types::{Csn, ObjectId, PageId};

use crate::{
    NativeRuntimeError,
    model::CatalogState,
    wal_codec::{Mutation, Opcode},
};

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
const ANN_VECTOR_MAGIC: &[u8; 8] = b"HYANNV01";
const ANN_GRAPH_LAYER_MAGIC: &[u8; 8] = b"HYANNG01";
const ANN_DELTA_MAGIC: &[u8; 8] = b"HYANND01";
const ANN_INDEX_META_V1_SIZE: usize = 80;
const ANN_INDEX_META_V2_SIZE: usize = 144;
const ANN_INDEX_META_V3_SIZE: usize = 160;
const ANN_VECTOR_HEADER_SIZE: usize = 24;
const ANN_GRAPH_LAYER_HEADER_SIZE: usize = 16;
const ANN_DELTA_HEADER_SIZE: usize = 40;
const ANN_GENERATION_KEY_SIZE: usize = 65;
const ANN_GRAPH_LAYER_KEY_SIZE: usize = 67;
const ANN_DELTA_KEY_SIZE: usize = 33;
const ANN_DELTA_UPSERT: u8 = 1;
const ANN_DELTA_TOMBSTONE: u8 = 2;
const PRIVATE_MUTATION_CSN: u64 = u64::MAX;

pub(crate) const DEFAULT_INCREMENTAL_VECTOR_LIFECYCLE: IncrementalVectorLifecycle =
    IncrementalVectorLifecycle {
        delta_max_entries: 4_096,
        consolidate_after_deltas: 1_024,
        retain_generations: 1,
    };

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
struct AnnIndexState {
    base: HnswIndex,
    deltas: BTreeMap<ObjectId, DeltaRecord>,
    next_sequence: u64,
    view_identity: [u8; 32],
    lifecycle: IncrementalVectorLifecycle,
    retained_generations: Vec<[u8; 32]>,
}

impl AnnIndexState {
    fn new(base: HnswIndex, lifecycle: IncrementalVectorLifecycle) -> Self {
        let mut state = Self {
            base,
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

    fn effective_record(&self, object_id: ObjectId) -> Option<VectorRecord> {
        match self.deltas.get(&object_id) {
            Some(DeltaRecord::Upsert { record, .. }) => Some(record.clone()),
            Some(DeltaRecord::Tombstone { .. }) => None,
            None => self
                .base
                .export_snapshot()
                .vectors
                .into_iter()
                .find(|record| record.object_id == object_id),
        }
    }

    fn effective_vectors(&self) -> Vec<VectorRecord> {
        let mut vectors = self
            .base
            .export_snapshot()
            .vectors
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
        if self.effective_record(object_id).is_none() {
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
        validate_vector(self.definition(), query)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut hits = self
            .effective_vectors()
            .into_iter()
            .filter(|record| allowlist.is_none_or(|ids| ids.contains(&record.object_id)))
            .map(|record| {
                Ok(VectorHit {
                    object_id: record.object_id,
                    distance: distance(self.definition().metric(), query, &record.vector)?,
                })
            })
            .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
        sort_hits(&mut hits);
        hits.truncate(k);
        Ok(hits)
    }

    fn search(
        &self,
        query: &Vector,
        options: SearchOptions,
        allowlist: Option<&BTreeSet<ObjectId>>,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        if let Some(allowlist) = allowlist {
            let eligible_count = self
                .effective_vectors()
                .into_iter()
                .filter(|record| allowlist.contains(&record.object_id))
                .count();
            if eligible_count <= options.ef_search() {
                let hits = self.search_exact(query, options.k(), Some(allowlist))?;
                return Ok(AnnSearchResult {
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
                });
            }
        }
        let base_result = if let Some(allowlist) = allowlist {
            self.base.search_filtered(query, options, allowlist)?
        } else {
            self.base.search(query, options)?
        };
        let overridden = self.deltas.keys().copied().collect::<BTreeSet<_>>();
        let mut hits = base_result
            .hits
            .into_iter()
            .filter(|hit| !overridden.contains(&hit.object_id))
            .collect::<Vec<_>>();
        let mut exact_delta_candidates = 0_usize;
        for (object_id, delta) in &self.deltas {
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
        let current = self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        if !current.deltas.is_empty() || !current.retained_generations.is_empty() {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        let mut records = current
            .base
            .export_snapshot()
            .vectors
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
        let replacement = HnswIndex::build(current.definition(), records.into_values())?;
        let current = self
            .indexes
            .get_mut(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        current.base = replacement;
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
}

#[derive(Clone)]
struct PersistedIndexMetadata {
    build_identity: [u8; 32],
    vector_count: u64,
    graph_node_count: u64,
    entry_point: Option<ObjectId>,
    max_level: u16,
    view_identity: [u8; 32],
    delta_count: u64,
    delta_bytes: u64,
    next_sequence: u64,
    lifecycle: IncrementalVectorLifecycle,
    retained_generations: Vec<[u8; 32]>,
    version: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct ConsolidationPlan {
    index: ObjectId,
    base_identity: [u8; 32],
    captured_view_identity: [u8; 32],
    captured_deltas: BTreeMap<ObjectId, u64>,
    replacement: IndexSnapshot,
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

    pub(crate) fn captured_delta_count(&self) -> usize {
        self.captured_deltas.len()
    }

    pub(crate) fn effective_vector_count(&self) -> usize {
        self.replacement.vectors.len()
    }

    pub(crate) const fn replacement_identity(&self) -> [u8; 32] {
        self.replacement.build_identity
    }
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
            append_generation_entries(&mut entries, &index_state.base.export_snapshot())?;
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
    let metadata = decode_metadata_entries(&entries)?;
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

fn restore_index(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    index: ObjectId,
    metadata: PersistedIndexMetadata,
) -> Result<AnnIndexState, NativeRuntimeError> {
    let definition = catalog_ann_definition(catalog, index)?;
    let delta_prefix = object_prefix(ANN_DELTA_PREFIX, index);
    let mut vectors = Vec::new();
    let mut vector_ids = BTreeSet::new();
    let mut layers = BTreeMap::<ObjectId, BTreeMap<u16, Vec<ObjectId>>>::new();
    let mut deltas = BTreeMap::new();
    for (key, value) in entries {
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (found_index, build_identity, object_id) = decode_vector_key(key)?;
                if found_index == index && build_identity == metadata.build_identity {
                    if !vector_ids.insert(object_id) {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                    vectors.push(decode_vector_record(value, object_id, definition)?);
                } else if found_index == index
                    && !metadata.retained_generations.contains(&build_identity)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (found_index, build_identity, object_id, layer) = decode_graph_layer_key(key)?;
                if found_index == index && build_identity == metadata.build_identity {
                    if layers
                        .entry(object_id)
                        .or_default()
                        .insert(layer, decode_graph_layer(value)?)
                        .is_some()
                    {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                } else if found_index == index
                    && !metadata.retained_generations.contains(&build_identity)
                {
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
    vectors.sort_by_key(|record| record.object_id);
    validate_restored_index(&metadata, &vectors, &vector_ids, &layers, &deltas)?;
    let nodes = layers
        .into_iter()
        .map(|(object_id, layers)| graph_node(object_id, layers))
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot = IndexSnapshot {
        definition,
        vectors,
        nodes,
        entry_point: metadata.entry_point,
        max_level: metadata.max_level,
        build_identity: metadata.build_identity,
    };
    let base = HnswIndex::restore(&snapshot).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
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

fn validate_restored_index(
    metadata: &PersistedIndexMetadata,
    vectors: &[VectorRecord],
    vector_ids: &BTreeSet<ObjectId>,
    layers: &BTreeMap<ObjectId, BTreeMap<u16, Vec<ObjectId>>>,
    deltas: &BTreeMap<ObjectId, DeltaRecord>,
) -> Result<(), NativeRuntimeError> {
    if u64::try_from(vectors.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
        != metadata.vector_count
        || u64::try_from(layers.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != metadata.graph_node_count
        || *vector_ids != layers.keys().copied().collect()
        || u64::try_from(deltas.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
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
    root: PageId,
    catalog: &CatalogState,
    index: ObjectId,
    max_vectors: usize,
    max_delta_records: usize,
) -> Result<ConsolidationPlan, NativeRuntimeError> {
    if max_vectors == 0
        || max_vectors > MAX_ANN_CONSOLIDATION_VECTORS
        || max_delta_records == 0
        || max_delta_records > MAX_ANN_DELTA_RECORDS
    {
        return Err(NativeRuntimeError::InvalidAnnConsolidationLimit);
    }
    let state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state
        .indexes
        .get(&index)
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
    if max_delta_records
        > usize::try_from(current.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
    {
        return Err(NativeRuntimeError::InvalidAnnConsolidationLimit);
    }
    if current.deltas.is_empty() {
        return Err(NativeRuntimeError::AnnConsolidationNotNeeded);
    }
    let vectors = current.effective_vectors();
    if vectors.len() > max_vectors || current.deltas.len() > max_delta_records {
        return Err(NativeRuntimeError::AnnConsolidationLimitExceeded);
    }
    let replacement = HnswIndex::build(current.definition(), vectors)?.export_snapshot();
    Ok(ConsolidationPlan {
        index,
        base_identity: current.base.build_identity(),
        captured_view_identity: current.view_identity,
        captured_deltas: current
            .deltas
            .iter()
            .map(|(object_id, delta)| (*object_id, delta.sequence()))
            .collect(),
        replacement,
    })
}

pub(crate) fn encode_consolidation_mutation(plan: &ConsolidationPlan) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(112);
    encoded.extend_from_slice(b"HYANNC01");
    encoded.extend_from_slice(&plan.base_identity);
    encoded.extend_from_slice(&plan.captured_view_identity);
    encoded.extend_from_slice(&plan.replacement.build_identity);
    encoded.extend_from_slice(
        &u64::try_from(plan.captured_deltas.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    encoded
}

pub(crate) fn consolidate_tree(
    pages: &mut PageStore,
    root: Option<PageId>,
    creating_csn: Csn,
    catalog: &CatalogState,
    plan: &ConsolidationPlan,
) -> Result<BTree, NativeRuntimeError> {
    let root = root.ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let tree = BTree::from_root(root);
    let mut state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state
        .indexes
        .get_mut(&plan.index)
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index: plan.index })?;
    if current.base.build_identity() != plan.base_identity {
        return Err(NativeRuntimeError::AnnConsolidationStale);
    }
    if HnswIndex::restore(&plan.replacement)
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
        .export_snapshot()
        != plan.replacement
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
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
    let previous = current.base.export_snapshot();
    if !previous.vectors.is_empty()
        && previous.build_identity != plan.replacement.build_identity
        && !current
            .retained_generations
            .contains(&previous.build_identity)
    {
        current.retained_generations.push(previous.build_identity);
    }
    let retain = usize::from(current.lifecycle.retain_generations);
    if current.retained_generations.len() > retain {
        current
            .retained_generations
            .drain(..current.retained_generations.len() - retain);
    }
    current.base =
        HnswIndex::restore(&plan.replacement).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    current.refresh_view_identity();
    current.validate_delta_bounds()?;

    let mut entries = tree.scan(pages)?;
    entries.retain(|(key, _)| {
        if !ann_key_targets_index(key, plan.index) {
            return true;
        }
        ann_generation_identity(key)
            .is_some_and(|identity| current.retained_generations.contains(&identity))
    });
    if let Some((_, marker)) = entries
        .iter_mut()
        .find(|(key, _)| key.as_slice() == crate::SEARCH_FORMAT_KEY)
    {
        *marker = crate::SEARCH_FORMAT_VALUE_V3.to_vec();
    }
    let mut replacement_entries = BTreeMap::new();
    replacement_entries.insert(meta_key(plan.index), encode_metadata(current)?);
    append_generation_entries(&mut replacement_entries, &plan.replacement)?;
    for (object_id, delta) in &current.deltas {
        replacement_entries.insert(delta_key(plan.index, *object_id), encode_delta(delta)?);
    }
    entries.extend(replacement_entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
    let replacement = BTree::empty()
        .upsert_sorted_batch(pages, creating_csn, entries)?
        .tree;
    load_from_tree(pages, replacement.root(), catalog, true)?;
    Ok(replacement)
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
        .into_iter()
        .filter(|(key, _)| match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                decode_vector_key(key).is_ok_and(|(found_index, build_identity, _)| {
                    found_index == index && build_identity == current.base.build_identity()
                })
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                decode_graph_layer_key(key).is_ok_and(|(found_index, build_identity, _, _)| {
                    found_index == index && build_identity == current.base.build_identity()
                })
            }
            _ => false,
        })
        .count();
    Ok(IndexObservation {
        base_identity: current.base.build_identity(),
        view_identity: current.view_identity,
        base_vector_count: current.base.len(),
        effective_vector_count: current.effective_vectors().len(),
        delta_records: current.deltas.len(),
        delta_bytes: current.delta_bytes(),
        generation_records,
        selected_generation_records,
        lifecycle: current.lifecycle,
        maintenance_due: maintenance_due(current),
    })
}

pub(crate) fn maintenance_status(
    pages: &PageStore,
    root: PageId,
    catalog: &CatalogState,
    index: ObjectId,
) -> Result<MaintenanceStatus, NativeRuntimeError> {
    let state = load_from_tree(pages, Some(root), catalog, true)?;
    let current = state
        .indexes
        .get(&index)
        .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
    Ok(MaintenanceStatus {
        lifecycle: current.lifecycle,
        delta_records: current.deltas.len(),
        delta_bytes: current.delta_bytes(),
        due: maintenance_due(current),
    })
}

fn maintenance_due(state: &AnnIndexState) -> bool {
    state.deltas.len() >= usize::from(state.lifecycle.consolidate_after_deltas)
        || state.deltas.len()
            >= usize::try_from(state.lifecycle.delta_max_entries).unwrap_or(usize::MAX)
}

fn validate_physical_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    metadata: &BTreeMap<ObjectId, PersistedIndexMetadata>,
) -> Result<(), NativeRuntimeError> {
    let mut indexes_with_records = BTreeSet::new();
    for (key, value) in entries {
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (index, build_identity, object_id) = decode_vector_key(key)?;
                let definition = catalog_ann_definition(catalog, index)?;
                decode_vector_record(value, object_id, definition)?;
                let persisted = metadata
                    .get(&index)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                if build_identity != persisted.build_identity
                    && !persisted.retained_generations.contains(&build_identity)
                {
                    return Err(NativeRuntimeError::InvalidAnnTree);
                }
                indexes_with_records.insert(index);
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (index, build_identity, _, _) = decode_graph_layer_key(key)?;
                catalog_ann_definition(catalog, index)?;
                decode_graph_layer(value)?;
                let persisted = metadata
                    .get(&index)
                    .ok_or(NativeRuntimeError::InvalidAnnTree)?;
                if build_identity != persisted.build_identity
                    && !persisted.retained_generations.contains(&build_identity)
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

fn meta_key(index: ObjectId) -> Vec<u8> {
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

fn encode_metadata(state: &AnnIndexState) -> Result<Vec<u8>, NativeRuntimeError> {
    let snapshot = state.base.export_snapshot();
    let retained_count = u16::try_from(state.retained_generations.len())
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    let mut encoded = Vec::with_capacity(
        ANN_INDEX_META_V3_SIZE.saturating_add(state.retained_generations.len().saturating_mul(32)),
    );
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V3);
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
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC_V1);
    encoded.extend_from_slice(&state.view_identity);
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
    encoded.extend_from_slice(&retained_count.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    for generation in &state.retained_generations {
        encoded.extend_from_slice(generation);
    }
    Ok(encoded)
}

fn decode_metadata(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
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
        decode_retained_generations(encoded, version, lifecycle, build_identity)?;
    Ok(PersistedIndexMetadata {
        build_identity,
        vector_count: read_u64(&encoded[40..48]),
        graph_node_count: read_u64(&encoded[48..56]),
        entry_point: if raw_entry == 0 {
            None
        } else {
            Some(ObjectId::new(raw_entry).map_err(|_| NativeRuntimeError::InvalidAnnTree)?)
        },
        max_level: u16::from_le_bytes(
            encoded[72..74]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
        view_identity,
        delta_count,
        delta_bytes,
        next_sequence,
        lifecycle,
        retained_generations,
        version,
    })
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

fn decode_retained_generations(
    encoded: &[u8],
    version: u8,
    lifecycle: IncrementalVectorLifecycle,
    build_identity: [u8; 32],
) -> Result<Vec<[u8; 32]>, NativeRuntimeError> {
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
    let retained = encoded[ANN_INDEX_META_V3_SIZE..]
        .chunks_exact(32)
        .map(|identity| {
            identity
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)
        })
        .collect::<Result<Vec<[u8; 32]>, _>>()?;
    if retained.contains(&[0; 32])
        || retained.windows(2).any(|pair| pair[0] == pair[1])
        || retained.contains(&build_identity)
    {
        return Err(NativeRuntimeError::InvalidAnnTree);
    }
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
