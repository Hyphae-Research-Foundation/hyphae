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
maximum number of detached `group` batches in one call. It validates and
retains every batch before inserting one indivisible FIFO command, returns one
`NativePendingCommit` handle per request in input order, and never mixes that
cohort with neighboring commands. Pre-insertion failure inserts nothing and
produces no completion evidence. Although the transport uses one command, the
submission gate reserves one queue-capacity unit per retained request. The
worker releases those units when it claims the command, so an explicit cohort
cannot multiply the configured request bound.

Shutdown stops accepting new work, drains requests ordered before the shutdown
marker, joins the worker, and closes the database handle. A worker-level
persistence or synchronization failure makes the scheduler unavailable; the
database must be reopened before more reads or writes.

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
value.

The timed throughput window begins before cohort preparation and ends only
after every pending completion in the window is durable. Per-request latency
remains scheduler enqueue through durable response. G7 evidence reconciles the
exact cohort-size and position histograms, contiguous CSNs, one page and WAL
synchronization per cohort, and all scheduler timing components. It then drops
the live database, reopens the directory once, verifies every logical key from
one recovered snapshot, and requires the expected and recovered state digests
to match. These rules do not weaken the advisory strict-fsync latency target or
by themselves close G7.

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
- clean shutdown, post-shutdown rejection, and worker-failure unavailability;
- explicit full and partial cohorts that remain isolated from neighboring FIFO
  commands and report one page and WAL synchronization;
- queue capacity charged by logical request, including an explicit cohort that
  fills the configured bound and prevents another admission until claim;
- shutdown that drains an already accepted cohort after its bounded resource
  wait becomes available;
- strict reopen equivalence; and
- warm concurrency-one and contended latency/throughput receipts that separate
  queue time, execution time, page synchronization, WAL synchronization, and
  end-to-end acknowledgement.

This vertical is retained by the closed G1 substrate gate. It contributes to
G7 but does not close the controlled performance matrix. The shared resource
policy is specified by
[Native mixed-durability scheduler v1](mixed-durability-scheduler-v1.md).
