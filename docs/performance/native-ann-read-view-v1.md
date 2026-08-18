<!-- SPDX-License-Identifier: Apache-2.0 -->

# Native ANN read view v1

Status: planned implementation contract; no P4 or G7 closure claim

`NativeAnnReadView` is an owned, index-scoped, immutable current-root read
authority. It separates one governed physical hydration from the repeatable ANN
query hot loop. The v1 consumer is selected approximate search with
`metric-bound-adaptive-v1`; filtered, exact, hybrid, and compatibility entry
points remain outside this vertical until they adopt the same retained-view
authority explicitly.

The view is process-local. It is not a durable format, a cache file, or an
alternative MVCC authority. Opening or dropping one must not append WAL, move a
root, publish a generation, or change a visible CSN.

## Authority and ownership

Opening a view captures one committed coordinator snapshot and binds all of the
following values:

- data-directory identity, visible CSN, catalog version, and complete root-set
  identity;
- the exact catalog and search roots used by the load;
- vector-index object ID and canonical definition digest;
- durable base kind, ordered child identities, aggregate base build identity,
  and base vector count;
- delta count, delta-byte count, next sequence, and complete view identity; and
- routing-policy identity and logical partition count.

The catalog lookup is exact and index-scoped. The search load may read only the
target metadata and the target vector, graph-layer, and delta prefixes. It must
not load the complete catalog, restore another ANN index, or materialize SQL,
structure, lexical, or hybrid state. Corruption owned by the target fails the
open; corruption outside the selected index does not enter the view and cannot
consume its plan.

The returned object owns one validated ANN base and one exact delta overlay.
Cloning the public handle clones an `Arc`-like handle to that same state and
permit; it must not clone vectors, graph nodes, deltas, or governor admission.
The last handle drop destroys the owned state and releases its retained memory.
No borrowed page, B-tree cursor, transaction lifetime, or mutable database
reference may escape into the view.

## Governed open and one-time hydration

Opening follows one fail-closed sequence:

1. Acquire a small bounded planning permit and capture a coordinator snapshot.
2. Resolve the exact catalog object and target ANN metadata at those roots.
3. Derive conservative entry, encoded-byte, restore, and live-view memory
   ceilings from the validated definition and metadata.
4. Acquire a foreground-bounded hydration permit covering CPU, physical I/O,
   encoded entries, restored graph, delta overlay, and peak overlap before the
   first target-prefix scan.
5. Hold the snapshot/root pin while a bounded visitor reads each target prefix.
   A count or byte beyond the plan, cancellation, missing record, malformed
   child, identity mismatch, or root/metadata substitution aborts without a
   view.
6. Restore and validate the target base cooperatively, including vector
   ingestion, graph layers, child and aggregate identities, routing summaries,
   delta bounds, and view identity.
7. Drop encoded physical entries and atomically transfer or downgrade the
   hydration allocation to an owned memory-only permit sized for the retained
   base, summaries, and deltas. There must be no instant when live view memory
   is unaccounted.
8. Release the physical snapshot/root pin and return the immutable view plus
   its open receipt.

The memory-only permit requests zero compute threads and zero I/O slots and
remains live through every handle clone. A rejected, canceled, corrupt, or
panicking open returns no partial view and releases planning, hydration, child,
and retained allocations. The implementation may reduce the retained memory
reservation after measuring owned capacity, but it may never exceed the
pre-admitted peak or claim allocator precision it did not measure.

## Query hot-loop contract

Every query validates the query and options, then obtains its own bounded
foreground CPU and scratch-memory admission from the same governor generation
that owns the view. Parallel routed fanout subdivides that parent permit through
the persistent `NativeExecutionPool`. It does not create a pool, acquire child
capacity independently, or multiply the machine budget.

One immutable routing plan is shared by all child tasks. For a selected budget,
the first wave executes the first canonical child through the persistent pool;
its one selected partition, one executed worker batch, and one wave are explicit
receipt evidence rather than hidden inline work. If canonical merge cannot
certify the next omitted partition, later waves execute only the new suffixes
needed to reach cumulative prefixes `2, 4, ...`, followed by the exact preferred
budget. If that final preferred prefix remains uncertified, one complete-fallback
wave executes every remaining child. An explicit complete-fanout request remains
one complete wave because it does not authorize pruning. Ordered merge and exact
delta overlay must be byte-for-byte equivalent to serial `search_routed` for
worker capacities 1, 2, 4, and the calibrated maximum.

Every selected-prefix certificate is checked again after exact delta upserts
and tombstones are applied. It is accepted only when the merged result still
contains `k` hits and the next omitted lower bound is strictly greater than the
final kth distance. A tombstone-invalidated certificate continues widening;
the view never reports selected routing using only a pre-delta kth bound.

After a successful open, a query must perform none of the following:

- coordinator snapshot or catalog lookup;
- page, B-tree, WAL, manifest, or blob read;
- metadata decode, target-prefix scan, or physical validation;
- HNSW restore, routing-summary reconstruction, or corpus clone; or
- all-engine or multi-index materialization.

Per-query allocation is limited to the routing plan, child results, merge heap,
delta candidates, and final receipt under the query permit. Query cancellation
is cooperative before planning, before each prefix wave, before each child
execution, between widening waves, and before final merge. Cancellation or
child failure releases every query subdivision but does not poison, mutate, or
evict the reusable view. Concurrent queries may share the immutable view; each
owns independent query admission and cancellation.

## Root advance, vacuum, and reopen

A committed root advance never mutates an existing view. The old view continues
to answer from its captured base and delta identities; a newly opened view sees
the newer committed authority. An API that promises latest-root semantics must
open or select a view whose root-set identity still equals the current root. It
must not silently label an older retained view as latest.

The opening snapshot pin prevents vacuum from reclaiming physical input during
hydration. Once open completes, the view is fully owned and releases that pin.
Vacuum may then reclaim old pages or retained physical generations without
invalidating the view. A test must vacuum the captured root after hydration and
prove the existing view still returns identical results without physical I/O.
Dropped views leave no hidden pin or retention reference.

Views are not serialized across process restart. Reopen creates a new view from
the selected committed root, repeats one governed hydration, and must reproduce
the same authority identities and results when the durable root is unchanged.
Prior-or-complete recovery remains the source of durable truth; no view may be
recovered from process memory or used to conceal invalid durable state.

## Governor reconfiguration guard

The retained memory permit binds a view to one immutable governor policy and
governor generation. Installing, removing, or replacing the database governor
while any view from that database generation remains live must fail with an
explicit outstanding-view error. It must not split memory accounting between
old and new governors or allow new queries to charge CPU to a different policy.

Reconfiguration succeeds only after the last view releases its memory permit.
The database may then install the new governor and open fresh views. Policy
identity and governor generation are included in both open and query receipts.

## Receipts

The view-open receipt records:

- root-set authority, visible CSN, catalog/search roots, index ID, definition
  digest, base build identity, view identity, and routing policy;
- base kind, child identities, logical partitions, base vectors, and exact
  delta records/bytes;
- planned physical entries/bytes, observed physical entries/bytes, planned
  peak memory, retained memory, and one hydration/restore count;
- governor policy/generation, queue time, hydration CPU time, physical-I/O
  time, executed workers/batches, and cancellation outcome; and
- source commit and executable identity when a qualification harness supplies
  them.

Every query receipt binds the open-receipt identity and reports requested and
selected partitions, total partitions, routing outcome, next lower bound,
base/view identities, exact delta candidates, workers, worker batches, waves,
queue/CPU time, and query scratch bytes. It also reports
`hydration_performed=false`, zero query physical-I/O bytes, and zero restore
count. A query that touches storage or hydrates again fails the local gate; the
receipt must not hide that work inside execution time.

## RED/GREEN local acceptance

Implementation starts with deterministic local tests that are red against the
current per-query restore path and green only when these conditions hold:

1. **Target scope:** two ANN indexes plus SQL, structures, and lexical data open
   one target view under full-state/full-catalog fail guards. Target corruption
   fails; unrelated-index corruption is ignored.
2. **Bounded open:** understated metadata stops the bounded physical visitor
   before it exceeds admitted entries or bytes and before restore begins.
3. **One hydration:** one open followed by at least 10,000 selected queries
   records exactly one target scan and restore. Query-time fail-on-hydration,
   fail-on-page-read, and fail-on-catalog-read guards remain green.
4. **Deterministic fanout:** geometric prefixes reproduce the serial oracle and
   never execute one child twice. Worker capacities 1, 2, and 4 reproduce the
   same certified prefix, requested full fanout, and budget-triggered complete
   fallback, including exact delta upserts and tombstones.
5. **Admission and RAII:** open rejection occurs before physical scan; retained
   memory remains charged across clones; every query CPU/scratch allocation
   returns after success, cancellation, error, and panic; the last view drop
   returns retained memory exactly once.
6. **Cancellation and reuse:** cancellation during scan, restore, first wave,
   and widening returns no partial result. The same view remains usable after a
   canceled query.
7. **Lifecycle:** root advance leaves the old view stable, a new view observes
   the new view identity, vacuum after hydration cannot break the old view, and
   unchanged durable state reopens to identical identities and results.
8. **Reconfiguration:** governor replacement/removal fails while any clone is
   live and succeeds after the last drop, with no capacity leaked in either
   generation.

These tests are functional authority, not latency evidence. They must remain
small enough for ordinary local CI and must not weaken ANN recall, durable
validation, cancellation, or governor accounting to reduce runtime.

## G7 integration boundary

Each G7 ANN cell opens and validates its `NativeAnnReadView` during setup,
outside both warmup and the one-million-observation interval. The open receipt,
hydration duration, peak/retained memory, and any physical I/O are preserved as
separate setup evidence. Warmup and observations reuse that exact view and fail
if a query scans pages, restores an index, changes root authority, or reports
`hydration_performed=true`.

The view does not alter the frozen corpus, 100,000 warmup count, 1,000,000
observations, concurrency 1/8/32 matrix, exact recall oracle, control/interference
pairing, or release-source authority. Local acceptance and a clean G7 wiring
only make the path eligible for dedicated measurement. They do not close P4 or
G7 without the complete accepted bare-metal evidence.
