// SPDX-License-Identifier: Apache-2.0

//! Direct-Linux observations for point-resolved lexical document lifecycle.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativePhysicalObservation, NativeWriteBatch};
use hyphae_native_types::{DurabilityClass, ObjectId};

const POPULATIONS: [u64; 3] = [0, 256, 4_096];
const OBSERVATIONS: u64 = 32;
const SEARCH_INDEX: u128 = 100;

type BenchmarkError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-search-lifecycle-scaling-{}-{timestamp}",
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
enum Operation {
    Replace,
    Delete,
}

impl Operation {
    const fn name(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Delete => "delete",
        }
    }

    fn stage(
        self,
        database: &NativeDatabase,
        batch: &mut NativeWriteBatch,
        index: ObjectId,
        document_id: Vec<u8>,
        sequence: u64,
    ) -> Result<(), BenchmarkError> {
        match self {
            Self::Replace => database.stage_delta_replace_document(
                batch,
                index,
                document_id,
                format!("hyphae native lifecycle replacement {sequence}"),
            )?,
            Self::Delete => {
                database.stage_delta_delete_document(batch, index, document_id)?;
            }
        }
        Ok(())
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

fn physical_delta(
    before: NativePhysicalObservation,
    after: NativePhysicalObservation,
) -> Result<PhysicalDelta, BenchmarkError> {
    Ok(PhysicalDelta {
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

struct Samples {
    stage: Vec<u64>,
    commit: Vec<u64>,
    total: Vec<u64>,
    stage_reads: Vec<u64>,
    commit_reads: Vec<u64>,
    page_appends: Vec<u64>,
    wal_bytes: Vec<u64>,
    full_state_loads: Vec<u64>,
    full_catalog_loads: Vec<u64>,
}

impl Samples {
    fn new() -> Result<Self, BenchmarkError> {
        let capacity = usize::try_from(OBSERVATIONS)?;
        Ok(Self {
            stage: Vec::with_capacity(capacity),
            commit: Vec::with_capacity(capacity),
            total: Vec::with_capacity(capacity),
            stage_reads: Vec::with_capacity(capacity),
            commit_reads: Vec::with_capacity(capacity),
            page_appends: Vec::with_capacity(capacity),
            wal_bytes: Vec::with_capacity(capacity),
            full_state_loads: Vec::with_capacity(capacity),
            full_catalog_loads: Vec::with_capacity(capacity),
        })
    }

    fn push_physical(&mut self, stage: PhysicalDelta, commit: PhysicalDelta) {
        self.stage_reads.push(stage.reads);
        self.commit_reads.push(commit.reads);
        self.page_appends.push(stage.appends + commit.appends);
        self.wal_bytes.push(stage.wal_bytes + commit.wal_bytes);
        self.full_state_loads
            .push(stage.full_state_loads + commit.full_state_loads);
        self.full_catalog_loads
            .push(stage.full_catalog_loads + commit.full_catalog_loads);
    }
}

struct ResultRow {
    population: u64,
    operation: Operation,
    durability: DurabilityClass,
    samples: Samples,
}

fn document_id(prefix: &str, sequence: u64) -> Vec<u8> {
    format!("{prefix}-{sequence:08}").into_bytes()
}

fn seed_database(path: &Path, population: u64) -> Result<(), BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "lifecycle_documents")?;
    for sequence in 0..OBSERVATIONS {
        seed.index_document(
            index,
            document_id("measured", sequence),
            format!("legacy native lifecycle document {sequence}"),
        )?;
    }
    for sequence in 0..population {
        seed.index_document(
            index,
            document_id("unrelated", sequence),
            "unrelated lexical population".to_owned(),
        )?;
    }
    seed.commit()?;
    drop(database);
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
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn measure_configuration(
    root: &Path,
    baseline: &Path,
    population: u64,
    operation: Operation,
    durability: DurabilityClass,
) -> Result<ResultRow, BenchmarkError> {
    let path = root.join(format!(
        "population-{population}-{}-{durability:?}",
        operation.name()
    ));
    copy_directory(baseline, &path)?;
    let mut database = NativeDatabase::open(&path)?;
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut samples = Samples::new()?;
    for sequence in 0..OBSERVATIONS {
        let total_started = Instant::now();
        let mut batch =
            database.begin_optimistic_delta(i64::try_from(sequence + 2)?, durability)?;
        let before_stage = database.physical_observation()?;
        let stage_started = Instant::now();
        operation.stage(
            &database,
            &mut batch,
            index,
            document_id("measured", sequence),
            sequence,
        )?;
        samples.stage.push(nanos(stage_started.elapsed())?);
        let after_stage = database.physical_observation()?;
        let commit_started = Instant::now();
        database.commit_optimistic(batch)?;
        samples.commit.push(nanos(commit_started.elapsed())?);
        samples.total.push(nanos(total_started.elapsed())?);
        let after_commit = database.physical_observation()?;
        samples.push_physical(
            physical_delta(before_stage, after_stage)?,
            physical_delta(after_stage, after_commit)?,
        );
    }
    Ok(ResultRow {
        population,
        operation,
        durability,
        samples,
    })
}

fn collect_rows(root: &Path) -> Result<Vec<ResultRow>, BenchmarkError> {
    let mut rows = Vec::new();
    for population in POPULATIONS {
        let baseline = root.join(format!("population-{population}-baseline"));
        seed_database(&baseline, population)?;
        for operation in [Operation::Replace, Operation::Delete] {
            for durability in [DurabilityClass::Memory, DurabilityClass::Strict] {
                rows.push(measure_configuration(
                    root, &baseline, population, operation, durability,
                )?);
            }
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

fn print_row(row: ResultRow, trailing: bool) -> Result<(), BenchmarkError> {
    let durability = match row.durability {
        DurabilityClass::Memory => "memory",
        DurabilityClass::Strict => "strict",
        DurabilityClass::Group => return Err("group durability is not measured".into()),
    };
    println!("    {{");
    println!("      \"unrelated_documents\": {},", row.population);
    println!("      \"operation\": \"{}\",", row.operation.name());
    println!("      \"durability\": \"{durability}\",");
    println!("      \"observations\": {},", row.samples.commit.len());
    println!("      \"distributions\": {{");
    print_distribution("stage_nanos", row.samples.stage, true)?;
    print_distribution("commit_nanos", row.samples.commit, true)?;
    print_distribution("total_nanos", row.samples.total, true)?;
    print_distribution("stage_page_reads", row.samples.stage_reads, true)?;
    print_distribution("commit_page_reads", row.samples.commit_reads, true)?;
    print_distribution("page_appends", row.samples.page_appends, true)?;
    print_distribution("wal_bytes_appended", row.samples.wal_bytes, true)?;
    print_distribution("full_state_loads", row.samples.full_state_loads, true)?;
    print_distribution("full_catalog_loads", row.samples.full_catalog_loads, false)?;
    println!("      }}");
    println!("    }}{}", if trailing { "," } else { "" });
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let implementation_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let rows = collect_rows(temporary.path())?;
    let row_count = rows.len();
    println!("{{");
    println!("  \"schema\": \"hyphae.native.search-document-lifecycle-scaling.v1\",");
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
