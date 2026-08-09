// SPDX-License-Identifier: Apache-2.0

//! Controlled Native G7 benchmark runner.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Barrier, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use hyphae_client::v2::{HyphaeClient, RequestOptions};
use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    NativeProduct, ProductAuthorization, ProductDurabilityPolicy, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductSession, ProductSessionId,
};
use hyphae_native_runtime::{
    AnnSearchOptions, HnswConfig, NativeCommitScheduler, NativeDatabase, NativeSnapshot, Vector,
    VectorMetric,
};
use hyphae_native_types::ObjectId;
use serde_json::json;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

const VERSION: &str = "hyphae-native-g7-receipt-v2";
const DEFAULT_OBSERVATIONS: usize = 1_000_000;
const DEFAULT_WARMUP: usize = 100_000;
const STRUCTURE_KEYS: usize = 2_048;
const SQL_KEYS: usize = 128;
const CLOSURE_SEARCH_DOCUMENTS: usize = 1_000_000;
const CLOSURE_VECTOR_DIMENSION: u16 = 384;
const K: usize = 10;
const ANN_QUERY_BREADTH: usize = 64;
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(10);
const ANN_DELTA_MAX_ENTRIES: u32 = 4_096;
const ANN_CONSOLIDATE_AFTER_DELTAS: u16 = 4_096;
const CORPUS_GENERATOR: &str =
    "hyphae-native-g7-corpus-v2:deterministic-id-linear-vector-and-rare-term";

#[derive(Clone, Copy, Debug)]
struct Stats {
    p50: u64,
    p95: u64,
    p99: u64,
    p999: u64,
    maximum: u64,
    throughput: f64,
}

struct SearchFixture {
    database: NativeDatabase,
    snapshot: NativeSnapshot,
    lexical_index: ObjectId,
    vector_index: ObjectId,
    query: Vector,
    options: AnnSearchOptions,
    recall_at_10: f64,
}

impl SearchFixture {
    fn open_or_create(root: &Path, source_commit: &str) -> Result<Self, Box<dyn Error>> {
        let path = search_seed_path(root, source_commit)?;
        if !path.is_dir() {
            publish_search_seed(&path)?;
        }
        let database =
            NativeDatabase::open(&path).map_err(|error| format!("search seed open: {error}"))?;
        let lexical_index = ObjectId::new(7)?;
        let vector_index = ObjectId::new(8)?;
        Self::from_database(database, lexical_index, vector_index)
    }

    fn from_database(
        database: NativeDatabase,
        lexical_index: ObjectId,
        vector_index: ObjectId,
    ) -> Result<Self, Box<dyn Error>> {
        let snapshot = database.snapshot(0)?;
        let query = Vector::new({
            let mut values = vec![0.0; vector_dimension() as usize];
            values[0] = 1.0;
            values
        })?;
        let options = AnnSearchOptions::new(K, ANN_QUERY_BREADTH, Some(K))?;
        let exact_ids = snapshot
            .search_vector_exact(vector_index, &query, K)?
            .into_iter()
            .map(|hit| hit.object_id)
            .collect::<std::collections::BTreeSet<_>>();
        let approximate = snapshot.search_ann(vector_index, &query, options)?;
        let recalled = approximate
            .hits
            .iter()
            .filter(|hit| exact_ids.contains(&hit.object_id))
            .count();
        if recalled * 20 < K * 19 {
            return Err("ANN recall below G7 floor".into());
        }
        Ok(Self {
            database,
            snapshot,
            lexical_index,
            vector_index,
            query,
            options,
            recall_at_10: recalled as f64 / K as f64,
        })
    }
}

fn publish_search_seed(path: &Path) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("search seed path has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".search-staging-{}-{}",
        std::process::id(),
        unique_nonce()
    ));
    seed_search_database(&staging)?;
    fs::rename(&staging, path)?;
    Ok(())
}

fn seed_search_database(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut database =
        NativeDatabase::create(path).map_err(|error| format!("search seed: {error}"))?;
    let lexical_index = ObjectId::new(7)?;
    let vector_index = ObjectId::new(8)?;
    let document_count = search_documents();
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.create_search_index(lexical_index, "g7_search")?;
    seed.commit()?;
    for batch_start in (0..document_count).step_by(512) {
        let mut batch = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
        for id in batch_start..(batch_start + 512).min(document_count) {
            let document_id = (id as u128 + 1).to_be_bytes();
            let text = if id == document_count / 2 {
                "rare g7 native benchmark term"
            } else {
                "common g7 native benchmark"
            };
            batch.index_document(lexical_index, document_id.to_vec(), text)?;
            batch.set(
                filter_key(&document_id),
                if id % 2 == 0 {
                    b"keep".to_vec()
                } else {
                    b"drop".to_vec()
                },
                None,
            )?;
        }
        batch.commit()?;
    }
    // Build ANN last. Opening any later transaction restores the canonical
    // graph, so seeding lexical batches after this point would rebuild the
    // million-vector index once per batch.
    let mut vectors = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    vectors.create_vector_index_with_lifecycle(
        vector_index,
        "g7_ann",
        vector_dimension(),
        VectorMetric::Cosine,
        HnswConfig::new(8, 32, 16, 512, 7)?,
        ann_lifecycle(),
    )?;
    vectors.upsert_vectors(
        vector_index,
        (0..document_count)
            .map(|id| vector_fixture(id, document_count))
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?,
    )?;
    vectors.commit()?;
    drop(database);
    Ok(())
}

fn search_seed_path(root: &Path, source_commit: &str) -> Result<PathBuf, Box<dyn Error>> {
    let parent = std::env::var_os("HYPHAE_G7_SEARCH_SEED_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("search-seeds"));
    let digest = dataset_digest(source_commit);
    Ok(parent.join(format!("search-{}", digest.to_hex())))
}

fn dataset_digest(source_commit: &str) -> blake3::Hash {
    let documents = search_documents();
    let dimension = vector_dimension();
    blake3::hash(
        format!(
            "{CORPUS_GENERATOR}:source={source_commit}:documents={documents}:vectors={documents}:dimension={dimension}"
        )
        .as_bytes(),
    )
}

#[derive(Clone, Debug)]
struct CounterValue {
    status: &'static str,
    value: Option<u64>,
    unit: &'static str,
    provider: &'static str,
    reason: Option<&'static str>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let allocation_start = GLOBAL_ALLOCATOR.stats();
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("missing exact source commit")?;
    let platform = std::env::args()
        .nth(2)
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let state = std::env::args().nth(3).unwrap_or_else(|| "warm".to_owned());
    let concurrency = std::env::args()
        .nth(4)
        .unwrap_or_else(|| "1".to_owned())
        .parse::<usize>()?;
    let observations = std::env::var("HYPHAE_G7_OBSERVATIONS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_OBSERVATIONS);
    let warmup = std::env::var("HYPHAE_G7_WARMUP")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_WARMUP);
    if !matches!(state.as_str(), "warm" | "cold") || !matches!(concurrency, 1 | 8 | 32) {
        return Err("state must be warm/cold and concurrency must be 1, 8, or 32".into());
    }
    let root = temporary_root(&state, concurrency)?;
    let search = SearchFixture::open_or_create(&root, &source_commit)?;
    let background_enabled =
        std::env::var("HYPHAE_G7_BACKGROUND").is_ok_and(|value| value == "1" || value == "true");
    let background_stop = Arc::new(AtomicBool::new(false));
    let background_thread = background_enabled.then(|| {
        let stop = Arc::clone(&background_stop);
        let path = root.join("background-maintenance");
        thread::spawn(move || -> Result<u64, String> {
            let mut database = NativeDatabase::create(path).map_err(|error| error.to_string())?;
            let mut operations = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                let mut transaction = database
                    .begin(0, hyphae_native_types::DurabilityClass::Memory)
                    .map_err(|error| error.to_string())?;
                transaction
                    .set(operations.to_be_bytes().to_vec(), vec![0x7b; 4_096], None)
                    .map_err(|error| error.to_string())?;
                transaction.commit().map_err(|error| error.to_string())?;
                operations = operations.saturating_add(1);
                if operations.is_multiple_of(64) {
                    black_box(database.checkpoint().map_err(|error| error.to_string())?);
                }
                thread::sleep(BACKGROUND_INTERVAL);
            }
            Ok(operations)
        })
    });
    wait_for_profiler()?;
    let mut receipt = json!({
        "schema": VERSION,
        "gate": "G7",
        "status": "passed",
        "evidence_class": "closure-candidate",
        "source_commit": source_commit,
        "platform": platform,
        "state": state,
        "concurrency": concurrency,
        "dataset": dataset_metadata(&source_commit, observations, warmup),
        "workload": workload_metadata(),
        "durability": {
            "read_seed": "memory-committed",
            "search_seed": "memory-committed",
            "commit_cell": "group-physical-sync",
        },
        "proofs_included": false,
        "correctness": {
            "cell_assertions": "passed",
            "ann_recall_floor": 0.95,
            "cross_engine_visibility": "native-same-snapshot-search",
        },
        "cells": {},
        "counters": counters_process(&root)?,
        "saturation": {"status": "measured", "levels": [1, 8, 32], "method": "requested-concurrency"},
        "background_interference": if background_enabled {
            json!({"status": "measured", "method": "budgeted-native-wal-checkpoint", "workload": "4KiB memory commits at 100 operations/s plus a checkpoint every 64 commits"})
        } else {
            json!({"status": "control", "method": "no-background-worker", "workload": "none"})
        },
        "claims": [],
        "closure_declared": false,
    });
    let mut cells = BTreeMap::new();
    if state == "cold" {
        // Cold state is a fresh process and fresh data directory for this
        // receipt. Warm state retains the process-local seeded handles.
        fs::create_dir_all(root.join("cold-marker"))?;
    }
    cells.insert(
        "embedded-structure-point-get",
        run_embedded_structure(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("embedded structure: {error}"))?,
    );
    cells.insert(
        "embedded-prepared-sql-primary-key",
        run_embedded_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("embedded sql: {error}"))?,
    );
    cells.insert(
        "local-structure-point-get",
        run_local_structure(&root, state == "warm", concurrency, observations, warmup)
            .await
            .map_err(|error| format!("local structure: {error}"))?,
    );
    cells.insert(
        "local-prepared-sql-primary-key",
        run_local_sql(&root, state == "warm", concurrency, observations, warmup)
            .await
            .map_err(|error| format!("local sql: {error}"))?,
    );
    cells.insert(
        "indexed-sql-bounded-read",
        run_indexed_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("indexed sql: {error}"))?,
    );
    cells.insert(
        "two-index-join-bounded-read",
        run_join_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("two-index join: {error}"))?,
    );
    cells.insert(
        "bm25-top10",
        run_bm25(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("bm25: {error}"))?,
    );
    cells.insert(
        "filtered-bm25-top10",
        run_filtered_bm25(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("filtered bm25: {error}"))?,
    );
    cells.insert(
        "ann-top10-recall-095",
        run_ann(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("ann: {error}"))?,
    );
    cells.insert(
        "hybrid-top10",
        run_hybrid(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("hybrid: {error}"))?,
    );
    cells.insert(
        "strict-group-commit",
        run_commit(&root, concurrency, observations)?,
    );
    receipt["cells"] = serde_json::to_value(cells)?;
    receipt["physical_observation"] = physical_observation(&root)?;
    receipt["counters"] = counters_process(&root)?;
    let allocation_change = GLOBAL_ALLOCATOR.stats() - allocation_start;
    receipt["counters"]["allocations"] = counter_json(CounterValue {
        status: "measured",
        value: Some(
            u64::try_from(
                allocation_change
                    .allocations
                    .saturating_add(allocation_change.reallocations),
            )
            .unwrap_or(u64::MAX),
        ),
        unit: "count",
        provider: "stats-alloc-system-wrapper",
        reason: None,
    });
    background_stop.store(true, Ordering::Relaxed);
    if let Some(thread) = background_thread {
        let operations = thread
            .join()
            .map_err(|_| "background worker panicked")?
            .map_err(|error| format!("background maintenance failed: {error}"))?;
        receipt["background_interference"]["operations"] = json!(operations);
    }
    fs::remove_dir_all(&root)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn wait_for_profiler() -> Result<(), Box<dyn Error>> {
    let Some(ready_path) = std::env::var_os("HYPHAE_G7_PROFILE_READY_FILE").map(PathBuf::from)
    else {
        return Ok(());
    };
    let start_path = std::env::var_os("HYPHAE_G7_PROFILE_START_FILE")
        .map(PathBuf::from)
        .ok_or("profile ready file requires a start file")?;
    fs::write(&ready_path, std::process::id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !start_path.is_file() {
        if Instant::now() >= deadline {
            return Err("timed out waiting for the G7 profiler".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn temporary_root(state: &str, concurrency: usize) -> Result<PathBuf, Box<dyn Error>> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let parent = std::env::var_os("HYPHAE_G7_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!("hyphae-g7-{state}-{concurrency}-{timestamp}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn short_endpoint(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "h-g7-{label}-{}-{}",
        std::process::id(),
        unique_nonce()
    ))
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn dataset_metadata(source_commit: &str, observations: usize, warmup: usize) -> serde_json::Value {
    let documents = search_documents();
    let dimension = vector_dimension();
    let digest = dataset_digest(source_commit);
    json!({
        "structure_keys": STRUCTURE_KEYS,
        "search_documents": documents,
        "vector_count": documents,
        "vector_dimension": dimension,
        "observations": observations,
        "warmup": warmup,
        "generator": CORPUS_GENERATOR,
        "digest": digest.to_hex().to_string(),
    })
}

fn workload_metadata() -> serde_json::Value {
    let documents = search_documents();
    json!({
        "structure_keys": STRUCTURE_KEYS,
        "sql_rows": SQL_KEYS,
        "point_value_bytes": 64,
        "search_documents": documents,
        "vector_count": documents,
        "vector_dimension": vector_dimension(),
        "lexical_rare_documents": 1,
        "filtered_documents": documents.div_ceil(2),
        "result_limit": K,
        "lexical_index_state": "committed-hot",
        "vector_index_state": "committed-hot",
    })
}

fn search_documents() -> usize {
    static VALUE: OnceLock<usize> = OnceLock::new();
    *VALUE.get_or_init(|| smoke_override("HYPHAE_G7_SEARCH_DOCUMENTS", CLOSURE_SEARCH_DOCUMENTS))
}

fn vector_dimension() -> u16 {
    static VALUE: OnceLock<u16> = OnceLock::new();
    *VALUE.get_or_init(|| smoke_override("HYPHAE_G7_VECTOR_DIMENSION", CLOSURE_VECTOR_DIMENSION))
}

fn smoke_override<T>(name: &str, closure_value: T) -> T
where
    T: std::str::FromStr + Copy,
{
    if std::env::var("HYPHAE_G7_SMOKE").as_deref() != Ok("1") {
        return closure_value;
    }
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(closure_value)
}

fn counters_unavailable() -> serde_json::Value {
    let reason = "provider not attached to this runner invocation";
    json!({
        "allocations": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "rss": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
        "cpu_cycles": counter_json(CounterValue { status: "unavailable", value: None, unit: "cycles", provider: "none", reason: Some(reason) }),
        "cache_misses": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "page_faults": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "bytes_read": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
        "bytes_written": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
    })
}

fn counters_process(root: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    #[allow(unused_mut)]
    let mut counters = counters_unavailable();
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string(format!("/proc/{}/status", std::process::id()))?;
        if let Some(value) = status.lines().find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|value| value.saturating_mul(1024))
        }) {
            counters["rss"] = counter_json(CounterValue {
                status: "measured",
                value: Some(value),
                unit: "bytes",
                provider: "linux-proc-vmhwm",
                reason: None,
            });
        }
        let io = fs::read_to_string(format!("/proc/{}/io", std::process::id()))?;
        for (source, target, provider) in [
            ("read_bytes", "bytes_read", "linux-proc-read-bytes"),
            ("write_bytes", "bytes_written", "linux-proc-write-bytes"),
        ] {
            let field = if source == "read_bytes" {
                "read_bytes:"
            } else {
                "write_bytes:"
            };
            if let Some(value) = io.lines().find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|value| value.trim().parse::<u64>().ok())
            }) {
                counters[target] = counter_json(CounterValue {
                    status: "measured",
                    value: Some(value),
                    unit: "bytes",
                    provider,
                    reason: None,
                });
            }
        }
        let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id()))?;
        let fields = stat.split_whitespace().collect::<Vec<_>>();
        if fields.len() > 14 {
            let faults = fields[9]
                .parse::<u64>()?
                .saturating_add(fields[11].parse::<u64>()?);
            counters["page_faults"] = counter_json(CounterValue {
                status: "measured",
                value: Some(faults),
                unit: "count",
                provider: "linux-proc-stat",
                reason: None,
            });
        }
    }
    let _ = root;
    Ok(counters)
}

fn counter_json(counter: CounterValue) -> serde_json::Value {
    let mut value = json!({
        "status": counter.status,
        "value": counter.value,
        "unit": counter.unit,
        "provider": counter.provider,
    });
    if let Some(reason) = counter.reason {
        value["reason"] = json!(reason);
    }
    value
}

fn run_embedded_structure(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("structure");
    let mut database =
        NativeDatabase::create(&path).map_err(|error| format!("structure seed: {error}"))?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    for index in 0..STRUCTURE_KEYS {
        seed.set(index.to_be_bytes().to_vec(), vec![0xa5; 64], None)?;
    }
    seed.commit()?;
    let target = (STRUCTURE_KEYS / 2).to_be_bytes();
    if warm {
        for _ in 0..warmup {
            black_box(database.get_latest_structure(&target, 0)?);
        }
    }
    let target_value = [0xa5; 64];
    let stats = measure_concurrent(concurrency, observations, &|| {
        let value = database.get_latest_structure(&target, 0)?;
        if value.as_deref() != Some(target_value.as_slice()) {
            return Err("structure result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    Ok(stats_json(stats))
}

fn run_embedded_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("sql");
    let mut database = NativeDatabase::create(&path)
        .map_err(|error| format!("sql seed {}: {error}", path.display()))?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_items (id, payload) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Binary(vec![0x5a; 64]),
            ],
        )?;
    }
    seed.commit()?;
    let prepared = database.prepare_sql_latest("SELECT id, payload FROM g7_items WHERE id = ?")?;
    let parameters = [hyphae_native_runtime::SqlValue::Signed(
        (SQL_KEYS / 2) as i64,
    )];
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("prepared SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    Ok(stats_json(stats))
}

async fn run_local_structure(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-structure");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local structure seed: {error}"))?;
    let mut session = product_session();
    let mut context = product_context(&session, 1);
    context.durability = ProductDurabilityPolicy::MEMORY;
    product.dispatch(
        &mut session,
        &context,
        ProductOperation::StructureSet {
            key: b"g7-local-structure".to_vec(),
            value: vec![0x3c; 64],
            expires_at_micros: None,
        },
    )?;
    drop(session);
    let endpoint = short_endpoint("structure");
    let daemon = NativeDaemon::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let client = HyphaeClient::local(endpoint.to_string_lossy().into_owned())?;
        let options = RequestOptions::default();
        if warm {
            for _ in 0..warmup {
                require_structure_response(
                    client
                        .structure_get(b"g7-local-structure".to_vec(), options.clone())
                        .await?,
                )?;
            }
        }
        let stats = measure_async(concurrency, observations, || {
            let client = client.clone();
            let options = options.clone();
            async move {
                require_structure_response(
                    client
                        .structure_get(b"g7-local-structure".to_vec(), options)
                        .await?,
                )?;
                Ok::<(), Box<dyn Error>>(())
            }
        })
        .await?;
        Ok::<_, Box<dyn Error>>(stats_json(stats))
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

async fn run_local_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-sql");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local sql seed: {error}"))?;
    seed_product_sql(&mut product)?;
    let endpoint = short_endpoint("sql");
    let daemon = NativeDaemon::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let client = HyphaeClient::local(endpoint.to_string_lossy().into_owned())?;
        let options = RequestOptions::default();
        let prepared = client
            .prepare_sql(
                "SELECT id, payload FROM g7_items WHERE id = ?",
                options.clone(),
            )
            .await?;
        let hyphae_native_product::ProductResponse::PreparedSql { handle, .. } = prepared else {
            return Err("local SQL prepare returned an unexpected response".into());
        };
        if warm {
            for _ in 0..warmup {
                require_sql_response(
                    client
                        .execute_prepared(
                            handle,
                            vec![hyphae_native_product::ProductValue::Signed(
                                (SQL_KEYS / 2) as i64,
                            )],
                            options.clone(),
                        )
                        .await?,
                )?;
            }
        }
        let stats = measure_async(concurrency, observations, || {
            let client = client.clone();
            let options = options.clone();
            async move {
                require_sql_response(
                    client
                        .execute_prepared(
                            handle,
                            vec![hyphae_native_product::ProductValue::Signed(
                                (SQL_KEYS / 2) as i64,
                            )],
                            options,
                        )
                        .await?,
                )?;
                Ok::<(), Box<dyn Error>>(())
            }
        })
        .await?;
        Ok::<_, Box<dyn Error>>(stats_json(stats))
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

fn run_indexed_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("indexed");
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_indexed (id BIGINT PRIMARY KEY, email TEXT NOT NULL, payload BINARY NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_indexed (id, email, payload) VALUES (?, ?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("g7-email-{id}")),
                hyphae_native_runtime::SqlValue::Binary(vec![0x5a; 64]),
            ],
        )?;
    }
    seed.execute_sql(
        "CREATE UNIQUE INDEX g7_indexed_email ON g7_indexed (email)",
        &[],
    )?;
    seed.commit()?;
    let prepared =
        database.prepare_sql_latest("SELECT id, payload FROM g7_indexed WHERE email = ?")?;
    let parameters = [hyphae_native_runtime::SqlValue::Text(format!(
        "g7-email-{}",
        SQL_KEYS / 2
    ))];
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("indexed SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-indexed-sql");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_join_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = root.join("join");
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_profiles (id BIGINT PRIMARY KEY, city TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "CREATE TABLE g7_users (id BIGINT PRIMARY KEY, profile_id BIGINT NOT NULL, email TEXT NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_profiles (id, city) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("city-{id}")),
            ],
        )?;
        seed.execute_sql(
            "INSERT INTO g7_users (id, profile_id, email) VALUES (?, ?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("join-email-{id}")),
            ],
        )?;
    }
    seed.execute_sql(
        "CREATE UNIQUE INDEX g7_users_email ON g7_users (email)",
        &[],
    )?;
    seed.commit()?;
    let prepared = database.prepare_sql_latest(
        "SELECT g7_users.id, g7_profiles.city FROM g7_users INNER JOIN g7_profiles ON g7_users.profile_id = g7_profiles.id WHERE email = ?",
    )?;
    let parameters = [hyphae_native_runtime::SqlValue::Text(format!(
        "join-email-{}",
        SQL_KEYS / 2
    ))];
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("two-index join result mismatch".into());
        }
        Ok(())
    })?;
    Ok(stats_json(stats))
}

fn run_filtered_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if warm {
        for _ in 0..warmup {
            black_box(filtered_bm25_query(
                &fixture.database,
                fixture.lexical_index,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        if filtered_bm25_query(&fixture.database, fixture.lexical_index)? != 1 {
            return Err("filtered BM25 result mismatch".into());
        }
        Ok(())
    })?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-same-snapshot-filtered-bm25");
    value["filter_selectivity"] = json!(0.5);
    value["correctness_scope"] = json!("lexical-and-structure-same-snapshot");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_hybrid(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if warm {
        for _ in 0..warmup {
            black_box(hybrid_query(
                &fixture.database,
                &fixture.snapshot,
                fixture.lexical_index,
                fixture.vector_index,
                &fixture.query,
                fixture.options,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        if hybrid_query(
            &fixture.database,
            &fixture.snapshot,
            fixture.lexical_index,
            fixture.vector_index,
            &fixture.query,
            fixture.options,
        )? < K
        {
            return Err("hybrid result mismatch".into());
        }
        Ok(())
    })?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-same-snapshot-hybrid");
    value["lexical_branch"] = json!(true);
    value["vector_branch"] = json!("ann-exact-rerank");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn filter_key(document_id: &[u8; 16]) -> Vec<u8> {
    let mut key = b"g7-filter:".to_vec();
    key.extend_from_slice(document_id);
    key
}

fn filtered_bm25_query(
    database: &NativeDatabase,
    index: ObjectId,
) -> Result<usize, Box<dyn Error>> {
    let hits = database.match_latest_text(index, "rare", K)?;
    let mut admitted = 0;
    for hit in hits {
        let document_id: [u8; 16] = hit
            .document_id
            .as_slice()
            .try_into()
            .map_err(|_| "filtered BM25 document identity is not 16 bytes")?;
        if database
            .get_latest_structure(&filter_key(&document_id), 0)?
            .as_deref()
            == Some(b"keep".as_slice())
        {
            admitted += 1;
        }
    }
    Ok(admitted)
}

fn hybrid_query(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    lexical_index: ObjectId,
    vector_index: ObjectId,
    query: &Vector,
    options: AnnSearchOptions,
) -> Result<usize, Box<dyn Error>> {
    let lexical = database.match_latest_text(lexical_index, "rare", K)?;
    let vector = snapshot.search_ann(vector_index, query, options)?;
    let mut fused = BTreeMap::<ObjectId, f64>::new();
    for (rank, hit) in lexical.into_iter().enumerate() {
        let document_id: [u8; 16] = hit
            .document_id
            .as_slice()
            .try_into()
            .map_err(|_| "hybrid lexical identity is not 16 bytes")?;
        let object_id = ObjectId::new(u128::from_be_bytes(document_id))?;
        *fused.entry(object_id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    for (rank, hit) in vector.hits.into_iter().enumerate() {
        *fused.entry(hit.object_id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    Ok(fused.len())
}

fn seed_product_sql(product: &mut NativeProduct) -> Result<(), Box<dyn Error>> {
    let mut session = product_session();
    let mut context = product_context(&session, 1);
    context.durability = ProductDurabilityPolicy::MEMORY;
    product.dispatch(
        &mut session,
        &context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)"
                .into(),
            parameters: Vec::new(),
        },
    )?;
    for id in 0..SQL_KEYS {
        let mut context = product_context(&session, id as u128 + 2);
        context.durability = ProductDurabilityPolicy::MEMORY;
        product.dispatch(
            &mut session,
            &context,
            ProductOperation::ExecuteSql {
                statement: "INSERT INTO g7_items (id, payload) VALUES (?, ?)".into(),
                parameters: vec![
                    hyphae_native_product::ProductValue::Signed(id as i64),
                    hyphae_native_product::ProductValue::Binary(vec![0x5a; 64]),
                ],
            },
        )?;
    }
    Ok(())
}

fn product_session() -> ProductSession {
    ProductSession::new(
        ProductSessionId::new(1).expect("nonzero session"),
        ProductPrincipal::new("g7-runner").expect("valid principal"),
        ProductAuthorization::ALL,
    )
}

fn product_context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
}

fn require_structure_response(
    response: hyphae_native_product::ProductResponse,
) -> Result<(), Box<dyn Error>> {
    if !matches!(response, hyphae_native_product::ProductResponse::StructureValue(Some(value)) if value == vec![0x3c; 64])
    {
        return Err("local structure response mismatch".into());
    }
    Ok(())
}

fn require_sql_response(
    response: hyphae_native_product::ProductResponse,
) -> Result<(), Box<dyn Error>> {
    if !matches!(response, hyphae_native_product::ProductResponse::Sql { result: hyphae_native_product::ProductSqlResult::Rows { rows, .. }, .. } if rows.len() == 1)
    {
        return Err("local SQL response mismatch".into());
    }
    Ok(())
}

async fn measure_async<F, Fut>(
    concurrency: usize,
    observations: usize,
    mut operation: F,
) -> Result<Stats, Box<dyn Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), Box<dyn Error>>>,
{
    let started = Instant::now();
    let mut samples = Vec::with_capacity(observations);
    let rounds = observations.div_ceil(concurrency.max(1));
    for _ in 0..rounds {
        let sample = Instant::now();
        let results = join_all((0..concurrency).map(|_| operation())).await;
        let elapsed = sample.elapsed().as_nanos() as u64;
        for result in results {
            result?;
            samples.push(elapsed);
            if samples.len() == observations {
                break;
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    samples.sort_unstable();
    Ok(Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().ok_or("empty async benchmark")?,
        throughput: samples.len() as f64 / elapsed,
    })
}

fn run_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if warm {
        for _ in 0..warmup {
            black_box(
                fixture
                    .database
                    .match_latest_text(fixture.lexical_index, "rare", K)?,
            );
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let hits = fixture
            .database
            .match_latest_text(fixture.lexical_index, "rare", K)?;
        if hits.is_empty() {
            return Err("BM25 result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    Ok(stats_json(stats))
}

fn run_ann(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if warm {
        for _ in 0..warmup {
            black_box(fixture.snapshot.search_ann(
                fixture.vector_index,
                &fixture.query,
                fixture.options,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        black_box(fixture.snapshot.search_ann(
            fixture.vector_index,
            &fixture.query,
            fixture.options,
        )?);
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut output = stats_json(stats);
    output["recall_at_10"] = json!(fixture.recall_at_10);
    Ok(output)
}

fn vector_fixture(
    id: usize,
    document_count: usize,
) -> Result<(ObjectId, Vector), Box<dyn Error>> {
    let mut values = vec![0.0; vector_dimension() as usize];
    values[0] = 1.0;
    values[1] = id as f32 / document_count as f32;
    Ok((ObjectId::new(id as u128 + 1)?, Vector::new(values)?))
}

const fn ann_lifecycle() -> IncrementalVectorLifecycle {
    IncrementalVectorLifecycle {
        delta_max_entries: ANN_DELTA_MAX_ENTRIES,
        consolidate_after_deltas: ANN_CONSOLIDATE_AFTER_DELTAS,
        retain_generations: 1,
    }
}

fn run_commit(
    root: &Path,
    concurrency: usize,
    observations: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let group_path = root.join("group");
    let database =
        NativeDatabase::create(&group_path).map_err(|error| format!("group seed: {error}"))?;
    let scheduler = NativeCommitScheduler::start(
        database,
        hyphae_native_runtime::GroupCommitConfig::new(concurrency, Duration::from_millis(2), 64)?,
    )?;
    let clients = (0..concurrency)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let barrier = Barrier::new(concurrency);
    let started = Instant::now();
    let samples = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for (producer, client) in clients.into_iter().enumerate() {
            let count =
                observations / concurrency + usize::from(producer < observations % concurrency);
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<Vec<u64>, String> {
                let mut samples = Vec::with_capacity(count);
                barrier.wait();
                for sequence in 0..count {
                    let sample = Instant::now();
                    let mut batch = client
                        .begin_optimistic(0, hyphae_native_types::DurabilityClass::Group)
                        .map_err(|error| error.to_string())?;
                    batch
                        .set(
                            format!("g7-group-{producer}-{sequence}").into_bytes(),
                            b"v".to_vec(),
                            None,
                        )
                        .map_err(|error| error.to_string())?;
                    client.submit(batch).map_err(|error| error.to_string())?;
                    samples.push(sample.elapsed().as_nanos() as u64);
                }
                Ok(samples)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "group commit worker panicked".to_owned())?
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let stats = stats_from_samples(samples, started.elapsed().as_secs_f64());
    scheduler.shutdown()?;
    let mut output = stats_json(stats);
    output["group_concurrency"] = json!(concurrency);
    output["durability"] = json!("group-physical-sync");
    Ok(output)
}

fn measure_concurrent(
    concurrency: usize,
    observations: usize,
    operation: &(impl Fn() -> Result<(), Box<dyn Error>> + Sync),
) -> Result<Stats, Box<dyn Error>> {
    if concurrency == 0 || observations < concurrency {
        return Err("concurrent benchmark requires at least one observation per worker".into());
    }
    let barrier = Barrier::new(concurrency);
    let started = Instant::now();
    let samples = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for worker in 0..concurrency {
            let count =
                observations / concurrency + usize::from(worker < observations % concurrency);
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<Vec<u64>, String> {
                let mut samples = Vec::with_capacity(count);
                barrier.wait();
                for _ in 0..count {
                    let sample = Instant::now();
                    operation().map_err(|error| error.to_string())?;
                    samples.push(sample.elapsed().as_nanos() as u64);
                }
                Ok(samples)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "benchmark worker panicked".to_owned())?
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    Ok(stats_from_samples(samples, started.elapsed().as_secs_f64()))
}

fn stats_from_samples(mut samples: Vec<u64>, elapsed_seconds: f64) -> Stats {
    samples.sort_unstable();
    Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().unwrap_or(&0),
        throughput: samples.len() as f64 / elapsed_seconds,
    }
}

fn percentile(values: &[u64], per_mille: usize) -> u64 {
    values[values.len().saturating_sub(1).saturating_mul(per_mille) / 1_000]
}

fn stats_json(stats: Stats) -> serde_json::Value {
    json!({
        "status": "measured",
        "unit": "nanoseconds",
        "p50": stats.p50,
        "p95": stats.p95,
        "p99": stats.p99,
        "p999": stats.p999,
        "maximum": stats.maximum,
        "throughput_per_second": stats.throughput,
    })
}

fn physical_observation(root: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    let database = NativeDatabase::open(root.join("structure"))?;
    let observation = database.physical_observation()?;
    Ok(json!({
        "page_count": observation.page_count,
        "physical_page_reads": observation.physical_page_reads,
        "wal_bytes": observation.wal_bytes,
        "process_full_state_loads": observation.process_full_state_loads,
        "process_full_catalog_loads": observation.process_full_catalog_loads,
    }))
}
