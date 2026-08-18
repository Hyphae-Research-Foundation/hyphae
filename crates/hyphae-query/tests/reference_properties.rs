// SPDX-License-Identifier: Apache-2.0

//! Property tests for global query invariants and logical cursors.

use std::collections::BTreeMap;

use hyphae_query::{
    AggregationPlan, CompareOperator, ExecutionLimits, FieldPath, Filter, Metric, MetricValue,
    NamedMetric, NullPlacement, Query, Record, SortDirection, SortField, Value, execute,
};
use proptest::{prelude::*, test_runner::TestCaseError};

fn record(index: usize, score: i16) -> Record {
    Record::new(
        index.to_be_bytes(),
        Value::Object(BTreeMap::from([
            ("group".to_owned(), Value::Integer(i64::from(score) % 3)),
            ("score".to_owned(), Value::Integer(i64::from(score))),
        ])),
    )
}

fn query(limit: usize) -> Query {
    Query {
        filter: Filter::Compare {
            path: FieldPath::field("score"),
            operator: CompareOperator::GreaterOrEqual,
            value: Value::Integer(0),
        },
        sort: vec![SortField {
            path: FieldPath::field("score"),
            direction: SortDirection::Descending,
            nulls: NullPlacement::Last,
        }],
        cursor: None,
        limit,
        aggregation: Some(AggregationPlan {
            group_by: Vec::new(),
            metrics: vec![
                NamedMetric {
                    name: "count".to_owned(),
                    metric: Metric::Count,
                },
                NamedMetric {
                    name: "sum".to_owned(),
                    metric: Metric::Sum(FieldPath::field("score")),
                },
            ],
        }),
    }
}

fn test_error(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn shard_partition_and_input_order_cannot_change_global_results(
        scores in prop::collection::vec(-1_000_i16..=1_000, 1..96),
        limit in 1_usize..32,
    ) {
        let records = scores
            .iter()
            .enumerate()
            .map(|(index, score)| record(index, *score))
            .collect::<Vec<_>>();
        let left = records.iter().step_by(2).cloned().collect::<Vec<_>>();
        let right = records.iter().skip(1).step_by(2).cloned().collect::<Vec<_>>();
        let request = query(limit);
        let single = execute(
            &[records.as_slice()],
            &request,
            &ExecutionLimits::default(),
        ).map_err(test_error)?;
        let partitioned = execute(
            &[right.as_slice(), left.as_slice()],
            &request,
            &ExecutionLimits::default(),
        ).map_err(test_error)?;
        prop_assert_eq!(&single, &partitioned);

        let expected_scores = scores
            .iter()
            .copied()
            .filter(|score| *score >= 0)
            .collect::<Vec<_>>();
        let aggregation = single
            .aggregation
            .as_ref()
            .ok_or_else(|| TestCaseError::fail("missing aggregation"))?;
        let metrics = &aggregation.groups[0].metrics;
        let expected_count = u64::try_from(expected_scores.len()).map_err(test_error)?;
        prop_assert_eq!(
            &metrics[0].value,
            &MetricValue::Count(expected_count)
        );
        let sum = expected_scores.iter().map(|score| i128::from(*score)).sum::<i128>();
        let expected_sum = (!expected_scores.is_empty()).then_some(sum);
        prop_assert_eq!(&metrics[1].value, &MetricValue::Integer(expected_sum));
    }

    #[test]
    fn cursor_pagination_emits_each_globally_sorted_record_once(
        scores in prop::collection::vec(-1_000_i16..=1_000, 1..80),
        page_limit in 1_usize..16,
    ) {
        let records = scores
            .iter()
            .enumerate()
            .map(|(index, score)| record(index, *score))
            .collect::<Vec<_>>();
        let mut complete_request = query(records.len());
        complete_request.aggregation = None;
        let complete = execute(
            &[records.as_slice()],
            &complete_request,
            &ExecutionLimits::default(),
        ).map_err(test_error)?;
        let expected = complete
            .rows
            .iter()
            .map(|record| record.key.clone())
            .collect::<Vec<_>>();

        let mut page_request = query(page_limit);
        page_request.aggregation = None;
        let mut actual = Vec::new();
        loop {
            let page = execute(
                &[records.as_slice()],
                &page_request,
                &ExecutionLimits::default(),
            ).map_err(test_error)?;
            actual.extend(page.rows.into_iter().map(|record| record.key));
            prop_assert!(actual.len() <= records.len());
            let Some(cursor) = page.next_cursor else {
                break;
            };
            page_request.cursor = Some(cursor);
        }
        prop_assert_eq!(actual, expected);
    }
}
