# Native resource governor v1

Status: admission foundation and first engine routing implemented; P2 remains open

`NativeResourceGovernor` is the single in-process admission authority intended
for Native SQL, structures, lexical/vector search, WAL, recovery, and
administrative work. It introduces no protocol or durable state. Its immutable
policy is frozen by
[`native-governor-policy-v1.schema.json`](../../contracts/json-schema/native-governor-policy-v1.schema.json).

## Policy derivation

`NativeGovernorPolicy::derive` fails closed unless:

- the calibration hardware fingerprint equals the current static profile;
- `thread_scaling` has a stable recommendation from one internally consistent
  placement adapter (`unbound` or Linux hard affinity);
- the recommendation is inside its recorded logical-processor boundary; and
- total memory is known and current available memory can preserve 15 percent
  of total host memory as headroom.

The calibrated worker recommendation is a ceiling, not a target that every
request should consume. A system reserve is removed before any request token
is exposed. The storage I/O cap uses the independently recomputed `io_scaling`
recommendation, bounded to 64. If that curve is unavailable, the governor
admits only one I/O slot. Latency, bulk, and mixed modes derive different
per-class ceilings without changing the global ceiling.

The same immutable policy derives a waiting bound of 64 slots per schedulable
thread, clamped to 64–4096, and freezes a foreground burst limit of 16. These
values are present in the JSON policy and independently recomputed by the
semantic checker; runtime queue behavior cannot silently choose different
limits.

## Admission invariants

Every request declares compute threads, I/O slots, and arena/scratch bytes.
Admission reserves the global counters and the workload-class counters as one
rollback-safe operation. A failure returns no partial allocation. A permit is
RAII-owned; cancellation, error propagation, or normal scope exit returns all
three resources.

Nested work cannot call an independent pool and multiply the host budget. It
must subdivide a parent `GovernorPermit`. Nested permits atomically consume only
the parent's allocation and return it on drop. The Rust lifetime prevents a
parent permit from being released while a child subdivision is live.

The seven v1 classes are foreground point, foreground bounded, mutation, bulk,
maintenance, recovery, and administrative. Point work remains single-threaded
by policy. Global accounting prevents overlapping class ceilings from
oversubscribing the calibrated host.

## Bounded priority queue

`admit_queued_owned` provides synchronous bounded waiting without allowing
later arrivals to bypass queued work. Foreground point and mutation requests
use the high-priority FIFO, bounded foreground work uses the normal FIFO, and
bulk, maintenance, recovery, and administrative work share the background
FIFO. When background work is waiting, at most 16 preferred foreground
dispatches may complete before one eligible background request is forced.

Queue capacity includes the selected ticket until it claims tokens. A full
queue rejects without inserting a ticket. Timeout and cancellation remove both
selected and ordinary waiting tickets, select the next eligible request, and
wake the queue. Cancellation handles are bound to one governor so a foreign
handle fails closed. Direct nonqueued admission cannot bypass an existing
queue.

`QueuedGovernorPermit` records initial queue depth and queue time. Explicit
completion adds execution time and engine-observed physical-I/O time as
separate fields. Converting it into a clonable transaction allocation is
explicit and discards component timing rather than inventing a receipt.

## Routed execution surfaces

`NativeDatabase` can be given one caller-verified, process-local governor with
`set_resource_governor`. Opening or creating a data directory never calibrates
the host implicitly, invents a policy, or persists admission state. Removing
the governor changes no durable bytes.

Callers that require governed recovery use `open_with_resource_governor` (or
its pending-target equivalent). It acquires recovery capacity before opening,
verifying, replaying, repairing, or removing any durable file and leaves the
same governor attached to the returned handle. The legacy `open` remains an
explicit ungoverned compatibility surface; attaching a governor after it
returns cannot retroactively account for recovery.

`set_resource_governor_with_queue_wait` opts routed database calls into the
bounded priority queue with one caller-selected maximum synchronous wait. The
existing setter preserves zero-wait fail-fast behavior. A real scalar point
read test queues behind externally held capacity, resumes after RAII release,
and separately proves timeout removes its ticket.

The routed read slice holds an RAII permit for the complete call on:

- catalog and relational point/range reads, prepared SQL binding, and
  current-root prepared execution;
- scalar, hash, set, list, stream, and sorted-set point/bounded reads;
- lexical search and approximate, filtered, and exact ANN search; and
- complete all-engine snapshot materialization and native lexical/vector
  hybrid retrieval.

Point reads currently reserve one compute token, one I/O token, and a 64 KiB
arena envelope. Bounded SQL and approximate ANN reads reserve one compute
token, one I/O token, and a 16 MiB arena envelope. Exact ANN reserves up to the
foreground-bounded class compute ceiling only when a matching persistent pool
is installed; otherwise it remains single-threaded. An admitted metadata
preflight reads the effective vector count, then exact ANN reserves a checked
scratch envelope from that count, query dimension, two vector copies, result
metadata, and the fixed bounded-request base. Insufficient class memory rejects
before state materialization or worker dispatch. Other current memory envelopes
remain conservative fixed bounds pending request-plan accounting.
Admission rejection is exposed as `NativeRuntimeError::ResourceAdmission` (and
through `SqlError::Runtime` for SQL) before engine work begins. Tests hold the
same class capacity externally, prove that real SQL/structure/ANN entry points
reject, then prove permit drop restores service.

Serialized and detached optimistic write transactions acquire an owned
mutation permit before snapshot materialization. The permit is retained by the
`NativeWriteBatch` through staging, queueing, group or individual commit,
explicit rollback, or ordinary drop. Cloning a batch clones a handle to the
same allocation rather than acquiring another allocation; tokens return only
after the final clone drops. This prevents work from escaping admission merely
because its lifetime extends beyond `NativeDatabase::begin`. Owned and borrowed
permits share the same counters and nested-subdivision rule.

Expiry sweep, structure compaction, lexical-search compaction, ANN consolidation
planning, and ANN consolidation publication hold maintenance permits across
their complete operation. Maintenance rejection therefore happens before
scanning or durable mutation.

Snapshot pin/unpin and historical materialization, checkpoint, page vacuum,
retired-generation collection, and immutable-blob collection similarly hold an
administrative permit. Internal composition is explicit: `pin_current` calls
the already-admitted checkpoint implementation rather than reacquiring the
same class and self-blocking.

WAL retention and online backup also hold one administrative permit for their
complete operation. Backup invokes the already-admitted checkpoint primitive,
then retains the allocation through source synchronization, copying, hashing,
manifest publication, verification, and atomic promotion. The governed offline
verify and restore variants reject before reading or creating files when
capacity is unavailable; restore keeps the permit through logical recovery and
destination promotion.

## Persistent execution workers

`NativeExecutionTopology`, frozen by
[`native-execution-topology-v1.schema.json`](../../contracts/json-schema/native-execution-topology-v1.schema.json),
derives exactly the governor's schedulable worker
count from the matching `HardwareProfile`. When per-processor placement is
available it selects one hardware thread from each physical core before any
SMT sibling, groups workers by NUMA node, and records logical processor,
socket, core, node, and SMT rank. Linux workers fail pool construction if hard
affinity to their declared processor cannot be installed. Platforms without
complete placement use one explicit unbound portable pool rather than
inventing NUMA identity.

`NativeExecutionPool` owns persistent workers and one FIFO per NUMA node.
Workers consume their local queue first and may steal only when it is empty.
Every submitted deterministic batch owns an `OwnedNestedGovernorPermit`; all
child permits are reserved before the first job is dispatched, so partial
admission never launches partial work. Panicking operations are contained,
return all nested tokens, and do not kill the worker pool. Results merge in
input order independently of worker scheduling. A worker increments its
process-level completion counter before signaling the waiting caller. A
returned batch therefore cannot race ahead of completion observability; this
ordering remains true for successful and panicking jobs.

The pool is installed only through
`set_resource_governor_with_execution_pool`, which verifies hardware identity
and the complete worker budget. Replacing or clearing the governor removes the
pool, preventing a stale topology from surviving policy replacement. Current
exact ANN ranking is the first real engine path connected to this executor: it
reserves the complete foreground-bounded compute allocation, scores canonical
vector batches on persistent workers, and merges with the same distance/Object
ID ordering as the serial oracle. Approximate HNSW traversal remains bounded
and sequential; `hyphae-native-ann` no longer creates or consumes an implicit
global Rayon pool.

`search_vector_exact_latest_profiled` and its filtered counterpart return the
first engine-level execution receipt. The receipt separates metadata-planning
admission from ranking admission, reports queue ticket/depth/time when either
scope waited, records the exact compute and memory plan, carries the immutable
snapshot CSN, and reports batches returned by that query's executor invocation.
Batch accounting is not inferred from a process-global counter, so concurrent
queries cannot contaminate one another's evidence. The compatibility methods
delegate to these profiled paths and return only their canonical hits.

Large relational primary-key ranges are the second connected engine path.
After a single-threaded planning scope selects immutable B+tree leaves, the
execution scope reserves matched compute and I/O tokens per worker. Each job
owns a nested one-compute/one-I/O subdivision, reads through snapshot-frozen
page/blob handles, and returns rows for deterministic segment/key-order merge.
The profiled receipt reports both admission scopes and the query-local batch
count; limits at or below 256 rows retain the direct cached visitor.

Large native hash scans use the same one-compute/one-I/O child subdivision and
snapshot-frozen readers. They retain the direct visitor through 256 fields,
then merge segment results in exact binary field order while preserving TTL,
tombstone, and complete-cardinality validation. This is the first structure
family connected to the persistent executor.

## Open P2 work

This slice does not claim P2 closure. The remaining work is to route any proof
and long-running administrative surfaces outside this crate through the same
authority; replace fixed arena envelopes with request-plan memory accounting;
extend engine work receipts beyond exact ANN and relational ranges; connect the
pool to bulk build, additional SQL, structures, and lexical operators; derive a calibrated cross-node steal
threshold; and run measured interference and physical-core/SMT scaling
matrices.
Deterministic concurrency 1/8/32/64, queue saturation, cancellation, foreground
preference, and forced background-progress tests now cover the in-process
admission invariant, but they are not a substitute for bare-metal latency
evidence.
