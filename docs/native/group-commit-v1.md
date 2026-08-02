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
- the bounded submission-queue capacity.

All three bounds are validated before the worker starts. The first request
starts the collection interval. The worker drains requests until the cohort is
full or the interval expires, whichever occurs first. A singleton cohort is
valid and still receives one synchronization.

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
- strict reopen equivalence; and
- warm concurrency-one and contended latency/throughput receipts that separate
  queue time, execution time, page synchronization, WAL synchronization, and
  end-to-end acknowledgement.

This vertical advances G1 and G7. It does not close either gate, bounded WAL
replay, WAL retention, background expiry, or the complete scheduler/resource
policy. The remaining shared resource policy is narrowed by
[Native mixed-durability scheduler v1](mixed-durability-scheduler-v1.md).
