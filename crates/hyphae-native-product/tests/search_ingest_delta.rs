// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used)]

//! Point-resolved product batch ingest: no complete-state materialization
//! and durable equivalence with the materialized path.
//!
//! This binary holds one test so the process-wide materialization counter it
//! asserts on cannot be inflated by a concurrently running neighbour.

use std::{collections::BTreeMap, fs, path::PathBuf};

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogName,
    CatalogObjectV2, DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle,
    LexicalIndexPolicy, LogicalCatalogObject, NamedVectorDefinition, ObjectHeaderV2, QualifiedName,
    SearchCollectionDefinitionV2, SearchFieldDefinitionV2, SearchFieldOptions, VectorMetric,
    VectorSearchPolicy,
};
use hyphae_native_product::{
    AnnConsolidationRequest, NativeProduct, ProductDocValue, ProductDocument, ProductDurability,
    ProductLexicalBranch, ProductSearchCollectionBinding, ProductSearchFilter,
    ProductSearchIngestBatch, ProductSearchOperator, ProductSearchRequest, ProductVector,
    ProductVectorBranch, ProductVectorExecution,
};
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::{
    EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId, VectorElement, VectorType,
};

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-search-ingest-delta-{name}-{}",
        std::process::id()
    ))
}

fn name(value: &str) -> Result<CatalogName, Box<dyn std::error::Error>> {
    Ok(CatalogName::unquoted(value)?)
}

fn header(
    id: u128,
    owner: EngineKind,
    object: &str,
    parent: Option<u128>,
) -> Result<ObjectHeaderV2, Box<dyn std::error::Error>> {
    Ok(ObjectHeaderV2 {
        id: ObjectId::new(id)?,
        owner,
        name: QualifiedName::new(name("main")?, name("public")?, name(object)?),
        parent: parent.map(ObjectId::new).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn doc_value_field(
    id: u32,
    field: &str,
    logical_type: LogicalType,
) -> Result<SearchFieldDefinitionV2, Box<dyn std::error::Error>> {
    Ok(SearchFieldDefinitionV2 {
        id: FieldId::new(id)?,
        name: name(field)?,
        logical_type,
        analyzer: None,
        options: SearchFieldOptions {
            stored: true,
            doc_values: true,
            source: FieldSourcePolicy::Retained,
            lexical: LexicalIndexPolicy::None,
        },
    })
}

/// A lexical collection with doc values and one named ANN vector.
fn configure_chunked(
    path: &PathBuf,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    let _ = fs::remove_dir_all(path);
    let mut product = NativeProduct::create(path)?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Database(header(
            10,
            EngineKind::Kernel,
            "database",
            None,
        )?)),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(
            11,
            EngineKind::Kernel,
            "schema",
            Some(10),
        )?)),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: header(12, EngineKind::Search, "canonical", Some(11))?,
            tokenizer: AnalyzerTokenizer::UnicodeWord,
            filters: vec![AnalyzerFilter::Lowercase],
        })),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                bm25: None,
                header: header(13, EngineKind::Search, "chunks", Some(11))?,
                fields: vec![
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(1)?,
                        name: name("body")?,
                        logical_type: LogicalType::Text,
                        analyzer: Some(ObjectId::new(12)?),
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: false,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::Frequencies,
                        },
                    },
                    doc_value_field(2, "parent", LogicalType::Binary)?,
                    doc_value_field(3, "chunk_id", LogicalType::Binary)?,
                    doc_value_field(4, "byte_start", LogicalType::Signed(IntegerWidth::Bits64))?,
                    doc_value_field(5, "byte_end", LogicalType::Signed(IntegerWidth::Bits64))?,
                    doc_value_field(
                        6,
                        "chunk_ordinal",
                        LogicalType::Signed(IntegerWidth::Bits64),
                    )?,
                ],
                vectors: vec![
                    NamedVectorDefinition {
                        id: FieldId::new(7)?,
                        name: name("embedding")?,
                        vector_type: VectorType::new(VectorElement::Float32, 2)?,
                        metric: VectorMetric::SquaredL2,
                        policy: VectorSearchPolicy::Ann(AnnIndexDefinition::new(
                            VectorMetric::SquaredL2,
                            8,
                            32,
                            16,
                            256,
                            7,
                        )?),
                        lifecycle: IncrementalVectorLifecycle {
                            delta_max_entries: 1_000,
                            consolidate_after_deltas: 2,
                            retain_generations: 1,
                        },
                    },
                    NamedVectorDefinition {
                        id: FieldId::new(8)?,
                        name: name("exact")?,
                        vector_type: VectorType::new(VectorElement::Float32, 2)?,
                        metric: VectorMetric::SquaredL2,
                        policy: VectorSearchPolicy::Exact,
                        lifecycle: IncrementalVectorLifecycle {
                            delta_max_entries: 1_000,
                            consolidate_after_deltas: 2,
                            retain_generations: 1,
                        },
                    },
                ],
            },
        )),
        ProductDurability::Strict,
    )?;
    let collection = ObjectId::new(13)?;
    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let binding = product.resolve_search_collection_binding(collection, 0)?;
    Ok((product, binding))
}

fn chunk_document(id: u128, text: &str) -> Result<ProductDocument, Box<dyn std::error::Error>> {
    let coordinate = match id {
        301 => 1.0,
        302 => 2.0,
        303 => 3.0,
        304 => 4.0,
        _ => return Err("unexpected fixture identity".into()),
    };
    Ok(ProductDocument {
        object_id: ObjectId::new(id)?,
        text: text.into(),
        doc_values: BTreeMap::from([
            ("parent".into(), ProductDocValue::Bytes(vec![1, 2, 3])),
            (
                "chunk_id".into(),
                ProductDocValue::Bytes(id.to_be_bytes().to_vec()),
            ),
            ("byte_start".into(), ProductDocValue::Integer(0)),
            ("byte_end".into(), ProductDocValue::Integer(8)),
            (
                "chunk_ordinal".into(),
                ProductDocValue::Integer(i64::try_from(id)?),
            ),
        ]),
        vectors: BTreeMap::from([
            ("embedding".into(), ProductVector::new([coordinate, 0.0])?),
            ("exact".into(), ProductVector::new([coordinate, 0.0])?),
        ]),
    })
}

fn match_all(limit: usize) -> ProductSearchRequest {
    ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    }
}

fn full_state_loads(product: &mut NativeProduct) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 0,
        })?
        .physical
        .process_full_state_loads)
}

/// A vector-bearing batch must stage through the physical delta path: no
/// complete all-engine state load at `BEGIN`, staging, commit, or receipt,
/// while every durable side record (documents, postings, manifest,
/// idempotency marker) lands exactly as the materialized path writes it.
#[test]
#[allow(clippy::too_many_lines)]
fn vector_ingest_is_point_resolved_and_semantically_identical()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("delta-ingest");
    let (mut product, binding) = configure_chunked(&path)
        .map_err(|error| format!("vector collection configuration failed: {error:?}"))?;
    let first = ProductSearchIngestBatch {
        idempotency_id: 7,
        documents: vec![
            chunk_document(301, "rust database engine")?,
            chunk_document(302, "rust field guide")?,
        ],
    };
    // The first batch turns posting coverage on; the second exercises the
    // steady state where coverage is already durable. Consolidation places
    // the first vectors in the immutable base so the measured ingest proves
    // base-plus-delta behavior rather than an empty-base special case.
    product
        .ingest_search_batch(binding.collection, &first, 3, ProductDurability::Strict)
        .map_err(|error| format!("first vector ingest failed: {error:?}"))?;
    let vector_index = binding
        .vectors
        .iter()
        .find(|binding| binding.name == "embedding")
        .ok_or("missing vector binding")?
        .index;
    product
        .administration()
        .consolidate_ann(
            AnnConsolidationRequest::new(vector_index, 16, 16, ProductDurability::Strict)
                .ok_or("invalid consolidation request")?,
        )
        .map_err(|error| format!("ANN consolidation failed: {error:?}"))?;
    let second = ProductSearchIngestBatch {
        idempotency_id: 8,
        documents: vec![
            chunk_document(303, "database hardware")?,
            chunk_document(304, "garden tools")?,
        ],
    };
    let before = full_state_loads(&mut product)?;
    let ann_restores = NativeDatabase::process_ann_index_restore_count();
    let receipt = product
        .ingest_search_batch(binding.collection, &second, 4, ProductDurability::Strict)
        .map_err(|error| format!("second vector ingest failed: {error:?}"))?;
    assert_eq!(
        full_state_loads(&mut product)?,
        before,
        "vector ingest materialized the complete all-engine state"
    );
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        ann_restores,
        "vector ingest restored the immutable ANN base"
    );
    assert!(!receipt.idempotent_replay);
    assert_eq!(receipt.documents, 2);
    let commit = receipt.commit.ok_or("missing commit receipt")?;
    let snapshot = product.snapshot_bounded(4)?;
    assert_eq!(
        receipt.snapshot.root_digest,
        snapshot.identity().root_digest
    );
    assert_eq!(
        receipt.snapshot.visible_csn,
        snapshot.identity().visible_csn
    );
    assert_eq!(receipt.snapshot.logical_time_micros, 4);

    // Idempotent replay resolves through the durable marker and returns the
    // original commit without materializing state.
    let before = full_state_loads(&mut product)?;
    let replay =
        product.ingest_search_batch(binding.collection, &second, 5, ProductDurability::Strict)?;
    assert_eq!(full_state_loads(&mut product)?, before);
    assert!(replay.idempotent_replay);
    assert_eq!(replay.documents, 2);
    assert_eq!(replay.commit, Some(commit));
    let mut conflict = second.clone();
    conflict.documents[0].text = "different".into();
    assert_eq!(
        product
            .ingest_search_batch(binding.collection, &conflict, 5, ProductDurability::Strict)
            .expect_err("payload conflict admitted")
            .code(),
        hyphae_native_product::ProductErrorCode::IdempotencyConflict
    );
    // Duplicate identity across batches fails closed before any mutation.
    let duplicate = ProductSearchIngestBatch {
        idempotency_id: 9,
        documents: vec![chunk_document(303, "again")?],
    };
    assert_eq!(
        product
            .ingest_search_batch(binding.collection, &duplicate, 6, ProductDurability::Strict)
            .expect_err("duplicate identity admitted")
            .code(),
        hyphae_native_product::ProductErrorCode::CatalogConflict
    );
    assert!(
        product.snapshot_bounded(6)?.identity().root_digest == snapshot.identity().root_digest,
        "rejected batch published a root"
    );

    // Complete corpus, lexical scoring, and doc-value postings observe every
    // document from both paths identically.
    let page = NativeProduct::search_documents_at_snapshot(
        &product.snapshot_bounded(6)?,
        binding.collection,
        None,
        16,
    )?;
    assert_eq!(
        page.documents,
        [first.documents.clone(), second.documents.clone()].concat()
    );
    let result = product.search_collection(binding.collection, &match_all(16), 6)?;
    assert_eq!(result.total_documents, 4);
    let mut lexical = match_all(16);
    lexical.lexical = Some(ProductLexicalBranch {
        query: "database".into(),
        candidate_limit: 16,
        weight: 1,
        operator: None,
        prefix: false,
        fields: Vec::new(),
        fuzzy: None,
        phrase: false,
    });
    let hits = product.search_collection(binding.collection, &lexical, 6)?;
    let mut ids: Vec<u128> = hits.hits.iter().map(|hit| hit.object_id.get()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![301, 303]);
    let mut filtered = match_all(16);
    filtered.filter = ProductSearchFilter::Compare {
        field: "chunk_ordinal".into(),
        operator: ProductSearchOperator::Equal,
        value: ProductDocValue::Integer(304),
    };
    let filtered = product.search_collection(binding.collection, &filtered, 6)?;
    assert_eq!(filtered.hits.len(), 1);
    assert_eq!(filtered.hits[0].object_id.get(), 304);

    let mut exact_request = match_all(16);
    exact_request.vectors.push(ProductVectorBranch {
        target: "exact".into(),
        query: ProductVector::new([3.0, 0.0])?,
        candidate_limit: 4,
        weight: 1,
        execution: Some(ProductVectorExecution::Exact),
        max_distance: None,
    });
    let exact = product.search_collection(binding.collection, &exact_request, 6)?;
    assert_eq!(exact.hits[0].object_id.get(), 303);
    assert!(!exact.approximate);

    let mut ann = match_all(16);
    ann.vectors.push(ProductVectorBranch {
        target: "embedding".into(),
        query: ProductVector::new([3.0, 0.0])?,
        candidate_limit: 4,
        weight: 1,
        execution: Some(ProductVectorExecution::Ann {
            ef_search: 16,
            exact_rerank: Some(4),
        }),
        max_distance: None,
    });
    let ann = product.search_collection(binding.collection, &ann, 6)?;
    assert_eq!(ann.hits[0].object_id.get(), 303);
    assert_eq!(ann.hits.len(), 4);

    // Everything survives reopen: the delta path wrote the same durable
    // records the materialized path writes, manifest header and chunks
    // included.
    let manifest_records = product.manifest_records_for_test(binding.collection, 6)?;
    assert_eq!(manifest_records.len(), 2, "one header and one chunk");
    drop(product);
    let mut reopened = NativeProduct::open(&path)?;
    assert_eq!(
        reopened.manifest_records_for_test(binding.collection, 6)?,
        manifest_records
    );
    let result = reopened.search_collection(binding.collection, &match_all(16), 6)?;
    assert_eq!(result.total_documents, 4);
    let reopened_exact = reopened.search_collection(binding.collection, &exact_request, 6)?;
    assert_eq!(reopened_exact.hits, exact.hits);
    let replay =
        reopened.ingest_search_batch(binding.collection, &second, 7, ProductDurability::Strict)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.commit, Some(commit));
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}
