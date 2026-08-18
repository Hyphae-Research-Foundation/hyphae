#![no_main]
// SPDX-License-Identifier: Apache-2.0

use hyphae_core::VectorSpaceName;
use hyphae_query::FieldPath;
use hyphae_retrieval::{
    ExactAbstention, ExactAbstentionReason, ExactRetrievalOutcome, HybridRequest,
    LexicalField, LexicalIndexDefinition, LexicalLimits, LexicalRequest, fuse_hybrid,
    retrieve_lexical, tokenize_v1,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(object) = value.as_object() else {
        return;
    };
    let index_name = object
        .get("lexical_index")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("fuzz");
    let query = object
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let limit = object
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let Ok(name) = VectorSpaceName::new(index_name) else {
        return;
    };
    let field = LexicalField {
        path: FieldPath::field("body"),
        weight_micros: 1_000_000,
    };
    let Ok(index) = LexicalIndexDefinition::new(name.clone(), vec![field]) else {
        return;
    };
    let request = LexicalRequest {
        index: name,
        query: query.to_owned(),
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
    };
    let _ = tokenize_v1(query);
    let lexical = retrieve_lexical(&[], &index, &request, &LexicalLimits::default());

    let hybrid = HybridRequest {
        lexical_weight: object
            .get("lexical_weight")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1),
        vector_weight: object
            .get("vector_weight")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1),
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
    };
    if let Ok(lexical) = lexical {
        let vector = ExactRetrievalOutcome::Abstained(ExactAbstention {
            reason: ExactAbstentionReason::NoCandidates,
            best_score_nanos: None,
            runner_up_score_nanos: None,
            scanned_candidates: 0,
        });
        let _ = fuse_hybrid(&lexical, &vector, &hybrid);
    }
});
