// SPDX-License-Identifier: Apache-2.0

//! Concurrency-one Linux observation for native whole-list TTL.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{CommitReceipt, NativeDatabase, NativeRuntimeError, Ttl};
use hyphae_native_types::DurabilityClass;

const READ_OBSERVATIONS: usize = 200_000;
const READ_OPERATIONS_PER_OBSERVATION: usize = 32;
const READ_WARMUP: usize = 20_000;
const LIST_ELEMENTS: u32 = 2_048;
const LIST_KEY: &[u8] = b"list-ttl-benchmark";
const MEMORY_COMMIT_OBSERVATIONS: usize = 32;
const STRICT_COMMIT_OBSERVATIONS: usize = 16;
const CLEANUP_LISTS: u32 = 16;
const SMALL_CLEANUP_ELEMENTS: u32 = 1;
const LARGE_CLEANUP_ELEMENTS: u32 = 256;
const SMALL_CLEANUP_EXPIRY: i64 = 1_500_000;
const LARGE_CLEANUP_EXPIRY: i64 = 1_600_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-list-ttl-smoke-{}-{timestamp}",
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

#[derive(Clone, Copy)]
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    maximum_nanos: u64,
    throughput_per_second: f64,
}

struct ReadStats {
    persistent_length: Stats,
    persistent_snapshot_ttl: Stats,
    private_ttl: Stats,
    snapshot_ttl: Stats,
    physical_ttl: Stats,
    expiring_length: Stats,
}

struct MutationStats {
    memory_commit: Stats,
    strict_commit: Stats,
    cleanup_small: Stats,
    cleanup_large: Stats,
}

fn measure<F, T>(
    observations: usize,
    operations_per_observation: usize,
    warmup: usize,
    mut operation: F,
) -> Result<Stats, Box<dyn std::error::Error>>
where
    F: FnMut() -> Result<T, NativeRuntimeError>,
{
    for _ in 0..warmup {
        black_box(operation()?);
    }
    let mut samples = Vec::with_capacity(observations);
    let total_started = Instant::now();
    for _ in 0..observations {
        let started = Instant::now();
        for _ in 0..operations_per_observation {
            black_box(operation()?);
        }
        let elapsed = started.elapsed().as_nanos() / u128::try_from(operations_per_observation)?;
        samples.push(u64::try_from(elapsed)?);
    }
    let total = total_started.elapsed();
    samples.sort_unstable();
    let completed = observations
        .checked_mul(operations_per_observation)
        .ok_or("benchmark operation count overflow")?;
    let completed = u32::try_from(completed)?;
    Ok(Stats {
        p50_nanos: percentile(&samples, 500),
        p95_nanos: percentile(&samples, 950),
        p99_nanos: percentile(&samples, 990),
        p999_nanos: percentile(&samples, 999),
        maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
        throughput_per_second: f64::from(completed) / total.as_secs_f64(),
    })
}

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn seed_primary_list(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_list(LIST_KEY.to_vec())?;
    for element in 0..LIST_ELEMENTS {
        seed.rpush(LIST_KEY.to_vec(), element.to_be_bytes().to_vec())?;
    }
    seed.commit()?;
    Ok(())
}

fn seed_cleanup_lists(
    database: &mut NativeDatabase,
    prefix: &str,
    elements_per_list: u32,
    expiry: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(300, DurabilityClass::Memory)?;
    for list in 0..CLEANUP_LISTS {
        let key = format!("{prefix}-{list:04}").into_bytes();
        seed.create_list(key.clone())?;
        for element in 0..elements_per_list {
            seed.rpush(key.clone(), element.to_be_bytes().to_vec())?;
        }
        if !seed.expire_list(key, expiry)? {
            return Err("cleanup list did not accept expiry".into());
        }
    }
    seed.commit()?;
    Ok(())
}

fn measure_reads(database: &mut NativeDatabase) -> Result<ReadStats, Box<dyn std::error::Error>> {
    let persistent_length = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.llen_latest_list_at(LIST_KEY, 101),
    )?;
    let persistent_snapshot = database.snapshot(101)?;
    let persistent_snapshot_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || {
            let ttl = persistent_snapshot.ttl_list(LIST_KEY);
            if ttl != Ttl::Persistent {
                return Err(NativeRuntimeError::InvalidPreparedMutation);
            }
            Ok(ttl)
        },
    )?;
    let mut expire = database.begin(101, DurabilityClass::Memory)?;
    if !expire.expire_list(LIST_KEY.to_vec(), 1_000_000)? {
        return Err("primary list did not accept expiry".into());
    }
    expire.commit()?;
    let expiring_snapshot = database.snapshot(102)?;
    let private = database.begin_optimistic(102, DurabilityClass::Memory)?;
    let private_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || checked_list_ttl(private.ttl_list(LIST_KEY)),
    )?;
    let snapshot_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || checked_list_ttl(expiring_snapshot.ttl_list(LIST_KEY)),
    )?;
    let physical_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.ttl_latest_list(LIST_KEY, 102),
    )?;
    let expiring_length = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.llen_latest_list_at(LIST_KEY, 102),
    )?;
    Ok(ReadStats {
        persistent_length,
        persistent_snapshot_ttl,
        private_ttl,
        snapshot_ttl,
        physical_ttl,
        expiring_length,
    })
}

fn checked_list_ttl(ttl: Ttl) -> Result<Ttl, NativeRuntimeError> {
    if ttl != Ttl::RemainingMicros(999_898) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(ttl)
}

fn measure_mutations(
    database: &mut NativeDatabase,
) -> Result<MutationStats, Box<dyn std::error::Error>> {
    let mut memory_expiry = 2_000_000_i64;
    let memory_commit = measure(MEMORY_COMMIT_OBSERVATIONS, 1, 0, || {
        memory_expiry += 1;
        expire_primary_list(database, 200, memory_expiry, DurabilityClass::Memory)
    })?;
    let mut strict_expiry = 3_000_000_i64;
    let strict_commit = measure(STRICT_COMMIT_OBSERVATIONS, 1, 0, || {
        strict_expiry += 1;
        expire_primary_list(database, 201, strict_expiry, DurabilityClass::Strict)
    })?;

    seed_cleanup_lists(
        database,
        "list-ttl-cleanup-small",
        SMALL_CLEANUP_ELEMENTS,
        SMALL_CLEANUP_EXPIRY,
    )?;
    let cleanup_small = measure(usize::try_from(CLEANUP_LISTS)?, 1, 0, || {
        cleanup_one_list(database, SMALL_CLEANUP_EXPIRY)
    })?;

    seed_cleanup_lists(
        database,
        "list-ttl-cleanup-large",
        LARGE_CLEANUP_ELEMENTS,
        LARGE_CLEANUP_EXPIRY,
    )?;
    let cleanup_large = measure(usize::try_from(CLEANUP_LISTS)?, 1, 0, || {
        cleanup_one_list(database, LARGE_CLEANUP_EXPIRY)
    })?;
    Ok(MutationStats {
        memory_commit,
        strict_commit,
        cleanup_small,
        cleanup_large,
    })
}

fn expire_primary_list(
    database: &mut NativeDatabase,
    logical_time_micros: i64,
    expiry: i64,
    durability: DurabilityClass,
) -> Result<CommitReceipt, NativeRuntimeError> {
    let mut transaction = database.begin(logical_time_micros, durability)?;
    if !transaction.expire_list(LIST_KEY.to_vec(), expiry)? {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    transaction.commit()
}

fn cleanup_one_list(
    database: &mut NativeDatabase,
    logical_time_micros: i64,
) -> Result<hyphae_native_runtime::ExpirySweepReceipt, NativeRuntimeError> {
    let receipt =
        database.expire_due_structures(logical_time_micros, 1, DurabilityClass::Memory)?;
    if receipt.expired_keys != 1 {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(receipt)
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_nanos\": {},", stats.p50_nanos);
    println!("      \"p95_nanos\": {},", stats.p95_nanos);
    println!("      \"p99_nanos\": {},", stats.p99_nanos);
    println!("      \"p999_nanos\": {},", stats.p999_nanos);
    println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
    println!(
        "      \"throughput_per_second\": {:.3}",
        stats.throughput_per_second
    );
    println!("    }}{}", if comma { "," } else { "" });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    seed_primary_list(&mut database)?;
    let reads = measure_reads(&mut database)?;
    let mutations = measure_mutations(&mut database)?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.list-ttl-smoke.v1\",");
    println!("  \"status\": \"observation-not-universal-slo\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"list_elements\": {LIST_ELEMENTS},");
    println!("  \"read_observations\": {READ_OBSERVATIONS},");
    println!("  \"read_operations_per_observation\": {READ_OPERATIONS_PER_OBSERVATION},");
    println!("  \"read_warmup\": {READ_WARMUP},");
    println!("  \"memory_commit_observations\": {MEMORY_COMMIT_OBSERVATIONS},");
    println!("  \"strict_commit_observations\": {STRICT_COMMIT_OBSERVATIONS},");
    println!("  \"cleanup_lists_per_cardinality\": {CLEANUP_LISTS},");
    println!("  \"small_cleanup_elements\": {SMALL_CLEANUP_ELEMENTS},");
    println!("  \"large_cleanup_elements\": {LARGE_CLEANUP_ELEMENTS},");
    println!("  \"operations\": {{");
    print_stats(
        "persistent_list_llen_physical",
        reads.persistent_length,
        true,
    );
    print_stats(
        "persistent_list_ttl_materialized_snapshot",
        reads.persistent_snapshot_ttl,
        true,
    );
    print_stats("expiring_list_ttl_private_batch", reads.private_ttl, true);
    print_stats(
        "expiring_list_ttl_materialized_snapshot",
        reads.snapshot_ttl,
        true,
    );
    print_stats("expiring_list_ttl_physical", reads.physical_ttl, true);
    print_stats("expiring_list_llen_physical", reads.expiring_length, true);
    print_stats("expire_list_memory_commit", mutations.memory_commit, true);
    print_stats("expire_list_strict_commit", mutations.strict_commit, true);
    print_stats(
        "expire_list_cleanup_1_element_memory",
        mutations.cleanup_small,
        true,
    );
    print_stats(
        "expire_list_cleanup_256_elements_memory",
        mutations.cleanup_large,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
