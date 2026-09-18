// SPDX-License-Identifier: Apache-2.0

//! Strict DROP INDEX SQL vertical.

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlError, SqlResult, SqlValue};
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

#[test]
fn drop_index_rejects_foreign_key_dependency_without_mutating_batch()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-drop-index-fk-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT NOT NULL)",
        &[],
    )?;
    let SqlResult::Command {
        object_id: Some(index),
        ..
    } = seed.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?
    else {
        return Err("CREATE INDEX did not return an object ID".into());
    };
    seed.execute_sql(
        "CREATE TABLE invites (id BIGINT PRIMARY KEY, email TEXT, \
         FOREIGN KEY (email) REFERENCES users (email))",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO users (id, email) VALUES (1, 'first@example.test')",
        &[],
    )?;
    seed.commit()?;

    let mut blocked = database.begin_sql(2, DurabilityClass::Strict)?;
    let mutation_count = blocked.mutation_count();
    assert!(matches!(
        blocked.execute_sql("DROP INDEX users_email", &[]),
        Err(SqlError::Runtime(
            NativeRuntimeError::CatalogDependencyConflict { object }
        )) if object == index
    ));
    assert_eq!(blocked.mutation_count(), mutation_count);
    assert_eq!(
        blocked.execute_sql(
            "SELECT id FROM users WHERE email = 'first@example.test'",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![SqlValue::Signed(1)]],
        }
    );
    blocked.execute_sql(
        "INSERT INTO users (id, email) VALUES (2, 'second@example.test')",
        &[],
    )?;
    blocked.execute_sql(
        "INSERT INTO invites (id, email) VALUES (2, 'second@example.test')",
        &[],
    )?;
    blocked.commit()?;
    drop(database);

    let mut reopened = NativeDatabase::open(&temporary)?;
    let mut read = reopened.begin_sql(3, DurabilityClass::Memory)?;
    assert_eq!(
        read.execute_sql(
            "SELECT id FROM users WHERE email = 'second@example.test'",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![SqlValue::Signed(2)]],
        }
    );
    read.rollback();
    drop(reopened);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
