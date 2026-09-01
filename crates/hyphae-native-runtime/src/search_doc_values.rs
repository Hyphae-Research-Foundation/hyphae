// SPDX-License-Identifier: Apache-2.0

//! Bounded native doc-value filtering, sorting, faceting, and aggregation.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

use hyphae_native_types::CanonicalF64;
use thiserror::Error;

/// Maximum candidates admitted by the default policy.
pub const MAX_DOC_VALUE_CANDIDATES: usize = 100_000;
/// Maximum matched candidates retained by the default policy.
pub const MAX_DOC_VALUE_MATCHES: usize = 100_000;
/// Maximum filter nodes admitted by the default policy.
pub const MAX_DOC_VALUE_FILTER_NODES: usize = 128;
/// Maximum filter depth admitted by the default policy.
pub const MAX_DOC_VALUE_FILTER_DEPTH: usize = 32;
/// Maximum explicit sort fields admitted by the default policy.
pub const MAX_DOC_VALUE_SORTS: usize = 8;
/// Maximum facet requests admitted by the default policy.
pub const MAX_DOC_VALUE_FACETS: usize = 8;
/// Maximum aggregate metrics admitted by the default policy.
pub const MAX_DOC_VALUE_AGGREGATIONS: usize = 16;
/// Maximum distinct terms retained per facet by the default policy.
pub const MAX_DOC_VALUE_FACET_TERMS: usize = 10_000;
/// Maximum doc-value fields admitted on one candidate by the default policy.
pub const MAX_DOC_VALUES_PER_CANDIDATE: usize = 64;
/// Maximum bytes admitted by one field name or string/binary value.
pub const MAX_DOC_VALUE_BYTES: usize = 4_096;
/// Maximum returned hits admitted by the default policy.
pub const MAX_DOC_VALUE_HITS: usize = 1_024;

/// One scalar stored in a native search doc-values column.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DocValue {
    /// Boolean scalar.
    Boolean(bool),
    /// Signed 64-bit integer scalar.
    Integer(i64),
    /// Canonical IEEE-754 binary64 scalar under the deterministic total
    /// order; `NaN` payloads and signed zero are collapsed on ingest.
    Float(CanonicalF64),
    /// UTF-8 scalar.
    String(String),
    /// Opaque binary scalar.
    Bytes(Vec<u8>),
}

/// One scored search candidate and its column-oriented scalar values.
#[derive(Clone, Debug, PartialEq)]
pub struct DocValueCandidate {
    /// Stable, nonempty binary document identity.
    pub document_id: Vec<u8>,
    /// Finite nonnegative relevance score.
    pub score: f64,
    /// Exact field-name to scalar mapping. Absence represents missing.
    pub values: BTreeMap<String, DocValue>,
}

/// Scalar comparison operator used by a doc-value filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocValueOperator {
    /// Exact type-and-value equality.
    Equal,
    /// Exact inequality. A missing field does not match.
    NotEqual,
    /// Same-type less-than comparison.
    Less,
    /// Same-type less-than-or-equal comparison.
    LessOrEqual,
    /// Same-type greater-than comparison.
    Greater,
    /// Same-type greater-than-or-equal comparison.
    GreaterOrEqual,
}

/// Recursive deterministic predicate over doc values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocValueFilter {
    /// Matches every candidate.
    MatchAll,
    /// Matches candidates containing the exact field.
    Exists(String),
    /// Compares an existing field with one same-type literal.
    Compare {
        /// Exact field name.
        field: String,
        /// Comparison operation.
        operator: DocValueOperator,
        /// Literal value.
        value: DocValue,
    },
    /// Every child must match. An empty list matches.
    All(Vec<Self>),
    /// At least one child must match. An empty list does not match.
    Any(Vec<Self>),
    /// Two-valued negation.
    Not(Box<Self>),
    /// Matches when the field equals any member of a bounded same-type set.
    /// An empty set does not match. Members must share one scalar type.
    In {
        /// Exact field name.
        field: String,
        /// Bounded literal members, compared with exact equality.
        values: Vec<DocValue>,
    },
    /// Matches candidates missing the exact field entirely.
    IsNull(String),
    /// Matches when a string field contains the literal pattern with `_`
    /// matching exactly one character and `%` matching any run, anchored at
    /// both ends. Patterns are bounded and contain no escape syntax.
    Like {
        /// Exact field name.
        field: String,
        /// Bounded literal pattern over `_`, `%`, and plain characters.
        pattern: String,
    },
}

/// Maximum members admitted by one [`DocValueFilter::In`] set.
pub const MAX_DOC_VALUE_IN_MEMBERS: usize = 256;
/// Maximum UTF-8 bytes admitted by one [`DocValueFilter::Like`] pattern.
pub const MAX_DOC_VALUE_LIKE_PATTERN_BYTES: usize = 256;

/// Deterministic anchored `LIKE` match over characters: `_` matches exactly
/// one character, `%` matches any run (including empty), everything else
/// matches itself. Iterative two-pointer algorithm — no recursion, linear
/// backtracking bounded by the input lengths.
#[must_use]
pub fn like_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0_usize, 0_usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '_' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '%' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '%' {
        p += 1;
    }
    p == pattern.len()
}

/// Sort direction for scores or doc values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocValueSortDirection {
    /// Natural ascending order.
    Ascending,
    /// Reverse order.
    Descending,
}

/// Explicit placement of a missing doc value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingPlacement {
    /// Missing values precede present values.
    First,
    /// Missing values follow present values.
    Last,
}

/// Sort source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocValueSortSource {
    /// Native lexical or vector score.
    Score,
    /// Exact doc-value field.
    Field(String),
}

/// One deterministic sort component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocValueSort {
    /// Value supplying this component.
    pub source: DocValueSortSource,
    /// Component direction.
    pub direction: DocValueSortDirection,
    /// Placement used only when a field is missing.
    pub missing: MissingPlacement,
}

/// One bounded terms-facet request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetRequest {
    /// Exact doc-value field.
    pub field: String,
    /// Maximum terms returned after complete counting.
    pub limit: usize,
}

/// One aggregate calculation over the complete filtered set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocValueAggregation {
    /// Count every filtered candidate.
    Count,
    /// Checked signed integer sum, ignoring missing fields.
    Sum(String),
    /// Minimum present scalar under the canonical total order.
    Min(String),
    /// Maximum present scalar under the canonical total order.
    Max(String),
    /// Arithmetic mean of present numeric scalars as a canonical float.
    Average(String),
}

/// One named aggregate calculation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedDocValueAggregation {
    /// Unique nonempty result name.
    pub name: String,
    /// Calculation.
    pub aggregation: DocValueAggregation,
}

/// Complete bounded doc-values request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocValueRequest {
    /// Predicate applied before sorting, facets, and aggregations.
    pub filter: DocValueFilter,
    /// Explicit sort. Empty means score descending.
    pub sort: Vec<DocValueSort>,
    /// Maximum returned hits.
    pub limit: usize,
    /// Terms facets evaluated over every filtered candidate.
    pub facets: Vec<FacetRequest>,
    /// Aggregations evaluated over every filtered candidate.
    pub aggregations: Vec<NamedDocValueAggregation>,
}

/// Explicit shape and memory limits for one execution.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct DocValueLimits {
    /// Maximum input candidates scanned.
    pub max_candidates: usize,
    /// Maximum filtered candidates retained for global sort.
    pub max_matches: usize,
    /// Maximum returned hits.
    pub max_hits: usize,
    /// Maximum recursive filter nodes.
    pub max_filter_nodes: usize,
    /// Maximum recursive filter depth, counting the root.
    pub max_filter_depth: usize,
    /// Maximum sort components.
    pub max_sorts: usize,
    /// Maximum facet requests.
    pub max_facets: usize,
    /// Maximum aggregate metrics.
    pub max_aggregations: usize,
    /// Maximum distinct values retained by each facet.
    pub max_facet_terms: usize,
    /// Maximum fields on one candidate.
    pub max_values_per_candidate: usize,
    /// Maximum bytes in one field name or string/binary scalar.
    pub max_value_bytes: usize,
}

impl Default for DocValueLimits {
    fn default() -> Self {
        Self {
            max_candidates: MAX_DOC_VALUE_CANDIDATES,
            max_matches: MAX_DOC_VALUE_MATCHES,
            max_hits: MAX_DOC_VALUE_HITS,
            max_filter_nodes: MAX_DOC_VALUE_FILTER_NODES,
            max_filter_depth: MAX_DOC_VALUE_FILTER_DEPTH,
            max_sorts: MAX_DOC_VALUE_SORTS,
            max_facets: MAX_DOC_VALUE_FACETS,
            max_aggregations: MAX_DOC_VALUE_AGGREGATIONS,
            max_facet_terms: MAX_DOC_VALUE_FACET_TERMS,
            max_values_per_candidate: MAX_DOC_VALUES_PER_CANDIDATE,
            max_value_bytes: MAX_DOC_VALUE_BYTES,
        }
    }
}

/// One facet term and its complete filtered-set count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetBucket {
    /// Canonical scalar term.
    pub value: DocValue,
    /// Number of filtered candidates containing that exact term.
    pub count: u64,
}

/// Terms-facet output in count-descending, value-ascending order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetResult {
    /// Source field.
    pub field: String,
    /// Bounded leading buckets.
    pub buckets: Vec<FacetBucket>,
}

/// Value emitted by one aggregate metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocValueAggregationValue {
    /// Complete filtered count.
    Count(u64),
    /// Checked sum; `None` means no present inputs.
    Integer(Option<i128>),
    /// Finite-guarded float sum; `None` means no present inputs.
    Float(Option<CanonicalF64>),
    /// Minimum or maximum; `None` means no present inputs.
    Value(Option<DocValue>),
}

/// One named aggregate output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedDocValueAggregationValue {
    /// Request name.
    pub name: String,
    /// Calculated value.
    pub value: DocValueAggregationValue,
}

/// Complete successful bounded execution.
#[derive(Clone, Debug, PartialEq)]
pub struct DocValueResult {
    /// Globally sorted and limited candidates.
    pub hits: Vec<DocValueCandidate>,
    /// Facets in request order.
    pub facets: Vec<FacetResult>,
    /// Aggregations in request order.
    pub aggregations: Vec<NamedDocValueAggregationValue>,
    /// Number of candidates inspected.
    pub scanned_candidates: usize,
    /// Number matching before the result limit.
    pub matched_candidates: usize,
}

/// Validation or complete-execution failure for the doc-values surface.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DocValueError {
    /// A returned page must be nonempty and policy-bounded.
    #[error("doc-values hit limit {requested} is outside 1..={maximum}")]
    InvalidHitLimit {
        /// Rejected requested hit count.
        requested: usize,
        /// Configured maximum hit count.
        maximum: usize,
    },
    /// Input exceeds the candidate scan budget.
    #[error("doc-values candidate count {actual} exceeds {maximum}")]
    CandidateLimit {
        /// Supplied candidate count.
        actual: usize,
        /// Configured candidate maximum.
        maximum: usize,
    },
    /// Filtered retention exceeds policy.
    #[error("doc-values matched count exceeds {maximum}")]
    MatchLimit {
        /// Configured matched-candidate maximum.
        maximum: usize,
    },
    /// Request shape exceeds one explicit limit.
    #[error("doc-values {kind} count {actual} exceeds {maximum}")]
    ShapeLimit {
        /// Shape component that exceeded policy.
        kind: &'static str,
        /// Observed component count.
        actual: usize,
        /// Configured component maximum.
        maximum: usize,
    },
    /// A document identity is empty.
    #[error("doc-values candidate has an empty document identity")]
    EmptyDocumentId,
    /// A document identity occurs more than once.
    #[error("doc-values candidate identity is duplicated")]
    DuplicateDocumentId,
    /// A score is negative, infinite, or NaN.
    #[error("doc-values candidate score is noncanonical")]
    NoncanonicalScore,
    /// A field or metric name is empty.
    #[error("doc-values field or metric name is empty")]
    EmptyName,
    /// A field name or scalar exceeds its byte limit.
    #[error("doc-values scalar or field exceeds {maximum} bytes")]
    ValueTooLarge {
        /// Configured byte maximum.
        maximum: usize,
    },
    /// A facet has an invalid output limit.
    #[error("doc-values facet limit {requested} is outside 1..={maximum}")]
    InvalidFacetLimit {
        /// Rejected requested bucket count.
        requested: usize,
        /// Configured bucket maximum.
        maximum: usize,
    },
    /// Facet memory would exceed its distinct-term budget.
    #[error("doc-values facet for {field} exceeds {maximum} distinct terms")]
    FacetTermLimit {
        /// Faceted field that exceeded policy.
        field: String,
        /// Configured distinct-term maximum.
        maximum: usize,
    },
    /// Aggregate names must be unique.
    #[error("doc-values aggregate name is duplicated: {0}")]
    DuplicateAggregationName(String),
    /// Integer sum encountered a present non-integer scalar.
    #[error("doc-values sum {name} encountered a non-integer scalar")]
    AggregationType {
        /// Aggregate result name.
        name: String,
    },
    /// Checked integer aggregation overflowed.
    #[error("doc-values sum {name} overflowed")]
    AggregationOverflow {
        /// Aggregate result name.
        name: String,
    },
}

/// Executes filtering, global sorting, facets, and aggregations without partial results.
///
/// # Errors
///
/// Returns a typed validation, shape, memory, type, or arithmetic error before
/// publishing any result.
pub fn execute_doc_values(
    candidates: &[DocValueCandidate],
    request: &DocValueRequest,
    limits: &DocValueLimits,
) -> Result<DocValueResult, DocValueError> {
    validate_request(request, limits)?;
    if candidates.len() > limits.max_candidates {
        return Err(DocValueError::CandidateLimit {
            actual: candidates.len(),
            maximum: limits.max_candidates,
        });
    }
    let mut identities = BTreeSet::new();
    for candidate in candidates {
        validate_candidate(candidate, limits)?;
        if !identities.insert(candidate.document_id.as_slice()) {
            return Err(DocValueError::DuplicateDocumentId);
        }
    }

    let mut matched = Vec::new();
    for candidate in candidates {
        if filter_matches(&request.filter, candidate) {
            if matched.len() == limits.max_matches {
                return Err(DocValueError::MatchLimit {
                    maximum: limits.max_matches,
                });
            }
            matched.push(candidate);
        }
    }

    let facets = evaluate_facets(&matched, &request.facets, limits.max_facet_terms)?;
    let aggregations = evaluate_aggregations(&matched, &request.aggregations)?;
    matched.sort_by(|left, right| compare_candidates(left, right, &request.sort));
    let matched_candidates = matched.len();
    let hits = matched.into_iter().take(request.limit).cloned().collect();
    Ok(DocValueResult {
        hits,
        facets,
        aggregations,
        scanned_candidates: candidates.len(),
        matched_candidates,
    })
}

fn validate_request(
    request: &DocValueRequest,
    limits: &DocValueLimits,
) -> Result<(), DocValueError> {
    if request.limit == 0 || request.limit > limits.max_hits {
        return Err(DocValueError::InvalidHitLimit {
            requested: request.limit,
            maximum: limits.max_hits,
        });
    }
    check_shape("sort", request.sort.len(), limits.max_sorts)?;
    check_shape("facet", request.facets.len(), limits.max_facets)?;
    check_shape(
        "aggregation",
        request.aggregations.len(),
        limits.max_aggregations,
    )?;
    let (nodes, depth) = filter_shape(&request.filter);
    check_shape("filter node", nodes, limits.max_filter_nodes)?;
    check_shape("filter depth", depth, limits.max_filter_depth)?;
    validate_filter_names(&request.filter, limits.max_value_bytes)?;
    for sort in &request.sort {
        if let DocValueSortSource::Field(field) = &sort.source {
            validate_name(field, limits.max_value_bytes)?;
        }
    }
    let mut facet_fields = BTreeSet::new();
    for facet in &request.facets {
        validate_name(&facet.field, limits.max_value_bytes)?;
        if facet.limit == 0 || facet.limit > limits.max_facet_terms {
            return Err(DocValueError::InvalidFacetLimit {
                requested: facet.limit,
                maximum: limits.max_facet_terms,
            });
        }
        if !facet_fields.insert(facet.field.as_str()) {
            return Err(DocValueError::ShapeLimit {
                kind: "duplicate facet",
                actual: 2,
                maximum: 1,
            });
        }
    }
    let mut names = BTreeSet::new();
    for aggregation in &request.aggregations {
        validate_name(&aggregation.name, limits.max_value_bytes)?;
        if !names.insert(aggregation.name.as_str()) {
            return Err(DocValueError::DuplicateAggregationName(
                aggregation.name.clone(),
            ));
        }
        if let DocValueAggregation::Sum(field)
        | DocValueAggregation::Min(field)
        | DocValueAggregation::Max(field)
        | DocValueAggregation::Average(field) = &aggregation.aggregation
        {
            validate_name(field, limits.max_value_bytes)?;
        }
    }
    Ok(())
}

fn validate_candidate(
    candidate: &DocValueCandidate,
    limits: &DocValueLimits,
) -> Result<(), DocValueError> {
    if candidate.document_id.is_empty() {
        return Err(DocValueError::EmptyDocumentId);
    }
    if !candidate.score.is_finite() || candidate.score < 0.0 {
        return Err(DocValueError::NoncanonicalScore);
    }
    check_shape(
        "fields per candidate",
        candidate.values.len(),
        limits.max_values_per_candidate,
    )?;
    for (field, value) in &candidate.values {
        validate_name(field, limits.max_value_bytes)?;
        let length = match value {
            DocValue::String(value) => value.len(),
            DocValue::Bytes(value) => value.len(),
            DocValue::Boolean(_) | DocValue::Integer(_) | DocValue::Float(_) => 0,
        };
        if length > limits.max_value_bytes {
            return Err(DocValueError::ValueTooLarge {
                maximum: limits.max_value_bytes,
            });
        }
    }
    Ok(())
}

fn check_shape(kind: &'static str, actual: usize, maximum: usize) -> Result<(), DocValueError> {
    if actual > maximum {
        Err(DocValueError::ShapeLimit {
            kind,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_name(name: &str, maximum: usize) -> Result<(), DocValueError> {
    if name.is_empty() {
        Err(DocValueError::EmptyName)
    } else if name.len() > maximum {
        Err(DocValueError::ValueTooLarge { maximum })
    } else {
        Ok(())
    }
}

fn filter_shape(filter: &DocValueFilter) -> (usize, usize) {
    match filter {
        DocValueFilter::MatchAll
        | DocValueFilter::Exists(_)
        | DocValueFilter::Compare { .. }
        | DocValueFilter::In { .. }
        | DocValueFilter::IsNull(_)
        | DocValueFilter::Like { .. } => (1, 1),
        DocValueFilter::Not(child) => {
            let (nodes, depth) = filter_shape(child);
            (nodes.saturating_add(1), depth.saturating_add(1))
        }
        DocValueFilter::All(children) | DocValueFilter::Any(children) => {
            let mut nodes = 1_usize;
            let mut depth = 1_usize;
            for child in children {
                let (child_nodes, child_depth) = filter_shape(child);
                nodes = nodes.saturating_add(child_nodes);
                depth = depth.max(child_depth.saturating_add(1));
            }
            (nodes, depth)
        }
    }
}

fn validate_filter_names(filter: &DocValueFilter, maximum: usize) -> Result<(), DocValueError> {
    match filter {
        DocValueFilter::MatchAll => Ok(()),
        DocValueFilter::Exists(field) | DocValueFilter::IsNull(field) => {
            validate_name(field, maximum)
        }
        DocValueFilter::Compare { field, value, .. } => {
            validate_name(field, maximum)?;
            match value {
                DocValue::String(value) if value.len() > maximum => {
                    Err(DocValueError::ValueTooLarge { maximum })
                }
                DocValue::Bytes(value) if value.len() > maximum => {
                    Err(DocValueError::ValueTooLarge { maximum })
                }
                _ => Ok(()),
            }
        }
        DocValueFilter::All(children) | DocValueFilter::Any(children) => {
            for child in children {
                validate_filter_names(child, maximum)?;
            }
            Ok(())
        }
        DocValueFilter::Not(child) => validate_filter_names(child, maximum),
        DocValueFilter::In { field, values } => {
            validate_name(field, maximum)?;
            if values.is_empty() || values.len() > MAX_DOC_VALUE_IN_MEMBERS {
                return Err(DocValueError::ValueTooLarge {
                    maximum: MAX_DOC_VALUE_IN_MEMBERS,
                });
            }
            let first = std::mem::discriminant(&values[0]);
            for value in values {
                if std::mem::discriminant(value) != first {
                    return Err(DocValueError::ValueTooLarge { maximum });
                }
                match value {
                    DocValue::String(value) if value.len() > maximum => {
                        return Err(DocValueError::ValueTooLarge { maximum });
                    }
                    DocValue::Bytes(value) if value.len() > maximum => {
                        return Err(DocValueError::ValueTooLarge { maximum });
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        DocValueFilter::Like { field, pattern } => {
            validate_name(field, maximum)?;
            if pattern.is_empty() || pattern.len() > MAX_DOC_VALUE_LIKE_PATTERN_BYTES {
                return Err(DocValueError::ValueTooLarge {
                    maximum: MAX_DOC_VALUE_LIKE_PATTERN_BYTES,
                });
            }
            Ok(())
        }
    }
}

fn filter_matches(filter: &DocValueFilter, candidate: &DocValueCandidate) -> bool {
    match filter {
        DocValueFilter::MatchAll => true,
        DocValueFilter::Exists(field) => candidate.values.contains_key(field),
        DocValueFilter::Compare {
            field,
            operator,
            value,
        } => candidate.values.get(field).is_some_and(|candidate_value| {
            let comparable =
                std::mem::discriminant(candidate_value) == std::mem::discriminant(value);
            comparable
                && match operator {
                    DocValueOperator::Equal => candidate_value == value,
                    DocValueOperator::NotEqual => candidate_value != value,
                    DocValueOperator::Less => candidate_value < value,
                    DocValueOperator::LessOrEqual => candidate_value <= value,
                    DocValueOperator::Greater => candidate_value > value,
                    DocValueOperator::GreaterOrEqual => candidate_value >= value,
                }
        }),
        DocValueFilter::All(children) => children
            .iter()
            .all(|child| filter_matches(child, candidate)),
        DocValueFilter::Any(children) => children
            .iter()
            .any(|child| filter_matches(child, candidate)),
        DocValueFilter::Not(child) => !filter_matches(child, candidate),
        DocValueFilter::In { field, values } => {
            candidate.values.get(field).is_some_and(|candidate_value| {
                values.iter().any(|value| {
                    std::mem::discriminant(candidate_value) == std::mem::discriminant(value)
                        && candidate_value == value
                })
            })
        }
        DocValueFilter::IsNull(field) => !candidate.values.contains_key(field),
        DocValueFilter::Like { field, pattern } => {
            candidate.values.get(field).is_some_and(|candidate_value| {
                if let DocValue::String(text) = candidate_value {
                    like_matches(pattern, text)
                } else {
                    false
                }
            })
        }
    }
}

fn compare_candidates(
    left: &DocValueCandidate,
    right: &DocValueCandidate,
    sorts: &[DocValueSort],
) -> Ordering {
    if sorts.is_empty() {
        return right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.document_id.cmp(&right.document_id));
    }
    for sort in sorts {
        let ordering = match &sort.source {
            DocValueSortSource::Score => left.score.total_cmp(&right.score),
            DocValueSortSource::Field(field) => compare_optional_values(
                left.values.get(field),
                right.values.get(field),
                sort.missing,
                sort.direction,
            ),
        };
        let ordering = match (&sort.source, sort.direction) {
            (DocValueSortSource::Score, DocValueSortDirection::Descending) => ordering.reverse(),
            _ => ordering,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.document_id.cmp(&right.document_id)
}

fn compare_optional_values(
    left: Option<&DocValue>,
    right: Option<&DocValue>,
    missing: MissingPlacement,
    direction: DocValueSortDirection,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            DocValueSortDirection::Ascending => left.cmp(right),
            DocValueSortDirection::Descending => right.cmp(left),
        },
        (None, None) => Ordering::Equal,
        (None, Some(_)) => match missing {
            MissingPlacement::First => Ordering::Less,
            MissingPlacement::Last => Ordering::Greater,
        },
        (Some(_), None) => match missing {
            MissingPlacement::First => Ordering::Greater,
            MissingPlacement::Last => Ordering::Less,
        },
    }
}

/// Sums one field across matched candidates: integers under checked
/// 128-bit accumulation, floats under finite-guarded binary64
/// accumulation. Mixed types or a non-finite float sum fail closed.
fn evaluate_sum(
    matched: &[&DocValueCandidate],
    field: &str,
    name: &str,
) -> Result<DocValueAggregationValue, DocValueError> {
    let mut integer_sum = None::<i128>;
    let mut float_sum = None::<f64>;
    for candidate in matched {
        let Some(value) = candidate.values.get(field) else {
            continue;
        };
        match value {
            DocValue::Integer(value) if float_sum.is_none() => {
                integer_sum = Some(
                    integer_sum
                        .unwrap_or_default()
                        .checked_add(i128::from(*value))
                        .ok_or(DocValueError::AggregationOverflow {
                            name: name.to_owned(),
                        })?,
                );
            }
            DocValue::Float(value) if integer_sum.is_none() => {
                let updated = float_sum.unwrap_or_default() + value.get();
                if !updated.is_finite() {
                    return Err(DocValueError::AggregationOverflow {
                        name: name.to_owned(),
                    });
                }
                float_sum = Some(updated);
            }
            _ => {
                return Err(DocValueError::AggregationType {
                    name: name.to_owned(),
                });
            }
        }
    }
    Ok(match float_sum {
        Some(sum) => DocValueAggregationValue::Float(Some(CanonicalF64::new(sum))),
        None => DocValueAggregationValue::Integer(integer_sum),
    })
}

/// Arithmetic mean of present numeric scalars: the checked sum divided
/// by the present-value count, always as a canonical float. No present
/// values yield an absent aggregate; a non-finite mean fails closed.
fn evaluate_average(
    matched: &[&DocValueCandidate],
    field: &str,
    name: &str,
) -> Result<DocValueAggregationValue, DocValueError> {
    let count = matched
        .iter()
        .filter(|candidate| candidate.values.contains_key(field))
        .count();
    let Ok(count) = u32::try_from(count) else {
        return Err(DocValueError::AggregationOverflow {
            name: name.to_owned(),
        });
    };
    if count == 0 {
        return Ok(DocValueAggregationValue::Float(None));
    }
    let sum = match evaluate_sum(matched, field, name)? {
        DocValueAggregationValue::Integer(Some(sum)) => integer_sum_as_f64(sum),
        DocValueAggregationValue::Float(Some(sum)) => sum.get(),
        _ => return Ok(DocValueAggregationValue::Float(None)),
    };
    let mean = sum / f64::from(count);
    if !mean.is_finite() {
        return Err(DocValueError::AggregationOverflow {
            name: name.to_owned(),
        });
    }
    Ok(DocValueAggregationValue::Float(Some(CanonicalF64::new(
        mean,
    ))))
}

/// Deterministic i128 -> f64 conversion via two u64 halves (IEEE
/// round-to-nearest on canonical inputs; avoids the lossy direct cast).
fn integer_sum_as_f64(sum: i128) -> f64 {
    let negative = sum < 0;
    let magnitude = sum.unsigned_abs();
    let high = u64::try_from(magnitude >> 64).unwrap_or(u64::MAX);
    let low = u64::try_from(magnitude & u128::from(u64::MAX)).unwrap_or(u64::MAX);
    let mut value = high_to_f64(high) * 18_446_744_073_709_551_616.0 + high_to_f64(low);
    if negative {
        value = -value;
    }
    value
}

/// Lossless-enough u64 -> f64 through two u32 halves.
fn high_to_f64(value: u64) -> f64 {
    let upper = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let lower = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(upper) * 4_294_967_296.0 + f64::from(lower)
}

fn evaluate_facets(
    matched: &[&DocValueCandidate],
    requests: &[FacetRequest],
    maximum_terms: usize,
) -> Result<Vec<FacetResult>, DocValueError> {
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let mut counts = BTreeMap::<DocValue, u64>::new();
        for candidate in matched {
            let Some(value) = candidate.values.get(&request.field) else {
                continue;
            };
            if !counts.contains_key(value) && counts.len() == maximum_terms {
                return Err(DocValueError::FacetTermLimit {
                    field: request.field.clone(),
                    maximum: maximum_terms,
                });
            }
            let count = counts.entry(value.clone()).or_default();
            *count = count.checked_add(1).ok_or(DocValueError::ShapeLimit {
                kind: "facet count",
                actual: usize::MAX,
                maximum: usize::MAX - 1,
            })?;
        }
        let mut buckets: Vec<_> = counts
            .into_iter()
            .map(|(value, count)| FacetBucket { value, count })
            .collect();
        buckets.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.value.cmp(&right.value))
        });
        buckets.truncate(request.limit);
        results.push(FacetResult {
            field: request.field.clone(),
            buckets,
        });
    }
    Ok(results)
}

fn evaluate_aggregations(
    matched: &[&DocValueCandidate],
    requests: &[NamedDocValueAggregation],
) -> Result<Vec<NamedDocValueAggregationValue>, DocValueError> {
    requests
        .iter()
        .map(|request| {
            let value =
                match &request.aggregation {
                    DocValueAggregation::Count => DocValueAggregationValue::Count(
                        u64::try_from(matched.len()).map_err(|_| DocValueError::ShapeLimit {
                            kind: "aggregation count",
                            actual: matched.len(),
                            maximum: usize::MAX,
                        })?,
                    ),
                    DocValueAggregation::Sum(field) => evaluate_sum(matched, field, &request.name)?,
                    DocValueAggregation::Average(field) => {
                        evaluate_average(matched, field, &request.name)?
                    }
                    DocValueAggregation::Min(field) => DocValueAggregationValue::Value(
                        matched
                            .iter()
                            .filter_map(|candidate| candidate.values.get(field))
                            .min()
                            .cloned(),
                    ),
                    DocValueAggregation::Max(field) => DocValueAggregationValue::Value(
                        matched
                            .iter()
                            .filter_map(|candidate| candidate.values.get(field))
                            .max()
                            .cloned(),
                    ),
                };
            Ok(NamedDocValueAggregationValue {
                name: request.name.clone(),
                value,
            })
        })
        .collect()
}
