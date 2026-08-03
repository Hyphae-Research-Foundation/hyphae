// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

use crate::{
    FrameKind, LOCAL_OPERATION_HEADER_SIZE, LocalFailureCode, LocalOperationCodecError,
    LocalTransportError, NativeDatabase, NativeSchedulerClock, UdsFrameConnection,
    decode_local_structure_get, encode_local_failure, encode_local_value,
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

/// Serial local session exposing one physical scalar `GET`.
pub struct LocalStructureGetSession<'database, Clock: NativeSchedulerClock + ?Sized> {
    database: &'database NativeDatabase,
    clock: &'database Clock,
    request_buffer: Vec<u8>,
    response_buffer: Vec<u8>,
    echo_buffer: Vec<u8>,
}

impl<'database, Clock: NativeSchedulerClock + ?Sized> LocalStructureGetSession<'database, Clock> {
    /// Creates one reusable serial session over the supplied database and
    /// logical-time authority.
    pub fn new(database: &'database NativeDatabase, clock: &'database Clock) -> Self {
        Self {
            database,
            clock,
            request_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            response_buffer: Vec::with_capacity(LOCAL_OPERATION_HEADER_SIZE),
            echo_buffer: Vec::new(),
        }
    }

    /// Serves one minimal `HELLO`, `PING`/`STRUCTURE GET`, `CLOSE` session.
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
                    self.serve_structure_get(connection, stream_id, request_id, maximum_payload)?;
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

    fn serve_structure_get(
        &mut self,
        connection: &mut UdsFrameConnection,
        stream_id: u32,
        request_id: u64,
        maximum_payload: usize,
    ) -> Result<(), LocalSessionError> {
        let key = match decode_local_structure_get(&self.request_buffer) {
            Ok(key) => key,
            Err(LocalOperationCodecError::KeyTooLarge) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::KeyTooLarge,
                );
            }
            Err(_) => {
                return self.send_failure(
                    connection,
                    stream_id,
                    request_id,
                    LocalFailureCode::InvalidRequest,
                );
            }
        };
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
