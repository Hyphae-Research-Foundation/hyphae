// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use hyphae_native_ann::{
    AnnSearchResult, GraphNodeRecord, HnswConfig, HnswIndex, IndexSnapshot, Metric, SearchOptions,
    Vector, VectorHit, VectorIndexDefinition, VectorRecord,
};
use hyphae_native_btree::BTree;
use hyphae_native_catalog::{CatalogObject, SearchCollectionDefinition, VectorMetric};
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

const ANN_INDEX_META_MAGIC: &[u8; 8] = b"HYANNM01";
const ANN_VECTOR_MAGIC: &[u8; 8] = b"HYANNV01";
const ANN_GRAPH_LAYER_MAGIC: &[u8; 8] = b"HYANNG01";
const ANN_INDEX_META_SIZE: usize = 80;
const ANN_VECTOR_HEADER_SIZE: usize = 24;
const ANN_GRAPH_LAYER_HEADER_SIZE: usize = 16;
const ANN_GENERATION_KEY_SIZE: usize = 65;
const ANN_GRAPH_LAYER_KEY_SIZE: usize = 67;
const PRIVATE_MUTATION_CSN: u64 = u64::MAX;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AnnState {
    indexes: BTreeMap<ObjectId, HnswIndex>,
}

impl AnnState {
    pub(crate) fn create(
        &mut self,
        definition: VectorIndexDefinition,
    ) -> Result<(), NativeRuntimeError> {
        if self
            .indexes
            .insert(definition.index_id(), HnswIndex::new(definition)?)
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
            .upsert(object_id, creating_csn, vector)?;
        Ok(())
    }

    pub(crate) fn upsert_many(
        &mut self,
        index: ObjectId,
        creating_csn: Csn,
        vectors: &[(ObjectId, Vector)],
    ) -> Result<(), NativeRuntimeError> {
        self.apply_batch(
            index,
            creating_csn,
            vectors
                .iter()
                .map(|(object_id, vector)| AnnMutation::Upsert(*object_id, vector.clone())),
        )
    }

    pub(crate) fn delete(
        &mut self,
        index: ObjectId,
        object_id: ObjectId,
    ) -> Result<bool, NativeRuntimeError> {
        Ok(self
            .indexes
            .get_mut(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .delete(object_id)?)
    }

    pub(crate) fn search(
        &self,
        index: ObjectId,
        query: &Vector,
        options: SearchOptions,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        Ok(self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search(query, options)?)
    }

    pub(crate) fn search_exact(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        Ok(self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact(query, k)?)
    }

    pub(crate) fn search_filtered(
        &self,
        index: ObjectId,
        query: &Vector,
        options: SearchOptions,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<AnnSearchResult, NativeRuntimeError> {
        Ok(self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_filtered(query, options, allowlist)?)
    }

    pub(crate) fn search_exact_filtered(
        &self,
        index: ObjectId,
        query: &Vector,
        k: usize,
        allowlist: &BTreeSet<ObjectId>,
    ) -> Result<Vec<VectorHit>, NativeRuntimeError> {
        Ok(self
            .indexes
            .get(&index)
            .ok_or(NativeRuntimeError::UnknownVectorIndex { index })?
            .search_exact_filtered(query, k, allowlist)?)
    }

    fn apply_batch(
        &mut self,
        index: ObjectId,
        creating_csn: Csn,
        mutations: impl IntoIterator<Item = AnnMutation>,
    ) -> Result<(), NativeRuntimeError> {
        let mut definition = self.indexes.get(&index).map(HnswIndex::definition);
        let mut vectors = self
            .indexes
            .get(&index)
            .map(|current| {
                current
                    .export_snapshot()
                    .vectors
                    .into_iter()
                    .map(|record| (record.object_id, record))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        for mutation in mutations {
            match mutation {
                AnnMutation::Create(created) => {
                    if created.index_id() != index || definition.replace(created).is_some() {
                        return Err(NativeRuntimeError::InvalidPreparedMutation);
                    }
                }
                AnnMutation::Upsert(object_id, vector) => {
                    if definition.is_none() {
                        return Err(NativeRuntimeError::UnknownVectorIndex { index });
                    }
                    vectors.insert(
                        object_id,
                        VectorRecord {
                            object_id,
                            creating_csn,
                            vector,
                        },
                    );
                }
                AnnMutation::Delete(object_id) => {
                    if definition.is_none() {
                        return Err(NativeRuntimeError::UnknownVectorIndex { index });
                    }
                    if vectors.remove(&object_id).is_none() {
                        return Err(NativeRuntimeError::InvalidPreparedMutation);
                    }
                }
            }
        }
        let definition = definition.ok_or(NativeRuntimeError::UnknownVectorIndex { index })?;
        let replacement = HnswIndex::build(definition, vectors.into_values())?;
        self.indexes.insert(index, replacement);
        Ok(())
    }
}

enum AnnMutation {
    Create(VectorIndexDefinition),
    Upsert(ObjectId, Vector),
    Delete(ObjectId),
}

#[derive(Clone, Copy)]
struct PersistedIndexMetadata {
    build_identity: [u8; 32],
    vector_count: u64,
    graph_node_count: u64,
    entry_point: Option<ObjectId>,
    max_level: u16,
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
        Some(ANN_INDEX_META_PREFIX | ANN_VECTOR_PREFIX | ANN_GRAPH_LAYER_PREFIX)
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
    let mut grouped = BTreeMap::<ObjectId, Vec<AnnMutation>>::new();
    for mutation in ann_mutations {
        let index = mutation
            .target
            .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
        let operation = match mutation.opcode {
            Opcode::CreateAnnIndex => {
                let object = CatalogObject::decode_definition(&mutation.value)?;
                let CatalogObject::Search(definition) = object else {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                };
                if definition.header.id != index {
                    return Err(NativeRuntimeError::InvalidPreparedMutation);
                }
                AnnMutation::Create(definition_from_search(&definition)?)
            }
            Opcode::UpsertVector => AnnMutation::Upsert(
                decode_object_identity(&mutation.key)?,
                decode_vector_mutation(&mutation.value)?,
            ),
            Opcode::DeleteVector => AnnMutation::Delete(decode_object_identity(&mutation.key)?),
            _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
        };
        grouped.entry(index).or_default().push(operation);
    }

    for (index, mutations) in &mut grouped {
        state.apply_batch(*index, creating_csn, std::mem::take(mutations))?;
    }

    validate_catalog_coverage(catalog, &state)?;
    for index in grouped.keys() {
        let snapshot = state
            .indexes
            .get(index)
            .ok_or(NativeRuntimeError::InvalidAnnTree)?
            .export_snapshot();
        tree = persist_generation(pages, tree, creating_csn, &snapshot)?;
    }
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
    let mut metadata = BTreeMap::new();
    for (key, value) in &entries {
        if key.first() == Some(&ANN_INDEX_META_PREFIX) {
            let index = decode_meta_key(key)?;
            if metadata.insert(index, decode_metadata(value)?).is_some() {
                return Err(NativeRuntimeError::InvalidAnnTree);
            }
        }
    }
    validate_physical_entries(&entries, catalog, &metadata)?;

    let mut state = AnnState::default();
    for (index, metadata) in metadata {
        let definition = catalog_ann_definition(catalog, index)?;
        let vector_prefix = generation_prefix(ANN_VECTOR_PREFIX, index, metadata.build_identity);
        let graph_prefix =
            generation_prefix(ANN_GRAPH_LAYER_PREFIX, index, metadata.build_identity);
        let mut vectors = Vec::new();
        let mut vector_ids = BTreeSet::new();
        let mut layers = BTreeMap::<ObjectId, BTreeMap<u16, Vec<ObjectId>>>::new();

        for (key, value) in &entries {
            match key.first().copied() {
                Some(ANN_VECTOR_PREFIX) => {
                    let (found_index, build_identity, object_id) = decode_vector_key(key)?;
                    if found_index == index && build_identity == metadata.build_identity {
                        if !key.starts_with(&vector_prefix) || !vector_ids.insert(object_id) {
                            return Err(NativeRuntimeError::InvalidAnnTree);
                        }
                        vectors.push(decode_vector_record(value, object_id, definition)?);
                    }
                }
                Some(ANN_GRAPH_LAYER_PREFIX) => {
                    let (found_index, build_identity, object_id, layer) =
                        decode_graph_layer_key(key)?;
                    if found_index == index
                        && build_identity == metadata.build_identity
                        && (!key.starts_with(&graph_prefix)
                            || layers
                                .entry(object_id)
                                .or_default()
                                .insert(layer, decode_graph_layer(value)?)
                                .is_some())
                    {
                        return Err(NativeRuntimeError::InvalidAnnTree);
                    }
                }
                _ => {}
            }
        }
        vectors.sort_by_key(|record| record.object_id);
        if u64::try_from(vectors.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            != metadata.vector_count
            || u64::try_from(layers.len()).map_err(|_| NativeRuntimeError::InvalidAnnTree)?
                != metadata.graph_node_count
            || vector_ids != layers.keys().copied().collect()
        {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
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
        let restored =
            HnswIndex::restore(&snapshot).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
        if state.indexes.insert(index, restored).is_some() {
            return Err(NativeRuntimeError::InvalidAnnTree);
        }
    }

    if require_complete {
        validate_catalog_coverage(catalog, &state)?;
    }
    Ok(state)
}

fn validate_physical_entries(
    entries: &[(Vec<u8>, Vec<u8>)],
    catalog: &CatalogState,
    metadata: &BTreeMap<ObjectId, PersistedIndexMetadata>,
) -> Result<(), NativeRuntimeError> {
    let mut indexes_with_generation_records = BTreeSet::new();
    for (key, value) in entries {
        match key.first().copied() {
            Some(ANN_VECTOR_PREFIX) => {
                let (index, _, object_id) = decode_vector_key(key)?;
                let definition = catalog_ann_definition(catalog, index)?;
                decode_vector_record(value, object_id, definition)?;
                indexes_with_generation_records.insert(index);
            }
            Some(ANN_GRAPH_LAYER_PREFIX) => {
                let (index, _, _, _) = decode_graph_layer_key(key)?;
                catalog_ann_definition(catalog, index)?;
                decode_graph_layer(value)?;
                indexes_with_generation_records.insert(index);
            }
            _ => {}
        }
    }
    if indexes_with_generation_records
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

fn persist_generation(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    snapshot: &IndexSnapshot,
) -> Result<BTree, NativeRuntimeError> {
    tree = tree
        .upsert(
            pages,
            creating_csn,
            meta_key(snapshot.definition.index_id()),
            encode_metadata(snapshot)?,
        )?
        .tree;
    for record in &snapshot.vectors {
        tree = tree
            .insert_unique(
                pages,
                creating_csn,
                vector_key(
                    snapshot.definition.index_id(),
                    snapshot.build_identity,
                    record.object_id,
                ),
                encode_vector_record(record)?,
            )?
            .tree;
    }
    for node in &snapshot.nodes {
        for (layer, neighbors) in node.neighbors.iter().enumerate() {
            let layer = u16::try_from(layer).map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
            tree = tree
                .insert_unique(
                    pages,
                    creating_csn,
                    graph_layer_key(
                        snapshot.definition.index_id(),
                        snapshot.build_identity,
                        node.object_id,
                        layer,
                    ),
                    encode_graph_layer(neighbors)?,
                )?
                .tree;
        }
    }
    Ok(tree)
}

fn meta_key(index: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(ANN_INDEX_META_PREFIX);
    key.extend_from_slice(&index.get().to_be_bytes());
    key
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

fn generation_prefix(prefix: u8, index: ObjectId, build_identity: [u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(49);
    key.push(prefix);
    key.extend_from_slice(&index.get().to_be_bytes());
    key.extend_from_slice(&build_identity);
    key
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

fn decode_index(encoded: &[u8]) -> Result<ObjectId, NativeRuntimeError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeRuntimeError::InvalidAnnTree)
}

fn encode_metadata(snapshot: &IndexSnapshot) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut encoded = Vec::with_capacity(ANN_INDEX_META_SIZE);
    encoded.extend_from_slice(ANN_INDEX_META_MAGIC);
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
    Ok(encoded)
}

fn decode_metadata(encoded: &[u8]) -> Result<PersistedIndexMetadata, NativeRuntimeError> {
    if encoded.len() != ANN_INDEX_META_SIZE
        || encoded.get(..8) != Some(ANN_INDEX_META_MAGIC.as_slice())
        || encoded[74..80].iter().any(|byte| *byte != 0)
    {
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
    Ok(PersistedIndexMetadata {
        build_identity,
        vector_count: u64::from_le_bytes(
            encoded[40..48]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
        graph_node_count: u64::from_le_bytes(
            encoded[48..56]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ),
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
    })
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
    Ok(VectorRecord {
        object_id,
        creating_csn: Csn::new(u64::from_le_bytes(
            encoded[8..16]
                .try_into()
                .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        ))
        .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
        vector: decode_vector_mutation(&encoded[ANN_VECTOR_HEADER_SIZE..])
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?,
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
