// SPDX-License-Identifier: GPL-3.0-only

use std::{sync::Arc, time::Duration};

use hyphae_native_product::{ProductErrorCode, ProductOperation, ProductResponse};
use reqwest::{StatusCode, Url, header};

use super::{ClientError, RequestOptions, ResponseFuture, Transport};

const DEFAULT_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PRODUCT_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_MEDIA_TYPE_V2;
const ERROR_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_ERROR_MEDIA_TYPE_V2;

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

    /// Adds one opaque bearer token.
    pub fn bearer_token(mut self, token: &str) -> Result<Self, ClientError> {
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
        let encoded =
            hyphae_native_protocol::encode_product_request(&hyphae_native_protocol::WireRequest {
                operation,
                logical_time_micros: options.logical_time_micros,
                deadline_micros: options.deadline_micros,
                idempotency_token: options.idempotency_token,
                limits: options.limits,
                durability: options.durability,
            })
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let mut endpoint = self.origin.clone();
        endpoint.set_path("/v2/execute");
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
            () = wait_cancelled(options.cancellation.clone()) => {
                return Err(product_error(ProductErrorCode::Cancelled, request_id));
            }
        };
        let status = response.status();
        if let Some(session_id) = response
            .headers()
            .get(hyphae_contracts::v2::SESSION_ID_HEADER_V2)
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session_id.to_owned());
        }
        let response_request_id = response
            .headers()
            .get("x-hyphae-request-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if response_request_id != Some(request_id) {
            return Err(ClientError::Http(
                "HTTP v2 response request ID mismatch".to_owned(),
            ));
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
        hyphae_native_protocol::decode_product_response(&encoded)
            .map_err(|error| ClientError::Protocol(error.to_string()))
    }
}

impl Transport for HttpTransport {
    fn execute(&self, operation: ProductOperation, options: RequestOptions) -> ResponseFuture<'_> {
        Box::pin(self.execute_inner(operation, options))
    }
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
            () = wait_cancelled(cancellation.clone()) => {
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

async fn wait_cancelled(token: super::CancellationToken) {
    while !token.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn unique_request_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(1)
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
