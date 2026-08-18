# Hyphae 0.2 local evidence

These machine-readable files are observations from local release-candidate
gates. They are not hosted Linux/macOS/Windows release evidence and do not
authorize publication.

- `0.2-retrieval-benchmark-*.json`: deterministic mixed retrieval benchmark
  under the [0.2 methodology](../../performance/retrieval-benchmark-0.2.md).
- `0.2-score-model-benchmark-*.json`: canonical score-model comparison used by
  ADR-0015.
- `0.2-load-gate-*.json`: concurrent public HTTP write and proof-bearing
  retrieval gate.
- `0.2-soak-gate-*.json`: kill/restart, index rebuild, backup, and restore
  gate.
- `0.2-fuzz-*.json`: bounded fuzz execution counts and crash status.
- `0.2-cargo-audit-*.json`: dependency vulnerability audit result.
- `0.2-dependency-review-local.json`: reviewed dependency and lockfile delta
  from the `0.1.0` baseline.

Environment details and command parameters live inside each report when the
producer supports them. A final hosted run must be tied to the exact release
commit before its tag can be published.

## Native phase-1 observations

### Retained gate closures

These reviewed summaries preserve hosted evidence after temporary workflow
artifacts expire. The SHA in each filename identifies the tested source
candidate; the summary is necessarily committed later and does not claim its
own commit as the evaluated source.

- [Native G0 exact-SHA closure](closures/native-g0-14b4ec9.json)
- [Native G1 exact-SHA closure](closures/native-g1-14b4ec9.json)
- [Native G2 exact-SHA closure](closures/native-g2-a839037.json)
- [Native G3 exact-SHA closure](closures/native-g3-a839037.json)
- [Native G4 exact-SHA closure](closures/native-g4-0059fce.json)
- [Native G5 exact-SHA closure](closures/native-g5-b7cf651.json)
- [Native G6 exact-SHA local-product closure](closures/native-g6-c57cc07.json)
- [Native G7 C-60 operational-scale closure](closures/native-g7-ff188af.json)
- [Native G8 exact-SHA release closure](closures/native-g8-e88f2ea.json)

### Source-bound observations

- [Native phase-1 kernel evidence — 2026-08-01](native-phase1-kernel-2026-08-01.md)
  records the first page/WAL/MVCC/catalog convergence vertical and its explicit
  remaining gates.
- `native-microsecond-smoke-windows.json` is a dirty-worktree, concurrency-one,
  batch-averaged observation. It is not named-pipe evidence and does not pass
  the microsecond performance gate.
- `native-microsecond-smoke-wsl2.json` repeats the same smoke on clean commit
  `85b7a4d` under WSL2. Commit binding improves reproducibility but the
  batch-average, tiny-corpus, transport, concurrency, and hardware-counter
  gaps still keep it outside the gate.
- [Native blobs, relational mutations, and conflict substrate evidence —
  2026-08-01](native-blobs-mutations-conflicts-2026-08-01.md) records the
  content-addressed blob store, UPDATE/DELETE tombstones, WAL-rebuilt point
  conflict table, expanded crash matrix, and their explicit concurrency and
  retention limits.
- `native-microsecond-smoke-multilevel-wsl2.json` binds a 2,049-row,
  height-two physical B+tree observation to clean commit `5a73795`. It remains
  outside G7 because timing is batch-averaged, concurrency is one, and
  transport/interference/hardware controls are absent.
- [Native borrowed point-read evidence —
  2026-08-01](native-borrowed-point-read-2026-08-01.md) removes owned
  per-node decoding from that height-two route and binds the matched
  sub-microsecond batch-average p50 observation to clean commit `7b0053c`.
- `native-microsecond-smoke-borrowed-read-wsl2.json` is the machine-readable
  matched receipt. It still does not pass G7 because individual-operation,
  concurrency, transport, interference, allocation, and hardware-counter
  controls remain absent.
- [Native relational version-chain evidence —
  2026-08-01](native-relational-version-chains-2026-08-01.md) binds immutable
  closed row histories and legacy V1 compatibility to one source commit.
- [Native optimistic-writer evidence —
  2026-08-01](native-optimistic-writers-2026-08-01.md) binds detached
  concurrent preparation, admitted-root rebase, first-committer-wins, and
  recovery to one source commit.
- [Native structure B+tree evidence —
  2026-08-01](native-structure-btree-2026-08-01.md) binds the first scalable
  scalar keyspace, direct buffered reads, TTL/blob envelopes, and legacy
  compatibility to one source commit.
- [Native scalar structure mutation evidence —
  2026-08-01](native-scalar-structure-mutations-2026-08-01.md) binds physical
  tombstones, `DELETE`, `EXPIRE`, `NX`/`XX`, signed counters, recovery, and
  their explicit remaining G3 limits.
- `native-microsecond-smoke-scalar-mutations-wsl2.json` is the matched clean
  read-path observation for that scalar-mutation source commit. It does not
  time mutations and remains outside G7.
- [Native hash structure evidence —
  2026-08-01](native-hash-structure-2026-08-01.md) binds the first compound
  structure family, field-granular storage/conflicts, cardinality validation,
  multilevel recovery, and explicit remaining G3 limits.
- `native-microsecond-smoke-hash-wsl2.json` is its schema-v5 clean physical
  `HGET` observation over 2,048 fields. It does not time mutations and remains
  outside G7.
- [Native set structure evidence —
  2026-08-01](native-set-structure-2026-08-01.md) binds the first exact binary
  set, member-granular storage/conflicts, cardinality validation, multilevel
  recovery, crash boundaries, and explicit remaining G3 limits.
- `native-microsecond-smoke-set-wsl2.json` is its schema-v11 clean physical
  `SISMEMBER` observation over 2,048 members. It does not time mutations and
  remains outside G7.
- [Native inverted-search evidence —
  2026-08-01](native-inverted-search-2026-08-01.md) binds the first physical
  collection/document/term/posting namespaces, prefix-pruned `MATCH`, exact
  reference-BM25 equivalence, multilevel recovery, corruption rejection, and
  explicit remaining G4 limits.
- `native-microsecond-smoke-search-wsl2.json` is its schema-v6 clean physical
  `MATCH` observation over 2,048 documents. Search uses one complete call per
  timer observation; the rare-term baseline remains outside G7.
- [Native ANN kernel evidence —
  2026-08-01](native-ann-kernel-2026-08-01.md) binds the first Hyphae-owned
  deterministic HNSW implementation, exact oracle, canonical rebuild,
  fail-closed restore and explicit persistence boundary to one source commit.
- `native-ann-kernel-wsl2.json` is its 10,000-vector, 32-dimensional,
  100-query WSL2 observation. Recall@10 passed the bounded 0.95 floor at
  0.970, but HNSW remained slower than exact search and the receipt does not
  pass G4 or G7.
- [Native ANN durability evidence —
  2026-08-01](native-ann-durability-2026-08-01.md) binds catalog, WAL,
  search-B+tree generations, batch rebuild, all-engine MVCC snapshots,
  optimistic conflicts, recovery/corruption behavior and the seven commit
  interruption boundaries to one source commit.
- `native-ann-durability-wsl2.json` is its 512-vector, 32-dimensional,
  10,000-observation-per-route WSL2 receipt. It measures a validated
  materialized snapshot at concurrency one, not direct buffered traversal, and
  therefore passes neither G4 nor G7.
- [Native canonical type-codec evidence —
  2026-08-01](native-type-codecs-2026-08-01.md) binds recursive type
  descriptors, checked primitive row payloads, memcomparable ordered-index
  components, explicit unsupported nested codecs, and cross-platform
  validation to one source commit.
- [Native catalog-definition evidence —
  2026-08-01](native-catalog-definitions-2026-08-01.md) binds canonical
  relation/structure/search definitions, full-definition WAL and `HYCAT002`
  persistence, legacy reconstruction, snapshot/reopen proof, and explicit
  single-page limits to one source commit.
- [Native scalable catalog B+tree evidence —
  2026-08-02](native-catalog-btree-2026-08-02.md) binds `HYCAT003` ID/name
  namespaces, definition blobs, V1/V2 migration, buffered lookup,
  copy-on-write DDL, corruption rejection and reopen to one source commit.
- `native-catalog-btree-windows.json` and
  `native-catalog-btree-wsl2.json` are matched release observations over
  1,024 objects and 50,000 calls per lookup route. They record microsecond
  point lookup and millisecond strict DDL, not a complete G7 gate.
- [Native typed SQL-row evidence —
  2026-08-01](native-typed-sql-rows-2026-08-01.md) binds catalog-typed DDL,
  canonical `HYTUPL01` rows, primitive and composite primary-key binding,
  typed prepared point reads, recovery, and historical binary compatibility
  to one source commit.
- [Native secondary-index evidence —
  2026-08-01](native-secondary-indexes-2026-08-01.md) binds canonical catalog
  definitions, physical relational B+tree namespaces, exact and composite SQL
  lookup, uniqueness, both optimistic index/row commit orders, recovery
  validation, and explicit remaining G2/G7 limits to one source commit.
- [Native direct secondary-index execution evidence —
  2026-08-01](native-direct-secondary-index-2026-08-01.md) binds catalog-only
  latest-plan preparation, current-root physical index-to-row execution,
  materialized/historical equivalence, stale-plan and corruption failures,
  reopen proof, and explicit remaining G2/G7 limits to one source commit.
- `native-microsecond-smoke-secondary-sql-wsl2.json` is its schema-v7 clean
  exact physical and prepared-SQL observation over a 2,048-row unique index.
  Each secondary timer sample is one complete call; the result remains outside
  G7.
- [Native typed indexed-mutation evidence —
  2026-08-01](native-typed-indexed-mutations-2026-08-01.md) binds typed
  exact-PK update/delete, atomic old/new projection maintenance, unique rebase,
  retained history, V1 compatibility, reopen, and a seven-boundary crash
  matrix to one source commit.
- [Native bounded relational-scan evidence —
  2026-08-01](native-bounded-relational-scan-2026-08-01.md) binds exclusive
  buffered prefix visitation, bounded current-root primary-key scan,
  transaction/materialized/physical SQL equivalence, V1/V2 reopen, typed
  failure paths, and explicit remaining G2/G7 limits to one source commit.
- `native-microsecond-smoke-relational-scan-wsl2.json` is its schema-v8 clean
  release observation for direct and prepared-SQL `LIMIT 10` scans over a
  multilevel 2,048-row typed relation. Each scan timer sample is one complete
  call; the result remains outside G7.
- [Native primary-key range evidence —
  2026-08-01](native-primary-key-ranges-2026-08-01.md) binds inclusive,
  exclusive and unbounded prefix-range visitation, public current-root
  primary-key bounds, composite SQL row comparisons, three-executor
  equivalence, empty-range safety, reopen/failure coverage, and explicit
  remaining G2/G7 limits to one source commit.
- `native-microsecond-smoke-primary-range-wsl2.json` is its schema-v9 clean
  release observation for direct and prepared-SQL `[1024, 1034)` primary-key
  ranges over a multilevel 2,048-row typed relation. Each range timer sample
  is one complete call; the result remains outside G7.
- [Native SQL residual-filter evidence —
  2026-08-01](native-sql-residual-filters-2026-08-01.md) binds parameterized
  scalar comparison, `IS [NOT] NULL`, SQL three-valued boolean logic and
  precedence, exact/range access extraction, post-filter `LIMIT`,
  transaction/materialized/physical/reopen equivalence, and explicit remaining
  G2/G7 limits to source and benchmark commits.
- `native-microsecond-smoke-residual-filter-wsl2.json` is its schema-v10 clean
  release observation for a prepared primary-key range plus alternating
  boolean residual filter over a multilevel 2,048-row typed relation. It
  records complete calls that return 10 matches and remains outside G7.
- [Native SQL scalar-literal evidence —
  2026-08-01](native-sql-scalar-literals-2026-08-01.md) binds catalog-typed
  `NULL`, boolean, signed-integer and escaped text literals, retained physical
  access paths, three-valued logic, materialized/current-root/reopen
  equivalence, and explicit remaining G2 limits to one source commit.
- [Native SQL mutation-literal evidence —
  2026-08-01](native-sql-mutation-literals-2026-08-01.md) binds the same scalar
  operands to `INSERT`, exact-primary-key `UPDATE` and `DELETE`, including
  fail-before-write behavior, index maintenance, recovery, and explicit
  remaining G2 limits to one source commit.
- [Native indexed inner-join evidence —
  2026-08-01](native-indexed-inner-join-2026-08-01.md) binds the first exact
  qualified `INNER JOIN`, primary/unique-secondary left access,
  single-primary-key right access, three-executor historical/private/physical
  equivalence, reopen and typed failure behavior to one source commit.
- `native-indexed-inner-join-wsl2.json` is its clean release observation over
  2,048 rows per relation and 100,000 complete calls per route. It remains
  outside G2 and G7.
- [Native bounded inner-join evidence —
  2026-08-01](native-bounded-inner-join-2026-08-01.md) binds full and ranged
  left primary-key inputs, output-level limits, private/historical/physical
  equivalence, early stop, reopen, and typed failure paths to one source
  commit.
- `native-bounded-inner-join-wsl2.json` is its schema-v2 clean release
  observation for exact and `LIMIT 10` joins over 2,048 rows per relation and
  100,000 complete calls per route. It remains outside G2 and G7.
- [Native secondary-index inner-join evidence —
  2026-08-01](native-secondary-inner-join-2026-08-01.md) binds a non-unique
  secondary equality to bounded canonical left traversal, output-level early
  stop, private/historical/physical equivalence, reopen, and typed failure
  paths at one source commit.
- `native-secondary-inner-join-wsl2.json` is its schema-v3 clean release
  observation for exact, primary-scan `LIMIT 10`, and secondary `LIMIT 10`
  joins over 2,048 rows per relation and 100,000 calls per route. It remains
  outside G2 and G7.
- [Native right secondary-index inner-join evidence —
  2026-08-01](native-right-secondary-inner-join-2026-08-01.md) makes right
  access explicit and binds an exact single-column `UNIQUE` secondary lookup
  across private, retained, physical and reopened execution.
- `native-right-secondary-inner-join-wsl2.json` is its schema-v4 clean release
  comparison of primary-key and unique-secondary right lookups over 2,048
  rows and 100,000 calls per route. It remains outside G2 and G7.
- [Native composite inner-join evidence —
  2026-08-01](native-composite-inner-join-2026-08-01.md) binds exact composite
  primary and `UNIQUE` secondary right keys, catalog-order alignment,
  private/retained/physical/reopened equivalence, null semantics, and typed
  fail-closed behavior to one source commit.
- `native-composite-inner-join-wsl2.json` is its schema-v5 clean release
  comparison of one-column and two-column unique-secondary right lookups over
  2,048 rows and 100,000 calls per route. It remains outside G2 and G7.
- [Native chunked-list evidence —
  2026-08-01](native-list-2026-08-01.md) binds typed deque semantics, packed
  end chunks, blob-backed elements, MVCC history, strict reopen, whole-list
  FCW, corruption rejection, and all seven crash boundaries to one source
  commit.
- `native-list-wsl2.json` is its clean release observation for physical
  `LLEN` and ten-element ranges from both ends of a 2,048-element list. It
  remains outside G3 and G7.
- [Native dual-index sorted-set evidence —
  2026-08-01](native-sorted-set-2026-08-01.md) binds canonical binary64
  ordering, exact member and ordered physical indexes, member-level FCW,
  retained snapshots, corruption rejection, and all seven crash boundaries
  to one source commit.
- `native-sorted-set-wsl2.json` is its clean release observation for physical
  `ZCARD`, middle-member `ZSCORE`, and ten-entry head `ZRANGE` over 2,048
  members. It remains outside G3 and G7.
- [Native sorted-set score-range evidence —
  2026-08-02](native-sorted-set-score-ranges-linux-2026-08-02.md) binds
  inclusive, exclusive, unbounded, empty, and inverted score bounds, live
  offset/limit semantics, direct physical pruning, execution-mode
  equivalence, and fail-closed decoding to one source commit.
- `native-sorted-set-score-range-linux.json` is its direct-Linux release
  observation for a ten-member middle score range over 2,048 members. It
  remains outside G3 and G7.
- [Native sorted-set member-rank evidence —
  2026-08-02](native-sorted-set-ranks-linux-2026-08-02.md) binds
  bidirectional zero-based ranks, reverse physical B+tree traversal,
  execution-mode equivalence, live-only counting, and fail-closed decoding to
  one source commit.
- `native-sorted-set-rank-linux.json` is its direct-Linux position-sensitive
  release observation over 2,048 members. It remains outside G3 and G7.
- [PR #59–#61 direct-Linux integration evidence —
  2026-08-02](native-pr59-pr60-pr61-integration-linux-2026-08-02.md) proves
  exact ancestry for the SQL secondary-prefix, sorted-set score-range, and
  sorted-set member-rank heads; combined runtime/documentation resolution;
  direct-Linux gates; and same-corpus latency observations.
- `native-pr59-pr60-pr61-integration-sql-linux.json`,
  `native-pr59-pr60-pr61-integration-score-linux.json`, and
  `native-pr59-pr60-pr61-integration-rank-linux.json` are the clean
  single-process observations from the integrated source commit. They remain
  outside G2, G3, and G7.
- [Native sorted-set reverse-range evidence —
  2026-08-02](native-sorted-set-reverse-ranges-linux-2026-08-02.md) binds
  descending signed-rank and bounded-score ranges, complete tie reversal,
  execution-mode equivalence, reverse physical pruning, live-only
  rank/offset accounting, and fail-closed physical decoding to one source
  commit.
- `native-sorted-set-reverse-range-linux.json` is its direct-Linux release
  observation over 2,048 members. It remains outside G3 and G7.
- [Native bounded hash-scan evidence —
  2026-08-02](native-hash-scan-linux-2026-08-02.md) binds exact binary field
  order, exclusive cursor behavior, execution-mode equivalence, bounded
  physical prefix traversal, live-only result accounting, and fail-closed
  reached-state decoding to one source commit.
- `native-hash-scan-linux.json` is its direct-Linux release observation over
  2,048 fields. It remains outside G3 and G7.
- [Native whole-hash lifecycle evidence —
  2026-08-02](native-hash-lifecycle-linux-2026-08-02.md) binds typed
  delete/recreate, lifecycle-fence concurrency, physical prefix tombstoning,
  fail-closed replay, all seven singleton crash boundaries, compaction, and
  separated private/memory/strict latency to one source commit.
- `native-hash-lifecycle-linux.json` is its direct-Linux release observation
  over 0-, 64-, and 2,048-field hashes. It remains outside complete G3 and G7.
- [Native whole-hash TTL evidence —
  2026-08-03](native-hash-ttl-linux-2026-08-03.md) binds absolute whole-family
  expiry, compatible `HYHSHM01`/`HYHSHM02` metadata, a typed shared expiry
  index, deterministic visibility, cross-family reuse, lifecycle conflicts,
  mixed active cleanup, crash boundaries, and compaction to direct Linux.
- `native-hash-ttl-linux.json` records separated private/snapshot/physical
  TTL, physical `HGET`, memory/strict expiry commit, hash cleanup, and a
  matched parent/current persistent-read control. It remains outside complete
  G3 and G7.
- [Native hash field commands evidence —
  2026-08-03](native-hash-field-commands-linux-2026-08-03.md) binds bounded
  multi-field read/set/delete, a signed field counter, canonical mutation
  order, failure atomicity, field conflicts, lifecycle fencing, crash
  boundaries, and reached-corruption checks to direct Linux.
- `native-hash-field-commands-linux.json` records snapshot/private/physical
  batch reads, a same-corpus singular-read control, memory/strict mutations,
  and a matched parent/current persistent-read control. It remains outside
  complete G3 and G7.
- [Native reverse hash-scan evidence —
  2026-08-03](native-hash-reverse-scan-linux-2026-08-03.md) binds descending
  exact-byte order, exclusive live/dead cursors, private/snapshot/physical
  equivalence, whole-hash TTL, height-two reverse pruning, early stop, and
  reached-corruption handling to direct Linux.
- `native-hash-reverse-scan-linux.json` records the native reverse visitor
  against a full ascending materialize/reverse/truncate fallback plus repeated
  pinned parent/current HGET controls. It remains outside complete G3 and G7.
- [Native hash pattern-scan evidence —
  2026-08-03](native-hash-pattern-scan-linux-2026-08-03.md) binds one bounded
  binary-glob grammar, exact and leading-prefix physical routes, sparse
  empty-page progress, matcher budgets, TTL/reopen equivalence, early stop,
  and reached-corruption handling to direct Linux.
- `native-hash-pattern-scan-linux.json` records a 32-visit prunable-prefix
  route, a 1,985-visit leading-wildcard route, their full-HSCAN application
  fallbacks, and repeated pinned parent/current HGET controls. It preserves
  the leading-wildcard slowdown as an explicit optimization target and
  remains outside complete G3 and G7.
- [Native hash field TTL evidence —
  2026-08-03](native-hash-field-ttl-linux-2026-08-03.md) binds absolute
  per-field expiry, compatible `HYSTRV01` envelopes, accepted WAL opcode 32,
  the collision-free `0x0c` index, all hash read surfaces, field/lifecycle
  conflicts, bounded combined cleanup, crash boundaries, and compaction to
  direct Linux.
- `native-hash-field-ttl-linux.json` records separated TTL, `HGET`, no-due and
  due `HLEN`, memory/strict mutation, 64-field cleanup, and repeated isolated
  parent/current HGET controls. It retains invalid preliminary observations
  as exclusions and remains outside complete G3 and G7.
- [Native whole-set TTL evidence —
  2026-08-03](native-set-ttl-linux-2026-08-03.md) binds absolute complete-set
  expiry, compatible `HYSETM01`/`HYSETM02` metadata, typed shared cleanup,
  set-algebra visibility, lifecycle conflicts, group durability, all seven
  cleanup crash boundaries, corruption rejection, compaction, and page vacuum
  to direct Linux.
- `native-set-ttl-linux.json` records separated private/snapshot/physical TTL,
  physical `SISMEMBER`, memory/strict expiry commit, 1- and 256-member cleanup,
  and a matched parent/current persistent-read control. It remains outside
  complete G3 and G7.
- [Native set member commands evidence —
  2026-08-03](native-set-commands-linux-2026-08-03.md) binds bounded batch
  add/remove, positional membership, ascending cursor scans, canonical
  mutation order, failure atomicity, whole-set TTL, member/lifecycle
  conflicts, randomized model equivalence, all seven singleton crash
  boundaries, multilevel pruning, and reached-corruption checks to direct
  Linux.
- `native-set-commands-linux.json` records private/snapshot/physical
  membership batches, 32 singular physical membership calls, head/middle/tail
  scans, private batch preparation, memory/strict commits, and a matched
  parent/current persistent-read control. It remains outside complete G3 and
  G7.
- [Native whole-set lifecycle evidence —
  2026-08-03](native-set-lifecycle-linux-2026-08-03.md) binds explicit
  complete-set deletion, retained snapshots, same-transaction recreation as
  every implemented structure family, lifecycle conflicts, all seven
  singleton delete and replacement crash boundaries, reached corruption,
  compaction, vacuum, and reopen to direct Linux.
- `native-set-lifecycle-linux.json` records cardinality-separated private
  deletion plus Memory and Strict publication for empty, 64-member, and
  2,048-member sets. It remains outside process-kill/power-loss evidence,
  complete G3, and G7.
- [Native whole-list lifecycle evidence —
  2026-08-03](native-list-lifecycle-linux-2026-08-03.md) binds explicit
  complete-list deletion, all implemented typed recreations, whole-list
  conflicts, 14 singleton crash boundaries, multichunk/blob retirement,
  corruption rejection, compaction, vacuum, blob collection, and reopen to
  direct Linux.
- `native-list-lifecycle-linux.json` records private deletion plus Memory and
  Strict publication for empty, 64-element, and 2,048-element lists. It
  remains outside process-kill/power-loss evidence, complete G3, and G7.
- [Native whole-list TTL evidence —
  2026-08-03](native-list-ttl-linux-2026-08-03.md) binds absolute complete-list
  expiry, compatible `HYLSTM01`/`HYLSTM02` metadata, typed shared cleanup,
  both-end mutation visibility, lifecycle conflicts, Group durability, all
  seven cleanup crash boundaries, corruption rejection, compaction, page
  vacuum, and blob collection to direct Linux.
- `native-list-ttl-linux.json` records separated private/snapshot/physical
  TTL, physical `LLEN`, memory/strict expiry commit, 1- and 256-element
  cleanup, five alternating pinned parent/current persistent-read pairs, and
  a matched list-lifecycle control. It remains outside complete G3 and G7.
- [Native durable scalar-expiry evidence —
  2026-08-01](native-expiry-2026-08-01.md) binds `HYSTRBT2`, ordered scalar
  expiry identities, bounded cleanup, renewal conflicts, fail-closed
  reconstruction, legacy `HYSTRBT1` compatibility, and all seven cleanup
  crash boundaries to one source commit.
- `native-expiry-wsl2.json` is its clean release observation for an empty hot
  due scan plus memory- and strict-durability cleanup batches. It records
  microsecond empty scans and millisecond cleanup batches, and remains
  outside G3 and G7.
- [Native ordered B+tree batch copy-on-write evidence —
  2026-08-02](native-btree-batch-cow-2026-08-02.md) binds complete pre-write
  validation, one rewrite per affected node and level, unchanged-subtree page
  retention, old-root readability, and `HYSTRBT2` physical cleanup
  integration to one source commit.
- `native-btree-batch-cow-wsl2.json` records latency and exact appended-page
  amplification for the same expiry datasets. It improves cleanup p50 by
  about 80% while leaving compaction, background scheduling, and the G3/G7
  matrices open.
- [Native structure reachability-compaction evidence —
  2026-08-02](native-structure-compaction-2026-08-02.md) binds complete
  pre-write validation, canonical tombstone filtering, logical-state
  equivalence, retained historical roots, and all seven interruption
  boundaries to one source commit.
- `native-structure-compaction-wsl2.json` records a 41-to-10 reduction in
  reachable pages and a 93.133% empty-expiry-scan p50 improvement while also
  recording append-only file growth. Physical page-file vacuum remains open.
- [Native directory identity and writer exclusion evidence —
  2026-08-02](native-directory-identity-linux-2026-08-02.md) binds canonical
  native `FORMAT`, stable UUIDv7 identity, fail-closed marker validation, and
  same-process plus cross-process writer exclusion to one direct Linux
  commit. Offline promotion and manifest/anchor lineage threading remain
  open.
- [Native dependency-closure evidence —
  2026-08-02](native-dependency-closure-2026-08-02.md) binds the exact
  historical non-dev graph rooted at `hyphae-native-runtime`, reviewed versions,
  sources/licenses, forbidden-engine rejection, workspace lint inheritance,
  and host-observed unsafe metrics to one source commit.
- `native-dependency-closure-wsl2.json` records 11 native workspace packages,
  19 external primitives/build dependencies, zero native unsafe findings, and
  every external parser/exclusion residual. At that source commit, semantic
  third-party unsafe review and the remaining G0 corpus/conformance work were
  still open.
- [Native bounded-WAL-replay evidence —
  2026-08-02](native-wal-replay-2026-08-02.md) binds fixed-size retention
  anchors, identity-preserving prefix retirement, suffix-only recovery, six
  interruption boundaries, and fail-closed suffix validation to one source
  commit.
- `native-wal-replay-windows.json` and `native-wal-replay-wsl2.json` are
  matched release observations for a 402-commit prefix and four-commit suffix.
  They record 98.7775% fewer WAL bytes and faster warm reopen, but neither
  native-ext4 nor physical power-loss evidence.
- [Native manifest-retention evidence —
  2026-08-02](native-manifest-retention-2026-08-02.md) binds the existing WAL
  anchor to identity-preserving manifest-prefix retirement, partial cleanup,
  seven interruption boundaries and exact all-engine reopen.
- `native-manifest-retention-windows.json` and
  `native-manifest-retention-wsl2.json` are matched generation-131
  observations. They record 131-to-3 manifests and 97.5504% fewer bytes; the
  WSL2 data directory was `tmpfs`, not persistent ext4.
- [Native immutable-blob collection evidence —
  2026-08-02](native-blob-collection-2026-08-02.md) binds authoritative
  all-engine reference tracing, committed generation floors, exact
  digest-ordered pruning, four interruption boundaries, and idempotent
  recovery to one source commit.
- `native-blob-collection-windows.json` and
  `native-blob-collection-wsl2.json` are matched 130-file observations. They
  record 128 removed files and 97.0063% fewer bytes; Windows reports directory
  synchronization unsupported and WSL2 used `tmpfs`, not persistent ext4.

## Hosted release evidence

The release workflow generates
`hyphae-vVERSION.release-evidence.json` after the release commit is checked
out and after the native archives, provenance predicates, and both SBOMs
exist. The document conforms to
[`packaging/release-evidence-v1.schema.json`](../../../packaging/release-evidence-v1.schema.json)
and binds:

- the release tag and workspace version;
- the exact Git commit, its tree object, and, for any tag ref, the fetched tag
  object plus peeled commit target;
- the workflow path, full Git ref, event, run ID, run attempt, and run URL;
- the filename, role, byte size, and SHA-256 digest of every primary release
  payload.

For a tagged `push` or exact-tag recovery run, the primary payloads also include
`hyphae-vVERSION.required-checks.json` with role `required-checks`. That report
conforms structurally to
[`packaging/required-checks-report-v1.schema.json`](../../../packaging/required-checks-report-v1.schema.json)
and records exactly the 20 canonical required GitHub Actions checks. Nineteen
records bind the reviewed PR head and the G8 closure record binds the tagged
merge commit on `main`. Each ordered record carries the matching `head_sha`, unique
check-run ID, workflow-run ID, canonical GitHub job URL, GitHub Actions app
identity, canonical workflow path, authoritative branch and event, run
attempt, start/completion timestamps, and `completed`/`success` state. All
checks from one workflow path must resolve to one workflow run. The report also
records the unique merged in-repository PR to `main`, including its number,
head/base commits, merge commit, and merge time; an all-state query for that
head branch must return no second PR, and the PR's complete issue-event history
must contain no base-ref change or successful automatic base change. The
producer fetches each workflow run and requires its ID, path, `head_sha`,
branch, event, repository, attempt, state, and conclusion to agree with that
check. It fetches every selected Jobs API record and requires the job's exact
ID, workflow-run ID, name, `head_sha`, state, conclusion, and `run_attempt` to
agree with the check and the workflow run's current attempt. A partial rerun
that mixes jobs from different attempts fails closed; a complete rerun of all
jobs can restore one coherent attempt. For one canonical job in each of the six
workflow runs, the producer also records the successful
`Verify the pull-request integration tree` Jobs API step, which requires its
event merge SHA/tree to equal the tested head SHA/tree. The release workflow
verifies the recorded merge commit's parents and tree and its ancestry from
`main`. It selects the latest unambiguous completion time after excluding the
current tag workflow run and fails closed if another relevant run is still
incomplete. Pull-request and ordinary manual candidate runs omit this report
and cannot publish. A manual recovery can publish only when it supplies both
the existing immutable tag and its exact peeled commit; it then requires the
same report and all tag/source checks as a tag push.

The schemas validate portable structure. The repository verifiers additionally
enforce relationships JSON Schema cannot express here, including equality
between root and per-check commits, IDs embedded in URLs, the exact
job-to-workflow mapping, the release tag/version/commit tuple, and the
complete canonical artifact set.
Per-archive provenance may come from an earlier attempt of the same workflow
run when only failed jobs are rerun. Its predicate and digest preserve that
attempt explicitly; a different run ID or an attempt later than the assemble
attempt is rejected. This provenance allowance does not apply to the 17
required-check records, which must all name jobs from their workflow runs'
current attempts. The semantic verifier also requires the canonical native
runner pair for each target: Linux/X64, macOS/X64, macOS/ARM64, or Windows/X64.

The manifest deliberately excludes itself, `SHA256SUMS`, and Sigstore bundles
to avoid a cryptographic self-reference. `SHA256SUMS` includes the completed
manifest, and the workflow signs both the manifest and `SHA256SUMS` with the
same keyless release identity.

This hosted manifest is a release asset, not a checked-in local gate report.
It and the checks report record what the workflow and GitHub Checks API
reported for one commit. The publish job fetches the checks again immediately
before creating the GitHub Release and requires the result to be byte-identical
to the signed report. It also re-fetches the remote tag, verifies its object and
peeled target against the signed manifest, and rechecks target ancestry from
`main`; any mismatch fails closed. This minimizes but cannot remove the final
network race or prevent later mutation without immutable-release and
protected-tag repository governance. The artifacts do not prove check
independence or absence of flaky reruns, authorize publication, or replace the
independent release gates.

The report is not an independent trust root against repository writers. If
branch governance does not require protected workflow ownership, independent
review, and last-push approval, a writer can weaken a workflow before producing
new successful checks. Preventing that authority also requires protected tags
and immutable releases; the signed artifacts only make later substitution
detectable.

## Native page-generation vacuum

- `native-page-vacuum-2026-08-02.md` binds current-root physical page-file
  reclamation, V2 WAL/root metadata, retention-floor writer rejection, mixed
  manifest checkpoints, ANN identity preservation, orphan handling, and the
  six-boundary crash matrix to exact source.
- `native-page-vacuum-windows.json` is its clean release-mode,
  concurrency-one Windows observation. It records 3,580 to 72 pages,
  57,475,072 reclaimed bytes, strict/no-op maintenance latency, separate warm
  point-read percentiles, an isolated same-filesystem sync probe, and reopen
  verification. It is an observation, not a G7 gate.

## Native group commit

- `native-group-commit-2026-08-02.md` binds the bounded multi-producer
  scheduler, independent conflict admission, private MVCC root chain, one
  shared page/WAL flush, orderly shutdown and five-boundary crash matrix to
  exact source.
- `native-group-commit-windows.json` records the clean Windows/NTFS
  eight-producer comparison: 3.502910 times strict throughput, microsecond queue
  wait and millisecond cohort/end-to-end latency.
- `native-group-commit-wsl2.json` records the clean WSL2/v9fs comparison:
  1.654654 times strict throughput. Its sub-microsecond sync observations are
  explicitly not native-ext4 or physical power-loss evidence.

## Native active expiry

- `native-active-expiry-scheduler-2026-08-02.md` binds the optional
  engine-owned timer, injected clock, bounded foreground fairness, strict and
  memory cleanup, terminal failure, shutdown ordering, and recovery matrix to
  exact source.
- `native-active-expiry-scheduler-windows.json` records one optimized
  Windows/NTFS enabled-versus-disabled observation over 512 due keys and 256
  foreground commits.
- `native-active-expiry-scheduler-wsl2.json` records the matched WSL2/tmpfs
  observation. It is not native-ext4 or power-loss durability evidence.

## Native SQL primary-key left prefixes

- `native-primary-key-prefix-scans-2026-08-02.md` binds strict composite
  primary-key left-prefix planning, canonical half-open physical bounds,
  residual-before-limit semantics, transaction/snapshot/current/reopen
  equivalence, adjacent text-prefix isolation, and fail-closed corruption to
  exact source.
- `native-microsecond-smoke-primary-prefix-wsl2.json` is its clean schema-v13
  WSL2 release observation. It reports pure prefix and prefix-plus-residual
  calls separately and remains outside G7.

## Native SQL primary-key prefix ranges

- `native-primary-key-prefix-ranges-2026-08-02.md` binds the first physical
  range on the primary-key component immediately following a strict equality
  prefix to exact source. It covers canonical inclusive/exclusive bounds,
  remaining suffixes, residual-before-limit behavior, transaction/snapshot/
  current/reopen equivalence, and fail-closed physical corruption.
- `native-microsecond-smoke-primary-prefix-range-wsl2.json` is its clean
  schema-v14 WSL2 release observation. It reports the strict-prefix and
  prefix-plus-range routes separately and remains outside G7.

## Native SQL secondary-index ranges

- `native-secondary-index-ranges-2026-08-02.md` binds the first ordered
  physical secondary-index range scan to exact source. It covers the
  order-preserving `HYRIDX02` identity, physical-metadata range admission,
  legacy `HYRIDX01` exact-only compatibility, canonical inclusive/exclusive
  bounds, residual-before-limit behavior, private/retained/current/reopen
  equivalence, and fail-closed malformed-identity and forged-projection
  cases.
- `native-microsecond-smoke-secondary-range-wsl2.json` is its clean
  schema-v15 WSL2 release observation. It reports the ordered secondary
  range and its deliberately expensive unindexed differential baseline
  separately and remains outside G7.

## Native SQL composite secondary prefix ranges

- `native-secondary-index-prefix-ranges-linux-2026-08-02.md` binds a strict
  ordered-secondary equality prefix plus a range on the immediately following
  index column to exact Linux source. It covers canonical bounds, remaining
  suffixes and primary-key ties, residual-before-limit behavior, private/
  retained/current/reopen equivalence, legacy fallback, false-plan rejection,
  and fail-closed physical corruption.
- `native-microsecond-smoke-secondary-prefix-range-linux.json` is its clean
  schema-v16 ext4 release observation. The new indexed and unindexed routes
  use a second isolated native database so the inherited schema-v15 corpus
  and measurement order remain unchanged. It remains outside G7.

## Native ext4 Linux baseline

- `native-ext4-linux-baseline-2026-08-02.md` binds the first native-Linux,
  non-tmpfs execution of the schema-v15 smoke to exact source: an AWS EC2
  devbox with the benchmark data directory on persistent ext4. It opens the
  native-ext4 observation lane and its explicit warm/memory-durability,
  virtualization, and non-comparability limits.
- `native-microsecond-smoke-ext4-linux.json` is its clean schema-v15
  release observation. Its numbers are a new devbox baseline, not
  run-to-run comparable with the WSL2 or Windows receipts, and it remains
  outside G7.

## Native lineage ext4 latency

- `native-lineage-ext4-latency-2026-08-02.md` binds a direct-Linux schema-v15
  repeat to the exact lineage-bearing source tree later merged by PR 53. It
  records all 21 routes and a bounded same-host comparison without inventing
  a regression threshold.
- `native-microsecond-smoke-lineage-ext4-linux.json` is the raw clean receipt.
  It is warm, memory-durability, concurrency-one embedded/local-frame
  evidence; it is not strict-durability, transport, power-loss, G1, or G7
  closure.

## Native process crash matrix on Linux

- `native-process-crash-matrix-linux-2026-08-02.md` binds seven real
  process-kill/reopen cycles to the exact singleton all-engine commit source.
  It verifies prior-or-complete CSN visibility across relational,
  structure/TTL, lexical, and blob state while the child retains the writer
  lock until `SIGKILL`.
- `native-process-crash-matrix-linux.json` is the exact release receipt from
  AWS Linux/ext4. It is process-crash evidence, not sector, filesystem
  reordering, device-cache, EC2-stop, or physical power-loss evidence.

## Native checkpoint process crash matrix on Linux

- `native-checkpoint-process-crash-linux-2026-08-02.md` extends the live
  writer-lock/`SIGKILL` harness to staged manifest, published manifest,
  appended checkpoint WAL, and synchronized checkpoint WAL boundaries.
- `native-process-crash-matrix-v2-linux.json` preserves the prior seven
  singleton results and records four checkpoint authority outcomes. It
  remains process-crash evidence, not physical power-loss evidence.

## Native durable snapshot pins on Linux

- `native-snapshot-pins-linux-2026-08-02.md` binds the `HYPIN001` registry,
  exact all-engine historical materialization, three retained page
  generations, pin-aware WAL/blob blocking, explicit unpin/collection, and
  direct Linux gates to source commit `01355d0`.
- `native-snapshot-pins-linux.json` records three pin publications and
  historical materializations, four-generation reopen, vacuum observations,
  and exact retained/removed file bytes on AWS Linux/ext4.
- `native-snapshot-pins-process-crash-linux.json` is schema v3 of the existing
  process harness: seven commit, four checkpoint, and two snapshot-pin
  `SIGKILL`/reopen scenarios. It remains process-crash evidence, not physical
  power-loss evidence.

## Native block-layer power-loss replay on Linux

- `native-block-power-loss-replay-linux-2026-08-02.md` binds seven singleton
  commit and four checkpoint interruption marks to an exact source tree,
  Linux `dm-log-writes` target, pinned replay utility, and fresh ext4 images.
- `native-block-power-loss-replay-linux.json` is the raw stable-media replay
  receipt. It proves the worst recorded state through completed flush/FUA
  barriers, normal ext4 journal recovery, native reopen, and exact cleanup.
  Its status is deliberately `block-replay-not-physical-device-cut`; literal
  EC2/EBS power removal and device-firmware behavior remain outside the claim.

## Native local UDS transport on Linux

- `native-local-uds-linux-2026-08-03.md` binds the first filesystem-backed
  `HYPHLCL1` transport to exact contract, implementation, harness, failure
  tests, direct-Linux host disclosure, and validation logs.
- `native-local-uds-linux.json` records three release observations and their
  median statistics. It measures framing plus a kernel UDS round trip, not an
  engine operation, durability, saturation, Windows named-pipe behavior, or a
  G1/G6/G7 gate closure.

## Native local structure GET on Linux

- `native-local-structure-get-linux-2026-08-03.md` binds the first
  engine-bearing `HYPHLCL1` operation to its frozen binary contract, physical
  B+tree execution, TTL clock authority, failure tests, exact Linux source,
  and validation logs.
- `native-local-structure-get-linux.json` records three release observations
  of embedded physical `GET`, persistent `PING`, and persistent engine-bearing
  `GET`. The bounded p50/p99 targets are met, but the receipt is not a
  regression threshold or G0/G1/G6/G7 closure.

## Native local structure SET and TTL on Linux

- `native-local-structure-set-ttl-linux-2026-08-03.md` binds the frozen binary
  contract, u128 transaction receipt correction, implicit native commits,
  controlled expiry, failure recovery, strict reopen proof, and direct-Linux
  validation to exact commits and logs.
- `native-local-structure-set-ttl-linux.json` records three release
  observations of physical TTL, persistent GET/TTL, and memory/strict SET.
  The read routes remain in the microsecond domain, while memory SET is
  hundreds of microseconds and strict SET is milliseconds. The receipt exposes
  that deficit and is not a regression threshold or G0/G1/G3/G6/G7 closure.

## Native local SEARCH MATCH on Linux

- `native-local-search-match-linux-2026-08-03.md` binds the first local
  search-engine operation to its canonical request/result contract, visible
  CSN, physical inverted-index route, failure recovery, reopen proof, exact
  Linux source, and validation logs.
- `native-local-search-match-linux.json` records three release observations of
  embedded physical MATCH, persistent PING, and persistent one-hit MATCH.
  They remain in the microsecond domain but do not establish a regression
  threshold or close G0/G1/G4/G6/G7.

## Native local all-engine transaction on Linux

- `native-local-all-engine-transaction-linux-2026-08-03.md` binds one explicit
  local SQL + structure + lexical transaction to exact contracts, commits,
  golden codecs, failure-state tests, optimistic conflict, strict reopen, and
  all seven deterministic commit interruption boundaries.
- `native-local-all-engine-transaction-linux.json` records three direct-Linux
  release observations of PING, each engine's stage receipt, and memory/strict
  commit. Staging remains in the microsecond domain; memory and strict commit
  remain in milliseconds. The receipt exposes that deficit and is not a
  regression threshold or G0/G1/G5/G6/G7 closure.
