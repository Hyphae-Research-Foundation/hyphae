// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, num::NonZeroU64};

use hyphae_native_types::DurabilityClass;
use thiserror::Error;

use crate::{
    FrameKind, LOCAL_COMMIT_RECEIPT_SIZE, LOCAL_OPERATION_HEADER_SIZE,
    LOCAL_SEARCH_RESULTS_HEADER_SIZE, LOCAL_SQL_PREPARED_RECEIPT_SIZE, LOCAL_SQL_ROWS_HEADER_SIZE,
    LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE, LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE,
    LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE, LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE,
    LocalFailureCode, LocalOperationCodecError, LocalSearchCodecError, LocalSearchMatchRequest,
    LocalSqlColumn, LocalSqlPreparedReceipt, LocalStructureCommitReceipt, LocalStructureRequest,
    LocalStructureSetRequest, LocalTransactionBeginReceipt, LocalTransactionCommitReceipt,
    LocalTransactionDeleteDocumentRequest, LocalTransactionEngine,
    LocalTransactionIndexDocumentRequest, LocalTransactionReplaceDocumentRequest,
    LocalTransactionRollbackReceipt, LocalTransactionSqlDmlRequest, LocalTransactionStageReceipt,
    LocalTransactionStructureSetRequest, LocalTransportError, LocalTtlValue,
    MAX_LOCAL_PREPARED_STATEMENTS, MAX_LOCAL_SQL_COLUMNS, MAX_LOCAL_SQL_PARAMETERS,
    MAX_LOCAL_SQL_ROWS, MAX_LOCAL_TRANSACTION_OPERATIONS, NativeDatabase, NativeRuntimeError,
    NativeSchedulerClock, NativeWriteBatch, PreparedStatement, SqlError, SqlResult, Ttl,
    UdsFrameConnection, decode_local_search_match, decode_local_sql_execute,
    decode_local_sql_prepare, decode_local_structure_request, decode_local_transaction_begin,
    decode_local_transaction_commit, decode_local_transaction_delete_document,
    decode_local_transaction_index_document, decode_local_transaction_replace_document,
    decode_local_transaction_rollback, decode_local_transaction_sql_dml, encode_local_failure,
    encode_local_search_match_results, encode_local_sql_prepared_receipt, encode_local_sql_rows,
    encode_local_structure_commit_receipt, encode_local_transaction_begin_receipt,
    encode_local_transaction_commit_receipt, encode_local_transaction_rollback_receipt,
    encode_local_transaction_stage_receipt, encode_local_ttl, encode_local_value,
    local_search::{
        TRANSACTION_DELETE_DOCUMENT_OPCODE, TRANSACTION_INDEX_DOCUMENT_OPCODE,
        TRANSACTION_REPLACE_DOCUMENT_OPCODE,
    },
    local_sql::EXECUTE_TRANSACTION_DML_OPCODE,
};

/// Failure of one serial engine-bearing local session.
#[derive(Debug, Error)]
pub enum LocalSessionError {
    /// The framed local transport failed.
    #[error(transparent)]
    Transport(#[from] LocalTransportError),
    /// The peer closed before a canonical `CLOSE` exchange.
    #[error("native local session peer closed without CLOSE")]
    PeerClosed,
    /// The connection frame bound cannot hold one operation header.
    #[error("native local session frame bound is smaller than an operation header")]
    PayloadBoundTooSmall,
    /// The first frame was not a canonical minimal `HELLO`.
    #[error("native local session handshake is invalid")]
    InvalidHandshake,
}

struct RetainedSqlPlan {
    prepared: PreparedStatement,
    columns: Vec<LocalSqlColumn>,
    maximum_rows: usize,
}

struct ActiveLocalTransaction {
    handle: NonZeroU64,
    batch: NativeWriteBatch,
    durability: DurabilityClass,
    logical_time_micros: i64,
    staged_operations: u64,
}

/// Serial local session exposing scalar, lexical, prepared SQL, and explicit
/// all-engine transaction operations.
pub struct LocalDataSession<'database, Clock: NativeSchedulerClock + ?Sized> {
    database: &'database mut NativeDatabase,
    clock: &'database Clock,
    prepared_statements: BTreeMap<u64, RetainedSqlPlan>,
    next_plan_id: Option<NonZeroU64>,
    active_transaction: Option<ActiveLocalTransaction>,
    next_transaction_handle: Option<NonZeroU64>,
    request_buffer: Vec<u8>,
    response_buffer: Vec<u8>,
    echo_buffer: Vec<u8>,
}

impl<'database, Clock: NativeSchedulerClock + ?Sized> LocalDataSession<'database, Clock> {
    /// Creates one reusable serial session over the supplied database and
    /// logical-time authority.
    pub fn new(database: &'database mut NativeDatabase, clock: &'database Clock) -> Self {
        Self {
            database,
            clock,
            prepared_statements: BTreeMap::new(),
            next_plan_id: NonZeroU64::new(1),
            active_transaction: None,
            next_transaction_handle: NonZeroU64::new(1),
            request_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            response_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            echo_buffer: Vec::new(),
        }
    }

    /// Serves one minimal `HELLO`, engine operation, `CLOSE` session.
    ///
    /// Request-local codec and engine failures are returned to the peer and
    /// do not terminate the session.
    ///
    /// # Errors
    ///
    /// Returns an error for an insufficient frame bound, invalid handshake,
    /// premature peer close, or framed transport failure.
    pub fn serve(&mut self, connection: &mut UdsFrameConnection) -> Result<(), LocalSessionError> {
        let maximum_payload = connection.maximum_payload();
        if maximum_payload < LOCAL_OPERATION_HEADER_SIZE {
            return Err(LocalSessionError::PayloadBoundTooSmall);
        }
        self.handshake(connection)?;
        loop {
            let frame = connection.receive()?.ok_or(LocalSessionError::PeerClosed)?;
            let stream_id = frame.stream_id;
            let request_id = frame.request_id;
            match frame.kind {
                FrameKind::Ping => {
                    self.echo_buffer.clear();
                    self.echo_buffer.extend_from_slice(frame.payload);
                    connection.send(FrameKind::Ping, stream_id, request_id, &self.echo_buffer)?;
                }
                FrameKind::Structure => {
                    self.request_buffer.clear();
                    self.request_buffer.extend_from_slice(frame.payload);
                    self.serve_structure_request(
                        connection,
                        stream_id,
                        request_id,
                        maximum_payload,
                    )?;
                }
                FrameKind::Search => {
                    self.request_buffer.clear();
                    self.request_buffer.extend_from_slice(frame.payload);
                    self.serve_search_request(connection, stream_id, request_id, maximum_payload)?;
                }
                FrameKind::Prepare => {
                    self.request_buffer.clear();
                    self.request_buffer.extend_from_slice(frame.payload);
                    if self.active_transaction.is_some() {
                        self.send_failure(
                            connection,
                            stream_id,
                            request_id,
                            LocalFailureCode::TransactionActive,
                        )?;
                    } else {
                        self.serve_sql_prepare_request(
                            connection,
                            stream_id,
                            request_id,
                            maximum_payload,
                        )?;
                    }
                }
                FrameKind::Execute => {
                    self.request_buffer.clear();
                    self.request_buffer.extend_from_slice(frame.payload);
                    self.serve_sql_execute_request(
                        connection,
                        stream_id,
                        request_id,
                        maximum_payload,
                    )?;
                }
                FrameKind::Begin | FrameKind::Commit | FrameKind::Rollback => {
                    let kind = frame.kind;
                    self.request_buffer.clear();
                    self.request_buffer.extend_from_slice(frame.payload);
                    self.serve_transaction_control_request(
                        connection,
                        kind,
                        stream_id,
                        request_id,
                        maximum_payload,
                    )?;
                }
                FrameKind::Close => {
                    drop(self.active_transaction.take());
                    self.echo_buffer.clear();
                    self.echo_buffer.extend_from_slice(frame.payload);
                    connection.send(FrameKind::Close, stream_id, request_id, &self.echo_buffer)?;
                    return Ok(());
                }
                _ => self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    if self.active_transaction.is_some() {
                        LocalFailureCode::TransactionActive
                    } else {
                        LocalFailureCode::UnexpectedFrame
                    },
                )?,
            }
        }
    }

    fn serve_transaction_control_request(
        &mut self,
        connection: &mut UdsFrameConnection,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        match kind {
            FrameKind::Begin => {
                self.serve_transaction_begin(connection, stream_id, request_id, maximum_payload)
            }
            FrameKind::Commit => {
                self.serve_transaction_commit(connection, stream_id, request_id, maximum_payload)
            }
            FrameKind::Rollback => {
                self.serve_transaction_rollback(connection, stream_id, request_id, maximum_payload)
            }
            _ => self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::UnexpectedFrame,
            ),
        }
    }

    fn handshake(&mut self, connection: &mut UdsFrameConnection) -> Result<(), LocalSessionError> {
        let hello = connection.receive()?.ok_or(LocalSessionError::PeerClosed)?;
        let stream_id = hello.stream_id;
        let request_id = hello.request_id;
        if hello.kind != FrameKind::Hello || stream_id != 0 || !hello.payload.is_empty() {
            self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::UnexpectedFrame,
            )?;
            return Err(LocalSessionError::InvalidHandshake);
        }
        connection.send(FrameKind::Welcome, 0, request_id, b"")?;
        Ok(())
    }

    fn serve_structure_request(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let result = match decode_local_structure_request(&request_buffer) {
            Ok(
                LocalStructureRequest::Get(_)
                | LocalStructureRequest::Set(_)
                | LocalStructureRequest::Ttl(_),
            ) if self.active_transaction.is_some() => self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionActive,
            ),
            Ok(LocalStructureRequest::Get(key)) => {
                self.serve_structure_get(connection, stream_id, request_id, maximum_payload, key)
            }
            Ok(LocalStructureRequest::Set(request)) => self.serve_structure_set(
                connection,
                stream_id,
                request_id,
                maximum_payload,
                request,
            ),
            Ok(LocalStructureRequest::Ttl(key)) => {
                self.serve_structure_ttl(connection, stream_id, request_id, maximum_payload, key)
            }
            Ok(LocalStructureRequest::TransactionSet(request)) => self
                .serve_transaction_structure_set(
                    connection,
                    stream_id,
                    request_id,
                    maximum_payload,
                    request,
                ),
            Err(error) => self.send_failure(
                connection,
                stream_id,
                request_id,
                codec_failure_code(&error),
            ),
        };
        self.request_buffer = request_buffer;
        result
    }

    fn serve_structure_get(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        key: &[u8],
    ) -> Result<(), LocalSessionError> {
        let logical_time_micros = self.clock.logical_time_micros();
        let Ok(value) = self.database.get_latest_structure(key, logical_time_micros) else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::EngineFailure,
            );
        };
        let Ok(response) =
            encode_local_value(&mut self.response_buffer, value.as_deref(), maximum_payload)
        else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        };
        connection.send(FrameKind::Value, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_structure_set(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalStructureSetRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_COMMIT_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let logical_time_micros = self.clock.logical_time_micros();
        let expires_at_micros = match request.relative_ttl_micros {
            Some(relative) => match logical_time_micros.checked_add(relative) {
                Some(absolute) => Some(absolute),
                None => {
                    return self.send_failure(
                        connection,
                        stream_id,
                        request_id,
                        LocalFailureCode::ExpiryOverflow,
                    );
                }
            },
            None => None,
        };
        let Ok(mut transaction) = self.database.begin(logical_time_micros, request.durability)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        if transaction
            .set(
                request.key.to_vec(),
                request.value.to_vec(),
                expires_at_micros,
            )
            .is_err()
        {
            drop(transaction);
            return self.send_engine_failure(connection, stream_id, request_id);
        }
        let Ok(receipt) = transaction.commit() else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let local_receipt = LocalStructureCommitReceipt {
            transaction_id: receipt.transaction_id,
            commit_csn: receipt.commit_csn,
            durability: receipt.durability,
        };
        let Ok(response) =
            encode_local_structure_commit_receipt(&mut self.response_buffer, local_receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        debug_assert!(response.len() <= maximum_payload);
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_transaction_structure_set(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalTransactionStructureSetRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let receipt = match self.stage_transaction_structure_set(request) {
            Ok(receipt) => receipt,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) =
            encode_local_transaction_stage_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn stage_transaction_structure_set(
        &mut self,
        request: LocalTransactionStructureSetRequest<'_>,
    ) -> Result<LocalTransactionStageReceipt, LocalFailureCode> {
        let Self {
            database,
            active_transaction,
            ..
        } = self;
        let active = active_transaction_for_handle(active_transaction, request.handle)?;
        ensure_transaction_stage_capacity(active.staged_operations)?;
        let expires_at_micros = request.relative_ttl_micros.map_or(Ok(None), |relative| {
            active
                .logical_time_micros
                .checked_add(relative)
                .map(Some)
                .ok_or(LocalFailureCode::ExpiryOverflow)
        })?;
        database
            .stage_delta_set(
                &mut active.batch,
                request.key.to_vec(),
                request.value.to_vec(),
                expires_at_micros,
            )
            .map_err(|_| LocalFailureCode::EngineFailure)?;
        let operation_ordinal = advance_staged_operations(active)?;
        Ok(LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Structure,
            handle: request.handle,
            operation_ordinal,
            rows_affected: 1,
        })
    }

    fn serve_structure_ttl(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        key: &[u8],
    ) -> Result<(), LocalSessionError> {
        let logical_time_micros = self.clock.logical_time_micros();
        let Ok(ttl) = self.database.ttl_latest_structure(key, logical_time_micros) else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let local_ttl = match ttl {
            Ttl::Missing => LocalTtlValue::Missing,
            Ttl::Persistent => LocalTtlValue::Persistent,
            Ttl::RemainingMicros(value) => LocalTtlValue::RemainingMicros(value),
        };
        let Ok(response) = encode_local_ttl(&mut self.response_buffer, local_ttl) else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        if response.len() > maximum_payload {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        connection.send(FrameKind::Value, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_search_request(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let result = if let Some(opcode) = request_buffer.get(1).copied()
            && matches!(
                opcode,
                TRANSACTION_INDEX_DOCUMENT_OPCODE
                    | TRANSACTION_REPLACE_DOCUMENT_OPCODE
                    | TRANSACTION_DELETE_DOCUMENT_OPCODE
            ) {
            match opcode {
                TRANSACTION_INDEX_DOCUMENT_OPCODE => {
                    match decode_local_transaction_index_document(&request_buffer) {
                        Ok(request) => self.serve_transaction_index_document(
                            connection,
                            stream_id,
                            request_id,
                            maximum_payload,
                            request,
                        ),
                        Err(_) => self.send_failure(
                            connection,
                            stream_id,
                            request_id,
                            LocalFailureCode::InvalidRequest,
                        ),
                    }
                }
                TRANSACTION_REPLACE_DOCUMENT_OPCODE => {
                    match decode_local_transaction_replace_document(&request_buffer) {
                        Ok(request) => self.serve_transaction_replace_document(
                            connection,
                            stream_id,
                            request_id,
                            maximum_payload,
                            request,
                        ),
                        Err(_) => self.send_failure(
                            connection,
                            stream_id,
                            request_id,
                            LocalFailureCode::InvalidRequest,
                        ),
                    }
                }
                TRANSACTION_DELETE_DOCUMENT_OPCODE => {
                    match decode_local_transaction_delete_document(&request_buffer) {
                        Ok(request) => self.serve_transaction_delete_document(
                            connection,
                            stream_id,
                            request_id,
                            maximum_payload,
                            request,
                        ),
                        Err(_) => self.send_failure(
                            connection,
                            stream_id,
                            request_id,
                            LocalFailureCode::InvalidRequest,
                        ),
                    }
                }
                _ => unreachable!("transaction search opcode was prevalidated"),
            }
        } else if self.active_transaction.is_some() {
            self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionActive,
            )
        } else {
            match decode_local_search_match(&request_buffer) {
                Ok(request) => self.serve_search_match(
                    connection,
                    stream_id,
                    request_id,
                    maximum_payload,
                    request,
                ),
                Err(_) => self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::InvalidRequest,
                ),
            }
        };
        self.request_buffer = request_buffer;
        result
    }

    fn serve_search_match(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalSearchMatchRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_SEARCH_RESULTS_HEADER_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let Ok((visible_csn, hits)) =
            self.database
                .match_latest_text_with_csn(request.index, request.query, request.limit)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let response = match encode_local_search_match_results(
            &mut self.response_buffer,
            visible_csn,
            &hits,
            maximum_payload,
        ) {
            Ok(response) => response,
            Err(LocalSearchCodecError::PayloadTooLarge) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::ResponseTooLarge,
                );
            }
            Err(_) => {
                return self.send_engine_failure(connection, stream_id, request_id);
            }
        };
        connection.send(FrameKind::Value, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_transaction_index_document(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalTransactionIndexDocumentRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let receipt = match self.stage_transaction_index_document(request) {
            Ok(receipt) => receipt,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) =
            encode_local_transaction_stage_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn stage_transaction_index_document(
        &mut self,
        request: LocalTransactionIndexDocumentRequest<'_>,
    ) -> Result<LocalTransactionStageReceipt, LocalFailureCode> {
        let Self {
            database,
            active_transaction,
            ..
        } = self;
        let active = active_transaction_for_handle(active_transaction, request.handle)?;
        ensure_transaction_stage_capacity(active.staged_operations)?;
        database
            .stage_delta_index_document(
                &mut active.batch,
                request.index,
                request.document_id.to_vec(),
                request.text.to_owned(),
            )
            .map_err(|_| LocalFailureCode::EngineFailure)?;
        let operation_ordinal = advance_staged_operations(active)?;
        Ok(LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Search,
            handle: request.handle,
            operation_ordinal,
            rows_affected: 1,
        })
    }

    fn serve_transaction_replace_document(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalTransactionReplaceDocumentRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let receipt = match self.stage_transaction_replace_document(request) {
            Ok(receipt) => receipt,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) =
            encode_local_transaction_stage_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn stage_transaction_replace_document(
        &mut self,
        request: LocalTransactionReplaceDocumentRequest<'_>,
    ) -> Result<LocalTransactionStageReceipt, LocalFailureCode> {
        let Self {
            database,
            active_transaction,
            ..
        } = self;
        let active = active_transaction_for_handle(active_transaction, request.handle)?;
        ensure_transaction_stage_capacity(active.staged_operations)?;
        database
            .stage_delta_replace_document(
                &mut active.batch,
                request.index,
                request.document_id.to_vec(),
                request.text.to_owned(),
            )
            .map_err(|_| LocalFailureCode::EngineFailure)?;
        let operation_ordinal = advance_staged_operations(active)?;
        Ok(LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Search,
            handle: request.handle,
            operation_ordinal,
            rows_affected: 1,
        })
    }

    fn serve_transaction_delete_document(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        request: LocalTransactionDeleteDocumentRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let receipt = match self.stage_transaction_delete_document(request) {
            Ok(receipt) => receipt,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) =
            encode_local_transaction_stage_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn stage_transaction_delete_document(
        &mut self,
        request: LocalTransactionDeleteDocumentRequest<'_>,
    ) -> Result<LocalTransactionStageReceipt, LocalFailureCode> {
        let Self {
            database,
            active_transaction,
            ..
        } = self;
        let active = active_transaction_for_handle(active_transaction, request.handle)?;
        ensure_transaction_stage_capacity(active.staged_operations)?;
        database
            .stage_delta_delete_document(
                &mut active.batch,
                request.index,
                request.document_id.to_vec(),
            )
            .map_err(|_| LocalFailureCode::EngineFailure)?;
        let operation_ordinal = advance_staged_operations(active)?;
        Ok(LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Search,
            handle: request.handle,
            operation_ordinal,
            rows_affected: 1,
        })
    }

    fn serve_sql_prepare_request(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let result = match decode_local_sql_prepare(&request_buffer) {
            Ok(statement) => self.serve_sql_prepare(
                connection,
                stream_id,
                request_id,
                maximum_payload,
                statement,
            ),
            Err(_) => self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::InvalidRequest,
            ),
        };
        self.request_buffer = request_buffer;
        result
    }

    fn serve_sql_prepare(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        statement: &str,
    ) -> Result<(), LocalSessionError> {
        if maximum_payload < LOCAL_SQL_PREPARED_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let (receipt, retained) = match self.prepare_local_sql_plan(statement) {
            Ok(prepared) => prepared,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) = encode_local_sql_prepared_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        self.prepared_statements
            .insert(receipt.plan_id.get(), retained);
        self.next_plan_id = receipt
            .plan_id
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new);
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn prepare_local_sql_plan(
        &self,
        statement: &str,
    ) -> Result<(LocalSqlPreparedReceipt, RetainedSqlPlan), LocalFailureCode> {
        if self.prepared_statements.len() >= MAX_LOCAL_PREPARED_STATEMENTS {
            return Err(LocalFailureCode::SqlResourceLimit);
        }
        let plan_id = self
            .next_plan_id
            .ok_or(LocalFailureCode::SqlResourceLimit)?;
        let prepared = self
            .database
            .prepare_sql_latest(statement)
            .map_err(|error| prepare_sql_failure_code(&error))?;
        let parameter_count = prepared.parameter_count();
        if parameter_count > MAX_LOCAL_SQL_PARAMETERS {
            return Err(LocalFailureCode::SqlResourceLimit);
        }
        let maximum_rows = prepared
            .maximum_result_rows()
            .filter(|maximum| *maximum <= MAX_LOCAL_SQL_ROWS)
            .ok_or(LocalFailureCode::SqlResourceLimit)?;
        let schema = prepared
            .result_schema()
            .map_err(|_| LocalFailureCode::EngineFailure)?;
        if schema.len() > MAX_LOCAL_SQL_COLUMNS {
            return Err(LocalFailureCode::SqlResourceLimit);
        }
        let columns = schema
            .into_iter()
            .map(|(name, logical_type)| LocalSqlColumn { name, logical_type })
            .collect::<Vec<_>>();
        let receipt = LocalSqlPreparedReceipt {
            plan_id,
            catalog_version: prepared.catalog_version(),
            parameter_count: bounded_sql_count(parameter_count)?,
            column_count: bounded_sql_count(columns.len())?,
            maximum_rows: bounded_sql_count(maximum_rows)?,
        };
        Ok((
            receipt,
            RetainedSqlPlan {
                prepared,
                columns,
                maximum_rows,
            },
        ))
    }

    fn serve_sql_execute_request(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        if self.request_buffer.get(1) == Some(&EXECUTE_TRANSACTION_DML_OPCODE) {
            if maximum_payload < LOCAL_TRANSACTION_STAGE_RECEIPT_SIZE {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::ResponseTooLarge,
                );
            }
            let request_buffer = std::mem::take(&mut self.request_buffer);
            let result = match decode_local_transaction_sql_dml(&request_buffer) {
                Ok(request) => {
                    self.serve_transaction_sql_dml(connection, stream_id, request_id, &request)
                }
                Err(_) => self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::InvalidRequest,
                ),
            };
            self.request_buffer = request_buffer;
            return result;
        }
        if self.active_transaction.is_some() {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionActive,
            );
        }
        if maximum_payload < LOCAL_SQL_ROWS_HEADER_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let result = match decode_local_sql_execute(&request_buffer) {
            Ok(request) => self.serve_sql_execute(
                connection,
                stream_id,
                request_id,
                maximum_payload,
                request.plan_id,
                &request.parameters,
            ),
            Err(_) => self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::InvalidRequest,
            ),
        };
        self.request_buffer = request_buffer;
        result
    }

    fn serve_transaction_sql_dml(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        request: &LocalTransactionSqlDmlRequest<'_>,
    ) -> Result<(), LocalSessionError> {
        let receipt = match self.stage_transaction_sql_dml(request) {
            Ok(receipt) => receipt,
            Err(code) => return self.send_failure(connection, stream_id, request_id, code),
        };
        let Ok(response) =
            encode_local_transaction_stage_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn stage_transaction_sql_dml(
        &mut self,
        request: &LocalTransactionSqlDmlRequest<'_>,
    ) -> Result<LocalTransactionStageReceipt, LocalFailureCode> {
        let Self {
            database,
            active_transaction,
            ..
        } = self;
        let active = active_transaction_for_handle(active_transaction, request.handle)?;
        ensure_transaction_stage_capacity(active.staged_operations)?;
        let result = database
            .stage_delta_sql_dml(&mut active.batch, request.statement, &request.parameters)
            .map_err(|error| execute_sql_failure_code(&error))?;
        let SqlResult::Command {
            rows_affected,
            object_id: None,
        } = result
        else {
            return Err(LocalFailureCode::EngineFailure);
        };
        let operation_ordinal = advance_staged_operations(active)?;
        Ok(LocalTransactionStageReceipt {
            engine: LocalTransactionEngine::Relational,
            handle: request.handle,
            operation_ordinal,
            rows_affected,
        })
    }

    fn serve_sql_execute(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
        plan_id: NonZeroU64,
        parameters: &[crate::SqlValue],
    ) -> Result<(), LocalSessionError> {
        let Some(retained) = self.prepared_statements.get(&plan_id.get()) else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::UnknownPrepared,
            );
        };
        if parameters.len() != retained.prepared.parameter_count() {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::SqlParameters,
            );
        }
        let (visible_csn, result) = match self
            .database
            .execute_prepared_latest_with_csn(&retained.prepared, parameters)
        {
            Ok(result) => result,
            Err(error) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    execute_sql_failure_code(&error),
                );
            }
        };
        let SqlResult::Rows { columns, rows } = result else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        if columns.len() != retained.columns.len()
            || columns
                .iter()
                .zip(&retained.columns)
                .any(|(actual, expected)| actual != &expected.name)
            || rows.len() > retained.maximum_rows
        {
            return self.send_engine_failure(connection, stream_id, request_id);
        }
        let response = match encode_local_sql_rows(
            &mut self.response_buffer,
            visible_csn,
            &retained.columns,
            &rows,
            maximum_payload,
        ) {
            Ok(response) => response,
            Err(crate::LocalSqlCodecError::PayloadTooLarge) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::ResponseTooLarge,
                );
            }
            Err(_) => return self.send_engine_failure(connection, stream_id, request_id),
        };
        connection.send(FrameKind::Value, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_transaction_begin(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        if self.active_transaction.is_some() {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionActive,
            );
        }
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let durability = match decode_local_transaction_begin(&request_buffer) {
            Ok(durability) => durability,
            Err(crate::LocalTransactionCodecError::UnsupportedDurability(_)) => {
                self.request_buffer = request_buffer;
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::UnsupportedDurability,
                );
            }
            Err(_) => {
                self.request_buffer = request_buffer;
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::InvalidRequest,
                );
            }
        };
        self.request_buffer = request_buffer;
        if maximum_payload < LOCAL_TRANSACTION_BEGIN_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let Some(handle) = self.next_transaction_handle else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionResourceLimit,
            );
        };
        let logical_time_micros = self.clock.logical_time_micros();
        let Ok(batch) = self
            .database
            .begin_optimistic_delta(logical_time_micros, durability)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let receipt = LocalTransactionBeginReceipt {
            durability,
            handle,
            read_csn: batch.read_csn(),
            logical_time_micros,
        };
        let Ok(response) =
            encode_local_transaction_begin_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        self.next_transaction_handle = handle.get().checked_add(1).and_then(NonZeroU64::new);
        self.active_transaction = Some(ActiveLocalTransaction {
            handle,
            batch,
            durability,
            logical_time_micros,
            staged_operations: 0,
        });
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_transaction_commit(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let request = decode_local_transaction_commit(&request_buffer);
        self.request_buffer = request_buffer;
        let Ok((handle, expected_operations)) = request else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::InvalidRequest,
            );
        };
        if maximum_payload < LOCAL_TRANSACTION_COMMIT_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let Some(active) = self.active_transaction.as_ref() else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionInactive,
            );
        };
        if active.handle != handle {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionMismatch,
            );
        }
        if active.staged_operations == 0 || expected_operations == 0 {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionEmpty,
            );
        }
        if active.staged_operations != expected_operations {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionMismatch,
            );
        }
        let Some(active) = self.active_transaction.take() else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let Ok(staged_operations) = u32::try_from(active.staged_operations) else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let commit = match self.database.commit_optimistic(active.batch) {
            Ok(commit) => commit,
            Err(NativeRuntimeError::WriteConflict(_)) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::TransactionConflict,
                );
            }
            Err(_) => return self.send_engine_failure(connection, stream_id, request_id),
        };
        let receipt = LocalTransactionCommitReceipt {
            durability: active.durability,
            handle,
            transaction_id: commit.transaction_id,
            commit_csn: commit.commit_csn,
            staged_operations,
        };
        let Ok(response) =
            encode_local_transaction_commit_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn serve_transaction_rollback(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let request_buffer = std::mem::take(&mut self.request_buffer);
        let request = decode_local_transaction_rollback(&request_buffer);
        self.request_buffer = request_buffer;
        let Ok(handle) = request else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::InvalidRequest,
            );
        };
        if maximum_payload < LOCAL_TRANSACTION_ROLLBACK_RECEIPT_SIZE {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::ResponseTooLarge,
            );
        }
        let Some(active) = self.active_transaction.as_ref() else {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionInactive,
            );
        };
        if active.handle != handle {
            return self.send_failure(
                connection,
                stream_id,
                request_id,
                LocalFailureCode::TransactionMismatch,
            );
        }
        let Some(active) = self.active_transaction.take() else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        let receipt = LocalTransactionRollbackReceipt {
            handle,
            discarded_operations: active.staged_operations,
        };
        let Ok(response) =
            encode_local_transaction_rollback_receipt(&mut self.response_buffer, receipt)
        else {
            return self.send_engine_failure(connection, stream_id, request_id);
        };
        drop(active);
        connection.send(FrameKind::Receipt, stream_id, request_id, response)?;
        Ok(())
    }

    fn send_engine_failure(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
    ) -> Result<(), LocalSessionError> {
        self.send_failure(
            connection,
            stream_id,
            request_id,
            LocalFailureCode::EngineFailure,
        )
    }

    fn send_failure(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        code: LocalFailureCode,
    ) -> Result<(), LocalSessionError> {
        let payload = encode_local_failure(&mut self.response_buffer, code);
        connection.send(FrameKind::Failure, stream_id, request_id, payload)?;
        Ok(())
    }
}

fn active_transaction_for_handle(
    active_transaction: &mut Option<ActiveLocalTransaction>,
    handle: NonZeroU64,
) -> Result<&mut ActiveLocalTransaction, LocalFailureCode> {
    let active = active_transaction
        .as_mut()
        .ok_or(LocalFailureCode::TransactionInactive)?;
    if active.handle != handle {
        return Err(LocalFailureCode::TransactionMismatch);
    }
    Ok(active)
}

fn ensure_transaction_stage_capacity(staged_operations: u64) -> Result<(), LocalFailureCode> {
    let maximum = u64::try_from(MAX_LOCAL_TRANSACTION_OPERATIONS)
        .map_err(|_| LocalFailureCode::TransactionResourceLimit)?;
    if staged_operations >= maximum {
        return Err(LocalFailureCode::TransactionResourceLimit);
    }
    Ok(())
}

fn advance_staged_operations(active: &mut ActiveLocalTransaction) -> Result<u64, LocalFailureCode> {
    ensure_transaction_stage_capacity(active.staged_operations)?;
    active.staged_operations = active
        .staged_operations
        .checked_add(1)
        .ok_or(LocalFailureCode::TransactionResourceLimit)?;
    Ok(active.staged_operations)
}

fn codec_failure_code(error: &LocalOperationCodecError) -> LocalFailureCode {
    match error {
        LocalOperationCodecError::KeyTooLarge => LocalFailureCode::KeyTooLarge,
        LocalOperationCodecError::UnsupportedDurability(_) => {
            LocalFailureCode::UnsupportedDurability
        }
        _ => LocalFailureCode::InvalidRequest,
    }
}

fn prepare_sql_failure_code(error: &SqlError) -> LocalFailureCode {
    match error {
        SqlError::Runtime(_) | SqlError::InvalidCatalogObject => LocalFailureCode::EngineFailure,
        _ => LocalFailureCode::SqlInvalid,
    }
}

fn execute_sql_failure_code(error: &SqlError) -> LocalFailureCode {
    match error {
        SqlError::CatalogChanged => LocalFailureCode::SqlCatalogChanged,
        SqlError::ParameterMismatch
        | SqlError::TypeMismatch
        | SqlError::NullViolation
        | SqlError::InvalidPrimaryKey
        | SqlError::InvalidSecondaryIndexRange => LocalFailureCode::SqlParameters,
        SqlError::Runtime(_) | SqlError::InvalidStoredRow | SqlError::InvalidCatalogObject => {
            LocalFailureCode::EngineFailure
        }
        _ => LocalFailureCode::SqlInvalid,
    }
}

fn bounded_sql_count(count: usize) -> Result<u32, LocalFailureCode> {
    u32::try_from(count).map_err(|_| LocalFailureCode::SqlResourceLimit)
}

#[cfg(test)]
mod tests {
    use super::{execute_sql_failure_code, prepare_sql_failure_code};
    use crate::{LocalFailureCode, SqlError};

    #[test]
    fn sql_errors_map_to_stable_local_failure_classes() {
        assert_eq!(
            prepare_sql_failure_code(&SqlError::InvalidCatalogObject),
            LocalFailureCode::EngineFailure
        );
        assert_eq!(
            execute_sql_failure_code(&SqlError::CatalogChanged),
            LocalFailureCode::SqlCatalogChanged
        );
        assert_eq!(
            execute_sql_failure_code(&SqlError::InvalidSecondaryIndexRange),
            LocalFailureCode::SqlParameters
        );
        assert_eq!(
            execute_sql_failure_code(&SqlError::InvalidStoredRow),
            LocalFailureCode::EngineFailure
        );
    }
}
