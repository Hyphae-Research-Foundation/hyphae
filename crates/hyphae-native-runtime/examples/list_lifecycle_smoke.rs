// SPDX-License-Identifier: AGPL-3.0-only

//! Native whole-list lifecycle latency smoke with durability separated.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const DEFAULT_OBSERVATIONS: usize = 31;
const ELEMENT_COUNTS: [u32; 3] = [0, 64, 2_048];

type BenchmarkError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-list-lifecycle-{label}-{}-{timestamp}",
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
    elements: u32,
    private_delete: Stats,
    memory_commit: Stats,
    strict_commit: Stats,
}

fn observations() -> Result<usize, BenchmarkError> {
    match std::env::var("HYPHAE_LIST_LIFECYCLE_OBSERVATIONS") {
        Ok(value) => {
            let parsed = value.parse::<usize>()?;
            if parsed < 2 {
                return Err("HYPHAE_LIST_LIFECYCLE_OBSERVATIONS must be at least 2".into());
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

fn list_key(label: &str, index: usize) -> Vec<u8> {
    format!("{label}-{index:04}").into_bytes()
}

fn seed_list(
    transaction: &mut hyphae_native_runtime::NativeWriteBatch,
    key: &[u8],
    elements: u32,
) -> Result<(), BenchmarkError> {
    transaction.create_list(key.to_vec())?;
    for index in 0..elements {
        transaction.rpush(key.to_vec(), index.to_be_bytes().to_vec())?;
    }
    Ok(())
}

fn prepare_private_database(path: &Path, elements: u32) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed_list(&mut seed, b"private", elements)?;
    seed.commit()?;
    Ok(database)
}

fn measure_private_delete(elements: u32, count: usize) -> Result<Stats, BenchmarkError> {
    let temporary = TemporaryDirectory::create("private")?;
    let database = prepare_private_database(temporary.path(), elements)?;
    let mut samples = Vec::with_capacity(count);
    for logical_time in 0..count {
        let mut batch = database.begin_optimistic(
            i64::try_from(logical_time)?.saturating_add(2),
            DurabilityClass::Memory,
        )?;
        let started = Instant::now();
        if !batch.delete_list(b"private".to_vec())? {
            return Err("private list unexpectedly missing".into());
        }
        samples.push(nanos(started.elapsed())?);
        batch.rollback();
    }
    summarize(samples)
}

fn prepare_commit_database(
    path: &Path,
    label: &str,
    elements: u32,
    count: usize,
) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    for index in 0..count {
        seed_list(&mut seed, &list_key(label, index), elements)?;
    }
    seed.commit()?;
    Ok(database)
}

fn measure_commits(
    elements: u32,
    count: usize,
    durability: DurabilityClass,
    label: &str,
) -> Result<Stats, BenchmarkError> {
    let temporary = TemporaryDirectory::create(label)?;
    let mut database = prepare_commit_database(temporary.path(), label, elements, count)?;
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let mut transaction =
            database.begin(i64::try_from(index)?.saturating_add(2), durability)?;
        if !transaction.delete_list(list_key(label, index))? {
            return Err("commit list unexpectedly missing".into());
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

fn observe(elements: u32, count: usize) -> Result<Observation, BenchmarkError> {
    Ok(Observation {
        elements,
        private_delete: measure_private_delete(elements, count)?,
        memory_commit: measure_commits(elements, count, DurabilityClass::Memory, "memory")?,
        strict_commit: measure_commits(elements, count, DurabilityClass::Strict, "strict")?,
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
    println!("  \"schema\": \"hyphae-native-list-lifecycle-smoke-v1\",");
    println!("  \"mode\": \"release-concurrency-1\",");
    println!("  \"observations_per_route\": {count},");
    println!("  \"element_encoding\": \"u32-big-endian\",");
    println!("  \"metrics\": [");
    for (index, result) in results.iter().enumerate() {
        println!("    {{");
        println!("      \"elements\": {},", result.elements);
        print_stats("private_delete_list", result.private_delete, true);
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
    let results = ELEMENT_COUNTS
        .into_iter()
        .map(|elements| observe(elements, count))
        .collect::<Result<Vec<_>, _>>()?;
    print_report(count, &results);
    Ok(())
}
