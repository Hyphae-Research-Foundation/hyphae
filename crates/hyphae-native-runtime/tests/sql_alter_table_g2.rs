// SPDX-License-Identifier: Apache-2.0

//! Metadata-only ALTER TABLE RENAME vertical.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

#[test]
fn alter_table_rename_keeps_identity_and_rows_and_survives_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-rename-table-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    tx.execute_sql(
        "CREATE TABLE people (id BIGINT PRIMARY KEY, email TEXT)",
        &[],
    )?;
    tx.execute_sql(
        "INSERT INTO people (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.commit()?;
    let prepared = database.prepare_sql_latest("SELECT email FROM people WHERE id = 1")?;
    let mut ddl = database.begin_sql(2, DurabilityClass::Strict)?;
    ddl.execute_sql("ALTER TABLE people RENAME TO contacts", &[])?;
    ddl.commit()?;
    assert!(matches!(
        database.execute_prepared_latest(&prepared, &[]),
        Err(SqlError::CatalogChanged)
    ));
    let mut read = database.begin_sql(3, DurabilityClass::Memory)?;
    let result = read.execute_sql("SELECT email FROM contacts WHERE id = 1", &[])?;
    read.rollback();
    assert_eq!(
        result,
        SqlResult::Rows {
            columns: vec!["email".to_owned()],
            rows: vec![vec![SqlValue::Text("a@example.com".to_owned())]],
        }
    );
    assert!(
        database
            .prepare_sql_latest("SELECT email FROM people WHERE id = 1")
            .is_err()
    );
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    assert!(
        reopened
            .prepare_sql_latest("SELECT email FROM people WHERE id = 1")
            .is_err()
    );
    assert!(
        reopened
            .prepare_sql_latest("SELECT email FROM contacts WHERE id = 1")
            .is_ok()
    );
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
