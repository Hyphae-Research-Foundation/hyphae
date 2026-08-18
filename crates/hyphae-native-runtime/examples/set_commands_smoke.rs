// SPDX-License-Identifier: Apache-2.0

//! Concurrency-one Linux observation for native set member commands.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

const READ_KEY: &[u8] = b"set-command-read";
const SET_MEMBERS: u32 = 4_096;
const BATCH_MEMBERS: u32 = 32;
const READ_OBSERVATIONS: usize = 100_000;
const READ_WARMUP: usize = 10_000;
const SCAN_OBSERVATIONS: usize = 20_000;
const SCAN_WARMUP: usize = 2_000;
const PREPARE_OBSERVATIONS: u32 = 256;
const MEMORY_COMMIT_OBSERVATIONS: u32 = 32;
const STRICT_COMMIT_OBSERVATIONS: u32 = 16;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-set-commands-{}-{timestamp}",
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
    members_per_call: u32,
}

struct ReadStats {
    snapshot_many: Stats,
    private_many: Stats,
    physical_many: Stats,
    physical_singles: Stats,
    scan_head: Stats,
    scan_middle: Stats,
    scan_tail: Stats,
}

struct MutationStats {
    prepare_add: Stats,
    prepare_remove: Stats,
    memory_add_commit: Stats,
    strict_add_commit: Stats,
    memory_remove_commit: Stats,
}

fn member(index: u32) -> Vec<u8> {
    index.to_be_bytes().to_vec()
}

fn batch_members(start: u32) -> Vec<Vec<u8>> {
    (start..start + BATCH_MEMBERS).map(member).collect()
}

fn indexed_key(prefix: &str, index: u32) -> Vec<u8> {
    format!("{prefix}-{index:04}").into_bytes()
}

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn finish_stats(
    mut samples: Vec<u64>,
    elapsed_seconds: f64,
    members_per_call: u32,
) -> Result<Stats, Box<dyn std::error::Error>> {
    samples.sort_unstable();
    let completed = u32::try_from(samples.len())?;
    Ok(Stats {
        p50_nanos: percentile(&samples, 500),
        p95_nanos: percentile(&samples, 950),
        p99_nanos: percentile(&samples, 990),
        p999_nanos: percentile(&samples, 999),
        maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
        throughput_per_second: f64::from(completed) / elapsed_seconds,
        members_per_call,
    })
}

fn measure<F, T>(
    observations: usize,
    warmup: usize,
    members_per_call: u32,
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
    finish_stats(
        samples,
        total_started.elapsed().as_secs_f64(),
        members_per_call,
    )
}

fn measure_indexed<F, T>(
    observations: u32,
    members_per_call: u32,
    mut operation: F,
) -> Result<Stats, Box<dyn std::error::Error>>
where
    F: FnMut(u32) -> Result<T, NativeRuntimeError>,
{
    let mut samples = Vec::with_capacity(usize::try_from(observations)?);
    let total_started = Instant::now();
    for index in 0..observations {
        let started = Instant::now();
        black_box(operation(index)?);
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    finish_stats(
        samples,
        total_started.elapsed().as_secs_f64(),
        members_per_call,
    )
}

fn seed(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_set(READ_KEY.to_vec())?;
    seed.sadd_many(READ_KEY.to_vec(), (0..SET_MEMBERS).map(member).collect())?;
    for index in 0..PREPARE_OBSERVATIONS {
        seed.create_set(indexed_key("prepare-add", index))?;
        let remove_key = indexed_key("prepare-remove", index);
        seed.create_set(remove_key.clone())?;
        seed.sadd_many(remove_key, batch_members(0))?;
    }
    for index in 0..MEMORY_COMMIT_OBSERVATIONS {
        seed.create_set(indexed_key("commit-memory-add", index))?;
        let remove_key = indexed_key("commit-memory-remove", index);
        seed.create_set(remove_key.clone())?;
        seed.sadd_many(remove_key, batch_members(0))?;
    }
    for index in 0..STRICT_COMMIT_OBSERVATIONS {
        seed.create_set(indexed_key("commit-strict-add", index))?;
    }
    seed.commit()?;
    Ok(())
}

fn validate_routes(
    database: &NativeDatabase,
    probes: &[Vec<u8>],
) -> Result<(), Box<dyn std::error::Error>> {
    if database
        .smismember_latest_set_at(READ_KEY, probes, 101)?
        .iter()
        .any(|present| !present)
    {
        return Err("physical SMISMEMBER missed a seeded member".into());
    }
    for (cursor, expected_first) in [
        (None, 0_u32),
        (Some(2_047_u32.to_be_bytes().to_vec()), 2_048),
        (Some(4_079_u32.to_be_bytes().to_vec()), 4_080),
    ] {
        let page = database.sscan_latest_set_at(READ_KEY, cursor.as_deref(), 16, 101)?;
        if page.len() != 16 || page.first() != Some(&member(expected_first)) {
            return Err("physical SSCAN returned an unexpected page".into());
        }
    }
    Ok(())
}

fn measure_reads(
    database: &NativeDatabase,
    probes: &[Vec<u8>],
) -> Result<ReadStats, Box<dyn std::error::Error>> {
    let snapshot = database.snapshot(101)?;
    let private = database.begin_optimistic(101, DurabilityClass::Memory)?;
    let snapshot_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_MEMBERS, || {
        snapshot.smismember(READ_KEY, probes)
    })?;
    let private_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_MEMBERS, || {
        private.smismember(READ_KEY, probes)
    })?;
    let physical_many = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_MEMBERS, || {
        database.smismember_latest_set_at(READ_KEY, probes, 101)
    })?;
    let physical_singles = measure(READ_OBSERVATIONS, READ_WARMUP, BATCH_MEMBERS, || {
        for probe in probes {
            black_box(database.sismember_latest_set_at(READ_KEY, probe, 101)?);
        }
        Ok(())
    })?;
    let scan_head = measure(SCAN_OBSERVATIONS, SCAN_WARMUP, 16, || {
        database.sscan_latest_set_at(READ_KEY, None, 16, 101)
    })?;
    let middle_cursor = 2_047_u32.to_be_bytes();
    let scan_middle = measure(SCAN_OBSERVATIONS, SCAN_WARMUP, 16, || {
        database.sscan_latest_set_at(READ_KEY, Some(&middle_cursor), 16, 101)
    })?;
    let tail_cursor = 4_079_u32.to_be_bytes();
    let scan_tail = measure(SCAN_OBSERVATIONS, SCAN_WARMUP, 16, || {
        database.sscan_latest_set_at(READ_KEY, Some(&tail_cursor), 16, 101)
    })?;
    Ok(ReadStats {
        snapshot_many,
        private_many,
        physical_many,
        physical_singles,
        scan_head,
        scan_middle,
        scan_tail,
    })
}

fn measure_mutations(
    database: &mut NativeDatabase,
) -> Result<MutationStats, Box<dyn std::error::Error>> {
    let additions = batch_members(10_000);
    let removals = batch_members(0);
    let mut prepare_add = database.begin_optimistic(200, DurabilityClass::Memory)?;
    let prepare_add_stats = measure_indexed(PREPARE_OBSERVATIONS, BATCH_MEMBERS, |index| {
        prepare_add.sadd_many(indexed_key("prepare-add", index), additions.clone())
    })?;
    prepare_add.rollback();

    let mut prepare_remove = database.begin_optimistic(200, DurabilityClass::Memory)?;
    let prepare_remove_stats = measure_indexed(PREPARE_OBSERVATIONS, BATCH_MEMBERS, |index| {
        prepare_remove.srem_many(indexed_key("prepare-remove", index), removals.clone())
    })?;
    prepare_remove.rollback();

    let memory_add_commit = measure_indexed(MEMORY_COMMIT_OBSERVATIONS, BATCH_MEMBERS, |index| {
        let mut transaction = database.begin(300, DurabilityClass::Memory)?;
        transaction.sadd_many(indexed_key("commit-memory-add", index), additions.clone())?;
        transaction.commit()
    })?;
    let strict_add_commit = measure_indexed(STRICT_COMMIT_OBSERVATIONS, BATCH_MEMBERS, |index| {
        let mut transaction = database.begin(301, DurabilityClass::Strict)?;
        transaction.sadd_many(indexed_key("commit-strict-add", index), additions.clone())?;
        transaction.commit()
    })?;
    let memory_remove_commit =
        measure_indexed(MEMORY_COMMIT_OBSERVATIONS, BATCH_MEMBERS, |index| {
            let mut transaction = database.begin(302, DurabilityClass::Memory)?;
            transaction.srem_many(indexed_key("commit-memory-remove", index), removals.clone())?;
            transaction.commit()
        })?;
    Ok(MutationStats {
        prepare_add: prepare_add_stats,
        prepare_remove: prepare_remove_stats,
        memory_add_commit,
        strict_add_commit,
        memory_remove_commit,
    })
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    let p50_nanos = u32::try_from(stats.p50_nanos).map_or(f64::INFINITY, f64::from);
    println!("    \"{name}\": {{");
    println!("      \"members_per_call\": {},", stats.members_per_call);
    println!("      \"p50_nanos\": {},", stats.p50_nanos);
    println!("      \"p95_nanos\": {},", stats.p95_nanos);
    println!("      \"p99_nanos\": {},", stats.p99_nanos);
    println!("      \"p999_nanos\": {},", stats.p999_nanos);
    println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
    println!(
        "      \"p50_nanos_per_member\": {:.3},",
        p50_nanos / f64::from(stats.members_per_call)
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
    seed(&mut database)?;
    let probes = batch_members(1_000);
    validate_routes(&database, &probes)?;
    let reads = measure_reads(&database, &probes)?;
    let mutations = measure_mutations(&mut database)?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.set-commands-smoke.v1\",");
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
    println!("  \"set_members\": {SET_MEMBERS},");
    println!("  \"batch_members\": {BATCH_MEMBERS},");
    println!("  \"read_observations\": {READ_OBSERVATIONS},");
    println!("  \"read_warmup\": {READ_WARMUP},");
    println!("  \"scan_observations\": {SCAN_OBSERVATIONS},");
    println!("  \"scan_warmup\": {SCAN_WARMUP},");
    println!("  \"prepare_observations\": {PREPARE_OBSERVATIONS},");
    println!("  \"memory_commit_observations\": {MEMORY_COMMIT_OBSERVATIONS},");
    println!("  \"strict_commit_observations\": {STRICT_COMMIT_OBSERVATIONS},");
    println!("  \"operations\": {{");
    print_stats("snapshot_smismember_32", reads.snapshot_many, true);
    print_stats("private_smismember_32", reads.private_many, true);
    print_stats("physical_smismember_32", reads.physical_many, true);
    print_stats("physical_32_single_sismember", reads.physical_singles, true);
    print_stats("physical_sscan_head_16", reads.scan_head, true);
    print_stats("physical_sscan_middle_16", reads.scan_middle, true);
    print_stats("physical_sscan_tail_16", reads.scan_tail, true);
    print_stats("private_sadd_many_32_prepare", mutations.prepare_add, true);
    print_stats(
        "private_srem_many_32_prepare",
        mutations.prepare_remove,
        true,
    );
    print_stats(
        "memory_sadd_many_32_commit",
        mutations.memory_add_commit,
        true,
    );
    print_stats(
        "strict_sadd_many_32_commit",
        mutations.strict_add_commit,
        true,
    );
    print_stats(
        "memory_srem_many_32_commit",
        mutations.memory_remove_commit,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}
