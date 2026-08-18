<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native page-generation vacuum v1

Status: normative experimental contract; current-root implementation, codec
coverage, crash matrix, and one Windows release observation are complete;
multi-generation retention, blob/WAL collection, scheduling, and G7 remain
pending

This protocol reclaims superseded native page bytes without weakening WAL
authority or pretending that logical reachability compaction shrinks the
append-only page file. V1 is an explicit, current-root retention policy. It
retains already materialized in-process snapshots for reads, retires physical
roots older than the vacuum commit, and rejects detached writers prepared
before that retention floor.

## Required invariants

1. A page generation is a positive `u64`. Generation one uses the historical
   `pages.hydb` filename. Later final files use
   `pages-NNNNNNNNNNNNNNNNNNNN.hydb`; their create-new stages append `.tmp`.
2. A `RootSet`, WAL commit manifest, and root checkpoint manifest bind both the
   page generation and a nonzero retention-floor CSN.
3. Ordinary commits inherit both values. A vacuum commit advances the page
   generation by exactly one and sets the retention floor to its own CSN.
4. Every commit at or above the latest retention floor references the latest
   page generation. Earlier WAL history remains authenticated input but its
   retired physical roots are not dereferenced during recovery.
5. The new generation is complete, verified, synchronized, and published under
   its final immutable filename before a WAL record can reference it.
6. The synchronized WAL commit is the generation publication point. A final
   generation without that commit is an orphan, not visible state.
7. The prior generation is removed only after the vacuum WAL commit is
   synchronized. Recovery accepts either the complete prior generation or the
   complete replacement generation, never a mixture.
8. The buffer pool keys frames by `(page_generation, page_id)`. A reused page
   ID can never resolve to a frame from a retired generation.
9. Vacuum changes physical identity and retention only. Catalog definitions,
   current relational rows and indexes, structures, lexical state, ANN state,
   blob references, catalog version, and logical results remain equal.
10. A vacuum that cannot reduce physical page count is a no-op: it writes no
    WAL transaction, advances no CSN, and publishes no generation.

V1 does not implicitly retain historical snapshots after process restart.
[Durable snapshot pins v1](snapshot-pins-v1.md) now provide explicit,
identity-bound multi-generation retention: vacuum preserves every generation
named by a stable pin, while unpinned operation keeps this current-root policy.
A `NativeSnapshot` already materialized before vacuum remains usable because
it owns its logical state. A detached `NativeWriteBatch` whose read CSN
precedes the new floor fails explicitly instead of rebasing across retired
history.

## Current-root rewrite

Vacuum first validates the complete current root set and materializes the
current logical state as a differential oracle. It then builds an empty
create-new page generation:

- the catalog root payload is copied canonically into a new catalog page;
- legacy inline relational, structure, and search roots are copied as new
  pages of the same kind;
- B+tree structure and search entries are scanned in canonical order and bulk
  rebuilt into the candidate generation;
- relational V1 B+tree entries are bulk rebuilt byte-for-byte;
- for relational V2, every B+tree entry is retained, but each row pointer is
  replaced with a pointer to one newly written current head version;
- a retained V2 head keeps its exact canonical `RowRecord` and original begin
  CSN, drops its `next` link, and therefore retires older version pages; and
- blob references remain content addressed and unchanged. Blob garbage
  collection is a separate gate.

The candidate root set is fully traversed and materialized. Its logical state
must equal the source state before publication. Candidate root pages may use
the vacuum commit CSN; retained V2 head pages use the row begin CSN required by
the version-chain format.

## WAL commit manifest v2

Ordinary generation-one commits keep the exact `HYCMT001` 124-byte body for
disk compatibility. A commit that carries non-default storage state uses
`HYCMT002`, a 140-byte body:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYCMT002` |
| 8 | 8 | read CSN or zero |
| 16 | 8 | commit CSN |
| 24 | 8 | catalog version |
| 32 | 8 | blob generation |
| 40 | 4 | mutation count |
| 44 | 8 | mutation bytes |
| 52 | 8 | logical time in microseconds |
| 60 | 32 | mutation digest |
| 92 | 32 | four ordered page IDs |
| 124 | 8 | page generation |
| 132 | 8 | retention-floor CSN |

`HYCMT001` decodes as page generation one and retention floor one. Recovery
rejects zero generations/floors, a floor newer than its commit, generation or
floor regression, a generation transition whose floor is not that transition
commit, and any retained commit that references another generation.

The vacuum transaction contains exactly one kernel mutation with opcode
`VacuumPageGeneration`. It has no target, key, value, or expiry. It changes no
logical engine state and claims one global maintenance write identity.

## Root manifest storage state

The standalone historical codec uses `HYROOT02` after a generation transition.
Its 192-byte header retains the v1 root-entry payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYROOT02` |
| 8 | 2 | format version `2` |
| 10 | 2 | header length `192` |
| 12 | 4 | reserved zero |
| 16 | 8 | manifest generation |
| 24 | 8 | visible CSN |
| 32 | 8 | catalog version |
| 40 | 8 | committed WAL LSN |
| 48 | 8 | blob generation |
| 56 | 4 | root-entry count |
| 60 | 4 | root payload length |
| 64 | 4 | CRC32C |
| 68 | 12 | reserved zero |
| 80 | 32 | committed WAL block digest |
| 112 | 32 | previous manifest digest |
| 144 | 32 | complete manifest digest |
| 176 | 8 | page generation |
| 184 | 8 | retention-floor CSN |
| 192 | variable | canonical root entries |

Native directories always publish `HYROOT03`, including at generation one.
Its 216-byte header preserves these page-generation and retention-floor
offsets, then adds the 24-byte directory lineage before the root payload.
`HYROOT01` and `HYROOT02` remain byte-identical decodeable historical formats,
but native-marker recovery rejects them as authority. Manifest-chain digests
therefore bind every storage transition and its exact directory lineage.

## Publication and cleanup order

For a strict vacuum:

1. reserve the next CSN and generation under the serialized writer guard;
2. validate and rewrite the complete current root set to the temporary file;
3. synchronize candidate page bytes;
4. rename the stage to its immutable generation filename and synchronize the
   data directory where supported;
5. append the v2 transaction's `BEGIN` and vacuum mutation as a recoverably
   abortable precommit;
6. append the terminal v2 `COMMIT` record and synchronize the WAL;
7. publish the new `RootSet` in memory;
8. replace the active buffer pool and page-store handle; and
9. remove prior final generations and synchronize the directory where
   supported.

Recovery reads and authenticates the WAL before choosing a page file. It opens
the latest committed generation, verifies every retained commit/root, validates
checkpoints, then removes canonical temporary files and unreferenced final
generations. Unknown page-generation filenames fail closed.

V1 vacuum is always strict. `Memory` or grouped acknowledgement cannot describe
retirement of the only prior physical generation.

## Deterministic interruption matrix

The test-only vacuum entry point interrupts after:

1. candidate page bytes are synchronized under the temporary name;
2. the immutable candidate generation is published without a WAL reference;
3. the vacuum WAL precommit is appended without a terminal commit;
4. the vacuum WAL transaction is synchronized;
5. the replacement root set is published in memory; and
6. the prior generation is removed.

Reopen must return the complete prior state for boundaries 1 through 3 and the
complete vacuumed state for boundaries 4 through 6. Retrying after a
pre-publication recovery performs one vacuum. Retrying after a committed
vacuum is a no-op unless newer writes created reclaimable pages.

## Acceptance evidence

The current implementation proves:

- codec goldens and malformed/truncated cases for page generation,
  `HYCMT001` compatibility, `HYCMT002`, `HYROOT01`, and `HYROOT02`;
- a buffer-pool regression proving equal page IDs from different generations
  cannot alias;
- cross-engine logical equality before, immediately after, and after reopen;
- exact current-row V2 chain truncation without changing current row bytes;
- rejection of a detached writer older than the retention floor;
- corruption rejection before WAL publication;
- all six interruption boundaries with prior-or-complete recovery;
- exact page/file bytes before and after on a corpus that contains relational
  history, structures, lexical state, and ANN generations;
- a no-op case with no CSN, WAL, or generation movement; and
- vacuum latency reported separately from point-operation and fsync latency.

The [2026-08-02 evidence](../gates/evidence/native-page-vacuum-2026-08-02.md)
binds those results to exact source and a machine-readable Windows release
observation. Structure tombstone removal remains the separate reachability
compaction gate; vacuum preserves every entry reachable from its captured
current root.

This vertical closes neither G1 nor the complete retention program. Durable
snapshot pins now retain multiple explicitly named restartable generations,
but this vacuum protocol still does not collect blobs, rewrite or truncate WAL
blocks, schedule background retention policy, define quotas, or establish
interference and p99.9 gates.
