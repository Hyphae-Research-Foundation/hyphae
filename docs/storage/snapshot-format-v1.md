<!-- SPDX-License-Identifier: Apache-2.0 -->
# Logical snapshot formats v1 and v2

Status: normative for disk formats `1` and `2`.

A snapshot is a portable, canonical logical image at one verified log
checkpoint. It never copies Redb implementation files. All integers are
unsigned little-endian unless a field states otherwise. Every count and length
is checked before allocation.

## Common header

Both versions use a 112-byte header.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYSNAP01` |
| 8 | 2 | Disk format version (`1` or `2`) |
| 10 | 2 | Reserved flags; zero |
| 12 | 8 | Materialized commit-frame sequence, or zero |
| 20 | 32 | Commit-frame BLAKE3 digest, or all zero |
| 52 | 8 | KV entry count |
| 60 | 8 | Durable idempotency receipt count |
| 68 | 8 | Payload length |
| 76 | 4 | CRC32C of header bytes `0..76` followed by payload |
| 80 | 32 | BLAKE3 of header bytes `0..80` followed by payload |

The snapshot BLAKE3 is the logical witness digest. It is not a signature.

## Format-1 payload

Format 1 contains the KV section followed by receipts.

Each KV entry is `u32 key length || u64 value length || key || value`. Keys
are nonempty, at most 1 MiB, and strictly bytewise increasing.

Each receipt is 88 bytes:

| Size | Field |
|---:|---|
| 16 | Transaction UUID |
| 8 | Commit-frame sequence |
| 32 | Commit-frame digest |
| 32 | Canonical transaction digest |

Receipts are strictly sorted by UUID bytes and cannot reference a sequence
after the checkpoint.

## Format-2 payload

Format 2 extends the logical payload without changing the outer snapshot
framing. It encodes these canonical sections:

1. KV entries, in binary-key order.
2. Vector-space definitions, in name-byte order.
3. Vector records, in `(space name, binary key)` order.
4. Lexical-index definitions, in name-byte order.
5. Durable receipts, in UUID-byte order.

The payload carries explicit section counts. Vector identifiers, dimensions,
Q15 values, and ordering follow
[durable vector record format v1](vector-record-format-v1.md). Lexical
definitions contain their canonical index name plus exact field paths and
positive `weight_micros`; definitions and paths cannot repeat.

The snapshot metadata exposed by storage includes `entry_count`,
`vector_space_count`, `vector_count`, `lexical_index_count`, and
`receipt_count`. All five logical collections participate in the canonical
snapshot digest, backup identity, restore comparison, compaction, and derived
index rebuild.

## Creation and verification

Hyphae streams caught-up logical state into a unique temporary file, writes
and synchronizes it, verifies it from disk, and atomically renames it into
`snapshots/`. On Unix the snapshot directory is synchronized after rename. A
canonical snapshot already present for the same checkpoint is verified before
reuse.

Verification rejects unsupported versions, flags, length overflow or
mismatch, invalid or unordered logical records, duplicate identities,
trailing or truncated bytes, CRC32C mismatch, and BLAKE3 mismatch. Format 2
also rejects unknown vector/lexical tags, invalid Q15 values, dimension
mismatch, and malformed field paths.

Bounded verification preflights metadata length and declared logical counts
on the same open handle later scanned, accounts decoded logical bytes during
that scan, and checks one cooperative deadline inside record and 64 KiB
payload loops. `SnapshotReadLimits.entries` covers retained KV, vector-space,
vector, and lexical-definition records; receipts remain bounded by the file
length and deadline but are not retained by an offline witness loader.

Snapshot creation measures the complete logical payload before opening a
temporary output, applies the same record/file/decoded policy throughout
write and reread verification, and removes its unique temporary file on every
pre-promotion error. The final rename remains the activation boundary.

The checkpoint digest links logical state to a verified authoritative commit.
CRC32C and BLAKE3 detect accidental corruption; they do not authenticate a
directory against an attacker able to rewrite the snapshot and log together.
