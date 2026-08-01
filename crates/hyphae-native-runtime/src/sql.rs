// SPDX-License-Identifier: Apache-2.0

use std::ops::Bound;

use hyphae_native_catalog::{
    CatalogName, CatalogObject, ColumnDefinition, ObjectHeader, RelationDefinition,
    SecondaryIndexDefinition,
};
use hyphae_native_mvcc::Snapshot;
use hyphae_native_records::{ColumnValueRef, RowTuple, RowTupleView};
use hyphae_native_types::{
    CatalogVersion, ColumnId, DecimalType, EngineKind, IntegerWidth, LogicalType, ObjectId,
    ScalarValue,
};
use thiserror::Error;

use crate::{NativeDatabase, NativeRuntimeError, NativeSnapshot, NativeWriteBatch, qualified_name};

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreparedPlan {
    PrimaryKeyLookup {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        parameter_columns: Vec<usize>,
        output_columns: Vec<String>,
        legacy_binary: bool,
    },
    SecondaryIndexLookup {
        table: ObjectId,
        index: ObjectId,
        relation: Box<RelationDefinition>,
        index_definition: Box<SecondaryIndexDefinition>,
        projection: Vec<usize>,
        parameter_columns: Vec<usize>,
        output_columns: Vec<String>,
    },
    PrimaryKeyScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        output_columns: Vec<String>,
        limit: usize,
        legacy_binary: bool,
    },
    PrimaryKeyRangeScan {
        table: ObjectId,
        relation: Box<RelationDefinition>,
        projection: Vec<usize>,
        output_columns: Vec<String>,
        range: PrimaryKeyRange,
        limit: usize,
        legacy_binary: bool,
    },
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
        columns: Vec<String>,
    },
    Update {
        name: String,
        assignments: Vec<String>,
        predicates: Vec<String>,
    },
    Delete {
        name: String,
        predicates: Vec<String>,
    },
    Select {
        name: String,
        projection: Projection,
        predicates: Vec<SelectPredicate>,
        order_by: Vec<String>,
        limit: Option<usize>,
    },
    ExplainSelect {
        name: String,
        projection: Projection,
        predicates: Vec<SelectPredicate>,
        order_by: Vec<String>,
        limit: Option<usize>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedColumn {
    name: String,
    logical_type: LogicalType,
    nullable: bool,
    inline_primary_key: bool,
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
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectPredicate {
    columns: Vec<String>,
    operator: ComparisonOperator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyRangeEndpoint {
    parameter_offset: usize,
    parameter_columns: Vec<usize>,
    inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrimaryKeyRange {
    lower: Option<PrimaryKeyRangeEndpoint>,
    upper: Option<PrimaryKeyRangeEndpoint>,
    parameter_count: usize,
}

type PrimaryKeyBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

#[derive(Clone, Copy)]
struct ScanExecution {
    limit: usize,
    legacy_binary: bool,
}

struct BoundSelect {
    table: ObjectId,
    projection: Vec<usize>,
    parameter_columns: Vec<usize>,
    output_columns: Vec<String>,
    access: SelectAccess,
}

enum SelectAccess {
    PrimaryKey {
        legacy_binary: bool,
    },
    SecondaryIndex {
        index: ObjectId,
    },
    PrimaryKeyScan {
        limit: usize,
        legacy_binary: bool,
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
    prepare_catalog(
        snapshot.catalog_version(),
        &snapshot.state.catalog,
        statement,
    )
}

pub(crate) fn prepare_catalog(
    catalog_version: CatalogVersion,
    catalog: &crate::model::CatalogState,
    statement: &str,
) -> Result<PreparedStatement, SqlError> {
    let Statement::Select {
        name,
        projection,
        predicates,
        order_by,
        limit,
    } = parse(statement)?
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let bound = bind_select(catalog, &name, &projection, &predicates, &order_by, limit)?;
    let relation = relation_by_id(catalog, bound.table)?.clone();
    let plan = match bound.access {
        SelectAccess::PrimaryKey { legacy_binary } => PreparedPlan::PrimaryKeyLookup {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            parameter_columns: bound.parameter_columns,
            output_columns: bound.output_columns,
            legacy_binary,
        },
        SelectAccess::SecondaryIndex { index } => PreparedPlan::SecondaryIndexLookup {
            table: bound.table,
            index,
            relation: Box::new(relation),
            index_definition: Box::new(secondary_index_by_id(catalog, index)?.clone()),
            projection: bound.projection,
            parameter_columns: bound.parameter_columns,
            output_columns: bound.output_columns,
        },
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary,
        } => PreparedPlan::PrimaryKeyScan {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            output_columns: bound.output_columns,
            limit,
            legacy_binary,
        },
        SelectAccess::PrimaryKeyRangeScan {
            range,
            limit,
            legacy_binary,
        } => PreparedPlan::PrimaryKeyRangeScan {
            table: bound.table,
            relation: Box::new(relation),
            projection: bound.projection,
            output_columns: bound.output_columns,
            range,
            limit,
            legacy_binary,
        },
    };
    Ok(PreparedStatement {
        catalog_version,
        plan,
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
            parameter_columns,
            legacy_binary: true,
            ..
        } if projection.as_slice() == [1] && parameter_columns.as_slice() == [0] => {
            Ok(snapshot.select(*table, primary_key))
        }
        PreparedPlan::PrimaryKeyLookup { .. }
        | PreparedPlan::SecondaryIndexLookup { .. }
        | PreparedPlan::PrimaryKeyScan { .. }
        | PreparedPlan::PrimaryKeyRangeScan { .. } => Err(SqlError::ParameterMismatch),
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
        } => execute_create(transaction, &name, columns, primary_key, parameters),
        Statement::CreateIndex {
            name,
            table,
            columns,
            unique,
        } => execute_create_index(transaction, &name, &table, &columns, unique, parameters),
        Statement::Insert { name, columns } => {
            execute_insert(transaction, &name, &columns, parameters)
        }
        Statement::Update {
            name,
            assignments,
            predicates,
        } => execute_update(transaction, &name, &assignments, &predicates, parameters),
        Statement::Delete { name, predicates } => {
            execute_delete(transaction, &name, &predicates, parameters)
        }
        Statement::Select {
            name,
            projection,
            predicates,
            order_by,
            limit,
        } => execute_select(
            transaction,
            &name,
            &projection,
            &predicates,
            &order_by,
            limit,
            parameters,
        ),
        Statement::ExplainSelect {
            name,
            projection,
            predicates,
            order_by,
            limit,
        } => execute_explain(
            transaction,
            &name,
            &projection,
            &predicates,
            &order_by,
            limit,
            parameters,
        ),
    }
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
            parameter_columns,
            output_columns,
            legacy_binary,
        } => {
            let primary_key = bind_primary_key(relation, parameter_columns, parameters)?;
            let rows = snapshot.select(*table, &primary_key).map_or_else(
                || Ok(Vec::new()),
                |stored| {
                    materialize_row(
                        relation,
                        projection,
                        *legacy_binary,
                        parameters,
                        parameter_columns,
                        stored,
                    )
                    .map(|row| vec![row])
                },
            )?;
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::SecondaryIndexLookup {
            table,
            index,
            relation,
            index_definition,
            projection,
            parameter_columns,
            output_columns,
        } => {
            let Some(index_key) = bind_secondary_index_key(
                relation,
                index_definition,
                parameter_columns,
                parameters,
            )?
            else {
                return Ok(SqlResult::Rows {
                    columns: output_columns.clone(),
                    rows: Vec::new(),
                });
            };
            let primary_keys = snapshot
                .state
                .relational
                .secondary_index_lookup(*index, &index_key)
                .map_err(NativeRuntimeError::from)?;
            let mut rows = Vec::new();
            if let Some(primary_keys) = primary_keys {
                rows.reserve(primary_keys.len());
                for primary_key in primary_keys {
                    let stored = snapshot
                        .select(*table, primary_key)
                        .ok_or(SqlError::InvalidStoredRow)?;
                    rows.push(materialize_row(
                        relation,
                        projection,
                        false,
                        parameters,
                        parameter_columns,
                        stored,
                    )?);
                }
            }
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::PrimaryKeyScan { .. } => execute_snapshot_scan(snapshot, plan, parameters),
        PreparedPlan::PrimaryKeyRangeScan { .. } => {
            execute_snapshot_range_scan(snapshot, plan, parameters)
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
            parameter_columns,
            output_columns,
            legacy_binary,
        } => {
            let primary_key = bind_primary_key(relation, parameter_columns, parameters)?;
            let rows = database
                .select_relational_at(snapshot, *table, &primary_key)?
                .map_or_else(
                    || Ok(Vec::new()),
                    |stored| {
                        materialize_row(
                            relation,
                            projection,
                            *legacy_binary,
                            parameters,
                            parameter_columns,
                            &stored,
                        )
                        .map(|row| vec![row])
                    },
                )?;
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::SecondaryIndexLookup {
            table,
            index,
            relation,
            index_definition,
            projection,
            parameter_columns,
            output_columns,
        } => {
            let Some(index_key) = bind_secondary_index_key(
                relation,
                index_definition,
                parameter_columns,
                parameters,
            )?
            else {
                return Ok(SqlResult::Rows {
                    columns: output_columns.clone(),
                    rows: Vec::new(),
                });
            };
            let matches = database.select_secondary_index_at(snapshot, *index, &index_key)?;
            let mut rows = Vec::with_capacity(matches.len());
            for matched in matches {
                if matched.table != *table {
                    return Err(SqlError::InvalidCatalogObject);
                }
                rows.push(materialize_row(
                    relation,
                    projection,
                    false,
                    parameters,
                    parameter_columns,
                    &matched.row,
                )?);
            }
            Ok(SqlResult::Rows {
                columns: output_columns.clone(),
                rows,
            })
        }
        PreparedPlan::PrimaryKeyScan { .. } => {
            execute_latest_scan(database, snapshot, plan, parameters)
        }
        PreparedPlan::PrimaryKeyRangeScan { .. } => {
            execute_latest_range_scan(database, snapshot, plan, parameters)
        }
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
        output_columns,
        limit,
        legacy_binary,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    let rows = stored_rows
        .iter()
        .take(*limit)
        .map(|(primary_key, stored)| {
            materialize_scanned_row(relation, projection, *legacy_binary, primary_key, stored)
        })
        .collect::<Result<Vec<_>, _>>()?;
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
        output_columns,
        limit,
        legacy_binary,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let matches = database.scan_relational_at(snapshot, *table, None, *limit)?;
    let rows = matches
        .into_iter()
        .map(|matched| {
            if matched.table != *table {
                return Err(SqlError::InvalidCatalogObject);
            }
            materialize_scanned_row(
                relation,
                projection,
                *legacy_binary,
                &matched.primary_key,
                &matched.row,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SqlResult::Rows {
        columns: output_columns.clone(),
        rows,
    })
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
        output_columns,
        range,
        limit,
        legacy_binary,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let (lower, upper) = bind_primary_key_range(relation, range, parameters)?;
    let stored_rows = snapshot
        .state
        .relational
        .tables
        .get(table)
        .ok_or(SqlError::InvalidStoredRow)?;
    if primary_key_range_is_empty(&lower, &upper) {
        return Ok(SqlResult::Rows {
            columns: output_columns.clone(),
            rows: Vec::new(),
        });
    }
    let rows = stored_rows
        .range((lower, upper))
        .take(*limit)
        .map(|(primary_key, stored)| {
            materialize_scanned_row(relation, projection, *legacy_binary, primary_key, stored)
        })
        .collect::<Result<Vec<_>, _>>()?;
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
        output_columns,
        range,
        limit,
        legacy_binary,
    } = plan
    else {
        return Err(SqlError::InvalidSyntax);
    };
    let (lower, upper) = bind_primary_key_range(relation, range, parameters)?;
    let matches = database.scan_relational_range_at(
        snapshot,
        *table,
        crate::bound_as_slice(&lower),
        crate::bound_as_slice(&upper),
        *limit,
    )?;
    let rows = matches
        .into_iter()
        .map(|matched| {
            if matched.table != *table {
                return Err(SqlError::InvalidCatalogObject);
            }
            materialize_scanned_row(
                relation,
                projection,
                *legacy_binary,
                &matched.primary_key,
                &matched.row,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
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
    name: &str,
    projection: &Projection,
    predicates: &[SelectPredicate],
    order_by: &[String],
    limit: Option<usize>,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    let bound = bind_select(
        &transaction.state.catalog,
        name,
        projection,
        predicates,
        order_by,
        limit,
    )?;
    let plan = match bound.access {
        SelectAccess::PrimaryKey { .. } => {
            format!("PrimaryKeyLookup(table={})", bound.table)
        }
        SelectAccess::SecondaryIndex { index } => {
            format!("SecondaryIndexLookup(table={},index={index})", bound.table)
        }
        SelectAccess::PrimaryKeyScan { limit, .. } => {
            format!("PrimaryKeyScan(table={},limit={limit})", bound.table)
        }
        SelectAccess::PrimaryKeyRangeScan { range, limit, .. } => format!(
            "PrimaryKeyRangeScan(table={},lower={},upper={},limit={limit})",
            bound.table,
            range_bound_name(range.lower.as_ref()),
            range_bound_name(range.upper.as_ref()),
        ),
    };
    Ok(SqlResult::Rows {
        columns: vec!["plan".to_owned()],
        rows: vec![vec![SqlValue::Text(plan)]],
    })
}

fn execute_create(
    transaction: &mut NativeWriteBatch,
    name: &str,
    parsed_columns: Vec<ParsedColumn>,
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
    for (index, parsed) in parsed_columns.into_iter().enumerate() {
        let raw_id = u32::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(SqlError::InvalidSyntax)?;
        columns.push(ColumnDefinition {
            id: ColumnId::new(raw_id).map_err(|_| SqlError::InvalidSyntax)?,
            name: CatalogName::unquoted(parsed.name).map_err(NativeRuntimeError::from)?,
            logical_type: parsed.logical_type,
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
    let mut definition = RelationDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Relational,
            name: qualified_name(name).map_err(NativeRuntimeError::from)?,
        },
        columns,
        primary_key,
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
    supplied_columns: &[String],
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    if supplied_columns.len() != parameters.len() {
        return Err(SqlError::ParameterMismatch);
    }
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let definition = definition.clone();
    let values = bind_insert_values(&definition, supplied_columns, parameters)?;
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
    assignments: &[String],
    predicates: &[String],
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let definition = definition.clone();
    let assignment_columns = bind_update_columns(&definition, assignments)?;
    let predicate_columns = bind_primary_key_columns(&definition, predicates)?;
    if parameters.len() != assignment_columns.len() + predicate_columns.len() {
        return Err(SqlError::ParameterMismatch);
    }
    let (assignment_values, predicate_values) = parameters.split_at(assignment_columns.len());
    let primary_key = bind_primary_key(&definition, &predicate_columns, predicate_values)?;
    let update = if is_legacy_binary_relation(&definition) {
        if assignment_columns.as_slice() != [1] {
            return Err(SqlError::InvalidSyntax);
        }
        legacy_binary_value(assignment_values.first(), false)?
    } else {
        let assignments =
            bind_update_assignments(&definition, &assignment_columns, assignment_values)?;
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
    predicates: &[String],
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let (table, definition) = relation_named(&transaction.state.catalog, name)?;
    let predicate_columns = bind_primary_key_columns(definition, predicates)?;
    let primary_key = bind_primary_key(definition, &predicate_columns, parameters)?;
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
    name: &str,
    projection: &Projection,
    predicates: &[SelectPredicate],
    order_by: &[String],
    limit: Option<usize>,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let bound = bind_select(
        &transaction.state.catalog,
        name,
        projection,
        predicates,
        order_by,
        limit,
    )?;
    let definition = relation_by_id(&transaction.state.catalog, bound.table)?;
    let rows = match bound.access {
        SelectAccess::PrimaryKey { legacy_binary } => {
            let primary_key = bind_primary_key(definition, &bound.parameter_columns, parameters)?;
            transaction.select(bound.table, &primary_key).map_or_else(
                || Ok(Vec::new()),
                |stored| {
                    materialize_row(
                        definition,
                        &bound.projection,
                        legacy_binary,
                        parameters,
                        &bound.parameter_columns,
                        stored,
                    )
                    .map(|row| vec![row])
                },
            )?
        }
        SelectAccess::SecondaryIndex { index } => {
            let index_definition = secondary_index_by_id(&transaction.state.catalog, index)?;
            let index_key = bind_secondary_index_key(
                definition,
                index_definition,
                &bound.parameter_columns,
                parameters,
            )?;
            let primary_keys = index_key
                .as_deref()
                .map(|index_key| {
                    transaction
                        .state
                        .relational
                        .secondary_index_lookup(index, index_key)
                        .map_err(NativeRuntimeError::from)
                })
                .transpose()?
                .flatten();
            let mut rows = Vec::new();
            if let Some(primary_keys) = primary_keys {
                rows.reserve(primary_keys.len());
                for primary_key in primary_keys {
                    let stored = transaction
                        .select(bound.table, primary_key)
                        .ok_or(SqlError::InvalidStoredRow)?;
                    rows.push(materialize_row(
                        definition,
                        &bound.projection,
                        false,
                        parameters,
                        &bound.parameter_columns,
                        stored,
                    )?);
                }
            }
            rows
        }
        SelectAccess::PrimaryKeyScan {
            limit,
            legacy_binary,
        } => transaction_scan_rows(
            transaction,
            bound.table,
            definition,
            &bound.projection,
            limit,
            legacy_binary,
            parameters,
        )?,
        SelectAccess::PrimaryKeyRangeScan {
            range,
            limit,
            legacy_binary,
        } => transaction_range_rows(
            transaction,
            bound.table,
            definition,
            &bound.projection,
            &range,
            ScanExecution {
                limit,
                legacy_binary,
            },
            parameters,
        )?,
    };
    Ok(SqlResult::Rows {
        columns: bound.output_columns,
        rows,
    })
}

fn transaction_scan_rows(
    transaction: &NativeWriteBatch,
    table: ObjectId,
    definition: &RelationDefinition,
    projection: &[usize],
    limit: usize,
    legacy_binary: bool,
    parameters: &[SqlValue],
) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    if !parameters.is_empty() {
        return Err(SqlError::ParameterMismatch);
    }
    transaction
        .state
        .relational
        .tables
        .get(&table)
        .ok_or(SqlError::InvalidStoredRow)?
        .iter()
        .take(limit)
        .map(|(primary_key, stored)| {
            materialize_scanned_row(definition, projection, legacy_binary, primary_key, stored)
        })
        .collect()
}

fn transaction_range_rows(
    transaction: &NativeWriteBatch,
    table: ObjectId,
    definition: &RelationDefinition,
    projection: &[usize],
    range: &PrimaryKeyRange,
    execution: ScanExecution,
    parameters: &[SqlValue],
) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    let (lower, upper) = bind_primary_key_range(definition, range, parameters)?;
    let stored_rows = transaction
        .state
        .relational
        .tables
        .get(&table)
        .ok_or(SqlError::InvalidStoredRow)?;
    if primary_key_range_is_empty(&lower, &upper) {
        return Ok(Vec::new());
    }
    stored_rows
        .range((lower, upper))
        .take(execution.limit)
        .map(|(primary_key, stored)| {
            materialize_scanned_row(
                definition,
                projection,
                execution.legacy_binary,
                primary_key,
                stored,
            )
        })
        .collect()
}

fn bind_select(
    catalog: &crate::model::CatalogState,
    name: &str,
    projection: &Projection,
    predicates: &[SelectPredicate],
    order_by: &[String],
    limit: Option<usize>,
) -> Result<BoundSelect, SqlError> {
    let (table, definition) = relation_named(catalog, name)?;
    let projection = match projection {
        Projection::All => (0..definition.columns.len()).collect(),
        Projection::Columns(names) => names
            .iter()
            .map(|name| column_index(&definition.columns, name))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let expected_primary_key = primary_key_indices(definition)?;
    let legacy_binary = is_legacy_binary_relation(definition);
    let (parameter_columns, access) = if predicates.is_empty() {
        let limit = limit.ok_or(SqlError::InvalidSyntax)?;
        validate_primary_key_order(definition, order_by, &expected_primary_key)?;
        (
            Vec::new(),
            SelectAccess::PrimaryKeyScan {
                limit,
                legacy_binary,
            },
        )
    } else if predicates
        .iter()
        .all(|predicate| predicate.operator == ComparisonOperator::Equal)
    {
        if !order_by.is_empty() || limit.is_some() {
            return Err(SqlError::InvalidSyntax);
        }
        let parameter_columns = bind_equality_predicate_columns(definition, predicates)?;
        let access = bind_equality_access(
            catalog,
            table,
            definition,
            &parameter_columns,
            &expected_primary_key,
            legacy_binary,
        )?;
        (parameter_columns, access)
    } else {
        let limit = limit.ok_or(SqlError::InvalidSyntax)?;
        validate_primary_key_order(definition, order_by, &expected_primary_key)?;
        let range = bind_primary_key_range_shape(definition, predicates, &expected_primary_key)?;
        (
            Vec::new(),
            SelectAccess::PrimaryKeyRangeScan {
                range,
                limit,
                legacy_binary,
            },
        )
    };
    let output_columns = projection
        .iter()
        .map(|index| definition.columns[*index].name.display().to_owned())
        .collect();
    Ok(BoundSelect {
        table,
        projection,
        parameter_columns,
        output_columns,
        access,
    })
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

fn bind_equality_predicate_columns(
    definition: &RelationDefinition,
    predicates: &[SelectPredicate],
) -> Result<Vec<usize>, SqlError> {
    let mut columns = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        let [name] = predicate.columns.as_slice() else {
            return Err(SqlError::InvalidSyntax);
        };
        let column = column_index(&definition.columns, name)?;
        if columns.contains(&column) {
            return Err(SqlError::DuplicateColumn);
        }
        columns.push(column);
    }
    Ok(columns)
}

fn bind_equality_access(
    catalog: &crate::model::CatalogState,
    table: ObjectId,
    definition: &RelationDefinition,
    parameter_columns: &[usize],
    expected_primary_key: &[usize],
    legacy_binary: bool,
) -> Result<SelectAccess, SqlError> {
    if same_column_set(parameter_columns, expected_primary_key) {
        return Ok(SelectAccess::PrimaryKey { legacy_binary });
    }
    let secondary_index = catalog.objects.iter().find_map(|(id, object)| {
        let CatalogObject::SecondaryIndex(index) = object else {
            return None;
        };
        if index.relation != table {
            return None;
        }
        let columns = secondary_index_column_indices(definition, index).ok()?;
        same_column_set(parameter_columns, &columns).then_some(*id)
    });
    match secondary_index {
        Some(index) => Ok(SelectAccess::SecondaryIndex { index }),
        None if parameter_columns
            .iter()
            .any(|column| expected_primary_key.contains(column)) =>
        {
            Err(SqlError::InvalidPrimaryKey)
        }
        None => Err(SqlError::NoAccessPath),
    }
}

fn bind_primary_key_range_shape(
    definition: &RelationDefinition,
    predicates: &[SelectPredicate],
    expected_primary_key: &[usize],
) -> Result<PrimaryKeyRange, SqlError> {
    let mut lower = None;
    let mut upper = None;
    let mut parameter_count = 0_usize;
    for predicate in predicates {
        let columns = predicate
            .columns
            .iter()
            .map(|name| column_index(&definition.columns, name))
            .collect::<Result<Vec<_>, _>>()?;
        if columns != expected_primary_key {
            return Err(SqlError::InvalidPrimaryKey);
        }
        let endpoint = PrimaryKeyRangeEndpoint {
            parameter_offset: parameter_count,
            parameter_columns: columns,
            inclusive: matches!(
                predicate.operator,
                ComparisonOperator::GreaterOrEqual | ComparisonOperator::LessOrEqual
            ),
        };
        parameter_count = parameter_count
            .checked_add(endpoint.parameter_columns.len())
            .ok_or(SqlError::ParameterMismatch)?;
        match predicate.operator {
            ComparisonOperator::Greater | ComparisonOperator::GreaterOrEqual => {
                if lower.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
            }
            ComparisonOperator::Less | ComparisonOperator::LessOrEqual => {
                if upper.replace(endpoint).is_some() {
                    return Err(SqlError::InvalidPrimaryKey);
                }
            }
            ComparisonOperator::Equal => return Err(SqlError::InvalidSyntax),
        }
    }
    if lower.is_none() && upper.is_none() {
        return Err(SqlError::InvalidPrimaryKey);
    }
    Ok(PrimaryKeyRange {
        lower,
        upper,
        parameter_count,
    })
}

fn range_bound_name(endpoint: Option<&PrimaryKeyRangeEndpoint>) -> &'static str {
    match endpoint {
        Some(endpoint) if endpoint.inclusive => "inclusive",
        Some(_) => "exclusive",
        None => "unbounded",
    }
}

fn same_column_set(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len() && left.iter().all(|column| right.contains(column))
}

fn bind_insert_values<'value>(
    definition: &RelationDefinition,
    supplied_columns: &[String],
    parameters: &'value [SqlValue],
) -> Result<Vec<Option<&'value SqlValue>>, SqlError> {
    let mut values = vec![None; definition.columns.len()];
    for (name, value) in supplied_columns.iter().zip(parameters) {
        let index = column_index(&definition.columns, name)?;
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
    Ok(values)
}

fn bind_update_columns(
    definition: &RelationDefinition,
    assignments: &[String],
) -> Result<Vec<usize>, SqlError> {
    if assignments.is_empty() {
        return Err(SqlError::InvalidSyntax);
    }
    let mut columns = Vec::with_capacity(assignments.len());
    let primary_key = primary_key_indices(definition)?;
    for name in assignments {
        let column = column_index(&definition.columns, name)?;
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
    predicates: &[String],
) -> Result<Vec<usize>, SqlError> {
    let mut columns = Vec::with_capacity(predicates.len());
    for name in predicates {
        let column = column_index(&definition.columns, name)?;
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

fn bind_primary_key_range(
    definition: &RelationDefinition,
    range: &PrimaryKeyRange,
    parameters: &[SqlValue],
) -> Result<PrimaryKeyBounds, SqlError> {
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

fn primary_key_range_is_empty(lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
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
    let end = endpoint
        .parameter_offset
        .checked_add(endpoint.parameter_columns.len())
        .ok_or(SqlError::ParameterMismatch)?;
    let values = parameters
        .get(endpoint.parameter_offset..end)
        .ok_or(SqlError::ParameterMismatch)?;
    let primary_key = bind_primary_key(definition, &endpoint.parameter_columns, values)?;
    Ok(Some((primary_key, endpoint.inclusive)))
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

fn materialize_row(
    definition: &RelationDefinition,
    projection: &[usize],
    legacy_binary: bool,
    parameters: &[SqlValue],
    parameter_columns: &[usize],
    stored: &[u8],
) -> Result<Vec<SqlValue>, SqlError> {
    if legacy_binary {
        let primary_key = parameters
            .iter()
            .zip(parameter_columns)
            .find_map(|(value, column)| (*column == 0).then_some(value))
            .ok_or(SqlError::InvalidPrimaryKey)?;
        let SqlValue::Binary(primary_key) = primary_key else {
            return Err(SqlError::InvalidPrimaryKey);
        };
        return materialize_legacy_row(projection, primary_key, stored);
    }
    materialize_typed_row(definition, projection, stored)
}

fn materialize_scanned_row(
    definition: &RelationDefinition,
    projection: &[usize],
    legacy_binary: bool,
    primary_key: &[u8],
    stored: &[u8],
) -> Result<Vec<SqlValue>, SqlError> {
    if legacy_binary {
        materialize_legacy_row(projection, primary_key, stored)
    } else {
        materialize_typed_row(definition, projection, stored)
    }
}

fn materialize_legacy_row(
    projection: &[usize],
    primary_key: &[u8],
    stored: &[u8],
) -> Result<Vec<SqlValue>, SqlError> {
    projection
        .iter()
        .map(|index| match index {
            0 => Ok(SqlValue::Binary(primary_key.to_vec())),
            1 => Ok(SqlValue::Binary(stored.to_vec())),
            _ => Err(SqlError::InvalidStoredRow),
        })
        .collect()
}

fn materialize_typed_row(
    definition: &RelationDefinition,
    projection: &[usize],
    stored: &[u8],
) -> Result<Vec<SqlValue>, SqlError> {
    let tuple = RowTupleView::decode(stored).map_err(|_| SqlError::InvalidStoredRow)?;
    if tuple.column_count() != definition.columns.len() {
        return Err(SqlError::InvalidStoredRow);
    }
    projection
        .iter()
        .map(|index| {
            let column = definition
                .columns
                .get(*index)
                .ok_or(SqlError::InvalidStoredRow)?;
            match tuple.value(*index).ok_or(SqlError::InvalidStoredRow)? {
                ColumnValueRef::Null => Ok(SqlValue::Null),
                ColumnValueRef::Bytes(encoded) => {
                    SqlValue::decode_storage(&column.logical_type, encoded)
                        .map_err(|_| SqlError::InvalidStoredRow)
                }
            }
        })
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

fn map_runtime_error(error: NativeRuntimeError) -> SqlError {
    match error {
        NativeRuntimeError::UniqueSecondaryIndexViolation => SqlError::UniqueViolation,
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
    let parsed = if parser.consume_keyword("CREATE") {
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
        let Statement::Select {
            name,
            projection,
            predicates,
            order_by,
            limit,
        } = parse_select(&mut parser)?
        else {
            return Err(SqlError::InvalidSyntax);
        };
        Statement::ExplainSelect {
            name,
            projection,
            predicates,
            order_by,
            limit,
        }
    } else {
        return Err(SqlError::InvalidSyntax);
    };
    parser.consume_symbol(';');
    parser.finish()?;
    Ok(parsed)
}

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
                } else {
                    break;
                }
            }
            columns.push(ParsedColumn {
                name: column_name,
                logical_type,
                nullable,
                inline_primary_key,
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
    for index in 0..columns.len() {
        if index != 0 {
            parser.expect_symbol(',')?;
        }
        parser.expect_symbol('?')?;
    }
    parser.expect_symbol(')')?;
    Ok(Statement::Insert { name, columns })
}

fn parse_update(parser: &mut Parser) -> Result<Statement, SqlError> {
    let name = parser.identifier()?;
    parser.expect_keyword("SET")?;
    let mut assignments = Vec::new();
    loop {
        assignments.push(parser.identifier()?);
        parser.expect_symbol('=')?;
        parser.expect_symbol('?')?;
        if !parser.consume_symbol(',') {
            break;
        }
    }
    parser.expect_keyword("WHERE")?;
    let predicates = parse_parameter_predicates(parser)?;
    Ok(Statement::Update {
        name,
        assignments,
        predicates,
    })
}

fn parse_delete(parser: &mut Parser) -> Result<Statement, SqlError> {
    parser.expect_keyword("FROM")?;
    let name = parser.identifier()?;
    parser.expect_keyword("WHERE")?;
    let predicates = parse_parameter_predicates(parser)?;
    Ok(Statement::Delete { name, predicates })
}

fn parse_select(parser: &mut Parser) -> Result<Statement, SqlError> {
    let projection = if parser.consume_symbol('*') {
        Projection::All
    } else {
        Projection::Columns(parser.identifier_list_until_keyword("FROM")?)
    };
    parser.expect_keyword("FROM")?;
    let name = parser.identifier()?;
    let predicates = if parser.consume_keyword("WHERE") {
        parse_select_predicates(parser)?
    } else {
        Vec::new()
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
    if predicates.is_empty() && limit.is_none() {
        return Err(SqlError::InvalidSyntax);
    }
    let equality_only = predicates
        .iter()
        .all(|predicate| predicate.operator == ComparisonOperator::Equal);
    if !predicates.is_empty() && equality_only && (!order_by.is_empty() || limit.is_some()) {
        return Err(SqlError::InvalidSyntax);
    }
    if !predicates.is_empty() && !equality_only && limit.is_none() {
        return Err(SqlError::InvalidSyntax);
    }
    Ok(Statement::Select {
        name,
        projection,
        predicates,
        order_by,
        limit,
    })
}

fn parse_select_predicates(parser: &mut Parser) -> Result<Vec<SelectPredicate>, SqlError> {
    let mut predicates = Vec::new();
    loop {
        predicates.push(parse_select_predicate(parser)?);
        if !parser.consume_keyword("AND") {
            break;
        }
    }
    Ok(predicates)
}

fn parse_select_predicate(parser: &mut Parser) -> Result<SelectPredicate, SqlError> {
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
    let operator = parser.comparison_operator()?;
    if row_value {
        parser.expect_symbol('(')?;
        for index in 0..columns.len() {
            if index != 0 {
                parser.expect_symbol(',')?;
            }
            parser.expect_symbol('?')?;
        }
        parser.expect_symbol(')')?;
    } else {
        parser.expect_symbol('?')?;
    }
    Ok(SelectPredicate { columns, operator })
}

fn parse_parameter_predicates(parser: &mut Parser) -> Result<Vec<String>, SqlError> {
    let mut predicates = Vec::new();
    loop {
        predicates.push(parser.identifier()?);
        parser.expect_symbol('=')?;
        parser.expect_symbol('?')?;
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
            '(' | ')' | ',' | '=' | '<' | '>' | ';' | '?' | '*'
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

    fn comparison_operator(&mut self) -> Result<ComparisonOperator, SqlError> {
        if self.consume_symbol('=') {
            Ok(ComparisonOperator::Equal)
        } else if self.consume_symbol('<') {
            if self.consume_symbol('=') {
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

    fn identifier(&mut self) -> Result<String, SqlError> {
        let Some(Token::Word(identifier)) = self.tokens.get(self.offset) else {
            return Err(SqlError::InvalidSyntax);
        };
        self.offset += 1;
        Ok(identifier.clone())
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
        ComparisonOperator, Projection, SelectPredicate, SqlError, Statement, parse,
        primary_key_range_is_empty,
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
    fn typed_update_delete_parse_assignment_and_predicate_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Update {
            name,
            assignments,
            predicates,
        } = parse(
            "UPDATE events
             SET payload = ?, status = ?
             WHERE sequence = ? AND tenant = ?",
        )?
        else {
            return Err("expected typed update".into());
        };
        assert_eq!(name, "events");
        assert_eq!(assignments, ["payload", "status"]);
        assert_eq!(predicates, ["sequence", "tenant"]);

        let Statement::Delete { name, predicates } =
            parse("DELETE FROM events WHERE sequence = ? AND tenant = ?")?
        else {
            return Err("expected typed delete".into());
        };
        assert_eq!(name, "events");
        assert_eq!(predicates, ["sequence", "tenant"]);
        Ok(())
    }

    #[test]
    fn typed_select_projection_and_predicates_parse_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            projection,
            predicates,
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
            predicates,
            [
                SelectPredicate {
                    columns: vec!["sequence".to_owned()],
                    operator: ComparisonOperator::Equal,
                },
                SelectPredicate {
                    columns: vec!["tenant".to_owned()],
                    operator: ComparisonOperator::Equal,
                },
            ]
        );
        assert!(matches!(
            parse("SELECT payload FROM events"),
            Err(SqlError::InvalidSyntax)
        ));
        Ok(())
    }

    #[test]
    fn bounded_primary_key_scan_parses_exactly() -> Result<(), Box<dyn std::error::Error>> {
        let Statement::Select {
            projection,
            predicates,
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
        assert!(predicates.is_empty());
        assert_eq!(order_by, ["tenant", "sequence"]);
        assert_eq!(limit, Some(32));
        assert!(matches!(
            parse("SELECT payload FROM events LIMIT 0")?,
            Statement::Select {
                predicates,
                order_by,
                limit: Some(0),
                ..
            } if predicates.is_empty() && order_by.is_empty()
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
            predicates,
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
            predicates,
            [
                SelectPredicate {
                    columns: vec!["tenant".to_owned(), "sequence".to_owned()],
                    operator: ComparisonOperator::GreaterOrEqual,
                },
                SelectPredicate {
                    columns: vec!["tenant".to_owned(), "sequence".to_owned()],
                    operator: ComparisonOperator::Less,
                },
            ]
        );
        assert_eq!(order_by, ["tenant", "sequence"]);
        assert_eq!(limit, Some(16));

        assert!(matches!(
            parse("SELECT payload FROM events WHERE sequence > ? LIMIT 5")?,
            Statement::Select {
                predicates,
                order_by,
                limit: Some(5),
                ..
            } if predicates
                == [SelectPredicate {
                    columns: vec!["sequence".to_owned()],
                    operator: ComparisonOperator::Greater,
                }] && order_by.is_empty()
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
            parse("SELECT payload FROM events WHERE sequence != ? LIMIT 5"),
            Err(SqlError::InvalidSyntax)
        ));
        Ok(())
    }

    #[test]
    fn empty_primary_key_ranges_are_detected_without_btree_map_panics() {
        let one = vec![1_u8];
        let two = vec![2_u8];
        assert!(!primary_key_range_is_empty(
            &Bound::Included(one.clone()),
            &Bound::Included(one.clone()),
        ));
        assert!(primary_key_range_is_empty(
            &Bound::Excluded(one.clone()),
            &Bound::Included(one.clone()),
        ));
        assert!(primary_key_range_is_empty(
            &Bound::Included(one.clone()),
            &Bound::Excluded(one.clone()),
        ));
        assert!(primary_key_range_is_empty(
            &Bound::Excluded(one.clone()),
            &Bound::Excluded(one.clone()),
        ));
        assert!(primary_key_range_is_empty(
            &Bound::Included(two),
            &Bound::Included(one),
        ));
        assert!(!primary_key_range_is_empty(
            &Bound::Unbounded,
            &Bound::Unbounded,
        ));
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
            Statement::ExplainSelect { name, predicates, .. }
                if name == "people"
                    && predicates
                        == [SelectPredicate {
                            columns: vec!["email".to_owned()],
                            operator: ComparisonOperator::Equal,
                        }]
        ));
        Ok(())
    }
}
