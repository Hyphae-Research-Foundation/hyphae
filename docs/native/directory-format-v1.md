# Native directory format v1

Status: partially implemented by the experimental native runtime. Canonical
`FORMAT` creation and validation, lifetime-held `LOCK` ownership, and lineage
threading through `HYROOT03` manifests and `HYWAR002` retention anchors are
implemented. Offline promotion remains unimplemented. ADR-0021 and ADR-0022
govern this contract. G1 remains open

This contract fixes the root-level identity of one native data directory:
the `FORMAT` marker required by
[ADR-0021](../adr/0021-native-cutover-and-format-evolution.md), the `LOCK`
writer-exclusion file, the lineage identity required by
[ADR-0022](../adr/0022-cloud-ready-local-primitives.md), and the promotion
marker used by the offline format-2 to native migration. Every other native
file family (`HYWAL001`, `HYROOT03`, `HYWAR002`, pages, blobs, segments) lives
inside a directory governed by this marker. Historical `HYROOT01`,
`HYROOT02`, and `HYWAR001` remain decodeable but are not native-marker
authority.

## Scope

This contract governs the target native data directory described by the
[native architecture](../architecture/native-local-ecosystem.md): `FORMAT`,
`LOCK`, `manifest/`, `wal/`, `pages/`, `blobs/`, `segments/`, `snapshots/`,
and `tmp/`.

The current experimental convergence layout, which materializes
`pages.hydb`, `wal.hywal`, `roots/`, and `blobs/`, now creates and validates
the canonical marker and enforces single-writer ownership. It remains
implementation evidence only: the layout does not satisfy the complete
contract until offline promotion and lineage threading are implemented and
verified. No directory without a valid marker may be promoted to contract
status.

Disk format 2 directories remain governed by their own `FORMAT` marker and
are never opened, converted, or rewritten by the native runtime.

## FORMAT marker file

A native data directory contains a `FORMAT` file at its root. The file
holds exactly one ASCII line terminated by a single LF:

```text
hyphae-native-format=1 directory=<uuid-v7-lowercase-hex> epoch=<decimal-u64>
```

| Field | Canonical form |
|---|---|
| format version | literal `hyphae-native-format=1` |
| directory identifier | `directory=` plus one UUIDv7 in lowercase hyphenated hex (36 characters) |
| history epoch | `epoch=` plus a decimal `u64` without leading zeros; zero is invalid |

Fields appear in exactly this order, separated by single ASCII spaces. No
other bytes precede the version field or follow the LF. A reader consumes
at most 128 bytes; longer content fails closed.

The `hyphae-native-format=` prefix is deliberately distinct from the disk
format 2 prefix `hyphae-disk-format=`, so no version renumbering of either
family can make one marker parse as the other.

The pair (directory identifier, history epoch) is the lineage identity
required by ADR-0022 decision 1. The identifier is generated once when the
directory is created and never changes. The epoch starts at one and
increments only when a sanctioned history-destructive operation, for
example a retention operation that truncates prefixes or a restore from a
snapshot, creates a divergent history. No other operation may change the
epoch, and the epoch never decreases.

Opening fails closed when:

- `FORMAT` is missing while other directory content exists;
- the format version is unknown to the opening binary;
- the line is malformed, truncated, not LF-terminated, or followed by
  additional bytes;
- a field is missing, duplicated, out of order, or noncanonical; or
- format-2 and native families are mixed in one directory.

If `FORMAT` begins with the `hyphae-disk-format=` prefix, the native
runtime reports an explicit format-2 directory error. It never converts,
upgrades, or reinterprets the directory in place.

## LOCK and single-writer ownership

A native data directory contains a `LOCK` file at its root. Opening the
directory acquires an exclusive advisory operating-system lock on `LOCK`
and holds it for the entire lifetime of the handle. Exactly one writer may
own a directory at a time.

A lock that is already held by another process fails the open with an
explicit already-locked error. A lock that is lost or cannot be acquired
fails closed; the runtime never proceeds with a downgraded or cooperative
ownership claim.

The lock is process exclusion only. It carries no data authority: the WAL
and the verified manifest chain remain the only truth authorities. A stale
`LOCK` file left by a dead process holds no lock and never blocks recovery
by itself.

## Lineage threading

`HYROOT03` and `HYWAR002` record the exact 24-byte lineage identity defined
by [native canonical types](types-v1.md): 16 UUIDv7 bytes in network order,
followed by the nonzero history epoch as a little-endian `u64`.

`HYROOT03` has a 216-byte header. It preserves all `HYROOT02` fields and
offsets through byte 191, stores the directory UUID at bytes 192 through 207,
the history epoch at bytes 208 through 215, and begins canonical root entries
at byte 216.

`HYWAR002` is 280 bytes. It preserves the `HYWAR001` fields through the prior
anchor digest at byte 223, stores the directory UUID at bytes 224 through
239, the history epoch at bytes 240 through 247, and stores its final BLAKE3
digest at bytes 248 through 279. Its checksum remains at bytes 12 through 15.

The earlier reserved manifest bytes are not silently reinterpreted: they
cannot hold the complete 24-byte identity, and split-field identity would
make recovery and independent inspection needlessly ambiguous. The explicit
new magics, versions, and header lengths keep historical decoders fail-closed.

Two divergent histories of the same origin must be distinguishable
offline by comparing the recorded (directory identifier, history epoch)
pairs together with the manifest and anchor digest chains, without opening
either directory for write.

The codecs may continue to decode `HYROOT01`, `HYROOT02`, and `HYWAR001` for
historical compatibility and explicit import tooling. A directory governed
by a native `FORMAT` marker rejects any manifest or retention-anchor authority
that lacks the exact marker lineage, and one chain may never mix identities.

## Promotion marker (migration)

ADR-0021 migration step 6 promotes a migrated target only through an
explicit promotion marker:

1. the importer creates the native target directory with `FORMAT.pending`
   in place of `FORMAT`; the pending file already carries the complete
   canonical marker line;
2. a directory holding `FORMAT.pending` never opens as an authority; only
   the importer may continue or destroy it, and every other open path
   fails closed with an explicit pending-migration error;
3. after complete validation of counts, digests, and semantic equivalence,
   promotion is the atomic same-directory rename of `FORMAT.pending` to
   `FORMAT`, followed by synchronization of the parent directory where the
   platform supports it, following the existing WAL and manifest
   publication pattern; and
4. rollback before promotion deletes the complete target directory. The
   read-only format-2 source remains intact and authoritative until
   operational policy retires it.

A directory containing both `FORMAT` and `FORMAT.pending` is impossible
under this protocol and fails closed.

## Verification

Implementation of this contract requires:

- a golden test of the canonical `FORMAT` line, byte for byte;
- a fail-closed matrix covering missing, unknown-version, malformed,
  duplicated-field, mixed-family, format-2-prefixed, and pending markers;
- crash-boundary tests around the promotion rename: interruption before
  the rename, after the rename but before parent synchronization, and
  after synchronization must each reopen to one explicit state, never a
  mixture;
- the ADR-0022 lineage round-trip: two histories diverged from one origin
  must be distinguishable offline through the recorded identity and the
  digest chains; and
- a double-writer test proving that a second open fails with the explicit
  already-locked error while the first handle lives.

The 2026-08-02 Linux implementation slice covers the golden marker, UUIDv7
shape, stable reopen identity, missing/malformed/unknown/format-2/pending/
conflicting marker failures, mixed-family rejection, and live double-writer
exclusion. The promotion crash boundaries and the ADR-0022 lineage divergence
round-trip remain open. This partial evidence does not close G1 or authorize
format-2 migration.
