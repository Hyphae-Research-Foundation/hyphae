// SPDX-License-Identifier: Apache-2.0

//! Multi-row `INSERT ... VALUES (...),(...)` bounded conformance.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

type TestError = Box<dyn std::error::Error>;

static NEXT_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        // Tests in this binary run in parallel; a clock alone collides on
        // hosts whose timer resolution is coarser than a test start, so the
        // name carries a process-wide sequence as well.
        let sequence = NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-sql-multirow-{}-{sequence}-{nanos}",
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
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL, \
         label TEXT NOT NULL)",
        &[],
    )?;
    seed.commit()?;
    Ok(database)
}

#[test]
fn multi_row_insert_commits_every_row_with_literals_and_parameters() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;

    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let result = batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES \
         (1, 100, 'alpha'), (2, 200, 'beta'), (?, ?, ?)",
        &[
            SqlValue::Signed(3),
            SqlValue::Signed(300),
            SqlValue::Text("gamma".to_owned()),
        ],
    )?;
    assert_eq!(
        result,
        SqlResult::Command {
            rows_affected: 3,
            object_id: None,
        }
    );
    database.commit_optimistic(batch)?;

    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT id, balance, label FROM accounts WHERE id = ?")?;
    for (id, balance, label) in [(1, 100, "alpha"), (2, 200, "beta"), (3, 300, "gamma")] {
        let SqlResult::Rows { rows, .. } =
            snapshot.execute_prepared(&prepared, &[SqlValue::Signed(id)])?
        else {
            return Err("expected rows".into());
        };
        assert_eq!(
            rows,
            vec![vec![
                SqlValue::Signed(id),
                SqlValue::Signed(balance),
                SqlValue::Text(label.to_owned()),
            ]]
        );
    }
    Ok(())
}

#[test]
fn multi_row_insert_is_atomic_when_one_row_fails() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    // Second row violates NOT NULL: the whole batch must fail closed and no
    // row may become visible after the batch is dropped.
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let outcome = batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES \
         (10, 1, 'kept'), (11, NULL, 'broken')",
        &[],
    );
    assert!(matches!(outcome, Err(SqlError::NullViolation)));
    drop(batch);

    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT id FROM accounts WHERE id = ?")?;
    let SqlResult::Rows { rows, .. } =
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(10)])?
    else {
        return Err("expected rows".into());
    };
    assert!(rows.is_empty(), "failed batch must not leak row 10");
    Ok(())
}

#[test]
fn multi_row_insert_duplicate_primary_key_fails_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let outcome = batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES \
         (20, 1, 'first'), (20, 2, 'duplicate')",
        &[],
    );
    assert!(
        outcome.is_err(),
        "duplicate primary key inside one VALUES list must fail"
    );
    Ok(())
}

#[test]
fn multi_row_insert_stays_off_the_delta_staging_path() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Memory)?;
    let staged = database.stage_delta_sql_dml(
        &mut batch,
        "INSERT INTO accounts (id, balance, label) VALUES (30, 1, 'a'), (31, 2, 'b')",
        &[],
    );
    assert!(matches!(staged, Err(SqlError::InvalidSyntax)));
    batch.rollback();
    Ok(())
}

#[test]
fn multi_row_insert_row_budget_fails_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    let mut statement =
        String::from("INSERT INTO accounts (id, balance, label) VALUES (0, 0, 'x')");
    for row in 1..=hyphae_native_runtime::MAX_SQL_INSERT_ROWS {
        use std::fmt::Write as _;
        let _ = write!(statement, ", ({row}, 0, 'x')");
    }
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let outcome = batch.execute_sql_dml(&statement, &[]);
    assert!(matches!(outcome, Err(SqlError::InsertRowBudgetExceeded)));
    Ok(())
}
