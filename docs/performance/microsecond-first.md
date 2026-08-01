# Microsecond-first performance contract

Status: target contract; a first bounded dirty-worktree smoke observation
exists, but no target gate has passed

Hyphae's local ecosystem is designed around bounded hot paths measured in
microseconds. This is not a claim that every query, transport, or durable
commit completes in less than one millisecond.

## Separate clocks

Every benchmark and user-visible receipt must separate:

1. admission and queueing;
2. parse, bind, optimize, or prepared-plan lookup;
3. engine execution;
4. local transport and serialization;
5. WAL append and physical synchronization; and
6. result encoding or proof construction.

An aggregate latency may also be reported, but it cannot replace these
components.

## Provisional phase-1 targets

These are falsifiable design targets, not current results. They assume warm
data, bounded payloads and result counts, a pinned build, and fully disclosed
reference hardware.

| Operation | p50 target | p99 target |
|---|---:|---:|
| Embedded structure point get, value at most 64 bytes | 2 us | 10 us |
| Native local-protocol structure point get | 25 us | 100 us |
| Embedded prepared SQL primary-key read | 5 us | 25 us |
| Native local-protocol prepared SQL primary-key read | 35 us | 150 us |
| Indexed SQL query returning at most 100 rows | 50 us | 250 us |
| Two-index join returning at most 10 rows | 75 us | 400 us |
| BM25 top 10 over 1 million hot documents | 100 us | 500 us |
| Filtered BM25 top 10 over 1 million hot documents | 200 us | 750 us |
| HNSW top 10 over 1 million 384-dimensional vectors at recall >= 0.95 | 250 us | 900 us |
| Hybrid lexical/vector top 10 over the same bounded corpus | 400 us | 950 us |

Strict durable group commit on a disclosed NVMe device has a research target
of p50 250 us and p99 900 us. It is hardware-dependent and is not a portable
product guarantee.

## Hot-path invariants

- No TCP, HTTP, JSON, or compatibility-protocol hop exists between engines.
- Prepared point operations perform no heap allocation after request decode.
- General SQL and search execution use one request arena rather than
  unbounded per-row allocation.
- Indexed point and range reads do not fall back to a full logical scan.
- Readers do not acquire a global engine mutex.
- Background compaction, merge, expiry, statistics, snapshot, and backup work
  has independent admission and CPU/I/O budgets.
- The benchmark must include saturation and background interference, not only
  an idle-process happy path.

## Work that has no universal microsecond promise

- physical fsync or storage flush;
- cold page or blob reads;
- unbounded scans, joins, sorting, aggregation, or spill;
- large result serialization;
- broad lexical candidate sets;
- ANN workloads outside the declared corpus, dimension, recall, and memory
  envelope;
- checkpointing, compaction, backup, restore, or proof reexecution; and
- requests waiting behind an explicitly saturated admission queue.

Those paths still require budgets, progress reporting, cancellation, and
tail-latency evidence.

## Measurement protocol

An accepted receipt records:

- exact commit, compiler, profile, target triple, operating system, CPU,
  topology, RAM, storage device and filesystem;
- CPU governor, affinity, process priority, background services and
  virtualization status;
- dataset generator and digest, row/document/vector counts, dimensions,
  payload sizes, selectivity, result size and index state;
- durability class, warm/cold state and whether proofs are included;
- concurrency 1, 8 and 32 plus a saturation sweep;
- at least 1,000,000 hot-path observations in an HDR-style histogram;
- p50, p95, p99 and p99.9, throughput, allocations, RSS, CPU cycles, cache
  misses, page faults and bytes read/written; and
- correctness, recall, crash-recovery, and cross-engine visibility results for
  the same build.

Shared or virtualized machines may publish observations but cannot establish a
hard regression threshold. Linux and Windows both require functional lanes;
performance gates use stable, dedicated, disclosed hardware.

## Current baseline

The first native convergence slice produced a dirty-worktree
[Windows microsecond smoke observation](../gates/evidence/native-microsecond-smoke-windows.json)
and a clean-commit
[WSL2 repeat](../gates/evidence/native-microsecond-smoke-wsl2.json). They used
one million warm observations with 32 calls averaged per timer sample and
reported sub-microsecond batch averages for embedded structure get,
allocation-free prepared primary-key SQL, and local-frame codec plus embedded
dispatch. Neither is a passing receipt: the corpus is tiny, concurrency is
one, scheduling is uncontrolled, hardware/allocation counters and real
named-pipe/UDS transport are absent, and the clean run is virtualized.

The checked `0.2.0` WSL2 evidence is a correctness baseline, not evidence for
this target. Its 10,000-document, 128-dimensional scenario reports p50
latencies of approximately 22.9 ms exact, 83.4 ms lexical, and 113.9 ms hybrid
in the checked
[benchmark receipt](../gates/evidence/0.2-retrieval-benchmark-wsl2-x86_64.json).
The native architecture must replace full scans and global serialization
before any microsecond claim is credible.
