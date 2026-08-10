// SPDX-License-Identifier: GPL-3.0-only

//! Secure loopback-first HTTP delivery for public versioned contracts.
//!
//! Opening the embedded engine does not start a listener. Callers explicitly
//! construct [`HyphaeServer`], bind it, and provide a graceful-shutdown future.

mod config;
mod error;
mod native_v2;
mod server;

pub use config::{BearerToken, DEFAULT_PORT, ServerConfig, ServerConfigError, ServerLimits};
pub use error::ServerError;
pub use native_v2::{
    BoundNativeHttpV2Server, NATIVE_V1_COMPATIBILITY_POLICY, NativeHttpV2Config,
    NativeHttpV2ConfigError, NativeHttpV2Error, NativeHttpV2Limits, NativeHttpV2Server,
};
pub use server::{BoundServer, HyphaeServer};

use error::ApiError;
