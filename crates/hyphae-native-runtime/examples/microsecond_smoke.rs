// SPDX-License-Identifier: Apache-2.0

//! One-million-observation warm, concurrency-one native latency smoke.

use std::{
    fs,
    hint::black_box,
    ops::Bound,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    DEFAULT_MAX_FRAME_PAYLOAD, FrameKind, NativeDatabase, NativeSnapshot, NativeTransaction,
    PreparedStatement, SqlResult, SqlValue, decode_frame, encode_frame,
};
use hyphae_native_types::{DurabilityClass, IntegerWidth, LogicalType, ObjectId};

const OBSERVATIONS: u32 = 1_000_000;
const WARMUP: u32 = 100_000;
const OPERATIONS_PER_OBSERVATION: u32 = 32;
const RELATIONAL_SCALE_ROWS: u32 = 2_048;
const RELATIONAL_TARGET_ROW: u32 = RELATIONAL_SCALE_ROWS / 2;
const SECONDARY_SCALE_ROWS: u32 = 2_048;
const SECONDARY_TARGET_ROW: u32 = SECONDARY_SCALE_ROWS / 2;
const SECONDARY_OBSERVATIONS: u32 = 100_000;
const SECONDARY_OPERATIONS_PER_OBSERVATION: u32 = 1;
const SECONDARY_RANGE_OBSERVATIONS: u32 = 10_000;
const SECONDARY_RANGE_OPERATIONS_PER_OBSERVATION: u32 = 1;
const SECONDARY_RANGE_LOWER_ROW: u32 = SECONDARY_TARGET_ROW;
const SECONDARY_RANGE_UPPER_ROW: u32 = SECONDARY_RANGE_LOWER_ROW + 10;
const SCAN_LIMIT: usize = 10;
const SCAN_OBSERVATIONS: u32 = 100_000;
const SCAN_OPERATIONS_PER_OBSERVATION: u32 = 1;
const RANGE_LOWER_ROW: u32 = SECONDARY_TARGET_ROW;
const RANGE_UPPER_ROW: u32 = RANGE_LOWER_ROW + 10;
const PREFIX_ROWS_PER_TENANT: u32 = 1_024;
const PREFIX_TARGET_TENANT: &str = "a";
const PREFIX_LOWER_ROW: u32 = PREFIX_ROWS_PER_TENANT / 2;
const STRUCTURE_SCALE_KEYS: u32 = 2_048;
const STRUCTURE_TARGET_KEY: u32 = STRUCTURE_SCALE_KEYS / 2;
const HASH_SCALE_FIELDS: u32 = 2_048;
const HASH_TARGET_FIELD: u32 = HASH_SCALE_FIELDS / 2;
const HASH_KEY: &[u8] = b"benchmark-hash";
const SET_SCALE_MEMBERS: u32 = 2_048;
const SET_TARGET_MEMBER: u32 = SET_SCALE_MEMBERS / 2;
const SET_KEY: &[u8] = b"benchmark-set";
const SEARCH_SCALE_DOCUMENTS: u32 = 2_048;
const SEARCH_TARGET_DOCUMENT: u32 = SEARCH_SCALE_DOCUMENTS / 2;
const SEARCH_OBSERVATIONS: u32 = 100_000;
const SEARCH_OPERATIONS_PER_OBSERVATION: u32 = 1;
const SEARCH_QUERY: &str = "needle";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-microsecond-smoke-{}-{timestamp}",
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

#[derive(Clone, Copy)]
struct Stats {
    p50_nanos: u64,
    p95_nanos: u64,
    p99_nanos: u64,
    p999_nanos: u64,
    throughput_per_second: f64,
}

struct OperationStats {
    structure: Stats,
    structure_btree: Stats,
    hash: Stats,
    hash_btree: Stats,
    set: Stats,
    set_btree: Stats,
    search_btree: Stats,
    prepared_sql: Stats,
    relational_btree: Stats,
    relational_scan: Stats,
    prepared_sql_scan: Stats,
    relational_range: Stats,
    prepared_sql_range: Stats,
    prepared_sql_residual_range: Stats,
    prepared_sql_prefix: Stats,
    prepared_sql_prefix_range: Stats,
    secondary_btree: Stats,
    secondary_prepared_sql: Stats,
    secondary_range_scan: Stats,
    secondary_range_physical: Stats,
    codec_dispatch: Stats,
}

struct BenchmarkInputs<'a> {
    prepared: &'a PreparedStatement,
    scan_prepared: &'a PreparedStatement,
    range_prepared: &'a PreparedStatement,
    residual_prepared: &'a PreparedStatement,
    prefix_prepared: &'a PreparedStatement,
    prefix_range_prepared: &'a PreparedStatement,
    secondary_prepared: &'a PreparedStatement,
    secondary_range_scan_prepared: &'a PreparedStatement,
    secondary_range_prepared: &'a PreparedStatement,
    table: ObjectId,
    scan_table: ObjectId,
    secondary_index: ObjectId,
    search_index: ObjectId,
    relational_target: &'a [u8],
    secondary_index_key: &'a [u8],
    secondary_parameters: &'a [SqlValue],
    secondary_range_parameters: &'a [SqlValue],
    range_lower: &'a [u8],
    range_upper: &'a [u8],
    range_parameters: &'a [SqlValue],
    residual_parameters: &'a [SqlValue],
    prefix_parameters: &'a [SqlValue],
    prefix_range_parameters: &'a [SqlValue],
    structure_target: &'a [u8],
    hash_target: &'a [u8],
    set_target: &'a [u8],
    frame: &'a [u8],
}

struct RangeBenchmarkInput {
    prepared: PreparedStatement,
    lower: Vec<u8>,
    upper: Vec<u8>,
    parameters: [SqlValue; 2],
}

struct PrefixBenchmarkInput {
    prepared: PreparedStatement,
    parameters: [SqlValue; 1],
    range_prepared: PreparedStatement,
    range_parameters: [SqlValue; 2],
}

struct SecondaryRangeBenchmarkInput {
    scan_prepared: PreparedStatement,
    physical_prepared: PreparedStatement,
    parameters: [SqlValue; 2],
}

struct SecondaryExactBenchmarkInput {
    prepared: PreparedStatement,
    parameters: [SqlValue; 1],
    index_key: Vec<u8>,
}

fn seed_prefix_sql_data(
    transaction: &mut NativeTransaction<'_>,
    dataset_hasher: &mut blake3::Hasher,
) -> Result<(), Box<dyn std::error::Error>> {
    transaction.execute_sql(
        "CREATE TABLE benchmark_ledger (
            tenant TEXT NOT NULL,
            id BIGINT NOT NULL,
            payload BINARY NOT NULL,
            PRIMARY KEY (tenant, id)
        )",
        &[],
    )?;
    for tenant in ["a", "aa"] {
        for row in 0..PREFIX_ROWS_PER_TENANT {
            let payload = vec![u8::try_from(row % 251)?; 96];
            dataset_hasher.update(tenant.as_bytes());
            dataset_hasher.update(&row.to_be_bytes());
            dataset_hasher.update(&payload);
            transaction.execute_sql(
                "INSERT INTO benchmark_ledger (tenant, id, payload) VALUES (?, ?, ?)",
                &[
                    SqlValue::Text(tenant.to_owned()),
                    SqlValue::Signed(i64::from(row)),
                    SqlValue::Binary(payload),
                ],
            )?;
        }
    }
    Ok(())
}

fn seed_scaled_data(
    transaction: &mut NativeTransaction<'_>,
    table: ObjectId,
    search_index: ObjectId,
    dataset_hasher: &mut blake3::Hasher,
) -> Result<(), Box<dyn std::error::Error>> {
    for row in 0..RELATIONAL_SCALE_ROWS {
        let key = row.to_be_bytes();
        let value = vec![u8::try_from(row % 251)?; 96];
        dataset_hasher.update(&key);
        dataset_hasher.update(&value);
        transaction.insert(table, key.to_vec(), value)?;
    }
    for key in 0..STRUCTURE_SCALE_KEYS {
        let key = key.to_be_bytes();
        let value = vec![key[3] % 251; 64];
        dataset_hasher.update(&key);
        dataset_hasher.update(&value);
        transaction.set(key.to_vec(), value, None)?;
    }
    transaction.create_hash(HASH_KEY.to_vec())?;
    for field in 0..HASH_SCALE_FIELDS {
        let field = field.to_be_bytes();
        let value = vec![field[3] % 251; 64];
        dataset_hasher.update(&field);
        dataset_hasher.update(&value);
        transaction.hset(HASH_KEY.to_vec(), field.to_vec(), value)?;
    }
    transaction.create_set(SET_KEY.to_vec())?;
    for member in 0..SET_SCALE_MEMBERS {
        let member = member.to_be_bytes();
        dataset_hasher.update(&member);
        transaction.sadd(SET_KEY.to_vec(), member.to_vec())?;
    }
    for document in 0..SEARCH_SCALE_DOCUMENTS {
        let document_id = document.to_be_bytes();
        let text = if document == SEARCH_TARGET_DOCUMENT {
            format!("needle native search common document{document}")
        } else {
            format!("native search common document{document}")
        };
        dataset_hasher.update(&document_id);
        dataset_hasher.update(text.as_bytes());
        transaction.index_document(search_index, document_id.to_vec(), text)?;
    }
    Ok(())
}

fn seed_secondary_sql_data(
    transaction: &mut NativeTransaction<'_>,
    dataset_hasher: &mut blake3::Hasher,
) -> Result<(ObjectId, ObjectId), Box<dyn std::error::Error>> {
    let created = transaction.execute_sql(
        "CREATE TABLE benchmark_people (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            scan_email TEXT NOT NULL,
            active BOOLEAN NOT NULL,
            payload BINARY NOT NULL
        )",
        &[],
    )?;
    let SqlResult::Command {
        object_id: Some(table),
        ..
    } = created
    else {
        return Err("secondary benchmark table identity was not returned".into());
    };
    for row in 0..SECONDARY_SCALE_ROWS {
        let email = format!("person-{row}!hyphae.local");
        let active = row % 2 == 0;
        let payload = vec![u8::try_from(row % 251)?; 96];
        dataset_hasher.update(&row.to_be_bytes());
        dataset_hasher.update(email.as_bytes());
        dataset_hasher.update(email.as_bytes());
        dataset_hasher.update(&[u8::from(active)]);
        dataset_hasher.update(&payload);
        transaction.execute_sql(
            "INSERT INTO benchmark_people (id, email, scan_email, active, payload)
             VALUES (?, ?, ?, ?, ?)",
            &[
                SqlValue::Signed(i64::from(row)),
                SqlValue::Text(email.clone()),
                SqlValue::Text(email),
                SqlValue::Boolean(active),
                SqlValue::Binary(payload),
            ],
        )?;
    }
    let created = transaction.execute_sql(
        "CREATE UNIQUE INDEX benchmark_people_email ON benchmark_people (email)",
        &[],
    )?;
    let SqlResult::Command {
        object_id: Some(index),
        ..
    } = created
    else {
        return Err("secondary benchmark index identity was not returned".into());
    };
    Ok((table, index))
}

fn warm_operations(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    inputs: &BenchmarkInputs<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        black_box(snapshot.get(black_box(b"session")));
        black_box(database.get_latest_structure(black_box(inputs.structure_target), 101)?);
        black_box(snapshot.hget(black_box(HASH_KEY), black_box(inputs.hash_target))?);
        black_box(database.hget_latest_hash(black_box(HASH_KEY), black_box(inputs.hash_target))?);
        black_box(snapshot.sismember(black_box(SET_KEY), black_box(inputs.set_target))?);
        black_box(database.sismember_latest_set(black_box(SET_KEY), black_box(inputs.set_target))?);
        black_box(database.match_latest_text(
            inputs.search_index,
            black_box(SEARCH_QUERY),
            black_box(1),
        )?);
        black_box(
            snapshot
                .execute_prepared_binary(inputs.prepared, black_box(inputs.relational_target))?,
        );
        black_box(
            database.select_latest_relational(inputs.table, black_box(inputs.relational_target))?,
        );
        black_box(database.scan_latest_relational(
            inputs.scan_table,
            None,
            black_box(SCAN_LIMIT),
        )?);
        black_box(database.execute_prepared_latest(inputs.scan_prepared, &[])?);
        black_box(database.scan_latest_relational_range(
            inputs.scan_table,
            Bound::Included(inputs.range_lower),
            Bound::Excluded(inputs.range_upper),
            black_box(SCAN_LIMIT),
        )?);
        black_box(
            database.execute_prepared_latest(
                inputs.range_prepared,
                black_box(inputs.range_parameters),
            )?,
        );
        black_box(database.execute_prepared_latest(
            inputs.residual_prepared,
            black_box(inputs.residual_parameters),
        )?);
        black_box(database.execute_prepared_latest(
            inputs.prefix_prepared,
            black_box(inputs.prefix_parameters),
        )?);
        black_box(database.execute_prepared_latest(
            inputs.prefix_range_prepared,
            black_box(inputs.prefix_range_parameters),
        )?);
        black_box(database.select_latest_secondary_index(
            inputs.secondary_index,
            black_box(inputs.secondary_index_key),
        )?);
        black_box(database.execute_prepared_latest(
            inputs.secondary_prepared,
            black_box(inputs.secondary_parameters),
        )?);
        black_box(database.execute_prepared_latest(
            inputs.secondary_range_scan_prepared,
            black_box(inputs.secondary_range_parameters),
        )?);
        black_box(database.execute_prepared_latest(
            inputs.secondary_range_prepared,
            black_box(inputs.secondary_range_parameters),
        )?);
        let decoded = decode_frame(black_box(inputs.frame), DEFAULT_MAX_FRAME_PAYLOAD)?;
        black_box(snapshot.get(black_box(decoded.payload)));
    }
    Ok(())
}

fn measure_operations(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    inputs: &BenchmarkInputs<'_>,
) -> Result<OperationStats, Box<dyn std::error::Error>> {
    warm_operations(database, snapshot, inputs)?;
    let (structure, structure_btree, hash, hash_btree, set, set_btree) =
        measure_structure_reads(database, snapshot, inputs);
    let (relational_scan, prepared_sql_scan, relational_range, prepared_sql_range) =
        measure_relational_scans(database, inputs);
    let prepared_sql_residual_range = measure_residual_filter(database, inputs);
    let prepared_sql_prefix = measure_prefix_scan(database, inputs);
    let prepared_sql_prefix_range = measure_prefix_range_scan(database, inputs);
    let (secondary_range_scan, secondary_range_physical) =
        measure_secondary_range(database, inputs);
    Ok(OperationStats {
        structure,
        structure_btree,
        hash,
        hash_btree,
        set,
        set_btree,
        search_btree: measure_counted(
            || {
                black_box(
                    database
                        .match_latest_text(
                            inputs.search_index,
                            black_box(SEARCH_QUERY),
                            black_box(1),
                        )
                        .is_ok(),
                );
            },
            SEARCH_OBSERVATIONS,
            SEARCH_OPERATIONS_PER_OBSERVATION,
        ),
        prepared_sql: measure(|| {
            black_box(
                snapshot
                    .execute_prepared_binary(inputs.prepared, black_box(inputs.relational_target))
                    .is_ok(),
            );
        }),
        relational_btree: measure(|| {
            black_box(
                database
                    .select_latest_relational(inputs.table, black_box(inputs.relational_target))
                    .is_ok(),
            );
        }),
        relational_scan,
        prepared_sql_scan,
        relational_range,
        prepared_sql_range,
        prepared_sql_residual_range,
        prepared_sql_prefix,
        prepared_sql_prefix_range,
        secondary_btree: measure_counted(
            || {
                black_box(
                    database
                        .select_latest_secondary_index(
                            inputs.secondary_index,
                            black_box(inputs.secondary_index_key),
                        )
                        .is_ok(),
                );
            },
            SECONDARY_OBSERVATIONS,
            SECONDARY_OPERATIONS_PER_OBSERVATION,
        ),
        secondary_prepared_sql: measure_counted(
            || {
                black_box(
                    database
                        .execute_prepared_latest(
                            inputs.secondary_prepared,
                            black_box(inputs.secondary_parameters),
                        )
                        .is_ok(),
                );
            },
            SECONDARY_OBSERVATIONS,
            SECONDARY_OPERATIONS_PER_OBSERVATION,
        ),
        secondary_range_scan,
        secondary_range_physical,
        codec_dispatch: measure(|| {
            if let Ok(decoded) = decode_frame(black_box(inputs.frame), DEFAULT_MAX_FRAME_PAYLOAD) {
                black_box(snapshot.get(black_box(decoded.payload)));
            }
        }),
    })
}

fn measure_secondary_range(
    database: &NativeDatabase,
    inputs: &BenchmarkInputs<'_>,
) -> (Stats, Stats) {
    let scan = measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.secondary_range_scan_prepared,
                        black_box(inputs.secondary_range_parameters),
                    )
                    .is_ok(),
            );
        },
        SECONDARY_RANGE_OBSERVATIONS,
        SECONDARY_OPERATIONS_PER_OBSERVATION,
    );
    let physical = measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.secondary_range_prepared,
                        black_box(inputs.secondary_range_parameters),
                    )
                    .is_ok(),
            );
        },
        SECONDARY_RANGE_OBSERVATIONS,
        SECONDARY_OPERATIONS_PER_OBSERVATION,
    );
    (scan, physical)
}

fn measure_structure_reads(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    inputs: &BenchmarkInputs<'_>,
) -> (Stats, Stats, Stats, Stats, Stats, Stats) {
    let structure = measure(|| {
        black_box(snapshot.get(black_box(b"session")));
    });
    let structure_btree = measure(|| {
        black_box(
            database
                .get_latest_structure(black_box(inputs.structure_target), 101)
                .is_ok(),
        );
    });
    let hash = measure(|| {
        black_box(
            snapshot
                .hget(black_box(HASH_KEY), black_box(inputs.hash_target))
                .is_ok(),
        );
    });
    let hash_btree = measure(|| {
        black_box(
            database
                .hget_latest_hash(black_box(HASH_KEY), black_box(inputs.hash_target))
                .is_ok(),
        );
    });
    let set = measure(|| {
        black_box(
            snapshot
                .sismember(black_box(SET_KEY), black_box(inputs.set_target))
                .is_ok(),
        );
    });
    let set_btree = measure(|| {
        black_box(
            database
                .sismember_latest_set(black_box(SET_KEY), black_box(inputs.set_target))
                .is_ok(),
        );
    });
    (structure, structure_btree, hash, hash_btree, set, set_btree)
}

fn measure_relational_scans(
    database: &NativeDatabase,
    inputs: &BenchmarkInputs<'_>,
) -> (Stats, Stats, Stats, Stats) {
    let direct = measure_counted(
        || {
            black_box(
                database
                    .scan_latest_relational(inputs.scan_table, None, black_box(SCAN_LIMIT))
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    );
    let prepared = measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(inputs.scan_prepared, &[])
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    );
    let range_direct = measure_counted(
        || {
            black_box(
                database
                    .scan_latest_relational_range(
                        inputs.scan_table,
                        Bound::Included(inputs.range_lower),
                        Bound::Excluded(inputs.range_upper),
                        black_box(SCAN_LIMIT),
                    )
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    );
    let range_prepared = measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.range_prepared,
                        black_box(inputs.range_parameters),
                    )
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    );
    (direct, prepared, range_direct, range_prepared)
}

fn measure_residual_filter(database: &NativeDatabase, inputs: &BenchmarkInputs<'_>) -> Stats {
    measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.residual_prepared,
                        black_box(inputs.residual_parameters),
                    )
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    )
}

fn measure_prefix_scan(database: &NativeDatabase, inputs: &BenchmarkInputs<'_>) -> Stats {
    measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.prefix_prepared,
                        black_box(inputs.prefix_parameters),
                    )
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    )
}

fn measure_prefix_range_scan(database: &NativeDatabase, inputs: &BenchmarkInputs<'_>) -> Stats {
    measure_counted(
        || {
            black_box(
                database
                    .execute_prepared_latest(
                        inputs.prefix_range_prepared,
                        black_box(inputs.prefix_range_parameters),
                    )
                    .is_ok(),
            );
        },
        SCAN_OBSERVATIONS,
        SCAN_OPERATIONS_PER_OBSERVATION,
    )
}

fn validate_secondary_routes(
    database: &NativeDatabase,
    index: ObjectId,
    index_key: &[u8],
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<(), Box<dyn std::error::Error>> {
    if database
        .select_latest_secondary_index(index, index_key)?
        .len()
        != 1
    {
        return Err("physical secondary benchmark key did not identify one row".into());
    }
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(prepared, parameters)?
    else {
        return Err("physical secondary benchmark SQL did not return rows".into());
    };
    if rows.len() != 1 {
        return Err("physical secondary benchmark SQL did not identify one row".into());
    }
    Ok(())
}

fn validate_scan_routes(
    database: &NativeDatabase,
    table: ObjectId,
    prepared: &PreparedStatement,
) -> Result<(), Box<dyn std::error::Error>> {
    if database
        .scan_latest_relational(table, None, SCAN_LIMIT)?
        .len()
        != SCAN_LIMIT
    {
        return Err("physical relational scan benchmark did not reach its limit".into());
    }
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(prepared, &[])? else {
        return Err("physical relational scan benchmark SQL did not return rows".into());
    };
    if rows.len() != SCAN_LIMIT {
        return Err("physical relational scan benchmark SQL did not reach its limit".into());
    }
    Ok(())
}

fn validate_range_routes(
    database: &NativeDatabase,
    table: ObjectId,
    lower: &[u8],
    upper: &[u8],
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<(), Box<dyn std::error::Error>> {
    if database
        .scan_latest_relational_range(
            table,
            Bound::Included(lower),
            Bound::Excluded(upper),
            SCAN_LIMIT,
        )?
        .len()
        != SCAN_LIMIT
    {
        return Err("physical relational range benchmark did not reach its limit".into());
    }
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(prepared, parameters)?
    else {
        return Err("physical relational range benchmark SQL did not return rows".into());
    };
    if rows.len() != SCAN_LIMIT {
        return Err("physical relational range benchmark SQL did not reach its limit".into());
    }
    Ok(())
}

fn prepare_range_benchmark(
    database: &NativeDatabase,
    table: ObjectId,
) -> Result<RangeBenchmarkInput, Box<dyn std::error::Error>> {
    let prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_people
         WHERE id >= ? AND id < ?
         ORDER BY id
         LIMIT 10",
    )?;
    let parameters = [
        SqlValue::Signed(i64::from(RANGE_LOWER_ROW)),
        SqlValue::Signed(i64::from(RANGE_UPPER_ROW)),
    ];
    let logical_type = LogicalType::Signed(IntegerWidth::Bits64);
    let lower = parameters[0].encode_ordered_component(&logical_type)?;
    let upper = parameters[1].encode_ordered_component(&logical_type)?;
    validate_range_routes(database, table, &lower, &upper, &prepared, &parameters)?;
    Ok(RangeBenchmarkInput {
        prepared,
        lower,
        upper,
        parameters,
    })
}

fn prepare_residual_benchmark(
    database: &NativeDatabase,
) -> Result<(PreparedStatement, [SqlValue; 2]), Box<dyn std::error::Error>> {
    let prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_people
         WHERE id >= ? AND active = ?
         ORDER BY id
         LIMIT 10",
    )?;
    let parameters = [
        SqlValue::Signed(i64::from(RANGE_LOWER_ROW)),
        SqlValue::Boolean(true),
    ];
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(&prepared, &parameters)?
    else {
        return Err("residual benchmark SQL did not return rows".into());
    };
    if rows.len() != SCAN_LIMIT {
        return Err("residual benchmark SQL did not reach its post-filter limit".into());
    }
    Ok((prepared, parameters))
}

fn prepare_secondary_range_benchmark(
    database: &NativeDatabase,
) -> Result<SecondaryRangeBenchmarkInput, Box<dyn std::error::Error>> {
    let scan_prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_people
         WHERE scan_email >= ? AND scan_email < ?
         ORDER BY id
         LIMIT 10",
    )?;
    let physical_prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_people
         WHERE email >= ? AND email < ?
         ORDER BY email
         LIMIT 10",
    )?;
    let parameters = [
        SqlValue::Text(format!("person-{SECONDARY_RANGE_LOWER_ROW}!hyphae.local")),
        SqlValue::Text(format!("person-{SECONDARY_RANGE_UPPER_ROW}!hyphae.local")),
    ];
    let scan = database.execute_prepared_latest(&scan_prepared, &parameters)?;
    let physical = database.execute_prepared_latest(&physical_prepared, &parameters)?;
    let SqlResult::Rows { rows, .. } = &physical else {
        return Err("physical secondary range benchmark did not return rows".into());
    };
    if rows.len() != SCAN_LIMIT || scan != physical {
        return Err("secondary range benchmark routes did not return the same ten rows".into());
    }
    Ok(SecondaryRangeBenchmarkInput {
        scan_prepared,
        physical_prepared,
        parameters,
    })
}

fn prepare_secondary_exact_benchmark(
    database: &NativeDatabase,
    index: ObjectId,
) -> Result<SecondaryExactBenchmarkInput, Box<dyn std::error::Error>> {
    let prepared =
        database.prepare_sql_latest("SELECT id, payload FROM benchmark_people WHERE email = ?")?;
    let parameters = [SqlValue::Text(format!(
        "person-{SECONDARY_TARGET_ROW}!hyphae.local"
    ))];
    let index_key = parameters[0].encode_ordered_component(&LogicalType::Text)?;
    validate_secondary_routes(database, index, &index_key, &prepared, &parameters)?;
    Ok(SecondaryExactBenchmarkInput {
        prepared,
        parameters,
        index_key,
    })
}

fn prepare_prefix_benchmark(
    database: &NativeDatabase,
) -> Result<PrefixBenchmarkInput, Box<dyn std::error::Error>> {
    let prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_ledger
         WHERE tenant = ?
         ORDER BY tenant, id
         LIMIT 10",
    )?;
    let parameters = [SqlValue::Text(PREFIX_TARGET_TENANT.to_owned())];
    validate_prefix_benchmark(database, &prepared, &parameters)?;
    let range_prepared = database.prepare_sql_latest(
        "SELECT id, payload FROM benchmark_ledger
         WHERE id >= ? AND tenant = ?
         ORDER BY tenant, id
         LIMIT 10",
    )?;
    let range_parameters = [
        SqlValue::Signed(i64::from(PREFIX_LOWER_ROW)),
        SqlValue::Text(PREFIX_TARGET_TENANT.to_owned()),
    ];
    validate_prefix_benchmark(database, &range_prepared, &range_parameters)?;
    Ok(PrefixBenchmarkInput {
        prepared,
        parameters,
        range_prepared,
        range_parameters,
    })
}

fn validate_prefix_benchmark(
    database: &NativeDatabase,
    prepared: &PreparedStatement,
    parameters: &[SqlValue],
) -> Result<(), Box<dyn std::error::Error>> {
    let SqlResult::Rows { rows, .. } = database.execute_prepared_latest(prepared, parameters)?
    else {
        return Err("prefix benchmark SQL did not return rows".into());
    };
    if rows.len() != SCAN_LIMIT {
        return Err("prefix benchmark SQL did not reach its post-filter limit".into());
    }
    Ok(())
}

fn validate_multilevel_dataset(
    database: &NativeDatabase,
    search_index: ObjectId,
) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let relational = database.latest_relational_tree_height()?;
    let structure = database.latest_structure_tree_height()?;
    let search = database.latest_search_tree_height()?;
    if relational < 2 || structure < 2 || search < 2 {
        return Err("benchmark dataset did not produce three multilevel B+trees".into());
    }
    if database.scard_latest_set(SET_KEY)? != usize::try_from(SET_SCALE_MEMBERS)?
        || !database.sismember_latest_set(SET_KEY, &SET_TARGET_MEMBER.to_be_bytes())?
    {
        return Err("physical set benchmark corpus is incomplete".into());
    }
    if database.match_latest_text(search_index, SEARCH_QUERY, 1)?[0].document_id
        != SEARCH_TARGET_DOCUMENT.to_be_bytes()
    {
        return Err("physical search benchmark target did not rank first".into());
    }
    Ok((relational, structure, search))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commit = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dirty-uncommitted".to_owned());
    let rustc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "unknown".to_owned());
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let table = ObjectId::new(1)?;
    let index = ObjectId::new(2)?;
    let mut transaction = database.begin(100, DurabilityClass::Memory)?;
    transaction.create_relation(table, "accounts")?;
    transaction.insert(table, b"mario".to_vec(), b"active".to_vec())?;
    transaction.create_search_index(index, "notes")?;
    let mut dataset_hasher = blake3::Hasher::new();
    dataset_hasher.update(b"hyphae-native-microsecond-smoke-v15");
    seed_scaled_data(&mut transaction, table, index, &mut dataset_hasher)?;
    let (secondary_table, secondary_index) =
        seed_secondary_sql_data(&mut transaction, &mut dataset_hasher)?;
    seed_prefix_sql_data(&mut transaction, &mut dataset_hasher)?;
    transaction.set(b"session".to_vec(), vec![7_u8; 64], None)?;
    transaction.commit()?;
    let (relational_tree_height, structure_tree_height, search_tree_height) =
        validate_multilevel_dataset(&database, index)?;
    let snapshot = database.snapshot(101)?;
    let prepared = snapshot.prepare_sql("SELECT row FROM accounts WHERE primary_key = ?")?;
    let scan_prepared = database
        .prepare_sql_latest("SELECT id, payload FROM benchmark_people ORDER BY id LIMIT 10")?;
    let range = prepare_range_benchmark(&database, secondary_table)?;
    let (residual_prepared, residual_parameters) = prepare_residual_benchmark(&database)?;
    let prefix = prepare_prefix_benchmark(&database)?;
    let secondary_range = prepare_secondary_range_benchmark(&database)?;
    let secondary_exact = prepare_secondary_exact_benchmark(&database, secondary_index)?;
    let relational_target = RELATIONAL_TARGET_ROW.to_be_bytes();
    validate_scan_routes(&database, secondary_table, &scan_prepared)?;
    let structure_target = STRUCTURE_TARGET_KEY.to_be_bytes();
    let hash_target = HASH_TARGET_FIELD.to_be_bytes();
    let set_target = SET_TARGET_MEMBER.to_be_bytes();
    let frame = encode_frame(
        FrameKind::Structure,
        1,
        1,
        b"session",
        DEFAULT_MAX_FRAME_PAYLOAD,
    )?;

    let operations = measure_operations(
        &database,
        &snapshot,
        &BenchmarkInputs {
            prepared: &prepared,
            scan_prepared: &scan_prepared,
            range_prepared: &range.prepared,
            residual_prepared: &residual_prepared,
            prefix_prepared: &prefix.prepared,
            prefix_range_prepared: &prefix.range_prepared,
            secondary_prepared: &secondary_exact.prepared,
            secondary_range_scan_prepared: &secondary_range.scan_prepared,
            secondary_range_prepared: &secondary_range.physical_prepared,
            table,
            scan_table: secondary_table,
            secondary_index,
            search_index: index,
            relational_target: &relational_target,
            secondary_index_key: &secondary_exact.index_key,
            secondary_parameters: &secondary_exact.parameters,
            secondary_range_parameters: &secondary_range.parameters,
            range_lower: &range.lower,
            range_upper: &range.upper,
            range_parameters: &range.parameters,
            residual_parameters: &residual_parameters,
            prefix_parameters: &prefix.parameters,
            prefix_range_parameters: &prefix.range_parameters,
            structure_target: &structure_target,
            hash_target: &hash_target,
            set_target: &set_target,
            frame: &frame,
        },
    )?;
    dataset_hasher.update(b"accounts:mario=active;session=64x07");
    let dataset_digest = dataset_hasher.finalize();

    print_report(
        &commit,
        &rustc,
        dataset_digest,
        relational_tree_height,
        structure_tree_height,
        search_tree_height,
        &operations,
    );
    Ok(())
}

fn print_report(
    commit: &str,
    rustc: &str,
    dataset_digest: blake3::Hash,
    relational_tree_height: usize,
    structure_tree_height: usize,
    search_tree_height: usize,
    operations: &OperationStats,
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.microsecond-smoke.v15\",");
    println!("  \"status\": \"observation-not-gate\",");
    println!("  \"commit\": \"{commit}\",");
    println!("  \"rustc\": \"{rustc}\",");
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"profile\": \"release\",");
    println!("  \"observations_per_operation\": {OBSERVATIONS},");
    println!("  \"operations_per_observation\": {OPERATIONS_PER_OBSERVATION},");
    println!("  \"warmup_per_operation\": {WARMUP},");
    println!("  \"search_observations\": {SEARCH_OBSERVATIONS},");
    println!("  \"search_operations_per_observation\": {SEARCH_OPERATIONS_PER_OBSERVATION},");
    println!("  \"secondary_observations\": {SECONDARY_OBSERVATIONS},");
    println!("  \"secondary_operations_per_observation\": {SECONDARY_OPERATIONS_PER_OBSERVATION},");
    println!("  \"secondary_range_observations\": {SECONDARY_RANGE_OBSERVATIONS},");
    println!(
        "  \"secondary_range_operations_per_observation\": \
         {SECONDARY_RANGE_OPERATIONS_PER_OBSERVATION},"
    );
    println!("  \"secondary_range_lower_row_inclusive\": {SECONDARY_RANGE_LOWER_ROW},");
    println!("  \"secondary_range_upper_row_exclusive\": {SECONDARY_RANGE_UPPER_ROW},");
    println!("  \"scan_limit\": {SCAN_LIMIT},");
    println!("  \"range_lower_row_inclusive\": {RANGE_LOWER_ROW},");
    println!("  \"range_upper_row_exclusive\": {RANGE_UPPER_ROW},");
    println!("  \"prefix_rows_per_tenant\": {PREFIX_ROWS_PER_TENANT},");
    println!("  \"prefix_target_tenant\": \"{PREFIX_TARGET_TENANT}\",");
    println!("  \"prefix_lower_row_inclusive\": {PREFIX_LOWER_ROW},");
    println!("  \"scan_observations\": {SCAN_OBSERVATIONS},");
    println!("  \"scan_operations_per_observation\": {SCAN_OPERATIONS_PER_OBSERVATION},");
    println!("  \"concurrency\": 1,");
    println!("  \"durability\": \"memory\",");
    println!("  \"warm_state\": true,");
    println!("  \"proofs\": false,");
    println!(
        "  \"primary_relational_rows\": {},",
        RELATIONAL_SCALE_ROWS.saturating_add(1)
    );
    println!("  \"secondary_index_rows\": {SECONDARY_SCALE_ROWS},");
    println!(
        "  \"primary_key_prefix_rows\": {},",
        PREFIX_ROWS_PER_TENANT.saturating_mul(2)
    );
    println!(
        "  \"relational_rows\": {},",
        RELATIONAL_SCALE_ROWS
            .saturating_add(SECONDARY_SCALE_ROWS)
            .saturating_add(PREFIX_ROWS_PER_TENANT.saturating_mul(2))
            .saturating_add(1)
    );
    println!("  \"relational_tree_height\": {relational_tree_height},");
    println!("  \"structure_keys\": {STRUCTURE_SCALE_KEYS},");
    println!("  \"structure_tree_height\": {structure_tree_height},");
    println!("  \"hash_fields\": {HASH_SCALE_FIELDS},");
    println!("  \"set_members\": {SET_SCALE_MEMBERS},");
    println!("  \"search_documents\": {SEARCH_SCALE_DOCUMENTS},");
    println!("  \"search_tree_height\": {search_tree_height},");
    println!("  \"search_query_document_frequency\": 1,");
    println!("  \"dataset_digest_blake3\": \"{dataset_digest}\",");
    println!("  \"transport_note\": \"codec plus embedded dispatch; no named-pipe transport\",");
    print_operation_stats(operations);
    println!("}}");
}

fn print_operation_stats(operations: &OperationStats) {
    println!("  \"operations\": {{");
    print_stats("embedded_structure_get_64b", operations.structure, true);
    print_stats(
        "buffered_structure_btree_get_64b_multilevel",
        operations.structure_btree,
        true,
    );
    print_stats(
        "embedded_hash_hget_64b_materialized_scaled_snapshot",
        operations.hash,
        true,
    );
    print_stats(
        "buffered_hash_hget_64b_multilevel",
        operations.hash_btree,
        true,
    );
    print_stats(
        "embedded_set_sismember_materialized_scaled_snapshot",
        operations.set,
        true,
    );
    print_stats(
        "buffered_set_sismember_multilevel",
        operations.set_btree,
        true,
    );
    print_stats(
        "buffered_inverted_btree_bm25_match_top1_rare_term",
        operations.search_btree,
        true,
    );
    print_stats(
        "embedded_prepared_sql_pk_materialized_scaled_snapshot",
        operations.prepared_sql,
        true,
    );
    print_stats(
        "buffered_relational_btree_pk_multilevel",
        operations.relational_btree,
        true,
    );
    print_relational_scan_stats(operations);
    print_stats(
        "buffered_relational_btree_secondary_exact_unique_multilevel",
        operations.secondary_btree,
        true,
    );
    print_stats(
        "physical_prepared_sql_secondary_exact_unique_multilevel",
        operations.secondary_prepared_sql,
        true,
    );
    print_stats(
        "physical_prepared_sql_unindexed_text_range_pk_scan_limit10_multilevel",
        operations.secondary_range_scan,
        true,
    );
    print_stats(
        "physical_prepared_sql_secondary_range_limit10_multilevel",
        operations.secondary_range_physical,
        true,
    );
    print_stats(
        "local_frame_decode_plus_structure_dispatch_64b",
        operations.codec_dispatch,
        false,
    );
    println!("  }}");
}

fn print_relational_scan_stats(operations: &OperationStats) {
    print_stats(
        "buffered_relational_btree_pk_scan_limit10_multilevel",
        operations.relational_scan,
        true,
    );
    print_stats(
        "physical_prepared_sql_pk_scan_limit10_multilevel",
        operations.prepared_sql_scan,
        true,
    );
    print_stats(
        "buffered_relational_btree_pk_range_limit10_multilevel",
        operations.relational_range,
        true,
    );
    print_stats(
        "physical_prepared_sql_pk_range_limit10_multilevel",
        operations.prepared_sql_range,
        true,
    );
    print_stats(
        "physical_prepared_sql_pk_range_residual_boolean_limit10_multilevel",
        operations.prepared_sql_residual_range,
        true,
    );
    print_stats(
        "physical_prepared_sql_pk_prefix_limit10_multilevel",
        operations.prepared_sql_prefix,
        true,
    );
    print_stats(
        "physical_prepared_sql_pk_prefix_range_limit10_multilevel",
        operations.prepared_sql_prefix_range,
        true,
    );
}

fn measure(mut operation: impl FnMut()) -> Stats {
    measure_counted(&mut operation, OBSERVATIONS, OPERATIONS_PER_OBSERVATION)
}

fn measure_counted(
    mut operation: impl FnMut(),
    observation_count: u32,
    operations_per_observation: u32,
) -> Stats {
    let mut observations = Vec::with_capacity(usize::try_from(observation_count).unwrap_or(0));
    let aggregate_start = Instant::now();
    for _ in 0..observation_count {
        let start = Instant::now();
        for _ in 0..operations_per_observation {
            operation();
        }
        observations.push(
            u64::try_from(start.elapsed().as_nanos() / u128::from(operations_per_observation))
                .unwrap_or(u64::MAX),
        );
    }
    let aggregate_elapsed = aggregate_start.elapsed();
    observations.sort_unstable();
    Stats {
        p50_nanos: percentile(&observations, 500),
        p95_nanos: percentile(&observations, 950),
        p99_nanos: percentile(&observations, 990),
        p999_nanos: percentile(&observations, 999),
        throughput_per_second: f64::from(observation_count * operations_per_observation)
            / aggregate_elapsed.as_secs_f64(),
    }
}

fn percentile(observations: &[u64], permille: usize) -> u64 {
    let last = observations.len().saturating_sub(1);
    let index = last.saturating_mul(permille).saturating_add(999) / 1000;
    observations.get(index.min(last)).copied().unwrap_or(0)
}

fn micros(nanos: u64) -> f64 {
    Duration::from_nanos(nanos).as_secs_f64() * 1_000_000.0
}

fn print_stats(name: &str, stats: Stats, comma: bool) {
    let suffix = if comma { "," } else { "" };
    println!("    \"{name}\": {{");
    println!("      \"p50_us\": {:.3},", micros(stats.p50_nanos));
    println!("      \"p95_us\": {:.3},", micros(stats.p95_nanos));
    println!("      \"p99_us\": {:.3},", micros(stats.p99_nanos));
    println!("      \"p99_9_us\": {:.3},", micros(stats.p999_nanos));
    println!(
        "      \"throughput_ops_s\": {:.0}",
        stats.throughput_per_second
    );
    println!("    }}{suffix}");
}
