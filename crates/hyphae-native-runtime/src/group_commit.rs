// SPDX-License-Identifier: Apache-2.0

//! Bounded multi-producer scheduler for native group durability.

use std::{
    ops::Deref,
    sync::{
        Arc, Mutex, RwLock,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
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
    /// Native admission or persistence rejected the submitted transaction.
    #[error("native group commit request failed: {source}")]
    Runtime {
        /// Shared typed runtime failure.
        #[source]
        source: Arc<NativeRuntimeError>,
    },
}

/// Per-request timing and durability receipt produced by the scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledCommitReceipt {
    /// Independent native transaction receipt.
    pub commit: CommitReceipt,
    /// Time between queue admission and cohort execution.
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
            Self::Unavailable => None,
            Self::Runtime { source } => Some(source),
        }
    }
}

struct CommitRequest {
    batch: NativeWriteBatch,
    submitted_at: Instant,
    response: SyncSender<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>,
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
        let submitted_at = Instant::now();
        let (response, receiver) = mpsc::sync_channel(1);
        {
            let gate = self
                .gate
                .lock()
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
            if !gate.accepting {
                return Err(GroupCommitSubmitError::Unavailable);
            }
            gate.sender
                .send(SchedulerCommand::Commit(Box::new(CommitRequest {
                    batch,
                    submitted_at,
                    response,
                })))
                .map_err(|_| GroupCommitSubmitError::Unavailable)?;
        }
        let mut result = receiver
            .recv()
            .map_err(|_| GroupCommitSubmitError::Unavailable)?;
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
        response,
    } = request;
    let Ok(mut database) = database.write() else {
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    let Some(database) = database.as_mut() else {
        deliver(&response, Err(GroupCommitSubmitError::Unavailable));
        return false;
    };
    match database.commit_optimistic_scheduled(batch) {
        Ok(report) => {
            deliver(
                &response,
                Ok(ScheduledCommitReceipt {
                    commit: report.commit,
                    queue_wait: execution_started.saturating_duration_since(submitted_at),
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
    let (batches, waiters): (Vec<_>, Vec<_>) = requests
        .into_iter()
        .map(|request| (request.batch, (request.submitted_at, request.response)))
        .unzip();
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
            for ((submitted_at, response), outcome) in waiters.into_iter().zip(report.outcomes) {
                let result = match outcome {
                    GroupCommitOutcome::Committed(commit) => Ok(ScheduledCommitReceipt {
                        commit,
                        queue_wait: execution_started.saturating_duration_since(submitted_at),
                        cohort_execution: report.execution_time,
                        page_synchronization: report.page_synchronization_time,
                        wal_synchronization: report.wal_synchronization_time,
                        end_to_end: Duration::ZERO,
                    }),
                    GroupCommitOutcome::Rejected(source) => {
                        Err(GroupCommitSubmitError::runtime(source))
                    }
                };
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
            for (_submitted_at, response) in waiters {
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

fn deliver_all_unavailable(
    waiters: Vec<(
        Instant,
        SyncSender<Result<ScheduledCommitReceipt, GroupCommitSubmitError>>,
    )>,
) {
    for (_submitted_at, response) in waiters {
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
        GroupCommitConfig, GroupCommitConfigError, MAX_GROUP_COMMIT_QUEUE_CAPACITY,
        MAX_GROUP_COMMIT_WAIT,
    };
    use crate::MAX_GROUP_COMMIT_BATCH_SIZE;
    use std::time::Duration;

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
}
