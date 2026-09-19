<!-- SPDX-License-Identifier: Apache-2.0 -->
# ANN delta overlay format v1

Status: reader-only validation contract. The native runtime can decode,
authenticate, hydrate, and query this format. No ANN format writer synthesizes
`HYANNM05`, `HYANNO01`, `HYANND02`, `HYANNN01`, or `HYANNA02`; no WAL opcode,
product API, protocol feature, catalog capability, backup/proof version, or
release claim is assigned to it. ANN mutation, initial-bulk publication, and
ANN consolidation of an M05 index fail closed. Test-only fixture encoders are
not format writers. Lexical compaction and page-generation vacuum may preserve
already authenticated bytes without becoming M05 writers.

This format layers an authenticated object-keyed overlay over an immutable
legacy `HYANND01` delta without changing any M01 through M04 byte. It is a
passive compatibility slice, not publication authority.

## Integer and identity rules

All numeric value fields are unsigned little-endian unless a table says
otherwise. Every `ObjectId` and every `ObjectId` component in a key is a
nonzero 128-bit unsigned integer in big-endian order. Hashes and build
identities are opaque 32-byte strings. A reserved or padding byte must be zero.
Every value and fixed key has exactly the stated length; truncation and trailing
bytes are corruption.

The search B+tree retains its existing format marker. Prefixes `0x00` through
`0x08` were already allocated. The first three unallocated search prefixes are
assigned only for passive M05 decoding:

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
         base_build_identity || frozen_legacy_view_identity || overlay_root ||
         u64le(legacy_count) || u64le(legacy_bytes) ||
         u64le(overlay_count) || u64le(overlay_bytes) ||
         u64le(overlay_node_count) ||
         u64le(effective_count) || u64le(effective_bytes) ||
         u64le(legacy_next_sequence) || u64le(next_sequence))
```

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

All metadata-derived allocation and physical scans are checked against the
existing 4,096-record, 64-MiB delta, page, and governor contracts. Arithmetic
overflow, impossible counts or bytes, unsupported magic, bad padding, and any
trailing data are corruption.

## Non-emission boundary

Current creation, foreground ANN mutation, initial-bulk publication, and ANN
consolidation continue to emit M04 metadata and D01 records under prefixes
`0x05` and `0x08`. M05 input is rejected independently by all three ANN write
paths, so they cannot downgrade or partially rewrite a layered authority.
Lexical search compaction and page-generation vacuum may copy already validated
ANN bytes byte-for-byte; neither synthesizes or changes the format.

`HYANNA02` is unassigned and non-emittable. WAL remains unchanged, including
the existing `HYANNA01` marker and opcode set. This reader creates no product or
protocol capability and does not expand any release or performance claim.

Normal recovery cannot generate an M05 root because no WAL mutation does so.
Tests install an already committed test-only root into page-backed root
authority, then exercise the ordinary loader and byte-preserving maintenance
primitives. They do not claim WAL publication or recovery emission.
