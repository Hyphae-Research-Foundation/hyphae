// SPDX-License-Identifier: Apache-2.0

//! Keyspace point suite: Hyphae native structures vs Redis.
//!
//! Workload: `keys` string keys (`key-%010d` -> 64-byte values), then skewed
//! GET and SET phases. Measured surfaces:
//! - `hyphae embedded`: direct library calls (`get_latest_structure`,
//!   `commit_optimistic` with one `set` per commit);
//! - `redis uds`: the `redis` crate over a Unix domain socket against a local
//!   `redis-server` with `appendonly yes, appendfsync always` for the strict
//!   phase (fsync-per-write comparable to `DurabilityClass::Strict`) — the
//!   caller restarts the server between phases when changing fsync policy;
//! - `redis uds everysec`: same socket, `appendfsync everysec` (group-like).
//!
//! Fairness notes: Redis is measured over UDS (its fastest local transport)
//! while embedded Hyphae has no transport at all; the receipt therefore also
//! reports Hyphae `Memory` durability (no fsync ack, like everysec) so both
//! deltas (transport, fsync policy) stay visible instead of being blended.

use anyhow::Context;
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;
use redis::ConnectionLike;

use crate::util::{fresh_dir, Recorder, Xorshift};

pub struct KeyspaceSuiteConfig {
    pub keys: u64,
    pub gets: usize,
    pub strict_sets: usize,
    pub relaxed_sets: usize,
    pub scratch_root: String,
    pub seed: u64,
    /// Unix socket of a strict redis (appendfsync always); empty disables.
    pub redis_strict_socket: String,
    /// Unix socket of an everysec redis; empty disables.
    pub redis_everysec_socket: String,
}

fn key(index: u64) -> Vec<u8> {
    format!("key-{index:010}").into_bytes()
}

fn value(index: u64) -> Vec<u8> {
    format!("value-{index:010}-{:040x}", (index as u128) * 0x5851).into_bytes()
}

pub fn run(config: &KeyspaceSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let hyphae = hyphae_run(config).context("hyphae keyspace suite")?;
    let redis_strict = if config.redis_strict_socket.is_empty() {
        serde_json::Value::Null
    } else {
        redis_run(
            &config.redis_strict_socket,
            "redis-uds-appendfsync-always",
            config,
        )
        .context("redis strict keyspace suite")?
    };
    let redis_everysec = if config.redis_everysec_socket.is_empty() {
        serde_json::Value::Null
    } else {
        redis_run(
            &config.redis_everysec_socket,
            "redis-uds-appendfsync-everysec",
            config,
        )
        .context("redis everysec keyspace suite")?
    };
    Ok(serde_json::json!({
        "workload": {
            "keys": config.keys,
            "gets": config.gets,
            "strict_sets": config.strict_sets,
            "relaxed_sets": config.relaxed_sets,
            "seed": config.seed,
        },
        "hyphae": hyphae,
        "redis_strict": redis_strict,
        "redis_everysec": redis_everysec,
    }))
}

fn hyphae_run(config: &KeyspaceSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "keyspace-hyphae");
    let mut database = NativeDatabase::create(&path)?;

    // Load all keys in batches of 1,000 under strict durability through the
    // point-resolved delta path (no full-state materialization per begin).
    let mut loaded = 0_u64;
    while loaded < config.keys {
        let upper = (loaded + 1_000).min(config.keys);
        let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
        for index in loaded..upper {
            database.stage_delta_set(&mut batch, key(index), value(index), None)?;
        }
        database.commit_optimistic(batch)?;
        loaded = upper;
    }

    let mut rng = Xorshift::new(config.seed);
    let mut gets = Recorder::with_capacity(config.gets);
    let mut hit = 0_u64;
    for _ in 0..config.gets {
        let index = rng.skewed(config.keys);
        let lookup = key(index);
        let found = gets.record(|| database.get_latest_structure(&lookup, 0))?;
        if found.is_some() {
            hit += 1;
        }
    }
    let get_summary = gets.summary("get_latest");

    let mut strict = Recorder::with_capacity(config.strict_sets);
    for sequence in 0..config.strict_sets {
        let index = rng.skewed(config.keys);
        strict.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
            database.stage_delta_set(&mut batch, key(index), value(sequence as u64), None)?;
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
    }
    let strict_summary = strict.summary("set_strict_fsync_per_commit");

    let mut relaxed = Recorder::with_capacity(config.relaxed_sets);
    for sequence in 0..config.relaxed_sets {
        let index = rng.skewed(config.keys);
        relaxed.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Memory)?;
            database.stage_delta_set(&mut batch, key(index), value(sequence as u64), None)?;
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
    }
    let relaxed_summary = relaxed.summary("set_memory_no_fsync_ack");

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "engine": "hyphae-native-embedded",
        "transport": "none (in-process library call)",
        "get_hits": hit,
        "get": get_summary,
        "set_strict": strict_summary,
        "set_memory": relaxed_summary,
    }))
}

fn redis_run(
    socket: &str,
    label: &str,
    config: &KeyspaceSuiteConfig,
) -> anyhow::Result<serde_json::Value> {
    let client = redis::Client::open(format!("unix://{socket}"))?;
    let mut connection = client.get_connection()?;
    redis::cmd("FLUSHALL").exec(&mut connection)?;

    // Load with pipelines of 1,000.
    let mut loaded = 0_u64;
    while loaded < config.keys {
        let upper = (loaded + 1_000).min(config.keys);
        let mut pipeline = redis::pipe();
        for index in loaded..upper {
            pipeline
                .cmd("SET")
                .arg(key(index))
                .arg(value(index))
                .ignore();
        }
        pipeline.exec(&mut connection)?;
        loaded = upper;
    }

    let mut rng = Xorshift::new(config.seed);
    let mut gets = Recorder::with_capacity(config.gets);
    let mut hit = 0_u64;
    for _ in 0..config.gets {
        let index = rng.skewed(config.keys);
        let lookup = key(index);
        let found: Option<Vec<u8>> =
            gets.record(|| redis::cmd("GET").arg(&lookup).query(&mut connection))?;
        if found.is_some() {
            hit += 1;
        }
    }
    let get_summary = gets.summary("get_uds");

    let mut sets = Recorder::with_capacity(config.strict_sets);
    for sequence in 0..config.strict_sets {
        let index = rng.skewed(config.keys);
        let write_key = key(index);
        let write_value = value(sequence as u64);
        sets.record(|| -> anyhow::Result<()> {
            redis::cmd("SET")
                .arg(&write_key)
                .arg(&write_value)
                .exec(&mut connection)?;
            Ok(())
        })?;
    }
    let set_summary = sets.summary("set_uds");

    let info: String = redis::cmd("INFO").arg("server").query(&mut connection)?;
    let version = info
        .lines()
        .find_map(|line| line.strip_prefix("redis_version:"))
        .unwrap_or("unknown")
        .trim()
        .to_owned();
    let connected = connection.check_connection();
    Ok(serde_json::json!({
        "engine": label,
        "version": version,
        "transport": "unix domain socket",
        "connection_alive": connected,
        "get_hits": hit,
        "get": get_summary,
        "set": set_summary,
    }))
}
