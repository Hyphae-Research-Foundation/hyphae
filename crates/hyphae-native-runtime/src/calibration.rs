// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded active calibration for hardware-aware Native kernel selection.

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    error::Error as StdError,
    fs::{self, File, OpenOptions},
    hint::black_box,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use nix::{
    sched::{CpuSet, sched_setaffinity},
    unistd::Pid,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::HardwareProfile;
use hyphae_native_btree::BTree;
use hyphae_native_pages::{BufferPool, PageStore};
use hyphae_native_types::{Csn, EngineKind, TransactionId};
use hyphae_native_wal::{PendingRecord, RecordKind, WAL_BLOCK_SIZE, WalFile};

const CALIBRATION_SCHEMA: &str = "hyphae-native-hardware-calibration-v1";
const CACHE_SCHEMA: &str = "hyphae-native-hardware-calibration-cache-v1";
const PPM: u128 = 1_000_000;
const PICOSECONDS_PER_SECOND: u128 = 1_000_000_000_000;
const OPERATION_CALIBRATION_FLOOR: Duration = Duration::from_millis(1);
const OPERATION_CALIBRATION_CONFIRMATIONS: u8 = 3;
const OPERATION_CALIBRATION_MAX_REFINEMENTS: u8 = 6;
const OPERATION_CALIBRATION_TARGET_LOWER_PPM: u128 = 900_000;
const OPERATION_CALIBRATION_TARGET_UPPER_PPM: u128 = 1_100_000;
const MAX_OPERATIONS_PER_SAMPLE: u64 = 1 << 32;
const THREAD_SCALING_MAX_OPERATIONS_PER_SAMPLE: u64 = 1 << 20;
const THREAD_SCALING_BATCH_MINIMUM_TARGET_PPM: u128 = 800_000;
const THREAD_SCALING_BATCH_MAXIMUM_TARGET_PPM: u128 = 1_250_000;
const SMT_RECOMMENDATION_RATIO_PPM: u64 = 1_050_000;
const IO_RECOMMENDATION_FLOOR_PPM: u64 = 950_000;
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Supported active calibration modes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CalibrationMode {
    /// Bounded first-install calibration targeting approximately 5–15 seconds.
    Quick,
    /// Opt-in qualification calibration targeting approximately 3–10 minutes.
    Thorough,
}

/// Build inputs that bind calibration to one executable and toolchain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalibrationRequest {
    /// Requested calibration duration and sample policy.
    pub mode: CalibrationMode,
    /// Full compiler identity captured when the executable was built.
    pub compiler_identity: String,
    /// Product version or immutable build identity.
    pub hyphae_build_identity: String,
    /// Executable whose exact bytes identify the measured build.
    pub executable_path: PathBuf,
}

impl CalibrationRequest {
    /// Creates a request bound to the current executable.
    ///
    /// # Errors
    ///
    /// Returns an error if the current executable path cannot be resolved.
    pub fn for_current_executable(
        mode: CalibrationMode,
        compiler_identity: impl Into<String>,
        hyphae_build_identity: impl Into<String>,
    ) -> Result<Self, CalibrationError> {
        Ok(Self {
            mode,
            compiler_identity: compiler_identity.into(),
            hyphae_build_identity: hyphae_build_identity.into(),
            executable_path: std::env::current_exe()
                .map_err(CalibrationError::CurrentExecutable)?,
        })
    }
}

/// Failure before a complete calibration receipt can be produced.
#[derive(Debug, Error)]
pub enum CalibrationError {
    /// The current executable path was unavailable.
    #[error("calibration could not resolve the current executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    /// The executable could not be read for exact build identity.
    #[error("calibration could not read executable {path}: {source}")]
    ReadExecutable {
        /// Executable path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// The immutable cache entry could not be read.
    #[error("calibration could not read cache entry {path}: {source}")]
    ReadCache {
        /// Cache entry path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// An existing cache entry was malformed or failed semantic validation.
    #[error("calibration cache entry {path} is invalid: {reason}")]
    InvalidCache {
        /// Cache entry path.
        path: PathBuf,
        /// Fail-closed rejection reason.
        reason: &'static str,
    },
    /// The cache directory or atomic entry could not be created.
    #[error("calibration could not write cache entry {path}: {source}")]
    WriteCache {
        /// Cache directory, temporary file, or final entry path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// A measured Native primitive could not create its bounded fixture.
    #[error("calibration primitive setup failed for {primitive}: {source}")]
    PrimitiveSetup {
        /// Primitive whose fixture failed.
        primitive: &'static str,
        /// Underlying setup failure.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// A required build identity was empty.
    #[error("calibration requires a non-empty {0}")]
    MissingIdentity(&'static str),
    /// A diagnostic request did not cover the exact canonical scaling curve.
    #[error(
        "thread-scaling diagnostic worker counts differ: expected {expected:?}, got {actual:?}"
    )]
    InvalidDiagnosticWorkerCounts {
        /// Canonical worker counts derived from the hardware profile.
        expected: Vec<usize>,
        /// Worker counts supplied by the diagnostic orchestrator.
        actual: Vec<usize>,
    },
    /// A diagnostic candidate disagreed with its independent reference.
    #[error("thread-scaling diagnostic correctness failed at {worker_count} workers")]
    DiagnosticCorrectness {
        /// Worker point whose output differed from the reference.
        worker_count: usize,
    },
    /// The cache identity could not be encoded.
    #[error("calibration identity could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Immutable inputs used to decide whether a cached calibration is reusable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationIdentity {
    /// Stable static hardware profile fingerprint.
    pub hardware_fingerprint: String,
    /// Kernel release from the static profile.
    pub kernel_release: String,
    /// Filesystem for the calibrated data path.
    pub filesystem: Option<String>,
    /// Compiler identity embedded by the build.
    pub compiler_identity: String,
    /// Hyphae product/build identity.
    pub hyphae_build_identity: String,
    /// BLAKE3 digest of the exact executable bytes.
    pub executable_blake3: String,
    /// Digest over every cache-reuse input above.
    pub cache_key: String,
}

#[derive(Serialize)]
struct CalibrationCacheFingerprint<'a> {
    identity: &'a CalibrationIdentity,
    mode: CalibrationMode,
    policy: CalibrationPolicy,
}

#[derive(Deserialize, Serialize)]
struct CalibrationCacheEnvelope {
    schema: String,
    receipt_blake3: String,
    receipt: HardwareCalibration,
}

/// Frozen sampling and rejection policy recorded in each receipt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationPolicy {
    /// Nominal lower duration bound for the complete calibration.
    pub minimum_duration_ms: u64,
    /// Nominal upper duration bound for the complete calibration.
    pub maximum_duration_ms: u64,
    /// Unrecorded warmup batches before sampling.
    pub warmup_batches: u32,
    /// Recorded samples for each primitive and input size.
    pub samples_per_measurement: u32,
    /// Target wall time for one sample batch.
    pub target_sample_duration_ms: u64,
    /// Maximum accepted median absolute deviation in parts per million.
    pub maximum_relative_mad_ppm: u64,
    /// Maximum accepted full sample range for diagnostic cells in parts per million.
    pub maximum_relative_range_ppm: u64,
}

impl CalibrationMode {
    fn policy(self) -> CalibrationPolicy {
        match self {
            Self::Quick => CalibrationPolicy {
                minimum_duration_ms: 5_000,
                maximum_duration_ms: 15_000,
                warmup_batches: 2,
                samples_per_measurement: 15,
                target_sample_duration_ms: 15,
                maximum_relative_mad_ppm: 75_000,
                maximum_relative_range_ppm: 500_000,
            },
            Self::Thorough => CalibrationPolicy {
                minimum_duration_ms: 180_000,
                maximum_duration_ms: 600_000,
                warmup_batches: 4,
                samples_per_measurement: 31,
                target_sample_duration_ms: 225,
                maximum_relative_mad_ppm: 40_000,
                maximum_relative_range_ppm: 300_000,
            },
        }
    }
}

/// Feature detection and differential correctness summary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationFeatureDetection {
    /// Instruction sets reported by static runtime feature detection.
    pub instruction_sets: Vec<String>,
    /// Whether every measured candidate matched its reference result.
    pub differential_tests_passed: bool,
}

/// Integer-only timing statistics for one calibrated primitive.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationStatistics {
    /// Fixed timing unit for all values in this structure.
    pub unit: String,
    /// Minimum sampled time per operation.
    pub minimum: u64,
    /// Median sampled time per operation.
    pub median: u64,
    /// Maximum sampled time per operation.
    pub maximum: u64,
    /// Median absolute deviation from the sample median.
    pub median_absolute_deviation: u64,
    /// Median absolute deviation divided by the median, in parts per million.
    pub relative_mad_ppm: u64,
    /// Full sample range divided by the median, in parts per million.
    pub relative_range_ppm: u64,
    /// Derived median byte throughput where the operation has a byte width.
    pub median_bytes_per_second: Option<u64>,
}

/// Differential correctness evidence for one candidate implementation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationCorrectness {
    /// `passed` only when candidate and reference digests are identical.
    pub status: String,
    /// Digest of the candidate output.
    pub result_digest_blake3: String,
    /// Digest of the reference output.
    pub reference_digest_blake3: String,
}

/// One primitive, candidate, and representative input width.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationMeasurement {
    /// Logical primitive identity.
    pub primitive: String,
    /// Candidate implementation identity.
    pub variant: String,
    /// Representative input width.
    pub input_size: u64,
    /// Unit for `input_size`.
    pub input_unit: String,
    /// Bytes touched by one operation when meaningful.
    pub bytes_per_operation: u64,
    /// Calibrated inner-loop operations per recorded sample.
    pub operations_per_sample: u64,
    /// Hard upper bound applied while adapting the inner-loop batch.
    pub maximum_operations_per_sample: u64,
    /// Number of recorded samples.
    pub sample_count: u32,
    /// Distribution summary.
    pub statistics: CalibrationStatistics,
    /// Differential result evidence.
    pub correctness: CalibrationCorrectness,
    /// `stable`, `unstable`, or `rejected`.
    pub status: String,
}

/// Frozen, diagnostic-only thread-scaling sampling policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ThreadScalingDiagnosticPolicy {
    /// Fixed diagnostic mode; never scheduling authority.
    pub mode: CalibrationMode,
    /// Unrecorded batches executed before hot-state calibration.
    pub warmup_batches: u32,
    /// Chronological samples retained for each worker point.
    pub samples_per_measurement: u32,
    /// Target duration for one calibrated sample batch.
    pub target_sample_duration_ms: u64,
    /// Maximum accepted median absolute deviation in parts per million.
    pub maximum_relative_mad_ppm: u64,
    /// Lower convergence bound in parts per million of the target.
    pub operation_calibration_target_lower_ppm: u64,
    /// Upper convergence bound in parts per million of the target.
    pub operation_calibration_target_upper_ppm: u64,
    /// Consecutive in-window probes required for convergence.
    pub operation_calibration_confirmations: u8,
    /// Maximum number of hot-state refinement probes.
    pub operation_calibration_max_refinements: u8,
}

/// One raw, non-authoritative thread-scaling worker point.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ThreadScalingDiagnosticPoint {
    /// Concurrent workers participating in this point.
    pub worker_count: usize,
    /// Candidate and binding identity.
    pub variant: String,
    /// Logical bytes scanned by one operation across all workers.
    pub bytes_per_operation: u64,
    /// Hot-state operations executed in each recorded sample.
    pub operations_per_sample: u64,
    /// Hard operation limit used by calibration.
    pub maximum_operations_per_sample: u64,
    /// `converged` only when the retained median confirms the calibrated target.
    pub batch_calibration_status: String,
    /// Exactly 31 chronological picosecond-per-operation samples.
    pub samples_picoseconds_per_operation: Vec<u64>,
    /// Integer-only statistics derived from the raw samples.
    pub statistics: CalibrationStatistics,
    /// Differential correctness evidence for the worker pool.
    pub correctness: CalibrationCorrectness,
    /// `stable` only when correctness, convergence, and MAD pass.
    pub status: String,
}

/// Raw thread-scaling diagnostics that cannot authorize scheduling or G7.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ThreadScalingDiagnostic {
    /// Frozen thorough diagnostic policy.
    pub policy: ThreadScalingDiagnosticPolicy,
    /// Processor binding used consistently for the complete curve.
    pub binding: String,
    /// Exact canonical worker curve in ascending order.
    pub worker_points: Vec<ThreadScalingDiagnosticPoint>,
}

/// Candidate authorized for scheduler consumption.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectedCalibrationKernel {
    /// Logical primitive identity.
    pub primitive: String,
    /// Representative input width.
    pub input_size: u64,
    /// Unit for `input_size`.
    pub input_unit: String,
    /// Selected implementation identity.
    pub variant: String,
    /// Fail-closed selection rationale.
    pub reason: String,
}

/// Explicitly unsupported calibration surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnsupportedCalibration {
    /// Missing primitive or subsystem.
    pub primitive: String,
    /// Why it cannot yet influence scheduling.
    pub reason: String,
}

/// Measured and unmeasured scope of one receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationCoverage {
    /// Canonically ordered measured primitive identities.
    pub measured: Vec<String>,
    /// Missing surfaces reported fail-closed.
    pub unsupported: Vec<UnsupportedCalibration>,
}

/// Reproducible worker-count decision derived from the measured scaling curve.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationThreadScaling {
    /// Processor placement adapter used by every scaling point.
    pub binding: String,
    /// Effective physical-core boundary after process quota.
    pub physical_core_boundary: u64,
    /// Effective logical-processor boundary after process quota.
    pub logical_processor_boundary: u64,
    /// Canonically ordered worker counts present in the curve.
    pub measured_thread_counts: Vec<u64>,
    /// `stable` only when every scaling point passed correctness and variance.
    pub status: String,
    /// Best stable worker count inside the physical-core range.
    pub physical_peak_threads: Option<u64>,
    /// Throughput at `physical_peak_threads`.
    pub physical_peak_bytes_per_second: Option<u64>,
    /// Best stable worker count in the SMT-only range, when measured.
    pub smt_peak_threads: Option<u64>,
    /// Throughput at `smt_peak_threads`, when measured.
    pub smt_peak_bytes_per_second: Option<u64>,
    /// SMT peak divided by physical peak in parts per million.
    pub smt_to_physical_throughput_ppm: Option<u64>,
    /// Whether the SMT range cleared the frozen five-percent gain threshold.
    pub smt_recommended: bool,
    /// Reproducible worker cap for the recorded binding, absent when unavailable.
    pub recommended_worker_count: Option<u64>,
    /// Human-readable derivation, never a performance claim.
    pub recommendation: String,
}

/// Reproducible storage-concurrency decision from the buffered read sweep.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalibrationIoScaling {
    /// Portable v1 adapter used for the controlled outstanding-read sweep.
    pub binding: String,
    /// Canonically ordered outstanding-read depths present in the curve.
    pub measured_queue_depths: Vec<u64>,
    /// `stable` only when every queue-depth point passed policy.
    pub status: String,
    /// Depth with the highest measured throughput, ties preferring less work.
    pub peak_queue_depth: Option<u64>,
    /// Throughput at `peak_queue_depth`.
    pub peak_bytes_per_second: Option<u64>,
    /// Smallest depth within five percent of peak throughput.
    pub recommended_io_slots: Option<u64>,
    /// Human-readable derivation, never a performance claim.
    pub recommendation: String,
}

/// Versioned active calibration receipt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CalibrationCacheStatus {
    /// Embedded caller deliberately did not request persistence.
    Disabled,
    /// An exact accepted immutable cache entry was reused.
    Hit,
    /// No cache entry existed, so active calibration ran.
    Miss,
}

/// Versioned active calibration receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareCalibration {
    /// Versioned receipt schema.
    pub schema: String,
    /// Requested active mode.
    pub mode: CalibrationMode,
    /// `stable`, `unstable`, or `rejected`.
    pub status: String,
    /// Whether correctness, timing, and scheduler-input stability allow use.
    pub accepted_for_scheduling: bool,
    /// Whether this result was measured directly or reused from exact cache.
    pub cache_status: CalibrationCacheStatus,
    /// Complete elapsed wall time.
    pub elapsed_ms: u64,
    /// Exact cache and executable identity.
    pub identity: CalibrationIdentity,
    /// Sampling and rejection policy.
    pub policy: CalibrationPolicy,
    /// Static feature detection and differential correctness summary.
    pub feature_detection: CalibrationFeatureDetection,
    /// Primitive measurements.
    pub measurements: Vec<CalibrationMeasurement>,
    /// Stable, correct candidates only.
    pub selected_kernels: Vec<SelectedCalibrationKernel>,
    /// Derived worker-count decision from the topology-aware scaling curve.
    pub thread_scaling: CalibrationThreadScaling,
    /// Derived storage-concurrency decision from the queue-depth curve.
    pub io_scaling: CalibrationIoScaling,
    /// Honest implementation coverage.
    pub coverage: CalibrationCoverage,
    /// Performance claims remain empty until qualification evidence exists.
    pub claims: Vec<String>,
}

impl HardwareCalibration {
    /// Runs bounded active CPU and memory calibration against `profile`.
    ///
    /// The current slice never selects an instruction-specific SIMD kernel.
    /// Those candidates remain unsupported until safe implementations and
    /// differential tests exist.
    ///
    /// # Errors
    ///
    /// Returns an error when build identity is missing or the exact executable
    /// cannot be hashed.
    pub fn run(
        profile: &HardwareProfile,
        request: &CalibrationRequest,
    ) -> Result<Self, CalibrationError> {
        let policy = request.mode.policy();
        Self::run_with_policy(profile, request, policy)
    }

    /// Reuses or atomically creates one immutable exact-identity cache entry.
    ///
    /// Only a complete stable receipt is cacheable. An existing malformed,
    /// unstable, mismatched, or partial entry is rejected rather than silently
    /// overwritten.
    ///
    /// # Errors
    ///
    /// Returns an error for missing build identity, executable hashing failure,
    /// invalid existing cache content, or cache I/O failure.
    pub fn run_cached(
        profile: &HardwareProfile,
        request: &CalibrationRequest,
        cache_directory: impl AsRef<Path>,
    ) -> Result<Self, CalibrationError> {
        validate_identity("compiler identity", &request.compiler_identity)?;
        validate_identity("Hyphae build identity", &request.hyphae_build_identity)?;
        let policy = request.mode.policy();
        let identity = calibration_identity(profile, request, policy)?;
        let cache_path = cache_directory
            .as_ref()
            .join(format!("{}.json", identity.cache_key));
        if let Some(mut cached) = read_cache(&cache_path, request.mode, policy, &identity)? {
            cached.cache_status = CalibrationCacheStatus::Hit;
            return Ok(cached);
        }
        let receipt = Self::measure(
            profile,
            request.mode,
            policy,
            identity,
            CalibrationCacheStatus::Miss,
        )?;
        if receipt.accepted_for_scheduling {
            write_cache(cache_directory.as_ref(), &cache_path, &receipt)?;
        }
        Ok(receipt)
    }

    fn run_with_policy(
        profile: &HardwareProfile,
        request: &CalibrationRequest,
        policy: CalibrationPolicy,
    ) -> Result<Self, CalibrationError> {
        validate_identity("compiler identity", &request.compiler_identity)?;
        validate_identity("Hyphae build identity", &request.hyphae_build_identity)?;
        let identity = calibration_identity(profile, request, policy)?;
        Self::measure(
            profile,
            request.mode,
            policy,
            identity,
            CalibrationCacheStatus::Disabled,
        )
    }

    fn measure(
        profile: &HardwareProfile,
        mode: CalibrationMode,
        policy: CalibrationPolicy,
        identity: CalibrationIdentity,
        cache_status: CalibrationCacheStatus,
    ) -> Result<Self, CalibrationError> {
        let started = Instant::now();
        let mut measurements = Vec::new();
        let mut unsupported = unsupported_coverage();
        measure_vector_primitives(&mut measurements, policy);
        measure_byte_primitives(&mut measurements, policy);
        measure_memory_primitives(&mut measurements, policy);
        measure_numa_memory(
            profile,
            &mut measurements,
            &mut unsupported,
            policy,
            started,
        );
        measure_engine_primitives(&mut measurements, policy)?;
        measure_thread_scaling(profile, &mut measurements, &mut unsupported, policy)?;
        measure_storage_primitives(profile, &mut measurements, &mut unsupported, policy)?;

        let atomic = AtomicU64::new(0);
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "atomic-fetch-add",
                "sequentially-consistent-u64",
                64,
                "bits",
                8,
            ),
            policy,
            || atomic.fetch_add(1, AtomicOrdering::SeqCst),
            0,
        ));

        Ok(finish_calibration(
            profile,
            mode,
            policy,
            identity,
            cache_status,
            CalibrationRun {
                measurements,
                unsupported,
                started,
            },
        ))
    }
}

fn measure_vector_primitives(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    for dimension in [8_usize, 128, 384, 1_536] {
        let (left, right) = vector_inputs(dimension);
        let bytes = dimension.saturating_mul(8);
        measurements.push(measure_f64(
            &MeasurementSpec::new(
                "vector-dot",
                "portable-iterator-f64",
                dimension,
                "dimensions",
                bytes,
            ),
            policy,
            || dot_candidate(&left, &right),
            dot_reference(&left, &right),
        ));
        measurements.push(measure_f64(
            &MeasurementSpec::new(
                "vector-l2-squared",
                "portable-iterator-f64",
                dimension,
                "dimensions",
                bytes,
            ),
            policy,
            || l2_candidate(&left, &right),
            l2_reference(&left, &right),
        ));
        measurements.push(measure_f64(
            &MeasurementSpec::new(
                "vector-cosine-similarity",
                "portable-iterator-f64",
                dimension,
                "dimensions",
                bytes,
            ),
            policy,
            || cosine_candidate(&left, &right),
            cosine_reference(&left, &right),
        ));
    }
}

fn measure_byte_primitives(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    for bytes in [16_usize, 128, 4_096] {
        let input = byte_input(bytes);
        let expected_blake3 = blake3_reference(&input);
        measurements.push(measure_bytes32(
            &MeasurementSpec::new("blake3-hash", "blake3-portable", bytes, "bytes", bytes),
            policy,
            || *blake3::hash(&input).as_bytes(),
            expected_blake3,
        ));
        let expected_crc = crc32c_reference(&input);
        measurements.push(measure_u32(
            &MeasurementSpec::new("crc32c", "crc32c-runtime-detected", bytes, "bytes", bytes),
            policy,
            || crc32c::crc32c(&input),
            expected_crc,
        ));
        let mut right = input.clone();
        if let Some(last) = right.last_mut() {
            *last = last.wrapping_add(1);
        }
        measurements.push(measure_ordering(
            &MeasurementSpec::new(
                "byte-comparison",
                "slice-lexicographic",
                bytes,
                "bytes",
                bytes.saturating_mul(2),
            ),
            policy,
            || input.cmp(&right),
            compare_reference(&input, &right),
        ));
    }
}

fn measure_memory_primitives(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    for bytes in [64 * 1_024_usize, 8 * 1_024 * 1_024] {
        let input = byte_input(bytes);
        let expected = sequential_sum_reference(&input);
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "memory-sequential-read",
                "portable-word-scan",
                bytes,
                "bytes",
                bytes,
            ),
            policy,
            || sequential_sum_candidate(&input),
            expected,
        ));
        let indices = random_indices(bytes, 4_096);
        let expected = random_sum_reference(&input, &indices);
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "memory-random-read",
                "deterministic-index-scan",
                bytes,
                "working-set-bytes",
                indices.len().saturating_mul(8),
            ),
            policy,
            || random_sum_candidate(&input, &indices),
            expected,
        ));
    }
}

fn measure_numa_memory(
    profile: &HardwareProfile,
    measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    policy: CalibrationPolicy,
    calibration_started: Instant,
) {
    #[cfg(target_os = "linux")]
    {
        measure_linux_numa_memory(
            profile,
            measurements,
            unsupported,
            policy,
            calibration_started,
        );
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (profile, measurements, policy, calibration_started);
        unsupported.push(UnsupportedCalibration {
            primitive: "numa-local-remote-memory".to_owned(),
            reason: "the current operating system has no safe NUMA affinity adapter".to_owned(),
        });
    }
}

#[cfg(target_os = "linux")]
fn measure_linux_numa_memory(
    profile: &HardwareProfile,
    measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    policy: CalibrationPolicy,
    calibration_started: Instant,
) {
    const WORKING_SET_BYTES: usize = 8 * 1_024 * 1_024;
    let nodes = representative_numa_nodes(profile);
    if nodes.len() < 2 {
        unsupported.push(UnsupportedCalibration {
            primitive: "numa-local-remote-memory".to_owned(),
            reason: "fewer than two process-visible NUMA nodes expose usable processors".to_owned(),
        });
        return;
    }

    let Some(_residency_provider) = safe_numa_residency_provider() else {
        unsupported.push(UnsupportedCalibration {
            primitive: "numa-local-remote-memory".to_owned(),
            reason: "page residency cannot be proven by the current safe Linux adapter; first-touch timing alone is not scheduling evidence".to_owned(),
        });
        return;
    };
    let Some(deadline) = numa_calibration_deadline(calibration_started, policy.maximum_duration_ms)
    else {
        unsupported.push(UnsupportedCalibration {
            primitive: "numa-local-remote-memory".to_owned(),
            reason: "NUMA calibration deadline overflowed before the directed matrix".to_owned(),
        });
        return;
    };

    let mut matrix = Vec::with_capacity(nodes.len().saturating_mul(nodes.len()));
    for (source_node, source_cpu) in &nodes {
        if numa_deadline_reached(deadline, Instant::now()) {
            unsupported.push(UnsupportedCalibration {
                primitive: "numa-local-remote-memory".to_owned(),
                reason: "NUMA calibration reached its cooperative deadline before completing the directed matrix".to_owned(),
            });
            return;
        }
        let input = match first_touch_input(*source_cpu, WORKING_SET_BYTES) {
            Ok(input) => input,
            Err(reason) => {
                unsupported.push(UnsupportedCalibration {
                    primitive: "numa-local-remote-memory".to_owned(),
                    reason,
                });
                return;
            }
        };
        let expected = sequential_sum_reference(&input);
        for (reader_node, reader_cpu) in &nodes {
            if numa_deadline_reached(deadline, Instant::now()) {
                unsupported.push(UnsupportedCalibration {
                    primitive: "numa-local-remote-memory".to_owned(),
                    reason: "NUMA calibration reached its cooperative deadline before completing the directed matrix".to_owned(),
                });
                return;
            }
            let reader =
                match CalibrationPinnedMemoryReader::create(*reader_cpu, Arc::clone(&input)) {
                    Ok(reader) => reader,
                    Err(reason) => {
                        unsupported.push(UnsupportedCalibration {
                            primitive: "numa-local-remote-memory".to_owned(),
                            reason,
                        });
                        return;
                    }
                };
            let variant = format!(
                "linux-first-touch-node-{source_node}-read-node-{reader_node}-cpu-{reader_cpu}"
            );
            matrix.push(measure_u64(
                &MeasurementSpec::new(
                    "numa-memory-read",
                    &variant,
                    WORKING_SET_BYTES,
                    "working-set-bytes",
                    WORKING_SET_BYTES,
                )
                .with_operation_cap(64),
                policy,
                || reader.execute(),
                expected,
            ));
        }
    }
    measurements.extend(matrix);
}

#[cfg(any(target_os = "linux", test))]
fn numa_calibration_deadline(started: Instant, maximum_duration_ms: u64) -> Option<Instant> {
    started.checked_add(Duration::from_millis(maximum_duration_ms))
}

#[cfg(any(target_os = "linux", test))]
fn numa_deadline_reached(deadline: Instant, now: Instant) -> bool {
    now >= deadline
}

#[cfg(any(target_os = "linux", test))]
fn safe_numa_residency_provider() -> Option<&'static str> {
    // Affinity proves where the touching thread ran, not where Linux retained
    // every page. Until a safe provider can bind and audit the exact mapping,
    // the scheduler must not consume first-touch timing as NUMA authority.
    None
}

#[cfg(any(target_os = "linux", test))]
fn representative_numa_nodes(profile: &HardwareProfile) -> Vec<(u32, usize)> {
    profile
        .memory
        .numa_nodes
        .iter()
        .filter_map(|node| {
            let cpu = crate::hardware::parse_cpu_list(&node.cpu_list)
                .into_iter()
                .next()
                .and_then(|cpu| usize::try_from(cpu).ok())?;
            Some((node.id, cpu))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn first_touch_input(cpu: usize, bytes: usize) -> Result<Arc<Vec<u8>>, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name(format!("hyphae-calibration-numa-touch-{cpu}"))
        .spawn(move || {
            let result = pin_current_thread(cpu).map(|()| Arc::new(byte_input(bytes)));
            let _ignored = sender.send(result);
        })
        .map_err(|error| format!("could not start NUMA first-touch worker: {error}"))?;
    let result = receiver
        .recv()
        .map_err(|error| format!("NUMA first-touch worker ended without a result: {error}"))?;
    worker
        .join()
        .map_err(|_| "NUMA first-touch worker panicked".to_owned())?;
    result
}

#[cfg(target_os = "linux")]
fn pin_current_thread(cpu: usize) -> Result<(), String> {
    let mut cpu_set = CpuSet::new();
    cpu_set
        .set(cpu)
        .map_err(|error| format!("CPU {cpu} exceeds the affinity adapter: {error}"))?;
    sched_setaffinity(Pid::from_raw(0), &cpu_set)
        .map_err(|error| format!("could not bind calibration worker to CPU {cpu}: {error}"))
}

fn measure_engine_primitives(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    measure_btree_lookup(measurements, policy)?;
    measure_posting_decode(measurements, policy);
    measure_bitmap_intersection(measurements, policy);
    measure_arena_allocation(measurements, policy);
    measure_channel_handoff(measurements, policy)?;
    Ok(())
}

fn measure_storage_primitives(
    profile: &HardwareProfile,
    measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    let storage_path = Path::new(&profile.storage.path);
    let scratch = CalibrationScratch::create_in(storage_path, "storage-and-wal")?;
    measure_append_primitives(&scratch.path, measurements, policy)?;
    measure_random_page_read(&scratch.path, measurements, policy)?;
    measure_queue_depth(profile, &scratch.path, measurements, policy)?;
    measure_native_wal(&scratch.path, measurements, policy)?;
    measure_direct_io(&scratch.path, measurements, unsupported, policy);
    Ok(())
}

fn measure_append_primitives(
    scratch_path: &Path,
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    const PAGE_BYTES: usize = 4_096;
    let page = vec![0xa5_u8; PAGE_BYTES];
    let expected_page_checksum = checksum_bytes(&page);

    let mut buffered_append = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(scratch_path.join("buffered-append.bin"))
        .map_err(|source| primitive_setup("buffered-append", source))?;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "buffered-append",
            "filesystem-buffered-append-4k",
            PAGE_BYTES,
            "bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(128),
        policy,
        || {
            buffered_append
                .write_all(&page)
                .map_or(u64::MAX, |()| expected_page_checksum)
        },
        expected_page_checksum,
    ));

    let mut data_sync_append = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(scratch_path.join("data-sync-append.bin"))
        .map_err(|source| primitive_setup("data-sync-append", source))?;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "data-sync-append",
            "filesystem-append-4k-sync-data",
            PAGE_BYTES,
            "bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(1),
        policy,
        || {
            data_sync_append
                .write_all(&page)
                .and_then(|()| data_sync_append.sync_data())
                .map_or(u64::MAX, |()| expected_page_checksum)
        },
        expected_page_checksum,
    ));

    let mut full_sync_append = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(scratch_path.join("full-sync-append.bin"))
        .map_err(|source| primitive_setup("full-sync-append", source))?;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "full-sync-append",
            "filesystem-append-4k-sync-all",
            PAGE_BYTES,
            "bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(1),
        policy,
        || {
            full_sync_append
                .write_all(&page)
                .and_then(|()| full_sync_append.sync_all())
                .map_or(u64::MAX, |()| expected_page_checksum)
        },
        expected_page_checksum,
    ));
    Ok(())
}

fn measure_random_page_read(
    scratch_path: &Path,
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    const PAGE_BYTES: usize = 4_096;
    const READ_FIXTURE_BYTES: usize = 8 * 1_024 * 1_024;
    let page = vec![0xa5_u8; PAGE_BYTES];
    let expected_page_checksum = checksum_bytes(&page);
    let read_path = scratch_path.join("random-page-read.bin");
    let mut read_fixture = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&read_path)
        .map_err(|source| primitive_setup("random-page-read", source))?;
    let read_fixture_bytes = vec![0xa5_u8; READ_FIXTURE_BYTES];
    read_fixture
        .write_all(&read_fixture_bytes)
        .and_then(|()| read_fixture.sync_all())
        .map_err(|source| primitive_setup("random-page-read", source))?;
    drop(read_fixture);
    let mut random_read =
        File::open(&read_path).map_err(|source| primitive_setup("random-page-read", source))?;
    let mut read_buffer = vec![0_u8; PAGE_BYTES];
    let mut read_sequence = 0_u64;
    let page_count = READ_FIXTURE_BYTES / PAGE_BYTES;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "random-page-read",
            "filesystem-buffered-seek-read-4k",
            READ_FIXTURE_BYTES,
            "working-set-bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(128),
        policy,
        || {
            let page_index = usize::try_from(read_sequence)
                .unwrap_or(0)
                .wrapping_mul(2_654_435_761)
                % page_count;
            read_sequence = read_sequence.wrapping_add(1);
            let offset = u64::try_from(page_index.saturating_mul(PAGE_BYTES)).unwrap_or(u64::MAX);
            random_read
                .seek(SeekFrom::Start(offset))
                .and_then(|_| random_read.read_exact(&mut read_buffer))
                .map_or(u64::MAX, |()| checksum_bytes(&read_buffer))
        },
        expected_page_checksum,
    ));
    Ok(())
}

fn measure_queue_depth(
    profile: &HardwareProfile,
    scratch_path: &Path,
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    const PAGE_BYTES: usize = 4_096;
    const FIXTURE_BYTES: usize = 8 * 1_024 * 1_024;
    let path = scratch_path.join("queue-depth-read.bin");
    let fixture = vec![0x96_u8; FIXTURE_BYTES];
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|source| primitive_setup("queue-depth-random-read", source))?;
    file.write_all(&fixture)
        .and_then(|()| file.sync_all())
        .map_err(|source| primitive_setup("queue-depth-random-read", source))?;
    drop(file);
    let expected_page = checksum_bytes(&fixture[..PAGE_BYTES]);
    for depth in storage_queue_depth_levels(profile) {
        let pool = CalibrationIoPool::create(depth, &path, FIXTURE_BYTES, PAGE_BYTES)?;
        let expected = expected_page.wrapping_mul(u64::try_from(depth).unwrap_or(u64::MAX));
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "queue-depth-random-read",
                "persistent-sync-workers-buffered-4k",
                depth,
                "outstanding-reads",
                PAGE_BYTES.saturating_mul(depth),
            )
            .with_operation_cap(64),
            policy,
            || pool.execute(),
            expected,
        ));
    }
    Ok(())
}

fn storage_queue_depth_levels(profile: &HardwareProfile) -> Vec<usize> {
    let discovered = profile
        .storage
        .queue_depth
        .and_then(|depth| usize::try_from(depth).ok())
        .unwrap_or(16)
        .clamp(1, 64);
    [1, 4, 16, discovered]
        .into_iter()
        .filter(|depth| *depth <= discovered)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn measure_native_wal(
    scratch_path: &Path,
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    let transaction_id =
        TransactionId::new(1).map_err(|source| primitive_setup("native-wal-append", source))?;
    let record = PendingRecord::new(
        RecordKind::Mutation,
        EngineKind::Structure,
        0,
        transaction_id,
        vec![0x5a; 256],
    )
    .map_err(|source| primitive_setup("native-wal-append", source))?;
    let mut wal = WalFile::create(scratch_path.join("native-unsynchronized.wal"))
        .map_err(|source| primitive_setup("native-wal-append", source))?;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "native-wal-append",
            "native-block-framed-unsynchronized",
            256,
            "record-body-bytes",
            WAL_BLOCK_SIZE,
        )
        .with_operation_cap(8),
        policy,
        || {
            wal.append_records(vec![record.clone()], false)
                .map_or(u64::MAX, |receipts| {
                    u64::try_from(receipts.len()).unwrap_or(u64::MAX)
                })
        },
        1,
    ));

    let mut group_wal = WalFile::create(scratch_path.join("native-group-sync.wal"))
        .map_err(|source| primitive_setup("native-wal-group-flush", source))?;
    let group = vec![record; 8];
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "native-wal-group-flush",
            "native-eight-record-group-sync-data",
            group.len(),
            "records",
            WAL_BLOCK_SIZE,
        )
        .with_operation_cap(1),
        policy,
        || {
            group_wal
                .append_records(group.clone(), true)
                .map_or(u64::MAX, |receipts| {
                    u64::try_from(receipts.len()).unwrap_or(u64::MAX)
                })
        },
        1,
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
#[repr(align(4096))]
struct DirectIoPage([u8; 4_096]);

#[cfg(target_os = "linux")]
fn measure_direct_io(
    scratch_path: &Path,
    measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    policy: CalibrationPolicy,
) {
    const PAGE_BYTES: usize = 4_096;
    let page = DirectIoPage([0x3c; PAGE_BYTES]);
    let expected = checksum_bytes(&page.0);
    let setup = (|| -> Result<(File, File), io::Error> {
        let mut direct_append = OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(scratch_path.join("direct-append.bin"))?;
        write_direct_page(&mut direct_append, &page)?;
        let mut direct_sync = OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(scratch_path.join("direct-sync-append.bin"))?;
        write_direct_page(&mut direct_sync, &page)?;
        direct_sync.sync_data()?;
        Ok((direct_append, direct_sync))
    })();
    let (mut direct_append, mut direct_sync) = match setup {
        Ok(files) => files,
        Err(error) => {
            unsupported.push(UnsupportedCalibration {
                primitive: "direct-io".to_owned(),
                reason: format!("Linux O_DIRECT 4 KiB aligned probe failed: {error}"),
            });
            return;
        }
    };

    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "direct-page-append",
            "linux-o-direct-aligned-4k",
            PAGE_BYTES,
            "bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(8),
        policy,
        || write_direct_page(&mut direct_append, &page).map_or(u64::MAX, |()| expected),
        expected,
    ));
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "direct-page-sync",
            "linux-o-direct-aligned-4k-sync-data",
            PAGE_BYTES,
            "bytes",
            PAGE_BYTES,
        )
        .with_operation_cap(1),
        policy,
        || {
            write_direct_page(&mut direct_sync, &page)
                .and_then(|()| direct_sync.sync_data())
                .map_or(u64::MAX, |()| expected)
        },
        expected,
    ));
}

#[cfg(target_os = "linux")]
fn write_direct_page(file: &mut File, page: &DirectIoPage) -> io::Result<()> {
    let written = file.write(&page.0)?;
    if written == page.0.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("direct write completed {written} of {} bytes", page.0.len()),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn measure_direct_io(
    _scratch_path: &Path,
    _measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    _policy: CalibrationPolicy,
) {
    unsupported.push(UnsupportedCalibration {
        primitive: "direct-io".to_owned(),
        reason: "a safe aligned direct-I/O adapter is not implemented for this operating system"
            .to_owned(),
    });
}

fn measure_btree_lookup(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    const ENTRY_COUNT: u32 = 4_096;
    const VALUE_BYTES: usize = 32;
    let scratch = CalibrationScratch::create("btree-page-lookup")?;
    let mut store = PageStore::create(scratch.path.join("pages.hydb"))
        .map_err(|source| primitive_setup("btree-page-lookup", source))?;
    let entries = (0..ENTRY_COUNT)
        .map(|index| {
            (
                index.to_be_bytes().to_vec(),
                vec![u8::try_from(index % 251).unwrap_or(0); VALUE_BYTES],
            )
        })
        .collect();
    let csn = Csn::new(1).map_err(|source| primitive_setup("btree-page-lookup", source))?;
    let tree = BTree::empty()
        .upsert_sorted_batch(&mut store, csn, entries)
        .map_err(|source| primitive_setup("btree-page-lookup", source))?
        .tree;
    let pool =
        BufferPool::new(128, 4).map_err(|source| primitive_setup("btree-page-lookup", source))?;
    let target = (ENTRY_COUNT / 2).to_be_bytes();
    let expected_value = vec![u8::try_from((ENTRY_COUNT / 2) % 251).unwrap_or(0); VALUE_BYTES];
    let expected = checksum_bytes(&expected_value);
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "btree-page-lookup",
            "native-buffer-pool-pinned",
            usize::try_from(ENTRY_COUNT).unwrap_or(usize::MAX),
            "entries",
            target.len().saturating_add(VALUE_BYTES),
        ),
        policy,
        || match tree.get_cached_pinned(&store, &pool, &target) {
            Ok(Some(value)) => checksum_bytes(value.bytes()),
            Ok(None) | Err(_) => u64::MAX,
        },
        expected,
    ));
    Ok(())
}

fn measure_posting_decode(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    let encoded = crate::encode_search_posting(17);
    let reference = u64::from(u32::from_le_bytes(
        encoded[8..12].try_into().unwrap_or([0; 4]),
    ));
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "posting-decode",
            "native-search-posting-v1",
            encoded.len(),
            "bytes",
            encoded.len(),
        ),
        policy,
        || crate::decode_search_posting(&encoded).map_or(u64::MAX, u64::from),
        reference,
    ));
}

fn measure_bitmap_intersection(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    for bits in [65_536_usize, 1_048_576] {
        let words = bits.div_ceil(64);
        let left = (0..words)
            .map(|index| {
                0xaaaa_aaaa_aaaa_aaaa_u64.rotate_left(u32::try_from(index % 64).unwrap_or(0))
            })
            .collect::<Vec<_>>();
        let right = (0..words)
            .map(|index| {
                0xf0f0_f0f0_f0f0_f0f0_u64.rotate_right(u32::try_from(index % 64).unwrap_or(0))
            })
            .collect::<Vec<_>>();
        let reference = bitmap_intersection_reference(&left, &right);
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "bitmap-intersection",
                "portable-u64-popcount",
                bits,
                "bits",
                words.saturating_mul(16),
            ),
            policy,
            || bitmap_intersection_candidate(&left, &right),
            reference,
        ));
    }
}

fn measure_arena_allocation(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) {
    for bytes in [4_096_usize, 65_536] {
        let expected = u64::try_from(bytes).unwrap_or(u64::MAX) ^ 0x5a ^ 0xa5;
        measurements.push(measure_u64(
            &MeasurementSpec::new(
                "arena-allocation",
                "vec-capacity-fill",
                bytes,
                "bytes",
                bytes,
            ),
            policy,
            || {
                let mut arena: Vec<u8> = Vec::with_capacity(bytes);
                arena.resize(bytes, 0x5a);
                let first = arena.first().copied().map_or(0, u64::from);
                let last = arena.last().copied().map_or(0, u64::from) ^ 0xff;
                u64::try_from(arena.len()).unwrap_or(u64::MAX) ^ first ^ last
            },
            expected,
        ));
    }
}

fn measure_channel_handoff(
    measurements: &mut Vec<CalibrationMeasurement>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    let channel = CalibrationChannel::create()?;
    measurements.push(measure_u64(
        &MeasurementSpec::new(
            "channel-handoff",
            "bounded-cross-thread-round-trip",
            64,
            "bits",
            16,
        ),
        policy,
        || channel.round_trip(),
        0,
    ));
    Ok(())
}

fn measure_thread_scaling(
    profile: &HardwareProfile,
    measurements: &mut Vec<CalibrationMeasurement>,
    unsupported: &mut Vec<UnsupportedCalibration>,
    policy: CalibrationPolicy,
) -> Result<(), CalibrationError> {
    const WORKING_SET_BYTES_PER_WORKER: usize = 256 * 1_024;
    const SCANS_PER_OPERATION: usize = 4;
    let reference_input = byte_input(WORKING_SET_BYTES_PER_WORKER);
    let expected_per_worker = (0..SCANS_PER_OPERATION).fold(0_u64, |total, _| {
        total.wrapping_add(sequential_sum_reference(&reference_input))
    });
    let physical_limit = effective_physical_core_limit(profile);
    #[cfg(target_os = "linux")]
    let cpu_order = thread_binding_cpu_order(profile);
    #[cfg(not(target_os = "linux"))]
    let cpu_order: Option<Vec<usize>> = None;
    if cpu_order.is_none() {
        unsupported.push(UnsupportedCalibration {
            primitive: "thread-affinity-and-numa-scaling".to_owned(),
            reason: if cfg!(target_os = "linux") {
                "the process-visible Linux core topology is incomplete"
            } else {
                "the current operating system has no safe hard-affinity adapter"
            }
            .to_owned(),
        });
    }
    for worker_count in thread_scaling_levels(profile) {
        let binding = cpu_order
            .as_deref()
            .and_then(|order| order.get(..worker_count));
        let pool = CalibrationThreadPool::create(
            worker_count,
            WORKING_SET_BYTES_PER_WORKER,
            SCANS_PER_OPERATION,
            binding,
        )?;
        let expected =
            expected_per_worker.wrapping_mul(u64::try_from(worker_count).unwrap_or(u64::MAX));
        let variant = thread_scaling_variant(worker_count, physical_limit, binding.is_some());
        measurements.push(measure_u64(
            &thread_scaling_measurement_spec(
                worker_count,
                variant,
                WORKING_SET_BYTES_PER_WORKER.saturating_mul(SCANS_PER_OPERATION),
            ),
            policy,
            || pool.execute(),
            expected,
        ));
    }
    Ok(())
}

impl ThreadScalingDiagnostic {
    /// Measures the exact canonical thread-scaling curve while retaining every sample.
    ///
    /// The result is diagnostic-only: it is not a [`HardwareCalibration`], is never
    /// cached, and cannot authorize a scheduler or governor policy.
    ///
    /// # Errors
    ///
    /// Returns an error when `requested_worker_counts` is not the exact canonical
    /// curve, a worker cannot be created or bound, or differential correctness fails.
    pub fn run(
        profile: &HardwareProfile,
        requested_worker_counts: &[usize],
    ) -> Result<Self, CalibrationError> {
        const WORKING_SET_BYTES_PER_WORKER: usize = 256 * 1_024;
        const SCANS_PER_OPERATION: usize = 4;

        let expected_worker_counts = thread_scaling_levels(profile);
        if requested_worker_counts != expected_worker_counts {
            return Err(CalibrationError::InvalidDiagnosticWorkerCounts {
                expected: expected_worker_counts,
                actual: requested_worker_counts.to_vec(),
            });
        }
        let policy = CalibrationMode::Thorough.policy();
        let diagnostic_policy = ThreadScalingDiagnosticPolicy::frozen(policy);
        let reference_input = byte_input(WORKING_SET_BYTES_PER_WORKER);
        let expected_per_worker = (0..SCANS_PER_OPERATION).fold(0_u64, |total, _| {
            total.wrapping_add(sequential_sum_reference(&reference_input))
        });
        let physical_limit = effective_physical_core_limit(profile);
        #[cfg(target_os = "linux")]
        let cpu_order = thread_binding_cpu_order(profile);
        #[cfg(not(target_os = "linux"))]
        let cpu_order: Option<Vec<usize>> = None;
        let binding_name = if cpu_order.is_some() {
            "linux-sched-affinity"
        } else {
            "unbound"
        };
        let mut worker_points = Vec::with_capacity(requested_worker_counts.len());
        for &worker_count in requested_worker_counts {
            let binding = cpu_order
                .as_deref()
                .and_then(|order| order.get(..worker_count));
            let pool = CalibrationThreadPool::create(
                worker_count,
                WORKING_SET_BYTES_PER_WORKER,
                SCANS_PER_OPERATION,
                binding,
            )?;
            let expected =
                expected_per_worker.wrapping_mul(u64::try_from(worker_count).unwrap_or(u64::MAX));
            let variant = thread_scaling_variant(worker_count, physical_limit, binding.is_some());
            worker_points.push(measure_thread_scaling_diagnostic_point(
                &pool,
                worker_count,
                variant,
                WORKING_SET_BYTES_PER_WORKER.saturating_mul(SCANS_PER_OPERATION),
                expected,
                policy,
            )?);
        }
        Ok(Self {
            policy: diagnostic_policy,
            binding: binding_name.to_owned(),
            worker_points,
        })
    }
}

impl ThreadScalingDiagnosticPolicy {
    fn frozen(policy: CalibrationPolicy) -> Self {
        Self {
            mode: CalibrationMode::Thorough,
            warmup_batches: policy.warmup_batches,
            samples_per_measurement: policy.samples_per_measurement,
            target_sample_duration_ms: policy.target_sample_duration_ms,
            maximum_relative_mad_ppm: policy.maximum_relative_mad_ppm,
            operation_calibration_target_lower_ppm: u64::try_from(
                OPERATION_CALIBRATION_TARGET_LOWER_PPM,
            )
            .unwrap_or(u64::MAX),
            operation_calibration_target_upper_ppm: u64::try_from(
                OPERATION_CALIBRATION_TARGET_UPPER_PPM,
            )
            .unwrap_or(u64::MAX),
            operation_calibration_confirmations: OPERATION_CALIBRATION_CONFIRMATIONS,
            operation_calibration_max_refinements: OPERATION_CALIBRATION_MAX_REFINEMENTS,
        }
    }
}

fn measure_thread_scaling_diagnostic_point(
    pool: &CalibrationThreadPool,
    worker_count: usize,
    variant: &str,
    bytes_per_worker: usize,
    expected: u64,
    policy: CalibrationPolicy,
) -> Result<ThreadScalingDiagnosticPoint, CalibrationError> {
    let spec = thread_scaling_measurement_spec(worker_count, variant, bytes_per_worker);
    let candidate = pool.execute();
    let candidate_bytes = candidate.to_le_bytes();
    let reference_bytes = expected.to_le_bytes();
    if candidate != expected {
        return Err(CalibrationError::DiagnosticCorrectness { worker_count });
    }
    let correctness = CalibrationCorrectness {
        status: "passed".to_owned(),
        result_digest_blake3: blake3::hash(&candidate_bytes).to_hex().to_string(),
        reference_digest_blake3: blake3::hash(&reference_bytes).to_hex().to_string(),
    };
    let mut operation = || pool.execute();
    let sampled = sample_operation(&spec, policy, &mut operation);
    let converged = sampled.operation_calibration.converged
        && sampled.operation_calibration.operations < spec.max_operations_per_sample;
    let stable =
        converged && sampled.statistics.relative_mad_ppm <= policy.maximum_relative_mad_ppm;
    Ok(ThreadScalingDiagnosticPoint {
        worker_count,
        variant: variant.to_owned(),
        bytes_per_operation: u64::try_from(spec.bytes_per_operation).unwrap_or(u64::MAX),
        operations_per_sample: sampled.operation_calibration.operations,
        maximum_operations_per_sample: spec.max_operations_per_sample,
        batch_calibration_status: if converged {
            "converged"
        } else {
            "not-converged"
        }
        .to_owned(),
        samples_picoseconds_per_operation: sampled.samples,
        statistics: sampled.statistics,
        correctness,
        status: if stable { "stable" } else { "unstable" }.to_owned(),
    })
}

fn thread_scaling_variant(worker_count: usize, physical_limit: usize, bound: bool) -> &'static str {
    match (worker_count <= physical_limit, bound) {
        (true, true) => "persistent-workers-physical-range-linux-affinity",
        (false, true) => "persistent-workers-smt-range-linux-affinity",
        (true, false) => "persistent-workers-physical-range-unbound",
        (false, false) => "persistent-workers-smt-range-unbound",
    }
}

fn thread_scaling_measurement_spec(
    worker_count: usize,
    variant: &str,
    bytes_per_worker: usize,
) -> MeasurementSpec<'_> {
    MeasurementSpec::new(
        "thread-scaling-memory-scan",
        variant,
        worker_count,
        "threads",
        bytes_per_worker.saturating_mul(worker_count),
    )
    .with_operation_cap(THREAD_SCALING_MAX_OPERATIONS_PER_SAMPLE)
    .requiring_target_convergence()
}

#[cfg(any(target_os = "linux", test))]
fn thread_binding_cpu_order(profile: &HardwareProfile) -> Option<Vec<usize>> {
    let logical_limit = effective_logical_processor_limit(profile);
    let order = physical_core_first_processor_order(profile)?;
    if order.len() < logical_limit {
        return None;
    }
    order
        .into_iter()
        .take(logical_limit)
        .map(|(logical_id, _)| usize::try_from(logical_id))
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

pub(crate) fn physical_core_first_processor_order(
    profile: &HardwareProfile,
) -> Option<Vec<(u32, u32)>> {
    let mut cores = std::collections::BTreeMap::<(Option<u32>, u32, u32), Vec<u32>>::new();
    for processor in &profile.cpu.processor_topology {
        cores
            .entry((
                processor.numa_node_id,
                processor.socket_id,
                processor.core_id,
            ))
            .or_default()
            .push(processor.logical_id);
    }
    if cores.is_empty() {
        return None;
    }
    for siblings in cores.values_mut() {
        siblings.sort_unstable();
        siblings.dedup();
    }
    let mut by_node = std::collections::BTreeMap::<Option<u32>, Vec<Vec<u32>>>::new();
    for ((node, _, _), siblings) in cores {
        by_node.entry(node).or_default().push(siblings);
    }
    let maximum_cores_per_node = by_node.values().map(Vec::len).max().unwrap_or(0);
    let maximum_siblings = by_node
        .values()
        .flat_map(|cores| cores.iter().map(Vec::len))
        .max()
        .unwrap_or(0);
    let mut order = Vec::new();
    for core_index in 0..maximum_cores_per_node {
        for cores in by_node.values() {
            if let Some(cpu) = cores.get(core_index).and_then(|siblings| siblings.first()) {
                order.push((*cpu, 0));
            }
        }
    }
    for sibling_index in 1..maximum_siblings {
        let smt_rank = u32::try_from(sibling_index).ok()?;
        for core_index in 0..maximum_cores_per_node {
            for cores in by_node.values() {
                if let Some(cpu) = cores
                    .get(core_index)
                    .and_then(|siblings| siblings.get(sibling_index))
                {
                    order.push((*cpu, smt_rank));
                }
            }
        }
    }
    let distinct = order
        .iter()
        .map(|(logical_id, _)| *logical_id)
        .collect::<BTreeSet<_>>();
    if distinct.len() != order.len() || order.len() != profile.cpu.processor_topology.len() {
        return None;
    }
    Some(order)
}

fn effective_logical_processor_limit(profile: &HardwareProfile) -> usize {
    let logical = profile.cpu.logical_processors_available.max(1);
    profile.cpu.quota_millicores.map_or(logical, |quota| {
        let quota_processors = usize::try_from(quota.div_ceil(1_000)).unwrap_or(usize::MAX);
        logical.min(quota_processors.max(1))
    })
}

fn effective_physical_core_limit(profile: &HardwareProfile) -> usize {
    profile
        .cpu
        .physical_cores_visible
        .unwrap_or_else(|| effective_logical_processor_limit(profile))
        .min(effective_logical_processor_limit(profile))
        .max(1)
}

fn thread_scaling_levels(profile: &HardwareProfile) -> Vec<usize> {
    let physical = effective_physical_core_limit(profile);
    let logical = effective_logical_processor_limit(profile);
    let mut levels = [1, 2, 4, 8, 16, 32, physical, logical]
        .into_iter()
        .filter(|level| *level <= logical)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    levels.sort_unstable();
    levels
}

fn primitive_setup(
    primitive: &'static str,
    source: impl StdError + Send + Sync + 'static,
) -> CalibrationError {
    CalibrationError::PrimitiveSetup {
        primitive,
        source: Box::new(source),
    }
}

struct CalibrationScratch {
    path: PathBuf,
}

impl CalibrationScratch {
    fn create(primitive: &'static str) -> Result<Self, CalibrationError> {
        Self::create_in(&std::env::temp_dir(), primitive)
    }

    fn create_in(base_path: &Path, primitive: &'static str) -> Result<Self, CalibrationError> {
        let base_directory = if base_path.is_dir() {
            base_path
        } else {
            base_path.parent().ok_or_else(|| {
                primitive_setup(
                    primitive,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "calibration path has no parent directory",
                    ),
                )
            })?
        };
        let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        let path = base_directory.join(format!(
            "hyphae-calibration-{}-{}-{sequence}",
            std::process::id(),
            primitive
        ));
        fs::create_dir(&path).map_err(|source| primitive_setup(primitive, source))?;
        Ok(Self { path })
    }
}

impl Drop for CalibrationScratch {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

enum ChannelMessage {
    Value(u64),
    Stop,
}

struct CalibrationChannel {
    request: SyncSender<ChannelMessage>,
    response: Receiver<u64>,
    sequence: AtomicU64,
    worker: Option<JoinHandle<()>>,
}

impl CalibrationChannel {
    fn create() -> Result<Self, CalibrationError> {
        let (request_sender, request_receiver) = mpsc::sync_channel(1);
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("hyphae-calibration-channel".to_owned())
            .spawn(move || {
                while let Ok(message) = request_receiver.recv() {
                    match message {
                        ChannelMessage::Value(value) => {
                            if response_sender.send(value).is_err() {
                                break;
                            }
                        }
                        ChannelMessage::Stop => break,
                    }
                }
            })
            .map_err(|source| primitive_setup("channel-handoff", source))?;
        Ok(Self {
            request: request_sender,
            response: response_receiver,
            sequence: AtomicU64::new(0),
            worker: Some(worker),
        })
    }

    fn round_trip(&self) -> u64 {
        let value = self.sequence.fetch_add(1, AtomicOrdering::Relaxed);
        if self.request.send(ChannelMessage::Value(value)).is_err() {
            return u64::MAX;
        }
        self.response.recv().map_or(u64::MAX, |response| {
            if response == value {
                response
            } else {
                u64::MAX
            }
        })
    }
}

impl Drop for CalibrationChannel {
    fn drop(&mut self) {
        let _ignored = self.request.send(ChannelMessage::Stop);
        if let Some(worker) = self.worker.take() {
            let _ignored = worker.join();
        }
    }
}

#[cfg(target_os = "linux")]
enum NumaReaderMessage {
    Read,
    Stop,
}

#[cfg(target_os = "linux")]
struct CalibrationPinnedMemoryReader {
    request: SyncSender<NumaReaderMessage>,
    response: Receiver<u64>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl CalibrationPinnedMemoryReader {
    fn create(cpu: usize, input: Arc<Vec<u8>>) -> Result<Self, String> {
        let (request_sender, request_receiver) = mpsc::sync_channel(1);
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name(format!("hyphae-calibration-numa-reader-{cpu}"))
            .spawn(move || {
                if let Err(reason) = pin_current_thread(cpu) {
                    let _ignored = ready_sender.send(Err(reason));
                    return;
                }
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(message) = request_receiver.recv() {
                    match message {
                        NumaReaderMessage::Read => {
                            if response_sender
                                .send(sequential_sum_candidate(black_box(input.as_slice())))
                                .is_err()
                            {
                                break;
                            }
                        }
                        NumaReaderMessage::Stop => break,
                    }
                }
            })
            .map_err(|error| format!("could not start NUMA reader on CPU {cpu}: {error}"))?;
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                request: request_sender,
                response: response_receiver,
                worker: Some(worker),
            }),
            Ok(Err(reason)) => {
                let _ignored = worker.join();
                Err(reason)
            }
            Err(error) => {
                let _ignored = worker.join();
                Err(format!(
                    "NUMA reader on CPU {cpu} ended before affinity confirmation: {error}"
                ))
            }
        }
    }

    fn execute(&self) -> u64 {
        if self.request.send(NumaReaderMessage::Read).is_err() {
            return u64::MAX;
        }
        self.response.recv().unwrap_or(u64::MAX)
    }
}

#[cfg(target_os = "linux")]
impl Drop for CalibrationPinnedMemoryReader {
    fn drop(&mut self) {
        let _ignored = self.request.send(NumaReaderMessage::Stop);
        if let Some(worker) = self.worker.take() {
            let _ignored = worker.join();
        }
    }
}

enum ThreadPoolMessage {
    Execute,
    Stop,
}

struct CalibrationThreadPool {
    requests: Vec<SyncSender<ThreadPoolMessage>>,
    responses: Receiver<u64>,
    workers: Vec<JoinHandle<()>>,
    #[cfg(test)]
    worker_input_addresses: Vec<usize>,
}

impl CalibrationThreadPool {
    fn create(
        worker_count: usize,
        working_set_bytes: usize,
        scans_per_operation: usize,
        cpu_order: Option<&[usize]>,
    ) -> Result<Self, CalibrationError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(worker_count.max(1));
        let mut pool = Self {
            requests: Vec::with_capacity(worker_count),
            responses: response_receiver,
            workers: Vec::with_capacity(worker_count),
            #[cfg(test)]
            worker_input_addresses: Vec::with_capacity(worker_count),
        };
        for worker_index in 0..worker_count {
            let cpu = cpu_order.and_then(|order| order.get(worker_index)).copied();
            let (request_sender, request_receiver) = mpsc::sync_channel(1);
            let worker_response = response_sender.clone();
            let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name(format!("hyphae-calibration-scaling-{worker_index}"))
                .spawn(move || {
                    if let Err(reason) = bind_scaling_worker(cpu) {
                        let _ignored = ready_sender.send(Err(reason));
                        return;
                    }
                    let worker_input = byte_input(working_set_bytes);
                    if ready_sender
                        .send(Ok(worker_input.as_ptr() as usize))
                        .is_err()
                    {
                        return;
                    }
                    while let Ok(message) = request_receiver.recv() {
                        match message {
                            ThreadPoolMessage::Execute => {
                                let result = (0..scans_per_operation).fold(0_u64, |total, _| {
                                    total.wrapping_add(sequential_sum_candidate(black_box(
                                        worker_input.as_slice(),
                                    )))
                                });
                                if worker_response.send(result).is_err() {
                                    break;
                                }
                            }
                            ThreadPoolMessage::Stop => break,
                        }
                    }
                })
                .map_err(|source| primitive_setup("thread-scaling-memory-scan", source))?;
            match ready_receiver.recv() {
                Ok(Ok(worker_input_address)) => {
                    #[cfg(test)]
                    pool.worker_input_addresses.push(worker_input_address);
                    #[cfg(not(test))]
                    let _ = worker_input_address;
                }
                Ok(Err(reason)) => {
                    let _ignored = worker.join();
                    return Err(primitive_setup(
                        "thread-scaling-memory-scan",
                        io::Error::other(reason),
                    ));
                }
                Err(source) => {
                    let _ignored = worker.join();
                    return Err(primitive_setup("thread-scaling-memory-scan", source));
                }
            }
            pool.requests.push(request_sender);
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    fn execute(&self) -> u64 {
        for request in &self.requests {
            if request.send(ThreadPoolMessage::Execute).is_err() {
                return u64::MAX;
            }
        }
        let mut total = 0_u64;
        for _ in &self.requests {
            let Ok(value) = self.responses.recv() else {
                return u64::MAX;
            };
            total = total.wrapping_add(value);
        }
        total
    }
}

fn bind_scaling_worker(cpu: Option<usize>) -> Result<(), String> {
    let Some(cpu) = cpu else {
        return Ok(());
    };
    #[cfg(target_os = "linux")]
    {
        pin_current_thread(cpu)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(format!(
            "hard processor affinity is unavailable for CPU {cpu} on this platform"
        ))
    }
}

impl Drop for CalibrationThreadPool {
    fn drop(&mut self) {
        for request in &self.requests {
            let _ignored = request.send(ThreadPoolMessage::Stop);
        }
        for worker in self.workers.drain(..) {
            let _ignored = worker.join();
        }
    }
}

enum IoPoolMessage {
    Read(u64),
    Stop,
}

struct CalibrationIoPool {
    requests: Vec<SyncSender<IoPoolMessage>>,
    responses: Receiver<u64>,
    sequence: AtomicU64,
    page_count: u64,
    page_bytes: u64,
    workers: Vec<JoinHandle<()>>,
}

impl CalibrationIoPool {
    fn create(
        worker_count: usize,
        path: &Path,
        fixture_bytes: usize,
        page_bytes: usize,
    ) -> Result<Self, CalibrationError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(worker_count.max(1));
        let mut pool = Self {
            requests: Vec::with_capacity(worker_count),
            responses: response_receiver,
            sequence: AtomicU64::new(0),
            page_count: u64::try_from(fixture_bytes / page_bytes).unwrap_or(u64::MAX),
            page_bytes: u64::try_from(page_bytes).unwrap_or(u64::MAX),
            workers: Vec::with_capacity(worker_count),
        };
        for worker_index in 0..worker_count {
            let mut file = File::open(path)
                .map_err(|source| primitive_setup("queue-depth-random-read", source))?;
            let (request_sender, request_receiver) = mpsc::sync_channel(1);
            let worker_response = response_sender.clone();
            let worker = thread::Builder::new()
                .name(format!("hyphae-calibration-io-{worker_index}"))
                .spawn(move || {
                    let mut buffer = vec![0_u8; page_bytes];
                    while let Ok(message) = request_receiver.recv() {
                        match message {
                            IoPoolMessage::Read(offset) => {
                                let result = file
                                    .seek(SeekFrom::Start(offset))
                                    .and_then(|_| file.read_exact(&mut buffer))
                                    .map_or(u64::MAX, |()| checksum_bytes(&buffer));
                                if worker_response.send(result).is_err() {
                                    break;
                                }
                            }
                            IoPoolMessage::Stop => break,
                        }
                    }
                })
                .map_err(|source| primitive_setup("queue-depth-random-read", source))?;
            pool.requests.push(request_sender);
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    fn execute(&self) -> u64 {
        let sequence = self.sequence.fetch_add(1, AtomicOrdering::Relaxed);
        for (worker, request) in self.requests.iter().enumerate() {
            let worker = u64::try_from(worker).unwrap_or(u64::MAX);
            let page = sequence
                .wrapping_mul(u64::try_from(self.requests.len()).unwrap_or(u64::MAX))
                .wrapping_add(worker)
                .wrapping_mul(2_654_435_761)
                % self.page_count;
            if request
                .send(IoPoolMessage::Read(page.saturating_mul(self.page_bytes)))
                .is_err()
            {
                return u64::MAX;
            }
        }
        let mut total = 0_u64;
        for _ in &self.requests {
            let Ok(value) = self.responses.recv() else {
                return u64::MAX;
            };
            total = total.wrapping_add(value);
        }
        total
    }
}

impl Drop for CalibrationIoPool {
    fn drop(&mut self) {
        for request in &self.requests {
            let _ignored = request.send(IoPoolMessage::Stop);
        }
        for worker in self.workers.drain(..) {
            let _ignored = worker.join();
        }
    }
}

fn checksum_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        |checksum, byte| checksum.wrapping_mul(131).wrapping_add(u64::from(*byte)),
    )
}

fn bitmap_intersection_candidate(left: &[u64], right: &[u64]) -> u64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| u64::from((left & right).count_ones()))
        .sum()
}

fn bitmap_intersection_reference(left: &[u64], right: &[u64]) -> u64 {
    let mut count = 0_u64;
    for index in 0..left.len() {
        count += u64::from((left[index] & right[index]).count_ones());
    }
    count
}

fn validate_identity(field: &'static str, value: &str) -> Result<(), CalibrationError> {
    if value.trim().is_empty() {
        Err(CalibrationError::MissingIdentity(field))
    } else {
        Ok(())
    }
}

fn calibration_identity(
    profile: &HardwareProfile,
    request: &CalibrationRequest,
    policy: CalibrationPolicy,
) -> Result<CalibrationIdentity, CalibrationError> {
    let executable_blake3 = hash_file(&request.executable_path)?;
    let mut identity = CalibrationIdentity {
        hardware_fingerprint: profile.fingerprint.clone(),
        kernel_release: profile.operating_system.kernel_release.clone(),
        filesystem: profile.storage.filesystem.clone(),
        compiler_identity: request.compiler_identity.clone(),
        hyphae_build_identity: request.hyphae_build_identity.clone(),
        executable_blake3,
        cache_key: String::new(),
    };
    let encoded = serde_json::to_vec(&CalibrationCacheFingerprint {
        identity: &identity,
        mode: request.mode,
        policy,
    })?;
    identity.cache_key = blake3::hash(&encoded).to_hex().to_string();
    Ok(identity)
}

fn hash_file(path: &Path) -> Result<String, CalibrationError> {
    let mut file = File::open(path).map_err(|source| CalibrationError::ReadExecutable {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1_024].into_boxed_slice();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| CalibrationError::ReadExecutable {
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

struct CalibrationRun {
    measurements: Vec<CalibrationMeasurement>,
    unsupported: Vec<UnsupportedCalibration>,
    started: Instant,
}

pub(crate) fn summarize_thread_scaling(
    profile: &HardwareProfile,
    measurements: &[CalibrationMeasurement],
) -> CalibrationThreadScaling {
    let physical_boundary =
        u64::try_from(effective_physical_core_limit(profile)).unwrap_or(u64::MAX);
    let logical_boundary =
        u64::try_from(effective_logical_processor_limit(profile)).unwrap_or(u64::MAX);
    let scaling = measurements
        .iter()
        .filter(|measurement| measurement.primitive == "thread-scaling-memory-scan")
        .collect::<Vec<_>>();
    let mut measured_thread_counts = scaling
        .iter()
        .map(|measurement| measurement.input_size)
        .collect::<Vec<_>>();
    measured_thread_counts.sort_unstable();
    let binding = thread_scaling_binding(&scaling);
    let stable = binding != "inconsistent"
        && scaling.iter().all(|measurement| {
            measurement.status == "stable"
                && measurement.correctness.status == "passed"
                && measurement.statistics.median_bytes_per_second.is_some()
        });
    let unavailable = || {
        CalibrationThreadScaling {
            binding: binding.to_owned(),
        physical_core_boundary: physical_boundary,
        logical_processor_boundary: logical_boundary,
        measured_thread_counts: measured_thread_counts.clone(),
        status: "unavailable".to_owned(),
        physical_peak_threads: None,
        physical_peak_bytes_per_second: None,
        smt_peak_threads: None,
        smt_peak_bytes_per_second: None,
        smt_to_physical_throughput_ppm: None,
        smt_recommended: false,
            recommended_worker_count: None,
        recommendation: "thread scaling is unavailable because at least one curve point is missing, incorrect, or unstable".to_owned(),
    }
    };
    if !stable {
        return unavailable();
    }
    let Some(physical_peak) = scaling_peak(
        scaling
            .iter()
            .copied()
            .filter(|measurement| measurement.input_size <= physical_boundary),
    ) else {
        return unavailable();
    };
    let physical_throughput = physical_peak
        .statistics
        .median_bytes_per_second
        .unwrap_or(0);
    let smt_peak = scaling_peak(
        scaling
            .iter()
            .copied()
            .filter(|measurement| measurement.input_size > physical_boundary),
    );
    let smt_throughput =
        smt_peak.and_then(|measurement| measurement.statistics.median_bytes_per_second);
    let smt_ratio = smt_throughput.map(|throughput| {
        let ratio = u128::from(throughput)
            .saturating_mul(PPM)
            .checked_div(u128::from(physical_throughput).max(1))
            .unwrap_or(u128::MAX);
        u64::try_from(ratio).unwrap_or(u64::MAX)
    });
    let smt_recommended = smt_ratio.is_some_and(|ratio| ratio >= SMT_RECOMMENDATION_RATIO_PPM);
    let recommended = if smt_recommended {
        smt_peak.map_or(physical_peak.input_size, |measurement| {
            measurement.input_size
        })
    } else {
        physical_peak.input_size
    };
    CalibrationThreadScaling {
        binding: binding.to_owned(),
        physical_core_boundary: physical_boundary,
        logical_processor_boundary: logical_boundary,
        measured_thread_counts,
        status: "stable".to_owned(),
        physical_peak_threads: Some(physical_peak.input_size),
        physical_peak_bytes_per_second: Some(physical_throughput),
        smt_peak_threads: smt_peak.map(|measurement| measurement.input_size),
        smt_peak_bytes_per_second: smt_throughput,
        smt_to_physical_throughput_ppm: smt_ratio,
        smt_recommended,
        recommended_worker_count: Some(recommended),
        recommendation: if smt_recommended {
            "SMT cleared the frozen five-percent throughput-gain threshold; use the measured SMT peak for the recorded placement adapter"
        } else {
            "SMT did not clear the frozen five-percent throughput-gain threshold; use the measured physical-range peak for the recorded placement adapter"
        }
        .to_owned(),
    }
}

fn thread_scaling_binding(scaling: &[&CalibrationMeasurement]) -> &'static str {
    if !scaling.is_empty()
        && scaling
            .iter()
            .all(|measurement| measurement.variant.ends_with("linux-affinity"))
    {
        "linux-sched-affinity"
    } else if !scaling.is_empty()
        && scaling
            .iter()
            .all(|measurement| measurement.variant.ends_with("unbound"))
    {
        "unbound"
    } else {
        "inconsistent"
    }
}

fn scaling_peak<'a>(
    measurements: impl Iterator<Item = &'a CalibrationMeasurement>,
) -> Option<&'a CalibrationMeasurement> {
    measurements.max_by(|left, right| {
        left.statistics
            .median_bytes_per_second
            .cmp(&right.statistics.median_bytes_per_second)
            .then_with(|| right.input_size.cmp(&left.input_size))
    })
}

pub(crate) fn summarize_io_scaling(
    measurements: &[CalibrationMeasurement],
) -> CalibrationIoScaling {
    let scaling = measurements
        .iter()
        .filter(|measurement| measurement.primitive == "queue-depth-random-read")
        .collect::<Vec<_>>();
    let mut measured_queue_depths = scaling
        .iter()
        .map(|measurement| measurement.input_size)
        .collect::<Vec<_>>();
    measured_queue_depths.sort_unstable();
    let stable = !scaling.is_empty()
        && scaling.iter().all(|measurement| {
            measurement.status == "stable"
                && measurement.correctness.status == "passed"
                && measurement.statistics.median_bytes_per_second.is_some()
        });
    if !stable {
        return CalibrationIoScaling {
            binding: "buffered-sync-workers".to_owned(),
            measured_queue_depths,
            status: "unavailable".to_owned(),
            peak_queue_depth: None,
            peak_bytes_per_second: None,
            recommended_io_slots: None,
            recommendation: "I/O concurrency is unavailable because at least one queue-depth point is missing, incorrect, or unstable".to_owned(),
        };
    }
    let peak = scaling_peak(scaling.iter().copied());
    let peak_depth = peak.map(|measurement| measurement.input_size);
    let peak_throughput =
        peak.and_then(|measurement| measurement.statistics.median_bytes_per_second);
    let recommended = peak_throughput.and_then(|peak_bytes| {
        scaling
            .iter()
            .filter(|measurement| {
                measurement
                    .statistics
                    .median_bytes_per_second
                    .is_some_and(|throughput| {
                        u128::from(throughput).saturating_mul(PPM)
                            >= u128::from(peak_bytes)
                                .saturating_mul(u128::from(IO_RECOMMENDATION_FLOOR_PPM))
                    })
            })
            .map(|measurement| measurement.input_size)
            .min()
    });
    CalibrationIoScaling {
        binding: "buffered-sync-workers".to_owned(),
        measured_queue_depths,
        status: "stable".to_owned(),
        peak_queue_depth: peak_depth,
        peak_bytes_per_second: peak_throughput,
        recommended_io_slots: recommended,
        recommendation: "use the smallest measured outstanding-read depth within five percent of peak buffered-read throughput".to_owned(),
    }
}

fn finish_calibration(
    profile: &HardwareProfile,
    mode: CalibrationMode,
    policy: CalibrationPolicy,
    identity: CalibrationIdentity,
    cache_status: CalibrationCacheStatus,
    run: CalibrationRun,
) -> HardwareCalibration {
    let elapsed_ms = u64::try_from(run.started.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let measurements = run.measurements;
    let thread_scaling = summarize_thread_scaling(profile, &measurements);
    let io_scaling = summarize_io_scaling(&measurements);
    let differential_tests_passed = measurements
        .iter()
        .all(|measurement| measurement.correctness.status == "passed");
    let timing_inside_policy =
        elapsed_ms >= policy.minimum_duration_ms && elapsed_ms <= policy.maximum_duration_ms;
    let scheduling_inputs_stable = scheduling_measurements_are_stable(&measurements);
    let accepted_for_scheduling =
        differential_tests_passed && timing_inside_policy && scheduling_inputs_stable;
    let status = if !differential_tests_passed || !timing_inside_policy {
        "rejected"
    } else if scheduling_inputs_stable {
        "stable"
    } else {
        "unstable"
    };
    let selected_kernels = measurements
        .iter()
        .filter(|measurement| accepted_for_scheduling && measurement.status == "stable")
        .map(|measurement| SelectedCalibrationKernel {
            primitive: measurement.primitive.clone(),
            input_size: measurement.input_size,
            input_unit: measurement.input_unit.clone(),
            variant: measurement.variant.clone(),
            reason: "candidate passed correctness and variance policy".to_owned(),
        })
        .collect();
    let measured = measurements
        .iter()
        .map(|measurement| measurement.primitive.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    HardwareCalibration {
        schema: CALIBRATION_SCHEMA.to_owned(),
        mode,
        status: status.to_owned(),
        accepted_for_scheduling,
        cache_status,
        elapsed_ms,
        identity,
        policy,
        feature_detection: CalibrationFeatureDetection {
            instruction_sets: profile.cpu.instruction_sets.clone(),
            differential_tests_passed,
        },
        measurements,
        selected_kernels,
        thread_scaling,
        io_scaling,
        coverage: CalibrationCoverage {
            measured,
            unsupported: run.unsupported,
        },
        claims: Vec::new(),
    }
}

fn read_cache(
    path: &Path,
    mode: CalibrationMode,
    policy: CalibrationPolicy,
    identity: &CalibrationIdentity,
) -> Result<Option<HardwareCalibration>, CalibrationError> {
    let encoded = match fs::read(path) {
        Ok(encoded) => encoded,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CalibrationError::ReadCache {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let envelope: CalibrationCacheEnvelope =
        serde_json::from_slice(&encoded).map_err(|_| CalibrationError::InvalidCache {
            path: path.to_path_buf(),
            reason: "JSON does not decode as calibration cache v1",
        })?;
    if envelope.schema != CACHE_SCHEMA {
        return Err(CalibrationError::InvalidCache {
            path: path.to_path_buf(),
            reason: "cache envelope schema is not supported",
        });
    }
    let receipt_bytes = serde_json::to_vec(&envelope.receipt)?;
    if blake3::hash(&receipt_bytes).to_hex().as_str() != envelope.receipt_blake3 {
        return Err(CalibrationError::InvalidCache {
            path: path.to_path_buf(),
            reason: "cache receipt checksum does not match",
        });
    }
    let receipt = envelope.receipt;
    if !receipt.is_reusable(mode, policy, identity) {
        return Err(CalibrationError::InvalidCache {
            path: path.to_path_buf(),
            reason: "identity, policy, correctness, stability, or selection validation failed",
        });
    }
    Ok(Some(receipt))
}

fn write_cache(
    directory: &Path,
    final_path: &Path,
    receipt: &HardwareCalibration,
) -> Result<(), CalibrationError> {
    fs::create_dir_all(directory).map_err(|source| CalibrationError::WriteCache {
        path: directory.to_path_buf(),
        source,
    })?;
    let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    let temporary_path = directory.join(format!(
        ".{}.{}.{}.tmp",
        receipt.identity.cache_key,
        std::process::id(),
        sequence
    ));
    let mut cached = receipt.clone();
    cached.cache_status = CalibrationCacheStatus::Hit;
    let receipt_bytes = serde_json::to_vec(&cached)?;
    let envelope = CalibrationCacheEnvelope {
        schema: CACHE_SCHEMA.to_owned(),
        receipt_blake3: blake3::hash(&receipt_bytes).to_hex().to_string(),
        receipt: cached,
    };
    let encoded = serde_json::to_vec_pretty(&envelope)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|source| CalibrationError::WriteCache {
            path: temporary_path.clone(),
            source,
        })?;
    if let Err(source) = file.write_all(&encoded).and_then(|()| file.sync_all()) {
        let _ignored = fs::remove_file(&temporary_path);
        return Err(CalibrationError::WriteCache {
            path: temporary_path,
            source,
        });
    }
    drop(file);
    if let Err(source) = fs::rename(&temporary_path, final_path) {
        let _ignored = fs::remove_file(&temporary_path);
        return Err(CalibrationError::WriteCache {
            path: final_path.to_path_buf(),
            source,
        });
    }
    #[cfg(unix)]
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CalibrationError::WriteCache {
            path: directory.to_path_buf(),
            source,
        })?;
    Ok(())
}

impl HardwareCalibration {
    fn is_reusable(
        &self,
        mode: CalibrationMode,
        policy: CalibrationPolicy,
        identity: &CalibrationIdentity,
    ) -> bool {
        self.schema == CALIBRATION_SCHEMA
            && self.mode == mode
            && self.status == "stable"
            && self.accepted_for_scheduling
            && self.identity == *identity
            && self.policy == policy
            && self.elapsed_ms >= policy.minimum_duration_ms
            && self.elapsed_ms <= policy.maximum_duration_ms
            && self.feature_detection.differential_tests_passed
            && !self.measurements.is_empty()
            && self.measurements.iter().all(|measurement| {
                measurement.correctness.status == "passed"
                    && measurement.correctness.result_digest_blake3
                        == measurement.correctness.reference_digest_blake3
            })
            && scheduling_measurements_are_stable(&self.measurements)
            && self.thread_scaling.status == "stable"
            && self.thread_scaling.recommended_worker_count.is_some()
            && self.io_scaling.status == "stable"
            && self.io_scaling.recommended_io_slots.is_some()
            && reusable_numa_coverage(&self.measurements, &self.coverage)
            && selections_match_stable_measurements(&self.selected_kernels, &self.measurements)
            && self.claims.is_empty()
    }
}

fn reusable_numa_coverage(
    measurements: &[CalibrationMeasurement],
    coverage: &CalibrationCoverage,
) -> bool {
    let cells = measurements
        .iter()
        .filter(|measurement| measurement.primitive == "numa-memory-read")
        .collect::<Vec<_>>();
    let unsupported = coverage
        .unsupported
        .iter()
        .any(|entry| entry.primitive == "numa-local-remote-memory");
    if cells.is_empty() {
        return unsupported;
    }
    if unsupported {
        return false;
    }
    let pairs = cells
        .iter()
        .filter_map(|measurement| parse_numa_measurement_variant(&measurement.variant))
        .collect::<BTreeSet<_>>();
    if pairs.len() != cells.len() {
        return false;
    }
    let sources = pairs
        .iter()
        .map(|(source, _)| *source)
        .collect::<BTreeSet<_>>();
    let readers = pairs
        .iter()
        .map(|(_, reader)| *reader)
        .collect::<BTreeSet<_>>();
    let Some(expected_cells) = sources.len().checked_mul(sources.len()) else {
        return false;
    };
    sources.len() >= 2 && sources == readers && pairs.len() == expected_cells
}

fn parse_numa_measurement_variant(variant: &str) -> Option<(u32, u32)> {
    let rest = variant.strip_prefix("linux-first-touch-node-")?;
    let (source, rest) = rest.split_once("-read-node-")?;
    let (reader, cpu) = rest.split_once("-cpu-")?;
    let _cpu = cpu.parse::<u32>().ok()?;
    Some((source.parse().ok()?, reader.parse().ok()?))
}

fn selections_match_stable_measurements(
    selections: &[SelectedCalibrationKernel],
    measurements: &[CalibrationMeasurement],
) -> bool {
    let stable_measurements = measurements
        .iter()
        .filter(|measurement| measurement.status == "stable")
        .collect::<Vec<_>>();
    selections.len() == stable_measurements.len()
        && stable_measurements.iter().all(|measurement| {
            selections.iter().any(|selection| {
                selection.primitive == measurement.primitive
                    && selection.variant == measurement.variant
                    && selection.input_size == measurement.input_size
                    && selection.input_unit == measurement.input_unit
            })
        })
}

fn scheduling_measurements_are_stable(measurements: &[CalibrationMeasurement]) -> bool {
    measurements
        .iter()
        .filter(|measurement| measurement_influences_scheduling(measurement))
        .all(|measurement| measurement.status == "stable")
}

fn measurement_influences_scheduling(measurement: &CalibrationMeasurement) -> bool {
    matches!(
        measurement.primitive.as_str(),
        "thread-scaling-memory-scan" | "numa-memory-read"
    )
}

fn uses_robust_median_stability(primitive: &str) -> bool {
    matches!(
        primitive,
        "thread-scaling-memory-scan" | "queue-depth-random-read" | "numa-memory-read"
    )
}

fn unsupported_coverage() -> Vec<UnsupportedCalibration> {
    [
        (
            "simd-vector-kernels",
            "safe instruction-specific candidates and differential tests are not implemented",
        ),
        (
            "asynchronous-io-adapters",
            "io_uring, IOCP, and equivalent platform-specific adapters are pending",
        ),
    ]
    .into_iter()
    .map(|(primitive, reason)| UnsupportedCalibration {
        primitive: primitive.to_owned(),
        reason: reason.to_owned(),
    })
    .collect()
}

fn measure_f64(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: impl FnMut() -> f64,
    reference: f64,
) -> CalibrationMeasurement {
    measure(spec, policy, operation, reference, |value| {
        value.to_bits().to_le_bytes().to_vec()
    })
}

fn measure_u64(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: impl FnMut() -> u64,
    reference: u64,
) -> CalibrationMeasurement {
    measure(spec, policy, operation, reference, |value| {
        value.to_le_bytes().to_vec()
    })
}

fn measure_u32(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: impl FnMut() -> u32,
    reference: u32,
) -> CalibrationMeasurement {
    measure(spec, policy, operation, reference, |value| {
        value.to_le_bytes().to_vec()
    })
}

fn measure_bytes32(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: impl FnMut() -> [u8; 32],
    reference: [u8; 32],
) -> CalibrationMeasurement {
    measure(spec, policy, operation, reference, |value| value.to_vec())
}

fn measure_ordering(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: impl FnMut() -> Ordering,
    reference: Ordering,
) -> CalibrationMeasurement {
    measure(spec, policy, operation, reference, |value| {
        vec![match value {
            Ordering::Less => 0,
            Ordering::Equal => 1,
            Ordering::Greater => 2,
        }]
    })
}

struct MeasurementSpec<'a> {
    primitive: &'a str,
    variant: &'a str,
    input_size: usize,
    input_unit: &'a str,
    bytes_per_operation: usize,
    max_operations_per_sample: u64,
    require_target_convergence: bool,
}

impl<'a> MeasurementSpec<'a> {
    fn new(
        primitive: &'a str,
        variant: &'a str,
        input_size: usize,
        input_unit: &'a str,
        bytes_per_operation: usize,
    ) -> Self {
        Self {
            primitive,
            variant,
            input_size,
            input_unit,
            bytes_per_operation,
            max_operations_per_sample: MAX_OPERATIONS_PER_SAMPLE,
            require_target_convergence: false,
        }
    }

    fn with_operation_cap(mut self, max_operations_per_sample: u64) -> Self {
        self.max_operations_per_sample = max_operations_per_sample.max(1);
        self
    }

    fn requiring_target_convergence(mut self) -> Self {
        self.require_target_convergence = true;
        self
    }
}

fn measure<T: Copy + PartialEq>(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    mut operation: impl FnMut() -> T,
    reference: T,
    encode: impl Fn(T) -> Vec<u8>,
) -> CalibrationMeasurement {
    let candidate = operation();
    let candidate_bytes = encode(candidate);
    let reference_bytes = encode(reference);
    let correctness_passed = candidate == reference;
    let sampled = sample_operation(spec, policy, &mut operation);
    let stable = (!spec.require_target_convergence || sampled.operation_calibration.converged)
        && statistics_are_stable_for_primitive(spec.primitive, &sampled.statistics, policy);
    let status = if !correctness_passed {
        "rejected"
    } else if stable {
        "stable"
    } else {
        "unstable"
    };
    CalibrationMeasurement {
        primitive: spec.primitive.to_owned(),
        variant: spec.variant.to_owned(),
        input_size: u64::try_from(spec.input_size).unwrap_or(u64::MAX),
        input_unit: spec.input_unit.to_owned(),
        bytes_per_operation: u64::try_from(spec.bytes_per_operation).unwrap_or(u64::MAX),
        operations_per_sample: sampled.operation_calibration.operations,
        maximum_operations_per_sample: spec.max_operations_per_sample,
        sample_count: policy.samples_per_measurement,
        statistics: sampled.statistics,
        correctness: CalibrationCorrectness {
            status: if correctness_passed {
                "passed"
            } else {
                "failed"
            }
            .to_owned(),
            result_digest_blake3: blake3::hash(&candidate_bytes).to_hex().to_string(),
            reference_digest_blake3: blake3::hash(&reference_bytes).to_hex().to_string(),
        },
        status: status.to_owned(),
    }
}

struct SampledOperation {
    operation_calibration: OperationCalibration,
    samples: Vec<u64>,
    statistics: CalibrationStatistics,
}

fn sample_operation<T>(
    spec: &MeasurementSpec<'_>,
    policy: CalibrationPolicy,
    operation: &mut impl FnMut() -> T,
) -> SampledOperation {
    let preliminary_calibration = operations_per_sample(
        operation,
        policy.target_sample_duration_ms,
        spec.max_operations_per_sample,
        false,
    );
    for _ in 0..policy.warmup_batches {
        run_batch(operation, preliminary_calibration.operations);
    }
    let mut operation_calibration = if spec.require_target_convergence {
        operations_per_sample(
            operation,
            policy.target_sample_duration_ms,
            spec.max_operations_per_sample,
            true,
        )
    } else {
        preliminary_calibration
    };
    let mut samples = Vec::with_capacity(policy.samples_per_measurement as usize);
    for _ in 0..policy.samples_per_measurement {
        samples.push(run_batch(operation, operation_calibration.operations));
    }
    let statistics = summarize(&samples, spec.bytes_per_operation);
    if spec.require_target_convergence {
        operation_calibration.converged = operation_calibration.operations
            < spec.max_operations_per_sample
            && recorded_batch_confirms_target(
                &statistics,
                operation_calibration.operations,
                policy.target_sample_duration_ms,
            );
    }
    SampledOperation {
        operation_calibration,
        samples,
        statistics,
    }
}

fn recorded_batch_confirms_target(
    statistics: &CalibrationStatistics,
    operations: u64,
    target_ms: u64,
) -> bool {
    let median_batch_picoseconds =
        u128::from(statistics.median).saturating_mul(u128::from(operations));
    let target_picoseconds = u128::from(target_ms).saturating_mul(1_000_000_000).max(1);
    median_batch_picoseconds.saturating_mul(PPM)
        >= target_picoseconds.saturating_mul(THREAD_SCALING_BATCH_MINIMUM_TARGET_PPM)
        && median_batch_picoseconds.saturating_mul(PPM)
            <= target_picoseconds.saturating_mul(THREAD_SCALING_BATCH_MAXIMUM_TARGET_PPM)
}

fn statistics_are_stable_for_primitive(
    primitive: &str,
    statistics: &CalibrationStatistics,
    policy: CalibrationPolicy,
) -> bool {
    let median_is_stable = statistics.relative_mad_ppm <= policy.maximum_relative_mad_ppm;
    median_is_stable
        && (uses_robust_median_stability(primitive)
            || statistics.relative_range_ppm <= policy.maximum_relative_range_ppm)
}

fn operations_per_sample<T>(
    operation: &mut impl FnMut() -> T,
    target_ms: u64,
    max_operations_per_sample: u64,
    require_target_convergence: bool,
) -> OperationCalibration {
    let target = Duration::from_millis(target_ms.max(1));
    let operation_cap = max_operations_per_sample.max(1);
    if !require_target_convergence {
        return operations_per_sample_once(operation, target, operation_cap);
    }
    calibrated_operations_for_target(target, operation_cap, |operations| {
        let started = Instant::now();
        run_batch(operation, operations);
        started.elapsed()
    })
}

fn operations_per_sample_once<T>(
    operation: &mut impl FnMut() -> T,
    target: Duration,
    operation_cap: u64,
) -> OperationCalibration {
    let mut operations = 1_u64;
    loop {
        let started = Instant::now();
        run_batch(operation, operations);
        let elapsed = started.elapsed();
        if elapsed >= OPERATION_CALIBRATION_FLOOR || operations >= operation_cap {
            return OperationCalibration {
                operations: scaled_operations_for_elapsed(
                    operations,
                    elapsed,
                    target,
                    operation_cap,
                ),
                converged: true,
            };
        }
        operations = operations.saturating_mul(2).min(operation_cap);
    }
}

fn calibrated_operations_for_target(
    target: Duration,
    operation_cap: u64,
    mut measure_elapsed: impl FnMut(u64) -> Duration,
) -> OperationCalibration {
    let operation_cap = operation_cap.max(1);
    let mut operations = 1_u64;
    let initial_elapsed = loop {
        let elapsed = measure_elapsed(operations);
        if elapsed >= OPERATION_CALIBRATION_FLOOR || operations >= operation_cap {
            break elapsed;
        }
        operations = operations.saturating_mul(2).min(operation_cap);
    };

    operations = scaled_operations_for_elapsed(operations, initial_elapsed, target, operation_cap);
    let mut best_operations = operations;
    let mut best_distance = u128::MAX;
    let mut confirmations = 0_u8;
    for _ in 0..OPERATION_CALIBRATION_MAX_REFINEMENTS {
        let elapsed = measure_elapsed(operations);
        let distance = elapsed.as_nanos().abs_diff(target.as_nanos());
        if distance < best_distance {
            best_operations = operations;
            best_distance = distance;
        }
        if sample_duration_matches_target(elapsed, target) {
            confirmations = confirmations.saturating_add(1);
            if confirmations >= OPERATION_CALIBRATION_CONFIRMATIONS {
                return OperationCalibration {
                    operations,
                    converged: operations < operation_cap,
                };
            }
            continue;
        }
        confirmations = 0;
        let refined = scaled_operations_for_elapsed(operations, elapsed, target, operation_cap);
        if refined == operations {
            return OperationCalibration {
                operations: best_operations,
                converged: false,
            };
        }
        operations = refined;
    }
    OperationCalibration {
        operations: best_operations,
        converged: false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperationCalibration {
    operations: u64,
    converged: bool,
}

fn sample_duration_matches_target(elapsed: Duration, target: Duration) -> bool {
    let elapsed_ppm = elapsed.as_nanos().saturating_mul(PPM);
    let target_nanos = target.as_nanos();
    elapsed_ppm >= target_nanos.saturating_mul(OPERATION_CALIBRATION_TARGET_LOWER_PPM)
        && elapsed_ppm <= target_nanos.saturating_mul(OPERATION_CALIBRATION_TARGET_UPPER_PPM)
}

fn scaled_operations_for_elapsed(
    operations: u64,
    elapsed: Duration,
    target: Duration,
    operation_cap: u64,
) -> u64 {
    let operation_cap = operation_cap.max(1);
    let scaled = u128::from(operations.max(1))
        .saturating_mul(target.as_nanos())
        .div_ceil(elapsed.as_nanos().max(1))
        .clamp(1, u128::from(operation_cap));
    u64::try_from(scaled).unwrap_or(operation_cap)
}

fn run_batch<T>(operation: &mut impl FnMut() -> T, operations: u64) -> u64 {
    let started = Instant::now();
    for _ in 0..operations {
        black_box(operation());
    }
    let elapsed_ps = started.elapsed().as_nanos().saturating_mul(1_000);
    let per_operation = elapsed_ps.div_ceil(u128::from(operations)).max(1);
    u64::try_from(per_operation).unwrap_or(u64::MAX)
}

fn summarize(samples: &[u64], bytes_per_operation: usize) -> CalibrationStatistics {
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let minimum = ordered.first().copied().unwrap_or(1).max(1);
    let maximum = ordered.last().copied().unwrap_or(1).max(1);
    let median = ordered[ordered.len() / 2].max(1);
    let mut deviations = ordered
        .iter()
        .map(|sample| sample.abs_diff(median))
        .collect::<Vec<_>>();
    deviations.sort_unstable();
    let median_absolute_deviation = deviations[deviations.len() / 2];
    let relative_mad_ppm = ratio_ppm(median_absolute_deviation, median);
    let relative_range_ppm = ratio_ppm(maximum.saturating_sub(minimum), median);
    let median_bytes_per_second = (bytes_per_operation > 0).then(|| {
        let throughput = (bytes_per_operation as u128).saturating_mul(PICOSECONDS_PER_SECOND)
            / u128::from(median);
        u64::try_from(throughput).unwrap_or(u64::MAX).max(1)
    });
    CalibrationStatistics {
        unit: "picoseconds_per_operation".to_owned(),
        minimum,
        median,
        maximum,
        median_absolute_deviation,
        relative_mad_ppm,
        relative_range_ppm,
        median_bytes_per_second,
    }
}

fn ratio_ppm(numerator: u64, denominator: u64) -> u64 {
    let value = u128::from(numerator).saturating_mul(PPM) / u128::from(denominator.max(1));
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn vector_inputs(dimension: usize) -> (Vec<f32>, Vec<f32>) {
    let left = (0..dimension)
        .map(|index| (f32::from(u8::try_from(index % 29).unwrap_or(0)) - 14.0) / 17.0)
        .collect();
    let right = (0..dimension)
        .map(|index| {
            (f32::from(u8::try_from(index.wrapping_mul(7) % 31).unwrap_or(0)) - 15.0) / 19.0
        })
        .collect();
    (left, right)
}

fn dot_candidate(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum()
}

fn dot_reference(left: &[f32], right: &[f32]) -> f64 {
    let mut result = 0.0;
    for index in 0..left.len() {
        result += f64::from(left[index]) * f64::from(right[index]);
    }
    result
}

fn l2_candidate(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum()
}

fn l2_reference(left: &[f32], right: &[f32]) -> f64 {
    let mut result = 0.0;
    for index in 0..left.len() {
        let delta = f64::from(left[index]) - f64::from(right[index]);
        result += delta * delta;
    }
    result
}

fn cosine_candidate(left: &[f32], right: &[f32]) -> f64 {
    let dot = dot_candidate(left, right);
    let left_norm = dot_candidate(left, left).sqrt();
    let right_norm = dot_candidate(right, right).sqrt();
    dot / (left_norm * right_norm)
}

fn cosine_reference(left: &[f32], right: &[f32]) -> f64 {
    let dot = dot_reference(left, right);
    let left_norm = dot_reference(left, left).sqrt();
    let right_norm = dot_reference(right, right).sqrt();
    dot / (left_norm * right_norm)
}

fn byte_input(bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|index| u8::try_from(index.wrapping_mul(131).wrapping_add(17) % 256).unwrap_or(0))
        .collect()
}

fn blake3_reference(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for chunk in bytes.chunks(17) {
        hasher.update(chunk);
    }
    *hasher.finalize().as_bytes()
}

fn crc32c_reference(bytes: &[u8]) -> u32 {
    const REVERSED_CASTAGNOLI: u32 = 0x82f6_3b78;
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let low_bit_mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (REVERSED_CASTAGNOLI & low_bit_mask);
        }
    }
    !crc
}

fn compare_reference(left: &[u8], right: &[u8]) -> Ordering {
    for index in 0..left.len().min(right.len()) {
        match left[index].cmp(&right[index]) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    left.len().cmp(&right.len())
}

fn sequential_sum_candidate(bytes: &[u8]) -> u64 {
    bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))
        .fold(0, u64::wrapping_add)
}

fn sequential_sum_reference(bytes: &[u8]) -> u64 {
    let mut sum = 0_u64;
    for chunk in bytes.chunks_exact(8) {
        let mut word = [0_u8; 8];
        word.copy_from_slice(chunk);
        sum = sum.wrapping_add(u64::from_le_bytes(word));
    }
    sum
}

fn random_indices(bytes: usize, loads: usize) -> Vec<usize> {
    let words = (bytes / 8).max(1);
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..loads)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % words as u64).unwrap_or(0)
        })
        .collect()
}

fn random_sum_candidate(bytes: &[u8], indices: &[usize]) -> u64 {
    indices.iter().fold(0_u64, |sum, index| {
        let start = index.saturating_mul(8);
        let word = u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap_or([0; 8]));
        sum.wrapping_add(word)
    })
}

fn random_sum_reference(bytes: &[u8], indices: &[usize]) -> u64 {
    let mut sum = 0_u64;
    for index in indices {
        let start = index.saturating_mul(8);
        let mut word = [0_u8; 8];
        word.copy_from_slice(&bytes[start..start + 8]);
        sum = sum.wrapping_add(u64::from_le_bytes(word));
    }
    sum
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn test_policy() -> CalibrationPolicy {
        CalibrationPolicy {
            minimum_duration_ms: 1,
            maximum_duration_ms: 60_000,
            warmup_batches: 1,
            samples_per_measurement: 3,
            target_sample_duration_ms: 1,
            maximum_relative_mad_ppm: 1_000_000,
            maximum_relative_range_ppm: 1_000_000,
        }
    }

    fn measurement_with_status(primitive: &str, status: &str) -> CalibrationMeasurement {
        let mut measurement = measure_u64(
            &MeasurementSpec::new(primitive, "test-variant", 1, "items", 8),
            test_policy(),
            || 1,
            1,
        );
        measurement.status = status.to_owned();
        measurement
    }

    #[test]
    fn scheduler_acceptance_requires_worker_and_numa_stability() {
        let diagnostic = measurement_with_status("native-wal-group-flush", "unstable");
        assert!(scheduling_measurements_are_stable(&[diagnostic]));

        let io_fallback = measurement_with_status("queue-depth-random-read", "unstable");
        assert!(scheduling_measurements_are_stable(&[io_fallback]));

        for primitive in ["thread-scaling-memory-scan", "numa-memory-read"] {
            let scheduler_input = measurement_with_status(primitive, "unstable");
            assert!(!scheduling_measurements_are_stable(&[scheduler_input]));
        }
    }

    #[test]
    fn statistics_use_integer_median_and_mad() {
        let statistics = summarize(&[100, 110, 90, 105, 95], 8);
        assert_eq!(statistics.minimum, 90);
        assert_eq!(statistics.median, 100);
        assert_eq!(statistics.maximum, 110);
        assert_eq!(statistics.median_absolute_deviation, 5);
        assert_eq!(statistics.relative_mad_ppm, 50_000);
        assert_eq!(statistics.relative_range_ppm, 200_000);
        assert_eq!(statistics.median_bytes_per_second, Some(80_000_000_000));
    }

    #[test]
    fn thread_scaling_batch_cap_can_reach_the_thorough_sample_target() {
        let specification = thread_scaling_measurement_spec(4, "test-affinity", 1_048_576);
        let target =
            Duration::from_millis(CalibrationMode::Thorough.policy().target_sample_duration_ms);
        let scaled = scaled_operations_for_elapsed(
            64,
            Duration::from_micros(1_600),
            target,
            specification.max_operations_per_sample,
        );

        assert_eq!(scaled, 9_000);
    }

    #[test]
    fn thorough_targeted_batch_budget_fits_the_duration_contract() {
        // 39 fixed cells, 4 queue-depth cells, and 2 direct-I/O cells use the
        // one-shot selector. Up to 8 thread levels require refined convergence.
        // Expanding either group requires re-budgeting here.
        const MAXIMUM_ONE_SHOT_CELLS: u64 = 39 + 4 + 2;
        const MAXIMUM_CONVERGED_THREAD_CELLS: u64 = 8;
        let policy = CalibrationMode::Thorough.policy();
        let sampled_batches =
            u64::from(policy.samples_per_measurement) + u64::from(policy.warmup_batches);
        let targeted_duration_ms =
            MAXIMUM_ONE_SHOT_CELLS
                .saturating_mul(sampled_batches)
                .saturating_add(MAXIMUM_CONVERGED_THREAD_CELLS.saturating_mul(
                    sampled_batches + u64::from(OPERATION_CALIBRATION_MAX_REFINEMENTS),
                ))
                .saturating_mul(policy.target_sample_duration_ms);

        assert_eq!(targeted_duration_ms, 428_175);
        assert!(targeted_duration_ms <= policy.maximum_duration_ms);
    }

    #[test]
    fn quick_targeted_batch_budget_fits_the_duration_contract() {
        const MAXIMUM_ONE_SHOT_CELLS: u64 = 39 + 4 + 2;
        const MAXIMUM_CONVERGED_THREAD_CELLS: u64 = 8;
        let policy = CalibrationMode::Quick.policy();
        let sampled_batches =
            u64::from(policy.samples_per_measurement) + u64::from(policy.warmup_batches);
        let targeted_duration_ms =
            MAXIMUM_ONE_SHOT_CELLS
                .saturating_mul(sampled_batches)
                .saturating_add(MAXIMUM_CONVERGED_THREAD_CELLS.saturating_mul(
                    sampled_batches + u64::from(OPERATION_CALIBRATION_MAX_REFINEMENTS),
                ))
                .saturating_mul(policy.target_sample_duration_ms);

        assert_eq!(targeted_duration_ms, 14_235);
        assert!(targeted_duration_ms <= policy.maximum_duration_ms);
    }

    #[test]
    fn operation_calibration_rechecks_a_cold_target_before_sampling() {
        let target = Duration::from_millis(225);
        let mut measured_operations = Vec::new();
        let operations = calibrated_operations_for_target(
            target,
            THREAD_SCALING_MAX_OPERATIONS_PER_SAMPLE,
            |candidate| {
                measured_operations.push(candidate);
                match measured_operations.as_slice() {
                    [.., 64] if candidate == 64 => Duration::from_micros(1_600),
                    [.., 9_000] if candidate == 9_000 => {
                        if measured_operations.len() == 8 {
                            target
                        } else {
                            Duration::from_millis(40)
                        }
                    }
                    [.., 50_625] if candidate == 50_625 => target,
                    _ => Duration::from_micros(candidate.saturating_mul(25)),
                }
            },
        );

        assert_eq!(
            operations,
            OperationCalibration {
                operations: 50_625,
                converged: true,
            }
        );
        assert_eq!(
            &measured_operations[7..],
            &[9_000, 9_000, 50_625, 50_625, 50_625]
        );
    }

    #[test]
    fn operation_calibration_fails_closed_when_the_cap_blocks_the_target() {
        let calibration =
            calibrated_operations_for_target(Duration::from_millis(225), 64, |operations| {
                Duration::from_micros(operations.saturating_mul(25))
            });

        assert_eq!(
            calibration,
            OperationCalibration {
                operations: 64,
                converged: false,
            }
        );
    }

    #[test]
    fn operation_calibration_requires_three_consecutive_target_probes() {
        let target = Duration::from_millis(225);
        let mut measured_operations = Vec::new();
        let calibration = calibrated_operations_for_target(
            target,
            THREAD_SCALING_MAX_OPERATIONS_PER_SAMPLE,
            |operations| {
                measured_operations.push(operations);
                match measured_operations.as_slice() {
                    [1] => Duration::from_millis(1),
                    [.., 225] if measured_operations.len() <= 3 => target,
                    [.., 225] => Duration::from_millis(100),
                    [.., 507] => target,
                    _ => Duration::from_millis(1),
                }
            },
        );

        assert_eq!(
            calibration,
            OperationCalibration {
                operations: 507,
                converged: true,
            }
        );
        assert_eq!(measured_operations, [1, 225, 225, 225, 507, 507, 507]);
    }

    #[test]
    fn recorded_thread_scaling_samples_can_revoke_probe_convergence() {
        let inside = summarize(&[2_250_000_000; 31], 1);
        let outside = summarize(&[3_000_000_000; 31], 1);

        assert!(recorded_batch_confirms_target(&inside, 100, 225));
        assert!(!recorded_batch_confirms_target(&outside, 100, 225));
    }

    #[test]
    fn thread_scaling_workers_own_distinct_working_sets() -> Result<(), CalibrationError> {
        let worker_count = 2;
        let working_set_bytes = 4_096;
        let scans_per_operation = 3;
        let input = byte_input(working_set_bytes);
        let expected = sequential_sum_reference(&input)
            .wrapping_mul(u64::try_from(worker_count * scans_per_operation).unwrap_or(u64::MAX));
        let pool = CalibrationThreadPool::create(
            worker_count,
            working_set_bytes,
            scans_per_operation,
            None,
        )?;
        let distinct_addresses = pool
            .worker_input_addresses
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let specification = thread_scaling_measurement_spec(
            worker_count,
            "test-affinity",
            working_set_bytes * scans_per_operation,
        );

        assert_eq!(distinct_addresses.len(), worker_count);
        assert_eq!(
            specification.bytes_per_operation,
            worker_count * working_set_bytes * scans_per_operation
        );
        assert_eq!(pool.execute(), expected);
        Ok(())
    }

    #[test]
    fn scheduler_statistics_use_robust_median_stability_and_retain_tail_range() {
        let statistics = summarize(
            &[
                100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 1_000,
            ],
            8,
        );
        let policy = CalibrationMode::Thorough.policy();
        assert_eq!(statistics.relative_mad_ppm, 0);
        assert_eq!(statistics.relative_range_ppm, 9_000_000);
        assert!(statistics_are_stable_for_primitive(
            "thread-scaling-memory-scan",
            &statistics,
            policy
        ));
        assert!(!statistics_are_stable_for_primitive(
            "native-wal-group-flush",
            &statistics,
            policy
        ));
    }

    #[test]
    fn thread_scaling_levels_respect_physical_smt_and_quota_boundaries()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-scaling-levels-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let mut profile = HardwareProfile::discover(&directory)?;
        profile.cpu.logical_processors_available = 96;
        profile.cpu.physical_cores_visible = Some(48);
        profile.cpu.quota_millicores = None;
        assert_eq!(
            thread_scaling_levels(&profile),
            vec![1, 2, 4, 8, 16, 32, 48, 96]
        );

        profile.cpu.quota_millicores = Some(12_000);
        assert_eq!(thread_scaling_levels(&profile), vec![1, 2, 4, 8, 12]);
        assert_eq!(effective_physical_core_limit(&profile), 12);
        profile.storage.queue_depth = Some(64);
        assert_eq!(storage_queue_depth_levels(&profile), vec![1, 4, 16, 64]);
        profile.storage.queue_depth = Some(2);
        assert_eq!(storage_queue_depth_levels(&profile), vec![1, 2]);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn diagnostic_requires_the_exact_canonical_worker_curve()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-diagnostic-levels-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let mut profile = HardwareProfile::discover(&directory)?;
        profile.cpu.logical_processors_available = 8;
        profile.cpu.physical_cores_visible = Some(4);
        profile.cpu.quota_millicores = None;

        let Err(error) = ThreadScalingDiagnostic::run(&profile, &[1, 2, 4]) else {
            return Err("an incomplete diagnostic curve was accepted".into());
        };
        assert!(matches!(
            error,
            CalibrationError::InvalidDiagnosticWorkerCounts {
                expected,
                actual
            } if expected == vec![1, 2, 4, 8] && actual == vec![1, 2, 4]
        ));
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn representative_numa_nodes_cover_every_visible_node() -> Result<(), Box<dyn StdError>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-numa-pair-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let mut profile = HardwareProfile::discover(&directory)?;
        profile.memory.numa_nodes = vec![
            crate::HardwareNumaNode {
                id: 2,
                cpu_list: "8-15".to_owned(),
                total_bytes: Some(1_024),
                available_bytes: Some(512),
            },
            crate::HardwareNumaNode {
                id: 7,
                cpu_list: "32,40-47".to_owned(),
                total_bytes: Some(1_024),
                available_bytes: Some(512),
            },
        ];
        assert_eq!(representative_numa_nodes(&profile), vec![(2, 8), (7, 32)]);
        profile.memory.numa_nodes.truncate(1);
        assert_eq!(representative_numa_nodes(&profile), vec![(2, 8)]);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn numa_deadline_is_checked_at_every_directed_cell_boundary() -> Result<(), Box<dyn StdError>> {
        let started = Instant::now();
        let deadline = numa_calibration_deadline(started, 1)
            .ok_or_else(|| io::Error::other("test deadline overflow"))?;
        assert!(!numa_deadline_reached(deadline, started));
        assert!(numa_deadline_reached(deadline, deadline));
        Ok(())
    }

    #[test]
    fn first_touch_affinity_is_not_accepted_as_residency_evidence() {
        assert_eq!(safe_numa_residency_provider(), None);
    }

    #[test]
    fn affinity_order_spreads_physical_cores_across_nodes_before_smt()
    -> Result<(), Box<dyn StdError>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-affinity-order-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let mut profile = HardwareProfile::discover(&directory)?;
        profile.cpu.logical_processors_available = 8;
        profile.cpu.physical_cores_visible = Some(4);
        profile.cpu.processor_topology = [
            (0, 0, 0, 0),
            (4, 0, 0, 0),
            (1, 1, 0, 0),
            (5, 1, 0, 0),
            (2, 0, 1, 1),
            (6, 0, 1, 1),
            (3, 1, 1, 1),
            (7, 1, 1, 1),
        ]
        .into_iter()
        .map(
            |(logical_id, core_id, socket_id, node)| crate::HardwareProcessor {
                logical_id,
                core_id,
                socket_id,
                numa_node_id: Some(node),
                thread_siblings: if logical_id < 4 {
                    format!("{logical_id},{}", logical_id + 4)
                } else {
                    format!("{},{}", logical_id - 4, logical_id)
                },
            },
        )
        .collect();
        assert_eq!(
            thread_binding_cpu_order(&profile),
            Some(vec![0, 2, 1, 3, 4, 6, 5, 7])
        );
        profile.cpu.processor_topology.pop();
        assert_eq!(thread_binding_cpu_order(&profile), None);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn portable_candidates_match_independent_references() {
        let (left, right) = vector_inputs(384);
        assert_eq!(
            dot_candidate(&left, &right).to_bits(),
            dot_reference(&left, &right).to_bits()
        );
        assert_eq!(
            l2_candidate(&left, &right).to_bits(),
            l2_reference(&left, &right).to_bits()
        );
        assert_eq!(
            cosine_candidate(&left, &right).to_bits(),
            cosine_reference(&left, &right).to_bits()
        );
        let bytes = byte_input(8 * 1_024);
        assert_eq!(*blake3::hash(&bytes).as_bytes(), blake3_reference(&bytes));
        assert_eq!(crc32c::crc32c(&bytes), crc32c_reference(&bytes));
        assert_eq!(
            sequential_sum_candidate(&bytes),
            sequential_sum_reference(&bytes)
        );
        let indices = random_indices(bytes.len(), 1_024);
        assert_eq!(
            random_sum_candidate(&bytes, &indices),
            random_sum_reference(&bytes, &indices)
        );
    }

    #[test]
    fn executable_digest_and_cache_key_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-identity-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let executable = directory.join("fixture.bin");
        fs::write(&executable, b"fixed executable bytes")?;
        let profile = HardwareProfile::discover(&directory)?;
        let request = CalibrationRequest {
            mode: CalibrationMode::Quick,
            compiler_identity: "rustc test".to_owned(),
            hyphae_build_identity: "hyphae test".to_owned(),
            executable_path: executable,
        };
        let first = calibration_identity(&profile, &request, test_policy())?;
        let second = calibration_identity(&profile, &request, test_policy())?;
        assert_eq!(first, second);
        assert_eq!(first.executable_blake3.len(), 64);
        assert_eq!(first.cache_key.len(), 64);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn assert_mandatory_calibration_coverage(receipt: &HardwareCalibration) {
        for primitive in [
            "btree-page-lookup",
            "posting-decode",
            "bitmap-intersection",
            "arena-allocation",
            "channel-handoff",
            "buffered-append",
            "data-sync-append",
            "full-sync-append",
            "random-page-read",
            "native-wal-append",
            "native-wal-group-flush",
            "thread-scaling-memory-scan",
            "queue-depth-random-read",
        ] {
            assert!(
                receipt
                    .measurements
                    .iter()
                    .any(|measurement| measurement.primitive == primitive)
            );
            assert!(
                receipt
                    .coverage
                    .unsupported
                    .iter()
                    .all(|entry| entry.primitive != primitive)
            );
        }
    }

    #[test]
    fn short_calibration_is_schema_shaped_and_claim_free() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory =
            std::env::temp_dir().join(format!("hyphae-calibration-receipt-{}", std::process::id()));
        fs::create_dir_all(&directory)?;
        let executable = directory.join("fixture.bin");
        fs::write(&executable, b"fixed executable bytes")?;
        let profile = HardwareProfile::discover(&directory)?;
        let request = CalibrationRequest {
            mode: CalibrationMode::Quick,
            compiler_identity: "rustc test".to_owned(),
            hyphae_build_identity: "hyphae test".to_owned(),
            executable_path: executable,
        };
        let receipt = HardwareCalibration::run_with_policy(&profile, &request, test_policy())?;
        assert_eq!(receipt.schema, CALIBRATION_SCHEMA);
        let direct_measurements = receipt
            .measurements
            .iter()
            .filter(|measurement| {
                matches!(
                    measurement.primitive.as_str(),
                    "direct-page-append" | "direct-page-sync"
                )
            })
            .count();
        let numa_measurements = receipt
            .measurements
            .iter()
            .filter(|measurement| measurement.primitive == "numa-memory-read")
            .count();
        assert!(
            numa_measurements == 0
                || numa_measurements
                    == profile
                        .memory
                        .numa_nodes
                        .len()
                        .saturating_mul(profile.memory.numa_nodes.len())
        );
        assert!(matches!(direct_measurements, 0 | 2));
        assert_eq!(
            receipt.measurements.len(),
            39 + thread_scaling_levels(&profile).len()
                + storage_queue_depth_levels(&profile).len()
                + numa_measurements
                + direct_measurements
        );
        assert_eq!(
            direct_measurements == 0,
            receipt
                .coverage
                .unsupported
                .iter()
                .any(|entry| entry.primitive == "direct-io")
        );
        assert_eq!(
            numa_measurements == 0,
            receipt
                .coverage
                .unsupported
                .iter()
                .any(|entry| entry.primitive == "numa-local-remote-memory")
        );
        assert!(receipt.claims.is_empty());
        assert!(
            receipt
                .measurements
                .iter()
                .all(|measurement| measurement.correctness.status == "passed")
        );
        assert_eq!(
            receipt.thread_scaling.binding != "linux-sched-affinity",
            receipt
                .coverage
                .unsupported
                .iter()
                .any(|entry| entry.primitive == "thread-affinity-and-numa-scaling")
        );
        assert_mandatory_calibration_coverage(&receipt);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn accepted_cache_round_trips_and_rejects_corruption() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory =
            std::env::temp_dir().join(format!("hyphae-calibration-cache-{}", std::process::id()));
        let cache_directory = directory.join("cache");
        fs::create_dir_all(&directory)?;
        let executable = directory.join("fixture.bin");
        fs::write(&executable, b"fixed executable bytes")?;
        let profile = HardwareProfile::discover(&directory)?;
        let request = CalibrationRequest {
            mode: CalibrationMode::Quick,
            compiler_identity: "rustc test".to_owned(),
            hyphae_build_identity: "hyphae test".to_owned(),
            executable_path: executable,
        };
        let policy = test_policy();
        let identity = calibration_identity(&profile, &request, policy)?;
        let mut receipt = HardwareCalibration::run_with_policy(&profile, &request, policy)?;
        normalize_cache_fixture(&profile, &mut receipt, policy);
        let cache_path = cache_directory.join(format!("{}.json", identity.cache_key));
        write_cache(&cache_directory, &cache_path, &receipt)?;
        let cached = read_cache(&cache_path, request.mode, policy, &identity)?;
        assert!(matches!(
            cached.map(|value| value.cache_status),
            Some(CalibrationCacheStatus::Hit)
        ));

        let mut envelope: CalibrationCacheEnvelope =
            serde_json::from_slice(&fs::read(&cache_path)?)?;
        envelope.receipt.elapsed_ms = envelope.receipt.elapsed_ms.saturating_add(1);
        fs::write(&cache_path, serde_json::to_vec_pretty(&envelope)?)?;
        assert!(matches!(
            read_cache(&cache_path, request.mode, policy, &identity),
            Err(CalibrationError::InvalidCache { .. })
        ));
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn cache_identity_includes_mode_and_policy() -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-calibration-cache-key-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)?;
        let executable = directory.join("fixture.bin");
        fs::write(&executable, b"fixed executable bytes")?;
        let profile = HardwareProfile::discover(&directory)?;
        let quick = CalibrationRequest {
            mode: CalibrationMode::Quick,
            compiler_identity: "rustc test".to_owned(),
            hyphae_build_identity: "hyphae test".to_owned(),
            executable_path: executable.clone(),
        };
        let thorough = CalibrationRequest {
            mode: CalibrationMode::Thorough,
            executable_path: executable,
            ..quick.clone()
        };
        let quick_identity = calibration_identity(&profile, &quick, quick.mode.policy())?;
        let thorough_identity = calibration_identity(&profile, &thorough, thorough.mode.policy())?;
        assert_ne!(quick_identity.cache_key, thorough_identity.cache_key);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn normalize_cache_fixture(
        profile: &HardwareProfile,
        receipt: &mut HardwareCalibration,
        policy: CalibrationPolicy,
    ) {
        receipt.status = "stable".to_owned();
        receipt.accepted_for_scheduling = true;
        receipt.cache_status = CalibrationCacheStatus::Miss;
        receipt.elapsed_ms = policy.minimum_duration_ms;
        for measurement in &mut receipt.measurements {
            measurement.status = "stable".to_owned();
            measurement.statistics.relative_mad_ppm = 0;
            measurement.statistics.relative_range_ppm = 0;
            measurement.correctness.status = "passed".to_owned();
            measurement.correctness.reference_digest_blake3 =
                measurement.correctness.result_digest_blake3.clone();
        }
        receipt.selected_kernels = receipt
            .measurements
            .iter()
            .map(|measurement| SelectedCalibrationKernel {
                primitive: measurement.primitive.clone(),
                input_size: measurement.input_size,
                input_unit: measurement.input_unit.clone(),
                variant: measurement.variant.clone(),
                reason: "test fixture".to_owned(),
            })
            .collect();
        receipt.thread_scaling = summarize_thread_scaling(profile, &receipt.measurements);
        receipt.io_scaling = summarize_io_scaling(&receipt.measurements);
    }
}
