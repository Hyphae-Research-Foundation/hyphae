// SPDX-License-Identifier: Apache-2.0

//! Cross-engine BM25F output-equivalence harness.
//!
//! The native runtime's weighted-field scorer must produce exactly the
//! legacy reference engine's ranking, quantized nano scores, match set, and
//! cardinality on identical inputs, down to the vendored msun logarithm
//! (proven bit-equal in the runtime's unit tests) and the half-up nano
//! rounding. Corpora are generated deterministically; any divergence names
//! the seed and query that produced it.

use hyphae_native_runtime::bm25f::{Bm25fDocument, Bm25fField, score_bm25f};
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{
    LexicalField, LexicalIndexDefinition, LexicalLimits, LexicalOutcome, LexicalRequest,
    retrieve_lexical,
};

const VOCABULARY: [&str; 24] = [
    "rust",
    "engine",
    "database",
    "proof",
    "search",
    "vector",
    "index",
    "durable",
    "commit",
    "snapshot",
    "receipt",
    "catalog",
    "field",
    "weight",
    "score",
    "term",
    "token",
    "Straße",
    "café",
    "naïve",
    "определение",
    "查询",
    "quick",
    "lazy",
];

fn step(seed: u64, ordinal: u64) -> u64 {
    let mut state = seed
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(ordinal.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

fn text(seed: u64, document: u64, field: u64, words: u64) -> String {
    (0..words)
        .map(|ordinal| {
            let roll = step(seed, (document << 24) ^ (field << 16) ^ ordinal);
            VOCABULARY[usize::try_from(roll % VOCABULARY.len() as u64).unwrap_or(0)]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn native_bm25f_matches_the_legacy_reference_exactly() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 0..4_u64 {
        let field_count = 1 + (step(seed, 1) % 4) as usize;
        let weights: Vec<u32> = (0..field_count)
            .map(|index| 1_000_000 + (step(seed, 100 + index as u64) % 5_000_000) as u32)
            .collect();
        let document_count = 8 + (step(seed, 2) % 24);

        let mut native_documents = Vec::new();
        let mut legacy_records = Vec::new();
        for ordinal in 0..document_count {
            let mut fields = Vec::with_capacity(field_count);
            let mut object = std::collections::BTreeMap::new();
            for field in 0..field_count {
                let words = step(seed, (ordinal << 8) ^ field as u64) % 12;
                let value = text(seed, ordinal, field as u64, words);
                object.insert(format!("field{field}"), Value::String(value.clone()));
                fields.push(value);
            }
            let key = format!("doc-{ordinal:04}").into_bytes();
            native_documents.push(Bm25fDocument {
                key: key.clone(),
                fields,
            });
            legacy_records.push(Record::new(key, Value::Object(object)));
        }

        let native_fields: Vec<Bm25fField> = weights
            .iter()
            .map(|weight| Bm25fField {
                weight_micros: *weight,
            })
            .collect();
        let legacy_definition = LexicalIndexDefinition::new(
            hyphae_core::VectorSpaceName::new("equivalence")?,
            weights
                .iter()
                .enumerate()
                .map(|(index, weight)| LexicalField {
                    path: FieldPath::field(format!("field{index}")),
                    weight_micros: *weight,
                })
                .collect(),
        )?;

        for (query_ordinal, query) in [
            "rust engine",
            "database proof search",
            "straße café",
            "查询 определение",
            "missingterm",
            "rust rust weight",
        ]
        .into_iter()
        .enumerate()
        {
            let native = score_bm25f(&native_documents, &native_fields, query, 16);
            let legacy = retrieve_lexical(
                &legacy_records,
                &legacy_definition,
                &LexicalRequest {
                    index: hyphae_core::VectorSpaceName::new("equivalence")?,
                    query: query.to_owned(),
                    limit: 16,
                },
                &LexicalLimits::default(),
            )?;
            match legacy {
                LexicalOutcome::Matches { matches, .. } => {
                    let native = native.map_err(|error| {
                        format!("seed {seed} query {query_ordinal}: native failed {error:?}")
                    })?;
                    assert_eq!(
                        native.len(),
                        matches.len(),
                        "cardinality diverged: seed {seed} query {query_ordinal}"
                    );
                    for (ours, reference) in native.iter().zip(&matches) {
                        assert_eq!(
                            ours.key, reference.key,
                            "ranking diverged: seed {seed} query {query_ordinal}"
                        );
                        assert_eq!(
                            ours.score_nanos,
                            reference.score_nanos,
                            "score diverged: seed {seed} query {query_ordinal} key {:?}",
                            String::from_utf8_lossy(&ours.key)
                        );
                    }
                }
                LexicalOutcome::Abstained(_) => {
                    let native = native.map_err(|error| {
                        format!("native failed where legacy abstained: {error:?}")
                    })?;
                    assert!(
                        native.is_empty(),
                        "native matched where legacy abstained: seed {seed} query {query_ordinal}"
                    );
                }
            }
        }
    }
    Ok(())
}
