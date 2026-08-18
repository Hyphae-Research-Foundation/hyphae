// SPDX-License-Identifier: Apache-2.0

//! G6 incremental ANN foreground lifecycle evidence.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_catalog::{IncrementalVectorLifecycle, MAX_INCREMENTAL_VECTOR_DELTA_ENTRIES};
use hyphae_native_runtime::{
    AnnSearchOptions, HnswConfig, NativeDatabase, NativeRuntimeError, Vector, VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(std::env::temp_dir().join(format!(
            "hyphae-ann-incremental-{}-{}",
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

fn create_index(path: &Path) -> Result<(NativeDatabase, ObjectId), TestError> {
    let index = ObjectId::new(100)?;
    fs::create_dir_all(path.parent().ok_or("missing test parent")?)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(index, "vectors", 3, VectorMetric::SquaredL2, config()?)?;
    seed.upsert_vectors(
        index,
        [
            (ObjectId::new(1)?, Vector::new([0.0, 0.0, 0.0])?),
            (ObjectId::new(2)?, Vector::new([2.0, 0.0, 0.0])?),
            (ObjectId::new(3)?, Vector::new([4.0, 0.0, 0.0])?),
        ],
    )?;
    seed.commit()?;
    Ok((database, index))
}

fn lifecycle(
    delta_max_entries: u32,
    consolidate_after_deltas: u16,
    retain_generations: u16,
) -> IncrementalVectorLifecycle {
    IncrementalVectorLifecycle {
        delta_max_entries,
        consolidate_after_deltas,
        retain_generations,
    }
}

#[test]
fn foreground_mutations_keep_one_base_and_reopen_the_exact_effective_set() -> Result<(), TestError>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = create_index(&path)?;
    let initial = database.observe_ann_index(index)?;
    assert_eq!(initial.base_vector_count, 3);
    assert_eq!(initial.effective_vector_count, 3);
    assert_eq!(initial.delta_records, 0);
    assert_eq!(
        initial.generation_records,
        initial.selected_generation_records
    );

    let mut mutation = database.begin(2, DurabilityClass::Strict)?;
    mutation.upsert_vector(index, ObjectId::new(2)?, Vector::new([9.0, 0.0, 0.0])?)?;
    assert!(mutation.delete_vector(index, ObjectId::new(3)?)?);
    mutation.upsert_vector(index, ObjectId::new(4)?, Vector::new([1.0, 0.0, 0.0])?)?;
    mutation.commit()?;

    let after = database.observe_ann_index(index)?;
    assert_eq!(after.base_identity, initial.base_identity);
    assert_ne!(after.view_identity, initial.view_identity);
    assert_eq!(after.base_vector_count, 3);
    assert_eq!(after.effective_vector_count, 3);
    assert_eq!(after.delta_records, 3);
    assert_eq!(after.generation_records, initial.generation_records);
    assert_eq!(after.generation_records, after.selected_generation_records);
    let expected =
        database.search_vector_exact_latest(index, &Vector::new([0.0, 0.0, 0.0])?, 10)?;
    assert_eq!(
        expected.iter().map(|hit| hit.object_id).collect::<Vec<_>>(),
        [ObjectId::new(1)?, ObjectId::new(4)?, ObjectId::new(2)?]
    );
    let approximate = database.search_ann_latest(
        index,
        &Vector::new([0.0, 0.0, 0.0])?,
        AnnSearchOptions::new(3, 8, Some(4))?,
    )?;
    assert_eq!(approximate.build_identity, after.view_identity);
    assert_eq!(
        approximate
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [ObjectId::new(1)?, ObjectId::new(4)?, ObjectId::new(2)?]
    );
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.observe_ann_index(index)?, after);
    assert_eq!(
        reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0, 0.0])?, 10,)?,
        expected
    );
    Ok(())
}

#[test]
fn configured_object_delta_limit_is_hard_and_atomic() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    fs::create_dir_all(temporary.path())?;
    let path = temporary.path().join("data");
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(&path)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index_with_lifecycle(
        index,
        "vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
        lifecycle(8, 4, 1),
    )?;
    create.commit()?;

    let mut mutation = database.begin(2, DurabilityClass::Memory)?;
    let vectors = (1..=9)
        .map(|value| {
            Ok((
                ObjectId::new(u128::try_from(value)?)?,
                Vector::new([f32::from(i16::try_from(value)?), 0.0])?,
            ))
        })
        .collect::<Result<Vec<_>, TestError>>()?;
    assert!(matches!(
        mutation.upsert_vectors(index, vectors),
        Err(NativeRuntimeError::AnnDeltaLimitExceeded)
    ));
    assert_eq!(
        mutation.search_vector_exact(index, &Vector::new([0.0, 0.0])?, 1)?,
        []
    );
    mutation.rollback();
    assert_eq!(database.observe_ann_index(index)?.delta_records, 0);
    Ok(())
}

#[test]
fn durable_policy_marks_maintenance_due_and_builds_a_scheduler_plan() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    fs::create_dir_all(temporary.path())?;
    let path = temporary.path().join("data");
    let index = ObjectId::new(100)?;
    let policy = lifecycle(8, 2, 1);
    let mut database = NativeDatabase::create(&path)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index_with_lifecycle(
        index,
        "vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
        policy,
    )?;
    create.upsert_vector(index, ObjectId::new(1)?, Vector::new([0.0, 0.0])?)?;
    create.commit()?;

    assert_eq!(database.ann_maintenance_status(index)?.lifecycle, policy);
    assert!(!database.ann_maintenance_status(index)?.due);
    assert!(database.plan_due_ann_consolidation(index, 16)?.is_none());
    for (object, value) in [(2, 2.0), (3, 3.0)] {
        let mut update = database.begin(2, DurabilityClass::Strict)?;
        update.upsert_vector(index, ObjectId::new(object)?, Vector::new([value, 0.0])?)?;
        update.commit()?;
    }
    let due = database.ann_maintenance_status(index)?;
    assert!(due.due);
    assert_eq!(due.delta_records, 2);
    let plan = database
        .plan_due_ann_consolidation(index, 16)?
        .ok_or("due index did not produce a plan")?;
    database.consolidate_ann(plan, DurabilityClass::Strict)?;
    assert!(!database.ann_maintenance_status(index)?.due);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.ann_maintenance_status(index)?.lifecycle, policy);
    assert!(!reopened.ann_maintenance_status(index)?.due);
    Ok(())
}

#[test]
fn one_foreground_vector_write_does_not_scale_with_the_effective_set() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let path = temporary.path().join("data");
    let (mut database, index) = create_index(&path)?;
    let before_index = database.observe_ann_index(index)?;
    let before_physical = database.physical_observation()?;
    let mut update = database.begin(2, DurabilityClass::Memory)?;
    update.upsert_vector(index, ObjectId::new(2)?, Vector::new([3.0, 0.0, 0.0])?)?;
    update.commit()?;
    let after_physical = database.physical_observation()?;
    let after_index = database.observe_ann_index(index)?;

    assert_eq!(after_index.base_identity, before_index.base_identity);
    assert_eq!(
        after_index.generation_records,
        before_index.generation_records
    );
    assert_eq!(after_index.delta_records, 1);
    assert!(after_physical.page_count > before_physical.page_count);
    assert!(after_physical.page_count - before_physical.page_count < 16);
    assert!(fs::metadata(path.join("wal.hywal"))?.len() > before_physical.wal_bytes);
    Ok(())
}

#[test]
fn lifecycle_policy_rejects_zero_inverted_and_above_format_bounds() -> Result<(), TestError> {
    for policy in [
        lifecycle(0, 1, 0),
        lifecycle(4, 5, 0),
        lifecycle(MAX_INCREMENTAL_VECTOR_DELTA_ENTRIES + 1, 1, 0),
        lifecycle(4, 1, 65),
    ] {
        let temporary = TestDirectory::new();
        fs::create_dir_all(temporary.path())?;
        let mut database = NativeDatabase::create(temporary.path().join("data"))?;
        let mut create = database.begin(1, DurabilityClass::Memory)?;
        assert!(
            create
                .create_vector_index_with_lifecycle(
                    ObjectId::new(100)?,
                    "vectors",
                    2,
                    VectorMetric::SquaredL2,
                    config()?,
                    policy,
                )
                .is_err()
        );
        create.rollback();
    }
    Ok(())
}
