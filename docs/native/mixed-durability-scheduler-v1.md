<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native mixed-durability scheduler v1

Status: normative target contract; mixed execution, FIFO barriers, bounded
admission, exact queued cancellation, timing receipts, and bounded-load
evidence are implemented experimentally; sustained fairness, maintenance
scheduling, and failure/soak evidence remain pending

The native commit scheduler is the single writer-admission authority for
detached transactions. It orders `strict`, `group`, and `memory` durability
through one bounded queue while preserving the existing transaction, WAL, CSN,
and recovery contracts.

This contract extends [Native group commit v1](group-commit-v1.md). It does not
change the durable formats or allow one durability class to weaken another.

## Scope

One scheduler owns one `NativeDatabase` on one named worker thread. Cloneable
clients may prepare detached `NativeWriteBatch` values concurrently and submit
all three durability classes through the same scheduler.

The scheduler provides:

- one bounded FIFO admission queue;
- non-blocking bounded-admission failure;
- queued deadlines and explicit queued cancellation;
- consecutive `group` cohort formation;
- singleton `strict` and `memory` execution;
- typed shutdown and unavailable outcomes; and
- per-request admission, queue, execution, synchronization, and end-to-end
  timing.

Background expiry, compaction, checkpoints, and vacuum remain outside this
vertical until they submit typed maintenance commands through the same resource
policy.

## Ordering and cohort barriers

The queue defines decision order. The worker may inspect at most one command
beyond the current cohort and must retain that command as the next FIFO item.

- A `strict` request is a singleton durability barrier.
- A `memory` request is a singleton publication barrier.
- A `group` request starts a cohort containing only immediately consecutive
  live `group` requests, bounded by the configured cohort size and collection
  interval.
- An explicit group cohort is one indivisible FIFO barrier. Its requests are
  prevalidated and inserted together, never mixed with adjacent commands, and
  execute in their input order.
- A `strict`, `memory`, shutdown, cancelled, or expired command ends collection.
- A later `group` request must never jump over another durability class.

Successful transactions receive monotonically increasing transaction IDs and
contiguous CSNs in execution order. Semantic rejection consumes neither. A
rejected or cancelled request does not prevent later independent work.

## Bounded admission

The public client supports:

- blocking admission followed by a definite outcome;
- immediate admission that returns `saturated` when the queue is full; and
- controlled admission with an optional absolute queue deadline and explicit
  cancellation handle.

The admission-state lock protects `accepting`, sender acquisition, and logical
request-capacity accounting. No blocking queue send or response wait may hold
it. A full queue therefore cannot prevent another thread from stopping
admission.

Queue capacity counts retained logical commit requests, not transport
commands. A singleton reserves one unit. An explicit cohort atomically reserves
its complete request count before insertion and releases that count when the
worker claims the command; partial reservation is forbidden. Shutdown and
other internal control markers consume no logical-request capacity. A client
that observes `saturated`, `deadline exceeded`, `cancelled`, or `unavailable`
receives no commit acknowledgement.

## Exact cancellation boundary

Every controlled request has an atomic state:

1. `queued`;
2. `executing`;
3. `cancelled`; or
4. `completed`.

The worker must atomically change `queued` to `executing` before physical
mutation. A cancellation or queue deadline succeeds only by atomically changing
`queued` to `cancelled`.

If cancellation wins, the batch must not append a page, blob, or WAL record and
must not consume a transaction ID or CSN. If execution wins, cancellation
returns `too late` and the waiter receives the definite commit or failure
outcome even when its queue deadline has elapsed. There is no claim that a
deadline bounds filesystem synchronization once persistence begins.

Dropping a `NativePendingCommit` requests the same cancellation transition while
its request is still `queued`. If that transition wins, the request is skipped
without mutation. Once a request is `executing`, handle drop is too late: its
database decision is independent of response delivery and must not be rolled
back.

## Durability execution

The worker executes one FIFO item or group cohort while holding exclusive
database access:

- `strict` calls the existing singleton commit path and acknowledges only after
  required blob, page, and WAL synchronization;
- `group` calls the existing shared-flush group path and reports the accepted
  cohort size and position;
- `memory` calls the existing singleton commit path without page or WAL
  synchronization and makes no crash-survival promise.

The scheduler does not merge strict or memory requests into a group cohort.
Queued memory work also cannot pass an earlier strict barrier.

Any physical persistence, synchronization, or publication failure stops
admission and fails all remaining queued requests as unavailable. Semantic
admission failures are request-local.

## Shutdown

Shutdown atomically stops new admission, queues one FIFO marker, drains every
live request ordered before that marker, joins the worker, and closes the
database handle.

Cancelled or expired requests before the marker are skipped. Work after the
marker cannot exist. Shutdown must remain able to stop admission while producers
are blocked or the queue is saturated.

## Receipts

`ScheduledCommitReceipt` retains the independent `CommitReceipt` and reports:

- admission wait before queue insertion;
- queue wait from insertion to execution claim;
- database execution time;
- page synchronization time;
- WAL synchronization time; and
- caller-observed end-to-end time.

Singleton strict and memory receipts report cohort size one and position zero.
Timing fields are observational and never replace durability evidence.

## Verification

Executable evidence must cover:

- FIFO `group → strict → group` and `memory → strict` barriers;
- a consecutive group cohort sharing one page and WAL synchronization;
- strict and memory singleton receipts through the scheduler;
- immediate saturation without mutation;
- atomic weighted admission in which one explicit cohort can fill the logical
  request bound even though it occupies one transport command;
- queue-deadline and explicit-cancellation wins without consuming IDs or CSNs;
- cancellation losing to execution and returning a definite outcome;
- abandoned waiters without database rollback;
- shutdown while the queue is saturated and while accepted work is waiting for
  bounded resource admission, with all work before the marker drained;
- worker-failure unavailability;
- no starvation under sustained group submissions; and
- warm Windows and WSL2 latency/throughput under mixed and group-only load.

Evidence must distinguish queue admission, queue wait, execution, page sync,
WAL sync, and response time. WSL2 filesystem provenance must be recorded.
