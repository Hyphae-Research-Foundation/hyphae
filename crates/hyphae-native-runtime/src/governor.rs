// SPDX-License-Identifier: GPL-3.0-only

//! Shared fail-closed resource admission for every Native engine.

use std::{
    array,
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{HardwareCalibration, HardwareProfile};

const CLASS_COUNT: usize = 7;
const GOVERNOR_POLICY_SCHEMA: &str = "hyphae-native-governor-policy-v1";
const MEMORY_HEADROOM_PERCENT: u64 = 15;
const MAX_DISCOVERED_IO_SLOTS: u64 = 64;
const MIN_ADMISSION_QUEUE_CAPACITY: u64 = 64;
const MAX_ADMISSION_QUEUE_CAPACITY: u64 = 4_096;
const ADMISSION_QUEUE_SLOTS_PER_THREAD: u64 = 64;
const FOREGROUND_BURST_LIMIT: u64 = 16;

/// Scheduler objective used to derive bounded class limits.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GovernorMode {
    /// Preserve foreground latency and reserve most parallelism for requests.
    Latency,
    /// Maximize controlled construction, import, and index-build throughput.
    Bulk,
    /// Bound foreground and background work on a concurrently serving node.
    Mixed,
}

/// Resource-admission class shared by SQL, structures, and search.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadClass {
    /// Point lookup that should normally remain single-threaded.
    ForegroundPoint,
    /// Explicitly bounded query, join, lexical, vector, or hybrid work.
    ForegroundBounded,
    /// Mutation, publication, and commit work.
    Mutation,
    /// Initial load, index build, import, or other throughput work.
    Bulk,
    /// Compaction, expiry, statistics, and consolidation.
    Maintenance,
    /// WAL replay, manifest validation, and reopen.
    Recovery,
    /// Backup, proof, vacuum, and verification.
    Administrative,
}

impl WorkloadClass {
    const ALL: [Self; CLASS_COUNT] = [
        Self::ForegroundPoint,
        Self::ForegroundBounded,
        Self::Mutation,
        Self::Bulk,
        Self::Maintenance,
        Self::Recovery,
        Self::Administrative,
    ];

    const fn index(self) -> usize {
        match self {
            Self::ForegroundPoint => 0,
            Self::ForegroundBounded => 1,
            Self::Mutation => 2,
            Self::Bulk => 3,
            Self::Maintenance => 4,
            Self::Recovery => 5,
            Self::Administrative => 6,
        }
    }
}

/// Maximum simultaneous resources for one workload class.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GovernorClassLimit {
    /// Class governed by this row.
    pub class: WorkloadClass,
    /// Maximum compute threads held by the class.
    pub compute_threads: u64,
    /// Maximum storage operations admitted concurrently.
    pub io_slots: u64,
    /// Maximum request and scratch memory held by the class.
    pub memory_bytes: u64,
}

/// Immutable hardware-derived policy used by one governor instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeGovernorPolicy {
    /// Versioned policy schema.
    pub schema: String,
    /// Objective used to derive class caps.
    pub mode: GovernorMode,
    /// Stable hardware identity from discovery.
    pub hardware_fingerprint: String,
    /// Exact calibration cache identity.
    pub calibration_cache_key: String,
    /// Worker count selected by the stable unbound scaling curve.
    pub calibrated_worker_limit: u64,
    /// Threads deliberately withheld from request admission.
    pub reserved_system_threads: u64,
    /// Global compute tokens available after the reserve.
    pub schedulable_compute_threads: u64,
    /// Global I/O admission bound derived from the storage queue.
    pub io_slots: u64,
    /// Global memory admission bound after mandatory headroom.
    pub memory_bytes: u64,
    /// Fixed headroom percentage applied to visible host memory.
    pub memory_headroom_percent: u64,
    /// Maximum requests waiting for resource admission.
    pub admission_queue_capacity: u64,
    /// Maximum preferred foreground dispatches while background work waits.
    pub foreground_burst_limit: u64,
    /// Canonical row for every workload class.
    pub class_limits: Vec<GovernorClassLimit>,
}

impl NativeGovernorPolicy {
    /// Derives a policy only from a stable scaling recommendation and static
    /// hardware profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the calibration cannot authorize a worker count,
    /// its identity differs from the profile, or visible memory is unknown.
    pub fn derive(
        profile: &HardwareProfile,
        calibration: &HardwareCalibration,
        mode: GovernorMode,
    ) -> Result<Self, GovernorPolicyError> {
        if calibration.identity.hardware_fingerprint != profile.fingerprint {
            return Err(GovernorPolicyError::HardwareIdentityMismatch);
        }
        let scaling = &calibration.thread_scaling;
        if scaling.status != "stable"
            || !matches!(scaling.binding.as_str(), "unbound" | "linux-sched-affinity")
            || crate::calibration::summarize_thread_scaling(profile, &calibration.measurements)
                != *scaling
        {
            return Err(GovernorPolicyError::ScalingUnavailable);
        }
        let io_scaling = &calibration.io_scaling;
        if crate::calibration::summarize_io_scaling(&calibration.measurements) != *io_scaling {
            return Err(GovernorPolicyError::IoScalingInvalid);
        }
        let worker_limit = scaling
            .recommended_worker_count
            .ok_or(GovernorPolicyError::ScalingUnavailable)?;
        if worker_limit == 0 || worker_limit > scaling.logical_processor_boundary {
            return Err(GovernorPolicyError::InvalidWorkerLimit);
        }
        let total_memory = profile
            .memory
            .total_bytes
            .ok_or(GovernorPolicyError::MemoryUnknown)?;
        let required_headroom = percentage(total_memory, MEMORY_HEADROOM_PERCENT);
        let static_memory_limit = total_memory.saturating_sub(required_headroom);
        let memory_limit = profile
            .memory
            .available_bytes
            .map_or(static_memory_limit, |available| {
                static_memory_limit.min(available.saturating_sub(required_headroom))
            });
        if memory_limit == 0 {
            return Err(GovernorPolicyError::InsufficientMemory);
        }
        let reserve = system_thread_reserve(worker_limit);
        let schedulable = worker_limit.saturating_sub(reserve).max(1);
        let io_slots = if io_scaling.status == "stable" {
            io_scaling
                .recommended_io_slots
                .ok_or(GovernorPolicyError::IoScalingInvalid)?
                .clamp(1, MAX_DISCOVERED_IO_SLOTS)
        } else {
            1
        };
        Ok(Self {
            schema: GOVERNOR_POLICY_SCHEMA.to_owned(),
            mode,
            hardware_fingerprint: profile.fingerprint.clone(),
            calibration_cache_key: calibration.identity.cache_key.clone(),
            calibrated_worker_limit: worker_limit,
            reserved_system_threads: reserve,
            schedulable_compute_threads: schedulable,
            io_slots,
            memory_bytes: memory_limit,
            memory_headroom_percent: MEMORY_HEADROOM_PERCENT,
            admission_queue_capacity: admission_queue_capacity(schedulable),
            foreground_burst_limit: FOREGROUND_BURST_LIMIT,
            class_limits: class_limits(mode, schedulable, io_slots, memory_limit),
        })
    }

    /// Returns the immutable row for `class`.
    pub fn limit(&self, class: WorkloadClass) -> &GovernorClassLimit {
        &self.class_limits[class.index()]
    }
}

/// Fail-closed policy derivation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GovernorPolicyError {
    /// Calibration was produced for a different static profile.
    #[error("calibration hardware fingerprint differs from the current profile")]
    HardwareIdentityMismatch,
    /// No complete stable unbound scaling recommendation exists.
    #[error("stable thread-scaling recommendation is unavailable")]
    ScalingUnavailable,
    /// The recommendation exceeds its recorded logical boundary.
    #[error("thread-scaling recommendation is outside its logical boundary")]
    InvalidWorkerLimit,
    /// Static discovery could not establish visible host memory.
    #[error("visible host memory is unknown")]
    MemoryUnknown,
    /// Current available memory cannot preserve mandatory host headroom.
    #[error("available memory cannot preserve mandatory host headroom")]
    InsufficientMemory,
    /// I/O recommendation differs from its queue-depth measurements.
    #[error("I/O scaling recommendation is inconsistent with its measured curve")]
    IoScalingInvalid,
}

/// Atomic resource request for one admitted unit of work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GovernorRequest {
    /// Compute workers needed simultaneously.
    pub compute_threads: u64,
    /// Storage operations needed simultaneously.
    pub io_slots: u64,
    /// Request arena and scratch bytes needed simultaneously.
    pub memory_bytes: u64,
}

impl GovernorRequest {
    /// Creates a non-empty resource request.
    ///
    /// # Errors
    ///
    /// Returns an error when all three resources are zero.
    pub const fn new(
        compute_threads: u64,
        io_slots: u64,
        memory_bytes: u64,
    ) -> Result<Self, GovernorAdmissionError> {
        if compute_threads == 0 && io_slots == 0 && memory_bytes == 0 {
            return Err(GovernorAdmissionError::EmptyRequest);
        }
        Ok(Self {
            compute_threads,
            io_slots,
            memory_bytes,
        })
    }
}

/// Admission failure that does not mutate durable state.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GovernorAdmissionError {
    /// No resource was requested.
    #[error("governor request must reserve at least one resource")]
    EmptyRequest,
    /// Request exceeds the immutable class cap.
    #[error("governor request exceeds its workload-class limit")]
    ClassLimit,
    /// Other admitted work currently holds the required global tokens.
    #[error("governor global capacity is currently exhausted")]
    GlobalCapacity,
    /// Other work in the same class currently holds its tokens.
    #[error("governor workload-class capacity is currently exhausted")]
    ClassCapacity,
    /// A nested request exceeds the resources owned by its parent permit.
    #[error("nested work exceeds its parent permit")]
    ParentCapacity,
}

/// Bounded-queue failure that never begins engine execution.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GovernorQueueError {
    /// The immutable resource request itself is invalid or above its class cap.
    #[error(transparent)]
    Admission(#[from] GovernorAdmissionError),
    /// The policy-defined waiting bound is already occupied.
    #[error("governor admission queue is full")]
    Full,
    /// The caller cancelled before owning resource tokens.
    #[error("governor admission wait was cancelled")]
    Cancelled,
    /// The caller's maximum queue wait elapsed before admission.
    #[error("governor admission wait timed out")]
    TimedOut,
    /// Another thread poisoned the in-process queue authority.
    #[error("governor admission queue synchronization failed")]
    Synchronization,
    /// The monotonic process-local ticket space is exhausted.
    #[error("governor admission queue ticket space is exhausted")]
    TicketExhausted,
    /// The cancellation token belongs to another governor.
    #[error("governor cancellation token belongs to another authority")]
    ForeignCancellation,
}

/// Cancellation handle scoped to one governor.
#[derive(Clone, Debug)]
pub struct GovernorCancellation {
    cancelled: Arc<AtomicBool>,
    governor: Weak<NativeResourceGovernor>,
}

impl GovernorCancellation {
    /// Cancels every wait using this handle and wakes the owning governor.
    pub fn cancel(&self) {
        if let Some(governor) = self.governor.upgrade() {
            let _queue = governor
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.cancelled.store(true, Ordering::Release);
            governor.queue_changed.notify_all();
        } else {
            self.cancelled.store(true, Ordering::Release);
        }
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Evidence captured when a queued request starts execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GovernorAdmissionReceipt {
    /// Monotonic process-local queue ticket.
    pub ticket: u64,
    /// Admitted workload class.
    pub class: WorkloadClass,
    /// Exact resources held by the returned permit.
    pub request: GovernorRequest,
    /// Requests already waiting when this ticket entered.
    pub initial_queue_depth: u64,
    /// Time spent waiting for selection and resources.
    pub queue_time: Duration,
}

/// Component timing returned when queued execution finishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GovernorWorkReceipt {
    /// Queue and admission evidence.
    pub admission: GovernorAdmissionReceipt,
    /// Time from admission until explicit completion.
    pub execution_time: Duration,
    /// Physical-I/O time explicitly observed by the engine.
    pub io_time: Duration,
}

/// Point-in-time process-local usage for diagnostics and invariant tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GovernorUsageSnapshot {
    /// Globally held compute tokens.
    pub compute_threads: u64,
    /// Globally held I/O tokens.
    pub io_slots: u64,
    /// Globally held arena and scratch bytes.
    pub memory_bytes: u64,
    /// Requests waiting for or selected for admission.
    pub queued_requests: u64,
}

/// Owned allocation returned by bounded priority admission.
#[derive(Debug)]
pub struct QueuedGovernorPermit {
    permit: OwnedGovernorPermit,
    admission: GovernorAdmissionReceipt,
    execution_started: Instant,
    io_time: Duration,
}

impl QueuedGovernorPermit {
    /// Returns admission evidence before execution finishes.
    pub const fn admission(&self) -> &GovernorAdmissionReceipt {
        &self.admission
    }

    /// Returns the parent allocation used for nested subdivision.
    pub const fn permit(&self) -> &OwnedGovernorPermit {
        &self.permit
    }

    /// Returns elapsed execution time without releasing the allocation.
    pub fn execution_time(&self) -> Duration {
        self.execution_started.elapsed()
    }

    pub(crate) fn shrink_to(
        mut self,
        request: GovernorRequest,
    ) -> Result<Self, GovernorAdmissionError> {
        self.permit = self.permit.shrink_to(request)?;
        Ok(self)
    }

    /// Converts queued admission into a long-lived owned allocation.
    ///
    /// This deliberately discards component timing when a transaction must
    /// clone and retain the allocation beyond one execution scope.
    pub fn into_owned(self) -> OwnedGovernorPermit {
        self.permit
    }

    /// Adds one engine-observed physical-I/O interval.
    pub fn record_io(&mut self, elapsed: Duration) {
        self.io_time = self.io_time.saturating_add(elapsed);
    }

    /// Releases the allocation and returns separated queue/execution/I/O time.
    pub fn finish(self) -> GovernorWorkReceipt {
        GovernorWorkReceipt {
            admission: self.admission,
            execution_time: self.execution_started.elapsed(),
            io_time: self.io_time,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct QueuedRequest {
    class: WorkloadClass,
    request: GovernorRequest,
    enqueued: Instant,
    initial_queue_depth: u64,
}

#[derive(Debug, Default)]
struct AdmissionQueueState {
    next_ticket: u64,
    selected: Option<u64>,
    foreground_dispatches_since_background: u64,
    records: BTreeMap<u64, QueuedRequest>,
    high: VecDeque<u64>,
    normal: VecDeque<u64>,
    background: VecDeque<u64>,
}

#[derive(Debug, Default)]
struct ResourceUsage {
    compute_threads: AtomicU64,
    io_slots: AtomicU64,
    memory_bytes: AtomicU64,
}

/// Shared admission authority. It owns no engine and carries no durable state.
#[derive(Debug)]
pub struct NativeResourceGovernor {
    policy: NativeGovernorPolicy,
    global: ResourceUsage,
    classes: [ResourceUsage; CLASS_COUNT],
    queue: Mutex<AdmissionQueueState>,
    queue_changed: Condvar,
}

impl NativeResourceGovernor {
    /// Creates an empty governor using an already verified immutable policy.
    pub fn new(policy: NativeGovernorPolicy) -> Self {
        Self {
            policy,
            global: ResourceUsage::default(),
            classes: array::from_fn(|_| ResourceUsage::default()),
            queue: Mutex::new(AdmissionQueueState::default()),
            queue_changed: Condvar::new(),
        }
    }

    /// Returns the immutable policy used for every admission decision.
    pub const fn policy(&self) -> &NativeGovernorPolicy {
        &self.policy
    }

    /// Atomically reserves global and per-class resources without queueing.
    /// Dropping the returned permit releases every token.
    ///
    /// # Errors
    ///
    /// Returns an error when the request exceeds its immutable class limit or
    /// currently available global/class capacity.
    pub fn try_admit(
        &self,
        class: WorkloadClass,
        request: GovernorRequest,
    ) -> Result<GovernorPermit<'_>, GovernorAdmissionError> {
        let queue = self
            .queue
            .lock()
            .map_err(|_| GovernorAdmissionError::GlobalCapacity)?;
        if !queue.records.is_empty() {
            return Err(GovernorAdmissionError::GlobalCapacity);
        }
        self.reserve_admission(class, request)?;
        drop(queue);
        Ok(GovernorPermit {
            governor: self,
            class,
            request,
            nested_usage: ResourceUsage::default(),
        })
    }

    /// Atomically reserves resources in a permit that owns the governor.
    ///
    /// This form is used when work, such as a transaction, must outlive the
    /// database method that admitted it.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::try_admit`].
    pub fn try_admit_owned(
        self: &Arc<Self>,
        class: WorkloadClass,
        request: GovernorRequest,
    ) -> Result<OwnedGovernorPermit, GovernorAdmissionError> {
        let queue = self
            .queue
            .lock()
            .map_err(|_| GovernorAdmissionError::GlobalCapacity)?;
        if !queue.records.is_empty() {
            return Err(GovernorAdmissionError::GlobalCapacity);
        }
        self.reserve_admission(class, request)?;
        drop(queue);
        Ok(OwnedGovernorPermit {
            allocation: Arc::new(OwnedGovernorAllocation {
                governor: Arc::clone(self),
                class,
                request,
                nested_usage: ResourceUsage::default(),
            }),
        })
    }

    fn reserve_admission(
        &self,
        class: WorkloadClass,
        request: GovernorRequest,
    ) -> Result<(), GovernorAdmissionError> {
        let limit = self.policy.limit(class);
        if exceeds(request, limit) {
            return Err(GovernorAdmissionError::ClassLimit);
        }
        reserve_usage(
            &self.global,
            request,
            GovernorRequest {
                compute_threads: self.policy.schedulable_compute_threads,
                io_slots: self.policy.io_slots,
                memory_bytes: self.policy.memory_bytes,
            },
        )
        .map_err(|()| GovernorAdmissionError::GlobalCapacity)?;
        let class_usage = &self.classes[class.index()];
        let class_capacity = GovernorRequest {
            compute_threads: limit.compute_threads,
            io_slots: limit.io_slots,
            memory_bytes: limit.memory_bytes,
        };
        if reserve_usage(class_usage, request, class_capacity).is_err() {
            release_usage(&self.global, request);
            return Err(GovernorAdmissionError::ClassCapacity);
        }
        Ok(())
    }

    /// Creates a cancellation handle that wakes this queue on cancellation.
    pub fn cancellation_token(self: &Arc<Self>) -> GovernorCancellation {
        GovernorCancellation {
            cancelled: Arc::new(AtomicBool::new(false)),
            governor: Arc::downgrade(self),
        }
    }

    /// Waits in the bounded priority queue and returns an owned allocation.
    ///
    /// Foreground work receives preference, but a waiting background request
    /// is forced after the policy-defined foreground burst. Queue capacity
    /// includes the currently selected ticket until its caller claims tokens.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid request, full queue, cancellation,
    /// elapsed maximum wait, or poisoned in-process synchronization.
    pub fn admit_queued_owned(
        self: &Arc<Self>,
        class: WorkloadClass,
        request: GovernorRequest,
        maximum_wait: Duration,
        cancellation: &GovernorCancellation,
    ) -> Result<QueuedGovernorPermit, GovernorQueueError> {
        self.validate_request(class, request)?;
        if !Weak::ptr_eq(&cancellation.governor, &Arc::downgrade(self)) {
            return Err(GovernorQueueError::ForeignCancellation);
        }
        if cancellation.is_cancelled() {
            return Err(GovernorQueueError::Cancelled);
        }
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| GovernorQueueError::Synchronization)?;
        if queue.records.len()
            >= usize::try_from(self.policy.admission_queue_capacity).unwrap_or(usize::MAX)
        {
            return Err(GovernorQueueError::Full);
        }
        let ticket = queue.next_ticket;
        queue.next_ticket = queue
            .next_ticket
            .checked_add(1)
            .ok_or(GovernorQueueError::TicketExhausted)?;
        let initial_queue_depth = u64::try_from(queue.records.len()).unwrap_or(u64::MAX);
        let enqueued = Instant::now();
        queue.records.insert(
            ticket,
            QueuedRequest {
                class,
                request,
                enqueued,
                initial_queue_depth,
            },
        );
        queue.queue_mut(class).push_back(ticket);
        self.select_next_locked(&mut queue);
        let immediate =
            queue.selected == Some(ticket) && self.can_reserve_admission(class, request);
        self.queue_changed.notify_all();
        let deadline = enqueued.checked_add(maximum_wait);

        loop {
            if cancellation.is_cancelled() {
                Self::remove_ticket_locked(&mut queue, ticket, class);
                self.select_next_locked(&mut queue);
                self.queue_changed.notify_all();
                return Err(GovernorQueueError::Cancelled);
            }
            if queue.selected == Some(ticket) && self.can_reserve_admission(class, request) {
                self.reserve_admission(class, request)?;
                let queued = queue
                    .records
                    .remove(&ticket)
                    .ok_or(GovernorQueueError::Synchronization)?;
                queue.queue_mut(class).retain(|queued| *queued != ticket);
                queue.selected = None;
                queue.record_dispatch(class, self.policy.foreground_burst_limit);
                self.select_next_locked(&mut queue);
                self.queue_changed.notify_all();
                let queue_time = if immediate {
                    Duration::ZERO
                } else {
                    queued.enqueued.elapsed()
                };
                drop(queue);
                return Ok(QueuedGovernorPermit {
                    permit: self.owned_permit_from_reserved(class, request),
                    admission: GovernorAdmissionReceipt {
                        ticket,
                        class,
                        request,
                        initial_queue_depth: queued.initial_queue_depth,
                        queue_time,
                    },
                    execution_started: Instant::now(),
                    io_time: Duration::ZERO,
                });
            }
            let now = Instant::now();
            if let Some(deadline) = deadline {
                if now >= deadline {
                    Self::remove_ticket_locked(&mut queue, ticket, class);
                    self.select_next_locked(&mut queue);
                    self.queue_changed.notify_all();
                    return Err(GovernorQueueError::TimedOut);
                }
                let remaining = deadline.saturating_duration_since(now);
                let waited = self
                    .queue_changed
                    .wait_timeout(queue, remaining)
                    .map_err(|_| GovernorQueueError::Synchronization)?;
                queue = waited.0;
            } else {
                queue = self
                    .queue_changed
                    .wait(queue)
                    .map_err(|_| GovernorQueueError::Synchronization)?;
            }
        }
    }

    /// Returns requests waiting for or selected for admission.
    pub fn queued_requests(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .len()
    }

    /// Returns one non-transactional diagnostic observation of global usage.
    pub fn usage_snapshot(&self) -> GovernorUsageSnapshot {
        GovernorUsageSnapshot {
            compute_threads: self.global.compute_threads.load(Ordering::Acquire),
            io_slots: self.global.io_slots.load(Ordering::Acquire),
            memory_bytes: self.global.memory_bytes.load(Ordering::Acquire),
            queued_requests: u64::try_from(self.queued_requests()).unwrap_or(u64::MAX),
        }
    }

    fn validate_request(
        &self,
        class: WorkloadClass,
        request: GovernorRequest,
    ) -> Result<(), GovernorAdmissionError> {
        if request.compute_threads == 0 && request.io_slots == 0 && request.memory_bytes == 0 {
            return Err(GovernorAdmissionError::EmptyRequest);
        }
        if exceeds(request, self.policy.limit(class)) {
            return Err(GovernorAdmissionError::ClassLimit);
        }
        Ok(())
    }

    fn can_reserve_admission(&self, class: WorkloadClass, request: GovernorRequest) -> bool {
        let global_capacity = GovernorRequest {
            compute_threads: self.policy.schedulable_compute_threads,
            io_slots: self.policy.io_slots,
            memory_bytes: self.policy.memory_bytes,
        };
        let limit = self.policy.limit(class);
        let class_capacity = GovernorRequest {
            compute_threads: limit.compute_threads,
            io_slots: limit.io_slots,
            memory_bytes: limit.memory_bytes,
        };
        usage_fits(&self.global, request, global_capacity)
            && usage_fits(&self.classes[class.index()], request, class_capacity)
    }

    fn owned_permit_from_reserved(
        self: &Arc<Self>,
        class: WorkloadClass,
        request: GovernorRequest,
    ) -> OwnedGovernorPermit {
        OwnedGovernorPermit {
            allocation: Arc::new(OwnedGovernorAllocation {
                governor: Arc::clone(self),
                class,
                request,
                nested_usage: ResourceUsage::default(),
            }),
        }
    }

    fn select_next_locked(&self, queue: &mut AdmissionQueueState) {
        if queue.selected.is_some() {
            return;
        }
        let force_background = !queue.background.is_empty()
            && queue.foreground_dispatches_since_background >= self.policy.foreground_burst_limit;
        let first_admissible = |tickets: &VecDeque<u64>| {
            tickets.iter().copied().find(|ticket| {
                queue
                    .records
                    .get(ticket)
                    .is_some_and(|record| self.can_reserve_admission(record.class, record.request))
            })
        };
        let candidates = if force_background {
            [
                first_admissible(&queue.background),
                first_admissible(&queue.high),
                first_admissible(&queue.normal),
            ]
        } else {
            [
                first_admissible(&queue.high),
                first_admissible(&queue.normal),
                first_admissible(&queue.background),
            ]
        };
        queue.selected = candidates.into_iter().flatten().next();
    }

    fn remove_ticket_locked(queue: &mut AdmissionQueueState, ticket: u64, class: WorkloadClass) {
        queue.records.remove(&ticket);
        queue.queue_mut(class).retain(|queued| *queued != ticket);
        if queue.selected == Some(ticket) {
            queue.selected = None;
        }
    }

    fn shrink_admission(
        &self,
        class: WorkloadClass,
        current: GovernorRequest,
        retained: GovernorRequest,
    ) {
        let released = GovernorRequest {
            compute_threads: current.compute_threads - retained.compute_threads,
            io_slots: current.io_slots - retained.io_slots,
            memory_bytes: current.memory_bytes - retained.memory_bytes,
        };
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        release_usage(&self.classes[class.index()], released);
        release_usage(&self.global, released);
        self.select_next_locked(&mut queue);
        self.queue_changed.notify_all();
    }

    fn release_admission(&self, class: WorkloadClass, request: GovernorRequest) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        release_usage(&self.classes[class.index()], request);
        release_usage(&self.global, request);
        self.select_next_locked(&mut queue);
        self.queue_changed.notify_all();
    }
}

impl AdmissionQueueState {
    fn queue_mut(&mut self, class: WorkloadClass) -> &mut VecDeque<u64> {
        match class {
            WorkloadClass::ForegroundPoint | WorkloadClass::Mutation => &mut self.high,
            WorkloadClass::ForegroundBounded => &mut self.normal,
            WorkloadClass::Bulk
            | WorkloadClass::Maintenance
            | WorkloadClass::Recovery
            | WorkloadClass::Administrative => &mut self.background,
        }
    }

    fn record_dispatch(&mut self, class: WorkloadClass, foreground_burst_limit: u64) {
        if matches!(
            class,
            WorkloadClass::ForegroundPoint
                | WorkloadClass::ForegroundBounded
                | WorkloadClass::Mutation
        ) && !self.background.is_empty()
        {
            self.foreground_dispatches_since_background = self
                .foreground_dispatches_since_background
                .saturating_add(1)
                .min(foreground_burst_limit);
        } else {
            self.foreground_dispatches_since_background = 0;
        }
    }
}

/// RAII ownership of one allocation that keeps its governor alive.
#[derive(Clone, Debug)]
pub struct OwnedGovernorPermit {
    allocation: Arc<OwnedGovernorAllocation>,
}

#[derive(Debug)]
struct OwnedGovernorAllocation {
    governor: Arc<NativeResourceGovernor>,
    class: WorkloadClass,
    request: GovernorRequest,
    nested_usage: ResourceUsage,
}

impl OwnedGovernorPermit {
    /// Returns the class owning this allocation.
    pub fn class(&self) -> WorkloadClass {
        self.allocation.class
    }

    /// Returns the complete parent allocation.
    pub fn request(&self) -> GovernorRequest {
        self.allocation.request
    }

    pub(crate) fn shrink_to(
        mut self,
        request: GovernorRequest,
    ) -> Result<Self, GovernorAdmissionError> {
        if request.compute_threads == 0 && request.io_slots == 0 && request.memory_bytes == 0 {
            return Err(GovernorAdmissionError::EmptyRequest);
        }
        let allocation =
            Arc::get_mut(&mut self.allocation).ok_or(GovernorAdmissionError::ParentCapacity)?;
        let current = allocation.request;
        if request.compute_threads > current.compute_threads
            || request.io_slots > current.io_slots
            || request.memory_bytes > current.memory_bytes
            || allocation
                .nested_usage
                .compute_threads
                .load(Ordering::Acquire)
                > request.compute_threads
            || allocation.nested_usage.io_slots.load(Ordering::Acquire) > request.io_slots
            || allocation.nested_usage.memory_bytes.load(Ordering::Acquire) > request.memory_bytes
        {
            return Err(GovernorAdmissionError::ParentCapacity);
        }
        allocation.request = request;
        allocation
            .governor
            .shrink_admission(allocation.class, current, request);
        Ok(self)
    }

    /// Subdivides already-owned resources without reacquiring global tokens.
    ///
    /// # Errors
    ///
    /// Returns an error when active nested work already owns the requested
    /// portion of this allocation.
    pub fn try_subdivide(
        &self,
        request: GovernorRequest,
    ) -> Result<NestedGovernorPermit<'_>, GovernorAdmissionError> {
        reserve_usage(
            &self.allocation.nested_usage,
            request,
            self.allocation.request,
        )
        .map_err(|()| GovernorAdmissionError::ParentCapacity)?;
        Ok(NestedGovernorPermit {
            nested_usage: &self.allocation.nested_usage,
            request,
        })
    }

    /// Creates an owned subdivision suitable for a persistent worker pool.
    ///
    /// The child keeps the parent allocation alive and returns only its
    /// subdivision when dropped. Cloning the parent never multiplies global
    /// capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when active nested work already owns the requested
    /// portion of this allocation.
    pub fn try_subdivide_owned(
        &self,
        request: GovernorRequest,
    ) -> Result<OwnedNestedGovernorPermit, GovernorAdmissionError> {
        reserve_usage(
            &self.allocation.nested_usage,
            request,
            self.allocation.request,
        )
        .map_err(|()| GovernorAdmissionError::ParentCapacity)?;
        Ok(OwnedNestedGovernorPermit {
            parent: Arc::clone(&self.allocation),
            request,
        })
    }
}

impl Drop for OwnedGovernorAllocation {
    fn drop(&mut self) {
        self.governor.release_admission(self.class, self.request);
    }
}

/// RAII ownership of one global and class allocation.
pub struct GovernorPermit<'governor> {
    governor: &'governor NativeResourceGovernor,
    class: WorkloadClass,
    request: GovernorRequest,
    nested_usage: ResourceUsage,
}

impl GovernorPermit<'_> {
    /// Returns the class owning this allocation.
    pub const fn class(&self) -> WorkloadClass {
        self.class
    }

    /// Returns the complete parent allocation.
    pub const fn request(&self) -> GovernorRequest {
        self.request
    }

    /// Subdivides already-owned resources without reacquiring global tokens.
    /// This is the only supported nested-parallelism path.
    ///
    /// # Errors
    ///
    /// Returns an error when active nested work already holds the requested
    /// portion of the parent allocation.
    pub fn try_subdivide(
        &self,
        request: GovernorRequest,
    ) -> Result<NestedGovernorPermit<'_>, GovernorAdmissionError> {
        reserve_usage(&self.nested_usage, request, self.request)
            .map_err(|()| GovernorAdmissionError::ParentCapacity)?;
        Ok(NestedGovernorPermit {
            nested_usage: &self.nested_usage,
            request,
        })
    }
}

impl Drop for GovernorPermit<'_> {
    fn drop(&mut self) {
        self.governor.release_admission(self.class, self.request);
    }
}

/// RAII subdivision of an existing parent allocation.
pub struct NestedGovernorPermit<'permit> {
    nested_usage: &'permit ResourceUsage,
    request: GovernorRequest,
}

/// Owned subdivision of one parent allocation for persistent worker tasks.
#[derive(Debug)]
pub struct OwnedNestedGovernorPermit {
    parent: Arc<OwnedGovernorAllocation>,
    request: GovernorRequest,
}

impl OwnedNestedGovernorPermit {
    /// Returns the resources owned by this child.
    pub const fn request(&self) -> GovernorRequest {
        self.request
    }
}

impl Drop for OwnedNestedGovernorPermit {
    fn drop(&mut self) {
        release_usage(&self.parent.nested_usage, self.request);
    }
}

impl Drop for NestedGovernorPermit<'_> {
    fn drop(&mut self) {
        release_usage(self.nested_usage, self.request);
    }
}

fn reserve_usage(
    usage: &ResourceUsage,
    request: GovernorRequest,
    limit: GovernorRequest,
) -> Result<(), ()> {
    reserve(
        &usage.compute_threads,
        request.compute_threads,
        limit.compute_threads,
    )?;
    if reserve(&usage.io_slots, request.io_slots, limit.io_slots).is_err() {
        release(&usage.compute_threads, request.compute_threads);
        return Err(());
    }
    if reserve(
        &usage.memory_bytes,
        request.memory_bytes,
        limit.memory_bytes,
    )
    .is_err()
    {
        release(&usage.io_slots, request.io_slots);
        release(&usage.compute_threads, request.compute_threads);
        return Err(());
    }
    Ok(())
}

fn release_usage(usage: &ResourceUsage, request: GovernorRequest) {
    release(&usage.memory_bytes, request.memory_bytes);
    release(&usage.io_slots, request.io_slots);
    release(&usage.compute_threads, request.compute_threads);
}

fn usage_fits(usage: &ResourceUsage, request: GovernorRequest, limit: GovernorRequest) -> bool {
    usage
        .compute_threads
        .load(Ordering::Acquire)
        .checked_add(request.compute_threads)
        .is_some_and(|next| next <= limit.compute_threads)
        && usage
            .io_slots
            .load(Ordering::Acquire)
            .checked_add(request.io_slots)
            .is_some_and(|next| next <= limit.io_slots)
        && usage
            .memory_bytes
            .load(Ordering::Acquire)
            .checked_add(request.memory_bytes)
            .is_some_and(|next| next <= limit.memory_bytes)
}

fn reserve(value: &AtomicU64, requested: u64, limit: u64) -> Result<(), ()> {
    if requested == 0 {
        return Ok(());
    }
    let mut current = value.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(requested) else {
            return Err(());
        };
        if next > limit {
            return Err(());
        }
        match value.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn release(value: &AtomicU64, released: u64) {
    if released > 0 {
        let previous = value.fetch_sub(released, Ordering::AcqRel);
        debug_assert!(previous >= released);
    }
}

fn exceeds(request: GovernorRequest, limit: &GovernorClassLimit) -> bool {
    request.compute_threads > limit.compute_threads
        || request.io_slots > limit.io_slots
        || request.memory_bytes > limit.memory_bytes
}

fn percentage(value: u64, percent: u64) -> u64 {
    u64::try_from(u128::from(value).saturating_mul(u128::from(percent)) / 100).unwrap_or(u64::MAX)
}

fn system_thread_reserve(worker_limit: u64) -> u64 {
    if worker_limit <= 1 {
        0
    } else {
        (worker_limit / 12).max(1).min(worker_limit - 1)
    }
}

fn admission_queue_capacity(schedulable_threads: u64) -> u64 {
    schedulable_threads
        .saturating_mul(ADMISSION_QUEUE_SLOTS_PER_THREAD)
        .clamp(MIN_ADMISSION_QUEUE_CAPACITY, MAX_ADMISSION_QUEUE_CAPACITY)
}

fn class_limits(mode: GovernorMode, compute: u64, io: u64, memory: u64) -> Vec<GovernorClassLimit> {
    WorkloadClass::ALL
        .into_iter()
        .map(|class| GovernorClassLimit {
            class,
            compute_threads: class_compute_limit(mode, class, compute),
            io_slots: class_io_limit(class, io),
            memory_bytes: class_memory_limit(class, memory),
        })
        .collect()
}

fn class_compute_limit(mode: GovernorMode, class: WorkloadClass, total: u64) -> u64 {
    let quarter = total.div_ceil(4).max(1);
    let half = total.div_ceil(2).max(1);
    match (mode, class) {
        (_, WorkloadClass::ForegroundPoint) => 1,
        (_, WorkloadClass::Mutation) => total.min(2),
        (GovernorMode::Latency, WorkloadClass::ForegroundBounded)
        | (GovernorMode::Bulk, WorkloadClass::Bulk | WorkloadClass::Recovery) => total,
        (GovernorMode::Bulk, WorkloadClass::ForegroundBounded)
        | (GovernorMode::Latency, _)
        | (
            GovernorMode::Mixed,
            WorkloadClass::Maintenance | WorkloadClass::Recovery | WorkloadClass::Administrative,
        ) => quarter,
        (GovernorMode::Bulk, _)
        | (GovernorMode::Mixed, WorkloadClass::ForegroundBounded | WorkloadClass::Bulk) => half,
    }
}

fn class_io_limit(class: WorkloadClass, total: u64) -> u64 {
    match class {
        WorkloadClass::ForegroundPoint => 1,
        WorkloadClass::Mutation => total.min(2),
        WorkloadClass::ForegroundBounded => total.div_ceil(2).max(1),
        _ => total,
    }
}

fn class_memory_limit(class: WorkloadClass, total: u64) -> u64 {
    match class {
        WorkloadClass::ForegroundPoint => total.min(64 * 1_024 * 1_024),
        WorkloadClass::Mutation => total.div_ceil(8).max(1),
        WorkloadClass::ForegroundBounded => total.div_ceil(2).max(1),
        _ => total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CalibrationCacheStatus, CalibrationCorrectness, CalibrationCoverage,
        CalibrationFeatureDetection, CalibrationIdentity, CalibrationMeasurement, CalibrationMode,
        CalibrationPolicy, CalibrationStatistics, HardwareCpu, HardwareMemory,
        HardwareOperatingSystem, HardwareStorage,
    };
    use std::{
        sync::{Barrier, mpsc},
        thread,
        time::Duration,
    };

    fn hardware_profile() -> HardwareProfile {
        HardwareProfile {
            schema: "hyphae-native-hardware-profile-v1".to_owned(),
            fingerprint: "1".repeat(64),
            cpu: HardwareCpu {
                architecture: "test".to_owned(),
                logical_processors_available: 8,
                physical_cores_visible: Some(8),
                smt_threads_per_core: Some(1),
                sockets_visible: Some(1),
                numa_nodes_visible: Some(1),
                affinity: "0-7".to_owned(),
                quota_millicores: None,
                instruction_sets: Vec::new(),
                caches: Vec::new(),
                processor_topology: Vec::new(),
                frequency_governors: Vec::new(),
            },
            memory: HardwareMemory {
                total_bytes: Some(1_000),
                available_bytes: Some(900),
                page_size_bytes: Some(4_096),
                huge_page_size_bytes: None,
                huge_pages_total: None,
                numa_nodes: Vec::new(),
            },
            storage: HardwareStorage {
                path: "/test".to_owned(),
                filesystem: Some("testfs".to_owned()),
                device: Some("test-device".to_owned()),
                mount_options: Vec::new(),
                rotational: Some(false),
                queue_depth: Some(128),
                discard_max_bytes: None,
            },
            operating_system: HardwareOperatingSystem {
                family: "test".to_owned(),
                kernel_release: "test-kernel".to_owned(),
                virtualization: "none".to_owned(),
                local_transports: vec!["embedded".to_owned()],
            },
        }
    }

    fn scaling_measurement(threads: u64, throughput: u64) -> CalibrationMeasurement {
        CalibrationMeasurement {
            primitive: "thread-scaling-memory-scan".to_owned(),
            variant: "persistent-workers-physical-range-unbound".to_owned(),
            input_size: threads,
            input_unit: "threads".to_owned(),
            bytes_per_operation: 1_048_576_u64.saturating_mul(threads),
            operations_per_sample: 1,
            maximum_operations_per_sample: 64,
            sample_count: 15,
            statistics: CalibrationStatistics {
                unit: "picoseconds_per_operation".to_owned(),
                minimum: 1,
                median: 1,
                maximum: 1,
                median_absolute_deviation: 0,
                relative_mad_ppm: 0,
                relative_range_ppm: 0,
                median_bytes_per_second: Some(throughput),
            },
            correctness: CalibrationCorrectness {
                status: "passed".to_owned(),
                result_digest_blake3: "3".repeat(64),
                reference_digest_blake3: "3".repeat(64),
            },
            status: "stable".to_owned(),
        }
    }

    fn io_measurement(depth: u64, throughput: u64) -> CalibrationMeasurement {
        let mut measurement = scaling_measurement(depth, throughput);
        measurement.primitive = "queue-depth-random-read".to_owned();
        measurement.variant = "persistent-sync-workers-buffered-4k".to_owned();
        measurement.input_unit = "outstanding-reads".to_owned();
        measurement
    }

    fn calibration(profile: &HardwareProfile) -> HardwareCalibration {
        let measurements = vec![
            scaling_measurement(1, 100),
            scaling_measurement(8, 800),
            io_measurement(1, 100),
            io_measurement(4, 390),
        ];
        let thread_scaling = crate::calibration::summarize_thread_scaling(profile, &measurements);
        let io_scaling = crate::calibration::summarize_io_scaling(&measurements);
        HardwareCalibration {
            schema: "hyphae-native-hardware-calibration-v1".to_owned(),
            mode: CalibrationMode::Quick,
            status: "unstable".to_owned(),
            accepted_for_scheduling: false,
            cache_status: CalibrationCacheStatus::Disabled,
            elapsed_ms: 10_000,
            identity: CalibrationIdentity {
                hardware_fingerprint: profile.fingerprint.clone(),
                kernel_release: "test-kernel".to_owned(),
                filesystem: Some("testfs".to_owned()),
                compiler_identity: "rustc test".to_owned(),
                hyphae_build_identity: "hyphae test".to_owned(),
                executable_blake3: "2".repeat(64),
                cache_key: "4".repeat(64),
            },
            policy: CalibrationPolicy {
                minimum_duration_ms: 5_000,
                maximum_duration_ms: 15_000,
                warmup_batches: 2,
                samples_per_measurement: 15,
                target_sample_duration_ms: 15,
                maximum_relative_mad_ppm: 75_000,
                maximum_relative_range_ppm: 500_000,
            },
            feature_detection: CalibrationFeatureDetection {
                instruction_sets: Vec::new(),
                differential_tests_passed: true,
            },
            measurements,
            selected_kernels: Vec::new(),
            thread_scaling,
            io_scaling,
            coverage: CalibrationCoverage {
                measured: vec![
                    "queue-depth-random-read".to_owned(),
                    "thread-scaling-memory-scan".to_owned(),
                ],
                unsupported: Vec::new(),
            },
            claims: Vec::new(),
        }
    }

    fn test_policy() -> NativeGovernorPolicy {
        NativeGovernorPolicy {
            schema: GOVERNOR_POLICY_SCHEMA.to_owned(),
            mode: GovernorMode::Mixed,
            hardware_fingerprint: "1".repeat(64),
            calibration_cache_key: "2".repeat(64),
            calibrated_worker_limit: 8,
            reserved_system_threads: 1,
            schedulable_compute_threads: 7,
            io_slots: 4,
            memory_bytes: 1_024,
            memory_headroom_percent: MEMORY_HEADROOM_PERCENT,
            admission_queue_capacity: admission_queue_capacity(7),
            foreground_burst_limit: FOREGROUND_BURST_LIMIT,
            class_limits: class_limits(GovernorMode::Mixed, 7, 4, 1_024),
        }
    }

    const fn request(compute_threads: u64, io_slots: u64, memory_bytes: u64) -> GovernorRequest {
        GovernorRequest {
            compute_threads,
            io_slots,
            memory_bytes,
        }
    }

    #[test]
    fn global_capacity_prevents_cross_class_oversubscription() -> Result<(), GovernorAdmissionError>
    {
        let governor = NativeResourceGovernor::new(test_policy());
        let foreground = governor.try_admit(WorkloadClass::ForegroundBounded, request(3, 0, 0))?;
        let bulk = governor.try_admit(WorkloadClass::Bulk, request(3, 0, 0))?;
        assert!(matches!(
            governor.try_admit(WorkloadClass::Maintenance, request(2, 0, 0),),
            Err(GovernorAdmissionError::GlobalCapacity)
        ));
        drop(bulk);
        drop(foreground);
        assert!(
            governor
                .try_admit(WorkloadClass::ForegroundBounded, request(3, 0, 0),)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn permit_drop_returns_all_global_and_class_resources() -> Result<(), GovernorAdmissionError> {
        let governor = NativeResourceGovernor::new(test_policy());
        let request = request(3, 2, 512);
        {
            let _permit = governor.try_admit(WorkloadClass::Bulk, request)?;
            assert!(matches!(
                governor.try_admit(WorkloadClass::Bulk, request),
                Err(GovernorAdmissionError::ClassCapacity)
            ));
        }
        assert!(governor.try_admit(WorkloadClass::Bulk, request).is_ok());
        Ok(())
    }

    #[test]
    fn owned_permit_keeps_authority_alive_and_returns_capacity()
    -> Result<(), GovernorAdmissionError> {
        let governor = Arc::new(NativeResourceGovernor::new(test_policy()));
        let request = request(3, 2, 512);
        let permit = governor.try_admit_owned(WorkloadClass::Bulk, request)?;
        let cloned_permit = permit.clone();
        let survivor = Arc::clone(&permit.allocation.governor);
        drop(governor);
        assert!(matches!(
            survivor.try_admit(WorkloadClass::Bulk, request),
            Err(GovernorAdmissionError::ClassCapacity)
        ));
        drop(permit);
        assert!(matches!(
            survivor.try_admit(WorkloadClass::Bulk, request),
            Err(GovernorAdmissionError::ClassCapacity)
        ));
        drop(cloned_permit);
        assert!(survivor.try_admit(WorkloadClass::Bulk, request).is_ok());
        Ok(())
    }

    #[test]
    fn owned_permit_atomically_retains_memory_while_releasing_compute_and_io()
    -> Result<(), GovernorAdmissionError> {
        let governor = Arc::new(NativeResourceGovernor::new(test_policy()));
        let permit = governor.try_admit_owned(WorkloadClass::Bulk, request(3, 2, 512))?;
        let permit = permit.shrink_to(request(0, 0, 512))?;
        assert_eq!(
            governor.usage_snapshot(),
            GovernorUsageSnapshot {
                compute_threads: 0,
                io_slots: 0,
                memory_bytes: 512,
                queued_requests: 0,
            }
        );
        let foreground = governor.try_admit_owned(WorkloadClass::Mutation, request(1, 1, 0))?;
        assert_eq!(governor.usage_snapshot().memory_bytes, 512);
        drop(foreground);
        drop(permit);
        assert_eq!(
            governor.usage_snapshot(),
            GovernorUsageSnapshot {
                compute_threads: 0,
                io_slots: 0,
                memory_bytes: 0,
                queued_requests: 0,
            }
        );
        Ok(())
    }

    #[test]
    fn nested_parallelism_can_only_subdivide_parent_tokens() -> Result<(), GovernorAdmissionError> {
        let governor = NativeResourceGovernor::new(test_policy());
        let parent = governor.try_admit(WorkloadClass::Bulk, request(4, 2, 512))?;
        let child = parent.try_subdivide(request(3, 1, 256))?;
        assert!(matches!(
            parent.try_subdivide(request(2, 1, 256)),
            Err(GovernorAdmissionError::ParentCapacity)
        ));
        drop(child);
        assert!(parent.try_subdivide(request(4, 2, 512)).is_ok());
        Ok(())
    }

    #[test]
    fn bounded_queue_prioritizes_foreground_before_waiting_background()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let governor = Arc::new(NativeResourceGovernor::new(test_policy()));
        let foreground_hold =
            governor.try_admit_owned(WorkloadClass::ForegroundBounded, request(4, 0, 0))?;
        let bulk_hold = governor.try_admit_owned(WorkloadClass::Bulk, request(2, 0, 0))?;
        let released_slot =
            governor.try_admit_owned(WorkloadClass::Maintenance, request(1, 0, 0))?;
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let background_governor = Arc::clone(&governor);
        let background_tx = admitted_tx.clone();
        let background = thread::spawn(move || {
            let cancellation = background_governor.cancellation_token();
            let permit = background_governor.admit_queued_owned(
                WorkloadClass::Administrative,
                request(1, 0, 0),
                Duration::from_secs(5),
                &cancellation,
            )?;
            background_tx.send(WorkloadClass::Administrative)?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(permit)
        });
        wait_for_queue_depth(&governor, 1);

        let foreground_governor = Arc::clone(&governor);
        let foreground = thread::spawn(move || {
            let cancellation = foreground_governor.cancellation_token();
            let permit = foreground_governor.admit_queued_owned(
                WorkloadClass::ForegroundPoint,
                request(1, 0, 0),
                Duration::from_secs(5),
                &cancellation,
            )?;
            admitted_tx.send(WorkloadClass::ForegroundPoint)?;
            release_rx.recv()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(permit)
        });
        wait_for_queue_depth(&governor, 2);

        drop(released_slot);
        assert_eq!(
            admitted_rx.recv_timeout(Duration::from_secs(1))?,
            WorkloadClass::ForegroundPoint
        );
        release_tx.send(())?;
        drop(
            foreground
                .join()
                .map_err(|_| std::io::Error::other("foreground thread panicked"))??,
        );
        assert_eq!(
            admitted_rx.recv_timeout(Duration::from_secs(1))?,
            WorkloadClass::Administrative
        );
        drop(
            background
                .join()
                .map_err(|_| std::io::Error::other("background thread panicked"))??,
        );
        drop(bulk_hold);
        drop(foreground_hold);
        Ok(())
    }

    #[test]
    fn cancellation_and_queue_capacity_release_every_waiting_slot()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut policy = test_policy();
        policy.admission_queue_capacity = 1;
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let foreground_hold =
            governor.try_admit_owned(WorkloadClass::ForegroundBounded, request(4, 0, 0))?;
        let bulk_hold = governor.try_admit_owned(WorkloadClass::Bulk, request(3, 0, 0))?;
        let cancellation = governor.cancellation_token();
        let queued_governor = Arc::clone(&governor);
        let queued_cancellation = cancellation.clone();
        let queued = thread::spawn(move || {
            queued_governor.admit_queued_owned(
                WorkloadClass::Maintenance,
                request(1, 0, 0),
                Duration::from_secs(5),
                &queued_cancellation,
            )
        });
        wait_for_queue_depth(&governor, 1);
        assert!(matches!(
            governor.admit_queued_owned(
                WorkloadClass::Administrative,
                request(1, 0, 0),
                Duration::ZERO,
                &governor.cancellation_token(),
            ),
            Err(GovernorQueueError::Full)
        ));
        cancellation.cancel();
        assert!(matches!(
            queued
                .join()
                .map_err(|_| std::io::Error::other("queued thread panicked"))?,
            Err(GovernorQueueError::Cancelled)
        ));
        assert_eq!(governor.queued_requests(), 0);
        drop(bulk_hold);
        drop(foreground_hold);
        Ok(())
    }

    #[test]
    fn foreground_burst_forces_background_progress()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut policy = test_policy();
        policy.foreground_burst_limit = 2;
        let governor = Arc::new(NativeResourceGovernor::new(policy));
        let foreground_hold =
            governor.try_admit_owned(WorkloadClass::ForegroundBounded, request(4, 0, 0))?;
        let bulk_hold = governor.try_admit_owned(WorkloadClass::Bulk, request(2, 0, 0))?;
        let point_hold =
            governor.try_admit_owned(WorkloadClass::ForegroundPoint, request(1, 0, 0))?;
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let mut releases = BTreeMap::new();
        let mut workers = Vec::new();

        for (identity, class) in [
            (100_u8, WorkloadClass::Administrative),
            (0, WorkloadClass::ForegroundPoint),
            (1, WorkloadClass::ForegroundPoint),
            (2, WorkloadClass::ForegroundPoint),
        ] {
            let worker_governor = Arc::clone(&governor);
            let worker_tx = admitted_tx.clone();
            let (release_tx, release_rx) = mpsc::channel();
            releases.insert(identity, release_tx);
            workers.push(thread::spawn(move || {
                let cancellation = worker_governor.cancellation_token();
                let permit = worker_governor.admit_queued_owned(
                    class,
                    request(1, 0, 0),
                    Duration::from_secs(5),
                    &cancellation,
                )?;
                worker_tx.send(identity)?;
                release_rx.recv()?;
                drop(permit);
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }));
            wait_for_queue_depth(&governor, workers.len());
        }

        drop(point_hold);
        for expected in [0_u8, 1, 100, 2] {
            let admitted = admitted_rx.recv_timeout(Duration::from_secs(1))?;
            assert_eq!(admitted, expected);
            releases
                .remove(&admitted)
                .ok_or("missing release sender")?
                .send(())?;
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| std::io::Error::other("queue worker panicked"))??;
        }
        drop(bulk_hold);
        drop(foreground_hold);
        Ok(())
    }

    #[test]
    fn timeout_and_receipts_are_componentized_and_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let governor = Arc::new(NativeResourceGovernor::new(test_policy()));
        let immediate = governor.admit_queued_owned(
            WorkloadClass::ForegroundPoint,
            request(1, 0, 0),
            Duration::from_secs(1),
            &governor.cancellation_token(),
        )?;
        assert_eq!(immediate.admission().queue_time, Duration::ZERO);
        let mut immediate = immediate;
        immediate.record_io(Duration::from_micros(7));
        let receipt = immediate.finish();
        assert_eq!(receipt.io_time, Duration::from_micros(7));
        assert_eq!(receipt.admission.class, WorkloadClass::ForegroundPoint);
        assert_eq!(receipt.admission.request, request(1, 0, 0));

        let foreground_hold =
            governor.try_admit_owned(WorkloadClass::ForegroundBounded, request(4, 0, 0))?;
        let bulk_hold = governor.try_admit_owned(WorkloadClass::Bulk, request(3, 0, 0))?;
        assert!(matches!(
            governor.admit_queued_owned(
                WorkloadClass::Maintenance,
                request(1, 0, 0),
                Duration::from_millis(1),
                &governor.cancellation_token(),
            ),
            Err(GovernorQueueError::TimedOut)
        ));
        assert_eq!(governor.queued_requests(), 0);
        let foreign = Arc::new(NativeResourceGovernor::new(test_policy()));
        assert!(matches!(
            governor.admit_queued_owned(
                WorkloadClass::Maintenance,
                request(1, 0, 0),
                Duration::ZERO,
                &foreign.cancellation_token(),
            ),
            Err(GovernorQueueError::ForeignCancellation)
        ));
        drop(bulk_hold);
        drop(foreground_hold);
        Ok(())
    }

    #[test]
    fn concurrency_1_8_32_and_saturation_never_exceed_global_tokens()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for concurrency in [1_usize, 8, 32, 64] {
            let governor = Arc::new(NativeResourceGovernor::new(test_policy()));
            let start = Arc::new(Barrier::new(concurrency + 1));
            let active = Arc::new(AtomicU64::new(0));
            let peak = Arc::new(AtomicU64::new(0));
            let (receipt_tx, receipt_rx) = mpsc::channel();
            let mut workers = Vec::with_capacity(concurrency);
            for worker in 0..concurrency {
                let worker_governor = Arc::clone(&governor);
                let worker_start = Arc::clone(&start);
                let worker_active = Arc::clone(&active);
                let worker_peak = Arc::clone(&peak);
                let worker_receipt = receipt_tx.clone();
                workers.push(thread::spawn(move || {
                    let classes = [
                        WorkloadClass::ForegroundPoint,
                        WorkloadClass::ForegroundBounded,
                        WorkloadClass::Mutation,
                        WorkloadClass::Bulk,
                        WorkloadClass::Maintenance,
                        WorkloadClass::Recovery,
                        WorkloadClass::Administrative,
                    ];
                    worker_start.wait();
                    let cancellation = worker_governor.cancellation_token();
                    let permit = worker_governor.admit_queued_owned(
                        classes[worker % classes.len()],
                        request(1, 0, 1),
                        Duration::from_secs(5),
                        &cancellation,
                    )?;
                    let observed = worker_active.fetch_add(1, Ordering::AcqRel) + 1;
                    worker_peak.fetch_max(observed, Ordering::AcqRel);
                    thread::yield_now();
                    worker_active.fetch_sub(1, Ordering::AcqRel);
                    worker_receipt.send(permit.finish())?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                }));
            }
            start.wait();
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| std::io::Error::other("stress worker panicked"))??;
            }
            let receipts = receipt_rx.try_iter().collect::<Vec<_>>();
            assert_eq!(receipts.len(), concurrency);
            assert!(peak.load(Ordering::Acquire) <= governor.policy().schedulable_compute_threads);
            if concurrency == 64 {
                assert!(
                    receipts
                        .iter()
                        .any(|receipt| !receipt.admission.queue_time.is_zero())
                );
            }
            let usage = governor.usage_snapshot();
            assert_eq!(usage.compute_threads, 0);
            assert_eq!(usage.io_slots, 0);
            assert_eq!(usage.memory_bytes, 0);
            assert_eq!(usage.queued_requests, 0);
        }
        Ok(())
    }

    fn wait_for_queue_depth(governor: &NativeResourceGovernor, expected: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while governor.queued_requests() != expected {
            assert!(std::time::Instant::now() < deadline, "queue depth timeout");
            thread::yield_now();
        }
    }

    #[test]
    fn policy_helpers_preserve_headroom_and_reserve() {
        assert_eq!(percentage(1_000, 85), 850);
        assert_eq!(system_thread_reserve(1), 0);
        assert_eq!(system_thread_reserve(96), 8);
        let limits = class_limits(GovernorMode::Latency, 40, 32, 1_000);
        assert_eq!(
            limits[WorkloadClass::ForegroundPoint.index()].compute_threads,
            1
        );
        assert_eq!(
            limits[WorkloadClass::ForegroundBounded.index()].compute_threads,
            40
        );
        assert_eq!(limits[WorkloadClass::Bulk.index()].compute_threads, 10);
    }

    #[test]
    fn policy_derivation_consumes_the_verified_scaling_curve() -> Result<(), GovernorPolicyError> {
        let profile = hardware_profile();
        let calibration = calibration(&profile);
        let policy = NativeGovernorPolicy::derive(&profile, &calibration, GovernorMode::Mixed)?;
        assert_eq!(policy.schema, GOVERNOR_POLICY_SCHEMA);
        assert_eq!(policy.calibrated_worker_limit, 8);
        assert_eq!(policy.reserved_system_threads, 1);
        assert_eq!(policy.schedulable_compute_threads, 7);
        assert_eq!(policy.io_slots, 4);
        assert_eq!(policy.memory_bytes, 750);
        Ok(())
    }
}
