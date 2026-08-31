// SPDX-License-Identifier: Apache-2.0

//! Dedicated-hardware baseline benchmark harness.
//!
//! Usage:
//!   hyphae-baseline-harness <suite> <scratch_root> <output.json> [options]
//!
//! Suites: `sql`, `keyspace`, `lexical`, `ablation`, `all`.
//! Keyspace options: `--redis-strict <socket>` `--redis-everysec <socket>`.
//! Scale option: `--scale small|full` (small is a smoke profile).

mod ablation_suite;
mod keyspace_suite;
mod lexical_suite;
mod sql_suite;
mod util;

use anyhow::{bail, Context};

struct Arguments {
    suite: String,
    scratch_root: String,
    output: String,
    redis_strict: String,
    redis_everysec: String,
    full_scale: bool,
}

fn parse_arguments() -> anyhow::Result<Arguments> {
    let mut positional = Vec::new();
    let mut redis_strict = String::new();
    let mut redis_everysec = String::new();
    let mut full_scale = true;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--redis-strict" => {
                redis_strict = arguments
                    .next()
                    .context("--redis-strict requires a socket path")?;
            }
            "--redis-everysec" => {
                redis_everysec = arguments
                    .next()
                    .context("--redis-everysec requires a socket path")?;
            }
            "--scale" => {
                let scale = arguments.next().context("--scale requires a value")?;
                full_scale = match scale.as_str() {
                    "full" => true,
                    "small" => false,
                    other => bail!("unknown scale {other}"),
                };
            }
            other => positional.push(other.to_owned()),
        }
    }
    if positional.len() != 3 {
        bail!(
            "usage: hyphae-baseline-harness <suite> <scratch_root> <output.json> \
             [--redis-strict <socket>] [--redis-everysec <socket>] [--scale small|full]"
        );
    }
    Ok(Arguments {
        suite: positional[0].clone(),
        scratch_root: positional[1].clone(),
        output: positional[2].clone(),
        redis_strict,
        redis_everysec,
        full_scale,
    })
}

fn main() -> anyhow::Result<()> {
    let arguments = parse_arguments()?;
    std::fs::create_dir_all(&arguments.scratch_root)?;
    let seed = 0x5eed_2026_0829_0001;

    let scale = |full: u64, small: u64| if arguments.full_scale { full } else { small };
    let scale_usize = |full: usize, small: usize| if arguments.full_scale { full } else { small };

    let mut results = serde_json::Map::new();

    if matches!(arguments.suite.as_str(), "sql" | "all") {
        let config = sql_suite::SqlSuiteConfig {
            rows: scale(1_000_000, 20_000),
            point_reads: scale_usize(200_000, 5_000),
            strict_updates: scale_usize(10_000, 500),
            batched_updates: scale_usize(100_000, 5_000),
            scratch_root: arguments.scratch_root.clone(),
            seed,
        };
        eprintln!("running sql suite ({} rows)...", config.rows);
        results.insert("sql".to_owned(), sql_suite::run(&config)?);
    }

    if matches!(arguments.suite.as_str(), "keyspace" | "all") {
        let config = keyspace_suite::KeyspaceSuiteConfig {
            keys: scale(1_000_000, 20_000),
            gets: scale_usize(500_000, 10_000),
            strict_sets: scale_usize(10_000, 500),
            relaxed_sets: scale_usize(200_000, 5_000),
            scratch_root: arguments.scratch_root.clone(),
            seed,
            redis_strict_socket: arguments.redis_strict.clone(),
            redis_everysec_socket: arguments.redis_everysec.clone(),
        };
        eprintln!("running keyspace suite ({} keys)...", config.keys);
        results.insert("keyspace".to_owned(), keyspace_suite::run(&config)?);
    }

    if matches!(arguments.suite.as_str(), "lexical" | "all") {
        // Full scale is 100k documents, not 1M: copy-on-write posting pages
        // amplify batched ingest so a 1M-document strict-per-batch load wrote
        // 2.87 TB before completing on i7i.metal-24xl. That amplification is
        // itself reported as evidence; the query comparison runs at a scale
        // both engines complete identically.
        let config = lexical_suite::LexicalSuiteConfig {
            documents: scale(100_000, 20_000),
            vocabulary: 50_000,
            queries: scale_usize(10_000, 1_000),
            scratch_root: arguments.scratch_root.clone(),
            seed,
        };
        eprintln!("running lexical suite ({} documents)...", config.documents);
        results.insert("lexical".to_owned(), lexical_suite::run(&config)?);
    }

    if matches!(arguments.suite.as_str(), "ablation" | "all") {
        let config = ablation_suite::AblationConfig {
            commits_per_phase: scale_usize(10_000, 500),
            scratch_root: arguments.scratch_root.clone(),
            seed,
        };
        eprintln!(
            "running ablation suite ({} commits per phase)...",
            config.commits_per_phase
        );
        results.insert("ablation".to_owned(), ablation_suite::run(&config)?);
    }

    if results.is_empty() {
        bail!("unknown suite {}", arguments.suite);
    }

    util::write_receipt(
        &arguments.output,
        "hyphae-baseline-harness-v1",
        serde_json::Value::Object(results),
    )?;
    eprintln!("receipt written to {}", arguments.output);
    Ok(())
}
