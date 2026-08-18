// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
    time::Instant,
};

use hyphae_query::{BoundedQueryError, Record, execute_with_byte_limit};
use hyphae_storage::{SnapshotError, load_snapshot_with_timeout};

use super::{
    MAX_RESULT_PROOF_BYTES, ProofAnchor, ProofError, ProvenOperation, ProvenResult, ResultProof,
    VerificationLimits, VerificationReport, decode_proof, encode_proof,
};
use crate::decode_document;

const PROOF_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Writes a canonical result proof to a new file and synchronizes it.
///
/// Existing paths are never replaced.
///
/// # Errors
///
/// Returns a proof encoding, path, create, write, or synchronization error.
pub fn write_result_proof(path: impl AsRef<Path>, proof: &ResultProof) -> Result<(), ProofError> {
    let encoded = encode_proof(proof)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

/// Reads and verifies one canonical result-proof file under a byte limit.
///
/// # Errors
///
/// Returns an I/O, resource-limit, framing, canonicality, checksum, or digest
/// error.
pub fn read_result_proof(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
) -> Result<ResultProof, ProofError> {
    let mut no_deadline = || Ok(());
    decode_proof(&read_result_proof_bytes(
        path,
        maximum_bytes,
        &mut no_deadline,
    )?)
}

/// Verifies a result proof completely offline against a trusted anchor and
/// canonical snapshot witness.
///
/// # Errors
///
/// Returns an error for any proof or snapshot corruption, wrong anchor,
/// resource exhaustion, document failure, timeout, or replay mismatch. No
/// partial result is accepted.
pub fn verify_result_proof(
    proof_path: impl AsRef<Path>,
    snapshot_path: impl AsRef<Path>,
    expected_anchor_digest: [u8; 32],
    limits: &VerificationLimits,
) -> Result<VerificationReport, ProofError> {
    let started = Instant::now();
    let mut check_read_deadline = || check_timeout(started, limits);
    let proof = decode_proof(&read_result_proof_bytes(
        proof_path,
        limits.proof_bytes,
        &mut check_read_deadline,
    )?)?;
    check_timeout(started, limits)?;

    let anchor_digest = proof.anchor_digest();
    if anchor_digest != expected_anchor_digest {
        return Err(ProofError::AnchorMismatch);
    }

    let snapshot = match load_snapshot_with_timeout(
        snapshot_path,
        &limits.snapshot,
        remaining_timeout(started, limits)?,
    ) {
        Err(error) if error.is_timeout() => return Err(ProofError::TimedOut),
        Err(error) => return Err(error.into()),
        Ok(snapshot) => snapshot,
    };
    check_timeout(started, limits)?;
    if ProofAnchor::from_snapshot(&snapshot.info) != *proof.anchor() {
        return Err(ProofError::SnapshotAnchorMismatch);
    }

    let mut records = Vec::with_capacity(snapshot.entries.len());
    for entry in snapshot.entries {
        check_timeout(started, limits)?;
        records.push(Record {
            key: entry.key,
            value: decode_document(&entry.value)?,
        });
    }

    let verified_result = match (proof.operation(), proof.result()) {
        (ProvenOperation::Get { key }, ProvenResult::Get(expected)) => {
            let actual = records
                .binary_search_by(|record| record.key.as_slice().cmp(key))
                .ok()
                .map(|index| records[index].clone());
            if &actual != expected {
                return Err(ProofError::ReexecutionMismatch);
            }
            ProvenResult::Get(actual)
        }
        (ProvenOperation::Query(query), ProvenResult::Query(expected)) => {
            let query_limits = hyphae_query::ExecutionLimits {
                timeout: remaining_timeout(started, limits)?.min(limits.query.timeout),
                ..limits.query.clone()
            };
            let actual = execute_with_byte_limit(
                &[records.as_slice()],
                query,
                &query_limits,
                limits.snapshot.decoded_bytes,
            )
            .map_err(|source| match source {
                BoundedQueryError::Query(source) => ProofError::from(source),
                BoundedQueryError::RecordDocument(source) => ProofError::from(source),
                BoundedQueryError::ScannedByteBudgetExceeded { maximum } => {
                    ProofError::from(SnapshotError::DecodedBytesLimitExceeded { maximum })
                }
            })?;
            if &actual != expected {
                return Err(ProofError::ReexecutionMismatch);
            }
            ProvenResult::Query(actual)
        }
        _ => return Err(ProofError::OperationResultMismatch),
    };
    check_timeout(started, limits)?;

    Ok(VerificationReport {
        anchor: proof.anchor().clone(),
        anchor_digest,
        proof_digest: proof.proof_digest(),
        result: verified_result,
    })
}

fn read_result_proof_bytes(
    path: impl AsRef<Path>,
    maximum_bytes: u64,
    check_deadline: &mut impl FnMut() -> Result<(), ProofError>,
) -> Result<Vec<u8>, ProofError> {
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

    read_open_result_proof(
        file,
        &initial_metadata,
        maximum_bytes.min(MAX_RESULT_PROOF_BYTES),
        check_deadline,
    )
}

fn read_open_result_proof(
    mut file: File,
    initial_metadata: &Metadata,
    maximum_bytes: u64,
    check_deadline: &mut impl FnMut() -> Result<(), ProofError>,
) -> Result<Vec<u8>, ProofError> {
    let initial_length = initial_metadata.len();
    if initial_length > maximum_bytes {
        return Err(ProofError::ProofLimitExceeded {
            actual: initial_length,
            maximum: maximum_bytes,
        });
    }
    let capacity = usize::try_from(initial_length).map_err(|_| ProofError::LengthOverflow)?;
    let mut encoded = Vec::with_capacity(capacity);
    let mut remaining = maximum_bytes
        .checked_add(1)
        .ok_or(ProofError::LengthOverflow)?;
    let mut buffer = vec![0_u8; PROOF_READ_BUFFER_BYTES];
    while remaining > 0 {
        check_deadline()?;
        let read_length = usize::try_from(remaining.min(PROOF_READ_BUFFER_BYTES as u64))
            .map_err(|_| ProofError::LengthOverflow)?;
        let read = file.read(&mut buffer[..read_length])?;
        check_deadline()?;
        if read == 0 {
            break;
        }
        encoded.extend_from_slice(&buffer[..read]);
        remaining = remaining
            .checked_sub(u64::try_from(read).map_err(|_| ProofError::LengthOverflow)?)
            .ok_or(ProofError::LengthOverflow)?;
    }

    let final_metadata = file.metadata()?;
    check_deadline()?;
    ensure_regular_proof_file(&final_metadata)?;
    let actual = u64::try_from(encoded.len()).map_err(|_| ProofError::LengthOverflow)?;
    let observed = actual.max(final_metadata.len());
    if observed > maximum_bytes {
        return Err(ProofError::ProofLimitExceeded {
            actual: observed,
            maximum: maximum_bytes,
        });
    }
    if actual != initial_length || final_metadata.len() != initial_length {
        return Err(ProofError::Invalid {
            reason: "proof changed while being read",
        });
    }
    Ok(encoded)
}

fn ensure_regular_proof_file(metadata: &Metadata) -> Result<(), ProofError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(ProofError::Invalid {
            reason: "proof path is not a regular file",
        })
    }
}

fn remaining_timeout(
    started: Instant,
    limits: &VerificationLimits,
) -> Result<Duration, ProofError> {
    let remaining = limits
        .timeout
        .checked_sub(started.elapsed())
        .ok_or(ProofError::TimedOut)?;
    if remaining.is_zero() {
        Err(ProofError::TimedOut)
    } else {
        Ok(remaining)
    }
}

fn check_timeout(started: Instant, limits: &VerificationLimits) -> Result<(), ProofError> {
    if started.elapsed() >= limits.timeout {
        Err(ProofError::TimedOut)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, error::Error, fs, io::Write as _, path::PathBuf};

    use hyphae_query::{ExecutionLimits, Filter, Query, Record, Value};
    use uuid::Uuid;

    use super::{
        PROOF_READ_BUFFER_BYTES, VerificationLimits, read_open_result_proof, read_result_proof,
        verify_result_proof, write_result_proof,
    };
    use crate::{HyphaeEngine, MAX_RESULT_PROOF_BYTES, ProofError, ProvenResult};

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn create() -> Result<Self, Box<dyn Error>> {
            let path = std::env::temp_dir()
                .join(format!("hyphae-proof-rehashed-tamper-{}", Uuid::now_v7()));
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
    fn result_proof_reader_enforces_the_canonical_hard_limit() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let proof_path = temporary.path.join("oversized.hyproof");
        let file = fs::File::create(&proof_path)?;
        file.set_len(MAX_RESULT_PROOF_BYTES + 1)?;
        drop(file);

        assert!(matches!(
            read_result_proof(&proof_path, u64::MAX),
            Err(ProofError::ProofLimitExceeded {
                actual,
                maximum: MAX_RESULT_PROOF_BYTES,
            }) if actual == MAX_RESULT_PROOF_BYTES + 1
        ));
        Ok(())
    }

    #[test]
    fn result_proof_reader_detects_same_handle_growth() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let proof_path = temporary.path.join("growing.hyproof");
        fs::write(&proof_path, b"initial")?;
        let file = fs::File::open(&proof_path)?;
        let initial_metadata = file.metadata()?;
        let mut writer = fs::OpenOptions::new().append(true).open(&proof_path)?;
        writer.write_all(b"-growth")?;
        writer.sync_all()?;
        drop(writer);

        let mut no_deadline = || Ok(());
        assert!(matches!(
            read_open_result_proof(file, &initial_metadata, 1024, &mut no_deadline),
            Err(ProofError::Invalid {
                reason: "proof changed while being read",
            })
        ));
        Ok(())
    }

    #[test]
    fn result_proof_reader_checks_deadline_between_chunks() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let proof_path = temporary.path.join("timed.hyproof");
        fs::write(&proof_path, vec![0_u8; PROOF_READ_BUFFER_BYTES * 2])?;
        let file = fs::File::open(&proof_path)?;
        let initial_metadata = file.metadata()?;
        let mut checks = 0_u8;
        let mut deadline = || {
            checks += 1;
            if checks == 2 {
                Err(ProofError::TimedOut)
            } else {
                Ok(())
            }
        };

        assert!(matches!(
            read_open_result_proof(
                file,
                &initial_metadata,
                MAX_RESULT_PROOF_BYTES,
                &mut deadline,
            ),
            Err(ProofError::TimedOut)
        ));
        assert_eq!(checks, 2);
        Ok(())
    }

    #[test]
    fn self_consistently_rehashed_result_edits_are_rejected() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut opened = HyphaeEngine::open(temporary.path.join("data"))?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[record(b"a", 1), record(b"b", 2), record(b"c", 3)],
        )?;
        let artifact = opened.engine.query_with_proof(
            &Query {
                filter: Filter::MatchAll,
                sort: Vec::new(),
                cursor: None,
                limit: 3,
                aggregation: None,
            },
            &ExecutionLimits::default(),
        )?;

        for (name, mutation) in [
            ("delete", 1_u8),
            ("insert", 2_u8),
            ("reorder", 3_u8),
            ("edit", 4_u8),
        ] {
            let mut forged = artifact.proof.clone();
            let ProvenResult::Query(result) = &mut forged.result else {
                return Err(ProofError::OperationResultMismatch.into());
            };
            match mutation {
                1 => {
                    result.rows.remove(0);
                }
                2 => result.rows.push(result.rows[0].clone()),
                3 => result.rows.swap(0, 1),
                4 => result.rows[0].value = Value::Integer(999),
                _ => return Err(ProofError::OperationResultMismatch.into()),
            }
            let proof_path = temporary.path.join(format!("{name}.hyproof"));
            write_result_proof(&proof_path, &forged)?;
            assert!(matches!(
                verify_result_proof(
                    &proof_path,
                    &artifact.snapshot.path,
                    artifact.proof.anchor_digest(),
                    &VerificationLimits::default(),
                ),
                Err(ProofError::ReexecutionMismatch)
            ));
        }
        Ok(())
    }

    fn record(key: &[u8], score: i64) -> Record {
        Record::new(
            key,
            Value::Object(BTreeMap::from([(
                "score".to_owned(),
                Value::Integer(score),
            )])),
        )
    }
}
