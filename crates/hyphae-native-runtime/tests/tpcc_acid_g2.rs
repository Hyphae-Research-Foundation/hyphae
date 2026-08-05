// SPDX-License-Identifier: Apache-2.0

//! Bounded TPC-C ACID vertical for native SQL transactions.

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

fn signed_cell(result: SqlResult) -> Result<i64, Box<dyn std::error::Error>> {
    let SqlResult::Rows { rows, .. } = result else {
        return Err("expected rows".into());
    };
    let Some(ScalarValue::Signed(value)) = rows.first().and_then(|row| row.first()) else {
        return Err("expected signed cell".into());
    };
    Ok(*value)
}

#[test]
fn new_order_like_transaction_is_atomic_durable_and_conflict_safe()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-native-tpcc-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE district (d_id BIGINT PRIMARY KEY, d_next_o_id BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "CREATE TABLE orders (o_id BIGINT PRIMARY KEY, o_d_id BIGINT NOT NULL, o_total BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO district (d_id, d_next_o_id) VALUES (1, 3001)",
        &[],
    )?;
    seed.commit()?;

    let mut first = database.begin_optimistic(2, DurabilityClass::Strict)?;
    let mut second = database.begin_optimistic(2, DurabilityClass::Strict)?;
    first.execute_sql("UPDATE district SET d_next_o_id = 3002 WHERE d_id = 1", &[])?;
    first.execute_sql(
        "INSERT INTO orders (o_id, o_d_id, o_total) VALUES (3001, 1, 42)",
        &[],
    )?;
    second.execute_sql("UPDATE district SET d_next_o_id = 3002 WHERE d_id = 1", &[])?;
    second.execute_sql(
        "INSERT INTO orders (o_id, o_d_id, o_total) VALUES (3001, 1, 99)",
        &[],
    )?;
    database.commit_optimistic(first)?;
    assert!(matches!(
        database.commit_optimistic(second),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    drop(database);

    let reopened = NativeDatabase::open(&temporary)?;
    let mut observed = reopened.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT d_next_o_id FROM district WHERE d_id = 1", &[])?)?,
        3002
    );
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT o_total FROM orders WHERE o_id = 3001", &[])?)?,
        42
    );
    observed.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn aborted_new_order_like_transaction_publishes_nothing() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-tpcc-abort-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE district (d_id BIGINT PRIMARY KEY, d_next_o_id BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "CREATE TABLE orders (o_id BIGINT PRIMARY KEY, o_d_id BIGINT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO district (d_id, d_next_o_id) VALUES (1, 7)",
        &[],
    )?;
    seed.commit()?;
    let mut aborted = database.begin_optimistic(2, DurabilityClass::Memory)?;
    aborted.execute_sql("UPDATE district SET d_next_o_id = 8 WHERE d_id = 1", &[])?;
    aborted.execute_sql("INSERT INTO orders (o_id, o_d_id) VALUES (7, 1)", &[])?;
    aborted.rollback();
    let mut observed = database.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT d_next_o_id FROM district WHERE d_id = 1", &[])?)?,
        7
    );
    let SqlResult::Rows { rows, .. } =
        observed.execute_sql("SELECT o_id FROM orders WHERE o_id = 7", &[])?
    else {
        return Err("expected rows".into());
    };
    assert!(rows.is_empty());
    observed.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
