// SPDX-License-Identifier: Apache-2.0

//! Reproducible strict-versus-group native commit observation.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    GroupCommitConfig, NativeCommitScheduler, NativeDatabase, ScheduledCommitReceipt,
};
use hyphae_native_types::DurabilityClass;

const PRODUCERS: usize = 8;
const ROUNDS_PER_PRODUCER: usize = 32;
const TOTAL_COMMITS: usize = PRODUCERS * ROUNDS_PER_PRODUCER;

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
}

struct CohortObservation {
    size: usize,
    execution_nanos: u64,
    page_sync_nanos: u64,
    wal_sync_nanos: u64,
}

struct CohortSummary {
    execution: LatencyStats,
    page_sync: LatencyStats,
    wal_sync: LatencyStats,
    count: usize,
    size_min: usize,
    size_max: usize,
    size_mean: f64,
}

struct Observation {
    source_commit: String,
    source_tree: String,
    rustc: String,
    strict_wall_nanos: u64,
    strict_latency: LatencyStats,
    group_wall_nanos: u64,
    group_end_to_end: LatencyStats,
    group_admission_wait: LatencyStats,
    group_queue_wait: LatencyStats,
    cohort_execution: LatencyStats,
    cohort_page_sync: LatencyStats,
    cohort_wal_sync: LatencyStats,
    cohort_count: usize,
    cohort_size_min: usize,
    cohort_size_max: usize,
    cohort_size_mean: f64,
}

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-group-commit-{label}-{}-{timestamp}",
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

fn percentile(sorted: &[u64], numerator: usize, denominator: usize) -> u64 {
    let index = sorted
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(sorted.len().saturating_sub(1));
    sorted[index]
}

fn latency_stats(mut observations: Vec<u64>) -> LatencyStats {
    observations.sort_unstable();
    LatencyStats {
        p50: percentile(&observations, 50, 100),
        p95: percentile(&observations, 95, 100),
        p99: percentile(&observations, 99, 100),
    }
}

fn key(sequence: usize) -> Vec<u8> {
    format!("commit-key-{sequence:05}").into_bytes()
}

fn nanos(started: Instant) -> Result<u64, BenchmarkError> {
    u64::try_from(started.elapsed().as_nanos()).map_err(Into::into)
}

fn strict_observation(path: &Path) -> Result<(u64, LatencyStats), BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let wall_started = Instant::now();
    let mut latencies = Vec::with_capacity(TOTAL_COMMITS);
    for sequence in 0..TOTAL_COMMITS {
        let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
        batch.set(key(sequence), sequence.to_string().into_bytes(), None)?;
        let started = Instant::now();
        database.commit_optimistic(batch)?;
        latencies.push(nanos(started)?);
    }
    let wall_nanos = nanos(wall_started)?;
    verify(&database)?;
    drop(database);
    verify(&NativeDatabase::open(path)?)?;
    Ok((wall_nanos, latency_stats(latencies)))
}

fn group_observation(path: &Path) -> Result<(u64, Vec<ScheduledCommitReceipt>), BenchmarkError> {
    let database = NativeDatabase::create(path)?;
    let scheduler = NativeCommitScheduler::start(
        database,
        GroupCommitConfig::new(PRODUCERS, std::time::Duration::from_millis(10), 64)?,
    )?;
    let clients = (0..PRODUCERS)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(PRODUCERS));
    let wall_started = Instant::now();
    let receipts = std::thread::scope(|scope| {
        let handles = clients
            .into_iter()
            .enumerate()
            .map(|(producer, client)| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(
                    move || -> Result<Vec<ScheduledCommitReceipt>, BenchmarkError> {
                        let mut receipts = Vec::with_capacity(ROUNDS_PER_PRODUCER);
                        for round in 0..ROUNDS_PER_PRODUCER {
                            let sequence = producer
                                .checked_mul(ROUNDS_PER_PRODUCER)
                                .and_then(|base| base.checked_add(round))
                                .ok_or("benchmark sequence overflow")?;
                            let mut batch = client.begin_optimistic(0, DurabilityClass::Group)?;
                            batch.set(key(sequence), sequence.to_string().into_bytes(), None)?;
                            barrier.wait();
                            receipts.push(client.submit(batch)?);
                        }
                        Ok(receipts)
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut receipts = Vec::with_capacity(TOTAL_COMMITS);
        for handle in handles {
            receipts.extend(
                handle
                    .join()
                    .map_err(|_| std::io::Error::other("group producer panicked"))??,
            );
        }
        Ok::<_, BenchmarkError>(receipts)
    })?;
    let wall_nanos = nanos(wall_started)?;
    scheduler.shutdown()?;
    verify(&NativeDatabase::open(path)?)?;
    Ok((wall_nanos, receipts))
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    for sequence in 0..TOTAL_COMMITS {
        let expected = sequence.to_string().into_bytes();
        if database.get_latest_structure(&key(sequence), 0)? != Some(expected) {
            return Err(format!("missing committed key {sequence}").into());
        }
    }
    Ok(())
}

fn summarize_cohorts(receipts: &[ScheduledCommitReceipt]) -> Result<CohortSummary, BenchmarkError> {
    let mut cohorts = BTreeMap::new();
    for receipt in receipts {
        let start_csn = receipt
            .commit_csn
            .get()
            .checked_sub(u64::try_from(receipt.durability_cohort_position)?)
            .ok_or("invalid cohort position")?;
        cohorts.entry(start_csn).or_insert(CohortObservation {
            size: receipt.durability_cohort_size,
            execution_nanos: u64::try_from(receipt.cohort_execution.as_nanos())?,
            page_sync_nanos: u64::try_from(receipt.page_synchronization.as_nanos())?,
            wal_sync_nanos: u64::try_from(receipt.wal_synchronization.as_nanos())?,
        });
    }
    let sizes = cohorts
        .values()
        .map(|cohort| cohort.size)
        .collect::<Vec<_>>();
    let size_total = sizes.iter().try_fold(0_u64, |total, size| {
        total
            .checked_add(u64::try_from(*size)?)
            .ok_or_else(|| -> BenchmarkError { "cohort-size sum overflow".into() })
    })?;
    let cohort_count = cohorts.len();
    let size_total = u32::try_from(size_total)?;
    let cohort_count_u32 = u32::try_from(cohort_count)?;
    let size_mean = f64::from(size_total) / f64::from(cohort_count_u32);
    Ok(CohortSummary {
        execution: latency_stats(
            cohorts
                .values()
                .map(|cohort| cohort.execution_nanos)
                .collect(),
        ),
        page_sync: latency_stats(
            cohorts
                .values()
                .map(|cohort| cohort.page_sync_nanos)
                .collect(),
        ),
        wal_sync: latency_stats(
            cohorts
                .values()
                .map(|cohort| cohort.wal_sync_nanos)
                .collect(),
        ),
        count: cohort_count,
        size_min: *sizes.iter().min().ok_or("no group cohorts observed")?,
        size_max: *sizes.iter().max().ok_or("no group cohorts observed")?,
        size_mean,
    })
}

fn throughput(commits: usize, wall_nanos: u64) -> Result<f64, BenchmarkError> {
    Ok(f64::from(u32::try_from(commits)?)
        / std::time::Duration::from_nanos(wall_nanos).as_secs_f64())
}

fn print_latency(name: &str, stats: LatencyStats, trailing_comma: bool) {
    println!("  \"{name}\": {{");
    println!("    \"p50_nanos\": {},", stats.p50);
    println!("    \"p95_nanos\": {},", stats.p95);
    println!("    \"p99_nanos\": {}", stats.p99);
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

fn print_observation(observation: &Observation) -> Result<(), BenchmarkError> {
    let strict_throughput = throughput(TOTAL_COMMITS, observation.strict_wall_nanos)?;
    let group_throughput = throughput(TOTAL_COMMITS, observation.group_wall_nanos)?;
    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-group-commit-v1\",");
    println!("  \"source_commit\": \"{}\",", observation.source_commit);
    println!("  \"source_tree\": \"{}\",", observation.source_tree);
    println!("  \"rustc\": \"{}\",", observation.rustc);
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"commits\": {TOTAL_COMMITS},");
    println!("  \"producers\": {PRODUCERS},");
    println!(
        "  \"strict_wall_nanos\": {},",
        observation.strict_wall_nanos
    );
    println!("  \"strict_commits_per_second\": {strict_throughput:.3},");
    print_latency("strict_commit_latency", observation.strict_latency, true);
    println!("  \"group_wall_nanos\": {},", observation.group_wall_nanos);
    println!("  \"group_commits_per_second\": {group_throughput:.3},");
    println!(
        "  \"group_throughput_over_strict\": {:.6},",
        group_throughput / strict_throughput
    );
    print_latency("group_end_to_end", observation.group_end_to_end, true);
    print_latency(
        "group_admission_wait",
        observation.group_admission_wait,
        true,
    );
    print_latency("group_queue_wait", observation.group_queue_wait, true);
    print_latency("cohort_execution", observation.cohort_execution, true);
    print_latency("cohort_page_sync", observation.cohort_page_sync, true);
    print_latency("cohort_wal_sync", observation.cohort_wal_sync, true);
    println!("  \"cohort_count\": {},", observation.cohort_count);
    println!("  \"cohort_size_min\": {},", observation.cohort_size_min);
    println!("  \"cohort_size_max\": {},", observation.cohort_size_max);
    println!(
        "  \"cohort_size_mean\": {:.6},",
        observation.cohort_size_mean
    );
    println!("  \"strict_reopen_verified\": true,");
    println!("  \"group_reopen_verified\": true");
    println!("}}");
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let source_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let source_tree = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "unknown".to_owned());
    let strict_directory = TemporaryDirectory::create("strict")?;
    let group_directory = TemporaryDirectory::create("group")?;
    let (strict_wall_nanos, strict_latency) = strict_observation(strict_directory.path())?;
    let (group_wall_nanos, receipts) = group_observation(group_directory.path())?;
    let cohorts = summarize_cohorts(&receipts)?;

    print_observation(&Observation {
        source_commit,
        source_tree,
        rustc,
        strict_wall_nanos,
        strict_latency,
        group_wall_nanos,
        group_end_to_end: latency_stats(
            receipts
                .iter()
                .map(|receipt| u64::try_from(receipt.end_to_end.as_nanos()))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        group_admission_wait: latency_stats(
            receipts
                .iter()
                .map(|receipt| u64::try_from(receipt.admission_wait.as_nanos()))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        group_queue_wait: latency_stats(
            receipts
                .iter()
                .map(|receipt| u64::try_from(receipt.queue_wait.as_nanos()))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        cohort_execution: cohorts.execution,
        cohort_page_sync: cohorts.page_sync,
        cohort_wal_sync: cohorts.wal_sync,
        cohort_count: cohorts.count,
        cohort_size_min: cohorts.size_min,
        cohort_size_max: cohorts.size_max,
        cohort_size_mean: cohorts.size_mean,
    })?;
    Ok(())
}
