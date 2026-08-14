// SPDX-License-Identifier: AGPL-3.0-only

//! Direct-Linux release observation for bounded native set algebra.

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    NativeDatabase, NativeRuntimeError, SetAlgebraOperation, SetAlgebraRequest, SetAlgebraResult,
};
use hyphae_native_types::DurabilityClass;

const SMALL_A: &[u8] = b"small-a";
const SMALL_B: &[u8] = b"small-b";
const SMALL_C: &[u8] = b"small-c";
const LARGE_A: &[u8] = b"large-a";
const LARGE_B: &[u8] = b"large-b";
const LARGE_SMALL: &[u8] = b"large-small";
const SMALL_OBSERVATIONS: usize = 10_000;
const SMALL_WARMUP: usize = 1_000;
const LARGE_OBSERVATIONS: usize = 200;
const LARGE_WARMUP: usize = 20;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-set-algebra-{}-{timestamp}",
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
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    maximum_nanos: u64,
    throughput_per_second: f64,
}

struct Requests {
    small_union: SetAlgebraRequest,
    small_intersection: SetAlgebraRequest,
    small_difference: SetAlgebraRequest,
    large_union: SetAlgebraRequest,
    large_intersection: SetAlgebraRequest,
    large_difference: SetAlgebraRequest,
}

struct SurfaceMetrics {
    union: Stats,
    intersection: Stats,
    difference: Stats,
}

struct BenchmarkMetrics {
    private: SurfaceMetrics,
    snapshot: SurfaceMetrics,
    physical: SurfaceMetrics,
    large_physical: SurfaceMetrics,
}

fn member(index: u32) -> Vec<u8> {
    index.to_be_bytes().to_vec()
}

fn add_range(
    transaction: &mut hyphae_native_runtime::NativeTransaction<'_>,
    key: &[u8],
    range: impl Iterator<Item = u32>,
    dataset: &mut blake3::Hasher,
) -> Result<(), NativeRuntimeError> {
    for index in range {
        let member = member(index);
        dataset.update(key);
        dataset.update(&member);
        if !transaction.sadd(key.to_vec(), member)? {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(())
}

fn prepare_database(
    path: &Path,
) -> Result<(NativeDatabase, blake3::Hash), Box<dyn std::error::Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    for key in [SMALL_A, SMALL_B, SMALL_C, LARGE_A, LARGE_B, LARGE_SMALL] {
        seed.create_set(key.to_vec())?;
    }
    let mut dataset = blake3::Hasher::new();
    add_range(&mut seed, SMALL_A, 0..32, &mut dataset)?;
    add_range(&mut seed, SMALL_B, 16..48, &mut dataset)?;
    add_range(&mut seed, SMALL_C, (0..64).step_by(2), &mut dataset)?;
    add_range(&mut seed, LARGE_A, 0..4_096, &mut dataset)?;
    add_range(&mut seed, LARGE_B, 2_048..6_144, &mut dataset)?;
    add_range(&mut seed, LARGE_SMALL, 3_000..3_064, &mut dataset)?;
    seed.commit()?;
    drop(database);
    Ok((NativeDatabase::open(path)?, dataset.finalize()))
}

fn request(
    operation: SetAlgebraOperation,
    keys: &[&[u8]],
    output_limit: usize,
    visit_limit: usize,
) -> Result<SetAlgebraRequest, Box<dyn std::error::Error>> {
    Ok(SetAlgebraRequest::try_new(
        operation,
        keys.iter().map(|key| key.to_vec()).collect(),
        output_limit,
        visit_limit,
    )?)
}

fn requests() -> Result<Requests, Box<dyn std::error::Error>> {
    Ok(Requests {
        small_union: request(
            SetAlgebraOperation::Union,
            &[SMALL_A, SMALL_B, SMALL_C],
            128,
            512,
        )?,
        small_intersection: request(
            SetAlgebraOperation::Intersection,
            &[SMALL_A, SMALL_B, SMALL_C],
            128,
            512,
        )?,
        small_difference: request(
            SetAlgebraOperation::Difference,
            &[SMALL_A, SMALL_B, SMALL_C],
            128,
            512,
        )?,
        large_union: request(
            SetAlgebraOperation::Union,
            &[LARGE_A, LARGE_B],
            8_192,
            16_384,
        )?,
        large_intersection: request(
            SetAlgebraOperation::Intersection,
            &[LARGE_A, LARGE_SMALL, LARGE_B],
            128,
            1_024,
        )?,
        large_difference: request(
            SetAlgebraOperation::Difference,
            &[LARGE_A, LARGE_B],
            4_096,
            16_384,
        )?,
    })
}

fn validate_result(
    result: &SetAlgebraResult,
    expected_members: usize,
    expected_visits: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if result.members().len() != expected_members || result.visited() != expected_visits {
        return Err(format!(
            "set algebra result mismatch: members={} visits={}",
            result.members().len(),
            result.visited()
        )
        .into());
    }
    Ok(())
}

fn validate_routes(
    database: &NativeDatabase,
    requests: &Requests,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_result(
        &database.set_algebra_latest_at(&requests.small_union, 101)?,
        56,
        96,
    )?;
    validate_result(
        &database.set_algebra_latest_at(&requests.small_intersection, 101)?,
        8,
        80,
    )?;
    validate_result(
        &database.set_algebra_latest_at(&requests.small_difference, 101)?,
        8,
        80,
    )?;
    validate_result(
        &database.set_algebra_latest_at(&requests.large_union, 101)?,
        6_144,
        8_192,
    )?;
    validate_result(
        &database.set_algebra_latest_at(&requests.large_intersection, 101)?,
        64,
        192,
    )?;
    validate_result(
        &database.set_algebra_latest_at(&requests.large_difference, 101)?,
        2_048,
        8_192,
    )
}

fn percentile(samples: &[u64], per_mille: usize) -> u64 {
    let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
    samples[index]
}

fn measure<T>(
    observations: usize,
    warmup: usize,
    mut operation: impl FnMut() -> Result<T, NativeRuntimeError>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    for _ in 0..warmup {
        black_box(operation()?);
    }
    let mut samples = Vec::with_capacity(observations);
    let total_started = Instant::now();
    for _ in 0..observations {
        let started = Instant::now();
        black_box(operation()?);
        samples.push(u64::try_from(started.elapsed().as_nanos())?);
    }
    let elapsed = total_started.elapsed();
    samples.sort_unstable();
    let completed = u32::try_from(observations)?;
    Ok(Stats {
        p50_nanos: percentile(&samples, 500),
        p95_nanos: percentile(&samples, 950),
        p99_nanos: percentile(&samples, 990),
        p999_nanos: percentile(&samples, 999),
        maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
        throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
    })
}

fn measure_materialized(
    algebra: impl Fn(&SetAlgebraRequest) -> Result<SetAlgebraResult, NativeRuntimeError>,
    requests: &Requests,
) -> Result<SurfaceMetrics, Box<dyn std::error::Error>> {
    Ok(SurfaceMetrics {
        union: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            algebra(black_box(&requests.small_union))
        })?,
        intersection: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            algebra(black_box(&requests.small_intersection))
        })?,
        difference: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            algebra(black_box(&requests.small_difference))
        })?,
    })
}

fn measure_physical(
    database: &NativeDatabase,
    requests: &Requests,
) -> Result<SurfaceMetrics, Box<dyn std::error::Error>> {
    Ok(SurfaceMetrics {
        union: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.small_union), 101)
        })?,
        intersection: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.small_intersection), 101)
        })?,
        difference: measure(SMALL_OBSERVATIONS, SMALL_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.small_difference), 101)
        })?,
    })
}

fn measure_large_physical(
    database: &NativeDatabase,
    requests: &Requests,
) -> Result<SurfaceMetrics, Box<dyn std::error::Error>> {
    Ok(SurfaceMetrics {
        union: measure(LARGE_OBSERVATIONS, LARGE_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.large_union), 101)
        })?,
        intersection: measure(LARGE_OBSERVATIONS, LARGE_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.large_intersection), 101)
        })?,
        difference: measure(LARGE_OBSERVATIONS, LARGE_WARMUP, || {
            database.set_algebra_latest_at(black_box(&requests.large_difference), 101)
        })?,
    })
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_nanos\": {},", stats.p50_nanos);
    println!("      \"p95_nanos\": {},", stats.p95_nanos);
    println!("      \"p99_nanos\": {},", stats.p99_nanos);
    println!("      \"p999_nanos\": {},", stats.p999_nanos);
    println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
    println!(
        "      \"throughput_per_second\": {:.3}",
        stats.throughput_per_second
    );
    println!("    }}{}", if comma { "," } else { "" });
}

fn print_surface(prefix: &str, metrics: &SurfaceMetrics, final_surface: bool) {
    print_stats(&format!("{prefix}_union"), metrics.union, true);
    print_stats(
        &format!("{prefix}_intersection"),
        metrics.intersection,
        true,
    );
    print_stats(
        &format!("{prefix}_difference"),
        metrics.difference,
        !final_surface,
    );
}

fn print_report(
    commit: &str,
    rustc: &str,
    dataset: blake3::Hash,
    tree_height: usize,
    metrics: &BenchmarkMetrics,
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.set-algebra-smoke.v1\",");
    println!("  \"status\": \"observation-not-universal-slo\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!("  \"target\": \"x86_64-linux\",");
    println!("  \"profile\": \"release\",");
    println!("  \"concurrency\": 1,");
    println!("  \"small_observations_per_route\": {SMALL_OBSERVATIONS},");
    println!("  \"small_warmup_per_route\": {SMALL_WARMUP},");
    println!("  \"large_observations_per_route\": {LARGE_OBSERVATIONS},");
    println!("  \"large_warmup_per_route\": {LARGE_WARMUP},");
    println!("  \"dataset_blake3\": \"{}\",", dataset.to_hex());
    println!("  \"structure_tree_height\": {tree_height},");
    println!("  \"route_counters\": {{");
    println!("    \"small_union\": {{\"members\": 56, \"visits\": 96}},");
    println!("    \"small_intersection\": {{\"members\": 8, \"visits\": 80}},");
    println!("    \"small_difference\": {{\"members\": 8, \"visits\": 80}},");
    println!("    \"large_union\": {{\"members\": 6144, \"visits\": 8192}},");
    println!("    \"large_intersection\": {{\"members\": 64, \"visits\": 192}},");
    println!("    \"large_difference\": {{\"members\": 2048, \"visits\": 8192}}");
    println!("  }},");
    println!("  \"metrics\": {{");
    print_surface("small_private", &metrics.private, false);
    print_surface("small_snapshot", &metrics.snapshot, false);
    print_surface("small_physical", &metrics.physical, false);
    print_surface("large_physical", &metrics.large_physical, true);
    println!("  }}");
    println!("}}");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let (mut database, dataset) = prepare_database(temporary.path())?;
    let requests = requests()?;
    validate_routes(&database, &requests)?;

    let private = database.begin(101, DurabilityClass::Memory)?;
    let private_metrics = measure_materialized(|request| private.set_algebra(request), &requests)?;
    drop(private);
    let snapshot = database.snapshot(101)?;
    let snapshot_metrics =
        measure_materialized(|request| snapshot.set_algebra(request), &requests)?;
    let physical_metrics = measure_physical(&database, &requests)?;
    let large_physical_metrics = measure_large_physical(&database, &requests)?;
    let tree_height = database.latest_structure_tree_height()?;
    print_report(
        &commit,
        &rustc,
        dataset,
        tree_height,
        &BenchmarkMetrics {
            private: private_metrics,
            snapshot: snapshot_metrics,
            physical: physical_metrics,
            large_physical: large_physical_metrics,
        },
    );
    Ok(())
}
