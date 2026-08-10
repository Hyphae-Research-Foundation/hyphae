// SPDX-License-Identifier: GPL-3.0-only

//! Concurrency-one Linux observation for native hash field TTL.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{CommitReceipt, NativeDatabase, NativeRuntimeError, Ttl};
use hyphae_native_types::DurabilityClass;

const HASH_KEY: &[u8] = b"hash-field-ttl-benchmark";
const HASH_FIELDS: u32 = 2_048;
const READ_OBSERVATIONS: usize = 200_000;
const READ_OPERATIONS_PER_OBSERVATION: usize = 32;
const READ_WARMUP: usize = 20_000;
const HLEN_OBSERVATIONS: usize = 10_000;
const HLEN_WARMUP: usize = 1_000;
const MEMORY_COMMIT_OBSERVATIONS: usize = 32;
const STRICT_COMMIT_OBSERVATIONS: usize = 16;
const CLEANUP_HASHES: u32 = 16;
const CLEANUP_FIELDS_PER_HASH: u32 = 64;
const CLEANUP_EXPIRY: i64 = 1_500_000;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-hash-field-ttl-{}-{timestamp}",
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
    persistent_hget: Stats,
    persistent_hlen: Stats,
    persistent_snapshot_ttl: Stats,
    private_ttl: Stats,
    snapshot_ttl: Stats,
    physical_ttl: Stats,
    expiring_hget: Stats,
    due_hlen: Stats,
}

struct MutationStats {
    memory_commit: Stats,
    strict_commit: Stats,
    cleanup: Stats,
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

fn seed_primary_hash(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_hash(HASH_KEY.to_vec())?;
    for field in 0..HASH_FIELDS {
        seed.hset(
            HASH_KEY.to_vec(),
            field.to_be_bytes().to_vec(),
            vec![u8::try_from(field % 251)?; 64],
        )?;
    }
    seed.commit()?;
    Ok(())
}

fn seed_cleanup_hashes(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(300, DurabilityClass::Memory)?;
    for hash in 0..CLEANUP_HASHES {
        let key = format!("hash-field-ttl-cleanup-{hash:04}").into_bytes();
        seed.create_hash(key.clone())?;
        for field in 0..CLEANUP_FIELDS_PER_HASH {
            let identity = field.to_be_bytes().to_vec();
            seed.hset(key.clone(), identity.clone(), identity.clone())?;
            if !seed.expire_hash_field(key.clone(), identity, CLEANUP_EXPIRY)? {
                return Err("cleanup field did not accept expiry".into());
            }
        }
    }
    seed.commit()?;
    Ok(())
}

fn measure_reads(
    database: &mut NativeDatabase,
    target: &[u8],
) -> Result<ReadStats, Box<dyn std::error::Error>> {
    let persistent_fields = usize::try_from(HASH_FIELDS)?;
    let due_fields = usize::try_from(HASH_FIELDS - 1)?;
    let persistent_hget = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.hget_latest_hash(HASH_KEY, target),
    )?;
    let persistent_hlen = measure(HLEN_OBSERVATIONS, 1, HLEN_WARMUP, || {
        checked_hash_len(
            database.hlen_latest_hash_at(HASH_KEY, 101),
            persistent_fields,
        )
    })?;
    let persistent_snapshot = database.snapshot(101)?;
    let persistent_snapshot_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || checked_persistent_ttl(persistent_snapshot.ttl_hash_field(HASH_KEY, target)),
    )?;
    let mut expire = database.begin(101, DurabilityClass::Memory)?;
    if !expire.expire_hash_field(HASH_KEY.to_vec(), target.to_vec(), 1_000_000)? {
        return Err("primary field did not accept expiry".into());
    }
    expire.commit()?;
    let expiring_snapshot = database.snapshot(102)?;
    let private = database.begin_optimistic(102, DurabilityClass::Memory)?;
    let private_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || checked_expiring_ttl(private.ttl_hash_field(HASH_KEY, target)),
    )?;
    let snapshot_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || checked_expiring_ttl(expiring_snapshot.ttl_hash_field(HASH_KEY, target)),
    )?;
    let physical_ttl = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.ttl_latest_hash_field(HASH_KEY, target, 102),
    )?;
    let expiring_hget = measure(
        READ_OBSERVATIONS,
        READ_OPERATIONS_PER_OBSERVATION,
        READ_WARMUP,
        || database.hget_latest_hash_at(HASH_KEY, target, 102),
    )?;
    let due_hlen = measure(HLEN_OBSERVATIONS, 1, HLEN_WARMUP, || {
        checked_hash_len(
            database.hlen_latest_hash_at(HASH_KEY, 1_000_000),
            due_fields,
        )
    })?;
    Ok(ReadStats {
        persistent_hget,
        persistent_hlen,
        persistent_snapshot_ttl,
        private_ttl,
        snapshot_ttl,
        physical_ttl,
        expiring_hget,
        due_hlen,
    })
}

fn checked_hash_len(
    result: Result<usize, NativeRuntimeError>,
    expected: usize,
) -> Result<usize, NativeRuntimeError> {
    let actual = result?;
    if actual != expected {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(actual)
}

fn checked_persistent_ttl(ttl: Ttl) -> Result<Ttl, NativeRuntimeError> {
    if ttl != Ttl::Persistent {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(ttl)
}

fn checked_expiring_ttl(ttl: Ttl) -> Result<Ttl, NativeRuntimeError> {
    if ttl != Ttl::RemainingMicros(999_898) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    Ok(ttl)
}

fn measure_mutations(
    database: &mut NativeDatabase,
    target: &[u8],
) -> Result<MutationStats, Box<dyn std::error::Error>> {
    let mut memory_expiry = 2_000_000_i64;
    let memory_commit = measure(MEMORY_COMMIT_OBSERVATIONS, 1, 0, || {
        memory_expiry += 1;
        expire_primary_field(
            database,
            target,
            200,
            memory_expiry,
            DurabilityClass::Memory,
        )
    })?;
    let mut strict_expiry = 3_000_000_i64;
    let strict_commit = measure(STRICT_COMMIT_OBSERVATIONS, 1, 0, || {
        strict_expiry += 1;
        expire_primary_field(
            database,
            target,
            201,
            strict_expiry,
            DurabilityClass::Strict,
        )
    })?;
    seed_cleanup_hashes(database)?;
    let cleanup_fields = usize::try_from(CLEANUP_FIELDS_PER_HASH)?;
    let cleanup = measure(usize::try_from(CLEANUP_HASHES)?, 1, 0, || {
        let receipt = database.expire_due_structures(
            CLEANUP_EXPIRY,
            cleanup_fields,
            DurabilityClass::Memory,
        )?;
        if receipt.expired_keys != cleanup_fields {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        Ok(receipt)
    })?;
    Ok(MutationStats {
        memory_commit,
        strict_commit,
        cleanup,
    })
}

fn expire_primary_field(
    database: &mut NativeDatabase,
    target: &[u8],
    logical_time_micros: i64,
    expiry: i64,
    durability: DurabilityClass,
) -> Result<CommitReceipt, NativeRuntimeError> {
    let mut transaction = database.begin(logical_time_micros, durability)?;
    if !transaction.expire_hash_field(HASH_KEY.to_vec(), target.to_vec(), expiry)? {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    transaction.commit()
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
    seed_primary_hash(&mut database)?;
    let target = (HASH_FIELDS / 2).to_be_bytes();
    let reads = measure_reads(&mut database, &target)?;
    let mutations = measure_mutations(&mut database, &target)?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.hash-field-ttl-smoke.v1\",");
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
    println!("  \"hash_fields\": {HASH_FIELDS},");
    println!("  \"read_observations\": {READ_OBSERVATIONS},");
    println!("  \"read_operations_per_observation\": {READ_OPERATIONS_PER_OBSERVATION},");
    println!("  \"read_warmup\": {READ_WARMUP},");
    println!("  \"hlen_observations\": {HLEN_OBSERVATIONS},");
    println!("  \"hlen_warmup\": {HLEN_WARMUP},");
    println!("  \"memory_commit_observations\": {MEMORY_COMMIT_OBSERVATIONS},");
    println!("  \"strict_commit_observations\": {STRICT_COMMIT_OBSERVATIONS},");
    println!("  \"cleanup_hashes\": {CLEANUP_HASHES},");
    println!("  \"cleanup_fields_per_hash\": {CLEANUP_FIELDS_PER_HASH},");
    println!("  \"operations\": {{");
    print_stats(
        "persistent_hash_field_hget_physical",
        reads.persistent_hget,
        true,
    );
    print_stats(
        "persistent_hash_hlen_no_due_physical",
        reads.persistent_hlen,
        true,
    );
    print_stats(
        "persistent_hash_field_ttl_materialized_snapshot",
        reads.persistent_snapshot_ttl,
        true,
    );
    print_stats(
        "expiring_hash_field_ttl_private_batch",
        reads.private_ttl,
        true,
    );
    print_stats(
        "expiring_hash_field_ttl_materialized_snapshot",
        reads.snapshot_ttl,
        true,
    );
    print_stats("expiring_hash_field_ttl_physical", reads.physical_ttl, true);
    print_stats(
        "expiring_hash_field_hget_physical",
        reads.expiring_hget,
        true,
    );
    print_stats("expiring_hash_hlen_one_due_physical", reads.due_hlen, true);
    print_stats(
        "expire_hash_field_memory_commit",
        mutations.memory_commit,
        true,
    );
    print_stats(
        "expire_hash_field_strict_commit",
        mutations.strict_commit,
        true,
    );
    print_stats(
        "expire_hash_field_cleanup_64_fields_memory",
        mutations.cleanup,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
