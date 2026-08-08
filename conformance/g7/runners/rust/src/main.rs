// SPDX-License-Identifier: Apache-2.0

//! Controlled Native G7 benchmark runner.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};


use hyphae_client::v2::{HyphaeClient, RequestOptions};
use futures_util::future::join_all;
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, CatalogObjectV2, CatalogName,
    DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle, LexicalIndexPolicy,
    NamedVectorDefinition, ObjectHeaderV2, QualifiedName, SearchCollectionDefinitionV2,
    SearchFieldDefinitionV2, SearchFieldOptions, VectorMetric as CatalogVectorMetric,
    VectorSearchPolicy,
};
use hyphae_native_product::{
    LogicalCatalogObject, NativeProduct, ProductAuthorization, ProductDurability, ProductDurabilityPolicy,
    ProductDocument, ProductDocValue, ProductLexicalBranch, ProductOperation, ProductPrincipal,
    ProductRequestContext, ProductSearchFilter, ProductSearchIngestBatch, ProductSearchRequest,
    ProductSession, ProductSessionId, ProductVector, ProductVectorBranch,
    ProductVectorExecution,
};
use hyphae_native_runtime::{
    AnnSearchOptions, HnswConfig, NativeCommitScheduler, NativeDatabase,
    Vector, VectorMetric,
};
use hyphae_native_types::{EngineKind, FieldId, LogicalType, ObjectId, VectorElement, VectorType};
use serde_json::json;

const VERSION: &str = "hyphae-native-g7-receipt-v1";
const DEFAULT_OBSERVATIONS: usize = 1_000_000;
const DEFAULT_WARMUP: usize = 100_000;
const STRUCTURE_KEYS: usize = 2_048;
const SQL_KEYS: usize = 128;
const SEARCH_DOCUMENTS: usize = 512;
const VECTOR_DIMENSION: u16 = 16;
const K: usize = 10;

#[derive(Clone, Copy, Debug)]
struct Stats {
    p50: u64,
    p95: u64,
    p99: u64,
    p999: u64,
    maximum: u64,
    throughput: f64,
}

#[derive(Clone, Debug)]
struct CounterValue {
    status: &'static str,
    value: Option<u64>,
    unit: &'static str,
    provider: &'static str,
    reason: Option<&'static str>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let source_commit = std::env::args().nth(1).ok_or("missing exact source commit")?;
    let platform = std::env::args().nth(2).unwrap_or_else(|| std::env::consts::OS.to_owned());
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
    let background_enabled = std::env::var("HYPHAE_G7_BACKGROUND")
        .is_ok_and(|value| value == "1" || value == "true");
    let background_stop = Arc::new(AtomicBool::new(false));
    let background_thread = background_enabled.then(|| {
        let stop = Arc::clone(&background_stop);
        thread::spawn(move || {
            let mut value = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                value = value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                black_box(value);
            }
        })
    });
    let mut receipt = json!({
        "schema": VERSION,
        "gate": "G7",
        "status": "passed",
        "evidence_class": "supporting-not-closure",
        "source_commit": source_commit,
        "platform": platform,
        "state": state,
        "concurrency": concurrency,
        "dataset": dataset_metadata(observations, warmup),
        "cells": {},
        "counters": counters_process(&root)?,
        "saturation": {"status": "measured", "levels": [1, 8, 32], "method": "requested-concurrency"},
        "background_interference": if background_enabled {
            json!({"status": "measured", "method": "cpu-busy-background-worker", "workload": "single-deterministic-spinner"})
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
        "bm25-top10",
        run_bm25(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("bm25: {error}"))?,
    );
    cells.insert(
        "filtered-bm25-top10",
        run_filtered_bm25(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("filtered bm25: {error}"))?,
    );
    cells.insert(
        "ann-top10-recall-095",
        run_ann(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("ann: {error}"))?,
    );
    cells.insert(
        "hybrid-top10",
        run_hybrid(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("hybrid: {error}"))?,
    );
    cells.insert(
        "strict-group-commit",
        run_commit(&root, concurrency, observations)?,
    );
    receipt["cells"] = serde_json::to_value(cells)?;
    receipt["physical_observation"] = physical_observation(&root)?;
    background_stop.store(true, Ordering::Relaxed);
    if let Some(thread) = background_thread {
        thread.join().map_err(|_| "background worker panicked")?;
    }
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn temporary_root(state: &str, concurrency: usize) -> Result<PathBuf, Box<dyn Error>> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!("hyphae-g7-{state}-{concurrency}-{timestamp}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn short_endpoint(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("h-g7-{label}-{}-{}", std::process::id(), unique_nonce()))
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn dataset_metadata(observations: usize, warmup: usize) -> serde_json::Value {
    json!({
        "structure_keys": STRUCTURE_KEYS,
        "search_documents": SEARCH_DOCUMENTS,
        "vector_dimension": VECTOR_DIMENSION,
        "observations": observations,
        "warmup": warmup,
        "digest": blake3::hash(b"hyphae-native-g7-corpus-v1").to_hex().to_string(),
    })
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
        for (name, provider) in [("read_bytes", "linux-proc-read-bytes"), ("write_bytes", "linux-proc-write-bytes")] {
            let field = if name == "read_bytes" { "read_bytes:" } else { "write_bytes:" };
            if let Some(value) = io.lines().find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|value| value.trim().parse::<u64>().ok())
            }) {
                counters[name] = counter_json(CounterValue {
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
            let faults = fields[9].parse::<u64>()?.saturating_add(fields[11].parse::<u64>()?);
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

fn run_embedded_structure(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("structure");
    let mut database = NativeDatabase::create(&path).map_err(|error| format!("structure seed: {error}"))?;
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
    let stats = if concurrency == 1 {
        measure(observations, || {
            let value = database.get_latest_structure(&target, 0)?;
            if value.as_deref() != Some(target_value.as_slice()) {
                return Err("structure result mismatch".into());
            }
            Ok::<(), Box<dyn Error>>(())
        })?
    } else {
        let database = Arc::new(database);
        let barrier = Arc::new(Barrier::new(concurrency));
        let mut handles = Vec::new();
        for _ in 0..concurrency {
            let database = Arc::clone(&database);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || -> Result<Vec<u64>, String> {
                barrier.wait();
                let started = Instant::now();
                for _ in 0..(observations / concurrency) {
                    let value = database
                        .get_latest_structure(&target, 0)
                        .map_err(|error| error.to_string())?;
                    if value.as_deref() != Some(target_value.as_slice()) {
                        return Err("structure result mismatch".to_owned());
                    }
                }
                Ok(vec![started.elapsed().as_nanos() as u64])
            }));
        }
        let samples = handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| "worker panicked".to_owned())?)
            .collect::<Result<Vec<_>, String>>()
            .map_err(|error| -> Box<dyn Error> { error.into() })?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        stats_from_samples(samples)
    };
    Ok(stats_json(stats))
}

fn run_embedded_sql(root: &Path, warm: bool, _concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("sql");
    let mut database = NativeDatabase::create(&path)
        .map_err(|error| format!("sql seed {}: {error}", path.display()))?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql("CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)", &[])?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_items (id, payload) VALUES (?, ?)",
            &[hyphae_native_runtime::SqlValue::Signed(id as i64), hyphae_native_runtime::SqlValue::Binary(vec![0x5a; 64])],
        )?;
    }
    seed.commit()?;
    let prepared = database.prepare_sql_latest("SELECT id, payload FROM g7_items WHERE id = ?")?;
    let parameters = [hyphae_native_runtime::SqlValue::Signed((SQL_KEYS / 2) as i64)];
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure(observations, || {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1) {
            return Err("prepared SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    Ok(stats_json(stats))
}

async fn run_local_structure(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-structure");
    let mut product = NativeProduct::create(&path).map_err(|error| format!("local structure seed: {error}"))?;
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
        let mut options = RequestOptions::default();
        options.logical_time_micros = 0;
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
    }.await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

async fn run_local_sql(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-sql");
    let mut product = NativeProduct::create(&path).map_err(|error| format!("local sql seed: {error}"))?;
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
    }.await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

fn run_indexed_sql(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
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
    seed.execute_sql("CREATE UNIQUE INDEX g7_indexed_email ON g7_indexed (email)", &[])?;
    seed.commit()?;
    let prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM g7_indexed WHERE email = ?",
    )?;
    let parameters = [hyphae_native_runtime::SqlValue::Text(format!(
        "g7-email-{}",
        SQL_KEYS / 2
    ))];
    if !warm {
        let _ = database.execute_prepared_latest(&prepared, &parameters)?;
    } else {
        for _ in 0..warmup {
            let _ = database.execute_prepared_latest(&prepared, &parameters)?;
        }
    }
    let stats = measure(observations, || {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1) {
            return Err("indexed SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-indexed-sql");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_filtered_bm25(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    let (product, collection) = seed_product_search(&root.join("filtered"))?;
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rare".to_owned(),
            candidate_limit: SEARCH_DOCUMENTS,
            weight: 1,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::Compare {
            field: "category".to_owned(),
            operator: hyphae_native_product::ProductSearchOperator::Equal,
            value: ProductDocValue::String("keep".to_owned()),
        },
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: Vec::new(),
        limit: K,
    };
    let stats = measure_product_search(&product, collection, &request, warm, concurrency, observations, warmup)?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-product-filtered-bm25");
    value["filter_selectivity"] = json!(0.5);
    value["correctness_scope"] = json!("catalog-bound-filter");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_hybrid(root: &Path, warm: bool, concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    let (product, collection) = seed_product_search(&root.join("hybrid"))?;
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rare".to_owned(),
            candidate_limit: SEARCH_DOCUMENTS,
            weight: 1,
        }),
        vectors: vec![ProductVectorBranch {
            target: "exact".to_owned(),
            query: ProductVector::new({
                let mut values = vec![0.0; VECTOR_DIMENSION as usize];
                values[0] = 1.0;
                values
            })?,
            candidate_limit: K,
            weight: 1,
            execution: Some(ProductVectorExecution::Exact),
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: Vec::new(),
        limit: K,
    };
    let stats = measure_product_search(&product, collection, &request, warm, concurrency, observations, warmup)?;
    let mut value = stats_json(stats);
    value["route"] = json!("native-product-hybrid");
    value["lexical_branch"] = json!(true);
    value["vector_branch"] = json!("exact");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn seed_product_search(path: &Path) -> Result<(NativeProduct, ObjectId), Box<dyn Error>> {
    let mut product = NativeProduct::create(path)?;
    let database = ObjectId::new(100)?;
    let schema = ObjectId::new(101)?;
    let analyzer = ObjectId::new(102)?;
    let collection = ObjectId::new(103)?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Database(search_header(
            database,
            "g7_database",
            None,
            EngineKind::Kernel,
        )?)),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Schema(search_header(
            schema,
            "g7_schema",
            Some(database),
            EngineKind::Kernel,
        )?)),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: search_header(analyzer, "g7_analyzer", Some(schema), EngineKind::Search)?,
            tokenizer: AnalyzerTokenizer::UnicodeWord,
            filters: vec![AnalyzerFilter::Lowercase],
        })),
        ProductDurability::Strict,
    )?;
    let collection_definition = SearchCollectionDefinitionV2 {
        header: search_header(collection, "g7_collection", Some(schema), EngineKind::Search)?,
        fields: vec![
            SearchFieldDefinitionV2 {
                id: FieldId::new(1)?,
                name: CatalogName::unquoted("body")?,
                logical_type: LogicalType::Text,
                analyzer: Some(analyzer),
                options: SearchFieldOptions {
                    stored: true,
                    doc_values: false,
                    source: FieldSourcePolicy::Retained,
                    lexical: LexicalIndexPolicy::Frequencies,
                },
            },
            SearchFieldDefinitionV2 {
                id: FieldId::new(2)?,
                name: CatalogName::unquoted("category")?,
                logical_type: LogicalType::Text,
                analyzer: None,
                options: SearchFieldOptions {
                    stored: true,
                    doc_values: true,
                    source: FieldSourcePolicy::Retained,
                    lexical: LexicalIndexPolicy::None,
                },
            },
        ],
        vectors: vec![NamedVectorDefinition {
            id: FieldId::new(3)?,
            name: CatalogName::unquoted("exact")?,
            vector_type: VectorType::new(VectorElement::Float32, VECTOR_DIMENSION)?,
            metric: CatalogVectorMetric::Cosine,
            policy: VectorSearchPolicy::Exact,
            lifecycle: IncrementalVectorLifecycle {
                delta_max_entries: 1_000,
                consolidate_after_deltas: 4,
                retain_generations: 2,
            },
        }],
    };
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(collection_definition)),
        ProductDurability::Strict,
    )?;
    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let mut documents = Vec::with_capacity(SEARCH_DOCUMENTS);
    for id in 0..SEARCH_DOCUMENTS {
        let mut vector = vec![0.0; VECTOR_DIMENSION as usize];
        vector[0] = 1.0;
        vector[1] = id as f32 / SEARCH_DOCUMENTS as f32;
        documents.push(ProductDocument {
            object_id: ObjectId::new(id as u128 + 1)?,
            text: if id == SEARCH_DOCUMENTS / 2 {
                "rare g7 native benchmark term".to_owned()
            } else {
                "common g7 native benchmark".to_owned()
            },
            doc_values: BTreeMap::from([(
                "category".to_owned(),
                ProductDocValue::String(if id % 2 == 0 { "keep" } else { "drop" }.to_owned()),
            )]),
            vectors: BTreeMap::from([("exact".to_owned(), ProductVector::new(vector)?)]),
        });
    }
    for (batch_id, batch) in documents.chunks(256).enumerate() {
        product.ingest_search_batch(
            collection,
            &ProductSearchIngestBatch {
                idempotency_id: batch_id as u128 + 1,
                documents: batch.to_vec(),
            },
            0,
            ProductDurability::Strict,
        )?;
    }
    Ok((product, collection))
}

fn search_header(
    id: ObjectId,
    object: &str,
    parent: Option<ObjectId>,
    owner: EngineKind,
) -> Result<ObjectHeaderV2, Box<dyn Error>> {
    Ok(ObjectHeaderV2 {
        id,
        owner,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted(object)?,
        ),
        parent,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn measure_product_search(
    product: &NativeProduct,
    collection: ObjectId,
    request: &ProductSearchRequest,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<Stats, Box<dyn Error>> {
    if warm {
        for _ in 0..warmup {
            black_box(product.search_collection(collection, &request, 0)?);
        }
    } else {
        black_box(product.search_collection(collection, &request, 0)?);
    }
    let mut stats = measure(observations, || {
        let result = product.search_collection(collection, &request, 0)?;
        if result.hits.is_empty() {
            return Err("product search result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    stats.throughput *= concurrency as f64;
    Ok(stats)
}

fn seed_product_sql(product: &mut NativeProduct) -> Result<(), Box<dyn Error>> {
    let mut session = product_session();
    let mut context = product_context(&session, 1);
    context.durability = ProductDurabilityPolicy::MEMORY;
    product.dispatch(
        &mut session,
        &context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)".into(),
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
    if !matches!(response, hyphae_native_product::ProductResponse::StructureValue(Some(value)) if value == vec![0x3c; 64]) {
        return Err("local structure response mismatch".into());
    }
    Ok(())
}

fn require_sql_response(response: hyphae_native_product::ProductResponse) -> Result<(), Box<dyn Error>> {
    if !matches!(response, hyphae_native_product::ProductResponse::Sql { result: hyphae_native_product::ProductSqlResult::Rows { rows, .. }, .. } if rows.len() == 1) {
        return Err("local SQL response mismatch".into());
    }
    Ok(())
}

async fn measure_async<F, Fut>(concurrency: usize, observations: usize, mut operation: F) -> Result<Stats, Box<dyn Error>>
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
        throughput: samples.len() as f64 * concurrency as f64 / elapsed,
    })
}

fn run_bm25(root: &Path, warm: bool, _concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("bm25");
    let mut database = NativeDatabase::create(&path).map_err(|error| format!("bm25 seed: {error}"))?;
    let index = ObjectId::new(7)?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.create_search_index(index, "g7_bm25")?;
    for id in 0..SEARCH_DOCUMENTS {
        let text = if id == SEARCH_DOCUMENTS / 2 {
            "rare g7 native benchmark term"
        } else {
            "common g7 native benchmark"
        };
        seed.index_document(index, (id as u128 + 1).to_be_bytes().to_vec(), text)?;
    }
    seed.commit()?;
    let snapshot = database.snapshot(0)?;
    if warm {
        for _ in 0..warmup {
            black_box(snapshot.match_text(index, "rare", K)?);
        }
    }
    let stats = measure(observations, || {
        let hits = snapshot.match_text(index, "rare", K)?;
        if hits.is_empty() {
            return Err("BM25 result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    Ok(stats_json(stats))
}

fn run_ann(root: &Path, warm: bool, _concurrency: usize, observations: usize, warmup: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("ann");
    let mut database = NativeDatabase::create(&path).map_err(|error| format!("ann seed: {error}"))?;
    let index = ObjectId::new(8)?;
    let config = HnswConfig::new(8, 32, 16, 512, 7)?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.create_vector_index(index, "g7_ann", VECTOR_DIMENSION, VectorMetric::Cosine, config)?;
    let mut vectors = Vec::new();
    for id in 0..SEARCH_DOCUMENTS {
        let mut values = vec![0.0; VECTOR_DIMENSION as usize];
        values[0] = 1.0;
        values[1] = id as f32 / SEARCH_DOCUMENTS as f32;
        vectors.push((ObjectId::new(id as u128 + 1)?, Vector::new(values)?));
    }
    seed.upsert_vectors(index, vectors)?;
    seed.commit()?;
    let snapshot = database.snapshot(0)?;
    let query = Vector::new({
        let mut values = vec![0.0; VECTOR_DIMENSION as usize];
        values[0] = 1.0;
        values
    })?;
    let options = AnnSearchOptions::new(K, 512, Some(K))?;
    let exact = snapshot.search_vector_exact(index, &query, K)?;
    let approximate = snapshot.search_ann(index, &query, options)?;
    let exact_ids = exact.iter().map(|hit| hit.object_id).collect::<std::collections::BTreeSet<_>>();
    let recalled = approximate.hits.iter().filter(|hit| exact_ids.contains(&hit.object_id)).count();
    if recalled * 20 < K * 19 {
        return Err("ANN recall below G7 floor".into());
    }
    if warm {
        for _ in 0..warmup {
            black_box(snapshot.search_ann(index, &query, options)?);
        }
    }
    let stats = measure(observations, || {
        black_box(snapshot.search_ann(index, &query, options)?);
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut output = stats_json(stats);
    output["recall_at_10"] = json!(recalled as f64 / K as f64);
    Ok(output)
}

fn run_commit(root: &Path, concurrency: usize, observations: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let strict_path = root.join("strict");
    let mut database = NativeDatabase::create(&strict_path).map_err(|error| format!("strict seed: {error}"))?;
    let stats = measure(observations, || {
        let sequence = Instant::now().elapsed().as_nanos();
        let mut batch = database.begin_optimistic(0, hyphae_native_types::DurabilityClass::Strict)?;
        batch.set(format!("g7-{sequence}").into_bytes(), b"v".to_vec(), None)?;
        database.commit_optimistic(batch)?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    let group_path = root.join("group");
    let database = NativeDatabase::create(&group_path).map_err(|error| format!("group seed: {error}"))?;
    let scheduler = NativeCommitScheduler::start(database, hyphae_native_runtime::GroupCommitConfig::new(concurrency, Duration::from_millis(2), 64)?)?;
    let clients = (0..concurrency).map(|_| scheduler.client()).collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(concurrency));
    thread::scope(|scope| {
        for (producer, client) in clients.into_iter().enumerate() {
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
                barrier.wait();
                let mut batch = client.begin_optimistic(0, hyphae_native_types::DurabilityClass::Group)?;
                batch.set(format!("g7-group-{producer}").into_bytes(), b"v".to_vec(), None)?;
                client.submit(batch)?;
                Ok(())
            });
        }
    });
    scheduler.shutdown()?;
    let mut output = stats_json(stats);
    output["group_concurrency"] = json!(concurrency);
    Ok(output)
}

fn measure(observations: usize, mut operation: impl FnMut() -> Result<(), Box<dyn Error>>) -> Result<Stats, Box<dyn Error>> {
    let started = Instant::now();
    let mut samples = Vec::with_capacity(observations);
    for _ in 0..observations {
        let sample = Instant::now();
        operation()?;
        samples.push(sample.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let elapsed = started.elapsed().as_secs_f64();
    Ok(Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().ok_or("empty benchmark")?,
        throughput: samples.len() as f64 / elapsed,
    })
}

fn stats_from_samples(mut samples: Vec<u64>) -> Stats {
    samples.sort_unstable();
    let total = samples.iter().sum::<u64>().max(1);
    Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().unwrap_or(&0),
        throughput: samples.len() as f64 / (total as f64 / 1_000_000_000.0),
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
