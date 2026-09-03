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
//! Receipts at the 100k rung (c-16 devbox, release):
//!
//! | stage                  | baseline    | dense prescan | HYPOST02 corpus | borrowed leaves |
//! |------------------------|-------------|---------------|-----------------|-----------------|
//! | durable posting scorer | ~260 ms     | ~140 ms       | ~47 ms          | ~8 ms           |
//! | complete integrated    | ~200 ms     | ~165 ms       | ~71 ms          | ~28 ms          |
//! | integrated + Eq filter | ~230 ms     | ~180 ms       | ~80 ms          | ~34 ms          |
//! | integrated sparse      | —           | ~16 ms        | ~17 ms          | ~15 ms          |
//! | retained-model scorer  | ~245,000 ms | (fail-open path only)                             |
//!
//! At the 250k rung (393 segments, 93,702 physical entries) the durable
//! scorer went ~112 ms -> ~23 ms with the same change. Eligibility then
//! stopped copying: posting scans visit keys in place and bulk-build the id
//! set from sorted input, and the manifest decodes into a sorted bulk
//! build. At 250k: integrated 63 -> 39 ms, Eq filter 74 -> 33 ms, the
//! ladder's `price < 500` range + `category` facet (125,000 eligible)
//! 121 -> 46 ms. Fuzzy (distance 1) expansion over the durable dictionary
//! then stopped materializing entries: 207 -> ~60 ms at 250k. The "borrowed
//! leaves" column: segment planning reads only leaf boundary keys, posting
//! scans borrow from the buffer-pool frame, and the merge ranks arena
//! offsets — the allocator (malloc/free/memcmp on per-entry `Vec<u8>`) was
//! ~70% of scorer samples before it.
//!
//! The manifest stage isolates the collection manifest: at 250k the legacy
//! `HYPSMAN1` value was one 4 MB record decoded and cloned on every
//! `MatchAll` query and rewritten on every ingest batch; the chunked
//! `HYPSMAN2` layout keeps the per-batch rewrite at the header plus the
//! touched 16 KB chunks. Record the stage's `materialize_us` (the cost a
//! full allowlist still pays) and `contains_us` (one chunk decode) per rung.
//!
//! Stage trace at the HYPOST02 rung: plan terms ~14 ms, scan 166 segments
//! ~19 ms, finalize ~32 ms. The document-length side lookup that
//! dominated the baseline is gone: self-describing postings carry it,
//! and legacy roots resolve it once per query through a lazily built
//! header map that dense queries pay only when they actually meet a
//! HYPOST01 posting. The finalize stage merges contributions with one
//! sort-and-fold plus a top-k partition instead of a `BTreeMap`
//! accumulation and full sort. Remaining cost splits between term
//! planning and segment decoding.

use std::time::Instant;

use hyphae_native_product::{
    NativeProduct, ObjectId, ProductDocValue, ProductLexicalBranch, ProductSearchFilter,
    ProductSearchRequest,
};

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = std::env::args()
        .nth(1)
        .ok_or("usage: scale_stage_diagnostic <data-dir>")?;
    let open_started = Instant::now();
    let product = NativeProduct::open(std::path::Path::new(&data_dir))?;
    println!(
        "stage=open ms={:.1}",
        open_started.elapsed().as_secs_f64() * 1_000.0,
    );
    let collection = ObjectId::new(52)?;
    let binding = product.resolve_search_collection_binding(collection, 1_000_000)?;

    // Stage 0: the collection manifest on its own — header decode, complete
    // materialization (what every ANN allowlist and `Not`/`IsNull` filter
    // pays), and one membership probe (what a chunk-aware path pays per hit).
    let snapshot = product.snapshot_bounded(1_000_001)?;
    for round in 0..3 {
        let diagnostics = NativeProduct::manifest_diagnostics(&snapshot, collection)?;
        println!(
            "stage=manifest round={round} format={} total={} chunks={} header_bytes={} largest_chunk_bytes={} header_us={} materialize_us={} contains_us={} probe_present={}",
            if diagnostics.legacy {
                "HYPSMAN1"
            } else {
                "HYPSMAN2"
            },
            diagnostics.total,
            diagnostics.chunk_count,
            diagnostics.header_bytes,
            diagnostics.largest_chunk_bytes,
            diagnostics.header_decode_micros,
            diagnostics.materialize_micros,
            diagnostics.contains_micros,
            diagnostics.probe_present,
        );
    }

    // Stage 1: snapshot + raw durable posting scorer (bypasses the product
    // pipeline entirely).
    let scorer_rounds: u32 = std::env::var("HYPHAE_DIAG_SCORER_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    let mut durable_hits = Vec::new();
    for round in 0..scorer_rounds {
        let begun = Instant::now();
        let receipt = product.match_text_receipt_for_diagnostics(
            &snapshot,
            binding.lexical_index,
            "database engine",
            1_000,
        )?;
        if round == 0 {
            durable_hits.clone_from(&receipt.hits);
        }
        println!(
            "stage=durable_scorer round={round} hits={} ms={:.1} terms={} segments={} physical_entries={} workers={} batches={}",
            receipt.hits.len(),
            begun.elapsed().as_secs_f64() * 1_000.0,
            receipt.planned_terms,
            receipt.planned_segments,
            receipt.planned_physical_entries,
            receipt.planned_workers,
            receipt.worker_batches,
        );
    }

    // Stage 2: the retained-model scorer for comparison. Skipped under
    // HYPHAE_DIAG_SKIP_MODEL=1 (it costs minutes at the 100k rung and
    // drowns profiler samples).
    // When it runs, the model's ranked hits are the oracle: the durable
    // scorer must reproduce them exactly (ids, order, scores) or the run
    // fails, so a rung receipt doubles as scorer equivalence evidence.
    let skip_model = std::env::var("HYPHAE_DIAG_SKIP_MODEL").is_ok();
    for round in 0..3 {
        if skip_model {
            break;
        }
        let begun = Instant::now();
        let hits = snapshot.match_text_hits_for_diagnostics(
            binding.lexical_index,
            "database engine",
            1_000,
        )?;
        println!(
            "stage=model_scorer round={round} hits={} ms={:.1}",
            hits.len(),
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
        if round == 0 {
            if hits.len() != durable_hits.len() {
                return Err(format!(
                    "scorer divergence: model {} hits, durable {} hits",
                    hits.len(),
                    durable_hits.len()
                )
                .into());
            }
            for (position, (model, durable)) in hits.iter().zip(&durable_hits).enumerate() {
                if model.document_id != durable.document_id
                    || model.score.to_bits() != durable.score.to_bits()
                {
                    return Err(format!(
                        "scorer divergence at rank {position}: model ({:?}, {}) durable ({:?}, {})",
                        model.document_id, model.score, durable.document_id, durable.score
                    )
                    .into());
                }
            }
            println!(
                "stage=scorer_equivalence hits={} bit_identical=true",
                hits.len()
            );
        }
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

    // Stage 3b: a sparse query (rare trailing term) exercises the
    // per-posting descent path that dense queries no longer take.
    let sparse = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "99999 88888".to_owned(),
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
        let result = product.search_collection(collection, &sparse, 1_000_020 + round)?;
        println!(
            "stage=integrated_sparse round={round} hits={} ms={:.1}",
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

    // Stage 5: the ladder's range filter plus term facet — the most
    // superlinear scenario in `collection_scale_evidence`.
    let range_facet = ProductSearchRequest {
        filter: ProductSearchFilter::Compare {
            field: "price".to_owned(),
            operator: hyphae_native_product::ProductSearchOperator::Less,
            value: ProductDocValue::Integer(500),
        },
        facets: vec![hyphae_native_product::ProductFacetRequest {
            field: "category".to_owned(),
            limit: 8,
        }],
        ..filtered
    };
    let range_rounds: u32 = std::env::var("HYPHAE_DIAG_RANGE_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    for round in 0..range_rounds {
        let begun = Instant::now();
        let result =
            product.search_collection(collection, &range_facet, 1_000_020 + i64::from(round))?;
        println!(
            "stage=integrated_range_facet round={round} hits={} eligible={} ms={:.1}",
            result.hits.len(),
            result.eligible_documents,
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }

    // Stage 6: fuzzy expansion (distance 1) over the durable dictionary
    // in front of the same scorer.
    let fuzzy = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "datbase".to_owned(),
            candidate_limit: 1_000,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: Some(1),
            phrase: false,
        }),
        filter: ProductSearchFilter::MatchAll,
        facets: Vec::new(),
        ..range_facet
    };
    let fuzzy_rounds: u32 = std::env::var("HYPHAE_DIAG_FUZZY_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    for round in 0..fuzzy_rounds {
        let begun = Instant::now();
        let result = product.search_collection(collection, &fuzzy, 1_000_030 + i64::from(round))?;
        println!(
            "stage=integrated_fuzzy round={round} hits={} ms={:.1}",
            result.hits.len(),
            begun.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    Ok(())
}
