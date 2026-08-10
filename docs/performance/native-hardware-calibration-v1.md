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
a topology-derived thread-scaling curve, the controlled I/O-depth curve, and
two additional NUMA cells when at least two Linux nodes are process-visible:

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

On multi-node Linux, an 8 MiB fixture is allocated and first-touched by a
worker pinned to the representative source node. One pinned reader on that
node and one pinned reader on a distinct visible node then scan the identical
immutable bytes. Both cells carry canonical source-node, reader-node, and CPU
identity in their variant. The checker requires exactly one local and one
remote cell, the frozen working-set size, exact byte accounting, and one shared
first-touch node. A single-node machine or affinity failure records the NUMA
surface as explicitly unsupported instead of fabricating locality.

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
sample count. Receipts report integer picoseconds per operation, min/median/max,
median absolute deviation, relative MAD, relative range, and derived byte
throughput where meaningful. Integer statistics avoid platform-dependent JSON
floating-point representations.

Candidate outputs are digested and compared to a separately structured
reference path before timing can be selected. Vector, comparison, and memory
references use independent loops; CRC32C uses a portable bitwise Castagnoli
reference; BLAKE3 compares one-shot and chunked incremental APIs. A failed
comparison makes the cell `rejected`. A correct cell outside the frozen MAD or
range bound is `unstable`.
The complete receipt is accepted only when every measured cell is correct and
stable and total duration is inside its mode window. Otherwise
`accepted_for_scheduling` is false and `selected_kernels` is empty. The checker
recomputes these relationships and rejects inconsistent receipts.

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
