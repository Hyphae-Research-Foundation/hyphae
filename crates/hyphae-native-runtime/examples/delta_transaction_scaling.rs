// SPDX-License-Identifier: Apache-2.0

//! Direct-Linux scaling observations for point-resolved delta transactions.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, NativePhysicalObservation};
use hyphae_native_types::{DurabilityClass, ObjectId, ScalarValue};

const DEPTHS: [u64; 4] = [1, 32, 256, 1_024];
const POPULATIONS: [u64; 3] = [0, 256, 4_096];
const DEPTH_ROWS: u64 = 32;
const POPULATION_OBSERVATIONS: u64 = 32;
const SEARCH_INDEX: u128 = 100;

type BenchmarkError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, BenchmarkError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-delta-scaling-{}-{timestamp}",
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
    begin: Vec<u64>,
    sql_stage: Vec<u64>,
    structure_stage: Vec<u64>,
    search_stage: Vec<u64>,
    commit: Vec<u64>,
    total: Vec<u64>,
    reads: Vec<u64>,
    appends: Vec<u64>,
    wal_bytes: Vec<u64>,
    full_state_loads: Vec<u64>,
    full_catalog_loads: Vec<u64>,
}

impl Samples {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            begin: Vec::with_capacity(capacity),
            sql_stage: Vec::with_capacity(capacity),
            structure_stage: Vec::with_capacity(capacity),
            search_stage: Vec::with_capacity(capacity),
            commit: Vec::with_capacity(capacity),
            total: Vec::with_capacity(capacity),
            reads: Vec::with_capacity(capacity),
            appends: Vec::with_capacity(capacity),
            wal_bytes: Vec::with_capacity(capacity),
            full_state_loads: Vec::with_capacity(capacity),
            full_catalog_loads: Vec::with_capacity(capacity),
        }
    }

    fn push_physical(&mut self, delta: PhysicalDelta) {
        self.reads.push(delta.reads);
        self.appends.push(delta.appends);
        self.wal_bytes.push(delta.wal_bytes);
        self.full_state_loads.push(delta.full_state_loads);
        self.full_catalog_loads.push(delta.full_catalog_loads);
    }
}

struct ResultRow {
    scale: u64,
    samples: Samples,
}

fn seed_depth_database(path: &Path) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE depth_rows (
            id BIGINT PRIMARY KEY,
            body TEXT NOT NULL
        )",
        &[],
    )?;
    for id in 0..DEPTH_ROWS {
        seed.execute_sql(
            "INSERT INTO depth_rows (id, body) VALUES (?, ?)",
            &[
                ScalarValue::Signed(i64::try_from(id)?),
                ScalarValue::Text("depth-1".to_owned()),
            ],
        )?;
    }
    seed.commit()?;
    Ok(database)
}

fn advance_depth(database: &mut NativeDatabase, depth: u64) -> Result<(), BenchmarkError> {
    let mut batch =
        database.begin_optimistic_delta(i64::try_from(depth)?, DurabilityClass::Memory)?;
    for id in 0..DEPTH_ROWS {
        database.stage_delta_sql_dml(
            &mut batch,
            "UPDATE depth_rows SET body = ? WHERE id = ?",
            &[
                ScalarValue::Text(format!("depth-{depth}")),
                ScalarValue::Signed(i64::try_from(id)?),
            ],
        )?;
    }
    database.commit_optimistic(batch)?;
    Ok(())
}

fn measure_depth(database: &mut NativeDatabase, depth: u64) -> Result<Samples, BenchmarkError> {
    let mut samples = Samples::with_capacity(usize::try_from(DEPTH_ROWS)?);
    for id in 0..DEPTH_ROWS {
        let before = database.physical_observation()?;
        let total_started = Instant::now();
        let started = Instant::now();
        let mut batch =
            database.begin_optimistic_delta(i64::try_from(depth + 1)?, DurabilityClass::Memory)?;
        samples.begin.push(nanos(started.elapsed())?);
        let started = Instant::now();
        database.stage_delta_sql_dml(
            &mut batch,
            "UPDATE depth_rows SET body = ? WHERE id = ?",
            &[
                ScalarValue::Text(format!("measured-{depth}-{id}")),
                ScalarValue::Signed(i64::try_from(id)?),
            ],
        )?;
        samples.sql_stage.push(nanos(started.elapsed())?);
        let started = Instant::now();
        database.commit_optimistic(batch)?;
        samples.commit.push(nanos(started.elapsed())?);
        samples.total.push(nanos(total_started.elapsed())?);
        let after = database.physical_observation()?;
        samples.push_physical(physical_delta(before, after)?);
    }
    Ok(samples)
}

fn depth_sweep(path: &Path) -> Result<Vec<ResultRow>, BenchmarkError> {
    let mut database = seed_depth_database(path)?;
    let mut current_depth = 1_u64;
    let mut results = Vec::with_capacity(DEPTHS.len());
    for target in DEPTHS {
        while current_depth < target {
            current_depth += 1;
            advance_depth(&mut database, current_depth)?;
        }
        results.push(ResultRow {
            scale: target,
            samples: measure_depth(&mut database, target)?,
        });
        current_depth += 1;
    }
    Ok(results)
}

fn seed_population_database(
    path: &Path,
    population: u64,
) -> Result<NativeDatabase, BenchmarkError> {
    let mut database = NativeDatabase::create(path)?;
    let index = ObjectId::new(SEARCH_INDEX)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE population_rows (
            id BIGINT PRIMARY KEY,
            body TEXT NOT NULL
        )",
        &[],
    )?;
    seed.create_search_index(index, "population_documents")?;
    seed.execute_sql(
        "INSERT INTO population_rows (id, body) VALUES (0, 'target')",
        &[],
    )?;
    seed.set(b"target-key".to_vec(), b"target".to_vec(), None)?;
    for sequence in 1..=population {
        seed.execute_sql(
            "INSERT INTO population_rows (id, body) VALUES (?, ?)",
            &[
                ScalarValue::Signed(i64::try_from(sequence)?),
                ScalarValue::Text(format!("unrelated row {sequence}")),
            ],
        )?;
        seed.set(
            format!("unrelated-key-{sequence:08}").into_bytes(),
            sequence.to_le_bytes().to_vec(),
            None,
        )?;
        seed.index_document(
            index,
            format!("unrelated-doc-{sequence:08}").into_bytes(),
            format!("unrelated lexical document {sequence}"),
        )?;
    }
    seed.commit()?;
    Ok(database)
}

fn measure_population(
    database: &mut NativeDatabase,
    population: u64,
) -> Result<Samples, BenchmarkError> {
    let capacity = usize::try_from(POPULATION_OBSERVATIONS)?;
    let mut samples = Samples::with_capacity(capacity);
    let index = ObjectId::new(SEARCH_INDEX)?;
    for sequence in 1..=POPULATION_OBSERVATIONS {
        let before = database.physical_observation()?;
        let total_started = Instant::now();
        let started = Instant::now();
        let mut batch = database
            .begin_optimistic_delta(i64::try_from(sequence + 1)?, DurabilityClass::Memory)?;
        samples.begin.push(nanos(started.elapsed())?);

        let started = Instant::now();
        database.stage_delta_sql_dml(
            &mut batch,
            "UPDATE population_rows SET body = ? WHERE id = 0",
            &[ScalarValue::Text(format!(
                "population-{population}-measurement-{sequence}"
            ))],
        )?;
        samples.sql_stage.push(nanos(started.elapsed())?);

        let started = Instant::now();
        database.stage_delta_set(
            &mut batch,
            b"target-key".to_vec(),
            sequence.to_le_bytes().to_vec(),
            None,
        )?;
        samples.structure_stage.push(nanos(started.elapsed())?);

        let started = Instant::now();
        database.stage_delta_index_document(
            &mut batch,
            index,
            format!("measured-{population:08}-{sequence:08}").into_bytes(),
            format!("measured lexical document {population} {sequence}"),
        )?;
        samples.search_stage.push(nanos(started.elapsed())?);

        let started = Instant::now();
        database.commit_optimistic(batch)?;
        samples.commit.push(nanos(started.elapsed())?);
        samples.total.push(nanos(total_started.elapsed())?);
        let after = database.physical_observation()?;
        samples.push_physical(physical_delta(before, after)?);
    }
    Ok(samples)
}

fn population_sweep(root: &Path) -> Result<Vec<ResultRow>, BenchmarkError> {
    let mut results = Vec::with_capacity(POPULATIONS.len());
    for population in POPULATIONS {
        let mut database =
            seed_population_database(&root.join(format!("population-{population}")), population)?;
        results.push(ResultRow {
            scale: population,
            samples: measure_population(&mut database, population)?,
        });
    }
    Ok(results)
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

fn print_rows(
    name: &str,
    scale_name: &str,
    rows: Vec<ResultRow>,
    trailing: bool,
) -> Result<(), BenchmarkError> {
    println!("  \"{name}\": [");
    let length = rows.len();
    for (offset, row) in rows.into_iter().enumerate() {
        println!("    {{");
        println!("      \"{scale_name}\": {},", row.scale);
        println!("      \"observations\": {},", row.samples.commit.len());
        println!("      \"distributions\": {{");
        print_distribution("begin_nanos", row.samples.begin, true)?;
        print_distribution("sql_stage_nanos", row.samples.sql_stage, true)?;
        if !row.samples.structure_stage.is_empty() {
            print_distribution("structure_stage_nanos", row.samples.structure_stage, true)?;
            print_distribution("search_stage_nanos", row.samples.search_stage, true)?;
        }
        print_distribution("commit_nanos", row.samples.commit, true)?;
        print_distribution("total_nanos", row.samples.total, true)?;
        print_distribution("physical_page_reads", row.samples.reads, true)?;
        print_distribution("page_appends", row.samples.appends, true)?;
        print_distribution("wal_bytes_appended", row.samples.wal_bytes, true)?;
        print_distribution("full_state_loads", row.samples.full_state_loads, true)?;
        print_distribution("full_catalog_loads", row.samples.full_catalog_loads, false)?;
        println!("      }}");
        println!("    }}{}", if offset + 1 == length { "" } else { "," });
    }
    println!("  ]{}", if trailing { "," } else { "" });
    Ok(())
}

fn main() -> Result<(), BenchmarkError> {
    let implementation_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let depth = depth_sweep(&temporary.path().join("depth"))?;
    let population = population_sweep(temporary.path())?;
    println!("{{");
    println!("  \"schema\": \"hyphae.native.delta-transaction-scaling.v1\",");
    println!("  \"status\": \"observation-not-regression-gate\",");
    println!("  \"implementation_commit\": \"{implementation_commit}\",");
    println!("  \"profile\": \"release\",");
    println!("  \"durability\": \"memory\",");
    print_rows("version_depth_sweep", "prior_versions", depth, true)?;
    print_rows(
        "unrelated_population_sweep",
        "unrelated_items_per_engine",
        population,
        false,
    )?;
    println!("}}");
    Ok(())
}
