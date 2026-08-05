// SPDX-License-Identifier: Apache-2.0

//! Bounded relation-valued convergence tests for G5.

use hyphae_native_runtime::{
    AggregateOperation, AggregateResult, AggregateSpec, AnnSearchOptions, ConvergenceError,
    ConvergenceLimits, ConvergencePlan, ConvergenceSource, ConvergenceStrategy, HnswConfig,
    HybridSource, NativeDatabase, NativeHybridFusion, NativeVectorBranch, StructureSource, Vector,
    VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

static NEXT_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hyphae-convergence-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        Self(path)
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

fn id(value: u128) -> Result<ObjectId, Box<dyn std::error::Error>> {
    Ok(ObjectId::new(value)?)
}

fn bytes(id: ObjectId) -> Vec<u8> {
    id.get().to_be_bytes().to_vec()
}

fn plan(sources: Vec<ConvergenceSource>) -> ConvergencePlan {
    ConvergencePlan {
        sources,
        aggregates: vec![
            AggregateSpec {
                operation: AggregateOperation::Count,
                source: None,
            },
            AggregateSpec {
                operation: AggregateOperation::Sum,
                source: Some(0),
            },
            AggregateSpec {
                operation: AggregateOperation::Min,
                source: Some(0),
            },
            AggregateSpec {
                operation: AggregateOperation::Max,
                source: Some(0),
            },
        ],
        limits: ConvergenceLimits::default(),
    }
}

#[test]
fn all_structure_families_are_relation_valued_and_bounded() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TemporaryDirectory::create();
    let mut database = NativeDatabase::create(&temporary.0)?;
    let first = id(10)?;
    let second = id(20)?;
    let scalar = bytes(first);
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.set(scalar.clone(), b"2.5".to_vec(), None)?;
    seed.create_hash(b"hash".to_vec())?;
    seed.hset(b"hash".to_vec(), bytes(first), b"2.5".to_vec())?;
    seed.hset(b"hash".to_vec(), bytes(second), b"7.5".to_vec())?;
    seed.create_set(b"set".to_vec())?;
    seed.sadd(b"set".to_vec(), bytes(first))?;
    seed.create_list(b"list".to_vec())?;
    seed.rpush(b"list".to_vec(), bytes(first))?;
    seed.create_sorted_set(b"zset".to_vec())?;
    seed.zadd(b"zset".to_vec(), 2.5, bytes(first))?;
    seed.create_stream(b"stream".to_vec())?;
    seed.xadd(b"stream".to_vec(), &[(bytes(first), b"2.5".to_vec())])?;
    seed.commit()?;

    let snapshot = database.snapshot(0)?;
    let structures = [
        StructureSource::Scalar { key: scalar },
        StructureSource::Hash {
            key: b"hash".to_vec(),
        },
        StructureSource::Set {
            key: b"set".to_vec(),
        },
        StructureSource::List {
            key: b"list".to_vec(),
        },
        StructureSource::SortedSet {
            key: b"zset".to_vec(),
        },
        StructureSource::Stream {
            key: b"stream".to_vec(),
        },
    ];
    let expected = [
        ConvergenceStrategy::ScalarLookup,
        ConvergenceStrategy::HashRange,
        ConvergenceStrategy::SetRange,
        ConvergenceStrategy::ListRange,
        ConvergenceStrategy::SortedSetRange,
        ConvergenceStrategy::StreamRange,
    ];
    for (source, strategy) in structures.into_iter().zip(expected) {
        let mut request = plan(vec![ConvergenceSource::Structure(source)]);
        if matches!(
            strategy,
            ConvergenceStrategy::SetRange | ConvergenceStrategy::ListRange
        ) {
            request.aggregates.truncate(1);
        }
        let receipt = snapshot.converge(&request)?;
        assert_eq!(receipt.rows[0].object_id, first);
        assert_eq!(receipt.metrics.sources[0].strategy, strategy);
        assert!(receipt.metrics.sources[0].limit_pushed_down);
        assert_eq!(
            receipt.aggregates[0],
            AggregateResult::Count(u64::try_from(receipt.rows.len())?)
        );
    }

    let mut bounded = plan(vec![ConvergenceSource::Structure(StructureSource::Hash {
        key: b"hash".to_vec(),
    })]);
    bounded.limits.max_rows_per_source = 1;
    assert!(matches!(
        snapshot.converge(&bounded),
        Err(ConvergenceError::SourceRowLimitExceeded)
    ));
    Ok(())
}

#[test]
fn search_sources_join_on_object_id_with_oracle_metrics_and_snapshot_stability()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create();
    let mut database = NativeDatabase::create(&temporary.0)?;
    let lexical = id(1)?;
    let vectors = id(2)?;
    let first = id(10)?;
    let shared = id(20)?;
    let third = id(30)?;
    let config = HnswConfig::new(4, 16, 8, 32, 42)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config)?;
    seed.index_document(lexical, bytes(first), "rust rust")?;
    seed.index_document(lexical, bytes(shared), "rust")?;
    seed.upsert_vectors(
        vectors,
        [
            (first, Vector::new([0.0, 1.0])?),
            (shared, Vector::new([1.0, 0.0])?),
            (third, Vector::new([0.8, 0.2])?),
        ],
    )?;
    seed.commit()?;

    let query = Vector::new([1.0, 0.0])?;
    let ann = AnnSearchOptions::new(3, 8, Some(3))?;
    let request = ConvergencePlan {
        sources: vec![
            ConvergenceSource::Lexical {
                index: lexical,
                query: "rust".to_owned(),
                limit: 3,
            },
            ConvergenceSource::Exact {
                index: vectors,
                query: query.clone(),
                limit: 3,
            },
            ConvergenceSource::Ann {
                index: vectors,
                query: query.clone(),
                options: ann,
            },
            ConvergenceSource::Hybrid(HybridSource {
                lexical_index: lexical,
                lexical_query: "rust".to_owned(),
                lexical_limit: 3,
                vector_index: vectors,
                vector_query: query,
                vector_branch: NativeVectorBranch::Ann(ann),
                vector_limit: 3,
                fusion: NativeHybridFusion {
                    lexical_weight: 1,
                    vector_weight: 1,
                    limit: 3,
                },
            }),
        ],
        aggregates: vec![AggregateSpec {
            operation: AggregateOperation::Count,
            source: None,
        }],
        limits: ConvergenceLimits::default(),
    };
    let snapshot = database.snapshot(0)?;
    let receipt = snapshot.converge(&request)?;
    assert_eq!(receipt.snapshot_csn, snapshot.visible_csn());
    assert_eq!(
        receipt
            .rows
            .iter()
            .map(|row| row.object_id)
            .collect::<Vec<_>>(),
        vec![first, shared]
    );
    assert_eq!(receipt.aggregates, vec![AggregateResult::Count(2)]);
    assert_eq!(receipt.explanation.strategies.len(), 4);
    assert!(receipt.explanation.inner_join_by_object_id);
    assert!(receipt.explanation.stable_object_id_order);
    assert_eq!(receipt.metrics.sources[2].oracle_hits, Some(3));
    assert_eq!(receipt.metrics.sources[2].oracle_overlap, Some(3));
    assert_eq!(
        receipt.metrics.sources[2].oracle_recall_ppm,
        Some(1_000_000)
    );
    assert_eq!(receipt.metrics.sources[3].oracle_hits, Some(3));
    assert!(!receipt.metrics.aggregates_pushed_down);

    let mut later = database.begin(1, DurabilityClass::Memory)?;
    later.index_document(lexical, bytes(third), "rust rust rust")?;
    later.commit()?;
    assert_eq!(snapshot.converge(&request)?.rows, receipt.rows);
    Ok(())
}

#[test]
fn malformed_ids_and_invalid_aggregate_plans_fail_without_partial_rows()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create();
    let mut database = NativeDatabase::create(&temporary.0)?;
    let lexical = id(1)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.index_document(lexical, b"not-object-id".to_vec(), "rust")?;
    seed.commit()?;
    let snapshot = database.snapshot(0)?;
    let malformed = plan(vec![ConvergenceSource::Lexical {
        index: lexical,
        query: "rust".to_owned(),
        limit: 1,
    }]);
    let malformed_result = snapshot.converge(&malformed);
    assert!(
        matches!(malformed_result, Err(ConvergenceError::InvalidObjectId)),
        "unexpected malformed-ID result: {malformed_result:?}"
    );

    let mut invalid = malformed;
    invalid.aggregates = vec![AggregateSpec {
        operation: AggregateOperation::Sum,
        source: Some(2),
    }];
    assert!(matches!(
        snapshot.explain_convergence(&invalid),
        Err(ConvergenceError::InvalidAggregate)
    ));
    Ok(())
}
