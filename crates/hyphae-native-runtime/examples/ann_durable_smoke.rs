// SPDX-License-Identifier: Apache-2.0

//! Deterministic durability, quality, and warm-query observation for native ANN.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt::Write as _,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{AnnSearchOptions, HnswConfig, NativeDatabase, Vector, VectorMetric};
use hyphae_native_types::{DurabilityClass, ObjectId};

const VECTOR_COUNT: u16 = 512;
const DIMENSION: u16 = 32;
const QUERY_COUNT: u16 = 64;
const OBSERVATIONS: u32 = 10_000;
const WARMUP: u32 = 1_000;
const K: usize = 10;
const EF_SEARCH: usize = 128;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-ann-durable-smoke-{}-{timestamp}",
            std::process::id()
        ))))
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

struct Corpus {
    vectors: Vec<Vector>,
    queries: Vec<Vector>,
    digest: [u8; 32],
}

struct LatencySummary {
    p50: f64,
    p95: f64,
    p99: f64,
    p999: f64,
    maximum: f64,
    throughput_per_second: f64,
}

struct Receipt<'a> {
    source_commit: &'a str,
    environment: &'a str,
    dataset_digest: [u8; 32],
    build_identity: [u8; 32],
    commit_csn: u64,
    data_directory_bytes: u64,
    private_seed_duration: Duration,
    strict_commit_duration: Duration,
    reopen_duration: Duration,
    snapshot_materialization_duration: Duration,
    first_query_duration: Duration,
    recalled: usize,
    recall_denominator: usize,
    minimum_recall: usize,
    recall_p50: usize,
    mean_visited_nodes: f64,
    mean_candidate_count: f64,
    exact: LatencySummary,
    approximate: LatencySummary,
}

fn main() -> Result<(), Box<dyn Error>> {
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("ann_durable_smoke requires the exact source commit")?;
    let environment = std::env::args()
        .nth(2)
        .ok_or("ann_durable_smoke requires a disclosed environment label")?;
    let temporary = TemporaryDirectory::create()?;
    let corpus = build_corpus()?;
    let index = ObjectId::new(90_002)?;
    let config = HnswConfig::new(16, 128, 128, 256, 0xc0de_cafe)?;
    let options = AnnSearchOptions::new(K, EF_SEARCH, Some(EF_SEARCH))?;

    let mut database = NativeDatabase::create(temporary.path())?;
    let mut transaction = database.begin(1, DurabilityClass::Strict)?;
    transaction.create_vector_index(
        index,
        "durable_ann_smoke",
        DIMENSION,
        VectorMetric::Cosine,
        config,
    )?;
    let seed_started = Instant::now();
    let vectors = corpus
        .vectors
        .iter()
        .enumerate()
        .map(|(offset, vector)| {
            Ok((
                ObjectId::new(u128::try_from(offset)?.saturating_add(1))?,
                vector.clone(),
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    transaction.upsert_vectors(index, vectors)?;
    let private_seed_duration = seed_started.elapsed();
    let commit_started = Instant::now();
    let commit = transaction.commit()?;
    let strict_commit_duration = commit_started.elapsed();
    let before_reopen = database.search_ann_latest(index, &corpus.queries[0], options)?;
    drop(database);

    let reopen_started = Instant::now();
    let database = NativeDatabase::open(temporary.path())?;
    let reopen_duration = reopen_started.elapsed();
    let snapshot_started = Instant::now();
    let snapshot = database.snapshot(2)?;
    let snapshot_materialization_duration = snapshot_started.elapsed();
    let first_query_started = Instant::now();
    let after_reopen = snapshot.search_ann(index, &corpus.queries[0], options)?;
    let first_query_duration = first_query_started.elapsed();
    if after_reopen != before_reopen
        || after_reopen.snapshot_csn != Some(commit.commit_csn)
        || !after_reopen.approximate
    {
        return Err("reopened ANN generation differs from committed state".into());
    }

    let quality = measure_quality(&snapshot, index, &corpus.queries, options)?;
    if quality.recalled.saturating_mul(100) < quality.recall_denominator.saturating_mul(95) {
        return Err("durable ANN quality smoke missed the recall@10 floor".into());
    }
    for observation in 0..WARMUP {
        let query = &corpus.queries[usize::try_from(observation)? % corpus.queries.len()];
        black_box(snapshot.search_ann(index, black_box(query), options)?);
        black_box(snapshot.search_vector_exact(index, black_box(query), K)?);
    }
    let approximate = measure_latency(OBSERVATIONS, |observation| {
        let query = &corpus.queries[usize::try_from(observation)? % corpus.queries.len()];
        black_box(snapshot.search_ann(index, black_box(query), options)?);
        Ok(())
    })?;
    let exact = measure_latency(OBSERVATIONS, |observation| {
        let query = &corpus.queries[usize::try_from(observation)? % corpus.queries.len()];
        black_box(snapshot.search_vector_exact(index, black_box(query), K)?);
        Ok(())
    })?;

    print_receipt(&Receipt {
        source_commit: &source_commit,
        environment: &environment,
        dataset_digest: corpus.digest,
        build_identity: after_reopen.build_identity,
        commit_csn: commit.commit_csn.get(),
        data_directory_bytes: directory_bytes(temporary.path())?,
        private_seed_duration,
        strict_commit_duration,
        reopen_duration,
        snapshot_materialization_duration,
        first_query_duration,
        recalled: quality.recalled,
        recall_denominator: quality.recall_denominator,
        minimum_recall: quality.minimum_recall,
        recall_p50: quality.recall_p50,
        mean_visited_nodes: quality.mean_visited_nodes,
        mean_candidate_count: quality.mean_candidate_count,
        exact,
        approximate,
    })?;
    Ok(())
}

struct QualitySummary {
    recalled: usize,
    recall_denominator: usize,
    minimum_recall: usize,
    recall_p50: usize,
    mean_visited_nodes: f64,
    mean_candidate_count: f64,
}

fn measure_quality(
    snapshot: &hyphae_native_runtime::NativeSnapshot,
    index: ObjectId,
    queries: &[Vector],
    options: AnnSearchOptions,
) -> Result<QualitySummary, Box<dyn Error>> {
    let mut recall_counts = Vec::with_capacity(queries.len());
    let mut visited_nodes = 0_usize;
    let mut candidate_count = 0_usize;
    for query in queries {
        let exact = snapshot.search_vector_exact(index, query, K)?;
        let approximate = snapshot.search_ann(index, query, options)?;
        let exact_ids = exact
            .iter()
            .map(|hit| hit.object_id)
            .collect::<BTreeSet<_>>();
        recall_counts.push(
            approximate
                .hits
                .iter()
                .filter(|hit| exact_ids.contains(&hit.object_id))
                .count(),
        );
        visited_nodes = visited_nodes.saturating_add(approximate.visited_nodes);
        candidate_count = candidate_count.saturating_add(approximate.candidate_count);
    }
    recall_counts.sort_unstable();
    let recalled = recall_counts.iter().sum::<usize>();
    Ok(QualitySummary {
        recalled,
        recall_denominator: queries.len().saturating_mul(K),
        minimum_recall: recall_counts.first().copied().unwrap_or(0),
        recall_p50: recall_counts[percentile_index(recall_counts.len(), 50)],
        mean_visited_nodes: ratio(visited_nodes, queries.len())?,
        mean_candidate_count: ratio(candidate_count, queries.len())?,
    })
}

fn measure_latency(
    observations: u32,
    mut operation: impl FnMut(u32) -> Result<(), Box<dyn Error>>,
) -> Result<LatencySummary, Box<dyn Error>> {
    let mut latencies = Vec::with_capacity(usize::try_from(observations)?);
    let started = Instant::now();
    for observation in 0..observations {
        let sample_started = Instant::now();
        operation(observation)?;
        latencies.push(sample_started.elapsed());
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    Ok(LatencySummary {
        p50: duration_micros(latencies[percentile_index(latencies.len(), 50)]),
        p95: duration_micros(latencies[percentile_index(latencies.len(), 95)]),
        p99: duration_micros(latencies[percentile_index(latencies.len(), 99)]),
        p999: duration_micros(latencies[percentile_index_permille(latencies.len(), 999)]),
        maximum: latencies.last().copied().map_or(0.0, duration_micros),
        throughput_per_second: f64::from(observations) / elapsed.as_secs_f64(),
    })
}

fn build_corpus() -> Result<Corpus, Box<dyn Error>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-ann-durable-corpus-v1");
    let vectors = (1..=VECTOR_COUNT)
        .map(|seed| {
            let vector = deterministic_vector(u64::from(seed), DIMENSION)?;
            hash_vector(&mut hasher, &vector);
            Ok(vector)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let queries = (0..QUERY_COUNT)
        .map(|query| {
            let vector =
                deterministic_vector(1_000_000_u64.saturating_add(u64::from(query)), DIMENSION)?;
            hash_vector(&mut hasher, &vector);
            Ok(vector)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    Ok(Corpus {
        vectors,
        queries,
        digest: *hasher.finalize().as_bytes(),
    })
}

fn deterministic_vector(seed: u64, dimension: u16) -> Result<Vector, Box<dyn Error>> {
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    let mut values = Vec::with_capacity(usize::from(dimension));
    for component in 0..dimension {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state ^= u64::from(component).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let raw = u16::try_from((state >> 16) & u64::from(u16::MAX))?;
        values.push(f32::from(raw) / 32_767.5 - 1.0);
    }
    Ok(Vector::new(values)?)
}

fn hash_vector(hasher: &mut blake3::Hasher, vector: &Vector) {
    for value in vector.values() {
        hasher.update(&value.to_bits().to_le_bytes());
    }
}

fn directory_bytes(path: &Path) -> Result<u64, Box<dyn Error>> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or("data-directory byte count overflow")?;
        }
    }
    Ok(total)
}

fn print_receipt(receipt: &Receipt<'_>) -> Result<(), Box<dyn Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-ann-durable-smoke-v1\",");
    println!(
        "  \"source_commit\": {},",
        json_string(receipt.source_commit)?
    );
    println!("  \"environment\": {},", json_string(receipt.environment)?);
    println!(
        "  \"dataset_digest\": \"{}\",",
        encode_hex(&receipt.dataset_digest)
    );
    println!("  \"vector_count\": {VECTOR_COUNT},");
    println!("  \"dimension\": {DIMENSION},");
    println!("  \"query_count\": {QUERY_COUNT},");
    println!("  \"observations_per_route\": {OBSERVATIONS},");
    println!("  \"warmup_per_route\": {WARMUP},");
    println!("  \"k\": {K},");
    println!("  \"ef_search\": {EF_SEARCH},");
    println!("  \"durability\": \"strict\",");
    println!("  \"commit_csn\": {},", receipt.commit_csn);
    println!(
        "  \"build_identity\": \"{}\",",
        encode_hex(&receipt.build_identity)
    );
    println!(
        "  \"data_directory_bytes\": {},",
        receipt.data_directory_bytes
    );
    print_duration(
        "private_seed_duration_millis",
        receipt.private_seed_duration,
    );
    print_duration(
        "strict_commit_duration_millis",
        receipt.strict_commit_duration,
    );
    print_duration("reopen_duration_millis", receipt.reopen_duration);
    print_duration(
        "snapshot_materialization_duration_millis",
        receipt.snapshot_materialization_duration,
    );
    println!(
        "  \"first_query_after_snapshot_micros\": {:.3},",
        duration_micros(receipt.first_query_duration)
    );
    println!(
        "  \"recall_at_10\": {:.6},",
        ratio(receipt.recalled, receipt.recall_denominator)?
    );
    println!("  \"recall_at_10_floor\": 0.950000,");
    println!("  \"recall_floor_met\": true,");
    println!(
        "  \"minimum_query_recall_at_10\": {},",
        receipt.minimum_recall
    );
    println!("  \"p50_query_recall_at_10\": {},", receipt.recall_p50);
    println!(
        "  \"mean_visited_nodes\": {:.3},",
        receipt.mean_visited_nodes
    );
    println!(
        "  \"mean_candidate_count\": {:.3},",
        receipt.mean_candidate_count
    );
    print_latency("exact", &receipt.exact, true);
    print_latency("hnsw", &receipt.approximate, false);
    println!("}}");
    Ok(())
}

fn print_duration(name: &str, duration: Duration) {
    println!("  \"{name}\": {:.3},", duration_micros(duration) / 1_000.0);
}

fn print_latency(name: &str, summary: &LatencySummary, trailing_comma: bool) {
    println!("  \"{name}_latency_micros\": {{");
    println!("    \"p50\": {:.3},", summary.p50);
    println!("    \"p95\": {:.3},", summary.p95);
    println!("    \"p99\": {:.3},", summary.p99);
    println!("    \"p99_9\": {:.3},", summary.p999);
    println!("    \"maximum\": {:.3},", summary.maximum);
    println!(
        "    \"throughput_per_second\": {:.3}",
        summary.throughput_per_second
    );
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

const fn percentile_index(length: usize, percentile: usize) -> usize {
    length.saturating_sub(1).saturating_mul(percentile) / 100
}

const fn percentile_index_permille(length: usize, permille: usize) -> usize {
    length.saturating_sub(1).saturating_mul(permille) / 1_000
}

fn duration_micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn ratio(numerator: usize, denominator: usize) -> Result<f64, std::num::TryFromIntError> {
    Ok(f64::from(u32::try_from(numerator)?) / f64::from(u32::try_from(denominator)?))
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

fn json_string(value: &str) -> Result<String, std::fmt::Error> {
    let mut encoded = String::with_capacity(value.len().saturating_add(2));
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            character if character.is_control() => {
                write!(encoded, "\\u{:04x}", u32::from(character))?;
            }
            character => encoded.push(character),
        }
    }
    encoded.push('"');
    Ok(encoded)
}
