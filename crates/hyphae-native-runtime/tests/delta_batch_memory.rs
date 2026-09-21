// SPDX-License-Identifier: Apache-2.0

//! Batch-wide memory admission contracts for point-resolved all-engine deltas.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use hyphae_native_runtime::{
    GovernorAdmissionError, HnswConfig, NativeDatabase, NativeDeltaWriteBatch, NativeRuntimeError,
    SqlError, SqlResult, Vector, VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId, ScalarValue};

type TestError = Box<dyn std::error::Error>;

const MIB: usize = 1_024 * 1_024;
const SINGLE_ENGINE_PAYLOAD_BYTES: usize = 17 * MIB;
const HIDDEN_CAPACITY_BYTES: usize = 33 * MIB;
const OVERSIZED_DURABLE_VALUE_BYTES: usize = 33 * MIB;
const SQL_COLUMN_BYTES: usize = 12 * MIB;
const MIXED_PAYLOAD_BYTES: usize = 5 * MIB;
const MIXED_HASH_BYTES: usize = 4 * MIB;
const SEARCH_INDEX: u128 = 500;

struct TemporaryDirectory(PathBuf);

static NEXT_TEMPORARY_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        for _ in 0..1_024 {
            let nonce = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hy-delta-memory-{}-{nonce}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("failed to allocate a unique temporary directory".into())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn create_delta_database(path: &Path) -> Result<(NativeDatabase, ObjectId), TestError> {
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql("INSERT INTO events (id, body) VALUES (1, 'seed')", &[])?;
    seed.create_search_index(index, "documents")?;
    seed.create_hash(b"profile".to_vec())?;
    seed.commit()?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;
    Ok((database, index))
}

fn is_capacity_rejection(error: &NativeRuntimeError) -> bool {
    matches!(
        error,
        NativeRuntimeError::ResourceAdmission(GovernorAdmissionError::ParentCapacity)
    )
}

fn assert_sql_capacity_rejection(result: &Result<SqlResult, SqlError>) {
    assert!(matches!(
        result,
        Err(SqlError::Runtime(NativeRuntimeError::ResourceAdmission(
            GovernorAdmissionError::ParentCapacity
        )))
    ));
}

fn assert_sql_invalid_prepared_mutation(result: &Result<SqlResult, SqlError>) {
    assert!(
        matches!(
            result,
            Err(SqlError::Runtime(
                NativeRuntimeError::InvalidPreparedMutation
            ))
        ),
        "unexpected SQL result: {result:?}"
    );
}

fn bounded_search_text(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes);
    while text.len().saturating_add(2) <= bytes {
        text.push_str("d ");
    }
    if text.len() < bytes {
        text.push('d');
    }
    text
}

#[test]
fn delta_rejects_single_engine_retention_above_parent_before_mutation() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let (database, index) = create_delta_database(&temporary.path().join("data"))?;

    let mut sql = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert_sql_capacity_rejection(&database.stage_delta_sql_dml(
        &mut sql,
        "UPDATE events SET body = ? WHERE id = ?",
        &[
            ScalarValue::Text("s".repeat(SINGLE_ENGINE_PAYLOAD_BYTES)),
            ScalarValue::Signed(1),
        ],
    ));
    assert_eq!(sql.mutation_count(), 0);

    let mut scalar = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert!(
        database
            .stage_delta_set(
                &mut scalar,
                b"large-scalar".to_vec(),
                vec![b'v'; SINGLE_ENGINE_PAYLOAD_BYTES],
                None,
            )
            .is_err_and(|error| is_capacity_rejection(&error))
    );
    assert_eq!(scalar.mutation_count(), 0);

    let mut lexical = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert!(
        database
            .stage_delta_index_document(
                &mut lexical,
                index,
                b"large-document".to_vec(),
                bounded_search_text(SINGLE_ENGINE_PAYLOAD_BYTES),
            )
            .is_err_and(|error| is_capacity_rejection(&error))
    );
    assert_eq!(lexical.mutation_count(), 0);
    Ok(())
}

#[test]
fn delta_accounts_caller_capacity_in_scalar_and_lexical_mutations() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let (database, index) = create_delta_database(&temporary.path().join("data"))?;

    let mut scalar_value = Vec::with_capacity(HIDDEN_CAPACITY_BYTES);
    scalar_value.push(b'v');
    let mut scalar = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert!(
        database
            .stage_delta_set(&mut scalar, b"scalar".to_vec(), scalar_value, None)
            .is_err_and(|error| is_capacity_rejection(&error))
    );
    assert_eq!(scalar.mutation_count(), 0);

    let mut text = String::with_capacity(HIDDEN_CAPACITY_BYTES);
    text.push('d');
    let mut lexical = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert!(
        database
            .stage_delta_index_document(&mut lexical, index, b"document".to_vec(), text,)
            .is_err_and(|error| is_capacity_rejection(&error))
    );
    assert_eq!(lexical.mutation_count(), 0);
    Ok(())
}

fn stage_three_engine_prefix(
    database: &NativeDatabase,
    index: ObjectId,
    delta: &mut NativeDeltaWriteBatch,
) -> Result<(), TestError> {
    assert_eq!(
        database.stage_delta_sql_dml(
            delta,
            "UPDATE events SET body = ? WHERE id = ?",
            &[
                ScalarValue::Text("q".repeat(MIXED_PAYLOAD_BYTES)),
                ScalarValue::Signed(1),
            ],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    database.stage_delta_set(
        delta,
        b"mixed-scalar".to_vec(),
        vec![b's'; MIXED_PAYLOAD_BYTES],
        None,
    )?;
    database.stage_delta_index_document(
        delta,
        index,
        b"mixed-document".to_vec(),
        bounded_search_text(MIXED_PAYLOAD_BYTES),
    )?;
    Ok(())
}

#[test]
fn mixed_delta_rejects_the_operation_that_crosses_total_retained_memory() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let (mut database, index) = create_delta_database(&temporary.path().join("data"))?;
    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    stage_three_engine_prefix(&database, index, &mut delta)?;
    assert_eq!(delta.mutation_count(), 3);

    assert!(
        database
            .stage_delta_hset(
                &mut delta,
                b"profile".to_vec(),
                b"mixed-hash".to_vec(),
                vec![b'h'; MIXED_HASH_BYTES],
            )
            .is_err_and(|error| is_capacity_rejection(&error))
    );
    assert_eq!(delta.mutation_count(), 3);

    database.commit_optimistic(delta)?;
    assert_eq!(
        database.get_latest_structure(b"mixed-scalar", 2)?,
        Some(vec![b's'; MIXED_PAYLOAD_BYTES])
    );
    assert_eq!(database.hget_latest_hash(b"profile", b"mixed-hash")?, None);
    Ok(())
}

fn assert_oversized_durable_scalar_replaces_without_hydrating_old_payload(
    database: &mut NativeDatabase,
    logical_time_micros: i64,
    accepted_key: &[u8],
) -> Result<(), TestError> {
    let mut delta =
        database.begin_optimistic_delta(logical_time_micros, DurabilityClass::Memory)?;
    database.stage_delta_set(
        &mut delta,
        b"durable-large".to_vec(),
        b"replacement".to_vec(),
        None,
    )?;
    assert_eq!(delta.mutation_count(), 1);
    database.stage_delta_set(&mut delta, accepted_key.to_vec(), b"ok".to_vec(), None)?;
    assert_eq!(delta.mutation_count(), 2);
    database.commit_optimistic(delta)?;
    Ok(())
}

#[test]
fn durable_scalar_replacement_does_not_hydrate_old_blob() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.set(
        b"durable-large".to_vec(),
        vec![b'd'; OVERSIZED_DURABLE_VALUE_BYTES],
        None,
    )?;
    seed.commit()?;

    assert_oversized_durable_scalar_replaces_without_hydrating_old_payload(
        &mut database,
        2,
        b"accepted-v2",
    )?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;
    assert_oversized_durable_scalar_replaces_without_hydrating_old_payload(
        &mut database,
        3,
        b"accepted-v3",
    )?;
    assert_eq!(
        database
            .get_latest_structure(b"durable-large", 3)?
            .map(|value| value.len()),
        Some(b"replacement".len())
    );
    Ok(())
}

#[test]
fn durable_sql_row_rejection_rolls_back_relation_hydration() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, a TEXT NOT NULL, b TEXT NOT NULL, c TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO events (id, a, b, c) VALUES (?, ?, ?, ?)",
        &[
            ScalarValue::Signed(1),
            ScalarValue::Text("a".repeat(SQL_COLUMN_BYTES)),
            ScalarValue::Text("b".repeat(SQL_COLUMN_BYTES)),
            ScalarValue::Text("c".repeat(SQL_COLUMN_BYTES)),
        ],
    )?;
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert_sql_capacity_rejection(&database.stage_delta_sql_dml(
        &mut delta,
        "UPDATE events SET a = ? WHERE id = ?",
        &[
            ScalarValue::Text("small".to_owned()),
            ScalarValue::Signed(1),
        ],
    ));
    assert_eq!(delta.mutation_count(), 0);
    database.stage_delta_set(
        &mut delta,
        b"after-sql-rejection".to_vec(),
        b"ok".to_vec(),
        None,
    )?;
    database.commit_optimistic(delta)?;
    assert_eq!(
        database.get_latest_structure(b"after-sql-rejection", 2)?,
        Some(b"ok".to_vec())
    );
    Ok(())
}

#[test]
fn explicit_delta_type_stages_and_commits_without_materialized_access() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let (mut database, index) = create_delta_database(&temporary.path().join("data"))?;
    let mut delta: NativeDeltaWriteBatch =
        database.begin_optimistic_delta(2, DurabilityClass::Memory)?;

    database.stage_delta_sql_dml(
        &mut delta,
        "UPDATE events SET body = ? WHERE id = ?",
        &[
            ScalarValue::Text("staged".to_owned()),
            ScalarValue::Signed(1),
        ],
    )?;
    database.stage_delta_set(
        &mut delta,
        b"guarded-scalar".to_vec(),
        b"staged".to_vec(),
        None,
    )?;
    database.stage_delta_index_document(
        &mut delta,
        index,
        b"guarded-document".to_vec(),
        "staged".to_owned(),
    )?;
    assert_eq!(delta.mutation_count(), 3);
    database.commit_optimistic(delta)?;
    assert_eq!(
        database.get_latest_structure(b"guarded-scalar", 2)?,
        Some(b"staged".to_vec())
    );
    Ok(())
}

#[test]
fn absence_fences_accept_exactly_the_4096_record_bound() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("absence-fence-bound");
    let index = ObjectId::new(700)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "absence-fence-bound",
        2,
        VectorMetric::SquaredL2,
        HnswConfig::new(4, 16, 8, 32, 7)?,
    )?;
    seed.commit()?;

    let mut batch = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    for ordinal in 1..=4_096_u128 {
        assert!(!database.stage_delta_vector_absence_fence(
            &mut batch,
            index,
            ObjectId::new(ordinal)?,
        )?);
    }
    let physical_object = ObjectId::new(5_000)?;
    database.stage_delta_upsert_vector(
        &mut batch,
        index,
        physical_object,
        Vector::new([1.0, 0.0])?,
    )?;
    let retained_mutations = batch.mutation_count();
    assert_eq!(retained_mutations, 4_097);
    assert!(matches!(
        database.stage_delta_vector_absence_fence(&mut batch, index, ObjectId::new(4_097)?,),
        Err(NativeRuntimeError::AnnDeltaLimitExceeded)
    ));
    assert_eq!(batch.mutation_count(), retained_mutations);
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened.search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?[0].object_id,
        physical_object
    );
    Ok(())
}

#[test]
fn two_physical_ann_targets_share_one_root_structural_peak_and_reopen() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("two-physical-ann-targets");
    let indexes = [ObjectId::new(750)?, ObjectId::new(751)?];
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.into_iter().enumerate() {
        seed.create_vector_index(
            index,
            &format!("two-physical-ann-target-{ordinal}"),
            2,
            VectorMetric::SquaredL2,
            HnswConfig::new(4, 16, 8, 32, 7)?,
        )?;
    }
    seed.commit()?;

    let mut batch = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.into_iter().enumerate() {
        database.stage_delta_upsert_vector(
            &mut batch,
            index,
            ObjectId::new(u128::try_from(ordinal)? + 1)?,
            Vector::new([f32::from(u16::try_from(ordinal)?), 1.0])?,
        )?;
    }
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    for (ordinal, index) in indexes.into_iter().enumerate() {
        assert_eq!(
            reopened
                .search_vector_exact_latest(
                    index,
                    &Vector::new([f32::from(u16::try_from(ordinal)?), 1.0])?,
                    1,
                )?
                .first()
                .map(|hit| hit.object_id),
            Some(ObjectId::new(u128::try_from(ordinal)? + 1)?)
        );
    }
    Ok(())
}

#[test]
fn one_physical_delete_and_fifteen_absence_fences_fit_the_sixteen_target_parent()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("sixteen-ann-targets");
    let mut database = NativeDatabase::create(&data)?;
    let indexes = (0..16_u128)
        .map(|ordinal| ObjectId::new(800 + ordinal))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.iter().copied().enumerate() {
        seed.create_vector_index(
            index,
            &format!("sixteen-ann-target-{ordinal}"),
            2,
            VectorMetric::SquaredL2,
            HnswConfig::new(4, 16, 8, 32, 7)?,
        )?;
        if ordinal == 0 {
            seed.upsert_vector(index, ObjectId::new(1)?, Vector::new([1.0, 0.0])?)?;
        }
    }
    seed.commit()?;

    let deleted = ObjectId::new(1)?;
    let mut batch = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    assert!(database.stage_delta_vector_absence_fence(&mut batch, indexes[0], deleted,)?);
    for (ordinal, index) in indexes.iter().copied().enumerate().skip(1) {
        assert!(!database.stage_delta_vector_absence_fence(
            &mut batch,
            index,
            ObjectId::new(u128::try_from(ordinal)? + 1)?,
        )?);
    }
    assert_eq!(batch.mutation_count(), 16);
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert!(
        reopened
            .search_vector_exact_latest(indexes[0], &Vector::new([1.0, 0.0])?, 1)?
            .is_empty()
    );
    for index in indexes.into_iter().skip(1) {
        assert!(
            reopened
                .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 1)?
                .is_empty()
        );
    }
    Ok(())
}

#[test]
fn sixteen_existing_m05_targets_share_one_root_structural_peak_and_reopen() -> Result<(), TestError>
{
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("sixteen-physical-ann-targets");
    let indexes = (0..16_u128)
        .map(|ordinal| ObjectId::new(1_000 + ordinal))
        .collect::<Result<Vec<_>, _>>()?;
    let mut database = NativeDatabase::create(&data)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.iter().copied().enumerate() {
        create.create_vector_index(
            index,
            &format!("sixteen-physical-ann-target-{ordinal}"),
            2,
            VectorMetric::SquaredL2,
            HnswConfig::new(4, 16, 8, 32, 7)?,
        )?;
    }
    create.commit()?;
    let mut select_m05 = database.begin(2, DurabilityClass::Strict)?;
    for index in indexes.iter().copied() {
        select_m05.upsert_vector(index, ObjectId::new(1)?, Vector::new([0.0, 1.0])?)?;
    }
    select_m05.commit()?;

    let mut batch = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.iter().copied().enumerate() {
        database.stage_delta_upsert_vector(
            &mut batch,
            index,
            ObjectId::new(2)?,
            Vector::new([f32::from(u16::try_from(ordinal)?), 2.0])?,
        )?;
    }
    assert_eq!(batch.mutation_count(), 16);
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    for (ordinal, index) in indexes.into_iter().enumerate() {
        assert_eq!(
            reopened
                .search_vector_exact_latest(
                    index,
                    &Vector::new([f32::from(u16::try_from(ordinal)?), 2.0])?,
                    1,
                )?
                .first()
                .map(|hit| hit.object_id),
            Some(ObjectId::new(2)?)
        );
    }
    Ok(())
}

#[test]
fn physical_delete_admits_a_large_dimension_d01_and_d02_path_before_retention()
-> Result<(), TestError> {
    const DIMENSION: usize = 3_840;
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("large-d01-d02-delete");
    let index = ObjectId::new(900)?;
    let object = ObjectId::new(1)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_vector_index(
        index,
        "large-d01-d02-delete",
        u16::try_from(DIMENSION)?,
        VectorMetric::SquaredL2,
        HnswConfig::new(4, 16, 8, 32, 7)?,
    )?;
    create.commit()?;
    let mut legacy = database.begin(2, DurabilityClass::Strict)?;
    legacy.upsert_vector(index, object, Vector::new(vec![1.0; DIMENSION])?)?;
    legacy.commit()?;
    let mut overlay = database.begin(3, DurabilityClass::Strict)?;
    overlay.upsert_vector(index, object, Vector::new(vec![2.0; DIMENSION])?)?;
    overlay.commit()?;

    let mut deletion = database.begin_optimistic_delta(4, DurabilityClass::Strict)?;
    assert!(database.stage_delta_vector_absence_fence(&mut deletion, index, object)?);
    database.commit_optimistic(deletion)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert!(
        reopened
            .search_vector_exact_latest(index, &Vector::new(vec![2.0; DIMENSION])?, 1)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn delta_sql_fails_closed_for_outbound_and_inbound_foreign_keys() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    seed.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT children_parent_fk FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
    seed.commit()?;

    let mut child_delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert_sql_invalid_prepared_mutation(&database.stage_delta_sql_dml(
        &mut child_delta,
        "INSERT INTO children (id, parent_id) VALUES (1, 1)",
        &[],
    ));
    assert_eq!(child_delta.mutation_count(), 0);
    database.stage_delta_set(
        &mut child_delta,
        b"after-child-fk-rejection".to_vec(),
        b"ok".to_vec(),
        None,
    )?;
    database.commit_optimistic(child_delta)?;

    let mut parent_delta = database.begin_optimistic_delta(3, DurabilityClass::Memory)?;
    assert_sql_invalid_prepared_mutation(&database.stage_delta_sql_dml(
        &mut parent_delta,
        "DELETE FROM parents WHERE id = 1",
        &[],
    ));
    assert_eq!(parent_delta.mutation_count(), 0);
    database.stage_delta_set(
        &mut parent_delta,
        b"after-parent-fk-rejection".to_vec(),
        b"ok".to_vec(),
        None,
    )?;
    database.commit_optimistic(parent_delta)?;
    Ok(())
}

#[test]
fn failed_hash_increment_restores_hydration_and_keeps_prior_stage_committable()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_hash(b"counters".to_vec())?;
    seed.hset(
        b"counters".to_vec(),
        b"noncanonical".to_vec(),
        b"01".to_vec(),
    )?;
    seed.hset(
        b"counters".to_vec(),
        b"maximum".to_vec(),
        i64::MAX.to_string().into_bytes(),
    )?;
    seed.commit()?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_set(
        &mut delta,
        b"prior-stage".to_vec(),
        b"committed".to_vec(),
        None,
    )?;
    let mutation_count = delta.mutation_count();
    assert!(
        database
            .stage_delta_hincrement(
                &mut delta,
                b"counters".to_vec(),
                b"noncanonical".to_vec(),
                1,
            )
            .is_err()
    );
    assert_eq!(delta.mutation_count(), mutation_count);
    assert!(matches!(
        database.stage_delta_hincrement(&mut delta, b"counters".to_vec(), b"maximum".to_vec(), 1,),
        Err(NativeRuntimeError::StructureIntegerOverflow)
    ));
    assert_eq!(delta.mutation_count(), mutation_count);
    database.commit_optimistic(delta)?;
    assert_eq!(
        database.get_latest_structure(b"prior-stage", 2)?,
        Some(b"committed".to_vec())
    );
    Ok(())
}

#[test]
fn ann_delta_hydration_is_rejected_before_the_parent_allocation_is_exceeded()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let mut database = NativeDatabase::create(&data)?;
    let config = HnswConfig::new(4, 16, 8, 32, 7)?;
    let indexes = (0_u128..16)
        .map(|offset| ObjectId::new(10_000 + offset))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    for (ordinal, index) in indexes.iter().enumerate() {
        seed.create_vector_index(
            *index,
            &format!("bounded-{ordinal}"),
            2,
            VectorMetric::SquaredL2,
            config,
        )?;
    }
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    let mut admitted = Vec::new();
    let mut rejected = None;
    for (ordinal, index) in indexes.iter().enumerate() {
        let object = ObjectId::new(20_000 + u128::try_from(ordinal)?)?;
        let before = delta.mutation_count();
        match database.stage_delta_upsert_vector(
            &mut delta,
            *index,
            object,
            Vector::new([1.0, 0.0])?,
        ) {
            Ok(()) => admitted.push((*index, object)),
            Err(error) if is_capacity_rejection(&error) => {
                assert_eq!(delta.mutation_count(), before);
                rejected = Some((*index, object));
                break;
            }
            Err(error) => return Err(format!("ANN stage {ordinal} failed: {error:?}").into()),
        }
    }
    let (rejected_index, rejected_object) = rejected.ok_or("ANN delta capacity did not bind")?;
    assert!(admitted.len() >= 2);
    database
        .commit_optimistic(delta)
        .map_err(|error| format!("ANN bounded commit failed: {error:?}"))?;
    drop(database);

    let admitted_count = admitted.len();
    let reopened = NativeDatabase::open(&data).map_err(|error| {
        format!("ANN bounded reopen failed after {admitted_count} indexes: {error:?}")
    })?;
    for (index, object) in admitted {
        assert!(
            reopened
                .search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?
                .first()
                .is_some_and(|hit| hit.object_id == object)
        );
    }
    assert!(
        reopened
            .search_vector_exact_latest(rejected_index, &Vector::new([1.0, 0.0])?, 1,)?
            .iter()
            .all(|hit| hit.object_id != rejected_object)
    );
    Ok(())
}
