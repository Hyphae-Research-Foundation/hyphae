# Native scalar-expiry physical fast-path evidence

Date: 2026-08-01

Status: measured optimization; G1, G3, and G7 remain open

Measured source commit:
`c25b770026ef4fbaba0c30771e187b5c8c42f5e2`

Measured source tree:
`596b5319157cb502b0a8cf570e33ea753c36ac17`

Baseline source commit:
`8f6fc731f066fe18d7b45fa68ac677595abe6169`

Branch at measurement: `codex/native-expiry-fast-path`

## Change

`HYSTRBT2` cleanup now turns the already validated ordered due-key scan into a
structure-only physical delete batch. It no longer reconstructs the catalog,
relational state, lexical-search state, ANN generation, or complete structure
state before publishing expiry tombstones.

The physical batch is private to the deterministic cleanup path. Commit
admission requires all four prior engine roots, exactly one dirty structure
slot, the `HYSTRBT2` format, an empty materialized state, and only canonical
scalar-delete mutations. Any other shape fails closed. Whole-state and
`HYSTRBT1` directories retain the materialized compatibility path.

The optimization does not change the global CSN, WAL, durability, MVCC,
first-committer-wins, or crash-recovery authorities.

## Correctness evidence

A test-only tripwire makes every complete native-state load fail. The
`HYSTRBT2` cleanup succeeds while that tripwire is active, commits exactly one
due key, and leaves the scalar physically absent.

The focused WSL2 runtime suite passed all 123 tests. This includes legacy
cleanup compatibility, stale-cleanup versus renewal conflict, expiry-index
corruption rejection, signed timestamp ordering, and all seven cleanup commit
interruption boundaries. The runtime also passed Clippy over all targets and
features with warnings denied.

## Before and after

The [machine-readable receipt](native-expiry-fast-path-wsl2.json) used the
same benchmark harness, datasets, durability classes, batch sizes, machine,
release profile, warm-state conditions, and concurrency as the baseline
[durable-expiry receipt](native-expiry-wsl2.json). Compilation was outside
the timed region.

| Route | Baseline p50 | Fast-path p50 | p50 change | Baseline throughput | Fast-path throughput | Change |
|---|---:|---:|---:|---:|---:|---:|
| Empty ordered due scan | 15.890 us | 15.979 us | +0.560% | 59,271 scans/s | 58,860 scans/s | -0.695% |
| 64-key `Memory` cleanup | 16.635 ms | 15.101 ms | -9.223% | 3,798 keys/s | 4,182 keys/s | +10.134% |
| 16-key `Strict` cleanup | 4.840 ms | 4.282 ms | -11.521% | 3,234 keys/s | 3,684 keys/s | +13.932% |

Memory cleanup p95 and p99 improved by 11.712% and 11.444%. Strict cleanup
p95 and p99 improved by 11.712% and 11.124%. The empty scan was not expected
to improve because it never entered write preparation; its sub-percent
movement is treated as run noise, not a regression claim.

## Remaining boundary

Cleanup batches remain millisecond operations. This measurement isolates
complete-state materialization as a removed cost, but it also shows that
per-key copy-on-write publication and retained expiry tombstones now dominate.
The next native work must measure and reduce that physical write
amplification, then add compaction/vacuum evidence. This observation does not
close G1, G3, or G7.
