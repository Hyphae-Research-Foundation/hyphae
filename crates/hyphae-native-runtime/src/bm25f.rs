// SPDX-License-Identifier: Apache-2.0

//! Deterministic BM25F scoring over weighted fields, ported from the legacy
//! reference engine and proven output-equivalent by the cross-engine harness.
//!
//! Tokenization is the canonical analyzer (NFKC, Unicode case fold,
//! alphanumeric split, bounded token bytes), which produces exactly the
//! legacy tokenizer-v1 term stream. Scores quantize to nanos so ranking ties
//! break bytewise on document keys, identically to the legacy engine.

use crate::analyzer::CanonicalAnalyzer;

/// Maximum weighted fields in one BM25F definition.
pub const MAX_BM25F_FIELDS: usize = 64;
/// Maximum per-field weight in micros (one thousand as a whole weight).
pub const MAX_BM25F_FIELD_WEIGHT_MICROS: u32 = 1_000_000_000;

const K1: f64 = 1.2;
const B: f64 = 0.75;
const WEIGHT_SCALE: f64 = 1_000_000.0;

/// One weighted BM25F field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bm25fField {
    /// Positive field weight in micros; one million is a whole weight.
    pub weight_micros: u32,
}

/// One document as ordered per-field texts aligned with the definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bm25fDocument {
    /// Nonempty unique binary key.
    pub key: Vec<u8>,
    /// One text per defined field, in definition order.
    pub fields: Vec<String>,
}

/// One ranked BM25F match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bm25fMatch {
    /// Document key.
    pub key: Vec<u8>,
    /// Canonical BM25F score in nanos.
    pub score_nanos: i64,
}

/// Fail-closed BM25F rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bm25fError {
    /// Empty, oversized, or zero-weight field definition.
    InvalidDefinition,
    /// A document key is empty, repeated, or misaligned with the fields.
    InvalidDocument,
    /// The query analyzes to no terms.
    EmptyQuery,
    /// A count or score exceeded its bounded range.
    ArithmeticOverflow,
}

/// Scores every document against the analyzed query and returns the bounded
/// ranking: score descending, then key ascending, truncated to `limit`.
///
/// # Errors
///
/// Returns a definition, document, query, or overflow error and never a
/// partial ranking.
#[allow(clippy::too_many_lines)]
pub fn score_bm25f(
    documents: &[Bm25fDocument],
    fields: &[Bm25fField],
    query: &str,
    limit: usize,
) -> Result<Vec<Bm25fMatch>, Bm25fError> {
    if fields.is_empty()
        || fields.len() > MAX_BM25F_FIELDS
        || fields
            .iter()
            .any(|field| !(1..=MAX_BM25F_FIELD_WEIGHT_MICROS).contains(&field.weight_micros))
    {
        return Err(Bm25fError::InvalidDefinition);
    }
    let query_tokens: std::collections::BTreeSet<String> = tokenize(query).into_iter().collect();
    if query_tokens.is_empty() {
        return Err(Bm25fError::EmptyQuery);
    }

    let mut keys = std::collections::BTreeSet::new();
    let mut analyzed = Vec::with_capacity(documents.len());
    let mut total_lengths = vec![0_u64; fields.len()];
    for document in documents {
        if document.key.is_empty()
            || document.fields.len() != fields.len()
            || !keys.insert(document.key.as_slice())
        {
            return Err(Bm25fError::InvalidDocument);
        }
        let mut field_tokens = Vec::with_capacity(fields.len());
        for (index, text) in document.fields.iter().enumerate() {
            let tokens = tokenize(text);
            let length = u64::try_from(tokens.len()).map_err(|_| Bm25fError::ArithmeticOverflow)?;
            total_lengths[index] = total_lengths[index]
                .checked_add(length)
                .ok_or(Bm25fError::ArithmeticOverflow)?;
            field_tokens.push(tokens);
        }
        analyzed.push(field_tokens);
    }

    let document_count =
        u64::try_from(documents.len()).map_err(|_| Bm25fError::ArithmeticOverflow)?;
    let averages: Vec<f64> = total_lengths
        .iter()
        .map(|length| {
            if document_count == 0 {
                0.0
            } else {
                count_as_f64(*length) / count_as_f64(document_count)
            }
        })
        .collect();

    // Document frequency counts a document once when any field contains the
    // term, matching the legacy shared-IDF definition.
    let mut frequencies = std::collections::BTreeMap::new();
    for token in &query_tokens {
        let mut count = 0_u64;
        for field_tokens in &analyzed {
            if field_tokens
                .iter()
                .any(|tokens| tokens.iter().any(|candidate| candidate == token))
            {
                count = count.checked_add(1).ok_or(Bm25fError::ArithmeticOverflow)?;
            }
        }
        frequencies.insert(token.clone(), count);
    }

    let mut matches = Vec::new();
    for (document, field_tokens) in documents.iter().zip(&analyzed) {
        let mut score_nanos = 0_i64;
        for token in &query_tokens {
            let document_frequency = frequencies[token];
            if document_frequency == 0 {
                continue;
            }
            let mut combined_tf = 0.0_f64;
            for (index, field) in fields.iter().enumerate() {
                let term_frequency = field_tokens[index]
                    .iter()
                    .filter(|candidate| *candidate == token)
                    .count();
                let term_frequency =
                    u64::try_from(term_frequency).map_err(|_| Bm25fError::ArithmeticOverflow)?;
                let field_length = u64::try_from(field_tokens[index].len()).unwrap_or(u64::MAX);
                if term_frequency > 0 && averages[index] > 0.0 {
                    let normalization = 1.0 - B + B * count_as_f64(field_length) / averages[index];
                    combined_tf += (f64::from(field.weight_micros) / WEIGHT_SCALE)
                        * count_as_f64(term_frequency)
                        / normalization;
                }
            }
            if combined_tf == 0.0 {
                continue;
            }
            let numerator = count_as_f64(document_count.saturating_sub(document_frequency)) + 0.5;
            let denominator = count_as_f64(document_frequency) + 0.5;
            let idf = log_e(1.0 + numerator / denominator);
            let term_score = quantize_score(idf * combined_tf * (K1 + 1.0) / (combined_tf + K1))?;
            score_nanos = score_nanos
                .checked_add(term_score)
                .ok_or(Bm25fError::ArithmeticOverflow)?;
        }
        if score_nanos > 0 {
            matches.push(Bm25fMatch {
                key: document.key.clone(),
                score_nanos,
            });
        }
    }
    matches.sort_by(|left, right| {
        right
            .score_nanos
            .cmp(&left.score_nanos)
            .then_with(|| left.key.cmp(&right.key))
    });
    matches.truncate(limit);
    Ok(matches)
}

// The natural logarithm exactly as the legacy reference computes it: the
// musl-derived implementation from FreeBSD msun `e_log.c` (Sun Microsystems,
// 1993, freely-grantable notice preserved in the musl and libm sources),
// as published by the `libm` crate. Platform `ln` intrinsics differ in the
// last ulp, which the nano quantization would surface as one-nano drift
// against the legacy engine.
#[allow(clippy::excessive_precision)]
const LN2_HI: f64 = 6.931_471_803_691_238_2e-1;
const LN2_LO: f64 = 1.908_214_929_270_587_7e-10;
const LG1: f64 = 6.666_666_666_666_735e-1;
#[allow(clippy::excessive_precision)]
const LG2: f64 = 3.999_999_999_940_941_9e-1;
const LG3: f64 = 2.857_142_874_366_239e-1;
const LG4: f64 = 2.222_219_843_214_978_4e-1;
const LG5: f64 = 1.818_357_216_161_805e-1;
const LG6: f64 = 1.531_383_769_920_937_3e-1;
const LG7: f64 = 1.479_819_860_511_658_6e-1;

#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cast_possible_wrap)]
#[allow(clippy::excessive_precision)]
#[allow(clippy::eq_op)]
fn log_e(mut x: f64) -> f64 {
    let x1p54 = f64::from_bits(0x4350_0000_0000_0000);
    let mut ui = x.to_bits();
    let mut hx: u32 = (ui >> 32) as u32;
    let mut k: i32 = 0;
    if (hx < 0x0010_0000) || ((hx >> 31) != 0) {
        if ui << 1 == 0 {
            return -1.0 / (x * x);
        }
        if hx >> 31 != 0 {
            return (x - x) / 0.0;
        }
        k -= 54;
        x *= x1p54;
        ui = x.to_bits();
        hx = (ui >> 32) as u32;
    } else if hx >= 0x7ff0_0000 {
        return x;
    } else if hx == 0x3ff0_0000 && ui << 32 == 0 {
        return 0.0;
    }
    hx += 0x3ff0_0000 - 0x3fe6_a09e;
    k += ((hx >> 20) as i32) - 0x3ff;
    hx = (hx & 0x000f_ffff) + 0x3fe6_a09e;
    ui = (u64::from(hx) << 32) | (ui & 0xffff_ffff);
    x = f64::from_bits(ui);

    let f: f64 = x - 1.0;
    let hfsq: f64 = 0.5 * f * f;
    let s: f64 = f / (2.0 + f);
    let z: f64 = s * s;
    let w: f64 = z * z;
    let t1: f64 = w * (LG2 + w * (LG4 + w * LG6));
    let t2: f64 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    let r: f64 = t2 + t1;
    let dk: f64 = f64::from(k);
    s * (hfsq + r) + dk * LN2_LO - hfsq + f + dk * LN2_HI
}

fn tokenize(text: &str) -> Vec<String> {
    CanonicalAnalyzer::analyze(text)
        .tokens
        .into_iter()
        .map(|token| token.term)
        .collect()
}

#[allow(clippy::cast_precision_loss)]
const fn count_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
const fn maximum_i64_as_f64() -> f64 {
    i64::MAX as f64
}

#[allow(clippy::cast_possible_truncation)]
fn quantize_score(value: f64) -> Result<i64, Bm25fError> {
    if !value.is_finite() || value < 0.0 {
        return Err(Bm25fError::ArithmeticOverflow);
    }
    let scaled = value * 1_000_000_000.0;
    if !scaled.is_finite() {
        return Err(Bm25fError::ArithmeticOverflow);
    }
    if scaled >= maximum_i64_as_f64() {
        return Ok(i64::MAX);
    }
    Ok((scaled + 0.5).floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(key: &[u8], fields: &[&str]) -> Bm25fDocument {
        Bm25fDocument {
            key: key.to_vec(),
            fields: fields.iter().map(|text| (*text).to_owned()).collect(),
        }
    }

    #[test]
    fn field_weights_reorder_matches_deterministically() -> Result<(), Bm25fError> {
        let documents = [
            document(b"a", &["rust engine", "unrelated body text here"]),
            document(b"b", &["unrelated title", "rust engine rust engine body"]),
        ];
        let title_heavy = [
            Bm25fField {
                weight_micros: 5_000_000,
            },
            Bm25fField {
                weight_micros: 1_000_000,
            },
        ];
        let body_heavy = [
            Bm25fField {
                weight_micros: 1_000_000,
            },
            Bm25fField {
                weight_micros: 5_000_000,
            },
        ];
        let ranked_title = score_bm25f(&documents, &title_heavy, "rust", 2)?;
        let ranked_body = score_bm25f(&documents, &body_heavy, "rust", 2)?;
        assert_eq!(ranked_title[0].key, b"a");
        assert_eq!(ranked_body[0].key, b"b");
        Ok(())
    }

    #[test]
    fn vendored_log_matches_the_reference_bits() {
        assert_eq!(
            log_e(5.428_571_428_571_429).to_bits(),
            0x3ffb_111a_dd54_28fa
        );
        assert_eq!(log_e(1.0).to_bits(), 0);
        assert_eq!(log_e(2.0).to_bits(), std::f64::consts::LN_2.to_bits());
    }

    #[test]
    fn invalid_shapes_fail_closed() {
        let fields = [Bm25fField {
            weight_micros: 1_000_000,
        }];
        assert_eq!(
            score_bm25f(&[], &[], "rust", 4),
            Err(Bm25fError::InvalidDefinition)
        );
        assert_eq!(
            score_bm25f(&[], &fields, " -- ", 4),
            Err(Bm25fError::EmptyQuery)
        );
        assert_eq!(
            score_bm25f(&[document(b"", &["x"])], &fields, "rust", 4),
            Err(Bm25fError::InvalidDocument)
        );
        assert_eq!(
            score_bm25f(
                &[document(b"a", &["x"]), document(b"a", &["y"])],
                &fields,
                "rust",
                4
            ),
            Err(Bm25fError::InvalidDocument)
        );
    }
}
