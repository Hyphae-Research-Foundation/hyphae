// SPDX-License-Identifier: Apache-2.0

//! Catalog-declared parameter typing through native SQL prepare and execute.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

type TestError = Box<dyn std::error::Error>;
const INSERT: &str = "INSERT INTO typed_values (id, label, active) VALUES (?, ?, ?)";

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-sql-typed-parameters-{}-{nanos}",
            std::process::id()
        ))))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn parameterized_insert_persists_declared_integer_text_and_boolean_types() -> Result<(), TestError>
{
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE typed_values (id BIGINT PRIMARY KEY, label TEXT NOT NULL, active BOOLEAN NOT NULL)",
        &[],
    )?;
    seed.commit()?;

    let parameters = [
        SqlValue::Signed(7),
        SqlValue::Text("typed".to_owned()),
        SqlValue::Boolean(true),
    ];
    for wrong in [
        [
            SqlValue::Text("7".to_owned()),
            SqlValue::Text("typed".to_owned()),
            SqlValue::Boolean(true),
        ],
        [
            SqlValue::Signed(7),
            SqlValue::Boolean(true),
            SqlValue::Boolean(true),
        ],
        [
            SqlValue::Signed(7),
            SqlValue::Text("typed".to_owned()),
            SqlValue::Signed(1),
        ],
    ] {
        let mut rejected = database.begin(0, DurabilityClass::Strict)?;
        assert!(matches!(
            rejected.execute_sql_dml(INSERT, &wrong),
            Err(SqlError::TypeMismatch)
        ));
        rejected.rollback();
    }
    let mut insert = database.begin(0, DurabilityClass::Strict)?;
    insert.execute_sql_dml(INSERT, &parameters)?;
    insert.commit()?;
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    let prepared = reopened.prepare_sql_latest(
        "SELECT id, label, active FROM typed_values \
         WHERE id = ? AND label = ? AND active = ?",
    )?;
    assert_eq!(prepared.parameter_count(), 3);
    assert_eq!(
        reopened.execute_prepared_latest(&prepared, &parameters)?,
        SqlResult::Rows {
            columns: vec!["id".to_owned(), "label".to_owned(), "active".to_owned()],
            rows: vec![parameters.to_vec()],
        }
    );

    for wrong in [
        [
            SqlValue::Text("7".to_owned()),
            SqlValue::Text("typed".to_owned()),
            SqlValue::Boolean(true),
        ],
        [
            SqlValue::Signed(7),
            SqlValue::Boolean(true),
            SqlValue::Boolean(true),
        ],
        [
            SqlValue::Signed(7),
            SqlValue::Text("typed".to_owned()),
            SqlValue::Signed(1),
        ],
    ] {
        assert!(matches!(
            reopened.execute_prepared_latest(&prepared, &wrong),
            Err(SqlError::TypeMismatch)
        ));
    }
    Ok(())
}
