// SPDX-License-Identifier: Apache-2.0

//! SQL point-read / point-write suite: Hyphae native vs SQLite vs DuckDB.
//!
//! Workload: one table `accounts (id BIGINT PRIMARY KEY, balance BIGINT,
//! payload TEXT)`, `rows` rows. Measured phases per engine:
//! - `load`: bulk insert (one transaction per batch of 1,000);
//! - `point_select`: prepared `SELECT ... WHERE id = ?`, skewed keys;
//! - `point_update_strict`: prepared single-row UPDATE, one durable
//!   transaction per operation (fsync-equivalent settings);
//! - `point_update_batched`: single-row UPDATEs grouped 100 per durable
//!   transaction.
//!
//! Fairness notes: identical row content, identical key skew, identical
//! operation counts. SQLite runs WAL + `synchronous=FULL` for the strict
//! phase, matching Hyphae `DurabilityClass::Strict` fsync-per-commit.
//! DuckDB runs its default durable WAL. Reads use prepared statements
//! everywhere. DuckDB is an OLAP columnar engine measured here only as a
//! widely known reference point, not as an OLTP claim.

use anyhow::{anyhow, Context};
use hyphae_native_runtime::{NativeDatabase, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

use crate::util::{fresh_dir, Recorder, Xorshift};

const LOAD_BATCH: usize = 1_000;
const UPDATE_GROUP: usize = 100;

pub struct SqlSuiteConfig {
    pub rows: u64,
    pub point_reads: usize,
    pub strict_updates: usize,
    pub batched_updates: usize,
    pub scratch_root: String,
    pub seed: u64,
}

fn payload(id: u64) -> String {
    format!("account-payload-{id:012}-{:032x}", (id as u128) * 0x9e37)
}

pub fn run(config: &SqlSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let hyphae = hyphae_run(config).context("hyphae sql suite")?;
    let sqlite = sqlite_run(config).context("sqlite sql suite")?;
    let duckdb = duckdb_run(config).context("duckdb sql suite")?;
    Ok(serde_json::json!({
        "workload": {
            "rows": config.rows,
            "point_reads": config.point_reads,
            "strict_updates": config.strict_updates,
            "batched_updates": config.batched_updates,
            "seed": config.seed,
        },
        "hyphae": hyphae,
        "sqlite": sqlite,
        "duckdb": duckdb,
    }))
}

fn hyphae_run(config: &SqlSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "sql-hyphae");
    let mut database = NativeDatabase::create(&path)?;
    {
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL, \
             payload TEXT NOT NULL)",
            &[],
        )?;
        transaction.commit()?;
    }

    // Load phase: batches of LOAD_BATCH inserts, strict durability per batch,
    // through the point-resolved delta path (no full-state materialization).
    let mut load = Recorder::with_capacity((config.rows as usize).div_ceil(LOAD_BATCH));
    let mut inserted = 0_u64;
    while inserted < config.rows {
        let upper = (inserted + LOAD_BATCH as u64).min(config.rows);
        let range = inserted..upper;
        load.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
            for id in range.clone() {
                database.stage_delta_sql_dml(
                    &mut batch,
                    "INSERT INTO accounts (id, balance, payload) VALUES (?, ?, ?)",
                    &[
                        SqlValue::Signed(id as i64),
                        SqlValue::Signed(1_000),
                        SqlValue::Text(payload(id)),
                    ],
                )?;
            }
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
        inserted = upper;
    }
    let load_summary = load.summary("load_batch_1000");

    // Point reads through the latest-root prepared plan cache.
    let prepared =
        database.prepare_sql_latest("SELECT id, balance, payload FROM accounts WHERE id = ?")?;
    let mut rng = Xorshift::new(config.seed);
    let mut reads = Recorder::with_capacity(config.point_reads);
    let mut checksum = 0_u64;
    for _ in 0..config.point_reads {
        let key = rng.skewed(config.rows) as i64;
        let result = reads
            .record(|| database.execute_prepared_latest(&prepared, &[SqlValue::Signed(key)]))?;
        if let SqlResult::Rows { rows, .. } = result {
            checksum = checksum.wrapping_add(rows.len() as u64);
        }
    }
    let read_summary = reads.summary("point_select_prepared");

    // Strict updates: one durable commit per statement (delta path).
    let mut strict = Recorder::with_capacity(config.strict_updates);
    for sequence in 0..config.strict_updates {
        let key = rng.skewed(config.rows) as i64;
        strict.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
            database.stage_delta_sql_dml(
                &mut batch,
                "UPDATE accounts SET balance = ? WHERE id = ?",
                &[SqlValue::Signed(sequence as i64), SqlValue::Signed(key)],
            )?;
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
    }
    let strict_summary = strict.summary("point_update_strict");

    // Batched updates: UPDATE_GROUP statements per durable commit.
    let mut batched = Recorder::with_capacity(config.batched_updates / UPDATE_GROUP);
    let mut done = 0_usize;
    while done < config.batched_updates {
        let group = UPDATE_GROUP.min(config.batched_updates - done);
        let keys: Vec<i64> = (0..group).map(|_| rng.skewed(config.rows) as i64).collect();
        batched.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
            for (offset, key) in keys.iter().enumerate() {
                database.stage_delta_sql_dml(
                    &mut batch,
                    "UPDATE accounts SET balance = ? WHERE id = ?",
                    &[
                        SqlValue::Signed((done + offset) as i64),
                        SqlValue::Signed(*key),
                    ],
                )?;
            }
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
        done += group;
    }
    let batched_summary = batched.summary("point_update_batched_100");

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "hyphae-native",
        "durability": "strict per commit (WAL fsync)",
        "read_checksum": checksum,
        "load": load_summary,
        "point_select": read_summary,
        "point_update_strict": strict_summary,
        "point_update_batched": batched_summary,
    }))
}

fn sqlite_run(config: &SqlSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "sql-sqlite");
    std::fs::create_dir_all(&path)?;
    let file = path.join("accounts.sqlite3");
    let connection = rusqlite::Connection::open(&file)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL, \
         payload TEXT NOT NULL)",
        [],
    )?;

    let mut load = Recorder::with_capacity((config.rows as usize).div_ceil(LOAD_BATCH));
    let mut inserted = 0_u64;
    while inserted < config.rows {
        let upper = (inserted + LOAD_BATCH as u64).min(config.rows);
        let range = inserted..upper;
        load.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            {
                let mut statement = connection.prepare_cached(
                    "INSERT INTO accounts (id, balance, payload) VALUES (?, ?, ?)",
                )?;
                for id in range.clone() {
                    statement.execute(rusqlite::params![id as i64, 1_000_i64, payload(id)])?;
                }
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
        inserted = upper;
    }
    let load_summary = load.summary("load_batch_1000");

    let mut rng = Xorshift::new(config.seed);
    let mut reads = Recorder::with_capacity(config.point_reads);
    let mut checksum = 0_u64;
    for _ in 0..config.point_reads {
        let key = rng.skewed(config.rows) as i64;
        let found = reads.record(|| -> anyhow::Result<u64> {
            let mut statement = connection
                .prepare_cached("SELECT id, balance, payload FROM accounts WHERE id = ?")?;
            let mut query_rows = statement.query([key])?;
            let mut seen = 0_u64;
            while query_rows.next()?.is_some() {
                seen += 1;
            }
            Ok(seen)
        })?;
        checksum = checksum.wrapping_add(found);
    }
    let read_summary = reads.summary("point_select_prepared");

    let mut strict = Recorder::with_capacity(config.strict_updates);
    for sequence in 0..config.strict_updates {
        let key = rng.skewed(config.rows) as i64;
        strict.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            connection
                .prepare_cached("UPDATE accounts SET balance = ? WHERE id = ?")?
                .execute(rusqlite::params![sequence as i64, key])?;
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
    }
    let strict_summary = strict.summary("point_update_strict");

    let mut batched = Recorder::with_capacity(config.batched_updates / UPDATE_GROUP);
    let mut done = 0_usize;
    while done < config.batched_updates {
        let group = UPDATE_GROUP.min(config.batched_updates - done);
        let keys: Vec<i64> = (0..group).map(|_| rng.skewed(config.rows) as i64).collect();
        batched.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            for (offset, key) in keys.iter().enumerate() {
                connection
                    .prepare_cached("UPDATE accounts SET balance = ? WHERE id = ?")?
                    .execute(rusqlite::params![(done + offset) as i64, *key])?;
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
        done += group;
    }
    let batched_summary = batched.summary("point_update_batched_100");

    let version: String = connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    drop(connection);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "sqlite",
        "version": version,
        "durability": "WAL + synchronous=FULL",
        "read_checksum": checksum,
        "load": load_summary,
        "point_select": read_summary,
        "point_update_strict": strict_summary,
        "point_update_batched": batched_summary,
    }))
}

fn duckdb_run(config: &SqlSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "sql-duckdb");
    std::fs::create_dir_all(&path)?;
    let file = path.join("accounts.duckdb");
    let connection = duckdb::Connection::open(&file)?;
    connection.execute_batch(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL, \
         payload TEXT NOT NULL)",
    )?;

    let mut load = Recorder::with_capacity((config.rows as usize).div_ceil(LOAD_BATCH));
    let mut inserted = 0_u64;
    while inserted < config.rows {
        let upper = (inserted + LOAD_BATCH as u64).min(config.rows);
        let range = inserted..upper;
        load.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN TRANSACTION")?;
            {
                let mut statement = connection.prepare_cached(
                    "INSERT INTO accounts (id, balance, payload) VALUES (?, ?, ?)",
                )?;
                for id in range.clone() {
                    statement.execute(duckdb::params![id as i64, 1_000_i64, payload(id)])?;
                }
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
        inserted = upper;
    }
    let load_summary = load.summary("load_batch_1000");

    let mut rng = Xorshift::new(config.seed);
    let mut reads = Recorder::with_capacity(config.point_reads);
    let mut checksum = 0_u64;
    for _ in 0..config.point_reads {
        let key = rng.skewed(config.rows) as i64;
        let found = reads.record(|| -> anyhow::Result<u64> {
            let mut statement = connection
                .prepare_cached("SELECT id, balance, payload FROM accounts WHERE id = ?")?;
            let mut query_rows = statement.query([key])?;
            let mut seen = 0_u64;
            while query_rows.next()?.is_some() {
                seen += 1;
            }
            Ok(seen)
        })?;
        checksum = checksum.wrapping_add(found);
    }
    let read_summary = reads.summary("point_select_prepared");

    let mut strict = Recorder::with_capacity(config.strict_updates);
    for sequence in 0..config.strict_updates {
        let key = rng.skewed(config.rows) as i64;
        strict.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN TRANSACTION")?;
            connection
                .prepare_cached("UPDATE accounts SET balance = ? WHERE id = ?")?
                .execute(duckdb::params![sequence as i64, key])?;
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
    }
    let strict_summary = strict.summary("point_update_strict");

    let mut batched = Recorder::with_capacity(config.batched_updates / UPDATE_GROUP);
    let mut done = 0_usize;
    while done < config.batched_updates {
        let group = UPDATE_GROUP.min(config.batched_updates - done);
        let keys: Vec<i64> = (0..group).map(|_| rng.skewed(config.rows) as i64).collect();
        batched.record(|| -> anyhow::Result<()> {
            connection.execute_batch("BEGIN TRANSACTION")?;
            for (offset, key) in keys.iter().enumerate() {
                connection
                    .prepare_cached("UPDATE accounts SET balance = ? WHERE id = ?")?
                    .execute(duckdb::params![(done + offset) as i64, *key])?;
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })?;
        done += group;
    }
    let batched_summary = batched.summary("point_update_batched_100");

    let version: String = connection
        .query_row("SELECT version()", [], |row| row.get(0))
        .map_err(|error| anyhow!("duckdb version: {error}"))?;
    drop(connection);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "duckdb",
        "version": version,
        "durability": "default durable WAL (OLAP reference point)",
        "read_checksum": checksum,
        "load": load_summary,
        "point_select": read_summary,
        "point_update_strict": strict_summary,
        "point_update_batched": batched_summary,
    }))
}
