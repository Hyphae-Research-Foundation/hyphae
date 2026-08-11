// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use thiserror::Error;

use crate::BearerToken;

/// Default loopback port for the optional Native HTTP v2 adapter.
pub(super) const DEFAULT_NATIVE_HTTP_V2_PORT: u16 = 8_788;

/// HTTP framing limits enforced in addition to every product request context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeHttpV2Limits {
    /// Maximum complete product request envelope bytes.
    pub request_body_bytes: usize,
    /// Maximum complete product response envelope or NDJSON stream bytes.
    pub response_bytes: usize,
    /// Maximum time allowed to receive one complete request body.
    pub request_body_timeout: Duration,
    /// Maximum binary bytes represented by one provisional NDJSON record.
    pub stream_chunk_bytes: usize,
}

impl Default for NativeHttpV2Limits {
    fn default() -> Self {
        Self {
            request_body_bytes: hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES,
            response_bytes: hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES,
            request_body_timeout: Duration::from_secs(10),
            stream_chunk_bytes: 64 * 1024,
        }
    }
}

impl NativeHttpV2Limits {
    pub(super) fn validate(self) -> Result<(), NativeHttpV2ConfigError> {
        if self.request_body_bytes == 0
            || self.response_bytes == 0
            || self.request_body_timeout.is_zero()
            || self.stream_chunk_bytes == 0
        {
            return Err(NativeHttpV2ConfigError::ZeroLimit);
        }
        if self.request_body_bytes > hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES
            || self.response_bytes > hyphae_native_protocol::MAX_PRODUCT_WIRE_BYTES
            || self.stream_chunk_bytes > self.response_bytes
        {
            return Err(NativeHttpV2ConfigError::LimitAboveProductMaximum);
        }
        Ok(())
    }
}

/// Listener and authentication policy for an adapter over an existing owner.
#[derive(Clone, Debug)]
pub struct NativeHttpV2Config {
    /// Listener address; defaults to `127.0.0.1:8788`.
    pub bind: SocketAddr,
    /// Optional bearer credential. A non-loopback bind requires one.
    pub bearer_token: Option<BearerToken>,
    /// HTTP-only framing bounds.
    pub limits: NativeHttpV2Limits,
}

impl Default for NativeHttpV2Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_NATIVE_HTTP_V2_PORT),
            bearer_token: None,
            limits: NativeHttpV2Limits::default(),
        }
    }
}

impl NativeHttpV2Config {
    pub(super) fn validate(&self) -> Result<(), NativeHttpV2ConfigError> {
        if !self.bind.ip().is_loopback() && self.bearer_token.is_none() {
            return Err(NativeHttpV2ConfigError::RemoteBindRequiresAuthentication {
                bind: self.bind,
            });
        }
        self.limits.validate()
    }
}

/// Native HTTP v2 configuration rejected before a socket or product session is opened.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeHttpV2ConfigError {
    /// Remote listeners must authenticate every request.
    #[error("non-loopback Native HTTP v2 bind {bind} requires a bearer token")]
    RemoteBindRequiresAuthentication {
        /// Rejected listener address.
        bind: SocketAddr,
    },
    /// Every framing bound must be positive.
    #[error("Native HTTP v2 limits must be nonzero")]
    ZeroLimit,
    /// HTTP framing cannot exceed the canonical product-envelope maximum.
    #[error("Native HTTP v2 limit exceeds the product-envelope maximum")]
    LimitAboveProductMaximum,
}
