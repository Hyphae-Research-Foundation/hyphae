# Native whole-list lifecycle v1

Status: implemented and directly verified on Linux; hosted checks pending.

Evidence:
[Native whole-list lifecycle evidence on Linux — 2026-08-03](../gates/evidence/native-list-lifecycle-linux-2026-08-03.md).

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md) with
explicit complete-list deletion and checked same-transaction recreation. It
uses Hyphae-owned materialized state, WAL, MVCC, chunked B+tree storage,
snapshot authority, and commit sequencing.

## Surface

The embedded native write surface adds:

```text
DELETE_LIST(key) -> bool
```

`DELETE_LIST` requires the native B+tree structure format. It returns `true`
and prepares one complete-list lifecycle mutation when the transaction sees a
typed native list. It returns `false` without adding a mutation for a missing
list. A live scalar, hash, set, or sorted set fails with
`StructureKindMismatch`.

Deletion is source-data retirement. It is not cache eviction, does not read a
wall clock, and does not route through a compatibility protocol.

## Logical lifecycle

Successful deletion removes the complete private list incarnation, including
an explicitly typed empty list. `LLEN`, `LRANGE`, `LPUSH`, `RPUSH`, `LPOP`,
and `RPOP` observe the list as missing after the call.

Snapshots retained before commit continue to observe the complete prior
ordered value sequence. Snapshots at or after the deletion commit observe a
missing list. No read may expose a partial chunk population.

A transaction may recreate the deleted key as:

- a scalar;
- a hash;
- a set;
- an empty or populated new list; or
- a sorted set.

The replacement is a new typed incarnation. Retired chunks and their inline
or blob-backed elements never attach to it. Pushes or pops prepared earlier
in the same transaction may precede `DELETE_LIST`; the final visible state is
still missing or the later replacement.

## WAL and physical state

This slice reserves one additive structure opcode:

```text
DELETE_LIST = 35
```

The mutation has engine `Structure`, no target, the complete binary list key
as `key`, an empty value, and no expiry. Existing opcode meanings do not
change.

Physical application:

1. decodes live list metadata;
2. scans the complete canonical list-chunk prefix;
3. validates every reached chunk identity and envelope;
4. validates exact contiguous chunk coverage from `head_chunk` through
   `tail_chunk` and exact total element count;
5. tombstones every live chunk; and
6. tombstones list metadata in the same sorted copy-on-write B+tree batch.

An empty list publishes only the metadata tombstone. Missing, already retired,
wrong-family, malformed, noncontiguous, count-divergent, orphan, or
future/corrupt page state fails closed.

Current-state reconstruction recognizes retired list metadata only when all
reached chunks for that identity are tombstones. A live chunk below retired
metadata and a tombstoned chunk below live metadata are rejected unless the
live metadata and remaining chunks still describe the exact canonical list.
Checked recreation may replace the metadata tombstone while old chunk
tombstones remain unreachable to the new incarnation.

Chunk tombstoning does not immediately delete immutable blob objects.
Existing blob reachability collection remains the sole authority for later
physical blob reclamation.

## Concurrency

List creation, pushes, pops, and complete deletion publish the existing
whole-list conflict identity. This version intentionally does not admit
independent head and tail writers from the same snapshot.

- A push or pop prepared before an admitted deletion conflicts.
- A deletion prepared before an admitted push or pop conflicts.
- Two deletions prepared from the same incarnation conflict.
- Delete plus recreation publishes the same lifecycle identity, so a stale
  writer cannot attach to the replacement.

Transaction-wide first-committer-wins remains atomic: a conflict publishes no
subset of the list transaction.

## Durability, recovery, and compaction

Memory, Group, and Strict durability use the existing commit pipeline.
Interruption recovery at every singleton boundary exposes either the complete
prior list or the complete deletion/recreation publication.

Current-root structure compaction may drop list metadata and chunk tombstones
only after complete namespace validation. Page-generation vacuum must retain
the same current result and all pinned historical snapshots. Reopen may never
resurrect retired chunks or elements.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before `delete_list` exists;
- missing, empty, single-chunk, multichunk, blob-backed, and wrong-family
  behavior;
- private read-your-writes, retained snapshot, current-root physical, and
  reopened equivalence;
- same-transaction recreation as every implemented structure family,
  including a populated replacement list;
- deletion after earlier same-transaction pushes and pops;
- stale writer, writer-before-delete, duplicate-delete, and
  recreation-fence conflicts;
- all seven singleton commit interruption boundaries for deletion and
  deletion plus recreation;
- reached metadata, chunk identity, chunk envelope, contiguity, count, blob
  reference, and page corruption rejection;
- structure compaction, page-generation vacuum, blob reachability collection,
  and reopen without resurrection;
- direct-Linux release observations for empty, 64-element, and 2,048-element
  Memory and Strict deletion, with private preparation separated; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

List TTL is specified separately by
[Native whole-list TTL v1](native-list-ttl-v1.md). This lifecycle contract
does not add generic cross-family `DEL`, blocking operations, insertion by
index, trimming, moving, element mutation, batched push/pop, streams, protocol
compatibility, complete G3, or G7.

Complete deletion is cardinality-, chunk-, and payload-sensitive. Strict
durability includes physical synchronization. No surface receives a universal
microsecond latency promise.
