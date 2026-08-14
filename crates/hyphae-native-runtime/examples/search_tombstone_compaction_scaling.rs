// SPDX-License-Identifier: AGPL-3.0-only

//! Direct-Linux scaling observations for native lexical tombstone compaction.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativePhysicalObservation, SearchCompactionReceipt};
use hyphae_native_types::{DurabilityClass, ObjectId};

const POPULATIONS: [u64; 2] = [256, 4_096];
const TOMBSTONE_PERCENTAGES: [u64; 2] = [25, 75];
const DEFAULT_OBSERVATIONS: u64 = 5;
const MAX_OBSERVATIONS: u64 = 64;
const SEARCH_INDEX: u128 = 100;

type BenchmarkError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-search-compaction-scaling-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
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

#[derive(Clone, Copy)]
struct Distribution {
    p50: u64,
    p95: u64,
    maximum: u64,
}

impl Distribution {
    fn from_samples(mut samples: Vec<u64>) -> Result<Self, BenchmarkError> {
        samples.sort_unstable();
        Ok(Self {
            p50: percentile(&samples, 50)?,
            p95: percentile(&samples, 95)?,
            maximum: *samples.last().ok_or("empty benchmark distribution")?,
        })
    }
}

fn percentile(samples: &[u64], percentile: usize) -> Result<u64, BenchmarkError> {
    let index = samples
        .len()
        .checked_sub(1)
        .ok_or("empty benchmark distribution")?
        .saturating_mul(percentile)
        / 100;
    samples
        .get(index)
        .copied()
        .ok_or_else(|| "invalid benchmark percentile".into())
}

fn nanos(duration: Duration) -> Result<u64, BenchmarkError> {
    Ok(u64::try_from(duration.as_nanos())?)
}

#[derive(Clone, Copy)]
struct PhysicalDelta {
    reads: u64,
    appends: u64,
    wal_bytes: u64,
    full_state_loads: u64,
    full_catalog_loads: u64,
}

impl PhysicalDelta {
    fn between(
        before: NativePhysicalObservation,
        after: NativePhysicalObservation,
    ) -> Result<Self, BenchmarkError> {
        Ok(Self {
            reads: after
                .physical_page_reads
                .checked_sub(before.physical_page_reads)
                .ok_or("page read counter regressed")?,
            appends: after
                .page_count
                .checked_sub(before.page_count)
                .ok_or("page count regressed")?,
            wal_bytes: after
                .wal_bytes
                .checked_sub(before.wal_bytes)
                .ok_or("WAL byte count regressed")?,
            full_state_loads: after
                .process_full_state_loads
                .checked_sub(before.process_full_state_loads)
                .ok_or("full-state load counter regressed")?,
            full_catalog_loads: after
                .process_full_catalog_loads
                .checked_sub(before.process_full_catalog_loads)
                .ok_or("full-catalog load counter regressed")?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReceiptShape {
    scanned_entries: usize,
    retained_entries: usize,
    dropped_tombstones: usize,
    reachable_pages_before: usize,
    reachable_pages_after: usize,
    pages_appended: u64,
}

impl From<SearchCompactionReceipt> for ReceiptShape {
    fn from(receipt: SearchCompactionReceipt) -> Self {
        Self {
            scanned_entries: receipt.scanned_entries,
            retained_entries: receipt.retained_entries,
            dropped_tombstones: receipt.dropped_tombstones,
            reachable_pages_before: receipt.reachable_pages_before,
            reachable_pages_after: receipt.reachable_pages_after,
            pages_appended: receipt.pages_appended,
        }
    }
}

struct Samples {
    latency: Vec<u64>,
    reads: Vec<u64>,
    appends: Vec<u64>,
    wal_bytes: Vec<u64>,
    full_state_loads: Vec<u64>,
    full_catalog_loads: Vec<u64>,
}

impl Samples {
    fn new(observations: u64) -> Result<Self, BenchmarkError> {
        let capacity = usize::try_from(observations)?;
        Ok(Self {
            latency: Vec::with_capacity(capacity),
            reads: Vec::with_capacity(capacity),
            appends: Vec::with_capacity(capacity),
            wal_bytes: Vec::with_capacity(capacity),
            full_state_loads: Vec::with_capacity(capacity),
            full_catalog_loads: Vec::with_capacity(capacity),
        })
    }

    fn push(&mut self, latency: Duration, physical: PhysicalDelta) -> Result<(), BenchmarkError> {
        self.latency.push(nanos(latency)?);
        self.reads.push(physical.reads);
        self.appends.push(physical.appends);
        self.wal_bytes.push(physical.wal_bytes);
        self.full_state_loads.push(physical.full_state_loads);
        self.full_catalog_loads.push(physical.full_catalog_loads);
        Ok(())
    }
}

struct ResultRow {
    population: u64,
    tombstone_percentage: u64,
    deleted_documents: u64,
    observations: u64,
    plan: Samples,
    memory: Samples,
    strict: Samples,
    receipt: ReceiptShape,
}

fn document_id(sequence: u64) -> Vec<u8> {
    format!("doc-{sequence:08}").into_bytes()
}

fn document_text(sequence: u64) -> String {
    format!("shared token{sequence:08}")
}

fn seed_baseline(path: &Path, population: u64) -> Result<(), BenchmarkError> {
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "compaction_documents")?;
    for sequence in 0..population {
        seed.index_document(index, document_id(sequence), document_text(sequence))?;
    }
    seed.commit()?;
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), BenchmarkError> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else {
            fs::copy(source_path, &destination_path)?;
            fs::File::open(destination_path)?.sync_all()?;
        }
    }
    Ok(())
}

fn prepare_tombstones(
    baseline: &Path,
    path: &Path,
    deleted_documents: u64,
) -> Result<(), BenchmarkError> {
    copy_directory(baseline, path)?;
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut database = NativeDatabase::open(path)?;
    let mut deletion = database.begin(2, DurabilityClass::Strict)?;
    for sequence in 0..deleted_documents {
        deletion.delete_document(index, document_id(sequence))?;
    }
    deletion.commit()?;
    Ok(())
}

fn measure_plan(
    root: &Path,
    baseline: &Path,
    label: &str,
    observations: u64,
) -> Result<Samples, BenchmarkError> {
    let mut samples = Samples::new(observations)?;
    for observation in 0..observations {
        let path = root.join(format!("{label}-plan-{observation}"));
        copy_directory(baseline, &path)?;
        let mut database = NativeDatabase::open(&path)?;
        let before = database.physical_observation()?;
        let started = Instant::now();
        let receipt = black_box(database.compact_search(DurabilityClass::Memory)?);
        let latency = started.elapsed();
        let after = database.physical_observation()?;
        let physical = PhysicalDelta::between(before, after)?;
        if receipt.commit.is_some()
            || receipt.dropped_tombstones != 0
            || physical.appends != 0
            || physical.wal_bytes != 0
        {
            return Err("planning observation changed the V1 baseline".into());
        }
        samples.push(latency, physical)?;
        drop(database);
        fs::remove_dir_all(path)?;
    }
    Ok(samples)
}

fn measure_compaction(
    root: &Path,
    baseline: &Path,
    label: &str,
    durability: DurabilityClass,
    observations: u64,
) -> Result<(Samples, ReceiptShape), BenchmarkError> {
    let mut samples = Samples::new(observations)?;
    let mut expected_receipt = None;
    for observation in 0..observations {
        let path = root.join(format!("{label}-{durability:?}-{observation}"));
        copy_directory(baseline, &path)?;
        let mut database = NativeDatabase::open(&path)?;
        let before = database.physical_observation()?;
        let started = Instant::now();
        let receipt = black_box(database.compact_search(durability)?);
        let latency = started.elapsed();
        let after = database.physical_observation()?;
        let physical = PhysicalDelta::between(before, after)?;
        let shape = ReceiptShape::from(receipt);
        if receipt.commit.is_none()
            || receipt.dropped_tombstones == 0
            || physical.appends != receipt.pages_appended
            || physical.wal_bytes == 0
        {
            return Err("applied compaction receipt and physical delta disagree".into());
        }
        if let Some(expected) = expected_receipt {
            if shape != expected {
                return Err("compaction receipt changed across identical datasets".into());
            }
        } else {
            expected_receipt = Some(shape);
        }
        samples.push(latency, physical)?;
        drop(database);
        fs::remove_dir_all(path)?;
    }
    Ok((
        samples,
        expected_receipt.ok_or("missing compaction receipt")?,
    ))
}

fn collect_rows(root: &Path, observations: u64) -> Result<Vec<ResultRow>, BenchmarkError> {
    let mut rows = Vec::new();
    for population in POPULATIONS {
        let baseline = root.join(format!("population-{population}-baseline"));
        seed_baseline(&baseline, population)?;
        for percentage in TOMBSTONE_PERCENTAGES {
            let deleted_documents = population
                .checked_mul(percentage)
                .ok_or("deleted document count overflow")?
                / 100;
            let label = format!("population-{population}-tombstones-{percentage}");
            let tombstone_baseline = root.join(format!("{label}-baseline"));
            prepare_tombstones(&baseline, &tombstone_baseline, deleted_documents)?;
            let plan = measure_plan(root, &baseline, &label, observations)?;
            let (memory, receipt) = measure_compaction(
                root,
                &tombstone_baseline,
                &label,
                DurabilityClass::Memory,
                observations,
            )?;
            let (strict, strict_receipt) = measure_compaction(
                root,
                &tombstone_baseline,
                &label,
                DurabilityClass::Strict,
                observations,
            )?;
            if strict_receipt != receipt {
                return Err("durability changed the physical compaction result".into());
            }
            rows.push(ResultRow {
                population,
                tombstone_percentage: percentage,
                deleted_documents,
                observations,
                plan,
                memory,
                strict,
                receipt,
            });
        }
    }
    Ok(rows)
}

fn print_distribution(name: &str, samples: Vec<u64>, trailing: bool) -> Result<(), BenchmarkError> {
    let distribution = Distribution::from_samples(samples)?;
    println!("        \"{name}\": {{");
    println!("          \"p50\": {},", distribution.p50);
    println!("          \"p95\": {},", distribution.p95);
    println!("          \"maximum\": {}", distribution.maximum);
    println!("        }}{}", if trailing { "," } else { "" });
    Ok(())
}

fn print_samples(name: &str, samples: Samples, trailing: bool) -> Result<(), BenchmarkError> {
    println!("      \"{name}\": {{");
    print_distribution("latency_nanos", samples.latency, true)?;
    print_distribution("physical_page_reads", samples.reads, true)?;
    print_distribution("page_appends", samples.appends, true)?;
    print_distribution("wal_bytes_appended", samples.wal_bytes, true)?;
    print_distribution("full_state_loads", samples.full_state_loads, true)?;
    print_distribution("full_catalog_loads", samples.full_catalog_loads, false)?;
    println!("      }}{}", if trailing { "," } else { "" });
    Ok(())
}

fn print_receipt(receipt: ReceiptShape) {
    println!("      \"receipt\": {{");
    println!("        \"scanned_entries\": {},", receipt.scanned_entries);
    println!(
        "        \"retained_entries\": {},",
        receipt.retained_entries
    );
    println!(
        "        \"dropped_tombstones\": {},",
        receipt.dropped_tombstones
    );
    println!(
        "        \"reachable_pages_before\": {},",
        receipt.reachable_pages_before
    );
    println!(
        "        \"reachable_pages_after\": {},",
        receipt.reachable_pages_after
    );
    println!("        \"pages_appended\": {}", receipt.pages_appended);
    println!("      }}");
}

fn print_row(row: ResultRow, trailing: bool) -> Result<(), BenchmarkError> {
    println!("    {{");
    println!("      \"population\": {},", row.population);
    println!(
        "      \"tombstone_percentage\": {},",
        row.tombstone_percentage
    );
    println!("      \"deleted_documents\": {},", row.deleted_documents);
    println!("      \"observations\": {},", row.observations);
    print_samples("validated_v1_plan", row.plan, true)?;
    print_samples("memory_compaction", row.memory, true)?;
    print_samples("strict_compaction", row.strict, true)?;
    print_receipt(row.receipt);
    println!("    }}{}", if trailing { "," } else { "" });
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let implementation_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unknown".to_owned());
    let observations = std::env::var("HYPHAE_COMPACTION_OBSERVATIONS")
        .map_or(Ok(DEFAULT_OBSERVATIONS), |value| value.parse::<u64>())?;
    if observations == 0 || observations > MAX_OBSERVATIONS {
        return Err("HYPHAE_COMPACTION_OBSERVATIONS must be in 1..=64".into());
    }
    let temporary = TemporaryDirectory::create()?;
    let rows = collect_rows(temporary.path(), observations)?;
    let row_count = rows.len();
    println!("{{");
    println!("  \"schema\": \"hyphae.native.search-tombstone-compaction-scaling.v1\",");
    println!("  \"status\": \"observation-not-regression-gate\",");
    println!("  \"implementation_commit\": \"{implementation_commit}\",");
    println!("  \"profile\": \"release\",");
    println!("  \"rows\": [");
    for (offset, row) in rows.into_iter().enumerate() {
        print_row(row, offset + 1 != row_count)?;
    }
    println!("  ]");
    println!("}}");
    Ok(())
}
