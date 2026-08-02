# Native ordered B+tree batch copy-on-write evidence

Date: 2026-08-02

Status: measured optimization; G1, G3, and G7 remain open

Measured source commit:
`018e3394231f78e9f71cbb0d518688dc0c247b5a`

Measured source tree:
`a8c9e9f96bc03dfe20fc868844d633f87d5c46ce`

Immediate latency baseline commit:
`c25b770026ef4fbaba0c30771e187b5c8c42f5e2`

Branch at measurement: `codex/native-btree-batch-cow`

## Change

The native B+tree now accepts one strictly increasing, duplicate-free mutation
batch. It validates the complete batch before appending a page, reaches each
affected existing node at most once, merges each affected leaf once, rebuilds
each affected internal level once, and preserves the exact page IDs of
unaffected subtrees. The operation returns the new root and exact appended-page
count.

`HYSTRBT2` scalar-expiry cleanup now validates all due scalar and expiry-marker
pairs, sorts the resulting physical tombstones, rejects duplicate physical
keys, and applies the complete cleanup through that ordered batch primitive.
The global CSN, WAL, root publication, durability, MVCC,
first-committer-wins, and crash-recovery authorities are unchanged.

## Correctness evidence

The B+tree suite passed 15 tests. The batch path is equivalent to sequential
upsert across 256 ordered updates, retains the old root, grows a balanced
multi-level tree from 4,096 entries, preserves unaffected root-child page IDs,
and rejects duplicate, reversed, or oversized input before any page append.

The runtime suite passed all 125 tests with every feature enabled. It includes
the seven cleanup commit interruption boundaries, renewal conflict behavior,
expiry-index corruption rejection, the complete-state-load tripwire, and
pre-write rejection for stale logical time and duplicate physical cleanup
work. Both crates passed Clippy over all targets and features with warnings
denied.

The controlled red corpus seeded 512 keys and cleaned 64 keys from a
height-two tree. Sequential publication appended exactly 256 pages, or four
pages per key. The accepted test requires the batch implementation to append
fewer than 64 pages on that same corpus. This is a bounded regression gate, not
a substitute for the measured release observation below.

## Latency comparison

The [machine-readable receipt](native-btree-batch-cow-wsl2.json) uses the same
datasets, durability classes, batch sizes, machine, release profile,
warm-state conditions, and concurrency as the immediate physical-fast-path
baseline. Compilation is outside the timed region.

| Route | Fast-path baseline p50 | Batch COW p50 | p50 change | Baseline throughput | Batch COW throughput | Change |
|---|---:|---:|---:|---:|---:|---:|
| Empty ordered due scan | 15.979 us | 14.712 us | -7.929% | 58,860 scans/s | 64,081 scans/s | +8.870% |
| 64-key `Memory` cleanup | 15.101 ms | 2.940 ms | -80.533% | 4,182 keys/s | 20,806 keys/s | +397.453% |
| 16-key `Strict` cleanup | 4.282 ms | 870.389 us | -79.674% | 3,684 keys/s | 17,433 keys/s | +373.196% |

Memory cleanup p95 and p99 improved by 78.255% and 76.289%. Strict cleanup
p95 and p99 improved by 75.680% and 74.833%. The empty scan does not enter the
write path; its movement is treated as run noise rather than an optimization
claim.

The `Memory` route remains a millisecond operation. The `Strict` route reached
a sub-millisecond p50 but retained a 1.229 ms p99. These observations do not
establish a universal microsecond latency gate.

## Physical amplification

| Route | Keys | Batches | Pages appended | Bytes appended | Pages/key | Pages/batch |
|---|---:|---:|---:|---:|---:|---:|
| 64-key `Memory` cleanup | 4,096 | 64 | 222 | 3,637,248 | 0.054199 | 3.468750 |
| 16-key `Strict` cleanup | 512 | 32 | 98 | 1,605,632 | 0.191406 | 3.062500 |

The earlier benchmark schema did not record physical page counts, so this
receipt does not claim an exact before/after page delta for the benchmark
datasets. The exact four-pages-per-key red observation belongs to the separate
controlled 512-key regression corpus.

## Remaining boundary

Cleanup tombstones still accumulate because native compaction/vacuum is not
implemented. Normal non-expiry mutations still use sequential publication,
and the batch primitive is not yet wired into general relational, structure,
or search mutation paths. Background scheduling, cold-state behavior,
concurrency and saturation, background interference, p99.9 stability,
allocation receipts, and hardware counters remain unproven. This evidence
removes complete-state reconstruction and per-key cleanup copy-on-write as the
two measured expiry bottlenecks; it does not close G1, G3, or G7.
