// SPDX-License-Identifier: Apache-2.0

//! Reproducible durable multi-generation snapshot-pin observation.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_ann::HnswConfig;
use hyphae_native_runtime::{
    NativeDatabase, NativeSnapshot, PageGenerationCollectionReceipt, SnapshotPinId, Vector,
    VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

const TABLE: u128 = 1;
const SEARCH: u128 = 2;
const VECTORS: u128 = 3;
const FIRST_VECTOR: u128 = 101;
const SECOND_VECTOR: u128 = 102;
const PIN_COUNT: usize = 3;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-snapshot-pins-{}-{timestamp}",
            std::process::id()
        ))))
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
struct PinObservation {
    id: SnapshotPinId,
    page_generation: u64,
    publish_latency_ns: u64,
    materialize_latency_ns: u64,
}

struct BenchmarkMetadata {
    source_commit: String,
    source_tree: String,
    rustc: String,
    filesystem: String,
}

struct BenchmarkResult {
    observations: [PinObservation; PIN_COUNT],
    vacuum_latencies: [u64; PIN_COUNT],
    reopen_latency_ns: u64,
    final_reopen_latency_ns: u64,
    middle_collection: PageGenerationCollectionReceipt,
    repeated_collection: PageGenerationCollectionReceipt,
    final_collection: PageGenerationCollectionReceipt,
}

fn ann_config() -> Result<HnswConfig, Box<dyn std::error::Error>> {
    Ok(HnswConfig::new(8, 32, 16, 64, 0x4859_5048_4145)?)
}

fn version_value(version: u64) -> Vec<u8> {
    format!("generation-{version}").into_bytes()
}

fn query_vector() -> Result<Vector, Box<dyn std::error::Error>> {
    Ok(Vector::new([1.0, 0.0, 0.0])?)
}

fn version_vectors(version: u64) -> Result<[(ObjectId, Vector); 2], Box<dyn std::error::Error>> {
    let first = ObjectId::new(FIRST_VECTOR)?;
    let second = ObjectId::new(SECOND_VECTOR)?;
    if version % 2 == 1 {
        Ok([
            (first, Vector::new([1.0, 0.0, 0.0])?),
            (second, Vector::new([0.0, 1.0, 0.0])?),
        ])
    } else {
        Ok([
            (first, Vector::new([0.0, 1.0, 0.0])?),
            (second, Vector::new([1.0, 0.0, 0.0])?),
        ])
    }
}

fn expected_best_vector(version: u64) -> Result<ObjectId, Box<dyn std::error::Error>> {
    Ok(ObjectId::new(if version % 2 == 1 {
        FIRST_VECTOR
    } else {
        SECOND_VECTOR
    })?)
}

fn seed(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(TABLE)?;
    let search = ObjectId::new(SEARCH)?;
    let vectors = ObjectId::new(VECTORS)?;
    let mut transaction = database.begin(100, DurabilityClass::Strict)?;
    transaction.create_relation(table, "snapshot_pin_rows")?;
    transaction.insert(table, b"key".to_vec(), version_value(1))?;
    transaction.set(b"version".to_vec(), version_value(1), Some(10_000))?;
    transaction.create_search_index(search, "snapshot_pin_documents")?;
    transaction.index_document(search, b"doc-1".to_vec(), "snapshot generation1")?;
    transaction.create_vector_index(
        vectors,
        "snapshot_pin_vectors",
        3,
        VectorMetric::Cosine,
        ann_config()?,
    )?;
    transaction.upsert_vectors(vectors, version_vectors(1)?)?;
    transaction.commit()?;
    Ok(())
}

fn update_version(
    database: &mut NativeDatabase,
    version: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(TABLE)?;
    let search = ObjectId::new(SEARCH)?;
    let vectors = ObjectId::new(VECTORS)?;
    let mut transaction = database.begin(100 + i64::try_from(version)?, DurabilityClass::Strict)?;
    transaction.update(table, b"key".to_vec(), version_value(version))?;
    transaction.set(b"version".to_vec(), version_value(version), Some(10_000))?;
    transaction.index_document(
        search,
        format!("doc-{version}").into_bytes(),
        format!("snapshot generation{version}"),
    )?;
    transaction.upsert_vectors(vectors, version_vectors(version)?)?;
    transaction.commit()?;
    Ok(())
}

fn verify_snapshot(
    snapshot: &NativeSnapshot,
    version: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(TABLE)?;
    let search = ObjectId::new(SEARCH)?;
    let vectors = ObjectId::new(VECTORS)?;
    if snapshot.select(table, b"key") != Some(version_value(version).as_slice())
        || snapshot.get(b"version") != Some(version_value(version).as_slice())
        || snapshot.match_text(search, &format!("generation{version}"), 1)?[0].document_id
            != format!("doc-{version}").as_bytes()
        || snapshot.search_vector_exact(vectors, &query_vector()?, 2)?[0].object_id
            != expected_best_vector(version)?
    {
        return Err(format!("pinned generation {version} did not match exact state").into());
    }
    if !snapshot
        .match_text(search, &format!("generation{}", version + 1), 1)?
        .is_empty()
    {
        return Err(format!("pinned generation {version} exposed future lexical state").into());
    }
    Ok(())
}

fn verify_current(
    database: &NativeDatabase,
    version: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let table = ObjectId::new(TABLE)?;
    let search = ObjectId::new(SEARCH)?;
    let vectors = ObjectId::new(VECTORS)?;
    if database.select_latest_relational(table, b"key")? != Some(version_value(version))
        || database.get_latest_structure(b"version", 500)? != Some(version_value(version))
        || database.match_latest_text(search, &format!("generation{version}"), 1)?[0].document_id
            != format!("doc-{version}").as_bytes()
        || database.search_vector_exact_latest(vectors, &query_vector()?, 2)?[0].object_id
            != expected_best_vector(version)?
    {
        return Err(format!("active generation {version} did not match exact state").into());
    }
    Ok(())
}

fn publish_pin(
    database: &mut NativeDatabase,
    id: SnapshotPinId,
    logical_time_micros: i64,
) -> Result<PinObservation, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let receipt = database.pin_current(id, logical_time_micros)?;
    Ok(PinObservation {
        id,
        page_generation: receipt.page_generation.get(),
        publish_latency_ns: u64::try_from(started.elapsed().as_nanos())?,
        materialize_latency_ns: 0,
    })
}

fn materialize_pins(
    database: &NativeDatabase,
    observations: &mut [PinObservation; PIN_COUNT],
) -> Result<(), Box<dyn std::error::Error>> {
    for (index, observation) in observations.iter_mut().enumerate() {
        let started = Instant::now();
        let snapshot = database.open_pinned_snapshot(observation.id)?;
        observation.materialize_latency_ns = u64::try_from(started.elapsed().as_nanos())?;
        verify_snapshot(&snapshot, u64::try_from(index)? + 1)?;
    }
    Ok(())
}

fn require_collection(
    receipt: PageGenerationCollectionReceipt,
    removed_files: usize,
    retained_files: usize,
) -> Result<PageGenerationCollectionReceipt, Box<dyn std::error::Error>> {
    if receipt.removed_files != removed_files
        || receipt.retained_files != retained_files
        || (removed_files > 0 && receipt.removed_bytes == 0)
        || receipt.retained_bytes == 0
    {
        return Err("page-generation collection receipt diverged".into());
    }
    Ok(receipt)
}

fn print_pin_observations(observations: &[PinObservation; PIN_COUNT]) {
    println!("  \"pins\": [");
    for (index, observation) in observations.iter().enumerate() {
        let suffix = if index + 1 == observations.len() {
            ""
        } else {
            ","
        };
        println!("    {{");
        println!("      \"id\": \"{}\",", observation.id);
        println!(
            "      \"page_generation\": {},",
            observation.page_generation
        );
        println!(
            "      \"publish_latency_ns\": {},",
            observation.publish_latency_ns
        );
        println!(
            "      \"materialize_latency_ns\": {}",
            observation.materialize_latency_ns
        );
        println!("    }}{suffix}");
    }
    println!("  ],");
}

fn print_collection(name: &str, receipt: PageGenerationCollectionReceipt, suffix: &str) {
    println!("  \"{name}\": {{");
    println!("    \"removed_files\": {},", receipt.removed_files);
    println!("    \"removed_bytes\": {},", receipt.removed_bytes);
    println!("    \"retained_files\": {},", receipt.retained_files);
    println!("    \"retained_bytes\": {},", receipt.retained_bytes);
    println!(
        "    \"parent_directory_sync_supported\": {}",
        receipt.parent_directory_sync_supported
    );
    println!("  }}{suffix}");
}

fn run_benchmark() -> Result<BenchmarkResult, Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    seed(&mut database)?;
    let pins = [
        SnapshotPinId::new(1)?,
        SnapshotPinId::new(2)?,
        SnapshotPinId::new(3)?,
    ];
    let mut observations = [
        publish_pin(&mut database, pins[0], 501)?,
        PinObservation {
            id: pins[1],
            page_generation: 0,
            publish_latency_ns: 0,
            materialize_latency_ns: 0,
        },
        PinObservation {
            id: pins[2],
            page_generation: 0,
            publish_latency_ns: 0,
            materialize_latency_ns: 0,
        },
    ];
    let mut vacuum_latencies = [0_u64; PIN_COUNT];
    for version in 2..=4 {
        update_version(&mut database, version)?;
        let started = Instant::now();
        let vacuum = database.vacuum_pages()?;
        vacuum_latencies[usize::try_from(version - 2)?] =
            u64::try_from(started.elapsed().as_nanos())?;
        if !vacuum.applied || vacuum.active_generation.get() != version {
            return Err(format!("page generation {version} was not published").into());
        }
        if version <= 3 {
            observations[usize::try_from(version - 1)?] = publish_pin(
                &mut database,
                pins[usize::try_from(version - 1)?],
                500 + i64::try_from(version)?,
            )?;
        }
    }
    verify_current(&database, 4)?;
    drop(database);

    let reopen_started = Instant::now();
    let mut reopened = NativeDatabase::open(temporary.path())?;
    let reopen_latency_ns = u64::try_from(reopen_started.elapsed().as_nanos())?;
    if reopened.snapshot_pin_count() != PIN_COUNT {
        return Err("reopen did not recover all snapshot pins".into());
    }
    materialize_pins(&reopened, &mut observations)?;
    verify_current(&reopened, 4)?;

    reopened.unpin(pins[1])?;
    let middle_collection = require_collection(reopened.collect_retired_page_generations()?, 1, 3)?;
    let repeated_collection =
        require_collection(reopened.collect_retired_page_generations()?, 0, 3)?;
    reopened.unpin(pins[0])?;
    reopened.unpin(pins[2])?;
    let final_collection = require_collection(reopened.collect_retired_page_generations()?, 2, 1)?;
    drop(reopened);
    let final_reopen_started = Instant::now();
    let final_reopen = NativeDatabase::open(temporary.path())?;
    let final_reopen_latency_ns = u64::try_from(final_reopen_started.elapsed().as_nanos())?;
    verify_current(&final_reopen, 4)?;
    if final_reopen.snapshot_pin_count() != 0 {
        return Err("final reopen retained a removed snapshot pin".into());
    }

    Ok(BenchmarkResult {
        observations,
        vacuum_latencies,
        reopen_latency_ns,
        final_reopen_latency_ns,
        middle_collection,
        repeated_collection,
        final_collection,
    })
}

fn print_receipt(metadata: &BenchmarkMetadata, result: &BenchmarkResult) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.snapshot-pins.v1\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!("  \"source_commit\": \"{}\",", metadata.source_commit);
    println!("  \"source_tree\": \"{}\",", metadata.source_tree);
    println!("  \"rustc\": \"{}\",", metadata.rustc);
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"filesystem\": \"{}\",", metadata.filesystem);
    println!("  \"concurrency\": 1,");
    println!("  \"pinned_generations\": {PIN_COUNT},");
    println!("  \"active_generation\": 4,");
    println!("  \"reopen_latency_ns\": {},", result.reopen_latency_ns);
    println!(
        "  \"final_reopen_latency_ns\": {},",
        result.final_reopen_latency_ns
    );
    println!(
        "  \"vacuum_latency_ns\": [{}, {}, {}],",
        result.vacuum_latencies[0], result.vacuum_latencies[1], result.vacuum_latencies[2]
    );
    print_pin_observations(&result.observations);
    print_collection("middle_unpin_collection", result.middle_collection, ",");
    print_collection("repeated_collection", result.repeated_collection, ",");
    print_collection("final_unpin_collection", result.final_collection, "");
    println!("}}");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let metadata = BenchmarkMetadata {
        source_commit: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        source_tree: std::env::args()
            .nth(2)
            .unwrap_or_else(|| "dirty-uncommitted".to_owned()),
        rustc: std::env::args()
            .nth(3)
            .unwrap_or_else(|| "unknown".to_owned()),
        filesystem: std::env::args()
            .nth(4)
            .unwrap_or_else(|| "unknown".to_owned()),
    };
    let result = run_benchmark()?;
    print_receipt(&metadata, &result);
    Ok(())
}
