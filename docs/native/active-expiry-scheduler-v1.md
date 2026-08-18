<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native active-expiry scheduler v1

Status: implemented; evidence recorded in
[Native active-expiry scheduler evidence — 2026-08-02](../gates/evidence/native-active-expiry-scheduler-2026-08-02.md)
and extended to mixed scalar/hash cleanup in
[Native whole-hash TTL evidence on Linux — 2026-08-03](../gates/evidence/native-hash-ttl-linux-2026-08-03.md),
hash-field cleanup in
[Native hash field TTL evidence on Linux — 2026-08-03](../gates/evidence/native-hash-field-ttl-linux-2026-08-03.md),
whole-set cleanup in
[Native whole-set TTL evidence on Linux — 2026-08-03](../gates/evidence/native-set-ttl-linux-2026-08-03.md),
and whole-list cleanup in
[Native whole-list TTL evidence on Linux — 2026-08-03](../gates/evidence/native-list-ttl-linux-2026-08-03.md).

Active expiry is physical structure maintenance owned by the native commit
scheduler. It removes due scalar values, whole hashes, whole sets, whole
lists, and independently expiring hash fields through typed native expiry
indexes and one normal WAL/MVCC transaction without introducing another
writer, timer database, cache, or external service.

This contract extends
[Native mixed-durability scheduler v1](mixed-durability-scheduler-v1.md) and
[Native structure-engine semantics v1](structures-semantics-v1.md).

## Logical versus physical expiry

Scalar, hash, set, list, and hash-field reads evaluate stored absolute expiry
against supplied logical time. A due value or family is logically absent even
when active expiry has not yet written its tombstone.

The scheduler therefore provides bounded physical reclamation, not the
correctness authority for logical absence. Delayed, disabled, cancelled, or
failed background maintenance must never make a due value visible.

## One writer authority

The existing scheduler worker owns active expiry. No second maintenance thread
may hold or open `NativeDatabase`.

Foreground commits and expiry sweeps share one FIFO writer resource:

- a sweep cannot run inside a foreground commit or group cohort;
- a group cohort remains consecutive and cannot absorb maintenance;
- a sweep uses the current published root and the existing durable expiry
  index;
- a non-empty sweep consumes one transaction ID and one global CSN; and
- an empty sweep writes no page, blob, or WAL record and consumes neither.

Every non-empty sweep remains a structure-only transaction under the same WAL,
root publication, crash boundaries, and recovery rules as an explicit
`expire_due_structures` call.

## Configuration

Active expiry is optional and disabled by default. Its validated configuration
names:

- an interval in `100 µs..=60 s`;
- a batch limit in `1..=4096` combined due structure identities;
- `memory` or `strict` singleton durability; and
- a foreground budget in `1..=4096` submitted requests after a sweep becomes
  due.

`group` durability is rejected because background maintenance has no caller
cohort and must not weaken group ordering semantics.

Invalid configuration fails before the worker or filesystem is mutated.

## Clock authority

The scheduler receives a thread-safe clock that returns signed absolute
microseconds. The system-clock convenience implementation uses Unix epoch
microseconds and saturates at the signed domain bounds.

Tests and embedded hosts may inject a deterministic clock. One scheduler
samples its clock only when beginning a sweep and clamps samples to a
non-decreasing logical watermark. A wall-clock regression delays additional
physical cleanup but cannot resurrect an already tombstoned value.

Clock panics are outside the contract. A custom clock must be total and
non-blocking.

## Scheduling and fairness

The first deadline begins when the worker starts.

- An idle worker waits only until the earlier of a foreground command or the
  next expiry deadline.
- At a due deadline with no ready foreground command, it runs one sweep.
- Under continuous foreground load, it may serve at most the configured
  foreground budget after the deadline before running one sweep.
- One group request counts as one submitted request even when several share a
  cohort.
- After any attempted sweep, the next deadline is one complete interval after
  the attempt finishes; missed intervals do not cause a catch-up storm.
- `more_due=true` does not bypass the interval or foreground budget.

These rules bound maintenance starvation without claiming a foreground latency
SLO. Benchmarks must measure interference with active expiry both enabled and
disabled.

## Failure and shutdown

An invalid limit or durability fails before worker start. A page, B+tree,
expiry-index, WAL, synchronization, or publication failure during a sweep:

1. increments the observable failure count;
2. stops new admission;
3. makes queued and future clients unavailable; and
4. requires reopening the database before further work.

Errors are never swallowed or retried on the same handle.

Shutdown stops admission, drains live foreground commands ordered before the
shutdown marker, and performs no new scheduled sweep after consuming that
marker. It then joins the worker and closes the database.

## Observation

The scheduler exposes a lock-free snapshot containing:

- attempted sweeps;
- non-empty committed sweeps;
- expired scalar, hash, set, list, or hash-field identities;
- empty sweeps;
- failures;
- the latest sampled logical time;
- the latest sweep duration; and
- the maximum foreground requests observed after a due deadline.

Counters are diagnostic evidence, not durable state. Reopen reconstructs
expiry authority from roots and WAL rather than these counters.

## Verification

Executable evidence must cover:

- idle active expiry without a foreground trigger;
- logical scalar, whole-hash, whole-set, whole-list, and hash-field absence
  before the physical sweep;
- one non-empty sweep consuming exactly one transaction ID and CSN;
- an empty sweep consuming neither;
- deterministic mixed-family batch bounds and `more_due`;
- a regressing injected clock and its non-decreasing watermark;
- continuous group load with the configured foreground bound;
- strict and memory sweep durability;
- crash recovery at every existing commit boundary;
- sweep failure making the scheduler unavailable;
- shutdown with a due timer and queued work; and
- enabled-versus-disabled foreground latency plus cleanup throughput on the
  canonical Linux ext4/EBS host.

Persistent-filesystem and power-loss claims require a persistent Linux lane;
tmpfs synchronization timings are observational only.
