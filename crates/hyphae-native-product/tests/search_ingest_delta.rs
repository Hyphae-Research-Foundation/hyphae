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
use hyphae_native_manifest::RootManifestStore;
use hyphae_native_product::{
    AnnConsolidationRequest, NativeProduct, ProductDocValue, ProductDocument, ProductDurability,
    ProductLexicalBranch, ProductSearchCollectionBinding, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchOperator, ProductSearchRequest, ProductVector, ProductVectorBranch,
    ProductVectorExecution,
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

fn materialization_loads(
    product: &mut NativeProduct,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let physical = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 0,
        })?
        .physical;
    Ok((
        physical.process_full_state_loads,
        physical.process_full_catalog_loads,
    ))
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
    let mut omitted_exact = chunk_document(302, "rust field guide")?;
    omitted_exact.vectors.remove("exact");
    let first = ProductSearchIngestBatch {
        idempotency_id: 7,
        documents: vec![chunk_document(301, "rust database engine")?, omitted_exact],
    };
    // The first batch turns posting coverage on and selects M05; the second
    // exercises the steady-state overlay path where coverage is durable.
    let materializations = materialization_loads(&mut product)?;
    let ann_restores = NativeDatabase::process_ann_index_restore_count();
    let first_receipt = product
        .ingest_search_batch(binding.collection, &first, 3, ProductDurability::Strict)
        .map_err(|error| format!("first vector ingest failed: {error:?}"))?;
    assert_eq!(materialization_loads(&mut product)?, materializations);
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        ann_restores,
        "complete-image ingest restored an immutable ANN base"
    );
    let first_replay =
        product.ingest_search_batch(binding.collection, &first, 3, ProductDurability::Strict)?;
    assert!(first_replay.idempotent_replay);
    assert_eq!(first_replay.commit, first_receipt.commit);
    let vector_index = binding
        .vectors
        .iter()
        .find(|binding| binding.name == "embedding")
        .ok_or("missing vector binding")?
        .index;

    // A plain delete of an absent vector remains a mutation-free rollback.
    // A complete-image fence of the same absence commits conflict authority
    // without replacing the ANN root or appending any page.
    drop(product);
    let absent = ObjectId::new(999)?;
    let mut runtime = NativeDatabase::open(&path)?;
    let selected_before = runtime.observe_ann_index(vector_index)?;
    let before = runtime.physical_observation()?;
    let mut ordinary = runtime.begin(3, hyphae_native_types::DurabilityClass::Strict)?;
    assert!(!ordinary.delete_vector(vector_index, absent)?);
    ordinary.rollback();
    assert_eq!(
        runtime.physical_observation()?.page_count,
        before.page_count
    );
    runtime.checkpoint()?;
    let manifest_before = RootManifestStore::open(&path)?
        .current()
        .cloned()
        .ok_or("missing root manifest before absence fence")?;
    let roots_before = manifest_before
        .to_root_set()?
        .iter_roots()
        .collect::<Vec<_>>();
    let search_root_before = roots_before
        .iter()
        .find(|(slot, _)| slot.engine == EngineKind::Search)
        .map(|(_, page)| *page)
        .ok_or("missing search root before absence fence")?;
    let before_fence = runtime.physical_observation()?;
    let restores = NativeDatabase::process_ann_index_restore_count();
    let mut fence =
        runtime.begin_optimistic_delta(3, hyphae_native_types::DurabilityClass::Strict)?;
    assert!(!runtime.stage_delta_vector_absence_fence(&mut fence, vector_index, absent,)?);
    let fence_commit = runtime.commit_optimistic(fence)?;
    let after = runtime.physical_observation()?;
    assert_eq!(after.page_count, before_fence.page_count);
    assert!(after.wal_bytes > before_fence.wal_bytes);
    assert_eq!(
        after.process_full_state_loads,
        before_fence.process_full_state_loads
    );
    assert_eq!(
        after.process_full_catalog_loads,
        before_fence.process_full_catalog_loads
    );
    assert_eq!(NativeDatabase::process_ann_index_restore_count(), restores);
    assert_eq!(runtime.observe_ann_index(vector_index)?, selected_before);
    runtime.checkpoint()?;
    let manifest_after = RootManifestStore::open(&path)?
        .current()
        .cloned()
        .ok_or("missing root manifest after absence fence")?;
    let roots_after = manifest_after
        .to_root_set()?
        .iter_roots()
        .collect::<Vec<_>>();
    let search_root_after = roots_after
        .iter()
        .find(|(slot, _)| slot.engine == EngineKind::Search)
        .map(|(_, page)| *page)
        .ok_or("missing search root after absence fence")?;
    assert_eq!(roots_after, roots_before);
    assert_eq!(search_root_after, search_root_before);
    assert_eq!(manifest_after.visible_csn(), fence_commit.commit_csn);
    assert!(manifest_after.visible_csn() > manifest_before.visible_csn());
    assert_eq!(manifest_after.wal_anchor().lsn, fence_commit.commit_lsn);
    assert!(manifest_after.wal_anchor().lsn > manifest_before.wal_anchor().lsn);
    assert_ne!(
        manifest_after.wal_anchor().digest,
        manifest_before.wal_anchor().digest
    );
    drop(runtime);
    let mut product = NativeProduct::open(&path)?;

    let documents_before_consolidation = NativeProduct::search_documents_at_snapshot(
        &product.snapshot_bounded(3)?,
        binding.collection,
        None,
        16,
    )?
    .documents;
    let mut ann_before_request = match_all(16);
    ann_before_request.vectors.push(ProductVectorBranch {
        target: "embedding".into(),
        query: ProductVector::new([1.0, 0.0])?,
        candidate_limit: 16,
        weight: 1,
        execution: Some(ProductVectorExecution::Ann {
            ef_search: 16,
            exact_rerank: Some(16),
        }),
        max_distance: None,
    });
    let mut exact_before_request = match_all(16);
    exact_before_request.vectors.push(ProductVectorBranch {
        target: "exact".into(),
        query: ProductVector::new([1.0, 0.0])?,
        candidate_limit: 16,
        weight: 1,
        execution: Some(ProductVectorExecution::Exact),
        max_distance: None,
    });
    let ann_before_consolidation =
        product.search_collection(binding.collection, &ann_before_request, 3)?;
    let exact_before_consolidation =
        product.search_collection(binding.collection, &exact_before_request, 3)?;
    let consolidation = product
        .administration()
        .consolidate_ann(
            AnnConsolidationRequest::new(vector_index, 16, 16, ProductDurability::Strict)
                .ok_or("invalid consolidation request")?,
        )
        .map_err(|error| format!("ANN consolidation failed: {error:?}"))?;
    assert!(consolidation.consumed_delta_records >= 2);
    assert_eq!(consolidation.effective_vector_count, 2);
    assert_ne!(
        consolidation.replacement_base_identity,
        consolidation.previous_base_identity
    );
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(3)?,
            binding.collection,
            None,
            16,
        )?
        .documents,
        documents_before_consolidation
    );
    assert_eq!(
        product
            .search_collection(binding.collection, &ann_before_request, 3)?
            .hits,
        ann_before_consolidation.hits
    );
    assert_eq!(
        product
            .search_collection(binding.collection, &exact_before_request, 3)?
            .hits,
        exact_before_consolidation.hits
    );
    let second = ProductSearchIngestBatch {
        idempotency_id: 8,
        documents: vec![
            chunk_document(303, "database hardware")?,
            chunk_document(304, "garden tools")?,
        ],
    };
    let before = materialization_loads(&mut product)?;
    let ann_restores = NativeDatabase::process_ann_index_restore_count();
    let receipt = product
        .ingest_search_batch(binding.collection, &second, 4, ProductDurability::Strict)
        .map_err(|error| format!("second vector ingest failed: {error:?}"))?;
    assert_eq!(
        materialization_loads(&mut product)?,
        before,
        "vector ingest materialized complete all-engine or catalog state"
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
    let before = materialization_loads(&mut product)?;
    let replay =
        product.ingest_search_batch(binding.collection, &second, 5, ProductDurability::Strict)?;
    assert_eq!(materialization_loads(&mut product)?, before);
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
    assert!(exact.hits.iter().all(|hit| hit.object_id.get() != 302));
    assert!(!exact.approximate);

    let mut ann_request = match_all(16);
    ann_request.vectors.push(ProductVectorBranch {
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
    let ann = product.search_collection(binding.collection, &ann_request, 6)?;
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
    let mut replacement = chunk_document(303, "database hardware updated")?;
    replacement.vectors.remove("embedding");
    let update = ProductSearchDocumentUpdate {
        idempotency_id: 10,
        document: replacement,
    };
    let updated = reopened.update_search_document(
        binding.collection,
        &update,
        8,
        ProductDurability::Strict,
    )?;
    let update_replay = reopened.update_search_document(
        binding.collection,
        &update,
        8,
        ProductDurability::Strict,
    )?;
    assert!(update_replay.idempotent_replay);
    assert_eq!(update_replay.commit, updated.commit);
    let exact_after_omission = reopened.search_collection(binding.collection, &exact_request, 8)?;
    assert_eq!(exact_after_omission.hits[0].object_id.get(), 303);
    let ann_after_omission = reopened.search_collection(binding.collection, &ann_request, 8)?;
    assert!(
        ann_after_omission
            .hits
            .iter()
            .all(|hit| hit.object_id.get() != 303)
    );
    let delete = ProductSearchDocumentDelete {
        idempotency_id: 11,
        object_id: ObjectId::new(303)?,
    };
    let deleted = reopened.delete_search_document(
        binding.collection,
        delete,
        9,
        ProductDurability::Strict,
    )?;
    let delete_replay = reopened.delete_search_document(
        binding.collection,
        delete,
        9,
        ProductDurability::Strict,
    )?;
    assert!(delete_replay.idempotent_replay);
    assert_eq!(delete_replay.commit, deleted.commit);
    assert!(
        reopened
            .search_collection(binding.collection, &exact_request, 9)?
            .hits
            .iter()
            .all(|hit| hit.object_id.get() != 303)
    );
    let mut hybrid_after_delete = ann_request.clone();
    hybrid_after_delete.lexical = Some(ProductLexicalBranch {
        query: "database".into(),
        candidate_limit: 16,
        weight: 1,
        operator: None,
        prefix: false,
        fields: Vec::new(),
        fuzzy: None,
        phrase: false,
    });
    assert!(
        reopened
            .search_collection(binding.collection, &hybrid_after_delete, 9)?
            .hits
            .iter()
            .all(|hit| hit.object_id.get() != 303)
    );
    let documents = NativeProduct::search_documents_at_snapshot(
        &reopened.snapshot_bounded(9)?,
        binding.collection,
        None,
        16,
    )?;
    assert!(
        documents
            .documents
            .iter()
            .all(|document| document.object_id.get() != 303)
    );
    drop(reopened);
    let reopened = NativeProduct::open(&path)?;
    assert!(
        reopened
            .search_collection(binding.collection, &ann_request, 9)?
            .hits
            .iter()
            .all(|hit| hit.object_id.get() != 303)
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}
