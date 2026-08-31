// SPDX-License-Identifier: Apache-2.0

//! `ORDER BY <primary key> DESC LIMIT n` bounded conformance.

use hyphae_native_runtime::{NativeDatabase, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-sql-desc-{}-{nanos}",
            std::process::id()
        ))))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

fn seeded_database(path: &std::path::Path) -> Result<NativeDatabase, TestError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, label TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql_dml(
        "INSERT INTO events (id, label) VALUES \
         (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four'), (5, 'five')",
        &[],
    )?;
    seed.commit()?;
    Ok(database)
}

fn ids(result: SqlResult) -> Result<Vec<i64>, TestError> {
    let SqlResult::Rows { rows, .. } = result else {
        return Err("expected rows".into());
    };
    Ok(rows
        .into_iter()
        .filter_map(|row| match row.first() {
            Some(SqlValue::Signed(id)) => Some(*id),
            _ => None,
        })
        .collect())
}

const DESC_QUERY: &str = "SELECT id FROM events ORDER BY id DESC LIMIT 3";

#[test]
fn descending_scan_returns_latest_rows_first_on_all_surfaces() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;

    // Transactional.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    assert_eq!(
        ids(transaction.execute_sql(DESC_QUERY, &[])?)?,
        vec![5, 4, 3]
    );
    transaction.rollback();

    // Prepared snapshot.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(DESC_QUERY)?;
    assert_eq!(
        ids(snapshot.execute_prepared(&prepared, &[])?)?,
        vec![5, 4, 3]
    );

    // Prepared latest (physical reverse walk).
    let latest = database.prepare_sql_latest(DESC_QUERY)?;
    assert_eq!(
        ids(database.execute_prepared_latest(&latest, &[])?)?,
        vec![5, 4, 3]
    );

    // ASC remains the default and explicit ASC is accepted.
    let ascending = database.prepare_sql_latest("SELECT id FROM events ORDER BY id ASC LIMIT 3")?;
    assert_eq!(
        ids(database.execute_prepared_latest(&ascending, &[])?)?,
        vec![1, 2, 3]
    );
    Ok(())
}

#[test]
fn descending_range_scan_walks_the_bounded_window_backwards() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;
    let prepared = database.prepare_sql_latest(
        "SELECT id FROM events WHERE id >= ? AND id <= ? ORDER BY id DESC LIMIT 10",
    )?;
    assert_eq!(
        ids(database
            .execute_prepared_latest(&prepared, &[SqlValue::Signed(2), SqlValue::Signed(4)],)?)?,
        vec![4, 3, 2]
    );
    // New rows are visible to the same reversed plan.
    let mut database = database;
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    batch.execute_sql_dml("INSERT INTO events (id, label) VALUES (6, 'six')", &[])?;
    database.commit_optimistic(batch)?;
    let wide = database.prepare_sql_latest(
        "SELECT id FROM events WHERE id >= ? AND id <= ? ORDER BY id DESC LIMIT 2",
    )?;
    assert_eq!(
        ids(database.execute_prepared_latest(&wide, &[SqlValue::Signed(1), SqlValue::Signed(6)],)?)?,
        vec![6, 5]
    );
    Ok(())
}

#[test]
fn descending_shapes_outside_the_slice_fail_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // DESC without ORDER BY column list is unreachable grammar; DESC over a
    // point lookup, prefix scan, or secondary path fails closed.
    assert!(
        transaction
            .execute_sql(
                "SELECT id FROM events WHERE id = ? ORDER BY id DESC",
                &[SqlValue::Signed(1)]
            )
            .is_err()
    );
    // DESC over a non-primary-key column fails closed.
    assert!(
        transaction
            .execute_sql("SELECT id FROM events ORDER BY label DESC LIMIT 3", &[])
            .is_err()
    );
    transaction.rollback();
    Ok(())
}
