# Native ANN semantics v1

Status: normative target contract; deterministic in-memory HNSW kernel,
canonical `f32` admission, three metrics, exact oracle, mutation rebuild,
build identity and fail-closed canonical restore are implemented
experimentally; search B+tree, WAL, MVCC delta, filtering and background
generation publication remain pending

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

Canonical rebuild orders insertions by creating CSN then object ID and derives
randomness from index ID plus definition digest. Parallel optimization may not
change logical results without a new physical-build identity.

The first executable kernel is `hyphae-native-ann`. V1 bounds `M` to 2 through
64, requires `ef_construction >= M`, derives each node level from BLAKE3 over
the index-definition digest and object ID, and retains at most `M` directed
neighbors per layer. Neighbor selection uses exact metric distance followed by
object ID. An update or delete currently rebuilds the complete graph in
canonical order; this is a correctness reference, not the target foreground
mutation cost.

`IndexSnapshot` exports definition, vectors with creating CSNs, graph nodes,
entry point, maximum level and build identity. Restore reconstructs the graph
from the vector records and rejects any snapshot that differs from that
canonical build. The native search engine has not yet assigned page keys or
WAL operations to these records, so the kernel alone is not durable ANN.

## MVCC and mutation

New/updated vectors enter a transactional mutable delta. Deletions create
versioned tombstones. Background graph consolidation builds from a pinned
snapshot and publishes a new generation atomically.

A query merges visible delta and graph candidates, applies snapshot filters,
and may exact-rerank a declared candidate count.

## Filtering

Stable-ID bitmaps and typed doc-value predicates may run before, during, or
after graph traversal according to the physical plan. The explanation records
which strategy ran and whether it can reduce recall.

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

The primary bounded target is 1,000,000 vectors, 384 dimensions, top 10,
recall@10 at least 0.95, with latency and memory measured under the
[microsecond contract](../performance/microsecond-first.md).

Receipts also report build time, ingest/update/delete cost, graph bytes per
vector, tombstone ratio, rebuild time, p50/p95/p99/p99.9 and recall
distribution across at least ten deterministic query sets.

## Verification

Tests cover metric goldens, admission failures, deterministic rebuild,
snapshot visibility, update/delete, filter strategies, exact-rerank identity,
corruption and interrupted build recovery, quality regression and the explicit
approximation label.
