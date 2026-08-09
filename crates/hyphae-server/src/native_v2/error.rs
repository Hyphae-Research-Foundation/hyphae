// SPDX-License-Identifier: Apache-2.0

use std::{io, net::SocketAddr};

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyphae_contracts::v2::{
    ProductErrorDetailsV2, ProductErrorLimitV2, ProductErrorSourceSpanV2,
    ProductErrorUnknownDetailV2, ProductErrorV2,
};
use hyphae_native_product::{
    ProductError, ProductErrorCategory, ProductErrorCode, ProductLimitKind,
};
use thiserror::Error;

use super::{ERROR_MEDIA_TYPE, REQUEST_ID_HEADER, RequestMetadata};

/// Startup, listener, or product-session failure for Native HTTP v2.
#[derive(Debug, Error)]
pub enum NativeHttpV2Error {
    /// Secure listener configuration was rejected before bind.
    #[error(transparent)]
    Configuration(#[from] super::NativeHttpV2ConfigError),
    /// The existing one-owner product service rejected session creation.
    #[error("Native HTTP v2 product service failed: {0}")]
    Product(#[from] Box<ProductError>),
    /// The requested socket could not be bound.
    #[error("failed to bind Native HTTP v2 at {address}: {source}")]
    Bind {
        /// Requested listener address.
        address: SocketAddr,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// The HTTP listener failed while serving.
    #[error("Native HTTP v2 service failed: {0}")]
    Serve(#[source] io::Error),
}

pub(super) struct NativeApiError {
    pub(super) error: Box<ProductError>,
    pub(super) metadata: RequestMetadata,
    status: StatusCode,
    authenticate: bool,
}

impl NativeApiError {
    pub(super) fn product(error: ProductError, metadata: &RequestMetadata) -> Self {
        let error = if error.request_id().is_none() {
            error.with_request_id(metadata.request_id)
        } else {
            error
        };
        Self {
            status: product_status(error.category()),
            error: Box::new(error),
            metadata: metadata.clone(),
            authenticate: false,
        }
    }

    pub(super) fn code(code: ProductErrorCode, metadata: &RequestMetadata) -> Self {
        Self::product(ProductError::from_code(code), metadata)
    }

    pub(super) fn unauthorized(metadata: &RequestMetadata) -> Self {
        let mut value = Self::code(ProductErrorCode::AuthorizationDenied, metadata);
        value.status = StatusCode::UNAUTHORIZED;
        value.authenticate = true;
        value
    }

    pub(super) fn payload_too_large(metadata: &RequestMetadata) -> Self {
        let mut value = Self::code(ProductErrorCode::LimitExceeded, metadata);
        value.status = StatusCode::PAYLOAD_TOO_LARGE;
        value
    }

    pub(super) fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }
}

impl IntoResponse for NativeApiError {
    fn into_response(self) -> Response {
        let request_id = self.metadata.request_id.to_string();
        let (content_type, body) = if self.metadata.binary_errors {
            match hyphae_native_protocol::encode_failure(&self.error) {
                Ok(encoded) => (ERROR_MEDIA_TYPE, Body::from(encoded)),
                Err(_) => (
                    "application/json",
                    Body::from(b"{\"code\":\"internal\"}".as_slice()),
                ),
            }
        } else {
            let envelope = product_error_json(&self.error);
            match serde_json::to_vec(&envelope) {
                Ok(encoded) => ("application/json", Body::from(encoded)),
                Err(_) => (
                    "application/json",
                    Body::from(b"{\"code\":\"internal\"}".as_slice()),
                ),
            }
        };
        let mut response = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, content_type)
            .header(REQUEST_ID_HEADER, request_id)
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        if self.authenticate {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"hyphae-native-v2\""),
            );
        }
        response
    }
}

fn product_status(category: ProductErrorCategory) -> StatusCode {
    match category {
        ProductErrorCategory::InvalidRequest => StatusCode::BAD_REQUEST,
        ProductErrorCategory::NotFound => StatusCode::NOT_FOUND,
        ProductErrorCategory::Conflict => StatusCode::CONFLICT,
        ProductErrorCategory::Limit => StatusCode::UNPROCESSABLE_ENTITY,
        ProductErrorCategory::Deadline => StatusCode::REQUEST_TIMEOUT,
        ProductErrorCategory::Cancelled => {
            StatusCode::from_u16(499).unwrap_or(StatusCode::REQUEST_TIMEOUT)
        }
        ProductErrorCategory::Authorization => StatusCode::FORBIDDEN,
        ProductErrorCategory::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn product_error_json(error: &ProductError) -> ProductErrorV2 {
    ProductErrorV2 {
        code: error.code().as_str().to_owned(),
        category: error.category().as_str().to_owned(),
        retry: error.retry().as_str().to_owned(),
        message: error.message().to_owned(),
        request_id: error.request_id().map(|value| value.to_string()),
        trace_id: error.trace_id().map(|value| value.to_string()),
        object_id: error.object_id().map(|value| value.get().to_string()),
        transaction_state: error.transaction_state().as_str().to_owned(),
        transaction_id: error
            .details()
            .transaction_id()
            .map(|value| value.get().to_string()),
        limit: error.limit().map(|limit| ProductErrorLimitV2 {
            kind: limit_kind(&limit.kind()).to_owned(),
            configured: limit.configured(),
            observed: limit.observed(),
        }),
        source_span: error.source_span().map(|span| ProductErrorSourceSpanV2 {
            start: span.start(),
            end: span.end(),
        }),
        details: ProductErrorDetailsV2 {
            sql_subcode: error
                .details()
                .sql_subcode()
                .map(|value| value.as_str().to_owned()),
            unknown: error
                .details()
                .unknown()
                .iter()
                .map(|detail| ProductErrorUnknownDetailV2 {
                    tag: detail.tag(),
                    value_hex: encode_hex(detail.value()),
                })
                .collect(),
        },
    }
}

fn limit_kind(kind: &ProductLimitKind) -> &str {
    kind.as_str()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
