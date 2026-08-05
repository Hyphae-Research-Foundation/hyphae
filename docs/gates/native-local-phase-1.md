# Native local ecosystem phase-1 gate

Status: in progress. The current closure/revalidation state is maintained in
[`native-gate-status.md`](native-gate-status.md); this document remains the
normative definition of gate outcomes.

This is the ordered implementation gate for the Hyphae-owned relational,
structure, and search ecosystem. It describes future work, not shipped
`0.2.1` behavior.

A later gate may be prototyped early but cannot be declared complete while an
earlier gate is red.

| Gate | Required outcome | Exit evidence |
|---|---|---|
| G0 | Constitution and native specifications | Accepted architecture; versioned type, row, page, blob, WAL, MVCC, SQL, structure, search, ANN, local-protocol and benchmark contracts; clean-room/dependency inventory |
| G1 | Native substrate | Hyphae page/blob store, WAL, catalog, CSN/MVCC, partitioned memory and scheduler; no Redb on the target path; crash injection at every commit/checkpoint boundary |
| G2 | Bounded relational engine | The complete versioned Hyphae SQL G2 bounded profile: catalog-backed bounded DDL/DML and immediate constraints; snapshot transactions; primary, secondary and unique indexes; admitted bounded inner-join, nonrecursive CTE, window, prepared-plan and `EXPLAIN` shapes. Unsupported SQL fails closed. Exit evidence is hosted exact-SHA SQLLogicTest-format bounded conformance, metamorphic and isolation suites, plus canonical-derived bounded TPC-H correctness and TPC-C ACID fixtures. This does not claim universal SQL compatibility, official benchmark conformance, complete canonical benchmark execution or performance. |
| G3 | Structure engine | Native strings, counters, hashes, lists, sets, sorted sets, streams, TTL and atomic batches; model-based randomized tests, expiry under controlled time, restart equivalence and memory-amplification evidence |
| G4 | Bounded search engine | Canonical native analyzer and durable postings; bounded BM25, boolean/phrase/prefix/fuzzy execution; typed doc values, filters, sort, facets and metric aggregations; exact vector, filtered ANN and same-snapshot hybrid fusion. Exit evidence requires analyzer/scoring goldens, NDCG/recall thresholds, lifecycle/rebuild/compaction equivalence and fail-closed corruption matrices. Automatic segments, page-buffered ANN and production-scale performance remain explicit G7 non-claims. |
| G5 | Bounded convergence | One transaction mutates relational, structure and search/ANN surfaces under one root set and CSN; concurrent readers observe no mixed generation; typed relation-valued structure/search/vector sources join and aggregate by stable `ObjectId` with bounded native execution; one checkpoint plus verified native backup/restore preserves the complete admitted state. Universal federated SQL, online incremental backup and production-scale performance remain non-claims. |
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

G0 requires a reproducible hosted corpus and bounded quality receipts that
freeze generators, identities, reference semantics, metrics, and replay. It
does not require the final product-scale performance claim. Production-scale
latency and saturation remain G7 evidence; search quality at the complete
feature and target corpus remains G4 evidence. Those later gates may supply
inputs to G0, but G0 cannot silently claim their closure.

No disk format, public API, or crate boundary should be frozen before the
relevant G0 contract is reviewable.

The current reviewable drafts are:

- [canonical types](../native/types-v1.md);
- [directory format marker](../native/directory-format-v1.md);
- [page, row, and blob format](../native/page-row-blob-format-v1.md);
- [current-root page-generation vacuum](../native/page-vacuum-v1.md);
- [B+tree format](../native/btree-format-v1.md);
- [root manifest and checkpoint format](../native/root-manifest-checkpoint-v1.md);
- [WAL format](../native/wal-format-v1.md);
- [MVCC and commit semantics](../native/mvcc-commit-v1.md);
- [native group commit](../native/group-commit-v1.md);
- [native mixed-durability scheduler](../native/mixed-durability-scheduler-v1.md);
- [native active-expiry scheduler](../native/active-expiry-scheduler-v1.md);
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

The [native directory identity and writer exclusion
evidence](evidence/native-directory-identity-linux-2026-08-02.md) binds
canonical native `FORMAT`, UUIDv7 lineage identity, fail-closed marker-family
validation, and operating-system single-writer ownership to one direct Linux
commit. Offline promotion crash boundaries and manifest/anchor lineage
threading remain open, so this evidence closes neither G0 nor G1.

The [2026-08-01 kernel evidence](evidence/native-phase1-kernel-2026-08-01.md)
implements the first reviewable vertical. Steps 1 through 4 and the
reopen-equivalence portion of step 6 below execute in tests. Step 5 now covers
five in-process commit boundaries and four manifest/checkpoint boundaries, but
not blobs, group commit, filesystem reordering, or sector-level power loss.
Step 7 now has filesystem-backed UDS framing, persistent PING, structure
`GET`/`SET`/TTL, lexical `SEARCH MATCH`, and prepared SQL `SELECT` receipts
on direct Linux. Windows named-pipe transport, the complete session, explicit
all-engine transactions, and the full performance matrix remain open.

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

The [native bounded hash-scan
evidence](evidence/native-hash-scan-linux-2026-08-02.md) adds exact binary
field iteration across private, retained, current-root, and reopened
execution. Its exclusive field cursor remains valid after deletion, zero
limit validates type and existence, and the physical route maps the cursor
into the hash-field B+tree prefix, skips tombstones, and stops at the requested
live count.

The [native whole-hash lifecycle
evidence](evidence/native-hash-lifecycle-linux-2026-08-02.md) adds typed
`DELETE_HASH`, same-transaction recreation, a non-publishing lifecycle
dependency that preserves disjoint-field admission, metadata/field prefix
tombstoning, fail-closed replay, compaction, all seven singleton commit crash
boundaries, and latency separated into private, memory-publication, and strict
durability surfaces.

The [native whole-hash TTL
evidence](evidence/native-hash-ttl-linux-2026-08-03.md) adds absolute
whole-family expiry, persistent/expiring metadata compatibility, typed shared
expiry indexing, logical absence, cross-family key reuse, lifecycle
conflicts, mixed scalar/hash scheduler cleanup, crash boundaries, compaction,
and matched direct-Linux latency.

The [native hash field command
evidence](evidence/native-hash-field-commands-linux-2026-08-03.md) adds
bounded positional multi-read, canonical atomic multi-set/delete, signed field
counters, failure atomicity, field/lifecycle conflicts, crash boundaries,
reached-corruption checks, and separated direct-Linux latency.

The [native reverse hash-scan
evidence](evidence/native-hash-reverse-scan-linux-2026-08-03.md) adds
descending exact-byte scans over private, retained, and physical state;
exclusive live/dead cursors; whole-hash TTL; height-two reverse B+tree
pruning; early stop; fail-closed reached corruption; and a direct-Linux
comparison against full ascending materialization.

The [native hash pattern-scan
evidence](evidence/native-hash-pattern-scan-linux-2026-08-03.md) adds one
bounded binary-glob grammar with independent output, visit, and matcher-step
budgets; exact and leading-prefix physical routes; empty-page progress;
TTL/reopen equivalence; and reached-corruption handling. Its receipt retains
the negative leading-wildcard result as an optimization target.

The [native hash field TTL
evidence](evidence/native-hash-field-ttl-linux-2026-08-03.md) adds absolute
per-field expiry, the accepted WAL opcode `EXPIRE_HASH_FIELD=32`, the ordered
`0x0c` namespace, visibility across every hash read surface, expiry-clearing
mutations, field/lifecycle conflicts, combined active cleanup, crash
boundaries, compaction, and matched direct-Linux latency. Relative and
conditional field expiry, field persist/batches, floating counters, other
collection-family TTL, the complete G3 suite, and G7 remain open.

The [native hash randomized-model
evidence](evidence/native-hash-randomized-model-linux-2026-08-03.md) executes
the frozen dependency-free state machine across 16 fixed seeds, 4,096 actions,
4,524,373 comparisons, every hash read surface, and 128 reopen cycles. It also
retains a perturbed-oracle negative control and the physical field-TTL
contract correction exposed by the corpus. Other structure-family models,
concurrent histories, memory amplification, complete G3, and G7 remain open.

The [native set algebra
evidence](evidence/native-set-algebra-linux-2026-08-03.md) implements the
frozen bounded exact-byte `UNION`, `INTERSECTION`, and ordered `DIFFERENCE`
contract over private, retained, and current-root physical set state. It binds
logical-time expiry, restart equivalence, deterministic smallest-set
intersection, reached-corruption failures, hard output/visit limits, and
direct-Linux latency. Small embedded cases measured 1.838–6.013 microseconds
at p50; current-root physical cases measured 61.645–93.838 microseconds at
p50. Large union and difference remained cardinality-sensitive at 2.605 and
6.784 milliseconds p50. Store variants, sorted-set algebra/TTL, streams,
complete G3, and G7 remain open.

The [native whole-set TTL
evidence](evidence/native-set-ttl-linux-2026-08-03.md) implements the frozen
absolute complete-set expiry contract across membership, cardinality, bounded
algebra, snapshots, physical reads, restart, lifecycle conflicts, due-key
reuse, and shared cleanup. It binds additive WAL opcodes `EXPIRE_SET=33` and
internal `DELETE_SET=34`, backward-readable `HYSETM02` metadata, expiry marker
`3`, group durability, seven cleanup crash boundaries, corruption rejection,
compaction, page vacuum, and matched direct-Linux latency. Relative or
conditional expiry, persist, per-member TTL, destination algebra, complete
G3, and G7 remain open.

The [native whole-set lifecycle
evidence](evidence/native-set-lifecycle-linux-2026-08-03.md) promotes the
existing internal `DELETE_SET=34` path to an explicit embedded operation.
It proves complete retirement, retained history, same-transaction recreation
as every implemented structure family, lifecycle conflicts, all seven
singleton delete and replacement crash boundaries, fail-closed corruption,
compaction, page vacuum, and direct-Linux cardinality-sensitive latency.
Generic `DEL`, protocol compatibility, process-kill/block replay, complete
G3, and G7 remain open.

The [native whole-list lifecycle
evidence](evidence/native-list-lifecycle-linux-2026-08-03.md) adds
`DELETE_LIST=35` over the native chunked deque. It proves complete metadata
and chunk retirement, retained history, all typed recreations, whole-list
first-committer-wins conflicts, 14 singleton crash boundaries, multichunk
blob retirement, fail-closed corruption, compaction, page vacuum, blob
collection, reopen, and direct-Linux cardinality-sensitive latency.
Process-kill/block replay, blocking operations, streams, complete G3, and G7
remain open.

The [native whole-list TTL
evidence](evidence/native-list-ttl-linux-2026-08-03.md) implements absolute
complete-list expiry across both-end mutation, length/range reads, snapshots,
explicit-time physical reads, restart, lifecycle conflicts, due-key reuse,
and shared cleanup. It binds additive WAL opcode `EXPIRE_LIST=36`, existing
retirement opcode `DELETE_LIST=35`, backward-readable `HYLSTM02` metadata,
expiry marker `4`, Group durability, seven cleanup crash boundaries,
corruption rejection, compaction, page vacuum, blob collection, and repeated
matched direct-Linux latency. Relative or conditional expiry, persist,
per-element TTL, blocking/indexed/trim/move operations, complete G3, and G7
remain open.

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
fail-closed definition validation to one source commit. At that source commit,
catalog B+tree scaling, definition history, typed rows, and secondary indexes
remained open.

The [native scalable catalog B+tree
evidence](evidence/native-catalog-btree-2026-08-02.md) replaces the
single-page write path with `HYCAT003` ID and normalized-name namespaces,
inline/blob definition envelopes, buffered point lookup, V1/V2 migration,
copy-on-write incremental DDL, corruption rejection and Windows/WSL2 release
observations over 1,024 objects. ID and name lookup were observed in the
microsecond domain; strict DDL remained millisecond work. Drops, definition
history, dependency edges, schema evolution, cold/concurrent/saturated
behavior and the complete G0/G1/G7 evidence remain open.

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
Subtree-count order-statistic acceleration, algebra, TTL, protocol exposure,
model testing, amplification evidence, the complete G3 suite, and G7 remain
open.

The [native sorted-set score-range
evidence](evidence/native-sorted-set-score-ranges-linux-2026-08-02.md) adds
inclusive, exclusive, unbounded, empty, and inverted score bounds with live
offset/limit semantics across private, retained, current-root, and reopened
execution. The current-root path maps canonical binary64 bounds directly onto
the ordered B+tree namespace, prunes nonintersecting subtrees, ignores
tombstones without charging offset, stops at the requested live result count,
and fails closed on malformed ordered identities, scores, and markers. Its
direct-Linux observation measures one warm, bounded physical range.

The [native sorted-set member-rank
evidence](evidence/native-sorted-set-ranks-linux-2026-08-02.md) adds zero-based
`ZRANK` and `ZREVRANK` across private, retained, current-root, and reopened
execution. The physical path resolves the member score through the membership
index, walks the ordered B+tree toward the target in the requested direction,
ignores tombstones, stops at the live target, and fails closed on forged
metadata, scores, identities, or markers. Its direct-Linux observation
characterizes head, middle, and tail costs over 2,048 members.

The [native sorted-set reverse-range
evidence](evidence/native-sorted-set-reverse-ranges-linux-2026-08-02.md) adds
signed-rank `ZREVRANGE` and bounded-score `ZREVRANGE_BY_SCORE` across private,
retained, current-root, and reopened execution. Both reverse the complete
score/member order, count only live entries, and stop at the requested result
boundary. The physical paths start at the ordered prefix tail or traverse only
the bounded score interval in reverse, reject forged metadata/markers, and do
not materialize the complete sorted set. Subtree live counts, algebra, TTL,
protocol exposure, model testing, amplification evidence, the complete G3
suite, and G7 remain open.

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

The [native page-generation vacuum
evidence](evidence/native-page-vacuum-2026-08-02.md) closes that immediate
physical-reclamation gap for the current-root policy. It binds
generation-aware pages, WAL and root manifests; exact cross-engine/ANN
equality; detached-writer retention-floor rejection; six interruption
boundaries; orphan cleanup; and a clean Windows release receipt. The measured
64-row, nine-version corpus reclaimed 97.989% of page-file bytes while
preserving a 1.000 microsecond warm point-read p50. Strict vacuum itself
measured 29.658 milliseconds and the immediate no-op measured 12.672
milliseconds, so neither is claimed as a microsecond maintenance path.
Multi-generation retention, blob/WAL collection, background scheduling, and
the complete G1/G7 matrices remain open.

The [native dependency-closure
evidence](evidence/native-dependency-closure-2026-08-02.md) makes the non-dev
normal/build graph rooted at `hyphae-native-runtime` an exact fail-closed
allowlist. Its clean WSL2 receipt contains 11 Hyphae-owned packages, 19
reviewed external primitive/build packages, no forbidden engine, and zero
native unsafe findings. Reported third-party unsafe syntax still requires
semantic review, and the remaining golden/corpus/conformance requirements keep
G0 open.

The [native group-commit
evidence](evidence/native-group-commit-2026-08-02.md) adds a bounded
multi-producer scheduler, independent per-request admission, private MVCC root
chains, one shared page sync and WAL sync, per-request timing receipts, orderly
shutdown and a five-boundary crash matrix. Its exact eight-producer corpus
improved throughput by 3.502910 times on Windows and 1.654654 times under
WSL2/tmpfs while increasing individual end-to-end p50 latency. Queue wait
remained in the microsecond domain; cohort execution remained millisecond
work. Mixed-durability scheduling, native-ext4/power-loss evidence and the
complete G1/G7 matrices remain open.

The [native mixed-durability scheduler
evidence](evidence/native-mixed-scheduler-2026-08-02.md) puts strict, group,
and memory commits behind one bounded FIFO writer policy. It adds exact queued
cancellation/deadlines, immediate saturation, non-blocking admission locking,
durability barriers, and separate admission/queue/execution/sync timing. Under
six group producers plus one strict and one memory producer, all 256 requests
completed and reopened behind strict fence CSN 257. End-to-end p50 remained
millisecond-scale on Windows debug and WSL2 release; queue wait is now the
dominant measured pain point. Broader maintenance scheduling,
native-ext4/power-loss evidence and the complete G1/G7 matrices remain open.

The [native active-expiry scheduler
evidence](evidence/native-active-expiry-scheduler-2026-08-02.md) moves bounded
scalar cleanup into that same writer without a second database handle or
thread. It proves idle scheduling, one-CSN non-empty sweeps, empty no-ops,
non-decreasing clock authority, strict and memory durability, continuous group
fairness, typed terminal failure, shutdown ordering, and reopen. The matched
release benchmark drained 512 keys in eight transactions and retained all 256
foreground commits. Its WSL2 enabled p50 remained 1.679 milliseconds, while
p95 and p99 increased in the single run and 119 of 127 attempts were empty.
Adaptive backoff, additional maintenance classes, native-ext4/power-loss
evidence and the complete G1/G3/G7 matrices remain open.

The [native primary-key left-prefix scan
evidence](evidence/native-primary-key-prefix-scans-2026-08-02.md) binds the
longest strict composite-key prefix to one canonical half-open B+tree
interval, preserves residual-before-limit semantics, and proves private,
retained, current-root, corrupted-row, and reopened behavior. The clean
schema-v13 WSL2 observation measured pure prefix `LIMIT 10` at 14.557
microseconds p50 and 78.233 microseconds p99. A next-column range left as a
residual measured 198.603 microseconds p50 after examining 512 rows, so
prefix-plus-range planning, secondary ranges, broader SQL, and the remaining
G2/G7 matrices remain open.

The [native primary-key prefix-plus-range
evidence](evidence/native-primary-key-prefix-ranges-2026-08-02.md) replaces
that measured residual with an interval over the equality prefix and the
immediately following primary-key component. It proves inclusive/exclusive
endpoints with remaining composite-key suffixes, residual-before-limit,
private/retained/current/reopened equivalence, typed empty/failure cases, and
fail-closed corruption. The clean schema-v14 WSL2 observation measured the
former lower-bound residual case at 15.119 microseconds p50 and 40.127
microseconds p99. This is an operator observation, not G2 or G7; secondary
ranges, general expressions/planning, and the complete correctness and
performance matrices remain open.

The [native secondary-index range
evidence](evidence/native-secondary-index-ranges-2026-08-02.md) replaces the
length-first secondary identity assumption with the order-preserving
`HYRIDX02` layout and executes the first bounded physical secondary-index
range scans. Range planning is admitted only from persisted physical
metadata; legacy `HYRIDX01` indexes keep exact lookup and fall back to a
legal bounded primary-key scan instead of a false range plan. The evidence
proves inclusive/exclusive/one-sided/empty/`NULL` endpoints, complete simple
and composite keys, residual-before-limit, private/retained/current/reopened
equivalence, and fail-closed malformed-identity and forged-projection cases.
The clean schema-v15 WSL2 observation measured the ordered secondary range
at 24.328 microseconds p50 and 58.527 microseconds p99 while its unindexed
differential baseline returned the same ten rows at 7,971.720 microseconds
p50. This is an operator observation, not G2 or G7; composite
equality-prefix secondary ranges, descending and streaming execution, and
the complete correctness and performance matrices remain open.

The [native composite secondary prefix-range
evidence](evidence/native-secondary-index-prefix-ranges-linux-2026-08-02.md)
binds a nonempty strict `HYRIDX02` equality prefix plus lower/upper bounds on
the immediately following index column. It proves canonical bounds across
remaining index suffixes and primary-key ties, residual-before-limit
behavior, private/retained/current/reopened equivalence, legacy fallback,
false-plan rejection, and fail-closed malformed identities and forged
projections. Its schema-v16 Linux observation uses a second isolated native
database so the inherited schema-v15 corpus and measurement order remain
unchanged. After eliminating duplicate row decoding, it measured 46.535
microseconds p50 and 98.370 microseconds p99, meeting both halves of the
provisional bounded indexed-SQL target in this one warm scenario. This is an
operator observation, not G2 or G7; overlapping-index cost selection,
descending and streaming execution, and the complete correctness and
performance matrices remain open.

The [native ext4 Linux baseline
evidence](evidence/native-ext4-linux-baseline-2026-08-02.md) executes the
same schema-v15 smoke on native Linux for the first time, with the
benchmark data directory on persistent ext4 rather than tmpfs. It opens
the native-ext4 observation lane that the retention milestones name where
"native-ext4/power-loss evidence remain open", but the run is warm,
memory-durability, and concurrency one and does not fsync, so it covers
neither power-loss nor physical-durability evidence. It closes no gate;
its numbers are a new devbox baseline that is not run-to-run comparable
with the WSL2 or Windows receipts.

The [native lineage ext4 latency
evidence](evidence/native-lineage-ext4-latency-2026-08-02.md) repeats the
schema-v15 embedded/local-frame smoke directly on that Linux host for the
exact lineage-bearing source tree merged by PR 53. All 20 hot or indexed
routes remain below one millisecond through p99.9, and the local-frame route
observes p50/p99 `0.104/0.121 us`. This satisfies the first latency
observation for that source tree but does not time the one-CSN commit, strict
durability, UDS/named-pipe transport, or power loss and closes neither G1 nor
G7.

The [native local UDS
evidence](evidence/native-local-uds-linux-2026-08-03.md) adds the first real
filesystem-backed transport below `HYPHLCL1`: bounded reusable frame buffers,
fail-closed truncation and allocation checks, exact `0600` endpoint
permissions, identity-safe cleanup, and an ordered persistent connection.
Three direct-Linux release observations put the median persistent `PING`
round trip at p50 `23.261 us`, p99 `35.290 us`, and p99.9 `44.631 us`. This
removes the explicit no-UDS-receipt deficit from the minimal G1 vertical, but
does not close Windows named-pipe or complete session semantics, establish a
regression threshold, or close G1, G6, or G7.

The [native local structure GET
evidence](evidence/native-local-structure-get-linux-2026-08-03.md) adds the
first engine-bearing UDS operation. Canonical binary payloads, stable
request-local failures, server-authoritative TTL time, and direct physical
B+tree reads retain stream/request identity on one persistent connection.
Across three direct-Linux release runs, the median embedded physical read was
p50/p99 `0.816/1.588 us`; the complete `STRUCTURE GET` round trip was
`23.466/35.939 us`. This bounded observation is below the provisional
`25/100 us` local-protocol target, but it is warm, virtualized, concurrency
one, may allocate the returned value, and lacks the G7 cold/saturation/
interference/allocation/hardware-counter matrix. `SET`, TTL commands, SQL,
search, transactions, complete session semantics, and Windows named pipes
remain open.

The [native local structure SET and TTL
evidence](evidence/native-local-structure-set-ttl-linux-2026-08-03.md)
extends that serial session with canonical binary mutation/TTL payloads,
strict and memory durability receipts, exact transaction ID/CSN identity,
controlled expiry, request-local recovery, and strict reopen equivalence.
Across three direct-Linux runs, median physical TTL p50/p99 was
`0.832/0.886 us`; persistent GET and TTL round trips were
`23.546/34.286 us` and `23.487/34.489 us`. Memory `SET` was
`377.113/401.059 us`, while strict `SET` was
`6,738.743/6,928.074 us`. Those mutation results expose unfinished hot-path
and physical-durability work rather than satisfying G7. Group durability,
replay/idempotency, explicit transactions, SQL/search operations, complete
session semantics, Windows named pipes, and the G7 matrix remain open.

The [native local SEARCH MATCH
evidence](evidence/native-local-search-match-linux-2026-08-03.md) adds the
first search-engine operation to that UDS session. It binds a nonzero catalog
identity, bounded UTF-8 query, visible all-engine CSN, positive finite BM25
scores, and strict score/document ordering to the physical inverted index.
Across three direct-Linux runs over 2,048 documents, median physical MATCH
p50/p99 was `23.346/32.704 us`; the complete one-hit UDS round trip was
`56.150/68.327 us`, with independent PING at `23.358/33.728 us`. The receipt
does not subtract those distributions or close G4/G6/G7. SQL operations
beyond bounded prepared SELECT, document mutation, ANN/hybrid search,
streaming, Windows named pipes, concurrency, saturation, allocation, and
hardware-counter lanes remain open.

The [native local SQL SELECT
evidence](evidence/native-local-sql-select-linux-2026-08-03.md) adds the first
relational-engine operation to the same UDS session. It retains bounded
prepared plans and canonical typed parameters, then returns the complete
logical schema, typed rows, and visible all-engine CSN from direct physical
primary, secondary, bounded-scan/range, and indexed-join access paths. Across
three direct-Linux runs over 2,048 rows, median embedded prepared primary-key
SELECT p50/p99 was `1.878/2.022 us`; the one-row UDS EXECUTE round trip was
`21.924/32.976 us`, with independent PING at `23.365/33.780 us`. The UDS p50
being lower than PING is not negative overhead: independent percentiles are
not subtracted. DDL/DML over the protocol, explicit all-engine transactions,
streaming, Windows named pipes, concurrency, saturation, allocation, hardware
counters, and complete G2/G5/G6/G7 evidence remain open.

The [native local all-engine transaction
evidence](evidence/native-local-all-engine-transaction-linux-2026-08-03.md)
adds one explicit serial transaction over the existing detached optimistic
batch. SQL DML, scalar SET, and lexical document indexing stage under one
fixed read CSN and server time, then publish through one WAL transaction and
one commit CSN. Prior snapshots see none of the writes; strict reopen sees all
three. Wrong handles/counts, empty commit, semantic failure, response
preflight, rollback, close, peer loss, the 1,024-operation bound, optimistic
conflict, and all seven commit interruptions fail closed. Across three
direct-Linux runs, median SQL/structure/search stage p50 was
`24.415/22.364/24.271 us`. Memory and strict commit p50 was
`6.476/15.097 ms`, exposing unfinished publication and synchronization work.
This completes the minimal transaction proof required by G1 step 4 and
advances G5/G6, but it does not prove concurrent-reader mixing, cross-engine
SQL operators, backup/restore, Windows named pipes, or the G7 matrix and
closes no complete gate.

The [native delta all-engine transaction
evidence](evidence/native-delta-all-engine-transaction-linux-2026-08-03.md)
replaces complete engine-state materialization on that local transaction hot
path with one bounded physical delta. HYCAT004 point-resolves the named
relation and its exact secondary-index dependencies; SQL, scalar structure,
and immutable lexical overlays revalidate and publish through the existing
WAL transaction and one CSN. Deterministic guards reject any hot-path complete
engine or catalog load, latest and historical reads stop at the first visible
row version, full verification still rejects corrupt older links, and all
seven commit interruptions remain never-mixed. Across three CPU-0
direct-Linux runs, median memory/strict commit p50 is `1.001241/9.388397 ms`;
depth from 1 through 1,024 prior versions retains nine page reads, three page
appends, one 65,536-byte WAL block, and zero complete state/catalog loads.
This closes the bounded delta slice and removes the known materialization
bottleneck. It does not close G1, G5, G6, or G7: memory commit remains outside
the microsecond domain, and cold/concurrent/saturated performance,
per-operation allocation bounds, cross-engine SQL operators, backup/restore,
replication, and complete phase evidence remain open.

The [native lexical document lifecycle
evidence](evidence/native-search-document-lifecycle-linux-2026-08-03.md)
adds exact replacement and deletion to the point-resolved lexical delta and
local transaction surface. Search WAL opcodes `37` and `38`, the atomic
`HYSEABT1` to `HYSEABT2` upgrade, exact document/term/posting tombstones,
same-document conflict identity, historical BM25 equivalence, fourteen commit
interruptions, large-blob reclamation, and fail-closed V1/malformed tombstones
are implementation-gated. Across three CPU-0 direct-Linux runs, staging p50
remained between `22.469 us` and `141.813 us`. At 4,096 unrelated documents,
median memory commit p50 was `1.295983 ms` for replacement and `1.033210 ms`
for deletion; strict commit p50 was `9.670471/8.966219 ms`. Every measured
transaction appended one 65,536-byte WAL block and performed zero complete
engine-state or catalog loads. This removes mutable lexical documents from
the bounded-delta gap but closes no complete gate; hosted stack checks, search
tombstone compaction, broad query semantics, cross-engine SQL, and complete
phase evidence remain open.

The [native lexical tombstone compaction
evidence](evidence/native-search-tombstone-compaction-linux-2026-08-03.md)
adds explicit current-root `HYSEABT2` reachability compaction under search WAL
opcode `39`. Complete lexical and catalog-bound ANN validation precedes writer
admission; exact document, term, and posting tombstones are omitted while
every retained lexical and ANN byte, historical root, and non-search engine
root remains unchanged. V1/no-tombstone roots advance no page, WAL identity,
transaction ID, or CSN. Six focused equivalence tests, two public integration
tests, all seven commit interruptions, corruption-before-append gates, and the
compaction/vacuum/retention/blob sequence pass directly on Linux. Across three
CPU-0 runs, validated-plan p50 was `9.925-29.915 ms`; applied memory/strict
compaction p50 was `14.821-56.681/20.683-64.283 ms` across 256 and 4,096
documents at 25% and 75% tombstones. Applied work wrote one 65,536-byte WAL
block and appended 3-52 replacement pages. This closes explicit lexical
tombstone compaction, not automatic policy, background merge, broad search,
cross-engine SQL, or any complete phase gate.

The [native bounded-WAL-replay
evidence](evidence/native-wal-replay-2026-08-02.md) adds fixed-size
`HYWAR001` retention anchors, absolute retained block/LSN identity, explicit
stage/pending/stable publication, six-boundary recovery, suffix-only semantic
replay, and fail-closed anchor/suffix validation. On the matched 402-commit
prefix plus four-commit suffix corpus it removed 98.7775% of WAL bytes and
reduced warm reopen p50 by 13.293409 times on Windows and 15.461882 times under
WSL2/tmpfs. Pin registration, native-ext4/power-loss
evidence and the complete G1/G7 matrices remain open.

The [native manifest-retention
evidence](evidence/native-manifest-retention-2026-08-02.md) makes the same
`HYWAR001` anchor the exact manifest-chain trust root, preserves absolute
generation/digest identity, tolerates partially deleted lower prefixes, fails
closed from the retained base onward, and extends retention to seven crash
boundaries. On the matched generation-131 corpus it reduced manifest files
from 131 to 3 and bytes by 97.5504%. Manifest verification p50 improved by
38.218558 times on Windows/NTFS and 48.024295 times under WSL2/tmpfs. The
current-root policy, physical durability, pins and complete G1/G7 matrices
remain open.

The [native immutable-blob collection
evidence](evidence/native-blob-collection-2026-08-02.md) adds exact
all-engine liveness tracing, a committed generation floor independent from
post-collection file count, digest-ordered idempotent deletion, strict
single-root eligibility, and four interruption boundaries. On the matched
130-file corpus it removed 128 files and 97.0063% of encoded blob bytes while
preserving relational, scalar, lexical, ANN, and typed-catalog state. Blob
verification p50 improved by 26.467297 times on Windows/NTFS and 39.915948
times under WSL2/tmpfs. Multi-root pins, native-ext4/power-loss evidence,
automatic scheduling, and complete G1/G7 matrices remain open.

The [native lineage-threading
evidence](evidence/native-lineage-threading-linux-2026-08-02.md) makes the
directory UUIDv7 plus history epoch part of `HYROOT03` manifests and
`HYWAR002` retention anchors. Direct Linux tests preserve historical codec
bytes while rejecting legacy, mixed, and marker-divergent recovery authority
before retained WAL state can be selected or reset. A follow-up exhausts all
truncated prefixes and single-byte corruptions in both lineage-bearing
authority fixtures, for 1,088 deterministic corrupt inputs. Offline
promotion, a sanctioned epoch transition, physical power-loss evidence, and
the broader G1 matrix remain open.

The [native process crash matrix
evidence](evidence/native-process-crash-matrix-linux-2026-08-02.md) replaces
the singleton commit's in-process-only interruption claim with seven
`SIGKILL`/reopen cycles on direct Linux/ext4. A strict transaction mutates
relational, structure/TTL, and lexical state under one CSN while also
publishing a large immutable blob. The four pre-WAL boundaries reopen the
prior empty state; complete WAL append, WAL synchronization, and root
publication reopen the complete CSN 1. This is process-crash evidence, not
physical power loss: the kernel page cache survives, and checkpoint,
retention, maintenance, migration, resource-exhaustion, and storage-fault
lanes remain open.

The [native checkpoint process crash
evidence](evidence/native-checkpoint-process-crash-linux-2026-08-02.md)
extends that live-writer `SIGKILL` harness to all four checkpoint boundaries.
Recovery removes the staged temporary manifest, preserves a published
manifest as a non-authoritative suffix, and recognizes the complete
checkpoint record after either WAL boundary. All four reopen the same
complete all-engine CSN 1. Group commit, retention, maintenance, migration,
resource-exhaustion, filesystem-reordering, and physical power-loss lanes
remain open.

The [native block-layer power-loss replay
evidence](evidence/native-block-power-loss-replay-linux-2026-08-02.md)
reconstructs the worst stable ext4 image recorded by Linux `dm-log-writes`
through each of those seven commit and four checkpoint boundaries. Unlike
the process-kill lane, an appended but unsynchronized user WAL transaction
reopens the prior empty state, and an appended but unsynchronized checkpoint
WAL record leaves the published manifest as an unanchored suffix. Authority
begins only after WAL synchronization. All 11 replay images mount, recover the
required all-engine state, pass read-only `e2fsck`, and clean their isolated
loop/mapping resources. This closes the singleton/checkpoint block-ordering
slice, not literal EBS power removal, group commit, retention, maintenance,
migration, resource exhaustion, or the complete G1 matrix.

The [native durable snapshot-pin
evidence](evidence/native-snapshot-pins-linux-2026-08-02.md) adds an immutable
`HYPIN001` registry and exact named retention across relational, structure/TTL,
lexical, ANN, catalog, page, blob, manifest, and WAL authority. Three pinned
page generations reopen beside a fourth active generation; middle and final
unpin collections report exact removed/retained bytes. Schema-v3 process
evidence extends the existing harness to 13 `SIGKILL` cycles and proves staged
pin absence versus published pin completeness. The direct Linux workspace
funnel is green, but hosted CI, mutation testing, physical power loss, offline
promotion, and the broader G1 matrix remain open.

This evidence advances G0/G1 but closes neither gate. Relational table/primary
key storage now uses the native B+tree and canonical MVCC rows, and large row
values use native immutable blobs. The current implementation also retains
explicit per-key row-version chains with closed half-open intervals while
keeping V1 inline-row directories compatible. Catalog now retains complete
typed definitions in a multilevel native B+tree with separate ID/name
namespaces and definition blobs; new search roots use a native inverted
B+tree while legacy inline roots remain compatible. Detached transactions now
prepare concurrently and rebase disjoint all-engine writes under serialized
publication. Bounded simultaneous `group` submission now shares page/WAL
flushes, and current-root retention bounds WAL verification/replay to the
retained suffix. Explicit single-root maintenance now collects blobs
unreachable after page/WAL/manifest retirement. Mixed strict/group/memory
policy now has bounded-load concurrency, saturation, active-expiry fairness,
and terminal-failure evidence. Broader sustained fairness remains pending,
along with automated pin lifecycle policy and quotas,
cost-based overlapping secondary-index selection and streaming execution,
zero-copy relational operator cursors, general relational expressions beyond
the admitted residual slice, constraints/planning, remaining structure
families, positional postings, segments, buffered/filtered ANN, and hybrid
fusion required by G1–G4.
Typed point inserts, exact-PK update/delete, and catalog-bound
primary/secondary-key projection now use canonical tuples and
primitive/composite keys, but do not close G2. The structure keyspace has a
multilevel native B+tree, direct reads, tombstone/expiry mutations,
conditional writes, signed counters, independent-field hashes with bounded
ascending, descending, and binary-glob iteration, exact binary sets with
member-granular conflicts, chunked-deque lists, and dual-index sorted sets
with bounded bidirectional score/rank ranges, member-rank lookup, whole-hash
delete/recreate and TTL, bounded multi-field hash commands, signed hash-field
counters, independent absolute field TTL, bounded set algebra, and complete-set
TTL, plus complete-list delete/recreate and TTL. Subsequent G3 work added
durable sorted-set lifecycle/TTL, append-ordered streams with stable entry IDs
and TTL, active expiry for both families, crash matrices, seeded models for
every required family, an all-family restart matrix, and all-family atomic
conflict evidence. Floating counters, sorted-set algebra, relative/conditional
expiry, persist semantics, broader model alphabets and negative controls,
adaptive empty-expiry backoff, and a user-facing historical-retention policy
remain open. One cleanup transaction now retires all six families under one
CSN, and explicit sorted-set deletion retires its TTL index before reopen.
Ordered cleanup, current-root compaction, page-generation vacuum, and the
bounded physical-amplification test provide initial measurements, not a
complete production-scale memory claim.

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
