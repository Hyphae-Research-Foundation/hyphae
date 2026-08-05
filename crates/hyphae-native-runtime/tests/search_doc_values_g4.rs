// SPDX-License-Identifier: Apache-2.0

//! G4 equivalence and negative controls for bounded native doc values.

use std::collections::BTreeMap;

use hyphae_native_runtime::{
    DocValue, DocValueAggregation, DocValueAggregationValue, DocValueCandidate, DocValueError,
    DocValueFilter, DocValueLimits, DocValueOperator, DocValueRequest, DocValueSort,
    DocValueSortDirection, DocValueSortSource, FacetRequest, MissingPlacement,
    NamedDocValueAggregation, execute_doc_values,
};

fn candidate(id: u8, score: f64, group: &str, price: i64) -> DocValueCandidate {
    DocValueCandidate {
        document_id: vec![id],
        score,
        values: BTreeMap::from([
            ("active".to_owned(), DocValue::Boolean(id != 3)),
            ("group".to_owned(), DocValue::String(group.to_owned())),
            ("price".to_owned(), DocValue::Integer(price)),
        ]),
    }
}

fn request() -> DocValueRequest {
    DocValueRequest {
        filter: DocValueFilter::All(vec![
            DocValueFilter::Compare {
                field: "active".to_owned(),
                operator: DocValueOperator::Equal,
                value: DocValue::Boolean(true),
            },
            DocValueFilter::Compare {
                field: "price".to_owned(),
                operator: DocValueOperator::GreaterOrEqual,
                value: DocValue::Integer(10),
            },
        ]),
        sort: vec![DocValueSort {
            source: DocValueSortSource::Field("price".to_owned()),
            direction: DocValueSortDirection::Ascending,
            missing: MissingPlacement::Last,
        }],
        limit: 2,
        facets: vec![FacetRequest {
            field: "group".to_owned(),
            limit: 8,
        }],
        aggregations: vec![
            NamedDocValueAggregation {
                name: "count".to_owned(),
                aggregation: DocValueAggregation::Count,
            },
            NamedDocValueAggregation {
                name: "total".to_owned(),
                aggregation: DocValueAggregation::Sum("price".to_owned()),
            },
            NamedDocValueAggregation {
                name: "maximum".to_owned(),
                aggregation: DocValueAggregation::Max("price".to_owned()),
            },
        ],
    }
}

#[test]
fn optimized_surface_matches_a_direct_reference_and_aggregates_before_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let candidates = vec![
        candidate(4, 2.0, "b", 10),
        candidate(2, 8.0, "a", 30),
        candidate(1, 1.0, "a", 20),
        candidate(3, 9.0, "ignored", 40),
        candidate(5, 3.0, "b", 5),
    ];
    let result = execute_doc_values(&candidates, &request(), &DocValueLimits::default())?;

    let mut reference: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.values.get("active") == Some(&DocValue::Boolean(true))
                && candidate.values.get("price") >= Some(&DocValue::Integer(10))
        })
        .collect();
    reference.sort_by_key(|candidate| {
        (
            candidate.values.get("price"),
            candidate.document_id.as_slice(),
        )
    });
    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.document_id.as_slice())
            .collect::<Vec<_>>(),
        reference
            .iter()
            .take(2)
            .map(|hit| hit.document_id.as_slice())
            .collect::<Vec<_>>()
    );
    assert_eq!(result.scanned_candidates, 5);
    assert_eq!(result.matched_candidates, 3);
    assert_eq!(
        result.facets[0].buckets[0].value,
        DocValue::String("a".to_owned())
    );
    assert_eq!(result.facets[0].buckets[0].count, 2);
    assert_eq!(
        result.aggregations[0].value,
        DocValueAggregationValue::Count(3)
    );
    assert_eq!(
        result.aggregations[1].value,
        DocValueAggregationValue::Integer(Some(60))
    );
    assert_eq!(
        result.aggregations[2].value,
        DocValueAggregationValue::Value(Some(DocValue::Integer(30)))
    );
    Ok(())
}

#[test]
fn missing_sort_placement_and_binary_identity_ties_are_deterministic()
-> Result<(), Box<dyn std::error::Error>> {
    let mut missing = candidate(0, 1.0, "x", 1);
    missing.values.remove("price");
    let candidates = vec![
        candidate(2, 1.0, "x", 7),
        missing,
        candidate(1, 1.0, "x", 7),
    ];
    let mut request = request();
    request.filter = DocValueFilter::MatchAll;
    request.limit = 3;
    request.facets.clear();
    request.aggregations.clear();
    request.sort[0].missing = MissingPlacement::First;
    let result = execute_doc_values(&candidates, &request, &DocValueLimits::default())?;
    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.document_id.as_slice())
            .collect::<Vec<_>>(),
        [b"\0".as_slice(), b"\x01".as_slice(), b"\x02".as_slice()]
    );
    Ok(())
}

#[test]
fn negative_controls_reject_bounds_duplicates_types_and_noncanonical_scores() {
    let candidates = vec![candidate(1, 1.0, "a", 1), candidate(2, 1.0, "b", 2)];
    let limits = DocValueLimits {
        max_candidates: 1,
        ..DocValueLimits::default()
    };
    assert!(matches!(
        execute_doc_values(&candidates, &request(), &limits),
        Err(DocValueError::CandidateLimit { .. })
    ));

    let duplicate = vec![candidate(1, 1.0, "a", 1), candidate(1, 2.0, "b", 2)];
    assert_eq!(
        execute_doc_values(&duplicate, &request(), &DocValueLimits::default()),
        Err(DocValueError::DuplicateDocumentId)
    );

    let mut invalid = candidate(1, f64::NAN, "a", 1);
    assert_eq!(
        execute_doc_values(&[invalid.clone()], &request(), &DocValueLimits::default()),
        Err(DocValueError::NoncanonicalScore)
    );
    invalid.score = 1.0;
    invalid.values.insert(
        "price".to_owned(),
        DocValue::String("not an integer".to_owned()),
    );
    let mut sum_request = request();
    sum_request.filter = DocValueFilter::MatchAll;
    assert!(matches!(
        execute_doc_values(&[invalid], &sum_request, &DocValueLimits::default()),
        Err(DocValueError::AggregationType { .. })
    ));

    let depth_limits = DocValueLimits {
        max_filter_depth: 1,
        ..DocValueLimits::default()
    };
    assert!(matches!(
        execute_doc_values(&candidates, &request(), &depth_limits),
        Err(DocValueError::ShapeLimit {
            kind: "filter depth",
            ..
        })
    ));
}

#[test]
fn facets_fail_instead_of_returning_partial_counts() {
    let candidates = vec![candidate(1, 1.0, "a", 1), candidate(2, 1.0, "b", 2)];
    let mut request = request();
    request.filter = DocValueFilter::MatchAll;
    request.facets[0].limit = 1;
    let limits = DocValueLimits {
        max_facet_terms: 1,
        ..DocValueLimits::default()
    };
    assert!(matches!(
        execute_doc_values(&candidates, &request, &limits),
        Err(DocValueError::FacetTermLimit { .. })
    ));
}
