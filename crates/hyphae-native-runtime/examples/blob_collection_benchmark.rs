// SPDX-License-Identifier: AGPL-3.0-only

//! Reproducible retained-root immutable-blob collection observation.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_ann::HnswConfig;
use hyphae_native_catalog::{CatalogName, QualifiedName};
use hyphae_native_runtime::{
    AnnSearchOptions, BlobCollectionReceipt, NativeDatabase, Vector, VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

const DEAD_BLOBS: usize = 128;
const OPEN_ROUNDS: usize = 25;
const LARGE_TEXT_TOKENS: usize = 4_500;

type BenchmarkError = Box<dyn std::error::Error + Send + Sync>;

struct TemporaryDirectory(PathBuf);

#[derive(Clone, Copy)]
struct LatencyStats {
    p50: u64,
    p95: u64,
    p99: u64,
}

#[derive(Clone, Copy)]
struct BlobMetrics {
    files: usize,
    bytes: u64,
}

struct CorpusObservation {
    before_collection: BlobMetrics,
    final_metrics: BlobMetrics,
    collection: Option<BlobCollectionReceipt>,
}

struct OpenObservation {
    first_external_nanos: u64,
    warm_external: LatencyStats,
    warm_internal: LatencyStats,
    warm_blob_verification: LatencyStats,
    warm_root_validation: LatencyStats,
    blob_files: usize,
    blob_bytes: u64,
    generation_floor: u64,
    effective_generation: u64,
    verified: bool,
}

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-blob-collection-{label}-{}-{timestamp}",
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

fn blob_metrics(path: &Path) -> Result<BlobMetrics, BenchmarkError> {
    let mut files = 0;
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path.join("blobs"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().ends_with(".hyblob")
        {
            files += 1;
            bytes = bytes
                .checked_add(entry.metadata()?.len())
                .ok_or("blob byte count overflow")?;
        }
    }
    Ok(BlobMetrics { files, bytes })
}

fn vector_config() -> Result<HnswConfig, BenchmarkError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x424c_4f42_4743)?)
}

fn wide_catalog_sql() -> String {
    let mut columns = vec!["id BINARY PRIMARY KEY".to_owned()];
    for sequence in 1..=64 {
        columns.push(format!(
            "catalog_column_{sequence:03}_{} BINARY",
            "x".repeat(180)
        ));
    }
    format!("CREATE TABLE wide_catalog ({})", columns.join(", "))
}

fn wide_catalog_name() -> Result<QualifiedName, BenchmarkError> {
    Ok(QualifiedName::new(
        CatalogName::unquoted("main")?,
        CatalogName::unquoted("public")?,
        CatalogName::unquoted("wide_catalog")?,
    ))
}

fn large_text(version: usize) -> String {
    format!("versiontoken{version} {}", "x ".repeat(LARGE_TEXT_TOKENS))
}

fn build_corpus(path: &Path, collect: bool) -> Result<CorpusObservation, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let vector_id = ObjectId::new(101)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_relation(table, "blob_rows")?;
    seed.insert(table, b"key".to_vec(), b"inline-seed".to_vec())?;
    seed.set(b"session".to_vec(), b"inline-seed".to_vec(), None)?;
    seed.create_search_index(search, "blob_search")?;
    seed.index_document(search, b"seed".to_vec(), "blob collection seed")?;
    seed.create_vector_index(
        vectors,
        "blob_vectors",
        3,
        VectorMetric::Cosine,
        vector_config()?,
    )?;
    seed.upsert_vector(vectors, vector_id, Vector::new([1.0, 0.0, 0.0])?)?;
    let _created = seed.execute_sql(&wide_catalog_sql(), &[])?;
    seed.commit()?;

    for version in 0..=DEAD_BLOBS {
        let text = large_text(version);
        let bytes = text.as_bytes().to_vec();
        let mut update = database.begin(i64::try_from(version)? + 2, DurabilityClass::Strict)?;
        update.update(table, b"key".to_vec(), bytes.clone())?;
        update.set(b"session".to_vec(), bytes, None)?;
        if version == DEAD_BLOBS {
            update.index_document(search, b"current-blob".to_vec(), &text)?;
            update.upsert_vector(vectors, vector_id, Vector::new([0.0, 1.0, 0.0])?)?;
        }
        update.commit()?;
    }

    let vacuum = database.vacuum_pages()?;
    if !vacuum.applied {
        return Err("benchmark corpus did not produce a page vacuum".into());
    }
    database.checkpoint()?;
    database.truncate_wal_at_retention_checkpoint()?;
    let before_collection = blob_metrics(path)?;
    if before_collection.files != DEAD_BLOBS + 2 {
        return Err("benchmark produced an unexpected physical blob count".into());
    }
    let collection = collect.then(|| database.collect_blobs()).transpose()?;
    verify(&database)?;
    drop(database);
    Ok(CorpusObservation {
        before_collection,
        final_metrics: blob_metrics(path)?,
        collection,
    })
}

fn verify(database: &NativeDatabase) -> Result<(), BenchmarkError> {
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let vector_id = ObjectId::new(101)?;
    let expected = large_text(DEAD_BLOBS).into_bytes();
    let ann = database.search_ann_latest(
        vectors,
        &Vector::new([0.0, 1.0, 0.0])?,
        AnnSearchOptions::new(1, 8, Some(4))?,
    )?;
    if database.select_latest_relational(table, b"key")? != Some(expected.clone())
        || database.get_latest_structure(b"session", i64::MAX)? != Some(expected)
        || database.match_latest_text(search, &format!("versiontoken{DEAD_BLOBS}"), 1)?[0]
            .document_id
            != b"current-blob"
        || ann.hits.first().map(|hit| hit.object_id) != Some(vector_id)
        || database
            .catalog_object_named_latest(&wide_catalog_name()?)?
            .is_none()
    {
        return Err("reopened corpus diverged from expected all-engine state".into());
    }
    Ok(())
}

fn observe_open(path: &Path) -> Result<OpenObservation, BenchmarkError> {
    let mut external = Vec::with_capacity(OPEN_ROUNDS);
    let mut internal = Vec::with_capacity(OPEN_ROUNDS);
    let mut blobs = Vec::with_capacity(OPEN_ROUNDS);
    let mut roots = Vec::with_capacity(OPEN_ROUNDS);
    let mut final_report = None;
    for _ in 0..OPEN_ROUNDS {
        let started = Instant::now();
        let database = NativeDatabase::open(path)?;
        external.push(nanos(started.elapsed())?);
        verify(&database)?;
        let report = database.recovery_report();
        internal.push(nanos(report.open_time)?);
        blobs.push(nanos(report.blob_verification_time)?);
        roots.push(nanos(report.root_validation_time)?);
        final_report = Some(report.clone());
    }
    let report = final_report.ok_or("no reopen report observed")?;
    Ok(OpenObservation {
        first_external_nanos: external[0],
        warm_external: latency_stats(external[1..].to_vec()),
        warm_internal: latency_stats(internal[1..].to_vec()),
        warm_blob_verification: latency_stats(blobs[1..].to_vec()),
        warm_root_validation: latency_stats(roots[1..].to_vec()),
        blob_files: report.blob_count,
        blob_bytes: report.blob_bytes,
        generation_floor: report.blob_generation_floor,
        effective_generation: report.blob_generation,
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
    println!("    \"blob_files\": {},", observation.blob_files);
    println!("    \"blob_bytes\": {},", observation.blob_bytes);
    println!(
        "    \"generation_floor\": {},",
        observation.generation_floor
    );
    println!(
        "    \"effective_generation\": {},",
        observation.effective_generation
    );
    println!("    \"verified\": {},", observation.verified);
    print_stats("warm_external", observation.warm_external, true);
    print_stats("warm_internal", observation.warm_internal, true);
    print_stats(
        "warm_blob_verification",
        observation.warm_blob_verification,
        true,
    );
    print_stats(
        "warm_root_validation",
        observation.warm_root_validation,
        false,
    );
    println!("  }}{}", if trailing_comma { "," } else { "" });
}

fn print_collection(receipt: BlobCollectionReceipt) -> Result<(), BenchmarkError> {
    println!("  \"collection\": {{");
    println!(
        "    \"root_visible_csn\": {},",
        receipt.root_visible_csn.get()
    );
    println!("    \"generation_floor\": {},", receipt.generation_floor);
    println!("    \"live_files\": {},", receipt.live_files);
    println!("    \"live_bytes\": {},", receipt.live_bytes);
    println!("    \"candidate_files\": {},", receipt.candidate_files);
    println!("    \"candidate_bytes\": {},", receipt.candidate_bytes);
    println!("    \"removed_files\": {},", receipt.removed_files);
    println!("    \"removed_bytes\": {},", receipt.removed_bytes);
    println!("    \"retained_files\": {},", receipt.retained_files);
    println!("    \"retained_bytes\": {},", receipt.retained_bytes);
    println!(
        "    \"reference_trace_nanos\": {},",
        nanos(receipt.reference_trace_time)?
    );
    println!(
        "    \"candidate_deletion_nanos\": {},",
        nanos(receipt.candidate_deletion_time)?
    );
    println!(
        "    \"directory_synchronization_nanos\": {},",
        nanos(receipt.directory_synchronization_time)?
    );
    println!(
        "    \"parent_directory_sync_supported\": {},",
        receipt.parent_directory_sync_supported
    );
    println!("    \"total_nanos\": {}", nanos(receipt.total_time)?);
    println!("  }},");
    Ok(())
}

fn argument_or(index: usize, default: &str) -> String {
    match std::env::args().nth(index) {
        Some(value) => value,
        None => default.to_owned(),
    }
}

fn main() -> Result<(), BenchmarkError> {
    let source_commit = argument_or(1, "dirty-uncommitted");
    let source_tree = argument_or(2, "dirty-uncommitted");
    let rustc = argument_or(3, "unknown");
    let filesystem = argument_or(4, "unclassified");
    let uncollected_directory = TemporaryDirectory::create("uncollected")?;
    let collected_directory = TemporaryDirectory::create("collected")?;
    let uncollected_corpus = build_corpus(uncollected_directory.path(), false)?;
    let collected_corpus = build_corpus(collected_directory.path(), true)?;
    let uncollected_open = observe_open(uncollected_directory.path())?;
    let collected_open = observe_open(collected_directory.path())?;
    let collection = collected_corpus
        .collection
        .ok_or("collected corpus did not produce a receipt")?;
    let uncollected_bytes = f64::from(u32::try_from(uncollected_corpus.final_metrics.bytes)?);
    let collected_bytes = f64::from(u32::try_from(collected_corpus.final_metrics.bytes)?);
    let verification_speedup = Duration::from_nanos(uncollected_open.warm_blob_verification.p50)
        .as_secs_f64()
        / Duration::from_nanos(collected_open.warm_blob_verification.p50).as_secs_f64();
    let reopen_speedup = Duration::from_nanos(uncollected_open.warm_external.p50).as_secs_f64()
        / Duration::from_nanos(collected_open.warm_external.p50).as_secs_f64();

    println!("{{");
    println!("  \"benchmark\": \"hyphae-native-blob-collection-v1\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"source_tree\": \"{source_tree}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"os\": \"{}\",", std::env::consts::OS);
    println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
    println!("  \"filesystem\": \"{filesystem}\",");
    println!("  \"dead_blobs\": {DEAD_BLOBS},");
    println!("  \"open_rounds\": {OPEN_ROUNDS},");
    println!(
        "  \"blobs_before_collection\": {},",
        collected_corpus.before_collection.files
    );
    println!(
        "  \"blob_bytes_before_collection\": {},",
        collected_corpus.before_collection.bytes
    );
    println!(
        "  \"uncollected_final_blob_files\": {},",
        uncollected_corpus.final_metrics.files
    );
    println!(
        "  \"uncollected_final_blob_bytes\": {},",
        uncollected_corpus.final_metrics.bytes
    );
    println!(
        "  \"collected_final_blob_files\": {},",
        collected_corpus.final_metrics.files
    );
    println!(
        "  \"collected_final_blob_bytes\": {},",
        collected_corpus.final_metrics.bytes
    );
    println!(
        "  \"blob_byte_reduction\": {:.6},",
        1.0 - collected_bytes / uncollected_bytes
    );
    println!("  \"blob_verification_speedup\": {verification_speedup:.6},");
    println!("  \"warm_reopen_speedup\": {reopen_speedup:.6},");
    print_collection(collection)?;
    print_open("uncollected_open", &uncollected_open, true);
    print_open("collected_open", &collected_open, false);
    println!("}}");
    Ok(())
}
