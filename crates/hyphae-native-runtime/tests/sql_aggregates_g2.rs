// SPDX-License-Identifier: Apache-2.0

//! Bounded total-aggregate conformance: COUNT/SUM/MIN/MAX/AVG across the
//! transactional, prepared-snapshot, and prepared-latest surfaces.

use hyphae_native_runtime::{NativeDatabase, SqlError, SqlResult, SqlValue};
use hyphae_native_types::{CanonicalF64, DurabilityClass};

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-sql-aggregates-{}-{nanos}",
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

/// Seeds ledger: 5 rows, one NULL amount, mixed tenants.
fn seeded_database(path: &std::path::Path) -> Result<NativeDatabase, TestError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE ledger (id BIGINT PRIMARY KEY, tenant TEXT NOT NULL, amount BIGINT)",
        &[],
    )?;
    seed.execute_sql_dml(
        "INSERT INTO ledger (id, tenant, amount) VALUES \
         (1, 'alpha', 10), (2, 'alpha', 30), (3, 'beta', NULL), \
         (4, 'beta', -5), (5, 'alpha', 25)",
        &[],
    )?;
    seed.commit()?;
    Ok(database)
}

const AGGREGATE_QUERY: &str = "SELECT COUNT(*), COUNT(amount), SUM(amount), MIN(amount), \
     MAX(amount), AVG(amount) FROM ledger WHERE id >= ? AND id <= ?";

fn expected_row() -> Vec<SqlValue> {
    vec![
        SqlValue::Unsigned(5),
        SqlValue::Unsigned(4),
        SqlValue::Signed(60),
        SqlValue::Signed(-5),
        SqlValue::Signed(30),
        SqlValue::Float64(CanonicalF64::new(15.0)),
    ]
}

fn range_parameters() -> Vec<SqlValue> {
    vec![SqlValue::Signed(1), SqlValue::Signed(5)]
}

#[test]
fn aggregates_fold_identically_across_all_three_surfaces() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;

    // Transactional surface.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let SqlResult::Rows { columns, rows } =
        transaction.execute_sql(AGGREGATE_QUERY, &range_parameters())?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        columns,
        vec![
            "COUNT(*)",
            "COUNT(amount)",
            "SUM(amount)",
            "MIN(amount)",
            "MAX(amount)",
            "AVG(amount)",
        ]
    );
    assert_eq!(rows, vec![expected_row()]);
    transaction.rollback();

    // Prepared snapshot surface.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(AGGREGATE_QUERY)?;
    assert_eq!(prepared.maximum_result_rows(), Some(1));
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&prepared, &range_parameters())?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, vec![expected_row()]);

    // Prepared latest surface (physical roots).
    let prepared_latest = database.prepare_sql_latest(AGGREGATE_QUERY)?;
    let SqlResult::Rows { rows, .. } =
        database.execute_prepared_latest(&prepared_latest, &range_parameters())?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, vec![expected_row()]);
    Ok(())
}

#[test]
fn count_star_over_full_table_without_limit_is_admitted() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;
    let prepared = database.prepare_sql_latest("SELECT COUNT(*) FROM ledger")?;
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(&prepared, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, vec![vec![SqlValue::Unsigned(5)]]);
    Ok(())
}

#[test]
fn aggregates_over_empty_match_emit_null_and_zero_counts() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;
    let prepared = database.prepare_sql_latest(
        "SELECT COUNT(*), SUM(amount), MIN(amount), AVG(amount) FROM ledger \
         WHERE id >= ? AND id <= ?",
    )?;
    let SqlResult::Rows { rows, .. } = database
        .execute_prepared_latest(&prepared, &[SqlValue::Signed(100), SqlValue::Signed(200)])?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![
            SqlValue::Unsigned(0),
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
        ]]
    );
    Ok(())
}

#[test]
fn unsupported_aggregate_shapes_fail_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // SUM over TEXT.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT SUM(tenant) FROM ledger WHERE id = ?",
            &[SqlValue::Signed(1)]
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Aggregate with ORDER BY.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger WHERE id >= ? ORDER BY id",
            &[SqlValue::Signed(1)],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Aggregate with LIMIT.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger WHERE id >= ? LIMIT 1",
            &[SqlValue::Signed(1)],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // COUNT of unknown column.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(missing) FROM ledger WHERE id = ?",
            &[SqlValue::Signed(1)]
        ),
        Err(SqlError::UnknownColumn)
    ));
    // AVG(*) is not a shape.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT AVG(*) FROM ledger WHERE id = ?",
            &[SqlValue::Signed(1)]
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Mixed aggregate and plain column projection is not in the slice.
    assert!(
        transaction
            .execute_sql(
                "SELECT tenant, COUNT(*) FROM ledger WHERE id = ?",
                &[SqlValue::Signed(1)],
            )
            .is_err()
    );
    transaction.rollback();
    Ok(())
}

#[test]
fn aggregate_plans_are_reusable_and_catalog_bound() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let prepared = database.prepare_sql_latest(
        "SELECT COUNT(*), MAX(amount) FROM ledger WHERE tenant = ? AND id >= ? AND id <= ?",
    )?;
    // Residual filter (tenant equality is not indexed) over the id range.
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(
        &prepared,
        &[
            SqlValue::Text("alpha".to_owned()),
            SqlValue::Signed(1),
            SqlValue::Signed(5),
        ],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![SqlValue::Unsigned(3), SqlValue::Signed(30)]]
    );

    // Reuse with different parameters.
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(
        &prepared,
        &[
            SqlValue::Text("beta".to_owned()),
            SqlValue::Signed(1),
            SqlValue::Signed(5),
        ],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![SqlValue::Unsigned(2), SqlValue::Signed(-5)]]
    );

    // New rows are visible to later executions of the same plan.
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    batch.execute_sql_dml(
        "INSERT INTO ledger (id, tenant, amount) VALUES (6, 'beta', 100)",
        &[],
    )?;
    database.commit_optimistic(batch)?;
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(
        &prepared,
        &[
            SqlValue::Text("beta".to_owned()),
            SqlValue::Signed(1),
            SqlValue::Signed(10),
        ],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![vec![SqlValue::Unsigned(3), SqlValue::Signed(100)]]
    );
    Ok(())
}
