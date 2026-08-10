// SPDX-License-Identifier: GPL-3.0-only

//! Reproducible active-expiry scheduler interference observation.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    ActiveExpiryConfig, ActiveExpiryStats, GroupCommitConfig, NativeCommitScheduler,
    NativeDatabase, ScheduledCommitReceipt,
};
use hyphae_native_types::DurabilityClass;

const EXPIRING_KEYS: usize = 512;
const EXPIRY_BATCH_KEYS: usize = 64;
const FOREGROUND_PRODUCERS: usize = 4;
const ROUNDS_PER_PRODUCER: usize = 64;
const FOREGROUND_COMMITS: usize = FOREGROUND_PRODUCERS * ROUNDS_PER_PRODUCER;
const FOREGROUND_BUDGET: usize = 16;
const EXPIRY_INTERVAL: Duration = Duration::from_micros(100);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

struct ScenarioObservation {
    wall_nanos: u64,
    commits_per_second: f64,
    end_to_end: LatencyStats,
    queue_wait: LatencyStats,
    execution: LatencyStats,
    cleanup_nanos: Option<u64>,
    cleanup_keys_per_second: Option<f64>,
    active_expiry: Option<ActiveExpiryStats>,
    final_fence_csn: u64,
}

struct BenchmarkMetadata {
    source_commit: String,
    source_tree: String,
    rustc: String,
    profile: String,
}

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-active-expiry-{label}-{}-{timestamp}",
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

fn expiring_key(sequence: usize) -> Vec<u8> {
    format!("active-expiry-benchmark-due-{sequence:05}").into_bytes()
}

fn foreground_key(sequence: usize) -> Vec<u8> {
    format!("active-expiry-benchmark-live-{sequence:05}").into_bytes()
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

fn seed_expiring_keys(database: &mut NativeDatabase) -> Result<(), BenchmarkError> {
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    for sequence in 0..EXPIRING_KEYS {
        seed.set(
            expiring_key(sequence),
            sequence.to_string().into_bytes(),
            Some(1),
        )?;
    }
    seed.commit()?;
    Ok(())
}

fn start_scheduler(
    database: NativeDatabase,
    active_expiry: bool,
) -> Result<NativeCommitScheduler, BenchmarkError> {
    let scheduler = GroupCommitConfig::new(16, Duration::from_micros(200), 64)?;
    if active_expiry {
        Ok(NativeCommitScheduler::start_with_active_expiry(
            database,
            scheduler,
            ActiveExpiryConfig::new(
                EXPIRY_INTERVAL,
                EXPIRY_BATCH_KEYS,
                DurabilityClass::Memory,
                FOREGROUND_BUDGET,
            )?,
        )?)
    } else {
        Ok(NativeCommitScheduler::start(database, scheduler)?)
    }
}

fn run_foreground(
    scheduler: &NativeCommitScheduler,
) -> Result<Vec<ScheduledCommitReceipt>, BenchmarkError> {
    let clients = (0..FOREGROUND_PRODUCERS)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(FOREGROUND_PRODUCERS));
    std::thread::scope(|scope| {
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
                                .ok_or("foreground sequence overflow")?;
                            let mut batch = client.begin_optimistic(1, DurabilityClass::Group)?;
                            batch.set(
                                foreground_key(sequence),
                                sequence.to_string().into_bytes(),
                                None,
                            )?;
                            barrier.wait();
                            receipts.push(client.submit(batch)?);
                        }
                        Ok(receipts)
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut receipts = Vec::with_capacity(FOREGROUND_COMMITS);
        for handle in handles {
            receipts.extend(
                handle
                    .join()
                    .map_err(|_| std::io::Error::other("foreground producer panicked"))??,
            );
        }
        Ok(receipts)
    })
}

fn wait_for_cleanup(
    scheduler: &NativeCommitScheduler,
    started: Instant,
) -> Result<(u64, ActiveExpiryStats), BenchmarkError> {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        let stats = scheduler
            .active_expiry_stats()
            .ok_or("active expiry stats missing")?;
        let completed_sweeps = stats
            .committed_sweeps
            .saturating_add(stats.empty_sweeps)
            .saturating_add(stats.failures);
        if stats.expired_keys == u64::try_from(EXPIRING_KEYS)?
            && stats.committed_sweeps == u64::try_from(EXPIRING_KEYS / EXPIRY_BATCH_KEYS)?
            && stats.attempted_sweeps == completed_sweeps
        {
            return Ok((duration_nanos(started.elapsed())?, stats));
        }
        if stats.failures != 0 {
            return Err("active expiry failed during benchmark".into());
        }
        if Instant::now() >= deadline {
            return Err("active expiry did not drain benchmark keys".into());
        }
        std::thread::yield_now();
    }
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    for sequence in 0..EXPIRING_KEYS {
        if database
            .get_latest_structure(&expiring_key(sequence), 2)?
            .is_some()
        {
            return Err(format!("due key {sequence} remained logically visible").into());
        }
    }
    for sequence in 0..FOREGROUND_COMMITS {
        if database.get_latest_structure(&foreground_key(sequence), 2)?
            != Some(sequence.to_string().into_bytes())
        {
            return Err(format!("foreground key {sequence} is missing").into());
        }
    }
    if database.get_latest_structure(b"active-expiry-benchmark-fence", 2)?
        != Some(b"durable".to_vec())
    {
        return Err("final strict fence is missing".into());
    }
    Ok(())
}

fn receipt_stats(
    receipts: &[ScheduledCommitReceipt],
    select: fn(&ScheduledCommitReceipt) -> Duration,
) -> Result<LatencyStats, BenchmarkError> {
    latency_stats(
        receipts
            .iter()
            .map(|receipt| duration_nanos(select(receipt)))
            .collect::<Result<Vec<_>, _>>()?,
    )
}

fn run_scenario(active_expiry: bool) -> Result<ScenarioObservation, BenchmarkError> {
    let label = if active_expiry { "enabled" } else { "disabled" };
    let directory = TemporaryDirectory::create(label)?;
    let mut database = NativeDatabase::create(directory.path())?;
    seed_expiring_keys(&mut database)?;
    let scheduler = start_scheduler(database, active_expiry)?;

    let started = Instant::now();
    let receipts = run_foreground(&scheduler)?;
    let wall_nanos = duration_nanos(started.elapsed())?;
    let (cleanup_nanos, active_expiry_stats) = if active_expiry {
        let (cleanup_nanos, stats) = wait_for_cleanup(&scheduler, started)?;
        (Some(cleanup_nanos), Some(stats))
    } else {
        (None, None)
    };

    let mut fence = scheduler.begin_optimistic(2, DurabilityClass::Strict)?;
    fence.set(
        b"active-expiry-benchmark-fence".to_vec(),
        b"durable".to_vec(),
        None,
    )?;
    let final_fence = scheduler.submit(fence)?;
    scheduler.shutdown()?;
    verify(&NativeDatabase::open(directory.path())?)?;

    let commits_per_second = f64::from(u32::try_from(FOREGROUND_COMMITS)?)
        / Duration::from_nanos(wall_nanos).as_secs_f64();
    let cleanup_keys_per_second = cleanup_nanos.map(|nanos| {
        f64::from(u32::try_from(EXPIRING_KEYS).unwrap_or(u32::MAX))
            / Duration::from_nanos(nanos).as_secs_f64()
    });
    Ok(ScenarioObservation {
        wall_nanos,
        commits_per_second,
        end_to_end: receipt_stats(&receipts, |receipt| receipt.end_to_end)?,
        queue_wait: receipt_stats(&receipts, |receipt| receipt.queue_wait)?,
        execution: receipt_stats(&receipts, |receipt| receipt.cohort_execution)?,
        cleanup_nanos,
        cleanup_keys_per_second,
        active_expiry: active_expiry_stats,
        final_fence_csn: final_fence.commit_csn.get(),
    })
}

fn print_latency(name: &str, stats: LatencyStats, trailing_comma: bool) {
    println!("      \"{name}\": {{");
    println!("        \"p50_nanos\": {},", stats.p50);
    println!("        \"p95_nanos\": {},", stats.p95);
    println!("        \"p99_nanos\": {},", stats.p99);
    println!("        \"max_nanos\": {}", stats.max);
    println!("      }}{}", if trailing_comma { "," } else { "" });
}

fn print_scenario(
    name: &str,
    observation: &ScenarioObservation,
    trailing_comma: bool,
) -> Result<(), BenchmarkError> {
    println!("  \"{name}\": {{");
    println!("    \"foreground_commits\": {FOREGROUND_COMMITS},");
    println!("    \"foreground_wall_nanos\": {},", observation.wall_nanos);
    println!(
        "    \"foreground_commits_per_second\": {:.3},",
        observation.commits_per_second
    );
    println!(
        "    \"cleanup_nanos\": {},",
        observation
            .cleanup_nanos
            .map_or_else(|| "null".to_owned(), |value| value.to_string())
    );
    println!(
        "    \"cleanup_keys_per_second\": {},",
        observation
            .cleanup_keys_per_second
            .map_or_else(|| "null".to_owned(), |value| format!("{value:.3}"))
    );
    println!("    \"final_fence_csn\": {},", observation.final_fence_csn);
    println!("    \"reopen_verified\": true,");
    println!("    \"foreground_latency\": {{");
    print_latency("end_to_end", observation.end_to_end, true);
    print_latency("queue_wait", observation.queue_wait, true);
    print_latency("execution", observation.execution, false);
    println!("    }},");
    match observation.active_expiry {
        Some(stats) => {
            println!("    \"active_expiry\": {{");
            println!("      \"attempted_sweeps\": {},", stats.attempted_sweeps);
            println!("      \"committed_sweeps\": {},", stats.committed_sweeps);
            println!("      \"expired_keys\": {},", stats.expired_keys);
            println!("      \"empty_sweeps\": {},", stats.empty_sweeps);
            println!("      \"failures\": {},", stats.failures);
            println!(
                "      \"latest_logical_time_micros\": {},",
                stats.latest_logical_time_micros
            );
            println!(
                "      \"latest_sweep_nanos\": {},",
                duration_nanos(stats.latest_sweep_duration)?
            );
            println!(
                "      \"max_foreground_after_due\": {}",
                stats.max_foreground_after_due
            );
            println!("    }}");
        }
        None => println!("    \"active_expiry\": null"),
    }
    println!("  }}{}", if trailing_comma { "," } else { "" });
    Ok(())
}

fn metadata() -> BenchmarkMetadata {
    let mut args = std::env::args().skip(1);
    BenchmarkMetadata {
        source_commit: args
            .next()
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        source_tree: args
            .next()
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        rustc: args.next().unwrap_or_else(|| "unknown".to_owned()),
        profile: args.next().unwrap_or_else(|| "unknown".to_owned()),
    }
}

fn main() -> Result<(), BenchmarkError> {
    let metadata = metadata();
    let disabled = run_scenario(false)?;
    let enabled = run_scenario(true)?;
    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-active-expiry-scheduler-v1\",");
    println!("  \"source_commit\": \"{}\",", metadata.source_commit);
    println!("  \"source_tree\": \"{}\",", metadata.source_tree);
    println!("  \"rustc\": \"{}\",", metadata.rustc);
    println!("  \"profile\": \"{}\",", metadata.profile);
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"expiring_keys\": {EXPIRING_KEYS},");
    println!("  \"expiry_batch_keys\": {EXPIRY_BATCH_KEYS},");
    println!("  \"expiry_interval_micros\": 100,");
    println!("  \"foreground_producers\": {FOREGROUND_PRODUCERS},");
    println!("  \"rounds_per_producer\": {ROUNDS_PER_PRODUCER},");
    println!("  \"foreground_budget\": {FOREGROUND_BUDGET},");
    print_scenario("disabled", &disabled, true)?;
    print_scenario("enabled", &enabled, false)?;
    println!("}}");
    Ok(())
}
