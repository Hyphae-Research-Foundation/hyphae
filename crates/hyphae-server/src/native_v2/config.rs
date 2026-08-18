// SPDX-License-Identifier: Apache-2.0

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
    /// Explicit 1.2-only migrated legacy bearer for a bootstrapped directory.
    pub legacy_bearer_token: Option<BearerToken>,
    /// Product compatibility version selecting the bounded migration window.
    pub legacy_compatibility_version: hyphae_native_product::LegacyBearerCompatibilityVersion,
    /// HTTP-only framing bounds.
    pub limits: NativeHttpV2Limits,
}

impl Default for NativeHttpV2Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_NATIVE_HTTP_V2_PORT),
            bearer_token: None,
            legacy_bearer_token: None,
            legacy_compatibility_version:
                hyphae_native_product::LEGACY_BEARER_COMPATIBILITY_VERSION,
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

    pub(super) fn validate_managed(&self) -> Result<(), NativeHttpV2ConfigError> {
        if self.bearer_token.is_some() {
            return Err(NativeHttpV2ConfigError::ManagedAuthenticationConflict);
        }
        if !self.bind.ip().is_loopback() {
            return Err(NativeHttpV2ConfigError::RemoteManagedBindRequiresTls { bind: self.bind });
        }
        self.limits.validate()
    }

    pub(super) fn validate_managed_legacy(
        &self,
        state: hyphae_native_product::LegacyBearerState,
    ) -> Result<(), NativeHttpV2ConfigError> {
        if self.bearer_token.is_some() {
            return Err(NativeHttpV2ConfigError::ManagedAuthenticationConflict);
        }
        if self.legacy_bearer_token.is_some() && !self.bind.ip().is_loopback() {
            return Err(NativeHttpV2ConfigError::RemoteManagedBindRequiresTls { bind: self.bind });
        }
        match (
            self.legacy_compatibility_version.permits_authentication(),
            state,
            self.legacy_bearer_token.is_some(),
        ) {
            (
                true,
                hyphae_native_product::LegacyBearerState::MigrationPending
                | hyphae_native_product::LegacyBearerState::DualWindow,
                true,
            )
            | (
                true | false,
                hyphae_native_product::LegacyBearerState::NeverEnabled
                | hyphae_native_product::LegacyBearerState::Revoked,
                false,
            ) => {}
            (false, state, _) if state.is_enabled() => {
                return Err(NativeHttpV2ConfigError::LegacyCompatibilityExpired { state });
            }
            (_, state, configured) => {
                return Err(NativeHttpV2ConfigError::LegacyStateConfigurationMismatch {
                    state,
                    configured,
                });
            }
        }
        self.validate_managed()
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
    /// Catalog-managed authentication cannot be combined with a fixed bearer.
    #[error("catalog-managed Native HTTP v2 authentication rejects the legacy bearer token")]
    ManagedAuthenticationConflict,
    /// Durable API-key credentials cannot traverse this plaintext listener remotely.
    #[error(
        "non-loopback catalog-managed Native HTTP v2 bind {bind} requires TLS termination; bind this adapter to loopback"
    )]
    RemoteManagedBindRequiresTls {
        /// Rejected plaintext listener address.
        bind: SocketAddr,
    },
    /// Every framing bound must be positive.
    #[error("Native HTTP v2 limits must be nonzero")]
    ZeroLimit,
    /// HTTP framing cannot exceed the canonical product-envelope maximum.
    #[error("Native HTTP v2 limit exceeds the product-envelope maximum")]
    LimitAboveProductMaximum,
    /// Durable state and explicit legacy configuration disagree.
    #[error(
        "Native HTTP legacy bearer configuration ({configured}) conflicts with durable state {state:?}"
    )]
    LegacyStateConfigurationMismatch {
        /// Durable terminal-aware state.
        state: hyphae_native_product::LegacyBearerState,
        /// Whether a process-local legacy verifier was configured.
        configured: bool,
    },
    /// The one-minor compatibility line no longer authenticates legacy bearers.
    #[error(
        "Native HTTP legacy bearer state {state:?} is enabled outside compatibility version 1.2; revoke it offline or with a canonical Owner key"
    )]
    LegacyCompatibilityExpired {
        /// Durable enabled state requiring explicit revocation.
        state: hyphae_native_product::LegacyBearerState,
    },
    /// Configured bearer does not match the durable migrated verifier.
    #[error("configured Native HTTP legacy bearer does not match the migrated credential")]
    LegacyBearerMismatch,
}
