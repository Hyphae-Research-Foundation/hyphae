# Native scalable catalog B+tree evidence

Date: 2026-08-02

Status: implemented and measured scalable catalog persistence; broader G0,
G1, G2, and G7 evidence remains open

Measured source commit:
`105fc0fa37281924d689a4837ab78b88630ed2d7`

Measured source tree:
`4435923f3112d27a88b08a0f211b97e47fe0c992`

Branch at measurement: `codex/native-catalog-btree`

## Change

New catalog writes use the `HYCAT003` native copy-on-write B+tree instead of
placing the complete catalog in one 16 KiB page. The tree has independent
stable-ID and normalized-qualified-name namespaces. Each name entry resolves
to an ID, and each definition is checked against both keys before it is
admitted.

`HYCVAL01` stores definitions up to 8,192 bytes inline. Larger canonical
`HYCOBJ01` definitions use the existing immutable content-addressed blob
store. Strict commits stage and synchronize those blobs before page and WAL
publication. Legacy `HYCAT001` and `HYCAT002` roots remain readable and migrate
on the next catalog mutation.

Current catalog reads by ID and qualified name traverse the immutable tree
through the partitioned buffer pool. Name lookup verifies the resolved object
on the same root. Page-generation vacuum rebuilds the complete reachable
catalog B+tree while retaining definition blobs.

## Correctness evidence

The pre-change acceptance test attempted to commit 256 relations in one
transaction. It failed for the intended reason with a 49,164-byte
`PayloadTooLarge` catalog page. The unchanged corpus passes with a multilevel
tree after the implementation.

The native runtime passed 148 tests and strict Clippy over all targets and
features with warnings denied. Focused coverage proves:

- golden marker, ID-key, name-key, and inline-envelope bytes;
- normalized-name duplicate rejection independent of display spelling;
- 256-object multilevel ID and name lookup plus strict reopen;
- retained prior-root readability after an incremental copy-on-write DDL;
- fewer appended pages for one incremental DDL than a full catalog rewrite;
- immutable-blob storage and reopen for a definition larger than 8,192 bytes;
- prior-state recovery at catalog-blob stage and promotion boundaries;
- physical migration from both `HYCAT001` and `HYCAT002`;
- rejection of nonzero reserved envelope bytes and cross-linked name entries;
- existing seven-boundary all-engine commit recovery with `HYCAT003`; and
- page-generation vacuum equality and reopen with a catalog B+tree.

The ordinary commit matrix already exercises catalog page, WAL and root
publication boundaries because its first all-engine transaction creates both
relation and search definitions. The focused large-definition matrix adds the
two blob-specific boundaries.

## Release observations

The machine-readable receipts were produced from the same clean source under
Rust 1.96.0, release profile and concurrency one. The deterministic corpus
creates 1,024 binary relations in one strict transaction, performs 50,000
buffered lookups by ID and 50,000 by normalized name across the full ID range,
adds relation 1,025 in a second strict transaction, and verifies both routes
after reopen.

| Observation | Windows x86_64 | WSL2 x86_64 |
|---|---:|---:|
| Bulk private preparation | 9.460 ms | 9.819 ms |
| Bulk strict commit | 102.620 ms | 59.196 ms |
| Incremental strict DDL | 15.848 ms | 8.254 ms |
| ID lookup p50 / p99 | 2.000 / 2.900 µs | 1.056 / 1.715 µs |
| Name lookup p50 / p99 | 5.700 / 10.800 µs | 2.437 / 4.668 µs |
| Whole page file after bulk commit | 1,423 pages | 1,423 pages |
| Whole-file pages appended by incremental DDL | 6 | 6 |
| Reopen verification | passed | passed |

The lookup routes are observed in the microsecond domain. Strict DDL is a
millisecond operation and includes page plus WAL synchronization. The 1,423
pages are the complete native page file for catalog and relational table
markers, not a catalog-only allocation count. The six incremental pages cover
both the catalog and relational copy-on-write roots.

## Remaining boundary

This vertical removes the catalog's single-page object-count ceiling. It does
not implement drops, object history, dependency edges, constraints, schema
evolution, definition-blob collection, concurrent DDL submission, saturation,
cold-cache behavior, p99.9, or a complete G7 matrix. Preparation of general
SQL plans can still require broader catalog materialization. One observation
per platform is not a universal latency guarantee. This milestone advances
G0/G1 and closes neither gate.
