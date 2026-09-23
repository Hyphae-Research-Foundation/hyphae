// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used)]

//! Integrated product search persistence, strategy, and ingestion acceptance tests.

use std::{collections::BTreeMap, fs, path::PathBuf};

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, Bm25Parameters,
    CatalogName, CatalogObjectV2, DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle,
    LexicalIndexPolicy, LogicalCatalogObject, NamedVectorDefinition, ObjectHeaderV2, QualifiedName,
    SearchCollectionDefinitionV2, SearchFieldDefinitionV2, SearchFieldOptions, VectorMetric,
    VectorSearchPolicy,
};
use hyphae_native_product::proof::{
    NativeProof, NativeProofGenerationLimits, NativeProofKind, NativeVerificationLimits,
    ProofCodecLimits, encode_native_proof, generate_native_operation_proof,
    verify_native_proof_offline,
};
use hyphae_native_product::{
    AnnConsolidationRequest, MAX_PRODUCT_SEARCH_BATCH_BYTES, MAX_PRODUCT_SEARCH_VECTOR_TARGETS,
    NativeProduct, ProductAggregation, ProductAggregationValue, ProductAuthorization,
    ProductDocValue, ProductDocument, ProductDurability, ProductError, ProductErrorCategory,
    ProductErrorCode, ProductExplicitTransactionStatus, ProductFacetRequest, ProductHighlight,
    ProductLexicalBranch, ProductMissingPlacement, ProductNamedAggregation, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductResponse, ProductRetry,
    ProductSearchCollectionBinding, ProductSearchDocumentDelete, ProductSearchDocumentUpdate,
    ProductSearchFilter, ProductSearchIngestBatch, ProductSearchIngestionCoordinator,
    ProductSearchOperator, ProductSearchRequest, ProductSearchSort, ProductSession,
    ProductSessionId, ProductSortDirection, ProductSortSource, ProductStreamEnqueueOutcome,
    ProductTransactionHandle, ProductTransactionSearchMutation, ProductTransactionStageResult,
    ProductTransactionVectorMutation, ProductVector, ProductVectorBranch, ProductVectorExecution,
    ProductVectorStrategy, commit_explicit_transaction_with_interruption_for_test,
};
use hyphae_native_runtime::{AnnSearchOptions, AnnSearchStrategy, CommitBoundary, NativeDatabase};
use hyphae_native_types::{
    DurabilityClass, EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId, VectorElement,
    VectorType,
};

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-integrated-search-{name}-{}",
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

fn configure(
    path: &PathBuf,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    configure_full(path, None, vec![AnalyzerFilter::Lowercase], 2)
}

fn configure_with_bm25(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    configure_full(path, bm25, vec![AnalyzerFilter::Lowercase], 2)
}

fn configure_full(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
    analyzer_filters: Vec<AnalyzerFilter>,
    vector_target_count: usize,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    configure_full_with_delta_max(path, bm25, analyzer_filters, vector_target_count, 1_000)
}

#[allow(clippy::too_many_lines)]
fn configure_full_with_delta_max(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
    analyzer_filters: Vec<AnalyzerFilter>,
    vector_target_count: usize,
    delta_max_entries: u32,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    let mut product = configure_catalog_full(
        path,
        bm25,
        analyzer_filters,
        vector_target_count,
        delta_max_entries,
    )?;
    let collection = ObjectId::new(13)?;
    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let binding = product.resolve_search_collection_binding(collection, 0)?;
    Ok((product, binding))
}

fn configure_catalog(path: &PathBuf) -> Result<NativeProduct, Box<dyn std::error::Error>> {
    configure_catalog_full(path, None, vec![AnalyzerFilter::Lowercase], 2, 1_000)
}

#[allow(clippy::too_many_lines)]
fn configure_catalog_full(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
    analyzer_filters: Vec<AnalyzerFilter>,
    vector_target_count: usize,
    delta_max_entries: u32,
) -> Result<NativeProduct, Box<dyn std::error::Error>> {
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
            filters: analyzer_filters,
        })),
        ProductDurability::Strict,
    )?;
    let ann = AnnIndexDefinition::new(VectorMetric::SquaredL2, 8, 32, 16, 256, 7)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries,
        consolidate_after_deltas: u16::try_from(delta_max_entries.min(4))?,
        retain_generations: 2,
    };
    let mut vectors = vec![
        NamedVectorDefinition {
            id: FieldId::new(4)?,
            name: name("image")?,
            vector_type: VectorType::new(VectorElement::Float32, 2)?,
            metric: VectorMetric::SquaredL2,
            policy: VectorSearchPolicy::Ann(ann),
            lifecycle,
            embedding_profile: None,
        },
        NamedVectorDefinition {
            id: FieldId::new(5)?,
            name: name("semantic")?,
            vector_type: VectorType::new(VectorElement::Float32, 2)?,
            metric: VectorMetric::SquaredL2,
            policy: VectorSearchPolicy::Adaptive {
                exact_candidate_threshold: 2,
                ann,
            },
            lifecycle,
            embedding_profile: None,
        },
    ];
    vectors.truncate(vector_target_count);
    for index in vectors.len()..vector_target_count {
        vectors.push(NamedVectorDefinition {
            id: FieldId::new(u32::try_from(100 + index)?)?,
            name: name(&format!("target{index:02}"))?,
            vector_type: VectorType::new(VectorElement::Float32, 2)?,
            metric: VectorMetric::SquaredL2,
            policy: VectorSearchPolicy::Exact,
            lifecycle,
            embedding_profile: None,
        });
    }
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                bm25,
                header: header(13, EngineKind::Search, "products", Some(11))?,
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
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(9)?,
                        name: name("rating")?,
                        logical_type: LogicalType::Float64,
                        analyzer: None,
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: true,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::None,
                        },
                    },
                ],
                vectors,
            },
        )),
        ProductDurability::Strict,
    )?;
    Ok(product)
}

fn document(
    id: u128,
    text: &str,
    category: &str,
    price: i64,
    image: [f32; 2],
    semantic: [f32; 2],
) -> Result<ProductDocument, Box<dyn std::error::Error>> {
    Ok(ProductDocument {
        object_id: ObjectId::new(id)?,
        text: text.into(),
        doc_values: BTreeMap::from([
            ("category".into(), ProductDocValue::String(category.into())),
            ("price".into(), ProductDocValue::Integer(price)),
        ]),
        vectors: BTreeMap::from([
            ("image".into(), ProductVector::new(image)?),
            ("semantic".into(), ProductVector::new(semantic)?),
        ]),
    })
}

fn seed() -> Result<ProductSearchIngestBatch, Box<dyn std::error::Error>> {
    Ok(ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            document(
                201,
                "rust database engine",
                "book",
                30,
                [0.0, 0.0],
                [0.0, 0.0],
            )?,
            document(202, "rust field guide", "book", 10, [1.0, 0.0], [0.0, 1.0])?,
            document(203, "database hardware", "gear", 20, [2.0, 0.0], [1.0, 0.0])?,
            document(204, "garden tools", "gear", 40, [3.0, 0.0], [1.0, 1.0])?,
        ],
    })
}

fn assert_limit_error(error: &ProductError) {
    assert_eq!(error.code(), ProductErrorCode::LimitExceeded);
    assert_eq!(error.category(), ProductErrorCategory::Limit);
    assert_eq!(error.retry(), ProductRetry::Never);
    assert_eq!(error.message(), "native request exceeds a product limit");
}

#[test]
fn ann_delta_limit_fails_closed_and_retry_succeeds_after_consolidation()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("ann-delta-limit");
    let (mut product, binding) =
        configure_full_with_delta_max(&path, None, vec![AnalyzerFilter::Lowercase], 2, 1)
            .map_err(|error| format!("tiny-delta configuration failed: {error:?}"))?;
    product
        .ingest_search_batch(
            binding.collection,
            &ProductSearchIngestBatch {
                idempotency_id: 40,
                documents: vec![document(
                    901,
                    "accepted delta",
                    "book",
                    1,
                    [1.0, 0.0],
                    [1.0, 0.0],
                )?],
            },
            1,
            ProductDurability::Strict,
        )
        .map_err(|error| format!("initial tiny-delta ingest failed: {error:?}"))?;
    let blocked = ProductSearchIngestBatch {
        idempotency_id: 41,
        documents: vec![document(
            902,
            "blocked delta",
            "book",
            2,
            [2.0, 0.0],
            [2.0, 0.0],
        )?],
    };
    let before = product.snapshot_bounded(2)?.identity();

    for _ in 0..2 {
        let error = product
            .ingest_search_batch(binding.collection, &blocked, 2, ProductDurability::Strict)
            .expect_err("full ANN delta admitted");
        assert_limit_error(&error);
        assert_eq!(product.snapshot_bounded(2)?.identity(), before);
    }

    for vector in &binding.vectors {
        product
            .administration()
            .consolidate_ann(
                AnnConsolidationRequest::new(vector.index, 1, 1, ProductDurability::Strict)
                    .ok_or("invalid consolidation request")?,
            )
            .map_err(|error| format!("{} consolidation failed: {error:?}", vector.name))?;
    }
    let retried = product
        .ingest_search_batch(binding.collection, &blocked, 3, ProductDurability::Strict)
        .map_err(|error| format!("post-consolidation retry failed: {error:?}"))?;
    assert!(!retried.idempotent_replay, "rejection persisted a marker");
    assert_ne!(retried.snapshot.root_digest, before.root_digest);
    let replay =
        product.ingest_search_batch(binding.collection, &blocked, 4, ProductDurability::Strict)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.commit, retried.commit);

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn retained_memory_rejection_keeps_root_snapshot_and_idempotency_unpublished()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("retained-memory-limit");
    let (mut product, binding) = configure(&path)?;
    let mut oversized = document(903, "", "book", 3, [3.0, 0.0], [3.0, 0.0])?;
    oversized.text = "r".repeat(MAX_PRODUCT_SEARCH_BATCH_BYTES - 256);
    let rejected = ProductSearchIngestBatch {
        idempotency_id: 42,
        documents: vec![oversized],
    };
    let mut validation_stream = ProductSearchIngestionCoordinator {
        max_in_flight_bytes: MAX_PRODUCT_SEARCH_BATCH_BYTES,
        max_in_flight_batches: 1,
        max_tracked_idempotency_ids: 1,
    }
    .stream(binding.collection)?;
    assert_eq!(
        validation_stream.enqueue(rejected.clone())?,
        ProductStreamEnqueueOutcome::Enqueued,
        "fixture exceeded product batch validation before runtime admission"
    );
    drop(validation_stream);
    let before = product.snapshot_bounded(1)?.identity();

    for _ in 0..2 {
        let error = product
            .ingest_search_batch(binding.collection, &rejected, 1, ProductDurability::Strict)
            .expect_err("oversized retained delta admitted");
        assert_limit_error(&error);
        assert_eq!(product.snapshot_bounded(1)?.identity(), before);
    }

    let corrected = ProductSearchIngestBatch {
        idempotency_id: rejected.idempotency_id,
        documents: vec![document(
            903,
            "corrected retained delta",
            "book",
            3,
            [3.0, 0.0],
            [3.0, 0.0],
        )?],
    };
    let retried = product.ingest_search_batch(
        binding.collection,
        &corrected,
        2,
        ProductDurability::Strict,
    )?;
    assert!(!retried.idempotent_replay, "rejection persisted a marker");
    assert_ne!(retried.snapshot.root_digest, before.root_digest);
    let replay = product.ingest_search_batch(
        binding.collection,
        &corrected,
        3,
        ProductDurability::Strict,
    )?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.commit, retried.commit);

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn complete_document_scan_is_stable_bounded_and_snapshot_pinned()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("complete-document-scan");
    let (mut product, binding) = configure(&path)?;
    let batch = seed()?;
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;
    let snapshot = product.snapshot_bounded(7)?;

    let first =
        NativeProduct::search_documents_at_snapshot(&snapshot, binding.collection, None, 2)?;
    assert_eq!(first.snapshot, snapshot.identity());
    assert_eq!(first.documents, batch.documents[..2]);
    assert_eq!(first.continuation, Some(batch.documents[1].object_id));

    let second = NativeProduct::search_documents_at_snapshot(
        &snapshot,
        binding.collection,
        first.continuation,
        2,
    )?;
    assert_eq!(second.documents, batch.documents[2..]);
    assert_eq!(second.continuation, None);
    assert!(matches!(
        NativeProduct::search_documents_at_snapshot(
            &snapshot,
            binding.collection,
            None,
            0,
        ),
        Err(error) if error.code() == hyphae_native_product::ProductErrorCode::LimitExceeded
    ));

    drop(product);
    let _ignored = fs::remove_dir_all(path);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn complete_image_first_insert_handles_base_point_and_absent_vectors_across_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    for (state, target) in [
        ("base", "image"),
        ("point", "semantic"),
        ("absent", "image"),
    ] {
        let path = temporary(&format!("complete-image-{state}"));
        let (product, binding) = configure(&path)?;
        let object = ObjectId::new(700)?;
        let vector_index = binding
            .vectors
            .iter()
            .find(|candidate| candidate.name == target)
            .ok_or("missing vector target")?
            .index;
        drop(product);

        let mut runtime = NativeDatabase::open(&path)?;
        match state {
            "base" => {
                let plan = runtime.plan_initial_ann_bulk(
                    vector_index,
                    vec![(object, ProductVector::new([9.0, 0.0])?)],
                    1,
                )?;
                runtime.publish_initial_ann_bulk(plan, DurabilityClass::Strict)?;
            }
            "point" => {
                let mut transaction = runtime.begin(1, DurabilityClass::Strict)?;
                transaction.upsert_vector(vector_index, object, ProductVector::new([9.0, 0.0])?)?;
                transaction.commit()?;
            }
            "absent" => {
                let mut upsert = runtime.begin(1, DurabilityClass::Strict)?;
                upsert.upsert_vector(vector_index, object, ProductVector::new([9.0, 0.0])?)?;
                upsert.commit()?;
                let mut delete = runtime.begin(2, DurabilityClass::Strict)?;
                assert!(delete.delete_vector(vector_index, object)?);
                delete.commit()?;
            }
            _ => return Err("unknown vector state".into()),
        }
        drop(runtime);

        let mut product = NativeProduct::open(&path)?;
        let mut complete = document(700, "complete image", "book", 1, [0.0, 0.0], [0.0, 0.0])?;
        complete.vectors.remove(target);
        let batch = ProductSearchIngestBatch {
            idempotency_id: 90,
            documents: vec![complete.clone()],
        };
        let receipt = product.ingest_search_batch(
            binding.collection,
            &batch,
            2,
            ProductDurability::Strict,
        )?;
        let replay = product.ingest_search_batch(
            binding.collection,
            &batch,
            3,
            ProductDurability::Strict,
        )?;
        assert!(replay.idempotent_replay);
        assert_eq!(replay.commit, receipt.commit);

        let removed_execution = if target == "image" {
            Some(ProductVectorExecution::Ann {
                ef_search: 16,
                exact_rerank: Some(16),
            })
        } else {
            None
        };
        let removed = vector_request(target, [9.0, 0.0], removed_execution, Some(0.0))?;
        let supplied_target = if target == "image" {
            "semantic"
        } else {
            "image"
        };
        let supplied = vector_request(supplied_target, [0.0, 0.0], None, Some(0.0))?;
        let removed_result = product.search_collection(binding.collection, &removed, 3)?;
        assert!(removed_result.hits.is_empty());
        if target == "semantic" {
            assert_eq!(
                removed_result.vector_branches[0].strategy,
                ProductVectorStrategy::AdaptiveExactFiltered
            );
        }
        assert_eq!(
            product
                .search_collection(binding.collection, &supplied, 3)?
                .hits[0]
                .object_id,
            object
        );
        let page = NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(3)?,
            binding.collection,
            None,
            1,
        )?;
        assert_eq!(page.documents, [complete.clone()]);
        let mut hybrid = lexical_request("not-in-document");
        hybrid.vectors = removed.vectors.clone();
        assert!(
            product
                .search_collection(binding.collection, &hybrid, 3)?
                .hits
                .is_empty()
        );
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        assert!(
            reopened
                .search_collection(binding.collection, &removed, 3)?
                .hits
                .is_empty()
        );
        assert_eq!(
            reopened
                .search_collection(binding.collection, &supplied, 3)?
                .hits[0]
                .object_id,
            object
        );
        let page = NativeProduct::search_documents_at_snapshot(
            &reopened.snapshot_bounded(3)?,
            binding.collection,
            None,
            1,
        )?;
        assert_eq!(page.documents, [complete]);
        drop(reopened);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn ann_and_hybrid_match_all_exclude_foreign_vectors_across_lifecycle()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("ann-collection-universe");
    let (mut product, binding) = configure_full(&path, None, vec![AnalyzerFilter::Lowercase], 1)?;
    let image_index = binding
        .vectors
        .iter()
        .find(|vector| vector.name == "image")
        .ok_or("missing image target")?
        .index;
    let mut member = document(
        760,
        "primary collection member",
        "book",
        1,
        [1.0, 0.0],
        [1.0, 0.0],
    )?;
    member.vectors.remove("semantic");
    let mut second = document(
        761,
        "surviving collection member",
        "book",
        2,
        [2.0, 0.0],
        [2.0, 0.0],
    )?;
    second.vectors.remove("semantic");
    let mut third = document(
        762,
        "surviving collection member",
        "book",
        3,
        [3.0, 0.0],
        [3.0, 0.0],
    )?;
    third.vectors.remove("semantic");
    product.ingest_search_batch(
        binding.collection,
        &ProductSearchIngestBatch {
            idempotency_id: 90,
            documents: vec![member.clone(), second.clone(), third.clone()],
        },
        1,
        ProductDurability::Strict,
    )?;
    drop(product);

    let foreign = ObjectId::new(759)?;
    let mut runtime = NativeDatabase::open(&path)?;
    let consolidation = runtime.plan_ann_consolidation(image_index, 3, 3)?;
    runtime.consolidate_ann(consolidation, DurabilityClass::Strict)?;
    let mut raw_insert = runtime.begin(2, DurabilityClass::Strict)?;
    raw_insert.upsert_vector(image_index, foreign, ProductVector::new([0.0, 0.0])?)?;
    raw_insert.commit()?;
    let options = AnnSearchOptions::new(1, 2, None)?;
    let raw = runtime.search_ann_latest(image_index, &ProductVector::new([0.0, 0.0])?, options)?;
    assert_eq!(raw.hits[0].object_id, foreign);
    assert!(!raw.exact_reranked);
    let allowlist =
        std::collections::BTreeSet::from([member.object_id, second.object_id, third.object_id]);
    let scoped = runtime.search_ann_filtered_latest(
        image_index,
        &ProductVector::new([0.0, 0.0])?,
        options,
        &allowlist,
    )?;
    assert_eq!(scoped.hits[0].object_id, member.object_id);
    assert_eq!(
        scoped.strategy,
        AnnSearchStrategy::StableIdEligibilityTraversal
    );
    assert!(!scoped.exact_reranked);
    drop(runtime);

    let mut product = NativeProduct::open(&path)?;
    let mut ann = vector_request(
        "image",
        [0.0, 0.0],
        Some(ProductVectorExecution::Ann {
            ef_search: 2,
            exact_rerank: None,
        }),
        None,
    )?;
    ann.vectors[0].candidate_limit = 1;
    ann.limit = 1;
    let result = product.search_collection(binding.collection, &ann, 2)?;
    assert_eq!(result.total_documents, 3);
    assert_eq!(result.eligible_documents, 3);
    assert_eq!(result.hits[0].object_id, member.object_id);
    assert_eq!(
        result.vector_branches[0].candidate_count,
        scoped.candidate_count
    );
    assert_eq!(
        result.vector_branches[0].visited_nodes,
        scoped.visited_nodes
    );
    assert!(!result.vector_branches[0].exact_reranked);

    let mut hybrid = ann.clone();
    hybrid.lexical = Some(ProductLexicalBranch {
        query: "primary".to_owned(),
        candidate_limit: 1,
        weight: 1,
        operator: None,
        prefix: false,
        fields: Vec::new(),
        fuzzy: None,
        phrase: false,
    });
    let hybrid_result = product.search_collection(binding.collection, &hybrid, 2)?;
    assert_eq!(hybrid_result.hits.len(), 1);
    assert_eq!(hybrid_result.hits[0].object_id, member.object_id);

    let mut zero_distance = ann.clone();
    zero_distance.vectors[0].max_distance = Some(hyphae_native_product::CanonicalF64::new(0.0));
    assert!(
        product
            .search_collection(binding.collection, &zero_distance, 2)?
            .hits
            .is_empty()
    );

    let mut replacement = member.clone();
    replacement.text = "replacement member".to_owned();
    replacement
        .vectors
        .insert("image".to_owned(), ProductVector::new([0.0, 0.0])?);
    product.update_search_document(
        binding.collection,
        &ProductSearchDocumentUpdate {
            idempotency_id: 91,
            document: replacement,
        },
        3,
        ProductDurability::Strict,
    )?;
    let replacement_result = product.search_collection(binding.collection, &zero_distance, 3)?;
    assert_eq!(replacement_result.hits.len(), 1);
    assert_eq!(replacement_result.hits[0].object_id, member.object_id);

    product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 92,
            object_id: member.object_id,
        },
        4,
        ProductDurability::Strict,
    )?;
    for request in [&ann, &hybrid, &zero_distance] {
        let deleted = product.search_collection(binding.collection, request, 4)?;
        assert_eq!(deleted.total_documents, 2);
        assert!(
            deleted
                .hits
                .iter()
                .all(|hit| hit.object_id != foreign && hit.object_id != member.object_id)
        );
    }
    assert!(
        product
            .search_collection(binding.collection, &zero_distance, 4)?
            .hits
            .is_empty()
    );
    drop(product);

    let reopened = NativeProduct::open(&path)?;
    for request in [&ann, &hybrid, &zero_distance] {
        let deleted = reopened.search_collection(binding.collection, request, 5)?;
        assert_eq!(deleted.total_documents, 2);
        assert!(
            deleted
                .hits
                .iter()
                .all(|hit| hit.object_id != foreign && hit.object_id != member.object_id)
        );
    }
    assert!(
        reopened
            .search_collection(binding.collection, &zero_distance, 5)?
            .hits
            .is_empty()
    );
    drop(reopened);

    let runtime = NativeDatabase::open(&path)?;
    let raw = runtime.search_ann_latest(
        image_index,
        &ProductVector::new([0.0, 0.0])?,
        AnnSearchOptions::new(1, 2, None)?,
    )?;
    assert_eq!(raw.hits[0].object_id, foreign);
    drop(runtime);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn complete_image_fences_every_omitted_target_at_the_product_maximum()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("complete-image-max-targets");
    let (mut product, binding) = configure_full(
        &path,
        None,
        vec![AnalyzerFilter::Lowercase],
        MAX_PRODUCT_SEARCH_VECTOR_TARGETS,
    )
    .map_err(|error| format!("maximum-target configuration failed: {error:?}"))?;
    assert_eq!(binding.vectors.len(), MAX_PRODUCT_SEARCH_VECTOR_TARGETS);
    let mut expected_targets = vec!["image".to_owned(), "semantic".to_owned()];
    for ordinal in 2..MAX_PRODUCT_SEARCH_VECTOR_TARGETS {
        expected_targets.push(format!("target{ordinal:02}"));
    }
    assert_eq!(
        binding
            .vectors
            .iter()
            .map(|target| target.name.clone())
            .collect::<Vec<_>>(),
        expected_targets
    );

    // The first complete image inserts one physical vector and proves true
    // absence for every other named target.
    let mut complete = document(
        710,
        "maximum target image",
        "book",
        1,
        [3.0, 0.0],
        [4.0, 0.0],
    )?;
    complete.vectors.retain(|name, _| name == "image");
    let batch = ProductSearchIngestBatch {
        idempotency_id: 91,
        documents: vec![complete.clone()],
    };
    let original = product
        .ingest_search_batch(binding.collection, &batch, 1, ProductDurability::Strict)
        .map_err(|error| format!("maximum-target complete-image ingest failed: {error:?}"))?;
    let replay =
        product.ingest_search_batch(binding.collection, &batch, 2, ProductDurability::Strict)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.commit, original.commit);
    assert_eq!(
        product
            .search_collection(
                binding.collection,
                &vector_request("image", [3.0, 0.0], None, Some(0.0))?,
                2,
            )?
            .hits[0]
            .object_id,
        complete.object_id
    );
    for target in binding.vectors.iter().skip(1) {
        assert!(
            product
                .search_collection(
                    binding.collection,
                    &vector_request(&target.name, [4.0, 0.0], None, Some(0.0))?,
                    2,
                )?
                .hits
                .is_empty(),
            "omitted target {} remained visible",
            target.name
        );
    }

    // This replacement performs one physical delete for image and fifteen
    // conflict-authoritative absence fences for the targets proven absent by
    // the insert. Success at the product maximum guards the exact batch shape.
    let replacement = ProductDocument {
        object_id: complete.object_id,
        text: "maximum target replacement".to_owned(),
        doc_values: complete.doc_values.clone(),
        vectors: BTreeMap::new(),
    };
    let updated = product.update_search_document(
        binding.collection,
        &ProductSearchDocumentUpdate {
            idempotency_id: 92,
            document: replacement.clone(),
        },
        3,
        ProductDurability::Strict,
    )?;
    assert!(!updated.idempotent_replay);
    assert!(updated.commit.is_some());
    for target in &binding.vectors {
        let query = if target.name == "image" {
            [3.0, 0.0]
        } else {
            [4.0, 0.0]
        };
        assert!(
            product
                .search_collection(
                    binding.collection,
                    &vector_request(&target.name, query, None, Some(0.0))?,
                    3,
                )?
                .hits
                .is_empty(),
            "complete replacement retained target {}",
            target.name
        );
    }
    let page = NativeProduct::search_documents_at_snapshot(
        &product.snapshot_bounded(3)?,
        binding.collection,
        None,
        1,
    )?;
    assert_eq!(page.documents, [replacement]);

    // Deleting the vector-free complete image must still traverse and fence
    // every named target while removing the document and lexical state.
    let deleted = product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 93,
            object_id: complete.object_id,
        },
        4,
        ProductDurability::Strict,
    )?;
    assert!(!deleted.idempotent_replay);
    assert!(deleted.commit.is_some());
    assert!(
        NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(4)?,
            binding.collection,
            None,
            1,
        )?
        .documents
        .is_empty()
    );
    assert!(
        product
            .search_collection(
                binding.collection,
                &lexical_request("maximum target replacement"),
                4,
            )?
            .hits
            .is_empty()
    );

    // Exercise the delete entry point with the same maximum-width shape:
    // image is physically present and the other fifteen targets are absent.
    let mut direct_delete = document(
        711,
        "maximum target direct delete",
        "book",
        2,
        [5.0, 0.0],
        [6.0, 0.0],
    )?;
    direct_delete.vectors.retain(|name, _| name == "image");
    product.ingest_search_batch(
        binding.collection,
        &ProductSearchIngestBatch {
            idempotency_id: 94,
            documents: vec![direct_delete.clone()],
        },
        5,
        ProductDurability::Strict,
    )?;
    assert_eq!(
        product
            .search_collection(
                binding.collection,
                &vector_request("image", [5.0, 0.0], None, Some(0.0))?,
                5,
            )?
            .hits[0]
            .object_id,
        direct_delete.object_id
    );
    let direct_deleted = product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 95,
            object_id: direct_delete.object_id,
        },
        6,
        ProductDurability::Strict,
    )?;
    assert!(!direct_deleted.idempotent_replay);
    assert!(direct_deleted.commit.is_some());
    for target in &binding.vectors {
        assert!(
            product
                .search_collection(
                    binding.collection,
                    &vector_request(&target.name, [5.0, 0.0], None, None)?,
                    6,
                )?
                .hits
                .is_empty(),
            "direct delete retained target {}",
            target.name
        );
    }
    drop(product);

    let reopened = NativeProduct::open(&path)?;
    let page = NativeProduct::search_documents_at_snapshot(
        &reopened.snapshot_bounded(7)?,
        binding.collection,
        None,
        1,
    )?;
    assert!(page.documents.is_empty());
    for target in &binding.vectors {
        assert!(
            reopened
                .search_collection(
                    binding.collection,
                    &vector_request(&target.name, [3.0, 0.0], None, None)?,
                    7,
                )?
                .hits
                .is_empty(),
            "deleted document survived reopen in target {}",
            target.name
        );
    }
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_ann_and_hybrid_top_k_refill_after_complete_document_deletion()
-> Result<(), Box<dyn std::error::Error>> {
    const K: usize = 3;

    let path = temporary("delete-refills-top-k");
    let (mut product, binding) = configure_full(&path, None, vec![AnalyzerFilter::Lowercase], 3)?;
    let mut documents = Vec::new();
    for ordinal in 0..6_u128 {
        let coordinate = f32::from(u16::try_from(ordinal)?);
        let mut candidate = document(
            800 + ordinal,
            "shared deletion candidate",
            "book",
            i64::try_from(ordinal)?,
            [coordinate, 0.0],
            [coordinate, 0.0],
        )?;
        candidate.vectors.insert(
            "target02".to_owned(),
            ProductVector::new([coordinate, 0.0])?,
        );
        documents.push(candidate);
    }
    product.ingest_search_batch(
        binding.collection,
        &ProductSearchIngestBatch {
            idempotency_id: 94,
            documents,
        },
        1,
        ProductDurability::Strict,
    )?;

    let vector_branch = |target: &str, execution| ProductVectorBranch {
        target: target.to_owned(),
        query: ProductVector::new([0.0, 0.0]).expect("valid query vector"),
        candidate_limit: K,
        weight: 1,
        execution,
        max_distance: None,
    };
    let mut exact = lexical_request("");
    exact.lexical = None;
    exact.vectors = vec![vector_branch(
        "target02",
        Some(ProductVectorExecution::Exact),
    )];
    exact.limit = K;
    let mut ann = exact.clone();
    ann.vectors = vec![vector_branch(
        "image",
        Some(ProductVectorExecution::Ann {
            ef_search: 16,
            exact_rerank: Some(6),
        }),
    )];
    let mut hybrid = ann.clone();
    hybrid.lexical = Some(ProductLexicalBranch {
        query: "shared deletion".to_owned(),
        candidate_limit: K,
        weight: 1,
        operator: None,
        prefix: false,
        fields: Vec::new(),
        fuzzy: None,
        phrase: false,
    });

    let deleted_id = ObjectId::new(800)?;
    for request in [&exact, &ann, &hybrid] {
        let before = product.search_collection(binding.collection, request, 1)?;
        assert_eq!(before.hits.len(), K);
        assert_eq!(before.hits[0].object_id, deleted_id);
    }
    product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 95,
            object_id: deleted_id,
        },
        2,
        ProductDurability::Strict,
    )?;

    let assert_refilled = |result: &hyphae_native_product::ProductSearchResult| {
        assert_eq!(result.total_documents, 5);
        assert_eq!(result.hits.len(), K);
        assert!(result.hits.iter().all(|hit| hit.object_id != deleted_id));
    };
    for request in [&exact, &ann, &hybrid] {
        let after = product.search_collection(binding.collection, request, 2)?;
        assert_refilled(&after);
    }
    drop(product);

    let reopened = NativeProduct::open(&path)?;
    for request in [&exact, &ann, &hybrid] {
        let after = reopened.search_collection(binding.collection, request, 3)?;
        assert_refilled(&after);
    }
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn absent_at_snapshot_complete_insert_and_vector_upsert_conflict_in_both_orders()
-> Result<(), Box<dyn std::error::Error>> {
    for complete_first in [true, false] {
        let path = temporary(if complete_first {
            "insert-race-complete-first"
        } else {
            "insert-race-vector-first"
        });
        let (mut product, binding) = configure(&path)?;
        let mut complete_session = product_session(10)?;
        let mut vector_session = product_session(11)?;
        let complete_handle = begin_transaction(&mut product, &mut complete_session, 1)?;
        let vector_handle = begin_transaction(&mut product, &mut vector_session, 1)?;
        let mut complete = document(
            720,
            "concurrent complete insert",
            "book",
            1,
            [2.0, 0.0],
            [0.0, 0.0],
        )?;
        complete.vectors.remove("semantic");
        dispatch_operation(
            &mut product,
            &mut complete_session,
            2,
            ProductOperation::TransactionStageSearch {
                handle: complete_handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: binding.collection,
                    document: complete.clone(),
                },
            },
        )?;
        let semantic = binding
            .vectors
            .iter()
            .find(|target| target.name == "semantic")
            .ok_or("missing semantic target")?
            .index;
        dispatch_operation(
            &mut product,
            &mut vector_session,
            2,
            ProductOperation::TransactionStageVector {
                handle: vector_handle,
                mutation: ProductTransactionVectorMutation::Upsert {
                    index: semantic,
                    object_id: complete.object_id,
                    vector: ProductVector::new([9.0, 0.0])?,
                },
            },
        )?;

        let rejected = if complete_first {
            dispatch_operation(
                &mut product,
                &mut complete_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: complete_handle,
                },
            )?;
            dispatch_operation(
                &mut product,
                &mut vector_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: vector_handle,
                },
            )
        } else {
            dispatch_operation(
                &mut product,
                &mut vector_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: vector_handle,
                },
            )?;
            dispatch_operation(
                &mut product,
                &mut complete_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: complete_handle,
                },
            )
        };
        assert_eq!(
            rejected
                .expect_err("stale same-object writer committed")
                .code(),
            hyphae_native_product::ProductErrorCode::WriteConflict
        );

        let page = NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(0)?,
            binding.collection,
            None,
            1,
        )?;
        if complete_first {
            assert_eq!(page.documents, [complete.clone()]);
            assert_eq!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("image", [2.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits[0]
                    .object_id,
                complete.object_id
            );
            assert!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("semantic", [9.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits
                    .is_empty()
            );
        } else {
            assert!(page.documents.is_empty());
            assert!(
                product
                    .search_collection(binding.collection, &lexical_request("concurrent"), 0)?
                    .hits
                    .is_empty()
            );
        }
        drop(product);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn complete_replacement_and_vector_upsert_conflict_without_partial_images()
-> Result<(), Box<dyn std::error::Error>> {
    for complete_first in [true, false] {
        let path = temporary(if complete_first {
            "replace-race-complete-first"
        } else {
            "replace-race-vector-first"
        });
        let (mut product, binding) = configure(&path)?;
        let original = document(730, "original image", "book", 1, [0.0, 0.0], [0.0, 0.0])?;
        product.ingest_search_batch(
            binding.collection,
            &ProductSearchIngestBatch {
                idempotency_id: 92,
                documents: vec![original.clone()],
            },
            1,
            ProductDurability::Strict,
        )?;
        let mut complete_session = product_session(20)?;
        let mut vector_session = product_session(21)?;
        let complete_handle = begin_transaction(&mut product, &mut complete_session, 1)?;
        let vector_handle = begin_transaction(&mut product, &mut vector_session, 1)?;
        let mut replacement =
            document(730, "replacement image", "gear", 2, [2.0, 0.0], [0.0, 0.0])?;
        replacement.vectors.remove("semantic");
        dispatch_operation(
            &mut product,
            &mut complete_session,
            2,
            ProductOperation::TransactionStageSearch {
                handle: complete_handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: binding.collection,
                    document: replacement.clone(),
                },
            },
        )?;
        let semantic = binding
            .vectors
            .iter()
            .find(|target| target.name == "semantic")
            .ok_or("missing semantic target")?
            .index;
        dispatch_operation(
            &mut product,
            &mut vector_session,
            2,
            ProductOperation::TransactionStageVector {
                handle: vector_handle,
                mutation: ProductTransactionVectorMutation::Upsert {
                    index: semantic,
                    object_id: replacement.object_id,
                    vector: ProductVector::new([9.0, 0.0])?,
                },
            },
        )?;
        let rejected = if complete_first {
            dispatch_operation(
                &mut product,
                &mut complete_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: complete_handle,
                },
            )?;
            dispatch_operation(
                &mut product,
                &mut vector_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: vector_handle,
                },
            )
        } else {
            dispatch_operation(
                &mut product,
                &mut vector_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: vector_handle,
                },
            )?;
            dispatch_operation(
                &mut product,
                &mut complete_session,
                3,
                ProductOperation::TransactionCommit {
                    handle: complete_handle,
                },
            )
        };
        assert_eq!(
            rejected
                .expect_err("stale complete replacement committed")
                .code(),
            hyphae_native_product::ProductErrorCode::WriteConflict
        );

        let page = NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(0)?,
            binding.collection,
            None,
            1,
        )?;
        if complete_first {
            assert_eq!(page.documents, [replacement.clone()]);
            assert_eq!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("image", [2.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits[0]
                    .object_id,
                replacement.object_id
            );
            assert!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("semantic", [9.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits
                    .is_empty()
            );
            assert_eq!(
                product
                    .search_collection(binding.collection, &lexical_request("replacement"), 0)?
                    .hits[0]
                    .object_id,
                replacement.object_id
            );
        } else {
            let mut vector_winner = original.clone();
            vector_winner
                .vectors
                .insert("semantic".to_owned(), ProductVector::new([9.0, 0.0])?);
            assert_eq!(page.documents, [vector_winner]);
            assert!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("image", [2.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits
                    .is_empty()
            );
            assert_eq!(
                product
                    .search_collection(
                        binding.collection,
                        &vector_request("semantic", [9.0, 0.0], None, Some(0.0))?,
                        0,
                    )?
                    .hits[0]
                    .object_id,
                original.object_id
            );
            assert_eq!(
                product
                    .search_collection(binding.collection, &lexical_request("original"), 0)?
                    .hits[0]
                    .object_id,
                original.object_id
            );
        }
        drop(product);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn ordinary_absent_delete_is_a_noop_and_complete_replacement_rolls_back()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("complete-image-rollback");
    let (mut product, binding) = configure(&path)?;
    let original = document(740, "rollback original", "book", 1, [0.0, 0.0], [0.0, 0.0])?;
    product.ingest_search_batch(
        binding.collection,
        &ProductSearchIngestBatch {
            idempotency_id: 93,
            documents: vec![original.clone()],
        },
        1,
        ProductDurability::Strict,
    )?;
    let semantic = binding
        .vectors
        .iter()
        .find(|target| target.name == "semantic")
        .ok_or("missing semantic target")?
        .index;
    let before = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 1,
        })?;
    let mut session = product_session(30)?;
    let handle = begin_transaction(&mut product, &mut session, 1)?;
    let response = dispatch_operation(
        &mut product,
        &mut session,
        2,
        ProductOperation::TransactionStageVector {
            handle,
            mutation: ProductTransactionVectorMutation::Delete {
                index: semantic,
                object_id: ObjectId::new(999)?,
            },
        },
    )?;
    assert!(matches!(
        response,
        ProductResponse::TransactionStaged(ref receipt)
            if !receipt.changed
                && receipt.result == ProductTransactionStageResult::Vector(false)
    ));
    dispatch_operation(
        &mut product,
        &mut session,
        3,
        ProductOperation::TransactionRollback { handle },
    )?;
    let after_noop = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 1,
        })?;
    assert_eq!(after_noop.snapshot.root_digest, before.snapshot.root_digest);
    assert_eq!(after_noop.physical.page_count, before.physical.page_count);

    let handle = begin_transaction(&mut product, &mut session, 4)?;
    let mut replacement = document(
        740,
        "rollback replacement",
        "gear",
        2,
        [2.0, 0.0],
        [0.0, 0.0],
    )?;
    replacement.vectors.remove("semantic");
    dispatch_operation(
        &mut product,
        &mut session,
        5,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: replacement,
            },
        },
    )?;
    assert!(matches!(
        dispatch_operation(
            &mut product,
            &mut session,
            6,
            ProductOperation::TransactionRollback { handle },
        )?,
        ProductResponse::TransactionRolledBack(_)
    ));
    let page = NativeProduct::search_documents_at_snapshot(
        &product.snapshot_bounded(1)?,
        binding.collection,
        None,
        1,
    )?;
    assert_eq!(page.documents.as_slice(), std::slice::from_ref(&original));
    for target in ["image", "semantic"] {
        assert_eq!(
            product
                .search_collection(
                    binding.collection,
                    &vector_request(target, [0.0, 0.0], None, Some(0.0))?,
                    1,
                )?
                .hits[0]
                .object_id,
            original.object_id
        );
    }
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn complete_replacement_recovers_old_or_whole_image_at_every_commit_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    let boundaries = [
        (CommitBoundary::BlobStaged, false),
        (CommitBoundary::BlobPromoted, false),
        (CommitBoundary::PageAppended, false),
        (CommitBoundary::PageSynchronized, false),
        (CommitBoundary::WalAppended, true),
        (CommitBoundary::WalSynchronized, true),
        (CommitBoundary::RootPublished, true),
    ];
    for (boundary, replacement_committed) in boundaries {
        let path = temporary(&format!("complete-image-crash-{boundary:?}"));
        let (mut product, binding) = configure(&path)?;
        let original = document(750, "crash original", "book", 1, [0.0, 0.0], [0.0, 0.0])?;
        let original_receipt = product.ingest_search_batch(
            binding.collection,
            &ProductSearchIngestBatch {
                idempotency_id: 94,
                documents: vec![original.clone()],
            },
            1,
            ProductDurability::Strict,
        )?;
        let original_csn = original_receipt
            .commit
            .ok_or("missing original crash-matrix commit")?
            .commit_csn;
        let mut replacement =
            document(750, "crash replacement", "gear", 2, [2.0, 0.0], [0.0, 0.0])?;
        replacement.vectors.remove("semantic");
        let mut session = product_session(40)?;
        let handle = begin_transaction(&mut product, &mut session, 1)?;
        dispatch_operation(
            &mut product,
            &mut session,
            2,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: binding.collection,
                    document: replacement.clone(),
                },
            },
        )?;
        let context = proof_context(&session, 3);
        assert!(
            commit_explicit_transaction_with_interruption_for_test(
                &mut product,
                &mut session,
                &context,
                handle,
                boundary,
            )
            .is_err()
        );
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        let expected = if replacement_committed {
            &replacement
        } else {
            &original
        };
        let page = NativeProduct::search_documents_at_snapshot(
            &reopened.snapshot_bounded(1)?,
            binding.collection,
            None,
            1,
        )?;
        assert_eq!(page.documents.as_slice(), std::slice::from_ref(expected));
        assert_eq!(
            page.snapshot.visible_csn.map(hyphae_native_types::Csn::get),
            Some(original_csn + u64::from(replacement_committed)),
            "unexpected recovered CSN at {boundary:?}"
        );
        assert_eq!(
            reopened
                .search_collection(
                    binding.collection,
                    &vector_request(
                        "image",
                        if replacement_committed {
                            [2.0, 0.0]
                        } else {
                            [0.0, 0.0]
                        },
                        None,
                        Some(0.0),
                    )?,
                    1,
                )?
                .hits[0]
                .object_id,
            expected.object_id
        );
        assert!(
            reopened
                .search_collection(
                    binding.collection,
                    &vector_request(
                        "image",
                        if replacement_committed {
                            [0.0, 0.0]
                        } else {
                            [2.0, 0.0]
                        },
                        None,
                        Some(0.0),
                    )?,
                    1,
                )?
                .hits
                .is_empty(),
            "mixed image survived at {boundary:?}"
        );
        let semantic = reopened.search_collection(
            binding.collection,
            &vector_request("semantic", [0.0, 0.0], None, Some(0.0))?,
            1,
        )?;
        assert_eq!(semantic.hits.is_empty(), replacement_committed);
        assert_eq!(
            reopened
                .search_collection(
                    binding.collection,
                    &lexical_request(if replacement_committed {
                        "replacement"
                    } else {
                        "original"
                    }),
                    1,
                )?
                .hits[0]
                .object_id,
            expected.object_id
        );
        assert!(
            reopened
                .search_collection(
                    binding.collection,
                    &lexical_request(if replacement_committed {
                        "original"
                    } else {
                        "replacement"
                    }),
                    1,
                )?
                .hits
                .is_empty(),
            "mixed lexical image survived at {boundary:?}"
        );
        drop(reopened);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn proof_session() -> Result<ProductSession, Box<dyn std::error::Error>> {
    product_session(1)
}

fn product_session(id: u128) -> Result<ProductSession, Box<dyn std::error::Error>> {
    let principal = ProductPrincipal::new("integrated-proof").ok_or("invalid principal")?;
    Ok(ProductSession::new(
        ProductSessionId::new(id).ok_or("zero session")?,
        principal,
        ProductAuthorization::ALL,
    ))
}

fn proof_context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
}

#[allow(clippy::result_large_err)]
fn dispatch_operation(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    operation: ProductOperation,
) -> Result<ProductResponse, ProductError> {
    let context = proof_context(session, request_id);
    product.dispatch(session, &context, operation)
}

fn begin_transaction(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
) -> Result<ProductTransactionHandle, Box<dyn std::error::Error>> {
    let response = dispatch_operation(
        product,
        session,
        request_id,
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = response
    else {
        return Err("transaction did not begin".into());
    };
    Ok(handle)
}

fn vector_request(
    target: &str,
    query: [f32; 2],
    execution: Option<ProductVectorExecution>,
    max_distance: Option<f64>,
) -> Result<ProductSearchRequest, Box<dyn std::error::Error>> {
    Ok(ProductSearchRequest {
        lexical: None,
        vectors: vec![ProductVectorBranch {
            target: target.to_owned(),
            query: ProductVector::new(query)?,
            candidate_limit: 16,
            weight: 1,
            execution,
            max_distance: max_distance.map(hyphae_native_product::CanonicalF64::new),
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 16,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    })
}

fn lexical_request(query: &str) -> ProductSearchRequest {
    ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: query.to_owned(),
            candidate_limit: 16,
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
        limit: 16,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    }
}

#[test]
fn integrated_search_reopens_with_filters_sort_facets_metrics_and_same_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("reopen");
    let (mut product, binding) = configure(&path)?;
    let ingested =
        product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    assert_eq!(ingested.documents, 4);
    drop(product);

    let reopened = NativeProduct::open(&path)?;
    let result = reopened.search_collection(
        binding.collection,
        &ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust database".into(),
                candidate_limit: 4,
                weight: 1,
                operator: None,
                prefix: false,
                fields: Vec::new(),
                fuzzy: None,
                phrase: false,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::Compare {
                field: "category".into(),
                operator: ProductSearchOperator::Equal,
                value: ProductDocValue::String("book".into()),
            },
            sort: vec![ProductSearchSort {
                source: ProductSortSource::Field("price".into()),
                direction: ProductSortDirection::Ascending,
                missing: ProductMissingPlacement::Last,
            }],
            facets: vec![ProductFacetRequest {
                field: "category".into(),
                limit: 4,
            }],
            range_facets: Vec::new(),
            aggregations: vec![
                ProductNamedAggregation {
                    name: "count".into(),
                    aggregation: ProductAggregation::Count,
                },
                ProductNamedAggregation {
                    name: "sum_price".into(),
                    aggregation: ProductAggregation::Sum("price".into()),
                },
            ],
            limit: 10,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        7,
    )?;
    assert_eq!(result.snapshot.visible_csn, ingested.snapshot.visible_csn);
    assert_eq!(result.total_documents, 4);
    assert_eq!(result.eligible_documents, 2);
    assert_eq!(result.hits[0].object_id, ObjectId::new(202)?);
    assert_eq!(result.facets[0].buckets[0].count, 2);
    assert_eq!(
        result.aggregations[0].value,
        ProductAggregationValue::Count(2)
    );
    assert_eq!(
        result.aggregations[1].value,
        ProductAggregationValue::Integer(Some(40))
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn adaptive_exact_broad_filter_aware_ann_and_multi_target_rrf_are_reported()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("strategies");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 0, ProductDurability::Strict)?;

    let restrictive = product.search_collection(
        binding.collection,
        &ProductSearchRequest {
            lexical: None,
            vectors: vec![ProductVectorBranch {
                target: "semantic".into(),
                query: ProductVector::new([0.0, 0.0])?,
                candidate_limit: 2,
                weight: 1,
                execution: Some(ProductVectorExecution::Adaptive {
                    exact_candidate_threshold: 2,
                    ef_search: 8,
                    exact_rerank: Some(4),
                }),
                max_distance: None,
            }],
            filter: ProductSearchFilter::Compare {
                field: "price".into(),
                operator: ProductSearchOperator::Less,
                value: ProductDocValue::Integer(15),
            },
            sort: Vec::new(),
            facets: Vec::new(),
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 2,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        0,
    )?;
    assert_eq!(restrictive.eligible_documents, 1);
    assert_eq!(
        restrictive.vector_branches[0].strategy,
        ProductVectorStrategy::AdaptiveExactFiltered
    );
    assert!(!restrictive.approximate);

    let broad = product.search_collection(
        binding.collection,
        &ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".into(),
                candidate_limit: 4,
                weight: 2,
                operator: None,
                prefix: false,
                fields: Vec::new(),
                fuzzy: None,
                phrase: false,
            }),
            vectors: vec![
                ProductVectorBranch {
                    target: "image".into(),
                    query: ProductVector::new([0.0, 0.0])?,
                    candidate_limit: 3,
                    weight: 1,
                    execution: Some(ProductVectorExecution::Ann {
                        ef_search: 8,
                        exact_rerank: Some(4),
                    }),
                    max_distance: None,
                },
                ProductVectorBranch {
                    target: "semantic".into(),
                    query: ProductVector::new([1.0, 0.0])?,
                    candidate_limit: 3,
                    weight: 1,
                    execution: Some(ProductVectorExecution::Adaptive {
                        exact_candidate_threshold: 2,
                        ef_search: 8,
                        exact_rerank: Some(4),
                    }),
                    max_distance: None,
                },
            ],
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        0,
    )?;
    assert_eq!(broad.vector_branches.len(), 2);
    assert_eq!(
        broad.vector_branches[0].strategy,
        ProductVectorStrategy::FilterAwareAnn
    );
    assert_eq!(
        broad.vector_branches[1].strategy,
        ProductVectorStrategy::AdaptiveFilterAwareAnn
    );
    assert!(
        broad
            .vector_branches
            .iter()
            .all(|receipt| receipt.candidate_count > 0 && receipt.exact_reranked)
    );
    // Match-all is the four-document manifest allowlist, so the native
    // filtered path selects its established eligible-count <= ef exact mode.
    assert!(!broad.approximate);
    assert!(broad.lexical_candidates > 0);
    assert!(broad.hits.len() >= 3);

    let exact_policy_error = product
        .search_collection(
            binding.collection,
            &ProductSearchRequest {
                lexical: None,
                vectors: vec![ProductVectorBranch {
                    target: "image".into(),
                    query: ProductVector::new([0.0, 0.0])?,
                    candidate_limit: 2,
                    weight: 1,
                    execution: Some(ProductVectorExecution::Exact),
                    max_distance: None,
                }],
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
                range_facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 2,
                fusion: None,
                parent_dedupe: None,
                rerank: None,
                highlight: None,
                autocut: None,
                offset: 0,
            },
            0,
        )
        .expect_err("ANN catalog policy accepted exact execution");
    assert_eq!(
        exact_policy_error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    let ef_max_error = product
        .search_collection(
            binding.collection,
            &ProductSearchRequest {
                lexical: None,
                vectors: vec![ProductVectorBranch {
                    target: "image".into(),
                    query: ProductVector::new([0.0, 0.0])?,
                    candidate_limit: 2,
                    weight: 1,
                    execution: Some(ProductVectorExecution::Ann {
                        ef_search: 257,
                        exact_rerank: None,
                    }),
                    max_distance: None,
                }],
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
                range_facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 2,
                fusion: None,
                parent_dedupe: None,
                rerank: None,
                highlight: None,
                autocut: None,
                offset: 0,
            },
            0,
        )
        .expect_err("ef_search_max was not enforced");
    assert_eq!(
        ef_max_error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn bounded_product_ann_preserves_shadow_underfill_filtered_hybrid_and_native_counters()
-> Result<(), Box<dyn std::error::Error>> {
    const DOCUMENT_COUNT: u16 = 128;
    const KEPT_DOCUMENTS: u16 = 64;

    let path = temporary("bounded-shadow-traversal");
    let (mut product, binding) = configure_full(&path, None, vec![AnalyzerFilter::Lowercase], 1)?;
    let image_index = binding
        .vectors
        .iter()
        .find(|vector| vector.name == "image")
        .ok_or("missing image vector binding")?
        .index;
    let documents = (0..DOCUMENT_COUNT)
        .map(|ordinal| {
            let mut document = document(
                10_000 + u128::from(ordinal),
                if ordinal == 2 { "hybrid" } else { "plain" },
                if ordinal < KEPT_DOCUMENTS {
                    "keep"
                } else {
                    "drop"
                },
                i64::from(ordinal),
                [f32::from(ordinal), 0.0],
                [f32::from(ordinal), 0.0],
            )?;
            document.vectors.remove("semantic");
            Ok::<_, Box<dyn std::error::Error>>(document)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let shadowed = documents[0].object_id;
    let second = documents[1].object_id;
    let third = documents[2].object_id;
    for (batch, chunk) in documents.chunks(4).enumerate() {
        product
            .ingest_search_batch(
                binding.collection,
                &ProductSearchIngestBatch {
                    idempotency_id: u128::try_from(batch)? + 1,
                    documents: chunk.to_vec(),
                },
                1,
                ProductDurability::Strict,
            )
            .map_err(|error| format!("large product ingest batch {batch} failed: {error:?}"))?;
    }
    drop(product);

    // Consolidating the product ingest's object deltas creates a real single
    // HNSW base. The later product update then shadows its nearest vector.
    let mut runtime = NativeDatabase::open(&path)?;
    let consolidation = runtime.plan_ann_consolidation(
        image_index,
        usize::from(DOCUMENT_COUNT),
        usize::from(DOCUMENT_COUNT),
    )?;
    runtime.consolidate_ann(consolidation, DurabilityClass::Strict)?;
    let observation = runtime.observe_ann_index(image_index)?;
    assert_eq!(observation.base_vector_count, usize::from(DOCUMENT_COUNT));
    assert_eq!(observation.delta_records, 0);
    drop(runtime);

    let mut product = NativeProduct::open(&path)?;
    let mut replacement = documents[0].clone();
    assert_eq!(replacement.object_id, shadowed);
    replacement.vectors.remove("image");
    product
        .update_search_document(
            binding.collection,
            &ProductSearchDocumentUpdate {
                idempotency_id: 100,
                document: replacement,
            },
            2,
            ProductDurability::Strict,
        )
        .map_err(|error| format!("shadowing product update failed: {error:?}"))?;

    let filter = ProductSearchFilter::Compare {
        field: "category".into(),
        operator: ProductSearchOperator::Equal,
        value: ProductDocValue::String("keep".into()),
    };
    let mut low = vector_request(
        "image",
        [0.0, 0.0],
        Some(ProductVectorExecution::Ann {
            ef_search: 2,
            exact_rerank: None,
        }),
        None,
    )?;
    low.vectors[0].candidate_limit = 2;
    low.limit = 2;
    let underfilled = product
        .search_collection(binding.collection, &low, 2)
        .map_err(|error| format!("low-breadth product search failed: {error:?}"))?;
    assert_eq!(underfilled.eligible_documents, usize::from(DOCUMENT_COUNT));
    assert_eq!(underfilled.hits.len(), 1);
    assert_eq!(underfilled.hits[0].object_id, second);
    assert!(underfilled.approximate);
    assert_eq!(
        underfilled.vector_branches[0].strategy,
        ProductVectorStrategy::FilterAwareAnn
    );
    assert!(underfilled.vector_branches[0].visited_nodes > 0);
    assert!(!underfilled.vector_branches[0].exact_reranked);

    let mut high = low.clone();
    high.vectors[0].execution = Some(ProductVectorExecution::Ann {
        ef_search: 3,
        exact_rerank: None,
    });
    let filled = product
        .search_collection(binding.collection, &high, 2)
        .map_err(|error| format!("high-breadth product search failed: {error:?}"))?;
    assert_eq!(
        filled
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [second, third]
    );
    assert!(filled.approximate);

    let mut hybrid = low.clone();
    hybrid.filter = filter;
    hybrid.lexical = Some(ProductLexicalBranch {
        query: "hybrid".into(),
        candidate_limit: 1,
        weight: 2,
        operator: None,
        prefix: false,
        fields: Vec::new(),
        fuzzy: None,
        phrase: false,
    });
    let hybrid_result = product
        .search_collection(binding.collection, &hybrid, 2)
        .map_err(|error| format!("hybrid product search failed: {error:?}"))?;
    assert_eq!(
        hybrid_result.eligible_documents,
        usize::from(KEPT_DOCUMENTS)
    );
    assert_eq!(hybrid_result.lexical_candidates, 1);
    assert_eq!(hybrid_result.retrieval_candidates, 2);
    assert_eq!(
        hybrid_result
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [third, second]
    );
    assert!(hybrid_result.hits.iter().all(|hit| matches!(
        hit.doc_values.get("category"),
        Some(ProductDocValue::String(category)) if category == "keep"
    )));

    let mut single = low.clone();
    single.vectors[0].candidate_limit = 1;
    single.vectors[0].execution = Some(ProductVectorExecution::Ann {
        ef_search: 1,
        exact_rerank: None,
    });
    single.limit = 1;
    let single_result = product
        .search_collection(binding.collection, &single, 2)
        .map_err(|error| format!("single-candidate product search failed: {error:?}"))?;
    assert!(single_result.hits.is_empty());
    let product_receipt = single_result.vector_branches[0].clone();
    assert!(product_receipt.candidate_count > 0);
    assert!(product_receipt.candidate_count < usize::from(DOCUMENT_COUNT));
    assert_eq!(
        product_receipt.candidate_count,
        product_receipt.visited_nodes
    );
    assert!(!product_receipt.exact_reranked);
    drop(product);

    let runtime = NativeDatabase::open(&path)?;
    let collection_allowlist = (0..DOCUMENT_COUNT)
        .map(|ordinal| ObjectId::new(10_000 + u128::from(ordinal)))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    let direct = runtime.search_ann_filtered_latest(
        image_index,
        &ProductVector::new([0.0, 0.0])?,
        AnnSearchOptions::new(1, 1, None)?,
        &collection_allowlist,
    )?;
    assert_eq!(
        direct.strategy,
        AnnSearchStrategy::StableIdEligibilityTraversal
    );
    assert!(direct.hits.is_empty());
    assert_eq!(product_receipt.candidate_count, direct.candidate_count);
    assert_eq!(product_receipt.visited_nodes, direct.visited_nodes);
    assert_eq!(product_receipt.exact_reranked, direct.exact_reranked);
    assert_eq!(product_receipt.approximate, direct.approximate);

    let kept_allowlist = (0..KEPT_DOCUMENTS)
        .map(|ordinal| ObjectId::new(10_000 + u128::from(ordinal)))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    let direct_filtered = runtime.search_ann_filtered_latest(
        image_index,
        &ProductVector::new([0.0, 0.0])?,
        AnnSearchOptions::new(2, 2, None)?,
        &kept_allowlist,
    )?;
    assert_eq!(
        direct_filtered.strategy,
        AnnSearchStrategy::StableIdEligibilityTraversal
    );
    assert_eq!(
        direct_filtered
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [second]
    );
    assert_eq!(
        hybrid_result.vector_branches[0].candidate_count,
        direct_filtered.candidate_count
    );
    assert_eq!(
        hybrid_result.vector_branches[0].visited_nodes,
        direct_filtered.visited_nodes
    );
    assert_eq!(
        hybrid_result.vector_branches[0].exact_reranked,
        direct_filtered.exact_reranked
    );
    drop(runtime);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_ann_and_hybrid_proofs_reexecute_declared_branches_and_reject_ann_metadata_forgery()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("semantic-proofs");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 0, ProductDurability::Strict)?;
    let mut session = proof_session()?;

    let exact = ProductSearchRequest {
        lexical: None,
        vectors: vec![ProductVectorBranch {
            target: "semantic".into(),
            query: ProductVector::new([0.0, 0.0])?,
            candidate_limit: 2,
            weight: 1,
            execution: None,
            max_distance: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 2,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let ann = ProductSearchRequest {
        lexical: None,
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: ProductVector::new([0.0, 0.0])?,
            candidate_limit: 3,
            weight: 1,
            execution: None,
            max_distance: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 3,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let hybrid = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust".into(),
            candidate_limit: 4,
            weight: 2,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: vec![ProductVectorBranch {
            target: "semantic".into(),
            query: ProductVector::new([1.0, 0.0])?,
            candidate_limit: 3,
            weight: 1,
            execution: None,
            max_distance: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };

    let mut ann_artifact = None;
    for (request_id, expected, request) in [
        (1, NativeProofKind::Ann, exact),
        (2, NativeProofKind::Ann, ann),
        (3, NativeProofKind::Hybrid, hybrid),
    ] {
        let context = proof_context(&session, request_id);
        let (_, artifact) = generate_native_operation_proof(
            &mut product,
            &mut session,
            &context,
            &ProductOperation::SearchCollection {
                collection: binding.collection,
                request,
            },
            NativeProofGenerationLimits::default(),
        )?;
        assert_eq!(artifact.proof.content().kind, expected);
        let report = verify_native_proof_offline(
            &artifact.proof_bytes,
            &artifact.witness_bytes,
            artifact.trusted_anchor,
            &NativeVerificationLimits::default(),
        )?;
        assert!(report.semantic_reexecution_performed);
        if expected == NativeProofKind::Ann {
            ann_artifact = Some(artifact);
        }
    }

    let artifact = ann_artifact.ok_or("missing ANN proof artifact")?;
    let mut forged = artifact.proof.content().clone();
    forged
        .ann
        .as_mut()
        .ok_or("missing ANN metadata")?
        .search_breadth += 1;
    let forged = encode_native_proof(&NativeProof::new(forged)?, &ProofCodecLimits::default())?;
    assert!(
        verify_native_proof_offline(
            &forged,
            &artifact.witness_bytes,
            artifact.trusted_anchor,
            &NativeVerificationLimits::default(),
        )
        .is_err()
    );

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn invalid_batch_is_atomic_and_stream_enforces_backpressure_and_idempotency()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("atomic-stream");
    let (mut product, binding) = configure(&path)?;
    let before = product.snapshot_bounded(0)?.identity();
    let mut invalid = seed()?;
    invalid.idempotency_id = 2;
    invalid.documents[3]
        .vectors
        .insert("semantic".into(), ProductVector::new([1.0, 2.0, 3.0])?);
    assert!(
        product
            .ingest_search_batch(binding.collection, &invalid, 0, ProductDurability::Strict)
            .is_err()
    );
    assert_eq!(product.snapshot_bounded(0)?.identity(), before);

    let first = ProductSearchIngestBatch {
        idempotency_id: 10,
        documents: vec![document(301, "first", "book", 1, [0.0, 0.0], [0.0, 0.0])?],
    };
    let second = ProductSearchIngestBatch {
        idempotency_id: 11,
        documents: vec![document(302, "second", "book", 2, [1.0, 0.0], [1.0, 0.0])?],
    };
    let first_bytes = format!("{first:?}").len();
    let mut stream = ProductSearchIngestionCoordinator {
        max_in_flight_bytes: first_bytes * 2,
        max_in_flight_batches: 1,
        max_tracked_idempotency_ids: 4,
    }
    .stream(binding.collection)?;
    assert_eq!(
        stream.enqueue(first.clone())?,
        ProductStreamEnqueueOutcome::Enqueued
    );
    assert_eq!(
        stream.enqueue(first.clone())?,
        ProductStreamEnqueueOutcome::Idempotent
    );
    let mut conflicting_first = first.clone();
    conflicting_first.documents[0].text = "different".into();
    assert_eq!(
        stream
            .enqueue(conflicting_first)
            .expect_err("stream accepted conflicting idempotency payload")
            .code(),
        hyphae_native_product::ProductErrorCode::IdempotencyConflict,
    );
    let queued_bytes = stream.in_flight_bytes();
    assert!(stream.enqueue(second.clone()).is_err());
    assert_eq!(stream.in_flight_bytes(), queued_bytes);
    assert_eq!(stream.queued_batches(), 1);
    let receipt = stream
        .flush_next(&mut product, 0, ProductDurability::Strict)?
        .ok_or("missing stream receipt")?;
    assert!(!receipt.idempotent_replay);
    assert_eq!(stream.in_flight_bytes(), 0);
    assert_eq!(
        stream.enqueue(second)?,
        ProductStreamEnqueueOutcome::Enqueued
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn idempotency_update_and_m05_document_delete_survive_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("lifecycle");
    let (mut product, binding) = configure(&path)?;
    let batch = ProductSearchIngestBatch {
        idempotency_id: 41,
        documents: vec![document(
            501,
            "old token",
            "book",
            1,
            [0.0, 0.0],
            [0.0, 0.0],
        )?],
    };
    let original =
        product.ingest_search_batch(binding.collection, &batch, 0, ProductDurability::Strict)?;
    let replay =
        product.ingest_search_batch(binding.collection, &batch, 0, ProductDurability::Strict)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.documents, original.documents);
    assert_eq!(replay.commit, original.commit);

    let mut conflict = batch.clone();
    conflict.documents[0].text = "different token".into();
    assert_eq!(
        product
            .ingest_search_batch(binding.collection, &conflict, 0, ProductDurability::Strict,)
            .expect_err("different payload reused an idempotency token")
            .code(),
        hyphae_native_product::ProductErrorCode::IdempotencyConflict,
    );

    let mut replacement = document(501, "new token", "gear", 2, [1.0, 0.0], [1.0, 0.0])?;
    replacement.vectors.remove("semantic");
    let update = ProductSearchDocumentUpdate {
        idempotency_id: 42,
        document: replacement,
    };
    let updated = product.update_search_document(
        binding.collection,
        &update,
        0,
        ProductDurability::Strict,
    )?;
    let update_commit = updated.commit.ok_or("missing update commit")?;
    let after_update = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 0,
        })?
        .physical;
    let update_replay = product.update_search_document(
        binding.collection,
        &update,
        1,
        ProductDurability::Strict,
    )?;
    assert!(update_replay.idempotent_replay);
    assert_eq!(update_replay.commit, Some(update_commit));
    assert_eq!(
        update_replay.commit.map(|commit| commit.transaction_id),
        Some(update_commit.transaction_id)
    );
    assert_eq!(
        update_replay.commit.map(|commit| commit.commit_csn),
        Some(update_commit.commit_csn)
    );
    let after_update_replay = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 1,
        })?
        .physical;
    assert_eq!(after_update_replay.page_count, after_update.page_count);
    assert_eq!(after_update_replay.wal_bytes, after_update.wal_bytes);
    drop(product);

    let mut product = NativeProduct::open(&path)?;
    let before_reopened_update_replay = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 2,
        })?
        .physical;
    let reopened_update_replay = product.update_search_document(
        binding.collection,
        &update,
        2,
        ProductDurability::Strict,
    )?;
    assert!(reopened_update_replay.idempotent_replay);
    assert_eq!(reopened_update_replay.commit, Some(update_commit));
    assert_eq!(
        reopened_update_replay
            .commit
            .map(|commit| (commit.transaction_id, commit.commit_csn)),
        Some((update_commit.transaction_id, update_commit.commit_csn))
    );
    let after_reopened_update_replay = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 2,
        })?
        .physical;
    assert_eq!(
        after_reopened_update_replay.page_count,
        before_reopened_update_replay.page_count
    );
    assert_eq!(
        after_reopened_update_replay.wal_bytes,
        before_reopened_update_replay.wal_bytes
    );
    let query = |text: &str| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: text.into(),
            candidate_limit: 4,
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
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    assert!(
        product
            .search_collection(binding.collection, &query("old"), 0)?
            .hits
            .is_empty()
    );
    assert_eq!(
        product
            .search_collection(binding.collection, &query("new"), 0)?
            .hits
            .len(),
        1
    );
    assert_eq!(
        product
            .search_collection(
                binding.collection,
                &vector_request("image", [1.0, 0.0], None, Some(0.0))?,
                0,
            )?
            .hits[0]
            .object_id,
        ObjectId::new(501)?
    );
    assert!(
        product
            .search_collection(
                binding.collection,
                &vector_request("semantic", [0.0, 0.0], None, None)?,
                0,
            )?
            .hits
            .is_empty()
    );

    let delete = ProductSearchDocumentDelete {
        idempotency_id: 43,
        object_id: ObjectId::new(501)?,
    };
    let deleted =
        product.delete_search_document(binding.collection, delete, 3, ProductDurability::Strict)?;
    let delete_commit = deleted.commit.ok_or("missing delete commit")?;
    let after_delete = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 3,
        })?
        .physical;
    let delete_replay =
        product.delete_search_document(binding.collection, delete, 4, ProductDurability::Strict)?;
    assert!(delete_replay.idempotent_replay);
    assert_eq!(delete_replay.commit, Some(delete_commit));
    assert_eq!(
        delete_replay
            .commit
            .map(|commit| (commit.transaction_id, commit.commit_csn)),
        Some((delete_commit.transaction_id, delete_commit.commit_csn))
    );
    let after_delete_replay = product
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 4,
        })?
        .physical;
    assert_eq!(after_delete_replay.page_count, after_delete.page_count);
    assert_eq!(after_delete_replay.wal_bytes, after_delete.wal_bytes);
    drop(product);
    let mut reopened = NativeProduct::open(&path)?;
    let before_reopened_delete_replay = reopened
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 5,
        })?
        .physical;
    let reopened_delete_replay = reopened.delete_search_document(
        binding.collection,
        delete,
        5,
        ProductDurability::Strict,
    )?;
    assert!(reopened_delete_replay.idempotent_replay);
    assert_eq!(reopened_delete_replay.commit, Some(delete_commit));
    assert_eq!(
        reopened_delete_replay
            .commit
            .map(|commit| (commit.transaction_id, commit.commit_csn)),
        Some((delete_commit.transaction_id, delete_commit.commit_csn))
    );
    let after_reopened_delete_replay = reopened
        .administration()
        .status(hyphae_native_product::StatusRequest {
            logical_time_micros: 5,
        })?
        .physical;
    assert_eq!(
        after_reopened_delete_replay.page_count,
        before_reopened_delete_replay.page_count
    );
    assert_eq!(
        after_reopened_delete_replay.wal_bytes,
        before_reopened_delete_replay.wal_bytes
    );
    let reopened_replay =
        reopened.ingest_search_batch(binding.collection, &batch, 0, ProductDurability::Strict)?;
    assert_eq!(reopened_replay.commit, original.commit);
    assert_eq!(
        reopened.resolve_search_collection_binding(binding.collection, 0)?,
        binding,
    );
    assert!(
        reopened
            .search_collection(binding.collection, &query("new"), 0)?
            .hits
            .is_empty()
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

/// Deterministic pseudo-random sequence for the equivalence exercise.
fn equivalence_step(seed: u64, step: u64) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"posting-equivalence-v1");
    hasher.update(&seed.to_le_bytes());
    hasher.update(&step.to_le_bytes());
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .unwrap_or([0; 8]),
    )
}

/// Pure reference mirroring the runtime's linear `filter_matches`
/// semantics; the posting path must never diverge from it.
fn reference_eligible(
    documents: &std::collections::BTreeMap<u128, BTreeMap<String, ProductDocValue>>,
    filter: &ProductSearchFilter,
) -> std::collections::BTreeSet<u128> {
    fn matches(values: &BTreeMap<String, ProductDocValue>, filter: &ProductSearchFilter) -> bool {
        match filter {
            ProductSearchFilter::MatchAll => true,
            ProductSearchFilter::Exists(field) => values.contains_key(field),
            ProductSearchFilter::Compare {
                field,
                operator,
                value,
            } => values.get(field).is_some_and(|actual| {
                if std::mem::discriminant(actual) != std::mem::discriminant(value) {
                    return false;
                }
                match operator {
                    ProductSearchOperator::Equal => actual == value,
                    ProductSearchOperator::NotEqual => actual != value,
                    ProductSearchOperator::Less => actual < value,
                    ProductSearchOperator::LessOrEqual => actual <= value,
                    ProductSearchOperator::Greater => actual > value,
                    ProductSearchOperator::GreaterOrEqual => actual >= value,
                }
            }),
            ProductSearchFilter::All(children) => {
                children.iter().all(|child| matches(values, child))
            }
            ProductSearchFilter::Any(children) => {
                children.iter().any(|child| matches(values, child))
            }
            ProductSearchFilter::Not(child) => !matches(values, child),
            ProductSearchFilter::In {
                field,
                values: members,
            } => values.get(field).is_some_and(|actual| {
                members.iter().any(|member| {
                    std::mem::discriminant(actual) == std::mem::discriminant(member)
                        && actual == member
                })
            }),
            ProductSearchFilter::IsNull(field) => !values.contains_key(field),
            ProductSearchFilter::Like { field, pattern } => {
                values.get(field).is_some_and(|actual| {
                    if let ProductDocValue::String(text) = actual {
                        hyphae_native_runtime::like_matches(pattern, text)
                    } else {
                        false
                    }
                })
            }
        }
    }
    documents
        .iter()
        .filter(|(_, values)| matches(values, filter))
        .map(|(id, _)| *id)
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn posting_eligibility_matches_the_reference_under_randomized_lifecycle()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("posting-equivalence");
    let (mut product, binding) = configure(&path)?;
    let mut model: std::collections::BTreeMap<u128, BTreeMap<String, ProductDocValue>> =
        std::collections::BTreeMap::new();
    let categories = ["book", "gear", "tool", "misc"];
    let mut idempotency = 1_u128;

    for seed in 0..4_u64 {
        for step in 0..24_u64 {
            let roll = equivalence_step(seed, step);
            let id = 300 + u128::from(roll % 12);
            let category = categories[(roll >> 8) as usize % categories.len()];
            let price = i64::try_from((roll >> 16) % 100)? - 50;
            idempotency += 1;
            let doc = document(
                id,
                "equivalence corpus text",
                category,
                price,
                [0.0, 0.0],
                [0.0, 0.0],
            )?;
            match roll % 3 {
                0 | 1 if !model.contains_key(&id) => {
                    let batch = ProductSearchIngestBatch {
                        idempotency_id: idempotency,
                        documents: vec![doc.clone()],
                    };
                    product.ingest_search_batch(
                        binding.collection,
                        &batch,
                        0,
                        ProductDurability::Memory,
                    )?;
                    model.insert(id, doc.doc_values);
                }
                0 | 1 => {
                    product.update_search_document(
                        binding.collection,
                        &ProductSearchDocumentUpdate {
                            idempotency_id: idempotency,
                            document: doc.clone(),
                        },
                        0,
                        ProductDurability::Memory,
                    )?;
                    model.insert(id, doc.doc_values);
                }
                _ if model.contains_key(&id) => {
                    product.delete_search_document(
                        binding.collection,
                        ProductSearchDocumentDelete {
                            idempotency_id: idempotency,
                            object_id: ObjectId::new(id)?,
                        },
                        0,
                        ProductDurability::Memory,
                    )?;
                    model.remove(&id);
                }
                _ => {}
            }

            let probe_value = ProductDocValue::Integer(price);
            let filters = [
                ProductSearchFilter::MatchAll,
                ProductSearchFilter::Exists("category".into()),
                ProductSearchFilter::Compare {
                    field: "category".into(),
                    operator: ProductSearchOperator::Equal,
                    value: ProductDocValue::String(category.into()),
                },
                ProductSearchFilter::Compare {
                    field: "price".into(),
                    operator: ProductSearchOperator::Less,
                    value: probe_value.clone(),
                },
                ProductSearchFilter::Compare {
                    field: "price".into(),
                    operator: ProductSearchOperator::GreaterOrEqual,
                    value: probe_value.clone(),
                },
                ProductSearchFilter::Compare {
                    field: "price".into(),
                    operator: ProductSearchOperator::NotEqual,
                    value: probe_value.clone(),
                },
                ProductSearchFilter::Not(Box::new(ProductSearchFilter::Compare {
                    field: "category".into(),
                    operator: ProductSearchOperator::Equal,
                    value: ProductDocValue::String("book".into()),
                })),
                ProductSearchFilter::Any(vec![
                    ProductSearchFilter::Compare {
                        field: "category".into(),
                        operator: ProductSearchOperator::Equal,
                        value: ProductDocValue::String("gear".into()),
                    },
                    ProductSearchFilter::All(vec![
                        ProductSearchFilter::Exists("price".into()),
                        ProductSearchFilter::Compare {
                            field: "price".into(),
                            operator: ProductSearchOperator::Greater,
                            value: ProductDocValue::Integer(0),
                        },
                    ]),
                ]),
                ProductSearchFilter::In {
                    field: "category".into(),
                    values: vec![
                        ProductDocValue::String("book".into()),
                        ProductDocValue::String(category.into()),
                    ],
                },
                ProductSearchFilter::In {
                    field: "price".into(),
                    values: vec![probe_value.clone(), ProductDocValue::Integer(0)],
                },
                ProductSearchFilter::IsNull("category".into()),
                ProductSearchFilter::Not(Box::new(ProductSearchFilter::In {
                    field: "category".into(),
                    values: vec![ProductDocValue::String("misc".into())],
                })),
                ProductSearchFilter::Like {
                    field: "category".into(),
                    pattern: "g%".into(),
                },
                ProductSearchFilter::Like {
                    field: "category".into(),
                    pattern: "_oo_".into(),
                },
            ];
            for filter in filters {
                let request = ProductSearchRequest {
                    lexical: None,
                    vectors: Vec::new(),
                    filter: filter.clone(),
                    sort: Vec::new(),
                    facets: Vec::new(),
                    range_facets: Vec::new(),
                    aggregations: Vec::new(),
                    limit: 64,
                    fusion: None,
                    parent_dedupe: None,
                    rerank: None,
                    highlight: None,
                    autocut: None,
                    offset: 0,
                };
                let result = product.search_collection(binding.collection, &request, 0)?;
                let expected = reference_eligible(&model, &filter);
                assert_eq!(
                    result.eligible_documents,
                    expected.len(),
                    "eligible count diverged: seed {seed} step {step} filter {filter:?}"
                );
                assert_eq!(result.total_documents, model.len());
                let observed: std::collections::BTreeSet<u128> =
                    result.hits.iter().map(|hit| hit.object_id.get()).collect();
                assert_eq!(
                    observed, expected,
                    "hit set diverged: seed {seed} step {step} filter {filter:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn oversized_doc_values_fall_back_to_the_scan_without_diverging()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("posting-oversized");
    let (mut product, binding) = configure(&path)?;
    let mut batch = seed()?;
    // One category value too large for a bounded posting key marks the
    // field unindexed; filters touching it must fall back to the scan and
    // still answer exactly.
    let oversized = "x".repeat(4_000);
    batch.documents.push(document(
        205,
        "oversized value document",
        &oversized,
        99,
        [4.0, 0.0],
        [0.0, 4.0],
    )?);
    product.ingest_search_batch(binding.collection, &batch, 0, ProductDurability::Strict)?;

    let category_filter = ProductSearchFilter::Compare {
        field: "category".into(),
        operator: ProductSearchOperator::Equal,
        value: ProductDocValue::String("book".into()),
    };
    let request = ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: category_filter,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 16,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let result = product.search_collection(binding.collection, &request, 0)?;
    assert_eq!(result.total_documents, 5);
    assert_eq!(result.eligible_documents, 2);
    let observed: std::collections::BTreeSet<u128> =
        result.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(observed, std::collections::BTreeSet::from([201, 202]));

    // A filter on the untouched integer field keeps answering exactly too.
    let price_request = ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: ProductSearchFilter::Compare {
            field: "price".into(),
            operator: ProductSearchOperator::Greater,
            value: ProductDocValue::Integer(25),
        },
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 16,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let result = product.search_collection(binding.collection, &price_request, 0)?;
    let observed: std::collections::BTreeSet<u128> =
        result.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(observed, std::collections::BTreeSet::from([201, 204, 205]));
    Ok(())
}

#[test]
fn membership_operator_proofs_seal_at_semantics_three_and_verify_offline()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("operator-proof");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let mut session = proof_session()?;
    let context = proof_context(&session, 41);
    let operation = ProductOperation::SearchCollection {
        collection: binding.collection,
        request: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".into(),
                candidate_limit: 4,
                weight: 1,
                operator: None,
                prefix: false,
                fields: Vec::new(),
                fuzzy: None,
                phrase: false,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::In {
                field: "category".into(),
                values: vec![
                    ProductDocValue::String("book".into()),
                    ProductDocValue::String("gear".into()),
                ],
            },
            sort: Vec::new(),
            facets: Vec::new(),
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
    };
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &operation,
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 3);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);

    // A default-shaped proof keeps semantics version 2 and its exact bytes.
    let plain = ProductOperation::SearchCollection {
        collection: binding.collection,
        request: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".into(),
                candidate_limit: 4,
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
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
    };
    let context = proof_context(&session, 42);
    let (_, plain) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &plain,
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(plain.proof.content().semantics_version, 2);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn weighted_score_fusion_reorders_hybrid_results_and_binds_the_proof_method()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("weighted-fusion");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    // The vector query sits exactly on the lexically silent document: the
    // rank-based fusion compresses its advantage into one reciprocal step,
    // while the score-based blend lets the exact match dominate.
    let image = ProductVector::new([3.0, 0.0])?;
    let request = |fusion| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 4,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: image.clone(),
            candidate_limit: 4,
            weight: 2,
            execution: None,
            max_distance: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 4,
        fusion,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let rrf = product.search_collection(binding.collection, &request(None), 11)?;
    let weighted = product.search_collection(
        binding.collection,
        &request(Some(
            hyphae_native_product::ProductFusionMethod::WeightedScore,
        )),
        11,
    )?;
    let rrf_ids: Vec<u128> = rrf.hits.iter().map(|hit| hit.object_id.get()).collect();
    let weighted_ids: Vec<u128> = weighted
        .hits
        .iter()
        .map(|hit| hit.object_id.get())
        .collect();
    // Both fusions admit the same candidate set; the score-based blend
    // weights the vector branch heavily enough to change the leader.
    assert_eq!(
        rrf_ids.iter().collect::<std::collections::BTreeSet<_>>(),
        weighted_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_ne!(rrf_ids, weighted_ids);
    // The exact vector match leads under the score blend.
    assert_eq!(weighted_ids[0], 204);

    let mut session = proof_session()?;
    let context = proof_context(&session, 51);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(
                hyphae_native_product::ProductFusionMethod::WeightedScore,
            )),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 3);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn stemming_and_stop_word_analyzers_are_real_and_survive_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("analyzer-pipeline");
    let (mut product, binding) = configure_full(
        &path,
        None,
        vec![
            AnalyzerFilter::Lowercase,
            AnalyzerFilter::AsciiFolding,
            AnalyzerFilter::EnglishStopV1,
            AnalyzerFilter::EnglishStemV1,
        ],
        2,
    )?;
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            document(
                301,
                "The running dogs are chasing ponies",
                "book",
                10,
                [0.0, 0.0],
                [0.0, 0.0],
            )?,
            document(302, "café management", "book", 20, [1.0, 0.0], [0.0, 1.0])?,
            document(303, "quiet garden", "gear", 30, [2.0, 0.0], [1.0, 0.0])?,
        ],
    };
    product.ingest_search_batch(binding.collection, &batch, 11, ProductDurability::Strict)?;

    let search = |product: &NativeProduct, query: &str| {
        product
            .search_collection(
                binding.collection,
                &ProductSearchRequest {
                    lexical: Some(ProductLexicalBranch {
                        query: query.into(),
                        candidate_limit: 4,
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
                    limit: 4,
                    fusion: None,
                    parent_dedupe: None,
                    rerank: None,
                    highlight: None,
                    autocut: None,
                    offset: 0,
                },
                12,
            )
            .map(|result| {
                result
                    .hits
                    .iter()
                    .map(|hit| hit.object_id.get())
                    .collect::<Vec<_>>()
            })
            .map_err(Box::new)
    };
    // Morphological variants match through the stemmer, diacritics match
    // through the folder, and stop words carry no signal.
    assert_eq!(search(&product, "run dog")?, vec![301]);
    assert_eq!(search(&product, "chased pony")?, vec![301]);
    assert_eq!(search(&product, "cafe managing")?, vec![302]);
    assert_eq!(search(&product, "the are of")?, Vec::<u128>::new());
    drop(product);

    // The transformed terms are durable: recovery replays the raw mutation
    // text through the canonical analyzer and lands on the same postings.
    let reopened = NativeProduct::open(&path)?;
    assert_eq!(search(&reopened, "chased pony")?, vec![301]);
    assert_eq!(search(&reopened, "cafe managing")?, vec![302]);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

/// A collection whose doc-value fields are the chunk provenance columns.
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
    let doc_value_field = |id: u32,
                           field: &str,
                           logical_type: LogicalType|
     -> Result<SearchFieldDefinitionV2, Box<dyn std::error::Error>> {
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
    };
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
                vectors: Vec::new(),
            },
        )),
        ProductDurability::Strict,
    )?;
    let collection = ObjectId::new(13)?;
    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let binding = product.resolve_search_collection_binding(collection, 0)?;
    Ok((product, binding))
}

#[test]
#[allow(clippy::too_many_lines)]
fn chunked_ingest_binds_every_hit_to_exact_source_bytes() -> Result<(), Box<dyn std::error::Error>>
{
    let path = temporary("chunk-provenance");
    let (mut product, binding) = configure_chunked(&path)?;
    let source = "Hyphae proves its results. The chunker binds identity to bytes. \
                  Retrieval stays deterministic across hosts. Every chunk carries \
                  its parent and exact offsets. Proofs replay the same semantics.";
    let config = hyphae_native_product::chunker::ChunkerConfig {
        mode: hyphae_native_product::chunker::ChunkerMode::SentenceBounded {
            target: 64,
            maximum: 128,
        },
    };
    let parent_id = 777_u128;
    let documents = hyphae_native_product::chunker::chunk_documents(parent_id, source, config)
        .map_err(|error| format!("chunking failed: {error:?}"))?;
    assert!(documents.len() >= 3);
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: documents.clone(),
    };
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;

    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "deterministic retrieval".into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::Compare {
            field: "parent".into(),
            operator: ProductSearchOperator::Equal,
            value: ProductDocValue::Bytes(parent_id.to_le_bytes().to_vec()),
        },
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let result = product.search_collection(binding.collection, &request, 7)?;
    assert!(!result.hits.is_empty());
    let document_digest = hyphae_native_product::chunker::document_digest(source);
    let config_digest = config.digest();
    for hit in &result.hits {
        let ProductDocValue::Integer(byte_start) = hit.doc_values["byte_start"] else {
            return Err("byte_start doc value expected".into());
        };
        let ProductDocValue::Integer(byte_end) = hit.doc_values["byte_end"] else {
            return Err("byte_end doc value expected".into());
        };
        let ProductDocValue::Bytes(chunk_id) = &hit.doc_values["chunk_id"] else {
            return Err("chunk_id doc value expected".into());
        };
        let byte_start = usize::try_from(byte_start)?;
        let byte_end = usize::try_from(byte_end)?;
        // The retrieved chunk identity recomputes from the source digest,
        // the configuration digest, and the exact byte range: provenance.
        let expected = hyphae_native_product::chunker::chunk_identity(
            &document_digest,
            &config_digest,
            byte_start,
            byte_end,
        );
        assert_eq!(chunk_id.as_slice(), expected.as_slice());
        let matched = documents
            .iter()
            .find(|document| document.object_id == hit.object_id)
            .ok_or("hit outside the ingested chunks")?;
        assert_eq!(
            matched.text.as_bytes(),
            &source.as_bytes()[byte_start..byte_end]
        );
    }

    // The sealed proof binds the same provenance doc-values and verifies
    // offline: every retrieved chunk is provably traceable to source bytes.
    let mut session = proof_session()?;
    let context = proof_context(&session, 61);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request,
        },
        NativeProofGenerationLimits::default(),
    )?;
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn parent_dedupe_retains_first_k_per_parent_and_binds_the_proof()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("parent-dedupe");
    let (mut product, binding) = configure_chunked(&path)?;
    let config = hyphae_native_product::chunker::ChunkerConfig {
        mode: hyphae_native_product::chunker::ChunkerMode::FixedBytes {
            size: 40,
            overlap: 0,
        },
    };
    let first_parent = "shared token ".repeat(12);
    let second_parent = "shared token ".repeat(6);
    let mut documents = Vec::new();
    for (parent, source) in [
        (1_u128, first_parent.as_str()),
        (2_u128, second_parent.as_str()),
    ] {
        documents.extend(
            hyphae_native_product::chunker::chunk_documents(parent, source, config)
                .map_err(|error| format!("chunking failed: {error:?}"))?,
        );
    }
    assert!(documents.len() >= 4);
    product.ingest_search_batch(
        binding.collection,
        &ProductSearchIngestBatch {
            idempotency_id: 1,
            documents,
        },
        7,
        ProductDurability::Strict,
    )?;
    let request = |dedupe| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "shared token".into(),
            candidate_limit: 16,
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
        parent_dedupe: dedupe,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let all = product.search_collection(binding.collection, &request(None), 7)?;
    assert!(all.hits.len() >= 4);
    let deduped = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductParentDedupe {
            field: "parent".into(),
            first_k: 1,
        })),
        7,
    )?;
    assert_eq!(deduped.hits.len(), 2);
    let mut parents = std::collections::BTreeSet::new();
    for hit in &deduped.hits {
        let ProductDocValue::Bytes(parent) = &hit.doc_values["parent"] else {
            return Err("parent doc value expected".into());
        };
        parents.insert(parent.clone());
    }
    assert_eq!(parents.len(), 2);
    // The best hit overall survives deduplication in first position.
    assert_eq!(deduped.hits[0].object_id, all.hits[0].object_id);

    let mut session = proof_session()?;
    let context = proof_context(&session, 71);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(hyphae_native_product::ProductParentDedupe {
                field: "parent".into(),
                first_k: 1,
            })),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 3);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn attested_rerank_reorders_the_ranking_and_seals_the_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("attested-rerank");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let attestation = hyphae_native_product::proof::attestation::ModelAttestation::AttestedLocal {
        target: "bge-small-en-v1.5".to_owned(),
        weights_digest: [7; 32],
        input_digest: [8; 32],
        output_digest: [9; 32],
    }
    .encode()?;
    let request = |rerank| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 4,
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
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let base = product.search_collection(binding.collection, &request(None), 7)?;
    assert!(base.hits.len() >= 3);
    let last = base.hits[base.hits.len() - 1].object_id;
    // The external scores promote the weakest lexical hit to first place;
    // unscored hits keep their existing order after the scored ones.
    let stage = hyphae_native_product::ProductRerankStage {
        attestation: attestation.clone(),
        scores: vec![(last, 0.99)],
    };
    let reranked =
        product.search_collection(binding.collection, &request(Some(stage.clone())), 7)?;
    assert_eq!(reranked.hits[0].object_id, last);
    let remaining: Vec<_> = reranked.hits[1..].iter().map(|hit| hit.object_id).collect();
    let expected: Vec<_> = base
        .hits
        .iter()
        .map(|hit| hit.object_id)
        .filter(|id| *id != last)
        .collect();
    assert_eq!(remaining, expected);

    // Malformed envelopes fail closed before any execution.
    let mut tampered = stage.clone();
    tampered.attestation[0] ^= 1;
    assert!(
        product
            .search_collection(binding.collection, &request(Some(tampered)), 7)
            .is_err()
    );

    // The sealed proof carries the whole rerank section — envelope included
    // — at semantics three and re-executes the reorder offline.
    let mut session = proof_session()?;
    let context = proof_context(&session, 81);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(stage)),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 3);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn budgeted_highlighting_cuts_normalized_fragments_and_seals_at_version_four()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("highlighting");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |highlight| ProductSearchRequest {
        // Mixed case exercises the canonical normalization: fragments are
        // cut from the case-folded text the analyzer indexes.
        lexical: Some(ProductLexicalBranch {
            query: "Rust DATABASE".into(),
            candidate_limit: 4,
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
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight,
        autocut: None,
        offset: 0,
    };
    let plain = product.search_collection(binding.collection, &request(None), 7)?;
    assert!(plain.hits.iter().all(|hit| hit.fragments.is_empty()));
    let highlight = ProductHighlight {
        max_fragments: 2,
        fragment_bytes: 32,
    };
    let highlighted =
        product.search_collection(binding.collection, &request(Some(highlight)), 7)?;
    let order = |result: &hyphae_native_product::ProductSearchResult| {
        result
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>()
    };
    // Highlighting never reorders, admits, or drops hits.
    assert_eq!(order(&highlighted), order(&plain));
    let first = &highlighted.hits[0];
    assert!(!first.fragments.is_empty());
    assert!(first.fragments.len() <= 2);
    assert!(first.fragments.iter().all(|fragment| fragment.len() <= 32));
    assert!(
        first
            .fragments
            .iter()
            .any(|fragment| fragment.contains("rust") || fragment.contains("database"))
    );
    // The snapshot twin returns byte-identical fragments.
    let snapshot = product.snapshot_bounded(7)?;
    let at_snapshot = NativeProduct::search_collection_at_snapshot(
        &product,
        &snapshot,
        binding.collection,
        &request(Some(highlight)),
    )?;
    assert_eq!(at_snapshot.hits, highlighted.hits);
    // Unbounded budgets and a missing lexical branch fail closed.
    assert!(
        product
            .search_collection(
                binding.collection,
                &request(Some(ProductHighlight {
                    max_fragments: 0,
                    fragment_bytes: 32,
                })),
                7,
            )
            .is_err()
    );
    assert!(
        product
            .search_collection(
                binding.collection,
                &request(Some(ProductHighlight {
                    max_fragments: 1,
                    fragment_bytes: 8,
                })),
                7,
            )
            .is_err()
    );
    let mut lexicalless = request(Some(highlight));
    lexicalless.lexical = None;
    assert!(
        product
            .search_collection(binding.collection, &lexicalless, 7)
            .is_err()
    );
    // The sealed proof binds the highlight budget at semantics version
    // four and re-executes the highlighted request offline.
    let mut session = proof_session()?;
    let context = proof_context(&session, 83);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(highlight)),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 4);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

fn bm25_probe_batch() -> Result<ProductSearchIngestBatch, Box<dyn std::error::Error>> {
    // "rust" appears twice in a long document and once in a short one: with
    // the default length normalization the short document ranks first, with
    // b = 0 raw term frequency decides and the long document ranks first.
    Ok(ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            document(
                301,
                "rust rust alpha beta gamma delta epsilon zeta",
                "book",
                10,
                [0.0, 0.0],
                [0.0, 0.0],
            )?,
            document(302, "rust", "book", 20, [1.0, 0.0], [0.0, 1.0])?,
            document(303, "alpha beta", "gear", 30, [2.0, 0.0], [1.0, 0.0])?,
            document(304, "alpha beta", "gear", 40, [3.0, 0.0], [1.0, 1.0])?,
        ],
    })
}

fn lexical_ranking(
    product: &NativeProduct,
    binding: &ProductSearchCollectionBinding,
) -> Result<Vec<u128>, Box<dyn std::error::Error>> {
    let result = product.search_collection(
        binding.collection,
        &ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".into(),
                candidate_limit: 4,
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
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        11,
    )?;
    Ok(result.hits.iter().map(|hit| hit.object_id.get()).collect())
}

#[test]
fn tuned_bm25_parameters_change_the_ranking_and_survive_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let default_path = temporary("bm25-default");
    let (mut product, binding) = configure(&default_path)?;
    product.ingest_search_batch(
        binding.collection,
        &bm25_probe_batch()?,
        11,
        ProductDurability::Strict,
    )?;
    assert_eq!(lexical_ranking(&product, &binding)?, vec![302, 301]);
    drop(product);
    fs::remove_dir_all(&default_path)?;

    let tuned_path = temporary("bm25-tuned");
    let (mut product, binding) = configure_with_bm25(
        &tuned_path,
        Some(Bm25Parameters {
            k1_micros: 1_200_000,
            b_micros: 0,
        }),
    )?;
    product.ingest_search_batch(
        binding.collection,
        &bm25_probe_batch()?,
        11,
        ProductDurability::Strict,
    )?;
    assert_eq!(lexical_ranking(&product, &binding)?, vec![301, 302]);
    drop(product);

    // The tuned parameters live in the catalog representation and must
    // decode identically after reopening the directory.
    let reopened = NativeProduct::open(&tuned_path)?;
    let binding = reopened.resolve_search_collection_binding(ObjectId::new(13)?, 0)?;
    assert_eq!(lexical_ranking(&reopened, &binding)?, vec![301, 302]);
    drop(reopened);
    fs::remove_dir_all(tuned_path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_transaction_stages_a_complete_document_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("atomic-document");
    let (mut product, binding) = configure(&path)?;
    let mut session = proof_session()?;
    let c1 = proof_context(&session, 1);
    let begin = product.dispatch(&mut session, &c1, ProductOperation::TransactionBegin)?;
    let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
        hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
    ) = begin
    else {
        return Err("transaction did not begin".into());
    };
    let initial_document = document(
        900,
        "atomic staged first image",
        "book",
        11,
        [0.0, 0.0],
        [0.0, 0.0],
    )?;
    let c2 = proof_context(&session, 2);
    let staged = product.dispatch(
        &mut session,
        &c2,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: initial_document.clone(),
            },
        },
    )?;
    assert!(matches!(
        staged,
        ProductResponse::TransactionStaged(ref receipt)
            if receipt.changed && receipt.result == ProductTransactionStageResult::Search
    ));
    let c3 = proof_context(&session, 3);
    let committed = product.dispatch(
        &mut session,
        &c3,
        ProductOperation::TransactionCommit { handle },
    )?;
    let hyphae_native_product::ProductResponse::TransactionCommitted(receipt) = committed else {
        return Err("transaction did not commit".into());
    };
    assert_eq!(receipt.staged_operations, 1);
    let result = product.search_collection(
        binding.collection,
        &ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "atomic staged first".to_owned(),
                candidate_limit: 10,
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
            limit: 5,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        0,
    )?;
    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![900]
    );
    assert_eq!(
        result.hits[0].doc_values["category"],
        ProductDocValue::String("book".to_owned())
    );
    assert_eq!(
        result.hits[0].doc_values["price"],
        ProductDocValue::Integer(11)
    );
    assert_eq!(
        product
            .search_collection(
                binding.collection,
                &vector_request("image", [0.0, 0.0], None, Some(0.0))?,
                0,
            )?
            .hits[0]
            .object_id,
        initial_document.object_id
    );
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(0)?,
            binding.collection,
            None,
            1,
        )?
        .documents,
        [initial_document]
    );
    let c4 = proof_context(&session, 4);
    let begin = product.dispatch(&mut session, &c4, ProductOperation::TransactionBegin)?;
    let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
        hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
    ) = begin
    else {
        return Err("replacement transaction did not begin".into());
    };
    let mut replacement = document(
        900,
        "atomic staged replacement image",
        "gear",
        22,
        [2.0, 0.0],
        [2.0, 0.0],
    )?;
    replacement.vectors.remove("semantic");
    let c5 = proof_context(&session, 5);
    let staged = product.dispatch(
        &mut session,
        &c5,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: replacement.clone(),
            },
        },
    )?;
    assert!(matches!(
        staged,
        ProductResponse::TransactionStaged(ref receipt)
            if receipt.changed && receipt.result == ProductTransactionStageResult::Search
    ));
    let c6 = proof_context(&session, 6);
    let committed = product.dispatch(
        &mut session,
        &c6,
        ProductOperation::TransactionCommit { handle },
    )?;
    assert!(matches!(
        committed,
        ProductResponse::TransactionCommitted(ref receipt) if receipt.staged_operations == 1
    ));
    assert!(
        product
            .search_collection(binding.collection, &lexical_request("first"), 0)?
            .hits
            .is_empty()
    );
    let replacement_result =
        product.search_collection(binding.collection, &lexical_request("replacement"), 0)?;
    assert_eq!(replacement_result.hits.len(), 1);
    assert_eq!(replacement_result.hits[0].object_id, replacement.object_id);
    assert_eq!(
        replacement_result.hits[0].doc_values["category"],
        ProductDocValue::String("gear".to_owned())
    );
    assert_eq!(
        replacement_result.hits[0].doc_values["price"],
        ProductDocValue::Integer(22)
    );
    assert_eq!(
        product
            .search_collection(
                binding.collection,
                &vector_request("image", [2.0, 0.0], None, Some(0.0))?,
                0,
            )?
            .hits[0]
            .object_id,
        replacement.object_id
    );
    assert!(
        product
            .search_collection(
                binding.collection,
                &vector_request("semantic", [0.0, 0.0], None, Some(0.0))?,
                0,
            )?
            .hits
            .is_empty()
    );
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(0)?,
            binding.collection,
            None,
            1,
        )?
        .documents,
        [replacement.clone()]
    );
    drop(product);
    let reopened = NativeProduct::open(&path)?;
    reopened.resolve_search_collection_binding(binding.collection, 0)?;
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(
            &reopened.snapshot_bounded(0)?,
            binding.collection,
            None,
            1,
        )?
        .documents,
        [replacement]
    );
    assert!(
        reopened
            .search_collection(binding.collection, &lexical_request("first"), 0)?
            .hits
            .is_empty()
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn transaction_started_before_provisioning_never_observes_the_new_binding_or_manifest()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("transaction-provisioning-snapshot");
    let mut product = configure_catalog(&path)?;
    let collection = ObjectId::new(13)?;
    let mut session = proof_session()?;
    let c1 = proof_context(&session, 1);
    let begin = product.dispatch(&mut session, &c1, ProductOperation::TransactionBegin)?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        read_csn,
        staged_operations: 0,
        ..
    }) = begin
    else {
        return Err("transaction did not begin".into());
    };

    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let document = ProductDocument {
        object_id: ObjectId::new(901)?,
        text: "provisioned after begin".to_owned(),
        doc_values: BTreeMap::new(),
        vectors: BTreeMap::new(),
    };
    let c2 = proof_context(&session, 2);
    let error = product
        .dispatch(
            &mut session,
            &c2,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection,
                    document: document.clone(),
                },
            },
        )
        .expect_err("transaction observed a binding provisioned after its snapshot");
    assert_eq!(error.code(), ProductErrorCode::ObjectNotFound);
    assert_eq!(error.object_id(), Some(collection));
    assert_eq!(error.retry(), ProductRetry::Never);
    let c3 = proof_context(&session, 3);
    assert!(matches!(
        product.dispatch(
            &mut session,
            &c3,
            ProductOperation::ExplicitTransactionStatus { handle },
        )?,
        ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
            read_csn: current_read_csn,
            staged_operations: 0,
            ..
        }) if current_read_csn == read_csn
    ));

    let c4 = proof_context(&session, 4);
    let empty_commit = product
        .dispatch(
            &mut session,
            &c4,
            ProductOperation::TransactionCommit { handle },
        )
        .expect_err("failed stage became committable");
    assert_eq!(empty_commit.code(), ProductErrorCode::InvalidRequest);
    let c5 = proof_context(&session, 5);
    assert!(matches!(
        product.dispatch(
            &mut session,
            &c5,
            ProductOperation::TransactionRollback { handle },
        )?,
        ProductResponse::TransactionRolledBack(receipt) if receipt.discarded_operations == 0
    ));

    let c6 = proof_context(&session, 6);
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = product.dispatch(&mut session, &c6, ProductOperation::TransactionBegin)?
    else {
        return Err("post-provisioning transaction did not begin".into());
    };
    let c7 = proof_context(&session, 7);
    product.dispatch(
        &mut session,
        &c7,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: ProductTransactionSearchMutation::Document {
                collection,
                document,
            },
        },
    )?;
    let c8 = proof_context(&session, 8);
    let ProductResponse::TransactionCommitted(committed) = product.dispatch(
        &mut session,
        &c8,
        ProductOperation::TransactionCommit { handle },
    )?
    else {
        return Err("post-provisioning transaction did not commit".into());
    };
    assert_eq!(committed.staged_operations, 1);

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn transaction_document_stage_stays_on_its_catalog_epoch_across_a_disjoint_catalog_commit()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("transaction-catalog-epoch");
    let (mut product, binding) = configure(&path)?;
    let mut session = proof_session()?;
    let c1 = proof_context(&session, 1);
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        read_csn,
        ..
    }) = product.dispatch(&mut session, &c1, ProductOperation::TransactionBegin)?
    else {
        return Err("transaction did not begin".into());
    };
    product
        .create_catalog_object_v2(
            LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
                header: header(10_000, EngineKind::Search, "concurrent", Some(11))?,
                tokenizer: AnalyzerTokenizer::UnicodeWord,
                filters: vec![AnalyzerFilter::Lowercase, AnalyzerFilter::AsciiFolding],
            })),
            ProductDurability::Strict,
        )
        .map_err(|error| format!("concurrent catalog commit failed: {error:?}"))?;
    let latest_csn = product
        .snapshot_bounded(0)?
        .identity()
        .visible_csn
        .map(hyphae_native_types::Csn::get);
    assert!(latest_csn > read_csn);

    let c2 = proof_context(&session, 2);
    let staged = product
        .dispatch(
            &mut session,
            &c2,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: binding.collection,
                    document: ProductDocument {
                        object_id: ObjectId::new(902)?,
                        text: "catalog epoch".to_owned(),
                        doc_values: BTreeMap::new(),
                        vectors: BTreeMap::new(),
                    },
                },
            },
        )
        .map_err(|error| format!("snapshot-bound document stage failed: {error:?}"))?;
    assert!(matches!(
        staged,
        ProductResponse::TransactionStaged(ref receipt)
            if receipt.operation_ordinal == 1 && receipt.changed
    ));
    let c3 = proof_context(&session, 3);
    let ProductResponse::TransactionCommitted(committed) = product
        .dispatch(
            &mut session,
            &c3,
            ProductOperation::TransactionCommit { handle },
        )
        .map_err(|error| format!("snapshot-bound document commit failed: {error:?}"))?
    else {
        return Err("transaction did not commit across disjoint catalog change".into());
    };
    assert_eq!(committed.staged_operations, 1);
    let catalog = product.catalog_snapshot()?;
    assert!(matches!(
        product.catalog_describe(&catalog, ObjectId::new(10_000)?)?,
        Some(LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(_)))
    ));
    let snapshot = product.snapshot_bounded(0)?;
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(&snapshot, binding.collection, None, 5)?
            .documents
            .iter()
            .map(|document| document.object_id.get())
            .collect::<Vec<_>>(),
        vec![902]
    );

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn explicit_transaction_document_stage_rejects_unknown_collection_and_bad_doc_values()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("atomic-document-invalid");
    let (mut product, binding) = configure(&path)?;
    let mut session = proof_session()?;
    let c1 = proof_context(&session, 1);
    let begin = product.dispatch(&mut session, &c1, ProductOperation::TransactionBegin)?;
    let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
        hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
    ) = begin
    else {
        return Err("transaction did not begin".into());
    };
    let c2 = proof_context(&session, 2);
    let unknown = product.dispatch(
        &mut session,
        &c2,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: ObjectId::new(999)?,
                document: ProductDocument {
                    object_id: ObjectId::new(1)?,
                    text: "x".to_owned(),
                    doc_values: BTreeMap::new(),
                    vectors: BTreeMap::new(),
                },
            },
        },
    );
    assert!(unknown.is_err());
    let mut invalid = BTreeMap::new();
    invalid.insert(
        "missing_field".to_owned(),
        ProductDocValue::String("nope".to_owned()),
    );
    let c3 = proof_context(&session, 3);
    let bad_doc = product.dispatch(
        &mut session,
        &c3,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: ProductDocument {
                    object_id: ObjectId::new(2)?,
                    text: "y".to_owned(),
                    doc_values: invalid,
                    vectors: BTreeMap::new(),
                },
            },
        },
    );
    assert!(bad_doc.is_err());
    let c4 = proof_context(&session, 4);
    product.dispatch(
        &mut session,
        &c4,
        ProductOperation::TransactionRollback { handle },
    )?;
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn relative_score_fusion_normalizes_each_branch_over_its_own_range()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("relative-fusion");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let image = ProductVector::new([3.0, 0.0])?;
    let request = |fusion| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 4,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: image.clone(),
            candidate_limit: 4,
            weight: 1,
            execution: None,
            max_distance: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 4,
        fusion,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let relative = product.search_collection(
        binding.collection,
        &request(Some(
            hyphae_native_product::ProductFusionMethod::RelativeScore,
        )),
        11,
    )?;
    // Per-branch min-max normalization rewards candidates that are good
    // in BOTH branches. Document 201 is the best lexical hit (norm 1.0)
    // but the farthest vector hit (norm 0.0); document 204 sits exactly
    // on the vector query (norm 1.0) but is lexically silent (no
    // contribution). Document 203 ("database hardware", vector [2,0]) is
    // strong in both: vector norm (9-1)/9 ~ 0.889 plus a positive
    // lexical share pushes its fused score above either single-branch
    // extreme, so the balanced candidate leads.
    let ids: Vec<u128> = relative
        .hits
        .iter()
        .map(|hit| hit.object_id.get())
        .collect();
    assert_eq!(ids.first().copied(), Some(203));
    assert!(ids.contains(&201));
    assert!(ids.contains(&204));

    // The same request under RRF produces a valid ranking too; the two
    // methods must both admit the identical candidate set.
    let rrf = product.search_collection(binding.collection, &request(None), 11)?;
    assert_eq!(
        rrf.hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<std::collections::BTreeSet<_>>(),
        ids.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
    );

    // The proof pipeline binds the new fusion method.
    let mut session = proof_session()?;
    let context = proof_context(&session, 52);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(
                hyphae_native_product::ProductFusionMethod::RelativeScore,
            )),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 3);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn autocut_truncates_at_the_first_steep_quality_drop() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("autocut");
    let (mut product, binding) = configure(&path)?;
    // Two documents strongly about "rust database", two only weakly
    // related: the fused score curve has a steep knee after the leaders.
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            document(
                401,
                "rust database engine rust database core",
                "book",
                30,
                [0.0, 0.0],
                [0.0, 0.0],
            )?,
            document(
                402,
                "rust database handbook rust database",
                "book",
                10,
                [1.0, 0.0],
                [0.0, 1.0],
            )?,
            document(403, "garden rust", "gear", 20, [2.0, 0.0], [1.0, 0.0])?,
            document(
                404,
                "green garden database",
                "gear",
                40,
                [3.0, 0.0],
                [1.0, 1.0],
            )?,
        ],
    };
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;
    let request = |autocut| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 4,
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
        limit: 4,
        fusion: Some(hyphae_native_product::ProductFusionMethod::RelativeScore),
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut,
        offset: 0,
    };
    let full = product.search_collection(binding.collection, &request(None), 7)?;
    let cut = product.search_collection(binding.collection, &request(Some(1)), 7)?;
    // Without autocut the weak tail is present; with steepness 1 the
    // ranking stops at the knee after the strong leaders.
    assert!(full.hits.len() > cut.hits.len());
    assert!(!cut.hits.is_empty());
    let cut_ids: Vec<u128> = cut.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert!(cut_ids.iter().all(|id| *id == 401 || *id == 402));
    // The cut ranking is a strict prefix of the full ranking.
    let full_ids: Vec<u128> = full.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(&full_ids[..cut_ids.len()], cut_ids.as_slice());

    // Zero and oversized steepness fail closed.
    for steepness in [0, hyphae_native_product::MAX_AUTOCUT_STEEPNESS + 1] {
        let error = product.search_collection(binding.collection, &request(Some(steepness)), 7);
        let Err(error) = error else {
            return Err("invalid autocut was admitted".into());
        };
        assert_eq!(
            error.code(),
            hyphae_native_product::ProductErrorCode::InvalidRequest
        );
    }

    // The proof pipeline binds the stage at semantics version 5.
    let mut session = proof_session()?;
    let context = proof_context(&session, 53);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::SearchCollection {
            collection: binding.collection,
            request: request(Some(1)),
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(artifact.proof.content().semantics_version, 5);
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn float_doc_values_filter_sort_facet_and_aggregate() -> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::CanonicalF64;

    let path = temporary("float-doc-values");
    let (mut product, binding) = configure(&path)?;
    let rated = |id: u128,
                 text: &str,
                 rating: f64|
     -> Result<ProductDocument, Box<dyn std::error::Error>> {
        let mut document = document(id, text, "book", 10, [0.0, 0.0], [0.0, 0.0])?;
        document.doc_values.insert(
            "rating".into(),
            ProductDocValue::Float(CanonicalF64::new(rating)),
        );
        Ok(document)
    };
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            rated(501, "rust database", 4.5)?,
            rated(502, "rust database", 2.5)?,
            rated(503, "rust database", 4.5)?,
            rated(504, "rust database", -0.0)?,
        ],
    };
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;

    // Range filter over floats (>= 4.0), sorted ascending by rating.
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::Compare {
            field: "rating".into(),
            operator: hyphae_native_product::ProductSearchOperator::GreaterOrEqual,
            value: ProductDocValue::Float(CanonicalF64::new(4.0)),
        },
        sort: vec![hyphae_native_product::ProductSearchSort {
            source: hyphae_native_product::ProductSortSource::Field("rating".into()),
            direction: hyphae_native_product::ProductSortDirection::Ascending,
            missing: hyphae_native_product::ProductMissingPlacement::Last,
        }],
        facets: vec![hyphae_native_product::ProductFacetRequest {
            field: "rating".into(),
            limit: 8,
        }],
        range_facets: Vec::new(),
        aggregations: vec![hyphae_native_product::ProductNamedAggregation {
            name: "total".into(),
            aggregation: hyphae_native_product::ProductAggregation::Sum("rating".into()),
        }],
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let result = product.search_collection(binding.collection, &request, 7)?;
    let ids: Vec<u128> = result.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(ids, vec![501, 503]);
    // Facet buckets carry the exact canonical float values.
    let facet = result.facets.first().ok_or("missing facet")?;
    assert!(facet.buckets.iter().any(|bucket| {
        bucket.value == ProductDocValue::Float(CanonicalF64::new(4.5)) && bucket.count == 2
    }));
    // Sum over the filtered set is a finite float aggregate.
    let aggregation = result.aggregations.first().ok_or("missing aggregation")?;
    assert_eq!(
        aggregation.value,
        hyphae_native_product::ProductAggregationValue::Float(Some(CanonicalF64::new(9.0))),
    );

    // Signed zero collapses to canonical +0 and equality matches it.
    let zero_request = ProductSearchRequest {
        filter: ProductSearchFilter::Compare {
            field: "rating".into(),
            operator: hyphae_native_product::ProductSearchOperator::Equal,
            value: ProductDocValue::Float(CanonicalF64::new(0.0)),
        },
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        ..request.clone()
    };
    let zeros = product.search_collection(binding.collection, &zero_request, 7)?;
    assert_eq!(
        zeros
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![504],
    );

    // Mixed-type sum (integer price + float rating on the same field
    // name is impossible here, so force it: sum over "price" stays the
    // integer path).
    let integer_sum = ProductSearchRequest {
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: vec![hyphae_native_product::ProductNamedAggregation {
            name: "prices".into(),
            aggregation: hyphae_native_product::ProductAggregation::Sum("price".into()),
        }],
        ..request.clone()
    };
    let sums = product.search_collection(binding.collection, &integer_sum, 7)?;
    assert_eq!(
        sums.aggregations
            .first()
            .map(|aggregation| &aggregation.value),
        Some(&hyphae_native_product::ProductAggregationValue::Integer(
            Some(40)
        )),
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn offset_pages_the_final_ranking_without_touching_aggregates()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("offset-paging");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |offset, limit| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 4,
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
        aggregations: vec![hyphae_native_product::ProductNamedAggregation {
            name: "count".into(),
            aggregation: hyphae_native_product::ProductAggregation::Count,
        }],
        limit,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset,
    };
    let full = product.search_collection(binding.collection, &request(0, 4), 7)?;
    let paged = product.search_collection(binding.collection, &request(1, 2), 7)?;
    let full_ids: Vec<u128> = full.hits.iter().map(|hit| hit.object_id.get()).collect();
    let paged_ids: Vec<u128> = paged.hits.iter().map(|hit| hit.object_id.get()).collect();
    // The page is the exact middle window of the full ranking.
    assert!(full_ids.len() >= 3);
    assert_eq!(paged_ids, full_ids[1..3].to_vec());
    // Aggregates describe the complete filtered set, not the window.
    assert_eq!(
        paged
            .aggregations
            .first()
            .map(|aggregation| &aggregation.value),
        full.aggregations
            .first()
            .map(|aggregation| &aggregation.value),
    );
    // Past-the-end offsets return empty pages, never errors.
    let empty = product.search_collection(binding.collection, &request(full_ids.len(), 2), 7)?;
    assert!(empty.hits.is_empty());
    // offset + limit above the bounded ceiling fails closed.
    let error = product.search_collection(
        binding.collection,
        &request(hyphae_native_product::MAX_PRODUCT_SEARCH_HITS, 1),
        7,
    );
    let Err(error) = error else {
        return Err("unbounded offset was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::LimitExceeded
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn average_aggregation_is_a_canonical_float_over_present_values()
-> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::CanonicalF64;

    let path = temporary("average-aggregation");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |field: &str| ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: vec![hyphae_native_product::ProductNamedAggregation {
            name: "mean".into(),
            aggregation: hyphae_native_product::ProductAggregation::Average(field.into()),
        }],
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // Integer field: (30 + 10 + 20 + 40) / 4 = 25.0 as a canonical float.
    let result = product.search_collection(binding.collection, &request("price"), 7)?;
    assert_eq!(
        result
            .aggregations
            .first()
            .map(|aggregation| &aggregation.value),
        Some(&hyphae_native_product::ProductAggregationValue::Float(
            Some(CanonicalF64::new(25.0))
        )),
    );
    // A field with no present values yields an absent float aggregate.
    let absent = product.search_collection(binding.collection, &request("missing"), 7)?;
    assert_eq!(
        absent
            .aggregations
            .first()
            .map(|aggregation| &aggregation.value),
        Some(&hyphae_native_product::ProductAggregationValue::Float(None)),
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn range_facets_bucket_numeric_values_in_declared_order() -> Result<(), Box<dyn std::error::Error>>
{
    use hyphae_native_product::CanonicalF64;

    let path = temporary("range-facets");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    // Prices: 30, 10, 20, 40. Declared ranges: (-inf,15), [15,35), [35,+inf),
    // plus an overlapping [0,+inf) that must count independently.
    let request = ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: vec![hyphae_native_product::ProductRangeFacetRequest {
            field: "price".into(),
            ranges: vec![
                hyphae_native_product::ProductFacetRange {
                    lower: None,
                    upper: Some(CanonicalF64::new(15.0)),
                },
                hyphae_native_product::ProductFacetRange {
                    lower: Some(CanonicalF64::new(15.0)),
                    upper: Some(CanonicalF64::new(35.0)),
                },
                hyphae_native_product::ProductFacetRange {
                    lower: Some(CanonicalF64::new(35.0)),
                    upper: None,
                },
                hyphae_native_product::ProductFacetRange {
                    lower: Some(CanonicalF64::new(0.0)),
                    upper: None,
                },
            ],
        }],
        aggregations: Vec::new(),
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let result = product.search_collection(binding.collection, &request, 7)?;
    let facet = result.range_facets.first().ok_or("missing range facet")?;
    assert_eq!(facet.field, "price");
    let counts: Vec<u64> = facet.buckets.iter().map(|bucket| bucket.count).collect();
    assert_eq!(counts, vec![1, 2, 1, 4]);
    // Bucket values are the declared range ordinals, in request order.
    let ordinals: Vec<_> = facet
        .buckets
        .iter()
        .map(|bucket| bucket.value.clone())
        .collect();
    assert_eq!(
        ordinals,
        (0..4).map(ProductDocValue::Integer).collect::<Vec<_>>(),
    );

    // An inverted range fails closed.
    let inverted = ProductSearchRequest {
        range_facets: vec![hyphae_native_product::ProductRangeFacetRequest {
            field: "price".into(),
            ranges: vec![hyphae_native_product::ProductFacetRange {
                lower: Some(CanonicalF64::new(35.0)),
                upper: Some(CanonicalF64::new(15.0)),
            }],
        }],
        ..request.clone()
    };
    let error = product.search_collection(binding.collection, &inverted, 7);
    let Err(error) = error else {
        return Err("inverted range was admitted".into());
    };
    // The runtime validator rejects malformed range shapes as a shape
    // limit, which the product maps to limit-exceeded.
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::LimitExceeded
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn vector_distance_cutoff_discards_far_hits_before_fusion() -> Result<(), Box<dyn std::error::Error>>
{
    use hyphae_native_product::CanonicalF64;

    let path = temporary("distance-cutoff");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    // Image vectors sit at x = 0,1,2,3; the query at the origin. Distances
    // (squared L2) are 0, 1, 4, 9.
    let origin = ProductVector::new([0.0, 0.0])?;
    let request = |max_distance| ProductSearchRequest {
        lexical: None,
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: origin.clone(),
            candidate_limit: 4,
            weight: 1,
            execution: None,
            max_distance,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 4,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    let full = product.search_collection(binding.collection, &request(None), 7)?;
    assert_eq!(full.hits.len(), 4);
    let cut = product.search_collection(
        binding.collection,
        &request(Some(CanonicalF64::new(4.0))),
        7,
    )?;
    // Distances 0, 1, 4 stay (inclusive cutoff); 9 is discarded.
    let ids: Vec<u128> = cut.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(ids.len(), 3);
    assert!(!ids.contains(&204));
    // A negative cutoff fails closed.
    let error = product.search_collection(
        binding.collection,
        &request(Some(CanonicalF64::new(-1.0))),
        7,
    );
    let Err(error) = error else {
        return Err("negative cutoff was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn lexical_operator_and_requires_every_term_and_or_counts_minimum()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("lexical-operator");
    let (mut product, binding) = configure(&path)?;
    // 201 "rust database engine" has both; 202 "rust field guide" only
    // rust; 203 "database hardware" only database; 204 neither.
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |operator| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 8,
            weight: 1,
            operator,
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
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // Default OR admits all three matching documents.
    let any = product.search_collection(binding.collection, &request(None), 7)?;
    assert_eq!(any.hits.len(), 3);
    // AND admits only the document containing every analyzed term.
    let all = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductLexicalOperator::And)),
        7,
    )?;
    assert_eq!(
        all.hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![201],
    );
    // OR with minimum_match 2 equals AND here.
    let minimum = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductLexicalOperator::Or {
            minimum_match: 2,
        })),
        7,
    )?;
    assert_eq!(
        minimum
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![201],
    );
    // minimum_match 1 restores OR behavior.
    let one = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductLexicalOperator::Or {
            minimum_match: 1,
        })),
        7,
    )?;
    assert_eq!(one.hits.len(), 3);
    // minimum_match above the distinct-term count admits nothing.
    let unsatisfiable = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductLexicalOperator::Or {
            minimum_match: 3,
        })),
        7,
    )?;
    assert!(unsatisfiable.hits.is_empty());
    // Zero minimum_match fails closed.
    let error = product.search_collection(
        binding.collection,
        &request(Some(hyphae_native_product::ProductLexicalOperator::Or {
            minimum_match: 0,
        })),
        7,
    );
    let Err(error) = error else {
        return Err("zero minimum_match was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn lexical_prefix_expands_the_final_term_and_scores_bm25() -> Result<(), Box<dyn std::error::Error>>
{
    let path = temporary("lexical-prefix");
    let (mut product, binding) = configure(&path)?;
    // Terms: database, hardware, rust, garden, tools, engine, field, guide.
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |query: &str, prefix| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: query.into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix,
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
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // "gar" matches nothing exactly, but expands to "garden".
    let exact = product.search_collection(binding.collection, &request("gar", false), 7)?;
    assert!(exact.hits.is_empty());
    let expanded = product.search_collection(binding.collection, &request("gar", true), 7)?;
    assert_eq!(
        expanded
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![204],
    );
    // Earlier terms stay exact: "rust gu" expands only the final term.
    let mixed = product.search_collection(binding.collection, &request("rust gu", true), 7)?;
    let ids: Vec<u128> = mixed.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert!(ids.contains(&202));
    // A prefix with no expansion leaves the branch empty.
    let none = product.search_collection(binding.collection, &request("zzz", true), 7)?;
    assert!(none.hits.is_empty());
    // Prefix and operator together fail closed.
    let mut conflicted = request("rust", true);
    if let Some(lexical) = conflicted.lexical.as_mut() {
        lexical.operator = Some(hyphae_native_product::ProductLexicalOperator::And);
    }
    let error = product.search_collection(binding.collection, &conflicted, 7);
    let Err(error) = error else {
        return Err("prefix+operator was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn field_boosts_run_bm25f_over_body_and_doc_values() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("field-boosts");
    let (mut product, binding) = configure(&path)?;
    // Body vs category: doc 301 says "rust" only in category; doc 302
    // says it only in body.
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            {
                let mut document =
                    document(301, "database engine", "rust", 30, [0.0, 0.0], [0.0, 0.0])?;
                document
                    .doc_values
                    .insert("category".into(), ProductDocValue::String("rust".into()));
                document
            },
            document(302, "rust handbook", "book", 10, [1.0, 0.0], [0.0, 1.0])?,
        ],
    };
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;
    let request = |fields| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust".into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix: false,
            fields,
            fuzzy: None,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // Single-field BM25 sees only the body: 302 alone matches.
    let plain = product.search_collection(binding.collection, &request(Vec::new()), 7)?;
    assert_eq!(
        plain
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![302],
    );
    // Boosting category heavily surfaces 301 first; body keeps 302 present.
    let boosted = product.search_collection(
        binding.collection,
        &request(vec![
            hyphae_native_product::ProductLexicalFieldBoost {
                field: "category".into(),
                weight_micros: 5_000_000,
            },
            hyphae_native_product::ProductLexicalFieldBoost {
                field: "body".into(),
                weight_micros: 1_000_000,
            },
        ]),
        7,
    )?;
    let ids: Vec<u128> = boosted.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert_eq!(ids.first().copied(), Some(301));
    assert!(ids.contains(&302));
    // Unknown field names fail closed.
    let error = product.search_collection(
        binding.collection,
        &request(vec![hyphae_native_product::ProductLexicalFieldBoost {
            field: "missing".into(),
            weight_micros: 1_000_000,
        }]),
        7,
    );
    let Err(error) = error else {
        return Err("unknown boost field was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    // Boosts exclude the operator and prefix.
    let mut conflicted = request(vec![hyphae_native_product::ProductLexicalFieldBoost {
        field: "body".into(),
        weight_micros: 1_000_000,
    }]);
    if let Some(lexical) = conflicted.lexical.as_mut() {
        lexical.prefix = true;
    }
    let error = product.search_collection(binding.collection, &conflicted, 7);
    let Err(error) = error else {
        return Err("boosts+prefix was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn fuzzy_expansion_matches_typo_distance_terms() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("fuzzy-expansion");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |query: &str, fuzzy| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: query.into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // "datbase" is one deletion from "database": exact match finds
    // nothing, fuzzy(1) recovers both database documents.
    let exact = product.search_collection(binding.collection, &request("datbase", None), 7)?;
    assert!(exact.hits.is_empty());
    let fuzzy = product.search_collection(binding.collection, &request("datbase", Some(1)), 7)?;
    let ids: Vec<u128> = fuzzy.hits.iter().map(|hit| hit.object_id.get()).collect();
    assert!(ids.contains(&201));
    assert!(ids.contains(&203));
    // Distance zero and above the bound fail closed.
    for distance in [0, 3] {
        let error =
            product.search_collection(binding.collection, &request("datbase", Some(distance)), 7);
        let Err(error) = error else {
            return Err("invalid fuzzy distance was admitted".into());
        };
        assert_eq!(
            error.code(),
            hyphae_native_product::ProductErrorCode::InvalidRequest
        );
    }
    // Fuzzy excludes prefix.
    let mut conflicted = request("datbase", Some(1));
    if let Some(lexical) = conflicted.lexical.as_mut() {
        lexical.prefix = true;
    }
    let error = product.search_collection(binding.collection, &conflicted, 7);
    let Err(error) = error else {
        return Err("fuzzy+prefix was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn highlighting_covers_expanded_prefix_and_fuzzy_terms() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("highlight-expansion");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let request = |query: &str, prefix, fuzzy| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: query.into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix,
            fields: Vec::new(),
            fuzzy,
            phrase: false,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: Some(hyphae_native_product::ProductHighlight {
            max_fragments: 2,
            fragment_bytes: 32,
        }),
        autocut: None,
        offset: 0,
    };
    // Prefix "gar" -> garden: the fragment must contain the expanded term.
    let prefixed = product.search_collection(binding.collection, &request("gar", true, None), 7)?;
    let hit = prefixed.hits.first().ok_or("missing prefix hit")?;
    assert!(
        hit.fragments
            .iter()
            .any(|fragment| fragment.contains("garden")),
        "fragments {:?}",
        hit.fragments
    );
    // Fuzzy "datbase" -> database: same guarantee.
    let fuzzy =
        product.search_collection(binding.collection, &request("datbase", false, Some(1)), 7)?;
    let hit = fuzzy.hits.first().ok_or("missing fuzzy hit")?;
    assert!(
        hit.fragments
            .iter()
            .any(|fragment| fragment.contains("database")),
        "fragments {:?}",
        hit.fragments
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn phrase_matching_requires_consecutive_positions() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("phrase-matching");
    let (mut product, binding) = configure(&path)?;
    // 401 has "rust database" adjacent; 402 has both terms separated.
    let batch = ProductSearchIngestBatch {
        idempotency_id: 1,
        documents: vec![
            document(
                401,
                "the rust database engine",
                "book",
                30,
                [0.0, 0.0],
                [0.0, 0.0],
            )?,
            document(
                402,
                "rust is a great database companion",
                "book",
                10,
                [1.0, 0.0],
                [0.0, 1.0],
            )?,
        ],
    };
    product.ingest_search_batch(binding.collection, &batch, 7, ProductDurability::Strict)?;
    let request = |phrase| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "rust database".into(),
            candidate_limit: 8,
            weight: 1,
            operator: None,
            prefix: false,
            fields: Vec::new(),
            fuzzy: None,
            phrase,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    };
    // Ordinary match admits both; the phrase admits only the adjacent one.
    let any = product.search_collection(binding.collection, &request(false), 7)?;
    assert_eq!(any.hits.len(), 2);
    let phrase = product.search_collection(binding.collection, &request(true), 7)?;
    assert_eq!(
        phrase
            .hits
            .iter()
            .map(|hit| hit.object_id.get())
            .collect::<Vec<_>>(),
        vec![401],
    );
    // A single-term phrase degrades to an ordinary match.
    let mut single = request(true);
    if let Some(lexical) = single.lexical.as_mut() {
        lexical.query = "rust".into();
    }
    let single = product.search_collection(binding.collection, &single, 7)?;
    assert_eq!(single.hits.len(), 2);
    // Phrase excludes fuzzy.
    let mut conflicted = request(true);
    if let Some(lexical) = conflicted.lexical.as_mut() {
        lexical.fuzzy = Some(1);
    }
    let error = product.search_collection(binding.collection, &conflicted, 7);
    let Err(error) = error else {
        return Err("phrase+fuzzy was admitted".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::InvalidRequest
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn memory_proof_seals_lifecycle_and_applies_expiry_before_limit()
-> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::proof::{CanonicalBytes, NativeVerificationScope};
    use hyphae_native_product::{
        ProductMemoryRecallRequest, ProductResponse, memory_lifecycle_key,
    };
    let path = temporary("memory-lifecycle-proof");
    let (mut product, binding) = configure(&path)?;
    product.ingest_search_batch(binding.collection, &seed()?, 7, ProductDurability::Strict)?;
    let mut session = proof_session()?;
    for (id, value, expires) in [
        (201, b"expired".as_slice(), Some(50)),
        (202, b"live-envelope".as_slice(), Some(150)),
        (203, b"".as_slice(), None),
    ] {
        let mut context = proof_context(&session, id);
        context.logical_time_micros = 10;
        product.dispatch(
            &mut session,
            &context,
            ProductOperation::StructureSet {
                key: memory_lifecycle_key(binding.collection, ObjectId::new(id)?),
                value: value.to_vec(),
                expires_at_micros: expires,
            },
        )?;
    }
    let request = ProductMemoryRecallRequest {
        provenance: Vec::new(),
        collections: vec![binding.collection],
        search: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust database".into(),
                candidate_limit: 1,
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
            limit: 1,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
        limit: 1,
    };
    let mut context = proof_context(&session, 301);
    context.logical_time_micros = 100;
    let (response, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::MemoryRecall(request.clone()),
        NativeProofGenerationLimits::default(),
    )?;
    let ProductResponse::MemoryRecall(result) = response else {
        return Err("memory response missing".into());
    };
    assert_eq!(result.memories.len(), 1);
    assert_eq!(result.memories[0].hit.object_id.get(), 202);
    assert_eq!(result.memories[0].envelope, b"live-envelope");
    // Expiry, tombstones and missing lifecycle are removed even before the
    // one-candidate lexical budget; none can crowd out the live document.
    assert_eq!(result.expired_filtered, 3);
    assert!(
        result
            .searches
            .iter()
            .all(|search| search.result.snapshot == result.snapshot)
    );
    assert!(product.memory_recall(&request, 200)?.memories.is_empty());
    let mut hybrid = request.clone();
    hybrid.search.vectors.push(ProductVectorBranch {
        target: "semantic".into(),
        query: ProductVector::new([0.0, 1.0])?,
        candidate_limit: 16,
        weight: 1,
        execution: None,
        max_distance: None,
    });
    let (_, hybrid_proof) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &context,
        &ProductOperation::MemoryRecall(hybrid),
        NativeProofGenerationLimits::default(),
    )?;
    assert!(
        verify_native_proof_offline(
            &hybrid_proof.proof_bytes,
            &hybrid_proof.witness_bytes,
            hybrid_proof.trusted_anchor,
            &NativeVerificationLimits::default()
        )?
        .semantic_reexecution_performed
    );
    let mut invalid = request;
    invalid.collections.push(binding.collection);
    assert!(product.memory_recall(&invalid, 100).is_err());
    let mut forged = artifact.proof.content().clone();
    let mut bytes = forged.result.as_bytes().to_vec();
    let last = bytes.last_mut().ok_or("empty memory result")?;
    *last ^= 1;
    forged.result = CanonicalBytes::new(bytes);
    let forged = encode_native_proof(&NativeProof::new(forged)?, &ProofCodecLimits::default())?;
    drop(product);
    fs::remove_dir_all(&path)?;
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert_eq!(report.kind, NativeProofKind::Memory);
    assert_eq!(report.scope, NativeVerificationScope::SemanticReexecution);
    assert!(
        verify_native_proof_offline(
            &forged,
            &artifact.witness_bytes,
            artifact.trusted_anchor,
            &NativeVerificationLimits::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn synthetic_scientific_corpus_reopens_beyond_legacy_posting_charge()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("scientific-recovery-over-legacy-charge");
    let (mut product, binding) = configure_full(&path, None, vec![AnalyzerFilter::Lowercase], 0)?;
    let terms = (0..145)
        .map(|ordinal| format!("kinase{ordinal:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let text = format!("Synthetic scientific text about signaling and biomarkers. {terms} {terms}");
    assert!((2_000..=3_100).contains(&text.len()));
    for batch_ordinal in 0_u128..5 {
        let batch = ProductSearchIngestBatch {
            idempotency_id: batch_ordinal + 1,
            documents: (1_u128..=128)
                .map(|offset| {
                    Ok(ProductDocument {
                        object_id: ObjectId::new(batch_ordinal * 128 + offset)?,
                        text: text.clone(),
                        doc_values: BTreeMap::new(),
                        vectors: BTreeMap::new(),
                    })
                })
                .collect::<Result<_, Box<dyn std::error::Error>>>()?,
        };
        let receipt = product.ingest_search_batch(
            binding.collection,
            &batch,
            1,
            ProductDurability::Strict,
        )?;
        assert!(receipt.commit.is_some());
    }
    let before = product.snapshot_bounded(0)?.identity();
    drop(product);
    let reopened = NativeProduct::open(&path)?;
    assert_eq!(reopened.snapshot_bounded(0)?.identity(), before);
    let result =
        reopened.search_collection(binding.collection, &lexical_request("kinase001"), 1)?;
    assert_eq!(result.total_documents, 640);
    assert_eq!(result.hits.len(), 16);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn short_title_corpus_reopens_above_scientific_text_failure_count()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("short-title-recovery-control");
    let (mut product, binding) = configure_full(&path, None, vec![AnalyzerFilter::Lowercase], 0)?;
    for batch_ordinal in 0_u128..4 {
        let batch = ProductSearchIngestBatch {
            idempotency_id: batch_ordinal + 1,
            documents: (1_u128..=256)
                .map(|offset| {
                    Ok(ProductDocument {
                        object_id: ObjectId::new(batch_ordinal * 256 + offset)?,
                        text: "Kinase title".to_owned(),
                        doc_values: BTreeMap::new(),
                        vectors: BTreeMap::new(),
                    })
                })
                .collect::<Result<_, Box<dyn std::error::Error>>>()?,
        };
        assert!(
            product
                .ingest_search_batch(binding.collection, &batch, 1, ProductDurability::Strict)?
                .commit
                .is_some()
        );
    }
    drop(product);
    let reopened = NativeProduct::open(&path)?;
    let result = reopened.search_collection(binding.collection, &lexical_request("kinase"), 1)?;
    assert_eq!(result.total_documents, 1_024);
    assert_eq!(result.hits.len(), 16);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}
