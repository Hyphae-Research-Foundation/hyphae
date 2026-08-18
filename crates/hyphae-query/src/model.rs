// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::{FieldPath, Value};

/// One logical record supplied to the reference query executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    /// Globally unique nonempty binary key.
    pub key: Vec<u8>,
    /// Structured record value.
    pub value: Value,
}

impl Record {
    /// Creates a logical record. Key validity is checked by execution so a
    /// complete shard batch can be rejected consistently.
    pub fn new(key: impl Into<Vec<u8>>, value: Value) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }
}

/// Ordered comparison operator for one resolved field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOperator {
    /// Exact type-and-value equality.
    Equal,
    /// Exact inequality; missing still evaluates false.
    NotEqual,
    /// Less than, only between the same value variants.
    Less,
    /// Less than or equal, only between the same value variants.
    LessOrEqual,
    /// Greater than, only between the same value variants.
    Greater,
    /// Greater than or equal, only between the same value variants.
    GreaterOrEqual,
}

/// Deterministic filter expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Filter {
    /// Matches every record.
    MatchAll,
    /// Tests whether a path resolves, including to explicit null.
    Exists(FieldPath),
    /// Compares one resolved value against a literal.
    Compare {
        /// Field path.
        path: FieldPath,
        /// Comparison operator.
        operator: CompareOperator,
        /// Literal right-hand value.
        value: Value,
    },
    /// Tests a UTF-8 or binary prefix of the same type.
    Prefix {
        /// Field path.
        path: FieldPath,
        /// String or bytes prefix.
        prefix: Value,
    },
    /// Tests array membership, UTF-8 substring, or byte subsequence.
    Contains {
        /// Field path.
        path: FieldPath,
        /// Exact element or same-type needle.
        needle: Value,
    },
    /// Every child must match; an empty list is true.
    All(Vec<Self>),
    /// At least one child must match; an empty list is false.
    Any(Vec<Self>),
    /// Ordinary two-valued negation.
    Not(Box<Self>),
}

/// Sort direction for one field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    /// Natural ascending value order.
    Ascending,
    /// Reverse value order.
    Descending,
}

/// Explicit placement for missing and null sort values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NullPlacement {
    /// Missing and null precede non-null values.
    First,
    /// Missing and null follow non-null values.
    Last,
}

/// One requested sort component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortField {
    /// Value path.
    pub path: FieldPath,
    /// Value comparison direction.
    pub direction: SortDirection,
    /// Placement for missing and explicit null.
    pub nulls: NullPlacement,
}

/// Logical continuation position after one emitted record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    /// One normalized sort value per requested sort field. `None` represents
    /// both missing and explicit null for sorting.
    pub sort_values: Vec<Option<Value>>,
    /// Mandatory final binary-key tie-breaker.
    pub key: Vec<u8>,
}

/// One aggregate calculation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Metric {
    /// Count every matched record.
    Count,
    /// Checked integer sum, ignoring missing and null.
    Sum(FieldPath),
    /// Minimum non-null resolved value under the total value order.
    Min(FieldPath),
    /// Maximum non-null resolved value under the total value order.
    Max(FieldPath),
}

/// Named aggregate calculation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedMetric {
    /// Unique nonempty result name.
    pub name: String,
    /// Calculation.
    pub metric: Metric,
}

/// Optional global or grouped aggregation plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregationPlan {
    /// Group-key paths. Empty means one global group.
    pub group_by: Vec<FieldPath>,
    /// Calculations applied to each group.
    pub metrics: Vec<NamedMetric>,
}

/// Complete deterministic query request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Query {
    /// Filter applied before sorting and aggregation.
    pub filter: Filter,
    /// Requested sort fields; binary key ascending is always appended.
    pub sort: Vec<SortField>,
    /// Optional continuation position.
    pub cursor: Option<Cursor>,
    /// Maximum rows in this page; must be nonzero.
    pub limit: usize,
    /// Optional aggregation over the full filtered set.
    pub aggregation: Option<AggregationPlan>,
}

/// Runtime and shape budgets for one complete global execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    /// Maximum records inspected across all shards.
    pub max_scanned_records: u64,
    /// Maximum records retained after filtering.
    pub max_matched_records: u64,
    /// Maximum requested page size.
    pub max_returned_records: usize,
    /// Maximum distinct aggregation groups.
    pub max_groups: usize,
    /// Maximum nodes in the recursive filter expression.
    pub max_filter_nodes: usize,
    /// Maximum recursive filter depth, counting the root as one.
    pub max_filter_depth: usize,
    /// Maximum explicit sort fields.
    pub max_sort_fields: usize,
    /// Maximum group-key fields.
    pub max_group_fields: usize,
    /// Maximum metrics in one plan.
    pub max_metrics: usize,
    /// Cooperative monotonic execution timeout.
    pub timeout: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_scanned_records: 1_000_000,
            max_matched_records: 100_000,
            max_returned_records: 1_000,
            max_groups: 10_000,
            max_filter_nodes: 256,
            max_filter_depth: 64,
            max_sort_fields: 16,
            max_group_fields: 8,
            max_metrics: 32,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Value emitted by one aggregate metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricValue {
    /// Record count.
    Count(u64),
    /// Checked integer result; `None` means no non-null inputs.
    Integer(Option<i128>),
    /// Minimum or maximum value; `None` means no non-null inputs.
    Value(Option<Value>),
}

/// Named aggregate output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedMetricValue {
    /// Metric name copied from the plan.
    pub name: String,
    /// Calculated value.
    pub value: MetricValue,
}

/// One deterministic aggregation group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupResult {
    /// Group values in plan order. `None` means a missing path; explicit null is
    /// represented by `Some(Value::Null)`.
    pub key: Vec<Option<Value>>,
    /// Metric values in plan order.
    pub metrics: Vec<NamedMetricValue>,
}

/// Aggregation output. An ungrouped plan has one empty-key group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregationResult {
    /// Whether the originating plan had explicit group fields.
    pub grouped: bool,
    /// Groups in deterministic key order.
    pub groups: Vec<GroupResult>,
}

/// Complete successful query response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    /// Page rows after global merge, sort, cursor, and limit.
    pub rows: Vec<Record>,
    /// Cursor after the last row when more rows remain.
    pub next_cursor: Option<Cursor>,
    /// Optional aggregation over every filtered record before pagination.
    pub aggregation: Option<AggregationResult>,
    /// Records inspected across every shard.
    pub scanned_records: u64,
    /// Records matching the filter before pagination.
    pub matched_records: u64,
}
