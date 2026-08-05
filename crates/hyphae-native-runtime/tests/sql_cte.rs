// SPDX-License-Identifier: Apache-2.0

//! RED contract for the first non-recursive CTE vertical.

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

#[test]
fn recursive_nested_parameterized_and_mismatched_ctes_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-cte-negative-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql("CREATE TABLE accounts (id BIGINT PRIMARY KEY)", &[])?;
    for statement in [
        "WITH RECURSIVE ids AS (SELECT id FROM accounts LIMIT 1) SELECT id FROM ids LIMIT 1",
        "WITH ids AS (WITH nested AS (SELECT id FROM accounts LIMIT 1) SELECT id FROM nested LIMIT 1) SELECT id FROM ids LIMIT 1",
        "WITH ids AS (SELECT id FROM accounts LIMIT 1) SELECT id FROM other LIMIT 1",
    ] {
        assert!(
            transaction.execute_sql(statement, &[]).is_err(),
            "accepted {statement}"
        );
    }
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn non_recursive_cte_materializes_one_bound_subquery() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-native-cte-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, active BOOLEAN NOT NULL)",
        &[],
    )?;
    transaction.execute_sql("INSERT INTO accounts (id, active) VALUES (1, TRUE)", &[])?;
    transaction.execute_sql("INSERT INTO accounts (id, active) VALUES (2, FALSE)", &[])?;
    assert_eq!(
        transaction.execute_sql(
            "WITH active_accounts AS (SELECT id, active FROM accounts WHERE active = TRUE LIMIT 10) SELECT id FROM active_accounts LIMIT 10",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![ScalarValue::Signed(1)]],
        }
    );
    assert_eq!(
        transaction.execute_sql(
            "WITH selected AS (SELECT id FROM accounts WHERE id >= ? ORDER BY id LIMIT 10) SELECT id FROM selected LIMIT 10",
            &[ScalarValue::Signed(2)],
        )?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![ScalarValue::Signed(2)]],
        }
    );
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
