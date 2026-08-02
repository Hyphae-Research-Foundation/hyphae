# Native local ecosystem phase-1 gate

Status: in progress; G0 specifications are drafted but implementation and
dependency evidence remain incomplete

This is the ordered implementation gate for the Hyphae-owned relational,
structure, and search ecosystem. It describes future work, not shipped
`0.2.1` behavior.

A later gate may be prototyped early but cannot be declared complete while an
earlier gate is red.

| Gate | Required outcome | Exit evidence |
|---|---|---|
| G0 | Constitution and native specifications | Accepted architecture; versioned type, row, page, blob, WAL, MVCC, SQL, structure, search, ANN, local-protocol and benchmark contracts; clean-room/dependency inventory |
| G1 | Native substrate | Hyphae page/blob store, WAL, catalog, CSN/MVCC, partitioned memory and scheduler; no Redb on the target path; crash injection at every commit/checkpoint boundary |
| G2 | Relational engine | Native DDL/DML, constraints, transactions, indexes, joins, CTEs, windows, prepared plans and `EXPLAIN`; SQLLogicTest, metamorphic tests, isolation litmus, TPC-H correctness and TPC-C ACID evidence |
| G3 | Structure engine | Native strings, counters, hashes, lists, sets, sorted sets, streams, TTL and atomic batches; model-based randomized tests, expiry under controlled time, restart equivalence and memory-amplification evidence |
| G4 | Search engine | Native analyzers, postings, BM25, phrase/prefix/fuzzy, doc values, facets, aggregations, exact vector, ANN and hybrid; scoring goldens, NDCG/recall, rebuild and corruption evidence |
| G5 | Convergence | One transaction mutates all three engines; concurrent readers never observe a mixed CSN; SQL joins and aggregates native structure/search sources through stable IDs and native operators; one checkpoint, backup and restore preserves the complete state |
| G6 | Local product | Embedded API, native binary local protocol, CLI, SDK, administration, `EXPLAIN`, telemetry, doctor, backup and restore share one catalog and error model |
| G7 | Performance | The microsecond contract passes on stable hardware with warm/cold, concurrency, saturation, background interference, p99.9, allocation and hardware-counter receipts |
| G8 | Release evidence | Soak, crash, corruption, resource exhaustion, migration v2-to-native, multiplatform packaging, SBOM, signatures and independent restore verification are green on one exact commit |

## G0 required decisions

G0 must close these decisions before production implementation:

- stable IDs and canonical type semantics;
- page size, row and variable-length layouts, blob references and checksums;
- WAL block, operation, transaction, LSN/CSN and checkpoint formats;
- snapshot isolation conflict rules and the path to serializable execution;
- SQL grammar, null semantics, casts, collation and error model;
- structure ownership, TTL, eviction and volatile-versus-canonical behavior;
- analyzer, postings, doc-values, lexical score and ANN contracts;
- cross-engine transaction, link and view semantics;
- native local framing, authentication boundary and cancellation;
- strict, group and memory durability classes; and
- the reproducible performance and quality corpus.

No disk format, public API, or crate boundary should be frozen before the
relevant G0 contract is reviewable.

The current reviewable drafts are:

- [canonical types](../native/types-v1.md);
- [page, row, and blob format](../native/page-row-blob-format-v1.md);
- [B+tree format](../native/btree-format-v1.md);
- [root manifest and checkpoint format](../native/root-manifest-checkpoint-v1.md);
- [WAL format](../native/wal-format-v1.md);
- [MVCC and commit semantics](../native/mvcc-commit-v1.md);
- [catalog](../native/catalog-v1.md);
- [Hyphae SQL](../native/sql-semantics-v1.md);
- [structures](../native/structures-semantics-v1.md);
- [search](../native/search-semantics-v1.md);
- [ANN](../native/ann-semantics-v1.md);
- [native local protocol](../native/local-protocol-v1.md); and
- [clean-room/dependency policy](../native/dependency-inventory.md).

These drafts do not close G0 until their golden encodings, dependency audit,
benchmark corpus, and implementation-facing tests exist.

## Current experimental evidence

The [2026-08-01 kernel evidence](evidence/native-phase1-kernel-2026-08-01.md)
implements the first reviewable vertical. Steps 1 through 4 and the
reopen-equivalence portion of step 6 below execute in tests. Step 5 now covers
five in-process commit boundaries and four manifest/checkpoint boundaries, but
not blobs, group commit, filesystem reordering, or sector-level power loss.
Step 7 has an embedded and frame-codec smoke only; no named-pipe/UDS transport
receipt.

The follow-on [row, B+tree, and checkpoint
evidence](evidence/native-row-tree-checkpoint-2026-08-01.md) binds canonical
MVCC rows, native copy-on-write relational storage, immutable root manifests,
the four-boundary checkpoint matrix, and a clean physical point-read latency
observation to one source commit.

The [blob, mutation, and conflict
evidence](evidence/native-blobs-mutations-conflicts-2026-08-01.md) adds the
immutable content-addressed blob store, SQL UPDATE/DELETE with copy-on-write
roots and tombstones, a WAL-rebuilt point conflict table, two blob crash
boundaries, and a clean height-two B+tree latency observation.

The [relational version-chain
evidence](evidence/native-relational-version-chains-2026-08-01.md) binds the V2
row pointer, immutable closed histories, V1 compatibility, fail-closed chain
recovery, and the measured cost of the additional version-page lookup to one
source commit.

The [optimistic-writer
evidence](evidence/native-optimistic-writers-2026-08-01.md) binds concurrent
detached preparation, admitted-root rebase across all three engines,
first-committer-wins, lagging-read-CSN recovery, and the optimistic crash
matrix to one source commit.

The [structure B+tree
evidence](evidence/native-structure-btree-2026-08-01.md) binds the first
multilevel scalar keyspace, canonical TTL/blob envelope, legacy compatibility,
cross-engine blob deduplication, direct buffered reads, and a clean latency
observation to one source commit.

The [scalar structure mutation
evidence](evidence/native-scalar-structure-mutations-2026-08-01.md) binds
canonical physical tombstones, `DELETE`, independent `EXPIRE`, `NX`/`XX`,
checked signed counters, exact-boundary TTL absence, WAL expiry-presence
compatibility, same-key conflict admission, and crash recovery to one source
commit.

The [native hash structure
evidence](evidence/native-hash-structure-2026-08-01.md) binds the first
compound family: explicit type creation, independent field storage,
cardinality metadata, field tombstones, field-granular conflict/rebase,
multilevel scale, corruption rejection, crash recovery, and direct `HGET`
latency to one source commit.

The [native inverted-search
evidence](evidence/native-inverted-search-2026-08-01.md) binds the replacement
of the bounded search page for new directories with
collection/document/term/posting B+tree namespaces, separator-pruned posting
scans, exact reference-BM25 equivalence, shared large text blobs, multilevel
restart/corruption tests, legacy inline compatibility, and the first clean
physical `MATCH` latency baseline.

The [native ANN kernel
evidence](evidence/native-ann-kernel-2026-08-01.md) binds the first
Hyphae-owned deterministic HNSW implementation, three exact metric semantics,
canonical mutation rebuild, exact oracle, content-bound build identity,
fail-closed restore and a bounded quality observation to one source commit.

The [durable native ANN
evidence](evidence/native-ann-durability-2026-08-01.md) connects that kernel to
checked catalog definitions, search B+tree vector/graph generations, three WAL
opcodes, optimistic vector-level conflicts, historical all-engine CSNs,
canonical reopen, batch rebuild, corrupt/orphan rejection, and a seven-boundary
cross-engine crash matrix. Its clean WSL2 receipt observes a 512-vector,
32-dimensional, concurrency-one materialized-snapshot query. Buffered graph
traversal, filters, delta/tombstone merge, background publication,
reclamation, one-million-vector quality and the complete G7 matrix remain
open, so this milestone closes no gate.

The [native type-codec
evidence](evidence/native-type-codecs-2026-08-01.md) binds recursive logical
type descriptors, checked primitive row payloads, memcomparable ordered-index
components, null separation, malformed/noncanonical failure behavior, and
cross-platform workspace validation to one source commit. Persistent typed
catalog definitions, typed rows, and physical secondary indexes remain open.

The [native catalog-definition
evidence](evidence/native-catalog-definitions-2026-08-01.md) binds
`HYCOBJ01`, complete-definition WAL mutations, `HYCAT002` roots, immutable
snapshot introspection, legacy `HYCAT001` reconstruction, reopen proof, and
fail-closed definition validation to one source commit. Catalog B+tree
scaling, definition history, typed rows, and secondary indexes remain open.

The [native typed SQL-row
evidence](evidence/native-typed-sql-rows-2026-08-01.md) binds catalog-typed
DDL, checked primitive and composite primary-key inserts, `HYTUPL01` row
payloads, catalog-bound projection and prepared point lookup, strict
failure-before-mutation behavior, reopen equivalence, and raw binary-route
compatibility to one source commit. Scans, secondary indexes, typed
updates/deletes, expressions, planning, joins, and the remaining G2 evidence
remain open.

The [native secondary-index
evidence](evidence/native-secondary-indexes-2026-08-01.md) binds stable
catalog definitions, `HYRIDX01` metadata and physical entries, exact-key
single/composite SQL binding, unique/null semantics, bounded `EXPLAIN`, both
optimistic index/row commit orders, strict reopen, and fail-closed projection
validation to one source commit. Direct pinned execution, ranges, indexed
typed updates/deletes, statistics/cost planning, and the remaining G2/G7
evidence remain open.

The [native direct secondary-index
evidence](evidence/native-direct-secondary-index-2026-08-01.md) binds
catalog-only latest-plan preparation, one-root physical index-to-row
execution, deterministic order, materialized/historical equivalence,
catalog-version invalidation, reopen/corruption coverage, and a matched clean
schema-v7 WSL2 observation to one source commit. Streaming/ranges, indexed
typed updates/deletes, statistics/cost planning, and the remaining G2/G7
evidence remain open.

The [native typed indexed-mutation
evidence](evidence/native-typed-indexed-mutations-2026-08-01.md) binds
exact-primary-key typed update/delete, atomic old/new index projections,
unique admission recheck, retained history, inline-V1 compatibility, reopen,
and a seven-boundary crash matrix to one source commit. Primary-key changes,
multi-row/range mutation, expressions, general constraints/planning, and the
remaining G2 evidence remain open.

The [native bounded relational-scan
evidence](evidence/native-bounded-relational-scan-2026-08-01.md) binds a
reentrant buffered B+tree visitor, exclusive primary-key cursor, visible-row
limit, HYRELBT1/HYRELBT2 tombstone handling, bounded no-predicate SQL,
transaction/materialized/physical equivalence, reopen and failure coverage,
and a clean schema-v8 WSL2 observation to one source commit. Filters,
secondary ranges, descending/offset execution, zero-copy operator cursors,
joins, aggregation, statistics/cost planning, and the remaining G2/G7
evidence remain open.

The [native primary-key range
evidence](evidence/native-primary-key-ranges-2026-08-01.md) binds independent
inclusive/exclusive physical bounds, separator pruning, composite SQL row
comparisons, transaction/materialized/current-root equivalence, empty-range
safety, reopen and failure coverage, and a clean schema-v9 WSL2 observation to
one source commit. Residual/non-key filters, partial PK prefixes, secondary
ranges, descending/offset execution, zero-copy operator cursors, joins,
aggregation, statistics/cost planning, and the remaining G2/G7 evidence
remain open.

The [native SQL residual-filter
evidence](evidence/native-sql-residual-filters-2026-08-01.md) binds
parameterized scalar comparison, `IS [NOT] NULL`, SQL three-valued boolean
logic and precedence, exact/range access extraction, post-filter `LIMIT`,
transaction/materialized/current-root/reopen equivalence, and a clean
schema-v10 WSL2 observation to source and benchmark commits. Literals, casts,
functions, joins, aggregation, partial/secondary ranges, statistics/cost
planning, and the remaining G2/G7 evidence remain open.

The [native SQL scalar-literal
evidence](evidence/native-sql-scalar-literals-2026-08-01.md) binds `NULL`,
boolean, signed-integer and escaped text literals to catalog logical types,
retains exact/index/range access extraction, and proves materialized,
current-root and reopened equivalence. Remaining literal families, casts,
functions, joins, aggregation, partial/secondary ranges, statistics/cost
planning, and the remaining G2/G7 evidence remain open.

The [native SQL mutation-literal
evidence](evidence/native-sql-mutation-literals-2026-08-01.md) extends those
catalog-bound operands to `INSERT`, exact-primary-key `UPDATE`, and
exact-primary-key `DELETE`, including mixed parameter order, fail-before-write
binding, secondary-index maintenance, and reopen. General mutation
expressions, multi-row/range mutation, joins, aggregation, planning, and the
remaining G2 evidence remain open.

The [native indexed inner-join
evidence](evidence/native-indexed-inner-join-2026-08-01.md) binds the first
qualified `INNER JOIN` to exact primary/unique-secondary left access and a
single-column right primary key. It proves private, retained, physical and
reopened execution, typed fail-closed binding, and a clean 100,000-call WSL2
observation. Composite/right-secondary joins, aliases and expressions,
statistics/cardinality estimation, join ordering, hash/merge/outer operators,
spill, SQLLogicTest, TPC-H/TPC-C, and the full G2/G7 evidence remain open.

The [native bounded inner-join
evidence](evidence/native-bounded-inner-join-2026-08-01.md) extends that plan
to full and ranged left primary-key inputs with a mandatory output-level
`LIMIT`. It proves early stop after valid right matches, private rows,
historical/physical/reopened equivalence, typed failures, and clean exact plus
`LIMIT 10` WSL2 observations. At that source commit, non-unique secondary
inputs, general join ordering and algorithms, spill, the complete G2
correctness suites, and the G7 matrix remained open.

The [native secondary-index inner-join
evidence](evidence/native-secondary-inner-join-2026-08-01.md) adds bounded
non-unique secondary equality as a canonical left input. It proves
output-level early stop across private, retained, physical and reopened
execution, plus exact planning and typed rejection paths. Its clean release
observation measures 100,000 `LIMIT 10` calls through a 128-row secondary
cohort. At that source commit, secondary ranges, composite/right-secondary
access, general join ordering and algorithms, spill, the complete G2
correctness suites, and the G7 matrix remained open.

The [native right secondary-index inner-join
evidence](evidence/native-right-secondary-inner-join-2026-08-01.md) makes the
right access path explicit and adds exact lookup through a single-column
`UNIQUE` secondary index. Private index rewrites, historical/current/reopened
equivalence, typed rejection of a non-unique right index, and 100,000 clean
release calls are proven. Composite equality/keys, non-unique or ranged right
access, general join ordering and algorithms, spill, the complete G2
correctness suites, and the G7 matrix remain open.

The [native dual-index sorted-set
evidence](evidence/native-sorted-set-2026-08-01.md) binds exact binary
membership and score/member order to one native structure B+tree. It proves
canonical binary64 ordering, retained snapshots, member-granular optimistic
rebase, strict reopen, fail-closed dual-index recovery, all seven commit crash
boundaries, and clean physical microsecond observations over 2,048 members.
Score ranges, reverse/rank acceleration, algebra, TTL, protocol exposure,
model testing, amplification evidence, the complete G3 suite, and G7 remain
open.

The [native durable scalar-expiry
evidence](evidence/native-expiry-2026-08-01.md) binds `HYSTRBT2`, its ordered
expiry namespace, bounded deterministic cleanup, `HYSTRBT1` compatibility,
first-committer-wins renewal safety, fail-closed reconstruction, all seven
cleanup crash boundaries, and the first scheduler/cleanup latency
observations to one clean source commit. The empty hot scan is measured in
microseconds; its initial multi-key cleanup batches remained millisecond
operations and exposed complete-state materialization plus per-key
copy-on-write publication as measured performance work.

The [native ordered B+tree batch copy-on-write
evidence](evidence/native-btree-batch-cow-2026-08-02.md) removes both measured
cleanup bottlenecks. The batch primitive validates complete ordered input
before writing, rewrites each affected node and internal level once, preserves
unaffected subtree page IDs, and coalesces `HYSTRBT2` scalar plus expiry
tombstones. The same warm-state datasets improved `Memory` and `Strict` p50
latency by 80.533% and 79.674%, while appending 0.054199 and 0.191406 pages per
key. `Memory` remains millisecond-scale and `Strict` p99 remains above one
millisecond. At that source, tombstone compaction, general mutation
integration, scheduling, and the complete G7 matrix remained open.

The [native structure reachability-compaction
evidence](evidence/native-structure-compaction-2026-08-02.md) adds an explicit
`HYSTRBT2` maintenance commit that validates the complete current tree, drops
only canonical tombstones, preserves logical state and prior roots, and
recovers prior-or-complete at every commit boundary. On the measured
2,048-expired/2,048-live corpus it reduced reachable node pages from 41 to 10
and empty-expiry-scan p50 by 93.133%. The append-only file still grew by the
ten replacement pages. The
[native page-generation vacuum contract](../native/page-vacuum-v1.md) now
fixes the retention floor, generation publication, rewrite, and crash matrix;
implementation and measured physical reclamation remain open.

The [native dependency-closure
evidence](evidence/native-dependency-closure-2026-08-02.md) makes the non-dev
normal/build graph rooted at `hyphae-native-runtime` an exact fail-closed
allowlist. Its clean WSL2 receipt contains 11 Hyphae-owned packages, 19
reviewed external primitive/build packages, no forbidden engine, and zero
native unsafe findings. Reported third-party unsafe syntax still requires
semantic review, and the remaining golden/corpus/conformance requirements keep
G0 open.

This evidence advances G0/G1 but closes neither gate. Relational table/primary
key storage now uses the native B+tree and canonical MVCC rows, and large row
values use native immutable blobs. The current implementation also retains
explicit per-key row-version chains with closed half-open intervals while
keeping V1 inline-row directories compatible. Catalog now retains complete
typed definitions but remains bounded to one root page; new search roots use a
native inverted B+tree while legacy inline roots remain compatible. Detached
transactions now prepare concurrently and rebase disjoint all-engine writes
under serialized publication;
simultaneous commit submission and concurrency/saturation evidence remain
pending, along with retention/vacuum, secondary-index range/streaming
execution, zero-copy relational operator cursors, general relational
expressions beyond the admitted residual slice, constraints/planning,
remaining structure families, positional
postings, segments, buffered/filtered ANN, and hybrid fusion required by
G1–G4.
Typed point inserts, exact-PK update/delete, and catalog-bound
primary/secondary-key projection now use canonical tuples and
primitive/composite keys, but do not close G2. The structure keyspace has a
multilevel native B+tree, direct reads, tombstone/expiry mutations,
conditional writes, signed counters, independent-field hashes, exact binary
sets with member-granular conflicts, chunked-deque lists, and dual-index
sorted sets. It still lacks whole-hash lifecycle/iteration, set and sorted-set
algebra/TTL, score/reverse/rank sorted-set operations, streams, an engine-owned
background timer around the implemented scalar expiry primitive, model tests,
and retention-aware physical vacuum required to close G3. The ordered cleanup
and current-root compaction receipts provide exact first amplification
measurements, not the complete memory-amplification gate.

## G1 substrate exit

G1 requires a minimal vertical proof after the substrate exists:

1. create one relation and perform a primary-key insert/read;
2. create one keyspace value with TTL and perform a point read;
3. create one lexical index and perform a match;
4. commit mutations to all three engines under one CSN;
5. interrupt the process at every WAL, page, root-publication and checkpoint
   boundary;
6. recover and prove that readers see either the complete prior CSN or the
   complete committed CSN; and
7. run the first embedded and local-protocol latency receipt.

This slice proves architecture only. It does not close G2, G3, or G4.

## Phase-1 stop rule

Clustering, replication, multitenancy, control planes, SaaS billing, hosted
operations, embedding generation, HiveMind, and any LLM are outside this
phase. They cannot consume implementation capacity or weaken a phase-1 gate.

Phase 2 begins only after G8 has exact release evidence. Its architecture is
not declared by this document.
