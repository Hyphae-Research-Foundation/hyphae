# Microsecond-first performance contract

Status: target contract; versioned local smoke observations exist, but no
target gate has passed

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

Later native receipts keep this method versioned as the physical corpus
changes. Schema v4 added a 2,048-key height-two scalar B+tree route. Schema v5
added one explicitly typed hash with 2,048 independent 64-byte fields and
reported both materialized and direct buffered `HGET`. Schema v6 adds 2,048
documents in a height-two native inverted index. Its rare-term physical
`MATCH` uses 100,000 single-call observations rather than a 32-call batch and
observed p50 `20.926 us` and p99 `71.596 us` in the checked
[receipt](../gates/evidence/native-microsecond-smoke-search-wsl2.json).
Cross-schema latency is not a controlled comparison, and none of these
observations passes G7.

Schema v7 adds 2,048 rows under one unique text secondary index and measures
one complete exact-key call per timer sample. The buffered physical
index-to-row route observed p50 `8.514 us` and p99 `22.144 us`; the
catalog-version-bound physical prepared-SQL route, including parameter binding,
tuple decoding, and two projected values, observed p50 `9.497 us` and p99
`27.479 us` in the checked
[receipt](../gates/evidence/native-microsecond-smoke-secondary-sql-wsl2.json).
That is inside the provisional bounded indexed-SQL 50-us p50 and 250-us p99
target for this single scenario. It is not G7: the unique one-row corpus/query
is narrow, concurrency is one, results allocate, and the required
transport/saturation/interference/cold-state/allocation/hardware-counter lanes
are absent.

Schemas v8 and v9 added bounded physical primary-key scan and range routes.
Schema v10 adds a parameterized boolean residual filter over a primary-key
range and applies `LIMIT 10` after matching. Over a height-two, 2,048-row
relation with alternating boolean values, the prepared residual route observed
p50 `15.464 us` and p99 `40.370 us`; the same-run prepared 10-row range
observed p50 `12.657 us` and p99 `32.505 us` in the checked
[receipt](../gates/evidence/native-microsecond-smoke-residual-filter-wsl2.json).
The residual route visits roughly 19 rows to return 10 matches. This is a
virtualized, warm, concurrency-one observation outside G7, not a broad SQL
latency claim.

Schema v14 replaces the measured residual range after a strict composite
primary-key equality prefix with a bounded physical route. Over a height-two,
2,048-row relation split between adjacent text prefixes, the physical prepared
prefix-plus-range query observed p50 `15.119 us`, p99 `40.127 us`, and
throughput `60,804 ops/s` in the checked
[receipt](../gates/evidence/native-microsecond-smoke-primary-prefix-range-wsl2.json).
The prior schema-v13 residual route observed p50/p99 `198.603/589.950 us` and
`4,472 ops/s`; across those exact source-bound observations, the new route
reduced p50 by `92.387%`, p99 by `93.198%`, and raised throughput `13.597`
times. This is an algorithmic-work observation, not a controlled same-process
regression threshold or a G7 pass: WSL2 is virtualized, state is warm,
concurrency is one, and the required cold-state, saturation, interference,
allocation, transport, durability, and hardware-counter lanes are absent.

Schema v15 adds the first ordered physical secondary-index range and its
deliberately expensive unindexed differential baseline in one process and
corpus. Over the same height-two, 2,048-row relation with variable-width
text keys in `[a,b)`, the physical prepared secondary range observed p50
`24.328 us`, p99 `58.527 us`, and throughput `37,844 ops/s` in the checked
[receipt](../gates/evidence/native-microsecond-smoke-secondary-range-wsl2.json),
while the unindexed baseline returned the same ten validated rows at p50
`7,971.720 us`, p99 `10,082.542 us`, and `123 ops/s`. The indexed route
reduces p50 by `99.695%` and p99 by `99.420%` against that baseline and sits
inside the provisional bounded indexed-SQL target for this single scenario.
This is an algorithmic-work observation, not a controlled same-process
regression threshold or a G7 pass: WSL2 is virtualized, state is warm,
durability is memory, concurrency is one, and the required cold-state,
saturation, interference, allocation, transport, durability, and
hardware-counter lanes are absent.

The same schema-v15 build also ran on native Linux for the first time:
an AWS EC2 devbox with the benchmark data directory on persistent ext4
rather than tmpfs, recorded in the checked
[receipt](../gates/evidence/native-microsecond-smoke-ext4-linux.json).
The physical prepared secondary range observed p50 `42.698 us` and p99
`90.215 us`, still inside the provisional bounded indexed-SQL target
for this single scenario on that hardware. The hardware differs from
the WSL2 receipts, so the numbers are a new baseline rather than a
comparison. This is not a G7 pass or a regression threshold: EC2 is
virtualized, state is warm, durability is memory, concurrency is one,
and the timed paths do not fsync.

The checked `0.2.0` WSL2 evidence is a correctness baseline, not evidence for
this target. Its 10,000-document, 128-dimensional scenario reports p50
latencies of approximately 22.9 ms exact, 83.4 ms lexical, and 113.9 ms hybrid
in the checked
[benchmark receipt](../gates/evidence/0.2-retrieval-benchmark-wsl2-x86_64.json).
The native architecture must replace full scans and global serialization
before any microsecond claim is credible.

The first
[native HNSW kernel observation](../gates/evidence/native-ann-kernel-wsl2.json)
uses 10,000 vectors, 32 dimensions, 100 independent queries and top 10 under
WSL2. With `M=16` and construction/search breadth 128, recall@10 was `0.970`.
HNSW observed p50 `716.048 us` while the exact oracle observed p50
`639.333 us`; approximation was slower at this corpus size. The graph is not
page/WAL/MVCC-backed in that source commit, and the run lacks the
one-million-vector, 384-dimensional corpus, concurrency, p99.9, allocations,
hardware counters and interference lanes. It is a quality/latency observation,
not G4 or G7.

The later
[durable ANN observation](../gates/evidence/native-ann-durability-wsl2.json)
uses the search B+tree, WAL and global MVCC root from a clean commit, then
reopens and validates the graph before measurement. Its deliberately smaller
512-vector, 32-dimensional corpus reached recall@10 `1.000`. Over 10,000 warm
calls per route on a pinned materialized snapshot, HNSW observed p50/p99
`227.227/483.309 us` and exact observed `23.032/42.020 us`. Batch seeding took
`88.642 ms`, strict commit `127.021 ms`, reopen `89.845 ms`, and snapshot
materialization `86.492 ms`; none of those setup/I/O clocks are folded into
the query latency.

This scenario is inside the provisional HNSW p50/p99 target, but it is not a
gate pass: the corpus is 1,953 times smaller than the target and has 32 rather
than 384 dimensions; execution materializes the graph instead of traversing
buffered pages; WSL2 is virtualized; concurrency is one; and saturation,
interference, allocations, RSS and hardware counters are absent. Exact search
also remains roughly ten times faster on this small corpus.
