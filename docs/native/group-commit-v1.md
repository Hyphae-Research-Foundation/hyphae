# Native group commit v1

Status: normative target contract; bounded scheduler, independent admission,
private MVCC root chain, shared page/WAL flush, timing receipts and
five-boundary crash matrix are implemented experimentally

Group commit is a durability scheduler, not a transaction-composition feature.
It allows independent native transactions to share the physical synchronization
needed before acknowledgement while preserving one WAL authority and one CSN
order across all three engines.

## Scope

The first vertical accepts detached `NativeWriteBatch` values whose durability
class is `group`. It owns one `NativeDatabase` writer on a background thread and
exposes a bounded multi-producer submission queue. Preparation remains detached
from writer admission.

`strict` and `memory` transactions do not enter a group cohort. Existing direct
commit paths remain singleton operations in this first vertical. The unified
FIFO admission policy is specified separately by
[Native mixed-durability scheduler v1](mixed-durability-scheduler-v1.md).

## Bounded scheduler

The scheduler configuration names:

- the maximum number of requests in one cohort;
- the maximum collection interval after the first request arrives; and
- the bounded submission-queue capacity, measured in retained logical
  requests rather than transport commands.

All three bounds are validated before the worker starts. The first request
starts the collection interval. The worker drains requests until the cohort is
full or the interval expires, whichever occurs first. A singleton cohort is
valid and still receives one synchronization.

The additive explicit-cohort path accepts between one and the configured
maximum number of detached `group` batches in one call. It validates every
batch before inserting one indivisible FIFO command, returns one
`NativePendingCommit` handle per request in input order, and never mixes that
cohort with neighboring commands. Pre-insertion failure inserts nothing and
produces no completion evidence. Although the transport uses one command, the
submission gate reserves one queue-capacity unit per retained request. The
worker releases those units when it claims the command, so an explicit cohort
cannot multiply the configured request bound.

### Explicit-cohort preparation boundary

Concurrent preparation follows one linear ownership sequence:

1. begin and stage each detached batch while its mutation permit owns bounded
   compute, I/O, and memory;
2. consume each fully staged batch with `NativeCommitClient::retain_cohort_batch`;
3. collect the returned memory-only `NativeCommitBatch` values in canonical
   request order; and
4. pass the complete vector to `enqueue_cohort`, which performs the single
   atomic queue insertion.

`retain_cohort_batch` is a seal, not a submission. It checks that the scheduler
still accepts work; that the batch belongs to the same database and governor
authority; that its physical formats and mutation shape are valid and non-empty;
and that it requests `group` durability. Foreign, non-`group`, malformed, or
empty batches fail before queue insertion. The seal measures the exact retained
batch memory from the engine-owned delta ledger or existing materialized
allocation, reduces the mutation permit in place to
`{compute: 0, io: 0, memory: measured}`, and revalidates that exact memory-only
authority. It does not reacquire the governor, reserve queue capacity, stamp
submission time, or create completion evidence. Sealing an already sealed batch
is idempotent and must not expand its allocation.

The returned `NativeCommitBatch` is opaque to further staging. This prevents a
caller from adding mutations after releasing preparation resources. For
compatibility, `enqueue_cohort` applies the same seal and validation to an
unsealed input, but callers that prepare more batches than the mutation-class
compute or I/O limit must seal each batch immediately after staging. The final
enqueue revalidates every member against the scheduler authority before it
constructs pending handles or attempts the one indivisible FIFO insertion.

Ownership remains fail-closed across every pre-insertion exit:

- a seal or enqueue validation error consumes and drops the rejected batch;
- dropping a sealed but unqueued batch releases its retained memory and has no
  database effect;
- failure while assembling or atomically admitting a cohort drops every owned
  batch, reserves no partial queue capacity, performs no mutation, and produces
  no receipt; and
- shutdown rejects a new seal or enqueue as unavailable. A batch retained by a
  caller before shutdown remains caller-owned until it is dropped or its
  rejected enqueue consumes it.

After atomic insertion, dropping or explicitly cancelling a
`NativePendingCommit` requests cancellation only while that request remains
queued. A cancellation that wins consumes no transaction ID or CSN; the worker
skips that member and dropping its batch releases the retained memory. Other
live members remain one isolated cohort. Once execution claims a member,
cancellation or handle drop cannot roll back its database decision. Shutdown
stops new insertion and drains every live, uncancelled cohort ordered before its
FIFO marker; queued cancelled members are skipped.

Shutdown stops accepting new work and drains requests ordered before the
shutdown marker. The compatibility `shutdown` form joins the worker and closes
the database handle. The additive `shutdown_into_database` form joins the same
drained worker and returns its exact `NativeDatabase` owner for exclusive
post-queue maintenance. No client may submit after either shutdown marker. A
worker-level persistence or synchronization failure makes the scheduler
unavailable and prevents the database handle from being returned; the database
must be reopened before more reads or writes.

## Admission and ordering

Each request remains an independent transaction:

- it has its own transaction ID, mutation digest, WAL transaction, CSN, root
  set, receipt, and conflict result;
- accepted transactions receive contiguous CSNs in queue order;
- every accepted transaction rebases onto the prior accepted root in the same
  cohort;
- first-committer-wins validation includes earlier accepted writes in the
  cohort;
- one rejected request does not reject otherwise valid requests; and
- an empty or non-`group` request is rejected before physical mutation.

The cohort is not an atomic super-transaction. Before acknowledgement, a crash
may recover any valid committed prefix whose bytes reached stable storage.
After the cohort WAL synchronization completes, every accepted transaction in
that cohort must recover.

## Physical order

For one non-empty admitted cohort:

1. stage and promote any immutable blobs with their required durability;
2. append copy-on-write pages and complete WAL transactions in CSN order;
3. synchronize the page file exactly once;
4. synchronize the WAL exactly once;
5. publish the final root set and conflict table;
6. acknowledge every accepted request with its own receipt.

No acknowledgement occurs before step 4. Readers cannot observe roots from the
cohort while the writer holds admission. After publication, a new reader sees
the final cohort CSN and therefore every accepted transaction. Intermediate
root sets remain WAL-authoritative retained history even though no reader can
start between their simultaneous acknowledgements.

Large immutable blobs may require per-file and directory synchronization before
the shared page/WAL flush. Receipts and benchmarks must not describe that work
as a single filesystem synchronization.

## Receipt

Every `CommitReceipt` names:

- the selected `group` durability class;
- the number of accepted commits sharing the flush;
- its zero-based position in that cohort; and
- its existing transaction ID, CSN, catalog version, commit LSN, and WAL block
  digest.

Singleton `strict`, `memory`, and direct `group` commits report cohort size one
and position zero. A scheduler cohort containing rejected requests reports only
the accepted count in committed receipts.

`NativePendingCommit::wait` preserves the existing receipt API. The additive
`wait_with_evidence` form returns a `ScheduledCommitCompletion` containing the
same receipt plus explicit page- and WAL-synchronization counts. A successful
strict or `group` singleton reports one of each; a `memory` singleton reports
zero of each. Every successful member of one group cohort reports the same
cohort size, the complete zero-based position set, and exactly one shared page
synchronization and one shared WAL synchronization.
End-to-end time is sealed when the worker produces the completion; delaying
consumption of a pending handle cannot increase it.

## Strict G7 cohort evidence

The strict G7 surface uses the explicit-cohort path rather than timing-based
collection. Its authority is a bounded window of 32 outstanding logical
commits, submitted as full cohorts of 32 plus at most one final partial cohort.
Configured producer concurrency (`1`, `8`, or `32`) controls preparation; it
does not reduce the outstanding durable window. The runner measures the
maximum simultaneously active producers instead of copying the configured
value. Every producer seals each batch immediately after staging so one producer
can prepare all 32 requests without retaining 32 compute or I/O allocations.

The timed throughput window begins before cohort preparation and ends only
after every pending completion in the window is durable. Per-request latency
remains scheduler enqueue through durable response. G7 evidence reconciles the
exact cohort-size and position histograms, contiguous CSNs, one page and WAL
synchronization per cohort, and all scheduler timing components.

Strict evidence schema
`hyphae-native-g7-strict-group-commit-evidence-v2` keeps that hot interval
separate from bounded recovery maintenance. After all timed completions, the
runner drains the scheduler through `shutdown_into_database` and performs one
fail-closed sequence:

1. current-root page vacuum, which must apply and advance the CSN exactly once;
2. one checkpoint at that vacuum CSN, which does not advance the CSN;
3. WAL retention anchored to that exact checkpoint and CSN;
4. one drop and reopen from the retained anchor, with an empty retained WAL
   suffix; and
5. one full logical-key digest from the reopened snapshot.

The v2 receipt preserves the logical commit interval and its digest unchanged.
It records the maintenance CSN separately and requires it to equal the last
logical commit CSN plus one. The `maintenance` object records exact vacuum,
checkpoint, retention identities and one complete maintenance duration. The
`reopen` object records the retained base CSN, zero replayed transactions,
recovery component timings, open duration, full-key verification duration, and
equal expected/recovered logical-state digests. Maintenance, open, and
verification never enter hot latency histograms or throughput. The G7
controller nevertheless projects their complete measured pilot cost exactly
once per full cell; omitting, duplicating, or hiding that cost fails closed.
These rules do not weaken the advisory strict-fsync latency target or by
themselves close G7.

## Failure behavior

- Invalid scheduler bounds fail before a thread or file mutation exists.
- Queue or response disconnection returns a typed unavailable error.
- Semantic admission failures return only to the rejected request.
- Once physical cohort staging begins, any page, blob, WAL, synchronization, or
  publication failure fails every not-yet-acknowledged accepted request and
  stops the scheduler.
- Deterministic interruption points exist after the admitted WAL prefix, after
  all cohort appends, after page synchronization, after WAL synchronization,
  and after root publication.

Crash tests reopen the directory instead of reusing an interrupted handle.

## Verification

Required executable evidence covers:

- a singleton cohort;
- two or more concurrent disjoint requests sharing one page sync and one WAL
  sync;
- queue-order transaction IDs and contiguous CSNs;
- intra-cohort conflict rejection while an independent request commits;
- catalog-version advancement within one cohort;
- one request mutating relational, structure, and search roots under its one
  CSN;
- all deterministic interruption points with permitted prefix recovery before
  WAL synchronization and complete-cohort recovery afterwards;
- clean shutdown, post-shutdown rejection, worker-failure unavailability, and
  drain-then-return of the exact database owner without losing accepted work;
- explicit full and partial cohorts that remain isolated from neighboring FIFO
  commands and report one page and WAL synchronization;
- memory-only sealing under producer concurrency `1`, `8`, and `32`, including
  preparation of width 32 while the mutation class admits only two compute and
  I/O allocations;
- idempotent sealing plus rejection of foreign, non-`group`, empty, and
  malformed batches without mutation, evidence, or resource leaks;
- retained-batch drop, queued cancellation, pending-handle drop, admission
  failure, and shutdown paths that release every owned allocation;
- queue capacity charged by logical request, including an explicit cohort that
  fills the configured bound and prevents another admission until claim;
- shutdown that drains an already accepted cohort after its bounded resource
  wait becomes available;
- strict v2 vacuum/checkpoint/retention authority, bounded anchor reopen, and
  full logical-state equivalence, including failure paths that publish no
  terminal evidence; and
- warm concurrency-one and contended latency/throughput receipts that separate
  queue time, execution time, page synchronization, WAL synchronization, and
  end-to-end acknowledgement.

This vertical is retained by the closed G1 substrate gate. It contributes to
G7 but does not close the controlled performance matrix. The shared resource
policy is specified by
[Native mixed-durability scheduler v1](mixed-durability-scheduler-v1.md).
