# Fable 5 native-ecosystem handoff

Status: active engineering handoff; PR 47 remains draft

Prepared: 2026-08-02 11:25:54 -05:00

Audience: Fable 5 and the next Hyphae maintainer

## Read this first

The immediate work is not a general migration or product cutover. It is the
ordered secondary-index range milestone in draft
[PR 47](https://github.com/celiumsai/hyphae/pull/47). Its implementation and
hosted CI are green, but its schema-v15 latency receipt and evidence closure do
not exist. Do not mark the PR ready, merge it, or claim a gate closed until
that receipt and the listed documentation are source-bound and checked in.

The larger product direction is settled: one `hyphae` binary, one data
directory, three Hyphae-owned native engines, and one shared transactional
substrate. No user-facing native feature flag, native CLI subcommand, or
format-2-to-native migration shim has been implemented. The intended migration
is offline and fail-safe: read format 2, import into a separate native target
directory, verify counts and digests, then promote. Never rewrite the source
directory in place.

## 1. Exact source-control state

### Implementation anchor

| Field | Exact value at audit |
|---|---|
| Repository | `celiumsai/hyphae` |
| Branch | `codex/sql-secondary-range` |
| Implementation HEAD | `c2bd2c11ff407b177a972b6a34265c92b088fa66` |
| Tree | `22cd2efef01e6fda12bb97e1b2f19f0ef40609df` |
| Upstream | `origin/codex/sql-secondary-range` |
| Upstream divergence | `+0 / -0` |
| Base and merge-base | `origin/main` at `7e70b7402182ae3912e185be352411cd48667b91` |
| Worktree | clean |

This handoff is a documentation-only commit on top of the implementation
anchor. For benchmark attribution, bind the receipt to the exact source commit
actually executed and require a clean worktree. Do not silently reuse
`c2bd2c1` after code or benchmark-source changes.

GitHub had only two branches at the audit boundary: protected `main` and
`codex/sql-secondary-range`. The only open PR was PR 47.

### Commits in flight

In oldest-to-newest order:

| Commit | Purpose |
|---|---|
| `274c0dd65939f47737cf56fbde95f3b092c83df9` | Specify ordered secondary-index ranges |
| `55d005163ee8e72f162c323277b0185153c04472` | Add the planner red test and range-planning matrix |
| `af55a977b6ee3ca227ae7da39c27cc0b8a22a4ba` | Execute ordered secondary-index ranges |
| `60f1a5de3e307be9f6b81bf14b99cfc911981aac` | Add the schema-v15 benchmark routes |
| `a007874313cc0d93b46c021a2c1365a2915101d1` | Replace the invalid range corpus with variable-width ordered keys |
| `c2bd2c11ff407b177a972b6a34265c92b088fa66` | Bound secondary-range warmup independently |

The implementation diff against `main` is five files, 2,175 insertions and
344 deletions:

- `crates/hyphae-native-runtime/examples/microsecond_smoke.rs`;
- `crates/hyphae-native-runtime/src/lib.rs`;
- `crates/hyphae-native-runtime/src/model.rs`;
- `crates/hyphae-native-runtime/src/sql.rs`; and
- `docs/native/sql-semantics-v1.md`.

### Pull request state

| Field | State at audit |
|---|---|
| PR | [47 — Add ordered secondary index range scans](https://github.com/celiumsai/hyphae/pull/47) |
| Base / head | `main` <- `codex/sql-secondary-range` |
| State | open, draft |
| GitHub mergeability | mergeable, `mergeStateStatus=CLEAN` |
| Reviews / review requests / comments | none |
| Checks | every executed check green |
| Publish release | skipped as expected for a PR |

The successful checks cover quality, Linux stable and MSRV, macOS, Windows,
public-client conformance, optional integrations, release readiness,
dependency review and policy, bounded parser fuzzing, load/kill-restart soak,
platform packaging, and release-candidate assembly. They prove the hosted
workflow at the audited head. They do not supply the missing schema-v15
benchmark receipt.

## 2. What PR 47 actually establishes

The milestone replaces an incorrect range-order assumption with a versioned
physical layout:

- `HYRIDX01` remains readable, writable, recoverable, and valid for exact
  equality lookup.
- `HYRIDX01` is never advertised as range-capable because its identity is
  `u32(index_key_length) || index_key || primary_key`; variable-width values
  sort by length before value.
- New secondary indexes use `HYRIDX02` with
  `index_key || primary_key || u32(primary_key_length)`.
- The index key is ordered first, while canonical primary-key bytes provide a
  deterministic tie-breaker.
- Physical range planning is admitted only when persisted metadata confirms
  the ordered layout. Catalog intent alone is insufficient.
- Legacy layouts fall back to a bounded primary-key scan when that fallback is
  legal. They are not mislabeled as a physical secondary range.
- Current-root execution traverses the physical B+tree directly. Private and
  retained snapshots preserve equivalent semantics.
- Inclusive, exclusive, one-sided, empty, inverted, and SQL `NULL` bounds have
  explicit behavior.
- Complete simple and composite secondary keys are supported. A partial
  composite key is not.
- Residual predicates execute before `LIMIT`.
- Malformed ordered identities and forged row projections fail closed.

The principal executable coverage is:

- planner identity and physical-bound tests;
- variable-width physical ordering;
- private, retained, latest, and reopen equivalence;
- inclusive/exclusive/one-sided/empty/`NULL` boundaries;
- wrong arity, wrong type, duplicate-bound, skipped-key, and partial-composite
  rejection;
- `HYRIDX01` exact lookup and reopen compatibility without false range
  planning; and
- fail-closed malformed-identity and forged-projection cases.

This does not implement equality-prefix plus next-column ranges for composite
secondary indexes, descending scans, streaming cursors, offsets, multi-range
or bitmap access, statistics, cardinality estimation, or cost-based planning.

## 3. The unification plan

The accepted target is documented in
[ADR-0020](../adr/0020-native-local-data-ecosystem.md) and the
[native architecture](../architecture/native-local-ecosystem.md):

```text
embedded Rust API                 native local protocol
        \                                 /
                 typed local fabric
                         |
        +----------------+----------------+
        |                |                |
  relational         structures       search
  SQL/indexes        keys/TTL         lexical/vector
        +----------------+----------------+
                         |
     types / IDs / catalog / pages / blobs
     WAL / CSN / MVCC / scheduler / memory
     checkpoint / recovery / backup / proofs
```

These are three first-class engines, not three protocol facades over a generic
KV store and not SQL projections. Each owns its physical data. One
cross-engine transaction becomes visible under one CSN, so readers never see
a mixed commit.

### Direct answers to the handoff questions

| Question | Actual answer |
|---|---|
| A new native subcommand? | Not implemented and not named. ADR-0006 requires migration, doctor, verification, and other operations to remain subcommands of the one `hyphae` executable, but the native cutover command contract is still open. |
| A runtime feature flag? | No. None exists in the CLI history or current dependency graph. A legacy/native dual-authority flag was not selected. |
| A format-2-to-native shim? | No in-place or continuously dual-writing shim. The accepted direction is an offline importer into a separate target directory, followed by verification and promotion. |
| Is the native runtime in the shipped binary? | No. `hyphae-native-runtime` is unpublished, is not a dependency of `hyphae-cli`, and the CLI remains the default workspace member. |
| Is format 2 being extended into the target store? | No. Redb and disk format 2 remain the `0.2` compatibility path while the native store is built; they are not target-path authority or G1 evidence. |
| How will local clients connect? | Embedded Rust calls are direct. The future daemon edge uses the compact native protocol over UDS or Windows named pipes. The codec exists experimentally; those transports and session flow control remain pending. |

### Ordered program

The phase gate remains:

1. G0: contracts and clean-room/dependency discipline;
2. G1: native substrate;
3. G2: relational engine;
4. G3: structure engine;
5. G4: search engine;
6. G5: cross-engine convergence;
7. G6: local product surface;
8. G7: controlled performance evidence; and
9. G8: release, including format-2-to-native migration.

A later gate may be prototyped, but it cannot be declared complete while an
earlier required gate is red.

### Migration shape that was chosen

The target sequence is:

1. hold the format-2 source read-only;
2. create a different native target directory;
3. import a verified logical snapshot;
4. map legacy objects to stable native catalog identities;
5. verify counts, digests, and semantic equivalence;
6. promote the target only after all validation passes; and
7. retain the source for rollback until the operational policy permits
   retirement.

Only steps 1 through 3 are described at architecture level. The object mapping,
receipt and proof continuity, idempotency continuity, promotion marker,
rollback lifecycle, and edge-API compatibility have not been specified or
implemented.

## 4. Rejected architectures

These are settled alternatives, not open implementation suggestions.

| Rejected path | Why it was rejected |
|---|---|
| Bundle PostgreSQL, Valkey, and OpenSearch | It preserves three authorities, memory budgets, maintenance systems, consistency boundaries, and latency tails. |
| Put protocol translators around those products | A unified installer or API is not a unified engine. |
| Build SQL/RESP/OpenSearch facades over the old KV engine | Parsers and protocols do not supply native relational layouts, MVCC, specialized structures, postings, ANN, or their operational semantics. |
| Make structures and search SQL projections | It prevents them from being first-class engines with their own data and layouts. |
| Embed upstream engines as Rust libraries | Core semantics, scheduling, layout, and hot paths would remain outside Hyphae. |
| Use TCP, HTTP, JSON, RESP, PostgreSQL wire, or OpenSearch REST between engines | Internal work remains typed and direct; compatibility protocols belong only at an edge. |
| Reuse Redb or disk format 2 as the final native substrate | The target requires Hyphae-owned pages, WAL, MVCC, catalog, memory governance, and specialized indexes. |
| Rewrite format 2 in place | Failure would make rollback and independent source verification unsafe. |
| Toggle legacy and native authority with a feature flag | It creates two truth paths and makes recovery, proof, compatibility, and cutover evidence ambiguous. |
| Copy or cherry-pick historical branches | The porting ledger has no accepted ports. Historical code is read-only research input unless provenance, license, transformation, inherited tests, and human review are accepted per file. |
| Introduce HiveMind or another LLM now | The native local data ecosystem is the first phase; models remain later consumers of public contracts. |

## 5. Attempted paths that were superseded

These are useful technical dead ends. Do not repeat them without new evidence.

| Attempt | Finding | Replacement or remaining boundary |
|---|---|---|
| Serialize each engine's complete state into one copy-on-write page | Proved cross-engine commit shape but could not scale. | Native heaps, B+trees, postings, immutable blobs, and persisted ANN generations replaced the whole-state vertical. |
| Execute latest secondary SQL by materializing the complete snapshot | Correct as an oracle, too expensive as the hot path. | `prepare_sql_latest` and `execute_prepared_latest` traverse current physical roots. Retained historical snapshots still materialize and remain an explicit debt. |
| Expiry cleanup by materializing structure state and publishing one COW path per key | A 64-key memory-durability run reached millisecond latency and exposed page amplification. | A physical fast path and ordered batch COW removed repeated full-state work. Multi-generation pinning and full memory-amplification evidence remain open. |
| Rebuild the ANN graph for every vector and again at commit | A 512-vector batch took roughly 21 seconds per rebuild in the recorded experiment. | Duplicate-free batch ingest performs one rebuild per target index. Small-corpus exact search still beats HNSW, and graph traversal still materializes at open/snapshot time. |
| Assume mixed scheduling itself would preserve microsecond latency | Queue wait became the dominant cost under the measured WSL2 load; end-to-end p50 was over one millisecond. | Admission, queue, execution, and sync clocks are now separate. Stable hardware, sustained fairness, and physical durability lanes remain open. |
| Use `HYRIDX01` for arbitrary range traversal | Variable-width text orders by encoded length before value. | `HYRIDX02` is order-preserving; V1 stays exact-only. No eager rebuild was introduced. |

### PR 47 benchmark false starts

No output from these attempts is admissible evidence:

1. The first wrapper had shell quoting failures, including `tr: extra
   operand`; it emitted empty or unknown source metadata. It was discarded.
2. The first corpus used human-readable numeric suffixes such as
   `person-{row}`. Lexicographic bounds did not select the intended ten-row
   numeric interval, so differential validation correctly failed.
3. Commit `a007874` replaced that corpus with keys `a`, `aa`, through ten
   repetitions of `a`, while all other values sort under `z-...`. The range
   `[a,b)` now selects exactly ten rows and exercises variable widths.
4. The new unindexed baseline was initially included in the global 100,000-call
   warmup. That made a deliberately expensive primary-key scan dominate the
   run and produced an orphaned WSL process. The process was killed and its
   output was discarded.
5. Commit `c2bd2c1` gives the two secondary-range routes an independent
   1,000-call warmup and 10,000 observations each.
6. A targeted `cargo test --exact` invocation omitted the fully qualified
   `tests::` name and ran zero tests. It is not evidence.
7. A corrected schema-v15 run was interrupted before it produced a validated
   JSON receipt. No receipt exists.

## 6. Decisions made in code but not yet captured by a dedicated ADR

These are current implementation facts, not permission to freeze a public
format:

1. New secondary-index metadata uses `HYRIDX02`; `HYRIDX01` is compatible for
   exact lookup and recovery without implicit rebuild.
2. `HYRIDX02` identity is
   `index_key || primary_key || u32be(primary_key_length)`. This relies on the
   canonical complete-key codec and uses primary-key bytes as the tie-breaker.
3. The planner asks persisted physical metadata whether a secondary index is
   ordered. Missing, mismatched, malformed, or legacy metadata fails closed or
   uses a legal bounded fallback.
4. At most one lower and one upper bound is admitted. Duplicate bounds fail
   binding as `HYSQL014`; `LIMIT` is mandatory; ordering, when requested, is
   the complete ascending secondary key.
5. SQL `NULL` produces no matches only after complete arity and type
   validation. Residuals are evaluated before the output limit.
6. The native runtime stays unpublished and outside the product/default path
   during convergence. There is no accepted transition ADR for this isolation.
7. Native compatibility policy is currently family-specific:
   - old catalog roots upgrade on a later write;
   - old relational roots remain readable and writable without implicit
     conversion;
   - legacy structure and search roots remain compatible; and
   - `HYRIDX01` remains exact-only.
   A single format-evolution ADR is still needed.
8. Two SQL execution modes remain: historical snapshots may materialize,
   while latest/current-root execution is physical and catalog-version-bound.
9. Detached transactions prepare and rebase concurrently, but publication and
   durability I/O remain serialized behind exclusive database-handle access.
10. The experimental directory (`pages.hydb`, `wal.hywal`, `roots/`, `blobs/`)
    is evidence, not the final directory contract. The native product still
    needs a global format marker, writer lock, ownership detection, and cutover
    lifecycle.
11. No accepted mapping yet preserves format-2 documents, vector spaces,
    lexical definitions, receipts, proof anchors, and idempotency identities
    across native migration.
12. Compatibility gateways are allowed only after the native local ecosystem
    is complete. Which `0.2` edge APIs survive has not been decided.

Items 6 through 12 should be consolidated into a cutover and format-evolution
ADR before migration code is written. Do not invent `--native` as a shortcut.

## 7. Real test and CI state

### Confirmed at implementation anchor

The following local WSL2 checks passed on 2026-08-02:

```text
cargo test -p hyphae-native-runtime --locked
187 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

cargo clippy -p hyphae-native-runtime \
  --all-targets --all-features --locked -- -D warnings
passed

cargo fmt --all -- --check
passed
```

Hosted CI also passed the broader lanes at the implementation anchor:

```text
cargo test --workspace --all-features --locked
187 passed; 0 failed

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --all-features --no-deps --locked
```

Release readiness passed the documentation checker and historical
compatibility checks. Hosted Windows stable and Windows packaging passed.
Older local Windows executable attempts were blocked by Application Control
with OS error 4551; WSL2 is the local executable lane, while hosted Windows is
separate platform evidence.

### Not run or not proved

- No valid schema-v15 benchmark has completed.
- No mutation test was run. The repository has no accepted mutation tool,
  operator set, or surviving-mutant threshold for this milestone.
- The local checks above are package-scoped except formatting. Full-workspace
  validation is hosted evidence at the implementation anchor.
- No named-pipe or UDS transport benchmark exists.
- No cold-state, concurrency, saturation, interference, allocation/RSS,
  hardware-counter, fsync, proof-construction, or long-running cursor lane was
  added by PR 47.

## 8. Baselines that must remain visible

These are source-bound observations, not accepted regression thresholds and
not a G7 pass. The canonical checked-in reference is the
[schema-v14 receipt](../gates/evidence/native-microsecond-smoke-primary-prefix-range-wsl2.json)
from commit `7cc7cfcf6ede0574f10cff5c5aeb2c316fad614f`, Debian 13/WSL2,
Rust 1.96.0, release profile, warm memory durability, and concurrency one.

Every schema-v15 route inherited from v14 must be reported. A material change
must be investigated; do not invent an after-the-fact percentage threshold.

| Operation | v14 p50 us | v14 p99 us | v14 throughput ops/s |
|---|---:|---:|---:|
| Embedded structure GET, 64 B | 0.036 | 0.051 | 25,262,443 |
| Buffered structure B+tree GET | 0.590 | 1.661 | 1,567,752 |
| Materialized hash HGET | 0.072 | 0.117 | 12,846,055 |
| Buffered hash HGET | 1.037 | 2.670 | 878,643 |
| Materialized set membership | 0.086 | 0.108 | 11,076,066 |
| Buffered set membership | 1.685 | 3.840 | 549,579 |
| Buffered BM25 rare-term top 1 | 18.133 | 48.999 | 50,224 |
| Materialized prepared SQL PK | 0.034 | 0.056 | 27,040,412 |
| Buffered relational PK | 1.144 | 2.667 | 816,990 |
| Buffered PK scan, limit 10 | 12.083 | 29.702 | 75,742 |
| Physical prepared PK scan, limit 10 | 13.578 | 35.908 | 66,899 |
| Buffered PK range, limit 10 | 12.161 | 29.445 | 75,594 |
| Physical prepared PK range, limit 10 | 14.910 | 39.477 | 60,501 |
| Physical PK range plus residual, limit 10 | 22.216 | 69.169 | 41,126 |
| Physical PK prefix, limit 10 | 16.425 | 40.672 | 55,944 |
| Physical PK prefix plus range, limit 10 | 15.119 | 40.127 | 60,804 |
| Buffered exact unique secondary | 11.328 | 37.413 | 79,032 |
| Physical prepared exact unique secondary | 13.101 | 40.606 | 68,073 |
| Local frame decode plus structure dispatch | 0.081 | 0.195 | 9,630,767 |

The earlier schema-v13 residual prefix-range route observed p50/p99
`198.603/589.950 us` and `4,472 ops/s`. Its comparison with the v14 physical
prefix-range route identifies removed algorithmic work; it is not a portable
threshold.

The provisional phase-1 target relevant to PR 47 is indexed SQL returning at
most 100 rows: p50 `50 us`, p99 `250 us`, under the disclosed warm and bounded
conditions. Passing that one target would still not close G7.

Schema v15 must add both of these in the same process and corpus:

- `physical_prepared_sql_unindexed_text_range_pk_scan_limit10_multilevel`; and
- `physical_prepared_sql_secondary_range_limit10_multilevel`.

The intended v15 range slice is 2,048 rows, ten returned rows, variable-width
keys in `[a,b)`, memory durability, concurrency one, 1,000 range warmups, and
10,000 observations per new route.

## 9. Open gate and evidence ledger

### PR 47 closure still missing

The following files or updates do not exist at the implementation anchor:

- a machine-readable schema-v15 receipt, expected under
  `docs/gates/evidence/`;
- a `native-secondary-index-ranges-2026-08-02.md` evidence narrative;
- a new entry in `docs/gates/evidence/README.md`;
- a new experimental-evidence entry in
  `docs/gates/native-local-phase-1.md`;
- `HYRIDX02` and its exact entry identity in
  `docs/native/btree-format-v1.md`;
- `HYSQL014` in the SQL error-code summary;
- the implemented secondary-range capability in the SQL and native-runtime
  status headers;
- the new observation in
  `docs/performance/microsecond-first.md`; and
- the explicit mutation-testing exclusion.

Historical evidence that says secondary ranges were pending was correct for
its bound commit. Add a newer evidence entry; do not rewrite older evidence as
if the capability existed then.

### Phase gates

| Gate | Honest state after PR 47 implementation |
|---|---|
| G0 | Open. Specifications and substantial evidence exist, but golden/conformance coverage and the complete dependency/unsafe review remain incomplete. |
| G1 | Open. The native substrate is substantial; multi-generation pinning, complete crash/power-loss lanes, some bounded physical paths, and final ownership/concurrency remain. |
| G2 | Open. PR 47 adds one bounded relational access path; SQLLogicTest, constraints, general expressions, optimizer breadth, CTEs, windows, grouping, sorting/spill, broader joins, TPC-H, TPC-C, and isolation evidence remain. |
| G3 | Open. Several native structures exist, but command-family breadth, streams, model-based testing, TTL breadth, eviction, and amplification closure remain. |
| G4 | Open. Native BM25 and durable ANN slices exist; positions, phrases, filtering/facets/doc values, segments, buffered/filtered ANN, hybrid fusion, and quality matrices remain. |
| G5 | Open. One-CSN verticals exist, but complete cross-engine SQL operators, backup/restore, and mixed-reader proofs are incomplete. |
| G6 | Open. The runtime is not in the product binary; native transports, CLI/SDK/admin integration, and the common product error model are incomplete. |
| G7 | Open. Existing receipts are observations; the controlled performance matrix has not passed. |
| G8 | Open. Migration, soak/power-loss/resource-exhaustion closure, signed packaging, and independent restore evidence are incomplete on one exact release commit. |

PR 47 advances G2 implementation and adds a future G7 observation lane. It
closes no phase gate.

## 10. The single next step

Run the corrected schema-v15 release benchmark to completion from a clean
checkout of the current PR source, validate the JSON, and preserve the raw
source-bound output before editing narrative evidence.

One safe WSL2 shape is:

```bash
set -euo pipefail
cd /mnt/c/Users/Mario/MyBook/Documents/celiumsai/hyphae
test -z "$(git status --porcelain)"
commit="$(git rev-parse HEAD)"
rustc_id="$(rustc --version | tr ' ' '-')"
cargo run --release -p hyphae-native-runtime \
  --example microsecond_smoke -- "$commit" "$rustc_id" \
  > /tmp/hyphae-native-microsecond-smoke-v15.json
python3 -m json.tool /tmp/hyphae-native-microsecond-smoke-v15.json >/dev/null
```

Before admitting the receipt:

1. confirm `schema == hyphae.native.microsecond-smoke.v15`;
2. confirm the receipt commit equals the clean executed HEAD;
3. confirm both new range operations exist;
4. confirm the indexed and unindexed routes return the same ten rows;
5. compare every inherited v14 route without inventing a new threshold;
6. record all environment and methodological exclusions; and
7. only then add the receipt and evidence narrative.

This is the single next action because PR 47 cannot be closed honestly without
it. After PR 47 is merged through protected `main`, the next architecture
decision should be a cutover and format-evolution ADR covering the global
format marker and lock, v2 object mapping, receipts/proofs/idempotency,
staging/verification/promotion/rollback, and the one-binary compatibility
edge.

## 11. PR 47 definition of done

PR 47 can move from draft to ready only when:

- the schema-v15 receipt is valid, source-bound, and checked in;
- the two new routes are compared honestly in the same run;
- inherited v14 observations are reviewed for material changes;
- B+tree, SQL status/error, native-runtime status, performance, phase-gate, and
  evidence-index documentation are current;
- the evidence narrative records red/green coverage, compatibility,
  corruption behavior, commands, environments, exclusions, and remaining
  boundaries;
- local documentation and diff checks pass;
- hosted CI is green again at the documentation/evidence HEAD; and
- mutation testing is explicitly recorded as not run rather than implied.

Merge only through the protected `main` PR path. Do not leave the completed
implementation stranded on a long-lived branch.

## 12. Commands for reorientation

```powershell
git status --porcelain=v2 --branch
git rev-parse HEAD
git rev-parse 'HEAD^{tree}'
git log --reverse --oneline origin/main..HEAD
git diff --check origin/main...HEAD
gh pr view 47 --json state,isDraft,headRefOid,mergeStateStatus,statusCheckRollup
gh pr checks 47
rg -n "hyphae.native.microsecond-smoke.v15|secondary.*range" `
  crates/hyphae-native-runtime docs
```

If the branch advanced, re-establish the implementation and receipt anchors
before using any test or benchmark result. A green command from a different
tree is context, not evidence for the new HEAD.
