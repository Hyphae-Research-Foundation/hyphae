// SPDX-License-Identifier: GPL-3.0-only

//! Reproducible current-root page-vacuum observation.

use std::{
    fs::{self, OpenOptions},
    hint::black_box,
    io::Write,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_ann::HnswConfig;
use hyphae_native_runtime::{AnnSearchOptions, NativeDatabase, Vector, VectorMetric};
use hyphae_native_types::{DurabilityClass, ObjectId};

const ROWS: u32 = 64;
const VERSIONS: u32 = 8;
const VECTORS: u32 = 64;
const POINT_READ_OBSERVATIONS: usize = 10_000;
const PAGE_SIZE: u64 = 16_384;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-page-vacuum-{}-{timestamp}",
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

fn seed(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let mut transaction = database.begin(1, DurabilityClass::Strict)?;
    transaction.create_relation(table, "vacuum_rows")?;
    transaction.create_search_index(search, "vacuum_documents")?;
    transaction.create_vector_index(
        vectors,
        "vacuum_vectors",
        8,
        VectorMetric::Cosine,
        HnswConfig::new(8, 32, 16, 64, 0x4859_5048_4145)?,
    )?;
    for index in 0..ROWS {
        let identity = index.to_be_bytes().to_vec();
        transaction.insert(table, identity.clone(), row_value(0, index))?;
        transaction.set(structure_key(index), row_value(0, index), None)?;
        transaction.index_document(
            search,
            identity,
            format!("native vacuum document needle-{index}"),
        )?;
    }
    transaction.upsert_vectors(
        vectors,
        (0..VECTORS)
            .map(|index| {
                Ok((
                    ObjectId::new(u128::from(index) + 1_000)?,
                    benchmark_vector(index)?,
                ))
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
    )?;
    transaction.commit()?;

    for version in 1..=VERSIONS {
        let mut update = database.begin(i64::from(version) + 1, DurabilityClass::Strict)?;
        for index in 0..ROWS {
            update.update(
                table,
                index.to_be_bytes().to_vec(),
                row_value(version, index),
            )?;
            update.set(structure_key(index), row_value(version, index), None)?;
        }
        update.commit()?;
    }
    Ok(())
}

fn row_value(version: u32, index: u32) -> Vec<u8> {
    format!("version-{version:02}-row-{index:04}").into_bytes()
}

fn structure_key(index: u32) -> Vec<u8> {
    format!("structure-{index:04}").into_bytes()
}

fn benchmark_vector(index: u32) -> Result<Vector, Box<dyn std::error::Error>> {
    let offset = f32::from(u16::try_from(index)?) / f32::from(u16::try_from(VECTORS)?);
    Ok(Vector::new([
        1.0,
        offset,
        offset * offset,
        0.25,
        0.5,
        0.75,
        0.125,
        0.0625,
    ])?)
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

fn point_read_stats(
    database: &NativeDatabase,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    let table = ObjectId::new(1)?;
    let key = (ROWS / 2).to_be_bytes();
    for _ in 0..1_000 {
        black_box(database.select_latest_relational(table, &key)?);
    }
    let mut observations = Vec::with_capacity(POINT_READ_OBSERVATIONS);
    for _ in 0..POINT_READ_OBSERVATIONS {
        let started = Instant::now();
        black_box(database.select_latest_relational(table, &key)?);
        observations.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    observations.sort_unstable();
    Ok((
        percentile(&observations, 50, 100),
        percentile(&observations, 95, 100),
        percentile(&observations, 99, 100),
    ))
}

fn fsync_probe(directory: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let path = directory.join("fsync-probe.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&[0xa5; 4_096])?;
    let started = Instant::now();
    file.sync_data()?;
    let elapsed = u64::try_from(started.elapsed().as_nanos())?;
    drop(file);
    fs::remove_file(path)?;
    Ok(elapsed)
}

fn verify(database: &NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(1)?;
    let search = ObjectId::new(2)?;
    let vectors = ObjectId::new(3)?;
    let middle = ROWS / 2;
    if database.select_latest_relational(table, &middle.to_be_bytes())?
        != Some(row_value(VERSIONS, middle))
        || database.snapshot(100)?.get(&structure_key(middle))
            != Some(row_value(VERSIONS, middle).as_slice())
        || database.match_latest_text(search, "needle-32", 1)?[0].document_id
            != middle.to_be_bytes()
        || database
            .search_ann_latest(
                vectors,
                &benchmark_vector(0)?,
                AnnSearchOptions::new(4, 32, Some(16))?,
            )?
            .hits
            .is_empty()
    {
        return Err("vacuum benchmark state verification failed".into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source_commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let source_tree = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    seed(&mut database)?;
    verify(&database)?;

    let previous_path = temporary.path().join("pages.hydb");
    let bytes_before = fs::metadata(&previous_path)?.len();
    let point_read_before = point_read_stats(&database)?;
    let fsync_probe_nanos = fsync_probe(temporary.path())?;
    let started = Instant::now();
    let receipt = database.vacuum_pages()?;
    let vacuum_nanos = u64::try_from(started.elapsed().as_nanos())?;
    if !receipt.applied {
        return Err("vacuum benchmark did not reclaim a generation".into());
    }
    let active_path = temporary.path().join("pages-00000000000000000002.hydb");
    let bytes_after = fs::metadata(&active_path)?.len();
    verify(&database)?;
    let point_read_after = point_read_stats(&database)?;
    let no_op_started = Instant::now();
    let no_op = database.vacuum_pages()?;
    let no_op_nanos = u64::try_from(no_op_started.elapsed().as_nanos())?;
    if no_op.applied {
        return Err("second vacuum unexpectedly published a generation".into());
    }
    drop(database);
    verify(&NativeDatabase::open(temporary.path())?)?;

    println!("{{");
    println!("  \"schema\": \"hyphae.native.page-vacuum.v1\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"source_tree\": \"{source_tree}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"rows\": {ROWS},");
    println!("  \"versions_per_row\": {},", VERSIONS + 1);
    println!("  \"structure_keys\": {ROWS},");
    println!("  \"search_documents\": {ROWS},");
    println!("  \"ann_vectors\": {VECTORS},");
    println!("  \"vacuum_durability\": \"strict\",");
    println!("  \"vacuum_latency_ns\": {vacuum_nanos},");
    println!("  \"no_op_vacuum_latency_ns\": {no_op_nanos},");
    println!("  \"isolated_file_sync_probe_ns\": {fsync_probe_nanos},");
    println!("  \"point_read_observations\": {POINT_READ_OBSERVATIONS},");
    println!(
        "  \"point_read_before_ns\": {{\"p50\": {}, \"p95\": {}, \"p99\": {}}},",
        point_read_before.0, point_read_before.1, point_read_before.2
    );
    println!(
        "  \"point_read_after_ns\": {{\"p50\": {}, \"p95\": {}, \"p99\": {}}},",
        point_read_after.0, point_read_after.1, point_read_after.2
    );
    println!("  \"page_size_bytes\": {PAGE_SIZE},");
    println!("  \"pages_before\": {},", receipt.previous_page_count);
    println!("  \"pages_after\": {},", receipt.active_page_count);
    println!("  \"pages_reclaimed\": {},", receipt.reclaimed_pages);
    println!("  \"file_bytes_before\": {bytes_before},");
    println!("  \"file_bytes_after\": {bytes_after},");
    println!(
        "  \"file_bytes_reclaimed\": {},",
        bytes_before - bytes_after
    );
    println!("  \"reopen_verified\": true");
    println!("}}");
    Ok(())
}
