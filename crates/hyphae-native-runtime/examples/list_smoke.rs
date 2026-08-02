// SPDX-License-Identifier: Apache-2.0

//! Focused warm physical-read latency smoke for native chunked lists.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

const LIST_KEY: &[u8] = b"benchmark-list";
const LIST_ELEMENTS: u32 = 2_048;
const ELEMENT_BYTES: usize = 64;
const WARMUP: u32 = 10_000;
const OBSERVATIONS: u32 = 100_000;
const LLEN_OPERATIONS_PER_OBSERVATION: u32 = 16;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-list-smoke-{}-{timestamp}",
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

#[derive(Clone, Copy)]
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    max_nanos: u64,
    throughput_per_second: f64,
}

fn measure(
    mut operation: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
    observations: u32,
    operations_per_observation: u32,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let capacity = usize::try_from(observations)?;
    let mut samples = Vec::with_capacity(capacity);
    let total_started = Instant::now();
    for _ in 0..observations {
        let started = Instant::now();
        for _ in 0..operations_per_observation {
            operation()?;
        }
        let nanos =
            u64::try_from(started.elapsed().as_nanos())? / u64::from(operations_per_observation);
        samples.push(nanos);
    }
    let elapsed = total_started.elapsed().as_secs_f64();
    samples.sort_unstable();
    let percentile = |per_mille: usize| samples[(samples.len() - 1) * per_mille / 1_000];
    Ok(Stats {
        p50_nanos: percentile(500),
        p95_nanos: percentile(950),
        p99_nanos: percentile(990),
        p999_nanos: percentile(999),
        max_nanos: *samples.last().ok_or("missing latency observations")?,
        throughput_per_second: f64::from(observations) * f64::from(operations_per_observation)
            / elapsed,
    })
}

fn warm(database: &NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(database.llen_latest_list(black_box(LIST_KEY))?);
        black_box(database.lrange_latest_list(black_box(LIST_KEY), 0, 9)?);
        black_box(database.lrange_latest_list(black_box(LIST_KEY), -10, -1)?);
    }
    Ok(())
}

fn print_stats(name: &str, stats: Stats, trailing_comma: bool) {
    println!(
        "    \"{name}\": {{\"p50_nanos\": {}, \"p95_nanos\": {}, \
         \"p99_nanos\": {}, \"p999_nanos\": {}, \"max_nanos\": {}, \
         \"throughput_per_second\": {:.3}}}{}",
        stats.p50_nanos,
        stats.p95_nanos,
        stats.p99_nanos,
        stats.p999_nanos,
        stats.max_nanos,
        stats.throughput_per_second,
        if trailing_comma { "," } else { "" }
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(100, DurabilityClass::Strict)?;
    seed.create_list(LIST_KEY.to_vec())?;
    let mut dataset_hasher = blake3::Hasher::new();
    for index in 0..LIST_ELEMENTS {
        let mut value = vec![u8::try_from(index % 251)?; ELEMENT_BYTES];
        value[..4].copy_from_slice(&index.to_be_bytes());
        dataset_hasher.update(&value);
        seed.rpush(LIST_KEY.to_vec(), value)?;
    }
    seed.commit()?;
    drop(database);

    let database = NativeDatabase::open(temporary.path())?;
    if database.llen_latest_list(LIST_KEY)? != usize::try_from(LIST_ELEMENTS)? {
        return Err("reopened benchmark list cardinality is wrong".into());
    }
    warm(&database)?;
    let llen = measure(
        || {
            black_box(database.llen_latest_list(black_box(LIST_KEY))?);
            Ok(())
        },
        OBSERVATIONS,
        LLEN_OPERATIONS_PER_OBSERVATION,
    )?;
    let head_range_10 = measure(
        || {
            black_box(database.lrange_latest_list(black_box(LIST_KEY), 0, 9)?);
            Ok(())
        },
        OBSERVATIONS,
        1,
    )?;
    let tail_range_10 = measure(
        || {
            black_box(database.lrange_latest_list(black_box(LIST_KEY), -10, -1)?);
            Ok(())
        },
        OBSERVATIONS,
        1,
    )?;

    println!("{{");
    println!("  \"schema\": \"hyphae-native-list-smoke-v1\",");
    println!("  \"mode\": \"release-warm-concurrency-1\",");
    println!("  \"observations\": {OBSERVATIONS},");
    println!("  \"warmup\": {WARMUP},");
    println!("  \"list_elements\": {LIST_ELEMENTS},");
    println!("  \"element_bytes\": {ELEMENT_BYTES},");
    println!(
        "  \"dataset_blake3\": \"{}\",",
        dataset_hasher.finalize().to_hex()
    );
    println!(
        "  \"structure_tree_height\": {},",
        database.latest_structure_tree_height()?
    );
    println!("  \"metrics\": {{");
    print_stats("llen_physical", llen, true);
    print_stats("lrange_head_10_physical", head_range_10, true);
    print_stats("lrange_tail_10_physical", tail_range_10, false);
    println!("  }}");
    println!("}}");
    Ok(())
}
