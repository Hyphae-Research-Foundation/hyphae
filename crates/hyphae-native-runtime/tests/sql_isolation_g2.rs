// SPDX-License-Identifier: AGPL-3.0-only

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
fn snapshots_prevent_dirty_reads_and_phantoms_and_rollback_discards()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-isolation-phantom-{}",
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
    seed.execute_sql("INSERT INTO accounts (id, balance) VALUES (3, 300)", &[])?;
    seed.commit()?;

    let mut writer = database.begin_optimistic(2, DurabilityClass::Memory)?;
    writer.execute_sql("INSERT INTO accounts (id, balance) VALUES (2, 200)", &[])?;
    let mut reader = database.begin_optimistic(2, DurabilityClass::Memory)?;
    let SqlResult::Rows { rows: before, .. } =
        reader.execute_sql("SELECT id FROM accounts ORDER BY id LIMIT 10", &[])?
    else {
        return Err("expected rows before commit".into());
    };
    assert_eq!(
        before,
        vec![vec![ScalarValue::Signed(1)], vec![ScalarValue::Signed(3)]]
    );

    database.commit_optimistic(writer)?;
    let SqlResult::Rows { rows: retained, .. } =
        reader.execute_sql("SELECT id FROM accounts ORDER BY id LIMIT 10", &[])?
    else {
        return Err("expected retained rows".into());
    };
    assert_eq!(retained, before, "retained snapshot admitted a phantom");
    reader.rollback();

    let mut current = database.begin_optimistic(3, DurabilityClass::Memory)?;
    let SqlResult::Rows { rows: after, .. } =
        current.execute_sql("SELECT id FROM accounts ORDER BY id LIMIT 10", &[])?
    else {
        return Err("expected rows after commit".into());
    };
    assert_eq!(
        after,
        vec![
            vec![ScalarValue::Signed(1)],
            vec![ScalarValue::Signed(2)],
            vec![ScalarValue::Signed(3)],
        ]
    );
    current.execute_sql("UPDATE accounts SET balance = 999 WHERE id = 1", &[])?;
    current.rollback();
    let mut after_rollback = database.begin_optimistic(4, DurabilityClass::Memory)?;
    assert_eq!(read_balance(&mut after_rollback, 1)?, 100);
    after_rollback.rollback();
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
