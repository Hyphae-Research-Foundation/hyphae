# Native structure-engine semantics v1

Status: normative target contract; binary scalar `SET`/`GET`, `DELETE`,
independent `EXPIRE`, `NX`/`XX`, signed `INCRBY`, snapshot-time TTL, native
hashes, sets, chunked-deque lists, multilevel B+tree persistence, direct
buffered reads, large immutable blobs, dual-index sorted sets, bounded
sorted-set score ranges, bidirectional sorted-set member ranks, and reverse
sorted-set rank/score ranges are implemented in the convergence slice; the
ordered durable scalar-expiry index and bounded cleanup are
implemented; an engine-owned background timer, version-bearing responses, and
the remaining structure families remain pending.

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
chunked-deque lists, plus dual-index sorted sets with bidirectional member-rank
lookup and bounded score ranges. Streams, bitmaps, sketches, geo indexes, and
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
DELETE_HASH(key)
EXPIRE_HASH(key, expires_at)
TTL_HASH(key)
HSET(key, field, value)
HGET(key, field)
HDELETE(key, field)
HLEN(key)
HSCAN(key, start_after, limit)
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
ZRANGE_BY_SCORE(key, lower, upper, offset, limit)
ZREVRANGE(key, start, stop)
ZREVRANGE_BY_SCORE(key, lower, upper, offset, limit)
EXPIRE_DUE(logical_time, max_keys)
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

`CREATE_HASH` establishes the key's family before field mutation.
`DELETE_HASH` returns false without a mutation for a missing key, fails with a
kind error for another live structure family, and otherwise retires the
complete hash. `HSET` returns added versus updated, `HGET` reads one field,
`HDELETE` publishes a field tombstone, and `HLEN` reads durable cardinality.
`EXPIRE_HASH` applies one absolute expiry to the complete hash and
`TTL_HASH` evaluates it against deterministic logical time. Missing or already
expired hashes return false from `EXPIRE_HASH`; due hashes are absent to every
hash operation before physical cleanup.

`HSCAN` returns at most `limit` live field/value pairs in ascending exact
field-byte order. `start_after` is an optional exclusive field cursor, not an
opaque server token: the caller resumes with the last returned field. `None`
starts at the first field, while `Some(empty)` resumes after a real empty
field. A cursor does not need to remain live; its bytes still define the
exclusive lower bound. A zero limit validates the key kind and hash existence
before returning an empty result.

Private-transaction, retained-snapshot, current-root physical, and reopened
execution must return identical entries for the same visible state. The
physical route maps `start_after` directly into the hash-field B+tree
namespace, skips field tombstones without charging the limit, and stops after
the requested number of live entries. It must not materialize the complete
hash. A reached malformed identity, field envelope, expiry, or blob fails the
complete call rather than returning a partial result. Pattern matching,
reverse iteration, and whole-hash materialization are outside this first
bounded scan contract.

This bounded `HSCAN` contract is implemented across private, retained,
current-root, and reopened execution. Its red/green and physical-pruning
evidence are tracked separately from this normative file. Whole-hash
lifecycle, conflict-fence, crash, compaction, and separated durability-latency
evidence are bound in the
[native hash-lifecycle receipt](../gates/evidence/native-hash-lifecycle-linux-2026-08-02.md).

An empty hash remains a typed hash after its last field is deleted.
`DELETE_HASH` is the explicit family-lifecycle boundary: the deleted hash and
all of its fields become absent at one CSN while retained snapshots preserve
their earlier view. The same transaction may recreate the key as an empty
hash or another structure kind after deletion. Recreating a hash never exposes
fields from the retired incarnation.

Whole-hash expiry is the automatic equivalent lifecycle boundary. A
transaction may reuse a due hash key as a scalar or explicitly created
collection; it first records logical whole-hash cleanup so metadata, all
fields, and the old expiry entry retire in the same publication as the new
family. Retained snapshots before the boundary preserve the older incarnation.

Scalar mutation of a hash key fails with a kind error. Concurrent scalar
creation and hash creation over the same absent key conflict. Once the hash
exists, different field identities can prepare and commit independently;
same-field writers retain first-committer-wins semantics. Every field mutation
also validates, but does not publish, the hash lifecycle fence observed by its
snapshot. Hash creation and whole-hash deletion publish that fence. Therefore
a field writer prepared before an admitted deletion cannot resurrect the
retired hash, while disjoint fields in one live incarnation remain
independently committable. If a field mutation commits first, a later
whole-hash deletion rebases over and retires that admitted field as part of the
same logical hash.

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

`ZRANGE_BY_SCORE` accepts independently inclusive, exclusive, or unbounded
minimum and maximum scores plus a nonnegative `offset` and `limit`. Results
remain ascending by score and exact member bytes. Offset counts only live
members inside the score interval, and execution returns at most `limit`
members. A zero limit still validates the sorted-set kind, existence, and
both score bounds before returning an empty result. Equal inclusive endpoints
select that exact score; every other equal-endpoint combination, or a lower
endpoint greater than the upper endpoint, returns an empty result.

Private-transaction, retained-snapshot, current-root physical, and reopened
execution have identical results. Current-root execution maps the canonical
score bounds directly onto the ordered `0x0a` B+tree namespace, prunes
nonintersecting subtrees, ignores tombstones without charging offset, and
stops after the requested live result count. It does not materialize the
complete sorted set. Malformed ordered identities, scores, or live markers
fail the complete call rather than returning a partial result.

`ZREVRANGE` applies the same signed, inclusive `start` and `stop` rules to the
descending score/member sequence. Negative indexes count back from the tail of
that descending sequence. Reversing the result reverses the complete total
order, including exact member-byte order between equal scores.

`ZREVRANGE_BY_SCORE` accepts the same canonical lower and upper score bounds
as `ZRANGE_BY_SCORE`; output direction never changes which argument is the
lower or upper bound. It filters that interval, traverses matching live
members in descending score/member order, applies `offset` in that descending
order, and returns at most `limit` entries. Empty, inverted, equal-endpoint,
zero-limit, infinity, and `NaN` behavior is identical to the ascending
operation before output direction is considered.

Private-transaction, retained-snapshot, current-root physical, and reopened
reverse execution must be identical. The physical rank route starts at the
ordered prefix tail and stops after the last requested descending live rank.
The physical score route applies the canonical score bounds to the same
ordered namespace and visits only that bounded interval in reverse. Both
ignore tombstones without charging rank or offset and stop without
materializing the complete sorted set. Malformed metadata, identities, scores,
or live markers fail the complete call rather than returning a partial result.

These two reverse-range operations are implemented across the private,
snapshot, current-root, and reopened read surfaces. Their release evidence is
tracked separately from this normative contract.

`ZRANK` returns a live member's zero-based position in that exact ascending
score/member order. `ZREVRANK` returns its zero-based position after reversing
the complete total order, so equal-score members also reverse their bytewise
order. A missing member returns no rank; a missing sorted set or another
structure kind remains a typed error. Private transaction, retained snapshot,
current-root physical, and reopened execution have identical results.

Current-root rank lookup validates metadata and resolves the member's
canonical score through the membership index before targeting its exact
ordered identity. `ZRANK` walks the ordered `0x0a` namespace ascending through
that target; `ZREVRANK` walks it descending through the target. Both ignore
tombstones without charging rank, stop when the live target is found, and do
not materialize the complete sorted set. A missing or non-live target ordered
entry, visited live target under a conflicting score, malformed visited
identity, score, or marker, or a live rank reaching the declared cardinality
fails the complete call. This first lookup contract does not add subtree live
counts; order-statistic acceleration remains open.

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
| `0x00` | exact one-byte format key | ASCII `HYSTRBT2` |
| `0x01` | prefix + arbitrary binary user key | canonical `HYSTRV01` value |
| `0x02` | prefix + binary hash key | canonical `HYHSHM01` or `HYHSHM02` metadata |
| `0x03` | prefix + hash-field identity | canonical persistent `HYSTRV01` value |
| `0x04` | prefix + binary set key | canonical `HYSETM01` metadata |
| `0x05` | prefix + set-member identity | canonical empty persistent `HYSTRV01` value |
| `0x06` | prefix + binary list key | canonical `HYLSTM01` metadata |
| `0x07` | prefix + list-key identity + ordered chunk ID | canonical `HYLSTC01` chunk or structure tombstone |
| `0x08` | prefix + binary sorted-set key | canonical `HYZSTM01` metadata |
| `0x09` | prefix + sorted-set-member identity | canonical `HYZSCR01` score or structure tombstone |
| `0x0a` | prefix + sorted-set key + sortable score + member | canonical empty persistent `HYSTRV01` value or structure tombstone |
| `0x0b` | prefix + sortable expiry + binary structure key | typed one-byte live marker or tombstone |

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

Scalar and whole-hash logical expiry are independent from physical cleanup. The optional
[native active-expiry scheduler](active-expiry-scheduler-v1.md) visits the
ordered expiry namespace through the single native writer and commits bounded
tombstone transactions without making the timer a visibility authority.

Every `SET`, `EXPIRE`, and `DELETE` upserts one key through a new copy-on-write
path. Retained roots preserve older values, TTLs, and pre-delete visibility.
Current direct `GET` and `TTL` traverse verified pinned pages without
materializing the complete structure state, then decode only the selected
envelope and blob. Recovery scans and validates the complete reachable
namespace while omitting tombstones from materialized state.

Persistent hash metadata remains the exact 16-byte `HYHSHM01` encoding:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYHSHM01` |
| 8 | 8 | unsigned little-endian live field count |

Expiring hashes use the exact 24-byte `HYHSHM02` encoding:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYHSHM02` |
| 8 | 8 | unsigned little-endian live field count |
| 16 | 8 | signed little-endian absolute expiry |

The distinct magic is the expiry-presence bit, so every signed timestamp
round-trips without a sentinel collision. The `0x0b` marker is `0x00` for a
tombstone, `0x01` for a live scalar expiry, or `0x02` for a live whole-hash
expiry. Recovery validates an exact one-to-one typed match.

A hash-field identity is `u32` big-endian hash-key length, the hash-key bytes,
and the remaining field bytes. The physical key prepends `0x03`. This encoding
is unambiguous for empty or arbitrary binary keys and fields and keeps fields
clustered by hash-key length/key/field order.

Each field uses the same inline/blob `HYSTRV01` envelope but cannot carry an
independent expiry; the family metadata carries the whole-hash expiry. A field
delete stores the same canonical
tombstone as a scalar delete. Recovery requires every field to have prior hash
metadata and requires metadata cardinality to equal the exact number of live
field envelopes. Orphan fields, malformed identities, expiry-bearing fields,
and count mismatches fail closed.

Whole-hash deletion writes canonical `HYSTRV01` tombstones over every live
field path, the hash metadata path, and any live whole-hash expiry path through
one copy-on-write B+tree batch.
The WAL carries only the logical hash key; page construction enumerates the
admitted current-root field prefix after conflict validation. Recovery accepts
field tombstones under retired metadata, rejects live orphan fields, and
omits the retired family from materialized state. Recreation upserts live
metadata over the metadata tombstone while the retired field tombstones remain
non-visible until individually replaced.

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
and continues reading and writing it without an implicit conversion.
`HYSTRBT1` B+tree directories remain readable and writable through a
compatibility path that reconstructs due expiries from scalar envelopes.
New directories use `HYSTRBT2`; only that marker requires and maintains the
ordered expiry namespace.

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

`CREATE_HASH`, `DELETE_HASH`, and `EXPIRE_HASH` publish the scalar/collection ownership
identity as the hash lifecycle fence. `HSET` and `HDELETE` validate that
identity in addition to their field write key but publish only the field key.
This separates incarnation safety from field-level write admission.
`DELETE_HASH` physically scans and tombstones the admitted hash prefix, so its
cost is linear in retained physical field paths even though its WAL body is
bounded.

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
expired version from that snapshot even before physical cleanup. Historical
proofs pin both a root and logical time, so active cleanup cannot alter an
older snapshot.

Every live expiring scalar has exactly one live entry in the ordered expiry
namespace. Its physical key is:

```text
0x0b || sortable_i64(expires_at_micros) || scalar_key
```

`sortable_i64` flips the sign bit of the signed timestamp and writes the
result as unsigned big-endian bytes. Lexicographic order therefore matches
the complete signed timestamp domain, with exact binary scalar-key order as
the tie-break. The value is exactly `0x01` for live and `0x00` for a
tombstone; every other value fails closed. An expiry-bearing scalar key must
fit both its scalar identity and this nine-byte index overhead.

`SET`, `EXPIRE`, `DELETE`, and counter writes maintain the scalar envelope and
expiry index in the same structure mutation, WAL transaction, root
publication, and CSN. Replacing or removing an expiry tombstones the prior
index identity. Adding or changing an expiry publishes the new identity.
Rewriting a value under the same expiry keeps that identity live.

`EXPIRE_DUE(logical_time, max_keys)` visits the current physical index in
timestamp/key order, selects at most `max_keys` live identities whose expiry
is less than or equal to `logical_time`, revalidates each scalar against the
same deterministic time, and commits their scalar and index tombstones as one
native transaction. `max_keys` is in `1..=4096`; zero or a larger value is a
typed error. No due keys means no WAL record and no CSN advancement. The
receipt reports the expired-key count, whether another due live identity was
observed, and the optional commit receipt.

For `HYSTRBT2`, cleanup preparation is a physical-root operation. After the
ordered scan validates each selected scalar envelope, it builds a
structure-only delete batch without materializing the catalog, relational,
search, ANN, or complete structure state. The commit path admits that fast
path only when all four prior roots exist, only the structure slot is dirty,
the structure format is `HYSTRBT2`, and every mutation is one canonical scalar
delete. It otherwise fails closed. `HYSTRBT1` and whole-state compatibility
formats keep their materialized fallback.

The admitted `HYSTRBT2` cleanup resolves every scalar tombstone and matching
expiry-index tombstone against the captured structure root, sorts the complete
physical key/value set, and publishes it through one ordered B+tree batch.
Each affected existing node is rewritten at most once for that cleanup; an
unaffected subtree retains its prior page ID. Duplicate physical identities or
any scalar/index mismatch fail before the batch can publish. Cleanup evidence
must report both latency and pages appended so a faster result cannot hide
greater write amplification.

Recovery reconstructs pending work directly from the durable ordered
namespace. It requires a one-to-one match between every live expiry identity
and every scalar envelope carrying that exact timestamp. Missing, duplicate,
stale, malformed, persistent-key, or orphan live identities fail closed.
Index tombstones are ignored for logical reconstruction and remain physical
until native compaction; scan-amplification evidence is therefore required
before G3 closes.

At or after the exact expiry, both `GET` and `TTL` report the key as missing
whether or not `EXPIRE_DUE` has published cleanup. An optimistic cleanup batch
uses the scalar write identity, so a concurrent renewal of the same key wins
or conflicts under first-committer-wins; cleanup cannot delete the renewed
value.

### Reachability compaction

`COMPACT STRUCTURE` is an explicit physical-maintenance operation admitted
only for `HYSTRBT2`. It captures the complete current root set, validates the
entire structure tree and all cross-entry invariants, and scans the current
physical entries in canonical order before appending any page.

The rebuild retains the format marker and every live physical entry
byte-for-byte. It drops only canonical tombstones from scalar, hash-field, set
member, list-chunk, sorted-set member/order, and expiry-index namespaces. Any
unknown namespace, malformed value, orphan live entry, count mismatch, or
scalar/expiry mismatch fails before the replacement tree is built. An empty
tombstone set is a no-op: it appends no page, writes no WAL record, and advances
no CSN.

When work exists, one `COMPACT STRUCTURE=28` maintenance mutation with no
target, key, value, or expiry binds the physical rewrite to the WAL. A fresh
balanced B+tree is built through the ordered batch primitive, the other three
engine roots remain byte-identical, and the replacement structure root is
published under one new global CSN. Writer admission requires the captured
root set to remain current; otherwise the operation fails for retry instead of
compacting stale state.

The receipt reports scanned, retained, and dropped physical entries, reachable
node pages before and after, appended pages, and the optional commit receipt.
The pre-compaction snapshot and root remain readable and reconstruct the same
logical structure state as the new root. Every commit interruption recovers
either the prior root or the complete replacement root.

This operation removes tombstones from the current reachable tree but does not
shrink the append-only `pages.hydb` file. Older roots and immutable manifests
still own the superseded pages. Physical file reclamation requires an explicit
retention floor, complete cross-engine reachability tracing, a new page-file
generation, and atomic generation publication. The
[native page-generation vacuum contract](page-vacuum-v1.md) now defines that
separate current-root protocol; implementation and evidence remain pending.

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
controlled-clock TTL tests, expiry-index reconstruction, bounded cleanup,
blocking cancellation, memory-amplification receipts, eviction safety, and
cross-engine transactions.

Current experimental tests cover a 2,048-key multilevel tree, historical roots,
direct TTL and expiry reads, canonical tombstones, `NX`/`XX`, racing `NX`
writers, signed counter bounds, strict reopen, canonical-envelope corruption,
legacy whole-page and `HYSTRBT1` compatibility, optimistic disjoint-key
rebase, all commit crash boundaries with scalar and hash mutations, durable
expiry reconstruction and cleanup at all seven commit boundaries, stale
cleanup/renewal conflict, full signed timestamp order, forged expiry-index
corruption, typed scalar/hash creation races, disjoint-field rebase,
same-field conflict, hash count corruption, field tombstones, typed
scalar/hash/set creation races, disjoint-member rebase, same-member conflict,
set count corruption, member tombstones, chunked-list
restart/range/corruption/conflict behavior, and one blob deduplicated across
relational, scalar, hash field, and list values. They do not close the
structure gate.
