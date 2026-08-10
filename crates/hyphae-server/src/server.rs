// SPDX-License-Identifier: GPL-3.0-only

use std::{
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::{self, Body},
    extract::{Extension, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hyphae_contracts::v1::{
    CapabilitiesV1, CommitReceiptV1, DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1,
    DeleteRequestV1, DeleteVectorsRequestV1, ExactAbstentionReasonV1, ExactAbstentionV1,
    ExactRetrievalMatchV1, ExactRetrievalOutcomeV1, ExactRetrievalRequestV1,
    ExactRetrievalResponseV1, GetRequestV1, GetResponseV1, HealthV1, HybridAbstentionV1,
    HybridBranchAbsenceV1, HybridExplanationV1, HybridRetrievalMatchV1, HybridRetrievalOutcomeV1,
    HybridRetrievalRequestV1, HybridRetrievalResponseV1, LexicalAbstentionReasonV1,
    LexicalAbstentionV1, LexicalFieldContributionV1, LexicalRetrievalMatchV1,
    LexicalRetrievalOutcomeV1, LexicalRetrievalRequestV1, LexicalRetrievalResponseV1,
    LexicalTermContributionV1, ProofV1, PutRequestV1, PutVectorsRequestV1, QueryRequestV1,
    QueryResponseV1, RecordV1, RetrievalProofV1, VectorMetricV1, WitnessV1, decode_key_hex,
    encode_hex,
};
use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName, current_version};
use hyphae_engine::{
    BoundedEngineQueryError, EngineError, ExactRetrievalProofArtifact,
    HybridRetrievalProofArtifact, HyphaeEngine, LexicalRetrievalProofArtifact, MaintenanceLimits,
    ProofError, ProvenResult, ResultProofArtifact, StorageLimits,
};
use hyphae_query::{DEFAULT_QUERY_SCAN_BYTES, FieldPath};
use hyphae_retrieval::{
    ExactAbstentionReason, ExactRetrievalOutcome, ExactRetrievalRequest, HybridBranchAbsence,
    HybridOutcome, HybridRequest, LexicalAbstentionReason, LexicalField, LexicalIndexDefinition,
    LexicalOutcome, LexicalRequest,
};
use hyphae_storage::{
    AppendOutcome, LogError, MaterializedIndexError, SnapshotError, SnapshotReadLimits,
    StorageError, open_verified_snapshot_with_limits,
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, ReadBuf},
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{ApiError, BearerToken, ServerConfig, ServerError, ServerLimits};

const FEATURES: [&str; 14] = [
    "atomic_batch",
    "deterministic_query",
    "durable_vectors",
    "exact_retrieval",
    "hybrid_retrieval",
    "idempotency",
    "kv",
    "lexical_retrieval",
    "offline_result_proof",
    "offline_retrieval_proof",
    "provider_free_lexical",
    "snapshot_witness",
    "structured_aggregation",
    "typed_abstention",
];
const WITNESS_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct RequestId(String);

struct ServerState {
    engine: Arc<Mutex<HyphaeEngine>>,
    data_dir: PathBuf,
    limits: ServerLimits,
    bearer_token: Option<BearerToken>,
    admission: Arc<Semaphore>,
    ready: AtomicBool,
}

struct AdmittedFile {
    file: tokio::fs::File,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for AdmittedFile {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_read(context, buffer)
    }
}

/// Opened optional HTTP surface owning exactly one embedded Hyphae engine.
pub struct HyphaeServer {
    bind: SocketAddr,
    state: Arc<ServerState>,
}

impl HyphaeServer {
    /// Validates secure defaults and opens the exclusively owned engine.
    ///
    /// No socket is opened by this method. In particular, a non-loopback bind
    /// without authentication fails here before [`Self::bind`].
    ///
    /// # Errors
    ///
    /// Returns a configuration, data-directory lock, recovery, or corruption
    /// error.
    pub fn open(config: ServerConfig) -> Result<Self, ServerError> {
        Self::open_with_storage_limits(config, StorageLimits::default())
    }

    /// Validates secure defaults and opens the engine under explicit finite
    /// storage recovery and maintenance limits.
    ///
    /// No socket is opened by this method.
    ///
    /// # Errors
    ///
    /// Returns a configuration, limit, timeout, data-directory lock,
    /// recovery, or corruption error.
    pub fn open_with_storage_limits(
        config: ServerConfig,
        storage_limits: StorageLimits,
    ) -> Result<Self, ServerError> {
        config.validate()?;
        let opened = HyphaeEngine::open_with_limits(config.data_dir(), storage_limits)?;
        let data_dir = opened.engine.data_path().to_path_buf();
        Ok(Self {
            bind: config.bind,
            state: Arc::new(ServerState {
                engine: Arc::new(Mutex::new(opened.engine)),
                data_dir,
                admission: Arc::new(Semaphore::new(config.limits.concurrent_operations)),
                ready: AtomicBool::new(true),
                limits: config.limits,
                bearer_token: config.bearer_token,
            }),
        })
    }

    /// Opens the configured TCP listener and prepares graceful serving.
    ///
    /// # Errors
    ///
    /// Returns an operating-system socket bind failure.
    pub async fn bind(self) -> Result<BoundServer, ServerError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|source| ServerError::Bind {
                address: self.bind,
                source,
            })?;
        let local_addr = listener.local_addr().map_err(|source| ServerError::Bind {
            address: self.bind,
            source,
        })?;
        Ok(BoundServer {
            listener,
            local_addr,
            router: build_router(self.state),
        })
    }

    #[cfg(test)]
    fn test_router(&self) -> Router {
        build_router(Arc::clone(&self.state))
    }
}

/// Successfully bound HTTP service awaiting a shutdown signal.
pub struct BoundServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    router: Router,
}

impl BoundServer {
    /// Returns the actual local address, including an assigned ephemeral port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until the supplied graceful-shutdown future resolves.
    ///
    /// # Errors
    ///
    /// Returns an HTTP listener/service I/O failure.
    pub async fn run_with_shutdown<F>(self, shutdown: F) -> Result<(), ServerError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(ServerError::Serve)
    }
}

fn build_router(state: Arc<ServerState>) -> Router {
    let public = Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/health/live", get(liveness))
        .route("/v1/health/ready", get(readiness));
    let protected = Router::new()
        .route("/v1/kv/put", post(put_records))
        .route("/v1/kv/get", post(get_record))
        .route("/v1/kv/delete", post(delete_records))
        .route("/v1/query", post(query_records))
        .route("/v1/vector-spaces/define", post(define_vector_space))
        .route("/v1/vectors/put", post(put_vectors))
        .route("/v1/vectors/delete", post(delete_vectors))
        .route("/v1/retrieve/exact", post(retrieve_exact))
        .route("/v1/lexical-indexes/define", post(define_lexical_index))
        .route("/v1/retrieve/lexical", post(retrieve_lexical))
        .route("/v1/retrieve/hybrid", post(retrieve_hybrid))
        .route(
            "/v1/witnesses/{checkpoint_sequence}/{snapshot_digest}",
            get(download_witness),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authenticate,
        ));

    public
        .merge(protected)
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
        .layer(middleware::from_fn(assign_request_id))
}

async fn assign_request_id(mut request: Request, next: Next) -> Response {
    let request_id = RequestId(Uuid::now_v7().to_string());
    request.extensions_mut().insert(request_id.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

async fn authenticate(
    State(state): State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = &state.bearer_token else {
        return next.run(request).await;
    };
    let request_id = request_id(&request);
    if bearer_candidate(request.headers()).is_some_and(|candidate| expected.verifies(candidate)) {
        return next.run(request).await;
    }
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "valid bearer authentication is required",
        request_id,
    )
    .into_response()
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

async fn liveness(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    bounded_json(
        &HealthV1 {
            status: "live".to_owned(),
        },
        &state,
        &request_id.0,
    )
}

async fn readiness(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(ApiError::unavailable(&request_id.0));
    }
    bounded_json(
        &HealthV1 {
            status: "ready".to_owned(),
        },
        &state,
        &request_id.0,
    )
}

async fn capabilities(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let version = current_version();
    bounded_json(
        &CapabilitiesV1 {
            api_version: version.api.to_owned(),
            disk_format_version: version.disk_format,
            features: FEATURES.iter().map(ToString::to_string).collect(),
            limits: state.limits.as_contract(),
        },
        &state,
        &request_id.0,
    )
}

async fn put_records(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: PutRequestV1 = parse_json(request, &state, &request_id.0).await?;
    validate_batch(request.records.len(), &state, &request_id.0)?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    let records = request
        .records
        .iter()
        .map(RecordV1::to_domain)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        capture_write_outcome(engine.put_records(transaction_id, &records))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn delete_records(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: DeleteRequestV1 = parse_json(request, &state, &request_id.0).await?;
    validate_batch(request.keys_hex.len(), &state, &request_id.0)?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    let keys = request
        .keys_hex
        .iter()
        .map(|key| decode_key_hex(key))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        let keys = keys.iter().map(Vec::as_slice).collect::<Vec<_>>();
        capture_write_outcome(engine.delete_records(transaction_id, &keys))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn get_record(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: GetRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let key = decode_key_hex(&request.key_hex).map_err(|_| ApiError::invalid(&request_id.0))?;
    let maintenance =
        witness_maintenance_limits(state.limits.witness_bytes, WITNESS_VERIFICATION_TIMEOUT);
    let artifact = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        engine.get_record_with_proof_with_limits(&key, &maintenance)
    })
    .await?;
    let proof = proof_transport(&artifact, &state, &request_id.0)?;
    let ProvenResult::Get(record) = artifact.proof.result() else {
        return Err(ApiError::internal(&request_id.0));
    };
    let response = GetResponseV1 {
        found: record.is_some(),
        record: record.as_ref().map(RecordV1::from_domain),
        proof,
    };
    bounded_json(&response, &state, &request_id.0)
}

async fn query_records(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: QueryRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let timeout = requested_timeout(request.timeout_ms, &state, &request_id.0)?;
    let query = request
        .to_domain()
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let mut execution_limits = state.limits.query.clone();
    execution_limits.timeout = timeout;
    let maintenance = witness_maintenance_limits(state.limits.witness_bytes, timeout);
    let artifact = with_bounded_query(Arc::clone(&state), &request_id.0, move |engine| {
        engine.query_with_proof_with_limits(
            &query,
            &execution_limits,
            DEFAULT_QUERY_SCAN_BYTES,
            &maintenance,
        )
    })
    .await?;
    let proof = proof_transport(&artifact, &state, &request_id.0)?;
    let ProvenResult::Query(result) = artifact.proof.result() else {
        return Err(ApiError::internal(&request_id.0));
    };
    bounded_json(
        &QueryResponseV1::from_domain(result, proof),
        &state,
        &request_id.0,
    )
}

async fn define_vector_space(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: DefineVectorSpaceRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    if request.vector_space.metric != VectorMetricV1::CosineQ15Nanos {
        return Err(ApiError::invalid(&request_id.0));
    }
    let name = VectorSpaceName::new(request.vector_space.name)
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let definition = VectorSpaceDefinition::cosine(name, request.vector_space.dimension)
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        capture_write_outcome(engine.define_vector_space(transaction_id, definition))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn put_vectors(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: PutVectorsRequestV1 = parse_json(request, &state, &request_id.0).await?;
    validate_batch(request.vectors.len(), &state, &request_id.0)?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    let space =
        VectorSpaceName::new(request.vector_space).map_err(|_| ApiError::invalid(&request_id.0))?;
    let vectors = request
        .vectors
        .into_iter()
        .map(|vector| {
            Ok((
                decode_key_hex(&vector.key_hex).map_err(|_| ApiError::invalid(&request_id.0))?,
                Q15Vector::new(vector.values).map_err(|_| ApiError::invalid(&request_id.0))?,
            ))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        capture_write_outcome(engine.put_vectors(transaction_id, &space, &vectors))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn delete_vectors(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: DeleteVectorsRequestV1 = parse_json(request, &state, &request_id.0).await?;
    validate_batch(request.keys_hex.len(), &state, &request_id.0)?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    let space =
        VectorSpaceName::new(request.vector_space).map_err(|_| ApiError::invalid(&request_id.0))?;
    let keys = request
        .keys_hex
        .iter()
        .map(|key| decode_key_hex(key))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        let keys = keys.iter().map(Vec::as_slice).collect::<Vec<_>>();
        capture_write_outcome(engine.delete_vectors(transaction_id, &space, &keys))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn retrieve_exact(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: ExactRetrievalRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let timeout = requested_retrieval_timeout(request.timeout_ms, &state, &request_id.0)?;
    let request = exact_request(request, &request_id.0)?;
    let mut limits = state.limits.exact_retrieval.clone();
    limits.timeout = timeout;
    let maintenance = witness_maintenance_limits(state.limits.witness_bytes, timeout);
    let artifact = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        engine.retrieve_exact_with_proof_with_limits(&request, &limits, &maintenance)
    })
    .await?;
    let proof = retrieval_proof_transport(&artifact, &state, &request_id.0)?;
    let response = ExactRetrievalResponseV1 {
        outcome: exact_outcome_transport(artifact.proof.outcome()),
        proof,
    };
    bounded_json(&response, &state, &request_id.0)
}

fn exact_request(
    request: ExactRetrievalRequestV1,
    request_id: &str,
) -> Result<ExactRetrievalRequest, ApiError> {
    Ok(ExactRetrievalRequest {
        vector_space: VectorSpaceName::new(request.vector_space)
            .map_err(|_| ApiError::invalid(request_id))?,
        query: Q15Vector::new(request.query).map_err(|_| ApiError::invalid(request_id))?,
        limit: usize::try_from(request.limit).map_err(|_| ApiError::limit(request_id))?,
        minimum_score_nanos: request.minimum_score_nanos,
        minimum_margin_nanos: request.minimum_margin_nanos,
    })
}

fn lexical_request(
    request: LexicalRetrievalRequestV1,
    request_id: &str,
) -> Result<LexicalRequest, ApiError> {
    Ok(LexicalRequest {
        index: VectorSpaceName::new(request.lexical_index)
            .map_err(|_| ApiError::invalid(request_id))?,
        query: request.query,
        limit: usize::try_from(request.limit).map_err(|_| ApiError::limit(request_id))?,
    })
}

async fn define_lexical_index(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: DefineLexicalIndexRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let transaction_id = parse_transaction_id(request.transaction_id.as_deref(), &request_id.0)?;
    let name = VectorSpaceName::new(request.lexical_index.name)
        .map_err(|_| ApiError::invalid(&request_id.0))?;
    let fields = request
        .lexical_index
        .fields
        .into_iter()
        .map(|field| {
            Ok(LexicalField {
                path: FieldPath::new(field.path),
                weight_micros: field.weight_micros,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let definition =
        LexicalIndexDefinition::new(name, fields).map_err(|_| ApiError::invalid(&request_id.0))?;
    let outcome = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        capture_write_outcome(engine.define_lexical_index(transaction_id, definition))
    })
    .await?;
    if outcome.requires_recovery {
        state.ready.store(false, Ordering::Release);
    }
    bounded_json(&receipt(outcome.append), &state, &request_id.0)
}

async fn retrieve_lexical(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: LexicalRetrievalRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let timeout = requested_lexical_timeout(request.timeout_ms, &state, &request_id.0)?;
    let request = lexical_request(request, &request_id.0)?;
    let mut limits = state.limits.lexical_retrieval.clone();
    limits.timeout = timeout;
    let maintenance = witness_maintenance_limits(state.limits.witness_bytes, timeout);
    let artifact = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        engine.retrieve_lexical_with_proof_with_limits(&request, &limits, &maintenance)
    })
    .await?;
    let proof = lexical_retrieval_proof_transport(&artifact, &state, &request_id.0)?;
    bounded_json(
        &LexicalRetrievalResponseV1 {
            outcome: lexical_outcome_transport(artifact.proof.outcome()),
            proof,
        },
        &state,
        &request_id.0,
    )
}

async fn retrieve_hybrid(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let request: HybridRetrievalRequestV1 = parse_json(request, &state, &request_id.0).await?;
    let lexical_timeout =
        requested_lexical_timeout(request.lexical.timeout_ms, &state, &request_id.0)?;
    let vector_timeout =
        requested_retrieval_timeout(request.vector.timeout_ms, &state, &request_id.0)?;
    let lexical_request = lexical_request(request.lexical, &request_id.0)?;
    let vector_request = exact_request(request.vector, &request_id.0)?;
    let hybrid_request = HybridRequest {
        lexical_weight: request.lexical_weight,
        vector_weight: request.vector_weight,
        limit: usize::try_from(request.limit).map_err(|_| ApiError::limit(&request_id.0))?,
    };
    if hybrid_request.limit > state.limits.lexical_retrieval.max_returned {
        return Err(ApiError::limit(&request_id.0));
    }
    let mut lexical_limits = state.limits.lexical_retrieval.clone();
    lexical_limits.timeout = lexical_timeout;
    let mut vector_limits = state.limits.exact_retrieval.clone();
    vector_limits.timeout = vector_timeout;
    let maintenance = witness_maintenance_limits(
        state.limits.witness_bytes,
        lexical_timeout
            .checked_add(vector_timeout)
            .unwrap_or(Duration::MAX),
    );
    let artifact = with_engine(Arc::clone(&state), &request_id.0, move |engine| {
        engine.retrieve_hybrid_with_proof_with_limits(
            &lexical_request,
            &lexical_limits,
            &vector_request,
            &vector_limits,
            &hybrid_request,
            &maintenance,
        )
    })
    .await?;
    let proof = hybrid_retrieval_proof_transport(&artifact, &state, &request_id.0)?;
    bounded_json(
        &HybridRetrievalResponseV1 {
            outcome: hybrid_outcome_transport(artifact.proof.outcome()),
            proof,
        },
        &state,
        &request_id.0,
    )
}

async fn download_witness(
    State(state): State<Arc<ServerState>>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<Response, ApiError> {
    let (sequence, expected_digest) =
        parse_witness_path(request.uri().path()).ok_or_else(|| ApiError::invalid(&request_id.0))?;
    let path = state
        .data_dir
        .join("snapshots")
        .join(format!("snapshot-{sequence:020}.hysnap"));
    let permit = Arc::clone(&state.admission)
        .try_acquire_owned()
        .map_err(|_| busy(&request_id.0))?;
    let verification_path = path.clone();
    let verification_limits = SnapshotReadLimits {
        file_bytes: state.limits.witness_bytes,
        entries: state.limits.witness_bytes,
        decoded_bytes: state.limits.witness_bytes,
    };
    let verified = tokio::task::spawn_blocking(move || {
        open_verified_snapshot_with_limits(
            verification_path,
            &verification_limits,
            WITNESS_VERIFICATION_TIMEOUT,
        )
    })
    .await;
    let (verified_file, info) = match verified {
        Ok(Ok(verified)) => verified,
        Ok(Err(SnapshotError::Io(source))) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found(&request_id.0));
        }
        Ok(Err(
            SnapshotError::FileLimitExceeded { .. }
            | SnapshotError::EntryLimitExceeded { .. }
            | SnapshotError::DecodedBytesLimitExceeded { .. },
        )) => {
            return Err(ApiError::result_too_large(&request_id.0));
        }
        Ok(Err(source)) if source.is_timeout() => {
            return Err(ApiError::timeout(&request_id.0));
        }
        Ok(Err(_)) | Err(_) => return Err(ApiError::internal(&request_id.0)),
    };
    if info.checkpoint_sequence != sequence || info.snapshot_digest != expected_digest {
        return Err(not_found(&request_id.0));
    }
    let stream = ReaderStream::new(
        AdmittedFile {
            file: tokio::fs::File::from_std(verified_file),
            _permit: permit,
        }
        .take(info.file_bytes),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, info.file_bytes)
        .header(
            "digest",
            format!("blake3={}", encode_hex(&info.snapshot_digest)),
        )
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal(&request_id.0))
}

async fn parse_json<T: DeserializeOwned>(
    request: Request,
    state: &ServerState,
    request_id: &str,
) -> Result<T, ApiError> {
    if !is_json_content_type(request.headers()) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "content type must be application/json",
            request_id,
        ));
    }
    let bytes = tokio::time::timeout(
        state.limits.request_body_timeout,
        body::to_bytes(request.into_body(), state.limits.request_body_bytes),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "timeout",
            "request body deadline elapsed without starting an operation",
            request_id,
        )
    })?
    .map_err(|_| ApiError::payload_too_large(request_id))?;
    if bytes.is_empty() {
        return Err(ApiError::invalid(request_id));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid(request_id))?;
    validate_json_shape(&value, state.limits.json_depth, state.limits.json_nodes)
        .map_err(|()| ApiError::limit(request_id))?;
    serde_json::from_value(value).map_err(|_| ApiError::invalid(request_id))
}

fn validate_json_shape(
    root: &serde_json::Value,
    maximum_depth: usize,
    maximum_nodes: usize,
) -> Result<(), ()> {
    let mut stack = vec![(root, 0_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.checked_add(1).ok_or(())?;
        if nodes > maximum_nodes || depth > maximum_depth {
            return Err(());
        }
        match value {
            serde_json::Value::Array(values) => {
                let next_depth = depth.checked_add(1).ok_or(())?;
                stack.extend(values.iter().map(|value| (value, next_depth)));
            }
            serde_json::Value::Object(values) => {
                let next_depth = depth.checked_add(1).ok_or(())?;
                stack.extend(values.values().map(|value| (value, next_depth)));
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
    Ok(())
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    let media_type = media_type.to_ascii_lowercase();
    media_type == "application/json"
        || (media_type.starts_with("application/") && media_type.ends_with("+json"))
}

fn validate_batch(length: usize, state: &ServerState, request_id: &str) -> Result<(), ApiError> {
    if length == 0 {
        return Err(ApiError::invalid(request_id));
    }
    if length > state.limits.batch_items {
        return Err(ApiError::limit(request_id));
    }
    Ok(())
}

fn parse_transaction_id(value: Option<&str>, request_id: &str) -> Result<Uuid, ApiError> {
    value.map_or_else(
        || Ok(Uuid::now_v7()),
        |value| Uuid::parse_str(value).map_err(|_| ApiError::invalid(request_id)),
    )
}

fn requested_timeout(
    requested_ms: Option<u64>,
    state: &ServerState,
    request_id: &str,
) -> Result<Duration, ApiError> {
    let maximum_ms = u64::try_from(state.limits.query.timeout.as_millis()).unwrap_or(u64::MAX);
    let requested_ms = requested_ms.unwrap_or(maximum_ms);
    if requested_ms == 0 {
        return Err(ApiError::invalid(request_id));
    }
    if requested_ms > maximum_ms {
        return Err(ApiError::limit(request_id));
    }
    Ok(Duration::from_millis(requested_ms))
}

fn requested_retrieval_timeout(
    requested_ms: Option<u64>,
    state: &ServerState,
    request_id: &str,
) -> Result<Duration, ApiError> {
    let maximum_ms =
        u64::try_from(state.limits.exact_retrieval.timeout.as_millis()).unwrap_or(u64::MAX);
    let requested_ms = requested_ms.unwrap_or(maximum_ms);
    if requested_ms == 0 {
        return Err(ApiError::invalid(request_id));
    }
    if requested_ms > maximum_ms {
        return Err(ApiError::limit(request_id));
    }
    Ok(Duration::from_millis(requested_ms))
}

fn requested_lexical_timeout(
    requested_ms: Option<u64>,
    state: &ServerState,
    request_id: &str,
) -> Result<Duration, ApiError> {
    let maximum_ms =
        u64::try_from(state.limits.lexical_retrieval.timeout.as_millis()).unwrap_or(u64::MAX);
    let requested_ms = requested_ms.unwrap_or(maximum_ms);
    if requested_ms == 0 {
        return Err(ApiError::invalid(request_id));
    }
    if requested_ms > maximum_ms {
        return Err(ApiError::limit(request_id));
    }
    Ok(Duration::from_millis(requested_ms))
}

fn witness_maintenance_limits(witness_bytes: u64, timeout: Duration) -> MaintenanceLimits {
    MaintenanceLimits {
        timeout: WITNESS_VERIFICATION_TIMEOUT.min(timeout),
        snapshot: SnapshotReadLimits {
            file_bytes: witness_bytes,
            entries: witness_bytes,
            decoded_bytes: witness_bytes,
        },
    }
}

async fn with_engine<T, F>(
    state: Arc<ServerState>,
    request_id: &str,
    operation: F,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut HyphaeEngine) -> Result<T, EngineError> + Send + 'static,
{
    if !state.ready.load(Ordering::Acquire) {
        return Err(ApiError::unavailable(request_id));
    }
    let _permit = Arc::clone(&state.admission)
        .try_acquire_owned()
        .map_err(|_| busy(request_id))?;
    let engine = Arc::clone(&state.engine);
    let result = tokio::task::spawn_blocking(move || {
        let mut engine = engine.lock().map_err(|_| EngineTaskError::Poisoned)?;
        operation(&mut engine).map_err(EngineTaskError::Engine)
    })
    .await;
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(EngineTaskError::Engine(source))) => {
            if engine_error_requires_recovery(&source) {
                state.ready.store(false, Ordering::Release);
            }
            Err(ApiError::from_engine(source, request_id))
        }
        Ok(Err(EngineTaskError::Poisoned)) | Err(_) => {
            state.ready.store(false, Ordering::Release);
            Err(ApiError::internal(request_id))
        }
    }
}

async fn with_bounded_query<T, F>(
    state: Arc<ServerState>,
    request_id: &str,
    operation: F,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut HyphaeEngine) -> Result<T, BoundedEngineQueryError> + Send + 'static,
{
    if !state.ready.load(Ordering::Acquire) {
        return Err(ApiError::unavailable(request_id));
    }
    let _permit = Arc::clone(&state.admission)
        .try_acquire_owned()
        .map_err(|_| busy(request_id))?;
    let engine = Arc::clone(&state.engine);
    let result = tokio::task::spawn_blocking(move || {
        let mut engine = engine.lock().map_err(|_| BoundedQueryTaskError::Poisoned)?;
        operation(&mut engine).map_err(BoundedQueryTaskError::Query)
    })
    .await;
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(BoundedQueryTaskError::Query(source))) => {
            if let BoundedEngineQueryError::Engine(engine) = &source
                && engine_error_requires_recovery(engine)
            {
                state.ready.store(false, Ordering::Release);
            }
            Err(ApiError::from_bounded_query(source, request_id))
        }
        Ok(Err(BoundedQueryTaskError::Poisoned)) | Err(_) => {
            state.ready.store(false, Ordering::Release);
            Err(ApiError::internal(request_id))
        }
    }
}

enum EngineTaskError {
    Engine(EngineError),
    Poisoned,
}

enum BoundedQueryTaskError {
    Query(BoundedEngineQueryError),
    Poisoned,
}

struct WriteOutcome {
    append: AppendOutcome,
    requires_recovery: bool,
}

fn capture_write_outcome(
    result: Result<AppendOutcome, EngineError>,
) -> Result<WriteOutcome, EngineError> {
    match result {
        Ok(append) => Ok(WriteOutcome {
            append,
            requires_recovery: false,
        }),
        Err(EngineError::Storage(StorageError::CommittedButNotIndexed { receipt, .. })) => {
            Ok(WriteOutcome {
                append: AppendOutcome::Committed(receipt),
                requires_recovery: true,
            })
        }
        Err(source) => Err(source),
    }
}

fn engine_error_requires_recovery(error: &EngineError) -> bool {
    if matches!(
        error,
        EngineError::Proof(ProofError::ProofLimitExceeded { .. } | ProofError::LengthOverflow)
    ) {
        return false;
    }
    match error {
        EngineError::Storage(StorageError::Index { source }) => {
            materialized_index_error_requires_recovery(source)
        }
        EngineError::Storage(StorageError::Snapshot { source }) => {
            snapshot_error_requires_recovery(source)
        }
        EngineError::Storage(
            StorageError::CommittedButNotIndexed { .. }
            | StorageError::StaleIndex
            | StorageError::DataDirectory(_)
            | StorageError::Log(LogError::Poisoned),
        )
        | EngineError::Proof(_) => true,
        _ => false,
    }
}

fn snapshot_error_requires_recovery(error: &SnapshotError) -> bool {
    if error.is_timeout() || error.storage_limit().is_some() {
        return false;
    }
    match error {
        SnapshotError::FileLimitExceeded { .. }
        | SnapshotError::EntryLimitExceeded { .. }
        | SnapshotError::DecodedBytesLimitExceeded { .. } => false,
        SnapshotError::Index { source } => materialized_index_error_requires_recovery(source),
        _ => true,
    }
}

fn materialized_index_error_requires_recovery(error: &MaterializedIndexError) -> bool {
    !matches!(
        error,
        MaterializedIndexError::Vector(_)
            | MaterializedIndexError::UnknownVectorSpace { .. }
            | MaterializedIndexError::VectorSpaceConflict { .. }
            | MaterializedIndexError::Lexical(_)
            | MaterializedIndexError::LexicalIndexConflict { .. }
            | MaterializedIndexError::UnknownLexicalIndex { .. }
            | MaterializedIndexError::VectorCandidateBudgetExceeded { .. }
            | MaterializedIndexError::VectorByteBudgetExceeded { .. }
    )
}

fn proof_transport(
    artifact: &ResultProofArtifact,
    state: &ServerState,
    request_id: &str,
) -> Result<ProofV1, ApiError> {
    let encoded = artifact
        .proof
        .to_bytes()
        .map_err(|source| ApiError::from_engine(EngineError::Proof(source), request_id))?;
    if encoded.len() > state.limits.proof_bytes
        || artifact.snapshot.file_bytes > state.limits.witness_bytes
    {
        return Err(ApiError::result_too_large(request_id));
    }
    let anchor = artifact.proof.anchor();
    let snapshot_digest = encode_hex(&anchor.snapshot_digest);
    Ok(ProofV1 {
        encoding: "base64".to_owned(),
        data: BASE64.encode(encoded),
        proof_digest: encode_hex(&artifact.proof.proof_digest()),
        anchor_digest: encode_hex(&artifact.proof.anchor_digest()),
        checkpoint_sequence: anchor.checkpoint_sequence,
        checkpoint_digest: anchor
            .checkpoint_digest
            .as_ref()
            .map(|digest| encode_hex(digest)),
        snapshot_digest: snapshot_digest.clone(),
        witness: WitnessV1 {
            path: format!(
                "/v1/witnesses/{}/{}",
                anchor.checkpoint_sequence, snapshot_digest
            ),
            file_bytes: artifact.snapshot.file_bytes,
        },
    })
}

fn retrieval_proof_transport(
    artifact: &ExactRetrievalProofArtifact,
    state: &ServerState,
    request_id: &str,
) -> Result<RetrievalProofV1, ApiError> {
    let encoded = artifact
        .proof
        .to_bytes()
        .map_err(|source| ApiError::from_engine(EngineError::RetrievalProof(source), request_id))?;
    if encoded.len() > state.limits.proof_bytes
        || artifact.snapshot.file_bytes > state.limits.witness_bytes
    {
        return Err(ApiError::result_too_large(request_id));
    }
    let anchor = artifact.proof.anchor();
    let snapshot_digest = encode_hex(&anchor.snapshot_digest);
    Ok(RetrievalProofV1 {
        encoding: "base64".to_owned(),
        data: BASE64.encode(encoded),
        proof_digest: encode_hex(&artifact.proof.proof_digest()),
        anchor_digest: encode_hex(&artifact.proof.anchor_digest()),
        checkpoint_sequence: anchor.checkpoint_sequence,
        checkpoint_digest: anchor
            .checkpoint_digest
            .as_ref()
            .map(|digest| encode_hex(digest)),
        snapshot_digest: snapshot_digest.clone(),
        witness: WitnessV1 {
            path: format!(
                "/v1/witnesses/{}/{}",
                anchor.checkpoint_sequence, snapshot_digest
            ),
            file_bytes: artifact.snapshot.file_bytes,
        },
    })
}

fn lexical_retrieval_proof_transport(
    artifact: &LexicalRetrievalProofArtifact,
    state: &ServerState,
    request_id: &str,
) -> Result<RetrievalProofV1, ApiError> {
    retrieval_proof_transport_parts(
        artifact.proof.to_bytes(),
        artifact.proof.proof_digest(),
        artifact.proof.anchor_digest(),
        artifact.proof.anchor(),
        artifact.snapshot.file_bytes,
        state,
        request_id,
    )
}

fn hybrid_retrieval_proof_transport(
    artifact: &HybridRetrievalProofArtifact,
    state: &ServerState,
    request_id: &str,
) -> Result<RetrievalProofV1, ApiError> {
    retrieval_proof_transport_parts(
        artifact.proof.to_bytes(),
        artifact.proof.proof_digest(),
        artifact.proof.anchor_digest(),
        artifact.proof.anchor(),
        artifact.snapshot.file_bytes,
        state,
        request_id,
    )
}

#[allow(clippy::too_many_arguments)]
fn retrieval_proof_transport_parts(
    encoded: Result<Vec<u8>, hyphae_engine::RetrievalProofError>,
    proof_digest: [u8; 32],
    anchor_digest: [u8; 32],
    anchor: &hyphae_engine::RetrievalProofAnchor,
    witness_bytes: u64,
    state: &ServerState,
    request_id: &str,
) -> Result<RetrievalProofV1, ApiError> {
    let encoded = encoded
        .map_err(|source| ApiError::from_engine(EngineError::RetrievalProof(source), request_id))?;
    if encoded.len() > state.limits.proof_bytes || witness_bytes > state.limits.witness_bytes {
        return Err(ApiError::result_too_large(request_id));
    }
    let snapshot_digest = encode_hex(&anchor.snapshot_digest);
    Ok(RetrievalProofV1 {
        encoding: "base64".to_owned(),
        data: BASE64.encode(encoded),
        proof_digest: encode_hex(&proof_digest),
        anchor_digest: encode_hex(&anchor_digest),
        checkpoint_sequence: anchor.checkpoint_sequence,
        checkpoint_digest: anchor
            .checkpoint_digest
            .as_ref()
            .map(|digest| encode_hex(digest)),
        snapshot_digest: snapshot_digest.clone(),
        witness: WitnessV1 {
            path: format!(
                "/v1/witnesses/{}/{}",
                anchor.checkpoint_sequence, snapshot_digest
            ),
            file_bytes: witness_bytes,
        },
    })
}

fn exact_outcome_transport(outcome: &ExactRetrievalOutcome) -> ExactRetrievalOutcomeV1 {
    match outcome {
        ExactRetrievalOutcome::Matches {
            matches,
            scanned_candidates,
        } => ExactRetrievalOutcomeV1::Matches {
            matches: matches
                .iter()
                .map(|matched| ExactRetrievalMatchV1 {
                    key_hex: encode_hex(&matched.key),
                    score_nanos: matched.score_nanos,
                })
                .collect(),
            scanned_candidates: *scanned_candidates,
        },
        ExactRetrievalOutcome::Abstained(abstention) => ExactRetrievalOutcomeV1::Abstained {
            abstention: ExactAbstentionV1 {
                reason: match abstention.reason {
                    ExactAbstentionReason::NoCandidates => ExactAbstentionReasonV1::NoCandidates,
                    ExactAbstentionReason::BelowThreshold => {
                        ExactAbstentionReasonV1::BelowThreshold
                    }
                    ExactAbstentionReason::Ambiguous => ExactAbstentionReasonV1::Ambiguous,
                },
                best_score_nanos: abstention.best_score_nanos,
                runner_up_score_nanos: abstention.runner_up_score_nanos,
                scanned_candidates: abstention.scanned_candidates,
            },
        },
    }
}

fn lexical_outcome_transport(outcome: &LexicalOutcome) -> LexicalRetrievalOutcomeV1 {
    match outcome {
        LexicalOutcome::Matches {
            matches,
            scanned_documents,
            matched_documents,
            query_tokens,
        } => LexicalRetrievalOutcomeV1::Matches {
            matches: matches
                .iter()
                .map(|matched| LexicalRetrievalMatchV1 {
                    key_hex: encode_hex(&matched.key),
                    score_nanos: matched.score_nanos,
                    terms: matched
                        .terms
                        .iter()
                        .map(|term| LexicalTermContributionV1 {
                            token: term.token.clone(),
                            document_frequency: term.document_frequency,
                            score_nanos: term.score_nanos,
                            fields: term
                                .fields
                                .iter()
                                .map(|field| LexicalFieldContributionV1 {
                                    path: field.path.segments().to_vec(),
                                    term_frequency: field.term_frequency,
                                    field_length: field.field_length,
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
            scanned_documents: *scanned_documents,
            matched_documents: *matched_documents,
            query_tokens: query_tokens.clone(),
        },
        LexicalOutcome::Abstained(abstention) => LexicalRetrievalOutcomeV1::Abstained {
            abstention: LexicalAbstentionV1 {
                reason: match abstention.reason {
                    LexicalAbstentionReason::NoCandidates => {
                        LexicalAbstentionReasonV1::NoCandidates
                    }
                },
                scanned_documents: abstention.scanned_documents,
                query_tokens: abstention.query_tokens.clone(),
            },
        },
    }
}

fn hybrid_outcome_transport(outcome: &HybridOutcome) -> HybridRetrievalOutcomeV1 {
    match outcome {
        HybridOutcome::Matches {
            matches,
            lexical_absence,
            vector_absence,
        } => HybridRetrievalOutcomeV1::Matches {
            matches: matches
                .iter()
                .map(|matched| HybridRetrievalMatchV1 {
                    key_hex: encode_hex(&matched.key),
                    explanation: HybridExplanationV1 {
                        lexical_rank: matched.explanation.lexical_rank,
                        lexical_score_nanos: matched.explanation.lexical_score_nanos,
                        vector_rank: matched.explanation.vector_rank,
                        vector_score_nanos: matched.explanation.vector_score_nanos,
                        lexical_contribution: matched.explanation.lexical_contribution,
                        vector_contribution: matched.explanation.vector_contribution,
                        fusion_score: matched.explanation.fusion_score,
                        final_rank: matched.explanation.final_rank,
                    },
                })
                .collect(),
            lexical_absence: lexical_absence.map(hybrid_absence_transport),
            vector_absence: vector_absence.map(hybrid_absence_transport),
        },
        HybridOutcome::Abstained(abstention) => HybridRetrievalOutcomeV1::Abstained {
            abstention: HybridAbstentionV1 {
                lexical: hybrid_absence_transport(abstention.lexical),
                vector: hybrid_absence_transport(abstention.vector),
            },
        },
    }
}

fn hybrid_absence_transport(absence: HybridBranchAbsence) -> HybridBranchAbsenceV1 {
    match absence {
        HybridBranchAbsence::LexicalNoCandidates => HybridBranchAbsenceV1::LexicalNoCandidates,
        HybridBranchAbsence::VectorNoCandidates => HybridBranchAbsenceV1::VectorNoCandidates,
        HybridBranchAbsence::VectorBelowThreshold => HybridBranchAbsenceV1::VectorBelowThreshold,
        HybridBranchAbsence::VectorAmbiguous => HybridBranchAbsenceV1::VectorAmbiguous,
    }
}

fn receipt(outcome: AppendOutcome) -> CommitReceiptV1 {
    let (status, receipt) = match outcome {
        AppendOutcome::Committed(receipt) => ("committed", receipt),
        AppendOutcome::Existing(receipt) => ("existing", receipt),
    };
    CommitReceiptV1 {
        status: status.to_owned(),
        transaction_id: receipt.transaction_id.to_string(),
        commit_sequence: receipt.commit_sequence,
        commit_digest: encode_hex(&receipt.commit_digest),
        transaction_digest: encode_hex(&receipt.transaction_digest),
    }
}

fn bounded_json<T: Serialize>(
    value: &T,
    state: &ServerState,
    request_id: &str,
) -> Result<Response, ApiError> {
    let encoded = serde_json::to_vec(value).map_err(|_| ApiError::internal(request_id))?;
    if encoded.len() > state.limits.response_bytes {
        return Err(ApiError::result_too_large(request_id));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, encoded.len())
        .body(Body::from(encoded))
        .map_err(|_| ApiError::internal(request_id))
}

fn parse_witness_path(path: &str) -> Option<(u64, [u8; 32])> {
    let suffix = path.strip_prefix("/v1/witnesses/")?;
    let mut components = suffix.split('/');
    let sequence = components.next()?.parse().ok()?;
    let digest = components.next()?;
    if components.next().is_some()
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let decoded = decode_key_hex(digest).ok()?;
    decoded.try_into().ok().map(|digest| (sequence, digest))
}

fn request_id(request: &Request) -> String {
    request
        .extensions()
        .get::<RequestId>()
        .map_or_else(|| Uuid::now_v7().to_string(), |value| value.0.clone())
}

fn busy(request_id: &str) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "busy",
        "concurrent operation admission limit reached",
        request_id,
    )
}

fn not_found(request_id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "requested version 1 resource does not exist",
        request_id,
    )
}

async fn route_not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    not_found(&request_id.0)
}

async fn method_not_allowed(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not defined for this version 1 route",
        request_id.0,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        io::Write as _,
        net::Ipv4Addr,
        path::PathBuf,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use axum::{
        body::Body,
        http::{Request, header},
    };
    use serde_json::Value;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpStream,
        sync::oneshot,
    };
    use tokio_util::io::ReaderStream;
    use tower::ServiceExt;

    use hyphae_engine::EngineError;
    use hyphae_storage::{AppendOutcome, CommitReceipt, MaterializedIndexError, StorageError};

    use super::{HyphaeServer, ServerConfig, StatusCode, body, capture_write_outcome};
    use crate::{BearerToken, ServerConfigError};

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn create(name: &str) -> Result<Self, Box<dyn Error>> {
            let path = std::env::temp_dir().join(format!(
                "hyphae-server-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn remote_bind_is_rejected_before_socket_bind() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("remote-rejected")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.bind = (Ipv4Addr::UNSPECIFIED, 8_787).into();
        assert!(matches!(
            HyphaeServer::open(config),
            Err(crate::ServerError::Configuration(
                ServerConfigError::RemoteBindRequiresAuthentication { .. }
            ))
        ));
        Ok(())
    }

    #[test]
    fn bearer_tokens_require_visible_header_safe_entropy() {
        assert!(BearerToken::new("short").is_err());
        assert!(BearerToken::new("0123456789abcdef0123456789abcde\n").is_err());
        assert!(BearerToken::new("0123456789abcdef0123456789abcdef").is_ok());
    }

    #[test]
    fn durable_unmaterialized_commit_keeps_its_public_receipt() -> Result<(), Box<dyn Error>> {
        let receipt = CommitReceipt {
            transaction_id: uuid::Uuid::now_v7(),
            commit_sequence: 9,
            commit_digest: [7; 32],
            transaction_digest: [8; 32],
        };
        let outcome = capture_write_outcome(Err(EngineError::Storage(
            StorageError::CommittedButNotIndexed {
                receipt,
                source: Box::new(MaterializedIndexError::MalformedCheckpoint),
            },
        )))?;
        assert!(outcome.requires_recovery);
        assert!(matches!(
            outcome.append,
            AppendOutcome::Committed(actual) if actual == receipt
        ));
        Ok(())
    }

    #[tokio::test]
    async fn oversized_witness_is_rejected_by_metadata_before_hashing() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create("oversized-witness")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.limits.witness_bytes = 8;
        let app = HyphaeServer::open(config)?.test_router();
        let witness_path = temporary
            .path
            .join("snapshots")
            .join("snapshot-00000000000000000001.hysnap");
        fs::write(&witness_path, [0_u8; 9])?;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/witnesses/1/{}", "00".repeat(32)))
                    .body(Body::empty())?,
            )
            .await?;
        assert_error(response, StatusCode::PAYLOAD_TOO_LARGE, "result_too_large").await?;
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_policy_failure_does_not_drop_server_readiness() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create("snapshot-policy-readiness")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.limits.witness_bytes = 1;
        let server = HyphaeServer::open(config)?;
        let app = server.test_router();

        let put = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"records":[{"key_hex":"61","value":1}]}"#,
                None,
            )?)
            .await?;
        assert_eq!(put.status(), StatusCode::OK);

        let rejected = app
            .clone()
            .oneshot(json_request("/v1/kv/get", r#"{"key_hex":"61"}"#, None)?)
            .await?;
        assert_error(rejected, StatusCode::PAYLOAD_TOO_LARGE, "result_too_large").await?;
        assert!(server.state.ready.load(Ordering::Acquire));

        let ready = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(ready.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn witness_stream_stops_at_verified_length_after_late_append()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("witness-stream-length")?;
        let app = HyphaeServer::open(ServerConfig::new(&temporary.path))?.test_router();
        let put = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"transaction_id":"018f0000-0000-7000-8000-000000000011","records":[{"key_hex":"61","value":1}]}"#,
                None,
            )?)
            .await?;
        assert_eq!(put.status(), StatusCode::OK);

        let get = app
            .clone()
            .oneshot(json_request("/v1/kv/get", r#"{"key_hex":"61"}"#, None)?)
            .await?;
        assert_eq!(get.status(), StatusCode::OK);
        let get: Value = serde_json::from_slice(&response_bytes(get).await?)?;
        let witness_path = get["proof"]["witness"]["path"]
            .as_str()
            .ok_or("missing witness path")?
            .to_owned();
        let witness = app
            .oneshot(Request::builder().uri(witness_path).body(Body::empty())?)
            .await?;
        assert_eq!(witness.status(), StatusCode::OK);
        let verified_length = witness
            .headers()
            .get(header::CONTENT_LENGTH)
            .ok_or("missing witness content length")?
            .to_str()?
            .parse::<usize>()?;

        let mut snapshots = fs::read_dir(temporary.path.join("snapshots"))?;
        let snapshot_path = snapshots.next().ok_or("missing local snapshot")??.path();
        assert!(snapshots.next().is_none());
        let suffix = b"late-unverified-growth";
        let mut snapshot = fs::OpenOptions::new().append(true).open(snapshot_path)?;
        snapshot.write_all(suffix)?;
        snapshot.sync_all()?;
        drop(snapshot);

        let streamed = response_bytes(witness).await?;
        assert_eq!(streamed.len(), verified_length);
        assert!(!streamed.ends_with(suffix));
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn authenticated_put_get_and_witness_are_contract_shaped() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("authenticated-flow")?;
        let secret = "correct-hyphae-token-material-0001";
        let mut config = ServerConfig::new(&temporary.path);
        config.bearer_token = Some(BearerToken::new(secret)?);
        config.limits.concurrent_operations = 1;
        let app = HyphaeServer::open(config)?.test_router();

        let unauthorized = app
            .clone()
            .oneshot(json_request("/v1/kv/put", r#"{"records":[]}"#, None)?)
            .await?;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert!(unauthorized.headers().contains_key("x-request-id"));

        let wrong = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"records":[]}"#,
                Some("incorrect-hyphae-token-material-001"),
            )?)
            .await?;
        assert_error(wrong, StatusCode::UNAUTHORIZED, "unauthorized").await?;

        let duplicate_header = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/query")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {secret}"))
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::from(r#"{"limit":1}"#))?,
            )
            .await?;
        assert_error(duplicate_header, StatusCode::UNAUTHORIZED, "unauthorized").await?;

        let put = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"transaction_id":"018f0000-0000-7000-8000-000000000001","records":[{"key_hex":"61","value":{"score":7}}]}"#,
                Some(secret),
            )?)
            .await?;
        assert_eq!(put.status(), StatusCode::OK);
        let put: Value = serde_json::from_slice(&response_bytes(put).await?)?;
        assert_eq!(put["status"], "committed");

        let retry = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"transaction_id":"018f0000-0000-7000-8000-000000000001","records":[{"key_hex":"61","value":{"score":7}}]}"#,
                Some(secret),
            )?)
            .await?;
        assert_eq!(retry.status(), StatusCode::OK);
        let retry: Value = serde_json::from_slice(&response_bytes(retry).await?)?;
        assert_eq!(retry["status"], "existing");

        let conflict = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"transaction_id":"018f0000-0000-7000-8000-000000000001","records":[{"key_hex":"61","value":{"score":8}}]}"#,
                Some(secret),
            )?)
            .await?;
        assert_error(conflict, StatusCode::CONFLICT, "idempotency_conflict").await?;

        let get = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/get",
                r#"{"key_hex":"61"}"#,
                Some(secret),
            )?)
            .await?;
        assert_eq!(get.status(), StatusCode::OK);
        let get: Value = serde_json::from_slice(&response_bytes(get).await?)?;
        assert_eq!(get["found"], true);
        assert_eq!(get["record"]["value"]["score"], 7);
        assert_eq!(get["proof"]["encoding"], "base64");
        let witness_path = get["proof"]["witness"]["path"]
            .as_str()
            .ok_or("missing witness path")?;

        let witness = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(witness_path)
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(witness.status(), StatusCode::OK);
        assert!(witness.headers().contains_key("digest"));
        let blocked_while_stream_is_live = app
            .clone()
            .oneshot(json_request("/v1/query", r#"{"limit":1}"#, Some(secret))?)
            .await?;
        assert_error(
            blocked_while_stream_is_live,
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
        )
        .await?;
        assert!(response_bytes(witness).await?.starts_with(b"HYSNAP01"));

        let query = app
            .oneshot(json_request("/v1/query", r#"{"limit":10}"#, Some(secret))?)
            .await?;
        assert_eq!(query.status(), StatusCode::OK);
        let query: Value = serde_json::from_slice(&response_bytes(query).await?)?;
        assert_eq!(query["rows"].as_array().map(Vec::len), Some(1));
        assert_eq!(query["proof"]["encoding"], "base64");
        Ok(())
    }

    #[tokio::test]
    async fn public_routes_and_limit_failures_never_emit_framework_text()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("limits")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.limits.request_body_bytes = 1_024;
        config.limits.batch_items = 1;
        let app = HyphaeServer::open(config)?.test_router();

        let capabilities = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/capabilities")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(capabilities.status(), StatusCode::OK);
        let capabilities: Value = serde_json::from_slice(&response_bytes(capabilities).await?)?;
        assert_eq!(capabilities["api_version"], "v1");
        assert_eq!(capabilities["limits"]["batch_items"], 1);

        let too_many = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/delete",
                r#"{"keys_hex":["61","62"]}"#,
                None,
            )?)
            .await?;
        assert_error(too_many, StatusCode::UNPROCESSABLE_ENTITY, "limit_exceeded").await?;

        let unsupported = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/query")
                    .header("content-type", "text/plain")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_error(
            unsupported,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        )
        .await?;

        let oversized = app
            .clone()
            .oneshot(json_request(
                "/v1/query",
                &format!(r#"{{"limit":1,"ignored":"{}"}}"#, "x".repeat(2_000)),
                None,
            )?)
            .await?;
        assert_error(
            oversized,
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        )
        .await?;

        let missing = app
            .clone()
            .oneshot(Request::builder().uri("/v1/unknown").body(Body::empty())?)
            .await?;
        assert_error(missing, StatusCode::NOT_FOUND, "not_found").await?;

        let wrong_method = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/health/live")
                    .body(Body::empty())?,
            )
            .await?;
        assert_error(
            wrong_method,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn vector_lexical_and_hybrid_routes_return_proof_bearing_results()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("retrieval-flow")?;
        let app = HyphaeServer::open(ServerConfig::new(&temporary.path))?.test_router();

        for (path, payload) in [
            (
                "/v1/kv/put",
                r#"{"records":[{"key_hex":"616c706861","value":{"title":"Durable memory","body":"offline agent memory"}},{"key_hex":"62657461","value":{"title":"Fast search","body":"exact vector retrieval"}}]}"#,
            ),
            (
                "/v1/lexical-indexes/define",
                r#"{"lexical_index":{"name":"content","fields":[{"path":["title"],"weight_micros":2000000},{"path":["body"],"weight_micros":1000000}]}}"#,
            ),
            (
                "/v1/vector-spaces/define",
                r#"{"vector_space":{"name":"semantic","dimension":2,"metric":"cosine_q15_nanos"}}"#,
            ),
            (
                "/v1/vectors/put",
                r#"{"vector_space":"semantic","vectors":[{"key_hex":"616c706861","values":[32767,0]},{"key_hex":"62657461","values":[0,32767]}]}"#,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(json_request(path, payload, None)?)
                .await?;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }

        let exact = app
            .clone()
            .oneshot(json_request(
                "/v1/retrieve/exact",
                r#"{"vector_space":"semantic","query":[32767,0],"limit":2,"minimum_score_nanos":-1000000000,"minimum_margin_nanos":0}"#,
                None,
            )?)
            .await?;
        assert_eq!(exact.status(), StatusCode::OK);
        let exact: Value = serde_json::from_slice(&response_bytes(exact).await?)?;
        assert_eq!(exact["outcome"]["matches"][0]["key_hex"], "616c706861");
        assert_eq!(exact["proof"]["encoding"], "base64");

        let lexical = app
            .clone()
            .oneshot(json_request(
                "/v1/retrieve/lexical",
                r#"{"lexical_index":"content","query":"durable memory","limit":2}"#,
                None,
            )?)
            .await?;
        assert_eq!(lexical.status(), StatusCode::OK);
        let lexical: Value = serde_json::from_slice(&response_bytes(lexical).await?)?;
        assert_eq!(lexical["outcome"]["matches"][0]["key_hex"], "616c706861");
        assert_eq!(lexical["proof"]["encoding"], "base64");

        let hybrid = app
            .clone()
            .oneshot(json_request(
                "/v1/retrieve/hybrid",
                r#"{"lexical":{"lexical_index":"content","query":"durable memory","limit":2},"vector":{"vector_space":"semantic","query":[32767,0],"limit":2,"minimum_score_nanos":-1000000000,"minimum_margin_nanos":0},"lexical_weight":1,"vector_weight":1,"limit":2}"#,
                None,
            )?)
            .await?;
        assert_eq!(hybrid.status(), StatusCode::OK);
        let hybrid: Value = serde_json::from_slice(&response_bytes(hybrid).await?)?;
        assert_eq!(hybrid["outcome"]["matches"][0]["key_hex"], "616c706861");
        assert_eq!(
            hybrid["outcome"]["matches"][0]["explanation"]["final_rank"],
            1
        );
        assert_eq!(hybrid["proof"]["encoding"], "base64");

        let wrong_dimension = app
            .clone()
            .oneshot(json_request(
                "/v1/retrieve/exact",
                r#"{"vector_space":"semantic","query":[32767],"limit":2,"minimum_score_nanos":-1000000000,"minimum_margin_nanos":0}"#,
                None,
            )?)
            .await?;
        assert_error(wrong_dimension, StatusCode::BAD_REQUEST, "invalid_request").await?;

        let empty_query = app
            .oneshot(json_request(
                "/v1/retrieve/lexical",
                r#"{"lexical_index":"content","query":"---","limit":2}"#,
                None,
            )?)
            .await?;
        assert_error(empty_query, StatusCode::BAD_REQUEST, "invalid_request").await?;
        Ok(())
    }

    #[tokio::test]
    async fn shape_proof_and_admission_limits_fail_without_partial_results()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("bounded-work")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.limits.json_depth = 3;
        config.limits.concurrent_operations = 1;
        config.limits.proof_bytes = 128;
        let server = HyphaeServer::open(config)?;
        let app = server.test_router();

        let too_deep = app
            .clone()
            .oneshot(json_request(
                "/v1/query",
                r#"{"filter":{"op":"not","filter":{"op":"not","filter":{"op":"not","filter":{"op":"match_all"}}}},"limit":1}"#,
                None,
            )?)
            .await?;
        assert_error(too_deep, StatusCode::UNPROCESSABLE_ENTITY, "limit_exceeded").await?;

        let put = app
            .clone()
            .oneshot(json_request(
                "/v1/kv/put",
                r#"{"records":[{"key_hex":"61","value":1}]}"#,
                None,
            )?)
            .await?;
        assert_eq!(put.status(), StatusCode::OK);

        let proof_too_large = app
            .clone()
            .oneshot(json_request("/v1/kv/get", r#"{"key_hex":"61"}"#, None)?)
            .await?;
        assert_error(
            proof_too_large,
            StatusCode::PAYLOAD_TOO_LARGE,
            "result_too_large",
        )
        .await?;

        let permit = Arc::clone(&server.state.admission).try_acquire_owned()?;
        let busy = app
            .clone()
            .oneshot(json_request("/v1/query", r#"{"limit":1}"#, None)?)
            .await?;
        drop(permit);
        assert_error(busy, StatusCode::TOO_MANY_REQUESTS, "busy").await?;

        server
            .state
            .ready
            .store(false, std::sync::atomic::Ordering::Release);
        let unavailable = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())?,
            )
            .await?;
        assert_error(unavailable, StatusCode::SERVICE_UNAVAILABLE, "unavailable").await?;
        Ok(())
    }

    #[tokio::test]
    async fn stalled_json_body_times_out_before_any_operation_starts() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create("body-timeout")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.limits.request_body_timeout = Duration::from_millis(5);
        let app = HyphaeServer::open(config)?.test_router();
        let (_writer, reader) = tokio::io::duplex(1);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/query")
                    .header("content-type", "application/json")
                    .body(Body::from_stream(ReaderStream::new(reader)))?,
            )
            .await?;
        assert_error(response, StatusCode::REQUEST_TIMEOUT, "timeout").await?;
        Ok(())
    }

    #[tokio::test]
    async fn bound_server_stops_on_graceful_shutdown() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create("graceful")?;
        let mut config = ServerConfig::new(&temporary.path);
        config.bind.set_port(0);
        let bound = HyphaeServer::open(config)?.bind().await?;
        let local_addr = bound.local_addr();
        assert_ne!(local_addr.port(), 0);
        let (send, receive) = oneshot::channel::<()>();
        let serving = tokio::spawn(bound.run_with_shutdown(async move {
            let _ignored = receive.await;
        }));
        let mut connection = TcpStream::connect(local_addr).await?;
        connection
            .write_all(
                b"GET /v1/health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await?;
        let mut response = Vec::new();
        connection.read_to_end(&mut response).await?;
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(br#"{"status":"live"}"#));
        let _ignored = send.send(());
        serving.await??;
        Ok(())
    }

    fn json_request(
        uri: &str,
        body: &str,
        bearer: Option<&str>,
    ) -> Result<Request<Body>, axum::http::Error> {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        request.body(Body::from(body.to_owned()))
    }

    async fn response_bytes(
        response: axum::response::Response,
    ) -> Result<axum::body::Bytes, Box<dyn Error>> {
        Ok(body::to_bytes(response.into_body(), 64 * 1024 * 1024).await?)
    }

    async fn assert_error(
        response: axum::response::Response,
        status: StatusCode,
        code: &str,
    ) -> Result<(), Box<dyn Error>> {
        assert_eq!(response.status(), status);
        let header_request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .ok_or("missing request ID header")?
            .to_owned();
        let value: Value = serde_json::from_slice(&response_bytes(response).await?)?;
        assert_eq!(value["code"], code);
        assert_eq!(value["request_id"], header_request_id);
        Ok(())
    }
}
