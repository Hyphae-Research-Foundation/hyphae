// SPDX-License-Identifier: Apache-2.0

//! Matched persistent-hash HGET control for whole-hash TTL changes.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

const OBSERVATIONS: usize = 200_000;
const OPERATIONS_PER_OBSERVATION: usize = 32;
const WARMUP: usize = 20_000;
const HASH_FIELDS: u32 = 2_048;
const HASH_KEY: &[u8] = b"hash-ttl-benchmark";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-hash-hget-control-{}-{timestamp}",
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

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unknown".to_owned());
    let harness = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
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
    let target = (HASH_FIELDS / 2).to_be_bytes();
    for _ in 0..WARMUP {
        black_box(database.hget_latest_hash(HASH_KEY, &target)?);
    }
    let mut samples = Vec::with_capacity(OBSERVATIONS);
    let total_started = Instant::now();
    for _ in 0..OBSERVATIONS {
        let started = Instant::now();
        for _ in 0..OPERATIONS_PER_OBSERVATION {
            black_box(database.hget_latest_hash(HASH_KEY, &target)?);
        }
        let elapsed = started.elapsed().as_nanos() / u128::try_from(OPERATIONS_PER_OBSERVATION)?;
        samples.push(u64::try_from(elapsed)?);
    }
    let total = total_started.elapsed();
    samples.sort_unstable();
    let completed = OBSERVATIONS
        .checked_mul(OPERATIONS_PER_OBSERVATION)
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
    println!("{{");
    println!("  \"schema\": \"hyphae.native.hash-hget-control.v1\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"harness_commit\": \"{harness}\",");
    println!("  \"target\": \"x86_64-linux\",");
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"hash_fields\": {HASH_FIELDS},");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"operations_per_observation\": {OPERATIONS_PER_OBSERVATION},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"p50_nanos\": {},", percentile(&samples, 500));
    println!("  \"p95_nanos\": {},", percentile(&samples, 950));
    println!("  \"p99_nanos\": {},", percentile(&samples, 990));
    println!("  \"p999_nanos\": {},", percentile(&samples, 999));
    println!(
        "  \"maximum_nanos\": {},",
        samples.last().ok_or("control produced no samples")?
    );
    println!(
        "  \"throughput_per_second\": {:.3}",
        completed as f64 / total.as_secs_f64()
    );
    println!("}}");
    Ok(())
}
