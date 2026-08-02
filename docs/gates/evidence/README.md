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

For a tagged `push` run, the primary payloads also include
`hyphae-vVERSION.required-checks.json` with role `required-checks`. That report
conforms structurally to
[`packaging/required-checks-report-v1.schema.json`](../../../packaging/required-checks-report-v1.schema.json)
and records exactly the 17 canonical required GitHub Actions checks for the
release commit. Each ordered record carries the matching `head_sha`, unique
check-run ID, workflow-run ID, canonical GitHub job URL, GitHub Actions app
identity, canonical workflow path, PR head branch, `pull_request` event, run
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
incomplete. Pull-request and manual candidate runs, including a manual run
dispatched from a tag ref, omit this report and cannot publish.

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
