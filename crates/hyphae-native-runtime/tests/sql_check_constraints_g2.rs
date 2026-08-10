// SPDX-License-Identifier: GPL-3.0-only

//! Persistent column-level CHECK constraints for native SQL.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult};
use hyphae_native_types::DurabilityClass;

#[test]
fn check_constraints_reject_invalid_insert_update_and_survive_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-check-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Strict)?;
    transaction.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL CHECK (balance >= 0))",
        &[],
    )?;
    assert!(matches!(
        transaction.execute_sql("INSERT INTO accounts (id, balance) VALUES (1, -1)", &[]),
        Err(SqlError::CheckViolation)
    ));
    assert!(matches!(
        transaction.execute_sql("INSERT INTO accounts (id, balance) VALUES (1, 10)", &[])?,
        SqlResult::Command {
            rows_affected: 1,
            ..
        }
    ));
    transaction.commit()?;
    drop(database);
    let mut reopened = NativeDatabase::open(&temporary)?;
    let mut update = reopened.begin_sql(2, DurabilityClass::Memory)?;
    assert!(matches!(
        update.execute_sql("UPDATE accounts SET balance = -5 WHERE id = 1", &[]),
        Err(SqlError::CheckViolation)
    ));
    update.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
