// SPDX-License-Identifier: GPL-3.0-only

//! Matched persistent-set SISMEMBER control for whole-set TTL changes.

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
const SET_MEMBERS: u32 = 2_048;
const SET_KEY: &[u8] = b"set-ttl-benchmark";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-set-sismember-control-{}-{timestamp}",
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
    seed.create_set(SET_KEY.to_vec())?;
    for member in 0..SET_MEMBERS {
        if !seed.sadd(SET_KEY.to_vec(), member.to_be_bytes().to_vec())? {
            return Err("control set rejected a unique member".into());
        }
    }
    seed.commit()?;
    let target = (SET_MEMBERS / 2).to_be_bytes();
    for _ in 0..WARMUP {
        black_box(database.sismember_latest_set(SET_KEY, &target)?);
    }
    let mut samples = Vec::with_capacity(OBSERVATIONS);
    let total_started = Instant::now();
    for _ in 0..OBSERVATIONS {
        let started = Instant::now();
        for _ in 0..OPERATIONS_PER_OBSERVATION {
            black_box(database.sismember_latest_set(SET_KEY, &target)?);
        }
        let elapsed = started.elapsed().as_nanos() / u128::try_from(OPERATIONS_PER_OBSERVATION)?;
        samples.push(u64::try_from(elapsed)?);
    }
    let total = total_started.elapsed();
    samples.sort_unstable();
    let completed = OBSERVATIONS
        .checked_mul(OPERATIONS_PER_OBSERVATION)
        .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
    let completed = u32::try_from(completed)?;
    println!("{{");
    println!("  \"schema\": \"hyphae.native.set-sismember-control.v1\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"harness_commit\": \"{harness}\",");
    println!("  \"target\": \"x86_64-linux\",");
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"set_members\": {SET_MEMBERS},");
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
        f64::from(completed) / total.as_secs_f64()
    );
    println!("}}");
    Ok(())
}
