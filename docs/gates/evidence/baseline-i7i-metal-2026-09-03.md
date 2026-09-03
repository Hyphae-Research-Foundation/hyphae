<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — Hyphae 3.0 re-measurement on dedicated hardware (`2ff8a4b`)

- Date: 2026-09-03
- Host: AWS `i7i.metal-24xl` (bare metal, Intel Xeon Platinum 8559C, 96
  logical CPUs, 755 GiB RAM, six 3.75 TB local instance-store NVMe disks,
  ext4 `noatime`, `performance` governor, `/proc/cpuinfo` hypervisor flag
  absent), Ubuntu 24.04, kernel `7.0.0-1011-aws`, rustc 1.96.0, region
  `us-east-2c`
- Source: `2ff8a4b995f4b08c3be1c6ff6b05d8e0b1099456` on
  `feat/sql-slice-2-and-evidence`, clean tree (`.git` synced, `git status`
  empty on the host). `run-metal.sh` ran with the disk-selection fix later
  committed as `6de0d7d`; the fix does not touch any measured code.
- Runbook: one detached script ran every block in sequence (04:15–08:27 UTC,
  instance terminated by the script after mirroring) and mirrored its
  outputs to S3 after each block; raw outputs are in
  [`metal-2026-09-03/`](metal-2026-09-03/) and the harness JSON receipts
  (verbatim, no hand edits) in [`sql`](baseline-i7i-metal-sql-2026-09-03.json),
  [`keyspace`](baseline-i7i-metal-keyspace-2026-09-03.json),
  [`lexical`](baseline-i7i-metal-lexical-2026-09-03.json),
  [`ablation`](baseline-i7i-metal-ablation-2026-09-03.json),
  [`group-commit`](baseline-i7i-metal-group-commit-2026-09-03.json)
- Baselines and workloads: as in the
  [2026-08-30 receipt](baseline-i7i-metal-2026-08-30.md) — SQLite 3.50.2
  (WAL, `synchronous=FULL`), DuckDB v1.5.5, Redis 7.0.15 (UDS, `appendfsync
  always` and `everysec`), Tantivy 0.25; deterministic seeded workloads,
  byte-identical inputs across engines
- Host proof before measuring: `cargo test -p hyphae-native-product` 252
  passed, 0 failed ([`02-product-tests.txt`](metal-2026-09-03/02-product-tests.txt))

This is environment class 3 ("dedicated hardware") under the
[canonical claims policy](../../product/claims.md), on the same host class
as the 2026-08-30 receipt, so the two are directly comparable. Unlike that
receipt, every number here is bound to a committed source SHA. Numbers are
observations of this host, this build, and these workloads — not universal
rankings.

## 1. Commit-protocol model checking (TLC), with spec digest

`docs/formal/HyphaeCommit.tla` (SHA-256
`b13cfc83454e01ad87c8eeeba74c35b71a1c3a2ebb075380a6f011c4994c9728`) with
`HyphaeCommit.cfg` (SHA-256
`7dfeb0c5c573fb898611dee0598459ded66ddfea431a1df63b72bbd1e5c02ecc`; 3
transactions, 2 keys, 3 engines, ≤2 crashes), `tla2tools.jar` SHA-256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, 96
workers, `-deadlock`:

```text
79063806 states generated, 43885299 distinct states found, 0 states left on queue.
The depth of the complete state graph search is 30.
Model checking completed. No error has been found.  (01min 45s)
```

Identical state counts to the 2026-08-30 run, which makes that run
*Reproduced* in the publication policy's sense for the model-checking claim.
All six invariants held. The model checks the protocol as specified;
implementation fidelity remains carried by the physical crash matrices.

## 2. SQL point workload — 1M rows (Hyphae vs SQLite vs DuckDB)

1,000,000 rows, 200,000 skewed prepared point SELECTs, 10,000 strict
single-row UPDATE commits, 100,000 UPDATEs in groups of 100. Read checksums
matched across all three engines (200,000 hits each).

| Phase (p50 / p99) | Hyphae native | SQLite | DuckDB | Hyphae 2026-08-30 |
|---|---|---|---|---|
| Point SELECT prepared | 34.0 µs / 36.5 µs | **1.8 µs / 2.1 µs** | 190 µs / 253 µs | 30.8 µs / 33.3 µs |
| UPDATE, fsync per commit | 1.85 ms / 2.56 ms | **22 µs / 506 µs** | 920 µs / 1.37 ms | 1.82 ms / 2.51 ms |
| UPDATE, batched ×100 | 33.7 ms / 34.2 ms | **734 µs / 4.36 ms** | 23.2 ms / 29.6 ms | 29.4 ms / 30.0 ms |
| Load 1,000-row batches (batches/s) | 3.9/s | **932/s** | 13.1/s | 4.3/s |

Honest reading: unchanged from 2026-08-30 within ±10 % — the SQL point
path was not touched by the 56 commits between the receipts, and SQLite's
B-tree point read remains ~19× faster. The +10 % on the prepared SELECT
(30.8 → 34.0 µs) is inside what two single runs on different instances of
the same class show for this workload; it is recorded, not explained.

## 3. Keyspace point workload — 1M keys (Hyphae embedded vs Redis UDS)

500,000 skewed GETs; 10,000 strict SETs; 200,000 relaxed SETs.

| Phase (p50 / p99) | Hyphae embedded | Redis UDS `always` | Redis UDS `everysec` | Hyphae 2026-08-30 |
|---|---|---|---|---|
| GET | 11.1 µs / 14.1 µs | 8.0 µs / 9.9 µs | 7.9 µs / 8.8 µs | 13.5 µs / 16.0 µs |
| SET, fsync-per-write vs Strict | 1.82 ms / 2.50 ms | **509 µs / 945 µs** | — | 1.82 ms / 2.50 ms |
| SET, no-fsync-ack vs `everysec` | 357 µs / 376 µs | — | **8.7 µs / 10.4 µs** | 376 µs / 414 µs |

Honest reading: the embedded GET improved 18 % (13.5 → 11.1 µs) and is now
within 1.4× of Redis over a UDS round trip; strict SET is byte-for-byte the
same two-fsync cost; `Memory` SET is still root publication (357 µs), not
fsync.

## 4. Lexical BM25 — 100k documents (Hyphae vs Tantivy)

100,000 synthetic documents (~60 tokens, 50k-term skewed vocabulary),
10,000 two-term top-10 queries. Hit totals matched (100,000 vs 100,000).

| Phase (p50 / p99) | Hyphae native | Tantivy | Hyphae 2026-08-30 |
|---|---|---|---|
| Query top-10 | 255 µs / 856 µs | **77.6 µs / 83.1 µs** | 4.09 ms / 6.55 ms |
| Ingest 1,000-doc durable batch | 2.19 s / 2.89 s | **29.1 ms / 43.1 ms** | 16.6 s / 17.9 s |

Honest reading: the lexical query path is **16× faster** than on 2026-08-30
(4.09 ms → 255 µs) and durable ingest **7.6× faster** (16.6 → 2.19 s per
1,000 documents) — the self-describing postings, borrowed-leaf scorer,
dictionary walk, point-resolved ingest, and chunked manifest of the
intervening commits. Tantivy still leads by 3.3× on query latency (was 50×)
and 75× on ingest (was 580×). The p99 gap (856 µs vs 83 µs) is the
buffer-pool miss path described in §8.

## 5. Hyphae-only ablations (10,000 commits per phase)

Transaction shape (1 INSERT + 1 SET + 1 indexed document, Memory
durability, 2,000 commits):

| Path | p50 | p99 | 2026-08-30 p50 |
|---|---|---|---|
| Materialized batch (`begin_optimistic`) | 65.0 ms | 127 ms | 55.1 ms |
| **Delta batch (`begin_optimistic_delta`)** | **1.10 ms** | **1.16 ms** | 1.13 ms |

Engine composition (delta staging, Memory durability):

| Staged engines | p50 | marginal | 2026-08-30 |
|---|---|---|---|
| SQL only | 218 µs | — | 205 µs |
| SQL + structure | 386 µs | +168 µs | 444 µs |
| SQL + structure + search | 1.07 ms | +684 µs | 1.13 ms |

Durability (identical single-`SET` commits on the **materialized** path,
fresh directory, no maintenance between commits):

| Class | p50 end-to-end | Receipt clocks (p50) | 2026-08-30 p50 |
|---|---|---|---|
| Strict | 35.5 ms | execution 1.84 ms, wal_append 75 µs, page_sync 683 µs, wal_sync 662 µs | 4.98 ms |
| Memory | 34.2 ms | execution 440 µs, wal_append 45 µs, zero sync | 3.62 ms |
| Group (8 producers) | 40.4 ms | throughput 170/s vs strict 31/s | 16.1 ms |

**This is a regression and it is published as one.** The receipt clocks are
unchanged from 2026-08-30 (execution, WAL append, page and WAL sync sum to
3.3 ms for Strict and 0.5 ms for Memory), so the extra ~31 ms per commit
lives outside them: in `begin_optimistic`'s complete-state load or the
receipt path, and it grows with the commits accumulated in the directory
(10,000 unretired single-key commits by the end of each phase). The delta
path, the keyspace strict SET (1.82 ms, delta path), and the engine
composition rows do not show it. The materialized path is what
`update_search_document`, `delete_search_document`, and vector-carrying
ingest batches use, so this is not a benchmark curiosity. The same-host A/B
that locates it is in §5a.

The standalone group-commit smoke (`group_commit_benchmark`, 256 commits,
8 producers, delta path): strict p50 1.67 ms at 535 commits/s; group p50
3.28 ms end-to-end at 2,141 commits/s, 4.0× the strict throughput.

### 5a. Same-host A/B of the durability ablation: engine `8aeb6ea` vs `2ff8a4b`

On the DigitalOcean `c-16` devbox (virtualized, environment class 2, relative
same-host A/B only), the same harness source built against the two engines,
`ablation --scale full`, one run each while a 1M scorer-equivalence job ran
alongside (so absolute numbers are noisy; the ratio is the point):

| Phase p50 | engine `8aeb6ea` | engine `2ff8a4b` |
|---|---|---|
| durability Strict (materialized single SET) | 14.4 ms | 86.6 ms |
| durability Memory (materialized single SET) | 9.6 ms | 81.7 ms |
| durability Group, 8 producers | 46.5 ms | 103 ms |
| shape materialized | 139 ms | 159 ms |
| shape delta | 2.91 ms | 3.03 ms |
| composition SQL / +structure / +search (delta) | 586 / 1,315 / 3,143 µs | 597 / 1,084 / 2,930 µs |

Bisected at 4,000 commits per phase (Memory p50): branch base `17a841d`
4.3 ms, `eec0784` 3.8 ms, **`93dc3d3` 35.6 ms**, `2ff8a4b` 47.6 ms. The
regression enters with `93dc3d3` ("point-resolved batch ingest and
coalesced scalar root construction"). At 500 commits per phase both engines
measure 1.35–1.39 ms, so the cost grows with what accumulates in the tree.

Cross-check that separates data from code — one engine per column opening
the *same* directories, 20 materialized `begin` each:

| directory written by | engine `eec0784` | engine `93dc3d3` |
|---|---|---|
| `eec0784` (4,000 commits) | open 11.7 s, begin p50 2.15 ms | open 11.6 s, begin p50 2.65 ms |
| `93dc3d3` (4,000 commits) | open 164 s, begin p50 32.4 ms | open 165 s, begin p50 34.5 ms |

The engine does not matter; the directory does. An instrumented load showed
the structure tree holds exactly the distinct keys (no duplicates, ~3,700
entries after 4,000 commits) in both directories, so the difference is the
**physical layout**: the coalesced path rewrites an overflowing leaf with
`append_leaf_level`, which packs the first leaf to capacity and leaves the
remainder — under random single-key inserts, one entry — in a second leaf.
Every later insert into a full leaf repeats it, so the tree converges on one
leaf per key: a `hyphae-native-btree` unit test reproducing 4,000 random
single-key batch upserts counts **904 leaves where 10 would be full**. Each
complete-state load and each open then reads and verifies ~90× the pages.
That is also why `pages.hydb` is 44 % larger for the same commits (182 MB vs
127 MB) and why the 250k/1M reopen times in §7 and in the c-16 receipt are
inflated. The delta path never loads state, which is why it stayed flat,
and the harness's bulk loads (sorted runs) pack leaves densely, which is why
the SQL, keyspace, and lexical suites did not show it.

Fix: `rewrite_node_batch` now splits an overflowing rewritten leaf into the
minimal number of pages with the encoded bytes shared evenly
(`append_leaf_level_balanced`), so a full leaf plus one key becomes two
half-full leaves; the occupancy test asserts the leaf count stays within
2× the full-packing minimum. Fix commit `b53348e`; its same-host A/B on the
devbox (full scale, one run) returns the materialized single-SET phases to
13.9 / 10.5 / 44.9 ms (Strict / Memory / Group) against 14.4 / 9.6 / 46.5 for
`8aeb6ea`, and improves the materialized shape 159 → 115 ms and the delta
shape 3.03 → 2.49 ms. A fresh 250k ladder load on the devbox at `b53348e`
also ingests 1,346 docs/s (from 1,099) and halves the durable scorer stage
(22–24 → 13.6 ms) because posting leaves are dense again. Every table in
this receipt measures `2ff8a4b`, before the fix; the reopen, materialized
path, and scorer rows are therefore pessimistic for 3.0 and the ladder
should be re-measured on this host class at `b53348e` or later.


## 6. Delta-transaction scaling sweeps (pinned to CPU 0, median of three runs)

`delta_transaction_scaling`, Memory durability, 32 observations per point,
release binary with symbols (`taskset -c 0`). Every point: 0 complete-state
loads, 0 complete-catalog loads.

| prior versions | total p50 | stage p50 | commit p50 | page reads | page appends | WAL bytes |
|---|---|---|---|---|---|---|
| 1 | 194 µs | 32.5 µs | 161 µs | 9 | 3 | 65,536 |
| 32 | 196 µs | 33.4 µs | 161 µs | 9 | 3 | 65,536 |
| 256 | 197 µs | 33.5 µs | 163 µs | 9 | 3 | 65,536 |
| 1,024 | 195 µs | 33.4 µs | 161 µs | 9 | 3 | 65,536 |

| unrelated items per engine | total p50 | stage p50 | commit p50 | page reads | page appends |
|---|---|---|---|---|---|
| 0 | 346 µs | 34.1 µs | 273 µs | 17 | 5 |
| 256 | 805 µs | 35.6 µs | 707 µs | 30 | 15 |
| 4,096 | 1,045 µs | 52.6 µs | 917 µs | 43 | 20 |

The version-depth sweep is flat to the microsecond across three orders of
magnitude of history, and the population sweep grows with B+tree height
(page reads 17 → 43), not with item count — the product invariant of
[delta-all-engine-transaction-v1](../../native/delta-all-engine-transaction-v1.md),
now on dedicated hardware (the 2026-08-03 receipt was an `m6i.2xlarge`
under KVM: 258 µs flat, 905 µs → 1.65 ms).

## 7. Collection document-cap ladder — 250k and 1M

Same synthetic corpus and harnesses as the
[c-16 receipt](collection-manifest-chunked-1m-ladder-2026-09-03.md); fresh
loads on NVMe without maintenance windows; query ladders on reopened
directories (p50, 16 samples, `limit=10`, candidate limit 1,000). The 1M
rung ran with the collection bound lifted to 1,000,000 on this host only.

| rung | ingest docs/s | ingest wall | vacuum | directory | reopen | bm25 | filtered+facet | phrase | fuzzy(1) |
|------|---------------|-------------|--------|-----------|--------|------|----------------|--------|----------|
| 250k | 4,069 | 61.4 s | 14.0 s | 236 MB | 7.7 s | 6.2 ms | 10.7 ms | 7.2 ms | 12.0 ms |
| 1M | 3,755 | 266 s | 62.9 s | 1.01 GB | 34.5 s | 51.6 ms | 71.1 ms | 52.7 ms | 89.0 ms |
| ratio (4× documents) | 0.92× | | | | 4.5× | 8.3× | 6.6× | 7.3× | 7.4× |
| c-16 (virtualized), 1M | 1,014 | 986 s | 185 s | 995 MB | 107 s | 172 ms | 233 ms | 175 ms | 308 ms |

Manifest stage (`HYPSMAN2`): 250k — 487 chunks, 9,756-byte header,
header decode 0.25 ms, full materialization 1.5 ms, one probe 2 µs; 1M —
1,952 chunks, 39,056-byte header, header decode 0.5 ms, materialization
13 ms, one probe 3 µs. Sparse query (2 hits): 0.1 ms at 250k, 0.2 ms at
1M. Durable scorer ("database engine", 1,000 hits): 5.1 ms at 250k (393
segments, 93,702 entries), 51.9 ms at 1M (1,562 segments, 372,418
entries) — 10× the time for 4× the entries. The first bm25 sample after a
fresh load remains the cold-cache outlier (p95 3.7 s at 250k, 16.8 s at
1M) and is reported, not excluded.

## 8. Where the 1M scorer time goes (`perf`, 15 s, 999 Hz, DWARF call graphs)

Self time of the diagnostic process while looping the 1M durable scorer
([full report](metal-2026-09-03/07-perf-scorer-report.txt)):

| share | symbol |
|---|---|
| 17.3 % | `_blake3_hash_many_avx512` |
| 10.7 % | `_blake3_compress_in_place_avx512` |
| 10.7 % | `crc32c::hw_x86_64::crc_u64_parallel3` |
| 5.3 % | `crc32c::hw_x86_64::crc_u64_append` |
| 6.3 % | `hyphae_native_runtime::decode_lexical_segment` |
| 6.1 % + 3.9 % | stable sort / quicksort partition (score merge) |
| 3.7 % | kernel `_copy_to_iter` (file reads) |
| 3.4 % + 3.3 % | `BorrowedLeaf::entries` / `BorrowedLeaf::decode` |
| 2.8 % | `drop_in_place<NativeRuntimeError>` |

**44 % of the scorer's time is page verification** (BLAKE3 28 %, CRC32C
16 %) and another 4 % is the kernel copying pages out of the file: the
1,562 posting segments of a two-term query at 1M do not stay resident in
the 1,024-frame verified buffer pool between queries, so every query
re-reads and re-verifies them. That is the mechanism behind the
superlinear ladder (§7) and the lexical p99 (§4). It is a residency policy
problem — frame budget, or a per-generation "verified once" cache — not a
scorer algorithm problem, and it is the next engine slice. The
`drop_in_place<NativeRuntimeError>` line is the durable scorer's fail-open
probe constructing errors on a hot path and is a cheap follow-up.

## 9. Scorer equivalence at 1M

One oracle round (`HYPHAE_DIAG_MODEL_ROUNDS=1`) on the 1M directory of §7,
after the ladder, on this host ([raw](metal-2026-09-03/09-equivalence-1m.txt)):

```text
stage=durable_scorer round=0 hits=1000 ms=57.5 terms=2 segments=1562 physical_entries=372418
stage=model_scorer   round=0 hits=1000 ms=12596531.2
stage=scorer_equivalence hits=1000 bit_identical=true
```

The retained-model scorer reproduces the durable scorer's 1,000 ranked hits
— ids, order, and score bits — at 1,000,000 documents, taking 3.5 hours to
the durable scorer's 57 ms. This extends the 100k and 250k equivalence of
the previous receipts to the 1M rung on dedicated hardware.


## Scope and non-claims

- Single host, one run per phase (three for the delta sweeps), concurrency
  1 except the group-commit phases. Same host class as 2026-08-30 but a
  different physical machine; cross-receipt deltas under ~10 % are not
  claimed as changes.
- Baselines ran their documented durable defaults; Hyphae ran its shipped
  defaults (1,024-frame buffer pool, no governor or execution pool, so the
  parallel scorer path stays at `workers=1`).
- The collection bound stays 250,000: the 1M rows are a measurement above
  the bound, and the R5 vector conditions (ANN consolidation cost, RSS at
  1M × 768-dim) remain unmeasured.
- Synthetic corpora throughout (harness workloads and the ladder corpus);
  no relevance claim is made or implied by any number here.
- The materialized-path durability regression in §5 is a finding of this
  receipt, not a closed item; the shipped hot write path (delta) is
  unaffected, and the commits that use the materialized path are named.
- No claim of universal performance superiority is made or implied; the
  competitive read is unchanged from 2026-08-30 — Hyphae's differentiator
  is cross-engine transactional integration under one CSN with a
  model-checked commit protocol, while its per-engine hot paths remain
  behind the specialized baselines on this host, now by 3–19× rather than
  17–580×.
