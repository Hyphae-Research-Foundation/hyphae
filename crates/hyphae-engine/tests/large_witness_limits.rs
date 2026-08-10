// SPDX-License-Identifier: GPL-3.0-only
//! Regression coverage for large, bounded offline result and retrieval witnesses.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_engine::{
    ExactRetrievalProofArtifact, HyphaeEngine, ProofError, ResultProofArtifact,
    RetrievalProofError, VerificationLimits, retrieval_proof::RetrievalVerificationLimits,
    verify_exact_retrieval_proof, verify_result_proof, write_exact_retrieval_proof,
    write_result_proof,
};
use hyphae_retrieval::{ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalRequest};
use hyphae_storage::{SnapshotError, SnapshotReadLimits};
use uuid::Uuid;

const MEBIBYTE: u64 = 1024 * 1024;
const GIBIBYTE: u64 = 1024 * MEBIBYTE;
const OLD_SNAPSHOT_FILE_BYTES: u64 = 512 * MEBIBYTE;
const DEFAULT_SNAPSHOT_FILE_BYTES: u64 = 2 * GIBIBYTE;
const DEFAULT_DECODED_BYTES: u64 = GIBIBYTE;
const DEFAULT_CANDIDATE_BYTES: u64 = GIBIBYTE;
const EXACT_CANDIDATE_BYTES: u64 = 17;
const SNAPSHOT_DECODED_BYTES: u64 = 41;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-large-witness-{name}-{}-{}",
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

fn exact_artifact(root: &Path) -> Result<ExactRetrievalProofArtifact, Box<dyn Error>> {
    let space = VectorSpaceName::new("semantic")?;
    let mut opened = HyphaeEngine::open(root)?;
    opened.engine.define_vector_space(
        Uuid::now_v7(),
        VectorSpaceDefinition::cosine(space.clone(), 2)?,
    )?;
    opened.engine.put_vectors(
        Uuid::now_v7(),
        &space,
        &[
            (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0])?),
            (b"beta".to_vec(), Q15Vector::new(vec![0, 32_767])?),
        ],
    )?;
    Ok(opened.engine.retrieve_exact_with_proof(
        &ExactRetrievalRequest {
            vector_space: space,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 2,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        },
        &ExactRetrievalLimits {
            max_candidates: 2,
            max_candidate_bytes: 1_024,
            max_returned: 2,
            timeout: Duration::from_secs(5),
        },
    )?)
}

fn result_artifact(root: &Path) -> Result<ResultProofArtifact, Box<dyn Error>> {
    let opened = HyphaeEngine::open(root)?;
    Ok(opened.engine.get_record_with_proof(b"missing")?)
}

fn create_sparse_file(path: &Path, file_bytes: u64) -> Result<(), Box<dyn Error>> {
    drop(fs::File::create_new(path)?);
    #[cfg(windows)]
    {
        let status = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(path)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other("fsutil failed to mark the test file sparse").into());
        }
    }
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(file_bytes)?;
    assert_eq!(fs::metadata(path)?.len(), file_bytes);
    Ok(())
}

fn result_snapshot_error(
    proof_path: &Path,
    snapshot_path: &Path,
    anchor_digest: [u8; 32],
    limits: &VerificationLimits,
) -> Result<SnapshotError, Box<dyn Error>> {
    match verify_result_proof(proof_path, snapshot_path, anchor_digest, limits) {
        Err(ProofError::Snapshot { source }) => Ok(*source),
        Err(error) => Err(error.into()),
        Ok(_) => {
            Err(std::io::Error::other("expected result verification to reject the snapshot").into())
        }
    }
}

fn retrieval_snapshot_error(
    proof_path: &Path,
    snapshot_path: &Path,
    anchor_digest: [u8; 32],
    limits: &RetrievalVerificationLimits,
) -> Result<SnapshotError, Box<dyn Error>> {
    match verify_exact_retrieval_proof(proof_path, snapshot_path, anchor_digest, limits) {
        Err(RetrievalProofError::Snapshot { source }) => Ok(*source),
        Err(error) => Err(error.into()),
        Ok(_) => Err(std::io::Error::other(
            "expected retrieval verification to reject the snapshot",
        )
        .into()),
    }
}

fn assert_file_limit(error: &SnapshotError, actual: u64, maximum: u64) {
    assert!(matches!(
        error,
        SnapshotError::FileLimitExceeded {
            actual: found_actual,
            maximum: found_maximum,
        } if *found_actual == actual && *found_maximum == maximum
    ));
}

fn assert_bad_magic(error: &SnapshotError) {
    assert!(matches!(
        error,
        SnapshotError::Invalid {
            reason: "bad magic"
        }
    ));
}

#[test]
fn default_offline_limits_raise_snapshot_and_candidate_budgets_together() {
    let snapshot = SnapshotReadLimits::default();
    assert_eq!(snapshot.file_bytes, DEFAULT_SNAPSHOT_FILE_BYTES);
    assert_eq!(snapshot.decoded_bytes, DEFAULT_DECODED_BYTES);

    let result = VerificationLimits::default();
    assert_eq!(result.snapshot, snapshot);

    let retrieval = RetrievalVerificationLimits::default();
    assert_eq!(retrieval.snapshot, snapshot);
    assert_eq!(retrieval.max_candidate_bytes, DEFAULT_CANDIDATE_BYTES);
}

#[test]
fn verifier_accepts_exact_scaled_limits_and_rejects_one_byte_less() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create("scaled-boundaries")?;
    let artifact = exact_artifact(&temporary.path.join("data"))?;
    assert_eq!(artifact.snapshot.vector_space_count, 1);
    assert_eq!(artifact.snapshot.vector_count, 2);

    let proof_path = temporary.path.join("exact.hyrproof");
    write_exact_retrieval_proof(&proof_path, &artifact.proof)?;

    // Snapshot accounting is 8 bytes for the space definition plus
    // (8-byte space + key + 4-byte Q15 vector) for each candidate.
    // Exact replay counts only each key plus its 4-byte vector.
    let limits = RetrievalVerificationLimits {
        snapshot: SnapshotReadLimits {
            file_bytes: artifact.snapshot.file_bytes,
            entries: 3,
            decoded_bytes: SNAPSHOT_DECODED_BYTES,
        },
        max_candidates: 2,
        max_candidate_bytes: EXACT_CANDIDATE_BYTES,
        max_returned: 2,
        timeout: Duration::from_secs(5),
        ..RetrievalVerificationLimits::default()
    };
    let report = verify_exact_retrieval_proof(
        &proof_path,
        &artifact.snapshot.path,
        artifact.proof.anchor_digest(),
        &limits,
    )?;
    assert_eq!(report.outcome, artifact.proof.outcome().clone());

    let mut file_limited = limits.clone();
    file_limited.snapshot.file_bytes = artifact.snapshot.file_bytes.saturating_sub(1);
    assert!(matches!(
        verify_exact_retrieval_proof(
            &proof_path,
            &artifact.snapshot.path,
            artifact.proof.anchor_digest(),
            &file_limited,
        ),
        Err(RetrievalProofError::Snapshot { source })
            if matches!(
                source.as_ref(),
                SnapshotError::FileLimitExceeded { maximum, .. }
                    if *maximum == file_limited.snapshot.file_bytes
            )
    ));

    let mut decoded_limited = limits.clone();
    decoded_limited.snapshot.decoded_bytes = SNAPSHOT_DECODED_BYTES - 1;
    assert!(matches!(
        verify_exact_retrieval_proof(
            &proof_path,
            &artifact.snapshot.path,
            artifact.proof.anchor_digest(),
            &decoded_limited,
        ),
        Err(RetrievalProofError::Snapshot { source })
            if matches!(
                source.as_ref(),
                SnapshotError::DecodedBytesLimitExceeded { maximum }
                    if *maximum == decoded_limited.snapshot.decoded_bytes
            )
    ));

    let mut candidate_limited = limits;
    candidate_limited.max_candidate_bytes = EXACT_CANDIDATE_BYTES - 1;
    assert!(matches!(
        verify_exact_retrieval_proof(
            &proof_path,
            &artifact.snapshot.path,
            artifact.proof.anchor_digest(),
            &candidate_limited,
        ),
        Err(RetrievalProofError::Retrieval { source })
            if matches!(
                source.as_ref(),
                ExactRetrievalError::CandidateByteBudgetExceeded { maximum }
                    if *maximum == candidate_limited.max_candidate_bytes
            )
    ));
    Ok(())
}

#[test]
fn sparse_file_limits_cover_result_and_retrieval() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::create("sparse-file-boundaries")?;

    let result = result_artifact(&temporary.path.join("result-data"))?;
    let result_proof_path = temporary.path.join("result.hyproof");
    write_result_proof(&result_proof_path, &result.proof)?;

    let retrieval = exact_artifact(&temporary.path.join("retrieval-data"))?;
    let retrieval_proof_path = temporary.path.join("retrieval.hyrproof");
    write_exact_retrieval_proof(&retrieval_proof_path, &retrieval.proof)?;

    let above_old_limit = temporary.path.join("above-old-limit.hysnap");
    create_sparse_file(&above_old_limit, OLD_SNAPSHOT_FILE_BYTES + 1)?;

    let old_result_limits = VerificationLimits {
        snapshot: SnapshotReadLimits {
            file_bytes: OLD_SNAPSHOT_FILE_BYTES,
            ..SnapshotReadLimits::default()
        },
        ..VerificationLimits::default()
    };
    assert_file_limit(
        &result_snapshot_error(
            &result_proof_path,
            &above_old_limit,
            result.proof.anchor_digest(),
            &old_result_limits,
        )?,
        OLD_SNAPSHOT_FILE_BYTES + 1,
        OLD_SNAPSHOT_FILE_BYTES,
    );
    assert_bad_magic(&result_snapshot_error(
        &result_proof_path,
        &above_old_limit,
        result.proof.anchor_digest(),
        &VerificationLimits::default(),
    )?);

    let old_retrieval_limits = RetrievalVerificationLimits {
        snapshot: SnapshotReadLimits {
            file_bytes: OLD_SNAPSHOT_FILE_BYTES,
            ..SnapshotReadLimits::default()
        },
        ..RetrievalVerificationLimits::default()
    };
    assert_file_limit(
        &retrieval_snapshot_error(
            &retrieval_proof_path,
            &above_old_limit,
            retrieval.proof.anchor_digest(),
            &old_retrieval_limits,
        )?,
        OLD_SNAPSHOT_FILE_BYTES + 1,
        OLD_SNAPSHOT_FILE_BYTES,
    );
    assert_bad_magic(&retrieval_snapshot_error(
        &retrieval_proof_path,
        &above_old_limit,
        retrieval.proof.anchor_digest(),
        &RetrievalVerificationLimits::default(),
    )?);

    let above_default_limit = temporary.path.join("above-default-limit.hysnap");
    create_sparse_file(&above_default_limit, DEFAULT_SNAPSHOT_FILE_BYTES + 1)?;
    assert_file_limit(
        &result_snapshot_error(
            &result_proof_path,
            &above_default_limit,
            result.proof.anchor_digest(),
            &VerificationLimits::default(),
        )?,
        DEFAULT_SNAPSHOT_FILE_BYTES + 1,
        DEFAULT_SNAPSHOT_FILE_BYTES,
    );
    assert_file_limit(
        &retrieval_snapshot_error(
            &retrieval_proof_path,
            &above_default_limit,
            retrieval.proof.anchor_digest(),
            &RetrievalVerificationLimits::default(),
        )?,
        DEFAULT_SNAPSHOT_FILE_BYTES + 1,
        DEFAULT_SNAPSHOT_FILE_BYTES,
    );

    Ok(())
}
