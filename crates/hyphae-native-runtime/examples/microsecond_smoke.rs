// SPDX-License-Identifier: Apache-2.0

//! One-million-observation warm, concurrency-one native latency smoke.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, NativeDatabase, NativeSnapshot, NativeTransaction,
    PreparedStatement, decode_frame, encode_frame,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

const OBSERVATIONS: u32 = 1_000_000;
const WARMUP: u32 = 100_000;
const OPERATIONS_PER_OBSERVATION: u32 = 32;
const RELATIONAL_SCALE_ROWS: u32 = 2_048;
const RELATIONAL_TARGET_ROW: u32 = RELATIONAL_SCALE_ROWS / 2;
const STRUCTURE_SCALE_KEYS: u32 = 2_048;
const STRUCTURE_TARGET_KEY: u32 = STRUCTURE_SCALE_KEYS / 2;
const HASH_SCALE_FIELDS: u32 = 2_048;
const HASH_TARGET_FIELD: u32 = HASH_SCALE_FIELDS / 2;
const HASH_KEY: &[u8] = b"benchmark-hash";
const SEARCH_SCALE_DOCUMENTS: u32 = 2_048;
const SEARCH_TARGET_DOCUMENT: u32 = SEARCH_SCALE_DOCUMENTS / 2;
const SEARCH_OBSERVATIONS: u32 = 100_000;
const SEARCH_OPERATIONS_PER_OBSERVATION: u32 = 1;
const SEARCH_QUERY: &str = "needle";

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
    hash: Stats,
    hash_btree: Stats,
    search_btree: Stats,
    prepared_sql: Stats,
    relational_btree: Stats,
    codec_dispatch: Stats,
}

struct BenchmarkInputs<'a> {
    prepared: &'a PreparedStatement,
    table: ObjectId,
    search_index: ObjectId,
    relational_target: &'a [u8],
    structure_target: &'a [u8],
    hash_target: &'a [u8],
    frame: &'a [u8],
}

fn seed_scaled_data(
    transaction: &mut NativeTransaction<'_>,
    table: ObjectId,
    search_index: ObjectId,
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
        transaction.set(key.to_vec(), value, None)?;
    }
    transaction.create_hash(HASH_KEY.to_vec())?;
    for field in 0..HASH_SCALE_FIELDS {
        let field = field.to_be_bytes();
        let value = vec![field[3] % 251; 64];
        dataset_hasher.update(&field);
        dataset_hasher.update(&value);
        transaction.hset(HASH_KEY.to_vec(), field.to_vec(), value)?;
    }
    for document in 0..SEARCH_SCALE_DOCUMENTS {
        let document_id = document.to_be_bytes();
        let text = if document == SEARCH_TARGET_DOCUMENT {
            format!("needle native search common document{document}")
        } else {
            format!("native search common document{document}")
        };
        dataset_hasher.update(&document_id);
        dataset_hasher.update(text.as_bytes());
        transaction.index_document(search_index, document_id.to_vec(), text)?;
    }
    Ok(())
}

fn measure_operations(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    inputs: &BenchmarkInputs<'_>,
) -> Result<OperationStats, Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(snapshot.get(black_box(b"session")));
        black_box(database.get_latest_structure(black_box(inputs.structure_target), 101)?);
        black_box(snapshot.hget(black_box(HASH_KEY), black_box(inputs.hash_target))?);
        black_box(database.hget_latest_hash(black_box(HASH_KEY), black_box(inputs.hash_target))?);
        black_box(database.match_latest_text(
            inputs.search_index,
            black_box(SEARCH_QUERY),
            black_box(1),
        )?);
        black_box(
            snapshot
                .execute_prepared_binary(inputs.prepared, black_box(inputs.relational_target))?,
        );
        black_box(
            database.select_latest_relational(inputs.table, black_box(inputs.relational_target))?,
        );
        let decoded = decode_frame(black_box(inputs.frame), DEFAULT_MAX_FRAME_PAYLOAD)?;
        black_box(snapshot.get(black_box(decoded.payload)));
    }

    Ok(OperationStats {
        structure: measure(|| {
            black_box(snapshot.get(black_box(b"session")));
        }),
        structure_btree: measure(|| {
            black_box(
                database
                    .get_latest_structure(black_box(inputs.structure_target), 101)
                    .is_ok(),
            );
        }),
        hash: measure(|| {
            black_box(
                snapshot
                    .hget(black_box(HASH_KEY), black_box(inputs.hash_target))
                    .is_ok(),
            );
        }),
        hash_btree: measure(|| {
            black_box(
                database
                    .hget_latest_hash(black_box(HASH_KEY), black_box(inputs.hash_target))
                    .is_ok(),
            );
        }),
        search_btree: measure_counted(
            || {
                black_box(
                    database
                        .match_latest_text(
                            inputs.search_index,
                            black_box(SEARCH_QUERY),
                            black_box(1),
                        )
                        .is_ok(),
                );
            },
            SEARCH_OBSERVATIONS,
            SEARCH_OPERATIONS_PER_OBSERVATION,
        ),
        prepared_sql: measure(|| {
            black_box(
                snapshot
                    .execute_prepared_binary(inputs.prepared, black_box(inputs.relational_target))
                    .is_ok(),
            );
        }),
        relational_btree: measure(|| {
            black_box(
                database
                    .select_latest_relational(inputs.table, black_box(inputs.relational_target))
                    .is_ok(),
            );
        }),
        codec_dispatch: measure(|| {
            if let Ok(decoded) = decode_frame(black_box(inputs.frame), DEFAULT_MAX_FRAME_PAYLOAD) {
                black_box(snapshot.get(black_box(decoded.payload)));
            }
        }),
    })
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
    transaction.create_search_index(index, "notes")?;
    let mut dataset_hasher = blake3::Hasher::new();
    dataset_hasher.update(b"hyphae-native-microsecond-smoke-v6");
    seed_scaled_data(&mut transaction, table, index, &mut dataset_hasher)?;
    transaction.set(b"session".to_vec(), vec![7_u8; 64], None)?;
    transaction.commit()?;
    let relational_tree_height = database.latest_relational_tree_height()?;
    if relational_tree_height < 2 {
        return Err("relational benchmark dataset did not produce a multilevel B+tree".into());
    }
    let structure_tree_height = database.latest_structure_tree_height()?;
    if structure_tree_height < 2 {
        return Err("structure benchmark dataset did not produce a multilevel B+tree".into());
    }
    let search_tree_height = database.latest_search_tree_height()?;
    if search_tree_height < 2 {
        return Err("search benchmark dataset did not produce a multilevel B+tree".into());
    }
    if database.match_latest_text(index, SEARCH_QUERY, 1)?[0].document_id
        != SEARCH_TARGET_DOCUMENT.to_be_bytes()
    {
        return Err("physical search benchmark target did not rank first".into());
    }
    let snapshot = database.snapshot(101)?;
    let prepared = snapshot.prepare_sql("SELECT row FROM accounts WHERE primary_key = ?")?;
    let relational_target = RELATIONAL_TARGET_ROW.to_be_bytes();
    let structure_target = STRUCTURE_TARGET_KEY.to_be_bytes();
    let hash_target = HASH_TARGET_FIELD.to_be_bytes();
    let frame = encode_frame(
        FrameKind::Structure,
        1,
        1,
        b"session",
        DEFAULT_MAX_FRAME_PAYLOAD,
    )?;

    let operations = measure_operations(
        &database,
        &snapshot,
        &BenchmarkInputs {
            prepared: &prepared,
            table,
            search_index: index,
            relational_target: &relational_target,
            structure_target: &structure_target,
            hash_target: &hash_target,
            frame: &frame,
        },
    )?;
    dataset_hasher.update(b"accounts:mario=active;session=64x07");
    let dataset_digest = dataset_hasher.finalize();

    print_report(
        &commit,
        &rustc,
        dataset_digest,
        relational_tree_height,
        structure_tree_height,
        search_tree_height,
        &operations,
    );
    Ok(())
}

fn print_report(
    commit: &str,
    rustc: &str,
    dataset_digest: blake3::Hash,
    relational_tree_height: usize,
    structure_tree_height: usize,
    search_tree_height: usize,
    operations: &OperationStats,
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.microsecond-smoke.v6\",");
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
    println!("  \"search_observations\": {SEARCH_OBSERVATIONS},");
    println!("  \"search_operations_per_observation\": {SEARCH_OPERATIONS_PER_OBSERVATION},");
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
    println!("  \"hash_fields\": {HASH_SCALE_FIELDS},");
    println!("  \"search_documents\": {SEARCH_SCALE_DOCUMENTS},");
    println!("  \"search_tree_height\": {search_tree_height},");
    println!("  \"search_query_document_frequency\": 1,");
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
        "embedded_hash_hget_64b_materialized_scaled_snapshot",
        operations.hash,
        true,
    );
    print_stats(
        "buffered_hash_hget_64b_multilevel",
        operations.hash_btree,
        true,
    );
    print_stats(
        "buffered_inverted_btree_bm25_match_top1_rare_term",
        operations.search_btree,
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
    measure_counted(&mut operation, OBSERVATIONS, OPERATIONS_PER_OBSERVATION)
}

fn measure_counted(
    mut operation: impl FnMut(),
    observation_count: u32,
    operations_per_observation: u32,
) -> Stats {
    let mut observations = Vec::with_capacity(usize::try_from(observation_count).unwrap_or(0));
    let aggregate_start = Instant::now();
    for _ in 0..observation_count {
        let start = Instant::now();
        for _ in 0..operations_per_observation {
            operation();
        }
        observations.push(
            u64::try_from(start.elapsed().as_nanos() / u128::from(operations_per_observation))
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
        throughput_per_second: f64::from(observation_count * operations_per_observation)
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
