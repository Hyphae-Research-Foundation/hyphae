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
    if path.exists() {
        return Err("data dir must not exist".into());
    }

    let mut product = NativeProduct::create(&path)?;
    provision(&mut product)?;

    // Deterministic corpus: rotating vocabulary + doc values.
    let vocabulary = [
        "engine", "database", "search", "vector", "lexical", "graph", "index", "commit",
        "snapshot", "proof", "catalog", "keyspace", "durable", "bounded", "recall", "postings",
    ];
    let started = Instant::now();
    let mut ingested = 0_usize;
    let mut batch_id = 0_u128;
    while ingested < documents {
        let count = BATCH_DOCUMENTS.min(documents - ingested);
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
    println!(
        "documents={documents} ingest_seconds={:.1} docs_per_second={:.0}",
        ingest_elapsed.as_secs_f64(),
        (documents as f64) / ingest_elapsed.as_secs_f64().max(0.001),
    );
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
