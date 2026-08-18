// SPDX-License-Identifier: Apache-2.0

//! Property tests for exact global retrieval invariants.

use hyphae_retrieval::{
    RetrievalLimits, RetrievalOutcome, RetrievalRequest, VectorRecord, retrieve,
};
use proptest::{prelude::*, test_runner::TestCaseError};

fn nonzero_vector() -> impl Strategy<Value = (i16, i16)> {
    (-1_000_i16..=1_000, -1_000_i16..=1_000)
        .prop_filter("vector must be nonzero", |(x, y)| *x != 0 || *y != 0)
}

fn test_error(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn shard_partition_and_input_order_cannot_change_exact_ranking(
        vectors in prop::collection::vec(nonzero_vector(), 1..96),
        limit in 1_usize..32,
    ) {
        let records = vectors
            .iter()
            .enumerate()
            .map(|(index, (x, y))| {
                VectorRecord::new(
                    index.to_be_bytes(),
                    vec![f64::from(*x), f64::from(*y)],
                )
            })
            .collect::<Vec<_>>();
        let left = records.iter().step_by(2).cloned().collect::<Vec<_>>();
        let right = records.iter().skip(1).step_by(2).cloned().collect::<Vec<_>>();
        let request = RetrievalRequest {
            query: vec![1.0, 2.0],
            limit,
            minimum_score: -1.0,
            minimum_margin: 0.0,
        };
        let single = retrieve(
            &[records.as_slice()],
            &request,
            &RetrievalLimits::default(),
        ).map_err(test_error)?;
        let partitioned = retrieve(
            &[right.as_slice(), left.as_slice()],
            &request,
            &RetrievalLimits::default(),
        ).map_err(test_error)?;
        prop_assert_eq!(&single, &partitioned);

        let RetrievalOutcome::Matches { matches, .. } = single else {
            return Err(TestCaseError::fail("valid candidates unexpectedly abstained"));
        };
        for pair in matches.windows(2) {
            prop_assert!(pair[0].score >= pair[1].score);
        }
        for candidate in matches {
            prop_assert!(candidate.score.is_finite());
            prop_assert!((-1.0..=1.0).contains(&candidate.score));
        }
    }
}
