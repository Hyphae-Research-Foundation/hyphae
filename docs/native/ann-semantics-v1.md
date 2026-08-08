# Native ANN semantics v1

Status: normative bounded G6 contract; deterministic HNSW, canonical `f32`
admission, three metrics, exact oracle, catalog definitions, native search
B+tree generations, WAL mutations, all-engine MVCC visibility, object-keyed
base-plus-delta lifecycle, bounded consolidation, historical snapshots,
stable-ID eligibility traversal, adaptive exact filtering, durable lifecycle
policy, maintenance due signaling, retained generations, and fail-closed
recovery are implemented. Page-buffered traversal and a background scheduler
remain production non-claims.

ANN is a Hyphae-owned search-engine capability. Exact vector execution remains
the quality oracle.

## Vector admission

- V1 stored ANN vectors are canonical finite `f32`.
- Dimension is fixed by index definition from 1 through 65,535.
- Supported metrics are cosine distance, negative dot-product distance and
  squared L2 distance.
- Cosine vectors cannot be zero.
- NaN, infinity and dimension mismatch are rejected before commit.
- Object ID is the deterministic final tie-breaker.

## Exact oracle

Every index definition has an exact scorer using the same canonical vector and
metric semantics. Quality evaluation compares ANN top-k against the complete
exact ranking on a pinned snapshot.

## HNSW v1 target

The first approximate index is Hyphae-owned HNSW with versioned:

- `M`;
- `ef_construction`;
- default and maximum `ef_search`;
- level multiplier;
- entry-point selection;
- random seed derivation;
- neighbor pruning rule; and
- distance implementation identity.

Canonical base construction orders insertions by creating CSN then object ID
and derives randomness from index ID plus definition digest. Parallel
optimization may not change logical results without a new physical-build
identity.

The first executable kernel is `hyphae-native-ann`. V1 bounds `M` to 2 through
64, requires `ef_construction >= M`, derives each node level from BLAKE3 over
the index-definition digest and object ID, and retains at most `M` directed
neighbors per layer. Neighbor selection uses exact metric distance followed by
object ID. Foreground update and delete do not rebuild this graph. They replace
one object-keyed delta record above it.

`IndexSnapshot` exports definition, vectors with creating CSNs, graph nodes,
entry point, maximum level and build identity. Restore reconstructs the graph
from the vector records and rejects any snapshot that differs from that
canonical build.

## Implemented durable generation

The native runtime stores ANN in the same copy-on-write search B+tree as
lexical state:

- `0x05 + index ObjectId` selects legacy `HYANNM01` or current `HYANNM02`
  generation metadata;
- `0x06 + index ObjectId + build identity + object ObjectId` stores one
  `HYANNV01` vector with its creating CSN; and
- `0x07 + index ObjectId + build identity + object ObjectId + u16 layer`
  stores one `HYANNG01` neighbor list; and
- `0x08 + index ObjectId + object ObjectId` stores one current `HYANND01`
  upsert or tombstone with a monotonic per-index sequence and mutation CSN.

Every identity component is big-endian in the key. The 32-byte build identity
is content-bound by the kernel. Vector components remain canonical
little-endian `f32`; graph neighbors are stable 128-bit object IDs.

`HYANNM02` names the selected immutable base identity and a view identity over
that base plus the complete current delta, together with base counts, delta
record/byte counts and next sequence. `HYANNM01`, `HYANNV01`, and `HYANNG01`
remain readable. The v2 envelope includes and validates `HYANNM01` as its
predecessor-lineage tag. `HYSEABT1` and `HYSEABT2` remain readable; the first
base-plus-delta mutation selects `HYSEABT3`.

Index creation, including vectors staged in the same transaction, constructs
the initial canonical base. Every later foreground upsert or delete validates
against the effective set and rewrites only metadata plus the affected
object-keyed `HYANND01` record. Repeated writes to one object replace that
record. The base build identity and its vector/graph records therefore remain
unchanged across foreground mutation commits. Each index durably selects a
`delta_max_entries` no larger than the 4,096-record format ceiling, a
`consolidate_after_deltas` threshold no larger than that capacity, and one to
64 retained generations. Encoded delta data remains capped at 64 MiB. A
mutation exceeding its per-index or byte bound fails before publication.

Open and snapshot materialization scan the selected base and delta, validate
every ANN physical key/value, reconstruct the base `IndexSnapshot`, and require
`HnswIndex::restore` to reproduce it exactly. Delta envelopes, dimensions,
sequences, counts, bytes and the selected view identity are also checked.
Unknown indexes, missing metadata, orphan records, malformed vectors/layers,
count divergence, bad neighbors or a noncanonical build fail the complete root.
Queries currently traverse this validated in-memory materialization, not
buffer-pool pages directly.

## MVCC and mutation

New/updated vectors and tombstones enter the transaction-private object delta.
`upsert_vectors` admits a duplicate-free batch atomically without rebuilding
the base. Single-vector upsert and delete preserve read-your-writes. At commit,
all records receive the assigned CSN and are published with the all-engine root.
Retained root sets preserve historical base-plus-delta views.

The exact oracle ranks the effective set: start with base vectors, replace or
remove every object named by the delta, then apply metric order and object-ID
tie breaking. Approximate execution traverses the base graph, removes base hits
shadowed by any delta, scores every live delta upsert exactly, merges those
candidates and truncates to `k`. The receipt's build identity is the selected
view identity; without a delta it equals the canonical base identity. This is
honestly approximate because delta candidates are exact while base candidates
remain bounded by graph traversal.

## Bounded consolidation

`plan_ann_consolidation` captures one current effective set and constructs a
canonical replacement base outside writer admission. Both effective vectors
and captured delta records have caller-supplied hard bounds; the implementation
also caps those requests at 1,000,000 vectors and 4,096 delta records. An empty
delta is not a consolidation candidate.

`consolidate_ann` publishes a captured plan through an ordinary root commit and
the append-only search maintenance opcode 50. Publication requires the captured
base still to be selected. It consumes a captured object delta only when that
object still has the captured sequence, preserving any later replacement or
tombstone. The replacement search B+tree is written from current entries and
retains only the configured number of obsolete target generations. The new root
is selected atomically by the normal page, WAL and MVCC protocol, so recovery
observes the old or complete new view. Historical roots and snapshot pins keep
their normal page-generation safety; physical reclamation remains the existing
page vacuum operation rather than an ANN-specific reclaimer.

A query traverses the visible graph and may exact-rerank a declared candidate
count. Stable-ID eligibility participates during layer-zero graph traversal;
restrictive admitted sets no larger than `ef_search` use exact filtered
execution. Typed predicate construction remains outside this API.

## Filtering

Stable-ID bitmaps and typed doc-value predicates may run before, during, or
after graph traversal according to the physical plan. The explanation records
which strategy ran and whether it can reduce recall.

The current bounded implementation accepts a stable `ObjectId` allowlist.
Navigation may visit admitted and non-admitted connector nodes, but a distinct
eligible set is maintained during layer-zero expansion, so disallowed nodes do
not consume eligible candidate capacity. Receipts report
`StableIdEligibilityTraversal` and `FilteredApproximateTraversal`. When the
complete visible allowlist cardinality is at most `ef_search`, execution scores
that set exactly and reports `StableIdAdaptiveExact` and
`ExactFilteredCandidates`.

## Result contract

Every ANN result names:

- approximate status;
- index/build identity and snapshot CSN;
- metric, `k`, `ef_search`, filters and candidate count;
- whether exact reranking ran;
- returned distance/score and stable ID; and
- measured quality profile applicable to that build, when available.

A proof can attest to execution, inputs, graph identity, candidates and exact
reranking. It cannot claim global nearest-neighbor optimality unless the query
used the exact oracle.

## Quality gates

The G4 bounded correctness profile requires deterministic exact-oracle recall
evidence with recall@10 at least 0.95. The 1,000,000-vector, 384-dimension
latency and memory target remains a G7 performance profile under the
[microsecond contract](../performance/microsecond-first.md).

Receipts also report build time, ingest/update/delete cost, graph bytes per
vector, tombstone ratio, rebuild time, p50/p95/p99/p99.9 and recall
distribution across at least ten deterministic query sets.

## Verification

Current tests additionally prove unchanged base identity and generation-record
counts across foreground mutations, reopen and effective exact equivalence,
strict maintenance WAL decoding, bounded consolidation, preservation of later
object versions, stale-base rejection, old-or-new interruption recovery and
configured retention, policy bounds, due-plan generation, pin-safe old-root
retention, unpin plus page-vacuum collection, and filter-aware recall against
the exact oracle. Background scheduling and page-buffered traversal remain
future production work.
