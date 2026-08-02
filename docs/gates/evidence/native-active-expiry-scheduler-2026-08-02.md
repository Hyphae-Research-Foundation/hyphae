# Native active-expiry scheduler evidence — 2026-08-02

## Scope

This evidence covers the optional engine-owned scalar-expiry worker described
by [Native active-expiry scheduler v1](../../native/active-expiry-scheduler-v1.md).
It proves bounded physical cleanup inside the existing single commit scheduler.
Logical `GET` and `TTL` correctness remains independent from this worker.

The implementation and benchmark source is commit
`cdc54068014f8f1e0dcd025715653622f2c0a8b5`, tree
`32c13048069578cdb55278e747e75c62da764ca6`, built with Rust
`1.96.0`.

## Implemented behavior

- Active expiry is disabled by default and accepts only validated interval,
  key-batch, singleton-durability, and foreground-budget bounds.
- The existing scheduler worker owns both foreground commits and expiry
  maintenance. No second database handle, writer thread, timer store, cache, or
  service was added.
- The clock is injectable in tests; the system implementation uses saturated
  Unix-epoch microseconds. Scheduler samples are clamped to one non-decreasing
  logical watermark.
- A non-empty sweep uses the normal structure transaction and consumes exactly
  one transaction ID and one global CSN. An empty sweep consumes neither.
- Idle deadlines trigger cleanup without foreground traffic. Continuous
  foreground traffic is bounded by a request-count budget after the deadline.
- Memory and strict sweeps share the same commit path. Group durability is
  rejected because maintenance has no caller cohort.
- Sweep failure is retained as a typed terminal cause, increments diagnostics,
  stops admission, disconnects queued work, and requires reopen.
- Shutdown drains commands before its FIFO marker and never starts a scheduled
  sweep after consuming that marker.

No persistent byte format changed.

## Red, failure, and recovery evidence

The contract and tests were committed before the API. The red test build failed
with the expected missing active-expiry configuration, clock, stats, and
scheduler methods.

The green native-runtime suite contains 172 tests. Active-expiry coverage
proves:

- logical absence before physical cleanup;
- idle cleanup, one-CSN publication, empty no-op behavior, clock regression,
  and reopen;
- 12 queued group requests with an exact foreground budget of four while four
  strict expiry transactions drain eight due keys;
- retained `WalAppended` failure, stopped admission, typed shutdown failure,
  and successful reopen; and
- a due timer plus queued foreground commit followed by a shutdown marker,
  where the next transaction remains ID/CSN 3 and therefore proves no
  post-marker sweep occurred.

The existing `every_expiry_cleanup_boundary_recovers_prior_or_complete_index`
matrix injects all seven normal commit boundaries into the same
`expire_due_structures_at` operation used by the scheduler. Recovery observes
only the prior CSN or the complete cleanup CSN. The scheduler-specific failure
test additionally carries one of those injected failures through its terminal
admission and reopen path.

Strict clippy validation passed for the runtime and benchmark with warnings
denied.

## Enabled-versus-disabled observation

Both scenarios begin with 512 logically due scalar keys. Four synchronized
group producers then submit 64 disjoint commits each. The enabled scenario
uses 100-microsecond intervals, 64-key memory sweeps, and a 16-request
foreground budget. A final strict fence and reopen verify all foreground keys.
The enabled scenario publishes eight cleanup commits, so its fence is CSN 266;
the disabled fence is CSN 258.

| Observation | Windows release, NTFS disabled | Windows enabled | WSL2 release, tmpfs disabled | WSL2 enabled |
|---|---:|---:|---:|---:|
| Foreground throughput | 256.280/s | 256.562/s | 1,419.723/s | 1,546.682/s |
| End-to-end p50 | 14,190,100 ns | 14,383,800 ns | 2,194,048 ns | 1,679,265 ns |
| End-to-end p95 | 15,559,600 ns | 15,521,200 ns | 2,642,294 ns | 4,502,609 ns |
| End-to-end p99 | 20,359,500 ns | 15,967,100 ns | 2,927,520 ns | 5,054,276 ns |
| Queue-wait p50 | 9,344,100 ns | 10,754,600 ns | 310,327 ns | 264,660 ns |
| Cleanup throughput | — | 513.125 keys/s | — | 3,093.357 keys/s |
| Latest observed sweep | — | 129,700 ns | — | 93,276 ns |
| Maximum foreground after due | — | 4 | — | 0 |

This single disabled-then-enabled run is an observation, not a causal
performance claim. Windows foreground throughput changed by +0.110% and p50
by +1.365%. WSL2 throughput changed by +8.943% and p50 by -23.463%, while p95
and p99 increased by 70.405% and 72.647%. The mixed direction and run order
require repeated randomized-order measurements before claiming overhead or
improvement.

The result does establish the remaining performance gap. WSL2 enabled p50 is
still 1.679 milliseconds, not microseconds. It also exposes timer churn:
57 of 65 Windows attempts and 119 of 127 WSL2 attempts were empty. Adaptive
idle backoff and broader maintenance scheduling are measured follow-up work,
not behavior silently added to this contract.

Raw observations:

- [Windows](native-active-expiry-scheduler-windows.json)
- [WSL2](native-active-expiry-scheduler-wsl2.json)

## Environment boundaries

Windows ran the optimized executable successfully on NTFS. The Application
Control `os error 4551` that blocked the earlier mixed-scheduler release
executable did not recur here; this observation does not identify why that
machine policy changed.

WSL2 ran Linux `6.18.33.1-microsoft-standard-WSL2`. Source remained on
`/mnt/e`, while the Cargo target and benchmark data lived under `/tmp`.
Consequently its synchronization and cleanup timings are tmpfs observations,
not persistent-filesystem or power-loss durability evidence.

## Remaining work

- repeated randomized-order interference measurements and p99.9;
- adaptive empty-sweep backoff with a new contract;
- a unified policy for compaction, vacuum, checkpoint, WAL retention, and blob
  collection under the same scheduler;
- native-ext4 and power-loss evidence; and
- the complete G1, G3, and G7 exit matrices.

