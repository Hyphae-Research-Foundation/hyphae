// SPDX-License-Identifier: Apache-2.0

//! Deterministic bounded quality and latency observation for the native HNSW kernel.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt::Write as _,
    hint::black_box,
    time::{Duration, Instant},
};

use hyphae_native_ann::{
    HnswConfig, HnswIndex, Metric, SearchOptions, Vector, VectorIndexDefinition, VectorRecord,
};
use hyphae_native_types::{Csn, ObjectId};

const VECTOR_COUNT: u16 = 10_000;
const DIMENSION: u16 = 32;
const QUERY_COUNT: u16 = 100;
const K: usize = 10;
const EF_SEARCH: usize = 128;

struct LatencySummary {
    p50: f64,
    p95: f64,
    p99: f64,
    maximum: f64,
}

struct Corpus {
    records: Vec<VectorRecord>,
    queries: Vec<Vector>,
    digest: [u8; 32],
}

struct Receipt<'input> {
    source_commit: &'input str,
    environment: &'input str,
    dataset_digest: [u8; 32],
    definition: VectorIndexDefinition,
    max_level: u16,
    directed_edges: usize,
    build_duration: Duration,
    build_identity: [u8; 32],
    recalled: usize,
    recall_denominator: usize,
    minimum_recall: usize,
    recall_p50: usize,
    visited_total: usize,
    candidate_total: usize,
    query_count: usize,
    exact_summary: LatencySummary,
    approximate_summary: LatencySummary,
}

fn main() -> Result<(), Box<dyn Error>> {
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("quality_smoke requires the exact source commit")?;
    let environment = std::env::args()
        .nth(2)
        .ok_or("quality_smoke requires a disclosed environment label")?;
    let definition = VectorIndexDefinition::new(
        ObjectId::new(90_001)?,
        DIMENSION,
        Metric::Cosine,
        HnswConfig::new(16, 128, 128, 256, 0xc0de_cafe)?,
    )?;
    let corpus = build_corpus()?;

    let build_started = Instant::now();
    let index = HnswIndex::build(definition, corpus.records)?;
    let build_duration = build_started.elapsed();
    index.validate()?;
    let snapshot = index.export_snapshot();
    let directed_edges = snapshot
        .nodes
        .iter()
        .flat_map(|node| &node.neighbors)
        .map(Vec::len)
        .sum::<usize>();

    let options = SearchOptions::new(K, EF_SEARCH, Some(EF_SEARCH))?;
    for query in &corpus.queries {
        black_box(index.search(black_box(query), options)?);
    }

    let mut exact_latencies = Vec::with_capacity(corpus.queries.len());
    let mut approximate_latencies = Vec::with_capacity(corpus.queries.len());
    let mut recall_counts = Vec::with_capacity(corpus.queries.len());
    let mut visited_total = 0_usize;
    let mut candidate_total = 0_usize;
    for query in &corpus.queries {
        let exact_started = Instant::now();
        let exact = black_box(index.search_exact(black_box(query), K)?);
        exact_latencies.push(exact_started.elapsed());

        let approximate_started = Instant::now();
        let approximate = black_box(index.search(black_box(query), options)?);
        approximate_latencies.push(approximate_started.elapsed());
        visited_total = visited_total.saturating_add(approximate.visited_nodes);
        candidate_total = candidate_total.saturating_add(approximate.candidate_count);
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
    }

    recall_counts.sort_unstable();
    let recalled = recall_counts.iter().sum::<usize>();
    let recall_denominator = corpus.queries.len().saturating_mul(K);
    let exact_summary = summarize(&mut exact_latencies);
    let approximate_summary = summarize(&mut approximate_latencies);
    let query_count = corpus.queries.len();
    let recall_p50 = recall_counts[percentile_index(recall_counts.len(), 50)];
    let minimum_recall = recall_counts.first().copied().unwrap_or(0);
    if recalled.saturating_mul(100) < recall_denominator.saturating_mul(95) {
        return Err("native ANN quality smoke missed the recall@10 floor".into());
    }

    print_receipt(&Receipt {
        source_commit: &source_commit,
        environment: &environment,
        dataset_digest: corpus.digest,
        definition,
        max_level: snapshot.max_level,
        directed_edges,
        build_duration,
        build_identity: index.build_identity(),
        recalled,
        recall_denominator,
        minimum_recall,
        recall_p50,
        visited_total,
        candidate_total,
        query_count,
        exact_summary,
        approximate_summary,
    })?;
    Ok(())
}

fn build_corpus() -> Result<Corpus, Box<dyn Error>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-ann-quality-corpus-v1");
    let records = (1..=VECTOR_COUNT)
        .map(|value| {
            let vector = deterministic_vector(u64::from(value), DIMENSION)?;
            hasher.update(&u64::from(value).to_le_bytes());
            hash_vector(&mut hasher, &vector);
            Ok(VectorRecord {
                object_id: ObjectId::new(u128::from(value))?,
                creating_csn: Csn::new(u64::from(value))?,
                vector,
            })
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
        records,
        queries,
        digest: *hasher.finalize().as_bytes(),
    })
}

fn print_receipt(receipt: &Receipt<'_>) -> Result<(), Box<dyn Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-ann-quality-v1\",");
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
    println!("  \"k\": {K},");
    println!("  \"m\": {},", receipt.definition.config().m());
    println!(
        "  \"ef_construction\": {},",
        receipt.definition.config().ef_construction()
    );
    println!("  \"ef_search\": {EF_SEARCH},");
    println!("  \"max_level\": {},", receipt.max_level);
    println!("  \"directed_edges\": {},", receipt.directed_edges);
    println!(
        "  \"build_identity\": \"{}\",",
        encode_hex(&receipt.build_identity)
    );
    println!(
        "  \"build_duration_millis\": {:.3},",
        duration_micros(receipt.build_duration) / 1_000.0
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
        ratio(receipt.visited_total, receipt.query_count)?
    );
    println!(
        "  \"mean_candidate_count\": {:.3},",
        ratio(receipt.candidate_total, receipt.query_count)?
    );
    print_latency("exact", &receipt.exact_summary, true);
    print_latency("hnsw", &receipt.approximate_summary, false);
    println!("}}");
    Ok(())
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

fn summarize(latencies: &mut [Duration]) -> LatencySummary {
    latencies.sort_unstable();
    LatencySummary {
        p50: duration_micros(latencies[percentile_index(latencies.len(), 50)]),
        p95: duration_micros(latencies[percentile_index(latencies.len(), 95)]),
        p99: duration_micros(latencies[percentile_index(latencies.len(), 99)]),
        maximum: latencies.last().copied().map_or(0.0, duration_micros),
    }
}

const fn percentile_index(length: usize, percentile: usize) -> usize {
    length.saturating_sub(1).saturating_mul(percentile) / 100
}

fn duration_micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn ratio(numerator: usize, denominator: usize) -> Result<f64, std::num::TryFromIntError> {
    Ok(f64::from(u32::try_from(numerator)?) / f64::from(u32::try_from(denominator)?))
}

fn print_latency(name: &str, summary: &LatencySummary, trailing_comma: bool) {
    println!("  \"{name}_latency_micros\": {{");
    println!("    \"p50\": {:.3},", summary.p50);
    println!("    \"p95\": {:.3},", summary.p95);
    println!("    \"p99\": {:.3},", summary.p99);
    println!("    \"maximum\": {:.3}", summary.maximum);
    println!("  }}{}", if trailing_comma { "," } else { "" });
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
