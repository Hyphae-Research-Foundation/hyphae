// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic metamorphic equivalence corpus for the native SQL engine.

use std::path::Path;

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::DurabilityClass;
use serde::Deserialize;

#[derive(Deserialize)]
struct Corpus {
    schema: String,
    seed_statements: Vec<String>,
    equivalences: Vec<Equivalence>,
}

#[derive(Deserialize)]
struct Equivalence {
    id: String,
    left: String,
    right: String,
}

#[test]
fn deterministic_metamorphic_sql_pairs_are_equivalent() -> Result<(), Box<dyn std::error::Error>> {
    let corpus_path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/g2-metamorphic.json"
    ));
    let corpus: Corpus = serde_json::from_str(&std::fs::read_to_string(corpus_path)?)?;
    assert_eq!(corpus.schema, "hyphae-native-g2-metamorphic-v1");
    assert!(corpus.equivalences.len() >= 6);

    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-metamorphic-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    for statement in &corpus.seed_statements {
        transaction.execute_sql(statement, &[])?;
    }
    for equivalence in &corpus.equivalences {
        let left = transaction.execute_sql(&equivalence.left, &[])?;
        let right = transaction.execute_sql(&equivalence.right, &[])?;
        let (
            SqlResult::Rows {
                columns: left_columns,
                rows: left_rows,
            },
            SqlResult::Rows {
                columns: right_columns,
                rows: right_rows,
            },
        ) = (left, right)
        else {
            return Err(format!("{} did not produce row results", equivalence.id).into());
        };
        assert_eq!(left_columns, right_columns, "{} schema", equivalence.id);
        assert_eq!(left_rows, right_rows, "{} rows", equivalence.id);
    }
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
