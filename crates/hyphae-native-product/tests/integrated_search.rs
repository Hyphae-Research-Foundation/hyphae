// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used)]

//! Integrated product search persistence, strategy, and ingestion acceptance tests.

use std::{collections::BTreeMap, fs, path::PathBuf};

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogName,
    CatalogObjectV2, DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle,
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
    ProductDocValue, ProductDocument, ProductDurability, ProductFacetRequest, ProductLexicalBranch,
    ProductMissingPlacement, ProductNamedAggregation, ProductOperation, ProductPrincipal,
    ProductRequestContext, ProductSearchCollectionBinding, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchIngestionCoordinator, ProductSearchOperator, ProductSearchRequest,
    ProductSearchSort, ProductSession, ProductSessionId, ProductSortDirection, ProductSortSource,
    ProductStreamEnqueueOutcome, ProductVector, ProductVectorBranch, ProductVectorExecution,
    ProductVectorStrategy,
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

#[allow(clippy::too_many_lines)]
fn configure(
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
    let ann = AnnIndexDefinition::new(VectorMetric::SquaredL2, 8, 32, 16, 256, 7)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: 1_000,
        consolidate_after_deltas: 4,
        retain_generations: 2,
    };
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
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
