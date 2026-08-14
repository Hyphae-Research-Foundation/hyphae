// SPDX-License-Identifier: AGPL-3.0-only

//! P0 diagnostic baseline for one bounded embedded structure point read.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;
use serde_json::json;

const GENERATOR: &str = "hyphae-native-performance-structure-v1";
const KEY_COUNT: u32 = 2_048;
const VALUE_BYTES: usize = 64;
const LOGICAL_TIME_MICROS: i64 = 101;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-performance-baseline-{}-{nonce}",
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

fn key(sequence: u32) -> [u8; 4] {
    sequence.to_be_bytes()
}

fn value(sequence: u32) -> [u8; VALUE_BYTES] {
    let identity = sequence.to_le_bytes();
    let mut value = [0_u8; VALUE_BYTES];
    for (offset, byte) in value.iter_mut().enumerate() {
        *byte = identity[offset % identity.len()] ^ u8::try_from(offset).unwrap_or(u8::MAX);
    }
    value
}

fn seed_database(
    path: &Path,
) -> Result<(NativeDatabase, blake3::Hash), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut transaction = database.begin(100, DurabilityClass::Memory)?;
    let mut dataset = blake3::Hasher::new();
    dataset.update(GENERATOR.as_bytes());
    for sequence in 0..KEY_COUNT {
        let key = key(sequence);
        let value = value(sequence);
        dataset.update(&key);
        dataset.update(&value);
        transaction.set(key.to_vec(), value.to_vec(), None)?;
    }
    transaction.commit()?;
    if database.latest_structure_tree_height()? < 2 {
        return Err("performance baseline did not create a multilevel B+tree".into());
    }
    Ok((database, dataset.finalize()))
}

fn parse_positive_argument(
    name: &str,
    value: Option<String>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let value = value.ok_or_else(|| format!("performance baseline requires {name}"))?;
    let parsed = value.parse::<usize>()?;
    if parsed == 0 {
        return Err(format!("performance baseline {name} must be positive").into());
    }
    Ok(parsed)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let source_commit = arguments
        .next()
        .ok_or("performance baseline requires the source commit")?;
    let observations = parse_positive_argument("observations", arguments.next())?;
    let warmup = parse_positive_argument("warmup", arguments.next())?;
    if arguments.next().is_some() {
        return Err("performance baseline received unexpected arguments".into());
    }

    let temporary = TemporaryDirectory::create()?;
    let (database, dataset_digest) = seed_database(temporary.path())?;
    let target_key = key(KEY_COUNT / 2);
    let target_value = value(KEY_COUNT / 2);
    for _ in 0..warmup {
        let observed = database
            .get_latest_structure(black_box(&target_key), black_box(LOGICAL_TIME_MICROS))?;
        if observed.as_deref() != Some(target_value.as_slice()) {
            return Err("performance baseline warmup result diverged".into());
        }
    }

    let measurement_started = Instant::now();
    let mut engine_execution_nanos = 0_u128;
    for _ in 0..observations {
        let operation_started = Instant::now();
        let observed = database
            .get_latest_structure(black_box(&target_key), black_box(LOGICAL_TIME_MICROS))?;
        engine_execution_nanos = engine_execution_nanos
            .checked_add(operation_started.elapsed().as_nanos())
            .ok_or("performance baseline engine clock overflowed")?;
        if black_box(observed.as_deref()) != Some(target_value.as_slice()) {
            return Err("performance baseline measured result diverged".into());
        }
    }
    let elapsed_nanos = measurement_started.elapsed().as_nanos();
    if engine_execution_nanos > elapsed_nanos {
        return Err("performance baseline component clocks exceeded elapsed time".into());
    }
    let result_digest = blake3::hash(&target_value);
    let dataset_bytes = usize::try_from(KEY_COUNT)?
        .checked_mul(target_key.len() + target_value.len())
        .ok_or("performance baseline dataset byte count overflowed")?;

    let output = json!({
        "schema": "hyphae-native-performance-sample-v1",
        "source_commit": source_commit,
        "workload": {
            "class": "foreground-point",
            "engines": ["structures"],
            "operation": "embedded-structure-point-get",
            "parameters": {
                "key_bytes": target_key.len(),
                "value_bytes": target_value.len(),
                "logical_time_micros": LOGICAL_TIME_MICROS,
            },
        },
        "dataset": {
            "generator": GENERATOR,
            "digest": dataset_digest.to_hex().to_string(),
            "records": KEY_COUNT,
            "bytes": dataset_bytes,
        },
        "measurement": {
            "observations": observations,
            "warmup": warmup,
            "concurrency": 1,
            "state": "warm",
            "background_mode": "control",
            "elapsed_nanos": u64::try_from(elapsed_nanos)?,
            "engine_execution_nanos": u64::try_from(engine_execution_nanos)?,
        },
        "correctness": {
            "status": "passed",
            "oracle": "exact-expected-structure-value-v1",
            "result_digest": result_digest.to_hex().to_string(),
        },
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
