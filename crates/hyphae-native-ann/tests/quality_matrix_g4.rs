// SPDX-License-Identifier: Apache-2.0

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

/// Clustered corpora are the regime where naive truncate-to-M pruning
/// collapses recall: all M nearest neighbors of a node fall inside its own
/// cluster and the graph loses inter-cluster connectivity. The diversity
/// selection rule must keep cross-cluster edges alive.
#[test]
fn clustered_corpus_keeps_cross_cluster_recall() -> Result<(), Box<dyn std::error::Error>> {
    const CLUSTERS: u16 = 12;
    const PER_CLUSTER: u16 = 48;
    const CLUSTER_DIMENSION: u16 = 16;
    const CLUSTER_K: usize = 10;

    let definition = VectorIndexDefinition::new(
        ObjectId::new(94_002)?,
        CLUSTER_DIMENSION,
        Metric::SquaredL2,
        HnswConfig::new(8, 64, 48, 96, 0x434c_5553_5445_5253)?,
    )?;
    let mut records = Vec::new();
    for cluster in 0..CLUSTERS {
        for member in 0..PER_CLUSTER {
            let id = u128::from(cluster) * u128::from(PER_CLUSTER) + u128::from(member) + 1;
            records.push(VectorRecord {
                object_id: ObjectId::new(id)?,
                creating_csn: Csn::new(u64::try_from(id)?)?,
                vector: clustered_vector(cluster, u64::from(member))?,
            });
        }
    }
    let index = HnswIndex::build(definition, records)?;

    // Queries sit near cluster centroids; ground truth via the exact oracle.
    let mut overlap = 0_usize;
    let mut total = 0_usize;
    for cluster in 0..CLUSTERS {
        let query = clustered_vector(cluster, 1_000_003)?;
        let expected = index
            .search_exact(&query, CLUSTER_K)?
            .iter()
            .map(|hit| hit.object_id)
            .collect::<BTreeSet<_>>();
        let options = SearchOptions::new(CLUSTER_K, 48, None)?;
        overlap += index
            .search(&query, options)?
            .hits
            .iter()
            .filter(|hit| expected.contains(&hit.object_id))
            .count();
        total += CLUSTER_K;
    }
    let recall_ppm = overlap * 1_000_000 / total;
    assert!(
        recall_ppm >= 950_000,
        "clustered recall@10 was {recall_ppm} ppm"
    );
    Ok(())
}

fn clustered_vector(cluster: u16, member: u64) -> Result<Vector, Box<dyn std::error::Error>> {
    // Distant centroid per cluster plus a small deterministic jitter.
    let mut state = (u64::from(cluster) << 32) ^ member ^ 0x9e37_79b9_7f4a_7c15;
    let values = (0..16_u16)
        .map(|component| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let jitter =
                f32::from(u16::try_from(state >> 48).unwrap_or(0)) / f32::from(u16::MAX) / 8.0;
            let centroid = if component % 4 == cluster % 4 {
                f32::from(cluster) * 10.0
            } else {
                f32::from(cluster % 3) * 5.0
            };
            centroid + jitter
        })
        .collect::<Vec<_>>();
    Ok(Vector::new(values)?)
}

/// SQ8 quality gate: compressed top-3k candidates rescored with exact
/// distances must recover recall@k >= 0.95 against the exact oracle for
/// every metric, per ann-semantics-v1.
#[test]
fn sq8_compressed_rescoring_meets_the_recall_floor() -> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_ann::Sq8Quantizer;

    const SQ_VECTORS: u16 = 256;
    const SQ_DIMENSION: u16 = 16;
    const SQ_QUERIES: u16 = 16;
    const SQ_K: usize = 10;

    for metric in [Metric::SquaredL2, Metric::Cosine, Metric::NegativeDot] {
        let vectors: Vec<Vector> = (1..=SQ_VECTORS)
            .map(|seed| sq_vector(u64::from(seed), SQ_DIMENSION))
            .collect::<Result<_, _>>()?;
        let quantizer = Sq8Quantizer::train(SQ_DIMENSION, metric, &vectors)?;
        let codes = vectors
            .iter()
            .map(|vector| quantizer.encode(vector))
            .collect::<Result<Vec<_>, _>>()?;

        let mut recalled = 0_usize;
        for query_seed in 0..SQ_QUERIES {
            let query = sq_vector(1_000_000 + u64::from(query_seed), SQ_DIMENSION)?;
            let query_code = quantizer.encode(&query)?;

            // Exact oracle top-k.
            let mut exact: Vec<(usize, f64)> = vectors
                .iter()
                .enumerate()
                .map(|(ordinal, vector)| (ordinal, exact_distance(metric, &query, vector)))
                .collect();
            exact.sort_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            let expected: std::collections::BTreeSet<usize> = exact
                .iter()
                .take(SQ_K)
                .map(|(ordinal, _)| *ordinal)
                .collect();

            // Compressed candidates: top 3k by approximate distance.
            let mut approximate: Vec<(usize, f64)> = codes
                .iter()
                .enumerate()
                .map(|(ordinal, code)| Ok((ordinal, quantizer.distance(&query_code, code)?)))
                .collect::<Result<_, Box<dyn std::error::Error>>>()?;
            approximate.sort_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            approximate.truncate(SQ_K * 3);

            // Exact rescoring of the compressed candidates.
            let mut rescored: Vec<(usize, f64)> = approximate
                .into_iter()
                .map(|(ordinal, _)| (ordinal, exact_distance(metric, &query, &vectors[ordinal])))
                .collect();
            rescored.sort_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            recalled += rescored
                .iter()
                .take(SQ_K)
                .filter(|(ordinal, _)| expected.contains(ordinal))
                .count();
        }
        let opportunities = usize::from(SQ_QUERIES) * SQ_K;
        assert!(
            recalled * 100 >= opportunities * 95,
            "metric {metric:?} recalled {recalled}/{opportunities}"
        );
    }

    // Degenerate training fails closed.
    let flat = vec![sq_constant_vector(4, 1.5)?; 3];
    assert!(Sq8Quantizer::train(4, Metric::SquaredL2, &flat).is_err());
    Ok(())
}

fn sq_vector(seed: u64, dimension: u16) -> Result<Vector, Box<dyn std::error::Error>> {
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    let values = (0..dimension)
        .map(|component| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let unit = f32::from(u16::try_from(state >> 48).unwrap_or(0)) / f32::from(u16::MAX);
            unit * 4.0 - 2.0 + f32::from(component % 3)
        })
        .collect::<Vec<_>>();
    Ok(Vector::new(values)?)
}

fn sq_constant_vector(dimension: u16, value: f32) -> Result<Vector, Box<dyn std::error::Error>> {
    Ok(Vector::new(vec![value; usize::from(dimension)])?)
}

fn exact_distance(metric: Metric, left: &Vector, right: &Vector) -> f64 {
    let dot: f64 = left
        .values()
        .iter()
        .zip(right.values())
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum();
    match metric {
        Metric::SquaredL2 => left
            .values()
            .iter()
            .zip(right.values())
            .map(|(left, right)| {
                let delta = f64::from(*left) - f64::from(*right);
                delta * delta
            })
            .sum(),
        Metric::NegativeDot => -dot,
        Metric::Cosine => {
            let left_norm: f64 = left
                .values()
                .iter()
                .map(|value| f64::from(*value) * f64::from(*value))
                .sum();
            let right_norm: f64 = right
                .values()
                .iter()
                .map(|value| f64::from(*value) * f64::from(*value))
                .sum();
            1.0 - dot / (left_norm.sqrt() * right_norm.sqrt())
        }
    }
}
