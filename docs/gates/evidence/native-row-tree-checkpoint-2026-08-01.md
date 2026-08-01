# Native row, B+tree, and checkpoint evidence — 2026-08-01

Status: reviewable experimental substrate; G0 and G1 remain open

Code evidence is bound to clean commit
`868bc4370ac40c573ca8d37058dd92076e06bb79`. The checked latency receipt was
measured from that exact commit before this evidence document was added.

No PostgreSQL, SQLite, Valkey, Redis, OpenSearch, Elasticsearch, Lucene,
Tantivy, DataFusion, Redb, RocksDB, or upstream ANN implementation is used by
this native path.

## Implemented milestone

Three unpublished Hyphae-owned crates extend the first kernel:

- `hyphae-native-records` owns the exact committed MVCC row and immutable
  blob-reference codecs;
- `hyphae-native-btree` owns an immutable copy-on-write B+tree over native
  pages, including recursive splits, point lookup, ordered scan, historical
  roots, complete validation, and buffer-pool lookup; and
- `hyphae-native-manifest` owns immutable digest-chained root manifests,
  staged publication, and manifest-chain recovery.

The runtime now persists relational tables and primary-key rows in the native
B+tree. Leaf values are canonical two-column MVCC row records, not raw values
or a serialized database map. Current-root physical point lookup verifies the
table/primary-key-derived `RowId`, primary-key copy, visibility window, and row
codec after obtaining the page through the native partitioned buffer pool.

A durable checkpoint stages and publishes one immutable all-engine root
manifest, then appends and synchronizes a standalone WAL `CHECKPOINT` record.
Open cross-validates checkpoint generation, CSN, digest, prior-checkpoint LSN,
manifest root set, WAL commit anchor, and every committed current or
superseded root.

The data directory for this experimental path now contains `pages.hydb`,
`wal.hywal`, and `roots/` under one Hyphae-owned directory.

## Stable binary evidence

| Format | Exact encoded BLAKE3 |
|---|---|
| canonical MVCC row fixture | `ace9babb642187aae288a9d8823d20801341d271b4f83f417514270c88514d04` |
| canonical B+tree leaf fixture | `92def2e785f2d3e185cd52f89b98d548659f1471a7a2605472f1bf85eb7ec8ac` |
| canonical root manifest fixture | `b6211d9d373a4d01f5768126895aaaa4281bbba128929992259f9da7a4df047a` |

The existing native page, WAL block, local frame, and root-set checks remain
unchanged and continue to pass.

## Executed correctness evidence

The exact source commit passed:

- `cargo test --workspace --all-features --locked` under Debian WSL2;
- strict `cargo clippy --workspace --all-targets --all-features --locked --
  -D warnings` under Debian WSL2;
- 6 native B+tree tests;
- 5 canonical record/blob-reference tests;
- 4 immutable manifest tests;
- 16 runtime tests; and
- documentation validation for 131 Markdown files and 12 JSON examples plus
  the integration-boundary checker.

The focused tests cover 1,000 B+tree inserts and recursive splits, retained
historical roots, insert/upsert semantics, buffered reads, future-node
rejection, row/null/tombstone corruption, manifest checksum/digest corruption,
manifest gaps, temporary-stage cleanup, WAL checkpoint semantics, two
successive manifest generations, and recovery of a chain whose first manifest
was published before its checkpoint record.

The full Windows workspace remains blocked before test execution by Windows
Application Control (`os error 4551`) when a generated executable is loaded.
The policy was not weakened. Native code checks and strict Clippy passed on
Windows; complete executable validation used WSL2.

## Checkpoint crash matrix

After one cross-engine CSN was committed, the runtime was interrupted and
reopened at each checkpoint boundary:

| Boundary | Recovered checkpoint state |
|---|---|
| synchronized temporary manifest | temporary file removed; no published manifest or checkpoint |
| immutable manifest published | one verified unanchored manifest; WAL remains authority |
| WAL checkpoint block appended | manifest and checkpoint recovered in the in-process harness |
| WAL checkpoint block synchronized | manifest and checkpoint recovered |

The existing five-boundary commit matrix also remains green. These are
deterministic in-process interruptions, not sector-level power-loss evidence.
The Windows parent-directory flush remains explicitly unsupported by the safe
implementation.

## Latency observation

The clean
[WSL2 receipt](native-microsecond-smoke-row-tree-wsl2.json) contains one
million warm observations per operation, with 32 operations averaged in each
timer sample.

The native buffer-pool/B+tree/MVCC-row point lookup observed:

- p50 `0.255 us`;
- p95 `0.278 us`;
- p99 `0.639 us`;
- p99.9 `2.780 us`; and
- aggregate throughput `3,696,387 operations/s`.

This is the first measurement of the physical relational route, not merely
the materialized prepared-SQL snapshot path. It proves that this tiny warm
one-leaf path is in the microsecond domain on this machine. It does not pass
G7 because observations are batch-averaged, the corpus has one row, the tree
has no internal level, concurrency is one, the environment is WSL2, and
transport, interference, saturation, allocations, and hardware counters were
not measured.

## Explicit remaining work

- Blob references have an exact codec, but blob byte storage, staging,
  promotion, retention, and crash evidence remain pending.
- Inserts use canonical open-ended MVCC rows; update/delete version chains,
  tombstones in the physical tree, conflict tables, and vacuum/retention remain
  pending.
- The current B+tree lacks range cursors, prefix compression, bulk load,
  overflow values, secondary indexes, and calibrated fill-factor policy.
- Prepared SQL snapshots still materialize the relation tree. The new direct
  point API is the physical fast path; the general SQL executor must adopt
  native cursors and owned/pinned row views.
- Checkpoint recovery verifies the complete chain but still scans the complete
  WAL. Bounded replay, WAL retention/truncation, backup/restore, group commit,
  and a physical power-loss harness remain pending.
- Catalog, structure, and lexical-search state still use bounded single-page
  vertical scaffolding. They must migrate to their own scalable physical
  structures before G2, G3, or G4 can close.
