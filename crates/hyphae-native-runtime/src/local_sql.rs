// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroU64;

use hyphae_native_types::{
    CanonicalF32, CanonicalF64, CatalogVersion, Csn, LogicalType, MAX_SCALAR_BYTES, ScalarValue,
};
use thiserror::Error;

use crate::SqlValue;

/// Fixed request header for one local SQL `PREPARE`.
pub const LOCAL_SQL_PREPARE_HEADER_SIZE: usize = 8;
/// Fixed request header for one local SQL `EXECUTE`.
pub const LOCAL_SQL_EXECUTE_HEADER_SIZE: usize = 16;
/// Fixed request header for one transaction-bound SQL DML execution.
pub const LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE: usize = 24;
/// Fixed receipt size for one retained prepared statement.
pub const LOCAL_SQL_PREPARED_RECEIPT_SIZE: usize = 32;
/// Fixed header for one SQL row result.
pub const LOCAL_SQL_ROWS_HEADER_SIZE: usize = 20;
/// Maximum UTF-8 SQL statement accepted by the local session.
pub const MAX_LOCAL_SQL_STATEMENT_BYTES: usize = 65_536;
/// Maximum prepared statements retained by one local session.
pub const MAX_LOCAL_PREPARED_STATEMENTS: usize = 64;
/// Maximum parameters accepted by one local execution.
pub const MAX_LOCAL_SQL_PARAMETERS: usize = 1_024;
/// Maximum columns returned by one local execution.
pub const MAX_LOCAL_SQL_COLUMNS: usize = 1_024;
/// Maximum rows returned by one local execution.
pub const MAX_LOCAL_SQL_ROWS: usize = 1_024;
/// Maximum encoded logical-type descriptor carried by one result column.
pub const MAX_LOCAL_SQL_TYPE_DESCRIPTOR_BYTES: usize = 256;
/// Maximum UTF-8 bytes in one result column name.
pub const MAX_LOCAL_SQL_COLUMN_NAME_BYTES: usize = 4_096;

const SQL_PAYLOAD_VERSION: u8 = 1;
const PREPARE_SELECT_OPCODE: u8 = 1;
const EXECUTE_PREPARED_SELECT_OPCODE: u8 = 1;
pub(crate) const EXECUTE_TRANSACTION_DML_OPCODE: u8 = 2;
const SQL_PREPARED_RECEIPT_TAG: u8 = 2;
const SQL_ROWS_VALUE_TAG: u8 = 2;
const SCALAR_RECORD_HEADER_SIZE: usize = 8;
const COLUMN_RECORD_HEADER_SIZE: usize = 8;
const CELL_RECORD_HEADER_SIZE: usize = 8;
const SCALAR_NULL: u8 = 0;
const SCALAR_BOOLEAN: u8 = 1;
const SCALAR_SIGNED: u8 = 2;
const SCALAR_UNSIGNED: u8 = 3;
const SCALAR_DECIMAL: u8 = 4;
const SCALAR_FLOAT32: u8 = 5;
const SCALAR_FLOAT64: u8 = 6;
const SCALAR_TEXT: u8 = 7;
const SCALAR_BINARY: u8 = 8;
const SCALAR_DATE: u8 = 9;
const SCALAR_TIME: u8 = 10;
const SCALAR_TIMESTAMP: u8 = 11;
const SCALAR_INTERVAL: u8 = 12;
const SCALAR_UUID: u8 = 13;
const CELL_NULL: u8 = 0;
const CELL_PRESENT: u8 = 1;
const NANOS_PER_DAY: u64 = 86_400_000_000_000;

/// Stable metadata returned after retaining one prepared SQL statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSqlPreparedReceipt {
    /// Nonzero identifier scoped to the current local session.
    pub plan_id: NonZeroU64,
    /// Catalog version against which the statement was bound.
    pub catalog_version: CatalogVersion,
    /// Exact number of positional parameters.
    pub parameter_count: u32,
    /// Exact number of result columns.
    pub column_count: u32,
    /// Statically proven result-row upper bound.
    pub maximum_rows: u32,
}

/// One decoded prepared SQL execution request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSqlExecuteRequest {
    /// Nonzero identifier scoped to the current local session.
    pub plan_id: NonZeroU64,
    /// Canonical primitive parameters in placeholder order.
    pub parameters: Vec<SqlValue>,
}

/// One decoded transaction-bound SQL DML request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTransactionSqlDmlRequest<'payload> {
    /// Matching connection-local transaction handle.
    pub handle: NonZeroU64,
    /// One nonempty UTF-8 `INSERT`, `UPDATE`, or `DELETE`.
    pub statement: &'payload str,
    /// Canonical primitive parameters in placeholder order.
    pub parameters: Vec<SqlValue>,
}

/// One logical SQL result column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSqlColumn {
    /// Stable output name selected by the binder.
    pub name: String,
    /// Complete native logical type.
    pub logical_type: LogicalType,
}

/// One complete local SQL row result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSqlRows {
    /// All-engine CSN visible to the execution.
    pub visible_csn: Csn,
    /// Ordered result schema.
    pub columns: Vec<LocalSqlColumn>,
    /// Row-major canonical scalar values.
    pub rows: Vec<Vec<SqlValue>>,
}

/// Canonical local SQL payload failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalSqlCodecError {
    /// A fixed header or declared record is incomplete.
    #[error("native local SQL payload is truncated")]
    Truncated,
    /// The payload version is unsupported.
    #[error("native local SQL payload version {0} is unsupported")]
    UnsupportedVersion(u8),
    /// Reserved payload bytes are nonzero.
    #[error("native local SQL payload reserved bytes are nonzero")]
    ReservedBytes,
    /// The PREPARE opcode is unknown.
    #[error("native local SQL PREPARE opcode {0} is unknown")]
    UnknownPrepareOpcode(u8),
    /// The EXECUTE opcode is unknown.
    #[error("native local SQL EXECUTE opcode {0} is unknown")]
    UnknownExecuteOpcode(u8),
    /// The receipt tag is unknown.
    #[error("native local SQL receipt tag {0} is unknown")]
    UnknownReceiptTag(u8),
    /// The value tag is unknown.
    #[error("native local SQL value tag {0} is unknown")]
    UnknownValueTag(u8),
    /// The scalar tag is unknown.
    #[error("native local SQL scalar tag {0} is unknown")]
    UnknownScalarTag(u8),
    /// SQL text is empty.
    #[error("native local SQL statement is empty")]
    EmptyStatement,
    /// SQL text exceeds its canonical bound.
    #[error("native local SQL statement exceeds its canonical bound")]
    StatementTooLarge,
    /// SQL or a result column name is not valid UTF-8.
    #[error("native local SQL text is invalid UTF-8")]
    InvalidUtf8,
    /// A declared and physical payload length diverges.
    #[error("native local SQL payload length mismatch")]
    LengthMismatch,
    /// A plan, catalog, or visible-CSN identity is zero.
    #[error("native local SQL identity is invalid")]
    InvalidIdentity,
    /// The parameter count exceeds the local bound.
    #[error("native local SQL parameter count exceeds its bound")]
    ParameterCountExceeded,
    /// The result column count exceeds the local bound.
    #[error("native local SQL column count exceeds its bound")]
    ColumnCountExceeded,
    /// The result row count exceeds the local bound.
    #[error("native local SQL row count exceeds its bound")]
    RowCountExceeded,
    /// One result row does not match the declared schema width.
    #[error("native local SQL row width differs from the schema")]
    RowWidthMismatch,
    /// A scalar is malformed or noncanonical.
    #[error("native local SQL scalar is invalid")]
    InvalidScalar,
    /// A null scalar or cell carries bytes.
    #[error("native local SQL null value is noncanonical")]
    NoncanonicalNull,
    /// A logical-type descriptor is malformed or overlong.
    #[error("native local SQL logical type descriptor is invalid")]
    InvalidTypeDescriptor,
    /// A result column name exceeds its local bound.
    #[error("native local SQL column name exceeds its bound")]
    ColumnNameTooLarge,
    /// The complete encoded payload exceeds the negotiated frame bound.
    #[error("native local SQL payload exceeds the configured frame bound")]
    PayloadTooLarge,
}

/// Encodes one canonical local SQL `PREPARE SELECT` request.
///
/// # Errors
///
/// Returns an error for empty, overlong, or frame-exceeding SQL text.
pub fn encode_local_sql_prepare<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    statement: &str,
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSqlCodecError> {
    validate_statement_length(statement.len())?;
    let encoded_length = LOCAL_SQL_PREPARE_HEADER_SIZE
        .checked_add(statement.len())
        .ok_or(LocalSqlCodecError::PayloadTooLarge)?;
    ensure_payload_bound(encoded_length, maximum_payload)?;
    let statement_length =
        u32::try_from(statement.len()).map_err(|_| LocalSqlCodecError::StatementTooLarge)?;
    buffer.resize(encoded_length, 0);
    buffer.fill(0);
    buffer[0] = SQL_PAYLOAD_VERSION;
    buffer[1] = PREPARE_SELECT_OPCODE;
    buffer[4..8].copy_from_slice(&statement_length.to_le_bytes());
    buffer[LOCAL_SQL_PREPARE_HEADER_SIZE..].copy_from_slice(statement.as_bytes());
    Ok(buffer)
}

/// Decodes one canonical local SQL `PREPARE SELECT` request.
///
/// # Errors
///
/// Returns an error for malformed framing, invalid UTF-8, empty SQL, or an
/// overlong statement.
pub fn decode_local_sql_prepare(payload: &[u8]) -> Result<&str, LocalSqlCodecError> {
    require_header(payload, LOCAL_SQL_PREPARE_HEADER_SIZE)?;
    validate_version_and_reserved(payload)?;
    if payload[1] != PREPARE_SELECT_OPCODE {
        return Err(LocalSqlCodecError::UnknownPrepareOpcode(payload[1]));
    }
    let statement_length = usize_from_u32(read_u32(payload, 4)?)?;
    validate_statement_length(statement_length)?;
    require_exact_remaining(payload, LOCAL_SQL_PREPARE_HEADER_SIZE, statement_length)?;
    std::str::from_utf8(&payload[LOCAL_SQL_PREPARE_HEADER_SIZE..])
        .map_err(|_| LocalSqlCodecError::InvalidUtf8)
}

/// Encodes one fixed canonical prepared-statement receipt.
///
/// # Errors
///
/// Returns an error when reported counts exceed local protocol bounds.
pub fn encode_local_sql_prepared_receipt(
    buffer: &mut Vec<u8>,
    receipt: LocalSqlPreparedReceipt,
) -> Result<&[u8], LocalSqlCodecError> {
    validate_receipt_counts(receipt)?;
    buffer.resize(LOCAL_SQL_PREPARED_RECEIPT_SIZE, 0);
    buffer.fill(0);
    buffer[0] = SQL_PAYLOAD_VERSION;
    buffer[1] = SQL_PREPARED_RECEIPT_TAG;
    buffer[4..12].copy_from_slice(&receipt.plan_id.get().to_le_bytes());
    buffer[12..20].copy_from_slice(&receipt.catalog_version.get().to_le_bytes());
    buffer[20..24].copy_from_slice(&receipt.parameter_count.to_le_bytes());
    buffer[24..28].copy_from_slice(&receipt.column_count.to_le_bytes());
    buffer[28..32].copy_from_slice(&receipt.maximum_rows.to_le_bytes());
    Ok(buffer)
}

/// Decodes one fixed canonical prepared-statement receipt.
///
/// # Errors
///
/// Returns an error for malformed framing, zero identities, or over-limit
/// counts.
pub fn decode_local_sql_prepared_receipt(
    payload: &[u8],
) -> Result<LocalSqlPreparedReceipt, LocalSqlCodecError> {
    require_fixed(payload, LOCAL_SQL_PREPARED_RECEIPT_SIZE)?;
    validate_version_and_reserved(payload)?;
    if payload[1] != SQL_PREPARED_RECEIPT_TAG {
        return Err(LocalSqlCodecError::UnknownReceiptTag(payload[1]));
    }
    let receipt = LocalSqlPreparedReceipt {
        plan_id: NonZeroU64::new(read_u64(payload, 4)?)
            .ok_or(LocalSqlCodecError::InvalidIdentity)?,
        catalog_version: CatalogVersion::new(read_u64(payload, 12)?)
            .map_err(|_| LocalSqlCodecError::InvalidIdentity)?,
        parameter_count: read_u32(payload, 20)?,
        column_count: read_u32(payload, 24)?,
        maximum_rows: read_u32(payload, 28)?,
    };
    validate_receipt_counts(receipt)?;
    Ok(receipt)
}

/// Encodes one canonical prepared SQL execution request.
///
/// # Errors
///
/// Returns an error for too many parameters, a noncanonical scalar, or a
/// request exceeding the negotiated frame bound.
pub fn encode_local_sql_execute<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    plan_id: NonZeroU64,
    parameters: &[SqlValue],
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSqlCodecError> {
    validate_parameter_count(parameters.len())?;
    ensure_payload_bound(LOCAL_SQL_EXECUTE_HEADER_SIZE, maximum_payload)?;
    let parameter_count =
        u32::try_from(parameters.len()).map_err(|_| LocalSqlCodecError::ParameterCountExceeded)?;
    let mut encoded_length = LOCAL_SQL_EXECUTE_HEADER_SIZE;
    for parameter in parameters {
        encoded_length = checked_payload_add(
            encoded_length,
            SCALAR_RECORD_HEADER_SIZE
                .checked_add(scalar_payload_length(parameter)?)
                .ok_or(LocalSqlCodecError::PayloadTooLarge)?,
            maximum_payload,
        )?;
    }
    buffer.resize(encoded_length, 0);
    buffer.fill(0);
    buffer[0] = SQL_PAYLOAD_VERSION;
    buffer[1] = EXECUTE_PREPARED_SELECT_OPCODE;
    buffer[4..12].copy_from_slice(&plan_id.get().to_le_bytes());
    buffer[12..16].copy_from_slice(&parameter_count.to_le_bytes());
    let mut offset = LOCAL_SQL_EXECUTE_HEADER_SIZE;
    for parameter in parameters {
        encode_parameter_record(buffer, &mut offset, parameter)?;
    }
    debug_assert_eq!(offset, encoded_length);
    Ok(buffer)
}

/// Decodes one canonical prepared SQL execution request.
///
/// # Errors
///
/// Returns an error for malformed framing, an invalid plan identity, too many
/// parameters, or a noncanonical scalar.
pub fn decode_local_sql_execute(
    payload: &[u8],
) -> Result<LocalSqlExecuteRequest, LocalSqlCodecError> {
    require_header(payload, LOCAL_SQL_EXECUTE_HEADER_SIZE)?;
    validate_version_and_reserved(payload)?;
    if payload[1] != EXECUTE_PREPARED_SELECT_OPCODE {
        return Err(LocalSqlCodecError::UnknownExecuteOpcode(payload[1]));
    }
    let plan_id =
        NonZeroU64::new(read_u64(payload, 4)?).ok_or(LocalSqlCodecError::InvalidIdentity)?;
    let parameter_count = usize_from_u32(read_u32(payload, 12)?)?;
    validate_parameter_count(parameter_count)?;
    let mut parameters = Vec::with_capacity(parameter_count);
    let mut offset = LOCAL_SQL_EXECUTE_HEADER_SIZE;
    for _ in 0..parameter_count {
        parameters.push(decode_parameter_record(payload, &mut offset)?);
    }
    if offset != payload.len() {
        return Err(LocalSqlCodecError::LengthMismatch);
    }
    Ok(LocalSqlExecuteRequest {
        plan_id,
        parameters,
    })
}

/// Encodes one canonical transaction-bound SQL DML request.
///
/// # Errors
///
/// Returns an error for empty/overlong SQL, too many parameters,
/// noncanonical scalars, or a request exceeding the negotiated frame bound.
pub fn encode_local_transaction_sql_dml<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    handle: NonZeroU64,
    statement: &str,
    parameters: &[SqlValue],
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSqlCodecError> {
    validate_statement_length(statement.len())?;
    validate_parameter_count(parameters.len())?;
    ensure_payload_bound(LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE, maximum_payload)?;
    let statement_length =
        u32::try_from(statement.len()).map_err(|_| LocalSqlCodecError::StatementTooLarge)?;
    let parameter_count =
        u32::try_from(parameters.len()).map_err(|_| LocalSqlCodecError::ParameterCountExceeded)?;
    let mut encoded_length = LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE
        .checked_add(statement.len())
        .ok_or(LocalSqlCodecError::PayloadTooLarge)?;
    ensure_payload_bound(encoded_length, maximum_payload)?;
    for parameter in parameters {
        encoded_length = checked_payload_add(
            encoded_length,
            SCALAR_RECORD_HEADER_SIZE
                .checked_add(scalar_payload_length(parameter)?)
                .ok_or(LocalSqlCodecError::PayloadTooLarge)?,
            maximum_payload,
        )?;
    }
    buffer.resize(encoded_length, 0);
    buffer.fill(0);
    buffer[0] = SQL_PAYLOAD_VERSION;
    buffer[1] = EXECUTE_TRANSACTION_DML_OPCODE;
    buffer[4..12].copy_from_slice(&handle.get().to_le_bytes());
    buffer[12..16].copy_from_slice(&statement_length.to_le_bytes());
    buffer[16..20].copy_from_slice(&parameter_count.to_le_bytes());
    let mut offset = LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE;
    write_exact(buffer, &mut offset, statement.as_bytes())?;
    for parameter in parameters {
        encode_parameter_record(buffer, &mut offset, parameter)?;
    }
    debug_assert_eq!(offset, encoded_length);
    Ok(buffer)
}

/// Decodes one canonical transaction-bound SQL DML request.
///
/// # Errors
///
/// Returns an error for malformed framing, handle, UTF-8, statement,
/// parameter, scalar, or trailing-byte boundaries.
pub fn decode_local_transaction_sql_dml(
    payload: &[u8],
) -> Result<LocalTransactionSqlDmlRequest<'_>, LocalSqlCodecError> {
    require_header(payload, LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE)?;
    validate_version_and_reserved(payload)?;
    if payload[1] != EXECUTE_TRANSACTION_DML_OPCODE {
        return Err(LocalSqlCodecError::UnknownExecuteOpcode(payload[1]));
    }
    if payload[20..24] != [0, 0, 0, 0] {
        return Err(LocalSqlCodecError::ReservedBytes);
    }
    let handle =
        NonZeroU64::new(read_u64(payload, 4)?).ok_or(LocalSqlCodecError::InvalidIdentity)?;
    let statement_length = usize_from_u32(read_u32(payload, 12)?)?;
    validate_statement_length(statement_length)?;
    let parameter_count = usize_from_u32(read_u32(payload, 16)?)?;
    validate_parameter_count(parameter_count)?;
    let mut offset = LOCAL_TRANSACTION_SQL_DML_HEADER_SIZE;
    let statement = std::str::from_utf8(take(payload, &mut offset, statement_length)?)
        .map_err(|_| LocalSqlCodecError::InvalidUtf8)?;
    let mut parameters = Vec::with_capacity(parameter_count);
    for _ in 0..parameter_count {
        parameters.push(decode_parameter_record(payload, &mut offset)?);
    }
    if offset != payload.len() {
        return Err(LocalSqlCodecError::LengthMismatch);
    }
    Ok(LocalTransactionSqlDmlRequest {
        handle,
        statement,
        parameters,
    })
}

/// Encodes one complete canonical SQL row result.
///
/// # Errors
///
/// Returns an error for over-limit schema/rows, a row-width mismatch,
/// noncanonical values, or a response exceeding the negotiated frame bound.
pub fn encode_local_sql_rows<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    visible_csn: Csn,
    columns: &[LocalSqlColumn],
    rows: &[Vec<SqlValue>],
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSqlCodecError> {
    validate_column_count(columns.len())?;
    validate_row_count(rows.len())?;
    ensure_payload_bound(LOCAL_SQL_ROWS_HEADER_SIZE, maximum_payload)?;
    let encoded_schema = encode_schema_descriptors(columns)?;
    let mut encoded_length = LOCAL_SQL_ROWS_HEADER_SIZE;
    for (column, descriptor) in columns.iter().zip(&encoded_schema) {
        encoded_length = checked_payload_add(
            encoded_length,
            COLUMN_RECORD_HEADER_SIZE
                .checked_add(column.name.len())
                .and_then(|length| length.checked_add(descriptor.len()))
                .ok_or(LocalSqlCodecError::PayloadTooLarge)?,
            maximum_payload,
        )?;
    }
    for row in rows {
        if row.len() != columns.len() {
            return Err(LocalSqlCodecError::RowWidthMismatch);
        }
        for (value, column) in row.iter().zip(columns) {
            let value_length = encoded_cell_length(value, &column.logical_type)?;
            encoded_length = checked_payload_add(
                encoded_length,
                CELL_RECORD_HEADER_SIZE
                    .checked_add(value_length)
                    .ok_or(LocalSqlCodecError::PayloadTooLarge)?,
                maximum_payload,
            )?;
        }
    }

    let column_count =
        u32::try_from(columns.len()).map_err(|_| LocalSqlCodecError::ColumnCountExceeded)?;
    let row_count = u32::try_from(rows.len()).map_err(|_| LocalSqlCodecError::RowCountExceeded)?;
    buffer.resize(encoded_length, 0);
    buffer.fill(0);
    buffer[0] = SQL_PAYLOAD_VERSION;
    buffer[1] = SQL_ROWS_VALUE_TAG;
    buffer[4..12].copy_from_slice(&visible_csn.get().to_le_bytes());
    buffer[12..16].copy_from_slice(&column_count.to_le_bytes());
    buffer[16..20].copy_from_slice(&row_count.to_le_bytes());
    let mut offset = LOCAL_SQL_ROWS_HEADER_SIZE;
    for (column, descriptor) in columns.iter().zip(&encoded_schema) {
        encode_column_record(buffer, &mut offset, column, descriptor)?;
    }
    for row in rows {
        for (value, column) in row.iter().zip(columns) {
            encode_cell_record(buffer, &mut offset, value, &column.logical_type)?;
        }
    }
    debug_assert_eq!(offset, encoded_length);
    Ok(buffer)
}

/// Decodes one complete canonical SQL row result.
///
/// # Errors
///
/// Returns an error for malformed framing, zero CSN, invalid schema, row
/// bounds, noncanonical cells, or trailing bytes.
pub fn decode_local_sql_rows(payload: &[u8]) -> Result<LocalSqlRows, LocalSqlCodecError> {
    require_header(payload, LOCAL_SQL_ROWS_HEADER_SIZE)?;
    validate_version_and_reserved(payload)?;
    if payload[1] != SQL_ROWS_VALUE_TAG {
        return Err(LocalSqlCodecError::UnknownValueTag(payload[1]));
    }
    let visible_csn =
        Csn::new(read_u64(payload, 4)?).map_err(|_| LocalSqlCodecError::InvalidIdentity)?;
    let column_count = usize_from_u32(read_u32(payload, 12)?)?;
    let row_count = usize_from_u32(read_u32(payload, 16)?)?;
    validate_column_count(column_count)?;
    validate_row_count(row_count)?;
    let mut offset = LOCAL_SQL_ROWS_HEADER_SIZE;
    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        columns.push(decode_column_record(payload, &mut offset)?);
    }
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let mut row = Vec::with_capacity(column_count);
        for column in &columns {
            row.push(decode_cell_record(
                payload,
                &mut offset,
                &column.logical_type,
            )?);
        }
        rows.push(row);
    }
    if offset != payload.len() {
        return Err(LocalSqlCodecError::LengthMismatch);
    }
    Ok(LocalSqlRows {
        visible_csn,
        columns,
        rows,
    })
}

fn validate_receipt_counts(receipt: LocalSqlPreparedReceipt) -> Result<(), LocalSqlCodecError> {
    validate_parameter_count(
        usize::try_from(receipt.parameter_count)
            .map_err(|_| LocalSqlCodecError::ParameterCountExceeded)?,
    )?;
    validate_column_count(
        usize::try_from(receipt.column_count)
            .map_err(|_| LocalSqlCodecError::ColumnCountExceeded)?,
    )?;
    validate_row_count(
        usize::try_from(receipt.maximum_rows).map_err(|_| LocalSqlCodecError::RowCountExceeded)?,
    )
}

fn encode_schema_descriptors(
    columns: &[LocalSqlColumn],
) -> Result<Vec<Vec<u8>>, LocalSqlCodecError> {
    columns
        .iter()
        .map(|column| {
            if column.name.len() > MAX_LOCAL_SQL_COLUMN_NAME_BYTES {
                return Err(LocalSqlCodecError::ColumnNameTooLarge);
            }
            let descriptor = column
                .logical_type
                .encode_descriptor()
                .map_err(|_| LocalSqlCodecError::InvalidTypeDescriptor)?;
            if descriptor.is_empty() || descriptor.len() > MAX_LOCAL_SQL_TYPE_DESCRIPTOR_BYTES {
                return Err(LocalSqlCodecError::InvalidTypeDescriptor);
            }
            Ok(descriptor)
        })
        .collect()
}

fn encode_column_record(
    output: &mut [u8],
    offset: &mut usize,
    column: &LocalSqlColumn,
    descriptor: &[u8],
) -> Result<(), LocalSqlCodecError> {
    let name_length =
        u32::try_from(column.name.len()).map_err(|_| LocalSqlCodecError::ColumnNameTooLarge)?;
    let descriptor_length =
        u32::try_from(descriptor.len()).map_err(|_| LocalSqlCodecError::InvalidTypeDescriptor)?;
    write_exact(output, offset, &name_length.to_le_bytes())?;
    write_exact(output, offset, &descriptor_length.to_le_bytes())?;
    write_exact(output, offset, column.name.as_bytes())?;
    write_exact(output, offset, descriptor)
}

fn decode_column_record(
    payload: &[u8],
    offset: &mut usize,
) -> Result<LocalSqlColumn, LocalSqlCodecError> {
    require_from_offset(payload, *offset, COLUMN_RECORD_HEADER_SIZE)?;
    let name_length = usize_from_u32(read_u32(payload, *offset)?)?;
    let descriptor_length = usize_from_u32(read_u32(payload, *offset + 4)?)?;
    if name_length > MAX_LOCAL_SQL_COLUMN_NAME_BYTES {
        return Err(LocalSqlCodecError::ColumnNameTooLarge);
    }
    if descriptor_length == 0 || descriptor_length > MAX_LOCAL_SQL_TYPE_DESCRIPTOR_BYTES {
        return Err(LocalSqlCodecError::InvalidTypeDescriptor);
    }
    *offset = offset
        .checked_add(COLUMN_RECORD_HEADER_SIZE)
        .ok_or(LocalSqlCodecError::Truncated)?;
    let name_bytes = take(payload, offset, name_length)?;
    let descriptor = take(payload, offset, descriptor_length)?;
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| LocalSqlCodecError::InvalidUtf8)?
        .to_owned();
    let logical_type = LogicalType::decode_descriptor(descriptor)
        .map_err(|_| LocalSqlCodecError::InvalidTypeDescriptor)?;
    Ok(LocalSqlColumn { name, logical_type })
}

fn encoded_cell_length(
    value: &SqlValue,
    logical_type: &LogicalType,
) -> Result<usize, LocalSqlCodecError> {
    match value {
        ScalarValue::Null => Ok(0),
        _ => value
            .encode_storage(logical_type)
            .map(|encoded| encoded.len())
            .map_err(|_| LocalSqlCodecError::InvalidScalar),
    }
}

fn encode_cell_record(
    output: &mut [u8],
    offset: &mut usize,
    value: &SqlValue,
    logical_type: &LogicalType,
) -> Result<(), LocalSqlCodecError> {
    let encoded = match value {
        ScalarValue::Null => None,
        _ => Some(
            value
                .encode_storage(logical_type)
                .map_err(|_| LocalSqlCodecError::InvalidScalar)?,
        ),
    };
    let value_length = encoded.as_ref().map_or(0, Vec::len);
    let value_length =
        u32::try_from(value_length).map_err(|_| LocalSqlCodecError::InvalidScalar)?;
    let tag = if encoded.is_some() {
        CELL_PRESENT
    } else {
        CELL_NULL
    };
    write_exact(output, offset, &[tag, 0, 0, 0])?;
    write_exact(output, offset, &value_length.to_le_bytes())?;
    if let Some(encoded) = encoded {
        write_exact(output, offset, &encoded)?;
    }
    Ok(())
}

fn decode_cell_record(
    payload: &[u8],
    offset: &mut usize,
    logical_type: &LogicalType,
) -> Result<SqlValue, LocalSqlCodecError> {
    require_from_offset(payload, *offset, CELL_RECORD_HEADER_SIZE)?;
    let tag = payload[*offset];
    if payload[*offset + 1..*offset + 4] != [0, 0, 0] {
        return Err(LocalSqlCodecError::ReservedBytes);
    }
    let value_length = usize_from_u32(read_u32(payload, *offset + 4)?)?;
    if value_length > MAX_SCALAR_BYTES {
        return Err(LocalSqlCodecError::InvalidScalar);
    }
    *offset = offset
        .checked_add(CELL_RECORD_HEADER_SIZE)
        .ok_or(LocalSqlCodecError::Truncated)?;
    let encoded = take(payload, offset, value_length)?;
    match tag {
        CELL_NULL if encoded.is_empty() => Ok(ScalarValue::Null),
        CELL_NULL => Err(LocalSqlCodecError::NoncanonicalNull),
        CELL_PRESENT => ScalarValue::decode_storage(logical_type, encoded)
            .map_err(|_| LocalSqlCodecError::InvalidScalar),
        _ => Err(LocalSqlCodecError::UnknownValueTag(tag)),
    }
}

fn scalar_payload_length(value: &SqlValue) -> Result<usize, LocalSqlCodecError> {
    let length = match value {
        ScalarValue::Null => 0,
        ScalarValue::Boolean(_) => 1,
        ScalarValue::Signed(_)
        | ScalarValue::Unsigned(_)
        | ScalarValue::Float64(_)
        | ScalarValue::Time(_)
        | ScalarValue::Timestamp(_) => 8,
        ScalarValue::Decimal(_) | ScalarValue::Interval { .. } | ScalarValue::Uuid(_) => 16,
        ScalarValue::Float32(_) | ScalarValue::Date(_) => 4,
        ScalarValue::Text(value) => value.len(),
        ScalarValue::Binary(value) => value.len(),
        _ => return Err(LocalSqlCodecError::InvalidScalar),
    };
    if length > MAX_SCALAR_BYTES {
        return Err(LocalSqlCodecError::InvalidScalar);
    }
    if let ScalarValue::Time(value) = value {
        validate_time(*value)?;
    }
    Ok(length)
}

fn encode_parameter_record(
    output: &mut [u8],
    offset: &mut usize,
    value: &SqlValue,
) -> Result<(), LocalSqlCodecError> {
    let length = scalar_payload_length(value)?;
    let length = u32::try_from(length).map_err(|_| LocalSqlCodecError::InvalidScalar)?;
    let tag = scalar_tag(value).ok_or(LocalSqlCodecError::InvalidScalar)?;
    write_exact(output, offset, &[tag, 0, 0, 0])?;
    write_exact(output, offset, &length.to_le_bytes())?;
    match value {
        ScalarValue::Null => {}
        ScalarValue::Boolean(value) => write_exact(output, offset, &[u8::from(*value)])?,
        ScalarValue::Signed(value) | ScalarValue::Timestamp(value) => {
            write_exact(output, offset, &value.to_le_bytes())?;
        }
        ScalarValue::Unsigned(value) | ScalarValue::Time(value) => {
            write_exact(output, offset, &value.to_le_bytes())?;
        }
        ScalarValue::Decimal(value) => write_exact(output, offset, &value.to_le_bytes())?,
        ScalarValue::Float32(value) => write_exact(output, offset, &value.bits().to_le_bytes())?,
        ScalarValue::Float64(value) => write_exact(output, offset, &value.bits().to_le_bytes())?,
        ScalarValue::Text(value) => write_exact(output, offset, value.as_bytes())?,
        ScalarValue::Binary(value) => write_exact(output, offset, value)?,
        ScalarValue::Date(value) => write_exact(output, offset, &value.to_le_bytes())?,
        ScalarValue::Interval {
            months,
            days,
            nanoseconds,
        } => {
            write_exact(output, offset, &months.to_le_bytes())?;
            write_exact(output, offset, &days.to_le_bytes())?;
            write_exact(output, offset, &nanoseconds.to_le_bytes())?;
        }
        ScalarValue::Uuid(value) => write_exact(output, offset, value)?,
        _ => return Err(LocalSqlCodecError::InvalidScalar),
    }
    Ok(())
}

fn decode_parameter_record(
    payload: &[u8],
    offset: &mut usize,
) -> Result<SqlValue, LocalSqlCodecError> {
    require_from_offset(payload, *offset, SCALAR_RECORD_HEADER_SIZE)?;
    let tag = payload[*offset];
    if payload[*offset + 1..*offset + 4] != [0, 0, 0] {
        return Err(LocalSqlCodecError::ReservedBytes);
    }
    let value_length = usize_from_u32(read_u32(payload, *offset + 4)?)?;
    if value_length > MAX_SCALAR_BYTES {
        return Err(LocalSqlCodecError::InvalidScalar);
    }
    *offset = offset
        .checked_add(SCALAR_RECORD_HEADER_SIZE)
        .ok_or(LocalSqlCodecError::Truncated)?;
    let encoded = take(payload, offset, value_length)?;
    decode_scalar(tag, encoded)
}

fn decode_scalar(tag: u8, encoded: &[u8]) -> Result<SqlValue, LocalSqlCodecError> {
    match tag {
        SCALAR_NULL if encoded.is_empty() => Ok(ScalarValue::Null),
        SCALAR_NULL => Err(LocalSqlCodecError::NoncanonicalNull),
        SCALAR_BOOLEAN => match encoded {
            [0] => Ok(ScalarValue::Boolean(false)),
            [1] => Ok(ScalarValue::Boolean(true)),
            _ => Err(LocalSqlCodecError::InvalidScalar),
        },
        SCALAR_SIGNED => Ok(ScalarValue::Signed(i64::from_le_bytes(exact(encoded)?))),
        SCALAR_UNSIGNED => Ok(ScalarValue::Unsigned(u64::from_le_bytes(exact(encoded)?))),
        SCALAR_DECIMAL => Ok(ScalarValue::Decimal(i128::from_le_bytes(exact(encoded)?))),
        SCALAR_FLOAT32 => {
            let bits = u32::from_le_bytes(exact(encoded)?);
            let value = CanonicalF32::new(f32::from_bits(bits));
            if value.bits() != bits {
                return Err(LocalSqlCodecError::InvalidScalar);
            }
            Ok(ScalarValue::Float32(value))
        }
        SCALAR_FLOAT64 => {
            let bits = u64::from_le_bytes(exact(encoded)?);
            let value = CanonicalF64::new(f64::from_bits(bits));
            if value.bits() != bits {
                return Err(LocalSqlCodecError::InvalidScalar);
            }
            Ok(ScalarValue::Float64(value))
        }
        SCALAR_TEXT => std::str::from_utf8(encoded)
            .map(|value| ScalarValue::Text(value.to_owned()))
            .map_err(|_| LocalSqlCodecError::InvalidUtf8),
        SCALAR_BINARY => Ok(ScalarValue::Binary(encoded.to_vec())),
        SCALAR_DATE => Ok(ScalarValue::Date(i32::from_le_bytes(exact(encoded)?))),
        SCALAR_TIME => {
            let value = u64::from_le_bytes(exact(encoded)?);
            validate_time(value)?;
            Ok(ScalarValue::Time(value))
        }
        SCALAR_TIMESTAMP => Ok(ScalarValue::Timestamp(i64::from_le_bytes(exact(encoded)?))),
        SCALAR_INTERVAL => {
            if encoded.len() != 16 {
                return Err(LocalSqlCodecError::InvalidScalar);
            }
            Ok(ScalarValue::Interval {
                months: i32::from_le_bytes(exact(&encoded[0..4])?),
                days: i32::from_le_bytes(exact(&encoded[4..8])?),
                nanoseconds: i64::from_le_bytes(exact(&encoded[8..16])?),
            })
        }
        SCALAR_UUID => Ok(ScalarValue::Uuid(exact(encoded)?)),
        _ => Err(LocalSqlCodecError::UnknownScalarTag(tag)),
    }
}

const fn scalar_tag(value: &SqlValue) -> Option<u8> {
    match value {
        ScalarValue::Null => Some(SCALAR_NULL),
        ScalarValue::Boolean(_) => Some(SCALAR_BOOLEAN),
        ScalarValue::Signed(_) => Some(SCALAR_SIGNED),
        ScalarValue::Unsigned(_) => Some(SCALAR_UNSIGNED),
        ScalarValue::Decimal(_) => Some(SCALAR_DECIMAL),
        ScalarValue::Float32(_) => Some(SCALAR_FLOAT32),
        ScalarValue::Float64(_) => Some(SCALAR_FLOAT64),
        ScalarValue::Text(_) => Some(SCALAR_TEXT),
        ScalarValue::Binary(_) => Some(SCALAR_BINARY),
        ScalarValue::Date(_) => Some(SCALAR_DATE),
        ScalarValue::Time(_) => Some(SCALAR_TIME),
        ScalarValue::Timestamp(_) => Some(SCALAR_TIMESTAMP),
        ScalarValue::Interval { .. } => Some(SCALAR_INTERVAL),
        ScalarValue::Uuid(_) => Some(SCALAR_UUID),
        _ => None,
    }
}

fn validate_statement_length(length: usize) -> Result<(), LocalSqlCodecError> {
    if length == 0 {
        return Err(LocalSqlCodecError::EmptyStatement);
    }
    if length > MAX_LOCAL_SQL_STATEMENT_BYTES {
        return Err(LocalSqlCodecError::StatementTooLarge);
    }
    Ok(())
}

fn validate_parameter_count(count: usize) -> Result<(), LocalSqlCodecError> {
    if count > MAX_LOCAL_SQL_PARAMETERS {
        return Err(LocalSqlCodecError::ParameterCountExceeded);
    }
    Ok(())
}

fn validate_column_count(count: usize) -> Result<(), LocalSqlCodecError> {
    if count > MAX_LOCAL_SQL_COLUMNS {
        return Err(LocalSqlCodecError::ColumnCountExceeded);
    }
    Ok(())
}

fn validate_row_count(count: usize) -> Result<(), LocalSqlCodecError> {
    if count > MAX_LOCAL_SQL_ROWS {
        return Err(LocalSqlCodecError::RowCountExceeded);
    }
    Ok(())
}

fn validate_time(value: u64) -> Result<(), LocalSqlCodecError> {
    if value >= NANOS_PER_DAY {
        return Err(LocalSqlCodecError::InvalidScalar);
    }
    Ok(())
}

fn validate_version_and_reserved(payload: &[u8]) -> Result<(), LocalSqlCodecError> {
    if payload[0] != SQL_PAYLOAD_VERSION {
        return Err(LocalSqlCodecError::UnsupportedVersion(payload[0]));
    }
    if payload[2..4] != [0, 0] {
        return Err(LocalSqlCodecError::ReservedBytes);
    }
    Ok(())
}

fn require_header(payload: &[u8], length: usize) -> Result<(), LocalSqlCodecError> {
    if payload.len() < length {
        return Err(LocalSqlCodecError::Truncated);
    }
    Ok(())
}

fn require_fixed(payload: &[u8], length: usize) -> Result<(), LocalSqlCodecError> {
    if payload.len() < length {
        return Err(LocalSqlCodecError::Truncated);
    }
    if payload.len() != length {
        return Err(LocalSqlCodecError::LengthMismatch);
    }
    Ok(())
}

fn require_exact_remaining(
    payload: &[u8],
    offset: usize,
    length: usize,
) -> Result<(), LocalSqlCodecError> {
    let expected = offset
        .checked_add(length)
        .ok_or(LocalSqlCodecError::LengthMismatch)?;
    if expected != payload.len() {
        return Err(LocalSqlCodecError::LengthMismatch);
    }
    Ok(())
}

fn require_from_offset(
    payload: &[u8],
    offset: usize,
    length: usize,
) -> Result<(), LocalSqlCodecError> {
    let end = offset
        .checked_add(length)
        .ok_or(LocalSqlCodecError::Truncated)?;
    if end > payload.len() {
        return Err(LocalSqlCodecError::Truncated);
    }
    Ok(())
}

fn ensure_payload_bound(
    encoded_length: usize,
    maximum_payload: usize,
) -> Result<(), LocalSqlCodecError> {
    if encoded_length > maximum_payload {
        return Err(LocalSqlCodecError::PayloadTooLarge);
    }
    Ok(())
}

fn checked_payload_add(
    current: usize,
    additional: usize,
    maximum_payload: usize,
) -> Result<usize, LocalSqlCodecError> {
    let length = current
        .checked_add(additional)
        .ok_or(LocalSqlCodecError::PayloadTooLarge)?;
    ensure_payload_bound(length, maximum_payload)?;
    Ok(length)
}

fn read_u32(payload: &[u8], offset: usize) -> Result<u32, LocalSqlCodecError> {
    let bytes = payload
        .get(offset..offset.checked_add(4).ok_or(LocalSqlCodecError::Truncated)?)
        .ok_or(LocalSqlCodecError::Truncated)?;
    Ok(u32::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| LocalSqlCodecError::Truncated)?,
    ))
}

fn read_u64(payload: &[u8], offset: usize) -> Result<u64, LocalSqlCodecError> {
    let bytes = payload
        .get(offset..offset.checked_add(8).ok_or(LocalSqlCodecError::Truncated)?)
        .ok_or(LocalSqlCodecError::Truncated)?;
    Ok(u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| LocalSqlCodecError::Truncated)?,
    ))
}

fn usize_from_u32(value: u32) -> Result<usize, LocalSqlCodecError> {
    usize::try_from(value).map_err(|_| LocalSqlCodecError::LengthMismatch)
}

fn take<'payload>(
    payload: &'payload [u8],
    offset: &mut usize,
    length: usize,
) -> Result<&'payload [u8], LocalSqlCodecError> {
    let end = offset
        .checked_add(length)
        .ok_or(LocalSqlCodecError::Truncated)?;
    let value = payload
        .get(*offset..end)
        .ok_or(LocalSqlCodecError::Truncated)?;
    *offset = end;
    Ok(value)
}

fn exact<const LENGTH: usize>(encoded: &[u8]) -> Result<[u8; LENGTH], LocalSqlCodecError> {
    encoded
        .try_into()
        .map_err(|_| LocalSqlCodecError::InvalidScalar)
}

fn write_exact(
    output: &mut [u8],
    offset: &mut usize,
    value: &[u8],
) -> Result<(), LocalSqlCodecError> {
    let end = offset
        .checked_add(value.len())
        .ok_or(LocalSqlCodecError::PayloadTooLarge)?;
    let target = output
        .get_mut(*offset..end)
        .ok_or(LocalSqlCodecError::PayloadTooLarge)?;
    target.copy_from_slice(value);
    *offset = end;
    Ok(())
}
