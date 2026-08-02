// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
    #[error("native search document ID already exists")]
    DuplicateDocumentId,
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
            .any(|entry| entry.header().name == header.name)
        {
            return Err(ModelError::DuplicateObjectName);
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
    pub(crate) entries: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
}

impl RelationState {
    pub(crate) fn create_table(&mut self, id: ObjectId) -> Result<(), ModelError> {
        if self.tables.insert(id, BTreeMap::new()).is_some() {
            return Err(ModelError::DuplicateObjectId);
        }
        Ok(())
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StructureState {
    pub(crate) entries: BTreeMap<Vec<u8>, StructureEntry>,
    pub(crate) hashes: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>,
    pub(crate) sets: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    pub(crate) lists: BTreeMap<Vec<u8>, VecDeque<Vec<u8>>>,
    pub(crate) sorted_sets: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, SortedSetScore>>,
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

impl StructureState {
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
        {
            return false;
        }
        self.hashes.insert(key, BTreeMap::new());
        true
    }

    pub(crate) fn hset(&mut self, key: &[u8], field: Vec<u8>, value: Vec<u8>) -> Option<bool> {
        self.hashes
            .get_mut(key)
            .map(|fields| fields.insert(field, value).is_none())
    }

    pub(crate) fn hget(&self, key: &[u8], field: &[u8]) -> Option<&[u8]> {
        self.hashes
            .get(key)
            .and_then(|fields| fields.get(field))
            .map(Vec::as_slice)
    }

    pub(crate) fn hdelete(&mut self, key: &[u8], field: &[u8]) -> Option<bool> {
        self.hashes
            .get_mut(key)
            .map(|fields| fields.remove(field).is_some())
    }

    pub(crate) fn hlen(&self, key: &[u8]) -> Option<usize> {
        self.hashes.get(key).map(BTreeMap::len)
    }

    pub(crate) fn create_set(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
        {
            return false;
        }
        self.sets.insert(key, BTreeSet::new());
        true
    }

    pub(crate) fn sadd(&mut self, key: &[u8], member: Vec<u8>) -> Option<bool> {
        self.sets.get_mut(key).map(|members| members.insert(member))
    }

    pub(crate) fn sismember(&self, key: &[u8], member: &[u8]) -> Option<bool> {
        self.sets.get(key).map(|members| members.contains(member))
    }

    pub(crate) fn srem(&mut self, key: &[u8], member: &[u8]) -> Option<bool> {
        self.sets.get_mut(key).map(|members| members.remove(member))
    }

    pub(crate) fn scard(&self, key: &[u8]) -> Option<usize> {
        self.sets.get(key).map(BTreeSet::len)
    }

    pub(crate) fn create_list(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
        {
            return false;
        }
        self.lists.insert(key, VecDeque::new());
        true
    }

    pub(crate) fn lpush(&mut self, key: &[u8], value: Vec<u8>) -> Option<usize> {
        self.lists.get_mut(key).map(|values| {
            values.push_front(value);
            values.len()
        })
    }

    pub(crate) fn rpush(&mut self, key: &[u8], value: Vec<u8>) -> Option<usize> {
        self.lists.get_mut(key).map(|values| {
            values.push_back(value);
            values.len()
        })
    }

    pub(crate) fn lpop(&mut self, key: &[u8]) -> ListPop {
        match self.lists.get_mut(key) {
            None => ListPop::Missing,
            Some(values) => values.pop_front().map_or(ListPop::Empty, ListPop::Value),
        }
    }

    pub(crate) fn rpop(&mut self, key: &[u8]) -> ListPop {
        match self.lists.get_mut(key) {
            None => ListPop::Missing,
            Some(values) => values.pop_back().map_or(ListPop::Empty, ListPop::Value),
        }
    }

    pub(crate) fn llen(&self, key: &[u8]) -> Option<usize> {
        self.lists.get(key).map(VecDeque::len)
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

    pub(crate) fn create_sorted_set(&mut self, key: Vec<u8>) -> bool {
        if self.entries.contains_key(&key)
            || self.hashes.contains_key(&key)
            || self.sets.contains_key(&key)
            || self.lists.contains_key(&key)
            || self.sorted_sets.contains_key(&key)
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

    pub(crate) fn zrange(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
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
        Some(
            ordered[start..=stop]
                .iter()
                .map(|(score, member)| ((*member).clone(), *score))
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

    pub(crate) fn encode(&self) -> Result<Vec<u8>, ModelError> {
        if !self.hashes.is_empty()
            || !self.sets.is_empty()
            || !self.lists.is_empty()
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
            sets: BTreeMap::new(),
            lists: BTreeMap::new(),
            sorted_sets: BTreeMap::new(),
        })
    }
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
        CATALOG_MAGIC_V1, CATALOG_MAGIC_V2, CatalogState, ModelError, RelationState, SearchState,
        StructureState, TtlValue, legacy_catalog_object, put_bytes, put_len,
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
