<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Evidence — Hyphae 3.0.0 on dedicated hardware after the B+tree split fix and the buffer-pool bound (`a443c52`)

- Date: 2026-09-03 (12:03–12:42 UTC for blocks 00–08; the 1M scorer
  equivalence ran until 16:11 UTC and the script then terminated the
  instance, see §7)
- Host: AWS `i7i.metal-24xl` (bare metal, Intel Xeon Platinum 8559C, 96
  logical CPUs, 755 GiB RAM, six 3.75 TB local instance-store NVMe disks,
  ext4 `noatime`, `performance` governor, hypervisor flag absent), Ubuntu
  24.04, kernel `7.0.0-1011-aws`, rustc 1.96.0, region `us-east-2c` — the
  same host class and region as the
  [`2ff8a4b` receipt](baseline-i7i-metal-2026-09-03.md) taken eight hours
  earlier on a different physical machine
- Source: `a443c52e68a5c5c69cd62455c04ad313bc8d7dc3` on
  `feat/sql-slice-2-and-evidence` (workspace version 3.0.0), clean tree.
  Relative to `2ff8a4b` the measured code differs by `b53348e` (B+tree batch
  rewrites split an overflowing leaf evenly) and the buffer-pool bound
  (1,024 → 8,192 frames); everything else between the two SHAs is
  documentation, release configuration, and relicensing evidence.
- Runbook: the same detached script as the earlier receipt minus TLC (already
  reproduced twice) plus a same-directory buffer-pool comparison; raw
  outputs in [`metal-2026-09-03-a443c52/`](metal-2026-09-03-a443c52/) and
  verbatim harness receipts in
  [`sql`](baseline-i7i-metal-sql-2026-09-03-a443c52.json),
  [`keyspace`](baseline-i7i-metal-keyspace-2026-09-03-a443c52.json),
  [`lexical`](baseline-i7i-metal-lexical-2026-09-03-a443c52.json),
  [`ablation`](baseline-i7i-metal-ablation-2026-09-03-a443c52.json),
  [`group-commit`](baseline-i7i-metal-group-commit-2026-09-03-a443c52.json)
- Host proof: `cargo test -p hyphae-native-product` 252 passed, 0 failed
- Baselines, workloads, and durability postures: unchanged from the
  [2026-08-30 receipt](baseline-i7i-metal-2026-08-30.md)

Environment class 3 under the [claims policy](../../product/claims.md).
Three receipts on this host class now exist eight days apart (`8aeb6ea`,
`2ff8a4b`, `a443c52`); the rows below carry all three so the effect of the
two commits is read against the run-to-run noise of the class rather than
against a single earlier number.

## 1. SQL point workload — 1M rows

| Phase (p50 / p99) | `a443c52` | `2ff8a4b` | `8aeb6ea` (08-30) | SQLite | DuckDB |
|---|---|---|---|---|---|
| Point SELECT prepared | **20.3 µs / 36.3 µs** | 34.0 / 36.5 | 30.8 / 33.3 | 1.8 / 2.1 | 191 / 235 |
| UPDATE, fsync per commit | 1.75 ms / 2.43 ms | 1.85 / 2.56 | 1.82 / 2.51 | 21.9 µs / 402 µs | 915 µs / 1.38 ms |
| UPDATE, batched ×100 | 30.6 ms / 31.4 ms | 33.7 / 34.2 | 29.4 / 30.0 | 724 µs / 4.34 ms | 23.7 / 30.8 ms |
| Load 1,000-row batches | 4.0/s | 3.9/s | 4.3/s | 963/s | 13.1/s |

Read checksums matched across the three engines (200,000 hits each). The
prepared point read is 40 % faster than either earlier receipt: denser
relational leaves and a pool that keeps the hot path resident. SQLite's
point read is still 11× faster.

## 2. Keyspace point workload — 1M keys

| Phase (p50 / p99) | `a443c52` | `2ff8a4b` | `8aeb6ea` | Redis UDS `always` | Redis UDS `everysec` |
|---|---|---|---|---|---|
| GET | **2.2 µs / 16.3 µs** | 11.1 / 14.1 | 13.5 / 16.0 | 7.9 / 10.8 | 8.2 / 9.5 |
| SET, fsync-per-write vs Strict | 1.81 ms / 2.53 ms | 1.82 / 2.50 | 1.82 / 2.50 | 509 µs / 910 µs | — |
| SET, no-fsync-ack vs `everysec` | 371 µs / 404 µs | 357 / 376 | 376 / 414 | — | 9.6 µs / 11.5 µs |

The embedded GET is now 3.6× faster than Redis over its UDS round trip at
p50 (2.2 µs against 7.9 µs); at p99 (16.3 µs) it is the slower one, which
is the pool-miss tail. Strict and `Memory` SETs are unchanged: root
publication and two fsyncs, as before.

## 3. Lexical BM25 — 100k documents

| Phase (p50 / p99) | `a443c52` | `2ff8a4b` | `8aeb6ea` | Tantivy |
|---|---|---|---|---|
| Query top-10 | **111 µs / 181 µs** | 255 / 856 | 4.09 ms / 6.55 ms | 78.3 µs / 82.9 µs |
| Ingest 1,000-doc durable batch | **1.12 s / 1.73 s** | 2.19 / 2.89 | 16.6 / 17.9 | 27.0 ms / 45.7 ms |

Hit totals matched (100,000 vs 100,000). Against the 2.2.0-era receipt the
query is 37× faster and durable ingest 15× faster; Tantivy still leads by
1.4× on query latency (was 50×) and 41× on ingest (was 580×). The p99 gap
closed from 10× to 2.2×.

## 4. Hyphae-only ablations (10,000 commits per phase)

Durability, identical single-`SET` commits on the materialized path:

| Class | `a443c52` p50 | `2ff8a4b` p50 | `8aeb6ea` p50 | receipt clocks p50 (`a443c52`) |
|---|---|---|---|---|
| Strict | **4.39 ms** | 35.5 ms | 4.98 ms | execution 1.58 ms, wal_append 49 µs, page_sync 536 µs, wal_sync 589 µs |
| Memory | **3.86 ms** | 34.2 ms | 3.62 ms | execution 366 µs, wal_append 41 µs |
| Group, 8 producers | **12.4 ms** | 40.4 ms | 16.1 ms | 497 commits/s vs strict 196/s (2.5×) |

The regression published in the `2ff8a4b` receipt (§5 there) is closed on
the same host class: the materialized single-`SET` commit is back at the
2.2.0 level and slightly under it, with the receipt clocks themselves
unchanged across all three runs — the ~31 ms that lived outside the clocks
was the complete-state load reading a tree of one-entry leaves.

Transaction shape (1 INSERT + 1 SET + 1 indexed document, Memory
durability, 2,000 commits) and engine composition (delta staging):

| Row | `a443c52` | `2ff8a4b` | `8aeb6ea` |
|---|---|---|---|
| Materialized batch p50 / p99 | 46.6 ms / 95.8 ms | 65.0 / 127 | 55.1 / 101 |
| **Delta batch** p50 / p99 | **1.02 ms / 1.09 ms** | 1.10 / 1.16 | 1.13 / 1.26 |
| SQL only | 200 µs | 218 | 205 |
| SQL + structure | 371 µs | 386 | 444 |
| SQL + structure + search | 1.02 ms | 1.07 | 1.13 |

Standalone group-commit smoke (256 commits, 8 producers, delta path):
strict p50 1.54 ms at 587/s; group p50 3.14 ms at 2,263/s, 3.86× strict
throughput.

## 5. Delta-transaction scaling sweeps (CPU 0, median of three runs)

| prior versions | total p50 | stage | commit | page reads | appends | full-state loads |
|---|---|---|---|---|---|---|
| 1 | 194 µs | 32.4 µs | 161 µs | 9 | 3 | 0 |
| 32 | 193 µs | 32.4 µs | 161 µs | 9 | 3 | 0 |
| 256 | 202 µs | 38.8 µs | 163 µs | 9 | 3 | 0 |
| 1,024 | 201 µs | 38.8 µs | 162 µs | 9 | 3 | 0 |

| unrelated items per engine | total p50 | stage | commit | page reads | appends |
|---|---|---|---|---|---|
| 0 | 346 µs | 34.2 µs | 272 µs | 17 | 5 |
| 256 | 676 µs | 33.6 µs | 577 µs | 30 | 11 |
| 4,096 | 893 µs | 51.1 µs | 764 µs | 41 | 14 |

Version depth stays flat (193–202 µs across three orders of magnitude of
history); the population sweep is 16 % and 15 % cheaper than on `2ff8a4b`
at 256 and 4,096 items (805 → 676 µs, 1,045 → 893 µs) with fewer page
appends per commit (15 → 11, 20 → 14) — the even split writes fewer leaves
per rewritten path.

## 6. Collection document-cap ladder — 250k and 1M

Fresh loads on NVMe, ladders on reopened directories (p50, 16 samples,
`limit=10`, candidate limit 1,000); the 1M rung with the bound lifted on
this host only.

| rung | ingest docs/s | ingest wall | vacuum | directory | reopen | bm25 | filtered+facet | phrase | fuzzy(1) |
|------|---------------|-------------|--------|-----------|--------|------|----------------|--------|----------|
| 250k `a443c52` | 4,265 | 58.6 s | 13.8 s | 236 MB | 7.7 s | 6.2 ms | 10.6 ms | 7.3 ms | 12.0 ms |
| 250k `2ff8a4b` | 4,069 | 61.4 s | 14.0 s | 236 MB | 7.7 s | 6.2 ms | 10.7 ms | 7.2 ms | 12.0 ms |
| 1M `a443c52` | 3,638 | 275 s | 64.3 s | 1.01 GB | 34.6 s | **23.2 ms** | **42.6 ms** | **24.2 ms** | **54.2 ms** |
| 1M `2ff8a4b` | 3,755 | 266 s | 62.9 s | 1.01 GB | 34.5 s | 51.6 ms | 71.1 ms | 52.7 ms | 89.0 ms |
| ratio 250k → 1M, `a443c52` | 0.85× | | | | 4.5× | **3.7×** | 4.0× | 3.3× | 4.5× |

The 1M query ladder is now linear in the document count (3.3–4.5× for 4×
the documents, against 6.6–8.3× on `2ff8a4b`), and at 250k nothing moved:
the 1,024-frame pool already held a 250k query's segments. Reopen is
unchanged at both rungs on this host (open decodes complete state once and
validates retained roots; neither commit touches that path), so the c-16
reopen improvement recorded in the ladder receipt was the slower host's
page-read cost, not a general effect. Diagnostic stages at 1M: manifest
1,952 chunks / 39,056-byte header; durable scorer 22.3 ms (1,562 segments,
372,418 entries); integrated `MatchAll` 23.3 ms; Eq filter 31.8 ms; range +
facet 49.1 ms; fuzzy 56.2 ms. The first bm25 sample after a fresh load is
still the cold-cache outlier (p95 17 s at 1M, 3.8 s at 250k), reported and
not excluded.

### Buffer-pool bound, same host, same 1M directory

| `HYPHAE_BUFFER_POOL_FRAMES` | durable scorer (rounds 3–4) | integrated `MatchAll` | range + facet | fuzzy(1) |
|---|---|---|---|---|
| 1,024 (2.x default) | 50.8 / 51.2 ms | 52.0 / 51.9 ms | 78.4 ms | 90.6 ms |
| 8,192 (3.0 default) | 22.4 / 22.4 ms | 23.6 / 23.4 ms | 49.7 ms | 56.4 ms |

This is the controlled version of the devbox sweep in the
[ladder receipt](collection-manifest-chunked-1m-ladder-2026-09-03.md):
2.3× on the scorer from residency alone. The `perf` self profile of the 1M
scorer no longer shows page verification at all — BLAKE3 and CRC32C, 44 %
of samples on `2ff8a4b`, are absent from the top 40 — and is now
`decode_lexical_segment` 12.6 %, the score sorts 20.8 %, `BorrowedLeaf`
decode + entries 16.2 %, `execute_lexical_plan` 7.6 %, and
`drop_in_place<NativeRuntimeError>` 5.5 % (the fail-open probe
constructing errors on the hot path; the cheap next fix)
([report](metal-2026-09-03-a443c52/07-perf-scorer-report.txt)).

## 7. Scorer equivalence at 1M

One oracle round on the 1M directory of §6 after the ladder
([raw](metal-2026-09-03-a443c52/09-equivalence-1m.txt)):

```text
stage=durable_scorer round=0 hits=1000 ms=39.5 terms=2 segments=1562 physical_entries=372418
stage=model_scorer   round=0 hits=1000 ms=12711486.6
stage=scorer_equivalence hits=1000 bit_identical=true
```

The retained-model scorer reproduces the durable scorer's 1,000 ranked
hits — ids, order, and score bits — at 1,000,000 documents on a tree whose
physical leaf layout the B+tree fix changed; the model took 3.5 hours, the
durable scorer 39.5 ms cold and 22 ms warm. With the `2ff8a4b` run this is
the second 1M equivalence on dedicated hardware and the fourth rung
(100k, 250k, 1M ×2) on which the two scorers agree bit for bit.

## Scope and non-claims

- Single host, one run per phase (three for the delta sweeps); same host
  class as the two earlier receipts but a different physical machine each
  time — deltas under ~10 % are not claimed.
- Shipped defaults throughout: 8,192-frame pool, no governor or execution
  pool (`workers=1`), `--scale full` baselines at their documented durable
  defaults.
- The collection bound stays 250,000; the 1M rows measure above the bound
  and the R5 vector conditions remain unmeasured.
- Synthetic corpora; no relevance claim.
- No universal superiority claim. On this host Hyphae's point reads and
  lexical queries are now within 1.4–11× of the specialized baselines and
  its embedded GET is faster than Redis over UDS at p50, while durable
  writes remain the honest cost of copy-on-write publication plus two
  fsyncs; the differentiator is unchanged — one CSN across three engines
  with a model-checked commit protocol and receipts for every number.
