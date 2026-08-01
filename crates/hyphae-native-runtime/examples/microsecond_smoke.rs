// SPDX-License-Identifier: Apache-2.0

//! One-million-observation warm, concurrency-one native latency smoke.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, NativeDatabase, decode_frame, encode_frame,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

const OBSERVATIONS: u32 = 1_000_000;
const WARMUP: u32 = 100_000;
const OPERATIONS_PER_OBSERVATION: u32 = 32;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-microsecond-smoke-{}-{timestamp}",
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
    throughput_per_second: f64,
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
    let table = ObjectId::new(1)?;
    let index = ObjectId::new(2)?;
    let mut transaction = database.begin(100, DurabilityClass::Memory)?;
    transaction.create_relation(table, "accounts")?;
    transaction.insert(table, b"mario".to_vec(), b"active".to_vec())?;
    transaction.set(b"session".to_vec(), vec![7_u8; 64], None);
    transaction.create_search_index(index, "notes")?;
    transaction.index_document(index, b"doc-1".to_vec(), "native rust search")?;
    transaction.commit()?;
    let snapshot = database.snapshot(101)?;
    let prepared = snapshot.prepare_sql("SELECT row FROM accounts WHERE primary_key = ?")?;
    let frame = encode_frame(
        FrameKind::Structure,
        1,
        1,
        b"session",
        DEFAULT_MAX_FRAME_PAYLOAD,
    )?;

    for _ in 0..WARMUP {
        black_box(snapshot.get(black_box(b"session")));
        black_box(snapshot.execute_prepared_binary(&prepared, black_box(b"mario"))?);
        let decoded = decode_frame(black_box(&frame), DEFAULT_MAX_FRAME_PAYLOAD)?;
        black_box(snapshot.get(black_box(decoded.payload)));
    }

    let structure = measure(|| {
        black_box(snapshot.get(black_box(b"session")));
    });
    let prepared_sql = measure(|| {
        black_box(
            snapshot
                .execute_prepared_binary(&prepared, black_box(b"mario"))
                .is_ok(),
        );
    });
    let codec_dispatch = measure(|| {
        if let Ok(decoded) = decode_frame(black_box(&frame), DEFAULT_MAX_FRAME_PAYLOAD) {
            black_box(snapshot.get(black_box(decoded.payload)));
        }
    });
    let dataset_digest = blake3::hash(b"accounts:mario=active;session=64x07;notes:doc-1");

    println!("{{");
    println!("  \"schema\": \"hyphae.native.microsecond-smoke.v1\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"observations_per_operation\": {OBSERVATIONS},");
    println!("  \"operations_per_observation\": {OPERATIONS_PER_OBSERVATION},");
    println!("  \"warmup_per_operation\": {WARMUP},");
    println!("  \"concurrency\": 1,");
    println!("  \"durability\": \"memory\",");
    println!("  \"warm_state\": true,");
    println!("  \"proofs\": false,");
    println!("  \"dataset_digest_blake3\": \"{dataset_digest}\",");
    println!("  \"transport_note\": \"codec plus embedded dispatch; no named-pipe transport\",");
    println!("  \"operations\": {{");
    print_stats("embedded_structure_get_64b", structure, true);
    print_stats("embedded_prepared_sql_pk", prepared_sql, true);
    print_stats(
        "local_frame_decode_plus_structure_dispatch_64b",
        codec_dispatch,
        false,
    );
    println!("  }}");
    println!("}}");
    Ok(())
}

fn measure(mut operation: impl FnMut()) -> Stats {
    let mut observations = Vec::with_capacity(usize::try_from(OBSERVATIONS).unwrap_or(0));
    let aggregate_start = Instant::now();
    for _ in 0..OBSERVATIONS {
        let start = Instant::now();
        for _ in 0..OPERATIONS_PER_OBSERVATION {
            operation();
        }
        observations.push(
            u64::try_from(start.elapsed().as_nanos() / u128::from(OPERATIONS_PER_OBSERVATION))
                .unwrap_or(u64::MAX),
        );
    }
    let aggregate_elapsed = aggregate_start.elapsed();
    observations.sort_unstable();
    Stats {
        p50_nanos: percentile(&observations, 500),
        p95_nanos: percentile(&observations, 950),
        p99_nanos: percentile(&observations, 990),
        p999_nanos: percentile(&observations, 999),
        throughput_per_second: f64::from(OBSERVATIONS * OPERATIONS_PER_OBSERVATION)
            / aggregate_elapsed.as_secs_f64(),
    }
}

fn percentile(observations: &[u64], permille: usize) -> u64 {
    let last = observations.len().saturating_sub(1);
    let index = last.saturating_mul(permille).saturating_add(999) / 1000;
    observations.get(index.min(last)).copied().unwrap_or(0)
}

fn micros(nanos: u64) -> f64 {
    Duration::from_nanos(nanos).as_secs_f64() * 1_000_000.0
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    let suffix = if comma { "," } else { "" };
    println!("    \"{name}\": {{");
    println!("      \"p50_us\": {:.3},", micros(stats.p50_nanos));
    println!("      \"p95_us\": {:.3},", micros(stats.p95_nanos));
    println!("      \"p99_us\": {:.3},", micros(stats.p99_nanos));
    println!("      \"p99_9_us\": {:.3},", micros(stats.p999_nanos));
    println!(
        "      \"throughput_ops_s\": {:.0}",
        stats.throughput_per_second
    );
    println!("    }}{suffix}");
}
