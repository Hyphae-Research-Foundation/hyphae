// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

use hyphae_query::Record;
use hyphae_retrieval::{
    DurableVectorRecord, ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalRequest,
    HybridRequest, LexicalLimits, fuse_hybrid, retrieve_exact, retrieve_lexical,
};
use hyphae_storage::{SnapshotContents, load_snapshot_with_timeout};

use super::{
    ExactRetrievalProof, ExactRetrievalVerificationReport, HybridRetrievalProof,
    HybridRetrievalVerificationReport, LexicalRetrievalProof, LexicalRetrievalVerificationReport,
    MAX_RETRIEVAL_PROOF_BYTES, RetrievalProofAnchor, RetrievalProofError,
    RetrievalVerificationLimits, decode_hybrid_proof, decode_lexical_proof, decode_proof,
    encode_hybrid_proof, encode_lexical_proof, encode_proof,
};
use crate::decode_document;

const PROOF_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Writes a canonical exact-retrieval proof to a new synchronized file.
///
/// Existing paths are never replaced.
///
/// # Errors
///
/// Returns a proof encoding, create, write, or synchronization error.
pub fn write_exact_retrieval_proof(
    path: impl AsRef<Path>,
    proof: &ExactRetrievalProof,
) -> Result<(), RetrievalProofError> {
    let encoded = encode_proof(proof)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

/// Writes a canonical lexical-retrieval proof to a new synchronized file.
///
/// # Errors
///
/// Returns a proof encoding, create, write, or synchronization error.
pub fn write_lexical_retrieval_proof(
    path: impl AsRef<Path>,
    proof: &LexicalRetrievalProof,
) -> Result<(), RetrievalProofError> {
    write_new(path, &encode_lexical_proof(proof)?)
}

/// Writes a canonical hybrid-retrieval proof to a new synchronized file.
///
/// # Errors
///
/// Returns a proof encoding, create, write, or synchronization error.
pub fn write_hybrid_retrieval_proof(
    path: impl AsRef<Path>,
    proof: &HybridRetrievalProof,
) -> Result<(), RetrievalProofError> {
    write_new(path, &encode_hybrid_proof(proof)?)
}

/// Reads and verifies one canonical exact-retrieval proof under a byte limit.
///
/// # Errors
///
/// Returns an I/O, resource-limit, framing, canonicality, checksum, or digest
/// error.
pub fn read_exact_retrieval_proof(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
) -> Result<ExactRetrievalProof, RetrievalProofError> {
    let mut no_deadline = || Ok(());
    decode_proof(&read_bounded(path, maximum_bytes, &mut no_deadline)?)
}

/// Reads and verifies one canonical lexical-retrieval proof.
///
/// # Errors
///
/// Returns an I/O, resource-limit, framing, canonicality, checksum, or digest
/// error.
pub fn read_lexical_retrieval_proof(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
) -> Result<LexicalRetrievalProof, RetrievalProofError> {
    let mut no_deadline = || Ok(());
    decode_lexical_proof(&read_bounded(path, maximum_bytes, &mut no_deadline)?)
}

/// Reads and verifies one canonical hybrid-retrieval proof.
///
/// # Errors
///
/// Returns an I/O, resource-limit, framing, canonicality, checksum, or digest
/// error.
pub fn read_hybrid_retrieval_proof(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
) -> Result<HybridRetrievalProof, RetrievalProofError> {
    let mut no_deadline = || Ok(());
    decode_hybrid_proof(&read_bounded(path, maximum_bytes, &mut no_deadline)?)
}

/// Verifies an exact-retrieval proof completely offline against a trusted
/// anchor and canonical format-2 snapshot witness.
///
/// # Errors
///
/// Returns an error for corruption, wrong trust anchor, wrong witness,
/// unsupported semantics, resource exhaustion, timeout, or replay mismatch.
/// No partial outcome is accepted.
pub fn verify_exact_retrieval_proof(
    proof_path: impl AsRef<Path>,
    snapshot_path: impl AsRef<Path>,
    expected_anchor_digest: [u8; 32],
    limits: &RetrievalVerificationLimits,
) -> Result<ExactRetrievalVerificationReport, RetrievalProofError> {
    let started = Instant::now();
    let mut check_read_deadline = || check_timeout(started, limits);
    let proof = decode_proof(&read_bounded(
        proof_path,
        limits.proof_bytes,
        &mut check_read_deadline,
    )?)?;
    check_timeout(started, limits)?;

    let anchor_digest = proof.anchor_digest();
    if anchor_digest != expected_anchor_digest {
        return Err(RetrievalProofError::AnchorMismatch);
    }

    let snapshot = load_snapshot_before_deadline(snapshot_path, limits, started)?;
    check_timeout(started, limits)?;
    if snapshot.info.disk_format_version != 2 {
        return Err(RetrievalProofError::SnapshotFormatMismatch);
    }
    if RetrievalProofAnchor::from_snapshot(&snapshot.info) != *proof.anchor() {
        return Err(RetrievalProofError::SnapshotAnchorMismatch);
    }

    let Some(definition) = snapshot
        .vector_spaces
        .iter()
        .find(|definition| definition.name == proof.request().vector_space)
    else {
        return Err(RetrievalProofError::Invalid {
            reason: "proof references an unknown vector space",
        });
    };
    definition.validate_vector(&proof.request().query)?;

    let candidates = materialize_exact_candidates(&snapshot, proof.request(), limits, || {
        check_timeout(started, limits)
    })?;

    let remaining = limits
        .timeout
        .checked_sub(started.elapsed())
        .ok_or(RetrievalProofError::TimedOut)?;
    if remaining.is_zero() {
        return Err(RetrievalProofError::TimedOut);
    }
    let execution_limits = ExactRetrievalLimits {
        max_candidates: limits.max_candidates,
        max_candidate_bytes: limits.max_candidate_bytes,
        max_returned: limits.max_returned,
        timeout: remaining,
    };
    let actual = retrieve_exact(&candidates, proof.request(), &execution_limits)?;
    if &actual != proof.outcome() {
        return Err(RetrievalProofError::ReexecutionMismatch);
    }
    check_timeout(started, limits)?;

    Ok(ExactRetrievalVerificationReport {
        anchor: proof.anchor().clone(),
        anchor_digest,
        proof_digest: proof.proof_digest(),
        outcome: actual,
    })
}

/// Verifies a lexical-retrieval proof completely offline.
///
/// # Errors
///
/// Returns an error for corruption, wrong anchor/witness, unsupported
/// semantics, resource exhaustion, timeout, or replay mismatch.
pub fn verify_lexical_retrieval_proof(
    proof_path: impl AsRef<Path>,
    snapshot_path: impl AsRef<Path>,
    expected_anchor_digest: [u8; 32],
    limits: &RetrievalVerificationLimits,
) -> Result<LexicalRetrievalVerificationReport, RetrievalProofError> {
    let started = Instant::now();
    let mut check_read_deadline = || check_timeout(started, limits);
    let proof = decode_lexical_proof(&read_bounded(
        proof_path,
        limits.proof_bytes,
        &mut check_read_deadline,
    )?)?;
    let snapshot = load_bound_snapshot(
        snapshot_path,
        proof.anchor(),
        expected_anchor_digest,
        limits,
        started,
    )?;
    let definition = snapshot
        .lexical_indexes
        .iter()
        .find(|definition| definition.name == proof.request().index)
        .ok_or(RetrievalProofError::Invalid {
            reason: "proof references an unknown lexical index",
        })?;
    let records = decode_records(&snapshot, started, limits)?;
    let remaining = remaining_timeout(started, limits)?;
    let execution_limits = LexicalLimits {
        max_documents: limits.max_documents,
        max_tokens: limits.max_tokens,
        max_candidates: limits.max_lexical_candidates,
        max_returned: limits.max_lexical_returned,
        timeout: remaining,
    };
    let actual = retrieve_lexical(&records, definition, proof.request(), &execution_limits)?;
    if &actual != proof.outcome() {
        return Err(RetrievalProofError::ReexecutionMismatch);
    }
    check_timeout(started, limits)?;
    Ok(LexicalRetrievalVerificationReport {
        anchor: proof.anchor().clone(),
        anchor_digest: proof.anchor_digest(),
        proof_digest: proof.proof_digest(),
        outcome: actual,
    })
}

/// Verifies a hybrid-retrieval proof completely offline.
///
/// # Errors
///
/// Returns an error for corruption, wrong anchor/witness, unsupported
/// semantics, resource exhaustion, timeout, branch mismatch, or fusion
/// mismatch.
pub fn verify_hybrid_retrieval_proof(
    proof_path: impl AsRef<Path>,
    snapshot_path: impl AsRef<Path>,
    expected_anchor_digest: [u8; 32],
    limits: &RetrievalVerificationLimits,
) -> Result<HybridRetrievalVerificationReport, RetrievalProofError> {
    let started = Instant::now();
    let mut check_read_deadline = || check_timeout(started, limits);
    let proof = decode_hybrid_proof(&read_bounded(
        proof_path,
        limits.proof_bytes,
        &mut check_read_deadline,
    )?)?;
    let snapshot = load_bound_snapshot(
        snapshot_path,
        proof.anchor(),
        expected_anchor_digest,
        limits,
        started,
    )?;
    let definition = snapshot
        .lexical_indexes
        .iter()
        .find(|definition| definition.name == proof.lexical_request().index)
        .ok_or(RetrievalProofError::Invalid {
            reason: "proof references an unknown lexical index",
        })?;
    let records = decode_records(&snapshot, started, limits)?;
    let lexical = retrieve_lexical(
        &records,
        definition,
        proof.lexical_request(),
        &LexicalLimits {
            max_documents: limits.max_documents,
            max_tokens: limits.max_tokens,
            max_candidates: limits.max_lexical_candidates,
            max_returned: limits.max_lexical_returned,
            timeout: remaining_timeout(started, limits)?,
        },
    )?;
    if &lexical != proof.lexical_outcome() {
        return Err(RetrievalProofError::ReexecutionMismatch);
    }
    let vector = replay_exact(&snapshot, proof.vector_request(), started, limits)?;
    if &vector != proof.vector_outcome() {
        return Err(RetrievalProofError::ReexecutionMismatch);
    }
    if proof.fusion_request().limit > limits.max_hybrid_returned {
        return Err(RetrievalProofError::Invalid {
            reason: "hybrid result limit exceeds verifier policy",
        });
    }
    let fusion_request = HybridRequest {
        lexical_weight: proof.fusion_request().lexical_weight,
        vector_weight: proof.fusion_request().vector_weight,
        limit: proof.fusion_request().limit,
    };
    let actual = fuse_hybrid(&lexical, &vector, &fusion_request)?;
    if &actual != proof.outcome() {
        return Err(RetrievalProofError::ReexecutionMismatch);
    }
    check_timeout(started, limits)?;
    Ok(HybridRetrievalVerificationReport {
        anchor: proof.anchor().clone(),
        anchor_digest: proof.anchor_digest(),
        proof_digest: proof.proof_digest(),
        outcome: actual,
    })
}

fn write_new(path: impl AsRef<Path>, encoded: &[u8]) -> Result<(), RetrievalProofError> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(encoded)?;
    file.sync_all()?;
    Ok(())
}

fn read_bounded(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
    check_deadline: &mut impl FnMut() -> Result<(), RetrievalProofError>,
) -> Result<Vec<u8>, RetrievalProofError> {
    check_deadline()?;
    let path = path.as_ref();
    let path_metadata = std::fs::metadata(path)?;
    check_deadline()?;
    ensure_regular_proof_file(&path_metadata)?;

    let file = File::open(path)?;
    check_deadline()?;
    let initial_metadata = file.metadata()?;
    check_deadline()?;
    ensure_regular_proof_file(&initial_metadata)?;

    read_open_bounded(
        file,
        &initial_metadata,
        maximum_bytes.min(MAX_RETRIEVAL_PROOF_BYTES),
        check_deadline,
    )
}

fn read_open_bounded(
    mut file: File,
    initial_metadata: &Metadata,
    maximum_bytes: u64,
    check_deadline: &mut impl FnMut() -> Result<(), RetrievalProofError>,
) -> Result<Vec<u8>, RetrievalProofError> {
    let initial_length = initial_metadata.len();
    if initial_length > maximum_bytes {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: initial_length,
            maximum: maximum_bytes,
        });
    }
    let capacity =
        usize::try_from(initial_length).map_err(|_| RetrievalProofError::LengthOverflow)?;
    let mut encoded = Vec::with_capacity(capacity);
    let mut remaining = maximum_bytes
        .checked_add(1)
        .ok_or(RetrievalProofError::LengthOverflow)?;
    let mut buffer = vec![0_u8; PROOF_READ_BUFFER_BYTES];
    while remaining > 0 {
        check_deadline()?;
        let read_length = usize::try_from(remaining.min(PROOF_READ_BUFFER_BYTES as u64))
            .map_err(|_| RetrievalProofError::LengthOverflow)?;
        let read = file.read(&mut buffer[..read_length])?;
        check_deadline()?;
        if read == 0 {
            break;
        }
        encoded.extend_from_slice(&buffer[..read]);
        remaining = remaining
            .checked_sub(u64::try_from(read).map_err(|_| RetrievalProofError::LengthOverflow)?)
            .ok_or(RetrievalProofError::LengthOverflow)?;
    }

    let final_metadata = file.metadata()?;
    check_deadline()?;
    ensure_regular_proof_file(&final_metadata)?;
    let actual = u64::try_from(encoded.len()).map_err(|_| RetrievalProofError::LengthOverflow)?;
    let observed = actual.max(final_metadata.len());
    if observed > maximum_bytes {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: observed,
            maximum: maximum_bytes,
        });
    }
    if actual != initial_length || final_metadata.len() != initial_length {
        return Err(RetrievalProofError::Invalid {
            reason: "proof changed while being read",
        });
    }
    Ok(encoded)
}

fn ensure_regular_proof_file(metadata: &Metadata) -> Result<(), RetrievalProofError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(RetrievalProofError::Invalid {
            reason: "proof path is not a regular file",
        })
    }
}

fn load_bound_snapshot(
    path: impl AsRef<Path>,
    anchor: &RetrievalProofAnchor,
    expected_anchor_digest: [u8; 32],
    limits: &RetrievalVerificationLimits,
    started: Instant,
) -> Result<SnapshotContents, RetrievalProofError> {
    if anchor.digest() != expected_anchor_digest {
        return Err(RetrievalProofError::AnchorMismatch);
    }
    let snapshot = load_snapshot_before_deadline(path, limits, started)?;
    check_timeout(started, limits)?;
    if snapshot.info.disk_format_version != 2 {
        return Err(RetrievalProofError::SnapshotFormatMismatch);
    }
    if RetrievalProofAnchor::from_snapshot(&snapshot.info) != *anchor {
        return Err(RetrievalProofError::SnapshotAnchorMismatch);
    }
    Ok(snapshot)
}

fn load_snapshot_before_deadline(
    path: impl AsRef<Path>,
    limits: &RetrievalVerificationLimits,
    started: Instant,
) -> Result<SnapshotContents, RetrievalProofError> {
    match load_snapshot_with_timeout(path, &limits.snapshot, remaining_timeout(started, limits)?) {
        Err(error) if error.is_timeout() => Err(RetrievalProofError::TimedOut),
        Err(error) => Err(error.into()),
        Ok(snapshot) => Ok(snapshot),
    }
}

fn decode_records(
    snapshot: &SnapshotContents,
    started: Instant,
    limits: &RetrievalVerificationLimits,
) -> Result<Vec<Record>, RetrievalProofError> {
    if u64::try_from(snapshot.entries.len()).unwrap_or(u64::MAX) > limits.max_documents {
        return Err(RetrievalProofError::Invalid {
            reason: "snapshot document count exceeds verifier policy",
        });
    }
    snapshot
        .entries
        .iter()
        .map(|entry| {
            check_timeout(started, limits)?;
            Ok(Record {
                key: entry.key.clone(),
                value: decode_document(&entry.value)?,
            })
        })
        .collect()
}

fn replay_exact(
    snapshot: &SnapshotContents,
    request: &ExactRetrievalRequest,
    started: Instant,
    limits: &RetrievalVerificationLimits,
) -> Result<hyphae_retrieval::ExactRetrievalOutcome, RetrievalProofError> {
    let definition = snapshot
        .vector_spaces
        .iter()
        .find(|definition| definition.name == request.vector_space)
        .ok_or(RetrievalProofError::Invalid {
            reason: "proof references an unknown vector space",
        })?;
    definition.validate_vector(&request.query)?;
    let candidates =
        materialize_exact_candidates(snapshot, request, limits, || check_timeout(started, limits))?;
    Ok(retrieve_exact(
        &candidates,
        request,
        &ExactRetrievalLimits {
            max_candidates: limits.max_candidates,
            max_candidate_bytes: limits.max_candidate_bytes,
            max_returned: limits.max_returned,
            timeout: remaining_timeout(started, limits)?,
        },
    )?)
}

fn materialize_exact_candidates(
    snapshot: &SnapshotContents,
    request: &ExactRetrievalRequest,
    limits: &RetrievalVerificationLimits,
    mut check_deadline: impl FnMut() -> Result<(), RetrievalProofError>,
) -> Result<Vec<DurableVectorRecord>, RetrievalProofError> {
    let matching = || {
        snapshot
            .vectors
            .iter()
            .filter(|vector| vector.space == request.vector_space)
    };
    let mut candidate_count = 0_u64;
    let mut candidate_bytes = 0_u64;
    for vector in matching() {
        check_deadline()?;
        if candidate_count >= limits.max_candidates {
            return Err(ExactRetrievalError::CandidateBudgetExceeded {
                maximum: limits.max_candidates,
            }
            .into());
        }
        candidate_count =
            candidate_count
                .checked_add(1)
                .ok_or(ExactRetrievalError::CandidateBudgetExceeded {
                    maximum: limits.max_candidates,
                })?;

        let vector_bytes = u64::try_from(vector.vector.as_slice().len())
            .ok()
            .and_then(|elements| elements.checked_mul(2))
            .ok_or(ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            })?;
        let bytes = u64::try_from(vector.key.len())
            .ok()
            .and_then(|key_bytes| key_bytes.checked_add(vector_bytes))
            .ok_or(ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            })?;
        candidate_bytes = candidate_bytes.checked_add(bytes).ok_or(
            ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            },
        )?;
        if candidate_bytes > limits.max_candidate_bytes {
            return Err(ExactRetrievalError::CandidateByteBudgetExceeded {
                maximum: limits.max_candidate_bytes,
            }
            .into());
        }
    }
    check_deadline()?;

    let capacity = usize::try_from(candidate_count).map_err(|_| {
        ExactRetrievalError::CandidateBudgetExceeded {
            maximum: limits.max_candidates,
        }
    })?;
    let mut candidates = Vec::with_capacity(capacity);
    for vector in matching() {
        check_deadline()?;
        candidates.push(DurableVectorRecord {
            key: vector.key.clone(),
            vector: vector.vector.clone(),
        });
    }
    check_deadline()?;
    Ok(candidates)
}

fn remaining_timeout(
    started: Instant,
    limits: &RetrievalVerificationLimits,
) -> Result<Duration, RetrievalProofError> {
    let remaining = limits
        .timeout
        .checked_sub(started.elapsed())
        .ok_or(RetrievalProofError::TimedOut)?;
    if remaining.is_zero() {
        Err(RetrievalProofError::TimedOut)
    } else {
        Ok(remaining)
    }
}

fn check_timeout(
    started: Instant,
    limits: &RetrievalVerificationLimits,
) -> Result<(), RetrievalProofError> {
    if started.elapsed() >= limits.timeout {
        Err(RetrievalProofError::TimedOut)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap, error::Error, fs, io::Write as _, path::PathBuf, time::Duration,
    };

    use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
    use hyphae_query::{FieldPath, Record, Value};
    use hyphae_retrieval::{
        ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalOutcome, ExactRetrievalRequest,
        HybridRequest, LexicalField, LexicalIndexDefinition, LexicalLimits, LexicalRequest,
    };
    use hyphae_storage::{SnapshotReadLimits, load_snapshot};
    use uuid::Uuid;

    use super::{
        PROOF_READ_BUFFER_BYTES, RetrievalVerificationLimits, materialize_exact_candidates,
        read_exact_retrieval_proof, read_open_bounded, verify_exact_retrieval_proof,
        verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof, write_exact_retrieval_proof,
        write_hybrid_retrieval_proof, write_lexical_retrieval_proof,
    };
    use crate::{
        HyphaeEngine, MAX_RETRIEVAL_PROOF_BYTES, RetrievalProofError,
        write_exact_retrieval_proof as write,
    };

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn create(name: &str) -> Result<Self, Box<dyn Error>> {
            let path = std::env::temp_dir()
                .join(format!("hyphae-retrieval-proof-{name}-{}", Uuid::now_v7()));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn retrieval_proof_reader_enforces_the_canonical_hard_limit() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("oversized-proof")?;
        let proof_path = temporary.path.join("oversized.hyrproof");
        let file = fs::File::create(&proof_path)?;
        file.set_len(MAX_RETRIEVAL_PROOF_BYTES + 1)?;
        drop(file);

        assert!(matches!(
            read_exact_retrieval_proof(&proof_path, u64::MAX),
            Err(RetrievalProofError::ProofLimitExceeded {
                actual,
                maximum: MAX_RETRIEVAL_PROOF_BYTES,
            }) if actual == MAX_RETRIEVAL_PROOF_BYTES + 1
        ));
        Ok(())
    }

    #[test]
    fn retrieval_proof_reader_detects_same_handle_growth() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("growing-proof")?;
        let proof_path = temporary.path.join("growing.hyrproof");
        fs::write(&proof_path, b"initial")?;
        let file = fs::File::open(&proof_path)?;
        let initial_metadata = file.metadata()?;
        let mut writer = fs::OpenOptions::new().append(true).open(&proof_path)?;
        writer.write_all(b"-growth")?;
        writer.sync_all()?;
        drop(writer);

        let mut no_deadline = || Ok(());
        assert!(matches!(
            read_open_bounded(file, &initial_metadata, 1024, &mut no_deadline),
            Err(RetrievalProofError::Invalid {
                reason: "proof changed while being read",
            })
        ));
        Ok(())
    }

    #[test]
    fn retrieval_proof_reader_checks_deadline_between_chunks() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("timed-proof")?;
        let proof_path = temporary.path.join("timed.hyrproof");
        fs::write(&proof_path, vec![0_u8; PROOF_READ_BUFFER_BYTES * 2])?;
        let file = fs::File::open(&proof_path)?;
        let initial_metadata = file.metadata()?;
        let mut checks = 0_u8;
        let mut deadline = || {
            checks += 1;
            if checks == 2 {
                Err(RetrievalProofError::TimedOut)
            } else {
                Ok(())
            }
        };

        assert!(matches!(
            read_open_bounded(
                file,
                &initial_metadata,
                MAX_RETRIEVAL_PROOF_BYTES,
                &mut deadline,
            ),
            Err(RetrievalProofError::TimedOut)
        ));
        assert_eq!(checks, 2);
        Ok(())
    }

    #[test]
    fn exact_candidate_preflight_rejects_count_and_bytes_before_materialization()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("candidate-preflight")?;
        let (artifact, _) = create_artifact(&temporary.path.join("data"))?;
        let snapshot = load_snapshot(&artifact.snapshot.path, &SnapshotReadLimits::default())?;

        let count_limits = RetrievalVerificationLimits {
            max_candidates: 1,
            ..RetrievalVerificationLimits::default()
        };
        let mut count_checks = 0_u8;
        let count_result = materialize_exact_candidates(
            &snapshot,
            artifact.proof.request(),
            &count_limits,
            || {
                count_checks += 1;
                Ok(())
            },
        );
        assert!(matches!(
            count_result,
            Err(RetrievalProofError::Retrieval { source })
                if *source
                    == ExactRetrievalError::CandidateBudgetExceeded { maximum: 1 }
        ));
        assert_eq!(count_checks, 2);

        let byte_limits = RetrievalVerificationLimits {
            max_candidate_bytes: 9,
            ..RetrievalVerificationLimits::default()
        };
        let mut byte_checks = 0_u8;
        let byte_result =
            materialize_exact_candidates(&snapshot, artifact.proof.request(), &byte_limits, || {
                byte_checks += 1;
                Ok(())
            });
        assert!(matches!(
            byte_result,
            Err(RetrievalProofError::Retrieval { source })
                if *source
                    == ExactRetrievalError::CandidateByteBudgetExceeded { maximum: 9 }
        ));
        assert_eq!(byte_checks, 2);
        Ok(())
    }

    #[test]
    fn exact_candidate_materialization_checks_its_deadline() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("candidate-deadline")?;
        let (artifact, _) = create_artifact(&temporary.path.join("data"))?;
        let snapshot = load_snapshot(&artifact.snapshot.path, &SnapshotReadLimits::default())?;
        let limits = RetrievalVerificationLimits::default();
        let mut checks = 0_u8;

        let result =
            materialize_exact_candidates(&snapshot, artifact.proof.request(), &limits, || {
                checks += 1;
                if checks == 4 {
                    Err(RetrievalProofError::TimedOut)
                } else {
                    Ok(())
                }
            });

        assert!(matches!(result, Err(RetrievalProofError::TimedOut)));
        assert_eq!(checks, 4);
        Ok(())
    }

    fn request(space: VectorSpaceName) -> Result<ExactRetrievalRequest, Box<dyn Error>> {
        Ok(ExactRetrievalRequest {
            vector_space: space,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 2,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        })
    }

    fn create_artifact(
        root: &std::path::Path,
    ) -> Result<(crate::ExactRetrievalProofArtifact, ExactRetrievalLimits), Box<dyn Error>> {
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
        let limits = ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 1024,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        };
        let artifact = opened
            .engine
            .retrieve_exact_with_proof(&request(space)?, &limits)?;
        Ok((artifact, limits))
    }

    fn lexical_record(key: &[u8], title: &str, body: &str) -> Record {
        Record::new(
            key,
            Value::Object(BTreeMap::from([
                ("title".into(), Value::String(title.into())),
                ("body".into(), Value::String(body.into())),
            ])),
        )
    }

    #[allow(clippy::type_complexity)]
    fn create_multimodal_engine(
        root: &std::path::Path,
    ) -> Result<
        (
            HyphaeEngine,
            LexicalRequest,
            LexicalLimits,
            ExactRetrievalRequest,
            ExactRetrievalLimits,
            HybridRequest,
        ),
        Box<dyn Error>,
    > {
        let lexical_name = VectorSpaceName::new("content")?;
        let vector_space = VectorSpaceName::new("semantic")?;
        let mut opened = HyphaeEngine::open(root)?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[
                lexical_record(b"alpha", "Durable memory", "offline agent memory"),
                lexical_record(b"beta", "Fast search", "exact vector retrieval"),
            ],
        )?;
        opened.engine.define_lexical_index(
            Uuid::now_v7(),
            LexicalIndexDefinition::new(
                lexical_name.clone(),
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
        Ok((
            opened.engine,
            LexicalRequest {
                index: lexical_name,
                query: "durable memory".into(),
                limit: 2,
            },
            LexicalLimits {
                max_documents: 10,
                max_tokens: 1_000,
                max_candidates: 10,
                max_returned: 10,
                timeout: Duration::from_secs(2),
            },
            ExactRetrievalRequest {
                vector_space,
                query: Q15Vector::new(vec![32_767, 0])?,
                limit: 2,
                minimum_score_nanos: -1_000_000_000,
                minimum_margin_nanos: 0,
            },
            ExactRetrievalLimits {
                max_candidates: 10,
                max_candidate_bytes: 1_024,
                max_returned: 10,
                timeout: Duration::from_secs(2),
            },
            HybridRequest {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 2,
            },
        ))
    }

    #[test]
    fn exact_proof_verifies_after_originating_directory_is_deleted() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("offline")?;
        let data = temporary.path.join("data");
        let portable = temporary.path.join("portable");
        fs::create_dir_all(&portable)?;
        let (artifact, _execution_limits) = create_artifact(&data)?;
        let proof_path = portable.join("result.hyrproof");
        let witness_path = portable.join("witness.hysnap");
        write_exact_retrieval_proof(&proof_path, &artifact.proof)?;
        fs::copy(&artifact.snapshot.path, &witness_path)?;
        fs::remove_dir_all(&data)?;

        let report = verify_exact_retrieval_proof(
            &proof_path,
            &witness_path,
            artifact.proof.anchor_digest(),
            &RetrievalVerificationLimits::default(),
        )?;
        assert_eq!(report.outcome, artifact.proof.outcome().clone());
        Ok(())
    }

    #[test]
    fn self_consistently_rehashed_request_and_outcome_edits_are_rejected()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("tamper")?;
        let (artifact, _) = create_artifact(&temporary.path.join("data"))?;

        for mutation in 0..5 {
            let mut forged = artifact.proof.clone();
            match mutation {
                0 => forged.request.query = Q15Vector::new(vec![0, 32_767])?,
                1 => {
                    let ExactRetrievalOutcome::Matches { matches, .. } = &mut forged.outcome else {
                        return Err("expected matches".into());
                    };
                    matches[0].score_nanos -= 1;
                }
                2 => {
                    let ExactRetrievalOutcome::Matches { matches, .. } = &mut forged.outcome else {
                        return Err("expected matches".into());
                    };
                    matches.swap(0, 1);
                }
                3 => {
                    let ExactRetrievalOutcome::Matches { matches, .. } = &mut forged.outcome else {
                        return Err("expected matches".into());
                    };
                    matches[0].key = b"forged".to_vec();
                }
                4 => forged.request.vector_space = VectorSpaceName::new("other")?,
                _ => unreachable!(),
            }
            let proof_path = temporary.path.join(format!("forged-{mutation}.hyrproof"));
            if write(&proof_path, &forged).is_err() {
                continue;
            }
            let result = verify_exact_retrieval_proof(
                &proof_path,
                &artifact.snapshot.path,
                artifact.proof.anchor_digest(),
                &RetrievalVerificationLimits::default(),
            );
            assert!(result.is_err(), "mutation {mutation} unexpectedly verified");
        }
        Ok(())
    }

    #[test]
    fn wrong_witness_anchor_and_semantics_are_rejected() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("binding")?;
        let (artifact, _) = create_artifact(&temporary.path.join("data"))?;
        let proof_path = temporary.path.join("proof.hyrproof");
        write(&proof_path, &artifact.proof)?;

        assert!(matches!(
            verify_exact_retrieval_proof(
                &proof_path,
                &artifact.snapshot.path,
                [0; 32],
                &RetrievalVerificationLimits::default(),
            ),
            Err(RetrievalProofError::AnchorMismatch)
        ));

        let mut encoded = fs::read(&proof_path)?;
        encoded[14..16].copy_from_slice(&99_u16.to_le_bytes());
        let semantics_path = temporary.path.join("semantics.hyrproof");
        fs::write(&semantics_path, encoded)?;
        assert!(matches!(
            super::read_exact_retrieval_proof(&semantics_path, u64::MAX),
            Err(RetrievalProofError::UnsupportedSemantics { found: 99, .. })
        ));
        Ok(())
    }

    #[test]
    fn lexical_and_hybrid_proofs_verify_without_the_originating_directory()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("multimodal-offline")?;
        let data = temporary.path.join("data");
        let portable = temporary.path.join("portable");
        fs::create_dir_all(&portable)?;
        let (
            engine,
            lexical_request,
            lexical_limits,
            vector_request,
            vector_limits,
            hybrid_request,
        ) = create_multimodal_engine(&data)?;
        let lexical = engine.retrieve_lexical_with_proof(&lexical_request, &lexical_limits)?;
        let hybrid = engine.retrieve_hybrid_with_proof(
            &lexical_request,
            &lexical_limits,
            &vector_request,
            &vector_limits,
            &hybrid_request,
        )?;
        let lexical_proof = portable.join("lexical.hyrproof");
        let hybrid_proof = portable.join("hybrid.hyrproof");
        let witness = portable.join("witness.hysnap");
        write_lexical_retrieval_proof(&lexical_proof, &lexical.proof)?;
        write_hybrid_retrieval_proof(&hybrid_proof, &hybrid.proof)?;
        fs::copy(&hybrid.snapshot.path, &witness)?;
        drop(engine);
        fs::remove_dir_all(&data)?;

        let lexical_report = verify_lexical_retrieval_proof(
            &lexical_proof,
            &witness,
            lexical.proof.anchor_digest(),
            &RetrievalVerificationLimits::default(),
        )?;
        let hybrid_report = verify_hybrid_retrieval_proof(
            &hybrid_proof,
            &witness,
            hybrid.proof.anchor_digest(),
            &RetrievalVerificationLimits::default(),
        )?;
        assert_eq!(lexical_report.outcome, lexical.proof.outcome().clone());
        assert_eq!(hybrid_report.outcome, hybrid.proof.outcome().clone());
        Ok(())
    }

    #[test]
    fn lexical_and_hybrid_self_consistent_tampering_fails_reexecution() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create("multimodal-tamper")?;
        let (
            engine,
            lexical_request,
            lexical_limits,
            vector_request,
            vector_limits,
            hybrid_request,
        ) = create_multimodal_engine(&temporary.path.join("data"))?;
        let lexical = engine.retrieve_lexical_with_proof(&lexical_request, &lexical_limits)?;
        let hybrid = engine.retrieve_hybrid_with_proof(
            &lexical_request,
            &lexical_limits,
            &vector_request,
            &vector_limits,
            &hybrid_request,
        )?;

        let mut forged_lexical = lexical.proof.clone();
        let hyphae_retrieval::LexicalOutcome::Matches { matches, .. } = &mut forged_lexical.outcome
        else {
            return Err("expected lexical matches".into());
        };
        matches[0].score_nanos -= 1;
        matches[0].terms[0].score_nanos -= 1;
        let lexical_path = temporary.path.join("forged-lexical.hyrproof");
        write_lexical_retrieval_proof(&lexical_path, &forged_lexical)?;
        assert!(
            verify_lexical_retrieval_proof(
                &lexical_path,
                &lexical.snapshot.path,
                lexical.proof.anchor_digest(),
                &RetrievalVerificationLimits::default(),
            )
            .is_err()
        );

        let mut forged_hybrid = hybrid.proof.clone();
        forged_hybrid.vector_request.query = Q15Vector::new(vec![0, 32_767])?;
        let hybrid_path = temporary.path.join("forged-hybrid.hyrproof");
        write_hybrid_retrieval_proof(&hybrid_path, &forged_hybrid)?;
        assert!(
            verify_hybrid_retrieval_proof(
                &hybrid_path,
                &hybrid.snapshot.path,
                hybrid.proof.anchor_digest(),
                &RetrievalVerificationLimits::default(),
            )
            .is_err()
        );
        Ok(())
    }
}
