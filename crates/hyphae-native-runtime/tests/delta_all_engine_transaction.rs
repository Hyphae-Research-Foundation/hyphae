// SPDX-License-Identifier: Apache-2.0

//! Contract tests for point-resolved all-engine delta transactions.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, SqlResult};
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
