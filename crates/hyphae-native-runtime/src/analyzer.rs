// SPDX-License-Identifier: AGPL-3.0-only

//! Canonical, provider-free text analysis for native lexical indexes.

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// Stable name of the native canonical analyzer.
pub const CANONICAL_ANALYZER_NAME: &str = "hyphae.native.lexical";
/// Semantic version of the native canonical analyzer.
pub const CANONICAL_ANALYZER_VERSION: u32 = 1;
/// Maximum UTF-8 byte length of a retained canonical token.
pub const MAX_CANONICAL_TOKEN_BYTES: usize = 256;

const CANONICAL_ANALYZER_SPEC: &[u8] = b"hyphae.native.lexical\0v1\0nfkc\0unicode-default-case-fold\0alphanumeric\0max-token-bytes=256\0oversized=discard";

/// Stable identity used to detect analyzer incompatibility in durable metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalyzerIdentity {
    /// Stable analyzer name.
    pub name: &'static str,
    /// Semantic analyzer version.
    pub version: u32,
    /// BLAKE3 digest of the complete analyzer contract.
    pub digest: [u8; 32],
}

/// One retained canonical token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzedToken {
    /// NFKC-normalized and Unicode-case-folded term.
    pub term: String,
    /// Zero-based token position, including gaps left by discarded oversized tokens.
    pub position: usize,
    /// Inclusive UTF-8 byte offset in [`Analysis::normalized_text`].
    pub start_offset: usize,
    /// Exclusive UTF-8 byte offset in [`Analysis::normalized_text`].
    pub end_offset: usize,
}

/// Complete canonical analysis of one UTF-8 string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Analysis {
    /// NFKC-normalized and Unicode-case-folded text used for token offsets.
    pub normalized_text: String,
    /// Retained tokens in position order.
    pub tokens: Vec<AnalyzedToken>,
}

/// Stateless canonical analyzer for the native runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalAnalyzer;

impl CanonicalAnalyzer {
    /// Returns the durable identity of this analyzer contract.
    #[must_use]
    pub fn identity() -> AnalyzerIdentity {
        AnalyzerIdentity {
            name: CANONICAL_ANALYZER_NAME,
            version: CANONICAL_ANALYZER_VERSION,
            digest: *blake3::hash(CANONICAL_ANALYZER_SPEC).as_bytes(),
        }
    }

    /// Applies NFKC, Unicode default case folding, and alphanumeric tokenization.
    ///
    /// Tokens longer than [`MAX_CANONICAL_TOKEN_BYTES`] are discarded as a
    /// unit. Their positions remain reserved so phrase positions cannot become
    /// adjacent because a bounded token was omitted.
    #[must_use]
    pub fn analyze(text: &str) -> Analysis {
        let normalized_text: String = text.nfkc().case_fold().collect();
        let mut tokens = Vec::new();
        let mut token_start = None;
        let mut position = 0_usize;

        for (offset, character) in normalized_text.char_indices() {
            if character.is_alphanumeric() {
                token_start.get_or_insert(offset);
            } else if let Some(start_offset) = token_start.take() {
                push_token(
                    &normalized_text,
                    &mut tokens,
                    &mut position,
                    start_offset,
                    offset,
                );
            }
        }
        if let Some(start_offset) = token_start {
            push_token(
                &normalized_text,
                &mut tokens,
                &mut position,
                start_offset,
                normalized_text.len(),
            );
        }

        Analysis {
            normalized_text,
            tokens,
        }
    }
}

fn push_token(
    text: &str,
    tokens: &mut Vec<AnalyzedToken>,
    position: &mut usize,
    start_offset: usize,
    end_offset: usize,
) {
    if end_offset - start_offset <= MAX_CANONICAL_TOKEN_BYTES {
        tokens.push(AnalyzedToken {
            term: text[start_offset..end_offset].to_owned(),
            position: *position,
            start_offset,
            end_offset,
        });
    }
    *position += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_analysis_matches_golden() {
        let analysis = CanonicalAnalyzer::analyze("Straße ＨＥＬＬＯ cafe\u{301} ①");

        assert_eq!(analysis.normalized_text, "strasse hello café 1");
        assert_eq!(
            analysis.tokens,
            [
                token("strasse", 0, 0, 7),
                token("hello", 1, 8, 13),
                token("café", 2, 14, 19),
                token("1", 3, 20, 21),
            ]
        );
    }

    #[test]
    fn identity_digest_is_stable_golden() {
        assert_eq!(
            blake3::Hash::from(CanonicalAnalyzer::identity().digest)
                .to_hex()
                .as_str(),
            "1e2706a2234e27ad43b75d836f81e2cd65971877b76e86e0d0cf36542b8714de"
        );
    }

    #[test]
    fn case_folding_is_not_simple_lowercasing() {
        assert_eq!("Straße".to_lowercase(), "straße");
        assert_eq!(
            CanonicalAnalyzer::analyze("Straße").tokens[0].term,
            "strasse"
        );
    }

    #[test]
    fn nfkc_is_not_only_canonical_composition() {
        assert_eq!(CanonicalAnalyzer::analyze("Ｈ①").tokens[0].term, "h1");
        assert_ne!("Ｈ①".nfc().collect::<String>(), "H1");
    }

    #[test]
    fn oversized_tokens_are_discarded_without_collapsing_positions() {
        let input = format!("{} ok", "a".repeat(MAX_CANONICAL_TOKEN_BYTES + 1));
        let analysis = CanonicalAnalyzer::analyze(&input);

        assert_eq!(analysis.tokens, [token("ok", 1, 258, 260)]);
    }

    #[test]
    fn punctuation_does_not_create_empty_tokens() {
        let analysis = CanonicalAnalyzer::analyze(" -- \t… ");

        assert!(analysis.tokens.is_empty());
    }

    fn token(term: &str, position: usize, start_offset: usize, end_offset: usize) -> AnalyzedToken {
        AnalyzedToken {
            term: term.to_owned(),
            position,
            start_offset,
            end_offset,
        }
    }
}
