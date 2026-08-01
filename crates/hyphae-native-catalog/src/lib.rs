// SPDX-License-Identifier: Apache-2.0

//! Immutable versioned catalog model for Hyphae's native engines.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use hyphae_native_types::{
    CatalogVersion, ColumnId, EngineKind, FieldId, LogicalType, ObjectId, VectorType,
};
use thiserror::Error;

/// Catalog construction or lookup failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CatalogError {
    /// One name component is empty.
    #[error("catalog name component must be nonempty")]
    EmptyName,
    /// One stable object identity already exists.
    #[error("catalog object ID {0} already exists")]
    DuplicateObjectId(ObjectId),
    /// One normalized qualified name already exists.
    #[error("catalog object name already exists: {0}")]
    DuplicateName(Box<QualifiedName>),
    /// A column ID is duplicated inside a relation.
    #[error("column ID {0} is duplicated")]
    DuplicateColumnId(ColumnId),
    /// A field ID is duplicated inside a search collection.
    #[error("field ID {0} is duplicated")]
    DuplicateFieldId(FieldId),
    /// A primary-key column is not part of the relation.
    #[error("primary-key column {0} does not exist")]
    MissingPrimaryKeyColumn(ColumnId),
    /// Catalog version space is exhausted.
    #[error("catalog version space is exhausted")]
    VersionExhausted,
}

/// Display and normalized representation of one catalog name component.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CatalogName {
    display: String,
    lookup: String,
}

impl CatalogName {
    /// Constructs an unquoted identifier and folds ASCII uppercase to lowercase.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty identifier.
    pub fn unquoted(value: impl Into<String>) -> Result<Self, CatalogError> {
        let display = value.into();
        if display.is_empty() {
            return Err(CatalogError::EmptyName);
        }
        let lookup = display
            .chars()
            .map(|character| {
                if character.is_ascii_uppercase() {
                    character.to_ascii_lowercase()
                } else {
                    character
                }
            })
            .collect();
        Ok(Self { display, lookup })
    }

    /// Constructs a quoted identifier and preserves exact UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty identifier.
    pub fn quoted(value: impl Into<String>) -> Result<Self, CatalogError> {
        let display = value.into();
        if display.is_empty() {
            return Err(CatalogError::EmptyName);
        }
        Ok(Self {
            lookup: display.clone(),
            display,
        })
    }

    /// Returns the original display spelling.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Returns the normalized lookup spelling.
    pub fn lookup(&self) -> &str {
        &self.lookup
    }
}

/// Fully qualified normalized catalog name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QualifiedName {
    /// Database component.
    pub database: CatalogName,
    /// Schema component.
    pub schema: CatalogName,
    /// Object component.
    pub object: CatalogName,
}

impl QualifiedName {
    /// Constructs one fully qualified name.
    pub const fn new(database: CatalogName, schema: CatalogName, object: CatalogName) -> Self {
        Self {
            database,
            schema,
            object,
        }
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}.{}.{}",
            self.database.display(),
            self.schema.display(),
            self.object.display()
        )
    }
}

/// Shared immutable object metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectHeader {
    /// Stable object identity.
    pub id: ObjectId,
    /// Owning engine.
    pub owner: EngineKind,
    /// Qualified display and lookup name.
    pub name: QualifiedName,
}

/// One native relational column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDefinition {
    /// Stable column identity.
    pub id: ColumnId,
    /// Display and lookup name.
    pub name: CatalogName,
    /// Canonical logical type.
    pub logical_type: LogicalType,
    /// Whether SQL `NULL` is accepted.
    pub nullable: bool,
}

/// Native relational object definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationDefinition {
    /// Shared object metadata.
    pub header: ObjectHeader,
    /// Columns ordered by stable column identity.
    pub columns: Vec<ColumnDefinition>,
    /// Ordered primary-key column identities.
    pub primary_key: Vec<ColumnId>,
}

impl RelationDefinition {
    /// Validates one relation definition.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate columns or a missing primary-key column.
    pub fn validate(&self) -> Result<(), CatalogError> {
        let mut columns = BTreeSet::new();
        for column in &self.columns {
            if !columns.insert(column.id) {
                return Err(CatalogError::DuplicateColumnId(column.id));
            }
        }
        for column in &self.primary_key {
            if !columns.contains(column) {
                return Err(CatalogError::MissingPrimaryKeyColumn(*column));
            }
        }
        Ok(())
    }
}

/// Native structure-object family.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum StructureKind {
    /// String or binary scalar.
    String = 1,
    /// Checked numeric counter.
    Counter = 2,
    /// Typed hash/map.
    Hash = 3,
    /// Chunked deque list.
    List = 4,
    /// Unordered set.
    Set = 5,
    /// Score-ordered set.
    SortedSet = 6,
    /// Append-ordered stream.
    Stream = 7,
}

/// Structure ownership and eviction class.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum StructureOwnership {
    /// Durable source data that cannot be silently evicted.
    Canonical = 1,
    /// Explicitly evictable or reconstructible cache data.
    Cache = 2,
}

/// Native keyspace/structure object definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructureDefinition {
    /// Shared object metadata.
    pub header: ObjectHeader,
    /// Native structure family.
    pub kind: StructureKind,
    /// Canonical key type.
    pub key_type: LogicalType,
    /// Canonical value/member type.
    pub value_type: LogicalType,
    /// Source or cache ownership.
    pub ownership: StructureOwnership,
    /// Whether versions may carry an expiry timestamp.
    pub ttl_enabled: bool,
}

/// One search field mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchFieldDefinition {
    /// Stable search-field identity.
    pub id: FieldId,
    /// Display and lookup name.
    pub name: CatalogName,
    /// Source logical type.
    pub logical_type: LogicalType,
    /// Analyzer identity for text, when applicable.
    pub analyzer: Option<ObjectId>,
    /// Whether typed doc values are materialized.
    pub doc_values: bool,
}

/// Native search collection definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCollectionDefinition {
    /// Shared object metadata.
    pub header: ObjectHeader,
    /// Search fields ordered by stable field identity.
    pub fields: Vec<SearchFieldDefinition>,
    /// Optional fixed-dimension vector index.
    pub vector: Option<VectorType>,
}

impl SearchCollectionDefinition {
    /// Validates one search definition.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate field identities.
    pub fn validate(&self) -> Result<(), CatalogError> {
        let mut fields = BTreeSet::new();
        for field in &self.fields {
            if !fields.insert(field.id) {
                return Err(CatalogError::DuplicateFieldId(field.id));
            }
        }
        Ok(())
    }
}

/// One native catalog object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogObject {
    /// Relational relation.
    Relation(RelationDefinition),
    /// Keyspace structure.
    Structure(StructureDefinition),
    /// Search collection.
    Search(SearchCollectionDefinition),
}

impl CatalogObject {
    /// Returns common object metadata.
    pub const fn header(&self) -> &ObjectHeader {
        match self {
            Self::Relation(definition) => &definition.header,
            Self::Structure(definition) => &definition.header,
            Self::Search(definition) => &definition.header,
        }
    }

    fn validate(&self) -> Result<(), CatalogError> {
        match self {
            Self::Relation(definition) => definition.validate(),
            Self::Structure(_) => Ok(()),
            Self::Search(definition) => definition.validate(),
        }
    }
}

/// Immutable catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    version: CatalogVersion,
    objects: BTreeMap<ObjectId, CatalogObject>,
    names: BTreeMap<QualifiedName, ObjectId>,
}

impl CatalogSnapshot {
    /// Constructs an empty catalog at one explicit version.
    pub fn empty(version: CatalogVersion) -> Arc<Self> {
        Arc::new(Self {
            version,
            objects: BTreeMap::new(),
            names: BTreeMap::new(),
        })
    }

    /// Returns the immutable catalog version.
    pub const fn version(&self) -> CatalogVersion {
        self.version
    }

    /// Looks up an object by stable identity.
    pub fn object(&self, id: ObjectId) -> Option<&CatalogObject> {
        self.objects.get(&id)
    }

    /// Looks up an object by normalized qualified name.
    pub fn object_named(&self, name: &QualifiedName) -> Option<&CatalogObject> {
        self.names.get(name).and_then(|id| self.objects.get(id))
    }

    /// Returns the number of live objects.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Returns whether the catalog contains no live objects.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

/// Private catalog write set that produces a new immutable snapshot.
#[derive(Debug)]
pub struct CatalogTransaction {
    base: Arc<CatalogSnapshot>,
    additions: Vec<CatalogObject>,
}

impl CatalogTransaction {
    /// Begins from one immutable base snapshot.
    pub fn begin(base: Arc<CatalogSnapshot>) -> Self {
        Self {
            base,
            additions: Vec::new(),
        }
    }

    /// Stages one validated object.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid definitions or duplicate ID/name in the
    /// base or current write set.
    pub fn create(&mut self, object: CatalogObject) -> Result<(), CatalogError> {
        object.validate()?;
        let header = object.header();
        if self.base.objects.contains_key(&header.id)
            || self
                .additions
                .iter()
                .any(|existing| existing.header().id == header.id)
        {
            return Err(CatalogError::DuplicateObjectId(header.id));
        }
        if self.base.names.contains_key(&header.name)
            || self
                .additions
                .iter()
                .any(|existing| existing.header().name == header.name)
        {
            return Err(CatalogError::DuplicateName(Box::new(header.name.clone())));
        }
        self.additions.push(object);
        Ok(())
    }

    /// Commits a new immutable in-memory catalog snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when catalog version space is exhausted.
    pub fn commit(self) -> Result<Arc<CatalogSnapshot>, CatalogError> {
        let version = self
            .base
            .version
            .checked_next()
            .ok_or(CatalogError::VersionExhausted)?;
        let mut objects = self.base.objects.clone();
        let mut names = self.base.names.clone();
        for object in self.additions {
            let header = object.header();
            names.insert(header.name.clone(), header.id);
            objects.insert(header.id, object);
        }
        Ok(Arc::new(CatalogSnapshot {
            version,
            objects,
            names,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hyphae_native_types::{
        CatalogVersion, ColumnId, EngineKind, IntegerWidth, LogicalType, ObjectId,
    };

    use super::{
        CatalogError, CatalogName, CatalogObject, CatalogSnapshot, CatalogTransaction,
        ColumnDefinition, ObjectHeader, QualifiedName, RelationDefinition,
    };

    fn relation(id: u128, name: &str) -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::Relation(RelationDefinition {
            header: ObjectHeader {
                id: ObjectId::new(id)?,
                owner: EngineKind::Relational,
                name: QualifiedName::new(
                    CatalogName::unquoted("main")?,
                    CatalogName::unquoted("public")?,
                    CatalogName::unquoted(name)?,
                ),
            },
            columns: vec![ColumnDefinition {
                id: ColumnId::new(1)?,
                name: CatalogName::unquoted("id")?,
                logical_type: LogicalType::Unsigned(IntegerWidth::Bits64),
                nullable: false,
            }],
            primary_key: vec![ColumnId::new(1)?],
        }))
    }

    #[test]
    fn unquoted_names_fold_ascii_only() -> Result<(), CatalogError> {
        let name = CatalogName::unquoted("Accounts")?;
        assert_eq!(name.display(), "Accounts");
        assert_eq!(name.lookup(), "accounts");
        Ok(())
    }

    #[test]
    fn commit_preserves_the_base_snapshot() -> Result<(), Box<dyn std::error::Error>> {
        let base = CatalogSnapshot::empty(CatalogVersion::new(1)?);
        let mut transaction = CatalogTransaction::begin(Arc::clone(&base));
        transaction.create(relation(1, "accounts")?)?;
        let committed = transaction.commit()?;
        assert!(base.is_empty());
        assert_eq!(committed.len(), 1);
        assert_eq!(committed.version().get(), 2);
        Ok(())
    }

    #[test]
    fn duplicate_names_and_ids_fail_before_commit() -> Result<(), Box<dyn std::error::Error>> {
        let base = CatalogSnapshot::empty(CatalogVersion::new(1)?);
        let mut transaction = CatalogTransaction::begin(base);
        transaction.create(relation(1, "accounts")?)?;
        assert!(matches!(
            transaction.create(relation(2, "accounts")?),
            Err(CatalogError::DuplicateName(_))
        ));
        assert!(matches!(
            transaction.create(relation(1, "other")?),
            Err(CatalogError::DuplicateObjectId(_))
        ));
        Ok(())
    }

    #[test]
    fn primary_key_must_reference_a_live_column() -> Result<(), Box<dyn std::error::Error>> {
        let CatalogObject::Relation(mut invalid) = relation(1, "accounts")? else {
            return Err("expected relation".into());
        };
        invalid.primary_key = vec![ColumnId::new(2)?];
        assert_eq!(
            invalid.validate(),
            Err(CatalogError::MissingPrimaryKeyColumn(ColumnId::new(2)?))
        );
        Ok(())
    }
}
