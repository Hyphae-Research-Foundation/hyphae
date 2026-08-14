// SPDX-License-Identifier: AGPL-3.0-only

//! Strict DROP INDEX SQL vertical.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

#[test]
fn drop_index_invalidates_plans_and_survives_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-drop-index-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    tx.execute_sql(
        "CREATE TABLE people (id BIGINT PRIMARY KEY, email TEXT NOT NULL)",
        &[],
    )?;
    tx.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
    tx.execute_sql(
        "INSERT INTO people (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.commit()?;
    let prepared = database.prepare_sql_latest("SELECT id FROM people WHERE email = ?")?;
    let mut ddl = database.begin_sql(2, DurabilityClass::Strict)?;
    ddl.execute_sql("DROP INDEX people_email", &[])?;
    ddl.commit()?;
    assert!(matches!(
        database.execute_prepared_latest(&prepared, &[]),
        Err(SqlError::CatalogChanged)
    ));
    assert!(
        database
            .prepare_sql_latest("SELECT id FROM people WHERE email = ?")
            .is_err()
    );
    let mut read = database.begin_sql(3, DurabilityClass::Memory)?;
    assert_eq!(
        read.execute_sql("SELECT email FROM people WHERE id = 1", &[])?,
        SqlResult::Rows {
            columns: vec!["email".to_owned()],
            rows: vec![vec![SqlValue::Text("a@example.com".to_owned())]],
        }
    );
    read.rollback();
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    assert!(
        reopened
            .prepare_sql_latest("SELECT id FROM people WHERE email = ?")
            .is_err()
    );
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
