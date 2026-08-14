// SPDX-License-Identifier: AGPL-3.0-only

//! Focused ordered-expiry scan and bounded-cleanup latency smoke.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_pages::PAGE_SIZE;
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const WARMUP: u32 = 10_000;
const EMPTY_OBSERVATIONS: u32 = 100_000;
const EMPTY_OPERATIONS_PER_OBSERVATION: u32 = 8;
const MEMORY_KEYS: u32 = 4_096;
const MEMORY_BATCH: usize = 64;
const STRICT_KEYS: u32 = 512;
const STRICT_BATCH: usize = 16;
const DUE_AT: i64 = 100;
const BEFORE_DUE: i64 = 99;
const PAGE_FILE: &str = "pages.hydb";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-expiry-{label}-{}-{timestamp}",
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
    first_nanos: u64,
    last_nanos: u64,
    throughput_per_second: f64,
}

struct CleanupStats {
    latency: Stats,
    pages_appended: u64,
    bytes_appended: u64,
    pages_per_key: f64,
}

fn summarize(
    samples: Vec<u64>,
    operations_per_sample: u32,
    elapsed_seconds: f64,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let first_nanos = *samples.first().ok_or("missing latency observations")?;
    let last_nanos = *samples.last().ok_or("missing latency observations")?;
    let throughput_per_second = f64::from(u32::try_from(samples.len())?)
        * f64::from(operations_per_sample)
        / elapsed_seconds;
    let mut ordered = samples;
    ordered.sort_unstable();
    let percentile = |per_mille: usize| ordered[(ordered.len() - 1) * per_mille / 1_000];
    Ok(Stats {
        p50_nanos: percentile(500),
        p95_nanos: percentile(950),
        p99_nanos: percentile(990),
        p999_nanos: percentile(999),
        max_nanos: *ordered.last().ok_or("missing latency observations")?,
        first_nanos,
        last_nanos,
        throughput_per_second,
    })
}

fn seed(path: &Path, keys: u32) -> Result<(NativeDatabase, String), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let mut dataset_hasher = blake3::Hasher::new();
    for index in 0..keys {
        let key = index.to_be_bytes().to_vec();
        dataset_hasher.update(&key);
        dataset_hasher.update(&DUE_AT.to_be_bytes());
        transaction.set(key.clone(), key, Some(DUE_AT))?;
    }
    transaction.commit()?;
    drop(database);
    Ok((
        NativeDatabase::open(path)?,
        dataset_hasher.finalize().to_hex().to_string(),
    ))
}

fn measure_empty(database: &mut NativeDatabase) -> Result<Stats, Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        let receipt = database.expire_due_structures(
            black_box(BEFORE_DUE),
            black_box(MEMORY_BATCH),
            DurabilityClass::Memory,
        )?;
        if receipt.commit.is_some() {
            return Err("empty expiry warmup published a commit".into());
        }
    }
    let mut samples = Vec::with_capacity(usize::try_from(EMPTY_OBSERVATIONS)?);
    let total_started = Instant::now();
    for _ in 0..EMPTY_OBSERVATIONS {
        let started = Instant::now();
        for _ in 0..EMPTY_OPERATIONS_PER_OBSERVATION {
            let receipt = black_box(database.expire_due_structures(
                black_box(BEFORE_DUE),
                black_box(MEMORY_BATCH),
                DurabilityClass::Memory,
            )?);
            if receipt.commit.is_some() {
                return Err("empty expiry observation published a commit".into());
            }
        }
        samples.push(
            u64::try_from(started.elapsed().as_nanos())?
                / u64::from(EMPTY_OPERATIONS_PER_OBSERVATION),
        );
    }
    summarize(
        samples,
        EMPTY_OPERATIONS_PER_OBSERVATION,
        total_started.elapsed().as_secs_f64(),
    )
}

fn measure_cleanup(
    database: &mut NativeDatabase,
    path: &Path,
    keys: u32,
    batch: usize,
    durability: DurabilityClass,
) -> Result<CleanupStats, Box<dyn std::error::Error>> {
    let expected_batches = usize::try_from(keys)?.div_ceil(batch);
    let mut samples = Vec::with_capacity(expected_batches);
    let mut expired = 0_usize;
    let bytes_before = fs::metadata(path.join(PAGE_FILE))?.len();
    let total_started = Instant::now();
    while expired < usize::try_from(keys)? {
        let started = Instant::now();
        let receipt = black_box(database.expire_due_structures(DUE_AT, batch, durability)?);
        if receipt.expired_keys == 0 || receipt.commit.is_none() {
            return Err("due expiry observation did not publish cleanup".into());
        }
        expired = expired
            .checked_add(receipt.expired_keys)
            .ok_or("expired-key count overflow")?;
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    if samples.len() != expected_batches
        || database
            .expire_due_structures(DUE_AT, batch, durability)?
            .commit
            .is_some()
    {
        return Err("expiry cleanup batch count or terminal empty sweep is wrong".into());
    }
    let bytes_appended = fs::metadata(path.join(PAGE_FILE))?
        .len()
        .checked_sub(bytes_before)
        .ok_or("expiry cleanup page file shrank")?;
    let page_size = u64::try_from(PAGE_SIZE)?;
    if bytes_appended % page_size != 0 {
        return Err("expiry cleanup appended a partial native page".into());
    }
    Ok(CleanupStats {
        latency: summarize(samples, 1, total_started.elapsed().as_secs_f64())?,
        pages_appended: bytes_appended / page_size,
        bytes_appended,
        pages_per_key: f64::from(u32::try_from(bytes_appended / page_size)?) / f64::from(keys),
    })
}

fn print_stats(name: &str, stats: Stats, keys_per_batch: u32, trailing_comma: bool) {
    println!(
        "    \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"p999_nanos\": {}, \"max_nanos\": {}, \
         \"first_nanos\": {}, \"last_nanos\": {}, \
         \"operations_per_second\": {:.3}, \"work_units_per_second\": {:.3}}}{}",
        stats.p50_nanos,
        stats.p95_nanos,
        stats.p99_nanos,
        stats.p999_nanos,
        stats.max_nanos,
        stats.first_nanos,
        stats.last_nanos,
        stats.throughput_per_second,
        stats.throughput_per_second * f64::from(keys_per_batch),
        if trailing_comma { "," } else { "" }
    );
}

fn print_cleanup_stats(
    name: &str,
    stats: &CleanupStats,
    keys_per_batch: u32,
    trailing_comma: bool,
) {
    println!(
        "    \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"p999_nanos\": {}, \"max_nanos\": {}, \
         \"first_nanos\": {}, \"last_nanos\": {}, \
         \"operations_per_second\": {:.3}, \"work_units_per_second\": {:.3}, \
         \"pages_appended\": {}, \"bytes_appended\": {}, \"pages_per_key\": {:.6}}}{}",
        stats.latency.p50_nanos,
        stats.latency.p95_nanos,
        stats.latency.p99_nanos,
        stats.latency.p999_nanos,
        stats.latency.max_nanos,
        stats.latency.first_nanos,
        stats.latency.last_nanos,
        stats.latency.throughput_per_second,
        stats.latency.throughput_per_second * f64::from(keys_per_batch),
        stats.pages_appended,
        stats.bytes_appended,
        stats.pages_per_key,
        if trailing_comma { "," } else { "" }
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let memory_directory = TemporaryDirectory::create("memory")?;
    let (mut memory_database, memory_digest) = seed(memory_directory.path(), MEMORY_KEYS)?;
    let tree_height = memory_database.latest_structure_tree_height()?;
    let empty = measure_empty(&mut memory_database)?;
    let memory_cleanup = measure_cleanup(
        &mut memory_database,
        memory_directory.path(),
        MEMORY_KEYS,
        MEMORY_BATCH,
        DurabilityClass::Memory,
    )?;

    let strict_directory = TemporaryDirectory::create("strict")?;
    let (mut strict_database, strict_digest) = seed(strict_directory.path(), STRICT_KEYS)?;
    let strict_cleanup = measure_cleanup(
        &mut strict_database,
        strict_directory.path(),
        STRICT_KEYS,
        STRICT_BATCH,
        DurabilityClass::Strict,
    )?;

    println!("{{");
    println!("  \"schema\": \"hyphae-native-expiry-smoke-v2\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"empty_observations\": {EMPTY_OBSERVATIONS},");
    println!("  \"empty_warmup\": {WARMUP},");
    println!("  \"empty_operations_per_observation\": {EMPTY_OPERATIONS_PER_OBSERVATION},");
    println!("  \"memory_keys\": {MEMORY_KEYS},");
    println!("  \"memory_batch\": {MEMORY_BATCH},");
    println!("  \"strict_keys\": {STRICT_KEYS},");
    println!("  \"strict_batch\": {STRICT_BATCH},");
    println!("  \"structure_tree_height\": {tree_height},");
    println!("  \"memory_dataset_blake3\": \"{memory_digest}\",");
    println!("  \"strict_dataset_blake3\": \"{strict_digest}\",");
    println!("  \"metrics\": {{");
    print_stats("empty_due_scan", empty, 1, true);
    print_cleanup_stats(
        "memory_cleanup",
        &memory_cleanup,
        u32::try_from(MEMORY_BATCH)?,
        true,
    );
    print_cleanup_stats(
        "strict_cleanup",
        &strict_cleanup,
        u32::try_from(STRICT_BATCH)?,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
