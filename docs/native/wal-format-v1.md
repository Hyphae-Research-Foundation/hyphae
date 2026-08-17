<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native WAL format v1

Status: normative target contract; block/record framing, append, integrity
chain, incomplete-tail repair, typed transaction envelopes, root-manifest
checkpoint anchors, and page-generation commit metadata are implemented
experimentally; committed mutation decoding reconstructs the write-conflict
table and the first bounded group-commit scheduler shares page/WAL flushes,
while current-root WAL retention, bounded suffix replay, and idempotent
retention retries are implemented

The identity-preserving current-root design for bounded replay and prefix
deletion is fixed separately by [Native WAL retention and bounded replay
v1](wal-retention-v1.md).

The WAL is the only transaction authority for the three native engines. It
records one cross-engine transaction, not three engine-specific commits.

## Block layout

- Block size: 65,536 bytes.
- Header size: 112 bytes.
- Maximum block payload: 65,424 bytes.
- Multi-block records are forbidden. Large values must use blob references.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYWAL001` |
| 8 | 2 | format version `1` |
| 10 | 2 | header length `112` |
| 12 | 4 | flags |
| 16 | 8 | block sequence |
| 24 | 8 | first record LSN |
| 32 | 8 | last record LSN |
| 40 | 4 | payload length |
| 44 | 4 | CRC32C with integrity fields zeroed |
| 48 | 32 | previous complete-block digest |
| 80 | 32 | BLAKE3 digest of header and payload with this field zeroed |
| 112 | variable | record payload followed by zero padding |

LSN is the byte offset of a record header in the logical WAL stream. Block
sequences and complete record LSNs are strictly increasing.

## Record header

Every record begins with:

| Width | Field |
|---:|---|
| u32 | total record length |
| u32 | body length |
| u8 | record kind |
| u8 | engine: kernel `0`, relational `1`, structure `2`, search `3` |
| u16 | flags |
| u64 | record LSN |
| 128 bits | transaction ID |
| u32 | record CRC32C |
| u32 | reserved zero |

Record kinds are `BEGIN`, `MUTATION`, `COMMIT`, `ABORT`, `CHECKPOINT`, and
`CATALOG`. Unknown kinds or versions fail closed.

## Transaction records

`BEGIN` contains:

- snapshot/read CSN;
- catalog version;
- transaction logical time in signed UTC microseconds;
- durability class; and
- bounded operation and byte ceilings.

`MUTATION` contains a versioned engine opcode, target `ObjectId`, canonical key
bytes, canonical value/reference bytes, and the expected prior version when
conflict detection requires it.

The implemented `HYMUT001` body is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | magic `HYMUT001` |
| 8 | 1 | opcode |
| 9 | 1 | engine |
| 10 | 1 | flags; bit 0 means expiry is present |
| 11 | 1 | reserved zero |
| 12 | 16 | target object ID; zero for the structure keyspace |
| 28 | 8 | signed absolute expiry; legacy no-expiry sentinel is `i64::MAX` |
| 36 | 4 | key-byte length |
| 40 | 4 | value-byte length |
| 44 | variable | key bytes followed by value bytes |

The expiry-presence flag lets an explicit timestamp of `i64::MAX` round-trip.
For compatibility, the decoder also accepts an earlier flag-zero body with a
non-`i64::MAX` expiry as present. Unknown flag bits fail closed.

Current opcodes are relational `CREATE TABLE=1`, `INSERT ROW=2`, `UPDATE
ROW=6`, `DELETE ROW=7`, `CREATE SECONDARY INDEX=13`; structure `SET VALUE=3`,
`DELETE VALUE=8`, `EXPIRE VALUE=9`, `CREATE HASH=10`, `SET HASH FIELD=11`,
`DELETE HASH FIELD=12`, `CREATE SET=14`, `ADD SET MEMBER=15`, `DELETE SET
MEMBER=16`, `CREATE LIST=20`, `PUSH LIST HEAD=21`, `PUSH LIST TAIL=22`, `POP
LIST HEAD=23`, `POP LIST TAIL=24`, `CREATE SORTED SET=25`, `UPSERT SORTED SET
MEMBER=26`, `DELETE SORTED SET MEMBER=27`, `COMPACT STRUCTURE=28`, `DELETE
HASH=30`, `EXPIRE HASH=31`, `EXPIRE HASH FIELD=32`, `EXPIRE SET=33`, `DELETE
SET=34`, `DELETE LIST=35`, `EXPIRE LIST=36`; and search
`CREATE INDEX=4`, `INDEX DOCUMENT=5`, `CREATE ANN INDEX=17`, `UPSERT VECTOR=18`,
`DELETE VECTOR=19`, `REPLACE DOCUMENT=37`, `DELETE DOCUMENT=38`.
`DELETE VALUE`, collection creation, whole-hash/hash-field deletion, set/sorted-set
member deletion, and vector deletion require an empty value and no expiry.
`EXPIRE VALUE` requires an explicit expiry and carries the retained logical
value. The ordered expiry index is a derived physical structure maintained by
the existing scalar opcodes, so it introduces no second mutation stream or
WAL opcode.

`EXPIRE HASH` uses the exact binary hash key, no target, an empty value, and an
explicit signed expiry. The logical mutation is the replay authority; it
updates versioned hash metadata and the typed ordered expiry entry without
copying fields into the WAL body. Expiry-driven physical cleanup reuses
`DELETE HASH`.

`EXPIRE HASH FIELD=32` is the accepted additive opcode specified by
[Native hash field TTL v1](native-hash-field-ttl-v1.md). It uses the canonical
compound hash-key/field identity, no target, an empty value, and one explicit
signed expiry. Replay preserves the admitted field value while deriving its
collision-free field-expiry index update.

`EXPIRE SET=33` and `DELETE SET=34` are the complete-set lifecycle opcodes
specified by [Native whole-set TTL v1](native-set-ttl-v1.md).
`DELETE LIST=35` and `EXPIRE LIST=36` are the complete-list lifecycle opcodes
specified by [Native whole-list lifecycle v1](native-list-lifecycle-v1.md) and
[Native whole-list TTL v1](native-list-ttl-v1.md). The expiry mutations carry
an empty value and one explicit signed expiry. Complete deletion carries no
expiry and is also the replay authority used by active cleanup.

`REPLACE DOCUMENT=37` and `DELETE DOCUMENT=38` are the lexical lifecycle
opcodes specified by
[Native lexical document lifecycle v1](search-document-lifecycle-v1.md).
Both use the exact binary document ID as key, the nonzero search collection as
target, and no expiry. Replacement carries UTF-8 source text; deletion carries
an empty value. They are the only replay authority for their derived
document, collection-statistic, term, and posting updates.

`COMPACT SEARCH=39` is the physical lexical-rebuild opcode specified by
[Native lexical tombstone compaction
v1](search-tombstone-compaction-v1.md). It requires the search engine, a zero
target, empty key and value, and no expiry. The complete prior search root is
its deterministic rebuild authority; recovery never interprets the empty
maintenance body as a logical delete set. The opcode cannot be combined with
user mutations.

`COMPACT STRUCTURE` is a physical-maintenance opcode. It requires the structure
engine, a zero target, empty key and value, and no expiry. Its commit advances
the global CSN and names the replacement structure root while retaining the
other engine roots. The mutation is not a logical delete stream and cannot be
combined with user mutations. Recovery validates the complete prior and
replacement roots; it never attempts to infer dropped entries from the empty
maintenance body.

Relational `CREATE TABLE` and `CREATE SECONDARY INDEX` carry one complete
`HYCOBJ01` definition as their value and the normalized qualified-name
identity as their key. Secondary-index entry changes are deliberately not
independent WAL mutations. An admitted index definition plus the canonical row
mutation is the single projection authority: index creation backfills the
current admitted rows; row insertion derives live projections; update removes
old projections and adds new ones; delete removes old projections. Optimistic
rebase, page construction, and recovery repeat those derivations from the
admitted catalog and canonical row mutations. This prevents separate row/index
operation streams from diverging.

Hash field mutations use `u32` big-endian hash-key length, hash-key bytes, and
field bytes as their mutation key. The decoder rejects truncated identities.
This makes first-committer-wins field-granular while keeping creation of a hash
on the same write key as scalar creation.

`DELETE HASH` uses the exact binary hash key, an empty value, no expiry, and no
target. It shares the scalar/collection ownership conflict identity with
`CREATE HASH` and `EXPIRE HASH`. Hash-field mutations retain their field write
identity and also
validate that ownership identity as a lifecycle read dependency without
publishing it. This rejects a stale field mutation after deletion/recreation
without serializing disjoint field writers. The bounded logical mutation is
the replay authority; physical publication deterministically tombstones the
admitted current-root hash metadata and field prefix.

Set member mutations use the same compound identity with the set key followed
by the member bytes. Conflict identities add disjoint scalar/collection,
hash-field, and set-member domains, so arbitrary binary user keys cannot alias
a member mutation. Set creation shares the scalar/collection ownership domain;
different set members retain first-committer-wins independently.

List mutation keys are the exact binary list key. Creation carries an empty
value; pushes carry the inserted logical bytes; pops carry the exact removed
logical bytes, including an allowed empty value. Expiry and target IDs are
forbidden. WAL publication replaces each push/pop logical value with the same
persistent `HYSTRV01` inline/blob envelope used by the physical chunk, making
end-value verification content-bound without duplicating a large payload.
Creation and every end mutation share the scalar/collection ownership conflict
identity, so concurrent list writers retain first-committer-wins semantics.

`CREATE ANN INDEX` carries the complete catalog `HYCOBJ01` search definition
and normalized name identity. `UPSERT VECTOR` and `DELETE VECTOR` require a
16-byte big-endian object ID key; upsert values contain one or more canonical
little-endian `f32` components and delete values are empty. Expiry is
forbidden. Recovery groups the ordered vector mutations by target index,
rebuilds one canonical HNSW generation with the commit CSN, and persists it in
the search B+tree. The conflict table keys vector writes by target index plus
object ID, so disjoint vectors may rebase while same-vector writers retain
first-committer-wins.

`COMMIT` contains:

- read CSN and assigned commit CSN;
- catalog version;
- immutable blob generation;
- mutation count and aggregate mutation bytes;
- logical commit time;
- BLAKE3 digest of the ordered canonical mutation records; and
- the four current catalog, relational, structure, and search root page IDs.

The implemented `HYCMT001` body is exactly 124 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | magic `HYCMT001` |
| 8 | 8 | read CSN; zero means genesis |
| 16 | 8 | commit CSN |
| 24 | 8 | catalog version |
| 32 | 8 | blob generation |
| 40 | 4 | mutation count |
| 44 | 8 | aggregate mutation-body bytes |
| 52 | 8 | logical UTC microseconds |
| 60 | 32 | ordered mutation-set BLAKE3 digest |
| 92 | 32 | four little-endian root page IDs |

The current relational, scalar `SET`/`EXPIRE`, hash-field `HSET`, and search
`INDEX DOCUMENT`/`REPLACE DOCUMENT` mutation bodies store values at or below
8,192 bytes inline. Larger values are promoted to the shared immutable blob
namespace first. The WAL stores the relational one-byte envelope, structure
`HYSTRV01` envelope, or search `HYDOCS01` envelope with its 56-byte reference
instead of duplicating large content. The search envelope also binds the
analyzed token count. A structure, whole-hash, hash-field, or lexical-document
delete keeps its WAL value empty; page construction publishes the owning
namespace's canonical tombstone.

`ABORT` is advisory and never makes preceding mutations visible.

## Ordering and atomicity

- A transaction has exactly one `BEGIN`, zero or more `MUTATION` records, and
  at most one terminal `COMMIT` or `ABORT`.
- Empty user commits are rejected.
- A committed transaction's mutation count and digest must match.
- Reusing a transaction ID with identical contents returns the original
  receipt. Reuse with different contents is an idempotency conflict.
- A commit CSN is unique and strictly increasing.
- A recovered read CSN is either genesis or an existing CSN lower than its
  commit CSN. It may lag the immediately preceding commit when a detached
  transaction prepared from an older snapshot and its write set remained
  conflict-free.
- Engine mutations become visible only when the root set named by `COMMIT` is
  installed and `global_visible_csn` advances.

## Durability classes

- `strict`: write the transaction and synchronize before acknowledgement.
- `group`: combine multiple transaction blocks in one synchronization, then
  acknowledge each included commit.
- `memory`: publish without synchronization; recovery may lose the acknowledged
  suffix and receipts must identify that risk.

All benchmark and API receipts name the durability class.

The scheduler, admission, receipt, failure, and shared-flush requirements for
`group` are fixed by [Native group commit v1](group-commit-v1.md). A group is a
durability cohort of independent WAL transactions, not one atomic
super-transaction.

## Recovery

1. Verify the checkpoint chain and each referenced root manifest.
2. Scan blocks from the selected checkpoint LSN.
3. Truncate only an incomplete physical tail.
4. Reject every complete corrupt block, record, sequence, digest chain,
   transaction boundary, or content digest.
5. Ignore complete transactions without a valid commit.
6. Replay committed transactions in CSN order.
7. Verify or rebuild the committed root set before advancing visibility.

Recovery never guesses an opcode or skips an unknown committed mutation.
Without a retention anchor, the current vertical still scans the complete WAL.
With a verified native `HYWAR002` anchor, it reconstructs the base roots from
the bound immutable manifest, verifies and decodes only the identity-preserving
WAL suffix, and rebuilds point-write conflict state from that suffix. The
manifest chain is pruned from that exact lineage-bearing trust root.
`HYWAR001` remains decodeable only for historical tooling and is not authority
under `FORMAT`.

## Checkpoints

A kernel `CHECKPOINT` occurs outside a user transaction and has this exact
64-byte body:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYCHK001` |
| 8 | 8 | visible committed CSN |
| 16 | 8 | root-manifest generation |
| 24 | 32 | complete root-manifest digest |
| 56 | 8 | prior checkpoint record LSN; zero for the first |

Recovery verifies the checkpoint sequence against the complete WAL commit and
immutable manifest chains. A published manifest without a matching record is
an unanchored suffix and cannot independently become transaction authority.
See [Native root manifest and checkpoint format
v1](root-manifest-checkpoint-v1.md).

The implemented v1 maintenance path retires only the prefix through a
current-root checkpoint whose visible CSN equals its retention floor. It
rejects an ineligible checkpoint and preserves absolute block, LSN, digest,
CSN, and transaction identities. Replica, backup, archive, and
multi-generation pin registration remain outside v1.

## Verification

Required tests cover golden blocks, all record kinds, transaction idempotency,
cross-engine ordering, torn writes at every byte, complete corruption,
sequence/digest divergence, unknown opcode rejection, group synchronization
receipts, checkpoint replay, and bounded recovery.

Current tests cover the block golden, complete transaction envelope, semantic
mutation round-trip and count/digest verification, checkpoint encoding/chain
validation, blob-reference commits, complete corruption, incomplete physical
tail repair, deterministic blob/page/WAL/root/checkpoint interruptions, and
set creation/member and ANN create/upsert/delete mutation round-trips. ANN
shape tests reject truncated object identities and non-`f32`-aligned payloads;
the cross-engine ANN matrix interrupts every implemented commit boundary. The
direct-Linux process-crash matrix additionally retains the writer lock until
the parent sends `SIGKILL` at every singleton blob/page/WAL/root boundary, then
reopens to either the prior state or the complete relational, structure, and
lexical CSN. Kernel page-cache survival means this is not sector, filesystem
reordering, device-cache, or physical power-loss evidence. The broader list
above remains gate work.
