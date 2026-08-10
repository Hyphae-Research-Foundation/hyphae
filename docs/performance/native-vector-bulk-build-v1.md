# Native vector bulk-build experiment v1

Status: P4 experimental interface; no durable generation or release claim

This contract isolates vector corpus construction from online HNSW mutation.
It currently qualifies one deterministic partitioned-HNSW candidate against
the canonical serial HNSW and exact-flat oracles. It does not select the final
P4 production algorithm.

## Plan

`HnswPartitionPlan` validates every vector and rejects duplicate object IDs.
It recursively bisects the corpus with distinct definition-derived projection
axes, orders equal projections by stable object ID, and assigns frozen target
sizes so final partition cardinalities differ by at most one. The plan identity
commits to the index definition, partition boundaries, object IDs, creating
CSNs, and vector bits. Input arrival order therefore cannot change the plan.

The projection tree is a deterministic geometric partitioner, not a trained model.
It is deliberately a bakeoff baseline. Centroids, radii, multi-axis routing,
bounded cross-partition links, and learned selection remain P4 work.

## Governed build

`NativeDatabase::build_partitioned_hnsw_experimental` transfers each planned
partition by ownership into the shared persistent execution pool. It does not
spawn an algorithm-private thread pool. The global governor admits the build as
`Bulk`, bounds worker count by both the calibrated policy and partition count,
and reserves vector storage plus a conservative maximum-layer graph budget.

The returned `NativePartitionedHnswBuildReceipt` records source vectors,
partitions, reserved workers and memory, worker batches, admission timing, and
complete elapsed time. Without an installed governor the same interface runs a
serial oracle path.

The diagnostic runner compares canonical single-HNSW, serial-partitioned, and
governed-partitioned build time and recall against exact top-k:

```text
cargo run --release --locked -p hyphae-native-runtime \
  --example partitioned_hnsw_bakeoff -- \
  <source-commit> <vectors> <dimension> <partitions> \
  <selected-partitions> <queries> <k> \
  <hardware-probe-path> <governor-policy.json>
```

The hardware probe path must be the same path used to discover the profile
from which the governor policy was derived. Validate the emitted JSON before
using it as diagnostic evidence:

```text
python tools/check_native_vector_bulk_bakeoff.py \
  <receipt.json> <source-commit>
```

The receipt and checker remain fail-closed and disclose every missing P4 gate
dimension. They cannot declare closure.

The selected-partition argument enables a separate centroid-ranked query cell.
The receipt keeps full-fanout and selected-fanout recall and timing distinct;
selection never changes the default full-fanout API or hides its recall cost.

Candidates run sequentially. After the single-HNSW phase, the runner retains
only bounded query vectors, exact top-k identities, and recall counts; it drops
the index, regenerates the byte-identical corpus, and then runs each
partitioned phase. Serial and parallel partitioned identities must match. This
prevents the bakeoff itself from holding three complete indices and multiple
corpus copies at once.

Assembly does not trust worker ordering. It reconstructs the geometric order
from built child entries, rejects empty, repeated, misplaced, or
definition-mismatched children, recomputes the plan identity, and only then
derives the aggregate build identity. This validation uses references to child
vectors and does not retain a second full corpus copy.

`PartitionedIndexSnapshot` now freezes the definition, recursive-plan identity,
aggregate build identity, and canonical child snapshots. Restore validates each
child graph, reconstructs every recursive boundary without cloning vector
payloads, and rejects reordered children or either identity mismatch. This is
the persistence-facing logical format; it is not yet wired to page/WAL
publication.

## Query semantics

Exact search runs against every disjoint child generation and performs one
canonical `(distance, object_id)` top-k merge. It is identical to exact-flat
ranking.

Default approximate search fans out to every child, merges canonically, and
reports the existing approximate-traversal recall risk. An explicit
`search_selected` experiment ranks deterministic centroid/radius summaries,
visits no more than its caller budget, and reports the exact selected partition
identities. It remains a bakeoff route: it has no cross-partition navigation
links or accepted-corpus quality policy, and therefore never replaces full
fanout silently.

## Durability boundary

The candidate is process-local. The build API does not append pages or WAL,
does not move the current ANN root, and does not publish a generation. Durable
checkpoints, online deltas, interruption/restart, cross-partition navigation,
atomic generation publication, consolidation, and reopen belong to the next
P4 slices.

## Qualification still required

P4 remains open until all roadmap corpora prove recall@10 of at least 0.95 for
each corpus and reproduce generation identities at 1, 8, 32, and maximum
calibrated workers. Evidence must also include build throughput, peak RSS,
write amplification, query latency, recovery, update/delete/consolidation, and
the frozen million-vector 384-dimensional build budget. Epoch HNSW and a
Vamana/DiskANN-style candidate must be measured through the same contract
before physical-strategy selection.
