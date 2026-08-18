<!-- SPDX-License-Identifier: Apache-2.0 -->
# Lexical retrieval reference semantics v1

Status: normative for Hyphae `0.2`.

## Index definition

A lexical index has a canonical identifier and one or more unique field
paths. Each field has a positive `weight_micros` in `1..=1_000_000_000`.
Field order is canonical path-byte order. Version 1 fixes `k1 = 1.2` and
`b = 0.75`.

## Tokenization

Configured string fields are normalized by `hyphae-unicode-tokenizer-v1`:
NFKC, Unicode default case folding, then maximal Unicode-alphanumeric runs.
Tokens over 256 UTF-8 bytes are discarded. Arrays, objects, integers, bytes,
booleans, null, and missing fields contribute no text.

The query uses the same tokenizer. Repeated query tokens are evaluated once in
canonical token-byte order. An empty normalized query is invalid.

## BM25F

For query term `t`, document `d`, and configured field `f`:

```text
normalized_tf_f =
  weight_f * tf(t, d, f)
  / (1 - b + b * field_length(d, f) / average_field_length(f))

combined_tf = sum(normalized_tf_f)
idf = ln(1 + (document_count - document_frequency(t) + 0.5)
             / (document_frequency(t) + 0.5))
term_score = idf * combined_tf * (k1 + 1) / (combined_tf + k1)
```

An empty field has length zero. If the corpus average for a field is zero, that
field contributes zero. Document frequency counts a document once when the
term occurs in any configured field.

The reference implementation uses the pinned pure-Rust logarithm and
round-half-away-from-zero to expose `score_nanos`. The finite nonnegative sum
is clamped to signed-i64 maximum before conversion. Ranking uses
`score_nanos` descending and binary object key ascending.

## Outcomes and limits

No matching documents returns typed `no_candidates` abstention. Invalid
definition/query, stale materialization, document/token/candidate budget, or
timeout is an error with no partial result.

The scanned-document count, matched-document count, canonical token list, and
per-result term/field contributions are available for proof reexecution and
explanation.
