// SPDX-License-Identifier: AGPL-3.0-only

//! Controlled Native G7 benchmark runner.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    hint::black_box,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::mpsc::{self, TryRecvError},
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
    ProductPreparedHandle, ProductPrincipal, ProductRequestContext, ProductResponse,
    ProductSession, ProductSessionId,
};
use hyphae_native_runtime::{
    ANN_PARTITION_ROUTING_POLICY_V1, AnnPartitionRoutingOutcome, AnnSearchOptions, CalibrationMode,
    CalibrationRequest, HardwareCalibration, HardwareProfile, HnswConfig,
    InitialAnnBulkBuildEvidence, InitialAnnBulkBuilder, InitialAnnBulkProgress,
    InitialAnnBulkProgressStage, MAX_INITIAL_ANN_BULK_PARTITIONS,
    NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM, NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE,
    NativeCommitBatch, NativeCommitClient, NativeCommitScheduler, NativeDatabase,
    NativeDeltaWriteBatch, NativeExecutionPool, NativeExecutionTopology,
    NativeFilteredLexicalReadView, NativeFilteredLexicalReadViewOpenReceipt, NativeGovernorPolicy,
    NativeHybridFusion, NativeHybridOutcome, NativeHybridReadView, NativeHybridReadViewOpenReceipt,
    NativeHybridReadViewOpenRequest, NativeHybridReadViewQuery, NativeLexicalReadView,
    NativeLexicalReadViewOpenRequest, NativeResourceGovernor, NativeStructureScalarFilter,
    ThreadScalingDiagnostic, Vector, VectorMetric, WorkloadClass,
};
use hyphae_native_types::ObjectId;
use serde_json::json;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

const VERSION: &str = "hyphae-native-g7-receipt-v4";
const DEFAULT_OBSERVATIONS: usize = 1_000_000;
const DEFAULT_WARMUP: usize = 100_000;
const STRUCTURE_KEYS: usize = 2_048;
const SQL_KEYS: usize = 128;
const CLOSURE_SEARCH_DOCUMENTS: usize = 1_000_000;
const CLOSURE_VECTOR_DIMENSION: u16 = 384;
const CLOSURE_ANN_LOGICAL_PARTITIONS: usize = 64;
const ANN_PARTITION_POLICY: &str = "g7-fixed-64-logical-partitions-v1";
const K: usize = 10;
const ANN_QUERY_BREADTH: usize = 64;
const G7_PREFERRED_ANN_PARTITIONS: usize = 32;
const G7_LEXICAL_RETAINED_POSTINGS: usize = K;
const G7_LEXICAL_RETAINED_BYTES: u64 = 1024 * 1024;
const G7_FILTER_KEY_PREFIX: &[u8] = b"g7-filter:";
const G7_FILTER_EXPECTED_VALUE: &[u8] = b"keep";
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(10);
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_CHUNK_UNITS: usize = 10_000;
const EVIDENCE_MEASUREMENT_CHUNK: usize = 256;
const STRICT_GROUP_COMMIT_COHORT_WIDTH: usize = 32;
const STRICT_GROUP_COMMIT_QUEUE_CAPACITY: usize = 64;
const STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT: usize = 32;
const STRICT_GROUP_COMMIT_COLLECTION_WAIT: Duration = Duration::ZERO;
const STRICT_GROUP_COMMIT_EXECUTION_WAIT: Duration = Duration::from_secs(60);
const G7_DATABASE_QUEUE_WAIT: Duration = Duration::from_secs(60);
const SEED_BATCH_DOCUMENTS: usize = 512;
const MAX_SEED_COHORTS: usize = 2;
const SEED_PARTITION_RULE: &str = "batch-index-modulo-cohort-count-ordinal-commit-v1";
const LOCAL_DAEMON_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SeedCohortPlan {
    cohort_count: usize,
    batch_size: usize,
    partition_rule: &'static str,
}

struct LocalDaemonThread {
    shutdown: mpsc::SyncSender<()>,
    completed: mpsc::Receiver<Result<(), String>>,
    worker: Option<thread::JoinHandle<()>>,
    #[cfg(test)]
    server_thread_id: thread::ThreadId,
}

impl LocalDaemonThread {
    fn start(
        product: NativeProduct,
        endpoint: String,
        config: NativeDaemonConfig,
    ) -> Result<Self, Box<dyn Error>> {
        let (ready_send, ready_receive) = mpsc::sync_channel(1);
        let (shutdown_send, shutdown_receive) = mpsc::sync_channel(1);
        let (completed_send, completed_receive) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("hyphae-g7-local-daemon".to_owned())
            .spawn(move || {
                let result = run_local_daemon_thread(
                    product,
                    endpoint,
                    config,
                    &ready_send,
                    &shutdown_receive,
                );
                let _ignored = completed_send.send(result);
            })?;
        let server_thread_id = match ready_receive.recv_timeout(LOCAL_DAEMON_LIFECYCLE_TIMEOUT) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                worker
                    .join()
                    .map_err(|_| "local daemon startup thread panicked")?;
                return Err(error.into());
            }
            Err(error) => {
                let _ignored = shutdown_send.try_send(());
                if completed_receive
                    .recv_timeout(LOCAL_DAEMON_LIFECYCLE_TIMEOUT)
                    .is_ok()
                {
                    worker
                        .join()
                        .map_err(|_| "local daemon startup thread panicked")?;
                }
                return Err(format!("local daemon startup did not become ready: {error}").into());
            }
        };
        if server_thread_id == thread::current().id() {
            return Err("local daemon did not acquire a dedicated thread".into());
        }
        Ok(Self {
            shutdown: shutdown_send,
            completed: completed_receive,
            worker: Some(worker),
            #[cfg(test)]
            server_thread_id,
        })
    }

    #[cfg(test)]
    fn server_thread_id(&self) -> thread::ThreadId {
        self.server_thread_id
    }

    fn shutdown(mut self) -> Result<(), Box<dyn Error>> {
        self.shutdown
            .send(())
            .map_err(|_| "local daemon stopped before shutdown")?;
        let completed = self
            .completed
            .recv_timeout(LOCAL_DAEMON_LIFECYCLE_TIMEOUT)
            .map_err(|error| format!("local daemon shutdown did not complete: {error}"))?;
        self.worker
            .take()
            .ok_or("local daemon worker is unavailable")?
            .join()
            .map_err(|_| "local daemon shutdown thread panicked")?;
        completed.map_err(Into::into)
    }
}

impl Drop for LocalDaemonThread {
    fn drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let _ignored = self.shutdown.try_send(());
        if self
            .completed
            .recv_timeout(LOCAL_DAEMON_LIFECYCLE_TIMEOUT)
            .is_ok()
            && let Some(worker) = self.worker.take()
        {
            let _ignored = worker.join();
        }
    }
}

fn run_local_daemon_thread(
    product: NativeProduct,
    endpoint: String,
    config: NativeDaemonConfig,
    ready: &mpsc::SyncSender<Result<thread::ThreadId, String>>,
    shutdown: &mpsc::Receiver<()>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let daemon = match NativeDaemon::start(product, endpoint, config) {
            Ok(daemon) => daemon,
            Err(error) => {
                let message = error.to_string();
                let _ignored = ready.send(Err(message.clone()));
                return Err(message);
            }
        };
        ready
            .send(Ok(thread::current().id()))
            .map_err(|_| "local daemon ready receiver disappeared".to_owned())?;
        loop {
            match shutdown.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
        daemon
            .shutdown()
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    })
}

impl SeedCohortPlan {
    fn for_database(database: &NativeDatabase, document_count: usize) -> Self {
        let total_batches = document_count.div_ceil(SEED_BATCH_DOCUMENTS).max(1);
        Self {
            cohort_count: seed_cohort_count(database).min(total_batches),
            batch_size: SEED_BATCH_DOCUMENTS,
            partition_rule: SEED_PARTITION_RULE,
        }
    }
}

/// Bounds staging by both process visibility and the installed governor's
/// effective Mutation-class compute and I/O capacity. Retained-memory
/// admission remains enforced by each detached delta batch itself.
fn seed_cohort_count(database: &NativeDatabase) -> usize {
    let host_parallelism = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let admitted_parallelism = database
        .resource_governor()
        .map_or(host_parallelism, |governor| {
            let policy = governor.policy();
            let mutation = policy.limit(WorkloadClass::Mutation);
            [
                policy.schedulable_compute_threads,
                policy.io_slots,
                mutation.compute_threads,
                mutation.io_slots,
            ]
            .into_iter()
            .min()
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(1)
        });
    bounded_seed_cohort_count(host_parallelism, admitted_parallelism)
}

fn bounded_seed_cohort_count(host_parallelism: usize, admitted_parallelism: usize) -> usize {
    host_parallelism
        .min(admitted_parallelism)
        .clamp(1, MAX_SEED_COHORTS)
}

fn stage_seed_batch(
    database: &NativeDatabase,
    lexical_index: ObjectId,
    batch_start: usize,
    batch_end: usize,
    document_count: usize,
) -> Result<NativeDeltaWriteBatch, String> {
    let mut batch = database
        .begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Memory)
        .map_err(|error| error.to_string())?;
    for id in batch_start..batch_end {
        let document_id = (id as u128 + 1).to_be_bytes();
        let text = if id == document_count / 2 {
            "rare g7 native benchmark term"
        } else {
            "common g7 native benchmark"
        };
        database
            .stage_delta_index_document(
                &mut batch,
                lexical_index,
                document_id.to_vec(),
                text.to_owned(),
            )
            .map_err(|error| error.to_string())?;
        database
            .stage_delta_set(
                &mut batch,
                filter_key(&document_id),
                if id % 2 == 0 {
                    b"keep".to_vec()
                } else {
                    b"drop".to_vec()
                },
                None,
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(batch)
}

fn sort_staged_seed_batches<T>(staged: &mut [(usize, T)]) {
    staged.sort_unstable_by_key(|(batch_index, _)| *batch_index);
}

/// Stages disjoint delta batches concurrently, then commits the tagged
/// batches in deterministic ordinal order. Commit publication remains the
/// sole-writer path and progress advances only after each successful commit.
fn seed_lexical_with_cohorts(
    database: &mut NativeDatabase,
    lexical_index: ObjectId,
    document_count: usize,
    plan: SeedCohortPlan,
    progress: &Arc<CellProgress>,
) -> Result<(), Box<dyn Error>> {
    if document_count == 0 {
        return Ok(());
    }
    let total_batches = document_count.div_ceil(plan.batch_size);
    let cohorts = plan.cohort_count.min(total_batches).max(1);
    let mut window_start = 0usize;
    while window_start < total_batches {
        let window = cohorts.min(total_batches - window_start);
        let read_database: &NativeDatabase = database;
        let mut staged: Vec<(usize, Result<NativeDeltaWriteBatch, String>)> =
            thread::scope(|scope| {
                let (sender, receiver) = std::sync::mpsc::channel();
                for slot in 0..window {
                    let batch_index = window_start + slot;
                    let batch_start = batch_index * plan.batch_size;
                    let batch_end = (batch_start + plan.batch_size).min(document_count);
                    let slot_sender = sender.clone();
                    scope.spawn(move || {
                        let staged = stage_seed_batch(
                            read_database,
                            lexical_index,
                            batch_start,
                            batch_end,
                            document_count,
                        );
                        let _ignored = slot_sender.send((batch_index, staged));
                    });
                }
                drop(sender);
                receiver.into_iter().collect()
            });
        if staged.len() != window {
            return Err(format!(
                "G7 seed window staged {} batches but {window} were required",
                staged.len()
            )
            .into());
        }
        sort_staged_seed_batches(&mut staged);
        for (batch_index, staged_batch) in staged {
            let batch = staged_batch.map_err(|error| -> Box<dyn Error> { error.into() })?;
            database.commit_optimistic(batch)?;
            let batch_start = batch_index * plan.batch_size;
            let committed = (batch_start + plan.batch_size).min(document_count) - batch_start;
            progress.advance_search_seed_lexical(committed)?;
        }
        window_start += window;
    }
    Ok(())
}
const READ_WORKLOADS: u64 = 10;
const G7_SURFACES: usize = 11;
const G7_SURFACE_NAMES: [&str; G7_SURFACES] = [
    "embedded-structure-point-get",
    "embedded-prepared-sql-primary-key",
    "local-structure-point-get",
    "local-prepared-sql-primary-key",
    "indexed-sql-bounded-read",
    "two-index-join-bounded-read",
    "bm25-top10",
    "filtered-bm25-top10",
    "ann-top10-recall-095",
    "hybrid-top10",
    "strict-group-commit",
];
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct StrictGroupCommitPlan {
    logical_commits: usize,
    cohort_count: usize,
    final_cohort_size: usize,
    cohort_size_histogram: BTreeMap<usize, usize>,
    cohort_position_histogram: BTreeMap<usize, usize>,
}

struct StrictGroupCommitWindowTimer {
    started: Instant,
}

#[derive(Debug, Default)]
struct StrictProducerActivity {
    current: AtomicUsize,
    maximum: AtomicUsize,
}

impl StrictProducerActivity {
    fn enter(self: &Arc<Self>) -> StrictProducerActivityGuard {
        let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        StrictProducerActivityGuard {
            activity: Arc::clone(self),
        }
    }

    fn current(&self) -> usize {
        self.current.load(Ordering::SeqCst)
    }

    fn maximum(&self) -> usize {
        self.maximum.load(Ordering::SeqCst)
    }
}

struct StrictProducerActivityGuard {
    activity: Arc<StrictProducerActivity>,
}

impl Drop for StrictProducerActivityGuard {
    fn drop(&mut self) {
        let previous = self.activity.current.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "strict producer activity underflowed");
    }
}

impl StrictGroupCommitWindowTimer {
    fn start() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    fn finish(self) -> Result<Duration, Box<dyn Error>> {
        self.finish_at(Instant::now())
    }

    fn finish_at(self, finished: Instant) -> Result<Duration, Box<dyn Error>> {
        finished
            .checked_duration_since(self.started)
            .ok_or_else(|| "strict group commit window clock moved backwards".into())
    }
}

impl StrictGroupCommitPlan {
    fn new(logical_commits: usize) -> Result<Self, Box<dyn Error>> {
        if logical_commits == 0 {
            return Err("strict group commit requires at least one logical commit".into());
        }
        let full_cohorts = logical_commits / STRICT_GROUP_COMMIT_COHORT_WIDTH;
        let remainder = logical_commits % STRICT_GROUP_COMMIT_COHORT_WIDTH;
        let cohort_count = logical_commits.div_ceil(STRICT_GROUP_COMMIT_COHORT_WIDTH);
        let final_cohort_size = if remainder == 0 {
            STRICT_GROUP_COMMIT_COHORT_WIDTH
        } else {
            remainder
        };
        let mut cohort_size_histogram = BTreeMap::new();
        if full_cohorts > 0 {
            cohort_size_histogram.insert(STRICT_GROUP_COMMIT_COHORT_WIDTH, full_cohorts);
        }
        if remainder > 0 {
            cohort_size_histogram.insert(remainder, 1);
        }
        let cohort_position_histogram = (0..STRICT_GROUP_COMMIT_COHORT_WIDTH)
            .map(|position| {
                (
                    position,
                    full_cohorts.saturating_add(usize::from(position < remainder)),
                )
            })
            .collect();
        Ok(Self {
            logical_commits,
            cohort_count,
            final_cohort_size,
            cohort_size_histogram,
            cohort_position_histogram,
        })
    }

    fn validate_histograms(
        &self,
        cohort_sizes: &BTreeMap<usize, usize>,
        cohort_positions: &BTreeMap<usize, usize>,
    ) -> Result<(), Box<dyn Error>> {
        if cohort_sizes != &self.cohort_size_histogram {
            return Err("strict group commit cohort-size histogram is not canonical".into());
        }
        if cohort_positions != &self.cohort_position_histogram {
            return Err("strict group commit cohort-position histogram is not canonical".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StrictCommitObservation {
    transaction_id: u128,
    commit_csn: u64,
    catalog_version: u64,
    commit_lsn: u64,
    wal_block_digest: [u8; 32],
    cohort_size: usize,
    cohort_position: usize,
    page_synchronizations: usize,
    wal_synchronizations: usize,
    admission_wait_nanos: u64,
    queue_wait_nanos: u64,
    cohort_execution_nanos: u64,
    page_synchronization_nanos: u64,
    wal_synchronization_nanos: u64,
    end_to_end_nanos: u64,
}

impl StrictCommitObservation {
    fn from_completion(
        completion: hyphae_native_runtime::ScheduledCommitCompletion,
    ) -> Result<Self, Box<dyn Error>> {
        let receipt = completion.receipt;
        if receipt.commit.durability != hyphae_native_types::DurabilityClass::Group {
            return Err("strict group commit returned a non-group durability receipt".into());
        }
        Ok(Self {
            transaction_id: receipt.commit.transaction_id.get(),
            commit_csn: receipt.commit.commit_csn.get(),
            catalog_version: receipt.commit.catalog_version.get(),
            commit_lsn: receipt.commit.commit_lsn.get(),
            wal_block_digest: receipt.commit.wal_block_digest,
            cohort_size: receipt.commit.durability_cohort_size,
            cohort_position: receipt.commit.durability_cohort_position,
            page_synchronizations: completion.page_synchronizations,
            wal_synchronizations: completion.wal_synchronizations,
            admission_wait_nanos: duration_nanos(receipt.admission_wait)?,
            queue_wait_nanos: duration_nanos(receipt.queue_wait)?,
            cohort_execution_nanos: duration_nanos(receipt.cohort_execution)?,
            page_synchronization_nanos: duration_nanos(receipt.page_synchronization)?,
            wal_synchronization_nanos: duration_nanos(receipt.wal_synchronization)?,
            end_to_end_nanos: duration_nanos(receipt.end_to_end)?,
        })
    }
}

#[derive(Debug, Default)]
struct StrictGroupCommitTimings {
    admission_wait: Vec<u64>,
    queue_wait: Vec<u64>,
    cohort_execution: Vec<u64>,
    page_synchronization: Vec<u64>,
    wal_synchronization: Vec<u64>,
    end_to_end: Vec<u64>,
}

impl StrictGroupCommitTimings {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            admission_wait: Vec::with_capacity(capacity),
            queue_wait: Vec::with_capacity(capacity),
            cohort_execution: Vec::with_capacity(capacity),
            page_synchronization: Vec::with_capacity(capacity),
            wal_synchronization: Vec::with_capacity(capacity),
            end_to_end: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, observation: StrictCommitObservation) {
        self.admission_wait.push(observation.admission_wait_nanos);
        self.queue_wait.push(observation.queue_wait_nanos);
        self.cohort_execution
            .push(observation.cohort_execution_nanos);
        self.page_synchronization
            .push(observation.page_synchronization_nanos);
        self.wal_synchronization
            .push(observation.wal_synchronization_nanos);
        self.end_to_end.push(observation.end_to_end_nanos);
    }

    fn json(&self) -> Result<serde_json::Value, Box<dyn Error>> {
        Ok(json!({
            "admission_wait": nanosecond_summary(&self.admission_wait)?,
            "queue_wait": nanosecond_summary(&self.queue_wait)?,
            "cohort_execution": nanosecond_summary(&self.cohort_execution)?,
            "page_synchronization": nanosecond_summary(&self.page_synchronization)?,
            "wal_synchronization": nanosecond_summary(&self.wal_synchronization)?,
            "end_to_end": nanosecond_summary(&self.end_to_end)?,
        }))
    }
}

#[derive(Debug)]
struct StrictGroupCommitEvidence {
    plan: StrictGroupCommitPlan,
    baseline_visible_csn: u64,
    maximum_active_producers: usize,
    observed_cohorts: usize,
    observed_commits: usize,
    maximum_outstanding: usize,
    cohort_size_histogram: BTreeMap<usize, usize>,
    cohort_position_histogram: BTreeMap<usize, usize>,
    first_commit_csn: Option<u64>,
    last_commit_csn: Option<u64>,
    receipt_digest: blake3::Hasher,
    page_synchronizations: usize,
    wal_synchronizations: usize,
    cohort_execution_nanos_total: u64,
    page_synchronization_nanos_total: u64,
    wal_synchronization_nanos_total: u64,
    timings: StrictGroupCommitTimings,
}

impl StrictGroupCommitEvidence {
    fn new(plan: StrictGroupCommitPlan, baseline_visible_csn: u64) -> Self {
        let mut receipt_digest = blake3::Hasher::new();
        receipt_digest.update(b"blake3-csn-ordered-native-commit-receipts-v1\0");
        Self {
            timings: StrictGroupCommitTimings::with_capacity(plan.logical_commits),
            plan,
            baseline_visible_csn,
            maximum_active_producers: 0,
            observed_cohorts: 0,
            observed_commits: 0,
            maximum_outstanding: 0,
            cohort_size_histogram: BTreeMap::new(),
            cohort_position_histogram: BTreeMap::new(),
            first_commit_csn: None,
            last_commit_csn: None,
            receipt_digest,
            page_synchronizations: 0,
            wal_synchronizations: 0,
            cohort_execution_nanos_total: 0,
            page_synchronization_nanos_total: 0,
            wal_synchronization_nanos_total: 0,
        }
    }

    fn observe_cohort(
        &mut self,
        observations: &[StrictCommitObservation],
        outstanding: usize,
    ) -> Result<(), Box<dyn Error>> {
        if self.observed_cohorts >= self.plan.cohort_count {
            return Err("strict group commit produced an extra cohort".into());
        }
        let expected_size = if self.observed_cohorts + 1 == self.plan.cohort_count {
            self.plan.final_cohort_size
        } else {
            STRICT_GROUP_COMMIT_COHORT_WIDTH
        };
        if observations.len() != expected_size
            || outstanding != expected_size
            || outstanding > STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT
        {
            return Err("strict group commit cohort or outstanding window changed width".into());
        }
        let first = observations
            .first()
            .ok_or("strict group commit produced an empty cohort")?;
        if first.page_synchronizations != 1 || first.wal_synchronizations != 1 {
            return Err(
                "strict group commit cohort did not perform exactly one physical flush".into(),
            );
        }
        if first.cohort_execution_nanos == 0
            || first.page_synchronization_nanos == 0
            || first.wal_synchronization_nanos == 0
            || first.cohort_execution_nanos
                < first
                    .page_synchronization_nanos
                    .checked_add(first.wal_synchronization_nanos)
                    .ok_or("strict group commit synchronization timing overflowed")?
        {
            return Err("strict group commit cohort timings are not physically credible".into());
        }
        for (position, observation) in observations.iter().copied().enumerate() {
            if observation.cohort_size != expected_size
                || observation.cohort_position != position
                || observation.end_to_end_nanos == 0
                || observation.page_synchronizations != first.page_synchronizations
                || observation.wal_synchronizations != first.wal_synchronizations
                || observation.cohort_execution_nanos != first.cohort_execution_nanos
                || observation.page_synchronization_nanos != first.page_synchronization_nanos
                || observation.wal_synchronization_nanos != first.wal_synchronization_nanos
            {
                return Err("strict group commit returned inconsistent cohort receipts".into());
            }
            let expected_csn = self
                .baseline_visible_csn
                .checked_add(u64::try_from(self.observed_commits)?)
                .and_then(|value| value.checked_add(1))
                .ok_or("strict group commit CSN interval overflowed")?;
            if observation.commit_csn != expected_csn {
                return Err("strict group commit CSNs are not contiguous and ordered".into());
            }
            self.first_commit_csn.get_or_insert(observation.commit_csn);
            self.last_commit_csn = Some(observation.commit_csn);
            self.hash_receipt(observation)?;
            *self
                .cohort_position_histogram
                .entry(observation.cohort_position)
                .or_default() += 1;
            self.timings.push(observation);
            self.observed_commits += 1;
        }
        *self.cohort_size_histogram.entry(expected_size).or_default() += 1;
        self.maximum_outstanding = self.maximum_outstanding.max(outstanding);
        self.observed_cohorts += 1;
        self.page_synchronizations = self
            .page_synchronizations
            .checked_add(first.page_synchronizations)
            .ok_or("strict group commit page synchronization count overflowed")?;
        self.wal_synchronizations = self
            .wal_synchronizations
            .checked_add(first.wal_synchronizations)
            .ok_or("strict group commit WAL synchronization count overflowed")?;
        self.cohort_execution_nanos_total = self
            .cohort_execution_nanos_total
            .checked_add(first.cohort_execution_nanos)
            .ok_or("strict group commit execution timing overflowed")?;
        self.page_synchronization_nanos_total = self
            .page_synchronization_nanos_total
            .checked_add(first.page_synchronization_nanos)
            .ok_or("strict group commit page timing overflowed")?;
        self.wal_synchronization_nanos_total = self
            .wal_synchronization_nanos_total
            .checked_add(first.wal_synchronization_nanos)
            .ok_or("strict group commit WAL timing overflowed")?;
        Ok(())
    }

    fn hash_receipt(&mut self, observation: StrictCommitObservation) -> Result<(), Box<dyn Error>> {
        self.receipt_digest
            .update(&observation.commit_csn.to_le_bytes());
        self.receipt_digest
            .update(&observation.transaction_id.to_le_bytes());
        self.receipt_digest
            .update(&observation.catalog_version.to_le_bytes());
        self.receipt_digest
            .update(&observation.commit_lsn.to_le_bytes());
        self.receipt_digest.update(&observation.wal_block_digest);
        self.receipt_digest
            .update(&u64::try_from(observation.cohort_size)?.to_le_bytes());
        self.receipt_digest
            .update(&u64::try_from(observation.cohort_position)?.to_le_bytes());
        Ok(())
    }

    fn validate_complete(&self) -> Result<(), Box<dyn Error>> {
        if self.observed_cohorts != self.plan.cohort_count
            || self.observed_commits != self.plan.logical_commits
            || self.maximum_active_producers == 0
            || self.maximum_outstanding
                != STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT.min(self.plan.logical_commits)
            || self.page_synchronizations != self.plan.cohort_count
            || self.wal_synchronizations != self.plan.cohort_count
        {
            return Err("strict group commit evidence is incomplete".into());
        }
        self.plan
            .validate_histograms(&self.cohort_size_histogram, &self.cohort_position_histogram)
    }

    fn record_producer_activity(
        &mut self,
        current: usize,
        maximum: usize,
        expected: usize,
    ) -> Result<(), Box<dyn Error>> {
        if current != 0 || maximum != expected {
            return Err(
                "strict group commit did not observe its configured producer concurrency".into(),
            );
        }
        self.maximum_active_producers = maximum;
        Ok(())
    }

    fn stats(&self, elapsed_seconds: f64) -> Result<Stats, Box<dyn Error>> {
        if elapsed_seconds <= 0.0 || !elapsed_seconds.is_finite() {
            return Err("strict group commit wall time is not positive and finite".into());
        }
        Ok(stats_from_samples(
            self.timings.end_to_end.clone(),
            elapsed_seconds,
        ))
    }

    fn json(
        &self,
        producer_concurrency: usize,
        maintenance: &StrictGroupCommitMaintenanceEvidence,
        reopen: &StrictGroupCommitReopenEvidence,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        self.validate_complete()?;
        if self.maximum_active_producers != producer_concurrency {
            return Err(
                "strict group commit producer activity changed before serialization".into(),
            );
        }
        if maintenance.total_time_nanos == 0
            || maintenance.maintenance_csn
                != self
                    .last_commit_csn
                    .ok_or("strict group commit evidence omitted its last CSN")?
                    .saturating_add(1)
        {
            return Err("strict group commit maintenance changed before serialization".into());
        }
        Ok(json!({
            "schema": "hyphae-native-g7-strict-group-commit-evidence-v2",
            "latency_scope": "scheduler-enqueue-through-durable-response-v1",
            "throughput_scope": "bounded-cohort-window-wall-time-v1",
            "submission_mode": "explicit-bounded-cohort-v1",
            "producer_concurrency": producer_concurrency,
            "maximum_active_producers": self.maximum_active_producers,
            "cohort_width": STRICT_GROUP_COMMIT_COHORT_WIDTH,
            "scheduler_queue_capacity": STRICT_GROUP_COMMIT_QUEUE_CAPACITY,
            "outstanding_limit": STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT,
            "maximum_outstanding": self.maximum_outstanding,
            "logical_commits": self.observed_commits,
            "cohort_count": self.observed_cohorts,
            "final_cohort_size": self.plan.final_cohort_size,
            "cohort_size_histogram": self.cohort_size_histogram,
            "cohort_position_histogram": self.cohort_position_histogram,
            "first_commit_csn": self.first_commit_csn
                .ok_or("strict group commit evidence omitted its first CSN")?,
            "last_commit_csn": self.last_commit_csn
                .ok_or("strict group commit evidence omitted its last CSN")?,
            "distinct_commit_csns": self.observed_commits,
            "commit_receipt_digest_algorithm":
                "blake3-csn-ordered-native-commit-receipts-v1",
            "commit_receipt_digest": self.receipt_digest.clone().finalize().to_hex().to_string(),
            "page_synchronizations": self.page_synchronizations,
            "wal_synchronizations": self.wal_synchronizations,
            "cohort_execution_nanos_total": self.cohort_execution_nanos_total,
            "page_synchronization_nanos_total": self.page_synchronization_nanos_total,
            "wal_synchronization_nanos_total": self.wal_synchronization_nanos_total,
            "timing_sample_count": self.observed_commits,
            "timings_nanoseconds": self.timings.json()?,
            "maintenance": maintenance.value.clone(),
            "reopen": reopen.json(),
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StrictGroupCommitReopenEvidence {
    baseline_visible_csn: u64,
    baseline_committed_transactions: usize,
    reopened_visible_csn: u64,
    reopened_committed_transactions: usize,
    wal_base_csn: u64,
    retained_wal_blocks: usize,
    retained_wal_bytes: u64,
    replayed_transactions: usize,
    verified_logical_commits: usize,
    missing_keys: usize,
    mismatched_values: usize,
    expected_state_digest: String,
    recovered_state_digest: String,
    manifest_verification_time_nanos: u64,
    wal_physical_verification_time_nanos: u64,
    wal_semantic_replay_time_nanos: u64,
    root_validation_time_nanos: u64,
    open_time_nanos: u64,
    verification_time_nanos: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StrictGroupCommitMaintenanceEvidence {
    value: serde_json::Value,
    maintenance_csn: u64,
    total_time_nanos: u64,
}

impl StrictGroupCommitReopenEvidence {
    fn validate(
        &self,
        plan: &StrictGroupCommitPlan,
        first_commit_csn: u64,
        last_commit_csn: u64,
        maintenance_csn: u64,
    ) -> Result<(), Box<dyn Error>> {
        let recovery_component_nanos = self
            .manifest_verification_time_nanos
            .checked_add(self.wal_physical_verification_time_nanos)
            .and_then(|value| value.checked_add(self.wal_semantic_replay_time_nanos))
            .and_then(|value| value.checked_add(self.root_validation_time_nanos))
            .ok_or("strict group commit recovery timing overflowed")?;
        if self
            .reopened_visible_csn
            .checked_sub(self.baseline_visible_csn)
            != Some(u64::try_from(plan.logical_commits)?.saturating_add(1))
            || self
                .reopened_committed_transactions
                .checked_sub(self.baseline_committed_transactions)
                != Some(plan.logical_commits.saturating_add(1))
            || first_commit_csn != self.baseline_visible_csn.saturating_add(1)
            || last_commit_csn.saturating_add(1) != maintenance_csn
            || self.reopened_visible_csn != maintenance_csn
            || self.wal_base_csn != maintenance_csn
            || self.retained_wal_blocks != 0
            || self.retained_wal_bytes != 0
            || self.replayed_transactions != 0
            || self.verified_logical_commits != plan.logical_commits
            || self.missing_keys != 0
            || self.mismatched_values != 0
            || self.expected_state_digest != self.recovered_state_digest
            || self.manifest_verification_time_nanos == 0
            || self.wal_physical_verification_time_nanos == 0
            || self.wal_semantic_replay_time_nanos == 0
            || self.root_validation_time_nanos == 0
            || self.open_time_nanos == 0
            || self.open_time_nanos < recovery_component_nanos
            || self.verification_time_nanos == 0
        {
            return Err("strict group commit reopen evidence is incomplete or inconsistent".into());
        }
        Ok(())
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "provider": "retained-anchor-reopened-root-snapshot-full-key-digest-v2",
            "baseline_visible_csn": self.baseline_visible_csn,
            "baseline_committed_transactions": self.baseline_committed_transactions,
            "reopened_visible_csn": self.reopened_visible_csn,
            "reopened_committed_transactions": self.reopened_committed_transactions,
            "wal_base_csn": self.wal_base_csn,
            "retained_wal_blocks": self.retained_wal_blocks,
            "retained_wal_bytes": self.retained_wal_bytes,
            "replayed_transactions": self.replayed_transactions,
            "verified_logical_commits": self.verified_logical_commits,
            "missing_keys": self.missing_keys,
            "mismatched_values": self.mismatched_values,
            "state_digest_algorithm": "blake3-logical-id-key-value-v1",
            "expected_state_digest": self.expected_state_digest,
            "recovered_state_digest": self.recovered_state_digest,
            "manifest_verification_time_nanos": self.manifest_verification_time_nanos,
            "wal_physical_verification_time_nanos": self.wal_physical_verification_time_nanos,
            "wal_semantic_replay_time_nanos": self.wal_semantic_replay_time_nanos,
            "root_validation_time_nanos": self.root_validation_time_nanos,
            "open_time_nanos": self.open_time_nanos,
            "verification_time_nanos": self.verification_time_nanos,
        })
    }
}

fn strict_group_commit_key(sequence: usize) -> Vec<u8> {
    format!("g7-group-{sequence}").into_bytes()
}

fn strict_group_commit_value(sequence: usize) -> Vec<u8> {
    format!("g7-group-value-{sequence}").into_bytes()
}

fn update_strict_group_state_digest(
    digest: &mut blake3::Hasher,
    sequence: usize,
    key: &[u8],
    value: &[u8],
) -> Result<(), Box<dyn Error>> {
    digest.update(&u64::try_from(sequence)?.to_le_bytes());
    digest.update(&u64::try_from(key.len())?.to_le_bytes());
    digest.update(key);
    digest.update(&u64::try_from(value.len())?.to_le_bytes());
    digest.update(value);
    Ok(())
}

fn duration_nanos(duration: Duration) -> Result<u64, Box<dyn Error>> {
    u64::try_from(duration.as_nanos()).map_err(|_| "nanosecond duration exceeds u64".into())
}

fn nanosecond_summary(samples: &[u64]) -> Result<serde_json::Value, Box<dyn Error>> {
    if samples.is_empty() {
        return Err("cannot summarize an empty timing sample".into());
    }
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    Ok(json!({
        "p50": percentile(&samples, 500),
        "p95": percentile(&samples, 950),
        "p99": percentile(&samples, 990),
        "p999": percentile(&samples, 999),
        "maximum": samples.last().copied().ok_or("timing summary lost its sample")?,
    }))
}

#[derive(Clone, Copy, Debug)]
struct RoutingObservation {
    execution_workers: u64,
    execution_worker_batches: u64,
    execution_waves: u64,
    selected_partitions: u64,
    targeted_single_batches: u64,
    generic_single_fallback_batches: u64,
    next_partition_lower_bound: f64,
    kth_distance: f64,
}

#[derive(Clone, Copy, Debug)]
struct RoutingIntervalEvidence {
    execution_workers_max: u64,
    execution_worker_batches_max: u64,
    execution_waves_max: u64,
    selected_certified: u64,
    selected_partitions_max: u64,
    targeted_single_batches: u64,
    generic_single_fallback_batches: u64,
    next_partition_lower_bound_present: u64,
    minimum_next_partition_lower_bound: f64,
    maximum_kth_distance: f64,
}

impl RoutingIntervalEvidence {
    fn new() -> Self {
        Self {
            execution_workers_max: 0,
            execution_worker_batches_max: 0,
            execution_waves_max: 0,
            selected_certified: 0,
            selected_partitions_max: 0,
            targeted_single_batches: 0,
            generic_single_fallback_batches: 0,
            next_partition_lower_bound_present: 0,
            minimum_next_partition_lower_bound: f64::INFINITY,
            maximum_kth_distance: 0.0,
        }
    }

    fn observe(&mut self, observation: RoutingObservation) -> Result<(), Box<dyn Error>> {
        self.execution_workers_max = self
            .execution_workers_max
            .max(observation.execution_workers);
        self.execution_worker_batches_max = self
            .execution_worker_batches_max
            .max(observation.execution_worker_batches);
        self.execution_waves_max = self.execution_waves_max.max(observation.execution_waves);
        self.selected_partitions_max = self
            .selected_partitions_max
            .max(observation.selected_partitions);
        self.minimum_next_partition_lower_bound = self
            .minimum_next_partition_lower_bound
            .min(observation.next_partition_lower_bound);
        self.maximum_kth_distance = self.maximum_kth_distance.max(observation.kth_distance);
        self.targeted_single_batches = self
            .targeted_single_batches
            .checked_add(observation.targeted_single_batches)
            .ok_or("G7 targeted ANN routing evidence overflowed")?;
        self.generic_single_fallback_batches = self
            .generic_single_fallback_batches
            .checked_add(observation.generic_single_fallback_batches)
            .ok_or("G7 generic ANN fallback evidence overflowed")?;
        self.selected_certified = self
            .selected_certified
            .checked_add(1)
            .ok_or("G7 selected ANN evidence overflowed")?;
        self.next_partition_lower_bound_present = self
            .next_partition_lower_bound_present
            .checked_add(1)
            .ok_or("G7 ANN bound evidence overflowed")?;
        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
        self.execution_workers_max = self.execution_workers_max.max(other.execution_workers_max);
        self.execution_worker_batches_max = self
            .execution_worker_batches_max
            .max(other.execution_worker_batches_max);
        self.execution_waves_max = self.execution_waves_max.max(other.execution_waves_max);
        self.selected_partitions_max = self
            .selected_partitions_max
            .max(other.selected_partitions_max);
        self.minimum_next_partition_lower_bound = self
            .minimum_next_partition_lower_bound
            .min(other.minimum_next_partition_lower_bound);
        self.maximum_kth_distance = self.maximum_kth_distance.max(other.maximum_kth_distance);
        self.targeted_single_batches = self
            .targeted_single_batches
            .checked_add(other.targeted_single_batches)
            .ok_or("G7 targeted ANN routing merge overflowed")?;
        self.generic_single_fallback_batches = self
            .generic_single_fallback_batches
            .checked_add(other.generic_single_fallback_batches)
            .ok_or("G7 generic ANN fallback merge overflowed")?;
        self.selected_certified = self
            .selected_certified
            .checked_add(other.selected_certified)
            .ok_or("G7 selected ANN merge overflowed")?;
        self.next_partition_lower_bound_present = self
            .next_partition_lower_bound_present
            .checked_add(other.next_partition_lower_bound_present)
            .ok_or("G7 ANN bound merge overflowed")?;
        Ok(())
    }

    fn observation(
        receipt: &hyphae_native_runtime::AnnSelectedSearchReceipt,
        next_lower_bound: f64,
        kth_distance: f64,
    ) -> Result<RoutingObservation, Box<dyn Error>> {
        Ok(RoutingObservation {
            execution_workers: u64::try_from(receipt.execution_workers)?,
            execution_worker_batches: u64::try_from(receipt.execution_worker_batches)?,
            execution_waves: u64::try_from(receipt.execution_waves)?,
            selected_partitions: u64::try_from(receipt.selected_partitions.len())?,
            targeted_single_batches: u64::try_from(receipt.targeted_single_batches)?,
            generic_single_fallback_batches: u64::try_from(
                receipt.generic_single_fallback_batches,
            )?,
            next_partition_lower_bound: next_lower_bound,
            kth_distance,
        })
    }

    fn json(&self, observations: usize) -> Result<serde_json::Value, Box<dyn Error>> {
        let observations = u64::try_from(observations)?;
        if self.selected_certified != observations
            || self.next_partition_lower_bound_present != observations
            || !self.minimum_next_partition_lower_bound.is_finite()
            || !self.maximum_kth_distance.is_finite()
            || self.minimum_next_partition_lower_bound <= self.maximum_kth_distance
        {
            return Err("G7 ANN routing interval did not preserve strict certification".into());
        }
        Ok(json!({
            "observations": observations,
            "execution_workers_max": self.execution_workers_max,
            "execution_worker_batches_max": self.execution_worker_batches_max,
            "execution_waves_max": self.execution_waves_max,
            "selected_certified": self.selected_certified,
            "full_fanout_requested": 0,
            "full_fanout_budget_fallback": 0,
            "single_generation_fallback": 0,
            "next_partition_lower_bound_present": self.next_partition_lower_bound_present,
            "selected_partitions_max": self.selected_partitions_max,
            "targeted_single_batches": self.targeted_single_batches,
            "generic_single_fallback_batches": self.generic_single_fallback_batches,
            "minimum_next_partition_lower_bound": self.minimum_next_partition_lower_bound,
            "maximum_kth_distance": self.maximum_kth_distance,
        }))
    }
}

impl Default for RoutingIntervalEvidence {
    fn default() -> Self {
        Self::new()
    }
}

trait MeasurementEvidence: Default + Send {
    type Observation: Send;

    fn observe(&mut self, observation: Self::Observation) -> Result<(), Box<dyn Error>>;
    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>>;
}

#[derive(Clone, Copy, Debug)]
struct QueryPhaseTiming {
    started: Instant,
    finished: Instant,
}

struct EvidenceWorkerResult<E> {
    samples: Vec<u64>,
    evidence: E,
    phases: Vec<Option<QueryPhaseTiming>>,
}

#[derive(Debug, Default)]
struct MeasurementFailure {
    failed: AtomicBool,
    message: Mutex<Option<String>>,
}

impl MeasurementEvidence for RoutingIntervalEvidence {
    type Observation = RoutingObservation;

    fn observe(&mut self, observation: Self::Observation) -> Result<(), Box<dyn Error>> {
        Self::observe(self, observation)
    }

    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
        Self::merge(self, other)
    }
}

#[derive(Clone, Copy, Debug)]
struct HybridObservation {
    routing: RoutingObservation,
    peak_admission_compute_threads: u64,
    peak_admission_memory_bytes: u64,
    result_retention_memory_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct HybridIntervalEvidence {
    routing: RoutingIntervalEvidence,
    peak_admission_executions: u64,
    peak_admission_compute_threads: u64,
    peak_admission_memory_bytes_min: u64,
    peak_admission_memory_bytes_max: u64,
    result_retention_executions: u64,
    result_retention_memory_bytes_min: u64,
    result_retention_memory_bytes_max: u64,
    fusion_executions: u64,
}

impl Default for HybridIntervalEvidence {
    fn default() -> Self {
        Self {
            routing: RoutingIntervalEvidence::default(),
            peak_admission_executions: 0,
            peak_admission_compute_threads: 0,
            peak_admission_memory_bytes_min: u64::MAX,
            peak_admission_memory_bytes_max: 0,
            result_retention_executions: 0,
            result_retention_memory_bytes_min: u64::MAX,
            result_retention_memory_bytes_max: 0,
            fusion_executions: 0,
        }
    }
}

impl MeasurementEvidence for HybridIntervalEvidence {
    type Observation = HybridObservation;

    fn observe(&mut self, observation: Self::Observation) -> Result<(), Box<dyn Error>> {
        self.routing.observe(observation.routing)?;
        self.peak_admission_executions = self.peak_admission_executions.saturating_add(1);
        self.peak_admission_compute_threads = self
            .peak_admission_compute_threads
            .max(observation.peak_admission_compute_threads);
        self.peak_admission_memory_bytes_min = self
            .peak_admission_memory_bytes_min
            .min(observation.peak_admission_memory_bytes);
        self.peak_admission_memory_bytes_max = self
            .peak_admission_memory_bytes_max
            .max(observation.peak_admission_memory_bytes);
        self.result_retention_executions = self.result_retention_executions.saturating_add(1);
        self.result_retention_memory_bytes_min = self
            .result_retention_memory_bytes_min
            .min(observation.result_retention_memory_bytes);
        self.result_retention_memory_bytes_max = self
            .result_retention_memory_bytes_max
            .max(observation.result_retention_memory_bytes);
        self.fusion_executions = self.fusion_executions.saturating_add(1);
        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
        self.routing.merge(other.routing)?;
        self.peak_admission_executions = self
            .peak_admission_executions
            .saturating_add(other.peak_admission_executions);
        self.peak_admission_compute_threads = self
            .peak_admission_compute_threads
            .max(other.peak_admission_compute_threads);
        self.peak_admission_memory_bytes_min = self
            .peak_admission_memory_bytes_min
            .min(other.peak_admission_memory_bytes_min);
        self.peak_admission_memory_bytes_max = self
            .peak_admission_memory_bytes_max
            .max(other.peak_admission_memory_bytes_max);
        self.result_retention_executions = self
            .result_retention_executions
            .saturating_add(other.result_retention_executions);
        self.result_retention_memory_bytes_min = self
            .result_retention_memory_bytes_min
            .min(other.result_retention_memory_bytes_min);
        self.result_retention_memory_bytes_max = self
            .result_retention_memory_bytes_max
            .max(other.result_retention_memory_bytes_max);
        self.fusion_executions = self
            .fusion_executions
            .saturating_add(other.fusion_executions);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct LexicalObservation {
    execution_sequence: u64,
    postings_evaluated: u64,
    physical_page_reads: u64,
}

#[derive(Clone, Copy, Debug)]
struct LexicalIntervalEvidence {
    observations: u64,
    postings_evaluated: u64,
    physical_page_reads: u64,
    execution_sequence_first: u64,
    execution_sequence_last: u64,
}

impl Default for LexicalIntervalEvidence {
    fn default() -> Self {
        Self {
            observations: 0,
            postings_evaluated: 0,
            physical_page_reads: 0,
            execution_sequence_first: u64::MAX,
            execution_sequence_last: 0,
        }
    }
}

impl MeasurementEvidence for LexicalIntervalEvidence {
    type Observation = LexicalObservation;

    fn observe(&mut self, observation: Self::Observation) -> Result<(), Box<dyn Error>> {
        self.observations = self.observations.saturating_add(1);
        self.postings_evaluated = self
            .postings_evaluated
            .saturating_add(observation.postings_evaluated);
        self.physical_page_reads = self
            .physical_page_reads
            .saturating_add(observation.physical_page_reads);
        self.execution_sequence_first = self
            .execution_sequence_first
            .min(observation.execution_sequence);
        self.execution_sequence_last = self
            .execution_sequence_last
            .max(observation.execution_sequence);
        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
        self.observations = self.observations.saturating_add(other.observations);
        self.postings_evaluated = self
            .postings_evaluated
            .saturating_add(other.postings_evaluated);
        self.physical_page_reads = self
            .physical_page_reads
            .saturating_add(other.physical_page_reads);
        self.execution_sequence_first = self
            .execution_sequence_first
            .min(other.execution_sequence_first);
        self.execution_sequence_last = self
            .execution_sequence_last
            .max(other.execution_sequence_last);
        Ok(())
    }
}

impl LexicalIntervalEvidence {
    fn json(
        &self,
        expected_observations: usize,
        global_physical_page_reads: u64,
        full_state_loads: u64,
        full_catalog_loads: u64,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        let expected = u64::try_from(expected_observations)?;
        if self.observations != expected
            || self.postings_evaluated != expected
            || self.physical_page_reads != 0
            || global_physical_page_reads != 0
            || full_state_loads != 0
            || full_catalog_loads != 0
            || self.execution_sequence_first == u64::MAX
            || self.execution_sequence_last < self.execution_sequence_first
            || self
                .execution_sequence_last
                .checked_sub(self.execution_sequence_first)
                .and_then(|delta| delta.checked_add(1))
                != Some(expected)
        {
            return Err("G7 lexical interval changed or skipped its retained execution".into());
        }
        Ok(json!({
            "observations": expected,
            "postings_evaluated": self.postings_evaluated,
            "execution_sequence_first": self.execution_sequence_first,
            "execution_sequence_last": self.execution_sequence_last,
            "receipt_physical_page_reads": self.physical_page_reads,
            "process_physical_page_reads": global_physical_page_reads,
            "full_state_loads": full_state_loads,
            "full_catalog_loads": full_catalog_loads,
            "lexical_execution": hyphae_native_runtime::NATIVE_LEXICAL_READ_VIEW_EXECUTION,
            "provider": "lexical-read-view-interval-counters-v1",
        }))
    }
}

#[derive(Clone, Copy, Debug)]
struct FilteredLexicalObservation {
    execution_sequence: u64,
    postings_scored: u64,
    filter_records_evaluated: u64,
    filter_records_matched: u64,
    physical_page_reads: u64,
}

#[derive(Clone, Copy, Debug)]
struct FilteredLexicalIntervalEvidence {
    observations: u64,
    execution_sequence_first: u64,
    execution_sequence_last: u64,
    postings_scored: u64,
    filter_records_evaluated: u64,
    filter_records_matched: u64,
    physical_page_reads: u64,
}

impl Default for FilteredLexicalIntervalEvidence {
    fn default() -> Self {
        Self {
            observations: 0,
            execution_sequence_first: u64::MAX,
            execution_sequence_last: 0,
            postings_scored: 0,
            filter_records_evaluated: 0,
            filter_records_matched: 0,
            physical_page_reads: 0,
        }
    }
}

impl MeasurementEvidence for FilteredLexicalIntervalEvidence {
    type Observation = FilteredLexicalObservation;

    fn observe(&mut self, observation: Self::Observation) -> Result<(), Box<dyn Error>> {
        self.observations = self.observations.saturating_add(1);
        self.execution_sequence_first = self
            .execution_sequence_first
            .min(observation.execution_sequence);
        self.execution_sequence_last = self
            .execution_sequence_last
            .max(observation.execution_sequence);
        self.postings_scored = self
            .postings_scored
            .saturating_add(observation.postings_scored);
        self.filter_records_evaluated = self
            .filter_records_evaluated
            .saturating_add(observation.filter_records_evaluated);
        self.filter_records_matched = self
            .filter_records_matched
            .saturating_add(observation.filter_records_matched);
        self.physical_page_reads = self
            .physical_page_reads
            .saturating_add(observation.physical_page_reads);
        Ok(())
    }

    fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
        self.observations = self.observations.saturating_add(other.observations);
        self.execution_sequence_first = self
            .execution_sequence_first
            .min(other.execution_sequence_first);
        self.execution_sequence_last = self
            .execution_sequence_last
            .max(other.execution_sequence_last);
        self.postings_scored = self.postings_scored.saturating_add(other.postings_scored);
        self.filter_records_evaluated = self
            .filter_records_evaluated
            .saturating_add(other.filter_records_evaluated);
        self.filter_records_matched = self
            .filter_records_matched
            .saturating_add(other.filter_records_matched);
        self.physical_page_reads = self
            .physical_page_reads
            .saturating_add(other.physical_page_reads);
        Ok(())
    }
}

impl FilteredLexicalIntervalEvidence {
    fn json(
        &self,
        expected_observations: usize,
        process_page_reads: u64,
        full_state_loads: u64,
        full_catalog_loads: u64,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        let expected = u64::try_from(expected_observations)?;
        if self.observations != expected
            || self.postings_scored != expected
            || self.filter_records_evaluated != expected
            || self.filter_records_matched != expected
            || self.physical_page_reads != 0
            || process_page_reads != 0
            || full_state_loads != 0
            || full_catalog_loads != 0
            || self.execution_sequence_first == u64::MAX
            || self.execution_sequence_last < self.execution_sequence_first
            || self
                .execution_sequence_last
                .checked_sub(self.execution_sequence_first)
                .and_then(|delta| delta.checked_add(1))
                != Some(expected)
        {
            return Err("G7 filtered lexical interval changed its root-bound predicate".into());
        }
        Ok(json!({
            "observations": expected,
            "execution_sequence_first": self.execution_sequence_first,
            "execution_sequence_last": self.execution_sequence_last,
            "postings_scored": self.postings_scored,
            "filter_records_evaluated": self.filter_records_evaluated,
            "filter_records_matched": self.filter_records_matched,
            "receipt_physical_page_reads": self.physical_page_reads,
            "process_physical_page_reads": process_page_reads,
            "full_state_loads": full_state_loads,
            "full_catalog_loads": full_catalog_loads,
            "filter_execution": hyphae_native_runtime::NATIVE_STRUCTURE_FILTER_EXECUTION,
            "provider": "filtered-lexical-read-view-interval-counters-v1",
        }))
    }
}

struct SearchFixture {
    database: NativeDatabase,
    hybrid_view: NativeHybridReadView,
    hybrid_view_open: NativeHybridReadViewOpenReceipt,
    lexical_view: NativeLexicalReadView,
    filtered_lexical_view: NativeFilteredLexicalReadView,
    filtered_lexical_view_open: NativeFilteredLexicalReadViewOpenReceipt,
    ann_view: hyphae_native_runtime::NativeAnnReadView,
    foreground_compute_threads: u64,
    query: Vector,
    options: AnnSearchOptions,
    initial_ann_bulk: serde_json::Value,
}

struct ExecutionAuthority {
    profile: HardwareProfile,
    calibration: HardwareCalibration,
    policy: NativeGovernorPolicy,
    topology: NativeExecutionTopology,
    topology_digest: String,
    executable_blake3: String,
    installations: Mutex<BTreeSet<String>>,
    governor: Arc<NativeResourceGovernor>,
    execution_pool: Arc<NativeExecutionPool>,
}

struct CellProgressSink {
    path: PathBuf,
    source_commit: String,
    source_tree: String,
    dataset_digest: String,
    started: Instant,
    sequence: u64,
    last_written_completed: u64,
}

#[derive(Clone)]
struct ProgressPhase {
    stage: String,
    kind: String,
    name: String,
    index: usize,
    total: usize,
    base_completed: u64,
    phase_total: u64,
    started: Instant,
    heartbeat_while_idle: bool,
}

struct CellProgress {
    sink: Mutex<Option<CellProgressSink>>,
    completed: AtomicU64,
    total: u64,
    search_documents: u64,
    phase: Mutex<ProgressPhase>,
    initial_ann_bulk: Mutex<Option<serde_json::Value>>,
    ann_suboperation: Mutex<Option<serde_json::Value>>,
    maintenance_evidence: Mutex<Option<serde_json::Value>>,
    failure: Mutex<Option<String>>,
}

struct SurfaceProgress {
    cell: Arc<CellProgress>,
    name: String,
    index: usize,
    total: u64,
    completed: AtomicU64,
    phase_base: AtomicU64,
    phase_total: AtomicU64,
}

struct SurfaceIdlePhase<'progress> {
    progress: &'progress SurfaceProgress,
    phase: String,
    completed: bool,
}

struct ProgressReporter {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

struct PartialReceiptSink {
    path: Option<PathBuf>,
    source_commit: String,
    source_tree: Option<String>,
    dataset_digest: String,
    platform: String,
    state: String,
    concurrency: usize,
    sequence: u64,
}

struct CellProgressUpdate<'a> {
    stage: &'a str,
    status: &'a str,
    checkpoint_digest: Option<String>,
    details: Option<serde_json::Value>,
}

impl CellProgress {
    fn from_environment(
        source_commit: &str,
        observations: usize,
        warmup: usize,
        warm: bool,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let path = std::env::var_os("HYPHAE_G7_PROGRESS_FILE").map(PathBuf::from);
        let source_tree = path
            .as_ref()
            .map(|_| {
                std::env::var("HYPHAE_G7_SOURCE_TREE")
                    .map_err(|_| "G7 progress requires HYPHAE_G7_SOURCE_TREE")
            })
            .transpose()?;
        let total = total_work_units(search_documents(), observations, warmup, warm)?;
        let search_documents = u64::try_from(search_documents())?;
        Self::new(
            path,
            source_commit.to_owned(),
            source_tree,
            dataset_digest(source_commit).to_hex().to_string(),
            total,
            search_documents,
        )
    }

    fn new(
        path: Option<PathBuf>,
        source_commit: String,
        source_tree: Option<String>,
        dataset_digest: String,
        total: u64,
        search_documents: u64,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let sink = match (path, source_tree) {
            (Some(path), Some(source_tree)) => Some(CellProgressSink {
                path,
                source_commit,
                source_tree,
                dataset_digest,
                started: Instant::now(),
                sequence: 0,
                last_written_completed: 0,
            }),
            (None, None) => None,
            _ => return Err("G7 progress path and source tree must be configured together".into()),
        };
        Ok(Arc::new(Self {
            sink: Mutex::new(sink),
            completed: AtomicU64::new(0),
            total,
            search_documents,
            phase: Mutex::new(ProgressPhase {
                stage: "cell-started".to_owned(),
                kind: "cell".to_owned(),
                name: "g7-cell".to_owned(),
                index: 0,
                total: G7_SURFACES,
                base_completed: 0,
                phase_total: 0,
                started: Instant::now(),
                heartbeat_while_idle: false,
            }),
            initial_ann_bulk: Mutex::new(None),
            ann_suboperation: Mutex::new(None),
            maintenance_evidence: Mutex::new(None),
            failure: Mutex::new(None),
        }))
    }

    fn start_reporter(self: &Arc<Self>) -> ProgressReporter {
        let stop = Arc::new(AtomicBool::new(false));
        let reporter_stop = Arc::clone(&stop);
        let progress = Arc::clone(self);
        let handle = thread::spawn(move || {
            while !reporter_stop.load(Ordering::Relaxed) {
                thread::park_timeout(PROGRESS_REPORT_INTERVAL);
                if reporter_stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(error) = progress.flush_if_advanced() {
                    progress.record_failure(error.to_string());
                    break;
                }
            }
        });
        ProgressReporter {
            stop,
            handle: Some(handle),
        }
    }

    fn begin_ann_build(
        &self,
        authority: &ExecutionAuthority,
        logical_partitions: usize,
    ) -> std::io::Result<()> {
        self.set_phase(
            "ann-private-build",
            "search-seed",
            "ann-bulk-build",
            1,
            2,
            self.search_documents,
        )?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "ann-private-build",
                status: "running",
                checkpoint_digest: None,
                details: Some(json!({
                    "builder": "partitioned-hnsw-v1",
                    "partition_policy": ANN_PARTITION_POLICY,
                    "requested_partitions": logical_partitions,
                    "topology_workers": authority.topology.worker_count(),
                    "topology_digest": authority.topology_digest,
                    "planned_workers": null,
                    "planned_memory_bytes": null,
                    "worker_batches": null,
                })),
            },
        )
    }

    fn begin_ann_publication(&self, evidence: &serde_json::Value) -> std::io::Result<()> {
        self.set_stage("ann-publication")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "ann-publication",
                status: "running",
                checkpoint_digest: None,
                details: Some(evidence.clone()),
            },
        )
    }

    fn update_ann_build(&self, progress: InitialAnnBulkProgress) -> std::io::Result<()> {
        let stage = match progress.stage {
            InitialAnnBulkProgressStage::Planning => "ann-planning",
            InitialAnnBulkProgressStage::Building => "ann-child-build",
        };
        let completed = if progress.stage == InitialAnnBulkProgressStage::Planning {
            0
        } else {
            usize::try_from(self.search_documents)
                .map_err(std::io::Error::other)?
                .checked_mul(progress.completed)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress multiplication overflow"))?
                .checked_div(progress.total)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress total must be nonzero"))?
        };
        let base_completed = self
            .phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .base_completed;
        self.completed.fetch_max(
            base_completed
                .checked_add(u64::try_from(completed).map_err(std::io::Error::other)?)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress units overflow"))?,
            Ordering::Relaxed,
        );
        self.set_stage(stage)?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage,
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
            },
        )
    }

    fn complete_ann(
        &self,
        checkpoint: [u8; 32],
        evidence: &serde_json::Value,
    ) -> std::io::Result<()> {
        let ann_units = self.search_documents;
        let base_completed = self
            .phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .base_completed;
        self.completed.fetch_max(
            base_completed
                .checked_add(ann_units)
                .ok_or_else(|| std::io::Error::other("G7 ANN progress units overflow"))?,
            Ordering::Relaxed,
        );
        let checkpoint_digest = blake3::Hash::from_bytes(checkpoint).to_hex().to_string();
        *self
            .initial_ann_bulk
            .lock()
            .map_err(|_| std::io::Error::other("G7 ANN evidence synchronization failed"))? =
            Some(evidence.clone());
        *self
            .ann_suboperation
            .lock()
            .map_err(|_| std::io::Error::other("G7 ANN progress synchronization failed"))? =
            Some(json!({
                "operation": "ann-bulk-build",
                "stage": "ann-published",
                "status": "completed",
                "completed_units": ann_units,
                "total_units": ann_units,
                "unit": "vectors",
                "checkpoint_digest": checkpoint_digest,
            }));
        self.set_stage("ann-published")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "ann-published",
                status: "running",
                checkpoint_digest: None,
                details: Some(evidence.clone()),
            },
        )
    }

    fn begin_search_seed_lexical(&self) -> std::io::Result<()> {
        self.begin_search_seed_lexical_with_plan(None)
    }

    fn begin_search_seed_lexical_with_plan(
        &self,
        plan: Option<SeedCohortPlan>,
    ) -> std::io::Result<()> {
        self.set_phase(
            "search-seed-lexical",
            "search-seed",
            "lexical-filter-seed",
            0,
            2,
            self.search_documents,
        )?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-lexical",
                status: "running",
                checkpoint_digest: None,
                details: plan.map(|plan| {
                    json!({
                        "cohort_count": plan.cohort_count,
                        "batch_size": plan.batch_size,
                        "partition_rule": plan.partition_rule,
                    })
                }),
            },
        )
    }

    fn advance_search_seed_lexical(&self, units: usize) -> std::io::Result<()> {
        self.advance(u64::try_from(units).map_err(std::io::Error::other)?)
    }

    fn complete_search_seed(&self) -> std::io::Result<()> {
        self.set_stage("search-seed-ready")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-ready",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn begin_search_seed_maintenance(&self) -> std::io::Result<()> {
        self.set_phase(
            "search-seed-maintenance",
            "search-seed",
            "vacuum-checkpoint",
            2,
            3,
            0,
        )?;
        self.phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .heartbeat_while_idle = true;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-maintenance",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn complete_search_seed_maintenance(
        &self,
        vacuum: &hyphae_native_runtime::PageVacuumReceipt,
    ) -> std::io::Result<()> {
        *self.maintenance_evidence.lock().map_err(|_| {
            std::io::Error::other("G7 maintenance evidence synchronization failed")
        })? = Some(json!({
            "vacuum": {
                "applied": vacuum.applied,
                "previous_page_count": vacuum.previous_page_count,
                "active_page_count": vacuum.active_page_count,
                "reclaimed_pages": vacuum.reclaimed_pages,
            },
            "checkpoint": "completed",
            "ann_identity_preserved": true,
        }));
        self.phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .heartbeat_while_idle = false;
        self.set_stage("search-seed-maintenance-completed")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-maintenance-completed",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn begin_search_seed_open(&self) -> std::io::Result<()> {
        self.set_phase(
            "search-seed-open",
            "search-seed",
            "open-and-hydrate",
            3,
            4,
            0,
        )?;
        self.phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .heartbeat_while_idle = true;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-open",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn complete_search_seed_open(&self) -> std::io::Result<()> {
        self.phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .heartbeat_while_idle = false;
        self.set_stage("search-seed-open-completed")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-open-completed",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn finish_search_seed_lexical(&self) -> std::io::Result<()> {
        self.set_stage("search-seed-lexical-completed")?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "search-seed-lexical-completed",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn mark_reused_search_seed(
        &self,
        checkpoint: [u8; 32],
        evidence: &serde_json::Value,
    ) -> std::io::Result<()> {
        self.begin_search_seed_lexical()?;
        self.advance_search_seed_lexical(
            usize::try_from(self.search_documents).map_err(std::io::Error::other)?,
        )?;
        self.finish_search_seed_lexical()?;
        self.set_phase(
            "ann-seed-verify",
            "search-seed",
            "ann-bulk-build",
            1,
            2,
            self.search_documents,
        )?;
        self.complete_ann(checkpoint, evidence)?;
        self.complete_search_seed()
    }

    fn begin_surface(
        self: &Arc<Self>,
        name: &str,
        index: usize,
        total_units: usize,
    ) -> std::io::Result<SurfaceProgress> {
        if G7_SURFACE_NAMES.get(index).copied() != Some(name) {
            return Err(std::io::Error::other(format!(
                "G7 progress surface {name} does not match index {index}"
            )));
        }
        self.set_phase(
            "surface-started",
            "cell-workload",
            name,
            index,
            G7_SURFACES,
            u64::try_from(total_units).map_err(std::io::Error::other)?,
        )?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "surface-started",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )?;
        Ok(SurfaceProgress {
            cell: Arc::clone(self),
            name: name.to_owned(),
            index,
            total: u64::try_from(total_units).map_err(std::io::Error::other)?,
            completed: AtomicU64::new(0),
            phase_base: AtomicU64::new(0),
            phase_total: AtomicU64::new(0),
        })
    }

    fn complete_cell(&self, receipt: &serde_json::Value) -> std::io::Result<()> {
        self.check_failure()?;
        let completed = self.completed.load(Ordering::Relaxed);
        if completed != self.total {
            return Err(std::io::Error::other(format!(
                "G7 cell progress completed {completed} of {} work units",
                self.total
            )));
        }
        let checkpoint = blake3::hash(&serde_json::to_vec(receipt).map_err(std::io::Error::other)?)
            .to_hex()
            .to_string();
        self.set_phase(
            "cell-completed",
            "cell",
            "g7-cell",
            G7_SURFACES,
            G7_SURFACES,
            0,
        )?;
        self.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "cell-completed",
                status: "completed",
                checkpoint_digest: Some(checkpoint),
                details: None,
            },
        )
    }

    fn set_phase(
        &self,
        stage: &str,
        kind: &str,
        name: &str,
        index: usize,
        total: usize,
        phase_total: u64,
    ) -> std::io::Result<()> {
        let completed = self.completed.load(Ordering::Relaxed);
        *self
            .phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))? =
            ProgressPhase {
                stage: stage.to_owned(),
                kind: kind.to_owned(),
                name: name.to_owned(),
                index,
                total,
                base_completed: completed,
                phase_total,
                started: Instant::now(),
                heartbeat_while_idle: false,
            };
        Ok(())
    }

    fn set_stage(&self, stage: &str) -> std::io::Result<()> {
        self.phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .stage = stage.to_owned();
        Ok(())
    }

    fn advance(&self, units: u64) -> std::io::Result<()> {
        let previous = self.completed.fetch_add(units, Ordering::Relaxed);
        if previous.saturating_add(units) > self.total {
            return Err(std::io::Error::other(
                "G7 cell progress completed units exceed total",
            ));
        }
        Ok(())
    }

    fn flush_if_advanced(&self) -> std::io::Result<()> {
        self.write_snapshot(
            false,
            CellProgressUpdate {
                stage: "",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn write_snapshot(&self, force: bool, update: CellProgressUpdate<'_>) -> std::io::Result<()> {
        self.check_failure()?;
        let completed = self.completed.load(Ordering::Relaxed);
        let phase = self
            .phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .clone();
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress sink synchronization failed"))?;
        let Some(sink) = sink.as_mut() else {
            return Ok(());
        };
        if !force && completed == sink.last_written_completed && !phase.heartbeat_while_idle {
            return Ok(());
        }
        let stage = if update.stage.is_empty() {
            phase.stage.as_str()
        } else {
            update.stage
        };
        let mut details = update.details.unwrap_or_else(|| json!({}));
        let phase_completed = completed.saturating_sub(phase.base_completed);
        let eta = progress_eta(
            phase.started.elapsed(),
            phase_completed,
            phase.phase_total,
            completed,
            self.total,
            update.status == "completed",
        )?;
        if let Some(object) = details.as_object_mut() {
            object.insert("eta".to_owned(), eta);
            object.insert(
                "phase".to_owned(),
                json!({
                    "kind": phase.kind,
                    "name": phase.name,
                    "index": phase.index,
                    "total": phase.total,
                    "completed_units": phase_completed.min(phase.phase_total),
                    "total_units": phase.phase_total,
                }),
            );
            if let Some(ann) = self
                .ann_suboperation
                .lock()
                .map_err(|_| std::io::Error::other("G7 ANN progress synchronization failed"))?
                .clone()
            {
                object.insert("suboperation".to_owned(), ann);
            }
            if let Some(evidence) = self
                .initial_ann_bulk
                .lock()
                .map_err(|_| std::io::Error::other("G7 ANN evidence synchronization failed"))?
                .clone()
            {
                object.insert("initial_ann_bulk".to_owned(), evidence);
            }
            if let Some(evidence) = self
                .maintenance_evidence
                .lock()
                .map_err(|_| {
                    std::io::Error::other("G7 maintenance evidence synchronization failed")
                })?
                .clone()
            {
                object.insert("maintenance".to_owned(), evidence);
            }
        }
        sink.write(
            completed,
            self.total,
            CellProgressUpdate {
                stage,
                status: update.status,
                checkpoint_digest: update.checkpoint_digest,
                details: Some(details),
            },
        )
    }

    fn record_failure(&self, error: String) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(error);
        }
    }

    fn check_failure(&self) -> std::io::Result<()> {
        if let Some(error) = self
            .failure
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress failure synchronization failed"))?
            .as_ref()
        {
            return Err(std::io::Error::other(error.clone()));
        }
        Ok(())
    }
}

impl CellProgressSink {
    fn write(
        &mut self,
        completed: u64,
        total: u64,
        update: CellProgressUpdate<'_>,
    ) -> std::io::Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("G7 progress sequence overflow"))?;
        if completed > total {
            return Err(std::io::Error::other(
                "G7 cell progress completed units exceed total",
            ));
        }
        let elapsed_nanos = u64::try_from(self.started.elapsed().as_nanos())
            .map_err(|_| std::io::Error::other("G7 cell progress elapsed time exceeds u64"))?;
        let record = json!({
            "schema": "hyphae-native-performance-progress-v1",
            "source_commit": self.source_commit,
            "source_tree": self.source_tree,
            "dataset_digest": self.dataset_digest,
            "operation": "g7-cell",
            "stage": update.stage,
            "sequence": self.sequence,
            "completed_units": completed,
            "total_units": total,
            "unit": "work-units",
            "elapsed_nanos": elapsed_nanos,
            "status": update.status,
            "checkpoint_digest": update.checkpoint_digest,
            "details": update.details.unwrap_or_else(|| json!({})),
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
        fs::rename(staging, &self.path)?;
        self.last_written_completed = completed;
        Ok(())
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl SurfaceIdlePhase<'_> {
    fn complete(mut self) -> std::io::Result<()> {
        self.progress.finish_idle_phase(&self.phase)?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for SurfaceIdlePhase<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.progress.disable_idle_heartbeat();
        }
    }
}

impl SurfaceProgress {
    fn begin_phase(&self, phase: &str, total_units: usize) -> std::io::Result<()> {
        let completed = self.completed.load(Ordering::Relaxed);
        self.phase_base.store(completed, Ordering::Relaxed);
        self.phase_total.store(
            u64::try_from(total_units).map_err(std::io::Error::other)?,
            Ordering::Relaxed,
        );
        self.cell.set_phase(
            &format!("surface-{phase}"),
            phase,
            &self.name,
            self.index,
            G7_SURFACES,
            u64::try_from(total_units).map_err(std::io::Error::other)?,
        )?;
        self.cell.write_snapshot(
            true,
            CellProgressUpdate {
                stage: &format!("surface-{phase}"),
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn advance(&self, units: usize) -> std::io::Result<()> {
        let units = u64::try_from(units).map_err(std::io::Error::other)?;
        let previous = self.completed.fetch_add(units, Ordering::Relaxed);
        if previous.saturating_add(units) > self.total {
            return Err(std::io::Error::other(format!(
                "G7 surface {} completed units exceed total",
                self.name
            )));
        }
        self.cell.advance(units)
    }

    fn begin_idle_phase(&self, phase: &str) -> std::io::Result<()> {
        self.cell.set_phase(
            &format!("surface-{phase}"),
            phase,
            &self.name,
            self.index,
            G7_SURFACES,
            0,
        )?;
        self.cell
            .phase
            .lock()
            .map_err(|_| std::io::Error::other("G7 progress phase synchronization failed"))?
            .heartbeat_while_idle = true;
        let snapshot = self.cell.write_snapshot(
            true,
            CellProgressUpdate {
                stage: &format!("surface-{phase}"),
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        );
        if snapshot.is_err() {
            self.disable_idle_heartbeat();
        }
        snapshot
    }

    fn idle_phase(&self, phase: &str) -> std::io::Result<SurfaceIdlePhase<'_>> {
        self.begin_idle_phase(phase)?;
        Ok(SurfaceIdlePhase {
            progress: self,
            phase: phase.to_owned(),
            completed: false,
        })
    }

    fn disable_idle_heartbeat(&self) {
        if let Ok(mut phase) = self.cell.phase.lock() {
            phase.heartbeat_while_idle = false;
        }
    }

    fn finish_idle_phase(&self, phase: &str) -> std::io::Result<()> {
        self.disable_idle_heartbeat();
        self.cell.set_stage(&format!("surface-{phase}-completed"))?;
        self.cell.write_snapshot(
            true,
            CellProgressUpdate {
                stage: &format!("surface-{phase}-completed"),
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn finish_phase(&self, phase: &str) -> std::io::Result<()> {
        let phase_completed = self
            .completed
            .load(Ordering::Relaxed)
            .saturating_sub(self.phase_base.load(Ordering::Relaxed));
        let phase_total = self.phase_total.load(Ordering::Relaxed);
        if phase_completed != phase_total {
            return Err(std::io::Error::other(format!(
                "G7 surface {} phase {phase} completed {phase_completed} of {phase_total} units",
                self.name
            )));
        }
        let stage = format!("surface-{phase}-completed");
        self.cell.set_stage(&stage)?;
        self.cell.write_snapshot(
            true,
            CellProgressUpdate {
                stage: &stage,
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }

    fn complete(&self) -> std::io::Result<()> {
        let completed = self.completed.load(Ordering::Relaxed);
        if completed != self.total {
            return Err(std::io::Error::other(format!(
                "G7 surface {} completed {completed} of {} units",
                self.name, self.total
            )));
        }
        self.cell.set_phase(
            "surface-completed",
            "cell-workload",
            &self.name,
            self.index,
            G7_SURFACES,
            0,
        )?;
        self.cell.write_snapshot(
            true,
            CellProgressUpdate {
                stage: "surface-completed",
                status: "running",
                checkpoint_digest: None,
                details: None,
            },
        )
    }
}

impl PartialReceiptSink {
    fn from_environment(
        source_commit: &str,
        platform: &str,
        state: &str,
        concurrency: usize,
    ) -> Result<Self, Box<dyn Error>> {
        let path = std::env::var_os("HYPHAE_G7_PARTIAL_RECEIPT_FILE").map(PathBuf::from);
        let source_tree = path
            .as_ref()
            .map(|_| {
                std::env::var("HYPHAE_G7_SOURCE_TREE")
                    .map_err(|_| "G7 partial receipt requires HYPHAE_G7_SOURCE_TREE")
            })
            .transpose()?;
        Ok(Self {
            path,
            source_commit: source_commit.to_owned(),
            source_tree,
            dataset_digest: dataset_digest(source_commit).to_hex().to_string(),
            platform: platform.to_owned(),
            state: state.to_owned(),
            concurrency,
            sequence: 0,
        })
    }

    fn begin_surface(
        &mut self,
        name: &str,
        cells: &BTreeMap<&str, serde_json::Value>,
    ) -> Result<(), Box<dyn Error>> {
        if !G7_SURFACE_NAMES.contains(&name) || cells.contains_key(name) {
            return Err(format!("invalid G7 partial receipt current surface: {name}").into());
        }
        self.write("running", Some(name), cells)
    }

    fn complete_surface(
        &mut self,
        cells: &BTreeMap<&str, serde_json::Value>,
    ) -> Result<(), Box<dyn Error>> {
        self.write("running", None, cells)
    }

    fn complete(
        &mut self,
        cells: &BTreeMap<&str, serde_json::Value>,
    ) -> Result<(), Box<dyn Error>> {
        if cells.len() != G7_SURFACES
            || G7_SURFACE_NAMES
                .iter()
                .any(|name| !cells.contains_key(name))
        {
            return Err("G7 partial receipt cannot complete before every surface".into());
        }
        self.write("completed", None, cells)
    }

    fn write(
        &mut self,
        status: &str,
        current_cell: Option<&str>,
        cells: &BTreeMap<&str, serde_json::Value>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("G7 partial receipt sequence overflow")?;
        write_json_atomic(
            path,
            &json!({
                "schema": "hyphae-native-g7-partial-receipt-v1",
                "source_commit": self.source_commit,
                "source_tree": self.source_tree,
                "dataset_digest": self.dataset_digest,
                "platform": self.platform,
                "state": self.state,
                "concurrency": self.concurrency,
                "sequence": self.sequence,
                "status": status,
                "completed_count": cells.len(),
                "total_cells": G7_SURFACES,
                "current_cell": current_cell,
                "cells": cells,
            }),
        )
    }
}

fn total_work_units(
    search_documents: usize,
    observations: usize,
    warmup: usize,
    warm: bool,
) -> Result<u64, Box<dyn Error>> {
    let search_seed = u64::try_from(search_documents)?
        .checked_mul(2)
        .ok_or("G7 progress search seed units overflow")?;
    let observations = u64::try_from(observations)?;
    let warmup = if warm { u64::try_from(warmup)? } else { 0 };
    let read_units = observations
        .checked_add(warmup)
        .and_then(|units| units.checked_mul(READ_WORKLOADS))
        .ok_or("G7 progress read units overflow")?;
    search_seed
        .checked_add(read_units)
        .and_then(|units| units.checked_add(observations))
        .ok_or_else(|| "G7 progress total units overflow".into())
}

impl ExecutionAuthority {
    fn from_environment(data_path: &Path) -> Result<Self, Box<dyn Error>> {
        let expected_profile = read_required_json("HYPHAE_G7_HARDWARE_PROFILE_FILE")?;
        let policy: NativeGovernorPolicy =
            serde_json::from_value(read_required_json("HYPHAE_G7_GOVERNOR_POLICY_FILE")?)
                .map_err(|error| format!("invalid G7 governor policy: {error}"))?;
        let calibration: HardwareCalibration =
            serde_json::from_value(read_required_json("HYPHAE_G7_HARDWARE_CALIBRATION_FILE")?)
                .map_err(|error| format!("invalid G7 hardware calibration: {error}"))?;
        let executable_blake3 = current_executable_blake3()?;
        if calibration.identity.executable_blake3 != executable_blake3 {
            return Err("G7 calibration targets another executable, not the exact runner".into());
        }
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
        let topology =
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &calibration)
                .map_err(|error| format!("G7 execution topology derivation failed: {error}"))?;
        let actual_topology = serde_json::to_value(&topology)?;
        if actual_topology != expected_topology {
            return Err("live G7 execution topology differs from the supplied topology".into());
        }
        if topology.worker_count() == 0 {
            return Err("G7 execution topology has no workers".into());
        }
        let topology_digest = blake3::hash(&serde_json::to_vec(&actual_topology)?)
            .to_hex()
            .to_string();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let execution_pool = Arc::new(
            NativeExecutionPool::new_with_calibration(&profile, &policy, &calibration)
                .map_err(|error| format!("G7 execution pool creation failed: {error}"))?,
        );
        if execution_pool.topology() != &topology {
            return Err("G7 shared execution pool differs from canonical topology".into());
        }
        Ok(Self {
            profile,
            calibration,
            policy,
            topology,
            topology_digest,
            executable_blake3,
            installations: Mutex::new(BTreeSet::new()),
            governor,
            execution_pool,
        })
    }

    fn install(&self, database: &mut NativeDatabase, surface: &str) -> Result<(), Box<dyn Error>> {
        self.reinstall(database)?;
        self.record_installation(surface)?;
        Ok(())
    }

    fn reinstall(&self, database: &mut NativeDatabase) -> Result<(), Box<dyn Error>> {
        install_database_execution_authority(
            database,
            Arc::clone(&self.governor),
            Arc::clone(&self.execution_pool),
        )?;
        if database.resource_governor().is_none() || database.execution_pool().is_none() {
            return Err("G7 database did not retain its execution authority".into());
        }
        Ok(())
    }

    fn install_product(
        &self,
        product: &mut NativeProduct,
        surface: &str,
    ) -> Result<(), Box<dyn Error>> {
        install_product_execution_authority(
            product,
            Arc::clone(&self.governor),
            Arc::clone(&self.execution_pool),
        )?;
        if !product.has_execution_authority() {
            return Err("G7 product did not retain its execution authority".into());
        }
        self.record_installation(surface)
    }

    fn record_installation(&self, surface: &str) -> Result<(), Box<dyn Error>> {
        if !self
            .installations
            .lock()
            .map_err(|_| "G7 execution installation registry synchronization failed")?
            .insert(surface.to_owned())
        {
            return Err(format!("G7 execution authority surface repeated: {surface}").into());
        }
        Ok(())
    }

    fn observation(&self) -> Result<serde_json::Value, Box<dyn Error>> {
        let installations = self
            .installations
            .lock()
            .map_err(|_| "G7 execution installation registry synchronization failed")?;
        let local_dispatches = self.execution_pool.local_dispatches();
        let stolen_dispatches = self.execution_pool.stolen_dispatches();
        let completed_jobs = self.execution_pool.completed_jobs();
        Ok(json!({
            "status": "measured",
            "database_queue_wait_millis": G7_DATABASE_QUEUE_WAIT.as_millis(),
            "topology_digest": self.topology_digest,
            "runner_executable_blake3": self.executable_blake3,
            "calibration_executable_blake3": self.calibration.identity.executable_blake3,
            "installations": installations.len(),
            "installed_surfaces": installations.iter().collect::<Vec<_>>(),
            "registered_pools": 1,
            "local_dispatches": local_dispatches,
            "stolen_dispatches": stolen_dispatches,
            "completed_jobs": completed_jobs,
            "numa_steal_status": serde_json::to_value(&self.topology)?["numa_steal_policy"]["status"],
        }))
    }
}

fn install_database_execution_authority(
    database: &mut NativeDatabase,
    governor: Arc<NativeResourceGovernor>,
    execution_pool: Arc<NativeExecutionPool>,
) -> Result<(), Box<dyn Error>> {
    database
        .set_resource_governor_with_execution_pool(
            governor,
            execution_pool,
            G7_DATABASE_QUEUE_WAIT,
        )
        .map_err(|error| format!("G7 execution authority install failed: {error}").into())
}

fn install_product_execution_authority(
    product: &mut NativeProduct,
    governor: Arc<NativeResourceGovernor>,
    execution_pool: Arc<NativeExecutionPool>,
) -> Result<(), Box<dyn Error>> {
    product
        .set_resource_governor_with_execution_pool(
            governor,
            execution_pool,
            G7_DATABASE_QUEUE_WAIT,
        )
        .map_err(Into::into)
}

impl SearchFixture {
    fn open_or_create(
        root: &Path,
        source_commit: &str,
        authority: &ExecutionAuthority,
        progress: &Arc<CellProgress>,
    ) -> Result<Self, Box<dyn Error>> {
        let path = search_seed_path(root, source_commit)?;
        let created = !path.is_dir();
        if !path.is_dir() {
            publish_search_seed(&path, source_commit, authority, progress)?;
        }
        progress.begin_search_seed_open()?;
        let mut database =
            NativeDatabase::open(&path).map_err(|error| format!("search seed open: {error}"))?;
        authority.install(&mut database, "search-fixture")?;
        let lexical_index = ObjectId::new(7)?;
        let vector_index = ObjectId::new(8)?;
        let initial_ann_bulk = load_initial_ann_bulk_evidence(&path, source_commit, authority)?;
        let observed = database.observe_ann_index(vector_index)?;
        let aggregate_identity = required_json_string(&initial_ann_bulk, "aggregate_identity")?;
        let observed_identity = blake3::Hash::from_bytes(observed.base_identity)
            .to_hex()
            .to_string();
        if aggregate_identity != observed_identity {
            return Err("published G7 ANN base differs from its durable build evidence".into());
        }
        progress.complete_search_seed_open()?;
        if !created {
            progress.mark_reused_search_seed(observed.base_identity, &initial_ann_bulk)?;
        }
        progress.begin_search_seed_open()?;
        let fixture = Self::from_database(database, lexical_index, vector_index, initial_ann_bulk)?;
        progress.complete_search_seed_open()?;
        Ok(fixture)
    }

    fn from_database(
        database: NativeDatabase,
        lexical_index: ObjectId,
        vector_index: ObjectId,
        initial_ann_bulk: serde_json::Value,
    ) -> Result<Self, Box<dyn Error>> {
        let query = Vector::new({
            let mut values = vec![0.0; vector_dimension() as usize];
            values[0] = 1.0;
            values
        })?;
        let options = AnnSearchOptions::new(K, ANN_QUERY_BREADTH, Some(K))?;
        let foreground_compute_threads = database
            .resource_governor()
            .ok_or("search fixture has no resource governor")?
            .policy()
            .limit(WorkloadClass::ForegroundBounded)
            .compute_threads;
        let hybrid_request = NativeHybridReadViewOpenRequest {
            lexical: NativeLexicalReadViewOpenRequest {
                index: lexical_index,
                query: "rare",
                limit: K,
                maximum_retained_postings: G7_LEXICAL_RETAINED_POSTINGS,
                maximum_retained_bytes: G7_LEXICAL_RETAINED_BYTES,
            },
            vector_index,
        };
        let (hybrid_view, hybrid_view_open) = database.open_hybrid_read_view(&hybrid_request)?;
        validate_g7_search_fixture_open(
            &hybrid_view_open,
            lexical_index,
            vector_index,
            &initial_ann_bulk,
        )?;
        let filter_request = NativeStructureScalarFilter {
            key_prefix: G7_FILTER_KEY_PREFIX,
            expected_inline_value: G7_FILTER_EXPECTED_VALUE,
            logical_time_micros: 0,
        };
        let lexical_view = hybrid_view.lexical_view();
        let (filtered_lexical_view, filtered_lexical_view_open) = database
            .open_filtered_lexical_read_view_from_lexical(&lexical_view, &filter_request)?;
        validate_g7_filtered_search_fixture_open(
            &filtered_lexical_view_open,
            &hybrid_view_open.lexical,
        )?;
        let ann_view = hybrid_view.ann_view();
        Ok(Self {
            database,
            hybrid_view,
            hybrid_view_open,
            lexical_view,
            filtered_lexical_view,
            filtered_lexical_view_open,
            ann_view,
            foreground_compute_threads,
            query,
            options,
            initial_ann_bulk,
        })
    }
}

fn validate_g7_search_fixture_open(
    open: &NativeHybridReadViewOpenReceipt,
    lexical_index: ObjectId,
    vector_index: ObjectId,
    initial_ann_bulk: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let lexical = &open.lexical;
    let ann = &open.ann;
    let expected_partitions =
        usize::try_from(required_json_u64(initial_ann_bulk, "planned_partitions")?)?;
    if open.snapshot_csn.is_none()
        || open.root_identity != lexical.root_identity
        || open.root_identity != ann.root_identity
        || open.snapshot_csn != lexical.snapshot_csn
        || open.snapshot_csn != ann.snapshot_csn
        || open.catalog_version != lexical.catalog_version
        || open.catalog_version != ann.catalog_version
        || lexical.index_id != lexical_index
        || ann.index_id != vector_index
        || lexical.lexical_plan_scope != NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE
        || lexical.lexical_index_identity_algorithm != NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM
        || lexical.planned_terms != 1
        || lexical.retained_postings != 1
        || lexical.maximum_retained_postings != G7_LEXICAL_RETAINED_POSTINGS
        || lexical.maximum_retained_bytes != G7_LEXICAL_RETAINED_BYTES
        || lexical.planned_physical_entries == 0
        || lexical.planned_physical_bytes == 0
        || lexical.observed_physical_entries == 0
        || lexical.observed_physical_entries > lexical.planned_physical_entries
        || lexical.observed_physical_bytes == 0
        || lexical.observed_physical_bytes > lexical.planned_physical_bytes
        || lexical.admitted_retained_memory_bytes == 0
        || lexical.admitted_retained_memory_bytes > G7_LEXICAL_RETAINED_BYTES
        || lexical.retained_memory_bytes == 0
        || lexical.retained_memory_bytes > lexical.admitted_retained_memory_bytes
        || ann.logical_partitions != expected_partitions
        || ann.hydration_restore_count != 1
        || ann.observed_physical_entries > ann.planned_physical_entries
        || ann.observed_physical_bytes > ann.planned_physical_bytes
        || ann.retained_memory_bytes > ann.planned_peak_memory_bytes
    {
        return Err("G7 search fixture open receipt violates its shared bounded authority".into());
    }
    Ok(())
}

fn validate_g7_filtered_search_fixture_open(
    open: &NativeFilteredLexicalReadViewOpenReceipt,
    shared_lexical: &hyphae_native_runtime::NativeLexicalReadViewOpenReceipt,
) -> Result<(), Box<dyn Error>> {
    if open.snapshot_csn.is_none()
        || open.root_identity != shared_lexical.root_identity
        || open.snapshot_csn != shared_lexical.snapshot_csn
        || open.catalog_version != shared_lexical.catalog_version
        || open.lexical.lexical_index_identity != shared_lexical.lexical_index_identity
        || open.lexical.index_id != shared_lexical.index_id
        || open.lexical.lexical_plan_scope != NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE
        || open.structure_filter_identity_algorithm
            != hyphae_native_runtime::NATIVE_STRUCTURE_FILTER_IDENTITY_ALGORITHM
        || open.structure_filter_value_scope
            != hyphae_native_runtime::NATIVE_STRUCTURE_FILTER_VALUE_SCOPE
        || open.structure_filter_identity == [0; 32]
        || open.retained_filter_records != 1
        || open.planned_filter_physical_entries == 0
        || open.observed_filter_physical_entries != 1
        || open.observed_filter_physical_entries > open.planned_filter_physical_entries
        || open.planned_filter_physical_bytes == 0
        || open.observed_filter_physical_bytes == 0
        || open.observed_filter_physical_bytes > open.planned_filter_physical_bytes
        || open.retained_filter_memory_bytes == 0
        || open.filter_planning.class != WorkloadClass::ForegroundBounded
        || open.filter_planning.request.compute_threads == 0
        || open.filter_planning.request.io_slots == 0
        || open.filter_hydration.class != WorkloadClass::ForegroundBounded
        || open.filter_hydration.request.compute_threads == 0
        || open.filter_hydration.request.io_slots == 0
        || open.filter_hydration.request.memory_bytes < open.retained_filter_memory_bytes
    {
        return Err("G7 filtered lexical fixture violates its same-root bounded authority".into());
    }
    Ok(())
}

fn publish_search_seed(
    path: &Path,
    source_commit: &str,
    authority: &ExecutionAuthority,
    progress: &Arc<CellProgress>,
) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("search seed path has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".search-staging-{}-{}",
        std::process::id(),
        unique_nonce()
    ));
    let evidence = seed_search_database(&staging, source_commit, authority, progress)?;
    publish_search_seed_directory(&staging, path, &evidence)
}

fn publish_search_seed_directory(
    staging: &Path,
    path: &Path,
    evidence: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    if !staging.is_dir() {
        return Err("G7 search seed staging directory is missing".into());
    }
    write_json_atomic(&initial_ann_bulk_evidence_path(staging), evidence)?;
    fs::rename(staging, path)?;
    Ok(())
}

fn seed_search_database(
    path: &Path,
    source_commit: &str,
    authority: &ExecutionAuthority,
    progress: &Arc<CellProgress>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut database =
        NativeDatabase::create(path).map_err(|error| format!("search seed: {error}"))?;
    authority.install(&mut database, "search-seed-builder")?;
    let lexical_index = ObjectId::new(7)?;
    let vector_index = ObjectId::new(8)?;
    let document_count = search_documents();
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
    let logical_partitions = logical_ann_partitions(document_count);
    if logical_partitions > MAX_INITIAL_ANN_BULK_PARTITIONS {
        return Err("G7 logical ANN partition policy exceeds the durable format".into());
    }
    // Seed scalar and lexical state before publishing ANN. Opening any later
    // transaction can cross the vector restoration boundary, so the ANN bulk
    // generation must remain the final mutating seed operation.
    let cohort_plan = SeedCohortPlan::for_database(&database, document_count);
    progress.begin_search_seed_lexical_with_plan(Some(cohort_plan))?;
    seed_lexical_with_cohorts(
        &mut database,
        lexical_index,
        document_count,
        cohort_plan,
        progress,
    )?;
    progress.finish_search_seed_lexical()?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    progress.begin_ann_build(authority, logical_partitions)?;
    let vectors = (0..document_count)
        .map(|id| vector_fixture(id, document_count))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let callback_progress = Arc::clone(progress);
    let plan = database.plan_initial_ann_bulk_with_progress(
        vector_index,
        vectors,
        logical_partitions,
        move |update| {
            if let Err(error) = callback_progress.update_ann_build(update) {
                callback_progress.record_failure(error.to_string());
            }
        },
    )?;
    progress.check_failure()?;
    let build = plan.build_evidence();
    validate_initial_ann_bulk_build(build, authority, logical_partitions)?;
    let evidence = initial_ann_bulk_evidence(source_commit, build, authority)?;
    progress.begin_ann_publication(&evidence)?;
    let published =
        database.publish_initial_ann_bulk(plan, hyphae_native_types::DurabilityClass::Memory)?;
    if published.build != build {
        return Err("published G7 ANN build evidence changed after planning".into());
    }
    let observed = database.observe_ann_index(vector_index)?;
    if observed.base_identity != build.build_identity {
        return Err("published G7 ANN generation differs from its planned aggregate".into());
    }
    progress.complete_ann(observed.base_identity, &evidence)?;
    progress.begin_search_seed_maintenance()?;
    let vacuum = database.vacuum_pages()?;
    black_box(database.checkpoint()?);
    let maintained = database.observe_ann_index(vector_index)?;
    if maintained.base_identity != observed.base_identity {
        return Err("G7 search seed maintenance changed the published ANN identity".into());
    }
    progress.complete_search_seed_maintenance(&vacuum)?;
    progress.complete_search_seed()?;
    drop(database);
    Ok(evidence)
}

fn validate_initial_ann_bulk_build(
    build: InitialAnnBulkBuildEvidence,
    authority: &ExecutionAuthority,
    logical_partitions: usize,
) -> Result<(), Box<dyn Error>> {
    if build.builder != InitialAnnBulkBuilder::PartitionedHnswV1 {
        return Err("G7 initial ANN bulk selected an unexpected builder".into());
    }
    if build.planned_vectors != search_documents()
        || build.planned_partitions != logical_partitions
        || build.planned_compute_threads == 0
        || build.planned_compute_threads as usize > authority.topology.worker_count()
        || build.planned_memory_bytes == 0
        || build.worker_batches == 0
    {
        return Err("G7 initial ANN bulk returned incomplete resource evidence".into());
    }
    if authority.topology.worker_count() > 1
        && logical_partitions > 1
        && build.planned_compute_threads <= 1
    {
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
    phase_elapsed: Duration,
    phase_completed: u64,
    phase_total: u64,
    completed: u64,
    total: u64,
    finished: bool,
) -> std::io::Result<serde_json::Value> {
    if finished {
        return Ok(json!({
            "status": "completed",
            "estimated_remaining_nanos": 0,
        }));
    }
    if completed > total || phase_completed > phase_total {
        return Err(std::io::Error::other("G7 cell progress exceeds total"));
    }
    if completed == total {
        return Ok(json!({
            "status": "estimated",
            "estimated_remaining_nanos": 0,
        }));
    }
    if phase_completed == 0 {
        return Ok(json!({
            "status": "pending",
            "estimated_remaining_nanos": null,
        }));
    }
    let remaining_units = total
        .checked_sub(completed)
        .ok_or_else(|| std::io::Error::other("G7 cell progress exceeds total"))?
        as u128;
    let remaining_nanos = phase_elapsed
        .as_nanos()
        .checked_mul(remaining_units)
        .ok_or_else(|| std::io::Error::other("G7 progress ETA multiplication overflow"))?
        .checked_div(phase_completed as u128)
        .ok_or_else(|| std::io::Error::other("G7 progress ETA divisor must be nonzero"))?;
    let remaining_nanos = u64::try_from(remaining_nanos)
        .map_err(|_| std::io::Error::other("G7 progress ETA exceeds u64"))?;
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
        "partition_policy": ANN_PARTITION_POLICY,
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
    seed_path.join("initial-ann-bulk.json")
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

fn current_executable_blake3() -> Result<String, Box<dyn Error>> {
    let path = std::env::current_exe()?;
    let mut executable = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1_024];
    loop {
        let count = executable.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn run_hardware_calibration(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let data_path = arguments
        .first()
        .map(PathBuf::from)
        .ok_or("G7 runner hardware calibration requires one data path")?;
    let mode = match arguments.get(1).map(String::as_str) {
        Some("quick") => CalibrationMode::Quick,
        Some("thorough") => CalibrationMode::Thorough,
        _ => return Err("G7 runner hardware calibration mode must be quick or thorough".into()),
    };
    let profile = HardwareProfile::discover(&data_path)?;
    let request = CalibrationRequest::for_current_executable(
        mode,
        env!("HYPHAE_RUSTC_IDENTITY"),
        concat!("hyphae-native-g7-runner/", env!("CARGO_PKG_VERSION")),
    )?;
    let calibration = HardwareCalibration::run(&profile, &request)?;
    serde_json::to_writer_pretty(std::io::stdout().lock(), &calibration)?;
    println!();
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct HardwareCalibrationDiagnosticArguments {
    source_commit: String,
    source_tree: String,
    platform: String,
    hardware_profile: PathBuf,
    producer_executable_blake3: String,
    compiler_identity: String,
    hyphae_build_identity: String,
    worker_counts: Vec<usize>,
}

impl HardwareCalibrationDiagnosticArguments {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn Error>> {
        const FIELDS: [&str; 8] = [
            "--source-commit",
            "--source-tree",
            "--platform",
            "--hardware-profile",
            "--producer-executable-blake3",
            "--compiler-identity",
            "--hyphae-build-identity",
            "--worker-counts",
        ];
        if arguments.len() != FIELDS.len() * 2 {
            return Err("hardware calibration diagnostic requires eight named values".into());
        }
        let mut values = BTreeMap::new();
        for pair in arguments.chunks_exact(2) {
            let field = pair[0].as_str();
            if !FIELDS.contains(&field) || values.insert(field, pair[1].clone()).is_some() {
                return Err(format!("unexpected or duplicate diagnostic field {field}").into());
            }
        }
        let mut take = |field: &'static str| -> Result<String, Box<dyn Error>> {
            values
                .remove(field)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("missing diagnostic field {field}").into())
        };
        let source_commit = take("--source-commit")?;
        let source_tree = take("--source-tree")?;
        if !is_canonical_hex_digest(&source_commit, 40)
            || !is_canonical_hex_digest(&source_tree, 40)
        {
            return Err("diagnostic source commit and tree must be lowercase Git objects".into());
        }
        let platform = take("--platform")?;
        let hardware_profile = PathBuf::from(take("--hardware-profile")?);
        let producer_executable_blake3 = take("--producer-executable-blake3")?;
        if !is_canonical_hex_digest(&producer_executable_blake3, 64) {
            return Err("diagnostic producer digest must be lowercase BLAKE3".into());
        }
        let compiler_identity = take("--compiler-identity")?;
        let hyphae_build_identity = take("--hyphae-build-identity")?;
        let worker_counts = take("--worker-counts")?
            .split(',')
            .map(str::parse::<usize>)
            .collect::<Result<Vec<_>, _>>()?;
        if worker_counts.is_empty()
            || worker_counts.contains(&0)
            || !worker_counts.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err("diagnostic worker counts must be positive, unique, and ordered".into());
        }
        Ok(Self {
            source_commit,
            source_tree,
            platform,
            hardware_profile,
            producer_executable_blake3,
            compiler_identity,
            hyphae_build_identity,
            worker_counts,
        })
    }
}

fn is_canonical_hex_digest(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn run_hardware_calibration_diagnostic(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let arguments = HardwareCalibrationDiagnosticArguments::parse(arguments)?;
    if arguments.platform != std::env::consts::OS {
        return Err("diagnostic platform differs from the executing platform".into());
    }
    if arguments.compiler_identity != env!("HYPHAE_RUSTC_IDENTITY") {
        return Err("diagnostic compiler identity differs from the producer build".into());
    }
    let expected_build_identity = concat!("hyphae-native-g7-runner/", env!("CARGO_PKG_VERSION"));
    if arguments.hyphae_build_identity != expected_build_identity {
        return Err("diagnostic Hyphae build identity differs from the producer build".into());
    }
    let executable_blake3 = current_executable_blake3()?;
    if arguments.producer_executable_blake3 != executable_blake3 {
        return Err("diagnostic producer executable digest differs from its bytes".into());
    }
    let profile = HardwareProfile::from_json_slice(&fs::read(&arguments.hardware_profile)?)?;
    let diagnostic = ThreadScalingDiagnostic::run(&profile, &arguments.worker_counts)?;
    let receipt = json!({
        "schema": "hyphae-native-hardware-calibration-diagnostic-v1",
        "authority": false,
        "evidence_class": "diagnostic-only",
        "claims": [],
        "closure_declared": false,
        "source": {
            "commit": arguments.source_commit,
            "tree": arguments.source_tree,
        },
        "platform": arguments.platform,
        "identity": {
            "hardware_fingerprint": profile.fingerprint,
            "producer_executable_blake3": executable_blake3,
            "compiler_identity": arguments.compiler_identity,
            "hyphae_build_identity": arguments.hyphae_build_identity,
        },
        "policy": diagnostic.policy,
        "surface": {
            "primitive": "thread-scaling-memory-scan",
            "binding": diagnostic.binding,
            "worker_points": diagnostic.worker_points,
        },
    });
    serde_json::to_writer_pretty(std::io::stdout().lock(), &receipt)?;
    println!();
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let allocation_start = GLOBAL_ALLOCATOR.stats();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--hardware-calibrate") {
        return run_hardware_calibration(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("--hardware-calibration-diagnostic") {
        return run_hardware_calibration_diagnostic(&arguments[1..]);
    }
    let source_commit = arguments
        .first()
        .ok_or("missing exact source commit")?
        .clone();
    let platform = arguments
        .get(1)
        .cloned()
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let state = arguments
        .get(2)
        .cloned()
        .unwrap_or_else(|| "warm".to_owned());
    let concurrency = arguments
        .get(3)
        .cloned()
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
    let authority = Arc::new(ExecutionAuthority::from_environment(&search_seed_path(
        &root,
        &source_commit,
    )?)?);
    let progress =
        CellProgress::from_environment(&source_commit, observations, warmup, state == "warm")?;
    let _progress_reporter = progress.start_reporter();
    let search = SearchFixture::open_or_create(&root, &source_commit, &authority, &progress)?;
    let mut partial_receipt =
        PartialReceiptSink::from_environment(&source_commit, &platform, &state, concurrency)?;
    let background_enabled =
        std::env::var("HYPHAE_G7_BACKGROUND").is_ok_and(|value| value == "1" || value == "true");
    let background_stop = Arc::new(AtomicBool::new(false));
    let background_thread = background_enabled.then(|| {
        let stop = Arc::clone(&background_stop);
        let authority = Arc::clone(&authority);
        let path = root.join("background-maintenance");
        thread::spawn(move || -> Result<u64, String> {
            let mut database = NativeDatabase::create(path).map_err(|error| error.to_string())?;
            authority
                .install(&mut database, "background-maintenance")
                .map_err(|error| error.to_string())?;
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
    let read_surface_units = observations
        .checked_add(if state == "warm" { warmup } else { 0 })
        .ok_or("G7 read surface progress units overflow")?;
    if state == "cold" {
        // Cold state is a fresh process and fresh data directory for this
        // receipt. Warm state retains the process-local seeded handles.
        fs::create_dir_all(root.join("cold-marker"))?;
    }
    let surface = progress.begin_surface("embedded-structure-point-get", 0, read_surface_units)?;
    partial_receipt.begin_surface("embedded-structure-point-get", &cells)?;
    let value = run_embedded_structure(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("embedded structure: {error}"))?;
    surface.complete()?;
    cells.insert("embedded-structure-point-get", value);
    partial_receipt.complete_surface(&cells)?;

    let surface =
        progress.begin_surface("embedded-prepared-sql-primary-key", 1, read_surface_units)?;
    partial_receipt.begin_surface("embedded-prepared-sql-primary-key", &cells)?;
    let value = run_embedded_sql(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("embedded sql: {error}"))?;
    surface.complete()?;
    cells.insert("embedded-prepared-sql-primary-key", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("local-structure-point-get", 2, read_surface_units)?;
    partial_receipt.begin_surface("local-structure-point-get", &cells)?;
    let value = run_local_structure(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .await
    .map_err(|error| format!("local structure: {error}"))?;
    surface.complete()?;
    cells.insert("local-structure-point-get", value);
    partial_receipt.complete_surface(&cells)?;

    let surface =
        progress.begin_surface("local-prepared-sql-primary-key", 3, read_surface_units)?;
    partial_receipt.begin_surface("local-prepared-sql-primary-key", &cells)?;
    let value = run_local_sql(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .await
    .map_err(|error| format!("local sql: {error}"))?;
    surface.complete()?;
    cells.insert("local-prepared-sql-primary-key", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("indexed-sql-bounded-read", 4, read_surface_units)?;
    partial_receipt.begin_surface("indexed-sql-bounded-read", &cells)?;
    let value = run_indexed_sql(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("indexed sql: {error}"))?;
    surface.complete()?;
    cells.insert("indexed-sql-bounded-read", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("two-index-join-bounded-read", 5, read_surface_units)?;
    partial_receipt.begin_surface("two-index-join-bounded-read", &cells)?;
    let value = run_join_sql(
        &root,
        &authority,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("two-index join: {error}"))?;
    surface.complete()?;
    cells.insert("two-index-join-bounded-read", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("bm25-top10", 6, read_surface_units)?;
    partial_receipt.begin_surface("bm25-top10", &cells)?;
    let value = run_bm25(
        &search,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("bm25: {error}"))?;
    surface.complete()?;
    cells.insert("bm25-top10", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("filtered-bm25-top10", 7, read_surface_units)?;
    partial_receipt.begin_surface("filtered-bm25-top10", &cells)?;
    let value = run_filtered_bm25(
        &search,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("filtered bm25: {error}"))?;
    surface.complete()?;
    cells.insert("filtered-bm25-top10", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("ann-top10-recall-095", 8, read_surface_units)?;
    partial_receipt.begin_surface("ann-top10-recall-095", &cells)?;
    let value = run_ann(
        &search,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("ann: {error}"))?;
    surface.complete()?;
    cells.insert("ann-top10-recall-095", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("hybrid-top10", 9, read_surface_units)?;
    partial_receipt.begin_surface("hybrid-top10", &cells)?;
    let value = run_hybrid(
        &search,
        state == "warm",
        concurrency,
        observations,
        warmup,
        &surface,
    )
    .map_err(|error| format!("hybrid: {error}"))?;
    surface.complete()?;
    cells.insert("hybrid-top10", value);
    partial_receipt.complete_surface(&cells)?;

    let surface = progress.begin_surface("strict-group-commit", 10, observations)?;
    partial_receipt.begin_surface("strict-group-commit", &cells)?;
    let value = run_commit(&root, &authority, concurrency, observations, &surface)?;
    surface.complete()?;
    cells.insert("strict-group-commit", value);
    partial_receipt.complete_surface(&cells)?;
    receipt["cells"] = serde_json::to_value(&cells)?;
    receipt["physical_observation"] = physical_observation(&root, &authority)?;
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
    receipt["execution_authority"] = authority.observation()?;
    finalize_cell(&progress, &mut partial_receipt, &cells, &root, &receipt)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn finalize_cell(
    progress: &CellProgress,
    partial_receipt: &mut PartialReceiptSink,
    cells: &BTreeMap<&str, serde_json::Value>,
    root: &Path,
    receipt: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    partial_receipt.complete(cells)?;
    fs::remove_dir_all(root)?;
    progress.complete_cell(receipt)?;
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

fn logical_ann_partitions(vector_count: usize) -> usize {
    CLOSURE_ANN_LOGICAL_PARTITIONS.min(vector_count).max(1)
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
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("structure");
    let mut database =
        NativeDatabase::create(&path).map_err(|error| format!("structure seed: {error}"))?;
    authority.install(&mut database, "embedded-structure")?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    for index in 0..STRUCTURE_KEYS {
        seed.set(index.to_be_bytes().to_vec(), vec![0xa5; 64], None)?;
    }
    seed.commit()?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    let target = (STRUCTURE_KEYS / 2).to_be_bytes();
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(database.get_latest_structure(&target, 0)?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let target_value = [0xa5; 64];
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let value = database.get_latest_structure(&target, 0)?;
        if value.as_deref() != Some(target_value.as_slice()) {
            return Err("structure result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    progress.finish_phase("measure")?;
    stats_with_materialization(stats, materialization)
}

fn run_embedded_sql(
    root: &Path,
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("sql");
    let mut database = NativeDatabase::create(&path)
        .map_err(|error| format!("sql seed {}: {error}", path.display()))?;
    authority.install(&mut database, "embedded-sql")?;
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
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("prepared SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    progress.finish_phase("measure")?;
    stats_with_materialization(stats, materialization)
}

fn local_clients(endpoint: &Path, concurrency: usize) -> Result<Vec<HyphaeClient>, Box<dyn Error>> {
    if concurrency == 0 {
        return Err("local benchmark requires at least one independent client".into());
    }
    let endpoint = endpoint.to_string_lossy().into_owned();
    (0..concurrency)
        .map(|_| HyphaeClient::local(endpoint.clone()).map_err(Into::into))
        .collect()
}

#[derive(Clone)]
struct LocalSqlSession {
    client: HyphaeClient,
    handle: ProductPreparedHandle,
}

async fn prepare_local_sql_sessions(
    clients: Vec<HyphaeClient>,
    options: &RequestOptions,
) -> Result<Vec<LocalSqlSession>, Box<dyn Error>> {
    let mut sessions = Vec::with_capacity(clients.len());
    for client in clients {
        let prepared = client
            .prepare_sql(
                "SELECT id, payload FROM g7_items WHERE id = ?",
                options.clone(),
            )
            .await?;
        let ProductResponse::PreparedSql { handle, .. } = prepared else {
            return Err("local SQL prepare returned an unexpected response".into());
        };
        sessions.push(LocalSqlSession { client, handle });
    }
    Ok(sessions)
}

async fn run_local_structure(
    root: &Path,
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-structure");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local structure seed: {error}"))?;
    authority.install_product(&mut product, "local-structure-seed")?;
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
    authority.install(&mut database, "local-structure-migration")?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    drop(database);
    let mut product = NativeProduct::open(&path)
        .map_err(|error| format!("local structure reopen after migration: {error}"))?;
    authority.install_product(&mut product, "local-structure-daemon")?;
    let endpoint = short_endpoint("structure");
    let daemon = LocalDaemonThread::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let clients = local_clients(&endpoint, concurrency)?;
        let options = RequestOptions::default();
        let materialization = NativeDatabase::process_materialization_observation();
        if warm {
            progress.begin_phase("warmup", warmup)?;
            warm_async(
                concurrency,
                warmup,
                progress,
                |worker| {
                    let client = clients[worker].clone();
                    let options = options.clone();
                    async move {
                        Ok::<_, Box<dyn Error>>(
                            client
                                .structure_get(b"g7-local-structure".to_vec(), options)
                                .await?,
                        )
                    }
                },
                &require_structure_response,
            )
            .await?;
            progress.finish_phase("warmup")?;
        }
        progress.begin_phase("measure", observations)?;
        let stats = measure_async(
            concurrency,
            observations,
            progress,
            |worker| {
                let client = clients[worker].clone();
                let options = options.clone();
                async move {
                    Ok::<_, Box<dyn Error>>(
                        client
                            .structure_get(b"g7-local-structure".to_vec(), options)
                            .await?,
                    )
                }
            },
            &require_structure_response,
        )
        .await?;
        progress.finish_phase("measure")?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    daemon.shutdown()?;
    result
}

async fn run_local_sql(
    root: &Path,
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("local-sql");
    let mut product =
        NativeProduct::create(&path).map_err(|error| format!("local sql seed: {error}"))?;
    authority.install_product(&mut product, "local-sql-daemon")?;
    seed_product_sql(&mut product)?;
    let endpoint = short_endpoint("sql");
    let daemon = LocalDaemonThread::start(
        product,
        endpoint.to_string_lossy().into_owned(),
        NativeDaemonConfig::default(),
    )?;
    let result = async {
        let clients = local_clients(&endpoint, concurrency)?;
        let options = RequestOptions::default();
        let sessions = prepare_local_sql_sessions(clients, &options).await?;
        let materialization = NativeDatabase::process_materialization_observation();
        if warm {
            progress.begin_phase("warmup", warmup)?;
            warm_async(
                concurrency,
                warmup,
                progress,
                |worker| {
                    let session = sessions[worker].clone();
                    let options = options.clone();
                    async move {
                        Ok::<_, Box<dyn Error>>(
                            session
                                .client
                                .execute_prepared(
                                    session.handle,
                                    vec![hyphae_native_product::ProductValue::Signed(
                                        (SQL_KEYS / 2) as i64,
                                    )],
                                    options,
                                )
                                .await?,
                        )
                    }
                },
                &require_sql_response,
            )
            .await?;
            progress.finish_phase("warmup")?;
        }
        progress.begin_phase("measure", observations)?;
        let stats = measure_async(
            concurrency,
            observations,
            progress,
            |worker| {
                let session = sessions[worker].clone();
                let options = options.clone();
                async move {
                    Ok::<_, Box<dyn Error>>(
                        session
                            .client
                            .execute_prepared(
                                session.handle,
                                vec![hyphae_native_product::ProductValue::Signed(
                                    (SQL_KEYS / 2) as i64,
                                )],
                                options,
                            )
                            .await?,
                    )
                }
            },
            &require_sql_response,
        )
        .await?;
        progress.finish_phase("measure")?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    daemon.shutdown()?;
    result
}

fn run_indexed_sql(
    root: &Path,
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let path = root.join("indexed");
    let mut database = NativeDatabase::create(&path)?;
    authority.install(&mut database, "indexed-sql")?;
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
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("indexed SQL result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    progress.finish_phase("measure")?;
    let mut value = stats_with_materialization(stats, materialization)?;
    value["route"] = json!("native-indexed-sql");
    value["concurrency"] = json!(concurrency);
    Ok(value)
}

fn run_join_sql(
    root: &Path,
    authority: &ExecutionAuthority,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = root.join("join");
    let mut database = NativeDatabase::create(&path)?;
    authority.install(&mut database, "join-sql")?;
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
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(database.execute_prepared_latest(&prepared, &parameters)?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let result = database.execute_prepared_latest(&prepared, &parameters)?;
        if !matches!(result, hyphae_native_runtime::SqlResult::Rows { rows, .. } if rows.len() == 1)
        {
            return Err("two-index join result mismatch".into());
        }
        Ok(())
    })?;
    progress.finish_phase("measure")?;
    stats_with_materialization(stats, materialization)
}

fn run_filtered_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    let physical_before = fixture.database.physical_observation()?;
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            let receipt = fixture.filtered_lexical_view.search()?;
            validate_g7_filtered_lexical_query(&receipt, &fixture.filtered_lexical_view_open)?;
            black_box(receipt);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let (stats, filtered) = measure_concurrent_with_evidence::<_, FilteredLexicalIntervalEvidence>(
        concurrency,
        observations,
        progress,
        &|| {
            fixture
                .filtered_lexical_view
                .search()
                .map_err(|error| -> Box<dyn Error> { error.into() })
        },
        &|receipt| {
            validate_g7_filtered_lexical_query(&receipt, &fixture.filtered_lexical_view_open)?;
            let observation = FilteredLexicalObservation {
                execution_sequence: receipt.execution_sequence,
                postings_scored: u64::try_from(receipt.postings_scored)?,
                filter_records_evaluated: u64::try_from(receipt.filter_records_evaluated)?,
                filter_records_matched: u64::try_from(receipt.filter_records_matched)?,
                physical_page_reads: receipt.physical_page_reads,
            };
            black_box(receipt);
            Ok(observation)
        },
    )?;
    progress.finish_phase("measure")?;
    let physical_after = fixture.database.physical_observation()?;
    let interval_page_reads = physical_after
        .physical_page_reads
        .saturating_sub(physical_before.physical_page_reads);
    let mut value = stats_with_materialization(stats, materialization)?;
    let materialization = value
        .get("materialization")
        .and_then(serde_json::Value::as_object)
        .ok_or("filtered BM25 materialization interval is missing")?;
    let full_state_loads = materialization
        .get("full_state_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("filtered BM25 full-state counter is missing")?;
    let full_catalog_loads = materialization
        .get("full_catalog_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("filtered BM25 full-catalog counter is missing")?;
    value["route"] = json!("native-root-bound-filter-before-rank");
    value["correctness_scope"] = json!("lexical-and-structure-one-root-query-bound");
    value["corpus_filter_density"] = json!(0.5);
    value["candidate_filter_selectivity"] = json!(1.0);
    value["concurrency"] = json!(concurrency);
    value["filtered_lexical_read_view_open"] =
        filtered_lexical_read_view_open_json(&fixture.filtered_lexical_view_open)?;
    value["filtered_lexical_read_view_query_interval"] = filtered.json(
        observations,
        interval_page_reads,
        full_state_loads,
        full_catalog_loads,
    )?;
    Ok(value)
}

fn run_hybrid(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let offered_concurrency = u64::try_from(concurrency)?;
    let preferred_partitions = G7_PREFERRED_ANN_PARTITIONS
        .min(fixture.hybrid_view_open.ann.logical_partitions)
        .max(1);
    let query_workers = fixture
        .foreground_compute_threads
        .checked_div(offered_concurrency.max(1))
        .unwrap_or(0)
        .max(1)
        .min(u64::try_from(preferred_partitions)?);
    let query_queue_wait = Duration::from_secs(60);
    let query = NativeHybridReadViewQuery {
        vector_query: &fixture.query,
        ann_options: fixture.options,
        maximum_partitions: preferred_partitions,
        fusion: NativeHybridFusion {
            lexical_weight: 1,
            vector_weight: 1,
            limit: K,
        },
    };
    let physical_before = fixture.database.physical_observation()?;
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            let receipt = fixture.hybrid_view.search_selected_with_worker_budget(
                &query,
                query_workers,
                query_queue_wait,
            )?;
            validate_g7_hybrid_query(
                &receipt,
                &fixture.hybrid_view_open,
                preferred_partitions,
                query_workers,
            )?;
            black_box(receipt);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let (stats, evidence) = measure_concurrent_with_evidence::<_, HybridIntervalEvidence>(
        concurrency,
        observations,
        progress,
        &|| {
            fixture
                .hybrid_view
                .search_selected_with_worker_budget(&query, query_workers, query_queue_wait)
                .map_err(|error| -> Box<dyn Error> { error.into() })
        },
        &|receipt| {
            let (next_lower_bound, kth_distance) = validate_g7_hybrid_query(
                &receipt,
                &fixture.hybrid_view_open,
                preferred_partitions,
                query_workers,
            )?;
            let routing = RoutingIntervalEvidence::observation(
                &receipt.ann.search,
                next_lower_bound,
                kth_distance,
            )?;
            let observation = HybridObservation {
                routing,
                peak_admission_compute_threads: receipt.peak_admission.request.compute_threads,
                peak_admission_memory_bytes: receipt.peak_admission.request.memory_bytes,
                result_retention_memory_bytes: receipt.result_retention.request.memory_bytes,
            };
            black_box(receipt);
            Ok(observation)
        },
    )?;
    progress.finish_phase("measure")?;
    let physical_after = fixture.database.physical_observation()?;
    let restores_after = NativeDatabase::process_ann_index_restore_count();
    let interval_page_reads = physical_after
        .physical_page_reads
        .saturating_sub(physical_before.physical_page_reads);
    let interval_restores = restores_after.saturating_sub(restores_before);
    if interval_page_reads != 0 || interval_restores != 0 {
        return Err("hybrid read-view interval crossed the hydration boundary".into());
    }
    let mut value = stats_with_materialization(stats, materialization)?;
    let materialization_interval = value
        .get("materialization")
        .and_then(serde_json::Value::as_object)
        .ok_or("hybrid materialization interval is missing")?;
    let full_state_loads = materialization_interval
        .get("full_state_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("hybrid full-state counter is missing")?;
    let full_catalog_loads = materialization_interval
        .get("full_catalog_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("hybrid full-catalog counter is missing")?;
    if full_state_loads != 0 || full_catalog_loads != 0 {
        return Err("hybrid read view materialized complete state".into());
    }
    let lexical_open = &fixture.hybrid_view_open.lexical;
    let ann_open = &fixture.hybrid_view_open.ann;
    let snapshot_csn = fixture
        .hybrid_view_open
        .snapshot_csn
        .map(hyphae_native_types::Csn::get)
        .ok_or("hybrid read view omitted its snapshot CSN")?;
    value["route"] = json!("native-same-snapshot-hybrid");
    value["concurrency"] = json!(concurrency);
    value["per_query_worker_limit"] = json!(query_workers);
    value["preferred_partition_budget"] = json!(preferred_partitions);
    value["query_queue_wait_millis"] = json!(query_queue_wait.as_millis());
    value["hybrid_read_view_open"] = json!({
        "root_identity": hex_digest(fixture.hybrid_view_open.root_identity),
        "snapshot_csn": snapshot_csn,
        "lexical_index_identity": hex_digest(lexical_open.lexical_index_identity),
        "ann_view_identity": hex_digest(ann_open.view_identity),
        "lexical_plan_scope": lexical_open.lexical_plan_scope,
        "planned_physical_entries": lexical_open.planned_physical_entries,
        "planned_physical_bytes": lexical_open.planned_physical_bytes,
        "observed_physical_entries": lexical_open.observed_physical_entries,
        "observed_physical_bytes": lexical_open.observed_physical_bytes,
        "admitted_retained_memory_bytes": lexical_open.admitted_retained_memory_bytes,
        "retained_memory_bytes": lexical_open.retained_memory_bytes,
    });
    let expected_observations = u64::try_from(observations)?;
    if evidence.peak_admission_executions != expected_observations
        || evidence.peak_admission_compute_threads != query_workers
        || evidence.peak_admission_memory_bytes_min == u64::MAX
        || evidence.peak_admission_memory_bytes_min == 0
        || evidence.peak_admission_memory_bytes_min > evidence.peak_admission_memory_bytes_max
        || evidence.peak_admission_memory_bytes_min < evidence.result_retention_memory_bytes_max
        || evidence.result_retention_executions != expected_observations
        || evidence.fusion_executions != expected_observations
        || evidence.result_retention_memory_bytes_min == u64::MAX
        || evidence.result_retention_memory_bytes_min == 0
        || evidence.result_retention_memory_bytes_min > evidence.result_retention_memory_bytes_max
    {
        return Err("hybrid fusion interval omitted bounded governor evidence".into());
    }
    value["hybrid_read_view_query_interval"] = json!({
        "observations": observations,
        "hydrations": 0,
        "physical_page_reads": interval_page_reads,
        "index_scoped_restores": interval_restores,
        "full_state_loads": full_state_loads,
        "full_catalog_loads": full_catalog_loads,
        "lexical_execution": hyphae_native_runtime::NATIVE_LEXICAL_READ_VIEW_EXECUTION,
        "peak_admission_executions": evidence.peak_admission_executions,
        "peak_admission_class": "foreground-bounded",
        "peak_admission_compute_threads": evidence.peak_admission_compute_threads,
        "peak_admission_io_slots": 0,
        "peak_admission_memory_bytes_min": evidence.peak_admission_memory_bytes_min,
        "peak_admission_memory_bytes_max": evidence.peak_admission_memory_bytes_max,
        "result_retention_executions": evidence.result_retention_executions,
        "result_retention_class": "foreground-bounded",
        "result_retention_compute_threads": 0,
        "result_retention_io_slots": 0,
        "result_retention_memory_bytes_min": evidence.result_retention_memory_bytes_min,
        "result_retention_memory_bytes_max": evidence.result_retention_memory_bytes_max,
        "fusion_executions": evidence.fusion_executions,
        "fusion_class": "foreground-bounded",
        "fusion_compute_threads": 1,
        "fusion_io_slots": 0,
        "fusion_memory_bytes": 0,
        "provider": "hybrid-read-view-interval-counters-v1",
    });
    value["hybrid_ann_routing_interval"] = evidence.routing.json(observations)?;
    value["hybrid_oracle"] = hybrid_oracle(
        fixture,
        &query,
        query_workers,
        query_queue_wait,
        preferred_partitions,
    )?;
    Ok(value)
}

fn filter_key(document_id: &[u8; 16]) -> Vec<u8> {
    let mut key = b"g7-filter:".to_vec();
    key.extend_from_slice(document_id);
    key
}

fn validate_g7_filtered_lexical_query(
    receipt: &hyphae_native_runtime::NativeFilteredLexicalReadViewQueryReceipt,
    open: &NativeFilteredLexicalReadViewOpenReceipt,
) -> Result<(), Box<dyn Error>> {
    if receipt.filter_execution != hyphae_native_runtime::NATIVE_STRUCTURE_FILTER_EXECUTION
        || receipt.root_identity != open.root_identity
        || receipt.snapshot_csn != open.snapshot_csn
        || receipt.catalog_version != open.catalog_version
        || receipt.lexical_index_identity != open.lexical.lexical_index_identity
        || receipt.structure_filter_identity != open.structure_filter_identity
        || receipt.postings_scored != 1
        || receipt.filter_records_evaluated != 1
        || receipt.filter_records_matched != 1
        || receipt.hits.len() != 1
        || receipt.execution.class != WorkloadClass::ForegroundBounded
        || receipt.execution.request.compute_threads != 1
        || receipt.execution.request.io_slots != 0
        || receipt.execution.request.memory_bytes == 0
        || receipt.physical_page_reads != 0
    {
        return Err("G7 filtered BM25 query changed its same-root predicate authority".into());
    }
    Ok(())
}

fn filtered_lexical_read_view_open_json(
    open: &NativeFilteredLexicalReadViewOpenReceipt,
) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(json!({
        "root_identity": hex_digest(open.root_identity),
        "snapshot_csn": open.snapshot_csn
            .map(hyphae_native_types::Csn::get)
            .ok_or("filtered lexical read view omitted its snapshot CSN")?,
        "lexical_index_identity": hex_digest(open.lexical.lexical_index_identity),
        "lexical_plan_scope": open.lexical.lexical_plan_scope,
        "structure_filter_identity_algorithm": open.structure_filter_identity_algorithm,
        "structure_filter_value_scope": open.structure_filter_value_scope,
        "structure_filter_identity": hex_digest(open.structure_filter_identity),
        "retained_filter_records": open.retained_filter_records,
        "planned_filter_physical_entries": open.planned_filter_physical_entries,
        "planned_filter_physical_bytes": open.planned_filter_physical_bytes,
        "observed_filter_physical_entries": open.observed_filter_physical_entries,
        "observed_filter_physical_bytes": open.observed_filter_physical_bytes,
        "retained_filter_memory_bytes": open.retained_filter_memory_bytes,
        "filter_planning": engine_work_receipt_json(&open.filter_planning)?,
        "filter_hydration": engine_work_receipt_json(&open.filter_hydration)?,
        "open_filter_physical_page_reads": open.physical_page_reads,
    }))
}

fn engine_work_receipt_json(
    receipt: &hyphae_native_runtime::NativeEngineWorkReceipt,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if receipt.class != WorkloadClass::ForegroundBounded
        || receipt.request.compute_threads == 0
        || receipt.request.memory_bytes == 0
    {
        return Err("G7 engine work receipt omitted its bounded governor authority".into());
    }
    Ok(json!({
        "class": "foreground-bounded",
        "compute_threads": receipt.request.compute_threads,
        "io_slots": receipt.request.io_slots,
        "memory_bytes": receipt.request.memory_bytes,
        "queue_ticket": receipt.queue_ticket,
        "initial_queue_depth": receipt.initial_queue_depth,
        "queue_time_nanos": u64::try_from(receipt.queue_time.as_nanos())?,
        "execution_time_nanos": u64::try_from(receipt.execution_time.as_nanos())?,
    }))
}

fn validate_g7_hybrid_query(
    receipt: &hyphae_native_runtime::NativeHybridReadViewQueryReceipt,
    open: &NativeHybridReadViewOpenReceipt,
    preferred_partitions: usize,
    worker_limit: u64,
) -> Result<(f64, f64), Box<dyn Error>> {
    if receipt.root_identity != open.root_identity
        || receipt.snapshot_csn != open.snapshot_csn
        || receipt.catalog_version != open.catalog_version
        || receipt.peak_admission.class != WorkloadClass::ForegroundBounded
        || receipt.peak_admission.request.compute_threads != worker_limit
        || receipt.peak_admission.request.io_slots != 0
        || receipt.peak_admission.request.memory_bytes == 0
        || receipt.result_retention.class != WorkloadClass::ForegroundBounded
        || receipt.result_retention.request.compute_threads != 0
        || receipt.result_retention.request.io_slots != 0
        || receipt.result_retention.request.memory_bytes == 0
        || receipt.fusion.class != WorkloadClass::ForegroundBounded
        || receipt.fusion.request.compute_threads != 1
        || receipt.fusion.request.io_slots != 0
        || receipt.fusion.request.memory_bytes != 0
        || receipt.peak_admission.request.memory_bytes
            < receipt
                .result_retention
                .request
                .memory_bytes
                .checked_add(
                    receipt
                        .lexical
                        .execution
                        .request
                        .memory_bytes
                        .max(receipt.ann.execution.request.memory_bytes),
                )
                .ok_or("hybrid peak admission memory overflow")?
        || receipt.peak_admission.execution_time < receipt.lexical.execution.execution_time
        || receipt.peak_admission.execution_time < receipt.ann.execution.execution_time
        || receipt.peak_admission.execution_time < receipt.fusion.execution_time
        || receipt.peak_admission.execution_time < receipt.result_retention.execution_time
        || receipt.result_retention.execution_time < receipt.lexical.execution.execution_time
        || receipt.result_retention.execution_time < receipt.ann.execution.execution_time
        || receipt.result_retention.execution_time < receipt.fusion.execution_time
    {
        return Err("hybrid query crossed or changed its shared read-view authority".into());
    }
    validate_g7_lexical_query(&receipt.lexical, &open.lexical)?;
    let routing = validate_g7_ann_query_authority(
        &receipt.ann,
        &open.ann,
        preferred_partitions,
        worker_limit,
    )?;
    match &receipt.outcome {
        NativeHybridOutcome::Matches(matches) if matches.len() == K => Ok(routing),
        _ => Err("hybrid query did not return the complete fused top-k".into()),
    }
}

fn validate_g7_lexical_query(
    receipt: &hyphae_native_runtime::NativeLexicalReadViewQueryReceipt,
    open: &hyphae_native_runtime::NativeLexicalReadViewOpenReceipt,
) -> Result<(), Box<dyn Error>> {
    if receipt.lexical_execution != hyphae_native_runtime::NATIVE_LEXICAL_READ_VIEW_EXECUTION
        || receipt.lexical_index_identity != open.lexical_index_identity
        || receipt.root_identity != open.root_identity
        || receipt.snapshot_csn != open.snapshot_csn
        || receipt.catalog_version != open.catalog_version
        || receipt.execution_sequence == 0
        || receipt.postings_evaluated != open.retained_postings
        || receipt.hits.is_empty()
        || receipt.hits.len() > K
        || receipt.execution.class != WorkloadClass::ForegroundBounded
        || receipt.execution.request.compute_threads == 0
        || receipt.execution.request.io_slots != 0
        || receipt.execution.request.memory_bytes == 0
        || receipt.physical_page_reads != 0
    {
        return Err("G7 lexical query changed or exceeded its retained read-view authority".into());
    }
    Ok(())
}

fn validate_g7_ann_query_authority(
    receipt: &hyphae_native_runtime::NativeAnnReadViewQueryReceipt,
    open: &hyphae_native_runtime::NativeAnnReadViewOpenReceipt,
    preferred_partitions: usize,
    worker_limit: u64,
) -> Result<(f64, f64), Box<dyn Error>> {
    if receipt.root_identity != open.root_identity
        || receipt.view_identity != open.view_identity
        || receipt.governor_policy_identity != open.governor_policy_identity
        || receipt.governor_generation != open.governor_generation
        || receipt.requested_worker_limit != worker_limit
        || receipt.query_scratch_bytes == 0
        || receipt.execution.class != WorkloadClass::ForegroundBounded
        || receipt.execution.request.compute_threads == 0
        || receipt.execution.request.compute_threads > worker_limit
        || receipt.execution.request.io_slots != 0
        || receipt.execution.request.memory_bytes != receipt.query_scratch_bytes
        || receipt.hydration_performed
        || receipt.physical_page_reads != 0
        || receipt.restore_count != 0
        || receipt.search.base_build_identity != open.base_build_identity
        || receipt.search.view_identity != open.view_identity
        || receipt.search.search.index_id != open.index_id
        || receipt.search.search.snapshot_csn != open.snapshot_csn
        || receipt.search.search.build_identity != open.view_identity
    {
        return Err("G7 ANN query changed its retained read-view authority".into());
    }
    validate_g7_ann_selected_route(
        &receipt.search,
        preferred_partitions,
        open.logical_partitions,
        worker_limit,
    )
}

fn hybrid_oracle(
    fixture: &SearchFixture,
    query: &NativeHybridReadViewQuery<'_>,
    query_workers: u64,
    query_queue_wait: Duration,
    preferred_partitions: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let lexical = fixture.lexical_view.search()?;
    if lexical.root_identity != fixture.hybrid_view_open.root_identity
        || lexical.snapshot_csn != fixture.hybrid_view_open.snapshot_csn
        || lexical.lexical_index_identity != fixture.hybrid_view_open.lexical.lexical_index_identity
        || lexical.physical_page_reads != 0
    {
        return Err("hybrid oracle lexical branch changed authority".into());
    }
    let vector = fixture.ann_view.search_selected_with_worker_budget(
        &fixture.query,
        fixture.options,
        preferred_partitions,
        query_workers,
        query_queue_wait,
    )?;
    validate_g7_ann_query_authority(
        &vector,
        &fixture.hybrid_view_open.ann,
        preferred_partitions,
        query_workers,
    )?;
    if vector.root_identity != fixture.hybrid_view_open.root_identity
        || vector.view_identity != fixture.hybrid_view_open.ann.view_identity
        || vector.hydration_performed
        || vector.physical_page_reads != 0
        || vector.restore_count != 0
    {
        return Err("hybrid oracle vector branch changed authority".into());
    }
    let lexical_ranking = lexical
        .hits
        .iter()
        .map(|hit| canonical_lexical_object_id(&hit.document_id))
        .collect::<Result<Vec<_>, _>>()?;
    let vector_ranking = vector
        .search
        .search
        .hits
        .iter()
        .map(|hit| canonical_object_id(hit.object_id))
        .collect::<Vec<_>>();
    let expected = fuse_oracle_rankings(&lexical_ranking, &vector_ranking)?;
    let native = fixture.hybrid_view.search_selected_with_worker_budget(
        query,
        query_workers,
        query_queue_wait,
    )?;
    validate_g7_hybrid_query(
        &native,
        &fixture.hybrid_view_open,
        preferred_partitions,
        query_workers,
    )?;
    if native_hybrid_results(&native.outcome)? != expected {
        return Err("native hybrid fusion differs from the independent RRF oracle".into());
    }
    let canonical = serde_json::to_vec(&expected)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &canonical);
    let digest = hex_bytes(digest.as_ref());
    let snapshot_csn = fixture
        .hybrid_view_open
        .snapshot_csn
        .map(hyphae_native_types::Csn::get)
        .ok_or("hybrid oracle omitted its snapshot CSN")?;
    Ok(json!({
        "status": "passed",
        "method": "independent-branch-rrf-v1",
        "root_identity": hex_digest(fixture.hybrid_view_open.root_identity),
        "snapshot_csn": snapshot_csn,
        "rrf_constant": 60,
        "contribution_scale": 1_000_000_000_u64,
        "lexical_weight": 1,
        "vector_weight": 1,
        "result_limit": K,
        "tie_break": "fusion-score-desc-object-id-asc",
        "lexical_ranking": lexical_ranking,
        "vector_ranking": vector_ranking,
        "fused_results": expected,
        "result_digest": digest,
        "oracle_digest": digest,
    }))
}

fn fuse_oracle_rankings(
    lexical: &[String],
    vector: &[String],
) -> Result<Vec<BTreeMap<String, serde_json::Value>>, Box<dyn Error>> {
    let lexical_ranks = lexical
        .iter()
        .enumerate()
        .map(|(index, object_id)| Ok((object_id.clone(), u64::try_from(index)? + 1)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    let vector_ranks = vector
        .iter()
        .enumerate()
        .map(|(index, object_id)| Ok((object_id.clone(), u64::try_from(index)? + 1)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    if lexical_ranks.len() != lexical.len() || vector_ranks.len() != vector.len() {
        return Err("hybrid oracle branch repeated one object identity".into());
    }
    let identities = lexical_ranks
        .keys()
        .chain(vector_ranks.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut results = identities
        .into_iter()
        .map(|object_id| {
            let lexical_rank = lexical_ranks.get(&object_id).copied();
            let vector_rank = vector_ranks.get(&object_id).copied();
            oracle_result(object_id, lexical_rank, vector_rank, 0)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    results.sort_by(|left, right| {
        oracle_u64(right, "fusion_score")
            .cmp(&oracle_u64(left, "fusion_score"))
            .then_with(|| oracle_string(left, "object_id").cmp(oracle_string(right, "object_id")))
    });
    results.truncate(K);
    for (index, result) in results.iter_mut().enumerate() {
        result.insert("final_rank".to_owned(), json!(u64::try_from(index)? + 1));
    }
    Ok(results)
}

fn native_hybrid_results(
    outcome: &NativeHybridOutcome,
) -> Result<Vec<BTreeMap<String, serde_json::Value>>, Box<dyn Error>> {
    let NativeHybridOutcome::Matches(matches) = outcome else {
        return Err("native hybrid oracle abstained".into());
    };
    matches
        .iter()
        .map(|matched| {
            Ok(BTreeMap::from([
                (
                    "object_id".to_owned(),
                    json!(canonical_object_id(matched.object_id)),
                ),
                (
                    "lexical_rank".to_owned(),
                    json!(matched.explanation.lexical_rank),
                ),
                (
                    "vector_rank".to_owned(),
                    json!(matched.explanation.vector_rank),
                ),
                (
                    "lexical_contribution".to_owned(),
                    json!(matched.explanation.lexical_contribution),
                ),
                (
                    "vector_contribution".to_owned(),
                    json!(matched.explanation.vector_contribution),
                ),
                (
                    "fusion_score".to_owned(),
                    json!(matched.explanation.fusion_score),
                ),
                (
                    "final_rank".to_owned(),
                    json!(matched.explanation.final_rank),
                ),
            ]))
        })
        .collect()
}

fn oracle_result(
    object_id: String,
    lexical_rank: Option<u64>,
    vector_rank: Option<u64>,
    final_rank: u64,
) -> Result<BTreeMap<String, serde_json::Value>, Box<dyn Error>> {
    const SCALE: u64 = 1_000_000_000;
    let contribution = |rank: Option<u64>| -> Result<u64, Box<dyn Error>> {
        rank.map_or(Ok(0), |rank| {
            60_u64
                .checked_add(rank)
                .and_then(|denominator| SCALE.checked_div(denominator))
                .ok_or_else(|| "hybrid oracle contribution overflow".into())
        })
    };
    let lexical_contribution = contribution(lexical_rank)?;
    let vector_contribution = contribution(vector_rank)?;
    let fusion_score = lexical_contribution
        .checked_add(vector_contribution)
        .ok_or("hybrid oracle fusion overflow")?;
    Ok(BTreeMap::from([
        ("object_id".to_owned(), json!(object_id)),
        ("lexical_rank".to_owned(), json!(lexical_rank)),
        ("vector_rank".to_owned(), json!(vector_rank)),
        (
            "lexical_contribution".to_owned(),
            json!(lexical_contribution),
        ),
        ("vector_contribution".to_owned(), json!(vector_contribution)),
        ("fusion_score".to_owned(), json!(fusion_score)),
        ("final_rank".to_owned(), json!(final_rank)),
    ]))
}

fn oracle_u64(value: &BTreeMap<String, serde_json::Value>, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn oracle_string<'value>(
    value: &'value BTreeMap<String, serde_json::Value>,
    key: &str,
) -> &'value str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn canonical_lexical_object_id(document_id: &[u8]) -> Result<String, Box<dyn Error>> {
    let bytes: [u8; 16] = document_id
        .try_into()
        .map_err(|_| "hybrid lexical identity is not 16 bytes")?;
    canonical_nonzero_object_id(u128::from_be_bytes(bytes))
}

fn canonical_object_id(object_id: ObjectId) -> String {
    format!("{:032x}", object_id.get())
}

fn canonical_nonzero_object_id(value: u128) -> Result<String, Box<dyn Error>> {
    if value == 0 {
        return Err("hybrid object identity is zero".into());
    }
    Ok(format!("{value:032x}"))
}

fn hex_digest(digest: [u8; 32]) -> String {
    hex_bytes(&digest)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
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

async fn warm_async<F, Fut, T>(
    concurrency: usize,
    observations: usize,
    progress: &SurfaceProgress,
    mut operation: F,
    validate: &(impl Fn(T) -> Result<(), Box<dyn Error>> + Sync),
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn Error>>>,
{
    if concurrency == 0 || observations < concurrency {
        return Err("async benchmark requires at least one observation per client".into());
    }
    let mut completed = 0;
    let rounds = observations.div_ceil(concurrency.max(1));
    for _ in 0..rounds {
        let active = concurrency.min(observations - completed);
        let results = join_all((0..active).map(&mut operation)).await;
        for result in results {
            validate(result?)?;
            progress.advance(1)?;
            completed += 1;
        }
    }
    Ok(())
}

async fn timed_async_operation<Fut, T>(future: Fut) -> Result<(u64, T), Box<dyn Error>>
where
    Fut: std::future::Future<Output = Result<T, Box<dyn Error>>>,
{
    let started = Instant::now();
    let output = future.await?;
    Ok((started.elapsed().as_nanos() as u64, output))
}

async fn measure_async<F, Fut, T>(
    concurrency: usize,
    observations: usize,
    progress: &SurfaceProgress,
    mut operation: F,
    validate: &(impl Fn(T) -> Result<(), Box<dyn Error>> + Sync),
) -> Result<Stats, Box<dyn Error>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn Error>>>,
{
    if concurrency == 0 || observations < concurrency {
        return Err("async benchmark requires at least one observation per client".into());
    }
    let mut samples = Vec::with_capacity(observations);
    let mut query_elapsed = Duration::ZERO;
    let rounds = observations.div_ceil(concurrency);
    for _ in 0..rounds {
        let active = concurrency.min(observations - samples.len());
        let round_started = Instant::now();
        let results =
            join_all((0..active).map(|worker| timed_async_operation(operation(worker)))).await;
        query_elapsed += round_started.elapsed();
        for result in results {
            let (elapsed, output) = result?;
            validate(output)?;
            samples.push(elapsed);
        }
        progress.advance(active)?;
    }
    Ok(stats_from_samples(samples, query_elapsed.as_secs_f64()))
}

fn run_bm25(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    let physical_before = fixture.database.physical_observation()?;
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            let receipt = fixture.lexical_view.search()?;
            validate_g7_lexical_query(&receipt, &fixture.hybrid_view_open.lexical)?;
            black_box(receipt);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let (stats, lexical) = measure_concurrent_with_evidence::<_, LexicalIntervalEvidence>(
        concurrency,
        observations,
        progress,
        &|| {
            fixture
                .lexical_view
                .search()
                .map_err(|error| -> Box<dyn Error> { error.into() })
        },
        &|receipt| {
            validate_g7_lexical_query(&receipt, &fixture.hybrid_view_open.lexical)?;
            let observation = LexicalObservation {
                execution_sequence: receipt.execution_sequence,
                postings_evaluated: u64::try_from(receipt.postings_evaluated)?,
                physical_page_reads: receipt.physical_page_reads,
            };
            black_box(receipt);
            Ok(observation)
        },
    )?;
    progress.finish_phase("measure")?;
    let physical_after = fixture.database.physical_observation()?;
    let interval_page_reads = physical_after
        .physical_page_reads
        .saturating_sub(physical_before.physical_page_reads);
    let mut value = stats_with_materialization(stats, materialization)?;
    let materialization = value
        .get("materialization")
        .and_then(serde_json::Value::as_object)
        .ok_or("BM25 materialization interval is missing")?;
    let full_state_loads = materialization
        .get("full_state_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("BM25 full-state counter is missing")?;
    let full_catalog_loads = materialization
        .get("full_catalog_loads")
        .and_then(serde_json::Value::as_u64)
        .ok_or("BM25 full-catalog counter is missing")?;
    value["route"] = json!("native-retained-lexical-read-view");
    value["lexical_read_view_open"] =
        lexical_read_view_open_json(&fixture.hybrid_view_open.lexical)?;
    value["lexical_read_view_query_interval"] = lexical.json(
        observations,
        interval_page_reads,
        full_state_loads,
        full_catalog_loads,
    )?;
    Ok(value)
}

fn lexical_read_view_open_json(
    open: &hyphae_native_runtime::NativeLexicalReadViewOpenReceipt,
) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(json!({
        "root_identity": hex_digest(open.root_identity),
        "snapshot_csn": open.snapshot_csn
            .map(hyphae_native_types::Csn::get)
            .ok_or("lexical read view omitted its snapshot CSN")?,
        "lexical_index_identity_algorithm": open.lexical_index_identity_algorithm,
        "lexical_index_identity": hex_digest(open.lexical_index_identity),
        "lexical_plan_scope": open.lexical_plan_scope,
        "index_id": canonical_object_id(open.index_id),
        "planned_terms": open.planned_terms,
        "retained_postings": open.retained_postings,
        "maximum_retained_postings": open.maximum_retained_postings,
        "maximum_retained_bytes": open.maximum_retained_bytes,
        "planned_physical_entries": open.planned_physical_entries,
        "planned_physical_bytes": open.planned_physical_bytes,
        "observed_physical_entries": open.observed_physical_entries,
        "observed_physical_bytes": open.observed_physical_bytes,
        "admitted_retained_memory_bytes": open.admitted_retained_memory_bytes,
        "retained_memory_bytes": open.retained_memory_bytes,
        "open_physical_page_reads": open.physical_page_reads,
    }))
}

fn run_ann(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let view = &fixture.ann_view;
    let ann_view_open = &fixture.hybrid_view_open.ann;
    let offered_concurrency = u64::try_from(concurrency).map_err(|_| "invalid ANN concurrency")?;
    let preferred_partitions = G7_PREFERRED_ANN_PARTITIONS
        .min(ann_view_open.logical_partitions)
        .max(1);
    let query_workers = fixture
        .foreground_compute_threads
        .checked_div(offered_concurrency.max(1))
        .unwrap_or(0)
        .max(1)
        .min(u64::try_from(preferred_partitions)?);
    let query_queue_wait = Duration::from_secs(60);
    let materialization = NativeDatabase::process_materialization_observation();
    let physical_before = fixture.database.physical_observation()?;
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            let receipt = view.search_selected_with_worker_budget(
                &fixture.query,
                fixture.options,
                preferred_partitions,
                query_workers,
                query_queue_wait,
            )?;
            validate_g7_ann_query_authority(
                &receipt,
                ann_view_open,
                preferred_partitions,
                query_workers,
            )?;
            black_box(receipt);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let (stats, routing) = measure_concurrent_with_evidence::<_, RoutingIntervalEvidence>(
        concurrency,
        observations,
        progress,
        &|| {
            view.search_selected_with_worker_budget(
                &fixture.query,
                fixture.options,
                preferred_partitions,
                query_workers,
                query_queue_wait,
            )
            .map_err(|error| -> Box<dyn Error> { error.into() })
        },
        &|receipt| {
            let (next_lower_bound, kth_distance) = validate_g7_ann_query_authority(
                &receipt,
                ann_view_open,
                preferred_partitions,
                query_workers,
            )?;
            let observation = RoutingIntervalEvidence::observation(
                &receipt.search,
                next_lower_bound,
                kth_distance,
            )?;
            black_box(receipt);
            Ok(observation)
        },
    )?;
    progress.finish_phase("measure")?;
    let physical_after = fixture.database.physical_observation()?;
    let restores_after = NativeDatabase::process_ann_index_restore_count();
    let interval_page_reads = physical_after
        .physical_page_reads
        .saturating_sub(physical_before.physical_page_reads);
    let interval_restores = restores_after.saturating_sub(restores_before);
    if interval_page_reads != 0 || interval_restores != 0 {
        return Err("ANN read-view interval crossed the hydration boundary".into());
    }
    // Keep correctness outside the measured interval so a cold cell's first
    // ANN search is one of its observations rather than fixture validation.
    let correctness = view.search_selected_with_worker_budget(
        &fixture.query,
        fixture.options,
        preferred_partitions,
        query_workers,
        query_queue_wait,
    )?;
    validate_g7_ann_query_authority(
        &correctness,
        ann_view_open,
        preferred_partitions,
        query_workers,
    )?;
    let expected_ids = (1..=K)
        .map(|id| ObjectId::new(id as u128))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let recalled = correctness
        .search
        .search
        .hits
        .iter()
        .filter(|hit| expected_ids.contains(&hit.object_id))
        .count();
    if recalled * 20 < K * 19 {
        return Err("ANN recall below G7 floor".into());
    }
    let mut output = stats_with_materialization(stats, materialization)?;
    output["recall_at_10"] = json!(recalled as f64 / K as f64);
    output["per_query_worker_limit"] = json!(query_workers);
    output["query_queue_wait_millis"] = json!(query_queue_wait.as_millis());
    output["preferred_partition_budget"] = json!(preferred_partitions);
    output["post_open_hydration_performed"] = json!(correctness.hydration_performed);
    output["post_open_physical_page_reads"] = json!(correctness.physical_page_reads);
    output["post_open_restore_count"] = json!(correctness.restore_count);
    output["ann_read_view_query_interval"] = json!({
        "physical_page_reads": interval_page_reads,
        "index_scoped_restores": interval_restores,
        "provider": "database-page-counter-plus-process-ann-restore-counter",
    });
    output["ann_routing_interval"] = routing.json(observations)?;
    output["ann_read_view_open"] = json!({
        "root_identity": blake3::Hash::from_bytes(ann_view_open.root_identity)
            .to_hex()
            .to_string(),
        "snapshot_csn": ann_view_open.snapshot_csn
            .map(hyphae_native_types::Csn::get)
            .ok_or("ANN read view omitted its snapshot CSN")?,
        "base_build_identity": blake3::Hash::from_bytes(ann_view_open.base_build_identity)
        .to_hex()
        .to_string(),
        "view_identity": blake3::Hash::from_bytes(ann_view_open.view_identity)
            .to_hex()
            .to_string(),
        "logical_partitions": ann_view_open.logical_partitions,
        "planned_physical_entries": ann_view_open.planned_physical_entries,
        "planned_physical_bytes": ann_view_open.planned_physical_bytes,
        "observed_physical_entries": ann_view_open.observed_physical_entries,
        "observed_physical_bytes": ann_view_open.observed_physical_bytes,
        "planned_peak_memory_bytes": ann_view_open.planned_peak_memory_bytes,
        "retained_memory_bytes": ann_view_open.retained_memory_bytes,
        "hydration_restore_count": ann_view_open.hydration_restore_count,
        "process_physical_page_read_delta": ann_view_open.process_physical_page_read_delta,
        "governor_generation": ann_view_open.governor_generation,
        "routing_policy_identity": blake3::Hash::from_bytes(
            ann_view_open.routing_policy_identity,
        )
        .to_hex()
        .to_string(),
    });
    Ok(output)
}

fn validate_g7_ann_selected_route(
    receipt: &hyphae_native_runtime::AnnSelectedSearchReceipt,
    preferred_partitions: usize,
    total_partitions: usize,
    worker_limit: u64,
) -> Result<(f64, f64), Box<dyn Error>> {
    let single_route_batches = receipt
        .targeted_single_batches
        .checked_add(receipt.generic_single_fallback_batches)
        .ok_or("G7 ANN route batch evidence overflowed")?;
    let next_lower_bound = receipt
        .next_partition_lower_bound
        .ok_or("G7 ANN route omitted its next-partition lower bound")?;
    let kth_distance = receipt
        .search
        .hits
        .last()
        .filter(|_| receipt.search.hits.len() == K)
        .ok_or("G7 ANN route did not return the complete top-k")?
        .distance;
    if receipt.requested_maximum_partitions != preferred_partitions
        || receipt.total_partitions != total_partitions
        || receipt.routing_mode
            != hyphae_native_runtime::AnnPartitionRoutingMode::SelectedPartitions
        || receipt.routing_outcome != AnnPartitionRoutingOutcome::SelectedCertified
        || receipt.routing_policy != ANN_PARTITION_ROUTING_POLICY_V1
        || receipt.base_build_identity == [0; 32]
        || receipt.view_identity == [0; 32]
        || receipt.selected_partitions.is_empty()
        || receipt.selected_partitions.len() > preferred_partitions
        || receipt.selected_partitions.len() >= total_partitions
        || receipt
            .selected_partitions
            .iter()
            .any(|partition| *partition >= total_partitions)
        || receipt
            .selected_partitions
            .iter()
            .enumerate()
            .any(|(index, partition)| receipt.selected_partitions[..index].contains(partition))
        || receipt.execution_workers == 0
        || u64::try_from(receipt.execution_workers)? > worker_limit
        || receipt.execution_worker_batches == 0
        || receipt.execution_worker_batches > preferred_partitions
        || !(1..=6).contains(&receipt.execution_waves)
        || single_route_batches == 0
        || single_route_batches > receipt.execution_worker_batches
        || single_route_batches > receipt.execution_waves
        || !next_lower_bound.is_finite()
        || !kth_distance.is_finite()
        || next_lower_bound < 0.0
        || kth_distance < 0.0
        || next_lower_bound <= kth_distance
    {
        return Err("G7 ANN route was not selected-certified within the preferred budget".into());
    }
    Ok((next_lower_bound, kth_distance))
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
    authority: &ExecutionAuthority,
    concurrency: usize,
    observations: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if !matches!(concurrency, 1 | 8 | 32) {
        return Err("strict group commit requires producer concurrency 1, 8, or 32".into());
    }
    if observations < STRICT_GROUP_COMMIT_COHORT_WIDTH {
        return Err("strict group commit requires at least one complete cohort".into());
    }
    let plan = StrictGroupCommitPlan::new(observations)?;
    fs::create_dir_all(root)?;
    let group_path = root.join("group");
    let mut database =
        NativeDatabase::create(&group_path).map_err(|error| format!("group seed: {error}"))?;
    let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
    seed.set(b"g7-group-seed".to_vec(), b"v".to_vec(), None)?;
    seed.commit()?;
    database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
    drop(database);

    let mut database = NativeDatabase::open(&group_path)?;
    let baseline_visible_csn = database
        .recovery_report()
        .visible_csn
        .map(hyphae_native_types::Csn::get)
        .ok_or("strict group commit baseline reopen omitted its visible CSN")?;
    let baseline_committed_transactions = database.recovery_report().committed_transactions;
    authority.install(&mut database, "group-commit")?;
    let materialization = NativeDatabase::process_materialization_observation();
    let config = hyphae_native_runtime::GroupCommitConfig::new(
        STRICT_GROUP_COMMIT_COHORT_WIDTH,
        STRICT_GROUP_COMMIT_COLLECTION_WAIT,
        STRICT_GROUP_COMMIT_QUEUE_CAPACITY,
    )?
    .with_execution_admission_wait(STRICT_GROUP_COMMIT_EXECUTION_WAIT)?;
    let scheduler = NativeCommitScheduler::start(database, config)?;
    let producer_clients = (0..concurrency)
        .map(|_| scheduler.client())
        .collect::<Vec<_>>();
    let cohort_client = scheduler.client();
    progress.begin_phase("measure", observations)?;
    let (evidence, measured_wall) = measure_strict_group_commits(
        cohort_client,
        producer_clients,
        plan.clone(),
        baseline_visible_csn,
        progress,
    )?;
    progress.finish_phase("measure")?;
    let stats = evidence.stats(measured_wall.as_secs_f64())?;
    let mut output = stats_with_materialization(stats, materialization)?;
    let mut database = scheduler.shutdown_into_database()?;
    let last_commit_csn = evidence
        .last_commit_csn
        .ok_or("strict group commit omitted its last CSN")?;
    let maintenance =
        perform_strict_group_commit_maintenance(&mut database, last_commit_csn, progress)?;
    drop(database);
    let reopen_phase = progress.idle_phase("strict-reopen")?;
    let reopen = verify_strict_group_commit_reopen(
        &group_path,
        authority,
        &plan,
        baseline_visible_csn,
        baseline_committed_transactions,
    )?;
    reopen_phase.complete()?;
    reopen.validate(
        &plan,
        evidence
            .first_commit_csn
            .ok_or("strict group commit omitted its first CSN")?,
        evidence
            .last_commit_csn
            .ok_or("strict group commit omitted its last CSN")?,
        maintenance.maintenance_csn,
    )?;
    output["durability"] = json!("group-physical-sync");
    output["group_commit_evidence"] = evidence.json(concurrency, &maintenance, &reopen)?;
    Ok(output)
}

fn perform_strict_group_commit_maintenance(
    database: &mut NativeDatabase,
    last_commit_csn: u64,
    progress: &SurfaceProgress,
) -> Result<StrictGroupCommitMaintenanceEvidence, Box<dyn Error>> {
    let maintenance_started = Instant::now();
    let vacuum_phase = progress.idle_phase("strict-maintenance-vacuum")?;
    let vacuum_started = Instant::now();
    let vacuum = database.vacuum_pages()?;
    let vacuum_time_nanos = duration_nanos(vacuum_started.elapsed())?;
    vacuum_phase.complete()?;
    let vacuum_commit = vacuum
        .commit
        .as_ref()
        .filter(|_| vacuum.applied)
        .ok_or("strict group commit maintenance vacuum did not compact the current root")?;
    let maintenance_csn = vacuum_commit.commit_csn.get();
    if maintenance_csn != last_commit_csn.saturating_add(1)
        || vacuum.active_generation.get() != vacuum.previous_generation.get().saturating_add(1)
        || vacuum.active_page_count >= vacuum.previous_page_count
        || vacuum.reclaimed_pages != vacuum.previous_page_count - vacuum.active_page_count
        || vacuum_commit.durability != hyphae_native_types::DurabilityClass::Strict
    {
        return Err("strict group commit maintenance vacuum authority is inconsistent".into());
    }

    let checkpoint_phase = progress.idle_phase("strict-maintenance-checkpoint")?;
    let checkpoint_started = Instant::now();
    let checkpoint = database.checkpoint()?;
    let checkpoint_time_nanos = duration_nanos(checkpoint_started.elapsed())?;
    checkpoint_phase.complete()?;
    if checkpoint.visible_csn.get() != maintenance_csn
        || checkpoint.transaction_id.get() <= vacuum_commit.transaction_id.get()
        || checkpoint.checkpoint_lsn.get() <= vacuum_commit.commit_lsn.get()
    {
        return Err("strict group commit maintenance checkpoint authority is inconsistent".into());
    }

    let retention_phase = progress.idle_phase("strict-maintenance-retention")?;
    let retention_started = Instant::now();
    let retention = database.truncate_wal_at_retention_checkpoint()?;
    let retention_time_nanos = duration_nanos(retention_started.elapsed())?;
    retention_phase.complete()?;
    if retention.base_visible_csn.get() != maintenance_csn
        || retention.checkpoint_lsn != checkpoint.checkpoint_lsn
        || retention.retired_wal_blocks == 0
        || retention.retired_wal_bytes == 0
        || retention.retained_manifest_files != 1
        || retention.retained_manifest_bytes == 0
    {
        return Err("strict group commit WAL retention authority is inconsistent".into());
    }
    let total_time_nanos = duration_nanos(maintenance_started.elapsed())?;
    let stage_time_nanos = vacuum_time_nanos
        .checked_add(checkpoint_time_nanos)
        .and_then(|value| value.checked_add(retention_time_nanos))
        .ok_or("strict group commit maintenance timing overflowed")?;
    if total_time_nanos < stage_time_nanos {
        return Err("strict group commit maintenance timing is inconsistent".into());
    }
    let digest = |bytes: [u8; 32]| blake3::Hash::from_bytes(bytes).to_hex().to_string();
    Ok(StrictGroupCommitMaintenanceEvidence {
        maintenance_csn,
        total_time_nanos,
        value: json!({
            "provider": "vacuum-checkpoint-wal-retention-v1",
            "total_time_nanos": total_time_nanos,
            "vacuum": {
                "applied": vacuum.applied,
                "previous_page_generation": vacuum.previous_generation.get(),
                "active_page_generation": vacuum.active_generation.get(),
                "previous_page_count": vacuum.previous_page_count,
                "active_page_count": vacuum.active_page_count,
                "reclaimed_pages": vacuum.reclaimed_pages,
                "commit_csn": maintenance_csn,
                "commit_transaction_id": vacuum_commit.transaction_id.get(),
                "commit_lsn": vacuum_commit.commit_lsn.get(),
                "wal_block_digest": digest(vacuum_commit.wal_block_digest),
                "commit_durability": "strict",
                "duration_nanos": vacuum_time_nanos,
            },
            "checkpoint": {
                "visible_csn": checkpoint.visible_csn.get(),
                "transaction_id": checkpoint.transaction_id.get(),
                "manifest_generation": checkpoint.manifest_generation.get(),
                "manifest_digest": digest(checkpoint.manifest_digest),
                "checkpoint_lsn": checkpoint.checkpoint_lsn.get(),
                "parent_directory_sync_supported": checkpoint.parent_directory_sync_supported,
                "duration_nanos": checkpoint_time_nanos,
            },
            "wal_retention": {
                "base_visible_csn": retention.base_visible_csn.get(),
                "anchor_epoch": retention.anchor_epoch,
                "anchor_digest": digest(retention.anchor_digest),
                "retired_wal_blocks": retention.retired_wal_blocks,
                "retired_wal_bytes": retention.retired_wal_bytes,
                "checkpoint_lsn": retention.checkpoint_lsn.get(),
                "retired_manifest_files": retention.retired_manifest_files,
                "retired_manifest_bytes": retention.retired_manifest_bytes,
                "retained_manifest_files": retention.retained_manifest_files,
                "retained_manifest_bytes": retention.retained_manifest_bytes,
                "parent_directory_sync_supported": retention.parent_directory_sync_supported,
                "manifest_directory_sync_supported": retention.manifest_directory_sync_supported,
                "duration_nanos": retention_time_nanos,
            },
        }),
    })
}

enum CommitProducerCommand {
    Prepare {
        assignments: Vec<(usize, usize)>,
        active_gate: Arc<Barrier>,
    },
    Stop,
}

type PreparedCommitResult = Result<Vec<(usize, NativeCommitBatch)>, String>;

fn run_commit_producer(
    client: NativeCommitClient,
    commands: mpsc::Receiver<CommitProducerCommand>,
    prepared: mpsc::Sender<PreparedCommitResult>,
    activity: Arc<StrictProducerActivity>,
) {
    while let Ok(command) = commands.recv() {
        let CommitProducerCommand::Prepare {
            assignments,
            active_gate,
        } = command
        else {
            break;
        };
        let _activity_guard = if assignments.is_empty() {
            None
        } else {
            let guard = activity.enter();
            active_gate.wait();
            Some(guard)
        };
        let result = assignments
            .into_iter()
            .map(|(position, sequence)| {
                let mut batch = client
                    .begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Group)
                    .map_err(|error| error.to_string())?;
                client
                    .stage_delta_set(
                        &mut batch,
                        strict_group_commit_key(sequence),
                        strict_group_commit_value(sequence),
                        None,
                    )
                    .map_err(|error| error.to_string())?;
                let batch = client
                    .retain_cohort_batch(batch)
                    .map_err(|error| error.to_string())?;
                Ok((position, batch))
            })
            .collect();
        if prepared.send(result).is_err() {
            break;
        }
    }
}

fn measure_strict_group_commits(
    cohort_client: NativeCommitClient,
    producer_clients: Vec<NativeCommitClient>,
    plan: StrictGroupCommitPlan,
    baseline_visible_csn: u64,
    progress: &SurfaceProgress,
) -> Result<(StrictGroupCommitEvidence, Duration), Box<dyn Error>> {
    let producer_concurrency = producer_clients.len();
    if producer_concurrency == 0 || producer_concurrency > STRICT_GROUP_COMMIT_COHORT_WIDTH {
        return Err("strict group commit producer count is outside its cohort width".into());
    }
    thread::scope(|scope| {
        let (prepared_send, prepared_receive) = mpsc::channel();
        let producer_activity = Arc::new(StrictProducerActivity::default());
        let mut commands = Vec::with_capacity(producer_concurrency);
        let mut workers = Vec::with_capacity(producer_concurrency);
        for client in producer_clients {
            let (command_send, command_receive) = mpsc::sync_channel(1);
            commands.push(command_send);
            let prepared_send = prepared_send.clone();
            let producer_activity = Arc::clone(&producer_activity);
            workers.push(scope.spawn(move || {
                run_commit_producer(client, command_receive, prepared_send, producer_activity);
            }));
        }
        drop(prepared_send);

        let measured = (|| -> Result<(StrictGroupCommitEvidence, Duration), Box<dyn Error>> {
            let mut evidence = StrictGroupCommitEvidence::new(plan.clone(), baseline_visible_csn);
            let mut measured_wall = Duration::ZERO;
            let mut pending_progress = 0_usize;
            for cohort_index in 0..plan.cohort_count {
                let window_timer = StrictGroupCommitWindowTimer::start();
                let sequence_start = cohort_index
                    .checked_mul(STRICT_GROUP_COMMIT_COHORT_WIDTH)
                    .ok_or("strict group commit sequence start overflowed")?;
                let cohort_size = if cohort_index + 1 == plan.cohort_count {
                    plan.final_cohort_size
                } else {
                    STRICT_GROUP_COMMIT_COHORT_WIDTH
                };
                let active_gate = Arc::new(Barrier::new(producer_concurrency.min(cohort_size)));
                for (producer, command) in commands.iter().enumerate() {
                    let assignments = (producer..cohort_size)
                        .step_by(producer_concurrency)
                        .map(|position| (position, sequence_start + position))
                        .collect();
                    command
                        .send(CommitProducerCommand::Prepare {
                            assignments,
                            active_gate: Arc::clone(&active_gate),
                        })
                        .map_err(|_| "strict group commit producer stopped before preparation")?;
                }
                let mut prepared = Vec::with_capacity(cohort_size);
                let mut preparation_error = None;
                for _ in 0..producer_concurrency {
                    match prepared_receive.recv() {
                        Ok(Ok(mut batches)) => prepared.append(&mut batches),
                        Ok(Err(error)) => {
                            preparation_error.get_or_insert(error);
                        }
                        Err(_) => {
                            return Err(
                                "strict group commit producer response channel disconnected".into(),
                            );
                        }
                    };
                }
                if let Some(error) = preparation_error {
                    return Err(error.into());
                }
                prepared.sort_unstable_by_key(|(position, _)| *position);
                if prepared.len() != cohort_size
                    || prepared
                        .iter()
                        .enumerate()
                        .any(|(position, (observed, _))| position != *observed)
                {
                    return Err("strict group commit preparation lost canonical order".into());
                }
                let batches = prepared.into_iter().map(|(_, batch)| batch).collect();
                let pending = cohort_client.enqueue_cohort(batches)?;
                let outstanding = pending.len();
                let completions = pending
                    .into_iter()
                    .map(|pending| pending.wait_with_evidence())
                    .collect::<Result<Vec<_>, _>>()?;
                measured_wall = measured_wall
                    .checked_add(window_timer.finish()?)
                    .ok_or("strict group commit measured wall time overflowed")?;
                let observations = completions
                    .into_iter()
                    .map(StrictCommitObservation::from_completion)
                    .collect::<Result<Vec<_>, _>>()?;
                evidence.observe_cohort(&observations, outstanding)?;
                pending_progress = pending_progress
                    .checked_add(cohort_size)
                    .ok_or("strict group commit progress overflowed")?;
                if pending_progress >= PROGRESS_CHUNK_UNITS {
                    progress.advance(pending_progress)?;
                    pending_progress = 0;
                }
            }
            if pending_progress > 0 {
                progress.advance(pending_progress)?;
            }
            evidence.record_producer_activity(
                producer_activity.current(),
                producer_activity.maximum(),
                producer_concurrency,
            )?;
            evidence.validate_complete()?;
            Ok((evidence, measured_wall))
        })();

        for command in &commands {
            let _ignored = command.send(CommitProducerCommand::Stop);
        }
        let mut worker_failed = false;
        for worker in workers {
            worker_failed |= worker.join().is_err();
        }
        if worker_failed {
            return Err("strict group commit producer panicked".into());
        }
        measured
    })
}

fn verify_strict_group_commit_reopen(
    group_path: &Path,
    authority: &ExecutionAuthority,
    plan: &StrictGroupCommitPlan,
    baseline_visible_csn: u64,
    baseline_committed_transactions: usize,
) -> Result<StrictGroupCommitReopenEvidence, Box<dyn Error>> {
    let mut reopened = NativeDatabase::open(group_path)?;
    authority.reinstall(&mut reopened)?;
    inspect_strict_group_commit_reopen(
        &reopened,
        plan,
        baseline_visible_csn,
        baseline_committed_transactions,
    )
}

fn inspect_strict_group_commit_reopen(
    reopened: &NativeDatabase,
    plan: &StrictGroupCommitPlan,
    baseline_visible_csn: u64,
    baseline_committed_transactions: usize,
) -> Result<StrictGroupCommitReopenEvidence, Box<dyn Error>> {
    let open_time_nanos = duration_nanos(reopened.recovery_report().open_time)?;
    let recovery = reopened.recovery_report();
    let reopened_visible_csn = reopened
        .recovery_report()
        .visible_csn
        .map(hyphae_native_types::Csn::get)
        .ok_or("strict group commit final reopen omitted its visible CSN")?;
    let reopened_committed_transactions = reopened.recovery_report().committed_transactions;
    let verification_started = Instant::now();
    let snapshot = reopened.snapshot(0)?;
    let mut expected_digest = blake3::Hasher::new();
    let mut recovered_digest = blake3::Hasher::new();
    expected_digest.update(b"blake3-logical-id-key-value-v1\0");
    recovered_digest.update(b"blake3-logical-id-key-value-v1\0");
    let mut missing_keys = 0_usize;
    let mut mismatched_values = 0_usize;
    for sequence in 0..plan.logical_commits {
        let key = strict_group_commit_key(sequence);
        let expected = strict_group_commit_value(sequence);
        update_strict_group_state_digest(&mut expected_digest, sequence, &key, &expected)?;
        match snapshot.get(&key) {
            Some(observed) => {
                update_strict_group_state_digest(&mut recovered_digest, sequence, &key, observed)?;
                mismatched_values += usize::from(observed != expected);
            }
            None => {
                missing_keys += 1;
                update_strict_group_state_digest(&mut recovered_digest, sequence, &key, &[])?;
            }
        }
    }
    let verification_time_nanos = duration_nanos(verification_started.elapsed())?;
    Ok(StrictGroupCommitReopenEvidence {
        baseline_visible_csn,
        baseline_committed_transactions,
        reopened_visible_csn,
        reopened_committed_transactions,
        wal_base_csn: recovery
            .wal_base_csn
            .ok_or("strict group commit retained reopen omitted its WAL base CSN")?
            .get(),
        retained_wal_blocks: recovery.retained_wal_blocks,
        retained_wal_bytes: recovery.retained_wal_bytes,
        replayed_transactions: recovery.replayed_transactions,
        verified_logical_commits: plan.logical_commits,
        missing_keys,
        mismatched_values,
        expected_state_digest: expected_digest.finalize().to_hex().to_string(),
        recovered_state_digest: recovered_digest.finalize().to_hex().to_string(),
        manifest_verification_time_nanos: duration_nanos(recovery.manifest_verification_time)?,
        wal_physical_verification_time_nanos: duration_nanos(
            recovery.wal_physical_verification_time,
        )?,
        wal_semantic_replay_time_nanos: duration_nanos(recovery.wal_semantic_replay_time)?,
        root_validation_time_nanos: duration_nanos(recovery.root_validation_time)?,
        open_time_nanos,
        verification_time_nanos,
    })
}

fn measure_concurrent(
    concurrency: usize,
    observations: usize,
    progress: &SurfaceProgress,
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
                let mut pending_progress = 0;
                barrier.wait();
                for _ in 0..count {
                    let sample = Instant::now();
                    operation().map_err(|error| error.to_string())?;
                    samples.push(sample.elapsed().as_nanos() as u64);
                    pending_progress += 1;
                    if pending_progress == PROGRESS_CHUNK_UNITS {
                        progress
                            .advance(pending_progress)
                            .map_err(|error| error.to_string())?;
                        pending_progress = 0;
                    }
                }
                if pending_progress > 0 {
                    progress
                        .advance(pending_progress)
                        .map_err(|error| error.to_string())?;
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

fn measure_concurrent_with_evidence<T, E>(
    concurrency: usize,
    observations: usize,
    progress: &SurfaceProgress,
    operation: &(impl Fn() -> Result<T, Box<dyn Error>> + Sync),
    validate: &(impl Fn(T) -> Result<E::Observation, Box<dyn Error>> + Sync),
) -> Result<(Stats, E), Box<dyn Error>>
where
    T: Send,
    E: MeasurementEvidence,
{
    if concurrency == 0 || observations < concurrency {
        return Err("routed benchmark requires at least one observation per worker".into());
    }
    let barrier = Barrier::new(concurrency);
    let failure = MeasurementFailure::default();
    let maximum_worker_observations = observations.div_ceil(concurrency);
    let rounds = maximum_worker_observations.div_ceil(EVIDENCE_MEASUREMENT_CHUNK);
    let workers = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for worker in 0..concurrency {
            let count =
                observations / concurrency + usize::from(worker < observations % concurrency);
            let barrier = &barrier;
            let failure = &failure;
            handles.push(scope.spawn(move || {
                run_evidence_worker::<T, E>(
                    count, rounds, barrier, failure, progress, operation, validate,
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| {
                    "routed benchmark worker panicked outside guarded phases".to_owned()
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?;
    let failed = failure.failed.load(Ordering::Acquire);
    let error = failure
        .message
        .into_inner()
        .map_err(|_| "routed benchmark failure state was poisoned")?;
    if failed {
        return Err(error
            .unwrap_or_else(|| "routed benchmark failed without an error message".to_owned())
            .into());
    }
    let mut samples = Vec::with_capacity(observations);
    let mut evidence = E::default();
    let mut query_elapsed = Duration::ZERO;
    for round in 0..rounds {
        let started = workers
            .iter()
            .filter_map(|worker| worker.phases[round].map(|phase| phase.started))
            .min()
            .ok_or("routed benchmark query phase omitted every worker")?;
        let finished = workers
            .iter()
            .filter_map(|worker| worker.phases[round].map(|phase| phase.finished))
            .max()
            .ok_or("routed benchmark query phase omitted its completion")?;
        query_elapsed = query_elapsed.saturating_add(finished.saturating_duration_since(started));
    }
    for worker in workers {
        samples.extend(worker.samples);
        evidence.merge(worker.evidence)?;
    }
    if samples.len() != observations || query_elapsed.is_zero() {
        return Err("routed benchmark did not measure every requested observation".into());
    }
    Ok((
        stats_from_samples(samples, query_elapsed.as_secs_f64()),
        evidence,
    ))
}

fn run_evidence_worker<T, E>(
    observations: usize,
    rounds: usize,
    barrier: &Barrier,
    failure: &MeasurementFailure,
    progress: &SurfaceProgress,
    operation: &(impl Fn() -> Result<T, Box<dyn Error>> + Sync),
    validate: &(impl Fn(T) -> Result<E::Observation, Box<dyn Error>> + Sync),
) -> EvidenceWorkerResult<E>
where
    T: Send,
    E: MeasurementEvidence,
{
    let mut result = EvidenceWorkerResult {
        samples: Vec::with_capacity(observations),
        evidence: E::default(),
        phases: Vec::with_capacity(rounds),
    };
    let mut pending_progress = 0;
    for round in 0..rounds {
        let offset = round.saturating_mul(EVIDENCE_MEASUREMENT_CHUNK);
        let count = observations
            .saturating_sub(offset)
            .min(EVIDENCE_MEASUREMENT_CHUNK);
        barrier.wait();
        let phase = run_evidence_query_phase(count, failure, operation, &mut result.samples);
        result
            .phases
            .push(phase.as_ref().map(|(_, timing)| *timing));
        barrier.wait();
        if let Some((receipts, _)) = phase {
            for receipt in receipts {
                if measurement_failed(failure) {
                    break;
                }
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let observation = validate(receipt)?;
                    result.evidence.observe(observation)?;
                    Ok::<(), Box<dyn Error>>(())
                })) {
                    Ok(Ok(())) => {
                        pending_progress += 1;
                    }
                    Ok(Err(error)) => record_measurement_failure(failure, error.to_string()),
                    Err(_) => record_measurement_failure(
                        failure,
                        "routed benchmark receipt validator panicked".to_owned(),
                    ),
                }
            }
        }
        if pending_progress >= PROGRESS_CHUNK_UNITS {
            if let Err(error) = progress.advance(pending_progress) {
                record_measurement_failure(failure, error.to_string());
            }
            pending_progress = 0;
        }
        barrier.wait();
    }
    if pending_progress > 0
        && let Err(error) = progress.advance(pending_progress)
    {
        record_measurement_failure(failure, error.to_string());
    }
    result
}

fn run_evidence_query_phase<T>(
    count: usize,
    failure: &MeasurementFailure,
    operation: &(impl Fn() -> Result<T, Box<dyn Error>> + Sync),
    samples: &mut Vec<u64>,
) -> Option<(Vec<T>, QueryPhaseTiming)>
where
    T: Send,
{
    if count == 0 || measurement_failed(failure) {
        return None;
    }
    let mut receipts = Vec::with_capacity(count);
    let phase_started = Instant::now();
    for _ in 0..count {
        if measurement_failed(failure) {
            break;
        }
        let sample = Instant::now();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
            Ok(Ok(receipt)) => {
                samples.push(sample.elapsed().as_nanos() as u64);
                receipts.push(receipt);
            }
            Ok(Err(error)) => {
                record_measurement_failure(failure, error.to_string());
                break;
            }
            Err(_) => {
                record_measurement_failure(
                    failure,
                    "routed benchmark operation panicked".to_owned(),
                );
                break;
            }
        }
    }
    let timing = QueryPhaseTiming {
        started: phase_started,
        finished: Instant::now(),
    };
    Some((receipts, timing))
}

fn measurement_failed(failure: &MeasurementFailure) -> bool {
    failure.failed.load(Ordering::Acquire)
}

fn record_measurement_failure(failure: &MeasurementFailure, message: String) {
    if failure
        .failed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
        && let Ok(mut error) = failure.message.lock()
    {
        *error = Some(message);
    }
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

fn physical_observation(
    root: &Path,
    authority: &ExecutionAuthority,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut database = NativeDatabase::open(root.join("structure"))?;
    authority.install(&mut database, "physical-observation")?;
    let observation = database.physical_observation()?;
    Ok(json!({
        "page_count": observation.page_count,
        "physical_page_reads": observation.physical_page_reads,
        "wal_bytes": observation.wal_bytes,
        "process_full_state_loads": observation.process_full_state_loads,
        "process_full_catalog_loads": observation.process_full_catalog_loads,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database_queue_wait_policy(workers: u64) -> NativeGovernorPolicy {
        let memory_bytes = 1_024 * 1_024 * 1_024;
        NativeGovernorPolicy {
            schema: "hyphae-native-governor-policy-v1".to_owned(),
            mode: hyphae_native_runtime::GovernorMode::Mixed,
            hardware_fingerprint: "1".repeat(64),
            calibration_cache_key: "2".repeat(64),
            calibrated_worker_limit: workers,
            reserved_system_threads: 0,
            schedulable_compute_threads: workers,
            io_slots: 1,
            memory_bytes,
            memory_headroom_percent: 15,
            admission_queue_capacity: workers * 2,
            foreground_burst_limit: 16,
            class_limits: [
                WorkloadClass::ForegroundPoint,
                WorkloadClass::ForegroundBounded,
                WorkloadClass::Mutation,
                WorkloadClass::Bulk,
                WorkloadClass::Maintenance,
                WorkloadClass::Recovery,
                WorkloadClass::Administrative,
            ]
            .into_iter()
            .map(|class| hyphae_native_runtime::GovernorClassLimit {
                class,
                compute_threads: if class == WorkloadClass::ForegroundPoint {
                    1
                } else {
                    workers
                },
                io_slots: 1,
                memory_bytes,
            })
            .collect(),
        }
    }

    fn database_queue_wait_pool(
        root: &Path,
        policy: &NativeGovernorPolicy,
    ) -> Result<Arc<NativeExecutionPool>, Box<dyn Error>> {
        let mut profile = HardwareProfile::discover(root)?;
        profile.fingerprint.clone_from(&policy.hardware_fingerprint);
        profile.cpu.logical_processors_available =
            usize::try_from(policy.schedulable_compute_threads)?;
        profile.cpu.processor_topology.clear();
        Ok(Arc::new(NativeExecutionPool::new(&profile, policy)?))
    }

    fn wait_for_database_queue(governor: &NativeResourceGovernor, expected: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while governor.queued_requests() != expected && Instant::now() < deadline {
            thread::yield_now();
        }
        governor.queued_requests() == expected
    }

    #[test]
    fn database_authority_waits_for_concurrent_point_reads_and_drains_usage()
    -> Result<(), Box<dyn Error>> {
        assert_eq!(G7_DATABASE_QUEUE_WAIT, Duration::from_secs(60));
        for concurrency in [8_usize, 32] {
            let root = std::env::temp_dir().join(format!(
                "hyphae-g7-database-queue-wait-{}-{}-{concurrency}",
                std::process::id(),
                unique_nonce()
            ));
            let result = (|| -> Result<(), Box<dyn Error>> {
                let mut database = NativeDatabase::create(&root)?;
                let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
                seed.set(b"queued-read".to_vec(), b"value".to_vec(), None)?;
                seed.commit()?;
                database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;

                let policy = database_queue_wait_policy(u64::try_from(concurrency)?);
                let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
                let pool = database_queue_wait_pool(&root, &policy)?;
                install_database_execution_authority(&mut database, Arc::clone(&governor), pool)?;
                let held = governor.try_admit_owned(
                    WorkloadClass::ForegroundPoint,
                    hyphae_native_runtime::GovernorRequest {
                        compute_threads: 0,
                        io_slots: 1,
                        memory_bytes: 0,
                    },
                )?;
                let database = Arc::new(database);
                let gate = Arc::new(Barrier::new(concurrency + 1));
                let (queued, results) = thread::scope(|scope| {
                    let handles = (0..concurrency)
                        .map(|_| {
                            let database = Arc::clone(&database);
                            let gate = Arc::clone(&gate);
                            scope.spawn(move || {
                                gate.wait();
                                database.get_latest_structure(b"queued-read", 0)
                            })
                        })
                        .collect::<Vec<_>>();
                    gate.wait();
                    let queued = wait_for_database_queue(&governor, concurrency);
                    drop(held);
                    let results = handles
                        .into_iter()
                        .map(|handle| {
                            handle
                                .join()
                                .map_err(|_| "queued database read panicked")?
                                .map_err(Into::into)
                        })
                        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
                    Ok::<_, Box<dyn Error>>((queued, results))
                })?;
                assert!(queued, "all concurrent database reads must wait");
                assert!(
                    results
                        .iter()
                        .all(|value| value.as_deref() == Some(b"value"))
                );
                let usage = governor.usage_snapshot();
                assert_eq!(usage.compute_threads, 0, "{usage:?}");
                assert_eq!(usage.io_slots, 0, "{usage:?}");
                assert_eq!(usage.memory_bytes, 0, "{usage:?}");
                assert_eq!(usage.queued_requests, 0, "{usage:?}");
                Ok(())
            })();
            fs::remove_dir_all(&root)?;
            result?;
        }
        Ok(())
    }

    #[test]
    fn product_authority_waits_for_point_read_and_drains_usage() -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-product-queue-wait-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let result = (|| -> Result<(), Box<dyn Error>> {
            let mut product = NativeProduct::create(&root)?;
            let mut session = product_session();
            let mut context = product_context(&session, 1);
            context.durability = ProductDurabilityPolicy::MEMORY;
            product.dispatch(
                &mut session,
                &context,
                ProductOperation::StructureSet {
                    key: b"queued-product-read".to_vec(),
                    value: b"value".to_vec(),
                    expires_at_micros: None,
                },
            )?;

            let policy = database_queue_wait_policy(1);
            let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
            let pool = database_queue_wait_pool(&root, &policy)?;
            install_product_execution_authority(&mut product, Arc::clone(&governor), pool)?;
            let held = governor.try_admit_owned(
                WorkloadClass::ForegroundPoint,
                hyphae_native_runtime::GovernorRequest {
                    compute_threads: 0,
                    io_slots: 1,
                    memory_bytes: 0,
                },
            )?;
            let reader = thread::spawn(move || {
                product.dispatch(
                    &mut session,
                    &context,
                    ProductOperation::StructureGet {
                        key: b"queued-product-read".to_vec(),
                    },
                )
                .map_err(|error| error.to_string())
            });
            let queued = wait_for_database_queue(&governor, 1);
            drop(held);
            let response = reader
                .join()
                .map_err(|_| "queued product read panicked")?
                .map_err(|error| format!("queued product read failed: {error}"))?;
            assert!(queued, "the product read must wait for database admission");
            assert_eq!(
                response,
                ProductResponse::StructureValue(Some(b"value".to_vec()))
            );
            let usage = governor.usage_snapshot();
            assert_eq!(usage.compute_threads, 0, "{usage:?}");
            assert_eq!(usage.io_slots, 0, "{usage:?}");
            assert_eq!(usage.memory_bytes, 0, "{usage:?}");
            assert_eq!(usage.queued_requests, 0, "{usage:?}");
            Ok(())
        })();
        fs::remove_dir_all(&root)?;
        result
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct CountEvidence(u64);

    impl MeasurementEvidence for CountEvidence {
        type Observation = ();

        fn observe(&mut self, (): Self::Observation) -> Result<(), Box<dyn Error>> {
            self.0 = self.0.saturating_add(1);
            Ok(())
        }

        fn merge(&mut self, other: Self) -> Result<(), Box<dyn Error>> {
            self.0 = self.0.saturating_add(other.0);
            Ok(())
        }
    }

    struct BufferedReceipt {
        live: Arc<AtomicU64>,
    }

    impl Drop for BufferedReceipt {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn measurement_test_progress(total: usize) -> Result<SurfaceProgress, Box<dyn Error>> {
        let cell = CellProgress::new(
            None,
            "1".repeat(40),
            None,
            "3".repeat(64),
            u64::try_from(total)?,
            0,
        )?;
        cell.begin_surface(G7_SURFACE_NAMES[0], 0, total)
            .map_err(Into::into)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_measurement_records_each_operation_latency() -> Result<(), Box<dyn Error>> {
        let progress = measurement_test_progress(3)?;
        progress.begin_phase("measure", 3)?;
        let delays = [
            Duration::from_millis(1),
            Duration::from_millis(12),
            Duration::from_millis(24),
        ];
        let stats = measure_async(
            3,
            3,
            &progress,
            |worker| async move {
                tokio::time::sleep(delays[worker]).await;
                Ok::<_, Box<dyn Error>>(worker)
            },
            &|_| Ok(()),
        )
        .await?;
        progress.finish_phase("measure")?;

        assert!(stats.p50 < stats.maximum);
        assert!(stats.p95 <= stats.maximum);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_rounds_are_balanced_gapless_and_reach_requested_concurrency()
    -> Result<(), Box<dyn Error>> {
        let concurrency = 3;
        let observations = 10;
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("measure", observations)?;
        let counts = Arc::new(
            (0..concurrency)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        );
        let in_flight = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));
        let _stats = measure_async(
            concurrency,
            observations,
            &progress,
            |worker| {
                let counts = Arc::clone(&counts);
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                async move {
                    counts[worker].fetch_add(1, Ordering::Relaxed);
                    let live = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(live, Ordering::AcqRel);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    Ok::<_, Box<dyn Error>>(worker)
                }
            },
            &|_| Ok(()),
        )
        .await?;
        progress.finish_phase("measure")?;

        assert_eq!(peak.load(Ordering::Acquire), u64::try_from(concurrency)?);
        assert_eq!(
            counts
                .iter()
                .map(|count| count.load(Ordering::Acquire))
                .collect::<Vec<_>>(),
            vec![4, 3, 3]
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_warmup_reaches_every_independent_worker() -> Result<(), Box<dyn Error>> {
        let concurrency = 4;
        let observations = 11;
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("warmup", observations)?;
        let counts = Arc::new(
            (0..concurrency)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        );
        warm_async(
            concurrency,
            observations,
            &progress,
            |worker| {
                let counts = Arc::clone(&counts);
                async move {
                    counts[worker].fetch_add(1, Ordering::Relaxed);
                    Ok::<_, Box<dyn Error>>(())
                }
            },
            &|()| Ok(()),
        )
        .await?;
        progress.finish_phase("warmup")?;

        assert_eq!(
            counts
                .iter()
                .map(|count| count.load(Ordering::Acquire))
                .collect::<Vec<_>>(),
            vec![3, 3, 3, 2]
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_throughput_excludes_response_validation() -> Result<(), Box<dyn Error>> {
        let progress = measurement_test_progress(4)?;
        progress.begin_phase("measure", 4)?;
        let stats = measure_async(
            2,
            4,
            &progress,
            |_| async {
                tokio::time::sleep(Duration::from_millis(2)).await;
                Ok::<_, Box<dyn Error>>(())
            },
            &|()| {
                thread::sleep(Duration::from_millis(20));
                Ok(())
            },
        )
        .await?;
        progress.finish_phase("measure")?;

        assert!(stats.throughput > 200.0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_progress_is_outside_the_query_timing_window() -> Result<(), Box<dyn Error>> {
        let progress_path = std::env::temp_dir().join(format!(
            "hyphae-g7-async-progress-{}-{}.json",
            std::process::id(),
            unique_nonce()
        ));
        let cell = CellProgress::new(
            Some(progress_path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            4,
            0,
        )?;
        let progress = cell.begin_surface(G7_SURFACE_NAMES[0], 0, 4)?;
        progress.begin_phase("measure", 4)?;
        let stats = measure_async(
            2,
            4,
            &progress,
            |_| async {
                tokio::time::sleep(Duration::from_millis(2)).await;
                Ok::<_, Box<dyn Error>>(())
            },
            &|()| Ok(()),
        )
        .await?;
        progress.finish_phase("measure")?;
        fs::remove_file(progress_path)?;

        assert!(stats.throughput > 200.0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_sql_clients_prepare_one_handle_per_independent_session()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-local-sql-sessions-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let endpoint = short_endpoint("sql-sessions");
        let mut product = NativeProduct::create(&root)?;
        seed_product_sql(&mut product)?;
        let daemon = LocalDaemonThread::start(
            product,
            endpoint.to_string_lossy().into_owned(),
            NativeDaemonConfig::default(),
        )?;
        let sessions =
            prepare_local_sql_sessions(local_clients(&endpoint, 3)?, &RequestOptions::default())
                .await?;

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.handle.get())
                .collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
        for session in sessions {
            require_sql_response(
                session
                    .client
                    .execute_prepared(
                        session.handle,
                        vec![hyphae_native_product::ProductValue::Signed(
                            (SQL_KEYS / 2) as i64,
                        )],
                        RequestOptions::default(),
                    )
                    .await?,
            )?;
        }
        daemon.shutdown()?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_daemon_owns_a_dedicated_runtime_thread_and_stops_cleanly()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-local-daemon-thread-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let endpoint = short_endpoint("daemon-thread");
        let product = NativeProduct::create(&root)?;
        let caller_thread = thread::current().id();
        let daemon = LocalDaemonThread::start(
            product,
            endpoint.to_string_lossy().into_owned(),
            NativeDaemonConfig::default(),
        )?;

        assert_ne!(daemon.server_thread_id(), caller_thread);
        daemon.shutdown()?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_daemon_startup_error_does_not_stop_existing_owner() -> Result<(), Box<dyn Error>>
    {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-local-daemon-error-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let endpoint = short_endpoint("daemon-error");
        fs::create_dir_all(&root)?;
        let owner = LocalDaemonThread::start(
            NativeProduct::create(root.join("owner"))?,
            endpoint.to_string_lossy().into_owned(),
            NativeDaemonConfig::default(),
        )?;
        let duplicate = LocalDaemonThread::start(
            NativeProduct::create(root.join("duplicate"))?,
            endpoint.to_string_lossy().into_owned(),
            NativeDaemonConfig::default(),
        );

        assert!(duplicate.is_err());
        let client = HyphaeClient::local(endpoint.to_string_lossy().into_owned())?;
        black_box(client.capabilities(RequestOptions::default()).await?);
        owner.shutdown()?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ann_worker_budget_is_bounded_by_the_selected_partition_ceiling() {
        fn worker_budget(
            foreground_threads: u64,
            concurrency: u64,
            preferred_partitions: usize,
        ) -> u64 {
            foreground_threads
                .checked_div(concurrency.max(1))
                .unwrap_or(0)
                .max(1)
                .min(u64::try_from(preferred_partitions).unwrap_or(u64::MAX))
        }

        assert_eq!(worker_budget(96, 1, 32), 32);
        assert_eq!(worker_budget(96, 8, 32), 12);
        assert_eq!(worker_budget(96, 32, 32), 3);
        assert_eq!(worker_budget(4, 8, 32), 1);
    }

    #[test]
    fn evidence_validation_does_not_reduce_measured_engine_throughput() -> Result<(), Box<dyn Error>>
    {
        let observations = 512;
        let fast_progress = measurement_test_progress(observations)?;
        fast_progress.begin_phase("measure", observations)?;
        let (fast, fast_evidence) = measure_concurrent_with_evidence::<_, CountEvidence>(
            4,
            observations,
            &fast_progress,
            &|| {
                thread::sleep(Duration::from_micros(100));
                Ok(())
            },
            &|()| Ok(()),
        )?;
        fast_progress.finish_phase("measure")?;

        let slow_progress = measurement_test_progress(observations)?;
        slow_progress.begin_phase("measure", observations)?;
        let (slow, slow_evidence) = measure_concurrent_with_evidence::<_, CountEvidence>(
            4,
            observations,
            &slow_progress,
            &|| {
                thread::sleep(Duration::from_micros(100));
                Ok(())
            },
            &|()| {
                thread::sleep(Duration::from_micros(200));
                Ok(())
            },
        )?;
        slow_progress.finish_phase("measure")?;

        assert_eq!(fast_evidence.0, u64::try_from(observations)?);
        assert_eq!(slow_evidence.0, u64::try_from(observations)?);
        assert!(slow.throughput >= fast.throughput * 0.5);
        assert!(slow.p99 < 5_000_000);
        Ok(())
    }

    #[test]
    fn evidence_measurement_validates_each_receipt_once_and_fails_on_corruption()
    -> Result<(), Box<dyn Error>> {
        let observations = EVIDENCE_MEASUREMENT_CHUNK + 17;
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("measure", observations)?;
        let sequence = AtomicU64::new(0);
        let validated = AtomicU64::new(0);
        let result = measure_concurrent_with_evidence::<_, CountEvidence>(
            3,
            observations,
            &progress,
            &|| Ok(sequence.fetch_add(1, Ordering::Relaxed)),
            &|receipt| {
                validated.fetch_add(1, Ordering::Relaxed);
                if receipt == 111 {
                    return Err("corrupt routed receipt".into());
                }
                Ok(())
            },
        );

        assert!(result.is_err());
        let validated = validated.load(Ordering::Relaxed);
        assert!(validated > 0);
        assert!(validated < u64::try_from(observations)?);
        Ok(())
    }

    #[test]
    fn evidence_measurement_bounds_live_receipts_by_worker_chunk() -> Result<(), Box<dyn Error>> {
        let concurrency = 3;
        let observations = EVIDENCE_MEASUREMENT_CHUNK * concurrency + 19;
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("measure", observations)?;
        let live = Arc::new(AtomicU64::new(0));
        let peak = AtomicU64::new(0);
        let (_, evidence) = measure_concurrent_with_evidence::<_, CountEvidence>(
            concurrency,
            observations,
            &progress,
            &|| {
                let current = live.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                peak.fetch_max(current, Ordering::Relaxed);
                Ok(BufferedReceipt {
                    live: Arc::clone(&live),
                })
            },
            &|receipt| {
                black_box(&receipt);
                Ok(())
            },
        )?;
        progress.finish_phase("measure")?;

        assert_eq!(evidence.0, u64::try_from(observations)?);
        assert_eq!(live.load(Ordering::Relaxed), 0);
        assert!(
            peak.load(Ordering::Relaxed)
                <= u64::try_from(concurrency.saturating_mul(EVIDENCE_MEASUREMENT_CHUNK))?
        );
        Ok(())
    }

    #[test]
    fn evidence_measurement_operation_panic_is_bounded_and_does_not_deadlock()
    -> Result<(), Box<dyn Error>> {
        let observations = EVIDENCE_MEASUREMENT_CHUNK * 2;
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("measure", observations)?;
        let sequence = AtomicU64::new(0);
        let result = measure_concurrent_with_evidence::<_, CountEvidence>(
            4,
            observations,
            &progress,
            &|| {
                let current = sequence.fetch_add(1, Ordering::Relaxed);
                assert_ne!(current, 57, "injected routed operation panic");
                Ok(())
            },
            &|()| Ok(()),
        );

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn diagnostic_arguments_bind_every_external_authority() -> Result<(), Box<dyn Error>> {
        let arguments = [
            "--source-commit",
            &"1".repeat(40),
            "--source-tree",
            &"2".repeat(40),
            "--platform",
            "linux",
            "--hardware-profile",
            "/tmp/profile.json",
            "--producer-executable-blake3",
            &"3".repeat(64),
            "--compiler-identity",
            "rustc test",
            "--hyphae-build-identity",
            "hyphae-native-g7-runner/0.0.0",
            "--worker-counts",
            "1,2,4,8",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let parsed = HardwareCalibrationDiagnosticArguments::parse(&arguments)?;
        assert_eq!(parsed.source_commit, "1".repeat(40));
        assert_eq!(parsed.source_tree, "2".repeat(40));
        assert_eq!(parsed.worker_counts, vec![1, 2, 4, 8]);

        let mut duplicate = arguments;
        duplicate[2] = "--source-commit".to_owned();
        assert!(HardwareCalibrationDiagnosticArguments::parse(&duplicate).is_err());
        Ok(())
    }

    fn create_lexical_seed_test_database(
        path: &Path,
        lexical_index: ObjectId,
    ) -> Result<NativeDatabase, Box<dyn Error>> {
        let mut database = NativeDatabase::create(path)?;
        let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
        seed.create_search_index(lexical_index, "g7_search")?;
        seed.commit()?;
        Ok(database)
    }

    fn test_seed_progress(document_count: usize) -> Result<Arc<CellProgress>, Box<dyn Error>> {
        CellProgress::new(
            None,
            "1".repeat(40),
            None,
            "3".repeat(64),
            u64::try_from(document_count)?,
            u64::try_from(document_count)?,
        )
    }

    #[test]
    fn staged_seed_results_are_sorted_by_batch_ordinal() {
        let mut staged = vec![(3, "third"), (1, "first"), (2, "second")];
        sort_staged_seed_batches(&mut staged);
        assert_eq!(staged, vec![(1, "first"), (2, "second"), (3, "third")]);
    }

    #[test]
    fn seed_cohort_bound_respects_one_slot_authority() {
        assert_eq!(bounded_seed_cohort_count(96, 1), 1);
        assert_eq!(bounded_seed_cohort_count(96, 2), 2);
        assert_eq!(bounded_seed_cohort_count(1, 2), 1);
        assert_eq!(bounded_seed_cohort_count(96, 8), MAX_SEED_COHORTS);
    }

    #[test]
    fn seed_progress_records_the_executed_cohort_plan() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-g7-seed-cohort-progress-{}-{}.json",
            std::process::id(),
            unique_nonce()
        ));
        let progress = CellProgress::new(
            Some(path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            1_024,
            1_024,
        )?;
        let plan = SeedCohortPlan {
            cohort_count: 2,
            batch_size: SEED_BATCH_DOCUMENTS,
            partition_rule: SEED_PARTITION_RULE,
        };
        progress.begin_search_seed_lexical_with_plan(Some(plan))?;
        let observed: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        fs::remove_file(path)?;

        assert_eq!(observed["details"]["cohort_count"], plan.cohort_count);
        assert_eq!(observed["details"]["batch_size"], plan.batch_size);
        assert_eq!(observed["details"]["partition_rule"], plan.partition_rule);
        Ok(())
    }

    #[test]
    fn two_cohort_seed_matches_serial_seed_after_reopen() -> Result<(), Box<dyn Error>> {
        const DOCUMENT_COUNT: usize = SEED_BATCH_DOCUMENTS * 2 + 1;
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-seed-cohort-equivalence-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let serial_path = root.join("serial");
        let cohort_path = root.join("cohort");
        let lexical_index = ObjectId::new(7)?;
        fs::create_dir_all(&root)?;

        let mut serial = create_lexical_seed_test_database(&serial_path, lexical_index)?;
        let serial_progress = test_seed_progress(DOCUMENT_COUNT)?;
        serial_progress.begin_search_seed_lexical()?;
        seed_lexical_with_cohorts(
            &mut serial,
            lexical_index,
            DOCUMENT_COUNT,
            SeedCohortPlan {
                cohort_count: 1,
                batch_size: SEED_BATCH_DOCUMENTS,
                partition_rule: SEED_PARTITION_RULE,
            },
            &serial_progress,
        )?;
        serial_progress.finish_search_seed_lexical()?;
        drop(serial);

        let mut cohort = create_lexical_seed_test_database(&cohort_path, lexical_index)?;
        let cohort_progress = test_seed_progress(DOCUMENT_COUNT)?;
        let cohort_plan = SeedCohortPlan {
            cohort_count: 2,
            batch_size: SEED_BATCH_DOCUMENTS,
            partition_rule: SEED_PARTITION_RULE,
        };
        cohort_progress.begin_search_seed_lexical_with_plan(Some(cohort_plan))?;
        seed_lexical_with_cohorts(
            &mut cohort,
            lexical_index,
            DOCUMENT_COUNT,
            cohort_plan,
            &cohort_progress,
        )?;
        cohort_progress.finish_search_seed_lexical()?;
        drop(cohort);

        let serial = NativeDatabase::open(&serial_path)?;
        let cohort = NativeDatabase::open(&cohort_path)?;
        let serial_snapshot = serial.snapshot(0)?;
        let cohort_snapshot = cohort.snapshot(0)?;
        let expected_documents = (0..DOCUMENT_COUNT)
            .map(|id| {
                let text = if id == DOCUMENT_COUNT / 2 {
                    "rare g7 native benchmark term"
                } else {
                    "common g7 native benchmark"
                };
                ((id as u128 + 1).to_be_bytes().to_vec(), text.to_owned())
            })
            .collect::<Vec<_>>();

        assert_eq!(
            serial_snapshot.search_documents(lexical_index),
            Some(expected_documents.clone())
        );
        assert_eq!(
            cohort_snapshot.search_documents(lexical_index),
            Some(expected_documents)
        );
        assert_eq!(
            serial.recovery_report().committed_transactions,
            cohort.recovery_report().committed_transactions
        );
        for query in ["rare", "common", "native benchmark"] {
            assert_eq!(
                serial.match_latest_text(lexical_index, query, 10)?,
                cohort.match_latest_text(lexical_index, query, 10)?
            );
        }
        for id in 0..DOCUMENT_COUNT {
            let document_id = (id as u128 + 1).to_be_bytes();
            let expected = if id % 2 == 0 {
                b"keep".as_slice()
            } else {
                b"drop".as_slice()
            };
            assert_eq!(
                serial
                    .get_latest_structure(&filter_key(&document_id), 0)?
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(
                cohort
                    .get_latest_structure(&filter_key(&document_id), 0)?
                    .as_deref(),
                Some(expected)
            );
        }
        drop(serial);
        drop(cohort);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn logical_ann_partition_policy_is_corpus_bound_not_hardware_bound() {
        assert_eq!(logical_ann_partitions(1), 1);
        assert_eq!(logical_ann_partitions(63), 63);
        assert_eq!(logical_ann_partitions(64), 64);
        assert_eq!(logical_ann_partitions(1_000_000), 64);
        assert_eq!(ANN_PARTITION_POLICY, "g7-fixed-64-logical-partitions-v1");
    }

    #[test]
    fn current_executable_digest_hashes_the_exact_runner() -> Result<(), Box<dyn Error>> {
        let expected = blake3::hash(&fs::read(std::env::current_exe()?)?)
            .to_hex()
            .to_string();
        assert_eq!(current_executable_blake3()?, expected);
        Ok(())
    }

    #[test]
    fn hybrid_oracle_is_canonical_and_rejects_native_contribution_drift()
    -> Result<(), Box<dyn Error>> {
        let lexical = [1_u128, 2, 3]
            .into_iter()
            .map(|value| format!("{value:032x}"))
            .collect::<Vec<_>>();
        let vector = [2_u128, 1, 4]
            .into_iter()
            .map(|value| format!("{value:032x}"))
            .collect::<Vec<_>>();
        let expected = fuse_oracle_rankings(&lexical, &vector)?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&expected)?);

        assert_eq!(
            hex_bytes(digest.as_ref()),
            "2b7c71d552ac4b5b5ab35062e8445431c26de892ceb16ee0aa9ef63b4e6514a2"
        );
        assert_eq!(oracle_string(&expected[0], "object_id"), lexical[0]);
        assert_eq!(oracle_string(&expected[1], "object_id"), lexical[1]);
        assert_eq!(oracle_u64(&expected[0], "fusion_score"), 32_522_474);

        let mut matches = expected
            .iter()
            .map(|result| {
                let object_id = u128::from_str_radix(oracle_string(result, "object_id"), 16)?;
                Ok(hyphae_native_runtime::NativeHybridMatch {
                    object_id: ObjectId::new(object_id)?,
                    explanation: hyphae_native_runtime::NativeHybridExplanation {
                        lexical_rank: result["lexical_rank"].as_u64(),
                        lexical_score_nanos: None,
                        vector_rank: result["vector_rank"].as_u64(),
                        vector_score_nanos: None,
                        lexical_contribution: oracle_u64(result, "lexical_contribution"),
                        vector_contribution: oracle_u64(result, "vector_contribution"),
                        fusion_score: oracle_u64(result, "fusion_score"),
                        final_rank: oracle_u64(result, "final_rank"),
                    },
                })
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        let outcome = NativeHybridOutcome::Matches(matches.clone());
        assert_eq!(native_hybrid_results(&outcome)?, expected);

        matches[0].explanation.lexical_contribution += 1;
        let corrupted = NativeHybridOutcome::Matches(matches);
        assert_ne!(native_hybrid_results(&corrupted)?, expected);
        assert!(fuse_oracle_rankings(&[lexical[0].clone(), lexical[0].clone()], &vector).is_err());
        Ok(())
    }

    #[test]
    fn hybrid_oracle_matches_the_normative_disjoint_g7_rankings() -> Result<(), Box<dyn Error>> {
        let lexical = vec![format!("{:032x}", 500_001_u128)];
        let vector = (1_u128..=10)
            .map(|value| format!("{value:032x}"))
            .collect::<Vec<_>>();
        let expected = fuse_oracle_rankings(&lexical, &vector)?;
        let ordered_ids = expected
            .iter()
            .map(|result| u128::from_str_radix(oracle_string(result, "object_id"), 16))
            .collect::<Result<Vec<_>, _>>()?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&expected)?);

        assert_eq!(ordered_ids, vec![1, 500_001, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(expected.len(), K);
        assert_eq!(
            hex_bytes(digest.as_ref()),
            "53146580a1857d393a55bf5c68d8e2d0a437fddb750d2f87161500a8f7a6f2c9"
        );
        Ok(())
    }

    #[test]
    fn routing_interval_requires_complete_strict_certification() -> Result<(), Box<dyn Error>> {
        let route = valid_selected_route()?;
        let (next, kth) = validate_g7_ann_selected_route(&route, 32, 64, 4)?;
        let mut evidence = RoutingIntervalEvidence::new();
        let workers = thread::scope(|scope| -> Result<_, Box<dyn Error>> {
            let mut handles = Vec::new();
            for _ in 0..3 {
                let route = route.clone();
                handles.push(scope.spawn(move || {
                    let mut evidence = RoutingIntervalEvidence::new();
                    let observation = RoutingIntervalEvidence::observation(&route, next, kth)
                        .map_err(|error| error.to_string())?;
                    evidence
                        .observe(observation)
                        .map_err(|error| error.to_string())?;
                    Ok::<_, String>(evidence)
                }));
            }
            let mut workers = Vec::new();
            for handle in handles {
                workers.push(
                    handle
                        .join()
                        .map_err(|_| "routing evidence worker panicked")?
                        .map_err(|error| -> Box<dyn Error> { error.into() })?,
                );
            }
            Ok(workers)
        })?;
        for worker in workers {
            evidence.merge(worker)?;
        }
        assert!(evidence.json(3).is_ok());

        evidence.selected_certified = 2;
        assert!(evidence.json(3).is_err());
        evidence.selected_certified = 3;
        evidence.minimum_next_partition_lower_bound = evidence.maximum_kth_distance;
        assert!(evidence.json(3).is_err());
        Ok(())
    }

    #[test]
    fn selected_route_validation_rejects_every_authority_drift() -> Result<(), Box<dyn Error>> {
        let valid = valid_selected_route()?;
        assert!(validate_g7_ann_selected_route(&valid, 32, 64, 4).is_ok());

        let mut mutated = valid.clone();
        mutated.next_partition_lower_bound = mutated.search.hits.last().map(|hit| hit.distance);
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.next_partition_lower_bound = Some(f64::NAN);
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.routing_mode = hyphae_native_runtime::AnnPartitionRoutingMode::FullFanout;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.routing_policy = "unbound-routing-policy";
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.total_partitions = 63;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.execution_workers = 5;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.execution_worker_batches = 33;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.targeted_single_batches = 3;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid.clone();
        mutated.targeted_single_batches = 0;
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        mutated = valid;
        mutated.search.hits.pop();
        assert!(validate_g7_ann_selected_route(&mutated, 32, 64, 4).is_err());
        Ok(())
    }

    fn valid_selected_route()
    -> Result<hyphae_native_runtime::AnnSelectedSearchReceipt, Box<dyn Error>> {
        Ok(hyphae_native_runtime::AnnSelectedSearchReceipt {
            search: hyphae_native_runtime::AnnSearchReceipt {
                index_id: ObjectId::new(8)?,
                snapshot_csn: None,
                approximate: true,
                build_identity: [2; 32],
                metric: VectorMetric::Cosine,
                ef_search: ANN_QUERY_BREADTH,
                candidate_count: K,
                eligible_candidate_count: K,
                strategy: hyphae_native_runtime::AnnSearchStrategy::GraphTraversal,
                recall_risk: hyphae_native_runtime::AnnRecallRisk::ApproximateTraversal,
                exact_reranked: true,
                visited_nodes: K,
                hits: (0..K)
                    .map(|index| {
                        Ok(hyphae_native_runtime::VectorHit {
                            object_id: ObjectId::new(u128::try_from(index)? + 1)?,
                            distance: 0.1 + (index as f64 * 0.01),
                        })
                    })
                    .collect::<Result<Vec<_>, Box<dyn Error>>>()?,
            },
            base_build_identity: [1; 32],
            view_identity: [2; 32],
            exact_delta_candidates: 0,
            requested_maximum_partitions: 32,
            selected_partitions: vec![0, 1],
            total_partitions: 64,
            routing_mode: hyphae_native_runtime::AnnPartitionRoutingMode::SelectedPartitions,
            routing_outcome: AnnPartitionRoutingOutcome::SelectedCertified,
            next_partition_lower_bound: Some(0.75),
            routing_policy: ANN_PARTITION_ROUTING_POLICY_V1,
            execution_workers: 2,
            execution_worker_batches: 2,
            execution_waves: 2,
            targeted_single_batches: 2,
            generic_single_fallback_batches: 0,
        })
    }

    #[test]
    fn routing_interval_counts_every_single_item_wave_without_overflow()
    -> Result<(), Box<dyn Error>> {
        let route = valid_selected_route()?;
        assert_eq!(route.targeted_single_batches, 2);
        assert_eq!(route.generic_single_fallback_batches, 0);
        assert_eq!(
            route.targeted_single_batches + route.generic_single_fallback_batches,
            route.execution_worker_batches
        );

        let mut evidence = RoutingIntervalEvidence::new();
        evidence.targeted_single_batches = u64::MAX;
        let observation = RoutingObservation {
            execution_workers: 1,
            execution_worker_batches: 1,
            execution_waves: 1,
            selected_partitions: 1,
            targeted_single_batches: 1,
            generic_single_fallback_batches: 0,
            next_partition_lower_bound: 1.0,
            kth_distance: 0.5,
        };
        assert!(evidence.observe(observation).is_err());
        Ok(())
    }

    #[test]
    fn ann_publication_is_not_terminal_cell_progress() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-g7-progress-regression-{}-{}.json",
            std::process::id(),
            unique_nonce()
        ));
        let progress = CellProgress::new(
            Some(path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            30,
            10,
        )?;
        progress.begin_search_seed_lexical()?;
        progress.advance_search_seed_lexical(10)?;
        progress.finish_search_seed_lexical()?;
        let lexical: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.set_phase(
            "ann-private-build",
            "search-seed",
            "ann-bulk-build",
            1,
            2,
            10,
        )?;
        progress.complete_ann([7; 32], &json!({"builder": "test"}))?;
        let ann: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;

        let surface = progress.begin_surface(G7_SURFACE_NAMES[0], 0, 10)?;
        surface.begin_phase("measure", 10)?;
        surface.advance(10)?;
        surface.finish_phase("measure")?;
        surface.complete()?;
        progress.complete_cell(&json!({"cells": {"embedded-structure-point-get": {}}}))?;
        let terminal: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        fs::remove_file(path)?;

        assert_eq!(lexical["stage"], "search-seed-lexical-completed");
        assert_eq!(lexical["completed_units"], 10);
        assert_eq!(ann["stage"], "ann-published");
        assert_eq!(ann["status"], "running");
        assert_eq!(ann["completed_units"], 20);
        assert_eq!(ann["total_units"], 30);
        assert_eq!(ann["details"]["suboperation"]["status"], "completed");
        assert_eq!(ann["details"]["suboperation"]["completed_units"], 10);
        assert_eq!(ann["details"]["suboperation"]["total_units"], 10);
        assert!(ann["sequence"].as_u64() > lexical["sequence"].as_u64());
        assert_eq!(terminal["stage"], "cell-completed");
        assert_eq!(terminal["status"], "completed");
        assert_eq!(terminal["completed_units"], terminal["total_units"]);
        assert!(terminal["sequence"].as_u64() > ann["sequence"].as_u64());
        Ok(())
    }

    #[test]
    fn slow_seed_maintenance_and_open_emit_heartbeats_but_workloads_do_not()
    -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-g7-progress-heartbeat-{}-{}.json",
            std::process::id(),
            unique_nonce()
        ));
        let progress = CellProgress::new(
            Some(path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            10,
            10,
        )?;

        progress.begin_search_seed_maintenance()?;
        let maintenance: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.flush_if_advanced()?;
        let heartbeat: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.begin_search_seed_open()?;
        let opened: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.flush_if_advanced()?;
        let open_heartbeat: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.complete_search_seed_open()?;
        let surface = progress.begin_surface(G7_SURFACE_NAMES[0], 0, 10)?;
        let started: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        progress.flush_if_advanced()?;
        let quiet: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        fs::remove_file(path)?;

        assert_eq!(maintenance["stage"], "search-seed-maintenance");
        assert_eq!(maintenance["completed_units"], heartbeat["completed_units"]);
        assert!(heartbeat["sequence"].as_u64() > maintenance["sequence"].as_u64());
        assert_eq!(opened["stage"], "search-seed-open");
        assert_eq!(opened["completed_units"], open_heartbeat["completed_units"]);
        assert!(open_heartbeat["sequence"].as_u64() > opened["sequence"].as_u64());
        assert_eq!(started["sequence"], quiet["sequence"]);
        drop(surface);
        Ok(())
    }

    #[test]
    fn progress_reporter_stops_immediately_during_idle_heartbeat_phase()
    -> Result<(), Box<dyn Error>> {
        let progress = CellProgress::new(None, "1".repeat(40), None, "3".repeat(64), 10, 10)?;
        progress.begin_search_seed_open()?;
        let started = Instant::now();
        let reporter = progress.start_reporter();
        drop(reporter);
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn surface_idle_phase_error_disarms_heartbeat_without_completing_the_stage()
    -> Result<(), Box<dyn Error>> {
        let progress = CellProgress::new(None, "1".repeat(40), None, "3".repeat(64), 10, 10)?;
        let surface = progress.begin_surface(G7_SURFACE_NAMES[0], 0, 10)?;
        let failed = (|| -> Result<(), Box<dyn Error>> {
            let _phase = surface.idle_phase("injected-failure")?;
            Err("injected idle phase failure".into())
        })();
        assert!(failed.is_err());
        let phase = progress
            .phase
            .lock()
            .map_err(|_| "progress phase lock poisoned")?;
        assert_eq!(phase.stage, "surface-injected-failure");
        assert!(!phase.heartbeat_while_idle);
        Ok(())
    }

    #[test]
    fn search_seed_directory_is_not_published_before_its_evidence() -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-seed-publication-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let staging = root.join("staging");
        let published = root.join("published");
        fs::create_dir_all(&staging)?;
        publish_search_seed_directory(&staging, &published, &json!({"status": "complete"}))?;
        assert!(published.is_dir());
        assert!(initial_ann_bulk_evidence_path(&published).is_file());

        let missing_staging = root.join("missing-staging");
        let incomplete = root.join("incomplete");
        assert!(
            publish_search_seed_directory(
                &missing_staging,
                &incomplete,
                &json!({"status": "complete"})
            )
            .is_err()
        );
        assert!(!incomplete.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn partial_receipt_is_atomic_cumulative_and_terminal_only_after_all_surfaces()
    -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-g7-partial-receipt-{}-{}.json",
            std::process::id(),
            unique_nonce()
        ));
        let mut sink = PartialReceiptSink {
            path: Some(path.clone()),
            source_commit: "1".repeat(40),
            source_tree: Some("2".repeat(40)),
            dataset_digest: "3".repeat(64),
            platform: "linux".to_owned(),
            state: "warm".to_owned(),
            concurrency: 1,
            sequence: 0,
        };
        let mut cells = BTreeMap::new();
        sink.begin_surface(G7_SURFACE_NAMES[0], &cells)?;
        assert!(sink.complete(&cells).is_err());
        for name in G7_SURFACE_NAMES {
            cells.insert(name, json!({"status": "measured"}));
        }
        sink.complete(&cells)?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        fs::remove_file(path)?;

        assert_eq!(receipt.as_object().map(serde_json::Map::len), Some(13));
        assert_eq!(receipt["schema"], "hyphae-native-g7-partial-receipt-v1");
        assert_eq!(receipt["status"], "completed");
        assert_eq!(receipt["completed_count"], G7_SURFACES);
        assert_eq!(receipt["total_cells"], G7_SURFACES);
        assert!(receipt["current_cell"].is_null());
        assert_eq!(
            receipt["cells"].as_object().map(serde_json::Map::len),
            Some(G7_SURFACES)
        );
        Ok(())
    }

    #[test]
    fn cell_terminal_follows_partial_terminal_and_required_cleanup() -> Result<(), Box<dyn Error>> {
        let base = std::env::temp_dir().join(format!(
            "hyphae-g7-terminal-order-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        fs::create_dir_all(&base)?;
        let root = base.join("cell-root");
        fs::create_dir_all(&root)?;
        let blocked_parent = base.join("blocked-partial-parent");
        fs::write(&blocked_parent, b"not-a-directory")?;
        let progress_path = base.join("progress.json");
        let progress = CellProgress::new(
            Some(progress_path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            0,
            0,
        )?;
        progress.begin_search_seed_open()?;
        let mut partial = PartialReceiptSink {
            path: Some(blocked_parent.join("partial.json")),
            source_commit: "1".repeat(40),
            source_tree: Some("2".repeat(40)),
            dataset_digest: "3".repeat(64),
            platform: "linux".to_owned(),
            state: "warm".to_owned(),
            concurrency: 1,
            sequence: 0,
        };
        let cells = G7_SURFACE_NAMES
            .into_iter()
            .map(|name| (name, json!({"status": "measured"})))
            .collect::<BTreeMap<_, _>>();

        assert!(finalize_cell(&progress, &mut partial, &cells, &root, &json!({})).is_err());
        let observed: serde_json::Value = serde_json::from_slice(&fs::read(&progress_path)?)?;
        let root_preserved = root.is_dir();
        fs::remove_dir_all(&base)?;

        assert_ne!(observed["stage"], "cell-completed");
        assert!(root_preserved);

        let cleanup_base = std::env::temp_dir().join(format!(
            "hyphae-g7-cleanup-order-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        fs::create_dir_all(&cleanup_base)?;
        let cleanup_target = cleanup_base.join("not-a-directory");
        fs::write(&cleanup_target, b"cell-root")?;
        let cleanup_progress_path = cleanup_base.join("progress.json");
        let cleanup_partial_path = cleanup_base.join("partial.json");
        let cleanup_progress = CellProgress::new(
            Some(cleanup_progress_path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            0,
            0,
        )?;
        cleanup_progress.begin_search_seed_open()?;
        let mut cleanup_partial = PartialReceiptSink {
            path: Some(cleanup_partial_path.clone()),
            source_commit: "1".repeat(40),
            source_tree: Some("2".repeat(40)),
            dataset_digest: "3".repeat(64),
            platform: "linux".to_owned(),
            state: "warm".to_owned(),
            concurrency: 1,
            sequence: 0,
        };

        assert!(
            finalize_cell(
                &cleanup_progress,
                &mut cleanup_partial,
                &cells,
                &cleanup_target,
                &json!({})
            )
            .is_err()
        );
        let cleanup_progress_observed: serde_json::Value =
            serde_json::from_slice(&fs::read(&cleanup_progress_path)?)?;
        let cleanup_partial_observed: serde_json::Value =
            serde_json::from_slice(&fs::read(&cleanup_partial_path)?)?;
        fs::remove_dir_all(&cleanup_base)?;

        assert_eq!(cleanup_partial_observed["status"], "completed");
        assert_ne!(cleanup_progress_observed["stage"], "cell-completed");

        let success_base = std::env::temp_dir().join(format!(
            "hyphae-g7-terminal-success-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        let success_root = success_base.join("cell-root");
        fs::create_dir_all(&success_root)?;
        let success_progress_path = success_base.join("progress.json");
        let success_partial_path = success_base.join("partial.json");
        let success_progress = CellProgress::new(
            Some(success_progress_path.clone()),
            "1".repeat(40),
            Some("2".repeat(40)),
            "3".repeat(64),
            0,
            0,
        )?;
        success_progress.begin_search_seed_open()?;
        let mut success_partial = PartialReceiptSink {
            path: Some(success_partial_path.clone()),
            source_commit: "1".repeat(40),
            source_tree: Some("2".repeat(40)),
            dataset_digest: "3".repeat(64),
            platform: "linux".to_owned(),
            state: "warm".to_owned(),
            concurrency: 1,
            sequence: 0,
        };

        finalize_cell(
            &success_progress,
            &mut success_partial,
            &cells,
            &success_root,
            &json!({}),
        )?;
        let success_progress_observed: serde_json::Value =
            serde_json::from_slice(&fs::read(&success_progress_path)?)?;
        let success_partial_observed: serde_json::Value =
            serde_json::from_slice(&fs::read(&success_partial_path)?)?;
        let success_root_removed = !success_root.exists();
        fs::remove_dir_all(&success_base)?;

        assert_eq!(success_partial_observed["status"], "completed");
        assert!(success_root_removed);
        assert_eq!(success_progress_observed["stage"], "cell-completed");
        Ok(())
    }

    #[test]
    fn closure_progress_budget_preserves_every_requested_operation() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            total_work_units(1_000_000, 1_000_000, 100_000, true)?,
            14_000_000
        );
        assert_eq!(
            total_work_units(1_000_000, 1_000_000, 100_000, false)?,
            13_000_000
        );
        Ok(())
    }

    #[test]
    fn strict_group_commit_plan_uses_fixed_cohorts_for_full_and_partial_windows()
    -> Result<(), Box<dyn Error>> {
        let cases = [
            (64, 2, 32, BTreeMap::from([(32, 2)])),
            (80, 3, 16, BTreeMap::from([(16, 1), (32, 2)])),
            (10_000, 313, 16, BTreeMap::from([(16, 1), (32, 312)])),
        ];
        for (observations, cohort_count, final_size, size_histogram) in cases {
            let plan = StrictGroupCommitPlan::new(observations)?;
            assert_eq!(plan.cohort_count, cohort_count);
            assert_eq!(plan.final_cohort_size, final_size);
            assert_eq!(plan.cohort_size_histogram, size_histogram);
            for position in 0..32 {
                let full = observations / 32;
                let remainder = observations % 32;
                assert_eq!(
                    plan.cohort_position_histogram.get(&position).copied(),
                    Some(full + usize::from(position < remainder))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn strict_group_commit_window_includes_preparation_without_changing_scheduler_latency()
    -> Result<(), Box<dyn Error>> {
        let started = Instant::now();
        let preparation = Duration::from_millis(7);
        let scheduler_latency = Duration::from_millis(3);
        let scheduler_started = started
            .checked_add(preparation)
            .ok_or("test preparation instant overflowed")?;
        let finished = scheduler_started
            .checked_add(scheduler_latency)
            .ok_or("test scheduler instant overflowed")?;

        let measured_wall = StrictGroupCommitWindowTimer { started }.finish_at(finished)?;

        assert_eq!(measured_wall, preparation + scheduler_latency);
        assert_eq!(
            finished.duration_since(scheduler_started),
            scheduler_latency
        );
        Ok(())
    }

    #[test]
    fn strict_group_commit_activity_is_observed_for_every_producer_and_unwinds_after_panic() {
        for concurrency in [1, 8, 32] {
            let activity = Arc::new(StrictProducerActivity::default());
            let gate = Arc::new(Barrier::new(concurrency));
            thread::scope(|scope| {
                for _ in 0..concurrency {
                    let activity = Arc::clone(&activity);
                    let gate = Arc::clone(&gate);
                    scope.spawn(move || {
                        let _guard = activity.enter();
                        gate.wait();
                    });
                }
            });
            assert_eq!(activity.current(), 0);
            assert_eq!(activity.maximum(), concurrency);
        }

        let activity = Arc::new(StrictProducerActivity::default());
        let panicking_activity = Arc::clone(&activity);
        assert!(
            thread::spawn(move || {
                let _guard = panicking_activity.enter();
                panic!("injected strict producer panic");
            })
            .join()
            .is_err()
        );
        assert_eq!(activity.current(), 0);
        assert_eq!(activity.maximum(), 1);
    }

    #[test]
    fn strict_group_commit_plan_rejects_forged_histograms() -> Result<(), Box<dyn Error>> {
        let plan = StrictGroupCommitPlan::new(80)?;
        let mut forged_sizes = plan.cohort_size_histogram.clone();
        forged_sizes.insert(31, 1);
        assert!(
            plan.validate_histograms(&forged_sizes, &plan.cohort_position_histogram)
                .is_err()
        );

        let mut forged_positions = plan.cohort_position_histogram.clone();
        forged_positions.insert(31, 1);
        assert!(
            plan.validate_histograms(&plan.cohort_size_histogram, &forged_positions)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn strict_group_commit_evidence_rejects_forged_receipts() -> Result<(), Box<dyn Error>> {
        let cohort = |start_csn: u64| {
            (0..STRICT_GROUP_COMMIT_COHORT_WIDTH)
                .map(|position| StrictCommitObservation {
                    transaction_id: u128::from(start_csn) + position as u128,
                    commit_csn: start_csn + position as u64,
                    catalog_version: 1,
                    commit_lsn: start_csn + position as u64,
                    wal_block_digest: [position as u8; 32],
                    cohort_size: STRICT_GROUP_COMMIT_COHORT_WIDTH,
                    cohort_position: position,
                    page_synchronizations: 1,
                    wal_synchronizations: 1,
                    admission_wait_nanos: 0,
                    queue_wait_nanos: 1,
                    cohort_execution_nanos: 3,
                    page_synchronization_nanos: 1,
                    wal_synchronization_nanos: 1,
                    end_to_end_nanos: 4,
                })
                .collect::<Vec<_>>()
        };
        let plan = StrictGroupCommitPlan::new(64)?;
        let mut evidence = StrictGroupCommitEvidence::new(plan.clone(), 2);
        evidence.observe_cohort(&cohort(3), STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT)?;
        evidence.observe_cohort(&cohort(35), STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT)?;
        evidence.record_producer_activity(0, 8, 8)?;
        evidence.validate_complete()?;

        let mut forged_position = cohort(3);
        forged_position[7].cohort_position = 6;
        assert!(
            StrictGroupCommitEvidence::new(plan.clone(), 2)
                .observe_cohort(&forged_position, STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT)
                .is_err()
        );

        let mut forged_csn = cohort(3);
        forged_csn[9].commit_csn += 1;
        assert!(
            StrictGroupCommitEvidence::new(plan, 2)
                .observe_cohort(&forged_csn, STRICT_GROUP_COMMIT_OUTSTANDING_LIMIT)
                .is_err()
        );
        Ok(())
    }

    fn strict_group_commit_mutation_cap_two_policy() -> NativeGovernorPolicy {
        let memory_bytes = 128 * 1_024 * 1_024;
        NativeGovernorPolicy {
            schema: "hyphae-native-governor-policy-v1".to_owned(),
            mode: hyphae_native_runtime::GovernorMode::Mixed,
            hardware_fingerprint: "1".repeat(64),
            calibration_cache_key: "2".repeat(64),
            calibrated_worker_limit: 2,
            reserved_system_threads: 0,
            schedulable_compute_threads: 2,
            io_slots: 2,
            memory_bytes,
            memory_headroom_percent: 15,
            admission_queue_capacity: 64,
            foreground_burst_limit: 16,
            class_limits: [
                WorkloadClass::ForegroundPoint,
                WorkloadClass::ForegroundBounded,
                WorkloadClass::Mutation,
                WorkloadClass::Bulk,
                WorkloadClass::Maintenance,
                WorkloadClass::Recovery,
                WorkloadClass::Administrative,
            ]
            .into_iter()
            .map(|class| hyphae_native_runtime::GovernorClassLimit {
                class,
                compute_threads: 2,
                io_slots: 2,
                memory_bytes,
            })
            .collect(),
        }
    }

    #[test]
    fn strict_group_commit_small_cohorts_with_mutation_cap_two_survive_full_reopen()
    -> Result<(), Box<dyn Error>> {
        const OBSERVATIONS: usize = 64;
        for concurrency in [1, 8, 32] {
            let diagnostic = exercise_strict_group_commit_recovery(OBSERVATIONS, concurrency)?;
            assert_eq!(diagnostic["observations"], OBSERVATIONS);
            assert_eq!(diagnostic["concurrency"], concurrency);
            let evidence = &diagnostic["group_commit_evidence"];
            assert_eq!(
                evidence["schema"],
                "hyphae-native-g7-strict-group-commit-evidence-v2"
            );
            assert_eq!(evidence["reopen"]["replayed_transactions"], 0);
            assert_eq!(evidence["reopen"]["verified_logical_commits"], OBSERVATIONS);
        }
        Ok(())
    }

    fn exercise_strict_group_commit_recovery(
        observations: usize,
        concurrency: usize,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "hyphae-g7-strict-group-reopen-{}-{}-{concurrency}",
            std::process::id(),
            unique_nonce()
        ));
        let result = exercise_strict_group_commit_recovery_at(&root, observations, concurrency);
        fs::remove_dir_all(&root)?;
        result
    }

    fn exercise_strict_group_commit_recovery_at(
        root: &Path,
        observations: usize,
        concurrency: usize,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        let mut database = NativeDatabase::create(root)?;
        let mut seed = database.begin(0, hyphae_native_types::DurabilityClass::Memory)?;
        seed.set(b"g7-group-seed".to_vec(), b"v".to_vec(), None)?;
        seed.commit()?;
        database.migrate_structure_to_v3(hyphae_native_types::DurabilityClass::Memory)?;
        drop(database);

        let mut baseline = NativeDatabase::open(root)?;
        let baseline_visible_csn = baseline
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
            .ok_or("test baseline omitted its visible CSN")?;
        let baseline_committed_transactions = baseline.recovery_report().committed_transactions;
        let governor = Arc::new(NativeResourceGovernor::new(
            strict_group_commit_mutation_cap_two_policy(),
        ));
        baseline.set_resource_governor_with_queue_wait(
            Arc::clone(&governor),
            Duration::from_millis(100),
        )?;
        let config = hyphae_native_runtime::GroupCommitConfig::new(
            STRICT_GROUP_COMMIT_COHORT_WIDTH,
            STRICT_GROUP_COMMIT_COLLECTION_WAIT,
            STRICT_GROUP_COMMIT_QUEUE_CAPACITY,
        )?
        .with_execution_admission_wait(STRICT_GROUP_COMMIT_EXECUTION_WAIT)?;
        let scheduler = NativeCommitScheduler::start(baseline, config)?;
        let producers = (0..concurrency)
            .map(|_| scheduler.client())
            .collect::<Vec<_>>();
        let progress = measurement_test_progress(observations)?;
        progress.begin_phase("measure", observations)?;
        let plan = StrictGroupCommitPlan::new(observations)?;
        let (evidence, measured_wall) = measure_strict_group_commits(
            scheduler.client(),
            producers,
            plan.clone(),
            baseline_visible_csn,
            &progress,
        )?;
        progress.finish_phase("measure")?;
        assert!(measured_wall > Duration::ZERO);
        assert_eq!(evidence.maximum_active_producers, concurrency);
        let mut database = scheduler.shutdown_into_database()?;
        let maintenance = perform_strict_group_commit_maintenance(
            &mut database,
            evidence.last_commit_csn.ok_or("test omitted last CSN")?,
            &progress,
        )?;
        drop(database);

        let reopened = NativeDatabase::open(root)?;
        let reopen = inspect_strict_group_commit_reopen(
            &reopened,
            &plan,
            baseline_visible_csn,
            baseline_committed_transactions,
        )?;
        reopen.validate(
            &plan,
            evidence.first_commit_csn.ok_or("test omitted first CSN")?,
            evidence.last_commit_csn.ok_or("test omitted last CSN")?,
            maintenance.maintenance_csn,
        )?;
        let serialized = evidence.json(concurrency, &maintenance, &reopen)?;
        assert_eq!(serialized["reopen"]["replayed_transactions"], 0);
        let usage = governor.usage_snapshot();
        assert_eq!(usage.compute_threads, 0, "{usage:?}");
        assert_eq!(usage.io_slots, 0, "{usage:?}");
        assert_eq!(usage.memory_bytes, 0, "{usage:?}");
        Ok(json!({
            "schema": "hyphae-native-g7-strict-group-commit-diagnostic-v1",
            "closure_declared": false,
            "observations": observations,
            "concurrency": concurrency,
            "hot_wall_time_nanos": duration_nanos(measured_wall)?,
            "group_commit_evidence": serialized,
        }))
    }
}
