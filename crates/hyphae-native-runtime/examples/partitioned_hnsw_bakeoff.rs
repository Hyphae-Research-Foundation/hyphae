// SPDX-License-Identifier: Apache-2.0

//! P4 process-local deterministic HNSW bulk-build bakeoff.

use std::{
    collections::BTreeSet,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_ann::{
    HnswConfig, HnswIndex, Metric, PartitionedHnswIndex, SearchOptions, VectorIndexDefinition,
};
use hyphae_native_runtime::{
    HardwareProfile, NativeDatabase, NativeExecutionPool, NativeGovernorPolicy,
    NativePartitionedHnswBuildReceipt, NativeResourceGovernor, Vector, VectorRecord,
};
use hyphae_native_types::{Csn, ObjectId};
use serde_json::json;

const GENERATOR: &str = "hyphae-partitioned-hnsw-bakeoff-v1";

struct BakeoffArguments {
    source_commit: String,
    vector_count: usize,
    dimension: u16,
    partition_count: usize,
    selected_partitions: usize,
    query_count: usize,
    k: usize,
    hardware_probe_path: PathBuf,
    policy_path: PathBuf,
}

impl BakeoffArguments {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut arguments = std::env::args().skip(1);
        let source_commit = arguments
            .next()
            .ok_or("partitioned HNSW bakeoff requires source commit")?;
        let vector_count = positive("vector count", arguments.next())?;
        let dimension = u16::try_from(positive("dimension", arguments.next())?)?;
        let partition_count = positive("partition count", arguments.next())?;
        let selected_partitions = positive("selected partition count", arguments.next())?;
        let query_count = positive("query count", arguments.next())?;
        let k = positive("k", arguments.next())?;
        let hardware_probe_path = PathBuf::from(
            arguments
                .next()
                .ok_or("partitioned HNSW bakeoff requires hardware probe path")?,
        );
        let policy_path = PathBuf::from(
            arguments
                .next()
                .ok_or("partitioned HNSW bakeoff requires governor policy")?,
        );
        if arguments.next().is_some() {
            return Err("partitioned HNSW bakeoff received unexpected arguments".into());
        }
        if query_count > vector_count || k > vector_count || selected_partitions > partition_count {
            return Err(
                "partitioned HNSW bakeoff queries, k, or selected partitions exceed bounds".into(),
            );
        }
        Ok(Self {
            source_commit,
            vector_count,
            dimension,
            partition_count,
            selected_partitions,
            query_count,
            k,
            hardware_probe_path,
            policy_path,
        })
    }
}

struct QualityEvidence {
    ef_search: usize,
    single_recall_ppm: u64,
    partitioned_recall_ppm: u64,
    selected_recall_ppm: u64,
    minimum_single_ppm: u64,
    minimum_partitioned_ppm: u64,
    minimum_selected_ppm: u64,
    single_query_nanos: u64,
    partitioned_query_nanos: u64,
    selected_query_nanos: u64,
}

struct QueryBaseline {
    query: Vector,
    exact: BTreeSet<ObjectId>,
    single_recalled: usize,
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-partitioned-hnsw-{}-{nonce}",
            std::process::id()
        ));
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn positive(name: &str, value: Option<String>) -> Result<usize, Box<dyn std::error::Error>> {
    let parsed = value
        .ok_or_else(|| format!("partitioned HNSW bakeoff requires {name}"))?
        .parse::<usize>()?;
    if parsed == 0 {
        return Err(format!("partitioned HNSW bakeoff {name} must be positive").into());
    }
    Ok(parsed)
}

fn generated_vector(
    sequence: usize,
    dimension: usize,
) -> Result<Vector, Box<dyn std::error::Error>> {
    let values = (0..dimension)
        .map(|component| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(GENERATOR.as_bytes());
            hasher.update(&u64::try_from(sequence).unwrap_or(u64::MAX).to_le_bytes());
            hasher.update(&u64::try_from(component).unwrap_or(u64::MAX).to_le_bytes());
            let digest = hasher.finalize();
            let mut bytes = [0_u8; 2];
            bytes.copy_from_slice(&digest.as_bytes()[..2]);
            let unit = f32::from(u16::from_le_bytes(bytes)) / f32::from(u16::MAX);
            unit.mul_add(2.0, -1.0)
        })
        .collect::<Vec<_>>();
    Ok(Vector::new(values)?)
}

fn records(
    count: usize,
    dimension: usize,
) -> Result<Vec<VectorRecord>, Box<dyn std::error::Error>> {
    (0..count)
        .map(|sequence| {
            Ok(VectorRecord {
                object_id: ObjectId::new(u128::try_from(sequence)?.saturating_add(1))?,
                creating_csn: Csn::new(u64::try_from(sequence)?.saturating_add(1))?,
                vector: generated_vector(sequence, dimension)?,
            })
        })
        .collect()
}

fn dataset_digest(corpus: &[VectorRecord]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(GENERATOR.as_bytes());
    for record in corpus {
        hasher.update(&record.object_id.get().to_be_bytes());
        for value in record.vector.values() {
            hasher.update(&value.to_bits().to_le_bytes());
        }
    }
    hasher.finalize()
}

fn index_definition(dimension: u16) -> Result<VectorIndexDefinition, Box<dyn std::error::Error>> {
    Ok(VectorIndexDefinition::new(
        ObjectId::new(1)?,
        dimension,
        Metric::SquaredL2,
        HnswConfig::new(16, 128, 64, 256, 0x4859_5048_4145_5034)?,
    )?)
}

fn recall(exact: &BTreeSet<ObjectId>, observed: &[hyphae_native_runtime::VectorHit]) -> usize {
    observed
        .iter()
        .filter(|hit| exact.contains(&hit.object_id))
        .count()
}

fn build_query_baselines(
    single: &HnswIndex,
    queries: Vec<Vector>,
    k: usize,
) -> Result<(Vec<QueryBaseline>, u64), Box<dyn std::error::Error>> {
    let ef_search = k.saturating_mul(8).max(k).min(256);
    let options = SearchOptions::new(k, ef_search, Some(ef_search))?;
    let started = Instant::now();
    let baselines = queries
        .into_iter()
        .map(|query| {
            let exact = single
                .search_exact(black_box(&query), k)?
                .into_iter()
                .map(|hit| hit.object_id)
                .collect::<BTreeSet<_>>();
            let single_recalled = recall(&exact, &single.search(black_box(&query), options)?.hits);
            Ok(QueryBaseline {
                query,
                exact,
                single_recalled,
            })
        })
        .collect::<Result<Vec<_>, hyphae_native_ann::AnnError>>()?;
    Ok((baselines, u64::try_from(started.elapsed().as_nanos())?))
}

fn measure_quality(
    partitioned: &PartitionedHnswIndex,
    baselines: &[QueryBaseline],
    k: usize,
    maximum_partitions: usize,
    single_query_nanos: u64,
) -> Result<QualityEvidence, Box<dyn std::error::Error>> {
    let ef_search = k.saturating_mul(8).max(k).min(256);
    let options = SearchOptions::new(k, ef_search, Some(ef_search))?;
    let mut single_recalled = 0_usize;
    let mut partitioned_recalled = 0_usize;
    let mut selected_recalled = 0_usize;
    let mut expected = 0_usize;
    let mut minimum_single_ppm = 1_000_000_u64;
    let mut minimum_partitioned_ppm = 1_000_000_u64;
    let mut minimum_selected_ppm = 1_000_000_u64;
    let mut partitioned_query_nanos = 0_u128;
    let mut selected_query_nanos = 0_u128;
    for baseline in baselines {
        let partitioned_started = Instant::now();
        let partitioned_exact = partitioned
            .search_exact(black_box(&baseline.query), k)?
            .into_iter()
            .map(|hit| hit.object_id)
            .collect::<BTreeSet<_>>();
        if partitioned_exact != baseline.exact {
            return Err("partitioned exact oracle differs from canonical single exact".into());
        }
        let partitioned_hits = recall(
            &baseline.exact,
            &partitioned
                .search(black_box(&baseline.query), options)?
                .hits,
        );
        partitioned_query_nanos = partitioned_query_nanos
            .checked_add(partitioned_started.elapsed().as_nanos())
            .ok_or("partitioned query clock overflowed")?;
        let selected_started = Instant::now();
        let selected_hits = recall(
            &baseline.exact,
            &partitioned
                .search_selected(black_box(&baseline.query), options, maximum_partitions)?
                .result
                .hits,
        );
        selected_query_nanos = selected_query_nanos
            .checked_add(selected_started.elapsed().as_nanos())
            .ok_or("selected partition query clock overflowed")?;
        single_recalled = single_recalled.saturating_add(baseline.single_recalled);
        partitioned_recalled = partitioned_recalled.saturating_add(partitioned_hits);
        selected_recalled = selected_recalled.saturating_add(selected_hits);
        expected = expected.saturating_add(baseline.exact.len());
        let denominator = u64::try_from(baseline.exact.len())?;
        minimum_single_ppm = minimum_single_ppm
            .min(u64::try_from(baseline.single_recalled)?.saturating_mul(1_000_000) / denominator);
        minimum_partitioned_ppm = minimum_partitioned_ppm
            .min(u64::try_from(partitioned_hits)?.saturating_mul(1_000_000) / denominator);
        minimum_selected_ppm = minimum_selected_ppm
            .min(u64::try_from(selected_hits)?.saturating_mul(1_000_000) / denominator);
    }
    let expected = u64::try_from(expected)?;
    Ok(QualityEvidence {
        ef_search,
        single_recall_ppm: u64::try_from(single_recalled)?.saturating_mul(1_000_000) / expected,
        partitioned_recall_ppm: u64::try_from(partitioned_recalled)?.saturating_mul(1_000_000)
            / expected,
        selected_recall_ppm: u64::try_from(selected_recalled)?.saturating_mul(1_000_000) / expected,
        minimum_single_ppm,
        minimum_partitioned_ppm,
        minimum_selected_ppm,
        single_query_nanos,
        partitioned_query_nanos: u64::try_from(partitioned_query_nanos)?,
        selected_query_nanos: u64::try_from(selected_query_nanos)?,
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn dataset_json(
    arguments: &BakeoffArguments,
    digest: blake3::Hash,
    elapsed: Duration,
) -> Result<serde_json::Value, std::num::TryFromIntError> {
    Ok(json!({
        "generator": GENERATOR,
        "digest": digest.to_hex().to_string(),
        "vectors": arguments.vector_count,
        "dimension": arguments.dimension,
        "metric": "squared-l2",
        "corpus_construction_nanos": u64::try_from(elapsed.as_nanos())?,
    }))
}

fn build_json(
    arguments: &BakeoffArguments,
    serial_hnsw_time: Duration,
    serial_partitioned_time: Duration,
    parallel: &NativePartitionedHnswBuildReceipt,
    single_build_identity: [u8; 32],
) -> Result<serde_json::Value, std::num::TryFromIntError> {
    Ok(json!({
        "requested_partitions": arguments.partition_count,
        "effective_partitions": parallel.planned_partitions,
        "serial_hnsw_nanos": u64::try_from(serial_hnsw_time.as_nanos())?,
        "serial_partitioned_nanos": u64::try_from(serial_partitioned_time.as_nanos())?,
        "parallel_partitioned_nanos": u64::try_from(parallel.total_time.as_nanos())?,
        "planned_compute_threads": parallel.planned_compute_threads,
        "planned_memory_bytes": parallel.planned_memory_bytes,
        "worker_batches": parallel.worker_batches,
        "single_build_identity": encode_hex(&single_build_identity),
        "partitioned_build_identity": encode_hex(&parallel.index.build_identity()),
        "deterministic_across_serial_and_parallel": true,
        "durable_publication": false,
    }))
}

fn quality_json(arguments: &BakeoffArguments, quality: &QualityEvidence) -> serde_json::Value {
    json!({
        "queries": arguments.query_count,
        "k": arguments.k,
        "ef_search": quality.ef_search,
        "selected_partitions": arguments.selected_partitions,
        "single_hnsw_recall_ppm": quality.single_recall_ppm,
        "partitioned_hnsw_recall_ppm": quality.partitioned_recall_ppm,
        "selected_partition_recall_ppm": quality.selected_recall_ppm,
        "minimum_single_query_recall_ppm": quality.minimum_single_ppm,
        "minimum_partitioned_query_recall_ppm": quality.minimum_partitioned_ppm,
        "minimum_selected_query_recall_ppm": quality.minimum_selected_ppm,
        "single_query_batch_nanos": quality.single_query_nanos,
        "partitioned_query_batch_nanos": quality.partitioned_query_nanos,
        "selected_query_batch_nanos": quality.selected_query_nanos,
        "oracle": "partitioned-exact-flat-canonical-top-k-v1",
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = BakeoffArguments::parse()?;
    let profile = HardwareProfile::discover(&arguments.hardware_probe_path)?;
    let policy: NativeGovernorPolicy = serde_json::from_slice(&fs::read(&arguments.policy_path)?)?;
    let definition = index_definition(arguments.dimension)?;
    let corpus_started = Instant::now();
    let corpus = records(arguments.vector_count, usize::from(arguments.dimension))?;
    let corpus_time = corpus_started.elapsed();
    let queries = corpus
        .iter()
        .take(arguments.query_count)
        .map(|record| record.vector.clone())
        .collect::<Vec<_>>();
    let dataset_digest = dataset_digest(&corpus);

    let serial_hnsw_started = Instant::now();
    let serial_hnsw = HnswIndex::build(definition, corpus)?;
    let serial_hnsw_time = serial_hnsw_started.elapsed();
    let single_build_identity = serial_hnsw.build_identity();
    let (baselines, single_query_nanos) =
        build_query_baselines(&serial_hnsw, queries, arguments.k)?;
    drop(serial_hnsw);

    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let serial_partitioned = database.build_partitioned_hnsw_experimental(
        definition,
        records(arguments.vector_count, usize::from(arguments.dimension))?,
        arguments.partition_count,
    )?;
    let serial_partitioned_time = serial_partitioned.total_time;
    let serial_partitioned_identity = serial_partitioned.index.build_identity();
    drop(serial_partitioned);
    let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
    let execution_pool = Arc::new(NativeExecutionPool::new(&profile, &policy)?);
    database.set_resource_governor_with_execution_pool(
        Arc::clone(&governor),
        Arc::clone(&execution_pool),
        std::time::Duration::ZERO,
    )?;
    let parallel_partitioned = database.build_partitioned_hnsw_experimental(
        definition,
        records(arguments.vector_count, usize::from(arguments.dimension))?,
        arguments.partition_count,
    )?;
    if serial_partitioned_identity != parallel_partitioned.index.build_identity() {
        return Err("partitioned HNSW identities differ across worker counts".into());
    }
    let quality = measure_quality(
        &parallel_partitioned.index,
        &baselines,
        arguments.k,
        arguments.selected_partitions,
        single_query_nanos,
    )?;
    let output = json!({
        "schema": "hyphae-native-vector-bulk-bakeoff-v1",
        "status": "diagnostic",
        "source_commit": arguments.source_commit,
        "hardware_fingerprint": profile.fingerprint,
        "governor_calibration_cache_key": policy.calibration_cache_key,
        "dataset": dataset_json(&arguments, dataset_digest, corpus_time)?,
        "build": build_json(
            &arguments,
            serial_hnsw_time,
            serial_partitioned_time,
            &parallel_partitioned,
            single_build_identity,
        )?,
        "quality": quality_json(&arguments, &quality),
        "missing_gate_evidence": [
            "peak-rss",
            "write-amplification",
            "checkpoint-restart",
            "durable-publication-and-reopen",
            "update-delete-consolidation",
            "accepted-corpus-matrix"
        ],
        "claims": [],
        "closure_declared": false
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
