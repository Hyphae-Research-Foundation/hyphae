// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::atomic::{AtomicU64, Ordering};

use hyphae_native_product::{ProductOperation, ProductResponse};
use hyphae_native_protocol::{
    API_KEY_AUTH_TRAILER_BYTES, AsyncFrameIo, FrameKind, Hello, ProtocolCapabilities,
    ProvisionalStream, decode_end, decode_failure, decode_welcome, encode_authenticated_hello,
    encode_cancel, encode_hello, encode_product_request, encode_window_update,
};
use tokio::sync::Mutex;

#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;

use super::{ClientError, RequestOptions, ResponseFuture, Transport};

const CLIENT_IDENTITY: &str = "hyphae-rust-sdk-v2";
#[cfg(windows)]
const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Exact `HYPHLCL1` transport over `AF_UNIX` or Windows named pipes.
pub struct LocalTransport {
    endpoint: String,
    client_identity: String,
    api_key: Option<LocalApiKey>,
    state: Mutex<Option<Connection>>,
    next_request_id: AtomicU64,
}

struct LocalApiKey {
    bytes: Vec<u8>,
}

impl LocalApiKey {
    fn new(value: impl AsRef<str>) -> Result<Self, ClientError> {
        let bytes = value.as_ref().as_bytes();
        if bytes.len() != API_KEY_AUTH_TRAILER_BYTES {
            return Err(ClientError::Local(
                "local API-key credential is invalid".to_owned(),
            ));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
        })
    }

    fn expose(&self) -> Result<&str, ClientError> {
        std::str::from_utf8(&self.bytes)
            .map_err(|_| ClientError::Local("local API-key credential is invalid".to_owned()))
    }
}

impl Drop for LocalApiKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

impl std::fmt::Debug for LocalTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalTransport")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

struct Connection {
    stream: interprocess::local_socket::tokio::Stream,
    codec: AsyncFrameIo,
    maximum_response_bytes: usize,
    next_stream_id: u32,
}

#[allow(clippy::missing_errors_doc)]
impl LocalTransport {
    /// Configures one platform-local endpoint path or named-pipe identity.
    pub fn new(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        let endpoint = normalize_endpoint(endpoint.into())?;
        if endpoint.is_empty() {
            return Err(ClientError::Local("local endpoint is empty".to_owned()));
        }
        Ok(Self {
            endpoint,
            client_identity: CLIENT_IDENTITY.to_owned(),
            api_key: None,
            state: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
        })
    }

    /// Overrides the bounded identity sent in the native-local handshake.
    pub fn client_identity(mut self, identity: impl Into<String>) -> Result<Self, ClientError> {
        let identity = identity.into();
        if identity.is_empty() || identity.len() > 4 * 1024 {
            return Err(ClientError::Local(
                "local client identity is invalid".to_owned(),
            ));
        }
        self.client_identity = identity;
        Ok(self)
    }

    /// Requires durable Native API-key authentication during local handshake.
    pub fn api_key(mut self, api_key: impl AsRef<str>) -> Result<Self, ClientError> {
        self.api_key = Some(LocalApiKey::new(api_key)?);
        Ok(self)
    }

    async fn connect(&self, handshake_id: u64) -> Result<Connection, ClientError> {
        use interprocess::local_socket::tokio::prelude::*;

        #[cfg(unix)]
        let name = self
            .endpoint
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .map_err(|error| ClientError::Local(error.to_string()))?;
        #[cfg(windows)]
        let name = self
            .endpoint
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .map_err(|error| ClientError::Local(error.to_string()))?;
        let stream = interprocess::local_socket::tokio::Stream::connect(name)
            .await
            .map_err(|error| ClientError::Local(error.to_string()))?;
        let mut codec = AsyncFrameIo::new(hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD)
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let mut hello = Hello {
            client_identity: self.client_identity.clone(),
            ..Hello::default()
        };
        let mut hello_payload = if let Some(api_key) = &self.api_key {
            hello.capabilities = ProtocolCapabilities::G6_AUTHENTICATED;
            hello.required_capabilities = ProtocolCapabilities::G6_AUTHENTICATED;
            encode_authenticated_hello(&hello, api_key.expose()?)
        } else {
            encode_hello(&hello)
        }
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let sent = codec
            .send(
                &mut &stream,
                FrameKind::Hello,
                0,
                handshake_id,
                &hello_payload,
            )
            .await;
        hello_payload.fill(0);
        sent.map_err(|error| ClientError::Local(error.to_string()))?;
        let welcome = codec
            .receive(&mut &stream)
            .await
            .map_err(|error| ClientError::Local(error.to_string()))?
            .ok_or_else(|| ClientError::Local("server closed during handshake".to_owned()))?;
        if welcome.kind == FrameKind::Failure {
            return Err(Box::new(
                decode_failure(&welcome.payload)
                    .map_err(|error| ClientError::Protocol(error.to_string()))?,
            )
            .into());
        }
        if welcome.kind != FrameKind::Welcome
            || welcome.stream_id != 0
            || welcome.request_id != handshake_id
        {
            return Err(ClientError::Protocol(
                "server returned a mismatched welcome frame".to_owned(),
            ));
        }
        let welcome = decode_welcome(&welcome.payload)
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        if self.api_key.is_some()
            && !welcome
                .capabilities
                .contains(ProtocolCapabilities::API_KEY_AUTH)
        {
            return Err(ClientError::Protocol(
                "server downgraded local API-key authentication".to_owned(),
            ));
        }
        codec = AsyncFrameIo::new(
            usize::try_from(welcome.maximum_frame_payload)
                .map_err(|_| ClientError::Protocol("invalid negotiated frame limit".to_owned()))?,
        )
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let maximum_response_bytes = usize::try_from(welcome.initial_window)
            .map_err(|_| ClientError::Protocol("invalid negotiated window".to_owned()))?;
        Ok(Connection {
            stream,
            codec,
            maximum_response_bytes,
            next_stream_id: 1,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_inner(
        &self,
        operation: ProductOperation,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        let request_id = options
            .request_id
            .unwrap_or_else(|| self.next_request_id.fetch_add(1, Ordering::Relaxed));
        if request_id == 0 {
            return Err(ClientError::Protocol(
                "request identity must be nonzero".to_owned(),
            ));
        }
        if options.cancellation.is_cancelled() {
            return Err(product_error(
                hyphae_native_product::ProductErrorCode::Cancelled,
                request_id,
            ));
        }
        let encoded = encode_product_request(&hyphae_native_protocol::WireRequest {
            operation,
            logical_time_micros: options.logical_time_micros,
            deadline_micros: options.deadline_micros,
            idempotency_token: options.idempotency_token,
            limits: options.limits,
            durability: options.durability,
        })
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let mut state = self.state.lock().await;
        if state.is_none() {
            let handshake_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
            *state = Some(self.connect(handshake_id.max(1)).await?);
        }
        let connection = state.as_mut().ok_or_else(|| {
            ClientError::Local("local connection state is unavailable".to_owned())
        })?;
        let stream_id = connection.next_stream_id;
        connection.next_stream_id = connection
            .next_stream_id
            .checked_add(1)
            .filter(|value| *value != 0)
            .unwrap_or(1);
        let kind = operation_frame_kind(&encoded)?;
        connection
            .codec
            .send(
                &mut &connection.stream,
                kind,
                stream_id,
                request_id,
                &encoded,
            )
            .await
            .map_err(|error| ClientError::Local(error.to_string()))?;

        let maximum = options
            .limits
            .max_response_bytes
            .min(hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES);
        let mut provisional = ProvisionalStream::new();
        let mut credited = 0_u64;
        loop {
            if options.cancellation.is_cancelled() {
                connection
                    .codec
                    .send(
                        &mut &connection.stream,
                        FrameKind::Cancel,
                        stream_id,
                        request_id,
                        &encode_cancel(1),
                    )
                    .await
                    .map_err(|error| ClientError::Local(error.to_string()))?;
                return Err(product_error(
                    hyphae_native_product::ProductErrorCode::Cancelled,
                    request_id,
                ));
            }
            let mut reader = &connection.stream;
            let received = tokio::select! {
                result = connection.codec.receive(&mut reader) => {
                    result.map_err(|error| ClientError::Local(error.to_string()))?
                }
                () = options.cancellation.cancelled() => {
                    connection.codec.send(
                        &mut &connection.stream,
                        FrameKind::Cancel,
                        stream_id,
                        request_id,
                        &encode_cancel(1),
                    ).await.map_err(|error| ClientError::Local(error.to_string()))?;
                    *state = None;
                    return Err(product_error(
                        hyphae_native_product::ProductErrorCode::Cancelled,
                        request_id,
                    ));
                }
                () = wait_deadline(options.deadline_micros) => {
                    connection.codec.send(
                        &mut &connection.stream,
                        FrameKind::Cancel,
                        stream_id,
                        request_id,
                        &encode_cancel(2),
                    ).await.map_err(|error| ClientError::Local(error.to_string()))?;
                    *state = None;
                    return Err(product_error(
                        hyphae_native_product::ProductErrorCode::DeadlineExceeded,
                        request_id,
                    ));
                }
            };
            let Some(frame) = received else {
                *state = None;
                return Err(ClientError::Local(
                    "server closed before stream completion".to_owned(),
                ));
            };
            if frame.request_id != request_id || frame.stream_id != stream_id {
                *state = None;
                return Err(ClientError::Protocol(
                    "serial client received a mismatched response".to_owned(),
                ));
            }
            match frame.kind {
                FrameKind::Data => {
                    provisional
                        .push(&frame.payload, maximum)
                        .map_err(|error| ClientError::Protocol(error.to_string()))?;
                    credited = credited.saturating_add(
                        u64::try_from(frame.payload.len())
                            .map_err(|_| ClientError::Protocol("response too large".to_owned()))?,
                    );
                    if credited >= u64::try_from(connection.maximum_response_bytes / 2).unwrap_or(1)
                    {
                        connection
                            .codec
                            .send(
                                &mut &connection.stream,
                                FrameKind::WindowUpdate,
                                stream_id,
                                request_id,
                                &encode_window_update(credited)
                                    .map_err(|error| ClientError::Protocol(error.to_string()))?,
                            )
                            .await
                            .map_err(|error| ClientError::Local(error.to_string()))?;
                        credited = 0;
                    }
                }
                FrameKind::End => {
                    let bytes = provisional
                        .complete(
                            decode_end(&frame.payload)
                                .map_err(|error| ClientError::Protocol(error.to_string()))?,
                        )
                        .map_err(|error| ClientError::Protocol(error.to_string()))?;
                    return hyphae_native_protocol::decode_product_response(&bytes)
                        .map_err(|error| ClientError::Protocol(error.to_string()));
                }
                FrameKind::Failure => {
                    return Err(Box::new(
                        decode_failure(&frame.payload)
                            .map_err(|error| ClientError::Protocol(error.to_string()))?,
                    )
                    .into());
                }
                _ => {
                    *state = None;
                    return Err(ClientError::Protocol(
                        "server returned an invalid response frame".to_owned(),
                    ));
                }
            }
        }
    }
}

fn product_error(code: hyphae_native_product::ProductErrorCode, request_id: u64) -> ClientError {
    Box::new(
        hyphae_native_product::ProductError::from_code(code)
            .with_request_id(u128::from(request_id)),
    )
    .into()
}

#[cfg_attr(unix, allow(clippy::unnecessary_wraps))]
fn normalize_endpoint(endpoint: String) -> Result<String, ClientError> {
    #[cfg(unix)]
    {
        Ok(endpoint)
    }
    #[cfg(windows)]
    {
        let bare = endpoint
            .get(..WINDOWS_PIPE_PREFIX.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(WINDOWS_PIPE_PREFIX))
            .map_or(endpoint.as_str(), |_| {
                &endpoint[WINDOWS_PIPE_PREFIX.len()..]
            });
        if bare.is_empty() || bare.starts_with(r"\\") {
            return Err(ClientError::Local(
                "Windows local endpoint must be a local named-pipe namespace".to_owned(),
            ));
        }
        Ok(bare.to_owned())
    }
}

impl Transport for LocalTransport {
    fn execute(&self, operation: ProductOperation, options: RequestOptions) -> ResponseFuture<'_> {
        Box::pin(self.execute_inner(operation, options))
    }
}

fn operation_frame_kind(encoded: &[u8]) -> Result<FrameKind, ClientError> {
    if encoded.len() < 14 {
        return Err(ClientError::Protocol(
            "encoded request is truncated".to_owned(),
        ));
    }
    Ok(match u16::from_le_bytes([encoded[12], encoded[13]]) {
        2 => FrameKind::Prepare,
        12 => FrameKind::Deallocate,
        _ => FrameKind::Execute,
    })
}

async fn wait_deadline(deadline_micros: Option<i64>) {
    let Some(deadline_micros) = deadline_micros else {
        std::future::pending::<()>().await;
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_micros();
    let deadline = u128::try_from(deadline_micros).unwrap_or(0);
    if deadline > now {
        let remaining = deadline - now;
        tokio::time::sleep(std::time::Duration::from_micros(
            u64::try_from(remaining).unwrap_or(u64::MAX),
        ))
        .await;
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn named_pipe_endpoints_normalize_to_one_bare_namespace() -> Result<(), ClientError> {
        let bare = LocalTransport::new("hyphae-test")?;
        let full = LocalTransport::new(r"\\.\pipe\hyphae-test")?;
        assert_eq!(bare.endpoint, "hyphae-test");
        assert_eq!(full.endpoint, "hyphae-test");
        assert!(LocalTransport::new(r"\\server\pipe\hyphae-test").is_err());
        Ok(())
    }
}
