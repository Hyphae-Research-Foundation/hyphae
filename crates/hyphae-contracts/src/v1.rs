// SPDX-License-Identifier: Apache-2.0

//! Typed version 1 HTTP wire models and deterministic domain conversions.

use std::collections::BTreeMap;

use hyphae_query::{
    AggregationPlan, AggregationResult, CompareOperator, Cursor, FieldPath, Filter, GroupResult,
    Metric, MetricValue, NamedMetric, NamedMetricValue, NullPlacement, Query, QueryResult, Record,
    SortDirection, SortField, Value,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Reserved object key representing opaque bytes in the natural JSON value
/// surface.
pub const BYTES_HEX_KEY: &str = "$hyphae_bytes_hex";

/// Stable version 1 public error envelope.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorV1 {
    /// Stable machine-readable error code.
    pub code: String,
    /// Bounded human-readable diagnostic.
    pub message: String,
    /// Generated request identifier also returned as a response header.
    #[schemars(with = "uuid::Uuid")]
    pub request_id: String,
}

/// Public capability response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesV1 {
    /// Fixed API path version.
    pub api_version: String,
    /// Current numeric data-directory format.
    pub disk_format_version: u16,
    /// Sorted implemented feature identifiers.
    pub features: Vec<String>,
    /// Effective policy limits visible to clients.
    pub limits: ApiLimitsV1,
}

/// Public server policy limits.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiLimitsV1 {
    /// Maximum decoded binary key bytes.
    pub key_bytes: u64,
    /// Maximum canonical structured document payload bytes.
    pub document_bytes: u64,
    /// Maximum complete JSON request bytes.
    pub request_body_bytes: u64,
    /// Maximum JSON nesting depth.
    pub json_depth: u64,
    /// Maximum JSON nodes.
    pub json_nodes: u64,
    /// Maximum time allowed to receive one complete JSON body.
    pub request_body_timeout_ms: u64,
    /// Maximum records or keys in one atomic batch.
    pub batch_items: u64,
    /// Maximum records inspected globally by one query.
    pub scanned_records: u64,
    /// Maximum matched records retained by one query.
    pub matched_records: u64,
    /// Maximum rows returned by one query page.
    pub result_rows: u64,
    /// Maximum distinct aggregation groups.
    pub aggregation_groups: u64,
    /// Maximum recursive filter nodes.
    pub filter_nodes: u64,
    /// Maximum recursive filter depth.
    pub filter_depth: u64,
    /// Maximum explicit sort fields.
    pub sort_fields: u64,
    /// Maximum aggregation group fields.
    pub group_fields: u64,
    /// Maximum aggregation metrics.
    pub metrics: u64,
    /// Maximum admitted concurrent data operations.
    pub concurrent_operations: u64,
    /// Maximum requested query timeout.
    pub query_timeout_ms: u64,
    /// Maximum canonical result-proof bytes before base64 transport.
    pub proof_bytes: u64,
    /// Maximum downloadable canonical snapshot witness bytes.
    pub witness_bytes: u64,
    /// Maximum serialized JSON response bytes.
    pub response_bytes: u64,
    /// Maximum dimensions in one durable vector space.
    pub vector_dimensions: u64,
    /// Maximum vectors inspected by one exact retrieval.
    pub retrieval_candidates: u64,
    /// Maximum aggregate candidate key and vector bytes loaded.
    pub retrieval_candidate_bytes: u64,
    /// Maximum matches returned by one retrieval.
    pub retrieval_results: u64,
    /// Maximum requested retrieval timeout.
    pub retrieval_timeout_ms: u64,
    /// Maximum canonical retrieval-proof bytes before base64 transport.
    pub retrieval_proof_bytes: u64,
    /// Maximum durable documents inspected by one lexical retrieval.
    pub lexical_documents: u64,
    /// Maximum normalized tokens processed by one lexical retrieval.
    pub lexical_tokens: u64,
    /// Maximum matching lexical candidates retained before ranking.
    pub lexical_candidates: u64,
    /// Maximum matches returned by lexical or hybrid retrieval.
    pub lexical_results: u64,
    /// Maximum requested lexical retrieval timeout.
    pub lexical_timeout_ms: u64,
}

/// Liveness or readiness response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthV1 {
    /// `live` or `ready`.
    pub status: String,
}

/// One structured record on the wire.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordV1 {
    /// Nonempty binary key encoded as hexadecimal.
    pub key_hex: String,
    /// Natural JSON structured value with the reserved bytes envelope.
    pub value: serde_json::Value,
}

/// Atomic durable put batch.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PutRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Nonempty atomic record batch.
    pub records: Vec<RecordV1>,
}

/// Atomic durable delete batch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Nonempty binary keys encoded as hexadecimal.
    pub keys_hex: Vec<String>,
}

/// Exact durable KV lookup.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetRequestV1 {
    /// Nonempty binary key encoded as hexadecimal.
    pub key_hex: String,
}

/// Canonical durable vector metric.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorMetricV1 {
    /// Signed-Q15 integer cosine scored in nanos.
    CosineQ15Nanos,
}

/// One immutable durable vector-space definition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VectorSpaceV1 {
    /// Stable lowercase vector-space name.
    pub name: String,
    /// Fixed number of signed-Q15 elements.
    pub dimension: u16,
    /// Canonical scoring metric.
    pub metric: VectorMetricV1,
}

/// Atomic durable vector-space definition request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefineVectorSpaceRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Immutable vector-space definition.
    pub vector_space: VectorSpaceV1,
}

/// One canonical signed-Q15 vector on the wire.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VectorV1 {
    /// Nonempty binary object key encoded as hexadecimal.
    pub key_hex: String,
    /// Signed-Q15 elements whose length must match the space dimension.
    pub values: Vec<i16>,
}

/// Atomic durable vector upsert batch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PutVectorsRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Existing vector-space name.
    pub vector_space: String,
    /// Nonempty atomic vector batch.
    pub vectors: Vec<VectorV1>,
}

/// Atomic durable vector deletion batch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteVectorsRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Existing vector-space name.
    pub vector_space: String,
    /// Nonempty binary object keys encoded as hexadecimal.
    pub keys_hex: Vec<String>,
}

/// Durable exact-retrieval request under reference semantics v2.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExactRetrievalRequestV1 {
    /// Existing vector-space name.
    pub vector_space: String,
    /// Canonical signed-Q15 query vector.
    pub query: Vec<i16>,
    /// Maximum returned matches.
    pub limit: u64,
    /// Inclusive minimum canonical cosine score.
    pub minimum_score_nanos: i64,
    /// Minimum canonical best/runner-up margin.
    pub minimum_margin_nanos: u64,
    /// Optional per-request timeout bounded by server policy.
    pub timeout_ms: Option<u64>,
}

/// One canonical exact-retrieval match.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExactRetrievalMatchV1 {
    /// Binary object key encoded as hexadecimal.
    pub key_hex: String,
    /// Canonical signed integer cosine score.
    pub score_nanos: i64,
}

/// Stable normal exact-retrieval abstention reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExactAbstentionReasonV1 {
    /// The selected space contains no vectors.
    NoCandidates,
    /// The best score is below the request threshold.
    BelowThreshold,
    /// The best/runner-up margin is insufficient.
    Ambiguous,
}

/// Stable exact-retrieval abstention evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExactAbstentionV1 {
    /// Stable reason.
    pub reason: ExactAbstentionReasonV1,
    /// Best score when a candidate exists.
    pub best_score_nanos: Option<i64>,
    /// Runner-up score when one exists.
    pub runner_up_score_nanos: Option<i64>,
    /// Global candidates inspected.
    pub scanned_candidates: u64,
}

/// Complete durable exact-retrieval outcome.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExactRetrievalOutcomeV1 {
    /// Accepted globally ranked matches.
    Matches {
        /// Final canonical matches.
        matches: Vec<ExactRetrievalMatchV1>,
        /// Global candidates inspected.
        scanned_candidates: u64,
    },
    /// Typed normal abstention.
    Abstained {
        /// Complete abstention evidence.
        abstention: ExactAbstentionV1,
    },
}

/// Portable retrieval proof plus its downloadable witness reference.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalProofV1 {
    /// Fixed encoding identifier `base64`.
    pub encoding: String,
    /// Canonical `.hyrproof` bytes in standard padded base64.
    pub data: String,
    /// Canonical proof digest as hexadecimal.
    pub proof_digest: String,
    /// Caller-pinnable retrieval anchor digest as hexadecimal.
    pub anchor_digest: String,
    /// Snapshot/log checkpoint sequence.
    pub checkpoint_sequence: u64,
    /// Commit digest as hexadecimal, absent for empty history.
    pub checkpoint_digest: Option<String>,
    /// Canonical logical snapshot digest as hexadecimal.
    pub snapshot_digest: String,
    /// Authenticated endpoint for the complete offline witness.
    pub witness: WitnessV1,
}

/// Proof-bearing durable exact-retrieval response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExactRetrievalResponseV1 {
    /// Complete matches or typed abstention.
    pub outcome: ExactRetrievalOutcomeV1,
    /// Canonical retrieval proof and witness identity.
    pub proof: RetrievalProofV1,
}

/// One configured field in an immutable lexical-index definition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalFieldV1 {
    /// Nonempty exact object path.
    pub path: Vec<String>,
    /// Positive field weight in millionths.
    pub weight_micros: u32,
}

/// One immutable provider-free lexical-index definition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalIndexV1 {
    /// Stable lowercase index name.
    pub name: String,
    /// Nonempty unique fields; the server canonicalizes path order.
    pub fields: Vec<LexicalFieldV1>,
}

/// Atomic durable lexical-index definition request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefineLexicalIndexRequestV1 {
    /// Optional UUID idempotency key; a `UUIDv7` is generated when absent.
    #[schemars(with = "Option<uuid::Uuid>")]
    pub transaction_id: Option<String>,
    /// Immutable lexical-index definition.
    pub lexical_index: LexicalIndexV1,
}

/// Durable provider-free lexical retrieval request under semantics v1.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalRetrievalRequestV1 {
    /// Existing lexical-index name.
    pub lexical_index: String,
    /// UTF-8 query analyzed by tokenizer v1.
    pub query: String,
    /// Maximum returned matches.
    pub limit: u64,
    /// Optional per-request timeout bounded by server policy.
    pub timeout_ms: Option<u64>,
}

/// One configured-field contribution to a lexical term score.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalFieldContributionV1 {
    /// Canonical exact field path.
    pub path: Vec<String>,
    /// Raw term frequency in this field.
    pub term_frequency: u64,
    /// Normalized field token length.
    pub field_length: u64,
}

/// One canonical query-term contribution to a lexical result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalTermContributionV1 {
    /// Canonical tokenizer-v1 token.
    pub token: String,
    /// Corpus document frequency.
    pub document_frequency: u64,
    /// Quantized BM25F contribution.
    pub score_nanos: i64,
    /// Configured fields in canonical path order.
    pub fields: Vec<LexicalFieldContributionV1>,
}

/// One canonical lexical retrieval match.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalRetrievalMatchV1 {
    /// Binary object key encoded as hexadecimal.
    pub key_hex: String,
    /// Canonical BM25F score in nanos.
    pub score_nanos: i64,
    /// Deterministic per-term explanation.
    pub terms: Vec<LexicalTermContributionV1>,
}

/// Stable normal lexical abstention reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalAbstentionReasonV1 {
    /// No document contains a normalized query token.
    NoCandidates,
}

/// Stable lexical abstention evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalAbstentionV1 {
    /// Stable reason.
    pub reason: LexicalAbstentionReasonV1,
    /// Durable documents inspected.
    pub scanned_documents: u64,
    /// Canonical unique normalized query tokens.
    pub query_tokens: Vec<String>,
}

/// Complete durable lexical retrieval outcome.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LexicalRetrievalOutcomeV1 {
    /// Accepted globally ranked matches.
    Matches {
        /// Final canonical matches.
        matches: Vec<LexicalRetrievalMatchV1>,
        /// Durable documents inspected.
        scanned_documents: u64,
        /// Documents containing at least one query token.
        matched_documents: u64,
        /// Canonical unique normalized query tokens.
        query_tokens: Vec<String>,
    },
    /// Typed normal abstention.
    Abstained {
        /// Complete abstention evidence.
        abstention: LexicalAbstentionV1,
    },
}

/// Proof-bearing durable lexical retrieval response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalRetrievalResponseV1 {
    /// Complete matches or typed abstention.
    pub outcome: LexicalRetrievalOutcomeV1,
    /// Canonical retrieval proof and witness identity.
    pub proof: RetrievalProofV1,
}

/// Complete hybrid retrieval request under deterministic RRF semantics v1.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HybridRetrievalRequestV1 {
    /// Complete lexical branch request and candidate limit.
    pub lexical: LexicalRetrievalRequestV1,
    /// Complete exact-vector branch request and candidate limit.
    pub vector: ExactRetrievalRequestV1,
    /// Positive lexical RRF weight in `1..=1_000_000`.
    pub lexical_weight: u32,
    /// Positive vector RRF weight in `1..=1_000_000`.
    pub vector_weight: u32,
    /// Maximum final matches after deduplication and fusion.
    pub limit: u64,
}

/// Preserved reason for one absent hybrid branch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HybridBranchAbsenceV1 {
    /// Lexical branch had no candidates.
    LexicalNoCandidates,
    /// Exact-vector branch had no candidates.
    VectorNoCandidates,
    /// Exact-vector branch was below its threshold.
    VectorBelowThreshold,
    /// Exact-vector branch was ambiguous under margin policy.
    VectorAmbiguous,
}

/// Full deterministic explanation for one hybrid result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HybridExplanationV1 {
    /// One-based lexical rank when present.
    pub lexical_rank: Option<u64>,
    /// Canonical lexical score when present.
    pub lexical_score_nanos: Option<i64>,
    /// One-based exact-vector rank when present.
    pub vector_rank: Option<u64>,
    /// Canonical exact-vector score when present.
    pub vector_score_nanos: Option<i64>,
    /// Integer lexical RRF contribution.
    pub lexical_contribution: u64,
    /// Integer vector RRF contribution.
    pub vector_contribution: u64,
    /// Checked contribution sum.
    pub fusion_score: u64,
    /// One-based final rank.
    pub final_rank: u64,
}

/// One canonical hybrid retrieval match.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HybridRetrievalMatchV1 {
    /// Binary object key encoded as hexadecimal.
    pub key_hex: String,
    /// Complete fusion explanation.
    pub explanation: HybridExplanationV1,
}

/// Both hybrid branches abstained normally.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HybridAbstentionV1 {
    /// Lexical branch absence.
    pub lexical: HybridBranchAbsenceV1,
    /// Exact-vector branch absence.
    pub vector: HybridBranchAbsenceV1,
}

/// Complete deterministic hybrid retrieval outcome.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HybridRetrievalOutcomeV1 {
    /// Fused or explicit single-modality matches.
    Matches {
        /// Final canonical matches.
        matches: Vec<HybridRetrievalMatchV1>,
        /// Preserved lexical absence.
        lexical_absence: Option<HybridBranchAbsenceV1>,
        /// Preserved exact-vector absence.
        vector_absence: Option<HybridBranchAbsenceV1>,
    },
    /// Both branches abstained.
    Abstained {
        /// Complete branch abstention evidence.
        abstention: HybridAbstentionV1,
    },
}

/// Proof-bearing deterministic hybrid retrieval response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HybridRetrievalResponseV1 {
    /// Complete fused matches or typed abstention.
    pub outcome: HybridRetrievalOutcomeV1,
    /// Canonical retrieval proof and witness identity.
    pub proof: RetrievalProofV1,
}

/// Durable commit receipt.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitReceiptV1 {
    /// `committed` for a new commit or `existing` for an exact retry.
    pub status: String,
    /// UUID idempotency identifier.
    #[schemars(with = "uuid::Uuid")]
    pub transaction_id: String,
    /// Authoritative commit-frame sequence.
    pub commit_sequence: u64,
    /// Commit-frame digest as hexadecimal.
    pub commit_digest: String,
    /// Canonical transaction digest as hexadecimal.
    pub transaction_digest: String,
}

/// Portable result proof plus its downloadable witness reference.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProofV1 {
    /// Fixed encoding identifier `base64`.
    pub encoding: String,
    /// Canonical `.hyproof` bytes in standard padded base64.
    pub data: String,
    /// Canonical proof digest as hexadecimal.
    pub proof_digest: String,
    /// Caller-pinnable anchor digest as hexadecimal.
    pub anchor_digest: String,
    /// Snapshot/log checkpoint sequence.
    pub checkpoint_sequence: u64,
    /// Commit digest as hexadecimal, absent for empty history.
    pub checkpoint_digest: Option<String>,
    /// Canonical logical snapshot digest as hexadecimal.
    pub snapshot_digest: String,
    /// Authenticated endpoint for the complete offline witness.
    pub witness: WitnessV1,
}

/// Download reference for one canonical logical snapshot.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessV1 {
    /// Versioned relative HTTP path.
    pub path: String,
    /// Complete snapshot file length.
    pub file_bytes: u64,
}

/// Proof-bearing exact lookup response.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetResponseV1 {
    /// Whether the key exists at the proven checkpoint.
    pub found: bool,
    /// Present record, or `null` for proven absence.
    pub record: Option<RecordV1>,
    /// Complete result proof and witness identity.
    pub proof: ProofV1,
}

/// Ordered comparison operator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOperatorV1 {
    /// Exact equality.
    Equal,
    /// Exact inequality.
    NotEqual,
    /// Same-variant less than.
    Less,
    /// Same-variant less than or equal.
    LessOrEqual,
    /// Same-variant greater than.
    Greater,
    /// Same-variant greater than or equal.
    GreaterOrEqual,
}

/// Versioned recursive filter expression.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FilterV1 {
    /// Match every record.
    #[default]
    MatchAll,
    /// Test whether a path resolves.
    Exists {
        /// Exact object path; an empty list selects the root.
        path: Vec<String>,
    },
    /// Compare one resolved value.
    Compare {
        /// Exact object path.
        path: Vec<String>,
        /// Ordered operator.
        operator: CompareOperatorV1,
        /// Natural JSON literal.
        value: serde_json::Value,
    },
    /// Test a string or bytes prefix.
    Prefix {
        /// Exact object path.
        path: Vec<String>,
        /// Same-type prefix literal.
        prefix: serde_json::Value,
    },
    /// Test array membership, string substring, or byte subsequence.
    Contains {
        /// Exact object path.
        path: Vec<String>,
        /// Same-type needle literal.
        needle: serde_json::Value,
    },
    /// Require every child.
    All {
        /// Child filters.
        filters: Vec<Self>,
    },
    /// Require at least one child.
    Any {
        /// Child filters.
        filters: Vec<Self>,
    },
    /// Negate one child.
    Not {
        /// Child filter.
        filter: Box<Self>,
    },
}

/// Wire sort direction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirectionV1 {
    /// Natural ascending order.
    Ascending,
    /// Reverse natural order.
    Descending,
}

/// Wire null placement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NullPlacementV1 {
    /// Missing/null precede non-null.
    First,
    /// Missing/null follow non-null.
    Last,
}

/// One versioned sort field.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SortFieldV1 {
    /// Exact object path.
    pub path: Vec<String>,
    /// Value direction.
    pub direction: SortDirectionV1,
    /// Explicit missing/null placement.
    pub nulls: NullPlacementV1,
}

/// Logical continuation cursor.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CursorV1 {
    /// Normalized sort values; JSON null represents missing/null.
    pub sort_values: Vec<Option<serde_json::Value>>,
    /// Final binary key tie-breaker as hexadecimal.
    pub key_hex: String,
}

/// One named aggregation metric.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "metric", rename_all = "snake_case", deny_unknown_fields)]
pub enum NamedMetricV1 {
    /// Count every matched record.
    Count {
        /// Unique result name.
        name: String,
    },
    /// Checked integer sum.
    Sum {
        /// Unique result name.
        name: String,
        /// Exact object path.
        path: Vec<String>,
    },
    /// Minimum non-null value.
    Min {
        /// Unique result name.
        name: String,
        /// Exact object path.
        path: Vec<String>,
    },
    /// Maximum non-null value.
    Max {
        /// Unique result name.
        name: String,
        /// Exact object path.
        path: Vec<String>,
    },
}

/// Optional grouped aggregation plan.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AggregationPlanV1 {
    /// Ordered group-key paths.
    pub group_by: Vec<Vec<String>>,
    /// Named metrics.
    pub metrics: Vec<NamedMetricV1>,
}

/// Complete version 1 structured query request.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequestV1 {
    /// Recursive filter; defaults to match-all.
    #[serde(default)]
    pub filter: FilterV1,
    /// Deterministic sort fields.
    #[serde(default)]
    pub sort: Vec<SortFieldV1>,
    /// Optional logical continuation cursor.
    pub cursor: Option<CursorV1>,
    /// Final page size.
    pub limit: u32,
    /// Optional global or grouped aggregation.
    pub aggregation: Option<AggregationPlanV1>,
    /// Requested cooperative query timeout.
    pub timeout_ms: Option<u64>,
}

/// Versioned aggregate metric output.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetricValueV1 {
    /// Unsigned count.
    Count {
        /// Count value.
        value: u64,
    },
    /// Checked signed integer, represented as decimal to preserve `i128`.
    Integer {
        /// Decimal result, or null for no non-null inputs.
        value: Option<String>,
    },
    /// Minimum or maximum structured value.
    Value {
        /// Result value, or null for no non-null inputs.
        value: Option<serde_json::Value>,
    },
}

/// Named aggregate result.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedMetricResultV1 {
    /// Metric name from the request.
    pub name: String,
    /// Typed metric value.
    pub result: MetricValueV1,
}

/// One aggregation group-key component preserving missing versus explicit null.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GroupKeyValueV1 {
    /// The requested field path did not resolve.
    Missing,
    /// The path resolved, including when the structured value is explicit null.
    Value {
        /// Natural JSON structured value.
        value: serde_json::Value,
    },
}

/// One deterministic aggregate group.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupResultV1 {
    /// Ordered group values with an explicit missing/null distinction.
    pub key: Vec<GroupKeyValueV1>,
    /// Metric results in request order.
    pub metrics: Vec<NamedMetricResultV1>,
}

/// Complete aggregation response.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AggregationResultV1 {
    /// Whether explicit group fields were requested.
    pub grouped: bool,
    /// Deterministically ordered groups.
    pub groups: Vec<GroupResultV1>,
}

/// Proof-bearing structured query response.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResponseV1 {
    /// Final page rows.
    pub rows: Vec<RecordV1>,
    /// Logical continuation cursor when more rows exist.
    pub next_cursor: Option<CursorV1>,
    /// Optional complete aggregation.
    pub aggregation: Option<AggregationResultV1>,
    /// Records inspected globally.
    pub scanned_records: u64,
    /// Records matching before pagination.
    pub matched_records: u64,
    /// Complete result proof and witness identity.
    pub proof: ProofV1,
}

/// Failure converting versioned JSON wire values into domain values.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WireValueError {
    /// JSON numbers must fit signed 64-bit integers.
    #[error("structured values accept only signed 64-bit integer JSON numbers")]
    NonIntegerNumber,
    /// Reserved bytes envelope must contain valid even-length hexadecimal.
    #[error("invalid reserved bytes hexadecimal envelope")]
    InvalidBytesHex,
    /// Binary keys must contain valid nonempty even-length hexadecimal.
    #[error("invalid binary key hexadecimal")]
    InvalidKeyHex,
    /// Field path contains an empty segment.
    #[error("field path contains an empty segment")]
    EmptyPathSegment,
    /// Public fixed-width page size does not fit this Rust target.
    #[error("query page size is not representable on this target")]
    LengthOverflow,
}

impl QueryRequestV1 {
    /// Converts the versioned wire request into deterministic domain semantics.
    ///
    /// # Errors
    ///
    /// Returns an error for non-integer values, invalid bytes/key hex, or
    /// empty field-path segments.
    pub fn to_domain(&self) -> Result<Query, WireValueError> {
        Ok(Query {
            filter: filter_to_domain(&self.filter)?,
            sort: self
                .sort
                .iter()
                .map(sort_to_domain)
                .collect::<Result<_, _>>()?,
            cursor: self.cursor.as_ref().map(cursor_to_domain).transpose()?,
            limit: usize::try_from(self.limit).map_err(|_| WireValueError::LengthOverflow)?,
            aggregation: self
                .aggregation
                .as_ref()
                .map(aggregation_to_domain)
                .transpose()?,
        })
    }
}

impl RecordV1 {
    /// Converts one versioned record into the domain record.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed key hex or unsupported JSON values.
    pub fn to_domain(&self) -> Result<Record, WireValueError> {
        Ok(Record {
            key: decode_key_hex(&self.key_hex)?,
            value: value_to_domain(&self.value)?,
        })
    }

    /// Converts one domain record into its versioned JSON representation.
    pub fn from_domain(record: &Record) -> Self {
        Self {
            key_hex: encode_hex(&record.key),
            value: value_from_domain(&record.value),
        }
    }
}

impl QueryResponseV1 {
    /// Converts a deterministic query result and proof into the wire response.
    pub fn from_domain(result: &QueryResult, proof: ProofV1) -> Self {
        Self {
            rows: result.rows.iter().map(RecordV1::from_domain).collect(),
            next_cursor: result.next_cursor.as_ref().map(cursor_from_domain),
            aggregation: result.aggregation.as_ref().map(aggregation_from_domain),
            scanned_records: result.scanned_records,
            matched_records: result.matched_records,
            proof,
        }
    }
}

/// Converts natural JSON into the deterministic structured value domain.
///
/// # Errors
///
/// Returns an error for non-integer numbers or malformed reserved bytes.
pub fn value_to_domain(value: &serde_json::Value) -> Result<Value, WireValueError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Boolean(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(Value::Integer)
            .ok_or(WireValueError::NonIntegerNumber),
        serde_json::Value::String(value) => Ok(Value::String(value.clone())),
        serde_json::Value::Array(values) => values
            .iter()
            .map(value_to_domain)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_json::Value::Object(values) => {
            if values.len() == 1
                && let Some(serde_json::Value::String(encoded)) = values.get(BYTES_HEX_KEY)
            {
                return decode_hex(encoded)
                    .map(Value::Bytes)
                    .ok_or(WireValueError::InvalidBytesHex);
            }
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), value_to_domain(value)?)))
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Value::Object)
        }
    }
}

/// Converts one deterministic structured value into natural versioned JSON.
pub fn value_from_domain(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(value) => serde_json::Value::Bool(*value),
        Value::Integer(value) => serde_json::Value::Number((*value).into()),
        Value::String(value) => serde_json::Value::String(value.clone()),
        Value::Bytes(value) => serde_json::json!({ BYTES_HEX_KEY: encode_hex(value) }),
        Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(value_from_domain).collect())
        }
        Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_from_domain(value)))
                .collect(),
        ),
    }
}

/// Decodes one nonempty hexadecimal binary key.
///
/// # Errors
///
/// Returns an error for empty, odd-length, or non-hexadecimal input.
pub fn decode_key_hex(encoded: &str) -> Result<Vec<u8>, WireValueError> {
    let decoded = decode_hex(encoded).ok_or(WireValueError::InvalidKeyHex)?;
    if decoded.is_empty() {
        return Err(WireValueError::InvalidKeyHex);
    }
    Ok(decoded)
}

/// Encodes arbitrary bytes as lowercase hexadecimal.
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?))
        .collect()
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn field_path(segments: &[String]) -> Result<FieldPath, WireValueError> {
    if segments.iter().any(String::is_empty) {
        return Err(WireValueError::EmptyPathSegment);
    }
    Ok(FieldPath::new(segments.iter().cloned()))
}

fn filter_to_domain(filter: &FilterV1) -> Result<Filter, WireValueError> {
    match filter {
        FilterV1::MatchAll => Ok(Filter::MatchAll),
        FilterV1::Exists { path } => Ok(Filter::Exists(field_path(path)?)),
        FilterV1::Compare {
            path,
            operator,
            value,
        } => Ok(Filter::Compare {
            path: field_path(path)?,
            operator: match operator {
                CompareOperatorV1::Equal => CompareOperator::Equal,
                CompareOperatorV1::NotEqual => CompareOperator::NotEqual,
                CompareOperatorV1::Less => CompareOperator::Less,
                CompareOperatorV1::LessOrEqual => CompareOperator::LessOrEqual,
                CompareOperatorV1::Greater => CompareOperator::Greater,
                CompareOperatorV1::GreaterOrEqual => CompareOperator::GreaterOrEqual,
            },
            value: value_to_domain(value)?,
        }),
        FilterV1::Prefix { path, prefix } => Ok(Filter::Prefix {
            path: field_path(path)?,
            prefix: value_to_domain(prefix)?,
        }),
        FilterV1::Contains { path, needle } => Ok(Filter::Contains {
            path: field_path(path)?,
            needle: value_to_domain(needle)?,
        }),
        FilterV1::All { filters } => filters
            .iter()
            .map(filter_to_domain)
            .collect::<Result<_, _>>()
            .map(Filter::All),
        FilterV1::Any { filters } => filters
            .iter()
            .map(filter_to_domain)
            .collect::<Result<_, _>>()
            .map(Filter::Any),
        FilterV1::Not { filter } => Ok(Filter::Not(Box::new(filter_to_domain(filter)?))),
    }
}

fn sort_to_domain(field: &SortFieldV1) -> Result<SortField, WireValueError> {
    Ok(SortField {
        path: field_path(&field.path)?,
        direction: match field.direction {
            SortDirectionV1::Ascending => SortDirection::Ascending,
            SortDirectionV1::Descending => SortDirection::Descending,
        },
        nulls: match field.nulls {
            NullPlacementV1::First => NullPlacement::First,
            NullPlacementV1::Last => NullPlacement::Last,
        },
    })
}

fn cursor_to_domain(cursor: &CursorV1) -> Result<Cursor, WireValueError> {
    Ok(Cursor {
        sort_values: cursor
            .sort_values
            .iter()
            .map(|value| value.as_ref().map(value_to_domain).transpose())
            .collect::<Result<_, _>>()?,
        key: decode_key_hex(&cursor.key_hex)?,
    })
}

fn aggregation_to_domain(plan: &AggregationPlanV1) -> Result<AggregationPlan, WireValueError> {
    Ok(AggregationPlan {
        group_by: plan
            .group_by
            .iter()
            .map(|path| field_path(path))
            .collect::<Result<_, _>>()?,
        metrics: plan
            .metrics
            .iter()
            .map(|metric| match metric {
                NamedMetricV1::Count { name } => Ok(NamedMetric {
                    name: name.clone(),
                    metric: Metric::Count,
                }),
                NamedMetricV1::Sum { name, path } => Ok(NamedMetric {
                    name: name.clone(),
                    metric: Metric::Sum(field_path(path)?),
                }),
                NamedMetricV1::Min { name, path } => Ok(NamedMetric {
                    name: name.clone(),
                    metric: Metric::Min(field_path(path)?),
                }),
                NamedMetricV1::Max { name, path } => Ok(NamedMetric {
                    name: name.clone(),
                    metric: Metric::Max(field_path(path)?),
                }),
            })
            .collect::<Result<_, _>>()?,
    })
}

fn cursor_from_domain(cursor: &Cursor) -> CursorV1 {
    CursorV1 {
        sort_values: cursor
            .sort_values
            .iter()
            .map(|value| value.as_ref().map(value_from_domain))
            .collect(),
        key_hex: encode_hex(&cursor.key),
    }
}

fn aggregation_from_domain(result: &AggregationResult) -> AggregationResultV1 {
    AggregationResultV1 {
        grouped: result.grouped,
        groups: result.groups.iter().map(group_from_domain).collect(),
    }
}

fn group_from_domain(group: &GroupResult) -> GroupResultV1 {
    GroupResultV1 {
        key: group
            .key
            .iter()
            .map(|value| match value {
                None => GroupKeyValueV1::Missing,
                Some(value) => GroupKeyValueV1::Value {
                    value: value_from_domain(value),
                },
            })
            .collect(),
        metrics: group.metrics.iter().map(metric_from_domain).collect(),
    }
}

fn metric_from_domain(metric: &NamedMetricValue) -> NamedMetricResultV1 {
    NamedMetricResultV1 {
        name: metric.name.clone(),
        result: match &metric.value {
            MetricValue::Count(value) => MetricValueV1::Count { value: *value },
            MetricValue::Integer(value) => MetricValueV1::Integer {
                value: value.map(|value| value.to_string()),
            },
            MetricValue::Value(value) => MetricValueV1::Value {
                value: value.as_ref().map(value_from_domain),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hyphae_query::{Filter, Value};

    use super::{
        BYTES_HEX_KEY, FilterV1, GroupKeyValueV1, GroupResultV1, QueryRequestV1, WireValueError,
        value_from_domain, value_to_domain,
    };

    #[test]
    fn natural_json_value_round_trips_domain_values() -> Result<(), WireValueError> {
        let value = Value::Object(BTreeMap::from([
            ("bytes".to_owned(), Value::Bytes(vec![0, 255])),
            ("number".to_owned(), Value::Integer(-7)),
        ]));
        assert_eq!(value_to_domain(&value_from_domain(&value))?, value);
        assert_eq!(
            value_to_domain(&serde_json::json!({BYTES_HEX_KEY: "00ff"}))?,
            Value::Bytes(vec![0, 255])
        );
        Ok(())
    }

    #[test]
    fn wire_query_rejects_noninteger_numbers_and_empty_paths() {
        assert_eq!(
            value_to_domain(&serde_json::json!(1.5)),
            Err(WireValueError::NonIntegerNumber)
        );
        let request = QueryRequestV1 {
            filter: FilterV1::Exists {
                path: vec![String::new()],
            },
            sort: Vec::new(),
            cursor: None,
            limit: 1,
            aggregation: None,
            timeout_ms: None,
        };
        assert_eq!(request.to_domain(), Err(WireValueError::EmptyPathSegment));
        assert_ne!(Filter::MatchAll, Filter::Any(Vec::new()));
    }

    #[test]
    fn aggregation_group_keys_preserve_missing_versus_explicit_null()
    -> Result<(), serde_json::Error> {
        let missing = GroupResultV1 {
            key: vec![GroupKeyValueV1::Missing],
            metrics: Vec::new(),
        };
        let explicit_null = GroupResultV1 {
            key: vec![GroupKeyValueV1::Value {
                value: serde_json::Value::Null,
            }],
            metrics: Vec::new(),
        };
        assert_ne!(
            serde_json::to_value(missing)?,
            serde_json::to_value(explicit_null)?
        );
        Ok(())
    }
}
