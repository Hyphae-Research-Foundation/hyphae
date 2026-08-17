// SPDX-License-Identifier: Apache-2.0

//! Optional `PliegoRS` application-state boundary for the public Hyphae client.
//!
//! This crate deliberately does not depend on or import `PliegoRS` internals.
//! A `PliegoRS` application may add [`PliegoHyphae`] to its own public state
//! mechanism. Omitting this crate or both environment values leaves the host
//! completely independent of Hyphae.

use std::{env, fmt};

use hyphae_client::{ClientConfigError, HyphaeClient};
use thiserror::Error;

/// Environment variable containing the root public Hyphae HTTP(S) origin.
pub const BASE_URL_ENV: &str = "HYPHAE_BASE_URL";
/// Environment variable containing the optional bearer secret.
pub const BEARER_TOKEN_ENV: &str = "HYPHAE_BEARER_TOKEN";

/// Optional adapter configuration containing only public client inputs.
#[derive(Clone, Eq, PartialEq)]
pub struct PliegoHyphaeConfig {
    base_url: String,
    bearer_token: Option<String>,
}

impl PliegoHyphaeConfig {
    /// Resolves optional integration state from process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a bearer secret exists without a Hyphae origin,
    /// or when either environment value is not valid Unicode.
    pub fn from_env() -> Result<Option<Self>, PliegoHyphaeConfigError> {
        let base_url = read_optional_env(BASE_URL_ENV)?;
        let bearer_token = read_optional_env(BEARER_TOKEN_ENV)?;
        Self::from_optional_values(base_url, bearer_token)
    }

    /// Resolves optional integration state from explicitly supplied values.
    ///
    /// `None, None` is the normal disabled state. This constructor makes that
    /// behavior testable without mutating process-global environment.
    ///
    /// # Errors
    ///
    /// Rejects a bearer secret without a corresponding base URL.
    pub fn from_optional_values(
        base_url: Option<String>,
        bearer_token: Option<String>,
    ) -> Result<Option<Self>, PliegoHyphaeConfigError> {
        match (base_url, bearer_token) {
            (None, None) => Ok(None),
            (None, Some(_)) => Err(PliegoHyphaeConfigError::TokenWithoutBaseUrl),
            (Some(base_url), bearer_token) => Ok(Some(Self {
                base_url,
                bearer_token,
            })),
        }
    }

    /// Builds a reusable adapter without opening a listener or data directory.
    ///
    /// # Errors
    ///
    /// Returns public-client configuration errors for malformed origins,
    /// secrets, timeouts, or response limits.
    pub fn build(self) -> Result<PliegoHyphae, ClientConfigError> {
        let mut builder = HyphaeClient::builder(&self.base_url)?;
        if let Some(token) = self.bearer_token {
            builder = builder.bearer_token(&token)?;
        }
        Ok(PliegoHyphae {
            client: builder.build()?,
        })
    }
}

impl fmt::Debug for PliegoHyphaeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PliegoHyphaeConfig")
            .field("base_url", &self.base_url)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Cloneable public client state for opt-in `PliegoRS` applications.
#[derive(Clone, Debug)]
pub struct PliegoHyphae {
    client: HyphaeClient,
}

impl PliegoHyphae {
    /// Borrows the versioned public Hyphae HTTP client.
    pub fn client(&self) -> &HyphaeClient {
        &self.client
    }
}

/// Errors while deciding whether the optional integration is enabled.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum PliegoHyphaeConfigError {
    /// One environment value was not valid Unicode.
    #[error("{name} is not valid Unicode")]
    InvalidEnvironmentEncoding {
        /// Environment variable name.
        name: &'static str,
    },
    /// A secret alone is ambiguous and must never imply a default endpoint.
    #[error("HYPHAE_BEARER_TOKEN requires HYPHAE_BASE_URL")]
    TokenWithoutBaseUrl,
}

fn read_optional_env(name: &'static str) -> Result<Option<String>, PliegoHyphaeConfigError> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_| PliegoHyphaeConfigError::InvalidEnvironmentEncoding { name })
}

#[cfg(test)]
mod tests {
    use super::{PliegoHyphaeConfig, PliegoHyphaeConfigError};

    #[test]
    fn absent_configuration_is_a_normal_disabled_state() {
        assert_eq!(
            PliegoHyphaeConfig::from_optional_values(None, None),
            Ok(None)
        );
    }

    #[test]
    fn a_secret_never_selects_an_implicit_endpoint() {
        assert_eq!(
            PliegoHyphaeConfig::from_optional_values(None, Some("secret".to_owned())),
            Err(PliegoHyphaeConfigError::TokenWithoutBaseUrl)
        );
    }

    #[test]
    fn enabled_adapter_uses_only_a_public_origin() -> Result<(), Box<dyn std::error::Error>> {
        let config = PliegoHyphaeConfig::from_optional_values(
            Some("http://127.0.0.1:8787".to_owned()),
            None,
        )?
        .ok_or("adapter should be enabled")?;
        let adapter = config.build()?;
        let _public_client = adapter.client();
        Ok(())
    }

    #[test]
    fn debug_output_redacts_bearer_secret() -> Result<(), Box<dyn std::error::Error>> {
        let config = PliegoHyphaeConfig::from_optional_values(
            Some("https://example.test".to_owned()),
            Some("do-not-print".to_owned()),
        )?
        .ok_or("adapter should be enabled")?;
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("do-not-print"));
        Ok(())
    }
}
