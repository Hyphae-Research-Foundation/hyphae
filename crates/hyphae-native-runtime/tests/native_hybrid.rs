// SPDX-License-Identifier: Apache-2.0

//! Embedded native hybrid retrieval coverage.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_runtime::{
    AnnPartitionRoutingOutcome, AnnSearchOptions, GovernorClassLimit, GovernorMode,
    HardwareProfile, HnswConfig, NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM,
    NATIVE_LEXICAL_READ_VIEW_EXECUTION, NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE,
    NATIVE_STRUCTURE_FILTER_EXECUTION, NATIVE_STRUCTURE_FILTER_IDENTITY_ALGORITHM, NativeDatabase,
    NativeExecutionPool, NativeFilteredLexicalReadViewOpenRequest, NativeGovernorPolicy,
    NativeHybridError, NativeHybridFusion, NativeHybridOutcome, NativeHybridReadViewOpenReceipt,
    NativeHybridReadViewOpenRequest, NativeHybridReadViewQuery, NativeHybridReadViewQueryReceipt,
    NativeHybridRequest, NativeLexicalReadViewOpenRequest, NativeResourceGovernor,
    NativeRuntimeError, NativeStructureScalarFilter, NativeVectorBranch, Vector, VectorMetric,
    WorkloadClass,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

static NEXT_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static READ_VIEW_TEST_LOCK: Mutex<()> = Mutex::new(());

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-hybrid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
                + u128::from(NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

fn config() -> Result<HnswConfig, Box<dyn std::error::Error>> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
}

fn governor_policy(profile: &HardwareProfile) -> NativeGovernorPolicy {
    let workers = 2;
    let memory_bytes = 512 * 1_024 * 1_024;
    let classes = [
        WorkloadClass::ForegroundPoint,
        WorkloadClass::ForegroundBounded,
        WorkloadClass::Mutation,
        WorkloadClass::Bulk,
        WorkloadClass::Maintenance,
        WorkloadClass::Recovery,
        WorkloadClass::Administrative,
    ];
    NativeGovernorPolicy {
        schema: "hyphae-native-governor-policy-v1".to_owned(),
        mode: GovernorMode::Mixed,
        hardware_fingerprint: profile.fingerprint.clone(),
        calibration_cache_key: "native-hybrid-read-view-test".to_owned(),
        calibrated_worker_limit: workers,
        reserved_system_threads: 0,
        schedulable_compute_threads: workers,
        io_slots: workers,
        memory_bytes,
        memory_headroom_percent: 0,
        admission_queue_capacity: 32,
        foreground_burst_limit: 8,
        class_limits: classes
            .into_iter()
            .map(|class| GovernorClassLimit {
                class,
                compute_threads: workers,
                io_slots: workers,
                memory_bytes,
            })
            .collect(),
    }
}

fn install_governor(
    database: &mut NativeDatabase,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = HardwareProfile::discover(path)?;
    let policy = governor_policy(&profile);
    database.set_resource_governor_with_execution_pool(
        Arc::new(NativeResourceGovernor::new(policy.clone())),
        Arc::new(NativeExecutionPool::new(&profile, &policy)?),
        Duration::ZERO,
    )?;
    Ok(())
}

fn install_governor_with_wait(
    database: &mut NativeDatabase,
    path: &std::path::Path,
    maximum_wait: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = HardwareProfile::discover(path)?;
    let policy = governor_policy(&profile);
    database.set_resource_governor_with_execution_pool(
        Arc::new(NativeResourceGovernor::new(policy.clone())),
        Arc::new(NativeExecutionPool::new(&profile, &policy)?),
        maximum_wait,
    )?;
    Ok(())
}

fn seed_read_view_fixture(
    database: &mut NativeDatabase,
) -> Result<(ObjectId, ObjectId, Vector), Box<dyn std::error::Error>> {
    let lexical = ObjectId::new(101)?;
    let vectors = ObjectId::new(102)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: 64,
        consolidate_after_deltas: 32,
        retain_generations: 1,
    };
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "read_view_documents")?;
    seed.create_vector_index_with_lifecycle(
        vectors,
        "read_view_vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
        lifecycle,
    )?;
    for id in 1..=8_u128 {
        let text = if id <= 3 {
            "rare native"
        } else {
            "common native"
        };
        seed.index_document(lexical, id.to_be_bytes().to_vec(), text)?;
    }
    seed.commit()?;
    let vector_records = (1..=8_u8)
        .map(|id| {
            Ok((
                ObjectId::new(u128::from(id))?,
                Vector::new([f32::from(id), 0.0])?,
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let plan = database.plan_initial_ann_bulk(vectors, vector_records, 2)?;
    database.publish_initial_ann_bulk(plan, DurabilityClass::Memory)?;
    Ok((lexical, vectors, Vector::new([2.0, 0.0])?))
}

fn assert_hybrid_read_view_receipts(
    open: &NativeHybridReadViewOpenReceipt,
    first: &NativeHybridReadViewQueryReceipt,
) {
    assert_eq!(open.root_identity, open.lexical.root_identity);
    assert_eq!(open.root_identity, open.ann.root_identity);
    assert_eq!(open.snapshot_csn, open.lexical.snapshot_csn);
    assert_eq!(open.snapshot_csn, open.ann.snapshot_csn);
    assert_eq!(open.catalog_version, open.lexical.catalog_version);
    assert_eq!(open.catalog_version, open.ann.catalog_version);
    assert_eq!(
        open.lexical.lexical_plan_scope,
        NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE
    );
    assert_eq!(
        open.lexical.lexical_index_identity_algorithm,
        NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM
    );
    assert_ne!(open.lexical.lexical_index_identity, [0; 32]);
    assert_eq!(open.lexical.retained_postings, 3);
    assert!(open.lexical.retained_memory_bytes > 0);
    assert_eq!(open.lexical.hydration.request.compute_threads, 1);
    assert_eq!(open.lexical.hydration.request.io_slots, 1);
    assert!(open.lexical.observed_physical_entries <= open.lexical.planned_physical_entries);
    assert!(open.lexical.observed_physical_bytes <= open.lexical.planned_physical_bytes);
    assert!(open.lexical.retained_memory_bytes <= open.lexical.admitted_retained_memory_bytes);
    assert_eq!(first.root_identity, open.root_identity);
    assert_eq!(first.snapshot_csn, open.snapshot_csn);
    assert_eq!(first.lexical.root_identity, open.root_identity);
    assert_eq!(
        first.lexical.lexical_index_identity,
        open.lexical.lexical_index_identity
    );
    assert_eq!(
        first.lexical.lexical_execution,
        NATIVE_LEXICAL_READ_VIEW_EXECUTION
    );
    assert_eq!(first.ann.root_identity, open.root_identity);
    assert_eq!(first.lexical.postings_evaluated, 3);
    assert_eq!(first.lexical.physical_page_reads, 0);
    assert_eq!(first.ann.physical_page_reads, 0);
    assert_eq!(first.peak_admission.request.compute_threads, 2);
    assert_eq!(first.peak_admission.request.io_slots, 0);
    assert!(
        first.peak_admission.request.memory_bytes
            >= first.result_retention.request.memory_bytes
                + first
                    .lexical
                    .execution
                    .request
                    .memory_bytes
                    .max(first.ann.execution.request.memory_bytes)
    );
    assert_eq!(first.result_retention.request.compute_threads, 0);
    assert_eq!(first.result_retention.request.io_slots, 0);
    assert!(first.result_retention.request.memory_bytes > 0);
    assert_eq!(first.fusion.request.compute_threads, 1);
    assert_eq!(first.fusion.request.io_slots, 0);
    assert_eq!(first.fusion.request.memory_bytes, 0);
    assert!(first.result_retention.execution_time >= first.lexical.execution.execution_time);
    assert!(first.result_retention.execution_time >= first.ann.execution.execution_time);
    assert!(first.result_retention.execution_time >= first.fusion.execution_time);
    assert!(first.peak_admission.execution_time >= first.result_retention.execution_time);
    assert!(first.ann.execution.execution_time > Duration::ZERO);
}

fn read_view_open_request(
    lexical: ObjectId,
    vectors: ObjectId,
) -> NativeHybridReadViewOpenRequest<'static> {
    NativeHybridReadViewOpenRequest {
        lexical: NativeLexicalReadViewOpenRequest {
            index: lexical,
            query: "rare",
            limit: 3,
            maximum_retained_postings: 16,
            maximum_retained_bytes: 64 * 1_024,
        },
        vector_index: vectors,
    }
}

fn request(
    lexical: ObjectId,
    vectors: ObjectId,
    query: &Vector,
    vector_branch: NativeVectorBranch,
) -> NativeHybridRequest<'_> {
    NativeHybridRequest {
        lexical_index: lexical,
        lexical_query: "rust",
        lexical_limit: 3,
        vector_index: vectors,
        vector_query: query,
        vector_branch,
        vector_limit: 3,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    }
}

#[test]
fn exact_hybrid_fuses_stable_ids_on_one_snapshot_with_explanations()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.0.join("data"))?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let first = ObjectId::new(10)?;
    let shared = ObjectId::new(20)?;
    let third = ObjectId::new(30)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, first.get().to_be_bytes().to_vec(), "rust rust")?;
    seed.index_document(lexical, shared.get().to_be_bytes().to_vec(), "rust")?;
    seed.upsert_vectors(
        vectors,
        [
            (shared, Vector::new([1.0, 0.0])?),
            (third, Vector::new([0.8, 0.2])?),
            (first, Vector::new([0.0, 1.0])?),
        ],
    )?;
    seed.commit()?;

    let query = Vector::new([1.0, 0.0])?;
    let snapshot = database.snapshot(7)?;
    let receipt = snapshot.retrieve_hybrid(&request(
        lexical,
        vectors,
        &query,
        NativeVectorBranch::Exact,
    ))?;
    assert_eq!(receipt.snapshot_csn, snapshot.visible_csn());
    assert_eq!(receipt.lexical_candidates, 2);
    assert_eq!(receipt.vector_candidates, 3);
    assert!(receipt.ann.is_none());
    let NativeHybridOutcome::Matches(matches) = receipt.outcome else {
        return Err(std::io::Error::other("hybrid unexpectedly abstained").into());
    };
    assert_eq!(matches[0].object_id, shared);
    assert_eq!(matches[0].explanation.lexical_rank, Some(2));
    assert_eq!(matches[0].explanation.vector_rank, Some(1));
    assert_eq!(matches[0].explanation.final_rank, 1);

    let mut later = database.begin(8, DurabilityClass::Memory)?;
    later.index_document(
        lexical,
        third.get().to_be_bytes().to_vec(),
        "rust rust rust",
    )?;
    later.commit()?;
    assert_eq!(
        snapshot
            .retrieve_hybrid(&request(
                lexical,
                vectors,
                &query,
                NativeVectorBranch::Exact,
            ))?
            .snapshot_csn,
        receipt.snapshot_csn
    );
    Ok(())
}

#[test]
fn ann_hybrid_exposes_ann_receipt_and_rejects_branch_limit_mismatch()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.0.join("data"))?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let stable = ObjectId::new(10)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, stable.get().to_be_bytes().to_vec(), "rust")?;
    seed.upsert_vector(vectors, stable, Vector::new([1.0, 0.0])?)?;
    seed.commit()?;

    let query = Vector::new([1.0, 0.0])?;
    let options = AnnSearchOptions::new(1, 8, Some(1))?;
    let mut request = request(lexical, vectors, &query, NativeVectorBranch::Ann(options));
    request.vector_limit = 1;
    let receipt = database.retrieve_hybrid_latest(0, &request)?;
    let ann = receipt
        .ann
        .ok_or_else(|| std::io::Error::other("missing ANN receipt"))?;
    assert!(ann.approximate);
    assert_eq!(ann.snapshot_csn, receipt.snapshot_csn);

    request.vector_limit = 2;
    assert!(matches!(
        database.retrieve_hybrid_latest(0, &request),
        Err(NativeHybridError::InvalidRequest)
    ));
    request.vector_limit = 1;
    request.lexical_limit = 0;
    assert!(matches!(
        database.retrieve_hybrid_latest(0, &request),
        Err(NativeHybridError::InvalidRequest)
    ));
    Ok(())
}

#[test]
fn hybrid_rejects_lexical_ids_that_cannot_join_vector_ids() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.0.join("data"))?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let stable = ObjectId::new(10)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, b"not-an-id".to_vec(), "rust")?;
    seed.upsert_vector(vectors, stable, Vector::new([1.0, 0.0])?)?;
    seed.commit()?;
    let query = Vector::new([1.0, 0.0])?;
    assert!(matches!(
        database.retrieve_hybrid_latest(
            0,
            &request(lexical, vectors, &query, NativeVectorBranch::Exact)
        ),
        Err(NativeHybridError::InvalidStableId)
    ));
    Ok(())
}

#[test]
fn hybrid_read_view_binds_owned_lexical_and_ann_execution_to_one_root()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;

    let reads_before = database.physical_observation()?.physical_page_reads;
    let (view, open) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    assert!(
        open.lexical.hydration.request.memory_bytes >= open.lexical.admitted_retained_memory_bytes
    );
    assert!(database.physical_observation()?.physical_page_reads >= reads_before);
    let reads_after_open = database.physical_observation()?.physical_page_reads;

    let options = AnnSearchOptions::new(3, 16, Some(16))?;
    let request = NativeHybridReadViewQuery {
        vector_query: &query,
        ann_options: options,
        maximum_partitions: 1,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    };
    let first = view.search_selected(&request)?;
    assert_eq!(
        database.physical_observation()?.physical_page_reads,
        reads_after_open
    );
    assert_hybrid_read_view_receipts(&open, &first);
    assert_eq!(
        first.ann.search.routing_outcome,
        AnnPartitionRoutingOutcome::SelectedCertified
    );
    assert!(matches!(first.outcome, NativeHybridOutcome::Matches(_)));

    let mut later = database.begin(1, DurabilityClass::Memory)?;
    later.replace_document(lexical, 8_u128.to_be_bytes().to_vec(), "rare rare rare")?;
    later.upsert_vector(vectors, ObjectId::new(8)?, Vector::new([2.0, 0.0])?)?;
    later.commit()?;
    let reads_after_root_advance = database.physical_observation()?.physical_page_reads;
    for sequence in 2..=128_u64 {
        let receipt = view.search_selected(&request)?;
        assert_eq!(receipt.execution_sequence, sequence);
        assert_eq!(receipt.outcome, first.outcome);
    }
    assert_eq!(
        database.physical_observation()?.physical_page_reads,
        reads_after_root_advance
    );
    let scheduled = view.search_selected_with_worker_budget(&request, 1, Duration::ZERO)?;
    assert_eq!(scheduled.ann.requested_worker_limit, 1);
    let cancellation = database
        .resource_governor()
        .ok_or("missing governor")?
        .cancellation_token();
    cancellation.cancel();
    assert!(matches!(
        view.search_selected_with_cancellation(&request, &cancellation),
        Err(NativeHybridError::Runtime(
            NativeRuntimeError::ResourceQueue(hyphae_native_runtime::GovernorQueueError::Cancelled)
        ))
    ));
    assert!(view.search_selected(&request).is_ok());

    let invalid_fusion = NativeHybridReadViewQuery {
        fusion: NativeHybridFusion {
            lexical_weight: 0,
            ..request.fusion.clone()
        },
        ..request.clone()
    };
    assert!(matches!(
        view.search_selected(&invalid_fusion),
        Err(NativeHybridError::InvalidRequest)
    ));
    Ok(())
}

#[test]
fn hybrid_read_view_never_hydrates_or_reads_an_unneeded_document_blob()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(201)?;
    let vectors = ObjectId::new(202)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "blob_documents")?;
    seed.create_vector_index(
        vectors,
        "blob_vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
    )?;
    let large_text = format!("rare {}", "ordinary ".repeat(1_100));
    seed.index_document(lexical, 1_u128.to_be_bytes().to_vec(), &large_text)?;
    seed.commit()?;
    let plan = database.plan_initial_ann_bulk(
        vectors,
        vec![(ObjectId::new(1)?, Vector::new([1.0, 0.0])?)],
        1,
    )?;
    database.publish_initial_ann_bulk(plan, DurabilityClass::Memory)?;
    install_governor(&mut database, &path)?;

    let blob_path = std::fs::read_dir(path.join("blobs"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .extension()
                .is_some_and(|extension| extension == "hyblob")
        })
        .ok_or("fixture did not externalize its lexical document")?;
    std::fs::remove_file(blob_path)?;

    let request = NativeHybridReadViewOpenRequest {
        lexical: NativeLexicalReadViewOpenRequest {
            index: lexical,
            query: "rare",
            limit: 1,
            maximum_retained_postings: 1,
            maximum_retained_bytes: 64 * 1_024,
        },
        vector_index: vectors,
    };
    let (view, open) = database.open_hybrid_read_view(&request)?;
    let query = Vector::new([1.0, 0.0])?;
    let receipt = view.search_selected(&NativeHybridReadViewQuery {
        vector_query: &query,
        ann_options: AnnSearchOptions::new(1, 8, Some(8))?,
        maximum_partitions: 1,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 1,
        },
    })?;
    assert_eq!(open.lexical.retained_postings, 1);
    assert_eq!(receipt.lexical.postings_evaluated, 1);
    assert_eq!(receipt.lexical.physical_page_reads, 0);
    assert!(matches!(receipt.outcome, NativeHybridOutcome::Matches(_)));
    Ok(())
}

#[test]
fn hybrid_read_view_physical_plan_covers_a_maximum_inline_document_record()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(211)?;
    let vectors = ObjectId::new(212)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "inline_documents")?;
    seed.create_vector_index(
        vectors,
        "inline_vectors",
        2,
        VectorMetric::SquaredL2,
        config()?,
    )?;
    let inline_text = (0..1_023)
        .map(|index| format!("t{index:04}"))
        .collect::<Vec<_>>()
        .join(" ");
    let inline_text = format!("rare {inline_text}");
    assert!(inline_text.len() > 6_000 && inline_text.len() <= 8_192);
    seed.index_document(lexical, 1_u128.to_be_bytes().to_vec(), &inline_text)?;
    seed.commit()?;
    let plan = database.plan_initial_ann_bulk(
        vectors,
        vec![(ObjectId::new(1)?, Vector::new([1.0, 0.0])?)],
        1,
    )?;
    database.publish_initial_ann_bulk(plan, DurabilityClass::Memory)?;
    install_governor(&mut database, &path)?;
    assert_eq!(std::fs::read_dir(path.join("blobs"))?.count(), 0);

    let request = NativeHybridReadViewOpenRequest {
        lexical: NativeLexicalReadViewOpenRequest {
            index: lexical,
            query: "rare",
            limit: 1,
            maximum_retained_postings: 1,
            maximum_retained_bytes: 64 * 1_024,
        },
        vector_index: vectors,
    };
    let (_view, open) = database.open_hybrid_read_view(&request)?;
    assert_eq!(open.lexical.observed_physical_entries, 4);
    assert_eq!(open.lexical.observed_physical_bytes, 6_334);
    assert_eq!(open.lexical.planned_physical_bytes, 16_502);
    assert!(open.lexical.planned_physical_bytes >= open.lexical.observed_physical_bytes);
    Ok(())
}

#[test]
fn hybrid_read_view_cancellation_wakes_queued_lexical_admission_without_partial_receipt()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let retained_view_memory = governor.usage_snapshot().memory_bytes;
    let held = governor.try_admit_owned(
        WorkloadClass::ForegroundBounded,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 2,
            io_slots: 0,
            memory_bytes: 0,
        },
    )?;
    let cancellation = governor.cancellation_token();
    let worker_cancellation = cancellation.clone();
    let worker_view = view.clone();
    let ann_options = AnnSearchOptions::new(3, 16, Some(16))?;
    let worker = std::thread::spawn(move || {
        let request = NativeHybridReadViewQuery {
            vector_query: &query,
            ann_options,
            maximum_partitions: 1,
            fusion: NativeHybridFusion {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 3,
            },
        };
        worker_view.search_selected_with_cancellation(&request, &worker_cancellation)
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while governor.queued_requests() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(governor.queued_requests(), 1);
    assert_eq!(governor.usage_snapshot().memory_bytes, retained_view_memory);
    let cancelled_at = std::time::Instant::now();
    cancellation.cancel();
    assert!(matches!(
        worker.join().map_err(|_| "queued search panicked")?,
        Err(NativeHybridError::Runtime(
            NativeRuntimeError::ResourceQueue(hyphae_native_runtime::GovernorQueueError::Cancelled)
        ))
    ));
    assert!(cancelled_at.elapsed() < Duration::from_millis(500));
    assert_eq!(governor.queued_requests(), 0);
    assert_eq!(governor.usage_snapshot().memory_bytes, retained_view_memory);
    drop(held);
    assert_eq!(governor.usage_snapshot().compute_threads, 0);
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    Ok(())
}

#[test]
fn hybrid_result_retention_is_admitted_before_the_first_branch()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    let lexical_view = view.lexical_view();
    let ann_view = view.ann_view();
    let ann_options = AnnSearchOptions::new(3, 16, Some(16))?;

    let lexical_probe = lexical_view.search()?;
    let ann_probe = ann_view.search_selected(&query, ann_options, 1)?;
    let branch_memory = lexical_probe
        .execution
        .request
        .memory_bytes
        .checked_add(1_024)
        .ok_or("branch memory overflow")?;
    assert!(ann_probe.execution.request.memory_bytes > branch_memory);

    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let baseline = governor.usage_snapshot().memory_bytes;
    let held_memory = governor
        .policy()
        .memory_bytes
        .checked_sub(baseline)
        .and_then(|available| available.checked_sub(branch_memory))
        .ok_or("fixture leaves insufficient memory for the lexical branch")?;
    let held = governor.try_admit_owned(
        WorkloadClass::ForegroundBounded,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 0,
            io_slots: 0,
            memory_bytes: held_memory,
        },
    )?;
    let cancellation = governor.cancellation_token();
    let worker_cancellation = cancellation.clone();
    let worker_view = view.clone();
    let worker = std::thread::spawn(move || {
        let request = NativeHybridReadViewQuery {
            vector_query: &query,
            ann_options,
            maximum_partitions: 1,
            fusion: NativeHybridFusion {
                lexical_weight: 1,
                vector_weight: 1,
                limit: 3,
            },
        };
        worker_view.search_selected_with_cancellation(&request, &worker_cancellation)
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while governor.queued_requests() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(governor.queued_requests(), 1);
    cancellation.cancel();
    assert!(matches!(
        worker.join().map_err(|_| "queued search panicked")?,
        Err(NativeHybridError::Runtime(
            NativeRuntimeError::ResourceQueue(hyphae_native_runtime::GovernorQueueError::Cancelled)
        ))
    ));
    drop(held);

    let after_cancel = lexical_view.search()?;
    assert_eq!(
        after_cancel.execution_sequence, 2,
        "the cancelled hybrid query must not execute the lexical branch"
    );
    drop(ann_view);
    drop(lexical_view);
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    Ok(())
}

#[test]
fn hybrid_worker_queue_budget_applies_to_the_lexical_branch_and_releases_retention()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    let lexical_view = view.lexical_view();
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let baseline = governor.usage_snapshot().memory_bytes;
    let held = governor.try_admit_owned(
        WorkloadClass::ForegroundBounded,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 2,
            io_slots: 0,
            memory_bytes: 0,
        },
    )?;
    let request = NativeHybridReadViewQuery {
        vector_query: &query,
        ann_options: AnnSearchOptions::new(3, 16, Some(16))?,
        maximum_partitions: 1,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    };

    let started = std::time::Instant::now();
    let result = view.search_selected_with_worker_budget(&request, 1, Duration::from_millis(10));
    assert!(
        matches!(
            result,
            Err(NativeHybridError::Runtime(
                NativeRuntimeError::ResourceQueue(
                    hyphae_native_runtime::GovernorQueueError::TimedOut
                )
            ))
        ),
        "unexpected bounded-wait result: {result:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(governor.usage_snapshot().memory_bytes, baseline);
    drop(held);
    assert_eq!(lexical_view.search()?.execution_sequence, 1);
    drop(lexical_view);
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    Ok(())
}

#[test]
fn hybrid_peak_admission_prevents_result_retention_hold_and_wait()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let request = NativeHybridReadViewQuery {
        vector_query: &query,
        ann_options: AnnSearchOptions::new(3, 16, Some(16))?,
        maximum_partitions: 1,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    };
    let probe = view.search_selected_with_worker_budget(&request, 1, Duration::from_secs(2))?;
    let peak_memory = probe.peak_admission.request.memory_bytes;
    let retention_memory = probe.result_retention.request.memory_bytes;
    assert!(peak_memory > retention_memory);
    let baseline = governor.usage_snapshot().memory_bytes;
    let remaining_for_one_peak = peak_memory
        .checked_add(retention_memory)
        .ok_or("test capacity overflow")?;
    let held_memory = governor
        .policy()
        .memory_bytes
        .checked_sub(baseline)
        .and_then(|available| available.checked_sub(remaining_for_one_peak))
        .ok_or("test policy cannot isolate one peak admission")?;
    let held = governor.try_admit_owned(
        WorkloadClass::ForegroundBounded,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 0,
            io_slots: 0,
            memory_bytes: held_memory,
        },
    )?;
    let start = Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    let ann_options = AnnSearchOptions::new(3, 16, Some(16))?;
    for _ in 0..2 {
        let worker_view = view.clone();
        let worker_query = query.clone();
        let worker_start = Arc::clone(&start);
        workers.push(std::thread::spawn(move || {
            let request = NativeHybridReadViewQuery {
                vector_query: &worker_query,
                ann_options,
                maximum_partitions: 1,
                fusion: NativeHybridFusion {
                    lexical_weight: 1,
                    vector_weight: 1,
                    limit: 3,
                },
            };
            worker_start.wait();
            worker_view.search_selected_with_worker_budget(&request, 1, Duration::from_secs(2))
        }));
    }
    let mut results = Vec::with_capacity(workers.len());
    for worker in workers {
        results.push(worker.join().map_err(|_| "hybrid search panicked")?);
    }
    assert!(results.iter().all(Result::is_ok));
    drop(held);
    drop(view);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    Ok(())
}

#[test]
fn filtered_lexical_read_view_filters_before_final_rank_on_one_root_without_hot_reads()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(301)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "filtered_documents")?;
    for id in 1..=12_u128 {
        let text = if id <= 10 { "rare rare rare" } else { "rare" };
        seed.index_document(lexical, id.to_be_bytes().to_vec(), text)?;
        seed.set(
            [b"filter:".as_slice(), id.to_be_bytes().as_slice()].concat(),
            if id == 12 {
                b"keep".to_vec()
            } else {
                b"drop".to_vec()
            },
            None,
        )?;
    }
    seed.commit()?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let request = NativeFilteredLexicalReadViewOpenRequest {
        lexical: NativeLexicalReadViewOpenRequest {
            index: lexical,
            query: "rare",
            limit: 1,
            maximum_retained_postings: 16,
            maximum_retained_bytes: 256 * 1_024,
        },
        filter: NativeStructureScalarFilter {
            key_prefix: b"filter:",
            expected_inline_value: b"keep",
            logical_time_micros: 0,
        },
    };
    let (view, open) = database.open_filtered_lexical_read_view(&request)?;
    assert_eq!(
        open.structure_filter_identity_algorithm,
        NATIVE_STRUCTURE_FILTER_IDENTITY_ALGORITHM
    );
    assert_eq!(open.retained_filter_records, 12);
    assert_eq!(open.observed_filter_physical_entries, 12);
    assert!(open.observed_filter_physical_bytes <= open.planned_filter_physical_bytes);
    assert_eq!(open.filter_hydration.request.compute_threads, 1);
    assert_eq!(open.filter_hydration.request.io_slots, 1);
    assert_eq!(open.filter_planning.request.compute_threads, 1);
    assert_eq!(open.filter_planning.request.io_slots, 1);
    let reads_after_open = database.physical_observation()?.physical_page_reads;
    let receipt = view.search()?;
    assert_eq!(receipt.execution_sequence, 1);
    assert_eq!(receipt.filter_execution, NATIVE_STRUCTURE_FILTER_EXECUTION);
    assert_eq!(receipt.root_identity, open.root_identity);
    assert_eq!(
        receipt.lexical_index_identity,
        open.lexical.lexical_index_identity
    );
    assert_eq!(
        receipt.structure_filter_identity,
        open.structure_filter_identity
    );
    assert_eq!(receipt.filter_records_evaluated, 12);
    assert_eq!(receipt.filter_records_matched, 1);
    assert_eq!(receipt.postings_scored, 1);
    assert_eq!(receipt.hits.len(), 1);
    assert_eq!(receipt.hits[0].document_id, 12_u128.to_be_bytes());
    assert_eq!(receipt.physical_page_reads, 0);
    assert_eq!(
        database.physical_observation()?.physical_page_reads,
        reads_after_open
    );

    let mut later = database.begin(1, DurabilityClass::Memory)?;
    later.set(
        [b"filter:".as_slice(), 1_u128.to_be_bytes().as_slice()].concat(),
        b"keep".to_vec(),
        None,
    )?;
    later.commit()?;
    let second = view.search()?;
    assert_eq!(second.execution_sequence, 2);
    assert_eq!(second.hits, receipt.hits);
    Ok(())
}

#[test]
fn filtered_view_derives_from_the_existing_hybrid_lexical_authority_once()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, _query) = seed_read_view_fixture(&mut database)?;
    let mut filters = database.begin(0, DurabilityClass::Memory)?;
    for id in 1..=3_u128 {
        filters.set(
            [b"filter:".as_slice(), id.to_be_bytes().as_slice()].concat(),
            b"keep".to_vec(),
            None,
        )?;
    }
    filters.commit()?;
    install_governor(&mut database, &path)?;
    let (hybrid, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    let lexical_view = hybrid.lexical_view();
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let memory_before_filter = governor.usage_snapshot().memory_bytes;
    let reads_before_filter = database.physical_observation()?.physical_page_reads;

    let (filtered, open) = database.open_filtered_lexical_read_view_from_lexical(
        &lexical_view,
        &NativeStructureScalarFilter {
            key_prefix: b"filter:",
            expected_inline_value: b"keep",
            logical_time_micros: 0,
        },
    )?;
    assert_eq!(&open.lexical, lexical_view.open_receipt());
    assert_eq!(
        open.root_identity,
        lexical_view.open_receipt().root_identity
    );
    assert_eq!(
        governor.usage_snapshot().memory_bytes - memory_before_filter,
        open.retained_filter_memory_bytes
    );
    assert_eq!(
        database.physical_observation()?.physical_page_reads - reads_before_filter,
        open.physical_page_reads
    );

    drop(hybrid);
    drop(lexical_view);
    assert!(governor.usage_snapshot().memory_bytes > open.retained_filter_memory_bytes);
    assert_eq!(filtered.search()?.hits.len(), 3);
    drop(filtered);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    Ok(())
}

#[test]
fn filtered_read_view_execution_sequence_is_gapless_under_concurrency()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(305)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "concurrent_filtered_documents")?;
    for id in 1..=8_u128 {
        seed.index_document(lexical, id.to_be_bytes().to_vec(), "rare native")?;
        seed.set(
            [b"filter:".as_slice(), id.to_be_bytes().as_slice()].concat(),
            b"keep".to_vec(),
            None,
        )?;
    }
    seed.commit()?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) =
        database.open_filtered_lexical_read_view(&NativeFilteredLexicalReadViewOpenRequest {
            lexical: NativeLexicalReadViewOpenRequest {
                index: lexical,
                query: "rare",
                limit: 8,
                maximum_retained_postings: 8,
                maximum_retained_bytes: 256 * 1_024,
            },
            filter: NativeStructureScalarFilter {
                key_prefix: b"filter:",
                expected_inline_value: b"keep",
                logical_time_micros: 0,
            },
        })?;
    let start = Arc::new(std::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let worker_view = view.clone();
        let worker_start = Arc::clone(&start);
        workers.push(std::thread::spawn(move || {
            worker_start.wait();
            worker_view
                .search()
                .map(|receipt| receipt.execution_sequence)
        }));
    }
    let mut sequences = Vec::with_capacity(workers.len());
    for worker in workers {
        sequences.push(worker.join().map_err(|_| "filtered search panicked")??);
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn filtered_read_view_cancellation_releases_queued_admission_and_preserves_reuse()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(307)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "cancelled_filtered_documents")?;
    seed.index_document(lexical, 1_u128.to_be_bytes().to_vec(), "rare")?;
    seed.set(
        [b"filter:".as_slice(), 1_u128.to_be_bytes().as_slice()].concat(),
        b"keep".to_vec(),
        None,
    )?;
    seed.commit()?;
    install_governor_with_wait(&mut database, &path, Duration::from_secs(2))?;
    let (view, _) =
        database.open_filtered_lexical_read_view(&NativeFilteredLexicalReadViewOpenRequest {
            lexical: NativeLexicalReadViewOpenRequest {
                index: lexical,
                query: "rare",
                limit: 1,
                maximum_retained_postings: 1,
                maximum_retained_bytes: 64 * 1_024,
            },
            filter: NativeStructureScalarFilter {
                key_prefix: b"filter:",
                expected_inline_value: b"keep",
                logical_time_micros: 0,
            },
        })?;
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let baseline = governor.usage_snapshot().memory_bytes;
    let held = governor.try_admit_owned(
        WorkloadClass::ForegroundBounded,
        hyphae_native_runtime::GovernorRequest {
            compute_threads: 2,
            io_slots: 0,
            memory_bytes: 0,
        },
    )?;
    let cancellation = governor.cancellation_token();
    let worker_cancellation = cancellation.clone();
    let worker_view = view.clone();
    let worker = std::thread::spawn(move || {
        worker_view.search_with_cancellation(Some(&worker_cancellation))
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while governor.queued_requests() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(governor.queued_requests(), 1);
    cancellation.cancel();
    assert!(matches!(
        worker.join().map_err(|_| "filtered search panicked")?,
        Err(NativeRuntimeError::ResourceQueue(
            hyphae_native_runtime::GovernorQueueError::Cancelled
        ))
    ));
    assert_eq!(governor.usage_snapshot().memory_bytes, baseline);
    drop(held);
    assert_eq!(view.search()?.execution_sequence, 1);
    Ok(())
}

#[test]
fn filtered_lexical_read_view_retains_unique_candidates_and_counts_missing_records_honestly()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(311)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "missing_filters")?;
    seed.index_document(lexical, 1_u128.to_be_bytes().to_vec(), "rare native")?;
    seed.set(
        [b"filter:".as_slice(), 1_u128.to_be_bytes().as_slice()].concat(),
        b"keep".to_vec(),
        None,
    )?;
    seed.index_document(lexical, 2_u128.to_be_bytes().to_vec(), "rare native")?;
    seed.commit()?;
    install_governor(&mut database, &path)?;
    let (view, open) =
        database.open_filtered_lexical_read_view(&NativeFilteredLexicalReadViewOpenRequest {
            lexical: NativeLexicalReadViewOpenRequest {
                index: lexical,
                query: "rare native",
                limit: 10,
                maximum_retained_postings: 8,
                maximum_retained_bytes: 128 * 1_024,
            },
            filter: NativeStructureScalarFilter {
                key_prefix: b"filter:",
                expected_inline_value: b"keep",
                logical_time_micros: 0,
            },
        })?;
    assert_eq!(open.lexical.retained_postings, 4);
    assert_eq!(open.retained_filter_records, 2);
    assert_eq!(open.observed_filter_physical_entries, 1);
    let receipt = view.search()?;
    assert_eq!(receipt.filter_records_evaluated, 2);
    assert_eq!(receipt.filter_records_matched, 1);
    assert_eq!(receipt.hits.len(), 1);
    Ok(())
}

#[test]
fn filtered_lexical_read_view_rejects_blob_backed_predicates_before_returning_a_view()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let lexical = ObjectId::new(321)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "blob_filter")?;
    seed.index_document(lexical, 1_u128.to_be_bytes().to_vec(), "rare")?;
    seed.set(
        [b"filter:".as_slice(), 1_u128.to_be_bytes().as_slice()].concat(),
        vec![b'x'; 8_193],
        None,
    )?;
    seed.commit()?;
    install_governor(&mut database, &path)?;
    let error = database
        .open_filtered_lexical_read_view(&NativeFilteredLexicalReadViewOpenRequest {
            lexical: NativeLexicalReadViewOpenRequest {
                index: lexical,
                query: "rare",
                limit: 1,
                maximum_retained_postings: 2,
                maximum_retained_bytes: 64 * 1_024,
            },
            filter: NativeStructureScalarFilter {
                key_prefix: b"filter:",
                expected_inline_value: b"keep",
                logical_time_micros: 0,
            },
        })
        .err()
        .ok_or("blob-backed filter unexpectedly opened")?;
    assert!(matches!(
        error,
        NativeRuntimeError::Model(message)
            if message == "structure-filter-inline-scalar-only-v1"
    ));
    assert_eq!(
        database
            .resource_governor()
            .ok_or("missing governor")?
            .usage_snapshot()
            .memory_bytes,
        0
    );
    Ok(())
}

#[test]
fn hybrid_read_view_fails_before_ann_hydration_when_lexical_budget_is_insufficient()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, _query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;
    let mut request = read_view_open_request(lexical, vectors);
    request.lexical.maximum_retained_postings = 2;
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    let error = database
        .open_hybrid_read_view(&request)
        .err()
        .ok_or("posting ceiling unexpectedly admitted")?;
    assert!(
        error
            .to_string()
            .contains("lexical-read-view-retention-postings required=3 maximum=2")
    );
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        restores_before
    );
    Ok(())
}

#[test]
fn hybrid_read_view_byte_ceiling_fails_even_when_posting_count_fits()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, _query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;
    let mut request = read_view_open_request(lexical, vectors);
    request.lexical.maximum_retained_bytes = 128;
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    let error = database
        .open_hybrid_read_view(&request)
        .err()
        .ok_or("byte ceiling unexpectedly admitted")?;
    assert!(
        error
            .to_string()
            .contains("lexical-read-view-retention-bytes")
    );
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        restores_before
    );
    assert_eq!(
        database
            .resource_governor()
            .ok_or("missing governor")?
            .usage_snapshot()
            .memory_bytes,
        0
    );
    Ok(())
}

#[test]
fn hybrid_read_view_rejects_an_unbounded_missing_term_query_before_ann_hydration()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, _query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;
    let oversized = "missing ".repeat(600);
    let request = NativeHybridReadViewOpenRequest {
        lexical: NativeLexicalReadViewOpenRequest {
            index: lexical,
            query: &oversized,
            limit: 1,
            maximum_retained_postings: 16,
            maximum_retained_bytes: 64 * 1_024,
        },
        vector_index: vectors,
    };
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    assert!(database.open_hybrid_read_view(&request).is_err());
    assert_eq!(
        NativeDatabase::process_ann_index_restore_count(),
        restores_before
    );
    assert_eq!(
        database
            .resource_governor()
            .ok_or("missing governor")?
            .usage_snapshot()
            .memory_bytes,
        0
    );
    Ok(())
}

#[test]
fn dropped_database_invalidates_hybrid_read_view_without_cached_results()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical, vectors, query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;
    let (view, _) = database.open_hybrid_read_view(&read_view_open_request(lexical, vectors))?;
    drop(database);
    let request = NativeHybridReadViewQuery {
        vector_query: &query,
        ann_options: AnnSearchOptions::new(3, 16, Some(16))?,
        maximum_partitions: 1,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    };
    assert!(matches!(
        view.search_selected(&request),
        Err(NativeHybridError::Runtime(
            NativeRuntimeError::AnnReadViewDatabaseClosed
        ))
    ));
    Ok(())
}

#[test]
fn hybrid_sibling_handles_share_one_hydration_and_release_on_last_drop()
-> Result<(), Box<dyn std::error::Error>> {
    let _guard = READ_VIEW_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = TemporaryDirectory::create()?;
    let path = temporary.0.join("data");
    let mut database = NativeDatabase::create(&path)?;
    let (lexical_index, vector_index, _query) = seed_read_view_fixture(&mut database)?;
    install_governor(&mut database, &path)?;
    let governor = Arc::clone(database.resource_governor().ok_or("missing governor")?);
    let (hybrid, open) =
        database.open_hybrid_read_view(&read_view_open_request(lexical_index, vector_index))?;
    assert_eq!(open.ann.hydration_restore_count, 1);
    let lexical = hybrid.lexical_view();
    let ann = hybrid.ann_view();
    let retained = governor.usage_snapshot().memory_bytes;
    assert!(retained > 0);
    drop(hybrid);
    assert_eq!(governor.usage_snapshot().memory_bytes, retained);
    assert!(matches!(
        database.clear_resource_governor(),
        Err(NativeRuntimeError::OutstandingAnnReadViews { count: 2 })
    ));
    drop(lexical);
    assert!(matches!(
        database.clear_resource_governor(),
        Err(NativeRuntimeError::OutstandingAnnReadViews { count: 1 })
    ));
    drop(ann);
    assert_eq!(governor.usage_snapshot().memory_bytes, 0);
    database.clear_resource_governor()?;
    Ok(())
}
