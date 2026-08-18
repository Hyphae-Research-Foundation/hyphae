// SPDX-License-Identifier: Apache-2.0

//! Produces local Gate 9 performance evidence for durable retrieval.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_engine::{
    HyphaeEngine, RetrievalVerificationLimits, encode_document, verify_exact_retrieval_proof,
    verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof, write_exact_retrieval_proof,
    write_hybrid_retrieval_proof, write_lexical_retrieval_proof,
};
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{
    DurableVectorRecord, ExactRetrievalLimits, ExactRetrievalOutcome, ExactRetrievalRequest,
    HybridOutcome, HybridRequest, LexicalField, LexicalIndexDefinition, LexicalLimits,
    LexicalOutcome, LexicalRequest, fuse_hybrid, retrieve_exact, retrieve_lexical,
};
use hyphae_storage::{AppendOutcome, CompactionOutcome};
use serde_json::{Value as JsonValue, json};
use uuid::Uuid;

const BATCH_SIZE: usize = 256;
const SCORE_MINIMUM: i64 = -1_000_000_000;

#[derive(Clone, Copy, Debug)]
struct Scenario {
    corpus: usize,
    dimensions: usize,
    top_k: usize,
}

type BenchmarkCorpus = (Vec<Record>, Vec<(Vec<u8>, Q15Vector)>, u64);

fn main() -> Result<(), Box<dyn Error>> {
    let (iterations, scenarios) = parse_arguments()?;
    let root = std::env::temp_dir().join(format!(
        "hyphae-retrieval-benchmark-{}-{}",
        std::process::id(),
        Uuid::now_v7()
    ));
    fs::create_dir_all(&root)?;
    let result = run_matrix(&root, iterations, &scenarios);
    let _ignored = fs::remove_dir_all(&root);
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

fn parse_arguments() -> Result<(usize, Vec<Scenario>), Box<dyn Error>> {
    let mut iterations = 7;
    let mut scenarios = Vec::new();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--iterations" => {
                index += 1;
                iterations = arguments
                    .get(index)
                    .ok_or("missing --iterations value")?
                    .parse()?;
            }
            "--scenario" => {
                index += 1;
                scenarios.push(parse_scenario(
                    arguments.get(index).ok_or("missing --scenario value")?,
                )?);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        index += 1;
    }
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    if scenarios.is_empty() {
        scenarios = vec![
            Scenario {
                corpus: 256,
                dimensions: 32,
                top_k: 5,
            },
            Scenario {
                corpus: 1_024,
                dimensions: 128,
                top_k: 10,
            },
            Scenario {
                corpus: 4_096,
                dimensions: 256,
                top_k: 20,
            },
        ];
    }
    Ok((iterations, scenarios))
}

fn parse_scenario(value: &str) -> Result<Scenario, Box<dyn Error>> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err("scenario must be CORPUS:DIMENSIONS:TOP_K".into());
    }
    let scenario = Scenario {
        corpus: parts[0].parse()?,
        dimensions: parts[1].parse()?,
        top_k: parts[2].parse()?,
    };
    if scenario.corpus == 0
        || scenario.dimensions == 0
        || scenario.dimensions > usize::from(u16::MAX)
        || scenario.top_k == 0
        || scenario.top_k > scenario.corpus
    {
        return Err("scenario values are outside supported bounds".into());
    }
    Ok(scenario)
}

fn run_matrix(
    root: &Path,
    iterations: usize,
    scenarios: &[Scenario],
) -> Result<JsonValue, Box<dyn Error>> {
    let mut reports = Vec::with_capacity(scenarios.len());
    for (index, scenario) in scenarios.iter().copied().enumerate() {
        reports.push(run_scenario(
            &root.join(format!("scenario-{index}")),
            scenario,
            iterations,
        )?);
    }
    Ok(json!({
        "schema": "hyphae-retrieval-benchmark-v2",
        "engine_version": env!("CARGO_PKG_VERSION"),
        "disk_format_version": hyphae_core::DISK_FORMAT_VERSION,
        "iterations": iterations,
        "timing_clock": "std::time::Instant",
        "scenarios": reports
    }))
}

#[allow(clippy::too_many_lines)]
fn run_scenario(
    root: &Path,
    scenario: Scenario,
    iterations: usize,
) -> Result<JsonValue, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let data = root.join("data");
    let backup = root.join("backup");
    let restored = root.join("restored");
    let proofs = root.join("proofs");
    fs::create_dir_all(&proofs)?;

    let (records, vectors, logical_payload_bytes) = make_corpus(scenario)?;
    let vector_space = VectorSpaceName::new("semantic")?;
    let lexical_index = VectorSpaceName::new("content")?;
    let lexical_definition = LexicalIndexDefinition::new(
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
    )?;
    let setup_started = Instant::now();
    let mut opened = HyphaeEngine::open(&data)?;
    committed(opened.engine.define_vector_space(
        Uuid::now_v7(),
        VectorSpaceDefinition::cosine(vector_space.clone(), u16::try_from(scenario.dimensions)?)?,
    )?)?;
    committed(
        opened
            .engine
            .define_lexical_index(Uuid::now_v7(), lexical_definition.clone())?,
    )?;
    let record_ingest_started = Instant::now();
    for batch in records.chunks(BATCH_SIZE) {
        committed(opened.engine.put_records(Uuid::now_v7(), batch)?)?;
    }
    let record_ingest_nanos = elapsed_nanos(record_ingest_started);
    let vector_ingest_started = Instant::now();
    for batch in vectors.chunks(BATCH_SIZE) {
        committed(
            opened
                .engine
                .put_vectors(Uuid::now_v7(), &vector_space, batch)?,
        )?;
    }
    let vector_ingest_nanos = elapsed_nanos(vector_ingest_started);
    let setup_nanos = elapsed_nanos(setup_started);
    let pre_snapshot_data_bytes = directory_bytes(&data)?;
    drop(opened);

    let reopen_started = Instant::now();
    let reopened = HyphaeEngine::open(&data)?;
    let reopen_nanos = elapsed_nanos(reopen_started);
    let reopen_replayed_transactions = reopened.recovery.replayed_transactions;
    drop(reopened);

    fs::remove_file(data.join("indexes/primary.redb"))?;
    let replay_started = Instant::now();
    let rebuilt = HyphaeEngine::open(&data)?;
    let replay_nanos = elapsed_nanos(replay_started);
    let replayed_transactions = rebuilt.recovery.replayed_transactions;
    let mut engine = rebuilt.engine;

    let exact_request = ExactRetrievalRequest {
        vector_space: vector_space.clone(),
        query: vectors[0].1.clone(),
        limit: scenario.top_k,
        minimum_score_nanos: SCORE_MINIMUM,
        minimum_margin_nanos: 0,
    };
    let exact_limits = ExactRetrievalLimits {
        max_candidates: u64::try_from(scenario.corpus)?,
        max_candidate_bytes: candidate_byte_budget(scenario)?,
        max_returned: scenario.top_k,
        timeout: Duration::from_secs(60),
    };
    let lexical_request = LexicalRequest {
        index: lexical_index,
        query: "durable memory".to_owned(),
        limit: scenario.top_k,
    };
    let lexical_limits = LexicalLimits {
        max_documents: u64::try_from(scenario.corpus)?,
        max_tokens: u64::try_from(scenario.corpus.saturating_mul(16))?,
        max_candidates: u64::try_from(scenario.corpus)?,
        max_returned: scenario.top_k,
        timeout: Duration::from_secs(60),
    };
    let hybrid_request = HybridRequest {
        lexical_weight: 1,
        vector_weight: 1,
        limit: scenario.top_k,
    };

    let durable_candidates = vectors
        .iter()
        .map(|(key, vector)| DurableVectorRecord {
            key: key.clone(),
            vector: vector.clone(),
        })
        .collect::<Vec<_>>();
    let exact_reference = retrieve_exact(&durable_candidates, &exact_request, &exact_limits)?;
    let lexical_reference = retrieve_lexical(
        &records,
        &lexical_definition,
        &lexical_request,
        &lexical_limits,
    )?;
    let hybrid_reference = fuse_hybrid(&lexical_reference, &exact_reference, &hybrid_request)?;
    let exact_engine = engine.retrieve_exact(&exact_request, &exact_limits)?;
    let lexical_engine = engine.retrieve_lexical(&lexical_request, &lexical_limits)?;
    let hybrid_engine = engine.retrieve_hybrid(
        &lexical_request,
        &lexical_limits,
        &exact_request,
        &exact_limits,
        &hybrid_request,
    )?;
    let exact_recall_at_k_micros =
        recall_at_k_micros(&exact_engine, &exact_reference, scenario.top_k)?;
    if exact_recall_at_k_micros != 1_000_000 {
        return Err("durable exact retrieval diverged from the exhaustive reference".into());
    }
    if lexical_engine != lexical_reference {
        return Err("durable lexical retrieval diverged from the BM25F reference".into());
    }
    if hybrid_engine != hybrid_reference {
        return Err("durable hybrid retrieval diverged from the RRF reference".into());
    }

    let exact_latency = measure(iterations, || {
        let outcome = engine.retrieve_exact(&exact_request, &exact_limits)?;
        assert_exact_count(&outcome, scenario.top_k)
    })?;
    let lexical_latency = measure(iterations, || {
        let outcome = engine.retrieve_lexical(&lexical_request, &lexical_limits)?;
        assert_lexical_count(&outcome, scenario.top_k)
    })?;
    let hybrid_latency = measure(iterations, || {
        let outcome = engine.retrieve_hybrid(
            &lexical_request,
            &lexical_limits,
            &exact_request,
            &exact_limits,
            &hybrid_request,
        )?;
        assert_hybrid_count(&outcome, scenario.top_k)
    })?;

    let exact_generation = measure(iterations, || {
        let artifact = engine.retrieve_exact_with_proof(&exact_request, &exact_limits)?;
        let _encoded = artifact.proof.to_bytes()?;
        Ok(())
    })?;
    let lexical_generation = measure(iterations, || {
        let artifact = engine.retrieve_lexical_with_proof(&lexical_request, &lexical_limits)?;
        let _encoded = artifact.proof.to_bytes()?;
        Ok(())
    })?;
    let hybrid_generation = measure(iterations, || {
        let artifact = engine.retrieve_hybrid_with_proof(
            &lexical_request,
            &lexical_limits,
            &exact_request,
            &exact_limits,
            &hybrid_request,
        )?;
        let _encoded = artifact.proof.to_bytes()?;
        Ok(())
    })?;

    let exact_artifact = engine.retrieve_exact_with_proof(&exact_request, &exact_limits)?;
    let lexical_artifact = engine.retrieve_lexical_with_proof(&lexical_request, &lexical_limits)?;
    let hybrid_artifact = engine.retrieve_hybrid_with_proof(
        &lexical_request,
        &lexical_limits,
        &exact_request,
        &exact_limits,
        &hybrid_request,
    )?;
    let exact_proof_path = proofs.join("exact.hyproof");
    let lexical_proof_path = proofs.join("lexical.hyproof");
    let hybrid_proof_path = proofs.join("hybrid.hyproof");
    write_exact_retrieval_proof(&exact_proof_path, &exact_artifact.proof)?;
    write_lexical_retrieval_proof(&lexical_proof_path, &lexical_artifact.proof)?;
    write_hybrid_retrieval_proof(&hybrid_proof_path, &hybrid_artifact.proof)?;
    let verification_limits = verification_limits(scenario)?;
    let exact_verification = measure(iterations, || {
        verify_exact_retrieval_proof(
            &exact_proof_path,
            &exact_artifact.snapshot.path,
            exact_artifact.proof.anchor_digest(),
            &verification_limits,
        )?;
        Ok(())
    })?;
    let lexical_verification = measure(iterations, || {
        verify_lexical_retrieval_proof(
            &lexical_proof_path,
            &lexical_artifact.snapshot.path,
            lexical_artifact.proof.anchor_digest(),
            &verification_limits,
        )?;
        Ok(())
    })?;
    let hybrid_verification = measure(iterations, || {
        verify_hybrid_retrieval_proof(
            &hybrid_proof_path,
            &hybrid_artifact.snapshot.path,
            hybrid_artifact.proof.anchor_digest(),
            &verification_limits,
        )?;
        Ok(())
    })?;

    let snapshot_started = Instant::now();
    let snapshot = engine.snapshot()?;
    let snapshot_nanos = elapsed_nanos(snapshot_started);
    let compact_started = Instant::now();
    let compacted = engine.compact()?;
    let compact_nanos = elapsed_nanos(compact_started);
    let compact_generation = match compacted {
        CompactionOutcome::Compacted(report) => report.generation,
        CompactionOutcome::NoChanges { .. } => 0,
    };
    let backup_started = Instant::now();
    let backup_info = engine.backup(&backup)?;
    let backup_nanos = elapsed_nanos(backup_started);
    drop(engine);

    let compact_reopen_started = Instant::now();
    let compact_reopened = HyphaeEngine::open(&data)?;
    let compact_reopen_nanos = elapsed_nanos(compact_reopen_started);
    drop(compact_reopened);

    let restore_started = Instant::now();
    let restore_info = HyphaeEngine::restore_backup(&backup, &restored)?;
    let restore_nanos = elapsed_nanos(restore_started);
    let restored_open_started = Instant::now();
    let mut restored_engine = HyphaeEngine::open(&restored)?.engine;
    let restored_open_nanos = elapsed_nanos(restored_open_started);
    assert_exact_count(
        &restored_engine.retrieve_exact(&exact_request, &exact_limits)?,
        scenario.top_k,
    )?;
    assert_lexical_count(
        &restored_engine.retrieve_lexical(&lexical_request, &lexical_limits)?,
        scenario.top_k,
    )?;

    let update_record_started = Instant::now();
    let updated_record = Record::new(
        records[0].key.clone(),
        Value::Object(BTreeMap::from([
            (
                "body".to_owned(),
                Value::String("durable memory updated benchmark record".to_owned()),
            ),
            (
                "title".to_owned(),
                Value::String("Hyphae updated item".to_owned()),
            ),
        ])),
    );
    committed(restored_engine.put_records(Uuid::now_v7(), std::slice::from_ref(&updated_record))?)?;
    let update_record_nanos = elapsed_nanos(update_record_started);

    let mut updated_elements = vectors[0].1.as_slice().to_vec();
    updated_elements[0] = if updated_elements[0] == i16::MAX {
        i16::MAX - 1
    } else {
        updated_elements[0].saturating_add(1)
    };
    let updated_vector = Q15Vector::new(updated_elements)?;
    let update_vector_started = Instant::now();
    committed(restored_engine.put_vectors(
        Uuid::now_v7(),
        &vector_space,
        &[(vectors[0].0.clone(), updated_vector)],
    )?)?;
    let update_vector_nanos = elapsed_nanos(update_vector_started);

    let deleted_key = records
        .last()
        .ok_or("benchmark corpus unexpectedly empty")?
        .key
        .as_slice();
    let delete_record_started = Instant::now();
    committed(restored_engine.delete_records(Uuid::now_v7(), &[deleted_key])?)?;
    let delete_record_nanos = elapsed_nanos(delete_record_started);
    let delete_vector_started = Instant::now();
    committed(restored_engine.delete_vectors(Uuid::now_v7(), &vector_space, &[deleted_key])?)?;
    let delete_vector_nanos = elapsed_nanos(delete_vector_started);
    if restored_engine.get_record(&updated_record.key)? != Some(updated_record) {
        return Err("record update was not immediately visible".into());
    }
    if restored_engine.get_record(deleted_key)?.is_some() {
        return Err("record delete was not immediately visible".into());
    }
    assert_deleted_vector_absent(
        &restored_engine.retrieve_exact(&exact_request, &exact_limits)?,
        deleted_key,
        scenario.corpus.saturating_sub(1),
    )?;
    drop(restored_engine);

    let final_data_bytes = directory_bytes(&data)?;
    let backup_bytes = directory_bytes(&backup)?;
    let write_amplification_micros = if logical_payload_bytes == 0 {
        0
    } else {
        u64::try_from(
            u128::from(pre_snapshot_data_bytes)
                .saturating_mul(1_000_000)
                .checked_div(u128::from(logical_payload_bytes))
                .unwrap_or_default(),
        )?
    };

    Ok(json!({
        "parameters": {
            "corpus_size": scenario.corpus,
            "dimensions": scenario.dimensions,
            "top_k": scenario.top_k,
            "dataset": "deterministic_synthetic_mixed_document_vector_v1",
            "lexical_corpus": true,
            "mixed_document_vector_workload": true,
            "near_ties_covered_by_compatibility_fixture": true,
            "empty_space_covered_by_unit_fixture": true,
            "sparse_space_covered_by_unit_fixture": true,
            "updates_and_deletes_exercised": true
        },
        "quality": {
            "exact_reference": "exhaustive canonical scorer over every durable candidate",
            "exact_recall_at_k_micros": exact_recall_at_k_micros,
            "lexical_reference": "hyphae BM25F reference executor",
            "lexical_matches_reference": true,
            "hybrid_reference": "hyphae deterministic RRF reference executor",
            "hybrid_matches_reference": true
        },
        "ingest": {
            "records": scenario.corpus,
            "record_ingest_nanos": record_ingest_nanos,
            "records_per_second": rate_per_second(scenario.corpus, record_ingest_nanos)?,
            "vectors": scenario.corpus,
            "vector_ingest_nanos": vector_ingest_nanos,
            "vectors_per_second": rate_per_second(scenario.corpus, vector_ingest_nanos)?
        },
        "storage": {
            "logical_payload_bytes": logical_payload_bytes,
            "data_bytes_before_snapshot": pre_snapshot_data_bytes,
            "data_bytes_after_compaction": final_data_bytes,
            "backup_bytes": backup_bytes,
            "write_amplification_micros": write_amplification_micros,
            "snapshot_bytes": snapshot.file_bytes,
            "backup_snapshot_bytes": backup_info.snapshot.file_bytes,
            "restored_snapshot_bytes": restore_info.snapshot.file_bytes
        },
        "lifecycle_nanos": {
            "initial_write": setup_nanos,
            "reopen_caught_up": reopen_nanos,
            "reopen_replayed_transactions": reopen_replayed_transactions,
            "rebuild_from_authority": replay_nanos,
            "rebuild_replayed_transactions": replayed_transactions,
            "snapshot": snapshot_nanos,
            "compaction": compact_nanos,
            "compaction_generation": compact_generation,
            "reopen_after_compaction": compact_reopen_nanos,
            "backup": backup_nanos,
            "restore": restore_nanos,
            "open_restored": restored_open_nanos,
            "update_record": update_record_nanos,
            "update_vector": update_vector_nanos,
            "delete_record": delete_record_nanos,
            "delete_vector": delete_vector_nanos
        },
        "retrieval_latency_nanos": {
            "exact": exact_latency,
            "lexical": lexical_latency,
            "hybrid": hybrid_latency
        },
        "proof_generation_nanos": {
            "exact": exact_generation,
            "lexical": lexical_generation,
            "hybrid": hybrid_generation
        },
        "proof_verification_nanos": {
            "exact": exact_verification,
            "lexical": lexical_verification,
            "hybrid": hybrid_verification
        },
        "proof_bytes": {
            "exact": fs::metadata(exact_proof_path)?.len(),
            "lexical": fs::metadata(lexical_proof_path)?.len(),
            "hybrid": fs::metadata(hybrid_proof_path)?.len()
        }
    }))
}

fn make_corpus(scenario: Scenario) -> Result<BenchmarkCorpus, Box<dyn Error>> {
    let mut records = Vec::with_capacity(scenario.corpus);
    let mut vectors = Vec::with_capacity(scenario.corpus);
    let mut logical_payload_bytes = 0_u64;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64
        ^ u64::try_from(scenario.corpus)?
        ^ u64::try_from(scenario.dimensions)?;
    for item in 0..scenario.corpus {
        let key = u64::try_from(item)?.to_be_bytes().to_vec();
        let record = Record::new(
            key.clone(),
            Value::Object(BTreeMap::from([
                (
                    "body".to_owned(),
                    Value::String(format!(
                        "durable memory retrieval corpus item {} group {}",
                        item,
                        item % 31
                    )),
                ),
                (
                    "title".to_owned(),
                    Value::String(format!("Hyphae durable item {item}")),
                ),
            ])),
        );
        let encoded = encode_document(&record.value)?;
        logical_payload_bytes = logical_payload_bytes
            .saturating_add(u64::try_from(key.len().saturating_add(encoded.len()))?);
        records.push(record);

        let mut elements = Vec::with_capacity(scenario.dimensions);
        for _ in 0..scenario.dimensions {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let raw = i32::try_from((state >> 32) % 65_535)?;
            elements.push(i16::try_from(raw - 32_767)?);
        }
        if elements.iter().all(|element| *element == 0) {
            elements[0] = 1;
        }
        logical_payload_bytes = logical_payload_bytes.saturating_add(u64::try_from(
            key.len()
                .saturating_add(scenario.dimensions.saturating_mul(2)),
        )?);
        vectors.push((key, Q15Vector::new(elements)?));
    }
    Ok((records, vectors, logical_payload_bytes))
}

fn verification_limits(scenario: Scenario) -> Result<RetrievalVerificationLimits, Box<dyn Error>> {
    Ok(RetrievalVerificationLimits {
        max_candidates: u64::try_from(scenario.corpus)?,
        max_candidate_bytes: candidate_byte_budget(scenario)?,
        max_returned: scenario.top_k,
        max_documents: u64::try_from(scenario.corpus)?,
        max_tokens: u64::try_from(scenario.corpus.saturating_mul(16))?,
        max_lexical_candidates: u64::try_from(scenario.corpus)?,
        max_lexical_returned: scenario.top_k,
        max_hybrid_returned: scenario.top_k,
        timeout: Duration::from_secs(60),
        ..RetrievalVerificationLimits::default()
    })
}

fn candidate_byte_budget(scenario: Scenario) -> Result<u64, Box<dyn Error>> {
    // Eight-byte keys plus the two-byte encoded dimension precede Q15 elements.
    Ok(u64::try_from(scenario.corpus.saturating_mul(
        scenario.dimensions.saturating_mul(2).saturating_add(10),
    ))?)
}

fn measure(
    iterations: usize,
    mut operation: impl FnMut() -> Result<(), Box<dyn Error>>,
) -> Result<JsonValue, Box<dyn Error>> {
    operation()?;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        operation()?;
        samples.push(elapsed_nanos(started));
    }
    samples.sort_unstable();
    let total = samples.iter().fold(0_u128, |sum, sample| {
        sum.saturating_add(u128::from(*sample))
    });
    let mean = u64::try_from(total / u128::try_from(samples.len())?)?;
    let p50 = samples[(samples.len() - 1) / 2];
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    let p99 = samples[(samples.len() * 99).div_ceil(100) - 1];
    Ok(json!({
        "minimum": samples[0],
        "mean": mean,
        "p50": p50,
        "p95": p95,
        "p99": p99,
        "maximum": samples[samples.len() - 1]
    }))
}

fn rate_per_second(items: usize, nanos: u64) -> Result<u64, Box<dyn Error>> {
    if nanos == 0 {
        return Ok(u64::MAX);
    }
    Ok(u64::try_from(
        u128::try_from(items)?
            .saturating_mul(1_000_000_000)
            .checked_div(u128::from(nanos))
            .unwrap_or_default(),
    )?)
}

fn recall_at_k_micros(
    actual: &ExactRetrievalOutcome,
    reference: &ExactRetrievalOutcome,
    top_k: usize,
) -> Result<u64, Box<dyn Error>> {
    let ExactRetrievalOutcome::Matches {
        matches: actual, ..
    } = actual
    else {
        return Err("durable exact retrieval unexpectedly abstained".into());
    };
    let ExactRetrievalOutcome::Matches {
        matches: reference, ..
    } = reference
    else {
        return Err("exact reference unexpectedly abstained".into());
    };
    let actual_keys = actual
        .iter()
        .take(top_k)
        .map(|matched| matched.key.as_slice())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_keys = reference
        .iter()
        .take(top_k)
        .map(|matched| matched.key.as_slice())
        .collect::<std::collections::BTreeSet<_>>();
    let matches = actual_keys.intersection(&expected_keys).count();
    Ok(u64::try_from(
        u128::try_from(matches)?
            .saturating_mul(1_000_000)
            .checked_div(u128::try_from(top_k)?)
            .unwrap_or_default(),
    )?)
}

fn assert_deleted_vector_absent(
    outcome: &ExactRetrievalOutcome,
    deleted_key: &[u8],
    expected_candidates: usize,
) -> Result<(), Box<dyn Error>> {
    let ExactRetrievalOutcome::Matches {
        matches,
        scanned_candidates,
    } = outcome
    else {
        return Err("exact retrieval after delete unexpectedly abstained".into());
    };
    if matches.iter().any(|matched| matched.key == deleted_key) {
        return Err("deleted vector remained visible".into());
    }
    if *scanned_candidates != u64::try_from(expected_candidates)? {
        return Err("exact retrieval did not observe the post-delete candidate set".into());
    }
    Ok(())
}

fn assert_exact_count(
    outcome: &ExactRetrievalOutcome,
    minimum: usize,
) -> Result<(), Box<dyn Error>> {
    match outcome {
        ExactRetrievalOutcome::Matches { matches, .. } if matches.len() == minimum => Ok(()),
        _ => Err("exact retrieval did not return the requested complete ranking".into()),
    }
}

fn assert_lexical_count(outcome: &LexicalOutcome, minimum: usize) -> Result<(), Box<dyn Error>> {
    match outcome {
        LexicalOutcome::Matches { matches, .. } if matches.len() == minimum => Ok(()),
        _ => Err("lexical retrieval did not return the requested complete ranking".into()),
    }
}

fn assert_hybrid_count(outcome: &HybridOutcome, minimum: usize) -> Result<(), Box<dyn Error>> {
    match outcome {
        HybridOutcome::Matches { matches, .. } if matches.len() == minimum => Ok(()),
        _ => Err("hybrid retrieval did not return the requested complete ranking".into()),
    }
}

fn committed(outcome: AppendOutcome) -> Result<(), Box<dyn Error>> {
    match outcome {
        AppendOutcome::Committed(_) => Ok(()),
        AppendOutcome::Existing(_) => Err("benchmark transaction unexpectedly existed".into()),
    }
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn directory_bytes(root: &Path) -> Result<u64, Box<dyn Error>> {
    let mut total = 0_u64;
    let mut pending = vec![PathBuf::from(root)];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs};

    use uuid::Uuid;

    use super::{Scenario, run_matrix};

    #[test]
    #[ignore = "explicit optimized Gate 9 benchmark"]
    fn write_gate_evidence() -> Result<(), Box<dyn Error>> {
        let output = std::env::var_os("HYPHAE_RETRIEVAL_BENCHMARK_OUTPUT")
            .ok_or("HYPHAE_RETRIEVAL_BENCHMARK_OUTPUT is required")?;
        let iterations = std::env::var("HYPHAE_RETRIEVAL_BENCHMARK_ITERATIONS")
            .unwrap_or_else(|_| "7".to_owned())
            .parse()?;
        let scenarios = std::env::var("HYPHAE_RETRIEVAL_BENCHMARK_SCENARIOS")
            .ok()
            .map_or_else(
                || {
                    Ok::<_, Box<dyn Error>>(vec![
                        Scenario {
                            corpus: 256,
                            dimensions: 32,
                            top_k: 5,
                        },
                        Scenario {
                            corpus: 1_024,
                            dimensions: 128,
                            top_k: 10,
                        },
                        Scenario {
                            corpus: 4_096,
                            dimensions: 256,
                            top_k: 20,
                        },
                    ])
                },
                |encoded| {
                    encoded
                        .split(',')
                        .map(super::parse_scenario)
                        .collect::<Result<Vec<_>, _>>()
                },
            )?;
        let root = std::env::temp_dir().join(format!(
            "hyphae-retrieval-benchmark-test-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
        fs::create_dir_all(&root)?;
        let result = run_matrix(&root, iterations, &scenarios);
        let _ignored = fs::remove_dir_all(&root);
        fs::write(output, serde_json::to_vec_pretty(&result?)?)?;
        Ok(())
    }
}
