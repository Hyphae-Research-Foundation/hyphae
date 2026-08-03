// SPDX-License-Identifier: Apache-2.0

//! Focused warm physical-read latency smoke for native sorted-set ranks.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const SORTED_SET_KEY: &[u8] = b"benchmark-sorted-set-ranks";
const MEMBERS: u32 = 2_048;
const MEMBER_BYTES: usize = 64;
const WARMUP: u32 = 1_000;
const OBSERVATIONS: u32 = 10_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-sorted-set-rank-smoke-{}-{timestamp}",
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

struct RankMetrics {
    zrank_head: Stats,
    zrevrank_tail: Stats,
    zrank_middle: Stats,
    zrevrank_middle: Stats,
    zrank_tail: Stats,
    zrevrank_head: Stats,
}

fn measure_rank(
    mut operation: impl FnMut() -> Result<Option<usize>, Box<dyn std::error::Error>>,
    expected: usize,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(usize::try_from(OBSERVATIONS)?);
    let total_started = Instant::now();
    for _ in 0..OBSERVATIONS {
        let started = Instant::now();
        let actual = black_box(operation()?);
        if actual != Some(expected) {
            return Err(format!("rank mismatch: expected {expected}, found {actual:?}").into());
        }
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

fn member(index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut member = vec![u8::try_from(index % 251)?; MEMBER_BYTES];
    member[..4].copy_from_slice(&index.to_be_bytes());
    Ok(member)
}

fn seed_and_reopen(
    path: &Path,
) -> Result<(NativeDatabase, blake3::Hash), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(100, DurabilityClass::Strict)?;
    seed.create_sorted_set(SORTED_SET_KEY.to_vec())?;
    let mut dataset_hasher = blake3::Hasher::new();
    for index in 0..MEMBERS {
        let member = member(index)?;
        dataset_hasher.update(&member);
        dataset_hasher.update(&f64::from(index).to_bits().to_be_bytes());
        seed.zadd(SORTED_SET_KEY.to_vec(), f64::from(index), member)?;
    }
    seed.commit()?;
    drop(database);

    let database = NativeDatabase::open(path)?;
    if database.zcard_latest_sorted_set(SORTED_SET_KEY)? != usize::try_from(MEMBERS)? {
        return Err("reopened benchmark sorted-set cardinality is wrong".into());
    }
    Ok((database, dataset_hasher.finalize()))
}

fn warm(
    database: &NativeDatabase,
    head: &[u8],
    middle: &[u8],
    tail: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(database.zrank_latest_sorted_set(SORTED_SET_KEY, head)?);
        black_box(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, tail)?);
        black_box(database.zrank_latest_sorted_set(SORTED_SET_KEY, middle)?);
        black_box(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, middle)?);
        black_box(database.zrank_latest_sorted_set(SORTED_SET_KEY, tail)?);
        black_box(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, head)?);
    }
    Ok(())
}

fn measure_all(
    database: &NativeDatabase,
    head: &[u8],
    middle: &[u8],
    tail: &[u8],
) -> Result<RankMetrics, Box<dyn std::error::Error>> {
    let last = usize::try_from(MEMBERS - 1)?;
    let middle_rank = usize::try_from(MEMBERS / 2)?;
    Ok(RankMetrics {
        zrank_head: measure_rank(
            || Ok(database.zrank_latest_sorted_set(SORTED_SET_KEY, head)?),
            0,
        )?,
        zrevrank_tail: measure_rank(
            || Ok(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, tail)?),
            0,
        )?,
        zrank_middle: measure_rank(
            || Ok(database.zrank_latest_sorted_set(SORTED_SET_KEY, middle)?),
            middle_rank,
        )?,
        zrevrank_middle: measure_rank(
            || Ok(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, middle)?),
            last - middle_rank,
        )?,
        zrank_tail: measure_rank(
            || Ok(database.zrank_latest_sorted_set(SORTED_SET_KEY, tail)?),
            last,
        )?,
        zrevrank_head: measure_rank(
            || Ok(database.zrevrank_latest_sorted_set(SORTED_SET_KEY, head)?),
            last,
        )?,
    })
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

fn print_receipt(
    database: &NativeDatabase,
    dataset: blake3::Hash,
    metrics: &RankMetrics,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-sorted-set-rank-smoke-v1\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"members\": {MEMBERS},");
    println!("  \"member_bytes\": {MEMBER_BYTES},");
    println!("  \"dataset_blake3\": \"{}\",", dataset.to_hex());
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("zrank_head_physical", metrics.zrank_head, true);
    print_stats("zrevrank_tail_physical", metrics.zrevrank_tail, true);
    print_stats("zrank_middle_physical", metrics.zrank_middle, true);
    print_stats("zrevrank_middle_physical", metrics.zrevrank_middle, true);
    print_stats("zrank_tail_physical", metrics.zrank_tail, true);
    print_stats("zrevrank_head_physical", metrics.zrevrank_head, false);
    println!("  }}");
    println!("}}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let (database, dataset) = seed_and_reopen(temporary.path())?;
    let head = member(0)?;
    let middle = member(MEMBERS / 2)?;
    let tail = member(MEMBERS - 1)?;
    warm(&database, &head, &middle, &tail)?;
    let metrics = measure_all(&database, &head, &middle, &tail)?;
    print_receipt(&database, dataset, &metrics)?;
    Ok(())
}
