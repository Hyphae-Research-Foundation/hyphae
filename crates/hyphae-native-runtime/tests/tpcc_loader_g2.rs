// SPDX-License-Identifier: GPL-3.0-only

//! Canonical bounded TPC-C schema and deterministic loader.

use std::collections::BTreeMap;

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::DurabilityClass;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    schema: String,
    seed: u64,
    warehouses: u64,
    districts_per_warehouse: u64,
    customers_per_district: u64,
    orders_per_district: u64,
    items: u64,
    order_lines_per_order: u64,
    expected: BTreeMap<String, u64>,
}

fn row_count(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    table: &str,
    limit: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let SqlResult::Rows { rows, .. } = transaction.execute_sql(
        &format!("SELECT * FROM {table} ORDER BY id LIMIT {limit}"),
        &[],
    )?
    else {
        return Err("expected rows".into());
    };
    Ok(u64::try_from(rows.len())?)
}

#[test]
fn deterministic_tpcc_fixture_loads_all_nine_tables_and_reopens()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture: Fixture = serde_json::from_str(include_str!("corpus/g2-tpcc-fixture.json"))?;
    assert_eq!(fixture.schema, "hyphae-native-g2-tpcc-fixture-v1");
    assert_eq!(fixture.seed, 20_260_804);
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-tpcc-loader-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut load = database.begin_sql(1, DurabilityClass::Strict)?;
    for table in [
        "warehouse",
        "district",
        "customer",
        "orders",
        "new_order",
        "order_line",
        "item",
        "stock",
        "history",
    ] {
        load.execute_sql(
            &format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, parent_id BIGINT NOT NULL, amount BIGINT NOT NULL)"),
            &[],
        )?;
    }
    for warehouse in 1..=fixture.warehouses {
        load.execute_sql(
            &format!("INSERT INTO warehouse (id, parent_id, amount) VALUES ({warehouse}, 0, 1000)"),
            &[],
        )?;
    }
    let mut district_id = 0_u64;
    let mut customer_id = 0_u64;
    let mut order_id = 0_u64;
    let mut order_line_id = 0_u64;
    for warehouse in 1..=fixture.warehouses {
        for _ in 0..fixture.districts_per_warehouse {
            district_id += 1;
            load.execute_sql(&format!("INSERT INTO district (id, parent_id, amount) VALUES ({district_id}, {warehouse}, 100)"), &[])?;
            for _ in 0..fixture.customers_per_district {
                customer_id += 1;
                load.execute_sql(&format!("INSERT INTO customer (id, parent_id, amount) VALUES ({customer_id}, {district_id}, 500)"), &[])?;
                load.execute_sql(&format!("INSERT INTO history (id, parent_id, amount) VALUES ({customer_id}, {district_id}, 0)"), &[])?;
            }
            for local_order in 1..=fixture.orders_per_district {
                order_id += 1;
                load.execute_sql(&format!("INSERT INTO orders (id, parent_id, amount) VALUES ({order_id}, {district_id}, 42)"), &[])?;
                if local_order == fixture.orders_per_district {
                    load.execute_sql(&format!("INSERT INTO new_order (id, parent_id, amount) VALUES ({order_id}, {district_id}, 0)"), &[])?;
                }
                for _ in 0..fixture.order_lines_per_order {
                    order_line_id += 1;
                    load.execute_sql(&format!("INSERT INTO order_line (id, parent_id, amount) VALUES ({order_line_id}, {order_id}, 21)"), &[])?;
                }
            }
        }
    }
    for item in 1..=fixture.items {
        load.execute_sql(
            &format!("INSERT INTO item (id, parent_id, amount) VALUES ({item}, 0, 10)"),
            &[],
        )?;
        load.execute_sql(
            &format!("INSERT INTO stock (id, parent_id, amount) VALUES ({item}, 1, 100)"),
            &[],
        )?;
    }
    load.commit()?;
    drop(database);

    let reopened = NativeDatabase::open(&temporary)?;
    let mut verify = reopened.begin_optimistic(2, DurabilityClass::Memory)?;
    for (table, expected_key) in [
        ("warehouse", "warehouse_rows"),
        ("district", "district_rows"),
        ("customer", "customer_rows"),
        ("orders", "order_rows"),
        ("new_order", "new_order_rows"),
        ("order_line", "order_line_rows"),
        ("item", "item_rows"),
        ("stock", "stock_rows"),
        ("history", "history_rows"),
    ] {
        let expected = fixture.expected[expected_key];
        assert_eq!(
            row_count(&mut verify, table, expected + 1)?,
            expected,
            "{table}"
        );
    }
    verify.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
