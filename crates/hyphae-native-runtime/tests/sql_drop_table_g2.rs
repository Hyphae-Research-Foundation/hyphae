// SPDX-License-Identifier: AGPL-3.0-only

//! Strict DROP TABLE RESTRICT vertical.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult};
use hyphae_native_types::DurabilityClass;

#[test]
fn drop_table_is_restrictive_invalidates_plans_and_survives_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-drop-table-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    tx.execute_sql(
        "CREATE TABLE people (id BIGINT PRIMARY KEY, email TEXT)",
        &[],
    )?;
    tx.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
    tx.execute_sql(
        "INSERT INTO people (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.commit()?;
    let prepared = database.prepare_sql_latest("SELECT email FROM people WHERE id = 1")?;
    let mut blocked = database.begin_sql(2, DurabilityClass::Memory)?;
    assert!(blocked.execute_sql("DROP TABLE people", &[]).is_err());
    blocked.rollback();
    let mut ddl = database.begin_sql(3, DurabilityClass::Strict)?;
    ddl.execute_sql("DROP INDEX people_email", &[])?;
    ddl.execute_sql("DROP TABLE people", &[])?;
    ddl.commit()?;
    let mut recreate = database.begin_sql(4, DurabilityClass::Strict)?;
    let SqlResult::Command {
        object_id: Some(recreated_id),
        ..
    } = recreate.execute_sql("CREATE TABLE people (id BIGINT PRIMARY KEY)", &[])?
    else {
        return Err("CREATE TABLE did not return an object ID".into());
    };
    assert!(recreated_id.get() > 2);
    recreate.commit()?;
    assert!(matches!(
        database.execute_prepared_latest(&prepared, &[]),
        Err(SqlError::CatalogChanged)
    ));
    assert!(
        database
            .prepare_sql_latest("SELECT email FROM people WHERE id = 1")
            .is_err()
    );
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    assert!(
        reopened
            .prepare_sql_latest("SELECT id FROM people WHERE id = 1")
            .is_ok()
    );
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
