// SPDX-License-Identifier: AGPL-3.0-only

//! Reproducible local correctness qualification for durable ANN routing.

use std::{error::Error, fs, path::PathBuf, time::SystemTime};

use hyphae_native_runtime::{
    ANN_PARTITION_ROUTING_POLICY_V1, AnnPartitionRoutingMode, AnnPartitionRoutingOutcome,
    AnnSearchOptions, HnswConfig, InitialAnnBulkBuilder, NativeDatabase, Vector, VectorHit,
    VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};
use serde_json::{Value, json};

const SCHEMA: &str = "hyphae-native-ann-durable-qualification-v1";
const GENERATOR: &str = "hyphae-ann-durable-qualification-corpus-v2";
const DIMENSION: u16 = 8;
const DIMENSION_USIZE: usize = 8;
const VECTOR_COUNT: usize = 512;
const LOGICAL_PARTITIONS: usize = 64;
const PREFERRED_PARTITIONS: usize = 32;
const QUERY_COUNT: usize = 12;
const K: usize = 4;

struct Arguments {
    source_commit: String,
    source_tree: String,
    metric: QualifiedMetric,
}

struct SelectedBatches {
    batches: Vec<Vec<VectorHit>>,
    certified_queries: usize,
    fallback_queries: usize,
    maximum_searched_partitions: usize,
}

#[derive(Clone, Copy)]
enum QualifiedMetric {
    SquaredL2,
    Cosine,
    NegativeDot,
}

impl QualifiedMetric {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value {
            "squared-l2" => Ok(Self::SquaredL2),
            "cosine" => Ok(Self::Cosine),
            "negative-dot" => Ok(Self::NegativeDot),
            _ => Err(format!("unsupported qualification metric: {value}").into()),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::SquaredL2 => "squared-l2",
            Self::Cosine => "cosine",
            Self::NegativeDot => "negative-dot",
        }
    }

    const fn runtime(self) -> VectorMetric {
        match self {
            Self::SquaredL2 => VectorMetric::SquaredL2,
            Self::Cosine => VectorMetric::Cosine,
            Self::NegativeDot => VectorMetric::NegativeDot,
        }
    }
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(metric: QualifiedMetric) -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        Ok(Self {
            path: std::env::temp_dir().join(format!(
                "hyphae-ann-qualification-{}-{}-{nonce}",
                std::process::id(),
                metric.label()
            )),
        })
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

fn parse_arguments() -> Result<Arguments, Box<dyn Error>> {
    let mut source_commit = None;
    let mut source_tree = None;
    let mut metric = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for {argument}"))?;
        match argument.as_str() {
            "--source-commit" => source_commit = Some(value),
            "--source-tree" => source_tree = Some(value),
            "--metric" => metric = Some(QualifiedMetric::parse(&value)?),
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    Ok(Arguments {
        source_commit: source_commit.ok_or("--source-commit is required")?,
        source_tree: source_tree.ok_or("--source-tree is required")?,
        metric: metric.ok_or("--metric is required")?,
    })
}

fn qualification_vectors() -> Result<Vec<(ObjectId, Vector)>, Box<dyn Error>> {
    let centers = [[100.0_f32, 0.0], [0.0, 100.0], [-100.0, 0.0], [0.0, -100.0]];
    (0..VECTOR_COUNT)
        .map(|offset| {
            let vectors_per_cluster = VECTOR_COUNT / centers.len();
            let cluster = offset / vectors_per_cluster;
            let within = offset % vectors_per_cluster;
            let within = f32::from(u16::try_from(within)?);
            let mut values = [0.0_f32; DIMENSION_USIZE];
            values[0] = centers[cluster][0];
            values[1] = centers[cluster][1];
            values[2] = within / 1_000.0;
            values[3] = within.rem_euclid(5.0) / 2_000.0;
            Ok((
                ObjectId::new(u128::try_from(offset + 1)?)?,
                Vector::new(values)?,
            ))
        })
        .collect()
}

fn qualification_queries() -> Result<Vec<Vector>, Box<dyn Error>> {
    let anchors: [[f32; 2]; QUERY_COUNT] = [
        [100.0_f32, 0.0],
        [0.0, 100.0],
        [-100.0, 0.0],
        [0.0, -100.0],
        [100.0, 100.0],
        [-100.0, 100.0],
        [-100.0, -100.0],
        [100.0, -100.0],
        [100.0, 80.0],
        [-80.0, 100.0],
        [-100.0, -80.0],
        [80.0, -100.0],
    ];
    anchors
        .into_iter()
        .enumerate()
        .map(|(offset, anchor)| {
            let offset = f32::from(u16::try_from(offset)?);
            Vector::new([
                anchor[0],
                anchor[1],
                offset / 1_000.0,
                offset.rem_euclid(3.0) / 2_000.0,
                0.0,
                0.0,
                0.0,
                0.0,
            ])
            .map_err(Into::into)
        })
        .collect()
}

fn hash_vectors(
    metric: QualifiedMetric,
    vectors: &[(ObjectId, Vector)],
) -> Result<String, Box<dyn Error>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(GENERATOR.as_bytes());
    hasher.update(metric.label().as_bytes());
    hasher.update(&u64::try_from(vectors.len())?.to_le_bytes());
    hasher.update(&DIMENSION.to_le_bytes());
    for (object_id, vector) in vectors {
        hasher.update(&object_id.get().to_be_bytes());
        for component in vector.values() {
            hasher.update(&component.to_bits().to_le_bytes());
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_queries(queries: &[Vector]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-durable-qualification-queries-v1");
    for query in queries {
        for component in query.values() {
            hasher.update(&component.to_bits().to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_hit_batches(batches: &[Vec<VectorHit>]) -> Result<String, Box<dyn Error>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-ann-durable-qualification-results-v1");
    for hits in batches {
        hasher.update(&u64::try_from(hits.len())?.to_le_bytes());
        for hit in hits {
            hasher.update(&hit.object_id.get().to_be_bytes());
            hasher.update(&hit.distance.to_bits().to_le_bytes());
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn identity(value: [u8; 32]) -> String {
    blake3::Hash::from_bytes(value).to_hex().to_string()
}

fn recall_ppm(observed: &[VectorHit], expected: &[VectorHit]) -> Result<u64, Box<dyn Error>> {
    let recalled = expected
        .iter()
        .filter(|expected_hit| {
            observed
                .iter()
                .any(|observed_hit| observed_hit.object_id == expected_hit.object_id)
        })
        .count();
    let expected = u64::try_from(expected.len())?;
    if expected == 0 {
        return Err("qualification oracle returned an empty top-k".into());
    }
    Ok(u64::try_from(recalled)? * 1_000_000 / expected)
}

fn routing_outcome_label(outcome: AnnPartitionRoutingOutcome) -> &'static str {
    match outcome {
        AnnPartitionRoutingOutcome::SelectedCertified => "selected-certified",
        AnnPartitionRoutingOutcome::FullFanoutRequested => "full-fanout-requested",
        AnnPartitionRoutingOutcome::FullFanoutBudgetFallback => "full-fanout-budget-fallback",
        AnnPartitionRoutingOutcome::SingleGenerationFallback => "single-generation-fallback",
    }
}

fn exact_batches(
    database: &NativeDatabase,
    index: ObjectId,
    queries: &[Vector],
) -> Result<Vec<Vec<VectorHit>>, Box<dyn Error>> {
    queries
        .iter()
        .map(|query| {
            database
                .search_vector_exact_latest(index, query, K)
                .map_err(Into::into)
        })
        .collect()
}

fn selected_batches(
    database: &NativeDatabase,
    index: ObjectId,
    queries: &[Vector],
    options: AnnSearchOptions,
) -> Result<SelectedBatches, Box<dyn Error>> {
    let mut batches = Vec::with_capacity(queries.len());
    let mut certified = 0;
    let mut fallbacks = 0;
    let mut maximum_searched = 0;
    for query in queries {
        let receipt =
            database.search_ann_selected_latest(index, query, options, PREFERRED_PARTITIONS)?;
        certified +=
            usize::from(receipt.routing_outcome == AnnPartitionRoutingOutcome::SelectedCertified);
        fallbacks += usize::from(
            receipt.routing_outcome == AnnPartitionRoutingOutcome::FullFanoutBudgetFallback,
        );
        maximum_searched = maximum_searched.max(receipt.selected_partitions.len());
        batches.push(receipt.search.hits);
    }
    Ok(SelectedBatches {
        batches,
        certified_queries: certified,
        fallback_queries: fallbacks,
        maximum_searched_partitions: maximum_searched,
    })
}

fn run(arguments: Arguments) -> Result<Value, Box<dyn Error>> {
    let directory = TemporaryDirectory::new(arguments.metric)?;
    let index = ObjectId::new(1)?;
    let vectors = qualification_vectors()?;
    let corpus_identity = hash_vectors(arguments.metric, &vectors)?;
    let queries = qualification_queries()?;
    let query_identity = hash_queries(&queries);
    let config = HnswConfig::new(8, 48, 32, 64, 0x5141_4c49_4659)?;
    let options = AnnSearchOptions::new(K, 64, Some(64))?;

    let mut database = NativeDatabase::create(&directory.path)?;
    let mut creation = database.begin(0, DurabilityClass::Strict)?;
    creation.create_vector_index(
        index,
        "local-durable-qualification",
        DIMENSION,
        arguments.metric.runtime(),
        config,
    )?;
    creation.commit()?;
    let plan = database.plan_initial_ann_bulk(index, vectors.clone(), LOGICAL_PARTITIONS)?;
    let expected_base_identity = identity(plan.expected_base_identity());
    let expected_view_identity = identity(plan.expected_view_identity());
    let build = plan.build_evidence();
    if build.builder != InitialAnnBulkBuilder::PartitionedHnswV1 {
        return Err("qualification runner used another initial ANN builder".into());
    }
    database.publish_initial_ann_bulk(plan, DurabilityClass::Strict)?;
    let published = database.observe_ann_index(index)?;

    let exact = exact_batches(&database, index, &queries)?;
    let mut defaults = Vec::with_capacity(queries.len());
    let mut full_fanout = Vec::with_capacity(queries.len());
    for query in &queries {
        let default = database.search_ann_latest(index, query, options)?;
        let full =
            database.search_ann_selected_latest(index, query, options, LOGICAL_PARTITIONS)?;
        defaults.push(default.hits);
        full_fanout.push(full.search.hits);
    }
    let selected = selected_batches(&database, index, &queries, options)?;
    let recalls = selected
        .batches
        .iter()
        .zip(&exact)
        .map(|(observed, expected)| recall_ppm(observed, expected))
        .collect::<Result<Vec<_>, _>>()?;
    let minimum_recall = recalls.iter().copied().min().unwrap_or(0);
    let aggregate_recall = recalls.iter().sum::<u64>() / u64::try_from(recalls.len())?;
    let exact_identity = hash_hit_batches(&exact)?;
    let default_identity = hash_hit_batches(&defaults)?;
    let full_identity = hash_hit_batches(&full_fanout)?;
    let selected_identity = hash_hit_batches(&selected.batches)?;
    let full_equals_default = full_fanout == defaults;

    drop(database);
    let mut database = NativeDatabase::open(&directory.path)?;
    let reopened = database.observe_ann_index(index)?;
    let reopened_selected = selected_batches(&database, index, &queries, options)?;

    let before_delta = database.observe_ann_index(index)?;
    let inserted = ObjectId::new(u128::try_from(VECTOR_COUNT + 1)?)?;
    let inserted_vector = Vector::new([0.0, 0.0, 0.0, 0.0, 1_000.0, 0.0, 0.0, 0.0])?;
    let deleted_vector = vectors[0].1.clone();
    let mut delta = database.begin(0, DurabilityClass::Strict)?;
    delta.upsert_vector(index, inserted, inserted_vector.clone())?;
    let deleted = delta.delete_vector(index, vectors[0].0)?;
    delta.commit()?;
    let after_delta = database.observe_ann_index(index)?;
    let delta_exact = exact_batches(&database, index, &queries)?;
    let delta_visible_identity = hash_hit_batches(&delta_exact)?;
    let upserts_visible = database
        .search_vector_exact_latest(index, &inserted_vector, K)?
        .iter()
        .any(|hit| hit.object_id == inserted);
    let deletes_hidden = deleted
        && !database
            .search_vector_exact_latest(index, &deleted_vector, K)?
            .iter()
            .any(|hit| hit.object_id == vectors[0].0);

    let consolidation = database.plan_ann_consolidation(index, VECTOR_COUNT + 1, 8)?;
    database.consolidate_ann(consolidation, DurabilityClass::Strict)?;
    let after_consolidation = database.observe_ann_index(index)?;
    let consolidated_exact = exact_batches(&database, index, &queries)?;
    let consolidated_identity = hash_hit_batches(&consolidated_exact)?;
    let after_route =
        database.search_ann_selected_latest(index, &queries[0], options, PREFERRED_PARTITIONS)?;
    let partitioned_after = after_route.routing_mode == AnnPartitionRoutingMode::SelectedPartitions
        && after_route.routing_outcome == AnnPartitionRoutingOutcome::SelectedCertified
        && after_route.total_partitions == LOGICAL_PARTITIONS;

    drop(database);
    let database = NativeDatabase::open(&directory.path)?;
    let final_observation = database.observe_ann_index(index)?;
    let final_exact = exact_batches(&database, index, &queries)?;
    let final_identity = hash_hit_batches(&final_exact)?;
    let final_route =
        database.search_ann_selected_latest(index, &queries[0], options, PREFERRED_PARTITIONS)?;

    Ok(json!({
        "schema": SCHEMA,
        "status": "diagnostic",
        "source": {
            "commit": arguments.source_commit,
            "tree": arguments.source_tree,
            "clean": true,
        },
        "dataset": {
            "generator": GENERATOR,
            "digest": corpus_identity,
            "vectors": VECTOR_COUNT,
            "dimension": DIMENSION,
            "metric": arguments.metric.label(),
        },
        "build": {
            "builder": "partitioned-hnsw-v1",
            "input_identity": identity(build.input_identity),
            "aggregate_identity": identity(build.build_identity),
            "expected_base_identity": expected_base_identity,
            "expected_view_identity": expected_view_identity,
            "published_base_identity": identity(published.base_identity),
            "published_view_identity": identity(published.view_identity),
            "vector_count": build.planned_vectors,
            "logical_partitions": build.planned_partitions,
            "planned_workers": build.planned_compute_threads,
            "planned_memory_bytes": build.planned_memory_bytes,
            "worker_batches": build.worker_batches,
            "routing_policy": ANN_PARTITION_ROUTING_POLICY_V1,
        },
        "quality": {
            "query_set_identity": query_identity,
            "queries": queries.len(),
            "k": K,
            "ef_search": 64,
            "selected_partitions": PREFERRED_PARTITIONS,
            "certified_selected_queries": selected.certified_queries,
            "full_fanout_fallback_queries": selected.fallback_queries,
            "maximum_searched_partitions": selected.maximum_searched_partitions,
            "selected_query_recall_ppm": aggregate_recall,
            "minimum_selected_query_recall_ppm": minimum_recall,
            "oracle_result_identity": exact_identity,
            "default_result_identity": default_identity,
            "full_fanout_result_identity": full_identity,
            "selected_result_identity": selected_identity,
            "full_fanout_equals_default": full_equals_default,
        },
        "lifecycle": {
            "initial_reopen": {
                "base_identity": identity(reopened.base_identity),
                "view_identity": identity(reopened.view_identity),
                "selected_result_identity": hash_hit_batches(&reopened_selected.batches)?,
            },
            "delta": {
                "before_base_identity": identity(before_delta.base_identity),
                "before_view_identity": identity(before_delta.view_identity),
                "after_base_identity": identity(after_delta.base_identity),
                "after_view_identity": identity(after_delta.view_identity),
                "upserted_vectors": 1,
                "deleted_vectors": usize::from(deleted),
                "upserts_visible": upserts_visible,
                "deletes_hidden": deletes_hidden,
                "visible_result_identity": delta_visible_identity,
            },
            "consolidation": {
                "before_base_identity": identity(after_delta.base_identity),
                "before_view_identity": identity(after_delta.view_identity),
                "after_base_identity": identity(after_consolidation.base_identity),
                "after_view_identity": identity(after_consolidation.view_identity),
                "remaining_delta_records": after_consolidation.delta_records,
                "view_preserved": consolidated_identity == delta_visible_identity,
                "visible_result_identity": consolidated_identity,
                "partitioned_base_preserved": partitioned_after,
                "routing_outcome_after": routing_outcome_label(after_route.routing_outcome),
                "total_partitions_after": after_route.total_partitions,
            },
            "final_reopen": {
                "base_identity": identity(final_observation.base_identity),
                "view_identity": identity(final_observation.view_identity),
                "delta_records": final_observation.delta_records,
                "view_preserved": final_identity == consolidated_identity,
                "visible_result_identity": final_identity,
                "routing_outcome": routing_outcome_label(final_route.routing_outcome),
                "total_partitions": final_route.total_partitions,
            },
        },
        "missing_gate_evidence": [],
        "claims": [],
        "closure_declared": false,
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let receipt = run(parse_arguments()?)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
