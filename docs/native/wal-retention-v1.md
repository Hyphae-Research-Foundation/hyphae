<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native WAL retention and bounded replay v1

Status: normative contract; current-root anchors, identity-preserving prefix
retirement, bounded suffix replay, interruption recovery, fail-closed
validation, lineage-bearing `HYWAR002`, native-marker validation, and direct
Linux tests are implemented

This protocol bounds native restart work without renumbering the authoritative
WAL or weakening its digest chain. It removes only a prefix made obsolete by a
current-root retention checkpoint. Every retained block keeps its original
block sequence, LSNs, bytes, and digest.

V1 deliberately defaults to current-root retention. A snapshot older than the
published retention floor is reconstructable after restart only when a
verified [durable snapshot pin](snapshot-pins-v1.md) names its exact manifest
and blocks an incompatible base selection. Replica pins, incremental-backup
pins, and remote WAL archiving require later contracts.

## Pain points fixed

The initial native WAL always starts at physical block one. Opening a database
therefore verifies and decodes every historical block, rebuilds every
transaction, and reconstructs the conflict table from genesis even after page
vacuum has retired every pre-floor physical root.

Deleting or rewriting those blocks directly is invalid:

- block sequence is derived from the absolute physical WAL address;
- every block authenticates the preceding block digest;
- record LSNs are encoded into each record;
- commit roots and root manifests retain exact WAL LSN/digest anchors;
- checkpoint records retain prior-checkpoint LSNs; and
- transaction IDs must not restart after history is removed.

V1 introduces one compacted-history trust root instead of changing any of
those identities.

## Required invariants

1. LSNs and block sequences are absolute, positive, monotonic, and never
   reused within one data directory.
2. A retained WAL block is byte-identical to the block originally appended.
3. The first retained block sequence is the retention anchor's
   `retired_through_sequence + 1`.
4. The first retained block's previous digest is the anchor's
   `retired_block_digest`.
5. The first retained transaction commit CSN is exactly
   `base_visible_csn + 1`.
6. The anchor references one synchronized checkpoint record that was the final
   record in its WAL and binds one verified immutable root manifest.
7. The referenced manifest's visible CSN and retention-floor CSN are equal.
   This makes every older restartable snapshot explicitly unavailable.
8. The referenced manifest reconstructs the complete catalog, relational,
   structure, lexical, and ANN root set at the base CSN.
9. Conflict reconstruction starts empty at the base CSN and publishes only
   retained suffix writes. A suffix transaction may not read below the base
   retention floor.
10. The next transaction ID comes from the anchor or a newer retained record.
    Prefix deletion never resets transaction identity.
11. Complete corruption in the anchor or retained suffix fails closed. Only an
    incomplete final physical WAL block may be repaired.
12. An acknowledgement is returned only after the new anchor, truncated WAL,
    and relevant directory entries are synchronized.

## Legacy retention anchor format

One historical `HYWAR001` anchor is exactly 256 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYWAR001` |
| 8 | 2 | format version `1` |
| 10 | 2 | header length `256` |
| 12 | 4 | CRC32C with checksum and anchor-digest fields zeroed |
| 16 | 8 | monotonically increasing anchor epoch |
| 24 | 8 | retired-through WAL block sequence |
| 32 | 8 | retired checkpoint-record LSN |
| 40 | 32 | retired checkpoint block digest |
| 72 | 8 | base visible CSN |
| 80 | 8 | referenced root-manifest generation |
| 88 | 32 | referenced root-manifest digest |
| 120 | 8 | root commit LSN from the manifest |
| 128 | 32 | root commit block digest from the manifest |
| 160 | 16 | next transaction ID |
| 176 | 8 | total checkpoint count through this anchor |
| 184 | 8 | total committed transaction count through this anchor |
| 192 | 32 | prior anchor digest; zero for the first anchor |
| 224 | 32 | BLAKE3 digest after CRC publication, with this field zeroed |

The committed transaction count must equal the base visible CSN in v1 because
native CSNs are contiguous and every committed maintenance transaction also
consumes one CSN. The duplicate field is retained as an explicit
cross-validation and reporting boundary.

### Lineage-bearing anchor v2

Every retention anchor created under a native `FORMAT` marker uses
`HYWAR002`. It extends the same fixed-field authority to 280 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYWAR002` |
| 8 | 2 | format version `2` |
| 10 | 2 | header length `280` |
| 12 | 4 | CRC32C with checksum and final-digest fields zeroed |
| 16 | 208 | all `HYWAR001` fields from anchor epoch through prior anchor digest |
| 224 | 16 | directory UUIDv7 in network byte order |
| 240 | 8 | nonzero history epoch, little-endian |
| 248 | 32 | BLAKE3 digest after CRC publication, with this field zeroed |

The checksum and digest cover the complete lineage identity. The referenced
manifest must carry the same lineage, and every prior/current/candidate anchor
in one chain must match it exactly. A mismatch fails before WAL prefix bytes
can be ignored or reset.

`HYWAR001` remains byte-identical and decodeable for historical compatibility
and explicit import tooling. It carries no lineage and is rejected as an
authority inside a native-marked directory.

Canonical final names are
`wal-anchor-NNNNNNNNNNNNNNNNNNNN.hywa`, where the decimal component is the
anchor epoch. Create-new stages append `.tmp`; a synchronized destructive
transition renames that stage to `.hywa.pending`; only after WAL reset
synchronization does it become `.hywa`. Unknown lookalike names, noncanonical
widths, duplicate epochs, zero fields, digest divergence, and noncontiguous
anchor transitions fail closed.

Only the latest stable anchor is required after cleanup. During publication,
the immediately prior stable anchor may coexist with one `.pending` candidate.
The explicit candidate state authorizes completion of a destructive reset
without allowing a corrupt retained suffix to masquerade as an old prefix.

## Eligibility

`truncate_wal_at_retention_checkpoint` is strict maintenance and rejects the
request unless all of these are true:

- a committed root exists;
- the current root has `visible_csn == retention_floor_csn`;
- the latest root manifest matches that root exactly;
- the latest WAL record is the synchronized checkpoint for that manifest;
- the checkpoint block is the last complete WAL block;
- no transaction, group cohort, checkpoint, vacuum, or background task is in
  progress;
- no durable snapshot pin, backup, replica, or archive authority requires the
  candidate prefix; and
- the next transaction ID and all cumulative counters fit their fields.

The current page-vacuum contract advances the floor to its own commit CSN.
Therefore the normal v1 sequence is: vacuum, checkpoint immediately, then
truncate. A later user commit makes that checkpoint ineligible until another
vacuum advances the retention floor.

The strict G7 recovery receipt uses this normal sequence only after every
timed group completion has returned and the scheduler has drained. The vacuum
is an explicit maintenance transaction, so its CSN is exactly one greater than
the last logical benchmark commit. The checkpoint retains that CSN without
advancing it, and the retention anchor uses it as `base_visible_csn`. Reopen
must then report an empty retained WAL suffix and zero replayed transactions.
All three maintenance calls and the subsequent open and logical verification
are outside the hot latency/throughput interval but remain measured and
projected once in the G7 runtime budget.

## Publication order

Under exclusive writer and maintenance admission:

1. verify the complete current WAL, checkpoint, manifest chain, roots, page
   generation, blobs, and conflict boundary;
2. construct the next anchor using the final checkpoint block;
3. write and synchronize the create-new anchor stage;
4. rename the stage to its `.pending` name and synchronize the data directory
   where supported;
5. poison the current WAL writer against further append;
6. reset `wal.hywal` to zero bytes and synchronize the file;
7. reopen the WAL writer at absolute sequence
   `retired_through_sequence + 1` with the retired block digest as its prior
   digest;
8. rename the `.pending` candidate to its immutable `.hywa` name and
   synchronize the directory;
9. remove the prior stable anchor, if any, and synchronize the directory; and
10. publish the new in-memory recovery base and return the receipt.

Any uncertain anchor publication, truncation, synchronization, or reopen
failure poisons the handle. The caller must drop it and reopen the data
directory.

## Recovery selection

With no anchor, recovery starts from implicit sequence zero and digest zero.

With one stable `.hywa` anchor, recovery:

1. verifies the anchor checksum and digest;
2. opens and verifies the complete root-manifest chain;
3. requires the referenced manifest identity and WAL root anchor to match;
4. reconstructs the base `RootSet` from that manifest;
5. verifies the retained WAL starting at the anchor's next absolute sequence
   and prior digest;
6. decodes only suffix transactions and checkpoints;
7. validates suffix CSN, retention, page-generation, transaction-ID, and
   checkpoint continuity against the base; and
8. validates the base roots and every retained committed root before
   publishing the latest root.

An empty WAL is valid after anchor publication. The next append still uses the
absolute next block sequence and therefore produces absolute LSNs greater than
every retired LSN.

One `.pending` anchor means reset publication was interrupted. Recovery first
verifies the candidate, referenced manifest, and its link to the prior stable
anchor or genesis. That explicit state authorizes discarding bytes at or below
the candidate's retired sequence, synchronizing the empty WAL, promoting the
candidate to `.hywa`, and removing the prior stable anchor.

Two stable `.hywa` anchors mean promotion completed but prior-anchor cleanup
was interrupted. The newer anchor must be the exact next epoch and bind the
older digest; recovery removes the older anchor only after the retained WAL
verifies from the newer base.

A stable anchor never authorizes ignoring its retained suffix. Any complete
first-block sequence, previous-digest, checksum, or content corruption fails
closed. A WAL containing both pre-anchor residual bytes and a post-anchor
suffix is impossible under the publication protocol and also fails closed.

## Bounded semantic replay

Recovery reports separate work:

- `wal_base_csn`;
- `retired_wal_blocks`;
- `retained_wal_blocks`;
- `retained_wal_bytes`;
- `replayed_transactions`;
- `base_checkpoint_count`;
- `retained_checkpoint_count`; and
- incomplete tail bytes repaired.

`committed_transactions` remains the logical total:
`base_committed_transactions + replayed_transactions`.

The replay bound is the retained suffix, not database age. A database with one
million retired commits and ten retained commits verifies and semantically
replays only the ten-commit suffix plus one fixed-size anchor and the retained
manifest chain. Manifest pruning and bounding that separate chain are later
work in the original WAL-only vertical. They are now implemented under the
same anchor; the required authority, publication order, failure states, and
evidence are fixed by [Native manifest retention
v1](manifest-retention-v1.md).

## Deterministic interruption matrix

The test-only truncation path interrupts after:

1. the candidate anchor stage is synchronized;
2. the `.pending` candidate is published while the old WAL remains complete;
3. the old WAL length is reset but not explicitly synchronized;
4. the empty WAL is synchronized;
5. the candidate is promoted to stable `.hywa`; and
6. the prior anchor is removed.

Every boundary must reopen to the same complete all-engine logical state. It
must choose either the fully verified old prefix or the fully verified anchor,
never a synthetic mixture. Retrying truncation after recovery is idempotent.

Tests additionally corrupt every anchor field, the anchor checksum/digest, the
referenced manifest, the first retained block sequence/previous digest, the
first suffix commit CSN/read CSN, and the next transaction ID boundary.

## Required performance evidence

One reproducible corpus must:

- create enough pre-floor transactions to exceed the retained suffix by at
  least 100 times;
- vacuum, checkpoint, and truncate;
- append a fixed retained suffix that mutates all three engines;
- reopen and verify exact logical equivalence;
- report pre/post WAL bytes and blocks;
- report anchor publication and WAL reset synchronization separately;
- report open, physical verification, semantic replay, and root validation
  times separately; and
- compare the same logical final state with and without truncation.

Warm, cold, Windows/NTFS, native Linux/ext4, saturation, and power-loss
evidence remain separate. A bounded block count does not by itself prove a
universal microsecond restart.

## Non-goals

V1 does not:

- retain restartable history below the current floor without a verified
  durable snapshot pin;
- collect immutable blobs or old manifest generations;
- archive WAL remotely;
- define replica or incremental-backup pin registration;
- truncate a checkpoint whose visible CSN is newer than the retention floor;
- renumber LSNs, blocks, CSNs, transaction IDs, or manifest generations; or
- close G1 or G7 without the broader recovery, fault, and performance matrix.
