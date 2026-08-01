// SPDX-License-Identifier: Apache-2.0

//! One-million-observation warm, concurrency-one native latency smoke.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, NativeDatabase, NativeTransaction, decode_frame,
    encode_frame,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

const OBSERVATIONS: u32 = 1_000_000;
const WARMUP: u32 = 100_000;
const OPERATIONS_PER_OBSERVATION: u32 = 32;
const RELATIONAL_SCALE_ROWS: u32 = 2_048;
const RELATIONAL_TARGET_ROW: u32 = RELATIONAL_SCALE_ROWS / 2;
const STRUCTURE_SCALE_KEYS: u32 = 2_048;
const STRUCTURE_TARGET_KEY: u32 = STRUCTURE_SCALE_KEYS / 2;

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

struct OperationStats {
    structure: Stats,
    structure_btree: Stats,
    prepared_sql: Stats,
    relational_btree: Stats,
    codec_dispatch: Stats,
}

fn seed_scaled_data(
    transaction: &mut NativeTransaction<'_>,
    table: ObjectId,
    dataset_hasher: &mut blake3::Hasher,
) -> Result<(), Box<dyn std::error::Error>> {
    for row in 0..RELATIONAL_SCALE_ROWS {
        let key = row.to_be_bytes();
        let value = vec![u8::try_from(row % 251)?; 96];
        dataset_hasher.update(&key);
        dataset_hasher.update(&value);
        transaction.insert(table, key.to_vec(), value)?;
    }
    for key in 0..STRUCTURE_SCALE_KEYS {
        let key = key.to_be_bytes();
        let value = vec![key[3] % 251; 64];
        dataset_hasher.update(&key);
        dataset_hasher.update(&value);
        transaction.set(key.to_vec(), value, None);
    }
    Ok(())
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
    let mut dataset_hasher = blake3::Hasher::new();
    dataset_hasher.update(b"hyphae-native-microsecond-smoke-v4");
    seed_scaled_data(&mut transaction, table, &mut dataset_hasher)?;
    transaction.set(b"session".to_vec(), vec![7_u8; 64], None);
    transaction.create_search_index(index, "notes")?;
    transaction.index_document(index, b"doc-1".to_vec(), "native rust search")?;
    transaction.commit()?;
    let relational_tree_height = database.latest_relational_tree_height()?;
    if relational_tree_height < 2 {
        return Err("relational benchmark dataset did not produce a multilevel B+tree".into());
    }
    let structure_tree_height = database.latest_structure_tree_height()?;
    if structure_tree_height < 2 {
        return Err("structure benchmark dataset did not produce a multilevel B+tree".into());
    }
    let snapshot = database.snapshot(101)?;
    let prepared = snapshot.prepare_sql("SELECT row FROM accounts WHERE primary_key = ?")?;
    let relational_target = RELATIONAL_TARGET_ROW.to_be_bytes();
    let structure_target = STRUCTURE_TARGET_KEY.to_be_bytes();
    let frame = encode_frame(
        FrameKind::Structure,
        1,
        1,
        b"session",
        DEFAULT_MAX_FRAME_PAYLOAD,
    )?;

    for _ in 0..WARMUP {
        black_box(snapshot.get(black_box(b"session")));
        black_box(database.get_latest_structure(black_box(&structure_target), 101)?);
        black_box(snapshot.execute_prepared_binary(&prepared, black_box(&relational_target))?);
        black_box(database.select_latest_relational(table, black_box(&relational_target))?);
        let decoded = decode_frame(black_box(&frame), DEFAULT_MAX_FRAME_PAYLOAD)?;
        black_box(snapshot.get(black_box(decoded.payload)));
    }

    let structure = measure(|| {
        black_box(snapshot.get(black_box(b"session")));
    });
    let structure_btree = measure(|| {
        black_box(
            database
                .get_latest_structure(black_box(&structure_target), 101)
                .is_ok(),
        );
    });
    let prepared_sql = measure(|| {
        black_box(
            snapshot
                .execute_prepared_binary(&prepared, black_box(&relational_target))
                .is_ok(),
        );
    });
    let relational_btree = measure(|| {
        black_box(
            database
                .select_latest_relational(table, black_box(&relational_target))
                .is_ok(),
        );
    });
    let codec_dispatch = measure(|| {
        if let Ok(decoded) = decode_frame(black_box(&frame), DEFAULT_MAX_FRAME_PAYLOAD) {
            black_box(snapshot.get(black_box(decoded.payload)));
        }
    });
    dataset_hasher.update(b"accounts:mario=active;session=64x07;notes:doc-1");
    let dataset_digest = dataset_hasher.finalize();

    print_report(
        &commit,
        &rustc,
        dataset_digest,
        relational_tree_height,
        structure_tree_height,
        &OperationStats {
            structure,
            structure_btree,
            prepared_sql,
            relational_btree,
            codec_dispatch,
        },
    );
    Ok(())
}

fn print_report(
    commit: &str,
    rustc: &str,
    dataset_digest: blake3::Hash,
    relational_tree_height: usize,
    structure_tree_height: usize,
    operations: &OperationStats,
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.microsecond-smoke.v4\",");
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
    println!(
        "  \"relational_rows\": {},",
        RELATIONAL_SCALE_ROWS.saturating_add(1)
    );
    println!("  \"relational_tree_height\": {relational_tree_height},");
    println!("  \"structure_keys\": {STRUCTURE_SCALE_KEYS},");
    println!("  \"structure_tree_height\": {structure_tree_height},");
    println!("  \"dataset_digest_blake3\": \"{dataset_digest}\",");
    println!("  \"transport_note\": \"codec plus embedded dispatch; no named-pipe transport\",");
    println!("  \"operations\": {{");
    print_stats("embedded_structure_get_64b", operations.structure, true);
    print_stats(
        "buffered_structure_btree_get_64b_multilevel",
        operations.structure_btree,
        true,
    );
    print_stats(
        "embedded_prepared_sql_pk_materialized_scaled_snapshot",
        operations.prepared_sql,
        true,
    );
    print_stats(
        "buffered_relational_btree_pk_multilevel",
        operations.relational_btree,
        true,
    );
    print_stats(
        "local_frame_decode_plus_structure_dispatch_64b",
        operations.codec_dispatch,
        false,
    );
    println!("  }}");
    println!("}}");
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
