# Native durable snapshot pins v1

Status: normative proposed contract; acceptance criteria are frozen before
implementation; executable red/green evidence, crash coverage, direct Linux
receipts, and hosted CI remain pending

This protocol turns a current all-engine root set into a durable, named
historical snapshot that remains reopenable after process restart and later
page-generation vacuum. A pin protects one exact relational, structure,
lexical/ANN, catalog, page, blob, manifest, and WAL identity. It is not an
in-process reference count and it is not a best-effort backup hint.

V1 adds multi-generation retention without changing the authority order:
synchronized WAL still publishes commits, synchronized checkpoint records
still authenticate root manifests, and a pin can only reference an already
authenticated manifest. The pin is retention authority, not transaction
authority.

## Product behavior

A caller supplies a nonzero 128-bit `SnapshotPinId` and a logical UTC time.
`pin_current` checkpoints the current committed root, then publishes one
immutable pin record. A successful call means that:

- `open_pinned_snapshot(id)` can materialize the exact all-engine state after
  closing and reopening the database;
- later commits and page vacuums may continue normally;
- every page-file generation needed by the pin remains physically present;
- WAL and manifest retention cannot retire authority required by the pin;
- blob collection cannot remove content required by the pin; and
- a duplicate ID is rejected rather than overwritten.

`unpin(id)` durably removes only the retention claim. It does not implicitly
delete pages, manifests, WAL, or blobs. The caller must run explicit
maintenance after unpinning. This keeps the destructive side effect separate
from the identity operation and makes retry behavior observable.

## Required invariants

1. A pin ID is a nonzero `u128`. Its canonical text is 32 lowercase
   hexadecimal digits.
2. A pin belongs to exactly one `LineageIdentity` (directory UUID plus history
   epoch). Copying a pin file into another native directory fails closed.
3. A pin references one positive visible CSN, one exact immutable manifest
   generation and digest, one root-set digest, one WAL commit anchor, one page
   generation, one blob-generation floor, and one retention-floor CSN.
4. The referenced manifest must decode, carry the same lineage, reconstruct
   the exact root set, and match every duplicated identity in the pin.
5. The referenced manifest must have a synchronized checkpoint record in the
   verified WAL history or in the selected WAL-retention trust root. A merely
   published, unanchored manifest cannot be pinned.
6. The referenced page generation must exist as one canonical complete file.
   Every root reachable from the manifest must validate against that file and
   the verified blob store before database open succeeds.
7. Pin publication is create-new. Canonical pins are immutable. Reusing an ID,
   replacing bytes, or publishing divergent bytes under one ID fails.
8. Every canonical pin is verified before temporary pin stages or unpinned
   page generations are removed during recovery.
9. Unknown files, directories, malformed names, corrupt records, checksum
   divergence, missing manifests, missing page generations, or lineage
   mismatch in `pins/` fail database open.
10. A complete canonical `.tmp` pin is never authority. Recovery counts and
    removes canonical temporary stages only after every stable pin and its
    referenced state verify.
11. Vacuum may retire an inactive page generation only when no stable pin
    references it. It must retain any number of distinct pinned generations.
12. Startup and explicit page-generation collection remove only complete,
    inactive, unpinned generation files. The active generation and every
    pinned generation are immutable retention roots.
13. WAL retention may select a base only when every pin older than that base
    has been removed. A pin for the exact selected base remains valid.
14. Manifest pruning may never remove a pin's referenced manifest. Existing
    retained-chain rules remain unchanged above the oldest required base.
15. Blob collection must either trace every retained historical pin or reject
    collection before deletion. V1 chooses the fail-closed rule for a pin
    whose root differs from the sole current retained root.
16. Pin create, reopen, unpin, and collection are idempotent only where stated:
    duplicate create fails, reopen is read-only, missing unpin fails, and
    repeated collection returns a zero-removal receipt.
17. A pin captures logical time. TTL reads from the reopened snapshot use that
    captured time; they do not silently advance to reopen time.
18. Pinned reads materialize from the pinned page generation and never alias
    equal page IDs in the active generation.

## Canonical pin record

Stable records live in `pins/pin-<id>.hypin`. Create-new stages append `.tmp`.
The record is exactly 240 bytes:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYPIN001` |
| 8 | 2 | format version `1` |
| 10 | 2 | record length `240` |
| 12 | 4 | reserved zero |
| 16 | 24 | encoded `LineageIdentity` |
| 40 | 16 | pin ID, unsigned big-endian |
| 56 | 8 | visible CSN |
| 64 | 8 | logical UTC microseconds, signed little-endian |
| 72 | 8 | manifest generation |
| 80 | 32 | manifest digest |
| 112 | 32 | root-set digest |
| 144 | 8 | page generation |
| 152 | 8 | blob generation |
| 160 | 8 | retention-floor CSN |
| 168 | 8 | committed WAL LSN |
| 176 | 32 | committed WAL block digest |
| 208 | 32 | BLAKE3 checksum of bytes `0..208` |

Every unsigned integer except the pin ID is little-endian. Zero pin IDs, CSNs,
page/manifest generations, floors, LSNs, digests, or checksums are invalid.
Blob generation may be zero before the first immutable blob is committed. The
visible CSN must be at least the retention floor. Reserved bytes must be zero.
Decode requires the exact record length; trailing bytes are corruption.

The filename ID and payload ID must match. Directory enumeration sorts by pin
ID so recovery reports and collection behavior are deterministic.

## Publication protocol

`pin_current(id, logical_time_micros)` uses this order:

1. reject an empty database and an already visible stable or temporary target;
2. publish and synchronize an immutable current-root manifest;
3. append and synchronize its WAL checkpoint record;
4. construct the pin from the exact manifest, root set, WAL anchor, lineage,
   page generation, blob generation, retention floor, and logical time;
5. create the `.hypin.tmp` file with create-new semantics;
6. write and synchronize the complete record;
7. rename it to `.hypin`; and
8. synchronize `pins/` before acknowledging success on supported platforms.

A failure before step 7 leaves no pin authority. A failure after step 7 may
leave a complete pin that recovery must honor. Retrying the same ID therefore
reports `SnapshotPinExists`; it never mutates the prior pin.

`unpin(id)` verifies the in-memory record, removes its canonical file, and
synchronizes `pins/` before acknowledging. A stop before acknowledgement may
recover either the complete original pin or no pin, but never divergent pin
content. Once absence is durably acknowledged, the ID can be created again
for a new snapshot.

## Recovery and historical open

Database open performs:

1. validate the native directory marker and open the pin namespace;
2. enumerate canonical pins and stages without deleting either;
3. decode every stable pin and require the active lineage;
4. recover WAL, retention anchors, and the retained manifest chain;
5. cross-check every pin against its exact manifest and checkpoint authority;
6. open every distinct referenced page generation read-only and validate all
   pinned roots and blobs;
7. open and validate the active generation and current root;
8. remove verified temporary pin stages;
9. remove inactive page generations not named by any pin; and
10. publish the recovered pin registry in the database handle.

`open_pinned_snapshot(id)` repeats exact root/page/blob validation before it
materializes state. The resulting `NativeSnapshot` owns its logical state and
metadata, so closing the temporary historical page-file handle cannot affect
the snapshot.

A pin cannot resurrect a retired manifest or WAL prefix. If external file
deletion makes a pin unverifiable, open fails; it does not downgrade the pin
to a warning.

## Retention and collection

Page vacuum creates and WAL-publishes the next generation exactly as before.
After root publication it removes the previous file only when the pin registry
does not name that generation. `collect_retired_page_generations` performs the
same deterministic check across all inactive generations and reports removed
and retained counts, bytes, and directory-sync support.

WAL-retention eligibility compares the proposed base manifest/root with all
pins. A pin whose manifest generation or visible CSN is older than the base
returns `SnapshotPinsBlockWalRetention` before staging an anchor. This also
prevents manifest-prefix pruning from invalidating a pin.

Blob collection retains its current exact-root preconditions. If any pin names
another root digest, page generation, or blob generation, it returns
`SnapshotPinsBlockBlobCollection` before starting a reference trace. A pin for
the exact current root is already covered by that root's trace.

After the last historical pin is removed, the intended cleanup sequence is:

1. collect retired page generations;
2. vacuum the current page generation if logical garbage remains;
3. checkpoint the resulting current root;
4. run WAL/manifest retention; and
5. collect blobs.

Each operation retains its own receipt and interruption matrix. Unpin alone
claims no reclaimed bytes.

## Executable acceptance criteria

The implementation gate must demonstrate all of the following on direct
Linux/ext4:

1. pin a cross-engine snapshot, commit divergent relational, structure,
   lexical, and ANN state, vacuum, restart, and read the exact old state by ID;
2. retain at least three simultaneously pinned page generations and reopen a
   pin from each generation after a later vacuum;
3. prove current reads use the active generation while pinned reads with equal
   page IDs use their named historical generation;
4. reject duplicate IDs, unknown IDs, missing unpin, zero IDs, malformed
   filenames, non-files, truncated/extended records, nonzero reserved bytes,
   checksum corruption, filename/payload mismatch, lineage mismatch, manifest
   mismatch, missing manifest, and missing/corrupt pinned page files;
5. prove a canonical temporary stage is not authority and is removed only
   after stable registry validation;
6. interrupt pin creation after synchronized stage and after rename, then
   reopen to respectively absent-or-complete semantics;
7. interrupt vacuum at every existing boundary while an old generation is
   pinned and prove the pinned snapshot always reopens;
8. prove WAL retention and blob collection reject an older historical pin
   before destructive mutation;
9. unpin one of several pins, explicitly collect, and prove only generations
   still referenced by active state or remaining pins survive;
10. unpin the final historical pin, run the complete cleanup sequence, reopen,
    and prove current state plus zero retired unpinned generations;
11. prove captured logical time preserves TTL results across restart;
12. bind a machine-readable receipt to source commit/tree, filesystem,
    commands, scenario counts, crash termination mode, retained/removed bytes,
    and all exclusions.

The red run must fail because these APIs or behaviors are absent on the
pre-implementation source. Green requires formatter, locked Clippy with
warnings denied, targeted codec/registry/runtime tests, the full affected
workspace lane, documentation checks, crash scenarios, and clean diff/status.
If mutation tooling remains unavailable, Gate 4 stays explicitly not run.

## Non-goals

V1 does not define wall-clock expiration, leases, remote replicas, backup
shipping, archive export, tenant policy, pin quotas, background scheduling, or
physical-device power-cut evidence. It does not allow writes through a pinned
