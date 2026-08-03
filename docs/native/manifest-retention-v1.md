# Native manifest retention v1

Status: normative contract; anchored retained-chain open, identity-preserving
prefix retirement, idempotent partial cleanup, fail-closed validation, runtime
instrumentation, lineage validation, and direct Linux tests are implemented

This protocol bounds root-manifest verification after current-root WAL
retention. Native directories use the lineage-bearing `HYWAR002` anchor as the
compacted-history trust root and delete only immutable manifest generations
older than the manifest bound by that anchor.

V1 does not rewrite, renumber, or re-digest any retained manifest. The base
manifest keeps its original generation, predecessor digest, bytes, and
filename.

## Pain point

WAL retention bounds physical verification and semantic replay to the retained
suffix, but open currently verifies every immutable manifest from generation
one. Checkpoint-heavy databases therefore retain an unbounded independent
restart path even after their WAL and page files have retired the same history.

Deleting old manifests without another trust root is invalid:

- generation one is currently the only implicit chain root;
- each manifest authenticates the preceding manifest digest;
- checkpoints name exact manifest generations and digests; and
- a retained WAL suffix may reference manifests newer than the retention
  checkpoint.

`HYWAR002` binds the exact current-root checkpoint manifest generation,
digest, and directory lineage. V1 makes that binding the explicit retained
manifest-chain root.

## Required invariants

1. Manifest generations are absolute, positive, monotonic, and never reused.
2. A retained manifest is byte-identical to the originally published file.
3. The first retained manifest generation equals the active retention anchor's
   `manifest_generation`.
4. The first retained manifest digest equals the active retention anchor's
   `manifest_digest`.
5. The first retained manifest's predecessor digest is preserved. It is not
   required to be zero after prefix retirement.
6. Every later retained manifest is contiguous and binds the exact digest of
   its retained predecessor.
7. The base manifest reconstructs the same complete root set, visible CSN,
   retention floor, page generation, WAL LSN, and WAL digest as the anchor.
8. Every retained WAL checkpoint references one retained manifest with exact
   generation, CSN, and digest identity.
9. A stable or recoverable pending WAL-retention anchor must verify before any
   manifest prefix is removed.
10. Any corruption, gap, duplicate, or digest divergence at or above the base
    generation fails closed.
11. A missing, corrupt, or partially removed generation below the verified
    base is retired state and cannot influence recovery.
12. Acknowledged strict pruning requires synchronization of the roots
    directory where the platform supports it.

## Authority and eligibility

No new manifest-anchor file is introduced. The selected `HYWAR002` anchor is
the only prefix-retirement authority.

Without a WAL-retention anchor, manifest recovery remains unchanged: it starts
at generation one, requires a zero predecessor digest, and verifies the
complete contiguous chain.

With one stable anchor, recovery selects its manifest generation and digest.
With one valid `.hywa.pending` candidate, recovery selects the candidate
because that state authorizes completion of the destructive WAL reset. With
two stable anchors during cleanup, recovery selects the newer contiguous
anchor.

Prefix deletion is eligible only after:

- the selected anchor checksum, digest, epoch chain, and identity fields
  verify;
- the exact base manifest decodes and matches the anchor;
- every manifest at or above the base forms one contiguous digest chain;
- the retained WAL verifies from the anchor's absolute block/digest base;
- every retained checkpoint matches one retained manifest; and
- every retained committed root validates against the selected page and blob
  generations.

## Retained-chain open

`RootManifestStore::open_after` receives the selected base generation and
digest:

1. enumerate only canonical files in `roots/`;
2. classify canonical `.tmp` stages separately;
3. require the exact base generation to exist;
4. decode the base and require its complete digest to equal the selected
   digest;
5. verify every generation above the base contiguously;
6. retain paths below the base as removable prefix candidates without using
   their contents as recovery authority;
7. remove temporary stages only after the retained chain verifies; and
8. return the retained manifests plus exact prefix-file counts and bytes.

The caller must cross-validate the base manifest against the selected anchor
before it removes any prefix candidate.

Canonical files below the base may be missing in any pattern because a prior
cleanup can stop between file deletions. Noncanonical entries, directories,
or filenames at any generation still fail closed.

## Publication and pruning order

Manifest prefix retirement extends the WAL-retention publication order. Under
exclusive writer and maintenance admission:

1. verify the complete pre-retention WAL, manifest chain, roots, pages, and
   blobs;
2. publish and synchronize the new `.hywa.pending` anchor;
3. reset and synchronize `wal.hywal`;
4. promote the candidate to immutable `.hywa`;
5. reopen the retained WAL from the anchor and validate its complete suffix;
6. open the manifest chain from the anchor's exact generation and digest;
7. cross-validate the base manifest and every retained checkpoint/root;
8. delete manifest files strictly below the base generation;
9. synchronize `roots/` where supported;
10. remove the prior stable WAL anchor; and
11. update in-memory recovery metadata and acknowledge.

Deletion is file-granular and idempotent. A failure or process stop during step
8 leaves any subset of the retired prefix. The next open repeats retained-chain
validation before deleting the remaining subset.

Any uncertain retained-chain validation, deletion, or synchronization failure
poisons the maintenance handle. The caller must drop it and reopen.

## Recovery report

Recovery and maintenance receipts report:

- `manifest_base_generation`;
- retained manifest count and bytes;
- retired manifest prefix count and bytes discovered;
- retired manifest files removed;
- retained-chain verification time;
- manifest-prefix deletion time; and
- whether roots-directory synchronization is supported.

The retained count includes the base manifest. It is independent from the
logical cumulative checkpoint count held by `HYWAR002`.

## Deterministic interruption and corruption matrix

Tests interrupt:

1. after WAL-anchor stabilization but before manifest deletion;
2. after deleting a nonempty proper subset of the manifest prefix;
3. after deleting the complete prefix but before directory synchronization;
4. after roots-directory synchronization; and
5. after prior-anchor cleanup.

Every boundary reopens to the same complete all-engine state and reports the
same absolute CSN, transaction ID, checkpoint count, base generation, and
retained manifest chain.

Tests additionally:

- corrupt and remove the base manifest;
- corrupt every retained suffix manifest;
- introduce retained generation gaps and digest divergence;
- leave arbitrary lower-generation prefix gaps;
- leave canonical temporary stages above and below the base;
- add noncanonical lookalike entries;
- retry pruning after complete and partial cleanup; and
- prove that no anchor preserves the generation-one strict chain rule.

## Required performance evidence

One reproducible corpus must:

- publish at least 100 pre-base manifests;
- publish a fixed retained suffix with at least one later checkpoint;
- compare the same final logical state with and without prefix retirement;
- report manifest count and bytes before and after;
- report retained-chain verification separately from WAL and root validation;
- reopen at least 25 times and report first, p50, p95, and p99;
- verify exact relational, structure, lexical, and ANN state; and
- bind observations to one exact commit and tree.

Warm, cold, Windows/NTFS, native Linux/ext4, saturation, power-loss, and large
manifest payload evidence remain separate.

## Non-goals

V1 does not:

- retain restartable history below the current root unless a verified
  [durable snapshot pin](snapshot-pins-v1.md) names the exact manifest;
- rewrite a retained manifest's predecessor digest;
- merge multiple manifests into one new format;
- prune manifests newer than the anchor;
- collect immutable blobs or page generations;
- define replica, archive, or incremental-backup pin registration;
- replace the WAL as transaction authority; or
- close G1 or G7 without the broader recovery and physical durability matrix.
