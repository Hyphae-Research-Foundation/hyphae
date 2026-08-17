// SPDX-License-Identifier: Apache-2.0

//! Bounded, process-local telemetry for the embedded native product.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{CatalogVersion, ProductErrorCategory};

/// Version of the fixed product metric registry.
pub const TELEMETRY_REGISTRY_VERSION: u16 = 1;
/// Maximum lifecycle events retained by one registry.
pub const MAX_TELEMETRY_EVENTS: usize = 1_024;
/// Fixed upper bounds, in microseconds, for every timing histogram.
pub const TELEMETRY_HISTOGRAM_BOUNDS_MICROS: [u64; 10] = [
    10, 50, 100, 500, 1_000, 5_000, 10_000, 50_000, 250_000, 1_000_000,
];

static PROCESS_START_IDENTITY: OnceLock<u128> = OnceLock::new();
static NEXT_SESSION_IDENTITY: AtomicU64 = AtomicU64::new(1);

/// Product metric storage kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    /// Saturating monotonic counter.
    Counter,
    /// Current unsigned value.
    Gauge,
    /// Saturating bounded timing histogram.
    Histogram,
}

/// Timing classes that must remain independently observable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimingClass {
    /// Request admission work.
    Admission,
    /// Scheduler or writer queueing.
    Queueing,
    /// Parse, bind, optimize, or prepared-plan lookup.
    Planning,
    /// Native engine execution.
    EngineExecution,
    /// Local protocol or HTTP transport work.
    Transport,
    /// Result encoding.
    ResultEncoding,
    /// Offline or online proof construction.
    ProofConstruction,
    /// WAL append excluding synchronization.
    WalAppend,
    /// Page-file synchronization.
    PageSynchronization,
    /// WAL synchronization.
    WalSynchronization,
    /// Canonical request decoding.
    RequestDecoding,
    /// Offline proof decoding, integrity checks, and semantic verification.
    ProofVerification,
    /// Complete selected durability boundary.
    Durability,
}

/// Closed identities in the v1 product metric registry.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(usize)]
pub enum MetricId {
    /// All admitted product requests.
    Requests = 0,
    /// Admission timing.
    AdmissionMicros = 1,
    /// Queueing timing.
    QueueingMicros = 2,
    /// Planning timing.
    PlanningMicros = 3,
    /// Engine execution timing.
    EngineExecutionMicros = 4,
    /// Transport timing.
    TransportMicros = 5,
    /// Result encoding timing.
    ResultEncodingMicros = 6,
    /// Proof construction timing.
    ProofConstructionMicros = 7,
    /// WAL append timing.
    WalAppendMicros = 8,
    /// Page synchronization timing.
    PageSynchronizationMicros = 9,
    /// WAL synchronization timing.
    WalSynchronizationMicros = 10,
    /// Current scheduler saturation gauge.
    SchedulerSaturation = 11,
    /// Active-expiry attempts.
    ActiveExpiry = 12,
    /// Checkpoint attempts.
    Checkpoints = 13,
    /// Structure and search compaction attempts.
    Compactions = 14,
    /// Page vacuum attempts.
    Vacuums = 15,
    /// WAL retention attempts.
    WalRetentions = 16,
    /// Blob collection attempts.
    BlobCollections = 17,
    /// ANN consolidation attempts.
    AnnConsolidations = 18,
    /// Backup attempts.
    Backups = 19,
    /// Restore attempts.
    Restores = 20,
    /// Safely observed cancellation requests.
    Cancellations = 21,
    /// Expired request deadlines.
    Deadlines = 22,
    /// Product failures, with category retained in bounded events.
    Errors = 23,
    /// Verified native opens and recoveries.
    Recoveries = 24,
    /// Doctor attempts.
    DoctorRuns = 25,
    /// Canonical request decoding timing.
    RequestDecodingMicros = 26,
    /// Offline proof verification timing.
    ProofVerificationMicros = 27,
    /// Complete selected durability timing.
    DurabilityMicros = 28,
}

const METRIC_COUNT: usize = MetricId::DurabilityMicros as usize + 1;

/// One fixed non-user-controlled metric label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricLabel {
    /// Registry-controlled label key.
    pub key: &'static str,
    /// Registry-controlled label value.
    pub value: &'static str,
}

/// Definition of one fixed product metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricDescriptor {
    /// Stable metric identity.
    pub id: MetricId,
    /// Stable dotted metric name.
    pub name: &'static str,
    /// Storage kind.
    pub kind: MetricKind,
    /// Independent timing class, when this is a timing histogram.
    pub timing_class: Option<TimingClass>,
    /// Fixed labels. Callers cannot add labels.
    pub labels: &'static [MetricLabel],
}

const NO_LABELS: &[MetricLabel] = &[];

/// Complete fixed v1 metric registry, in stable snapshot order.
pub const METRIC_REGISTRY_V1: [MetricDescriptor; METRIC_COUNT] = [
    counter(MetricId::Requests, "hyphae.product.requests"),
    timing(
        MetricId::AdmissionMicros,
        "hyphae.product.timing.admission_us",
        TimingClass::Admission,
    ),
    timing(
        MetricId::QueueingMicros,
        "hyphae.product.timing.queueing_us",
        TimingClass::Queueing,
    ),
    timing(
        MetricId::PlanningMicros,
        "hyphae.product.timing.planning_us",
        TimingClass::Planning,
    ),
    timing(
        MetricId::EngineExecutionMicros,
        "hyphae.product.timing.engine_execution_us",
        TimingClass::EngineExecution,
    ),
    timing(
        MetricId::TransportMicros,
        "hyphae.product.timing.transport_us",
        TimingClass::Transport,
    ),
    timing(
        MetricId::ResultEncodingMicros,
        "hyphae.product.timing.result_encoding_us",
        TimingClass::ResultEncoding,
    ),
    timing(
        MetricId::ProofConstructionMicros,
        "hyphae.product.timing.proof_construction_us",
        TimingClass::ProofConstruction,
    ),
    timing(
        MetricId::WalAppendMicros,
        "hyphae.product.timing.wal_append_us",
        TimingClass::WalAppend,
    ),
    timing(
        MetricId::PageSynchronizationMicros,
        "hyphae.product.timing.page_synchronization_us",
        TimingClass::PageSynchronization,
    ),
    timing(
        MetricId::WalSynchronizationMicros,
        "hyphae.product.timing.wal_synchronization_us",
        TimingClass::WalSynchronization,
    ),
    gauge(
        MetricId::SchedulerSaturation,
        "hyphae.product.scheduler.saturation",
    ),
    counter(MetricId::ActiveExpiry, "hyphae.product.active_expiry"),
    counter(MetricId::Checkpoints, "hyphae.product.checkpoints"),
    counter(MetricId::Compactions, "hyphae.product.compactions"),
    counter(MetricId::Vacuums, "hyphae.product.vacuums"),
    counter(MetricId::WalRetentions, "hyphae.product.wal_retentions"),
    counter(MetricId::BlobCollections, "hyphae.product.blob_collections"),
    counter(
        MetricId::AnnConsolidations,
        "hyphae.product.ann_consolidations",
    ),
    counter(MetricId::Backups, "hyphae.product.backups"),
    counter(MetricId::Restores, "hyphae.product.restores"),
    counter(MetricId::Cancellations, "hyphae.product.cancellations"),
    counter(MetricId::Deadlines, "hyphae.product.deadlines"),
    counter(MetricId::Errors, "hyphae.product.errors"),
    counter(MetricId::Recoveries, "hyphae.product.recoveries"),
    counter(MetricId::DoctorRuns, "hyphae.product.doctor_runs"),
    timing(
        MetricId::RequestDecodingMicros,
        "hyphae.product.timing.request_decoding_us",
        TimingClass::RequestDecoding,
    ),
    timing(
        MetricId::ProofVerificationMicros,
        "hyphae.product.timing.proof_verification_us",
        TimingClass::ProofVerification,
    ),
    timing(
        MetricId::DurabilityMicros,
        "hyphae.product.timing.durability_us",
        TimingClass::Durability,
    ),
];

const fn counter(id: MetricId, name: &'static str) -> MetricDescriptor {
    MetricDescriptor {
        id,
        name,
        kind: MetricKind::Counter,
        timing_class: None,
        labels: NO_LABELS,
    }
}

const fn gauge(id: MetricId, name: &'static str) -> MetricDescriptor {
    MetricDescriptor {
        id,
        name,
        kind: MetricKind::Gauge,
        timing_class: None,
        labels: NO_LABELS,
    }
}

const fn timing(id: MetricId, name: &'static str, timing_class: TimingClass) -> MetricDescriptor {
    MetricDescriptor {
        id,
        name,
        kind: MetricKind::Histogram,
        timing_class: Some(timing_class),
        labels: NO_LABELS,
    }
}

/// Bounded lifecycle event kind. No variant carries caller text or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryEventKind {
    /// A backup entered a product phase.
    Backup,
    /// A restore entered a product phase.
    Restore,
    /// Doctor completed one classified attempt.
    Doctor,
    /// A caller cancellation was honored.
    Cancelled,
    /// A deadline was observed as expired.
    Deadline,
    /// A product failure, retaining only its stable category.
    Error(ProductErrorCategory),
}

/// One redacted bounded lifecycle event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryEvent {
    /// Caller-supplied capture time in microseconds.
    pub captured_at_micros: i64,
    /// Closed event identity.
    pub kind: TelemetryEventKind,
}

/// Value of one metric in an internally consistent snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricValue {
    /// Counter value.
    Counter(u64),
    /// Gauge value.
    Gauge(u64),
    /// Timing histogram with non-cumulative bucket counts.
    Histogram {
        /// Number of observations.
        count: u64,
        /// Saturating sum of observed microseconds.
        sum_micros: u64,
        /// Counts for fixed bounds plus one overflow bucket.
        buckets: [u64; TELEMETRY_HISTOGRAM_BOUNDS_MICROS.len() + 1],
    },
}

/// One stable metric row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricRow {
    /// Fixed descriptor.
    pub descriptor: MetricDescriptor,
    /// Captured metric value.
    pub value: MetricValue,
}

/// Internally consistent product telemetry snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetrySnapshot {
    /// Fixed registry version.
    pub registry_version: u16,
    /// Identity shared by snapshots from this registry instance.
    pub process_start_identity: u128,
    /// Identity unique to this product open within the process.
    pub session_start_identity: u128,
    /// Snapshot capture time supplied by the caller.
    pub captured_at_micros: i64,
    /// Current catalog version, when a database snapshot was available.
    pub catalog_version: Option<CatalogVersion>,
    /// Stable rows in [`METRIC_REGISTRY_V1`] order.
    pub metrics: Vec<MetricRow>,
    /// Oldest-to-newest bounded redacted lifecycle events.
    pub events: Vec<TelemetryEvent>,
    /// Events discarded because the configured ring was full.
    pub dropped_events: u64,
}

/// Bounded registry configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryConfig {
    max_events: usize,
}

impl TelemetryConfig {
    /// Constructs event retention bounded by [`MAX_TELEMETRY_EVENTS`].
    pub const fn new(max_events: usize) -> Option<Self> {
        if max_events <= MAX_TELEMETRY_EVENTS {
            Some(Self { max_events })
        } else {
            None
        }
    }

    /// Returns the event ring capacity. Zero disables event capture only.
    pub const fn max_events(self) -> usize {
        self.max_events
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { max_events: 256 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct MetricState {
    value: u64,
    histogram_count: u64,
    histogram_sum: u64,
    histogram_buckets: [u64; TELEMETRY_HISTOGRAM_BOUNDS_MICROS.len() + 1],
}

#[derive(Debug)]
struct RegistryState {
    metrics: [MetricState; METRIC_COUNT],
    events: VecDeque<TelemetryEvent>,
    dropped_events: u64,
}

/// Cloneable process-local telemetry registry.
#[derive(Clone, Debug)]
pub struct TelemetryRegistry {
    process_start_identity: u128,
    session_start_identity: u128,
    max_events: usize,
    state: Arc<Mutex<RegistryState>>,
}

impl Default for TelemetryRegistry {
    fn default() -> Self {
        Self::new(TelemetryConfig::default())
    }
}

impl TelemetryRegistry {
    /// Creates one independent bounded registry.
    pub fn new(config: TelemetryConfig) -> Self {
        let process_start_identity = *PROCESS_START_IDENTITY.get_or_init(|| {
            let started = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            started ^ u128::from(std::process::id())
        });
        let session_sequence = NEXT_SESSION_IDENTITY.fetch_add(1, Ordering::Relaxed);
        let session_start_identity = process_start_identity
            .rotate_left(37)
            .wrapping_add(u128::from(session_sequence));
        Self {
            process_start_identity,
            session_start_identity,
            max_events: config.max_events,
            state: Arc::new(Mutex::new(RegistryState {
                metrics: [MetricState::default(); METRIC_COUNT],
                events: VecDeque::with_capacity(config.max_events),
                dropped_events: 0,
            })),
        }
    }

    /// Saturating increment of one fixed counter.
    pub fn increment(&self, id: MetricId, amount: u64) {
        if METRIC_REGISTRY_V1[id as usize].kind != MetricKind::Counter {
            return;
        }
        let mut state = self.lock_state();
        let metric = &mut state.metrics[id as usize];
        metric.value = metric.value.saturating_add(amount);
    }

    /// Sets one fixed gauge.
    pub fn set_gauge(&self, id: MetricId, value: u64) {
        if METRIC_REGISTRY_V1[id as usize].kind != MetricKind::Gauge {
            return;
        }
        self.lock_state().metrics[id as usize].value = value;
    }

    /// Returns this process's stable start identity.
    pub const fn process_start_identity(&self) -> u128 {
        self.process_start_identity
    }

    /// Returns this product-open/session identity.
    pub const fn session_start_identity(&self) -> u128 {
        self.session_start_identity
    }

    /// Records one duration into the histogram for its independent class.
    pub fn record_timing(&self, class: TimingClass, duration: Duration) {
        let id = timing_metric(class);
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = TELEMETRY_HISTOGRAM_BOUNDS_MICROS
            .iter()
            .position(|bound| micros <= *bound)
            .unwrap_or(TELEMETRY_HISTOGRAM_BOUNDS_MICROS.len());
        let mut state = self.lock_state();
        let metric = &mut state.metrics[id as usize];
        metric.histogram_count = metric.histogram_count.saturating_add(1);
        metric.histogram_sum = metric.histogram_sum.saturating_add(micros);
        metric.histogram_buckets[bucket] = metric.histogram_buckets[bucket].saturating_add(1);
    }

    /// Retains one event containing only fixed identities and stable categories.
    pub fn record_event(&self, event: TelemetryEvent) {
        let mut state = self.lock_state();
        if self.max_events == 0 {
            state.dropped_events = state.dropped_events.saturating_add(1);
            return;
        }
        if state.events.len() == self.max_events {
            state.events.pop_front();
            state.dropped_events = state.dropped_events.saturating_add(1);
        }
        state.events.push_back(event);
    }

    /// Captures all rows and events under one registry lock.
    pub fn snapshot(
        &self,
        captured_at_micros: i64,
        catalog_version: Option<CatalogVersion>,
    ) -> TelemetrySnapshot {
        let state = self.lock_state();
        let metrics = METRIC_REGISTRY_V1
            .iter()
            .copied()
            .map(|descriptor| {
                let metric = state.metrics[descriptor.id as usize];
                let value = match descriptor.kind {
                    MetricKind::Counter => MetricValue::Counter(metric.value),
                    MetricKind::Gauge => MetricValue::Gauge(metric.value),
                    MetricKind::Histogram => MetricValue::Histogram {
                        count: metric.histogram_count,
                        sum_micros: metric.histogram_sum,
                        buckets: metric.histogram_buckets,
                    },
                };
                MetricRow { descriptor, value }
            })
            .collect();
        TelemetrySnapshot {
            registry_version: TELEMETRY_REGISTRY_VERSION,
            process_start_identity: self.process_start_identity,
            session_start_identity: self.session_start_identity,
            captured_at_micros,
            catalog_version,
            metrics,
            events: state.events.iter().copied().collect(),
            dropped_events: state.dropped_events,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

const fn timing_metric(class: TimingClass) -> MetricId {
    match class {
        TimingClass::Admission => MetricId::AdmissionMicros,
        TimingClass::Queueing => MetricId::QueueingMicros,
        TimingClass::Planning => MetricId::PlanningMicros,
        TimingClass::EngineExecution => MetricId::EngineExecutionMicros,
        TimingClass::Transport => MetricId::TransportMicros,
        TimingClass::ResultEncoding => MetricId::ResultEncodingMicros,
        TimingClass::RequestDecoding => MetricId::RequestDecodingMicros,
        TimingClass::ProofConstruction => MetricId::ProofConstructionMicros,
        TimingClass::ProofVerification => MetricId::ProofVerificationMicros,
        TimingClass::WalAppend => MetricId::WalAppendMicros,
        TimingClass::PageSynchronization => MetricId::PageSynchronizationMicros,
        TimingClass::WalSynchronization => MetricId::WalSynchronizationMicros,
        TimingClass::Durability => MetricId::DurabilityMicros,
    }
}
