// SPDX-License-Identifier: Apache-2.0

//! Black-box conformance for the autonomous single-binary experience.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_engine::{
    HyphaeEngine, write_exact_retrieval_proof, write_hybrid_retrieval_proof,
    write_lexical_retrieval_proof,
};
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{
    ExactRetrievalLimits, ExactRetrievalRequest, HybridRequest, LexicalField,
    LexicalIndexDefinition, LexicalLimits, LexicalRequest,
};
use uuid::Uuid;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let path =
            std::env::temp_dir().join(format!("hyphae-cli-single-binary-{}", Uuid::now_v7()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

fn run(data_directory: &Path, arguments: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(arguments)
        .arg("--data-dir")
        .arg(data_directory)
        .output()?;
    decode_output(&output, arguments)
}

fn run_without_data(arguments: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(arguments)
        .output()?;
    decode_output(&output, arguments)
}

fn decode_output(
    output: &std::process::Output,
    arguments: &[&str],
) -> Result<serde_json::Value, Box<dyn Error>> {
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "hyphae {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn one_binary_persists_queries_and_compacts_without_external_services() -> Result<(), Box<dyn Error>>
{
    let data_directory = TestDirectory::create()?;
    let transaction_id = "019f0000-0000-7000-8000-000000000001";

    let committed = run(
        &data_directory.path,
        &[
            "put",
            "--key",
            "alpha",
            "--json",
            r#"{"score":10,"group":"x"}"#,
            "--transaction-id",
            transaction_id,
        ],
    )?;
    assert_eq!(committed["status"], "committed");

    let repeated = run(
        &data_directory.path,
        &[
            "put",
            "--key",
            "alpha",
            "--json",
            r#"{"group":"x","score":10}"#,
            "--transaction-id",
            transaction_id,
        ],
    )?;
    assert_eq!(repeated["status"], "existing");
    assert_eq!(repeated["commit_digest"], committed["commit_digest"]);

    run(
        &data_directory.path,
        &[
            "put",
            "--key",
            "beta",
            "--json",
            r#"{"score":20,"group":"x"}"#,
        ],
    )?;

    let filtered = run(
        &data_directory.path,
        &[
            "query", "--field", "group", "--equals", r#""x""#, "--sort", "score",
        ],
    )?;
    assert_eq!(filtered["scanned_records"], 2);
    assert_eq!(filtered["matched_records"], 2);

    let before = run(
        &data_directory.path,
        &["query", "--sort", "score", "--descending", "--limit", "2"],
    )?;
    assert_eq!(before["rows"][0]["key_hex"], "62657461");
    assert_eq!(before["rows"][1]["key_hex"], "616c706861");

    let proof_path = data_directory.path.join("query.hyproof");
    let proof_path_string = proof_path.to_string_lossy().into_owned();
    let proven = run(
        &data_directory.path,
        &[
            "query",
            "--sort",
            "score",
            "--descending",
            "--limit",
            "2",
            "--proof-out",
            &proof_path_string,
        ],
    )?;
    assert_eq!(proven["rows"], before["rows"]);
    let snapshot_path = proven["proof"]["snapshot_path"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("missing snapshot path"))?;
    let anchor = proven["proof"]["anchor_digest"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("missing anchor digest"))?;
    let verified = run_without_data(&[
        "verify",
        "--proof",
        &proof_path_string,
        "--snapshot",
        snapshot_path,
        "--anchor",
        anchor,
    ])?;
    assert_eq!(verified["status"], "verified");

    let snapshot = run(&data_directory.path, &["snapshot"])?;
    assert_eq!(snapshot["entry_count"], 2);
    let compacted = run(&data_directory.path, &["compact"])?;
    assert_eq!(compacted["status"], "compacted");

    let after = run(
        &data_directory.path,
        &["query", "--sort", "score", "--descending", "--limit", "2"],
    )?;
    assert_eq!(after["rows"], before["rows"]);
    assert_eq!(after["matched_records"], before["matched_records"]);
    assert_eq!(after["scanned_records"], before["scanned_records"]);
    Ok(())
}

#[test]
fn one_binary_backup_restore_and_doctor_are_offline() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create()?;
    let source = temporary.path.join("source");
    let backup = temporary.path.join("backup");
    let restored = temporary.path.join("restored");
    let transaction_id = "019f0000-0000-7000-8000-000000000010";
    let backup_string = backup.to_string_lossy().into_owned();

    let committed = run(
        &source,
        &[
            "put",
            "--key",
            "alpha",
            "--json",
            r#"{"durable":true}"#,
            "--transaction-id",
            transaction_id,
        ],
    )?;
    assert_eq!(committed["status"], "committed");
    assert_eq!(
        run(&source, &["backup", "--out", &backup_string])?["status"],
        "created"
    );
    assert_eq!(
        run_without_data(&["backup-verify", "--backup", &backup_string])?["status"],
        "verified"
    );
    assert_eq!(
        run(&restored, &["restore", "--backup", &backup_string])?["status"],
        "restored"
    );

    let read = run(&restored, &["get", "--key", "alpha"])?;
    assert_eq!(read["found"], true);
    let retry = run(
        &restored,
        &[
            "put",
            "--key",
            "alpha",
            "--json",
            r#"{"durable":true}"#,
            "--transaction-id",
            transaction_id,
        ],
    )?;
    assert_eq!(retry["status"], "existing");
    assert_eq!(retry["commit_digest"], committed["commit_digest"]);
    assert_eq!(run(&restored, &["doctor"])?["status"], "healthy");
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn one_binary_verifies_all_retrieval_proofs_offline() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create()?;
    let data = temporary.path.join("data");
    let mut opened = HyphaeEngine::open(&data)?;
    opened.engine.put_records(
        Uuid::now_v7(),
        &[
            retrieval_record(b"alpha", "Durable memory", "offline agent memory"),
            retrieval_record(b"beta", "Fast search", "exact vector retrieval"),
        ],
    )?;
    let vector_space = VectorSpaceName::new("semantic")?;
    opened.engine.define_vector_space(
        Uuid::now_v7(),
        VectorSpaceDefinition::cosine(vector_space.clone(), 2)?,
    )?;
    opened.engine.put_vectors(
        Uuid::now_v7(),
        &vector_space,
        &[
            (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0])?),
            (b"beta".to_vec(), Q15Vector::new(vec![0, 32_767])?),
        ],
    )?;
    let lexical_index = VectorSpaceName::new("content")?;
    opened.engine.define_lexical_index(
        Uuid::now_v7(),
        LexicalIndexDefinition::new(
            lexical_index.clone(),
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
    )?;

    let vector_request = ExactRetrievalRequest {
        vector_space,
        query: Q15Vector::new(vec![32_767, 0])?,
        limit: 2,
        minimum_score_nanos: -1_000_000_000,
        minimum_margin_nanos: 0,
    };
    let lexical_request = LexicalRequest {
        index: lexical_index,
        query: "durable memory".to_owned(),
        limit: 2,
    };
    let exact_limits = ExactRetrievalLimits {
        max_candidates: 10,
        max_candidate_bytes: 1_024,
        max_returned: 10,
        timeout: Duration::from_secs(1),
    };
    let lexical_limits = LexicalLimits {
        max_documents: 10,
        max_tokens: 1_000,
        max_candidates: 10,
        max_returned: 10,
        timeout: Duration::from_secs(1),
    };

    let exact = opened
        .engine
        .retrieve_exact_with_proof(&vector_request, &exact_limits)?;
    let lexical = opened
        .engine
        .retrieve_lexical_with_proof(&lexical_request, &lexical_limits)?;
    let hybrid = opened.engine.retrieve_hybrid_with_proof(
        &lexical_request,
        &lexical_limits,
        &vector_request,
        &exact_limits,
        &HybridRequest {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 2,
        },
    )?;

    let exact_path = temporary.path.join("exact.hyrproof");
    let lexical_path = temporary.path.join("lexical.hyrproof");
    let hybrid_path = temporary.path.join("hybrid.hyrproof");
    write_exact_retrieval_proof(&exact_path, &exact.proof)?;
    write_lexical_retrieval_proof(&lexical_path, &lexical.proof)?;
    write_hybrid_retrieval_proof(&hybrid_path, &hybrid.proof)?;

    for (kind, proof_path, artifact_anchor, snapshot_path) in [
        (
            "exact",
            exact_path,
            exact.proof.anchor_digest(),
            exact.snapshot.path,
        ),
        (
            "lexical",
            lexical_path,
            lexical.proof.anchor_digest(),
            lexical.snapshot.path,
        ),
        (
            "hybrid",
            hybrid_path,
            hybrid.proof.anchor_digest(),
            hybrid.snapshot.path,
        ),
    ] {
        let proof = proof_path.to_string_lossy().into_owned();
        let snapshot = snapshot_path.to_string_lossy().into_owned();
        let anchor = encode_hex(&artifact_anchor);
        let verified = run_without_data(&[
            "verify-retrieval",
            "--kind",
            kind,
            "--proof",
            &proof,
            "--snapshot",
            &snapshot,
            "--anchor",
            &anchor,
        ])?;
        assert_eq!(verified["status"], "verified");
        assert_eq!(verified["operation"], kind);
    }
    Ok(())
}

fn retrieval_record(key: &[u8], title: &str, body: &str) -> Record {
    Record::new(
        key,
        Value::Object(BTreeMap::from([
            ("body".to_owned(), Value::String(body.to_owned())),
            ("title".to_owned(), Value::String(title.to_owned())),
        ])),
    )
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
