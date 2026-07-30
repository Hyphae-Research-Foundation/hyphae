// SPDX-License-Identifier: Apache-2.0

use std::{io, net::SocketAddr};

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyphae_contracts::v1::ErrorV1;
use hyphae_engine::{BoundedEngineQueryError, EngineError, ProofError, RetrievalProofError};
use hyphae_query::QueryError;
use hyphae_retrieval::{ExactRetrievalError, HybridError, LexicalError};
use hyphae_storage::{
    LogError, MaterializedIndexError, MutationError, SnapshotError, StorageError,
    StorageLimitError, storage_limit_from_io,
};
use thiserror::Error;

use crate::ServerConfigError;

/// Failure before or while running the optional HTTP delivery surface.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Secure configuration validation failed before socket bind.
    #[error(transparent)]
    Configuration(#[from] ServerConfigError),
    /// The exclusively owned embedded engine could not open.
    #[error("failed to open Hyphae engine: {0}")]
    Engine(#[from] EngineError),
    /// The requested socket could not be bound.
    #[error("failed to bind Hyphae server at {address}: {source}")]
    Bind {
        /// Requested listener address.
        address: SocketAddr,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// The bound HTTP service failed.
    #[error("Hyphae HTTP service failed: {0}")]
    Serve(#[source] io::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: String,
}

impl ApiError {
    pub(crate) fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            message,
            request_id: request_id.into(),
        }
    }

    pub(crate) fn invalid(request_id: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request does not satisfy the version 1 contract",
            request_id,
        )
    }

    pub(crate) fn limit(request_id: &str) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit_exceeded",
            "request exceeds an enforced server limit",
            request_id,
        )
    }

    pub(crate) fn payload_too_large(request_id: &str) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "request or response byte budget exceeded",
            request_id,
        )
    }

    pub(crate) fn timeout(request_id: &str) -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "timeout",
            "operation deadline elapsed without a partial result",
            request_id,
        )
    }

    pub(crate) fn result_too_large(request_id: &str) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "result_too_large",
            "proof-bearing result exceeds an enforced byte limit",
            request_id,
        )
    }

    pub(crate) fn internal(request_id: &str) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "internal operation failed; inspect local server diagnostics",
            request_id,
        )
    }

    pub(crate) fn unavailable(request_id: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "owned engine requires local recovery before serving data operations",
            request_id,
        )
    }

    pub(crate) fn from_engine(error: EngineError, request_id: &str) -> Self {
        match error {
            EngineError::DuplicateDocumentKey | EngineError::EmptyBatch => {
                Self::invalid(request_id)
            }
            EngineError::Document(_) => Self::limit(request_id),
            EngineError::Query(source) => Self::from_query(&source, request_id),
            EngineError::Storage(source) => Self::from_storage(&source, request_id),
            EngineError::ExactRetrieval(source) => Self::from_exact_retrieval(&source, request_id),
            EngineError::Lexical(source) => Self::from_lexical(&source, request_id),
            EngineError::Hybrid(source) => Self::from_hybrid(&source, request_id),
            EngineError::Proof(ProofError::ProofLimitExceeded { .. })
            | EngineError::RetrievalProof(RetrievalProofError::ProofLimitExceeded { .. }) => {
                Self::result_too_large(request_id)
            }
            EngineError::Backup(_)
            | EngineError::Proof(_)
            | EngineError::RetrievalProof(_)
            | EngineError::Retrieval(_) => Self::internal(request_id),
        }
    }

    pub(crate) fn from_bounded_query(error: BoundedEngineQueryError, request_id: &str) -> Self {
        match error {
            BoundedEngineQueryError::Engine(source) => Self::from_engine(source, request_id),
            BoundedEngineQueryError::ScannedByteBudgetExceeded { .. } => Self::limit(request_id),
        }
    }

    fn from_lexical(error: &LexicalError, request_id: &str) -> Self {
        match error {
            LexicalError::TimedOut => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                "timeout",
                "lexical retrieval deadline elapsed without a partial result",
                request_id,
            ),
            LexicalError::ResultLimitExceeded { .. }
            | LexicalError::DocumentBudgetExceeded { .. }
            | LexicalError::TokenBudgetExceeded { .. }
            | LexicalError::CandidateBudgetExceeded { .. } => Self::limit(request_id),
            LexicalError::EmptyFields
            | LexicalError::TooManyFields
            | LexicalError::EmptyFieldPath
            | LexicalError::InvalidFieldSegment
            | LexicalError::DuplicateFieldPath
            | LexicalError::InvalidFieldWeight
            | LexicalError::IndexMismatch
            | LexicalError::EmptyQuery
            | LexicalError::ZeroLimit
            | LexicalError::EmptyDocumentKey
            | LexicalError::DuplicateDocumentKey => Self::invalid(request_id),
            LexicalError::ArithmeticOverflow | LexicalError::MalformedProjection => {
                Self::internal(request_id)
            }
        }
    }

    fn from_hybrid(error: &HybridError, request_id: &str) -> Self {
        match error {
            HybridError::InvalidWeight
            | HybridError::ZeroLimit
            | HybridError::DuplicateBranchKey => Self::invalid(request_id),
            HybridError::ArithmeticOverflow => Self::internal(request_id),
        }
    }

    fn from_exact_retrieval(error: &ExactRetrievalError, request_id: &str) -> Self {
        match error {
            ExactRetrievalError::TimedOut => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                "timeout",
                "retrieval deadline elapsed without a partial result",
                request_id,
            ),
            ExactRetrievalError::ResultLimitExceeded { .. }
            | ExactRetrievalError::CandidateBudgetExceeded { .. }
            | ExactRetrievalError::CandidateByteBudgetExceeded { .. } => Self::limit(request_id),
            ExactRetrievalError::EmptyCandidateKey
            | ExactRetrievalError::DuplicateCandidateKey
            | ExactRetrievalError::DimensionMismatch { .. }
            | ExactRetrievalError::ZeroLimit
            | ExactRetrievalError::InvalidMinimumScore
            | ExactRetrievalError::InvalidMinimumMargin => Self::invalid(request_id),
            ExactRetrievalError::ArithmeticOverflow => Self::internal(request_id),
        }
    }

    fn from_query(error: &QueryError, request_id: &str) -> Self {
        match error {
            QueryError::TimedOut => Self::timeout(request_id),
            QueryError::ResultLimitExceeded { .. }
            | QueryError::FilterNodesExceeded { .. }
            | QueryError::FilterDepthExceeded { .. }
            | QueryError::SortFieldsExceeded { .. }
            | QueryError::GroupFieldsExceeded { .. }
            | QueryError::MetricsExceeded { .. }
            | QueryError::ScannedBudgetExceeded { .. }
            | QueryError::MatchedBudgetExceeded { .. }
            | QueryError::GroupBudgetExceeded { .. } => Self::limit(request_id),
            QueryError::EmptyRecordKey
            | QueryError::DuplicateRecordKey
            | QueryError::ZeroLimit
            | QueryError::CursorShape { .. }
            | QueryError::EmptyCursorKey
            | QueryError::NoncanonicalCursorNull
            | QueryError::InvalidPrefixType
            | QueryError::InvalidFieldPath
            | QueryError::EmptyMetricName
            | QueryError::DuplicateMetricName { .. }
            | QueryError::MetricTypeMismatch { .. }
            | QueryError::ArithmeticOverflow { .. }
            | QueryError::MetricStateMismatch => Self::invalid(request_id),
        }
    }

    fn from_storage(error: &StorageError, request_id: &str) -> Self {
        match error {
            StorageError::Index { source }
                if matches!(
                    source.as_ref(),
                    MaterializedIndexError::VectorSpaceConflict { .. }
                        | MaterializedIndexError::LexicalIndexConflict { .. }
                ) =>
            {
                Self::new(
                    StatusCode::CONFLICT,
                    "definition_conflict",
                    "immutable retrieval definition already exists with different contents",
                    request_id,
                )
            }
            StorageError::Index { source }
                if matches!(
                    source.as_ref(),
                    MaterializedIndexError::UnknownVectorSpace { .. }
                        | MaterializedIndexError::UnknownLexicalIndex { .. }
                        | MaterializedIndexError::Vector(_)
                ) =>
            {
                Self::invalid(request_id)
            }
            StorageError::Index { source } => {
                if let MaterializedIndexError::Lexical(source) = source.as_ref() {
                    return Self::from_lexical(source, request_id);
                }
                Self::internal(request_id)
            }
            StorageError::Mutation(
                MutationError::EmptyKey
                | MutationError::KeyTooLarge { .. }
                | MutationError::OperationTooLarge { .. },
            )
            | StorageError::Log(
                LogError::EmptyTransaction
                | LogError::TooManyOperations
                | LogError::PayloadTooLarge { .. },
            ) => Self::limit(request_id),
            StorageError::Log(LogError::Io(source))
                if matches!(
                    storage_limit_from_io(source),
                    Some(StorageLimitError::TimedOut)
                ) =>
            {
                Self::timeout(request_id)
            }
            StorageError::Log(LogError::Io(source)) if storage_limit_from_io(source).is_some() => {
                Self::limit(request_id)
            }
            StorageError::Snapshot { source } if source.is_timeout() => Self::timeout(request_id),
            StorageError::Snapshot { source } if source.storage_limit().is_some() => {
                Self::limit(request_id)
            }
            StorageError::Snapshot { source }
                if matches!(
                    source.as_ref(),
                    SnapshotError::FileLimitExceeded { .. }
                        | SnapshotError::EntryLimitExceeded { .. }
                        | SnapshotError::DecodedBytesLimitExceeded { .. }
                ) =>
            {
                Self::result_too_large(request_id)
            }
            StorageError::Log(LogError::IdempotencyConflict { .. }) => Self::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "transaction identifier was already committed with different contents",
                request_id,
            ),
            _ => Self::internal(request_id),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let envelope = ErrorV1 {
            code: self.code.to_owned(),
            message: self.message.to_owned(),
            request_id: self.request_id,
        };
        let mut response = (self.status, Json(envelope)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"hyphae\""),
            );
        }
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}
