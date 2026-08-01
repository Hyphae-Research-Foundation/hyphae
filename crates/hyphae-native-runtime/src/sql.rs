// SPDX-License-Identifier: Apache-2.0

use hyphae_native_types::{CatalogVersion, EngineKind, ObjectId};
use thiserror::Error;

use crate::{NativeRuntimeError, NativeSnapshot, NativeWriteBatch};

/// Value accepted or returned by the first native SQL slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SqlValue {
    /// Arbitrary binary SQL value.
    Binary(Vec<u8>),
}

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

/// Native SQL lexer, binder, or execution failure.
#[derive(Debug, Error)]
pub enum SqlError {
    /// The statement is outside the exact first-slice grammar.
    #[error("HYSQL001 invalid or unsupported native SQL syntax")]
    InvalidSyntax,
    /// Parameter arity or type differs from the bound plan.
    #[error("HYSQL002 native SQL parameter mismatch")]
    ParameterMismatch,
    /// A prepared plan's catalog version is no longer current.
    #[error("HYSQL003 native SQL prepared plan requires rebind")]
    CatalogChanged,
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
    SelectByPrimaryKey { table: ObjectId },
}

enum Statement {
    CreateTable { name: String },
    Insert { name: String },
    Update { name: String },
    Delete { name: String },
    Select { name: String },
}

pub(crate) fn prepare(
    snapshot: &NativeSnapshot,
    statement: &str,
) -> Result<PreparedStatement, SqlError> {
    let Statement::Select { name } = parse(statement)? else {
        return Err(SqlError::InvalidSyntax);
    };
    let table = snapshot
        .state
        .catalog
        .id_named(&name, EngineKind::Relational)
        .map_err(NativeRuntimeError::from)?;
    Ok(PreparedStatement {
        catalog_version: snapshot.catalog_version(),
        plan: PreparedPlan::SelectByPrimaryKey { table },
    })
}

pub(crate) fn execute_prepared(
    snapshot: &NativeSnapshot,
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    let [SqlValue::Binary(primary_key)] = parameters else {
        return Err(SqlError::ParameterMismatch);
    };
    let rows = execute_prepared_binary(snapshot, prepared, primary_key)?
        .map_or_else(Vec::new, |row| vec![vec![SqlValue::Binary(row.to_vec())]]);
    Ok(SqlResult::Rows {
        columns: vec!["row".to_owned()],
        rows,
    })
}

pub(crate) fn execute_prepared_binary<'snapshot>(
    snapshot: &'snapshot NativeSnapshot,
    prepared: &PreparedStatement,
    primary_key: &[u8],
) -> Result<Option<&'snapshot [u8]>, SqlError> {
    if prepared.catalog_version != snapshot.catalog_version() {
        return Err(SqlError::CatalogChanged);
    }
    match prepared.plan {
        PreparedPlan::SelectByPrimaryKey { table } => Ok(snapshot.select(table, primary_key)),
    }
}

pub(crate) fn execute_transaction(
    transaction: &mut NativeWriteBatch,
    statement: &str,
    parameters: &[SqlValue],
) -> Result<SqlResult, SqlError> {
    match parse(statement)? {
        Statement::CreateTable { name } => {
            if !parameters.is_empty() {
                return Err(SqlError::ParameterMismatch);
            }
            let id = transaction
                .state
                .catalog
                .next_object_id()
                .map_err(NativeRuntimeError::from)?;
            transaction.create_relation(id, &name)?;
            Ok(SqlResult::Command {
                rows_affected: 0,
                object_id: Some(id),
            })
        }
        Statement::Insert { name } => {
            let [SqlValue::Binary(primary_key), SqlValue::Binary(row)] = parameters else {
                return Err(SqlError::ParameterMismatch);
            };
            let table = transaction
                .state
                .catalog
                .id_named(&name, EngineKind::Relational)
                .map_err(NativeRuntimeError::from)?;
            transaction.insert(table, primary_key.clone(), row.clone())?;
            Ok(SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            })
        }
        Statement::Update { name } => {
            let [SqlValue::Binary(row), SqlValue::Binary(primary_key)] = parameters else {
                return Err(SqlError::ParameterMismatch);
            };
            let table = transaction
                .state
                .catalog
                .id_named(&name, EngineKind::Relational)
                .map_err(NativeRuntimeError::from)?;
            transaction.update(table, primary_key.clone(), row.clone())?;
            Ok(SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            })
        }
        Statement::Delete { name } => {
            let [SqlValue::Binary(primary_key)] = parameters else {
                return Err(SqlError::ParameterMismatch);
            };
            let table = transaction
                .state
                .catalog
                .id_named(&name, EngineKind::Relational)
                .map_err(NativeRuntimeError::from)?;
            transaction.delete(table, primary_key.clone())?;
            Ok(SqlResult::Command {
                rows_affected: 1,
                object_id: None,
            })
        }
        Statement::Select { name } => {
            let [SqlValue::Binary(primary_key)] = parameters else {
                return Err(SqlError::ParameterMismatch);
            };
            let table = transaction
                .state
                .catalog
                .id_named(&name, EngineKind::Relational)
                .map_err(NativeRuntimeError::from)?;
            let rows = transaction
                .select(table, primary_key)
                .map_or_else(Vec::new, |row| vec![vec![SqlValue::Binary(row.to_vec())]]);
            Ok(SqlResult::Rows {
                columns: vec!["row".to_owned()],
                rows,
            })
        }
    }
}

fn parse(statement: &str) -> Result<Statement, SqlError> {
    let mut parser = Parser::new(lex(statement)?);
    let parsed = if parser.consume_keyword("CREATE") {
        parser.expect_keyword("TABLE")?;
        let name = parser.identifier()?;
        parser.expect_symbol("(")?;
        parser.expect_keyword("PRIMARY_KEY")?;
        parser.expect_keyword("BINARY")?;
        parser.expect_keyword("PRIMARY")?;
        parser.expect_keyword("KEY")?;
        parser.expect_symbol(",")?;
        parser.expect_keyword("ROW")?;
        parser.expect_keyword("BINARY")?;
        parser.expect_symbol(")")?;
        Statement::CreateTable { name }
    } else if parser.consume_keyword("INSERT") {
        parser.expect_keyword("INTO")?;
        let name = parser.identifier()?;
        parser.expect_symbol("(")?;
        parser.expect_keyword("PRIMARY_KEY")?;
        parser.expect_symbol(",")?;
        parser.expect_keyword("ROW")?;
        parser.expect_symbol(")")?;
        parser.expect_keyword("VALUES")?;
        parser.expect_symbol("(")?;
        parser.expect_symbol("?")?;
        parser.expect_symbol(",")?;
        parser.expect_symbol("?")?;
        parser.expect_symbol(")")?;
        Statement::Insert { name }
    } else if parser.consume_keyword("UPDATE") {
        let name = parser.identifier()?;
        parser.expect_keyword("SET")?;
        parser.expect_keyword("ROW")?;
        parser.expect_symbol("=")?;
        parser.expect_symbol("?")?;
        parser.expect_keyword("WHERE")?;
        parser.expect_keyword("PRIMARY_KEY")?;
        parser.expect_symbol("=")?;
        parser.expect_symbol("?")?;
        Statement::Update { name }
    } else if parser.consume_keyword("DELETE") {
        parser.expect_keyword("FROM")?;
        let name = parser.identifier()?;
        parser.expect_keyword("WHERE")?;
        parser.expect_keyword("PRIMARY_KEY")?;
        parser.expect_symbol("=")?;
        parser.expect_symbol("?")?;
        Statement::Delete { name }
    } else if parser.consume_keyword("SELECT") {
        parser.expect_keyword("ROW")?;
        parser.expect_keyword("FROM")?;
        let name = parser.identifier()?;
        parser.expect_keyword("WHERE")?;
        parser.expect_keyword("PRIMARY_KEY")?;
        parser.expect_symbol("=")?;
        parser.expect_symbol("?")?;
        Statement::Select { name }
    } else {
        return Err(SqlError::InvalidSyntax);
    };
    parser.consume_symbol(";");
    parser.finish()?;
    Ok(parsed)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Word(String),
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
        } else if matches!(character, '(' | ')' | ',' | '=' | ';' | '?') {
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

    fn consume_symbol(&mut self, expected: &str) -> bool {
        let Some(expected) = expected.chars().next() else {
            return false;
        };
        if self.tokens.get(self.offset) == Some(&Token::Symbol(expected)) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, expected: &str) -> Result<(), SqlError> {
        if self.consume_symbol(expected) {
            Ok(())
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
    use super::{Statement, parse};

    #[test]
    fn first_slice_grammar_is_exact() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            parse("CREATE TABLE accounts (primary_key BINARY PRIMARY KEY, row BINARY);")?,
            Statement::CreateTable { name } if name == "accounts"
        ));
        assert!(matches!(
            parse("INSERT INTO accounts (primary_key, row) VALUES (?, ?)")?,
            Statement::Insert { name } if name == "accounts"
        ));
        assert!(matches!(
            parse("UPDATE accounts SET row = ? WHERE primary_key = ?")?,
            Statement::Update { name } if name == "accounts"
        ));
        assert!(matches!(
            parse("DELETE FROM accounts WHERE primary_key = ?")?,
            Statement::Delete { name } if name == "accounts"
        ));
        assert!(matches!(
            parse("SELECT row FROM accounts WHERE primary_key = ?")?,
            Statement::Select { name } if name == "accounts"
        ));
        assert!(parse("SELECT * FROM accounts").is_err());
        Ok(())
    }
}
