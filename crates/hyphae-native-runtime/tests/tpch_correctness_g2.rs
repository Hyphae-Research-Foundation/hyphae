// SPDX-License-Identifier: Apache-2.0

//! Bounded TPC-H correctness vertical using native SQL types and joins.

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

#[test]
fn tpch_q3_shape_returns_reference_order_customer_rows() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-native-tpch-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql(
        "CREATE TABLE customer (c_custkey BIGINT PRIMARY KEY, c_mktsegment TEXT NOT NULL)",
        &[],
    )?;
    transaction.execute_sql(
        "CREATE TABLE orders (o_orderkey BIGINT PRIMARY KEY, o_custkey BIGINT NOT NULL, o_orderstatus TEXT NOT NULL)",
        &[],
    )?;
    transaction.execute_sql(
        "CREATE INDEX customers_segment ON customer (c_mktsegment)",
        &[],
    )?;
    transaction.execute_sql(
        "CREATE UNIQUE INDEX orders_customer ON orders (o_custkey)",
        &[],
    )?;
    for statement in [
        "INSERT INTO customer (c_custkey, c_mktsegment) VALUES (1, 'BUILDING')",
        "INSERT INTO customer (c_custkey, c_mktsegment) VALUES (2, 'AUTOMOBILE')",
        "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus) VALUES (10, 1, 'O')",
        "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus) VALUES (20, 2, 'F')",
    ] {
        transaction.execute_sql(statement, &[])?;
    }
    assert_eq!(
        transaction.execute_sql(
            "SELECT customer.c_custkey, orders.o_orderkey FROM customer INNER JOIN orders ON customer.c_custkey = orders.o_custkey WHERE c_mktsegment = 'BUILDING' ORDER BY c_custkey LIMIT 10",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["customer.c_custkey".to_owned(), "orders.o_orderkey".to_owned()],
            rows: vec![vec![ScalarValue::Signed(1), ScalarValue::Signed(10)]],
        }
    );
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
