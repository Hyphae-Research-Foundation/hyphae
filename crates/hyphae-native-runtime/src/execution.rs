// SPDX-License-Identifier: GPL-3.0-only

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
};

#[cfg(target_os = "linux")]
use nix::{
    sched::{CpuSet, sched_setaffinity},
    unistd::Pid,
};
use serde::Serialize;
use thiserror::Error;

use crate::{
    GovernorAdmissionError, GovernorRequest, HardwareProfile, NativeGovernorPolicy,
    OwnedGovernorPermit,
};

struct Job {
    operation: Box<dyn FnOnce() + Send + 'static>,
    completed: mpsc::Sender<bool>,
}
const EXECUTION_TOPOLOGY_SCHEMA: &str = "hyphae-native-execution-topology-v1";

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
            return Ok(Self {
                schema: EXECUTION_TOPOLOGY_SCHEMA.to_owned(),
                hardware_fingerprint: profile.fingerprint.clone(),
                schedulable_compute_threads: policy.schedulable_compute_threads,
                hard_affinity: false,
                pools: vec![NativeNumaPoolTopology {
                    numa_node_id: None,
                    workers,
                }],
            });
        }

        let pools = derive_discovered_pools(profile, worker_count)?;
        Ok(Self {
            schema: EXECUTION_TOPOLOGY_SCHEMA.to_owned(),
            hardware_fingerprint: profile.fingerprint.clone(),
            schedulable_compute_threads: policy.schedulable_compute_threads,
            hard_affinity: cfg!(target_os = "linux"),
            pools,
        })
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
    let mut siblings = BTreeMap::<(u32, u32), Vec<u32>>::new();
    for processor in &profile.cpu.processor_topology {
        if !seen.insert(processor.logical_id) {
            return Err(NativeExecutionError::DuplicateProcessor(
                processor.logical_id,
            ));
        }
        siblings
            .entry((processor.socket_id, processor.core_id))
            .or_default()
            .push(processor.logical_id);
    }
    for logical_ids in siblings.values_mut() {
        logical_ids.sort_unstable();
    }
    let mut candidates = profile
        .cpu
        .processor_topology
        .iter()
        .map(|processor| {
            let smt_rank = siblings
                .get(&(processor.socket_id, processor.core_id))
                .and_then(|logical_ids| {
                    logical_ids
                        .iter()
                        .position(|logical_id| *logical_id == processor.logical_id)
                })
                .and_then(|rank| u32::try_from(rank).ok())
                .ok_or(NativeExecutionError::InsufficientTopology)?;
            Ok((smt_rank, processor))
        })
        .collect::<Result<Vec<_>, NativeExecutionError>>()?;
    candidates.sort_by_key(|(smt_rank, processor)| {
        (
            *smt_rank,
            processor.core_id,
            processor.numa_node_id,
            processor.socket_id,
            processor.logical_id,
        )
    });
    if candidates.len() < worker_count {
        return Err(NativeExecutionError::InsufficientTopology);
    }
    let mut by_node = BTreeMap::<Option<u32>, Vec<NativeWorkerPlacement>>::new();
    for (worker_index, (smt_rank, processor)) in
        candidates.into_iter().take(worker_count).enumerate()
    {
        by_node
            .entry(processor.numa_node_id)
            .or_default()
            .push(NativeWorkerPlacement {
                worker_index,
                numa_node_id: processor.numa_node_id,
                logical_processor_id: Some(processor.logical_id),
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

struct WorkQueue {
    jobs: Mutex<VecDeque<Job>>,
}

struct ExecutionInner {
    queues: Vec<WorkQueue>,
    wake_lock: Mutex<()>,
    changed: Condvar,
    shutdown: AtomicBool,
    completed_jobs: AtomicU64,
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
        let inner = Arc::new(ExecutionInner {
            queues: topology
                .pools
                .iter()
                .map(|_| WorkQueue {
                    jobs: Mutex::new(VecDeque::new()),
                })
                .collect(),
            wake_lock: Mutex::new(()),
            changed: Condvar::new(),
            shutdown: AtomicBool::new(false),
            completed_jobs: AtomicU64::new(0),
        });
        let worker_count = topology.worker_count();
        if worker_count == 0 {
            return Err(NativeExecutionError::EmptyExecution);
        }
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
                    run_worker(&worker_inner, pool_index);
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
                completed: completed_tx.clone(),
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

    fn submit(&self, pool_hint: usize, job: Job) {
        debug_assert!(!self.inner.shutdown.load(Ordering::Acquire));
        let wake = self
            .inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pool_index = pool_hint % self.inner.queues.len();
        self.inner.queues[pool_index]
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(job);
        self.inner.changed.notify_one();
        drop(wake);
    }
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
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.changed.notify_all();
        for worker in self.workers.drain(..) {
            let _ignored = worker.join();
        }
    }
}

fn run_worker(inner: &ExecutionInner, local_pool: usize) {
    loop {
        if let Some(job) = take_job(inner, local_pool) {
            complete_job(inner, job);
            continue;
        }
        let mut wake = inner
            .wake_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = take_job(inner, local_pool) {
            drop(wake);
            complete_job(inner, job);
            continue;
        }
        if inner.shutdown.load(Ordering::Acquire) {
            break;
        }
        wake = inner
            .changed
            .wait(wake)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(wake);
    }
}

fn complete_job(inner: &ExecutionInner, job: Job) {
    let outcome = catch_unwind(AssertUnwindSafe(job.operation)).is_ok();
    inner.completed_jobs.fetch_add(1, Ordering::AcqRel);
    let _ignored = job.completed.send(outcome);
}

fn take_job(inner: &ExecutionInner, local_pool: usize) -> Option<Job> {
    let local = inner.queues[local_pool]
        .jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop_front();
    if local.is_some() {
        return local;
    }
    inner
        .queues
        .iter()
        .enumerate()
        .filter(|(pool_index, _)| *pool_index != local_pool)
        .find_map(|(_, queue)| {
            queue
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        })
}

fn stop_and_join(inner: &ExecutionInner, workers: Vec<JoinHandle<()>>) {
    inner.shutdown.store(true, Ordering::Release);
    inner.changed.notify_all();
    for worker in workers {
        let _ignored = worker.join();
    }
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
        GovernorClassLimit, GovernorMode, HardwareCpu, HardwareMemory, HardwareOperatingSystem,
        HardwareProcessor, HardwareStorage, NativeResourceGovernor, WorkloadClass,
    };
    use std::{
        error::Error,
        sync::{Barrier, Mutex, atomic::AtomicUsize},
    };

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
}
