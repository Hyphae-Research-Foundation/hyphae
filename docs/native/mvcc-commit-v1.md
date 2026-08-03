# Native MVCC and commit semantics v1

Status: normative target contract; immutable snapshots, CSN reservation,
root-set hashing, recovery restore, concurrent detached write preparation, and
serialized all-engine publication are implemented experimentally; a
point-write conflict table is rebuilt from committed WAL, and relational
writes retain explicit immutable version chains; concurrent commit submission
and lock-free publication remain pending

V1 provides snapshot isolation across relational, structure, and search
objects under one global commit sequence.

## Snapshots

A transaction begins with:

```text
Snapshot {
  visible_csn,
  catalog_version,
  logical_time_micros,
  root_set
}
```

All reads use that immutable snapshot plus the transaction's private writes.
A later engine checkpoint or index merge cannot change the result.

## Version visibility

A version with `[begin_csn, end_csn)` is visible when:

```text
begin_csn <= snapshot.visible_csn < end_csn
```

`end_csn = u64::MAX` is open-ended. Tombstones participate in the same rule.
Private writes shadow snapshot versions for read-your-writes.

The implemented relational V2 format publishes one open row-version page and
links immutable closed copies toward older versions. Each older `end_csn`
equals the next-newer `begin_csn`; recovery validates the entire chain and
fails closed on cycles or discontinuities. Historical roots retain their
original pages until explicit current-root vacuum advances the page generation
and retention floor. Closing a version never mutates bytes reachable through
an older snapshot. Materialized in-process snapshots survive vacuum;
restartable pre-floor roots survive only when explicitly registered through a
verified [durable snapshot pin](snapshot-pins-v1.md). Unpinned operation keeps
the current-root retention floor.

## Snapshot-isolation conflicts

- Two concurrent transactions may read the same version.
- First committer wins for the same logical write key.
- A commit aborts when a written key has a committed version newer than the
  transaction's read CSN.
- Unique and foreign-key checks run against the candidate commit snapshot and
  the complete private write set.
- Predicate/range conflicts are not silently treated as serializable.

Serializable execution is a later mode using versioned range intents and
serializable snapshot isolation. It cannot be claimed by the v1 snapshot mode.

The current conflict table maps canonical `(engine, object, key)` write
identities to their latest committed CSN. Catalog creates additionally claim
the global object-ID and engine-qualified name identities. Admission rejects a
key whose latest writer is newer than the transaction read CSN, and recovery
reconstructs the table from decoded, digest-verified committed WAL mutations.

`begin_optimistic` captures and materializes a snapshot into an owned
`NativeWriteBatch` without file handles or writer admission. Multiple threads
can therefore read and mutate private batches concurrently. At
`commit_optimistic`, writer admission is serialized, the conflict table checks
the batch's original read CSN, and Hyphae reapplies admitted mutations to the
current root set. This rebase preserves intervening disjoint relational,
structure, and search writes instead of publishing stale whole-engine state.

The litmus test prepares two batches concurrently from the same CSN, commits
disjoint mutations across all three engines, rejects a later same-row loser,
and verifies the result after recovery. A separate genesis test proves that a
second disjoint commit may retain `read_csn = None` while receiving commit CSN
2.

Publication and durability I/O still execute under one writer guard, and the
public submit method currently requires exclusive `&mut NativeDatabase`
access. The evidence therefore establishes concurrent transaction execution
with first-committer-wins and serialized commit publication, not simultaneous
commit submissions, multi-client throughput, or lock-free writers.

## Cross-engine commit

The coordinator performs:

1. freeze and canonicalize the transaction write set;
2. validate catalog version, constraints and conflicts;
3. reserve the next CSN;
4. build private copy-on-write roots for every affected partition;
5. append one ordered cross-engine WAL transaction;
6. satisfy its durability class;
7. install all affected roots in one immutable `RootSet`;
8. publish the root set; and
9. advance `global_visible_csn` with release ordering.

Failure before step 7 leaves no engine-visible mutation. Failure after WAL
commit but before publication is recovered by replay. Readers never observe
only a relational, structure, or search subset.

## Root set

The root set binds:

- visible CSN and catalog version;
- WAL commit LSN and digest;
- catalog root;
- relational partition roots;
- structure partition roots;
- search delta and segment-generation roots;
- blob-generation root; and
- complete BLAKE3 root digest.

It is immutable after publication.

## Logical time and TTL

Each snapshot pins logical UTC microseconds. Structure reads treat a value as
expired when its committed `expires_at` is not greater than snapshot logical
time. The expiry scheduler later commits a tombstone transaction; it never
deletes an unversioned value in place.

Proofs and replay pin the original snapshot logical time. Wall-clock movement
cannot change a verified historical result.

## Background index work

Search and ANN deltas are visible at the committing CSN. Segment or graph
consolidation creates a new physical generation for the same logical state and
publishes it atomically. A reader retains its original generation until the
snapshot is released.

Structure reachability compaction follows the same snapshot rule but publishes
a replacement B+tree root under a new CSN because the WAL commit manifest is
the root authority. It requires the captured all-engine root set to remain
current, retains the other roots exactly, and preserves the prior structure
root for historical readers. It changes physical reachability, not logical
structure state. Page-file reclamation cannot discard that prior root until a
separate retention policy proves no retained snapshot or manifest owns it.

## Read-only and prepared transactions

Read-only transactions never enter the WAL. Prepared plans are not prepared
transactions: they bind syntax and catalog IDs but acquire a fresh snapshot on
execution unless explicitly attached to a live transaction.

## Verification

Required evidence includes isolation litmus tests, model checking of version
visibility, write/write conflict tests, constraint races, read-your-writes,
logical-time/TTL replay, root publication ordering, crash injection at every
commit step, and cross-engine readers that assert no mixed CSN.

Current tests cover half-open visibility, retained historical root sets,
atomic all-engine root publication, dropped-write nonpublication,
stale-same-key conflict rejection, disjoint-key admission, monotonic
idempotent conflict replay, WAL reconstruction, read-your-writes, logical TTL,
explicit closed relational chains, same-transaction version coalescing,
version-chain cycle rejection, V1 directory compatibility, concurrent detached
preparation, all-engine disjoint rebase, original-read-CSN recovery, and both
serialized and optimistic in-process commit interruption matrices.
Simultaneous commit submission, constraints, range intents, serializable
execution, and model-checked publication ordering remain pending.
