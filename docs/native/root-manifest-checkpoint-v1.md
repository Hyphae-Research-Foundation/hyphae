<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native root manifest and checkpoint format v1

Status: normative experimental format; immutable manifest publication,
digest-chain recovery, WAL checkpoint records, cross-validation, temporary
stage recovery, page-generation V2 metadata, and deterministic interruption
tests are implemented; `HYWAR002`-anchored retained-chain recovery,
manifest-prefix retirement, lineage-bearing `HYROOT03` publication, and
native-marker lineage validation are also implemented

The WAL remains transaction authority. A root manifest is an immutable,
content-authenticated snapshot of one already committed all-engine `RootSet`.
A standalone WAL `CHECKPOINT` record makes that manifest an authoritative
recovery anchor. A published manifest without such a record is verified but
reported as an unanchored suffix.

## Manifest header

Manifest files live under `roots/` and use a 176-byte header.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYROOT01` |
| 8 | 2 | format version `1` |
| 10 | 2 | header length `176` |
| 12 | 4 | flags/reserved zero |
| 16 | 8 | nonzero manifest generation |
| 24 | 8 | visible CSN |
| 32 | 8 | catalog version |
| 40 | 8 | committed WAL LSN |
| 48 | 8 | blob generation |
| 56 | 4 | root-entry count |
| 60 | 4 | root payload length |
| 64 | 4 | CRC32C |
| 68 | 12 | reserved zero |
| 80 | 32 | digest of the WAL block containing the committed LSN |
| 112 | 32 | previous manifest digest; zero only at generation one |
| 144 | 32 | complete manifest BLAKE3 digest |
| 176 | variable | canonical root entries |

The CRC32C is calculated with the checksum and final manifest digest zeroed.
The BLAKE3 digest includes the checksum and is calculated with only its own
field zeroed.

Each 12-byte root entry is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | engine kind |
| 1 | 1 | reserved zero |
| 2 | 2 | partition ID |
| 4 | 8 | nonzero page ID |

Root slots are strictly ordered by `(engine, partition)` and cannot repeat.
The current vertical publishes catalog, relational, structure, and search
roots; the format permits up to 4,096 slots.

## Lineage-bearing manifest v3

Every manifest created under a native `FORMAT` marker uses `HYROOT03`.
The 216-byte header preserves the `HYROOT02` storage-state fields and adds
the exact marker lineage:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYROOT03` |
| 8 | 2 | format version `3` |
| 10 | 2 | header length `216` |
| 12 | 4 | reserved zero |
| 16 | 48 | generation, CSN, catalog, WAL LSN, blob generation, root counts |
| 64 | 4 | CRC32C |
| 68 | 12 | reserved zero |
| 80 | 32 | committed WAL block digest |
| 112 | 32 | previous manifest digest |
| 144 | 32 | complete manifest digest |
| 176 | 8 | page generation |
| 184 | 8 | retention-floor CSN |
| 192 | 16 | directory UUIDv7 in network byte order |
| 208 | 8 | nonzero history epoch, little-endian |
| 216 | variable | canonical root entries |

The checksum and digest rules are unchanged and therefore cover the complete
lineage. Every manifest in one chain has the same lineage identity. Recovery
under a native marker requires that identity to equal `FORMAT` before any
manifest can become an authority.

`HYROOT01` and `HYROOT02` remain byte-identical and decodeable for historical
compatibility. They have no lineage and are rejected as authority inside a
native-marked directory. Generation-one/page-generation-one manifests do not
use the legacy `HYROOT01` shortcut once a native marker exists.

## Immutable publication

Generation `N` is staged with create-new semantics as
`roots/manifest-NNNNNNNNNNNNNNNN.tmp`. The file is written completely and
synchronized before same-directory rename to the `.hyroot` suffix.

On Unix, publication also synchronizes the roots directory. Safe Rust does not
currently provide the equivalent directory flush used here on Windows, so a
Windows strict power-loss claim remains gated. Recovery removes only canonical
temporary manifest names after the complete final manifest chain verifies.
Unexpected entries fail closed.

Generations start at one and are contiguous. Each generation binds the digest
of its predecessor. Complete-file corruption, a gap, a case-variant filename,
or a digest divergence fails open rather than being skipped.

## WAL checkpoint body

The standalone kernel `CHECKPOINT` body is exactly 64 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYCHK001` |
| 8 | 8 | visible committed CSN |
| 16 | 8 | manifest generation |
| 24 | 32 | manifest digest |
| 56 | 8 | prior checkpoint record LSN; zero for the first |

Checkpoint records occur outside a user transaction and use their own nonzero
transaction identity. Generations strictly increase, visible CSNs never move
backward, and prior-checkpoint LSNs form an exact chain.

During open, Hyphae verifies that every checkpoint:

1. references a present manifest with the same generation, CSN, and digest;
2. references a CSN committed earlier in the verified WAL;
3. reconstructs exactly the same catalog version, WAL anchor, blob generation,
   and engine root map as that WAL commit; and
4. retains valid reachable pages for every committed root, including roots
   superseded by later commits.

Without a retention anchor, recovery scans the complete WAL. With a verified
`HYWAR002` anchor, current-root retention and bounded suffix replay are
implemented. The same anchor selects one exact immutable manifest generation
and digest as the retained-chain trust root, so recovery no longer reads
retired lower generations. The identity-preserving publication, partial
cleanup, failure, and evidence rules are fixed separately by [Native manifest
retention v1](manifest-retention-v1.md).

The standalone codec still decodes `HYWAR001` for historical inspection.
Native-marker recovery rejects it before using any compacted-history state.

## Interruption states

Deterministic tests reopen after:

- synchronized temporary manifest creation;
- immutable manifest publication without a WAL anchor;
- checkpoint-record append before explicit WAL synchronization; and
- checkpoint-record synchronization.

The first state removes one temporary stage. The second reports one unanchored
manifest suffix. The latter two recover the checkpoint. In addition to the
deterministic in-process tests, the direct-Linux process-crash harness now
holds the writer lock until the parent sends `SIGKILL` at each of these four
boundaries, then verifies the exact manifest/checkpoint authority counts and
complete all-engine CSN after reopen.

The separate [block-layer power-loss replay
gate](block-power-loss-replay-v1.md) records writes below fresh ext4
filesystems with `dm-log-writes` and reconstructs only the stable order through
each interruption mark. In that lane an appended but unsynchronized
checkpoint WAL record is absent, leaving the published manifest as an
unanchored suffix; the synchronized checkpoint becomes authority. Literal
EBS/host power removal, device-firmware caches, and torn completed sectors
remain outside the evidence.
