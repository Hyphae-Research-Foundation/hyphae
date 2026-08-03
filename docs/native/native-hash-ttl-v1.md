# Native whole-hash TTL v1

Status: implemented; evidence recorded in
[Native whole-hash TTL evidence on Linux — 2026-08-03](../gates/evidence/native-hash-ttl-linux-2026-08-03.md).

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md),
[Native active-expiry scheduler v1](active-expiry-scheduler-v1.md), and
[Native WAL format v1](wal-format-v1.md) with absolute expiry for one complete
native hash family. It does not add a timer service, sidecar, compatibility
protocol, or second writer.

## Scope

The new operations are:

```text
EXPIRE_HASH(key, expires_at_micros)
TTL_HASH(key)
```

`EXPIRE_HASH` applies one signed absolute-microsecond expiry to the complete
hash incarnation. It returns false without a mutation when the hash is missing
or already expired at the transaction logical time. It fails with a kind error
when another live structure family owns the key. A live hash, including an
empty hash, returns true and records the exact supplied timestamp.

`TTL_HASH` returns `Missing`, `Persistent`, or a strictly positive
`RemainingMicros`. Equality with the supplied logical time is expired and
therefore `Missing`. A retained snapshot always uses its own logical time.
Current-root physical callers supply logical time explicitly.

`HGET`, `HLEN`, and `HSCAN` treat a due hash as missing even before physical
cleanup. `HSET`, `HDELETE`, `DELETE_HASH`, and another `EXPIRE_HASH` use the
same absence rule. Expiry never deletes one field independently.

Relative TTL, field TTL, sliding expiry, `PERSIST_HASH`, and TTL for the other
collection families are outside this version.

## Incarnation and kind semantics

Logical expiry is an automatic whole-hash lifecycle boundary. A due hash and
all of its fields are absent at the evaluating logical time, while retained
snapshots before the boundary keep their earlier view.

A transaction may reuse the key after logical expiry as a scalar or any
explicitly created collection family. The transaction first records bounded
logical `DELETE_HASH` cleanup and then the new-family mutation. Physical
publication retires the old metadata, every old field path, and its expiry
index entry before publishing the replacement. No field from an expired
incarnation may become visible through recreation.

Every whole-hash expiry change publishes the existing scalar/collection
ownership identity. Hash-field mutations continue to publish only their field
identity but validate the ownership identity observed by their snapshot. Thus
disjoint field writers still commute within one stable incarnation, while a
field writer prepared before an admitted expiry change or expiry-driven reuse
must conflict rather than update a retired incarnation.

## WAL

`EXPIRE_HASH=31` is an additive structure-engine opcode. Its mutation has:

- no target;
- the exact binary hash key;
- an empty value; and
- an explicit signed expiry, including valid `i64::MIN`, zero, or `i64::MAX`.

The logical mutation is the replay authority. Field values are not copied into
the WAL body. Recovery applies the expiry to the admitted hash metadata and
derives the ordered expiry-index update through the normal structure mutation
path.

Expiry-driven cleanup uses the existing bounded `DELETE_HASH=30` mutation. It
does not introduce a physical-only deletion opcode.

## Physical encoding

Persistent hash metadata remains the exact 16-byte `HYHSHM01` encoding:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYHSHM01` |
| 8 | 8 | unsigned little-endian live field count |

Expiring hash metadata is the exact 24-byte `HYHSHM02` encoding:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYHSHM02` |
| 8 | 8 | unsigned little-endian live field count |
| 16 | 8 | signed little-endian absolute expiry |

The distinct magic is the expiry-presence bit. No timestamp is reserved as a
sentinel. Clearing the final field preserves the selected metadata version and
expiry. Creating or recreating a hash writes persistent `HYHSHM01`.

The existing `0x0b` ordered expiry namespace remains:

```text
0x0b + sortable_signed_expiry + exact_binary_user_key
```

Its value is exactly one byte:

| Marker | Meaning |
|---:|---|
| `0x00` | tombstone |
| `0x01` | live scalar expiry |
| `0x02` | live whole-hash expiry |

One logical key may have at most one live expiry entry across all structure
families. Re-expiry tombstones the prior entry and writes the new hash marker
atomically with metadata. Whole-hash deletion tombstones its live expiry
entry. Recovery requires a one-to-one match between live scalar/hash metadata
expiry and the typed expiry marker; a wrong kind, stale live entry, duplicate,
missing entry, malformed metadata, or orphan field fails closed.

## Active expiry

The existing single-writer scheduler visits scalar and hash markers in one
expiry/time/key order. `max_keys` bounds the combined number of due logical
keys, not each kind separately. A non-empty sweep may contain both
`DELETE_VALUE` and `DELETE_HASH` and still consumes one transaction ID and one
global CSN. `more_due` reports any additional due live marker of either kind.

Due-hash cleanup validates that the admitted metadata has the indexed expiry,
then tombstones the metadata, every retained field path, and the expiry entry
through one copy-on-write sorted batch. An empty sweep remains a no-op.
Scheduler failure, shutdown, fairness, and durability rules are unchanged.

## Verification gates

The slice is not complete until executable evidence covers:

- private, retained-snapshot, current-root physical, and reopened TTL/read
  parity at before/equal/after logical times;
- empty and populated hashes, repeated expiry, explicit delete, and same-batch
  delete/recreate or expired-key type reuse;
- WAL round-trip and semantic replay for the full signed timestamp domain;
- first-committer-wins conflicts between expiry, field mutation, deletion, and
  recreation;
- one-to-one recovery validation for both metadata versions and typed expiry
  markers, including malformed and cross-kind cases;
- bounded mixed scalar/hash active expiry, `more_due`, memory and strict
  durability, empty-sweep accounting, and scheduler observability;
- crash recovery at every existing commit interruption boundary;
- compaction removing retired metadata, fields, and expiry entries without
  changing reopened logical state; and
- Linux ext4/EBS measurements separating private/snapshot TTL, current-root
  physical TTL/read, memory commit, strict commit, and active cleanup.

On the same host and corpus, persistent-hash `HGET` p50 and p95 must not
regress by more than 10 percent against a fresh parent-commit control run.
Microsecond-first claims apply only to measured embedded read paths. Commit,
queueing, synchronization, cleanup, and cold-I/O costs remain separate.
