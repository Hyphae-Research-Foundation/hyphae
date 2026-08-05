// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;

use hyphae_native_catalog::{
    CatalogError, CatalogName, CatalogObject, ColumnDefinition, ObjectHeader, QualifiedName,
    RelationDefinition, SearchCollectionDefinition, SearchFieldDefinition,
};
use hyphae_native_types::{ColumnId, EngineKind, FieldId, LogicalType, ObjectId};
use thiserror::Error;

const CATALOG_MAGIC_V1: [u8; 8] = *b"HYCAT001";
const CATALOG_MAGIC_V2: [u8; 8] = *b"HYCAT002";
const STRUCTURE_MAGIC: [u8; 8] = *b"HYSTR001";
const SEARCH_MAGIC: [u8; 8] = *b"HYSEA001";

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum ModelError {
    #[error("native state payload is truncated")]
    Truncated,
    #[error("native state payload has invalid magic")]
    InvalidMagic,
    #[error("native state payload contains invalid UTF-8")]
    InvalidUtf8,
    #[error("native state payload has trailing bytes")]
    TrailingBytes,
    #[error("native state payload length exceeds u32")]
    LengthOverflow,
    #[error("native state payload contains a zero object ID")]
    ZeroObjectId,
    #[error("native catalog object ID space is exhausted")]
    ObjectIdExhausted,
    #[error("native catalog object ID already exists")]
    DuplicateObjectId,
    #[error("native catalog object name already exists")]
    DuplicateObjectName,
    #[error("native catalog object does not exist")]
    UnknownObject,
    #[error("native catalog object belongs to a different engine")]
    WrongEngine,
    #[error("native secondary index references an unknown relation")]
    UnknownSecondaryIndexRelation,
    #[error("native relational primary key already exists")]
    DuplicatePrimaryKey,
    #[error("native relational primary key does not exist")]
    MissingPrimaryKey,
    #[error("native relational secondary-index entry already exists")]
    DuplicateSecondaryIndexEntry,
    #[error("native relational secondary-index entry does not exist")]
    MissingSecondaryIndexEntry,
    #[error("native relational unique secondary index is violated")]
    UniqueSecondaryIndexViolation,
    #[error("legacy native structure state cannot encode collection families")]
    UnsupportedLegacyStructureFamily,
    #[error("native hash field value is not a canonical signed 64-bit integer")]
    StructureValueNotInteger,
    #[error("native signed 64-bit hash field counter overflow")]
    StructureIntegerOverflow,
    #[error("native search document ID already exists")]
    DuplicateDocumentId,
    #[error("native search document ID does not exist")]
    MissingDocumentId,
    #[error("native state payload contains a duplicate canonical entry")]
    DuplicateEncodedEntry,
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CatalogState {
    pub(crate) objects: BTreeMap<ObjectId, CatalogObject>,
}

impl CatalogState {
    pub(crate) fn create(&mut self, object: CatalogObject) -> Result<(), ModelError> {
        object.validate()?;
        let header = object.header();
        let id = header.id;
        if self.objects.contains_key(&id) {
            return Err(ModelError::DuplicateObjectId);
        }
        if self
            .objects
            .values()
            .any(|entry| same_catalog_lookup(&entry.header().name, &header.name))
        {
            return Err(ModelError::DuplicateObjectName);
        }
        if let CatalogObject::Relation(definition) = &object {
            for foreign_key in &definition.foreign_keys {
                let parent = if foreign_key.referenced_relation == definition.header.id {
                    Some(definition)
                } else {
                    match self.objects.get(&foreign_key.referenced_relation) {
                        Some(CatalogObject::Relation(parent)) => Some(parent),
                        _ => None,
                    }
                };
                let Some(parent) = parent else {
                    return Err(ModelError::Catalog(CatalogError::InvalidDefinitionEncoding));
                };
                foreign_key.validate_relations(definition, parent)?;
            }
        }
        if let CatalogObject::SecondaryIndex(definition) = &object {
            let Some(CatalogObject::Relation(relation)) = self.objects.get(&definition.relation)
            else {
                return Err(ModelError::UnknownSecondaryIndexRelation);
            };
            definition.validate_relation(relation)?;
        }
        self.objects.insert(id, object);
        Ok(())
    }

    pub(crate) fn replace(&mut self, object: CatalogObject) -> Result<CatalogObject, ModelError> {
        object.validate()?;
        let id = object.header().id;
        let old = self.objects.get(&id).ok_or(ModelError::UnknownObject)?;
        if old.header().owner != object.header().owner {
            return Err(ModelError::WrongEngine);
        }
        if self.objects.values().any(|candidate| {
            candidate.header().id != id
                && same_catalog_lookup(&candidate.header().name, &object.header().name)
        }) {
            return Err(ModelError::DuplicateObjectName);
        }
        self.objects
            .insert(id, object)
            .ok_or(ModelError::UnknownObject)
    }

    pub(crate) fn remove(&mut self, id: ObjectId) -> Result<CatalogObject, ModelError> {
        let object = self.objects.get(&id).ok_or(ModelError::UnknownObject)?;
        if matches!(object, CatalogObject::Relation(_))
            && self.objects.values().any(|candidate| match candidate {
                CatalogObject::SecondaryIndex(index) => index.relation == id,
                CatalogObject::Relation(relation) => relation
                    .foreign_keys
                    .iter()
                    .any(|foreign_key| foreign_key.referenced_relation == id),
                CatalogObject::Structure(_) | CatalogObject::Search(_) => false,
            })
        {
            return Err(ModelError::Catalog(CatalogError::InvalidDefinitionEncoding));
        }
        self.objects.remove(&id).ok_or(ModelError::UnknownObject)
    }

    pub(crate) fn object(&self, id: ObjectId) -> Option<&CatalogObject> {
        self.objects.get(&id)
    }

    pub(crate) fn require(&self, id: ObjectId, owner: EngineKind) -> Result<(), ModelError> {
        let entry = self.objects.get(&id).ok_or(ModelError::UnknownObject)?;
        if entry.header().owner != owner {
            return Err(ModelError::WrongEngine);
        }
        Ok(())
    }

    pub(crate) fn id_named(&self, name: &str, owner: EngineKind) -> Result<ObjectId, ModelError> {
        let lookup = normalize_name(name);
        self.objects
            .iter()
            .find(|(_, entry)| {
                entry.header().owner == owner && entry.header().name.object.lookup() == lookup
            })
            .map(|(id, _)| *id)
            .ok_or(ModelError::UnknownObject)
    }

    pub(crate) fn next_object_id(&self) -> Result<ObjectId, ModelError> {
        let next = self.objects.keys().next_back().map_or(Ok(1_u128), |id| {
            id.get().checked_add(1).ok_or(ModelError::ObjectIdExhausted)
        })?;
        ObjectId::new(next).map_err(|_| ModelError::ZeroObjectId)
    }

    #[cfg(test)]
    pub(crate) fn encode(&self) -> Result<Vec<u8>, ModelError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CATALOG_MAGIC_V2);
        put_len(&mut bytes, self.objects.len())?;
        for object in self.objects.values() {
            put_bytes(&mut bytes, &object.encode_definition()?)?;
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ModelError> {
        if bytes.get(..8) == Some(CATALOG_MAGIC_V1.as_slice()) {
            return Self::decode_v1(bytes);
        }
        let mut decoder = Decoder::new(bytes, CATALOG_MAGIC_V2)?;
        let count = decoder.len()?;
        let mut state = Self::default();
        for _ in 0..count {
            state.create(CatalogObject::decode_definition(&decoder.bytes()?)?)?;
        }
        decoder.finish()?;
        Ok(state)
    }

    fn decode_v1(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut decoder = Decoder::new(bytes, CATALOG_MAGIC_V1)?;
        let count = decoder.len()?;
        let mut state = Self::default();
        for _ in 0..count {
            let id = decoder.object_id()?;
            let owner = decode_engine(decoder.byte()?)?;
            let name = decoder.string()?;
            state.create(legacy_catalog_object(id, owner, &name)?)?;
        }
        decoder.finish()?;
        Ok(state)
    }
}

fn same_catalog_lookup(left: &QualifiedName, right: &QualifiedName) -> bool {
    left.database.lookup() == right.database.lookup()
        && left.schema.lookup() == right.schema.lookup()
        && left.object.lookup() == right.object.lookup()
}

fn legacy_catalog_object(
    id: ObjectId,
    owner: EngineKind,
    name: &str,
) -> Result<CatalogObject, ModelError> {
    let header = ObjectHeader {
        id,
        owner,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted(name)?,
        ),
    };
    match owner {
        EngineKind::Relational => Ok(CatalogObject::Relation(RelationDefinition {
            header,
            columns: vec![
                ColumnDefinition {
                    id: ColumnId::new(1).map_err(|_| ModelError::ZeroObjectId)?,
                    name: CatalogName::unquoted("primary_key")?,
                    logical_type: LogicalType::Binary,
                    nullable: false,
                },
                ColumnDefinition {
                    id: ColumnId::new(2).map_err(|_| ModelError::ZeroObjectId)?,
                    name: CatalogName::unquoted("row")?,
                    logical_type: LogicalType::Binary,
                    nullable: false,
                },
            ],
            primary_key: vec![ColumnId::new(1).map_err(|_| ModelError::ZeroObjectId)?],
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        })),
        EngineKind::Search => Ok(CatalogObject::Search(SearchCollectionDefinition {
            header,
            fields: vec![SearchFieldDefinition {
                id: FieldId::new(1).map_err(|_| ModelError::ZeroObjectId)?,
                name: CatalogName::unquoted("text")?,
                logical_type: LogicalType::Text,
                analyzer: None,
                doc_values: false,
            }],
            vector: None,
            ann: None,
        })),
        EngineKind::Kernel | EngineKind::Structure => Err(ModelError::WrongEngine),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RelationState {
    pub(crate) tables: BTreeMap<ObjectId, BTreeMap<Vec<u8>, Vec<u8>>>,
    pub(crate) indexes: BTreeMap<ObjectId, SecondaryIndexState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SecondaryIndexState {
    pub(crate) relation: ObjectId,
    pub(crate) unique: bool,
    pub(crate) nulls_distinct: bool,
    pub(crate) layout: SecondaryIndexLayout,
    pub(crate) entries: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecondaryIndexLayout {
    LegacyLengthFirstV1,
    OrderedV2,
}

impl RelationState {
    #[allow(dead_code, reason = "wired by the pending DROP TABLE WAL/SQL vertical")]
    pub(crate) fn drop_table(
        &mut self,
        id: ObjectId,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, ModelError> {
        if self.indexes.values().any(|index| index.relation == id) {
            return Err(ModelError::Catalog(CatalogError::InvalidDefinitionEncoding));
        }
        self.tables.remove(&id).ok_or(ModelError::UnknownObject)
    }

    pub(crate) fn create_table(&mut self, id: ObjectId) -> Result<(), ModelError> {
        if self.tables.insert(id, BTreeMap::new()).is_some() {
            return Err(ModelError::DuplicateObjectId);
        }
        Ok(())
    }

    #[allow(dead_code, reason = "wired by the pending DROP INDEX WAL/SQL vertical")]
    pub(crate) fn drop_secondary_index(
        &mut self,
        id: ObjectId,
    ) -> Result<SecondaryIndexState, ModelError> {
        self.indexes.remove(&id).ok_or(ModelError::UnknownObject)
    }

    pub(crate) fn create_secondary_index(
        &mut self,
        id: ObjectId,
        relation: ObjectId,
        unique: bool,
        nulls_distinct: bool,
    ) -> Result<(), ModelError> {
        if !self.tables.contains_key(&relation) {
            return Err(ModelError::UnknownObject);
        }
        if self
            .indexes
            .insert(
                id,
                SecondaryIndexState {
                    relation,
                    unique,
                    nulls_distinct,
                    layout: SecondaryIndexLayout::OrderedV2,
                    entries: BTreeMap::new(),
                },
            )
            .is_some()
        {
            return Err(ModelError::DuplicateObjectId);
        }
        Ok(())
    }

    pub(crate) fn validate_secondary_index_insert(
        &self,
        index: ObjectId,
        index_key: &[u8],
        primary_key: &[u8],
        contains_null: bool,
    ) -> Result<(), ModelError> {
        let index = self.indexes.get(&index).ok_or(ModelError::UnknownObject)?;
        let entries = index.entries.get(index_key);
        if entries.is_some_and(|entries| entries.contains(primary_key)) {
            return Err(ModelError::DuplicateSecondaryIndexEntry);
        }
        if index.unique
            && !(contains_null && index.nulls_distinct)
            && entries.is_some_and(|entries| !entries.is_empty())
        {
            return Err(ModelError::UniqueSecondaryIndexViolation);
        }
        Ok(())
    }

    pub(crate) fn insert_secondary_index(
        &mut self,
        index: ObjectId,
        index_key: Vec<u8>,
        primary_key: Vec<u8>,
        contains_null: bool,
    ) -> Result<(), ModelError> {
        self.validate_secondary_index_insert(index, &index_key, &primary_key, contains_null)?;
        self.indexes
            .get_mut(&index)
            .ok_or(ModelError::UnknownObject)?
            .entries
            .entry(index_key)
            .or_default()
            .insert(primary_key);
        Ok(())
    }

    pub(crate) fn remove_secondary_index(
        &mut self,
        index: ObjectId,
        index_key: &[u8],
        primary_key: &[u8],
    ) -> Result<(), ModelError> {
        let index = self
            .indexes
            .get_mut(&index)
            .ok_or(ModelError::UnknownObject)?;
        let entries = index
            .entries
            .get_mut(index_key)
            .ok_or(ModelError::MissingSecondaryIndexEntry)?;
        if !entries.remove(primary_key) {
            return Err(ModelError::MissingSecondaryIndexEntry);
        }
        if entries.is_empty() {
            index.entries.remove(index_key);
        }
        Ok(())
    }

    pub(crate) fn secondary_index_lookup(
        &self,
        index: ObjectId,
        index_key: &[u8],
    ) -> Result<Option<&BTreeSet<Vec<u8>>>, ModelError> {
        let index = self.indexes.get(&index).ok_or(ModelError::UnknownObject)?;
        Ok(index.entries.get(index_key))
    }

    pub(crate) fn insert(
        &mut self,
        table: ObjectId,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<(), ModelError> {
        let rows = self
            .tables
            .get_mut(&table)
            .ok_or(ModelError::UnknownObject)?;
        if rows.insert(key, value).is_some() {
            return Err(ModelError::DuplicatePrimaryKey);
        }
        Ok(())
    }

    pub(crate) fn select(&self, table: ObjectId, key: &[u8]) -> Option<&[u8]> {
        self.tables
            .get(&table)
            .and_then(|rows| rows.get(key))
            .map(Vec::as_slice)
    }

    pub(crate) fn update(
        &mut self,
        table: ObjectId,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<(), ModelError> {
        let row = self
            .tables
            .get_mut(&table)
            .ok_or(ModelError::UnknownObject)?
            .get_mut(key)
            .ok_or(ModelError::MissingPrimaryKey)?;
        *row = value;
        Ok(())
    }

    pub(crate) fn delete(&mut self, table: ObjectId, key: &[u8]) -> Result<(), ModelError> {
        let rows = self
            .tables
            .get_mut(&table)
            .ok_or(ModelError::UnknownObject)?;
        rows.remove(key).ok_or(ModelError::MissingPrimaryKey)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StructureEntry {
    pub(crate) value: Vec<u8>,
    pub(crate) expires_at_micros: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SortedSetScore(u64);

impl SortedSetScore {
    pub(crate) fn new(value: f64) -> Option<Self> {
        if value.is_nan() {
            return None;
        }
        Some(Self(if value == 0.0 { 0 } else { value.to_bits() }))
    }

    pub(crate) fn from_canonical_bits(bits: u64) -> Option<Self> {
        let value = f64::from_bits(bits);
        if value.is_nan() || bits == (-0.0_f64).to_bits() {
            return None;
        }
        Some(Self(bits))
    }

    pub(crate) const fn canonical_bits(self) -> u64 {
        self.0
    }

    pub(crate) const fn value(self) -> f64 {
        f64::from_bits(self.0)
    }

    pub(crate) const fn sortable_bits(self) -> u64 {
        if self.0 & (1_u64 << 63) == 0 {
            self.0 ^ (1_u64 << 63)
        } else {
            !self.0
        }
    }

    pub(crate) fn from_sortable_bits(bits: u64) -> Option<Self> {
        let canonical = if bits & (1_u64 << 63) == 0 {
            !bits
        } else {
            bits ^ (1_u64 << 63)
        };
        Self::from_canonical_bits(canonical)
    }
}

impl Ord for SortedSetScore {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value().total_cmp(&other.value())
    }
}

impl PartialOrd for SortedSetScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TtlValue {
    Persistent,
    Remaining(i64),
}

pub(crate) type StreamFields = Vec<(Vec<u8>, Vec<u8>)>;
pub(crate) type StreamEntries = BTreeMap<u64, StreamFields>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StructureState {
    pub(crate) entries: BTreeMap<Vec<u8>, StructureEntry>,
    pub(crate) hashes: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>,
    pub(crate) hash_expiries: BTreeMap<Vec<u8>, i64>,
    pub(crate) hash_field_expiries: BTreeMap<(Vec<u8>, Vec<u8>), i64>,
    pub(crate) sets: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    pub(crate) set_expiries: BTreeMap<Vec<u8>, i64>,
    pub(crate) lists: BTreeMap<Vec<u8>, VecDeque<Vec<u8>>>,
    pub(crate) list_expiries: BTreeMap<Vec<u8>, i64>,
    pub(crate) sorted_sets: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, SortedSetScore>>,
    pub(crate) streams: BTreeMap<Vec<u8>, StreamEntries>,
    pub(crate) stream_expiries: BTreeMap<Vec<u8>, i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ListPop {
    Missing,
    Empty,
    Value(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortedSetMemberState {
    MissingSet,
    MissingMember,
    Present(SortedSetScore),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortedSetRankState {
    MissingSet,
    MissingMember,
    Present { forward: usize, reverse: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortedSetDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HashPatternModelStop {
    Exhausted,
    OutputLimit,
    VisitLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HashPatternModelPage {
    pub(crate) entries: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) continuation: Option<Vec<u8>>,
    pub(crate) stop: HashPatternModelStop,
    pub(crate) visited: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HashPatternModelRequest<'request> {
    pub(crate) leading_literal_prefix: &'request [u8],
    pub(crate) exact_literal: bool,
    pub(crate) start_after: Option<&'request [u8]>,
    pub(crate) output_limit: usize,
    pub(crate) visit_limit: usize,
    pub(crate) logical_time_micros: i64,
}

impl StructureState {
    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn create_stream(&mut self, key: Vec<u8>) -> Result<(), ModelError> {
        if self.streams.contains_key(&key) {
            return Err(ModelError::DuplicateEncodedEntry);
        }
        self.streams.insert(key, BTreeMap::new());
        Ok(())
    }

    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn delete_stream(&mut self, key: &[u8]) -> Option<StreamEntries> {
        self.stream_expiries.remove(key);
        self.streams.remove(key)
    }

    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn expire_stream(&mut self, key: &[u8], expires_at_micros: i64) -> bool {
        if !self.streams.contains_key(key) {
            return false;
        }
        self.stream_expiries.insert(key.to_vec(), expires_at_micros);
        true
    }

    pub(crate) fn stream_is_visible(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.streams.contains_key(key)
            && self
                .stream_expiries
                .get(key)
                .is_none_or(|expiry| *expiry > logical_time_micros)
    }

    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn xadd_with_id(
        &mut self,
        key: &[u8],
        id: u64,
        fields: StreamFields,
    ) -> Result<(), ModelError> {
        if id == 0 || fields.is_empty() {
            return Err(ModelError::DuplicateEncodedEntry);
        }
        let mut seen = BTreeSet::new();
        if fields
            .iter()
            .any(|(field, _)| !seen.insert(field.as_slice()))
        {
            return Err(ModelError::DuplicateEncodedEntry);
        }
        let stream = self.streams.get_mut(key).ok_or(ModelError::UnknownObject)?;
        if stream.last_key_value().is_some_and(|(last, _)| id <= *last) {
            return Err(ModelError::DuplicateEncodedEntry);
        }
        stream.insert(id, fields);
        Ok(())
    }

    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn xadd(&mut self, key: &[u8], fields: StreamFields) -> Result<u64, ModelError> {
        let id = self
            .streams
            .get(key)
            .ok_or(ModelError::UnknownObject)?
            .last_key_value()
            .map_or(Some(1), |(id, _)| id.checked_add(1))
            .ok_or(ModelError::LengthOverflow)?;
        self.xadd_with_id(key, id, fields)?;
        Ok(id)
    }

    #[allow(dead_code, reason = "first G3 stream model slice; WAL wiring follows")]
    pub(crate) fn xrange(
        &self,
        key: &[u8],
        start: u64,
        end: u64,
        limit: usize,
    ) -> Option<Vec<(u64, StreamFields)>> {
        let stream = self.streams.get(key)?;
        if !self.stream_is_visible(key, i64::MIN) {
            return None;
        }
        Some(
            stream
                .range(start..=end)
                .take(limit)
                .map(|(id, fields)| (*id, fields.clone()))
                .collect(),
        )
    }

    pub(crate) fn set(&mut self, key: Vec<u8>, value: Vec<u8>, expires_at_micros: Option<i64>) {
        self.entries.insert(
            key,
            StructureEntry {
                value,
                expires_at_micros,
            },
        );
    }

    pub(crate) fn get(&self, key: &[u8], logical_time_micros: i64) -> Option<&[u8]> {
        self.visible_entry(key, logical_time_micros)
            .map(|entry| entry.value.as_slice())
    }

    pub(crate) fn visible_entry(
        &self,
        key: &[u8],
        logical_time_micros: i64,
    ) -> Option<&StructureEntry> {
        self.entries.get(key).filter(|entry| {
            entry
                .expires_at_micros
                .is_none_or(|expiry| expiry > logical_time_micros)
        })
    }

    pub(crate) fn delete(&mut self, key: &[u8]) -> Option<StructureEntry> {
        self.entries.remove(key)
    }

    pub(crate) fn create_hash(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
            || self.streams.contains_key(&key)
        {
            return false;
        }
        self.hashes.insert(key, BTreeMap::new());
        true
    }

    pub(crate) fn delete_hash(&mut self, key: &[u8]) -> bool {
        self.hash_expiries.remove(key);
        self.hash_field_expiries
            .retain(|(hash, _), _| hash.as_slice() != key);
        self.hashes.remove(key).is_some()
    }

    pub(crate) fn hash_is_visible(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.hashes.contains_key(key)
            && self
                .hash_expiries
                .get(key)
                .is_none_or(|expiry| *expiry > logical_time_micros)
    }

    pub(crate) fn hash_is_expired(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.hashes.contains_key(key)
            && self
                .hash_expiries
                .get(key)
                .is_some_and(|expiry| *expiry <= logical_time_micros)
    }

    pub(crate) fn hash_expiry(&self, key: &[u8]) -> Option<i64> {
        self.hash_expiries.get(key).copied()
    }

    pub(crate) fn expire_hash(
        &mut self,
        key: &[u8],
        expires_at_micros: i64,
        logical_time_micros: i64,
    ) -> bool {
        if !self.hash_is_visible(key, logical_time_micros) {
            return false;
        }
        self.set_hash_expiry(key, expires_at_micros)
    }

    pub(crate) fn set_hash_expiry(&mut self, key: &[u8], expires_at_micros: i64) -> bool {
        if !self.hashes.contains_key(key) {
            return false;
        }
        self.hash_expiries.insert(key.to_vec(), expires_at_micros);
        true
    }

    pub(crate) fn hash_field_expiry(&self, key: &[u8], field: &[u8]) -> Option<i64> {
        self.hash_field_expiries
            .get(&(key.to_vec(), field.to_vec()))
            .copied()
    }

    pub(crate) fn expire_hash_field(
        &mut self,
        key: &[u8],
        field: &[u8],
        expires_at_micros: i64,
        logical_time_micros: i64,
    ) -> Option<bool> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        if self.hget_at(key, field, logical_time_micros).is_none() {
            return Some(false);
        }
        Some(self.set_hash_field_expiry(key, field, expires_at_micros))
    }

    pub(crate) fn set_hash_field_expiry(
        &mut self,
        key: &[u8],
        field: &[u8],
        expires_at_micros: i64,
    ) -> bool {
        if self
            .hashes
            .get(key)
            .is_none_or(|fields| !fields.contains_key(field))
        {
            return false;
        }
        self.hash_field_expiries
            .insert((key.to_vec(), field.to_vec()), expires_at_micros);
        true
    }

    pub(crate) fn hset(&mut self, key: &[u8], field: Vec<u8>, value: Vec<u8>) -> Option<bool> {
        let fields = self.hashes.get_mut(key)?;
        self.hash_field_expiries
            .remove(&(key.to_vec(), field.clone()));
        Some(fields.insert(field, value).is_none())
    }

    pub(crate) fn hset_at(
        &mut self,
        key: &[u8],
        field: Vec<u8>,
        value: Vec<u8>,
        logical_time_micros: i64,
    ) -> Option<bool> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        let added = self.hget_at(key, &field, logical_time_micros).is_none();
        self.hset(key, field, value)?;
        Some(added)
    }

    pub(crate) fn hset_many(
        &mut self,
        key: &[u8],
        updates: &[(Vec<u8>, Vec<u8>)],
    ) -> Option<usize> {
        let fields = self.hashes.get_mut(key)?;
        let mut added = 0;
        for (field, value) in updates {
            self.hash_field_expiries
                .remove(&(key.to_vec(), field.clone()));
            added += usize::from(fields.insert(field.clone(), value.clone()).is_none());
        }
        Some(added)
    }

    pub(crate) fn hset_many_at(
        &mut self,
        key: &[u8],
        updates: &[(Vec<u8>, Vec<u8>)],
        logical_time_micros: i64,
    ) -> Option<usize> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        let added = updates
            .iter()
            .filter(|(field, _)| self.hget_at(key, field, logical_time_micros).is_none())
            .count();
        self.hset_many(key, updates)?;
        Some(added)
    }

    pub(crate) fn hget(&self, key: &[u8], field: &[u8]) -> Option<&[u8]> {
        self.hashes
            .get(key)
            .and_then(|fields| fields.get(field))
            .map(Vec::as_slice)
    }

    pub(crate) fn hget_at(
        &self,
        key: &[u8],
        field: &[u8],
        logical_time_micros: i64,
    ) -> Option<&[u8]> {
        if !self.hash_is_visible(key, logical_time_micros)
            || self
                .hash_field_expiry(key, field)
                .is_some_and(|expiry| expiry <= logical_time_micros)
        {
            return None;
        }
        self.hget(key, field)
    }

    pub(crate) fn hget_many_at(
        &self,
        key: &[u8],
        fields: &[Vec<u8>],
        logical_time_micros: i64,
    ) -> Option<Vec<Option<Vec<u8>>>> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        Some(
            fields
                .iter()
                .map(|field| {
                    self.hget_at(key, field, logical_time_micros)
                        .map(<[u8]>::to_vec)
                })
                .collect(),
        )
    }

    pub(crate) fn hdelete(&mut self, key: &[u8], field: &[u8]) -> Option<bool> {
        let fields = self.hashes.get_mut(key)?;
        let deleted = fields.remove(field).is_some();
        if deleted {
            self.hash_field_expiries
                .remove(&(key.to_vec(), field.to_vec()));
        }
        Some(deleted)
    }

    pub(crate) fn hdelete_many(&mut self, key: &[u8], fields: &[Vec<u8>]) -> Option<usize> {
        let hash = self.hashes.get_mut(key)?;
        let mut deleted = 0;
        for field in fields {
            if hash.remove(field.as_slice()).is_some() {
                self.hash_field_expiries
                    .remove(&(key.to_vec(), field.clone()));
                deleted += 1;
            }
        }
        Some(deleted)
    }

    #[cfg(test)]
    pub(crate) fn hincrement_i64(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: i64,
    ) -> Result<Option<i64>, ModelError> {
        let Some(hash) = self.hashes.get_mut(key) else {
            return Ok(None);
        };
        let base = hash
            .get(field)
            .map_or(Ok(0), |value| parse_canonical_hash_i64(value))?;
        let value = base
            .checked_add(delta)
            .ok_or(ModelError::StructureIntegerOverflow)?;
        hash.insert(field.to_vec(), value.to_string().into_bytes());
        self.hash_field_expiries
            .remove(&(key.to_vec(), field.to_vec()));
        Ok(Some(value))
    }

    pub(crate) fn hincrement_i64_at(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: i64,
        logical_time_micros: i64,
    ) -> Result<Option<i64>, ModelError> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return Ok(None);
        }
        let base = self
            .hget_at(key, field, logical_time_micros)
            .map_or(Ok(0), parse_canonical_hash_i64)?;
        let value = base
            .checked_add(delta)
            .ok_or(ModelError::StructureIntegerOverflow)?;
        self.hset(key, field.to_vec(), value.to_string().into_bytes());
        Ok(Some(value))
    }

    #[cfg(test)]
    pub(crate) fn hlen(&self, key: &[u8]) -> Option<usize> {
        self.hashes.get(key).map(BTreeMap::len)
    }

    pub(crate) fn hlen_at(&self, key: &[u8], logical_time_micros: i64) -> Option<usize> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        self.hashes.get(key).map(|fields| {
            fields
                .keys()
                .filter(|field| {
                    self.hash_field_expiry(key, field)
                        .is_none_or(|expiry| expiry > logical_time_micros)
                })
                .count()
        })
    }

    pub(crate) fn hscan_at(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
        logical_time_micros: i64,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        let fields = self.hashes.get(key)?;
        Some(
            fields
                .iter()
                .filter(|(field, _)| start_after.is_none_or(|cursor| field.as_slice() > cursor))
                .filter(|(field, _)| {
                    self.hash_field_expiry(key, field)
                        .is_none_or(|expiry| expiry > logical_time_micros)
                })
                .take(limit)
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect(),
        )
    }

    pub(crate) fn hscan_match_at<F, E>(
        &self,
        key: &[u8],
        request: HashPatternModelRequest<'_>,
        mut matcher: F,
    ) -> Result<Option<HashPatternModelPage>, E>
    where
        F: FnMut(&[u8]) -> Result<bool, E>,
    {
        if !self.hash_is_visible(key, request.logical_time_micros) {
            return Ok(None);
        }
        debug_assert!(request.output_limit > 0);
        debug_assert!(request.visit_limit > 0);
        let Some(fields) = self.hashes.get(key) else {
            return Ok(None);
        };
        if request.exact_literal {
            let Some(value) = fields.get(request.leading_literal_prefix) else {
                return Ok(Some(HashPatternModelPage {
                    entries: Vec::new(),
                    continuation: None,
                    stop: HashPatternModelStop::Exhausted,
                    visited: 0,
                }));
            };
            if request
                .start_after
                .is_some_and(|cursor| request.leading_literal_prefix <= cursor)
            {
                return Ok(Some(HashPatternModelPage {
                    entries: Vec::new(),
                    continuation: None,
                    stop: HashPatternModelStop::Exhausted,
                    visited: 0,
                }));
            }
            let visible = self
                .hash_field_expiry(key, request.leading_literal_prefix)
                .is_none_or(|expiry| expiry > request.logical_time_micros);
            let entries = if visible && matcher(request.leading_literal_prefix)? {
                vec![(request.leading_literal_prefix.to_vec(), value.clone())]
            } else {
                Vec::new()
            };
            return Ok(Some(HashPatternModelPage {
                entries,
                continuation: None,
                stop: HashPatternModelStop::Exhausted,
                visited: 1,
            }));
        }
        let mut entries = Vec::with_capacity(request.output_limit.min(request.visit_limit));
        let mut visited = 0;
        for (field, value) in fields
            .iter()
            .filter(|(field, _)| {
                request
                    .start_after
                    .is_none_or(|cursor| field.as_slice() > cursor)
            })
            .filter(|(field, _)| field.starts_with(request.leading_literal_prefix))
        {
            visited += 1;
            let continuation = Some(field.clone());
            let visible = self
                .hash_field_expiry(key, field)
                .is_none_or(|expiry| expiry > request.logical_time_micros);
            if visible && matcher(field)? {
                entries.push((field.clone(), value.clone()));
                if entries.len() == request.output_limit {
                    return Ok(Some(HashPatternModelPage {
                        entries,
                        continuation,
                        stop: HashPatternModelStop::OutputLimit,
                        visited,
                    }));
                }
            }
            if visited == request.visit_limit {
                return Ok(Some(HashPatternModelPage {
                    entries,
                    continuation,
                    stop: HashPatternModelStop::VisitLimit,
                    visited,
                }));
            }
        }
        Ok(Some(HashPatternModelPage {
            entries,
            continuation: None,
            stop: HashPatternModelStop::Exhausted,
            visited,
        }))
    }

    #[cfg(test)]
    pub(crate) fn hscan_reverse(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let fields = self.hashes.get(key)?;
        Some(
            fields
                .iter()
                .rev()
                .filter(|(field, _)| start_before.is_none_or(|cursor| field.as_slice() < cursor))
                .take(limit)
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect(),
        )
    }

    pub(crate) fn hscan_reverse_at(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
        logical_time_micros: i64,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        if !self.hash_is_visible(key, logical_time_micros) {
            return None;
        }
        let fields = self.hashes.get(key)?;
        Some(
            fields
                .iter()
                .rev()
                .filter(|(field, _)| start_before.is_none_or(|cursor| field.as_slice() < cursor))
                .filter(|(field, _)| {
                    self.hash_field_expiry(key, field)
                        .is_none_or(|expiry| expiry > logical_time_micros)
                })
                .take(limit)
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect(),
        )
    }

    pub(crate) fn create_set(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
            || self.streams.contains_key(&key)
        {
            return false;
        }
        self.sets.insert(key, BTreeSet::new());
        true
    }

    pub(crate) fn delete_set(&mut self, key: &[u8]) -> bool {
        self.set_expiries.remove(key);
        self.sets.remove(key).is_some()
    }

    pub(crate) fn set_is_visible(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.sets.contains_key(key)
            && self
                .set_expiries
                .get(key)
                .is_none_or(|expiry| *expiry > logical_time_micros)
    }

    pub(crate) fn set_is_expired(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.sets.contains_key(key)
            && self
                .set_expiries
                .get(key)
                .is_some_and(|expiry| *expiry <= logical_time_micros)
    }

    pub(crate) fn set_expiry(&self, key: &[u8]) -> Option<i64> {
        self.set_expiries.get(key).copied()
    }

    pub(crate) fn expire_set(
        &mut self,
        key: &[u8],
        expires_at_micros: i64,
        logical_time_micros: i64,
    ) -> bool {
        if !self.set_is_visible(key, logical_time_micros) {
            return false;
        }
        self.set_set_expiry(key, expires_at_micros)
    }

    pub(crate) fn set_set_expiry(&mut self, key: &[u8], expires_at_micros: i64) -> bool {
        if !self.sets.contains_key(key) {
            return false;
        }
        self.set_expiries.insert(key.to_vec(), expires_at_micros);
        true
    }

    pub(crate) fn sadd(&mut self, key: &[u8], member: Vec<u8>) -> Option<bool> {
        self.sets.get_mut(key).map(|members| members.insert(member))
    }

    pub(crate) fn sadd_at(
        &mut self,
        key: &[u8],
        member: Vec<u8>,
        logical_time_micros: i64,
    ) -> Option<bool> {
        self.set_is_visible(key, logical_time_micros)
            .then(|| self.sadd(key, member))
            .flatten()
    }

    pub(crate) fn sadd_many_at(
        &mut self,
        key: &[u8],
        members: &[Vec<u8>],
        logical_time_micros: i64,
    ) -> Option<usize> {
        if !self.set_is_visible(key, logical_time_micros) {
            return None;
        }
        self.sets.get_mut(key).map(|set| {
            members
                .iter()
                .filter(|member| set.insert((*member).clone()))
                .count()
        })
    }

    pub(crate) fn sismember(&self, key: &[u8], member: &[u8]) -> Option<bool> {
        self.sets.get(key).map(|members| members.contains(member))
    }

    pub(crate) fn sismember_at(
        &self,
        key: &[u8],
        member: &[u8],
        logical_time_micros: i64,
    ) -> Option<bool> {
        self.set_is_visible(key, logical_time_micros)
            .then(|| self.sismember(key, member))
            .flatten()
    }

    pub(crate) fn smismember_at(
        &self,
        key: &[u8],
        members: &[Vec<u8>],
        logical_time_micros: i64,
    ) -> Option<Vec<bool>> {
        if !self.set_is_visible(key, logical_time_micros) {
            return None;
        }
        self.sets
            .get(key)
            .map(|set| members.iter().map(|member| set.contains(member)).collect())
    }

    pub(crate) fn srem(&mut self, key: &[u8], member: &[u8]) -> Option<bool> {
        self.sets.get_mut(key).map(|members| members.remove(member))
    }

    pub(crate) fn srem_at(
        &mut self,
        key: &[u8],
        member: &[u8],
        logical_time_micros: i64,
    ) -> Option<bool> {
        self.set_is_visible(key, logical_time_micros)
            .then(|| self.srem(key, member))
            .flatten()
    }

    pub(crate) fn srem_many_at(
        &mut self,
        key: &[u8],
        members: &[Vec<u8>],
        logical_time_micros: i64,
    ) -> Option<usize> {
        if !self.set_is_visible(key, logical_time_micros) {
            return None;
        }
        self.sets.get_mut(key).map(|set| {
            members
                .iter()
                .filter(|member| set.remove(member.as_slice()))
                .count()
        })
    }

    pub(crate) fn scard(&self, key: &[u8]) -> Option<usize> {
        self.sets.get(key).map(BTreeSet::len)
    }

    pub(crate) fn scard_at(&self, key: &[u8], logical_time_micros: i64) -> Option<usize> {
        self.set_is_visible(key, logical_time_micros)
            .then(|| self.scard(key))
            .flatten()
    }

    pub(crate) fn sscan_at(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
        logical_time_micros: i64,
    ) -> Option<Vec<Vec<u8>>> {
        if !self.set_is_visible(key, logical_time_micros) {
            return None;
        }
        self.sets.get(key).map(|members| {
            let lower =
                start_after.map_or(Bound::Unbounded, |member| Bound::Excluded(member.to_vec()));
            members
                .range((lower, Bound::Unbounded))
                .take(limit)
                .cloned()
                .collect()
        })
    }

    pub(crate) fn create_list(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
            || self.streams.contains_key(&key)
        {
            return false;
        }
        self.lists.insert(key, VecDeque::new());
        true
    }

    pub(crate) fn delete_list(&mut self, key: &[u8]) -> bool {
        self.list_expiries.remove(key);
        self.lists.remove(key).is_some()
    }

    pub(crate) fn list_is_visible(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.lists.contains_key(key)
            && self
                .list_expiries
                .get(key)
                .is_none_or(|expiry| *expiry > logical_time_micros)
    }

    pub(crate) fn list_is_expired(&self, key: &[u8], logical_time_micros: i64) -> bool {
        self.lists.contains_key(key)
            && self
                .list_expiries
                .get(key)
                .is_some_and(|expiry| *expiry <= logical_time_micros)
    }

    pub(crate) fn list_expiry(&self, key: &[u8]) -> Option<i64> {
        self.list_expiries.get(key).copied()
    }

    pub(crate) fn expire_list(
        &mut self,
        key: &[u8],
        expires_at_micros: i64,
        logical_time_micros: i64,
    ) -> bool {
        if !self.list_is_visible(key, logical_time_micros) {
            return false;
        }
        self.set_list_expiry(key, expires_at_micros)
    }

    pub(crate) fn set_list_expiry(&mut self, key: &[u8], expires_at_micros: i64) -> bool {
        if !self.lists.contains_key(key) {
            return false;
        }
        self.list_expiries.insert(key.to_vec(), expires_at_micros);
        true
    }

    pub(crate) fn lpush(&mut self, key: &[u8], value: Vec<u8>) -> Option<usize> {
        self.lists.get_mut(key).map(|values| {
            values.push_front(value);
            values.len()
        })
    }

    pub(crate) fn lpush_at(
        &mut self,
        key: &[u8],
        value: Vec<u8>,
        logical_time_micros: i64,
    ) -> Option<usize> {
        self.list_is_visible(key, logical_time_micros)
            .then(|| self.lpush(key, value))
            .flatten()
    }

    pub(crate) fn rpush(&mut self, key: &[u8], value: Vec<u8>) -> Option<usize> {
        self.lists.get_mut(key).map(|values| {
            values.push_back(value);
            values.len()
        })
    }

    pub(crate) fn rpush_at(
        &mut self,
        key: &[u8],
        value: Vec<u8>,
        logical_time_micros: i64,
    ) -> Option<usize> {
        self.list_is_visible(key, logical_time_micros)
            .then(|| self.rpush(key, value))
            .flatten()
    }

    pub(crate) fn lpop(&mut self, key: &[u8]) -> ListPop {
        match self.lists.get_mut(key) {
            None => ListPop::Missing,
            Some(values) => values.pop_front().map_or(ListPop::Empty, ListPop::Value),
        }
    }

    pub(crate) fn lpop_at(&mut self, key: &[u8], logical_time_micros: i64) -> ListPop {
        if self.list_is_visible(key, logical_time_micros) {
            self.lpop(key)
        } else {
            ListPop::Missing
        }
    }

    pub(crate) fn rpop(&mut self, key: &[u8]) -> ListPop {
        match self.lists.get_mut(key) {
            None => ListPop::Missing,
            Some(values) => values.pop_back().map_or(ListPop::Empty, ListPop::Value),
        }
    }

    pub(crate) fn rpop_at(&mut self, key: &[u8], logical_time_micros: i64) -> ListPop {
        if self.list_is_visible(key, logical_time_micros) {
            self.rpop(key)
        } else {
            ListPop::Missing
        }
    }

    pub(crate) fn llen(&self, key: &[u8]) -> Option<usize> {
        self.lists.get(key).map(VecDeque::len)
    }

    pub(crate) fn llen_at(&self, key: &[u8], logical_time_micros: i64) -> Option<usize> {
        self.list_is_visible(key, logical_time_micros)
            .then(|| self.llen(key))
            .flatten()
    }

    pub(crate) fn lrange(&self, key: &[u8], start: i64, stop: i64) -> Option<Vec<Vec<u8>>> {
        let values = self.lists.get(key)?;
        let Some((start, stop)) = normalize_list_range(values.len(), start, stop) else {
            return Some(Vec::new());
        };
        Some(
            values
                .range(start..=stop)
                .cloned()
                .collect::<Vec<Vec<u8>>>(),
        )
    }

    pub(crate) fn lrange_at(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
        logical_time_micros: i64,
    ) -> Option<Vec<Vec<u8>>> {
        self.list_is_visible(key, logical_time_micros)
            .then(|| self.lrange(key, start, stop))
            .flatten()
    }

    #[allow(dead_code, reason = "G3 sorted-set lifecycle WAL wiring follows")]
    pub(crate) fn delete_sorted_set(
        &mut self,
        key: &[u8],
    ) -> Option<BTreeMap<Vec<u8>, SortedSetScore>> {
        self.sorted_sets.remove(key)
    }

    pub(crate) fn create_sorted_set(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
            || self.streams.contains_key(&key)
        {
            return false;
        }
        self.sorted_sets.insert(key, BTreeMap::new());
        true
    }

    pub(crate) fn zadd(
        &mut self,
        key: &[u8],
        member: Vec<u8>,
        score: SortedSetScore,
    ) -> SortedSetMemberState {
        let Some(members) = self.sorted_sets.get_mut(key) else {
            return SortedSetMemberState::MissingSet;
        };
        members.insert(member, score).map_or(
            SortedSetMemberState::MissingMember,
            SortedSetMemberState::Present,
        )
    }

    pub(crate) fn zscore(&self, key: &[u8], member: &[u8]) -> SortedSetMemberState {
        let Some(members) = self.sorted_sets.get(key) else {
            return SortedSetMemberState::MissingSet;
        };
        members.get(member).copied().map_or(
            SortedSetMemberState::MissingMember,
            SortedSetMemberState::Present,
        )
    }

    pub(crate) fn zrem(&mut self, key: &[u8], member: &[u8]) -> SortedSetMemberState {
        let Some(members) = self.sorted_sets.get_mut(key) else {
            return SortedSetMemberState::MissingSet;
        };
        members.remove(member).map_or(
            SortedSetMemberState::MissingMember,
            SortedSetMemberState::Present,
        )
    }

    pub(crate) fn zcard(&self, key: &[u8]) -> Option<usize> {
        self.sorted_sets.get(key).map(BTreeMap::len)
    }

    pub(crate) fn sorted_set_ranks(&self, key: &[u8], member: &[u8]) -> SortedSetRankState {
        let Some(members) = self.sorted_sets.get(key) else {
            return SortedSetRankState::MissingSet;
        };
        let Some(target_score) = members.get(member).copied() else {
            return SortedSetRankState::MissingMember;
        };
        let forward = members
            .iter()
            .filter(|(candidate, score)| (**score, candidate.as_slice()) < (target_score, member))
            .count();
        SortedSetRankState::Present {
            forward,
            reverse: members.len() - forward - 1,
        }
    }

    pub(crate) fn zrange(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        self.sorted_set_rank_range(key, start, stop, SortedSetDirection::Ascending)
    }

    pub(crate) fn zrevrange(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        self.sorted_set_rank_range(key, start, stop, SortedSetDirection::Descending)
    }

    fn sorted_set_rank_range(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
        direction: SortedSetDirection,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        let members = self.sorted_sets.get(key)?;
        let Some((start, stop)) = normalize_list_range(members.len(), start, stop) else {
            return Some(Vec::new());
        };
        let mut ordered = members
            .iter()
            .map(|(member, score)| (*score, member))
            .collect::<Vec<_>>();
        ordered.sort_unstable();
        if direction == SortedSetDirection::Descending {
            ordered.reverse();
        }
        Some(
            ordered[start..=stop]
                .iter()
                .map(|(score, member)| ((*member).clone(), *score))
                .collect(),
        )
    }

    pub(crate) fn zrange_by_score(
        &self,
        key: &[u8],
        lower: Bound<SortedSetScore>,
        upper: Bound<SortedSetScore>,
        offset: usize,
        limit: usize,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        self.sorted_set_score_range(
            key,
            lower,
            upper,
            offset,
            limit,
            SortedSetDirection::Ascending,
        )
    }

    pub(crate) fn zrevrange_by_score(
        &self,
        key: &[u8],
        lower: Bound<SortedSetScore>,
        upper: Bound<SortedSetScore>,
        offset: usize,
        limit: usize,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        self.sorted_set_score_range(
            key,
            lower,
            upper,
            offset,
            limit,
            SortedSetDirection::Descending,
        )
    }

    fn sorted_set_score_range(
        &self,
        key: &[u8],
        lower: Bound<SortedSetScore>,
        upper: Bound<SortedSetScore>,
        offset: usize,
        limit: usize,
        direction: SortedSetDirection,
    ) -> Option<Vec<(Vec<u8>, SortedSetScore)>> {
        let members = self.sorted_sets.get(key)?;
        if limit == 0 || sorted_set_score_range_is_empty(&lower, &upper) {
            return Some(Vec::new());
        }
        let mut ordered = members
            .iter()
            .map(|(member, score)| (*score, member))
            .collect::<Vec<_>>();
        ordered.sort_unstable();
        if direction == SortedSetDirection::Descending {
            ordered.reverse();
        }
        Some(
            ordered
                .into_iter()
                .filter(|(score, _)| sorted_set_score_is_within(*score, &lower, &upper))
                .skip(offset)
                .take(limit)
                .map(|(score, member)| (member.clone(), score))
                .collect(),
        )
    }

    pub(crate) fn ttl_micros(&self, key: &[u8], logical_time_micros: i64) -> Option<TtlValue> {
        self.visible_entry(key, logical_time_micros).map(|entry| {
            entry
                .expires_at_micros
                .map_or(TtlValue::Persistent, |expiry| {
                    TtlValue::Remaining(expiry.saturating_sub(logical_time_micros))
                })
        })
    }

    pub(crate) fn ttl_hash_micros(&self, key: &[u8], logical_time_micros: i64) -> Option<TtlValue> {
        self.hash_is_visible(key, logical_time_micros).then(|| {
            self.hash_expiry(key)
                .map_or(TtlValue::Persistent, |expiry| {
                    TtlValue::Remaining(expiry.saturating_sub(logical_time_micros))
                })
        })
    }

    pub(crate) fn ttl_hash_field_micros(
        &self,
        key: &[u8],
        field: &[u8],
        logical_time_micros: i64,
    ) -> Option<TtlValue> {
        self.hget_at(key, field, logical_time_micros).map(|_| {
            self.hash_field_expiry(key, field)
                .map_or(TtlValue::Persistent, |expiry| {
                    TtlValue::Remaining(expiry.saturating_sub(logical_time_micros))
                })
        })
    }

    pub(crate) fn ttl_set_micros(&self, key: &[u8], logical_time_micros: i64) -> Option<TtlValue> {
        self.set_is_visible(key, logical_time_micros).then(|| {
            self.set_expiry(key).map_or(TtlValue::Persistent, |expiry| {
                TtlValue::Remaining(expiry.saturating_sub(logical_time_micros))
            })
        })
    }

    pub(crate) fn ttl_list_micros(&self, key: &[u8], logical_time_micros: i64) -> Option<TtlValue> {
        self.list_is_visible(key, logical_time_micros).then(|| {
            self.list_expiry(key)
                .map_or(TtlValue::Persistent, |expiry| {
                    TtlValue::Remaining(expiry.saturating_sub(logical_time_micros))
                })
        })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, ModelError> {
        if !self.hashes.is_empty()
            || !self.hash_expiries.is_empty()
            || !self.sets.is_empty()
            || !self.set_expiries.is_empty()
            || !self.lists.is_empty()
            || !self.list_expiries.is_empty()
            || !self.sorted_sets.is_empty()
        {
            return Err(ModelError::UnsupportedLegacyStructureFamily);
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&STRUCTURE_MAGIC);
        put_len(&mut bytes, self.entries.len())?;
        for (key, entry) in &self.entries {
            put_bytes(&mut bytes, key)?;
            bytes.extend_from_slice(&entry.expires_at_micros.unwrap_or(i64::MAX).to_le_bytes());
            put_bytes(&mut bytes, &entry.value)?;
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut decoder = Decoder::new(bytes, STRUCTURE_MAGIC)?;
        let count = decoder.len()?;
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let key = decoder.bytes()?;
            let raw_expiry = decoder.i64()?;
            let value = decoder.bytes()?;
            let entry = StructureEntry {
                value,
                expires_at_micros: (raw_expiry != i64::MAX).then_some(raw_expiry),
            };
            if entries.insert(key, entry).is_some() {
                return Err(ModelError::DuplicateEncodedEntry);
            }
        }
        decoder.finish()?;
        Ok(Self {
            entries,
            hashes: BTreeMap::new(),
            hash_expiries: BTreeMap::new(),
            hash_field_expiries: BTreeMap::new(),
            sets: BTreeMap::new(),
            set_expiries: BTreeMap::new(),
            lists: BTreeMap::new(),
            list_expiries: BTreeMap::new(),
            sorted_sets: BTreeMap::new(),
            streams: BTreeMap::new(),
            stream_expiries: BTreeMap::new(),
        })
    }
}

fn parse_canonical_hash_i64(value: &[u8]) -> Result<i64, ModelError> {
    let text = std::str::from_utf8(value).map_err(|_| ModelError::StructureValueNotInteger)?;
    let parsed = text
        .parse::<i64>()
        .map_err(|_| ModelError::StructureValueNotInteger)?;
    if parsed.to_string().as_bytes() != value {
        return Err(ModelError::StructureValueNotInteger);
    }
    Ok(parsed)
}

pub(crate) fn normalize_list_range(length: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    if length == 0 {
        return None;
    }
    let length = i128::try_from(length).ok()?;
    let normalize = |index: i64| {
        let index = i128::from(index);
        if index < 0 { length + index } else { index }
    };
    let start = normalize(start).max(0);
    let stop = normalize(stop).min(length - 1);
    if start >= length || stop < 0 || start > stop {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(stop).ok()?))
}

pub(crate) fn sorted_set_score_range_is_empty(
    lower: &Bound<SortedSetScore>,
    upper: &Bound<SortedSetScore>,
) -> bool {
    match (lower, upper) {
        (Bound::Included(lower), Bound::Included(upper)) => lower > upper,
        (
            Bound::Included(lower) | Bound::Excluded(lower),
            Bound::Included(upper) | Bound::Excluded(upper),
        ) => lower >= upper,
        _ => false,
    }
}

fn sorted_set_score_is_within(
    score: SortedSetScore,
    lower: &Bound<SortedSetScore>,
    upper: &Bound<SortedSetScore>,
) -> bool {
    let above_lower = match lower {
        Bound::Included(lower) => score >= *lower,
        Bound::Excluded(lower) => score > *lower,
        Bound::Unbounded => true,
    };
    let below_upper = match upper {
        Bound::Included(upper) => score <= *upper,
        Bound::Excluded(upper) => score < *upper,
        Bound::Unbounded => true,
    };
    above_lower && below_upper
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SearchState {
    pub(crate) indexes: BTreeMap<ObjectId, BTreeMap<Vec<u8>, String>>,
}

impl SearchState {
    pub(crate) fn create_index(&mut self, id: ObjectId) -> Result<(), ModelError> {
        if self.indexes.insert(id, BTreeMap::new()).is_some() {
            return Err(ModelError::DuplicateObjectId);
        }
        Ok(())
    }

    pub(crate) fn index_document(
        &mut self,
        index: ObjectId,
        document_id: Vec<u8>,
        text: String,
    ) -> Result<(), ModelError> {
        let documents = self
            .indexes
            .get_mut(&index)
            .ok_or(ModelError::UnknownObject)?;
        if documents.insert(document_id, text).is_some() {
            return Err(ModelError::DuplicateDocumentId);
        }
        Ok(())
    }

    pub(crate) fn replace_document(
        &mut self,
        index: ObjectId,
        document_id: &[u8],
        text: String,
    ) -> Result<(), ModelError> {
        let documents = self
            .indexes
            .get_mut(&index)
            .ok_or(ModelError::UnknownObject)?;
        let document = documents
            .get_mut(document_id)
            .ok_or(ModelError::MissingDocumentId)?;
        *document = text;
        Ok(())
    }

    pub(crate) fn delete_document(
        &mut self,
        index: ObjectId,
        document_id: &[u8],
    ) -> Result<(), ModelError> {
        let documents = self
            .indexes
            .get_mut(&index)
            .ok_or(ModelError::UnknownObject)?;
        documents
            .remove(document_id)
            .ok_or(ModelError::MissingDocumentId)?;
        Ok(())
    }

    pub(crate) fn search(
        &self,
        index: ObjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, f64)>, ModelError> {
        let documents = self.indexes.get(&index).ok_or(ModelError::UnknownObject)?;
        let query_tokens: BTreeSet<String> = analyze(query).into_iter().collect();
        if query_tokens.is_empty() || limit == 0 || documents.is_empty() {
            return Ok(Vec::new());
        }
        let analyzed: Vec<(&Vec<u8>, Vec<String>)> = documents
            .iter()
            .map(|(id, text)| (id, analyze(text)))
            .collect();
        let document_count = count_f64(analyzed.len())?;
        let average_length = analyzed
            .iter()
            .map(|(_, tokens)| count_f64(tokens.len()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .sum::<f64>()
            / document_count;
        let mut hits = Vec::new();
        for (document_id, tokens) in &analyzed {
            let mut score = 0.0_f64;
            for query_token in &query_tokens {
                let term_frequency =
                    count_f64(tokens.iter().filter(|token| *token == query_token).count())?;
                if term_frequency == 0.0 {
                    continue;
                }
                let document_frequency = count_f64(
                    analyzed
                        .iter()
                        .filter(|(_, candidate)| candidate.iter().any(|token| token == query_token))
                        .count(),
                )?;
                score += bm25_term_score(
                    bm25_idf(document_count, document_frequency),
                    term_frequency,
                    count_f64(tokens.len())?,
                    average_length,
                );
            }
            if score > 0.0 {
                hits.push(((*document_id).clone(), score));
            }
        }
        hits.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, ModelError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SEARCH_MAGIC);
        put_len(&mut bytes, self.indexes.len())?;
        for (index, documents) in &self.indexes {
            bytes.extend_from_slice(&index.get().to_le_bytes());
            put_len(&mut bytes, documents.len())?;
            for (document_id, text) in documents {
                put_bytes(&mut bytes, document_id)?;
                put_bytes(&mut bytes, text.as_bytes())?;
            }
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut decoder = Decoder::new(bytes, SEARCH_MAGIC)?;
        let count = decoder.len()?;
        let mut indexes = BTreeMap::new();
        for _ in 0..count {
            let id = decoder.object_id()?;
            let document_count = decoder.len()?;
            let mut documents = BTreeMap::new();
            for _ in 0..document_count {
                let document_id = decoder.bytes()?;
                let text = decoder.string()?;
                if documents.insert(document_id, text).is_some() {
                    return Err(ModelError::DuplicateEncodedEntry);
                }
            }
            if indexes.insert(id, documents).is_some() {
                return Err(ModelError::DuplicateEncodedEntry);
            }
        }
        decoder.finish()?;
        Ok(Self { indexes })
    }
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn analyze(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(crate) fn bm25_idf(document_count: f64, document_frequency: f64) -> f64 {
    (1.0 + (document_count - document_frequency + 0.5) / (document_frequency + 0.5)).ln()
}

pub(crate) fn bm25_term_score(
    idf: f64,
    term_frequency: f64,
    document_length: f64,
    average_length: f64,
) -> f64 {
    let normalization = 1.2 * (1.0 - 0.75 + 0.75 * (document_length / average_length));
    idf * (term_frequency * 2.2) / (term_frequency + normalization)
}

fn put_len(bytes: &mut Vec<u8>, value: usize) -> Result<(), ModelError> {
    let value = u32::try_from(value).map_err(|_| ModelError::LengthOverflow)?;
    bytes.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn count_f64(value: usize) -> Result<f64, ModelError> {
    u32::try_from(value)
        .map(f64::from)
        .map_err(|_| ModelError::LengthOverflow)
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ModelError> {
    put_len(output, value.len())?;
    output.extend_from_slice(value);
    Ok(())
}

fn decode_engine(value: u8) -> Result<EngineKind, ModelError> {
    match value {
        0 => Ok(EngineKind::Kernel),
        1 => Ok(EngineKind::Relational),
        2 => Ok(EngineKind::Structure),
        3 => Ok(EngineKind::Search),
        _ => Err(ModelError::WrongEngine),
    }
}

struct Decoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> Decoder<'bytes> {
    fn new(bytes: &'bytes [u8], magic: [u8; 8]) -> Result<Self, ModelError> {
        if bytes.get(..8) != Some(magic.as_slice()) {
            return Err(ModelError::InvalidMagic);
        }
        Ok(Self { bytes, offset: 8 })
    }

    fn take(&mut self, length: usize) -> Result<&'bytes [u8], ModelError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ModelError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ModelError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ModelError> {
        self.take(1)
            .and_then(|bytes| bytes.first().copied().ok_or(ModelError::Truncated))
    }

    fn len(&mut self) -> Result<usize, ModelError> {
        let mut encoded = [0_u8; 4];
        encoded.copy_from_slice(self.take(4)?);
        usize::try_from(u32::from_le_bytes(encoded)).map_err(|_| ModelError::LengthOverflow)
    }

    fn i64(&mut self) -> Result<i64, ModelError> {
        let mut encoded = [0_u8; 8];
        encoded.copy_from_slice(self.take(8)?);
        Ok(i64::from_le_bytes(encoded))
    }

    fn object_id(&mut self) -> Result<ObjectId, ModelError> {
        let mut encoded = [0_u8; 16];
        encoded.copy_from_slice(self.take(16)?);
        ObjectId::new(u128::from_le_bytes(encoded)).map_err(|_| ModelError::ZeroObjectId)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ModelError> {
        let length = self.len()?;
        Ok(self.take(length)?.to_vec())
    }

    fn string(&mut self) -> Result<String, ModelError> {
        String::from_utf8(self.bytes()?).map_err(|_| ModelError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), ModelError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ModelError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use hyphae_native_catalog::CatalogObject;
    use hyphae_native_types::{EngineKind, ObjectId};

    use super::{
        CATALOG_MAGIC_V1, CATALOG_MAGIC_V2, CatalogState, HashPatternModelRequest,
        HashPatternModelStop, ModelError, RelationState, SearchState, SortedSetMemberState,
        SortedSetScore, StructureState, TtlValue, legacy_catalog_object, put_bytes, put_len,
    };

    #[test]
    fn every_engine_state_has_a_canonical_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;

        let mut catalog = CatalogState::default();
        catalog.create(legacy_catalog_object(
            table,
            EngineKind::Relational,
            "Accounts",
        )?)?;
        catalog.create(legacy_catalog_object(index, EngineKind::Search, "Notes")?)?;
        assert_eq!(CatalogState::decode(&catalog.encode()?)?, catalog);

        let mut relational = RelationState::default();
        relational.create_table(table)?;
        relational.insert(table, b"mario".to_vec(), b"active".to_vec())?;
        assert_eq!(
            relational.select(table, b"mario"),
            Some(b"active".as_slice())
        );

        let mut structures = StructureState::default();
        structures.set(b"session".to_vec(), b"open".to_vec(), Some(50));
        assert_eq!(StructureState::decode(&structures.encode()?)?, structures);

        let mut search = SearchState::default();
        search.create_index(index)?;
        search.index_document(index, b"doc-1".to_vec(), "native search".to_owned())?;
        assert_eq!(SearchState::decode(&search.encode()?)?, search);
        Ok(())
    }

    #[test]
    fn legacy_catalog_names_reconstruct_fixed_definitions_and_upgrade_on_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;
        let mut encoded = CATALOG_MAGIC_V1.to_vec();
        put_len(&mut encoded, 2)?;
        encoded.extend_from_slice(&table.get().to_le_bytes());
        encoded.push(EngineKind::Relational as u8);
        put_bytes(&mut encoded, b"Accounts")?;
        encoded.extend_from_slice(&index.get().to_le_bytes());
        encoded.push(EngineKind::Search as u8);
        put_bytes(&mut encoded, b"Notes")?;

        let decoded = CatalogState::decode(&encoded)?;
        let Some(CatalogObject::Relation(relation)) = decoded.object(table) else {
            return Err("legacy relation was not reconstructed".into());
        };
        assert_eq!(relation.columns.len(), 2);
        assert_eq!(relation.header.name.object.lookup(), "accounts");
        let Some(CatalogObject::Search(search)) = decoded.object(index) else {
            return Err("legacy search definition was not reconstructed".into());
        };
        assert_eq!(search.fields.len(), 1);
        assert_eq!(search.header.name.object.lookup(), "notes");
        assert!(decoded.encode()?.starts_with(&CATALOG_MAGIC_V2));
        Ok(())
    }

    #[test]
    fn catalog_rejects_names_with_the_same_normalized_lookup()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut catalog = CatalogState::default();
        catalog.create(legacy_catalog_object(
            ObjectId::new(1)?,
            EngineKind::Relational,
            "Accounts",
        )?)?;
        assert_eq!(
            catalog.create(legacy_catalog_object(
                ObjectId::new(2)?,
                EngineKind::Relational,
                "accounts",
            )?),
            Err(ModelError::DuplicateObjectName)
        );
        Ok(())
    }

    #[test]
    fn relational_table_drop_is_restrictive_and_removes_rows()
    -> Result<(), Box<dyn std::error::Error>> {
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;
        let mut relational = RelationState::default();
        relational.create_table(table)?;
        relational.insert(table, b"pk".to_vec(), b"row".to_vec())?;
        relational.create_secondary_index(index, table, false, true)?;
        assert!(relational.drop_table(table).is_err());
        relational.drop_secondary_index(index)?;
        let removed = relational.drop_table(table)?;
        assert_eq!(removed.get(b"pk".as_slice()), Some(&b"row".to_vec()));
        assert!(matches!(
            relational.drop_table(table),
            Err(ModelError::UnknownObject)
        ));
        Ok(())
    }

    #[test]
    fn relational_index_drop_is_strict_and_removes_all_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;
        let mut relational = RelationState::default();
        relational.create_table(table)?;
        relational.create_secondary_index(index, table, false, true)?;
        relational.insert_secondary_index(index, b"email".to_vec(), b"pk".to_vec(), false)?;
        let removed = relational.drop_secondary_index(index)?;
        assert_eq!(removed.relation, table);
        assert_eq!(removed.entries.len(), 1);
        assert!(matches!(
            relational.drop_secondary_index(index),
            Err(ModelError::UnknownObject)
        ));
        Ok(())
    }

    #[test]
    fn sorted_set_model_delete_is_typed_and_retires_members()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = StructureState::default();
        assert!(state.create_sorted_set(b"scores".to_vec()));
        let score = SortedSetScore::new(1.0).ok_or("score")?;
        assert_eq!(
            state.zadd(b"scores", b"alice".to_vec(), score),
            SortedSetMemberState::MissingMember
        );
        assert_eq!(
            state
                .delete_sorted_set(b"scores")
                .ok_or("missing sorted set")?
                .len(),
            1
        );
        assert!(matches!(
            state.zscore(b"scores", b"alice"),
            SortedSetMemberState::MissingSet
        ));
        assert!(state.delete_sorted_set(b"scores").is_none());
        assert!(state.create_list(b"scores".to_vec()));
        Ok(())
    }

    #[test]
    fn stream_model_ttl_is_exact_at_controlled_time() -> Result<(), Box<dyn std::error::Error>> {
        let mut state = StructureState::default();
        state.create_stream(b"events".to_vec())?;
        state.xadd(b"events", vec![(b"kind".to_vec(), b"a".to_vec())])?;
        assert!(state.expire_stream(b"events", 10));
        assert!(state.stream_is_visible(b"events", 9));
        assert!(!state.stream_is_visible(b"events", 10));
        assert!(!state.stream_is_visible(b"events", 11));
        assert!(state.delete_stream(b"events").is_some());
        assert!(!state.expire_stream(b"events", 20));
        Ok(())
    }

    #[test]
    fn stream_model_replay_requires_strictly_increasing_explicit_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = StructureState::default();
        state.create_stream(b"events".to_vec())?;
        state.xadd_with_id(b"events", 7, vec![(b"kind".to_vec(), b"seed".to_vec())])?;
        assert!(
            state
                .xadd_with_id(
                    b"events",
                    7,
                    vec![(b"kind".to_vec(), b"duplicate".to_vec())],
                )
                .is_err()
        );
        assert!(
            state
                .xadd_with_id(
                    b"events",
                    6,
                    vec![(b"kind".to_vec(), b"retrograde".to_vec())],
                )
                .is_err()
        );
        assert_eq!(
            state.xadd(b"events", vec![(b"kind".to_vec(), b"next".to_vec())],)?,
            8
        );
        Ok(())
    }

    #[test]
    fn stream_model_delete_is_a_typed_lifecycle_boundary() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut state = StructureState::default();
        state.create_stream(b"events".to_vec())?;
        state.xadd(b"events", vec![(b"kind".to_vec(), b"a".to_vec())])?;
        assert_eq!(
            state
                .delete_stream(b"events")
                .ok_or("missing stream")?
                .len(),
            1
        );
        assert_eq!(state.xrange(b"events", 0, u64::MAX, 8), None);
        assert!(state.delete_stream(b"events").is_none());
        assert!(state.create_list(b"events".to_vec()));
        Ok(())
    }

    #[test]
    fn stream_model_is_typed_and_rejects_invalid_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut state = StructureState::default();
        state.create_stream(b"events".to_vec())?;
        assert!(matches!(
            state.create_stream(b"events".to_vec()),
            Err(ModelError::DuplicateEncodedEntry)
        ));
        assert!(matches!(
            state.xadd(b"events", Vec::new()),
            Err(ModelError::DuplicateEncodedEntry)
        ));
        assert!(matches!(
            state.xadd(
                b"events",
                vec![
                    (b"kind".to_vec(), b"a".to_vec()),
                    (b"kind".to_vec(), b"b".to_vec()),
                ],
            ),
            Err(ModelError::DuplicateEncodedEntry)
        ));
        assert!(!state.create_list(b"events".to_vec()));
        assert!(!state.create_set(b"events".to_vec()));
        Ok(())
    }

    #[test]
    fn stream_model_assigns_stable_monotonic_ids_and_exact_ranges()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = StructureState::default();
        state.create_stream(b"events".to_vec())?;
        assert_eq!(
            state.xadd(b"events", vec![(b"kind".to_vec(), b"a".to_vec())])?,
            1
        );
        assert_eq!(
            state.xadd(b"events", vec![(b"kind".to_vec(), b"b".to_vec())])?,
            2
        );
        assert_eq!(
            state.xrange(b"events", 1, 2, 1),
            Some(vec![(1, vec![(b"kind".to_vec(), b"a".to_vec())])])
        );
        assert_eq!(
            state
                .xrange(b"events", 2, u64::MAX, 8)
                .ok_or("missing stream")?
                .len(),
            1
        );
        assert_eq!(state.xrange(b"missing", 0, u64::MAX, 8), None);
        Ok(())
    }

    #[test]
    fn lexical_search_uses_bm25_and_stable_tie_breaks() -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(1)?;
        let mut search = SearchState::default();
        search.create_index(index)?;
        search.index_document(index, b"b".to_vec(), "rust storage".to_owned())?;
        search.index_document(index, b"a".to_vec(), "rust rust storage".to_owned())?;
        let hits = search.search(index, "rust", 10)?;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, b"a");
        assert!(hits[0].1 > hits[1].1);
        Ok(())
    }

    #[test]
    fn ttl_is_evaluated_against_snapshot_logical_time() {
        let mut structures = StructureState::default();
        structures.set(b"k".to_vec(), b"v".to_vec(), Some(10));
        assert_eq!(structures.get(b"k", 9), Some(b"v".as_slice()));
        assert_eq!(structures.get(b"k", 10), None);
        assert_eq!(structures.ttl_micros(b"k", 9), Some(TtlValue::Remaining(1)));
        assert_eq!(structures.ttl_micros(b"k", 10), None);
    }

    #[test]
    fn whole_hash_ttl_is_a_logical_incarnation_boundary() {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"profile".to_vec()));
        assert_eq!(
            structures.hset(b"profile", b"name".to_vec(), b"Mario".to_vec()),
            Some(true)
        );
        assert!(structures.expire_hash(b"profile", 10, 0));
        assert_eq!(
            structures.ttl_hash_micros(b"profile", 9),
            Some(TtlValue::Remaining(1))
        );
        assert_eq!(
            structures.hget_at(b"profile", b"name", 9),
            Some(b"Mario".as_slice())
        );
        assert_eq!(structures.ttl_hash_micros(b"profile", 10), None);
        assert_eq!(structures.hget_at(b"profile", b"name", 10), None);
        assert!(!structures.expire_hash(b"profile", 20, 10));
        assert!(structures.delete_hash(b"profile"));
        assert!(structures.create_hash(b"profile".to_vec()));
        assert_eq!(
            structures.ttl_hash_micros(b"profile", 10),
            Some(TtlValue::Persistent)
        );
        assert_eq!(structures.hget_at(b"profile", b"name", 10), None);
    }

    #[test]
    fn whole_set_ttl_is_a_logical_incarnation_boundary() {
        let mut structures = StructureState::default();
        assert!(structures.create_set(b"members".to_vec()));
        assert_eq!(
            structures.sadd_at(b"members", b"one".to_vec(), 0),
            Some(true)
        );
        assert!(structures.expire_set(b"members", 10, 0));
        assert_eq!(
            structures.ttl_set_micros(b"members", 9),
            Some(TtlValue::Remaining(1))
        );
        assert_eq!(structures.sismember_at(b"members", b"one", 9), Some(true));
        assert_eq!(structures.ttl_set_micros(b"members", 10), None);
        assert_eq!(structures.scard_at(b"members", 10), None);
        assert!(!structures.expire_set(b"members", 20, 10));
        assert!(structures.delete_set(b"members"));
        assert!(structures.create_set(b"members".to_vec()));
        assert_eq!(
            structures.ttl_set_micros(b"members", 10),
            Some(TtlValue::Persistent)
        );
        assert_eq!(structures.sismember_at(b"members", b"one", 10), Some(false));
    }

    #[test]
    fn hash_field_commands_form_one_atomic_model_transition() {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"profile".to_vec()));
        assert_eq!(
            structures.hset_many(
                b"profile",
                &[
                    (b"age".to_vec(), b"40".to_vec()),
                    (b"name".to_vec(), b"Mario".to_vec()),
                ],
            ),
            Some(2)
        );
        assert_eq!(
            structures.hincrement_i64(b"profile", b"age", 2),
            Ok(Some(42))
        );
        assert_eq!(
            structures.hget_many_at(
                b"profile",
                &[b"name".to_vec(), b"missing".to_vec(), b"name".to_vec()],
                9,
            ),
            Some(vec![Some(b"Mario".to_vec()), None, Some(b"Mario".to_vec()),])
        );
        assert_eq!(
            structures.hdelete_many(b"profile", &[b"missing".to_vec(), b"name".to_vec()]),
            Some(1)
        );
        assert!(structures.expire_hash(b"profile", 10, 9));
        assert_eq!(
            structures.hget_many_at(b"profile", &[b"age".to_vec()], 10),
            None
        );
    }

    #[test]
    fn reverse_hash_scan_is_descending_and_cursor_exclusive() {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"map".to_vec()));
        for (field, value) in [
            (b"".as_slice(), b"empty".as_slice()),
            (b"a".as_slice(), b"one".as_slice()),
            (b"a\0".as_slice(), b"two".as_slice()),
            (b"b".as_slice(), b"three".as_slice()),
            (b"z".as_slice(), b"four".as_slice()),
        ] {
            assert_eq!(
                structures.hset(b"map", field.to_vec(), value.to_vec()),
                Some(true)
            );
        }
        assert_eq!(
            structures.hscan_reverse(b"map", None, 3),
            Some(vec![
                (b"z".to_vec(), b"four".to_vec()),
                (b"b".to_vec(), b"three".to_vec()),
                (b"a\0".to_vec(), b"two".to_vec()),
            ])
        );
        assert_eq!(
            structures.hscan_reverse(b"map", Some(b"b"), 10),
            Some(vec![
                (b"a\0".to_vec(), b"two".to_vec()),
                (b"a".to_vec(), b"one".to_vec()),
                (b"".to_vec(), b"empty".to_vec()),
            ])
        );
        assert!(structures.expire_hash(b"map", 10, 9));
        assert_eq!(structures.hscan_reverse_at(b"map", None, 1, 10), None);
    }

    #[test]
    fn hash_pattern_scan_pages_advance_across_nonmatches() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"map".to_vec()));
        for field in [b"a".as_slice(), b"user:1", b"user:2", b"z"] {
            assert_eq!(
                structures.hset(b"map", field.to_vec(), field.to_vec()),
                Some(true)
            );
        }

        let first = structures
            .hscan_match_at(
                b"map",
                HashPatternModelRequest {
                    leading_literal_prefix: b"",
                    exact_literal: false,
                    start_after: None,
                    output_limit: 1,
                    visit_limit: 2,
                    logical_time_micros: 9,
                },
                |field| Ok::<_, std::convert::Infallible>(field.starts_with(b"user:")),
            )?
            .ok_or("pattern scan lost the hash")?;
        assert_eq!(first.entries, [(b"user:1".to_vec(), b"user:1".to_vec())]);
        assert_eq!(first.continuation, Some(b"user:1".to_vec()));
        assert_eq!(first.visited, 2);
        assert_eq!(first.stop, HashPatternModelStop::OutputLimit);

        let empty_progress = structures
            .hscan_match_at(
                b"map",
                HashPatternModelRequest {
                    leading_literal_prefix: b"",
                    exact_literal: false,
                    start_after: Some(b"user:2"),
                    output_limit: 1,
                    visit_limit: 1,
                    logical_time_micros: 9,
                },
                |field| Ok::<_, std::convert::Infallible>(field.starts_with(b"user:")),
            )?
            .ok_or("pattern scan lost the hash")?;
        assert!(empty_progress.entries.is_empty());
        assert_eq!(empty_progress.continuation, Some(b"z".to_vec()));
        assert_eq!(empty_progress.visited, 1);
        assert_eq!(empty_progress.stop, HashPatternModelStop::VisitLimit);
        Ok(())
    }

    #[test]
    fn hash_field_ttl_is_logical_and_replacement_clears_it() {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"profile".to_vec()));
        assert_eq!(
            structures.hset(b"profile", b"name".to_vec(), b"Mario".to_vec()),
            Some(true)
        );
        assert_eq!(
            structures.expire_hash_field(b"profile", b"name", 20, 10),
            Some(true)
        );
        assert_eq!(
            structures.ttl_hash_field_micros(b"profile", b"name", 10),
            Some(TtlValue::Remaining(10))
        );
        assert_eq!(
            structures.hget_at(b"profile", b"name", 19),
            Some(b"Mario".as_slice())
        );
        assert_eq!(structures.hget_at(b"profile", b"name", 20), None);
        assert_eq!(structures.hlen_at(b"profile", 20), Some(0));
        assert_eq!(
            structures.hscan_at(b"profile", None, 10, 20),
            Some(Vec::new())
        );
        assert_eq!(
            structures.hset_at(b"profile", b"name".to_vec(), b"mario".to_vec(), 20),
            Some(true)
        );
        assert_eq!(structures.hash_field_expiry(b"profile", b"name"), None);
        assert_eq!(
            structures.hget_at(b"profile", b"name", 20),
            Some(b"mario".as_slice())
        );
        assert_eq!(
            structures.ttl_hash_field_micros(b"profile", b"name", 20),
            Some(TtlValue::Persistent)
        );
    }

    #[test]
    fn hashes_are_typed_and_field_mutations_track_cardinality() {
        let mut structures = StructureState::default();
        assert!(structures.create_hash(b"profile".to_vec()));
        assert!(!structures.create_hash(b"profile".to_vec()));
        assert_eq!(
            structures.hset(b"profile", b"name".to_vec(), b"Mario".to_vec()),
            Some(true)
        );
        assert_eq!(
            structures.hset(b"profile", b"name".to_vec(), b"mario".to_vec()),
            Some(false)
        );
        assert_eq!(
            structures.hget(b"profile", b"name"),
            Some(b"mario".as_slice())
        );
        assert_eq!(structures.hlen(b"profile"), Some(1));
        assert_eq!(structures.hdelete(b"profile", b"missing"), Some(false));
        assert_eq!(structures.hdelete(b"profile", b"name"), Some(true));
        assert_eq!(structures.hlen(b"profile"), Some(0));
        assert!(matches!(
            structures.encode(),
            Err(ModelError::UnsupportedLegacyStructureFamily)
        ));
    }
}
