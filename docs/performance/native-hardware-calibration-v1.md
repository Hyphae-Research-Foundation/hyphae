<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->

# Native hardware calibration v1

Status: CPU, memory, engine, storage, and WAL quick/thorough slice implemented;
P1 remains open

Active calibration measures the current executable on the hardware described by
`HardwareProfile`. The embedded Rust surfaces are
`CalibrationRequest::for_current_executable` and `HardwareCalibration::run`.
The CLI surface is:

```text
hyphae hardware calibrate [--data-dir <PATH>] [--mode <quick|thorough>]
                           [--cache-dir <PATH> | --no-cache]
```

`quick` records 15 samples per cell and targets approximately 5–15 seconds.
`thorough` records 31 longer samples per cell and targets approximately 3–10
minutes. Discovery remains the separate, read-only, sub-second command. Active
calibration reads the executable for its exact digest and allocates bounded
process memory. It writes only a uniquely named, bounded scratch directory on
the selected data filesystem, closes every fixture, and removes that directory
before returning. It never modifies an existing Hyphae file or host policy.
The CLI caches accepted results under the platform user cache directory unless
`--cache-dir` overrides it or `--no-cache` disables persistence.

The public receipt is frozen by
[`native-hardware-calibration-v1.schema.json`](../../contracts/json-schema/native-hardware-calibration-v1.schema.json).
The semantic checker is:

```text
python3 tools/check_native_hardware_calibration.py --receipt <RECEIPT.json>
```

Thread-scaling failures can be inspected without rerunning every calibration
surface. The separate diagnostic contract retains exactly 31 chronological
samples for every canonical worker count:

```text
python3 tools/run_native_hardware_calibration_diagnostic.py \
  --producer <EXACT-G7-RUNNER> \
  --source-commit <COMMIT> --source-tree <TREE> \
  --platform <PLATFORM> --hardware-profile <PROFILE.json> \
  --producer-executable-blake3 <BLAKE3> \
  --compiler-identity <IDENTITY> --hyphae-build-identity <IDENTITY> \
  --worker-counts <ORDERED-COUNTS> --output <DIAGNOSTIC.json>
```

The producer measures through the same private worker buffers, affinity order,
hot-state batch convergence, correctness reference, and 225 ms thorough policy.
The orchestrator independently hashes the producer bytes before execution. The
checker recomputes every statistic from the raw samples and requires the frozen
operation cap, exact worker curve, differential digests, source identity, and
hardware fingerprint. The envelope is permanently `diagnostic-only`, with
`authority=false`, empty claims, and no cache or scheduling fields. It cannot
select a governor policy, substitute for a complete thorough receipt, or close
G7.

## Identity and cache key

Each receipt binds the stable hardware fingerprint, kernel release, selected
filesystem, compiler identity embedded at build time, Hyphae package identity,
and a BLAKE3 digest of the exact running executable. `cache_key` hashes those
reuse inputs together with mode and frozen sampling policy. Quick and thorough
results therefore cannot collide.

Accepted receipts are wrapped in an internal versioned envelope with a BLAKE3
checksum, then written through a new temporary file, file sync, atomic rename,
and directory sync on Unix. Cache entries are immutable. Reuse requires
the exact identity and policy plus a complete stable receipt, matching result
digests, complete selections, and empty claims. Existing malformed or unstable
entries fail closed and are never overwritten silently. `cache_status` reports
`disabled`, `miss`, or `hit`; only an accepted receipt may be a hit.

## Measurements and rejection

The active CPU, memory, engine, storage, and WAL slice measures 39 fixed cells,
a topology-derived thread-scaling curve, and the controlled I/O-depth curve:

- portable `f64` dot product, squared L2, and cosine at 8, 128, 384, and 1,536
  dimensions;
- BLAKE3, CRC32C, and lexicographic byte comparison at 16, 128, and 4,096
  bytes;
- sequential and deterministic-random memory reads over cache-sized and
  memory-sized inputs; and
- sequentially consistent 64-bit atomic fetch-add cost;
- a hot pinned lookup through the real Native B+tree and buffer pool over a
  4,096-entry multilevel fixture;
- the canonical Native search-posting decoder;
- portable bitmap intersection over 65,536-bit and 1,048,576-bit sets;
- bounded 4 KiB and 64 KiB request-arena allocation/fill; and
- a bounded cross-thread channel round trip;
- buffered 4 KiB append, 4 KiB append plus `sync_data`, and 4 KiB append plus
  `sync_all` on the selected data filesystem;
- deterministic 4 KiB seeks and reads over an 8 MiB temporary page fixture;
- a controlled outstanding-read curve at depths 1, 4, 16, and the discovered
  device limit (bounded to 64), using persistent workers and independent file
  handles; and
- block-framed Native WAL append without synchronization and an eight-record
  Native group append with physical `sync_data`; and
- on Linux filesystems that accept the active capability probe, aligned 4 KiB
  `O_DIRECT` append with and without `sync_data`; and
- a persistent-worker memory-scan curve at powers of two, the effective
  physical-core boundary, and the effective logical-processor boundary.

First-touch plus CPU affinity does not prove where Linux retained every page.
The current safe adapter therefore records multi-node NUMA memory calibration
as explicitly unsupported and emits no timing cells. A future adapter may emit
the complete directed `N x N` matrix only after proving the exact isolated VMA
residency before and after every cell. The checker already requires identical
source and reader sets, every directed pair exactly once, canonical picoseconds
per operation, the frozen working set, and exact byte accounting. Measurement
is atomic: any residency, affinity, or cooperative-deadline failure discards
the whole matrix. Unsupported NUMA evidence disables cross-node stealing.

The scaling curve respects the process affinity and cgroup CPU quota reported
by the static profile. It tests the physical range first and adds an SMT range
only when logical capacity exceeds visible physical cores. Workers are reused
across samples so the result measures dispatch plus parallel work rather than
thread creation. When Linux exposes a complete processor topology, each worker
is hard-bound through the safe `nix` scheduler adapter. Physical
representatives are distributed across visible NUMA nodes before sibling SMT
threads are admitted. Incomplete Linux topology and other platforms retain an
explicitly `unbound` curve and an unsupported affinity entry; the two bindings
cannot be mixed in one recommendation.

The curve is measured from the maximum visible worker prefix down to one
worker, then serialized in canonical ascending order. Each smaller prefix is
therefore measured immediately after a larger superset exercised the same
processors. One persistent maximum-size pool retains the same bound thread and
private first-touch buffer for every point; a point dispatches only the exact
canonical worker prefix that the scheduler later consumes.

One scaling sample is one typed batch command per active worker, not one
coordinator dispatch per logical scan. Commands carry a monotonic generation
and the calibrated iteration count. Every active worker acknowledges that
generation before a shared start gate releases the prefix, performs every real
scan inside its local iteration loop, and returns one generation- and
worker-identified completion. The coordinator rejects a missing, duplicate,
stale, or disconnected response and permanently poisons that pool. The frozen
operation cap is enforced at the pool boundary. A poisoned generation wakes
workers waiting at the gate, while released workers check cancellation between
real scan iterations; teardown is therefore bounded by one scan iteration per
worker rather than the remaining batch. Timing starts before dispatch, ends
after the last active worker response using the monotonic-clock checked
difference, and is divided once by the batch iteration count. It therefore
includes bounded dispatch and rendezvous overhead and the slowest worker,
while avoiding thousands of channel round trips inside a 225-millisecond
sample. Warmups,
convergence probes, the retained-sample median, correctness, and the
four-percent MAD limit remain the independent authority for every point.

`thread_scaling` derives a scheduler-facing decision mechanically from that
curve. It records the effective physical and logical boundaries, each measured
worker count, the physical-range peak, the SMT-range peak, and their throughput
ratio. SMT is recommended only when its best point exceeds the best physical
point by at least five percent; otherwise the lower physical-range peak wins.
Ties select the lower worker count. If any curve point is missing, incorrect,
or unstable, the recommendation is absent. The checker independently
recomputes every field from the timing cells, so a receipt cannot invent a
larger worker budget than its evidence supports.

`io_scaling` applies the same rule to storage concurrency. It selects the
smallest measured outstanding-read depth whose buffered-read throughput is
within five percent of the measured peak. This captures the saturation knee
without consuming extra queue slots for negligible gain. An incorrect or
unstable depth makes the recommendation unavailable; the governor then falls
back to one slot. The portable worker sweep is not mislabelled as `io_uring`,
IOCP, or direct asynchronous I/O.

Every cell records an explicit per-sample operation cap in addition to the
adaptive batch policy. Stateful cells use deliberately small caps. The checker
rejects a batch above its recorded cap. This bounds scratch growth and makes
synchronization semantics visible instead of amortizing them into an
uncontrolled batch.

Every timing cell uses adaptive inner batches, unrecorded warmups, and an odd
sample count. The thorough policy targets 225 milliseconds per sample. The
thread-scaling selector requires three consecutive in-window hot-state probes
and fails closed if the target cannot be reached without exhausting the
recorded cap. The retained 31-sample median is the final convergence authority;
an out-of-window median remains valid `unstable` evidence and cannot authorize
scheduling. Other
surfaces retain one bounded extrapolation so the complete Quick and Thorough
measurement plans fit their 15-second and 600-second contracts. The semantic
checker independently verifies that recorded thread-scaling batch medians
remain near the target. Receipts report integer picoseconds per operation,
min/median/max, median absolute deviation, relative MAD, relative range, and
derived byte throughput where meaningful. Integer statistics avoid
platform-dependent JSON floating-point representations.

Each affinity-bound thread-scaling worker allocates and initializes its own
private working buffer only after binding to its processor. This removes the
shared page-home and cache-directory bias of one buffer initialized by the
coordinator. First touch reduces that known bias; it is not proof that an
operating system or hypervisor preserved NUMA residency for the full sample.
Calibration and execution consume the same physical-core-first processor order:
physical cores are spread round-robin across visible NUMA nodes before any SMT
sibling is admitted. A calibrated worker-count prefix therefore names the exact
processor prefix later used by the scheduler.

Candidate outputs are digested and compared to a separately structured
reference path before timing can be selected. Vector, comparison, and memory
references use independent loops; CRC32C uses a portable bitwise Castagnoli
reference; BLAKE3 compares one-shot and chunked incremental APIs. A failed
comparison makes the cell `rejected`. A correct diagnostic cell outside the
frozen MAD or range bound is `unstable`.

Thread scaling is deliberately stricter than the general cell rule. The pool
compares every worker completion—not merely the wrapping aggregate—against the
independent per-worker reference multiplied by that batch's iteration count.
A mismatched worker, stale generation, missing response, or protocol failure
aborts calibration through `PrimitiveSetup`; no partial calibration receipt or
`rejected` thread-scaling cell is emitted. This exception prevents compensating
worker errors from manufacturing a correct aggregate and prevents incomplete
topology evidence from looking like an ordinary candidate-kernel rejection.
Only an arithmetically correct thread-scaling point can reach timing, where
convergence or variance may still leave the complete point `unstable`.

Scheduler acceptance is narrower than diagnostic stability. Every measurement
must be correct and total duration must remain inside the mode window. The
worker and topology inputs `thread-scaling-memory-scan` and any measured
`numa-memory-read` cells use the frozen relative-MAD bound because the scheduler
decision is derived from their median. `queue-depth-random-read` uses the same
robust median rule when stable, but an unstable I/O curve remains admissible
only through the existing one-slot governor fallback. Full min/max and relative
range remain in the receipt as tail-jitter evidence, but tail pauses cannot
erase an otherwise stable median curve. Other measurements are diagnostics or
candidate-kernel observations and continue to require both MAD and range
stability. Variance in `fsync`, WAL, direct I/O, or another non-topology
diagnostic does not invalidate an independently stable worker placement
decision. This preserves observed jitter instead of hiding it or using it as
unrelated scheduler authority.

`accepted_for_scheduling` is false and `selected_kernels` is empty whenever
correctness, elapsed time, or a topology input fails. When scheduling is
accepted, `selected_kernels` contains exactly the individually stable
measurements; unstable diagnostics remain in the receipt and are never
selected. The checker recomputes these relationships and rejects inconsistent
receipts.

## Honest coverage boundary

Direct I/O is fail-closed. The Linux adapter uses a safely aligned page and a
real temporary-file write before adding either timing cell. A rejected open or
aligned write records `direct-io` as unsupported with the observed reason.
Other operating systems remain explicitly unsupported until they have an
equivalent safe adapter; buffered timing is never relabelled as direct I/O.

This receipt does not claim P1 completion or G7 performance. The following
work remains unsupported or unqualified until implemented and independently
verified:

- instruction-specific safe SIMD candidates;
- multi-node bare-metal qualification of the Linux local/remote first-touch
  adapter and equivalent NUMA adapters outside Linux;
- safe direct-I/O adapters outside Linux and platform-specific asynchronous
  I/O adapters;
- processor-bound scaling outside Linux and NUMA-local production worker
  pools.

`claims` is therefore required to remain empty. Scheduler consumption and
performance publication require the remaining P1 work and qualification
evidence.
