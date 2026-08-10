// SPDX-License-Identifier: GPL-3.0-only

//! Reproducible mixed-durability native scheduler observation.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    GroupCommitConfig, NativeCommitScheduler, NativeDatabase, ScheduledCommitReceipt,
};
use hyphae_native_types::DurabilityClass;

const GROUP_PRODUCERS: usize = 6;
const STRICT_PRODUCERS: usize = 1;
const MEMORY_PRODUCERS: usize = 1;
const PRODUCERS: usize = GROUP_PRODUCERS + STRICT_PRODUCERS + MEMORY_PRODUCERS;
const ROUNDS_PER_PRODUCER: usize = 32;
const TOTAL_COMMITS: usize = PRODUCERS * ROUNDS_PER_PRODUCER;

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

struct ClassSummary {
    count: usize,
    end_to_end: LatencyStats,
    admission_wait: LatencyStats,
    queue_wait: LatencyStats,
    execution: LatencyStats,
    page_sync: LatencyStats,
    wal_sync: LatencyStats,
}

struct Observation {
    source_commit: String,
    source_tree: String,
    rustc: String,
    wall_nanos: u64,
    receipts: Vec<ScheduledCommitReceipt>,
    final_fence: ScheduledCommitReceipt,
}

impl TemporaryDirectory {
    fn create() -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-mixed-scheduler-{}-{timestamp}",
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

fn durability_for_producer(producer: usize) -> DurabilityClass {
    if producer < GROUP_PRODUCERS {
        DurabilityClass::Group
    } else if producer < GROUP_PRODUCERS + STRICT_PRODUCERS {
        DurabilityClass::Strict
    } else {
        DurabilityClass::Memory
    }
}

fn key(sequence: usize) -> Vec<u8> {
    format!("mixed-key-{sequence:05}").into_bytes()
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

fn latency_stats(mut observations: Vec<u64>) -> Result<LatencyStats, BenchmarkError> {
    observations.sort_unstable();
    let max = *observations.last().ok_or("no latency observations")?;
    Ok(LatencyStats {
        p50: percentile(&observations, 50, 100),
        p95: percentile(&observations, 95, 100),
        p99: percentile(&observations, 99, 100),
        max,
    })
}

fn duration_nanos(duration: Duration) -> Result<u64, BenchmarkError> {
    u64::try_from(duration.as_nanos()).map_err(Into::into)
}

fn run_workload(path: &Path) -> Result<Observation, BenchmarkError> {
    let database = NativeDatabase::create(path)?;
    let scheduler = NativeCommitScheduler::start(
        database,
        GroupCommitConfig::new(16, Duration::from_micros(200), 64)?,
    )?;
    let clients = (0..PRODUCERS)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(PRODUCERS));
    let wall_started = Instant::now();
    let receipts = run_producers(clients, &barrier)?;
    let wall_nanos = duration_nanos(wall_started.elapsed())?;

    let mut fence = scheduler.begin_optimistic(1, DurabilityClass::Strict)?;
    fence.set(b"mixed-final-fence".to_vec(), b"durable".to_vec(), None)?;
    let final_fence = scheduler.submit(fence)?;
    scheduler.shutdown()?;
    verify(&NativeDatabase::open(path)?)?;

    Ok(Observation {
        source_commit: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        source_tree: std::env::args()
            .nth(2)
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        rustc: std::env::args()
            .nth(3)
            .unwrap_or_else(|| "unknown".to_owned()),
        wall_nanos,
        receipts,
        final_fence,
    })
}

fn run_producers(
    clients: Vec<hyphae_native_runtime::NativeCommitClient>,
    barrier: &Arc<Barrier>,
) -> Result<Vec<ScheduledCommitReceipt>, BenchmarkError> {
    std::thread::scope(|scope| {
        let handles = clients
            .into_iter()
            .enumerate()
            .map(|(producer, client)| {
                let barrier = Arc::clone(barrier);
                scope.spawn(
                    move || -> Result<Vec<ScheduledCommitReceipt>, BenchmarkError> {
                        let durability = durability_for_producer(producer);
                        let mut receipts = Vec::with_capacity(ROUNDS_PER_PRODUCER);
                        for round in 0..ROUNDS_PER_PRODUCER {
                            let sequence = producer
                                .checked_mul(ROUNDS_PER_PRODUCER)
                                .and_then(|base| base.checked_add(round))
                                .ok_or("benchmark sequence overflow")?;
                            let mut batch = client.begin_optimistic(0, durability)?;
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
                    .map_err(|_| std::io::Error::other("mixed producer panicked"))??,
            );
        }
        Ok(receipts)
    })
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    for sequence in 0..TOTAL_COMMITS {
        if database.get_latest_structure(&key(sequence), 1)?
            != Some(sequence.to_string().into_bytes())
        {
            return Err(format!("missing committed key {sequence}").into());
        }
    }
    if database.get_latest_structure(b"mixed-final-fence", 1)? != Some(b"durable".to_vec()) {
        return Err("final strict fence is missing".into());
    }
    Ok(())
}

fn class_summary(
    receipts: &[ScheduledCommitReceipt],
    durability: DurabilityClass,
) -> Result<ClassSummary, BenchmarkError> {
    let selected = receipts
        .iter()
        .filter(|receipt| receipt.durability == durability)
        .collect::<Vec<_>>();
    let stats = |select: fn(&ScheduledCommitReceipt) -> Duration| {
        latency_stats(
            selected
                .iter()
                .map(|receipt| duration_nanos(select(receipt)))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    Ok(ClassSummary {
        count: selected.len(),
        end_to_end: stats(|receipt| receipt.end_to_end)?,
        admission_wait: stats(|receipt| receipt.admission_wait)?,
        queue_wait: stats(|receipt| receipt.queue_wait)?,
        execution: stats(|receipt| receipt.cohort_execution)?,
        page_sync: stats(|receipt| receipt.page_synchronization)?,
        wal_sync: stats(|receipt| receipt.wal_synchronization)?,
    })
}

fn max_group_run(receipts: &[ScheduledCommitReceipt]) -> usize {
    let mut ordered = receipts.iter().collect::<Vec<_>>();
    ordered.sort_unstable_by_key(|receipt| receipt.commit_csn);
    ordered
        .iter()
        .fold((0_usize, 0_usize), |(current, maximum), receipt| {
            if receipt.durability == DurabilityClass::Group {
                let current = current.saturating_add(1);
                (current, maximum.max(current))
            } else {
                (0, maximum)
            }
        })
        .1
}

fn print_latency(name: &str, stats: LatencyStats, trailing_comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_nanos\": {},", stats.p50);
    println!("      \"p95_nanos\": {},", stats.p95);
    println!("      \"p99_nanos\": {},", stats.p99);
    println!("      \"max_nanos\": {}", stats.max);
    println!("    }}{}", if trailing_comma { "," } else { "" });
}

fn print_class(name: &str, summary: &ClassSummary, trailing_comma: bool) {
    println!("  \"{name}\": {{");
    println!("    \"commits\": {},", summary.count);
    print_latency("end_to_end", summary.end_to_end, true);
    print_latency("admission_wait", summary.admission_wait, true);
    print_latency("queue_wait", summary.queue_wait, true);
    print_latency("execution", summary.execution, true);
    print_latency("page_sync", summary.page_sync, true);
    print_latency("wal_sync", summary.wal_sync, false);
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

fn print_observation(observation: &Observation) -> Result<(), BenchmarkError> {
    let throughput = f64::from(u32::try_from(TOTAL_COMMITS)?)
        / Duration::from_nanos(observation.wall_nanos).as_secs_f64();
    let group = class_summary(&observation.receipts, DurabilityClass::Group)?;
    let strict = class_summary(&observation.receipts, DurabilityClass::Strict)?;
    let memory = class_summary(&observation.receipts, DurabilityClass::Memory)?;
    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-mixed-scheduler-v1\",");
    println!("  \"source_commit\": \"{}\",", observation.source_commit);
    println!("  \"source_tree\": \"{}\",", observation.source_tree);
    println!("  \"rustc\": \"{}\",", observation.rustc);
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"producers\": {PRODUCERS},");
    println!("  \"rounds_per_producer\": {ROUNDS_PER_PRODUCER},");
    println!("  \"workload_commits\": {TOTAL_COMMITS},");
    println!("  \"wall_nanos\": {},", observation.wall_nanos);
    println!("  \"commits_per_second\": {throughput:.3},");
    println!(
        "  \"max_consecutive_group_commits\": {},",
        max_group_run(&observation.receipts)
    );
    println!(
        "  \"final_fence_csn\": {},",
        observation.final_fence.commit_csn.get()
    );
    println!("  \"reopen_verified\": true,");
    print_class("group", &group, true);
    print_class("strict", &strict, true);
    print_class("memory", &memory, false);
    println!("}}");
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let directory = TemporaryDirectory::create()?;
    let observation = run_workload(directory.path())?;
    print_observation(&observation)
}
