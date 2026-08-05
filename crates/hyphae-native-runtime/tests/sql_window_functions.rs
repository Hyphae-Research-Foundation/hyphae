// SPDX-License-Identifier: Apache-2.0

//! RED contract for bounded primary-key ordered window functions.

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::{DurabilityClass, ScalarValue};

#[test]
fn row_number_and_rank_follow_complete_primary_key_order() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-window-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
        &[],
    )?;
    for (id, name) in [(2, "beta"), (1, "alpha"), (3, "gamma")] {
        transaction.execute_sql(
            "INSERT INTO accounts (id, name) VALUES (?, ?)",
            &[ScalarValue::Signed(id), ScalarValue::Text(name.to_owned())],
        )?;
    }
    assert_eq!(
        transaction.execute_sql(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS row_number FROM accounts ORDER BY id LIMIT 3",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["id".to_owned(), "row_number".to_owned()],
            rows: vec![
                vec![ScalarValue::Signed(1), ScalarValue::Unsigned(1)],
                vec![ScalarValue::Signed(2), ScalarValue::Unsigned(2)],
                vec![ScalarValue::Signed(3), ScalarValue::Unsigned(3)],
            ],
        }
    );
    assert_eq!(
        transaction.execute_sql(
            "SELECT name, RANK() OVER (ORDER BY id) AS position FROM accounts ORDER BY id LIMIT 2",
            &[],
        )?,
        SqlResult::Rows {
            columns: vec!["name".to_owned(), "position".to_owned()],
            rows: vec![
                vec![
                    ScalarValue::Text("alpha".to_owned()),
                    ScalarValue::Unsigned(1)
                ],
                vec![
                    ScalarValue::Text("beta".to_owned()),
                    ScalarValue::Unsigned(2)
                ],
            ],
        }
    );
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn unsupported_window_shapes_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-window-negative-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, name TEXT)",
        &[],
    )?;
    for statement in [
        "SELECT id, ROW_NUMBER() OVER () AS n FROM accounts LIMIT 1",
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) AS n FROM accounts ORDER BY id LIMIT 1",
        "SELECT id, RANK() OVER (ORDER BY name) AS n FROM accounts ORDER BY id LIMIT 1",
        "SELECT id, DENSE_RANK() OVER (ORDER BY id) AS n FROM accounts ORDER BY id LIMIT 1",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id DESC) AS n FROM accounts ORDER BY id LIMIT 1",
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
