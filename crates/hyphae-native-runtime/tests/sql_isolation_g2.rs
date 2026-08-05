// SPDX-License-Identifier: Apache-2.0

//! G2 isolation litmus for native optimistic SQL transactions.

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

fn read_balance(
    batch: &mut hyphae_native_runtime::NativeWriteBatch,
    id: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    let SqlResult::Rows { rows, .. } = batch.execute_sql(
        "SELECT balance FROM accounts WHERE id = ?",
        &[ScalarValue::Signed(id)],
    )?
    else {
        return Err("expected rows".into());
    };
    let Some(ScalarValue::Signed(value)) = rows.first().and_then(|row| row.first()) else {
        return Err("expected one signed balance".into());
    };
    Ok(*value)
}

#[test]
fn optimistic_sql_is_repeatable_and_first_committer_wins() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-isolation-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql("INSERT INTO accounts (id, balance) VALUES (1, 100)", &[])?;
    seed.commit()?;

    let mut first = database.begin_optimistic(2, DurabilityClass::Memory)?;
    let mut second = database.begin_optimistic(2, DurabilityClass::Memory)?;
    assert_eq!(read_balance(&mut first, 1)?, 100);
    assert_eq!(read_balance(&mut second, 1)?, 100);
    first.execute_sql("UPDATE accounts SET balance = 90 WHERE id = 1", &[])?;
    second.execute_sql("UPDATE accounts SET balance = 80 WHERE id = 1", &[])?;
    database.commit_optimistic(first)?;
    assert_eq!(read_balance(&mut second, 1)?, 80);
    assert!(matches!(
        database.commit_optimistic(second),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut latest = database.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(read_balance(&mut latest, 1)?, 90);
    latest.rollback();
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    let mut observed = reopened.begin_optimistic(4, DurabilityClass::Memory)?;
    assert_eq!(read_balance(&mut observed, 1)?, 90);
    observed.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn disjoint_optimistic_sql_writes_both_commit() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-isolation-disjoint-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql("INSERT INTO accounts (id, balance) VALUES (1, 100)", &[])?;
    seed.execute_sql("INSERT INTO accounts (id, balance) VALUES (2, 100)", &[])?;
    seed.commit()?;
    let mut first = database.begin_optimistic(2, DurabilityClass::Memory)?;
    let mut second = database.begin_optimistic(2, DurabilityClass::Memory)?;
    first.execute_sql("UPDATE accounts SET balance = 90 WHERE id = 1", &[])?;
    second.execute_sql("UPDATE accounts SET balance = 80 WHERE id = 2", &[])?;
    database.commit_optimistic(first)?;
    database.commit_optimistic(second)?;
    let mut observed = database.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(read_balance(&mut observed, 1)?, 90);
    assert_eq!(read_balance(&mut observed, 2)?, 80);
    observed.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
