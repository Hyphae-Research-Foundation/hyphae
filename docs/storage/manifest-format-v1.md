<!-- SPDX-License-Identifier: Apache-2.0 -->
# Storage manifest format v1

Status: normative for Hyphae `0.1.0` disk format `1`.

Storage manifests are immutable, generation-numbered commit records. The
highest canonical `manifest/*.hymanifest` filename is the active generation;
temporary files do not participate. Creating a new generation therefore does
not overwrite a live manifest and remains interruption-safe on Windows and
Unix.

All integers are unsigned little-endian. Each manifest is exactly 140 bytes.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `HYMNFST1` |
| 8 | 2 | Manifest format version (`1`) |
| 10 | 2 | Hyphae disk format version (`1`) |
| 12 | 4 | Reserved flags; must be zero |
| 16 | 8 | Manifest generation |
| 24 | 8 | Active log segment number |
| 32 | 8 | Retired-prefix checkpoint sequence, or zero |
| 40 | 32 | Retired-prefix checkpoint digest, or all zero |
| 72 | 32 | Logical snapshot digest, or all zero |
| 104 | 4 | CRC32C of bytes `0..104` |
| 108 | 32 | BLAKE3 of bytes `0..108` |

The filename is the zero-padded 20-digit generation followed by
`.hymanifest`; it must equal the generation encoded inside. Generation `1`
has active segment `1` and an empty anchor. Later generations require a
snapshot anchor and use their generation as the active segment number.

## Commit protocol

Hyphae writes a complete manifest to `tmp/`, synchronizes it, atomically
renames it to a previously unused generation filename under `manifest/`, and
synchronizes the manifest directory on Unix. A crash before rename leaves an
ignored temporary file. A crash after rename selects the new complete
generation. A corrupt committed highest generation fails loudly; Hyphae does
not silently fall back to older state.

Opening a format-1 data directory with no committed manifest performs the
idempotent bootstrap migration to generation `1`. This makes directories
created by the early phase-2 implementation forward-compatible without
inventing a legacy public disk version.
