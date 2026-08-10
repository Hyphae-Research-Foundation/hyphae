# Native ANN initial bulk publication v1

Status: implementation contract; no P4 or G7 closure claim

This contract turns one already-created, empty native vector index into a
durable partitioned HNSW generation. Graph construction happens outside writer
admission under the shared `Bulk` governor and persistent execution pool.
Publication uses the ordinary page, WAL, MVCC, and root-set commit protocol.

It does not select the final P4 algorithm, qualify recall, or replace the
incremental delta and consolidation lifecycle.

## Preconditions

The vector index must already exist in the committed catalog with:

- an empty selected base generation;
- no online delta records;
- no retained generations; and
- the inverted-B+tree search format.

Creating the empty index is an ordinary independent commit. A failed, canceled,
or stale bulk build therefore leaves a valid empty index rather than a partial
generation.

The input batch must contain unique object IDs and vectors admitted by the
catalog-bound definition. An empty input or zero requested partitions is
rejected. The initial V4 lifecycle admits at most 221 effective child
partitions when one generation is retained. The runtime derives a lower limit
from the catalog-bound retention policy (220 for two and 123 for 64 retained
generations), reserving space for the initial partitioned generation, later
single generations, and the current consolidated base. A larger effective
request fails before planning or governor work, rather than producing a
candidate that cannot complete its durable lifecycle.

## Plan and governed build

Planning captures the complete root-set identity, selected empty-base identity,
base-plus-delta view identity, and the next candidate CSN. Every source vector
is bound to that candidate CSN before geometric partitioning. The plan identity
therefore commits to the same vector-version CSN that durable publication will
use.

One shared `NativeResourceGovernor` admission reserves the candidate's complete
worker and build-memory budget as `Bulk` before validation, geometric
projection, sorting, or identity construction begins. Those planning phases
and child ingestion are cooperatively cancelable. The build then transfers
disjoint partitions by ownership to the existing `NativeExecutionPool`; the
algorithm must not create another thread pool. The returned evidence records
the `PartitionedHnswV1` builder identifier, input and aggregate identities,
vector and partition counts, planned workers and memory, executed worker
batches, queueing, and elapsed time. An optional thread-safe progress callback
reports planning start/completion and monotonic completed child generations so
a controller can distinguish active work from a stalled process without
weakening the build.

The current release qualifies this accounting with one ANN index in an
otherwise bounded database fixture. Authority capture and consolidation still
hydrate the complete ANN root when multiple large indexes coexist. The current
exact-search entry point materializes the complete all-engine state before its
governed query workspace runs. Those unrelated-index and unrelated-engine
allocations are not represented by the target plan's receipt. Broader
qualification therefore requires index-scoped loading or explicit full-root
admission and is not part of the 1.0 G7 claim.

Cancellation wakes a queued governor admission and is then observed
cooperatively between canonical node insertions. All admitted workers are
joined before execution cancellation returns. No candidate is returned, and
all governor tokens must be released, after cancellation, invalid input,
worker failure, or panic. On success the build allocation is atomically
downgraded to memory-only ownership and transferred to the returned plan.
Compute and I/O tokens become available to foreground work without any instant
in which the unpublished candidate's memory is unaccounted. The memory
reservation remains live until the plan is published or dropped.

Building alone appends no page or WAL record and does not advance the visible
CSN or any engine root.

## Publication

Writer admission is acquired only after the candidate is complete. Publication
keeps the plan's memory-only `Bulk` allocation and separately requests one CPU
and one I/O token as high-priority `Mutation` work with zero additional memory.
Queue selection may skip an older request that cannot fit while the plan owns
its memory, preventing a circular head-of-line wait. It must recheck, while
holding writer admission, all of the following:

1. the current root set is byte-for-byte the captured root set;
2. the current selected base and view identities are the captured identities;
3. the transaction's next commit CSN is the captured candidate CSN;
4. the index remains empty and has no delta or retained generation; and
5. restoring the complete partitioned snapshot reproduces its input and
   aggregate identities.

Any mismatch returns a stale-plan error before page or WAL publication. The
caller may plan again from the new committed authority; the implementation must
not silently rebase a completed graph.

The durable metadata format identifies the aggregate generation and every
ordered child generation. Child vectors and graph layers retain their existing
physical key formats and are keyed by the child build identity. Restore must
validate every child graph, geometric partition boundary, input identity,
aggregate identity, and child order. Routing summaries are derived and checked
on restore rather than trusted as persisted state.

The WAL mutation anchors the captured base/view identities, candidate input and
aggregate identities, candidate CSN, vector count, and partition count. The WAL
does not duplicate vector or graph payloads.

## Atomicity and lifecycle

All ordinary commit interruption boundaries have prior-or-complete semantics:
reopen observes either the previous empty generation or the complete
partitioned generation, never a subset or a mixed set of children.

After publication, online upserts and deletes remain bounded delta records over
the partitioned base. Consolidation may later replace it with a single
generation while preserving later deltas. Retention, validation, compaction,
vacuum, and backup must understand that one retained partitioned generation owns
multiple physical child identities.

## Local acceptance before bare metal

AWS is not a functional-debugging environment. Before a bare-metal run this
vertical must pass locally with small and medium deterministic corpora:

- serial, two-worker, and four-worker builds have identical snapshots and
  identities;
- at least one multi-partition build executes more than one worker batch;
- exact fanout equals the flat oracle and approximate results meet the existing
  deterministic contract;
- rejection, cancellation, and worker panic release all tokens; successful
  plans retain exactly their declared memory until publication or drop while
  foreground CPU/I/O admission remains possible;
- a concurrent ordinary commit makes the plan stale without damaging either
  root;
- publish, reopen, incremental update/delete, and consolidation preserve
  results and identities;
- malformed or reordered child metadata fails closed; and
- every commit interruption reopens to the prior or complete generation.

Only after these checks are green may AWS measure the frozen 1, 8, 32, and
maximum-worker curves, the million-vector 384-dimensional corpus, RSS, write
amplification, recovery time, interference, and accepted-corpus recall.
