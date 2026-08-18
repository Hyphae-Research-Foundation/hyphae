// SPDX-License-Identifier: Apache-2.0

//! Generates the canonical retrieval and proof vectors checked into
//! `compatibility/v2/retrieval-golden-v1.json`.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use hyphae_core::{Q15Vector, VectorSpaceName};
use hyphae_engine::{
    EXACT_RETRIEVAL_SEMANTICS_VERSION, ExactRetrievalProof, HYBRID_RETRIEVAL_SEMANTICS_VERSION,
    HybridRetrievalProof, LEXICAL_RETRIEVAL_SEMANTICS_VERSION, LexicalRetrievalProof,
    RETRIEVAL_PROOF_FORMAT_VERSION,
};
use hyphae_query::{FieldPath, Record, Value};
use hyphae_retrieval::{
    DurableVectorRecord, ExactRetrievalLimits, ExactRetrievalOutcome, ExactRetrievalRequest,
    HybridRequest, LexicalField, LexicalIndexDefinition, LexicalLimits, LexicalOutcome,
    LexicalRequest, fuse_hybrid, retrieve_exact, retrieve_lexical, tokenize_v1,
};
use hyphae_storage::SnapshotInfo;
use serde_json::{Value as JsonValue, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(&build_golden()?)?);
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn build_golden() -> Result<JsonValue, Box<dyn std::error::Error>> {
    let snapshot = golden_snapshot();
    let vector_space = VectorSpaceName::new("semantic")?;
    let vector_request = ExactRetrievalRequest {
        vector_space,
        query: Q15Vector::new(vec![32_767, 0])?,
        limit: 3,
        minimum_score_nanos: -1_000_000_000,
        minimum_margin_nanos: 0,
    };
    let vector_candidates = vec![
        DurableVectorRecord {
            key: b"alpha".to_vec(),
            vector: Q15Vector::new(vec![32_767, 0])?,
        },
        DurableVectorRecord {
            key: b"beta".to_vec(),
            vector: Q15Vector::new(vec![0, 32_767])?,
        },
        DurableVectorRecord {
            key: b"gamma".to_vec(),
            vector: Q15Vector::new(vec![-32_767, 0])?,
        },
    ];
    let vector_outcome = retrieve_exact(
        &vector_candidates,
        &vector_request,
        &ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 1_024,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        },
    )?;
    let near_tie_request = ExactRetrievalRequest {
        vector_space: VectorSpaceName::new("sparse-near-tie")?,
        query: Q15Vector::new(vec![32_767, 0, 0, 0, 0, 0, 0, 0])?,
        limit: 2,
        minimum_score_nanos: -1_000_000_000,
        minimum_margin_nanos: 0,
    };
    let near_tie_candidates = vec![
        DurableVectorRecord {
            key: b"near-a".to_vec(),
            vector: Q15Vector::new(vec![32_767, 16, 0, 0, 0, 0, 0, 0])?,
        },
        DurableVectorRecord {
            key: b"near-b".to_vec(),
            vector: Q15Vector::new(vec![32_767, 32, 0, 0, 0, 0, 0, 0])?,
        },
    ];
    let near_tie_outcome = retrieve_exact(
        &near_tie_candidates,
        &near_tie_request,
        &ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 1_024,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        },
    )?;
    let ExactRetrievalOutcome::Matches {
        matches: near_tie_matches,
        ..
    } = near_tie_outcome
    else {
        return Err("near-tie golden unexpectedly abstained".into());
    };

    let lexical_definition = LexicalIndexDefinition::new(
        VectorSpaceName::new("content")?,
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
    )?;
    let lexical_request = LexicalRequest {
        index: lexical_definition.name.clone(),
        query: "DURABLE memory".into(),
        limit: 3,
    };
    let records = vec![
        record(b"alpha", "Durable memory", "offline agent memory"),
        record(b"beta", "Fast search", "exact vector retrieval"),
        record(b"gamma", "Memory systems", "durable storage"),
    ];
    let lexical_outcome = retrieve_lexical(
        &records,
        &lexical_definition,
        &lexical_request,
        &LexicalLimits {
            max_documents: 10,
            max_tokens: 1_000,
            max_candidates: 10,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        },
    )?;
    let hybrid_request = HybridRequest {
        lexical_weight: 1,
        vector_weight: 1,
        limit: 3,
    };
    let hybrid_outcome = fuse_hybrid(&lexical_outcome, &vector_outcome, &hybrid_request)?;

    let exact_proof =
        ExactRetrievalProof::new(&snapshot, vector_request.clone(), vector_outcome.clone())?;
    let lexical_proof =
        LexicalRetrievalProof::new(&snapshot, lexical_request.clone(), lexical_outcome.clone())?;
    let hybrid_proof = HybridRetrievalProof::new(
        &snapshot,
        lexical_request.clone(),
        lexical_outcome.clone(),
        vector_request.clone(),
        vector_outcome.clone(),
        hybrid_request.clone(),
        hybrid_outcome.clone(),
    )?;
    let exact_proof_bytes = exact_proof.to_bytes()?;
    let lexical_proof_bytes = lexical_proof.to_bytes()?;
    let hybrid_proof_bytes = hybrid_proof.to_bytes()?;

    Ok(json!({
        "schema": "hyphae-retrieval-golden-v1",
        "score_scale": 1_000_000_000_u64,
        "proof_format_version": RETRIEVAL_PROOF_FORMAT_VERSION,
        "semantics_versions": {
            "exact": EXACT_RETRIEVAL_SEMANTICS_VERSION,
            "lexical": LEXICAL_RETRIEVAL_SEMANTICS_VERSION,
            "hybrid": HYBRID_RETRIEVAL_SEMANTICS_VERSION
        },
        "tokenizer": [
            {
                "input": "Straße STRASSE Café CAFE\u{301}",
                "tokens": tokenize_v1("Straße STRASSE Café CAFE\u{301}")
            },
            {
                "input": "東京-memory_42",
                "tokens": tokenize_v1("東京-memory_42")
            }
        ],
        "invalid_q15": [
            {"name": "empty-vector", "values": []},
            {"name": "zero-vector", "values": [0, 0]},
            {"name": "dimension-mismatch", "query": [32767, 0], "candidate": [32767]}
        ],
        "exact": [
            {
                "name": "orthogonal-and-opposite",
                "query": [32767, 0],
                "candidates": [
                    {"key_hex": "61", "vector": [32767, 0]},
                    {"key_hex": "62", "vector": [0, 32767]},
                    {"key_hex": "63", "vector": [-32767, 0]}
                ],
                "ordered": [
                    {"key_hex": "61", "score_nanos": 1_000_000_000},
                    {"key_hex": "62", "score_nanos": 0},
                    {"key_hex": "63", "score_nanos": -1_000_000_000}
                ]
            },
            {
                "name": "binary-key-tie",
                "query": [1, 1],
                "candidates": [
                    {"key_hex": "ff", "vector": [7, 7]},
                    {"key_hex": "00", "vector": [2, 2]}
                ],
                "ordered": [
                    {"key_hex": "00", "score_nanos": 1_000_000_000},
                    {"key_hex": "ff", "score_nanos": 1_000_000_000}
                ]
            },
            {
                "name": "sparse-near-tie",
                "query": near_tie_request.query.as_slice(),
                "candidates": near_tie_candidates.iter().map(|candidate| json!({
                    "key_hex": encode_hex(&candidate.key),
                    "vector": candidate.vector.as_slice()
                })).collect::<Vec<_>>(),
                "ordered": near_tie_matches.iter().map(|matched| json!({
                    "key_hex": encode_hex(&matched.key),
                    "score_nanos": matched.score_nanos
                })).collect::<Vec<_>>()
            }
        ],
        "lexical": lexical_json(&lexical_definition, &lexical_request, &records, &lexical_outcome),
        "hybrid": hybrid_json(
            &hybrid_request,
            &vector_request,
            &vector_candidates,
            &hybrid_outcome
        ),
        "proofs": {
            "exact": proof_json(
                &exact_proof_bytes,
                exact_proof.proof_digest(),
                exact_proof.anchor_digest()
            ),
            "lexical": proof_json(
                &lexical_proof_bytes,
                lexical_proof.proof_digest(),
                lexical_proof.anchor_digest()
            ),
            "hybrid": proof_json(
                &hybrid_proof_bytes,
                hybrid_proof.proof_digest(),
                hybrid_proof.anchor_digest()
            )
        }
    }))
}

fn golden_snapshot() -> SnapshotInfo {
    SnapshotInfo {
        path: PathBuf::from("witness.hysnap"),
        disk_format_version: 2,
        checkpoint_sequence: 7,
        checkpoint_digest: Some([0x11; 32]),
        entry_count: 3,
        vector_space_count: 1,
        vector_count: 3,
        lexical_index_count: 1,
        receipt_count: 7,
        snapshot_digest: [0x22; 32],
        file_bytes: 1_024,
    }
}

fn record(key: &[u8], title: &str, body: &str) -> Record {
    Record::new(
        key,
        Value::Object(BTreeMap::from([
            ("body".into(), Value::String(body.into())),
            ("title".into(), Value::String(title.into())),
        ])),
    )
}

fn proof_json(bytes: &[u8], proof_digest: [u8; 32], anchor_digest: [u8; 32]) -> JsonValue {
    json!({
        "encoding": "hex",
        "bytes": encode_hex(bytes),
        "proof_digest": encode_hex(&proof_digest),
        "anchor_digest": encode_hex(&anchor_digest)
    })
}

fn lexical_json(
    definition: &LexicalIndexDefinition,
    request: &LexicalRequest,
    records: &[Record],
    outcome: &LexicalOutcome,
) -> JsonValue {
    match outcome {
        LexicalOutcome::Matches {
            matches,
            scanned_documents,
            matched_documents,
            query_tokens,
        } => json!({
            "name": "bm25f-unicode-and-fields",
            "definition": {
                "name": definition.name.as_str(),
                "fields": definition.fields.iter().map(|field| json!({
                    "path": field.path.segments(),
                    "weight_micros": field.weight_micros
                })).collect::<Vec<_>>()
            },
            "request": {
                "index": request.index.as_str(),
                "query": request.query,
                "limit": request.limit
            },
            "records": records.iter().map(|record| json!({
                "key_hex": encode_hex(&record.key),
                "value": value_json(&record.value)
            })).collect::<Vec<_>>(),
            "query_tokens": query_tokens,
            "scanned_documents": scanned_documents,
            "matched_documents": matched_documents,
            "ordered": matches.iter().map(|matched| json!({
                "key_hex": encode_hex(&matched.key),
                "score_nanos": matched.score_nanos,
                "terms": matched.terms.iter().map(|term| json!({
                    "token": term.token,
                    "document_frequency": term.document_frequency,
                    "score_nanos": term.score_nanos,
                    "fields": term.fields.iter().map(|field| json!({
                        "path": field.path.segments(),
                        "term_frequency": field.term_frequency,
                        "field_length": field.field_length
                    })).collect::<Vec<_>>()
                })).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }),
        LexicalOutcome::Abstained(_) => json!({"error": "golden unexpectedly abstained"}),
    }
}

fn hybrid_json(
    request: &HybridRequest,
    vector_request: &ExactRetrievalRequest,
    vector_candidates: &[DurableVectorRecord],
    outcome: &hyphae_retrieval::HybridOutcome,
) -> JsonValue {
    match outcome {
        hyphae_retrieval::HybridOutcome::Matches { matches, .. } => json!({
            "name": "two-branch-rrf",
            "k": 60,
            "request": {
                "lexical_weight": request.lexical_weight,
                "vector_weight": request.vector_weight,
                "limit": request.limit
            },
            "vector_request": {
                "space": vector_request.vector_space.as_str(),
                "query": vector_request.query.as_slice(),
                "limit": vector_request.limit,
                "minimum_score_nanos": vector_request.minimum_score_nanos,
                "minimum_margin_nanos": vector_request.minimum_margin_nanos
            },
            "vector_candidates": vector_candidates.iter().map(|candidate| json!({
                "key_hex": encode_hex(&candidate.key),
                "vector": candidate.vector.as_slice()
            })).collect::<Vec<_>>(),
            "ordered": matches.iter().map(|matched| json!({
                "key_hex": encode_hex(&matched.key),
                "lexical_rank": matched.explanation.lexical_rank,
                "vector_rank": matched.explanation.vector_rank,
                "lexical_contribution": matched.explanation.lexical_contribution,
                "vector_contribution": matched.explanation.vector_contribution,
                "fusion_score": matched.explanation.fusion_score,
                "final_rank": matched.explanation.final_rank
            })).collect::<Vec<_>>()
        }),
        hyphae_retrieval::HybridOutcome::Abstained(_) => {
            json!({"error": "golden unexpectedly abstained"})
        }
    }
}

fn value_json(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Boolean(value) => json!(value),
        Value::Integer(value) => json!(value),
        Value::String(value) => json!(value),
        Value::Bytes(value) => json!({"$bytes_hex": encode_hex(value)}),
        Value::Array(values) => JsonValue::Array(values.iter().map(value_json).collect()),
        Value::Object(fields) => JsonValue::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), value_json(value)))
                .collect(),
        ),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::build_golden;

    #[test]
    fn checked_in_retrieval_golden_matches_generator() -> Result<(), Box<dyn std::error::Error>> {
        let checked_in: serde_json::Value = serde_json::from_str(include_str!(
            "../../../compatibility/v2/retrieval-golden-v1.json"
        ))?;
        assert_eq!(checked_in, build_golden()?);
        Ok(())
    }
}
