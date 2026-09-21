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
- exact nonzero mutation count and exact aggregate encoded mutation bytes.

The writer computes both aggregates before any blob or page publication.
Recovery treats them as exact: it rejects zero-mutation transactions and checks
each next mutation against the declared count, byte total, universal limits,
and decoded-memory authority before retaining that mutation.

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

Before publication, the writer preflights the complete transaction's record
slots, encoded block vectors, and cloned key/value payload capacities with
checked arithmetic. That `wal_publication_peak` is simultaneously live with the
transaction's retained mutation, proof, and page-publication state, so governed
admission adds the two values; it must not take their maximum or treat retained
memory as reusable WAL scratch. The WAL peak is subdivided from that combined
permit before page or WAL append. Consolidation applies the same rule after
adding target hydration, replacement-build ownership, exact-prefix structural
work, replacement key/value clones, and the fixed C02 payload. Overflow or an
understated replay rejects before publication.

The complete append-only opcode inventory is:

| Opcode | Engine | Mutation |
|---:|---|---|
| 1 | relational | create table |
| 2 | relational | insert row |
| 3 | structure | set value |
| 4 | search | create lexical index |
| 5 | search | index document |
| 6 | relational | update row |
| 7 | relational | delete row |
| 8 | structure | delete value |
| 9 | structure | expire value |
| 10 | structure | create hash |
| 11 | structure | set hash field |
| 12 | structure | delete hash field |
| 13 | relational | create secondary index |
| 14 | structure | create set |
| 15 | structure | add set member |
| 16 | structure | delete set member |
| 17 | search | create ANN index |
| 18 | search | upsert vector |
| 19 | search | delete vector |
| 20 | structure | create list |
| 21 | structure | push list head |
| 22 | structure | push list tail |
| 23 | structure | pop list head |
| 24 | structure | pop list tail |
| 25 | structure | create sorted set |
| 26 | structure | upsert sorted-set member |
| 27 | structure | delete sorted-set member |
| 28 | structure | compact structure |
| 29 | kernel | vacuum page generation |
| 30 | structure | delete hash |
| 31 | structure | expire hash |
| 32 | structure | expire hash field |
| 33 | structure | expire set |
| 34 | structure | delete set |
| 35 | structure | delete list |
| 36 | structure | expire list |
| 37 | search | replace document |
| 38 | search | delete document |
| 39 | search | compact search |
| 40 | relational | drop secondary index |
| 41 | relational | rename table |
| 42 | structure | migrate structure v3 |
| 43 | relational | drop table |
| 44 | structure | create stream |
| 45 | structure | append stream entry |
| 46 | structure | delete stream |
| 47 | structure | expire stream |
| 48 | structure | delete sorted set |
| 49 | structure | expire sorted set |
| 50 | search | consolidate ANN |
| 51 | kernel | create catalog object v2 |
| 52 | structure | clean structure-v3 retirement |
| 53 | search | publish initial ANN bulk |
| 54 | kernel | migrate catalog v7 |
| 55 | search | internal HYANNA01 delta authority |
| 56 | search | internal HYANNA02 overlay authority |
| 57 | search | fence vector absence |

`DELETE VALUE`, collection creation, whole-hash/hash-field deletion, set/sorted-set
member deletion, vector deletion, and vector-absence fencing require an empty
value and no expiry.
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
forbidden. Creation plus same-transaction vectors builds canonical M04. Later
physical point upsert or delete publishes M05 without rebuilding the HNSW base.
Internal opcode 56 adds one trailing fixed 184-byte `HYANNA02` authority per
physically changed point-writer index; it binds the duplicated index, exact
physical operation count, prior/result view and sparse-Merkle roots,
prior/result sequence, and M04-to-M05 flag. Its complete byte layout is fixed by
[ANN delta overlay format v1](../storage/ann-delta-overlay-format-v1.md).
Markers are sorted, unique, exact-covering, and cannot mix with opcode 55
`HYANNA01`.

Three append-only meanings remain distinct:

- historical unmarked `DELETE VECTOR=19` retains the M01-through-M04 physical
  delete and index-generation conflict semantics under which it was written;
- M05 physical `DELETE VECTOR=19` requires a live admitted object, writes one
  authenticated D02 tombstone, and is covered by the index's `HYANNA02`; and
- `FENCE VECTOR ABSENCE=57` requires a nonzero target index, one exact 16-byte
  big-endian object ID key, empty value, and no expiry. It proves that object
  absent in the prior committed ANN view and authorizes no physical change.

Opcode 57 is an ordinary logical mutation, not an internal marker. It consumes
one transaction mutation/count entry but no D02 leaf slot, Merkle node,
metadata/manifest replacement, M05 sequence, view-identity transition, or
`HYANNA02.operation_count`. A transaction admits at most 4,096 opcode 57
records in total. A mixed transaction's marker count and
`result_next_sequence - prior_next_sequence` equal only its opcode 18 and
physical opcode 19 members. A fence-only index has no marker; an unmarked
physical M05 delete, a marker that counts a fence, and a marker without a
physical point are invalid. Legacy unmarked and HYANNA01 transactions retain
historical decode and conflict semantics.

For HYANNA02, live and recovery validation use object plus generation authority
while publication records only the object writer. Disjoint object upserts and
deletes may therefore rebase while same-object writes retain
first-committer-wins. Opcode 57 validates and publishes its object conflict key
and the index lifecycle-fence key; it does not publish the generation key. This
makes same-object fence/upsert/delete races conflict in either order, allows
disjoint physical points to compose, and conflicts with stale initial-bulk or
consolidation replacement in either order.

Recovery first streams a bounded immutable-root comparison over every ANN
namespace `[0x05,0x0c)`; it does not trust WAL mutations or metadata changes to
enumerate targets. Both roots' decoded metadata bound changed keys, changed
indexes, and changed-node work. Every changed key contributes its embedded
index. If either side's metadata is M05, a point-writer transaction requires
the HYANNA02 marker and physical vector-mutation target to match exactly;
opcode 50 instead requires the HYANNC02 replacement authority defined below.
Recovery then replays the selected proof and performs a bounded immutable-page
diff over every target ANN prefix. Equal page IDs are inherited, while changed
paths, deletions, splits, and height changes must yield exactly the authorized
replacement with no extra key. Empty prior roots are traversed as empty trees,
preventing an unproved M05 first commit while preserving M01-M04 creation.

Recovery also takes opcode 57 targets from WAL, replays D02-first/D01-second/
base-last prior absence against the prior root, authenticates the exact D02
leaf or non-membership path, and requires zero physical change attributable to
each fenced object/path beyond co-committed physical point mutations. It then
rebuilds the same object and lifecycle-fence conflict keys used by live
admission. Only after these proofs is conflict state rebuilt. Omitted, extra,
stripped, retargeted, and HYANNA01-downgraded M05 point markers fail. M05
consolidation publishes the generation fence under HYANNC02. Initial-bulk
rewriting of M05 remains outside the writer and fails closed.

A transaction whose mutations are all opcode 57 must retain all four prior
root page IDs, blob generation, and catalog version exactly. Its `COMMIT` still
records the transaction ID, assigns the next CSN, appends WAL, and advances
global visible authority; there is no exception for a physically unchanged
search root.

M01 through M03 remain writable through their frozen legacy path. An accepted
ordinary vector upsert or delete reconstructs that target under the historical
rules and atomically publishes M04 metadata; a read, rejected mutation, or
failed commit does not upgrade it. The original ordinary vector WAL and any
`HYANNA01` marker required by that path remain its authority. This compatibility
path does not synthesize M05 or relabel an old transaction as `HYANNA02`.

`CONSOLIDATE ANN=50` is a target-index maintenance mutation with empty key, no
expiry, and one value. New publications use the fixed 384-byte `HYANNC02` body:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANNC02` |
| 8 | 16 | index `ObjectId`, big-endian, equal to the mutation target |
| 24 | 1 | captured metadata format: `4` for M04 or `5` for M05 |
| 25 | 1 | canonical result metadata format `5` |
| 26 | 6 | reserved zero |
| 32 | 32 | captured selected base identity |
| 64 | 32 | captured complete view identity |
| 96 | 32 | canonical replacement base identity |
| 128 | 32 | captured ordered effective-record digest |
| 160 | 32 | publication-time prior view identity |
| 192 | 32 | publication-time prior overlay root |
| 224 | 32 | canonical result view identity |
| 256 | 32 | canonical result overlay root |
| 288 | 32 | canonical effective-vector-set digest |
| 320 | 8 | captured `next_sequence` |
| 328 | 8 | captured effective delta record count |
| 336 | 8 | publication-time prior `next_sequence` |
| 344 | 8 | result `next_sequence` |
| 352 | 8 | consumed captured-record count |
| 360 | 8 | preserved later-record count |
| 368 | 8 | effective live-vector count |
| 376 | 8 | reserved zero |

The target and all identity, root, and digest fields are nonzero. The captured
record count is in `1..=4,096`, consumed count does not exceed captured count,
preserved count is at most 4,096, and effective live-vector count is at most
1,000,000. Captured `next_sequence` is nonzero, publication-time prior
`next_sequence` is at least the captured value, and result `next_sequence`
equals the publication-time prior value. The captured/result format bytes are
the M04/M05 capture flag and the mandatory canonical M05 result assertion; no
other value is legal.

An M04 capture hashes the complete D01 map. An M05 capture first composes D02
over frozen D01 and hashes that complete effective map. Records are ordered by
big-endian object ID; every sequence is nonzero and below captured
`next_sequence`. The digest is:

```text
BLAKE3("hyphae-ann-consolidation-effective-records-v2" ||
       index_object_id_be128 ||
       u64le(captured_delta_count) ||
       ("hyphae-ann-consolidation-effective-record-v2" ||
        object_id_be128 || u64le(sequence) || u8(kind) ||
        u64le(mutation_csn) || upsert_payload)...)
```

Kind is `1` for upsert and `2` for tombstone. An upsert contributes exactly the
`u64le(dimension)` followed by that many canonical little-endian finite `f32`
components; a tombstone contributes no upsert payload. The nonzero mutation CSN
is the CSN in the effective D01 or D02 record. Domain separation, kind, and
dimension make the stream unambiguous without a variable WAL tail.

The duplicated target, count, and digest bind the complete ordered
effective-record capture. A prefix, duplicate, reordering, omitted tombstone,
added object, or changed sequence, kind, CSN, or vector is invalid. The M04/M05
flag fixes how the
captured view is reconstructed and must agree with the captured view identity.
`HYANNC02` ends at byte 384; no object list or other per-record tail follows the
fixed body.

The canonical effective-vector-set digest at offset 288 is independent of the
ordered-record digest. For every live effective vector, compute the record hash
and the four wrapping lane sums below; `xor32` is the bytewise XOR of all record
hashes, and `sum0` through `sum3` are the wrapping `u64` sums of their four
little-endian lanes:

```text
record_hash = BLAKE3("hyphae-ann-effective-vector-record-v1" ||
                     object_id_be128 || u64le(creating_csn) ||
                     canonical_f32_components_le)

BLAKE3("hyphae-ann-effective-vector-set-v1" ||
       index_object_id_be128 ||
       u64le(effective_live_vector_count) ||
       xor32 || u64le(sum0) || u64le(sum1) ||
       u64le(sum2) || u64le(sum3))
```

The vector component count is the catalog-bound index dimension. An empty
effective set uses zero `xor32`, zero lane sums, and zero count. This digest does
include the index in its own hash input, so an otherwise identical effective set
under another index has a different authority. The enclosing fixed body also
binds the same nonzero target at offset 8, and live publication and recovery
require both target identities to agree.

Historical fixed 112-byte `HYANNC01` remains authoritative for retained opcode
50 commits made under its original M01-through-M04 transition rules, including
a legacy consolidation that upgrades an M01, M02, or M03 source to M04. Its
target index comes from the enclosing mutation, and its frozen body remains
`magic(8) || captured_base(32) || captured_view(32) || replacement_base(32) ||
captured_count_u64le(8)`. Recovery must validate that original physical
transition rather than treating C01 as a merely readable annotation. C01 cannot
authorize an M05 source, an M05 result, or a new publication.

Every new opcode 50 commit requires `HYANNC02`. C02 accepts captured format `4`
or `5` only and result format `5` only; M01 through M03 never enter either C02
format byte. They first reach M04 through the historical path above.

Live publication and recovery first reconstruct the capture from physical D01
and D02 records below captured `next_sequence`, composing D02 over D01. That
cohort must reproduce the captured count, digest, and format-specific view
exactly; live publication additionally requires equality with the in-memory
plan's captured object/sequence map. Publication then classifies the current
effective delta. A record still at its captured sequence is consumed; a record
with a greater sequence is preserved as later; and an object absent from the
live plan's capture is preserved only with a sequence at or above the boundary.

The reconstruction requirement supplies the same-object stale rule. A later
D02 may shadow a captured D01 because that exact D01 remains physically
available below the boundary; the later D02 is preserved. Overwriting a
captured D02, or overwriting a captured D01 in its own layer, removes or changes
the below-bound capture and is stale even if the new record has a greater
sequence. Missing captured records, changed capture bytes, lower later
sequences, and unclassified records are stale or corrupt.

The result must be M05 with the replacement selected and one canonical
overlay-only delta: D01 count and bytes are zero and the D01 namespace is empty,
while every surviving later record is encoded once as D02 with object, kind,
sequence, mutation CSN, and vector components unchanged. Result
`next_sequence` equals the publication-time prior value. Frozen legacy
`next_sequence` equals the minimum preserved sequence, or that same final value
for an empty overlay. The fixed C02 body is fully encoded and shape-validated
before the unpublished B+tree tail is opened or any page is appended.

Recovery validates the transition against the immediately preceding retained
committed root, including when both roots are superseded by a later commit. It
requires unchanged lifecycle policy, the exact canonical retained-generation
transition, a canonical replacement HNSW base, exact overlay
counts/bytes/nodes/root/view identity, and logical effective-set equality. The
prior selected nonempty generation is appended at the end only when it differs
from the replacement and is not already retained; it otherwise remains at its
existing position. Only the oldest generations needed to meet policy are
removed. Every surviving selected or retained child descriptor must have its
complete vector and graph namespace, and surviving prior generation records
must be byte-identical. Missing graph layers, orphan generation records, a
descriptor-only retention claim, or an extra retained generation is invalid.

The exact-prefix B+tree transition covers only the search format key and the
target's prior/result ANN keys under `0x05` through `0x0b`; the replacement
cannot change lexical data or another ANN index. HYANNA02, opcode 57, and user
point mutations cannot be mixed into this maintenance transaction.

The deterministic strict interruption cut is exact: `BlobStaged` through
`PageSynchronized` reopen the prior committed root, while `WalAppended` through
`RootPublished` replay and reopen the complete canonical replacement. No cut
may expose the replacement base with a prior delta, a partial D02 overlay, a
renumbered sequence, or a partially updated retained-generation list.

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
4. Enforce each `BEGIN` aggregate incrementally before retaining mutation
   payloads; an overrun, underrun, zero count, or decoded-memory excess fails
   closed.
5. Reject every complete corrupt block, record, sequence, digest chain,
   transaction boundary, or content digest.
6. Ignore complete transactions without a valid commit.
7. Replay committed transactions in CSN order.
8. Verify or rebuild the committed root set before advancing visibility.

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
receipts, checkpoint replay, and bounded recovery. Opcode 57 coverage must fix
its golden body and strict shape, reject the 4,097th fence, distinguish unmarked
opcode 19 from M05 opcode 19 plus HYANNA02, prove mixed marker counts exclude
fences, reject marker substitution, and recover the exact object, generation,
and lifecycle conflict-key rules across all crash cuts. Opcode 50 coverage must
fix the 384-byte HYANNC02 golden, both index-bound captured-record and
effective-vector-set digests, and the M04/M05-capture/M05-result
format restriction; retain C01 fixtures as authority
only for their original legacy transitions; reject malformed flags, counts,
sequences, targets, and historical-version substitution; classify the captured
cohort and later objects exactly; preserve a reconstructable captured-D01/later-
D02 shadow; reject a captured-D02 overwrite; require canonical overlay-only M05
output;
preserve publication-time `next_sequence`, prove complete retained vector/graph
generations, and prove the strict old-or-complete cut at all seven boundaries.

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
