# Native immutable-blob collection v1

Status: normative target contract; implementation, interruption matrix, and
performance evidence are pending

This protocol bounds the verified immutable-blob corpus after current-root
page vacuum, checkpoint, and WAL retention. It removes only complete
content-addressed blob files that the sole retained restart root cannot
reference.

V1 is intentionally conservative. It does not attempt collection while a WAL
suffix, historical restart root, or unretired manifest generation remains.

## Pain point

Blob publication precedes root publication so that a committed root never
references absent content. A transaction interrupted after blob promotion can
therefore leave a complete orphan. Copy-on-write updates and deletes also
leave blobs reachable only from retired roots.

The current blob store verifies every complete `HYBLOB01` file during open and
derives `blob_generation` from the physical file count. Page vacuum and WAL
retention can retire every root that references old content, but they do not
remove the corresponding files. Blob bytes and restart verification therefore
remain unbounded.

Deleting every blob absent from the latest materialized query result is not
safe. Liveness must include:

- catalog definition blobs;
- every relational row version reachable from the retained root, including
  closed history that the canonical decoder is required to validate;
- scalar, hash, list, set, sorted-set, and expiry-index values;
- lexical source documents;
- content deduplicated across engines; and
- every future blob-bearing format accepted by root validation.

## Required invariants

1. A blob is identified by its complete canonical `BlobReference`: stable
   `BlobId`, logical length, and BLAKE3 digest.
2. Collection starts only after the complete physical blob inventory, retained
   WAL state, retained manifest chain, pages, roots, and all-engine logical
   state verify.
3. The sole retained root is the exact root bound by the active stable
   `HYWAR001` retention anchor.
4. The retained WAL suffix is empty, the retained manifest chain contains
   exactly the anchor manifest, and the root's visible CSN equals its retention
   floor.
5. Liveness is produced by the same complete root-validation traversal used by
   recovery. A sampled query, cached state, caller-supplied reference set, or
   filename scan is not collection authority.
6. A candidate is removed only when its verified complete reference is absent
   from the traced live-reference set.
7. A live reference must resolve to exactly one verified immutable file before
   any candidate is removed.
8. Shared content is live when any engine or catalog object references it.
9. Collection never rewrites a blob, root, manifest, WAL block, CSN, LSN,
   page generation, digest, or filename.
10. The committed blob-generation floor never decreases. Removing physical
    files does not lower the in-memory generation used by the next commit.
11. A restart derives the generation as the maximum of the latest retained
    committed root's generation floor and the verified physical file count.
12. A newly published unique blob advances the generation once. Reusing an
    existing verified blob does not.
13. File deletion is idempotent and directory synchronization follows all
    removals where the platform supports it.
14. Any uncertain validation, deletion, or synchronization failure poisons the
    maintenance handle. The caller must drop it and reopen.

## Eligibility and maintenance order

The public maintenance sequence is:

1. `vacuum_pages`;
2. `checkpoint`;
3. `truncate_wal_at_retention_checkpoint`; and
4. `collect_blobs`.

Collection is eligible only when all of the following remain true under the
exclusive mutable database handle:

- one stable WAL-retention anchor exists and no pending candidate exists;
- the active WAL file contains zero retained bytes and records;
- the manifest store retains exactly the anchor generation;
- the coordinator's current root equals the anchor manifest root;
- `visible_csn == retention_floor_csn == anchor.base_visible_csn`;
- the root page generation equals the open active generation; and
- the root blob generation does not exceed the blob store's recovered
  generation floor.

Any later commit, checkpoint, unanchored manifest, WAL suffix, or retention
transition makes collection ineligible until the sequence is repeated.

## Authoritative liveness trace

`BlobStore` exposes a scoped reference trace. While the scope is active, every
successful exact `read(BlobReference)` records the complete reference after
file checksum, content digest, identity, length, and caller reference match.

The runtime:

1. enables an empty trace;
2. executes complete `validate_roots` for the sole retained root;
3. disables the trace on success or error;
4. rejects an empty or incomplete validation result through the existing
   format-specific validators;
5. cross-checks every traced reference against the verified store inventory;
   and
6. computes `inventory - live` by complete reference identity.

The trace is not a general telemetry facility. Nested traces fail, and
collection owns exclusive maintenance admission. Incidental concurrent reads
would be conservative because they can add liveness, never authorize a
deletion.

The complete recovery traversal already decodes all catalog definitions,
relational version chains, structures, lexical documents, and ANN metadata.
Every new blob-bearing durable format must enter that traversal before it can
be accepted by collection.

## Blob generation after collection

Existing format-1 roots store a `blob_generation` that was originally equal to
the verified file count at commit. It is a monotonic storage bound, not a blob
identifier and not a filename generation.

V1 preserves the field and existing bytes:

- `BlobStore::create` starts at generation zero;
- legacy `BlobStore::open` retains count-derived behavior for isolated crate
  callers;
- runtime open supplies the latest retained committed generation as a floor;
- physical verification may raise the runtime generation when complete
  promoted orphans exist;
- collection leaves the in-memory generation unchanged; and
- the next unique publication increments the current generation.

If an uncommitted promoted orphan raised only a transient recovered generation
and is later collected without another commit, a subsequent restart may return
to the latest committed floor. No retained root observed the transient value,
so this does not reuse committed durable identity.

## Deletion and synchronization

The store records the exact candidate count and encoded bytes before removal.
Candidates are ordered by lowercase digest filename for deterministic tests
and receipts.

For each candidate:

1. require that the in-memory verified reference still matches the candidate;
2. require the canonical final path and no alternate path;
3. remove the exact final file;
4. remove the reference from the verified in-memory inventory; and
5. accumulate removed files and bytes.

After the final removal, synchronize `blobs/` where supported. A successful
zero-candidate retry also synchronizes the directory. This completes a prior
attempt that may have removed every candidate but stopped before directory
synchronization.

Windows reports directory synchronization as unsupported. A successful
Windows observation proves process-restart behavior, not sector-level
power-loss durability.

## Recovery and maintenance evidence

Recovery reports:

- verified physical blob files and bytes;
- recovered committed blob-generation floor;
- effective runtime blob generation;
- complete blob-verification time;
- interrupted temporary files removed; and
- whether parent-directory synchronization is supported.

The collection receipt reports:

- root visible CSN and generation floor;
- live blob files and bytes;
- candidate blob files and bytes;
- removed blob files and bytes;
- retained physical blob files and bytes;
- reference-trace time;
- candidate-deletion time;
- directory-synchronization time;
- total collection time; and
- whether directory synchronization is supported.

## Deterministic interruption and corruption matrix

Tests interrupt:

1. after authoritative liveness tracing and before deletion;
2. after removing the first candidate from a corpus containing at least two;
3. after removing all candidates but before directory synchronization; and
4. after directory synchronization but before returning the receipt.

Every boundary must reopen to the exact same catalog, relational, structure,
lexical, and ANN state. Retrying collection removes the remaining candidates
or performs an idempotent directory synchronization.

Tests additionally:

- retain one blob referenced by each blob-bearing engine family;
- retain one digest shared across at least three engines;
- collect a promoted transaction orphan;
- collect blobs referenced only by roots retired through vacuum and WAL
  retention;
- reject collection before vacuum, before checkpoint, before WAL retention,
  with a WAL suffix, and with an unanchored manifest suffix;
- fail closed for a missing or corrupt live blob;
- fail closed for corruption in a complete dead candidate before collection;
- reject nested traces and forged live-reference identity;
- reopen when physical count is lower than the committed generation floor;
- publish a new unique blob after collection and prove generation advancement;
  and
- prove a second collection is an idempotent zero-candidate operation.

## Required performance evidence

One reproducible corpus must:

- create at least 100 distinct blobs that become dead;
- retain blob content in relational, scalar keyspace, lexical, and catalog
  paths, including one cross-engine deduplicated blob;
- vacuum, checkpoint, retain WAL, and collect;
- compare the same final logical state before and after collection;
- report blob files and bytes before and after;
- report physical blob verification, root validation, liveness tracing,
  deletion, synchronization, and total collection separately;
- reopen at least 25 times and report first, p50, p95, and p99;
- bind observations to one exact commit and tree; and
- label the actual filesystem holding benchmark data.

Warm, cold, Windows/NTFS, native Linux/ext4, saturation, large blobs, storage
wear, and physical power-loss evidence remain separate.

## Non-goals

V1 does not:

- collect while more than one restartable root is retained;
- register snapshot, replica, backup, archive, or change-feed pins;
- collect page generations, WAL blocks, or manifests;
- stream, chunk, compress, encrypt, or rewrite blob content;
- repair missing or corrupt live content;
- use reference counts as recovery authority;
- claim Windows directory-sync durability;
- make collection automatic or background scheduled; or
- close G1 or G7 without the broader scheduler, recovery, and physical
  durability matrix.
