// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{self, Body, Bytes},
    extract::{Extension, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::stream;
use hyphae_native_product::{
    ApiKeyCredential, ApiKeyRequestAuthentication, ApiKeyRequestSessionAuthentication,
    MAX_API_KEY_CREDENTIAL_BYTES, NativeProductClient, NativeProductHandle,
    PendingTerminalCredential, ProductAuthorization, ProductCancellationToken, ProductErrorCode,
    ProductOperation, ProductPreparedHandle, ProductPrincipal, ProductResponse, ProductSessionId,
    TimingClass,
};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use uuid::Uuid;

use super::{
    NativeApiError, NativeHttpV2Config, NativeHttpV2Error, PRODUCT_MEDIA_TYPE, REQUEST_ID_HEADER,
};

const SESSION_ID_HEADER: &str = hyphae_contracts::v2::SESSION_ID_HEADER_V2;
const PROTOCOL_MINOR_HEADER: &str = hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2;
const PROTOCOL_MINOR_VALUE: &str = hyphae_contracts::v2::PROTOCOL_MINOR_VALUE_V2;
const STREAM_COMPLETION_HEADER: &str = "x-hyphae-stream-completion";
const MAX_HTTP_SESSIONS: usize = 256;
const MAX_HTTP_PREPARED_PER_SESSION: usize = 128;
pub(super) const MAX_HTTP_REQUESTS: usize = 256;
pub(super) const MAX_HTTP_BODY_READERS: usize = 256;
const MAX_HTTP_STREAMS: usize = 64;
const HTTP_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub(super) struct RequestMetadata {
    pub(super) request_id: u128,
    pub(super) binary_errors: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AuthenticatedPrincipal(pub(super) [u8; 32]);

pub(super) struct NativeHttpV2State {
    handle: NativeProductHandle,
    session_mode: NativeHttpV2SessionMode,
    limits: super::NativeHttpV2Limits,
    sessions: Mutex<BTreeMap<u128, SessionEntry>>,
    prepared_handles: Arc<Mutex<BTreeSet<u64>>>,
    active_requests: Arc<Mutex<BTreeMap<(ProductSessionId, u128), ProductCancellationToken>>>,
    session_slots: Arc<Semaphore>,
    request_slots: Arc<Semaphore>,
    auth_slots: Arc<Semaphore>,
    pub(super) body_slots: Arc<Semaphore>,
    stream_slots: Arc<Semaphore>,
}

enum NativeHttpV2SessionMode {
    Unmanaged {
        bearer_token: Option<crate::BearerToken>,
    },
    Managed {
        legacy_bearer_token: Option<crate::BearerToken>,
    },
}

#[derive(Clone)]
pub(super) struct RequestAuthentication {
    principal: AuthenticatedPrincipal,
    initial_client: Option<Arc<NativeProductClient>>,
    pending_terminal: Option<PendingTerminalCredential>,
    _request_slot: Option<Arc<OwnedSemaphorePermit>>,
}

#[derive(Clone)]
struct RequestBodyAdmission(Arc<OwnedSemaphorePermit>);

struct SessionEntry {
    session: Arc<HttpProductSession>,
    expires_at: Instant,
}

pub(super) struct HttpProductSession {
    client: Arc<NativeProductClient>,
    principal: AuthenticatedPrincipal,
    prepared: Mutex<BTreeMap<ProductPreparedHandle, PreparedBinding>>,
    prepared_slots: Arc<Semaphore>,
    _slot: OwnedSemaphorePermit,
}

struct PreparedBinding {
    internal: ProductPreparedHandle,
    _slot: OwnedSemaphorePermit,
    _lease: PreparedHandleLease,
}

struct PreparedHandleLease {
    handle: u64,
    issued: Arc<Mutex<BTreeSet<u64>>>,
}

impl Drop for PreparedHandleLease {
    fn drop(&mut self) {
        if let Ok(mut issued) = self.issued.lock() {
            issued.remove(&self.handle);
        }
    }
}

/// Optional HTTP edge adapter over an already-running one-owner product service.
///
/// This type has no data-directory path and cannot open `HyphaeEngine`,
/// format-2 state, or a second `NativeProduct`. All execution goes through the
/// supplied [`NativeProductHandle`].
pub struct NativeHttpV2Server {
    bind: SocketAddr,
    pub(super) state: Arc<NativeHttpV2State>,
}

impl NativeHttpV2Server {
    /// Selects authentication from the service catalog and validates HTTP policy.
    ///
    /// An empty catalog preserves the unmanaged compatibility adapter. A
    /// bootstrapped catalog automatically selects managed API-key sessions.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the listener or HTTP limits are invalid.
    pub fn new(
        handle: NativeProductHandle,
        config: NativeHttpV2Config,
    ) -> Result<Self, NativeHttpV2Error> {
        if handle.access_control_bootstrapped() {
            let legacy_state = handle.legacy_bearer_state();
            config.validate_managed_legacy(legacy_state)?;
            if let Some(candidate) = config.legacy_bearer_token.as_ref()
                && !handle
                    .legacy_bearer_digest_matches(candidate.digest())
                    .map_err(Box::new)?
            {
                return Err(super::NativeHttpV2ConfigError::LegacyBearerMismatch.into());
            }
            let bind = config.bind;
            let limits = config.limits;
            return Ok(Self::configured(
                handle,
                bind,
                limits,
                NativeHttpV2SessionMode::Managed {
                    legacy_bearer_token: config.legacy_bearer_token,
                },
            ));
        }
        config.validate()?;
        let session_mode = NativeHttpV2SessionMode::Unmanaged {
            bearer_token: config.bearer_token,
        };
        Ok(Self::configured(
            handle,
            config.bind,
            config.limits,
            session_mode,
        ))
    }

    /// Enables catalog-managed `hyp1_` API-key authentication for every request.
    ///
    /// This force-managed constructor derives every product session from
    /// [`NativeProductHandle::open_authenticated_session`]. The legacy fixed
    /// bearer configuration field must be unset so catalog RBAC remains the
    /// sole authentication authority. This plaintext adapter must remain on a
    /// loopback listener; remote managed exposure requires a TLS-terminating
    /// proxy in front of that loopback endpoint.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when HTTP limits are invalid, a legacy
    /// fixed bearer is also configured, or the listener is not loopback.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "constructor parity intentionally consumes secret-bearing configuration"
    )]
    pub fn new_managed(
        handle: NativeProductHandle,
        config: NativeHttpV2Config,
    ) -> Result<Self, NativeHttpV2Error> {
        config.validate_managed()?;
        if config.legacy_bearer_token.is_some() {
            return Err(
                super::NativeHttpV2ConfigError::LegacyStateConfigurationMismatch {
                    state: handle.legacy_bearer_state(),
                    configured: true,
                }
                .into(),
            );
        }
        let bind = config.bind;
        let limits = config.limits;
        Ok(Self::configured(
            handle,
            bind,
            limits,
            NativeHttpV2SessionMode::Managed {
                legacy_bearer_token: None,
            },
        ))
    }

    fn configured(
        handle: NativeProductHandle,
        bind: SocketAddr,
        limits: super::NativeHttpV2Limits,
        session_mode: NativeHttpV2SessionMode,
    ) -> Self {
        Self {
            bind,
            state: Arc::new(NativeHttpV2State {
                handle,
                session_mode,
                limits,
                sessions: Mutex::new(BTreeMap::new()),
                prepared_handles: Arc::new(Mutex::new(BTreeSet::new())),
                active_requests: Arc::new(Mutex::new(BTreeMap::new())),
                session_slots: Arc::new(Semaphore::new(MAX_HTTP_SESSIONS)),
                request_slots: Arc::new(Semaphore::new(MAX_HTTP_REQUESTS)),
                auth_slots: Arc::new(Semaphore::new(MAX_HTTP_REQUESTS)),
                body_slots: Arc::new(Semaphore::new(MAX_HTTP_BODY_READERS)),
                stream_slots: Arc::new(Semaphore::new(MAX_HTTP_STREAMS)),
            }),
        }
    }

    /// Binds the configured listener without opening any storage authority.
    ///
    /// # Errors
    ///
    /// Returns a bind or local-address inspection error.
    pub async fn bind(self) -> Result<BoundNativeHttpV2Server, NativeHttpV2Error> {
        let listener =
            TcpListener::bind(self.bind)
                .await
                .map_err(|source| NativeHttpV2Error::Bind {
                    address: self.bind,
                    source,
                })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| NativeHttpV2Error::Bind {
                address: self.bind,
                source,
            })?;
        Ok(BoundNativeHttpV2Server {
            listener,
            local_addr,
            router: build_router(self.state),
        })
    }

    #[cfg(test)]
    pub(super) fn test_router(&self) -> Router {
        build_router(Arc::clone(&self.state))
    }

    #[cfg(test)]
    pub(super) fn test_cancellation(&self, request_id: u128) -> Option<ProductCancellationToken> {
        let active = self.state.active_requests.lock().ok()?;
        let mut matches = active
            .iter()
            .filter(|((_, candidate), _)| *candidate == request_id)
            .map(|(_, token)| token.clone());
        let token = matches.next()?;
        matches.next().is_none().then_some(token)
    }

    #[cfg(test)]
    pub(super) fn test_register_cancellation(
        &self,
        session_id: ProductSessionId,
        request_id: u128,
    ) -> Result<CancellationGuard, NativeApiError> {
        let metadata = RequestMetadata {
            request_id,
            binary_errors: false,
        };
        CancellationGuard::new(
            ProductCancellationToken::new(),
            Arc::clone(&self.state.active_requests),
            (session_id, request_id),
            &metadata,
        )
    }

    #[cfg(test)]
    pub(super) fn test_product_session_id(&self, session_id: u128) -> Option<ProductSessionId> {
        self.state
            .sessions
            .lock()
            .ok()?
            .get(&session_id)
            .map(|entry| entry.session.client.session_id())
    }

    #[cfg(test)]
    pub(super) fn test_authentication_permits(&self) -> usize {
        self.state.auth_slots.available_permits()
    }

    #[cfg(test)]
    pub(super) fn test_request_permits(&self) -> usize {
        self.state.request_slots.available_permits()
    }

    #[cfg(test)]
    pub(super) fn test_unmanaged_authentication(
        principal: AuthenticatedPrincipal,
    ) -> RequestAuthentication {
        RequestAuthentication {
            principal,
            initial_client: None,
            pending_terminal: None,
            _request_slot: None,
        }
    }

    /// Builds an in-process router for SDK parity and embedding tests.
    ///
    /// The router owns no storage authority; it continues to dispatch through
    /// the supplied one-owner product-service handle.
    pub fn into_router(self) -> Router {
        build_router(self.state)
    }
}

/// Bound Native HTTP v2 listener awaiting a shutdown signal.
pub struct BoundNativeHttpV2Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    router: Router,
}

impl BoundNativeHttpV2Server {
    /// Returns the actual listener address, including an assigned ephemeral port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until graceful shutdown resolves.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP listener fails while serving.
    pub async fn run_with_shutdown<F>(self, shutdown: F) -> Result<(), NativeHttpV2Error>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(NativeHttpV2Error::Serve)
    }
}

#[derive(Clone, Copy)]
pub(super) enum OperationFamily {
    Any,
    Catalog,
    Sql,
    Structures,
    Search,
    SearchCollection,
    SearchIngest,
    SearchDocument,
    Admin,
    Telemetry,
    Doctor,
    Backup,
    Restore,
    ProofVerify,
    Transaction,
    Security,
}

#[allow(clippy::needless_pass_by_value)]
fn build_router(state: Arc<NativeHttpV2State>) -> Router {
    Router::new()
        .route("/v2/capabilities", get(capabilities))
        .route("/v2/execute", post(execute_any))
        .route("/v2/catalog", post(execute_catalog))
        .route("/v2/sql", post(execute_sql))
        .route("/v2/structures", post(execute_structures))
        .route("/v2/search", post(execute_search))
        .route("/v2/search/collection", post(execute_search_collection))
        .route("/v2/search/ingest", post(execute_search_ingest))
        .route("/v2/search/document", post(execute_search_document))
        .route("/v2/admin", post(execute_admin))
        .route("/v2/telemetry", post(execute_telemetry))
        .route("/v2/doctor", post(execute_doctor))
        .route("/v2/backup", post(execute_backup))
        .route("/v2/restore", post(execute_restore))
        .route("/v2/proofs/verify", post(execute_proof_verify))
        .route("/v2/transactions/status", post(execute_transaction))
        .route("/v2/security/keys", post(execute_security))
        .route("/v2/read-stream", post(read_stream))
        .route("/v1", get(v1_unmappable).post(v1_unmappable))
        .route("/v1/{*path}", get(v1_unmappable).post(v1_unmappable))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authenticate,
        ))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            admit_request_body,
        ))
        .layer(middleware::from_fn(require_supported_protocol_minor))
        .layer(middleware::from_fn(assign_request_id))
}

async fn admit_request_body(
    State(state): State<Arc<NativeHttpV2State>>,
    Extension(metadata): Extension<RequestMetadata>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.method() == axum::http::Method::POST {
        if let Err(error) = validate_request_headers(request.headers(), &metadata, &state) {
            return error.into_response();
        }
        let Ok(permit) = Arc::clone(&state.body_slots).try_acquire_owned() else {
            return NativeApiError::code(ProductErrorCode::Unavailable, &metadata).into_response();
        };
        request
            .extensions_mut()
            .insert(RequestBodyAdmission(Arc::new(permit)));
    }
    next.run(request).await
}

fn validate_request_headers(
    headers: &HeaderMap,
    metadata: &RequestMetadata,
    state: &NativeHttpV2State,
) -> Result<(), NativeApiError> {
    parse_deadline_header(headers, metadata)?;
    parse_session_header(headers, metadata)?;
    require_product_content_type(headers, metadata)?;
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    if let Some(value) = lengths.next() {
        if lengths.next().is_some() {
            return Err(NativeApiError::code(
                ProductErrorCode::InvalidRequest,
                metadata,
            ));
        }
        let length = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| NativeApiError::code(ProductErrorCode::InvalidRequest, metadata))?;
        if length > state.limits.request_body_bytes {
            return Err(NativeApiError::payload_too_large(metadata));
        }
    }
    Ok(())
}

async fn assign_request_id(mut request: Request, next: Next) -> Response {
    let binary_errors = accepts_binary_errors(request.headers());
    let request_id = match request_id_header(request.headers()) {
        Ok(Some(value)) => value,
        Ok(None) => Uuid::now_v7().as_u128(),
        Err(()) => {
            let metadata = RequestMetadata {
                request_id: Uuid::now_v7().as_u128(),
                binary_errors,
            };
            return NativeApiError::code(ProductErrorCode::InvalidRequest, &metadata)
                .into_response();
        }
    };
    let metadata = RequestMetadata {
        request_id,
        binary_errors,
    };
    request.extensions_mut().insert(metadata.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

async fn require_supported_protocol_minor(request: Request, next: Next) -> Response {
    if request.uri().path() != "/v2" && !request.uri().path().starts_with("/v2/") {
        return next.run(request).await;
    }
    let metadata = request.extensions().get::<RequestMetadata>().cloned();
    let supported = request
        .headers()
        .get_all(PROTOCOL_MINOR_HEADER)
        .iter()
        .collect::<Vec<_>>();
    if supported.len() != 1 || supported[0].as_bytes() != PROTOCOL_MINOR_VALUE.as_bytes() {
        let metadata = metadata.unwrap_or(RequestMetadata {
            request_id: Uuid::now_v7().as_u128(),
            binary_errors: accepts_binary_errors(request.headers()),
        });
        return NativeApiError::code(ProductErrorCode::InvalidRequest, &metadata).into_response();
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        PROTOCOL_MINOR_HEADER,
        HeaderValue::from_static(PROTOCOL_MINOR_VALUE),
    );
    response
}

async fn authenticate(
    State(state): State<Arc<NativeHttpV2State>>,
    Extension(metadata): Extension<RequestMetadata>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_slot = match Arc::clone(&state.request_slots).try_acquire_owned() {
        Ok(permit) => Arc::new(permit),
        Err(_) => {
            return NativeApiError::code(ProductErrorCode::Unavailable, &metadata).into_response();
        }
    };
    let authentication = match &state.session_mode {
        NativeHttpV2SessionMode::Unmanaged { bearer_token } => {
            let principal = match bearer_token {
                None => Some(*blake3::hash(b"anonymous-loopback").as_bytes()),
                Some(expected) => bearer_candidate(request.headers()).and_then(|candidate| {
                    expected
                        .verifies(candidate)
                        .then(|| *blake3::hash(candidate).as_bytes())
                }),
            };
            principal
                .ok_or_else(|| NativeApiError::unauthorized(&metadata))
                .and_then(|principal| {
                    let principal = AuthenticatedPrincipal(principal);
                    let initial_client = if request.headers().contains_key(SESSION_ID_HEADER) {
                        None
                    } else {
                        Some(Arc::new(open_unmanaged_product_client(
                            &state, &principal, &metadata,
                        )?))
                    };
                    Ok(RequestAuthentication {
                        principal,
                        initial_client,
                        pending_terminal: None,
                        _request_slot: Some(Arc::clone(&request_slot)),
                    })
                })
        }
        NativeHttpV2SessionMode::Managed {
            legacy_bearer_token,
        } => {
            authenticate_managed_request(
                &state,
                request.headers(),
                &metadata,
                legacy_bearer_token.as_ref(),
                Arc::clone(&request_slot),
            )
            .await
        }
    };
    let authentication = match authentication {
        Ok(authentication) => authentication,
        Err(error) => return error.into_response(),
    };
    if authentication.pending_terminal.is_some()
        && (request.method() != axum::http::Method::POST
            || request.uri().path() != "/v2/security/keys")
    {
        return NativeApiError::unauthorized(&metadata).into_response();
    }
    request.extensions_mut().insert(authentication);
    next.run(request).await
}

async fn authenticate_managed_request(
    state: &NativeHttpV2State,
    headers: &HeaderMap,
    metadata: &RequestMetadata,
    legacy_bearer_token: Option<&crate::BearerToken>,
    request_slot: Arc<OwnedSemaphorePermit>,
) -> Result<RequestAuthentication, NativeApiError> {
    let raw_candidate = bearer_candidate(headers)
        .and_then(|candidate| std::str::from_utf8(candidate).ok())
        .ok_or_else(|| NativeApiError::unauthorized(metadata))?;
    if raw_candidate.starts_with("hyp1_") {
        let candidate = managed_bearer_candidate(headers)
            .ok_or_else(|| NativeApiError::unauthorized(metadata))?;
        return authenticate_canonical_request(state, headers, metadata, candidate, request_slot)
            .await;
    }
    let expected = legacy_bearer_token.ok_or_else(|| NativeApiError::unauthorized(metadata))?;
    if !expected.verifies(raw_candidate.as_bytes()) {
        return Err(NativeApiError::unauthorized(metadata));
    }
    if !headers.contains_key(SESSION_ID_HEADER) {
        let handle = state.handle.clone();
        let digest = expected.digest();
        let durable_match = tokio::task::spawn_blocking(move || {
            handle
                .legacy_bearer_digest_matches(digest)
                .map_err(Box::new)
        })
        .await
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
        .map_err(|error| NativeApiError::product(*error, metadata))?;
        if !durable_match {
            return Err(NativeApiError::unauthorized(metadata));
        }
    }
    let principal = AuthenticatedPrincipal(legacy_credential_fingerprint(raw_candidate.as_bytes()));
    if headers.contains_key(SESSION_ID_HEADER) {
        return Ok(RequestAuthentication {
            principal,
            initial_client: None,
            pending_terminal: None,
            _request_slot: Some(request_slot),
        });
    }
    let handle = state.handle.clone();
    let credential = hyphae_native_product::LegacyBearerCredential::new(raw_candidate.as_bytes())
        .map_err(|_| NativeApiError::unauthorized(metadata))?;
    let client = tokio::task::spawn_blocking(move || {
        handle
            .open_legacy_owner_session(credential)
            .map_err(Box::new)
    })
    .await
    .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
    .map_err(|_| NativeApiError::unauthorized(metadata))?;
    Ok(RequestAuthentication {
        principal,
        initial_client: Some(Arc::new(client)),
        pending_terminal: None,
        _request_slot: Some(request_slot),
    })
}

async fn authenticate_canonical_request(
    state: &NativeHttpV2State,
    headers: &HeaderMap,
    metadata: &RequestMetadata,
    candidate: &str,
    request_slot: Arc<OwnedSemaphorePermit>,
) -> Result<RequestAuthentication, NativeApiError> {
    let principal = AuthenticatedPrincipal(managed_credential_fingerprint(candidate.as_bytes()));
    let (initial_client, pending_terminal) = if headers.contains_key(SESSION_ID_HEADER) {
        let auth_slot = Arc::clone(&state.auth_slots)
            .try_acquire_owned()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
        let credential =
            ApiKeyCredential::new(candidate).map_err(|_| NativeApiError::unauthorized(metadata))?;
        let handle = state.handle.clone();
        let authentication = tokio::task::spawn_blocking(move || {
            let _auth_slot = auth_slot;
            handle
                .authenticate_api_key_request(credential)
                .map_err(Box::new)
        })
        .await
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
        .map_err(|error| NativeApiError::product(*error, metadata))?;
        match authentication {
            ApiKeyRequestAuthentication::Authenticated(_) => (None, None),
            ApiKeyRequestAuthentication::PendingTerminal(pending) => (None, Some(pending)),
        }
    } else {
        let auth_slot = Arc::clone(&state.auth_slots)
            .try_acquire_owned()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
        let credential =
            ApiKeyCredential::new(candidate).map_err(|_| NativeApiError::unauthorized(metadata))?;
        let handle = state.handle.clone();
        let authentication = tokio::task::spawn_blocking(move || {
            let _auth_slot = auth_slot;
            handle
                .open_api_key_request_session(credential)
                .map_err(Box::new)
        })
        .await
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
        .map_err(|error| NativeApiError::product(*error, metadata))?;
        match authentication {
            ApiKeyRequestSessionAuthentication::Authenticated(client) => {
                (Some(Arc::new(client)), None)
            }
            ApiKeyRequestSessionAuthentication::PendingTerminal(pending) => (None, Some(pending)),
        }
    };
    Ok(RequestAuthentication {
        principal,
        initial_client,
        pending_terminal,
        _request_slot: Some(request_slot),
    })
}

async fn capabilities(
    State(state): State<Arc<NativeHttpV2State>>,
    Extension(metadata): Extension<RequestMetadata>,
    Extension(authentication): Extension<RequestAuthentication>,
) -> Result<Response, NativeApiError> {
    execute_operation(
        state,
        metadata,
        hyphae_native_protocol::WireRequest {
            operation: ProductOperation::Capabilities,
            logical_time_micros: 0,
            deadline_micros: None,
            idempotency_token: None,
            limits: hyphae_native_product::ProductLimits::default(),
            durability: hyphae_native_product::ProductDurabilityPolicy::STRICT,
        },
        Duration::ZERO,
        false,
        authentication,
        None,
    )
    .await
}

macro_rules! family_handler {
    ($name:ident, $family:ident) => {
        async fn $name(
            State(state): State<Arc<NativeHttpV2State>>,
            Extension(metadata): Extension<RequestMetadata>,
            Extension(authentication): Extension<RequestAuthentication>,
            request: Request,
        ) -> Result<Response, NativeApiError> {
            execute_request(
                state,
                metadata,
                request,
                OperationFamily::$family,
                false,
                authentication,
            )
            .await
        }
    };
}

family_handler!(execute_any, Any);
family_handler!(execute_catalog, Catalog);
family_handler!(execute_sql, Sql);
family_handler!(execute_structures, Structures);
family_handler!(execute_search, Search);
family_handler!(execute_search_collection, SearchCollection);
family_handler!(execute_search_ingest, SearchIngest);
family_handler!(execute_search_document, SearchDocument);
family_handler!(execute_admin, Admin);
family_handler!(execute_telemetry, Telemetry);
family_handler!(execute_doctor, Doctor);
family_handler!(execute_backup, Backup);
family_handler!(execute_restore, Restore);
family_handler!(execute_proof_verify, ProofVerify);
family_handler!(execute_transaction, Transaction);
family_handler!(execute_security, Security);

async fn read_stream(
    State(state): State<Arc<NativeHttpV2State>>,
    Extension(metadata): Extension<RequestMetadata>,
    Extension(authentication): Extension<RequestAuthentication>,
    request: Request,
) -> Result<Response, NativeApiError> {
    execute_request(
        state,
        metadata,
        request,
        OperationFamily::Any,
        true,
        authentication,
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn execute_request(
    state: Arc<NativeHttpV2State>,
    metadata: RequestMetadata,
    mut request: Request,
    family: OperationFamily,
    stream_response: bool,
    authentication: RequestAuthentication,
) -> Result<Response, NativeApiError> {
    let pending_terminal = authentication.pending_terminal.is_some();
    let rejection = |code| {
        if pending_terminal {
            NativeApiError::unauthorized(&metadata)
        } else {
            NativeApiError::code(code, &metadata)
        }
    };
    let deadline_header = parse_deadline_header(request.headers(), &metadata)?;
    let session_id = parse_session_header(request.headers(), &metadata)?;
    require_product_content_type(request.headers(), &metadata)?;
    let request_admission = request
        .extensions_mut()
        .remove::<RequestBodyAdmission>()
        .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, &metadata))?;
    let _request_admission = request_admission.0;
    let body = tokio::time::timeout(
        state.limits.request_body_timeout,
        body::to_bytes(request.into_body(), state.limits.request_body_bytes),
    )
    .await
    .map_err(|_| rejection(ProductErrorCode::DeadlineExceeded))?
    .map_err(|_| {
        if pending_terminal {
            NativeApiError::unauthorized(&metadata)
        } else {
            NativeApiError::payload_too_large(&metadata)
        }
    })?;
    let decode_started = Instant::now();
    let mut wire = hyphae_native_protocol::decode_product_request(&body)
        .map_err(|_| rejection(ProductErrorCode::InvalidRequest))?;
    let decode_time = decode_started.elapsed();
    if deadline_header.is_some() && deadline_header != wire.deadline_micros {
        return Err(rejection(ProductErrorCode::InvalidRequest));
    }
    if !family_accepts(family, &wire.operation)
        || (stream_response && !is_read_operation(&wire.operation))
    {
        return Err(rejection(ProductErrorCode::InvalidRequest));
    }
    if matches!(family, OperationFamily::Security) {
        if !matches!(state.session_mode, NativeHttpV2SessionMode::Managed { .. }) {
            return Err(rejection(ProductErrorCode::AuthorizationDenied));
        }
        if wire.durability != hyphae_native_product::ProductDurabilityPolicy::STRICT {
            return Err(rejection(ProductErrorCode::InvalidRequest));
        }
    }
    let mut authentication = authentication;
    if let Some(pending) = authentication.pending_terminal.take() {
        let Some(idempotency_token) = wire.idempotency_token else {
            return Err(NativeApiError::unauthorized(&metadata));
        };
        let operation = wire.operation.clone();
        let handle = state.handle.clone();
        let existing_session = session_id.is_some();
        let authenticated = tokio::task::spawn_blocking(move || {
            if existing_session {
                handle
                    .authenticate_exact_terminal_replay(pending, idempotency_token, operation)
                    .map(|()| None)
                    .map_err(Box::new)
            } else {
                handle
                    .open_exact_terminal_replay_session(pending, idempotency_token, operation)
                    .map(Some)
                    .map_err(Box::new)
            }
        })
        .await
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, &metadata))?
        .map_err(|error| {
            if error.code() == ProductErrorCode::AuthorizationDenied {
                NativeApiError::unauthorized(&metadata)
            } else {
                NativeApiError::product(*error, &metadata)
            }
        })?;
        if let Some(client) = authenticated {
            authentication.initial_client = Some(Arc::new(client));
        }
    }
    wire.limits.max_request_bytes = wire
        .limits
        .max_request_bytes
        .min(state.limits.request_body_bytes);
    wire.limits.max_response_bytes = wire
        .limits
        .max_response_bytes
        .min(state.limits.response_bytes);
    execute_operation(
        state,
        metadata,
        wire,
        decode_time,
        stream_response,
        authentication,
        session_id,
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn execute_operation(
    state: Arc<NativeHttpV2State>,
    metadata: RequestMetadata,
    wire: hyphae_native_protocol::WireRequest,
    decode_time: Duration,
    stream_response: bool,
    authentication: RequestAuthentication,
    requested_session: Option<u128>,
) -> Result<Response, NativeApiError> {
    let transport_started = Instant::now();
    let mut operation = wire.operation;
    let one_time_secret = matches!(
        operation,
        ProductOperation::SecurityApiKeyIssueSelfStart { .. }
            | ProductOperation::SecurityApiKeyIssueStart { .. }
            | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
            | ProductOperation::SecurityApiKeyRotateStart { .. }
    );
    let mut new_session = None;
    let session = if operation_requires_existing_session(&operation) {
        let session_id = requested_session
            .ok_or_else(|| NativeApiError::code(ProductErrorCode::SqlInvalidValue, &metadata))?;
        Some(lookup_session(
            &state,
            session_id,
            &authentication.principal,
            &metadata,
        )?)
    } else if operation_starts_session(&operation) {
        if let Some(session_id) = requested_session {
            Some(lookup_session(
                &state,
                session_id,
                &authentication.principal,
                &metadata,
            )?)
        } else {
            let created = create_session(&state, authentication.clone(), &metadata)?;
            new_session = Some(created.0);
            Some(created.1)
        }
    } else if let Some(session_id) = requested_session {
        Some(lookup_session(
            &state,
            session_id,
            &authentication.principal,
            &metadata,
        )?)
    } else {
        None
    };

    let client = if let Some(session) = &session {
        Arc::clone(&session.client)
    } else {
        product_client_for_request(&state, &authentication, &metadata)?
    };
    let token = ProductCancellationToken::new();
    // Reserve correlation identity before service admission. A duplicate
    // cannot enter the queue or replace this cancellation token.
    let cancellation = CancellationGuard::new(
        token.clone(),
        Arc::clone(&state.active_requests),
        (client.session_id(), metadata.request_id),
        &metadata,
    )?;

    let prepared_slot = if matches!(operation, ProductOperation::PrepareSql { .. }) {
        let session = session
            .as_ref()
            .ok_or_else(|| NativeApiError::code(ProductErrorCode::InvalidRequest, &metadata))?;
        Some(
            Arc::clone(&session.prepared_slots)
                .try_acquire_owned()
                .map_err(|_| NativeApiError::code(ProductErrorCode::LimitExceeded, &metadata))?,
        )
    } else {
        None
    };

    let external_prepared = match &mut operation {
        ProductOperation::ExecutePrepared { handle, .. }
        | ProductOperation::DeallocatePrepared { handle } => {
            let session = session.as_ref().ok_or_else(|| {
                NativeApiError::code(ProductErrorCode::SqlInvalidValue, &metadata)
            })?;
            let external = *handle;
            *handle = session
                .prepared
                .lock()
                .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, &metadata))?
                .get(&external)
                .map(|binding| binding.internal)
                .ok_or_else(|| {
                    NativeApiError::code(ProductErrorCode::SqlInvalidValue, &metadata)
                })?;
            Some(external)
        }
        _ => None,
    };

    client.record_timing(TimingClass::RequestDecoding, decode_time);
    let mut context = client.request_context(metadata.request_id, wire.logical_time_micros);
    context.idempotency_token = wire.idempotency_token;
    context.deadline_micros = wire.deadline_micros;
    context.cancellation = token.clone();
    context.limits = wire.limits;
    context.durability = wire.durability;
    context
        .checkpoint()
        .map_err(|error| NativeApiError::product(error, &metadata))?;
    let pending = client
        .try_submit(context, operation)
        .map_err(|error| NativeApiError::product(error, &metadata))?;
    let task = tokio::task::spawn_blocking(move || pending.wait().map_err(Box::new));
    let mut response = wait_for_product(task, &token, wire.deadline_micros, &metadata).await?;

    let mut inserted_prepared = None;
    if let ProductResponse::PreparedSql { handle, .. } = &mut response {
        let session = session
            .as_ref()
            .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, &metadata))?;
        let external = insert_prepared(
            &state,
            session,
            *handle,
            prepared_slot
                .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, &metadata))?,
            &metadata,
        )?;
        *handle = external;
        inserted_prepared = Some(external);
    } else if let (ProductResponse::Deallocated, Some(external), Some(session)) =
        (&response, external_prepared, &session)
    {
        session
            .prepared
            .lock()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, &metadata))?
            .remove(&external);
    }

    completion_checkpoint(&token, wire.deadline_micros, &metadata)?;
    let encoding_started = Instant::now();
    let encoded = hyphae_native_protocol::encode_product_response(&response)
        .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, &metadata))?;
    client.record_timing(TimingClass::ResultEncoding, encoding_started.elapsed());
    client.record_timing(TimingClass::Transport, transport_started.elapsed());
    let maximum = state
        .limits
        .response_bytes
        .min(wire.limits.max_response_bytes);
    if encoded.len() > maximum {
        remove_prepared(session.as_ref(), inserted_prepared, &metadata)?;
        return Err(NativeApiError::payload_too_large(&metadata));
    }
    if let Err(error) = completion_checkpoint(&token, wire.deadline_micros, &metadata) {
        remove_prepared(session.as_ref(), inserted_prepared, &metadata)?;
        return Err(error);
    }

    let response_session = requested_session.or(new_session);
    if stream_response {
        let permit = Arc::clone(&state.stream_slots)
            .try_acquire_owned()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, &metadata))?;
        return incremental_ndjson_response(
            encoded,
            maximum,
            state.limits.stream_chunk_bytes,
            &metadata,
            token,
            wire.deadline_micros,
            cancellation,
            permit,
            response_session,
        );
    }
    if let Some(session_id) = new_session {
        insert_session(
            &state,
            session_id,
            session.ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, &metadata))?,
            &metadata,
        )?;
    }
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
        .header(header::CONTENT_LENGTH, encoded.len());
    if one_time_secret {
        builder = builder
            .header(header::CACHE_CONTROL, "no-store, private, max-age=0")
            .header(header::PRAGMA, "no-cache");
    }
    if let Some(session_id) = response_session {
        builder = builder.header(SESSION_ID_HEADER, format!("{session_id:032x}"));
    }
    let body_state = Some(ResponseBodyState {
        encoded: Some(Bytes::from(encoded)),
        cancellation,
        token,
        deadline_micros: wire.deadline_micros,
    });
    let body = Body::from_stream(stream::unfold(body_state, |state| async move {
        let mut state = state?;
        if state.should_stop() {
            return None;
        }
        if let Some(encoded) = state.encoded.take() {
            Some((Ok::<_, Infallible>(encoded), Some(state)))
        } else {
            state.cancellation.complete();
            None
        }
    }));
    let response = builder
        .body(body)
        .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, &metadata))?;
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn incremental_ndjson_response(
    encoded: Vec<u8>,
    maximum: usize,
    chunk_bytes: usize,
    metadata: &RequestMetadata,
    token: ProductCancellationToken,
    deadline_micros: Option<i64>,
    cancellation: CancellationGuard,
    permit: OwnedSemaphorePermit,
    response_session: Option<u128>,
) -> Result<Response, NativeApiError> {
    let digest_hex = blake3::hash(&encoded).to_hex().to_string();
    let chunks = encoded.len().div_ceil(chunk_bytes);
    let mut total = 0_usize;
    for (sequence, chunk) in encoded.chunks(chunk_bytes).enumerate() {
        let line = provisional_record(sequence, chunk, metadata)?;
        total = total.saturating_add(line.len());
        if total > maximum {
            return Err(NativeApiError::payload_too_large(metadata));
        }
    }
    let completion = completion_record(
        metadata.request_id,
        chunks,
        encoded.len(),
        &digest_hex,
        metadata,
    )?;
    total = total.saturating_add(completion.len());
    if total > maximum {
        return Err(NativeApiError::payload_too_large(metadata));
    }

    let state = Some(NdjsonState {
        encoded: Bytes::from(encoded),
        offset: 0,
        sequence: 0,
        chunk_bytes,
        chunks,
        digest_hex,
        metadata: metadata.clone(),
        token,
        deadline_micros,
        cancellation,
        _permit: permit,
    });
    let body = Body::from_stream(stream::unfold(state, |state| async move {
        let mut state = state?;
        if state.should_stop() {
            return None;
        }
        if state.offset < state.encoded.len() {
            let end = state
                .offset
                .saturating_add(state.chunk_bytes)
                .min(state.encoded.len());
            let line = provisional_record(
                state.sequence,
                &state.encoded[state.offset..end],
                &state.metadata,
            )
            .ok()?;
            state.offset = end;
            state.sequence = state.sequence.saturating_add(1);
            return Some((Ok::<_, Infallible>(line), Some(state)));
        }
        let line = completion_record(
            state.metadata.request_id,
            state.chunks,
            state.encoded.len(),
            &state.digest_hex,
            &state.metadata,
        )
        .ok()?;
        state.cancellation.complete();
        Some((Ok::<_, Infallible>(line), None))
    }));
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(STREAM_COMPLETION_HEADER, "required");
    if let Some(session_id) = response_session {
        builder = builder.header(SESSION_ID_HEADER, format!("{session_id:032x}"));
    }
    builder
        .body(body)
        .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, metadata))
}

struct NdjsonState {
    encoded: Bytes,
    offset: usize,
    sequence: usize,
    chunk_bytes: usize,
    chunks: usize,
    digest_hex: String,
    metadata: RequestMetadata,
    token: ProductCancellationToken,
    deadline_micros: Option<i64>,
    cancellation: CancellationGuard,
    _permit: OwnedSemaphorePermit,
}

impl NdjsonState {
    fn should_stop(&self) -> bool {
        if self.token.is_cancelled() {
            return true;
        }
        if self
            .deadline_micros
            .is_some_and(|deadline| unix_time_micros() >= deadline)
        {
            self.token.cancel();
            return true;
        }
        false
    }
}

fn provisional_record(
    sequence: usize,
    chunk: &[u8],
    metadata: &RequestMetadata,
) -> Result<Bytes, NativeApiError> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "type": "data",
        "provisional": true,
        "sequence": sequence,
        "data_base64": BASE64.encode(chunk),
    }))
    .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, metadata))?;
    line.push(b'\n');
    Ok(Bytes::from(line))
}

fn completion_record(
    request_id: u128,
    chunks: usize,
    response_bytes: usize,
    digest_hex: &str,
    metadata: &RequestMetadata,
) -> Result<Bytes, NativeApiError> {
    let mut completion = serde_json::to_vec(&serde_json::json!({
        "type": "completion",
        "status": "complete",
        "request_id": request_id.to_string(),
        "chunks": chunks,
        "response_bytes": response_bytes,
        "digest_hex": digest_hex,
    }))
    .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, metadata))?;
    completion.push(b'\n');
    Ok(Bytes::from(completion))
}

pub(super) struct CancellationGuard {
    pub(super) token: ProductCancellationToken,
    active_requests: Arc<Mutex<BTreeMap<(ProductSessionId, u128), ProductCancellationToken>>>,
    key: (ProductSessionId, u128),
    armed: bool,
}

struct ResponseBodyState {
    encoded: Option<Bytes>,
    cancellation: CancellationGuard,
    token: ProductCancellationToken,
    deadline_micros: Option<i64>,
}

impl ResponseBodyState {
    fn should_stop(&self) -> bool {
        if self.token.is_cancelled() {
            return true;
        }
        if self
            .deadline_micros
            .is_some_and(|deadline| unix_time_micros() >= deadline)
        {
            self.token.cancel();
            return true;
        }
        false
    }
}

impl CancellationGuard {
    fn new(
        token: ProductCancellationToken,
        active_requests: Arc<Mutex<BTreeMap<(ProductSessionId, u128), ProductCancellationToken>>>,
        key: (ProductSessionId, u128),
        metadata: &RequestMetadata,
    ) -> Result<Self, NativeApiError> {
        let mut active = active_requests
            .lock()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
        if active.contains_key(&key) {
            return Err(NativeApiError::code(
                ProductErrorCode::InvalidRequest,
                metadata,
            ));
        }
        active.insert(key, token.clone());
        drop(active);
        Ok(Self {
            token,
            active_requests,
            key,
            armed: true,
        })
    }

    fn complete(&mut self) {
        self.armed = false;
        self.remove_active();
    }

    fn remove_active(&self) {
        if let Ok(mut active) = self.active_requests.lock() {
            active.remove(&self.key);
        }
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
        self.remove_active();
    }
}

async fn v1_unmappable(
    Extension(metadata): Extension<RequestMetadata>,
) -> Result<Response, NativeApiError> {
    Err(
        NativeApiError::code(ProductErrorCode::InvalidRequest, &metadata)
            .with_status(StatusCode::CONFLICT),
    )
}

async fn not_found(
    Extension(metadata): Extension<RequestMetadata>,
) -> Result<Response, NativeApiError> {
    Err(NativeApiError::code(
        ProductErrorCode::ObjectNotFound,
        &metadata,
    ))
}

async fn method_not_allowed(
    Extension(metadata): Extension<RequestMetadata>,
) -> Result<Response, NativeApiError> {
    Err(
        NativeApiError::code(ProductErrorCode::InvalidRequest, &metadata)
            .with_status(StatusCode::METHOD_NOT_ALLOWED),
    )
}

pub(super) fn family_accepts(family: OperationFamily, operation: &ProductOperation) -> bool {
    match family {
        OperationFamily::Any => !operation.is_key_lifecycle(),
        OperationFamily::Catalog => matches!(
            operation,
            ProductOperation::CatalogObject { .. }
                | ProductOperation::CatalogObjectNamed { .. }
                | ProductOperation::CatalogList(_)
                | ProductOperation::CatalogVisibleList(_)
                | ProductOperation::CatalogDependencies(_)
                | ProductOperation::CatalogDescribe { .. }
                | ProductOperation::CatalogResolve { .. }
                | ProductOperation::CatalogCreate { .. }
        ),
        OperationFamily::Sql => matches!(
            operation,
            ProductOperation::PrepareSql { .. }
                | ProductOperation::DeallocatePrepared { .. }
                | ProductOperation::ExecutePrepared { .. }
                | ProductOperation::ExecuteSql { .. }
                | ProductOperation::AdminExplainSql { .. }
        ),
        OperationFamily::Structures => matches!(
            operation,
            ProductOperation::StructureGet { .. }
                | ProductOperation::StructureSet { .. }
                | ProductOperation::StructureTtl { .. }
        ),
        OperationFamily::Search => matches!(operation, ProductOperation::Search { .. }),
        OperationFamily::SearchCollection => {
            matches!(operation, ProductOperation::SearchCollection { .. })
        }
        OperationFamily::SearchIngest => {
            matches!(operation, ProductOperation::SearchIngest { .. })
        }
        OperationFamily::SearchDocument => matches!(
            operation,
            ProductOperation::SearchDocumentUpdate { .. }
                | ProductOperation::SearchDocumentDelete { .. }
        ),
        OperationFamily::Admin => matches!(
            operation,
            ProductOperation::AdminStatus
                | ProductOperation::AdminCheckpoint
                | ProductOperation::AdminExplainSql { .. }
        ),
        OperationFamily::Telemetry => matches!(operation, ProductOperation::Telemetry),
        OperationFamily::Doctor => matches!(operation, ProductOperation::Doctor(_)),
        OperationFamily::Backup => matches!(operation, ProductOperation::Backup(_)),
        OperationFamily::Restore => matches!(operation, ProductOperation::Restore(_)),
        OperationFamily::ProofVerify => {
            matches!(operation, ProductOperation::VerifyProof { .. })
        }
        OperationFamily::Transaction => {
            matches!(operation, ProductOperation::TransactionStatus { .. })
        }
        OperationFamily::Security => matches!(
            operation,
            ProductOperation::SecurityApiKeyIssueSelfStart { .. }
                | ProductOperation::SecurityApiKeyIssueStart { .. }
                | ProductOperation::SecurityApiKeyIssueSelfActivate { .. }
                | ProductOperation::SecurityApiKeyIssueActivate { .. }
                | ProductOperation::SecurityApiKeyRotateSelfStart { .. }
                | ProductOperation::SecurityApiKeyRotateStart { .. }
                | ProductOperation::SecurityApiKeyRotateSelfActivate { .. }
                | ProductOperation::SecurityApiKeyRotateActivate { .. }
                | ProductOperation::SecurityApiKeyIssueSelfAbort { .. }
                | ProductOperation::SecurityApiKeyIssueAbort { .. }
                | ProductOperation::SecurityApiKeyRotateSelfAbort { .. }
                | ProductOperation::SecurityApiKeyRotateAbort { .. }
                | ProductOperation::SecurityApiKeyRevokeSelf { .. }
                | ProductOperation::SecurityApiKeyRevoke { .. }
                | ProductOperation::SecurityLegacyBearerRevoke
        ),
    }
}

fn is_read_operation(operation: &ProductOperation) -> bool {
    operation.is_read_only()
}

fn operation_requires_existing_session(operation: &ProductOperation) -> bool {
    matches!(
        operation,
        ProductOperation::ExecutePrepared { .. }
            | ProductOperation::DeallocatePrepared { .. }
            | ProductOperation::TransactionStageSql { .. }
            | ProductOperation::TransactionStageStructure { .. }
            | ProductOperation::TransactionStageSearch { .. }
            | ProductOperation::TransactionStageVector { .. }
            | ProductOperation::TransactionCommit { .. }
            | ProductOperation::TransactionRollback { .. }
            | ProductOperation::ExplicitTransactionStatus { .. }
    )
}

fn operation_starts_session(operation: &ProductOperation) -> bool {
    matches!(
        operation,
        ProductOperation::PrepareSql { .. } | ProductOperation::TransactionBegin
    )
}

fn request_id_header(headers: &HeaderMap) -> Result<Option<u128>, ()> {
    let mut values = headers.get_all(REQUEST_ID_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    let parsed = value.parse::<u128>().map_err(|_| ())?;
    if parsed == 0 || parsed.to_string() != value {
        return Err(());
    }
    Ok(Some(parsed))
}

fn parse_deadline_header(
    headers: &HeaderMap,
    metadata: &RequestMetadata,
) -> Result<Option<i64>, NativeApiError> {
    let mut values = headers
        .get_all(hyphae_contracts::v2::DEADLINE_HEADER_V2)
        .iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(NativeApiError::code(
            ProductErrorCode::InvalidRequest,
            metadata,
        ));
    }
    let parsed = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| NativeApiError::code(ProductErrorCode::InvalidRequest, metadata))?;
    Ok(Some(parsed))
}

fn parse_session_header(
    headers: &HeaderMap,
    metadata: &RequestMetadata,
) -> Result<Option<u128>, NativeApiError> {
    let mut values = headers.get_all(SESSION_ID_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(NativeApiError::code(
            ProductErrorCode::InvalidRequest,
            metadata,
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| NativeApiError::code(ProductErrorCode::InvalidRequest, metadata))?;
    let parsed = u128::from_str_radix(value, 16)
        .ok()
        .filter(|value| *value != 0)
        .filter(|parsed| format!("{parsed:032x}") == value)
        .ok_or_else(|| NativeApiError::code(ProductErrorCode::InvalidRequest, metadata))?;
    Ok(Some(parsed))
}

fn open_unmanaged_product_client(
    state: &NativeHttpV2State,
    principal: &AuthenticatedPrincipal,
    metadata: &RequestMetadata,
) -> Result<NativeProductClient, NativeApiError> {
    let principal = ProductPrincipal::new(format!("http:native-v2:{}", encode_hex(&principal.0)))
        .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, metadata))?;
    state
        .handle
        .open_session(principal, ProductAuthorization::ALL)
        .map_err(|error| NativeApiError::product(error, metadata))
}

fn product_client_for_request(
    state: &NativeHttpV2State,
    authentication: &RequestAuthentication,
    metadata: &RequestMetadata,
) -> Result<Arc<NativeProductClient>, NativeApiError> {
    if let Some(client) = &authentication.initial_client {
        return Ok(Arc::clone(client));
    }
    match &state.session_mode {
        NativeHttpV2SessionMode::Unmanaged { .. } => {
            open_unmanaged_product_client(state, &authentication.principal, metadata).map(Arc::new)
        }
        NativeHttpV2SessionMode::Managed { .. } => {
            Err(NativeApiError::code(ProductErrorCode::Internal, metadata))
        }
    }
}

pub(super) fn create_session(
    state: &NativeHttpV2State,
    authentication: RequestAuthentication,
    metadata: &RequestMetadata,
) -> Result<(u128, Arc<HttpProductSession>), NativeApiError> {
    cleanup_sessions(state, metadata)?;
    let session_id = reserve_session_id(state, metadata)?;
    let slot = Arc::clone(&state.session_slots)
        .try_acquire_owned()
        .map_err(|_| NativeApiError::code(ProductErrorCode::LimitExceeded, metadata))?;
    let client = product_client_for_request(state, &authentication, metadata)?;
    let session = Arc::new(HttpProductSession {
        client,
        principal: authentication.principal,
        prepared: Mutex::new(BTreeMap::new()),
        prepared_slots: Arc::new(Semaphore::new(MAX_HTTP_PREPARED_PER_SESSION)),
        _slot: slot,
    });
    Ok((session_id, session))
}

fn reserve_session_id(
    state: &NativeHttpV2State,
    metadata: &RequestMetadata,
) -> Result<u128, NativeApiError> {
    let sessions = state
        .sessions
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
    let mut session_id = Uuid::now_v7().as_u128();
    while session_id == 0 || sessions.contains_key(&session_id) {
        session_id = Uuid::now_v7().as_u128();
    }
    Ok(session_id)
}

pub(super) fn insert_session(
    state: &NativeHttpV2State,
    session_id: u128,
    session: Arc<HttpProductSession>,
    metadata: &RequestMetadata,
) -> Result<(), NativeApiError> {
    state
        .sessions
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
        .insert(
            session_id,
            SessionEntry {
                session,
                expires_at: Instant::now() + HTTP_SESSION_IDLE_TIMEOUT,
            },
        );
    Ok(())
}

pub(super) fn lookup_session(
    state: &NativeHttpV2State,
    session_id: u128,
    principal: &AuthenticatedPrincipal,
    metadata: &RequestMetadata,
) -> Result<Arc<HttpProductSession>, NativeApiError> {
    cleanup_sessions(state, metadata)?;
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
    let Some(entry) = sessions.get_mut(&session_id) else {
        return Err(match &state.session_mode {
            NativeHttpV2SessionMode::Unmanaged { .. } => {
                NativeApiError::code(ProductErrorCode::SqlInvalidValue, metadata)
            }
            NativeHttpV2SessionMode::Managed { .. } => NativeApiError::unauthorized(metadata),
        });
    };
    if &entry.session.principal != principal {
        return Err(match &state.session_mode {
            NativeHttpV2SessionMode::Unmanaged { .. } => {
                NativeApiError::code(ProductErrorCode::AuthorizationDenied, metadata)
            }
            NativeHttpV2SessionMode::Managed { .. } => NativeApiError::unauthorized(metadata),
        });
    }
    entry.expires_at = Instant::now() + HTTP_SESSION_IDLE_TIMEOUT;
    Ok(Arc::clone(&entry.session))
}

fn cleanup_sessions(
    state: &NativeHttpV2State,
    metadata: &RequestMetadata,
) -> Result<(), NativeApiError> {
    let now = Instant::now();
    state
        .sessions
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
        .retain(|_, entry| entry.expires_at > now || Arc::strong_count(&entry.session) > 1);
    Ok(())
}

fn insert_prepared(
    state: &NativeHttpV2State,
    session: &HttpProductSession,
    internal: ProductPreparedHandle,
    slot: OwnedSemaphorePermit,
    metadata: &RequestMetadata,
) -> Result<ProductPreparedHandle, NativeApiError> {
    let uuid = Uuid::now_v7();
    let mut material = [0_u8; 24];
    material[..16].copy_from_slice(uuid.as_bytes());
    material[16..].copy_from_slice(&internal.get().to_le_bytes());
    let digest = blake3::hash(&material);
    let mut value = [0_u8; 8];
    value.copy_from_slice(&digest.as_bytes()[..8]);
    let mut external = ProductPreparedHandle::new(u64::from_be_bytes(value).max(1))
        .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, metadata))?;
    let mut issued = state
        .prepared_handles
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
    while external == internal || !issued.insert(external.get()) {
        external = ProductPreparedHandle::new(external.get().wrapping_add(1).max(1))
            .ok_or_else(|| NativeApiError::code(ProductErrorCode::Internal, metadata))?;
    }
    drop(issued);
    let lease = PreparedHandleLease {
        handle: external.get(),
        issued: Arc::clone(&state.prepared_handles),
    };
    let mut prepared = session
        .prepared
        .lock()
        .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?;
    prepared.insert(
        external,
        PreparedBinding {
            internal,
            _slot: slot,
            _lease: lease,
        },
    );
    Ok(external)
}

fn remove_prepared(
    session: Option<&Arc<HttpProductSession>>,
    handle: Option<ProductPreparedHandle>,
    metadata: &RequestMetadata,
) -> Result<(), NativeApiError> {
    if let (Some(session), Some(handle)) = (session, handle) {
        session
            .prepared
            .lock()
            .map_err(|_| NativeApiError::code(ProductErrorCode::Unavailable, metadata))?
            .remove(&handle);
    }
    Ok(())
}

async fn wait_for_product(
    mut task: tokio::task::JoinHandle<
        Result<ProductResponse, Box<hyphae_native_product::ProductError>>,
    >,
    token: &ProductCancellationToken,
    deadline_micros: Option<i64>,
    metadata: &RequestMetadata,
) -> Result<ProductResponse, NativeApiError> {
    let result = if let Some(duration) = remaining_deadline(deadline_micros) {
        tokio::select! {
            result = &mut task => result,
            () = tokio::time::sleep(duration) => {
                token.cancel();
                return Err(NativeApiError::code(ProductErrorCode::DeadlineExceeded, metadata));
            }
        }
    } else if deadline_micros.is_some() {
        token.cancel();
        return Err(NativeApiError::code(
            ProductErrorCode::DeadlineExceeded,
            metadata,
        ));
    } else {
        task.await
    };
    result
        .map_err(|_| NativeApiError::code(ProductErrorCode::Internal, metadata))?
        .map_err(|error| NativeApiError::product(*error, metadata))
}

fn completion_checkpoint(
    token: &ProductCancellationToken,
    deadline_micros: Option<i64>,
    metadata: &RequestMetadata,
) -> Result<(), NativeApiError> {
    if token.is_cancelled() {
        return Err(NativeApiError::code(ProductErrorCode::Cancelled, metadata));
    }
    if deadline_micros.is_some_and(|deadline| unix_time_micros() >= deadline) {
        token.cancel();
        return Err(NativeApiError::code(
            ProductErrorCode::DeadlineExceeded,
            metadata,
        ));
    }
    Ok(())
}

fn remaining_deadline(deadline_micros: Option<i64>) -> Option<Duration> {
    let deadline = deadline_micros?;
    let remaining = deadline.saturating_sub(unix_time_micros());
    (remaining > 0).then(|| Duration::from_micros(u64::try_from(remaining).unwrap_or(u64::MAX)))
}

fn unix_time_micros() -> i64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_micros();
    i64::try_from(micros).unwrap_or(i64::MAX)
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

fn require_product_content_type(
    headers: &HeaderMap,
    metadata: &RequestMetadata,
) -> Result<(), NativeApiError> {
    let valid = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.eq_ignore_ascii_case(PRODUCT_MEDIA_TYPE));
    if valid {
        Ok(())
    } else {
        Err(
            NativeApiError::code(ProductErrorCode::InvalidRequest, metadata)
                .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        )
    }
}

fn accepts_binary_errors(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.trim().split(';').next())
        .any(|value| value.eq_ignore_ascii_case(super::ERROR_MEDIA_TYPE))
}

fn bearer_candidate(headers: &HeaderMap) -> Option<&[u8]> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.as_bytes();
    let separator = value.iter().position(|byte| *byte == b' ')?;
    if !value[..separator].eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    let candidate = &value[separator.saturating_add(1)..];
    (!candidate.is_empty()).then_some(candidate)
}

fn managed_bearer_candidate(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let candidate = value.strip_prefix("Bearer ")?;
    is_canonical_api_key(candidate.as_bytes()).then_some(candidate)
}

fn is_canonical_api_key(candidate: &[u8]) -> bool {
    const PREFIX_BYTES: usize = 5;
    const SEPARATOR_INDEX: usize = 37;

    candidate.len() == MAX_API_KEY_CREDENTIAL_BYTES
        && candidate.starts_with(b"hyp1_")
        && candidate.get(SEPARATOR_INDEX) == Some(&b'_')
        && candidate[PREFIX_BYTES..SEPARATOR_INDEX]
            .iter()
            .all(u8::is_ascii_hexdigit)
        && candidate[PREFIX_BYTES..SEPARATOR_INDEX]
            .iter()
            .all(|byte| !byte.is_ascii_uppercase())
        && candidate[SEPARATOR_INDEX + 1..]
            .iter()
            .all(u8::is_ascii_hexdigit)
        && candidate[SEPARATOR_INDEX + 1..]
            .iter()
            .all(|byte| !byte.is_ascii_uppercase())
        && candidate[PREFIX_BYTES..SEPARATOR_INDEX]
            .iter()
            .any(|byte| *byte != b'0')
}

fn managed_credential_fingerprint(candidate: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-http-v2-managed-credential-v1\0");
    hasher.update(candidate);
    *hasher.finalize().as_bytes()
}

fn legacy_credential_fingerprint(candidate: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-http-v2-legacy-credential-v1\0");
    hasher.update(candidate);
    *hasher.finalize().as_bytes()
}
