// SPDX-License-Identifier: Apache-2.0

//! Evidence harness for the collection document-cap ladder.
//!
//! Ingests a deterministic corpus up to the requested document count and
//! reports ingest throughput, BM25 latency, filtered/faceted integrated
//! latency, and lexical-mode latencies at each rung. Run on dedicated
//! hardware; the receipt feeds the evidence chain that gates raising
//! `MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS`.
//!
//! Usage: `cargo run --release -p hyphae-native-product \
//!   --example collection_scale_evidence -- <documents> <data-dir>`
//!
//! First receipts (c-16 devbox, release, real disk, Group durability):
//!
//! | rung | ingest docs/s | bm25 p50 | filtered p50 | phrase p50 | fuzzy p50 |
//! |------|--------------|----------|--------------|------------|-----------|
//! | 10k  | 309          | 14.7 ms  | 17.6 ms      | 17.7 ms    | 40.4 ms   |
//! | 100k | 48           | 323 ms   | 356 ms       | 329 ms     | 516 ms    |
//! | 100k after scorer work (HYPOST02 corpus) | reused | 65 ms | 101 ms | 86 ms | 194 ms |
//!
//! The first 100k row scaled superlinearly (10x documents -> ~22x query
//! latency). The scorer work recorded in `scale_stage_diagnostic` —
//! self-describing postings, lazy length prescan, linear score merge,
//! dictionary-backed prefix/fuzzy expansion — brought the 100k query
//! ladder to roughly 4x the 10k row, i.e. near-linear.
//!
//! Ingest receipts after point-resolved batch ingest (delta staging, no
//! complete-state load at BEGIN/commit/receipt, coalesced scalar root
//! construction, buffer-pool probes in root construction), same devbox:
//!
//! | rung | ingest docs/s | ingest s | vacuum s | dir after | reopen | bm25 p50 | filtered p50 | phrase p50 | fuzzy p50 |
//! |------|--------------|----------|----------|-----------|--------|----------|--------------|------------|-----------|
//! | 100k | 1,198        | 83.5     | 17.9     | 385 MB    | 11.5 s | 73 ms    | 108 ms       | 97 ms      | 194 ms    |
//! | 250k | 934          | 268      | 50.1     | 2.1 GB    | 52 s   | 207 ms   | 308 ms       | 268 ms     | 634 ms    |
//!
//! After the borrowed-leaf scorer (planning on boundary keys, borrowed
//! posting scans, arena ranking) and single complete-state open:
//!
//! | rung | ingest docs/s | reopen | bm25 p50 | filtered p50 | phrase p50 | fuzzy p50 |
//! |------|--------------|--------|----------|--------------|------------|-----------|
//! | 100k | 1,261        | 12.8 s | 26 ms    | 43 ms        | 28 ms      | 78 ms     |
//! | 250k | 949          | 35.6 s | 63 ms    | 137 ms       | 68 ms      | 227 ms    |
//!
//! 100k -> 250k (2.5x documents): bm25 2.4x, phrase 2.4x, fuzzy 2.9x,
//! filtered+facet 3.2x. The first bm25 sample after a fresh load is a
//! cold-cache outlier (p95 in the seconds) and is reported, not hidden.
//!
//! After eligibility stopped copying keys (posting scans visit in place,
//! sorted bulk set builds), shipped bound 250k, reopened corpora:
//!
//! | rung | bm25 p50 | filtered p50 | phrase p50 | fuzzy p50 |
//! |------|----------|--------------|------------|-----------|
//! | 100k | 19 ms    | 20 ms        | 22 ms      | 80 ms     |
//! | 250k | 41 ms    | 46 ms        | 42 ms      | 211 ms    |
//!
//! Ratios for 2.5x documents: 2.2x / 2.4x / 1.9x / 2.6x. After the
//! dictionary walk stopped materializing entries, fuzzy went 80 -> 19 ms
//! at 100k and 211 -> 61 ms at 250k.
//!
//! The 250k rung ran with the collection bound lifted on the measurement
//! host only; the shipped bound is unchanged until the contract is raised.
//! Open time is dominated by validating every retained committed root:
//! without the post-load `vacuum_pages` + `checkpoint` + `retain_wal`
//! maintenance step a 100k directory holding ~400 unretired batch commits
//! reopened in ~17 minutes (one complete-state validation per commit).
//! Set `HYPHAE_SCALE_SKIP_MAINTENANCE=1` to reproduce that number.

use std::collections::BTreeMap;
use std::time::Instant;

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, CatalogName, CatalogObjectV2,
    DefinitionVersion, FieldSourcePolicy, LexicalIndexPolicy, NamedVectorDefinition,
    ObjectHeaderV2, QualifiedName, SearchCollectionDefinitionV2, SearchFieldDefinitionV2,
    SearchFieldOptions, VectorMetric, VectorSearchPolicy,
};
use hyphae_native_product::{
    LogicalCatalogObject, NativeProduct, ObjectId, ProductDocValue, ProductDocument,
    ProductDurability, ProductLexicalBranch, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchRequest,
};
use hyphae_native_types::{
    EngineKind, FieldId, IntegerWidth, LogicalType, VectorElement, VectorType,
};

const BATCH_DOCUMENTS: usize = 256;

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let documents: usize = arguments
        .next()
        .ok_or("usage: collection_scale_evidence <documents> <data-dir>")?
        .parse()?;
    let data_dir = arguments
        .next()
        .ok_or("usage: collection_scale_evidence <documents> <data-dir>")?;
    let path = std::path::PathBuf::from(data_dir);
    // An existing data dir reopens and skips ingest so the query ladder
    // can be re-measured after scorer changes without a 50-minute reseed.
    let reuse = path.exists();
    let mut product = if reuse {
        NativeProduct::open(&path)?
    } else {
        let mut product = NativeProduct::create(&path)?;
        provision(&mut product)?;
        product
    };
    // HYPHAE_SCALE_APPEND=<n> appends n more documents to an existing
    // corpus (ids continue after `documents`) to profile ingest at scale.
    let append: usize = std::env::var("HYPHAE_SCALE_APPEND")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    // Deterministic corpus: rotating vocabulary + doc values.
    let vocabulary = [
        "engine", "database", "search", "vector", "lexical", "graph", "index", "commit",
        "snapshot", "proof", "catalog", "keyspace", "durable", "bounded", "recall", "postings",
    ];
    let started = Instant::now();
    let mut ingested = if reuse { documents } else { 0_usize };
    let mut batch_id = if reuse {
        u128::try_from(documents.div_ceil(BATCH_DOCUMENTS))?
    } else {
        0_u128
    };
    // Appended runs continue ids/batches after whatever is already there
    // so repeated profiling runs never collide on idempotency markers.
    let existing = if reuse {
        product
            .search_collection(
                ObjectId::new(52)?,
                &ProductSearchRequest {
                    lexical: None,
                    vectors: Vec::new(),
                    filter: ProductSearchFilter::MatchAll,
                    sort: Vec::new(),
                    facets: Vec::new(),
                    range_facets: Vec::new(),
                    aggregations: Vec::new(),
                    limit: 1,
                    fusion: None,
                    parent_dedupe: None,
                    rerank: None,
                    highlight: None,
                    autocut: None,
                    offset: 0,
                },
                1,
            )
            .map_or(documents, |result| result.total_documents)
    } else {
        0
    };
    if reuse && existing > documents {
        ingested = existing;
        batch_id = u128::try_from(existing.div_ceil(BATCH_DOCUMENTS))? + 1;
    }
    let target = ingested.max(documents) + append;
    while ingested < target {
        let count = BATCH_DOCUMENTS.min(target - ingested);
        let mut batch = Vec::with_capacity(count);
        for offset in 0..count {
            let ordinal = ingested + offset;
            let a = vocabulary[ordinal % vocabulary.len()];
            let b = vocabulary[(ordinal / 3 + 5) % vocabulary.len()];
            let c = vocabulary[(ordinal / 7 + 11) % vocabulary.len()];
            batch.push(ProductDocument {
                object_id: ObjectId::new(u128::try_from(ordinal + 1)?)?,
                text: format!("{a} {b} {c} document {ordinal}"),
                doc_values: BTreeMap::from([
                    (
                        "category".to_owned(),
                        ProductDocValue::String(
                            (*vocabulary.get(ordinal % 4).unwrap_or(&"misc")).to_owned(),
                        ),
                    ),
                    (
                        "price".to_owned(),
                        ProductDocValue::Integer(i64::try_from(ordinal % 1_000)?),
                    ),
                ]),
                vectors: BTreeMap::new(),
            });
        }
        batch_id += 1;
        product.ingest_search_batch(
            ObjectId::new(52)?,
            &ProductSearchIngestBatch {
                idempotency_id: batch_id,
                documents: batch,
            },
            i64::try_from(batch_id)?,
            ProductDurability::Group,
        )?;
        ingested += count;
    }
    let ingest_elapsed = started.elapsed();
    // Steady-state maintenance after a bulk load: rebuild the current root
    // into a compact generation (advances the retention floor), publish a
    // synchronized checkpoint at that floor, and retire the WAL prefix. A
    // later open then validates the retained root and replays at most the
    // suffix instead of re-validating one root per ingested batch. The
    // open-time cost without this step is reported separately.
    if (!reuse || append > 0) && std::env::var_os("HYPHAE_SCALE_SKIP_MAINTENANCE").is_none() {
        let maintenance_started = Instant::now();
        let mut admin = product.administration();
        let vacuum = admin.vacuum_pages()?;
        let after_vacuum = maintenance_started.elapsed();
        admin.checkpoint()?;
        admin.retain_wal()?;
        admin.collect_retired_page_generations()?;
        println!(
            "maintenance vacuum_seconds={:.1} vacuum_applied={} checkpoint_retain_seconds={:.1}",
            after_vacuum.as_secs_f64(),
            vacuum.applied,
            maintenance_started
                .elapsed()
                .saturating_sub(after_vacuum)
                .as_secs_f64()
        );
    }

    // Query ladder: plain BM25, filtered+faceted, phrase, fuzzy.
    let request =
        |query: &str, phrase: bool, fuzzy: Option<usize>, filtered: bool| ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: query.to_owned(),
                candidate_limit: 1_000,
                weight: 1,
                operator: None,
                prefix: false,
                fields: Vec::new(),
                fuzzy,
                phrase,
            }),
            vectors: Vec::new(),
            filter: if filtered {
                ProductSearchFilter::Compare {
                    field: "price".to_owned(),
                    operator: hyphae_native_product::ProductSearchOperator::Less,
                    value: ProductDocValue::Integer(500),
                }
            } else {
                ProductSearchFilter::MatchAll
            },
            sort: Vec::new(),
            facets: if filtered {
                vec![hyphae_native_product::ProductFacetRequest {
                    field: "category".to_owned(),
                    limit: 8,
                }]
            } else {
                Vec::new()
            },
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 10,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        };
    let collection = ObjectId::new(52)?;
    let scenarios: Vec<(&str, ProductSearchRequest)> = vec![
        ("bm25", request("database engine", false, None, false)),
        (
            "filtered+facet",
            request("database engine", false, None, true),
        ),
        ("phrase", request("database engine", true, None, false)),
        ("fuzzy1", request("datbase", false, Some(1), false)),
    ];
    if reuse && append > 0 {
        println!(
            "documents={target} ingest=appended appended={append} ingest_seconds={:.1} docs_per_second={:.0}",
            ingest_elapsed.as_secs_f64(),
            (append as f64) / ingest_elapsed.as_secs_f64().max(0.001),
        );
    } else if reuse {
        println!("documents={documents} ingest=reused");
    } else {
        println!(
            "documents={documents} ingest_seconds={:.1} docs_per_second={:.0}",
            ingest_elapsed.as_secs_f64(),
            (documents as f64) / ingest_elapsed.as_secs_f64().max(0.001),
        );
    }
    for (name, search) in scenarios {
        let mut latencies = Vec::with_capacity(16);
        let mut hits = 0_usize;
        for round in 0..16 {
            let begun = Instant::now();
            let result =
                product.search_collection(collection, &search, i64::try_from(batch_id)? + round)?;
            latencies.push(begun.elapsed().as_secs_f64() * 1_000.0);
            hits = result.hits.len();
        }
        latencies.sort_by(f64::total_cmp);
        println!(
            "scenario={name} hits={hits} p50_ms={:.1} p95_ms={:.1}",
            latencies[latencies.len() / 2],
            latencies[(latencies.len() * 95) / 100],
        );
    }
    Ok(())
}

fn provision(product: &mut NativeProduct) -> Result<(), Box<dyn std::error::Error>> {
    let name = |value: &str| CatalogName::unquoted(value);
    let qualified = |value: &str| -> Result<QualifiedName, Box<dyn std::error::Error>> {
        Ok(QualifiedName::new(
            name("main")?,
            name("public")?,
            name(value)?,
        ))
    };
    let header = |id: u128,
                  owner: EngineKind,
                  object: &str,
                  parent: Option<u128>|
     -> Result<ObjectHeaderV2, Box<dyn std::error::Error>> {
        Ok(ObjectHeaderV2 {
            id: ObjectId::new(id)?,
            owner,
            name: qualified(object)?,
            parent: parent.map(ObjectId::new).transpose()?,
            definition_version: DefinitionVersion::FIRST,
        })
    };
    for object in [
        LogicalCatalogObject::V2(CatalogObjectV2::Database(header(
            50,
            EngineKind::Kernel,
            "database",
            None,
        )?)),
        LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(
            51,
            EngineKind::Kernel,
            "schema",
            Some(50),
        )?)),
        LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: header(53, EngineKind::Search, "analyzer", Some(51))?,
            tokenizer: AnalyzerTokenizer::UnicodeWord,
            filters: vec![AnalyzerFilter::Lowercase],
        })),
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                header: header(52, EngineKind::Search, "notes", Some(51))?,
                fields: vec![
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(1)?,
                        name: name("body")?,
                        logical_type: LogicalType::Text,
                        analyzer: Some(ObjectId::new(53)?),
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: false,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::Frequencies,
                        },
                    },
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(2)?,
                        name: name("category")?,
                        logical_type: LogicalType::Text,
                        analyzer: None,
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: true,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::None,
                        },
                    },
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(3)?,
                        name: name("price")?,
                        logical_type: LogicalType::Signed(IntegerWidth::Bits64),
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
                    id: FieldId::new(4)?,
                    name: name("exact")?,
                    vector_type: VectorType::new(VectorElement::Float32, 2)?,
                    metric: VectorMetric::SquaredL2,
                    policy: VectorSearchPolicy::Exact,
                    lifecycle: hyphae_native_catalog::IncrementalVectorLifecycle {
                        delta_max_entries: 1_000,
                        consolidate_after_deltas: 4,
                        retain_generations: 2,
                    },
                }],
                bm25: None,
            },
        )),
    ] {
        product.create_catalog_object_v2(object, ProductDurability::Group)?;
    }
    product.provision_search_collection(ObjectId::new(52)?, 0, ProductDurability::Group)?;
    Ok(())
}
