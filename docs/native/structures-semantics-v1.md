# Native structure-engine semantics v1

Status: normative target contract; binary scalar `SET`/`GET`, `DELETE`,
independent `EXPIRE`, `NX`/`XX`, signed `INCRBY`, snapshot-time TTL, native
hashes, multilevel B+tree persistence, direct buffered reads, and large
immutable blobs are implemented in the convergence slice; version-bearing
responses, the expiry scheduler, and the remaining structure families remain
pending

The structure engine is a first-class owner of keyspace data. It is not a
Valkey process, RESP dispatcher, relational projection, or disposable cache by
default.

## Object ownership

Each structure object is declared:

- `canonical`: durable source data, never silently evicted; or
- `cache`: evictable or reconstructible under an explicit policy.

The catalog declares key/value types, memory class, TTL policy, partition key,
durability default and relational access schema.

## Native structures

V1 target families are:

- binary/text strings and signed/unsigned counters;
- hashes/maps;
- chunked-deque lists;
- hash sets;
- sorted sets using membership hash plus ordered index;
- append-ordered streams with stable entry IDs;
- bitmaps;
- HyperLogLog-style cardinality estimates;
- geo points and radius indexes; and
- typed atomic registers.

Each family has a versioned physical format and typed operation set. A key
cannot change structure kind without delete/recreate or an explicit checked
conversion.

The implemented families are binary scalars, canonical signed-decimal
counters, and explicitly created binary hash/maps. Lists, sets, sorted sets,
streams, bitmaps, sketches, geo indexes, and typed registers remain targets.

## First vertical operations

```text
GET(key)
SET(key, value, condition, optional_ttl)
DELETE(key)
EXPIRE(key, expires_at)
TTL(key)
INCRBY(key, signed_delta)
CREATE_HASH(key)
HSET(key, field, value)
HGET(key, field)
HDELETE(key, field)
HLEN(key)
```

`SET` conditions are unconditional, if-absent, if-present, or
expected-version. The response includes existence, prior/new version CSN and
expiry.

The implemented slice exposes unconditional, if-absent (`NX`), and if-present
(`XX`) `SET`. A false predicate adds no mutation. Two detached `NX`
transactions may both prepare against the same missing snapshot key, but the
shared first-committer-wins table admits only one publication. Expected-version
conditions and version-bearing responses remain target behavior.

`DELETE` returns false for a missing or snapshot-expired key and otherwise
publishes a tombstone. `EXPIRE` returns false under the same absence rule and
otherwise rewrites the value with an absolute expiry.

`INCRBY` operates on canonical signed decimal bytes in the exact `i64` domain.
Missing or expired keys start at zero, an existing TTL is preserved, and the
result is stored as its canonical decimal representation. Empty input,
whitespace, a leading plus, redundant leading zeroes, `-0`, non-UTF-8 bytes,
and out-of-range input fail as non-integers. Arithmetic overflow is a separate
error. Either failure adds no mutation.

`CREATE_HASH` establishes the key's family before field mutation. `HSET`
returns added versus updated, `HGET` reads one field, `HDELETE` publishes a
field tombstone, and `HLEN` reads durable cardinality. An empty hash remains a
typed hash after its last field is deleted; whole-hash delete/recreate is not
implemented yet.

Scalar mutation of a hash key fails with a kind error. Concurrent scalar
creation and hash creation over the same absent key conflict. Once the hash
exists, different field identities can prepare and commit independently;
same-field writers retain first-committer-wins semantics.

## First physical namespace

New data directories store the structure partition in the native copy-on-write
B+tree:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYSTRBT1` |
| `0x01` | prefix + arbitrary binary user key | canonical `HYSTRV01` value |
| `0x02` | prefix + binary hash key | canonical `HYHSHM01` metadata |
| `0x03` | prefix + hash-field identity | canonical persistent `HYSTRV01` value |

The exact value envelope is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYSTRV01` |
| 8 | 1 | flags; bit 0 means expiry, bit 1 means tombstone |
| 9 | 1 | storage: `0` inline, `1` immutable blob reference |
| 10 | 6 | reserved zero |
| 16 | 8 | signed little-endian absolute expiry; zero when the flag is clear |
| 24 | variable | inline bytes or the exact 56-byte native blob reference |

Inline values are at most 8,192 bytes. Larger values must use a blob reference
whose declared logical length is above that threshold. The expiry flag
distinguishes persistent values from an explicit timestamp of zero or
`i64::MAX`; there is no sentinel collision.

The only canonical tombstone has flags exactly `0x02`, inline storage, zero
reserved and expiry bytes, and an empty payload. Any flag combination or
payload on a tombstone fails closed.

Every `SET`, `EXPIRE`, and `DELETE` upserts one key through a new copy-on-write
path. Retained roots preserve older values, TTLs, and pre-delete visibility.
Current direct `GET` and `TTL` traverse verified pinned pages without
materializing the complete structure state, then decode only the selected
envelope and blob. Recovery scans and validates the complete reachable
namespace while omitting tombstones from materialized state.

The exact hash metadata is 16 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYHSHM01` |
| 8 | 8 | unsigned little-endian live field count |

A hash-field identity is `u32` big-endian hash-key length, the hash-key bytes,
and the remaining field bytes. The physical key prepends `0x03`. This encoding
is unambiguous for empty or arbitrary binary keys and fields and keeps fields
clustered by hash-key length/key/field order.

Each field uses the same inline/blob `HYSTRV01` envelope but cannot carry an
independent expiry in this version. A field delete stores the same canonical
tombstone as a scalar delete. Recovery requires every field to have prior hash
metadata and requires metadata cardinality to equal the exact number of live
field envelopes. Orphan fields, malformed identities, expiry-bearing fields,
and count mismatches fail closed.

Earlier convergence directories used one `StructureNode` page containing the
`HYSTRT01` whole-state codec. Open detects that format from the root page kind
and continues reading and writing it without an implicit conversion. New
directories use `HYSTRBT1`.

## Atomicity

Operations execute in the common MVCC transaction. A transaction may combine
structure operations with relational and search mutations and either publishes
all under one CSN or none.

Multi-key operations declare their complete key set before commit where
possible. Dynamic access is bounded and participates in conflict detection.
Unlike best-effort command batches, a failed operation cannot leave earlier
private operations committed.

Hash field write keys use the canonical hash-field identity. Therefore
different fields rebase onto the admitted current root without losing the
metadata count, while same-field updates conflict. Physical `HSET`/`HDELETE`
rewrite the field path and the 16-byte metadata path; they never serialize the
complete hash as one value.

## TTL and expiry

Expiry is an absolute signed UTC microsecond timestamp captured in the value
version. Reads compare against snapshot logical time. Lazy expiry hides an
expired version from that snapshot; a bounded timing wheel schedules a
tombstone transaction.

TTL changes are versioned writes. At or after the exact expiry, both `GET` and
`TTL` report the key as missing. Historical proofs pin logical time. Restart
reconstruction of the pending timing wheel remains unimplemented.

## Eviction

Canonical objects reject memory pressure or spill to their declared page/blob
class; they are not evicted. Cache objects may choose no-eviction, LRU, LFU,
TTL-priority, random, or size policy. Every eviction is a committed tombstone
or an explicitly non-durable memory-class event recorded in telemetry.

## Blocking and streams

Blocking operations wait on version publication, support deadlines and
cancellation, and never occupy an engine owner thread. Stream consumer state
is ordinary versioned structure data.

## Relational access

Every structure exposes a typed relation-valued iterator defined by its
catalog schema. Snapshot, filter and limit pushdown are mandatory. SQL access
does not rewrite the structure as a table.

## Verification

Required evidence includes model-based randomized operations for every
structure, restart equivalence, multi-key atomicity, write conflicts,
controlled-clock TTL tests, timing-wheel rebuild, blocking cancellation,
memory-amplification receipts, eviction safety, and cross-engine transactions.

Current experimental tests cover a 2,048-key multilevel tree, historical roots,
direct TTL and expiry reads, canonical tombstones, `NX`/`XX`, racing `NX`
writers, signed counter bounds, strict reopen, canonical-envelope corruption,
legacy whole-page compatibility, optimistic disjoint-key rebase, all commit
crash boundaries with scalar and hash mutations, typed scalar/hash creation
races, disjoint-field rebase, same-field conflict, hash count corruption,
field tombstones, and one blob deduplicated across relational, scalar, and hash
field values. They do not close the structure gate.
