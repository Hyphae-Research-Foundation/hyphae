// SPDX-License-Identifier: AGPL-3.0-only

#![allow(clippy::expect_used)]

//! Direct and one-owner product operation dispatcher coverage.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_product::{
    MetricId, MetricValue, NativeProduct, NativeProductService, NativeProductServiceConfig,
    ProductAuthorization, ProductCommitOutcome, ProductDurability, ProductDurabilityPolicy,
    ProductErrorCode, ProductOperation, ProductPermission, ProductPrincipal, ProductRequestContext,
    ProductResponse, ProductSession, ProductSessionId, ProductSqlResult, ProductValue,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-native-product-operation-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn principal(name: &str) -> Result<ProductPrincipal, Box<dyn Error>> {
    ProductPrincipal::new(name).ok_or_else(|| "test principal is invalid".into())
}

fn direct_session(
    name: &str,
    authorization: ProductAuthorization,
) -> Result<ProductSession, Box<dyn Error>> {
    Ok(ProductSession::new(
        ProductSessionId::new(1).ok_or("zero session")?,
        principal(name)?,
        authorization,
    ))
}

fn context(
    session: &ProductSession,
    request_id: u128,
    logical_time_micros: i64,
) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        logical_time_micros,
        session.principal().clone(),
        session.authorization(),
    )
}

fn memory_context(
    session: &ProductSession,
    request_id: u128,
    logical_time_micros: i64,
) -> ProductRequestContext {
    let mut context = context(session, request_id, logical_time_micros);
    context.durability = ProductDurabilityPolicy::MEMORY;
    context
}

#[test]
fn direct_facade_and_operation_dispatcher_return_the_same_prepared_read()
-> Result<(), Box<dyn Error>> {
    let path = temporary("parity");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut session = direct_session("parity", ProductAuthorization::ALL)?;

    let create_context = memory_context(&session, 1, 10);
    product.dispatch(
        &mut session,
        &create_context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE items (id BIGINT PRIMARY KEY, label TEXT NOT NULL)".to_owned(),
            parameters: vec![],
        },
    )?;
    let insert_context = memory_context(&session, 2, 10);
    product.dispatch(
        &mut session,
        &insert_context,
        ProductOperation::ExecuteSql {
            statement: "INSERT INTO items (id, label) VALUES (?, ?)".to_owned(),
            parameters: vec![
                ProductValue::Signed(7),
                ProductValue::Text("seven".to_owned()),
            ],
        },
    )?;

    let statement = "SELECT label FROM items WHERE id = ?";
    let direct_plan = product.prepare_sql(statement)?;
    let direct = product.execute_prepared(&direct_plan, &[ProductValue::Signed(7)])?;
    let prepare_context = context(&session, 3, 10);
    let prepared = product.dispatch(
        &mut session,
        &prepare_context,
        ProductOperation::PrepareSql {
            statement: statement.to_owned(),
        },
    )?;
    let ProductResponse::PreparedSql { handle, .. } = prepared else {
        return Err("prepare returned the wrong response".into());
    };
    let execute_context = context(&session, 4, 10);
    let dispatched = product.dispatch(
        &mut session,
        &execute_context,
        ProductOperation::ExecutePrepared {
            handle,
            parameters: vec![ProductValue::Signed(7)],
        },
    )?;
    assert_eq!(
        dispatched,
        ProductResponse::Sql {
            result: direct.value,
            snapshot: Some(direct.snapshot),
            commit: None,
        }
    );

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn cancellation_deadline_and_authorization_fail_before_mutation() -> Result<(), Box<dyn Error>> {
    let path = temporary("admission");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut writer = direct_session("writer", ProductAuthorization::ALL)?;

    let cancelled = memory_context(&writer, 11, 100);
    cancelled.cancellation.cancel();
    let error = product
        .dispatch(
            &mut writer,
            &cancelled,
            ProductOperation::StructureSet {
                key: b"cancelled".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            },
        )
        .expect_err("cancelled mutation was accepted");
    assert_eq!(error.code(), ProductErrorCode::Cancelled);
    assert_eq!(error.request_id(), Some(11));

    let mut expired = memory_context(&writer, 12, 100);
    expired.deadline_micros = Some(0);
    let error = product
        .dispatch(
            &mut writer,
            &expired,
            ProductOperation::StructureSet {
                key: b"expired".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            },
        )
        .expect_err("expired mutation was accepted");
    assert_eq!(error.code(), ProductErrorCode::DeadlineExceeded);

    let write_only = ProductAuthorization::from_permissions([ProductPermission::DataRead]);
    let mut reader = ProductSession::new(
        ProductSessionId::new(2).expect("nonzero session"),
        principal("reader")?,
        write_only,
    );
    let reader_context = context(&reader, 13, 100);
    let error = product
        .dispatch(
            &mut reader,
            &reader_context,
            ProductOperation::StructureSet {
                key: b"denied".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            },
        )
        .expect_err("unauthorized mutation was accepted");
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);

    let mut response_limited = memory_context(&writer, 14, 100);
    response_limited.limits.max_response_bytes = 1;
    let error = product
        .dispatch(
            &mut writer,
            &response_limited,
            ProductOperation::StructureSet {
                key: b"response-limited".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            },
        )
        .expect_err("response-limited mutation was accepted");
    assert_eq!(error.code(), ProductErrorCode::LimitExceeded);

    let snapshot = product.snapshot_bounded(100)?;
    assert_eq!(snapshot.structure_get(b"cancelled"), None);
    assert_eq!(snapshot.structure_get(b"expired"), None);
    assert_eq!(snapshot.structure_get(b"denied"), None);
    assert_eq!(snapshot.structure_get(b"response-limited"), None);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn mutation_receipt_reports_selected_durability_and_status() -> Result<(), Box<dyn Error>> {
    let path = temporary("durability");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut session = direct_session("durability", ProductAuthorization::ALL)?;
    let set_context = memory_context(&session, 21, 0);
    let response = product.dispatch(
        &mut session,
        &set_context,
        ProductOperation::StructureSet {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: None,
        },
    )?;
    let ProductResponse::StructureSet(ProductCommitOutcome::Committed(receipt)) = response else {
        return Err("set did not return a committed receipt".into());
    };
    assert_eq!(receipt.durability, ProductDurability::Memory);

    let status_context = context(&session, 22, 0);
    let status = product.dispatch(
        &mut session,
        &status_context,
        ProductOperation::TransactionStatus {
            transaction_id: receipt.transaction_id,
        },
    )?;
    assert_eq!(
        status,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Committed(receipt)
        )
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn durable_status_reopens_and_does_not_alias_request_ids() -> Result<(), Box<dyn Error>> {
    let path = temporary("durable-status");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut owner = direct_session("owner", ProductAuthorization::ALL)?;
    let mut first_context = context(&owner, 77, 0);
    first_context.idempotency_token = Some(1001);
    let first = product.dispatch(
        &mut owner,
        &first_context,
        ProductOperation::StructureSet {
            key: b"first".to_vec(),
            value: b"one".to_vec(),
            expires_at_micros: None,
        },
    )?;
    let ProductResponse::StructureSet(ProductCommitOutcome::Committed(first_receipt)) = first
    else {
        return Err("first mutation did not commit".into());
    };
    let mut second_context = context(&owner, 77, 0);
    second_context.idempotency_token = Some(1002);
    let second = product.dispatch(
        &mut owner,
        &second_context,
        ProductOperation::StructureSet {
            key: b"second".to_vec(),
            value: b"two".to_vec(),
            expires_at_micros: None,
        },
    )?;
    let ProductResponse::StructureSet(ProductCommitOutcome::Committed(second_receipt)) = second
    else {
        return Err("second mutation did not commit".into());
    };
    assert_ne!(first_receipt.transaction_id, second_receipt.transaction_id);
    assert_ne!(first_receipt.transaction_id.get(), 77);
    let replay_context = ProductRequestContext {
        idempotency_token: Some(1001),
        request_id: 82,
        ..context(&owner, 82, 0)
    };
    let conflict = product
        .dispatch(
            &mut owner,
            &replay_context,
            ProductOperation::StructureSet {
                key: b"third".to_vec(),
                value: b"three".to_vec(),
                expires_at_micros: None,
            },
        )
        .expect_err("reused idempotency token published a second mutation");
    assert_eq!(conflict.code(), ProductErrorCode::IdempotencyConflict);

    drop(product);
    let mut reopened = NativeProduct::open(&path)?;
    let mut reconnected = ProductSession::new(
        ProductSessionId::new(9).ok_or("nonzero session")?,
        principal("owner")?,
        ProductAuthorization::ALL,
    );
    let reconnect_context = context(&reconnected, 78, 0);
    let status = reopened.dispatch(
        &mut reconnected,
        &reconnect_context,
        ProductOperation::TransactionStatus {
            transaction_id: first_receipt.transaction_id,
        },
    )?;
    assert!(matches!(
        status,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Committed(receipt)
        ) if receipt.transaction_id == first_receipt.transaction_id
    ));
    let token_context = context(&reconnected, 80, 0);
    let token_status = reopened.dispatch(
        &mut reconnected,
        &token_context,
        ProductOperation::TransactionStatusByIdempotency {
            idempotency_token: 1001,
        },
    )?;
    assert!(matches!(
        token_status,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Committed(receipt)
        ) if receipt.transaction_id == first_receipt.transaction_id
    ));

    let mut stranger = ProductSession::new(
        ProductSessionId::new(10).ok_or("nonzero session")?,
        principal("stranger")?,
        ProductAuthorization::ALL,
    );
    let stranger_context = context(&stranger, 79, 0);
    let unauthorized = reopened.dispatch(
        &mut stranger,
        &stranger_context,
        ProductOperation::TransactionStatus {
            transaction_id: first_receipt.transaction_id,
        },
    )?;
    assert_eq!(
        unauthorized,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Unknown
        )
    );
    let unauthorized_token_context = context(&stranger, 81, 0);
    let unauthorized_token = reopened.dispatch(
        &mut stranger,
        &unauthorized_token_context,
        ProductOperation::TransactionStatusByIdempotency {
            idempotency_token: 1001,
        },
    )?;
    assert_eq!(
        unauthorized_token,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Unknown
        )
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn service_sessions_are_isolated_while_clients_share_one_owner() -> Result<(), Box<dyn Error>> {
    let path = temporary("clients");
    let _ = fs::remove_dir_all(&path);
    let service = NativeProductService::start(
        NativeProduct::create(&path)?,
        NativeProductServiceConfig::default(),
    )?;
    let handle = service.handle();
    let first = handle.open_session(principal("first")?, ProductAuthorization::ALL)?;
    let second = handle.open_session(principal("second")?, ProductAuthorization::ALL)?;

    let mut create_context = first.request_context(30, 0);
    create_context.durability = ProductDurabilityPolicy::MEMORY;
    first.dispatch(
        create_context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE shared_items (id BIGINT PRIMARY KEY)".to_owned(),
            parameters: vec![],
        },
    )?;
    let prepared = first.dispatch(
        first.request_context(31, 0),
        ProductOperation::PrepareSql {
            statement: "SELECT id FROM shared_items WHERE id = ?".to_owned(),
        },
    )?;
    let ProductResponse::PreparedSql { handle, .. } = prepared else {
        return Err("first session did not retain its prepared plan".into());
    };
    let foreign = second
        .dispatch(
            second.request_context(32, 0),
            ProductOperation::ExecutePrepared {
                handle,
                parameters: vec![ProductValue::Signed(1)],
            },
        )
        .expect_err("second session used another session's prepared handle");
    assert_eq!(foreign.code(), ProductErrorCode::SqlInvalidValue);

    let mut set_context = first.request_context(33, 0);
    set_context.durability = ProductDurabilityPolicy::MEMORY;
    first.dispatch(
        set_context,
        ProductOperation::StructureSet {
            key: b"shared".to_vec(),
            value: b"one".to_vec(),
            expires_at_micros: None,
        },
    )?;
    assert_eq!(
        second.dispatch(
            second.request_context(34, 0),
            ProductOperation::StructureGet {
                key: b"shared".to_vec(),
            },
        )?,
        ProductResponse::StructureValue(Some(b"one".to_vec()))
    );

    service.shutdown()?;
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn service_status_never_leaks_across_principals() -> Result<(), Box<dyn Error>> {
    let path = temporary("service-status-auth");
    let _ = fs::remove_dir_all(&path);
    let service = NativeProductService::start(
        NativeProduct::create(&path)?,
        NativeProductServiceConfig::default(),
    )?;
    let handle = service.handle();
    let owner = handle.open_session(principal("service-owner")?, ProductAuthorization::ALL)?;
    let stranger =
        handle.open_session(principal("service-stranger")?, ProductAuthorization::ALL)?;
    let mut mutation = owner.request_context(90, 0);
    mutation.idempotency_token = Some(9001);
    let response = owner.dispatch(
        mutation,
        ProductOperation::StructureSet {
            key: b"private-status".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: None,
        },
    )?;
    let ProductResponse::StructureSet(ProductCommitOutcome::Committed(receipt)) = response else {
        return Err("service mutation did not commit".into());
    };
    assert_eq!(
        stranger.dispatch(
            stranger.request_context(91, 0),
            ProductOperation::TransactionStatus {
                transaction_id: receipt.transaction_id,
            },
        )?,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Unknown
        )
    );
    assert_eq!(
        stranger.dispatch(
            stranger.request_context(92, 0),
            ProductOperation::TransactionStatusByIdempotency {
                idempotency_token: 9001,
            },
        )?,
        ProductResponse::TransactionStatus(
            hyphae_native_product::ProductTransactionStatus::Unknown
        )
    );
    drop(owner);
    drop(stranger);
    drop(service.shutdown()?);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn graceful_shutdown_drains_admitted_mutation_and_stops_new_admission() -> Result<(), Box<dyn Error>>
{
    let path = temporary("shutdown");
    let _ = fs::remove_dir_all(&path);
    let service = NativeProductService::start(
        NativeProduct::create(&path)?,
        NativeProductServiceConfig {
            queue_capacity: 1,
            ..NativeProductServiceConfig::default()
        },
    )?;
    let client = service
        .handle()
        .open_session(principal("shutdown")?, ProductAuthorization::ALL)?;
    let mut request = client.request_context(41, 0);
    request.durability = ProductDurabilityPolicy::MEMORY;
    let pending = client.submit(
        request,
        ProductOperation::StructureSet {
            key: b"drained".to_vec(),
            value: b"yes".to_vec(),
            expires_at_micros: None,
        },
    )?;

    let product = service.shutdown()?;
    assert!(matches!(
        pending.wait()?,
        ProductResponse::StructureSet(ProductCommitOutcome::Committed(_))
    ));
    let rejected = client
        .dispatch(
            client.request_context(42, 0),
            ProductOperation::Capabilities,
        )
        .expect_err("service accepted work after shutdown");
    assert_eq!(rejected.code(), ProductErrorCode::Unavailable);
    assert_eq!(
        product.snapshot_bounded(0)?.structure_get(b"drained"),
        Some(b"yes".as_slice())
    );
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn direct_sql_query_response_is_typed() -> Result<(), Box<dyn Error>> {
    let path = temporary("query");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut session = direct_session("query", ProductAuthorization::ALL)?;
    let create_context = memory_context(&session, 51, 0);
    product.dispatch(
        &mut session,
        &create_context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE values_table (id BIGINT PRIMARY KEY)".to_owned(),
            parameters: vec![],
        },
    )?;
    let query_context = context(&session, 52, 0);
    let response = product.dispatch(
        &mut session,
        &query_context,
        ProductOperation::ExecuteSql {
            statement: "SELECT id FROM values_table WHERE id = ?".to_owned(),
            parameters: vec![ProductValue::Signed(1)],
        },
    )?;
    assert!(matches!(
        response,
        ProductResponse::Sql {
            result: ProductSqlResult::Rows { .. },
            snapshot: Some(_),
            commit: None,
        }
    ));
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn dispatcher_records_admission_execution_planning_and_durability_clocks()
-> Result<(), Box<dyn Error>> {
    let path = temporary("telemetry-clocks");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let mut session = direct_session("telemetry-clocks", ProductAuthorization::ALL)?;
    let request = memory_context(&session, 61, 0);
    product.dispatch(
        &mut session,
        &request,
        ProductOperation::StructureSet {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: None,
        },
    )?;
    let planning = context(&session, 62, 0);
    let table = memory_context(&session, 63, 0);
    product.dispatch(
        &mut session,
        &table,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE telemetry_items (id BIGINT PRIMARY KEY)".into(),
            parameters: vec![],
        },
    )?;
    product.dispatch(
        &mut session,
        &planning,
        ProductOperation::PrepareSql {
            statement: "SELECT id FROM telemetry_items LIMIT 1".into(),
        },
    )?;
    let snapshot = product.telemetry_snapshot(0)?;
    for id in [
        MetricId::AdmissionMicros,
        MetricId::EngineExecutionMicros,
        MetricId::PlanningMicros,
        MetricId::WalAppendMicros,
        MetricId::PageSynchronizationMicros,
        MetricId::WalSynchronizationMicros,
        MetricId::DurabilityMicros,
    ] {
        let row = snapshot
            .metrics
            .iter()
            .find(|row| row.descriptor.id == id)
            .ok_or("timing row missing")?;
        assert!(matches!(row.value, MetricValue::Histogram { count, .. } if count >= 1));
    }
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}
