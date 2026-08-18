// SPDX-License-Identifier: Apache-2.0

//! Executable compatibility evidence for every supported disk format.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_engine::HyphaeEngine;
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{
    ExactRetrievalLimits, ExactRetrievalOutcome, ExactRetrievalRequest, LexicalField,
    LexicalIndexDefinition, LexicalLimits, LexicalOutcome, LexicalRequest,
};
use hyphae_storage::{AppendOutcome, restore_backup};
use uuid::Uuid;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-disk-compatibility-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if !encoded.len().is_multiple_of(2) {
        return Err("fixture contains odd-length hexadecimal data".into());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(text, 16)?)
        })
        .collect()
}

fn fixture_path(root: &Path, relative: &str) -> Result<PathBuf, Box<dyn Error>> {
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("fixture contains a noncanonical relative path".into());
    }
    Ok(root.join(relative))
}

#[test]
fn disk_format_1_opens_rebuilds_and_preserves_idempotency() -> Result<(), Box<dyn Error>> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/data-directory.json"))?;
    assert_eq!(fixture["fixture_version"], 1);
    assert_eq!(fixture["disk_format_version"], 1);

    let temporary = TestDirectory::create()?;
    let data = temporary.path.join("data");
    let files = fixture["files_hex"]
        .as_object()
        .ok_or("fixture files_hex is not an object")?;
    for (relative, encoded) in files {
        let path = fixture_path(&data, relative)?;
        fs::create_dir_all(path.parent().ok_or("fixture file has no parent")?)?;
        fs::write(
            path,
            decode_hex(encoded.as_str().ok_or("fixture file is not hexadecimal")?)?,
        )?;
    }

    assert!(!data.join("indexes/primary.redb").exists());
    let mut opened = HyphaeEngine::open(&data)?;
    let expected_value = Value::Object(BTreeMap::from([
        ("group".to_owned(), Value::String("fixture".to_owned())),
        ("score".to_owned(), Value::Integer(42)),
    ]));
    assert_eq!(
        opened.engine.get_record(b"alpha")?,
        Some(Record::new(b"alpha", expected_value.clone()))
    );
    assert!(data.join("indexes/primary.redb").exists());

    let transaction_id = Uuid::parse_str(
        fixture["expected"]["transaction_id"]
            .as_str()
            .ok_or("fixture transaction ID is not a string")?,
    )?;
    let repeated = opened
        .engine
        .put_record(transaction_id, &Record::new(b"alpha", expected_value))?;
    let AppendOutcome::Existing(receipt) = repeated else {
        return Err("historical transaction was appended again".into());
    };
    assert_eq!(
        receipt.commit_sequence,
        fixture["expected"]["commit_sequence"]
            .as_u64()
            .ok_or("fixture commit sequence is not an integer")?
    );
    assert_eq!(
        hex(&receipt.commit_digest),
        fixture["expected"]["commit_digest"]
            .as_str()
            .ok_or("fixture commit digest is not a string")?
    );
    assert_eq!(
        hex(&receipt.transaction_digest),
        fixture["expected"]["transaction_digest"]
            .as_str()
            .ok_or("fixture transaction digest is not a string")?
    );
    Ok(())
}

#[test]
fn disk_format_2_rebuilds_retrieves_restores_and_preserves_receipts() -> Result<(), Box<dyn Error>>
{
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../compatibility/v2/data-directory.json"
    ))?;
    assert_eq!(fixture["fixture_version"], 1);
    assert_eq!(fixture["disk_format_version"], 2);

    let temporary = TestDirectory::create()?;
    let data = temporary.path.join("data-v2");
    materialize_fixture(&fixture, &data)?;
    assert!(!data.join("indexes/primary.redb").exists());

    let mut opened = HyphaeEngine::open(&data)?;
    let expected_value = Value::Object(BTreeMap::from([
        (
            "body".to_owned(),
            Value::String("offline agent memory".to_owned()),
        ),
        (
            "title".to_owned(),
            Value::String("Durable memory".to_owned()),
        ),
    ]));
    assert_eq!(
        opened.engine.get_record(b"alpha")?,
        Some(Record::new(b"alpha", expected_value.clone()))
    );
    assert!(data.join("indexes/primary.redb").exists());
    assert_retrieval_state(&opened.engine, &fixture)?;

    let record_transaction_id = transaction_id(&fixture, 0)?;
    let repeated = opened.engine.put_records(
        record_transaction_id,
        &[
            Record::new(b"alpha", expected_value),
            Record::new(
                b"beta",
                Value::Object(BTreeMap::from([
                    (
                        "body".to_owned(),
                        Value::String("exact vector retrieval".to_owned()),
                    ),
                    ("title".to_owned(), Value::String("Fast search".to_owned())),
                ])),
            ),
        ],
    )?;
    assert_receipt(&fixture, 0, repeated)?;

    let space = VectorSpaceName::new("semantic")?;
    assert_receipt(
        &fixture,
        1,
        opened.engine.define_vector_space(
            transaction_id(&fixture, 1)?,
            VectorSpaceDefinition::cosine(space.clone(), 2)?,
        )?,
    )?;
    assert_receipt(
        &fixture,
        2,
        opened.engine.put_vectors(
            transaction_id(&fixture, 2)?,
            &space,
            &[
                (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0])?),
                (b"beta".to_vec(), Q15Vector::new(vec![0, 32_767])?),
            ],
        )?,
    )?;
    assert_receipt(
        &fixture,
        3,
        opened
            .engine
            .define_lexical_index(transaction_id(&fixture, 3)?, lexical_definition()?)?,
    )?;

    let snapshot = opened.engine.snapshot()?;
    assert_eq!(
        snapshot.checkpoint_sequence,
        fixture["expected"]["checkpoint_sequence"]
            .as_u64()
            .ok_or("checkpoint sequence is not an integer")?
    );
    assert_eq!(snapshot.entry_count, 2);
    assert_eq!(snapshot.vector_space_count, 1);
    assert_eq!(snapshot.vector_count, 2);
    assert_eq!(snapshot.lexical_index_count, 1);
    assert_eq!(snapshot.receipt_count, 4);

    let backup = temporary.path.join("backup-v2");
    let restored = temporary.path.join("restored-v2");
    let backup_info = opened.engine.backup(&backup)?;
    assert_eq!(backup_info.snapshot.lexical_index_count, 1);
    drop(opened);

    fs::remove_file(data.join("indexes/primary.redb"))?;
    let rebuilt = HyphaeEngine::open(&data)?;
    assert_retrieval_state(&rebuilt.engine, &fixture)?;
    drop(rebuilt);

    let restore_info = restore_backup(&backup, &restored)?;
    assert_eq!(restore_info.snapshot.lexical_index_count, 1);
    let restored_engine = HyphaeEngine::open(&restored)?;
    assert_retrieval_state(&restored_engine.engine, &fixture)?;
    Ok(())
}

fn materialize_fixture(fixture: &serde_json::Value, data: &Path) -> Result<(), Box<dyn Error>> {
    let files = fixture["files_hex"]
        .as_object()
        .ok_or("fixture files_hex is not an object")?;
    for (relative, encoded) in files {
        let path = fixture_path(data, relative)?;
        fs::create_dir_all(path.parent().ok_or("fixture file has no parent")?)?;
        fs::write(
            path,
            decode_hex(encoded.as_str().ok_or("fixture file is not hexadecimal")?)?,
        )?;
    }
    Ok(())
}

fn transaction_id(fixture: &serde_json::Value, index: usize) -> Result<Uuid, Box<dyn Error>> {
    Ok(Uuid::parse_str(
        fixture["expected"]["transactions"][index]["transaction_id"]
            .as_str()
            .ok_or("fixture transaction ID is not a string")?,
    )?)
}

fn assert_receipt(
    fixture: &serde_json::Value,
    index: usize,
    outcome: AppendOutcome,
) -> Result<(), Box<dyn Error>> {
    let AppendOutcome::Existing(receipt) = outcome else {
        return Err("historical format-2 transaction was appended again".into());
    };
    let expected = &fixture["expected"]["transactions"][index];
    assert_eq!(
        receipt.commit_sequence,
        expected["commit_sequence"]
            .as_u64()
            .ok_or("receipt commit sequence is not an integer")?
    );
    assert_eq!(
        hex(&receipt.commit_digest),
        expected["commit_digest"]
            .as_str()
            .ok_or("receipt commit digest is not a string")?
    );
    assert_eq!(
        hex(&receipt.transaction_digest),
        expected["transaction_digest"]
            .as_str()
            .ok_or("receipt transaction digest is not a string")?
    );
    Ok(())
}

fn assert_retrieval_state(
    engine: &HyphaeEngine,
    fixture: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let exact = engine.retrieve_exact(
        &ExactRetrievalRequest {
            vector_space: VectorSpaceName::new("semantic")?,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 2,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        },
        &ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 1_024,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        },
    )?;
    let ExactRetrievalOutcome::Matches { matches, .. } = exact else {
        return Err("format-2 exact retrieval unexpectedly abstained".into());
    };
    let exact_order = matches
        .iter()
        .map(|entry| hex(&entry.key))
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(exact_order)?,
        fixture["expected"]["exact_order"]
    );

    let lexical = engine.retrieve_lexical(
        &LexicalRequest {
            index: VectorSpaceName::new("content")?,
            query: "durable memory".to_owned(),
            limit: 2,
        },
        &LexicalLimits {
            max_documents: 10,
            max_tokens: 1_000,
            max_candidates: 10,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        },
    )?;
    let LexicalOutcome::Matches { matches, .. } = lexical else {
        return Err("format-2 lexical retrieval unexpectedly abstained".into());
    };
    assert_eq!(
        matches.first().map(|entry| hex(&entry.key)),
        fixture["expected"]["lexical_first_key"]
            .as_str()
            .map(str::to_owned)
    );
    Ok(())
}

fn lexical_definition() -> Result<LexicalIndexDefinition, Box<dyn Error>> {
    Ok(LexicalIndexDefinition::new(
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
    )?)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}
