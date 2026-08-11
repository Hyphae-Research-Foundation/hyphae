// SPDX-License-Identifier: AGPL-3.0-only

//! Linux release observation for native reverse hash scans.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{HashFieldEntry, NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

const HASH_KEY: &[u8] = b"benchmark-hash-reverse-scan";
const FIELDS: u32 = 2_048;
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
            "hyphae-native-hash-reverse-scan-{}-{timestamp}",
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

fn field(index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut field = vec![u8::try_from(index % 251)?; FIELD_BYTES];
    field[..4].copy_from_slice(&index.to_be_bytes());
    Ok(field)
}

fn value(index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut value = vec![u8::try_from((index + 97) % 251)?; VALUE_BYTES];
    value[..4].copy_from_slice(&index.to_le_bytes());
    Ok(value)
}

fn prepare_database(
    path: &Path,
) -> Result<(NativeDatabase, blake3::Hash), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_hash(HASH_KEY.to_vec())?;
    let mut dataset_hasher = blake3::Hasher::new();
    for index in 0..FIELDS {
        let field = field(index)?;
        let value = value(index)?;
        dataset_hasher.update(&field);
        dataset_hasher.update(&value);
        seed.hset(HASH_KEY.to_vec(), field, value)?;
    }
    seed.commit()?;
    drop(database);

    let database = NativeDatabase::open(path)?;
    if database.hlen_latest_hash(HASH_KEY)? != usize::try_from(FIELDS)? {
        return Err("reopened benchmark hash cardinality is wrong".into());
    }
    Ok((database, dataset_hasher.finalize()))
}

fn fallback_reverse(database: &NativeDatabase) -> Result<Vec<HashFieldEntry>, NativeRuntimeError> {
    let mut entries = database.hscan_latest_hash(HASH_KEY, None, FIELD_COUNT)?;
    entries.reverse();
    entries.truncate(PAGE_FIELDS);
    Ok(entries)
}

fn validate_route(
    entries: &[HashFieldEntry],
    greatest: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let least = greatest
        .checked_sub(u32::try_from(PAGE_FIELDS.saturating_sub(1))?)
        .ok_or("invalid reverse validation cohort")?;
    let greatest_field = field(greatest)?;
    let least_field = field(least)?;
    let greatest_value = value(greatest)?;
    let least_value = value(least)?;
    if entries.len() != PAGE_FIELDS
        || entries
            .first()
            .is_none_or(|entry| entry.field() != greatest_field)
        || entries
            .last()
            .is_none_or(|entry| entry.field() != least_field)
        || entries
            .first()
            .is_none_or(|entry| entry.value() != greatest_value)
        || entries
            .last()
            .is_none_or(|entry| entry.value() != least_value)
    {
        return Err("reverse hash scan returned the wrong cohort".into());
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let (database, dataset) = prepare_database(temporary.path())?;
    let middle_cursor = field(FIELDS / 2)?;

    let reverse_top = database.hscan_reverse_latest_hash(HASH_KEY, None, PAGE_FIELDS)?;
    validate_route(&reverse_top, FIELDS - 1)?;
    let reverse_middle =
        database.hscan_reverse_latest_hash(HASH_KEY, Some(&middle_cursor), PAGE_FIELDS)?;
    validate_route(&reverse_middle, FIELDS / 2 - 1)?;
    let fallback = fallback_reverse(&database)?;
    if fallback != reverse_top {
        return Err("native reverse route and fallback disagree".into());
    }

    let reverse_top =
        measure(|| database.hscan_reverse_latest_hash(black_box(HASH_KEY), None, PAGE_FIELDS))?;
    let reverse_middle = measure(|| {
        database.hscan_reverse_latest_hash(
            black_box(HASH_KEY),
            Some(black_box(&middle_cursor)),
            PAGE_FIELDS,
        )
    })?;
    let fallback = measure(|| fallback_reverse(&database))?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.hash-reverse-scan-smoke.v1\",");
    println!("  \"status\": \"observation-not-universal-slo\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"target\": \"x86_64-linux\",");
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"observations_per_route\": {OBSERVATIONS},");
    println!("  \"warmup_per_route\": {WARMUP},");
    println!("  \"hash_fields\": {FIELDS},");
    println!("  \"page_fields\": {PAGE_FIELDS},");
    println!("  \"field_bytes\": {FIELD_BYTES},");
    println!("  \"value_bytes\": {VALUE_BYTES},");
    println!("  \"dataset_blake3\": \"{}\",", dataset.to_hex());
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("reverse_top_32_physical", reverse_top, true);
    print_stats("reverse_middle_32_physical", reverse_middle, true);
    print_stats(
        "ascending_all_reverse_truncate_32_fallback",
        fallback,
        false,
    );
    println!("  }},");
    println!("  \"comparison\": {{");
    println!(
        "    \"reverse_top_p50_improvement_percent\": {:.3},",
        relative_improvement(fallback.p50_nanos, reverse_top.p50_nanos)
    );
    println!(
        "    \"reverse_top_p95_improvement_percent\": {:.3},",
        relative_improvement(fallback.p95_nanos, reverse_top.p95_nanos)
    );
    println!(
        "    \"reverse_top_p99_improvement_percent\": {:.3},",
        relative_improvement(fallback.p99_nanos, reverse_top.p99_nanos)
    );
    println!(
        "    \"reverse_top_throughput_improvement_percent\": {:.3}",
        (reverse_top.throughput_per_second - fallback.throughput_per_second) * 100.0
            / fallback.throughput_per_second
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
