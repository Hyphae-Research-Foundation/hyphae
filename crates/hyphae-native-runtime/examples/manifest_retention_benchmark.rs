// SPDX-License-Identifier: Apache-2.0

//! Reproducible complete-chain versus retained-manifest reopen observation.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_ann::HnswConfig;
use hyphae_native_runtime::{
    AnnSearchOptions, NativeDatabase, Vector, VectorMetric, WalRetentionReceipt,
};
use hyphae_native_types::{DurabilityClass, ManifestGeneration, ObjectId};

const HISTORICAL_UPDATES: usize = 8;
const PRE_BASE_MANIFESTS: usize = 128;
const SUFFIX_CHECKPOINTS: usize = 2;
const OPEN_ROUNDS: usize = 25;

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
}

#[derive(Clone, Copy)]
struct ManifestMetrics {
    files: usize,
    bytes: u64,
}

struct CorpusObservation {
    before_retention: ManifestMetrics,
    final_metrics: ManifestMetrics,
    retention: Option<WalRetentionReceipt>,
}

struct OpenObservation {
    first_external_nanos: u64,
    warm_external: LatencyStats,
    warm_internal: LatencyStats,
    warm_manifest_verification: LatencyStats,
    warm_physical_wal: LatencyStats,
    warm_semantic_replay: LatencyStats,
    warm_root_validation: LatencyStats,
    manifest_base_generation: ManifestGeneration,
    manifest_count: usize,
    retained_manifest_bytes: u64,
    checkpoint_count: usize,
    replayed_transactions: usize,
    verified: bool,
}

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-manifest-retention-{label}-{}-{timestamp}",
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

fn manifest_metrics(path: &Path) -> Result<ManifestMetrics, BenchmarkError> {
    let mut files = 0;
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path.join("roots"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().ends_with(".hyroot")
        {
            files += 1;
            bytes = bytes
                .checked_add(entry.metadata()?.len())
                .ok_or("manifest byte count overflow")?;
        }
    }
    Ok(ManifestMetrics { files, bytes })
}

fn vector_config() -> Result<HnswConfig, BenchmarkError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
}

fn build_corpus(path: &Path, truncate: bool) -> Result<CorpusObservation, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let vector_id = ObjectId::new(101)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_relation(table, "manifest_rows")?;
    seed.insert(table, b"key".to_vec(), b"history-0".to_vec())?;
    seed.set(b"session".to_vec(), b"history-0".to_vec(), None)?;
    seed.create_search_index(search, "manifest_search")?;
    seed.index_document(search, b"seed".to_vec(), "historical manifest")?;
    seed.create_vector_index(
        vectors,
        "manifest_vectors",
        3,
        VectorMetric::Cosine,
        vector_config()?,
    )?;
    seed.upsert_vector(vectors, vector_id, Vector::new([1.0, 0.0, 0.0])?)?;
    seed.commit()?;

    for version in 1..=HISTORICAL_UPDATES {
        let value = format!("history-{version}").into_bytes();
        let mut update = database.begin(
            i64::try_from(version)?.saturating_add(1),
            DurabilityClass::Strict,
        )?;
        update.update(table, b"key".to_vec(), value.clone())?;
        update.set(b"session".to_vec(), value, None)?;
        update.commit()?;
    }
    for _ in 0..PRE_BASE_MANIFESTS {
        database.checkpoint()?;
    }
    let vacuum = database.vacuum_pages()?;
    if !vacuum.applied {
        return Err("benchmark corpus did not produce a page vacuum".into());
    }
    let base_checkpoint = database.checkpoint()?;
    if usize::try_from(base_checkpoint.manifest_generation.get())? != PRE_BASE_MANIFESTS + 1 {
        return Err("benchmark produced an unexpected manifest base".into());
    }
    let before_retention = manifest_metrics(path)?;
    let retention = truncate
        .then(|| database.truncate_wal_at_retention_checkpoint())
        .transpose()?;

    let mut suffix = database.begin(100, DurabilityClass::Strict)?;
    suffix.update(table, b"key".to_vec(), b"suffix-final".to_vec())?;
    suffix.set(b"session".to_vec(), b"suffix-final".to_vec(), None)?;
    suffix.index_document(search, b"suffix-doc".to_vec(), "retained suffix")?;
    suffix.upsert_vector(vectors, vector_id, Vector::new([0.0, 1.0, 0.0])?)?;
    suffix.commit()?;
    for _ in 0..SUFFIX_CHECKPOINTS {
        database.checkpoint()?;
    }
    verify(&database)?;
    drop(database);
    Ok(CorpusObservation {
        before_retention,
        final_metrics: manifest_metrics(path)?,
        retention,
    })
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let vector_id = ObjectId::new(101)?;
    let ann = database.search_ann_latest(
        vectors,
        &Vector::new([0.0, 1.0, 0.0])?,
        AnnSearchOptions::new(1, 8, Some(4))?,
    )?;
    if database.select_latest_relational(table, b"key")? != Some(b"suffix-final".to_vec())
        || database.get_latest_structure(b"session", i64::MAX)? != Some(b"suffix-final".to_vec())
        || database.match_latest_text(search, "retained", 1)?[0].document_id != b"suffix-doc"
        || ann.hits.first().map(|hit| hit.object_id) != Some(vector_id)
    {
        return Err("reopened corpus diverged from the expected all-engine state".into());
    }
    Ok(())
}

fn observe_open(path: &Path) -> Result<OpenObservation, BenchmarkError> {
    let mut external = Vec::with_capacity(OPEN_ROUNDS);
    let mut internal = Vec::with_capacity(OPEN_ROUNDS);
    let mut manifests = Vec::with_capacity(OPEN_ROUNDS);
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
        manifests.push(nanos(report.manifest_verification_time)?);
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
        warm_manifest_verification: latency_stats(manifests[1..].to_vec()),
        warm_physical_wal: latency_stats(physical[1..].to_vec()),
        warm_semantic_replay: latency_stats(semantic[1..].to_vec()),
        warm_root_validation: latency_stats(roots[1..].to_vec()),
        manifest_base_generation: report
            .manifest_base_generation
            .ok_or("reopen did not report a manifest base")?,
        manifest_count: report.manifest_count,
        retained_manifest_bytes: report.retained_manifest_bytes,
        checkpoint_count: report.checkpoint_count,
        replayed_transactions: report.replayed_transactions,
        verified: true,
    })
}

fn print_stats(name: &str, stats: LatencyStats, trailing_comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_nanos\": {},", stats.p50);
    println!("      \"p95_nanos\": {},", stats.p95);
    println!("      \"p99_nanos\": {}", stats.p99);
    println!("    }}{}", if trailing_comma { "," } else { "" });
}

fn print_open(name: &str, observation: &OpenObservation, trailing_comma: bool) {
    println!("  \"{name}\": {{");
    println!(
        "    \"first_external_nanos\": {},",
        observation.first_external_nanos
    );
    println!(
        "    \"manifest_base_generation\": {},",
        observation.manifest_base_generation.get()
    );
    println!("    \"manifest_count\": {},", observation.manifest_count);
    println!(
        "    \"retained_manifest_bytes\": {},",
        observation.retained_manifest_bytes
    );
    println!(
        "    \"checkpoint_count\": {},",
        observation.checkpoint_count
    );
    println!(
        "    \"replayed_transactions\": {},",
        observation.replayed_transactions
    );
    println!("    \"verified\": {},", observation.verified);
    print_stats("warm_external", observation.warm_external, true);
    print_stats("warm_internal", observation.warm_internal, true);
    print_stats(
        "warm_manifest_verification",
        observation.warm_manifest_verification,
        true,
    );
    print_stats("warm_physical_wal", observation.warm_physical_wal, true);
    print_stats(
        "warm_semantic_replay",
        observation.warm_semantic_replay,
        true,
    );
    print_stats(
        "warm_root_validation",
        observation.warm_root_validation,
        false,
    );
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
        "    \"retired_manifest_files\": {},",
        receipt.retired_manifest_files
    );
    println!(
        "    \"retired_manifest_bytes\": {},",
        receipt.retired_manifest_bytes
    );
    println!(
        "    \"retained_manifest_files\": {},",
        receipt.retained_manifest_files
    );
    println!(
        "    \"retained_manifest_bytes\": {},",
        receipt.retained_manifest_bytes
    );
    println!(
        "    \"manifest_pruning_nanos\": {},",
        nanos(receipt.manifest_pruning_time)?
    );
    println!(
        "    \"manifest_directory_sync_supported\": {},",
        receipt.manifest_directory_sync_supported
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
    let full_manifest_bytes = f64::from(u32::try_from(full_corpus.final_metrics.bytes)?);
    let retained_manifest_bytes = f64::from(u32::try_from(retained_corpus.final_metrics.bytes)?);
    let manifest_speedup = Duration::from_nanos(full_open.warm_manifest_verification.p50)
        .as_secs_f64()
        / Duration::from_nanos(retained_open.warm_manifest_verification.p50).as_secs_f64();
    let reopen_speedup = Duration::from_nanos(full_open.warm_external.p50).as_secs_f64()
        / Duration::from_nanos(retained_open.warm_external.p50).as_secs_f64();

    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-manifest-retention-v1\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"source_tree\": \"{source_tree}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"historical_updates\": {HISTORICAL_UPDATES},");
    println!("  \"pre_base_manifests\": {PRE_BASE_MANIFESTS},");
    println!("  \"suffix_checkpoints\": {SUFFIX_CHECKPOINTS},");
    println!("  \"open_rounds\": {OPEN_ROUNDS},");
    println!(
        "  \"manifests_before_retention\": {},",
        retained_corpus.before_retention.files
    );
    println!(
        "  \"manifest_bytes_before_retention\": {},",
        retained_corpus.before_retention.bytes
    );
    println!(
        "  \"full_final_manifest_files\": {},",
        full_corpus.final_metrics.files
    );
    println!(
        "  \"full_final_manifest_bytes\": {},",
        full_corpus.final_metrics.bytes
    );
    println!(
        "  \"retained_final_manifest_files\": {},",
        retained_corpus.final_metrics.files
    );
    println!(
        "  \"retained_final_manifest_bytes\": {},",
        retained_corpus.final_metrics.bytes
    );
    println!(
        "  \"manifest_byte_reduction\": {:.6},",
        1.0 - retained_manifest_bytes / full_manifest_bytes
    );
    println!("  \"manifest_verification_speedup\": {manifest_speedup:.6},");
    println!("  \"warm_reopen_speedup\": {reopen_speedup:.6},");
    print_retention(retention)?;
    print_open("full_chain_open", &full_open, true);
    print_open("retained_chain_open", &retained_open, false);
    println!("}}");
    Ok(())
}
