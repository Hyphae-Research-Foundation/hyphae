// SPDX-License-Identifier: Apache-2.0

//! Hyphae-only ablations isolating where commit time goes.
//!
//! Three controlled experiments, one variable each:
//! - `durability`: identical single-SET commits under Strict vs Group
//!   (scheduler cohorts) vs Memory — isolates physical fsync policy;
//! - `transaction_shape`: identical all-engine transactions (1 SQL INSERT +
//!   1 SET + 1 indexed document) prepared through the materialized optimistic
//!   batch vs the point-resolved delta batch — isolates full-state
//!   materialization cost;
//! - `engine_composition`: Memory-durability commits staging SQL only, then
//!   SQL+structure, then SQL+structure+search — isolates per-engine
//!   copy-on-write root construction cost from fsync entirely.
//!
//! Every phase reports the receipt-attributed clocks (`execution`,
//! `wal_append`, `page_sync`, `wal_sync`) in addition to end-to-end latency,
//! matching the microsecond-first measurement contract.

use anyhow::Context;
use hyphae_native_runtime::{
    CommitReceipt, GroupCommitConfig, NativeCommitScheduler, NativeDatabase, SqlValue,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

use crate::util::{fresh_dir, Recorder, Xorshift};

pub struct AblationConfig {
    pub commits_per_phase: usize,
    pub scratch_root: String,
    pub seed: u64,
}

struct ReceiptClocks {
    execution: Vec<u64>,
    wal_append: Vec<u64>,
    page_sync: Vec<u64>,
    wal_sync: Vec<u64>,
}

impl ReceiptClocks {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            execution: Vec::with_capacity(capacity),
            wal_append: Vec::with_capacity(capacity),
            page_sync: Vec::with_capacity(capacity),
            wal_sync: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, receipt: &CommitReceipt) {
        let nanos =
            |duration: std::time::Duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.execution.push(nanos(receipt.execution_time));
        self.wal_append.push(nanos(receipt.wal_append_time));
        self.page_sync
            .push(nanos(receipt.page_synchronization_time));
        self.wal_sync.push(nanos(receipt.wal_synchronization_time));
    }

    fn summarize(self) -> serde_json::Value {
        fn stats(mut samples: Vec<u64>) -> serde_json::Value {
            if samples.is_empty() {
                return serde_json::json!(null);
            }
            samples.sort_unstable();
            let index = |numerator: usize, denominator: usize| {
                samples[samples
                    .len()
                    .saturating_mul(numerator)
                    .div_ceil(denominator)
                    .saturating_sub(1)
                    .min(samples.len() - 1)]
            };
            serde_json::json!({
                "p50": index(50, 100),
                "p99": index(99, 100),
            })
        }
        serde_json::json!({
            "execution_nanos": stats(self.execution),
            "wal_append_nanos": stats(self.wal_append),
            "page_sync_nanos": stats(self.page_sync),
            "wal_sync_nanos": stats(self.wal_sync),
        })
    }
}

fn key(rng: &mut Xorshift) -> Vec<u8> {
    format!("ablation-key-{:010}", rng.below(100_000)).into_bytes()
}

pub fn run(config: &AblationConfig) -> anyhow::Result<serde_json::Value> {
    let durability = durability_ablation(config).context("durability ablation")?;
    let shape = shape_ablation(config).context("transaction shape ablation")?;
    let composition = composition_ablation(config).context("engine composition ablation")?;
    Ok(serde_json::json!({
        "commits_per_phase": config.commits_per_phase,
        "durability": durability,
        "transaction_shape": shape,
        "engine_composition": composition,
    }))
}

fn durability_ablation(config: &AblationConfig) -> anyhow::Result<serde_json::Value> {
    let mut phases = serde_json::Map::new();

    for (label, durability) in [
        ("strict", DurabilityClass::Strict),
        ("memory", DurabilityClass::Memory),
    ] {
        let path = fresh_dir(&config.scratch_root, &format!("ablation-dur-{label}"));
        let mut database = NativeDatabase::create(&path)?;
        let mut rng = Xorshift::new(config.seed);
        let mut recorder = Recorder::with_capacity(config.commits_per_phase);
        let mut clocks = ReceiptClocks::with_capacity(config.commits_per_phase);
        for sequence in 0..config.commits_per_phase {
            let write_key = key(&mut rng);
            let receipt = recorder.record(|| -> anyhow::Result<CommitReceipt> {
                let mut batch = database.begin_optimistic(0, durability)?;
                batch.set(write_key.clone(), sequence.to_string().into_bytes(), None)?;
                Ok(database.commit_optimistic(batch)?)
            })?;
            clocks.push(&receipt);
        }
        drop(database);
        std::fs::remove_dir_all(&path).ok();
        phases.insert(
            label.to_owned(),
            serde_json::json!({
                "end_to_end": recorder.summary(label),
                "receipt_clocks": clocks.summarize(),
            }),
        );
    }

    // Group durability through the scheduler with 8 concurrent producers.
    {
        let path = fresh_dir(&config.scratch_root, "ablation-dur-group");
        let database = NativeDatabase::create(&path)?;
        let scheduler = NativeCommitScheduler::start(
            database,
            GroupCommitConfig::new(32, std::time::Duration::from_micros(200), 1_024)?,
        )
        .map_err(|error| anyhow::anyhow!("scheduler start: {error}"))?;
        const PRODUCERS: usize = 8;
        let rounds = config.commits_per_phase / PRODUCERS;
        let clients: Vec<_> = (0..PRODUCERS).map(|_| scheduler.client()).collect();
        let started = std::time::Instant::now();
        let all_nanos = std::thread::scope(|scope| -> anyhow::Result<Vec<u64>> {
            let handles: Vec<_> = clients
                .into_iter()
                .enumerate()
                .map(|(producer, client)| {
                    scope.spawn(move || -> anyhow::Result<Vec<u64>> {
                        // Disjoint per-producer key namespace: this phase
                        // isolates fsync amortization, not conflict
                        // behavior. Overlapping keys would trip
                        // first-committer-wins by design.
                        let mut nanos = Vec::with_capacity(rounds);
                        for sequence in 0..rounds {
                            let mut batch = client
                                .begin_optimistic(0, DurabilityClass::Group)
                                .map_err(|error| anyhow::anyhow!("group begin: {error}"))?;
                            batch.set(
                                format!("ablation-group-p{producer:02}-{sequence:08}").into_bytes(),
                                sequence.to_string().into_bytes(),
                                None,
                            )?;
                            let commit_started = std::time::Instant::now();
                            client
                                .submit(batch)
                                .map_err(|error| anyhow::anyhow!("group submit: {error}"))?;
                            nanos.push(
                                u64::try_from(commit_started.elapsed().as_nanos())
                                    .unwrap_or(u64::MAX),
                            );
                        }
                        Ok(nanos)
                    })
                })
                .collect();
            let mut merged = Vec::with_capacity(rounds * PRODUCERS);
            for handle in handles {
                merged.extend(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("group producer panicked"))??,
                );
            }
            Ok(merged)
        })?;
        let wall_nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        scheduler
            .shutdown()
            .map_err(|error| anyhow::anyhow!("scheduler shutdown: {error}"))?;
        std::fs::remove_dir_all(&path).ok();
        let mut sorted = all_nanos;
        sorted.sort_unstable();
        let pick = |numerator: usize, denominator: usize| {
            if sorted.is_empty() {
                0
            } else {
                sorted[sorted
                    .len()
                    .saturating_mul(numerator)
                    .div_ceil(denominator)
                    .saturating_sub(1)
                    .min(sorted.len() - 1)]
            }
        };
        phases.insert(
            "group_8_producers".to_owned(),
            serde_json::json!({
                "end_to_end": {
                    "label": "group",
                    "operations": sorted.len(),
                    "wall_nanos": wall_nanos,
                    "ops_per_second": if wall_nanos == 0 { 0.0 } else {
                        sorted.len() as f64 / (wall_nanos as f64 / 1e9)
                    },
                    "latency_nanos": {
                        "p50": pick(50, 100),
                        "p95": pick(95, 100),
                        "p99": pick(99, 100),
                    },
                },
            }),
        );
    }

    Ok(serde_json::Value::Object(phases))
}

fn shape_ablation(config: &AblationConfig) -> anyhow::Result<serde_json::Value> {
    // The materialized arm re-materializes the complete database state at
    // every begin, so its cost grows with accumulated commits; 2,000 samples
    // are enough to expose the asymmetry without an O(n^2) wall.
    let commits = config.commits_per_phase.min(2_000);
    let path = fresh_dir(&config.scratch_root, "ablation-shape");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(7_101)?;
    {
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
            &[],
        )?;
        transaction.create_search_index(lexical, "ablation_lexical")?;
        transaction.commit()?;
    }

    let mut materialized = Recorder::with_capacity(commits);
    for sequence in 0..commits {
        let id = sequence as i64;
        materialized.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic(0, DurabilityClass::Memory)?;
            batch.execute_sql_dml(
                "INSERT INTO events (id, body) VALUES (?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(format!("materialized event {id}")),
                ],
            )?;
            batch.set(
                format!("event-key-{id}").into_bytes(),
                id.to_string().into_bytes(),
                None,
            )?;
            batch.index_document(
                lexical,
                id.to_be_bytes().to_vec(),
                format!("materialized indexed body {id}"),
            )?;
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
    }
    let materialized_summary = materialized.summary("all_engine_materialized_batch");

    let offset = commits as i64;
    let mut delta = Recorder::with_capacity(commits);
    for sequence in 0..commits {
        let id = offset + sequence as i64;
        delta.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Memory)?;
            database.stage_delta_sql_dml(
                &mut batch,
                "INSERT INTO events (id, body) VALUES (?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(format!("delta event {id}")),
                ],
            )?;
            database.stage_delta_set(
                &mut batch,
                format!("event-key-{id}").into_bytes(),
                id.to_string().into_bytes(),
                None,
            )?;
            database.stage_delta_index_document(
                &mut batch,
                lexical,
                id.to_be_bytes().to_vec(),
                format!("delta indexed body {id}"),
            )?;
            database.commit_optimistic(batch)?;
            Ok(())
        })?;
    }
    let delta_summary = delta.summary("all_engine_delta_batch");

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "durability": "memory (isolates preparation cost from fsync)",
        "operations_per_transaction": "1 SQL INSERT + 1 SET + 1 index_document",
        "materialized": materialized_summary,
        "delta": delta_summary,
    }))
}

fn composition_ablation(config: &AblationConfig) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, "ablation-composition");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(7_201)?;
    {
        let mut transaction = database.begin(0, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE compose (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
            &[],
        )?;
        transaction.create_search_index(lexical, "ablation_compose")?;
        transaction.commit()?;
    }

    // Delta staging (the hot write path): materialized begin cost grows with
    // state and is measured separately by the transaction-shape ablation.
    let mut base = 0_i64;
    let mut phase = |engines: u8, base: &mut i64| -> anyhow::Result<serde_json::Value> {
        let mut recorder = Recorder::with_capacity(config.commits_per_phase);
        for sequence in 0..config.commits_per_phase {
            let id = *base + sequence as i64;
            recorder.record(|| -> anyhow::Result<()> {
                let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Memory)?;
                database.stage_delta_sql_dml(
                    &mut batch,
                    "INSERT INTO compose (id, body) VALUES (?, ?)",
                    &[
                        SqlValue::Signed(id),
                        SqlValue::Text(format!("compose body {id}")),
                    ],
                )?;
                if engines >= 2 {
                    database.stage_delta_set(
                        &mut batch,
                        format!("compose-key-{id}").into_bytes(),
                        id.to_string().into_bytes(),
                        None,
                    )?;
                }
                if engines >= 3 {
                    database.stage_delta_index_document(
                        &mut batch,
                        lexical,
                        id.to_be_bytes().to_vec(),
                        format!("compose indexed body {id}"),
                    )?;
                }
                database.commit_optimistic(batch)?;
                Ok(())
            })?;
        }
        *base += config.commits_per_phase as i64;
        Ok(recorder.summary(match engines {
            1 => "sql_only",
            2 => "sql_plus_structure",
            _ => "sql_plus_structure_plus_search",
        }))
    };

    let sql_only = phase(1, &mut base)?;
    let sql_structure = phase(2, &mut base)?;
    let all_three = phase(3, &mut base)?;

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "durability": "memory (isolates root construction from fsync)",
        "path": "delta staging (point-resolved hot write path)",
        "sql_only": sql_only,
        "sql_plus_structure": sql_structure,
        "sql_plus_structure_plus_search": all_three,
    }))
}
