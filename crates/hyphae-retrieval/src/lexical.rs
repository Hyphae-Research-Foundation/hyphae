// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic provider-free lexical retrieval under semantics v1.

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    time::{Duration, Instant},
};

use hyphae_core::VectorSpaceName;
use hyphae_query::{FieldPath, Record, Value};
use thiserror::Error;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// Maximum UTF-8 token length retained by tokenizer v1.
pub const MAX_LEXICAL_TOKEN_BYTES: usize = 256;
/// Maximum positive field weight.
pub const MAX_LEXICAL_FIELD_WEIGHT_MICROS: u32 = 1_000_000_000;
/// Maximum fields in one lexical definition.
pub const MAX_LEXICAL_FIELDS: usize = 64;
/// Maximum exact segments in one configured path.
pub const MAX_LEXICAL_PATH_SEGMENTS: usize = 32;
/// Maximum UTF-8 bytes in one path segment.
pub const MAX_LEXICAL_PATH_SEGMENT_BYTES: usize = 1_024;
const WEIGHT_SCALE: f64 = 1_000_000.0;
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// One configured document field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalField {
    /// Exact canonical document field path.
    pub path: FieldPath,
    /// Positive weight in millionths.
    pub weight_micros: u32,
}

/// Immutable named lexical-index definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalIndexDefinition {
    /// Canonical index identifier.
    pub name: VectorSpaceName,
    /// Unique fields sorted by exact path.
    pub fields: Vec<LexicalField>,
}

impl LexicalIndexDefinition {
    /// Constructs and canonicalizes one definition.
    ///
    /// # Errors
    ///
    /// Rejects empty definitions, empty paths, duplicate paths, and invalid
    /// weights.
    pub fn new(name: VectorSpaceName, mut fields: Vec<LexicalField>) -> Result<Self, LexicalError> {
        if fields.is_empty() {
            return Err(LexicalError::EmptyFields);
        }
        if fields.len() > MAX_LEXICAL_FIELDS {
            return Err(LexicalError::TooManyFields);
        }
        if fields.iter().any(|field| field.path.segments().is_empty()) {
            return Err(LexicalError::EmptyFieldPath);
        }
        if fields.iter().any(|field| {
            field.path.segments().len() > MAX_LEXICAL_PATH_SEGMENTS
                || field.path.segments().iter().any(|segment| {
                    segment.is_empty() || segment.len() > MAX_LEXICAL_PATH_SEGMENT_BYTES
                })
        }) {
            return Err(LexicalError::InvalidFieldSegment);
        }
        if fields
            .iter()
            .any(|field| !(1..=MAX_LEXICAL_FIELD_WEIGHT_MICROS).contains(&field.weight_micros))
        {
            return Err(LexicalError::InvalidFieldWeight);
        }
        fields.sort_by(|left, right| left.path.cmp(&right.path));
        if fields.windows(2).any(|pair| pair[0].path == pair[1].path) {
            return Err(LexicalError::DuplicateFieldPath);
        }
        Ok(Self { name, fields })
    }
}

/// Complete lexical retrieval request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalRequest {
    /// Named durable definition.
    pub index: VectorSpaceName,
    /// UTF-8 query analyzed by tokenizer v1.
    pub query: String,
    /// Maximum returned documents.
    pub limit: usize,
}

/// Complete bounded execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalLimits {
    /// Maximum documents inspected.
    pub max_documents: u64,
    /// Maximum normalized tokens across corpus and query.
    pub max_tokens: u64,
    /// Maximum documents retained after matching.
    pub max_candidates: u64,
    /// Maximum returned documents.
    pub max_returned: usize,
    /// Cooperative timeout.
    pub timeout: Duration,
}

impl Default for LexicalLimits {
    fn default() -> Self {
        Self {
            max_documents: 1_000_000,
            max_tokens: 10_000_000,
            max_candidates: 100_000,
            max_returned: 1_000,
            timeout: Duration::from_secs(30),
        }
    }
}

/// One field contribution for one query term.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalFieldContribution {
    /// Canonical field path.
    pub path: FieldPath,
    /// Raw term frequency.
    pub term_frequency: u64,
    /// Field token length.
    pub field_length: u64,
}

/// One canonical query-term explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalTermContribution {
    /// Canonical token.
    pub token: String,
    /// Corpus document frequency.
    pub document_frequency: u64,
    /// Quantized contribution to the final score.
    pub score_nanos: i64,
    /// Configured fields in canonical order.
    pub fields: Vec<LexicalFieldContribution>,
}

/// One canonical lexical match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalMatch {
    /// Binary object key.
    pub key: Vec<u8>,
    /// Canonical BM25F score in nanos.
    pub score_nanos: i64,
    /// Per-term deterministic explanation.
    pub terms: Vec<LexicalTermContribution>,
}

/// Stable normal abstention reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexicalAbstentionReason {
    /// No document contains any normalized query token.
    NoCandidates,
}

/// Stable normal abstention evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalAbstention {
    /// Stable reason.
    pub reason: LexicalAbstentionReason,
    /// Documents inspected.
    pub scanned_documents: u64,
    /// Canonical unique query tokens.
    pub query_tokens: Vec<String>,
}

/// Complete lexical outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexicalOutcome {
    /// Accepted ranked documents.
    Matches {
        /// Final matches.
        matches: Vec<LexicalMatch>,
        /// Documents inspected.
        scanned_documents: u64,
        /// Documents containing a query token.
        matched_documents: u64,
        /// Canonical unique query tokens.
        query_tokens: Vec<String>,
    },
    /// Typed normal abstention.
    Abstained(LexicalAbstention),
}

/// Complete lexical execution failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LexicalError {
    /// At least one field is required.
    #[error("lexical definition requires at least one field")]
    EmptyFields,
    /// The definition exceeds the field-count bound.
    #[error("lexical definition exceeds 64 fields")]
    TooManyFields,
    /// Root/empty field paths are not accepted.
    #[error("lexical field path must be nonempty")]
    EmptyFieldPath,
    /// Path segments must be nonempty and bounded.
    #[error("lexical field path contains an invalid segment")]
    InvalidFieldSegment,
    /// Field paths must be unique.
    #[error("lexical field paths must be unique")]
    DuplicateFieldPath,
    /// Field weights must be positive and bounded.
    #[error("lexical field weight is outside 1..=1000000000")]
    InvalidFieldWeight,
    /// Request and definition names differ.
    #[error("lexical request index does not match the definition")]
    IndexMismatch,
    /// The normalized query has no tokens.
    #[error("lexical query has no retained normalized tokens")]
    EmptyQuery,
    /// At least one result must be requested.
    #[error("lexical result limit must be nonzero")]
    ZeroLimit,
    /// Requested result count exceeds policy.
    #[error("lexical result limit {requested} exceeds maximum {maximum}")]
    ResultLimitExceeded {
        /// Requested count.
        requested: usize,
        /// Maximum count.
        maximum: usize,
    },
    /// Record keys must be nonempty.
    #[error("lexical document key must be nonempty")]
    EmptyDocumentKey,
    /// Record keys must be unique.
    #[error("duplicate lexical document key")]
    DuplicateDocumentKey,
    /// Document budget exhausted.
    #[error("lexical document budget exceeded: {maximum}")]
    DocumentBudgetExceeded {
        /// Maximum documents.
        maximum: u64,
    },
    /// Token budget exhausted.
    #[error("lexical token budget exceeded: {maximum}")]
    TokenBudgetExceeded {
        /// Maximum tokens.
        maximum: u64,
    },
    /// Candidate budget exhausted.
    #[error("lexical candidate budget exceeded: {maximum}")]
    CandidateBudgetExceeded {
        /// Maximum candidates.
        maximum: u64,
    },
    /// Rebuildable lexical statistics are structurally inconsistent.
    #[error("materialized lexical projection is malformed")]
    MalformedProjection,
    /// Cooperative deadline elapsed.
    #[error("lexical retrieval timed out")]
    TimedOut,
    /// Canonical numeric operation failed.
    #[error("lexical score arithmetic overflow or non-finite result")]
    ArithmeticOverflow,
}

/// Applies tokenizer semantics `hyphae-unicode-tokenizer-v1`.
pub fn tokenize_v1(input: &str) -> Vec<String> {
    match tokenize_v1_checked(
        input,
        || Ok::<(), Infallible>(()),
        || Ok::<(), Infallible>(()),
    ) {
        Ok(tokens) => tokens,
        Err(never) => match never {},
    }
}

/// Applies tokenizer v1 while cooperatively checking work and retained-token policy.
///
/// `checkpoint` runs throughout normalization, including within one very long
/// token. `accept_token` runs immediately before each retained token is added.
///
/// # Errors
///
/// Returns the first error produced by either callback without returning a
/// partial token list.
pub fn tokenize_v1_checked<E>(
    input: &str,
    mut checkpoint: impl FnMut() -> Result<(), E>,
    mut accept_token: impl FnMut() -> Result<(), E>,
) -> Result<Vec<String>, E> {
    const CHECKPOINT_INTERVAL: usize = 256;

    checkpoint()?;
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut discarding_oversized_token = false;
    for (index, character) in input.nfkc().case_fold().enumerate() {
        if index % CHECKPOINT_INTERVAL == 0 {
            checkpoint()?;
        }
        if character.is_alphanumeric() {
            if !discarding_oversized_token {
                let next_length = token.len().saturating_add(character.len_utf8());
                if next_length <= MAX_LEXICAL_TOKEN_BYTES {
                    token.push(character);
                } else {
                    token.clear();
                    discarding_oversized_token = true;
                }
            }
        } else {
            push_token_checked(
                &mut tokens,
                &mut token,
                &mut discarding_oversized_token,
                &mut accept_token,
            )?;
        }
    }
    checkpoint()?;
    push_token_checked(
        &mut tokens,
        &mut token,
        &mut discarding_oversized_token,
        &mut accept_token,
    )?;
    Ok(tokens)
}

fn push_token_checked<E>(
    tokens: &mut Vec<String>,
    token: &mut String,
    discarding_oversized_token: &mut bool,
    accept_token: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    if !*discarding_oversized_token && !token.is_empty() {
        accept_token()?;
        tokens.push(std::mem::take(token));
    } else {
        token.clear();
    }
    *discarding_oversized_token = false;
    Ok(())
}

struct AnalyzedDocument {
    key: Vec<u8>,
    fields: Vec<Vec<String>>,
}

#[derive(Clone, Copy)]
struct LexicalDeadline {
    started: Instant,
    timeout: Duration,
}

impl LexicalDeadline {
    fn check(self) -> Result<(), LexicalError> {
        check_timeout(self.started, self.timeout)
    }
}

struct ScoringContext<'a> {
    document_count: u64,
    averages: &'a [f64],
    frequencies: &'a BTreeMap<String, u64>,
    definition: &'a LexicalIndexDefinition,
    query_tokens: &'a [String],
    deadline: LexicalDeadline,
}

/// Rebuildable lexical statistics for one document that contains at least one
/// normalized query term.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalMaterializedDocument {
    /// Binary document key.
    pub key: Vec<u8>,
    /// Token count for every configured field in canonical field order.
    pub field_lengths: Vec<u64>,
    /// Per-field frequencies for each canonical query token.
    pub term_frequencies: BTreeMap<String, Vec<u64>>,
}

/// Complete bounded view read from a rebuildable lexical projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalMaterializedCorpus {
    /// Number of authoritative documents represented by the projection.
    pub document_count: u64,
    /// Total normalized token count across all configured fields.
    pub token_count: u64,
    /// Per-field token totals across the complete corpus.
    pub total_field_lengths: Vec<u64>,
    /// Candidate documents containing at least one query token.
    pub documents: Vec<LexicalMaterializedDocument>,
}

/// Executes the deterministic BM25F-compatible reference algorithm.
///
/// # Errors
///
/// Returns an invalid-input, budget, timeout, or numeric error and never a
/// partial ranking.
pub fn retrieve_lexical(
    records: &[Record],
    definition: &LexicalIndexDefinition,
    request: &LexicalRequest,
    limits: &LexicalLimits,
) -> Result<LexicalOutcome, LexicalError> {
    validate_request(definition, request, limits)?;
    let started = Instant::now();
    let query_tokens = tokenize_before_deadline(&request.query, started, limits.timeout)?
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if query_tokens.is_empty() {
        return Err(LexicalError::EmptyQuery);
    }
    let mut token_count = u64::try_from(query_tokens.len()).unwrap_or(u64::MAX);
    if token_count > limits.max_tokens {
        return Err(LexicalError::TokenBudgetExceeded {
            maximum: limits.max_tokens,
        });
    }
    let mut keys = BTreeSet::new();
    let mut documents = Vec::with_capacity(records.len());
    let mut total_lengths = vec![0_u64; definition.fields.len()];
    for record in records {
        check_timeout(started, limits.timeout)?;
        if u64::try_from(documents.len()).unwrap_or(u64::MAX) >= limits.max_documents {
            return Err(LexicalError::DocumentBudgetExceeded {
                maximum: limits.max_documents,
            });
        }
        if record.key.is_empty() {
            return Err(LexicalError::EmptyDocumentKey);
        }
        if !keys.insert(record.key.as_slice()) {
            return Err(LexicalError::DuplicateDocumentKey);
        }
        let mut fields = Vec::with_capacity(definition.fields.len());
        for (field_index, field) in definition.fields.iter().enumerate() {
            let tokens = match field.path.resolve(&record.value) {
                Some(Value::String(value)) => {
                    tokenize_with_limits(value, &mut token_count, started, limits)?
                }
                _ => Vec::new(),
            };
            let length = u64::try_from(tokens.len()).unwrap_or(u64::MAX);
            total_lengths[field_index] = total_lengths[field_index]
                .checked_add(length)
                .ok_or(LexicalError::ArithmeticOverflow)?;
            fields.push(tokens);
        }
        documents.push(AnalyzedDocument {
            key: record.key.clone(),
            fields,
        });
    }
    score_documents(
        &documents,
        &total_lengths,
        definition,
        request,
        limits,
        &query_tokens,
        started,
    )
}

/// Executes BM25F from a rebuildable materialized lexical projection.
///
/// # Errors
///
/// Returns an invalid-input, malformed projection, budget, timeout, or
/// numeric error and never a partial ranking.
pub fn retrieve_lexical_materialized(
    corpus: &LexicalMaterializedCorpus,
    definition: &LexicalIndexDefinition,
    request: &LexicalRequest,
    limits: &LexicalLimits,
) -> Result<LexicalOutcome, LexicalError> {
    validate_request(definition, request, limits)?;
    let started = Instant::now();
    let query_tokens = tokenize_before_deadline(&request.query, started, limits.timeout)?
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if query_tokens.is_empty() {
        return Err(LexicalError::EmptyQuery);
    }
    validate_materialized_corpus(corpus, definition, &query_tokens, limits, started)?;
    let averages = corpus
        .total_field_lengths
        .iter()
        .map(|length| {
            if corpus.document_count == 0 {
                0.0
            } else {
                bounded_count_as_f64(*length) / bounded_count_as_f64(corpus.document_count)
            }
        })
        .collect::<Vec<_>>();
    let mut frequencies = BTreeMap::new();
    for token in &query_tokens {
        check_timeout(started, limits.timeout)?;
        let mut frequency = 0_u64;
        for document in &corpus.documents {
            check_timeout(started, limits.timeout)?;
            if document
                .term_frequencies
                .get(token)
                .is_some_and(|fields| fields.iter().any(|value| *value > 0))
            {
                frequency = frequency
                    .checked_add(1)
                    .ok_or(LexicalError::ArithmeticOverflow)?;
            }
        }
        frequencies.insert(token.clone(), frequency);
    }
    let scoring = ScoringContext {
        document_count: corpus.document_count,
        averages: &averages,
        frequencies: &frequencies,
        definition,
        query_tokens: &query_tokens,
        deadline: LexicalDeadline {
            started,
            timeout: limits.timeout,
        },
    };
    let mut matches = Vec::with_capacity(corpus.documents.len().min(request.limit));
    for document in &corpus.documents {
        scoring.deadline.check()?;
        if let Some(matched) = score_materialized_document(document, &scoring)? {
            matches.push(matched);
        }
    }
    finish_ranking(
        matches,
        corpus.document_count,
        &query_tokens,
        request.limit,
        started,
        limits.timeout,
    )
}

fn validate_materialized_corpus(
    corpus: &LexicalMaterializedCorpus,
    definition: &LexicalIndexDefinition,
    query_tokens: &[String],
    limits: &LexicalLimits,
    started: Instant,
) -> Result<(), LexicalError> {
    check_timeout(started, limits.timeout)?;
    if corpus.document_count > limits.max_documents {
        return Err(LexicalError::DocumentBudgetExceeded {
            maximum: limits.max_documents,
        });
    }
    let total_tokens = corpus
        .token_count
        .checked_add(u64::try_from(query_tokens.len()).unwrap_or(u64::MAX))
        .ok_or(LexicalError::TokenBudgetExceeded {
            maximum: limits.max_tokens,
        })?;
    if total_tokens > limits.max_tokens {
        return Err(LexicalError::TokenBudgetExceeded {
            maximum: limits.max_tokens,
        });
    }
    if u64::try_from(corpus.documents.len()).unwrap_or(u64::MAX) > limits.max_candidates {
        return Err(LexicalError::CandidateBudgetExceeded {
            maximum: limits.max_candidates,
        });
    }
    if corpus.total_field_lengths.len() != definition.fields.len() {
        return Err(LexicalError::MalformedProjection);
    }
    let mut keys = BTreeSet::new();
    for document in &corpus.documents {
        check_timeout(started, limits.timeout)?;
        if document.key.is_empty() {
            return Err(LexicalError::EmptyDocumentKey);
        }
        if !keys.insert(document.key.as_slice()) {
            return Err(LexicalError::DuplicateDocumentKey);
        }
        if document.field_lengths.len() != definition.fields.len()
            || document.term_frequencies.len() != query_tokens.len()
        {
            return Err(LexicalError::MalformedProjection);
        }
        for token in query_tokens {
            check_timeout(started, limits.timeout)?;
            if document
                .term_frequencies
                .get(token)
                .is_none_or(|frequencies| frequencies.len() != definition.fields.len())
            {
                return Err(LexicalError::MalformedProjection);
            }
        }
    }
    Ok(())
}

fn tokenize_before_deadline(
    input: &str,
    started: Instant,
    timeout: Duration,
) -> Result<Vec<String>, LexicalError> {
    tokenize_v1_checked(
        input,
        || check_timeout(started, timeout),
        || check_timeout(started, timeout),
    )
}

fn tokenize_with_limits(
    input: &str,
    token_count: &mut u64,
    started: Instant,
    limits: &LexicalLimits,
) -> Result<Vec<String>, LexicalError> {
    tokenize_v1_checked(
        input,
        || check_timeout(started, limits.timeout),
        || {
            *token_count = token_count
                .checked_add(1)
                .ok_or(LexicalError::TokenBudgetExceeded {
                    maximum: limits.max_tokens,
                })?;
            if *token_count > limits.max_tokens {
                return Err(LexicalError::TokenBudgetExceeded {
                    maximum: limits.max_tokens,
                });
            }
            Ok(())
        },
    )
}

fn validate_request(
    definition: &LexicalIndexDefinition,
    request: &LexicalRequest,
    limits: &LexicalLimits,
) -> Result<(), LexicalError> {
    if request.index != definition.name {
        return Err(LexicalError::IndexMismatch);
    }
    if request.limit == 0 {
        return Err(LexicalError::ZeroLimit);
    }
    if request.limit > limits.max_returned {
        return Err(LexicalError::ResultLimitExceeded {
            requested: request.limit,
            maximum: limits.max_returned,
        });
    }
    Ok(())
}

fn score_documents(
    documents: &[AnalyzedDocument],
    total_lengths: &[u64],
    definition: &LexicalIndexDefinition,
    request: &LexicalRequest,
    limits: &LexicalLimits,
    query_tokens: &[String],
    started: Instant,
) -> Result<LexicalOutcome, LexicalError> {
    check_timeout(started, limits.timeout)?;
    let document_count = u64::try_from(documents.len()).unwrap_or(u64::MAX);
    let averages = total_lengths
        .iter()
        .map(|length| {
            if document_count == 0 {
                0.0
            } else {
                bounded_count_as_f64(*length) / bounded_count_as_f64(document_count)
            }
        })
        .collect::<Vec<_>>();
    let mut frequencies = BTreeMap::new();
    for token in query_tokens {
        check_timeout(started, limits.timeout)?;
        let mut count = 0_u64;
        for document in documents {
            check_timeout(started, limits.timeout)?;
            let mut present = false;
            'fields: for field in &document.fields {
                for candidate in field {
                    check_timeout(started, limits.timeout)?;
                    if candidate == token {
                        present = true;
                        break 'fields;
                    }
                }
            }
            if present {
                count = count
                    .checked_add(1)
                    .ok_or(LexicalError::ArithmeticOverflow)?;
            }
        }
        frequencies.insert(token.clone(), count);
    }
    let scoring = ScoringContext {
        document_count,
        averages: &averages,
        frequencies: &frequencies,
        definition,
        query_tokens,
        deadline: LexicalDeadline {
            started,
            timeout: limits.timeout,
        },
    };
    let mut matches = Vec::new();
    for document in documents {
        scoring.deadline.check()?;
        let document_match = score_document(document, &scoring)?;
        if let Some(matched) = document_match {
            if u64::try_from(matches.len()).unwrap_or(u64::MAX) >= limits.max_candidates {
                return Err(LexicalError::CandidateBudgetExceeded {
                    maximum: limits.max_candidates,
                });
            }
            matches.push(matched);
        }
    }
    finish_ranking(
        matches,
        document_count,
        query_tokens,
        request.limit,
        started,
        limits.timeout,
    )
}

fn score_document(
    document: &AnalyzedDocument,
    context: &ScoringContext<'_>,
) -> Result<Option<LexicalMatch>, LexicalError> {
    context.deadline.check()?;
    let field_lengths = document
        .fields
        .iter()
        .map(|field| u64::try_from(field.len()).unwrap_or(u64::MAX))
        .collect::<Vec<_>>();
    let mut term_frequencies = BTreeMap::new();
    for token in context.query_tokens {
        context.deadline.check()?;
        let mut per_field = Vec::with_capacity(document.fields.len());
        for field in &document.fields {
            let mut frequency = 0_u64;
            for candidate in field {
                context.deadline.check()?;
                if candidate == token {
                    frequency = frequency
                        .checked_add(1)
                        .ok_or(LexicalError::ArithmeticOverflow)?;
                }
            }
            per_field.push(frequency);
        }
        term_frequencies.insert(token.clone(), per_field);
    }
    score_statistics(&document.key, &field_lengths, &term_frequencies, context)
}

fn score_materialized_document(
    document: &LexicalMaterializedDocument,
    context: &ScoringContext<'_>,
) -> Result<Option<LexicalMatch>, LexicalError> {
    score_statistics(
        &document.key,
        &document.field_lengths,
        &document.term_frequencies,
        context,
    )
}

fn score_statistics(
    key: &[u8],
    field_lengths: &[u64],
    term_frequencies: &BTreeMap<String, Vec<u64>>,
    context: &ScoringContext<'_>,
) -> Result<Option<LexicalMatch>, LexicalError> {
    context.deadline.check()?;
    let mut terms = Vec::new();
    let mut score_nanos = 0_i64;
    for token in context.query_tokens {
        context.deadline.check()?;
        let document_frequency = context.frequencies[token];
        if document_frequency == 0 {
            continue;
        }
        let mut combined_tf = 0.0_f64;
        let mut fields = Vec::with_capacity(context.definition.fields.len());
        for (index, definition_field) in context.definition.fields.iter().enumerate() {
            context.deadline.check()?;
            let term_frequency = term_frequencies[token][index];
            let field_length = field_lengths[index];
            fields.push(LexicalFieldContribution {
                path: definition_field.path.clone(),
                term_frequency,
                field_length,
            });
            if term_frequency > 0 && context.averages[index] > 0.0 {
                let normalization =
                    1.0 - B + B * bounded_count_as_f64(field_length) / context.averages[index];
                combined_tf += (f64::from(definition_field.weight_micros) / WEIGHT_SCALE)
                    * bounded_count_as_f64(term_frequency)
                    / normalization;
            }
        }
        if combined_tf == 0.0 {
            continue;
        }
        let numerator =
            bounded_count_as_f64(context.document_count.saturating_sub(document_frequency)) + 0.5;
        let denominator = bounded_count_as_f64(document_frequency) + 0.5;
        let idf = libm::log(1.0 + numerator / denominator);
        let term_score = quantize_score(idf * combined_tf * (K1 + 1.0) / (combined_tf + K1))?;
        score_nanos = score_nanos
            .checked_add(term_score)
            .ok_or(LexicalError::ArithmeticOverflow)?;
        terms.push(LexicalTermContribution {
            token: token.clone(),
            document_frequency,
            score_nanos: term_score,
            fields,
        });
    }
    Ok((score_nanos > 0).then(|| LexicalMatch {
        key: key.to_vec(),
        score_nanos,
        terms,
    }))
}

fn finish_ranking(
    mut matches: Vec<LexicalMatch>,
    document_count: u64,
    query_tokens: &[String],
    limit: usize,
    started: Instant,
    timeout: Duration,
) -> Result<LexicalOutcome, LexicalError> {
    check_timeout(started, timeout)?;
    matches.sort_by(|left, right| {
        right
            .score_nanos
            .cmp(&left.score_nanos)
            .then_with(|| left.key.cmp(&right.key))
    });
    check_timeout(started, timeout)?;
    let matched_documents = u64::try_from(matches.len()).unwrap_or(u64::MAX);
    matches.truncate(limit);
    Ok(if matches.is_empty() {
        LexicalOutcome::Abstained(LexicalAbstention {
            reason: LexicalAbstentionReason::NoCandidates,
            scanned_documents: document_count,
            query_tokens: query_tokens.to_vec(),
        })
    } else {
        LexicalOutcome::Matches {
            matches,
            scanned_documents: document_count,
            matched_documents,
            query_tokens: query_tokens.to_vec(),
        }
    })
}

fn quantize_score(value: f64) -> Result<i64, LexicalError> {
    if !value.is_finite() || value < 0.0 {
        return Err(LexicalError::ArithmeticOverflow);
    }
    let scaled = value * 1_000_000_000.0;
    if !scaled.is_finite() {
        return Err(LexicalError::ArithmeticOverflow);
    }
    if scaled >= maximum_i64_as_f64() {
        return Ok(i64::MAX);
    }
    Ok(rounded_nonnegative_f64_as_i64(scaled))
}

/// Counts accepted by lexical execution are bounded far below the 53-bit
/// integer precision of `f64`, so this conversion is exact.
#[allow(clippy::cast_precision_loss)]
fn bounded_count_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
fn maximum_i64_as_f64() -> f64 {
    i64::MAX as f64
}

/// The caller has already checked finiteness, non-negativity, and the i64
/// upper bound.
#[allow(clippy::cast_possible_truncation)]
fn rounded_nonnegative_f64_as_i64(value: f64) -> i64 {
    libm::floor(value + 0.5) as i64
}

fn check_timeout(started: Instant, timeout: Duration) -> Result<(), LexicalError> {
    if started.elapsed() >= timeout {
        Err(LexicalError::TimedOut)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use proptest::prelude::*;

    fn record(key: &[u8], title: &str, body: &str) -> Record {
        Record::new(
            key,
            Value::Object(BTreeMap::from([
                ("title".into(), Value::String(title.into())),
                ("body".into(), Value::String(body.into())),
            ])),
        )
    }

    fn definition() -> Result<LexicalIndexDefinition, LexicalError> {
        LexicalIndexDefinition::new(
            VectorSpaceName::new("docs").map_err(|_| LexicalError::EmptyFields)?,
            vec![
                LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                },
                LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 2_000_000,
                },
            ],
        )
    }

    fn materialize_reference_corpus(
        records: &[Record],
        definition: &LexicalIndexDefinition,
        query: &str,
    ) -> LexicalMaterializedCorpus {
        let query_tokens = tokenize_v1(query)
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut token_count = 0_u64;
        let mut total_field_lengths = vec![0_u64; definition.fields.len()];
        let mut documents = Vec::new();

        for record in records {
            let fields = definition
                .fields
                .iter()
                .map(|field| match field.path.resolve(&record.value) {
                    Some(Value::String(value)) => tokenize_v1(value),
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>();
            let field_lengths = fields
                .iter()
                .map(|field| u64::try_from(field.len()).unwrap_or(u64::MAX))
                .collect::<Vec<_>>();
            for (total, length) in total_field_lengths.iter_mut().zip(&field_lengths) {
                *total = total.saturating_add(*length);
                token_count = token_count.saturating_add(*length);
            }
            let term_frequencies = query_tokens
                .iter()
                .map(|token| {
                    let frequencies = fields
                        .iter()
                        .map(|field| {
                            u64::try_from(
                                field.iter().filter(|candidate| *candidate == token).count(),
                            )
                            .unwrap_or(u64::MAX)
                        })
                        .collect::<Vec<_>>();
                    (token.clone(), frequencies)
                })
                .collect::<BTreeMap<_, _>>();
            if term_frequencies
                .values()
                .any(|frequencies| frequencies.iter().any(|frequency| *frequency > 0))
            {
                documents.push(LexicalMaterializedDocument {
                    key: record.key.clone(),
                    field_lengths,
                    term_frequencies,
                });
            }
        }

        LexicalMaterializedCorpus {
            document_count: u64::try_from(records.len()).unwrap_or(u64::MAX),
            token_count,
            total_field_lengths,
            documents,
        }
    }

    #[test]
    fn tokenizer_pins_nfkc_casefold_and_alphanumeric_runs() {
        assert_eq!(
            tokenize_v1("Straße ＡＢＣ—café"),
            vec!["strasse", "abc", "café"]
        );
    }

    #[test]
    fn checked_tokenizer_matches_v1_and_checks_inside_one_long_token() {
        let input = "Straße ＡＢＣ—café";
        let mut accepted = 0_usize;
        let checked = match tokenize_v1_checked(
            input,
            || Ok::<(), Infallible>(()),
            || {
                accepted += 1;
                Ok::<(), Infallible>(())
            },
        ) {
            Ok(tokens) => tokens,
            Err(never) => match never {},
        };
        assert_eq!(checked, tokenize_v1(input));
        assert_eq!(accepted, checked.len());
        assert_eq!(
            tokenize_v1(&format!("{} tail", "a".repeat(257))),
            vec!["tail"]
        );

        let long_token = "a".repeat(2_048);
        let mut checkpoints = 0_usize;
        let interrupted = tokenize_v1_checked(
            &long_token,
            || {
                checkpoints += 1;
                if checkpoints == 3 {
                    Err("stop")
                } else {
                    Ok(())
                }
            },
            || Ok::<(), &'static str>(()),
        );
        assert_eq!(interrupted, Err("stop"));
        assert_eq!(checkpoints, 3);
    }

    #[test]
    fn zero_timeout_includes_query_tokenization_for_both_scorers() -> Result<(), LexicalError> {
        let definition = definition()?;
        let request = LexicalRequest {
            index: definition.name.clone(),
            query: "rust".into(),
            limit: 1,
        };
        let limits = LexicalLimits {
            timeout: Duration::ZERO,
            ..LexicalLimits::default()
        };
        let corpus = materialize_reference_corpus(&[], &definition, &request.query);

        assert_eq!(
            retrieve_lexical(&[], &definition, &request, &limits),
            Err(LexicalError::TimedOut)
        );
        assert_eq!(
            retrieve_lexical_materialized(&corpus, &definition, &request, &limits),
            Err(LexicalError::TimedOut)
        );
        Ok(())
    }

    #[test]
    fn duplicate_query_tokens_count_once_against_token_budget() -> Result<(), LexicalError> {
        let definition = definition()?;
        let request = LexicalRequest {
            index: definition.name.clone(),
            query: "rust rust".into(),
            limit: 1,
        };
        let limits = LexicalLimits {
            max_tokens: 1,
            ..LexicalLimits::default()
        };
        let corpus = materialize_reference_corpus(&[], &definition, &request.query);

        let reference = retrieve_lexical(&[], &definition, &request, &limits)?;
        let materialized = retrieve_lexical_materialized(&corpus, &definition, &request, &limits)?;
        assert_eq!(reference, materialized);
        assert!(matches!(
            reference,
            LexicalOutcome::Abstained(LexicalAbstention {
                query_tokens,
                ..
            }) if query_tokens == vec!["rust"]
        ));
        Ok(())
    }

    #[test]
    fn bm25f_is_deterministic_and_binary_key_breaks_ties() -> Result<(), LexicalError> {
        let definition = definition()?;
        let outcome = retrieve_lexical(
            &[
                record(b"b", "Rust memory", "durable engine"),
                record(b"a", "Rust memory", "durable engine"),
                record(b"z", "other", "nothing"),
            ],
            &definition,
            &LexicalRequest {
                index: definition.name.clone(),
                query: "RUST rust".into(),
                limit: 10,
            },
            &LexicalLimits::default(),
        )?;
        let LexicalOutcome::Matches { matches, .. } = outcome else {
            return Err(LexicalError::ArithmeticOverflow);
        };
        assert_eq!(matches[0].key, b"a");
        assert_eq!(matches[1].key, b"b");
        assert_eq!(matches[0].score_nanos, matches[1].score_nanos);
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn materialized_scorer_matches_reference_for_random_corpora(
            generated in prop::collection::vec(
                ("[a-z ]{0,24}", "[a-z ]{0,48}"),
                0..32
            ),
            query in "(rust|durable|engine|memory)( (rust|durable|engine|memory)){0,2}",
            limit in 1_usize..16
        ) {
            let definition = definition().map_err(|error| TestCaseError::fail(error.to_string()))?;
            let records = generated
                .iter()
                .enumerate()
                .map(|(index, (title, body))| {
                    record(&u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(), title, body)
                })
                .collect::<Vec<_>>();
            let request = LexicalRequest {
                index: definition.name.clone(),
                query: query.clone(),
                limit,
            };
            let limits = LexicalLimits::default();
            let reference = retrieve_lexical(&records, &definition, &request, &limits)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            let corpus = materialize_reference_corpus(&records, &definition, &query);
            let materialized =
                retrieve_lexical_materialized(&corpus, &definition, &request, &limits)
                    .map_err(|error| TestCaseError::fail(error.to_string()))?;

            prop_assert_eq!(materialized, reference);
        }
    }
}
