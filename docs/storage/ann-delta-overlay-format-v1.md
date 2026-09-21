<!-- SPDX-License-Identifier: Apache-2.0 -->
# ANN delta overlay format v1

Status: bounded point-mutation and validation contract. The native runtime can
decode, authenticate, hydrate, query, and publish `HYANNM05` through foreground
point upsert and present-object delete paths authenticated by internal WAL
opcode 56 and `HYANNA02`. Complete-image prior absence uses conflict-only
opcode 57 without changing this physical format. Bounded opcode 50
consolidation publishes a canonical overlay-only M05 result under `HYANNC02`.
This slice adds no M05 initial-bulk rewriting, catalog, protocol, backup/proof
version, background scheduling, or release claim. Initial-bulk publication
against M05 fails closed. Lexical compaction and page-generation vacuum may
preserve already authenticated bytes without becoming semantic M05 writers.

This format layers an authenticated object-keyed overlay over an immutable
legacy `HYANND01` delta without changing any M01 through M04 byte. It is a
bounded compatibility and publication slice.

## Integer and identity rules

All numeric value fields are unsigned little-endian unless a table says
otherwise. Every `ObjectId` and every `ObjectId` component in a key is a
nonzero 128-bit unsigned integer in big-endian order. Hashes and build
identities are opaque 32-byte strings. A reserved or padding byte must be zero.
Every value and fixed key has exactly the stated length; truncation and trailing
bytes are corruption.

The search B+tree retains its existing format marker. Prefixes `0x00` through
`0x08` were already allocated. The first three unallocated search prefixes are
assigned for M05 decoding and physical point publication:

| Prefix | Canonical key | Value |
|---:|---|---|
| `0x09` | prefix + index `ObjectId` | the one `HYANNO01` manifest |
| `0x0a` | prefix + index `ObjectId` + object `ObjectId` | one `HYANND02` overlay leaf |
| `0x0b` | prefix + index `ObjectId` + depth `u8` + 16-byte path | one `HYANNN01` internal node |

An M05 index has exactly one `0x09` key. M01 through M04 indexes must have no
key in any of these three namespaces. A malformed key, an overlay key owned by
an absent index, or an overlay key owned by non-M05 metadata is corruption.

## M05 metadata

The M05 value starts with this fixed 280-byte header. Child and retained
generation descriptors follow it in the exact M04 order and encoding. Thus M05
does not reinterpret an M04 descriptor, and M01 through M04 sizes and bytes do
not change.

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANNM05` |
| 8 | 32 | selected base build identity |
| 40 | 32 | final layered view identity |
| 72 | 32 | partition input identity, or all zero for a single base |
| 104 | 8 | base vector count |
| 112 | 8 | base graph-node count |
| 120 | 8 | frozen legacy D01 record count |
| 128 | 8 | frozen legacy D01 value bytes |
| 136 | 8 | final `next_sequence` |
| 144 | 4 | `delta_max_entries` |
| 148 | 2 | `consolidate_after_deltas` |
| 150 | 2 | `retain_generations` |
| 152 | 1 | base kind: `1` single, `2` partitioned |
| 153 | 1 | reserved |
| 154 | 2 | selected child count |
| 156 | 2 | retained generation count |
| 158 | 2 | reserved |
| 160 | 32 | frozen legacy view identity |
| 192 | 32 | overlay sparse-Merkle root |
| 224 | 8 | overlay leaf count |
| 232 | 8 | overlay leaf value bytes |
| 240 | 8 | persisted overlay internal-node count |
| 248 | 8 | effective layered delta record count |
| 256 | 8 | effective layered delta value bytes |
| 264 | 8 | frozen legacy `next_sequence` |
| 272 | 8 | reserved |

The selected child count is nonzero. Starting at offset 280, each selected
child is the existing 72-byte M04 descriptor. Each retained generation then
uses the existing 40-byte M04 retained header followed by its existing 72-byte
child descriptors. The descriptor counts must consume the value exactly.

The lifecycle remains valid under the existing contract. Frozen legacy,
overlay, and effective record counts are each at most `delta_max_entries`,
which is at most 4,096. Each corresponding byte count is at most 64 MiB. A
nonzero count consumes at least 40 bytes per record. The effective count is the
cardinality of the union of legacy and overlay object keys, so it is between
the greater input count and their checked sum.

## Fixed manifest

`HYANNO01` is exactly 184 bytes and duplicates the complete layered authority
needed to detect substitution between metadata, manifest, leaves, and nodes.

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANNO01` |
| 8 | 32 | frozen legacy view identity |
| 40 | 32 | overlay root |
| 72 | 32 | final view identity |
| 104 | 8 | frozen legacy record count |
| 112 | 8 | frozen legacy value bytes |
| 120 | 8 | overlay leaf count |
| 128 | 8 | overlay leaf value bytes |
| 136 | 8 | overlay internal-node count |
| 144 | 8 | effective layered record count |
| 152 | 8 | effective layered value bytes |
| 160 | 8 | frozen legacy `next_sequence` |
| 168 | 8 | final `next_sequence` |
| 176 | 8 | reserved |

Every duplicated field must equal M05 exactly. The three identities in the
manifest are nonzero.

## Overlay leaves

An overlay leaf value has the D01 field layout but a distinct magic. It is
exactly 40 bytes for a tombstone and `40 + 4 * dimension` bytes for an upsert.

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANND02` |
| 8 | 1 | kind: `1` upsert, `2` tombstone |
| 9 | 7 | reserved |
| 16 | 8 | mutation sequence |
| 24 | 8 | nonzero mutation CSN |
| 32 | 2 | index dimension for an upsert, zero for a tombstone |
| 34 | 6 | reserved |
| 40 | variable | canonical little-endian finite `f32` components for an upsert |

Sequence zero is invalid. A tombstone has no payload. An upsert has exactly the
catalogued dimension, satisfies the existing metric admission rules, and has
no trailing component bytes.

All live D01 records have distinct sequences below frozen
`legacy_next_sequence`. All D02 records have distinct sequences greater than or
equal to frozen `legacy_next_sequence` and below final `next_sequence`.
Sequences are also distinct across the two layers. If the overlay is empty,
the two next-sequence values are equal. If it is nonempty, final
`next_sequence` is greater.

## Sparse Merkle tree

The tree is a fixed-depth 32-level radix-16 sparse Merkle tree over the 32 high
to low nibbles of the 128-bit object ID. A nonempty leaf persists every one of
its reachable internal ancestors at depths 31 through 0. Shared ancestors are
stored once. Empty leaves and empty internal nodes are not stored.

The fixed 34-byte internal-node key is:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 1 | `0x0b` |
| 1 | 16 | index `ObjectId`, big-endian |
| 17 | 1 | depth, 0 through 31 |
| 18 | 16 | path, big-endian |

The path retains exactly its first `depth` high nibbles. Every lower nibble is
zero, including all 32 nibbles at depth zero. This canonical padding prevents
aliases for one logical node.

The variable node value has a 56-byte header:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANNN01` |
| 8 | 1 | depth, equal to the key depth |
| 9 | 1 | present-child count, 1 through 16 |
| 10 | 6 | reserved |
| 16 | 2 | child bitmap, little-endian |
| 18 | 6 | reserved |
| 24 | 32 | node hash |
| 56 | `32 * count` | present child hashes in increasing child position |

The child count equals the bitmap population count. A set bitmap bit consumes
one hash at that position. An absent bit consumes no encoded hash. Encoding an
explicit child whose hash equals the depth-specific empty hash is corruption.
Missing, duplicate, unreachable, or additional nodes are corruption.

An empty overlay has no node record and its root is `empty_hash(0)`. A nonempty
overlay has at least 32 nodes and at most `32 * overlay_leaf_count`, with the
absolute bound 131,072. Validation recomputes every reachable level. The
index-scoped reader scans persisted nodes under bounded physical limits and
keeps only one leaf-sized frontier per level; it does not collect the possible
131,072 nodes into an in-memory node map.

## Hash definitions

Each expression is ordinary BLAKE3 with 32-byte output. Domain strings are the
listed ASCII bytes with no terminator. `u64le`, `u16le`, and `u8` denote fixed
width encodings. Concatenation is denoted by `||`.

```text
empty_hash(depth) =
  BLAKE3("hyphae-ann-overlay-empty-v1" || u8(depth))

leaf_hash(object, encoded_D02) =
  BLAKE3("hyphae-ann-overlay-leaf-v1" ||
         object_be128 || u64le(len(encoded_D02)) || encoded_D02)

node_hash(depth, bitmap, children) =
  BLAKE3("hyphae-ann-overlay-node-v1" || u8(depth) || u16le(bitmap) ||
         child_hash[0] || ... || child_hash[15])
```

For `node_hash`, a present position uses its encoded child hash and an absent
position uses `empty_hash(depth + 1)`. Positions are always hashed 0 through
15. The node's encoded hash must equal this result.

The final view hash uses manifest fields but not the manifest's final-view
field, avoiding a circular definition:

```text
view_identity =
  BLAKE3("hyphae-ann-overlay-view-v1" ||
         u128be(index_object_id) || base_build_identity ||
         frozen_legacy_view_identity || overlay_root ||
         u64le(legacy_count) || u64le(legacy_bytes) ||
         u64le(overlay_count) || u64le(overlay_bytes) ||
         u64le(overlay_node_count) ||
         u64le(effective_count) || u64le(effective_bytes) ||
         u64le(legacy_next_sequence) || u64le(next_sequence))
```

The index `ObjectId` is encoded as its unsigned 128-bit big-endian value. Thus,
identical base and overlay bytes in different indexes have different final view
identities.

The frozen legacy identity is exactly piecewise under the unchanged M02 through
M04 rule. If the D01 map is empty, the identity is the base build identity for
every nonzero frozen next sequence. If D01 is nonempty, it is the existing
`hyphae-ann-base-delta-view-v1` BLAKE3 identity over the selected base, frozen
next sequence, and complete object-ordered D01 map. This preserves historical
identity semantics instead of silently redefining D01.

XOR, modular addition, subtraction, counters without ordered object binding,
and other additive or commutative accumulators are explicitly invalid. They do
not prove object position, permit cancellation and duplicate ambiguity, and
cannot replace any leaf, node, root, frozen-view, or final-view check above.

## Layer validation and hydration

Validation is fail closed and complete:

1. Decode one M05 and exactly one manifest; require every duplicate field to
   agree.
2. Restore and validate the selected base and retained generations under the
   unchanged M04 rules.
3. Decode every D01 record, check its count and bytes, check sequence uniqueness
   and range, and reproduce the frozen legacy identity.
4. Decode every D02 leaf, check its count and bytes, and check sequence
   uniqueness and range across both layers.
5. Recompute leaf hashes, every reachable internal node, exact node count, and
   the root. Reject explicit empties and any missing or unreachable path.
6. Compose the effective object-keyed delta by taking D02 first and D01 second;
   use the selected base only when neither layer names the object. An overlay
   tombstone suppresses both lower layers.
7. Check effective record count and bytes, final `next_sequence`, and final view
   identity before returning a readable state.

Exact hydration and ANN hydration share that composed map. Exact search skips
every base object named by the composed delta. ANN search suppresses the same
base objects, exact-scores live composed upserts, and never revives an object
hidden by an overlay tombstone.

The complete-root loader first uses a borrowed B+tree range visit over
`[0x05,0x06)` for M05 metadata and computes the same conservative per-index
hydration estimate used by governed index hydration:
2 MiB fixed per index, restored vector/graph estimates, twice the D01 bytes,
twice the D02 bytes, and 256 bytes per overlay leaf. The checked aggregate for
every ANN index sharing an M05 root must not exceed the 64 MiB recovery/product
authority. Rejection occurs before any physical ANN value is decoded.

After admission, one borrowed B+tree range visit over `[0x06,0x0c)` streams
every physical ANN entry once. The borrowed leaf decoder validates canonical
page bytes and passes key/value slices without creating key/value vectors.
Aggregate and per-index namespace entry/byte limits, plus the fixed maximum
encoded size for vectors, graph layers, D01, D02, and nodes, are checked before
decoding or copying each value. M05 nodes validate top-down in persisted
`(index, depth, path)` order:
one depth authenticates the expected hashes for the next depth, and depth 31
binds directly to the previously visited D02 leaf hashes. Retained
authentication state is at most 4,096 leaf hashes plus one 4,096-node expected
frontier; no encoded global entry vector or persisted node map is retained.
Total work is linear in physical entries with at most the fixed 32-level path
work. An oversized D02 value is rejected with no vector allocation and no D02
decoder invocation. M01 through M04 roots without M05 retain their historical
load path and acceptance behavior.

That historical acceptance is an explicit grandfathering boundary: an
M01-M04-only root may open, pin, back up, restore, and recover even when its
aggregate lexical-plus-ANN hydration estimate exceeds the M05 shared 64 MiB
authority. Per-index metadata and physical namespace limits still apply. An
M04 initial-bulk publication remains M04 and does not activate the aggregate
cap. The first physical point or C02 consolidation candidate that would emit
M05 substitutes its projected target charge into the complete root, activates
the cap, and rejects before page or WAL publication if the candidate does not
fit.

All metadata-derived allocation and physical scans are checked against the
existing 4,096-record, 64-MiB delta, page, and governor contracts. Arithmetic
overflow, impossible counts or bytes, unsupported magic, bad padding, and any
trailing data are corruption.

## Point mutation and absence-fence writer

Index creation and vectors in the same transaction retain the canonical M04
base path. A later ordinary or physical-delta vector upsert, or deletion of a
present vector, uses the same M05 point writer. The first physical write freezes
the admitted M04 D01 count, bytes, `next_sequence`, and view identity as
immutable legacy without scanning, copying, or rewriting the D01 namespace.
Empty D01 freezes canonically with the base identity. Existing M05 writes
replace or add one D02 leaf and its 32 ancestors, then replace the manifest and
metadata. A delete leaf is the 40-byte D02 tombstone. Physical points do not
rewrite a base, D01 record, unrelated overlay leaf, or unrelated sparse path.

Staging retains one bounded intent and one exact authenticated path per physical
object: the exact D01 and D02 values, if present, and at most 32 encoded nodes.
Sequence and mutation CSN are assigned only after writer admission. Publication
checks the generation and frozen-legacy authority again, permits rebasing over
disjoint point writes, and rejects same-object or changed-generation histories.
The update computes overlay and effective counts/bytes with checked arithmetic;
the complete projected root must remain within the existing 4,096-record,
64-MiB delta, 131,072-node, mutation-memory, and recovery-memory limits.
Complete-root validation caches the exact reopen charge for each ANN index.
Writer admission remeasures lexical retention with the same allocation-free
borrowed `[0x00,0x05)` visit used by reopen accounting, subtracts it from the
shared 64-MiB authority, replaces only touched-index ANN charges, and inherits
unchanged index charges from the validated source root. It performs no
root-wide ANN metadata or catalog-definition scan and no D01 prefix scan.
Group admission carries each accepted point publication's projected metadata,
manifest, leaf, and sparse path into the next member, so same-index members are
charged and planned in commit order rather than repeatedly against the cohort's
base root. Materialized legacy charges likewise come from the sequential
candidate state. Initial-bulk and consolidation publication compute the exact
candidate metadata charge, substitute it into the same lexical-plus-ANN
authority, and reject an over-limit candidate before creating a page.

Upsert and physical-delete accounting includes both retained state and the
scratch needed after scheduler dequeue. Let `V = 4 * dimension` and let one
maximum encoded sparse-path key/value pair be
`34 + 56 + 16 * 32 = 602` bytes. One upsert intent conservatively charges

```text
2 * V + 4 * 32 * 602 + 4,096 +
BTree::sorted_batch_structural_memory_bound(35)
```

The terms cover the staged and encoded vector, four path/publication copies,
fixed point state, and the largest 35-entry metadata/manifest/leaf/path B+tree
batch structure.

A physical delete uses its observed authenticated path rather than the maximum
value size. It charges the exact capacities of its captured D01 and D02 values
and 32 optional encoded node values; the path container, option slots, point
intent, allocation bookkeeping, and fixed 256-byte structural allowance; the
largest simultaneously live delta/vector, selected-base lookup, node/hash, and
path-index proof workspace; and
`BTree::sorted_batch_structural_memory_bound(35)`. The first physical intent on
an index additionally charges the 2,097,152-byte recovery allowance. Per-index
metadata authority remains `2 * encoded_metadata_length + 4,096` bytes.

A conflict-only fence retains no physical path or publication replacement; its
bounded conflict/proof plan is 256 bytes. The ordinary opcode 18, 19, or 57
mutation key/value capacities, mutation overhead, and mutation-vector growth
are charged separately. Every capacity and structural term uses checked
arithmetic. Replaying the same inputs must reproduce the stored retained ledger
exactly.

Before scheduler enqueue, the memory-only queue request is exactly
`max(1, checked_add(replayed_retained_ledger, wal_publication_peak))` with zero
compute and I/O tokens. The replayed ledger contains the complete upsert and
delete proof/publication scratch above, plus physical paths, recovery authority,
fences, ordinary mutation capacity, and all other batch-retained state. The
separately preflighted WAL peak covers record/encoded-vector slots and cloned
key/value payloads that are simultaneously live with that retained state; it is
additive, not a replacement or maximum. Commit replays both values and
subdivides WAL memory from the combined permit. No second point scratch permit
is acquired after dequeue. Queue admission and commit reject a mismatch before
page or WAL publication.

The physical delta API rejects a second staged operation for the same
`(index, object)` without losing its first operation. The materialized API,
which permits repeated private upserts/deletes, assigns one sequence per
physical operation and publishes the final leaf, giving last-write semantics
and legal unused sequence positions below final `next_sequence`. Duplicate-free
`upsert_vectors_with_progress` batches on M05 are planned atomically through the
same point publications; build progress remains exclusive to same-transaction
base creation.

Complete-image omission first executes the same exact object lookup. If the
effective object is live, opcode 19 is a physical delete and publishes the D02
tombstone above. If it is absent, opcode 57 is a conflict-only fence. Prior
absence is proved by authenticating the exact D02 leaf or non-membership path
against the current overlay root, then decoding the exact D01 record, then
checking every selected base-child vector key only when neither delta layer
names the object. D02 and D01 tombstones suppress lower values; duplicate live
base ownership is corruption. Admission re-runs that lookup against the
admitted root. Opcode 57 against a live object fails closed rather than being
silently converted during replay.

The conflict-only branch publishes no key in prefixes `0x05` through `0x0b`.
It leaves the base and final view identities, manifest, overlay root, counts,
bytes, `next_sequence`, and all leaf/node encodings unchanged, and consumes no
delta slot or legal-unused sequence position. It does consume one WAL mutation.
A transaction admits at most 4,096 opcode 57 records in total, separately from
the physical operation count.

## M05 consolidation writer

New bounded foreground consolidation captures M04 or M05. Its immutable plan
captures the source-version flag, selected base and view identities, captured
`next_sequence`, the complete effective vector set used for the replacement
build, and the complete effective delta. M04 uses its D01 map; M05 composes D02
over frozen D01 first. The effective records are ordered by object ID and
bounded by 4,096. The fixed 384-byte `HYANNC02` opcode 50 body binds their count
and domain-separated digest, including index, object, sequence, kind, mutation
CSN, and canonical vector components, without appending a per-record tail.
[Native WAL format v1](../native/wal-format-v1.md) fixes those bytes. Historical
112-byte `HYANNC01` remains recovery authority for retained commits under its
original M01-through-M04 transition rules, including legacy M01-M03
consolidation upgrades to M04, and is never emitted by this writer. New C02
captures are M04 or M05 only and their result is M05 only. The C02 captured
record digest includes the target index. The canonical effective-vector-set
digest independently includes that index before its count and commutative
accumulators. The same fixed body duplicates the target, and publication and
recovery require both index authorities to agree. The WAL contract fixes both
formulas.

Publication re-reads the complete target at the admitted current root. The
captured base and definition must still match. It reconstructs physical D01 and
D02 records below captured `next_sequence`, composes their effective map, and
requires that cohort to reproduce the captured object/sequence map, count,
ordered effective-record digest, and format-specific view exactly. A current
effective record still at its captured sequence is consumed into the
replacement. A greater-sequence record is preserved as later; an object absent
from the capture is preserved only at or above the boundary.

A later D02 may therefore shadow a captured D01: the old D01 still proves the
capture, and the later D02 survives. A captured D02 overwrite, or a captured
D01 overwrite in D01, removes or changes the below-bound proof and makes the
plan stale even when the new sequence is greater. Missing captured records,
changed kind/CSN/vector, lower later sequences, or any unclassified record
reject before page or WAL publication. Consolidation assigns no sequence:
result `next_sequence` is exactly the publication-time prior value.

The result always has this one canonical M05 shape:

- the replacement immutable base is selected;
- D01 count and bytes are zero and the D01 namespace is empty;
- frozen legacy view identity equals the replacement base identity;
- every preserved later record is encoded once as D02, retaining its object,
  kind, sequence, mutation CSN, and vector components;
- frozen legacy `next_sequence` is the minimum preserved sequence, or final
  `next_sequence` when no record remains;
- overlay count/bytes equal effective count/bytes; and
- the manifest, complete ordered sparse-Merkle tree, root, and final view
  identity are regenerated from exactly those D02 records.

The publication-time retained-generation list remains in order. The descriptor
whose identity equals the replacement is first removed, promoting that
generation to selected while preserving the order of all other retained
descriptors. The prior selected nonempty base is then appended at the end only
when it differs from the replacement and is not already retained. Oldest
entries are removed only to satisfy `retain_generations`. The resulting list is
ordered and unique and never contains the selected identity. Each surviving
selected or retained descriptor must own the complete vector and graph records
for every child. Existing surviving generation records, including every graph
layer, are preserved byte-for-byte; retired generations and only retired
generations are removed.

Publication admission covers every simultaneously live owner additively:
target-index hydration and validation, the replacement base when its build
permit is not already retained, the exact-prefix structural plan, the
caller-owned replacement payload, and the WAL encoder/cloned-payload peak. The
B+tree structural bound explicitly excludes payload ownership, so the ANN
writer separately charges the retained capacities of every replacement key
clone and value clone that it passes to the prefix rewrite, including any
simultaneously live source replacement set. No term may replace another through
a maximum-of calculation, and no key or value clone may be materialized before
that combined checked admission. An overflow, understated clone/WAL charge, or
candidate beyond the admitted peak rejects before the unpublished tail is
opened.

The complete fixed C02 value is encoded and shape-validated before one
exact-prefix B+tree replacement opens its unpublished tail. The replacement
covers the format key and all target ANN prefixes `0x05` through `0x0b`. It
compares the complete current key set before appending a page, builds into the
unpublished tail, validates the full target, every selected/retained generation
and graph, and the canonical M05 shape there, and rolls the tail back on any
mismatch.
Under strict interruption, cuts through `PageSynchronized` reopen the complete
prior root and cuts from `WalAppended` through `RootPublished` reopen the
complete replacement. Recovery independently reconstructs the capture
classification from each transition's immediately preceding retained committed
root and HYANNC02, even when that result root is later superseded. It then
requires the exact replacement base, canonical overlay-only later-record set,
unchanged `next_sequence`, lifecycle policy, exact retention transition,
complete generation records, and logical effective set.

## WAL authority

Internal append-only opcode 56 is search-engine mutation `HYANNA02`. Its key is
empty, expiry is absent, and its target is the nonzero index duplicated in this
fixed 184-byte authority body:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | ASCII `HYANNA02` |
| 8 | 1 | flags: bit 0 is exact M04-to-M05 transition; all other bits zero |
| 9 | 7 | reserved |
| 16 | 16 | index `ObjectId`, big-endian, equal to mutation target |
| 32 | 4 | operation count, 1 through 4,096 |
| 36 | 4 | reserved |
| 40 | 32 | admitted prior view identity |
| 72 | 32 | committed result view identity |
| 104 | 32 | admitted prior overlay root |
| 136 | 32 | committed result overlay root |
| 168 | 8 | admitted prior `next_sequence` |
| 176 | 8 | committed result `next_sequence` |

Every identity is nonzero, prior and result view/root identities differ, and
result `next_sequence` is prior `next_sequence + operation_count` exactly.
Markers are unique, ascending, and trailing. They cover exactly every
physically changed point-writer index and count every ordinary vector upsert and
physical vector delete for that index. Opcode 57 is excluded from the marker
set, operation count, and sequence advance. A mixed index has one marker for
only its physical members; a fence-only index has none. Indexes created in the
same transaction remain outside that marker set. `HYANNA01` and `HYANNA02` may
not coexist in one transaction. Historical unmarked opcode 19 and HYANNA01
transactions retain their existing decode and conflict semantics.

Live and recovered HYANNA02 admission validates an object conflict key plus the
index generation key, but publishes only the object key. Thus stale writes to
the same object conflict while disjoint writes from one snapshot may rebase.
Opcode 57 validates and publishes its object and index lifecycle-fence keys but
does not publish the generation key. This conflicts with same-object physical
points and stale initial-bulk/consolidation replacement while permitting
disjoint physical points and fences to compose. Creation, initial-bulk,
consolidation, and historical base-replacement paths publish the generation
key, so a stale physical point batch cannot cross them. Recovery
replays every retained HYANNA02 transition from its committed prior root,
reconstructs the exact leaf/path/manifest/metadata bytes at the commit CSN,
checks the prior/result authority body, and then reconstructs those same live
conflict keys. It first streams a bounded immutable-root comparison over all
seven ANN namespaces `[0x05,0x0c)`, independently of WAL target claims, and
extracts the index from every changed key. Metadata on both sides supplies
finite changed-key, changed-index, and node-work bounds; excess or orphan keys
fail closed. For every point-writer-changed index, either-side M05 metadata
requires exact HYANNA02 and vector-mutation agreement. Opcode 50 replacement
instead requires one exact HYANNC02 and the consolidation proof above. The
selected authority set must equal the physically changed M05 target set
exactly, so metadata-stable leaf, node, manifest, or base changes,
omitted/extra targets, stripped authorities, and HYANNA01 downgrades fail. An
absent prior root is compared as an empty tree: any result M05 physical state
requires authority and cannot be introduced as an unproved first commit, while
writer-created M01-M04 roots retain their historical validity. Recovery
also takes opcode 57 targets from WAL, proves their prior absence under the
same D02/D01/base order and authenticated path, requires no physical difference
attributable to each fenced object/path beyond co-committed physical point
mutations, and reconstructs the object and lifecycle-fence conflict keys.

M01 through M03 are not read-only compatibility formats. Their frozen legacy
ordinary mutation path remains writable and atomically emits M04 metadata on
the first accepted vector write; reads and rejected writes preserve the old
bytes. A retained C01 consolidation remains authoritative under its original
rules. Neither path emits M05, and C02 cannot name M01, M02, or M03 as its
captured or result format.

The physical transition proof compares immutable B+tree roots over all seven
target-index ANN prefixes. Equal page IDs are inherited without decoding;
changed copy-on-write paths are partitioned by both roots' separators through
splits and height changes. The bounded leaf difference set must equal exactly
the planned metadata, manifest, D02 leaf, and node replacements. An unexpected
base, D01, overlay, or intermediate key fails even when the declared result
metadata and Merkle root are otherwise self-consistent.
Every retained transition is checked, including a bad superseded root followed
by a later byte-for-byte repair. A lexical-only root change beside shared,
unchanged M05 pages produces no ANN target and remains valid.

## Remaining boundary

M05 initial-bulk rewriting and background scheduling are not part of these
writers. M05 input remains rejected by initial-bulk replacement. Conflict-only
absence fencing is not a new physical M05 record kind. Lexical search
compaction and page-generation vacuum may copy already validated ANN bytes
byte-for-byte; neither synthesizes or changes an overlay. No product protocol
feature, catalog capability, backup/proof version, or broader performance claim
is added by this slice.
