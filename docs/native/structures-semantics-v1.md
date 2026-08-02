# Native structure-engine semantics v1

Status: normative target contract; binary scalar `SET`/`GET`, `DELETE`,
independent `EXPIRE`, `NX`/`XX`, signed `INCRBY`, snapshot-time TTL, native
hashes, sets, chunked-deque lists, multilevel B+tree persistence, direct
buffered reads, and large immutable blobs are implemented in the convergence
slice; the dual-index sorted-set contract is admitted for implementation;
version-bearing responses, the expiry scheduler, and the remaining structure
families remain pending

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
counters, explicitly created binary hash/maps, exact binary sets, and
chunked-deque lists. Sorted sets, streams, bitmaps, sketches, geo indexes, and
typed registers remain targets.

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
CREATE_SET(key)
SADD(key, member)
SISMEMBER(key, member)
SREM(key, member)
SCARD(key)
CREATE_LIST(key)
LPUSH(key, value)
RPUSH(key, value)
LPOP(key)
RPOP(key)
LLEN(key)
LRANGE(key, start, stop)
CREATE_SORTED_SET(key)
ZADD(key, score, member)
ZSCORE(key, member)
ZREM(key, member)
ZCARD(key)
ZRANGE(key, start, stop)
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

`CREATE_SET` establishes a binary set before member mutation. `SADD` returns
true only when it adds a missing member, `SISMEMBER` reports exact membership,
`SREM` returns true only when it removes a live member, and `SCARD` reads the
durable exact cardinality. An empty set remains typed after its last member is
removed; whole-set deletion and per-member TTL are outside this slice.

Scalar, hash, and set kinds are mutually exclusive for one user key.
Concurrent creation of different kinds over the same absent key conflicts.
Once a set exists, different member identities can prepare and commit
independently; same-member writers retain first-committer-wins semantics.

`CREATE_LIST` establishes a binary chunked deque before element mutation.
`LPUSH` and `RPUSH` insert one exact binary value and return the new length.
`LPOP` and `RPOP` return and remove one end value, or return absence without a
mutation when the typed list is empty. `LLEN` returns the exact durable
length. `LRANGE` uses signed zero-based indexes and an inclusive stop:
negative indexes count back from the tail, bounds clamp to the list, and an
empty or inverted normalized interval returns an empty result.

An empty list remains typed. Scalar, hash, set, and list kinds are mutually
exclusive for one user key. Every list mutation conflicts on the complete
list identity in this version; concurrent end operations do not silently
commute. Whole-list deletion, list TTL, blocking pop, insertion by index,
trimming, moving between lists, and element mutation remain pending.

`CREATE_SORTED_SET` establishes a typed binary sorted set before member
mutation. `ZADD` adds a member or replaces its score and reports added,
updated, or unchanged. `ZSCORE` returns the exact canonical score for a member,
`ZREM` returns true only for a live member, and `ZCARD` returns exact durable
cardinality. `ZRANGE` uses the same signed, inclusive rank interval as
`LRANGE`; results are ascending by score with exact member bytes as the
deterministic tie-breaker.

Scores use finite or infinite IEEE 754 binary64 values except `NaN`, which is
rejected before mutation. Negative zero is normalized to positive zero. These
rules make one canonical score bit pattern per accepted numeric value. An
empty sorted set remains typed. Scalar, hash, set, list, and sorted-set kinds
are mutually exclusive for one user key. Different sorted-set members may
prepare and commit independently; changes to the same member retain
first-committer-wins semantics.

## First physical namespace

New data directories store the structure partition in the native copy-on-write
B+tree:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYSTRBT1` |
| `0x01` | prefix + arbitrary binary user key | canonical `HYSTRV01` value |
| `0x02` | prefix + binary hash key | canonical `HYHSHM01` metadata |
| `0x03` | prefix + hash-field identity | canonical persistent `HYSTRV01` value |
| `0x04` | prefix + binary set key | canonical `HYSETM01` metadata |
| `0x05` | prefix + set-member identity | canonical empty persistent `HYSTRV01` value |
| `0x06` | prefix + binary list key | canonical `HYLSTM01` metadata |
| `0x07` | prefix + list-key identity + ordered chunk ID | canonical `HYLSTC01` chunk or structure tombstone |
| `0x08` | prefix + binary sorted-set key | canonical `HYZSTM01` metadata |
| `0x09` | prefix + sorted-set-member identity | canonical `HYZSCR01` score or structure tombstone |
| `0x0a` | prefix + sorted-set key + sortable score + member | canonical empty persistent `HYSTRV01` value or structure tombstone |

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

The exact set metadata is 16 bytes: ASCII magic `HYSETM01` followed by the
unsigned little-endian live member count. A set-member identity uses the same
unambiguous compound form as a hash field: `u32` big-endian set-key length,
the set-key bytes, and the remaining member bytes. The physical key prepends
`0x05`.

A live member is represented only by the canonical persistent inline
`HYSTRV01` envelope with an empty payload. `SREM` stores the canonical
structure tombstone. Recovery requires every member to have prior set
metadata and requires metadata cardinality to equal the exact number of live
member envelopes. Orphan members, malformed identities, non-empty live
payloads, expiry-bearing members, and count mismatches fail closed.

List metadata is exactly 32 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYLSTM01` |
| 8 | 8 | unsigned little-endian live element count |
| 16 | 8 | signed little-endian head chunk ID |
| 24 | 8 | signed little-endian tail chunk ID |

An empty list requires count zero and both chunk IDs zero. A non-empty list
requires head less than or equal to tail and one live chunk at every ID in the
inclusive interval. Chunk IDs start at zero, decrease for new head chunks and
increase for new tail chunks. Exhausting either signed 64-bit direction fails
before mutation.

A list-chunk key is `0x07`, `u32` big-endian list-key length, list-key bytes,
then the signed chunk ID with its sign bit flipped and encoded big-endian.
This preserves list grouping and signed numeric chunk order.

A live chunk uses:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYLSTC01` |
| 8 | 2 | unsigned little-endian element count |
| 10 | 6 | reserved zero |
| 16 | variable | repeated `u32` little-endian envelope length plus envelope |

Each chunk contains 1 through 64 elements and is at most 10,000 encoded bytes.
Each element is one persistent, non-tombstone `HYSTRV01` envelope, so values
above the scalar inline threshold use the same immutable blob store. A push
rewrites an end chunk while both count and byte limits admit the value;
otherwise it creates one adjacent chunk. A pop rewrites a non-empty end chunk
or tombstones it and advances the corresponding metadata boundary. The last
pop restores canonical empty metadata. Empty live chunks, gaps, extra live
chunks, expiry-bearing elements, malformed envelopes, blob mismatches, length
mismatches, or metadata/count disagreement fail closed.

Sorted-set metadata is exactly 16 bytes: ASCII magic `HYZSTM01` followed by
the unsigned little-endian live member count. A membership identity uses the
same compound form as hashes and sets: `u32` big-endian sorted-set-key length,
the key bytes, and the remaining member bytes. The physical membership key
prepends `0x09`; its live value is exactly 16 bytes, ASCII magic `HYZSCR01`
followed by the canonical score bits in big-endian order. A deleted membership
stores the canonical structure tombstone.

The ordered key is `0x0a`, `u32` big-endian sorted-set-key length, the key
bytes, an order-preserving transformed binary64 score, and the member bytes.
Negative score encodings invert every bit; nonnegative encodings flip the sign
bit. Lexicographic key order is therefore ascending numeric score, then
ascending exact member bytes. Its live value is the canonical empty persistent
`HYSTRV01` envelope; score replacement tombstones the prior ordered key before
publishing the new one.

Recovery requires every membership index entry and every ordered index entry
to agree one-to-one on key, member, and score, and requires metadata
cardinality to equal both live index counts. Orphan entries, `NaN`, negative
zero, duplicate members, malformed identities, non-empty live markers,
expiry-bearing entries, stale live ordered scores, or count mismatches fail
closed.

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

Set member write keys use the canonical set-member identity. Therefore
different members rebase onto the admitted current root without losing the
metadata count, while same-member changes conflict. Physical `SADD`/`SREM`
rewrite the member path and the 16-byte metadata path; they never serialize
the complete set as one value.

List creation and every push/pop share one whole-list conflict identity.
Physical mutations rewrite metadata plus at most one end chunk. A pop mutation
carries the exact removed value so optimistic replay and physical publication
can reject a changed end rather than removing a different element.

Sorted-set member write keys use the canonical membership identity. Different
members rebase onto the admitted current root while same-member changes
conflict. Physical `ZADD` rewrites the member score, old/new ordered keys as
needed, and metadata only when cardinality changes. `ZREM` tombstones both
indexes and decrements metadata. The complete sorted set is never serialized
as one value.

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
field tombstones, typed scalar/hash/set creation races, disjoint-member
rebase, same-member conflict, set count corruption, member tombstones,
chunked-list restart/range/corruption/conflict behavior, and one blob
deduplicated across relational, scalar, hash field, and list values. They do
not close the structure gate.
