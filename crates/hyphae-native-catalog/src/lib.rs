// SPDX-License-Identifier: AGPL-3.0-only

//! Immutable versioned catalog model for Hyphae's native engines.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    sync::Arc,
};

use hyphae_native_types::{
    CatalogVersion, ColumnId, EngineKind, FieldId, LogicalType, NativeTypeError, ObjectId,
    ScalarValue, VectorType,
};
use thiserror::Error;

mod codec;

/// Maximum UTF-8 byte length of one catalog name component.
pub const MAX_CATALOG_NAME_BYTES: usize = 1_024;
/// Maximum number of columns, fields, or key members in one definition list.
pub const MAX_CATALOG_DEFINITION_ITEMS: usize = 100_000;
/// Maximum canonical byte length of one catalog object definition.
pub const MAX_CATALOG_DEFINITION_BYTES: usize = 16 * 1024 * 1024;
/// Maximum object-keyed mutations admitted by one incremental vector delta.
pub const MAX_INCREMENTAL_VECTOR_DELTA_ENTRIES: u32 = 4_096;
/// Maximum obsolete ANN generations retained by one vector definition.
pub const MAX_INCREMENTAL_VECTOR_RETAINED_GENERATIONS: u16 = 64;

/// Catalog construction or lookup failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CatalogError {
    /// One name component is empty.
    #[error("catalog name component must be nonempty")]
    EmptyName,
    /// One name component exceeds the canonical UTF-8 byte bound.
    #[error("catalog name component exceeds 1024 UTF-8 bytes")]
    NameTooLong,
    /// One stable object identity already exists.
    #[error("catalog object ID {0} already exists")]
    DuplicateObjectId(ObjectId),
    /// One normalized qualified name already exists.
    #[error("catalog object name already exists: {0}")]
    DuplicateName(Box<QualifiedName>),
    /// A column ID is duplicated inside a relation.
    #[error("column ID {0} is duplicated")]
    DuplicateColumnId(ColumnId),
    /// A normalized column name is duplicated inside a relation.
    #[error("column name is duplicated: {0}")]
    DuplicateColumnName(Box<CatalogName>),
    /// Relation columns are not strictly ordered by stable column ID.
    #[error("relation columns must be strictly ordered by column ID")]
    NoncanonicalColumnOrder,
    /// A relation has no columns.
    #[error("relation must contain at least one column")]
    EmptyRelation,
    /// A field ID is duplicated inside a search collection.
    #[error("field ID {0} is duplicated")]
    DuplicateFieldId(FieldId),
    /// A normalized field name is duplicated inside a search collection.
    #[error("search field name is duplicated: {0}")]
    DuplicateFieldName(Box<CatalogName>),
    /// Search fields are not strictly ordered by stable field ID.
    #[error("search fields must be strictly ordered by field ID")]
    NoncanonicalFieldOrder,
    /// ANN configuration is present without a fixed vector type.
    #[error("ANN configuration requires a fixed vector type")]
    AnnRequiresVector,
    /// HNSW `M` is outside the versioned range.
    #[error("catalog HNSW M must be in 2 through 64")]
    InvalidAnnM,
    /// HNSW construction breadth is smaller than `M`.
    #[error("catalog HNSW ef_construction must be at least M")]
    InvalidAnnEfConstruction,
    /// HNSW query breadth is zero, inverted, or exceeds its maximum.
    #[error("catalog HNSW ef_search bounds are invalid")]
    InvalidAnnEfSearch,
    /// A primary-key column is not part of the relation.
    #[error("primary-key column {0} does not exist")]
    MissingPrimaryKeyColumn(ColumnId),
    /// A primary-key column was listed more than once.
    #[error("primary-key column {0} is duplicated")]
    DuplicatePrimaryKeyColumn(ColumnId),
    /// A primary-key column was declared nullable.
    #[error("primary-key column {0} cannot be nullable")]
    NullablePrimaryKeyColumn(ColumnId),
    /// A secondary index has no key columns.
    #[error("secondary index must contain at least one key column")]
    EmptySecondaryIndex,
    /// A secondary-index column was listed more than once.
    #[error("secondary-index column {0} is duplicated")]
    DuplicateSecondaryIndexColumn(ColumnId),
    /// A secondary index names itself as its owning relation.
    #[error("secondary index cannot reference itself as its relation")]
    SelfReferentialSecondaryIndex,
    /// A secondary index references an absent or non-relation object.
    #[error("secondary index relation {0} does not exist")]
    MissingSecondaryIndexRelation(ObjectId),
    /// A secondary-index column is not part of its relation.
    #[error("secondary-index column {0} does not exist in its relation")]
    MissingSecondaryIndexColumn(ColumnId),
    /// An object variant names the wrong owning engine.
    #[error("catalog object owner does not match its object kind")]
    WrongObjectOwner,
    /// A definition list exceeds its canonical item bound.
    #[error("catalog definition contains more than 100000 items")]
    TooManyDefinitionItems,
    /// A definition exceeds its canonical byte bound.
    #[error("catalog definition exceeds 16 MiB")]
    DefinitionTooLarge,
    /// A catalog definition is malformed or noncanonical.
    #[error("catalog definition encoding is malformed or noncanonical")]
    InvalidDefinitionEncoding,
    /// A nested native logical-type descriptor is invalid.
    #[error(transparent)]
    NativeType(#[from] NativeTypeError),
    /// A cross-engine link endpoint or mapping is malformed.
    #[error("cross-engine link definition is invalid")]
    InvalidCrossEngineLink,
    /// Catalog version space is exhausted.
    #[error("catalog version space is exhausted")]
    VersionExhausted,
    /// A V2 definition version is zero.
    #[error("catalog definition version must be nonzero")]
    InvalidDefinitionVersion,
    /// A V2 parent is absent, self-referential, or has the wrong object kind.
    #[error("catalog object hierarchy is invalid")]
    InvalidObjectHierarchy,
    /// An analyzer repeats one token-filter policy.
    #[error("analyzer token-filter policy is duplicated")]
    DuplicateAnalyzerFilter,
    /// A search field has a contradictory or empty materialization policy.
    #[error("search field policy is invalid")]
    InvalidSearchFieldPolicy,
    /// A named vector field ID is duplicated.
    #[error("named vector field ID {0} is duplicated")]
    DuplicateVectorId(FieldId),
    /// A normalized named vector or search-field name is duplicated.
    #[error("named vector or search-field name is duplicated: {0}")]
    DuplicateVectorName(Box<CatalogName>),
    /// A named vector policy or lifecycle is invalid.
    #[error("named vector policy or lifecycle is invalid")]
    InvalidVectorPolicy,
    /// A keyspace TTL, memory, or eviction policy is contradictory.
    #[error("keyspace policy is invalid")]
    InvalidKeyspacePolicy,
    /// A derived dependency names an object absent from the supplied set.
    #[error("catalog dependency target {0} does not exist")]
    MissingDependencyTarget(ObjectId),
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
        validate_name_length(&display)?;
        let lookup = fold_unquoted_name(&display);
        Ok(Self { display, lookup })
    }

    /// Constructs a quoted identifier and preserves exact UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty identifier.
    pub fn quoted(value: impl Into<String>) -> Result<Self, CatalogError> {
        let display = value.into();
        validate_name_length(&display)?;
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

    fn from_encoded_parts(display: String, lookup: String) -> Result<Self, CatalogError> {
        validate_name_length(&display)?;
        validate_name_length(&lookup)?;
        if lookup != display && lookup != fold_unquoted_name(&display) {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        Ok(Self { display, lookup })
    }
}

impl std::fmt::Display for CatalogName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display.fmt(formatter)
    }
}

fn validate_name_length(value: &str) -> Result<(), CatalogError> {
    if value.is_empty() {
        return Err(CatalogError::EmptyName);
    }
    if value.len() > MAX_CATALOG_NAME_BYTES {
        return Err(CatalogError::NameTooLong);
    }
    Ok(())
}

fn fold_unquoted_name(value: &str) -> String {
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

/// Nonzero logical definition revision embedded in `HYCOBJ02`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DefinitionVersion(NonZeroU64);

impl DefinitionVersion {
    /// First logical definition revision.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Constructs a nonzero logical definition revision.
    ///
    /// # Errors
    ///
    /// Returns an error for zero.
    pub fn new(value: u64) -> Result<Self, CatalogError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(CatalogError::InvalidDefinitionVersion)
    }

    /// Returns the encoded revision number.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Stable SHA-256 digest of one complete canonical logical definition.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DefinitionDigest([u8; 32]);

impl DefinitionDigest {
    /// Constructs a digest from its canonical bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the canonical 32-byte digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Stable logical catalog object family used by V2 views and dependency edges.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum CatalogObjectKind {
    /// Top-level database namespace.
    Database = 1,
    /// Schema namespace owned by one database.
    Schema = 2,
    /// Relational relation.
    Relation = 3,
    /// Relational secondary index.
    SecondaryIndex = 4,
    /// First-class structure keyspace.
    Keyspace = 5,
    /// V1-compatible structure object.
    Structure = 6,
    /// Search collection.
    SearchCollection = 7,
    /// Reusable lexical analyzer.
    Analyzer = 8,
    /// Explicit cross-engine link.
    CrossEngineLink = 9,
}

/// Shared metadata for native V2-only catalog definitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectHeaderV2 {
    /// Stable object identity.
    pub id: ObjectId,
    /// Owning engine.
    pub owner: EngineKind,
    /// Qualified display and lookup name.
    pub name: QualifiedName,
    /// Stable parent identity, absent only for a database.
    pub parent: Option<ObjectId>,
    /// Monotonic logical definition revision for this stable object ID.
    pub definition_version: DefinitionVersion,
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

/// Comparison operator admitted by a column CHECK constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ColumnCheckOperator {
    /// `=`.
    Equal = 1,
    /// `<>`.
    NotEqual = 2,
    /// `<`.
    Less = 3,
    /// `<=`.
    LessOrEqual = 4,
    /// `>`.
    Greater = 5,
    /// `>=`.
    GreaterOrEqual = 6,
}

/// One canonical column-local CHECK predicate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnCheckConstraint {
    /// Comparison against the literal.
    pub operator: ColumnCheckOperator,
    /// Typed literal operand.
    pub operand: ScalarValue,
}

/// One immediate MATCH SIMPLE primary-key foreign key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignKeyDefinition {
    /// Optional normalized constraint name.
    pub name: Option<CatalogName>,
    /// Ordered child columns.
    pub columns: Vec<ColumnId>,
    /// Referenced relation identity.
    pub referenced_relation: ObjectId,
    /// Referenced primary-key columns, or ordered unique-index columns.
    pub referenced_columns: Vec<ColumnId>,
    /// Referenced unique index, or `None` for the primary key.
    pub referenced_index: Option<ObjectId>,
}

impl ForeignKeyDefinition {
    /// Validates one immediate primary-key foreign key against both relations.
    ///
    /// # Errors
    ///
    /// Returns an error for missing columns, arity/type mismatch, or a target
    /// that is not the complete ordered parent primary key.
    pub fn validate_relations(
        &self,
        child: &RelationDefinition,
        parent: &RelationDefinition,
    ) -> Result<(), CatalogError> {
        if self.columns.is_empty()
            || self.columns.len() != self.referenced_columns.len()
            || (self.referenced_index.is_none() && self.referenced_columns != parent.primary_key)
        {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        for (child_id, parent_id) in self.columns.iter().zip(&self.referenced_columns) {
            let child_column = child
                .columns
                .iter()
                .find(|column| column.id == *child_id)
                .ok_or(CatalogError::InvalidDefinitionEncoding)?;
            let parent_column = parent
                .columns
                .iter()
                .find(|column| column.id == *parent_id)
                .ok_or(CatalogError::InvalidDefinitionEncoding)?;
            if child_column.logical_type != parent_column.logical_type {
                return Err(CatalogError::InvalidDefinitionEncoding);
            }
        }
        Ok(())
    }
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
    /// Column-local constraints keyed by stable column identity.
    pub checks: Vec<(ColumnId, ColumnCheckConstraint)>,
    /// Immediate MATCH SIMPLE foreign keys declared by this relation.
    pub foreign_keys: Vec<ForeignKeyDefinition>,
}

impl RelationDefinition {
    /// Validates one relation definition.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate columns or a missing primary-key column.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.columns.is_empty() {
            return Err(CatalogError::EmptyRelation);
        }
        if self.columns.len() > MAX_CATALOG_DEFINITION_ITEMS
            || self.primary_key.len() > MAX_CATALOG_DEFINITION_ITEMS
        {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        let mut column_ids = BTreeSet::new();
        let mut column_names = BTreeSet::new();
        let mut previous_id = None;
        for column in &self.columns {
            if previous_id == Some(column.id) {
                return Err(CatalogError::DuplicateColumnId(column.id));
            }
            if previous_id.is_some_and(|previous| previous > column.id) {
                return Err(CatalogError::NoncanonicalColumnOrder);
            }
            previous_id = Some(column.id);
            if !column_ids.insert(column.id) {
                return Err(CatalogError::DuplicateColumnId(column.id));
            }
            if !column_names.insert(column.name.lookup()) {
                return Err(CatalogError::DuplicateColumnName(Box::new(
                    column.name.clone(),
                )));
            }
        }
        let mut primary_key = BTreeSet::new();
        for column in &self.primary_key {
            if !primary_key.insert(*column) {
                return Err(CatalogError::DuplicatePrimaryKeyColumn(*column));
            }
            if !column_ids.contains(column) {
                return Err(CatalogError::MissingPrimaryKeyColumn(*column));
            }
            if self
                .columns
                .iter()
                .find(|definition| definition.id == *column)
                .is_some_and(|definition| definition.nullable)
            {
                return Err(CatalogError::NullablePrimaryKeyColumn(*column));
            }
        }
        let mut previous_check = None;
        for (column, check) in &self.checks {
            if previous_check.is_some_and(|previous| previous >= *column)
                || !column_ids.contains(column)
                || matches!(check.operand, ScalarValue::Null)
            {
                return Err(CatalogError::InvalidDefinitionEncoding);
            }
            let definition = self
                .columns
                .iter()
                .find(|definition| definition.id == *column)
                .ok_or(CatalogError::InvalidDefinitionEncoding)?;
            check
                .operand
                .encode_storage(&definition.logical_type)
                .map_err(|_| CatalogError::InvalidDefinitionEncoding)?;
            previous_check = Some(*column);
        }
        let mut names = BTreeSet::new();
        for foreign_key in &self.foreign_keys {
            if let Some(name) = &foreign_key.name
                && !names.insert(name.lookup())
            {
                return Err(CatalogError::InvalidDefinitionEncoding);
            }
            if foreign_key.columns.is_empty()
                || foreign_key.columns.len() != foreign_key.referenced_columns.len()
                || foreign_key
                    .columns
                    .iter()
                    .any(|column| !column_ids.contains(column))
            {
                return Err(CatalogError::InvalidDefinitionEncoding);
            }
        }
        Ok(())
    }
}

/// Native relational secondary-index definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondaryIndexDefinition {
    /// Shared object metadata.
    pub header: ObjectHeader,
    /// Stable relation identity indexed by this object.
    pub relation: ObjectId,
    /// Ordered index-key column identities.
    pub columns: Vec<ColumnId>,
    /// Whether non-null index keys must identify at most one row.
    pub unique: bool,
    /// Whether SQL-null key components remain distinct for uniqueness.
    pub nulls_distinct: bool,
}

impl SecondaryIndexDefinition {
    /// Validates one secondary-index definition independent of its relation.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or duplicate column list, excessive
    /// members, or a self-referential relation identity.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.columns.is_empty() {
            return Err(CatalogError::EmptySecondaryIndex);
        }
        if self.columns.len() > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        if self.relation == self.header.id {
            return Err(CatalogError::SelfReferentialSecondaryIndex);
        }
        let mut columns = BTreeSet::new();
        for column in &self.columns {
            if !columns.insert(*column) {
                return Err(CatalogError::DuplicateSecondaryIndexColumn(*column));
            }
        }
        Ok(())
    }

    /// Validates stable relation and column references against one definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied relation has the wrong identity or
    /// does not contain every indexed column.
    pub fn validate_relation(&self, relation: &RelationDefinition) -> Result<(), CatalogError> {
        if relation.header.id != self.relation {
            return Err(CatalogError::MissingSecondaryIndexRelation(self.relation));
        }
        if let Some(column) = self.columns.iter().find(|column| {
            !relation
                .columns
                .iter()
                .any(|definition| definition.id == **column)
        }) {
            return Err(CatalogError::MissingSecondaryIndexColumn(*column));
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

/// Keyspace expiry policy.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum KeyspaceTtlPolicy {
    /// Values cannot carry expiry timestamps.
    Disabled = 1,
    /// Values may carry an explicit per-value expiry.
    PerValue = 2,
    /// Values use the keyspace default when no explicit expiry is supplied.
    Default = 3,
}

/// Keyspace memory-accounting class.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum KeyspaceMemoryClass {
    /// Durable canonical data receives protected memory treatment.
    Durable = 1,
    /// Normal managed memory treatment.
    Standard = 2,
    /// Reconstructible cache data eligible for eviction.
    Cache = 3,
}

/// Keyspace eviction behavior.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum KeyspaceEvictionPolicy {
    /// Never evict live values implicitly.
    None = 1,
    /// Evict least-recently-used values under memory pressure.
    LeastRecentlyUsed = 2,
    /// Evict values nearest expiry under memory pressure.
    NearestExpiry = 3,
}

/// First-class V2 keyspace definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyspaceDefinition {
    /// Shared V2 metadata.
    pub header: ObjectHeaderV2,
    /// Native structure family admitted in this keyspace.
    pub kind: StructureKind,
    /// Canonical key type.
    pub key_type: LogicalType,
    /// Canonical value or member type.
    pub value_type: LogicalType,
    /// Source or cache ownership.
    pub ownership: StructureOwnership,
    /// Expiry policy.
    pub ttl_policy: KeyspaceTtlPolicy,
    /// Default expiry in milliseconds, required only by `Default`.
    pub default_ttl_millis: Option<u64>,
    /// Memory-accounting class.
    pub memory_class: KeyspaceMemoryClass,
    /// Eviction policy.
    pub eviction: KeyspaceEvictionPolicy,
    /// Optional stable relation exposing the value schema.
    pub relation_schema: Option<ObjectId>,
}

impl KeyspaceDefinition {
    /// Validates ownership and policy combinations independent of dependencies.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid owner or contradictory policy.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.header.owner != EngineKind::Structure {
            return Err(CatalogError::WrongObjectOwner);
        }
        if self.default_ttl_millis.is_some() != (self.ttl_policy == KeyspaceTtlPolicy::Default)
            || self.default_ttl_millis == Some(0)
            || (self.ownership == StructureOwnership::Canonical
                && self.eviction != KeyspaceEvictionPolicy::None)
            || (self.memory_class == KeyspaceMemoryClass::Durable
                && self.ownership != StructureOwnership::Canonical)
            || (self.memory_class == KeyspaceMemoryClass::Cache
                && self.ownership != StructureOwnership::Cache)
        {
            return Err(CatalogError::InvalidKeyspacePolicy);
        }
        Ok(())
    }
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

/// Reusable analyzer tokenization strategy.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum AnalyzerTokenizer {
    /// Unicode word-boundary tokenizer.
    UnicodeWord = 1,
    /// Whitespace-delimited tokenizer.
    Whitespace = 2,
    /// Treat the complete input as one keyword.
    Keyword = 3,
}

/// One analyzer token-filter stage.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum AnalyzerFilter {
    /// Unicode lowercase normalization.
    Lowercase = 1,
    /// ASCII-compatible accent folding.
    AsciiFolding = 2,
    /// English stop-word removal, version one.
    EnglishStopV1 = 3,
    /// English stemming, version one.
    EnglishStemV1 = 4,
}

/// First-class reusable V2 lexical analyzer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzerDefinition {
    /// Shared V2 metadata.
    pub header: ObjectHeaderV2,
    /// Tokenization strategy.
    pub tokenizer: AnalyzerTokenizer,
    /// Ordered token-filter pipeline.
    pub filters: Vec<AnalyzerFilter>,
}

impl AnalyzerDefinition {
    /// Validates a bounded filter pipeline with no repeated stages.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate or excessive filters.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.header.owner != EngineKind::Search {
            return Err(CatalogError::WrongObjectOwner);
        }
        if self.filters.len() > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        let mut filters = BTreeSet::new();
        if self.filters.iter().any(|filter| !filters.insert(*filter)) {
            return Err(CatalogError::DuplicateAnalyzerFilter);
        }
        Ok(())
    }
}

/// Search field source retention policy.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum FieldSourcePolicy {
    /// Exclude this field from retained source.
    Excluded = 1,
    /// Retain the canonical source representation.
    Retained = 2,
}

/// Lexical indexing policy for one field.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LexicalIndexPolicy {
    /// Do not build a lexical index for this field.
    None = 1,
    /// Index analyzed terms and frequencies.
    Frequencies = 2,
    /// Index terms, frequencies, and positions.
    Positions = 3,
}

/// Complete V2 field storage and indexing options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SearchFieldOptions {
    /// Whether the original field value is independently retrievable.
    pub stored: bool,
    /// Whether typed filtering, sorting, facets, and metrics are materialized.
    pub doc_values: bool,
    /// Whether canonical source retains this field.
    pub source: FieldSourcePolicy,
    /// Lexical index detail.
    pub lexical: LexicalIndexPolicy,
}

/// One V2 search field mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchFieldDefinitionV2 {
    /// Stable field identity.
    pub id: FieldId,
    /// Display and lookup name.
    pub name: CatalogName,
    /// Source logical type.
    pub logical_type: LogicalType,
    /// Analyzer dependency for lexical indexing.
    pub analyzer: Option<ObjectId>,
    /// Storage and index options.
    pub options: SearchFieldOptions,
}

/// Selection policy for exact and approximate vector execution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VectorSearchPolicy {
    /// Always evaluate exact distance over admitted candidates.
    Exact,
    /// Always use the named ANN definition.
    Ann(AnnIndexDefinition),
    /// Select exact below the candidate threshold and ANN otherwise.
    Adaptive {
        /// Maximum admitted exact candidate count.
        exact_candidate_threshold: u32,
        /// ANN definition used above the threshold.
        ann: AnnIndexDefinition,
    },
}

/// Incremental ANN lifecycle configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IncrementalVectorLifecycle {
    /// Maximum pending mutations before consolidation is required.
    pub delta_max_entries: u32,
    /// Number of immutable deltas that triggers consolidation.
    pub consolidate_after_deltas: u16,
    /// Maximum obsolete generations retained before reclamation.
    pub retain_generations: u16,
}

impl IncrementalVectorLifecycle {
    /// Validates bounded lifecycle controls.
    ///
    /// # Errors
    ///
    /// Returns an error for zero, inverted, or above-format bounds.
    pub fn validate(self) -> Result<(), CatalogError> {
        if self.delta_max_entries == 0
            || self.delta_max_entries > MAX_INCREMENTAL_VECTOR_DELTA_ENTRIES
            || self.consolidate_after_deltas == 0
            || u32::from(self.consolidate_after_deltas) > self.delta_max_entries
            || self.retain_generations == 0
            || self.retain_generations > MAX_INCREMENTAL_VECTOR_RETAINED_GENERATIONS
        {
            Err(CatalogError::InvalidVectorPolicy)
        } else {
            Ok(())
        }
    }
}

/// One named vector field owned by a V2 search collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedVectorDefinition {
    /// Stable field identity for vector mutation and binding.
    pub id: FieldId,
    /// Display and lookup name.
    pub name: CatalogName,
    /// Fixed element and dimension.
    pub vector_type: VectorType,
    /// Fixed distance metric.
    pub metric: VectorMetric,
    /// Exact, ANN, or adaptive execution policy.
    pub policy: VectorSearchPolicy,
    /// Incremental delta/consolidation/reclamation policy.
    pub lifecycle: IncrementalVectorLifecycle,
}

/// Native V2 search collection with complete field policy and named vectors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCollectionDefinitionV2 {
    /// Shared V2 metadata.
    pub header: ObjectHeaderV2,
    /// Fields ordered by stable identity.
    pub fields: Vec<SearchFieldDefinitionV2>,
    /// Named vectors ordered by stable identity.
    pub vectors: Vec<NamedVectorDefinition>,
}

/// Vector distance fixed by one catalog search definition.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum VectorMetric {
    /// One minus cosine similarity.
    Cosine = 1,
    /// Negated dot product.
    NegativeDot = 2,
    /// Squared Euclidean distance.
    SquaredL2 = 3,
}

/// Versioned HNSW construction and query configuration stored in the catalog.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnnIndexDefinition {
    metric: VectorMetric,
    m: u16,
    ef_construction: u16,
    ef_search_default: u16,
    ef_search_max: u16,
    seed: u64,
}

impl AnnIndexDefinition {
    /// Constructs a checked catalog ANN definition.
    ///
    /// # Errors
    ///
    /// Returns an error unless `M` is in 2 through 64, construction breadth
    /// is at least `M`, and default query breadth is nonzero and no larger
    /// than its configured maximum.
    pub fn new(
        metric: VectorMetric,
        m: u16,
        ef_construction: u16,
        ef_search_default: u16,
        ef_search_max: u16,
        seed: u64,
    ) -> Result<Self, CatalogError> {
        if !(2..=64).contains(&m) {
            return Err(CatalogError::InvalidAnnM);
        }
        if ef_construction < m {
            return Err(CatalogError::InvalidAnnEfConstruction);
        }
        if ef_search_default == 0 || ef_search_default > ef_search_max {
            return Err(CatalogError::InvalidAnnEfSearch);
        }
        Ok(Self {
            metric,
            m,
            ef_construction,
            ef_search_default,
            ef_search_max,
            seed,
        })
    }

    /// Fixed vector distance.
    pub const fn metric(self) -> VectorMetric {
        self.metric
    }

    /// Maximum retained neighbors per node and layer.
    pub const fn m(self) -> u16 {
        self.m
    }

    /// Candidate breadth used during deterministic construction.
    pub const fn ef_construction(self) -> u16 {
        self.ef_construction
    }

    /// Default query breadth.
    pub const fn ef_search_default(self) -> u16 {
        self.ef_search_default
    }

    /// Maximum admitted query breadth.
    pub const fn ef_search_max(self) -> u16 {
        self.ef_search_max
    }

    /// Definition-pinned deterministic seed.
    pub const fn seed(self) -> u64 {
        self.seed
    }
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
    /// Optional versioned approximate-nearest-neighbor configuration.
    pub ann: Option<AnnIndexDefinition>,
}

/// One stable member-identity correspondence in a cross-engine link.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CrossEngineLinkMapping {
    /// Stable member identity in the source object.
    pub source: u32,
    /// Stable member identity in the target object.
    pub target: u32,
}

/// How a cross-engine link is maintained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CrossEngineLinkMaintenance {
    /// The application maintains both endpoints explicitly.
    Manual = 1,
    /// Hyphae derives target changes from source changes.
    Derived = 2,
}

/// Behavior when a linked source value is deleted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CrossEngineLinkDeleteBehavior {
    /// Reject deletion while a linked target value exists.
    Restrict = 1,
    /// Delete the linked target value.
    Cascade = 2,
    /// Leave the target value unchanged.
    Retain = 3,
}

/// Explicit stable-ID link between objects owned by different native engines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossEngineLinkDefinition {
    /// Shared object metadata; links are owned by the kernel.
    pub header: ObjectHeader,
    /// Stable source object identity.
    pub source: ObjectId,
    /// Stable target object identity.
    pub target: ObjectId,
    /// Canonically ordered stable member-ID mappings.
    pub mapping: Vec<CrossEngineLinkMapping>,
    /// Maintenance policy.
    pub maintenance: CrossEngineLinkMaintenance,
    /// Source-delete policy.
    pub delete_behavior: CrossEngineLinkDeleteBehavior,
    /// Whether derived updates commit in the originating transaction.
    pub synchronous: bool,
}

impl CrossEngineLinkDefinition {
    /// Validates endpoint and bounded canonical mapping invariants.
    ///
    /// # Errors
    ///
    /// Returns an error for self-links, zero or noncanonical mappings, or an
    /// excessive mapping count.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.source == self.target
            || self.header.id == self.source
            || self.header.id == self.target
            || self.mapping.is_empty()
        {
            return Err(CatalogError::InvalidCrossEngineLink);
        }
        if self.mapping.len() > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        let mut previous = None;
        for mapping in &self.mapping {
            if mapping.source == 0
                || mapping.target == 0
                || previous.is_some_and(|value| value >= *mapping)
            {
                return Err(CatalogError::InvalidCrossEngineLink);
            }
            previous = Some(*mapping);
        }
        Ok(())
    }
}

impl SearchCollectionDefinition {
    /// Validates one search definition.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate field identities.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.ann.is_some() && self.vector.is_none() {
            return Err(CatalogError::AnnRequiresVector);
        }
        if self.fields.len() > MAX_CATALOG_DEFINITION_ITEMS {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        let mut field_ids = BTreeSet::new();
        let mut field_names = BTreeSet::new();
        let mut previous_id = None;
        for field in &self.fields {
            if previous_id == Some(field.id) {
                return Err(CatalogError::DuplicateFieldId(field.id));
            }
            if previous_id.is_some_and(|previous| previous > field.id) {
                return Err(CatalogError::NoncanonicalFieldOrder);
            }
            previous_id = Some(field.id);
            if !field_ids.insert(field.id) {
                return Err(CatalogError::DuplicateFieldId(field.id));
            }
            if !field_names.insert(field.name.lookup()) {
                return Err(CatalogError::DuplicateFieldName(Box::new(
                    field.name.clone(),
                )));
            }
        }
        Ok(())
    }
}

impl SearchCollectionDefinitionV2 {
    /// Validates canonical field/vector order, names, and policy combinations.
    ///
    /// # Errors
    ///
    /// Returns an error for wrong ownership, duplicates, noncanonical order,
    /// invalid analyzers, or invalid vector lifecycle settings.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.header.owner != EngineKind::Search {
            return Err(CatalogError::WrongObjectOwner);
        }
        if self.fields.len() > MAX_CATALOG_DEFINITION_ITEMS
            || self.vectors.len() > MAX_CATALOG_DEFINITION_ITEMS
        {
            return Err(CatalogError::TooManyDefinitionItems);
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut previous_id = None;
        for field in &self.fields {
            if previous_id == Some(field.id) {
                return Err(CatalogError::DuplicateFieldId(field.id));
            }
            if previous_id.is_some_and(|previous| previous > field.id) {
                return Err(CatalogError::NoncanonicalFieldOrder);
            }
            previous_id = Some(field.id);
            if !ids.insert(field.id) {
                return Err(CatalogError::DuplicateFieldId(field.id));
            }
            if !names.insert(field.name.lookup()) {
                return Err(CatalogError::DuplicateFieldName(Box::new(
                    field.name.clone(),
                )));
            }
            if field.options.lexical == LexicalIndexPolicy::None && field.analyzer.is_some() {
                return Err(CatalogError::InvalidSearchFieldPolicy);
            }
            if field.options.lexical != LexicalIndexPolicy::None && field.analyzer.is_none() {
                return Err(CatalogError::InvalidSearchFieldPolicy);
            }
            if !field.options.stored
                && !field.options.doc_values
                && field.options.source == FieldSourcePolicy::Excluded
                && field.options.lexical == LexicalIndexPolicy::None
            {
                return Err(CatalogError::InvalidSearchFieldPolicy);
            }
        }

        previous_id = None;
        for vector in &self.vectors {
            if previous_id == Some(vector.id) || ids.contains(&vector.id) {
                return Err(CatalogError::DuplicateVectorId(vector.id));
            }
            if previous_id.is_some_and(|previous| previous > vector.id) {
                return Err(CatalogError::NoncanonicalFieldOrder);
            }
            previous_id = Some(vector.id);
            ids.insert(vector.id);
            if !names.insert(vector.name.lookup()) {
                return Err(CatalogError::DuplicateVectorName(Box::new(
                    vector.name.clone(),
                )));
            }
            let policy_metric = match vector.policy {
                VectorSearchPolicy::Exact => None,
                VectorSearchPolicy::Ann(ann) => Some(ann.metric()),
                VectorSearchPolicy::Adaptive {
                    exact_candidate_threshold,
                    ann,
                } => {
                    if exact_candidate_threshold == 0 {
                        return Err(CatalogError::InvalidVectorPolicy);
                    }
                    Some(ann.metric())
                }
            };
            if policy_metric.is_some_and(|metric| metric != vector.metric)
                || vector.lifecycle.delta_max_entries == 0
                || vector.lifecycle.consolidate_after_deltas == 0
                || vector.lifecycle.retain_generations == 0
            {
                return Err(CatalogError::InvalidVectorPolicy);
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
    /// Relational secondary index.
    SecondaryIndex(SecondaryIndexDefinition),
    /// Keyspace structure.
    Structure(StructureDefinition),
    /// Search collection.
    Search(SearchCollectionDefinition),
    /// Explicit cross-engine stable-ID link.
    CrossEngineLink(CrossEngineLinkDefinition),
}

impl CatalogObject {
    /// Returns common object metadata.
    pub const fn header(&self) -> &ObjectHeader {
        match self {
            Self::Relation(definition) => &definition.header,
            Self::SecondaryIndex(definition) => &definition.header,
            Self::Structure(definition) => &definition.header,
            Self::Search(definition) => &definition.header,
            Self::CrossEngineLink(definition) => &definition.header,
        }
    }

    /// Returns the stable logical object family.
    pub const fn kind(&self) -> CatalogObjectKind {
        match self {
            Self::Relation(_) => CatalogObjectKind::Relation,
            Self::SecondaryIndex(_) => CatalogObjectKind::SecondaryIndex,
            Self::Structure(_) => CatalogObjectKind::Structure,
            Self::Search(_) => CatalogObjectKind::SearchCollection,
            Self::CrossEngineLink(_) => CatalogObjectKind::CrossEngineLink,
        }
    }

    /// Derives stable object dependencies declared by this V1-compatible object.
    pub fn dependencies(&self) -> Vec<DependencyEdge> {
        let dependent = self.header().id;
        let mut edges = BTreeSet::new();
        match self {
            Self::Relation(definition) => {
                for foreign_key in &definition.foreign_keys {
                    if foreign_key.referenced_relation != dependent {
                        edges.insert(DependencyEdge::new(
                            dependent,
                            foreign_key.referenced_relation,
                            DependencyKind::ForeignKey,
                        ));
                    }
                    if let Some(index) = foreign_key.referenced_index {
                        edges.insert(DependencyEdge::new(
                            dependent,
                            index,
                            DependencyKind::ForeignKey,
                        ));
                    }
                }
            }
            Self::SecondaryIndex(definition) => {
                edges.insert(DependencyEdge::new(
                    dependent,
                    definition.relation,
                    DependencyKind::SecondaryIndexRelation,
                ));
            }
            Self::Search(definition) => {
                for analyzer in definition.fields.iter().filter_map(|field| field.analyzer) {
                    edges.insert(DependencyEdge::new(
                        dependent,
                        analyzer,
                        DependencyKind::Analyzer,
                    ));
                }
            }
            Self::CrossEngineLink(definition) => {
                edges.insert(DependencyEdge::new(
                    dependent,
                    definition.source,
                    DependencyKind::LinkEndpoint,
                ));
                edges.insert(DependencyEdge::new(
                    dependent,
                    definition.target,
                    DependencyKind::LinkEndpoint,
                ));
            }
            Self::Structure(_) => {}
        }
        edges.into_iter().collect()
    }

    /// Validates the object kind, owner, names, identities, and definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the object violates a canonical catalog
    /// invariant.
    pub fn validate(&self) -> Result<(), CatalogError> {
        match self {
            Self::Relation(definition) => {
                if definition.header.owner != EngineKind::Relational {
                    return Err(CatalogError::WrongObjectOwner);
                }
                definition.validate()
            }
            Self::SecondaryIndex(definition) => {
                if definition.header.owner != EngineKind::Relational {
                    return Err(CatalogError::WrongObjectOwner);
                }
                definition.validate()
            }
            Self::Structure(definition) => {
                if definition.header.owner != EngineKind::Structure {
                    return Err(CatalogError::WrongObjectOwner);
                }
                Ok(())
            }
            Self::Search(definition) => {
                if definition.header.owner != EngineKind::Search {
                    return Err(CatalogError::WrongObjectOwner);
                }
                definition.validate()
            }
            Self::CrossEngineLink(definition) => {
                if definition.header.owner != EngineKind::Kernel {
                    return Err(CatalogError::WrongObjectOwner);
                }
                definition.validate()
            }
        }
    }
}

/// V2-only logical catalog object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogObjectV2 {
    /// Top-level database namespace.
    Database(ObjectHeaderV2),
    /// Schema namespace owned by a database.
    Schema(ObjectHeaderV2),
    /// First-class structure keyspace.
    Keyspace(KeyspaceDefinition),
    /// Reusable lexical analyzer.
    Analyzer(AnalyzerDefinition),
    /// Search collection with complete field and vector policy.
    SearchCollection(SearchCollectionDefinitionV2),
}

/// V1-compatible object promoted into the logical V2 hierarchy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibleCatalogObjectV2 {
    /// Existing object definition, preserving all V1 constructors and fields.
    pub object: CatalogObject,
    /// Stable hierarchy parent.
    pub parent: ObjectId,
    /// Monotonic logical definition revision.
    pub definition_version: DefinitionVersion,
}

/// Complete logical object admitted by the `HYCOBJ02` codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalCatalogObject {
    /// Existing relation/index/structure/search/link represented without loss.
    Compatible(CompatibleCatalogObjectV2),
    /// V2-native namespace, keyspace, analyzer, or search definition.
    V2(CatalogObjectV2),
}

impl LogicalCatalogObject {
    /// Promotes one legacy object into the logical V2 view.
    ///
    /// Legacy definitions do not carry an explicit hierarchy parent. Their
    /// stable object identity is used as the compatibility parent marker and
    /// is never emitted by the canonical `HYCOBJ02` encoder.
    pub fn from_legacy(object: CatalogObject) -> Self {
        let parent = object.header().id;
        Self::Compatible(CompatibleCatalogObjectV2 {
            object,
            parent,
            definition_version: DefinitionVersion::FIRST,
        })
    }

    /// Returns the stable object identity.
    pub const fn id(&self) -> ObjectId {
        match self {
            Self::Compatible(definition) => definition.object.header().id,
            Self::V2(definition) => definition.header().id,
        }
    }

    /// Returns the owning engine.
    pub const fn owner(&self) -> EngineKind {
        match self {
            Self::Compatible(definition) => definition.object.header().owner,
            Self::V2(definition) => definition.header().owner,
        }
    }

    /// Returns the qualified display and lookup name.
    pub const fn name(&self) -> &QualifiedName {
        match self {
            Self::Compatible(definition) => &definition.object.header().name,
            Self::V2(definition) => &definition.header().name,
        }
    }

    /// Returns the stable logical object family.
    pub const fn kind(&self) -> CatalogObjectKind {
        match self {
            Self::Compatible(definition) => definition.object.kind(),
            Self::V2(definition) => definition.kind(),
        }
    }

    /// Returns the hierarchy parent, absent only for a database.
    pub const fn parent(&self) -> Option<ObjectId> {
        match self {
            Self::Compatible(definition) => {
                if definition.parent.get() == definition.object.header().id.get() {
                    None
                } else {
                    Some(definition.parent)
                }
            }
            Self::V2(definition) => definition.header().parent,
        }
    }

    /// Returns the stable logical definition revision.
    pub const fn definition_version(&self) -> DefinitionVersion {
        match self {
            Self::Compatible(definition) => definition.definition_version,
            Self::V2(definition) => definition.header().definition_version,
        }
    }

    /// Validates the complete logical object.
    ///
    /// # Errors
    ///
    /// Returns an error for a self-parent or invalid contained definition.
    pub fn validate(&self) -> Result<(), CatalogError> {
        match self {
            Self::Compatible(definition) => definition.object.validate(),
            Self::V2(definition) => definition.validate(),
        }
    }

    /// Derives parent and definition-declared dependency edges.
    pub fn dependencies(&self) -> Vec<DependencyEdge> {
        let mut edges = BTreeSet::new();
        if let Some(parent) = self.parent() {
            edges.insert(DependencyEdge::new(
                self.id(),
                parent,
                DependencyKind::Parent,
            ));
        }
        match self {
            Self::Compatible(definition) => edges.extend(definition.object.dependencies()),
            Self::V2(definition) => edges.extend(definition.dependencies()),
        }
        edges.into_iter().collect()
    }
}

impl CatalogObjectV2 {
    /// Returns common V2 metadata.
    pub const fn header(&self) -> &ObjectHeaderV2 {
        match self {
            Self::Database(header) | Self::Schema(header) => header,
            Self::Keyspace(definition) => &definition.header,
            Self::Analyzer(definition) => &definition.header,
            Self::SearchCollection(definition) => &definition.header,
        }
    }

    /// Returns the stable logical object family.
    pub const fn kind(&self) -> CatalogObjectKind {
        match self {
            Self::Database(_) => CatalogObjectKind::Database,
            Self::Schema(_) => CatalogObjectKind::Schema,
            Self::Keyspace(_) => CatalogObjectKind::Keyspace,
            Self::Analyzer(_) => CatalogObjectKind::Analyzer,
            Self::SearchCollection(_) => CatalogObjectKind::SearchCollection,
        }
    }

    /// Validates owner, hierarchy shape, and definition-local policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the logical definition is noncanonical.
    pub fn validate(&self) -> Result<(), CatalogError> {
        let header = self.header();
        match self {
            Self::Database(_) => {
                if header.owner != EngineKind::Kernel {
                    return Err(CatalogError::WrongObjectOwner);
                }
                if header.parent.is_some() {
                    return Err(CatalogError::InvalidObjectHierarchy);
                }
                Ok(())
            }
            Self::Schema(_) => {
                if header.owner != EngineKind::Kernel {
                    return Err(CatalogError::WrongObjectOwner);
                }
                validate_parent(header)
            }
            Self::Keyspace(definition) => {
                validate_parent(header)?;
                definition.validate()
            }
            Self::Analyzer(definition) => {
                validate_parent(header)?;
                definition.validate()
            }
            Self::SearchCollection(definition) => {
                validate_parent(header)?;
                definition.validate()
            }
        }
    }

    /// Derives parent and referenced-object dependency edges.
    pub fn dependencies(&self) -> Vec<DependencyEdge> {
        let dependent = self.header().id;
        let mut edges = BTreeSet::new();
        if let Some(parent) = self.header().parent {
            edges.insert(DependencyEdge::new(
                dependent,
                parent,
                DependencyKind::Parent,
            ));
        }
        match self {
            Self::Keyspace(definition) => {
                if let Some(relation) = definition.relation_schema {
                    edges.insert(DependencyEdge::new(
                        dependent,
                        relation,
                        DependencyKind::RelationSchema,
                    ));
                }
            }
            Self::SearchCollection(definition) => {
                for analyzer in definition.fields.iter().filter_map(|field| field.analyzer) {
                    edges.insert(DependencyEdge::new(
                        dependent,
                        analyzer,
                        DependencyKind::Analyzer,
                    ));
                }
            }
            Self::Database(_) | Self::Schema(_) | Self::Analyzer(_) => {}
        }
        edges.into_iter().collect()
    }
}

fn validate_parent(header: &ObjectHeaderV2) -> Result<(), CatalogError> {
    match header.parent {
        Some(parent) if parent != header.id => Ok(()),
        Some(_) | None => Err(CatalogError::InvalidObjectHierarchy),
    }
}

/// Semantic reason for one derived catalog dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum DependencyKind {
    /// Namespace parent.
    Parent = 1,
    /// Secondary index owning relation.
    SecondaryIndexRelation = 2,
    /// Foreign-key target relation or unique index.
    ForeignKey = 3,
    /// Search field analyzer.
    Analyzer = 4,
    /// Cross-engine link endpoint.
    LinkEndpoint = 5,
    /// Keyspace relation-valued schema.
    RelationSchema = 6,
}

/// Canonical directed edge from a dependent to its prerequisite.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DependencyEdge {
    /// Object that declares the dependency.
    pub dependent: ObjectId,
    /// Object required by the dependent.
    pub prerequisite: ObjectId,
    /// Semantic edge class.
    pub kind: DependencyKind,
}

impl DependencyEdge {
    /// Constructs one directed dependency edge.
    pub const fn new(dependent: ObjectId, prerequisite: ObjectId, kind: DependencyKind) -> Self {
        Self {
            dependent,
            prerequisite,
            kind,
        }
    }
}

/// Direction used to query derived dependency edges.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DependencyDirection {
    /// Edges declared by the object.
    Outgoing,
    /// Edges whose dependents require the object.
    Incoming,
}

/// Canonically derives and validates dependency edges for one complete object set.
///
/// # Errors
///
/// Returns an error when an edge is self-referential or names an absent target.
pub fn derive_dependency_edges<'a>(
    objects: impl IntoIterator<Item = &'a CatalogObject>,
) -> Result<Vec<DependencyEdge>, CatalogError> {
    let objects: Vec<_> = objects.into_iter().collect();
    let ids: BTreeSet<_> = objects.iter().map(|object| object.header().id).collect();
    let mut edges = BTreeSet::new();
    for edge in objects.iter().flat_map(|object| object.dependencies()) {
        if edge.dependent == edge.prerequisite {
            return Err(CatalogError::InvalidObjectHierarchy);
        }
        if !ids.contains(&edge.prerequisite) {
            return Err(CatalogError::MissingDependencyTarget(edge.prerequisite));
        }
        edges.insert(edge);
    }
    Ok(edges.into_iter().collect())
}

/// Returns all edges touching one object in a requested direction.
pub fn dependency_edges_for(
    edges: &[DependencyEdge],
    object: ObjectId,
    direction: DependencyDirection,
) -> Vec<DependencyEdge> {
    edges
        .iter()
        .copied()
        .filter(|edge| match direction {
            DependencyDirection::Outgoing => edge.dependent == object,
            DependencyDirection::Incoming => edge.prerequisite == object,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Canonically derives and validates dependencies for logical V2 objects.
///
/// This validates database/schema hierarchy kinds in addition to requiring
/// every referenced target to exist.
///
/// # Errors
///
/// Returns an error for absent targets, self-edges, or invalid parent kinds.
pub fn derive_logical_dependency_edges<'a>(
    objects: impl IntoIterator<Item = &'a LogicalCatalogObject>,
) -> Result<Vec<DependencyEdge>, CatalogError> {
    let objects: Vec<_> = objects.into_iter().collect();
    let kinds: BTreeMap<_, _> = objects
        .iter()
        .map(|object| (object.id(), object.kind()))
        .collect();
    if kinds.len() != objects.len() {
        return Err(CatalogError::InvalidObjectHierarchy);
    }
    let mut edges = BTreeSet::new();
    for object in objects {
        object.validate()?;
        for edge in object.dependencies() {
            if edge.dependent == edge.prerequisite {
                return Err(CatalogError::InvalidObjectHierarchy);
            }
            let target_kind = kinds
                .get(&edge.prerequisite)
                .copied()
                .ok_or(CatalogError::MissingDependencyTarget(edge.prerequisite))?;
            let valid_target = match edge.kind {
                DependencyKind::Parent => match object.kind() {
                    CatalogObjectKind::Database => false,
                    CatalogObjectKind::Schema => target_kind == CatalogObjectKind::Database,
                    _ => target_kind == CatalogObjectKind::Schema,
                },
                DependencyKind::SecondaryIndexRelation | DependencyKind::RelationSchema => {
                    target_kind == CatalogObjectKind::Relation
                }
                DependencyKind::ForeignKey => matches!(
                    target_kind,
                    CatalogObjectKind::Relation | CatalogObjectKind::SecondaryIndex
                ),
                DependencyKind::Analyzer => target_kind == CatalogObjectKind::Analyzer,
                DependencyKind::LinkEndpoint => !matches!(
                    target_kind,
                    CatalogObjectKind::Database
                        | CatalogObjectKind::Schema
                        | CatalogObjectKind::CrossEngineLink
                ),
            };
            if !valid_target {
                return Err(CatalogError::InvalidObjectHierarchy);
            }
            edges.insert(edge);
        }
    }
    Ok(edges.into_iter().collect())
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
    removals: BTreeSet<ObjectId>,
}

impl CatalogTransaction {
    /// Begins from one immutable base snapshot.
    pub fn begin(base: Arc<CatalogSnapshot>) -> Self {
        Self {
            base,
            additions: Vec::new(),
            removals: BTreeSet::new(),
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
        if let CatalogObject::Relation(definition) = &object {
            for foreign_key in &definition.foreign_keys {
                let parent = if foreign_key.referenced_relation == definition.header.id {
                    Some(definition)
                } else {
                    self.additions
                        .iter()
                        .find(|existing| existing.header().id == foreign_key.referenced_relation)
                        .or_else(|| self.base.objects.get(&foreign_key.referenced_relation))
                        .and_then(|object| match object {
                            CatalogObject::Relation(parent) => Some(parent),
                            _ => None,
                        })
                };
                let Some(parent) = parent else {
                    return Err(CatalogError::InvalidDefinitionEncoding);
                };
                foreign_key.validate_relations(definition, parent)?;
            }
        }
        if let CatalogObject::SecondaryIndex(definition) = &object {
            let relation = self
                .additions
                .iter()
                .find(|existing| existing.header().id == definition.relation)
                .or_else(|| self.base.objects.get(&definition.relation));
            let Some(CatalogObject::Relation(relation)) = relation else {
                return Err(CatalogError::MissingSecondaryIndexRelation(
                    definition.relation,
                ));
            };
            definition.validate_relation(relation)?;
        }
        if let CatalogObject::CrossEngineLink(definition) = &object {
            let source = self
                .additions
                .iter()
                .find(|existing| existing.header().id == definition.source)
                .or_else(|| self.base.objects.get(&definition.source));
            let target = self
                .additions
                .iter()
                .find(|existing| existing.header().id == definition.target)
                .or_else(|| self.base.objects.get(&definition.target));
            let (Some(source), Some(target)) = (source, target) else {
                return Err(CatalogError::InvalidCrossEngineLink);
            };
            if source.header().owner == target.header().owner {
                return Err(CatalogError::InvalidCrossEngineLink);
            }
        }
        self.additions.push(object);
        Ok(())
    }

    /// Stages strict object removal with dependency checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the object is absent or still owns a dependent
    /// secondary index or foreign-key edge.
    pub fn remove(&mut self, id: ObjectId) -> Result<(), CatalogError> {
        self.additions
            .iter()
            .find(|object| object.header().id == id)
            .or_else(|| self.base.objects.get(&id))
            .ok_or(CatalogError::InvalidDefinitionEncoding)?;
        if self
            .additions
            .iter()
            .chain(self.base.objects.values())
            .filter(|candidate| !self.removals.contains(&candidate.header().id))
            .any(|candidate| match candidate {
                CatalogObject::SecondaryIndex(index) => index.relation == id,
                CatalogObject::Relation(relation) => relation
                    .foreign_keys
                    .iter()
                    .any(|foreign_key| foreign_key.referenced_relation == id),
                CatalogObject::CrossEngineLink(link) => link.source == id || link.target == id,
                CatalogObject::Structure(_) | CatalogObject::Search(_) => false,
            })
        {
            return Err(CatalogError::InvalidDefinitionEncoding);
        }
        self.removals.insert(id);
        Ok(())
    }

    /// Commits a new immutable in-memory catalog snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when catalog version space is exhausted.
    pub fn commit(mut self) -> Result<Arc<CatalogSnapshot>, CatalogError> {
        let version = self
            .base
            .version
            .checked_next()
            .ok_or(CatalogError::VersionExhausted)?;
        let mut objects = self.base.objects.clone();
        let mut names = self.base.names.clone();
        for id in self.removals {
            if let Some(object) = objects.remove(&id) {
                names.remove(&object.header().name);
            } else if let Some(index) = self
                .additions
                .iter()
                .position(|object| object.header().id == id)
            {
                self.additions.remove(index);
            }
        }
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
        CatalogVersion, ColumnId, EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId,
    };

    use super::{
        AnalyzerDefinition, AnalyzerTokenizer, CatalogError, CatalogName, CatalogObject,
        CatalogObjectV2, CatalogSnapshot, CatalogTransaction, ColumnDefinition,
        CompatibleCatalogObjectV2, DefinitionVersion, DependencyDirection, DependencyKind,
        FieldSourcePolicy, LexicalIndexPolicy, LogicalCatalogObject, ObjectHeader, ObjectHeaderV2,
        QualifiedName, RelationDefinition, SearchCollectionDefinitionV2, SearchFieldDefinitionV2,
        SearchFieldOptions, SecondaryIndexDefinition, dependency_edges_for,
        derive_logical_dependency_edges,
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
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }))
    }

    fn secondary_index(
        id: u128,
        relation: u128,
        column: u32,
    ) -> Result<CatalogObject, Box<dyn std::error::Error>> {
        Ok(CatalogObject::SecondaryIndex(SecondaryIndexDefinition {
            header: ObjectHeader {
                id: ObjectId::new(id)?,
                owner: EngineKind::Relational,
                name: QualifiedName::new(
                    CatalogName::unquoted("main")?,
                    CatalogName::unquoted("public")?,
                    CatalogName::unquoted("accounts_by_id")?,
                ),
            },
            relation: ObjectId::new(relation)?,
            columns: vec![ColumnId::new(column)?],
            unique: true,
            nulls_distinct: true,
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
    fn remove_rejects_unknown_and_dependent_objects() -> Result<(), Box<dyn std::error::Error>> {
        let base = CatalogSnapshot::empty(CatalogVersion::new(1)?);
        let mut transaction = CatalogTransaction::begin(base);
        transaction.create(relation(1, "accounts")?)?;
        transaction.create(secondary_index(2, 1, 1)?)?;
        assert!(transaction.remove(ObjectId::new(1)?).is_err());
        transaction.remove(ObjectId::new(2)?)?;
        transaction.remove(ObjectId::new(1)?)?;
        assert!(transaction.remove(ObjectId::new(99)?).is_err());
        let committed = transaction.commit()?;
        assert!(committed.is_empty());
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

    #[test]
    fn secondary_index_must_reference_a_staged_or_committed_relation()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = CatalogSnapshot::empty(CatalogVersion::new(1)?);
        let mut transaction = CatalogTransaction::begin(base);
        assert_eq!(
            transaction.create(secondary_index(2, 1, 1)?),
            Err(CatalogError::MissingSecondaryIndexRelation(ObjectId::new(
                1
            )?))
        );
        transaction.create(relation(1, "accounts")?)?;
        assert_eq!(
            transaction.create(secondary_index(2, 1, 2)?),
            Err(CatalogError::MissingSecondaryIndexColumn(ColumnId::new(2)?))
        );
        transaction.create(secondary_index(2, 1, 1)?)?;
        let committed = transaction.commit()?;
        assert_eq!(committed.len(), 2);
        Ok(())
    }

    #[test]
    fn logical_dependencies_are_derived_in_both_directions()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = LogicalCatalogObject::V2(CatalogObjectV2::Database(ObjectHeaderV2 {
            id: ObjectId::new(10)?,
            owner: EngineKind::Kernel,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted("database")?,
            ),
            parent: None,
            definition_version: DefinitionVersion::FIRST,
        }));
        let schema = LogicalCatalogObject::V2(CatalogObjectV2::Schema(ObjectHeaderV2 {
            id: ObjectId::new(11)?,
            owner: EngineKind::Kernel,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted("schema")?,
            ),
            parent: Some(ObjectId::new(10)?),
            definition_version: DefinitionVersion::FIRST,
        }));
        let analyzer = LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: ObjectHeaderV2 {
                id: ObjectId::new(12)?,
                owner: EngineKind::Search,
                name: QualifiedName::new(
                    CatalogName::unquoted("main")?,
                    CatalogName::unquoted("public")?,
                    CatalogName::unquoted("plain")?,
                ),
                parent: Some(ObjectId::new(11)?),
                definition_version: DefinitionVersion::FIRST,
            },
            tokenizer: AnalyzerTokenizer::Whitespace,
            filters: Vec::new(),
        }));
        let search = LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                header: ObjectHeaderV2 {
                    id: ObjectId::new(13)?,
                    owner: EngineKind::Search,
                    name: QualifiedName::new(
                        CatalogName::unquoted("main")?,
                        CatalogName::unquoted("public")?,
                        CatalogName::unquoted("documents")?,
                    ),
                    parent: Some(ObjectId::new(11)?),
                    definition_version: DefinitionVersion::FIRST,
                },
                fields: vec![SearchFieldDefinitionV2 {
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
                }],
                vectors: Vec::new(),
            },
        ));
        let objects = [&database, &schema, &analyzer, &search];
        let edges = derive_logical_dependency_edges(objects)?;
        let outgoing =
            dependency_edges_for(&edges, ObjectId::new(13)?, DependencyDirection::Outgoing);
        let incoming =
            dependency_edges_for(&edges, ObjectId::new(12)?, DependencyDirection::Incoming);
        assert_eq!(outgoing.len(), 2);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].kind, DependencyKind::Analyzer);
        Ok(())
    }

    #[test]
    fn logical_dependency_derivation_rejects_missing_and_wrong_parents()
    -> Result<(), Box<dyn std::error::Error>> {
        let relation = LogicalCatalogObject::Compatible(CompatibleCatalogObjectV2 {
            object: relation(1, "accounts")?,
            parent: ObjectId::new(99)?,
            definition_version: DefinitionVersion::FIRST,
        });
        assert_eq!(
            derive_logical_dependency_edges([&relation]),
            Err(CatalogError::MissingDependencyTarget(ObjectId::new(99)?))
        );
        Ok(())
    }

    #[test]
    fn legacy_promotion_is_deterministic_for_full_width_object_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        let id = u128::MAX - 1;
        let legacy = relation(id, "high_id_relation")?;
        let promoted = LogicalCatalogObject::from_legacy(legacy.clone());
        assert_eq!(promoted.id(), ObjectId::new(id)?);
        assert_eq!(promoted.parent(), None);
        assert_eq!(promoted.definition_version(), DefinitionVersion::FIRST);
        let encoded = promoted.encode_definition_v2()?;
        assert_eq!(
            LogicalCatalogObject::decode_definition_v2(&encoded)?,
            promoted
        );
        let LogicalCatalogObject::Compatible(compatible) = promoted else {
            return Err("legacy definition was not compatibly promoted".into());
        };
        assert_eq!(compatible.object, legacy);
        Ok(())
    }
}
