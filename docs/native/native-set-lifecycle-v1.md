# Native whole-set lifecycle v1

Status: implemented and directly verified on Linux; hosted checks pending.

Evidence:
[Native whole-set lifecycle evidence on Linux — 2026-08-03](../gates/evidence/native-set-lifecycle-linux-2026-08-03.md).

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md),
[Native whole-set TTL v1](native-set-ttl-v1.md), and
[Native set member commands v1](native-set-commands-v1.md). It exposes
explicit whole-set deletion and checked same-transaction recreation using
Hyphae's existing lifecycle mutation, WAL, MVCC, B+tree, and snapshot
authority.

## Surface

The embedded native write surface adds:

```text
DELETE_SET(key) -> bool
```

`DELETE_SET` requires the native B+tree structure format. It returns `true`
and prepares one whole-set lifecycle mutation when the transaction sees a
visible native set. It returns `false` without adding a mutation for a missing
or logically expired set. A live scalar, hash, list, or sorted set fails with
`StructureKindMismatch`.

Deletion is explicit source-data retirement, not cache eviction. It does not
read a wall clock, allocate another user-visible key, or route through a
compatibility protocol.

## Logical lifecycle

Successful deletion removes the complete private set incarnation: metadata,
all exact members, and whole-set expiry. `SISMEMBER`, `SMISMEMBER`, `SCARD`,
`SSCAN`, and set algebra observe the set as missing after the call.

Snapshots retained before commit continue to observe the complete prior
incarnation. Snapshots at or after the deletion commit observe a missing set.
No operation may expose a partially deleted member population.

A transaction may recreate the deleted user key as:

- an empty or populated new set;
- a scalar;
- a hash;
- a list; or
- a sorted set.

The recreation is a new typed incarnation in the same atomic publication.
Old members and expiry never reappear. Deleting and recreating the set, then
adding members, publishes only the newly added membership as visible state.

Member mutations prepared earlier in the same transaction may precede
`DELETE_SET`; the final visible state is still missing or the later recreated
incarnation. This version does not require command-level mutation coalescing,
so the WAL may retain semantically superseded member mutations.

## WAL and physical state

The implementation promotes the existing internal structure opcode:

```text
DELETE_SET = 34
```

No opcode or format changes. The mutation has engine `Structure`, no target,
the complete set key as `key`, an empty value, and no expiry.

Physical application:

1. decodes live set metadata and validates declared cardinality;
2. visits the complete exact-member prefix and validates every reached
   identity and member envelope;
3. verifies the live member count exactly;
4. tombstones every live member and the set metadata; and
5. tombstones the exact current set-expiry marker when metadata carries an
   expiry.

Missing, tombstoned, wrong-family, malformed, count-mismatched, or
expiry-marker-mismatched state fails closed. Replay accepts only the same
complete transition that physical commit accepts.

## Concurrency

`DELETE_SET` publishes the complete set lifecycle conflict identity.

- A member writer prepared before an admitted deletion conflicts and cannot
  resurrect the retired incarnation.
- A deletion prepared before an admitted disjoint member write may rebase,
  validate the admitted state, and retire that member with the complete set.
- Two deletions prepared from the same live incarnation conflict; only one
  publication succeeds.
- Checked recreation publishes the lifecycle identity, so stale member
  writers from the prior incarnation cannot attach to the replacement.

Transaction-wide first-committer-wins remains atomic: a conflict publishes no
subset of the deleting or recreating transaction.

## Durability, recovery, and compaction

Memory, Group, and Strict durability use the existing commit pipeline.
Interruption recovery at every singleton boundary exposes either the complete
prior set or the complete deletion/recreation publication.

Current-root structure compaction may drop deletion tombstones only after
complete namespace validation. Page-generation vacuum must retain the same
current result and all still-pinned historical snapshots. Reopen may never
resurrect retired metadata, members, or expiry markers.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before `delete_set` exists;
- missing, due, empty, populated, expiring, and wrong-family behavior;
- private read-your-writes, retained snapshot, current-root physical, and
  reopened equivalence;
- same-transaction recreation as every currently implemented structure
  family, including recreation as a populated set;
- deletion after earlier same-transaction member mutations;
- stale-member, admitted-member rebase, duplicate-delete, and
  recreation-fence conflicts;
- exact expiry-marker retirement;
- all seven singleton commit interruption boundaries for deletion and
  deletion-plus-recreation;
- reached metadata, member identity, member envelope, cardinality, expiry
  marker, and page corruption rejection;
- structure compaction, page-generation vacuum, and reopen without
  resurrection;
- direct-Linux release observations for empty, 64-member, and 2,048-member
  Memory and Strict deletion, with publication and synchronization separated
  where available; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

This contract does not add a cross-family generic `DEL`, relative or
conditional expiry, `PERSIST_SET`, member TTL, destination-set algebra,
pattern/reverse/random scans, command-level mutation coalescing, protocol
compatibility, complete G3, or G7.

Whole-set deletion is cardinality-sensitive because physical commit validates
and tombstones the complete live member prefix. Strict durability includes
physical synchronization. Neither surface receives a universal microsecond
latency promise.
