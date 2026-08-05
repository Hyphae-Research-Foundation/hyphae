// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    ops::{Bound, ControlFlow},
};

use hyphae_native_catalog::{
    CatalogName, CatalogObject, ColumnCheckConstraint, ColumnCheckOperator, ColumnDefinition,
    ObjectHeader, RelationDefinition, SecondaryIndexDefinition,
};
use hyphae_native_mvcc::Snapshot;
use hyphae_native_records::{ColumnValueRef, RowTuple, RowTupleView};
use hyphae_native_types::{
    CatalogVersion, ColumnId, DecimalType, EngineKind, IntegerWidth, LogicalType, ObjectId,
    ScalarValue,
};
use thiserror::Error;

use crate::{
    NativeDatabase, NativeRuntimeError, NativeSnapshot, NativeWriteBatch,
    model::SecondaryIndexLayout, qualified_name,
};

/// Value accepted or returned by native SQL.
pub type SqlValue = ScalarValue;

/// Result of one native SQL statement or prepared execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SqlResult {
    /// DDL or DML completion.
    Command {
        /// Number of logical rows affected.
        rows_affected: u64,
        /// Stable object identity created by DDL, when applicable.
        object_id: Option<ObjectId>,
    },
    /// Materialized result rows.
    Rows {
        /// Stable output column names.
        columns: Vec<String>,
        /// Rows in executor order.
        rows: Vec<Vec<SqlValue>>,
    },
}

/// Native SQL lexer, parser, binder, or execution failure.
#[derive(Debug, Error)]
pub enum SqlError {
    /// The statement is outside the current exact grammar.
    #[error("HYSQL001 invalid or unsupported native SQL syntax")]
    InvalidSyntax,
    /// Parameter arity differs from the bound plan.
    #[error("HYSQL002 native SQL parameter mismatch")]
    ParameterMismatch,
    /// A prepared plan's catalog version is no longer current.
    #[error("HYSQL003 native SQL prepared plan requires rebind")]
    CatalogChanged,
    /// A referenced column is absent from the bound relation.
    #[error("HYSQL004 native SQL column does not exist")]
    UnknownColumn,
    /// A column was supplied more than once where identity must be unique.
    #[error("HYSQL005 native SQL column is duplicated")]
    DuplicateColumn,
    /// A scalar does not match its catalog logical type or domain.
    #[error("HYSQL006 native SQL value does not match its logical type")]
    TypeMismatch,
    /// A required column received or defaulted to SQL null.
    #[error("HYSQL007 native SQL NOT NULL constraint failed")]
    NullViolation,
    /// A primary-key predicate is incomplete, duplicated, null, or out of order.
    #[error("HYSQL008 native SQL primary-key binding is invalid")]
    InvalidPrimaryKey,
    /// Stored bytes do not match the catalog-bound tuple representation.
    #[error("HYSQL009 native SQL stored row is malformed")]
    InvalidStoredRow,
    /// Catalog ownership and object kind disagree.
    #[error("HYSQL010 native SQL catalog object kind is invalid")]
    InvalidCatalogObject,
    /// No primary or secondary index can satisfy the exact predicates.
    #[error("HYSQL011 native SQL query has no implemented access path")]
    NoAccessPath,
    /// A non-null unique secondary-index key already identifies a row.
    #[error("HYSQL012 native SQL unique constraint failed")]
    UniqueViolation,
    /// Updating a primary-key column is outside the current mutation contract.
    #[error("HYSQL013 native SQL primary-key mutation is not implemented")]
    PrimaryKeyMutationUnsupported,
    /// Secondary-index range bounds are duplicated or malformed.
    #[error("HYSQL014 native SQL secondary-index range binding is invalid")]
    InvalidSecondaryIndexRange,
    /// A native SQL CHECK predicate evaluated to false.
    #[error("HYSQL015 native SQL CHECK constraint failed")]
    CheckViolation,
    /// Native storage or engine execution failed.
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
}

/// Catalog-bound parameterized native SQL plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedStatement {
    catalog_version: CatalogVersion,
    plan: PreparedPlan,
}

impl PreparedStatement {
    /// Returns the catalog version used by the binder.
    pub const fn catalog_version(&self) -> CatalogVersion {
        self.catalog_version
    }

    pub(crate) fn parameter_count(&self) -> usize {
        self.plan.parameter_count()
    }

    pub(crate) fn result_schema(&self) -> Result<Vec<(String, LogicalType)>, SqlError> {
        self.plan.result_schema()
    }

    pub(crate) fn maximum_result_rows(&self) -> Option<usize> {
        self.plan.maximum_result_rows()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreparedPlan {
    PrimaryKeyLookup {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        key: KeyBinding,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        legacy_binary: bool,
    },
    SecondaryIndexLookup {
        table: ObjectId,
        index: ObjectId,
        relation: Box<RelationDefinition>,
        index_definition: Box<SecondaryIndexDefinition>,
        projection: Vec<usize>,
        key: KeyBinding,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        limit: Option<usize>,
    },
    SecondaryIndexRangeScan {
        table: ObjectId,
        index: ObjectId,
        relation: Box<RelationDefinition>,
        index_definition: Box<SecondaryIndexDefinition>,
        projection: Vec<usize>,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        range: SecondaryIndexRange,
        limit: usize,
    },
    PrimaryKeyScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        filter: Option<BoundFilterExpression>,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        limit: usize,
        legacy_binary: bool,
    },
    PrimaryKeyPrefixScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        prefix: KeyBinding,
        limit: usize,
    },
    PrimaryKeyPrefixRangeScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        range: PrimaryKeyPrefixRange,
        limit: usize,
    },
    PrimaryKeyRangeScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        filter: BoundFilterExpression,
        parameter_count: usize,
        residual: bool,
        output_columns: Vec<String>,
        range: PrimaryKeyRange,
        limit: usize,
        legacy_binary: bool,
    },
    IndexedInnerJoin {
        left_table: ObjectId,
        right_table: ObjectId,
        left_relation: Box<RelationDefinition>,
        right_relation: Box<RelationDefinition>,
        left_access: JoinLeftAccess,
        left_filter: Option<BoundFilterExpression>,
        left_join_columns: Vec<usize>,
        right_access: JoinRightAccess,
        projection: Vec<JoinProjection>,
        parameter_count: usize,
        output_columns: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum JoinLeftAccess {
    PrimaryKey {
        key: KeyBinding,
    },
    UniqueSecondaryIndex {
        index: ObjectId,
        definition: Box<SecondaryIndexDefinition>,
        key: KeyBinding,
    },
    BoundedSecondaryIndex {
        index: ObjectId,
        definition: Box<SecondaryIndexDefinition>,
        key: KeyBinding,
        limit: usize,
    },
    BoundedPrimaryKeyScan {
        range: Option<PrimaryKeyRange>,
        limit: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum JoinRightAccess {
    PrimaryKey {
        columns: Vec<usize>,
    },
    UniqueSecondaryIndex {
        index: ObjectId,
        columns: Vec<usize>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JoinSide {
    Left,
    Right,
}

impl PreparedPlan {
    fn parameter_count(&self) -> usize {
        match self {
            Self::PrimaryKeyLookup {
                parameter_count, ..
            }
            | Self::SecondaryIndexLookup {
                parameter_count, ..
            }
            | Self::SecondaryIndexRangeScan {
                parameter_count, ..
            }
            | Self::PrimaryKeyScan {
                parameter_count, ..
            }
            | Self::PrimaryKeyPrefixScan {
                parameter_count, ..
            }
            | Self::PrimaryKeyPrefixRangeScan {
                parameter_count, ..
            }
            | Self::PrimaryKeyRangeScan {
                parameter_count, ..
            }
            | Self::IndexedInnerJoin {
                parameter_count, ..
            } => *parameter_count,
        }
    }

    fn maximum_result_rows(&self) -> Option<usize> {
        match self {
            Self::PrimaryKeyLookup { .. } => Some(1),
            Self::SecondaryIndexLookup {
                index_definition,
                limit,
                ..
            } => {
                if index_definition.unique {
                    Some(1)
                } else {
                    *limit
                }
            }
            Self::SecondaryIndexRangeScan { limit, .. }
            | Self::PrimaryKeyScan { limit, .. }
            | Self::PrimaryKeyPrefixScan { limit, .. }
            | Self::PrimaryKeyPrefixRangeScan { limit, .. }
            | Self::PrimaryKeyRangeScan { limit, .. } => Some(*limit),
            Self::IndexedInnerJoin { left_access, .. } => match left_access {
                JoinLeftAccess::PrimaryKey { .. } | JoinLeftAccess::UniqueSecondaryIndex { .. } => {
                    Some(1)
                }
                JoinLeftAccess::BoundedSecondaryIndex { limit, .. }
                | JoinLeftAccess::BoundedPrimaryKeyScan { limit, .. } => Some(*limit),
            },
        }
    }

    fn result_schema(&self) -> Result<Vec<(String, LogicalType)>, SqlError> {
        match self {
            Self::PrimaryKeyLookup {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::SecondaryIndexLookup {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::SecondaryIndexRangeScan {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::PrimaryKeyScan {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::PrimaryKeyPrefixScan {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::PrimaryKeyPrefixRangeScan {
                relation,
                projection,
                output_columns,
                ..
            }
            | Self::PrimaryKeyRangeScan {
                relation,
                projection,
                output_columns,
                ..
            } => projected_schema(relation, projection, output_columns),
            Self::IndexedInnerJoin {
                left_relation,
                right_relation,
                projection,
                output_columns,
                ..
            } => join_projected_schema(left_relation, right_relation, projection, output_columns),
        }
    }
}

fn projected_schema(
    relation: &RelationDefinition,
    projection: &[usize],
    output_columns: &[String],
) -> Result<Vec<(String, LogicalType)>, SqlError> {
    if projection.len() != output_columns.len() {
        return Err(SqlError::InvalidCatalogObject);
    }
    projection
        .iter()
        .zip(output_columns)
        .map(|(column, output)| {
            let definition = relation
                .columns
                .get(*column)
                .ok_or(SqlError::UnknownColumn)?;
            Ok((output.clone(), definition.logical_type.clone()))
        })
        .collect()
}

fn join_projected_schema(
    left_relation: &RelationDefinition,
    right_relation: &RelationDefinition,
    projection: &[JoinProjection],
    output_columns: &[String],
) -> Result<Vec<(String, LogicalType)>, SqlError> {
    if projection.len() != output_columns.len() {
        return Err(SqlError::InvalidCatalogObject);
    }
    projection
        .iter()
        .zip(output_columns)
        .map(|(projected, output)| {
            let relation = match projected.side {
                JoinSide::Left => left_relation,
                JoinSide::Right => right_relation,
            };
            let definition = relation
                .columns
                .get(projected.column)
                .ok_or(SqlError::UnknownColumn)?;
            Ok((output.clone(), definition.logical_type.clone()))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JoinProjection {
    side: JoinSide,
    column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ParsedColumn>,
        primary_key: Vec<String>,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
    },
    Insert {
        name: String,
        values: Vec<ColumnOperand>,
        parameter_count: usize,
    },
    Update {
        name: String,
        assignments: Vec<ColumnOperand>,
        predicates: Vec<ColumnOperand>,
        parameter_count: usize,
    },
    Delete {
        name: String,
        predicates: Vec<ColumnOperand>,
        parameter_count: usize,
    },
    Select {
        name: String,
        projection: Projection,
        filter: Option<FilterExpression>,
        parameter_count: usize,
        order_by: Vec<String>,
        limit: Option<usize>,
    },
    ExplainSelect {
        name: String,
        projection: Projection,
        filter: Option<FilterExpression>,
        parameter_count: usize,
        order_by: Vec<String>,
        limit: Option<usize>,
    },
    SelectJoin(ParsedInnerJoin),
    ExplainSelectJoin(ParsedInnerJoin),
    WithSelect(ParsedCteSelect),
    SelectWindow(ParsedWindowSelect),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowFunction {
    RowNumber,
    Rank,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedWindowSelect {
    name: String,
    value_column: String,
    function: WindowFunction,
    partition_column: Option<String>,
    order_column: String,
    alias: String,
    outer_order_by: Vec<String>,
    limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedCteSelect {
    name: String,
    inner: Box<Statement>,
    outer: Box<Statement>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedInnerJoin {
    left_name: String,
    right_name: String,
    projection: Vec<String>,
    equalities: Vec<ParsedJoinEquality>,
    filter: Option<FilterExpression>,
    parameter_count: usize,
    order_by: Vec<String>,
    limit: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedJoinEquality {
    left_column: String,
    right_column: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedColumn {
    name: String,
    logical_type: LogicalType,
    nullable: bool,
    inline_primary_key: bool,
    check: Option<ParsedColumnCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedColumnCheck {
    operator: ComparisonOperator,
    operand: ScalarOperand,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ColumnOperand {
    column: String,
    operand: ScalarOperand,
}

struct BoundUpdateAssignment {
    column: usize,
    value: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Projection {
    All,
    Columns(Vec<String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComparisonOperator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilterExpression {
    Comparison {
        columns: Vec<String>,
        operator: ComparisonOperator,
        operands: Vec<ScalarOperand>,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BoundFilterExpression {
    Comparison {
        columns: Vec<usize>,
        operator: ComparisonOperator,
        operands: Vec<BoundScalarOperand>,
    },
    IsNull {
        column: usize,
        negated: bool,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScalarOperand {
    Parameter(usize),
    Null,
    Boolean(bool),
    Integer(String),
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BoundScalarOperand {
    Parameter(usize),
    Literal(SqlValue),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KeyBinding {
    columns: Vec<usize>,
    operands: Vec<BoundScalarOperand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyRangeEndpoint {
    key: KeyBinding,
    inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyRange {
    lower: Option<PrimaryKeyRangeEndpoint>,
    upper: Option<PrimaryKeyRangeEndpoint>,
    parameter_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecondaryIndexRangeEndpoint {
    key: KeyBinding,
    inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecondaryIndexPrefixRangeEndpoint {
    operand: BoundScalarOperand,
    inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SecondaryIndexRangeKind {
    Complete {
        lower: Option<SecondaryIndexRangeEndpoint>,
        upper: Option<SecondaryIndexRangeEndpoint>,
    },
    Prefix {
        prefix: KeyBinding,
        range_column: usize,
        lower: Option<SecondaryIndexPrefixRangeEndpoint>,
        upper: Option<SecondaryIndexPrefixRangeEndpoint>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecondaryIndexRange {
    kind: SecondaryIndexRangeKind,
    parameter_count: usize,
}

enum BoundSecondaryIndexEndpoint {
    Unbounded,
    Null,
    Key { encoded: Vec<u8>, inclusive: bool },
}

impl BoundSecondaryIndexEndpoint {
    fn into_bound(self) -> Option<Bound<Vec<u8>>> {
        match self {
            Self::Unbounded => Some(Bound::Unbounded),
            Self::Null => None,
            Self::Key { encoded, inclusive } if inclusive => Some(Bound::Included(encoded)),
            Self::Key { encoded, .. } => Some(Bound::Excluded(encoded)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyPrefixRangeEndpoint {
    operand: BoundScalarOperand,
    inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyPrefixRange {
    prefix: KeyBinding,
    range_column: usize,
    lower: Option<PrimaryKeyPrefixRangeEndpoint>,
    upper: Option<PrimaryKeyPrefixRangeEndpoint>,
}

type KeyBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

#[derive(Clone, Copy)]
struct ScanExecution {
    limit: usize,
    legacy_binary: bool,
}

struct BoundSelect {
    table: ObjectId,
    projection: Vec<usize>,
    filter: Option<BoundFilterExpression>,
    parameter_count: usize,
    residual: bool,
    output_columns: Vec<String>,
    access: SelectAccess,
}

#[derive(Clone, Copy)]
struct SelectQuery<'query> {
    name: &'query str,
    projection: &'query Projection,
    filter: Option<&'query FilterExpression>,
    parameter_count: usize,
    order_by: &'query [String],
    limit: Option<usize>,
}

enum SelectAccess {
    PrimaryKey {
        key: KeyBinding,
        legacy_binary: bool,
    },
    SecondaryIndex {
        index: ObjectId,
        key: KeyBinding,
        limit: Option<usize>,
    },
    SecondaryIndexRangeScan {
        index: ObjectId,
        range: SecondaryIndexRange,
        limit: usize,
    },
    PrimaryKeyScan {
        limit: usize,
        legacy_binary: bool,
    },
    PrimaryKeyPrefixScan {
        prefix: KeyBinding,
        limit: usize,
    },
    PrimaryKeyPrefixRangeScan {
        range: PrimaryKeyPrefixRange,
        limit: usize,
    },
    PrimaryKeyRangeScan {
        range: PrimaryKeyRange,
        limit: usize,
        legacy_binary: bool,
    },
}

pub(crate) fn prepare(
    snapshot: &NativeSnapshot,
    statement: &str,
) -> Result<PreparedStatement, SqlError> {
    let ordered_secondary_indexes = snapshot
        .state
        .relational
        .indexes
        .iter()
        .filter_map(|(index, state)| {
            (state.layout == SecondaryIndexLayout::OrderedV2).then_some(*index)
        })
        .collect();
    prepare_catalog(
        snapshot.catalog_version(),
        &snapshot.state.catalog,
        &ordered_secondary_indexes,
        statement,
    )
}

pub(crate) fn prepare_catalog(
    catalog_version: CatalogVersion,
    catalog: &crate::model::CatalogState,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
    statement: &str,
) -> Result<PreparedStatement, SqlError> {
    let plan = match parse(statement)? {
        Statement::Select {
            name,
            projection,
            filter,
            parameter_count,
            order_by,
            limit,
        } => prepare_select_plan(
            catalog,
            SelectQuery {
                name: &name,
                projection: &projection,
                filter: filter.as_ref(),
                parameter_count,
                order_by: &order_by,
                limit,
            },
            ordered_secondary_indexes,
        )?,
        Statement::SelectJoin(join) => {
            bind_indexed_inner_join(catalog, ordered_secondary_indexes, &join)?
        }
        _ => return Err(SqlError::InvalidSyntax),
    };
    Ok(PreparedStatement {
        catalog_version,
        plan,
    })
}

// Keep the exhaustive access-to-plan mapping together so new operators cannot
// bypass catalog binding or prepared-plan metadata.
#[allow(clippy::too_many_lines)]
fn prepare_select_plan(
    catalog: &crate::model::CatalogState,
    query: SelectQuery<'_>,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
) -> Result<PreparedPlan, SqlError> {
    let bound = bind_select(catalog, query, ordered_secondary_indexes)?;
    let relation = relation_by_id(catalog, bound.table)?.clone();
    Ok(match bound.access {
        SelectAccess::PrimaryKey { key, legacy_binary } => PreparedPlan::PrimaryKeyLookup {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            key,
            filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
            parameter_count: bound.parameter_count,
            residual: bound.residual,
            output_columns: bound.output_columns,
            legacy_binary,
        },
        SelectAccess::SecondaryIndex { index, key, limit } => PreparedPlan::SecondaryIndexLookup {
            table: bound.table,
            index,
            relation: Box::new(relation),
            index_definition: Box::new(secondary_index_by_id(catalog, index)?.clone()),
            projection: bound.projection,
            key,
            filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
            parameter_count: bound.parameter_count,
            residual: bound.residual,
            output_columns: bound.output_columns,
            limit,
        },
        SelectAccess::SecondaryIndexRangeScan {
            index,
            range,
            limit,
        } => PreparedPlan::SecondaryIndexRangeScan {
            table: bound.table,
            index,
            relation: Box::new(relation),
            index_definition: Box::new(secondary_index_by_id(catalog, index)?.clone()),
            projection: bound.projection,
            filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
            parameter_count: bound.parameter_count,
            residual: bound.residual,
            output_columns: bound.output_columns,
            range,
            limit,
        },
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary,
        } => PreparedPlan::PrimaryKeyScan {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            filter: bound.filter,
            parameter_count: bound.parameter_count,
            residual: bound.residual,
            output_columns: bound.output_columns,
            limit,
            legacy_binary,
        },
        SelectAccess::PrimaryKeyPrefixScan { prefix, limit } => {
            PreparedPlan::PrimaryKeyPrefixScan {
                table: bound.table,
                relation: Box::new(relation),
                projection: bound.projection,
                filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
                parameter_count: bound.parameter_count,
                residual: bound.residual,
                output_columns: bound.output_columns,
                prefix,
                limit,
            }
        }
        SelectAccess::PrimaryKeyPrefixRangeScan { range, limit } => {
            PreparedPlan::PrimaryKeyPrefixRangeScan {
                table: bound.table,
                relation: Box::new(relation),
                projection: bound.projection,
                filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
                parameter_count: bound.parameter_count,
                residual: bound.residual,
                output_columns: bound.output_columns,
                range,
                limit,
            }
        }
        SelectAccess::PrimaryKeyRangeScan {
            range,
            limit,
            legacy_binary,
        } => PreparedPlan::PrimaryKeyRangeScan {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            filter: bound.filter.ok_or(SqlError::InvalidSyntax)?,
            parameter_count: bound.parameter_count,
            residual: bound.residual,
            output_columns: bound.output_columns,
            range,
            limit,
            legacy_binary,
        },
    })
}

pub(crate) fn execute_prepared(
    snapshot: &NativeSnapshot,
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    ensure_catalog_version(snapshot.catalog_version(), prepared)?;
    execute_bound_snapshot(snapshot, &prepared.plan, parameters)
}

pub(crate) fn execute_prepared_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    ensure_catalog_version(snapshot.catalog_version, prepared)?;
    execute_bound_latest(database, snapshot, &prepared.plan, parameters)
}

pub(crate) fn execute_prepared_binary<'snapshot>(
    snapshot: &'snapshot NativeSnapshot,
    prepared: &PreparedStatement,
    primary_key: &[u8],
) -> Result<Option<&'snapshot [u8]>, SqlError> {
    ensure_catalog_version(snapshot.catalog_version(), prepared)?;
    match &prepared.plan {
        PreparedPlan::PrimaryKeyLookup {
            table,
            projection,
            key,
            parameter_count: 1,
            residual: false,
            legacy_binary: true,
            ..
        } if projection.as_slice() == [1]
            && key.columns.as_slice() == [0]
            && key.operands.as_slice() == [BoundScalarOperand::Parameter(0)] =>
        {
            Ok(snapshot.select(*table, primary_key))
        }
        PreparedPlan::PrimaryKeyLookup { .. }
        | PreparedPlan::SecondaryIndexLookup { .. }
        | PreparedPlan::SecondaryIndexRangeScan { .. }
        | PreparedPlan::PrimaryKeyScan { .. }
        | PreparedPlan::PrimaryKeyPrefixScan { .. }
        | PreparedPlan::PrimaryKeyPrefixRangeScan { .. }
        | PreparedPlan::PrimaryKeyRangeScan { .. }
        | PreparedPlan::IndexedInnerJoin { .. } => Err(SqlError::ParameterMismatch),
    }
}

pub(crate) fn execute_transaction(
    transaction: &mut NativeWriteBatch,
    statement: &str,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    match parse(statement)? {
        Statement::CreateTable {
            name,
            columns,
            primary_key,
        } => execute_create(transaction, &name, &columns, primary_key, parameters),
        Statement::CreateIndex {
            name,
            table,
            columns,
            unique,
        } => execute_create_index(transaction, &name, &table, &columns, unique, parameters),
        Statement::Insert {
            name,
            values,
            parameter_count,
        } => execute_insert(transaction, &name, &values, parameter_count, parameters),
        Statement::Update {
            name,
            assignments,
            predicates,
            parameter_count,
        } => execute_update(
            transaction,
            &name,
            &assignments,
            &predicates,
            parameter_count,
            parameters,
        ),
        Statement::Delete {
            name,
            predicates,
            parameter_count,
        } => execute_delete(transaction, &name, &predicates, parameter_count, parameters),
        Statement::Select {
            name,
            projection,
            filter,
            parameter_count,
            order_by,
            limit,
        } => execute_select(
            transaction,
            SelectQuery {
                name: &name,
                projection: &projection,
                filter: filter.as_ref(),
                parameter_count,
                order_by: &order_by,
                limit,
            },
            parameters,
        ),
        Statement::ExplainSelect {
            name,
            projection,
            filter,
            parameter_count,
            order_by,
            limit,
        } => execute_explain(
            transaction,
            SelectQuery {
                name: &name,
                projection: &projection,
                filter: filter.as_ref(),
                parameter_count,
                order_by: &order_by,
                limit,
            },
            parameters,
        ),
        Statement::SelectJoin(join) => {
            let ordered_secondary_indexes = transaction_ordered_secondary_indexes(transaction);
            let plan = bind_indexed_inner_join(
                &transaction.state.catalog,
                &ordered_secondary_indexes,
                &join,
            )?;
            execute_indexed_join_transaction(transaction, &plan, parameters)
        }
        Statement::ExplainSelectJoin(join) => {
            execute_indexed_join_explain(transaction, &join, parameters)
        }
        Statement::WithSelect(cte) => execute_cte_select(transaction, &cte, parameters),
        Statement::SelectWindow(window) => execute_window_select(transaction, &window, parameters),
    }
}

fn execute_window_select(
    transaction: &mut NativeWriteBatch,
    window: &ParsedWindowSelect,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let (_, definition) = relation_named(&transaction.state.catalog, &window.name)?;
    let order_index = column_index(&definition.columns, &window.order_column)?;
    let partition_index = window
        .partition_column
        .as_ref()
        .map(|column| column_index(&definition.columns, column))
        .transpose()?;
    let expected_key = partition_index
        .into_iter()
        .chain(std::iter::once(order_index))
        .map(|index| definition.columns[index].id)
        .collect::<Vec<_>>();
    if definition.primary_key != expected_key {
        return Err(SqlError::InvalidSyntax);
    }
    let order_columns = window
        .partition_column
        .iter()
        .cloned()
        .chain(std::iter::once(window.order_column.clone()))
        .collect::<Vec<_>>();
    let query = format!(
        "SELECT {}, {} FROM {} ORDER BY {} LIMIT {}",
        window.value_column,
        order_columns.join(", "),
        window.name,
        window.outer_order_by.join(", "),
        window.limit
    );
    let Statement::Select {
        name,
        projection,
        filter,
        parameter_count,
        order_by,
        limit,
    } = parse(&query)?
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let SqlResult::Rows { columns: _, rows } = execute_select(
        transaction,
        SelectQuery {
            name: &name,
            projection: &projection,
            filter: filter.as_ref(),
            parameter_count,
            order_by: &order_by,
            limit,
        },
        &[],
    )?
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let mut output = Vec::with_capacity(rows.len());
    let mut previous_partition = None;
    let mut ordinal = 0_u64;
    for row in rows {
        let value = row.first().cloned().ok_or(SqlError::InvalidStoredRow)?;
        let partition = window
            .partition_column
            .as_ref()
            .map(|_| row.get(1).cloned().ok_or(SqlError::InvalidStoredRow))
            .transpose()?;
        if partition != previous_partition {
            ordinal = 0;
            previous_partition = partition;
        }
        ordinal = ordinal.checked_add(1).ok_or(SqlError::InvalidSyntax)?;
        let rank = match window.function {
            WindowFunction::RowNumber | WindowFunction::Rank => ordinal,
        };
        output.push(vec![value, SqlValue::Unsigned(rank)]);
    }
    Ok(SqlResult::Rows {
        columns: vec![window.value_column.clone(), window.alias.clone()],
        rows: output,
    })
}

fn execute_cte_select(
    transaction: &mut NativeWriteBatch,
    cte: &ParsedCteSelect,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let inner_parameter_count = statement_parameter_count(&cte.inner)?;
    let outer_parameter_count = statement_parameter_count(&cte.outer)?;
    if parameters.len() != inner_parameter_count + outer_parameter_count {
        return Err(SqlError::ParameterMismatch);
    }
    let (inner_parameters, outer_parameters) = parameters.split_at(inner_parameter_count);
    let inner = execute_parsed_transaction(transaction, &cte.inner, inner_parameters)?;
    let SqlResult::Rows { columns, rows } = inner else {
        return Err(SqlError::InvalidSyntax);
    };
    let Statement::Select {
        name,
        projection,
        filter,
        parameter_count,
        order_by,
        limit,
    } = cte.outer.as_ref()
    else {
        return Err(SqlError::InvalidSyntax);
    };
    if normalize_identifier(name) != normalize_identifier(&cte.name)
        || filter.is_some()
        || *parameter_count != outer_parameters.len()
        || !order_by.is_empty()
    {
        return Err(SqlError::InvalidSyntax);
    }
    let projection = match projection {
        Projection::All => (0..columns.len()).collect::<Vec<_>>(),
        Projection::Columns(names) => names
            .iter()
            .map(|name| {
                let lookup = normalize_identifier(name);
                columns
                    .iter()
                    .position(|column| normalize_identifier(column) == lookup)
                    .ok_or(SqlError::UnknownColumn)
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let output_columns = projection
        .iter()
        .map(|index| columns.get(*index).cloned().ok_or(SqlError::UnknownColumn))
        .collect::<Result<Vec<_>, _>>()?;
    let limit = limit.unwrap_or(rows.len());
    let rows = rows
        .into_iter()
        .take(limit)
        .map(|row| {
            projection
                .iter()
                .map(|index| row.get(*index).cloned().ok_or(SqlError::InvalidStoredRow))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SqlResult::Rows {
        columns: output_columns,
        rows,
    })
}

fn statement_parameter_count(statement: &Statement) -> Result<usize, SqlError> {
    match statement {
        Statement::Select {
            parameter_count, ..
        } => Ok(*parameter_count),
        _ => Err(SqlError::InvalidSyntax),
    }
}

fn execute_parsed_transaction(
    transaction: &mut NativeWriteBatch,
    statement: &Statement,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    match statement {
        Statement::Select {
            name,
            projection,
            filter,
            parameter_count,
            order_by,
            limit,
        } => execute_select(
            transaction,
            SelectQuery {
                name,
                projection,
                filter: filter.as_ref(),
                parameter_count: *parameter_count,
                order_by,
                limit: *limit,
            },
            parameters,
        ),
        _ => Err(SqlError::InvalidSyntax),
    }
}

pub(crate) struct TransactionDml {
    statement: Statement,
}

impl TransactionDml {
    pub(crate) fn parse(statement: &str) -> Result<Self, SqlError> {
        let statement = parse(statement)?;
        match statement {
            Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } => {
                Ok(Self { statement })
            }
            _ => Err(SqlError::InvalidSyntax),
        }
    }

    pub(crate) fn relation_name(&self) -> Result<&str, SqlError> {
        match &self.statement {
            Statement::Insert { name, .. }
            | Statement::Update { name, .. }
            | Statement::Delete { name, .. } => Ok(name),
            _ => Err(SqlError::InvalidSyntax),
        }
    }

    pub(crate) fn primary_key(
        &self,
        transaction: &NativeWriteBatch,
        parameters: &[SqlValue],
    ) -> Result<(ObjectId, Vec<u8>), SqlError> {
        match &self.statement {
            Statement::Insert {
                name,
                values,
                parameter_count,
            } => {
                let (table, definition) = relation_named(&transaction.state.catalog, name)?;
                let resolved =
                    resolve_mutation_operands(definition, values, *parameter_count, parameters)?;
                let values = bind_insert_values(definition, values, &resolved)?;
                let primary_key = if is_legacy_binary_relation(definition) {
                    legacy_binary_value(values[0], false)?
                } else {
                    encode_primary_key(definition, &values)?
                };
                Ok((table, primary_key))
            }
            Statement::Update {
                name,
                predicates,
                parameter_count,
                ..
            }
            | Statement::Delete {
                name,
                predicates,
                parameter_count,
            } => {
                let (table, definition) = relation_named(&transaction.state.catalog, name)?;
                let predicate_columns = bind_primary_key_columns(definition, predicates)?;
                let predicate_values = resolve_mutation_operands(
                    definition,
                    predicates,
                    *parameter_count,
                    parameters,
                )?;
                let primary_key =
                    bind_primary_key(definition, &predicate_columns, &predicate_values)?;
                Ok((table, primary_key))
            }
            _ => Err(SqlError::InvalidSyntax),
        }
    }

    pub(crate) fn candidate_row(
        &self,
        transaction: &NativeWriteBatch,
        parameters: &[SqlValue],
    ) -> Result<Option<Vec<u8>>, SqlError> {
        match &self.statement {
            Statement::Insert {
                name,
                values,
                parameter_count,
            } => {
                let (_, definition) = relation_named(&transaction.state.catalog, name)?;
                let resolved =
                    resolve_mutation_operands(definition, values, *parameter_count, parameters)?;
                let values = bind_insert_values(definition, values, &resolved)?;
                if is_legacy_binary_relation(definition) {
                    Ok(Some(legacy_binary_value(values[1], false)?))
                } else {
                    Ok(Some(encode_tuple(definition, &values)?))
                }
            }
            Statement::Update {
                name,
                assignments,
                predicates,
                parameter_count,
            } => transaction_update_candidate(
                transaction,
                name,
                assignments,
                predicates,
                *parameter_count,
                parameters,
            ),
            Statement::Delete { .. } => Ok(None),
            _ => Err(SqlError::InvalidSyntax),
        }
    }

    pub(crate) fn execute(
        &self,
        transaction: &mut NativeWriteBatch,
        parameters: &[SqlValue],
    ) -> Result<SqlResult, SqlError> {
        match &self.statement {
            Statement::Insert {
                name,
                values,
                parameter_count,
            } => execute_insert(transaction, name, values, *parameter_count, parameters),
            Statement::Update {
                name,
                assignments,
                predicates,
                parameter_count,
            } => execute_update(
                transaction,
                name,
                assignments,
                predicates,
                *parameter_count,
                parameters,
            ),
            Statement::Delete {
                name,
                predicates,
                parameter_count,
            } => execute_delete(transaction, name, predicates, *parameter_count, parameters),
            _ => Err(SqlError::InvalidSyntax),
        }
    }
}

fn transaction_update_candidate(
    transaction: &NativeWriteBatch,
    name: &str,
    assignments: &[ColumnOperand],
    predicates: &[ColumnOperand],
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<Option<Vec<u8>>, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let assignment_columns = bind_update_columns(definition, assignments)?;
    let predicate_columns = bind_primary_key_columns(definition, predicates)?;
    let assignment_values =
        resolve_mutation_operands(definition, assignments, parameter_count, parameters)?;
    let predicate_values =
        resolve_mutation_operands(definition, predicates, parameter_count, parameters)?;
    let primary_key = bind_primary_key(definition, &predicate_columns, &predicate_values)?;
    let Some(stored) = transaction.select(table, &primary_key) else {
        return Ok(None);
    };
    if is_legacy_binary_relation(definition) {
        if assignment_columns.as_slice() != [1] {
            return Err(SqlError::InvalidSyntax);
        }
        Ok(Some(legacy_binary_value(assignment_values.first(), false)?))
    } else {
        let assignments =
            bind_update_assignments(definition, &assignment_columns, &assignment_values)?;
        Ok(Some(encode_updated_tuple(
            definition,
            &assignments,
            stored,
        )?))
    }
}

pub(crate) fn execute_transaction_dml(
    transaction: &mut NativeWriteBatch,
    statement: &str,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    TransactionDml::parse(statement)?.execute(transaction, parameters)
}

fn execute_bound_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    match plan {
        PreparedPlan::PrimaryKeyLookup {
            table,
            relation,
            projection,
            key,
            filter,
            parameter_count,
            output_columns,
            legacy_binary,
            ..
        } => {
            validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
            if key_contains_null(key, parameters)? {
                return Ok(SqlResult::Rows {
                    columns: output_columns.clone(),
                    rows: Vec::new(),
                });
            }
            let primary_key = bind_primary_key_binding(relation, key, parameters)?;
            let rows = snapshot.select(*table, &primary_key).map_or_else(
                || Ok(Vec::new()),
                |stored| {
                    materialize_filtered_row(
                        relation,
                        projection,
                        *legacy_binary,
                        &primary_key,
                        stored,
                        Some(filter),
                        parameters,
                    )
                    .map(|row| row.into_iter().collect())
                },
            )?;
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::SecondaryIndexLookup { .. } => {
            execute_secondary_index_snapshot(snapshot, plan, parameters)
        }
        PreparedPlan::SecondaryIndexRangeScan { .. } => {
            execute_secondary_index_range_snapshot(snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyScan { .. } => execute_snapshot_scan(snapshot, plan, parameters),
        PreparedPlan::PrimaryKeyPrefixScan { .. } => {
            execute_snapshot_prefix_scan(snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyPrefixRangeScan { .. } => {
            execute_snapshot_prefix_range_scan(snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyRangeScan { .. } => {
            execute_snapshot_range_scan(snapshot, plan, parameters)
        }
        PreparedPlan::IndexedInnerJoin { .. } => {
            execute_indexed_join_snapshot(snapshot, plan, parameters)
        }
    }
}

fn execute_bound_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    match plan {
        PreparedPlan::PrimaryKeyLookup {
            table,
            relation,
            projection,
            key,
            filter,
            parameter_count,
            output_columns,
            legacy_binary,
            ..
        } => {
            validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
            if key_contains_null(key, parameters)? {
                return Ok(SqlResult::Rows {
                    columns: output_columns.clone(),
                    rows: Vec::new(),
                });
            }
            let primary_key = bind_primary_key_binding(relation, key, parameters)?;
            let rows = database
                .select_relational_at(snapshot, *table, &primary_key)?
                .map_or_else(
                    || Ok(Vec::new()),
                    |stored| {
                        materialize_filtered_row(
                            relation,
                            projection,
                            *legacy_binary,
                            &primary_key,
                            &stored,
                            Some(filter),
                            parameters,
                        )
                        .map(|row| row.into_iter().collect())
                    },
                )?;
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::SecondaryIndexLookup { .. } => {
            execute_secondary_index_latest(database, snapshot, plan, parameters)
        }
        PreparedPlan::SecondaryIndexRangeScan { .. } => {
            execute_secondary_index_range_latest(database, snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyScan { .. } => {
            execute_latest_scan(database, snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyPrefixScan { .. } => {
            execute_latest_prefix_scan(database, snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyPrefixRangeScan { .. } => {
            execute_latest_prefix_range_scan(database, snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyRangeScan { .. } => {
            execute_latest_range_scan(database, snapshot, plan, parameters)
        }
        PreparedPlan::IndexedInnerJoin { .. } => {
            execute_indexed_join_latest(database, snapshot, plan, parameters)
        }
    }
}

fn execute_secondary_index_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::SecondaryIndexLookup {
        table,
        index,
        relation,
        index_definition,
        projection,
        key,
        filter,
        parameter_count,
        output_columns,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some(index_key) =
        bind_secondary_index_key_binding(relation, index_definition, key, parameters)?
    else {
        return Ok(empty_rows_result(output_columns));
    };
    let mut rows = Vec::with_capacity(limit.unwrap_or(0).min(256));
    if *limit == Some(0) {
        return Ok(rows_result(output_columns, rows));
    }
    let primary_keys = snapshot
        .state
        .relational
        .secondary_index_lookup(*index, &index_key)
        .map_err(NativeRuntimeError::from)?;
    if let Some(primary_keys) = primary_keys {
        if limit.is_none() {
            rows.reserve(primary_keys.len());
        }
        for primary_key in primary_keys {
            let stored = snapshot
                .select(*table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            if let Some(row) = materialize_filtered_row(
                relation,
                projection,
                false,
                primary_key,
                stored,
                Some(filter),
                parameters,
            )? {
                rows.push(row);
                if limit.is_some_and(|limit| rows.len() == limit) {
                    break;
                }
            }
        }
    }
    Ok(rows_result(output_columns, rows))
}

fn execute_secondary_index_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::SecondaryIndexLookup {
        table,
        index,
        relation,
        index_definition,
        projection,
        key,
        filter,
        parameter_count,
        output_columns,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some(index_key) =
        bind_secondary_index_key_binding(relation, index_definition, key, parameters)?
    else {
        return Ok(empty_rows_result(output_columns));
    };
    let mut rows = Vec::with_capacity(limit.unwrap_or(0).min(256));
    if *limit == Some(0) {
        return Ok(rows_result(output_columns, rows));
    }
    database
        .visit_secondary_index_at(
            snapshot,
            *index,
            &index_key,
            |matched_table, primary_key, stored| {
                if matched_table != *table {
                    return Err(SqlError::InvalidCatalogObject);
                }
                if let Some(row) = materialize_filtered_row(
                    relation,
                    projection,
                    false,
                    primary_key,
                    stored,
                    Some(filter),
                    parameters,
                )? {
                    rows.push(row);
                    if limit.is_some_and(|limit| rows.len() == limit) {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(map_relational_visit_error)?;
    Ok(rows_result(output_columns, rows))
}

fn execute_secondary_index_range_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::SecondaryIndexRangeScan {
        table,
        index,
        relation,
        index_definition,
        projection,
        filter,
        parameter_count,
        residual,
        output_columns,
        range,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some((lower, upper)) =
        bind_secondary_index_range(relation, index_definition, range, parameters)?
    else {
        return Ok(empty_rows_result(output_columns));
    };
    if key_range_is_empty(&lower, &upper) || *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let index_state = snapshot
        .state
        .relational
        .indexes
        .get(index)
        .ok_or(SqlError::InvalidCatalogObject)?;
    if index_state.layout != SecondaryIndexLayout::OrderedV2 || index_state.relation != *table {
        return Err(SqlError::InvalidCatalogObject);
    }
    let index_columns = secondary_index_column_indices(relation, index_definition)?;
    let row_filter = if *residual { Some(filter) } else { None };
    let mut rows = Vec::with_capacity((*limit).min(256));
    for (index_key, primary_keys) in index_state.entries.range((lower, upper)) {
        for primary_key in primary_keys {
            let stored = snapshot
                .select(*table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            let values = decode_complete_row(relation, false, primary_key, stored)?;
            validate_secondary_index_values(relation, &index_columns, index_key, &values)?;
            if let Some(row) =
                materialize_decoded_row(relation, projection, &values, row_filter, parameters)?
            {
                rows.push(row);
                if rows.len() == *limit {
                    return Ok(rows_result(output_columns, rows));
                }
            }
        }
    }
    Ok(rows_result(output_columns, rows))
}

fn execute_secondary_index_range_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::SecondaryIndexRangeScan {
        table,
        index,
        relation,
        index_definition,
        projection,
        filter,
        parameter_count,
        residual,
        output_columns,
        range,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some((lower, upper)) =
        bind_secondary_index_range(relation, index_definition, range, parameters)?
    else {
        return Ok(empty_rows_result(output_columns));
    };
    if key_range_is_empty(&lower, &upper) || *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let index_columns = secondary_index_column_indices(relation, index_definition)?;
    let row_filter = if *residual { Some(filter) } else { None };
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_secondary_index_range_at(
            snapshot,
            *index,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            |matched_table, index_key, primary_key, stored| {
                if matched_table != *table {
                    return Err(SqlError::InvalidCatalogObject);
                }
                let values = decode_complete_row(relation, false, primary_key, stored)?;
                validate_secondary_index_values(relation, &index_columns, index_key, &values)?;
                if let Some(row) =
                    materialize_decoded_row(relation, projection, &values, row_filter, parameters)?
                {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(map_relational_visit_error)?;
    Ok(rows_result(output_columns, rows))
}

fn execute_indexed_join_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_access,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        parameter_count,
        output_columns,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(
        left_relation,
        left_filter.as_ref(),
        *parameter_count,
        parameters,
    )?;
    if let JoinLeftAccess::BoundedPrimaryKeyScan { range, limit } = left_access {
        return execute_bounded_join_snapshot(snapshot, plan, range.as_ref(), *limit, parameters);
    }
    if matches!(left_access, JoinLeftAccess::BoundedSecondaryIndex { .. }) {
        return execute_bounded_secondary_join_snapshot(snapshot, plan, left_access, parameters);
    }
    let left = snapshot_join_left(
        snapshot,
        *left_table,
        left_relation,
        left_access,
        parameters,
    )?;
    materialize_indexed_join(
        &JoinMaterialization {
            left_relation,
            right_relation,
            left_filter: left_filter.as_ref(),
            projection,
            output_columns,
            parameters,
        },
        left,
        |values| {
            snapshot_join_right(
                snapshot,
                *right_table,
                right_relation,
                right_access,
                values,
                left_join_columns,
            )
        },
    )
}

fn execute_indexed_join_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_access,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        parameter_count,
        output_columns,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(
        left_relation,
        left_filter.as_ref(),
        *parameter_count,
        parameters,
    )?;
    if let JoinLeftAccess::BoundedPrimaryKeyScan { range, limit } = left_access {
        return execute_bounded_join_latest(
            database,
            snapshot,
            plan,
            range.as_ref(),
            *limit,
            parameters,
        );
    }
    if matches!(left_access, JoinLeftAccess::BoundedSecondaryIndex { .. }) {
        return execute_bounded_secondary_join_latest(
            database,
            snapshot,
            plan,
            left_access,
            parameters,
        );
    }
    let left = latest_join_left(
        database,
        snapshot,
        *left_table,
        left_relation,
        left_access,
        parameters,
    )?;
    materialize_indexed_join(
        &JoinMaterialization {
            left_relation,
            right_relation,
            left_filter: left_filter.as_ref(),
            projection,
            output_columns,
            parameters,
        },
        left,
        |value| {
            latest_join_right(
                database,
                snapshot,
                *right_table,
                right_relation,
                right_access,
                value,
                left_join_columns,
            )
        },
    )
}

fn execute_indexed_join_transaction(
    transaction: &NativeWriteBatch,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_access,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        parameter_count,
        output_columns,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(
        left_relation,
        left_filter.as_ref(),
        *parameter_count,
        parameters,
    )?;
    if let JoinLeftAccess::BoundedPrimaryKeyScan { range, limit } = left_access {
        return execute_bounded_join_transaction(
            transaction,
            plan,
            range.as_ref(),
            *limit,
            parameters,
        );
    }
    if matches!(left_access, JoinLeftAccess::BoundedSecondaryIndex { .. }) {
        return execute_bounded_secondary_join_transaction(
            transaction,
            plan,
            left_access,
            parameters,
        );
    }
    let left = transaction_join_left(
        transaction,
        *left_table,
        left_relation,
        left_access,
        parameters,
    )?;
    materialize_indexed_join(
        &JoinMaterialization {
            left_relation,
            right_relation,
            left_filter: left_filter.as_ref(),
            projection,
            output_columns,
            parameters,
        },
        left,
        |value| {
            transaction_join_right(
                transaction,
                *right_table,
                right_relation,
                right_access,
                value,
                left_join_columns,
            )
        },
    )
}

fn execute_bounded_join_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    range: Option<&PrimaryKeyRange>,
    limit: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let bounds = join_scan_bounds(left_relation, range, parameters)?;
    if limit == 0 || key_range_is_empty(&bounds.0, &bounds.1) {
        return Ok(empty_join_result(output_columns));
    }
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(left_table)
        .ok_or(SqlError::InvalidStoredRow)?;
    collect_bounded_join(
        &JoinMaterialization {
            left_relation,
            right_relation,
            left_filter: left_filter.as_ref(),
            projection,
            output_columns,
            parameters,
        },
        stored_rows.range(bounds),
        limit,
        |values| {
            snapshot_join_right(
                snapshot,
                *right_table,
                right_relation,
                right_access,
                values,
                left_join_columns,
            )
        },
    )
}

fn execute_bounded_join_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    range: Option<&PrimaryKeyRange>,
    limit: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let (lower, upper) = join_scan_bounds(left_relation, range, parameters)?;
    if limit == 0 || key_range_is_empty(&lower, &upper) {
        return Ok(empty_join_result(output_columns));
    }
    let context = JoinMaterialization {
        left_relation,
        right_relation,
        left_filter: left_filter.as_ref(),
        projection,
        output_columns,
        parameters,
    };
    let mut rows = Vec::with_capacity(limit.min(256));
    database
        .visit_relational_range_at(
            snapshot,
            *left_table,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            |primary_key, stored| {
                if let Some(row) = materialize_join_row(&context, primary_key, stored, |value| {
                    latest_join_right(
                        database,
                        snapshot,
                        *right_table,
                        right_relation,
                        right_access,
                        value,
                        left_join_columns,
                    )
                })? {
                    rows.push(row);
                    if rows.len() == limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(|error| match error {
            crate::RelationalVisitError::Runtime(error) => SqlError::Runtime(error),
            crate::RelationalVisitError::Visitor(error) => error,
        })?;
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_bounded_join_transaction(
    transaction: &NativeWriteBatch,
    plan: &PreparedPlan,
    range: Option<&PrimaryKeyRange>,
    limit: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let bounds = join_scan_bounds(left_relation, range, parameters)?;
    if limit == 0 || key_range_is_empty(&bounds.0, &bounds.1) {
        return Ok(empty_join_result(output_columns));
    }
    let stored_rows = transaction
        .state
        .relational
        .tables
        .get(left_table)
        .ok_or(SqlError::InvalidStoredRow)?;
    collect_bounded_join(
        &JoinMaterialization {
            left_relation,
            right_relation,
            left_filter: left_filter.as_ref(),
            projection,
            output_columns,
            parameters,
        },
        stored_rows.range(bounds),
        limit,
        |value| {
            transaction_join_right(
                transaction,
                *right_table,
                right_relation,
                right_access,
                value,
                left_join_columns,
            )
        },
    )
}

fn execute_bounded_secondary_join_snapshot(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let JoinLeftAccess::BoundedSecondaryIndex {
        index,
        definition,
        key,
        limit,
    } = access
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let Some(index_key) =
        bind_secondary_index_key_binding(left_relation, definition, key, parameters)?
    else {
        return Ok(empty_join_result(output_columns));
    };
    if *limit == 0 {
        return Ok(empty_join_result(output_columns));
    }
    let Some(primary_keys) = snapshot
        .state
        .relational
        .secondary_index_lookup(*index, &index_key)
        .map_err(NativeRuntimeError::from)?
    else {
        return Ok(empty_join_result(output_columns));
    };
    let context = JoinMaterialization {
        left_relation,
        right_relation,
        left_filter: left_filter.as_ref(),
        projection,
        output_columns,
        parameters,
    };
    let mut rows = Vec::with_capacity((*limit).min(256));
    for primary_key in primary_keys {
        let stored = snapshot
            .select(*left_table, primary_key)
            .ok_or(SqlError::InvalidStoredRow)?;
        if let Some(row) = materialize_join_row(&context, primary_key, stored, |value| {
            snapshot_join_right(
                snapshot,
                *right_table,
                right_relation,
                right_access,
                value,
                left_join_columns,
            )
        })? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_bounded_secondary_join_latest(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let JoinLeftAccess::BoundedSecondaryIndex {
        index,
        definition,
        key,
        limit,
    } = access
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let Some(index_key) =
        bind_secondary_index_key_binding(left_relation, definition, key, parameters)?
    else {
        return Ok(empty_join_result(output_columns));
    };
    if *limit == 0 {
        return Ok(empty_join_result(output_columns));
    }
    let context = JoinMaterialization {
        left_relation,
        right_relation,
        left_filter: left_filter.as_ref(),
        projection,
        output_columns,
        parameters,
    };
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_secondary_index_at(
            snapshot,
            *index,
            &index_key,
            |matched_table, primary_key, stored| {
                if matched_table != *left_table {
                    return Err(SqlError::InvalidCatalogObject);
                }
                if let Some(row) = materialize_join_row(&context, primary_key, stored, |value| {
                    latest_join_right(
                        database,
                        snapshot,
                        *right_table,
                        right_relation,
                        right_access,
                        value,
                        left_join_columns,
                    )
                })? {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(|error| match error {
            crate::RelationalVisitError::Runtime(error) => SqlError::Runtime(error),
            crate::RelationalVisitError::Visitor(error) => error,
        })?;
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_bounded_secondary_join_transaction(
    transaction: &NativeWriteBatch,
    plan: &PreparedPlan,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let JoinLeftAccess::BoundedSecondaryIndex {
        index,
        definition,
        key,
        limit,
    } = access
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_relation,
        right_relation,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        output_columns,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let Some(index_key) =
        bind_secondary_index_key_binding(left_relation, definition, key, parameters)?
    else {
        return Ok(empty_join_result(output_columns));
    };
    if *limit == 0 {
        return Ok(empty_join_result(output_columns));
    }
    let Some(primary_keys) = transaction
        .state
        .relational
        .secondary_index_lookup(*index, &index_key)
        .map_err(NativeRuntimeError::from)?
    else {
        return Ok(empty_join_result(output_columns));
    };
    let context = JoinMaterialization {
        left_relation,
        right_relation,
        left_filter: left_filter.as_ref(),
        projection,
        output_columns,
        parameters,
    };
    let mut rows = Vec::with_capacity((*limit).min(256));
    for primary_key in primary_keys {
        let stored = transaction
            .select(*left_table, primary_key)
            .ok_or(SqlError::InvalidStoredRow)?;
        if let Some(row) = materialize_join_row(&context, primary_key, stored, |value| {
            transaction_join_right(
                transaction,
                *right_table,
                right_relation,
                right_access,
                value,
                left_join_columns,
            )
        })? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn join_scan_bounds(
    relation: &RelationDefinition,
    range: Option<&PrimaryKeyRange>,
    parameters: &[SqlValue],
) -> Result<KeyBounds, SqlError> {
    range.map_or_else(
        || Ok((Bound::Unbounded, Bound::Unbounded)),
        |range| bind_primary_key_range(relation, range, parameters),
    )
}

fn collect_bounded_join<'row>(
    context: &JoinMaterialization<'_>,
    stored_rows: impl IntoIterator<Item = (&'row Vec<u8>, &'row Vec<u8>)>,
    limit: usize,
    mut right_lookup: impl FnMut(&[SqlValue]) -> Result<JoinInputRow, SqlError>,
) -> Result<SqlResult, SqlError> {
    let mut rows = Vec::with_capacity(limit.min(256));
    for (primary_key, stored) in stored_rows {
        if let Some(row) = materialize_join_row(context, primary_key, stored, &mut right_lookup)? {
            rows.push(row);
            if rows.len() == limit {
                break;
            }
        }
    }
    Ok(SqlResult::Rows {
        columns: context.output_columns.to_vec(),
        rows,
    })
}

type JoinInputRow = Option<(Vec<u8>, Vec<u8>)>;

fn snapshot_join_left(
    snapshot: &NativeSnapshot,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinLeftAccess::PrimaryKey { key } => {
            if key_contains_null(key, parameters)? {
                return Ok(None);
            }
            let primary_key = bind_primary_key_binding(relation, key, parameters)?;
            Ok(snapshot
                .select(table, &primary_key)
                .map(|stored| (primary_key, stored.to_vec())))
        }
        JoinLeftAccess::UniqueSecondaryIndex {
            index,
            definition,
            key,
        } => {
            let Some(index_key) =
                bind_secondary_index_key_binding(relation, definition, key, parameters)?
            else {
                return Ok(None);
            };
            let primary_keys = snapshot
                .state
                .relational
                .secondary_index_lookup(*index, &index_key)
                .map_err(NativeRuntimeError::from)?;
            let Some(primary_keys) = primary_keys else {
                return Ok(None);
            };
            let Some(primary_key) = at_most_one_unique_key(primary_keys.iter())? else {
                return Ok(None);
            };
            let stored = snapshot
                .select(table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            Ok(Some((primary_key.clone(), stored.to_vec())))
        }
        JoinLeftAccess::BoundedPrimaryKeyScan { .. }
        | JoinLeftAccess::BoundedSecondaryIndex { .. } => Err(SqlError::InvalidSyntax),
    }
}

fn latest_join_left(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinLeftAccess::PrimaryKey { key } => {
            if key_contains_null(key, parameters)? {
                return Ok(None);
            }
            let primary_key = bind_primary_key_binding(relation, key, parameters)?;
            Ok(database
                .select_relational_at(snapshot, table, &primary_key)?
                .map(|stored| (primary_key, stored)))
        }
        JoinLeftAccess::UniqueSecondaryIndex {
            index,
            definition,
            key,
        } => {
            let Some(index_key) =
                bind_secondary_index_key_binding(relation, definition, key, parameters)?
            else {
                return Ok(None);
            };
            let index_rows = database.select_secondary_index_at(snapshot, *index, &index_key)?;
            let Some(matched) = at_most_one_unique_key(index_rows.iter())? else {
                return Ok(None);
            };
            if matched.table != table {
                return Err(SqlError::InvalidCatalogObject);
            }
            Ok(Some((matched.primary_key.clone(), matched.row.clone())))
        }
        JoinLeftAccess::BoundedPrimaryKeyScan { .. }
        | JoinLeftAccess::BoundedSecondaryIndex { .. } => Err(SqlError::InvalidSyntax),
    }
}

fn transaction_join_left(
    transaction: &NativeWriteBatch,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinLeftAccess,
    parameters: &[SqlValue],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinLeftAccess::PrimaryKey { key } => {
            if key_contains_null(key, parameters)? {
                return Ok(None);
            }
            let primary_key = bind_primary_key_binding(relation, key, parameters)?;
            Ok(transaction
                .select(table, &primary_key)
                .map(|stored| (primary_key, stored.to_vec())))
        }
        JoinLeftAccess::UniqueSecondaryIndex {
            index,
            definition,
            key,
        } => {
            let Some(index_key) =
                bind_secondary_index_key_binding(relation, definition, key, parameters)?
            else {
                return Ok(None);
            };
            let primary_keys = transaction
                .state
                .relational
                .secondary_index_lookup(*index, &index_key)
                .map_err(NativeRuntimeError::from)?;
            let Some(primary_keys) = primary_keys else {
                return Ok(None);
            };
            let Some(primary_key) = at_most_one_unique_key(primary_keys.iter())? else {
                return Ok(None);
            };
            let stored = transaction
                .select(table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            Ok(Some((primary_key.clone(), stored.to_vec())))
        }
        JoinLeftAccess::BoundedPrimaryKeyScan { .. }
        | JoinLeftAccess::BoundedSecondaryIndex { .. } => Err(SqlError::InvalidSyntax),
    }
}

fn snapshot_join_right(
    snapshot: &NativeSnapshot,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinRightAccess,
    left_values: &[SqlValue],
    left_columns: &[usize],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinRightAccess::PrimaryKey { columns } => {
            let Some(primary_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            Ok(snapshot
                .select(table, &primary_key)
                .map(|stored| (primary_key, stored.to_vec())))
        }
        JoinRightAccess::UniqueSecondaryIndex { index, columns } => {
            let Some(index_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            let primary_keys = snapshot
                .state
                .relational
                .secondary_index_lookup(*index, &index_key)
                .map_err(NativeRuntimeError::from)?;
            let Some(primary_keys) = primary_keys else {
                return Ok(None);
            };
            let Some(primary_key) = at_most_one_unique_key(primary_keys.iter())? else {
                return Ok(None);
            };
            let stored = snapshot
                .select(table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            Ok(Some((primary_key.clone(), stored.to_vec())))
        }
    }
}

fn latest_join_right(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinRightAccess,
    left_values: &[SqlValue],
    left_columns: &[usize],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinRightAccess::PrimaryKey { columns } => {
            let Some(primary_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            Ok(database
                .select_relational_at(snapshot, table, &primary_key)?
                .map(|stored| (primary_key, stored)))
        }
        JoinRightAccess::UniqueSecondaryIndex { index, columns } => {
            let Some(index_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            let rows = database.select_secondary_index_at(snapshot, *index, &index_key)?;
            let Some(matched) = at_most_one_unique_key(rows.iter())? else {
                return Ok(None);
            };
            if matched.table != table {
                return Err(SqlError::InvalidCatalogObject);
            }
            Ok(Some((matched.primary_key.clone(), matched.row.clone())))
        }
    }
}

fn transaction_join_right(
    transaction: &NativeWriteBatch,
    table: ObjectId,
    relation: &RelationDefinition,
    access: &JoinRightAccess,
    left_values: &[SqlValue],
    left_columns: &[usize],
) -> Result<JoinInputRow, SqlError> {
    match access {
        JoinRightAccess::PrimaryKey { columns } => {
            let Some(primary_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            Ok(transaction
                .select(table, &primary_key)
                .map(|stored| (primary_key, stored.to_vec())))
        }
        JoinRightAccess::UniqueSecondaryIndex { index, columns } => {
            let Some(index_key) =
                bind_join_access_key(relation, columns, left_values, left_columns)?
            else {
                return Ok(None);
            };
            let primary_keys = transaction
                .state
                .relational
                .secondary_index_lookup(*index, &index_key)
                .map_err(NativeRuntimeError::from)?;
            let Some(primary_keys) = primary_keys else {
                return Ok(None);
            };
            let Some(primary_key) = at_most_one_unique_key(primary_keys.iter())? else {
                return Ok(None);
            };
            let stored = transaction
                .select(table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            Ok(Some((primary_key.clone(), stored.to_vec())))
        }
    }
}

fn bind_join_access_key(
    right_relation: &RelationDefinition,
    right_columns: &[usize],
    left_values: &[SqlValue],
    left_columns: &[usize],
) -> Result<Option<Vec<u8>>, SqlError> {
    if right_columns.is_empty() || right_columns.len() != left_columns.len() {
        return Err(SqlError::NoAccessPath);
    }
    let mut encoded = Vec::new();
    for (right_column, left_column) in right_columns.iter().zip(left_columns) {
        let value = left_values
            .get(*left_column)
            .ok_or(SqlError::InvalidStoredRow)?;
        if matches!(value, SqlValue::Null) {
            return Ok(None);
        }
        let logical_type = &right_relation
            .columns
            .get(*right_column)
            .ok_or(SqlError::InvalidCatalogObject)?
            .logical_type;
        encoded.extend_from_slice(
            &value
                .encode_ordered_component(logical_type)
                .map_err(|_| SqlError::TypeMismatch)?,
        );
    }
    Ok(Some(encoded))
}

fn at_most_one_unique_key<'item, T: 'item>(
    mut items: impl ExactSizeIterator<Item = &'item T>,
) -> Result<Option<&'item T>, SqlError> {
    if items.len() > 1 {
        return Err(SqlError::InvalidStoredRow);
    }
    Ok(items.next())
}

struct JoinMaterialization<'plan> {
    left_relation: &'plan RelationDefinition,
    right_relation: &'plan RelationDefinition,
    left_filter: Option<&'plan BoundFilterExpression>,
    projection: &'plan [JoinProjection],
    output_columns: &'plan [String],
    parameters: &'plan [SqlValue],
}

fn materialize_indexed_join(
    context: &JoinMaterialization<'_>,
    left: JoinInputRow,
    right_lookup: impl FnMut(&[SqlValue]) -> Result<JoinInputRow, SqlError>,
) -> Result<SqlResult, SqlError> {
    let Some((left_primary_key, left_stored)) = left else {
        return Ok(empty_join_result(context.output_columns));
    };
    let Some(row) = materialize_join_row(context, &left_primary_key, &left_stored, right_lookup)?
    else {
        return Ok(empty_join_result(context.output_columns));
    };
    Ok(SqlResult::Rows {
        columns: context.output_columns.to_vec(),
        rows: vec![row],
    })
}

fn materialize_join_row(
    context: &JoinMaterialization<'_>,
    left_primary_key: &[u8],
    left_stored: &[u8],
    mut right_lookup: impl FnMut(&[SqlValue]) -> Result<JoinInputRow, SqlError>,
) -> Result<Option<Vec<SqlValue>>, SqlError> {
    let left_values =
        decode_complete_row(context.left_relation, false, left_primary_key, left_stored)?;
    if let Some(left_filter) = context.left_filter
        && evaluate_filter(
            context.left_relation,
            left_filter,
            &left_values,
            context.parameters,
        )? != TruthValue::True
    {
        return Ok(None);
    }
    let Some((right_primary_key, right_stored)) = right_lookup(&left_values)? else {
        return Ok(None);
    };
    let right_values = decode_complete_row(
        context.right_relation,
        false,
        &right_primary_key,
        &right_stored,
    )?;
    let row = context
        .projection
        .iter()
        .map(|projection| match projection.side {
            JoinSide::Left => left_values.get(projection.column),
            JoinSide::Right => right_values.get(projection.column),
        })
        .map(|value| value.cloned().ok_or(SqlError::InvalidStoredRow))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(row))
}

fn empty_join_result(output_columns: &[String]) -> SqlResult {
    empty_rows_result(output_columns)
}

fn empty_rows_result(output_columns: &[String]) -> SqlResult {
    rows_result(output_columns, Vec::new())
}

fn rows_result(output_columns: &[String], rows: Vec<Vec<SqlValue>>) -> SqlResult {
    SqlResult::Rows {
        columns: output_columns.to_vec(),
        rows,
    }
}

fn map_relational_visit_error(error: crate::RelationalVisitError<SqlError>) -> SqlError {
    match error {
        crate::RelationalVisitError::Runtime(error) => SqlError::Runtime(error),
        crate::RelationalVisitError::Visitor(error) => error,
    }
}

fn execute_snapshot_scan(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        limit,
        legacy_binary,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, filter.as_ref(), *parameter_count, parameters)?;
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    if *limit == 0 {
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    for (primary_key, stored) in stored_rows {
        if let Some(row) = materialize_filtered_row(
            relation,
            projection,
            *legacy_binary,
            primary_key,
            stored,
            filter.as_ref(),
            parameters,
        )? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_latest_scan(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        limit,
        legacy_binary,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, filter.as_ref(), *parameter_count, parameters)?;
    if *limit == 0 {
        database.scan_relational_at(snapshot, *table, None, 0)?;
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_relational_range_at(
            snapshot,
            *table,
            Bound::Unbounded,
            Bound::Unbounded,
            |primary_key, stored| {
                if let Some(row) = materialize_filtered_row(
                    relation,
                    projection,
                    *legacy_binary,
                    primary_key,
                    stored,
                    filter.as_ref(),
                    parameters,
                )? {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(|error| match error {
            crate::RelationalVisitError::Runtime(error) => SqlError::Runtime(error),
            crate::RelationalVisitError::Visitor(error) => error,
        })?;
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_snapshot_prefix_scan(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyPrefixScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        prefix,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some(prefix) = bind_primary_key_prefix(relation, prefix, parameters)? else {
        return Ok(empty_rows_result(output_columns));
    };
    if *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let bounds = primary_key_prefix_bounds(prefix);
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    let mut rows = Vec::with_capacity((*limit).min(256));
    for (primary_key, stored) in stored_rows.range(bounds) {
        if let Some(row) = materialize_filtered_row(
            relation,
            projection,
            false,
            primary_key,
            stored,
            Some(filter),
            parameters,
        )? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(rows_result(output_columns, rows))
}

fn execute_latest_prefix_scan(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyPrefixScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        prefix,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some(prefix) = bind_primary_key_prefix(relation, prefix, parameters)? else {
        return Ok(empty_rows_result(output_columns));
    };
    let (lower, upper) = primary_key_prefix_bounds(prefix);
    if *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_relational_range_at(
            snapshot,
            *table,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            |primary_key, stored| {
                if let Some(row) = materialize_filtered_row(
                    relation,
                    projection,
                    false,
                    primary_key,
                    stored,
                    Some(filter),
                    parameters,
                )? {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(map_relational_visit_error)?;
    Ok(rows_result(output_columns, rows))
}

fn execute_snapshot_prefix_range_scan(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyPrefixRangeScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        range,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some((lower, upper)) = bind_primary_key_prefix_range(relation, range, parameters)? else {
        return Ok(empty_rows_result(output_columns));
    };
    if key_range_is_empty(&lower, &upper) || *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    let mut rows = Vec::with_capacity((*limit).min(256));
    for (primary_key, stored) in stored_rows.range((lower, upper)) {
        if let Some(row) = materialize_filtered_row(
            relation,
            projection,
            false,
            primary_key,
            stored,
            Some(filter),
            parameters,
        )? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(rows_result(output_columns, rows))
}

fn execute_latest_prefix_range_scan(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyPrefixRangeScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        range,
        limit,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let Some((lower, upper)) = bind_primary_key_prefix_range(relation, range, parameters)? else {
        return Ok(empty_rows_result(output_columns));
    };
    if key_range_is_empty(&lower, &upper) || *limit == 0 {
        return Ok(empty_rows_result(output_columns));
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_relational_range_at(
            snapshot,
            *table,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            |primary_key, stored| {
                if let Some(row) = materialize_filtered_row(
                    relation,
                    projection,
                    false,
                    primary_key,
                    stored,
                    Some(filter),
                    parameters,
                )? {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(map_relational_visit_error)?;
    Ok(rows_result(output_columns, rows))
}

fn execute_snapshot_range_scan(
    snapshot: &NativeSnapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyRangeScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        range,
        limit,
        legacy_binary,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let (lower, upper) = bind_primary_key_range(relation, range, parameters)?;
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    if key_range_is_empty(&lower, &upper) || *limit == 0 {
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    for (primary_key, stored) in stored_rows.range((lower, upper)) {
        if let Some(row) = materialize_filtered_row(
            relation,
            projection,
            *legacy_binary,
            primary_key,
            stored,
            Some(filter),
            parameters,
        )? {
            rows.push(row);
            if rows.len() == *limit {
                break;
            }
        }
    }
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_latest_range_scan(
    database: &NativeDatabase,
    snapshot: &Snapshot,
    plan: &PreparedPlan,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let PreparedPlan::PrimaryKeyRangeScan {
        table,
        relation,
        projection,
        filter,
        parameter_count,
        output_columns,
        range,
        limit,
        legacy_binary,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    validate_filter_parameters(relation, Some(filter), *parameter_count, parameters)?;
    let (lower, upper) = bind_primary_key_range(relation, range, parameters)?;
    if *limit == 0 {
        database.scan_relational_range_at(
            snapshot,
            *table,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            0,
        )?;
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    if key_range_is_empty(&lower, &upper) {
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    let mut rows = Vec::with_capacity((*limit).min(256));
    database
        .visit_relational_range_at(
            snapshot,
            *table,
            crate::bound_as_slice(&lower),
            crate::bound_as_slice(&upper),
            |primary_key, stored| {
                if let Some(row) = materialize_filtered_row(
                    relation,
                    projection,
                    *legacy_binary,
                    primary_key,
                    stored,
                    Some(filter),
                    parameters,
                )? {
                    rows.push(row);
                    if rows.len() == *limit {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(|error| match error {
            crate::RelationalVisitError::Runtime(error) => SqlError::Runtime(error),
            crate::RelationalVisitError::Visitor(error) => error,
        })?;
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
}

fn execute_create_index(
    transaction: &mut NativeWriteBatch,
    name: &str,
    table_name: &str,
    column_names: &[String],
    unique: bool,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let (relation, table) = relation_named(&transaction.state.catalog, table_name)?;
    if is_legacy_binary_relation(table) {
        return Err(SqlError::InvalidSyntax);
    }
    let mut columns = Vec::with_capacity(column_names.len());
    for name in column_names {
        let column = table.columns[column_index(&table.columns, name)?].id;
        if columns.contains(&column) {
            return Err(SqlError::DuplicateColumn);
        }
        columns.push(column);
    }
    let id = transaction
        .state
        .catalog
        .next_object_id()
        .map_err(NativeRuntimeError::from)?;
    let definition = SecondaryIndexDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Relational,
            name: qualified_name(name).map_err(NativeRuntimeError::from)?,
        },
        relation,
        columns,
        unique,
        nulls_distinct: true,
    };
    definition.validate().map_err(NativeRuntimeError::from)?;
    transaction
        .create_secondary_index_definition(&definition)
        .map_err(map_runtime_error)?;
    Ok(SqlResult::Command {
        rows_affected: 0,
        object_id: Some(id),
    })
}

fn execute_explain(
    transaction: &NativeWriteBatch,
    query: SelectQuery<'_>,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let ordered_secondary_indexes = transaction_ordered_secondary_indexes(transaction);
    let bound = bind_select(
        &transaction.state.catalog,
        query,
        &ordered_secondary_indexes,
    )?;
    let plan = match bound.access {
        SelectAccess::PrimaryKey { .. } => {
            format!(
                "PrimaryKeyLookup(table={}{}",
                bound.table,
                explain_suffix(bound.residual)
            )
        }
        SelectAccess::SecondaryIndex { index, limit, .. } => limit.map_or_else(
            || {
                format!(
                    "SecondaryIndexLookup(table={},index={index}{}",
                    bound.table,
                    explain_suffix(bound.residual)
                )
            },
            |limit| {
                format!(
                    "SecondaryIndexLookup(table={},index={index},limit={limit}{}",
                    bound.table,
                    explain_suffix(bound.residual)
                )
            },
        ),
        SelectAccess::SecondaryIndexRangeScan {
            index,
            range,
            limit,
        } => explain_secondary_index_range(
            &transaction.state.catalog,
            bound.table,
            bound.residual,
            index,
            &range,
            limit,
        )?,
        SelectAccess::PrimaryKeyScan { limit, .. } => {
            format!(
                "PrimaryKeyScan(table={},limit={limit}{}",
                bound.table,
                explain_suffix(bound.residual)
            )
        }
        SelectAccess::PrimaryKeyPrefixScan { prefix, limit } => format!(
            "PrimaryKeyPrefixScan(table={},columns={},limit={limit}{}",
            bound.table,
            prefix.columns.len(),
            explain_suffix(bound.residual),
        ),
        SelectAccess::PrimaryKeyPrefixRangeScan { range, limit } => {
            let relation = relation_by_id(&transaction.state.catalog, bound.table)?;
            let range_column = relation
                .columns
                .get(range.range_column)
                .ok_or(SqlError::InvalidCatalogObject)?
                .id
                .get();
            format!(
                "PrimaryKeyPrefixRangeScan(table={},prefix_columns={},\
                 range_column={range_column},lower={},upper={},limit={limit}{}",
                bound.table,
                range.prefix.columns.len(),
                prefix_range_bound_name(range.lower.as_ref()),
                prefix_range_bound_name(range.upper.as_ref()),
                explain_suffix(bound.residual),
            )
        }
        SelectAccess::PrimaryKeyRangeScan { range, limit, .. } => format!(
            "PrimaryKeyRangeScan(table={},lower={},upper={},limit={limit}{}",
            bound.table,
            range_bound_name(range.lower.as_ref()),
            range_bound_name(range.upper.as_ref()),
            explain_suffix(bound.residual),
        ),
    };
    Ok(SqlResult::Rows {
        columns: vec!["plan".to_owned()],
        rows: vec![vec![SqlValue::Text(plan)]],
    })
}

fn explain_secondary_index_range(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    residual: bool,
    index: ObjectId,
    range: &SecondaryIndexRange,
    limit: usize,
) -> Result<String, SqlError> {
    Ok(match &range.kind {
        SecondaryIndexRangeKind::Complete { lower, upper } => format!(
            "SecondaryIndexRangeScan(table={},index={index},lower={},upper={},limit={limit}{}",
            table,
            secondary_range_bound_name(lower.as_ref()),
            secondary_range_bound_name(upper.as_ref()),
            explain_suffix(residual),
        ),
        SecondaryIndexRangeKind::Prefix {
            prefix,
            range_column,
            lower,
            upper,
        } => {
            let relation = relation_by_id(catalog, table)?;
            let range_column = relation
                .columns
                .get(*range_column)
                .ok_or(SqlError::InvalidCatalogObject)?
                .id
                .get();
            format!(
                "SecondaryIndexPrefixRangeScan(table={},index={index},\
                 prefix_columns={},range_column={range_column},lower={},\
                 upper={},limit={limit}{}",
                table,
                prefix.columns.len(),
                secondary_prefix_range_bound_name(lower.as_ref()),
                secondary_prefix_range_bound_name(upper.as_ref()),
                explain_suffix(residual),
            )
        }
    })
}

fn execute_indexed_join_explain(
    transaction: &NativeWriteBatch,
    parsed: &ParsedInnerJoin,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let ordered_secondary_indexes = transaction_ordered_secondary_indexes(transaction);
    let plan = bind_indexed_inner_join(
        &transaction.state.catalog,
        &ordered_secondary_indexes,
        parsed,
    )?;
    let PreparedPlan::IndexedInnerJoin {
        left_table,
        right_table,
        left_access,
        right_access,
        ..
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let left_access = match left_access {
        JoinLeftAccess::PrimaryKey { .. } => "primary-key".to_owned(),
        JoinLeftAccess::UniqueSecondaryIndex { index, .. } => {
            format!("unique-secondary(index={index})")
        }
        JoinLeftAccess::BoundedSecondaryIndex { index, limit, .. } => {
            format!("secondary(index={index},limit={limit})")
        }
        JoinLeftAccess::BoundedPrimaryKeyScan { range: None, limit } => {
            format!("primary-key-scan(limit={limit})")
        }
        JoinLeftAccess::BoundedPrimaryKeyScan {
            range: Some(range),
            limit,
        } => format!(
            "primary-key-range(lower={},upper={},limit={limit})",
            range_bound_name(range.lower.as_ref()),
            range_bound_name(range.upper.as_ref()),
        ),
    };
    let right_access = match right_access {
        JoinRightAccess::PrimaryKey { .. } => "primary-key".to_owned(),
        JoinRightAccess::UniqueSecondaryIndex { index, .. } => {
            format!("unique-secondary(index={index})")
        }
    };
    Ok(SqlResult::Rows {
        columns: vec!["plan".to_owned()],
        rows: vec![vec![SqlValue::Text(format!(
            "IndexedInnerJoin(left_table={left_table},left_access={left_access},right_table={right_table},right_access={right_access})"
        ))]],
    })
}

fn explain_suffix(residual: bool) -> &'static str {
    if residual { ",residual=true)" } else { ")" }
}

fn execute_create(
    transaction: &mut NativeWriteBatch,
    name: &str,
    parsed_columns: &[ParsedColumn],
    primary_key_names: Vec<String>,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let id = transaction
        .state
        .catalog
        .next_object_id()
        .map_err(NativeRuntimeError::from)?;
    let mut columns = Vec::with_capacity(parsed_columns.len());
    for (index, parsed) in parsed_columns.iter().enumerate() {
        let raw_id = u32::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(SqlError::InvalidSyntax)?;
        columns.push(ColumnDefinition {
            id: ColumnId::new(raw_id).map_err(|_| SqlError::InvalidSyntax)?,
            name: CatalogName::unquoted(parsed.name.clone()).map_err(NativeRuntimeError::from)?,
            logical_type: parsed.logical_type.clone(),
            nullable: parsed.nullable,
        });
    }
    let mut primary_key = Vec::with_capacity(primary_key_names.len());
    for name in primary_key_names {
        let index = column_index(&columns, &name)?;
        let column = &mut columns[index];
        if primary_key.contains(&column.id) {
            return Err(SqlError::DuplicateColumn);
        }
        column.nullable = false;
        primary_key.push(column.id);
    }
    if primary_key.is_empty() {
        return Err(SqlError::InvalidPrimaryKey);
    }
    let mut checks = Vec::new();
    for parsed in parsed_columns {
        let Some(check) = &parsed.check else {
            continue;
        };
        let index = columns
            .iter()
            .position(|column| column.name.lookup() == normalize_identifier(&parsed.name))
            .ok_or(SqlError::UnknownColumn)?;
        let column = &columns[index];
        let operand = match bind_scalar_operand(&column.logical_type, &check.operand)? {
            BoundScalarOperand::Literal(value) if !matches!(value, SqlValue::Null) => value,
            BoundScalarOperand::Literal(_) | BoundScalarOperand::Parameter(_) => {
                return Err(SqlError::InvalidSyntax);
            }
        };
        checks.push((
            column.id,
            ColumnCheckConstraint {
                operator: catalog_check_operator(check.operator),
                operand,
            },
        ));
    }
    let mut definition = RelationDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Relational,
            name: qualified_name(name).map_err(NativeRuntimeError::from)?,
        },
        columns,
        primary_key,
        checks,
    };
    if is_legacy_binary_relation(&definition) {
        for column in &mut definition.columns {
            column.nullable = false;
        }
    }
    definition.validate().map_err(NativeRuntimeError::from)?;
    transaction.create_relation_definition(definition)?;
    Ok(SqlResult::Command {
        rows_affected: 0,
        object_id: Some(id),
    })
}

fn execute_insert(
    transaction: &mut NativeWriteBatch,
    name: &str,
    supplied_values: &[ColumnOperand],
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let definition = definition.clone();
    let resolved =
        resolve_mutation_operands(&definition, supplied_values, parameter_count, parameters)?;
    let values = bind_insert_values(&definition, supplied_values, &resolved)?;
    if is_legacy_binary_relation(&definition) {
        let primary_key = legacy_binary_value(values[0], false)?;
        let row = legacy_binary_value(values[1], false)?;
        transaction
            .insert(table, primary_key, row)
            .map_err(map_runtime_error)?;
    } else {
        let primary_key = encode_primary_key(&definition, &values)?;
        let tuple = encode_tuple(&definition, &values)?;
        transaction
            .insert(table, primary_key, tuple)
            .map_err(map_runtime_error)?;
    }
    Ok(SqlResult::Command {
        rows_affected: 1,
        object_id: None,
    })
}

fn execute_update(
    transaction: &mut NativeWriteBatch,
    name: &str,
    assignments: &[ColumnOperand],
    predicates: &[ColumnOperand],
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let definition = definition.clone();
    let assignment_columns = bind_update_columns(&definition, assignments)?;
    let predicate_columns = bind_primary_key_columns(&definition, predicates)?;
    let assignment_values =
        resolve_mutation_operands(&definition, assignments, parameter_count, parameters)?;
    let predicate_values =
        resolve_mutation_operands(&definition, predicates, parameter_count, parameters)?;
    let primary_key = bind_primary_key(&definition, &predicate_columns, &predicate_values)?;
    let update = if is_legacy_binary_relation(&definition) {
        if assignment_columns.as_slice() != [1] {
            return Err(SqlError::InvalidSyntax);
        }
        legacy_binary_value(assignment_values.first(), false)?
    } else {
        let assignments =
            bind_update_assignments(&definition, &assignment_columns, &assignment_values)?;
        let Some(stored) = transaction.select(table, &primary_key) else {
            return Ok(command_result(0));
        };
        encode_updated_tuple(&definition, &assignments, stored)?
    };
    if transaction.select(table, &primary_key).is_none() {
        return Ok(command_result(0));
    }
    transaction
        .update(table, primary_key, update)
        .map_err(map_runtime_error)?;
    Ok(command_result(1))
}

fn execute_delete(
    transaction: &mut NativeWriteBatch,
    name: &str,
    predicates: &[ColumnOperand],
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let predicate_columns = bind_primary_key_columns(definition, predicates)?;
    let predicate_values =
        resolve_mutation_operands(definition, predicates, parameter_count, parameters)?;
    let primary_key = bind_primary_key(definition, &predicate_columns, &predicate_values)?;
    if transaction.select(table, &primary_key).is_none() {
        return Ok(command_result(0));
    }
    transaction
        .delete(table, primary_key)
        .map_err(map_runtime_error)?;
    Ok(command_result(1))
}

fn command_result(rows_affected: u64) -> SqlResult {
    SqlResult::Command {
        rows_affected,
        object_id: None,
    }
}

fn execute_select(
    transaction: &NativeWriteBatch,
    query: SelectQuery<'_>,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let ordered_secondary_indexes = transaction_ordered_secondary_indexes(transaction);
    let bound = bind_select(
        &transaction.state.catalog,
        query,
        &ordered_secondary_indexes,
    )?;
    let BoundSelect {
        table,
        projection,
        filter,
        parameter_count,
        residual,
        output_columns,
        access,
    } = bound;
    let definition = relation_by_id(&transaction.state.catalog, table)?;
    validate_filter_parameters(definition, filter.as_ref(), parameter_count, parameters)?;
    let context = TransactionSelectContext {
        transaction,
        table,
        definition,
        projection: &projection,
        filter: filter.as_ref(),
        residual,
        parameters,
    };
    let rows = match access {
        SelectAccess::PrimaryKey { key, legacy_binary } => {
            context.primary_key_rows(&key, legacy_binary)?
        }
        SelectAccess::SecondaryIndex { index, key, limit } => {
            context.secondary_index_rows(index, &key, limit)?
        }
        SelectAccess::SecondaryIndexRangeScan {
            index,
            range,
            limit,
        } => context.secondary_index_range_rows(index, &range, limit)?,
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary,
        } => context.scan_rows(limit, legacy_binary)?,
        SelectAccess::PrimaryKeyPrefixScan { prefix, limit } => {
            context.prefix_rows(&prefix, limit)?
        }
        SelectAccess::PrimaryKeyPrefixRangeScan { range, limit } => {
            context.prefix_range_rows(&range, limit)?
        }
        SelectAccess::PrimaryKeyRangeScan {
            range,
            limit,
            legacy_binary,
        } => context.range_rows(
            &range,
            ScanExecution {
                limit,
                legacy_binary,
            },
        )?,
    };
    Ok(SqlResult::Rows {
        columns: output_columns,
        rows,
    })
}

fn transaction_ordered_secondary_indexes(transaction: &NativeWriteBatch) -> BTreeSet<ObjectId> {
    transaction
        .state
        .relational
        .indexes
        .iter()
        .filter_map(|(index, state)| {
            (state.layout == SecondaryIndexLayout::OrderedV2).then_some(*index)
        })
        .collect()
}

struct TransactionSelectContext<'context> {
    transaction: &'context NativeWriteBatch,
    table: ObjectId,
    definition: &'context RelationDefinition,
    projection: &'context [usize],
    filter: Option<&'context BoundFilterExpression>,
    residual: bool,
    parameters: &'context [SqlValue],
}

impl TransactionSelectContext<'_> {
    fn primary_key_rows(
        &self,
        key: &KeyBinding,
        legacy_binary: bool,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        if key_contains_null(key, self.parameters)? {
            return Ok(Vec::new());
        }
        let primary_key = bind_primary_key_binding(self.definition, key, self.parameters)?;
        self.transaction
            .select(self.table, &primary_key)
            .map_or_else(
                || Ok(Vec::new()),
                |stored| {
                    materialize_filtered_row(
                        self.definition,
                        self.projection,
                        legacy_binary,
                        &primary_key,
                        stored,
                        self.filter,
                        self.parameters,
                    )
                    .map(|row| row.into_iter().collect())
                },
            )
    }

    fn secondary_index_rows(
        &self,
        index: ObjectId,
        key: &KeyBinding,
        limit: Option<usize>,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        let definition = secondary_index_by_id(&self.transaction.state.catalog, index)?;
        let Some(index_key) =
            bind_secondary_index_key_binding(self.definition, definition, key, self.parameters)?
        else {
            return Ok(Vec::new());
        };
        let Some(primary_keys) = self
            .transaction
            .state
            .relational
            .secondary_index_lookup(index, &index_key)
            .map_err(NativeRuntimeError::from)?
        else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::with_capacity(primary_keys.len());
        for primary_key in primary_keys {
            let stored = self
                .transaction
                .select(self.table, primary_key)
                .ok_or(SqlError::InvalidStoredRow)?;
            if let Some(row) = materialize_filtered_row(
                self.definition,
                self.projection,
                false,
                primary_key,
                stored,
                self.filter,
                self.parameters,
            )? {
                rows.push(row);
                if limit.is_some_and(|limit| rows.len() == limit) {
                    break;
                }
            }
        }
        Ok(rows)
    }

    fn secondary_index_range_rows(
        &self,
        index: ObjectId,
        range: &SecondaryIndexRange,
        limit: usize,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        let definition = secondary_index_by_id(&self.transaction.state.catalog, index)?;
        let Some((lower, upper)) =
            bind_secondary_index_range(self.definition, definition, range, self.parameters)?
        else {
            return Ok(Vec::new());
        };
        if key_range_is_empty(&lower, &upper) || limit == 0 {
            return Ok(Vec::new());
        }
        let index_state = self
            .transaction
            .state
            .relational
            .indexes
            .get(&index)
            .ok_or(SqlError::InvalidCatalogObject)?;
        if index_state.layout != SecondaryIndexLayout::OrderedV2
            || index_state.relation != self.table
        {
            return Err(SqlError::InvalidCatalogObject);
        }
        let index_columns = secondary_index_column_indices(self.definition, definition)?;
        let row_filter = if self.residual { self.filter } else { None };
        let mut rows = Vec::with_capacity(limit.min(256));
        for (index_key, primary_keys) in index_state.entries.range((lower, upper)) {
            for primary_key in primary_keys {
                let stored = self
                    .transaction
                    .select(self.table, primary_key)
                    .ok_or(SqlError::InvalidStoredRow)?;
                let values = decode_complete_row(self.definition, false, primary_key, stored)?;
                validate_secondary_index_values(
                    self.definition,
                    &index_columns,
                    index_key,
                    &values,
                )?;
                if let Some(row) = materialize_decoded_row(
                    self.definition,
                    self.projection,
                    &values,
                    row_filter,
                    self.parameters,
                )? {
                    rows.push(row);
                    if rows.len() == limit {
                        return Ok(rows);
                    }
                }
            }
        }
        Ok(rows)
    }

    fn scan_rows(&self, limit: usize, legacy_binary: bool) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let stored_rows = self
            .transaction
            .state
            .relational
            .tables
            .get(&self.table)
            .ok_or(SqlError::InvalidStoredRow)?;
        self.collect_rows(stored_rows, limit, legacy_binary)
    }

    fn prefix_rows(
        &self,
        prefix: &KeyBinding,
        limit: usize,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        let Some(prefix) = bind_primary_key_prefix(self.definition, prefix, self.parameters)?
        else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let stored_rows = self
            .transaction
            .state
            .relational
            .tables
            .get(&self.table)
            .ok_or(SqlError::InvalidStoredRow)?;
        self.collect_rows(
            stored_rows.range(primary_key_prefix_bounds(prefix)),
            limit,
            false,
        )
    }

    fn prefix_range_rows(
        &self,
        range: &PrimaryKeyPrefixRange,
        limit: usize,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        let Some((lower, upper)) =
            bind_primary_key_prefix_range(self.definition, range, self.parameters)?
        else {
            return Ok(Vec::new());
        };
        if key_range_is_empty(&lower, &upper) || limit == 0 {
            return Ok(Vec::new());
        }
        let stored_rows = self
            .transaction
            .state
            .relational
            .tables
            .get(&self.table)
            .ok_or(SqlError::InvalidStoredRow)?;
        self.collect_rows(stored_rows.range((lower, upper)), limit, false)
    }

    fn range_rows(
        &self,
        range: &PrimaryKeyRange,
        execution: ScanExecution,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        let (lower, upper) = bind_primary_key_range(self.definition, range, self.parameters)?;
        let stored_rows = self
            .transaction
            .state
            .relational
            .tables
            .get(&self.table)
            .ok_or(SqlError::InvalidStoredRow)?;
        if key_range_is_empty(&lower, &upper) || execution.limit == 0 {
            return Ok(Vec::new());
        }
        self.collect_rows(
            stored_rows.range((lower, upper)),
            execution.limit,
            execution.legacy_binary,
        )
    }

    fn collect_rows<'row>(
        &self,
        stored_rows: impl IntoIterator<Item = (&'row Vec<u8>, &'row Vec<u8>)>,
        limit: usize,
        legacy_binary: bool,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        let mut rows = Vec::with_capacity(limit.min(256));
        for (primary_key, stored) in stored_rows {
            if let Some(row) = materialize_filtered_row(
                self.definition,
                self.projection,
                legacy_binary,
                primary_key,
                stored,
                self.filter,
                self.parameters,
            )? {
                rows.push(row);
                if rows.len() == limit {
                    break;
                }
            }
        }
        Ok(rows)
    }
}

fn bind_select(
    catalog: &crate::model::CatalogState,
    query: SelectQuery<'_>,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
) -> Result<BoundSelect, SqlError> {
    let (table, definition) = relation_named(catalog, query.name)?;
    let projection = bind_projection(definition, query.projection)?;
    let filter = query
        .filter
        .map(|expression| bind_filter_expression(definition, expression))
        .transpose()?;
    let total_terms = filter.as_ref().map_or(0, filter_term_count);
    let (access, used_terms) = bind_select_access(
        catalog,
        table,
        definition,
        query,
        filter.as_ref(),
        ordered_secondary_indexes,
    )?;
    Ok(BoundSelect {
        table,
        output_columns: projection_output_columns(definition, &projection),
        projection,
        filter,
        parameter_count: query.parameter_count,
        residual: total_terms > used_terms,
        access,
    })
}

fn bind_select_access(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    definition: &RelationDefinition,
    query: SelectQuery<'_>,
    filter: Option<&BoundFilterExpression>,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
) -> Result<(SelectAccess, usize), SqlError> {
    let expected_primary_key = primary_key_indices(definition)?;
    let legacy_binary = is_legacy_binary_relation(definition);
    let mut comparisons = Vec::new();
    if let Some(filter) = filter {
        collect_top_level_comparisons(filter, &mut comparisons);
        let secondary_range_columns =
            ordered_secondary_index_columns(catalog, table, definition, ordered_secondary_indexes)?;
        validate_row_comparison_shapes(
            &comparisons,
            &expected_primary_key,
            &secondary_range_columns,
        )?;
    }
    let (access, used_terms) = if filter.is_none() {
        let limit = query.limit.ok_or(SqlError::InvalidSyntax)?;
        validate_primary_key_order(definition, query.order_by, &expected_primary_key)?;
        (
            SelectAccess::PrimaryKeyScan {
                limit,
                legacy_binary,
            },
            0,
        )
    } else if let Some(key) = find_equality_key(&comparisons, &expected_primary_key) {
        if !query.order_by.is_empty() || query.limit.is_some() {
            return Err(SqlError::InvalidSyntax);
        }
        let used_terms = key.columns.len();
        (SelectAccess::PrimaryKey { key, legacy_binary }, used_terms)
    } else if let Some((index, key)) =
        find_secondary_equality_key(catalog, table, definition, &comparisons)?
    {
        validate_primary_key_order(definition, query.order_by, &expected_primary_key)?;
        let used_terms = key.columns.len();
        (
            SelectAccess::SecondaryIndex {
                index,
                key,
                limit: query.limit,
            },
            used_terms,
        )
    } else if let Some((range, range_terms)) =
        bind_primary_key_range_shape(&comparisons, &expected_primary_key, query.parameter_count)?
    {
        let limit = query.limit.ok_or(SqlError::InvalidSyntax)?;
        validate_primary_key_order(definition, query.order_by, &expected_primary_key)?;
        (
            SelectAccess::PrimaryKeyRangeScan {
                range,
                limit,
                legacy_binary,
            },
            range_terms,
        )
    } else if let Some(access) = bind_primary_key_prefix_range_access(
        definition,
        &comparisons,
        &expected_primary_key,
        query.order_by,
        query.limit,
    )? {
        access
    } else if let Some(prefix) = find_key_prefix(&comparisons, &expected_primary_key) {
        let limit = query.limit.ok_or(SqlError::InvalidSyntax)?;
        validate_primary_key_order(definition, query.order_by, &expected_primary_key)?;
        let used_terms = prefix.columns.len();
        (
            SelectAccess::PrimaryKeyPrefixScan { prefix, limit },
            used_terms,
        )
    } else if let Some(access) = bind_secondary_index_range_access(
        catalog,
        table,
        definition,
        &comparisons,
        query.parameter_count,
        query.order_by,
        query.limit,
        ordered_secondary_indexes,
    )? {
        access
    } else {
        bind_primary_scan_fallback(
            definition,
            &comparisons,
            &expected_primary_key,
            query,
            legacy_binary,
        )?
    };
    Ok((access, used_terms))
}

fn bind_primary_scan_fallback(
    definition: &RelationDefinition,
    comparisons: &[&BoundFilterExpression],
    expected_primary_key: &[usize],
    query: SelectQuery<'_>,
    legacy_binary: bool,
) -> Result<(SelectAccess, usize), SqlError> {
    let Some(limit) = query.limit else {
        let incomplete_primary_key = comparisons.iter().any(|predicate| {
            matches!(
                predicate,
                BoundFilterExpression::Comparison { columns, .. }
                    if columns.len() == 1 && expected_primary_key.contains(&columns[0])
            )
        });
        return Err(if incomplete_primary_key {
            SqlError::InvalidPrimaryKey
        } else {
            SqlError::NoAccessPath
        });
    };
    validate_primary_key_order(definition, query.order_by, expected_primary_key)?;
    Ok((
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary,
        },
        0,
    ))
}

fn projection_output_columns(definition: &RelationDefinition, projection: &[usize]) -> Vec<String> {
    projection
        .iter()
        .map(|index| definition.columns[*index].name.display().to_owned())
        .collect()
}

fn bind_projection(
    definition: &RelationDefinition,
    projection: &Projection,
) -> Result<Vec<usize>, SqlError> {
    match projection {
        Projection::All => Ok((0..definition.columns.len()).collect()),
        Projection::Columns(names) => names
            .iter()
            .map(|name| column_index(&definition.columns, name))
            .collect(),
    }
}

fn bind_indexed_inner_join(
    catalog: &crate::model::CatalogState,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
    parsed: &ParsedInnerJoin,
) -> Result<PreparedPlan, SqlError> {
    let all_columns = Projection::All;
    let bound = bind_select(
        catalog,
        SelectQuery {
            name: &parsed.left_name,
            projection: &all_columns,
            filter: parsed.filter.as_ref(),
            parameter_count: parsed.parameter_count,
            order_by: &parsed.order_by,
            limit: parsed.limit,
        },
        ordered_secondary_indexes,
    )?;
    let left_relation = relation_by_id(catalog, bound.table)?.clone();
    if is_legacy_binary_relation(&left_relation) {
        return Err(SqlError::InvalidSyntax);
    }
    let left_access = bind_join_left_access(catalog, bound.access)?;
    let left_filter = bound.filter;
    let (right_table, right_relation) = relation_named(catalog, &parsed.right_name)?;
    if is_legacy_binary_relation(right_relation) {
        return Err(SqlError::InvalidSyntax);
    }
    let join_equalities = bind_join_equalities(parsed, &left_relation, right_relation)?;
    let (right_access, left_join_columns) =
        bind_join_right_access(catalog, right_table, right_relation, &join_equalities)?;
    let right_join_columns = match &right_access {
        JoinRightAccess::PrimaryKey { columns }
        | JoinRightAccess::UniqueSecondaryIndex { columns, .. } => columns,
    };
    for (left_column, right_column) in left_join_columns.iter().zip(right_join_columns) {
        if left_relation.columns[*left_column].logical_type
            != right_relation.columns[*right_column].logical_type
        {
            return Err(SqlError::TypeMismatch);
        }
    }
    let mut projection = Vec::with_capacity(parsed.projection.len());
    let mut output_columns = Vec::with_capacity(parsed.projection.len());
    for reference in &parsed.projection {
        let (bound, output) = bind_join_projection(
            reference,
            &parsed.left_name,
            &left_relation,
            &parsed.right_name,
            right_relation,
        )?;
        projection.push(bound);
        output_columns.push(output);
    }
    Ok(PreparedPlan::IndexedInnerJoin {
        left_table: bound.table,
        right_table,
        left_relation: Box::new(left_relation),
        right_relation: Box::new(right_relation.clone()),
        left_access,
        left_filter,
        left_join_columns,
        right_access,
        projection,
        parameter_count: parsed.parameter_count,
        output_columns,
    })
}

fn bind_join_left_access(
    catalog: &crate::model::CatalogState,
    access: SelectAccess,
) -> Result<JoinLeftAccess, SqlError> {
    Ok(match access {
        SelectAccess::PrimaryKey {
            key,
            legacy_binary: false,
        } => JoinLeftAccess::PrimaryKey { key },
        SelectAccess::SecondaryIndex { index, key, limit } => {
            let definition = secondary_index_by_id(catalog, index)?;
            match (definition.unique, limit) {
                (true, None) => JoinLeftAccess::UniqueSecondaryIndex {
                    index,
                    definition: Box::new(definition.clone()),
                    key,
                },
                (_, Some(limit)) => JoinLeftAccess::BoundedSecondaryIndex {
                    index,
                    definition: Box::new(definition.clone()),
                    key,
                    limit,
                },
                (false, None) => return Err(SqlError::NoAccessPath),
            }
        }
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary: false,
        } => JoinLeftAccess::BoundedPrimaryKeyScan { range: None, limit },
        SelectAccess::PrimaryKeyPrefixScan { limit, .. }
        | SelectAccess::PrimaryKeyPrefixRangeScan { limit, .. } => {
            JoinLeftAccess::BoundedPrimaryKeyScan { range: None, limit }
        }
        SelectAccess::PrimaryKeyRangeScan {
            range,
            limit,
            legacy_binary: false,
        } => JoinLeftAccess::BoundedPrimaryKeyScan {
            range: Some(range),
            limit,
        },
        SelectAccess::PrimaryKey { .. }
        | SelectAccess::SecondaryIndexRangeScan { .. }
        | SelectAccess::PrimaryKeyScan { .. }
        | SelectAccess::PrimaryKeyRangeScan { .. } => return Err(SqlError::NoAccessPath),
    })
}

fn bind_join_equalities(
    parsed: &ParsedInnerJoin,
    left_relation: &RelationDefinition,
    right_relation: &RelationDefinition,
) -> Result<Vec<(usize, usize)>, SqlError> {
    let mut bound = Vec::with_capacity(parsed.equalities.len());
    for equality in &parsed.equalities {
        let left_column =
            qualified_column(&equality.left_column, &parsed.left_name, left_relation)?;
        let right_column =
            qualified_column(&equality.right_column, &parsed.right_name, right_relation)?;
        if bound.iter().any(|(bound_left, bound_right)| {
            *bound_left == left_column || *bound_right == right_column
        }) {
            return Err(SqlError::DuplicateColumn);
        }
        bound.push((left_column, right_column));
    }
    Ok(bound)
}

fn bind_join_right_access(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    relation: &RelationDefinition,
    equalities: &[(usize, usize)],
) -> Result<(JoinRightAccess, Vec<usize>), SqlError> {
    let primary_key = primary_key_indices(relation)?;
    if let Some(left_columns) = align_join_columns(&primary_key, equalities) {
        return Ok((
            JoinRightAccess::PrimaryKey {
                columns: primary_key,
            },
            left_columns,
        ));
    }
    for (index, object) in &catalog.objects {
        let CatalogObject::SecondaryIndex(definition) = object else {
            continue;
        };
        if definition.relation != table || !definition.unique {
            continue;
        }
        let columns = secondary_index_column_indices(relation, definition)?;
        if let Some(left_columns) = align_join_columns(&columns, equalities) {
            return Ok((
                JoinRightAccess::UniqueSecondaryIndex {
                    index: *index,
                    columns,
                },
                left_columns,
            ));
        }
    }
    Err(SqlError::NoAccessPath)
}

fn align_join_columns(
    right_key_columns: &[usize],
    equalities: &[(usize, usize)],
) -> Option<Vec<usize>> {
    if right_key_columns.len() != equalities.len() {
        return None;
    }
    right_key_columns
        .iter()
        .map(|right_key_column| {
            equalities.iter().find_map(|(left_column, right_column)| {
                (*right_column == *right_key_column).then_some(*left_column)
            })
        })
        .collect()
}

fn qualified_column(
    reference: &str,
    relation_name: &str,
    definition: &RelationDefinition,
) -> Result<usize, SqlError> {
    let (qualifier, column) = split_qualified_column(reference)?;
    if normalize_identifier(qualifier) != normalize_identifier(relation_name) {
        return Err(SqlError::UnknownColumn);
    }
    column_index(&definition.columns, column)
}

fn bind_join_projection(
    reference: &str,
    left_name: &str,
    left: &RelationDefinition,
    right_name: &str,
    right: &RelationDefinition,
) -> Result<(JoinProjection, String), SqlError> {
    let (qualifier, column_name) = split_qualified_column(reference)?;
    let (side, column, relation_display, column_display) =
        if normalize_identifier(qualifier) == normalize_identifier(left_name) {
            let column = column_index(&left.columns, column_name)?;
            (
                JoinSide::Left,
                column,
                left.header.name.object.display(),
                left.columns[column].name.display(),
            )
        } else if normalize_identifier(qualifier) == normalize_identifier(right_name) {
            let column = column_index(&right.columns, column_name)?;
            (
                JoinSide::Right,
                column,
                right.header.name.object.display(),
                right.columns[column].name.display(),
            )
        } else {
            return Err(SqlError::UnknownColumn);
        };
    Ok((
        JoinProjection { side, column },
        format!("{relation_display}.{column_display}"),
    ))
}

fn split_qualified_column(reference: &str) -> Result<(&str, &str), SqlError> {
    let (qualifier, column) = reference.split_once('.').ok_or(SqlError::InvalidSyntax)?;
    if qualifier.is_empty() || column.is_empty() || column.contains('.') {
        return Err(SqlError::InvalidSyntax);
    }
    Ok((qualifier, column))
}

fn validate_primary_key_order(
    definition: &RelationDefinition,
    order_by: &[String],
    expected_primary_key: &[usize],
) -> Result<(), SqlError> {
    if order_by.is_empty() {
        return Ok(());
    }
    let ordered_columns = order_by
        .iter()
        .map(|name| column_index(&definition.columns, name))
        .collect::<Result<Vec<_>, _>>()?;
    if ordered_columns == expected_primary_key {
        Ok(())
    } else {
        Err(SqlError::InvalidPrimaryKey)
    }
}

fn validate_secondary_index_order(
    definition: &RelationDefinition,
    order_by: &[String],
    expected_index_columns: &[usize],
) -> Result<(), SqlError> {
    if order_by.is_empty() {
        return Ok(());
    }
    let ordered_columns = order_by
        .iter()
        .map(|name| column_index(&definition.columns, name))
        .collect::<Result<Vec<_>, _>>()?;
    if ordered_columns == expected_index_columns {
        Ok(())
    } else {
        Err(SqlError::InvalidSecondaryIndexRange)
    }
}

fn bind_filter_expression(
    definition: &RelationDefinition,
    expression: &FilterExpression,
) -> Result<BoundFilterExpression, SqlError> {
    match expression {
        FilterExpression::Comparison {
            columns,
            operator,
            operands,
        } => {
            let columns = columns
                .iter()
                .map(|name| column_index(&definition.columns, name))
                .collect::<Result<Vec<_>, _>>()?;
            let operands = columns
                .iter()
                .zip(operands)
                .map(|(column, operand)| {
                    bind_scalar_operand(&definition.columns[*column].logical_type, operand)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundFilterExpression::Comparison {
                columns,
                operator: *operator,
                operands,
            })
        }
        FilterExpression::IsNull { column, negated } => Ok(BoundFilterExpression::IsNull {
            column: column_index(&definition.columns, column)?,
            negated: *negated,
        }),
        FilterExpression::And(left, right) => Ok(BoundFilterExpression::And(
            Box::new(bind_filter_expression(definition, left)?),
            Box::new(bind_filter_expression(definition, right)?),
        )),
        FilterExpression::Or(left, right) => Ok(BoundFilterExpression::Or(
            Box::new(bind_filter_expression(definition, left)?),
            Box::new(bind_filter_expression(definition, right)?),
        )),
        FilterExpression::Not(expression) => Ok(BoundFilterExpression::Not(Box::new(
            bind_filter_expression(definition, expression)?,
        ))),
    }
}

fn bind_scalar_operand(
    logical_type: &LogicalType,
    operand: &ScalarOperand,
) -> Result<BoundScalarOperand, SqlError> {
    let value = match operand {
        ScalarOperand::Parameter(position) => {
            return Ok(BoundScalarOperand::Parameter(*position));
        }
        ScalarOperand::Null => SqlValue::Null,
        ScalarOperand::Boolean(value) if *logical_type == LogicalType::Boolean => {
            SqlValue::Boolean(*value)
        }
        ScalarOperand::Text(value) if *logical_type == LogicalType::Text => {
            SqlValue::Text(value.clone())
        }
        ScalarOperand::Integer(value) => match logical_type {
            LogicalType::Signed(_) => {
                SqlValue::Signed(value.parse::<i64>().map_err(|_| SqlError::TypeMismatch)?)
            }
            LogicalType::Unsigned(_) => {
                SqlValue::Unsigned(value.parse::<u64>().map_err(|_| SqlError::TypeMismatch)?)
            }
            _ => return Err(SqlError::TypeMismatch),
        },
        ScalarOperand::Boolean(_) | ScalarOperand::Text(_) => {
            return Err(SqlError::TypeMismatch);
        }
    };
    if !matches!(value, SqlValue::Null) {
        value
            .encode_ordered_component(logical_type)
            .map_err(|_| SqlError::TypeMismatch)?;
    }
    Ok(BoundScalarOperand::Literal(value))
}

fn collect_top_level_comparisons<'filter>(
    expression: &'filter BoundFilterExpression,
    comparisons: &mut Vec<&'filter BoundFilterExpression>,
) {
    match expression {
        BoundFilterExpression::And(left, right) => {
            collect_top_level_comparisons(left, comparisons);
            collect_top_level_comparisons(right, comparisons);
        }
        BoundFilterExpression::Comparison { .. } => comparisons.push(expression),
        BoundFilterExpression::IsNull { .. }
        | BoundFilterExpression::Or(_, _)
        | BoundFilterExpression::Not(_) => {}
    }
}

fn validate_row_comparison_shapes(
    comparisons: &[&BoundFilterExpression],
    expected_primary_key: &[usize],
    secondary_range_columns: &[Vec<usize>],
) -> Result<(), SqlError> {
    for comparison in comparisons {
        let BoundFilterExpression::Comparison {
            columns, operator, ..
        } = comparison
        else {
            continue;
        };
        if columns.len() <= 1 {
            continue;
        }
        let range_operator = matches!(
            operator,
            ComparisonOperator::Less
                | ComparisonOperator::LessOrEqual
                | ComparisonOperator::Greater
                | ComparisonOperator::GreaterOrEqual
        );
        if !range_operator {
            return Err(SqlError::InvalidPrimaryKey);
        }
        if columns != expected_primary_key
            && !secondary_range_columns
                .iter()
                .any(|index_columns| index_columns == columns)
        {
            return Err(SqlError::InvalidSecondaryIndexRange);
        }
    }
    Ok(())
}

fn filter_term_count(expression: &BoundFilterExpression) -> usize {
    match expression {
        BoundFilterExpression::Comparison { .. } | BoundFilterExpression::IsNull { .. } => 1,
        BoundFilterExpression::And(left, right) | BoundFilterExpression::Or(left, right) => {
            filter_term_count(left).saturating_add(filter_term_count(right))
        }
        BoundFilterExpression::Not(expression) => filter_term_count(expression),
    }
}

fn find_equality_key(
    comparisons: &[&BoundFilterExpression],
    key_columns: &[usize],
) -> Option<KeyBinding> {
    let mut operands = Vec::with_capacity(key_columns.len());
    for key_column in key_columns {
        let matching = comparisons
            .iter()
            .filter_map(|comparison| {
                let BoundFilterExpression::Comparison {
                    columns,
                    operator: ComparisonOperator::Equal,
                    operands,
                } = comparison
                else {
                    return None;
                };
                (columns.as_slice() == [*key_column])
                    .then_some(operands.as_slice())
                    .and_then(|operands| match operands {
                        [operand] => Some(operand.clone()),
                        _ => None,
                    })
            })
            .collect::<Vec<_>>();
        let [operand] = matching.as_slice() else {
            return None;
        };
        operands.push(operand.clone());
    }
    Some(KeyBinding {
        columns: key_columns.to_vec(),
        operands,
    })
}

fn find_key_prefix(
    comparisons: &[&BoundFilterExpression],
    key_columns: &[usize],
) -> Option<KeyBinding> {
    let mut columns = Vec::with_capacity(key_columns.len().saturating_sub(1));
    let mut operands = Vec::with_capacity(key_columns.len().saturating_sub(1));
    for key_column in key_columns {
        let matching = comparisons
            .iter()
            .filter_map(|comparison| {
                let BoundFilterExpression::Comparison {
                    columns,
                    operator: ComparisonOperator::Equal,
                    operands,
                } = comparison
                else {
                    return None;
                };
                (columns.as_slice() == [*key_column])
                    .then_some(operands.as_slice())
                    .and_then(|operands| match operands {
                        [operand] => Some(operand.clone()),
                        _ => None,
                    })
            })
            .collect::<Vec<_>>();
        let [operand] = matching.as_slice() else {
            break;
        };
        columns.push(*key_column);
        operands.push(operand.clone());
    }
    (!columns.is_empty() && columns.len() < key_columns.len())
        .then_some(KeyBinding { columns, operands })
}

fn bind_primary_key_prefix_range_shape(
    comparisons: &[&BoundFilterExpression],
    primary_key: &[usize],
) -> Result<Option<(PrimaryKeyPrefixRange, usize)>, SqlError> {
    let Some(prefix) = find_key_prefix(comparisons, primary_key) else {
        return Ok(None);
    };
    let range_column = primary_key
        .get(prefix.columns.len())
        .copied()
        .ok_or(SqlError::InvalidPrimaryKey)?;
    let mut lower = None;
    let mut upper = None;
    let mut range_terms = 0_usize;
    for predicate in comparisons {
        let BoundFilterExpression::Comparison {
            columns,
            operator,
            operands,
        } = predicate
        else {
            continue;
        };
        if columns.as_slice() != [range_column] {
            continue;
        }
        let [operand] = operands.as_slice() else {
            return Err(SqlError::InvalidPrimaryKey);
        };
        let endpoint = PrimaryKeyPrefixRangeEndpoint {
            operand: operand.clone(),
            inclusive: matches!(
                operator,
                ComparisonOperator::GreaterOrEqual | ComparisonOperator::LessOrEqual
            ),
        };
        match operator {
            ComparisonOperator::Greater | ComparisonOperator::GreaterOrEqual => {
                if lower.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
                range_terms = range_terms.saturating_add(1);
            }
            ComparisonOperator::Less | ComparisonOperator::LessOrEqual => {
                if upper.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
                range_terms = range_terms.saturating_add(1);
            }
            ComparisonOperator::Equal | ComparisonOperator::NotEqual => {}
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(None);
    }
    let used_terms = prefix.columns.len().saturating_add(range_terms);
    Ok(Some((
        PrimaryKeyPrefixRange {
            prefix,
            range_column,
            lower,
            upper,
        },
        used_terms,
    )))
}

fn bind_primary_key_prefix_range_access(
    definition: &RelationDefinition,
    comparisons: &[&BoundFilterExpression],
    primary_key: &[usize],
    order_by: &[String],
    limit: Option<usize>,
) -> Result<Option<(SelectAccess, usize)>, SqlError> {
    let Some((range, range_terms)) = bind_primary_key_prefix_range_shape(comparisons, primary_key)?
    else {
        return Ok(None);
    };
    let limit = limit.ok_or(SqlError::InvalidSyntax)?;
    validate_primary_key_order(definition, order_by, primary_key)?;
    Ok(Some((
        SelectAccess::PrimaryKeyPrefixRangeScan { range, limit },
        range_terms,
    )))
}

fn find_secondary_equality_key(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    definition: &RelationDefinition,
    comparisons: &[&BoundFilterExpression],
) -> Result<Option<(ObjectId, KeyBinding)>, SqlError> {
    for (id, object) in &catalog.objects {
        let CatalogObject::SecondaryIndex(index) = object else {
            continue;
        };
        if index.relation != table {
            continue;
        }
        let columns = secondary_index_column_indices(definition, index)?;
        if let Some(key) = find_equality_key(comparisons, &columns) {
            return Ok(Some((*id, key)));
        }
    }
    Ok(None)
}

fn ordered_secondary_index_columns(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    definition: &RelationDefinition,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
) -> Result<Vec<Vec<usize>>, SqlError> {
    catalog
        .objects
        .iter()
        .filter_map(|(id, object)| {
            let CatalogObject::SecondaryIndex(index) = object else {
                return None;
            };
            (index.relation == table && ordered_secondary_indexes.contains(id))
                .then_some(secondary_index_column_indices(definition, index))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn bind_secondary_index_range_access(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    definition: &RelationDefinition,
    comparisons: &[&BoundFilterExpression],
    parameter_count: usize,
    order_by: &[String],
    limit: Option<usize>,
    ordered_secondary_indexes: &BTreeSet<ObjectId>,
) -> Result<Option<(SelectAccess, usize)>, SqlError> {
    let mut matched_invalid_order = false;
    for (index, object) in &catalog.objects {
        let CatalogObject::SecondaryIndex(index_definition) = object else {
            continue;
        };
        if index_definition.relation != table || !ordered_secondary_indexes.contains(index) {
            continue;
        }
        let columns = secondary_index_column_indices(definition, index_definition)?;
        let bound_range = bind_secondary_index_range_shape(comparisons, &columns, parameter_count)?;
        let (range, used_terms) = if let Some(range) = bound_range {
            range
        } else {
            let Some(range) =
                bind_secondary_index_prefix_range_shape(comparisons, &columns, parameter_count)?
            else {
                continue;
            };
            range
        };
        let limit = limit.ok_or(SqlError::InvalidSyntax)?;
        match validate_secondary_index_order(definition, order_by, &columns) {
            Ok(()) => {}
            Err(SqlError::InvalidSecondaryIndexRange) => {
                matched_invalid_order = true;
                continue;
            }
            Err(error) => return Err(error),
        }
        return Ok(Some((
            SelectAccess::SecondaryIndexRangeScan {
                index: *index,
                range,
                limit,
            },
            used_terms,
        )));
    }
    if matched_invalid_order {
        Err(SqlError::InvalidSecondaryIndexRange)
    } else {
        Ok(None)
    }
}

fn bind_secondary_index_range_shape(
    comparisons: &[&BoundFilterExpression],
    index_columns: &[usize],
    parameter_count: usize,
) -> Result<Option<(SecondaryIndexRange, usize)>, SqlError> {
    let mut lower = None;
    let mut upper = None;
    let mut used_terms = 0_usize;
    for predicate in comparisons {
        let BoundFilterExpression::Comparison {
            columns,
            operator,
            operands,
        } = predicate
        else {
            continue;
        };
        if columns != index_columns {
            continue;
        }
        if operands.len() != index_columns.len() {
            return Err(SqlError::InvalidSecondaryIndexRange);
        }
        let endpoint = SecondaryIndexRangeEndpoint {
            key: KeyBinding {
                columns: columns.clone(),
                operands: operands.clone(),
            },
            inclusive: matches!(
                operator,
                ComparisonOperator::GreaterOrEqual | ComparisonOperator::LessOrEqual
            ),
        };
        match operator {
            ComparisonOperator::Greater | ComparisonOperator::GreaterOrEqual => {
                if lower.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidSecondaryIndexRange);
                }
                used_terms = used_terms.saturating_add(1);
            }
            ComparisonOperator::Less | ComparisonOperator::LessOrEqual => {
                if upper.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidSecondaryIndexRange);
                }
                used_terms = used_terms.saturating_add(1);
            }
            ComparisonOperator::Equal | ComparisonOperator::NotEqual => {}
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(None);
    }
    Ok(Some((
        SecondaryIndexRange {
            kind: SecondaryIndexRangeKind::Complete { lower, upper },
            parameter_count,
        },
        used_terms,
    )))
}

fn bind_secondary_index_prefix_range_shape(
    comparisons: &[&BoundFilterExpression],
    index_columns: &[usize],
    parameter_count: usize,
) -> Result<Option<(SecondaryIndexRange, usize)>, SqlError> {
    let Some(prefix) = find_key_prefix(comparisons, index_columns) else {
        return Ok(None);
    };
    let range_column = index_columns
        .get(prefix.columns.len())
        .copied()
        .ok_or(SqlError::InvalidSecondaryIndexRange)?;
    let mut lower = None;
    let mut upper = None;
    let mut range_terms = 0_usize;
    for predicate in comparisons {
        let BoundFilterExpression::Comparison {
            columns,
            operator,
            operands,
        } = predicate
        else {
            continue;
        };
        if columns.as_slice() != [range_column] {
            continue;
        }
        let [operand] = operands.as_slice() else {
            return Err(SqlError::InvalidSecondaryIndexRange);
        };
        let endpoint = SecondaryIndexPrefixRangeEndpoint {
            operand: operand.clone(),
            inclusive: matches!(
                operator,
                ComparisonOperator::GreaterOrEqual | ComparisonOperator::LessOrEqual
            ),
        };
        match operator {
            ComparisonOperator::Greater | ComparisonOperator::GreaterOrEqual => {
                if lower.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidSecondaryIndexRange);
                }
                range_terms = range_terms.saturating_add(1);
            }
            ComparisonOperator::Less | ComparisonOperator::LessOrEqual => {
                if upper.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidSecondaryIndexRange);
                }
                range_terms = range_terms.saturating_add(1);
            }
            ComparisonOperator::Equal | ComparisonOperator::NotEqual => {}
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(None);
    }
    let used_terms = prefix.columns.len().saturating_add(range_terms);
    Ok(Some((
        SecondaryIndexRange {
            kind: SecondaryIndexRangeKind::Prefix {
                prefix,
                range_column,
                lower,
                upper,
            },
            parameter_count,
        },
        used_terms,
    )))
}

fn bind_primary_key_range_shape(
    comparisons: &[&BoundFilterExpression],
    expected_primary_key: &[usize],
    parameter_count: usize,
) -> Result<Option<(PrimaryKeyRange, usize)>, SqlError> {
    let mut lower = None;
    let mut upper = None;
    let mut used_terms = 0_usize;
    for predicate in comparisons {
        let BoundFilterExpression::Comparison {
            columns,
            operator,
            operands,
        } = predicate
        else {
            continue;
        };
        if columns != expected_primary_key {
            continue;
        }
        let endpoint = PrimaryKeyRangeEndpoint {
            key: KeyBinding {
                columns: columns.clone(),
                operands: operands.clone(),
            },
            inclusive: matches!(
                operator,
                ComparisonOperator::GreaterOrEqual | ComparisonOperator::LessOrEqual
            ),
        };
        match operator {
            ComparisonOperator::Greater | ComparisonOperator::GreaterOrEqual => {
                if lower.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
                used_terms = used_terms.saturating_add(1);
            }
            ComparisonOperator::Less | ComparisonOperator::LessOrEqual => {
                if upper.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
                used_terms = used_terms.saturating_add(1);
            }
            ComparisonOperator::Equal | ComparisonOperator::NotEqual => {}
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(None);
    }
    Ok(Some((
        PrimaryKeyRange {
            lower,
            upper,
            parameter_count,
        },
        used_terms,
    )))
}

fn range_bound_name(endpoint: Option<&PrimaryKeyRangeEndpoint>) -> &'static str {
    match endpoint {
        Some(endpoint) if endpoint.inclusive => "inclusive",
        Some(_) => "exclusive",
        None => "unbounded",
    }
}

fn secondary_range_bound_name(endpoint: Option<&SecondaryIndexRangeEndpoint>) -> &'static str {
    match endpoint {
        Some(endpoint) if endpoint.inclusive => "inclusive",
        Some(_) => "exclusive",
        None => "unbounded",
    }
}

fn secondary_prefix_range_bound_name(
    endpoint: Option<&SecondaryIndexPrefixRangeEndpoint>,
) -> &'static str {
    match endpoint {
        Some(endpoint) if endpoint.inclusive => "inclusive",
        Some(_) => "exclusive",
        None => "unbounded",
    }
}

fn prefix_range_bound_name(endpoint: Option<&PrimaryKeyPrefixRangeEndpoint>) -> &'static str {
    match endpoint {
        Some(endpoint) if endpoint.inclusive => "inclusive",
        Some(_) => "exclusive",
        None => "unbounded",
    }
}

fn same_column_set(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len() && left.iter().all(|column| right.contains(column))
}

fn catalog_check_operator(operator: ComparisonOperator) -> ColumnCheckOperator {
    match operator {
        ComparisonOperator::Equal => ColumnCheckOperator::Equal,
        ComparisonOperator::NotEqual => ColumnCheckOperator::NotEqual,
        ComparisonOperator::Less => ColumnCheckOperator::Less,
        ComparisonOperator::LessOrEqual => ColumnCheckOperator::LessOrEqual,
        ComparisonOperator::Greater => ColumnCheckOperator::Greater,
        ComparisonOperator::GreaterOrEqual => ColumnCheckOperator::GreaterOrEqual,
    }
}

fn validate_checks(
    definition: &RelationDefinition,
    values: &[Option<&SqlValue>],
) -> Result<(), SqlError> {
    for (column_id, check) in &definition.checks {
        let index = definition
            .columns
            .iter()
            .position(|column| column.id == *column_id)
            .ok_or(SqlError::InvalidCatalogObject)?;
        let Some(value) = values.get(index).copied().flatten() else {
            continue;
        };
        if matches!(value, SqlValue::Null) {
            continue;
        }
        let ordering = compare_sql_values(
            &definition.columns[index].logical_type,
            value,
            &check.operand,
        )?
        .ok_or(SqlError::CheckViolation)?;
        let passes = match check.operator {
            ColumnCheckOperator::Equal => ordering == Ordering::Equal,
            ColumnCheckOperator::NotEqual => ordering != Ordering::Equal,
            ColumnCheckOperator::Less => ordering == Ordering::Less,
            ColumnCheckOperator::LessOrEqual => ordering != Ordering::Greater,
            ColumnCheckOperator::Greater => ordering == Ordering::Greater,
            ColumnCheckOperator::GreaterOrEqual => ordering != Ordering::Less,
        };
        if !passes {
            return Err(SqlError::CheckViolation);
        }
    }
    Ok(())
}

fn bind_insert_values<'value>(
    definition: &RelationDefinition,
    supplied_values: &[ColumnOperand],
    resolved: &'value [SqlValue],
) -> Result<Vec<Option<&'value SqlValue>>, SqlError> {
    let mut values = vec![None; definition.columns.len()];
    for (binding, value) in supplied_values.iter().zip(resolved) {
        let index = column_index(&definition.columns, &binding.column)?;
        if values[index].is_some() {
            return Err(SqlError::DuplicateColumn);
        }
        values[index] = Some(value);
    }
    for (index, column) in definition.columns.iter().enumerate() {
        if values[index].is_none() && !column.nullable {
            return Err(SqlError::NullViolation);
        }
        if values[index].is_some_and(|value| matches!(value, SqlValue::Null)) && !column.nullable {
            return Err(SqlError::NullViolation);
        }
    }
    validate_checks(definition, &values)?;
    Ok(values)
}

fn bind_update_columns(
    definition: &RelationDefinition,
    assignments: &[ColumnOperand],
) -> Result<Vec<usize>, SqlError> {
    if assignments.is_empty() {
        return Err(SqlError::InvalidSyntax);
    }
    let mut columns = Vec::with_capacity(assignments.len());
    let primary_key = primary_key_indices(definition)?;
    for assignment in assignments {
        let column = column_index(&definition.columns, &assignment.column)?;
        if columns.contains(&column) {
            return Err(SqlError::DuplicateColumn);
        }
        if primary_key.contains(&column) {
            return Err(SqlError::PrimaryKeyMutationUnsupported);
        }
        columns.push(column);
    }
    Ok(columns)
}

fn bind_primary_key_columns(
    definition: &RelationDefinition,
    predicates: &[ColumnOperand],
) -> Result<Vec<usize>, SqlError> {
    let mut columns = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        let column = column_index(&definition.columns, &predicate.column)?;
        if columns.contains(&column) {
            return Err(SqlError::DuplicateColumn);
        }
        columns.push(column);
    }
    if !same_column_set(&columns, &primary_key_indices(definition)?) {
        return Err(SqlError::InvalidPrimaryKey);
    }
    Ok(columns)
}

fn resolve_mutation_operands(
    definition: &RelationDefinition,
    bindings: &[ColumnOperand],
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<Vec<SqlValue>, SqlError> {
    if parameters.len() != parameter_count {
        return Err(SqlError::ParameterMismatch);
    }
    bindings
        .iter()
        .map(|binding| {
            let column = column_index(&definition.columns, &binding.column)?;
            let definition = definition
                .columns
                .get(column)
                .ok_or(SqlError::InvalidCatalogObject)?;
            let operand = bind_scalar_operand(&definition.logical_type, &binding.operand)?;
            let value = resolve_operand(&operand, parameters)?.clone();
            if !matches!(value, SqlValue::Null) {
                value
                    .encode_storage(&definition.logical_type)
                    .map_err(|_| SqlError::TypeMismatch)?;
            }
            Ok(value)
        })
        .collect()
}

fn bind_update_assignments(
    definition: &RelationDefinition,
    columns: &[usize],
    parameters: &[SqlValue],
) -> Result<Vec<BoundUpdateAssignment>, SqlError> {
    if columns.len() != parameters.len() {
        return Err(SqlError::ParameterMismatch);
    }
    columns
        .iter()
        .copied()
        .zip(parameters)
        .map(|(index, value)| {
            let column = definition
                .columns
                .get(index)
                .ok_or(SqlError::InvalidCatalogObject)?;
            let encoded = if matches!(value, SqlValue::Null) {
                if !column.nullable {
                    return Err(SqlError::NullViolation);
                }
                None
            } else {
                Some(
                    value
                        .encode_storage(&column.logical_type)
                        .map_err(|_| SqlError::TypeMismatch)?,
                )
            };
            Ok(BoundUpdateAssignment {
                column: index,
                value: encoded,
            })
        })
        .collect()
}

fn encode_updated_tuple(
    definition: &RelationDefinition,
    assignments: &[BoundUpdateAssignment],
    stored: &[u8],
) -> Result<Vec<u8>, SqlError> {
    let tuple = RowTupleView::decode(stored).map_err(|_| SqlError::InvalidStoredRow)?;
    if tuple.column_count() != definition.columns.len() {
        return Err(SqlError::InvalidStoredRow);
    }
    let mut values = Vec::with_capacity(definition.columns.len());
    for (index, column) in definition.columns.iter().enumerate() {
        let value = match tuple.value(index).ok_or(SqlError::InvalidStoredRow)? {
            ColumnValueRef::Null => None,
            ColumnValueRef::Bytes(encoded) => {
                SqlValue::decode_storage(&column.logical_type, encoded)
                    .map_err(|_| SqlError::InvalidStoredRow)?;
                Some(encoded.to_vec())
            }
        };
        values.push(value);
    }
    for assignment in assignments {
        values
            .get_mut(assignment.column)
            .ok_or(SqlError::InvalidCatalogObject)?
            .clone_from(&assignment.value);
    }
    let decoded = definition
        .columns
        .iter()
        .zip(&values)
        .map(|(column, value)| {
            value
                .as_ref()
                .map(|encoded| {
                    SqlValue::decode_storage(&column.logical_type, encoded)
                        .map_err(|_| SqlError::InvalidStoredRow)
                })
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let references = decoded.iter().map(Option::as_ref).collect::<Vec<_>>();
    validate_checks(definition, &references)?;
    RowTuple::new(values)
        .and_then(|tuple| tuple.encode())
        .map_err(|_| SqlError::InvalidStoredRow)
}

fn encode_tuple(
    definition: &RelationDefinition,
    values: &[Option<&SqlValue>],
) -> Result<Vec<u8>, SqlError> {
    let mut encoded_values = Vec::with_capacity(values.len());
    for (column, value) in definition.columns.iter().zip(values) {
        match value {
            None | Some(SqlValue::Null) => encoded_values.push(None),
            Some(value) => encoded_values.push(Some(
                value
                    .encode_storage(&column.logical_type)
                    .map_err(|_| SqlError::TypeMismatch)?,
            )),
        }
    }
    RowTuple::new(encoded_values)
        .and_then(|tuple| tuple.encode())
        .map_err(|_| SqlError::InvalidStoredRow)
}

fn encode_primary_key(
    definition: &RelationDefinition,
    values: &[Option<&SqlValue>],
) -> Result<Vec<u8>, SqlError> {
    let mut encoded = Vec::new();
    for index in primary_key_indices(definition)? {
        let value = values[index].ok_or(SqlError::InvalidPrimaryKey)?;
        if matches!(value, SqlValue::Null) {
            return Err(SqlError::InvalidPrimaryKey);
        }
        encoded.extend_from_slice(
            &value
                .encode_ordered_component(&definition.columns[index].logical_type)
                .map_err(|_| SqlError::TypeMismatch)?,
        );
    }
    Ok(encoded)
}

fn bind_primary_key(
    definition: &RelationDefinition,
    parameter_columns: &[usize],
    parameters: &[SqlValue],
) -> Result<Vec<u8>, SqlError> {
    if parameter_columns.len() != parameters.len() {
        return Err(SqlError::ParameterMismatch);
    }
    if is_legacy_binary_relation(definition) {
        let [SqlValue::Binary(primary_key)] = parameters else {
            return Err(SqlError::ParameterMismatch);
        };
        return Ok(primary_key.clone());
    }
    let mut values = vec![None; definition.columns.len()];
    for (column, value) in parameter_columns.iter().copied().zip(parameters) {
        values[column] = Some(value);
    }
    encode_primary_key(definition, &values)
}

fn bind_primary_key_prefix(
    definition: &RelationDefinition,
    prefix: &KeyBinding,
    parameters: &[SqlValue],
) -> Result<Option<Vec<u8>>, SqlError> {
    if prefix.columns.len() != prefix.operands.len() || prefix.columns.is_empty() {
        return Err(SqlError::InvalidPrimaryKey);
    }
    let primary_key = primary_key_indices(definition)?;
    if prefix.columns.len() >= primary_key.len()
        || prefix.columns.as_slice() != &primary_key[..prefix.columns.len()]
    {
        return Err(SqlError::InvalidPrimaryKey);
    }
    let mut encoded = Vec::new();
    for (column, operand) in prefix.columns.iter().zip(&prefix.operands) {
        let value = resolve_operand(operand, parameters)?;
        if matches!(value, SqlValue::Null) {
            return Ok(None);
        }
        let logical_type = &definition
            .columns
            .get(*column)
            .ok_or(SqlError::InvalidCatalogObject)?
            .logical_type;
        encoded.extend_from_slice(
            &value
                .encode_ordered_component(logical_type)
                .map_err(|_| SqlError::TypeMismatch)?,
        );
    }
    Ok(Some(encoded))
}

fn primary_key_prefix_bounds(prefix: Vec<u8>) -> KeyBounds {
    let upper = binary_prefix_successor(&prefix);
    (
        Bound::Included(prefix),
        upper.map_or(Bound::Unbounded, Bound::Excluded),
    )
}

fn binary_prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    let index = upper.iter().rposition(|byte| *byte != u8::MAX)?;
    upper[index] += 1;
    upper.truncate(index + 1);
    Some(upper)
}

fn bind_primary_key_prefix_range(
    definition: &RelationDefinition,
    range: &PrimaryKeyPrefixRange,
    parameters: &[SqlValue],
) -> Result<Option<KeyBounds>, SqlError> {
    let Some(prefix) = bind_primary_key_prefix(definition, &range.prefix, parameters)? else {
        return Ok(None);
    };
    let primary_key = primary_key_indices(definition)?;
    if primary_key.get(range.prefix.columns.len()) != Some(&range.range_column) {
        return Err(SqlError::InvalidPrimaryKey);
    }
    let logical_type = &definition
        .columns
        .get(range.range_column)
        .ok_or(SqlError::InvalidCatalogObject)?
        .logical_type;
    let prefix_upper = binary_prefix_successor(&prefix);
    let lower = match range.lower.as_ref() {
        Some(endpoint) => {
            let value = resolve_operand(&endpoint.operand, parameters)?;
            if matches!(value, SqlValue::Null) {
                return Ok(None);
            }
            let key = append_ordered_component(&prefix, value, logical_type)?;
            if endpoint.inclusive {
                Bound::Included(key)
            } else {
                let Some(successor) = binary_prefix_successor(&key) else {
                    return Ok(None);
                };
                Bound::Included(successor)
            }
        }
        None => Bound::Included(prefix.clone()),
    };
    let upper = match range.upper.as_ref() {
        Some(endpoint) => {
            let value = resolve_operand(&endpoint.operand, parameters)?;
            if matches!(value, SqlValue::Null) {
                return Ok(None);
            }
            let key = append_ordered_component(&prefix, value, logical_type)?;
            if endpoint.inclusive {
                binary_prefix_successor(&key)
                    .or(prefix_upper)
                    .map_or(Bound::Unbounded, Bound::Excluded)
            } else {
                Bound::Excluded(key)
            }
        }
        None => prefix_upper.map_or(Bound::Unbounded, Bound::Excluded),
    };
    Ok(Some((lower, upper)))
}

fn append_ordered_component(
    prefix: &[u8],
    value: &SqlValue,
    logical_type: &LogicalType,
) -> Result<Vec<u8>, SqlError> {
    let component = value
        .encode_ordered_component(logical_type)
        .map_err(|_| SqlError::TypeMismatch)?;
    let mut encoded = Vec::with_capacity(prefix.len().saturating_add(component.len()));
    encoded.extend_from_slice(prefix);
    encoded.extend_from_slice(&component);
    Ok(encoded)
}

fn bind_primary_key_range(
    definition: &RelationDefinition,
    range: &PrimaryKeyRange,
    parameters: &[SqlValue],
) -> Result<KeyBounds, SqlError> {
    if parameters.len() != range.parameter_count {
        return Err(SqlError::ParameterMismatch);
    }
    let lower = bind_primary_key_range_endpoint(definition, range.lower.as_ref(), parameters)?
        .map_or(Bound::Unbounded, |(key, inclusive)| {
            if inclusive {
                Bound::Included(key)
            } else {
                Bound::Excluded(key)
            }
        });
    let upper = bind_primary_key_range_endpoint(definition, range.upper.as_ref(), parameters)?
        .map_or(Bound::Unbounded, |(key, inclusive)| {
            if inclusive {
                Bound::Included(key)
            } else {
                Bound::Excluded(key)
            }
        });
    Ok((lower, upper))
}

fn key_range_is_empty(lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
    let (lower_key, lower_inclusive) = match lower {
        Bound::Included(key) => (key, true),
        Bound::Excluded(key) => (key, false),
        Bound::Unbounded => return false,
    };
    let (upper_key, upper_inclusive) = match upper {
        Bound::Included(key) => (key, true),
        Bound::Excluded(key) => (key, false),
        Bound::Unbounded => return false,
    };
    lower_key > upper_key || (lower_key == upper_key && !(lower_inclusive && upper_inclusive))
}

fn bind_primary_key_range_endpoint(
    definition: &RelationDefinition,
    endpoint: Option<&PrimaryKeyRangeEndpoint>,
    parameters: &[SqlValue],
) -> Result<Option<(Vec<u8>, bool)>, SqlError> {
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };
    let primary_key = bind_primary_key_binding(definition, &endpoint.key, parameters)?;
    Ok(Some((primary_key, endpoint.inclusive)))
}

fn bind_primary_key_binding(
    definition: &RelationDefinition,
    key: &KeyBinding,
    parameters: &[SqlValue],
) -> Result<Vec<u8>, SqlError> {
    let values = key
        .operands
        .iter()
        .map(|operand| resolve_operand(operand, parameters).cloned())
        .collect::<Result<Vec<_>, _>>()?;
    bind_primary_key(definition, &key.columns, &values)
}

fn bind_secondary_index_key(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    parameter_columns: &[usize],
    parameters: &[SqlValue],
) -> Result<Option<Vec<u8>>, SqlError> {
    if parameter_columns.len() != parameters.len() {
        return Err(SqlError::ParameterMismatch);
    }
    let mut values = vec![None; relation.columns.len()];
    let mut contains_null = false;
    for (column, value) in parameter_columns.iter().copied().zip(parameters) {
        contains_null |= matches!(value, SqlValue::Null);
        values[column] = Some(value);
    }
    let mut encoded = Vec::new();
    for column in secondary_index_column_indices(relation, index)? {
        let value = values[column].ok_or(SqlError::NoAccessPath)?;
        encoded.extend_from_slice(
            &value
                .encode_ordered_component(&relation.columns[column].logical_type)
                .map_err(|_| SqlError::TypeMismatch)?,
        );
    }
    Ok((!contains_null).then_some(encoded))
}

fn bind_secondary_index_key_binding(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    key: &KeyBinding,
    parameters: &[SqlValue],
) -> Result<Option<Vec<u8>>, SqlError> {
    let values = key
        .operands
        .iter()
        .map(|operand| resolve_operand(operand, parameters).cloned())
        .collect::<Result<Vec<_>, _>>()?;
    bind_secondary_index_key(relation, index, &key.columns, &values)
}

fn bind_secondary_index_range(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    range: &SecondaryIndexRange,
    parameters: &[SqlValue],
) -> Result<Option<KeyBounds>, SqlError> {
    if parameters.len() != range.parameter_count {
        return Err(SqlError::ParameterMismatch);
    }
    match &range.kind {
        SecondaryIndexRangeKind::Complete { lower, upper } => bind_complete_secondary_index_range(
            relation,
            index,
            lower.as_ref(),
            upper.as_ref(),
            parameters,
        ),
        SecondaryIndexRangeKind::Prefix {
            prefix,
            range_column,
            lower,
            upper,
        } => bind_secondary_index_prefix_range(
            relation,
            index,
            prefix,
            *range_column,
            lower.as_ref(),
            upper.as_ref(),
            parameters,
        ),
    }
}

fn bind_complete_secondary_index_range(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    lower: Option<&SecondaryIndexRangeEndpoint>,
    upper: Option<&SecondaryIndexRangeEndpoint>,
    parameters: &[SqlValue],
) -> Result<Option<KeyBounds>, SqlError> {
    let Some(lower) =
        bind_secondary_index_range_endpoint(relation, index, lower, parameters)?.into_bound()
    else {
        return Ok(None);
    };
    let Some(upper) =
        bind_secondary_index_range_endpoint(relation, index, upper, parameters)?.into_bound()
    else {
        return Ok(None);
    };
    Ok(Some((lower, upper)))
}

fn bind_secondary_index_range_endpoint(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    endpoint: Option<&SecondaryIndexRangeEndpoint>,
    parameters: &[SqlValue],
) -> Result<BoundSecondaryIndexEndpoint, SqlError> {
    let Some(endpoint) = endpoint else {
        return Ok(BoundSecondaryIndexEndpoint::Unbounded);
    };
    Ok(
        match bind_secondary_index_key_binding(relation, index, &endpoint.key, parameters)? {
            Some(encoded) => BoundSecondaryIndexEndpoint::Key {
                encoded,
                inclusive: endpoint.inclusive,
            },
            None => BoundSecondaryIndexEndpoint::Null,
        },
    )
}

fn bind_secondary_index_prefix_range(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    prefix: &KeyBinding,
    range_column: usize,
    lower: Option<&SecondaryIndexPrefixRangeEndpoint>,
    upper: Option<&SecondaryIndexPrefixRangeEndpoint>,
    parameters: &[SqlValue],
) -> Result<Option<KeyBounds>, SqlError> {
    let prefix_column_count = prefix.columns.len();
    let Some(encoded_prefix) = bind_secondary_index_prefix(relation, index, prefix, parameters)?
    else {
        return Ok(None);
    };
    let index_columns = secondary_index_column_indices(relation, index)?;
    if index_columns.get(prefix_column_count) != Some(&range_column) {
        return Err(SqlError::InvalidSecondaryIndexRange);
    }
    let logical_type = &relation
        .columns
        .get(range_column)
        .ok_or(SqlError::InvalidCatalogObject)?
        .logical_type;
    let prefix_upper = binary_prefix_successor(&encoded_prefix);
    let lower =
        bind_secondary_index_prefix_lower(&encoded_prefix, lower, logical_type, parameters)?;
    let Some(lower) = lower else {
        return Ok(None);
    };
    let upper = bind_secondary_index_prefix_upper(
        &encoded_prefix,
        prefix_upper,
        upper,
        logical_type,
        parameters,
    )?;
    let Some(upper) = upper else {
        return Ok(None);
    };
    Ok(Some((lower, upper)))
}

fn bind_secondary_index_prefix(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
    prefix: &KeyBinding,
    parameters: &[SqlValue],
) -> Result<Option<Vec<u8>>, SqlError> {
    let index_columns = secondary_index_column_indices(relation, index)?;
    if prefix.columns.is_empty()
        || prefix.columns.len() >= index_columns.len()
        || !index_columns.starts_with(&prefix.columns)
        || prefix.columns.len() != prefix.operands.len()
    {
        return Err(SqlError::InvalidSecondaryIndexRange);
    }
    let mut encoded = Vec::new();
    for (column, operand) in prefix.columns.iter().zip(&prefix.operands) {
        let value = resolve_operand(operand, parameters)?;
        if matches!(value, SqlValue::Null) {
            return Ok(None);
        }
        encoded = append_ordered_component(
            &encoded,
            value,
            &relation
                .columns
                .get(*column)
                .ok_or(SqlError::InvalidCatalogObject)?
                .logical_type,
        )?;
    }
    Ok(Some(encoded))
}

fn bind_secondary_index_prefix_lower(
    prefix: &[u8],
    endpoint: Option<&SecondaryIndexPrefixRangeEndpoint>,
    logical_type: &LogicalType,
    parameters: &[SqlValue],
) -> Result<Option<Bound<Vec<u8>>>, SqlError> {
    let Some(endpoint) = endpoint else {
        return Ok(Some(Bound::Included(prefix.to_vec())));
    };
    let value = resolve_operand(&endpoint.operand, parameters)?;
    if matches!(value, SqlValue::Null) {
        return Ok(None);
    }
    let key = append_ordered_component(prefix, value, logical_type)?;
    if endpoint.inclusive {
        Ok(Some(Bound::Included(key)))
    } else {
        Ok(binary_prefix_successor(&key).map(Bound::Included))
    }
}

fn bind_secondary_index_prefix_upper(
    prefix: &[u8],
    prefix_upper: Option<Vec<u8>>,
    endpoint: Option<&SecondaryIndexPrefixRangeEndpoint>,
    logical_type: &LogicalType,
    parameters: &[SqlValue],
) -> Result<Option<Bound<Vec<u8>>>, SqlError> {
    let Some(endpoint) = endpoint else {
        return Ok(Some(prefix_upper.map_or(Bound::Unbounded, Bound::Excluded)));
    };
    let value = resolve_operand(&endpoint.operand, parameters)?;
    if matches!(value, SqlValue::Null) {
        return Ok(None);
    }
    let key = append_ordered_component(prefix, value, logical_type)?;
    if endpoint.inclusive {
        Ok(Some(
            binary_prefix_successor(&key)
                .or(prefix_upper)
                .map_or(Bound::Unbounded, Bound::Excluded),
        ))
    } else {
        Ok(Some(Bound::Excluded(key)))
    }
}

fn validate_secondary_index_values(
    relation: &RelationDefinition,
    index_columns: &[usize],
    stored_index_key: &[u8],
    values: &[SqlValue],
) -> Result<(), SqlError> {
    let mut offset = 0_usize;
    for &column in index_columns {
        let component = values
            .get(column)
            .ok_or(SqlError::InvalidStoredRow)?
            .encode_ordered_component(&relation.columns[column].logical_type)
            .map_err(|_| SqlError::InvalidStoredRow)?;
        let end = offset
            .checked_add(component.len())
            .ok_or(SqlError::InvalidStoredRow)?;
        if stored_index_key.get(offset..end) != Some(component.as_slice()) {
            return Err(SqlError::InvalidStoredRow);
        }
        offset = end;
    }
    if offset == stored_index_key.len() {
        Ok(())
    } else {
        Err(SqlError::InvalidStoredRow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TruthValue {
    True,
    False,
    Unknown,
}

impl TruthValue {
    const fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    const fn and(self, right: Self) -> Self {
        match (self, right) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    const fn or(self, right: Self) -> Self {
        match (self, right) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}

fn validate_filter_parameters(
    definition: &RelationDefinition,
    filter: Option<&BoundFilterExpression>,
    parameter_count: usize,
    parameters: &[SqlValue],
) -> Result<(), SqlError> {
    if parameters.len() != parameter_count {
        return Err(SqlError::ParameterMismatch);
    }
    if let Some(filter) = filter {
        validate_filter_parameter_types(definition, filter, parameters)?;
    }
    Ok(())
}

fn validate_filter_parameter_types(
    definition: &RelationDefinition,
    expression: &BoundFilterExpression,
    parameters: &[SqlValue],
) -> Result<(), SqlError> {
    match expression {
        BoundFilterExpression::Comparison {
            columns, operands, ..
        } => {
            if columns.len() != operands.len() {
                return Err(SqlError::ParameterMismatch);
            }
            for (column, operand) in columns.iter().zip(operands) {
                let value = resolve_operand(operand, parameters)?;
                if matches!(value, SqlValue::Null) {
                    continue;
                }
                value
                    .encode_ordered_component(
                        &definition
                            .columns
                            .get(*column)
                            .ok_or(SqlError::InvalidCatalogObject)?
                            .logical_type,
                    )
                    .map_err(|_| SqlError::TypeMismatch)?;
            }
            Ok(())
        }
        BoundFilterExpression::IsNull { column, .. } => {
            definition
                .columns
                .get(*column)
                .ok_or(SqlError::InvalidCatalogObject)?;
            Ok(())
        }
        BoundFilterExpression::And(left, right) | BoundFilterExpression::Or(left, right) => {
            validate_filter_parameter_types(definition, left, parameters)?;
            validate_filter_parameter_types(definition, right, parameters)
        }
        BoundFilterExpression::Not(expression) => {
            validate_filter_parameter_types(definition, expression, parameters)
        }
    }
}

fn evaluate_filter(
    definition: &RelationDefinition,
    expression: &BoundFilterExpression,
    row: &[SqlValue],
    parameters: &[SqlValue],
) -> Result<TruthValue, SqlError> {
    match expression {
        BoundFilterExpression::Comparison {
            columns,
            operator,
            operands,
        } => evaluate_comparison(definition, columns, *operator, operands, row, parameters),
        BoundFilterExpression::IsNull { column, negated } => {
            let is_null = matches!(
                row.get(*column).ok_or(SqlError::InvalidStoredRow)?,
                SqlValue::Null
            );
            Ok(if is_null == *negated {
                TruthValue::False
            } else {
                TruthValue::True
            })
        }
        BoundFilterExpression::And(left, right) => {
            Ok(evaluate_filter(definition, left, row, parameters)?
                .and(evaluate_filter(definition, right, row, parameters)?))
        }
        BoundFilterExpression::Or(left, right) => {
            Ok(evaluate_filter(definition, left, row, parameters)?
                .or(evaluate_filter(definition, right, row, parameters)?))
        }
        BoundFilterExpression::Not(expression) => {
            Ok(evaluate_filter(definition, expression, row, parameters)?.not())
        }
    }
}

fn evaluate_comparison(
    definition: &RelationDefinition,
    columns: &[usize],
    operator: ComparisonOperator,
    operands: &[BoundScalarOperand],
    row: &[SqlValue],
    parameters: &[SqlValue],
) -> Result<TruthValue, SqlError> {
    let mut contains_unknown = false;
    for (column, operand) in columns.iter().zip(operands) {
        let left = row.get(*column).ok_or(SqlError::InvalidStoredRow)?;
        let right = resolve_operand(operand, parameters)?;
        let Some(ordering) = compare_sql_values(
            &definition
                .columns
                .get(*column)
                .ok_or(SqlError::InvalidCatalogObject)?
                .logical_type,
            left,
            right,
        )?
        else {
            contains_unknown = true;
            if !matches!(
                operator,
                ComparisonOperator::Equal | ComparisonOperator::NotEqual
            ) {
                return Ok(TruthValue::Unknown);
            }
            continue;
        };
        if ordering != Ordering::Equal {
            return Ok(match operator {
                ComparisonOperator::Equal => TruthValue::False,
                ComparisonOperator::NotEqual => TruthValue::True,
                ComparisonOperator::Less => truth(ordering == Ordering::Less),
                ComparisonOperator::LessOrEqual => truth(ordering != Ordering::Greater),
                ComparisonOperator::Greater => truth(ordering == Ordering::Greater),
                ComparisonOperator::GreaterOrEqual => truth(ordering != Ordering::Less),
            });
        }
    }
    if contains_unknown {
        return Ok(TruthValue::Unknown);
    }
    Ok(match operator {
        ComparisonOperator::Equal
        | ComparisonOperator::LessOrEqual
        | ComparisonOperator::GreaterOrEqual => TruthValue::True,
        ComparisonOperator::NotEqual | ComparisonOperator::Less | ComparisonOperator::Greater => {
            TruthValue::False
        }
    })
}

fn resolve_operand<'value>(
    operand: &'value BoundScalarOperand,
    parameters: &'value [SqlValue],
) -> Result<&'value SqlValue, SqlError> {
    match operand {
        BoundScalarOperand::Parameter(position) => {
            parameters.get(*position).ok_or(SqlError::ParameterMismatch)
        }
        BoundScalarOperand::Literal(value) => Ok(value),
    }
}

fn compare_sql_values(
    logical_type: &LogicalType,
    left: &SqlValue,
    right: &SqlValue,
) -> Result<Option<Ordering>, SqlError> {
    if matches!(left, SqlValue::Null) || matches!(right, SqlValue::Null) {
        return Ok(None);
    }
    let left = left
        .encode_ordered_component(logical_type)
        .map_err(|_| SqlError::InvalidStoredRow)?;
    let right = right
        .encode_ordered_component(logical_type)
        .map_err(|_| SqlError::TypeMismatch)?;
    Ok(Some(left.cmp(&right)))
}

const fn truth(value: bool) -> TruthValue {
    if value {
        TruthValue::True
    } else {
        TruthValue::False
    }
}

fn key_contains_null(key: &KeyBinding, parameters: &[SqlValue]) -> Result<bool, SqlError> {
    key.operands.iter().try_fold(false, |contains, operand| {
        resolve_operand(operand, parameters)
            .map(|value| contains || matches!(value, SqlValue::Null))
    })
}

fn materialize_filtered_row(
    definition: &RelationDefinition,
    projection: &[usize],
    legacy_binary: bool,
    primary_key: &[u8],
    stored: &[u8],
    filter: Option<&BoundFilterExpression>,
    parameters: &[SqlValue],
) -> Result<Option<Vec<SqlValue>>, SqlError> {
    let values = decode_complete_row(definition, legacy_binary, primary_key, stored)?;
    materialize_decoded_row(definition, projection, &values, filter, parameters)
}

fn materialize_decoded_row(
    definition: &RelationDefinition,
    projection: &[usize],
    values: &[SqlValue],
    filter: Option<&BoundFilterExpression>,
    parameters: &[SqlValue],
) -> Result<Option<Vec<SqlValue>>, SqlError> {
    if let Some(expression) = filter
        && evaluate_filter(definition, expression, values, parameters)? != TruthValue::True
    {
        return Ok(None);
    }
    projection
        .iter()
        .map(|index| {
            values
                .get(*index)
                .cloned()
                .ok_or(SqlError::InvalidStoredRow)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn decode_complete_row(
    definition: &RelationDefinition,
    legacy_binary: bool,
    primary_key: &[u8],
    stored: &[u8],
) -> Result<Vec<SqlValue>, SqlError> {
    if legacy_binary {
        return Ok(vec![
            SqlValue::Binary(primary_key.to_vec()),
            SqlValue::Binary(stored.to_vec()),
        ]);
    }
    let tuple = RowTupleView::decode(stored).map_err(|_| SqlError::InvalidStoredRow)?;
    if tuple.column_count() != definition.columns.len() {
        return Err(SqlError::InvalidStoredRow);
    }
    definition
        .columns
        .iter()
        .enumerate()
        .map(
            |(index, column)| match tuple.value(index).ok_or(SqlError::InvalidStoredRow)? {
                ColumnValueRef::Null => Ok(SqlValue::Null),
                ColumnValueRef::Bytes(encoded) => {
                    SqlValue::decode_storage(&column.logical_type, encoded)
                        .map_err(|_| SqlError::InvalidStoredRow)
                }
            },
        )
        .collect()
}

fn legacy_binary_value(value: Option<&SqlValue>, nullable: bool) -> Result<Vec<u8>, SqlError> {
    match value {
        Some(SqlValue::Binary(value)) => Ok(value.clone()),
        Some(SqlValue::Null) | None if nullable => Ok(Vec::new()),
        Some(SqlValue::Null) | None => Err(SqlError::NullViolation),
        Some(_) => Err(SqlError::TypeMismatch),
    }
}

fn primary_key_indices(definition: &RelationDefinition) -> Result<Vec<usize>, SqlError> {
    definition
        .primary_key
        .iter()
        .map(|id| {
            definition
                .columns
                .iter()
                .position(|column| column.id == *id)
                .ok_or(SqlError::InvalidCatalogObject)
        })
        .collect()
}

fn secondary_index_column_indices(
    relation: &RelationDefinition,
    index: &SecondaryIndexDefinition,
) -> Result<Vec<usize>, SqlError> {
    index
        .columns
        .iter()
        .map(|id| {
            relation
                .columns
                .iter()
                .position(|column| column.id == *id)
                .ok_or(SqlError::InvalidCatalogObject)
        })
        .collect()
}

fn relation_named<'catalog>(
    catalog: &'catalog crate::model::CatalogState,
    name: &str,
) -> Result<(ObjectId, &'catalog RelationDefinition), SqlError> {
    let id = catalog
        .id_named(name, EngineKind::Relational)
        .map_err(NativeRuntimeError::from)?;
    Ok((id, relation_by_id(catalog, id)?))
}

fn relation_by_id(
    catalog: &crate::model::CatalogState,
    id: ObjectId,
) -> Result<&RelationDefinition, SqlError> {
    match catalog.object(id) {
        Some(CatalogObject::Relation(definition)) => Ok(definition),
        Some(
            CatalogObject::SecondaryIndex(_)
            | CatalogObject::Structure(_)
            | CatalogObject::Search(_),
        )
        | None => Err(SqlError::InvalidCatalogObject),
    }
}

fn secondary_index_by_id(
    catalog: &crate::model::CatalogState,
    id: ObjectId,
) -> Result<&SecondaryIndexDefinition, SqlError> {
    match catalog.object(id) {
        Some(CatalogObject::SecondaryIndex(definition)) => Ok(definition),
        Some(
            CatalogObject::Relation(_) | CatalogObject::Structure(_) | CatalogObject::Search(_),
        )
        | None => Err(SqlError::InvalidCatalogObject),
    }
}

pub(crate) fn map_runtime_error(error: NativeRuntimeError) -> SqlError {
    match error {
        NativeRuntimeError::UniqueSecondaryIndexViolation => SqlError::UniqueViolation,
        NativeRuntimeError::CheckConstraintViolation => SqlError::CheckViolation,
        error => SqlError::Runtime(error),
    }
}

fn column_index(columns: &[ColumnDefinition], name: &str) -> Result<usize, SqlError> {
    let lookup = normalize_identifier(name);
    columns
        .iter()
        .position(|column| column.name.lookup() == lookup)
        .ok_or(SqlError::UnknownColumn)
}

fn normalize_identifier(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character
            }
        })
        .collect()
}

fn is_legacy_binary_relation(definition: &RelationDefinition) -> bool {
    definition.columns.len() == 2
        && definition.columns[0].name.lookup() == "primary_key"
        && definition.columns[0].logical_type == LogicalType::Binary
        && definition.columns[1].name.lookup() == "row"
        && definition.columns[1].logical_type == LogicalType::Binary
        && definition.primary_key.as_slice() == [definition.columns[0].id]
}

fn ensure_catalog_version(
    catalog_version: CatalogVersion,
    prepared: &PreparedStatement,
) -> Result<(), SqlError> {
    if prepared.catalog_version == catalog_version {
        Ok(())
    } else {
        Err(SqlError::CatalogChanged)
    }
}

fn parse(statement: &str) -> Result<Statement, SqlError> {
    let mut parser = Parser::new(lex(statement)?);
    let parsed = if parser.consume_keyword("WITH") {
        parse_with_select(&mut parser)?
    } else if parser.consume_keyword("CREATE") {
        parse_create(&mut parser)?
    } else if parser.consume_keyword("INSERT") {
        parse_insert(&mut parser)?
    } else if parser.consume_keyword("UPDATE") {
        parse_update(&mut parser)?
    } else if parser.consume_keyword("DELETE") {
        parse_delete(&mut parser)?
    } else if parser.consume_keyword("SELECT") {
        parse_select(&mut parser)?
    } else if parser.consume_keyword("EXPLAIN") {
        parser.expect_keyword("SELECT")?;
        match parse_select(&mut parser)? {
            Statement::Select {
                name,
                projection,
                filter,
                parameter_count,
                order_by,
                limit,
            } => Statement::ExplainSelect {
                name,
                projection,
                filter,
                parameter_count,
                order_by,
                limit,
            },
            Statement::SelectJoin(join) => Statement::ExplainSelectJoin(join),
            _ => return Err(SqlError::InvalidSyntax),
        }
    } else {
        return Err(SqlError::InvalidSyntax);
    };
    parser.consume_symbol(';');
    parser.finish()?;
    Ok(parsed)
}

fn parse_with_select(parser: &mut Parser) -> Result<Statement, SqlError> {
    if parser.consume_keyword("RECURSIVE") {
        return Err(SqlError::InvalidSyntax);
    }
    let name = parser.identifier()?;
    parser.expect_keyword("AS")?;
    parser.expect_symbol('(')?;
    parser.expect_keyword("SELECT")?;
    let inner = parse_select(parser)?;
    parser.expect_symbol(')')?;
    parser.expect_keyword("SELECT")?;
    let outer = parse_select(parser)?;
    let Statement::Select {
        name: outer_name, ..
    } = &outer
    else {
        return Err(SqlError::InvalidSyntax);
    };
    if normalize_identifier(outer_name) != normalize_identifier(&name) {
        return Err(SqlError::InvalidSyntax);
    }
    Ok(Statement::WithSelect(ParsedCteSelect {
        name,
        inner: Box::new(inner),
        outer: Box::new(outer),
    }))
}

#[allow(clippy::too_many_lines)]
fn parse_create(parser: &mut Parser) -> Result<Statement, SqlError> {
    let unique = parser.consume_keyword("UNIQUE");
    if parser.consume_keyword("INDEX") {
        return parse_create_index(parser, unique);
    }
    if unique {
        return Err(SqlError::InvalidSyntax);
    }
    parser.expect_keyword("TABLE")?;
    let name = parser.identifier()?;
    parser.expect_symbol('(')?;
    let mut columns = Vec::new();
    let mut table_primary_key = None;
    loop {
        if parser.consume_keyword("PRIMARY") {
            if table_primary_key.is_some() {
                return Err(SqlError::InvalidSyntax);
            }
            parser.expect_keyword("KEY")?;
            parser.expect_symbol('(')?;
            let primary_key = parser.identifier_list(')')?;
            table_primary_key = Some(primary_key);
        } else {
            let column_name = parser.identifier()?;
            let logical_type = parser.logical_type()?;
            let mut nullable = true;
            let mut nullability_seen = false;
            let mut inline_primary_key = false;
            let mut check = None;
            loop {
                if parser.consume_keyword("NOT") {
                    if nullability_seen {
                        return Err(SqlError::InvalidSyntax);
                    }
                    parser.expect_keyword("NULL")?;
                    nullable = false;
                    nullability_seen = true;
                } else if parser.consume_keyword("NULL") {
                    if nullability_seen {
                        return Err(SqlError::InvalidSyntax);
                    }
                    nullable = true;
                    nullability_seen = true;
                } else if parser.consume_keyword("PRIMARY") {
                    if inline_primary_key {
                        return Err(SqlError::InvalidSyntax);
                    }
                    parser.expect_keyword("KEY")?;
                    nullable = false;
                    inline_primary_key = true;
                } else if parser.consume_keyword("CHECK") {
                    if check.is_some() {
                        return Err(SqlError::InvalidSyntax);
                    }
                    parser.expect_symbol('(')?;
                    if normalize_identifier(&parser.identifier()?)
                        != normalize_identifier(&column_name)
                    {
                        return Err(SqlError::InvalidSyntax);
                    }
                    let operator = parser.comparison_operator()?;
                    let mut parameters = 0;
                    let operand = parser.scalar_operand(&mut parameters)?;
                    if parameters != 0 {
                        return Err(SqlError::InvalidSyntax);
                    }
                    parser.expect_symbol(')')?;
                    check = Some(ParsedColumnCheck { operator, operand });
                } else {
                    break;
                }
            }
            columns.push(ParsedColumn {
                name: column_name,
                logical_type,
                nullable,
                inline_primary_key,
                check,
            });
        }
        if parser.consume_symbol(')') {
            break;
        }
        parser.expect_symbol(',')?;
    }
    if columns.is_empty() {
        return Err(SqlError::InvalidSyntax);
    }
    let inline_primary_key: Vec<String> = columns
        .iter()
        .filter(|column| column.inline_primary_key)
        .map(|column| column.name.clone())
        .collect();
    let primary_key = match (table_primary_key, inline_primary_key.is_empty()) {
        (Some(primary_key), true) => primary_key,
        (None, false) => inline_primary_key,
        (Some(_), false) => return Err(SqlError::InvalidSyntax),
        (None, true) => return Err(SqlError::InvalidPrimaryKey),
    };
    Ok(Statement::CreateTable {
        name,
        columns,
        primary_key,
    })
}

fn parse_create_index(parser: &mut Parser, unique: bool) -> Result<Statement, SqlError> {
    let name = parser.identifier()?;
    parser.expect_keyword("ON")?;
    let table = parser.identifier()?;
    parser.expect_symbol('(')?;
    let columns = parser.identifier_list(')')?;
    Ok(Statement::CreateIndex {
        name,
        table,
        columns,
        unique,
    })
}

fn parse_insert(parser: &mut Parser) -> Result<Statement, SqlError> {
    parser.expect_keyword("INTO")?;
    let name = parser.identifier()?;
    parser.expect_symbol('(')?;
    let columns = parser.identifier_list(')')?;
    parser.expect_keyword("VALUES")?;
    parser.expect_symbol('(')?;
    let mut parameter_count = 0_usize;
    let mut values = Vec::with_capacity(columns.len());
    for (index, column) in columns.into_iter().enumerate() {
        if index != 0 {
            parser.expect_symbol(',')?;
        }
        values.push(ColumnOperand {
            column,
            operand: parser.scalar_operand(&mut parameter_count)?,
        });
    }
    parser.expect_symbol(')')?;
    Ok(Statement::Insert {
        name,
        values,
        parameter_count,
    })
}

fn parse_update(parser: &mut Parser) -> Result<Statement, SqlError> {
    let name = parser.identifier()?;
    parser.expect_keyword("SET")?;
    let mut parameter_count = 0_usize;
    let mut assignments = Vec::new();
    loop {
        let column = parser.identifier()?;
        parser.expect_symbol('=')?;
        assignments.push(ColumnOperand {
            column,
            operand: parser.scalar_operand(&mut parameter_count)?,
        });
        if !parser.consume_symbol(',') {
            break;
        }
    }
    parser.expect_keyword("WHERE")?;
    let predicates = parse_mutation_predicates(parser, &mut parameter_count)?;
    Ok(Statement::Update {
        name,
        assignments,
        predicates,
        parameter_count,
    })
}

fn parse_delete(parser: &mut Parser) -> Result<Statement, SqlError> {
    parser.expect_keyword("FROM")?;
    let name = parser.identifier()?;
    parser.expect_keyword("WHERE")?;
    let mut parameter_count = 0_usize;
    let predicates = parse_mutation_predicates(parser, &mut parameter_count)?;
    Ok(Statement::Delete {
        name,
        predicates,
        parameter_count,
    })
}

fn parse_window_select(parser: &mut Parser) -> Result<Statement, SqlError> {
    let value_column = parser.identifier()?;
    parser.expect_symbol(',')?;
    let function = if parser.consume_keyword("ROW_NUMBER") {
        WindowFunction::RowNumber
    } else if parser.consume_keyword("RANK") {
        WindowFunction::Rank
    } else {
        return Err(SqlError::InvalidSyntax);
    };
    parser.expect_symbol('(')?;
    parser.expect_symbol(')')?;
    parser.expect_keyword("OVER")?;
    parser.expect_symbol('(')?;
    let partition_column = if parser.consume_keyword("PARTITION") {
        parser.expect_keyword("BY")?;
        let column = parser.identifier()?;
        if parser.consume_symbol(',') {
            return Err(SqlError::InvalidSyntax);
        }
        Some(column)
    } else {
        None
    };
    parser.expect_keyword("ORDER")?;
    parser.expect_keyword("BY")?;
    let order_column = parser.identifier()?;
    if parser.consume_keyword("DESC") || parser.consume_symbol(',') {
        return Err(SqlError::InvalidSyntax);
    }
    parser.expect_symbol(')')?;
    parser.expect_keyword("AS")?;
    let alias = parser.identifier()?;
    parser.expect_keyword("FROM")?;
    let name = parser.identifier()?;
    parser.expect_keyword("ORDER")?;
    parser.expect_keyword("BY")?;
    let outer_order = parser.identifier()?;
    let mut outer_order_by = vec![outer_order];
    while parser.consume_symbol(',') {
        outer_order_by.push(parser.identifier()?);
    }
    let expected_order_by = partition_column
        .iter()
        .cloned()
        .chain(std::iter::once(order_column.clone()))
        .collect::<Vec<_>>();
    if outer_order_by
        .iter()
        .map(|column| normalize_identifier(column))
        .ne(expected_order_by
            .iter()
            .map(|column| normalize_identifier(column)))
    {
        return Err(SqlError::InvalidSyntax);
    }
    parser.expect_keyword("LIMIT")?;
    let limit = parser.number_usize()?;
    Ok(Statement::SelectWindow(ParsedWindowSelect {
        name,
        value_column,
        function,
        partition_column,
        order_column,
        alias,
        outer_order_by,
        limit,
    }))
}

fn parse_select(parser: &mut Parser) -> Result<Statement, SqlError> {
    if parser.looks_like_window_select() {
        return parse_window_select(parser);
    }
    let projection = if parser.consume_symbol('*') {
        Projection::All
    } else {
        Projection::Columns(parser.identifier_list_until_keyword("FROM")?)
    };
    parser.expect_keyword("FROM")?;
    let name = parser.identifier()?;
    if parser.consume_keyword("INNER") {
        return parse_inner_join(parser, name, projection);
    }
    let mut parameter_count = 0_usize;
    let filter = if parser.consume_keyword("WHERE") {
        Some(parse_filter_expression(parser, &mut parameter_count)?)
    } else {
        None
    };
    let order_by = if parser.consume_keyword("ORDER") {
        parser.expect_keyword("BY")?;
        let mut columns = vec![parser.identifier()?];
        while parser.consume_symbol(',') {
            columns.push(parser.identifier()?);
        }
        columns
    } else {
        Vec::new()
    };
    let limit = if parser.consume_keyword("LIMIT") {
        Some(parser.number_usize()?)
    } else {
        None
    };
    if filter.is_none() && limit.is_none() {
        return Err(SqlError::InvalidSyntax);
    }
    if filter
        .as_ref()
        .is_some_and(|expression| !has_top_level_scalar_equality(expression))
        && limit.is_none()
    {
        return Err(SqlError::InvalidSyntax);
    }
    Ok(Statement::Select {
        name,
        projection,
        filter,
        parameter_count,
        order_by,
        limit,
    })
}

fn parse_inner_join(
    parser: &mut Parser,
    left_name: String,
    projection: Projection,
) -> Result<Statement, SqlError> {
    parser.expect_keyword("JOIN")?;
    let right_name = parser.identifier()?;
    parser.expect_keyword("ON")?;
    let mut equalities = Vec::new();
    loop {
        let left_column = parser.identifier()?;
        parser.expect_symbol('=')?;
        let right_column = parser.identifier()?;
        equalities.push(ParsedJoinEquality {
            left_column,
            right_column,
        });
        if !parser.consume_keyword("AND") {
            break;
        }
    }
    let mut parameter_count = 0_usize;
    let filter = if parser.consume_keyword("WHERE") {
        Some(parse_filter_expression(parser, &mut parameter_count)?)
    } else {
        None
    };
    let order_by = if parser.consume_keyword("ORDER") {
        parser.expect_keyword("BY")?;
        let mut columns = vec![parser.identifier()?];
        while parser.consume_symbol(',') {
            columns.push(parser.identifier()?);
        }
        columns
    } else {
        Vec::new()
    };
    let limit = if parser.consume_keyword("LIMIT") {
        Some(parser.number_usize()?)
    } else {
        None
    };
    if filter.is_none() && limit.is_none() {
        return Err(SqlError::InvalidSyntax);
    }
    let Projection::Columns(projection) = projection else {
        return Err(SqlError::InvalidSyntax);
    };
    Ok(Statement::SelectJoin(ParsedInnerJoin {
        left_name,
        right_name,
        projection,
        equalities,
        filter,
        parameter_count,
        order_by,
        limit,
    }))
}

fn parse_filter_expression(
    parser: &mut Parser,
    parameter_count: &mut usize,
) -> Result<FilterExpression, SqlError> {
    let mut expression = parse_filter_term(parser, parameter_count)?;
    while parser.consume_keyword("OR") {
        expression = FilterExpression::Or(
            Box::new(expression),
            Box::new(parse_filter_term(parser, parameter_count)?),
        );
    }
    Ok(expression)
}

fn parse_filter_term(
    parser: &mut Parser,
    parameter_count: &mut usize,
) -> Result<FilterExpression, SqlError> {
    let mut expression = parse_filter_factor(parser, parameter_count)?;
    while parser.consume_keyword("AND") {
        expression = FilterExpression::And(
            Box::new(expression),
            Box::new(parse_filter_factor(parser, parameter_count)?),
        );
    }
    Ok(expression)
}

fn parse_filter_factor(
    parser: &mut Parser,
    parameter_count: &mut usize,
) -> Result<FilterExpression, SqlError> {
    if parser.consume_keyword("NOT") {
        return Ok(FilterExpression::Not(Box::new(parse_filter_factor(
            parser,
            parameter_count,
        )?)));
    }
    if parser.starts_row_comparison() {
        return parse_filter_predicate(parser, parameter_count);
    }
    if parser.consume_symbol('(') {
        let expression = parse_filter_expression(parser, parameter_count)?;
        parser.expect_symbol(')')?;
        return Ok(expression);
    }
    parse_filter_predicate(parser, parameter_count)
}

fn parse_filter_predicate(
    parser: &mut Parser,
    parameter_count: &mut usize,
) -> Result<FilterExpression, SqlError> {
    let row_value = parser.consume_symbol('(');
    let columns = if row_value {
        let columns = parser.identifier_list(')')?;
        if columns.len() < 2 {
            return Err(SqlError::InvalidSyntax);
        }
        columns
    } else {
        vec![parser.identifier()?]
    };
    if !row_value && parser.consume_keyword("IS") {
        let negated = parser.consume_keyword("NOT");
        parser.expect_keyword("NULL")?;
        return Ok(FilterExpression::IsNull {
            column: columns.into_iter().next().ok_or(SqlError::InvalidSyntax)?,
            negated,
        });
    }
    let operator = parser.comparison_operator()?;
    let operands = if row_value {
        parser.expect_symbol('(')?;
        let mut operands = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            if index != 0 {
                parser.expect_symbol(',')?;
            }
            operands.push(parser.scalar_operand(parameter_count)?);
        }
        parser.expect_symbol(')')?;
        operands
    } else {
        vec![parser.scalar_operand(parameter_count)?]
    };
    Ok(FilterExpression::Comparison {
        columns,
        operator,
        operands,
    })
}

fn has_top_level_scalar_equality(expression: &FilterExpression) -> bool {
    match expression {
        FilterExpression::Comparison {
            columns, operator, ..
        } => columns.len() == 1 && *operator == ComparisonOperator::Equal,
        FilterExpression::And(left, right) => {
            has_top_level_scalar_equality(left) || has_top_level_scalar_equality(right)
        }
        FilterExpression::IsNull { .. } | FilterExpression::Or(_, _) | FilterExpression::Not(_) => {
            false
        }
    }
}

fn parse_mutation_predicates(
    parser: &mut Parser,
    parameter_count: &mut usize,
) -> Result<Vec<ColumnOperand>, SqlError> {
    let mut predicates = Vec::new();
    loop {
        let column = parser.identifier()?;
        parser.expect_symbol('=')?;
        predicates.push(ColumnOperand {
            column,
            operand: parser.scalar_operand(parameter_count)?,
        });
        if !parser.consume_keyword("AND") {
            break;
        }
    }
    Ok(predicates)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Word(String),
    Number(String),
    String(String),
    Symbol(char),
}

fn lex(statement: &str) -> Result<Vec<Token>, SqlError> {
    let characters: Vec<char> = statement.chars().collect();
    let mut tokens = Vec::new();
    let mut offset = 0;
    while offset < characters.len() {
        let character = characters[offset];
        if character.is_whitespace() {
            offset += 1;
        } else if matches!(
            character,
            '(' | ')' | ',' | '.' | '=' | '!' | '<' | '>' | '-' | ';' | '?' | '*'
        ) {
            tokens.push(Token::Symbol(character));
            offset += 1;
        } else if character == '"' {
            offset += 1;
            let mut identifier = String::new();
            let mut closed = false;
            while offset < characters.len() {
                if characters[offset] == '"' {
                    if characters.get(offset + 1) == Some(&'"') {
                        identifier.push('"');
                        offset += 2;
                    } else {
                        offset += 1;
                        closed = true;
                        break;
                    }
                } else {
                    identifier.push(characters[offset]);
                    offset += 1;
                }
            }
            if !closed || identifier.is_empty() {
                return Err(SqlError::InvalidSyntax);
            }
            tokens.push(Token::Word(identifier));
        } else if character == '\'' {
            offset += 1;
            let mut value = String::new();
            let mut closed = false;
            while offset < characters.len() {
                if characters[offset] == '\'' {
                    if characters.get(offset + 1) == Some(&'\'') {
                        value.push('\'');
                        offset += 2;
                    } else {
                        offset += 1;
                        closed = true;
                        break;
                    }
                } else {
                    value.push(characters[offset]);
                    offset += 1;
                }
            }
            if !closed {
                return Err(SqlError::InvalidSyntax);
            }
            tokens.push(Token::String(value));
        } else if character.is_ascii_digit() {
            let start = offset;
            offset += 1;
            while offset < characters.len() && characters[offset].is_ascii_digit() {
                offset += 1;
            }
            tokens.push(Token::Number(characters[start..offset].iter().collect()));
        } else if character.is_alphabetic() || character == '_' {
            let start = offset;
            offset += 1;
            while offset < characters.len()
                && (characters[offset].is_alphanumeric() || characters[offset] == '_')
            {
                offset += 1;
            }
            tokens.push(Token::Word(characters[start..offset].iter().collect()));
        } else {
            return Err(SqlError::InvalidSyntax);
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    offset: usize,
}

impl Parser {
    const fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, offset: 0 }
    }

    fn looks_like_window_select(&self) -> bool {
        matches!(self.tokens.get(self.offset), Some(Token::Word(_)))
            && self.tokens.get(self.offset + 1) == Some(&Token::Symbol(','))
            && matches!(
                self.tokens.get(self.offset + 2),
                Some(Token::Word(value))
                    if value.eq_ignore_ascii_case("ROW_NUMBER") || value.eq_ignore_ascii_case("RANK")
            )
    }

    fn consume_keyword(&mut self, expected: &str) -> bool {
        if self.tokens.get(self.offset).is_some_and(
            |token| matches!(token, Token::Word(value) if value.eq_ignore_ascii_case(expected)),
        ) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<(), SqlError> {
        if self.consume_keyword(expected) {
            Ok(())
        } else {
            Err(SqlError::InvalidSyntax)
        }
    }

    fn consume_symbol(&mut self, expected: char) -> bool {
        if self.tokens.get(self.offset) == Some(&Token::Symbol(expected)) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, expected: char) -> Result<(), SqlError> {
        if self.consume_symbol(expected) {
            Ok(())
        } else {
            Err(SqlError::InvalidSyntax)
        }
    }

    fn starts_row_comparison(&self) -> bool {
        self.tokens.get(self.offset) == Some(&Token::Symbol('('))
            && matches!(self.tokens.get(self.offset + 1), Some(Token::Word(_)))
            && self.tokens.get(self.offset + 2) == Some(&Token::Symbol(','))
    }

    fn comparison_operator(&mut self) -> Result<ComparisonOperator, SqlError> {
        if self.consume_symbol('=') {
            Ok(ComparisonOperator::Equal)
        } else if self.consume_symbol('!') {
            self.expect_symbol('=')?;
            Ok(ComparisonOperator::NotEqual)
        } else if self.consume_symbol('<') {
            if self.consume_symbol('>') {
                Ok(ComparisonOperator::NotEqual)
            } else if self.consume_symbol('=') {
                Ok(ComparisonOperator::LessOrEqual)
            } else {
                Ok(ComparisonOperator::Less)
            }
        } else if self.consume_symbol('>') {
            if self.consume_symbol('=') {
                Ok(ComparisonOperator::GreaterOrEqual)
            } else {
                Ok(ComparisonOperator::Greater)
            }
        } else {
            Err(SqlError::InvalidSyntax)
        }
    }

    fn scalar_operand(&mut self, parameter_count: &mut usize) -> Result<ScalarOperand, SqlError> {
        if self.consume_symbol('?') {
            let position = *parameter_count;
            *parameter_count = parameter_count
                .checked_add(1)
                .ok_or(SqlError::ParameterMismatch)?;
            return Ok(ScalarOperand::Parameter(position));
        }
        if self.consume_keyword("NULL") {
            return Ok(ScalarOperand::Null);
        }
        if self.consume_keyword("TRUE") {
            return Ok(ScalarOperand::Boolean(true));
        }
        if self.consume_keyword("FALSE") {
            return Ok(ScalarOperand::Boolean(false));
        }
        let negative = self.consume_symbol('-');
        if let Some(Token::Number(value)) = self.tokens.get(self.offset) {
            let value = value.clone();
            self.offset += 1;
            let prefix = if negative { "-" } else { "" };
            return Ok(ScalarOperand::Integer(format!("{prefix}{value}")));
        }
        if negative {
            return Err(SqlError::InvalidSyntax);
        }
        if let Some(Token::String(value)) = self.tokens.get(self.offset) {
            let value = value.clone();
            self.offset += 1;
            return Ok(ScalarOperand::Text(value));
        }
        Err(SqlError::InvalidSyntax)
    }

    fn identifier(&mut self) -> Result<String, SqlError> {
        let Some(Token::Word(identifier)) = self.tokens.get(self.offset) else {
            return Err(SqlError::InvalidSyntax);
        };
        let mut qualified = identifier.clone();
        self.offset += 1;
        while self.consume_symbol('.') {
            let Some(Token::Word(identifier)) = self.tokens.get(self.offset) else {
                return Err(SqlError::InvalidSyntax);
            };
            qualified.push('.');
            qualified.push_str(identifier);
            self.offset += 1;
        }
        Ok(qualified)
    }

    fn number_u8(&mut self) -> Result<u8, SqlError> {
        let Some(Token::Number(number)) = self.tokens.get(self.offset) else {
            return Err(SqlError::InvalidSyntax);
        };
        self.offset += 1;
        number.parse().map_err(|_| SqlError::InvalidSyntax)
    }

    fn number_usize(&mut self) -> Result<usize, SqlError> {
        let Some(Token::Number(number)) = self.tokens.get(self.offset) else {
            return Err(SqlError::InvalidSyntax);
        };
        self.offset += 1;
        number.parse().map_err(|_| SqlError::InvalidSyntax)
    }

    fn identifier_list(&mut self, terminator: char) -> Result<Vec<String>, SqlError> {
        let mut identifiers = vec![self.identifier()?];
        while !self.consume_symbol(terminator) {
            self.expect_symbol(',')?;
            identifiers.push(self.identifier()?);
        }
        Ok(identifiers)
    }

    fn identifier_list_until_keyword(&mut self, terminator: &str) -> Result<Vec<String>, SqlError> {
        let mut identifiers = vec![self.identifier()?];
        while !self.tokens.get(self.offset).is_some_and(
            |token| matches!(token, Token::Word(value) if value.eq_ignore_ascii_case(terminator)),
        ) {
            self.expect_symbol(',')?;
            identifiers.push(self.identifier()?);
        }
        Ok(identifiers)
    }

    fn logical_type(&mut self) -> Result<LogicalType, SqlError> {
        let logical_type = if self.consume_keyword("BOOLEAN") || self.consume_keyword("BOOL") {
            LogicalType::Boolean
        } else if self.consume_keyword("TINYINT") || self.consume_keyword("INT8") {
            LogicalType::Signed(IntegerWidth::Bits8)
        } else if self.consume_keyword("SMALLINT") || self.consume_keyword("INT16") {
            LogicalType::Signed(IntegerWidth::Bits16)
        } else if self.consume_keyword("INTEGER")
            || self.consume_keyword("INT")
            || self.consume_keyword("INT32")
        {
            LogicalType::Signed(IntegerWidth::Bits32)
        } else if self.consume_keyword("BIGINT") || self.consume_keyword("INT64") {
            LogicalType::Signed(IntegerWidth::Bits64)
        } else if self.consume_keyword("UINT8") {
            LogicalType::Unsigned(IntegerWidth::Bits8)
        } else if self.consume_keyword("UINT16") {
            LogicalType::Unsigned(IntegerWidth::Bits16)
        } else if self.consume_keyword("UINT32") {
            LogicalType::Unsigned(IntegerWidth::Bits32)
        } else if self.consume_keyword("UINT64") {
            LogicalType::Unsigned(IntegerWidth::Bits64)
        } else if self.consume_keyword("DECIMAL") || self.consume_keyword("NUMERIC") {
            self.expect_symbol('(')?;
            let precision = self.number_u8()?;
            self.expect_symbol(',')?;
            let scale = self.number_u8()?;
            self.expect_symbol(')')?;
            LogicalType::Decimal(
                DecimalType::new(precision, scale).map_err(|_| SqlError::InvalidSyntax)?,
            )
        } else if self.consume_keyword("REAL") || self.consume_keyword("FLOAT32") {
            LogicalType::Float32
        } else if self.consume_keyword("DOUBLE") {
            self.consume_keyword("PRECISION");
            LogicalType::Float64
        } else if self.consume_keyword("FLOAT") || self.consume_keyword("FLOAT64") {
            LogicalType::Float64
        } else if self.consume_keyword("TEXT") {
            LogicalType::Text
        } else if self.consume_keyword("BINARY") {
            LogicalType::Binary
        } else if self.consume_keyword("DATE") {
            LogicalType::Date
        } else if self.consume_keyword("TIME") {
            LogicalType::Time
        } else if self.consume_keyword("TIMESTAMP") {
            LogicalType::Timestamp
        } else if self.consume_keyword("INTERVAL") {
            LogicalType::Interval
        } else if self.consume_keyword("UUID") {
            LogicalType::Uuid
        } else if self.consume_keyword("JSON") {
            LogicalType::Json
        } else {
            return Err(SqlError::InvalidSyntax);
        };
        Ok(logical_type)
    }

    fn finish(self) -> Result<(), SqlError> {
        if self.offset == self.tokens.len() {
            Ok(())
        } else {
            Err(SqlError::InvalidSyntax)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use hyphae_native_types::{DecimalType, IntegerWidth, LogicalType};

    use super::{
        ColumnOperand, ComparisonOperator, FilterExpression, ParsedJoinEquality, Projection,
        ScalarOperand, SqlError, Statement, TruthValue, binary_prefix_successor,
        key_range_is_empty, parse,
    };

    #[test]
    fn legacy_binary_grammar_remains_accepted() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            parse("CREATE TABLE accounts (primary_key BINARY PRIMARY KEY, row BINARY);")?,
            Statement::CreateTable { name, .. } if name == "accounts"
        ));
        assert!(matches!(
            parse("INSERT INTO accounts (primary_key, row) VALUES (?, ?)")?,
            Statement::Insert { name, .. } if name == "accounts"
        ));
        assert!(matches!(
            parse("UPDATE accounts SET row = ? WHERE primary_key = ?")?,
            Statement::Update { name, .. } if name == "accounts"
        ));
        assert!(matches!(
            parse("DELETE FROM accounts WHERE primary_key = ?")?,
            Statement::Delete { name, .. } if name == "accounts"
        ));
        assert!(matches!(
            parse("SELECT row FROM accounts WHERE primary_key = ?")?,
            Statement::Select { name, .. } if name == "accounts"
        ));
        Ok(())
    }

    #[test]
    fn typed_ddl_and_composite_primary_key_parse_exactly() -> Result<(), Box<dyn std::error::Error>>
    {
        let Statement::CreateTable {
            columns,
            primary_key,
            ..
        } = parse(
            "CREATE TABLE events (
                tenant UUID NOT NULL,
                sequence BIGINT,
                amount DECIMAL(18, 4) NULL,
                payload TEXT,
                PRIMARY KEY (tenant, sequence)
            )",
        )?
        else {
            return Err("expected create table".into());
        };
        assert_eq!(primary_key, ["tenant", "sequence"]);
        assert_eq!(columns.len(), 4);
        assert_eq!(columns[0].logical_type, LogicalType::Uuid);
        assert_eq!(
            columns[1].logical_type,
            LogicalType::Signed(IntegerWidth::Bits64)
        );
        assert_eq!(
            columns[2].logical_type,
            LogicalType::Decimal(DecimalType::new(18, 4)?)
        );
        assert_eq!(columns[3].logical_type, LogicalType::Text);
        Ok(())
    }

    #[test]
    fn typed_mutations_parse_operands_and_parameter_order() -> Result<(), Box<dyn std::error::Error>>
    {
        let Statement::Update {
            name,
            assignments,
            predicates,
            parameter_count,
        } = parse(
            "UPDATE events
             SET payload = ?, status = ?
             WHERE sequence = ? AND tenant = ?",
        )?
        else {
            return Err("expected typed update".into());
        };
        assert_eq!(name, "events");
        assert_eq!(
            assignments,
            [
                ColumnOperand {
                    column: "payload".to_owned(),
                    operand: ScalarOperand::Parameter(0),
                },
                ColumnOperand {
                    column: "status".to_owned(),
                    operand: ScalarOperand::Parameter(1),
                },
            ]
        );
        assert_eq!(
            predicates,
            [
                ColumnOperand {
                    column: "sequence".to_owned(),
                    operand: ScalarOperand::Parameter(2),
                },
                ColumnOperand {
                    column: "tenant".to_owned(),
                    operand: ScalarOperand::Parameter(3),
                },
            ]
        );
        assert_eq!(parameter_count, 4);

        let Statement::Delete {
            name,
            predicates,
            parameter_count,
        } = parse("DELETE FROM events WHERE sequence = ? AND tenant = ?")?
        else {
            return Err("expected typed delete".into());
        };
        assert_eq!(name, "events");
        assert_eq!(
            predicates,
            [
                ColumnOperand {
                    column: "sequence".to_owned(),
                    operand: ScalarOperand::Parameter(0),
                },
                ColumnOperand {
                    column: "tenant".to_owned(),
                    operand: ScalarOperand::Parameter(1),
                },
            ]
        );
        assert_eq!(parameter_count, 2);

        let Statement::Insert {
            values,
            parameter_count,
            ..
        } = parse(
            "INSERT INTO events (sequence, payload, active)
             VALUES (-2, 'Mario''s', ?)",
        )?
        else {
            return Err("expected typed insert".into());
        };
        assert_eq!(
            values,
            [
                ColumnOperand {
                    column: "sequence".to_owned(),
                    operand: ScalarOperand::Integer("-2".to_owned()),
                },
                ColumnOperand {
                    column: "payload".to_owned(),
                    operand: ScalarOperand::Text("Mario's".to_owned()),
                },
                ColumnOperand {
                    column: "active".to_owned(),
                    operand: ScalarOperand::Parameter(0),
                },
            ]
        );
        assert_eq!(parameter_count, 1);
        Ok(())
    }

    #[test]
    fn typed_select_projection_and_predicates_parse_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            projection,
            filter,
            parameter_count,
            ..
        } = parse("SELECT payload, amount FROM events WHERE sequence = ? AND tenant = ?")?
        else {
            return Err("expected select".into());
        };
        assert_eq!(
            projection,
            Projection::Columns(vec!["payload".to_owned(), "amount".to_owned()])
        );
        assert_eq!(
            filter,
            Some(FilterExpression::And(
                Box::new(FilterExpression::Comparison {
                    columns: vec!["sequence".to_owned()],
                    operator: ComparisonOperator::Equal,
                    operands: vec![ScalarOperand::Parameter(0)],
                }),
                Box::new(FilterExpression::Comparison {
                    columns: vec!["tenant".to_owned()],
                    operator: ComparisonOperator::Equal,
                    operands: vec![ScalarOperand::Parameter(1)],
                }),
            ))
        );
        assert_eq!(parameter_count, 2);
        assert!(matches!(
            parse("SELECT payload FROM events"),
            Err(SqlError::InvalidSyntax)
        ));
        Ok(())
    }

    #[test]
    fn residual_filter_precedence_and_parameter_positions_parse_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            filter,
            parameter_count,
            ..
        } = parse(
            "SELECT id FROM events
             WHERE NOT active = ? AND note IS NULL OR score <> ?
             LIMIT 8",
        )?
        else {
            return Err("expected residual select".into());
        };
        assert_eq!(parameter_count, 2);
        assert_eq!(
            filter,
            Some(FilterExpression::Or(
                Box::new(FilterExpression::And(
                    Box::new(FilterExpression::Not(Box::new(
                        FilterExpression::Comparison {
                            columns: vec!["active".to_owned()],
                            operator: ComparisonOperator::Equal,
                            operands: vec![ScalarOperand::Parameter(0)],
                        },
                    ))),
                    Box::new(FilterExpression::IsNull {
                        column: "note".to_owned(),
                        negated: false,
                    }),
                )),
                Box::new(FilterExpression::Comparison {
                    columns: vec!["score".to_owned()],
                    operator: ComparisonOperator::NotEqual,
                    operands: vec![ScalarOperand::Parameter(1)],
                }),
            ))
        );
        Ok(())
    }

    #[test]
    fn scalar_literals_parse_exactly_and_preserve_parameter_positions()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            filter,
            parameter_count,
            ..
        } = parse(
            "SELECT id FROM events
             WHERE id >= -2
               AND active = TRUE
               AND note = 'Mario''s'
               AND score > ?
             LIMIT 4",
        )?
        else {
            return Err("expected literal select".into());
        };
        assert_eq!(parameter_count, 1);
        assert_eq!(
            filter,
            Some(FilterExpression::And(
                Box::new(FilterExpression::And(
                    Box::new(FilterExpression::And(
                        Box::new(FilterExpression::Comparison {
                            columns: vec!["id".to_owned()],
                            operator: ComparisonOperator::GreaterOrEqual,
                            operands: vec![ScalarOperand::Integer("-2".to_owned())],
                        }),
                        Box::new(FilterExpression::Comparison {
                            columns: vec!["active".to_owned()],
                            operator: ComparisonOperator::Equal,
                            operands: vec![ScalarOperand::Boolean(true)],
                        }),
                    )),
                    Box::new(FilterExpression::Comparison {
                        columns: vec!["note".to_owned()],
                        operator: ComparisonOperator::Equal,
                        operands: vec![ScalarOperand::Text("Mario's".to_owned())],
                    }),
                )),
                Box::new(FilterExpression::Comparison {
                    columns: vec!["score".to_owned()],
                    operator: ComparisonOperator::Greater,
                    operands: vec![ScalarOperand::Parameter(0)],
                }),
            ))
        );
        assert!(matches!(
            parse("SELECT id FROM events WHERE note = 'unterminated LIMIT 1"),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            parse("SELECT id FROM events WHERE score = -TRUE LIMIT 1"),
            Err(SqlError::InvalidSyntax)
        ));
        Ok(())
    }

    #[test]
    fn three_valued_boolean_truth_tables_are_exact() {
        use TruthValue::{False, True, Unknown};

        let values = [True, False, Unknown];
        let expected_and = [
            [True, False, Unknown],
            [False, False, False],
            [Unknown, False, Unknown],
        ];
        let expected_or = [
            [True, True, True],
            [True, False, Unknown],
            [True, Unknown, Unknown],
        ];
        for (left_index, left) in values.into_iter().enumerate() {
            assert_eq!(left.not().not(), left);
            for (right_index, right) in values.into_iter().enumerate() {
                assert_eq!(left.and(right), expected_and[left_index][right_index]);
                assert_eq!(left.or(right), expected_or[left_index][right_index]);
            }
        }
        assert_eq!(True.not(), False);
        assert_eq!(False.not(), True);
        assert_eq!(Unknown.not(), Unknown);
    }

    #[test]
    fn bounded_primary_key_scan_parses_exactly() -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            projection,
            filter,
            order_by,
            limit,
            ..
        } = parse(
            "SELECT tenant, sequence, payload
             FROM events
             ORDER BY tenant, sequence
             LIMIT 32",
        )?
        else {
            return Err("expected bounded select".into());
        };
        assert_eq!(
            projection,
            Projection::Columns(vec![
                "tenant".to_owned(),
                "sequence".to_owned(),
                "payload".to_owned(),
            ])
        );
        assert!(filter.is_none());
        assert_eq!(order_by, ["tenant", "sequence"]);
        assert_eq!(limit, Some(32));
        assert!(matches!(
            parse("SELECT payload FROM events LIMIT 0")?,
            Statement::Select {
                filter,
                order_by,
                limit: Some(0),
                ..
            } if filter.is_none() && order_by.is_empty()
        ));
        assert!(matches!(
            parse("SELECT payload FROM events ORDER BY tenant"),
            Err(SqlError::InvalidSyntax)
        ));
        Ok(())
    }

    #[test]
    fn bounded_primary_key_range_parses_row_comparisons_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            filter,
            parameter_count,
            order_by,
            limit,
            ..
        } = parse(
            "SELECT payload
             FROM events
             WHERE (tenant, sequence) >= (?, ?)
               AND (tenant, sequence) < (?, ?)
             ORDER BY tenant, sequence
             LIMIT 16",
        )?
        else {
            return Err("expected bounded range select".into());
        };
        assert_eq!(
            filter,
            Some(FilterExpression::And(
                Box::new(FilterExpression::Comparison {
                    columns: vec!["tenant".to_owned(), "sequence".to_owned()],
                    operator: ComparisonOperator::GreaterOrEqual,
                    operands: vec![ScalarOperand::Parameter(0), ScalarOperand::Parameter(1),],
                }),
                Box::new(FilterExpression::Comparison {
                    columns: vec!["tenant".to_owned(), "sequence".to_owned()],
                    operator: ComparisonOperator::Less,
                    operands: vec![ScalarOperand::Parameter(2), ScalarOperand::Parameter(3),],
                }),
            ))
        );
        assert_eq!(parameter_count, 4);
        assert_eq!(order_by, ["tenant", "sequence"]);
        assert_eq!(limit, Some(16));

        assert!(matches!(
            parse("SELECT payload FROM events WHERE sequence > ? LIMIT 5")?,
            Statement::Select {
                filter,
                order_by,
                limit: Some(5),
                ..
            } if filter
                == Some(FilterExpression::Comparison {
                    columns: vec!["sequence".to_owned()],
                    operator: ComparisonOperator::Greater,
                    operands: vec![ScalarOperand::Parameter(0)],
                }) && order_by.is_empty()
        ));
        assert!(matches!(
            parse(
                "SELECT payload FROM events
                 WHERE (tenant, sequence) >= (?, ?)
                 ORDER BY tenant, sequence"
            ),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            parse("SELECT payload FROM events WHERE (tenant) >= (?) LIMIT 5"),
            Err(SqlError::InvalidSyntax)
        ));
        assert!(matches!(
            parse("SELECT payload FROM events WHERE sequence != ? LIMIT 5")?,
            Statement::Select {
                filter: Some(FilterExpression::Comparison {
                    operator: ComparisonOperator::NotEqual,
                    ..
                }),
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn empty_primary_key_ranges_are_detected_without_btree_map_panics() {
        let one = vec![1_u8];
        let two = vec![2_u8];
        assert!(!key_range_is_empty(
            &Bound::Included(one.clone()),
            &Bound::Included(one.clone()),
        ));
        assert!(key_range_is_empty(
            &Bound::Excluded(one.clone()),
            &Bound::Included(one.clone()),
        ));
        assert!(key_range_is_empty(
            &Bound::Included(one.clone()),
            &Bound::Excluded(one.clone()),
        ));
        assert!(key_range_is_empty(
            &Bound::Excluded(one.clone()),
            &Bound::Excluded(one.clone()),
        ));
        assert!(key_range_is_empty(
            &Bound::Included(two),
            &Bound::Included(one),
        ));
        assert!(!key_range_is_empty(&Bound::Unbounded, &Bound::Unbounded,));
    }

    #[test]
    fn binary_prefix_successor_is_minimal_and_handles_terminal_ff_bytes() {
        assert_eq!(
            binary_prefix_successor(&[0x01, 0x02]),
            Some(vec![0x01, 0x03])
        );
        assert_eq!(binary_prefix_successor(&[0x01, 0xff]), Some(vec![0x02]));
        assert_eq!(binary_prefix_successor(&[0xff]), None);
        assert_eq!(binary_prefix_successor(&[]), None);
    }

    #[test]
    fn secondary_index_and_explain_grammar_parse_exactly() -> Result<(), Box<dyn std::error::Error>>
    {
        assert!(matches!(
            parse("CREATE UNIQUE INDEX people_email ON people (email)")?,
            Statement::CreateIndex {
                name,
                table,
                columns,
                unique: true,
            } if name == "people_email" && table == "people" && columns == ["email"]
        ));
        assert!(matches!(
            parse("CREATE INDEX events_tenant_sequence ON events (tenant, sequence)")?,
            Statement::CreateIndex {
                columns,
                unique: false,
                ..
            } if columns == ["tenant", "sequence"]
        ));
        assert!(matches!(
            parse("EXPLAIN SELECT id FROM people WHERE email = ?")?,
            Statement::ExplainSelect { name, filter, .. }
                if name == "people"
                    && filter
                        == Some(FilterExpression::Comparison {
                            columns: vec!["email".to_owned()],
                            operator: ComparisonOperator::Equal,
                            operands: vec![ScalarOperand::Parameter(0)],
                        })
        ));
        Ok(())
    }

    #[test]
    fn indexed_inner_join_grammar_is_exact_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let query = "SELECT users.id, profiles.city
                     FROM users
                     INNER JOIN profiles ON users.profile_id = profiles.id
                     WHERE email = ?";
        let Statement::SelectJoin(join) = parse(query)? else {
            return Err("expected indexed inner join".into());
        };
        assert_eq!(join.left_name, "users");
        assert_eq!(join.right_name, "profiles");
        assert_eq!(join.projection, ["users.id", "profiles.city"]);
        assert_eq!(
            join.equalities,
            [ParsedJoinEquality {
                left_column: "users.profile_id".to_owned(),
                right_column: "profiles.id".to_owned(),
            }]
        );
        assert_eq!(
            join.filter,
            Some(FilterExpression::Comparison {
                columns: vec!["email".to_owned()],
                operator: ComparisonOperator::Equal,
                operands: vec![ScalarOperand::Parameter(0)],
            })
        );
        assert_eq!(join.parameter_count, 1);
        assert!(matches!(
            parse(&format!("EXPLAIN {query}"))?,
            Statement::ExplainSelectJoin(_)
        ));
        for unsupported in [
            "SELECT * FROM users INNER JOIN profiles ON users.profile_id = profiles.id WHERE email = ?",
            "SELECT users.id FROM users INNER JOIN profiles ON users.profile_id = profiles.id",
            "SELECT users.id FROM users LEFT JOIN profiles ON users.profile_id = profiles.id WHERE email = ?",
        ] {
            assert!(matches!(parse(unsupported), Err(SqlError::InvalidSyntax)));
        }
        assert!(matches!(
            parse(
                "SELECT users.id FROM users
                 INNER JOIN profiles ON profile_id = profiles.id
                 WHERE email = ?"
            )?,
            Statement::SelectJoin(_)
        ));
        let Statement::SelectJoin(composite) = parse(
            "SELECT users.id, profiles.city FROM users
             INNER JOIN profiles
               ON users.profile_code = profiles.code
              AND users.region = profiles.region
             WHERE email = ?",
        )?
        else {
            return Err("expected composite indexed inner join".into());
        };
        assert_eq!(
            composite.equalities,
            [
                ParsedJoinEquality {
                    left_column: "users.profile_code".to_owned(),
                    right_column: "profiles.code".to_owned(),
                },
                ParsedJoinEquality {
                    left_column: "users.region".to_owned(),
                    right_column: "profiles.region".to_owned(),
                },
            ]
        );
        Ok(())
    }
}
