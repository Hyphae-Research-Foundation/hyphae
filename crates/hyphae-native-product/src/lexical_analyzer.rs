// SPDX-License-Identifier: Apache-2.0

//! Deterministic lexical analyzer pipeline for integrated collections.
//!
//! The configurable pipeline runs as a text-to-text transform at the product
//! boundary, at ingest and at query, before the runtime's canonical analyzer
//! (NFKC, Unicode case fold, alphanumeric tokenization). The canonical
//! specification — `UnicodeWord` with exactly the `Lowercase` filter, or no
//! analyzer at all — is the identity transform, so existing collections keep
//! their exact durable bytes. Every version-suffixed stage is frozen: a new
//! word list or stemmer revision is a new filter variant, never a mutation.

use hyphae_native_catalog::{AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer};
use hyphae_native_runtime::CanonicalAnalyzer;

/// English stop words, version one. Frozen: the classic 33-word list.
const STOP_WORDS_EN_V1: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Configured non-identity lexical transform stages, in application order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LexicalTransform {
    /// Latin diacritic folding over the frozen version-one table.
    pub(crate) fold_ascii: bool,
    /// English stop-word removal, version one.
    pub(crate) stop_en: bool,
    /// English Porter stemming, version one.
    pub(crate) stem_en: bool,
}

impl LexicalTransform {
    /// Derives the transform from one analyzer definition, or `None` when
    /// the definition is the canonical identity. Fails closed on any shape
    /// the transform cannot honor exactly.
    pub(crate) fn from_definition(
        definition: &AnalyzerDefinition,
    ) -> Result<Option<Self>, LexicalAnalyzerError> {
        if definition.tokenizer != AnalyzerTokenizer::UnicodeWord {
            return Err(LexicalAnalyzerError::UnsupportedTokenizer);
        }
        let mut filters = definition.filters.iter().copied().peekable();
        if filters.next() != Some(AnalyzerFilter::Lowercase) {
            // The canonical analyzer always case folds; a pipeline without
            // Lowercase would promise case sensitivity it cannot deliver.
            return Err(LexicalAnalyzerError::LowercaseIsRequired);
        }
        let mut transform = Self {
            fold_ascii: false,
            stop_en: false,
            stem_en: false,
        };
        let mut previous = AnalyzerFilter::Lowercase;
        for filter in filters {
            if filter <= previous {
                return Err(LexicalAnalyzerError::FiltersOutOfOrder);
            }
            previous = filter;
            match filter {
                AnalyzerFilter::Lowercase => unreachable!("ordered filters exclude duplicates"),
                AnalyzerFilter::AsciiFolding => transform.fold_ascii = true,
                AnalyzerFilter::EnglishStopV1 => transform.stop_en = true,
                AnalyzerFilter::EnglishStemV1 => transform.stem_en = true,
            }
        }
        if transform.fold_ascii || transform.stop_en || transform.stem_en {
            Ok(Some(transform))
        } else {
            Ok(None)
        }
    }

    /// Applies the pipeline: canonical analysis, then per-term folding,
    /// stop-word removal, and stemming, rejoined with single spaces. The
    /// output re-analyzes canonically to exactly the intended terms.
    pub(crate) fn apply(self, text: &str) -> String {
        let analysis = CanonicalAnalyzer::analyze(text);
        let mut output = String::with_capacity(text.len());
        for token in analysis.tokens {
            let mut term = token.term;
            if self.fold_ascii {
                term = fold_ascii_v1(&term);
            }
            if self.stop_en && STOP_WORDS_EN_V1.binary_search(&term.as_str()).is_ok() {
                continue;
            }
            if self.stem_en {
                term = porter_stem_v1(&term);
            }
            if term.is_empty() {
                continue;
            }
            if !output.is_empty() {
                output.push(' ');
            }
            output.push_str(&term);
        }
        output
    }
}

/// Fail-closed analyzer-shape rejections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LexicalAnalyzerError {
    /// Only the Unicode word tokenizer preserves canonical term boundaries.
    UnsupportedTokenizer,
    /// The canonical analyzer always case folds.
    LowercaseIsRequired,
    /// Filters must be declared in ascending canonical order.
    FiltersOutOfOrder,
}

/// Latin diacritic folding, version one. A frozen explicit table over the
/// Latin-1 Supplement and Latin Extended-A blocks; everything else passes
/// through unchanged. Input terms are already NFKC case-folded.
fn fold_ascii_v1(term: &str) -> String {
    let mut output = String::with_capacity(term.len());
    for character in term.chars() {
        match character {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => output.push('a'),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => output.push('c'),
            'ď' | 'đ' | 'ð' => output.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => output.push('e'),
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => output.push('g'),
            'ĥ' | 'ħ' => output.push('h'),
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => output.push('i'),
            'ĵ' => output.push('j'),
            'ķ' => output.push('k'),
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => output.push('l'),
            'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => output.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => output.push('o'),
            'ŕ' | 'ŗ' | 'ř' => output.push('r'),
            'ś' | 'ŝ' | 'ş' | 'š' => output.push('s'),
            'ţ' | 'ť' | 'ŧ' => output.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => output.push('u'),
            'ŵ' => output.push('w'),
            'ý' | 'ÿ' | 'ŷ' => output.push('y'),
            'ź' | 'ż' | 'ž' => output.push('z'),
            'æ' => output.push_str("ae"),
            'œ' => output.push_str("oe"),
            'þ' => output.push_str("th"),
            other => output.push(other),
        }
    }
    output
}

/// Porter stemming algorithm, version one — the classic 1980 definition.
/// Non-ASCII-alphabetic terms and terms of two characters or fewer pass
/// through unchanged.
#[allow(clippy::too_many_lines)]
#[allow(clippy::items_after_statements)]
fn porter_stem_v1(term: &str) -> String {
    if term.len() <= 2 || !term.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return term.to_owned();
    }
    fn is_consonant(word: &[u8], index: usize) -> bool {
        match word[index] {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => index == 0 || !is_consonant(word, index - 1),
            _ => true,
        }
    }

    /// The measure `m` of the stem `word[..=end]`.
    fn measure(word: &[u8], end: usize) -> usize {
        let mut m = 0;
        let mut index = 0;
        while index <= end && is_consonant(word, index) {
            index += 1;
        }
        loop {
            if index > end {
                return m;
            }
            while index <= end && !is_consonant(word, index) {
                index += 1;
            }
            if index > end {
                return m;
            }
            m += 1;
            while index <= end && is_consonant(word, index) {
                index += 1;
            }
        }
    }

    fn has_vowel(word: &[u8], end: usize) -> bool {
        (0..=end).any(|index| !is_consonant(word, index))
    }

    fn double_consonant(word: &[u8]) -> bool {
        let length = word.len();
        length >= 2 && word[length - 1] == word[length - 2] && is_consonant(word, length - 1)
    }

    /// Consonant-vowel-consonant ending where the final consonant is not
    /// `w`, `x`, or `y`.
    fn cvc(word: &[u8]) -> bool {
        let length = word.len();
        length >= 3
            && is_consonant(word, length - 1)
            && !is_consonant(word, length - 2)
            && is_consonant(word, length - 3)
            && !matches!(word[length - 1], b'w' | b'x' | b'y')
    }

    fn ends_with(word: &[u8], suffix: &[u8]) -> bool {
        word.len() > suffix.len() && word.ends_with(suffix)
    }

    // Steps 2 through 4 share one replacement engine.
    fn replace(word: &mut Vec<u8>, suffix: &[u8], replacement: &[u8], minimum: usize) -> bool {
        if word.len() > suffix.len() && word.ends_with(suffix) {
            let stem_end = word.len() - suffix.len() - 1;
            if measure(word, stem_end) > minimum {
                word.truncate(word.len() - suffix.len());
                word.extend_from_slice(replacement);
                return true;
            }
            return true;
        }
        false
    }

    let mut word: Vec<u8> = term.as_bytes().to_vec();

    // Step 1a.
    if word.ends_with(b"sses") || word.ends_with(b"ies") {
        word.truncate(word.len() - 2);
    } else if word.ends_with(b"s") && !word.ends_with(b"ss") {
        word.truncate(word.len() - 1);
    }

    // Step 1b.
    let mut cleanup = false;
    if ends_with(&word, b"eed") {
        if measure(&word, word.len() - 4) > 0 {
            word.truncate(word.len() - 1);
        }
    } else if ends_with(&word, b"ed") && has_vowel(&word, word.len() - 3) {
        word.truncate(word.len() - 2);
        cleanup = true;
    } else if ends_with(&word, b"ing") && has_vowel(&word, word.len() - 4) {
        word.truncate(word.len() - 3);
        cleanup = true;
    }
    if cleanup {
        if word.ends_with(b"at") || word.ends_with(b"bl") || word.ends_with(b"iz") {
            word.push(b'e');
        } else if double_consonant(&word) && !matches!(word[word.len() - 1], b'l' | b's' | b'z') {
            word.truncate(word.len() - 1);
        } else if measure(&word, word.len() - 1) == 1 && cvc(&word) {
            word.push(b'e');
        }
    }

    // Step 1c.
    if ends_with(&word, b"y") && has_vowel(&word, word.len() - 2) {
        let last = word.len() - 1;
        word[last] = b'i';
    }

    const STEP2: [(&[u8], &[u8]); 20] = [
        (b"ational", b"ate"),
        (b"tional", b"tion"),
        (b"enci", b"ence"),
        (b"anci", b"ance"),
        (b"izer", b"ize"),
        (b"abli", b"able"),
        (b"alli", b"al"),
        (b"entli", b"ent"),
        (b"eli", b"e"),
        (b"ousli", b"ous"),
        (b"ization", b"ize"),
        (b"ation", b"ate"),
        (b"ator", b"ate"),
        (b"alism", b"al"),
        (b"iveness", b"ive"),
        (b"fulness", b"ful"),
        (b"ousness", b"ous"),
        (b"aliti", b"al"),
        (b"iviti", b"ive"),
        (b"biliti", b"ble"),
    ];
    for (suffix, replacement) in STEP2 {
        if replace(&mut word, suffix, replacement, 0) {
            break;
        }
    }

    const STEP3: [(&[u8], &[u8]); 7] = [
        (b"icate", b"ic"),
        (b"ative", b""),
        (b"alize", b"al"),
        (b"iciti", b"ic"),
        (b"ical", b"ic"),
        (b"ful", b""),
        (b"ness", b""),
    ];
    for (suffix, replacement) in STEP3 {
        if replace(&mut word, suffix, replacement, 0) {
            break;
        }
    }

    const STEP4: [&[u8]; 18] = [
        b"al", b"ance", b"ence", b"er", b"ic", b"able", b"ible", b"ant", b"ement", b"ment", b"ent",
        b"ou", b"ism", b"ate", b"iti", b"ous", b"ive", b"ize",
    ];
    for suffix in STEP4 {
        if word.len() > suffix.len() && word.ends_with(suffix) {
            let stem_end = word.len() - suffix.len() - 1;
            if measure(&word, stem_end) > 1 {
                word.truncate(word.len() - suffix.len());
            }
            break;
        }
    }
    // The `ion` suffix requires a preceding `s` or `t`.
    if ends_with(&word, b"ion") {
        let stem_len = word.len() - 3;
        if stem_len >= 1
            && matches!(word[stem_len - 1], b's' | b't')
            && measure(&word, stem_len - 1) > 1
        {
            word.truncate(stem_len);
        }
    }

    // Step 5a.
    if ends_with(&word, b"e") {
        let stem_end = word.len() - 2;
        let m = measure(&word, stem_end);
        if m > 1 || (m == 1 && !cvc(&word[..word.len() - 1])) {
            word.truncate(word.len() - 1);
        }
    }
    // Step 5b.
    if word.len() >= 2
        && word[word.len() - 1] == b'l'
        && double_consonant(&word)
        && measure(&word, word.len() - 1) > 1
    {
        word.truncate(word.len() - 1);
    }

    String::from_utf8(word).unwrap_or_else(|_| term.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_word_table_is_sorted_for_binary_search() {
        let mut sorted = STOP_WORDS_EN_V1;
        sorted.sort_unstable();
        assert_eq!(sorted, STOP_WORDS_EN_V1);
    }

    #[test]
    fn porter_stems_match_the_published_reference_samples() {
        for (word, stem) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("caress", "caress"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("bled", "bled"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("tanned", "tan"),
            ("falling", "fall"),
            ("hissing", "hiss"),
            ("fizzed", "fizz"),
            ("failing", "fail"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("valenci", "valenc"),
            ("digitizer", "digit"),
            ("operator", "oper"),
            ("feudalism", "feudal"),
            ("decisiveness", "decis"),
            ("hopefulness", "hope"),
            ("formaliti", "formal"),
            ("triplicate", "triplic"),
            ("formative", "form"),
            ("formalize", "formal"),
            ("electriciti", "electr"),
            ("electrical", "electr"),
            ("hopeful", "hope"),
            ("goodness", "good"),
            ("revival", "reviv"),
            ("allowance", "allow"),
            ("inference", "infer"),
            ("airliner", "airlin"),
            ("adjustment", "adjust"),
            ("dependent", "depend"),
            ("adoption", "adopt"),
            ("activate", "activ"),
            ("angulariti", "angular"),
            ("effective", "effect"),
            ("bowdlerize", "bowdler"),
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
            ("running", "run"),
            ("databases", "databas"),
        ] {
            assert_eq!(porter_stem_v1(word), stem, "stem of {word}");
        }
    }

    #[test]
    fn transform_folds_stops_and_stems_deterministically() {
        let transform = LexicalTransform {
            fold_ascii: true,
            stop_en: true,
            stem_en: true,
        };
        assert_eq!(
            transform.apply("The running dogs are chasing the café's ponies!"),
            "run dog chase cafe s poni"
        );
        assert_eq!(transform.apply("the and of"), "");
    }
}
