// SPDX-License-Identifier: AGPL-3.0-only

//! Persistent NUMA-local workers backed by one governor allocation.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use nix::{
    sched::{CpuSet, sched_setaffinity},
    unistd::Pid,
};
use serde::Serialize;
use thiserror::Error;

use crate::{
    GovernorAdmissionError, GovernorRequest, HardwareCalibration, HardwareProfile,
    NativeGovernorPolicy, OwnedGovernorPermit, WorkloadClass,
    calibration::physical_core_first_processor_order,
};

struct Job {
    class: WorkloadClass,
    enqueued_at: Instant,
    operation: Box<dyn FnOnce() + Send + 'static>,
    completed: Option<mpsc::Sender<bool>>,
    targeted_completion: Option<TargetedCompletion>,
}

struct TargetedCompletion {
    state: Arc<Mutex<Option<(bool, usize)>>>,
    event: mpsc::Sender<()>,
}
const EXECUTION_TOPOLOGY_SCHEMA: &str = "hyphae-native-execution-topology-v1";
const NUMA_STEAL_POLICY_SCHEMA: &str = "hyphae-native-numa-steal-policy-v1";
const NUMA_CALIBRATION_WORKING_SET_BYTES: u64 = 8 * 1_024 * 1_024;

/// Failure while deriving or executing on persistent native workers.
#[derive(Debug, Error)]
pub enum NativeExecutionError {
    /// Static hardware and policy identities differ.
    #[error("native execution topology hardware identity differs from governor policy")]
    HardwareIdentityMismatch,
    /// The policy requests more workers than static discovery can place.
    #[error("native execution topology cannot place every schedulable worker")]
    InsufficientTopology,
    /// Static discovery repeats one operating-system processor identifier.
    #[error("native execution topology repeats logical processor {0}")]
    DuplicateProcessor(u32),
    /// A policy or execution request contains no usable worker.
    #[error("native execution requires at least one worker and one item")]
    EmptyExecution,
    /// A persistent pool does not match the complete governor worker budget.
    #[error("native execution pool does not match the governor worker policy")]
    PolicyWorkerMismatch,
    /// Multi-node execution lacks a complete stable directed NUMA matrix.
    #[error("native execution requires a complete stable directed NUMA calibration matrix")]
    NumaCalibrationUnavailable,
    /// A persistent worker could not be created.
    #[error("native execution worker could not be created: {0}")]
    ThreadSpawn(#[source] io::Error),
    /// A worker could not bind to its declared logical processor.
    #[error("native execution worker could not bind to logical processor {logical_id}: {reason}")]
    Affinity {
        /// Declared operating-system processor identifier.
        logical_id: u32,
        /// Platform failure.
        reason: String,
    },
    /// A submitted operation panicked; the worker itself remains usable.
    #[error("native execution operation panicked")]
    OperationPanicked,
    /// Internal completion or result synchronization failed closed.
    #[error("native execution synchronization failed")]
    Synchronization,
    /// Parent governor capacity could not be subdivided for workers.
    #[error(transparent)]
    Admission(#[from] GovernorAdmissionError),
}

/// Deterministic placement of one persistent worker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeWorkerPlacement {
    /// Stable zero-based worker index.
    pub worker_index: usize,
    /// NUMA node preferred by this worker when known.
    pub numa_node_id: Option<u32>,
    /// Operating-system logical processor used for hard affinity when known.
    pub logical_processor_id: Option<u32>,
    /// Physical package identifier when known.
    pub socket_id: Option<u32>,
    /// Physical core identifier when known.
    pub core_id: Option<u32>,
    /// Zero for the first hardware thread selected from a physical core.
    pub smt_rank: Option<u32>,
}

/// One NUMA-local worker group.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeNumaPoolTopology {
    /// NUMA node, or `None` for the portable unbound pool.
    pub numa_node_id: Option<u32>,
    /// Deterministically ordered workers assigned to this pool.
    pub workers: Vec<NativeWorkerPlacement>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct NativeNumaStealTarget {
    home_numa_node_id: u32,
    remote_to_local_latency_ppm: u64,
    steal_after_nanoseconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct NativeNumaStealPoolPolicy {
    worker_numa_node_id: Option<u32>,
    steal_targets: Vec<NativeNumaStealTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct NativeNumaStealPolicy {
    schema: String,
    calibration_cache_key: String,
    status: String,
    working_set_bytes: u64,
    foreground_burst_limit: u64,
    pools: Vec<NativeNumaStealPoolPolicy>,
}

/// Reproducible persistent-worker topology derived from hardware and policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeExecutionTopology {
    /// Versioned serialized topology contract.
    pub schema: String,
    /// Static hardware fingerprint used for placement.
    pub hardware_fingerprint: String,
    /// Complete governor worker budget represented by `pools`.
    pub schedulable_compute_threads: u64,
    /// Whether workers are hard-bound to declared processors.
    pub hard_affinity: bool,
    /// NUMA-local worker pools.
    pub pools: Vec<NativeNumaPoolTopology>,
    /// Exact-source cross-node stealing policy.
    numa_steal_policy: NativeNumaStealPolicy,
}

impl NativeExecutionTopology {
    /// Derives physical-core-first placement from one verified policy/profile pair.
    ///
    /// # Errors
    ///
    /// Rejects mismatched identities, repeated processor identifiers, or a
    /// schedulable worker count larger than the visible topology.
    pub fn derive(
        profile: &HardwareProfile,
        policy: &NativeGovernorPolicy,
    ) -> Result<Self, NativeExecutionError> {
        if profile.fingerprint != policy.hardware_fingerprint {
            return Err(NativeExecutionError::HardwareIdentityMismatch);
        }
        let worker_count = usize::try_from(policy.schedulable_compute_threads)
            .map_err(|_| NativeExecutionError::InsufficientTopology)?;
        if worker_count == 0 || worker_count > profile.cpu.logical_processors_available {
            return Err(NativeExecutionError::InsufficientTopology);
        }
        if profile.cpu.processor_topology.is_empty() {
            let workers = (0..worker_count)
                .map(|worker_index| NativeWorkerPlacement {
                    worker_index,
                    numa_node_id: None,
                    logical_processor_id: None,
                    socket_id: None,
                    core_id: None,
                    smt_rank: None,
                })
                .collect();
            let pools = vec![NativeNumaPoolTopology {
                numa_node_id: None,
                workers,
            }];
            return Ok(Self {
                schema: EXECUTION_TOPOLOGY_SCHEMA.to_owned(),
                hardware_fingerprint: profile.fingerprint.clone(),
                schedulable_compute_threads: policy.schedulable_compute_threads,
                hard_affinity: false,
                numa_steal_policy: disabled_numa_steal_policy(policy, &pools),
                pools,
            });
        }

        let pools = derive_discovered_pools(profile, worker_count)?;
        let numa_steal_policy = disabled_numa_steal_policy(policy, &pools);
        Ok(Self {
            schema: EXECUTION_TOPOLOGY_SCHEMA.to_owned(),
            hardware_fingerprint: profile.fingerprint.clone(),
            schedulable_compute_threads: policy.schedulable_compute_threads,
            hard_affinity: cfg!(target_os = "linux"),
            pools,
            numa_steal_policy,
        })
    }

    /// Derives placement plus an exact-source cross-node steal policy.
    ///
    /// # Errors
    ///
    /// Multi-node topologies require every stable directed source/reader pair,
    /// or explicit unsupported coverage that disables remote stealing.
    pub fn derive_with_calibration(
        profile: &HardwareProfile,
        policy: &NativeGovernorPolicy,
        calibration: &HardwareCalibration,
    ) -> Result<Self, NativeExecutionError> {
        if calibration.identity.hardware_fingerprint != profile.fingerprint
            || calibration.identity.cache_key != policy.calibration_cache_key
        {
            return Err(NativeExecutionError::NumaCalibrationUnavailable);
        }
        let mut topology = Self::derive(profile, policy)?;
        topology.numa_steal_policy =
            derive_numa_steal_policy(profile, policy, calibration, &topology.pools)?;
        Ok(topology)
    }

    /// Returns the number of persistent workers.
    pub fn worker_count(&self) -> usize {
        self.pools.iter().map(|pool| pool.workers.len()).sum()
    }
}

fn derive_discovered_pools(
    profile: &HardwareProfile,
    worker_count: usize,
) -> Result<Vec<NativeNumaPoolTopology>, NativeExecutionError> {
    let mut seen = BTreeSet::new();
    for processor in &profile.cpu.processor_topology {
        if !seen.insert(processor.logical_id) {
            return Err(NativeExecutionError::DuplicateProcessor(
                processor.logical_id,
            ));
        }
    }
    let processors = profile
        .cpu
        .processor_topology
        .iter()
        .map(|processor| (processor.logical_id, processor))
        .collect::<BTreeMap<_, _>>();
    let candidates = physical_core_first_processor_order(profile)
        .ok_or(NativeExecutionError::InsufficientTopology)?;
    if candidates.len() < worker_count {
        return Err(NativeExecutionError::InsufficientTopology);
    }
    let mut by_node = BTreeMap::<Option<u32>, Vec<NativeWorkerPlacement>>::new();
    for (worker_index, (logical_id, smt_rank)) in
        candidates.into_iter().take(worker_count).enumerate()
    {
        let processor = processors
            .get(&logical_id)
            .ok_or(NativeExecutionError::InsufficientTopology)?;
        by_node
            .entry(processor.numa_node_id)
            .or_default()
            .push(NativeWorkerPlacement {
                worker_index,
                numa_node_id: processor.numa_node_id,
                logical_processor_id: Some(logical_id),
                socket_id: Some(processor.socket_id),
                core_id: Some(processor.core_id),
                smt_rank: Some(smt_rank),
            });
    }
    let mut pools = by_node
        .into_iter()
        .map(|(numa_node_id, workers)| NativeNumaPoolTopology {
            numa_node_id,
            workers,
        })
        .collect::<Vec<_>>();
    for (worker_index, worker) in pools
        .iter_mut()
        .flat_map(|pool| pool.workers.iter_mut())
        .enumerate()
    {
        worker.worker_index = worker_index;
    }
    Ok(pools)
}

fn disabled_numa_steal_policy(
    policy: &NativeGovernorPolicy,
    pools: &[NativeNumaPoolTopology],
) -> NativeNumaStealPolicy {
    let applicable = pools.len() > 1 && pools.iter().all(|pool| pool.numa_node_id.is_some());
    NativeNumaStealPolicy {
        schema: NUMA_STEAL_POLICY_SCHEMA.to_owned(),
        calibration_cache_key: policy.calibration_cache_key.clone(),
        status: if applicable {
            "disabled"
        } else {
            "not-applicable"
        }
        .to_owned(),
        working_set_bytes: NUMA_CALIBRATION_WORKING_SET_BYTES,
        foreground_burst_limit: policy.foreground_burst_limit,
        pools: pools
            .iter()
            .map(|pool| NativeNumaStealPoolPolicy {
                worker_numa_node_id: pool.numa_node_id,
                steal_targets: Vec::new(),
            })
            .collect(),
    }
}

fn derive_numa_steal_policy(
    profile: &HardwareProfile,
    policy: &NativeGovernorPolicy,
    calibration: &HardwareCalibration,
    pools: &[NativeNumaPoolTopology],
) -> Result<NativeNumaStealPolicy, NativeExecutionError> {
    if pools.len() <= 1 || pools.iter().any(|pool| pool.numa_node_id.is_none()) {
        return Ok(disabled_numa_steal_policy(policy, pools));
    }
    if calibration.identity.hardware_fingerprint != profile.fingerprint
        || calibration.identity.cache_key != policy.calibration_cache_key
        || calibration.status != "stable"
        || !calibration.accepted_for_scheduling
    {
        return Err(NativeExecutionError::NumaCalibrationUnavailable);
    }
    let numa_measurements = calibration
        .measurements
        .iter()
        .filter(|measurement| measurement.primitive == "numa-memory-read")
        .count();
    let numa_unsupported = calibration
        .coverage
        .unsupported
        .iter()
        .any(|entry| entry.primitive == "numa-local-remote-memory");
    if numa_measurements == 0 && numa_unsupported {
        return Ok(disabled_numa_steal_policy(policy, pools));
    }
    if numa_measurements == 0 || numa_unsupported {
        return Err(NativeExecutionError::NumaCalibrationUnavailable);
    }
    let nodes = pools
        .iter()
        .map(|pool| pool.numa_node_id)
        .collect::<Option<BTreeSet<_>>>()
        .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?;
    let mut matrix = BTreeMap::new();
    for measurement in calibration
        .measurements
        .iter()
        .filter(|measurement| measurement.primitive == "numa-memory-read")
    {
        let (source, reader) = parse_numa_variant(&measurement.variant)
            .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?;
        if !nodes.contains(&source) || !nodes.contains(&reader) {
            continue;
        }
        if measurement.input_size != NUMA_CALIBRATION_WORKING_SET_BYTES
            || measurement.input_unit != "working-set-bytes"
            || measurement.bytes_per_operation != NUMA_CALIBRATION_WORKING_SET_BYTES
            || measurement.statistics.unit != "picoseconds_per_operation"
            || measurement.status != "stable"
            || measurement.correctness.status != "passed"
            || measurement.correctness.result_digest_blake3
                != measurement.correctness.reference_digest_blake3
            || matrix
                .insert((source, reader), measurement.statistics.median)
                .is_some()
        {
            return Err(NativeExecutionError::NumaCalibrationUnavailable);
        }
    }
    let expected = nodes
        .iter()
        .flat_map(|source| nodes.iter().map(move |reader| (*source, *reader)))
        .collect::<BTreeSet<_>>();
    if matrix.keys().copied().collect::<BTreeSet<_>>() != expected {
        return Err(NativeExecutionError::NumaCalibrationUnavailable);
    }
    if !calibration_has_safe_numa_residency_evidence(calibration) {
        return Err(NativeExecutionError::NumaCalibrationUnavailable);
    }

    let mut pool_policies = Vec::with_capacity(pools.len());
    for pool in pools {
        let worker_node = pool
            .numa_node_id
            .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?;
        pool_policies.push(NativeNumaStealPoolPolicy {
            worker_numa_node_id: Some(worker_node),
            steal_targets: derive_numa_steal_targets(&matrix, &nodes, worker_node)?,
        });
    }
    Ok(NativeNumaStealPolicy {
        schema: NUMA_STEAL_POLICY_SCHEMA.to_owned(),
        calibration_cache_key: calibration.identity.cache_key.clone(),
        status: "calibrated".to_owned(),
        working_set_bytes: NUMA_CALIBRATION_WORKING_SET_BYTES,
        foreground_burst_limit: policy.foreground_burst_limit,
        pools: pool_policies,
    })
}

fn calibration_has_safe_numa_residency_evidence(_calibration: &HardwareCalibration) -> bool {
    // The v1 receipt has no exact-VMA pre/post residency proof. First-touch
    // timing must remain unusable until that evidence is versioned.
    false
}

fn derive_numa_steal_targets(
    matrix: &BTreeMap<(u32, u32), u64>,
    nodes: &BTreeSet<u32>,
    worker_node: u32,
) -> Result<Vec<NativeNumaStealTarget>, NativeExecutionError> {
    let mut targets = Vec::with_capacity(nodes.len().saturating_sub(1));
    for home_node in nodes.iter().copied().filter(|node| *node != worker_node) {
        let local = matrix
            .get(&(home_node, home_node))
            .copied()
            .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?;
        let remote = matrix
            .get(&(home_node, worker_node))
            .copied()
            .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?;
        if local == 0 || remote <= local {
            return Err(NativeExecutionError::NumaCalibrationUnavailable);
        }
        let ratio = u128::from(remote)
            .checked_mul(1_000_000)
            .and_then(|value| value.checked_add(u128::from(local).saturating_sub(1)))
            .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?
            / u128::from(local);
        let delay = u128::from(
            remote
                .checked_sub(local)
                .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?,
        )
        .checked_add(999)
        .ok_or(NativeExecutionError::NumaCalibrationUnavailable)?
            / 1_000;
        targets.push(NativeNumaStealTarget {
            home_numa_node_id: home_node,
            remote_to_local_latency_ppm: u64::try_from(ratio)
                .map_err(|_| NativeExecutionError::NumaCalibrationUnavailable)?,
            steal_after_nanoseconds: u64::try_from(delay)
                .map_err(|_| NativeExecutionError::NumaCalibrationUnavailable)?,
        });
    }
    targets.sort_by_key(|target| (target.steal_after_nanoseconds, target.home_numa_node_id));
    Ok(targets)
}

fn parse_numa_variant(variant: &str) -> Option<(u32, u32)> {
    let rest = variant.strip_prefix("linux-first-touch-node-")?;
    let (source, rest) = rest.split_once("-read-node-")?;
    let (reader, cpu) = rest.split_once("-cpu-")?;
    let _cpu = cpu.parse::<u32>().ok()?;
    Some((source.parse().ok()?, reader.parse().ok()?))
}

struct WorkQueue {
    jobs: Mutex<VecDeque<Job>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetedSingleTicket {
    worker_index: usize,
    generation: u64,
}

struct TargetedSingleJob {
    ticket: TargetedSingleTicket,
    job: Job,
}

struct WorkerWake {
    changed: Condvar,
}

struct WakeState {
    foreground_dispatches_since_background: Vec<u64>,
    high_dispatches_since_normal: Vec<u64>,
    next_worker_by_pool: Vec<usize>,
    targeted_slots: Vec<Option<TargetedSingleJob>>,
    targeted_generations: Vec<u64>,
    last_dispatch_was_targeted: Vec<bool>,
    waiting_by_worker: Vec<bool>,
}

struct ExecutionInner {
    queues: Vec<WorkQueue>,
    worker_wakes: Vec<WorkerWake>,
    pool_worker_ordinals: Vec<Vec<usize>>,
    wake_lock: Mutex<WakeState>,
    steal_policy: NativeNumaStealPolicy,
    shutdown: AtomicBool,
    completed_jobs: AtomicU64,
    local_dispatches: AtomicU64,
    stolen_dispatches: AtomicU64,
    #[cfg(test)]
    test_probe: ExecutionTestProbe,
}

#[cfg(test)]
type PreWaitHook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

#[cfg(test)]
struct ExecutionTestProbe {
    waiting_workers: Vec<std::sync::atomic::AtomicUsize>,
    notified_wake_returns: Vec<AtomicU64>,
    pre_wait_hooks: Vec<PreWaitHook>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetedSingleExecutionRoute {
    Targeted,
    GenericFallbackBusy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetedSingleExecutionReceipt {
    pub(crate) route: TargetedSingleExecutionRoute,
    pub(crate) worker_index: usize,
}

#[derive(Debug, Error)]
pub(crate) enum TargetedSingleExecutionError {
    #[error(transparent)]
    Execution(#[from] NativeExecutionError),
    #[error("targeted single-item execution was cancelled")]
    Cancelled,
    #[error("targeted single-item execution admission is closed")]
    Closed,
    #[error("targeted single-item cancellation belongs to another governor")]
    ForeignCancellation,
    #[error("targeted single-item generation is exhausted")]
    GenerationExhausted,
}

struct TargetedSubmission {
    route: TargetedSingleExecutionRoute,
    ticket: Option<TargetedSingleTicket>,
}

#[cfg(test)]
struct TargetedTestHandle<R> {
    ticket: TargetedSingleTicket,
    result: mpsc::Receiver<R>,
    completed: mpsc::Receiver<bool>,
}

fn new_wake_state(pool_count: usize, worker_count: usize) -> WakeState {
    WakeState {
        foreground_dispatches_since_background: vec![0; pool_count],
        high_dispatches_since_normal: vec![0; pool_count],
        next_worker_by_pool: vec![0; pool_count],
        targeted_slots: (0..worker_count).map(|_| None).collect(),
        targeted_generations: vec![0; worker_count],
        last_dispatch_was_targeted: vec![false; worker_count],
        waiting_by_worker: vec![false; worker_count],
    }
}

#[cfg(test)]
impl ExecutionTestProbe {
    fn new(pool_count: usize, worker_count: usize) -> Self {
        Self {
            waiting_workers: (0..pool_count)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            notified_wake_returns: (0..pool_count).map(|_| AtomicU64::new(0)).collect(),
            pre_wait_hooks: (0..worker_count).map(|_| Mutex::new(None)).collect(),
        }
    }
}

/// Persistent physical-core-first workers with NUMA-local queues and stealing.
pub struct NativeExecutionPool {
    topology: NativeExecutionTopology,
    inner: Arc<ExecutionInner>,
    workers: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for NativeExecutionPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeExecutionPool")
            .field("topology", &self.topology)
            .finish_non_exhaustive()
    }
}

impl NativeExecutionPool {
    /// Creates and, where supported, hard-binds every persistent worker.
    ///
    /// # Errors
    ///
    /// Returns a topology, thread-creation, or affinity error without leaving
    /// a partially live pool.
    pub fn new(
        profile: &HardwareProfile,
        policy: &NativeGovernorPolicy,
    ) -> Result<Self, NativeExecutionError> {
        let topology = NativeExecutionTopology::derive(profile, policy)?;
        Self::from_topology(topology)
    }

    /// Creates a pool whose cross-node policy is authorized by calibration.
    ///
    /// # Errors
    ///
    /// In addition to normal pool construction failures, multi-node execution
    /// rejects incomplete, unstable, or cross-identity NUMA evidence. Explicit
    /// unsupported coverage is accepted only with remote stealing disabled.
    pub fn new_with_calibration(
        profile: &HardwareProfile,
        policy: &NativeGovernorPolicy,
        calibration: &HardwareCalibration,
    ) -> Result<Self, NativeExecutionError> {
        let topology =
            NativeExecutionTopology::derive_with_calibration(profile, policy, calibration)?;
        Self::from_topology(topology)
    }

    fn from_topology(topology: NativeExecutionTopology) -> Result<Self, NativeExecutionError> {
        let pool_count = topology.pools.len();
        let worker_count = topology.worker_count();
        if worker_count == 0 {
            return Err(NativeExecutionError::EmptyExecution);
        }
        let pool_worker_ordinals = topology
            .pools
            .iter()
            .map(|pool| {
                pool.workers
                    .iter()
                    .map(|worker| worker.worker_index)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let inner = Arc::new(ExecutionInner {
            queues: topology
                .pools
                .iter()
                .map(|_| WorkQueue {
                    jobs: Mutex::new(VecDeque::new()),
                })
                .collect(),
            worker_wakes: (0..worker_count)
                .map(|_| WorkerWake {
                    changed: Condvar::new(),
                })
                .collect(),
            pool_worker_ordinals,
            wake_lock: Mutex::new(new_wake_state(pool_count, worker_count)),
            steal_policy: topology.numa_steal_policy.clone(),
            shutdown: AtomicBool::new(false),
            completed_jobs: AtomicU64::new(0),
            local_dispatches: AtomicU64::new(0),
            stolen_dispatches: AtomicU64::new(0),
            #[cfg(test)]
            test_probe: ExecutionTestProbe::new(pool_count, worker_count),
        });
        let (started_tx, started_rx) = mpsc::channel();
        let mut workers = Vec::with_capacity(worker_count);
        for (pool_index, placement) in
            topology
                .pools
                .iter()
                .enumerate()
                .flat_map(|(pool_index, pool)| {
                    pool.workers
                        .iter()
                        .cloned()
                        .map(move |worker| (pool_index, worker))
                })
        {
            let worker_inner = Arc::clone(&inner);
            let worker_started = started_tx.clone();
            let name = format!("hyphae-numa-{pool_index}-{}", placement.worker_index);
            let handle = match thread::Builder::new().name(name).spawn(move || {
                #[cfg(target_os = "linux")]
                let affinity = bind_worker(&placement);
                #[cfg(not(target_os = "linux"))]
                let affinity = Ok(());
                let affinity_ok = affinity.is_ok();
                let _ignored = worker_started.send(affinity);
                if affinity_ok {
                    run_worker(&worker_inner, pool_index, placement.worker_index);
                }
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    stop_and_join(&inner, workers);
                    return Err(NativeExecutionError::ThreadSpawn(error));
                }
            };
            workers.push(handle);
        }
        drop(started_tx);
        let mut startup_error = None;
        for _ in 0..worker_count {
            match started_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    startup_error.get_or_insert(error);
                }
                Err(_) => {
                    startup_error.get_or_insert(NativeExecutionError::Synchronization);
                }
            }
        }
        if let Some(error) = startup_error {
            stop_and_join(&inner, workers);
            return Err(error);
        }
        Ok(Self {
            topology,
            inner,
            workers,
        })
    }

    /// Returns immutable worker-placement evidence.
    pub const fn topology(&self) -> &NativeExecutionTopology {
        &self.topology
    }

    /// Returns the non-transactional number of completed worker batches.
    pub fn completed_jobs(&self) -> u64 {
        self.inner.completed_jobs.load(Ordering::Acquire)
    }

    /// Returns the non-transactional number of NUMA-local dispatches.
    pub fn local_dispatches(&self) -> u64 {
        self.inner.local_dispatches.load(Ordering::Acquire)
    }

    /// Returns the non-transactional number of calibrated remote steals.
    pub fn stolen_dispatches(&self) -> u64 {
        self.inner.stolen_dispatches.load(Ordering::Acquire)
    }

    /// Verifies that this pool can execute allocations from `policy`.
    ///
    /// # Errors
    ///
    /// Rejects another hardware fingerprint or worker budget.
    pub fn validate_policy(
        &self,
        policy: &NativeGovernorPolicy,
    ) -> Result<(), NativeExecutionError> {
        if self.topology.hardware_fingerprint != policy.hardware_fingerprint {
            return Err(NativeExecutionError::HardwareIdentityMismatch);
        }
        if u64::try_from(self.topology.worker_count()).ok()
            != Some(policy.schedulable_compute_threads)
        {
            return Err(NativeExecutionError::PolicyWorkerMismatch);
        }
        Ok(())
    }

    /// Executes deterministic batches under owned subdivisions of one parent.
    ///
    /// Result order always matches input order. Work is split across no more
    /// workers than the parent compute allocation, so a nested caller cannot
    /// oversubscribe global governor capacity.
    ///
    /// # Errors
    ///
    /// Rejects empty work, insufficient parent capacity, synchronization
    /// failure, or a panicking operation.
    pub fn execute_ordered<T, R, F>(
        &self,
        parent: &OwnedGovernorPermit,
        items: Vec<T>,
        operation: F,
    ) -> Result<Vec<R>, NativeExecutionError>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + 'static,
    {
        self.execute_ordered_profiled(parent, items, operation)
            .map(|(results, _worker_batches)| results)
    }

    pub(crate) fn execute_ordered_profiled<T, R, F>(
        &self,
        parent: &OwnedGovernorPermit,
        items: Vec<T>,
        operation: F,
    ) -> Result<(Vec<R>, usize), NativeExecutionError>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + 'static,
    {
        self.execute_ordered_profiled_with_child_request(
            parent,
            items,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
            operation,
        )
    }

    pub(crate) fn execute_ordered_profiled_with_child_request<T, R, F>(
        &self,
        parent: &OwnedGovernorPermit,
        items: Vec<T>,
        child_request: GovernorRequest,
        operation: F,
    ) -> Result<(Vec<R>, usize), NativeExecutionError>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + 'static,
    {
        if items.is_empty() {
            return Err(NativeExecutionError::EmptyExecution);
        }
        let parent_workers =
            usize::try_from(nested_request_capacity(parent.request(), child_request)?)
                .map_err(|_| NativeExecutionError::InsufficientTopology)?;
        let worker_count = items
            .len()
            .min(parent_workers)
            .min(self.topology.worker_count());
        if worker_count == 0 {
            return Err(NativeExecutionError::EmptyExecution);
        }
        let item_count = items.len();
        let mut batches = (0..worker_count)
            .map(|_| Vec::new())
            .collect::<Vec<Vec<(usize, T)>>>();
        for (index, item) in items.into_iter().enumerate() {
            batches[index % worker_count].push((index, item));
        }
        let operation = Arc::new(operation);
        let results = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(item_count)
                .collect::<Vec<Option<R>>>(),
        ));
        let (completed_tx, completed_rx) = mpsc::channel();
        let children = (0..worker_count)
            .map(|_| parent.try_subdivide_owned(child_request))
            .collect::<Result<Vec<_>, GovernorAdmissionError>>()?;
        for (pool_hint, (batch, child)) in batches.into_iter().zip(children).enumerate() {
            let batch_operation = Arc::clone(&operation);
            let batch_results = Arc::clone(&results);
            let job = Job {
                class: parent.class(),
                enqueued_at: Instant::now(),
                operation: Box::new(move || {
                    for (index, item) in batch {
                        let result = batch_operation(item);
                        let mut destination = batch_results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        destination[index] = Some(result);
                    }
                    drop(child);
                }),
                completed: Some(completed_tx.clone()),
                targeted_completion: None,
            };
            self.submit(pool_hint, job);
        }
        drop(completed_tx);
        let mut operation_panicked = false;
        for _ in 0..worker_count {
            operation_panicked |= !completed_rx
                .recv()
                .map_err(|_| NativeExecutionError::Synchronization)?;
        }
        if operation_panicked {
            return Err(NativeExecutionError::OperationPanicked);
        }
        let mut guard = results
            .lock()
            .map_err(|_| NativeExecutionError::Synchronization)?;
        let ordered = guard
            .iter_mut()
            .map(|result| result.take().ok_or(NativeExecutionError::Synchronization))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((ordered, worker_count))
    }

    pub(crate) fn execute_single_targeted_profiled<R>(
        &self,
        parent: &OwnedGovernorPermit,
        stable_hint: usize,
        cancellation: Option<&crate::GovernorCancellation>,
        operation: impl FnOnce() -> R + Send + 'static,
    ) -> Result<(R, TargetedSingleExecutionReceipt), TargetedSingleExecutionError>
    where
        R: Send + 'static,
    {
        if cancellation.is_some_and(|token| !parent.owns_cancellation(token)) {
            return Err(TargetedSingleExecutionError::ForeignCancellation);
        }
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(TargetedSingleExecutionError::Cancelled);
        }
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(TargetedSingleExecutionError::Closed);
        }
        let child = parent
            .try_subdivide_owned(GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            })
            .map_err(NativeExecutionError::Admission)?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let completion_state = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel();
        let job = Job {
            class: parent.class(),
            enqueued_at: Instant::now(),
            operation: Box::new(move || {
                let result = operation();
                let _ignored = result_tx.send(result);
                drop(child);
            }),
            completed: None,
            targeted_completion: Some(TargetedCompletion {
                state: Arc::clone(&completion_state),
                event: event_tx.clone(),
            }),
        };
        let submission = self.submit_single_targeted(stable_hint, job)?;
        let operation_completed = self.wait_for_targeted_completion(
            submission.ticket,
            cancellation,
            &completion_state,
            event_tx,
            &event_rx,
        )?;
        if !operation_completed {
            return Err(NativeExecutionError::OperationPanicked.into());
        }
        let result = result_rx
            .recv()
            .map_err(|_| NativeExecutionError::Synchronization)?;
        let worker_index = completion_state
            .lock()
            .map_err(|_| NativeExecutionError::Synchronization)?
            .as_ref()
            .map(|(_, worker_index)| *worker_index)
            .ok_or(NativeExecutionError::Synchronization)?;
        Ok((
            result,
            TargetedSingleExecutionReceipt {
                route: submission.route,
                worker_index,
            },
        ))
    }

    fn submit_single_targeted(
        &self,
        stable_hint: usize,
        job: Job,
    ) -> Result<TargetedSubmission, TargetedSingleExecutionError> {
        let mut wake = self
            .inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(TargetedSingleExecutionError::Closed);
        }
        let worker_index = stable_hint % self.topology.worker_count();
        let home_pool = worker_pool_index(&self.topology, worker_index)?;
        if wake.targeted_slots[worker_index].is_some() {
            self.submit_locked(&mut wake, home_pool, job);
            return Ok(TargetedSubmission {
                route: TargetedSingleExecutionRoute::GenericFallbackBusy,
                ticket: None,
            });
        }
        let generation = wake.targeted_generations[worker_index]
            .checked_add(1)
            .ok_or(TargetedSingleExecutionError::GenerationExhausted)?;
        wake.targeted_generations[worker_index] = generation;
        let ticket = TargetedSingleTicket {
            worker_index,
            generation,
        };
        wake.targeted_slots[worker_index] = Some(TargetedSingleJob { ticket, job });
        self.inner.worker_wakes[worker_index].changed.notify_one();
        Ok(TargetedSubmission {
            route: TargetedSingleExecutionRoute::Targeted,
            ticket: Some(ticket),
        })
    }

    fn wait_for_targeted_completion(
        &self,
        ticket: Option<TargetedSingleTicket>,
        cancellation: Option<&crate::GovernorCancellation>,
        state: &Arc<Mutex<Option<(bool, usize)>>>,
        event_tx: mpsc::Sender<()>,
        events: &mpsc::Receiver<()>,
    ) -> Result<bool, TargetedSingleExecutionError> {
        let cancellation_registration = cancellation
            .map(|cancellation| cancellation.register_waiter(event_tx))
            .transpose()
            .map_err(|()| NativeExecutionError::Synchronization)?;
        let _cancellation_waiter = match cancellation_registration {
            Some(crate::governor::CancellationWaiterRegistration::Registered(waiter)) => {
                Some(waiter)
            }
            Some(crate::governor::CancellationWaiterRegistration::Cancelled) | None => None,
        };
        loop {
            events
                .recv()
                .map_err(|_| NativeExecutionError::Synchronization)?;
            if let Some((outcome, _)) = *state
                .lock()
                .map_err(|_| NativeExecutionError::Synchronization)?
            {
                return Ok(outcome);
            }
            if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled)
                && ticket.is_some_and(|ticket| cancel_targeted_ticket(&self.inner, ticket))
            {
                return Err(TargetedSingleExecutionError::Cancelled);
            }
        }
    }

    fn submit(&self, pool_hint: usize, job: Job) {
        debug_assert!(!self.inner.shutdown.load(Ordering::Acquire));
        let mut wake = self
            .inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pool_index = pool_hint % self.inner.queues.len();
        self.submit_locked(&mut wake, pool_index, job);
    }

    fn submit_locked(&self, wake: &mut WakeState, pool_index: usize, job: Job) {
        self.inner.queues[pool_index]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(job);
        notify_eligible_pool_workers(&self.inner, wake, pool_index);
    }

    #[cfg(test)]
    pub(crate) fn wait_for_targeted_slot_for_test(
        &self,
        stable_hint: usize,
        timeout: Duration,
    ) -> Result<(), NativeExecutionError> {
        let worker_index = stable_hint % self.topology.worker_count();
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .inner
                .wake_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .targeted_slots[worker_index]
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(NativeExecutionError::Synchronization);
            }
            thread::yield_now();
        }
    }

    #[cfg(test)]
    fn wait_for_targeted_ticket_for_test(
        &self,
        stable_hint: usize,
        timeout: Duration,
    ) -> Result<TargetedSingleTicket, NativeExecutionError> {
        self.wait_for_targeted_slot_for_test(stable_hint, timeout)?;
        let worker_index = stable_hint % self.topology.worker_count();
        self.inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .targeted_slots[worker_index]
            .as_ref()
            .map(|job| job.ticket)
            .ok_or(NativeExecutionError::Synchronization)
    }

    #[cfg(test)]
    fn enqueue_targeted_for_test<R>(
        &self,
        parent: &OwnedGovernorPermit,
        stable_hint: usize,
        operation: impl FnOnce() -> R + Send + 'static,
    ) -> Result<TargetedTestHandle<R>, TargetedSingleExecutionError>
    where
        R: Send + 'static,
    {
        let child = parent
            .try_subdivide_owned(GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            })
            .map_err(NativeExecutionError::Admission)?;
        let (result_tx, result_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let job = Job {
            class: parent.class(),
            enqueued_at: Instant::now(),
            operation: Box::new(move || {
                let result = operation();
                let _ignored = result_tx.send(result);
                drop(child);
            }),
            completed: Some(completed_tx),
            targeted_completion: None,
        };
        let submission = self.submit_single_targeted(stable_hint, job)?;
        let ticket = submission
            .ticket
            .ok_or(NativeExecutionError::Synchronization)?;
        Ok(TargetedTestHandle {
            ticket,
            result: result_rx,
            completed: completed_rx,
        })
    }

    #[cfg(test)]
    fn wait_targeted_for_test<R>(
        handle: &TargetedTestHandle<R>,
    ) -> Result<(R, TargetedSingleExecutionReceipt), TargetedSingleExecutionError> {
        if !handle
            .completed
            .recv()
            .map_err(|_| NativeExecutionError::Synchronization)?
        {
            return Err(NativeExecutionError::OperationPanicked.into());
        }
        let result = handle
            .result
            .recv()
            .map_err(|_| NativeExecutionError::Synchronization)?;
        Ok((
            result,
            TargetedSingleExecutionReceipt {
                route: TargetedSingleExecutionRoute::Targeted,
                worker_index: handle.ticket.worker_index,
            },
        ))
    }

    #[cfg(test)]
    fn cancel_targeted_ticket_for_test(&self, ticket: TargetedSingleTicket) -> bool {
        cancel_targeted_ticket(&self.inner, ticket)
    }

    #[cfg(test)]
    fn set_targeted_generation_for_test(&self, worker_index: usize, generation: u64) {
        self.inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .targeted_generations[worker_index] = generation;
    }

    #[cfg(test)]
    fn targeted_generation_for_test(&self, worker_index: usize) -> u64 {
        self.inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .targeted_generations[worker_index]
    }

    #[cfg(test)]
    fn targeted_slot_occupied_for_test(&self, worker_index: usize) -> bool {
        self.inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .targeted_slots[worker_index]
            .is_some()
    }

    #[cfg(test)]
    fn wait_for_generic_queue_for_test(
        &self,
        pool_index: usize,
        timeout: Duration,
    ) -> Result<(), NativeExecutionError> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.inner.queues[pool_index]
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(NativeExecutionError::Synchronization);
            }
            thread::yield_now();
        }
    }

    #[cfg(test)]
    fn install_targeted_pre_wait_hook_for_test(
        &self,
        worker_index: usize,
        waiting: mpsc::SyncSender<()>,
        release: impl FnOnce() + Send + 'static,
    ) {
        let _wake = self
            .inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self.inner.test_probe.pre_wait_hooks[worker_index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            let _ignored = waiting.send(());
            release();
        }));
        self.inner.worker_wakes[worker_index].changed.notify_one();
    }

    #[cfg(test)]
    fn signal_shutdown_for_test(&self) {
        signal_shutdown(&self.inner);
    }

    #[cfg(test)]
    fn shutdown_and_join_for_test(mut self) {
        signal_shutdown(&self.inner);
        for worker in self.workers.drain(..) {
            let _ignored = worker.join();
        }
    }
}

fn notify_eligible_pool_workers(inner: &ExecutionInner, wake: &mut WakeState, home_pool: usize) {
    notify_next_pool_worker(inner, wake, home_pool);
    if inner.steal_policy.status != "calibrated" {
        return;
    }
    let Some(home_node) = inner.steal_policy.pools[home_pool].worker_numa_node_id else {
        return;
    };
    for (candidate_pool, policy) in inner.steal_policy.pools.iter().enumerate() {
        if candidate_pool != home_pool
            && policy
                .steal_targets
                .iter()
                .any(|target| target.home_numa_node_id == home_node)
        {
            notify_next_pool_worker(inner, wake, candidate_pool);
        }
    }
}

fn notify_next_pool_worker(inner: &ExecutionInner, wake: &mut WakeState, pool_index: usize) {
    let workers = &inner.pool_worker_ordinals[pool_index];
    if workers.is_empty() {
        return;
    }
    let offset = wake.next_worker_by_pool[pool_index] % workers.len();
    let selected_offset = (0..workers.len())
        .map(|delta| (offset + delta) % workers.len())
        .find(|candidate| wake.waiting_by_worker[workers[*candidate]])
        .unwrap_or(offset);
    wake.next_worker_by_pool[pool_index] = (selected_offset + 1) % workers.len();
    inner.worker_wakes[workers[selected_offset]]
        .changed
        .notify_one();
}

fn nested_request_capacity(
    parent: GovernorRequest,
    child: GovernorRequest,
) -> Result<u64, GovernorAdmissionError> {
    let requests = [
        (parent.compute_threads, child.compute_threads),
        (parent.io_slots, child.io_slots),
        (parent.memory_bytes, child.memory_bytes),
    ];
    if requests.iter().all(|(_, requested)| *requested == 0) {
        return Err(GovernorAdmissionError::EmptyRequest);
    }
    let capacity = requests
        .into_iter()
        .filter_map(|(available, requested)| available.checked_div(requested))
        .min()
        .unwrap_or(0);
    if capacity == 0 {
        Err(GovernorAdmissionError::ParentCapacity)
    } else {
        Ok(capacity)
    }
}

impl Drop for NativeExecutionPool {
    fn drop(&mut self) {
        signal_shutdown(&self.inner);
        for worker in self.workers.drain(..) {
            let _ignored = worker.join();
        }
    }
}

fn run_worker(inner: &ExecutionInner, local_pool: usize, worker_index: usize) {
    let mut wake = inner
        .wake_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let now = Instant::now();
        if let Some(job) = take_worker_job(inner, &mut wake, local_pool, worker_index, now) {
            drop(wake);
            complete_job(inner, worker_index, job);
            wake = inner
                .wake_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        }
        if inner.shutdown.load(Ordering::Acquire)
            && !has_any_accepted_job(inner, &wake, worker_index)
        {
            break;
        }
        record_worker_wait_started(inner, local_pool);
        wake.waiting_by_worker[worker_index] = true;
        run_pre_wait_hook(inner, worker_index);
        let (next_wake, notified) = if let Some(wait) = next_steal_wait(inner, local_pool, now) {
            let (next_wake, timeout) = inner.worker_wakes[worker_index]
                .changed
                .wait_timeout(wake, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (next_wake, !timeout.timed_out())
        } else {
            (
                inner.worker_wakes[worker_index]
                    .changed
                    .wait(wake)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                true,
            )
        };
        wake = next_wake;
        wake.waiting_by_worker[worker_index] = false;
        record_worker_wait_finished(inner, local_pool, notified);
    }
}

fn take_worker_job(
    inner: &ExecutionInner,
    wake: &mut WakeState,
    local_pool: usize,
    worker_index: usize,
    now: Instant,
) -> Option<Job> {
    let generic_eligible = has_eligible_job(inner, local_pool, now, |_| true);
    if !wake.last_dispatch_was_targeted[worker_index]
        && let Some(targeted) = wake.targeted_slots[worker_index].take()
    {
        wake.last_dispatch_was_targeted[worker_index] = true;
        inner.local_dispatches.fetch_add(1, Ordering::AcqRel);
        return Some(targeted.job);
    }
    if generic_eligible && let Some(job) = take_job(inner, wake, local_pool, now) {
        wake.last_dispatch_was_targeted[worker_index] = false;
        return Some(job);
    }
    if let Some(targeted) = wake.targeted_slots[worker_index].take() {
        wake.last_dispatch_was_targeted[worker_index] = true;
        inner.local_dispatches.fetch_add(1, Ordering::AcqRel);
        return Some(targeted.job);
    }
    take_job(inner, wake, local_pool, now).inspect(|_| {
        wake.last_dispatch_was_targeted[worker_index] = false;
    })
}

fn has_any_accepted_job(inner: &ExecutionInner, wake: &WakeState, worker_index: usize) -> bool {
    wake.targeted_slots[worker_index].is_some()
        || !inner.queues[worker_pool_for_inner(inner, worker_index)]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
}

fn worker_pool_for_inner(inner: &ExecutionInner, worker_index: usize) -> usize {
    inner
        .pool_worker_ordinals
        .iter()
        .position(|workers| workers.contains(&worker_index))
        .unwrap_or(0)
}

#[cfg(test)]
fn run_pre_wait_hook(inner: &ExecutionInner, worker_index: usize) {
    let hook = inner.test_probe.pre_wait_hooks[worker_index]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn run_pre_wait_hook(_inner: &ExecutionInner, _worker_index: usize) {}

#[cfg(test)]
fn record_worker_wait_started(inner: &ExecutionInner, local_pool: usize) {
    inner.test_probe.waiting_workers[local_pool].fetch_add(1, Ordering::AcqRel);
}

#[cfg(not(test))]
fn record_worker_wait_started(_inner: &ExecutionInner, _local_pool: usize) {}

#[cfg(test)]
fn record_worker_wait_finished(inner: &ExecutionInner, local_pool: usize, notified: bool) {
    inner.test_probe.waiting_workers[local_pool].fetch_sub(1, Ordering::AcqRel);
    if notified {
        inner.test_probe.notified_wake_returns[local_pool].fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(not(test))]
fn record_worker_wait_finished(_inner: &ExecutionInner, _local_pool: usize, _notified: bool) {}

fn complete_job(inner: &ExecutionInner, worker_index: usize, job: Job) {
    let outcome = catch_unwind(AssertUnwindSafe(job.operation)).is_ok();
    let targeted_event = job.targeted_completion.map(|completion| {
        *completion
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((outcome, worker_index));
        completion.event
    });
    inner.completed_jobs.fetch_add(1, Ordering::AcqRel);
    if let Some(event) = targeted_event {
        let _ignored = event.send(());
    }
    if let Some(completed) = job.completed {
        let _ignored = completed.send(outcome);
    }
}

fn take_job(
    inner: &ExecutionInner,
    wake: &mut WakeState,
    local_pool: usize,
    now: Instant,
) -> Option<Job> {
    let background_waiting = has_eligible_job(inner, local_pool, now, is_background);
    let normal_waiting = has_eligible_job(inner, local_pool, now, is_normal_priority);
    let burst_limit = inner.steal_policy.foreground_burst_limit;
    let force_background = background_waiting
        && wake.foreground_dispatches_since_background[local_pool] >= burst_limit;
    let force_normal =
        normal_waiting && wake.high_dispatches_since_normal[local_pool] >= burst_limit;
    let priorities: [fn(WorkloadClass) -> bool; 3] = if force_background {
        [is_background, is_high_priority, is_normal_priority]
    } else if force_normal {
        [is_normal_priority, is_high_priority, is_background]
    } else {
        [is_high_priority, is_normal_priority, is_background]
    };
    for predicate in priorities {
        if let Some((job, stolen)) = pop_eligible_job(inner, local_pool, now, predicate) {
            if is_foreground(job.class) && background_waiting {
                wake.foreground_dispatches_since_background[local_pool] = wake
                    .foreground_dispatches_since_background[local_pool]
                    .checked_add(1)
                    .unwrap_or(burst_limit)
                    .min(burst_limit);
            } else {
                wake.foreground_dispatches_since_background[local_pool] = 0;
            }
            if is_high_priority(job.class) && normal_waiting {
                wake.high_dispatches_since_normal[local_pool] = wake.high_dispatches_since_normal
                    [local_pool]
                    .checked_add(1)
                    .unwrap_or(burst_limit)
                    .min(burst_limit);
            } else if is_normal_priority(job.class) || !normal_waiting {
                wake.high_dispatches_since_normal[local_pool] = 0;
            }
            if stolen {
                inner.stolen_dispatches.fetch_add(1, Ordering::AcqRel);
            } else {
                inner.local_dispatches.fetch_add(1, Ordering::AcqRel);
            }
            return Some(job);
        }
    }
    None
}

fn has_eligible_job(
    inner: &ExecutionInner,
    local_pool: usize,
    now: Instant,
    predicate: fn(WorkloadClass) -> bool,
) -> bool {
    eligible_pools(inner, local_pool)
        .into_iter()
        .any(|(pool_index, delay)| {
            inner.queues[pool_index]
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|job| predicate(job.class) && job_age_at_least(job, now, delay))
        })
}

fn pop_eligible_job(
    inner: &ExecutionInner,
    local_pool: usize,
    now: Instant,
    predicate: fn(WorkloadClass) -> bool,
) -> Option<(Job, bool)> {
    eligible_pools(inner, local_pool)
        .into_iter()
        .find_map(|(pool_index, delay)| {
            let mut jobs = inner.queues[pool_index]
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let offset = jobs
                .iter()
                .position(|job| predicate(job.class) && job_age_at_least(job, now, delay))?;
            jobs.remove(offset)
                .map(|job| (job, pool_index != local_pool))
        })
}

fn eligible_pools(inner: &ExecutionInner, local_pool: usize) -> Vec<(usize, Duration)> {
    let mut pools = vec![(local_pool, Duration::ZERO)];
    if inner.steal_policy.status != "calibrated" {
        return pools;
    }
    let policy = &inner.steal_policy.pools[local_pool];
    for target in &policy.steal_targets {
        if let Some(pool_index) = inner
            .steal_policy
            .pools
            .iter()
            .position(|pool| pool.worker_numa_node_id == Some(target.home_numa_node_id))
        {
            pools.push((
                pool_index,
                Duration::from_nanos(target.steal_after_nanoseconds),
            ));
        }
    }
    pools
}

fn next_steal_wait(inner: &ExecutionInner, local_pool: usize, now: Instant) -> Option<Duration> {
    eligible_pools(inner, local_pool)
        .into_iter()
        .filter(|(pool_index, _)| *pool_index != local_pool)
        .flat_map(|(pool_index, delay)| {
            inner.queues[pool_index]
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter_map(move |job| {
                    let eligible_at = job.enqueued_at.checked_add(delay)?;
                    Some(eligible_at.saturating_duration_since(now))
                })
                .collect::<Vec<_>>()
        })
        .filter(|wait| !wait.is_zero())
        .min()
}

fn job_age_at_least(job: &Job, now: Instant, delay: Duration) -> bool {
    now.saturating_duration_since(job.enqueued_at) >= delay
}

const fn is_high_priority(class: WorkloadClass) -> bool {
    matches!(
        class,
        WorkloadClass::ForegroundPoint | WorkloadClass::Mutation
    )
}

const fn is_normal_priority(class: WorkloadClass) -> bool {
    matches!(class, WorkloadClass::ForegroundBounded)
}

const fn is_background(class: WorkloadClass) -> bool {
    matches!(
        class,
        WorkloadClass::Bulk
            | WorkloadClass::Maintenance
            | WorkloadClass::Recovery
            | WorkloadClass::Administrative
    )
}

const fn is_foreground(class: WorkloadClass) -> bool {
    is_high_priority(class) || is_normal_priority(class)
}

fn stop_and_join(inner: &ExecutionInner, workers: Vec<JoinHandle<()>>) {
    signal_shutdown(inner);
    for worker in workers {
        let _ignored = worker.join();
    }
}

fn signal_shutdown(inner: &ExecutionInner) {
    let wake = inner
        .wake_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    inner.shutdown.store(true, Ordering::Release);
    for worker in &inner.worker_wakes {
        worker.changed.notify_all();
    }
    drop(wake);
}

fn cancel_targeted_ticket(inner: &ExecutionInner, ticket: TargetedSingleTicket) -> bool {
    let mut wake = inner
        .wake_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if wake.targeted_slots[ticket.worker_index]
        .as_ref()
        .is_some_and(|job| job.ticket == ticket)
    {
        wake.targeted_slots[ticket.worker_index].take();
        true
    } else {
        false
    }
}

fn worker_pool_index(
    topology: &NativeExecutionTopology,
    worker_index: usize,
) -> Result<usize, NativeExecutionError> {
    topology
        .pools
        .iter()
        .position(|pool| {
            pool.workers
                .iter()
                .any(|worker| worker.worker_index == worker_index)
        })
        .ok_or(NativeExecutionError::InsufficientTopology)
}

#[cfg(test)]
fn worker_name(
    topology: &NativeExecutionTopology,
    worker_index: usize,
) -> Result<String, NativeExecutionError> {
    Ok(format!(
        "hyphae-numa-{}-{worker_index}",
        worker_pool_index(topology, worker_index)?
    ))
}

#[cfg(target_os = "linux")]
fn bind_worker(placement: &NativeWorkerPlacement) -> Result<(), NativeExecutionError> {
    let Some(logical_id) = placement.logical_processor_id else {
        return Ok(());
    };
    let mut cpu_set = CpuSet::new();
    cpu_set
        .set(usize::try_from(logical_id).map_err(|_| NativeExecutionError::InsufficientTopology)?)
        .map_err(|error| NativeExecutionError::Affinity {
            logical_id,
            reason: error.to_string(),
        })?;
    sched_setaffinity(Pid::from_raw(0), &cpu_set).map_err(|error| NativeExecutionError::Affinity {
        logical_id,
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CalibrationCacheStatus, CalibrationCorrectness, CalibrationCoverage,
        CalibrationFeatureDetection, CalibrationIdentity, CalibrationIoScaling,
        CalibrationMeasurement, CalibrationMode, CalibrationPolicy, CalibrationStatistics,
        CalibrationThreadScaling, GovernorClassLimit, GovernorMode, HardwareCpu, HardwareMemory,
        HardwareOperatingSystem, HardwareProcessor, HardwareStorage, NativeResourceGovernor,
        UnsupportedCalibration, WorkloadClass,
    };
    use std::{
        error::Error,
        sync::{Barrier, Condvar, Mutex, atomic::AtomicUsize},
    };

    #[derive(Default)]
    struct HomeWorkerReleaseGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl HomeWorkerReleaseGate {
        fn wait(&self) {
            let released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                self.changed
                    .wait_while(released, |released| !*released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }

        fn release(&self) {
            *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.changed.notify_all();
        }
    }

    struct HomeWorkerRelease {
        gate: Arc<HomeWorkerReleaseGate>,
    }

    impl HomeWorkerRelease {
        fn new() -> Self {
            Self {
                gate: Arc::new(HomeWorkerReleaseGate::default()),
            }
        }

        fn waiter(&self) -> Arc<HomeWorkerReleaseGate> {
            Arc::clone(&self.gate)
        }
    }

    impl Drop for HomeWorkerRelease {
        fn drop(&mut self) {
            self.gate.release();
        }
    }

    fn profile() -> HardwareProfile {
        HardwareProfile {
            schema: "hyphae-native-hardware-profile-v1".to_owned(),
            fingerprint: "1".repeat(64),
            cpu: HardwareCpu {
                architecture: std::env::consts::ARCH.to_owned(),
                logical_processors_available: 8,
                physical_cores_visible: Some(4),
                smt_threads_per_core: Some(2),
                sockets_visible: Some(2),
                numa_nodes_visible: Some(2),
                affinity: "0-7".to_owned(),
                quota_millicores: None,
                instruction_sets: Vec::new(),
                caches: Vec::new(),
                processor_topology: (0_u32..8)
                    .map(|logical_id| HardwareProcessor {
                        logical_id,
                        core_id: logical_id / 2,
                        socket_id: (logical_id / 4),
                        numa_node_id: Some(logical_id / 4),
                        thread_siblings: format!(
                            "{},{}",
                            logical_id - (logical_id % 2),
                            logical_id - (logical_id % 2) + 1
                        ),
                    })
                    .collect(),
                frequency_governors: Vec::new(),
            },
            memory: HardwareMemory {
                total_bytes: Some(1 << 30),
                available_bytes: Some(1 << 30),
                page_size_bytes: Some(4_096),
                huge_page_size_bytes: None,
                huge_pages_total: None,
                numa_nodes: Vec::new(),
            },
            storage: HardwareStorage {
                path: "/tmp".to_owned(),
                filesystem: None,
                device: None,
                mount_options: Vec::new(),
                rotational: None,
                queue_depth: None,
                discard_max_bytes: None,
            },
            operating_system: HardwareOperatingSystem {
                family: std::env::consts::OS.to_owned(),
                kernel_release: "test".to_owned(),
                virtualization: "none".to_owned(),
                local_transports: Vec::new(),
            },
        }
    }

    fn policy() -> NativeGovernorPolicy {
        let memory_bytes = 1 << 30;
        NativeGovernorPolicy {
            schema: "hyphae-native-governor-policy-v1".to_owned(),
            mode: GovernorMode::Bulk,
            hardware_fingerprint: "1".repeat(64),
            calibration_cache_key: "2".repeat(64),
            calibrated_worker_limit: 4,
            reserved_system_threads: 0,
            schedulable_compute_threads: 4,
            io_slots: 4,
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
            .map(|class| GovernorClassLimit {
                class,
                compute_threads: 4,
                io_slots: 4,
                memory_bytes,
            })
            .collect(),
        }
    }

    fn numa_measurement(source: u32, reader: u32, cpu: u32, median: u64) -> CalibrationMeasurement {
        CalibrationMeasurement {
            primitive: "numa-memory-read".to_owned(),
            variant: format!("linux-first-touch-node-{source}-read-node-{reader}-cpu-{cpu}"),
            input_size: NUMA_CALIBRATION_WORKING_SET_BYTES,
            input_unit: "working-set-bytes".to_owned(),
            bytes_per_operation: NUMA_CALIBRATION_WORKING_SET_BYTES,
            operations_per_sample: 1,
            maximum_operations_per_sample: 64,
            sample_count: 15,
            statistics: CalibrationStatistics {
                unit: "picoseconds_per_operation".to_owned(),
                minimum: median,
                median,
                maximum: median,
                median_absolute_deviation: 0,
                relative_mad_ppm: 0,
                relative_range_ppm: 0,
                median_bytes_per_second: Some(1),
            },
            correctness: CalibrationCorrectness {
                status: "passed".to_owned(),
                result_digest_blake3: "3".repeat(64),
                reference_digest_blake3: "3".repeat(64),
            },
            status: "stable".to_owned(),
            retry_history: Vec::new(),
        }
    }

    fn numa_calibration(profile: &HardwareProfile) -> HardwareCalibration {
        let measurements = vec![
            numa_measurement(0, 0, 0, 1_000_000),
            numa_measurement(0, 1, 4, 4_000_000),
            numa_measurement(1, 0, 0, 4_000_000),
            numa_measurement(1, 1, 4, 1_000_000),
        ];
        HardwareCalibration {
            schema: "hyphae-native-hardware-calibration-v1".to_owned(),
            mode: CalibrationMode::Quick,
            status: "stable".to_owned(),
            accepted_for_scheduling: true,
            cache_status: CalibrationCacheStatus::Disabled,
            elapsed_ms: 10_000,
            identity: CalibrationIdentity {
                hardware_fingerprint: profile.fingerprint.clone(),
                kernel_release: "test".to_owned(),
                filesystem: Some("test".to_owned()),
                compiler_identity: "test".to_owned(),
                hyphae_build_identity: "test".to_owned(),
                executable_blake3: "4".repeat(64),
                cache_key: "2".repeat(64),
            },
            policy: CalibrationPolicy {
                minimum_duration_ms: 5_000,
                maximum_duration_ms: 15_000,
                warmup_batches: 2,
                samples_per_measurement: 15,
                target_sample_duration_ms: 15,
                maximum_relative_mad_ppm: 75_000,
                maximum_relative_range_ppm: 500_000,
                measurement_retry_limit: 1,
            },
            feature_detection: CalibrationFeatureDetection {
                instruction_sets: Vec::new(),
                differential_tests_passed: true,
            },
            measurements,
            selected_kernels: Vec::new(),
            thread_scaling: CalibrationThreadScaling {
                binding: "linux-sched-affinity".to_owned(),
                physical_core_boundary: 4,
                logical_processor_boundary: 8,
                measured_thread_counts: vec![1, 4, 8],
                status: "stable".to_owned(),
                physical_peak_threads: Some(4),
                physical_peak_bytes_per_second: Some(1),
                smt_peak_threads: Some(8),
                smt_peak_bytes_per_second: Some(1),
                smt_to_physical_throughput_ppm: Some(1_000_000),
                smt_recommended: false,
                recommended_worker_count: Some(4),
                recommendation: "test".to_owned(),
            },
            io_scaling: CalibrationIoScaling {
                binding: "buffered-sync-workers".to_owned(),
                measured_queue_depths: vec![1],
                status: "stable".to_owned(),
                peak_queue_depth: Some(1),
                peak_bytes_per_second: Some(1),
                recommended_io_slots: Some(1),
                recommendation: "test".to_owned(),
            },
            coverage: CalibrationCoverage {
                measured: vec!["numa-memory-read".to_owned()],
                unsupported: Vec::new(),
            },
            claims: Vec::new(),
        }
    }

    fn selection_inner(steal_policy: NativeNumaStealPolicy) -> ExecutionInner {
        let pool_count = steal_policy.pools.len();
        let worker_count = pool_count;
        ExecutionInner {
            queues: (0..pool_count)
                .map(|_| WorkQueue {
                    jobs: Mutex::new(VecDeque::new()),
                })
                .collect(),
            worker_wakes: (0..worker_count)
                .map(|_| WorkerWake {
                    changed: Condvar::new(),
                })
                .collect(),
            pool_worker_ordinals: (0..worker_count).map(|worker| vec![worker]).collect(),
            wake_lock: Mutex::new(new_wake_state(pool_count, worker_count)),
            steal_policy,
            shutdown: AtomicBool::new(false),
            completed_jobs: AtomicU64::new(0),
            local_dispatches: AtomicU64::new(0),
            stolen_dispatches: AtomicU64::new(0),
            test_probe: ExecutionTestProbe::new(pool_count, worker_count),
        }
    }

    fn queued_job(class: WorkloadClass, enqueued_at: Instant) -> Job {
        let (completed, _receiver) = mpsc::channel();
        Job {
            class,
            enqueued_at,
            operation: Box::new(|| {}),
            completed: Some(completed),
            targeted_completion: None,
        }
    }

    fn wait_for_sleeping_workers(
        pool: &NativeExecutionPool,
        expected_by_pool: &[usize],
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .ok_or_else(|| io::Error::other("test deadline overflow"))?;
        loop {
            let actual = pool
                .inner
                .test_probe
                .waiting_workers
                .iter()
                .map(|waiting| waiting.load(Ordering::Acquire))
                .collect::<Vec<_>>();
            if actual == expected_by_pool {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "workers did not sleep: expected {expected_by_pool:?}, found {actual:?}"
                ))
                .into());
            }
            thread::yield_now();
        }
    }

    fn wake_returns(pool: &NativeExecutionPool) -> Vec<u64> {
        pool.inner
            .test_probe
            .notified_wake_returns
            .iter()
            .map(|returns| returns.load(Ordering::Acquire))
            .collect()
    }

    fn wait_for_wakes_to_settle(
        pool: &NativeExecutionPool,
        expected_sleeping: &[usize],
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .ok_or_else(|| io::Error::other("test deadline overflow"))?;
        let mut previous = wake_returns(pool);
        let mut stable_checks = 0_u8;
        loop {
            wait_for_sleeping_workers(pool, expected_sleeping)?;
            thread::sleep(Duration::from_millis(1));
            let current = wake_returns(pool);
            if current == previous {
                stable_checks = stable_checks.saturating_add(1);
                if stable_checks == 10 {
                    return Ok(current);
                }
            } else {
                previous = current;
                stable_checks = 0;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("worker wake returns did not settle").into());
            }
        }
    }

    fn calibrated_policy_for_scheduler_tests() -> Result<NativeNumaStealPolicy, Box<dyn Error>> {
        let mut topology = NativeExecutionTopology::derive(&profile(), &policy())?;
        let nodes = BTreeSet::from([0, 1]);
        let matrix = BTreeMap::from([
            ((0, 0), 1_000_000),
            ((0, 1), 4_000_000),
            ((1, 0), 4_000_000),
            ((1, 1), 1_000_000),
        ]);
        topology.numa_steal_policy.status = "calibrated".to_owned();
        for row in &mut topology.numa_steal_policy.pools {
            let worker_node = row
                .worker_numa_node_id
                .ok_or_else(|| io::Error::other("test pool lacks NUMA identity"))?;
            row.steal_targets = derive_numa_steal_targets(&matrix, &nodes, worker_node)?;
        }
        Ok(topology.numa_steal_policy)
    }

    fn portable_unbound_numa_topology() -> Result<NativeExecutionTopology, NativeExecutionError> {
        // These tests exercise queue isolation and wake routing, not kernel
        // placement. Keep the synthetic NUMA identities while avoiding host
        // cpuset assumptions that would make the fixture itself invalid.
        let mut topology = NativeExecutionTopology::derive(&profile(), &policy())?;
        topology.hard_affinity = false;
        for worker in topology.pools.iter_mut().flat_map(|pool| &mut pool.workers) {
            worker.logical_processor_id = None;
        }
        Ok(topology)
    }

    fn single_worker_topology() -> Result<NativeExecutionTopology, NativeExecutionError> {
        let mut policy = policy();
        policy.schedulable_compute_threads = 1;
        let mut topology = NativeExecutionTopology::derive(&profile(), &policy)?;
        topology.hard_affinity = false;
        let worker = topology
            .pools
            .iter_mut()
            .flat_map(|pool| &mut pool.workers)
            .next()
            .ok_or(NativeExecutionError::EmptyExecution)?;
        worker.logical_processor_id = None;
        Ok(topology)
    }

    #[test]
    fn topology_selects_physical_cores_before_smt_and_groups_numa() -> Result<(), Box<dyn Error>> {
        let topology = NativeExecutionTopology::derive(&profile(), &policy())?;
        assert_eq!(topology.worker_count(), 4);
        assert_eq!(topology.pools.len(), 2);
        let selected = topology
            .pools
            .iter()
            .flat_map(|pool| pool.workers.iter())
            .map(|worker| worker.logical_processor_id)
            .collect::<Option<BTreeSet<_>>>()
            .ok_or_else(|| io::Error::other("discovered placement became unbound"))?;
        assert_eq!(selected, BTreeSet::from([0, 2, 4, 6]));
        assert!(
            topology
                .pools
                .iter()
                .flat_map(|pool| &pool.workers)
                .all(|worker| worker.smt_rank == Some(0))
        );
        Ok(())
    }

    #[test]
    fn execution_uses_every_canonical_calibration_prefix_across_numa() -> Result<(), Box<dyn Error>>
    {
        let profile = profile();
        let canonical = physical_core_first_processor_order(&profile)
            .ok_or_else(|| io::Error::other("test topology has no canonical CPU order"))?;

        for worker_count in 1..=canonical.len() {
            let expected = canonical
                .iter()
                .take(worker_count)
                .copied()
                .collect::<BTreeSet<_>>();
            let actual = derive_discovered_pools(&profile, worker_count)?
                .into_iter()
                .flat_map(|pool| pool.workers)
                .map(|worker| {
                    Ok((
                        worker
                            .logical_processor_id
                            .ok_or_else(|| io::Error::other("worker lost logical CPU identity"))?,
                        worker
                            .smt_rank
                            .ok_or_else(|| io::Error::other("worker lost SMT rank"))?,
                    ))
                })
                .collect::<Result<BTreeSet<_>, io::Error>>()?;

            assert_eq!(actual, expected, "worker prefix {worker_count} diverged");
        }
        Ok(())
    }

    #[test]
    fn first_touch_matrix_cannot_authorize_numa_stealing() -> Result<(), Box<dyn Error>> {
        let profile = profile();
        let policy = policy();
        let calibration = numa_calibration(&profile);
        assert!(matches!(
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &calibration),
            Err(NativeExecutionError::NumaCalibrationUnavailable)
        ));
        let latent = calibrated_policy_for_scheduler_tests()?;
        assert!(latent.pools.iter().all(|pool| {
            pool.steal_targets.len() == 1
                && pool.steal_targets[0].remote_to_local_latency_ppm == 4_000_000
                && pool.steal_targets[0].steal_after_nanoseconds == 3_000
        }));

        let mut incomplete = calibration;
        incomplete.measurements.pop();
        assert!(matches!(
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &incomplete),
            Err(NativeExecutionError::NumaCalibrationUnavailable)
        ));
        let mut no_penalty = numa_calibration(&profile);
        let remote = no_penalty
            .measurements
            .iter_mut()
            .find(|measurement| measurement.variant == "linux-first-touch-node-0-read-node-1-cpu-4")
            .ok_or_else(|| io::Error::other("missing remote NUMA test cell"))?;
        remote.statistics.median = 1_000_000;
        assert!(matches!(
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &no_penalty),
            Err(NativeExecutionError::NumaCalibrationUnavailable)
        ));
        let mut wrong_unit = numa_calibration(&profile);
        wrong_unit.measurements[0].statistics.unit = "nanoseconds".to_owned();
        assert!(matches!(
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &wrong_unit),
            Err(NativeExecutionError::NumaCalibrationUnavailable)
        ));

        let mut unsupported = numa_calibration(&profile);
        unsupported.measurements.clear();
        unsupported.coverage.measured.clear();
        unsupported
            .coverage
            .unsupported
            .push(UnsupportedCalibration {
                primitive: "numa-local-remote-memory".to_owned(),
                reason: "page residency unavailable".to_owned(),
            });
        let topology =
            NativeExecutionTopology::derive_with_calibration(&profile, &policy, &unsupported)?;
        assert_eq!(topology.numa_steal_policy.status, "disabled");
        Ok(())
    }

    #[test]
    fn default_and_portable_topologies_disable_cross_node_stealing() -> Result<(), Box<dyn Error>> {
        let profile = profile();
        let topology = NativeExecutionTopology::derive(&profile, &policy())?;
        assert_eq!(topology.numa_steal_policy.status, "disabled");
        assert!(
            topology
                .numa_steal_policy
                .pools
                .iter()
                .all(|pool| pool.steal_targets.is_empty())
        );

        let mut portable = profile;
        portable.cpu.processor_topology.clear();
        let topology = NativeExecutionTopology::derive(&portable, &policy())?;
        assert_eq!(topology.numa_steal_policy.status, "not-applicable");
        Ok(())
    }

    #[test]
    fn cross_node_job_is_not_stolen_before_calibrated_age() -> Result<(), Box<dyn Error>> {
        let inner = selection_inner(calibrated_policy_for_scheduler_tests()?);
        let now = Instant::now();
        inner.queues[1]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(queued_job(
                WorkloadClass::ForegroundBounded,
                now.checked_sub(Duration::from_nanos(2_999))
                    .ok_or_else(|| io::Error::other("test clock underflow"))?,
            ));
        let mut wake = new_wake_state(2, 2);
        assert!(take_job(&inner, &mut wake, 0, now).is_none());
        assert!(
            take_job(
                &inner,
                &mut wake,
                0,
                now.checked_add(Duration::from_nanos(1))
                    .ok_or_else(|| io::Error::other("test clock overflow"))?,
            )
            .is_some()
        );
        assert_eq!(inner.local_dispatches.load(Ordering::Acquire), 0);
        assert_eq!(inner.stolen_dispatches.load(Ordering::Acquire), 1);
        Ok(())
    }

    #[test]
    fn execution_selection_prioritizes_foreground_but_forces_maintenance_progress()
    -> Result<(), Box<dyn Error>> {
        let mut steal_policy = calibrated_policy_for_scheduler_tests()?;
        steal_policy.foreground_burst_limit = 2;
        let inner = selection_inner(steal_policy);
        let now = Instant::now();
        inner.queues[1]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(queued_job(
                WorkloadClass::Maintenance,
                now.checked_sub(Duration::from_micros(3))
                    .ok_or_else(|| io::Error::other("test clock underflow"))?,
            ));
        let mut jobs = inner.queues[0]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.push_back(queued_job(WorkloadClass::ForegroundPoint, now));
        jobs.push_back(queued_job(WorkloadClass::Mutation, now));
        jobs.push_back(queued_job(WorkloadClass::ForegroundBounded, now));
        drop(jobs);
        let mut wake = new_wake_state(2, 2);
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::ForegroundPoint)
        );
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::Mutation)
        );
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::Maintenance)
        );
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::ForegroundBounded)
        );
        Ok(())
    }

    #[test]
    fn high_priority_burst_cannot_starve_bounded_foreground() -> Result<(), Box<dyn Error>> {
        let mut steal_policy = calibrated_policy_for_scheduler_tests()?;
        steal_policy.foreground_burst_limit = 2;
        let inner = selection_inner(steal_policy);
        let now = Instant::now();
        let mut jobs = inner.queues[0]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.push_back(queued_job(WorkloadClass::ForegroundPoint, now));
        jobs.push_back(queued_job(WorkloadClass::Mutation, now));
        jobs.push_back(queued_job(WorkloadClass::ForegroundPoint, now));
        jobs.push_back(queued_job(WorkloadClass::ForegroundBounded, now));
        drop(jobs);
        let mut wake = new_wake_state(2, 2);
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::ForegroundPoint)
        );
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::Mutation)
        );
        assert_eq!(
            take_job(&inner, &mut wake, 0, now).map(|job| job.class),
            Some(WorkloadClass::ForegroundBounded)
        );
        assert_eq!(inner.local_dispatches.load(Ordering::Acquire), 3);
        assert_eq!(inner.stolen_dispatches.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[test]
    fn one_local_job_wakes_only_one_sleeping_worker() -> Result<(), Box<dyn Error>> {
        let mut profile = profile();
        profile.cpu.processor_topology.clear();
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::new(&profile, &policy)?;
        wait_for_sleeping_workers(&pool, &[4])?;
        let before = wake_returns(&pool);

        assert_eq!(
            pool.execute_ordered(&parent, vec![41_u64], |value| value + 1)?,
            vec![42]
        );

        let after = wait_for_wakes_to_settle(&pool, &[4])?;
        assert_eq!(after[0].saturating_sub(before[0]), 1);
        Ok(())
    }

    #[test]
    fn submitted_job_wakes_only_its_numa_pool() -> Result<(), Box<dyn Error>> {
        let pool = NativeExecutionPool::from_topology(portable_unbound_numa_topology()?)?;
        wait_for_sleeping_workers(&pool, &[2, 2])?;
        let before = wake_returns(&pool);
        let (worker_tx, worker_rx) = mpsc::sync_channel(1);
        let (completed_tx, completed_rx) = mpsc::channel();

        pool.submit(
            1,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at: Instant::now(),
                operation: Box::new(move || {
                    let _ignored = worker_tx.send(
                        thread::current()
                            .name()
                            .unwrap_or("unnamed-worker")
                            .to_owned(),
                    );
                }),
                completed: Some(completed_tx),
                targeted_completion: None,
            },
        );
        assert!(completed_rx.recv_timeout(Duration::from_secs(2))?);
        assert!(
            worker_rx
                .recv_timeout(Duration::from_secs(2))?
                .starts_with("hyphae-numa-1-")
        );

        let after = wait_for_wakes_to_settle(&pool, &[2, 2])?;
        assert_eq!(after[0].saturating_sub(before[0]), 0);
        assert_eq!(after[1].saturating_sub(before[1]), 1);
        Ok(())
    }

    #[test]
    fn two_jobs_have_two_completions_and_at_most_two_wake_returns() -> Result<(), Box<dyn Error>> {
        let mut profile = profile();
        profile.cpu.processor_topology.clear();
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 2,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::new(&profile, &policy)?;
        wait_for_sleeping_workers(&pool, &[4])?;
        let before = wake_returns(&pool);
        let completed_before = pool.completed_jobs();
        let rendezvous = Arc::new(Barrier::new(2));

        let values = pool.execute_ordered(&parent, vec![3_u64, 5], {
            let rendezvous = Arc::clone(&rendezvous);
            move |value| {
                rendezvous.wait();
                value * 2
            }
        })?;

        assert_eq!(values, vec![6, 10]);
        assert_eq!(pool.completed_jobs().saturating_sub(completed_before), 2);
        let after = wait_for_wakes_to_settle(&pool, &[4])?;
        assert!(after[0].saturating_sub(before[0]) <= 2);
        Ok(())
    }

    #[test]
    fn generic_submit_wakes_sleeping_worker_instead_of_busy_round_robin_target()
    -> Result<(), Box<dyn Error>> {
        let mut topology = single_worker_topology()?;
        let second = NativeWorkerPlacement {
            worker_index: 1,
            logical_processor_id: None,
            numa_node_id: None,
            socket_id: None,
            core_id: None,
            smt_rank: Some(0),
        };
        topology.pools[0].workers.push(second);
        let pool = NativeExecutionPool::from_topology(topology)?;
        let release = HomeWorkerRelease::new();
        let (active_started_tx, active_started_rx) = mpsc::sync_channel(1);
        let (active_completed_tx, active_completed_rx) = mpsc::channel();
        let active_release = release.waiter();
        pool.submit(
            0,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at: Instant::now(),
                operation: Box::new(move || {
                    let _ignored = active_started_tx.send(());
                    active_release.wait();
                }),
                completed: Some(active_completed_tx),
                targeted_completion: None,
            },
        );
        active_started_rx.recv_timeout(Duration::from_secs(2))?;
        wait_for_sleeping_workers(&pool, &[1])?;
        pool.inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_worker_by_pool[0] = 0;

        let (generic_completed_tx, generic_completed_rx) = mpsc::channel();
        pool.submit(
            0,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at: Instant::now(),
                operation: Box::new(|| {}),
                completed: Some(generic_completed_tx),
                targeted_completion: None,
            },
        );
        assert!(generic_completed_rx.recv_timeout(Duration::from_secs(2))?);
        assert!(matches!(
            active_completed_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(release);
        assert!(active_completed_rx.recv_timeout(Duration::from_secs(2))?);
        Ok(())
    }

    #[test]
    fn shutdown_does_not_wait_on_ineligible_remote_pool_work() -> Result<(), Box<dyn Error>> {
        let pool = NativeExecutionPool::from_topology(portable_unbound_numa_topology()?)?;
        let release = HomeWorkerRelease::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        for _ in 0..2 {
            let started_tx = started_tx.clone();
            let completed_tx = completed_tx.clone();
            let worker_release = release.waiter();
            pool.submit(
                0,
                Job {
                    class: WorkloadClass::ForegroundBounded,
                    enqueued_at: Instant::now(),
                    operation: Box::new(move || {
                        let _ignored = started_tx.send(());
                        worker_release.wait();
                    }),
                    completed: Some(completed_tx),
                    targeted_completion: None,
                },
            );
        }
        drop(started_tx);
        started_rx.recv_timeout(Duration::from_secs(2))?;
        started_rx.recv_timeout(Duration::from_secs(2))?;
        pool.submit(
            0,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at: Instant::now(),
                operation: Box::new(|| {}),
                completed: Some(completed_tx),
                targeted_completion: None,
            },
        );
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        let shutdown = thread::spawn(move || {
            pool.shutdown_and_join_for_test();
            let _ignored = shutdown_tx.send(());
        });
        assert!(matches!(
            shutdown_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(release);
        for _ in 0..3 {
            assert!(completed_rx.recv_timeout(Duration::from_secs(2))?);
        }
        shutdown_rx.recv_timeout(Duration::from_secs(2))?;
        shutdown.join().map_err(|_| "shutdown thread panicked")?;
        Ok(())
    }

    #[test]
    fn calibrated_remote_candidate_observes_steal_delay() -> Result<(), Box<dyn Error>> {
        let mut topology = portable_unbound_numa_topology()?;
        topology.numa_steal_policy = calibrated_policy_for_scheduler_tests()?;
        let steal_delay = Duration::from_millis(80);
        for pool_policy in &mut topology.numa_steal_policy.pools {
            for target in &mut pool_policy.steal_targets {
                target.steal_after_nanoseconds = u64::try_from(steal_delay.as_nanos())?;
            }
        }
        let pool = NativeExecutionPool::from_topology(topology)?;
        wait_for_sleeping_workers(&pool, &[2, 2])?;

        let release_home = HomeWorkerRelease::new();
        let (home_started_tx, home_started_rx) = mpsc::channel();
        let (home_completed_tx, home_completed_rx) = mpsc::channel();
        for _ in 0..2 {
            let release_home = release_home.waiter();
            let home_started_tx = home_started_tx.clone();
            pool.submit(
                1,
                Job {
                    class: WorkloadClass::ForegroundBounded,
                    enqueued_at: Instant::now(),
                    operation: Box::new(move || {
                        let _ignored = home_started_tx.send(());
                        release_home.wait();
                    }),
                    completed: Some(home_completed_tx.clone()),
                    targeted_completion: None,
                },
            );
        }
        drop(home_started_tx);
        drop(home_completed_tx);
        home_started_rx.recv_timeout(Duration::from_secs(2))?;
        home_started_rx.recv_timeout(Duration::from_secs(2))?;
        let before = wait_for_wakes_to_settle(&pool, &[2, 0])?;

        let (stolen_worker_tx, stolen_worker_rx) = mpsc::sync_channel(1);
        let (stolen_completed_tx, stolen_completed_rx) = mpsc::channel();
        let enqueued_at = Instant::now();
        pool.submit(
            1,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at,
                operation: Box::new(move || {
                    let executed_at = Instant::now();
                    let _ignored = stolen_worker_tx.send((
                        thread::current()
                            .name()
                            .unwrap_or("unnamed-worker")
                            .to_owned(),
                        executed_at,
                    ));
                }),
                completed: Some(stolen_completed_tx),
                targeted_completion: None,
            },
        );
        assert!(stolen_completed_rx.recv_timeout(Duration::from_secs(2))?);
        let (stolen_worker, executed_at) = stolen_worker_rx.recv_timeout(Duration::from_secs(2))?;
        assert!(executed_at.saturating_duration_since(enqueued_at) >= steal_delay);
        assert!(stolen_worker.starts_with("hyphae-numa-0-"));
        let after = wake_returns(&pool);
        assert_eq!(after[0].saturating_sub(before[0]), 1);
        assert_eq!(after[1].saturating_sub(before[1]), 0);
        assert_eq!(pool.stolen_dispatches(), 1);

        drop(release_home);
        assert!(home_completed_rx.recv_timeout(Duration::from_secs(2))?);
        assert!(home_completed_rx.recv_timeout(Duration::from_secs(2))?);
        wait_for_sleeping_workers(&pool, &[2, 2])?;
        Ok(())
    }

    #[test]
    fn calibrated_remote_candidate_early_unwind_does_not_deadlock() -> Result<(), Box<dyn Error>> {
        let (scenario_completed_tx, scenario_completed_rx) = mpsc::sync_channel(1);
        let scenario = thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<(), Box<dyn Error>> {
                    let pool =
                        NativeExecutionPool::from_topology(portable_unbound_numa_topology()?)?;
                    let release_home = HomeWorkerRelease::new();
                    let (home_started_tx, home_started_rx) = mpsc::channel();
                    let (home_completed_tx, _home_completed_rx) = mpsc::channel();
                    for _ in 0..2 {
                        let release_home = release_home.waiter();
                        let home_started_tx = home_started_tx.clone();
                        pool.submit(
                            1,
                            Job {
                                class: WorkloadClass::ForegroundBounded,
                                enqueued_at: Instant::now(),
                                operation: Box::new(move || {
                                    let _ignored = home_started_tx.send(());
                                    release_home.wait();
                                }),
                                completed: Some(home_completed_tx.clone()),
                                targeted_completion: None,
                            },
                        );
                    }
                    drop(home_started_tx);
                    drop(home_completed_tx);
                    home_started_rx.recv_timeout(Duration::from_secs(2))?;
                    home_started_rx.recv_timeout(Duration::from_secs(2))?;
                    std::panic::resume_unwind(Box::new(
                        "injected failure before releasing home workers",
                    ));
                },
            ));
            let _ignored = scenario_completed_tx.send(outcome.is_err());
        });

        let unwound = scenario_completed_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| io::Error::other("pool teardown deadlocked after test unwind"))?;
        assert!(unwound);
        scenario
            .join()
            .map_err(|_| io::Error::other("unwind scenario thread panicked"))?;
        Ok(())
    }

    #[test]
    fn shutdown_wakes_and_joins_every_sleeping_worker() -> Result<(), Box<dyn Error>> {
        let mut profile = profile();
        profile.cpu.processor_topology.clear();
        let pool = NativeExecutionPool::new(&profile, &policy())?;
        wait_for_sleeping_workers(&pool, &[4])?;
        let inner = Arc::clone(&pool.inner);
        let before = inner.test_probe.notified_wake_returns[0].load(Ordering::Acquire);

        drop(pool);

        assert!(inner.shutdown.load(Ordering::Acquire));
        assert_eq!(
            inner.test_probe.notified_wake_returns[0]
                .load(Ordering::Acquire)
                .saturating_sub(before),
            4
        );
        assert_eq!(
            inner.test_probe.waiting_workers[0].load(Ordering::Acquire),
            0
        );
        Ok(())
    }

    #[test]
    fn persistent_pool_preserves_order_and_parent_capacity() -> Result<(), Box<dyn Error>> {
        let mut profile = profile();
        profile.cpu.processor_topology.clear();
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let parent = governor.try_admit_owned(
            WorkloadClass::Bulk,
            GovernorRequest {
                compute_threads: 4,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::new(&profile, &policy)?;
        let barrier = Arc::new(Barrier::new(4));
        let first = Arc::new(AtomicUsize::new(0));
        let worker_names = Arc::new(Mutex::new(BTreeSet::new()));
        let results = pool.execute_ordered(&parent, (0_u64..64).collect(), {
            let barrier = Arc::clone(&barrier);
            let first = Arc::clone(&first);
            let worker_names = Arc::clone(&worker_names);
            move |value| {
                if first.fetch_add(1, Ordering::AcqRel) < 4 {
                    barrier.wait();
                }
                worker_names
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(thread::current().name().unwrap_or("unknown").to_owned());
                value * value
            }
        })?;
        assert_eq!(
            results,
            (0_u64..64).map(|value| value * value).collect::<Vec<_>>()
        );
        assert_eq!(
            worker_names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            4
        );
        assert_eq!(parent.request().compute_threads, 4);
        drop(parent);
        assert_eq!(governor.usage_snapshot().compute_threads, 0);
        Ok(())
    }

    #[test]
    fn panicking_operation_returns_every_nested_token_and_pool_survives()
    -> Result<(), Box<dyn Error>> {
        let mut profile = profile();
        profile.cpu.processor_topology.clear();
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let parent = governor.try_admit_owned(
            WorkloadClass::Bulk,
            GovernorRequest {
                compute_threads: 4,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::new(&profile, &policy)?;
        assert!(matches!(
            pool.execute_ordered(&parent, vec![1_u64, 2, 3, 4], |value| {
                assert_ne!(value, 3, "injected operation panic");
                value
            }),
            Err(NativeExecutionError::OperationPanicked)
        ));
        let full_child = parent.try_subdivide_owned(GovernorRequest {
            compute_threads: 4,
            io_slots: 0,
            memory_bytes: 0,
        })?;
        drop(full_child);
        assert_eq!(
            pool.execute_ordered(&parent, vec![1_u64, 2, 3, 4], |value| value + 1)?,
            vec![2, 3, 4, 5]
        );
        Ok(())
    }

    #[test]
    fn targeted_single_item_uses_one_stable_existing_worker() -> Result<(), Box<dyn Error>> {
        let topology = portable_unbound_numa_topology()?;
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let target_worker = 5 % topology.worker_count();
        let target_pool = topology
            .pools
            .iter()
            .position(|pool| {
                pool.workers
                    .iter()
                    .any(|worker| worker.worker_index == target_worker)
            })
            .ok_or("stable target worker was absent from topology")?;
        let expected_worker_name = format!("hyphae-numa-{target_pool}-{target_worker}");
        let pool = NativeExecutionPool::from_topology(topology)?;
        let caller = thread::current().id();
        let mut worker_names = BTreeSet::new();
        for _ in 0..10_000 {
            let ((worker_thread, actual_worker_name), execution) = pool
                .execute_single_targeted_profiled(&parent, 5, None, || {
                    (
                        thread::current().id(),
                        thread::current()
                            .name()
                            .unwrap_or("unnamed-worker")
                            .to_owned(),
                    )
                })?;
            assert_ne!(worker_thread, caller);
            assert_eq!(execution.route, TargetedSingleExecutionRoute::Targeted);
            assert_eq!(execution.worker_index, target_worker);
            assert_eq!(
                actual_worker_name,
                worker_name(&pool.topology, execution.worker_index)?
            );
            assert_eq!(actual_worker_name, expected_worker_name);
            worker_names.insert(actual_worker_name);
        }
        assert_eq!(worker_names.len(), 1);
        Ok(())
    }

    #[test]
    fn targeted_single_item_busy_slot_falls_back_without_losing_work() -> Result<(), Box<dyn Error>>
    {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 3,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            portable_unbound_numa_topology()?,
        )?);
        let release = HomeWorkerRelease::new();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_release = release.waiter();
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(&active_parent, 1, None, move || {
                let _ignored = started_tx.send(());
                active_release.wait();
                11_u64
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2))?;

        let queued_pool = Arc::clone(&pool);
        let queued_parent = Arc::clone(&parent);
        let queued = thread::spawn(move || {
            queued_pool.execute_single_targeted_profiled(&queued_parent, 1, None, || 13_u64)
        });
        pool.wait_for_targeted_slot_for_test(1, Duration::from_secs(2))?;
        let fallback_caller = thread::current().id();
        let ((fallback_thread, actual_worker_name), fallback) = pool
            .execute_single_targeted_profiled(&parent, 1, None, || {
                (
                    thread::current().id(),
                    thread::current()
                        .name()
                        .unwrap_or("unnamed-worker")
                        .to_owned(),
                )
            })?;
        assert_ne!(fallback_thread, fallback_caller);
        assert_eq!(
            actual_worker_name,
            worker_name(&pool.topology, fallback.worker_index)?
        );
        assert_eq!(
            fallback.route,
            TargetedSingleExecutionRoute::GenericFallbackBusy
        );

        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            11
        );
        assert_eq!(
            queued.join().map_err(|_| "queued submitter panicked")??.0,
            13
        );
        Ok(())
    }

    #[test]
    fn targeted_flood_cannot_starve_eligible_generic_work() -> Result<(), Box<dyn Error>> {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 3,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            single_worker_topology()?
        )?);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let release = HomeWorkerRelease::new();
        let (active_tx, active_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_release = release.waiter();
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(&active_parent, 0, None, move || {
                let _ignored = active_tx.send(());
                active_release.wait();
                "active"
            })
        });
        active_rx.recv_timeout(Duration::from_secs(2))?;

        let generic_child = parent.try_subdivide_owned(GovernorRequest {
            compute_threads: 1,
            io_slots: 0,
            memory_bytes: 0,
        })?;
        let (generic_completed_tx, generic_completed_rx) = mpsc::channel();
        pool.submit(
            0,
            Job {
                class: WorkloadClass::ForegroundBounded,
                enqueued_at: Instant::now(),
                operation: Box::new({
                    let observed = Arc::clone(&observed);
                    move || {
                        observed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("generic");
                        drop(generic_child);
                    }
                }),
                completed: Some(generic_completed_tx),
                targeted_completion: None,
            },
        );
        let targeted_pool = Arc::clone(&pool);
        let targeted_parent = Arc::clone(&parent);
        let targeted_observed = Arc::clone(&observed);
        let targeted = thread::spawn(move || {
            targeted_pool.execute_single_targeted_profiled(&targeted_parent, 0, None, move || {
                targeted_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("targeted");
            })
        });
        pool.wait_for_targeted_slot_for_test(0, Duration::from_secs(2))?;
        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            "active"
        );
        let ((), targeted) = targeted
            .join()
            .map_err(|_| "targeted submitter panicked")??;
        assert_eq!(targeted.route, TargetedSingleExecutionRoute::Targeted);
        assert!(generic_completed_rx.recv_timeout(Duration::from_secs(2))?);
        assert_eq!(
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["generic", "targeted"]
        );
        Ok(())
    }

    #[test]
    fn targeted_queued_cancel_is_generation_safe_and_slot_reusable() -> Result<(), Box<dyn Error>> {
        let mut policy = policy();
        policy.schedulable_compute_threads = 2;
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 2,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            single_worker_topology()?
        )?);
        let release = HomeWorkerRelease::new();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_release = release.waiter();
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(&active_parent, 0, None, move || {
                let _ignored = started_tx.send(());
                active_release.wait();
                1_u64
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2))?;
        let cancellation = governor.cancellation_token();
        let queued_pool = Arc::clone(&pool);
        let queued_parent = Arc::clone(&parent);
        let queued_cancellation = cancellation.clone();
        let queued = thread::spawn(move || {
            queued_pool.execute_single_targeted_profiled(
                &queued_parent,
                0,
                Some(&queued_cancellation),
                || 2_u64,
            )
        });
        let stale = pool.wait_for_targeted_ticket_for_test(0, Duration::from_secs(2))?;
        cancellation.cancel();
        let (cancelled_tx, cancelled_rx) = mpsc::sync_channel(1);
        let cancelled_waiter = thread::spawn(move || {
            let result = queued.join().map_err(|_| "queued submitter panicked");
            let _ignored = cancelled_tx.send(result);
        });
        assert!(matches!(
            cancelled_rx.recv_timeout(Duration::from_secs(2))??,
            Err(TargetedSingleExecutionError::Cancelled)
        ));
        cancelled_waiter
            .join()
            .map_err(|_| "cancel result waiter panicked")?;
        let next = pool.enqueue_targeted_for_test(&parent, 0, || 3_u64)?;
        assert_ne!(stale, next.ticket);
        assert!(!pool.cancel_targeted_ticket_for_test(stale));
        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            1
        );
        assert_eq!(NativeExecutionPool::wait_targeted_for_test(&next)?.0, 3);
        assert_eq!(
            pool.execute_single_targeted_profiled(&parent, 0, None, || 4_u64)?
                .0,
            4
        );
        Ok(())
    }

    #[test]
    fn targeted_generation_overflow_fails_closed_without_overwriting_slot()
    -> Result<(), Box<dyn Error>> {
        let pool = NativeExecutionPool::from_topology(single_worker_topology()?)?;
        pool.set_targeted_generation_for_test(0, u64::MAX);
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        assert!(matches!(
            pool.execute_single_targeted_profiled(&parent, 0, None, || 1_u64),
            Err(TargetedSingleExecutionError::GenerationExhausted)
        ));
        assert!(!pool.targeted_slot_occupied_for_test(0));
        Ok(())
    }

    #[test]
    fn targeted_submit_between_worker_check_and_wait_has_no_lost_wake() -> Result<(), Box<dyn Error>>
    {
        let mut policy = policy();
        policy.schedulable_compute_threads = 1;
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::from_topology(single_worker_topology()?)?;
        let release = HomeWorkerRelease::new();
        let (waiting_tx, waiting_rx) = mpsc::sync_channel(1);
        let wait_release = release.waiter();
        pool.install_targeted_pre_wait_hook_for_test(0, waiting_tx, move || {
            wait_release.wait();
        });
        waiting_rx.recv_timeout(Duration::from_secs(2))?;
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let submitter = thread::spawn(move || {
            let result = pool.execute_single_targeted_profiled(&parent, 0, None, || 41_u64);
            let _ignored = completed_tx.send(result);
        });
        drop(release);
        assert_eq!(completed_rx.recv_timeout(Duration::from_secs(2))??.0, 41);
        submitter
            .join()
            .map_err(|_| "lost-wake submitter panicked")?;
        Ok(())
    }

    #[test]
    fn targeted_pre_cancel_and_foreign_cancel_fail_without_consuming_generation()
    -> Result<(), Box<dyn Error>> {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy.clone()));
        let foreign = Arc::new(NativeResourceGovernor::new(policy));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::from_topology(single_worker_topology()?)?;
        let generation = pool.targeted_generation_for_test(0);
        let cancelled = governor.cancellation_token();
        cancelled.cancel();
        assert!(matches!(
            pool.execute_single_targeted_profiled(&parent, 0, Some(&cancelled), || 1_u64),
            Err(TargetedSingleExecutionError::Cancelled)
        ));
        assert_eq!(pool.targeted_generation_for_test(0), generation);
        let foreign = foreign.cancellation_token();
        foreign.cancel();
        assert!(matches!(
            pool.execute_single_targeted_profiled(&parent, 0, Some(&foreign), || 2_u64),
            Err(TargetedSingleExecutionError::ForeignCancellation)
        ));
        assert_eq!(pool.targeted_generation_for_test(0), generation);
        assert!(!pool.targeted_slot_occupied_for_test(0));
        Ok(())
    }

    #[test]
    fn targeted_active_cancellation_remains_cooperative() -> Result<(), Box<dyn Error>> {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            single_worker_topology()?
        )?);
        let release = HomeWorkerRelease::new();
        let cancellation = governor.cancellation_token();
        let executions = Arc::new(AtomicU64::new(0));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_cancellation = cancellation.clone();
        let active_release = release.waiter();
        let active_executions = Arc::clone(&executions);
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(
                &active_parent,
                0,
                Some(&active_cancellation),
                move || {
                    active_executions.fetch_add(1, Ordering::AcqRel);
                    let _ignored = started_tx.send(());
                    active_release.wait();
                    1_u64
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(2))?;
        cancellation.cancel();
        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            1
        );
        assert_eq!(executions.load(Ordering::Acquire), 1);
        Ok(())
    }

    #[test]
    fn targeted_generic_fallback_cancellation_remains_cooperative() -> Result<(), Box<dyn Error>> {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 3,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            single_worker_topology()?
        )?);
        let release = HomeWorkerRelease::new();
        let cancellation = governor.cancellation_token();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_cancellation = cancellation.clone();
        let active_release = release.waiter();
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(
                &active_parent,
                0,
                Some(&active_cancellation),
                move || {
                    let _ignored = started_tx.send(());
                    active_release.wait();
                    1_u64
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(2))?;
        let queued = pool.enqueue_targeted_for_test(&parent, 0, || 3_u64)?;
        let fallback_pool = Arc::clone(&pool);
        let fallback_parent = Arc::clone(&parent);
        let fallback_cancellation = cancellation.clone();
        let fallback_executions = Arc::new(AtomicU64::new(0));
        let worker_executions = Arc::clone(&fallback_executions);
        let fallback = thread::spawn(move || {
            fallback_pool.execute_single_targeted_profiled(
                &fallback_parent,
                0,
                Some(&fallback_cancellation),
                move || {
                    worker_executions.fetch_add(1, Ordering::AcqRel);
                    2_u64
                },
            )
        });
        pool.wait_for_generic_queue_for_test(0, Duration::from_secs(2))?;
        cancellation.cancel();
        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            1
        );
        let (value, receipt) = fallback
            .join()
            .map_err(|_| "fallback submitter panicked")??;
        assert_eq!(value, 2);
        assert_eq!(fallback_executions.load(Ordering::Acquire), 1);
        assert_eq!(
            receipt.route,
            TargetedSingleExecutionRoute::GenericFallbackBusy
        );
        assert_eq!(NativeExecutionPool::wait_targeted_for_test(&queued)?.0, 3);
        Ok(())
    }

    #[test]
    fn targeted_panic_releases_child_and_slot_for_reuse() -> Result<(), Box<dyn Error>> {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?;
        let pool = NativeExecutionPool::from_topology(single_worker_topology()?)?;
        assert!(matches!(
            pool.execute_single_targeted_profiled(&parent, 0, None, || {
                std::panic::resume_unwind(Box::new("injected targeted panic"));
            }),
            Err(TargetedSingleExecutionError::Execution(
                NativeExecutionError::OperationPanicked
            ))
        ));
        let child = parent.try_subdivide_owned(GovernorRequest {
            compute_threads: 1,
            io_slots: 0,
            memory_bytes: 0,
        })?;
        drop(child);
        assert_eq!(
            pool.execute_single_targeted_profiled(&parent, 0, None, || 7_u64)?
                .0,
            7
        );
        Ok(())
    }

    #[test]
    fn targeted_shutdown_drains_accepted_and_rejects_closed_admission() -> Result<(), Box<dyn Error>>
    {
        let policy = policy();
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let parent = Arc::new(governor.try_admit_owned(
            WorkloadClass::ForegroundBounded,
            GovernorRequest {
                compute_threads: 2,
                io_slots: 0,
                memory_bytes: 0,
            },
        )?);
        let pool = Arc::new(NativeExecutionPool::from_topology(
            single_worker_topology()?
        )?);
        let release = HomeWorkerRelease::new();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_pool = Arc::clone(&pool);
        let active_parent = Arc::clone(&parent);
        let active_release = release.waiter();
        let active = thread::spawn(move || {
            active_pool.execute_single_targeted_profiled(&active_parent, 0, None, move || {
                let _ignored = started_tx.send(());
                active_release.wait();
                1_u64
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2))?;
        let queued = pool.enqueue_targeted_for_test(&parent, 0, || 2_u64)?;
        pool.signal_shutdown_for_test();
        assert!(matches!(
            pool.execute_single_targeted_profiled(&parent, 0, None, || 3_u64),
            Err(TargetedSingleExecutionError::Closed)
        ));
        drop(release);
        assert_eq!(
            active.join().map_err(|_| "active submitter panicked")??.0,
            1
        );
        assert_eq!(NativeExecutionPool::wait_targeted_for_test(&queued)?.0, 2);
        Arc::try_unwrap(pool)
            .map_err(|_| "shutdown test retained an unexpected pool owner")?
            .shutdown_and_join_for_test();
        Ok(())
    }
}
