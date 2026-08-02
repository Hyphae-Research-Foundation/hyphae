// SPDX-License-Identifier: Apache-2.0

//! Deterministic warm-path observation for the first native indexed inner join.

use std::{
    error::Error,
    fmt::Write as _,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, SqlResult, SqlValue};
use hyphae_native_types::DurabilityClass;

const ROWS: u32 = 2_048;
const OBSERVATIONS: u32 = 100_000;
const WARMUP: u32 = 10_000;
const P50_TARGET_MICROS: f64 = 75.0;
const P99_TARGET_MICROS: f64 = 400.0;
const QUERY: &str = "SELECT users.id, users.payload, profiles.city
                     FROM users
                     INNER JOIN profiles ON users.profile_id = profiles.id
                     WHERE email = ?";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-indexed-join-smoke-{}-{timestamp}",
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

struct LatencySummary {
    p50: f64,
    p95: f64,
    p99: f64,
    p999: f64,
    maximum: f64,
    throughput_per_second: f64,
}

struct Receipt<'receipt> {
    source_commit: &'receipt str,
    environment: &'receipt str,
    dataset_digest: blake3::Hash,
    commit_csn: u64,
    data_directory_bytes: u64,
    strict_commit_duration: Duration,
    reopen_duration: Duration,
    snapshot_materialization_duration: Duration,
    physical: LatencySummary,
    materialized: LatencySummary,
}

fn main() -> Result<(), Box<dyn Error>> {
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("indexed_join_smoke requires the exact source commit")?;
    let environment = std::env::args()
        .nth(2)
        .ok_or("indexed_join_smoke requires a disclosed environment label")?;
    let temporary = TemporaryDirectory::create()?;
    let (dataset_digest, strict_commit_duration, commit_csn) = create_dataset(temporary.path())?;

    let reopen_started = Instant::now();
    let database = NativeDatabase::open(temporary.path())?;
    let reopen_duration = reopen_started.elapsed();
    let snapshot_started = Instant::now();
    let snapshot = database.snapshot(2)?;
    let snapshot_materialization_duration = snapshot_started.elapsed();
    let physical_plan = database.prepare_sql_latest(QUERY)?;
    let materialized_plan = snapshot.prepare_sql(QUERY)?;
    let parameters = query_parameters();
    validate_result(
        database
            .execute_prepared_latest(&physical_plan, &parameters[usize::try_from(ROWS - 1)?])?,
    )?;
    validate_result(
        snapshot.execute_prepared(&materialized_plan, &parameters[usize::try_from(ROWS - 1)?])?,
    )?;

    for observation in 0..WARMUP {
        let parameter = &parameters[usize::try_from(observation % ROWS)?];
        black_box(database.execute_prepared_latest(&physical_plan, black_box(parameter))?);
        black_box(snapshot.execute_prepared(&materialized_plan, black_box(parameter))?);
    }
    let physical = measure_latency(OBSERVATIONS, |observation| {
        let parameter = &parameters[usize::try_from(observation % ROWS)?];
        black_box(database.execute_prepared_latest(&physical_plan, black_box(parameter))?);
        Ok(())
    })?;
    let materialized = measure_latency(OBSERVATIONS, |observation| {
        let parameter = &parameters[usize::try_from(observation % ROWS)?];
        black_box(snapshot.execute_prepared(&materialized_plan, black_box(parameter))?);
        Ok(())
    })?;
    print_receipt(&Receipt {
        source_commit: &source_commit,
        environment: &environment,
        dataset_digest,
        commit_csn,
        data_directory_bytes: directory_bytes(temporary.path())?,
        strict_commit_duration,
        reopen_duration,
        snapshot_materialization_duration,
        physical,
        materialized,
    })?;
    Ok(())
}

fn create_dataset(path: &Path) -> Result<(blake3::Hash, Duration, u64), Box<dyn Error>> {
    let mut database = NativeDatabase::create(path)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Strict)?;
    transaction.execute_sql(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            profile_id BIGINT NOT NULL,
            payload BINARY NOT NULL
        )",
        &[],
    )?;
    transaction.execute_sql(
        "CREATE TABLE profiles (
            id BIGINT PRIMARY KEY,
            city TEXT NOT NULL
        )",
        &[],
    )?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-indexed-inner-join-corpus-v1");
    for row in 0..ROWS {
        let id = i64::from(row) + 1;
        let profile_id = id + 10_000;
        let email = format!("person-{row:04}@hyphae.local");
        let city = format!("city-{row:04}");
        let payload = deterministic_payload(row);
        transaction.execute_sql(
            "INSERT INTO users (id, email, profile_id, payload) VALUES (?, ?, ?, ?)",
            &[
                SqlValue::Signed(id),
                SqlValue::Text(email.clone()),
                SqlValue::Signed(profile_id),
                SqlValue::Binary(payload.clone()),
            ],
        )?;
        transaction.execute_sql(
            "INSERT INTO profiles (id, city) VALUES (?, ?)",
            &[SqlValue::Signed(profile_id), SqlValue::Text(city.clone())],
        )?;
        hasher.update(&id.to_le_bytes());
        hasher.update(email.as_bytes());
        hasher.update(&profile_id.to_le_bytes());
        hasher.update(&payload);
        hasher.update(city.as_bytes());
    }
    transaction.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
    let commit_started = Instant::now();
    let outcome = transaction.commit()?;
    Ok((
        hasher.finalize(),
        commit_started.elapsed(),
        outcome.commit_csn.get(),
    ))
}

fn deterministic_payload(seed: u32) -> Vec<u8> {
    let mut state = u64::from(seed) ^ 0x9e37_79b9_7f4a_7c15;
    (0_u64..64)
        .map(|offset| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state ^= offset * 0xbf58_476d_1ce4_e5b9;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn query_parameters() -> Vec<[SqlValue; 1]> {
    (0..ROWS)
        .map(|row| [SqlValue::Text(format!("person-{row:04}@hyphae.local"))])
        .collect()
}

fn validate_result(result: SqlResult) -> Result<(), Box<dyn Error>> {
    match result {
        SqlResult::Rows { columns, rows }
            if columns == ["users.id", "users.payload", "profiles.city"] && rows.len() == 1 =>
        {
            Ok(())
        }
        _ => Err("indexed join benchmark returned an unexpected result".into()),
    }
}

fn measure_latency(
    observations: u32,
    mut operation: impl FnMut(u32) -> Result<(), Box<dyn Error>>,
) -> Result<LatencySummary, Box<dyn Error>> {
    let mut latencies = Vec::with_capacity(usize::try_from(observations)?);
    let started = Instant::now();
    for observation in 0..observations {
        let sample_started = Instant::now();
        operation(observation)?;
        latencies.push(sample_started.elapsed());
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    Ok(LatencySummary {
        p50: duration_micros(latencies[percentile_index(latencies.len(), 50)]),
        p95: duration_micros(latencies[percentile_index(latencies.len(), 95)]),
        p99: duration_micros(latencies[percentile_index(latencies.len(), 99)]),
        p999: duration_micros(latencies[percentile_index_permille(latencies.len(), 999)]),
        maximum: latencies.last().copied().map_or(0.0, duration_micros),
        throughput_per_second: f64::from(observations) / elapsed.as_secs_f64(),
    })
}

fn print_receipt(receipt: &Receipt<'_>) -> Result<(), Box<dyn Error>> {
    println!("{{");
    println!("  \"schema\": \"hyphae-native-indexed-inner-join-smoke-v1\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!(
        "  \"source_commit\": {},",
        json_string(receipt.source_commit)?
    );
    println!("  \"environment\": {},", json_string(receipt.environment)?);
    println!("  \"profile\": \"release\",");
    println!("  \"durability\": \"strict\",");
    println!("  \"warm_state\": true,");
    println!("  \"concurrency\": 1,");
    println!("  \"rows_per_relation\": {ROWS},");
    println!("  \"observations_per_route\": {OBSERVATIONS},");
    println!("  \"warmup_per_route\": {WARMUP},");
    println!("  \"commit_csn\": {},", receipt.commit_csn);
    println!(
        "  \"dataset_digest_blake3\": \"{}\",",
        receipt.dataset_digest
    );
    println!(
        "  \"data_directory_bytes\": {},",
        receipt.data_directory_bytes
    );
    print_duration(
        "strict_commit_duration_millis",
        receipt.strict_commit_duration,
    );
    print_duration("reopen_duration_millis", receipt.reopen_duration);
    print_duration(
        "snapshot_materialization_duration_millis",
        receipt.snapshot_materialization_duration,
    );
    println!("  \"provisional_targets_micros\": {{");
    println!("    \"p50\": {P50_TARGET_MICROS:.3},");
    println!("    \"p99\": {P99_TARGET_MICROS:.3}");
    println!("  }},");
    println!("  \"query\": {},", json_string(QUERY)?);
    println!("  \"routes\": {{");
    print_latency("physical_latest", &receipt.physical, true);
    print_latency("materialized_snapshot", &receipt.materialized, false);
    println!("  }}");
    println!("}}");
    Ok(())
}

fn print_duration(name: &str, duration: Duration) {
    println!("  \"{name}\": {:.3},", duration_micros(duration) / 1_000.0);
}

fn print_latency(name: &str, summary: &LatencySummary, trailing_comma: bool) {
    println!("    \"{name}\": {{");
    println!("      \"p50_us\": {:.3},", summary.p50);
    println!("      \"p95_us\": {:.3},", summary.p95);
    println!("      \"p99_us\": {:.3},", summary.p99);
    println!("      \"p99_9_us\": {:.3},", summary.p999);
    println!("      \"maximum_us\": {:.3},", summary.maximum);
    println!(
        "      \"throughput_ops_s\": {:.3},",
        summary.throughput_per_second
    );
    println!(
        "      \"provisional_p50_met\": {},",
        summary.p50 <= P50_TARGET_MICROS
    );
    println!(
        "      \"provisional_p99_met\": {}",
        summary.p99 <= P99_TARGET_MICROS
    );
    println!("    }}{}", if trailing_comma { "," } else { "" });
}

fn directory_bytes(path: &Path) -> Result<u64, Box<dyn Error>> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or("data-directory byte count overflow")?;
        }
    }
    Ok(total)
}

const fn percentile_index(length: usize, percentile: usize) -> usize {
    length.saturating_sub(1).saturating_mul(percentile) / 100
}

const fn percentile_index_permille(length: usize, permille: usize) -> usize {
    length.saturating_sub(1).saturating_mul(permille) / 1_000
}

fn duration_micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn json_string(value: &str) -> Result<String, std::fmt::Error> {
    let mut encoded = String::with_capacity(value.len().saturating_add(2));
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            character if character.is_control() => {
                write!(encoded, "\\u{:04x}", u32::from(character))?;
            }
            character => encoded.push(character),
        }
    }
    encoded.push('"');
    Ok(encoded)
}
