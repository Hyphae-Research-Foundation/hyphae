# Native whole-list TTL v1

Status: implemented; direct-Linux evidence recorded in
[Native whole-list TTL evidence — 2026-08-03](../gates/evidence/native-list-ttl-linux-2026-08-03.md).

This contract adds one absolute expiry to a complete native binary list. It
uses Hyphae's existing WAL, MVCC, ordered B+tree, chunked deque storage,
expiry scheduler, compaction, blob reachability, and commit sequencing. It
does not wrap Valkey, add a sidecar, introduce per-element TTL, or route an
internal operation through a compatibility protocol.

## Public surface

The embedded native runtime adds:

```text
EXPIRE_LIST(key, expires_at_micros) -> bool
TTL_LIST(key) -> Missing | Persistent | RemainingMicros(value)
```

Private-batch and retained-snapshot reads use their pinned logical time.
Current-root physical reads add explicit-time variants:

```text
LLEN_LATEST_LIST_AT(key, logical_time_micros)
LRANGE_LATEST_LIST_AT(key, start, stop, logical_time_micros)
TTL_LATEST_LIST(key, logical_time_micros)
```

The existing no-time current-root length and range methods remain
compatibility views at `i64::MIN`. Expiry-aware callers must use the explicit
logical-time methods.

`EXPIRE_LIST` requires `HYSTRBT2`. It returns `true` and replaces the complete
list expiry when the list is visible at the transaction's logical time. It
returns `false` without adding a mutation when the list is absent or already
due. Another live structure family returns `StructureKindMismatch`.

`TTL_LIST` returns:

- `Missing` for an absent, due, tombstoned, or different-kind key;
- `Persistent` for a visible list with no expiry; and
- `RemainingMicros(expiry - logical_time)` for a visible expiring list.

## Time and visibility

Expiry is one signed absolute microsecond timestamp. A list is visible exactly
when it exists and its expiry is absent or strictly greater than logical time.
It is due at equality. No wall clock is read inside the list engine.

Whole-list expiry applies uniformly to:

- `LPUSH`, `RPUSH`, `LPOP`, `RPOP`, `LLEN`, and `LRANGE`;
- private-batch, retained-snapshot, current-root physical, and reopened state;
- optimistic admission and group commit; and
- active expiry cleanup.

A due list behaves as missing and list commands that require an existing typed
list return `UnknownStructureList`. Pushing or popping preserves the
whole-list expiry. Setting a new whole-list expiry replaces the prior one.
This slice does not add persistence or conditional expiry commands.

## Lifecycle and reuse

A due list remains visible to historical snapshots pinned before its expiry.
At or after equality, a checked creation may retire the due incarnation and
reuse the user key as a scalar, hash, set, list, or sorted set. Reuse publishes
the complete prior list retirement and the new family creation in one native
transaction. No retired chunk or inline or blob-backed element may attach to
the new incarnation.

`EXPIRE_LIST`, every list mutation, and `DELETE_LIST` publish the existing
whole-list lifecycle conflict identity. A writer prepared before an admitted
expiry conflicts. An expiry prepared before an admitted push or pop conflicts.
This version intentionally does not admit independent head and tail writers or
expiry rebase.

The existing public `DELETE_LIST` mutation is also the internal complete-list
retirement authority for active cleanup and due-key reuse.

## WAL

One additive structure opcode is reserved:

```text
EXPIRE_LIST = 36
```

`EXPIRE_LIST` has:

- engine `Structure`;
- no target;
- the complete list key as `key`;
- an empty value; and
- exactly one `expires_at_micros`.

Existing `DELETE_LIST=35` remains unchanged and is reused for cleanup and due
reuse. Replay rejects missing, malformed, wrong-family, or duplicate lifecycle
transitions. Existing opcode bytes and golden encodings do not change.

## Physical layout

Persistent lists remain in the native namespaces:

| Prefix | Meaning |
|---:|---|
| `0x06` | list metadata |
| `0x07` | ordered list chunks |
| `0x0b` | ordered top-level expiry index |

Persistent metadata retains the existing 32-byte `HYLSTM01` encoding:

```text
magic[8] | length:u64_le | head_chunk:i64_le | tail_chunk:i64_le
```

Expiring metadata uses a new 40-byte encoding:

```text
HYLSTM02[8] | length:u64_le | head_chunk:i64_le | tail_chunk:i64_le
| expires_at_micros:i64_le
```

The ordered expiry identity remains:

```text
0x0b | sortable_i64_be(expiry) | exact_list_key
```

The value marker `4` is reserved for a live list-expiry entry. Marker `0`
remains a tombstone, `1` remains scalar expiry, `2` remains whole-hash expiry,
and `3` remains whole-set expiry. Replacing expiry tombstones the prior
matching marker and inserts the new marker atomically with metadata.

Physical reads decode metadata before chunk access. A due metadata record is
logically missing even before cleanup. A reached live expiry marker must match
the metadata timestamp and list marker exactly; disagreement fails
`InvalidStructureTree`.

## Cleanup, compaction, and recovery

The shared ordered expiry sweep admits due scalar, hash, set, list, and
hash-field identities under one global `max_keys` bound and one CSN. A due
list cleanup:

1. validates the current live list-expiry marker;
2. validates the live metadata and exact contiguous chunk coverage;
3. validates the total element count and every reached element envelope;
4. tombstones every live chunk;
5. tombstones the metadata and expiry marker; and
6. publishes one `DELETE_LIST` mutation.

Immutable blob objects remain governed by existing reachability collection.
An empty sweep writes no WAL record and advances no CSN. Cleanup interruption
at every native commit boundary must reopen to either the complete pre-cleanup
list or the complete retired list, never a partial chunk population.

Current-root compaction may omit canonical tombstones only after validating
metadata, chunks, element envelopes, and expiry markers. Page-generation
vacuum and blob collection must retain the same logical and historical
behavior.

## Required gates

The implementation gate requires:

- a compiler-reaching red test before the public methods exist;
- WAL byte reservation, round-trip, invalid-shape, semantic replay, and
  retained golden-codec coverage;
- exact visibility immediately before, at, and after expiry across private,
  retained, physical, and reopened state;
- all list commands under expiry, including empty and multichunk lists;
- expiry preservation through pushes and pops and expiry replacement;
- due reuse across every structure family without retired-element
  resurrection;
- optimistic lifecycle/list-writer conflicts and Group durability;
- bounded mixed scalar/hash/set/list/hash-field cleanup ordering;
- every cleanup commit boundary under strict durability;
- reached metadata, chunk identity, chunk gap, count, element envelope, blob
  reference, expiry marker, and page corruption failures;
- compaction, page vacuum, checkpoint/WAL retention, blob collection, and
  reopen preservation; and
- direct-Linux latency that separates logical reads, current-root reads,
  expiry mutation, cardinality-sensitive cleanup work, and physical
  durability.

## Boundaries

Passing this slice does not add generic cross-family `DEL`, relative expiry,
conditional expiry, persist, per-element TTL, blocking operations, insertion
by index, trimming, moving, element mutation, batched push/pop, streams,
network compatibility, complete G3, or G7.

Microsecond-first is a measured hot-path objective. Cleanup of a large list is
chunk- and payload-sensitive and strict durability includes physical
synchronization; neither receives a universal microsecond bound.
