// SPDX-License-Identifier: Apache-2.0

//! Bounded residual `LIKE` and `IN (list)` filter conformance.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-sql-like-in-{}-{nanos}",
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
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, region TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql_dml(
        "INSERT INTO users (id, name, region) VALUES \
         (1, 'alice', 'emea'), (2, 'albert', 'amer'), (3, 'bob', 'emea'), \
         (4, NULL, 'apac'), (5, 'carol', 'amer')",
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

#[test]
fn like_filters_are_residual_and_null_safe() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;
    let prepared = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND name LIKE 'al%' LIMIT 10",
    )?;
    let rows =
        ids(database
            .execute_prepared_latest(&prepared, &[SqlValue::Signed(1), SqlValue::Signed(5)])?)?;
    assert_eq!(rows, vec![1, 2]);

    // NULL name is unknown, never matched, and NOT LIKE does not resurrect it.
    let negated = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND name NOT LIKE 'al%' LIMIT 10",
    )?;
    let rows =
        ids(database
            .execute_prepared_latest(&negated, &[SqlValue::Signed(1), SqlValue::Signed(5)])?)?;
    assert_eq!(rows, vec![3, 5]);

    // Underscore matches exactly one character.
    let underscore = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND name LIKE 'b_b' LIMIT 10",
    )?;
    let rows = ids(database
        .execute_prepared_latest(&underscore, &[SqlValue::Signed(1), SqlValue::Signed(5)])?)?;
    assert_eq!(rows, vec![3]);
    Ok(())
}

#[test]
fn in_filters_match_lists_with_parameters_and_null_semantics() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    let list = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND region IN ('emea', ?) LIMIT 10",
    )?;
    let rows = ids(database.execute_prepared_latest(
        &list,
        &[
            SqlValue::Signed(1),
            SqlValue::Signed(5),
            SqlValue::Text("apac".to_owned()),
        ],
    )?)?;
    assert_eq!(rows, vec![1, 3, 4]);

    // NOT IN with a NULL member is unknown for non-matching rows: no rows.
    let not_in_null = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND region NOT IN ('emea', NULL) LIMIT 10",
    )?;
    let rows = ids(database
        .execute_prepared_latest(&not_in_null, &[SqlValue::Signed(1), SqlValue::Signed(5)])?)?;
    assert!(rows.is_empty(), "NOT IN with NULL member matches nothing");

    // Integer lists over the primary key column as residual predicate.
    let numbers = database.prepare_sql_latest(
        "SELECT id FROM users WHERE id >= ? AND id <= ? AND id IN (2, 4, 99) LIMIT 10",
    )?;
    let rows =
        ids(database
            .execute_prepared_latest(&numbers, &[SqlValue::Signed(1), SqlValue::Signed(5)])?)?;
    assert_eq!(rows, vec![2, 4]);
    Ok(())
}

#[test]
fn like_and_in_shapes_fail_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // LIKE over a non-text column.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT id FROM users WHERE id >= ? AND id <= ? AND id LIKE 'a%' LIMIT 10",
            &[SqlValue::Signed(1), SqlValue::Signed(5)],
        ),
        Err(SqlError::TypeMismatch)
    ));
    // LIKE pattern must be a literal string, not a parameter.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT id FROM users WHERE id >= ? AND id <= ? AND name LIKE ? LIMIT 10",
            &[
                SqlValue::Signed(1),
                SqlValue::Signed(5),
                SqlValue::Text("a%".to_owned()),
            ],
        ),
        Err(SqlError::InvalidSyntax)
    ));
    // LIKE/IN never create an access path on their own.
    assert!(matches!(
        transaction.execute_sql("SELECT id FROM users WHERE name LIKE 'al%'", &[]),
        Err(SqlError::InvalidSyntax)
    ));
    assert!(matches!(
        transaction.execute_sql("SELECT id FROM users WHERE region IN ('emea')", &[]),
        Err(SqlError::InvalidSyntax)
    ));
    // Empty IN list is a syntax error.
    assert!(
        transaction
            .execute_sql(
                "SELECT id FROM users WHERE id >= ? AND id <= ? AND region IN () LIMIT 10",
                &[SqlValue::Signed(1), SqlValue::Signed(5)],
            )
            .is_err()
    );
    transaction.rollback();
    Ok(())
}

const SURFACE_QUERY: &str = "SELECT id FROM users WHERE id >= ? AND id <= ? \
     AND name LIKE '%a%' AND region IN ('amer') LIMIT 10";

#[test]
fn like_and_in_work_across_all_three_surfaces() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let parameters = [SqlValue::Signed(1), SqlValue::Signed(5)];

    // Transactional.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let rows = ids(transaction.execute_sql(SURFACE_QUERY, &parameters)?)?;
    assert_eq!(rows, vec![2, 5]);
    transaction.rollback();

    // Prepared snapshot.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(SURFACE_QUERY)?;
    let rows = ids(snapshot.execute_prepared(&prepared, &parameters)?)?;
    assert_eq!(rows, vec![2, 5]);

    // Prepared latest.
    let latest = database.prepare_sql_latest(SURFACE_QUERY)?;
    let rows = ids(database.execute_prepared_latest(&latest, &parameters)?)?;
    assert_eq!(rows, vec![2, 5]);
    Ok(())
}
