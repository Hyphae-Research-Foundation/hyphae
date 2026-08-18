// SPDX-License-Identifier: Apache-2.0

//! Bounded binary-glob compilation and hash pattern-scan contracts.

use thiserror::Error;

use crate::{HashFieldEntry, MAX_HASH_FIELD_BATCH_SIZE};

/// Maximum source bytes accepted by one native hash glob.
pub const MAX_HASH_PATTERN_BYTES: usize = 1_024;

/// Maximum matcher steps admitted by one native hash pattern-scan call.
pub const MAX_HASH_PATTERN_MATCH_STEPS: usize = 16_777_216;

/// Binary-glob request validation or execution failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HashPatternError {
    /// The source pattern exceeds its fixed byte bound.
    #[error("native hash pattern contains {requested} bytes; maximum is {MAX_HASH_PATTERN_BYTES}")]
    PatternTooLarge {
        /// Rejected caller-supplied pattern size.
        requested: usize,
    },
    /// The source pattern is not one complete canonical binary glob.
    #[error("native hash pattern is malformed at byte {offset}")]
    InvalidPattern {
        /// First source offset that cannot be parsed canonically.
        offset: usize,
    },
    /// The returned-entry limit is zero or exceeds its fixed bound.
    #[error(
        "native hash pattern output limit {requested} is outside 1..={MAX_HASH_FIELD_BATCH_SIZE}"
    )]
    InvalidOutputLimit {
        /// Rejected caller-supplied returned-entry bound.
        requested: usize,
    },
    /// The physical-visit limit is zero or exceeds its fixed bound.
    #[error(
        "native hash pattern visit limit {requested} is outside 1..={MAX_HASH_FIELD_BATCH_SIZE}"
    )]
    InvalidVisitLimit {
        /// Rejected caller-supplied physical-visit bound.
        requested: usize,
    },
    /// The matcher-step limit is zero or exceeds its fixed bound.
    #[error(
        "native hash pattern step limit {requested} is outside 1..={MAX_HASH_PATTERN_MATCH_STEPS}"
    )]
    InvalidMatchStepLimit {
        /// Rejected caller-supplied matcher-step bound.
        requested: usize,
    },
    /// Data-dependent matching exhausted the caller's admitted step budget.
    #[error("native hash pattern exhausted its {maximum}-step match budget")]
    MatchStepLimitExceeded {
        /// Maximum matcher steps admitted by the request.
        maximum: usize,
    },
}

/// Reason one successful native hash pattern page stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashPatternScanStop {
    /// The selected physical range has no later candidate.
    Exhausted,
    /// The page emitted its requested live-match count.
    OutputLimit,
    /// The page consumed its requested physical candidate count.
    VisitLimit,
}

/// One compiled, bounded native hash pattern-scan request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashPatternScanRequest {
    source: Vec<u8>,
    compiled: CompiledHashPattern,
    start_after: Option<Vec<u8>>,
    output_limit: usize,
    visit_limit: usize,
    match_step_limit: usize,
}

impl HashPatternScanRequest {
    /// Compiles and validates one binary-glob scan request.
    ///
    /// # Errors
    ///
    /// Returns [`HashPatternError`] for an oversized or malformed pattern or
    /// a zero or oversized execution bound.
    pub fn try_new(
        pattern: impl Into<Vec<u8>>,
        start_after: Option<Vec<u8>>,
        output_limit: usize,
        visit_limit: usize,
        match_step_limit: usize,
    ) -> Result<Self, HashPatternError> {
        let source = pattern.into();
        let compiled = CompiledHashPattern::compile(&source)?;
        validate_bounded_nonzero(output_limit, MAX_HASH_FIELD_BATCH_SIZE, |requested| {
            HashPatternError::InvalidOutputLimit { requested }
        })?;
        validate_bounded_nonzero(visit_limit, MAX_HASH_FIELD_BATCH_SIZE, |requested| {
            HashPatternError::InvalidVisitLimit { requested }
        })?;
        validate_bounded_nonzero(
            match_step_limit,
            MAX_HASH_PATTERN_MATCH_STEPS,
            |requested| HashPatternError::InvalidMatchStepLimit { requested },
        )?;
        Ok(Self {
            source,
            compiled,
            start_after,
            output_limit,
            visit_limit,
            match_step_limit,
        })
    }

    /// Returns the exact source glob bytes.
    pub fn pattern(&self) -> &[u8] {
        &self.source
    }

    /// Returns the optional exclusive exact-field cursor.
    pub fn start_after(&self) -> Option<&[u8]> {
        self.start_after.as_deref()
    }

    /// Returns the maximum live matches emitted by one page.
    pub const fn output_limit(&self) -> usize {
        self.output_limit
    }

    /// Returns the maximum physical field identities consumed by one page.
    pub const fn visit_limit(&self) -> usize {
        self.visit_limit
    }

    /// Returns the maximum binary-glob matcher steps admitted by one page.
    pub const fn match_step_limit(&self) -> usize {
        self.match_step_limit
    }

    /// Replaces the exclusive cursor without recompiling the pattern.
    pub fn set_start_after(&mut self, start_after: Option<Vec<u8>>) {
        self.start_after = start_after;
    }

    pub(crate) const fn compiled(&self) -> &CompiledHashPattern {
        &self.compiled
    }
}

/// One bounded native hash pattern-scan page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashPatternScanPage {
    entries: Vec<HashFieldEntry>,
    continuation: Option<Vec<u8>>,
    stop: HashPatternScanStop,
    visited: usize,
    match_steps: usize,
}

impl HashPatternScanPage {
    pub(crate) const fn new(
        entries: Vec<HashFieldEntry>,
        continuation: Option<Vec<u8>>,
        stop: HashPatternScanStop,
        visited: usize,
        match_steps: usize,
    ) -> Self {
        Self {
            entries,
            continuation,
            stop,
            visited,
            match_steps,
        }
    }

    /// Returns live matching field/value entries in ascending exact-byte order.
    pub fn entries(&self) -> &[HashFieldEntry] {
        &self.entries
    }

    /// Consumes the page and returns its owned live matching entries.
    pub fn into_entries(self) -> Vec<HashFieldEntry> {
        self.entries
    }

    /// Returns the last visited exact field when another call may be needed.
    pub fn continuation(&self) -> Option<&[u8]> {
        self.continuation.as_deref()
    }

    /// Returns why this successful page stopped.
    pub const fn stop(&self) -> HashPatternScanStop {
        self.stop
    }

    /// Returns physical field identities consumed by this page.
    pub const fn visited(&self) -> usize {
        self.visited
    }

    /// Returns binary-glob matcher steps consumed by this page.
    pub const fn match_steps(&self) -> usize {
        self.match_steps
    }
}

fn validate_bounded_nonzero<E>(
    requested: usize,
    maximum: usize,
    error: impl FnOnce(usize) -> E,
) -> Result<(), E> {
    if requested == 0 || requested > maximum {
        Err(error(requested))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompiledHashPattern {
    tokens: Vec<HashPatternToken>,
    leading_literal_prefix: Vec<u8>,
    exact_literal: bool,
}

impl CompiledHashPattern {
    fn compile(source: &[u8]) -> Result<Self, HashPatternError> {
        if source.len() > MAX_HASH_PATTERN_BYTES {
            return Err(HashPatternError::PatternTooLarge {
                requested: source.len(),
            });
        }
        let mut tokens = Vec::with_capacity(source.len());
        let mut offset = 0;
        while offset < source.len() {
            match source[offset] {
                b'\\' => {
                    offset += 1;
                    let Some(literal) = source.get(offset).copied() else {
                        return Err(HashPatternError::InvalidPattern { offset: offset - 1 });
                    };
                    tokens.push(HashPatternToken::Literal(literal));
                    offset += 1;
                }
                b'?' => {
                    tokens.push(HashPatternToken::AnyOne);
                    offset += 1;
                }
                b'*' => {
                    if !matches!(tokens.last(), Some(HashPatternToken::AnyMany)) {
                        tokens.push(HashPatternToken::AnyMany);
                    }
                    offset += 1;
                }
                b'[' => {
                    tokens.push(parse_class(source, &mut offset)?);
                }
                literal => {
                    tokens.push(HashPatternToken::Literal(literal));
                    offset += 1;
                }
            }
        }
        let leading_literal_prefix = tokens
            .iter()
            .map_while(|token| match token {
                HashPatternToken::Literal(byte) => Some(*byte),
                _ => None,
            })
            .collect();
        let exact_literal = tokens
            .iter()
            .all(|token| matches!(token, HashPatternToken::Literal(_)));
        Ok(Self {
            tokens,
            leading_literal_prefix,
            exact_literal,
        })
    }

    pub(crate) fn leading_literal_prefix(&self) -> &[u8] {
        &self.leading_literal_prefix
    }

    pub(crate) const fn is_exact_literal(&self) -> bool {
        self.exact_literal
    }

    pub(crate) fn matches(
        &self,
        input: &[u8],
        budget: &mut HashPatternMatchBudget,
    ) -> Result<bool, HashPatternError> {
        let mut token_index = 0;
        let mut input_index = 0;
        let mut last_star = None;
        let mut star_retry_input = 0;
        while input_index < input.len() {
            if let Some(token) = self.tokens.get(token_index) {
                budget.charge()?;
                match token {
                    HashPatternToken::AnyMany => {
                        last_star = Some(token_index);
                        token_index += 1;
                        star_retry_input = input_index;
                        continue;
                    }
                    candidate if candidate.matches(input[input_index], budget)? => {
                        token_index += 1;
                        input_index += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            let Some(star_index) = last_star else {
                return Ok(false);
            };
            budget.charge()?;
            star_retry_input += 1;
            input_index = star_retry_input;
            token_index = star_index + 1;
        }
        while matches!(
            self.tokens.get(token_index),
            Some(HashPatternToken::AnyMany)
        ) {
            budget.charge()?;
            token_index += 1;
        }
        Ok(token_index == self.tokens.len())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HashPatternToken {
    Literal(u8),
    AnyOne,
    AnyMany,
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

impl HashPatternToken {
    fn matches(
        &self,
        input: u8,
        budget: &mut HashPatternMatchBudget,
    ) -> Result<bool, HashPatternError> {
        match self {
            Self::Literal(literal) => Ok(*literal == input),
            Self::AnyOne => Ok(true),
            Self::AnyMany => Ok(false),
            Self::Class { negated, ranges } => {
                let mut contained = false;
                for (lower, upper) in ranges {
                    budget.charge()?;
                    if *lower <= input && input <= *upper {
                        contained = true;
                        break;
                    }
                }
                Ok(contained != *negated)
            }
        }
    }
}

fn parse_class(source: &[u8], offset: &mut usize) -> Result<HashPatternToken, HashPatternError> {
    let opening = *offset;
    *offset += 1;
    let negated = source.get(*offset).copied() == Some(b'^');
    if negated {
        *offset += 1;
        if *offset == source.len() {
            return Err(HashPatternError::InvalidPattern { offset: opening });
        }
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let Some(next) = source.get(*offset).copied() else {
            return Err(HashPatternError::InvalidPattern { offset: opening });
        };
        if next == b']' && !first {
            *offset += 1;
            break;
        }
        let lower = parse_class_byte(source, offset)?;
        first = false;
        if source.get(*offset).copied() == Some(b'-')
            && source.get(*offset + 1).is_some_and(|byte| *byte != b']')
        {
            let range_offset = *offset;
            *offset += 1;
            let upper = parse_class_byte(source, offset)?;
            if upper < lower {
                return Err(HashPatternError::InvalidPattern {
                    offset: range_offset,
                });
            }
            ranges.push((lower, upper));
        } else {
            ranges.push((lower, lower));
        }
    }
    if ranges.is_empty() {
        return Err(HashPatternError::InvalidPattern { offset: opening });
    }
    Ok(HashPatternToken::Class { negated, ranges })
}

fn parse_class_byte(source: &[u8], offset: &mut usize) -> Result<u8, HashPatternError> {
    let Some(byte) = source.get(*offset).copied() else {
        return Err(HashPatternError::InvalidPattern { offset: *offset });
    };
    if byte == b'\\' {
        let quote_offset = *offset;
        *offset += 1;
        let Some(quoted) = source.get(*offset).copied() else {
            return Err(HashPatternError::InvalidPattern {
                offset: quote_offset,
            });
        };
        *offset += 1;
        Ok(quoted)
    } else {
        *offset += 1;
        Ok(byte)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HashPatternMatchBudget {
    maximum: usize,
    used: usize,
}

impl HashPatternMatchBudget {
    pub(crate) const fn new(maximum: usize) -> Self {
        Self { maximum, used: 0 }
    }

    pub(crate) const fn used(&self) -> usize {
        self.used
    }

    fn charge(&mut self) -> Result<(), HashPatternError> {
        if self.used == self.maximum {
            return Err(HashPatternError::MatchStepLimitExceeded {
                maximum: self.maximum,
            });
        }
        self.used += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HashPatternError, HashPatternMatchBudget, HashPatternScanRequest, MAX_HASH_PATTERN_BYTES,
        MAX_HASH_PATTERN_MATCH_STEPS,
    };

    fn matches(pattern: &[u8], input: &[u8]) -> Result<bool, HashPatternError> {
        let request =
            HashPatternScanRequest::try_new(pattern, None, 1, 1, MAX_HASH_PATTERN_MATCH_STEPS)?;
        let mut budget = HashPatternMatchBudget::new(request.match_step_limit());
        request.compiled().matches(input, &mut budget)
    }

    #[test]
    fn binary_glob_grammar_is_exact() -> Result<(), Box<dyn std::error::Error>> {
        for (pattern, input, expected) in [
            (b"".as_slice(), b"".as_slice(), true),
            (b"".as_slice(), b"a".as_slice(), false),
            (b"a?c".as_slice(), b"a\0c".as_slice(), true),
            (b"a*c".as_slice(), b"abbbc".as_slice(), true),
            (b"a*c".as_slice(), b"abbbd".as_slice(), false),
            (b"[a-c]".as_slice(), b"b".as_slice(), true),
            (b"[^a-c]".as_slice(), b"z".as_slice(), true),
            (b"[]]".as_slice(), b"]".as_slice(), true),
            (b"[-a]".as_slice(), b"-".as_slice(), true),
            (b"[a-]".as_slice(), b"-".as_slice(), true),
            (br"\*\?\[".as_slice(), b"*?[".as_slice(), true),
            (b"\0*".as_slice(), b"\0binary".as_slice(), true),
        ] {
            assert_eq!(matches(pattern, input)?, expected, "{pattern:?}");
        }
        Ok(())
    }

    #[test]
    fn malformed_and_oversized_patterns_fail_before_execution() {
        for pattern in [
            b"\\".as_slice(),
            b"[".as_slice(),
            b"[a".as_slice(),
            b"[z-a]".as_slice(),
            b"[a\\]".as_slice(),
        ] {
            assert!(matches!(
                HashPatternScanRequest::try_new(pattern, None, 1, 1, 100),
                Err(HashPatternError::InvalidPattern { .. })
            ));
        }
        assert!(matches!(
            HashPatternScanRequest::try_new(
                vec![b'a'; MAX_HASH_PATTERN_BYTES + 1],
                None,
                1,
                1,
                100,
            ),
            Err(HashPatternError::PatternTooLarge { .. })
        ));
    }

    #[test]
    fn request_limits_and_match_steps_are_exact() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            HashPatternScanRequest::try_new(b"*", None, 0, 1, 1),
            Err(HashPatternError::InvalidOutputLimit { requested: 0 })
        ));
        assert!(matches!(
            HashPatternScanRequest::try_new(b"*", None, 1, 0, 1),
            Err(HashPatternError::InvalidVisitLimit { requested: 0 })
        ));
        assert!(matches!(
            HashPatternScanRequest::try_new(b"*", None, 1, 1, 0),
            Err(HashPatternError::InvalidMatchStepLimit { requested: 0 })
        ));

        let request = HashPatternScanRequest::try_new(b"*aaaaab", None, 1, 1, 1)?;
        let mut budget = HashPatternMatchBudget::new(request.match_step_limit());
        assert_eq!(
            request.compiled().matches(b"aaaaaaaa", &mut budget),
            Err(HashPatternError::MatchStepLimitExceeded { maximum: 1 })
        );
        Ok(())
    }

    #[test]
    fn compiler_derives_exact_and_prefix_routes() -> Result<(), Box<dyn std::error::Error>> {
        let exact = HashPatternScanRequest::try_new(br"user\:\*", None, 1, 1, 100)?;
        assert!(exact.compiled().is_exact_literal());
        assert_eq!(exact.compiled().leading_literal_prefix(), b"user:*");

        let prefix = HashPatternScanRequest::try_new(b"user:*", None, 1, 1, 100)?;
        assert!(!prefix.compiled().is_exact_literal());
        assert_eq!(prefix.compiled().leading_literal_prefix(), b"user:");

        let leading_wildcard = HashPatternScanRequest::try_new(b"*user", None, 1, 1, 100)?;
        assert!(
            leading_wildcard
                .compiled()
                .leading_literal_prefix()
                .is_empty()
        );
        Ok(())
    }
}
