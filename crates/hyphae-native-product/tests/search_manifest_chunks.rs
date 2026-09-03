// SPDX-License-Identifier: Apache-2.0

//! Chunked collection manifest: identical durable records from every write
//! path across a chunk split, legacy read compatibility with a
//! first-mutation upgrade, and continuous pagination across chunk
//! boundaries after deletes.

use std::{collections::BTreeMap, fs, path::PathBuf};

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogName,
    CatalogObjectV2, DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle,
    LexicalIndexPolicy, LogicalCatalogObject, NamedVectorDefinition, ObjectHeaderV2, QualifiedName,
    SearchCollectionDefinitionV2, SearchFieldDefinitionV2, SearchFieldOptions, VectorMetric,
    VectorSearchPolicy,
};
use hyphae_native_product::{
    MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS, MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES, NativeProduct,
    ProductAuthorization, ProductDocValue, ProductDocument, ProductDurability, ProductErrorCode,
    ProductOperation, ProductPrincipal, ProductRequestContext, ProductResponse,
    ProductSearchCollectionBinding, ProductSearchDocumentDelete, ProductSearchFilter,
    ProductSearchIngestBatch, ProductSearchRequest, ProductSession, ProductSessionId,
    ProductTransactionSearchMutation, ProductVector,
};
use hyphae_native_types::{
    EngineKind, FieldId, IntegerWidth, LogicalType, ObjectId, VectorElement, VectorType,
};

const COLLECTION: u128 = 13;
const LEGACY_MAGIC: &[u8] = b"HYPSMAN1";
const HEADER_MAGIC: &[u8] = b"HYPSMAN2";
const CHUNK_MAGIC: &[u8] = b"HYPSCHK1";

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-search-manifest-chunks-{name}-{}",
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

/// A lexical collection with one integer doc value and one named vector, so
/// the same definition serves the delta path (vector-less documents) and the
/// materialized path (documents carrying the vector).
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
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                bm25: None,
                header: header(COLLECTION, EngineKind::Search, "chunks", Some(11))?,
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
                        name: name("ordinal")?,
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
                    id: FieldId::new(3)?,
                    name: name("embedding")?,
                    vector_type: VectorType::new(VectorElement::Float32, 2)?,
                    metric: VectorMetric::SquaredL2,
                    policy: VectorSearchPolicy::Ann(ann),
                    lifecycle: IncrementalVectorLifecycle {
                        delta_max_entries: 1_000,
                        consolidate_after_deltas: 4,
                        retain_generations: 2,
                    },
                }],
            },
        )),
        ProductDurability::Strict,
    )?;
    let collection = ObjectId::new(COLLECTION)?;
    product.provision_search_collection(collection, 0, ProductDurability::Strict)?;
    let binding = product.resolve_search_collection_binding(collection, 0)?;
    Ok((product, binding))
}

fn document(id: u128, with_vector: bool) -> Result<ProductDocument, Box<dyn std::error::Error>> {
    let mut vectors = BTreeMap::new();
    if with_vector {
        let component = f32::from(u8::try_from(id % 97)?);
        vectors.insert(
            "embedding".to_owned(),
            ProductVector::new([component, 1.0])?,
        );
    }
    Ok(ProductDocument {
        object_id: ObjectId::new(id)?,
        text: format!("document {id} body"),
        doc_values: BTreeMap::from([(
            "ordinal".to_owned(),
            ProductDocValue::Integer(i64::try_from(id)?),
        )]),
        vectors,
    })
}

/// Ingests identities `first..first + count` in bound-sized batches. When
/// `vector_leader` is set, the first document of every batch carries the
/// named vector so the batch takes the materialized transaction.
fn ingest_range(
    product: &mut NativeProduct,
    collection: ObjectId,
    first: u128,
    count: usize,
    vector_leader: bool,
    idempotency_base: u128,
    logical_time_micros: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let ids: Vec<u128> = (first..first + u128::try_from(count)?).collect();
    for (index, batch_ids) in ids.chunks(MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS).enumerate() {
        let documents = batch_ids
            .iter()
            .enumerate()
            .map(|(position, id)| document(*id, vector_leader && position == 0))
            .collect::<Result<Vec<_>, _>>()?;
        product.ingest_search_batch(
            collection,
            &ProductSearchIngestBatch {
                idempotency_id: idempotency_base + u128::try_from(index)?,
                documents,
            },
            logical_time_micros,
            ProductDurability::Memory,
        )?;
    }
    Ok(())
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

/// Every identity in ascending order by following page continuations.
fn paginate(
    product: &NativeProduct,
    collection: ObjectId,
    limit: usize,
    logical_time_micros: i64,
) -> Result<Vec<u128>, Box<dyn std::error::Error>> {
    let snapshot = product.snapshot_bounded(logical_time_micros)?;
    let mut start_after = None;
    let mut collected = Vec::new();
    loop {
        let page =
            NativeProduct::search_documents_at_snapshot(&snapshot, collection, start_after, limit)?;
        assert!(page.documents.len() <= limit);
        collected.extend(
            page.documents
                .iter()
                .map(|document| document.object_id.get()),
        );
        match page.continuation {
            Some(continuation) => {
                assert_eq!(
                    Some(continuation.get()),
                    collected.last().copied(),
                    "continuation must be the last returned identity"
                );
                start_after = Some(continuation);
            }
            None => return Ok(collected),
        }
    }
}

/// Counts `(legacy, header, chunk)` records by magic.
fn record_kinds(
    records: &[(Vec<u8>, Vec<u8>)],
) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let mut legacy = 0;
    let mut headers = 0;
    let mut chunks = 0;
    for (_, value) in records {
        if value.starts_with(LEGACY_MAGIC) {
            legacy += 1;
        } else if value.starts_with(HEADER_MAGIC) {
            headers += 1;
        } else if value.starts_with(CHUNK_MAGIC) {
            chunks += 1;
        } else {
            return Err("unknown manifest record magic".into());
        }
    }
    Ok((legacy, headers, chunks))
}

fn session() -> Result<ProductSession, Box<dyn std::error::Error>> {
    let principal = ProductPrincipal::new("manifest-chunks").ok_or("invalid principal")?;
    Ok(ProductSession::new(
        ProductSessionId::new(1).ok_or("zero session")?,
        principal,
        ProductAuthorization::ALL,
    ))
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
}

#[test]
fn delta_and_materialized_paths_write_identical_manifest_records_across_a_split()
-> Result<(), Box<dyn std::error::Error>> {
    let count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 276;
    let delta_path = temporary("delta");
    let (mut delta, binding) = configure(&delta_path)?;
    ingest_range(&mut delta, binding.collection, 1, count, false, 100, 1)?;
    let materialized_path = temporary("materialized");
    let (mut materialized, _) = configure(&materialized_path)?;
    ingest_range(
        &mut materialized,
        binding.collection,
        1,
        count,
        true,
        100,
        1,
    )?;

    let delta_records = delta.manifest_records_for_test(binding.collection, 1)?;
    let materialized_records = materialized.manifest_records_for_test(binding.collection, 1)?;
    assert_eq!(delta_records, materialized_records);
    let (legacy, headers, chunks) = record_kinds(&delta_records)?;
    assert_eq!((legacy, headers), (0, 1));
    assert_eq!(chunks, 2, "the corpus crossed exactly one split");

    let expected: Vec<u128> = (1..=u128::try_from(count)?).collect();
    assert_eq!(paginate(&delta, binding.collection, 300, 1)?, expected);
    assert_eq!(
        paginate(&materialized, binding.collection, 300, 1)?,
        expected
    );
    assert_eq!(
        delta
            .search_collection(binding.collection, &match_all(5), 1)?
            .total_documents,
        count
    );
    assert_eq!(
        materialized
            .search_collection(binding.collection, &match_all(5), 1)?
            .total_documents,
        count
    );

    drop(delta);
    drop(materialized);
    let delta = NativeProduct::open(&delta_path)?;
    let materialized = NativeProduct::open(&materialized_path)?;
    assert_eq!(
        delta.manifest_records_for_test(binding.collection, 1)?,
        delta_records
    );
    assert_eq!(
        materialized.manifest_records_for_test(binding.collection, 1)?,
        materialized_records
    );
    drop(delta);
    drop(materialized);
    fs::remove_dir_all(delta_path)?;
    fs::remove_dir_all(materialized_path)?;
    Ok(())
}

#[test]
fn legacy_manifest_is_readable_and_upgrades_on_the_first_accepted_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    let count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 276;
    let path = temporary("legacy");
    let (mut product, binding) = configure(&path)?;
    ingest_range(&mut product, binding.collection, 1, count, false, 100, 1)?;
    let expected: Vec<u128> = (1..=u128::try_from(count)?).collect();
    product.rewrite_manifest_as_legacy_for_test(binding.collection, 2)?;
    let records = product.manifest_records_for_test(binding.collection, 2)?;
    assert_eq!(record_kinds(&records)?, (1, 0, 0));
    drop(product);

    // Reads serve the legacy record and never rewrite it.
    let mut reopened = NativeProduct::open(&path)?;
    assert_eq!(
        reopened
            .search_collection(binding.collection, &match_all(5), 2)?
            .total_documents,
        count
    );
    assert_eq!(paginate(&reopened, binding.collection, 500, 2)?, expected);
    assert_eq!(
        reopened.manifest_records_for_test(binding.collection, 2)?,
        records
    );

    // A rejected mutation publishes nothing, so the record stays legacy.
    let duplicate = ProductSearchIngestBatch {
        idempotency_id: 900,
        documents: vec![document(1, false)?],
    };
    assert_eq!(
        reopened
            .ingest_search_batch(binding.collection, &duplicate, 3, ProductDurability::Memory)
            .err()
            .map(|error| error.code()),
        Some(ProductErrorCode::CatalogConflict)
    );
    assert_eq!(
        reopened.manifest_records_for_test(binding.collection, 3)?,
        records
    );

    // The first accepted mutation — a point-resolved delta batch — repacks
    // the manifest into format 2 in the same transaction.
    let more = 20;
    ingest_range(
        &mut reopened,
        binding.collection,
        u128::try_from(count)? + 1,
        more,
        false,
        1_000,
        4,
    )?;
    let upgraded = reopened.manifest_records_for_test(binding.collection, 4)?;
    assert_eq!(record_kinds(&upgraded)?, (0, 1, 2));
    let expected: Vec<u128> = (1..=u128::try_from(count + more)?).collect();
    assert_eq!(paginate(&reopened, binding.collection, 500, 4)?, expected);
    assert_eq!(
        reopened
            .search_collection(binding.collection, &match_all(5), 4)?
            .total_documents,
        count + more
    );
    drop(reopened);

    let reopened = NativeProduct::open(&path)?;
    assert_eq!(
        reopened.manifest_records_for_test(binding.collection, 4)?,
        upgraded
    );
    assert_eq!(paginate(&reopened, binding.collection, 500, 4)?, expected);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn delete_across_a_chunk_boundary_keeps_pagination_continuous()
-> Result<(), Box<dyn std::error::Error>> {
    let count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 276;
    let path = temporary("delete");
    let (mut product, binding) = configure(&path)?;
    ingest_range(&mut product, binding.collection, 1, count, false, 100, 1)?;
    let records = product.manifest_records_for_test(binding.collection, 1)?;
    assert_eq!(record_kinds(&records)?, (0, 1, 2));
    // Four full batches fill chunk 0 to the bound; the fifth splits it at the
    // midpoint, so identity 641 is the second chunk's floor.
    let boundary = u128::try_from(MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES / 2)? + 1;
    for (offset, id) in [boundary - 1, boundary].into_iter().enumerate() {
        let receipt = product.delete_search_document(
            binding.collection,
            ProductSearchDocumentDelete {
                idempotency_id: 700 + u128::try_from(offset)?,
                object_id: ObjectId::new(id)?,
            },
            2,
            ProductDurability::Memory,
        )?;
        assert_eq!(receipt.documents, 1);
    }
    let expected: Vec<u128> = (1..=u128::try_from(count)?)
        .filter(|id| *id != boundary - 1 && *id != boundary)
        .collect();
    assert_eq!(paginate(&product, binding.collection, 7, 2)?, expected);
    assert_eq!(
        product
            .search_collection(binding.collection, &match_all(5), 2)?
            .total_documents,
        count - 2
    );
    assert_eq!(
        record_kinds(&product.manifest_records_for_test(binding.collection, 2)?)?,
        (0, 1, 2),
        "deleting a chunk's floor identity keeps the chunk"
    );
    assert_eq!(
        product
            .delete_search_document(
                binding.collection,
                ProductSearchDocumentDelete {
                    idempotency_id: 800,
                    object_id: ObjectId::new(boundary)?,
                },
                3,
                ProductDurability::Memory,
            )
            .err()
            .map(|error| error.code()),
        Some(ProductErrorCode::ObjectNotFound)
    );

    drop(product);
    let reopened = NativeProduct::open(&path)?;
    assert_eq!(paginate(&reopened, binding.collection, 7, 2)?, expected);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn emptying_a_chunk_deletes_its_key_on_a_real_transaction() -> Result<(), Box<dyn std::error::Error>>
{
    // The legacy packer fills chunk 0 to the bound and leaves the remainder
    // in chunk 1, so two documents past the bound give a two-document chunk
    // whose removal exercises a durable chunk-key delete.
    let count = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES + 2;
    let path = temporary("empty-chunk");
    let (mut product, binding) = configure(&path)?;
    ingest_range(&mut product, binding.collection, 1, count, false, 100, 1)?;
    product.rewrite_manifest_as_legacy_for_test(binding.collection, 2)?;
    let bound = u128::try_from(MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES)?;
    // The first delete upgrades the manifest and removes one identity from
    // the two-document chunk.
    product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 700,
            object_id: ObjectId::new(bound + 1)?,
        },
        3,
        ProductDurability::Memory,
    )?;
    assert_eq!(
        record_kinds(&product.manifest_records_for_test(binding.collection, 3)?)?,
        (0, 1, 2)
    );
    // The second empties the chunk: its key is deleted, the header shrinks.
    product.delete_search_document(
        binding.collection,
        ProductSearchDocumentDelete {
            idempotency_id: 701,
            object_id: ObjectId::new(bound + 2)?,
        },
        4,
        ProductDurability::Memory,
    )?;
    let records = product.manifest_records_for_test(binding.collection, 4)?;
    assert_eq!(record_kinds(&records)?, (0, 1, 1));
    let expected: Vec<u128> = (1..=bound).collect();
    assert_eq!(paginate(&product, binding.collection, 300, 4)?, expected);
    assert_eq!(
        product
            .search_collection(binding.collection, &match_all(5), 4)?
            .total_documents,
        MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES
    );
    drop(product);
    let reopened = NativeProduct::open(&path)?;
    assert_eq!(
        reopened.manifest_records_for_test(binding.collection, 4)?,
        records
    );
    assert_eq!(paginate(&reopened, binding.collection, 300, 4)?, expected);
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn explicit_transaction_stages_documents_across_a_split_with_read_your_writes()
-> Result<(), Box<dyn std::error::Error>> {
    let base = MAX_PRODUCT_SEARCH_MANIFEST_CHUNK_ENTRIES - 4;
    let path = temporary("transaction");
    let (mut product, binding) = configure(&path)?;
    ingest_range(&mut product, binding.collection, 1, base, false, 100, 1)?;
    assert_eq!(
        record_kinds(&product.manifest_records_for_test(binding.collection, 1)?)?,
        (0, 1, 1)
    );

    let mut session = session()?;
    let begin_context = context(&session, 1);
    let begin = product.dispatch(
        &mut session,
        &begin_context,
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(
        hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
    ) = begin
    else {
        return Err("transaction did not begin".into());
    };
    let staged = 10;
    let first_new = u128::try_from(base)? + 1;
    let mut request_id = 2;
    for id in first_new..first_new + staged {
        let stage_context = context(&session, request_id);
        product.dispatch(
            &mut session,
            &stage_context,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Document {
                    collection: binding.collection,
                    document: document(id, false)?,
                },
            },
        )?;
        request_id += 1;
    }
    // Staging an identity the same transaction already staged is a replace,
    // observed through the batch's own writes.
    let replace_context = context(&session, request_id);
    product.dispatch(
        &mut session,
        &replace_context,
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: ProductTransactionSearchMutation::Document {
                collection: binding.collection,
                document: document(first_new, false)?,
            },
        },
    )?;
    request_id += 1;
    let commit_context = context(&session, request_id);
    let committed = product.dispatch(
        &mut session,
        &commit_context,
        ProductOperation::TransactionCommit { handle },
    )?;
    let ProductResponse::TransactionCommitted(_) = committed else {
        return Err("transaction did not commit".into());
    };

    let total = base + usize::try_from(staged)?;
    assert_eq!(
        record_kinds(&product.manifest_records_for_test(binding.collection, 5)?)?,
        (0, 1, 2),
        "the staged documents crossed the split inside one transaction"
    );
    let expected: Vec<u128> = (1..=u128::try_from(total)?).collect();
    assert_eq!(paginate(&product, binding.collection, 400, 5)?, expected);
    assert_eq!(
        product
            .search_collection(binding.collection, &match_all(5), 5)?
            .total_documents,
        total
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}
