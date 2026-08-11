// SPDX-License-Identifier: AGPL-3.0-only

//! Native whole-set lifecycle latency smoke with durability separated.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const DEFAULT_OBSERVATIONS: usize = 31;
const MEMBER_COUNTS: [u32; 3] = [0, 64, 2_048];

type BenchmarkError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-set-lifecycle-{label}-{}-{timestamp}",
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
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

struct Observation {
    members: u32,
    private_delete: Stats,
    memory_commit: Stats,
    strict_commit: Stats,
}

fn observations() -> Result<usize, BenchmarkError> {
    match std::env::var("HYPHAE_SET_LIFECYCLE_OBSERVATIONS") {
        Ok(value) => {
            let parsed = value.parse::<usize>()?;
            if parsed < 2 {
                return Err("HYPHAE_SET_LIFECYCLE_OBSERVATIONS must be at least 2".into());
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_OBSERVATIONS),
        Err(error) => Err(error.into()),
    }
}

fn nanos(duration: Duration) -> Result<u64, BenchmarkError> {
    Ok(u64::try_from(duration.as_nanos())?)
}

fn summarize(mut samples: Vec<u64>) -> Result<Stats, BenchmarkError> {
    if samples.is_empty() {
        return Err("missing latency observations".into());
    }
    samples.sort_unstable();
    let percentile = |hundredths: usize| samples[(samples.len() - 1) * hundredths / 100];
    Ok(Stats {
        p50: percentile(50),
        p95: percentile(95),
        p99: percentile(99),
        max: *samples.last().ok_or("missing maximum")?,
    })
}

fn set_key(label: &str, index: usize) -> Vec<u8> {
    format!("{label}-{index:04}").into_bytes()
}

fn seed_set(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    key: &[u8],
    members: u32,
) -> Result<(), BenchmarkError> {
    transaction.create_set(key.to_vec())?;
    if members != 0 {
        transaction.sadd_many(
            key.to_vec(),
            (0..members)
                .map(|index| index.to_be_bytes().to_vec())
                .collect(),
        )?;
    }
    Ok(())
}

fn prepare_private_database(path: &Path, members: u32) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed_set(&mut seed, b"private", members)?;
    seed.commit()?;
    Ok(database)
}

fn measure_private_delete(members: u32, count: usize) -> Result<Stats, BenchmarkError> {
    let temporary = TemporaryDirectory::create("private")?;
    let database = prepare_private_database(temporary.path(), members)?;
    let mut samples = Vec::with_capacity(count);
    for logical_time in 0..count {
        let mut batch = database.begin_optimistic(
            i64::try_from(logical_time)?.saturating_add(2),
            DurabilityClass::Memory,
        )?;
        let started = Instant::now();
        if !batch.delete_set(b"private".to_vec())? {
            return Err("private set unexpectedly missing".into());
        }
        samples.push(nanos(started.elapsed())?);
        batch.rollback();
    }
    summarize(samples)
}

fn prepare_commit_database(
    path: &Path,
    label: &str,
    members: u32,
    count: usize,
) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    for index in 0..count {
        seed_set(&mut seed, &set_key(label, index), members)?;
    }
    seed.commit()?;
    Ok(database)
}

fn measure_commits(
    members: u32,
    count: usize,
    durability: DurabilityClass,
    label: &str,
) -> Result<Stats, BenchmarkError> {
    let temporary = TemporaryDirectory::create(label)?;
    let mut database = prepare_commit_database(temporary.path(), label, members, count)?;
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let mut transaction =
            database.begin(i64::try_from(index)?.saturating_add(2), durability)?;
        if !transaction.delete_set(set_key(label, index))? {
            return Err("commit set unexpectedly missing".into());
        }
        let started = Instant::now();
        transaction.commit()?;
        samples.push(nanos(started.elapsed())?);
    }
    drop(database);
    let reopened = NativeDatabase::open(temporary.path())?;
    if reopened.recovery_report().committed_transactions != count.saturating_add(1) {
        return Err("reopened commit count does not match the benchmark corpus".into());
    }
    summarize(samples)
}

fn observe(members: u32, count: usize) -> Result<Observation, BenchmarkError> {
    Ok(Observation {
        members,
        private_delete: measure_private_delete(members, count)?,
        memory_commit: measure_commits(members, count, DurabilityClass::Memory, "memory")?,
        strict_commit: measure_commits(members, count, DurabilityClass::Strict, "strict")?,
    })
}

fn print_stats(name: &str, stats: Stats, trailing_comma: bool) {
    println!(
        "      \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"max_nanos\": {}}}{}",
        stats.p50,
        stats.p95,
        stats.p99,
        stats.max,
        if trailing_comma { "," } else { "" }
    );
}

fn print_report(count: usize, results: &[Observation]) {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-set-lifecycle-smoke-v1\",");
    println!("  \"mode\": \"release-concurrency-1\",");
    println!("  \"observations_per_route\": {count},");
    println!("  \"member_encoding\": \"u32-big-endian\",");
    println!("  \"metrics\": [");
    for (index, result) in results.iter().enumerate() {
        println!("    {{");
        println!("      \"members\": {},", result.members);
        print_stats("private_delete_set", result.private_delete, true);
        print_stats("memory_commit", result.memory_commit, true);
        print_stats("strict_commit", result.strict_commit, false);
        println!(
            "    }}{}",
            if index + 1 == results.len() { "" } else { "," }
        );
    }
    println!("  ]");
    println!("}}");
}

fn main() -> Result<(), BenchmarkError> {
    let count = observations()?;
    let results = MEMBER_COUNTS
        .into_iter()
        .map(|members| observe(members, count))
        .collect::<Result<Vec<_>, _>>()?;
    print_report(count, &results);
    Ok(())
}
