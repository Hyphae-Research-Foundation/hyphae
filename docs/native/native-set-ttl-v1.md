# Native whole-set TTL v1

Status: contract frozen; implementation and evidence pending.

This contract adds one absolute expiry to a complete native binary set. It
uses Hyphae's existing WAL, MVCC, ordered B+tree, expiry scheduler, compaction,
and commit sequencing. It does not wrap Valkey, add a sidecar, introduce
per-member TTL, or route an internal operation through a compatibility
protocol.

## Public surface

The embedded native runtime adds:

```text
EXPIRE_SET(key, expires_at_micros) -> bool
TTL_SET(key) -> Missing | Persistent | RemainingMicros(value)
```

Private-batch and retained-snapshot reads use their pinned logical time.
Current-root physical reads add explicit-time variants:

```text
SISMEMBER_LATEST_SET_AT(key, member, logical_time_micros)
SCARD_LATEST_SET_AT(key, logical_time_micros)
TTL_LATEST_SET(key, logical_time_micros)
```

The existing no-time current-root membership and cardinality methods remain
compatibility views at `i64::MIN`. Expiry-aware callers must use the explicit
logical-time methods.

`EXPIRE_SET` requires `HYSTRBT2`. It returns `true` and replaces the complete
set expiry when the set is visible at the transaction's logical time. It
returns `false` without adding a mutation when the set is absent or already
due. Another live structure family returns `StructureKindMismatch`.

`TTL_SET` returns:

- `Missing` for an absent, due, tombstoned, or different-kind key;
- `Persistent` for a visible set with no expiry; and
- `RemainingMicros(expiry - logical_time)` for a visible expiring set.

## Time and visibility

Expiry is one signed absolute microsecond timestamp. A set is visible exactly
when it exists and its expiry is absent or strictly greater than logical time.
It is due at equality. No wall clock is read inside the set engine.

Whole-set expiry applies uniformly to:

- `SADD`, `SISMEMBER`, `SREM`, and `SCARD`;
- bounded union, intersection, and ordered difference;
- private-batch, retained-snapshot, current-root physical, and reopened state;
- optimistic admission and group commit; and
- active expiry cleanup.

For command surfaces that require an existing typed set, a due set behaves as
missing and returns `UnknownStructureSet`. In read-only set algebra, a due set
uses the existing missing-input rule and is the mathematical empty set.

Adding or removing a member preserves the whole-set expiry. Setting a new
whole-set expiry replaces the prior one. This slice does not add persistence
or conditional expiry commands.

## Lifecycle and reuse

A due set remains visible to historical snapshots pinned before its expiry.
At or after equality, a checked creation may retire the due incarnation and
reuse the user key as a scalar, hash, set, list, or sorted set. Reuse publishes
the prior set lifecycle retirement and the new family creation in one native
transaction. No member from the retired set may reappear in the new
incarnation.

`EXPIRE_SET` publishes the set lifecycle conflict identity. A member writer
prepared before an admitted expiry conflicts. A whole-set expiry prepared
before an admitted disjoint member write may rebase, preserve the member, and
expire the resulting set. Live disjoint member writers remain independently
committable.

This slice requires an internal complete-set retirement mutation for active
cleanup and due-key reuse. It does not expose a public manual `DELETE_SET`
command.

## WAL

Two additive structure opcodes are reserved:

```text
EXPIRE_SET = 33
DELETE_SET = 34
```

`EXPIRE_SET` has:

- engine `Structure`;
- no target;
- the complete set key as `key`;
- an empty value; and
- exactly one `expires_at_micros`.

`DELETE_SET` has:

- engine `Structure`;
- no target;
- the complete set key as `key`;
- an empty value; and
- no expiry.

Replay rejects missing, malformed, wrong-family, or duplicate lifecycle
transitions. Existing opcode bytes and golden encodings do not change.

## Physical layout

Persistent sets remain in the native namespaces:

| Prefix | Meaning |
|---:|---|
| `0x04` | set metadata |
| `0x05` | exact binary set members |
| `0x0b` | ordered top-level expiry index |

Persistent metadata retains the existing 16-byte `HYSETM01` encoding:

```text
magic[8] | member_count:u64_le
```

Expiring metadata uses a new 24-byte encoding:

```text
HYSETM02[8] | member_count:u64_le | expires_at_micros:i64_le
```

The ordered expiry identity remains:

```text
0x0b | sortable_i64_be(expiry) | exact_set_key
```

The value marker `3` is reserved for a live set-expiry entry. Marker `0`
remains a tombstone, `1` remains scalar expiry, and `2` remains whole-hash
expiry. Replacing expiry tombstones the prior matching marker and inserts the
new marker atomically with metadata.

Physical reads decode metadata before member access. A due metadata record is
logically missing even before cleanup. A reached live expiry marker must match
the metadata timestamp and set marker exactly; disagreement fails
`InvalidStructureTree`.

## Cleanup, compaction, and recovery

The shared ordered expiry sweep admits due scalar, hash, set, and hash-field
identities under one global `max_keys` bound and one CSN. A due set cleanup:

1. validates the current live set-expiry marker;
2. validates the live metadata member count against the reached member
   namespace;
3. tombstones every live member;
4. tombstones the metadata and expiry marker; and
5. publishes one internal `DELETE_SET` mutation.

An empty sweep writes no WAL record and advances no CSN. Cleanup interruption
at every native commit boundary must reopen to either the complete pre-cleanup
set or the complete retired set, never a partial member population.

Current-root compaction may omit canonical tombstones only after validating
metadata, members, and expiry markers. Page-generation vacuum must retain the
same logical and historical behavior.

## Required gates

The implementation gate requires:

- a compiler-reaching red test before the public methods exist;
- WAL byte reservation, round-trip, invalid-shape, semantic replay, and
  retained golden-codec coverage;
- exact visibility immediately before, at, and after expiry across private,
  retained, physical, and reopened state;
- membership, cardinality, and all three set-algebra operations under expiry;
- persistent member mutations and expiry replacement;
- due reuse across every structure family without retired-member
  resurrection;
- optimistic lifecycle/member conflict and rebase tests;
- bounded mixed scalar/hash/set/hash-field cleanup ordering;
- every cleanup commit boundary under strict durability;
- reached metadata, member-count, member-envelope, and expiry-marker
  corruption failures;
- compaction and page-vacuum preservation; and
- direct-Linux latency that separates logical reads, current-root reads,
  expiry mutation, cleanup work, and physical durability.

## Boundaries

Passing this slice does not add public manual set deletion, relative expiry,
conditional expiry, persist, per-member TTL, destination-set algebra, sorted
set TTL/algebra, streams, network compatibility, complete G3, or G7.

Microsecond-first is a measured hot-path objective. Cleanup of a large set is
cardinality-sensitive and strict durability includes physical synchronization;
neither receives a universal microsecond bound.
