// SPDX-License-Identifier: Apache-2.0

use std::str;

use hyphae_native_types::{
    ColumnId, EngineKind, FieldId, LogicalType, ObjectId, ScalarValue, VectorElement, VectorType,
};

use super::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogError,
    CatalogName, CatalogObject, CatalogObjectKind, CatalogObjectV2, ColumnCheckConstraint,
    ColumnCheckOperator, ColumnDefinition, CompatibleCatalogObjectV2, CrossEngineLinkDefinition,
    CrossEngineLinkDeleteBehavior, CrossEngineLinkMaintenance, CrossEngineLinkMapping,
    DefinitionDigest, DefinitionVersion, FieldSourcePolicy, ForeignKeyDefinition,
    IncrementalVectorLifecycle, KeyspaceDefinition, KeyspaceEvictionPolicy, KeyspaceMemoryClass,
    KeyspaceTtlPolicy, LexicalIndexPolicy, LogicalCatalogObject, MAX_CATALOG_DEFINITION_BYTES,
    MAX_CATALOG_DEFINITION_ITEMS, MAX_CATALOG_NAME_BYTES, NamedVectorDefinition, ObjectHeader,
    ObjectHeaderV2, QualifiedName, RelationDefinition, SearchCollectionDefinition,
    SearchCollectionDefinitionV2, SearchFieldDefinition, SearchFieldDefinitionV2,
    SearchFieldOptions, SecondaryIndexDefinition, StructureDefinition, StructureKind,
    StructureOwnership, VectorMetric, VectorSearchPolicy,
};

const DEFINITION_MAGIC_V1: [u8; 8] = *b"HYCOBJ01";
const DEFINITION_MAGIC_V2: [u8; 8] = *b"HYCOBJ02";
const OBJECT_RELATION: u8 = 1;
const OBJECT_STRUCTURE: u8 = 2;
const OBJECT_SEARCH: u8 = 3;
const OBJECT_SECONDARY_INDEX: u8 = 4;
const OBJECT_CROSS_ENGINE_LINK: u8 = 5;
const REPRESENTATION_COMPATIBLE: u8 = 1;
const REPRESENTATION_V2: u8 = 2;

impl CatalogObject {
    /// Encodes one complete canonical catalog object definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the object violates catalog invariants or its
    /// canonical representation exceeds 16 MiB.
    pub fn encode_definition(&self) -> Result<Vec<u8>, CatalogError> {
        self.validate()?;
        let tag = match self {
            Self::Relation(_) => OBJECT_RELATION,
            Self::SecondaryIndex(_) => OBJECT_SECONDARY_INDEX,
            Self::Structure(_) => OBJECT_STRUCTURE,
            Self::Search(_) => OBJECT_SEARCH,
            Self::CrossEngineLink(_) => OBJECT_CROSS_ENGINE_LINK,
        };
        let mut encoder = Encoder::new(tag);
        encoder.put_header(self.header())?;
        match self {
            Self::Relation(definition) => encoder.put_relation(definition)?,
            Self::SecondaryIndex(definition) => encoder.put_secondary_index(definition)?,
            Self::Structure(definition) => encoder.put_structure(definition)?,
            Self::Search(definition) => encoder.put_search(definition)?,
            Self::CrossEngineLink(definition) => encoder.put_cross_engine_link(definition)?,
        }
        encoder.finish()
    }

    /// Decodes one complete canonical catalog object definition.
    ///
    /// # Errors
    ///
    /// Returns an error for corruption, invalid identities/types/names,
    /// noncanonical ordering, unsupported discriminants, excessive lengths,
    /// or trailing bytes.
    pub fn decode_definition(encoded: &[u8]) -> Result<Self, CatalogError> {
        let mut decoder = Decoder::new(encoded, DEFINITION_MAGIC_V1)?;
        let tag = decoder.byte()?;
        let header = decoder.header()?;
        let object = match tag {
            OBJECT_RELATION => Self::Relation(decoder.relation(header)?),
            OBJECT_SECONDARY_INDEX => Self::SecondaryIndex(decoder.secondary_index(header)?),
            OBJECT_STRUCTURE => Self::Structure(decoder.structure(header)?),
            OBJECT_SEARCH => Self::Search(decoder.search(header)?),
            OBJECT_CROSS_ENGINE_LINK => Self::CrossEngineLink(decoder.cross_engine_link(header)?),
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        decoder.finish()?;
        object.validate()?;
        if object.encode_definition()? != encoded {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(object)
    }

    /// Wraps this V1-compatible object in one canonical `HYCOBJ02` definition.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid object, self-parent, or excessive size.
    pub fn encode_definition_v2(
        &self,
        parent: ObjectId,
        definition_version: DefinitionVersion,
    ) -> Result<Vec<u8>, CatalogError> {
        LogicalCatalogObject::Compatible(CompatibleCatalogObjectV2 {
            object: self.clone(),
            parent,
            definition_version,
        })
        .encode_definition_v2()
    }

    /// Computes the stable digest of this object's canonical logical V2 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when V2 encoding fails.
    pub fn logical_definition_digest(
        &self,
        parent: ObjectId,
        definition_version: DefinitionVersion,
    ) -> Result<DefinitionDigest, CatalogError> {
        Ok(DefinitionDigest::from_bytes(sha256(
            &self.encode_definition_v2(parent, definition_version)?,
        )))
    }
}

impl LogicalCatalogObject {
    /// Encodes one complete canonical logical V2 definition using `HYCOBJ02`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid definition or excessive size.
    pub fn encode_definition_v2(&self) -> Result<Vec<u8>, CatalogError> {
        self.validate()?;
        let representation = match self {
            Self::Compatible(_) => REPRESENTATION_COMPATIBLE,
            Self::V2(_) => REPRESENTATION_V2,
        };
        let mut encoder = Encoder::new_v2(self.kind() as u8, representation);
        encoder.put_logical_header(self)?;
        match self {
            Self::Compatible(definition) => {
                encoder.put_bytes(&definition.object.encode_definition()?)?;
            }
            Self::V2(CatalogObjectV2::Database(_) | CatalogObjectV2::Schema(_)) => {}
            Self::V2(CatalogObjectV2::Keyspace(definition)) => {
                encoder.put_keyspace_v2(definition)?;
            }
            Self::V2(CatalogObjectV2::Analyzer(definition)) => {
                encoder.put_analyzer_v2(definition)?;
            }
            Self::V2(CatalogObjectV2::SearchCollection(definition)) => {
                encoder.put_search_v2(definition)?;
            }
        }
        encoder.finish()
    }

    /// Strictly decodes one canonical `HYCOBJ02` definition.
    ///
    /// # Errors
    ///
    /// Returns an error for truncation, unsupported tags, invalid values,
    /// noncanonical ordering or policy, mismatched wrapped V1 metadata, or
    /// trailing bytes.
    pub fn decode_definition_v2(encoded: &[u8]) -> Result<Self, CatalogError> {
        let mut decoder = Decoder::new(encoded, DEFINITION_MAGIC_V2)?;
        let kind = decoder.catalog_object_kind()?;
        let representation = decoder.byte()?;
        let header = decoder.header_v2()?;
        let object = match representation {
            REPRESENTATION_COMPATIBLE => {
                let compatible = CatalogObject::decode_definition(decoder.bytes()?)?;
                if compatible.kind() != kind
                    || compatible.header().id != header.id
                    || compatible.header().owner != header.owner
                    || compatible.header().name != header.name
                {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                }
                Self::Compatible(CompatibleCatalogObjectV2 {
                    object: compatible,
                    parent: header.parent.unwrap_or(header.id),
                    definition_version: header.definition_version,
                })
            }
            REPRESENTATION_V2 => Self::V2(match kind {
                CatalogObjectKind::Database => CatalogObjectV2::Database(header),
                CatalogObjectKind::Schema => CatalogObjectV2::Schema(header),
                CatalogObjectKind::Keyspace => {
                    CatalogObjectV2::Keyspace(decoder.keyspace_v2(header)?)
                }
                CatalogObjectKind::Analyzer => {
                    CatalogObjectV2::Analyzer(decoder.analyzer_v2(header)?)
                }
                CatalogObjectKind::SearchCollection => {
                    CatalogObjectV2::SearchCollection(decoder.search_v2(header)?)
                }
                CatalogObjectKind::Relation
                | CatalogObjectKind::SecondaryIndex
                | CatalogObjectKind::Structure
                | CatalogObjectKind::CrossEngineLink => {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                }
            }),
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        decoder.finish()?;
        object.validate()?;
        if object.encode_definition_v2()? != encoded {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(object)
    }

    /// Returns the stable SHA-256 digest of the canonical `HYCOBJ02` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when canonical encoding fails.
    pub fn definition_digest(&self) -> Result<DefinitionDigest, CatalogError> {
        Ok(DefinitionDigest::from_bytes(sha256(
            &self.encode_definition_v2()?,
        )))
    }
}

const SHA256_INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];
const SHA256_ROUND: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn sha256(input: &[u8]) -> [u8; 32] {
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((input.len() + 72) & !63);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = SHA256_INITIAL;
    for block in padded.chunks_exact(64) {
        sha256_compress(&mut state, block);
    }
    let mut digest = [0_u8; 32];
    for (encoded, word) in digest.chunks_exact_mut(4).zip(state) {
        encoded.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn sha256_compress(state: &mut [u32; 8], block: &[u8]) {
    let mut words = [0_u32; 64];
    for (word, encoded) in words[..16].iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
    }
    for index in 16..64 {
        let sigma0 = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
        let sigma1 = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(sigma0)
            .wrapping_add(words[index - 7])
            .wrapping_add(sigma1);
    }
    let [
        mut state0,
        mut state1,
        mut state2,
        mut state3,
        mut state4,
        mut state5,
        mut state6,
        mut state7,
    ] = *state;
    for index in 0..64 {
        let sum1 = state4.rotate_right(6) ^ state4.rotate_right(11) ^ state4.rotate_right(25);
        let choose = (state4 & state5) ^ (!state4 & state6);
        let temporary1 = state7
            .wrapping_add(sum1)
            .wrapping_add(choose)
            .wrapping_add(SHA256_ROUND[index])
            .wrapping_add(words[index]);
        let sum0 = state0.rotate_right(2) ^ state0.rotate_right(13) ^ state0.rotate_right(22);
        let majority = (state0 & state1) ^ (state0 & state2) ^ (state1 & state2);
        let temporary2 = sum0.wrapping_add(majority);
        state7 = state6;
        state6 = state5;
        state5 = state4;
        state4 = state3.wrapping_add(temporary1);
        state3 = state2;
        state2 = state1;
        state1 = state0;
        state0 = temporary1.wrapping_add(temporary2);
    }
    for (slot, value) in state.iter_mut().zip([
        state0, state1, state2, state3, state4, state5, state6, state7,
    ]) {
        *slot = slot.wrapping_add(value);
    }
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(tag: u8) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&DEFINITION_MAGIC_V1);
        bytes.push(tag);
        Self { bytes }
    }

    fn new_v2(kind: u8, representation: u8) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&DEFINITION_MAGIC_V2);
        bytes.push(kind);
        bytes.push(representation);
        Self { bytes }
    }

    fn put_logical_header(&mut self, object: &LogicalCatalogObject) -> Result<(), CatalogError> {
        self.put_fixed(&object.id().get().to_le_bytes())?;
        self.put_byte(object.owner() as u8)?;
        self.put_name(&object.name().database)?;
        self.put_name(&object.name().schema)?;
        self.put_name(&object.name().object)?;
        match object.parent() {
            Some(parent) => {
                self.put_byte(1)?;
                self.put_fixed(&parent.get().to_le_bytes())?;
            }
            None => self.put_byte(0)?,
        }
        self.put_fixed(&object.definition_version().get().to_le_bytes())
    }

    fn put_relation(&mut self, definition: &RelationDefinition) -> Result<(), CatalogError> {
        self.put_item_count(definition.columns.len())?;
        for column in &definition.columns {
            self.put_fixed(&column.id.get().to_le_bytes())?;
            self.put_name(&column.name)?;
            self.put_logical_type(&column.logical_type)?;
            self.put_bool(column.nullable)?;
        }
        self.put_item_count(definition.primary_key.len())?;
        for column in &definition.primary_key {
            self.put_fixed(&column.get().to_le_bytes())?;
        }
        if !definition.checks.is_empty() || !definition.foreign_keys.is_empty() {
            self.put_item_count(definition.checks.len())?;
            for (column, check) in &definition.checks {
                self.put_fixed(&column.get().to_le_bytes())?;
                self.put_byte(check.operator as u8)?;
                let logical_type = &definition
                    .columns
                    .iter()
                    .find(|definition| definition.id == *column)
                    .ok_or(CatalogError::InvalidDefinitionEncoding)?
                    .logical_type;
                self.put_bytes(&check.operand.encode_storage(logical_type)?)?;
            }
            self.put_item_count(definition.foreign_keys.len())?;
            for foreign_key in &definition.foreign_keys {
                match &foreign_key.name {
                    Some(name) => {
                        self.put_byte(1)?;
                        self.put_name(name)?;
                    }
                    None => self.put_byte(0)?,
                }
                self.put_item_count(foreign_key.columns.len())?;
                for column in &foreign_key.columns {
                    self.put_fixed(&column.get().to_le_bytes())?;
                }
                self.put_fixed(&foreign_key.referenced_relation.get().to_le_bytes())?;
                self.put_item_count(foreign_key.referenced_columns.len())?;
                for column in &foreign_key.referenced_columns {
                    self.put_fixed(&column.get().to_le_bytes())?;
                }
                match foreign_key.referenced_index {
                    Some(index) => {
                        self.put_byte(1)?;
                        self.put_fixed(&index.get().to_le_bytes())?;
                    }
                    None => self.put_byte(0)?,
                }
            }
        }
        Ok(())
    }

    fn put_secondary_index(
        &mut self,
        definition: &SecondaryIndexDefinition,
    ) -> Result<(), CatalogError> {
        self.put_fixed(&definition.relation.get().to_le_bytes())?;
        self.put_item_count(definition.columns.len())?;
        for column in &definition.columns {
            self.put_fixed(&column.get().to_le_bytes())?;
        }
        self.put_bool(definition.unique)?;
        self.put_bool(definition.nulls_distinct)
    }

    fn put_structure(&mut self, definition: &StructureDefinition) -> Result<(), CatalogError> {
        self.put_byte(definition.kind as u8)?;
        self.put_logical_type(&definition.key_type)?;
        self.put_logical_type(&definition.value_type)?;
        self.put_byte(definition.ownership as u8)?;
        self.put_bool(definition.ttl_enabled)
    }

    fn put_keyspace_v2(&mut self, definition: &KeyspaceDefinition) -> Result<(), CatalogError> {
        self.put_byte(definition.kind as u8)?;
        self.put_logical_type(&definition.key_type)?;
        self.put_logical_type(&definition.value_type)?;
        self.put_byte(definition.ownership as u8)?;
        self.put_byte(definition.ttl_policy as u8)?;
        match definition.default_ttl_millis {
            Some(default) => {
                self.put_byte(1)?;
                self.put_fixed(&default.to_le_bytes())?;
            }
            None => self.put_byte(0)?,
        }
        self.put_byte(definition.memory_class as u8)?;
        self.put_byte(definition.eviction as u8)?;
        self.put_optional_object_id(definition.relation_schema)
    }

    fn put_analyzer_v2(&mut self, definition: &AnalyzerDefinition) -> Result<(), CatalogError> {
        self.put_byte(definition.tokenizer as u8)?;
        self.put_item_count(definition.filters.len())?;
        for filter in &definition.filters {
            self.put_byte(*filter as u8)?;
        }
        Ok(())
    }

    fn put_search_v2(
        &mut self,
        definition: &SearchCollectionDefinitionV2,
    ) -> Result<(), CatalogError> {
        self.put_item_count(definition.fields.len())?;
        for field in &definition.fields {
            self.put_fixed(&field.id.get().to_le_bytes())?;
            self.put_name(&field.name)?;
            self.put_logical_type(&field.logical_type)?;
            self.put_optional_object_id(field.analyzer)?;
            self.put_bool(field.options.stored)?;
            self.put_bool(field.options.doc_values)?;
            self.put_byte(field.options.source as u8)?;
            self.put_byte(field.options.lexical as u8)?;
        }
        self.put_item_count(definition.vectors.len())?;
        for vector in &definition.vectors {
            self.put_fixed(&vector.id.get().to_le_bytes())?;
            self.put_name(&vector.name)?;
            self.put_byte(vector.vector_type.element() as u8)?;
            self.put_fixed(&vector.vector_type.dimension().to_le_bytes())?;
            self.put_byte(vector.metric as u8)?;
            match vector.policy {
                VectorSearchPolicy::Exact => self.put_byte(1)?,
                VectorSearchPolicy::Ann(ann) => {
                    self.put_byte(2)?;
                    self.put_ann(ann)?;
                }
                VectorSearchPolicy::Adaptive {
                    exact_candidate_threshold,
                    ann,
                } => {
                    self.put_byte(3)?;
                    self.put_fixed(&exact_candidate_threshold.to_le_bytes())?;
                    self.put_ann(ann)?;
                }
            }
            self.put_fixed(&vector.lifecycle.delta_max_entries.to_le_bytes())?;
            self.put_fixed(&vector.lifecycle.consolidate_after_deltas.to_le_bytes())?;
            self.put_fixed(&vector.lifecycle.retain_generations.to_le_bytes())?;
        }
        Ok(())
    }

    fn put_ann(&mut self, ann: AnnIndexDefinition) -> Result<(), CatalogError> {
        self.put_byte(ann.metric() as u8)?;
        self.put_fixed(&ann.m().to_le_bytes())?;
        self.put_fixed(&ann.ef_construction().to_le_bytes())?;
        self.put_fixed(&ann.ef_search_default().to_le_bytes())?;
        self.put_fixed(&ann.ef_search_max().to_le_bytes())?;
        self.put_fixed(&ann.seed().to_le_bytes())
    }

    fn put_optional_object_id(&mut self, id: Option<ObjectId>) -> Result<(), CatalogError> {
        match id {
            Some(id) => {
                self.put_byte(1)?;
                self.put_fixed(&id.get().to_le_bytes())
            }
            None => self.put_byte(0),
        }
    }

    fn put_search(&mut self, definition: &SearchCollectionDefinition) -> Result<(), CatalogError> {
        self.put_item_count(definition.fields.len())?;
        for field in &definition.fields {
            self.put_fixed(&field.id.get().to_le_bytes())?;
            self.put_name(&field.name)?;
            self.put_logical_type(&field.logical_type)?;
            match field.analyzer {
                Some(analyzer) => {
                    self.put_byte(1)?;
                    self.put_fixed(&analyzer.get().to_le_bytes())?;
                }
                None => self.put_byte(0)?,
            }
            self.put_bool(field.doc_values)?;
        }
        match (definition.vector, definition.ann) {
            (Some(vector), None) => {
                self.put_byte(1)?;
                self.put_byte(vector.element() as u8)?;
                self.put_fixed(&vector.dimension().to_le_bytes())?;
            }
            (Some(vector), Some(ann)) => {
                self.put_byte(2)?;
                self.put_byte(vector.element() as u8)?;
                self.put_fixed(&vector.dimension().to_le_bytes())?;
                self.put_byte(ann.metric() as u8)?;
                self.put_fixed(&ann.m().to_le_bytes())?;
                self.put_fixed(&ann.ef_construction().to_le_bytes())?;
                self.put_fixed(&ann.ef_search_default().to_le_bytes())?;
                self.put_fixed(&ann.ef_search_max().to_le_bytes())?;
                self.put_fixed(&ann.seed().to_le_bytes())?;
            }
            (None, None) => self.put_byte(0)?,
            (None, Some(_)) => return Err(CatalogError::AnnRequiresVector),
        }
        Ok(())
    }

    fn put_cross_engine_link(
        &mut self,
        definition: &CrossEngineLinkDefinition,
    ) -> Result<(), CatalogError> {
        self.put_fixed(&definition.source.get().to_le_bytes())?;
        self.put_fixed(&definition.target.get().to_le_bytes())?;
        self.put_item_count(definition.mapping.len())?;
        for mapping in &definition.mapping {
            self.put_fixed(&mapping.source.to_le_bytes())?;
            self.put_fixed(&mapping.target.to_le_bytes())?;
        }
        self.put_byte(definition.maintenance as u8)?;
        self.put_byte(definition.delete_behavior as u8)?;
        self.put_bool(definition.synchronous)
    }

    fn put_header(&mut self, header: &ObjectHeader) -> Result<(), CatalogError> {
        self.put_fixed(&header.id.get().to_le_bytes())?;
        self.put_byte(header.owner as u8)?;
        self.put_name(&header.name.database)?;
        self.put_name(&header.name.schema)?;
        self.put_name(&header.name.object)
    }

    fn put_name(&mut self, name: &CatalogName) -> Result<(), CatalogError> {
        self.put_bytes(name.display().as_bytes())?;
        self.put_bytes(name.lookup().as_bytes())
    }

    fn put_logical_type(&mut self, logical_type: &LogicalType) -> Result<(), CatalogError> {
        self.put_bytes(&logical_type.encode_descriptor()?)
    }

    fn put_bool(&mut self, value: bool) -> Result<(), CatalogError> {
        self.put_byte(u8::from(value))
    }

    fn put_byte(&mut self, value: u8) -> Result<(), CatalogError> {
        self.put_fixed(&[value])
    }

    fn put_item_count(&mut self, value: usize) -> Result<(), CatalogError> {
        if value > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        self.put_len(value)
    }

    fn put_bytes(&mut self, value: &[u8]) -> Result<(), CatalogError> {
        self.put_len(value.len())?;
        self.put_fixed(value)
    }

    fn put_len(&mut self, value: usize) -> Result<(), CatalogError> {
        let value = u32::try_from(value).map_err(|_| CatalogError::DefinitionTooLarge)?;
        self.put_fixed(&value.to_le_bytes())
    }

    fn put_fixed(&mut self, value: &[u8]) -> Result<(), CatalogError> {
        let new_length = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(CatalogError::DefinitionTooLarge)?;
        if new_length > MAX_CATALOG_DEFINITION_BYTES {
            return Err(CatalogError::DefinitionTooLarge);
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, CatalogError> {
        if self.bytes.len() > MAX_CATALOG_DEFINITION_BYTES {
            return Err(CatalogError::DefinitionTooLarge);
        }
        Ok(self.bytes)
    }
}

struct Decoder<'encoded> {
    encoded: &'encoded [u8],
    offset: usize,
}

impl<'encoded> Decoder<'encoded> {
    fn new(encoded: &'encoded [u8], magic: [u8; 8]) -> Result<Self, CatalogError> {
        if encoded.len() > MAX_CATALOG_DEFINITION_BYTES {
            return Err(CatalogError::DefinitionTooLarge);
        }
        if !encoded.starts_with(&magic) {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(Self {
            encoded,
            offset: magic.len(),
        })
    }

    fn relation(&mut self, header: ObjectHeader) -> Result<RelationDefinition, CatalogError> {
        let column_count = self.item_count()?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            columns.push(ColumnDefinition {
                id: self.column_id()?,
                name: self.name()?,
                logical_type: self.logical_type()?,
                nullable: self.boolean()?,
            });
        }
        let primary_key_count = self.item_count()?;
        let mut primary_key = Vec::with_capacity(primary_key_count);
        for _ in 0..primary_key_count {
            primary_key.push(self.column_id()?);
        }
        let mut checks = Vec::new();
        if self.offset < self.encoded.len() {
            let check_count = self.item_count()?;
            checks.reserve(check_count);
            for _ in 0..check_count {
                let column = self.column_id()?;
                let operator = match self.byte()? {
                    1 => ColumnCheckOperator::Equal,
                    2 => ColumnCheckOperator::NotEqual,
                    3 => ColumnCheckOperator::Less,
                    4 => ColumnCheckOperator::LessOrEqual,
                    5 => ColumnCheckOperator::Greater,
                    6 => ColumnCheckOperator::GreaterOrEqual,
                    _ => return Err(CatalogError::InvalidDefinitionEncoding),
                };
                let logical_type = &columns
                    .iter()
                    .find(|definition| definition.id == column)
                    .ok_or(CatalogError::InvalidDefinitionEncoding)?
                    .logical_type;
                let operand = ScalarValue::decode_storage(logical_type, self.bytes()?)?;
                checks.push((column, ColumnCheckConstraint { operator, operand }));
            }
        }
        let mut foreign_keys = Vec::new();
        if self.offset < self.encoded.len() {
            let foreign_key_count = self.item_count()?;
            foreign_keys.reserve(foreign_key_count);
            for _ in 0..foreign_key_count {
                let name = match self.byte()? {
                    0 => None,
                    1 => Some(self.name()?),
                    _ => return Err(CatalogError::InvalidDefinitionEncoding),
                };
                let child_count = self.item_count()?;
                let mut child_columns = Vec::with_capacity(child_count);
                for _ in 0..child_count {
                    child_columns.push(self.column_id()?);
                }
                let referenced_relation = self.object_id()?;
                let parent_count = self.item_count()?;
                let mut referenced_columns = Vec::with_capacity(parent_count);
                for _ in 0..parent_count {
                    referenced_columns.push(self.column_id()?);
                }
                let referenced_index = match self.byte()? {
                    0 => None,
                    1 => Some(self.object_id()?),
                    _ => return Err(CatalogError::InvalidDefinitionEncoding),
                };
                foreign_keys.push(ForeignKeyDefinition {
                    name,
                    columns: child_columns,
                    referenced_relation,
                    referenced_columns,
                    referenced_index,
                });
            }
        }
        Ok(RelationDefinition {
            header,
            columns,
            primary_key,
            checks,
            foreign_keys,
        })
    }

    fn secondary_index(
        &mut self,
        header: ObjectHeader,
    ) -> Result<SecondaryIndexDefinition, CatalogError> {
        let relation = self.object_id()?;
        let column_count = self.item_count()?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            columns.push(self.column_id()?);
        }
        Ok(SecondaryIndexDefinition {
            header,
            relation,
            columns,
            unique: self.boolean()?,
            nulls_distinct: self.boolean()?,
        })
    }

    fn structure(&mut self, header: ObjectHeader) -> Result<StructureDefinition, CatalogError> {
        let kind = match self.byte()? {
            1 => StructureKind::String,
            2 => StructureKind::Counter,
            3 => StructureKind::Hash,
            4 => StructureKind::List,
            5 => StructureKind::Set,
            6 => StructureKind::SortedSet,
            7 => StructureKind::Stream,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let key_type = self.logical_type()?;
        let value_type = self.logical_type()?;
        let ownership = match self.byte()? {
            1 => StructureOwnership::Canonical,
            2 => StructureOwnership::Cache,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let ttl_enabled = self.boolean()?;
        Ok(StructureDefinition {
            header,
            kind,
            key_type,
            value_type,
            ownership,
            ttl_enabled,
        })
    }

    fn keyspace_v2(&mut self, header: ObjectHeaderV2) -> Result<KeyspaceDefinition, CatalogError> {
        let kind = self.structure_kind()?;
        let key_type = self.logical_type()?;
        let value_type = self.logical_type()?;
        let ownership = self.structure_ownership()?;
        let ttl_policy = match self.byte()? {
            1 => KeyspaceTtlPolicy::Disabled,
            2 => KeyspaceTtlPolicy::PerValue,
            3 => KeyspaceTtlPolicy::Default,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let default_ttl_millis = match self.byte()? {
            0 => None,
            1 => Some(u64::from_le_bytes(self.fixed()?)),
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let memory_class = match self.byte()? {
            1 => KeyspaceMemoryClass::Durable,
            2 => KeyspaceMemoryClass::Standard,
            3 => KeyspaceMemoryClass::Cache,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let eviction = match self.byte()? {
            1 => KeyspaceEvictionPolicy::None,
            2 => KeyspaceEvictionPolicy::LeastRecentlyUsed,
            3 => KeyspaceEvictionPolicy::NearestExpiry,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let relation_schema = self.optional_object_id()?;
        Ok(KeyspaceDefinition {
            header,
            kind,
            key_type,
            value_type,
            ownership,
            ttl_policy,
            default_ttl_millis,
            memory_class,
            eviction,
            relation_schema,
        })
    }

    fn analyzer_v2(&mut self, header: ObjectHeaderV2) -> Result<AnalyzerDefinition, CatalogError> {
        let tokenizer = match self.byte()? {
            1 => AnalyzerTokenizer::UnicodeWord,
            2 => AnalyzerTokenizer::Whitespace,
            3 => AnalyzerTokenizer::Keyword,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let count = self.item_count()?;
        let mut filters = Vec::with_capacity(count);
        for _ in 0..count {
            filters.push(match self.byte()? {
                1 => AnalyzerFilter::Lowercase,
                2 => AnalyzerFilter::AsciiFolding,
                3 => AnalyzerFilter::EnglishStopV1,
                4 => AnalyzerFilter::EnglishStemV1,
                _ => return Err(CatalogError::InvalidDefinitionEncoding),
            });
        }
        Ok(AnalyzerDefinition {
            header,
            tokenizer,
            filters,
        })
    }

    fn search_v2(
        &mut self,
        header: ObjectHeaderV2,
    ) -> Result<SearchCollectionDefinitionV2, CatalogError> {
        let field_count = self.item_count()?;
        let mut fields = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            fields.push(SearchFieldDefinitionV2 {
                id: self.field_id()?,
                name: self.name()?,
                logical_type: self.logical_type()?,
                analyzer: self.optional_object_id()?,
                options: SearchFieldOptions {
                    stored: self.boolean()?,
                    doc_values: self.boolean()?,
                    source: match self.byte()? {
                        1 => FieldSourcePolicy::Excluded,
                        2 => FieldSourcePolicy::Retained,
                        _ => return Err(CatalogError::InvalidDefinitionEncoding),
                    },
                    lexical: match self.byte()? {
                        1 => LexicalIndexPolicy::None,
                        2 => LexicalIndexPolicy::Frequencies,
                        3 => LexicalIndexPolicy::Positions,
                        _ => return Err(CatalogError::InvalidDefinitionEncoding),
                    },
                },
            });
        }
        let vector_count = self.item_count()?;
        let mut vectors = Vec::with_capacity(vector_count);
        for _ in 0..vector_count {
            let id = self.field_id()?;
            let name = self.name()?;
            if self.byte()? != VectorElement::Float32 as u8 {
                return Err(CatalogError::InvalidDefinitionEncoding);
            }
            let vector_type =
                VectorType::new(VectorElement::Float32, u16::from_le_bytes(self.fixed()?))
                    .map_err(|_| CatalogError::InvalidDefinitionEncoding)?;
            let metric = self.vector_metric()?;
            let policy = match self.byte()? {
                1 => VectorSearchPolicy::Exact,
                2 => VectorSearchPolicy::Ann(self.ann()?),
                3 => VectorSearchPolicy::Adaptive {
                    exact_candidate_threshold: u32::from_le_bytes(self.fixed()?),
                    ann: self.ann()?,
                },
                _ => return Err(CatalogError::InvalidDefinitionEncoding),
            };
            vectors.push(NamedVectorDefinition {
                id,
                name,
                vector_type,
                metric,
                policy,
                lifecycle: IncrementalVectorLifecycle {
                    delta_max_entries: u32::from_le_bytes(self.fixed()?),
                    consolidate_after_deltas: u16::from_le_bytes(self.fixed()?),
                    retain_generations: u16::from_le_bytes(self.fixed()?),
                },
            });
        }
        Ok(SearchCollectionDefinitionV2 {
            header,
            fields,
            vectors,
        })
    }

    fn search(&mut self, header: ObjectHeader) -> Result<SearchCollectionDefinition, CatalogError> {
        let field_count = self.item_count()?;
        let mut fields = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            let id = self.field_id()?;
            let name = self.name()?;
            let logical_type = self.logical_type()?;
            let analyzer = match self.byte()? {
                0 => None,
                1 => Some(self.object_id()?),
                _ => return Err(CatalogError::InvalidDefinitionEncoding),
            };
            let doc_values = self.boolean()?;
            fields.push(SearchFieldDefinition {
                id,
                name,
                logical_type,
                analyzer,
                doc_values,
            });
        }
        let (vector, ann) = match self.byte()? {
            0 => (None, None),
            1 => {
                if self.byte()? != VectorElement::Float32 as u8 {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                }
                let dimension = u16::from_le_bytes(self.fixed()?);
                (
                    Some(
                        VectorType::new(VectorElement::Float32, dimension)
                            .map_err(|_| CatalogError::InvalidDefinitionEncoding)?,
                    ),
                    None,
                )
            }
            2 => {
                if self.byte()? != VectorElement::Float32 as u8 {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                }
                let dimension = u16::from_le_bytes(self.fixed()?);
                let vector = VectorType::new(VectorElement::Float32, dimension)
                    .map_err(|_| CatalogError::InvalidDefinitionEncoding)?;
                let metric = match self.byte()? {
                    1 => VectorMetric::Cosine,
                    2 => VectorMetric::NegativeDot,
                    3 => VectorMetric::SquaredL2,
                    _ => return Err(CatalogError::InvalidDefinitionEncoding),
                };
                (
                    Some(vector),
                    Some(AnnIndexDefinition::new(
                        metric,
                        u16::from_le_bytes(self.fixed()?),
                        u16::from_le_bytes(self.fixed()?),
                        u16::from_le_bytes(self.fixed()?),
                        u16::from_le_bytes(self.fixed()?),
                        u64::from_le_bytes(self.fixed()?),
                    )?),
                )
            }
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        Ok(SearchCollectionDefinition {
            header,
            fields,
            vector,
            ann,
        })
    }

    fn cross_engine_link(
        &mut self,
        header: ObjectHeader,
    ) -> Result<CrossEngineLinkDefinition, CatalogError> {
        let source = self.object_id()?;
        let target = self.object_id()?;
        let count = self.item_count()?;
        let mut mapping = Vec::with_capacity(count);
        for _ in 0..count {
            mapping.push(CrossEngineLinkMapping {
                source: u32::from_le_bytes(self.fixed()?),
                target: u32::from_le_bytes(self.fixed()?),
            });
        }
        let maintenance = match self.byte()? {
            1 => CrossEngineLinkMaintenance::Manual,
            2 => CrossEngineLinkMaintenance::Derived,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        let delete_behavior = match self.byte()? {
            1 => CrossEngineLinkDeleteBehavior::Restrict,
            2 => CrossEngineLinkDeleteBehavior::Cascade,
            3 => CrossEngineLinkDeleteBehavior::Retain,
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        Ok(CrossEngineLinkDefinition {
            header,
            source,
            target,
            mapping,
            maintenance,
            delete_behavior,
            synchronous: self.boolean()?,
        })
    }

    fn header(&mut self) -> Result<ObjectHeader, CatalogError> {
        Ok(ObjectHeader {
            id: self.object_id()?,
            owner: self.engine()?,
            name: QualifiedName::new(self.name()?, self.name()?, self.name()?),
        })
    }

    fn header_v2(&mut self) -> Result<ObjectHeaderV2, CatalogError> {
        let id = self.object_id()?;
        let owner = self.engine()?;
        let name = QualifiedName::new(self.name()?, self.name()?, self.name()?);
        let parent = self.optional_object_id()?;
        let definition_version = DefinitionVersion::new(u64::from_le_bytes(self.fixed()?))?;
        Ok(ObjectHeaderV2 {
            id,
            owner,
            name,
            parent,
            definition_version,
        })
    }

    fn catalog_object_kind(&mut self) -> Result<CatalogObjectKind, CatalogError> {
        match self.byte()? {
            1 => Ok(CatalogObjectKind::Database),
            2 => Ok(CatalogObjectKind::Schema),
            3 => Ok(CatalogObjectKind::Relation),
            4 => Ok(CatalogObjectKind::SecondaryIndex),
            5 => Ok(CatalogObjectKind::Keyspace),
            6 => Ok(CatalogObjectKind::Structure),
            7 => Ok(CatalogObjectKind::SearchCollection),
            8 => Ok(CatalogObjectKind::Analyzer),
            9 => Ok(CatalogObjectKind::CrossEngineLink),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn name(&mut self) -> Result<CatalogName, CatalogError> {
        let display = self.name_string()?;
        let lookup = self.name_string()?;
        CatalogName::from_encoded_parts(display, lookup)
    }

    fn logical_type(&mut self) -> Result<LogicalType, CatalogError> {
        Ok(LogicalType::decode_descriptor(self.bytes()?)?)
    }

    fn object_id(&mut self) -> Result<ObjectId, CatalogError> {
        ObjectId::new(u128::from_le_bytes(self.fixed()?))
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn optional_object_id(&mut self) -> Result<Option<ObjectId>, CatalogError> {
        match self.byte()? {
            0 => Ok(None),
            1 => self.object_id().map(Some),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn column_id(&mut self) -> Result<ColumnId, CatalogError> {
        ColumnId::new(u32::from_le_bytes(self.fixed()?))
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn field_id(&mut self) -> Result<FieldId, CatalogError> {
        FieldId::new(u32::from_le_bytes(self.fixed()?))
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn engine(&mut self) -> Result<EngineKind, CatalogError> {
        match self.byte()? {
            0 => Ok(EngineKind::Kernel),
            1 => Ok(EngineKind::Relational),
            2 => Ok(EngineKind::Structure),
            3 => Ok(EngineKind::Search),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn structure_kind(&mut self) -> Result<StructureKind, CatalogError> {
        match self.byte()? {
            1 => Ok(StructureKind::String),
            2 => Ok(StructureKind::Counter),
            3 => Ok(StructureKind::Hash),
            4 => Ok(StructureKind::List),
            5 => Ok(StructureKind::Set),
            6 => Ok(StructureKind::SortedSet),
            7 => Ok(StructureKind::Stream),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn structure_ownership(&mut self) -> Result<StructureOwnership, CatalogError> {
        match self.byte()? {
            1 => Ok(StructureOwnership::Canonical),
            2 => Ok(StructureOwnership::Cache),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn vector_metric(&mut self) -> Result<VectorMetric, CatalogError> {
        match self.byte()? {
            1 => Ok(VectorMetric::Cosine),
            2 => Ok(VectorMetric::NegativeDot),
            3 => Ok(VectorMetric::SquaredL2),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn ann(&mut self) -> Result<AnnIndexDefinition, CatalogError> {
        AnnIndexDefinition::new(
            self.vector_metric()?,
            u16::from_le_bytes(self.fixed()?),
            u16::from_le_bytes(self.fixed()?),
            u16::from_le_bytes(self.fixed()?),
            u16::from_le_bytes(self.fixed()?),
            u64::from_le_bytes(self.fixed()?),
        )
    }

    fn boolean(&mut self) -> Result<bool, CatalogError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CatalogError::InvalidDefinitionEncoding),
        }
    }

    fn item_count(&mut self) -> Result<usize, CatalogError> {
        let count = self.len()?;
        if count > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        Ok(count)
    }

    fn name_string(&mut self) -> Result<String, CatalogError> {
        let encoded = self.bytes()?;
        if encoded.len() > MAX_CATALOG_NAME_BYTES {
            return Err(CatalogError::NameTooLong);
        }
        str::from_utf8(encoded)
            .map(str::to_owned)
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn bytes(&mut self) -> Result<&'encoded [u8], CatalogError> {
        let length = self.len()?;
        self.take(length)
    }

    fn len(&mut self) -> Result<usize, CatalogError> {
        usize::try_from(u32::from_le_bytes(self.fixed()?))
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn byte(&mut self) -> Result<u8, CatalogError> {
        Ok(self.fixed::<1>()?[0])
    }

    fn fixed<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], CatalogError> {
        self.take(LENGTH)?
            .try_into()
            .map_err(|_| CatalogError::InvalidDefinitionEncoding)
    }

    fn take(&mut self, length: usize) -> Result<&'encoded [u8], CatalogError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(CatalogError::InvalidDefinitionEncoding)?;
        let value = self
            .encoded
            .get(self.offset..end)
            .ok_or(CatalogError::InvalidDefinitionEncoding)?;
        self.offset = end;
        Ok(value)
    }

    fn finish(self) -> Result<(), CatalogError> {
        if self.offset == self.encoded.len() {
            Ok(())
        } else {
            Err(CatalogError::InvalidDefinitionEncoding)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use hyphae_native_types::{
        ColumnId, EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId, VectorElement,
        VectorType,
    };

    use super::{
        AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogError,
        CatalogName, CatalogObject, CatalogObjectV2, ColumnDefinition, CompatibleCatalogObjectV2,
        CrossEngineLinkDefinition, CrossEngineLinkDeleteBehavior, CrossEngineLinkMaintenance,
        CrossEngineLinkMapping, DefinitionDigest, DefinitionVersion, FieldSourcePolicy,
        IncrementalVectorLifecycle, KeyspaceDefinition, KeyspaceEvictionPolicy,
        KeyspaceMemoryClass, KeyspaceTtlPolicy, LexicalIndexPolicy, LogicalCatalogObject,
        MAX_CATALOG_DEFINITION_BYTES, NamedVectorDefinition, ObjectHeader, ObjectHeaderV2,
        QualifiedName, RelationDefinition, SearchCollectionDefinition,
        SearchCollectionDefinitionV2, SearchFieldDefinition, SearchFieldDefinitionV2,
        SearchFieldOptions, SecondaryIndexDefinition, StructureDefinition, StructureKind,
        StructureOwnership, VectorMetric, VectorSearchPolicy,
    };
    use crate::MAX_CATALOG_NAME_BYTES;

    const RELATION_GOLDEN_HEX: &str = concat!(
        "4859434f424a3031010100000000000000000000000000000001040000006d61696e040000006d",
        "61696e060000007075626c6963060000007075626c6963080000006163636f756e74730800000061",
        "63636f756e7473020000000100000002000000696402000000696402000000034000020000000c00",
        "0000646973706c61795f6e616d650c000000646973706c61795f6e616d650100000007010100000001",
        "000000"
    );
    const SECONDARY_INDEX_GOLDEN_HEX: &str = concat!(
        "4859434f424a3031040400000000000000000000000000000001040000006d61696e040000006d",
        "61696e060000007075626c6963060000007075626c6963180000006163636f756e74735f62795f",
        "646973706c61795f6e616d65180000006163636f756e74735f62795f646973706c61795f6e616d",
        "650100000000000000000000000000000001000000020000000101"
    );
    const DATABASE_V2_GOLDEN_HEX: &str = concat!(
        "4859434f424a303201020a00000000000000000000000000000000040000006d61696e04000000",
        "6d61696e060000007075626c6963060000007075626c6963080000006461746162617365080000",
        "006461746162617365000100000000000000"
    );
    const LOGICAL_RELATION_V2_GOLDEN_HEX: &str = concat!(
        "4859434f424a303203010100000000000000000000000000000001040000006d61696e04000000",
        "6d61696e060000007075626c6963060000007075626c6963080000006163636f756e7473080000",
        "006163636f756e7473010b0000000000000000000000000000000100000000000000a300000048",
        "59434f424a3031010100000000000000000000000000000001040000006d61696e040000006d61",
        "696e060000007075626c6963060000007075626c6963080000006163636f756e74730800000061",
        "63636f756e7473020000000100000002000000696402000000696402000000034000020000000c",
        "000000646973706c61795f6e616d650c000000646973706c61795f6e616d650100000007010100",
        "000001000000"
    );

    fn hex(encoded: &[u8]) -> Result<String, std::fmt::Error> {
        let mut output = String::with_capacity(encoded.len() * 2);
        for byte in encoded {
            write!(&mut output, "{byte:02x}")?;
        }
        Ok(output)
    }

    fn header(
        id: u128,
        owner: EngineKind,
        name: &str,
    ) -> Result<ObjectHeader, Box<dyn std::error::Error>> {
        Ok(ObjectHeader {
            id: ObjectId::new(id)?,
            owner,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted(name)?,
            ),
        })
    }

    fn header_v2(
        id: u128,
        owner: EngineKind,
        name: &str,
        parent: Option<u128>,
    ) -> Result<ObjectHeaderV2, Box<dyn std::error::Error>> {
        Ok(ObjectHeaderV2 {
            id: ObjectId::new(id)?,
            owner,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted(name)?,
            ),
            parent: parent.map(ObjectId::new).transpose()?,
            definition_version: DefinitionVersion::FIRST,
        })
    }

    fn relation() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::Relation(RelationDefinition {
            header: header(1, EngineKind::Relational, "accounts")?,
            columns: vec![
                ColumnDefinition {
                    id: ColumnId::new(1)?,
                    name: CatalogName::unquoted("id")?,
                    logical_type: LogicalType::Unsigned(IntegerWidth::Bits64),
                    nullable: false,
                },
                ColumnDefinition {
                    id: ColumnId::new(2)?,
                    name: CatalogName::unquoted("display_name")?,
                    logical_type: LogicalType::Text,
                    nullable: true,
                },
            ],
            primary_key: vec![ColumnId::new(1)?],
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }))
    }

    fn structure() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::Structure(StructureDefinition {
            header: header(2, EngineKind::Structure, "sessions")?,
            kind: StructureKind::Hash,
            key_type: LogicalType::Text,
            value_type: LogicalType::Map(
                Box::new(LogicalType::Text),
                Box::new(LogicalType::Binary),
            ),
            ownership: StructureOwnership::Canonical,
            ttl_enabled: true,
        }))
    }

    fn secondary_index() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::SecondaryIndex(SecondaryIndexDefinition {
            header: header(4, EngineKind::Relational, "accounts_by_display_name")?,
            relation: ObjectId::new(1)?,
            columns: vec![ColumnId::new(2)?],
            unique: true,
            nulls_distinct: true,
        }))
    }

    fn search() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::Search(SearchCollectionDefinition {
            header: header(3, EngineKind::Search, "documents")?,
            fields: vec![
                SearchFieldDefinition {
                    id: FieldId::new(1)?,
                    name: CatalogName::unquoted("body")?,
                    logical_type: LogicalType::Text,
                    analyzer: Some(ObjectId::new(99)?),
                    doc_values: false,
                },
                SearchFieldDefinition {
                    id: FieldId::new(2)?,
                    name: CatalogName::unquoted("published_at")?,
                    logical_type: LogicalType::Timestamp,
                    analyzer: None,
                    doc_values: true,
                },
            ],
            vector: Some(VectorType::new(VectorElement::Float32, 384)?),
            ann: Some(AnnIndexDefinition::new(
                VectorMetric::Cosine,
                16,
                128,
                64,
                256,
                7,
            )?),
        }))
    }

    fn legacy_exact_vector_search() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::Search(SearchCollectionDefinition {
            header: header(5, EngineKind::Search, "legacy_vectors")?,
            fields: Vec::new(),
            vector: Some(VectorType::new(VectorElement::Float32, 3)?),
            ann: None,
        }))
    }

    fn cross_engine_link() -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::CrossEngineLink(CrossEngineLinkDefinition {
            header: header(6, EngineKind::Kernel, "accounts_to_documents")?,
            source: ObjectId::new(1)?,
            target: ObjectId::new(3)?,
            mapping: vec![
                CrossEngineLinkMapping {
                    source: 1,
                    target: 1,
                },
                CrossEngineLinkMapping {
                    source: 2,
                    target: 2,
                },
            ],
            maintenance: CrossEngineLinkMaintenance::Derived,
            delete_behavior: CrossEngineLinkDeleteBehavior::Cascade,
            synchronous: true,
        }))
    }

    fn database_v2() -> Result<LogicalCatalogObject, Box<dyn std::error::Error>> {
        Ok(LogicalCatalogObject::V2(CatalogObjectV2::Database(
            header_v2(10, EngineKind::Kernel, "database", None)?,
        )))
    }

    fn analyzer_v2() -> Result<LogicalCatalogObject, Box<dyn std::error::Error>> {
        Ok(LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(
            AnalyzerDefinition {
                header: header_v2(12, EngineKind::Search, "english", Some(11))?,
                tokenizer: AnalyzerTokenizer::UnicodeWord,
                filters: vec![
                    AnalyzerFilter::Lowercase,
                    AnalyzerFilter::EnglishStopV1,
                    AnalyzerFilter::EnglishStemV1,
                ],
            },
        )))
    }

    fn keyspace_v2() -> Result<LogicalCatalogObject, Box<dyn std::error::Error>> {
        Ok(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(
            KeyspaceDefinition {
                header: header_v2(13, EngineKind::Structure, "sessions", Some(11))?,
                kind: StructureKind::Hash,
                key_type: LogicalType::Text,
                value_type: LogicalType::Binary,
                ownership: StructureOwnership::Cache,
                ttl_policy: KeyspaceTtlPolicy::Default,
                default_ttl_millis: Some(3_600_000),
                memory_class: KeyspaceMemoryClass::Cache,
                eviction: KeyspaceEvictionPolicy::LeastRecentlyUsed,
                relation_schema: None,
            },
        )))
    }

    fn search_v2() -> Result<LogicalCatalogObject, Box<dyn std::error::Error>> {
        let ann = AnnIndexDefinition::new(VectorMetric::Cosine, 16, 128, 64, 256, 7)?;
        Ok(LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                header: header_v2(14, EngineKind::Search, "articles", Some(11))?,
                fields: vec![
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(1)?,
                        name: CatalogName::unquoted("body")?,
                        logical_type: LogicalType::Text,
                        analyzer: Some(ObjectId::new(12)?),
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: false,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::Positions,
                        },
                    },
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(2)?,
                        name: CatalogName::unquoted("published_at")?,
                        logical_type: LogicalType::Timestamp,
                        analyzer: None,
                        options: SearchFieldOptions {
                            stored: false,
                            doc_values: true,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::None,
                        },
                    },
                ],
                vectors: vec![
                    NamedVectorDefinition {
                        id: FieldId::new(3)?,
                        name: CatalogName::unquoted("title_vector")?,
                        vector_type: VectorType::new(VectorElement::Float32, 384)?,
                        metric: VectorMetric::Cosine,
                        policy: VectorSearchPolicy::Exact,
                        lifecycle: IncrementalVectorLifecycle {
                            delta_max_entries: 1_024,
                            consolidate_after_deltas: 4,
                            retain_generations: 2,
                        },
                    },
                    NamedVectorDefinition {
                        id: FieldId::new(4)?,
                        name: CatalogName::unquoted("body_vector")?,
                        vector_type: VectorType::new(VectorElement::Float32, 768)?,
                        metric: VectorMetric::Cosine,
                        policy: VectorSearchPolicy::Adaptive {
                            exact_candidate_threshold: 256,
                            ann,
                        },
                        lifecycle: IncrementalVectorLifecycle {
                            delta_max_entries: 4_096,
                            consolidate_after_deltas: 8,
                            retain_generations: 3,
                        },
                    },
                ],
            },
        )))
    }

    #[test]
    fn every_object_definition_has_one_canonical_round_trip()
    -> Result<(), Box<dyn std::error::Error>> {
        for object in [
            relation()?,
            structure()?,
            search()?,
            secondary_index()?,
            cross_engine_link()?,
        ] {
            let encoded = object.encode_definition()?;
            if object.header().id.get() == 1 {
                assert_eq!(hex(&encoded)?, RELATION_GOLDEN_HEX);
            } else if object.header().id.get() == 4 {
                assert_eq!(hex(&encoded)?, SECONDARY_INDEX_GOLDEN_HEX);
            }
            assert_eq!(CatalogObject::decode_definition(&encoded)?, object);
            assert_eq!(
                CatalogObject::decode_definition(&encoded)?.encode_definition()?,
                encoded
            );
        }
        Ok(())
    }

    #[test]
    fn legacy_exact_vector_definition_keeps_its_canonical_v1_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let object = legacy_exact_vector_search()?;
        let encoded = object.encode_definition()?;
        let decoded = CatalogObject::decode_definition(&encoded)?;
        assert_eq!(decoded, object);
        assert_eq!(decoded.encode_definition()?, encoded);
        Ok(())
    }

    #[test]
    fn v2_namespace_and_compatible_definition_have_golden_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database_v2()?;
        let database_encoded = database.encode_definition_v2()?;
        assert_eq!(hex(&database_encoded)?, DATABASE_V2_GOLDEN_HEX);
        assert_eq!(
            LogicalCatalogObject::decode_definition_v2(&database_encoded)?,
            database
        );

        let relation = relation()?;
        let compatible = LogicalCatalogObject::Compatible(CompatibleCatalogObjectV2 {
            object: relation.clone(),
            parent: ObjectId::new(11)?,
            definition_version: DefinitionVersion::FIRST,
        });
        let compatible_encoded = compatible.encode_definition_v2()?;
        assert_eq!(hex(&compatible_encoded)?, LOGICAL_RELATION_V2_GOLDEN_HEX);
        assert_eq!(
            LogicalCatalogObject::decode_definition_v2(&compatible_encoded)?,
            compatible
        );
        assert_eq!(
            relation.encode_definition()?,
            CatalogObject::decode_definition(&relation.encode_definition()?)?
                .encode_definition()?
        );
        Ok(())
    }

    #[test]
    fn every_v2_definition_has_one_canonical_round_trip_and_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        for object in [database_v2()?, analyzer_v2()?, keyspace_v2()?, search_v2()?] {
            let encoded = object.encode_definition_v2()?;
            let decoded = LogicalCatalogObject::decode_definition_v2(&encoded)?;
            assert_eq!(decoded, object);
            assert_eq!(decoded.encode_definition_v2()?, encoded);
            assert_eq!(decoded.definition_digest()?, object.definition_digest()?);
            assert_ne!(
                object.definition_digest()?,
                DefinitionDigest::from_bytes([0; 32])
            );
        }
        Ok(())
    }

    #[test]
    fn logical_digest_uses_canonical_sha256() -> Result<(), std::fmt::Error> {
        assert_eq!(
            hex(&super::sha256(b"abc"))?,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        Ok(())
    }

    #[test]
    fn v2_decoder_rejects_truncation_corruption_and_trailing_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        for object in [database_v2()?, analyzer_v2()?, keyspace_v2()?, search_v2()?] {
            let encoded = object.encode_definition_v2()?;
            for length in 0..encoded.len() {
                assert!(LogicalCatalogObject::decode_definition_v2(&encoded[..length]).is_err());
            }
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert_eq!(
                LogicalCatalogObject::decode_definition_v2(&trailing),
                Err(CatalogError::InvalidDefinitionEncoding)
            );
        }

        let encoded = search_v2()?.encode_definition_v2()?;
        let mut wrong_kind = encoded.clone();
        wrong_kind[8] = CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: header_v2(20, EngineKind::Search, "unused", Some(11))?,
            tokenizer: AnalyzerTokenizer::Keyword,
            filters: Vec::new(),
        })
        .kind() as u8;
        assert!(LogicalCatalogObject::decode_definition_v2(&wrong_kind).is_err());

        let relation = relation()?;
        let mut wrapped =
            relation.encode_definition_v2(ObjectId::new(11)?, DefinitionVersion::FIRST)?;
        let wrapped_v1 = relation.encode_definition()?;
        let v1_offset = wrapped
            .windows(wrapped_v1.len())
            .position(|window| window == wrapped_v1)
            .ok_or("wrapped V1 definition not found")?;
        wrapped[v1_offset + 9] = 2;
        assert!(LogicalCatalogObject::decode_definition_v2(&wrapped).is_err());
        Ok(())
    }

    #[test]
    fn duplicate_and_contradictory_v2_policies_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(mut analyzer)) = analyzer_v2()?
        else {
            return Err("expected analyzer".into());
        };
        analyzer.filters.push(AnalyzerFilter::Lowercase);
        assert_eq!(
            LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(analyzer)).encode_definition_v2(),
            Err(CatalogError::DuplicateAnalyzerFilter)
        );

        let LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(mut search)) = search_v2()?
        else {
            return Err("expected search collection".into());
        };
        search.vectors[1].name = search.vectors[0].name.clone();
        assert!(matches!(
            LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(search.clone()))
                .encode_definition_v2(),
            Err(CatalogError::DuplicateVectorName(_))
        ));
        search.vectors[1].name = CatalogName::unquoted("body_vector")?;
        search.vectors[1].lifecycle.delta_max_entries = 0;
        assert_eq!(
            LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(search))
                .encode_definition_v2(),
            Err(CatalogError::InvalidVectorPolicy)
        );

        let LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(mut keyspace)) = keyspace_v2()?
        else {
            return Err("expected keyspace".into());
        };
        keyspace.default_ttl_millis = None;
        assert_eq!(
            LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(keyspace)).encode_definition_v2(),
            Err(CatalogError::InvalidKeyspacePolicy)
        );
        Ok(())
    }

    #[test]
    fn ann_definition_requires_a_vector_and_checked_hnsw_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            AnnIndexDefinition::new(VectorMetric::SquaredL2, 1, 16, 16, 32, 0),
            Err(CatalogError::InvalidAnnM)
        );
        assert_eq!(
            AnnIndexDefinition::new(VectorMetric::NegativeDot, 16, 8, 16, 32, 0),
            Err(CatalogError::InvalidAnnEfConstruction)
        );
        assert_eq!(
            AnnIndexDefinition::new(VectorMetric::Cosine, 16, 64, 65, 64, 0),
            Err(CatalogError::InvalidAnnEfSearch)
        );

        let CatalogObject::Search(mut definition) = search()? else {
            return Err("expected search definition".into());
        };
        definition.vector = None;
        assert_eq!(
            CatalogObject::Search(definition).validate(),
            Err(CatalogError::AnnRequiresVector)
        );
        Ok(())
    }

    #[test]
    fn definition_decoder_rejects_every_truncated_prefix_and_corruption()
    -> Result<(), Box<dyn std::error::Error>> {
        for object in [
            relation()?,
            structure()?,
            search()?,
            secondary_index()?,
            cross_engine_link()?,
        ] {
            let encoded = object.encode_definition()?;
            for length in 0..encoded.len() {
                assert!(CatalogObject::decode_definition(&encoded[..length]).is_err());
            }
        }

        let encoded = relation()?.encode_definition()?;
        let mut wrong_owner = encoded.clone();
        wrong_owner[25] = EngineKind::Search as u8;
        assert_eq!(
            CatalogObject::decode_definition(&wrong_owner),
            Err(CatalogError::WrongObjectOwner)
        );

        let mut invalid_type = encoded.clone();
        let type_length_and_descriptor = [2, 0, 0, 0, 3, 64];
        let type_tag = invalid_type
            .windows(type_length_and_descriptor.len())
            .position(|window| window == type_length_and_descriptor)
            .map(|offset| offset + 4)
            .ok_or("type tag not found")?;
        invalid_type[type_tag] = 0xff;
        assert!(CatalogObject::decode_definition(&invalid_type).is_err());

        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            CatalogObject::decode_definition(&trailing),
            Err(CatalogError::InvalidDefinitionEncoding)
        );
        assert_eq!(
            CatalogObject::decode_definition(&vec![0; MAX_CATALOG_DEFINITION_BYTES + 1]),
            Err(CatalogError::DefinitionTooLarge)
        );
        Ok(())
    }

    #[test]
    fn secondary_index_validation_rejects_ambiguous_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let CatalogObject::SecondaryIndex(mut definition) = secondary_index()? else {
            return Err("expected secondary index".into());
        };
        definition.columns.clear();
        assert_eq!(
            CatalogObject::SecondaryIndex(definition.clone()).validate(),
            Err(CatalogError::EmptySecondaryIndex)
        );

        definition.columns = vec![ColumnId::new(2)?, ColumnId::new(2)?];
        assert_eq!(
            CatalogObject::SecondaryIndex(definition.clone()).validate(),
            Err(CatalogError::DuplicateSecondaryIndexColumn(ColumnId::new(
                2
            )?))
        );

        definition.columns = vec![ColumnId::new(2)?];
        definition.relation = definition.header.id;
        assert_eq!(
            CatalogObject::SecondaryIndex(definition.clone()).validate(),
            Err(CatalogError::SelfReferentialSecondaryIndex)
        );

        definition.relation = ObjectId::new(1)?;
        definition.header.owner = EngineKind::Search;
        assert_eq!(
            CatalogObject::SecondaryIndex(definition).validate(),
            Err(CatalogError::WrongObjectOwner)
        );
        Ok(())
    }

    #[test]
    fn object_validation_rejects_ambiguous_schema_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let CatalogObject::Relation(mut definition) = relation()? else {
            return Err("expected relation".into());
        };
        definition.header.owner = EngineKind::Search;
        assert_eq!(
            CatalogObject::Relation(definition.clone()).validate(),
            Err(CatalogError::WrongObjectOwner)
        );

        definition.header.owner = EngineKind::Relational;
        definition.columns[1].name = CatalogName::unquoted("ID")?;
        assert!(matches!(
            CatalogObject::Relation(definition.clone()).validate(),
            Err(CatalogError::DuplicateColumnName(_))
        ));

        definition.columns[1].name = CatalogName::unquoted("display_name")?;
        definition.columns.swap(0, 1);
        assert_eq!(
            CatalogObject::Relation(definition.clone()).validate(),
            Err(CatalogError::NoncanonicalColumnOrder)
        );

        definition.columns.swap(0, 1);
        definition.columns[0].nullable = true;
        assert_eq!(
            CatalogObject::Relation(definition).validate(),
            Err(CatalogError::NullablePrimaryKeyColumn(ColumnId::new(1)?))
        );
        assert_eq!(
            CatalogName::quoted("x".repeat(MAX_CATALOG_NAME_BYTES + 1)),
            Err(CatalogError::NameTooLong)
        );
        Ok(())
    }

    #[test]
    fn cross_engine_link_codec_rejects_unbounded_and_noncanonical_mappings()
    -> Result<(), Box<dyn std::error::Error>> {
        let CatalogObject::CrossEngineLink(mut definition) = cross_engine_link()? else {
            return Err("expected cross-engine link".into());
        };
        definition.mapping.swap(0, 1);
        assert_eq!(
            definition.validate(),
            Err(CatalogError::InvalidCrossEngineLink)
        );
        definition.mapping = vec![CrossEngineLinkMapping {
            source: 0,
            target: 1,
        }];
        assert_eq!(
            CatalogObject::CrossEngineLink(definition).encode_definition(),
            Err(CatalogError::InvalidCrossEngineLink)
        );
        Ok(())
    }
}
