// SPDX-License-Identifier: AGPL-3.0-only

//! Local authority tests for the owned, index-scoped ANN read view.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_runtime::{
    AnnSearchOptions, GovernorClassLimit, GovernorMode, HardwareProfile, HnswConfig,
    NativeDatabase, NativeExecutionPool, NativeGovernorPolicy, NativeResourceGovernor,
    NativeRuntimeError, Vector, VectorMetric, WorkloadClass,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn create() -> Result<Self, std::io::Error> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "hyphae-ann-read-view-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn governor_policy(profile: &HardwareProfile) -> NativeGovernorPolicy {
    let workers = 2;
    let memory_bytes = 512 * 1_024 * 1_024;
    let classes = [
        WorkloadClass::ForegroundPoint,
        WorkloadClass::ForegroundBounded,
        WorkloadClass::Mutation,
        WorkloadClass::Bulk,
        WorkloadClass::Maintenance,
        WorkloadClass::Recovery,
        WorkloadClass::Administrative,
    ];
    NativeGovernorPolicy {
        schema: "hyphae-native-governor-policy-v1".to_owned(),
        mode: GovernorMode::Mixed,
        hardware_fingerprint: profile.fingerprint.clone(),
        calibration_cache_key: "ann-read-view-test".to_owned(),
        calibrated_worker_limit: workers,
        reserved_system_threads: 0,
        schedulable_compute_threads: workers,
        io_slots: workers,
        memory_bytes,
        memory_headroom_percent: 0,
        admission_queue_capacity: 32,
        foreground_burst_limit: 8,
        class_limits: classes
            .into_iter()
            .map(|class| GovernorClassLimit {
                class,
                compute_threads: workers,
                io_slots: workers,
                memory_bytes,
            })
            .collect(),
    }
}

fn seed() -> Result<
    (
        TestDirectory,
        NativeDatabase,
        ObjectId,
        Arc<NativeResourceGovernor>,
    ),
    TestError,
> {
    let temporary = TestDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let selected = ObjectId::new(101)?;
    let unrelated = ObjectId::new(102)?;
    let config = HnswConfig::new(4, 16, 8, 32, 7)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: 64,
        consolidate_after_deltas: 32,
        retain_generations: 1,
    };
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index_with_lifecycle(
        selected,
        "selected",
        2,
        VectorMetric::SquaredL2,
        config,
        lifecycle,
    )?;
    create.create_vector_index_with_lifecycle(
        unrelated,
        "unrelated",
        2,
        VectorMetric::SquaredL2,
        config,
        lifecycle,
    )?;
    create.commit()?;
    for (index, offset) in [(selected, 0.0_f32), (unrelated, 100.0)] {
        let vectors = (1..=8_u8)
            .map(|id| {
                Ok((
                    ObjectId::new(u128::from(id))?,
                    Vector::new([f32::from(id) + offset, offset])?,
                ))
            })
            .collect::<Result<Vec<_>, TestError>>()?;
        let plan = database.plan_initial_ann_bulk(index, vectors, 2)?;
        database.publish_initial_ann_bulk(plan, DurabilityClass::Strict)?;
    }
    let profile = HardwareProfile::discover(&path)?;
    let policy = governor_policy(&profile);
    let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
    let pool = Arc::new(NativeExecutionPool::new(&profile, &policy)?);
    database.set_resource_governor_with_execution_pool(
        Arc::clone(&governor),
        pool,
        Duration::ZERO,
    )?;
    Ok((temporary, database, selected, governor))
}

fn options() -> Result<AnnSearchOptions, TestError> {
    Ok(AnnSearchOptions::new(4, 16, Some(16))?)
}

#[test]
fn one_open_hydrates_once_and_ten_thousand_queries_never_touch_storage() -> Result<(), TestError> {
    let _guard = test_lock();
    let (_temporary, database, index, _governor) = seed()?;
    let materialization = NativeDatabase::process_materialization_observation();
    let (view, open) = database.open_ann_read_view(index)?;
    assert_eq!(open.hydration_restore_count, 1);
    assert_ne!(open.routing_policy_identity, [0; 32]);
    assert!(open.observed_physical_entries > 0);
    let reads_after_open = database.physical_observation()?.physical_page_reads;
    let restores_after_open = NativeDatabase::process_ann_index_restore_count();
    let query = Vector::new([4.0, 0.0])?;
    for _ in 0..10_000 {
        let receipt = view.search_selected(&query, options()?, 2)?;
        assert!(!receipt.hydration_performed);
        assert_eq!(receipt.physical_page_reads, 0);
        assert_eq!(receipt.restore_count, 0);
    }
    assert_eq!(
        database.physical_observation()?.physical_page_reads,
        reads_after_open
    );
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        restores_after_open
    );
    let observed = NativeDatabase::process_materialization_observation();
    assert_eq!(observed.full_state_loads, materialization.full_state_loads);
    assert_eq!(
        observed.full_catalog_loads,
        materialization.full_catalog_loads
    );
    Ok(())
}

#[test]
fn clones_retain_memory_reconfiguration_is_guarded_and_last_drop_releases_once()
-> Result<(), TestError> {
    let _guard = test_lock();
    let (_temporary, mut database, index, governor) = seed()?;
    let baseline = governor.usage_snapshot().memory_bytes;
    let (view, open) = database.open_ann_read_view(index)?;
    assert!(open.retained_memory_bytes > 0);
    let charged = governor.usage_snapshot().memory_bytes;
    assert!(charged >= baseline.saturating_add(open.retained_memory_bytes));
    let clone = view.clone();
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, charged);
    assert!(matches!(
        database.clear_resource_governor(),
        Err(NativeRuntimeError::OutstandingAnnReadViews { count: 1 })
    ));
    drop(clone);
    assert_eq!(governor.usage_snapshot().memory_bytes, baseline);
    database.clear_resource_governor()?;
    Ok(())
}

#[test]
fn cancellation_does_not_poison_view_and_root_advance_vacuum_reopen_are_stable()
-> Result<(), TestError> {
    let _guard = test_lock();
    let (temporary, mut database, index, governor) = seed()?;
    let query = Vector::new([4.0, 0.0])?;
    let (old_view, old_open) = database.open_ann_read_view(index)?;
    let old = old_view.search_selected(&query, options()?, 2)?.search;
    let cancellation = governor.cancellation_token();
    cancellation.cancel();
    assert!(matches!(
        old_view.search_selected_with_cancellation(&query, options()?, 2, &cancellation),
        Err(NativeRuntimeError::ResourceQueue(
            hyphae_native_runtime::GovernorQueueError::Cancelled
        ))
    ));
    assert_eq!(old_view.search_selected(&query, options()?, 2)?.search, old);

    let mut update = database.begin(2, DurabilityClass::Strict)?;
    update.upsert_vector(index, ObjectId::new(4)?, Vector::new([40.0, 0.0])?)?;
    update.commit()?;
    let (new_view, new_open) = database.open_ann_read_view(index)?;
    assert_ne!(new_open.view_identity, old_open.view_identity);
    assert_eq!(old_view.search_selected(&query, options()?, 2)?.search, old);
    assert!(database.vacuum_pages()?.applied);
    assert_eq!(old_view.search_selected(&query, options()?, 2)?.search, old);
    drop(new_view);
    drop(database);

    let mut reopened = NativeDatabase::open(temporary.0.join("data"))?;
    let profile = HardwareProfile::discover(temporary.0.join("data"))?;
    let policy = governor_policy(&profile);
    let reopened_governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
    reopened.set_resource_governor_with_execution_pool(
        reopened_governor,
        Arc::new(NativeExecutionPool::new(&profile, &policy)?),
        Duration::ZERO,
    )?;
    let (_reopened_view, reopened_open) = reopened.open_ann_read_view(index)?;
    assert_eq!(reopened_open.view_identity, new_open.view_identity);
    Ok(())
}

#[test]
fn dropped_database_invalidates_owned_view_without_leaking_memory() -> Result<(), TestError> {
    let _guard = test_lock();
    let (_temporary, database, index, governor) = seed()?;
    let baseline = governor.usage_snapshot().memory_bytes;
    let (view, _) = database.open_ann_read_view(index)?;
    assert!(governor.usage_snapshot().memory_bytes > baseline);
    drop(database);
    assert!(matches!(
        view.search_selected(&Vector::new([4.0, 0.0])?, options()?, 2),
        Err(NativeRuntimeError::AnnReadViewDatabaseClosed)
    ));
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, baseline);
    Ok(())
}

#[test]
fn query_scratch_is_admitted_before_any_pool_execution() -> Result<(), TestError> {
    let _guard = test_lock();
    let (_temporary, database, index, governor) = seed()?;
    let (view, _) = database.open_ann_read_view(index)?;
    let query = Vector::new([4.0, 0.0])?;
    let scratch = view
        .search_selected(&query, options()?, 2)?
        .query_scratch_bytes;
    let usage = governor.usage_snapshot();
    let available = governor
        .policy()
        .memory_bytes
        .saturating_sub(usage.memory_bytes);
    assert!(available >= scratch);
    let held = governor.try_admit_owned(
        WorkloadClass::Bulk,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 0,
            io_slots: 0,
            memory_bytes: available.saturating_sub(scratch).saturating_add(1),
        },
    )?;
    let completed_before = database
        .execution_pool()
        .ok_or("missing execution pool")?
        .completed_jobs();
    assert!(matches!(
        view.search_selected(&query, options()?, 2),
        Err(NativeRuntimeError::ResourceAdmission(_))
    ));
    assert_eq!(
        database
            .execution_pool()
            .ok_or("missing execution pool")?
            .completed_jobs(),
        completed_before
    );
    drop(held);
    assert!(view.search_selected(&query, options()?, 2).is_ok());
    Ok(())
}

#[test]
fn explicit_worker_budget_queues_concurrency_without_admission_failures() -> Result<(), TestError> {
    let _guard = test_lock();
    let (_temporary, database, index, governor) = seed()?;
    let (view, _) = database.open_ann_read_view(index)?;
    let query = Vector::new([4.0, 0.0])?;
    assert!(matches!(
        view.search_selected_with_worker_budget(&query, options()?, 2, 0, Duration::from_secs(1)),
        Err(NativeRuntimeError::InvalidAnnReadViewWorkerLimit { requested: 0, .. })
    ));
    assert!(matches!(
        view.search_selected_with_worker_budget(&query, options()?, 2, 3, Duration::from_secs(1)),
        Err(NativeRuntimeError::InvalidAnnReadViewWorkerLimit {
            requested: 3,
            maximum: 2,
        })
    ));

    for concurrency in [8_usize, 32] {
        let barrier = Arc::new(Barrier::new(concurrency));
        let search_options = options()?;
        let handles = (0..concurrency)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let view = view.clone();
                let query = query.clone();
                thread::spawn(move || {
                    barrier.wait();
                    view.search_selected_with_worker_budget(
                        &query,
                        search_options,
                        2,
                        1,
                        Duration::from_secs(5),
                    )
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let receipt = handle.join().map_err(|_| "ANN query worker panicked")??;
            assert_eq!(receipt.requested_worker_limit, 1);
            assert_eq!(receipt.execution.request.compute_threads, 1);
        }
        assert_eq!(governor.usage_snapshot().compute_threads, 0);
    }
    Ok(())
}
