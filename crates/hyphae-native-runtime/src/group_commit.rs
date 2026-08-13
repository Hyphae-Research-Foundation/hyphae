// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded multi-producer scheduler for native group durability.

use std::{
    ops::Deref,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_types::DurabilityClass;
use thiserror::Error;

#[cfg(test)]
use crate::CommitBoundary;
use crate::{
    CommitReceipt, GovernorCancellation, GroupCommitOutcome, GroupCommitReport,
    MAX_EXPIRY_SWEEP_KEYS, MAX_GROUP_COMMIT_BATCH_SIZE, NativeCommitBatch, NativeDatabase,
    NativeDeltaWriteBatch, NativeResourceGovernor, NativeRuntimeError, NativeWriteBatch,
};

const DEFAULT_GROUP_COMMIT_BATCH_SIZE: usize = 32;
const DEFAULT_GROUP_COMMIT_WAIT: Duration = Duration::from_micros(200);
const DEFAULT_GROUP_COMMIT_QUEUE_CAPACITY: usize = 1_024;
const GROUP_COMMIT_EXECUTION_ADMISSION_WAIT: Duration = Duration::from_secs(1);
const REQUEST_QUEUED: u8 = 0;
const REQUEST_EXECUTING: u8 = 1;
const REQUEST_CANCELLED: u8 = 2;
const REQUEST_COMPLETED: u8 = 3;

/// Shortest accepted active-expiry interval.
pub const MIN_ACTIVE_EXPIRY_INTERVAL: Duration = Duration::from_micros(100);
/// Longest accepted active-expiry interval.
pub const MAX_ACTIVE_EXPIRY_INTERVAL: Duration = Duration::from_secs(60);
/// Largest foreground request budget before a due sweep.
pub const MAX_ACTIVE_EXPIRY_FOREGROUND_BUDGET: usize = 4_096;

/// Longest collection interval accepted by the first native scheduler.
pub const MAX_GROUP_COMMIT_WAIT: Duration = Duration::from_millis(10);
/// Longest bounded wait accepted for shared cohort execution resources.
pub const MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT: Duration = Duration::from_secs(60);
/// Largest bounded submission queue accepted by the first native scheduler.
pub const MAX_GROUP_COMMIT_QUEUE_CAPACITY: usize = 4_096;

/// Invalid native group-commit scheduler bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GroupCommitConfigError {
    /// The cohort bound is zero or exceeds the runtime's hard maximum.
    #[error(
        "native group commit batch size {requested} is outside 1..={MAX_GROUP_COMMIT_BATCH_SIZE}"
    )]
    BatchSize {
        /// Rejected cohort bound.
        requested: usize,
    },
    /// The collection interval exceeds the first scheduler's hard maximum.
    #[error("native group commit wait {requested:?} exceeds {MAX_GROUP_COMMIT_WAIT:?}")]
    Wait {
        /// Rejected collection interval.
        requested: Duration,
    },
    /// The queue cannot hold one cohort or exceeds its hard maximum.
    #[error(
        "native group commit queue capacity {requested} must be at least {minimum} and at most {MAX_GROUP_COMMIT_QUEUE_CAPACITY}"
    )]
    QueueCapacity {
        /// Rejected queue capacity.
        requested: usize,
        /// Minimum capacity required for the configured cohort.
        minimum: usize,
    },
}

/// Invalid independent execution-admission wait for a commit cohort.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "native group commit execution admission wait {requested:?} exceeds {MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT:?}"
)]
pub struct GroupCommitExecutionAdmissionWaitError {
    /// Rejected execution admission wait.
    pub requested: Duration,
}

/// Validated bounds for one native group-commit scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupCommitConfig {
    max_batch_size: usize,
    max_wait: Duration,
    queue_capacity: usize,
    execution_admission_wait: Duration,
}

impl GroupCommitConfig {
    /// Validates explicit scheduler bounds.
    ///
    /// A zero wait is valid and produces immediate singleton-or-ready cohorts.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an invalid cohort, wait, or queue bound.
    pub const fn new(
        max_batch_size: usize,
        max_wait: Duration,
        queue_capacity: usize,
    ) -> Result<Self, GroupCommitConfigError> {
        if max_batch_size == 0 || max_batch_size > MAX_GROUP_COMMIT_BATCH_SIZE {
            return Err(GroupCommitConfigError::BatchSize {
                requested: max_batch_size,
            });
        }
        if max_wait.as_nanos() > MAX_GROUP_COMMIT_WAIT.as_nanos() {
            return Err(GroupCommitConfigError::Wait {
                requested: max_wait,
            });
        }
        if queue_capacity < max_batch_size || queue_capacity > MAX_GROUP_COMMIT_QUEUE_CAPACITY {
            return Err(GroupCommitConfigError::QueueCapacity {
                requested: queue_capacity,
                minimum: max_batch_size,
            });
        }
        Ok(Self {
            max_batch_size,
            max_wait,
            queue_capacity,
            execution_admission_wait: GROUP_COMMIT_EXECUTION_ADMISSION_WAIT,
        })
    }

    /// Returns the largest cohort admitted by the worker.
    pub const fn max_batch_size(self) -> usize {
        self.max_batch_size
    }

    /// Returns the collection interval beginning with the first request.
    pub const fn max_wait(self) -> Duration {
        self.max_wait
    }

    /// Returns the bounded capacity measured in logical commit requests.
    ///
    /// An explicit cohort consumes one slot per member while remaining one
    /// atomic scheduler command.
    pub const fn queue_capacity(self) -> usize {
        self.queue_capacity
    }

    /// Sets the bounded wait for the cohort's shared compute and I/O permit.
    ///
    /// This bound is independent from the micro-batching collection interval.
    ///
    /// # Errors
    ///
    /// Rejects waits above [`MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT`].
    pub const fn with_execution_admission_wait(
        mut self,
        maximum_wait: Duration,
    ) -> Result<Self, GroupCommitExecutionAdmissionWaitError> {
        if maximum_wait.as_nanos() > MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT.as_nanos() {
            return Err(GroupCommitExecutionAdmissionWaitError {
                requested: maximum_wait,
            });
        }
        self.execution_admission_wait = maximum_wait;
        Ok(self)
    }
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            max_batch_size: DEFAULT_GROUP_COMMIT_BATCH_SIZE,
            max_wait: DEFAULT_GROUP_COMMIT_WAIT,
            queue_capacity: DEFAULT_GROUP_COMMIT_QUEUE_CAPACITY,
            execution_admission_wait: GROUP_COMMIT_EXECUTION_ADMISSION_WAIT,
        }
    }
}

/// Invalid native active-expiry scheduler bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ActiveExpiryConfigError {
    /// The timer interval is outside the accepted finite range.
    #[error(
        "native active expiry interval {requested:?} is outside {MIN_ACTIVE_EXPIRY_INTERVAL:?}..={MAX_ACTIVE_EXPIRY_INTERVAL:?}"
    )]
    Interval {
        /// Rejected timer interval.
        requested: Duration,
    },
    /// The physical tombstone batch is empty or exceeds the runtime bound.
    #[error("native active expiry batch size {requested} is outside 1..={MAX_EXPIRY_SWEEP_KEYS}")]
    BatchSize {
        /// Rejected batch size.
        requested: usize,
    },
    /// Group durability has no background maintenance cohort.
    #[error("native active expiry durability {requested:?} must be memory or strict")]
    Durability {
        /// Rejected durability class.
        requested: DurabilityClass,
    },
    /// The foreground fairness budget is zero or exceeds its hard maximum.
    #[error(
        "native active expiry foreground budget {requested} is outside 1..={MAX_ACTIVE_EXPIRY_FOREGROUND_BUDGET}"
    )]
    ForegroundBudget {
        /// Rejected foreground request budget.
        requested: usize,
    },
}

/// Validated active-expiry resource policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveExpiryConfig {
    interval: Duration,
    max_keys: usize,
    durability: DurabilityClass,
    foreground_budget: usize,
}

impl ActiveExpiryConfig {
    /// Validates one optional active-expiry policy.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an invalid interval, batch, durability, or
    /// foreground fairness bound.
    pub const fn new(
        interval: Duration,
        max_keys: usize,
        durability: DurabilityClass,
        foreground_budget: usize,
    ) -> Result<Self, ActiveExpiryConfigError> {
        if interval.as_nanos() < MIN_ACTIVE_EXPIRY_INTERVAL.as_nanos()
            || interval.as_nanos() > MAX_ACTIVE_EXPIRY_INTERVAL.as_nanos()
        {
            return Err(ActiveExpiryConfigError::Interval {
                requested: interval,
            });
        }
        if max_keys == 0 || max_keys > MAX_EXPIRY_SWEEP_KEYS {
            return Err(ActiveExpiryConfigError::BatchSize {
                requested: max_keys,
            });
        }
        if matches!(durability, DurabilityClass::Group) {
            return Err(ActiveExpiryConfigError::Durability {
                requested: durability,
            });
        }
        if foreground_budget == 0 || foreground_budget > MAX_ACTIVE_EXPIRY_FOREGROUND_BUDGET {
            return Err(ActiveExpiryConfigError::ForegroundBudget {
                requested: foreground_budget,
            });
        }
        Ok(Self {
            interval,
            max_keys,
            durability,
            foreground_budget,
        })
    }

    /// Returns the interval between completed sweep attempts.
    pub const fn interval(self) -> Duration {
        self.interval
    }

    /// Returns the maximum scalar keys tombstoned by one sweep.
    pub const fn max_keys(self) -> usize {
        self.max_keys
    }

    /// Returns singleton durability used by non-empty sweeps.
    pub const fn durability(self) -> DurabilityClass {
        self.durability
    }

    /// Returns foreground requests admitted after a due deadline.
    pub const fn foreground_budget(self) -> usize {
        self.foreground_budget
    }
}

/// Thread-safe absolute-microsecond authority for active expiry.
pub trait NativeSchedulerClock: Send + Sync {
    /// Returns one signed absolute logical-time sample.
    fn logical_time_micros(&self) -> i64;
}

struct SystemSchedulerClock;

impl NativeSchedulerClock for SystemSchedulerClock {
    fn logical_time_micros(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since_epoch) => i64::try_from(since_epoch.as_micros()).unwrap_or(i64::MAX),
            Err(before_epoch) => {
                let magnitude =
                    i64::try_from(before_epoch.duration().as_micros()).unwrap_or(i64::MAX);
                magnitude.saturating_neg()
            }
        }
    }
}

/// Lock-free diagnostic snapshot for one active-expiry worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveExpiryStats {
    /// Timer-driven sweep attempts.
    pub attempted_sweeps: u64,
    /// Non-empty sweeps that committed a tombstone transaction.
    pub committed_sweeps: u64,
    /// Scalar identities tombstoned by committed sweeps.
    pub expired_keys: u64,
    /// Sweep attempts that found no due scalar identity.
    pub empty_sweeps: u64,
    /// Fatal sweep attempts.
    pub failures: u64,
    /// Latest non-decreasing logical-time sample.
    pub latest_logical_time_micros: i64,
    /// Complete duration of the latest attempted sweep.
    pub latest_sweep_duration: Duration,
    /// Largest foreground request count observed after a due deadline.
    pub max_foreground_after_due: usize,
}

/// Terminal reason one active-expiry worker stopped.
#[derive(Clone, Debug, Error)]
pub enum ActiveExpiryFailure {
    /// The scheduler no longer owned an accessible database.
    #[error("native active expiry database is unavailable")]
    DatabaseUnavailable,
    /// Native persistence rejected the expiry transaction.
    #[error("native active expiry failed: {source}")]
    Runtime {
        /// Shared typed runtime failure retained after the worker stops.
        #[source]
        source: Arc<NativeRuntimeError>,
    },
}

impl ActiveExpiryFailure {
    fn submit_error(&self) -> GroupCommitSubmitError {
        match self {
            Self::DatabaseUnavailable => GroupCommitSubmitError::Unavailable,
            Self::Runtime { source } => GroupCommitSubmitError::Runtime {
                source: Arc::clone(source),
            },
        }
    }
}

#[derive(Default)]
struct ActiveExpiryMetrics {
    attempted_sweeps: AtomicU64,
    committed_sweeps: AtomicU64,
    expired_keys: AtomicU64,
    empty_sweeps: AtomicU64,
    failures: AtomicU64,
    latest_logical_time_micros: AtomicI64,
    latest_sweep_nanos: AtomicU64,
    max_foreground_after_due: AtomicUsize,
    terminal_failure: OnceLock<ActiveExpiryFailure>,
}

impl ActiveExpiryMetrics {
    fn snapshot(&self) -> ActiveExpiryStats {
        ActiveExpiryStats {
            attempted_sweeps: self.attempted_sweeps.load(Ordering::Acquire),
            committed_sweeps: self.committed_sweeps.load(Ordering::Acquire),
            expired_keys: self.expired_keys.load(Ordering::Acquire),
            empty_sweeps: self.empty_sweeps.load(Ordering::Acquire),
            failures: self.failures.load(Ordering::Acquire),
            latest_logical_time_micros: self.latest_logical_time_micros.load(Ordering::Acquire),
            latest_sweep_duration: Duration::from_nanos(
                self.latest_sweep_nanos.load(Ordering::Acquire),
            ),
            max_foreground_after_due: self.max_foreground_after_due.load(Ordering::Acquire),
        }
    }
}

struct ActiveExpiryRuntime {
    config: ActiveExpiryConfig,
    clock: Arc<dyn NativeSchedulerClock>,
    metrics: Arc<ActiveExpiryMetrics>,
    next_deadline: Instant,
    logical_watermark: i64,
    foreground_after_due: usize,
    #[cfg(test)]
    interruption: Option<CommitBoundary>,
}

impl ActiveExpiryRuntime {
    fn new(
        config: ActiveExpiryConfig,
        clock: Arc<dyn NativeSchedulerClock>,
        metrics: Arc<ActiveExpiryMetrics>,
    ) -> Self {
        Self {
            config,
            clock,
            metrics,
            next_deadline: Instant::now() + config.interval,
            logical_watermark: i64::MIN,
            foreground_after_due: 0,
            #[cfg(test)]
            interruption: None,
        }
    }

    fn wait_until_deadline(&self) -> Duration {
        self.next_deadline.saturating_duration_since(Instant::now())
    }

    fn should_force_sweep(&self) -> bool {
        self.wait_until_deadline().is_zero()
            && self.foreground_after_due >= self.config.foreground_budget
    }

    fn record_foreground(&mut self, requests: usize) {
        if self.wait_until_deadline().is_zero() {
            self.foreground_after_due = self.foreground_after_due.saturating_add(requests);
            self.metrics
                .max_foreground_after_due
                .fetch_max(self.foreground_after_due, Ordering::AcqRel);
        }
    }

    fn begin_sweep(&mut self) -> i64 {
        let sample = self.clock.logical_time_micros();
        self.logical_watermark = self.logical_watermark.max(sample);
        atomic_saturating_add(&self.metrics.attempted_sweeps, 1);
        self.metrics
            .latest_logical_time_micros
            .store(self.logical_watermark, Ordering::Release);
        self.logical_watermark
    }

    fn finish_sweep(&mut self, started: Instant) {
        let elapsed_nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics
            .latest_sweep_nanos
            .store(elapsed_nanos, Ordering::Release);
        self.foreground_after_due = 0;
        self.next_deadline = Instant::now() + self.config.interval;
    }
}

/// Failure returned to one group-commit scheduler client.
#[derive(Clone, Debug, Error)]
pub enum GroupCommitSubmitError {
    /// The scheduler stopped, failed, or no longer accepts submissions.
    #[error("native group commit scheduler is unavailable")]
    Unavailable,
    /// Immediate bounded admission found a full queue.
    #[error("native commit scheduler queue is saturated")]
    Saturated,
    /// The queue deadline elapsed before physical execution began.
    #[error("native commit scheduler queue deadline exceeded")]
    DeadlineExceeded,
    /// Explicit cancellation won before physical execution began.
    #[error("native commit scheduler request was cancelled")]
    Cancelled,
    /// Native admission or persistence rejected the submitted transaction.
    #[error("native group commit request failed: {source}")]
    Runtime {
        /// Shared typed runtime failure.
        #[source]
        source: Arc<NativeRuntimeError>,
    },
}

/// Result of attempting to cancel one controlled scheduler request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitCancellationOutcome {
    /// Cancellation won while the request was queued.
    Cancelled,
    /// Physical execution already owns the request.
    TooLate,
    /// The request already has a definite outcome.
    Completed,
}

/// One-use control state for exact queued scheduler cancellation.
pub struct NativeCommitControl {
    state: Arc<AtomicU8>,
}

impl NativeCommitControl {
    /// Creates control state for one scheduler submission.
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(REQUEST_QUEUED)),
        }
    }

    /// Returns a cloneable handle that may cancel this queued request.
    pub fn cancellation(&self) -> NativeCommitCancellation {
        NativeCommitCancellation {
            state: Arc::clone(&self.state),
        }
    }

    fn claim_execution(&self) -> bool {
        self.state
            .compare_exchange(
                REQUEST_QUEUED,
                REQUEST_EXECUTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn complete(&self) {
        self.state.store(REQUEST_COMPLETED, Ordering::Release);
    }
}

impl Default for NativeCommitControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Cloneable cancellation capability for one controlled scheduler request.
#[derive(Clone)]
pub struct NativeCommitCancellation {
    state: Arc<AtomicU8>,
}

impl NativeCommitCancellation {
    /// Cancels the request only while it remains queued.
    pub fn cancel(&self) -> CommitCancellationOutcome {
        cancel_state(&self.state)
    }
}

fn cancel_state(state: &AtomicU8) -> CommitCancellationOutcome {
    match state.compare_exchange(
        REQUEST_QUEUED,
        REQUEST_CANCELLED,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) | Err(REQUEST_CANCELLED) => CommitCancellationOutcome::Cancelled,
        Err(REQUEST_COMPLETED) => CommitCancellationOutcome::Completed,
        Err(_) => CommitCancellationOutcome::TooLate,
    }
}

/// Per-request timing and durability receipt produced by the scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledCommitReceipt {
    /// Independent native transaction receipt.
    pub commit: CommitReceipt,
    /// Time between submission and bounded queue insertion.
    pub admission_wait: Duration,
    /// Time between bounded queue insertion and execution claim.
    pub queue_wait: Duration,
    /// Complete database-side cohort execution time.
    pub cohort_execution: Duration,
    /// Time spent in the cohort's shared page-file synchronization.
    pub page_synchronization: Duration,
    /// Time spent in the cohort's shared WAL synchronization.
    pub wal_synchronization: Duration,
    /// Caller-observed submission-to-response time.
    pub end_to_end: Duration,
}

impl Deref for ScheduledCommitReceipt {
    type Target = CommitReceipt;

    fn deref(&self) -> &Self::Target {
        &self.commit
    }
}

/// Scheduler completion with exact physical synchronization counts.
///
/// This additive envelope preserves [`ScheduledCommitReceipt`] while exposing
/// cohort-level evidence that cannot be inferred from synchronization timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledCommitCompletion {
    /// Per-request scheduler and transaction receipt.
    pub receipt: ScheduledCommitReceipt,
    /// Physical page-file synchronizations performed for the cohort.
    pub page_synchronizations: usize,
    /// Physical WAL synchronizations performed for the cohort.
    pub wal_synchronizations: usize,
}

impl Deref for ScheduledCommitCompletion {
    type Target = ScheduledCommitReceipt;

    fn deref(&self) -> &Self::Target {
        &self.receipt
    }
}

/// Typed ownership of one accepted scheduler request.
///
/// The handle exposes exact queued cancellation and a definite wait without
/// leaking the scheduler's internal response channel. Dropping it requests
/// cancellation while the request is still queued.
pub struct NativePendingCommit {
    receiver: Receiver<Result<ScheduledCommitCompletion, GroupCommitSubmitError>>,
    state: Arc<AtomicU8>,
    #[cfg(test)]
    submitted_at: Instant,
    queue_deadline: Option<Instant>,
    poll_cancellation: bool,
    cancel_on_drop: bool,
}

impl NativePendingCommit {
    /// Returns a cloneable cancellation capability for this queued request.
    pub fn cancellation(&self) -> NativeCommitCancellation {
        NativeCommitCancellation {
            state: Arc::clone(&self.state),
        }
    }

    /// Cancels the request only if physical execution has not claimed it.
    pub fn cancel(&self) -> CommitCancellationOutcome {
        cancel_state(&self.state)
    }

    #[cfg(test)]
    pub(crate) fn completed_for_test(&self) -> bool {
        self.state.load(Ordering::Acquire) == REQUEST_COMPLETED
    }

    #[cfg(test)]
    pub(crate) fn submitted_at_for_test(&self) -> Instant {
        self.submitted_at
    }

    /// Waits for the request's definite scheduler outcome.
    ///
    /// # Errors
    ///
    /// Returns the request's typed cancellation, deadline, scheduler, or
    /// native execution failure.
    pub fn wait(self) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.wait_with_evidence()
            .map(|completion| completion.receipt)
    }

    /// Waits for the definite outcome and exact cohort synchronization evidence.
    ///
    /// # Errors
    ///
    /// Returns the request's typed cancellation, deadline, scheduler, or
    /// native execution failure. Failed requests never produce completion
    /// evidence.
    pub fn wait_with_evidence(
        mut self,
    ) -> Result<ScheduledCommitCompletion, GroupCommitSubmitError> {
        self.cancel_on_drop = false;
        await_response(
            &self.receiver,
            &self.state,
            self.queue_deadline,
            self.poll_cancellation,
        )
    }
}

impl Drop for NativePendingCommit {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            cancel_state(&self.state);
        }
    }
}

impl GroupCommitSubmitError {
    fn runtime(source: NativeRuntimeError) -> Self {
        Self::Runtime {
            source: Arc::new(source),
        }
    }

    /// Returns the underlying native failure when one is available.
    pub fn runtime_error(&self) -> Option<&NativeRuntimeError> {
        match self {
            Self::Unavailable | Self::Saturated | Self::DeadlineExceeded | Self::Cancelled => None,
            Self::Runtime { source } => Some(source),
        }
    }
}

type CommitResponse = SyncSender<Result<ScheduledCommitCompletion, GroupCommitSubmitError>>;
type CommitWaiter = (Instant, Instant, NativeCommitControl, CommitResponse);

struct CommitRequest {
    batch: NativeWriteBatch,
    submitted_at: Instant,
    enqueued_at: Instant,
    control: NativeCommitControl,
    response: CommitResponse,
}

enum SchedulerCommand {
    Commit(Box<CommitRequest>),
    Cohort(Vec<CommitRequest>),
    Shutdown,
}

struct SubmissionGate {
    sender: SyncSender<SchedulerCommand>,
    accepting: bool,
    queue_capacity: usize,
    queued_request_slots: usize,
}

#[cfg(test)]
struct SchedulerTestGates {
    cohort_collection: Arc<RwLock<()>>,
    commit_execution: Arc<RwLock<()>>,
    completion_return: Arc<RwLock<()>>,
}

/// Cloneable producer for one native group-commit scheduler.
#[derive(Clone)]
pub struct NativeCommitClient {
    gate: Arc<Mutex<SubmissionGate>>,
    database: Arc<RwLock<Option<NativeDatabase>>>,
    execution_cancellation: Option<GovernorCancellation>,
    resource_admission_wait: Duration,
    maximum_explicit_cohort_size: usize,
    #[cfg(test)]
    cohort_collection_gate: Arc<RwLock<()>>,
    #[cfg(test)]
    commit_execution_gate: Arc<RwLock<()>>,
    #[cfg(test)]
    completion_return_gate: Arc<RwLock<()>>,
}

impl NativeCommitClient {
    /// Prepares a detached batch against the scheduler's current visible root.
    ///
    /// Multiple clients may prepare under shared read access while no cohort is
    /// being physically published.
    ///
    /// # Errors
    ///
    /// Returns unavailable after shutdown/failure, or a typed native snapshot
    /// and materialization error.
    pub fn begin_optimistic(
        &self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeWriteBatch, GroupCommitSubmitError> {
        if !self.accepting()? {
            return Err(GroupCommitSubmitError::Unavailable);
        }
        let database = self
            .database
            .read()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        database
            .as_ref()
            .ok_or(GroupCommitSubmitError::Unavailable)?
            .begin_optimistic_scheduled(
                logical_time_micros,
                durability,
                self.resource_admission_wait,
                self.execution_cancellation.as_ref(),
            )
            .map_err(GroupCommitSubmitError::runtime)
    }

    /// Prepares one point-resolved all-engine delta batch without materializing
    /// complete engine state.
    ///
    /// # Errors
    ///
    /// Returns unavailable after shutdown/failure, or a typed native snapshot
    /// or physical-format error.
    pub fn begin_optimistic_delta(
        &self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeDeltaWriteBatch, GroupCommitSubmitError> {
        if !self.accepting()? {
            return Err(GroupCommitSubmitError::Unavailable);
        }
        let database = self
            .database
            .read()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        database
            .as_ref()
            .ok_or(GroupCommitSubmitError::Unavailable)?
            .begin_optimistic_delta_scheduled(
                logical_time_micros,
                durability,
                self.resource_admission_wait,
                self.execution_cancellation.as_ref(),
            )
            .map_err(GroupCommitSubmitError::runtime)
    }

    /// Hydrates and stages one scalar SET in a delta batch.
    ///
    /// # Errors
    ///
    /// Returns unavailable after shutdown/failure, or a typed structure or
    /// physical corruption error.
    pub fn stage_delta_set(
        &self,
        batch: &mut NativeDeltaWriteBatch,
        key: Vec<u8>,
        value: Vec<u8>,
        expires_at_micros: Option<i64>,
    ) -> Result<(), GroupCommitSubmitError> {
        if !self.accepting()? {
            return Err(GroupCommitSubmitError::Unavailable);
        }
        let database = self
            .database
            .read()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        database
            .as_ref()
            .ok_or(GroupCommitSubmitError::Unavailable)?
            .stage_delta_set(batch, key, value, expires_at_micros)
            .map_err(GroupCommitSubmitError::runtime)
    }

    /// Submits one detached `group` batch and waits for its own outcome.
    ///
    /// # Errors
    ///
    /// Returns unavailable for a stopped worker or the request's typed native
    /// admission/persistence failure.
    pub fn submit(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.enqueue_inner(
            batch.into().into_inner(),
            NativeCommitControl::new(),
            None,
            false,
            false,
        )?
        .wait()
    }

    /// Enqueues one detached batch and returns typed wait/cancellation ownership.
    ///
    /// Queue ownership retains only the batch's bounded memory allocation;
    /// compute and I/O admission are acquired once by the executing cohort.
    ///
    /// # Errors
    ///
    /// Returns unavailable for a stopped worker, or a typed native failure if
    /// the prepared batch cannot be converted to memory-only queue ownership.
    pub fn enqueue(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<NativePendingCommit, GroupCommitSubmitError> {
        self.enqueue_inner(
            batch.into().into_inner(),
            NativeCommitControl::new(),
            None,
            false,
            false,
        )
    }

    /// Atomically enqueues one explicit group-durability cohort.
    ///
    /// The returned handles preserve request order. The worker executes this
    /// command as an isolated FIFO barrier with one page and WAL synchronization
    /// for all requests that remain uncancelled when execution is claimed.
    /// Dropping or cancelling an individual handle before that claim excludes
    /// only that transaction from the cohort.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversized cohort, any non-group batch, any failed
    /// memory-only retention, or a stopped scheduler before inserting a command.
    /// Pre-insertion rejection performs no mutation and produces no evidence.
    pub fn enqueue_cohort(
        &self,
        batches: Vec<NativeCommitBatch>,
    ) -> Result<Vec<NativePendingCommit>, GroupCommitSubmitError> {
        let batches = batches
            .into_iter()
            .map(NativeCommitBatch::into_inner)
            .collect::<Vec<_>>();
        validate_explicit_cohort(&batches, self.maximum_explicit_cohort_size)?;
        let batches = batches
            .into_iter()
            .map(NativeWriteBatch::retain_scheduler_queue_memory)
            .collect::<Result<Vec<_>, _>>()
            .map_err(GroupCommitSubmitError::runtime)?;
        let submitted_at = Instant::now();
        let mut requests = Vec::with_capacity(batches.len());
        let mut pending = Vec::with_capacity(batches.len());
        for batch in batches {
            let control = NativeCommitControl::new();
            let state = Arc::clone(&control.state);
            let (response, receiver) = mpsc::sync_channel(1);
            requests.push(CommitRequest {
                batch,
                submitted_at,
                enqueued_at: submitted_at,
                control,
                response,
            });
            pending.push(NativePendingCommit {
                receiver,
                state,
                #[cfg(test)]
                submitted_at,
                queue_deadline: None,
                poll_cancellation: false,
                cancel_on_drop: true,
            });
        }
        admit_cohort_command(&self.gate, requests)?;
        Ok(pending)
    }

    /// Attempts immediate bounded admission and waits for a definite outcome.
    ///
    /// # Errors
    ///
    /// Returns [`GroupCommitSubmitError::Saturated`] without mutation when the
    /// bounded queue has no slot, or the same errors as [`Self::submit`].
    pub fn try_submit(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.enqueue_inner(
            batch.into().into_inner(),
            NativeCommitControl::new(),
            None,
            true,
            false,
        )?
        .wait()
    }

    /// Submits with exact queued cancellation and an optional queue deadline.
    ///
    /// Once execution claims the request, this method waits for its definite
    /// outcome even when the queue deadline subsequently elapses.
    ///
    /// # Errors
    ///
    /// Returns cancelled or deadline exceeded only before physical execution,
    /// or the same unavailable/runtime failures as [`Self::submit`].
    pub fn submit_controlled(
        &self,
        batch: impl Into<NativeCommitBatch>,
        control: NativeCommitControl,
        queue_deadline: Option<Instant>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.enqueue_inner(
            batch.into().into_inner(),
            control,
            queue_deadline,
            false,
            true,
        )?
        .wait()
    }

    fn enqueue_inner(
        &self,
        batch: NativeWriteBatch,
        control: NativeCommitControl,
        queue_deadline: Option<Instant>,
        immediate: bool,
        poll_cancellation: bool,
    ) -> Result<NativePendingCommit, GroupCommitSubmitError> {
        let submitted_at = Instant::now();
        let batch = batch
            .retain_scheduler_queue_memory()
            .map_err(GroupCommitSubmitError::runtime)?;
        let (response, receiver) = mpsc::sync_channel(1);
        let state = Arc::clone(&control.state);
        admit_command(
            &self.gate,
            SchedulerCommand::Commit(Box::new(CommitRequest {
                batch,
                submitted_at,
                enqueued_at: submitted_at,
                control,
                response,
            })),
            &state,
            queue_deadline,
            immediate,
        )?;
        Ok(NativePendingCommit {
            receiver,
            state,
            #[cfg(test)]
            submitted_at,
            queue_deadline,
            poll_cancellation,
            cancel_on_drop: true,
        })
    }

    fn accepting(&self) -> Result<bool, GroupCommitSubmitError> {
        self.gate
            .lock()
            .map(|gate| gate.accepting)
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn accepting_for_test(&self) -> Result<bool, GroupCommitSubmitError> {
        self.accepting()
    }

    #[cfg(test)]
    pub(crate) fn enqueue_for_test(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<
        Receiver<Result<ScheduledCommitCompletion, GroupCommitSubmitError>>,
        GroupCommitSubmitError,
    > {
        let submitted_at = Instant::now();
        let control = NativeCommitControl::new();
        let state = Arc::clone(&control.state);
        let (response, receiver) = mpsc::sync_channel(1);
        let batch = batch
            .into()
            .into_inner()
            .retain_scheduler_queue_memory()
            .map_err(GroupCommitSubmitError::runtime)?;
        admit_command(
            &self.gate,
            SchedulerCommand::Commit(Box::new(CommitRequest {
                batch,
                submitted_at,
                enqueued_at: submitted_at,
                control,
                response,
            })),
            &state,
            None,
            false,
        )?;
        Ok(receiver)
    }

    #[cfg(test)]
    pub(crate) fn block_cohort_collection_for_test(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, ()>, GroupCommitSubmitError> {
        self.cohort_collection_gate
            .write()
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn block_commit_execution_for_test(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, ()>, GroupCommitSubmitError> {
        self.commit_execution_gate
            .write()
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn block_completion_return_for_test(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, ()>, GroupCommitSubmitError> {
        self.completion_return_gate
            .write()
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn block_worker_for_test(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, Option<NativeDatabase>>, GroupCommitSubmitError>
    {
        self.database
            .read()
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }
}

/// Owner of one bounded native group-commit worker and database handle.
pub struct NativeCommitScheduler {
    client: NativeCommitClient,
    worker: Option<JoinHandle<()>>,
    active_expiry_metrics: Option<Arc<ActiveExpiryMetrics>>,
}

impl NativeCommitScheduler {
    /// Starts a named scheduler thread around one native database.
    ///
    /// # Errors
    ///
    /// Returns a typed runtime I/O failure if the worker thread cannot start.
    pub fn start(
        database: NativeDatabase,
        config: GroupCommitConfig,
    ) -> Result<Self, GroupCommitSubmitError> {
        Self::start_inner(database, config, None)
    }

    /// Starts one scheduler with system-clock active expiry.
    ///
    /// # Errors
    ///
    /// Returns a typed runtime I/O failure if the worker thread cannot start.
    pub fn start_with_active_expiry(
        database: NativeDatabase,
        config: GroupCommitConfig,
        active_expiry: ActiveExpiryConfig,
    ) -> Result<Self, GroupCommitSubmitError> {
        Self::start_with_active_expiry_clock(
            database,
            config,
            active_expiry,
            Arc::new(SystemSchedulerClock),
        )
    }

    /// Starts one scheduler with an injected absolute-microsecond clock.
    ///
    /// # Errors
    ///
    /// Returns a typed runtime I/O failure if the worker thread cannot start.
    pub fn start_with_active_expiry_clock(
        database: NativeDatabase,
        config: GroupCommitConfig,
        active_expiry: ActiveExpiryConfig,
        clock: Arc<dyn NativeSchedulerClock>,
    ) -> Result<Self, GroupCommitSubmitError> {
        let metrics = Arc::new(ActiveExpiryMetrics::default());
        let runtime = ActiveExpiryRuntime::new(active_expiry, clock, Arc::clone(&metrics));
        Self::start_inner(database, config, Some((runtime, metrics)))
    }

    #[cfg(test)]
    pub(crate) fn start_with_active_expiry_clock_at(
        database: NativeDatabase,
        config: GroupCommitConfig,
        active_expiry: ActiveExpiryConfig,
        clock: Arc<dyn NativeSchedulerClock>,
        interruption: CommitBoundary,
    ) -> Result<Self, GroupCommitSubmitError> {
        let metrics = Arc::new(ActiveExpiryMetrics::default());
        let mut runtime = ActiveExpiryRuntime::new(active_expiry, clock, Arc::clone(&metrics));
        runtime.interruption = Some(interruption);
        Self::start_inner(database, config, Some((runtime, metrics)))
    }

    fn start_inner(
        database: NativeDatabase,
        config: GroupCommitConfig,
        active_expiry: Option<(ActiveExpiryRuntime, Arc<ActiveExpiryMetrics>)>,
    ) -> Result<Self, GroupCommitSubmitError> {
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let gate = Arc::new(Mutex::new(SubmissionGate {
            sender,
            accepting: true,
            queue_capacity: config.queue_capacity,
            queued_request_slots: 0,
        }));
        let execution_cancellation = database
            .resource_governor()
            .map(NativeResourceGovernor::cancellation_token);
        let database = Arc::new(RwLock::new(Some(database)));
        #[cfg(test)]
        let test_gates = SchedulerTestGates {
            cohort_collection: Arc::new(RwLock::new(())),
            commit_execution: Arc::new(RwLock::new(())),
            completion_return: Arc::new(RwLock::new(())),
        };
        let client = NativeCommitClient {
            gate: Arc::clone(&gate),
            database: Arc::clone(&database),
            execution_cancellation: execution_cancellation.clone(),
            resource_admission_wait: config.execution_admission_wait,
            maximum_explicit_cohort_size: config.max_batch_size,
            #[cfg(test)]
            cohort_collection_gate: Arc::clone(&test_gates.cohort_collection),
            #[cfg(test)]
            commit_execution_gate: Arc::clone(&test_gates.commit_execution),
            #[cfg(test)]
            completion_return_gate: Arc::clone(&test_gates.completion_return),
        };
        let (active_expiry, active_expiry_metrics) = active_expiry
            .map_or((None, None), |(runtime, metrics)| {
                (Some(runtime), Some(metrics))
            });
        let worker = thread::Builder::new()
            .name("hyphae-commit-scheduler".to_owned())
            .spawn(move || {
                run_scheduler(
                    &receiver,
                    &database,
                    &gate,
                    config,
                    execution_cancellation.as_ref(),
                    active_expiry,
                    #[cfg(test)]
                    &test_gates,
                );
            })
            .map_err(|source| GroupCommitSubmitError::runtime(NativeRuntimeError::Io(source)))?;
        Ok(Self {
            client,
            worker: Some(worker),
            active_expiry_metrics,
        })
    }

    /// Returns the current lock-free active-expiry diagnostics.
    pub fn active_expiry_stats(&self) -> Option<ActiveExpiryStats> {
        self.active_expiry_metrics
            .as_ref()
            .map(|metrics| metrics.snapshot())
    }

    /// Returns the retained terminal active-expiry failure, when present.
    pub fn active_expiry_failure(&self) -> Option<ActiveExpiryFailure> {
        self.active_expiry_metrics
            .as_ref()
            .and_then(|metrics| metrics.terminal_failure.get().cloned())
    }

    /// Returns one cloneable producer bound to this scheduler.
    pub fn client(&self) -> NativeCommitClient {
        self.client.clone()
    }

    /// Prepares a detached batch through the scheduler's shared database view.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::begin_optimistic`].
    pub fn begin_optimistic(
        &self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeWriteBatch, GroupCommitSubmitError> {
        self.client
            .begin_optimistic(logical_time_micros, durability)
    }

    /// Prepares a detached delta batch through the scheduler's shared database
    /// view without materializing complete engine state.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::begin_optimistic_delta`].
    pub fn begin_optimistic_delta(
        &self,
        logical_time_micros: i64,
        durability: DurabilityClass,
    ) -> Result<NativeDeltaWriteBatch, GroupCommitSubmitError> {
        self.client
            .begin_optimistic_delta(logical_time_micros, durability)
    }

    /// Submits one batch through the owned worker.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::submit`].
    pub fn submit(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.client.submit(batch)
    }

    /// Enqueues one batch through the owned worker without blocking its caller.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::enqueue`].
    pub fn enqueue(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<NativePendingCommit, GroupCommitSubmitError> {
        self.client.enqueue(batch)
    }

    /// Atomically enqueues one explicit group-durability cohort.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::enqueue_cohort`].
    pub fn enqueue_cohort(
        &self,
        batches: Vec<NativeCommitBatch>,
    ) -> Result<Vec<NativePendingCommit>, GroupCommitSubmitError> {
        self.client.enqueue_cohort(batches)
    }

    /// Attempts immediate bounded admission through the owned worker.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::try_submit`].
    pub fn try_submit(
        &self,
        batch: impl Into<NativeCommitBatch>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.client.try_submit(batch)
    }

    /// Submits controlled queued work through the owned worker.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::submit_controlled`].
    pub fn submit_controlled(
        &self,
        batch: impl Into<NativeCommitBatch>,
        control: NativeCommitControl,
        queue_deadline: Option<Instant>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.client
            .submit_controlled(batch, control, queue_deadline)
    }

    /// Stops admission, drains commands preceding the marker, and joins the worker.
    ///
    /// # Errors
    ///
    /// Returns unavailable if scheduler synchronization is poisoned or the
    /// worker terminated abnormally, or the retained typed runtime failure
    /// when active expiry stopped the worker.
    pub fn shutdown(mut self) -> Result<(), GroupCommitSubmitError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), GroupCommitSubmitError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let sender = {
            let mut gate = self
                .client
                .gate
                .lock()
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
            if gate.accepting {
                gate.accepting = false;
                Some(gate.sender.clone())
            } else {
                None
            }
        };
        if let Some(sender) = sender {
            sender
                .send(SchedulerCommand::Shutdown)
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        }
        worker
            .join()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        match self.active_expiry_failure() {
            Some(failure) => Err(failure.submit_error()),
            None => Ok(()),
        }
    }
}

impl Drop for NativeCommitScheduler {
    fn drop(&mut self) {
        drop(self.stop_and_join());
    }
}

fn admit_command(
    gate: &Mutex<SubmissionGate>,
    mut command: SchedulerCommand,
    state: &AtomicU8,
    queue_deadline: Option<Instant>,
    immediate: bool,
) -> Result<(), GroupCommitSubmitError> {
    loop {
        check_queued_control(state, queue_deadline)?;
        let attempted_at = Instant::now();
        stamp_command_enqueued_at(&mut command, attempted_at);
        let admission = {
            let mut gate = gate
                .lock()
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
            if !gate.accepting {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            if gate.queued_request_slots >= gate.queue_capacity {
                drop(gate);
                if immediate {
                    return Err(GroupCommitSubmitError::Saturated);
                }
                thread::yield_now();
                continue;
            }
            match gate.sender.try_send(command) {
                Ok(()) => {
                    gate.queued_request_slots += 1;
                    return Ok(());
                }
                Err(error) => error,
            }
        };
        match admission {
            TrySendError::Disconnected(_) => {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            TrySendError::Full(returned) if immediate => {
                drop(returned);
                return Err(GroupCommitSubmitError::Saturated);
            }
            TrySendError::Full(returned) => {
                command = returned;
                thread::yield_now();
            }
        }
    }
}

fn admit_cohort_command(
    gate: &Mutex<SubmissionGate>,
    requests: Vec<CommitRequest>,
) -> Result<(), GroupCommitSubmitError> {
    let mut command = SchedulerCommand::Cohort(requests);
    let request_slots = command_request_slots(&command);
    loop {
        stamp_command_enqueued_at(&mut command, Instant::now());
        let admission = {
            let mut gate = gate
                .lock()
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
            if !gate.accepting {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            if gate
                .queued_request_slots
                .checked_add(request_slots)
                .is_none_or(|queued| queued > gate.queue_capacity)
            {
                drop(gate);
                thread::yield_now();
                continue;
            }
            match gate.sender.try_send(command) {
                Ok(()) => {
                    gate.queued_request_slots += request_slots;
                    return Ok(());
                }
                Err(error) => error,
            }
        };
        match admission {
            TrySendError::Disconnected(_) => {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            TrySendError::Full(returned) => {
                command = returned;
                thread::yield_now();
            }
        }
    }
}

fn command_request_slots(command: &SchedulerCommand) -> usize {
    match command {
        SchedulerCommand::Commit(_) => 1,
        SchedulerCommand::Cohort(requests) => requests.len(),
        SchedulerCommand::Shutdown => 0,
    }
}

fn release_dequeued_request_slots(
    gate: &Mutex<SubmissionGate>,
    command: &SchedulerCommand,
) -> bool {
    let Ok(mut gate) = gate.lock() else {
        return false;
    };
    let request_slots = command_request_slots(command);
    let Some(queued) = gate.queued_request_slots.checked_sub(request_slots) else {
        return false;
    };
    gate.queued_request_slots = queued;
    true
}

fn stamp_command_enqueued_at(command: &mut SchedulerCommand, enqueued_at: Instant) {
    match command {
        SchedulerCommand::Commit(request) => request.enqueued_at = enqueued_at,
        SchedulerCommand::Cohort(requests) => {
            for request in requests {
                request.enqueued_at = enqueued_at;
            }
        }
        SchedulerCommand::Shutdown => {}
    }
}

fn validate_explicit_cohort(
    batches: &[NativeWriteBatch],
    maximum_size: usize,
) -> Result<(), GroupCommitSubmitError> {
    if batches.is_empty() || batches.len() > maximum_size {
        return Err(GroupCommitSubmitError::runtime(
            NativeRuntimeError::InvalidGroupCommitBatchSize {
                requested: batches.len(),
            },
        ));
    }
    if batches
        .iter()
        .any(|batch| batch.durability != DurabilityClass::Group)
    {
        return Err(GroupCommitSubmitError::runtime(
            NativeRuntimeError::GroupCommitRequiresGroupDurability,
        ));
    }
    Ok(())
}

fn check_queued_control(
    state: &AtomicU8,
    queue_deadline: Option<Instant>,
) -> Result<(), GroupCommitSubmitError> {
    match state.load(Ordering::Acquire) {
        REQUEST_CANCELLED => return Err(GroupCommitSubmitError::Cancelled),
        REQUEST_EXECUTING | REQUEST_COMPLETED => return Err(GroupCommitSubmitError::Unavailable),
        _ => {}
    }
    if queue_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return match state.compare_exchange(
            REQUEST_QUEUED,
            REQUEST_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Err(GroupCommitSubmitError::DeadlineExceeded),
            Err(REQUEST_CANCELLED) => Err(GroupCommitSubmitError::Cancelled),
            Err(_) => Err(GroupCommitSubmitError::Unavailable),
        };
    }
    Ok(())
}

fn await_response(
    receiver: &Receiver<Result<ScheduledCommitCompletion, GroupCommitSubmitError>>,
    state: &AtomicU8,
    queue_deadline: Option<Instant>,
    poll_cancellation: bool,
) -> Result<ScheduledCommitCompletion, GroupCommitSubmitError> {
    if !poll_cancellation {
        return receiver
            .recv()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
    }
    loop {
        if state.load(Ordering::Acquire) == REQUEST_CANCELLED {
            return Err(GroupCommitSubmitError::Cancelled);
        }
        if queue_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            match state.compare_exchange(
                REQUEST_QUEUED,
                REQUEST_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Err(GroupCommitSubmitError::DeadlineExceeded),
                Err(REQUEST_CANCELLED) => return Err(GroupCommitSubmitError::Cancelled),
                Err(REQUEST_EXECUTING | REQUEST_COMPLETED) => {
                    return receiver
                        .recv()
                        .map_err(|_| GroupCommitSubmitError::Unavailable)?;
                }
                Err(_) => return Err(GroupCommitSubmitError::Unavailable),
            }
        }
        match receiver.recv_timeout(Duration::from_millis(1)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(GroupCommitSubmitError::Unavailable);
            }
        }
    }
}

fn run_scheduler(
    receiver: &Receiver<SchedulerCommand>,
    database: &RwLock<Option<NativeDatabase>>,
    gate: &Mutex<SubmissionGate>,
    config: GroupCommitConfig,
    execution_cancellation: Option<&GovernorCancellation>,
    mut active_expiry: Option<ActiveExpiryRuntime>,
    #[cfg(test)] test_gates: &SchedulerTestGates,
) {
    let mut shutdown = false;
    let mut pending = None;
    while !shutdown {
        if active_expiry
            .as_ref()
            .is_some_and(ActiveExpiryRuntime::should_force_sweep)
        {
            let Some(expiry) = active_expiry.as_mut() else {
                break;
            };
            if !execute_active_expiry(database, expiry) {
                break;
            }
            continue;
        }
        let first =
            match receive_scheduler_command(receiver, gate, &mut pending, active_expiry.as_ref()) {
                SchedulerWake::Command(SchedulerCommand::Commit(request)) => *request,
                SchedulerWake::Command(SchedulerCommand::Cohort(requests)) => {
                    if !execute_explicit_cohort(
                        database,
                        requests,
                        config,
                        execution_cancellation,
                        &mut active_expiry,
                        #[cfg(test)]
                        test_gates,
                    ) {
                        break;
                    }
                    continue;
                }
                SchedulerWake::Command(SchedulerCommand::Shutdown)
                | SchedulerWake::Disconnected => break,
                SchedulerWake::ExpiryDue => {
                    let Some(expiry) = active_expiry.as_mut() else {
                        break;
                    };
                    if !execute_active_expiry(database, expiry) {
                        break;
                    }
                    continue;
                }
            };
        #[cfg(test)]
        let Ok(cohort_collection_guard) = test_gates.cohort_collection.read() else {
            break;
        };
        #[cfg(test)]
        drop(cohort_collection_guard);
        let counts_after_due = expiry_is_due(active_expiry.as_ref());
        if first.batch.durability != DurabilityClass::Group {
            if !execute_single_request(
                database,
                first,
                config.execution_admission_wait,
                execution_cancellation,
                #[cfg(test)]
                &test_gates.commit_execution,
                #[cfg(test)]
                &test_gates.completion_return,
            ) {
                break;
            }
            if counts_after_due {
                record_expiry_foreground(&mut active_expiry, 1);
            }
            continue;
        }
        let requests = collect_group_requests(
            GroupCollectionContext {
                receiver,
                gate,
                active_expiry: active_expiry.as_ref(),
                config,
                counts_after_due,
            },
            first,
            &mut pending,
            &mut shutdown,
        );
        let request_count = requests.len();
        if !execute_group_requests(
            database,
            requests,
            config.execution_admission_wait,
            execution_cancellation,
            #[cfg(test)]
            &test_gates.commit_execution,
            #[cfg(test)]
            &test_gates.completion_return,
        ) {
            shutdown = true;
        } else if counts_after_due {
            record_expiry_foreground(&mut active_expiry, request_count);
        }
    }
    stop_accepting(gate);
    close_database(database);
}

fn expiry_is_due(active_expiry: Option<&ActiveExpiryRuntime>) -> bool {
    active_expiry.is_some_and(|expiry| expiry.wait_until_deadline().is_zero())
}

fn execute_explicit_cohort(
    database: &RwLock<Option<NativeDatabase>>,
    requests: Vec<CommitRequest>,
    config: GroupCommitConfig,
    execution_cancellation: Option<&GovernorCancellation>,
    active_expiry: &mut Option<ActiveExpiryRuntime>,
    #[cfg(test)] test_gates: &SchedulerTestGates,
) -> bool {
    #[cfg(test)]
    let Ok(cohort_collection_guard) = test_gates.cohort_collection.read() else {
        return false;
    };
    #[cfg(test)]
    drop(cohort_collection_guard);
    let request_count = requests.len();
    if explicit_cohort_exceeds_due_budget(active_expiry.as_ref(), request_count) {
        let Some(expiry) = active_expiry.as_mut() else {
            return false;
        };
        if !execute_active_expiry(database, expiry) {
            return false;
        }
    }
    let counts_after_due = active_expiry
        .as_ref()
        .is_some_and(|expiry| expiry.wait_until_deadline().is_zero());
    let completed = execute_group_requests(
        database,
        requests,
        config.execution_admission_wait,
        execution_cancellation,
        #[cfg(test)]
        &test_gates.commit_execution,
        #[cfg(test)]
        &test_gates.completion_return,
    );
    if completed && counts_after_due {
        record_expiry_foreground(active_expiry, request_count);
    }
    completed
}

fn explicit_cohort_exceeds_due_budget(
    active_expiry: Option<&ActiveExpiryRuntime>,
    request_count: usize,
) -> bool {
    active_expiry.is_some_and(|expiry| {
        expiry.wait_until_deadline().is_zero()
            && request_count
                > expiry
                    .config
                    .foreground_budget
                    .saturating_sub(expiry.foreground_after_due)
    })
}

#[derive(Clone, Copy)]
struct GroupCollectionContext<'a> {
    receiver: &'a Receiver<SchedulerCommand>,
    gate: &'a Mutex<SubmissionGate>,
    active_expiry: Option<&'a ActiveExpiryRuntime>,
    config: GroupCommitConfig,
    counts_after_due: bool,
}

fn collect_group_requests(
    context: GroupCollectionContext<'_>,
    first: CommitRequest,
    pending: &mut Option<SchedulerCommand>,
    shutdown: &mut bool,
) -> Vec<CommitRequest> {
    let GroupCollectionContext {
        receiver,
        gate,
        active_expiry,
        config,
        counts_after_due,
    } = context;
    let mut requests = vec![first];
    let deadline = Instant::now() + config.max_wait;
    let max_batch_size = active_expiry.map_or(config.max_batch_size, |expiry| {
        if counts_after_due {
            config.max_batch_size.min(
                expiry
                    .config
                    .foreground_budget
                    .saturating_sub(expiry.foreground_after_due)
                    .max(1),
            )
        } else {
            config.max_batch_size
        }
    });
    while requests.len() < max_batch_size {
        let mut remaining = deadline.saturating_duration_since(Instant::now());
        if !counts_after_due && let Some(expiry) = active_expiry {
            remaining = remaining.min(expiry.wait_until_deadline());
        }
        let next_command = receiver.recv_timeout(remaining);
        if let Ok(command) = &next_command
            && !release_dequeued_request_slots(gate, command)
        {
            *shutdown = true;
            break;
        }
        match next_command {
            Ok(SchedulerCommand::Commit(request))
                if request.batch.durability == DurabilityClass::Group =>
            {
                requests.push(*request);
            }
            Ok(command @ (SchedulerCommand::Commit(_) | SchedulerCommand::Cohort(_))) => {
                *pending = Some(command);
                break;
            }
            Ok(SchedulerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                *shutdown = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => break,
        }
    }
    requests
}

enum SchedulerWake {
    Command(SchedulerCommand),
    ExpiryDue,
    Disconnected,
}

fn receive_scheduler_command(
    receiver: &Receiver<SchedulerCommand>,
    gate: &Mutex<SubmissionGate>,
    pending: &mut Option<SchedulerCommand>,
    active_expiry: Option<&ActiveExpiryRuntime>,
) -> SchedulerWake {
    if let Some(command) = pending.take() {
        return SchedulerWake::Command(command);
    }
    let wake = match active_expiry {
        Some(expiry) => match receiver.recv_timeout(expiry.wait_until_deadline()) {
            Ok(command) => SchedulerWake::Command(command),
            Err(RecvTimeoutError::Timeout) => SchedulerWake::ExpiryDue,
            Err(RecvTimeoutError::Disconnected) => SchedulerWake::Disconnected,
        },
        None => receiver
            .recv()
            .map_or(SchedulerWake::Disconnected, SchedulerWake::Command),
    };
    if let SchedulerWake::Command(command) = &wake
        && !release_dequeued_request_slots(gate, command)
    {
        return SchedulerWake::Disconnected;
    }
    wake
}

fn record_expiry_foreground(active_expiry: &mut Option<ActiveExpiryRuntime>, requests: usize) {
    if let Some(expiry) = active_expiry {
        expiry.record_foreground(requests);
    }
}

fn execute_active_expiry(
    database: &RwLock<Option<NativeDatabase>>,
    active_expiry: &mut ActiveExpiryRuntime,
) -> bool {
    let started = Instant::now();
    let logical_time_micros = active_expiry.begin_sweep();
    let result = match database.write() {
        Ok(mut database) => match database.as_mut() {
            Some(database) => {
                #[cfg(not(test))]
                let sweep = database.expire_due_structures(
                    logical_time_micros,
                    active_expiry.config.max_keys,
                    active_expiry.config.durability,
                );
                #[cfg(test)]
                let sweep = database.expire_due_structures_at(
                    logical_time_micros,
                    active_expiry.config.max_keys,
                    active_expiry.config.durability,
                    active_expiry.interruption,
                );
                sweep.map_err(|source| ActiveExpiryFailure::Runtime {
                    source: Arc::new(source),
                })
            }
            None => Err(ActiveExpiryFailure::DatabaseUnavailable),
        },
        Err(_) => Err(ActiveExpiryFailure::DatabaseUnavailable),
    };
    let success = match result {
        Ok(receipt) => {
            atomic_saturating_add(
                &active_expiry.metrics.expired_keys,
                u64::try_from(receipt.expired_keys).unwrap_or(u64::MAX),
            );
            if receipt.commit.is_some() {
                atomic_saturating_add(&active_expiry.metrics.committed_sweeps, 1);
            } else {
                atomic_saturating_add(&active_expiry.metrics.empty_sweeps, 1);
            }
            true
        }
        Err(failure) => {
            let _ = active_expiry.metrics.terminal_failure.set(failure);
            atomic_saturating_add(&active_expiry.metrics.failures, 1);
            false
        }
    };
    active_expiry.finish_sweep(started);
    success
}

fn atomic_saturating_add(counter: &AtomicU64, value: u64) {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.saturating_add(value);
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn execute_single_request(
    database: &RwLock<Option<NativeDatabase>>,
    request: CommitRequest,
    execution_admission_wait: Duration,
    execution_cancellation: Option<&GovernorCancellation>,
    #[cfg(test)] commit_execution_gate: &RwLock<()>,
    #[cfg(test)] completion_return_gate: &RwLock<()>,
) -> bool {
    let execution_started = Instant::now();
    let CommitRequest {
        batch,
        submitted_at,
        enqueued_at,
        control,
        response,
    } = request;
    if !control.claim_execution() {
        deliver(&response, Err(GroupCommitSubmitError::Cancelled));
        return true;
    }
    let execution_permit = {
        let Ok(database) = database.read() else {
            control.complete();
            deliver(&response, Err(GroupCommitSubmitError::Unavailable));
            return false;
        };
        let Some(database) = database.as_ref() else {
            control.complete();
            deliver(&response, Err(GroupCommitSubmitError::Unavailable));
            return false;
        };
        database.admit_scheduled_commit_execution(execution_admission_wait, execution_cancellation)
    };
    let execution_permit = match execution_permit {
        Ok(permit) => permit,
        Err(source) => {
            control.complete();
            deliver(&response, Err(GroupCommitSubmitError::runtime(source)));
            return true;
        }
    };
    #[cfg(test)]
    let Ok(commit_execution_guard) = commit_execution_gate.read() else {
        drop(execution_permit);
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    #[cfg(test)]
    drop(commit_execution_guard);
    let Ok(mut database_guard) = database.write() else {
        drop(execution_permit);
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    let Some(database) = database_guard.as_mut() else {
        drop(database_guard);
        drop(execution_permit);
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    let (result, request_local) = match database.commit_optimistic_scheduled(batch) {
        Ok(report) => (Ok(report), true),
        Err(source) => {
            let request_local = scheduler_request_local(&source);
            (Err(GroupCommitSubmitError::runtime(source)), request_local)
        }
    };
    drop(database_guard);
    drop(execution_permit);
    let result = result.map(|report| ScheduledCommitCompletion {
        receipt: ScheduledCommitReceipt {
            commit: report.commit,
            admission_wait: enqueued_at.saturating_duration_since(submitted_at),
            queue_wait: execution_started.saturating_duration_since(enqueued_at),
            cohort_execution: report.commit.execution_time,
            page_synchronization: report.commit.page_synchronization_time,
            wal_synchronization: report.commit.wal_synchronization_time,
            end_to_end: submitted_at.elapsed(),
        },
        page_synchronizations: report.page_synchronizations,
        wal_synchronizations: report.wal_synchronizations,
    });
    control.complete();
    deliver(&response, result);
    #[cfg(test)]
    let Ok(completion_return_guard) = completion_return_gate.read() else {
        return false;
    };
    #[cfg(test)]
    drop(completion_return_guard);
    request_local
}

fn execute_group_requests(
    database: &RwLock<Option<NativeDatabase>>,
    requests: Vec<CommitRequest>,
    execution_admission_wait: Duration,
    execution_cancellation: Option<&GovernorCancellation>,
    #[cfg(test)] commit_execution_gate: &RwLock<()>,
    #[cfg(test)] completion_return_gate: &RwLock<()>,
) -> bool {
    let execution_started = Instant::now();
    let mut batches = Vec::with_capacity(requests.len());
    let mut waiters = Vec::with_capacity(requests.len());
    for request in requests {
        if request.control.claim_execution() {
            batches.push(request.batch);
            waiters.push((
                request.submitted_at,
                request.enqueued_at,
                request.control,
                request.response,
            ));
        } else {
            deliver(&request.response, Err(GroupCommitSubmitError::Cancelled));
        }
    }
    if batches.is_empty() {
        return true;
    }
    let execution_permit = {
        let Ok(database) = database.read() else {
            deliver_all_unavailable(waiters);
            return false;
        };
        let Some(database) = database.as_ref() else {
            deliver_all_unavailable(waiters);
            return false;
        };
        database.admit_scheduled_commit_execution(execution_admission_wait, execution_cancellation)
    };
    let execution_permit = match execution_permit {
        Ok(permit) => permit,
        Err(source) => {
            let failure = GroupCommitSubmitError::runtime(source);
            for (_submitted_at, _enqueued_at, control, response) in waiters {
                control.complete();
                deliver(&response, Err(failure.clone()));
            }
            return true;
        }
    };
    #[cfg(test)]
    let Ok(commit_execution_guard) = commit_execution_gate.read() else {
        drop(execution_permit);
        deliver_all_unavailable(waiters);
        return false;
    };
    #[cfg(test)]
    drop(commit_execution_guard);
    let Ok(mut database_guard) = database.write() else {
        drop(execution_permit);
        deliver_all_unavailable(waiters);
        return false;
    };
    let Some(database) = database_guard.as_mut() else {
        drop(database_guard);
        drop(execution_permit);
        deliver_all_unavailable(waiters);
        return false;
    };
    let report = database.commit_group(batches);
    drop(database_guard);
    drop(execution_permit);
    let completed = deliver_group_report(report, waiters, execution_started);
    #[cfg(test)]
    let Ok(completion_return_guard) = completion_return_gate.read() else {
        return false;
    };
    #[cfg(test)]
    drop(completion_return_guard);
    completed
}

fn deliver_group_report(
    report: Result<GroupCommitReport, NativeRuntimeError>,
    waiters: Vec<CommitWaiter>,
    execution_started: Instant,
) -> bool {
    match report {
        Ok(report) if report.outcomes.len() == waiters.len() => {
            for ((submitted_at, enqueued_at, control, response), outcome) in
                waiters.into_iter().zip(report.outcomes)
            {
                let result = match outcome {
                    GroupCommitOutcome::Committed(commit) => Ok(ScheduledCommitCompletion {
                        receipt: ScheduledCommitReceipt {
                            commit,
                            admission_wait: enqueued_at.saturating_duration_since(submitted_at),
                            queue_wait: execution_started.saturating_duration_since(enqueued_at),
                            cohort_execution: report.execution_time,
                            page_synchronization: report.page_synchronization_time,
                            wal_synchronization: report.wal_synchronization_time,
                            end_to_end: submitted_at.elapsed(),
                        },
                        page_synchronizations: report.page_synchronizations,
                        wal_synchronizations: report.wal_synchronizations,
                    }),
                    GroupCommitOutcome::Rejected(source) => {
                        Err(GroupCommitSubmitError::runtime(source))
                    }
                };
                control.complete();
                deliver(&response, result);
            }
            true
        }
        Ok(_) => {
            deliver_all_unavailable(waiters);
            false
        }
        Err(source) => {
            let failure = GroupCommitSubmitError::runtime(source);
            for (_submitted_at, _enqueued_at, control, response) in waiters {
                control.complete();
                deliver(&response, Err(failure.clone()));
            }
            false
        }
    }
}

fn scheduler_request_local(source: &NativeRuntimeError) -> bool {
    matches!(
        source,
        NativeRuntimeError::Ann(_)
            | NativeRuntimeError::WalSemantic(_)
            | NativeRuntimeError::WriteConflict(_)
            | NativeRuntimeError::Catalog(_)
            | NativeRuntimeError::Model(_)
            | NativeRuntimeError::UniqueSecondaryIndexViolation
            | NativeRuntimeError::UnknownSecondaryIndex { .. }
            | NativeRuntimeError::UnknownRelation { .. }
            | NativeRuntimeError::UnknownVectorIndex { .. }
            | NativeRuntimeError::InvalidPreparedMutation
            | NativeRuntimeError::StructureValueNotInteger
            | NativeRuntimeError::StructureIntegerOverflow
            | NativeRuntimeError::StructureKindMismatch
            | NativeRuntimeError::UnknownStructureHash
            | NativeRuntimeError::UnknownStructureSet
            | NativeRuntimeError::UnknownStructureList
            | NativeRuntimeError::UnknownStructureSortedSet
            | NativeRuntimeError::StructureScoreNotCanonical
            | NativeRuntimeError::StructureListIndexExhausted
            | NativeRuntimeError::StructureKeyExists
            | NativeRuntimeError::LegacyStructureFamilyUnsupported
            | NativeRuntimeError::StructureIdentityTooLarge
            | NativeRuntimeError::SearchIdentityTooLarge
            | NativeRuntimeError::SnapshotBelowRetentionFloor { .. }
            | NativeRuntimeError::GroupCommitRequiresGroupDurability
    )
}

fn deliver_all_unavailable(waiters: Vec<CommitWaiter>) {
    for (_submitted_at, _enqueued_at, control, response) in waiters {
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
    }
}

fn deliver(
    response: &SyncSender<Result<ScheduledCommitCompletion, GroupCommitSubmitError>>,
    result: Result<ScheduledCommitCompletion, GroupCommitSubmitError>,
) {
    // A caller may abandon its wait after the transaction already has a durable
    // outcome; that cannot roll the database decision back.
    drop(response.send(result));
}

fn stop_accepting(gate: &Mutex<SubmissionGate>) {
    if let Ok(mut gate) = gate.lock() {
        gate.accepting = false;
    }
}

fn close_database(database: &RwLock<Option<NativeDatabase>>) {
    if let Ok(mut database) = database.write() {
        drop(database.take());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveExpiryConfig, ActiveExpiryConfigError, CommitCancellationOutcome, GroupCommitConfig,
        GroupCommitConfigError, GroupCommitExecutionAdmissionWaitError, GroupCommitSubmitError,
        MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT, MAX_GROUP_COMMIT_QUEUE_CAPACITY,
        MAX_GROUP_COMMIT_WAIT, NativeCommitControl, REQUEST_QUEUED, SchedulerCommand,
        SubmissionGate, admit_command,
    };
    use crate::MAX_GROUP_COMMIT_BATCH_SIZE;
    use hyphae_native_types::DurabilityClass;
    use std::{
        sync::{Mutex, atomic::AtomicU8, mpsc},
        time::Duration,
    };

    #[test]
    fn scheduler_bounds_fail_closed() {
        assert_eq!(
            GroupCommitConfig::new(0, Duration::ZERO, 1),
            Err(GroupCommitConfigError::BatchSize { requested: 0 })
        );
        assert_eq!(
            GroupCommitConfig::new(
                MAX_GROUP_COMMIT_BATCH_SIZE + 1,
                Duration::ZERO,
                MAX_GROUP_COMMIT_BATCH_SIZE + 1,
            ),
            Err(GroupCommitConfigError::BatchSize {
                requested: MAX_GROUP_COMMIT_BATCH_SIZE + 1,
            })
        );
        assert_eq!(
            GroupCommitConfig::new(1, MAX_GROUP_COMMIT_WAIT + Duration::from_nanos(1), 1),
            Err(GroupCommitConfigError::Wait {
                requested: MAX_GROUP_COMMIT_WAIT + Duration::from_nanos(1),
            })
        );
        assert_eq!(
            GroupCommitConfig::new(8, Duration::ZERO, 7),
            Err(GroupCommitConfigError::QueueCapacity {
                requested: 7,
                minimum: 8,
            })
        );
        assert_eq!(
            GroupCommitConfig::new(1, Duration::ZERO, MAX_GROUP_COMMIT_QUEUE_CAPACITY + 1,),
            Err(GroupCommitConfigError::QueueCapacity {
                requested: MAX_GROUP_COMMIT_QUEUE_CAPACITY + 1,
                minimum: 1,
            })
        );
        assert_eq!(
            GroupCommitConfig::new(8, Duration::from_micros(100), 16)
                .map(GroupCommitConfig::max_batch_size),
            Ok(8)
        );
        assert!(matches!(
            GroupCommitConfig::new(1, Duration::ZERO, 1),
            Ok(config)
                if config.with_execution_admission_wait(
                    MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT + Duration::from_nanos(1),
                ) == Err(GroupCommitExecutionAdmissionWaitError {
                    requested: MAX_GROUP_COMMIT_EXECUTION_ADMISSION_WAIT
                        + Duration::from_nanos(1),
                })
        ));
        assert!(matches!(
            GroupCommitConfig::new(1, Duration::ZERO, 1),
            Ok(config)
                if config
                    .with_execution_admission_wait(Duration::from_secs(60))
                    .map(GroupCommitConfig::queue_capacity)
                    == Ok(1)
        ));
    }

    #[test]
    fn cancellation_state_has_one_exact_execution_boundary() {
        let control = NativeCommitControl::new();
        let cancellation = control.cancellation();
        assert!(control.claim_execution());
        assert_eq!(cancellation.cancel(), CommitCancellationOutcome::TooLate);
        control.complete();
        assert_eq!(cancellation.cancel(), CommitCancellationOutcome::Completed);

        let queued = NativeCommitControl::new();
        let cancellation = queued.cancellation();
        assert_eq!(cancellation.cancel(), CommitCancellationOutcome::Cancelled);
        assert!(!queued.claim_execution());
        assert_eq!(cancellation.cancel(), CommitCancellationOutcome::Cancelled);
    }

    #[test]
    fn immediate_admission_reports_saturation_without_holding_the_gate() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        assert!(sender.send(SchedulerCommand::Shutdown).is_ok());
        let gate = Mutex::new(SubmissionGate {
            sender,
            accepting: true,
            queue_capacity: 1,
            queued_request_slots: 0,
        });
        let state = AtomicU8::new(REQUEST_QUEUED);

        assert!(matches!(
            admit_command(&gate, SchedulerCommand::Shutdown, &state, None, true),
            Err(GroupCommitSubmitError::Saturated)
        ));
        assert!(gate.try_lock().is_ok());
    }

    #[test]
    fn active_expiry_bounds_fail_closed() {
        assert!(matches!(
            ActiveExpiryConfig::new(Duration::from_micros(99), 1, DurabilityClass::Memory, 1),
            Err(ActiveExpiryConfigError::Interval { .. })
        ));
        assert_eq!(
            ActiveExpiryConfig::new(Duration::from_micros(100), 0, DurabilityClass::Memory, 1),
            Err(ActiveExpiryConfigError::BatchSize { requested: 0 })
        );
        assert_eq!(
            ActiveExpiryConfig::new(Duration::from_micros(100), 1, DurabilityClass::Group, 1),
            Err(ActiveExpiryConfigError::Durability {
                requested: DurabilityClass::Group
            })
        );
        assert_eq!(
            ActiveExpiryConfig::new(Duration::from_micros(100), 1, DurabilityClass::Memory, 0),
            Err(ActiveExpiryConfigError::ForegroundBudget { requested: 0 })
        );
    }
}
