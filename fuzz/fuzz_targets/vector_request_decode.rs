#![no_main]
// SPDX-License-Identifier: GPL-3.0-only

use hyphae_core::{Q15Vector, VectorSpaceName};
use hyphae_retrieval::{ExactRetrievalLimits, ExactRetrievalRequest, retrieve_exact};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(object) = value.as_object() else {
        return;
    };
    let Some(space) = object.get("vector_space").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(values) = object.get("query").and_then(serde_json::Value::as_array) else {
        return;
    };
    let decoded = values
        .iter()
        .map(|value| value.as_i64().and_then(|value| i16::try_from(value).ok()))
        .collect::<Option<Vec<_>>>();
    if let Some(decoded) = decoded {
        let limit = object
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(1);
        let Ok(vector_space) = VectorSpaceName::new(space) else {
            return;
        };
        let Ok(query) = Q15Vector::new(decoded) else {
            return;
        };
        let request = ExactRetrievalRequest {
            vector_space,
            query,
            limit,
            minimum_score_nanos: object
                .get("minimum_score_nanos")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-1_000_000_000),
            minimum_margin_nanos: object
                .get("minimum_margin_nanos")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        };
        let _ = retrieve_exact(&[], &request, &ExactRetrievalLimits::default());
    }
});
