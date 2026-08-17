<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native directory witness v2

Status: implemented G6 retained native authority

`HYNWIT02` is the complete canonical native directory authority referenced by
`HYNPRF02`. It is intentionally incompatible with `HYNWIT01` and carries
format version 2. The witness contains no origin path and remains usable after
the originating directory is deleted.

## Scope

The witness bundles every directory below the native data root, including
empty directories, and every regular file with complete bytes and a BLAKE3
digest. The root itself is implicit. Symlinks and special files are rejected.

The witness carries the same directory lineage, history epoch, visible CSN,
catalog version, root digest, checkpoint sequence, and checkpoint digest as the
proof. Those anchor fields are defined by `native-proof-v1.md` (the retained
filename is the public documentation path; the current wire contract is v2).

## Envelope

All integers are unsigned little-endian. The 64-byte header is:

| Offset | Bytes | Meaning |
| --- | ---: | --- |
| 0 | 8 | ASCII magic `HYNWIT02` |
| 8 | 2 | format version `2` |
| 10 | 2 | flags, zero in v2 |
| 12 | 1 | complete-inventory kind `1` |
| 13 | 3 | reserved zero |
| 16 | 8 | exact payload bytes |
| 24 | 4 | CRC32C over header bytes 0..32 with this field zero plus payload |
| 28 | 4 | reserved zero |
| 32 | 32 | BLAKE3 envelope digest |

The digest domain is `hyphae-native-witness-envelope-v2`. The proof references
the exact digest and complete encoded length.

## Inventory

The payload contains the fixed anchor, entry/file/directory counts, total file
bytes, and exactly that many sorted entries. Directory entries carry canonical
UTF-8 relative paths. File entries carry path, byte length, BLAKE3 digest, and
complete file bytes.

Entries are strictly sorted by unsigned UTF-8 path bytes and unique. All parent
directories must appear before descendants. Paths reject absolute forms,
drive prefixes, `.`, `..`, empty components, backslashes, NUL, and non-UTF-8
source names. Traversal uses non-following metadata and rejects symlinks,
sockets, devices, FIFOs, and other special files.

`WitnessCodecLimits` bounds the envelope, entries, files, directories, path
bytes, one file, total file bytes, and total decoded bytes before or during
allocation. Decoding verifies every count, total, file digest, envelope digest,
and canonical byte-for-byte re-encoding.

## Semantic use

Artifact decoding alone proves inventory integrity. For recognized v2
operation proofs, the offline verifier safely extracts the inventory to a new
temporary directory, opens it through the native runtime, verifies its root and
checkpoint authority, and reexecutes the operation. The verifier reports
semantic reexecution only after the canonical result, evidence, bindings, and
kind-specific metadata match.

The original directory is never consulted. Temporary extraction is removed on
success and failure. An arbitrary directory bundle paired with opaque proof
sections remains artifact integrity only.

## Verification evidence

Tests cover origin deletion, complete round trips, truncation, selected
header/file tampering, trailing bytes, witness substitution, foreign anchors,
resource limits, incorrect file digests, unsafe/duplicate/unsorted/incomplete
paths, Unix symlink rejection, retained native reopen, and semantic operation
reexecution.
