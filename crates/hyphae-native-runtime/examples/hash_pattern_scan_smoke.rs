// SPDX-License-Identifier: Apache-2.0

//! Linux release observation for native bounded hash pattern scans.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    HashFieldEntry, HashPatternScanRequest, MAX_HASH_PATTERN_MATCH_STEPS, NativeDatabase,
    NativeRuntimeError,
};
use hyphae_native_types::DurabilityClass;

const HASH_KEY: &[u8] = b"benchmark-hash-pattern-scan";
const PREFIX_A: &[u8] = b"tenant-a:";
const PREFIX_B: &[u8] = b"tenant-b:";
const NEEDLE: &[u8] = b"needle";
const PREFIX_PATTERN: &[u8] = b"tenant-b:*";
const LEADING_WILDCARD_PATTERN: &[u8] = b"*needle";
const FIELDS_PER_TENANT: u32 = 1_024;
const FIELD_COUNT: usize = 2_048;
const PAGE_FIELDS: usize = 32;
const FIELD_BYTES: usize = 64;
const VALUE_BYTES: usize = 64;
const WARMUP: usize = 1_000;
const OBSERVATIONS: usize = 10_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-hash-pattern-scan-{}-{timestamp}",
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

#[derive(Clone, Copy)]
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    maximum_nanos: u64,
    throughput_per_second: f64,
}

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn measure<T>(
    mut operation: impl FnMut() -> Result<T, NativeRuntimeError>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(operation()?);
    }
    let mut samples = Vec::with_capacity(OBSERVATIONS);
    let total_started = Instant::now();
    for _ in 0..OBSERVATIONS {
        let started = Instant::now();
        black_box(operation()?);
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    let elapsed = total_started.elapsed();
    samples.sort_unstable();
    let completed = u32::try_from(OBSERVATIONS)?;
    Ok(Stats {
        p50_nanos: percentile(&samples, 500),
        p95_nanos: percentile(&samples, 950),
        p99_nanos: percentile(&samples, 990),
        p999_nanos: percentile(&samples, 999),
        maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
        throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
    })
}

fn field(tenant: u8, index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let prefix = match tenant {
        0 => PREFIX_A,
        1 => PREFIX_B,
        _ => return Err("benchmark tenant is outside the fixed dataset".into()),
    };
    let mut field = vec![u8::try_from(index % 251)?; FIELD_BYTES];
    field[..prefix.len()].copy_from_slice(prefix);
    field[prefix.len()..prefix.len() + 4].copy_from_slice(&index.to_be_bytes());
    if index.is_multiple_of(64) {
        field[FIELD_BYTES - NEEDLE.len()..].copy_from_slice(NEEDLE);
    }
    Ok(field)
}

fn value(tenant: u8, index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut value = vec![u8::try_from((index + u32::from(tenant) + 97) % 251)?; VALUE_BYTES];
    value[..4].copy_from_slice(&index.to_le_bytes());
    value[4] = tenant;
    Ok(value)
}

fn prepare_database(
    path: &Path,
) -> Result<(NativeDatabase, blake3::Hash), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_hash(HASH_KEY.to_vec())?;
    let mut dataset_hasher = blake3::Hasher::new();
    for tenant in 0..=1 {
        for index in 0..FIELDS_PER_TENANT {
            let field = field(tenant, index)?;
            let value = value(tenant, index)?;
            dataset_hasher.update(&field);
            dataset_hasher.update(&value);
            seed.hset(HASH_KEY.to_vec(), field, value)?;
        }
    }
    seed.commit()?;
    drop(database);

    let database = NativeDatabase::open(path)?;
    if database.hlen_latest_hash(HASH_KEY)? != FIELD_COUNT {
        return Err("reopened benchmark hash cardinality is wrong".into());
    }
    Ok((database, dataset_hasher.finalize()))
}

fn fallback_filter(
    database: &NativeDatabase,
    matches: impl Fn(&[u8]) -> bool,
) -> Result<Vec<HashFieldEntry>, NativeRuntimeError> {
    Ok(database
        .hscan_latest_hash(HASH_KEY, None, FIELD_COUNT)?
        .into_iter()
        .filter(|entry| matches(entry.field()))
        .take(PAGE_FIELDS)
        .collect())
}

fn validate_entries(
    entries: &[HashFieldEntry],
    expected_first: &[u8],
    expected_last: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    if entries.len() != PAGE_FIELDS
        || entries
            .first()
            .is_none_or(|entry| entry.field() != expected_first)
        || entries
            .last()
            .is_none_or(|entry| entry.field() != expected_last)
    {
        return Err("hash pattern scan returned the wrong cohort".into());
    }
    Ok(())
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_nanos\": {},", stats.p50_nanos);
    println!("      \"p95_nanos\": {},", stats.p95_nanos);
    println!("      \"p99_nanos\": {},", stats.p99_nanos);
    println!("      \"p999_nanos\": {},", stats.p999_nanos);
    println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
    println!(
        "      \"throughput_per_second\": {:.3}",
        stats.throughput_per_second
    );
    println!("    }}{}", if comma { "," } else { "" });
}

fn relative_improvement(reference: u64, candidate: u64) -> f64 {
    let reference = u32::try_from(reference).map_or(f64::INFINITY, f64::from);
    let candidate = u32::try_from(candidate).map_or(f64::INFINITY, f64::from);
    (reference - candidate) * 100.0 / reference
}

struct RouteObservation {
    native: Stats,
    fallback: Stats,
    visited: usize,
    match_steps: usize,
}

struct BenchmarkObservation {
    prefix: RouteObservation,
    leading_wildcard: RouteObservation,
}

fn measure_routes(
    database: &NativeDatabase,
    prefix_request: &HashPatternScanRequest,
    wildcard_request: &HashPatternScanRequest,
    prefix_page: &hyphae_native_runtime::HashPatternScanPage,
    wildcard_page: &hyphae_native_runtime::HashPatternScanPage,
) -> Result<BenchmarkObservation, Box<dyn std::error::Error>> {
    let prefix_native = measure(|| {
        database.hscan_match_latest_hash(black_box(HASH_KEY), black_box(prefix_request))
    })?;
    let prefix_fallback =
        measure(|| fallback_filter(black_box(database), |field| field.starts_with(PREFIX_B)))?;
    let wildcard_native = measure(|| {
        database.hscan_match_latest_hash(black_box(HASH_KEY), black_box(wildcard_request))
    })?;
    let wildcard_fallback =
        measure(|| fallback_filter(black_box(database), |field| field.ends_with(NEEDLE)))?;
    Ok(BenchmarkObservation {
        prefix: RouteObservation {
            native: prefix_native,
            fallback: prefix_fallback,
            visited: prefix_page.visited(),
            match_steps: prefix_page.match_steps(),
        },
        leading_wildcard: RouteObservation {
            native: wildcard_native,
            fallback: wildcard_fallback,
            visited: wildcard_page.visited(),
            match_steps: wildcard_page.match_steps(),
        },
    })
}

fn print_comparison(name: &str, route: &RouteObservation, comma: bool) {
    println!(
        "    \"{name}_p50_improvement_percent\": {:.3},",
        relative_improvement(route.fallback.p50_nanos, route.native.p50_nanos)
    );
    println!(
        "    \"{name}_p95_improvement_percent\": {:.3},",
        relative_improvement(route.fallback.p95_nanos, route.native.p95_nanos)
    );
    println!(
        "    \"{name}_p99_improvement_percent\": {:.3},",
        relative_improvement(route.fallback.p99_nanos, route.native.p99_nanos)
    );
    println!(
        "    \"{name}_throughput_improvement_percent\": {:.3}{}",
        (route.native.throughput_per_second - route.fallback.throughput_per_second) * 100.0
            / route.fallback.throughput_per_second,
        if comma { "," } else { "" }
    );
}

fn print_report(
    commit: &str,
    rustc: &str,
    dataset: blake3::Hash,
    structure_tree_height: usize,
    observation: &BenchmarkObservation,
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.hash-pattern-scan-smoke.v1\",");
    println!("  \"status\": \"observation-not-universal-slo\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"target\": \"x86_64-linux\",");
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"observations_per_route\": {OBSERVATIONS},");
    println!("  \"warmup_per_route\": {WARMUP},");
    println!("  \"hash_fields\": {FIELD_COUNT},");
    println!("  \"page_fields\": {PAGE_FIELDS},");
    println!("  \"field_bytes\": {FIELD_BYTES},");
    println!("  \"value_bytes\": {VALUE_BYTES},");
    println!("  \"dataset_blake3\": \"{}\",", dataset.to_hex());
    println!("  \"structure_tree_height\": {structure_tree_height},");
    println!("  \"route_counters\": {{");
    println!(
        "    \"prefix_native\": {{\"visited\": {}, \"match_steps\": {}}},",
        observation.prefix.visited, observation.prefix.match_steps
    );
    println!(
        "    \"leading_wildcard_native\": {{\"visited\": {}, \"match_steps\": {}}}",
        observation.leading_wildcard.visited, observation.leading_wildcard.match_steps
    );
    println!("  }},");
    println!("  \"metrics\": {{");
    print_stats("prefix_native_32", observation.prefix.native, true);
    print_stats(
        "prefix_full_hscan_filter_32",
        observation.prefix.fallback,
        true,
    );
    print_stats(
        "leading_wildcard_native_32",
        observation.leading_wildcard.native,
        true,
    );
    print_stats(
        "leading_wildcard_full_hscan_filter_32",
        observation.leading_wildcard.fallback,
        false,
    );
    println!("  }},");
    println!("  \"comparison\": {{");
    print_comparison("prefix", &observation.prefix, true);
    print_comparison("leading_wildcard", &observation.leading_wildcard, false);
    println!("  }}");
    println!("}}");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let (database, dataset) = prepare_database(temporary.path())?;
    let prefix_request = HashPatternScanRequest::try_new(
        PREFIX_PATTERN,
        None,
        PAGE_FIELDS,
        FIELD_COUNT,
        MAX_HASH_PATTERN_MATCH_STEPS,
    )?;
    let wildcard_request = HashPatternScanRequest::try_new(
        LEADING_WILDCARD_PATTERN,
        None,
        PAGE_FIELDS,
        FIELD_COUNT,
        MAX_HASH_PATTERN_MATCH_STEPS,
    )?;

    let prefix_page = database.hscan_match_latest_hash(HASH_KEY, &prefix_request)?;
    validate_entries(
        prefix_page.entries(),
        &field(1, 0)?,
        &field(1, u32::try_from(PAGE_FIELDS - 1)?)?,
    )?;
    let prefix_fallback = fallback_filter(&database, |field| field.starts_with(PREFIX_B))?;
    if prefix_fallback != prefix_page.entries() {
        return Err("native prefix glob and application fallback disagree".into());
    }

    let wildcard_page = database.hscan_match_latest_hash(HASH_KEY, &wildcard_request)?;
    validate_entries(
        wildcard_page.entries(),
        &field(0, 0)?,
        &field(1, FIELDS_PER_TENANT - 64)?,
    )?;
    let wildcard_fallback = fallback_filter(&database, |field| field.ends_with(NEEDLE))?;
    if wildcard_fallback != wildcard_page.entries() {
        return Err("native leading-wildcard glob and application fallback disagree".into());
    }

    let observation = measure_routes(
        &database,
        &prefix_request,
        &wildcard_request,
        &prefix_page,
        &wildcard_page,
    )?;
    print_report(
        &commit,
        &rustc,
        dataset,
        database.latest_structure_tree_height()?,
        &observation,
    );
    Ok(())
}
