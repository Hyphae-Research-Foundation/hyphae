# Native durable scalar-expiry evidence

Date: 2026-08-01

Status: first active scalar-expiry slice; G1, G3, and G7 remain open

Measured source commit:
`8f6fc731f066fe18d7b45fa68ac677595abe6169`

Measured source tree:
`fa8e4ac2a14dc28b18f8686ac466b7848ba4e74c`

Branch at measurement: `codex/native-expiry-scheduler`

## Change

New Hyphae directories use the `HYSTRBT2` structure format. Every expiring
scalar owns one live entry in an ordered native expiry namespace:

```text
0x0b || sign_flipped_i64_be(expires_at_micros) || scalar_key
```

The one-byte value is live or tombstone. `SET`, `EXPIRE`, `DELETE`, and
`INCRBY` maintain the scalar and derived expiry identities in one structure
mutation, WAL transaction, root publication, and CSN. No Redis, Valkey,
external scheduler, side database, network hop, JSON envelope, or second WAL
is involved.

`expire_due_structures(logical_time, max_keys, durability)` performs an
ordered current-root scan, selects at most `max_keys` due live identities,
revalidates them against deterministic time, and commits their scalar and
index tombstones atomically. The bound is `1..=4096`. An empty scan publishes
no WAL transaction and advances no CSN. The receipt reports expired keys,
whether another due live identity was observed, and the optional native
commit receipt.

## Compatibility and recovery

Existing `HYSTRT01` whole-state and `HYSTRBT1` B+tree directories keep their
original format. `HYSTRBT1` has no expiry index, so its compatibility cleanup
reconstructs due work from scalar envelopes. There is no silent on-open
rewrite.

`HYSTRBT2` recovery requires exact one-to-one agreement between every live
expiry marker and scalar envelope. Missing, stale, orphan, persistent-key,
malformed-identity, or noncanonical-marker projections fail closed. Index
tombstones are ignored logically and retained physically until a future
native compaction pass.

Optimistic cleanup writes the scalar conflict identity. A stale cleanup batch
cannot delete a concurrently renewed lease: first-committer-wins rejects it.
Historical snapshots retain their old root and logical time.

## Correctness evidence

The native runtime suite has 122 tests. New coverage proves:

- exact expiry-boundary cleanup and full signed `i64` timestamp order;
- deterministic timestamp/key tie-breaking, hard batch bounds, and
  no-commit empty scans;
- reschedule, TTL removal, scalar deletion, historical visibility, strict
  reopen, and `HYSTRBT1` compatibility;
- stale-cleanup versus renewal conflict;
- rejection of missing, stale, orphan, persistent-key, malformed, and invalid
  live expiry projections; and
- all seven commit interruption boundaries recovering either the complete
  prior index or the complete cleanup index, after which cleanup can resume.

The focused WSL2 crate suite passed all 122 tests, and the complete WSL2
workspace passed `cargo test --workspace --all-targets --all-features
--locked`. Windows compiled all tests and passed Clippy with warnings denied.
Execution of the changed Windows test binary was blocked by Application
Control (`os error 4551`); policy was not weakened. Hosted Windows CI remains
the cross-platform runtime authority.

## Latency observation

The [machine-readable WSL2 receipt](native-expiry-wsl2.json) is bound to the
clean source commit and tree above. It used WSL2 Linux
`6.18.33.1-microsoft-standard-WSL2`, an Intel Core Ultra 9 285H exposed as 16
vCPUs, Rust 1.96.0, release mode, one thread, strict seed, reopen, and a
height-two structure B+tree. Compilation is outside the timed region.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Empty ordered due scan | 15.890 us | 21.854 us | 33.785 us | 55.945 us | 625.181 us | 59,271 scans/s |
| 64-key `Memory` cleanup | 16.635 ms | 18.783 ms | 19.246 ms | 19.246 ms | 19.263 ms | 3,798 keys/s |
| 16-key `Strict` cleanup | 4.840 ms | 5.456 ms | 5.495 ms | 5.495 ms | 6.336 ms | 3,234 keys/s |

The empty route used 10,000 warmups and 100,000 observations, amortizing timer
overhead over eight complete API calls per observation. Cleanup consumed
4,096 keys in 64-key memory-durability transactions and 512 keys in 16-key
strict-durability transactions. The observed p50 batch costs correspond to
about 260 us/key and 302 us/key respectively; this is an effective ratio, not
an independently timed point operation.

The first-to-last cleanup samples rose from 16.635 ms to 19.246 ms for the
memory route and from 4.730 ms to 5.131 ms for the strict route. The current
path materializes the complete structure state at writer admission and
publishes scalar plus expiry copy-on-write paths per key. Those costs, plus
retained index tombstones, are now measured pain points rather than hidden
behind a localhost claim.

This receipt proves that an empty hot scheduler check is a microsecond
operation. It does **not** prove microsecond cleanup transactions: current
bounded batches are millisecond operations. It is one warm,
concurrency-one, single-machine observation and does not close G7.

## Remaining boundary

This slice does not complete the structure engine or G3. Remaining work
includes:

- an engine-owned background timer owner, wakeup policy, cancellation,
  telemetry, and saturation behavior around this deterministic primitive;
- compaction/vacuum for expiry and scalar tombstones, plus measured
  read/write/memory amplification;
- eliminating complete-state writer materialization and reducing per-key COW
  publication cost;
- TTL and whole-key lifecycle for hashes, sets, lists, sorted sets, streams,
  and future structure families;
- randomized model equivalence, concurrent-reader and cold-page evidence;
  and
- local protocol, CLI, SDK, SQL relation-valued access, backup/restore, and
  administration exposure.

G1, G3, and G7 remain open.
