// SPDX-License-Identifier: GPL-3.0-only

//! Controlled Native G7 benchmark runner.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Barrier, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use hyphae_client::v2::{HyphaeClient, RequestOptions};
use hyphae_native_catalog::IncrementalVectorLifecycle;
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    NativeProduct, ProductAuthorization, ProductDurabilityPolicy, ProductOperation,
    ProductPrincipal, ProductRequestContext, ProductSession, ProductSessionId,
};
use hyphae_native_runtime::{
    AnnSearchOptions, HardwareProfile, HnswConfig, InitialAnnBulkBuildEvidence,
    InitialAnnBulkBuilder, InitialAnnBulkProgress, InitialAnnBulkProgressStage,
    MAX_INITIAL_ANN_BULK_PARTITIONS, NativeCommitScheduler, NativeDatabase, NativeExecutionPool,
    NativeExecutionTopology, NativeGovernorPolicy, NativeResourceGovernor, NativeSnapshot, Vector,
    VectorMetric, WorkloadClass,
};
use hyphae_native_types::ObjectId;
use serde_json::json;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

const VERSION: &str = "hyphae-native-g7-receipt-v3";
const DEFAULT_OBSERVATIONS: usize = 1_000_000;
const DEFAULT_WARMUP: usize = 100_000;
const STRUCTURE_KEYS: usize = 2_048;
const SQL_KEYS: usize = 128;
const CLOSURE_SEARCH_DOCUMENTS: usize = 1_000_000;
const CLOSURE_VECTOR_DIMENSION: u16 = 384;
const K: usize = 10;
const ANN_QUERY_BREADTH: usize = 64;
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(10);
const ANN_DELTA_MAX_ENTRIES: u32 = 4_096;
const ANN_CONSOLIDATE_AFTER_DELTAS: u16 = 4_096;
const CORPUS_GENERATOR: &str =
    "hyphae-native-g7-corpus-v3:durable-partitioned-hnsw-v1-id-linear-vector-and-rare-term";

#[derive(Clone, Copy, Debug)]
struct Stats {
    p50: u64,
    p95: u64,
    p99: u64,
    p999: u64,
    maximum: u64,
    throughput: f64,
}

struct SearchFixture {
    database: NativeDatabase,
    snapshot: NativeSnapshot,
    lexical_index: ObjectId,
    vector_index: ObjectId,
    query: Vector,
    options: AnnSearchOptions,
    recall_at_10: f64,
    initial_ann_bulk: serde_json::Value,
}

struct ExecutionAuthority {
    profile: HardwareProfile,
    policy: NativeGovernorPolicy,
    topology: NativeExecutionTopology,
    topology_digest: String,
}

struct AnnProgressSink {
    path: PathBuf,
    source_commit: String,
    source_tree: String,
    dataset_digest: String,
    started: Instant,
    sequence: u64,
    total_vectors: usize,
    completed_vectors: usize,
}

struct AnnProgressUpdate<'a> {
    operation: &'a str,
    stage: &'a str,
    completed: usize,
    status: &'a str,
    checkpoint_digest: Option<String>,
    details: Option<serde_json::Value>,
}

impl AnnProgressSink {
    fn from_environment(source_commit: &str) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(path) = std::env::var_os("HYPHAE_G7_PROGRESS_FILE").map(PathBuf::from) else {
            return Ok(None);
        };
        let source_tree = std::env::var("HYPHAE_G7_SOURCE_TREE")
            .map_err(|_| "G7 progress requires HYPHAE_G7_SOURCE_TREE")?;
        Ok(Some(Self {
            path,
            source_commit: source_commit.to_owned(),
            source_tree,
            dataset_digest: dataset_digest(source_commit).to_hex().to_string(),
            started: Instant::now(),
            sequence: 0,
            total_vectors: search_documents(),
            completed_vectors: 0,
        }))
    }

    fn begin_build(&mut self, authority: &ExecutionAuthority) -> std::io::Result<()> {
        self.write(AnnProgressUpdate {
            operation: "ann-bulk-build",
            stage: "ann-private-build",
            completed: 0,
            status: "running",
            checkpoint_digest: None,
            details: Some(json!({
                "builder": "partitioned-hnsw-v1",
                "requested_partitions": authority.topology.worker_count(),
                "topology_workers": authority.topology.worker_count(),
                "topology_digest": authority.topology_digest,
                "planned_workers": null,
                "planned_memory_bytes": null,
                "worker_batches": null,
            })),
        })
    }

    fn begin_publication(&mut self, evidence: &serde_json::Value) -> std::io::Result<()> {
        self.write(AnnProgressUpdate {
            operation: "ann-bulk-build",
            stage: "ann-publication",
            completed: self.total_vectors,
            status: "running",
            checkpoint_digest: None,
            details: Some(evidence.clone()),
        })
    }

    fn update_build(&mut self, progress: InitialAnnBulkProgress) -> std::io::Result<()> {
        let stage = match progress.stage {
            InitialAnnBulkProgressStage::Planning => "ann-planning",
            InitialAnnBulkProgressStage::Building => "ann-child-build",
        };
        let completed = if progress.stage == InitialAnnBulkProgressStage::Planning {
            0
        } else {
            self.total_vectors
                .checked_mul(progress.completed)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress multiplication overflow"))?
                .checked_div(progress.total)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress total must be nonzero"))?
        };
        self.write(AnnProgressUpdate {
            operation: "ann-bulk-build",
            stage,
            completed,
            status: "running",
            checkpoint_digest: None,
            details: Some(json!({
                "builder": "partitioned-hnsw-v1",
                "unit": if progress.stage == InitialAnnBulkProgressStage::Planning {
                    "plan"
                } else {
                    "child-generation"
                },
                "stage_completed": progress.completed,
                "stage_total": progress.total,
            })),
        })
    }

    fn complete(
        &mut self,
        operation: &str,
        checkpoint: [u8; 32],
        evidence: &serde_json::Value,
    ) -> std::io::Result<()> {
        self.write(AnnProgressUpdate {
            operation,
            stage: "ann-published",
            completed: self.total_vectors,
            status: "completed",
            checkpoint_digest: Some(blake3::Hash::from_bytes(checkpoint).to_hex().to_string()),
            details: Some(evidence.clone()),
        })
    }

    fn write(&mut self, update: AnnProgressUpdate<'_>) -> std::io::Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("G7 progress sequence overflow"))?;
        if update.completed > self.total_vectors {
            return Err(std::io::Error::other(
                "G7 ANN progress completed vectors exceed dataset total",
            ));
        }
        self.completed_vectors = self.completed_vectors.max(update.completed);
        let mut details = update.details.unwrap_or_else(|| json!({}));
        if let Some(details) = details.as_object_mut() {
            details.insert(
                "eta".to_owned(),
                progress_eta(
                    self.started.elapsed(),
                    self.completed_vectors,
                    self.total_vectors,
                    update.status == "completed",
                )?,
            );
        }
        let elapsed_nanos = u64::try_from(self.started.elapsed().as_nanos())
            .map_err(|_| std::io::Error::other("G7 ANN progress elapsed time exceeds u64"))?;
        let record = json!({
            "schema": "hyphae-native-performance-progress-v1",
            "source_commit": self.source_commit,
            "source_tree": self.source_tree,
            "dataset_digest": self.dataset_digest,
            "operation": update.operation,
            "stage": update.stage,
            "sequence": self.sequence,
            "completed_units": self.completed_vectors,
            "total_units": self.total_vectors,
            "unit": "vectors",
            "elapsed_nanos": elapsed_nanos,
            "status": update.status,
            "checkpoint_digest": update.checkpoint_digest,
            "details": details,
        });
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("G7 progress path has no parent"))?;
        fs::create_dir_all(parent)?;
        let staging = parent.join(format!(
            ".{}-{}-{}",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("g7-progress"),
            std::process::id(),
            self.sequence,
        ));
        let encoded = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
        fs::write(&staging, encoded)?;
        fs::rename(staging, &self.path)
    }
}

impl ExecutionAuthority {
    fn from_environment(data_path: &Path) -> Result<Self, Box<dyn Error>> {
        let expected_profile = read_required_json("HYPHAE_G7_HARDWARE_PROFILE_FILE")?;
        let policy: NativeGovernorPolicy =
            serde_json::from_value(read_required_json("HYPHAE_G7_GOVERNOR_POLICY_FILE")?)
                .map_err(|error| format!("invalid G7 governor policy: {error}"))?;
        let expected_topology = read_required_json("HYPHAE_G7_EXECUTION_TOPOLOGY_FILE")?;
        let profile = HardwareProfile::discover(data_path)
            .map_err(|error| format!("G7 hardware discovery failed: {error}"))?;
        let expected_fingerprint = required_json_string(&expected_profile, "fingerprint")?;
        if expected_fingerprint != profile.fingerprint {
            return Err("live G7 hardware differs from the supplied hardware profile".into());
        }
        if policy.hardware_fingerprint != profile.fingerprint {
            return Err("G7 governor policy targets another hardware profile".into());
        }
        let topology = NativeExecutionTopology::derive(&profile, &policy)
            .map_err(|error| format!("G7 execution topology derivation failed: {error}"))?;
        let actual_topology = serde_json::to_value(&topology)?;
        if actual_topology != expected_topology {
            return Err("live G7 execution topology differs from the supplied topology".into());
        }
        if topology.worker_count() == 0 {
            return Err("G7 execution topology has no workers".into());
        }
        if topology.worker_count() > MAX_INITIAL_ANN_BULK_PARTITIONS {
            return Err(format!(
                "G7 execution topology has {} workers but durable ANN supports at most {} partitions",
                topology.worker_count(),
                MAX_INITIAL_ANN_BULK_PARTITIONS,
            )
            .into());
        }
        let topology_digest = blake3::hash(&serde_json::to_vec(&actual_topology)?)
            .to_hex()
            .to_string();
        Ok(Self {
            profile,
            policy,
            topology,
            topology_digest,
        })
    }

    fn install(&self, database: &mut NativeDatabase) -> Result<(), Box<dyn Error>> {
        let governor = Arc::new(NativeResourceGovernor::new(self.policy.clone()));
        let execution_pool = Arc::new(
            NativeExecutionPool::new(&self.profile, &self.policy)
                .map_err(|error| format!("G7 execution pool creation failed: {error}"))?,
        );
        database
            .set_resource_governor_with_execution_pool(
                Arc::clone(&governor),
                Arc::clone(&execution_pool),
                Duration::ZERO,
            )
            .map_err(|error| format!("G7 execution authority install failed: {error}"))?;
        if database.resource_governor().is_none() || database.execution_pool().is_none() {
            return Err("G7 database did not retain its execution authority".into());
        }
        Ok(())
    }
}

impl SearchFixture {
    fn open_or_create(root: &Path, source_commit: &str) -> Result<Self, Box<dyn Error>> {
        let path = search_seed_path(root, source_commit)?;
        let authority = ExecutionAuthority::from_environment(&path)?;
        let created = !path.is_dir();
        if !path.is_dir() {
            publish_search_seed(&path, source_commit, &authority)?;
        }
        let database =
            NativeDatabase::open(&path).map_err(|error| format!("search seed open: {error}"))?;
        let lexical_index = ObjectId::new(7)?;
        let vector_index = ObjectId::new(8)?;
        let initial_ann_bulk = load_initial_ann_bulk_evidence(&path, source_commit, &authority)?;
        let observed = database.observe_ann_index(vector_index)?;
        let aggregate_identity = required_json_string(&initial_ann_bulk, "aggregate_identity")?;
        let observed_identity = blake3::Hash::from_bytes(observed.base_identity)
            .to_hex()
            .to_string();
        if aggregate_identity != observed_identity {
            return Err("published G7 ANN base differs from its durable build evidence".into());
        }
        if !created && let Some(sink) = AnnProgressSink::from_environment(source_commit)?.as_mut() {
            sink.complete("ann-seed-verify", observed.base_identity, &initial_ann_bulk)?;
        }
        Self::from_database(database, lexical_index, vector_index, initial_ann_bulk)
    }

    fn from_database(
        database: NativeDatabase,
        lexical_index: ObjectId,
        vector_index: ObjectId,
        initial_ann_bulk: serde_json::Value,
    ) -> Result<Self, Box<dyn Error>> {
        let snapshot = database.snapshot(0)?;
        let query = Vector::new({
            let mut values = vec![0.0; vector_dimension() as usize];
            values[0] = 1.0;
            values
        })?;
        let options = AnnSearchOptions::new(K, ANN_QUERY_BREADTH, Some(K))?;
        let exact_ids = snapshot
            .search_vector_exact(vector_index, &query, K)?
            .into_iter()
            .map(|hit| hit.object_id)
            .collect::<std::collections::BTreeSet<_>>();
        let approximate = snapshot.search_ann(vector_index, &query, options)?;
        let recalled = approximate
            .hits
            .iter()
            .filter(|hit| exact_ids.contains(&hit.object_id))
            .count();
        if recalled * 20 < K * 19 {
            return Err("ANN recall below G7 floor".into());
        }
        Ok(Self {
            database,
            snapshot,
            lexical_index,
            vector_index,
            query,
            options,
            recall_at_10: recalled as f64 / K as f64,
            initial_ann_bulk,
        })
    }
}

fn publish_search_seed(
    path: &Path,
    source_commit: &str,
    authority: &ExecutionAuthority,
) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("search seed path has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".search-staging-{}-{}",
        std::process::id(),
        unique_nonce()
    ));
    let evidence = seed_search_database(&staging, source_commit, authority)?;
    fs::rename(&staging, path)?;
    write_json_atomic(&initial_ann_bulk_evidence_path(path), &evidence)
}

fn seed_search_database(
    path: &Path,
    source_commit: &str,
    authority: &ExecutionAuthority,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut database =
        NativeDatabase::create(path).map_err(|error| format!("search seed: {error}"))?;
    authority.install(&mut database)?;
    let lexical_index = ObjectId::new(7)?;
    let vector_index = ObjectId::new(8)?;
    let document_count = search_documents();
    let progress =
        AnnProgressSink::from_environment(source_commit)?.map(|sink| Arc::new(Mutex::new(sink)));
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.create_search_index(lexical_index, "g7_search")?;
    seed.create_vector_index_with_lifecycle(
        vector_index,
        "g7_ann",
        vector_dimension(),
        VectorMetric::Cosine,
        HnswConfig::new(8, 32, 16, 512, 7)?,
        ann_lifecycle(),
    )?;
    seed.commit()?;
    if let Some(sink) = &progress {
        sink.lock()
            .map_err(|_| "G7 ANN progress sink synchronization failed")?
            .begin_build(authority)?;
    }
    let vectors = (0..document_count)
        .map(|id| vector_fixture(id, document_count))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let progress_failure = Arc::new(Mutex::new(None::<String>));
    let callback_sink = progress.clone();
    let callback_failure = Arc::clone(&progress_failure);
    let plan = database.plan_initial_ann_bulk_with_progress(
        vector_index,
        vectors,
        authority.topology.worker_count(),
        move |update| {
            let Some(sink) = &callback_sink else {
                return;
            };
            let outcome = sink
                .lock()
                .map_err(|_| "G7 ANN progress sink synchronization failed".to_owned())
                .and_then(|mut sink| sink.update_build(update).map_err(|error| error.to_string()));
            if let Err(error) = outcome
                && let Ok(mut failure) = callback_failure.lock()
                && failure.is_none()
            {
                *failure = Some(error);
            }
        },
    )?;
    if let Some(error) = progress_failure
        .lock()
        .map_err(|_| "G7 ANN progress failure synchronization failed")?
        .take()
    {
        return Err(error.into());
    }
    let build = plan.build_evidence();
    validate_initial_ann_bulk_build(build, authority)?;
    let evidence = initial_ann_bulk_evidence(source_commit, build, authority)?;
    if let Some(sink) = &progress {
        sink.lock()
            .map_err(|_| "G7 ANN progress sink synchronization failed")?
            .begin_publication(&evidence)?;
    }
    let published =
        database.publish_initial_ann_bulk(plan, hyphae_native_types::DurabilityClass::Memory)?;
    if published.build != build {
        return Err("published G7 ANN build evidence changed after planning".into());
    }
    let observed = database.observe_ann_index(vector_index)?;
    if observed.base_identity != build.build_identity {
        return Err("published G7 ANN generation differs from its planned aggregate".into());
    }
    if let Some(sink) = &progress {
        sink.lock()
            .map_err(|_| "G7 ANN progress sink synchronization failed")?
            .complete("ann-bulk-build", observed.base_identity, &evidence)?;
    }
    // Lexical documents and scalar filters use the physical all-engine delta
    // path. Each batch resolves only the touched identities and preserves the
    // already-published ANN generation, avoiding the former O(n^2) sequence of
    // complete-state transaction materializations.
    for batch_start in (0..document_count).step_by(512) {
        let mut batch =
            database.begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Memory)?;
        for id in batch_start..(batch_start + 512).min(document_count) {
            let document_id = (id as u128 + 1).to_be_bytes();
            let text = if id == document_count / 2 {
                "rare g7 native benchmark term"
            } else {
                "common g7 native benchmark"
            };
            database.stage_delta_index_document(
                &mut batch,
                lexical_index,
                document_id.to_vec(),
                text.to_owned(),
            )?;
            database.stage_delta_set(
                &mut batch,
                filter_key(&document_id),
                if id % 2 == 0 {
                    b"keep".to_vec()
                } else {
                    b"drop".to_vec()
                },
                None,
            )?;
        }
        database.commit_optimistic(batch)?;
    }
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    drop(database);
    Ok(evidence)
}

fn validate_initial_ann_bulk_build(
    build: InitialAnnBulkBuildEvidence,
    authority: &ExecutionAuthority,
) -> Result<(), Box<dyn Error>> {
    if build.builder != InitialAnnBulkBuilder::PartitionedHnswV1 {
        return Err("G7 initial ANN bulk selected an unexpected builder".into());
    }
    if build.planned_vectors != search_documents()
        || build.planned_partitions != authority.topology.worker_count()
        || build.planned_compute_threads == 0
        || build.planned_compute_threads as usize > authority.topology.worker_count()
        || build.planned_memory_bytes == 0
        || build.worker_batches == 0
    {
        return Err("G7 initial ANN bulk returned incomplete resource evidence".into());
    }
    if authority.topology.worker_count() > 1 && build.planned_compute_threads <= 1 {
        return Err("G7 initial ANN bulk ignored a multi-worker execution topology".into());
    }
    if build.planned_compute_threads > 1 && build.worker_batches <= 1 {
        return Err("G7 initial ANN bulk did not execute multiple worker batches".into());
    }
    let execution = build
        .execution
        .ok_or("G7 initial ANN bulk did not use the resource governor")?;
    if execution.class != WorkloadClass::Bulk
        || execution.request.compute_threads != build.planned_compute_threads
        || execution.request.memory_bytes != build.planned_memory_bytes
    {
        return Err("G7 initial ANN bulk governor evidence differs from its plan".into());
    }
    Ok(())
}

fn progress_eta(
    elapsed: Duration,
    completed: usize,
    total: usize,
    finished: bool,
) -> std::io::Result<serde_json::Value> {
    if finished || completed >= total {
        return Ok(json!({
            "status": "completed",
            "estimated_remaining_nanos": 0,
        }));
    }
    if completed == 0 {
        return Ok(json!({
            "status": "pending",
            "estimated_remaining_nanos": null,
        }));
    }
    let remaining_units = total
        .checked_sub(completed)
        .ok_or_else(|| std::io::Error::other("G7 ANN progress exceeds total"))?
        as u128;
    let remaining_nanos = elapsed
        .as_nanos()
        .checked_mul(remaining_units)
        .ok_or_else(|| std::io::Error::other("G7 ANN progress ETA multiplication overflow"))?
        .checked_div(completed as u128)
        .ok_or_else(|| std::io::Error::other("G7 ANN progress ETA divisor must be nonzero"))?;
    let remaining_nanos = u64::try_from(remaining_nanos)
        .map_err(|_| std::io::Error::other("G7 ANN progress ETA exceeds u64"))?;
    Ok(json!({
        "status": "estimated",
        "estimated_remaining_nanos": remaining_nanos,
    }))
}

fn initial_ann_bulk_evidence(
    source_commit: &str,
    build: InitialAnnBulkBuildEvidence,
    authority: &ExecutionAuthority,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let execution = build
        .execution
        .ok_or("G7 initial ANN bulk did not produce governor execution evidence")?;
    Ok(json!({
        "schema": "hyphae-native-g7-initial-ann-bulk-v1",
        "source_commit": source_commit,
        "dataset_digest": dataset_digest(source_commit).to_hex().to_string(),
        "builder": "partitioned-hnsw-v1",
        "input_identity": blake3::Hash::from_bytes(build.input_identity).to_hex().to_string(),
        "aggregate_identity": blake3::Hash::from_bytes(build.build_identity).to_hex().to_string(),
        "planned_vectors": build.planned_vectors,
        "planned_partitions": build.planned_partitions,
        "planned_workers": build.planned_compute_threads,
        "planned_memory_bytes": build.planned_memory_bytes,
        "worker_batches": build.worker_batches,
        "total_time_nanos": u64::try_from(build.total_time.as_nanos()).unwrap_or(u64::MAX),
        "hardware_profile_fingerprint": authority.profile.fingerprint,
        "governor_policy_schema": authority.policy.schema,
        "governor_mode": serde_json::to_value(authority.policy.mode)?,
        "calibration_cache_key": authority.policy.calibration_cache_key,
        "topology_digest": authority.topology_digest,
        "topology_workers": authority.topology.worker_count(),
        "hard_affinity": authority.topology.hard_affinity,
        "governor_execution": {
            "class": "bulk",
            "compute_threads": execution.request.compute_threads,
            "io_slots": execution.request.io_slots,
            "memory_bytes": execution.request.memory_bytes,
            "queue_ticket": execution.queue_ticket,
            "initial_queue_depth": execution.initial_queue_depth,
            "queue_time_nanos": u64::try_from(execution.queue_time.as_nanos()).unwrap_or(u64::MAX),
            "execution_time_nanos": u64::try_from(execution.execution_time.as_nanos()).unwrap_or(u64::MAX),
        },
    }))
}

fn initial_ann_bulk_evidence_path(seed_path: &Path) -> PathBuf {
    let name = seed_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("search-seed");
    seed_path.with_file_name(format!("{name}.initial-ann-bulk.json"))
}

fn load_initial_ann_bulk_evidence(
    seed_path: &Path,
    source_commit: &str,
    authority: &ExecutionAuthority,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = initial_ann_bulk_evidence_path(seed_path);
    let evidence: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    if required_json_string(&evidence, "schema")? != "hyphae-native-g7-initial-ann-bulk-v1"
        || required_json_string(&evidence, "source_commit")? != source_commit
        || required_json_string(&evidence, "dataset_digest")?
            != dataset_digest(source_commit).to_hex().as_str()
        || required_json_string(&evidence, "builder")? != "partitioned-hnsw-v1"
        || required_json_string(&evidence, "hardware_profile_fingerprint")?
            != authority.profile.fingerprint
        || required_json_string(&evidence, "topology_digest")? != authority.topology_digest
        || required_json_u64(&evidence, "topology_workers")?
            != authority.topology.worker_count() as u64
        || required_json_u64(&evidence, "planned_vectors")? != search_documents() as u64
    {
        return Err("G7 initial ANN bulk evidence is not bound to this run".into());
    }
    let workers = required_json_u64(&evidence, "planned_workers")?;
    let worker_batches = required_json_u64(&evidence, "worker_batches")?;
    if workers == 0
        || required_json_u64(&evidence, "planned_memory_bytes")? == 0
        || worker_batches == 0
        || (workers > 1 && worker_batches <= 1)
    {
        return Err("G7 initial ANN bulk evidence does not prove governed parallel work".into());
    }
    Ok(evidence)
}

fn read_required_json(environment_name: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = PathBuf::from(
        std::env::var_os(environment_name)
            .ok_or_else(|| format!("missing required G7 contract: {environment_name}"))?,
    );
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!("G7 contract is not a regular file: {}", path.display()).into());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn required_json_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, Box<dyn Error>> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("G7 evidence is missing string field {field}").into())
}

fn required_json_u64(value: &serde_json::Value, field: &str) -> Result<u64, Box<dyn Error>> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("G7 evidence is missing integer field {field}").into())
}

fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("G7 evidence path has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("initial-ann-bulk"),
        std::process::id(),
        unique_nonce(),
    ));
    fs::write(&staging, serde_json::to_vec_pretty(value)?)?;
    fs::rename(staging, path)?;
    Ok(())
}

fn search_seed_path(root: &Path, source_commit: &str) -> Result<PathBuf, Box<dyn Error>> {
    let parent = std::env::var_os("HYPHAE_G7_SEARCH_SEED_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("search-seeds"));
    let digest = dataset_digest(source_commit);
    Ok(parent.join(format!("search-{}", digest.to_hex())))
}

fn dataset_digest(source_commit: &str) -> blake3::Hash {
    let documents = search_documents();
    let dimension = vector_dimension();
    blake3::hash(
        format!(
            "{CORPUS_GENERATOR}:source={source_commit}:documents={documents}:vectors={documents}:dimension={dimension}"
        )
        .as_bytes(),
    )
}

#[derive(Clone, Debug)]
struct CounterValue {
    status: &'static str,
    value: Option<u64>,
    unit: &'static str,
    provider: &'static str,
    reason: Option<&'static str>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let allocation_start = GLOBAL_ALLOCATOR.stats();
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("missing exact source commit")?;
    let platform = std::env::args()
        .nth(2)
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let state = std::env::args().nth(3).unwrap_or_else(|| "warm".to_owned());
    let concurrency = std::env::args()
        .nth(4)
        .unwrap_or_else(|| "1".to_owned())
        .parse::<usize>()?;
    let observations = std::env::var("HYPHAE_G7_OBSERVATIONS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_OBSERVATIONS);
    let warmup = std::env::var("HYPHAE_G7_WARMUP")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_WARMUP);
    if !matches!(state.as_str(), "warm" | "cold") || !matches!(concurrency, 1 | 8 | 32) {
        return Err("state must be warm/cold and concurrency must be 1, 8, or 32".into());
    }
    let root = temporary_root(&state, concurrency)?;
    let search = SearchFixture::open_or_create(&root, &source_commit)?;
    let background_enabled =
        std::env::var("HYPHAE_G7_BACKGROUND").is_ok_and(|value| value == "1" || value == "true");
    let background_stop = Arc::new(AtomicBool::new(false));
    let background_thread = background_enabled.then(|| {
        let stop = Arc::clone(&background_stop);
        let path = root.join("background-maintenance");
        thread::spawn(move || -> Result<u64, String> {
            let mut database = NativeDatabase::create(path).map_err(|error| error.to_string())?;
            let mut operations = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                let mut batch = database
                    .begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Memory)
                    .map_err(|error| error.to_string())?;
                database
                    .stage_delta_set(
                        &mut batch,
                        operations.to_be_bytes().to_vec(),
                        vec![0x7b; 4_096],
                        None,
                    )
                    .map_err(|error| error.to_string())?;
                database
                    .commit_optimistic(batch)
                    .map_err(|error| error.to_string())?;
                operations = operations.saturating_add(1);
                if operations.is_multiple_of(64) {
                    black_box(database.checkpoint().map_err(|error| error.to_string())?);
                }
                thread::sleep(BACKGROUND_INTERVAL);
            }
            Ok(operations)
        })
    });
    wait_for_profiler()?;
    let mut receipt = json!({
        "schema": VERSION,
        "gate": "G7",
        "status": "passed",
        "evidence_class": "closure-candidate",
        "source_commit": source_commit,
        "platform": platform,
        "state": state,
        "concurrency": concurrency,
        "dataset": dataset_metadata(&source_commit, observations, warmup),
        "workload": workload_metadata(),
        "durability": {
            "read_seed": "memory-committed",
            "search_seed": "memory-committed",
            "commit_cell": "group-physical-sync",
        },
        "proofs_included": false,
        "correctness": {
            "cell_assertions": "passed",
            "ann_recall_floor": 0.95,
            "cross_engine_visibility": "native-same-snapshot-search",
        },
        "initial_ann_bulk": search.initial_ann_bulk.clone(),
        "cells": {},
        "counters": counters_process(&root)?,
        "saturation": {"status": "measured", "levels": [1, 8, 32], "method": "requested-concurrency"},
        "background_interference": if background_enabled {
            json!({"status": "measured", "method": "budgeted-native-wal-checkpoint", "workload": "4KiB memory commits at 100 operations/s plus a checkpoint every 64 commits"})
        } else {
            json!({"status": "control", "method": "no-background-worker", "workload": "none"})
        },
        "claims": [],
        "closure_declared": false,
    });
    let mut cells = BTreeMap::new();
    if state == "cold" {
        // Cold state is a fresh process and fresh data directory for this
        // receipt. Warm state retains the process-local seeded handles.
        fs::create_dir_all(root.join("cold-marker"))?;
    }
    cells.insert(
        "embedded-structure-point-get",
        run_embedded_structure(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("embedded structure: {error}"))?,
    );
    cells.insert(
        "embedded-prepared-sql-primary-key",
        run_embedded_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("embedded sql: {error}"))?,
    );
    cells.insert(
        "local-structure-point-get",
        run_local_structure(&root, state == "warm", concurrency, observations, warmup)
            .await
            .map_err(|error| format!("local structure: {error}"))?,
    );
    cells.insert(
        "local-prepared-sql-primary-key",
        run_local_sql(&root, state == "warm", concurrency, observations, warmup)
            .await
            .map_err(|error| format!("local sql: {error}"))?,
    );
    cells.insert(
        "indexed-sql-bounded-read",
        run_indexed_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("indexed sql: {error}"))?,
    );
    cells.insert(
        "two-index-join-bounded-read",
        run_join_sql(&root, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("two-index join: {error}"))?,
    );
    cells.insert(
        "bm25-top10",
        run_bm25(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("bm25: {error}"))?,
    );
    cells.insert(
        "filtered-bm25-top10",
        run_filtered_bm25(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("filtered bm25: {error}"))?,
    );
    cells.insert(
        "ann-top10-recall-095",
        run_ann(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("ann: {error}"))?,
    );
    cells.insert(
        "hybrid-top10",
        run_hybrid(&search, state == "warm", concurrency, observations, warmup)
            .map_err(|error| format!("hybrid: {error}"))?,
    );
    cells.insert(
        "strict-group-commit",
        run_commit(&root, concurrency, observations)?,
    );
    receipt["cells"] = serde_json::to_value(cells)?;
    receipt["physical_observation"] = physical_observation(&root)?;
    receipt["counters"] = counters_process(&root)?;
    let allocation_change = GLOBAL_ALLOCATOR.stats() - allocation_start;
    receipt["counters"]["allocations"] = counter_json(CounterValue {
        status: "measured",
        value: Some(
            u64::try_from(
                allocation_change
                    .allocations
                    .saturating_add(allocation_change.reallocations),
            )
            .unwrap_or(u64::MAX),
        ),
        unit: "count",
        provider: "stats-alloc-system-wrapper",
        reason: None,
    });
    background_stop.store(true, Ordering::Relaxed);
    if let Some(thread) = background_thread {
        let operations = thread
            .join()
            .map_err(|_| "background worker panicked")?
            .map_err(|error| format!("background maintenance failed: {error}"))?;
        receipt["background_interference"]["operations"] = json!(operations);
    }
    fs::remove_dir_all(&root)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn wait_for_profiler() -> Result<(), Box<dyn Error>> {
    let Some(ready_path) = std::env::var_os("HYPHAE_G7_PROFILE_READY_FILE").map(PathBuf::from)
    else {
        return Ok(());
    };
    let start_path = std::env::var_os("HYPHAE_G7_PROFILE_START_FILE")
        .map(PathBuf::from)
        .ok_or("profile ready file requires a start file")?;
    fs::write(&ready_path, std::process::id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !start_path.is_file() {
        if Instant::now() >= deadline {
            return Err("timed out waiting for the G7 profiler".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn temporary_root(state: &str, concurrency: usize) -> Result<PathBuf, Box<dyn Error>> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let parent = std::env::var_os("HYPHAE_G7_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!("hyphae-g7-{state}-{concurrency}-{timestamp}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn short_endpoint(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "h-g7-{label}-{}-{}",
        std::process::id(),
        unique_nonce()
    ))
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn dataset_metadata(source_commit: &str, observations: usize, warmup: usize) -> serde_json::Value {
    let documents = search_documents();
    let dimension = vector_dimension();
    let digest = dataset_digest(source_commit);
    json!({
        "structure_keys": STRUCTURE_KEYS,
        "search_documents": documents,
        "vector_count": documents,
        "vector_dimension": dimension,
        "observations": observations,
        "warmup": warmup,
        "generator": CORPUS_GENERATOR,
        "digest": digest.to_hex().to_string(),
    })
}

fn workload_metadata() -> serde_json::Value {
    let documents = search_documents();
    json!({
        "structure_keys": STRUCTURE_KEYS,
        "sql_rows": SQL_KEYS,
        "point_value_bytes": 64,
        "search_documents": documents,
        "vector_count": documents,
        "vector_dimension": vector_dimension(),
        "lexical_rare_documents": 1,
        "filtered_documents": documents.div_ceil(2),
        "result_limit": K,
        "lexical_index_state": "committed-hot",
        "vector_index_state": "committed-hot",
    })
}

fn search_documents() -> usize {
    static VALUE: OnceLock<usize> = OnceLock::new();
    *VALUE.get_or_init(|| smoke_override("HYPHAE_G7_SEARCH_DOCUMENTS", CLOSURE_SEARCH_DOCUMENTS))
}

fn vector_dimension() -> u16 {
    static VALUE: OnceLock<u16> = OnceLock::new();
    *VALUE.get_or_init(|| smoke_override("HYPHAE_G7_VECTOR_DIMENSION", CLOSURE_VECTOR_DIMENSION))
}

fn smoke_override<T>(name: &str, closure_value: T) -> T
where
    T: std::str::FromStr + Copy,
{
    if std::env::var("HYPHAE_G7_SMOKE").as_deref() != Ok("1") {
        return closure_value;
    }
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(closure_value)
}

fn counters_unavailable() -> serde_json::Value {
    let reason = "provider not attached to this runner invocation";
    json!({
        "allocations": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "rss": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
        "cpu_cycles": counter_json(CounterValue { status: "unavailable", value: None, unit: "cycles", provider: "none", reason: Some(reason) }),
        "cache_misses": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "page_faults": counter_json(CounterValue { status: "unavailable", value: None, unit: "count", provider: "none", reason: Some(reason) }),
        "bytes_read": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
        "bytes_written": counter_json(CounterValue { status: "unavailable", value: None, unit: "bytes", provider: "none", reason: Some(reason) }),
    })
}

fn counters_process(root: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    #[allow(unused_mut)]
    let mut counters = counters_unavailable();
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string(format!("/proc/{}/status", std::process::id()))?;
        if let Some(value) = status.lines().find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|value| value.saturating_mul(1024))
        }) {
            counters["rss"] = counter_json(CounterValue {
                status: "measured",
                value: Some(value),
                unit: "bytes",
                provider: "linux-proc-vmhwm",
                reason: None,
            });
        }
        let io = fs::read_to_string(format!("/proc/{}/io", std::process::id()))?;
        for (source, target, provider) in [
            ("read_bytes", "bytes_read", "linux-proc-read-bytes"),
            ("write_bytes", "bytes_written", "linux-proc-write-bytes"),
        ] {
            let field = if source == "read_bytes" {
                "read_bytes:"
            } else {
                "write_bytes:"
            };
            if let Some(value) = io.lines().find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|value| value.trim().parse::<u64>().ok())
            }) {
                counters[target] = counter_json(CounterValue {
                    status: "measured",
                    value: Some(value),
                    unit: "bytes",
                    provider,
                    reason: None,
                });
            }
        }
        let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id()))?;
        let fields = stat.split_whitespace().collect::<Vec<_>>();
        if fields.len() > 14 {
            let faults = fields[9]
                .parse::<u64>()?
                .saturating_add(fields[11].parse::<u64>()?);
            counters["page_faults"] = counter_json(CounterValue {
                status: "measured",
                value: Some(faults),
                unit: "count",
                provider: "linux-proc-stat",
                reason: None,
            });
        }
    }
    let _ = root;
    Ok(counters)
}

fn counter_json(counter: CounterValue) -> serde_json::Value {
    let mut value = json!({
        "status": counter.status,
        "value": counter.value,
        "unit": counter.unit,
        "provider": counter.provider,
    });
    if let Some(reason) = counter.reason {
        value["reason"] = json!(reason);
    }
    value
}

fn run_embedded_structure(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("structure");
    let mut database =
        NativeDatabase::create(&path).map_err(|error| format!("structure seed: {error}"))?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    for index in 0..STRUCTURE_KEYS {
        seed.set(index.to_be_bytes().to_vec(), vec![0xa5; 64], None)?;
    }
    seed.commit()?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    let target = (STRUCTURE_KEYS / 2).to_be_bytes();
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(database.get_latest_structure(&target, 0)?);
        }
    }
    let target_value = [0xa5; 64];
    let stats = measure_concurrent(concurrency, observations, &|| {
        let value = database.get_latest_structure(&target, 0)?;
        if value.as_deref() != Some(target_value.as_slice()) {
            return Err("structure result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    stats_with_materialization(stats, materialization)
}

fn run_embedded_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("sql");
    let mut database = NativeDatabase::create(&path)
        .map_err(|error| format!("sql seed {}: {error}", path.display()))?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_items (id, payload) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Binary(vec![0x5a; 64]),
            ],
        )?;
    }
    seed.commit()?;
    let prepared = database.prepare_sql_latest("SELECT id, payload FROM g7_items WHERE id = ?")?;
    let parameters = [hyphae_native_runtime::SqlValue::Signed(
        (SQL_KEYS / 2) as i64,
    )];
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("prepared SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    stats_with_materialization(stats, materialization)
}

async fn run_local_structure(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-structure");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local structure seed: {error}"))?;
    let mut session = product_session();
    let mut context = product_context(&session, 1);
    context.durability = ProductDurabilityPolicy::MEMORY;
    product.dispatch(
        &mut session,
        &context,
        ProductOperation::StructureSet {
            key: b"g7-local-structure".to_vec(),
            value: vec![0x3c; 64],
            expires_at_micros: None,
        },
    )?;
    drop(session);
    drop(product);
    let mut database = NativeDatabase::open(&path)
        .map_err(|error| format!("local structure migration open: {error}"))?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    drop(database);
    let product = NativeProduct::open(&path)
        .map_err(|error| format!("local structure reopen after migration: {error}"))?;
    let endpoint = short_endpoint("structure");
    let daemon = NativeDaemon::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let client = HyphaeClient::local(endpoint.to_string_lossy().into_owned())?;
        let options = RequestOptions::default();
        let materialization = NativeDatabase::process_materialization_observation();
        if warm {
            for _ in 0..warmup {
                require_structure_response(
                    client
                        .structure_get(b"g7-local-structure".to_vec(), options.clone())
                        .await?,
                )?;
            }
        }
        let stats = measure_async(concurrency, observations, || {
            let client = client.clone();
            let options = options.clone();
            async move {
                require_structure_response(
                    client
                        .structure_get(b"g7-local-structure".to_vec(), options)
                        .await?,
                )?;
                Ok::<(), Box<dyn Error>>(())
            }
        })
        .await?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

async fn run_local_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-sql");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local sql seed: {error}"))?;
    seed_product_sql(&mut product)?;
    let endpoint = short_endpoint("sql");
    let daemon = NativeDaemon::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let client = HyphaeClient::local(endpoint.to_string_lossy().into_owned())?;
        let options = RequestOptions::default();
        let prepared = client
            .prepare_sql(
                "SELECT id, payload FROM g7_items WHERE id = ?",
                options.clone(),
            )
            .await?;
        let hyphae_native_product::ProductResponse::PreparedSql { handle, .. } = prepared else {
            return Err("local SQL prepare returned an unexpected response".into());
        };
        let materialization = NativeDatabase::process_materialization_observation();
        if warm {
            for _ in 0..warmup {
                require_sql_response(
                    client
                        .execute_prepared(
                            handle,
                            vec![hyphae_native_product::ProductValue::Signed(
                                (SQL_KEYS / 2) as i64,
                            )],
                            options.clone(),
                        )
                        .await?,
                )?;
            }
        }
        let stats = measure_async(concurrency, observations, || {
            let client = client.clone();
            let options = options.clone();
            async move {
                require_sql_response(
                    client
                        .execute_prepared(
                            handle,
                            vec![hyphae_native_product::ProductValue::Signed(
                                (SQL_KEYS / 2) as i64,
                            )],
                            options,
                        )
                        .await?,
                )?;
                Ok::<(), Box<dyn Error>>(())
            }
        })
        .await?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
    result
}

fn run_indexed_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("indexed");
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_indexed (id BIGINT PRIMARY KEY, email TEXT NOT NULL, payload BINARY NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_indexed (id, email, payload) VALUES (?, ?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("g7-email-{id}")),
                hyphae_native_runtime::SqlValue::Binary(vec![0x5a; 64]),
            ],
        )?;
    }
    seed.execute_sql(
        "CREATE UNIQUE INDEX g7_indexed_email ON g7_indexed (email)",
        &[],
    )?;
    seed.commit()?;
    let prepared =
        database.prepare_sql_latest("SELECT id, payload FROM g7_indexed WHERE email = ?")?;
    let parameters = [hyphae_native_runtime::SqlValue::Text(format!(
        "g7-email-{}",
        SQL_KEYS / 2
    ))];
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("indexed SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut value = stats_with_materialization(stats, materialization)?;
    value["route"] = json!("native-indexed-sql");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_join_sql(
    root: &Path,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = root.join("join");
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin_sql(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.execute_sql(
        "CREATE TABLE g7_profiles (id BIGINT PRIMARY KEY, city TEXT NOT NULL)",
        &[],
    )?;
    seed.execute_sql(
        "CREATE TABLE g7_users (id BIGINT PRIMARY KEY, profile_id BIGINT NOT NULL, email TEXT NOT NULL)",
        &[],
    )?;
    for id in 0..SQL_KEYS {
        seed.execute_sql(
            "INSERT INTO g7_profiles (id, city) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("city-{id}")),
            ],
        )?;
        seed.execute_sql(
            "INSERT INTO g7_users (id, profile_id, email) VALUES (?, ?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Signed(id as i64),
                hyphae_native_runtime::SqlValue::Text(format!("join-email-{id}")),
            ],
        )?;
    }
    seed.execute_sql(
        "CREATE UNIQUE INDEX g7_users_email ON g7_users (email)",
        &[],
    )?;
    seed.commit()?;
    let prepared = database.prepare_sql_latest(
        "SELECT g7_users.id, g7_profiles.city FROM g7_users INNER JOIN g7_profiles ON g7_users.profile_id = g7_profiles.id WHERE email = ?",
    )?;
    let parameters = [hyphae_native_runtime::SqlValue::Text(format!(
        "join-email-{}",
        SQL_KEYS / 2
    ))];
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("two-index join result mismatch".into());
        }
        Ok(())
    })?;
    stats_with_materialization(stats, materialization)
}

fn run_filtered_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(filtered_bm25_query(
                &fixture.database,
                fixture.lexical_index,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        if filtered_bm25_query(&fixture.database, fixture.lexical_index)? != 1 {
            return Err("filtered BM25 result mismatch".into());
        }
        Ok(())
    })?;
    let mut value = stats_with_materialization(stats, materialization)?;
    value["route"] = json!("native-same-snapshot-filtered-bm25");
    value["filter_selectivity"] = json!(0.5);
    value["correctness_scope"] = json!("lexical-and-structure-same-snapshot");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_hybrid(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(hybrid_query(
                &fixture.database,
                &fixture.snapshot,
                fixture.lexical_index,
                fixture.vector_index,
                &fixture.query,
                fixture.options,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        if hybrid_query(
            &fixture.database,
            &fixture.snapshot,
            fixture.lexical_index,
            fixture.vector_index,
            &fixture.query,
            fixture.options,
        )? < K
        {
            return Err("hybrid result mismatch".into());
        }
        Ok(())
    })?;
    let mut value = stats_with_materialization(stats, materialization)?;
    value["route"] = json!("native-same-snapshot-hybrid");
    value["lexical_branch"] = json!(true);
    value["vector_branch"] = json!("ann-exact-rerank");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn filter_key(document_id: &[u8; 16]) -> Vec<u8> {
    let mut key = b"g7-filter:".to_vec();
    key.extend_from_slice(document_id);
    key
}

fn filtered_bm25_query(
    database: &NativeDatabase,
    index: ObjectId,
) -> Result<usize, Box<dyn Error>> {
    let hits = database.match_latest_text(index, "rare", K)?;
    let mut admitted = 0;
    for hit in hits {
        let document_id: [u8; 16] = hit
            .document_id
            .as_slice()
            .try_into()
            .map_err(|_| "filtered BM25 document identity is not 16 bytes")?;
        if database
            .get_latest_structure(&filter_key(&document_id), 0)?
            .as_deref()
            == Some(b"keep".as_slice())
        {
            admitted += 1;
        }
    }
    Ok(admitted)
}

fn hybrid_query(
    database: &NativeDatabase,
    snapshot: &NativeSnapshot,
    lexical_index: ObjectId,
    vector_index: ObjectId,
    query: &Vector,
    options: AnnSearchOptions,
) -> Result<usize, Box<dyn Error>> {
    let lexical = database.match_latest_text(lexical_index, "rare", K)?;
    let vector = snapshot.search_ann(vector_index, query, options)?;
    let mut fused = BTreeMap::<ObjectId, f64>::new();
    for (rank, hit) in lexical.into_iter().enumerate() {
        let document_id: [u8; 16] = hit
            .document_id
            .as_slice()
            .try_into()
            .map_err(|_| "hybrid lexical identity is not 16 bytes")?;
        let object_id = ObjectId::new(u128::from_be_bytes(document_id))?;
        *fused.entry(object_id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    for (rank, hit) in vector.hits.into_iter().enumerate() {
        *fused.entry(hit.object_id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
    }
    Ok(fused.len())
}

fn seed_product_sql(product: &mut NativeProduct) -> Result<(), Box<dyn Error>> {
    let mut session = product_session();
    let mut context = product_context(&session, 1);
    context.durability = ProductDurabilityPolicy::MEMORY;
    product.dispatch(
        &mut session,
        &context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE g7_items (id BIGINT PRIMARY KEY, payload BINARY NOT NULL)"
                .into(),
            parameters: Vec::new(),
        },
    )?;
    for id in 0..SQL_KEYS {
        let mut context = product_context(&session, id as u128 + 2);
        context.durability = ProductDurabilityPolicy::MEMORY;
        product.dispatch(
            &mut session,
            &context,
            ProductOperation::ExecuteSql {
                statement: "INSERT INTO g7_items (id, payload) VALUES (?, ?)".into(),
                parameters: vec![
                    hyphae_native_product::ProductValue::Signed(id as i64),
                    hyphae_native_product::ProductValue::Binary(vec![0x5a; 64]),
                ],
            },
        )?;
    }
    Ok(())
}

fn product_session() -> ProductSession {
    ProductSession::new(
        ProductSessionId::new(1).expect("nonzero session"),
        ProductPrincipal::new("g7-runner").expect("valid principal"),
        ProductAuthorization::ALL,
    )
}

fn product_context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
}

fn require_structure_response(
    response: hyphae_native_product::ProductResponse,
) -> Result<(), Box<dyn Error>> {
    if !matches!(response, hyphae_native_product::ProductResponse::StructureValue(Some(value)) if value == vec![0x3c; 64])
    {
        return Err("local structure response mismatch".into());
    }
    Ok(())
}

fn require_sql_response(
    response: hyphae_native_product::ProductResponse,
) -> Result<(), Box<dyn Error>> {
    if !matches!(response, hyphae_native_product::ProductResponse::Sql { result: hyphae_native_product::ProductSqlResult::Rows { rows, .. }, .. } if rows.len() == 1)
    {
        return Err("local SQL response mismatch".into());
    }
    Ok(())
}

async fn measure_async<F, Fut>(
    concurrency: usize,
    observations: usize,
    mut operation: F,
) -> Result<Stats, Box<dyn Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), Box<dyn Error>>>,
{
    let started = Instant::now();
    let mut samples = Vec::with_capacity(observations);
    let rounds = observations.div_ceil(concurrency.max(1));
    for _ in 0..rounds {
        let sample = Instant::now();
        let results = join_all((0..concurrency).map(|_| operation())).await;
        let elapsed = sample.elapsed().as_nanos() as u64;
        for result in results {
            result?;
            samples.push(elapsed);
            if samples.len() == observations {
                break;
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    samples.sort_unstable();
    Ok(Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().ok_or("empty async benchmark")?,
        throughput: samples.len() as f64 / elapsed,
    })
}

fn run_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(
                fixture
                    .database
                    .match_latest_text(fixture.lexical_index, "rare", K)?,
            );
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        let hits = fixture
            .database
            .match_latest_text(fixture.lexical_index, "rare", K)?;
        if hits.is_empty() {
            return Err("BM25 result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    stats_with_materialization(stats, materialization)
}

fn run_ann(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        for _ in 0..warmup {
            black_box(fixture.snapshot.search_ann(
                fixture.vector_index,
                &fixture.query,
                fixture.options,
            )?);
        }
    }
    let stats = measure_concurrent(concurrency, observations, &|| {
        black_box(fixture.snapshot.search_ann(
            fixture.vector_index,
            &fixture.query,
            fixture.options,
        )?);
        Ok::<(), Box<dyn Error>>(())
    })?;
    let mut output = stats_with_materialization(stats, materialization)?;
    output["recall_at_10"] = json!(fixture.recall_at_10);
    Ok(output)
}

fn vector_fixture(id: usize, document_count: usize) -> Result<(ObjectId, Vector), Box<dyn Error>> {
    let mut values = vec![0.0; vector_dimension() as usize];
    values[0] = 1.0;
    values[1] = id as f32 / document_count as f32;
    Ok((ObjectId::new(id as u128 + 1)?, Vector::new(values)?))
}

const fn ann_lifecycle() -> IncrementalVectorLifecycle {
    IncrementalVectorLifecycle {
        delta_max_entries: ANN_DELTA_MAX_ENTRIES,
        consolidate_after_deltas: ANN_CONSOLIDATE_AFTER_DELTAS,
        retain_generations: 1,
    }
}

fn run_commit(
    root: &Path,
    concurrency: usize,
    observations: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let group_path = root.join("group");
    let mut database =
        NativeDatabase::create(&group_path).map_err(|error| format!("group seed: {error}"))?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.set(b"g7-group-seed".to_vec(), b"v".to_vec(), None)?;
    seed.commit()?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    let materialization = NativeDatabase::process_materialization_observation();
    let scheduler = NativeCommitScheduler::start(
        database,
        hyphae_native_runtime::GroupCommitConfig::new(concurrency, Duration::from_millis(2), 64)?,
    )?;
    let clients = (0..concurrency)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let barrier = Barrier::new(concurrency);
    let started = Instant::now();
    let samples = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for (producer, client) in clients.into_iter().enumerate() {
            let count =
                observations / concurrency + usize::from(producer < observations % concurrency);
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<Vec<u64>, String> {
                let mut samples = Vec::with_capacity(count);
                barrier.wait();
                for sequence in 0..count {
                    let sample = Instant::now();
                    let mut batch = client
                        .begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Group)
                        .map_err(|error| error.to_string())?;
                    client
                        .stage_delta_set(
                            &mut batch,
                            format!("g7-group-{producer}-{sequence}").into_bytes(),
                            b"v".to_vec(),
                            None,
                        )
                        .map_err(|error| error.to_string())?;
                    client.submit(batch).map_err(|error| error.to_string())?;
                    samples.push(sample.elapsed().as_nanos() as u64);
                }
                Ok(samples)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "group commit worker panicked".to_owned())?
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let stats = stats_from_samples(samples, started.elapsed().as_secs_f64());
    scheduler.shutdown()?;
    let mut output = stats_with_materialization(stats, materialization)?;
    output["group_concurrency"] = json!(concurrency);
    output["durability"] = json!("group-physical-sync");
    Ok(output)
}

fn measure_concurrent(
    concurrency: usize,
    observations: usize,
    operation: &(impl Fn() -> Result<(), Box<dyn Error>> + Sync),
) -> Result<Stats, Box<dyn Error>> {
    if concurrency == 0 || observations < concurrency {
        return Err("concurrent benchmark requires at least one observation per worker".into());
    }
    let barrier = Barrier::new(concurrency);
    let started = Instant::now();
    let samples = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for worker in 0..concurrency {
            let count =
                observations / concurrency + usize::from(worker < observations % concurrency);
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<Vec<u64>, String> {
                let mut samples = Vec::with_capacity(count);
                barrier.wait();
                for _ in 0..count {
                    let sample = Instant::now();
                    operation().map_err(|error| error.to_string())?;
                    samples.push(sample.elapsed().as_nanos() as u64);
                }
                Ok(samples)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "benchmark worker panicked".to_owned())?
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    Ok(stats_from_samples(samples, started.elapsed().as_secs_f64()))
}

fn stats_from_samples(mut samples: Vec<u64>, elapsed_seconds: f64) -> Stats {
    samples.sort_unstable();
    Stats {
        p50: percentile(&samples, 500),
        p95: percentile(&samples, 950),
        p99: percentile(&samples, 990),
        p999: percentile(&samples, 999),
        maximum: *samples.last().unwrap_or(&0),
        throughput: samples.len() as f64 / elapsed_seconds,
    }
}

fn percentile(values: &[u64], per_mille: usize) -> u64 {
    values[values.len().saturating_sub(1).saturating_mul(per_mille) / 1_000]
}

fn stats_json(stats: Stats) -> serde_json::Value {
    json!({
        "status": "measured",
        "unit": "nanoseconds",
        "p50": stats.p50,
        "p95": stats.p95,
        "p99": stats.p99,
        "p999": stats.p999,
        "maximum": stats.maximum,
        "throughput_per_second": stats.throughput,
    })
}

fn stats_with_materialization(
    stats: Stats,
    before: hyphae_native_runtime::NativeMaterializationObservation,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let after = NativeDatabase::process_materialization_observation();
    let full_state_loads = after
        .full_state_loads
        .checked_sub(before.full_state_loads)
        .ok_or("full-state materialization counter regressed")?;
    let full_catalog_loads = after
        .full_catalog_loads
        .checked_sub(before.full_catalog_loads)
        .ok_or("catalog materialization counter regressed")?;
    if full_state_loads != 0 || full_catalog_loads != 0 {
        return Err(format!(
            "measured hot path materialized complete state: full_state={full_state_loads}, full_catalog={full_catalog_loads}"
        )
        .into());
    }
    let mut value = stats_json(stats);
    value["materialization"] = json!({
        "full_state_loads": full_state_loads,
        "full_catalog_loads": full_catalog_loads,
        "provider": "process-interval-atomic-counters",
    });
    Ok(value)
}

fn physical_observation(root: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    let database = NativeDatabase::open(root.join("structure"))?;
    let observation = database.physical_observation()?;
    Ok(json!({
        "page_count": observation.page_count,
        "physical_page_reads": observation.physical_page_reads,
        "wal_bytes": observation.wal_bytes,
        "process_full_state_loads": observation.process_full_state_loads,
        "process_full_catalog_loads": observation.process_full_catalog_loads,
    }))
}
