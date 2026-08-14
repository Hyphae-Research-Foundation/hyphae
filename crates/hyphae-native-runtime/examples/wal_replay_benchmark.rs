// SPDX-License-Identifier: AGPL-3.0-only

//! Reproducible full-history versus anchored-WAL reopen observation.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, WalRetentionReceipt};
use hyphae_native_types::{DurabilityClass, ObjectId};

const HISTORICAL_UPDATES: usize = 400;
const PRE_FLOOR_COMMITS: usize = HISTORICAL_UPDATES + 2;
const SUFFIX_COMMITS: usize = 4;
const OPEN_ROUNDS: usize = 25;
const WAL_FILE: &str = "wal.hywal";

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
}

struct OpenObservation {
    first_external_nanos: u64,
    warm_external: LatencyStats,
    warm_internal: LatencyStats,
    warm_physical: LatencyStats,
    warm_semantic: LatencyStats,
    warm_roots: LatencyStats,
    retained_blocks: usize,
    replayed_transactions: usize,
    committed_transactions: usize,
    verified: bool,
}

struct CorpusObservation {
    wal_bytes: u64,
    retention: Option<WalRetentionReceipt>,
}

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-wal-replay-{label}-{}-{timestamp}",
            std::process::id()
        ));
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn percentile(sorted: &[u64], numerator: usize, denominator: usize) -> u64 {
    let index = sorted
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(sorted.len().saturating_sub(1));
    sorted[index]
}

fn latency_stats(mut observations: Vec<u64>) -> LatencyStats {
    observations.sort_unstable();
    LatencyStats {
        p50: percentile(&observations, 50, 100),
        p95: percentile(&observations, 95, 100),
        p99: percentile(&observations, 99, 100),
    }
}

fn nanos(duration: Duration) -> Result<u64, BenchmarkError> {
    u64::try_from(duration.as_nanos()).map_err(Into::into)
}

fn build_corpus(path: &Path, truncate: bool) -> Result<CorpusObservation, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_relation(table, "wal_replay_rows")?;
    seed.insert(table, b"key".to_vec(), b"history-0".to_vec())?;
    seed.set(b"session".to_vec(), b"history-0".to_vec(), None)?;
    seed.create_search_index(search, "wal_replay_search")?;
    seed.index_document(search, b"seed".to_vec(), "historical seed")?;
    seed.commit()?;

    for version in 1..=HISTORICAL_UPDATES {
        let value = format!("history-{version}").into_bytes();
        let logical_time = i64::try_from(version)?.saturating_add(1);
        let mut update = database.begin(logical_time, DurabilityClass::Strict)?;
        update.update(table, b"key".to_vec(), value.clone())?;
        update.set(b"session".to_vec(), value, None)?;
        update.commit()?;
    }
    let vacuum = database.vacuum_pages()?;
    if !vacuum.applied {
        return Err("benchmark corpus did not produce a page vacuum".into());
    }
    database.checkpoint()?;
    let retention = truncate
        .then(|| database.truncate_wal_at_retention_checkpoint())
        .transpose()?;

    for sequence in 1..=SUFFIX_COMMITS {
        let value = format!("suffix-{sequence}").into_bytes();
        let logical_time = i64::try_from(HISTORICAL_UPDATES + sequence + 2)?;
        let mut suffix = database.begin(logical_time, DurabilityClass::Strict)?;
        suffix.update(table, b"key".to_vec(), value.clone())?;
        suffix.set(b"session".to_vec(), value, None)?;
        suffix.index_document(
            search,
            format!("suffix-doc-{sequence}").into_bytes(),
            format!("suffix{sequence} retained"),
        )?;
        suffix.commit()?;
    }
    database.checkpoint()?;
    verify(&database)?;
    drop(database);
    Ok(CorpusObservation {
        wal_bytes: fs::metadata(path.join(WAL_FILE))?.len(),
        retention,
    })
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let expected = format!("suffix-{SUFFIX_COMMITS}").into_bytes();
    let expected_document_id = format!("suffix-doc-{SUFFIX_COMMITS}");
    if database.select_latest_relational(table, b"key")? != Some(expected.clone())
        || database.get_latest_structure(b"session", i64::MAX)? != Some(expected)
        || database
            .match_latest_text(search, &format!("suffix{SUFFIX_COMMITS}"), 1)?
            .first()
            .map(|hit| hit.document_id.as_slice())
            != Some(expected_document_id.as_bytes())
    {
        return Err("reopened corpus diverged from the expected all-engine state".into());
    }
    Ok(())
}

fn observe_open(path: &Path) -> Result<OpenObservation, BenchmarkError> {
    let mut external = Vec::with_capacity(OPEN_ROUNDS);
    let mut internal = Vec::with_capacity(OPEN_ROUNDS);
    let mut physical = Vec::with_capacity(OPEN_ROUNDS);
    let mut semantic = Vec::with_capacity(OPEN_ROUNDS);
    let mut roots = Vec::with_capacity(OPEN_ROUNDS);
    let mut final_report = None;
    for _ in 0..OPEN_ROUNDS {
        let started = Instant::now();
        let database = NativeDatabase::open(path)?;
        external.push(nanos(started.elapsed())?);
        verify(&database)?;
        let report = database.recovery_report();
        internal.push(nanos(report.open_time)?);
        physical.push(nanos(report.wal_physical_verification_time)?);
        semantic.push(nanos(report.wal_semantic_replay_time)?);
        roots.push(nanos(report.root_validation_time)?);
        final_report = Some(report.clone());
    }
    let report = final_report.ok_or("no reopen report observed")?;
    Ok(OpenObservation {
        first_external_nanos: external[0],
        warm_external: latency_stats(external[1..].to_vec()),
        warm_internal: latency_stats(internal[1..].to_vec()),
        warm_physical: latency_stats(physical[1..].to_vec()),
        warm_semantic: latency_stats(semantic[1..].to_vec()),
        warm_roots: latency_stats(roots[1..].to_vec()),
        retained_blocks: report.retained_wal_blocks,
        replayed_transactions: report.replayed_transactions,
        committed_transactions: report.committed_transactions,
        verified: true,
    })
}

fn print_stats(name: &str, stats: LatencyStats, trailing_comma: bool) {
    println!("  \"{name}\": {{");
    println!("    \"p50_nanos\": {},", stats.p50);
    println!("    \"p95_nanos\": {},", stats.p95);
    println!("    \"p99_nanos\": {}", stats.p99);
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

fn print_open(prefix: &str, observation: &OpenObservation, trailing_comma: bool) {
    println!("  \"{prefix}\": {{");
    println!(
        "    \"first_external_nanos\": {},",
        observation.first_external_nanos
    );
    println!(
        "    \"retained_wal_blocks\": {},",
        observation.retained_blocks
    );
    println!(
        "    \"replayed_transactions\": {},",
        observation.replayed_transactions
    );
    println!(
        "    \"committed_transactions\": {},",
        observation.committed_transactions
    );
    println!("    \"verified\": {},", observation.verified);
    print_stats("warm_external", observation.warm_external, true);
    print_stats("warm_internal", observation.warm_internal, true);
    print_stats("warm_physical_wal", observation.warm_physical, true);
    print_stats("warm_semantic_replay", observation.warm_semantic, true);
    print_stats("warm_root_validation", observation.warm_roots, false);
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

fn print_retention(receipt: WalRetentionReceipt) -> Result<(), BenchmarkError> {
    println!("  \"retention\": {{");
    println!("    \"anchor_epoch\": {},", receipt.anchor_epoch);
    println!(
        "    \"base_visible_csn\": {},",
        receipt.base_visible_csn.get()
    );
    println!(
        "    \"retired_wal_blocks\": {},",
        receipt.retired_wal_blocks
    );
    println!("    \"retired_wal_bytes\": {},", receipt.retired_wal_bytes);
    println!(
        "    \"anchor_publication_nanos\": {},",
        nanos(receipt.anchor_publication_time)?
    );
    println!(
        "    \"wal_reset_synchronization_nanos\": {},",
        nanos(receipt.wal_reset_synchronization_time)?
    );
    println!(
        "    \"anchor_stabilization_nanos\": {},",
        nanos(receipt.anchor_stabilization_time)?
    );
    println!("    \"total_nanos\": {}", nanos(receipt.total_time)?);
    println!("  }},");
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let source_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let source_tree = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "unknown".to_owned());
    let full_directory = TemporaryDirectory::create("full")?;
    let retained_directory = TemporaryDirectory::create("retained")?;
    let full_corpus = build_corpus(full_directory.path(), false)?;
    let retained_corpus = build_corpus(retained_directory.path(), true)?;
    let full_open = observe_open(full_directory.path())?;
    let retained_open = observe_open(retained_directory.path())?;
    let retention = retained_corpus
        .retention
        .ok_or("retained corpus did not produce a receipt")?;
    let full_wal_bytes = f64::from(u32::try_from(full_corpus.wal_bytes)?);
    let retained_wal_bytes = f64::from(u32::try_from(retained_corpus.wal_bytes)?);
    let warm_speedup = Duration::from_nanos(full_open.warm_external.p50).as_secs_f64()
        / Duration::from_nanos(retained_open.warm_external.p50).as_secs_f64();

    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-wal-replay-v1\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"source_tree\": \"{source_tree}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"historical_updates\": {HISTORICAL_UPDATES},");
    println!("  \"pre_floor_commits\": {PRE_FLOOR_COMMITS},");
    println!("  \"suffix_commits\": {SUFFIX_COMMITS},");
    println!("  \"open_rounds\": {OPEN_ROUNDS},");
    println!("  \"full_wal_bytes\": {},", full_corpus.wal_bytes);
    println!("  \"retained_wal_bytes\": {},", retained_corpus.wal_bytes);
    println!(
        "  \"wal_byte_reduction\": {:.6},",
        1.0 - retained_wal_bytes / full_wal_bytes
    );
    println!("  \"warm_reopen_speedup\": {warm_speedup:.6},");
    print_retention(retention)?;
    print_open("full_history_open", &full_open, true);
    print_open("retained_suffix_open", &retained_open, false);
    println!("}}");
    Ok(())
}
