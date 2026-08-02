# Native structure reachability-compaction evidence

Date: 2026-08-02

Status: measured current-root compaction; physical file vacuum, G1, G3, and G7
remain open

Measured source commit:
`72039da331c39d54854bf07ca53a3cddcae654b9`

Measured source tree:
`07884149183fe816c0bdac6a71e151e419a9d1c7`

Branch at measurement: `codex/native-structure-compaction`

## Change

`HYSTRBT2` now has an explicit structure-maintenance operation. It validates
the complete current tree and cross-entry state before appending pages, drops
only canonical scalar, collection, and expiry-index tombstones, and rebuilds a
fresh balanced B+tree from the retained ordered entries.

The operation uses one exact empty `COMPACT STRUCTURE=28` WAL mutation, advances
one global CSN only when work exists, retains the catalog, relational, and
search roots exactly, and preserves the prior structure root for historical
readers. A root with no tombstones appends no page, writes no WAL transaction,
and advances no CSN.

This is current-root reachability compaction, not page-file vacuum.
`pages.hydb` remains append-only and keeps pages owned by historical roots and
manifests.

## Correctness evidence

The runtime passed all 130 tests over all targets and features. Focused
coverage proves:

- byte-for-byte retention of every live scalar, hash, set, list, sorted-set,
  and expiry entry;
- identical materialized state before and after compaction;
- prior-root readability and exact retention of the other three engine roots;
- no-op behavior for an empty/currently clean root and typed rejection of
  legacy formats without writes;
- corruption rejection before the first compaction page append;
- strict reopen with no remaining reachable tombstones; and
- recovery to either the complete prior root or complete compacted root at all
  seven commit interruption boundaries.

The B+tree passed all 15 tests, including exact reachable-page accounting.
Both affected crates passed Clippy over all targets and features with warnings
denied.

## Measured scan effect

The [machine-readable receipt](native-structure-compaction-wsl2.json) used
2,048 cleaned expired scalars plus 2,048 live scalars. Each expired scalar
contributed one scalar tombstone and one expiry-index tombstone. The empty due
scan was measured 1,000 times before and after compaction on the same warm,
single-thread database.

| Empty due scan | Before | After | Change |
|---|---:|---:|---:|
| p50 | 124.945 us | 8.580 us | -93.133% |
| p95 | 151.930 us | 9.280 us | -93.892% |
| p99 | 282.599 us | 18.930 us | -93.301% |
| Throughput | 7,548 scans/s | 112,611 scans/s | +1,391.945% |

The scan result was empty in both cases and neither route wrote a transaction.
The difference isolates traversal of reachable expiry tombstones on this
corpus.

## Compaction observations

| Route | Latency | Scanned | Dropped | Reachable pages | Pages appended | Page-file growth |
|---|---:|---:|---:|---:|---:|---:|
| `Memory`, 2,048 expired + 2,048 live | 5.620 ms | 6,145 | 4,096 | 41 -> 10 | 10 | 163,840 B |
| `Strict`, 256 expired + 256 live | 860.286 us | 769 | 512 | 6 -> 3 | 3 | 49,152 B |

The `Memory` active root lost 75.610% of its reachable node pages; the `Strict`
root lost 50%. The strict result survived close/reopen and the next compaction
was a no-op.

The two compaction latencies are not a durability-class comparison because the
memory corpus is eight times larger. They are single maintenance operations,
not percentile distributions or a universal microsecond gate.

## Remaining boundary

The active roots became smaller, but the append-only page file grew by exactly
the new page count in both observations. Physical reclamation still needs a
retention floor, complete cross-engine page/blob reachability, a new file
generation, atomic generation publication, crash-safe rollback, and manifest
and WAL retirement rules.

Compaction is explicit rather than engine-scheduled. Cold-state behavior,
concurrent writers, saturation, background interference, p99.9 stability,
allocation receipts, and hardware counters remain unproven. This evidence
advances G1/G3 performance work but closes neither G1, G3, nor G7.
