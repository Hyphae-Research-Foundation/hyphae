# Native hash field TTL v1

Status: contract frozen; implementation and evidence pending.

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md),
[Native whole-hash TTL v1](native-hash-ttl-v1.md),
[Native hash field commands v1](native-hash-field-commands-v1.md),
[Native active-expiry scheduler v1](active-expiry-scheduler-v1.md), and
[Native WAL format v1](wal-format-v1.md) with absolute expiry for one exact
field in one native hash. It does not add a timer service, sidecar,
compatibility protocol, or second writer.

## Surface

The embedded native surface adds:

```text
EXPIRE_HASH_FIELD(key, field, expires_at_micros) -> bool
TTL_HASH_FIELD(key, field) -> Missing | Persistent | RemainingMicros
```

`EXPIRE_HASH_FIELD` requires an existing hash visible at the transaction
logical time. Another live structure family fails with
`StructureKindMismatch`; a missing or whole-hash-expired family fails with
`UnknownStructureHash`. A missing or already field-expired exact field returns
false without a mutation. A live field returns true and records the exact
signed timestamp, including a timestamp already due at the transaction
logical time.

`TTL_HASH_FIELD` returns `Missing` when the hash, whole-hash incarnation, or
field is missing or due at the evaluating logical time. A live field without
an independent expiry returns `Persistent`. A live expiring field returns a
strictly positive `RemainingMicros`. A non-hash structure also returns
`Missing`, matching the existing TTL query surfaces rather than the mutating
kind-error surface.

Private batches and retained snapshots use their fixed logical time.
Current-root physical callers supply logical time explicitly. Equality with
either the whole-hash or field expiry is not visible.

Relative TTL, conditional expiry modes, sliding expiry, field `PERSIST`,
batch expiry, and collection-family TTL beyond hashes are outside this
version.

## Read and mutation semantics

Whole-hash expiry dominates every field. If the family is visible, `HGET` and
`HGET_MANY` treat a due field as missing. `HLEN` counts only fields visible at
the evaluating logical time. Ascending `HSCAN`, descending `HSCAN_REVERSE`,
and `HSCAN_MATCH` omit due fields without changing cursor ordering.

A due field remains a physical candidate until cleanup. Physical scans charge
it against visit work exactly like a tombstone or nonmatch, but never against
a returned-live-field limit. A cursor remains the exact field identity
regardless of that field's visibility.

`HSET`, `HSET_MANY`, and `HINCRBY` clear an existing field expiry. Updating a
due field treats it as logically missing for command results: `HSET` counts it
as added and `HINCRBY` starts from zero. Physical metadata cardinality need
not change because the expired envelope already occupies one non-tombstoned
field path.

`HDELETE`, `HDELETE_MANY`, and `EXPIRE_HASH_FIELD` treat a due field as
missing and add no mutation. Deleting a live expiring field tombstones both
its field path and exact expiry-index path. Deleting or expiring the complete
hash retains the existing family semantics and logically hides every field
first.

## WAL and concurrency

`EXPIRE_HASH_FIELD=32` is an additive structure-engine opcode. Its mutation
has:

- no target;
- the canonical compound hash-key/field identity;
- an empty value; and
- one explicit signed absolute expiry.

The logical mutation is the replay authority. It does not copy the field value
into the WAL. Physical application reads the admitted field envelope,
preserves its inline bytes or immutable blob reference, sets the exact expiry,
and updates the derived field-expiry index.

The existing `SET_HASH_FIELD` opcode remains the authority for writes and
counters. Physical application deterministically tombstones any prior field
expiry while storing a persistent replacement. The existing
`DELETE_HASH_FIELD` opcode tombstones any prior field-expiry entry. Active
field cleanup also uses `DELETE_HASH_FIELD`; it does not introduce a
physical-only deletion opcode.

Expiry, value writes, counters, deletion, and expiry-driven cleanup publish
the same field conflict identity. Same-field writers therefore use
first-committer-wins. Disjoint fields may still rebase independently.
Every field operation validates the whole-hash lifecycle identity observed by
its snapshot, so whole-hash delete, expiry, or recreation prevents stale field
publication.

The expiry operation preflights both the existing field path and the larger
expiry-index identity before changing private state. A field identity valid
for ordinary storage can therefore be rejected for field expiry when the
additional timestamp namespace would exceed the native B+tree key bound.

## Physical encoding

An expiring field uses the existing `HYSTRV01` value envelope with its expiry
flag and exact signed timestamp. Persistent fields and canonical tombstones
retain their current bytes. Hash metadata remains `HYHSHM01` or `HYHSHM02`;
its field count is the number of non-tombstoned physical field envelopes,
including logically due fields not yet cleaned.

The existing `0x0b` namespace cannot identify fields safely: its trailing
bytes are one arbitrary binary user key, so a compound field identity could
alias a scalar or whole-hash key. Field expiry therefore uses a distinct
ordered namespace:

```text
0x0c + sortable_signed_expiry + compound_hash_field_identity
```

The compound identity is `u32` big-endian hash-key length, hash-key bytes, and
field bytes. The sortable timestamp is the existing sign-bit-flipped
big-endian encoding. The value is exactly one byte:

| Marker | Meaning |
|---:|---|
| `0x00` | tombstone |
| `0x01` | live hash-field expiry |

Re-expiry tombstones the prior exact index path and writes the new one.
Persistent replacement, field deletion, whole-hash deletion, and whole-hash
expiry cleanup tombstone every affected live field-expiry path in the same
copy-on-write publication.

Recovery requires a one-to-one match between every expiry-bearing live field
envelope and its field-expiry index entry. A stale, duplicate, missing,
malformed, wrong-field, wrong-hash, orphan, or tombstoned-live mismatch fails
closed. Persistent fields must not have a live field-expiry entry. Existing
trees without `0x0c` entries remain valid.

## Active expiry

The single-writer active-expiry scheduler merges due work from the existing
scalar/whole-hash namespace and the field-expiry namespace. The canonical
order is:

```text
(expires_at_micros, namespace_order, exact_identity)
```

The existing scalar/whole-hash namespace has order `0`; the field namespace
has order `1`. `max_keys` bounds the combined number of due scalar keys,
whole hashes, and fields. Each source lookahead is bounded to `max_keys + 1`;
the scheduler does not materialize either complete index.

Field cleanup validates that the admitted field envelope carries the indexed
due timestamp, then tombstones the field and expiry entry and decrements hash
metadata cardinality in one publication. Removing the last field preserves an
empty typed hash and its whole-hash expiry, if any. One sweep may combine
`DELETE_VALUE`, `DELETE_HASH`, and `DELETE_HASH_FIELD` under one transaction
ID and global CSN. `more_due` reports remaining work in either namespace.

Logical visibility never depends on scheduler progress.

## Verification gates

The slice is not complete until executable evidence covers:

- a compiler-reaching model red gate before field-expiry methods exist;
- the complete signed timestamp domain and expiry-index identity bounds;
- private, retained-snapshot, current-root, and reopened
  `EXPIRE_HASH_FIELD`, `TTL_HASH_FIELD`, `HGET`, `HGET_MANY`, and `HLEN`
  equivalence before, at, and after field and whole-hash expiry;
- ascending, descending, and pattern scans across live, due, tombstoned,
  binary, and cursor fields, including physical visit accounting;
- `HSET`, `HSET_MANY`, `HINCRBY`, singular/batch delete, re-expiry, and
  immediate-due behavior;
- same-field conflicts, disjoint-field rebasing, and whole-hash lifecycle
  fencing for expiry and cleanup;
- WAL round-trip and semantic replay for `EXPIRE_HASH_FIELD=32`;
- one-to-one recovery validation for old persistent fields, expiring fields,
  field-expiry indexes, metadata cardinality, blobs, and every mismatch listed
  above;
- bounded mixed scalar/hash/field active expiry, canonical ordering,
  `more_due`, memory and strict durability, and empty-sweep accounting;
- every existing singleton commit interruption boundary for expiry,
  expiry-clearing replacement, explicit delete, and active cleanup;
- compaction removing retired field and expiry paths without changing
  reopened logical state; and
- direct-Linux release measurements separating field TTL reads, persistent and
  expiring `HGET`, no-due and due `HLEN`, memory/strict expiry commits, and
  active cleanup.

On the same host and corpus, persistent-field `HGET` p50 and p95 must not
regress by more than 10 percent against a fresh parent-commit control run.
Microsecond-first claims apply only to measured embedded read paths. Commit,
queueing, synchronization, cleanup, scans, and cold-I/O costs remain separate.

## Boundaries

This contract does not add relative or sliding expiry, expiry conditions,
field-expiry batches, field `PERSIST`, reverse-pattern scans, floating-point
counters, collection TTL, streams, a compatibility protocol, or a complete
G3/G7 claim.
