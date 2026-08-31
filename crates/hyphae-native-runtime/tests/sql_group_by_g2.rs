// SPDX-License-Identifier: Apache-2.0

//! Streaming `GROUP BY <primary-key prefix>` bounded conformance.

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
            "hyphae-sql-group-by-{}-{nanos}",
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

/// Composite primary key (tenant, sequence): tenant is a groupable prefix.
fn seeded_database(path: &std::path::Path) -> Result<NativeDatabase, TestError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE ledger (tenant TEXT, sequence BIGINT, amount BIGINT, \
         PRIMARY KEY (tenant, sequence))",
        &[],
    )?;
    seed.execute_sql_dml(
        "INSERT INTO ledger (tenant, sequence, amount) VALUES \
         ('acme', 1, 10), ('acme', 2, 30), ('acme', 3, NULL), \
         ('globex', 1, 5), ('globex', 2, 7), \
         ('initech', 1, 100)",
        &[],
    )?;
    seed.commit()?;
    Ok(database)
}

const GROUPED: &str = "SELECT COUNT(*), COUNT(amount), SUM(amount), MAX(amount), AVG(amount) \
     FROM ledger GROUP BY tenant LIMIT 10";

fn expected_groups() -> Vec<Vec<SqlValue>> {
    vec![
        vec![
            SqlValue::Text("acme".to_owned()),
            SqlValue::Unsigned(3),
            SqlValue::Unsigned(2),
            SqlValue::Signed(40),
            SqlValue::Signed(30),
            SqlValue::Float64(CanonicalF64::new(20.0)),
        ],
        vec![
            SqlValue::Text("globex".to_owned()),
            SqlValue::Unsigned(2),
            SqlValue::Unsigned(2),
            SqlValue::Signed(12),
            SqlValue::Signed(7),
            SqlValue::Float64(CanonicalF64::new(6.0)),
        ],
        vec![
            SqlValue::Text("initech".to_owned()),
            SqlValue::Unsigned(1),
            SqlValue::Unsigned(1),
            SqlValue::Signed(100),
            SqlValue::Signed(100),
            SqlValue::Float64(CanonicalF64::new(100.0)),
        ],
    ]
}

#[test]
fn grouped_aggregates_stream_identically_across_all_surfaces() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;

    // Transactional.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let SqlResult::Rows { columns, rows } = transaction.execute_sql(GROUPED, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(
        columns,
        vec![
            "tenant",
            "COUNT(*)",
            "COUNT(amount)",
            "SUM(amount)",
            "MAX(amount)",
            "AVG(amount)",
        ]
    );
    assert_eq!(rows, expected_groups());
    transaction.rollback();

    // Prepared snapshot.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(GROUPED)?;
    assert_eq!(prepared.maximum_result_rows(), Some(10));
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&prepared, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, expected_groups());

    // Prepared latest.
    let latest = database.prepare_sql_latest(GROUPED)?;
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(&latest, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, expected_groups());
    Ok(())
}

#[test]
fn grouped_aggregates_respect_the_group_limit_and_range_filters() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let database = seeded_database(temporary.path())?;

    // LIMIT bounds emitted groups in key order.
    let limited =
        database.prepare_sql_latest("SELECT COUNT(*) FROM ledger GROUP BY tenant LIMIT 2")?;
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(&limited, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Text("acme".to_owned()), SqlValue::Unsigned(3)],
            vec![SqlValue::Text("globex".to_owned()), SqlValue::Unsigned(2)],
        ]
    );
    Ok(())
}

#[test]
fn grouped_shapes_outside_the_slice_fail_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // GROUP BY without aggregates.
    assert!(matches!(
        transaction.execute_sql("SELECT tenant FROM ledger GROUP BY tenant LIMIT 5", &[]),
        Err(SqlError::InvalidAggregate)
    ));
    // GROUP BY without LIMIT.
    assert!(matches!(
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY tenant", &[]),
        Err(SqlError::InvalidAggregate)
    ));
    // GROUP BY over a non-prefix column.
    assert!(matches!(
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY sequence LIMIT 5", &[],),
        Err(SqlError::InvalidAggregate)
    ));
    // GROUP BY over a non-key column.
    assert!(matches!(
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY amount LIMIT 5", &[],),
        Err(SqlError::InvalidAggregate)
    ));
    transaction.rollback();
    Ok(())
}
