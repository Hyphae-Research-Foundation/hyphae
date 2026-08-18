// SPDX-License-Identifier: Apache-2.0

//! Reproducible multilevel native-catalog observation.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_catalog::{CatalogName, QualifiedName};
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::{DurabilityClass, ObjectId};

const INITIAL_OBJECTS: usize = 1_024;
const LOOKUP_OBSERVATIONS: usize = 50_000;
const LOOKUP_WARMUP: usize = 2_000;
const PAGE_SIZE: u64 = 16_384;

struct TemporaryDirectory(PathBuf);

struct Observation {
    source_commit: String,
    source_tree: String,
    rustc: String,
    preparation_nanos: u64,
    bulk_commit_nanos: u64,
    incremental_commit_nanos: u64,
    id_lookup: (u64, u64, u64),
    name_lookup: (u64, u64, u64),
    bytes_after_bulk: u64,
    bytes_after_incremental: u64,
}

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-catalog-btree-{}-{timestamp}",
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

fn relation_name(sequence: usize) -> String {
    format!("catalog_relation_{sequence:05}")
}

fn qualified_relation_name(sequence: usize) -> Result<QualifiedName, Box<dyn std::error::Error>> {
    Ok(QualifiedName::new(
        CatalogName::unquoted("main")?,
        CatalogName::unquoted("public")?,
        CatalogName::unquoted(relation_name(sequence))?,
    ))
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

fn latency_stats(
    mut operation: impl FnMut(usize) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    for observation in 0..LOOKUP_WARMUP {
        operation(observation)?;
    }
    let mut observations = Vec::with_capacity(LOOKUP_OBSERVATIONS);
    for observation in 0..LOOKUP_OBSERVATIONS {
        let started = Instant::now();
        operation(observation)?;
        observations.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    observations.sort_unstable();
    Ok((
        percentile(&observations, 50, 100),
        percentile(&observations, 95, 100),
        percentile(&observations, 99, 100),
    ))
}

fn object_sequence(observation: usize) -> usize {
    observation
        .wrapping_mul(977)
        .wrapping_add(37)
        .rem_euclid(INITIAL_OBJECTS)
        + 1
}

fn verify(
    database: &NativeDatabase,
    names: &[QualifiedName],
    object_count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    for sequence in [1, INITIAL_OBJECTS / 2, object_count] {
        let id = ObjectId::new(u128::try_from(sequence)?)?;
        let object = database
            .catalog_object_latest(id)?
            .ok_or("catalog object lookup returned no object")?;
        if object.header().name.object.lookup() != relation_name(sequence) {
            return Err("catalog object lookup returned the wrong definition".into());
        }
        let name = if sequence <= names.len() {
            &names[sequence - 1]
        } else {
            return Err("catalog verification name is missing".into());
        };
        if database
            .catalog_object_named_latest(name)?
            .is_none_or(|named| named.header().id != id)
        {
            return Err("catalog name lookup returned the wrong definition".into());
        }
    }
    Ok(())
}

fn print_observation(observation: &Observation) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.catalog-btree.v1\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!("  \"source_commit\": \"{}\",", observation.source_commit);
    println!("  \"source_tree\": \"{}\",", observation.source_tree);
    println!("  \"rustc\": \"{}\",", observation.rustc);
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"initial_objects\": {INITIAL_OBJECTS},");
    println!("  \"objects_after_incremental\": {},", INITIAL_OBJECTS + 1);
    println!(
        "  \"bulk_prepare_latency_ns\": {},",
        observation.preparation_nanos
    );
    println!(
        "  \"bulk_strict_commit_latency_ns\": {},",
        observation.bulk_commit_nanos
    );
    println!(
        "  \"incremental_strict_commit_latency_ns\": {},",
        observation.incremental_commit_nanos
    );
    println!("  \"lookup_observations\": {LOOKUP_OBSERVATIONS},");
    println!(
        "  \"id_lookup_ns\": {{\"p50\": {}, \"p95\": {}, \"p99\": {}}},",
        observation.id_lookup.0, observation.id_lookup.1, observation.id_lookup.2
    );
    println!(
        "  \"name_lookup_ns\": {{\"p50\": {}, \"p95\": {}, \"p99\": {}}},",
        observation.name_lookup.0, observation.name_lookup.1, observation.name_lookup.2
    );
    println!("  \"page_size_bytes\": {PAGE_SIZE},");
    println!(
        "  \"page_bytes_after_bulk\": {},",
        observation.bytes_after_bulk
    );
    println!(
        "  \"pages_after_bulk\": {},",
        observation.bytes_after_bulk / PAGE_SIZE
    );
    println!(
        "  \"incremental_page_bytes_appended\": {},",
        observation.bytes_after_incremental - observation.bytes_after_bulk
    );
    println!(
        "  \"incremental_pages_appended\": {},",
        (observation.bytes_after_incremental - observation.bytes_after_bulk) / PAGE_SIZE
    );
    println!("  \"reopen_verified\": true");
    println!("}}");
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

    let preparation_started = Instant::now();
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    let mut names = Vec::with_capacity(INITIAL_OBJECTS + 1);
    for sequence in 1..=INITIAL_OBJECTS {
        let name = relation_name(sequence);
        transaction.create_relation(ObjectId::new(u128::try_from(sequence)?)?, &name)?;
        names.push(qualified_relation_name(sequence)?);
    }
    let preparation_nanos = u64::try_from(preparation_started.elapsed().as_nanos())?;
    let commit_started = Instant::now();
    transaction.commit()?;
    let bulk_commit_nanos = u64::try_from(commit_started.elapsed().as_nanos())?;
    let page_path = temporary.path().join("pages.hydb");
    let bytes_after_bulk = fs::metadata(&page_path)?.len();
    verify(&database, &names, INITIAL_OBJECTS)?;

    let id_lookup = latency_stats(|observation| {
        let sequence = object_sequence(observation);
        let id = ObjectId::new(u128::try_from(sequence)?)?;
        black_box(
            database
                .catalog_object_latest(id)?
                .ok_or("catalog object lookup returned no object")?,
        );
        Ok(())
    })?;
    let name_lookup = latency_stats(|observation| {
        let sequence = object_sequence(observation);
        black_box(
            database
                .catalog_object_named_latest(&names[sequence - 1])?
                .ok_or("catalog name lookup returned no object")?,
        );
        Ok(())
    })?;

    let incremental_sequence = INITIAL_OBJECTS + 1;
    let mut incremental = database.begin(0, DurabilityClass::Strict)?;
    incremental.create_relation(
        ObjectId::new(u128::try_from(incremental_sequence)?)?,
        &relation_name(incremental_sequence),
    )?;
    names.push(qualified_relation_name(incremental_sequence)?);
    let incremental_started = Instant::now();
    incremental.commit()?;
    let incremental_commit_nanos = u64::try_from(incremental_started.elapsed().as_nanos())?;
    let bytes_after_incremental = fs::metadata(&page_path)?.len();
    verify(&database, &names, incremental_sequence)?;

    drop(database);
    let reopened = NativeDatabase::open(temporary.path())?;
    verify(&reopened, &names, incremental_sequence)?;

    print_observation(&Observation {
        source_commit,
        source_tree,
        rustc,
        preparation_nanos,
        bulk_commit_nanos,
        incremental_commit_nanos,
        id_lookup,
        name_lookup,
        bytes_after_bulk,
        bytes_after_incremental,
    });
    Ok(())
}
