// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use hyphae_native_types::{EngineKind, ObjectId};
use thiserror::Error;

const CATALOG_MAGIC: [u8; 8] = *b"HYCAT001";
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
    #[error("native relational primary key already exists")]
    DuplicatePrimaryKey,
    #[error("native relational primary key does not exist")]
    MissingPrimaryKey,
    #[error("legacy native structure state cannot encode collection families")]
    UnsupportedLegacyStructureFamily,
    #[error("native search document ID already exists")]
    DuplicateDocumentId,
    #[error("native state payload contains a duplicate canonical entry")]
    DuplicateEncodedEntry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogEntry {
    pub(crate) owner: EngineKind,
    pub(crate) name: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CatalogState {
    pub(crate) objects: BTreeMap<ObjectId, CatalogEntry>,
}

impl CatalogState {
    pub(crate) fn create(
        &mut self,
        id: ObjectId,
        owner: EngineKind,
        name: String,
    ) -> Result<(), ModelError> {
        let lookup = normalize_name(&name);
        if self.objects.contains_key(&id) {
            return Err(ModelError::DuplicateObjectId);
        }
        if self
            .objects
            .values()
            .any(|entry| normalize_name(&entry.name) == lookup)
        {
            return Err(ModelError::DuplicateObjectName);
        }
        self.objects.insert(id, CatalogEntry { owner, name });
        Ok(())
    }

    pub(crate) fn require(&self, id: ObjectId, owner: EngineKind) -> Result<(), ModelError> {
        let entry = self.objects.get(&id).ok_or(ModelError::UnknownObject)?;
        if entry.owner != owner {
            return Err(ModelError::WrongEngine);
        }
        Ok(())
    }

    pub(crate) fn id_named(&self, name: &str, owner: EngineKind) -> Result<ObjectId, ModelError> {
        let lookup = normalize_name(name);
        self.objects
            .iter()
            .find(|(_, entry)| entry.owner == owner && normalize_name(&entry.name) == lookup)
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
        bytes.extend_from_slice(&CATALOG_MAGIC);
        put_len(&mut bytes, self.objects.len())?;
        for (id, entry) in &self.objects {
            bytes.extend_from_slice(&id.get().to_le_bytes());
            bytes.push(entry.owner as u8);
            put_bytes(&mut bytes, entry.name.as_bytes())?;
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut decoder = Decoder::new(bytes, CATALOG_MAGIC)?;
        let count = decoder.len()?;
        let mut objects = BTreeMap::new();
        for _ in 0..count {
            let id = decoder.object_id()?;
            let owner = decode_engine(decoder.byte()?)?;
            let name = decoder.string()?;
            if objects.insert(id, CatalogEntry { owner, name }).is_some() {
                return Err(ModelError::DuplicateEncodedEntry);
            }
        }
        decoder.finish()?;
        Ok(Self { objects })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RelationState {
    pub(crate) tables: BTreeMap<ObjectId, BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl RelationState {
    pub(crate) fn create_table(&mut self, id: ObjectId) -> Result<(), ModelError> {
        if self.tables.insert(id, BTreeMap::new()).is_some() {
            return Err(ModelError::DuplicateObjectId);
        }
        Ok(())
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
pub(crate) enum TtlValue {
    Persistent,
    Remaining(i64),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StructureState {
    pub(crate) entries: BTreeMap<Vec<u8>, StructureEntry>,
    pub(crate) hashes: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>,
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
        if self.entries.contains_key(&key) || self.hashes.contains_key(&key) {
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
        if !self.hashes.is_empty() {
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
        })
    }
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
                let idf = (1.0
                    + (document_count - document_frequency + 0.5) / (document_frequency + 0.5))
                    .ln();
                let normalization =
                    1.2 * (1.0 - 0.75 + 0.75 * (count_f64(tokens.len())? / average_length));
                score += idf * (term_frequency * 2.2) / (term_frequency + normalization);
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

fn analyze(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
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
    use hyphae_native_types::{EngineKind, ObjectId};

    use super::{CatalogState, ModelError, RelationState, SearchState, StructureState, TtlValue};

    #[test]
    fn every_engine_state_has_a_canonical_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let table = ObjectId::new(1)?;
        let index = ObjectId::new(2)?;

        let mut catalog = CatalogState::default();
        catalog.create(table, EngineKind::Relational, "Accounts".to_owned())?;
        catalog.create(index, EngineKind::Search, "Notes".to_owned())?;
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
