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
    // GROUP BY without LIMIT (streaming and ordered-grouped paths).
    assert!(matches!(
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY tenant", &[]),
        Err(SqlError::InvalidAggregate)
    ));
    assert!(matches!(
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY amount", &[]),
        Err(SqlError::InvalidAggregate)
    ));
    // Named plain columns that are not exactly the GROUP BY list.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT amount, COUNT(*) FROM ledger GROUP BY tenant LIMIT 5",
            &[],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Plain column after an aggregate stays outside the slice.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*), tenant FROM ledger GROUP BY tenant LIMIT 5",
            &[],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Ordered-grouped LIMIT above the explicit group ceiling.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger GROUP BY amount LIMIT 65537",
            &[],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    transaction.rollback();
    Ok(())
}

#[test]
fn ordered_grouping_over_arbitrary_columns_matches_postgres_shape() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // Postgres-style named key over a non-key column, with aliases.
    let SqlResult::Rows { columns, rows } = transaction.execute_sql(
        "SELECT amount AS bucket, COUNT(*) AS n FROM ledger GROUP BY amount LIMIT 10",
        &[],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["bucket", "n"]);
    // NULL groups order before every non-null amount.
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Null, SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(5), SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(7), SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(10), SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(30), SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(100), SqlValue::Unsigned(1)],
        ]
    );

    // LIMIT retains exactly the smallest keys even when later rows revisit
    // an evicted key: NULL and amount=5 are the two smallest buckets.
    let SqlResult::Rows { rows, .. } =
        transaction.execute_sql("SELECT COUNT(*) FROM ledger GROUP BY amount LIMIT 2", &[])?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Null, SqlValue::Unsigned(1)],
            vec![SqlValue::Signed(5), SqlValue::Unsigned(1)],
        ]
    );
    transaction.rollback();
    Ok(())
}

#[test]
fn named_group_keys_and_aliases_bind_across_all_surfaces() -> Result<(), TestError> {
    const NAMED: &str = "SELECT tenant AS who, COUNT(*) AS n, SUM(amount) AS total FROM ledger \
         GROUP BY tenant LIMIT 10";
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let expected_rows = vec![
        vec![
            SqlValue::Text("acme".to_owned()),
            SqlValue::Unsigned(3),
            SqlValue::Signed(40),
        ],
        vec![
            SqlValue::Text("globex".to_owned()),
            SqlValue::Unsigned(2),
            SqlValue::Signed(12),
        ],
        vec![
            SqlValue::Text("initech".to_owned()),
            SqlValue::Unsigned(1),
            SqlValue::Signed(100),
        ],
    ];

    // Transactional.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let SqlResult::Rows { columns, rows } = transaction.execute_sql(NAMED, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["who", "n", "total"]);
    assert_eq!(rows, expected_rows);
    transaction.rollback();

    // Prepared snapshot.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(NAMED)?;
    let SqlResult::Rows { columns, rows } = snapshot.execute_prepared(&prepared, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["who", "n", "total"]);
    assert_eq!(rows, expected_rows);

    // Prepared latest.
    let latest = database.prepare_sql_latest(NAMED)?;
    let SqlResult::Rows { columns, rows } = database.execute_prepared_latest(&latest, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["who", "n", "total"]);
    assert_eq!(rows, expected_rows);
    Ok(())
}

#[test]
fn plain_column_aliases_rename_outputs_only() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    let SqlResult::Rows { columns, rows } = transaction.execute_sql(
        "SELECT tenant AS who, sequence AS seq FROM ledger WHERE tenant = 'acme' LIMIT 2",
        &[],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["who", "seq"]);
    assert_eq!(rows.len(), 2);

    // Duplicate aliases fail closed.
    assert!(matches!(
        transaction.execute_sql("SELECT tenant AS a, sequence AS a FROM ledger LIMIT 1", &[],),
        Err(SqlError::DuplicateColumn)
    ));
    // Aliases are output names only: not referenceable from WHERE.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT tenant AS who FROM ledger WHERE who = 'acme' LIMIT 1",
            &[],
        ),
        Err(SqlError::UnknownColumn)
    ));
    transaction.rollback();
    Ok(())
}

#[test]
fn having_and_grouped_order_shape_the_canonical_analytics_query() -> Result<(), TestError> {
    const CANONICAL: &str = "SELECT tenant, COUNT(*) AS n FROM ledger GROUP BY tenant \
         HAVING COUNT(*) > 1 ORDER BY n DESC, tenant ASC LIMIT 10";
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let expected = vec![
        vec![SqlValue::Text("acme".to_owned()), SqlValue::Unsigned(3)],
        vec![SqlValue::Text("globex".to_owned()), SqlValue::Unsigned(2)],
    ];

    // Transactional.
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let SqlResult::Rows { columns, rows } = transaction.execute_sql(CANONICAL, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["tenant", "n"]);
    assert_eq!(rows, expected);
    transaction.rollback();

    // Prepared snapshot.
    let snapshot = database.snapshot(0)?;
    let prepared = snapshot.prepare_sql(CANONICAL)?;
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&prepared, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, expected);

    // Prepared latest.
    let latest = database.prepare_sql_latest(CANONICAL)?;
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(&latest, &[])? else {
        return Err("expected rows".into());
    };
    assert_eq!(rows, expected);
    Ok(())
}

#[test]
fn having_hidden_aggregates_and_alias_operands_fold_without_emitting() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // HAVING references SUM(amount), which is not projected: it folds as a
    // hidden accumulator and never appears in the result columns.
    let SqlResult::Rows { columns, rows } = transaction.execute_sql(
        "SELECT tenant, COUNT(*) AS n FROM ledger GROUP BY tenant \
         HAVING SUM(amount) >= 40 ORDER BY tenant ASC LIMIT 10",
        &[],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(columns, vec!["tenant", "n"]);
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Text("acme".to_owned()), SqlValue::Unsigned(3)],
            vec![SqlValue::Text("initech".to_owned()), SqlValue::Unsigned(1)],
        ]
    );

    // HAVING over a projected alias; NULL comparisons filter out (amount
    // NULL exists in acme but SUM skips nulls — MAX(amount) of the NULL-
    // only group would be NULL and filtered).
    let SqlResult::Rows { rows, .. } = transaction.execute_sql(
        "SELECT tenant, MAX(amount) AS peak FROM ledger GROUP BY tenant \
         HAVING peak > 20 ORDER BY peak DESC LIMIT 10",
        &[],
    )?
    else {
        return Err("expected rows".into());
    };
    assert_eq!(
        rows,
        vec![
            vec![SqlValue::Text("initech".to_owned()), SqlValue::Signed(100)],
            vec![SqlValue::Text("acme".to_owned()), SqlValue::Signed(30)],
        ]
    );
    transaction.rollback();
    Ok(())
}

#[test]
fn having_shapes_outside_the_slice_fail_closed() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = seeded_database(temporary.path())?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;

    // Parameters inside HAVING.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger GROUP BY tenant HAVING COUNT(*) > ? LIMIT 5",
            &[SqlValue::Unsigned(1)],
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // HAVING without GROUP BY.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT tenant FROM ledger HAVING tenant = 'acme' LIMIT 5",
            &[]
        ),
        Err(SqlError::InvalidAggregate)
    ));
    // Unknown operand names.
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger GROUP BY tenant HAVING bogus > 1 LIMIT 5",
            &[],
        ),
        Err(SqlError::UnknownColumn)
    ));
    assert!(matches!(
        transaction.execute_sql(
            "SELECT COUNT(*) FROM ledger GROUP BY tenant ORDER BY amount LIMIT 5",
            &[],
        ),
        Err(SqlError::UnknownColumn)
    ));
    transaction.rollback();
    Ok(())
}
