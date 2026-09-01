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
    NativeProduct, ProductAggregation, ProductAggregationValue, ProductAuthorization,
    ProductDocValue, ProductDocument, ProductDurability, ProductFacetRequest, ProductHighlight,
    ProductLexicalBranch, ProductMissingPlacement, ProductNamedAggregation, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductSearchCollectionBinding,
    ProductSearchDocumentDelete, ProductSearchDocumentUpdate, ProductSearchFilter,
    ProductSearchIngestBatch, ProductSearchIngestionCoordinator, ProductSearchOperator,
    ProductSearchRequest, ProductSearchSort, ProductSession, ProductSessionId,
    ProductSortDirection, ProductSortSource, ProductStreamEnqueueOutcome, ProductVector,
    ProductVectorBranch, ProductVectorExecution, ProductVectorStrategy,
};
use hyphae_native_types::{
    EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId, VectorElement, VectorType,
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
    configure_full(path, None, vec![AnalyzerFilter::Lowercase])
}

fn configure_with_bm25(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
) -> Result<(NativeProduct, ProductSearchCollectionBinding), Box<dyn std::error::Error>> {
    configure_full(path, bm25, vec![AnalyzerFilter::Lowercase])
}

#[allow(clippy::too_many_lines)]
fn configure_full(
    path: &PathBuf,
    bm25: Option<Bm25Parameters>,
    analyzer_filters: Vec<AnalyzerFilter>,
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
            filters: analyzer_filters,
        })),
        ProductDurability::Strict,
    )?;
    let ann = AnnIndexDefinition::new(VectorMetric::SquaredL2, 8, 32, 16, 256, 7)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: 1_000,
        consolidate_after_deltas: 4,
        retain_generations: 2,
    };
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
                vectors: vec![
                    NamedVectorDefinition {
                        id: FieldId::new(4)?,
                        name: name("image")?,
                        vector_type: VectorType::new(VectorElement::Float32, 2)?,
                        metric: VectorMetric::SquaredL2,
                        policy: VectorSearchPolicy::Ann(ann),
                        lifecycle,
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

fn proof_session() -> Result<ProductSession, Box<dyn std::error::Error>> {
    let principal = ProductPrincipal::new("integrated-proof").ok_or("invalid principal")?;
    Ok(ProductSession::new(
        ProductSessionId::new(1).ok_or("zero session")?,
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
            }],
            filter: ProductSearchFilter::Compare {
                field: "price".into(),
                operator: ProductSearchOperator::Less,
                value: ProductDocValue::Integer(15),
            },
            sort: Vec::new(),
            facets: Vec::new(),
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
                },
            ],
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
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
    assert!(broad.approximate);
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
                }],
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
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
                }],
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
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
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: vec![ProductVectorBranch {
            target: "semantic".into(),
            query: ProductVector::new([1.0, 0.0])?,
            candidate_limit: 3,
            weight: 1,
            execution: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
fn idempotency_conflicts_and_document_update_delete_survive_reopen()
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

    product.update_search_document(
        binding.collection,
        &ProductSearchDocumentUpdate {
            idempotency_id: 42,
            document: document(501, "new token", "gear", 2, [1.0, 0.0], [1.0, 0.0])?,
        },
        0,
        ProductDurability::Strict,
    )?;
    let query = |text: &str| ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: text.into(),
            candidate_limit: 4,
            weight: 1,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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

    product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 43,
            object_id: ObjectId::new(501)?,
        },
        0,
        ProductDurability::Strict,
    )?;
    drop(product);
    let mut reopened = NativeProduct::open(&path)?;
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
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
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
        }),
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: image.clone(),
            candidate_limit: 4,
            weight: 2,
            execution: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
                    }),
                    vectors: Vec::new(),
                    filter: ProductSearchFilter::MatchAll,
                    sort: Vec::new(),
                    facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::Compare {
            field: "parent".into(),
            operator: ProductSearchOperator::Equal,
            value: ProductDocValue::Bytes(parent_id.to_le_bytes().to_vec()),
        },
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
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
    let document = ProductDocument {
        object_id: ObjectId::new(900)?,
        text: "atomic staged document".to_owned(),
        doc_values: BTreeMap::new(),
        vectors: BTreeMap::new(),
    };
    let c2 = proof_context(&session, 2);
    product.dispatch(
        &mut session,
        &c2,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: document.clone(),
            },
        },
    )?;
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
                query: "atomic staged".to_owned(),
                candidate_limit: 10,
                weight: 1,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
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
    let c4 = proof_context(&session, 4);
    let begin = product.dispatch(&mut session, &c4, ProductOperation::TransactionBegin)?;
    let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
        hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
    ) = begin
    else {
        return Err("replacement transaction did not begin".into());
    };
    let c5 = proof_context(&session, 5);
    product.dispatch(
        &mut session,
        &c5,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: hyphae_native_product::ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document,
            },
        },
    )?;
    let c6 = proof_context(&session, 6);
    product.dispatch(
        &mut session,
        &c6,
        ProductOperation::TransactionCommit { handle },
    )?;
    drop(product);
    let reopened = NativeProduct::open(&path)?;
    reopened.resolve_search_collection_binding(binding.collection, 0)?;
    drop(reopened);
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
        }),
        vectors: vec![ProductVectorBranch {
            target: "image".into(),
            query: image.clone(),
            candidate_limit: 4,
            weight: 1,
            execution: None,
        }],
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
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
