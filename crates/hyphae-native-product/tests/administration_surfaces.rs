// SPDX-License-Identifier: Apache-2.0

//! Integration coverage for bounded administration, telemetry, doctor, and backup surfaces.

use std::{error::Error, fs, path::PathBuf, time::Duration};

use hyphae_native_product::{
    BackupPhase, BackupProductError, BackupRequest, DoctorRequest, DoctorStatus, MetricId,
    MetricValue, NativeProduct, ProductAuthorization, ProductExplain, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductResponse, ProductSession, ProductSessionId,
    ProgressControl, RestorePhase, RestoreRequest, SQL_PLAN_TEXT_VERSION,
    TELEMETRY_HISTOGRAM_BOUNDS_MICROS, TelemetryConfig, TelemetryEvent, TelemetryEventKind,
    TelemetryRegistry, TimingClass, doctor, restore,
};
use hyphae_native_runtime::{
    AnnSearchOptions, ConvergenceLimits, ConvergencePlan, ConvergenceSource, HnswConfig,
    NativeDatabase, NativeHybridFusion, NativeHybridRequest, NativeVectorBranch, StructureSource,
    Vector, VectorMetric,
};
use hyphae_native_types::DurabilityClass;

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-native-product-admin-{name}-{}",
        std::process::id()
    ))
}

#[test]
fn telemetry_is_fixed_bounded_and_redacted() -> Result<(), Box<dyn Error>> {
    let config = TelemetryConfig::new(1).ok_or("valid telemetry config rejected")?;
    let registry = TelemetryRegistry::new(config);
    registry.increment(MetricId::Requests, 2);
    registry.record_timing(TimingClass::EngineExecution, Duration::from_micros(51));
    registry.record_event(TelemetryEvent {
        captured_at_micros: 1,
        kind: TelemetryEventKind::Backup,
    });
    registry.record_event(TelemetryEvent {
        captured_at_micros: 2,
        kind: TelemetryEventKind::Doctor,
    });
    let snapshot = registry.snapshot(3, None);

    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].kind, TelemetryEventKind::Doctor);
    assert_eq!(snapshot.dropped_events, 1);
    assert!(
        snapshot
            .metrics
            .iter()
            .all(|row| row.descriptor.labels.is_empty())
    );
    let engine = snapshot
        .metrics
        .iter()
        .find(|row| row.descriptor.id == MetricId::EngineExecutionMicros)
        .ok_or("engine timing missing")?;
    let MetricValue::Histogram { count, buckets, .. } = &engine.value else {
        return Err("engine timing is not a histogram".into());
    };
    assert_eq!(*count, 1);
    assert_eq!(buckets.len(), TELEMETRY_HISTOGRAM_BOUNDS_MICROS.len() + 1);
    assert_eq!(buckets[2], 1);
    Ok(())
}

#[test]
fn telemetry_snapshots_saturate_and_restart_sessions_without_changing_process_identity()
-> Result<(), Box<dyn Error>> {
    let registry = TelemetryRegistry::new(TelemetryConfig::new(0).ok_or("invalid config")?);
    registry.increment(MetricId::Requests, u64::MAX);
    registry.increment(MetricId::Requests, 1);
    registry.record_timing(TimingClass::WalAppend, Duration::from_micros(u64::MAX));
    let first = registry.snapshot(1, None);
    let second_registry = TelemetryRegistry::new(TelemetryConfig::default());
    let second = second_registry.snapshot(2, None);
    assert_eq!(first.process_start_identity, second.process_start_identity);
    assert_ne!(first.session_start_identity, second.session_start_identity);
    let requests = first
        .metrics
        .iter()
        .find(|row| row.descriptor.id == MetricId::Requests)
        .ok_or("request counter missing")?;
    assert_eq!(requests.value, MetricValue::Counter(u64::MAX));
    Ok(())
}

#[test]
fn doctor_distinguishes_busy_and_healthy_verified_open() -> Result<(), Box<dyn Error>> {
    let path = temporary("doctor");
    let _ = fs::remove_dir_all(&path);
    let database = NativeDatabase::create(&path)?;
    let request = DoctorRequest::new(&path, 0)?;
    let busy = doctor(&request);
    assert_eq!(busy.status, DoctorStatus::Busy);
    assert!(!busy.verified_open);
    drop(database);

    let healthy = doctor(&request);
    assert_eq!(healthy.status, DoctorStatus::Healthy);
    assert!(healthy.verified_open);
    assert!(healthy.snapshot_verified);
    assert!(healthy.directory_lineage.is_some());
    let restarted = doctor(&request);
    assert_eq!(
        healthy.process_start_identity,
        restarted.process_start_identity
    );
    assert_ne!(
        healthy.session_start_identity,
        restarted.session_start_identity
    );
    fs::remove_dir_all(path)?;

    let malformed = temporary("doctor-corrupt");
    let _ = fs::remove_dir_all(&malformed);
    fs::create_dir(&malformed)?;
    assert_eq!(
        doctor(&DoctorRequest::new(&malformed, 0)?).status,
        DoctorStatus::Corrupt
    );
    fs::remove_dir_all(malformed)?;

    let invalid_os_path = PathBuf::from("invalid\0doctor-path");
    assert_eq!(
        doctor(&DoctorRequest::new(invalid_os_path, 0)?).status,
        DoctorStatus::Io
    );
    Ok(())
}

#[test]
fn product_doctor_and_telemetry_share_restart_identity() -> Result<(), Box<dyn Error>> {
    let root = temporary("doctor-telemetry-identity");
    let observed = root.join("observed");
    let target = root.join("target");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;
    let product = NativeProduct::create(&observed)?;
    let target_database = NativeDatabase::create(&target)?;
    drop(target_database);
    let telemetry = product.telemetry_snapshot(7)?;
    let report = product.doctor(&DoctorRequest::new(&target, 7)?);
    assert_eq!(report.status, DoctorStatus::Healthy);
    assert_eq!(
        report.process_start_identity,
        telemetry.process_start_identity
    );
    assert_eq!(
        report.session_start_identity,
        telemetry.session_start_identity
    );
    drop(product);
    let reopened = NativeProduct::open(&observed)?;
    let restarted = reopened.telemetry_snapshot(8)?;
    assert_eq!(
        telemetry.process_start_identity,
        restarted.process_start_identity
    );
    assert_ne!(
        telemetry.session_start_identity,
        restarted.session_start_identity
    );
    drop(reopened);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn doctor_rejects_format_page_wal_manifest_and_blob_corruption() -> Result<(), Box<dyn Error>> {
    for (name, relative) in [
        ("format", "FORMAT"),
        ("page", "pages.hydb"),
        ("wal", "wal.hywal"),
    ] {
        let path = temporary(&format!("doctor-matrix-{name}"));
        let _ = fs::remove_dir_all(&path);
        let mut database = NativeDatabase::create(&path)?;
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.set(b"key".to_vec(), b"value".to_vec(), None)?;
        transaction.commit()?;
        database.checkpoint()?;
        drop(database);
        let file = path.join(relative);
        let mut bytes = fs::read(&file)?;
        let offset = if name == "wal" {
            bytes.len() / 2
        } else {
            bytes.len().saturating_sub(1)
        };
        bytes[offset] ^= 1;
        fs::write(&file, bytes)?;
        assert_eq!(
            doctor(&DoctorRequest::new(&path, 0)?).status,
            DoctorStatus::Corrupt,
            "{name} corruption was accepted"
        );
        fs::remove_dir_all(path)?;
    }

    let manifest_path = temporary("doctor-matrix-manifest");
    let _ = fs::remove_dir_all(&manifest_path);
    let mut database = NativeDatabase::create(&manifest_path)?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    transaction.set(b"key".to_vec(), b"value".to_vec(), None)?;
    transaction.commit()?;
    database.checkpoint()?;
    drop(database);
    let manifest = fs::read_dir(manifest_path.join("roots"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "hyroot")
        })
        .ok_or("manifest file missing")?;
    let mut bytes = fs::read(&manifest)?;
    bytes[0] ^= 1;
    fs::write(manifest, bytes)?;
    assert_eq!(
        doctor(&DoctorRequest::new(&manifest_path, 0)?).status,
        DoctorStatus::Corrupt
    );
    fs::remove_dir_all(manifest_path)?;

    let blob_path = temporary("doctor-matrix-blob");
    let _ = fs::remove_dir_all(&blob_path);
    let mut database = NativeDatabase::create(&blob_path)?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    transaction.set(b"large".to_vec(), vec![9; 10_000], None)?;
    transaction.commit()?;
    drop(database);
    let blob = fs::read_dir(blob_path.join("blobs"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "hyblob")
        })
        .ok_or("blob file missing")?;
    let mut bytes = fs::read(&blob)?;
    bytes[0] ^= 1;
    fs::write(blob, bytes)?;
    assert_eq!(
        doctor(&DoctorRequest::new(&blob_path, 0)?).status,
        DoctorStatus::Corrupt
    );
    fs::remove_dir_all(blob_path)?;

    let mixed_path = temporary("doctor-matrix-mixed");
    let _ = fs::remove_dir_all(&mixed_path);
    let database = NativeDatabase::create(&mixed_path)?;
    drop(database);
    fs::create_dir(mixed_path.join("indexes"))?;
    assert_eq!(
        doctor(&DoctorRequest::new(&mixed_path, 0)?).status,
        DoctorStatus::Corrupt
    );
    fs::remove_dir_all(mixed_path)?;
    Ok(())
}

#[test]
fn sql_explain_is_versioned_bounded_opaque_text() -> Result<(), Box<dyn Error>> {
    let path = temporary("explain");
    let _ = fs::remove_dir_all(&path);
    let mut runtime = NativeDatabase::create(&path)?;
    let mut transaction = runtime.begin_sql(0, DurabilityClass::Memory)?;
    transaction.execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)", &[])?;
    transaction.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let ProductExplain::SqlPlanText(plan) = product
        .administration()
        .explain_sql("SELECT id FROM items WHERE id = 1")?
    else {
        return Err("SQL explain returned a non-SQL strategy".into());
    };
    assert_eq!(plan.version, SQL_PLAN_TEXT_VERSION);
    assert!(plan.text.starts_with("PrimaryKeyLookup("));
    assert_eq!(plan.catalog_version, 3);
    assert!(!plan.executed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn typed_explain_goldens_cover_convergence_ann_and_hybrid() -> Result<(), Box<dyn Error>> {
    let path = temporary("typed-explain");
    let _ = fs::remove_dir_all(&path);
    let mut database = NativeDatabase::create(&path)?;
    let lexical = hyphae_native_product::ObjectId::new(20)?;
    let vectors = hyphae_native_product::ObjectId::new(21)?;
    let object = hyphae_native_product::ObjectId::new(22)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(
        vectors,
        "vectors",
        2,
        VectorMetric::SquaredL2,
        HnswConfig::new(4, 16, 8, 32, 7)?,
    )?;
    seed.index_document(lexical, object.get().to_be_bytes().to_vec(), "rust engine")?;
    seed.upsert_vector(vectors, object, Vector::new([0.0, 0.0])?)?;
    seed.commit()?;
    let query = Vector::new([0.0, 0.0])?;
    let receipt =
        database
            .snapshot(0)?
            .search_ann(vectors, &query, AnnSearchOptions::new(1, 8, None)?)?;
    drop(database);

    let product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
    let snapshot = product.snapshot_bounded(0)?;
    let convergence = snapshot.explain_convergence(&ConvergencePlan {
        sources: vec![ConvergenceSource::Structure(StructureSource::Scalar {
            key: object.get().to_be_bytes().to_vec(),
        })],
        aggregates: vec![],
        limits: ConvergenceLimits::default(),
    })?;
    assert!(matches!(
        convergence,
        ProductExplain::Convergence(ref value)
            if value.strategies == [hyphae_native_product::ProductConvergenceStrategy::ScalarLookup]
    ));

    let ann = hyphae_native_product::explain_ann(&receipt);
    assert!(matches!(ann, ProductExplain::Ann(ref value) if value.index == vectors));
    let hybrid_request = NativeHybridRequest {
        lexical_index: lexical,
        lexical_query: "rust",
        lexical_limit: 2,
        vector_index: vectors,
        vector_query: &query,
        vector_branch: NativeVectorBranch::Ann(AnnSearchOptions::new(1, 8, Some(1))?),
        vector_limit: 2,
        fusion: NativeHybridFusion {
            lexical_weight: 2,
            vector_weight: 3,
            limit: 1,
        },
    };
    let hybrid = hyphae_native_product::explain_hybrid(&hybrid_request);
    assert!(matches!(
        hybrid,
        ProductExplain::Hybrid(ref value)
            if value.lexical_index == lexical && value.vector_index == vectors
    ));
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn backup_restore_has_safe_cancellation_and_doctor_after_restore() -> Result<(), Box<dyn Error>> {
    let root = temporary("backup");
    let source = root.join("source");
    let backup_path = root.join("backup");
    let cancelled_restore = root.join("cancelled");
    let restored_path = root.join("restored");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;

    let mut runtime = NativeDatabase::create(&source)?;
    let mut transaction = runtime.begin(0, DurabilityClass::Strict)?;
    transaction.set(b"key".to_vec(), b"value".to_vec(), None)?;
    transaction.commit()?;
    drop(runtime);

    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&source)?;
    let backup_request = BackupRequest::new(&backup_path)?;
    let mut phases = Vec::new();
    product.administration().backup(&backup_request, |phase| {
        phases.push(phase);
        ProgressControl::Continue
    })?;
    assert_eq!(phases.last(), Some(&BackupPhase::Complete));
    drop(product);

    let cancelled_request = RestoreRequest::new(&backup_path, &cancelled_restore)?;
    let error = restore(&cancelled_request, |phase| {
        if phase == RestorePhase::RestoringAndPromoting {
            ProgressControl::Cancel
        } else {
            ProgressControl::Continue
        }
    })
    .err()
    .ok_or("restore cancellation was ignored")?;
    assert_eq!(error, BackupProductError::Cancelled);
    assert!(!cancelled_restore.exists());

    let request = RestoreRequest::new(&backup_path, &restored_path)?;
    let observer_path = root.join("observer");
    let mut observer = NativeProduct::create(&observer_path)?;
    let restored = observer
        .administration()
        .restore(&request, |_| ProgressControl::Continue)?;
    assert_eq!(restored.doctor.status, DoctorStatus::Healthy);
    assert!(restored.doctor.snapshot_verified);
    assert_eq!(restored.phases.last(), Some(&RestorePhase::Complete));
    let reopened = NativeDatabase::open(&restored_path)?;
    assert_eq!(reopened.snapshot(0)?.get(b"key"), Some(b"value".as_slice()));
    drop(reopened);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn backup_restore_preserves_terminal_legacy_bearer_revocation() -> Result<(), Box<dyn Error>> {
    let root = temporary("legacy-revoked-backup");
    let source = root.join("source");
    let backup_path = root.join("backup");
    let restored_path = root.join("restored");
    let observer_path = root.join("observer");
    let _ignored = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;
    let legacy = b"legacy-backup-canary-0123456789abcdef";
    let mut product = NativeProduct::create(&source)?;
    drop(product);
    product = NativeProduct::open_offline_owner(&source)?;
    let started =
        product.start_legacy_bearer_migration_offline("Migrated owner", "canonical", legacy, 1)?;
    let canonical = started.secret.expose_secret().to_owned();
    product.activate_legacy_bearer_migration_offline(
        started.key_id,
        &canonical,
        started.authorization_epoch,
        "Migrated owner",
        "canonical",
        legacy,
        2,
    )?;
    let owner = product.authenticate_api_key(&canonical, 0)?;
    product.revoke_legacy_bearer_idempotent(&owner, 42, 3)?;
    product
        .administration()
        .backup(&BackupRequest::new(&backup_path)?, |_| {
            ProgressControl::Continue
        })?;
    drop(product);

    let mut observer = NativeProduct::create(&observer_path)?;
    observer
        .administration()
        .restore(&RestoreRequest::new(&backup_path, &restored_path)?, |_| {
            ProgressControl::Continue
        })?;
    drop(observer);
    let restored = NativeProduct::open(&restored_path)?;
    assert_eq!(
        restored.legacy_bearer_migration_inspection()?.state,
        hyphae_native_product::LegacyBearerState::Revoked
    );
    drop(restored);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn restore_product_operation_runs_complete_progress_and_doctor() -> Result<(), Box<dyn Error>> {
    let root = temporary("restore-operation");
    let source = root.join("source");
    let backup_path = root.join("backup");
    let restored_path = root.join("restored");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;
    let mut runtime = NativeDatabase::create(&source)?;
    let mut transaction = runtime.begin(0, DurabilityClass::Strict)?;
    transaction.set(b"complete".to_vec(), b"state".to_vec(), None)?;
    transaction.commit()?;
    drop(runtime);
    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&source)?;
    product
        .administration()
        .backup(&BackupRequest::new(&backup_path)?, |_| {
            ProgressControl::Continue
        })?;

    let principal = ProductPrincipal::new("restore-test").ok_or("invalid principal")?;
    let mut session = ProductSession::new(
        ProductSessionId::new(1).ok_or("invalid session")?,
        principal.clone(),
        ProductAuthorization::ALL,
    );
    let context =
        ProductRequestContext::new(100, session.id(), 0, principal, ProductAuthorization::ALL);
    let response = product.dispatch(
        &mut session,
        &context,
        ProductOperation::Restore(RestoreRequest::new(&backup_path, &restored_path)?),
    )?;
    let ProductResponse::Restore(restored) = response else {
        return Err("restore operation returned the wrong response".into());
    };
    assert_eq!(restored.doctor.status, DoctorStatus::Healthy);
    assert!(restored.doctor.snapshot_verified);
    let reopened = NativeDatabase::open(&restored_path)?;
    assert_eq!(
        reopened.snapshot(0)?.get(b"complete"),
        Some(b"state".as_slice())
    );
    drop(reopened);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn restore_response_limit_fails_before_creating_the_destination() -> Result<(), Box<dyn Error>> {
    let root = temporary("restore-response-limit");
    let source = root.join("source");
    let backup_path = root.join("backup");
    let restored_path = root.join("must-not-exist");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;
    let runtime = NativeDatabase::create(&source)?;
    drop(runtime);
    let mut product = NativeProduct::open_with_preview_default_scalar_migration(&source)?;
    product
        .administration()
        .backup(&BackupRequest::new(&backup_path)?, |_| {
            ProgressControl::Continue
        })?;
    let principal = ProductPrincipal::new("restore-limit-test").ok_or("invalid principal")?;
    let mut session = ProductSession::new(
        ProductSessionId::new(2).ok_or("invalid session")?,
        principal.clone(),
        ProductAuthorization::ALL,
    );
    let request = RestoreRequest::new(&backup_path, &restored_path)?;
    let response_bound = 222
        + request.backup.as_os_str().as_encoded_bytes().len()
        + request.destination.as_os_str().as_encoded_bytes().len();
    let mut context = ProductRequestContext::new(
        101,
        session.id(),
        0,
        principal.clone(),
        ProductAuthorization::ALL,
    );
    context.limits.max_response_bytes = response_bound - 1;
    let Err(error) = product.dispatch(
        &mut session,
        &context,
        ProductOperation::Restore(request.clone()),
    ) else {
        return Err("restore ran despite an insufficient response bound".into());
    };
    assert_eq!(
        error.code(),
        hyphae_native_product::ProductErrorCode::LimitExceeded
    );
    assert!(!restored_path.exists());

    let mut exact =
        ProductRequestContext::new(102, session.id(), 0, principal, ProductAuthorization::ALL);
    exact.limits.max_response_bytes = response_bound;
    assert!(matches!(
        product.dispatch(&mut session, &exact, ProductOperation::Restore(request))?,
        ProductResponse::Restore(_)
    ));
    assert!(restored_path.exists());
    drop(product);
    fs::remove_dir_all(root)?;
    Ok(())
}
