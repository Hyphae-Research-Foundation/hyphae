// SPDX-License-Identifier: Apache-2.0

//! Contract tests for point-resolved all-engine delta transactions.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlError, SqlResult};
use hyphae_native_types::{DurabilityClass, ObjectId, ScalarValue};

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = Path::new("/tmp").join(format!(
            "hy-delta-transaction-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
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
