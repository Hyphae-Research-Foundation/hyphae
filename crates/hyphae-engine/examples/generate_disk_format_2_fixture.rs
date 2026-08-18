// SPDX-License-Identifier: Apache-2.0

//! Generates the immutable disk-format-2 compatibility fixture.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_engine::HyphaeEngine;
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{LexicalField, LexicalIndexDefinition};
use hyphae_storage::{AppendOutcome, CommitReceipt, CompactionOutcome};
use serde_json::{Value as JsonValue, json};
use uuid::Uuid;

const RECORD_TRANSACTION: &str = "018f0000-0000-7000-8000-00000000f201";
const SPACE_TRANSACTION: &str = "018f0000-0000-7000-8000-00000000f202";
const VECTOR_TRANSACTION: &str = "018f0000-0000-7000-8000-00000000f203";
const LEXICAL_TRANSACTION: &str = "018f0000-0000-7000-8000-00000000f204";

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args_os().nth(1).map_or_else(
        || std::env::temp_dir().join("hyphae-format-2-fixture"),
        PathBuf::from,
    );
    if root.exists() {
        return Err(format!(
            "fixture staging directory already exists: {}",
            root.display()
        )
        .into());
    }
    fs::create_dir_all(&root)?;
    let result = generate(&root);
    let _ignored = fs::remove_dir_all(&root);
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

fn generate(root: &Path) -> Result<JsonValue, Box<dyn Error>> {
    let data = root.join("data");
    let mut opened = HyphaeEngine::open(&data)?;
    let record_receipt = committed(opened.engine.put_records(
        Uuid::parse_str(RECORD_TRANSACTION)?,
        &[
            record(b"alpha", "Durable memory", "offline agent memory"),
            record(b"beta", "Fast search", "exact vector retrieval"),
        ],
    )?)?;
    let space = VectorSpaceName::new("semantic")?;
    let space_receipt = committed(opened.engine.define_vector_space(
        Uuid::parse_str(SPACE_TRANSACTION)?,
        VectorSpaceDefinition::cosine(space.clone(), 2)?,
    )?)?;
    let vector_receipt = committed(opened.engine.put_vectors(
        Uuid::parse_str(VECTOR_TRANSACTION)?,
        &space,
        &[
            (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0])?),
            (b"beta".to_vec(), Q15Vector::new(vec![0, 32_767])?),
        ],
    )?)?;
    let lexical_receipt = committed(opened.engine.define_lexical_index(
        Uuid::parse_str(LEXICAL_TRANSACTION)?,
        LexicalIndexDefinition::new(
            VectorSpaceName::new("content")?,
            vec![
                LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                },
                LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 2_000_000,
                },
            ],
        )?,
    )?)?;
    let compacted = opened.engine.compact()?;
    let CompactionOutcome::Compacted(report) = compacted else {
        return Err("format-2 fixture did not compact a committed log".into());
    };
    let snapshot = report.snapshot;
    let manifest = latest_file(&data.join("manifest"), "hymanifest")?;
    let active_log = data
        .join("log")
        .join(format!("{:020}.hylog", report.generation));
    let selected = [
        data.join("FORMAT"),
        manifest,
        active_log,
        snapshot.path.clone(),
    ];
    let mut files = BTreeMap::new();
    for path in selected {
        files.insert(
            path.strip_prefix(&data)?
                .to_string_lossy()
                .replace('\\', "/"),
            encode_hex(&fs::read(path)?),
        );
    }

    Ok(json!({
        "disk_format_version": 2,
        "expected": {
            "checkpoint_sequence": snapshot.checkpoint_sequence,
            "snapshot_digest": encode_hex(&snapshot.snapshot_digest),
            "entry_count": snapshot.entry_count,
            "vector_space_count": snapshot.vector_space_count,
            "vector_count": snapshot.vector_count,
            "lexical_index_count": snapshot.lexical_index_count,
            "receipt_count": snapshot.receipt_count,
            "record": {
                "key_hex": "616c706861",
                "value": {
                    "body": "offline agent memory",
                    "title": "Durable memory"
                }
            },
            "vector_space": {
                "name": "semantic",
                "dimension": 2
            },
            "exact_order": ["616c706861", "62657461"],
            "lexical_index": "content",
            "lexical_first_key": "616c706861",
            "transactions": [
                receipt_json(RECORD_TRANSACTION, &record_receipt),
                receipt_json(SPACE_TRANSACTION, &space_receipt),
                receipt_json(VECTOR_TRANSACTION, &vector_receipt),
                receipt_json(LEXICAL_TRANSACTION, &lexical_receipt)
            ]
        },
        "files_hex": files,
        "fixture_version": 1,
        "purpose": "Open, rebuild, retrieve, and preserve receipts from a compacted disk-format-2 directory without its materialized index."
    }))
}

fn record(key: &[u8], title: &str, body: &str) -> Record {
    Record::new(
        key,
        Value::Object(BTreeMap::from([
            ("body".to_owned(), Value::String(body.to_owned())),
            ("title".to_owned(), Value::String(title.to_owned())),
        ])),
    )
}

fn committed(outcome: AppendOutcome) -> Result<CommitReceipt, Box<dyn Error>> {
    match outcome {
        AppendOutcome::Committed(receipt) => Ok(receipt),
        AppendOutcome::Existing(_) => Err("fixture transaction unexpectedly existed".into()),
    }
}

fn receipt_json(transaction_id: &str, receipt: &CommitReceipt) -> JsonValue {
    json!({
        "transaction_id": transaction_id,
        "commit_sequence": receipt.commit_sequence,
        "commit_digest": encode_hex(&receipt.commit_digest),
        "transaction_digest": encode_hex(&receipt.transaction_digest)
    })
}

fn latest_file(directory: &Path, extension: &str) -> Result<PathBuf, Box<dyn Error>> {
    fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|value| value == extension))
        .max()
        .ok_or_else(|| format!("no {extension} file in {}", directory.display()).into())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs};

    use uuid::Uuid;

    use super::generate;

    #[test]
    fn checked_in_format_2_fixture_is_current() -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-format-2-fixture-test-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
        fs::create_dir_all(&root)?;
        let generated = generate(&root);
        let _ignored = fs::remove_dir_all(&root);
        let generated = generated?;
        let checked_in: serde_json::Value = serde_json::from_str(include_str!(
            "../../../compatibility/v2/data-directory.json"
        ))?;
        assert_eq!(generated, checked_in);
        Ok(())
    }
}
