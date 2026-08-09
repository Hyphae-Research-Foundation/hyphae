// SPDX-License-Identifier: Apache-2.0

//! Warm physical-read latency smoke for reverse native sorted-set ranges.

use std::{
    fs,
    hint::black_box,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, SortedSetEntry};
use hyphae_native_types::DurabilityClass;

const SORTED_SET_KEY: &[u8] = b"benchmark-sorted-set";
const MEMBERS: u32 = 2_048;
const MEMBER_BYTES: usize = 64;
const WARMUP: u32 = 1_000;
const OBSERVATIONS: u32 = 10_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-sorted-set-reverse-range-smoke-{}-{timestamp}",
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
    reverse_head: Stats,
    ascending_tail: Stats,
    reverse_score_middle: Stats,
    ascending_score_middle: Stats,
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

fn member(index: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut member = vec![u8::try_from(index % 251)?; MEMBER_BYTES];
    member[..4].copy_from_slice(&index.to_be_bytes());
    Ok(member)
}

fn validate_entries(
    entries: &[SortedSetEntry],
    first_index: u32,
    last_index: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let first_member = member(first_index)?;
    let last_member = member(last_index)?;
    if entries.len() != 10
        || entries
            .first()
            .is_none_or(|entry| entry.member() != first_member.as_slice())
        || entries
            .last()
            .is_none_or(|entry| entry.member() != last_member.as_slice())
    {
        return Err("physical reverse-range benchmark returned the wrong cohort".into());
    }
    Ok(())
}

fn warm(
    database: &NativeDatabase,
    score_lower: f64,
    score_upper: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(database.zrevrange_latest_sorted_set(black_box(SORTED_SET_KEY), 0, 9)?);
        black_box(database.zrange_latest_sorted_set(black_box(SORTED_SET_KEY), -10, -1)?);
        black_box(database.zrevrange_by_score_latest_sorted_set(
            black_box(SORTED_SET_KEY),
            black_box(Bound::Included(score_lower)),
            black_box(Bound::Excluded(score_upper)),
            0,
            10,
        )?);
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

fn prepare_database(
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

fn validate_routes(
    database: &NativeDatabase,
    score_lower_index: u32,
    score_upper_index: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let score_lower = f64::from(score_lower_index);
    let score_upper = f64::from(score_upper_index);
    let reverse_head = database.zrevrange_latest_sorted_set(SORTED_SET_KEY, 0, 9)?;
    validate_entries(&reverse_head, MEMBERS - 1, MEMBERS - 10)?;
    let ascending_tail = database.zrange_latest_sorted_set(SORTED_SET_KEY, -10, -1)?;
    validate_entries(&ascending_tail, MEMBERS - 10, MEMBERS - 1)?;
    let reverse_score = database.zrevrange_by_score_latest_sorted_set(
        SORTED_SET_KEY,
        Bound::Included(score_lower),
        Bound::Excluded(score_upper),
        0,
        10,
    )?;
    validate_entries(&reverse_score, score_upper_index - 1, score_lower_index)?;
    let ascending_score = database.zrange_by_score_latest_sorted_set(
        SORTED_SET_KEY,
        Bound::Included(score_lower),
        Bound::Excluded(score_upper),
        0,
        10,
    )?;
    validate_entries(&ascending_score, score_lower_index, score_upper_index - 1)?;
    Ok(())
}

fn measure_routes(
    database: &NativeDatabase,
    score_lower: f64,
    score_upper: f64,
) -> Result<RouteStats, Box<dyn std::error::Error>> {
    let zrevrange_head_10 = measure(|| {
        black_box(database.zrevrange_latest_sorted_set(black_box(SORTED_SET_KEY), 0, 9)?);
        Ok(())
    })?;
    let zrange_tail_10 = measure(|| {
        black_box(database.zrange_latest_sorted_set(black_box(SORTED_SET_KEY), -10, -1)?);
        Ok(())
    })?;
    let zrevrange_by_score_middle_10 = measure(|| {
        black_box(database.zrevrange_by_score_latest_sorted_set(
            black_box(SORTED_SET_KEY),
            black_box(Bound::Included(score_lower)),
            black_box(Bound::Excluded(score_upper)),
            0,
            10,
        )?);
        Ok(())
    })?;
    let zrange_by_score_middle_10 = measure(|| {
        black_box(database.zrange_by_score_latest_sorted_set(
            black_box(SORTED_SET_KEY),
            black_box(Bound::Included(score_lower)),
            black_box(Bound::Excluded(score_upper)),
            0,
            10,
        )?);
        Ok(())
    })?;
    Ok(RouteStats {
        reverse_head: zrevrange_head_10,
        ascending_tail: zrange_tail_10,
        reverse_score_middle: zrevrange_by_score_middle_10,
        ascending_score_middle: zrange_by_score_middle_10,
    })
}

fn print_report(
    database: &NativeDatabase,
    dataset_digest: blake3::Hash,
    routes: &RouteStats,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-sorted-set-reverse-range-smoke-v1\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"members\": {MEMBERS},");
    println!("  \"member_bytes\": {MEMBER_BYTES},");
    println!("  \"dataset_blake3\": \"{}\",", dataset_digest.to_hex());
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("zrevrange_head_10_physical", routes.reverse_head, true);
    print_stats("zrange_tail_10_physical", routes.ascending_tail, true);
    print_stats(
        "zrevrange_by_score_middle_10_physical",
        routes.reverse_score_middle,
        true,
    );
    print_stats(
        "zrange_by_score_middle_10_physical",
        routes.ascending_score_middle,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let (database, dataset_digest) = prepare_database(temporary.path())?;
    let score_lower_index = MEMBERS / 2;
    let score_upper_index = score_lower_index + 10;
    validate_routes(&database, score_lower_index, score_upper_index)?;
    let score_lower = f64::from(score_lower_index);
    let score_upper = f64::from(score_upper_index);
    warm(&database, score_lower, score_upper)?;
    let routes = measure_routes(&database, score_lower, score_upper)?;
    print_report(&database, dataset_digest, &routes)?;
    Ok(())
}
