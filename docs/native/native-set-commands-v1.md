# Native set member commands v1

Status: implemented; [direct-Linux evidence captured on
2026-08-03](../gates/evidence/native-set-commands-linux-2026-08-03.md).

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md) and
[Native whole-set TTL v1](native-set-ttl-v1.md). It adds bounded multi-member
reads and mutations plus ascending member iteration without a new WAL opcode,
storage format, sidecar, or internal protocol hop.

## Surface

The embedded native runtime adds:

```text
SADD_MANY(key, members) -> added member count
SREM_MANY(key, members) -> deleted member count
SMISMEMBER(key, members) -> [bool]
SSCAN(key, start_after?, limit) -> [member]
```

The Rust methods are named `sadd_many`, `srem_many`, `smismember`, and
`sscan`. Private write batches expose all four operations. Retained snapshots
expose `smismember` and `sscan`. Current-root physical reads expose
`smismember_latest_set_at` and `sscan_latest_set_at` with explicit logical
time, plus compatibility variants evaluated at `i64::MIN`.

All operations require one existing visible native set. Another live
structure family fails with `StructureKindMismatch`. A missing or logically
expired set fails with `UnknownStructureSet`. None of these commands creates
a set implicitly.

## Bounds and input order

One multi-member call accepts at most 4,096 caller positions. The bound is
evaluated before any read result, private-state change, or mutation is
produced. Empty input is valid after set kind and visibility validation:
`SMISMEMBER` returns an empty vector and mutations return zero without adding
WAL work.

`SMISMEMBER` preserves caller positions and exact duplicates. Every output
position corresponds to the member at the same input position.

Mutation batches reject duplicate exact members before changing private
state. Accepted `SADD_MANY` and `SREM_MANY` inputs are sorted by exact member
bytes before mutations are prepared. Caller order therefore cannot alter WAL
mutation order, physical output, or conflict identities.

Every complete key/member identity is preflighted before the first mutation.
An oversized or otherwise invalid identity fails the complete call without
partially changing private state.

## Multi-member mutations

`SADD_MANY` stores every accepted binary member atomically in the surrounding
native transaction. Its result counts members absent before this command.
Members already present add no mutation and do not contribute to the count.

`SREM_MANY` removes every requested live member and returns that count.
Missing members add no mutation. Removing the final member preserves the
typed empty set.

Both commands preserve the whole-set absolute expiry exactly. They neither
renew nor clear it.

The runtime encodes each admitted insertion as the existing `ADD_SET_MEMBER`
mutation and each admitted removal as the existing `DELETE_SET_MEMBER`
mutation. No new opcode or physical format is introduced.

## Membership reads

Private-batch and retained-snapshot `SMISMEMBER` read one materialized set
incarnation and return booleans in caller order.

The current-root physical route captures one root set, validates live set
metadata and logical time once, and then resolves each requested exact member
from that same B+tree root. It does not materialize or scan the complete set.
A reached malformed metadata, member identity, member envelope, or page fails
the complete call rather than returning a partial vector.

## Bounded ascending scan

`SSCAN` returns at most `limit` live members in ascending exact-byte order.
`start_after` is an optional exclusive exact-member cursor, not an opaque
server token: the caller resumes with the final returned member. `None` starts
at the first member, while `Some(empty)` resumes after a real empty member. A
cursor need not remain live; its bytes still define the exclusive lower bound.
A zero limit validates identity, set kind, and visibility before returning an
empty vector.

Private-batch and retained-snapshot scans use the ordered native set model.
The current-root physical route maps `start_after` directly into the set-member
B+tree namespace, skips tombstones without charging the output limit, and
stops after the requested number of live members. It must not materialize the
complete set.

When a physical call visits the complete member prefix, it validates that the
live member count equals set metadata. A bounded prefix that stops at its
output limit does not claim complete cardinality validation. A malformed
reached identity, member envelope, metadata record, or page fails the complete
call rather than returning a partial vector.

## Concurrency and durability

Every member mutation publishes its existing member write identity and
validates the whole-set lifecycle identity. Same-member writers conflict under
first-committer-wins. Disjoint-member batches may rebase independently within
one live set incarnation. If any member in one multi-member transaction
conflicts, the complete native transaction fails; no subset publishes.

Whole-set expiry admitted after a prepared member command prevents that
command from resurrecting the retired incarnation. An admitted multi-member
transaction before later whole-set expiry remains part of the set that
eventually becomes due.

Commit, replay, interruption recovery, compaction, and retained snapshots use
the existing member mutations. Recovery must reproduce exact membership and
cardinality without a command-specific side log.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before the methods exist;
- an independent reference-model transition covering duplicates, empty
  inputs, hard bounds, ordering, cursors, counts, and failure atomicity;
- private, retained-snapshot, current-root physical, and reopened read
  equivalence;
- persistent and expiring sets, including exact TTL preservation;
- same-member conflicts, disjoint-member rebasing, and lifecycle-fence races;
- every existing singleton commit interruption boundary for one mixed
  multi-member transaction;
- complete-prefix cardinality validation and fail-closed reached metadata,
  member-identity, member-envelope, and page corruption;
- multilevel member-tree evidence that middle-page scans prune earlier pages;
- direct-Linux release observations separating batch call cost, per-member
  cost, physical membership reads, head/middle/tail scans, commit publication,
  and strict durability; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

This contract does not add unbounded `SMEMBERS`, pattern matching, reverse
iteration, random-member selection, member moves, destination-set algebra,
per-member TTL, public manual whole-set deletion, a compatibility protocol,
or a complete G3/G7 claim.

Microsecond-first is a measured hot-path objective. Batch mutations, large
members, cold page reads, and strict durability remain workload-sensitive and
receive no universal latency promise.
