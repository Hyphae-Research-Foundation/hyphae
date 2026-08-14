// SPDX-License-Identifier: AGPL-3.0-only

//! Durable integration contract for exact-field hash mutations in V3 deltas.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
};

use hyphae_native_runtime::{
    CommitBoundary, GroupCommitBoundary, GroupCommitOutcome, HashSetOutcome, NativeCommitBatch,
    NativeDatabase, NativeRuntimeError, SqlResult, Ttl,
};
use hyphae_native_types::{DurabilityClass, ScalarValue};

type TestError = Box<dyn std::error::Error>;

const HASH_KEY: &[u8] = b"profile";
const LARGE_VALUE_BYTES: usize = 16 * 1_024;

struct TemporaryDirectory(PathBuf);

static NEXT_TEMPORARY_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        for _ in 0..1_024 {
            let nonce = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hy-delta-hash-v3-{}-{nonce}", std::process::id()));
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

fn create_v3_hash_database(path: &Path) -> Result<NativeDatabase, TestError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_hash(HASH_KEY.to_vec())?;
    seed.hset(HASH_KEY.to_vec(), b"counter".to_vec(), b"40".to_vec())?;
    seed.hset(HASH_KEY.to_vec(), b"live".to_vec(), b"remove".to_vec())?;
    seed.commit()?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;
    Ok(database)
}

fn assert_sql_body(
    database: &NativeDatabase,
    logical_time_micros: i64,
    expected: &str,
) -> Result<(), TestError> {
    let snapshot = database.snapshot(logical_time_micros)?;
    let statement = snapshot.prepare_sql("SELECT body FROM events WHERE id = ?")?;
    let SqlResult::Rows { rows, .. } =
        snapshot.execute_prepared(&statement, &[ScalarValue::Signed(1)])?
    else {
        return Err("SELECT did not return rows".into());
    };
    assert_eq!(rows, vec![vec![ScalarValue::Text(expected.to_owned())]]);
    Ok(())
}

fn stage_mixed_delta(database: &mut NativeDatabase) -> Result<(), TestError> {
    let materialization_before = NativeDatabase::process_materialization_observation();
    let mut delta = database.begin_optimistic_delta(10, DurabilityClass::Strict)?;
    assert_eq!(
        database.stage_delta_sql_dml(
            &mut delta,
            "UPDATE events SET body = ? WHERE id = ?",
            &[
                ScalarValue::Text("after".to_owned()),
                ScalarValue::Signed(1),
            ],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    assert_eq!(
        database.stage_delta_hset(
            &mut delta,
            HASH_KEY.to_vec(),
            b"due-set".to_vec(),
            b"replaced".to_vec(),
        )?,
        HashSetOutcome::Added
    );
    let mutations_before_due_delete = delta.mutation_count();
    assert!(!database.stage_delta_hdelete(
        &mut delta,
        HASH_KEY.to_vec(),
        b"due-delete".to_vec(),
    )?);
    assert_eq!(delta.mutation_count(), mutations_before_due_delete);
    assert_eq!(
        database.stage_delta_hincrement(
            &mut delta,
            HASH_KEY.to_vec(),
            b"due-increment".to_vec(),
            5,
        )?,
        5
    );
    assert!(database.stage_delta_hdelete(
        &mut delta,
        HASH_KEY.to_vec(),
        b"live-delete".to_vec(),
    )?);
    assert_eq!(
        database.stage_delta_hset(
            &mut delta,
            HASH_KEY.to_vec(),
            b"blob".to_vec(),
            b"small".to_vec(),
        )?,
        HashSetOutcome::Updated
    );
    assert_eq!(
        NativeDatabase::process_materialization_observation(),
        materialization_before,
        "point hydration must remain bounded while staging"
    );
    database.commit_optimistic(delta)?;
    assert_eq!(
        NativeDatabase::process_materialization_observation(),
        materialization_before
    );
    Ok(())
}

fn assert_mixed_state(database: &NativeDatabase) -> Result<(), TestError> {
    for (field, expected) in [
        (b"due-set".as_slice(), Some(b"replaced".to_vec())),
        (b"due-increment".as_slice(), Some(b"5".to_vec())),
        (b"due-delete".as_slice(), None),
        (b"live-delete".as_slice(), None),
        (b"blob".as_slice(), Some(b"small".to_vec())),
    ] {
        assert_eq!(database.hget_latest_hash_at(HASH_KEY, field, 10)?, expected);
    }
    assert_eq!(
        database.ttl_latest_hash_field(HASH_KEY, b"due-set", 10)?,
        Ttl::Persistent
    );
    assert_eq!(
        database.ttl_latest_hash_field(HASH_KEY, b"due-increment", 10)?,
        Ttl::Persistent
    );
    assert_sql_body(database, 10, "after")
}

#[test]
fn mixed_sql_and_hash_delta_preserves_due_ttl_blob_and_reopen_semantics() -> Result<(), TestError> {
    let _guard = test_lock();
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.path().join("data");
    let mut database = NativeDatabase::create(&path)?;
    let original_blob = vec![0x41; LARGE_VALUE_BYTES];

    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql("INSERT INTO events (id, body) VALUES (1, 'before')", &[])?;
    seed.create_hash(HASH_KEY.to_vec())?;
    for (field, value) in [
        (b"due-set".as_slice(), b"old".as_slice()),
        (b"due-delete".as_slice(), b"old".as_slice()),
        (b"due-increment".as_slice(), b"41".as_slice()),
        (b"live-delete".as_slice(), b"old".as_slice()),
    ] {
        seed.hset(HASH_KEY.to_vec(), field.to_vec(), value.to_vec())?;
    }
    seed.hset(HASH_KEY.to_vec(), b"blob".to_vec(), original_blob)?;
    for field in [
        b"due-set".as_slice(),
        b"due-delete".as_slice(),
        b"due-increment".as_slice(),
    ] {
        assert!(seed.expire_hash_field(HASH_KEY.to_vec(), field.to_vec(), 10)?);
    }
    seed.commit()?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;

    stage_mixed_delta(&mut database)?;
    assert_mixed_state(&database)?;

    let cleanup = database.expire_due_structures(10, 8, DurabilityClass::Strict)?;
    assert_eq!(cleanup.expired_keys, 1);
    assert!(!cleanup.more_due);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_mixed_state(&reopened)?;
    Ok(())
}

#[test]
fn disjoint_hash_fields_rebase_but_the_same_field_conflicts() -> Result<(), TestError> {
    let _guard = test_lock();
    let temporary = TemporaryDirectory::create()?;
    let mut database = create_v3_hash_database(&temporary.path().join("data"))?;

    let mut first = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    let mut second = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_hset(
        &mut first,
        HASH_KEY.to_vec(),
        b"alpha".to_vec(),
        b"one".to_vec(),
    )?;
    database.stage_delta_hset(
        &mut second,
        HASH_KEY.to_vec(),
        b"beta".to_vec(),
        b"two".to_vec(),
    )?;
    database.commit_optimistic(first)?;
    database.commit_optimistic(second)?;
    assert_eq!(
        database.hget_latest_hash(HASH_KEY, b"alpha")?,
        Some(b"one".to_vec())
    );
    assert_eq!(
        database.hget_latest_hash(HASH_KEY, b"beta")?,
        Some(b"two".to_vec())
    );

    let mut winner = database.begin_optimistic_delta(3, DurabilityClass::Memory)?;
    let mut loser = database.begin_optimistic_delta(3, DurabilityClass::Memory)?;
    database.stage_delta_hset(
        &mut winner,
        HASH_KEY.to_vec(),
        b"race".to_vec(),
        b"winner".to_vec(),
    )?;
    database.stage_delta_hincrement(&mut loser, HASH_KEY.to_vec(), b"race".to_vec(), 1)?;
    database.commit_optimistic(winner)?;
    assert!(matches!(
        database.commit_optimistic(loser),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert_eq!(
        database.hget_latest_hash(HASH_KEY, b"race")?,
        Some(b"winner".to_vec())
    );
    Ok(())
}

#[test]
fn every_commit_boundary_recovers_the_prior_or_complete_hash_delta() -> Result<(), TestError> {
    let _guard = test_lock();
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
        let path = temporary.path().join("data");
        let mut database = create_v3_hash_database(&path)?;
        let replacement_blob = vec![0x52; LARGE_VALUE_BYTES];
        let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Strict)?;
        database.stage_delta_hset(
            &mut delta,
            HASH_KEY.to_vec(),
            b"blob".to_vec(),
            replacement_blob.clone(),
        )?;
        assert_eq!(
            database.stage_delta_hincrement(
                &mut delta,
                HASH_KEY.to_vec(),
                b"counter".to_vec(),
                2,
            )?,
            42
        );
        assert!(database.stage_delta_hdelete(&mut delta, HASH_KEY.to_vec(), b"live".to_vec(),)?);

        assert!(matches!(
            database.commit_optimistic_with_interruption(delta, boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&path)?;
        let blob = reopened.hget_latest_hash(HASH_KEY, b"blob")?;
        let counter = reopened.hget_latest_hash(HASH_KEY, b"counter")?;
        let live = reopened.hget_latest_hash(HASH_KEY, b"live")?;
        let prior = blob.is_none() && counter.as_deref() == Some(b"40") && live.is_some();
        let complete = blob.as_deref() == Some(replacement_blob.as_slice())
            && counter.as_deref() == Some(b"42")
            && live.is_none();
        assert!(prior || complete, "partial hash delta at {boundary:?}");
    }
    Ok(())
}

#[test]
fn group_cohort_commits_two_disjoint_hash_fields_with_one_flush() -> Result<(), TestError> {
    let _guard = test_lock();
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.path().join("data");
    let mut database = create_v3_hash_database(&path)?;

    let mut first = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    let mut second = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    database.stage_delta_hset(
        &mut first,
        HASH_KEY.to_vec(),
        b"group-a".to_vec(),
        b"one".to_vec(),
    )?;
    assert_eq!(
        database.stage_delta_hincrement(&mut second, HASH_KEY.to_vec(), b"group-b".to_vec(), 2,)?,
        2
    );

    let report = database.commit_group(vec![first, second])?;
    assert_eq!(report.accepted_commits, 2);
    assert_eq!(report.page_synchronizations, 1);
    assert_eq!(report.wal_synchronizations, 1);
    let [
        GroupCommitOutcome::Committed(first),
        GroupCommitOutcome::Committed(second),
    ] = report.outcomes.as_slice()
    else {
        return Err("hash delta cohort did not commit both requests".into());
    };
    assert_eq!(first.durability_cohort_size, 2);
    assert_eq!(second.durability_cohort_size, 2);
    assert_eq!(first.durability_cohort_position, 0);
    assert_eq!(second.durability_cohort_position, 1);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.hget_latest_hash(HASH_KEY, b"group-a")?,
        Some(b"one".to_vec())
    );
    assert_eq!(
        reopened.hget_latest_hash(HASH_KEY, b"group-b")?,
        Some(b"2".to_vec())
    );
    Ok(())
}

#[test]
fn hash_field_and_hash_lifecycle_conflicts_preserve_commit_order() -> Result<(), TestError> {
    let _guard = test_lock();

    let first_directory = TemporaryDirectory::create()?;
    let mut database = create_v3_hash_database(&first_directory.path().join("whole-first"))?;
    let mut field = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_hset(
        &mut field,
        HASH_KEY.to_vec(),
        b"field".to_vec(),
        b"value".to_vec(),
    )?;
    let mut lifecycle = database.begin_optimistic(2, DurabilityClass::Memory)?;
    assert!(lifecycle.delete_hash(HASH_KEY.to_vec())?);
    database.commit_optimistic(lifecycle)?;
    assert!(matches!(
        database.commit_optimistic(field),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert!(matches!(
        database.hget_latest_hash(HASH_KEY, b"field"),
        Err(NativeRuntimeError::UnknownStructureHash)
    ));

    let second_directory = TemporaryDirectory::create()?;
    let mut database = create_v3_hash_database(&second_directory.path().join("field-first"))?;
    let mut field = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_hset(
        &mut field,
        HASH_KEY.to_vec(),
        b"field".to_vec(),
        b"value".to_vec(),
    )?;
    let mut lifecycle = database.begin_optimistic(2, DurabilityClass::Memory)?;
    assert!(lifecycle.delete_hash(HASH_KEY.to_vec())?);
    database.commit_optimistic(field)?;
    database.commit_optimistic(lifecycle)?;
    assert!(matches!(
        database.hget_latest_hash(HASH_KEY, b"field"),
        Err(NativeRuntimeError::UnknownStructureHash)
    ));
    Ok(())
}

#[test]
fn group_cohort_rejects_same_field_and_hash_lifecycle_conflicts() -> Result<(), TestError> {
    let _guard = test_lock();
    let temporary = TemporaryDirectory::create()?;
    let mut database = create_v3_hash_database(&temporary.path().join("data"))?;

    let mut first = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    let mut same_field = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    for (batch, value) in [
        (&mut first, b"first".as_slice()),
        (&mut same_field, b"second"),
    ] {
        database.stage_delta_hset(batch, HASH_KEY.to_vec(), b"cohort".to_vec(), value.to_vec())?;
    }
    let report = database.commit_group(vec![first, same_field])?;
    assert!(matches!(
        report.outcomes.as_slice(),
        [
            GroupCommitOutcome::Committed(_),
            GroupCommitOutcome::Rejected(NativeRuntimeError::WriteConflict(_))
        ]
    ));

    let field_first_directory = TemporaryDirectory::create()?;
    let mut database = create_v3_hash_database(&field_first_directory.path().join("field-first"))?;
    let mut field = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    database.stage_delta_hset(
        &mut field,
        HASH_KEY.to_vec(),
        b"lifecycle".to_vec(),
        b"value".to_vec(),
    )?;
    let mut lifecycle = database.begin_optimistic(2, DurabilityClass::Group)?;
    assert!(lifecycle.delete_hash(HASH_KEY.to_vec())?);
    let report = database.commit_group(vec![
        NativeCommitBatch::from(field),
        NativeCommitBatch::from(lifecycle),
    ])?;
    assert!(matches!(
        report.outcomes.as_slice(),
        [
            GroupCommitOutcome::Committed(_),
            GroupCommitOutcome::Committed(_)
        ]
    ));
    assert!(matches!(
        database.hget_latest_hash(HASH_KEY, b"lifecycle"),
        Err(NativeRuntimeError::UnknownStructureHash)
    ));

    let lifecycle_first_directory = TemporaryDirectory::create()?;
    let mut database =
        create_v3_hash_database(&lifecycle_first_directory.path().join("lifecycle-first"))?;
    let mut lifecycle = database.begin_optimistic(2, DurabilityClass::Group)?;
    assert!(lifecycle.delete_hash(HASH_KEY.to_vec())?);
    let mut field = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
    database.stage_delta_hset(
        &mut field,
        HASH_KEY.to_vec(),
        b"lifecycle".to_vec(),
        b"value".to_vec(),
    )?;
    let report = database.commit_group(vec![
        NativeCommitBatch::from(lifecycle),
        NativeCommitBatch::from(field),
    ])?;
    assert!(matches!(
        report.outcomes.as_slice(),
        [
            GroupCommitOutcome::Committed(_),
            GroupCommitOutcome::Rejected(NativeRuntimeError::WriteConflict(_))
        ]
    ));
    Ok(())
}

#[test]
fn every_group_boundary_recovers_a_hash_delta_prefix_or_complete_cohort() -> Result<(), TestError> {
    let _guard = test_lock();
    for boundary in [
        GroupCommitBoundary::AdmittedWalPrefixAppended,
        GroupCommitBoundary::CohortAppended,
        GroupCommitBoundary::PageSynchronized,
        GroupCommitBoundary::WalSynchronized,
        GroupCommitBoundary::RootPublished,
    ] {
        let temporary = TemporaryDirectory::create()?;
        let path = temporary.path().join(format!("{boundary:?}"));
        let mut database = create_v3_hash_database(&path)?;
        let mut first = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
        let mut second = database.begin_optimistic_delta(2, DurabilityClass::Group)?;
        database.stage_delta_hset(
            &mut first,
            HASH_KEY.to_vec(),
            b"crash-a".to_vec(),
            b"one".to_vec(),
        )?;
        database.stage_delta_hset(
            &mut second,
            HASH_KEY.to_vec(),
            b"crash-b".to_vec(),
            b"two".to_vec(),
        )?;
        assert!(matches!(
            database.commit_group_with_interruption(vec![first, second], boundary),
            Err(NativeRuntimeError::InjectedGroupCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&path)?;
        let first = reopened.hget_latest_hash(HASH_KEY, b"crash-a")?;
        let second = reopened.hget_latest_hash(HASH_KEY, b"crash-b")?;
        assert!(
            first.is_some() || second.is_none(),
            "non-prefix recovery at {boundary:?}"
        );
        if matches!(
            boundary,
            GroupCommitBoundary::WalSynchronized | GroupCommitBoundary::RootPublished
        ) {
            assert_eq!(first, Some(b"one".to_vec()));
            assert_eq!(second, Some(b"two".to_vec()));
        }
    }
    Ok(())
}
