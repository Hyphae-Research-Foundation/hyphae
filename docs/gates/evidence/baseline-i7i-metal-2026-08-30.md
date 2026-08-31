<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — dedicated-hardware baselines and commit-protocol model checking

- Date: 2026-08-30
- Host: AWS `i7i.metal-24xl` (bare metal, Intel Xeon Platinum 8559C, 96
  logical CPUs, 755 GiB RAM, local instance-store NVMe ext4 `noatime`,
  `performance` governor, `/proc/cpuinfo` hypervisor flag absent), Ubuntu
  24.04, kernel `7.0.0-1011-aws`, rustc 1.96.0
- Source: `main` @ `8aeb6ea` plus the uncommitted
  [`benchmarks/baseline-harness`](../../../benchmarks/baseline-harness/README.md)
  workspace (recorded as `8aeb6ea-plus-harness`)
- Raw receipts (verbatim harness output, no hand edits):
  [`sql`](baseline-i7i-metal-sql-2026-08-30.json),
  [`keyspace`](baseline-i7i-metal-keyspace-2026-08-30.json),
  [`lexical`](baseline-i7i-metal-lexical-2026-08-30.json),
  [`ablation`](baseline-i7i-metal-ablation-2026-08-30.json)
- Baselines: SQLite 3.50.2 (`rusqlite` 0.37 bundled, WAL,
  `synchronous=FULL`), DuckDB v1.5.5 (`duckdb` 1.4.1 bundled, default
  durable WAL), Redis 7.0.15 (UDS only, `appendonly yes` with
  `appendfsync always` and `appendfsync everysec` servers), Tantivy 0.25
  (default BM25, 256 MiB writer heap)
- Workloads: deterministic seeded xorshift64*, byte-identical rows,
  keys, documents, and query strings across engines; exclusive
  per-operation latency; skewed key distribution

This is environment class 3 ("dedicated hardware") under the
[canonical claims policy](../../product/claims.md). Numbers below are
observations of this host, this build, and these workloads — not universal
rankings.

## 1. Commit-protocol model checking (TLC)

`docs/formal/HyphaeCommit.tla` with `HyphaeCommit.cfg` (3 transactions,
2 keys, 3 engines, ≤2 crashes), TLC2 2.19, 96 workers, `-deadlock`:

```text
79,063,806 states generated, 43,885,299 distinct states, depth 30
Model checking completed. No error has been found.  (1min 41s)
```

All six invariants held over the complete bounded state space: `TypeOk`,
`Atomicity` (no partial cross-engine commit in any reachable state,
including all crash/recovery shapes), `StrictDurability` (an acknowledged
Strict commit survives every crash), `FirstCommitterWins`,
`VisiblePrefixComplete`, `CsnBounded`. The model checks the protocol as
specified; implementation fidelity remains carried by the physical crash
matrices (`tests/all_engine_transaction_g5.rs`).

## 2. SQL point workload — 1M rows (Hyphae vs SQLite vs DuckDB)

1,000,000 rows, 200,000 skewed prepared point SELECTs, 10,000 strict
single-row UPDATE commits, 100,000 UPDATEs in groups of 100.

| Phase (p50 / p99) | Hyphae native | SQLite | DuckDB |
|---|---|---|---|
| Point SELECT prepared | 30.8 µs / 33.3 µs | **1.8 µs / 2.1 µs** | 190 µs / 232 µs |
| UPDATE, fsync per commit | 1.82 ms / 2.51 ms | **22 µs / 398 µs** | 922 µs / 1.39 ms |
| UPDATE, batched ×100 | 29.4 ms / 30.0 ms | **735 µs / 4.4 ms** | 23.8 ms / 31.1 ms |
| Load 1,000-row batches (ops/s of batches) | 4.3/s | **967/s** | 13.5/s |

Read checksums matched across all three engines (200,000 hits each).

Honest reading: SQLite's B-tree point path is roughly 17× faster than
Hyphae's current read path and its strict commit is dominated by a much
smaller write set (Hyphae's strict single-row commit carries page COW +
WAL block + fsync ×2: `wal_sync` p50 657 µs + `page_sync` p50 650 µs per
the ablation receipt below). Hyphae's prepared point read (30.8 µs p50 on
this host) beats DuckDB's OLTP-shape point path by ~6× but is not
competitive with SQLite on raw point latency at this stage. These numbers
are the honest cost of copy-on-write page publication plus per-commit
manifest embedding, and they define the optimization target.

## 3. Keyspace point workload — 1M keys (Hyphae embedded vs Redis UDS)

500,000 skewed GETs; 10,000 strict SETs; 200,000 relaxed SETs.

| Phase (p50 / p99) | Hyphae embedded | Redis UDS `always` | Redis UDS `everysec` |
|---|---|---|---|
| GET | 13.5 µs / 16.0 µs | 8.1 µs / 9.1 µs | 8.2 µs / 9.2 µs |
| SET, fsync-per-write vs Strict | 1.82 ms / 2.50 ms | **509 µs / 960 µs** | — |
| SET, no-fsync-ack vs `everysec` | 376 µs / 414 µs | — | **9.1 µs / 11.1 µs** |

Honest reading: embedded Hyphae GET (no transport at all) currently loses
to Redis GET *including* its UDS round trip (13.5 µs vs 8.1 µs p50):
Hyphae's read path re-resolves the current root and B+tree descent per
call. Redis `everysec` acknowledges from memory (9 µs) while Hyphae
`Memory` still builds and publishes a full copy-on-write root set per
commit (376 µs) — that gap is root-publication cost, not fsync. Strict
vs `always` differs 3.6×, consistent with Hyphae paying two fsyncs
(pages + WAL) against Redis's single AOF fsync.

## 4. Lexical BM25 — 100k documents (Hyphae vs Tantivy)

100,000 synthetic documents (~60 tokens, 50k-term skewed vocabulary),
10,000 two-term top-10 queries, identical corpus and query bytes.

| Phase (p50 / p99) | Hyphae native | Tantivy |
|---|---|---|
| Query top-10 | 4.09 ms / 6.55 ms | **76 µs / 81 µs** |
| Ingest 1,000-doc durable batch | 16.6 s / 17.9 s | **28 ms / 47 ms** |

Hit totals matched (100,000 vs 100,000).

Honest reading: Tantivy's segmented, positional, compressed inverted index
outclasses Hyphae's B+tree posting layout by ~50× on query latency and
~580× on ingest at this scale on this host. Hyphae's lexical engine is
transactionally integrated (same-CSN visibility with SQL/structures, which
Tantivy does not attempt), but the physical index layout is not yet
competitive as a standalone search engine. A separate observation from the
aborted first run: a 1M-document strict-batched ingest wrote 2.87 TB of
copy-on-write pages before being stopped — posting-page write
amplification is the dominant ingest cost and is now a named optimization
target.

## 5. Hyphae-only ablations (10,000 commits per phase)

Durability (identical single-SET commits):

| Class | p50 end-to-end | Receipt clocks (p50) |
|---|---|---|
| Strict | 4.98 ms | execution 1.69 ms, wal_append 64 µs, page_sync 650 µs, wal_sync 657 µs |
| Group (8 producers) | 16.1 ms | shared-cohort fsync amortized, throughput 451/s vs strict 198/s (2.3×) |
| Memory | 3.62 ms | execution 410 µs, wal_append 50 µs, zero sync |

Transaction shape (1 INSERT + 1 SET + 1 indexed doc, Memory durability,
2,000 commits):

| Path | p50 | p99 |
|---|---|---|
| Materialized batch (`begin_optimistic`) | 55.1 ms | 101 ms |
| **Delta batch (`begin_optimistic_delta`)** | **1.13 ms** | **1.26 ms** |

The delta path is ~49× faster at p50 and does not grow with database
state; the materialized arm grows with accumulated state (it re-loads
full state per begin). This quantifies on dedicated hardware the
motivation recorded in
[delta-all-engine-transaction-v1](../../native/delta-all-engine-transaction-v1.md).

Engine composition (delta staging, Memory durability):

| Staged engines | p50 | marginal cost |
|---|---|---|
| SQL only | 205 µs | — |
| SQL + structure | 444 µs | +239 µs |
| SQL + structure + search | 1.13 ms | +690 µs |

Root-set construction scales roughly linearly with the number of engines
staged; the search engine's posting/document page writes are the largest
marginal contributor.

## Scope and non-claims

- Single host, single run per phase, concurrency 1 except the group-commit
  phase; no cross-host replication of these observations yet.
- Baselines ran their documented durable defaults, not hand-tuned configs;
  Hyphae ran its shipped defaults (1,024-frame buffer pool).
- The first SQL/keyspace/lexical run used the materialized write path and
  was replaced by the delta path after the shape ablation exposed the
  asymmetry; the receipts above are the delta-path run. The materialized
  observations survive inside the ablation receipt.
- No claim of universal performance superiority is made or implied; the
  competitive read of this receipt is that Hyphae's differentiator is
  cross-engine transactional integration (one CSN across SQL, structures,
  search — see the atomicity model above), while its per-engine physical
  hot paths remain measurably behind the specialized baselines on this
  host.
