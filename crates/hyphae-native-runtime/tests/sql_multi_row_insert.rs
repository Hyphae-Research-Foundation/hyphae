// SPDX-License-Identifier: Apache-2.0

//! Multi-row `INSERT ... VALUES (...),(...)` bounded conformance.

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlError, SqlResult, SqlValue};
use hyphae_native_types::{DurabilityClass, ObjectId};

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
fn direct_insert_reports_primary_key_conflict_without_replacing_the_row() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let table = ObjectId::new(1)?;
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    batch.create_relation(table, "accounts")?;
    batch.insert(table, b"one".to_vec(), b"original".to_vec())?;
    let mutation_count = batch.mutation_count();

    assert!(matches!(
        batch.insert(table, b"one".to_vec(), b"replacement".to_vec()),
        Err(NativeRuntimeError::UniquePrimaryKeyViolation)
    ));
    assert_eq!(batch.mutation_count(), mutation_count);
    assert_eq!(batch.select(table, b"one"), Some(b"original".as_slice()));
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    assert_eq!(
        reopened.snapshot(0)?.select(table, b"one"),
        Some(b"original".as_slice())
    );
    Ok(())
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
fn multi_row_insert_persisted_primary_key_conflict_restores_batch() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut seed = database.begin_optimistic(0, DurabilityClass::Strict)?;
    seed.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES (1, 100, 'original')",
        &[],
    )?;
    database.commit_optimistic(seed)?;

    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = batch.mutation_count();
    let outcome = batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES \
         (20, 1, 'first'), (1, 2, 'replacement')",
        &[],
    );
    assert!(matches!(outcome, Err(SqlError::UniqueViolation)));
    assert_eq!(batch.mutation_count(), mutation_count);
    assert_eq!(
        batch.execute_sql("SELECT balance, label FROM accounts WHERE id = 1", &[])?,
        SqlResult::Rows {
            columns: vec!["balance".to_owned(), "label".to_owned()],
            rows: vec![vec![
                SqlValue::Signed(100),
                SqlValue::Text("original".to_owned()),
            ]],
        }
    );
    assert_eq!(
        batch.execute_sql("SELECT id FROM accounts WHERE id = 20", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES (30, 3, 'after')",
        &[],
    )?;
    database.commit_optimistic(batch)?;
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    let snapshot = reopened.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT label FROM accounts WHERE id = ?")?;
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(1)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: vec![vec![SqlValue::Text("original".to_owned())]],
        }
    );
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(20)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: Vec::new(),
        }
    );
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(30)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: vec![vec![SqlValue::Text("after".to_owned())]],
        }
    );
    Ok(())
}

#[test]
fn multi_row_insert_same_statement_primary_key_conflict_restores_batch() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = batch.mutation_count();

    assert!(matches!(
        batch.execute_sql_dml(
            "INSERT INTO accounts (id, balance, label) VALUES \
             (20, 1, 'first'), (20, 2, 'duplicate')",
            &[],
        ),
        Err(SqlError::UniqueViolation)
    ));
    assert_eq!(batch.mutation_count(), mutation_count);
    assert_eq!(
        batch.execute_sql("SELECT id FROM accounts WHERE id = 20", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES (30, 3, 'after')",
        &[],
    )?;
    database.commit_optimistic(batch)?;

    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT label FROM accounts WHERE id = ?")?;
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(20)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: Vec::new(),
        }
    );
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(30)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: vec![vec![SqlValue::Text("after".to_owned())]],
        }
    );
    Ok(())
}

#[test]
fn multi_row_insert_later_foreign_key_failure_restores_batch() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin_sql(0, DurabilityClass::Strict)?;
    seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    seed.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
    seed.commit()?;

    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = batch.mutation_count();
    assert!(matches!(
        batch.execute_sql_dml(
            "INSERT INTO children (id, parent_id) VALUES (10, 1), (11, 999)",
            &[],
        ),
        Err(SqlError::ForeignKeyViolation)
    ));
    assert_eq!(batch.mutation_count(), mutation_count);
    assert_eq!(
        batch.execute_sql("SELECT id FROM children WHERE id = 10", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    batch.execute_sql_dml("INSERT INTO children (id, parent_id) VALUES (12, 1)", &[])?;
    database.commit_optimistic(batch)?;

    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT parent_id FROM children WHERE id = ?")?;
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(10)])?,
        SqlResult::Rows {
            columns: vec!["parent_id".to_owned()],
            rows: Vec::new(),
        }
    );
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(12)])?,
        SqlResult::Rows {
            columns: vec!["parent_id".to_owned()],
            rows: vec![vec![SqlValue::Signed(1)]],
        }
    );
    Ok(())
}

#[test]
fn multi_row_insert_reports_the_first_row_constraint_failure() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin_sql(0, DurabilityClass::Strict)?;
    seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    seed.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, \
         FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
    seed.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 1)", &[])?;
    seed.commit()?;

    let mut foreign_key_first = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = foreign_key_first.mutation_count();
    assert!(matches!(
        foreign_key_first.execute_sql_dml(
            "INSERT INTO children (id, parent_id) VALUES (10, 999), (1, 1)",
            &[],
        ),
        Err(SqlError::ForeignKeyViolation)
    ));
    assert_eq!(foreign_key_first.mutation_count(), mutation_count);
    assert_eq!(
        foreign_key_first.execute_sql("SELECT id FROM children WHERE id = 10", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    foreign_key_first.rollback();

    let mut primary_key_first = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = primary_key_first.mutation_count();
    assert!(matches!(
        primary_key_first.execute_sql_dml(
            "INSERT INTO children (id, parent_id) VALUES (1, 1), (10, 999)",
            &[],
        ),
        Err(SqlError::UniqueViolation)
    ));
    assert_eq!(primary_key_first.mutation_count(), mutation_count);
    assert_eq!(
        primary_key_first.execute_sql("SELECT id FROM children WHERE id = 10", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    primary_key_first.rollback();
    Ok(())
}

#[test]
fn multi_row_insert_later_secondary_unique_failure_restores_batch() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut seed = database.begin_sql(0, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE UNIQUE INDEX accounts_label ON accounts (label)",
        &[],
    )?;
    seed.execute_sql(
        "INSERT INTO accounts (id, balance, label) VALUES (1, 100, 'original')",
        &[],
    )?;
    seed.commit()?;

    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    let mutation_count = batch.mutation_count();
    assert!(matches!(
        batch.execute_sql_dml(
            "INSERT INTO accounts (id, balance, label) VALUES \
             (20, 1, 'first'), (21, 2, 'original')",
            &[],
        ),
        Err(SqlError::UniqueViolation)
    ));
    assert_eq!(batch.mutation_count(), mutation_count);
    assert_eq!(
        batch.execute_sql("SELECT id FROM accounts WHERE id = 20", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: Vec::new(),
        }
    );
    batch.execute_sql_dml(
        "INSERT INTO accounts (id, balance, label) VALUES (30, 3, 'after')",
        &[],
    )?;
    database.commit_optimistic(batch)?;

    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql("SELECT label FROM accounts WHERE id = ?")?;
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(20)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: Vec::new(),
        }
    );
    assert_eq!(
        snapshot.execute_prepared(&prepared, &[SqlValue::Signed(30)])?,
        SqlResult::Rows {
            columns: vec!["label".to_owned()],
            rows: vec![vec![SqlValue::Text("after".to_owned())]],
        }
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
