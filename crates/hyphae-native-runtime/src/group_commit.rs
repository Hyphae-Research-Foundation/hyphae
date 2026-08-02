// SPDX-License-Identifier: Apache-2.0

//! Bounded multi-producer scheduler for native group durability.

use std::{
    ops::Deref,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use hyphae_native_types::DurabilityClass;
use thiserror::Error;

use crate::{
    CommitReceipt, GroupCommitOutcome, MAX_GROUP_COMMIT_BATCH_SIZE, NativeDatabase,
    NativeRuntimeError, NativeWriteBatch,
};

const DEFAULT_GROUP_COMMIT_BATCH_SIZE: usize = 32;
const DEFAULT_GROUP_COMMIT_WAIT: Duration = Duration::from_micros(200);
const DEFAULT_GROUP_COMMIT_QUEUE_CAPACITY: usize = 1_024;
const REQUEST_QUEUED: u8 = 0;
const REQUEST_EXECUTING: u8 = 1;
const REQUEST_CANCELLED: u8 = 2;
const REQUEST_COMPLETED: u8 = 3;

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
        let worker = thread::Builder::new()
            .name("hyphae-group-commit".to_owned())
            .spawn(move || run_scheduler(&receiver, &database, &gate, config))
            .map_err(|source| GroupCommitSubmitError::runtime(NativeRuntimeError::Io(source)))?;
        Ok(Self {
            client,
            worker: Some(worker),
        })
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
    /// worker terminated abnormally.
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
            .map_err(|_| GroupCommitSubmitError::Unavailable)
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
) {
    let mut shutdown = false;
    let mut pending = None;
    while !shutdown {
        let first = match pending.take().map_or_else(|| receiver.recv(), Ok) {
            Ok(SchedulerCommand::Commit(request)) => *request,
            Ok(SchedulerCommand::Shutdown) | Err(_) => break,
        };
        if first.batch.durability != DurabilityClass::Group {
            if !execute_single_request(database, first) {
                break;
            }
            continue;
        }
        let mut requests = vec![first];
        let deadline = Instant::now() + config.max_wait;
        while requests.len() < config.max_batch_size {
            let remaining = deadline.saturating_duration_since(Instant::now());
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
        if !execute_group_requests(database, requests) {
            shutdown = true;
        }
    }
    stop_accepting(gate);
    close_database(database);
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
        ActiveExpiryConfig, ActiveExpiryConfigError, CommitCancellationOutcome,
        GroupCommitConfig, GroupCommitConfigError, GroupCommitSubmitError,
        MAX_GROUP_COMMIT_QUEUE_CAPACITY, MAX_GROUP_COMMIT_WAIT, NativeCommitControl,
        REQUEST_QUEUED, SchedulerCommand, SubmissionGate, admit_command,
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
            ActiveExpiryConfig::new(
                Duration::from_micros(99),
                1,
                DurabilityClass::Memory,
                1
            ),
            Err(ActiveExpiryConfigError::Interval { .. })
        ));
        assert_eq!(
            ActiveExpiryConfig::new(
                Duration::from_micros(100),
                0,
                DurabilityClass::Memory,
                1
            ),
            Err(ActiveExpiryConfigError::BatchSize { requested: 0 })
        );
        assert_eq!(
            ActiveExpiryConfig::new(
                Duration::from_micros(100),
                1,
                DurabilityClass::Group,
                1
            ),
            Err(ActiveExpiryConfigError::Durability {
                requested: DurabilityClass::Group
            })
        );
        assert_eq!(
            ActiveExpiryConfig::new(
                Duration::from_micros(100),
                1,
                DurabilityClass::Memory,
                0
            ),
            Err(ActiveExpiryConfigError::ForegroundBudget { requested: 0 })
        );
    }
}
