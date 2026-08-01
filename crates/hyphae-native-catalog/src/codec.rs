// SPDX-License-Identifier: Apache-2.0

use std::str;

use hyphae_native_types::{
    ColumnId, EngineKind, FieldId, LogicalType, ObjectId, VectorElement, VectorType,
};

use super::{
    CatalogError, CatalogName, CatalogObject, ColumnDefinition, MAX_CATALOG_DEFINITION_BYTES,
    MAX_CATALOG_DEFINITION_ITEMS, MAX_CATALOG_NAME_BYTES, ObjectHeader, QualifiedName,
    RelationDefinition, SearchCollectionDefinition, SearchFieldDefinition, StructureDefinition,
    StructureKind, StructureOwnership,
};

const DEFINITION_MAGIC: [u8; 8] = *b"HYCOBJ01";
const OBJECT_RELATION: u8 = 1;
const OBJECT_STRUCTURE: u8 = 2;
const OBJECT_SEARCH: u8 = 3;

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
            Self::Structure(_) => OBJECT_STRUCTURE,
            Self::Search(_) => OBJECT_SEARCH,
        };
        let mut encoder = Encoder::new(tag);
        encoder.put_header(self.header())?;
        match self {
            Self::Relation(definition) => encoder.put_relation(definition)?,
            Self::Structure(definition) => encoder.put_structure(definition)?,
            Self::Search(definition) => encoder.put_search(definition)?,
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
        let mut decoder = Decoder::new(encoded)?;
        let tag = decoder.byte()?;
        let header = decoder.header()?;
        let object = match tag {
            OBJECT_RELATION => Self::Relation(decoder.relation(header)?),
            OBJECT_STRUCTURE => Self::Structure(decoder.structure(header)?),
            OBJECT_SEARCH => Self::Search(decoder.search(header)?),
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        decoder.finish()?;
        object.validate()?;
        if object.encode_definition()? != encoded {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(object)
    }
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(tag: u8) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&DEFINITION_MAGIC);
        bytes.push(tag);
        Self { bytes }
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
        Ok(())
    }

    fn put_structure(&mut self, definition: &StructureDefinition) -> Result<(), CatalogError> {
        self.put_byte(definition.kind as u8)?;
        self.put_logical_type(&definition.key_type)?;
        self.put_logical_type(&definition.value_type)?;
        self.put_byte(definition.ownership as u8)?;
        self.put_bool(definition.ttl_enabled)
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
        match definition.vector {
            Some(vector) => {
                self.put_byte(1)?;
                self.put_byte(vector.element() as u8)?;
                self.put_fixed(&vector.dimension().to_le_bytes())?;
            }
            None => self.put_byte(0)?,
        }
        Ok(())
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
    fn new(encoded: &'encoded [u8]) -> Result<Self, CatalogError> {
        if encoded.len() > MAX_CATALOG_DEFINITION_BYTES {
            return Err(CatalogError::DefinitionTooLarge);
        }
        if !encoded.starts_with(&DEFINITION_MAGIC) {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(Self {
            encoded,
            offset: DEFINITION_MAGIC.len(),
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
        Ok(RelationDefinition {
            header,
            columns,
            primary_key,
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
        let vector = match self.byte()? {
            0 => None,
            1 => {
                if self.byte()? != VectorElement::Float32 as u8 {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                }
                let dimension = u16::from_le_bytes(self.fixed()?);
                Some(
                    VectorType::new(VectorElement::Float32, dimension)
                        .map_err(|_| CatalogError::InvalidDefinitionEncoding)?,
                )
            }
            _ => return Err(CatalogError::InvalidDefinitionEncoding),
        };
        Ok(SearchCollectionDefinition {
            header,
            fields,
            vector,
        })
    }

    fn header(&mut self) -> Result<ObjectHeader, CatalogError> {
        Ok(ObjectHeader {
            id: self.object_id()?,
            owner: self.engine()?,
            name: QualifiedName::new(self.name()?, self.name()?, self.name()?),
        })
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
        CatalogError, CatalogName, CatalogObject, ColumnDefinition, MAX_CATALOG_DEFINITION_BYTES,
        ObjectHeader, QualifiedName, RelationDefinition, SearchCollectionDefinition,
        SearchFieldDefinition, StructureDefinition, StructureKind, StructureOwnership,
    };
    use crate::MAX_CATALOG_NAME_BYTES;

    const RELATION_GOLDEN_HEX: &str = concat!(
        "4859434f424a3031010100000000000000000000000000000001040000006d61696e040000006d",
        "61696e060000007075626c6963060000007075626c6963080000006163636f756e74730800000061",
        "63636f756e7473020000000100000002000000696402000000696402000000034000020000000c00",
        "0000646973706c61795f6e616d650c000000646973706c61795f6e616d650100000007010100000001",
        "000000"
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
        }))
    }

    #[test]
    fn every_object_definition_has_one_canonical_round_trip()
    -> Result<(), Box<dyn std::error::Error>> {
        for object in [relation()?, structure()?, search()?] {
            let encoded = object.encode_definition()?;
            if object.header().id.get() == 1 {
                assert_eq!(hex(&encoded)?, RELATION_GOLDEN_HEX);
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
    fn definition_decoder_rejects_every_truncated_prefix_and_corruption()
    -> Result<(), Box<dyn std::error::Error>> {
        let encoded = relation()?.encode_definition()?;
        for length in 0..encoded.len() {
            assert!(CatalogObject::decode_definition(&encoded[..length]).is_err());
        }

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
}
