# Native phase-1 kernel evidence — 2026-08-01

Status: reviewable experimental vertical; G0 and G1 remain open

This evidence covers the first Hyphae-owned execution path. It does not use
Redb, PostgreSQL, Valkey, Redis, OpenSearch, Lucene, Tantivy, DataFusion, or an
upstream ANN implementation.

## Implemented path

The unpublished native crates now provide:

- canonical stable IDs, logical declarations, and canonical floating values;
- 16 KiB immutable pages with CRC32C and BLAKE3, an append-only page file,
  incomplete-tail repair, and a partitioned buffer pool;
- a 64 KiB chained WAL with canonical record LSNs, typed transaction bodies,
  strict integrity recovery, and incomplete-tail repair;
- immutable catalog snapshots and the first persisted runtime catalog root;
- one serialized MVCC writer, immutable retained snapshots, CSN reservation,
  WAL-anchored root-set hashing, and recovery restore;
- binary primary-key relations with native SQL `CREATE TABLE`, `INSERT`, and
  prepared parameterized `SELECT`;
- binary scalar `SET`/`GET` with snapshot logical-time TTL;
- deterministic lexical `MATCH` with BM25 scoring over the bounded vertical;
- a 32-byte native local frame codec with bounds, version/kind checks, CRC32C,
  and borrowed payload decode; and
- one data directory containing `pages.hydb` and `wal.hywal`.

One strict transaction creates a relation and a search collection, inserts a
row, sets a value with TTL, and indexes a document. Its terminal WAL record
binds the catalog plus relational, structure, and search page roots. One root
publication makes every result visible at CSN 1. Reopen verifies the WAL digest
chain, semantic mutation digest/counts, root page kinds and page integrity
before restoring visibility.

## Executed correctness evidence

The targeted native suites passed after strict Clippy:

| Crate/path | Evidence |
|---|---|
| native types | 5 unit tests |
| native pages | 7 unit tests, including exact encoding digest and incomplete-tail repair |
| native WAL | 6 unit tests, including exact encoding digest, chain recovery, torn tail, and complete corruption |
| native catalog | 4 unit tests |
| native MVCC | 5 unit tests |
| native runtime | 12 unit tests, including SQL, TTL, BM25, local frame, cross-engine commit, recovery of current and superseded roots, historical snapshot, and crash matrix |

The exact page golden is BLAKE3
`d4e3a11874f9bd5b9c4afdc4093466b493de23ac4b80c11843ad9b465bbf7e71`.
The exact WAL-block golden is
`01aa068a8ecdde357f3f2c8c9f9addd851a54f6012db680b3fa1154c23f44627`.
The exact local-frame golden is
`70db9ece6d900078af4565c7c0017ee6de90c08d28f92443a0135d8cb6fb7120`.

Workspace validation used Rust 1.96.0 in both environments:

- native-crate tests and strict Clippy passed on Windows;
- documentation validation passed for 126 Markdown files and all maintained
  JSON examples;
- integration-boundary validation returned `integration-boundaries-ok`;
- full `cargo test --workspace --all-features --locked` passed under Debian
  WSL2; and
- full strict workspace Clippy with all targets/features passed under WSL2.

The equivalent full Windows workspace build remains blocked before test
execution by Windows Application Control (`os error 4551`) when loading a
generated proc-macro/build-script executable. Moving `CARGO_TARGET_DIR` from
the repository's `E:` junction to physical `C:` storage reproduced the same
policy block. No security policy was weakened. This is a Windows validation
residual, not a native-crate test failure.

## Commit crash matrix

The vertical was interrupted deterministically at each currently implemented
commit boundary and then reopened:

| Boundary | Recovered state |
|---|---|
| after page append | prior CSN |
| after page synchronization | prior CSN |
| after complete WAL append | complete CSN 1 |
| after WAL synchronization | complete CSN 1 |
| after root publication | complete CSN 1 |

No case exposed a relational, structure, or search subset. This is an
in-process deterministic matrix. It does not yet emulate sector-level power
loss, filesystem reordering, checkpoint interruption, blob promotion, or
group commit.

## Latency observation

The checked [Windows smoke receipt](native-microsecond-smoke-windows.json)
contains one million warm observations per operation, with 32 operations
averaged in each timer observation because the Windows timer returned
zero-duration samples for individual sub-microsecond calls.

The observed p99 batch-average values were 0.006 us for a 64-byte embedded
structure get, 0.006 us for an allocation-free prepared primary-key SQL read,
and 0.031 us for local-frame decode plus embedded structure dispatch.

These numbers demonstrate that the bounded in-process path is in the
microsecond domain on this machine. They do not pass G7: the source state was
dirty; the corpus was tiny; named-pipe transport, concurrency 8/32, saturation,
background interference, allocations, hardware counters, and individual tail
samples were not measured.

## Explicit remaining work

- G0: fixture corpus beyond three goldens, license/transitive-unsafe audit,
  benchmark corpus, and accepted review.
- G1: row/blob codecs, conflict table, checkpoint/root manifest, blob
  promotion, group commit, scheduler, resource budgets, fuzz/property models,
  and physical power-loss harness.
- G2: general types and rows, secondary indexes, constraints, updates/deletes,
  joins, CTEs, windows, optimizer, spill, SQLLogicTest, and ACID workloads.
- G3: counters, hashes, lists, sets, sorted sets, streams, expiry scheduler,
  eviction classes, and model-based compatibility tests.
- G4: persisted postings/segments, phrases, prefix/fuzzy, doc values, facets,
  aggregations, exact vector, HNSW, hybrid search, and million-item quality
  corpora.
- G5–G8: converged operators, checkpoint/backup/restore, named-pipe/UDS
  service, CLI/SDK administration, stable performance gates, soak, migration,
  packaging, and independent restore evidence.

The runtime currently serializes each small engine state into one copy-on-write
page. That is intentional vertical-slice scaffolding and must be replaced by
native heaps, trees, hash directories, postings, segments, and graphs before
scale claims.
