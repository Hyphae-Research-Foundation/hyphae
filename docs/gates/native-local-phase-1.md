# Native local ecosystem phase-1 gate

Status: in progress; G0 specifications are drafted but implementation and
exit evidence remain incomplete

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
composite secondary equality-prefix ranges and streaming execution,
zero-copy relational operator cursors, general relational expressions beyond
the admitted residual slice, constraints/planning, remaining structure
families, positional postings, segments, buffered/filtered ANN, and hybrid
fusion required by G1–G4.
Typed point inserts, exact-PK update/delete, and catalog-bound
primary/secondary-key projection now use canonical tuples and
primitive/composite keys, but do not close G2. The structure keyspace has a
multilevel native B+tree, direct reads, tombstone/expiry mutations,
conditional writes, signed counters, independent-field hashes, exact binary
sets with member-granular conflicts, chunked-deque lists, and dual-index
sorted sets. It still lacks whole-hash lifecycle/iteration, set and sorted-set
algebra/TTL, score/reverse/rank sorted-set operations, streams, adaptive
empty-expiry backoff, model tests, and a user-facing historical-retention
policy required to close G3. Ordered cleanup,
current-root compaction, and page-generation vacuum provide exact first
amplification measurements, not the complete memory-amplification gate.

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
