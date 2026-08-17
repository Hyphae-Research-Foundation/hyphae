// SPDX-License-Identifier: Apache-2.0

//! Fixed-vector quality expectations for the provider-neutral reference path.

use hyphae_retrieval::{
    AbstentionReason, RetrievalLimits, RetrievalOutcome, RetrievalRequest, VectorRecord, retrieve,
};

#[test]
fn labeled_reference_vectors_have_perfect_top_one_accuracy()
-> Result<(), Box<dyn std::error::Error>> {
    let candidates = vec![
        VectorRecord::new(b"rust", vec![1.0, 0.0, 0.0]),
        VectorRecord::new(b"storage", vec![0.0, 1.0, 0.0]),
        VectorRecord::new(b"network", vec![0.0, 0.0, 1.0]),
        VectorRecord::new(b"rust-storage", vec![0.7, 0.7, 0.0]),
    ];
    let cases = [
        (vec![1.0, 0.0, 0.0], b"rust".as_slice()),
        (vec![0.0, 1.0, 0.0], b"storage".as_slice()),
        (vec![0.0, 0.0, 1.0], b"network".as_slice()),
    ];
    let total = cases.len();
    let mut correct = 0_usize;
    for (query, expected) in cases {
        let outcome = retrieve(
            &[candidates.as_slice()],
            &RetrievalRequest {
                query,
                limit: 1,
                minimum_score: 0.5,
                minimum_margin: 0.05,
            },
            &RetrievalLimits::default(),
        )?;
        let RetrievalOutcome::Matches { matches, .. } = outcome else {
            return Err("labeled query unexpectedly abstained".into());
        };
        if matches[0].key == expected {
            correct += 1;
        }
    }
    assert_eq!(correct, total);
    Ok(())
}

#[test]
fn intentionally_ambiguous_reference_vector_abstains() -> Result<(), Box<dyn std::error::Error>> {
    let candidates = vec![
        VectorRecord::new(b"left", vec![1.0, 0.0]),
        VectorRecord::new(b"right", vec![0.0, 1.0]),
    ];
    let outcome = retrieve(
        &[candidates.as_slice()],
        &RetrievalRequest {
            query: vec![1.0, 1.0],
            limit: 2,
            minimum_score: 0.5,
            minimum_margin: 0.01,
        },
        &RetrievalLimits::default(),
    )?;
    assert!(matches!(
        outcome,
        RetrievalOutcome::Abstained(hyphae_retrieval::Abstention {
            reason: AbstentionReason::Ambiguous,
            ..
        })
    ));
    Ok(())
}
