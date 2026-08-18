// SPDX-License-Identifier: Apache-2.0

// Exercises the deprecated pre-daemon local session/transport on purpose.
#![allow(deprecated)]

//! Native local prepared SQL codec and session integration tests.

use std::{error::Error, num::NonZeroU64};

#[cfg(unix)]
use hyphae_native_runtime::MAX_LOCAL_PREPARED_STATEMENTS;
use hyphae_native_runtime::{
    LOCAL_SQL_EXECUTE_HEADER_SIZE, LOCAL_SQL_PREPARE_HEADER_SIZE, LOCAL_SQL_PREPARED_RECEIPT_SIZE,
    LocalFailureCode, LocalSqlCodecError, LocalSqlColumn, LocalSqlPreparedReceipt, LocalSqlRows,
    MAX_LOCAL_SQL_COLUMNS, MAX_LOCAL_SQL_PARAMETERS, MAX_LOCAL_SQL_ROWS,
    MAX_LOCAL_SQL_STATEMENT_BYTES, decode_local_failure, decode_local_sql_execute,
    decode_local_sql_prepare, decode_local_sql_prepared_receipt, decode_local_sql_rows,
    encode_local_failure, encode_local_sql_execute, encode_local_sql_prepare,
    encode_local_sql_prepared_receipt, encode_local_sql_rows,
};
use hyphae_native_types::{
    CanonicalF32, CanonicalF64, CatalogVersion, Csn, DecimalType, IntegerWidth, LogicalType,
    ScalarValue,
};

#[test]
fn canonical_prepare_and_receipt_bytes() -> Result<(), Box<dyn Error>> {
    let mut buffer = Vec::new();
    let statement = "SELECT id FROM people WHERE id = ?";
    let encoded = encode_local_sql_prepare(&mut buffer, statement, 256)?;
    let mut expected = vec![1, 1, 0, 0];
    expected.extend_from_slice(
        &u32::try_from(statement.len())
            .map_err(|_| "statement length does not fit u32")?
            .to_le_bytes(),
    );
    expected.extend_from_slice(statement.as_bytes());
    assert_eq!(encoded, expected);
    assert_eq!(decode_local_sql_prepare(encoded)?, statement);

    let receipt = LocalSqlPreparedReceipt {
        plan_id: NonZeroU64::new(7).ok_or("plan ID must be nonzero")?,
        catalog_version: CatalogVersion::new(3)?,
        parameter_count: 1,
        column_count: 1,
        maximum_rows: 1,
    };
    let encoded = encode_local_sql_prepared_receipt(&mut buffer, receipt)?;
    let mut expected = vec![1, 2, 0, 0];
    expected.extend_from_slice(&7_u64.to_le_bytes());
    expected.extend_from_slice(&3_u64.to_le_bytes());
    expected.extend_from_slice(&1_u32.to_le_bytes());
    expected.extend_from_slice(&1_u32.to_le_bytes());
    expected.extend_from_slice(&1_u32.to_le_bytes());
    assert_eq!(encoded, expected);
    assert_eq!(decode_local_sql_prepared_receipt(encoded)?, receipt);

    for (code, byte) in [
        (LocalFailureCode::SqlInvalid, 8),
        (LocalFailureCode::SqlParameters, 9),
        (LocalFailureCode::SqlCatalogChanged, 10),
        (LocalFailureCode::SqlResourceLimit, 11),
        (LocalFailureCode::UnknownPrepared, 12),
    ] {
        assert_eq!(encode_local_failure(&mut buffer, code), [1, byte, 0, 0]);
        assert_eq!(decode_local_failure(&buffer)?, code);
    }
    Ok(())
}

#[test]
fn prepare_codec_enforces_every_boundary() -> Result<(), Box<dyn Error>> {
    let mut buffer = Vec::new();
    let exact_statement = "x".repeat(MAX_LOCAL_SQL_STATEMENT_BYTES);
    let exact_length = LOCAL_SQL_PREPARE_HEADER_SIZE + exact_statement.len();
    let encoded = encode_local_sql_prepare(&mut buffer, &exact_statement, exact_length)?.to_vec();
    assert_eq!(decode_local_sql_prepare(&encoded)?, exact_statement);
    assert!(matches!(
        encode_local_sql_prepare(
            &mut buffer,
            &"x".repeat(MAX_LOCAL_SQL_STATEMENT_BYTES + 1),
            usize::MAX
        ),
        Err(LocalSqlCodecError::StatementTooLarge)
    ));
    assert!(matches!(
        encode_local_sql_prepare(&mut buffer, "", usize::MAX),
        Err(LocalSqlCodecError::EmptyStatement)
    ));
    assert!(matches!(
        encode_local_sql_prepare(&mut buffer, "x", LOCAL_SQL_PREPARE_HEADER_SIZE),
        Err(LocalSqlCodecError::PayloadTooLarge)
    ));

    let canonical = encode_local_sql_prepare(&mut buffer, "x", usize::MAX)?.to_vec();
    for length in 0..LOCAL_SQL_PREPARE_HEADER_SIZE {
        assert!(matches!(
            decode_local_sql_prepare(&canonical[..length]),
            Err(LocalSqlCodecError::Truncated)
        ));
    }
    let mut invalid = canonical.clone();
    invalid[0] = 2;
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::UnsupportedVersion(2))
    ));
    invalid = canonical.clone();
    invalid[1] = 2;
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::UnknownPrepareOpcode(2))
    ));
    invalid = canonical.clone();
    invalid[2] = 1;
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[4..8].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));
    invalid = canonical.clone();
    invalid[4..8].copy_from_slice(&1_u32.to_le_bytes());
    invalid[8] = 0xff;
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::InvalidUtf8)
    ));
    invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_sql_prepare(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn prepared_receipt_codec_enforces_every_boundary() -> Result<(), Box<dyn Error>> {
    let mut buffer = Vec::new();
    let receipt = LocalSqlPreparedReceipt {
        plan_id: NonZeroU64::new(1).ok_or("plan ID must be nonzero")?,
        catalog_version: CatalogVersion::new(1)?,
        parameter_count: u32::try_from(MAX_LOCAL_SQL_PARAMETERS)?,
        column_count: u32::try_from(MAX_LOCAL_SQL_COLUMNS)?,
        maximum_rows: u32::try_from(MAX_LOCAL_SQL_ROWS)?,
    };
    let canonical = encode_local_sql_prepared_receipt(&mut buffer, receipt)?.to_vec();
    assert_eq!(canonical.len(), LOCAL_SQL_PREPARED_RECEIPT_SIZE);
    assert_eq!(decode_local_sql_prepared_receipt(&canonical)?, receipt);
    for length in 0..LOCAL_SQL_PREPARED_RECEIPT_SIZE {
        assert!(matches!(
            decode_local_sql_prepared_receipt(&canonical[..length]),
            Err(LocalSqlCodecError::Truncated)
        ));
    }
    let mut invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_sql_prepared_receipt(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));
    for range in [4..12, 12..20] {
        invalid = canonical.clone();
        invalid[range].fill(0);
        assert!(matches!(
            decode_local_sql_prepared_receipt(&invalid),
            Err(LocalSqlCodecError::InvalidIdentity)
        ));
    }
    invalid = canonical.clone();
    invalid[20..24].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_PARAMETERS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_prepared_receipt(&invalid),
        Err(LocalSqlCodecError::ParameterCountExceeded)
    ));
    invalid = canonical.clone();
    invalid[24..28].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_COLUMNS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_prepared_receipt(&invalid),
        Err(LocalSqlCodecError::ColumnCountExceeded)
    ));
    invalid = canonical.clone();
    invalid[28..32].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_ROWS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_prepared_receipt(&invalid),
        Err(LocalSqlCodecError::RowCountExceeded)
    ));
    Ok(())
}

#[test]
fn execute_codec_round_trips_every_primitive_and_bounds_counts() -> Result<(), Box<dyn Error>> {
    let parameters = vec![
        ScalarValue::Null,
        ScalarValue::Boolean(true),
        ScalarValue::Signed(-2),
        ScalarValue::Unsigned(3),
        ScalarValue::Decimal(-4),
        ScalarValue::Float32(CanonicalF32::new(-1.5)),
        ScalarValue::Float64(CanonicalF64::new(2.5)),
        ScalarValue::Text("λ".to_owned()),
        ScalarValue::Binary(vec![0, 0xff]),
        ScalarValue::Date(-5),
        ScalarValue::Time(42),
        ScalarValue::Timestamp(-6),
        ScalarValue::Interval {
            months: -1,
            days: 2,
            nanoseconds: -3,
        },
        ScalarValue::Uuid([7; 16]),
    ];
    let plan_id = NonZeroU64::new(9).ok_or("plan ID must be nonzero")?;
    let mut buffer = Vec::new();
    let encoded = encode_local_sql_execute(&mut buffer, plan_id, &parameters, usize::MAX)?.to_vec();
    let decoded = decode_local_sql_execute(&encoded)?;
    assert_eq!(decoded.plan_id, plan_id);
    assert_eq!(decoded.parameters, parameters);

    let exact = vec![ScalarValue::Null; MAX_LOCAL_SQL_PARAMETERS];
    assert_eq!(
        decode_local_sql_execute(encode_local_sql_execute(
            &mut buffer,
            plan_id,
            &exact,
            usize::MAX,
        )?)?
        .parameters
        .len(),
        MAX_LOCAL_SQL_PARAMETERS
    );
    assert!(matches!(
        encode_local_sql_execute(
            &mut buffer,
            plan_id,
            &vec![ScalarValue::Null; MAX_LOCAL_SQL_PARAMETERS + 1],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::ParameterCountExceeded)
    ));
    assert!(matches!(
        encode_local_sql_execute(&mut buffer, plan_id, &[], LOCAL_SQL_EXECUTE_HEADER_SIZE - 1,),
        Err(LocalSqlCodecError::PayloadTooLarge)
    ));
    assert_eq!(
        encode_local_sql_execute(&mut buffer, plan_id, &[ScalarValue::Boolean(true)], 25)?.len(),
        25
    );
    assert!(matches!(
        encode_local_sql_execute(&mut buffer, plan_id, &[ScalarValue::Boolean(true)], 24),
        Err(LocalSqlCodecError::PayloadTooLarge)
    ));
    Ok(())
}

#[test]
fn execute_codec_rejects_noncanonical_headers_and_values() -> Result<(), Box<dyn Error>> {
    let plan_id = NonZeroU64::new(9).ok_or("plan ID must be nonzero")?;
    let mut buffer = Vec::new();
    let canonical =
        encode_local_sql_execute(&mut buffer, plan_id, &[ScalarValue::Boolean(true)], 25)?.to_vec();
    for length in 0..LOCAL_SQL_EXECUTE_HEADER_SIZE {
        assert!(matches!(
            decode_local_sql_execute(&canonical[..length]),
            Err(LocalSqlCodecError::Truncated)
        ));
    }
    for length in LOCAL_SQL_EXECUTE_HEADER_SIZE..canonical.len() {
        assert!(decode_local_sql_execute(&canonical[..length]).is_err());
    }
    let mut invalid = canonical.clone();
    invalid[0] = 2;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::UnsupportedVersion(2))
    ));
    invalid = canonical.clone();
    invalid[1] = 2;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::UnknownExecuteOpcode(2))
    ));
    invalid = canonical.clone();
    invalid[3] = 1;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::InvalidIdentity)
    ));
    invalid = canonical.clone();
    invalid[12..16].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_PARAMETERS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::ParameterCountExceeded)
    ));
    invalid = canonical.clone();
    invalid[16] = 0xff;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::UnknownScalarTag(0xff))
    ));
    invalid = canonical.clone();
    invalid[17] = 1;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[24] = 2;
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::InvalidScalar)
    ));
    invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));

    let negative_zero = encode_local_sql_execute(
        &mut buffer,
        plan_id,
        &[ScalarValue::Float64(CanonicalF64::new(1.0))],
        usize::MAX,
    )?
    .to_vec();
    invalid = negative_zero;
    invalid[24..32].copy_from_slice(&(-0.0_f64).to_bits().to_le_bytes());
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::InvalidScalar)
    ));
    let time = encode_local_sql_execute(&mut buffer, plan_id, &[ScalarValue::Time(1)], usize::MAX)?
        .to_vec();
    invalid = time;
    invalid[24..32].copy_from_slice(&86_400_000_000_000_u64.to_le_bytes());
    assert!(matches!(
        decode_local_sql_execute(&invalid),
        Err(LocalSqlCodecError::InvalidScalar)
    ));
    Ok(())
}

#[test]
fn row_codec_carries_schema_nulls_and_every_primitive_type() -> Result<(), Box<dyn Error>> {
    let columns = vec![
        LocalSqlColumn {
            name: "flag".to_owned(),
            logical_type: LogicalType::Boolean,
        },
        LocalSqlColumn {
            name: "signed".to_owned(),
            logical_type: LogicalType::Signed(IntegerWidth::Bits16),
        },
        LocalSqlColumn {
            name: "unsigned".to_owned(),
            logical_type: LogicalType::Unsigned(IntegerWidth::Bits32),
        },
        LocalSqlColumn {
            name: "amount".to_owned(),
            logical_type: LogicalType::Decimal(DecimalType::new(10, 2)?),
        },
        LocalSqlColumn {
            name: "f32".to_owned(),
            logical_type: LogicalType::Float32,
        },
        LocalSqlColumn {
            name: "f64".to_owned(),
            logical_type: LogicalType::Float64,
        },
        LocalSqlColumn {
            name: "text".to_owned(),
            logical_type: LogicalType::Text,
        },
        LocalSqlColumn {
            name: "binary".to_owned(),
            logical_type: LogicalType::Binary,
        },
        LocalSqlColumn {
            name: "date".to_owned(),
            logical_type: LogicalType::Date,
        },
        LocalSqlColumn {
            name: "time".to_owned(),
            logical_type: LogicalType::Time,
        },
        LocalSqlColumn {
            name: "timestamp".to_owned(),
            logical_type: LogicalType::Timestamp,
        },
        LocalSqlColumn {
            name: "interval".to_owned(),
            logical_type: LogicalType::Interval,
        },
        LocalSqlColumn {
            name: "uuid".to_owned(),
            logical_type: LogicalType::Uuid,
        },
    ];
    let present = vec![
        ScalarValue::Boolean(true),
        ScalarValue::Signed(-2),
        ScalarValue::Unsigned(3),
        ScalarValue::Decimal(4),
        ScalarValue::Float32(CanonicalF32::new(1.25)),
        ScalarValue::Float64(CanonicalF64::new(-2.5)),
        ScalarValue::Text("hyphae".to_owned()),
        ScalarValue::Binary(vec![0, 0xff]),
        ScalarValue::Date(-5),
        ScalarValue::Time(42),
        ScalarValue::Timestamp(-6),
        ScalarValue::Interval {
            months: 1,
            days: -2,
            nanoseconds: 3,
        },
        ScalarValue::Uuid([7; 16]),
    ];
    let rows = vec![present, vec![ScalarValue::Null; columns.len()]];
    let expected = LocalSqlRows {
        visible_csn: Csn::new(3)?,
        columns: columns.clone(),
        rows: rows.clone(),
    };
    let mut buffer = Vec::new();
    let encoded = encode_local_sql_rows(
        &mut buffer,
        expected.visible_csn,
        &columns,
        &rows,
        usize::MAX,
    )?;
    assert_eq!(decode_local_sql_rows(encoded)?, expected);

    let golden_columns = [LocalSqlColumn {
        name: "id".to_owned(),
        logical_type: LogicalType::Signed(IntegerWidth::Bits64),
    }];
    assert_eq!(
        encode_local_sql_rows(&mut buffer, Csn::new(3)?, &golden_columns, &[], usize::MAX)?,
        [
            1, 2, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0,
            b'i', b'd', 2, 64,
        ]
    );
    Ok(())
}

#[test]
fn row_codec_enforces_schema_row_and_payload_bounds() -> Result<(), Box<dyn Error>> {
    let mut buffer = Vec::new();
    let columns = [LocalSqlColumn {
        name: "id".to_owned(),
        logical_type: LogicalType::Signed(IntegerWidth::Bits64),
    }];
    let rows = [vec![ScalarValue::Signed(1)]];
    let canonical =
        encode_local_sql_rows(&mut buffer, Csn::new(1)?, &columns, &rows, usize::MAX)?.to_vec();
    assert!(matches!(
        encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &columns,
            &rows,
            canonical.len() - 1,
        ),
        Err(LocalSqlCodecError::PayloadTooLarge)
    ));
    assert!(matches!(
        encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &columns,
            &[Vec::new()],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::RowWidthMismatch)
    ));
    let exact_columns = vec![
        LocalSqlColumn {
            name: "c".to_owned(),
            logical_type: LogicalType::Boolean,
        };
        MAX_LOCAL_SQL_COLUMNS
    ];
    assert_eq!(
        decode_local_sql_rows(encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &exact_columns,
            &[],
            usize::MAX,
        )?)?
        .columns
        .len(),
        MAX_LOCAL_SQL_COLUMNS
    );
    assert!(matches!(
        encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &vec![
                LocalSqlColumn {
                    name: "c".to_owned(),
                    logical_type: LogicalType::Boolean,
                };
                MAX_LOCAL_SQL_COLUMNS + 1
            ],
            &[],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::ColumnCountExceeded)
    ));
    let exact_rows = vec![Vec::new(); MAX_LOCAL_SQL_ROWS];
    assert_eq!(
        decode_local_sql_rows(encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &[],
            &exact_rows,
            usize::MAX,
        )?)?
        .rows
        .len(),
        MAX_LOCAL_SQL_ROWS
    );
    assert!(matches!(
        encode_local_sql_rows(
            &mut buffer,
            Csn::new(1)?,
            &[],
            &vec![Vec::new(); MAX_LOCAL_SQL_ROWS + 1],
            usize::MAX,
        ),
        Err(LocalSqlCodecError::RowCountExceeded)
    ));
    Ok(())
}

#[test]
fn row_codec_rejects_noncanonical_schema_cells_and_lengths() -> Result<(), Box<dyn Error>> {
    let mut buffer = Vec::new();
    let columns = [LocalSqlColumn {
        name: "id".to_owned(),
        logical_type: LogicalType::Signed(IntegerWidth::Bits64),
    }];
    let rows = [vec![ScalarValue::Signed(1)]];
    let canonical =
        encode_local_sql_rows(&mut buffer, Csn::new(1)?, &columns, &rows, usize::MAX)?.to_vec();
    for length in 0..canonical.len() {
        assert!(decode_local_sql_rows(&canonical[..length]).is_err());
    }
    let mut invalid = canonical.clone();
    invalid[0] = 2;
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::UnsupportedVersion(2))
    ));
    invalid = canonical.clone();
    invalid[1] = 3;
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::UnknownValueTag(3))
    ));
    invalid = canonical.clone();
    invalid[2] = 1;
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[4..12].fill(0);
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::InvalidIdentity)
    ));
    invalid = canonical.clone();
    invalid[12..16].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_COLUMNS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::ColumnCountExceeded)
    ));
    invalid = canonical.clone();
    invalid[16..20].copy_from_slice(&u32::try_from(MAX_LOCAL_SQL_ROWS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::RowCountExceeded)
    ));
    invalid = canonical.clone();
    invalid[20..24].copy_from_slice(&1_u32.to_le_bytes());
    invalid[28] = 0xff;
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::InvalidUtf8)
    ));
    invalid = canonical.clone();
    invalid[24..28].fill(0);
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::InvalidTypeDescriptor)
    ));
    invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::LengthMismatch)
    ));

    let null_rows = [vec![ScalarValue::Null]];
    let null = encode_local_sql_rows(&mut buffer, Csn::new(1)?, &columns, &null_rows, usize::MAX)?
        .to_vec();
    invalid = null;
    let cell_offset = 20 + 8 + 2 + 2;
    invalid[cell_offset + 4..cell_offset + 8].copy_from_slice(&1_u32.to_le_bytes());
    invalid.push(0);
    assert!(matches!(
        decode_local_sql_rows(&invalid),
        Err(LocalSqlCodecError::NoncanonicalNull)
    ));
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        num::NonZeroU64,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, LocalFailureCode, LocalSqlPreparedReceipt, LocalSqlRows,
        NativeDatabase, NativeSchedulerClock, SqlError, SqlResult, SqlValue, UdsFrameConnection,
        UdsFrameListener, decode_local_failure, decode_local_sql_prepared_receipt,
        decode_local_sql_rows, encode_local_sql_execute, encode_local_sql_prepare,
    };
    use hyphae_native_types::{CatalogVersion, Csn, DurabilityClass, LogicalType, ScalarValue};

    use super::MAX_LOCAL_PREPARED_STATEMENTS;

    const MAXIMUM_PAYLOAD: usize = 512;
    const SQL_STREAM_ID: u32 = 11;
    const PRIMARY_SQL: &str = "SELECT id, email, active, payload FROM people WHERE id = ?";
    const UNIQUE_SQL: &str = "SELECT id FROM people WHERE email = ?";
    const BOUNDED_SQL: &str = "SELECT id FROM people WHERE active = ? ORDER BY id LIMIT 2";
    const JOIN_SQL: &str = "SELECT people.id, groups.label
                            FROM people
                            INNER JOIN groups ON people.group_id = groups.id
                            WHERE email = ?";

    struct ExpectedSql {
        visible_csn: Csn,
        catalog_version: CatalogVersion,
        primary: SqlResult,
        unique: SqlResult,
        bounded: SqlResult,
        join: SqlResult,
    }

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = Path::new("/tmp").join(format!("hy-sql-{}-{timestamp}", std::process::id()));
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

    struct CountingClock(AtomicUsize);

    impl NativeSchedulerClock for CountingClock {
        fn logical_time_micros(&self) -> i64 {
            self.0.fetch_add(1, Ordering::Relaxed);
            100
        }
    }

    fn receive(
        connection: &mut UdsFrameConnection,
        kind: FrameKind,
        request_id: u64,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let frame = connection.receive()?.ok_or("server closed early")?;
        if frame.kind != kind || frame.stream_id != SQL_STREAM_ID || frame.request_id != request_id
        {
            return Err("response identity diverged".into());
        }
        Ok(frame.payload.to_vec())
    }

    fn prepare(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        statement: &str,
        request_id: u64,
    ) -> Result<LocalSqlPreparedReceipt, Box<dyn std::error::Error>> {
        let request = encode_local_sql_prepare(buffer, statement, MAXIMUM_PAYLOAD)?;
        connection.send(FrameKind::Prepare, SQL_STREAM_ID, request_id, request)?;
        Ok(decode_local_sql_prepared_receipt(&receive(
            connection,
            FrameKind::Receipt,
            request_id,
        )?)?)
    }

    fn execute(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        receipt: LocalSqlPreparedReceipt,
        parameters: &[SqlValue],
        request_id: u64,
    ) -> Result<LocalSqlRows, Box<dyn std::error::Error>> {
        let request =
            encode_local_sql_execute(buffer, receipt.plan_id, parameters, MAXIMUM_PAYLOAD)?;
        connection.send(FrameKind::Execute, SQL_STREAM_ID, request_id, request)?;
        Ok(decode_local_sql_rows(&receive(
            connection,
            FrameKind::Value,
            request_id,
        )?)?)
    }

    fn assert_embedded_equivalence(
        actual: &LocalSqlRows,
        expected: &SqlResult,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let SqlResult::Rows { columns, rows } = expected else {
            return Err("expected an embedded row result".into());
        };
        assert_eq!(
            actual
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            columns.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(&actual.rows, rows);
        Ok(())
    }

    fn seed_database(database: &mut NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
        let mut seed = database.begin_sql(10, DurabilityClass::Strict)?;
        seed.execute_sql(
            "CREATE TABLE people (
                id BIGINT PRIMARY KEY,
                email TEXT NOT NULL,
                active BOOLEAN NOT NULL,
                payload BINARY NOT NULL,
                group_id BIGINT NOT NULL
            )",
            &[],
        )?;
        seed.execute_sql(
            "CREATE TABLE groups (
                id BIGINT PRIMARY KEY,
                label TEXT NOT NULL
            )",
            &[],
        )?;
        seed.execute_sql("INSERT INTO groups (id, label) VALUES (100, 'core')", &[])?;
        for (id, email, active, payload) in [
            (1_i64, "one@hyphae.local", true, b"small".to_vec()),
            (2, "two@hyphae.local", true, vec![b'x'; 700]),
            (3, "three@hyphae.local", false, b"third".to_vec()),
        ] {
            seed.execute_sql(
                "INSERT INTO people (id, email, active, payload, group_id)
                 VALUES (?, ?, ?, ?, ?)",
                &[
                    SqlValue::Signed(id),
                    SqlValue::Text(email.to_owned()),
                    SqlValue::Boolean(active),
                    SqlValue::Binary(payload),
                    SqlValue::Signed(100),
                ],
            )?;
        }
        seed.execute_sql("CREATE UNIQUE INDEX people_email ON people (email)", &[])?;
        seed.execute_sql("CREATE INDEX people_active ON people (active)", &[])?;
        seed.commit()?;
        Ok(())
    }

    fn collect_expected(
        database: &mut NativeDatabase,
    ) -> Result<ExpectedSql, Box<dyn std::error::Error>> {
        let stale = database.prepare_sql_latest(PRIMARY_SQL)?;
        let mut catalog_change = database.begin_sql(11, DurabilityClass::Strict)?;
        catalog_change.execute_sql(
            "CREATE TABLE audit (id BIGINT PRIMARY KEY, note TEXT NOT NULL)",
            &[],
        )?;
        catalog_change.commit()?;
        assert!(matches!(
            database.execute_prepared_latest(&stale, &[SqlValue::Signed(1)]),
            Err(SqlError::CatalogChanged)
        ));

        let primary = database.prepare_sql_latest(PRIMARY_SQL)?;
        let primary_result = database.execute_prepared_latest(&primary, &[SqlValue::Signed(1)])?;
        let visible_csn = database
            .snapshot(0)?
            .visible_csn()
            .ok_or("database has no visible CSN")?;
        let unique = database.prepare_sql_latest(UNIQUE_SQL)?;
        let unique_result = database
            .execute_prepared_latest(&unique, &[SqlValue::Text("one@hyphae.local".to_owned())])?;
        let bounded = database.prepare_sql_latest(BOUNDED_SQL)?;
        let bounded_result =
            database.execute_prepared_latest(&bounded, &[SqlValue::Boolean(true)])?;
        let join = database.prepare_sql_latest(JOIN_SQL)?;
        let join_result = database
            .execute_prepared_latest(&join, &[SqlValue::Text("one@hyphae.local".to_owned())])?;
        Ok(ExpectedSql {
            visible_csn,
            catalog_version: primary.catalog_version(),
            primary: primary_result,
            unique: unique_result,
            bounded: bounded_result,
            join: join_result,
        })
    }

    fn prepare_primary_after_failures(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        catalog_version: CatalogVersion,
    ) -> Result<LocalSqlPreparedReceipt, Box<dyn std::error::Error>> {
        client.send(FrameKind::Prepare, SQL_STREAM_ID, 2, &[1])?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 2)?)?,
            LocalFailureCode::InvalidRequest
        );
        let ddl = encode_local_sql_prepare(
            buffer,
            "CREATE TABLE forbidden (id BIGINT PRIMARY KEY)",
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Prepare, SQL_STREAM_ID, 3, ddl)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 3)?)?,
            LocalFailureCode::SqlInvalid
        );
        let unbounded = encode_local_sql_prepare(
            buffer,
            "SELECT id FROM people WHERE active = ?",
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Prepare, SQL_STREAM_ID, 4, unbounded)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 4)?)?,
            LocalFailureCode::SqlResourceLimit
        );

        let receipt = prepare(client, buffer, PRIMARY_SQL, 5)?;
        assert_eq!(receipt.plan_id.get(), 1);
        assert_eq!(receipt.catalog_version, catalog_version);
        assert_eq!(receipt.parameter_count, 1);
        assert_eq!(receipt.column_count, 4);
        assert_eq!(receipt.maximum_rows, 1);
        Ok(receipt)
    }

    fn assert_execute_failures(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        primary: LocalSqlPreparedReceipt,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let unknown = encode_local_sql_execute(
            buffer,
            NonZeroU64::new(999).ok_or("plan ID must be nonzero")?,
            &[],
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Execute, SQL_STREAM_ID, 6, unknown)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 6)?)?,
            LocalFailureCode::UnknownPrepared
        );
        client.send(FrameKind::Execute, SQL_STREAM_ID, 7, &[1])?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 7)?)?,
            LocalFailureCode::InvalidRequest
        );
        let wrong_count = encode_local_sql_execute(buffer, primary.plan_id, &[], MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Execute, SQL_STREAM_ID, 8, wrong_count)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 8)?)?,
            LocalFailureCode::SqlParameters
        );
        let wrong_type = encode_local_sql_execute(
            buffer,
            primary.plan_id,
            &[SqlValue::Text("one".to_owned())],
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Execute, SQL_STREAM_ID, 9, wrong_type)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 9)?)?,
            LocalFailureCode::SqlParameters
        );
        let oversized = encode_local_sql_execute(
            buffer,
            primary.plan_id,
            &[SqlValue::Signed(2)],
            MAXIMUM_PAYLOAD,
        )?;
        client.send(FrameKind::Execute, SQL_STREAM_ID, 10, oversized)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 10)?)?,
            LocalFailureCode::ResponseTooLarge
        );
        Ok(())
    }

    fn assert_physical_equivalence(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        primary_receipt: LocalSqlPreparedReceipt,
        expected: &ExpectedSql,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let primary_rows = execute(client, buffer, primary_receipt, &[SqlValue::Signed(1)], 11)?;
        assert_eq!(primary_rows.visible_csn, expected.visible_csn);
        assert_eq!(
            primary_rows
                .columns
                .iter()
                .map(|column| column.logical_type.clone())
                .collect::<Vec<_>>(),
            vec![
                LogicalType::Signed(hyphae_native_types::IntegerWidth::Bits64),
                LogicalType::Text,
                LogicalType::Boolean,
                LogicalType::Binary,
            ]
        );
        assert_embedded_equivalence(&primary_rows, &expected.primary)?;

        let unique_receipt = prepare(client, buffer, UNIQUE_SQL, 12)?;
        assert_eq!(unique_receipt.maximum_rows, 1);
        let unique_rows = execute(
            client,
            buffer,
            unique_receipt,
            &[SqlValue::Text("one@hyphae.local".to_owned())],
            13,
        )?;
        assert_embedded_equivalence(&unique_rows, &expected.unique)?;

        let bounded_receipt = prepare(client, buffer, BOUNDED_SQL, 14)?;
        assert_eq!(bounded_receipt.maximum_rows, 2);
        let bounded_rows = execute(
            client,
            buffer,
            bounded_receipt,
            &[SqlValue::Boolean(true)],
            15,
        )?;
        assert_embedded_equivalence(&bounded_rows, &expected.bounded)?;

        let join_receipt = prepare(client, buffer, JOIN_SQL, 16)?;
        assert_eq!(join_receipt.maximum_rows, 1);
        let join_rows = execute(
            client,
            buffer,
            join_receipt,
            &[SqlValue::Text("one@hyphae.local".to_owned())],
            17,
        )?;
        assert_embedded_equivalence(&join_rows, &expected.join)
    }

    fn fill_plan_table_and_reuse_first(
        client: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        primary_receipt: LocalSqlPreparedReceipt,
        expected_primary: &SqlResult,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for plan_count in 5..=MAX_LOCAL_PREPARED_STATEMENTS {
            let request_id = u64::try_from(20 + plan_count)?;
            let receipt = prepare(client, buffer, PRIMARY_SQL, request_id)?;
            assert_eq!(usize::try_from(receipt.plan_id.get())?, plan_count);
        }
        let overflow = encode_local_sql_prepare(buffer, PRIMARY_SQL, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Prepare, SQL_STREAM_ID, 90, overflow)?;
        assert_eq!(
            decode_local_failure(&receive(client, FrameKind::Failure, 90)?)?,
            LocalFailureCode::SqlResourceLimit
        );
        let retained = execute(client, buffer, primary_receipt, &[SqlValue::Signed(1)], 91)?;
        assert_embedded_equivalence(&retained, expected_primary)
    }

    #[test]
    fn uds_sql_matches_physical_plans_recovers_failures_and_reopens()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let mut database = NativeDatabase::create(&data)?;
        seed_database(&mut database)?;
        let expected = collect_expected(&mut database)?;

        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server_clock = Arc::clone(&clock);
        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            LocalDataSession::new(&mut database, server_clock.as_ref()).serve(&mut connection)?;
            listener.close()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let mut client = UdsFrameConnection::connect(&socket, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Hello, 0, 1, b"")?;
        let welcome = client.receive()?.ok_or("server closed during handshake")?;
        assert_eq!(welcome.kind, FrameKind::Welcome);
        assert_eq!(welcome.stream_id, 0);
        assert_eq!(welcome.request_id, 1);
        assert!(welcome.payload.is_empty());

        let mut buffer = Vec::new();
        let primary_receipt =
            prepare_primary_after_failures(&mut client, &mut buffer, expected.catalog_version)?;
        assert_execute_failures(&mut client, &mut buffer, primary_receipt)?;
        assert_physical_equivalence(&mut client, &mut buffer, primary_receipt, &expected)?;
        fill_plan_table_and_reuse_first(
            &mut client,
            &mut buffer,
            primary_receipt,
            &expected.primary,
        )?;

        client.send(FrameKind::Close, 0, 92, b"")?;
        let close = client.receive()?.ok_or("server closed before CLOSE")?;
        assert_eq!(close.kind, FrameKind::Close);
        assert_eq!(close.stream_id, 0);
        assert_eq!(close.request_id, 92);
        server
            .join()
            .map_err(|_| std::io::Error::other("local SQL server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(clock.0.load(Ordering::Relaxed), 0);

        let reopened = NativeDatabase::open(&data)?;
        let reopened_plan = reopened.prepare_sql_latest(PRIMARY_SQL)?;
        assert_eq!(
            reopened.execute_prepared_latest(&reopened_plan, &[ScalarValue::Signed(1)])?,
            expected.primary
        );
        Ok(())
    }
}
