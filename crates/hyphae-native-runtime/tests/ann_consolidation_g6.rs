// SPDX-License-Identifier: Apache-2.0

//! G6 bounded ANN consolidation and interruption evidence.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_runtime::{
    CommitBoundary, HnswConfig, NativeDatabase, NativeRuntimeError, SnapshotPinId, Vector,
    VectorMetric,
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
