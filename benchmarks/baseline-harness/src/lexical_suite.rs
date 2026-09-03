// SPDX-License-Identifier: Apache-2.0

//! Lexical BM25 suite: Hyphae native inverted B+tree vs Tantivy.
//!
//! Workload: `documents` synthetic ASCII documents over a shared skewed
//! vocabulary (identical corpora byte-for-byte), then `queries` two-term
//! disjunctive top-10 queries (identical query strings).
//!
//! Measured phases:
//! - `ingest`: batched indexing (1,000 documents per durable commit);
//! - `query_top10`: BM25 top-10, exclusive per-query latency.
//!
//! Fairness notes: both engines tokenize `wNNNNNN` words identically
//! (alphanumeric split + lowercase is a no-op on this corpus). Hyphae runs
//! `match_latest_text` (its physical posting-range path). Tantivy commits
//! durably per batch and queries one merged reader; its default BM25
//! parameters (k1=1.2, b=0.75) equal Hyphae's defaults. Scoring formulas
//! still differ in detail (for example Lucene-style IDF flooring), so this
//! compares end-to-end engine behavior, not formula-identical scores.

use anyhow::Context;
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::{DurabilityClass, ObjectId};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, FAST, STORED, TEXT};
use tantivy::{doc, Index, IndexWriter, ReloadPolicy};

use crate::util::{fresh_dir, synthesize_document, synthesize_query, Recorder, Xorshift};

const INGEST_BATCH: usize = 1_000;
const TOP_K: usize = 10;

pub struct LexicalSuiteConfig {
    pub documents: u64,
    pub vocabulary: u64,
    pub queries: usize,
    pub scratch_root: String,
    pub seed: u64,
}

fn corpus(config: &LexicalSuiteConfig) -> Vec<String> {
    let mut rng = Xorshift::new(config.seed);
    (0..config.documents)
        .map(|_| synthesize_document(&mut rng, config.vocabulary))
        .collect()
}

fn query_set(config: &LexicalSuiteConfig) -> Vec<String> {
    let mut rng = Xorshift::new(config.seed ^ 0x51ed_2701);
    (0..config.queries)
        .map(|_| synthesize_query(&mut rng, config.vocabulary))
        .collect()
}

pub fn run(config: &LexicalSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let documents = corpus(config);
    let queries = query_set(config);
    let hyphae = hyphae_run(config, &documents, &queries).context("hyphae lexical suite")?;
    let tantivy = tantivy_run(config, &documents, &queries).context("tantivy lexical suite")?;
    Ok(serde_json::json!({
        "workload": {
            "documents": config.documents,
            "vocabulary": config.vocabulary,
            "queries": config.queries,
            "top_k": TOP_K,
            "seed": config.seed,
        },
        "hyphae": hyphae,
        "tantivy": tantivy,
    }))
}

fn hyphae_run(
    config: &LexicalSuiteConfig,
    documents: &[String],
    queries: &[String],
) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "lexical-hyphae");
    let mut database = NativeDatabase::create(&path)?;
    let index = ObjectId::new(7_001)?;
    {
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.create_search_index(index, "baseline_lexical")?;
        transaction.commit()?;
    }

    let mut ingest = Recorder::with_capacity(documents.len().div_ceil(INGEST_BATCH));
    let mut ingested = 0_usize;
    while ingested < documents.len() {
        let upper = (ingested + INGEST_BATCH).min(documents.len());
        let range = ingested..upper;
        ingest.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
            for document_id in range.clone() {
                database.stage_delta_index_document(
                    &mut batch,
                    index,
                    (document_id as u64).to_be_bytes().to_vec(),
                    documents[document_id].clone(),
                )?;
            }
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
        ingested = upper;
    }
    let ingest_summary = ingest.summary("ingest_batch_1000");

    let mut search = Recorder::with_capacity(queries.len());
    let mut hits_total = 0_u64;
    for query in queries {
        let hits = search.record(|| database.match_latest_text(index, query, TOP_K))?;
        hits_total += hits.len() as u64;
    }
    let query_summary = search.summary("bm25_top10");

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "hyphae-native-lexical",
        "path": "match_latest_text (physical posting ranges)",
        "hits_total": hits_total,
        "ingest": ingest_summary,
        "query_top10": query_summary,
    }))
}

fn tantivy_run(
    config: &LexicalSuiteConfig,
    documents: &[String],
    queries: &[String],
) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "lexical-tantivy");
    std::fs::create_dir_all(&path)?;
    let mut schema_builder = Schema::builder();
    let id_field = schema_builder.add_u64_field("id", FAST | STORED);
    let body_field = schema_builder.add_text_field("body", TEXT);
    let schema = schema_builder.build();
    let index = Index::create_in_dir(&path, schema)?;
    let mut writer: IndexWriter = index.writer(256 * 1024 * 1024)?;

    let mut ingest = Recorder::with_capacity(documents.len().div_ceil(INGEST_BATCH));
    let mut ingested = 0_usize;
    while ingested < documents.len() {
        let upper = (ingested + INGEST_BATCH).min(documents.len());
        let range = ingested..upper;
        ingest.record(|| -> anyhow::Result<()> {
            for document_id in range.clone() {
                writer.add_document(doc!(
                    id_field => document_id as u64,
                    body_field => documents[document_id].clone(),
                ))?;
            }
            writer.commit()?;
            Ok(())
        })?;
        ingested = upper;
    }
    let ingest_summary = ingest.summary("ingest_batch_1000");

    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    reader.reload()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![body_field]);

    let mut search = Recorder::with_capacity(queries.len());
    let mut hits_total = 0_u64;
    for query_text in queries {
        let query = parser.parse_query(query_text)?;
        let hits = search.record(|| -> anyhow::Result<usize> {
            let top = searcher.search(&query, &TopDocs::with_limit(TOP_K))?;
            Ok(top.len())
        })?;
        hits_total += hits as u64;
    }
    let query_summary = search.summary("bm25_top10");

    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "tantivy",
        "version": tantivy::version_string(),
        "segments": searcher.segment_readers().len(),
        "hits_total": hits_total,
        "ingest": ingest_summary,
        "query_top10": query_summary,
    }))
}
