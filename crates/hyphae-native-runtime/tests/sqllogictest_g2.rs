// SPDX-License-Identifier: Apache-2.0

//! Bounded SQLLogicTest-compatible runner for the native relational engine.

use std::path::Path;

use hyphae_native_runtime::{NativeDatabase, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

fn scalar_text(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".to_owned(),
        SqlValue::Boolean(value) => value.to_string(),
        SqlValue::Signed(value) => value.to_string(),
        SqlValue::Unsigned(value) => value.to_string(),
        SqlValue::Text(value) => value.clone(),
        SqlValue::Decimal(value) => value.to_string(),
        SqlValue::Float64(value) => format!("{:.3}", value.get()),
        other => format!("{other:?}"),
    }
}

fn flush_case(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    header: &str,
    sql: &[String],
    expected: &[String],
) {
    if header.is_empty() {
        return;
    }
    let statement = sql.join(" ");
    let result = transaction
        .execute_sql(&statement, &[])
        .unwrap_or_else(|error| unreachable!("{statement}: {error}"));
    if header.starts_with("statement ok") {
        assert!(matches!(result, SqlResult::Command { .. }), "{statement}");
        return;
    }
    assert!(
        header.starts_with("query "),
        "unsupported SQLLogicTest header: {header}"
    );
    let SqlResult::Rows { rows, .. } = result else {
        unreachable!("query returned command: {statement}");
    };
    let mut actual = rows
        .into_iter()
        .map(|row| row.iter().map(scalar_text).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>();
    if header.split_whitespace().any(|word| word == "rowsort") {
        actual.sort();
    }
    assert_eq!(actual, expected, "{statement}");
}

fn run_corpus(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let temporary = std::env::temp_dir().join(format!("hyphae-native-slt-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    let mut header = String::new();
    let mut sql = Vec::new();
    let mut expected = Vec::new();
    let mut reading_expected = false;
    for line in content.lines().chain(std::iter::once("")) {
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            flush_case(&mut transaction, &header, &sql, &expected);
            header.clear();
            sql.clear();
            expected.clear();
            reading_expected = false;
        } else if header.is_empty() {
            line.clone_into(&mut header);
        } else if line == "----" {
            reading_expected = true;
        } else if reading_expected {
            expected.push(line.to_owned());
        } else {
            sql.push(line.to_owned());
        }
    }
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn bounded_g2_sqllogictest_corpus_passes() -> Result<(), Box<dyn std::error::Error>> {
    run_corpus(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/g2-smoke.slt"
    )))
}
