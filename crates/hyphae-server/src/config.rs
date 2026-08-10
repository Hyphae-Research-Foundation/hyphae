// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use hyphae_contracts::v1::ApiLimitsV1;
use hyphae_core::MAX_VECTOR_DIMENSIONS;
use hyphae_engine::{
    MAX_DOCUMENT_BYTES, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_NODES, MAX_RESULT_PROOF_BYTES,
    MAX_RETRIEVAL_PROOF_BYTES,
};
use hyphae_query::ExecutionLimits;
use hyphae_retrieval::{ExactRetrievalLimits, LexicalLimits};
use hyphae_storage::MAX_KEY_BYTES;
use subtle::ConstantTimeEq;
use thiserror::Error;

const MIN_BEARER_TOKEN_BYTES: usize = 32;
const MAX_BEARER_TOKEN_BYTES: usize = 4_096;

/// Default loopback-only address used by `hyphae serve`.
pub const DEFAULT_PORT: u16 = 8_787;

/// One bearer credential retained only as a BLAKE3 digest.
#[derive(Clone)]
pub struct BearerToken {
    digest: [u8; 32],
}

impl BearerToken {
    /// Hashes a sufficiently strong opaque bearer secret for later
    /// constant-time verification.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is shorter than 32 bytes or longer
    /// than 4096 bytes.
    pub fn new(secret: impl AsRef<[u8]>) -> Result<Self, ServerConfigError> {
        let secret = secret.as_ref();
        if !(MIN_BEARER_TOKEN_BYTES..=MAX_BEARER_TOKEN_BYTES).contains(&secret.len()) {
            return Err(ServerConfigError::InvalidBearerTokenLength {
                minimum: MIN_BEARER_TOKEN_BYTES,
                maximum: MAX_BEARER_TOKEN_BYTES,
                actual: secret.len(),
            });
        }
        if !secret.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
            return Err(ServerConfigError::InvalidBearerTokenCharacter);
        }
        Ok(Self {
            digest: *blake3::hash(secret).as_bytes(),
        })
    }

    pub(crate) fn verifies(&self, candidate: &[u8]) -> bool {
        if !(MIN_BEARER_TOKEN_BYTES..=MAX_BEARER_TOKEN_BYTES).contains(&candidate.len()) {
            return false;
        }
        let candidate = *blake3::hash(candidate).as_bytes();
        bool::from(self.digest.ct_eq(&candidate))
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

/// Request, response, admission, and witness policy enforced by one HTTP
/// server process. Embedded storage recovery/maintenance uses `StorageLimits`
/// through `HyphaeServer::open_with_storage_limits`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerLimits {
    /// Maximum complete JSON request bytes.
    pub request_body_bytes: usize,
    /// Maximum JSON nesting depth.
    pub json_depth: usize,
    /// Maximum JSON scalar, array, and object nodes.
    pub json_nodes: usize,
    /// Maximum time allowed to receive one complete JSON request body.
    pub request_body_timeout: Duration,
    /// Maximum records or keys in one atomic mutation batch.
    pub batch_items: usize,
    /// Maximum admitted concurrent data operations.
    pub concurrent_operations: usize,
    /// Maximum serialized JSON response bytes.
    pub response_bytes: usize,
    /// Maximum canonical proof bytes before base64 transport.
    pub proof_bytes: usize,
    /// Maximum downloadable snapshot witness bytes.
    pub witness_bytes: u64,
    /// Deterministic structured-query work, shape, result, and timeout limits.
    pub query: ExecutionLimits,
    /// Durable exact-retrieval work, result, byte, and timeout limits.
    pub exact_retrieval: ExactRetrievalLimits,
    /// Provider-free lexical work, result, token, and timeout limits.
    pub lexical_retrieval: LexicalLimits,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            request_body_bytes: 4 * 1024 * 1024,
            json_depth: MAX_DOCUMENT_DEPTH,
            json_nodes: 100_000,
            request_body_timeout: Duration::from_secs(10),
            batch_items: 1_000,
            concurrent_operations: 16,
            response_bytes: 32 * 1024 * 1024,
            proof_bytes: 16 * 1024 * 1024,
            witness_bytes: 512 * 1024 * 1024,
            query: ExecutionLimits::default(),
            exact_retrieval: ExactRetrievalLimits::default(),
            lexical_retrieval: LexicalLimits::default(),
        }
    }
}

impl ServerLimits {
    pub(crate) fn validate(&self) -> Result<(), ServerConfigError> {
        let scalar_limits = [
            self.request_body_bytes,
            self.json_depth,
            self.json_nodes,
            self.batch_items,
            self.concurrent_operations,
            self.response_bytes,
            self.proof_bytes,
            self.query.max_returned_records,
            self.query.max_groups,
            self.query.max_filter_nodes,
            self.query.max_filter_depth,
            self.query.max_sort_fields,
            self.query.max_group_fields,
            self.query.max_metrics,
            self.exact_retrieval.max_returned,
            self.lexical_retrieval.max_returned,
        ];
        if scalar_limits.contains(&0)
            || self.witness_bytes == 0
            || self.query.max_scanned_records == 0
            || self.query.max_matched_records == 0
            || self.exact_retrieval.max_candidates == 0
            || self.exact_retrieval.max_candidate_bytes == 0
            || self.lexical_retrieval.max_documents == 0
            || self.lexical_retrieval.max_tokens == 0
            || self.lexical_retrieval.max_candidates == 0
            || self.request_body_timeout.is_zero()
            || self.query.timeout.is_zero()
            || self.exact_retrieval.timeout.is_zero()
            || self.lexical_retrieval.timeout.is_zero()
        {
            return Err(ServerConfigError::ZeroLimit);
        }
        if self.json_depth > MAX_DOCUMENT_DEPTH {
            return Err(ServerConfigError::JsonDepthTooLarge {
                maximum: MAX_DOCUMENT_DEPTH,
                actual: self.json_depth,
            });
        }
        if self.json_nodes > MAX_DOCUMENT_NODES {
            return Err(ServerConfigError::JsonNodesTooLarge {
                maximum: MAX_DOCUMENT_NODES,
                actual: self.json_nodes,
            });
        }
        let maximum_proof_bytes = MAX_RESULT_PROOF_BYTES.min(MAX_RETRIEVAL_PROOF_BYTES);
        if u64::try_from(self.proof_bytes).unwrap_or(u64::MAX) > maximum_proof_bytes {
            return Err(ServerConfigError::ProofLimitTooLarge {
                maximum: maximum_proof_bytes,
                actual: u64::try_from(self.proof_bytes).unwrap_or(u64::MAX),
            });
        }
        Ok(())
    }

    pub(crate) fn as_contract(&self) -> ApiLimitsV1 {
        ApiLimitsV1 {
            key_bytes: usize_to_u64(MAX_KEY_BYTES),
            document_bytes: usize_to_u64(MAX_DOCUMENT_BYTES),
            request_body_bytes: usize_to_u64(self.request_body_bytes),
            json_depth: usize_to_u64(self.json_depth),
            json_nodes: usize_to_u64(self.json_nodes),
            request_body_timeout_ms: duration_millis(self.request_body_timeout),
            batch_items: usize_to_u64(self.batch_items),
            scanned_records: self.query.max_scanned_records,
            matched_records: self.query.max_matched_records,
            result_rows: usize_to_u64(self.query.max_returned_records),
            aggregation_groups: usize_to_u64(self.query.max_groups),
            filter_nodes: usize_to_u64(self.query.max_filter_nodes),
            filter_depth: usize_to_u64(self.query.max_filter_depth),
            sort_fields: usize_to_u64(self.query.max_sort_fields),
            group_fields: usize_to_u64(self.query.max_group_fields),
            metrics: usize_to_u64(self.query.max_metrics),
            concurrent_operations: usize_to_u64(self.concurrent_operations),
            query_timeout_ms: duration_millis(self.query.timeout),
            proof_bytes: usize_to_u64(self.proof_bytes),
            witness_bytes: self.witness_bytes,
            response_bytes: usize_to_u64(self.response_bytes),
            vector_dimensions: usize_to_u64(MAX_VECTOR_DIMENSIONS),
            retrieval_candidates: self.exact_retrieval.max_candidates,
            retrieval_candidate_bytes: self.exact_retrieval.max_candidate_bytes,
            retrieval_results: usize_to_u64(self.exact_retrieval.max_returned),
            retrieval_timeout_ms: duration_millis(self.exact_retrieval.timeout),
            retrieval_proof_bytes: usize_to_u64(self.proof_bytes),
            lexical_documents: self.lexical_retrieval.max_documents,
            lexical_tokens: self.lexical_retrieval.max_tokens,
            lexical_candidates: self.lexical_retrieval.max_candidates,
            lexical_results: usize_to_u64(self.lexical_retrieval.max_returned),
            lexical_timeout_ms: duration_millis(self.lexical_retrieval.timeout),
        }
    }
}

/// Validated input for one owned loopback-first Hyphae server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Exclusively owned Hyphae data directory.
    pub data_dir: PathBuf,
    /// Listener address; defaults to `127.0.0.1:8787`.
    pub bind: SocketAddr,
    /// Optional bearer credential. Mandatory for non-loopback binds.
    pub bearer_token: Option<BearerToken>,
    /// Effective bounded-resource policy.
    pub limits: ServerLimits,
}

impl ServerConfig {
    /// Creates the secure loopback default for one data directory.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
            bearer_token: None,
            limits: ServerLimits::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ServerConfigError> {
        if self.data_dir.as_os_str().is_empty() {
            return Err(ServerConfigError::EmptyDataDirectory);
        }
        if !self.bind.ip().is_loopback() && self.bearer_token.is_none() {
            return Err(ServerConfigError::RemoteBindRequiresAuthentication { bind: self.bind });
        }
        self.limits.validate()
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// Invalid secure-server configuration rejected before socket bind.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServerConfigError {
    /// The data-directory path is empty.
    #[error("server data-directory path must not be empty")]
    EmptyDataDirectory,
    /// A non-loopback listener was requested without bearer authentication.
    #[error("non-loopback bind {bind} requires a bearer token")]
    RemoteBindRequiresAuthentication {
        /// Rejected listener address.
        bind: SocketAddr,
    },
    /// Bearer token length does not meet the local security policy.
    #[error("bearer token is {actual} bytes; required range is {minimum}..={maximum}")]
    InvalidBearerTokenLength {
        /// Minimum accepted bytes.
        minimum: usize,
        /// Maximum accepted bytes.
        maximum: usize,
        /// Observed bytes.
        actual: usize,
    },
    /// Bearer tokens must be representable safely in one HTTP header.
    #[error("bearer token must contain only visible ASCII without whitespace")]
    InvalidBearerTokenCharacter,
    /// Every configured budget must be positive.
    #[error("server resource limits must be nonzero")]
    ZeroLimit,
    /// JSON depth policy cannot exceed the canonical document limit.
    #[error("JSON depth limit {actual} exceeds canonical maximum {maximum}")]
    JsonDepthTooLarge {
        /// Canonical maximum.
        maximum: usize,
        /// Requested limit.
        actual: usize,
    },
    /// JSON node policy cannot exceed the canonical document limit.
    #[error("JSON node limit {actual} exceeds canonical maximum {maximum}")]
    JsonNodesTooLarge {
        /// Canonical maximum.
        maximum: usize,
        /// Requested limit.
        actual: usize,
    },
    /// Proof policy cannot exceed the canonical proof codec hard bound.
    #[error("proof limit {actual} exceeds canonical maximum {maximum}")]
    ProofLimitTooLarge {
        /// Canonical maximum.
        maximum: u64,
        /// Requested limit.
        actual: u64,
    },
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn duration_millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}
