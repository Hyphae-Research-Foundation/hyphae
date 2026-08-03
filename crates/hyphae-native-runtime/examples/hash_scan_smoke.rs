// SPDX-License-Identifier: Apache-2.0

//! Warm physical-read latency smoke for bounded native hash scans.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{HashFieldEntry, NativeDatabase};
use hyphae_native_types::DurabilityClass;

const HASH_KEY: &[u8] = b"benchmark-hash-scan";
const FIELDS: u32 = 2_048;
const FIELD_BYTES: usize = 64;
const VALUE_BYTES: usize = 64;
const WARMUP: u32 = 1_000;
const OBSERVATIONS: u32 = 10_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-hash-scan-smoke-{}-{timestamp}",
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

#[derive(Clone, Copy)]
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    max_nanos: u64,
    throughput_per_second: f64,
}

struct RouteStats {
    head: Stats,
    middle: Stats,
    tail: Stats,
}

fn measure(
    mut operation: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(usize::try_from(OBSERVATIONS)?);
    let total_started = Instant::now();
    for _ in 0..OBSERVATIONS {
        let started = Instant::now();
        operation()?;
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    let elapsed = total_started.elapsed().as_secs_f64();
    samples.sort_unstable();
    let percentile = |per_mille: usize| samples[(samples.len() - 1) * per_mille / 1_000];
    Ok(Stats {
        p50_nanos: percentile(500),
        p95_nanos: percentile(950),
        p99_nanos: percentile(990),
        p999_nanos: percentile(999),
        max_nanos: *samples.last().ok_or("missing latency observations")?,
        throughput_per_second: f64::from(OBSERVATIONS) / elapsed,
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
    let mut seed = database.begin(100, DurabilityClass::Strict)?;
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

fn validate_entries(
    entries: &[HashFieldEntry],
    first: u32,
    last: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let first_field = field(first)?;
    let last_field = field(last)?;
    let first_value = value(first)?;
    let last_value = value(last)?;
    if entries.len() != 10
        || entries
            .first()
            .is_none_or(|entry| entry.field() != first_field.as_slice())
        || entries
            .last()
            .is_none_or(|entry| entry.field() != last_field.as_slice())
        || entries
            .first()
            .is_none_or(|entry| entry.value() != first_value.as_slice())
        || entries
            .last()
            .is_none_or(|entry| entry.value() != last_value.as_slice())
    {
        return Err("physical hash scan returned the wrong cohort".into());
    }
    Ok(())
}

fn validate_routes(database: &NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    validate_entries(&database.hscan_latest_hash(HASH_KEY, None, 10)?, 0, 9)?;
    let middle_cursor = field(FIELDS / 2 - 1)?;
    validate_entries(
        &database.hscan_latest_hash(HASH_KEY, Some(&middle_cursor), 10)?,
        FIELDS / 2,
        FIELDS / 2 + 9,
    )?;
    let tail_cursor = field(FIELDS - 11)?;
    validate_entries(
        &database.hscan_latest_hash(HASH_KEY, Some(&tail_cursor), 10)?,
        FIELDS - 10,
        FIELDS - 1,
    )?;
    Ok(())
}

fn warm(
    database: &NativeDatabase,
    middle: &[u8],
    tail: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), None, 10)?);
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), Some(black_box(middle)), 10)?);
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), Some(black_box(tail)), 10)?);
    }
    Ok(())
}

fn measure_routes(
    database: &NativeDatabase,
    middle: &[u8],
    tail: &[u8],
) -> Result<RouteStats, Box<dyn std::error::Error>> {
    let head = measure(|| {
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), None, 10)?);
        Ok(())
    })?;
    let middle = measure(|| {
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), Some(black_box(middle)), 10)?);
        Ok(())
    })?;
    let tail = measure(|| {
        black_box(database.hscan_latest_hash(black_box(HASH_KEY), Some(black_box(tail)), 10)?);
        Ok(())
    })?;
    Ok(RouteStats { head, middle, tail })
}

fn print_stats(name: &str, stats: Stats, trailing_comma: bool) {
    println!(
        "    \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"p999_nanos\": {}, \"max_nanos\": {}, \
         \"throughput_per_second\": {:.3}}}{}",
        stats.p50_nanos,
        stats.p95_nanos,
        stats.p99_nanos,
        stats.p999_nanos,
        stats.max_nanos,
        stats.throughput_per_second,
        if trailing_comma { "," } else { "" }
    );
}

fn print_report(
    database: &NativeDatabase,
    dataset: blake3::Hash,
    routes: &RouteStats,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-hash-scan-smoke-v1\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"fields\": {FIELDS},");
    println!("  \"field_bytes\": {FIELD_BYTES},");
    println!("  \"value_bytes\": {VALUE_BYTES},");
    println!("  \"dataset_blake3\": \"{}\",", dataset.to_hex());
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("hscan_head_10_physical", routes.head, true);
    print_stats("hscan_middle_10_physical", routes.middle, true);
    print_stats("hscan_tail_10_physical", routes.tail, false);
    println!("  }}");
    println!("}}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let (database, dataset) = prepare_database(temporary.path())?;
    validate_routes(&database)?;
    let middle = field(FIELDS / 2 - 1)?;
    let tail = field(FIELDS - 11)?;
    warm(&database, &middle, &tail)?;
    let routes = measure_routes(&database, &middle, &tail)?;
    print_report(&database, dataset, &routes)?;
    Ok(())
}
