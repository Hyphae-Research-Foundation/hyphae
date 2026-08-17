// SPDX-License-Identifier: Apache-2.0

//! Bounded compound lexical matching over the current native source-text model.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::{MatchHit, model::analyze};

/// Maximum fuzzy edit distance accepted by the bounded embedded surface.
pub const MAX_BOUNDED_SEARCH_EDIT_DISTANCE: u8 = 2;
/// Maximum nesting depth accepted by one compound query.
pub const MAX_BOUNDED_SEARCH_DEPTH: usize = 8;

/// One compound lexical expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundedSearchQuery {
    /// One analyzer-normalized exact term.
    Term(String),
    /// Contiguous analyzer-token sequence.
    Phrase(String),
    /// One analyzer-normalized term prefix.
    Prefix(String),
    /// One analyzer-normalized term within a bounded Levenshtein distance.
    Fuzzy {
        /// Query term.
        term: String,
        /// Inclusive edit-distance bound, at most two.
        max_distance: u8,
    },
    /// Flat boolean composition. See [`BoundedSearchQuery::boolean`].
    Boolean {
        /// Clauses that must all match.
        must: Vec<Self>,
        /// Clauses that contribute to ranking; one is required when `must` is empty.
        should: Vec<Self>,
        /// Clauses that exclude a document.
        must_not: Vec<Self>,
    },
}

impl BoundedSearchQuery {
    /// Constructs a boolean query with explicit clause groups.
    pub fn boolean(must: Vec<Self>, should: Vec<Self>, must_not: Vec<Self>) -> Self {
        Self::Boolean {
            must,
            should,
            must_not,
        }
    }
}

/// Complete work and memory limits for one embedded compound search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct BoundedSearchLimits {
    /// Maximum result count requested and retained.
    pub max_hits: usize,
    /// Maximum source documents examined.
    pub max_documents: usize,
    /// Maximum matching documents retained before deterministic ranking.
    pub max_matches: usize,
    /// Maximum aggregate UTF-8 source bytes analyzed.
    pub max_source_bytes: usize,
    /// Maximum aggregate token visits across all clauses.
    pub max_token_visits: usize,
    /// Maximum exact/prefix/phrase token comparisons.
    pub max_token_comparisons: usize,
    /// Maximum dynamic-programming cells evaluated by fuzzy clauses.
    pub max_fuzzy_steps: usize,
    /// Maximum total clauses in the expression tree.
    pub max_clauses: usize,
    /// Maximum aggregate UTF-8 bytes in leaf expressions.
    pub max_query_bytes: usize,
}

impl Default for BoundedSearchLimits {
    fn default() -> Self {
        Self {
            max_hits: 100,
            max_documents: 10_000,
            max_matches: 10_000,
            max_source_bytes: 64 * 1024 * 1024,
            max_token_visits: 1_000_000,
            max_token_comparisons: 1_000_000,
            max_fuzzy_steps: 2_000_000,
            max_clauses: 64,
            max_query_bytes: 4_096,
        }
    }
}

/// Auditable work counters and canonically ranked hits.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundedSearchResults {
    /// Hits ordered by descending score, then ascending binary document ID.
    pub hits: Vec<MatchHit>,
    /// Source documents examined.
    pub documents_examined: usize,
    /// Aggregate source bytes analyzed.
    pub source_bytes: usize,
    /// Aggregate clause-level token visits.
    pub token_visits: usize,
    /// Aggregate exact/prefix/phrase comparisons.
    pub token_comparisons: usize,
    /// Aggregate fuzzy dynamic-programming cells evaluated.
    pub fuzzy_steps: usize,
}

/// Validation or global work-budget failure. No partial result is returned.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BoundedSearchError {
    /// The requested collection does not exist in the snapshot.
    #[error("native bounded search collection does not exist")]
    UnknownIndex,
    /// A limit or requested result count is zero.
    #[error("native bounded search limits must be nonzero")]
    ZeroLimit,
    /// The expression has no positive clause or contains an empty boolean node.
    #[error("native bounded search query has no positive match expression")]
    NoPositiveClause,
    /// A term, prefix, or fuzzy expression does not analyze to exactly one token.
    #[error("native bounded search single-term expression is invalid")]
    InvalidSingleTerm,
    /// A phrase analyzes to no tokens.
    #[error("native bounded search phrase is empty")]
    EmptyPhrase,
    /// A fuzzy distance is above the supported bound.
    #[error("native bounded search fuzzy distance {0} exceeds the supported bound")]
    InvalidFuzzyDistance(u8),
    /// Query nesting exceeds the configured canonical depth.
    #[error("native bounded search query depth exceeds {MAX_BOUNDED_SEARCH_DEPTH}")]
    QueryTooDeep,
    /// Clause count exceeds the caller's budget.
    #[error("native bounded search clause budget exceeded: {maximum}")]
    ClauseBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Aggregate query bytes exceed the caller's budget.
    #[error("native bounded search query-byte budget exceeded: {maximum}")]
    QueryByteBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Source document visits exceed the caller's budget.
    #[error("native bounded search document budget exceeded: {maximum}")]
    DocumentBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Matching-document retention exceeds the caller's budget.
    #[error("native bounded search match budget exceeded: {maximum}")]
    MatchBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Aggregate source bytes exceed the caller's budget.
    #[error("native bounded search source-byte budget exceeded: {maximum}")]
    SourceByteBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Aggregate token visits exceed the caller's budget.
    #[error("native bounded search token-visit budget exceeded: {maximum}")]
    TokenVisitBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Aggregate non-fuzzy comparisons exceed the caller's budget.
    #[error("native bounded search comparison budget exceeded: {maximum}")]
    ComparisonBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// Aggregate fuzzy distance work exceeds the caller's budget.
    #[error("native bounded search fuzzy-step budget exceeded: {maximum}")]
    FuzzyStepBudgetExceeded {
        /// Configured maximum.
        maximum: usize,
    },
    /// A caller-owned cooperative checkpoint interrupted execution.
    #[error("native bounded search was interrupted")]
    ExecutionInterrupted,
}

#[derive(Clone, Debug)]
enum CompiledQuery {
    Term(String),
    Phrase(Vec<String>),
    Prefix(String),
    Fuzzy {
        term: Vec<char>,
        max_distance: usize,
    },
    Boolean {
        must: Vec<Self>,
        should: Vec<Self>,
        must_not: Vec<Self>,
    },
}

#[derive(Default)]
struct Work {
    documents: usize,
    source_bytes: usize,
    token_visits: usize,
    comparisons: usize,
    fuzzy_steps: usize,
}

pub(crate) fn search_documents(
    documents: &BTreeMap<Vec<u8>, String>,
    query: &BoundedSearchQuery,
    limit: usize,
    limits: BoundedSearchLimits,
) -> Result<BoundedSearchResults, BoundedSearchError> {
    search_documents_with_checkpoint(documents, query, limit, limits, &mut || true)
}

pub(crate) fn search_documents_with_checkpoint(
    documents: &BTreeMap<Vec<u8>, String>,
    query: &BoundedSearchQuery,
    limit: usize,
    limits: BoundedSearchLimits,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<BoundedSearchResults, BoundedSearchError> {
    validate_limits(limit, limits)?;
    let compiled = compile(query, limits)?;
    let mut work = Work::default();
    let mut hits = Vec::new();
    for (document_id, source) in documents {
        execution_checkpoint(checkpoint)?;
        add_bounded(&mut work.documents, 1, limits.max_documents).map_err(|()| {
            BoundedSearchError::DocumentBudgetExceeded {
                maximum: limits.max_documents,
            }
        })?;
        add_bounded(
            &mut work.source_bytes,
            source.len(),
            limits.max_source_bytes,
        )
        .map_err(|()| BoundedSearchError::SourceByteBudgetExceeded {
            maximum: limits.max_source_bytes,
        })?;
        let tokens = analyze(source);
        if let Some(score) = evaluate(&compiled, &tokens, &mut work, limits, checkpoint)? {
            if hits.len() == limits.max_matches {
                return Err(BoundedSearchError::MatchBudgetExceeded {
                    maximum: limits.max_matches,
                });
            }
            hits.push(MatchHit {
                document_id: document_id.clone(),
                score: f64::from(score),
            });
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.document_id.cmp(&right.document_id))
    });
    hits.truncate(limit);
    Ok(BoundedSearchResults {
        hits,
        documents_examined: work.documents,
        source_bytes: work.source_bytes,
        token_visits: work.token_visits,
        token_comparisons: work.comparisons,
        fuzzy_steps: work.fuzzy_steps,
    })
}

fn validate_limits(limit: usize, limits: BoundedSearchLimits) -> Result<(), BoundedSearchError> {
    if limit == 0
        || limit > limits.max_hits
        || limits.max_hits == 0
        || limits.max_documents == 0
        || limits.max_matches == 0
        || limits.max_source_bytes == 0
        || limits.max_token_visits == 0
        || limits.max_token_comparisons == 0
        || limits.max_fuzzy_steps == 0
        || limits.max_clauses == 0
        || limits.max_query_bytes == 0
    {
        return Err(BoundedSearchError::ZeroLimit);
    }
    Ok(())
}

fn compile(
    query: &BoundedSearchQuery,
    limits: BoundedSearchLimits,
) -> Result<CompiledQuery, BoundedSearchError> {
    let mut clauses = 0_usize;
    let mut bytes = 0_usize;
    compile_at(query, 1, &mut clauses, &mut bytes, limits)
}

fn compile_at(
    query: &BoundedSearchQuery,
    depth: usize,
    clauses: &mut usize,
    bytes: &mut usize,
    limits: BoundedSearchLimits,
) -> Result<CompiledQuery, BoundedSearchError> {
    if depth > MAX_BOUNDED_SEARCH_DEPTH {
        return Err(BoundedSearchError::QueryTooDeep);
    }
    add_bounded(clauses, 1, limits.max_clauses).map_err(|()| {
        BoundedSearchError::ClauseBudgetExceeded {
            maximum: limits.max_clauses,
        }
    })?;
    match query {
        BoundedSearchQuery::Term(value) => {
            account_query_bytes(bytes, value, limits)?;
            Ok(CompiledQuery::Term(single_token(value)?))
        }
        BoundedSearchQuery::Phrase(value) => {
            account_query_bytes(bytes, value, limits)?;
            let tokens = analyze(value);
            if tokens.is_empty() {
                Err(BoundedSearchError::EmptyPhrase)
            } else {
                Ok(CompiledQuery::Phrase(tokens))
            }
        }
        BoundedSearchQuery::Prefix(value) => {
            account_query_bytes(bytes, value, limits)?;
            Ok(CompiledQuery::Prefix(single_token(value)?))
        }
        BoundedSearchQuery::Fuzzy { term, max_distance } => {
            if *max_distance > MAX_BOUNDED_SEARCH_EDIT_DISTANCE {
                return Err(BoundedSearchError::InvalidFuzzyDistance(*max_distance));
            }
            account_query_bytes(bytes, term, limits)?;
            Ok(CompiledQuery::Fuzzy {
                term: single_token(term)?.chars().collect(),
                max_distance: usize::from(*max_distance),
            })
        }
        BoundedSearchQuery::Boolean {
            must,
            should,
            must_not,
        } => {
            if must.is_empty() && should.is_empty() {
                return Err(BoundedSearchError::NoPositiveClause);
            }
            Ok(CompiledQuery::Boolean {
                must: compile_all(must, depth, clauses, bytes, limits)?,
                should: compile_all(should, depth, clauses, bytes, limits)?,
                must_not: compile_all(must_not, depth, clauses, bytes, limits)?,
            })
        }
    }
}

fn compile_all(
    queries: &[BoundedSearchQuery],
    depth: usize,
    clauses: &mut usize,
    bytes: &mut usize,
    limits: BoundedSearchLimits,
) -> Result<Vec<CompiledQuery>, BoundedSearchError> {
    queries
        .iter()
        .map(|query| compile_at(query, depth + 1, clauses, bytes, limits))
        .collect()
}

fn single_token(value: &str) -> Result<String, BoundedSearchError> {
    let mut tokens = analyze(value).into_iter();
    let token = tokens.next().ok_or(BoundedSearchError::InvalidSingleTerm)?;
    if tokens.next().is_some() {
        Err(BoundedSearchError::InvalidSingleTerm)
    } else {
        Ok(token)
    }
}

fn account_query_bytes(
    used: &mut usize,
    value: &str,
    limits: BoundedSearchLimits,
) -> Result<(), BoundedSearchError> {
    add_bounded(used, value.len(), limits.max_query_bytes).map_err(|()| {
        BoundedSearchError::QueryByteBudgetExceeded {
            maximum: limits.max_query_bytes,
        }
    })
}

#[allow(clippy::too_many_lines)]
fn evaluate(
    query: &CompiledQuery,
    tokens: &[String],
    work: &mut Work,
    limits: BoundedSearchLimits,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Option<u32>, BoundedSearchError> {
    match query {
        CompiledQuery::Term(term) => {
            for token in tokens {
                execution_checkpoint(checkpoint)?;
                visit_token(work, limits)?;
                compare_token(work, limits)?;
                if token == term {
                    return Ok(Some(1));
                }
            }
            Ok(None)
        }
        CompiledQuery::Prefix(prefix) => {
            for token in tokens {
                execution_checkpoint(checkpoint)?;
                visit_token(work, limits)?;
                compare_token(work, limits)?;
                if token.starts_with(prefix) {
                    return Ok(Some(1));
                }
            }
            Ok(None)
        }
        CompiledQuery::Phrase(phrase) => {
            if phrase.len() > tokens.len() {
                return Ok(None);
            }
            for window in tokens.windows(phrase.len()) {
                execution_checkpoint(checkpoint)?;
                let mut matched = true;
                for (actual, expected) in window.iter().zip(phrase) {
                    execution_checkpoint(checkpoint)?;
                    visit_token(work, limits)?;
                    compare_token(work, limits)?;
                    if actual != expected {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Ok(Some(1));
                }
            }
            Ok(None)
        }
        CompiledQuery::Fuzzy { term, max_distance } => {
            for token in tokens {
                execution_checkpoint(checkpoint)?;
                visit_token(work, limits)?;
                let candidate: Vec<char> = token.chars().collect();
                let steps = term
                    .len()
                    .checked_add(1)
                    .and_then(|left| {
                        candidate
                            .len()
                            .checked_add(1)
                            .and_then(|right| left.checked_mul(right))
                    })
                    .ok_or(BoundedSearchError::FuzzyStepBudgetExceeded {
                        maximum: limits.max_fuzzy_steps,
                    })?;
                add_bounded(&mut work.fuzzy_steps, steps, limits.max_fuzzy_steps).map_err(
                    |()| BoundedSearchError::FuzzyStepBudgetExceeded {
                        maximum: limits.max_fuzzy_steps,
                    },
                )?;
                if levenshtein(term, &candidate, checkpoint)? <= *max_distance {
                    return Ok(Some(1));
                }
            }
            Ok(None)
        }
        CompiledQuery::Boolean {
            must,
            should,
            must_not,
        } => {
            let mut score = 0_u32;
            for clause in must {
                let Some(clause_score) = evaluate(clause, tokens, work, limits, checkpoint)? else {
                    return Ok(None);
                };
                score = score.saturating_add(clause_score);
            }
            for clause in must_not {
                if evaluate(clause, tokens, work, limits, checkpoint)?.is_some() {
                    return Ok(None);
                }
            }
            let mut should_matches = 0_u32;
            for clause in should {
                if let Some(clause_score) = evaluate(clause, tokens, work, limits, checkpoint)? {
                    should_matches = should_matches.saturating_add(clause_score);
                }
            }
            if must.is_empty() && should_matches == 0 {
                Ok(None)
            } else {
                Ok(Some(score.saturating_add(should_matches).max(1)))
            }
        }
    }
}

fn visit_token(work: &mut Work, limits: BoundedSearchLimits) -> Result<(), BoundedSearchError> {
    add_bounded(&mut work.token_visits, 1, limits.max_token_visits).map_err(|()| {
        BoundedSearchError::TokenVisitBudgetExceeded {
            maximum: limits.max_token_visits,
        }
    })
}

fn compare_token(work: &mut Work, limits: BoundedSearchLimits) -> Result<(), BoundedSearchError> {
    add_bounded(&mut work.comparisons, 1, limits.max_token_comparisons).map_err(|()| {
        BoundedSearchError::ComparisonBudgetExceeded {
            maximum: limits.max_token_comparisons,
        }
    })
}

fn add_bounded(value: &mut usize, amount: usize, maximum: usize) -> Result<(), ()> {
    let next = value.checked_add(amount).ok_or(())?;
    if next > maximum {
        Err(())
    } else {
        *value = next;
        Ok(())
    }
}

fn levenshtein(
    left: &[char],
    right: &[char],
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<usize, BoundedSearchError> {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_character) in left.iter().enumerate() {
        execution_checkpoint(checkpoint)?;
        current[0] = left_index + 1;
        for (right_index, right_character) in right.iter().enumerate() {
            execution_checkpoint(checkpoint)?;
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_character != right_character));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(previous[right.len()])
}

fn execution_checkpoint(checkpoint: &mut dyn FnMut() -> bool) -> Result<(), BoundedSearchError> {
    if checkpoint() {
        Ok(())
    } else {
        Err(BoundedSearchError::ExecutionInterrupted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> BTreeMap<Vec<u8>, String> {
        BTreeMap::from([
            (b"a".to_vec(), "rust storage engine".to_owned()),
            (b"b".to_vec(), "rust search storage".to_owned()),
            (b"c".to_vec(), "durable searching".to_owned()),
        ])
    }

    #[test]
    fn boolean_phrase_prefix_and_fuzzy_are_composable_and_deterministic()
    -> Result<(), Box<dyn std::error::Error>> {
        let query = BoundedSearchQuery::boolean(
            vec![BoundedSearchQuery::Term("RUST".to_owned())],
            vec![
                BoundedSearchQuery::Phrase("rust storage".to_owned()),
                BoundedSearchQuery::Prefix("stor".to_owned()),
                BoundedSearchQuery::Fuzzy {
                    term: "enginr".to_owned(),
                    max_distance: 1,
                },
            ],
            vec![BoundedSearchQuery::Term("search".to_owned())],
        );
        let first = search_documents(&documents(), &query, 10, BoundedSearchLimits::default());
        let second = search_documents(&documents(), &query, 10, BoundedSearchLimits::default());
        assert_eq!(first, second);
        let result = first?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].document_id, b"a");
        assert_eq!(result.hits[0].score.to_bits(), 4.0_f64.to_bits());
        Ok(())
    }

    #[test]
    fn every_execution_budget_fails_without_partial_results() {
        let query = BoundedSearchQuery::Fuzzy {
            term: "storage".to_owned(),
            max_distance: 1,
        };
        let defaults = BoundedSearchLimits::default();
        let cases = [
            (
                BoundedSearchLimits {
                    max_documents: 1,
                    ..defaults
                },
                BoundedSearchError::DocumentBudgetExceeded { maximum: 1 },
            ),
            (
                BoundedSearchLimits {
                    max_source_bytes: 1,
                    ..defaults
                },
                BoundedSearchError::SourceByteBudgetExceeded { maximum: 1 },
            ),
            (
                BoundedSearchLimits {
                    max_token_visits: 1,
                    ..defaults
                },
                BoundedSearchError::TokenVisitBudgetExceeded { maximum: 1 },
            ),
            (
                BoundedSearchLimits {
                    max_fuzzy_steps: 1,
                    ..defaults
                },
                BoundedSearchError::FuzzyStepBudgetExceeded { maximum: 1 },
            ),
            (
                BoundedSearchLimits {
                    max_matches: 1,
                    ..defaults
                },
                BoundedSearchError::MatchBudgetExceeded { maximum: 1 },
            ),
        ];
        for (limits, expected) in cases {
            assert_eq!(
                search_documents(&documents(), &query, 10, limits),
                Err(expected)
            );
        }
    }

    #[test]
    fn phrase_is_positional_and_ties_use_binary_document_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let docs = BTreeMap::from([
            (b"z".to_vec(), "one two".to_owned()),
            (b"a".to_vec(), "one two".to_owned()),
            (b"m".to_vec(), "one red two".to_owned()),
        ]);
        let result = search_documents(
            &docs,
            &BoundedSearchQuery::Phrase("one two".to_owned()),
            10,
            BoundedSearchLimits::default(),
        )?;
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.document_id.as_slice())
                .collect::<Vec<_>>(),
            vec![b"a", b"z"]
        );
        Ok(())
    }

    #[test]
    fn invalid_queries_are_rejected_before_document_work() {
        assert_eq!(
            search_documents(
                &documents(),
                &BoundedSearchQuery::Prefix("two terms".to_owned()),
                10,
                BoundedSearchLimits::default(),
            ),
            Err(BoundedSearchError::InvalidSingleTerm)
        );
        assert_eq!(
            search_documents(
                &documents(),
                &BoundedSearchQuery::boolean(
                    Vec::new(),
                    Vec::new(),
                    vec![BoundedSearchQuery::Term("rust".to_owned())]
                ),
                10,
                BoundedSearchLimits::default(),
            ),
            Err(BoundedSearchError::NoPositiveClause)
        );
    }

    #[test]
    fn search_observes_deterministic_mid_execution_cancellation() {
        let mut checkpoints = 0_usize;
        let result = search_documents_with_checkpoint(
            &documents(),
            &BoundedSearchQuery::Fuzzy {
                term: "searching".to_owned(),
                max_distance: 2,
            },
            10,
            BoundedSearchLimits::default(),
            &mut || {
                checkpoints = checkpoints.saturating_add(1);
                checkpoints < 6
            },
        );
        assert_eq!(result, Err(BoundedSearchError::ExecutionInterrupted));
        assert_eq!(checkpoints, 6);
    }
}
