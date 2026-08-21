// SPDX-License-Identifier: Apache-2.0

//! Native HTTP v2 JSON error and provisional-stream wire models.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable Native HTTP v2 media type for canonical product requests and responses.
pub const PRODUCT_MEDIA_TYPE_V2: &str = "application/vnd.hyphae.product-v1";
/// Optional exact binary `HYPERR01` response media type.
pub const PRODUCT_ERROR_MEDIA_TYPE_V2: &str = "application/vnd.hyphae.error-v1";
/// Stable HTTP request-correlation header.
pub const REQUEST_ID_HEADER_V2: &str = "x-hyphae-request-id";
/// Absolute Unix-time deadline header, in microseconds.
pub const DEADLINE_HEADER_V2: &str = "x-hyphae-deadline-micros";
/// Opaque lowercase hexadecimal identity for retained HTTP session state.
pub const SESSION_ID_HEADER_V2: &str = "x-hyphae-session-id";
/// Native product protocol minors offered by clients and the selection
/// echoed by servers.
pub const PROTOCOL_MINOR_HEADER_V2: &str = "x-hyphae-protocol-minor";
/// Highest Native HTTP v2 protocol minor this build serves.
pub const PROTOCOL_MINOR_VALUE_V2: &str = "3";
/// Every Native HTTP v2 protocol minor this build serves, ascending. The
/// server selects the highest member the client also offers and echoes the
/// selection; an offer with no supported member fails closed.
pub const PROTOCOL_MINORS_SUPPORTED_V2: &[u16] = &[3];

/// Typed configured-limit evidence in a Native product error.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductErrorLimitV2 {
    /// Stable product limit identity.
    pub kind: String,
    /// Configured maximum.
    pub configured: u64,
    /// Observed value.
    pub observed: u64,
}

/// Half-open source byte range in caller-supplied text.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductErrorSourceSpanV2 {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset.
    pub end: u32,
}

/// One bounded future error detail retained as hexadecimal bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductErrorUnknownDetailV2 {
    /// Canonical detail tag.
    pub tag: u16,
    /// Lowercase hexadecimal detail bytes.
    pub value_hex: String,
}

/// Known and future code-specific Product error details.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductErrorDetailsV2 {
    /// Native SQL subcode when the failure came from SQL.
    pub sql_subcode: Option<String>,
    /// Bounded future details in ascending tag order.
    pub unknown: Vec<ProductErrorUnknownDetailV2>,
}

/// JSON representation with exact field parity to `ProductError`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductErrorV2 {
    /// Stable product error code.
    pub code: String,
    /// Stable broad category.
    pub category: String,
    /// Stable retry classification.
    pub retry: String,
    /// Bounded redaction-safe message.
    pub message: String,
    /// Product request identity, as decimal text when present.
    pub request_id: Option<String>,
    /// Local trace identity, as decimal text when present.
    pub trace_id: Option<String>,
    /// Stable affected object identity, as decimal text when present.
    pub object_id: Option<String>,
    /// Transaction state at the failure boundary.
    pub transaction_state: String,
    /// Resolution transaction identity, as decimal text when present.
    pub transaction_id: Option<String>,
    /// Typed limit evidence when a specific limit is known.
    pub limit: Option<ProductErrorLimitV2>,
    /// SQL or query source location when available.
    pub source_span: Option<ProductErrorSourceSpanV2>,
    /// Code-specific typed and future details.
    pub details: ProductErrorDetailsV2,
}

/// One provisional binary response chunk in an NDJSON read stream.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalDataV2 {
    /// Fixed record identity `data`.
    pub r#type: String,
    /// Always true; this record has no standalone success semantics.
    pub provisional: bool,
    /// Zero-based chunk sequence.
    pub sequence: usize,
    /// Standard padded base64 product-response bytes.
    pub data_base64: String,
}

/// Mandatory terminal record that grants logical success to prior chunks.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamCompletionV2 {
    /// Fixed record identity `completion`.
    pub r#type: String,
    /// Fixed successful status `complete`.
    pub status: String,
    /// Product request identity as canonical decimal text.
    pub request_id: String,
    /// Number of preceding provisional records.
    pub chunks: usize,
    /// Reassembled binary Product response bytes.
    pub response_bytes: usize,
    /// Lowercase BLAKE3 digest of the reassembled response.
    pub digest_hex: String,
}

/// Native v1 compatibility policy advertised by the v2 contract.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeV1CompatibilityPolicyV2 {
    /// Only operations with complete semantic and proof parity may be mapped.
    pub exact_mappings_only: bool,
    /// Unmappable requests fail instead of falling through to another engine.
    pub unmappable_fails_explicitly: bool,
    /// The Native HTTP process never opens format-2 authority.
    pub opens_format_2_state: bool,
}
