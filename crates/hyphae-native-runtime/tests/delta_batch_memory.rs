// SPDX-License-Identifier: Apache-2.0

//! Batch-wide memory admission contracts for point-resolved all-engine deltas.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use hyphae_native_runtime::{
    GovernorAdmissionError, NativeDatabase, NativeDeltaWriteBatch, NativeRuntimeError, SqlError,
    SqlResult,
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
