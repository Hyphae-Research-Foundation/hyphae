// SPDX-License-Identifier: Apache-2.0

//! Bounded asynchronous Rust client for the public Hyphae HTTP API.
//!
//! This crate depends only on public wire contracts. It does not import the
//! server, engine, storage, query, or retrieval implementations.

/// Native product API v2 clients and transports.
pub mod v2;

use std::time::Duration;

use hyphae_contracts::v1::{
    CapabilitiesV1, CommitReceiptV1, DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1,
    DeleteRequestV1, DeleteVectorsRequestV1, ErrorV1, ExactRetrievalRequestV1,
    ExactRetrievalResponseV1, GetRequestV1, GetResponseV1, HealthV1, HybridRetrievalRequestV1,
    HybridRetrievalResponseV1, LexicalRetrievalRequestV1, LexicalRetrievalResponseV1, ProofV1,
    PutRequestV1, PutVectorsRequestV1, QueryRequestV1, QueryResponseV1, RetrievalProofV1,
};
use reqwest::{
    Method, StatusCode, Url,
    header::{self, HeaderMap, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

const DEFAULT_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_WITNESS_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Successful typed API value and its response correlation identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiResponse<T> {
    /// Decoded public contract value.
    pub value: T,
    /// UUID copied from `X-Request-Id`.
    pub request_id: String,
}

/// Stable server-declared error received from `/v1`.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("Hyphae API returned HTTP {status} {code} (request {request_id})")]
pub struct ApiFailure {
    /// HTTP status code.
    pub status: u16,
    /// Stable machine-readable error code.
    pub code: String,
    /// Bounded server diagnostic.
    pub message: String,
    /// UUID matching the response header.
    pub request_id: String,
}

/// Invalid client configuration detected before any network operation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ClientConfigError {
    /// Base URL is not syntactically valid.
    #[error("invalid Hyphae base URL")]
    InvalidBaseUrl,
    /// Only explicit HTTP and HTTPS origins are supported.
    #[error("Hyphae base URL must use http or https")]
    UnsupportedScheme,
    /// Base URL must be one origin without credentials, query, fragment, or path prefix.
    #[error("Hyphae base URL must be an origin without credentials, query, fragment, or path")]
    NonOriginBaseUrl,
    /// Bearer secret cannot be represented safely in one header.
    #[error("invalid bearer token for an HTTP authorization header")]
    InvalidBearerToken,
    /// Every local client bound must be positive.
    #[error("client timeout and response limits must be nonzero")]
    ZeroLimit,
    /// Underlying HTTP client construction failed.
    #[error("failed to construct HTTP client")]
    HttpClient,
}

/// Transport, bound, envelope, or declared API failure.
#[derive(Debug, Error)]
pub enum ClientError {
    /// HTTP transport failed before a complete bounded response arrived.
    #[error("Hyphae HTTP transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// Response declared or exceeded a local byte bound.
    #[error("Hyphae response exceeded local limit {maximum} bytes")]
    ResponseTooLarge {
        /// Configured maximum.
        maximum: usize,
    },
    /// A JSON operation returned a non-JSON media type.
    #[error("Hyphae response did not use a JSON content type")]
    InvalidContentType,
    /// JSON response did not match the public versioned contract.
    #[error("Hyphae response violated the version 1 contract: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// The request ID header was missing, duplicated, or malformed.
    #[error("Hyphae response has no single valid X-Request-Id header")]
    InvalidRequestId,
    /// Error envelope and header correlation identifiers disagree.
    #[error("Hyphae error envelope request ID differs from its response header")]
    RequestIdMismatch,
    /// Server returned a non-success response that conformed to `ErrorV1`.
    #[error(transparent)]
    Api(#[from] ApiFailure),
    /// A successful operation returned an unexpected HTTP status.
    #[error("Hyphae returned unexpected success status {0}")]
    UnexpectedStatus(u16),
    /// Proof witness reference was not canonical for its proof identity.
    #[error("proof contains a noncanonical witness reference")]
    InvalidWitnessReference,
    /// Downloaded witness digest header differs from the proof.
    #[error("downloaded witness digest header differs from the proof")]
    WitnessDigestMismatch,
    /// Downloaded witness length differs from the proof.
    #[error("downloaded witness length differs from the proof")]
    WitnessLengthMismatch,
}

/// Builder for a bounded public API client.
#[derive(Clone, Debug)]
#[must_use = "a client builder has no effect until build is called"]
pub struct ClientBuilder {
    base_url: Url,
    bearer_token: Option<HeaderValue>,
    timeout: Duration,
    response_bytes: usize,
    witness_bytes: usize,
}

impl ClientBuilder {
    /// Parses one root HTTP(S) origin.
    ///
    /// # Errors
    ///
    /// Rejects malformed URLs, non-HTTP schemes, credentials, query strings,
    /// fragments, and non-root paths.
    pub fn new(base_url: &str) -> Result<Self, ClientConfigError> {
        let mut base_url = Url::parse(base_url).map_err(|_| ClientConfigError::InvalidBaseUrl)?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(ClientConfigError::UnsupportedScheme);
        }
        if !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !matches!(base_url.path(), "" | "/")
        {
            return Err(ClientConfigError::NonOriginBaseUrl);
        }
        base_url.set_path("/");
        Ok(Self {
            base_url,
            bearer_token: None,
            timeout: DEFAULT_TIMEOUT,
            response_bytes: DEFAULT_RESPONSE_BYTES,
            witness_bytes: DEFAULT_WITNESS_BYTES,
        })
    }

    /// Configures an opaque bearer token without retaining a second copy.
    ///
    /// # Errors
    ///
    /// Rejects values that cannot be represented in one HTTP header.
    pub fn bearer_token(mut self, token: &str) -> Result<Self, ClientConfigError> {
        if token.is_empty() {
            return Err(ClientConfigError::InvalidBearerToken);
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientConfigError::InvalidBearerToken)?;
        value.set_sensitive(true);
        self.bearer_token = Some(value);
        Ok(self)
    }

    /// Sets the complete request/response deadline.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the maximum complete JSON response bytes.
    pub fn response_bytes(mut self, maximum: usize) -> Self {
        self.response_bytes = maximum;
        self
    }

    /// Sets the maximum complete snapshot witness bytes.
    pub fn witness_bytes(mut self, maximum: usize) -> Self {
        self.witness_bytes = maximum;
        self
    }

    /// Constructs the reusable client.
    ///
    /// # Errors
    ///
    /// Rejects zero limits or an underlying HTTP client configuration error.
    pub fn build(self) -> Result<HyphaeClient, ClientConfigError> {
        if self.timeout.is_zero() || self.response_bytes == 0 || self.witness_bytes == 0 {
            return Err(ClientConfigError::ZeroLimit);
        }
        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|_| ClientConfigError::HttpClient)?;
        Ok(HyphaeClient {
            http,
            base_url: self.base_url,
            bearer_token: self.bearer_token,
            response_bytes: self.response_bytes,
            witness_bytes: self.witness_bytes,
        })
    }
}

/// Reusable bounded client for Hyphae HTTP API version 1.
#[derive(Clone, Debug)]
pub struct HyphaeClient {
    http: reqwest::Client,
    base_url: Url,
    bearer_token: Option<HeaderValue>,
    response_bytes: usize,
    witness_bytes: usize,
}

impl HyphaeClient {
    /// Starts a client builder for one root origin.
    ///
    /// # Errors
    ///
    /// Returns a base-URL validation error.
    pub fn builder(base_url: &str) -> Result<ClientBuilder, ClientConfigError> {
        ClientBuilder::new(base_url)
    }

    /// Reports server capabilities and effective limits.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn capabilities(&self) -> Result<ApiResponse<CapabilitiesV1>, ClientError> {
        self.get_json("v1/capabilities", false).await
    }

    /// Reports process liveness.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn liveness(&self) -> Result<ApiResponse<HealthV1>, ClientError> {
        self.get_json("v1/health/live", false).await
    }

    /// Reports engine readiness.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn readiness(&self) -> Result<ApiResponse<HealthV1>, ClientError> {
        self.get_json("v1/health/ready", false).await
    }

    /// Atomically stores a structured-record batch.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn put(
        &self,
        request: &PutRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/kv/put", request).await
    }

    /// Atomically deletes a binary-key batch.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn delete(
        &self,
        request: &DeleteRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/kv/delete", request).await
    }

    /// Gets proven key presence or absence.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn get(
        &self,
        request: &GetRequestV1,
    ) -> Result<ApiResponse<GetResponseV1>, ClientError> {
        self.post_json("v1/kv/get", request).await
    }

    /// Executes a deterministic proof-bearing structured query.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn query(
        &self,
        request: &QueryRequestV1,
    ) -> Result<ApiResponse<QueryResponseV1>, ClientError> {
        self.post_json("v1/query", request).await
    }

    /// Defines or exactly reuses one immutable durable vector space.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn define_vector_space(
        &self,
        request: &DefineVectorSpaceRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/vector-spaces/define", request).await
    }

    /// Atomically stores one durable vector batch.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn put_vectors(
        &self,
        request: &PutVectorsRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/vectors/put", request).await
    }

    /// Atomically deletes one durable vector batch.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn delete_vectors(
        &self,
        request: &DeleteVectorsRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/vectors/delete", request).await
    }

    /// Executes proof-bearing exact durable vector retrieval.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn retrieve_exact(
        &self,
        request: &ExactRetrievalRequestV1,
    ) -> Result<ApiResponse<ExactRetrievalResponseV1>, ClientError> {
        self.post_json("v1/retrieve/exact", request).await
    }

    /// Defines or exactly reuses one immutable provider-free lexical index.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn define_lexical_index(
        &self,
        request: &DefineLexicalIndexRequestV1,
    ) -> Result<ApiResponse<CommitReceiptV1>, ClientError> {
        self.post_json("v1/lexical-indexes/define", request).await
    }

    /// Executes proof-bearing provider-free lexical retrieval.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn retrieve_lexical(
        &self,
        request: &LexicalRetrievalRequestV1,
    ) -> Result<ApiResponse<LexicalRetrievalResponseV1>, ClientError> {
        self.post_json("v1/retrieve/lexical", request).await
    }

    /// Executes proof-bearing deterministic hybrid retrieval.
    ///
    /// # Errors
    ///
    /// Returns a transport, bound, contract, correlation, or API error.
    pub async fn retrieve_hybrid(
        &self,
        request: &HybridRetrievalRequestV1,
    ) -> Result<ApiResponse<HybridRetrievalResponseV1>, ClientError> {
        self.post_json("v1/retrieve/hybrid", request).await
    }

    /// Downloads the exact snapshot witness referenced by a proof.
    ///
    /// # Errors
    ///
    /// Rejects a noncanonical reference, transport/bound/API failure, or a
    /// digest header that disagrees with the proof.
    pub async fn download_witness(
        &self,
        proof: &ProofV1,
    ) -> Result<ApiResponse<Vec<u8>>, ClientError> {
        self.download_witness_parts(
            proof.checkpoint_sequence,
            &proof.snapshot_digest,
            &proof.witness.path,
            proof.witness.file_bytes,
        )
        .await
    }

    /// Downloads the exact snapshot witness referenced by a retrieval proof.
    ///
    /// # Errors
    ///
    /// Rejects a noncanonical reference, transport/bound/API failure, or a
    /// digest header that disagrees with the proof.
    pub async fn download_retrieval_witness(
        &self,
        proof: &RetrievalProofV1,
    ) -> Result<ApiResponse<Vec<u8>>, ClientError> {
        self.download_witness_parts(
            proof.checkpoint_sequence,
            &proof.snapshot_digest,
            &proof.witness.path,
            proof.witness.file_bytes,
        )
        .await
    }

    async fn download_witness_parts(
        &self,
        checkpoint_sequence: u64,
        snapshot_digest: &str,
        witness_path: &str,
        witness_bytes: u64,
    ) -> Result<ApiResponse<Vec<u8>>, ClientError> {
        let expected_path = format!("/v1/witnesses/{checkpoint_sequence}/{snapshot_digest}");
        if witness_path != expected_path {
            return Err(ClientError::InvalidWitnessReference);
        }
        if witness_bytes > u64::try_from(self.witness_bytes).unwrap_or(u64::MAX) {
            return Err(ClientError::ResponseTooLarge {
                maximum: self.witness_bytes,
            });
        }
        let response = self
            .request(Method::GET, expected_path.trim_start_matches('/'), true)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(self.decode_api_failure(response).await?);
        }
        if response.status() != StatusCode::OK {
            return Err(ClientError::UnexpectedStatus(response.status().as_u16()));
        }
        let request_id = request_id(response.headers())?;
        let expected_digest = format!("blake3={snapshot_digest}");
        if single_header(response.headers(), "digest") != Some(expected_digest.as_str()) {
            return Err(ClientError::WitnessDigestMismatch);
        }
        let value = read_bounded(response, self.witness_bytes).await?;
        if u64::try_from(value.len()) != Ok(witness_bytes) {
            return Err(ClientError::WitnessLengthMismatch);
        }
        Ok(ApiResponse { value, request_id })
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        authenticated: bool,
    ) -> Result<ApiResponse<T>, ClientError> {
        let response = self
            .request(Method::GET, path, authenticated)
            .send()
            .await?;
        self.decode_json(response).await
    }

    async fn post_json<RequestBody: Serialize, ResponseBody: DeserializeOwned>(
        &self,
        path: &str,
        request: &RequestBody,
    ) -> Result<ApiResponse<ResponseBody>, ClientError> {
        let response = self
            .request(Method::POST, path, true)
            .json(request)
            .send()
            .await?;
        self.decode_json(response).await
    }

    fn request(&self, method: Method, path: &str, authenticated: bool) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, self.endpoint(path));
        if authenticated && let Some(token) = &self.bearer_token {
            request = request.header(header::AUTHORIZATION, token.clone());
        }
        request
    }

    fn endpoint(&self, path: &str) -> Url {
        let mut endpoint = self.base_url.clone();
        endpoint.set_path(&format!("/{}", path.trim_start_matches('/')));
        endpoint
    }

    async fn decode_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<ApiResponse<T>, ClientError> {
        if !response.status().is_success() {
            return Err(self.decode_api_failure(response).await?);
        }
        if response.status() != StatusCode::OK {
            return Err(ClientError::UnexpectedStatus(response.status().as_u16()));
        }
        require_json(response.headers())?;
        let request_id = request_id(response.headers())?;
        let encoded = read_bounded(response, self.response_bytes).await?;
        Ok(ApiResponse {
            value: serde_json::from_slice(&encoded)?,
            request_id,
        })
    }

    async fn decode_api_failure(
        &self,
        response: reqwest::Response,
    ) -> Result<ClientError, ClientError> {
        let status = response.status().as_u16();
        require_json(response.headers())?;
        let header_request_id = request_id(response.headers())?;
        let encoded = read_bounded(response, self.response_bytes).await?;
        let envelope: ErrorV1 = serde_json::from_slice(&encoded)?;
        if envelope.request_id != header_request_id {
            return Err(ClientError::RequestIdMismatch);
        }
        Ok(ClientError::Api(ApiFailure {
            status,
            code: envelope.code,
            message: envelope.message,
            request_id: envelope.request_id,
        }))
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, ClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(maximum).unwrap_or(u64::MAX))
    {
        return Err(ClientError::ResponseTooLarge { maximum });
    }
    let mut encoded = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let next_length = encoded
            .len()
            .checked_add(chunk.len())
            .ok_or(ClientError::ResponseTooLarge { maximum })?;
        if next_length > maximum {
            return Err(ClientError::ResponseTooLarge { maximum });
        }
        encoded.extend_from_slice(&chunk);
    }
    Ok(encoded)
}

fn require_json(headers: &HeaderMap) -> Result<(), ClientError> {
    let content_type = single_header(headers, header::CONTENT_TYPE.as_str())
        .ok_or(ClientError::InvalidContentType)?;
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type == "application/json"
        || (media_type.starts_with("application/") && media_type.ends_with("+json"))
    {
        Ok(())
    } else {
        Err(ClientError::InvalidContentType)
    }
}

fn request_id(headers: &HeaderMap) -> Result<String, ClientError> {
    single_header(headers, "x-request-id")
        .map(ToOwned::to_owned)
        .ok_or(ClientError::InvalidRequestId)
}

fn single_header<'headers>(headers: &'headers HeaderMap, name: &str) -> Option<&'headers str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ClientBuilder, ClientConfigError};

    #[test]
    fn builder_accepts_only_bounded_root_http_origins() -> Result<(), ClientConfigError> {
        ClientBuilder::new("http://127.0.0.1:8787")?.build()?;
        assert!(matches!(
            ClientBuilder::new("file:///tmp/hyphae"),
            Err(ClientConfigError::UnsupportedScheme)
        ));
        assert!(matches!(
            ClientBuilder::new("https://user@example.test/"),
            Err(ClientConfigError::NonOriginBaseUrl)
        ));
        assert!(matches!(
            ClientBuilder::new("https://example.test/prefix"),
            Err(ClientConfigError::NonOriginBaseUrl)
        ));
        Ok(())
    }

    #[test]
    fn builder_rejects_invalid_secrets_and_zero_limits() -> Result<(), ClientConfigError> {
        assert!(matches!(
            ClientBuilder::new("http://localhost")?.bearer_token("bad\nsecret"),
            Err(ClientConfigError::InvalidBearerToken)
        ));
        assert!(matches!(
            ClientBuilder::new("http://localhost")?
                .timeout(Duration::ZERO)
                .build(),
            Err(ClientConfigError::ZeroLimit)
        ));
        Ok(())
    }
}
