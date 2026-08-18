// SPDX-License-Identifier: Apache-2.0

//! Focused structure-tombstone reachability compaction smoke.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_pages::PAGE_SIZE;
use hyphae_native_runtime::{NativeDatabase, StructureCompactionReceipt};
use hyphae_native_types::DurabilityClass;

const MEMORY_EXPIRED_KEYS: u32 = 2_048;
const MEMORY_LIVE_KEYS: u32 = 2_048;
const STRICT_EXPIRED_KEYS: u32 = 256;
const STRICT_LIVE_KEYS: u32 = 256;
const CLEANUP_BATCH: usize = 64;
const DUE_AT: i64 = 100;
const SCAN_WARMUP: u32 = 100;
const SCAN_OBSERVATIONS: u32 = 1_000;
const PAGE_FILE: &str = "pages.hydb";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-compaction-{label}-{}-{timestamp}",
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
struct ScanStats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    maximum_nanos: u64,
    operations_per_second: f64,
}

struct CompactionStats {
    latency_nanos: u64,
    file_bytes_before: u64,
    file_bytes_after: u64,
    receipt: StructureCompactionReceipt,
}

fn prepare_dataset(
    path: &Path,
    expired_keys: u32,
    live_keys: u32,
) -> Result<(NativeDatabase, String), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    let mut hasher = blake3::Hasher::new();
    for index in 0..expired_keys {
        let mut key = vec![b'e'];
        key.extend_from_slice(&index.to_be_bytes());
        let value = index.to_be_bytes().repeat(8);
        hasher.update(&key);
        hasher.update(&value);
        hasher.update(&DUE_AT.to_be_bytes());
        seed.set(key, value, Some(DUE_AT))?;
    }
    for index in 0..live_keys {
        let mut key = vec![b'l'];
        key.extend_from_slice(&index.to_be_bytes());
        let value = index.to_be_bytes().repeat(8);
        hasher.update(&key);
        hasher.update(&value);
        seed.set(key, value, None)?;
    }
    seed.commit()?;
    let mut cleaned = 0_usize;
    while cleaned < usize::try_from(expired_keys)? {
        let receipt =
            database.expire_due_structures(DUE_AT, CLEANUP_BATCH, DurabilityClass::Strict)?;
        if receipt.expired_keys == 0 || receipt.commit.is_none() {
            return Err("dataset cleanup stopped before all due keys".into());
        }
        cleaned = cleaned
            .checked_add(receipt.expired_keys)
            .ok_or("cleanup count overflow")?;
    }
    if database
        .expire_due_structures(DUE_AT, CLEANUP_BATCH, DurabilityClass::Strict)?
        .commit
        .is_some()
    {
        return Err("terminal dataset cleanup published a commit".into());
    }
    drop(database);
    Ok((
        NativeDatabase::open(path)?,
        hasher.finalize().to_hex().to_string(),
    ))
}

fn measure_empty_due_scan(
    database: &mut NativeDatabase,
) -> Result<ScanStats, Box<dyn std::error::Error>> {
    for _ in 0..SCAN_WARMUP {
        let receipt = database.expire_due_structures(
            black_box(DUE_AT),
            black_box(CLEANUP_BATCH),
            DurabilityClass::Memory,
        )?;
        if receipt.commit.is_some() {
            return Err("empty scan warmup published a commit".into());
        }
    }
    let mut samples = Vec::with_capacity(usize::try_from(SCAN_OBSERVATIONS)?);
    let total_started = Instant::now();
    for _ in 0..SCAN_OBSERVATIONS {
        let started = Instant::now();
        let receipt = black_box(database.expire_due_structures(
            black_box(DUE_AT),
            black_box(CLEANUP_BATCH),
            DurabilityClass::Memory,
        )?);
        if receipt.commit.is_some() {
            return Err("empty scan observation published a commit".into());
        }
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    let operations_per_second =
        f64::from(SCAN_OBSERVATIONS) / total_started.elapsed().as_secs_f64();
    samples.sort_unstable();
    let percentile = |per_mille: usize| samples[(samples.len() - 1) * per_mille / 1_000];
    Ok(ScanStats {
        p50_nanos: percentile(500),
        p95_nanos: percentile(950),
        p99_nanos: percentile(990),
        maximum_nanos: *samples.last().ok_or("missing scan samples")?,
        operations_per_second,
    })
}

fn measure_compaction(
    database: &mut NativeDatabase,
    path: &Path,
    durability: DurabilityClass,
) -> Result<CompactionStats, Box<dyn std::error::Error>> {
    let file_bytes_before = fs::metadata(path.join(PAGE_FILE))?.len();
    let started = Instant::now();
    let receipt = black_box(database.compact_structure(durability)?);
    let latency_nanos = u64::try_from(started.elapsed().as_nanos())?;
    let file_bytes_after = fs::metadata(path.join(PAGE_FILE))?.len();
    let expected_growth = receipt
        .pages_appended
        .checked_mul(u64::try_from(PAGE_SIZE)?)
        .ok_or("compaction byte count overflow")?;
    if receipt.commit.is_none()
        || receipt.dropped_tombstones == 0
        || receipt.reachable_pages_after >= receipt.reachable_pages_before
        || file_bytes_after.checked_sub(file_bytes_before) != Some(expected_growth)
    {
        return Err("compaction receipt and physical growth disagree".into());
    }
    Ok(CompactionStats {
        latency_nanos,
        file_bytes_before,
        file_bytes_after,
        receipt,
    })
}

fn print_scan(name: &str, stats: ScanStats, trailing_comma: bool) {
    println!(
        "    \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"maximum_nanos\": {}, \
         \"operations_per_second\": {:.3}}}{}",
        stats.p50_nanos,
        stats.p95_nanos,
        stats.p99_nanos,
        stats.maximum_nanos,
        stats.operations_per_second,
        if trailing_comma { "," } else { "" }
    );
}

fn print_compaction(name: &str, stats: &CompactionStats, trailing_comma: bool) {
    let receipt = stats.receipt;
    println!(
        "    \"{name}\": {{\"latency_nanos\": {}, \"scanned_entries\": {}, \
         \"retained_entries\": {}, \"dropped_tombstones\": {}, \
         \"reachable_pages_before\": {}, \"reachable_pages_after\": {}, \
         \"pages_appended\": {}, \"file_bytes_before\": {}, \
         \"file_bytes_after\": {}}}{}",
        stats.latency_nanos,
        receipt.scanned_entries,
        receipt.retained_entries,
        receipt.dropped_tombstones,
        receipt.reachable_pages_before,
        receipt.reachable_pages_after,
        receipt.pages_appended,
        stats.file_bytes_before,
        stats.file_bytes_after,
        if trailing_comma { "," } else { "" }
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let memory_directory = TemporaryDirectory::create("memory")?;
    let (mut memory, memory_digest) = prepare_dataset(
        memory_directory.path(),
        MEMORY_EXPIRED_KEYS,
        MEMORY_LIVE_KEYS,
    )?;
    let pre_compaction_scan = measure_empty_due_scan(&mut memory)?;
    let memory_compaction = measure_compaction(
        &mut memory,
        memory_directory.path(),
        DurabilityClass::Memory,
    )?;
    let post_compaction_scan = measure_empty_due_scan(&mut memory)?;
    if memory
        .compact_structure(DurabilityClass::Memory)?
        .commit
        .is_some()
    {
        return Err("second memory compaction was not a no-op".into());
    }

    let strict_directory = TemporaryDirectory::create("strict")?;
    let (mut strict, strict_digest) = prepare_dataset(
        strict_directory.path(),
        STRICT_EXPIRED_KEYS,
        STRICT_LIVE_KEYS,
    )?;
    let strict_compaction = measure_compaction(
        &mut strict,
        strict_directory.path(),
        DurabilityClass::Strict,
    )?;
    drop(strict);
    let mut strict = NativeDatabase::open(strict_directory.path())?;
    if strict
        .compact_structure(DurabilityClass::Strict)?
        .commit
        .is_some()
        || strict.get_latest_structure(&[b'l', 0, 0, 0, 0], DUE_AT)? != Some(vec![0; 32])
        || strict
            .get_latest_structure(&[b'e', 0, 0, 0, 0], i64::MIN)?
            .is_some()
    {
        return Err("strict compaction did not survive reopen".into());
    }

    println!("{{");
    println!("  \"schema\": \"hyphae-native-structure-compaction-smoke-v1\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"scan_warmup\": {SCAN_WARMUP},");
    println!("  \"scan_observations\": {SCAN_OBSERVATIONS},");
    println!("  \"memory_expired_keys\": {MEMORY_EXPIRED_KEYS},");
    println!("  \"memory_live_keys\": {MEMORY_LIVE_KEYS},");
    println!("  \"strict_expired_keys\": {STRICT_EXPIRED_KEYS},");
    println!("  \"strict_live_keys\": {STRICT_LIVE_KEYS},");
    println!("  \"memory_dataset_blake3\": \"{memory_digest}\",");
    println!("  \"strict_dataset_blake3\": \"{strict_digest}\",");
    println!("  \"metrics\": {{");
    print_scan("pre_compaction_empty_due_scan", pre_compaction_scan, true);
    print_compaction("memory_compaction", &memory_compaction, true);
    print_scan("post_compaction_empty_due_scan", post_compaction_scan, true);
    print_compaction("strict_compaction", &strict_compaction, false);
    println!("  }}");
    println!("}}");
    Ok(())
}
