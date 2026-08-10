// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic typed query model and executable reference semantics.

mod document;
mod engine;
mod model;
mod value;

pub use document::{
    DocumentError, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_NODES, decode_document,
    encode_document, encoded_document_len,
};
pub use engine::{
    BoundedQueryError, DEFAULT_QUERY_SCAN_BYTES, MonotonicClock, QueryError, SystemClock, execute,
    execute_with_byte_limit, execute_with_clock, execute_with_clock_and_byte_limit, validate_query,
};
pub use model::{
    AggregationPlan, AggregationResult, CompareOperator, Cursor, ExecutionLimits, Filter,
    GroupResult, Metric, MetricValue, NamedMetric, NamedMetricValue, NullPlacement, Query,
    QueryResult, Record, SortDirection, SortField,
};
pub use value::{FieldPath, Value};
