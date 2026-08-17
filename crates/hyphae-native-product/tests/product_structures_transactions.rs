// SPDX-License-Identifier: Apache-2.0

//! Product structure-family and explicit all-engine transaction integration tests.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_catalog::{
    CatalogName, CatalogObject, ObjectHeader, QualifiedName, StructureDefinition,
    StructureOwnership,
};
use hyphae_native_product::{
    NativeProduct, ObjectId, ProductAuthorization, ProductDurabilityPolicy, ProductErrorCode,
    ProductExplicitTransactionStatus, ProductLimits, ProductListSide, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductResponse, ProductSession, ProductSessionId,
    ProductStructureKey, ProductStructureMutation, ProductStructureReadRequest,
    ProductStructureReadResult, ProductTransactionId, ProductTransactionSearchMutation,
    ProductTransactionSqlMutation, ProductTransactionStageResult, ProductTransactionStatus,
    ProductTransactionVectorMutation, ProductValue, ProductVector, StructureKind,
};
use hyphae_native_types::{EngineKind, LogicalType};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-product-families-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
    ))
}

#[test]
#[allow(clippy::too_many_lines)]
fn list_pop_stage_response_limit_does_not_retain_a_hidden_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("list-pop-response-limit");
    let _ = fs::remove_dir_all(&path);
    let mut runtime = hyphae_native_runtime::NativeDatabase::create(&path)?;
    let mut seed = runtime.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.create_catalog_object_v2(hyphae_native_product::LogicalCatalogObject::from_legacy(
        keyspace(4, "lists", StructureKind::List)?,
    ))?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), vec![b'x'; 512])?;
    seed.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let mut session = session()?;
    let begin_context = context(&session, 1);
    let begin = product.dispatch(
        &mut session,
        &begin_context,
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = begin
    else {
        return Err("transaction did not begin".into());
    };
    let expected_response_bytes = 16 + 8 + 8 + 1 + 1 + 1 + 1 + 4 + 512;
    let mut limited = context(&session, 2);
    limited.limits = ProductLimits {
        max_response_bytes: expected_response_bytes - 1,
        ..ProductLimits::default()
    };
    let Err(error) = product.dispatch(
        &mut session,
        &limited,
        ProductOperation::TransactionStageStructure {
            handle,
            mutation: ProductStructureMutation::ListPop {
                key: key(4, b"queue")?,
                side: ProductListSide::Left,
            },
        },
    ) else {
        return Err("oversized ListPop stage response was retained".into());
    };
    assert_eq!(error.code(), ProductErrorCode::LimitExceeded);

    let status_context = context(&session, 3);
    let status = product.dispatch(
        &mut session,
        &status_context,
        ProductOperation::ExplicitTransactionStatus { handle },
    )?;
    assert!(matches!(
        status,
        ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
            staged_operations: 0,
            ..
        })
    ));
    let commit_context = context(&session, 4);
    let Err(commit_error) = product.dispatch(
        &mut session,
        &commit_context,
        ProductOperation::TransactionCommit { handle },
    ) else {
        return Err("hidden ListPop mutation committed after response rejection".into());
    };
    assert_eq!(commit_error.code(), ProductErrorCode::InvalidRequest);
    let rollback_context = context(&session, 5);
    product.dispatch(
        &mut session,
        &rollback_context,
        ProductOperation::TransactionRollback { handle },
    )?;
    let read_context = context(&session, 6);
    let read = product.dispatch(
        &mut session,
        &read_context,
        ProductOperation::StructureRead(ProductStructureReadRequest::ListRange {
            key: key(4, b"queue")?,
            start: 0,
            stop: -1,
        }),
    )?;
    assert!(matches!(
        read,
        ProductResponse::StructureRead(read)
            if read.value == ProductStructureReadResult::Values(vec![vec![b'x'; 512]])
    ));

    let exact_begin_context = context(&session, 7);
    let exact_begin = product.dispatch(
        &mut session,
        &exact_begin_context,
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = exact_begin
    else {
        return Err("exact-bound transaction did not begin".into());
    };
    let mut exact = context(&session, 8);
    exact.limits.max_response_bytes = expected_response_bytes;
    let staged = product.dispatch(
        &mut session,
        &exact,
        ProductOperation::TransactionStageStructure {
            handle,
            mutation: ProductStructureMutation::ListPop {
                key: key(4, b"queue")?,
                side: ProductListSide::Left,
            },
        },
    )?;
    assert!(matches!(
        staged,
        ProductResponse::TransactionStaged(ref receipt)
            if receipt.result
                == ProductTransactionStageResult::Structure(
                    hyphae_native_product::ProductStructureMutationResult::Value(Some(vec![b'x'; 512]))
                )
    ));
    let final_rollback_context = context(&session, 9);
    product.dispatch(
        &mut session,
        &final_rollback_context,
        ProductOperation::TransactionRollback { handle },
    )?;
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

fn session() -> Result<ProductSession, Box<dyn std::error::Error>> {
    Ok(ProductSession::new(
        ProductSessionId::new(1).ok_or("zero session")?,
        ProductPrincipal::new("product-test").ok_or("invalid principal")?,
        ProductAuthorization::ALL,
    ))
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    let mut context = ProductRequestContext::new(
        request_id,
        session.id(),
        10,
        session.principal().clone(),
        session.authorization(),
    );
    context.durability = ProductDurabilityPolicy::MEMORY;
    context
}

fn keyspace(
    id: u128,
    name: &str,
    kind: StructureKind,
) -> Result<CatalogObject, Box<dyn std::error::Error>> {
    Ok(CatalogObject::Structure(StructureDefinition {
        header: ObjectHeader {
            id: ObjectId::new(id)?,
            owner: EngineKind::Structure,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted(name)?,
            ),
        },
        kind,
        key_type: LogicalType::Binary,
        value_type: LogicalType::Binary,
        ownership: StructureOwnership::Canonical,
        ttl_enabled: true,
    }))
}

fn key(id: u128, value: &[u8]) -> Result<ProductStructureKey, Box<dyn std::error::Error>> {
    Ok(ProductStructureKey {
        keyspace: ObjectId::new(id)?,
        key: value.to_vec(),
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_structure_family_is_catalogued_atomic_and_snapshot_equal()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("families");
    let _ = fs::remove_dir_all(&path);
    let mut runtime = hyphae_native_runtime::NativeDatabase::create(&path)?;
    let mut seed = runtime.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    for (id, name, family) in [
        (1, "strings", StructureKind::String),
        (2, "counters", StructureKind::Counter),
        (3, "hashes", StructureKind::Hash),
        (4, "lists", StructureKind::List),
        (5, "sets", StructureKind::Set),
        (6, "sorted", StructureKind::SortedSet),
        (7, "streams", StructureKind::Stream),
    ] {
        match keyspace(id, name, family)? {
            CatalogObject::Structure(definition) => {
                seed.create_catalog_object_v2(
                    hyphae_native_product::LogicalCatalogObject::from_legacy(
                        CatalogObject::Structure(definition),
                    ),
                )?;
            }
            _ => unreachable!(),
        }
    }
    seed.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let mut session = session()?;
    let request = context(&session, 1);
    let response = product.dispatch(
        &mut session,
        &request,
        ProductOperation::StructureMutate {
            mutations: vec![
                ProductStructureMutation::StringSet {
                    key: key(1, b"message")?,
                    value: b"hello".to_vec(),
                    expires_at_micros: Some(20),
                },
                ProductStructureMutation::CounterAdd {
                    key: key(2, b"count")?,
                    delta: 3,
                },
                ProductStructureMutation::Create {
                    key: key(3, b"hash")?,
                    family: StructureKind::Hash,
                },
                ProductStructureMutation::HashSet {
                    key: key(3, b"hash")?,
                    field: b"field".to_vec(),
                    value: b"value".to_vec(),
                },
                ProductStructureMutation::Create {
                    key: key(4, b"list")?,
                    family: StructureKind::List,
                },
                ProductStructureMutation::ListPush {
                    key: key(4, b"list")?,
                    side: ProductListSide::Right,
                    value: b"item".to_vec(),
                },
                ProductStructureMutation::Create {
                    key: key(5, b"set")?,
                    family: StructureKind::Set,
                },
                ProductStructureMutation::SetAdd {
                    key: key(5, b"set")?,
                    member: b"member".to_vec(),
                },
                ProductStructureMutation::Create {
                    key: key(6, b"sorted")?,
                    family: StructureKind::SortedSet,
                },
                ProductStructureMutation::SortedSetAdd {
                    key: key(6, b"sorted")?,
                    member: b"ranked".to_vec(),
                    score: hyphae_native_product::CanonicalF64::new(1.5),
                },
                ProductStructureMutation::Create {
                    key: key(7, b"stream")?,
                    family: StructureKind::Stream,
                },
                ProductStructureMutation::StreamAdd {
                    key: key(7, b"stream")?,
                    fields: vec![hyphae_native_product::ProductHashEntry {
                        field: b"kind".to_vec(),
                        value: b"created".to_vec(),
                    }],
                },
            ],
        },
    )?;
    assert!(matches!(response, ProductResponse::StructureMutated(_)));

    let request = context(&session, 2);
    let read = product.dispatch(
        &mut session,
        &request,
        ProductOperation::StructureRead(ProductStructureReadRequest::StreamRange {
            key: key(7, b"stream")?,
            start: 0,
            end: u64::MAX,
            limit: 8,
        }),
    )?;
    let ProductResponse::StructureRead(read) = read else {
        return Err("wrong structure response".into());
    };
    assert!(
        matches!(read.value, ProductStructureReadResult::StreamEntries(ref values) if values.len() == 1)
    );
    assert_eq!(read.snapshot, product.snapshot_bounded(10)?.identity());
    assert_eq!(
        product.snapshot_bounded(10)?.structure_get(b"message"),
        Some(b"hello".as_slice())
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_transaction_stages_all_engines_and_rolls_back_without_partial_state()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("transaction");
    let _ = fs::remove_dir_all(&path);
    let mut runtime = hyphae_native_runtime::NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(100)?;
    let vectors = ObjectId::new(200)?;
    let mut seed = runtime.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
        &[],
    )?;
    seed.create_catalog_object_v2(hyphae_native_product::LogicalCatalogObject::from_legacy(
        keyspace(50, "strings", StructureKind::String)?,
    ))?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(
        vectors,
        "vectors",
        2,
        hyphae_native_runtime::VectorMetric::Cosine,
        hyphae_native_runtime::HnswConfig::new(4, 16, 8, 32, 7)?,
    )?;
    seed.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let mut session = session()?;
    let request = context(&session, 10);
    let begin = product.dispatch(&mut session, &request, ProductOperation::TransactionBegin)?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = begin
    else {
        return Err("transaction did not begin".into());
    };
    for (request_id, operation) in [
        (
            11,
            ProductOperation::TransactionStageSql {
                handle,
                mutation: ProductTransactionSqlMutation {
                    statement: "INSERT INTO events (id, body) VALUES (?, ?)".to_owned(),
                    parameters: vec![
                        ProductValue::Signed(1),
                        ProductValue::Text("one".to_owned()),
                    ],
                },
            },
        ),
        (
            12,
            ProductOperation::TransactionStageStructure {
                handle,
                mutation: ProductStructureMutation::StringSet {
                    key: key(50, b"joint")?,
                    value: b"value".to_vec(),
                    expires_at_micros: None,
                },
            },
        ),
        (
            13,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Index {
                    index: lexical,
                    document_id: b"doc".to_vec(),
                    text: "one".to_owned(),
                },
            },
        ),
        (
            14,
            ProductOperation::TransactionStageVector {
                handle,
                mutation: ProductTransactionVectorMutation::Upsert {
                    index: vectors,
                    object_id: ObjectId::new(300)?,
                    vector: ProductVector::new([1.0, 0.0])?,
                },
            },
        ),
    ] {
        let request = context(&session, request_id);
        let staged = product.dispatch(&mut session, &request, operation)?;
        assert!(
            matches!(staged, ProductResponse::TransactionStaged(ref value) if !matches!(value.result, ProductTransactionStageResult::Vector(false)))
        );
    }
    let request = context(&session, 15);
    let committed = product.dispatch(
        &mut session,
        &request,
        ProductOperation::TransactionCommit { handle },
    )?;
    let ProductResponse::TransactionCommitted(committed) = committed else {
        return Err("transaction did not commit".into());
    };
    assert_eq!(committed.staged_operations, 4);

    let request = context(&session, 16);
    let begin = product.dispatch(&mut session, &request, ProductOperation::TransactionBegin)?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = begin
    else {
        return Err("rollback transaction did not begin".into());
    };
    let request = context(&session, 17);
    product.dispatch(
        &mut session,
        &request,
        ProductOperation::TransactionStageStructure {
            handle,
            mutation: ProductStructureMutation::StringSet {
                key: key(50, b"rollback")?,
                value: b"no".to_vec(),
                expires_at_micros: None,
            },
        },
    )?;
    let request = context(&session, 18);
    assert!(matches!(
        product.dispatch(
            &mut session,
            &request,
            ProductOperation::TransactionRollback { handle },
        )?,
        ProductResponse::TransactionRolledBack(_)
    ));
    assert_eq!(
        product.snapshot_bounded(10)?.structure_get(b"rollback"),
        None
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn unknown_explicit_commit_resolves_after_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("unknown-commit");
    let _ = fs::remove_dir_all(&path);
    let mut runtime = hyphae_native_runtime::NativeDatabase::create(&path)?;
    let mut seed = runtime.begin(0, hyphae_native_types::DurabilityClass::Strict)?;
    seed.create_catalog_object_v2(hyphae_native_product::LogicalCatalogObject::from_legacy(
        keyspace(50, "strings", StructureKind::String)?,
    ))?;
    seed.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let binding_csn = product
        .snapshot_bounded(0)?
        .identity()
        .visible_csn
        .ok_or("default scalar binding commit is missing")?
        .get();
    let mut product_session = session()?;
    let request = context(&product_session, 30);
    let begin = product.dispatch(
        &mut product_session,
        &request,
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = begin
    else {
        return Err("transaction did not begin".into());
    };
    let request = context(&product_session, 31);
    product.dispatch(
        &mut product_session,
        &request,
        ProductOperation::TransactionStageStructure {
            handle,
            mutation: ProductStructureMutation::StringSet {
                key: key(50, b"unknown")?,
                value: b"committed".to_vec(),
                expires_at_micros: None,
            },
        },
    )?;
    let request = context(&product_session, 32);
    let Err(error) = hyphae_native_product::commit_explicit_transaction_with_interruption_for_test(
        &mut product,
        &mut product_session,
        &request,
        handle,
        hyphae_native_runtime::CommitBoundary::WalSynchronized,
    ) else {
        return Err("interrupted commit unexpectedly acknowledged".into());
    };
    assert_eq!(error.code(), ProductErrorCode::UnknownCommit);
    let transaction_id = ProductTransactionId::from(
        error
            .details()
            .transaction_id()
            .ok_or("missing resolution identity")?,
    );
    drop(product);

    let mut reopened = NativeProduct::open(&path)?;
    let mut reopened_session = session()?;
    let request = context(&reopened_session, 33);
    let status = reopened.dispatch(
        &mut reopened_session,
        &request,
        ProductOperation::TransactionStatus { transaction_id },
    )?;
    assert!(matches!(
        status,
        ProductResponse::TransactionStatus(ProductTransactionStatus::Committed(receipt))
            if receipt.commit_csn == binding_csn + 1
    ));
    assert_eq!(
        reopened.snapshot_bounded(10)?.structure_get(b"unknown"),
        Some(b"committed".as_slice())
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}
