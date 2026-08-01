# Native structure-engine semantics v1

Status: normative target contract; binary scalar `SET`/`GET`, snapshot-time
TTL, native multilevel B+tree persistence, direct buffered reads, and large
immutable blobs are implemented in the convergence slice; the complete
structure families remain pending

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

## First vertical operations

```text
GET(key)
SET(key, value, condition, optional_ttl)
DELETE(key)
EXPIRE(key, expires_at)
TTL(key)
```

`SET` conditions are unconditional, if-absent, if-present, or
expected-version. The response includes existence, prior/new version CSN and
expiry.

The implemented slice currently exposes unconditional `SET`, `GET`, and
`TTL`. `DELETE`, independent `EXPIRE`, conditions, and version-bearing
responses remain target behavior.

## First physical namespace

New data directories store the structure partition in the native copy-on-write
B+tree:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYSTRBT1` |
| `0x01` | prefix + arbitrary binary user key | canonical `HYSTRV01` value |

The exact value envelope is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYSTRV01` |
| 8 | 1 | flags; bit 0 means an expiry is present |
| 9 | 1 | storage: `0` inline, `1` immutable blob reference |
| 10 | 6 | reserved zero |
| 16 | 8 | signed little-endian absolute expiry; zero when the flag is clear |
| 24 | variable | inline bytes or the exact 56-byte native blob reference |

Inline values are at most 8,192 bytes. Larger values must use a blob reference
whose declared logical length is above that threshold. The expiry flag
distinguishes persistent values from an explicit timestamp of zero or
`i64::MAX`; there is no sentinel collision.

Every `SET` upserts one key through a new copy-on-write path. Retained roots
preserve older values and TTLs. Current direct `GET` and `TTL` traverse verified
pinned pages without materializing the complete structure state, then decode
only the selected envelope and blob. Recovery scans and validates the complete
reachable namespace.

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

## TTL and expiry

Expiry is an absolute signed UTC microsecond timestamp captured in the value
version. Reads compare against snapshot logical time. Lazy expiry hides an
expired version from that snapshot; a bounded timing wheel schedules a
tombstone transaction.

TTL changes are versioned writes. Restart reconstructs the timing wheel from
visible versions. Historical proofs pin logical time.

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
direct TTL and expiry reads, strict reopen, canonical-envelope corruption,
legacy whole-page compatibility, optimistic disjoint-key rebase, crash
boundaries, and one blob deduplicated across relational and structure values.
They do not close the structure gate.
