// SPDX-License-Identifier: Apache-2.0

//! Stage-by-stage latency diagnostic against an existing scale corpus.
//!
//! Opens the data dir produced by `collection_scale_evidence` and times the
//! individual stages of one integrated BM25 query so cap-ladder work attacks
//! the real bottleneck instead of a guess.
//!
//! Usage: `cargo run --release -p hyphae-native-product \
//!   --example scale_stage_diagnostic -- <data-dir>`
//!
//! First receipt at the 100k rung (c-16 devbox, release):
//!
//! | stage                  | latency        |
//! |------------------------|----------------|
//! | durable posting scorer | ~260 ms        |
//! | retained-model scorer  | ~245,000 ms    |
//! | complete integrated    | ~200 ms        |
//! | integrated + Eq filter | ~230 ms        |
//!
//! The integrated pipeline adds almost nothing over the durable scorer:
//! the scorer itself is the 100k bottleneck (the model fallback is 1000x
//! worse and exists only for fail-open equivalence). The next cap rung
//! therefore needs posting-segment skipping (block-max style) in
//! `match_btree_text_profiled`, not product-pipeline work.

use std::time::Instant;

use hyphae_native_product::{
    NativeProduct, ObjectId, ProductDocValue, ProductLexicalBranch, ProductSearchFilter,
    ProductSearchRequest,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = std::env::args()
        .nth(1)
        .ok_or("usage: scale_stage_diagnostic <data-dir>")?;
    let product = NativeProduct::open(std::path::Path::new(&data_dir))?;
    let collection = ObjectId::new(52)?;
    let binding = product.resolve_search_collection_binding(collection, 1_000_000)?;

    // Stage 1: snapshot + raw durable posting scorer (bypasses the product
    // pipeline entirely).
    let snapshot = product.snapshot_bounded(1_000_001)?;
    for round in 0..3 {
        let begun = Instant::now();
        let hits = product.match_text_at_snapshot_for_diagnostics(
            &snapshot,
            binding.lexical_index,
            "database engine",
            1_000,
        )?;
        println!(
            "stage=durable_scorer round={round} hits={} ms={:.1}",
            hits,
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }

    // Stage 2: the retained-model scorer for comparison.
    for round in 0..3 {
        let begun = Instant::now();
        let hits =
            snapshot.match_text_for_diagnostics(binding.lexical_index, "database engine", 1_000)?;
        println!(
            "stage=model_scorer round={round} hits={} ms={:.1}",
            hits,
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    drop(snapshot);

    // Stage 3: the complete integrated request.
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "database engine".to_owned(),
            candidate_limit: 1_000,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
    for round in 0..3 {
        let begun = Instant::now();
        let result = product.search_collection(collection, &request, 1_000_002 + round)?;
        println!(
            "stage=integrated round={round} hits={} ms={:.1}",
            result.hits.len(),
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }

    // Stage 4: an Equal filter that the posting index accelerates, to
    // separate manifest cloning from posting scans.
    let filtered = ProductSearchRequest {
        filter: ProductSearchFilter::Compare {
            field: "category".to_owned(),
            operator: hyphae_native_product::ProductSearchOperator::Equal,
            value: ProductDocValue::String("engine".to_owned()),
        },
        ..request
    };
    for round in 0..3 {
        let begun = Instant::now();
        let result = product.search_collection(collection, &filtered, 1_000_010 + round)?;
        println!(
            "stage=integrated_equal_filter round={round} hits={} ms={:.1}",
            result.hits.len(),
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    Ok(())
}
