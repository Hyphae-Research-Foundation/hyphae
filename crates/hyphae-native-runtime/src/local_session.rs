// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

use crate::{
    FrameKind, LOCAL_COMMIT_RECEIPT_SIZE, LOCAL_OPERATION_HEADER_SIZE, LocalFailureCode,
    LocalOperationCodecError, LocalStructureCommitReceipt, LocalStructureRequest,
    LocalStructureSetRequest, LocalTransportError, LocalTtlValue, NativeDatabase,
    NativeSchedulerClock, Ttl, UdsFrameConnection, decode_local_structure_request,
    encode_local_failure, encode_local_structure_commit_receipt, encode_local_ttl,
    encode_local_value,
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

/// Serial local session exposing scalar `GET`, `SET`, and `TTL`.
pub struct LocalStructureSession<'database, Clock: NativeSchedulerClock + ?Sized> {
    database: &'database mut NativeDatabase,
    clock: &'database Clock,
    request_buffer: Vec<u8>,
    response_buffer: Vec<u8>,
    echo_buffer: Vec<u8>,
}

impl<'database, Clock: NativeSchedulerClock + ?Sized> LocalStructureSession<'database, Clock> {
    /// Creates one reusable serial session over the supplied database and
    /// logical-time authority.
    pub fn new(database: &'database mut NativeDatabase, clock: &'database Clock) -> Self {
        Self {
            database,
            clock,
            request_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            response_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            echo_buffer: Vec::new(),
        }
    }

    /// Serves one minimal `HELLO`, `PING`/structure, `CLOSE` session.
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
                FrameKind::Close => {
                    self.echo_buffer.clear();
                    self.echo_buffer.extend_from_slice(frame.payload);
                    connection.send(FrameKind::Close, stream_id, request_id, &self.echo_buffer)?;
                    return Ok(());
                }
                _ => self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::UnexpectedFrame,
                )?,
            }
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

fn codec_failure_code(error: &LocalOperationCodecError) -> LocalFailureCode {
    match error {
        LocalOperationCodecError::KeyTooLarge => LocalFailureCode::KeyTooLarge,
        LocalOperationCodecError::UnsupportedDurability(_) => {
            LocalFailureCode::UnsupportedDurability
        }
        _ => LocalFailureCode::InvalidRequest,
    }
}
