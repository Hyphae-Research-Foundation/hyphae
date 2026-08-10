// SPDX-License-Identifier: GPL-3.0-only

//! Expanded bounded TPC-H correctness corpus for admitted native SQL shapes.

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

fn rows(result: SqlResult) -> Result<Vec<Vec<ScalarValue>>, Box<dyn std::error::Error>> {
    let SqlResult::Rows { rows, .. } = result else {
        return Err("expected rows".into());
    };
    Ok(rows)
}

#[test]
fn admitted_tpch_query_shapes_match_reference_results() -> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-tpch-corpus-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Memory)?;
    for statement in [
        "CREATE TABLE customer (c_custkey BIGINT PRIMARY KEY, c_segment TEXT NOT NULL)",
        "CREATE TABLE orders (o_orderkey BIGINT PRIMARY KEY, o_custkey BIGINT NOT NULL, o_status TEXT NOT NULL)",
        "CREATE TABLE lineitem (l_orderkey BIGINT NOT NULL, l_linenumber BIGINT NOT NULL, l_quantity BIGINT NOT NULL, l_shipmode TEXT NOT NULL, PRIMARY KEY (l_orderkey, l_linenumber))",
        "CREATE TABLE supplier (s_suppkey BIGINT PRIMARY KEY, s_name TEXT NOT NULL)",
        "CREATE INDEX customer_segment ON customer (c_segment)",
        "CREATE UNIQUE INDEX orders_customer ON orders (o_custkey)",
        "INSERT INTO customer (c_custkey, c_segment) VALUES (1, 'BUILDING')",
        "INSERT INTO customer (c_custkey, c_segment) VALUES (2, 'AUTOMOBILE')",
        "INSERT INTO orders (o_orderkey, o_custkey, o_status) VALUES (10, 1, 'O')",
        "INSERT INTO orders (o_orderkey, o_custkey, o_status) VALUES (20, 2, 'F')",
        "INSERT INTO lineitem (l_orderkey, l_linenumber, l_quantity, l_shipmode) VALUES (10, 1, 5, 'AIR')",
        "INSERT INTO lineitem (l_orderkey, l_linenumber, l_quantity, l_shipmode) VALUES (10, 2, 9, 'SHIP')",
        "INSERT INTO lineitem (l_orderkey, l_linenumber, l_quantity, l_shipmode) VALUES (20, 1, 3, 'AIR')",
        "INSERT INTO supplier (s_suppkey, s_name) VALUES (100, 'Supplier#100')",
    ] {
        tx.execute_sql(statement, &[])?;
    }

    assert_eq!(
        rows(tx.execute_sql(
            "SELECT c_custkey FROM customer WHERE c_segment = 'BUILDING' ORDER BY c_custkey LIMIT 10",
            &[],
        )?)?,
        vec![vec![ScalarValue::Signed(1)]]
    );
    assert_eq!(
        rows(tx.execute_sql(
            "SELECT l_orderkey, l_linenumber FROM lineitem WHERE l_orderkey = 10 ORDER BY l_orderkey, l_linenumber LIMIT 10",
            &[],
        )?)?,
        vec![
            vec![ScalarValue::Signed(10), ScalarValue::Signed(1)],
            vec![ScalarValue::Signed(10), ScalarValue::Signed(2)],
        ]
    );
    assert_eq!(
        rows(tx.execute_sql(
            "WITH shipped AS (SELECT l_orderkey, l_linenumber FROM lineitem WHERE l_shipmode = 'AIR' ORDER BY l_orderkey, l_linenumber LIMIT 10) SELECT l_orderkey FROM shipped LIMIT 10",
            &[],
        )?)?,
        vec![vec![ScalarValue::Signed(10)], vec![ScalarValue::Signed(20)]]
    );
    assert_eq!(
        rows(tx.execute_sql("SELECT s_name FROM supplier WHERE s_suppkey = 100", &[],)?)?,
        vec![vec![ScalarValue::Text("Supplier#100".to_owned())]]
    );
    tx.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
