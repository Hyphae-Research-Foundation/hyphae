// SPDX-License-Identifier: Apache-2.0

//! End-to-end offline verification and tamper rejection.

use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use hyphae_engine::{
    HyphaeEngine, ProofError, ProvenResult, VerificationLimits, verify_result_proof,
    write_result_proof,
};
use hyphae_query::{
    AggregationPlan, ExecutionLimits, FieldPath, Filter, Metric, NamedMetric, NullPlacement, Query,
    Record, SortDirection, SortField, Value,
};
use hyphae_storage::SnapshotReadLimits;
use uuid::Uuid;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-proof-{name}-{}", Uuid::now_v7()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

fn value(score: i64, group: &str) -> Value {
    Value::Object(BTreeMap::from([
        ("group".to_owned(), Value::String(group.to_owned())),
        ("score".to_owned(), Value::Integer(score)),
    ]))
}

fn query() -> Query {
    Query {
        filter: Filter::MatchAll,
        sort: vec![SortField {
            path: FieldPath::field("score"),
            direction: SortDirection::Descending,
            nulls: NullPlacement::Last,
        }],
        cursor: None,
        limit: 2,
        aggregation: Some(AggregationPlan {
            group_by: Vec::new(),
            metrics: vec![NamedMetric {
                name: "count".to_owned(),
                metric: Metric::Count,
            }],
        }),
    }
}

fn seeded_engine(root: &Path) -> Result<hyphae_engine::OpenedEngine, Box<dyn Error>> {
    let mut opened = HyphaeEngine::open(root)?;
    opened.engine.put_records(
        Uuid::now_v7(),
        &[
            Record::new(b"alpha", value(10, "x")),
            Record::new(b"beta", value(20, "x")),
        ],
    )?;
    Ok(opened)
}

#[test]
fn query_and_get_proofs_verify_completely_offline() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create("offline")?;
    let opened = seeded_engine(&temporary.path().join("data"))?;
    let query_artifact = opened
        .engine
        .query_with_proof(&query(), &ExecutionLimits::default())?;
    let query_path = temporary.path().join("query.hyproof");
    write_result_proof(&query_path, &query_artifact.proof)?;
    let query_report = verify_result_proof(
        &query_path,
        &query_artifact.snapshot.path,
        query_artifact.proof.anchor_digest(),
        &VerificationLimits::default(),
    )?;
    assert_eq!(query_report.result, query_artifact.proof.result().clone());

    let proof_limited = VerificationLimits {
        proof_bytes: 1,
        ..VerificationLimits::default()
    };
    assert!(matches!(
        verify_result_proof(
            &query_path,
            &query_artifact.snapshot.path,
            query_artifact.proof.anchor_digest(),
            &proof_limited,
        ),
        Err(ProofError::ProofLimitExceeded { .. })
    ));
    let snapshot_limited = VerificationLimits {
        snapshot: SnapshotReadLimits {
            entries: 1,
            ..SnapshotReadLimits::default()
        },
        ..VerificationLimits::default()
    };
    assert!(matches!(
        verify_result_proof(
            &query_path,
            &query_artifact.snapshot.path,
            query_artifact.proof.anchor_digest(),
            &snapshot_limited,
        ),
        Err(ProofError::Snapshot { .. })
    ));
    let timed_out = VerificationLimits {
        timeout: Duration::ZERO,
        ..VerificationLimits::default()
    };
    assert!(matches!(
        verify_result_proof(
            &query_path,
            &query_artifact.snapshot.path,
            query_artifact.proof.anchor_digest(),
            &timed_out,
        ),
        Err(ProofError::TimedOut)
    ));

    for (name, key, expected_present) in [
        ("present", b"alpha".as_slice(), true),
        ("absent", b"missing".as_slice(), false),
    ] {
        let artifact = opened.engine.get_record_with_proof(key)?;
        let proof_path = temporary.path().join(format!("{name}.hyproof"));
        write_result_proof(&proof_path, &artifact.proof)?;
        let report = verify_result_proof(
            &proof_path,
            &artifact.snapshot.path,
            artifact.proof.anchor_digest(),
            &VerificationLimits::default(),
        )?;
        assert_eq!(
            matches!(report.result, ProvenResult::Get(Some(_))),
            expected_present
        );
    }
    Ok(())
}

#[test]
fn corruption_wrong_witness_and_rollback_are_rejected() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create("tamper")?;
    let mut opened = seeded_engine(&temporary.path().join("data"))?;
    let first = opened
        .engine
        .query_with_proof(&query(), &ExecutionLimits::default())?;
    let proof_path = temporary.path().join("original.hyproof");
    write_result_proof(&proof_path, &first.proof)?;

    let flipped_proof = temporary.path().join("flipped.hyproof");
    fs::copy(&proof_path, &flipped_proof)?;
    flip_last_byte(&flipped_proof)?;
    assert!(matches!(
        verify_result_proof(
            &flipped_proof,
            &first.snapshot.path,
            first.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::ChecksumMismatch)
    ));

    let truncated_proof = temporary.path().join("truncated.hyproof");
    let mut truncated = fs::read(&proof_path)?;
    truncated.pop();
    fs::write(&truncated_proof, truncated)?;
    assert!(matches!(
        verify_result_proof(
            &truncated_proof,
            &first.snapshot.path,
            first.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::Invalid {
            reason: "file length mismatch"
        })
    ));

    let inserted_proof = temporary.path().join("inserted.hyproof");
    let mut inserted = fs::read(&proof_path)?;
    inserted.push(0);
    fs::write(&inserted_proof, inserted)?;
    assert!(matches!(
        verify_result_proof(
            &inserted_proof,
            &first.snapshot.path,
            first.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::Invalid {
            reason: "file length mismatch"
        })
    ));

    let corrupted_snapshot = temporary.path().join("corrupted.hysnap");
    fs::copy(&first.snapshot.path, &corrupted_snapshot)?;
    flip_last_byte(&corrupted_snapshot)?;
    assert!(matches!(
        verify_result_proof(
            &proof_path,
            &corrupted_snapshot,
            first.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::Snapshot { .. })
    ));

    opened
        .engine
        .put_record(Uuid::now_v7(), &Record::new(b"gamma", value(30, "y")))?;
    let current = opened
        .engine
        .query_with_proof(&query(), &ExecutionLimits::default())?;
    assert!(matches!(
        verify_result_proof(
            &proof_path,
            &first.snapshot.path,
            current.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::AnchorMismatch)
    ));
    assert!(matches!(
        verify_result_proof(
            &proof_path,
            &current.snapshot.path,
            first.proof.anchor_digest(),
            &VerificationLimits::default(),
        ),
        Err(ProofError::SnapshotAnchorMismatch)
    ));
    Ok(())
}

fn flip_last_byte(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    std::io::Read::read_exact(&mut file, &mut last)?;
    file.seek(SeekFrom::End(-1))?;
    file.write_all(&[last[0] ^ 1])?;
    file.sync_all()?;
    Ok(())
}
