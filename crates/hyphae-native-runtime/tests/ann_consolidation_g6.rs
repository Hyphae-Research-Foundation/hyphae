// SPDX-License-Identifier: AGPL-3.0-only

//! G6 bounded ANN consolidation and interruption evidence.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_runtime::{
    AnnPartitionRoutingMode, AnnSearchOptions, CommitBoundary, GovernorClassLimit, GovernorMode,
    HardwareCpu, HardwareMemory, HardwareOperatingSystem, HardwareProfile, HardwareStorage,
    HnswConfig, NativeDatabase, NativeExecutionPool, NativeGovernorPolicy, NativeResourceGovernor,
    NativeRuntimeError, SnapshotPinId, Vector, VectorMetric, WorkloadClass,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(std::env::temp_dir().join(format!(
            "hyphae-ann-consolidation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn config() -> Result<HnswConfig, TestError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4836)?)
}

fn seed(path: &Path) -> Result<(NativeDatabase, ObjectId), TestError> {
    let index = ObjectId::new(100)?;
    fs::create_dir_all(path.parent().ok_or("missing test parent")?)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(index, "vectors", 2, VectorMetric::SquaredL2, config()?)?;
    seed.upsert_vectors(
        index,
        [
            (ObjectId::new(1)?, Vector::new([0.0, 0.0])?),
            (ObjectId::new(2)?, Vector::new([2.0, 0.0])?),
            (ObjectId::new(3)?, Vector::new([4.0, 0.0])?),
        ],
    )?;
    seed.commit()?;
    Ok((database, index))
}

const PARTITION_COUNT: usize = 4;
const PARTITIONED_VECTOR_COUNT: u16 = 16;

fn partitioned_vectors(count: u16) -> Result<Vec<(ObjectId, Vector)>, TestError> {
    (1..=count)
        .map(|value| {
            Ok((
                ObjectId::new(u128::from(value))?,
                Vector::new([f32::from(value), f32::from(value % 5)])?,
            ))
        })
        .collect()
}

fn seed_partitioned(path: &Path) -> Result<(NativeDatabase, ObjectId), TestError> {
    let index = ObjectId::new(100)?;
    fs::create_dir_all(path.parent().ok_or("missing test parent")?)?;
    let mut database = NativeDatabase::create(path)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index(index, "partitioned", 2, VectorMetric::SquaredL2, config()?)?;
    create.commit()?;
    let plan = database.plan_initial_ann_bulk(
        index,
        partitioned_vectors(PARTITIONED_VECTOR_COUNT)?,
        PARTITION_COUNT,
    )?;
    database.publish_initial_ann_bulk(plan, DurabilityClass::Strict)?;
    Ok((database, index))
}

fn selected_options() -> Result<AnnSearchOptions, TestError> {
    Ok(AnnSearchOptions::new(8, 32, Some(32))?)
}

fn assert_partitioned_routing(
    database: &NativeDatabase,
    index: ObjectId,
    query: &Vector,
) -> Result<(), TestError> {
    let receipt = database.search_ann_selected_latest(index, query, selected_options()?, 2)?;
    assert_ne!(
        receipt.routing_mode,
        AnnPartitionRoutingMode::SingleGenerationFallback
    );
    assert_eq!(receipt.total_partitions, PARTITION_COUNT);
    assert!(!receipt.selected_partitions.is_empty());
    assert!(receipt.selected_partitions.len() <= PARTITION_COUNT);
    Ok(())
}

fn test_governor_policy(profile: &HardwareProfile, workers: u64) -> NativeGovernorPolicy {
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
        calibration_cache_key: "ann-consolidation-test".to_owned(),
        calibrated_worker_limit: workers,
        reserved_system_threads: 0,
        schedulable_compute_threads: workers,
        io_slots: workers,
        memory_bytes,
        memory_headroom_percent: 0,
        admission_queue_capacity: 64,
        foreground_burst_limit: 16,
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

fn portable_execution_profile(path: &Path) -> HardwareProfile {
    // Hardware discovery launches subprocesses on some platforms. Keep this
    // file-lock lifecycle suite fork-free so sibling reopen tests are isolated.
    HardwareProfile {
        schema: "hyphae-native-hardware-profile-v1".to_owned(),
        fingerprint: "1".repeat(64),
        cpu: HardwareCpu {
            architecture: std::env::consts::ARCH.to_owned(),
            logical_processors_available: 2,
            physical_cores_visible: None,
            smt_threads_per_core: None,
            sockets_visible: None,
            numa_nodes_visible: None,
            affinity: "unknown".to_owned(),
            quota_millicores: None,
            instruction_sets: Vec::new(),
            caches: Vec::new(),
            processor_topology: Vec::new(),
            frequency_governors: Vec::new(),
        },
        memory: HardwareMemory {
            total_bytes: Some(1 << 30),
            available_bytes: Some(1 << 30),
            page_size_bytes: Some(4_096),
            huge_page_size_bytes: None,
            huge_pages_total: None,
            numa_nodes: Vec::new(),
        },
        storage: HardwareStorage {
            path: path.display().to_string(),
            filesystem: None,
            device: None,
            mount_options: Vec::new(),
            rotational: None,
            queue_depth: None,
            discard_max_bytes: None,
        },
        operating_system: HardwareOperatingSystem {
            family: std::env::consts::OS.to_owned(),
            kernel_release: "test".to_owned(),
            virtualization: "none".to_owned(),
            local_transports: Vec::new(),
        },
    }
}

#[test]
fn bounded_consolidation_switches_base_and_removes_old_generation_records() -> Result<(), TestError>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed(&path).map_err(|error| format!("seed: {error}"))?;
    let mut update = database.begin(2, DurabilityClass::Strict)?;
    update.upsert_vector(index, ObjectId::new(2)?, Vector::new([3.0, 0.0])?)?;
    update
        .commit()
        .map_err(|error| format!("update: {error}"))?;
    let before = database
        .observe_ann_index(index)
        .map_err(|error| format!("observe: {error}"))?;
    let exact = database
        .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)
        .map_err(|error| format!("exact: {error}"))?;
    let plan = database
        .plan_ann_consolidation(index, 10, 10)
        .map_err(|error| format!("plan: {error}"))?;
    assert_eq!(plan.base_identity(), before.base_identity);
    assert_eq!(plan.captured_delta_count(), 1);

    let receipt = database
        .consolidate_ann(plan, DurabilityClass::Strict)
        .map_err(|error| format!("consolidate: {error}"))?;
    let after = database.observe_ann_index(index)?;
    assert_eq!(after.base_identity, receipt.replacement_base_identity);
    assert_ne!(after.base_identity, before.base_identity);
    assert_eq!(after.base_vector_count, 3);
    assert_eq!(after.effective_vector_count, 3);
    assert_eq!(after.delta_records, 0);
    assert_eq!(after.lifecycle.retain_generations, 1);
    assert!(after.generation_records > after.selected_generation_records);
    assert_eq!(
        database.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)?,
        exact
    );
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.observe_ann_index(index)?, after);
    assert_eq!(
        reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)?,
        exact
    );
    Ok(())
}

#[test]
fn a_plan_preserves_later_object_versions_and_rejects_a_changed_base() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed(&path)?;
    let mut captured = database.begin(2, DurabilityClass::Strict)?;
    captured.upsert_vector(index, ObjectId::new(3)?, Vector::new([5.0, 0.0])?)?;
    captured.commit()?;
    let plan = database.plan_ann_consolidation(index, 10, 10)?;

    let mut later = database.begin(3, DurabilityClass::Strict)?;
    later.upsert_vector(index, ObjectId::new(2)?, Vector::new([8.0, 0.0])?)?;
    later.upsert_vector(index, ObjectId::new(4)?, Vector::new([1.0, 0.0])?)?;
    later.commit()?;
    let receipt = database.consolidate_ann(plan, DurabilityClass::Strict)?;
    assert_eq!(receipt.consumed_delta_records, 1);
    assert_eq!(receipt.preserved_later_delta_records, 2);
    let exact = database.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)?;
    assert_eq!(
        exact.iter().map(|hit| hit.object_id).collect::<Vec<_>>(),
        [
            ObjectId::new(1)?,
            ObjectId::new(4)?,
            ObjectId::new(3)?,
            ObjectId::new(2)?
        ]
    );

    let stale = database.plan_ann_consolidation(index, 10, 10)?;
    let fresh = database.plan_ann_consolidation(index, 10, 10)?;
    database.consolidate_ann(fresh, DurabilityClass::Strict)?;
    assert!(matches!(
        database.consolidate_ann(stale, DurabilityClass::Strict),
        Err(NativeRuntimeError::AnnConsolidationStale)
    ));
    Ok(())
}

#[test]
fn interrupted_consolidation_reopens_to_the_old_or_new_complete_view() -> Result<(), TestError> {
    for boundary in [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ] {
        let temporary = TestDirectory::new();
        let path = temporary.path().join(format!("data-{boundary:?}"));
        let (mut database, index) = seed(&path)?;
        let mut update = database.begin(2, DurabilityClass::Strict)?;
        update.upsert_vector(index, ObjectId::new(2)?, Vector::new([3.0, 0.0])?)?;
        update.commit()?;
        let old = database.observe_ann_index(index)?;
        let exact = database.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)?;
        let plan = database.plan_ann_consolidation(index, 10, 10)?;
        let replacement = plan.replacement_identity();
        assert!(matches!(
            database.consolidate_ann_with_interruption(
                plan,
                DurabilityClass::Strict,
                boundary
            ),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&path)?;
        let recovered = reopened.observe_ann_index(index)?;
        assert!(
            recovered.base_identity == old.base_identity || recovered.base_identity == replacement
        );
        assert_eq!(
            reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 10)?,
            exact
        );
    }
    Ok(())
}

#[test]
fn configured_retention_survives_consolidation_then_pin_safe_vacuum_collection()
-> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    fs::create_dir_all(temporary.path())?;
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(&path)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index_with_lifecycle(
        index,
        "vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
        IncrementalVectorLifecycle {
            delta_max_entries: 8,
            consolidate_after_deltas: 1,
            retain_generations: 1,
        },
    )?;
    create.upsert_vectors(
        index,
        [
            (ObjectId::new(1)?, Vector::new([0.0, 0.0])?),
            (ObjectId::new(2)?, Vector::new([2.0, 0.0])?),
            (ObjectId::new(3)?, Vector::new([4.0, 0.0])?),
        ],
    )?;
    create.commit()?;
    let old_base = database.observe_ann_index(index)?;
    let pin = SnapshotPinId::new(77)?;
    let pinned = database.pin_current(pin, 1)?;

    let mut update = database.begin(2, DurabilityClass::Strict)?;
    update.upsert_vector(index, ObjectId::new(2)?, Vector::new([3.0, 0.0])?)?;
    update.commit()?;
    let plan = database
        .plan_due_ann_consolidation(index, 16)?
        .ok_or("maintenance was not due")?;
    database.consolidate_ann(plan, DurabilityClass::Strict)?;
    let consolidated = database.observe_ann_index(index)?;
    assert_ne!(consolidated.base_identity, old_base.base_identity);
    assert!(consolidated.generation_records > consolidated.selected_generation_records);

    let mut second_update = database.begin(3, DurabilityClass::Strict)?;
    second_update.upsert_vector(index, ObjectId::new(3)?, Vector::new([5.0, 0.0])?)?;
    second_update.commit()?;
    let second_plan = database
        .plan_due_ann_consolidation(index, 16)?
        .ok_or("second maintenance was not due")?;
    database.consolidate_ann(second_plan, DurabilityClass::Strict)?;
    let retained = database.observe_ann_index(index)?;
    assert!(retained.generation_records > retained.selected_generation_records);
    assert!(retained.generation_records <= retained.selected_generation_records * 2);

    let vacuum = database.vacuum_pages()?;
    assert!(vacuum.applied);
    assert_ne!(vacuum.active_generation, pinned.page_generation);
    assert_eq!(
        database.collect_retired_page_generations()?.removed_files,
        0
    );
    let historical = database.open_pinned_snapshot(pin)?;
    assert_eq!(
        historical
            .search_vector_exact(index, &Vector::new([0.0, 0.0])?, 3)?
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [ObjectId::new(1)?, ObjectId::new(2)?, ObjectId::new(3)?]
    );

    database.unpin(pin)?;
    let collection = database.collect_retired_page_generations()?;
    assert_eq!(collection.removed_files, 1);
    Ok(())
}

#[test]
fn partitioned_consolidation_preserves_kind_count_and_routing_after_reopen() -> Result<(), TestError>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed_partitioned(&path)?;
    let query = Vector::new([8.0, 3.0])?;
    assert_partitioned_routing(&database, index, &query)?;

    let mut update = database.begin(2, DurabilityClass::Strict)?;
    update.upsert_vector(index, ObjectId::new(8)?, Vector::new([8.25, 3.25])?)?;
    update.commit()?;
    let expected = database.search_vector_exact_latest(index, &query, 8)?;
    let plan = database.plan_ann_consolidation(index, 32, 8)?;
    let receipt = database.consolidate_ann(plan, DurabilityClass::Strict)?;
    let observed = database.observe_ann_index(index)?;
    assert_eq!(observed.base_identity, receipt.replacement_base_identity);
    assert_eq!(observed.delta_records, 0);
    assert_partitioned_routing(&database, index, &query)?;
    assert_eq!(
        database.search_vector_exact_latest(index, &query, 8)?,
        expected
    );
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_partitioned_routing(&reopened, index, &query)?;
    assert_eq!(
        reopened.search_vector_exact_latest(index, &query, 8)?,
        expected
    );
    Ok(())
}

#[test]
fn partitioned_consolidation_identity_is_independent_of_worker_count() -> Result<(), TestError> {
    let serial_directory = TestDirectory::new();
    let parallel_directory = TestDirectory::new();
    let (mut serial, serial_index) = seed_partitioned(&serial_directory.path().join("data"))?;
    let (mut parallel, parallel_index) = seed_partitioned(&parallel_directory.path().join("data"))?;
    let replacement = Vector::new([7.5, 2.5])?;
    for (database, index) in [(&mut serial, serial_index), (&mut parallel, parallel_index)] {
        let mut update = database.begin(2, DurabilityClass::Strict)?;
        update.upsert_vector(index, ObjectId::new(7)?, replacement.clone())?;
        update.commit()?;
    }

    let profile = portable_execution_profile(parallel_directory.path());
    let workers = u64::try_from(
        profile
            .cpu
            .logical_processors_available
            .clamp(1, PARTITION_COUNT),
    )?;
    let policy = test_governor_policy(&profile, workers);
    let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
    let execution_pool = Arc::new(NativeExecutionPool::new(&profile, &policy)?);
    parallel.set_resource_governor_with_execution_pool(
        governor,
        Arc::clone(&execution_pool),
        Duration::ZERO,
    )?;

    let serial_plan = serial.plan_ann_consolidation(serial_index, 32, 8)?;
    let completed_before = execution_pool.completed_jobs();
    let parallel_plan = parallel.plan_ann_consolidation(parallel_index, 32, 8)?;
    if workers > 1 {
        assert!(execution_pool.completed_jobs() > completed_before);
    }
    assert_eq!(
        serial_plan.replacement_identity(),
        parallel_plan.replacement_identity()
    );
    let serial_receipt = serial.consolidate_ann(serial_plan, DurabilityClass::Strict)?;
    let parallel_receipt = parallel.consolidate_ann(parallel_plan, DurabilityClass::Strict)?;
    assert_eq!(
        serial_receipt.replacement_base_identity,
        parallel_receipt.replacement_base_identity
    );
    let query = Vector::new([7.5, 2.5])?;
    assert_partitioned_routing(&serial, serial_index, &query)?;
    assert_partitioned_routing(&parallel, parallel_index, &query)?;
    Ok(())
}

#[test]
fn cancelled_partitioned_consolidation_releases_governor_without_a_candidate()
-> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed_partitioned(&path)?;
    let mut update = database.begin(2, DurabilityClass::Strict)?;
    update.upsert_vector(index, ObjectId::new(8)?, Vector::new([8.25, 3.25])?)?;
    update.commit()?;

    let profile = portable_execution_profile(temporary.path());
    let policy = test_governor_policy(&profile, 2);
    let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
    let execution_pool = Arc::new(NativeExecutionPool::new(&profile, &policy)?);
    database.set_resource_governor_with_execution_pool(
        Arc::clone(&governor),
        execution_pool,
        Duration::ZERO,
    )?;
    let cancellation = governor.cancellation_token();
    cancellation.cancel();
    let before = database.observe_ann_index(index)?;
    assert!(matches!(
        database.plan_ann_consolidation_with_cancellation(index, 32, 8, &cancellation),
        Err(NativeRuntimeError::Ann(
            hyphae_native_ann::AnnError::BuildCancelled
        ))
    ));
    assert_eq!(governor.usage_snapshot().compute_threads, 0);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    assert_eq!(database.observe_ann_index(index)?, before);
    Ok(())
}

#[test]
fn partitioned_consolidation_preserves_later_same_object_delta() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed_partitioned(&path)?;
    let object_id = ObjectId::new(8)?;
    let mut captured = database.begin(2, DurabilityClass::Strict)?;
    captured.upsert_vector(index, object_id, Vector::new([8.25, 3.25])?)?;
    captured.commit()?;
    let plan = database.plan_ann_consolidation(index, 32, 8)?;

    let latest_vector = Vector::new([0.25, 0.5])?;
    let mut later = database.begin(3, DurabilityClass::Strict)?;
    later.upsert_vector(index, object_id, latest_vector.clone())?;
    later.commit()?;
    let receipt = database.consolidate_ann(plan, DurabilityClass::Strict)?;
    assert_eq!(receipt.consumed_delta_records, 0);
    assert_eq!(receipt.preserved_later_delta_records, 1);
    assert_eq!(
        database.search_vector_exact_latest(index, &latest_vector, 1)?[0].object_id,
        object_id
    );
    let selected =
        database.search_ann_selected_latest(index, &latest_vector, selected_options()?, 2)?;
    assert_eq!(selected.exact_delta_candidates, 1);
    assert_ne!(
        selected.routing_mode,
        AnnPartitionRoutingMode::SingleGenerationFallback
    );
    assert_eq!(selected.total_partitions, PARTITION_COUNT);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.search_vector_exact_latest(index, &latest_vector, 1)?[0].object_id,
        object_id
    );
    assert_partitioned_routing(&reopened, index, &latest_vector)?;
    Ok(())
}

#[test]
fn consolidation_receipt_counts_only_matching_captured_sequences() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed(&path)?;
    let replaced_object = ObjectId::new(2)?;
    let consumed_object = ObjectId::new(3)?;
    let mut captured = database.begin(2, DurabilityClass::Strict)?;
    captured.upsert_vector(index, replaced_object, Vector::new([3.0, 0.0])?)?;
    captured.upsert_vector(index, consumed_object, Vector::new([5.0, 0.0])?)?;
    captured.commit()?;
    let plan = database.plan_ann_consolidation(index, 10, 10)?;

    let mut later = database.begin(3, DurabilityClass::Strict)?;
    later.upsert_vector(index, replaced_object, Vector::new([9.0, 0.0])?)?;
    later.commit()?;
    let receipt = database.consolidate_ann(plan, DurabilityClass::Strict)?;
    assert_eq!(receipt.consumed_delta_records, 1);
    assert_eq!(receipt.preserved_later_delta_records, 1);
    assert_eq!(database.observe_ann_index(index)?.delta_records, 1);
    Ok(())
}

#[test]
fn interrupted_partitioned_consolidation_reopens_old_or_new_partitioned_view()
-> Result<(), TestError> {
    for boundary in [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ] {
        let temporary = TestDirectory::new();
        let path = temporary.path().join(format!("data-{boundary:?}"));
        let (mut database, index) = seed_partitioned(&path)?;
        let query = Vector::new([9.0, 4.0])?;
        let mut update = database.begin(2, DurabilityClass::Strict)?;
        update.upsert_vector(index, ObjectId::new(9)?, Vector::new([9.25, 4.25])?)?;
        update.commit()?;
        let old_identity = database.observe_ann_index(index)?.base_identity;
        let expected = database.search_vector_exact_latest(index, &query, 8)?;
        let plan = database.plan_ann_consolidation(index, 32, 8)?;
        let replacement_identity = plan.replacement_identity();
        assert!(matches!(
            database.consolidate_ann_with_interruption(
                plan,
                DurabilityClass::Strict,
                boundary
            ),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&path)?;
        let recovered_identity = reopened.observe_ann_index(index)?.base_identity;
        assert!(
            recovered_identity == old_identity || recovered_identity == replacement_identity,
            "unexpected identity after {boundary:?}"
        );
        assert_partitioned_routing(&reopened, index, &query)?;
        assert_eq!(
            reopened.search_vector_exact_latest(index, &query, 8)?,
            expected
        );
    }
    Ok(())
}

#[test]
fn lifecycle_stable_initial_caps_consolidate_and_reopen_for_r1_r2_r64() -> Result<(), TestError> {
    for (retain_generations, expected_partitions) in [(1, 111_usize), (2, 74), (64, 2)] {
        let temporary = TestDirectory::new();
        let path = temporary
            .path()
            .join(format!("data-retain-{retain_generations}"));
        fs::create_dir_all(path.parent().ok_or("missing test parent")?)?;
        let mut database = NativeDatabase::create(&path)?;
        let index = ObjectId::new(100)?;
        let mut create = database.begin(1, DurabilityClass::Memory)?;
        create.create_vector_index_with_lifecycle(
            index,
            "partitioned-cap",
            2,
            VectorMetric::SquaredL2,
            config()?,
            IncrementalVectorLifecycle {
                delta_max_entries: 8,
                consolidate_after_deltas: 1,
                retain_generations,
            },
        )?;
        create.commit()?;
        let vectors = (1..=u16::try_from(expected_partitions)?)
            .map(|value| {
                Ok((
                    ObjectId::new(u128::from(value))?,
                    Vector::new([f32::from(value), f32::from(value % 7)])?,
                ))
            })
            .collect::<Result<Vec<_>, TestError>>()?;
        let initial = database.plan_initial_ann_bulk(index, vectors, expected_partitions)?;
        database.publish_initial_ann_bulk(initial, DurabilityClass::Memory)?;

        let mut delta = database.begin(2, DurabilityClass::Memory)?;
        delta.upsert_vector(index, ObjectId::new(1)?, Vector::new([0.25, 0.5])?)?;
        delta.commit()?;
        let plan = database
            .plan_due_ann_consolidation(index, expected_partitions)?
            .ok_or("lifecycle maintenance was not due")?;
        database.consolidate_ann(plan, DurabilityClass::Memory)?;
        let query = Vector::new([0.25, 0.5])?;
        let selected = database.search_ann_selected_latest(
            index,
            &query,
            AnnSearchOptions::new(1, 8, Some(1))?,
            expected_partitions,
        )?;
        assert_ne!(
            selected.routing_mode,
            AnnPartitionRoutingMode::SingleGenerationFallback
        );
        assert_eq!(selected.total_partitions, expected_partitions);
        drop(database);

        let reopened = NativeDatabase::open(&path)?;
        let reopened_selected = reopened.search_ann_selected_latest(
            index,
            &query,
            AnnSearchOptions::new(1, 8, Some(1))?,
            expected_partitions,
        )?;
        assert_eq!(reopened_selected.total_partitions, expected_partitions);
        assert_eq!(
            reopened_selected.search.hits[0].object_id,
            ObjectId::new(1)?
        );
    }
    Ok(())
}

#[test]
fn empty_partitioned_consolidation_uses_canonical_single_exception() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = seed_partitioned(&path)?;
    let mut delete = database.begin(2, DurabilityClass::Strict)?;
    for value in 1..=PARTITIONED_VECTOR_COUNT {
        assert!(delete.delete_vector(index, ObjectId::new(u128::from(value))?)?);
    }
    delete.commit()?;
    let plan = database.plan_ann_consolidation(index, 1, usize::from(PARTITIONED_VECTOR_COUNT))?;
    database.consolidate_ann(plan, DurabilityClass::Strict)?;
    let observed = database.observe_ann_index(index)?;
    assert_eq!(observed.base_vector_count, 0);
    assert_eq!(observed.effective_vector_count, 0);
    assert_eq!(observed.delta_records, 0);
    let query = Vector::new([0.0, 0.0])?;
    let selected =
        database.search_ann_selected_latest(index, &query, selected_options()?, PARTITION_COUNT)?;
    assert_eq!(
        selected.routing_mode,
        AnnPartitionRoutingMode::SingleGenerationFallback
    );
    assert_eq!(selected.total_partitions, 1);
    assert!(selected.search.hits.is_empty());
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.observe_ann_index(index)?.base_vector_count, 0);
    let selected =
        reopened.search_ann_selected_latest(index, &query, selected_options()?, PARTITION_COUNT)?;
    assert_eq!(
        selected.routing_mode,
        AnnPartitionRoutingMode::SingleGenerationFallback
    );
    assert!(selected.search.hits.is_empty());
    Ok(())
}
