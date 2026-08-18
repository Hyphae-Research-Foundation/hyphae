// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{
    AggregationPlan, AggregationResult, CompareOperator, Cursor, DocumentError, ExecutionLimits,
    FieldPath, Filter, GroupResult, Metric, MetricValue, NamedMetricValue, NullPlacement, Query,
    QueryResult, Record, SortDirection, SortField, Value, encoded_document_len,
};

/// Fixed aggregate key and canonical-document byte budget used by the
/// compatibility-preserving query entry points.
pub const DEFAULT_QUERY_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Failure to validate or completely execute one structured query.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QueryError {
    /// Record keys must be nonempty.
    #[error("query input contains an empty record key")]
    EmptyRecordKey,

    /// Keys must be globally unique across shards.
    #[error("duplicate global record key")]
    DuplicateRecordKey,

    /// A page must request at least one row.
    #[error("query limit must be nonzero")]
    ZeroLimit,

    /// Requested page size exceeds policy.
    #[error("query limit {requested} exceeds maximum {maximum}")]
    ResultLimitExceeded {
        /// Requested page size.
        requested: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Filter expression is too large.
    #[error("filter has {actual} nodes; maximum is {maximum}")]
    FilterNodesExceeded {
        /// Observed filter nodes.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Filter recursion is too deep.
    #[error("filter depth is {actual}; maximum is {maximum}")]
    FilterDepthExceeded {
        /// Observed recursive depth, counting the root as one.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Too many explicit sort fields were requested.
    #[error("query has {actual} sort fields; maximum is {maximum}")]
    SortFieldsExceeded {
        /// Requested fields.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Cursor shape does not match the sort plan.
    #[error("cursor has {actual} sort values; query requires {expected}")]
    CursorShape {
        /// Cursor values.
        actual: usize,
        /// Query sort fields.
        expected: usize,
    },

    /// Cursor keys must be nonempty.
    #[error("cursor key must be nonempty")]
    EmptyCursorKey,

    /// Explicit null must use the canonical `None` cursor representation.
    #[error("cursor contains a noncanonical explicit null sort value")]
    NoncanonicalCursorNull,

    /// A prefix filter literal is not a string or bytes value.
    #[error("prefix filter requires a string or bytes literal")]
    InvalidPrefixType,

    /// Field-path segments must be nonempty UTF-8 strings.
    #[error("field path contains an empty segment")]
    InvalidFieldPath,

    /// Too many group fields were requested.
    #[error("aggregation has {actual} group fields; maximum is {maximum}")]
    GroupFieldsExceeded {
        /// Requested group fields.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Too many metrics were requested.
    #[error("aggregation has {actual} metrics; maximum is {maximum}")]
    MetricsExceeded {
        /// Requested metrics.
        actual: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// Metric names must be nonempty.
    #[error("aggregate metric name must be nonempty")]
    EmptyMetricName,

    /// Metric names must be unique within a plan.
    #[error("duplicate aggregate metric name: {name}")]
    DuplicateMetricName {
        /// Duplicated name.
        name: String,
    },

    /// Global scan budget was exhausted.
    #[error("global scanned-record budget exceeded: {maximum}")]
    ScannedBudgetExceeded {
        /// Configured maximum.
        maximum: u64,
    },

    /// Filtered-result memory budget was exhausted.
    #[error("global matched-record budget exceeded: {maximum}")]
    MatchedBudgetExceeded {
        /// Configured maximum.
        maximum: u64,
    },

    /// Aggregation group budget was exhausted.
    #[error("aggregation group budget exceeded: {maximum}")]
    GroupBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },

    /// Cooperative monotonic deadline expired.
    #[error("query execution timed out")]
    TimedOut,

    /// Sum encountered a non-integer value.
    #[error("aggregate metric {name} requires integer values")]
    MetricTypeMismatch {
        /// Metric name.
        name: String,
    },

    /// Checked aggregate arithmetic overflowed.
    #[error("aggregate metric {name} overflowed")]
    ArithmeticOverflow {
        /// Metric name.
        name: String,
    },

    /// Internal aggregate state did not match its validated metric plan.
    #[error("aggregate metric state does not match its plan")]
    MetricStateMismatch,
}

/// Failure from the additive query entry points that enforce an aggregate
/// key/canonical-document byte budget.
///
/// [`QueryError`] remains the exhaustive error surface published in 0.2.0.
/// This separate type adds byte-policy failures without invalidating legacy
/// downstream matches.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BoundedQueryError {
    /// Ordinary query validation or execution failed.
    #[error(transparent)]
    Query(#[from] QueryError),
    /// Aggregate key and canonical document bytes exceeded policy.
    #[error("global scanned-byte budget exceeded: {maximum}")]
    ScannedByteBudgetExceeded {
        /// Configured maximum.
        maximum: u64,
    },
    /// An in-memory record cannot be represented as one canonical document.
    #[error("query record is not a canonical document: {0}")]
    RecordDocument(#[from] DocumentError),
}

/// Injectable monotonic clock used to make timeout semantics testable.
pub trait MonotonicClock {
    /// Returns a nondecreasing duration in an arbitrary local epoch.
    fn now(&mut self) -> Duration;
}

/// Production monotonic clock backed by [`Instant`].
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl MonotonicClock for SystemClock {
    fn now(&mut self) -> Duration {
        self.origin.elapsed()
    }
}

/// Executes one query over complete logical shards using the system clock.
///
/// # Errors
///
/// Returns a validation, budget, timeout, duplicate-key, or aggregation error.
pub fn execute(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
) -> Result<QueryResult, QueryError> {
    execute_with_clock_without_byte_limit(shards, query, limits, &mut SystemClock::default())
}

/// Executes one query under an explicit aggregate key/document byte budget.
///
/// # Errors
///
/// Returns a validation, budget, timeout, duplicate-key, document, or
/// aggregation error.
pub fn execute_with_byte_limit(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
    max_scanned_bytes: u64,
) -> Result<QueryResult, BoundedQueryError> {
    execute_with_clock_and_byte_limit(
        shards,
        query,
        limits,
        max_scanned_bytes,
        &mut SystemClock::default(),
    )
}

/// Executes one query with an injectable monotonic clock.
///
/// This is public so conformance suites can prove timeout behavior without
/// wall-clock sleeps.
///
/// # Errors
///
/// Returns a validation, budget, timeout, duplicate-key, or aggregation error.
pub fn execute_with_clock(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
    clock: &mut impl MonotonicClock,
) -> Result<QueryResult, QueryError> {
    execute_with_clock_without_byte_limit(shards, query, limits, clock)
}

/// Executes one query with an injectable monotonic clock and explicit
/// aggregate key/document byte budget.
///
/// # Errors
///
/// Returns a validation, budget, timeout, duplicate-key, document, or
/// aggregation error.
pub fn execute_with_clock_and_byte_limit(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
    max_scanned_bytes: u64,
    clock: &mut impl MonotonicClock,
) -> Result<QueryResult, BoundedQueryError> {
    execute_with_clock_impl(shards, query, limits, Some(max_scanned_bytes), clock)
}

fn execute_with_clock_without_byte_limit(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
    clock: &mut impl MonotonicClock,
) -> Result<QueryResult, QueryError> {
    match execute_with_clock_impl(shards, query, limits, None, clock) {
        Ok(result) => Ok(result),
        Err(BoundedQueryError::Query(source)) => Err(source),
        // INVARIANT: `max_scanned_bytes` is `None` on this path, so the
        // bounded-only variants cannot be produced by the shared executor.
        Err(
            BoundedQueryError::ScannedByteBudgetExceeded { .. }
            | BoundedQueryError::RecordDocument(_),
        ) => unreachable!("legacy query execution does not inspect canonical document bytes"),
    }
}

fn execute_with_clock_impl(
    shards: &[&[Record]],
    query: &Query,
    limits: &ExecutionLimits,
    max_scanned_bytes: Option<u64>,
    clock: &mut impl MonotonicClock,
) -> Result<QueryResult, BoundedQueryError> {
    validate_query(query, limits)?;
    let started = clock.now();
    let deadline = started.checked_add(limits.timeout).unwrap_or(Duration::MAX);
    let mut budget = ExecutionBudget {
        clock,
        deadline,
        limits,
        scanned: 0,
        scanned_bytes: 0,
        max_scanned_bytes,
        matched: 0,
    };
    budget.check_timeout()?;

    let mut keys = BTreeSet::<&[u8]>::new();
    let mut candidates = Vec::new();
    for shard in shards {
        for record in *shard {
            budget.scan()?;
            budget.account_record_bytes(record)?;
            if record.key.is_empty() {
                return Err(QueryError::EmptyRecordKey.into());
            }
            if !keys.insert(record.key.as_slice()) {
                return Err(QueryError::DuplicateRecordKey.into());
            }
            if filter_matches(&query.filter, &record.value) {
                budget.match_record()?;
                candidates.push(record);
            }
        }
    }

    candidates.sort_by(|left, right| compare_records(left, right, &query.sort));
    budget.check_timeout()?;
    let aggregation = query
        .aggregation
        .as_ref()
        .map(|plan| aggregate(&candidates, plan, &mut budget))
        .transpose()?;

    let page_start = query.cursor.as_ref().map_or(0, |cursor| {
        candidates.partition_point(|record| {
            compare_record_to_cursor(record, cursor, &query.sort) != Ordering::Greater
        })
    });
    let remaining = &candidates[page_start..];
    let page_length = remaining.len().min(query.limit);
    let mut rows = Vec::with_capacity(page_length);
    for record in &remaining[..page_length] {
        budget.check_timeout()?;
        rows.push((*record).clone());
    }
    let next_cursor = if remaining.len() > page_length {
        rows.last().map(|record| cursor_for(record, &query.sort))
    } else {
        None
    };
    budget.check_timeout()?;

    Ok(QueryResult {
        rows,
        next_cursor,
        aggregation,
        scanned_records: budget.scanned,
        matched_records: budget.matched,
    })
}

struct ExecutionBudget<'limits, 'clock, Clock> {
    clock: &'clock mut Clock,
    deadline: Duration,
    limits: &'limits ExecutionLimits,
    scanned: u64,
    scanned_bytes: u64,
    max_scanned_bytes: Option<u64>,
    matched: u64,
}

impl<Clock: MonotonicClock> ExecutionBudget<'_, '_, Clock> {
    fn check_timeout(&mut self) -> Result<(), QueryError> {
        if self.clock.now() >= self.deadline {
            Err(QueryError::TimedOut)
        } else {
            Ok(())
        }
    }

    fn scan(&mut self) -> Result<(), QueryError> {
        self.check_timeout()?;
        if self.scanned >= self.limits.max_scanned_records {
            return Err(QueryError::ScannedBudgetExceeded {
                maximum: self.limits.max_scanned_records,
            });
        }
        self.scanned = self.scanned.saturating_add(1);
        Ok(())
    }

    fn account_record_bytes(&mut self, record: &Record) -> Result<(), BoundedQueryError> {
        let Some(maximum) = self.max_scanned_bytes else {
            return Ok(());
        };
        self.scanned_bytes =
            checked_scanned_bytes(self.scanned_bytes, record_bytes(record, maximum)?, maximum)?;
        Ok(())
    }

    fn match_record(&mut self) -> Result<(), QueryError> {
        if self.matched >= self.limits.max_matched_records {
            return Err(QueryError::MatchedBudgetExceeded {
                maximum: self.limits.max_matched_records,
            });
        }
        self.matched = self.matched.saturating_add(1);
        Ok(())
    }
}

fn checked_scanned_bytes(
    consumed: u64,
    record_bytes: u64,
    maximum: u64,
) -> Result<u64, BoundedQueryError> {
    let next = consumed
        .checked_add(record_bytes)
        .ok_or(BoundedQueryError::ScannedByteBudgetExceeded { maximum })?;
    if next > maximum {
        Err(BoundedQueryError::ScannedByteBudgetExceeded { maximum })
    } else {
        Ok(next)
    }
}

fn record_bytes(record: &Record, maximum: u64) -> Result<u64, BoundedQueryError> {
    let key_bytes = u64::try_from(record.key.len())
        .map_err(|_| BoundedQueryError::ScannedByteBudgetExceeded { maximum })?;
    let document_bytes = u64::try_from(encoded_document_len(&record.value)?)
        .map_err(|_| BoundedQueryError::ScannedByteBudgetExceeded { maximum })?;
    key_bytes
        .checked_add(document_bytes)
        .ok_or(BoundedQueryError::ScannedByteBudgetExceeded { maximum })
}

/// Validates query shape and cursor canonicality without scanning records.
///
/// # Errors
///
/// Returns the same pre-execution validation errors as [`execute`].
pub fn validate_query(query: &Query, limits: &ExecutionLimits) -> Result<(), QueryError> {
    if query.limit == 0 {
        return Err(QueryError::ZeroLimit);
    }
    if query.limit > limits.max_returned_records {
        return Err(QueryError::ResultLimitExceeded {
            requested: query.limit,
            maximum: limits.max_returned_records,
        });
    }
    let (filter_nodes, filter_depth) = filter_shape(&query.filter);
    if filter_nodes > limits.max_filter_nodes {
        return Err(QueryError::FilterNodesExceeded {
            actual: filter_nodes,
            maximum: limits.max_filter_nodes,
        });
    }
    if filter_depth > limits.max_filter_depth {
        return Err(QueryError::FilterDepthExceeded {
            actual: filter_depth,
            maximum: limits.max_filter_depth,
        });
    }
    validate_field_paths(query)?;
    validate_prefixes(&query.filter)?;
    if query.sort.len() > limits.max_sort_fields {
        return Err(QueryError::SortFieldsExceeded {
            actual: query.sort.len(),
            maximum: limits.max_sort_fields,
        });
    }
    if let Some(cursor) = &query.cursor {
        validate_cursor(cursor, query.sort.len())?;
    }
    if let Some(plan) = &query.aggregation {
        validate_aggregation(plan, limits)?;
    }
    Ok(())
}

fn validate_field_paths(query: &Query) -> Result<(), QueryError> {
    let mut filters = vec![&query.filter];
    while let Some(filter) = filters.pop() {
        let path = match filter {
            Filter::Exists(path)
            | Filter::Compare { path, .. }
            | Filter::Prefix { path, .. }
            | Filter::Contains { path, .. } => Some(path),
            Filter::All(children) | Filter::Any(children) => {
                filters.extend(children);
                None
            }
            Filter::Not(child) => {
                filters.push(child);
                None
            }
            Filter::MatchAll => None,
        };
        if path.is_some_and(path_has_empty_segment) {
            return Err(QueryError::InvalidFieldPath);
        }
    }
    if query
        .sort
        .iter()
        .any(|field| path_has_empty_segment(&field.path))
    {
        return Err(QueryError::InvalidFieldPath);
    }
    if let Some(plan) = &query.aggregation
        && (plan.group_by.iter().any(path_has_empty_segment)
            || plan.metrics.iter().any(|metric| match &metric.metric {
                Metric::Count => false,
                Metric::Sum(path) | Metric::Min(path) | Metric::Max(path) => {
                    path_has_empty_segment(path)
                }
            }))
    {
        return Err(QueryError::InvalidFieldPath);
    }
    Ok(())
}

fn path_has_empty_segment(path: &FieldPath) -> bool {
    path.segments().iter().any(String::is_empty)
}

fn filter_shape(filter: &Filter) -> (usize, usize) {
    let mut count = 0_usize;
    let mut maximum_depth = 0_usize;
    let mut pending = vec![(filter, 1_usize)];
    while let Some((current, depth)) = pending.pop() {
        count = count.saturating_add(1);
        maximum_depth = maximum_depth.max(depth);
        match current {
            Filter::All(children) | Filter::Any(children) => pending.extend(
                children
                    .iter()
                    .map(|child| (child, depth.saturating_add(1))),
            ),
            Filter::Not(child) => pending.push((child, depth.saturating_add(1))),
            Filter::MatchAll
            | Filter::Exists(_)
            | Filter::Compare { .. }
            | Filter::Prefix { .. }
            | Filter::Contains { .. } => {}
        }
    }
    (count, maximum_depth)
}

fn validate_prefixes(filter: &Filter) -> Result<(), QueryError> {
    let mut pending = vec![filter];
    while let Some(current) = pending.pop() {
        match current {
            Filter::Prefix { prefix, .. } => {
                if !matches!(prefix, Value::String(_) | Value::Bytes(_)) {
                    return Err(QueryError::InvalidPrefixType);
                }
            }
            Filter::All(children) | Filter::Any(children) => pending.extend(children),
            Filter::Not(child) => pending.push(child),
            Filter::MatchAll
            | Filter::Exists(_)
            | Filter::Compare { .. }
            | Filter::Contains { .. } => {}
        }
    }
    Ok(())
}

fn validate_cursor(cursor: &Cursor, sort_fields: usize) -> Result<(), QueryError> {
    if cursor.sort_values.len() != sort_fields {
        return Err(QueryError::CursorShape {
            actual: cursor.sort_values.len(),
            expected: sort_fields,
        });
    }
    if cursor.key.is_empty() {
        return Err(QueryError::EmptyCursorKey);
    }
    if cursor
        .sort_values
        .iter()
        .any(|value| value == &Some(Value::Null))
    {
        return Err(QueryError::NoncanonicalCursorNull);
    }
    Ok(())
}

fn validate_aggregation(
    plan: &AggregationPlan,
    limits: &ExecutionLimits,
) -> Result<(), QueryError> {
    if plan.group_by.len() > limits.max_group_fields {
        return Err(QueryError::GroupFieldsExceeded {
            actual: plan.group_by.len(),
            maximum: limits.max_group_fields,
        });
    }
    if plan.metrics.len() > limits.max_metrics {
        return Err(QueryError::MetricsExceeded {
            actual: plan.metrics.len(),
            maximum: limits.max_metrics,
        });
    }
    let mut names = BTreeSet::new();
    for metric in &plan.metrics {
        if metric.name.is_empty() {
            return Err(QueryError::EmptyMetricName);
        }
        if !names.insert(metric.name.as_str()) {
            return Err(QueryError::DuplicateMetricName {
                name: metric.name.clone(),
            });
        }
    }
    Ok(())
}

fn filter_matches(filter: &Filter, root: &Value) -> bool {
    match filter {
        Filter::MatchAll => true,
        Filter::Exists(path) => path.resolve(root).is_some(),
        Filter::Compare {
            path,
            operator,
            value,
        } => path
            .resolve(root)
            .is_some_and(|actual| compare_filter(actual, value, *operator)),
        Filter::Prefix { path, prefix } => path
            .resolve(root)
            .is_some_and(|actual| has_prefix(actual, prefix)),
        Filter::Contains { path, needle } => path
            .resolve(root)
            .is_some_and(|actual| contains(actual, needle)),
        Filter::All(children) => children.iter().all(|child| filter_matches(child, root)),
        Filter::Any(children) => children.iter().any(|child| filter_matches(child, root)),
        Filter::Not(child) => !filter_matches(child, root),
    }
}

fn compare_filter(left: &Value, right: &Value, operator: CompareOperator) -> bool {
    match operator {
        CompareOperator::Equal => left == right,
        CompareOperator::NotEqual => left != right,
        CompareOperator::Less => same_variant(left, right) && left < right,
        CompareOperator::LessOrEqual => same_variant(left, right) && left <= right,
        CompareOperator::Greater => same_variant(left, right) && left > right,
        CompareOperator::GreaterOrEqual => same_variant(left, right) && left >= right,
    }
}

fn same_variant(left: &Value, right: &Value) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn has_prefix(actual: &Value, prefix: &Value) -> bool {
    match (actual, prefix) {
        (Value::String(actual), Value::String(prefix)) => actual.starts_with(prefix),
        (Value::Bytes(actual), Value::Bytes(prefix)) => actual.starts_with(prefix),
        _ => false,
    }
}

fn contains(actual: &Value, needle: &Value) -> bool {
    match (actual, needle) {
        (Value::Array(values), needle) => values.contains(needle),
        (Value::String(actual), Value::String(needle)) => actual.contains(needle),
        (Value::Bytes(actual), Value::Bytes(needle)) => {
            needle.is_empty() || actual.windows(needle.len()).any(|window| window == needle)
        }
        _ => false,
    }
}

fn compare_records(left: &Record, right: &Record, sort: &[SortField]) -> Ordering {
    for field in sort {
        let ordering = compare_sort_values(
            normalized_sort_value(field.path.resolve(&left.value)),
            normalized_sort_value(field.path.resolve(&right.value)),
            field,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.key.cmp(&right.key)
}

fn compare_record_to_cursor(record: &Record, cursor: &Cursor, sort: &[SortField]) -> Ordering {
    for (field, cursor_value) in sort.iter().zip(&cursor.sort_values) {
        let ordering = compare_sort_values(
            normalized_sort_value(field.path.resolve(&record.value)),
            cursor_value.as_ref(),
            field,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    record.key.cmp(&cursor.key)
}

fn normalized_sort_value(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !matches!(value, Value::Null))
}

fn compare_sort_values(left: Option<&Value>, right: Option<&Value>, field: &SortField) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => match field.nulls {
            NullPlacement::First => Ordering::Less,
            NullPlacement::Last => Ordering::Greater,
        },
        (Some(_), None) => match field.nulls {
            NullPlacement::First => Ordering::Greater,
            NullPlacement::Last => Ordering::Less,
        },
        (Some(left), Some(right)) => match field.direction {
            SortDirection::Ascending => left.cmp(right),
            SortDirection::Descending => right.cmp(left),
        },
    }
}

fn cursor_for(record: &Record, sort: &[SortField]) -> Cursor {
    Cursor {
        sort_values: sort
            .iter()
            .map(|field| normalized_sort_value(field.path.resolve(&record.value)).cloned())
            .collect(),
        key: record.key.clone(),
    }
}

#[derive(Clone, Debug)]
enum MetricState {
    Count(u64),
    Sum(Option<i128>),
    Min(Option<Value>),
    Max(Option<Value>),
}

fn aggregate<Clock: MonotonicClock>(
    records: &[&Record],
    plan: &AggregationPlan,
    budget: &mut ExecutionBudget<'_, '_, Clock>,
) -> Result<AggregationResult, QueryError> {
    let grouped = !plan.group_by.is_empty();
    let mut groups: BTreeMap<Vec<Option<Value>>, Vec<MetricState>> = BTreeMap::new();
    if !grouped {
        ensure_group(&mut groups, Vec::new(), plan, budget.limits.max_groups)?;
    }
    for record in records {
        budget.check_timeout()?;
        let key = plan
            .group_by
            .iter()
            .map(|path| path.resolve(&record.value).cloned())
            .collect::<Vec<_>>();
        let states = ensure_group(&mut groups, key, plan, budget.limits.max_groups)?;
        update_metrics(states, &plan.metrics, &record.value)?;
    }

    let groups = groups
        .into_iter()
        .map(|(key, states)| GroupResult {
            key,
            metrics: plan
                .metrics
                .iter()
                .zip(states)
                .map(|(metric, state)| NamedMetricValue {
                    name: metric.name.clone(),
                    value: finish_metric(state),
                })
                .collect(),
        })
        .collect();
    Ok(AggregationResult { grouped, groups })
}

fn ensure_group<'groups>(
    groups: &'groups mut BTreeMap<Vec<Option<Value>>, Vec<MetricState>>,
    key: Vec<Option<Value>>,
    plan: &AggregationPlan,
    maximum: usize,
) -> Result<&'groups mut Vec<MetricState>, QueryError> {
    if !groups.contains_key(&key) && groups.len() >= maximum {
        return Err(QueryError::GroupBudgetExceeded { maximum });
    }
    Ok(groups.entry(key).or_insert_with(|| {
        plan.metrics
            .iter()
            .map(|metric| match metric.metric {
                Metric::Count => MetricState::Count(0),
                Metric::Sum(_) => MetricState::Sum(None),
                Metric::Min(_) => MetricState::Min(None),
                Metric::Max(_) => MetricState::Max(None),
            })
            .collect()
    }))
}

fn update_metrics(
    states: &mut [MetricState],
    metrics: &[crate::NamedMetric],
    root: &Value,
) -> Result<(), QueryError> {
    for (state, named) in states.iter_mut().zip(metrics) {
        match (&named.metric, state) {
            (Metric::Count, MetricState::Count(count)) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| QueryError::ArithmeticOverflow {
                        name: named.name.clone(),
                    })?;
            }
            (Metric::Sum(path), MetricState::Sum(sum)) => {
                let Some(value) = path.resolve(root) else {
                    continue;
                };
                match value {
                    Value::Null => {}
                    Value::Integer(value) => {
                        *sum = Some(
                            sum.unwrap_or(0)
                                .checked_add(i128::from(*value))
                                .ok_or_else(|| QueryError::ArithmeticOverflow {
                                    name: named.name.clone(),
                                })?,
                        );
                    }
                    _ => {
                        return Err(QueryError::MetricTypeMismatch {
                            name: named.name.clone(),
                        });
                    }
                }
            }
            (Metric::Min(path), MetricState::Min(minimum)) => {
                update_extreme(minimum, path, root, Ordering::Less);
            }
            (Metric::Max(path), MetricState::Max(maximum)) => {
                update_extreme(maximum, path, root, Ordering::Greater);
            }
            _ => return Err(QueryError::MetricStateMismatch),
        }
    }
    Ok(())
}

fn update_extreme(current: &mut Option<Value>, path: &FieldPath, root: &Value, desired: Ordering) {
    let Some(candidate) = path
        .resolve(root)
        .filter(|value| !matches!(value, Value::Null))
    else {
        return;
    };
    if current
        .as_ref()
        .is_none_or(|existing| candidate.cmp(existing) == desired)
    {
        *current = Some(candidate.clone());
    }
}

fn finish_metric(state: MetricState) -> MetricValue {
    match state {
        MetricState::Count(count) => MetricValue::Count(count),
        MetricState::Sum(sum) => MetricValue::Integer(sum),
        MetricState::Min(value) | MetricState::Max(value) => MetricValue::Value(value),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use super::{
        BoundedQueryError, MonotonicClock, QueryError, checked_scanned_bytes, execute,
        execute_with_byte_limit, execute_with_clock,
    };
    use crate::{
        AggregationPlan, CompareOperator, ExecutionLimits, FieldPath, Filter, Metric, MetricValue,
        NamedMetric, NullPlacement, Query, Record, SortDirection, SortField, Value,
    };

    fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn record(key: &'static [u8], score: i64, group: &'static str) -> Record {
        Record::new(
            key,
            object([
                ("score", Value::Integer(score)),
                ("group", Value::String(group.to_owned())),
            ]),
        )
    }

    fn score_descending() -> SortField {
        SortField {
            path: FieldPath::field("score"),
            direction: SortDirection::Descending,
            nulls: NullPlacement::Last,
        }
    }

    fn query(limit: usize) -> Query {
        Query {
            filter: Filter::MatchAll,
            sort: vec![score_descending()],
            cursor: None,
            limit,
            aggregation: None,
        }
    }

    #[test]
    fn global_merge_precedes_limit_and_cursor_is_stable() -> Result<(), QueryError> {
        let first = vec![record(b"a", 10, "x"), record(b"d", 1, "x")];
        let second = vec![
            record(b"b", 9, "y"),
            record(b"c", 8, "y"),
            record(b"e", 8, "z"),
        ];
        let shards = [first.as_slice(), second.as_slice()];
        let first_page = execute(&shards, &query(3), &ExecutionLimits::default())?;
        assert_eq!(
            first_page
                .rows
                .iter()
                .map(|record| record.key.as_slice())
                .collect::<Vec<_>>(),
            [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]
        );

        let second_page = execute(
            &shards,
            &Query {
                cursor: first_page.next_cursor,
                ..query(3)
            },
            &ExecutionLimits::default(),
        )?;
        assert_eq!(
            second_page
                .rows
                .iter()
                .map(|record| record.key.as_slice())
                .collect::<Vec<_>>(),
            [b"e".as_slice(), b"d".as_slice()]
        );
        assert_eq!(second_page.next_cursor, None);
        Ok(())
    }

    #[test]
    fn filters_and_grouped_aggregates_use_the_full_match_set() -> Result<(), QueryError> {
        let records = vec![
            record(b"a", 10, "x"),
            record(b"b", 8, "x"),
            record(b"c", 7, "y"),
            record(b"d", 2, "y"),
        ];
        let result = execute(
            &[records.as_slice()],
            &Query {
                filter: Filter::Compare {
                    path: FieldPath::field("score"),
                    operator: CompareOperator::GreaterOrEqual,
                    value: Value::Integer(7),
                },
                sort: vec![score_descending()],
                cursor: None,
                limit: 1,
                aggregation: Some(AggregationPlan {
                    group_by: vec![FieldPath::field("group")],
                    metrics: vec![
                        NamedMetric {
                            name: "count".to_owned(),
                            metric: Metric::Count,
                        },
                        NamedMetric {
                            name: "sum".to_owned(),
                            metric: Metric::Sum(FieldPath::field("score")),
                        },
                    ],
                }),
            },
            &ExecutionLimits::default(),
        )?;
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.matched_records, 3);
        let aggregation = result.aggregation.ok_or(QueryError::TimedOut)?;
        assert_eq!(aggregation.groups.len(), 2);
        assert_eq!(
            aggregation.groups[0].metrics[0].value,
            MetricValue::Count(2)
        );
        assert_eq!(
            aggregation.groups[0].metrics[1].value,
            MetricValue::Integer(Some(18))
        );
        assert_eq!(
            aggregation.groups[1].metrics[0].value,
            MetricValue::Count(1)
        );
        assert_eq!(
            aggregation.groups[1].metrics[1].value,
            MetricValue::Integer(Some(7))
        );
        Ok(())
    }

    #[test]
    fn missing_and_null_have_explicit_filter_and_sort_semantics() -> Result<(), QueryError> {
        let records = vec![
            Record::new(b"missing", object([])),
            Record::new(b"null", object([("value", Value::Null)])),
            Record::new(b"integer", object([("value", Value::Integer(1))])),
        ];
        let result = execute(
            &[records.as_slice()],
            &Query {
                filter: Filter::Not(Box::new(Filter::Compare {
                    path: FieldPath::field("value"),
                    operator: CompareOperator::Equal,
                    value: Value::Integer(1),
                })),
                sort: vec![SortField {
                    path: FieldPath::field("value"),
                    direction: SortDirection::Ascending,
                    nulls: NullPlacement::First,
                }],
                cursor: None,
                limit: 10,
                aggregation: None,
            },
            &ExecutionLimits::default(),
        )?;
        assert_eq!(
            result
                .rows
                .iter()
                .map(|record| record.key.as_slice())
                .collect::<Vec<_>>(),
            [b"missing".as_slice(), b"null".as_slice()]
        );
        Ok(())
    }

    #[test]
    fn global_work_budgets_fail_without_partial_results() {
        let records = vec![record(b"a", 1, "x"), record(b"b", 2, "x")];
        let limits = ExecutionLimits {
            max_scanned_records: 1,
            ..ExecutionLimits::default()
        };
        assert_eq!(
            execute(&[records.as_slice()], &query(1), &limits),
            Err(QueryError::ScannedBudgetExceeded { maximum: 1 })
        );
    }

    #[test]
    fn scanned_byte_budget_is_exact_global_and_includes_nonmatches() -> Result<(), BoundedQueryError>
    {
        let first = vec![record(b"a", 1, "x")];
        let second = vec![record(b"b", 2, "y")];
        let shards = [first.as_slice(), second.as_slice()];
        let query = Query {
            filter: Filter::Compare {
                path: FieldPath::field("score"),
                operator: CompareOperator::Greater,
                value: Value::Integer(100),
            },
            sort: Vec::new(),
            cursor: None,
            limit: 1,
            aggregation: None,
        };
        let total = first
            .iter()
            .chain(&second)
            .map(|record| super::record_bytes(record, u64::MAX))
            .try_fold(0_u64, |total, bytes| {
                total
                    .checked_add(bytes?)
                    .ok_or(BoundedQueryError::ScannedByteBudgetExceeded { maximum: u64::MAX })
            })?;
        let result = execute_with_byte_limit(&shards, &query, &ExecutionLimits::default(), total)?;
        assert_eq!(result.scanned_records, 2);
        assert_eq!(result.matched_records, 0);
        assert!(result.rows.is_empty());
        assert_eq!(
            execute_with_byte_limit(&shards, &query, &ExecutionLimits::default(), total - 1,),
            Err(BoundedQueryError::ScannedByteBudgetExceeded { maximum: total - 1 })
        );
        assert_eq!(
            checked_scanned_bytes(u64::MAX, 1, u64::MAX),
            Err(BoundedQueryError::ScannedByteBudgetExceeded { maximum: u64::MAX })
        );
        Ok(())
    }

    #[test]
    fn matched_and_group_budgets_are_global() {
        let records = vec![record(b"a", 1, "x"), record(b"b", 2, "y")];
        let matched_limits = ExecutionLimits {
            max_matched_records: 1,
            ..ExecutionLimits::default()
        };
        assert_eq!(
            execute(&[records.as_slice()], &query(1), &matched_limits),
            Err(QueryError::MatchedBudgetExceeded { maximum: 1 })
        );

        let group_limits = ExecutionLimits {
            max_groups: 1,
            ..ExecutionLimits::default()
        };
        let grouped = Query {
            aggregation: Some(AggregationPlan {
                group_by: vec![FieldPath::field("group")],
                metrics: vec![NamedMetric {
                    name: "count".to_owned(),
                    metric: Metric::Count,
                }],
            }),
            ..query(1)
        };
        assert_eq!(
            execute(&[records.as_slice()], &grouped, &group_limits),
            Err(QueryError::GroupBudgetExceeded { maximum: 1 })
        );
    }

    #[test]
    fn shape_and_cursor_validation_happen_before_execution() {
        let records = vec![record(b"a", 1, "x")];
        let filter_limits = ExecutionLimits {
            max_filter_nodes: 1,
            ..ExecutionLimits::default()
        };
        let nested = Query {
            filter: Filter::Not(Box::new(Filter::MatchAll)),
            ..query(1)
        };
        assert_eq!(
            execute(&[records.as_slice()], &nested, &filter_limits),
            Err(QueryError::FilterNodesExceeded {
                actual: 2,
                maximum: 1
            })
        );

        let depth_limits = ExecutionLimits {
            max_filter_depth: 1,
            ..ExecutionLimits::default()
        };
        assert_eq!(
            execute(&[records.as_slice()], &nested, &depth_limits),
            Err(QueryError::FilterDepthExceeded {
                actual: 2,
                maximum: 1
            })
        );

        let empty_segment = Query {
            filter: Filter::Exists(FieldPath::new(["nested", ""])),
            ..query(1)
        };
        assert_eq!(
            execute(
                &[records.as_slice()],
                &empty_segment,
                &ExecutionLimits::default()
            ),
            Err(QueryError::InvalidFieldPath)
        );

        let malformed_cursor = Query {
            cursor: Some(crate::Cursor {
                sort_values: Vec::new(),
                key: b"a".to_vec(),
            }),
            ..query(1)
        };
        assert_eq!(
            execute(
                &[records.as_slice()],
                &malformed_cursor,
                &ExecutionLimits::default()
            ),
            Err(QueryError::CursorShape {
                actual: 0,
                expected: 1
            })
        );
    }

    #[test]
    fn grouped_missing_and_null_are_distinct() -> Result<(), QueryError> {
        let records = vec![
            Record::new(b"missing", object([])),
            Record::new(b"null", object([("value", Value::Null)])),
        ];
        let result = execute(
            &[records.as_slice()],
            &Query {
                aggregation: Some(AggregationPlan {
                    group_by: vec![FieldPath::field("value")],
                    metrics: vec![NamedMetric {
                        name: "count".to_owned(),
                        metric: Metric::Count,
                    }],
                }),
                ..query(10)
            },
            &ExecutionLimits::default(),
        )?;
        let aggregation = result.aggregation.ok_or(QueryError::MetricStateMismatch)?;
        assert_eq!(aggregation.groups[0].key, [None]);
        assert_eq!(aggregation.groups[1].key, [Some(Value::Null)]);
        Ok(())
    }

    #[test]
    fn sum_rejects_non_integer_values() {
        let records = vec![Record::new(
            b"bad",
            object([("score", Value::String("not-an-integer".to_owned()))]),
        )];
        let request = Query {
            aggregation: Some(AggregationPlan {
                group_by: Vec::new(),
                metrics: vec![NamedMetric {
                    name: "sum".to_owned(),
                    metric: Metric::Sum(FieldPath::field("score")),
                }],
            }),
            ..query(1)
        };
        assert_eq!(
            execute(&[records.as_slice()], &request, &ExecutionLimits::default()),
            Err(QueryError::MetricTypeMismatch {
                name: "sum".to_owned()
            })
        );
    }

    #[test]
    fn duplicate_keys_across_shards_are_rejected() {
        let first = vec![record(b"same", 1, "x")];
        let second = vec![record(b"same", 2, "y")];
        assert_eq!(
            execute(
                &[first.as_slice(), second.as_slice()],
                &query(1),
                &ExecutionLimits::default()
            ),
            Err(QueryError::DuplicateRecordKey)
        );
    }

    struct StepClock {
        current: Duration,
        step: Duration,
    }

    impl MonotonicClock for StepClock {
        fn now(&mut self) -> Duration {
            let current = self.current;
            self.current = self.current.saturating_add(self.step);
            current
        }
    }

    #[test]
    fn timeout_uses_an_injectable_monotonic_clock() {
        let records = vec![record(b"a", 1, "x"), record(b"b", 2, "x")];
        let limits = ExecutionLimits {
            timeout: Duration::from_millis(3),
            ..ExecutionLimits::default()
        };
        let mut clock = StepClock {
            current: Duration::ZERO,
            step: Duration::from_millis(1),
        };
        assert_eq!(
            execute_with_clock(&[records.as_slice()], &query(1), &limits, &mut clock),
            Err(QueryError::TimedOut)
        );
    }

    #[test]
    fn timeout_is_checked_before_returning_the_final_page() {
        let limits = ExecutionLimits {
            timeout: Duration::from_millis(3),
            ..ExecutionLimits::default()
        };
        let mut clock = StepClock {
            current: Duration::ZERO,
            step: Duration::from_millis(1),
        };

        assert_eq!(
            execute_with_clock(&[&[]], &query(1), &limits, &mut clock),
            Err(QueryError::TimedOut)
        );
    }
}
