// SPDX-License-Identifier: Apache-2.0

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

use crate::{
    CommitReceipt, GroupCommitOutcome, MAX_EXPIRY_SWEEP_KEYS, MAX_GROUP_COMMIT_BATCH_SIZE,
    NativeDatabase, NativeRuntimeError, NativeWriteBatch,
};

const DEFAULT_GROUP_COMMIT_BATCH_SIZE: usize = 32;
const DEFAULT_GROUP_COMMIT_WAIT: Duration = Duration::from_micros(200);
const DEFAULT_GROUP_COMMIT_QUEUE_CAPACITY: usize = 1_024;
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

/// Validated bounds for one native group-commit scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupCommitConfig {
    max_batch_size: usize,
    max_wait: Duration,
    queue_capacity: usize,
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

    /// Returns the bounded multi-producer queue capacity.
    pub const fn queue_capacity(self) -> usize {
        self.queue_capacity
    }
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            max_batch_size: DEFAULT_GROUP_COMMIT_BATCH_SIZE,
            max_wait: DEFAULT_GROUP_COMMIT_WAIT,
            queue_capacity: DEFAULT_GROUP_COMMIT_QUEUE_CAPACITY,
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

type CommitResponse = SyncSender<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>;
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
    Shutdown,
}

struct SubmissionGate {
    sender: SyncSender<SchedulerCommand>,
    accepting: bool,
}

/// Cloneable producer for one native group-commit scheduler.
#[derive(Clone)]
pub struct NativeCommitClient {
    gate: Arc<Mutex<SubmissionGate>>,
    database: Arc<RwLock<Option<NativeDatabase>>>,
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
            .begin_optimistic(logical_time_micros, durability)
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
        batch: NativeWriteBatch,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.submit_inner(batch, NativeCommitControl::new(), None, false, false)
    }

    /// Attempts immediate bounded admission and waits for a definite outcome.
    ///
    /// # Errors
    ///
    /// Returns [`GroupCommitSubmitError::Saturated`] without mutation when the
    /// bounded queue has no slot, or the same errors as [`Self::submit`].
    pub fn try_submit(
        &self,
        batch: NativeWriteBatch,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.submit_inner(batch, NativeCommitControl::new(), None, true, false)
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
        batch: NativeWriteBatch,
        control: NativeCommitControl,
        queue_deadline: Option<Instant>,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.submit_inner(batch, control, queue_deadline, false, true)
    }

    fn submit_inner(
        &self,
        batch: NativeWriteBatch,
        control: NativeCommitControl,
        queue_deadline: Option<Instant>,
        immediate: bool,
        poll_cancellation: bool,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        let submitted_at = Instant::now();
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
        let mut result = await_response(&receiver, &state, queue_deadline, poll_cancellation);
        if let Ok(receipt) = &mut result {
            receipt.end_to_end = submitted_at.elapsed();
        }
        result
    }

    fn accepting(&self) -> Result<bool, GroupCommitSubmitError> {
        self.gate
            .lock()
            .map(|gate| gate.accepting)
            .map_err(|_| GroupCommitSubmitError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn enqueue_for_test(
        &self,
        batch: NativeWriteBatch,
    ) -> Result<
        Receiver<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>,
        GroupCommitSubmitError,
    > {
        let submitted_at = Instant::now();
        let control = NativeCommitControl::new();
        let state = Arc::clone(&control.state);
        let (response, receiver) = mpsc::sync_channel(1);
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

    fn start_inner(
        database: NativeDatabase,
        config: GroupCommitConfig,
        active_expiry: Option<(ActiveExpiryRuntime, Arc<ActiveExpiryMetrics>)>,
    ) -> Result<Self, GroupCommitSubmitError> {
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let gate = Arc::new(Mutex::new(SubmissionGate {
            sender,
            accepting: true,
        }));
        let database = Arc::new(RwLock::new(Some(database)));
        let client = NativeCommitClient {
            gate: Arc::clone(&gate),
            database: Arc::clone(&database),
        };
        let (active_expiry, active_expiry_metrics) = active_expiry
            .map_or((None, None), |(runtime, metrics)| {
                (Some(runtime), Some(metrics))
            });
        let worker = thread::Builder::new()
            .name("hyphae-commit-scheduler".to_owned())
            .spawn(move || run_scheduler(&receiver, &database, &gate, config, active_expiry))
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

    /// Submits one batch through the owned worker.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::submit`].
    pub fn submit(
        &self,
        batch: NativeWriteBatch,
    ) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
        self.client.submit(batch)
    }

    /// Attempts immediate bounded admission through the owned worker.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NativeCommitClient::try_submit`].
    pub fn try_submit(
        &self,
        batch: NativeWriteBatch,
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
        batch: NativeWriteBatch,
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
        if let SchedulerCommand::Commit(request) = &mut command {
            request.enqueued_at = attempted_at;
        }
        let admission = {
            let gate = gate
                .lock()
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
            if !gate.accepting {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            gate.sender.try_send(command)
        };
        match admission {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            Err(TrySendError::Full(returned)) if immediate => {
                drop(returned);
                return Err(GroupCommitSubmitError::Saturated);
            }
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::yield_now();
            }
        }
    }
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
    receiver: &Receiver<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>,
    state: &AtomicU8,
    queue_deadline: Option<Instant>,
    poll_cancellation: bool,
) -> Result<ScheduledCommitReceipt, GroupCommitSubmitError> {
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
    mut active_expiry: Option<ActiveExpiryRuntime>,
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
            match receive_scheduler_command(receiver, &mut pending, active_expiry.as_ref()) {
                SchedulerWake::Command(SchedulerCommand::Commit(request)) => *request,
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
        let counts_after_due = active_expiry
            .as_ref()
            .is_some_and(|expiry| expiry.wait_until_deadline().is_zero());
        if first.batch.durability != DurabilityClass::Group {
            if !execute_single_request(database, first) {
                break;
            }
            if counts_after_due {
                record_expiry_foreground(&mut active_expiry, 1);
            }
            continue;
        }
        let mut requests = vec![first];
        let deadline = Instant::now() + config.max_wait;
        let max_batch_size = active_expiry
            .as_ref()
            .map_or(config.max_batch_size, |expiry| {
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
            if !counts_after_due && let Some(expiry) = active_expiry.as_ref() {
                remaining = remaining.min(expiry.wait_until_deadline());
            }
            match receiver.recv_timeout(remaining) {
                Ok(SchedulerCommand::Commit(request))
                    if request.batch.durability == DurabilityClass::Group =>
                {
                    requests.push(*request);
                }
                Ok(command @ SchedulerCommand::Commit(_)) => {
                    pending = Some(command);
                    break;
                }
                Ok(SchedulerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    shutdown = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        let request_count = requests.len();
        if !execute_group_requests(database, requests) {
            shutdown = true;
        } else if counts_after_due {
            record_expiry_foreground(&mut active_expiry, request_count);
        }
    }
    stop_accepting(gate);
    close_database(database);
}

enum SchedulerWake {
    Command(SchedulerCommand),
    ExpiryDue,
    Disconnected,
}

fn receive_scheduler_command(
    receiver: &Receiver<SchedulerCommand>,
    pending: &mut Option<SchedulerCommand>,
    active_expiry: Option<&ActiveExpiryRuntime>,
) -> SchedulerWake {
    if let Some(command) = pending.take() {
        return SchedulerWake::Command(command);
    }
    match active_expiry {
        Some(expiry) => match receiver.recv_timeout(expiry.wait_until_deadline()) {
            Ok(command) => SchedulerWake::Command(command),
            Err(RecvTimeoutError::Timeout) => SchedulerWake::ExpiryDue,
            Err(RecvTimeoutError::Disconnected) => SchedulerWake::Disconnected,
        },
        None => receiver
            .recv()
            .map_or(SchedulerWake::Disconnected, SchedulerWake::Command),
    }
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
            Some(database) => database
                .expire_due_structures(
                    logical_time_micros,
                    active_expiry.config.max_keys,
                    active_expiry.config.durability,
                )
                .map_err(|source| ActiveExpiryFailure::Runtime {
                    source: Arc::new(source),
                }),
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
            atomic_saturating_add(&active_expiry.metrics.failures, 1);
            let _ = active_expiry.metrics.terminal_failure.set(failure);
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
    let Ok(mut database) = database.write() else {
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    let Some(database) = database.as_mut() else {
        control.complete();
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    match database.commit_optimistic_scheduled(batch) {
        Ok(report) => {
            control.complete();
            deliver(
                &response,
                Ok(ScheduledCommitReceipt {
                    commit: report.commit,
                    admission_wait: enqueued_at.saturating_duration_since(submitted_at),
                    queue_wait: execution_started.saturating_duration_since(enqueued_at),
                    cohort_execution: report.execution_time,
                    page_synchronization: report.page_synchronization_time,
                    wal_synchronization: report.wal_synchronization_time,
                    end_to_end: Duration::ZERO,
                }),
            );
            true
        }
        Err(source) => {
            let request_local = scheduler_request_local(&source);
            control.complete();
            deliver(&response, Err(GroupCommitSubmitError::runtime(source)));
            request_local
        }
    }
}

fn execute_group_requests(
    database: &RwLock<Option<NativeDatabase>>,
    requests: Vec<CommitRequest>,
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
    let Ok(mut database) = database.write() else {
        deliver_all_unavailable(waiters);
        return false;
    };
    let Some(database) = database.as_mut() else {
        deliver_all_unavailable(waiters);
        return false;
    };
    let report = database.commit_group(batches);
    match report {
        Ok(report) if report.outcomes.len() == waiters.len() => {
            for ((submitted_at, enqueued_at, control, response), outcome) in
                waiters.into_iter().zip(report.outcomes)
            {
                let result = match outcome {
                    GroupCommitOutcome::Committed(commit) => Ok(ScheduledCommitReceipt {
                        commit,
                        admission_wait: enqueued_at.saturating_duration_since(submitted_at),
                        queue_wait: execution_started.saturating_duration_since(enqueued_at),
                        cohort_execution: report.execution_time,
                        page_synchronization: report.page_synchronization_time,
                        wal_synchronization: report.wal_synchronization_time,
                        end_to_end: Duration::ZERO,
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
    response: &SyncSender<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>,
    result: Result<ScheduledCommitReceipt, GroupCommitSubmitError>,
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
        GroupCommitConfigError, GroupCommitSubmitError, MAX_GROUP_COMMIT_QUEUE_CAPACITY,
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
