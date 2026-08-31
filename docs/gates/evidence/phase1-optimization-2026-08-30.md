<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — phase-1 hot-path optimization, before/after

- Date: 2026-08-30
- Baseline source: `main` @ `8aeb6ea` ("before")
- Candidate source: `8aeb6ea` + phase-1 optimization set ("after"),
  uncommitted at measurement time
- Hosts: DigitalOcean c-16 droplets (16 vCPU Intel Xeon Platinum 8280,
  virtualized — comparisons are relative A/B on identical hardware, never
  absolute latency claims; environment class 2 under
  [claims](../../product/claims.md))
- Raw receipts: [`phase1/`](phase1/) (verbatim harness output)
- Correctness gate: `cargo clippy --workspace --all-targets --all-features
  --locked -- -D warnings` exit 0; `cargo test --workspace --all-features
  --locked` exit 0 (full suite including the G5 crash matrices and G2
  SQL conformance) on the candidate tree.

## The optimization set (no durable-format changes)

1. **Pages**: streaming CRC32C/BLAKE3 over the canonical form
   (`hyphae-native-pages`); previously every encode/decode materialized two
   16 KiB copies per hash. Digests are byte-identical.
2. **WAL**: blocks encode once per append (previously twice) and both block
   hashes stream in place (previously three 64 KiB copies per block)
   (`hyphae-native-wal`). Digests are byte-identical.
3. **B+tree point lookups**: ascending-order early exit in leaf and internal
   nodes (`hyphae-native-btree`). The lookup still validates canonical order
   for every entry it visits and fails closed on the first violation;
   complete-node validation remains in write paths, `verify`, and doctor.
   One test updated accordingly
   (`borrowed_lookup_validates_the_visited_prefix_and_stops_at_the_match`).
4. **Segment plans through the buffer pool**: `plan_range_segments_cached` /
   `scan_planned_segment_cached` (additive B+tree API) replace the direct
   page-store reads in `plan_btree_segments` and the lexical segment scan,
   removing a full second physical read + BLAKE3 verification of every
   posting page per query.
5. **Search ingest batching**: consecutive `IndexDocument` mutations inside
   one commit now coalesce into a single copy-on-write sorted batch
   (`index_documents_batch_in_search_tree`), rewriting each touched leaf and
   internal path once per run instead of once per document. Per-document
   validation order and failure classes are unchanged; duplicate identities
   inside a run fail closed exactly like sequential reinsertion.
6. **V3 scalar-family checks through the buffer pool** plus an operator
   sizing knob (`HYPHAE_BUFFER_POOL_FRAMES`) and a disjoint receipt clock
   (`CommitReceipt::logical_execution_time`).

## Results — search engine (the dominated deficit)

Same-droplet before/after, identical deterministic corpora:

| Metric (Hyphae) | Before | After | Factor |
|---|---|---|---|
| BM25 top-10 p50, 20k docs | 2,266 µs | 317 µs | **7.15×** |
| BM25 top-10 p99, 20k docs | 3,573 µs | 490 µs | ~7× |
| Ingest p50, 1,000-doc strict batch | 26.2 s | 5.2 s | **5.04×** |
| Ingest 100k docs, full scale | **did not complete** (ENOSPC after filling 161 GB) | completed | write amplification removed |

The remaining query gap to Tantivy (~7× on this host) is attributable to
the structural items deliberately deferred to later phases: no top-k
pruning (the fail-closed `live_postings == document_frequency` check reads
complete posting ranges), uncompressed fixed postings, and per-candidate
document-length lookups.

## Results — point reads and commits (honest null result)

An interleaved B-A-B-A experiment (alternating binaries on one droplet,
one discarded warm-up) shows the first run of a session is a cold outlier
and that warm point-path latencies are statistically indistinguishable
between before and after on this host:

| Sequence | SQL SELECT p50 | Keyspace GET p50 |
|---|---|---|
| B1 (cold) | 302.6 µs | 26.2 µs |
| A1 | 94.1 µs | 26.5 µs |
| B2 | 86.5 µs | 26.4 µs |
| A2 | 96.2 µs | 26.6 µs |

Commit latencies (strict/memory), transaction shapes, and engine
composition were likewise unchanged within virtualized-host noise (±5%).
Interpretation: the point-read paths are not dominated by the per-page
linear scans this phase removed; the remaining suspects are buffer-pool
capacity versus working set (16 MiB default against ≳60 MiB of leaves at
1M keys) and the two-lock-plus-notify admission cost. The
`HYPHAE_BUFFER_POOL_FRAMES` sizing experiment is recorded separately when
its receipts land.

Earlier same-day single-run comparisons that suggested a SELECT
regression are superseded by this interleaved series: the effect was the
cold-first-run artifact, in both directions.

## Method notes

- Workloads: deterministic seeded xorshift64*; identical bytes and
  operation sequences across arms; exclusive per-operation timing.
- One variable per experiment; binaries verified distinct before running
  (`&pool,` marker counts 43 vs 48 in `structure_v3.rs`).
- Virtualized droplets are used for relative A/B only. Absolute
  comparative claims against SQLite/DuckDB/Redis/Tantivy remain bound to
  the dedicated-hardware receipt
  ([baseline-i7i-metal-2026-08-30](baseline-i7i-metal-2026-08-30.md)),
  which predates this optimization set; a dedicated-hardware re-run is the
  designated closure for phase-1 comparative numbers.
