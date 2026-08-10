// SPDX-License-Identifier: GPL-3.0-only

//! Focused warm physical-read latency smoke for native sorted sets.

use std::{
    fs,
    hint::black_box,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const SORTED_SET_KEY: &[u8] = b"benchmark-sorted-set";
const MEMBERS: u32 = 2_048;
const MEMBER_BYTES: usize = 64;
const WARMUP: u32 = 10_000;
const OBSERVATIONS: u32 = 100_000;
const POINT_OPERATIONS_PER_OBSERVATION: u32 = 16;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-sorted-set-smoke-{}-{timestamp}",
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

fn measure(
    mut operation: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
    observations: u32,
    operations_per_observation: u32,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let capacity = usize::try_from(observations)?;
    let mut samples = Vec::with_capacity(capacity);
    let total_started = Instant::now();
    for _ in 0..observations {
        let started = Instant::now();
        for _ in 0..operations_per_observation {
            operation()?;
        }
        let nanos =
            u64::try_from(started.elapsed().as_nanos())? / u64::from(operations_per_observation);
        samples.push(nanos);
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
        throughput_per_second: f64::from(observations) * f64::from(operations_per_observation)
            / elapsed,
    })
}

fn member(index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut member = vec![u8::try_from(index % 251)?; MEMBER_BYTES];
    member[..4].copy_from_slice(&index.to_be_bytes());
    Ok(member)
}

fn warm(database: &NativeDatabase, middle_member: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let score_lower = f64::from(MEMBERS / 2);
    let score_upper = f64::from(MEMBERS / 2 + 10);
    for _ in 0..WARMUP {
        black_box(database.zcard_latest_sorted_set(black_box(SORTED_SET_KEY))?);
        black_box(
            database
                .zscore_latest_sorted_set(black_box(SORTED_SET_KEY), black_box(middle_member))?,
        );
        black_box(database.zrange_latest_sorted_set(black_box(SORTED_SET_KEY), 0, 9)?);
        black_box(database.zrange_by_score_latest_sorted_set(
            black_box(SORTED_SET_KEY),
            black_box(Bound::Included(score_lower)),
            black_box(Bound::Excluded(score_upper)),
            0,
            10,
        )?);
    }
    Ok(())
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

fn validate_score_range(
    database: &NativeDatabase,
    middle_member: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let score_range = database.zrange_by_score_latest_sorted_set(
        SORTED_SET_KEY,
        Bound::Included(f64::from(MEMBERS / 2)),
        Bound::Excluded(f64::from(MEMBERS / 2 + 10)),
        0,
        10,
    )?;
    if score_range.len() != 10
        || score_range
            .first()
            .is_none_or(|entry| entry.member() != middle_member)
        || score_range
            .last()
            .is_none_or(|entry| entry.score().to_bits() != f64::from(MEMBERS / 2 + 9).to_bits())
    {
        return Err("physical score range returned the wrong benchmark cohort".into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
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

    let database = NativeDatabase::open(temporary.path())?;
    if database.zcard_latest_sorted_set(SORTED_SET_KEY)? != usize::try_from(MEMBERS)? {
        return Err("reopened benchmark sorted-set cardinality is wrong".into());
    }
    let middle_member = member(MEMBERS / 2)?;
    validate_score_range(&database, &middle_member)?;
    warm(&database, &middle_member)?;
    let zcard = measure(
        || {
            black_box(database.zcard_latest_sorted_set(black_box(SORTED_SET_KEY))?);
            Ok(())
        },
        OBSERVATIONS,
        POINT_OPERATIONS_PER_OBSERVATION,
    )?;
    let zscore_middle =
        measure(
            || {
                black_box(database.zscore_latest_sorted_set(
                    black_box(SORTED_SET_KEY),
                    black_box(&middle_member),
                )?);
                Ok(())
            },
            OBSERVATIONS,
            POINT_OPERATIONS_PER_OBSERVATION,
        )?;
    let zrange_head_10 = measure(
        || {
            black_box(database.zrange_latest_sorted_set(black_box(SORTED_SET_KEY), 0, 9)?);
            Ok(())
        },
        OBSERVATIONS,
        1,
    )?;
    let score_lower = f64::from(MEMBERS / 2);
    let score_upper = f64::from(MEMBERS / 2 + 10);
    let zrange_by_score_middle_10 = measure(
        || {
            black_box(database.zrange_by_score_latest_sorted_set(
                black_box(SORTED_SET_KEY),
                black_box(Bound::Included(score_lower)),
                black_box(Bound::Excluded(score_upper)),
                0,
                10,
            )?);
            Ok(())
        },
        OBSERVATIONS,
        1,
    )?;

    println!("{{");
    println!("  \"schema\": \"hyphae-native-sorted-set-smoke-v2\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"members\": {MEMBERS},");
    println!("  \"member_bytes\": {MEMBER_BYTES},");
    println!(
        "  \"dataset_blake3\": \"{}\",",
        dataset_hasher.finalize().to_hex()
    );
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("zcard_physical", zcard, true);
    print_stats("zscore_middle_physical", zscore_middle, true);
    print_stats("zrange_head_10_physical", zrange_head_10, true);
    print_stats(
        "zrange_by_score_middle_10_physical",
        zrange_by_score_middle_10,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
