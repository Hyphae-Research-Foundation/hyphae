// SPDX-License-Identifier: Apache-2.0

use std::{net::IpAddr, sync::Arc, time::Duration};

use hyphae_native_product::{ProductErrorCode, ProductOperation, ProductResponse};
use reqwest::{StatusCode, Url, header};

use super::{ClientError, RequestOptions, ResponseFuture, Transport};

const DEFAULT_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PRODUCT_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_MEDIA_TYPE_V2;
const ERROR_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_ERROR_MEDIA_TYPE_V2;

/// Comma-joined ascending offer of every protocol minor this build speaks.
fn offered_protocol_minors() -> String {
    hyphae_contracts::v2::PROTOCOL_MINORS_SUPPORTED_V2
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Binary product-envelope HTTP `/v2` transport.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    origin: Url,
    http: reqwest::Client,
    bearer_token: Option<header::HeaderValue>,
    response_bytes: usize,
    session_id: Arc<tokio::sync::Mutex<Option<String>>>,
}

#[allow(clippy::missing_errors_doc)]
impl HttpTransport {
    /// Configures one root HTTP(S) origin.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let mut origin =
            Url::parse(base_url).map_err(|error| ClientError::Http(error.to_string()))?;
        if !matches!(origin.scheme(), "http" | "https")
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !matches!(origin.path(), "" | "/")
        {
            return Err(ClientError::Http(
                "base URL must be a root HTTP(S) origin".to_owned(),
            ));
        }
        origin.set_path("/");
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ClientError::Http(error.to_string()))?;
        Ok(Self {
            origin,
            http,
            bearer_token: None,
            response_bytes: DEFAULT_RESPONSE_BYTES,
            session_id: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Adds one opaque bearer token over TLS or a canonical loopback origin.
    pub fn bearer_token(mut self, token: &str) -> Result<Self, ClientError> {
        if self.origin.scheme() == "http" && !is_loopback_origin(&self.origin) {
            return Err(ClientError::Http(
                "durable API keys require HTTPS outside loopback".to_owned(),
            ));
        }
        if token.is_empty() {
            return Err(ClientError::Http("bearer token is empty".to_owned()));
        }
        let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| ClientError::Http(error.to_string()))?;
        value.set_sensitive(true);
        self.bearer_token = Some(value);
        Ok(self)
    }

    /// Sets the strict complete response bound.
    pub fn response_bytes(mut self, response_bytes: usize) -> Result<Self, ClientError> {
        if response_bytes == 0 || response_bytes > hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES {
            return Err(ClientError::Http("invalid response byte bound".to_owned()));
        }
        self.response_bytes = response_bytes;
        Ok(self)
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_inner(
        &self,
        operation: ProductOperation,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        let request_id = options
            .request_id
            .unwrap_or_else(|| unique_request_id().max(1));
        if request_id == 0 {
            return Err(ClientError::Protocol(
                "request identity must be nonzero".to_owned(),
            ));
        }
        if options.cancellation.is_cancelled() {
            return Err(product_error(ProductErrorCode::Cancelled, request_id));
        }
        let endpoint_path = endpoint_path(&operation);
        let one_time_secret = matches!(
            operation,
            ProductOperation::SecurityApiKeyIssueSelfStart { .. }
                | ProductOperation::SecurityApiKeyIssueStart { .. }
                | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
                | ProductOperation::SecurityApiKeyRotateStart { .. }
        );
        let encoded = hyphae_native_protocol::encode_product_request_for_minor(
            &hyphae_native_protocol::WireRequest {
                operation,
                logical_time_micros: options.logical_time_micros,
                deadline_micros: options.deadline_micros,
                idempotency_token: options.idempotency_token,
                limits: options.limits,
                durability: options.durability,
            },
            hyphae_native_protocol::PROTOCOL_MINOR,
        )
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let mut endpoint = self.origin.clone();
        endpoint.set_path(endpoint_path);
        let mut request = self
            .http
            .post(endpoint)
            .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
            .header(
                header::ACCEPT,
                format!("{PRODUCT_MEDIA_TYPE}, {ERROR_MEDIA_TYPE}"),
            )
            .header(
                hyphae_contracts::v2::REQUEST_ID_HEADER_V2,
                request_id.to_string(),
            )
            .header(
                hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2,
                offered_protocol_minors(),
            )
            .body(encoded);
        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header(hyphae_contracts::v2::SESSION_ID_HEADER_V2, session_id);
        }
        if let Some(deadline) = options.deadline_micros {
            request = request.header(
                hyphae_contracts::v2::DEADLINE_HEADER_V2,
                deadline.to_string(),
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_micros();
            let deadline = u128::try_from(deadline).unwrap_or(0);
            if deadline <= now {
                return Err(product_error(
                    ProductErrorCode::DeadlineExceeded,
                    request_id,
                ));
            }
            request = request.timeout(Duration::from_micros(
                u64::try_from(deadline - now).unwrap_or(u64::MAX),
            ));
        }
        if let Some(token) = &self.bearer_token {
            request = request.header(header::AUTHORIZATION, token.clone());
        }
        let response = tokio::select! {
            result = request.send() => {
                result.map_err(|error| {
                    if options.deadline_micros.is_some_and(|deadline| unix_time_micros() >= deadline) {
                        product_error(ProductErrorCode::DeadlineExceeded, request_id)
                    } else {
                        ClientError::Http(error.to_string())
                    }
                })?
            }
            () = options.cancellation.cancelled() => {
                return Err(product_error(ProductErrorCode::Cancelled, request_id));
            }
        };
        let selected_headers = response
            .headers()
            .get_all(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2)
            .iter()
            .collect::<Vec<_>>();
        let selected_minor = if selected_headers.len() == 1 {
            selected_headers[0]
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|minor| hyphae_contracts::v2::PROTOCOL_MINORS_SUPPORTED_V2.contains(minor))
        } else {
            None
        };
        let Some(selected_minor) = selected_minor else {
            return Err(ClientError::Http(
                "HTTP v2 protocol minor is missing or unsupported".to_owned(),
            ));
        };
        let status = response.status();
        if one_time_secret && status.is_success() {
            let cache_control = response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok());
            let pragma = response
                .headers()
                .get(header::PRAGMA)
                .and_then(|value| value.to_str().ok());
            if cache_control != Some("no-store, private, max-age=0")
                || pragma != Some("no-cache")
                || response.headers().contains_key(header::CONTENT_ENCODING)
            {
                return Err(ClientError::Http(
                    "HTTP API-key secret response is not cache-safe".to_owned(),
                ));
            }
        }
        let response_session_headers = response
            .headers()
            .get_all(hyphae_contracts::v2::SESSION_ID_HEADER_V2)
            .iter()
            .collect::<Vec<_>>();
        let response_session_id = if let [value] = response_session_headers.as_slice() {
            value
                .to_str()
                .ok()
                .filter(|value| is_valid_session_id(value))
                .map(ToOwned::to_owned)
        } else {
            None
        };
        if !response_session_headers.is_empty() && response_session_id.is_none() {
            return Err(ClientError::Http(
                "HTTP v2 response session ID is invalid".to_owned(),
            ));
        }
        let response_request_ids = response
            .headers()
            .get_all(hyphae_contracts::v2::REQUEST_ID_HEADER_V2)
            .iter()
            .collect::<Vec<_>>();
        if response_request_ids.len() != 1
            || response_request_ids[0]
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                != Some(request_id)
        {
            return Err(ClientError::Http(
                "HTTP v2 response request ID mismatch".to_owned(),
            ));
        }
        if let Some(session_id) = response_session_id {
            *self.session_id.lock().await = Some(session_id);
        }
        let media_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or_default()
            .to_owned();
        let maximum = self.response_bytes.min(options.limits.max_response_bytes);
        let encoded = read_bounded(response, maximum, &options.cancellation, request_id).await?;
        if !status.is_success() {
            if media_type != ERROR_MEDIA_TYPE {
                return Err(ClientError::Http(format!(
                    "HTTP {} did not return a typed product error",
                    status.as_u16()
                )));
            }
            return Err(Box::new(
                hyphae_native_protocol::decode_failure(&encoded)
                    .map_err(|error| ClientError::Protocol(error.to_string()))?,
            )
            .into());
        }
        if status != StatusCode::OK || media_type != PRODUCT_MEDIA_TYPE {
            return Err(ClientError::Http(
                "HTTP v2 returned an unexpected status or media type".to_owned(),
            ));
        }
        hyphae_native_protocol::decode_product_response_for_minor(&encoded, selected_minor)
            .map_err(|error| ClientError::Protocol(error.to_string()))
    }
}

impl Transport for HttpTransport {
    fn execute(&self, operation: ProductOperation, options: RequestOptions) -> ResponseFuture<'_> {
        Box::pin(self.execute_inner(operation, options))
    }
}

fn endpoint_path(operation: &ProductOperation) -> &'static str {
    if operation.is_key_lifecycle() {
        "/v2/security/keys"
    } else {
        "/v2/execute"
    }
}

fn is_valid_session_id(value: &str) -> bool {
    value.len() == 32
        && value != "00000000000000000000000000000000"
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn read_bounded(
    mut response: reqwest::Response,
    maximum: usize,
    cancellation: &super::CancellationToken,
    request_id: u64,
) -> Result<Vec<u8>, ClientError> {
    let declared_length = response.content_length();
    if declared_length.is_some_and(|value| value > u64::try_from(maximum).unwrap_or(u64::MAX)) {
        return Err(ProductErrorCode::LimitExceeded.into_product_error());
    }
    let mut encoded = Vec::new();
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => {
                result.map_err(|error| ClientError::Http(error.to_string()))?
            }
            () = cancellation.cancelled() => {
                return Err(product_error(ProductErrorCode::Cancelled, request_id));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if encoded.len().saturating_add(chunk.len()) > maximum {
            return Err(Box::new(hyphae_native_product::ProductError::from_code(
                ProductErrorCode::LimitExceeded,
            ))
            .into());
        }
        encoded.extend_from_slice(&chunk);
    }
    if declared_length.is_some_and(|declared| declared != encoded.len() as u64) {
        return Err(ClientError::Http(
            "HTTP v2 response length differs from Content-Length".to_owned(),
        ));
    }
    Ok(encoded)
}

fn unique_request_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(1)
}

fn is_loopback_origin(origin: &Url) -> bool {
    let Some(host) = origin.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn unix_time_micros() -> i64 {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_micros();
    i64::try_from(micros).unwrap_or(i64::MAX)
}

fn product_error(code: ProductErrorCode, request_id: u64) -> ClientError {
    Box::new(
        hyphae_native_product::ProductError::from_code(code)
            .with_request_id(u128::from(request_id)),
    )
    .into()
}

trait ProductCodeExt {
    fn into_product_error(self) -> ClientError;
}

impl ProductCodeExt for ProductErrorCode {
    fn into_product_error(self) -> ClientError {
        Box::new(hyphae_native_product::ProductError::from_code(self)).into()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use hyphae_native_product::{
        ApiKeyConfirmationDigest, ApiKeyId, BuiltInRole, ProductAuthorization, ProductOperation,
        ProductScope, SecurityId,
    };

    use super::{ClientError, HttpTransport, PRODUCT_MEDIA_TYPE, RequestOptions, endpoint_path};

    fn capabilities_response() -> Vec<u8> {
        hyphae_native_protocol::encode_product_response(
            &hyphae_native_product::ProductResponse::Capabilities(
                hyphae_native_product::capabilities(),
            ),
        )
        .expect("encode capabilities response")
    }

    #[test]
    fn durable_bearer_requires_tls_outside_canonical_loopback() {
        for origin in [
            "http://127.0.0.1:8787",
            "http://127.1:8787",
            "http://[::1]:8787",
            "http://localhost:8787",
            "http://LOCALHOST:8787",
            "https://example.test",
        ] {
            assert!(
                HttpTransport::new(origin)
                    .and_then(|transport| transport.bearer_token("candidate"))
                    .is_ok(),
                "expected bearer origin to be accepted: {origin}"
            );
        }

        for origin in [
            "http://example.test",
            "http://localhost.example",
            "http://192.168.1.10",
            "http://[::ffff:127.0.0.1]",
        ] {
            let result = HttpTransport::new(origin)
                .and_then(|transport| transport.bearer_token("candidate"));
            assert!(
                result.is_err(),
                "plaintext remote bearer must fail: {origin}"
            );
            if let Err(error) = result {
                assert!(
                    matches!(
                        error,
                        ClientError::Http(ref message)
                            if message == "durable API keys require HTTPS outside loopback"
                    ),
                    "unexpected rejection for {origin}: {error}"
                );
            }
        }

        assert!(HttpTransport::new("http://example.test").is_ok());
    }

    #[test]
    fn every_api_key_lifecycle_variant_uses_the_dedicated_route() {
        let principal_id = SecurityId::new(1).expect("nonzero principal");
        let key_id = ApiKeyId::from_bytes([1; 16]).expect("nonzero key");
        let confirmation_digest = ApiKeyConfirmationDigest::from_bytes([2; 32]);
        let operations = [
            ProductOperation::SecurityApiKeyIssueSelfStart {
                principal_id,
                label: "self issue".to_owned(),
                roles: vec![BuiltInRole::Reader],
                custom_roles: Vec::new(),
                permission_ceiling: BuiltInRole::Reader.authorization(),
                scope_ceiling: vec![ProductScope::Instance],
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyIssueStart {
                principal_id,
                label: "admin issue".to_owned(),
                roles: vec![BuiltInRole::Reader],
                custom_roles: Vec::new(),
                permission_ceiling: ProductAuthorization::ALL,
                scope_ceiling: vec![ProductScope::Instance],
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyIssueSelfActivate {
                key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyIssueActivate {
                key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyRotateSelfStart {
                predecessor_key_id: key_id,
                label: "self rotate".to_owned(),
                overlap_seconds: 0,
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyRotateStart {
                predecessor_key_id: key_id,
                label: "admin rotate".to_owned(),
                overlap_seconds: 0,
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyRotateSelfActivate {
                successor_key_id: key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyRotateActivate {
                successor_key_id: key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyIssueSelfAbort { key_id },
            ProductOperation::SecurityApiKeyIssueAbort { key_id },
            ProductOperation::SecurityApiKeyRotateSelfAbort {
                successor_key_id: key_id,
            },
            ProductOperation::SecurityApiKeyRotateAbort {
                successor_key_id: key_id,
            },
            ProductOperation::SecurityApiKeyRevokeSelf { key_id },
            ProductOperation::SecurityApiKeyRevoke { key_id },
            ProductOperation::SecurityLegacyBearerRevoke,
        ];
        for operation in operations {
            assert_eq!(endpoint_path(&operation), "/v2/security/keys");
        }
        assert_eq!(
            endpoint_path(&ProductOperation::Capabilities),
            "/v2/execute"
        );
    }

    #[tokio::test]
    async fn response_minor_must_be_exact_before_session_retention() {
        for selected_minor in [None, Some("2"), Some("garbage")] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test HTTP peer");
            let address = listener.local_addr().expect("test HTTP address");
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
                let mut request = vec![0; 8 * 1024];
                let length = stream.read(&mut request).await.expect("read HTTP request");
                let request = String::from_utf8_lossy(&request[..length]).to_ascii_lowercase();
                assert!(request.contains("x-hyphae-protocol-minor: 3,4,5,6\r\n"));
                let minor = selected_minor.map_or_else(String::new, |minor| {
                    format!("X-Hyphae-Protocol-Minor: {minor}\r\n")
                });
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {PRODUCT_MEDIA_TYPE}\r\nContent-Length: 0\r\nX-Hyphae-Request-Id: 71\r\nX-Hyphae-Session-Id: 11111111111111111111111111111111\r\n{minor}Connection: close\r\n\r\n"
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write HTTP response");
            });
            let transport =
                HttpTransport::new(&format!("http://{address}")).expect("construct HTTP transport");
            let error = transport
                .execute_inner(
                    ProductOperation::Capabilities,
                    RequestOptions {
                        request_id: Some(71),
                        ..RequestOptions::default()
                    },
                )
                .await
                .expect_err("nonexact selected minor must fail");
            assert!(matches!(
                error,
                ClientError::Http(ref message)
                    if message == "HTTP v2 protocol minor is missing or unsupported"
            ));
            assert!(transport.session_id.lock().await.is_none());
            peer.await.expect("join HTTP peer");
        }
    }

    #[tokio::test]
    async fn swapped_response_cannot_poison_the_next_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test HTTP peer");
        let address = listener.local_addr().expect("test HTTP address");
        let body = capabilities_response();
        let peer = tokio::spawn(async move {
            for (request_id, response_id, session) in [
                (
                    71,
                    99,
                    "X-Hyphae-Session-Id: 11111111111111111111111111111111\r\n",
                ),
                (72, 72, ""),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
                let mut request = vec![0; 8 * 1024];
                let length = stream.read(&mut request).await.expect("read HTTP request");
                let request = String::from_utf8_lossy(&request[..length]).to_ascii_lowercase();
                assert!(request.contains(&format!("x-hyphae-request-id: {request_id}\r\n")));
                assert!(!request.contains("x-hyphae-session-id:"));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {PRODUCT_MEDIA_TYPE}\r\nContent-Length: {}\r\nX-Hyphae-Protocol-Minor: 3\r\nX-Hyphae-Request-Id: {response_id}\r\n{session}Connection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write HTTP response");
                stream
                    .write_all(&body)
                    .await
                    .expect("write HTTP response body");
            }
        });
        let transport =
            HttpTransport::new(&format!("http://{address}")).expect("construct HTTP transport");
        let error = transport
            .execute_inner(
                ProductOperation::Capabilities,
                RequestOptions {
                    request_id: Some(71),
                    ..RequestOptions::default()
                },
            )
            .await
            .expect_err("swapped response must fail");
        assert!(matches!(
            error,
            ClientError::Http(ref message) if message == "HTTP v2 response request ID mismatch"
        ));
        assert!(transport.session_id.lock().await.is_none());
        let second = transport
            .execute_inner(
                ProductOperation::Capabilities,
                RequestOptions {
                    request_id: Some(72),
                    ..RequestOptions::default()
                },
            )
            .await
            .expect("valid next request");
        assert!(matches!(
            second,
            hyphae_native_product::ProductResponse::Capabilities(_)
        ));
        assert!(transport.session_id.lock().await.is_none());
        peer.await.expect("join HTTP peer");
    }
}
