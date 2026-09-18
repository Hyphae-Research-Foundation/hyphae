// SPDX-License-Identifier: Apache-2.0

//! Contract tests for point-resolved all-engine delta transactions.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use hyphae_native_runtime::{
    AnnSearchOptions, AnnSearchStrategy, CommitBoundary, HnswConfig, NativeDatabase,
    NativeRuntimeError, SqlError, SqlResult, Vector, VectorMetric,
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
fn concurrent_vector_deltas_conflict_on_the_index_sequence_authority() -> Result<(), TestError> {
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
    assert!(matches!(
        database.commit_optimistic(second),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    let hits = database.search_vector_exact_latest(vectors, &Vector::new([0.0, 1.0])?, 2)?;
    assert!(hits.iter().any(|hit| hit.object_id == first_object));
    assert!(hits.iter().all(|hit| hit.object_id != second_object));

    let mut stale_delta = database.begin_optimistic_delta(3, DurabilityClass::Strict)?;
    database.stage_delta_upsert_vector(
        &mut stale_delta,
        vectors,
        second_object,
        Vector::new([0.0, 1.0])?,
    )?;
    let mut materialized = database.begin_optimistic(3, DurabilityClass::Strict)?;
    materialized.upsert_vector(vectors, materialized_object, Vector::new([0.5, 0.5])?)?;
    database.commit_optimistic(materialized)?;
    assert!(matches!(
        database.commit_optimistic(stale_delta),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    let hits = database.search_vector_exact_latest(vectors, &Vector::new([0.0, 1.0])?, 3)?;
    assert!(hits.iter().any(|hit| hit.object_id == materialized_object));
    assert!(hits.iter().all(|hit| hit.object_id != second_object));
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
