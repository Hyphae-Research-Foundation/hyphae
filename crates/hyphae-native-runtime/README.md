# hyphae-native-runtime

This unpublished crate is the first executable convergence slice for Hyphae's
native data ecosystem. It owns a single data directory and coordinates native
catalog, relational, structure, lexical-search, and ANN state through
copy-on-write pages, one WAL transaction, and one published CSN.

Catalog create mutations and `HYCAT003` roots retain complete canonical
definitions in separate ID and normalized-name B+tree namespaces. Large
definitions use immutable blobs, and snapshots expose the definition pinned
to their catalog version. Legacy `HYCAT001` name-only and `HYCAT002`
single-page roots reopen and migrate on a later write.

Native SQL now binds primitive values and projections to those immutable
definitions. Typed inserts encode canonical primary-key components and
`HYTUPL01` catalog-ordered tuples; prepared point selects decode them after
commit and reopen. Type/domain, nullability, duplicate-column, and incomplete
key failures occur before mutation. Catalogued secondary indexes own physical
metadata and entries in the relational B+tree. `CREATE [UNIQUE] INDEX`
backfills atomically, inserts maintain every admitted projection, exact-key
`WHERE` can use composite indexes independent of predicate text order, and
bounded `EXPLAIN` identifies primary-key or secondary-index lookup. Unique
non-null collisions fail before publication; null equality retains SQL
three-valued behavior. Bounded primary-key scans, complete-key ranges,
left-prefix scans, prefix-plus-next-component ranges, ordered secondary-index
ranges, exact-PK typed updates/deletes, and current-root secondary execution
are physical native operators. New secondary indexes use the order-preserving
`HYRIDX02` identity; legacy `HYRIDX01` indexes keep exact lookup without false
range planning. Parameterized scalar residual filters support comparison,
`IS [NOT] NULL`, `NOT`, `AND`, and `OR`; the binder extracts admitted exact,
prefix, or range access and applies `LIMIT` after filtering. The historical
two-binary-column shape retains its raw bytes and allocation-free prepared
lookup. Qualified bounded `INNER JOIN` plans accept exact, primary-range, and
non-unique secondary left inputs, composite equalities, and primary or unique
secondary right access. They stop after `LIMIT` joined output rows; null join
keys and missing right rows do not consume that limit. General join
reordering, hash/merge and outer joins, literals beyond the admitted scalar
families, casts, arithmetic/functions, grouping, sorting/spill, cost planning,
and complete `EXPLAIN` remain pending.

The first relational physical route stores table markers and canonical MVCC
rows in the native copy-on-write B+tree and performs current-root point reads
through the native buffer pool. That route traverses node bytes without
materializing node entries, pins the matching leaf value, and validates a
borrowed row view before copying the selected value. Immutable root manifests
are anchored by standalone WAL checkpoint records and cross-validated during
recovery.
Binary SQL UPDATE and DELETE publish new roots while retained snapshots keep
their historical roots. Values above 8,192 bytes use the Hyphae-owned
content-addressed blob store, and committed WAL mutations rebuild the
point-write conflict table during recovery. New directories store a fixed
row-version pointer in the B+tree and retain an immutable, fail-closed chain
whose superseded records have explicit half-open `end_csn` boundaries.
Directories written with the earlier inline-row marker remain readable and
writable without an implicit format conversion.
New structure roots also use the native B+tree: binary keys map to a canonical
TTL/storage envelope, direct `GET`/`TTL` traverse pinned pages, and large
structure strings share the immutable blob namespace with relational values.
Explicit hashes use independent field paths, sets use exact member paths,
lists use bounded packed deque chunks, and sorted sets maintain both a
member-to-score index and a score/member ordered index. Their mutations share
the same WAL, MVCC root, conflict table, crash recovery, and strict reopen
authority as scalar structure values; none is serialized as one scalar value.
Legacy single-page structure roots remain readable and writable.
New lexical-search roots use native collection, stored-document, term, and
posting B+tree namespaces. Direct `MATCH` prunes to each query term's posting
range, loads only candidate document lengths, and produces the same BM25
scores as the materialized reference. Large UTF-8 source text shares the blob
namespace, and legacy single-page search roots remain compatible.

The same search B+tree now stores catalog-bound vector-index metadata,
immutable canonical `f32` vectors, and HNSW neighbor layers under a
content-bound generation identity. `CREATE ANN INDEX`, `UPSERT VECTOR`, and
`DELETE VECTOR` are WAL operations under the global CSN. Duplicate-free batch
upsert performs one atomic private rebuild, and commit groups every target
index into one canonical persisted replacement. Retained roots preserve
historical vector results; reopen validates and restores the complete graph
before serving exact or explicitly approximate search.

The implementation remains deliberately bounded: retained transaction
snapshots still materialize relation state, while current-root page vacuum,
WAL/manifest retention, and blob collection do not yet provide configurable
multi-generation pins. Composite secondary equality-prefix ranges, descending
traversal, streaming cursors, and zero-copy operator cursors remain pending.
Structures still lack streams, bitmaps, probabilistic structures, geo,
complete string/hash/set/list/sorted-set operations, collection TTL, active
expiry backoff, blocking operations, eviction, and protocol exposure. Lexical
search still lacks positions, phrases, filters, facets, doc values,
deletes/updates, segments, and hybrid fusion. ANN still materializes the
validated generation at open/snapshot time; direct buffer-pool graph
traversal, snapshot filters, delta/tombstone merge, background build
publication, and reclamation remain pending. Detached transactions can prepare
concurrently without holding writer admission; commit validates their
original read CSN and rebases disjoint mutations over the admitted current
root. Publication and its durability I/O are still serialized and require
exclusive access to the database handle. This proves the native
transaction/recovery architecture; it is not the complete SQL, Valkey-class
structure, or OpenSearch-class search engine.
