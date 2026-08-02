# Native ANN semantics v1

Status: normative target contract; deterministic HNSW, canonical `f32`
admission, three metrics, exact oracle, catalog definitions, native search
B+tree generations, WAL mutations, all-engine MVCC visibility, batch rebuild,
historical snapshots and fail-closed recovery are implemented experimentally;
buffered graph traversal, filtering, tombstone compaction and background
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
canonical build.

## Implemented durable generation

The native runtime stores ANN in the same copy-on-write search B+tree as
lexical state:

- `0x05 + index ObjectId` selects the current `HYANNM01` generation metadata;
- `0x06 + index ObjectId + build identity + object ObjectId` stores one
  `HYANNV01` vector with its creating CSN; and
- `0x07 + index ObjectId + build identity + object ObjectId + u16 layer`
  stores one `HYANNG01` neighbor list.

Every identity component is big-endian in the key. The 32-byte build identity
is content-bound by the kernel. Vector components remain canonical
little-endian `f32`; graph neighbors are stable 128-bit object IDs.

One commit groups all vector mutations by index, applies them in WAL order,
builds one private canonical replacement, persists its immutable records, and
switches the metadata pointer in the transaction's search root. The root is
published under the same CSN as catalog, relational and structure roots. Old
generation records remain physically reachable from newer B+tree roots but
are logically ignored; reclamation is pending.

Open and snapshot materialization scan the selected generation, validate every
ANN physical key/value, reconstruct the complete `IndexSnapshot`, and require
`HnswIndex::restore` to reproduce it exactly. Unknown indexes, missing
metadata, orphan generation records, malformed vectors/layers, count
divergence, bad neighbors or a noncanonical build fail the complete root.
Queries currently traverse this validated in-memory materialization, not
buffer-pool pages directly.

## MVCC and mutation

New/updated vectors enter the transaction-private replacement generation.
`upsert_vectors` admits a duplicate-free batch atomically and performs one
canonical rebuild. Single-vector upsert and delete preserve read-your-writes.
At commit, all mutations for an index are rebuilt once with the assigned CSN.
Retained root sets preserve historical generations.

The current implementation does not yet retain a separate mutable delta or
versioned tombstone set: foreground delete builds a replacement generation.
Background consolidation, tombstone compaction and delta/graph query merge
remain target work.

A query traverses the visible graph and may exact-rerank a declared candidate
count. Snapshot filtering remains pending.

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

Current tests cover metric goldens, admission failures, deterministic rebuild,
batch atomicity, historical snapshot visibility, optimistic write conflicts,
update/delete, exact-rerank identity, reopen equivalence, orphan/corrupt
generation rejection, and all seven cross-engine commit interruption
boundaries. Filter strategies, interrupted background builds, page-buffered
traversal and the complete quality matrix remain pending.
