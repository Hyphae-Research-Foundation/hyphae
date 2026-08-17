<!-- SPDX-License-Identifier: Apache-2.0 -->
# Compaction protocol v1

Status: normative for Hyphae `0.1.0` disk format `1`.

Compaction retires a verified log prefix without breaking its global sequence
or digest chain. It depends on three durable records:

- a logical snapshot containing sorted KV state and every historical
  idempotency receipt through its checkpoint;
- a new empty log segment whose first future frame continues at checkpoint
  sequence plus one and names the checkpoint digest as its previous digest;
- an immutable manifest generation selecting that snapshot and segment.

## Commit order

1. Catch the materialized index up to the authoritative log.
2. Create, synchronize, and reread-verify the logical snapshot.
3. Create and synchronize the next empty anchored segment.
4. Write and synchronize a new immutable manifest generation in `tmp/`.
5. Atomically rename that manifest into `manifest/`; this is the commit point.
6. Switch the live writer to the prepared segment.
7. Remove the retired segment and synchronize the log directory where
   supported. Cleanup failure does not roll back the committed generation and
   is retried after a fully verified reopen.

Snapshot creation, prepared-segment verification, and the precommit manifest
check spend one shared `MaintenanceLimits` deadline. A limit/timeout before
step 5 leaves the prior manifest authoritative; temporary snapshot and
restored-index files are removed. Once step 5 succeeds, cleanup is
best-effort and a later elapsed deadline cannot relabel the committed
generation as failed.

## Interruption semantics

| Last durable boundary | Recovery decision |
|---|---|
| Snapshot or prepared segment only | Older manifest remains active; orphan future segment is ignored and may be reused |
| New manifest committed | New snapshot anchor and segment win even if the retired segment remains |
| Retired segment removed | New snapshot plus later segment are sufficient to rebuild the index |

Opening validates the highest manifest, its exact snapshot checkpoint and
digest, the anchored active segment, and the redb checkpoint before cleaning
retired files. A missing rebuildable redb index is recreated transactionally
from snapshot KV and idempotency sections, then later committed frames are
replayed.

Immediate repeated compaction with no committed frames beyond the current
snapshot is a no-op. Complete unexpected frames in an uncommitted prepared
segment are rejected rather than silently adopted.
