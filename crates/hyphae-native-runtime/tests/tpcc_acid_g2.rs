// SPDX-License-Identifier: AGPL-3.0-only

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
fn order_status_and_stock_level_like_reads_are_snapshot_consistent()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-tpcc-reads-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    for statement in [
        "CREATE TABLE customer (c_id BIGINT PRIMARY KEY, c_balance BIGINT NOT NULL)",
        "CREATE TABLE orders (o_id BIGINT PRIMARY KEY, o_customer_id BIGINT NOT NULL, o_total BIGINT NOT NULL)",
        "CREATE TABLE stock (s_item_id BIGINT PRIMARY KEY, s_quantity BIGINT NOT NULL)",
        "INSERT INTO customer (c_id, c_balance) VALUES (100, 500)",
        "INSERT INTO orders (o_id, o_customer_id, o_total) VALUES (7, 100, 42)",
        "INSERT INTO stock (s_item_id, s_quantity) VALUES (1, 9)",
        "INSERT INTO stock (s_item_id, s_quantity) VALUES (2, 30)",
    ] {
        seed.execute_sql(statement, &[])?;
    }
    seed.commit()?;

    let mut reader = database.begin_optimistic(2, DurabilityClass::Memory)?;
    assert_eq!(
        signed_cell(reader.execute_sql("SELECT c_balance FROM customer WHERE c_id = 100", &[])?)?,
        500
    );
    assert_eq!(
        signed_cell(reader.execute_sql("SELECT o_total FROM orders WHERE o_id = 7", &[])?)?,
        42
    );
    let SqlResult::Rows {
        rows: low_stock, ..
    } = reader.execute_sql(
        "SELECT s_item_id FROM stock WHERE s_quantity < 10 ORDER BY s_item_id LIMIT 100",
        &[],
    )?
    else {
        return Err("expected stock rows".into());
    };
    assert_eq!(low_stock, vec![vec![ScalarValue::Signed(1)]]);

    let mut writer = database.begin_optimistic(2, DurabilityClass::Strict)?;
    writer.execute_sql("UPDATE stock SET s_quantity = 5 WHERE s_item_id = 2", &[])?;
    database.commit_optimistic(writer)?;
    let SqlResult::Rows { rows: retained, .. } = reader.execute_sql(
        "SELECT s_item_id FROM stock WHERE s_quantity < 10 ORDER BY s_item_id LIMIT 100",
        &[],
    )?
    else {
        return Err("expected retained stock rows".into());
    };
    assert_eq!(retained, low_stock);
    reader.rollback();
    let mut current = database.begin_optimistic(3, DurabilityClass::Memory)?;
    let SqlResult::Rows {
        rows: current_stock,
        ..
    } = current.execute_sql(
        "SELECT s_item_id FROM stock WHERE s_quantity < 10 ORDER BY s_item_id LIMIT 100",
        &[],
    )?
    else {
        return Err("expected current stock rows".into());
    };
    assert_eq!(
        current_stock,
        vec![vec![ScalarValue::Signed(1)], vec![ScalarValue::Signed(2)]]
    );
    current.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn delivery_like_transaction_updates_order_and_customer_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-tpcc-delivery-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    for statement in [
        "CREATE TABLE orders (o_id BIGINT PRIMARY KEY, o_carrier_id BIGINT, o_customer_id BIGINT NOT NULL)",
        "CREATE TABLE customer (c_id BIGINT PRIMARY KEY, c_balance BIGINT NOT NULL, c_delivery_cnt BIGINT NOT NULL)",
        "INSERT INTO orders (o_id, o_carrier_id, o_customer_id) VALUES (7, NULL, 100)",
        "INSERT INTO customer (c_id, c_balance, c_delivery_cnt) VALUES (100, 500, 0)",
    ] {
        seed.execute_sql(statement, &[])?;
    }
    seed.commit()?;
    let mut delivery = database.begin_optimistic(2, DurabilityClass::Strict)?;
    delivery.execute_sql("UPDATE orders SET o_carrier_id = 5 WHERE o_id = 7", &[])?;
    delivery.execute_sql(
        "UPDATE customer SET c_balance = 542, c_delivery_cnt = 1 WHERE c_id = 100",
        &[],
    )?;
    database.commit_optimistic(delivery)?;
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    let mut observed = reopened.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT o_carrier_id FROM orders WHERE o_id = 7", &[])?)?,
        5
    );
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT c_balance FROM customer WHERE c_id = 100", &[])?)?,
        542
    );
    assert_eq!(
        signed_cell(
            observed.execute_sql("SELECT c_delivery_cnt FROM customer WHERE c_id = 100", &[])?
        )?,
        1
    );
    observed.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn payment_like_transaction_updates_warehouse_district_and_customer_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-tpcc-payment-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    for statement in [
        "CREATE TABLE warehouse (w_id BIGINT PRIMARY KEY, w_ytd BIGINT NOT NULL)",
        "CREATE TABLE district (d_id BIGINT PRIMARY KEY, d_ytd BIGINT NOT NULL)",
        "CREATE TABLE customer (c_id BIGINT PRIMARY KEY, c_balance BIGINT NOT NULL, c_ytd_payment BIGINT NOT NULL, c_payment_cnt BIGINT NOT NULL)",
        "INSERT INTO warehouse (w_id, w_ytd) VALUES (1, 1000)",
        "INSERT INTO district (d_id, d_ytd) VALUES (10, 100)",
        "INSERT INTO customer (c_id, c_balance, c_ytd_payment, c_payment_cnt) VALUES (100, 500, 0, 0)",
    ] {
        seed.execute_sql(statement, &[])?;
    }
    seed.commit()?;

    let mut payment = database.begin_optimistic(2, DurabilityClass::Strict)?;
    payment.execute_sql("UPDATE warehouse SET w_ytd = 1042 WHERE w_id = 1", &[])?;
    payment.execute_sql("UPDATE district SET d_ytd = 142 WHERE d_id = 10", &[])?;
    payment.execute_sql(
        "UPDATE customer SET c_balance = 458, c_ytd_payment = 42, c_payment_cnt = 1 WHERE c_id = 100",
        &[],
    )?;
    database.commit_optimistic(payment)?;
    drop(database);

    let reopened = NativeDatabase::open(&temporary)?;
    let mut observed = reopened.begin_optimistic(3, DurabilityClass::Memory)?;
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT w_ytd FROM warehouse WHERE w_id = 1", &[])?)?,
        1042
    );
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT d_ytd FROM district WHERE d_id = 10", &[])?)?,
        142
    );
    assert_eq!(
        signed_cell(observed.execute_sql("SELECT c_balance FROM customer WHERE c_id = 100", &[])?)?,
        458
    );
    assert_eq!(
        signed_cell(
            observed.execute_sql("SELECT c_ytd_payment FROM customer WHERE c_id = 100", &[])?
        )?,
        42
    );
    assert_eq!(
        signed_cell(
            observed.execute_sql("SELECT c_payment_cnt FROM customer WHERE c_id = 100", &[])?
        )?,
        1
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
