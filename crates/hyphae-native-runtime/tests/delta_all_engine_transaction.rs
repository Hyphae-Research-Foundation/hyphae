// SPDX-License-Identifier: Apache-2.0

//! Contract tests for point-resolved all-engine delta transactions.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use hyphae_native_runtime::{
    AnnSearchOptions, AnnSearchStrategy, CommitBoundary, GroupCommitOutcome, HnswConfig,
    NativeCommitBatch, NativeDatabase, NativeRuntimeError, SqlError, SqlResult, Vector,
    VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId, ScalarValue};

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

static NEXT_TEMPORARY_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        for _ in 0..1_024 {
            let nonce = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hy-delta-transaction-{}-{nonce}",
                std::process::id()
            ));
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

#[test]
fn delta_transaction_commits_point_resolved_changes_under_one_csn() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(&data)?;

    let mut seed = database.begin(90, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (
            id BIGINT PRIMARY KEY,
            body TEXT NOT NULL
        )",
        &[],
    )?;
    seed.create_search_index(index, "documents")?;
    seed.execute_sql("INSERT INTO events (id, body) VALUES (0, 'seed')", &[])?;
    seed.set(b"joint-key".to_vec(), b"seed".to_vec(), None)?;
    let seeded = seed.commit()?;

    let mut delta = database.begin_optimistic_delta(100, DurabilityClass::Memory)?;
    assert_eq!(
        database.stage_delta_sql_dml(
            &mut delta,
            "UPDATE events SET body = ? WHERE id = ?",
            &[
                ScalarValue::Text("needle native".to_owned()),
                ScalarValue::Signed(0),
            ],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    database.stage_delta_set(
        &mut delta,
        b"joint-key".to_vec(),
        b"joint-value".to_vec(),
        None,
    )?;
    database.stage_delta_index_document(
        &mut delta,
        index,
        b"joint-doc".to_vec(),
        "needle native".to_owned(),
    )?;
    let committed = database.commit_optimistic(delta)?;
    assert_eq!(committed.commit_csn.get(), seeded.commit_csn.get() + 1);

    let snapshot = database.snapshot(100)?;
    let prepared = snapshot.prepare_sql("SELECT id, body FROM events WHERE id = ?")?;
    let SqlResult::Rows { rows, .. } =
        snapshot.execute_prepared(&prepared, &[ScalarValue::Signed(0)])?
    else {
        return Err("SELECT did not return rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![
            ScalarValue::Signed(0),
            ScalarValue::Text("needle native".to_owned()),
        ]]
    );
    assert_eq!(snapshot.get(b"joint-key"), Some(b"joint-value".as_slice()));
    assert_eq!(
        snapshot
            .match_text(index, "needle", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        vec![b"joint-doc".to_vec()]
    );
    Ok(())
}

#[test]
fn delta_sql_preserves_unique_index_semantics_and_sequential_key_reuse() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE accounts (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            body TEXT NOT NULL
        )",
        &[],
    )?;
    seed.execute_sql(
        "CREATE UNIQUE INDEX accounts_email ON accounts (email)",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO accounts (id, email, body)
         VALUES (1, 'one@example.test', 'one')",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO accounts (id, email, body)
         VALUES (2, 'two@example.test', 'two')",
        &[],
    )?;
    seed.commit()?;

    let mut duplicate = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    assert!(matches!(
        database.stage_delta_sql_dml(
            &mut duplicate,
            "UPDATE accounts SET email = ? WHERE id = ?",
            &[
                ScalarValue::Text("one@example.test".to_owned()),
                ScalarValue::Signed(2),
            ],
        ),
        Err(SqlError::UniqueViolation)
    ));

    let mut reuse = database.begin_optimistic_delta(3, DurabilityClass::Memory)?;
    assert_eq!(
        database.stage_delta_sql_dml(
            &mut reuse,
            "UPDATE accounts SET email = ? WHERE id = ?",
            &[
                ScalarValue::Text("moved@example.test".to_owned()),
                ScalarValue::Signed(1),
            ],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    assert_eq!(
        database.stage_delta_sql_dml(
            &mut reuse,
            "UPDATE accounts SET email = ? WHERE id = ?",
            &[
                ScalarValue::Text("one@example.test".to_owned()),
                ScalarValue::Signed(2),
            ],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    database.commit_optimistic(reuse)?;

    let snapshot = database.snapshot(3)?;
    let prepared = snapshot.prepare_sql("SELECT id, email FROM accounts WHERE email = ?")?;
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(
        &prepared,
        &[ScalarValue::Text("one@example.test".to_owned())],
    )?
    else {
        return Err("secondary-index SELECT did not return rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![
            ScalarValue::Signed(2),
            ScalarValue::Text("one@example.test".to_owned()),
        ]]
    );
    Ok(())
}

#[test]
fn concurrent_delta_inserts_conflict_on_one_unique_projection() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE accounts (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL
        )",
        &[],
    )?;
    seed.execute_sql(
        "CREATE UNIQUE INDEX accounts_email ON accounts (email)",
        &[],
    )?;
    seed.commit()?;

    let mut first = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    let mut second = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_sql_dml(
        &mut first,
        "INSERT INTO accounts (id, email) VALUES (?, ?)",
        &[
            ScalarValue::Signed(1),
            ScalarValue::Text("same@example.test".to_owned()),
        ],
    )?;
    database.stage_delta_sql_dml(
        &mut second,
        "INSERT INTO accounts (id, email) VALUES (?, ?)",
        &[
            ScalarValue::Signed(2),
            ScalarValue::Text("same@example.test".to_owned()),
        ],
    )?;

    database.commit_optimistic(first)?;
    assert!(matches!(
        database.commit_optimistic(second),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    Ok(())
}

fn ann_config() -> Result<HnswConfig, TestError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x0044_454c_5441)?)
}

#[test]
fn vector_delta_commit_is_exact_immediate_bounded_and_reopen_equal() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let lexical = ObjectId::new(700)?;
    let vectors = ObjectId::new(701)?;
    let base_object = ObjectId::new(1)?;
    let delta_object = ObjectId::new(2)?;
    let rolled_back_object = ObjectId::new(3)?;
    let query = Vector::new([0.0, 1.0])?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(
        vectors,
        "embeddings",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vector(vectors, base_object, Vector::new([1.0, 0.0])?)?;
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    database.stage_delta_set(
        &mut delta,
        b"vector-transaction".to_vec(),
        b"complete".to_vec(),
        None,
    )?;
    database.stage_delta_index_document(
        &mut delta,
        lexical,
        b"delta-document".to_vec(),
        "exact delta visibility".to_owned(),
    )?;
    database.stage_delta_upsert_vector(&mut delta, vectors, delta_object, query.clone())?;
    let committed = database.commit_optimistic(delta)?;

    let expected = database.search_vector_exact_latest(vectors, &query, 2)?;
    assert_eq!(expected[0].object_id, delta_object);
    let observed = database.observe_ann_index(vectors)?;
    let ann = database.search_ann_latest(vectors, &query, AnnSearchOptions::new(1, 8, Some(1))?)?;
    assert_eq!(ann.snapshot_csn, Some(committed.commit_csn));
    assert_eq!(ann.build_identity, observed.view_identity);
    let snapshot = database.snapshot(2)?;
    assert_eq!(
        snapshot.get(b"vector-transaction"),
        Some(b"complete".as_slice())
    );
    assert_eq!(
        snapshot.match_text(lexical, "visibility", 1)?[0].document_id,
        b"delta-document"
    );

    let mut rolled_back = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut rolled_back,
        vectors,
        rolled_back_object,
        Vector::new([0.0, 2.0])?,
    )?;
    rolled_back.rollback();
    assert!(
        database
            .search_vector_exact_latest(vectors, &Vector::new([0.0, 2.0])?, 3)?
            .iter()
            .all(|hit| hit.object_id != rolled_back_object)
    );
    drop(snapshot);
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened.search_vector_exact_latest(vectors, &query, 2)?,
        expected
    );
    let snapshot = reopened.snapshot(4)?;
    assert_eq!(
        snapshot.get(b"vector-transaction"),
        Some(b"complete".as_slice())
    );
    assert_eq!(
        snapshot.match_text(lexical, "visibility", 1)?[0].document_id,
        b"delta-document"
    );
    Ok(())
}

#[test]
fn vector_delta_runs_real_hnsw_base_plus_exact_delta_before_and_after_reopen()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let index = ObjectId::new(720)?;
    let delta_object = ObjectId::new(10_000)?;
    let query = Vector::new([31.25, 3.5])?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "traversed-embeddings",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vectors(
        index,
        (1..=64_u16)
            .map(|value| {
                Ok((
                    ObjectId::new(u128::from(value))?,
                    Vector::new([f32::from(value), f32::from(value % 7)])?,
                ))
            })
            .collect::<Result<Vec<_>, TestError>>()?,
    )?;
    seed.commit()?;
    let base = database.observe_ann_index(index)?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(&mut delta, index, delta_object, query.clone())?;
    let committed = database.commit_optimistic(delta)?;
    let observed = database.observe_ann_index(index)?;
    assert_eq!(observed.base_identity, base.base_identity);
    assert_ne!(observed.view_identity, base.view_identity);
    let options = AnnSearchOptions::new(4, 8, Some(4))?;
    let traversed = database.search_ann_latest(index, &query, options)?;
    assert!(traversed.approximate);
    assert_eq!(traversed.strategy, AnnSearchStrategy::GraphTraversal);
    assert!(traversed.visited_nodes > 0);
    assert_eq!(traversed.snapshot_csn, Some(committed.commit_csn));
    assert_eq!(traversed.build_identity, observed.view_identity);
    assert_eq!(traversed.hits[0].object_id, delta_object);
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(reopened.observe_ann_index(index)?, observed);
    let reopened_traversal = reopened.search_ann_latest(index, &query, options)?;
    assert_eq!(reopened_traversal, traversed);
    assert_eq!(
        reopened_traversal.strategy,
        AnnSearchStrategy::GraphTraversal
    );
    assert!(reopened_traversal.visited_nodes > 0);
    Ok(())
}

#[test]
fn graph_traversal_honors_request_ef_for_shadow_fill_and_underfill() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("shadow-delete-traversal");
    let index = ObjectId::new(725)?;
    let deleted = ObjectId::new(1)?;
    let second = ObjectId::new(2)?;
    let third = ObjectId::new(3)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "shadow-delete-traversal",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vectors(
        index,
        [
            (deleted, Vector::new([0.0, 0.0])?),
            (second, Vector::new([1.0, 0.0])?),
            (third, Vector::new([2.0, 0.0])?),
        ],
    )?;
    seed.commit()?;

    let mut deletion = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    assert!(database.stage_delta_vector_absence_fence(&mut deletion, index, deleted)?);
    database.commit_optimistic(deletion)?;
    let query = Vector::new([0.0, 0.0])?;
    let underfilled =
        database.search_ann_latest(index, &query, AnnSearchOptions::new(2, 2, None)?)?;
    assert_eq!(underfilled.strategy, AnnSearchStrategy::GraphTraversal);
    assert_eq!(
        underfilled
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [second]
    );
    assert_eq!(underfilled.ef_search, 2);
    let filled = database.search_ann_latest(index, &query, AnnSearchOptions::new(2, 3, None)?)?;
    assert_eq!(filled.ef_search, 3);
    assert_eq!(
        filled
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [second, third]
    );
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened
            .search_ann_latest(index, &query, AnnSearchOptions::new(2, 3, None)?)?
            .hits
            .iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [second, third]
    );
    Ok(())
}

#[test]
fn delta_overlay_rejects_a_second_private_write_to_the_same_object_without_losing_the_first()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let index = ObjectId::new(730)?;
    let object = ObjectId::new(1)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "single-private-intent",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(&mut delta, index, object, Vector::new([1.0, 0.0])?)?;
    let mutation_count = delta.mutation_count();
    assert!(matches!(
        database.stage_delta_upsert_vector(&mut delta, index, object, Vector::new([0.0, 1.0])?,),
        Err(NativeRuntimeError::InvalidPreparedMutation)
    ));
    assert_eq!(delta.mutation_count(), mutation_count);
    database.commit_optimistic(delta)?;
    assert_eq!(
        database.search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?[0].object_id,
        object
    );
    Ok(())
}

#[test]
fn concurrent_vector_deltas_rebase_disjoint_objects_and_conflict_on_one_object()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let vectors = ObjectId::new(750)?;
    let first_object = ObjectId::new(1)?;
    let second_object = ObjectId::new(2)?;
    let materialized_object = ObjectId::new(3)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        vectors,
        "embeddings",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut first = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    let mut second = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut first,
        vectors,
        first_object,
        Vector::new([1.0, 0.0])?,
    )?;
    database.stage_delta_upsert_vector(
        &mut second,
        vectors,
        second_object,
        Vector::new([0.0, 1.0])?,
    )?;
    database.commit_optimistic(first)?;
    database.commit_optimistic(second)?;
    let hits = database.search_vector_exact_latest(vectors, &Vector::new([0.0, 1.0])?, 2)?;
    assert!(hits.iter().any(|hit| hit.object_id == first_object));
    assert!(hits.iter().any(|hit| hit.object_id == second_object));

    let mut winner = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    let mut loser = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut winner,
        vectors,
        first_object,
        Vector::new([2.0, 0.0])?,
    )?;
    database.stage_delta_upsert_vector(
        &mut loser,
        vectors,
        first_object,
        Vector::new([3.0, 0.0])?,
    )?;
    database.commit_optimistic(winner)?;
    assert!(matches!(
        database.commit_optimistic(loser),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut stale_delta = database.begin_optimistic_delta(4, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut stale_delta,
        vectors,
        second_object,
        Vector::new([0.0, 1.0])?,
    )?;
    let mut materialized = database.begin_optimistic(4, DurabilityClass::Strict)?;
    materialized.upsert_vector(vectors, materialized_object, Vector::new([0.5, 0.5])?)?;
    database.commit_optimistic(materialized)?;
    database.commit_optimistic(stale_delta)?;
    let hits = database.search_vector_exact_latest(vectors, &Vector::new([0.0, 1.0])?, 3)?;
    assert!(hits.iter().any(|hit| hit.object_id == materialized_object));
    assert!(hits.iter().any(|hit| hit.object_id == second_object));
    Ok(())
}

#[test]
fn concurrent_materialized_disjoint_vectors_rebase_and_reopen() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let mut database = NativeDatabase::create(&data)?;
    let vectors = ObjectId::new(760)?;
    let first_object = ObjectId::new(1)?;
    let second_object = ObjectId::new(2)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        vectors,
        "legacy-disjoint",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut first = database.begin_optimistic(2, DurabilityClass::Strict)?;
    let mut second = database.begin_optimistic(2, DurabilityClass::Strict)?;
    first.upsert_vector(vectors, first_object, Vector::new([1.0, 0.0])?)?;
    second.upsert_vector(vectors, second_object, Vector::new([0.0, 1.0])?)?;
    database.commit_optimistic(first)?;
    database.commit_optimistic(second)?;
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    let hits = reopened.search_vector_exact_latest(vectors, &Vector::new([0.0, 0.0])?, 2)?;
    assert_eq!(
        hits.into_iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [first_object, second_object]
    );
    Ok(())
}

#[test]
fn materialized_overlay_upserts_use_last_write_semantics_before_and_after_m05()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let mut database = NativeDatabase::create(&data)?;
    let index = ObjectId::new(765)?;
    let object = ObjectId::new(1)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "materialized-overlay",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut first = database.begin(2, DurabilityClass::Strict)?;
    first.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
    first.upsert_vector(index, object, Vector::new([0.0, 1.0])?)?;
    first.commit()?;
    let first_view = database.observe_ann_index(index)?.view_identity;
    assert!(
        database.search_vector_exact_latest(index, &Vector::new([0.0, 1.0])?, 1)?[0]
            .distance
            .abs()
            < f64::EPSILON
    );

    let mut second = database.begin(3, DurabilityClass::Strict)?;
    second.upsert_vector(index, object, Vector::new([2.0, 0.0])?)?;
    second.commit()?;
    assert_ne!(database.observe_ann_index(index)?.view_identity, first_view);
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert!(
        reopened.search_vector_exact_latest(index, &Vector::new([2.0, 0.0])?, 1)?[0]
            .distance
            .abs()
            < f64::EPSILON
    );
    Ok(())
}

#[test]
fn group_commit_rebases_disjoint_overlay_points_and_reopens() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let mut database = NativeDatabase::create(&data)?;
    let index = ObjectId::new(770)?;
    let first = ObjectId::new(1)?;
    let second = ObjectId::new(2)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "group-overlay",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut left = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    let mut right = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    database.stage_delta_upsert_vector(&mut left, index, first, Vector::new([1.0, 0.0])?)?;
    database.stage_delta_upsert_vector(&mut right, index, second, Vector::new([0.0, 1.0])?)?;
    let report = database.commit_group(vec![left, right])?;
    assert_eq!(report.accepted_commits, 2);
    assert!(matches!(
        report.outcomes[0],
        GroupCommitOutcome::Committed(_)
    ));
    assert!(matches!(
        report.outcomes[1],
        GroupCommitOutcome::Committed(_)
    ));
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    let hits = reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 2)?;
    assert_eq!(
        hits.into_iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [first, second]
    );
    Ok(())
}

#[test]
fn group_admission_composes_disjoint_point_upsert_and_delete_in_both_orders()
-> Result<(), TestError> {
    for point_first in [true, false] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if point_first {
            "point-first"
        } else {
            "legacy-first"
        });
        let mut database = NativeDatabase::create(&data)?;
        let index = ObjectId::new(775)?;
        let base = ObjectId::new(1)?;
        let point_object = ObjectId::new(2)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "group-layout",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.upsert_vector(index, base, Vector::new([1.0, 0.0])?)?;
        seed.commit()?;

        let mut point = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
        database.stage_delta_upsert_vector(
            &mut point,
            index,
            point_object,
            Vector::new([0.0, 1.0])?,
        )?;
        let mut legacy = database.begin_optimistic(2, DurabilityClass::Group)?;
        assert!(legacy.delete_vector(index, base)?);
        let report = if point_first {
            database.commit_group(vec![
                NativeCommitBatch::from(point),
                NativeCommitBatch::from(legacy),
            ])?
        } else {
            database.commit_group(vec![
                NativeCommitBatch::from(legacy),
                NativeCommitBatch::from(point),
            ])?
        };
        assert_eq!(report.accepted_commits, 2);
        assert!(matches!(
            report.outcomes[0],
            GroupCommitOutcome::Committed(_)
        ));
        assert!(matches!(
            report.outcomes[1],
            GroupCommitOutcome::Committed(_)
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(reopened.recovery_report().committed_transactions, 3);
        let hits = reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 2)?;
        assert!(hits.iter().all(|hit| hit.object_id != base));
        assert!(hits.iter().any(|hit| hit.object_id == point_object));
    }
    Ok(())
}

#[test]
fn group_recovery_truncates_an_incomplete_second_marker_tail() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("marker-tail");
    let mut database = NativeDatabase::create(&data)?;
    let index = ObjectId::new(776)?;
    let first = ObjectId::new(1)?;
    let second = ObjectId::new(2)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "marker-tail",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;
    let mut left = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    let mut right = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    database.stage_delta_upsert_vector(&mut left, index, first, Vector::new([1.0, 0.0])?)?;
    database.stage_delta_upsert_vector(&mut right, index, second, Vector::new([0.0, 1.0])?)?;
    let report = database.commit_group(vec![left, right])?;
    assert_eq!(report.accepted_commits, 2);
    let wal = data.join("wal.hywal");
    let complete_length = fs::metadata(&wal)?.len();
    if complete_length <= 300 {
        return Err("group WAL fixture is unexpectedly short".into());
    }
    drop(database);
    fs::OpenOptions::new()
        .write(true)
        .open(&wal)?
        .set_len(complete_length - 300)?;

    let reopened = NativeDatabase::open(&data)?;
    assert!(reopened.recovery_report().wal_tail_bytes_removed > 0);
    let hits = reopened.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 2)?;
    assert!(hits.iter().any(|hit| hit.object_id == first));
    assert!(hits.iter().all(|hit| hit.object_id != second));
    Ok(())
}

#[test]
fn absent_vector_fence_commits_conflict_authority_without_ann_pages() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("absence-fence");
    let index = ObjectId::new(780)?;
    let object = ObjectId::new(1)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "absence-fence",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.commit()?;

    let mut ordinary = database.begin(2, DurabilityClass::Strict)?;
    assert!(!ordinary.delete_vector(index, object)?);
    ordinary.rollback();
    let physical_before = database.physical_observation()?;
    let mut fence = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    assert!(!database.stage_delta_vector_absence_fence(&mut fence, index, object)?);
    let committed = database.commit_optimistic(fence)?;
    let physical_after = database.physical_observation()?;
    assert_eq!(physical_after.page_count, physical_before.page_count);
    assert!(physical_after.wal_bytes > physical_before.wal_bytes);
    assert_eq!(committed.commit_csn.get(), 2);
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert!(
        reopened
            .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 1)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn absent_vector_fence_and_concurrent_upsert_conflict_in_both_orders() -> Result<(), TestError> {
    for fence_first in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if fence_first {
            "fence-first"
        } else {
            "upsert-first"
        });
        let index = ObjectId::new(785)?;
        let object = ObjectId::new(1)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "absence-race",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.commit()?;
        let mut fence = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        let mut upsert = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        assert!(!database.stage_delta_vector_absence_fence(&mut fence, index, object)?);
        database.stage_delta_upsert_vector(&mut upsert, index, object, Vector::new([1.0, 0.0])?)?;
        let rejected = if fence_first {
            database.commit_optimistic(fence)?;
            database.commit_optimistic(upsert)
        } else {
            database.commit_optimistic(upsert)?;
            database.commit_optimistic(fence)
        };
        assert!(matches!(
            rejected,
            Err(NativeRuntimeError::WriteConflict(_))
        ));
        let hits = database.search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 1)?;
        assert_eq!(hits.is_empty(), fence_first);
    }
    Ok(())
}

#[test]
fn mixed_fence_and_physical_point_writes_count_only_physical_hyanna02_operations()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("mixed-fence-physical");
    let index = ObjectId::new(787)?;
    let deleted = ObjectId::new(1)?;
    let inserted = ObjectId::new(2)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "mixed-fence-physical",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vector(index, deleted, Vector::new([0.0, 1.0])?)?;
    seed.commit()?;
    let wal_before = usize::try_from(fs::metadata(data.join("wal.hywal"))?.len())?;

    let mut mixed = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    assert!(database.stage_delta_vector_absence_fence(&mut mixed, index, deleted)?);
    assert!(!database.stage_delta_vector_absence_fence(&mut mixed, index, ObjectId::new(3)?,)?);
    database.stage_delta_upsert_vector(&mut mixed, index, inserted, Vector::new([1.0, 0.0])?)?;
    database.commit_optimistic(mixed)?;
    drop(database);

    let wal = fs::read(data.join("wal.hywal"))?;
    let wal = wal.get(wal_before..).ok_or("mixed WAL prefix moved")?;
    let markers = wal
        .windows(8)
        .enumerate()
        .filter(|(_, bytes)| *bytes == b"HYANNA02")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1);
    let count = u32::from_le_bytes(wal[markers[0] + 32..markers[0] + 36].try_into()?);
    assert_eq!(count, 2);
    assert_eq!(
        wal.windows(9)
            .filter(|bytes| *bytes == b"HYMUT001\x13")
            .count(),
        1
    );
    assert_eq!(
        wal.windows(9)
            .filter(|bytes| *bytes == b"HYMUT001\x12")
            .count(),
        1
    );
    assert_eq!(
        wal.windows(9)
            .filter(|bytes| *bytes == b"HYMUT001\x39")
            .count(),
        1
    );

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened.search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?[0].object_id,
        inserted
    );
    assert!(
        reopened
            .search_vector_exact_latest(index, &Vector::new([0.0, 1.0])?, 3)?
            .iter()
            .all(|hit| hit.object_id != deleted)
    );
    Ok(())
}

#[test]
fn group_commit_publishes_disjoint_authenticated_point_deletes() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("group-point-delete");
    let index = ObjectId::new(790)?;
    let first = ObjectId::new(1)?;
    let second = ObjectId::new(2)?;
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "group-point-delete",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vector(index, first, Vector::new([1.0, 0.0])?)?;
    seed.upsert_vector(index, second, Vector::new([0.0, 1.0])?)?;
    seed.commit()?;

    let mut left = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    let mut right = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    assert!(database.stage_delta_vector_absence_fence(&mut left, index, first)?);
    assert!(database.stage_delta_vector_absence_fence(&mut right, index, second)?);
    let report = database.commit_group(vec![left, right])?;
    assert_eq!(report.accepted_commits, 2);
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert!(
        reopened
            .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 2)?
            .is_empty()
    );
    assert!(
        reopened
            .search_ann_latest(
                index,
                &Vector::new([0.0, 0.0])?,
                AnnSearchOptions::new(2, 8, None)?
            )?
            .hits
            .is_empty()
    );
    Ok(())
}

#[test]
fn group_commit_composes_disjoint_absence_fence_and_physical_point_in_both_orders()
-> Result<(), TestError> {
    for fence_first in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if fence_first {
            "fence-first"
        } else {
            "point-first"
        });
        let index = ObjectId::new(792)?;
        let inserted = ObjectId::new(2)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "group-fence-point",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.commit()?;
        let mut fence = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
        let mut point = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
        assert!(!database.stage_delta_vector_absence_fence(
            &mut fence,
            index,
            ObjectId::new(1)?,
        )?);
        database.stage_delta_upsert_vector(
            &mut point,
            index,
            inserted,
            Vector::new([1.0, 0.0])?,
        )?;
        let report = if fence_first {
            database.commit_group(vec![fence, point])?
        } else {
            database.commit_group(vec![point, fence])?
        };
        assert_eq!(report.accepted_commits, 2);
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(
            reopened.search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 2)?[0].object_id,
            inserted
        );
    }
    Ok(())
}

#[test]
fn point_delete_removes_base_and_d02_vectors_across_reopen() -> Result<(), TestError> {
    for starts_in_base in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary
            .path()
            .join(if starts_in_base { "base" } else { "d02" });
        let index = ObjectId::new(795)?;
        let object = ObjectId::new(1)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "point-delete-source",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        if starts_in_base {
            create.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
        }
        create.commit()?;
        if !starts_in_base {
            let mut point = database.begin(2, DurabilityClass::Strict)?;
            point.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
            point.commit()?;
        }
        let mut delete = database.begin(3, DurabilityClass::Strict)?;
        assert!(delete.delete_vector(index, object)?);
        delete.commit()?;
        assert!(
            database
                .search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?
                .is_empty()
        );
        let physical_before_fence = database.physical_observation()?;
        let mut tombstone_fence = database.begin_optimistic_delta(4, DurabilityClass::Strict)?;
        assert!(!database.stage_delta_vector_absence_fence(&mut tombstone_fence, index, object,)?);
        database.commit_optimistic(tombstone_fence)?;
        assert_eq!(
            database.physical_observation()?.page_count,
            physical_before_fence.page_count
        );
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        assert!(
            reopened
                .search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?
                .is_empty()
        );
        assert!(
            reopened
                .search_ann_latest(
                    index,
                    &Vector::new([1.0, 0.0])?,
                    AnnSearchOptions::new(1, 8, None)?
                )?
                .hits
                .is_empty()
        );
    }
    Ok(())
}

#[test]
fn materialized_same_object_point_order_is_last_operation_wins() -> Result<(), TestError> {
    for delete_last in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if delete_last {
            "delete-last"
        } else {
            "upsert-last"
        });
        let index = ObjectId::new(796)?;
        let object = ObjectId::new(1)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "point-order",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        create.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
        create.commit()?;
        let mut select_m05 = database.begin(2, DurabilityClass::Strict)?;
        select_m05.upsert_vector(index, object, Vector::new([2.0, 0.0])?)?;
        select_m05.commit()?;

        let mut ordered = database.begin(3, DurabilityClass::Strict)?;
        if delete_last {
            ordered.upsert_vector(index, object, Vector::new([3.0, 0.0])?)?;
            assert!(ordered.delete_vector(index, object)?);
        } else {
            assert!(ordered.delete_vector(index, object)?);
            ordered.upsert_vector(index, object, Vector::new([3.0, 0.0])?)?;
        }
        ordered.commit()?;
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let hits = reopened.search_vector_exact_latest(index, &Vector::new([3.0, 0.0])?, 1)?;
        assert_eq!(hits.is_empty(), delete_last);
        if !delete_last {
            assert_eq!(hits[0].object_id, object);
            assert!(hits[0].distance.abs() < f64::EPSILON);
        }
    }
    Ok(())
}

#[test]
fn point_delete_crash_cuts_recover_old_or_complete_state() -> Result<(), TestError> {
    for boundary in [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(format!("delete-{boundary:?}"));
        let index = ObjectId::new(797)?;
        let object = ObjectId::new(1)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "point-delete-crash",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.upsert_vector(index, object, Vector::new([1.0, 0.0])?)?;
        seed.commit()?;
        let mut delete = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        assert!(database.stage_delta_vector_absence_fence(&mut delete, index, object)?);
        assert!(matches!(
            database.commit_optimistic_with_interruption(delete, boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let visible = reopened
            .search_vector_exact_latest(index, &Vector::new([1.0, 0.0])?, 1)?
            .iter()
            .any(|hit| hit.object_id == object);
        assert_eq!(
            reopened.recovery_report().committed_transactions == 1,
            visible
        );
    }
    Ok(())
}

#[test]
fn pure_absence_fence_crash_cuts_recover_prior_or_advanced_commit_authority()
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
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(format!("fence-{boundary:?}"));
        let index = ObjectId::new(798)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_vector_index(
            index,
            "absence-fence-crash",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.commit()?;
        let pages_before = database.physical_observation()?.page_count;
        let mut fence = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        assert!(!database.stage_delta_vector_absence_fence(
            &mut fence,
            index,
            ObjectId::new(1)?,
        )?);
        assert!(matches!(
            database.commit_optimistic_with_interruption(fence, boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let expects_fence_commit = matches!(
            boundary,
            CommitBoundary::WalAppended
                | CommitBoundary::WalSynchronized
                | CommitBoundary::RootPublished
        );
        assert_eq!(
            reopened.recovery_report().committed_transactions,
            if expects_fence_commit { 2 } else { 1 },
            "boundary {boundary:?}"
        );
        assert_eq!(reopened.physical_observation()?.page_count, pages_before);
        assert!(
            reopened
                .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 1)?
                .is_empty()
        );
    }
    Ok(())
}

#[test]
fn point_delete_and_upsert_compose_before_disjoint_rebase() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let index = ObjectId::new(780)?;
    let first = ObjectId::new(1)?;
    let second = ObjectId::new(2)?;
    let inserted = ObjectId::new(3)?;
    let stale_object = ObjectId::new(4)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "generation-fence",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vectors(
        index,
        [
            (first, Vector::new([1.0, 0.0])?),
            (second, Vector::new([0.0, 1.0])?),
        ],
    )?;
    seed.commit()?;

    let mut stale = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut stale,
        index,
        stale_object,
        Vector::new([2.0, 2.0])?,
    )?;
    let mut legacy = database.begin(2, DurabilityClass::Strict)?;
    legacy.upsert_vector(index, inserted, Vector::new([1.0, 1.0])?)?;
    assert!(legacy.delete_vector(index, first)?);
    legacy.commit()?;
    database.commit_optimistic(stale)?;
    assert!(
        database
            .search_vector_exact_latest(index, &Vector::new([2.0, 2.0])?, 4)?
            .iter()
            .any(|hit| hit.object_id == stale_object)
    );
    Ok(())
}

#[test]
fn ordered_point_delete_upsert_selects_m05_and_consolidates() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("point-consolidation"))?;
    let index = ObjectId::new(782)?;
    let first = ObjectId::new(1)?;
    let point_object = ObjectId::new(3)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(
        index,
        "point-consolidation-fence",
        2,
        VectorMetric::SquaredL2,
        ann_config()?,
    )?;
    seed.upsert_vectors(
        index,
        [
            (first, Vector::new([1.0, 0.0])?),
            (ObjectId::new(2)?, Vector::new([0.0, 1.0])?),
        ],
    )?;
    seed.commit()?;
    let mut legacy = database.begin(2, DurabilityClass::Strict)?;
    assert!(legacy.delete_vector(index, first)?);
    legacy.upsert_vector(index, first, Vector::new([2.0, 0.0])?)?;
    legacy.commit()?;
    let plan = database.plan_ann_consolidation(index, 16, 4)?;
    assert_eq!(plan.captured_delta_count(), 1);
    database.consolidate_ann(plan, DurabilityClass::Strict)?;
    assert_eq!(database.observe_ann_index(index)?.delta_records, 0);

    let mut point = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut point,
        index,
        point_object,
        Vector::new([2.0, 2.0])?,
    )?;
    database.commit_optimistic(point)?;
    assert!(
        database
            .search_vector_exact_latest(index, &Vector::new([2.0, 2.0])?, 3)?
            .iter()
            .any(|hit| hit.object_id == point_object)
    );
    Ok(())
}

#[test]
fn initial_bulk_and_overlay_points_fence_each_other_in_both_orders() -> Result<(), TestError> {
    for bulk_first in [true, false] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if bulk_first {
            "bulk-first"
        } else {
            "point-first"
        });
        let mut database = NativeDatabase::create(&data)?;
        let index = ObjectId::new(785)?;
        let point_object = ObjectId::new(10)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "bulk-point-fence",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        create.commit()?;
        let bulk = database.plan_initial_ann_bulk(
            index,
            vec![
                (ObjectId::new(1)?, Vector::new([1.0, 0.0])?),
                (ObjectId::new(2)?, Vector::new([0.0, 1.0])?),
            ],
            1,
        )?;
        let mut point = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        database.stage_delta_upsert_vector(
            &mut point,
            index,
            point_object,
            Vector::new([2.0, 2.0])?,
        )?;
        if bulk_first {
            database.publish_initial_ann_bulk(bulk, DurabilityClass::Strict)?;
            assert!(matches!(
                database.commit_optimistic(point),
                Err(NativeRuntimeError::WriteConflict(_))
            ));
        } else {
            database.commit_optimistic(point)?;
            let physical = database.physical_observation()?;
            assert!(matches!(
                database.publish_initial_ann_bulk(bulk, DurabilityClass::Strict),
                Err(NativeRuntimeError::InitialAnnBulkStale
                    | NativeRuntimeError::InvalidPreparedMutation)
            ));
            assert_eq!(
                database.physical_observation()?.page_count,
                physical.page_count
            );
            assert_eq!(
                database.physical_observation()?.wal_bytes,
                physical.wal_bytes
            );
        }
        drop(database);
        let reopened = NativeDatabase::open(&data)?;
        let hits = reopened.search_vector_exact_latest(index, &Vector::new([2.0, 2.0])?, 3)?;
        assert_eq!(
            hits.iter().any(|hit| hit.object_id == point_object),
            !bulk_first
        );
    }
    Ok(())
}

#[test]
fn initial_bulk_and_conflict_only_absence_fence_reject_stale_orderings() -> Result<(), TestError> {
    for bulk_first in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if bulk_first {
            "bulk-first"
        } else {
            "fence-first"
        });
        let index = ObjectId::new(786)?;
        let object = ObjectId::new(1)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "bulk-absence-fence",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        create.commit()?;
        let bulk =
            database.plan_initial_ann_bulk(index, vec![(object, Vector::new([1.0, 0.0])?)], 1)?;
        let mut fence = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        assert!(!database.stage_delta_vector_absence_fence(&mut fence, index, object)?);
        if bulk_first {
            database.publish_initial_ann_bulk(bulk, DurabilityClass::Strict)?;
            assert!(matches!(
                database.commit_optimistic(fence),
                Err(NativeRuntimeError::WriteConflict(_))
            ));
        } else {
            database.commit_optimistic(fence)?;
            assert!(matches!(
                database.publish_initial_ann_bulk(bulk, DurabilityClass::Strict),
                Err(NativeRuntimeError::InitialAnnBulkStale)
            ));
        }
    }
    Ok(())
}

#[test]
fn consolidation_before_a_stale_fence_conflicts_while_fence_then_plan_is_safe()
-> Result<(), TestError> {
    for consolidation_first in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if consolidation_first {
            "consolidation-first"
        } else {
            "fence-first"
        });
        let index = ObjectId::new(788)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "consolidation-absence-fence",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        create.upsert_vectors(
            index,
            [
                (ObjectId::new(1)?, Vector::new([1.0, 0.0])?),
                (ObjectId::new(2)?, Vector::new([0.0, 1.0])?),
            ],
        )?;
        create.commit()?;
        let mut legacy = database.begin(2, DurabilityClass::Strict)?;
        legacy.upsert_vector(index, ObjectId::new(3)?, Vector::new([1.0, 1.0])?)?;
        legacy.commit()?;
        let plan = database.plan_ann_consolidation(index, 16, 4)?;
        let mut fence = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
        assert!(!database.stage_delta_vector_absence_fence(
            &mut fence,
            index,
            ObjectId::new(99)?,
        )?);
        if consolidation_first {
            database.consolidate_ann(plan, DurabilityClass::Strict)?;
            assert!(matches!(
                database.commit_optimistic(fence),
                Err(NativeRuntimeError::WriteConflict(_))
            ));
        } else {
            database.commit_optimistic(fence)?;
            database.consolidate_ann(plan, DurabilityClass::Strict)?;
        }
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(
            reopened
                .search_vector_exact_latest(index, &Vector::new([0.0, 0.0])?, 8)?
                .len(),
            3
        );
    }
    Ok(())
}

#[test]
fn m05_consolidation_and_staged_hyanna02_point_fence_each_other_in_both_orders()
-> Result<(), TestError> {
    for consolidation_first in [false, true] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(if consolidation_first {
            "m05-consolidation-first"
        } else {
            "m05-point-first"
        });
        let index = ObjectId::new(789)?;
        let later_object = ObjectId::new(3)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut create = database.begin(1, DurabilityClass::Strict)?;
        create.create_vector_index(
            index,
            "m05-consolidation-point-fence",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        create.upsert_vector(index, ObjectId::new(1)?, Vector::new([1.0, 0.0])?)?;
        create.commit()?;
        let mut select_m05 = database.begin(2, DurabilityClass::Strict)?;
        select_m05.upsert_vector(index, ObjectId::new(2)?, Vector::new([0.0, 1.0])?)?;
        select_m05.commit()?;
        let plan = database.plan_ann_consolidation(index, 8, 8)?;

        let mut point = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
        database.stage_delta_upsert_vector(
            &mut point,
            index,
            later_object,
            Vector::new([2.0, 2.0])?,
        )?;
        if consolidation_first {
            database.consolidate_ann(plan, DurabilityClass::Strict)?;
            assert!(matches!(
                database.commit_optimistic(point),
                Err(NativeRuntimeError::WriteConflict(_))
            ));
        } else {
            database.commit_optimistic(point)?;
            let receipt = database.consolidate_ann(plan, DurabilityClass::Strict)?;
            assert_eq!(receipt.preserved_later_delta_records, 1);
        }
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let hits = reopened.search_vector_exact_latest(index, &Vector::new([2.0, 2.0])?, 3)?;
        assert_eq!(
            hits.iter().any(|hit| hit.object_id == later_object),
            !consolidation_first
        );
        assert_eq!(
            reopened.observe_ann_index(index)?.delta_records,
            usize::from(!consolidation_first)
        );
    }
    Ok(())
}

#[test]
fn vector_delta_crash_recovers_old_or_complete_all_engine_state() -> Result<(), TestError> {
    for boundary in [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ] {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let lexical = ObjectId::new(800)?;
        let vectors = ObjectId::new(801)?;
        let object = ObjectId::new(802)?;
        let query = Vector::new([0.0, 1.0])?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_search_index(lexical, "documents")?;
        seed.create_vector_index(
            vectors,
            "embeddings",
            2,
            VectorMetric::SquaredL2,
            ann_config()?,
        )?;
        seed.commit()?;

        let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        database.stage_delta_set(
            &mut delta,
            b"atomic-vector".to_vec(),
            b"complete".to_vec(),
            None,
        )?;
        database.stage_delta_index_document(
            &mut delta,
            lexical,
            b"atomic-document".to_vec(),
            "atomic vector delta".to_owned(),
        )?;
        database.stage_delta_upsert_vector(&mut delta, vectors, object, query.clone())?;
        assert!(matches!(
            database.commit_optimistic_with_interruption(delta, boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(3)?;
        let structure_visible = snapshot.get(b"atomic-vector").is_some();
        let lexical_visible = !snapshot.match_text(lexical, "atomic", 1)?.is_empty();
        let vector_visible = reopened
            .search_vector_exact_latest(vectors, &query, 1)?
            .first()
            .is_some_and(|hit| hit.object_id == object);
        let expects_new = matches!(
            boundary,
            CommitBoundary::WalAppended
                | CommitBoundary::WalSynchronized
                | CommitBoundary::RootPublished
        );
        assert_eq!(structure_visible, expects_new, "boundary {boundary:?}");
        assert_eq!(structure_visible, lexical_visible, "boundary {boundary:?}");
        assert_eq!(structure_visible, vector_visible, "boundary {boundary:?}");
    }
    Ok(())
}

#[test]
fn delta_set_preserves_collection_collision_and_expired_reuse_semantics() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_hash(b"expired-hash".to_vec())?;
    seed.hset(
        b"expired-hash".to_vec(),
        b"field".to_vec(),
        b"value".to_vec(),
    )?;
    assert!(seed.expire_hash(b"expired-hash".to_vec(), 10)?);
    seed.create_set(b"live-set".to_vec())?;
    seed.sadd(b"live-set".to_vec(), b"member".to_vec())?;
    seed.commit()?;

    let mut collision = database.begin_optimistic_delta(10, DurabilityClass::Memory)?;
    assert!(matches!(
        database.stage_delta_set(
            &mut collision,
            b"live-set".to_vec(),
            b"scalar".to_vec(),
            None,
        ),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));

    let mut reuse = database.begin_optimistic_delta(10, DurabilityClass::Memory)?;
    database.stage_delta_set(
        &mut reuse,
        b"expired-hash".to_vec(),
        b"scalar".to_vec(),
        None,
    )?;
    database.commit_optimistic(reuse)?;
    assert_eq!(
        database.get_latest_structure(b"expired-hash", 10)?,
        Some(b"scalar".to_vec())
    );
    Ok(())
}

#[test]
fn delta_overlay_resolves_prior_writes_for_all_three_engines() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (
            id BIGINT PRIMARY KEY,
            body TEXT NOT NULL
        )",
        &[],
    )?;
    seed.create_search_index(index, "documents")?;
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_sql_dml(
        &mut delta,
        "INSERT INTO events (id, body) VALUES (?, ?)",
        &[
            ScalarValue::Signed(1),
            ScalarValue::Text("first".to_owned()),
        ],
    )?;
    database.stage_delta_sql_dml(
        &mut delta,
        "UPDATE events SET body = ? WHERE id = ?",
        &[
            ScalarValue::Text("second".to_owned()),
            ScalarValue::Signed(1),
        ],
    )?;
    database.stage_delta_set(&mut delta, b"key".to_vec(), b"first".to_vec(), None)?;
    database.stage_delta_set(&mut delta, b"key".to_vec(), b"second".to_vec(), None)?;
    database.stage_delta_index_document(
        &mut delta,
        index,
        b"doc".to_vec(),
        "first document".to_owned(),
    )?;
    assert!(matches!(
        database.stage_delta_index_document(
            &mut delta,
            index,
            b"doc".to_vec(),
            "replacement".to_owned(),
        ),
        Err(NativeRuntimeError::Model(_))
    ));
    database.commit_optimistic(delta)?;

    let snapshot = database.snapshot(2)?;
    let prepared = snapshot.prepare_sql("SELECT body FROM events WHERE id = 1")?;
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&prepared, &[])? else {
        return Err("SELECT did not return rows".into());
    };
    assert_eq!(rows, vec![vec![ScalarValue::Text("second".to_owned())]]);
    assert_eq!(snapshot.get(b"key"), Some(b"second".as_slice()));
    assert_eq!(
        snapshot
            .match_text(index, "first", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc".to_vec()]
    );
    Ok(())
}
