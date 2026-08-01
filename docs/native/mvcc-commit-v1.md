# Native MVCC and commit semantics v1

Status: normative target contract; immutable snapshots, CSN reservation,
root-set hashing, recovery restore, and serialized all-engine publication are
implemented experimentally; conflict tables and lock-free publication remain
pending

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

## Read-only and prepared transactions

Read-only transactions never enter the WAL. Prepared plans are not prepared
transactions: they bind syntax and catalog IDs but acquire a fresh snapshot on
execution unless explicitly attached to a live transaction.

## Verification

Required evidence includes isolation litmus tests, model checking of version
visibility, write/write conflict tests, constraint races, read-your-writes,
logical-time/TTL replay, root publication ordering, crash injection at every
commit step, and cross-engine readers that assert no mixed CSN.
