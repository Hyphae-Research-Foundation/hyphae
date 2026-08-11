// SPDX-License-Identifier: AGPL-3.0-only

//! Controlled Native G7 benchmark runner.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    hint::black_box,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
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
    AnnPartitionRoutingOutcome, AnnSearchOptions, CalibrationMode, CalibrationRequest,
    HardwareCalibration, HardwareProfile, HnswConfig, InitialAnnBulkBuildEvidence,
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
const CLOSURE_ANN_LOGICAL_PARTITIONS: usize = 64;
const ANN_PARTITION_POLICY: &str = "g7-fixed-64-logical-partitions-v1";
const K: usize = 10;
const ANN_QUERY_BREADTH: usize = 64;
const G7_PREFERRED_ANN_PARTITIONS: usize = 32;
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(10);
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_CHUNK_UNITS: usize = 10_000;
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

struct SearchFixture {
    database: NativeDatabase,
    ann_view: Mutex<Option<hyphae_native_runtime::NativeAnnReadView>>,
    ann_view_open: hyphae_native_runtime::NativeAnnReadViewOpenReceipt,
    lexical_index: ObjectId,
    vector_index: ObjectId,
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
                details: None,
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
        database
            .set_resource_governor_with_execution_pool(
                Arc::clone(&self.governor),
                Arc::clone(&self.execution_pool),
                Duration::ZERO,
            )
            .map_err(|error| format!("G7 execution authority install failed: {error}"))?;
        if database.resource_governor().is_none() || database.execution_pool().is_none() {
            return Err("G7 database did not retain its execution authority".into());
        }
        self.record_installation(surface)?;
        Ok(())
    }

    fn install_product(
        &self,
        product: &mut NativeProduct,
        surface: &str,
    ) -> Result<(), Box<dyn Error>> {
        product.set_resource_governor_with_execution_pool(
            Arc::clone(&self.governor),
            Arc::clone(&self.execution_pool),
            Duration::ZERO,
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
        let (ann_view, ann_view_open) = database.open_ann_read_view(vector_index)?;
        Ok(Self {
            database,
            ann_view: Mutex::new(Some(ann_view)),
            ann_view_open,
            lexical_index,
            vector_index,
            foreground_compute_threads,
            query,
            options,
            initial_ann_bulk,
        })
    }
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
    progress.begin_search_seed_lexical()?;
    for batch_start in (0..document_count).step_by(512) {
        let batch_end = (batch_start + 512).min(document_count);
        let mut batch =
            database.begin_optimistic_delta(0, hyphae_native_types::DurabilityClass::Memory)?;
        for id in batch_start..batch_end {
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
        progress.advance_search_seed_lexical(batch_end - batch_start)?;
    }
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let allocation_start = GLOBAL_ALLOCATOR.stats();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--hardware-calibrate") {
        return run_hardware_calibration(&arguments[1..]);
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
            progress.begin_phase("warmup", warmup)?;
            for _ in 0..warmup {
                require_structure_response(
                    client
                        .structure_get(b"g7-local-structure".to_vec(), options.clone())
                        .await?,
                )?;
                progress.advance(1)?;
            }
            progress.finish_phase("warmup")?;
        }
        progress.begin_phase("measure", observations)?;
        let stats = measure_async(concurrency, observations, progress, || {
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
        progress.finish_phase("measure")?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
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
            progress.begin_phase("warmup", warmup)?;
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
                progress.advance(1)?;
            }
            progress.finish_phase("warmup")?;
        }
        progress.begin_phase("measure", observations)?;
        let stats = measure_async(concurrency, observations, progress, || {
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
        progress.finish_phase("measure")?;
        stats_with_materialization(stats, materialization)
    }
    .await;
    let shutdown = daemon.shutdown().await?;
    drop(shutdown);
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
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(filtered_bm25_query(
                &fixture.database,
                fixture.lexical_index,
            )?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        if filtered_bm25_query(&fixture.database, fixture.lexical_index)? != 1 {
            return Err("filtered BM25 result mismatch".into());
        }
        Ok(())
    })?;
    progress.finish_phase("measure")?;
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
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let snapshot = fixture.database.snapshot(0)?;
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(hybrid_query(
                &fixture.database,
                &snapshot,
                fixture.lexical_index,
                fixture.vector_index,
                &fixture.query,
                fixture.options,
            )?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        if hybrid_query(
            &fixture.database,
            &snapshot,
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
    progress.finish_phase("measure")?;
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
    progress: &SurfaceProgress,
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
            progress.advance(1)?;
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
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let materialization = NativeDatabase::process_materialization_observation();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        for _ in 0..warmup {
            black_box(
                fixture
                    .database
                    .match_latest_text(fixture.lexical_index, "rare", K)?,
            );
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let hits = fixture
            .database
            .match_latest_text(fixture.lexical_index, "rare", K)?;
        if hits.is_empty() {
            return Err("BM25 result mismatch".into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    progress.finish_phase("measure")?;
    stats_with_materialization(stats, materialization)
}

fn run_ann(
    fixture: &SearchFixture,
    warm: bool,
    concurrency: usize,
    observations: usize,
    warmup: usize,
    progress: &SurfaceProgress,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let view = fixture
        .ann_view
        .lock()
        .map_err(|_| "ANN read-view fixture lock poisoned")?
        .take()
        .ok_or("ANN read view was already consumed")?;
    let offered_concurrency = u64::try_from(concurrency).map_err(|_| "invalid ANN concurrency")?;
    let query_workers = fixture
        .foreground_compute_threads
        .checked_div(offered_concurrency.max(1))
        .unwrap_or(0)
        .max(1);
    let query_queue_wait = Duration::from_secs(60);
    let preferred_partitions = G7_PREFERRED_ANN_PARTITIONS
        .min(fixture.ann_view_open.logical_partitions)
        .max(1);
    let materialization = NativeDatabase::process_materialization_observation();
    let execution_workers_max = AtomicU64::new(0);
    let worker_batches_max = AtomicU64::new(0);
    let execution_waves_max = AtomicU64::new(0);
    let selected_certified = AtomicU64::new(0);
    let full_fanout_requested = AtomicU64::new(0);
    let budget_fallback = AtomicU64::new(0);
    let single_generation_fallback = AtomicU64::new(0);
    let lower_bound_present = AtomicU64::new(0);
    let physical_before = fixture.database.physical_observation()?;
    let restores_before = NativeDatabase::process_ann_index_restore_count();
    if warm {
        progress.begin_phase("warmup", warmup)?;
        if warmup > 0 {
            let first = view.search_selected_with_worker_budget(
                &fixture.query,
                fixture.options,
                preferred_partitions,
                query_workers,
                query_queue_wait,
            )?;
            validate_g7_ann_selected_route(&first.search, preferred_partitions)?;
            black_box(first);
            progress.advance(1)?;
        }
        for _ in 1..warmup {
            black_box(view.search_selected_with_worker_budget(
                &fixture.query,
                fixture.options,
                preferred_partitions,
                query_workers,
                query_queue_wait,
            )?);
            progress.advance(1)?;
        }
        progress.finish_phase("warmup")?;
    }
    progress.begin_phase("measure", observations)?;
    let stats = measure_concurrent(concurrency, observations, progress, &|| {
        let receipt = view.search_selected_with_worker_budget(
            &fixture.query,
            fixture.options,
            preferred_partitions,
            query_workers,
            query_queue_wait,
        )?;
        if receipt.hydration_performed
            || receipt.physical_page_reads != 0
            || receipt.restore_count != 0
        {
            return Err("ANN read-view query crossed the hydration boundary".into());
        }
        validate_g7_ann_selected_route(&receipt.search, preferred_partitions)?;
        execution_workers_max.fetch_max(
            u64::try_from(receipt.search.execution_workers).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        worker_batches_max.fetch_max(
            u64::try_from(receipt.search.execution_worker_batches).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        execution_waves_max.fetch_max(
            u64::try_from(receipt.search.execution_waves).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        match receipt.search.routing_outcome {
            AnnPartitionRoutingOutcome::SelectedCertified => {
                selected_certified.fetch_add(1, Ordering::Relaxed);
            }
            AnnPartitionRoutingOutcome::FullFanoutRequested => {
                full_fanout_requested.fetch_add(1, Ordering::Relaxed);
            }
            AnnPartitionRoutingOutcome::FullFanoutBudgetFallback => {
                budget_fallback.fetch_add(1, Ordering::Relaxed);
            }
            AnnPartitionRoutingOutcome::SingleGenerationFallback => {
                single_generation_fallback.fetch_add(1, Ordering::Relaxed);
            }
        }
        if receipt.search.next_partition_lower_bound.is_some() {
            lower_bound_present.fetch_add(1, Ordering::Relaxed);
        }
        black_box(receipt);
        Ok::<(), Box<dyn Error>>(())
    })?;
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
    if correctness.hydration_performed
        || correctness.physical_page_reads != 0
        || correctness.restore_count != 0
    {
        return Err("ANN read-view correctness query crossed the hydration boundary".into());
    }
    validate_g7_ann_selected_route(&correctness.search, preferred_partitions)?;
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
    output["ann_routing_interval"] = json!({
        "observations": observations,
        "execution_workers_max": execution_workers_max.load(Ordering::Relaxed),
        "execution_worker_batches_max": worker_batches_max.load(Ordering::Relaxed),
        "execution_waves_max": execution_waves_max.load(Ordering::Relaxed),
        "selected_certified": selected_certified.load(Ordering::Relaxed),
        "full_fanout_requested": full_fanout_requested.load(Ordering::Relaxed),
        "full_fanout_budget_fallback": budget_fallback.load(Ordering::Relaxed),
        "single_generation_fallback": single_generation_fallback.load(Ordering::Relaxed),
        "next_partition_lower_bound_present": lower_bound_present.load(Ordering::Relaxed),
    });
    output["ann_read_view_open"] = json!({
        "root_identity": blake3::Hash::from_bytes(fixture.ann_view_open.root_identity)
            .to_hex()
            .to_string(),
        "base_build_identity": blake3::Hash::from_bytes(
            fixture.ann_view_open.base_build_identity,
        )
        .to_hex()
        .to_string(),
        "view_identity": blake3::Hash::from_bytes(fixture.ann_view_open.view_identity)
            .to_hex()
            .to_string(),
        "logical_partitions": fixture.ann_view_open.logical_partitions,
        "planned_physical_entries": fixture.ann_view_open.planned_physical_entries,
        "planned_physical_bytes": fixture.ann_view_open.planned_physical_bytes,
        "observed_physical_entries": fixture.ann_view_open.observed_physical_entries,
        "observed_physical_bytes": fixture.ann_view_open.observed_physical_bytes,
        "planned_peak_memory_bytes": fixture.ann_view_open.planned_peak_memory_bytes,
        "retained_memory_bytes": fixture.ann_view_open.retained_memory_bytes,
        "hydration_restore_count": fixture.ann_view_open.hydration_restore_count,
        "process_physical_page_read_delta": fixture
            .ann_view_open
            .process_physical_page_read_delta,
        "governor_generation": fixture.ann_view_open.governor_generation,
        "routing_policy_identity": blake3::Hash::from_bytes(
            fixture.ann_view_open.routing_policy_identity,
        )
        .to_hex()
        .to_string(),
    });
    drop(view);
    Ok(output)
}

fn validate_g7_ann_selected_route(
    receipt: &hyphae_native_runtime::AnnSelectedSearchReceipt,
    preferred_partitions: usize,
) -> Result<(), Box<dyn Error>> {
    if receipt.requested_maximum_partitions != preferred_partitions
        || receipt.routing_outcome != AnnPartitionRoutingOutcome::SelectedCertified
        || receipt.selected_partitions.len() > preferred_partitions
        || receipt.execution_workers == 0
        || receipt.execution_worker_batches == 0
        || receipt.execution_waves != 1
        || receipt.next_partition_lower_bound.is_none()
    {
        return Err("G7 ANN route was not selected-certified within the preferred budget".into());
    }
    Ok(())
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
    fs::create_dir_all(root)?;
    let group_path = root.join("group");
    let mut database =
        NativeDatabase::create(&group_path).map_err(|error| format!("group seed: {error}"))?;
    authority.install(&mut database, "group-commit")?;
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
    progress.begin_phase("measure", observations)?;
    let started = Instant::now();
    let samples = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(concurrency);
        for (producer, client) in clients.into_iter().enumerate() {
            let count =
                observations / concurrency + usize::from(producer < observations % concurrency);
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<Vec<u64>, String> {
                let mut samples = Vec::with_capacity(count);
                let mut pending_progress = 0;
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
                    .map_err(|_| "group commit worker panicked".to_owned())?
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .map_err(|error| -> Box<dyn Error> { error.into() })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    progress.finish_phase("measure")?;
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
}
