// SPDX-License-Identifier: Apache-2.0

//! Immediate MATCH SIMPLE foreign keys over native primary keys.

use hyphae_native_runtime::{NativeDatabase, SqlError};
use hyphae_native_types::DurabilityClass;

#[test]
fn foreign_key_rejects_missing_parent_and_survives_reopen() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = std::env::temp_dir().join(format!("hyphae-native-fk-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    tx.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    tx.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    assert!(matches!(
        tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 99)", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    tx.execute_sql("INSERT INTO parents (id) VALUES (99)", &[])?;
    tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 99)", &[])?;
    assert!(matches!(
        tx.execute_sql("UPDATE children SET parent_id = 100 WHERE id = 1", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    assert!(matches!(
        tx.execute_sql("DELETE FROM parents WHERE id = 99", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (2, NULL)", &[])?;
    tx.commit()?;
    drop(database);

    let mut reopened = NativeDatabase::open(&temporary)?;
    let mut invalid = reopened.begin_sql(2, DurabilityClass::Memory)?;
    assert!(matches!(
        invalid.execute_sql("INSERT INTO children (id, parent_id) VALUES (3, 100)", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    invalid.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
