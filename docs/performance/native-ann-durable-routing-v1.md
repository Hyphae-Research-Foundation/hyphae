# Native ANN durable routing v1

Status: experimental implementation contract; no P4 or G7 closure claim

This contract exposes deterministic selected-partition routing for a durable
partitioned HNSW generation. It separates the logical on-disk partition layout
from the number of execution workers available on one machine.

The existing approximate-search API remains full fanout. Selected routing is a
separate checked API until accepted corpora prove its recall policy per query.

## Request and receipt

The caller supplies a non-zero preferred partition budget. The runtime returns
the ordinary ANN search receipt plus:

- the requested preferred budget;
- the selected partition indexes in deterministic routing-score order;
- the total available partition count; and
- an explicit routing mode: certified selected partitions, requested complete
  fanout, budget-triggered complete-fanout fallback, or single-generation
  fallback;
- the stable routing-policy identity; and
- the number of child-search workers that actually executed.

Zero is rejected. A budget at least as large as the durable child count runs
complete fanout in one wave and must equal the existing default result. A
smaller budget is a ceiling, not an instruction to execute every preferred
child. The runtime executes cumulative canonical prefixes `1, 2, 4, ...`,
ending at the exact preferred budget when that budget is not a power of two.
After every prefix it merges the accumulated child results and certifies the
next omitted child's metric lower bound. Certification may therefore stop at
any non-empty prefix within the preferred budget. If the preferred prefix
still cannot prove that the omitted partitions cannot improve the current kth
hit, one final wave executes every remaining child and reports
`FullFanoutBudgetFallback`. A consolidated or legacy single generation executes
its one graph and reports
`SingleGenerationFallback`; it must not pretend that partition pruning ran.

Filtered selected routing is outside v1. The existing filtered API retains its
adaptive exact/full behavior rather than silently applying an unqualified
partition policy.

## Determinism and lifecycle

The `metric-bound-adaptive-v1` policy ranks persisted-generation summaries by
a certified metric lower bound, projection-interval distance, and partition
index. Squared-L2 uses the centroid ball; cosine uses a unit-sphere chord ball;
negative dot uses the centroid plus Cauchy radius bound. Stored radii are
inflated and computed lower bounds are reduced by a dimension-scaled floating
point margin; numerical uncertainty can only widen the search, never certify
an unsafe omission. The receipt lists the exact searched order and the next
bound that caused fallback, when present.
Repeating a query against the same view, reopening the data directory, or
changing only execution-worker capacity must not change it.

Each widening step executes only the newly admitted suffix; already completed
children are never searched again. The receipt's selected partitions are the
exact accumulated prefix. Workers are the maximum concurrent workers used by
one wave, worker batches accumulate across waves, and waves include the first
prefix and every widening or complete-fallback suffix. This evidence makes a
prefix-one certification distinguishable from a preferred-budget fanout.

Base hits are merged with exact online delta upserts and tombstones before the
ordinary canonical `(distance, object_id)` top-k truncation. Consolidation of a
non-empty partitioned base rebuilds the same canonical geometric layout and
preserves its logical partition fanout, capped only when the effective vector
count is smaller because v1 does not encode empty child partitions. Worker
capacity affects execution only and cannot change memberships or the aggregate
identity. Removing every vector publishes the canonical empty single base and
reports the explicit single-generation fallback.

Selection certification is revalidated after that exact delta merge. The next
omitted partition's lower bound must remain strictly greater than the final
kth distance, and the merged result must still contain exactly `k` hits. A
tombstone that removes a selected base hit can invalidate an otherwise correct
base certificate; the query then continues the same geometric widening plan or
executes complete fallback. Delta application may improve a certificate, but
it can never preserve one using the pre-delta kth distance alone.

## Quality gate

Selected routing cannot become the default or enter release G7 evidence until
all of the following hold on every accepted corpus:

- recall at 10 is at least 0.95 for every measured query, not only in
  aggregate;
- complete fanout is byte-for-byte equivalent to the existing default result;
- selected partition order and results reproduce after reopen;
- online upsert, delete, consolidation, interruption, and recovery preserve
  the selected view semantics; and
- the receipt binds the routing policy, logical partition count, execution
  worker count, source commit, corpus identity, and build/view identity.

The `1,000,000 x 384` corpus is the frozen 1.0 release lane. Independent
`1,000,000 x 768` and `1,000,000 x 1,024` lanes belong to a later release and
require their own memory, NUMA, SIMD, recovery, and recall qualification.

## Current boundary

The checked current-root surface now plans and loads only the requested ANN
index, applies fail-closed physical entry and byte ceilings before restore,
observes cancellation throughout bounded B+tree traversal and HNSW restore,
and executes routed child searches through the shared persistent worker pool.
Its receipt reports actual workers, worker batches, widening waves, base and
view identities, exact delta candidates, routing outcome, and the next lower
bound. Target corruption fails the request while equivalent corruption in an
unrelated index is outside that index-scoped authority.

The surface is still one-shot: every `selected_latest` call reopens and
restores the requested index. It therefore does not yet prove allocation or
microsecond hot-loop behavior. A retained owned `NativeAnnReadView` must bind
one immutable root and keep only its admitted index memory between searches;
per-query execution then admits CPU without reading pages or restoring the
graph again. Non-empty consolidation now preserves the partitioned base, but
an exact-source qualification must still prove that lifecycle together with
the retained view. These remaining properties are required before G7 or P4
closure.
