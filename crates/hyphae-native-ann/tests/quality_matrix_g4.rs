// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded deterministic ANN recall matrix against the current exact oracle.

use std::collections::BTreeSet;

use hyphae_native_ann::{
    HnswConfig, HnswIndex, Metric, SearchOptions, Vector, VectorIndexDefinition, VectorRecord,
};
use hyphae_native_types::{Csn, ObjectId};

const VECTOR_COUNT: u16 = 384;
const DIMENSION: u16 = 24;
const QUERY_COUNT: u16 = 24;
const K: usize = 10;
const EF_SEARCH_MATRIX: [usize; 3] = [10, 32, 96];

#[test]
fn ann_recall_matrix_uses_exact_api_and_rejects_shifted_control()
-> Result<(), Box<dyn std::error::Error>> {
    let definition = VectorIndexDefinition::new(
        ObjectId::new(94_001)?,
        DIMENSION,
        Metric::Cosine,
        HnswConfig::new(16, 96, 32, 96, 0x4744_4d41_5452_4958)?,
    )?;
    let records = (1..=VECTOR_COUNT)
        .map(|id| {
            Ok(VectorRecord {
                object_id: ObjectId::new(u128::from(id))?,
                creating_csn: Csn::new(u64::from(id))?,
                vector: deterministic_vector(u64::from(id))?,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let index = HnswIndex::build(definition, records)?;
    let queries = (0..QUERY_COUNT)
        .map(|query| deterministic_vector(1_000_000 + u64::from(query)))
        .collect::<Result<Vec<_>, _>>()?;

    let exact = queries
        .iter()
        .map(|query| index.search_exact(query, K))
        .collect::<Result<Vec<_>, _>>()?;
    let mut matrix = Vec::new();
    for ef_search in EF_SEARCH_MATRIX {
        let options = SearchOptions::new(K, ef_search, Some(ef_search))?;
        let mut overlap = 0_usize;
        for (query, expected) in queries.iter().zip(&exact) {
            let expected_ids = expected
                .iter()
                .map(|hit| hit.object_id)
                .collect::<BTreeSet<_>>();
            overlap += index
                .search(query, options)?
                .hits
                .iter()
                .filter(|hit| expected_ids.contains(&hit.object_id))
                .count();
        }
        matrix.push((
            ef_search,
            overlap * 1_000_000 / (usize::from(QUERY_COUNT) * K),
        ));
    }

    let shifted_overlap = exact
        .iter()
        .zip(exact.iter().cycle().skip(1))
        .map(|(expected, shifted)| {
            let ids = expected
                .iter()
                .map(|hit| hit.object_id)
                .collect::<BTreeSet<_>>();
            shifted
                .iter()
                .filter(|hit| ids.contains(&hit.object_id))
                .count()
        })
        .sum::<usize>();
    let shifted_control_ppm = shifted_overlap * 1_000_000 / (usize::from(QUERY_COUNT) * K);

    assert_eq!(
        matrix.iter().map(|row| row.0).collect::<Vec<_>>(),
        EF_SEARCH_MATRIX
    );
    assert!(matrix.windows(2).all(|rows| rows[0].1 <= rows[1].1));
    assert!(matrix.last().is_some_and(|row| row.1 >= 950_000));
    assert!(matrix.last().is_some_and(|row| shifted_control_ppm < row.1));
    Ok(())
}

fn deterministic_vector(seed: u64) -> Result<Vector, Box<dyn std::error::Error>> {
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    let values = (0..DIMENSION)
        .map(|component| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state ^= u64::from(component).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let raw = u16::try_from((state >> 16) & u64::from(u16::MAX))?;
            Ok(f32::from(raw) / 32_767.5 - 1.0)
        })
        .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
    Ok(Vector::new(values)?)
}
