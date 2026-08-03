// SPDX-License-Identifier: Apache-2.0

//! Concurrency-one Linux observation for native hash field commands.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{HashFieldUpdate, NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

const HASH_KEY: &[u8] = b"hash-field-command-benchmark";
const HASH_FIELDS: u32 = 2_048;
const BATCH_FIELDS: u32 = 32;
const READ_OBSERVATIONS: usize = 100_000;
const READ_WARMUP: usize = 10_000;
const MEMORY_COMMIT_OBSERVATIONS: usize = 32;
const STRICT_COMMIT_OBSERVATIONS: usize = 16;
const DELETE_OBSERVATIONS: u32 = 16;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-hash-field-commands-{}-{timestamp}",
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
    fields_per_call: u32,
}

struct ReadStats {
    snapshot_many: Stats,
    private_many: Stats,
    physical_many: Stats,
    physical_singles: Stats,
}

struct MutationStats {
    memory_set: Stats,
    strict_set: Stats,
    memory_delete: Stats,
    memory_increment: Stats,
    strict_increment: Stats,
}

fn measure<F, T>(
    observations: usize,
    warmup: usize,
    fields_per_call: u32,
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
        black_box(operation()?);
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    let total = total_started.elapsed();
    samples.sort_unstable();
    let completed = u32::try_from(observations)?;
    Ok(Stats {
        p50_nanos: percentile(&samples, 500),
        p95_nanos: percentile(&samples, 950),
        p99_nanos: percentile(&samples, 990),
        p999_nanos: percentile(&samples, 999),
        maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
        throughput_per_second: f64::from(completed) / total.as_secs_f64(),
        fields_per_call,
    })
}

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn read_fields() -> Vec<Vec<u8>> {
    (1_000..1_000 + BATCH_FIELDS)
        .map(|field| field.to_be_bytes().to_vec())
        .collect()
}

fn updates(value: u8) -> Vec<HashFieldUpdate> {
    (0..BATCH_FIELDS)
        .map(|field| (field.to_be_bytes().to_vec(), vec![value; 64]))
        .collect()
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
    seed.hset(HASH_KEY.to_vec(), b"counter".to_vec(), b"0".to_vec())?;
    seed.commit()?;
    Ok(())
}

fn seed_delete_hashes(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(300, DurabilityClass::Memory)?;
    for hash in 0..DELETE_OBSERVATIONS {
        let key = format!("hash-field-delete-{hash:04}").into_bytes();
        seed.create_hash(key.clone())?;
        seed.hset_many(key, updates(u8::try_from(hash)?))?;
    }
    seed.commit()?;
    Ok(())
}

fn measure_reads(
    database: &NativeDatabase,
    fields: &[Vec<u8>],
) -> Result<ReadStats, Box<dyn std::error::Error>> {
    let snapshot = database.snapshot(101)?;
    let private = database.begin_optimistic(101, DurabilityClass::Memory)?;
    let snapshot_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_FIELDS, || {
        snapshot.hget_many(HASH_KEY, fields)
    })?;
    let private_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_FIELDS, || {
        private.hget_many(HASH_KEY, fields)
    })?;
    let physical_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_FIELDS, || {
        database.hget_many_latest_hash(HASH_KEY, fields)
    })?;
    let physical_singles = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_FIELDS, || {
        for field in fields {
            black_box(database.hget_latest_hash(HASH_KEY, field)?);
        }
        Ok(())
    })?;
    Ok(ReadStats {
        snapshot_many,
        private_many,
        physical_many,
        physical_singles,
    })
}

fn measure_mutations(
    database: &mut NativeDatabase,
) -> Result<MutationStats, Box<dyn std::error::Error>> {
    let mut sequence = 1_u8;
    let memory_set_commit = measure(MEMORY_COMMIT_OBSERVATIONS, 0, BATCH_FIELDS, || {
        sequence = sequence.wrapping_add(1);
        commit_set_batch(database, updates(sequence), DurabilityClass::Memory)
    })?;
    let strict_set_commit = measure(STRICT_COMMIT_OBSERVATIONS, 0, BATCH_FIELDS, || {
        sequence = sequence.wrapping_add(1);
        commit_set_batch(database, updates(sequence), DurabilityClass::Strict)
    })?;
    seed_delete_hashes(database)?;
    let mut delete_hash = 0_u32;
    let memory_delete_commit = measure(
        usize::try_from(DELETE_OBSERVATIONS)?,
        0,
        BATCH_FIELDS,
        || {
            let key = format!("hash-field-delete-{delete_hash:04}").into_bytes();
            delete_hash += 1;
            let mut transaction = database.begin(301, DurabilityClass::Memory)?;
            transaction.hdelete_many(
                key,
                (0..BATCH_FIELDS)
                    .map(|field| field.to_be_bytes().to_vec())
                    .collect(),
            )?;
            transaction.commit()
        },
    )?;
    let memory_increment_commit = measure(MEMORY_COMMIT_OBSERVATIONS, 0, 1, || {
        commit_increment(database, DurabilityClass::Memory)
    })?;
    let strict_increment_commit = measure(STRICT_COMMIT_OBSERVATIONS, 0, 1, || {
        commit_increment(database, DurabilityClass::Strict)
    })?;
    Ok(MutationStats {
        memory_set: memory_set_commit,
        strict_set: strict_set_commit,
        memory_delete: memory_delete_commit,
        memory_increment: memory_increment_commit,
        strict_increment: strict_increment_commit,
    })
}

fn commit_set_batch(
    database: &mut NativeDatabase,
    updates: Vec<HashFieldUpdate>,
    durability: DurabilityClass,
) -> Result<hyphae_native_runtime::CommitReceipt, NativeRuntimeError> {
    let mut transaction = database.begin(200, durability)?;
    transaction.hset_many(HASH_KEY.to_vec(), updates)?;
    transaction.commit()
}

fn commit_increment(
    database: &mut NativeDatabase,
    durability: DurabilityClass,
) -> Result<hyphae_native_runtime::CommitReceipt, NativeRuntimeError> {
    let mut transaction = database.begin(400, durability)?;
    transaction.hincrement_i64(HASH_KEY.to_vec(), b"counter".to_vec(), 1)?;
    transaction.commit()
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    let p50_nanos = u32::try_from(stats.p50_nanos).map_or(f64::INFINITY, f64::from);
    println!("    \"{name}\": {{");
    println!("      \"fields_per_call\": {},", stats.fields_per_call);
    println!("      \"p50_nanos\": {},", stats.p50_nanos);
    println!("      \"p95_nanos\": {},", stats.p95_nanos);
    println!("      \"p99_nanos\": {},", stats.p99_nanos);
    println!("      \"p999_nanos\": {},", stats.p999_nanos);
    println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
    println!(
        "      \"p50_nanos_per_field\": {:.3},",
        p50_nanos / f64::from(stats.fields_per_call)
    );
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
    let fields = read_fields();
    let reads = measure_reads(&database, &fields)?;
    let mutations = measure_mutations(&mut database)?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.hash-field-commands-smoke.v1\",");
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
    println!("  \"batch_fields\": {BATCH_FIELDS},");
    println!("  \"read_observations\": {READ_OBSERVATIONS},");
    println!("  \"read_warmup\": {READ_WARMUP},");
    println!("  \"memory_commit_observations\": {MEMORY_COMMIT_OBSERVATIONS},");
    println!("  \"strict_commit_observations\": {STRICT_COMMIT_OBSERVATIONS},");
    println!("  \"operations\": {{");
    print_stats("snapshot_hget_many_32", reads.snapshot_many, true);
    print_stats("private_hget_many_32", reads.private_many, true);
    print_stats("physical_hget_many_32", reads.physical_many, true);
    print_stats("physical_32_single_hget", reads.physical_singles, true);
    print_stats("memory_hset_many_32_commit", mutations.memory_set, true);
    print_stats("strict_hset_many_32_commit", mutations.strict_set, true);
    print_stats(
        "memory_hdelete_many_32_commit",
        mutations.memory_delete,
        true,
    );
    print_stats("memory_hincrement_commit", mutations.memory_increment, true);
    print_stats(
        "strict_hincrement_commit",
        mutations.strict_increment,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
