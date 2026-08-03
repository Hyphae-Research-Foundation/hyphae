# Native hash field commands v1

Status: accepted implementation target.

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md) and
[Native whole-hash TTL v1](native-hash-ttl-v1.md). It adds bounded
multi-field reads and mutations plus one signed field counter without adding
a WAL opcode, storage format, sidecar, or internal protocol hop.

## Surface

The embedded native surface adds:

```text
HGET_MANY(key, fields) -> [optional value]
HSET_MANY(key, [(field, value)]) -> added field count
HDELETE_MANY(key, fields) -> deleted field count
HINCRBY(key, field, delta_i64) -> new i64 value
```

The Rust methods are named `hget_many`, `hset_many`, `hdelete_many`, and
`hincrement_i64`. Snapshot and current-root physical surfaces expose
`hget_many`; mutations remain private-batch operations until commit.

All four operations require one existing visible native hash. Another live
structure family fails with `StructureKindMismatch`. A missing or logically
expired hash fails with `UnknownStructureHash`. None of these commands creates
a hash implicitly.

## Bounds and input order

One call accepts at most 4,096 field positions. The bound is evaluated before
any read result, private-state change, or mutation is produced. Empty input is
valid after hash kind and visibility validation: reads return an empty vector
and mutations return zero without adding WAL work.

`HGET_MANY` preserves caller position and exact duplicates. Every output
position corresponds to the field at the same input position; a missing field
is `None`.

Mutation batches reject duplicate exact field bytes before changing private
state. Accepted `HSET_MANY` and `HDELETE_MANY` inputs are sorted by exact field
bytes before mutations are prepared. Caller order therefore cannot alter WAL
mutation order, physical output, or conflict identities.

Every complete key/field identity is preflighted before the first mutation.
An oversized or otherwise invalid identity fails the complete call without
partially changing private state.

## Multi-field mutations

`HSET_MANY` stores every accepted binary value atomically in the surrounding
native transaction. Its result counts fields absent before this command;
replaced fields do not contribute to the count.

`HDELETE_MANY` deletes every requested field that exists before this command
and returns that count. Missing fields add no mutation. Deleting the last
field preserves the typed empty hash.

Both commands preserve the whole-hash absolute expiry exactly. They neither
renew nor clear it.

The runtime encodes each admitted upsert as the existing `SET_HASH_FIELD`
mutation and each admitted deletion as the existing `DELETE_HASH_FIELD`
mutation. No new opcode or physical format is introduced.

## Signed field counter

`HINCRBY` applies only to one field of an existing visible hash. A missing
field starts at zero. An existing value must be the canonical signed decimal
byte representation of one `i64`, using the same parser as scalar `INCRBY`.
Empty input, whitespace, a leading plus, redundant leading zeroes, `-0`,
non-UTF-8 bytes, and out-of-range values fail with
`StructureValueNotInteger`.

The addition uses checked `i64` arithmetic. Overflow fails with
`StructureIntegerOverflow`. Parse and overflow failures add no mutation and
do not change private state.

The result is written as canonical signed decimal bytes through one existing
`SET_HASH_FIELD` mutation and returned as `i64`. The hash expiry is preserved.

## Reads

Private-batch and retained-snapshot `HGET_MANY` read one materialized
incarnation and return owned values in caller order.

The current-root physical route captures one root set, validates live hash
metadata and logical time once, and then resolves each requested exact field
from that same B+tree root. It does not materialize or scan the complete hash.
A reached malformed metadata record, field envelope, expiry, or blob fails
the complete call rather than returning a partial vector.

## Concurrency and durability

Every field mutation publishes its existing field write identity and validates
the whole-hash lifecycle identity. Same-field writers conflict under
first-committer-wins. Disjoint-field batches may rebase independently within
one live hash incarnation. If any field in one multi-field transaction
conflicts, the complete native transaction fails; no subset publishes.

Whole-hash delete or expiry admitted after a prepared field command prevents
that command from resurrecting the retired incarnation. An admitted
multi-field transaction before later whole-hash retirement is retired with
the complete incarnation.

Commit, replay, interruption recovery, compaction, blob validation, and
retained snapshots use the existing field mutations. Recovery must reproduce
the exact added/deleted values and cardinality without a command-specific
side log.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before the methods exist;
- a reference-model transition covering duplicate, empty, bound, counter,
  and failure-atomicity rules;
- private, retained-snapshot, current-root physical, and reopened read
  equivalence;
- persistent and expiring hashes, including exact TTL preservation;
- same-field conflicts, disjoint-field rebasing, and lifecycle-fence races;
- every existing singleton commit interruption boundary for one mixed
  multi-field/counter transaction;
- fail-closed reached metadata, field, and blob corruption;
- a direct-Linux release observation separating batch call cost, per-field
  cost, commit publication, and strict durability; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

This contract does not add glob matching, reverse hash scans, per-field TTL,
relative or sliding expiry, floating-point counters, implicit hash creation,
streams, a compatibility protocol, or a complete G3/G7 claim.
