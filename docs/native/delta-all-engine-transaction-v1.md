<!-- SPDX-License-Identifier: Apache-2.0 -->

# Native delta all-engine transaction v1

Status: the base point-resolved execution is implemented and verified by
[`native-delta-all-engine-transaction-linux-2026-08-03.md`](../gates/evidence/native-delta-all-engine-transaction-linux-2026-08-03.md).
The current worktree additionally gives the point-resolved SQL, scalar,
lexical, and exact-field V3 Hash slices a conservative batch-wide
retained-memory ledger under the 32 MiB mutation allocation. Hash-field state
also has an 8 MiB sub-budget inside that parent bound. The linked evidence
predates this ledger; exact-SHA phase qualification remains required. This is
not allocation-exact or complete P6 evidence, and G7 remains open.

This contract replaces the materialized hot path behind the local
SQL-plus-structure-plus-search transaction with a Hyphae-owned physical delta
batch. The transaction keeps the protocol, durability classes, conflict
rules, WAL record sequence, root publication, transaction identity, and
single-CSN guarantee fixed.

The change is architectural, not a transport optimization. The existing
baseline stages one operation in roughly the same microsecond domain as a UDS
`PING`, but a memory commit is measured in milliseconds because both `BEGIN`
and writer admission materialize the complete all-engine state. A stable SQL
update also traverses the complete historical row-version chain while
rebasing and again while constructing the new relational root.

## Baseline and observed cause

The sealed direct-Linux baseline is
[`native-local-all-engine-transaction-linux-2026-08-03.md`](../gates/evidence/native-local-all-engine-transaction-linux-2026-08-03.md).
Its median run reports:

- `PING` p50: 23.853 microseconds;
- SQL stage p50: 24.415 microseconds;
- structure stage p50: 22.364 microseconds;
- search stage p50: 24.271 microseconds;
- memory commit p50: 6.475748 milliseconds; and
- strict commit p50: 15.097443 milliseconds.

A symbolized `perf` observation on the same AWS Linux host attributes 37.02%
of sampled server CPU to `begin_optimistic -> load_state`, another 18.24% to
the writer-admission `load_state`, and 18.16% to `commit_engine_roots`.
Within root construction, `decode_relational_chain` accounts for 13.20%.
These percentages are diagnostic observations over the complete harness, not
independently subtractable latency measurements.

## Product invariant

Local transactional work must scale with the touched keys, affected index
entries, B+tree height, generated pages, and WAL bytes. It must not scale with
the total number of unrelated rows, structures, lexical documents, vectors,
or historical versions.

The default local transaction therefore may not use `MaterializedState` or
call the full `load_state` path at `BEGIN`, during staging, or at commit
admission.

Full-state decoding remains an integrity, recovery, migration, vacuum, and
explicit verification surface. Removing it from the hot path must not weaken
open-time corruption detection or page verification on a buffer-pool miss.

## Delta batch

The implementation introduces one detached native delta batch captured from
one immutable `Snapshot`. It owns no file descriptor and holds no writer
guard between requests. It remains bound to the exact live `NativeDatabase`
handle that created it. Staging or committing through a peer handle, a handle
for another directory, or a reopened handle fails closed; dropping the owner
invalidates its detached batches.

The batch contains:

- the snapshot root set, read CSN, fixed logical time, and durability class;
- the ordered canonical mutation list used by WAL encoding;
- the complete read and write validation-key sets;
- a bounded catalog cache containing only definitions required by staged
  operations;
- a relational overlay keyed by relation and encoded primary key;
- a scalar-structure overlay keyed by binary structure key;
- a lexical overlay keyed by search collection and document ID;
- a conservative retained-memory ledger covering the in-scope catalog,
  relational, scalar, lexical, mutation, identity, and container capacities;
  and
- an 8 MiB retained sub-ledger for the V3 Hash-field slice.

The ledger uses retained input capacities and conservative container and
mutation overheads, not an allocation-exact heap measurement. It fails closed
when the computed upper bound exceeds the batch's admitted parent memory,
which is at most 32 MiB. Saturating arithmetic is rejection, not permission.
Commit replays the same retained-memory model and rejects a mismatched or
over-cap batch before blob, page, or WAL publication.

Overlay entries distinguish missing, live, expired, deleted, and replaced
states. A later operation in the same transaction resolves against the
overlay before the immutable snapshot. This preserves sequential private
semantics without materializing unrelated data.

The batch retains the existing limit of 1,024 successfully staged local
operations. SQL, scalar, lexical, and V3 Hash-field staging preflight the
candidate against the aggregate parent ledger. A rejected stage restores any
private hydration and leaves earlier staged operations committable. Hash-field
staging must additionally fit its identity, envelope, mutation, and retained
payload inside the 8 MiB Hash sub-budget. These are conservative checked
bounds; they must not be described as exact request-plan or RSS accounting.

`NativeDatabase::begin_optimistic_delta` returns the opaque
`NativeDeltaWriteBatch` authority. It does not dereference, borrow, or convert
back to the materialized `NativeWriteBatch`, so materialized reads and mutators
are absent at compile time. Delta mutation is available only through the
`NativeDatabase::stage_delta_*` surfaces, and delta callers use the explicit
`Result`-returning point APIs. Internal mode and shape guards remain mandatory
defense in depth.

Detached commit entrypoints consume either batch kind through the opaque
`NativeCommitBatch` envelope. Homogeneous singleton and group calls convert
implicitly. A mixed materialized/delta group converts each member explicitly;
the envelope exposes no operation or conversion back to either batch kind.

## Point-resolved staging

The first delta slice supports exactly the already-public local transaction
operations.

### SQL DML

`INSERT`, `UPDATE`, and `DELETE` retain the current typed SQL grammar and
exact-primary-key requirement. Planning resolves only:

- the named relation definition;
- secondary-index definitions owned by that relation;
- the addressed primary row;
- old and new secondary projections; and
- exact uniqueness probes required by those projections.

Planning may traverse catalog and secondary-index B+tree paths, but it may not
scan an unrelated relation or reconstruct a `RelationState`.

SQL delta staging requires a `HYCAT006` catalog root. An older catalog format
is rejected as an invalid prepared mutation; staging does not rebuild or
upgrade it implicitly. The current named-column, parameter, type, nullability,
and uniqueness semantics remain in scope. Relations with either outbound or
inbound foreign-key dependencies are deliberately unsupported by this delta
slice and fail closed before mutation. They must use a separately supported
path until bounded foreign-key validation is implemented.

For a latest-snapshot update, only the version-chain head required to decode
the current row is visited. Historical snapshot reads may follow older links
until the first visible version. Hot reads do not validate an unreachable
historical suffix; explicit integrity and recovery paths still validate the
complete chain.

### Scalar structure SET

`SET` resolves the exact scalar key and kind metadata from the captured
structure root. It preserves the current scalar-versus-hash/set/list/sorted
set collision rules and computes absolute expiry from the one logical time
sample captured at `BEGIN`.

For both `HYSTRBT2` and `HYSTRBT3`, replacement resolves the durable scalar
envelope without reading or retaining the old payload, including an old blob.
Only the addressed structure key, the replacement payload, and its
expiry-index entries may be read or changed.

### V3 Hash points

`HSET`, `HDEL`, and `HINCRBY` resolve one V3 Hash field without materializing
the collection. `NativeDatabase::delta_hget` and
`NativeDatabase::delta_ttl_hash_field` are the exact read-your-writes surfaces
for its value and field TTL. `NativeDatabase::delta_ttl_hash` reads exact V3
point metadata for the whole-Hash TTL, validates the typed identity and
backlink, and returns missing, persistent, or remaining time without building
a partial Hash map. Field `HSET` and `HINCRBY` preserve that whole-Hash TTL.
Aggregate and scanning Hash reads remain unsupported on a delta batch.

### Exact lexical document lifecycle

Create, replacement, and deletion staging resolve the exact search collection
definition and document identity. Create rejects an already-live identity;
replacement and deletion require one. Create and replacement tokenize the
supplied text once and record only the document metadata, document terms, term
metadata, and posting deltas required for that identity. Sequential lifecycle
operations resolve through the private overlay, including replace-delete-create
and create-delete sequences, without loading unrelated documents.

Document identities are never renamed. Deletion followed by creation may reuse
the same exact identity; the benchmark must continue to disclose lexical
identity growth.

## Commit admission and publication

Commit consumes the delta batch.

1. Acquire the existing native writer admission.
2. Reject a read CSN below the retention floor.
3. Validate the complete read/write conflict set with first-committer-wins.
4. Re-resolve only staged point identities against the admitted root set.
5. Reject any semantic divergence without appending pages, blobs, or WAL.
6. Apply relational, structure, and search deltas to their admitted B+tree
   roots with copy-on-write page mutation.
7. Stage and publish only large values referenced by admitted mutations.
8. Encode the existing canonical WAL transaction.
9. Apply the selected page/WAL synchronization policy.
10. Publish all changed roots once through the existing commit coordinator.

There is no second coordinator, per-engine commit, internal TCP/HTTP/JSON
path, or compatibility database. A successful receipt still carries one WAL
`TransactionId` and one commit CSN for all three engines.

Disjoint stale batches rebase onto the admitted roots. A conflicting batch
fails atomically. Rebase work is proportional to the staged validation and
mutation sets; it may not call `load_state`.

## Failure and crash semantics

Stable local failure codes and active/idle state transitions remain those in
[`local-all-engine-transaction-v1.md`](local-all-engine-transaction-v1.md).
The delta implementation must preserve:

- semantic-stage failure without losing earlier staged operations;
- rollback, close, peer-loss, and transport-loss discard with no durable ID;
- exact expected-operation-count checks;
- conflict consumption at commit;
- no partially published loser;
- prior-snapshot invisibility;
- reopen equivalence; and
- the existing seven commit crash boundaries.

The authoritative recovery cut remains:

- interruption through `PageSynchronized` reopens the prior state; and
- interruption from `WalAppended` through `RootPublished` reopens the
  complete new state.

No boundary may expose a mixed engine state.

## Explicit non-goals

This slice does not:

- add joins, scans, DDL, prepared DML, or transaction-private reads;
- validate inbound or outbound SQL foreign keys in a delta batch;
- expose aggregate or scanning Hash reads on a delta batch;
- make lexical document identities mutable;
- change group durability;
- remove full validation from recovery or explicit verification;
- bypass CRC32C/BLAKE3 verification on a page-cache miss;
- promise microsecond fsync or universal sub-millisecond commits;
- introduce a sidecar, compatibility engine, provider, LLM, or cloud service;
  or
- delete the materialized transaction path before its remaining callers are
  migrated and independently gated.

## Verification gates

The implementation is not complete until all of the following are sealed.

### Red gate

A compiler-reaching test target must fail before the delta API exists. The
test must exercise the public local transaction path rather than a private
benchmark-only helper.

### Deterministic correctness

- exact mutation, conflict-key, and overlay canonicality tests;
- replayed conservative memory-ledger equality and parent-capacity rejection;
- single-engine, hidden-capacity, and mixed SQL/scalar/lexical/Hash memory
  rejection before mutation, with earlier stages still committable;
- V2 and V3 scalar replacement without hydrating an oversized old payload;
- `HYCAT006` SQL admission and fail-closed inbound/outbound foreign-key cases;
- same-live-handle batch ownership across staging, commit, drop, and reopen;
- the public delta type cannot access materialized reads or mutators, while
  internal mode guards still prevent bypassing delta staging or its ledger;
- exact whole-Hash TTL for persistent, due, missing, and wrong-kind V3 points,
  preserved across staged field writes without a full-state load;
- later-in-batch read-your-prior-write semantics for each engine;
- semantic failure leaves the earlier overlay and operation ordinal intact;
- latest SQL update touches only the row-version head;
- a historical snapshot follows only as far as its first visible version;
- explicit full verification still rejects corruption in an older linked
  version;
- local `BEGIN`, stage, and commit succeed under a test guard that rejects
  any hot-path `load_state` call;
- unrelated row, structure, document, and version population does not change
  the number of point identities admitted for the same three-operation
  transaction;
- one transaction ID, one CSN, prior-snapshot invisibility, reopen, conflict,
  rollback, close, and peer-loss proofs remain green; and
- all seven process interruption boundaries remain never-mixed.

### Performance evidence

Evidence runs directly on `mario@10.77.10.10` from
`/home/mario/Hyphae-Research-Foundation/hyphae`, never through WSL.

The evidence must include:

- exact implementation, harness, binary, raw-output, and environment hashes;
- at least three valid pinned-CPU release executions;
- unchanged PING and per-engine stage distributions;
- memory and strict commit distributions without percentile subtraction;
- a stable-row SQL depth sweep at 1, 32, 256, and 1,024 prior versions;
- a population sweep that grows unrelated rows, structures, and documents;
- page-read, page-append, WAL-byte, allocation, and full-state-load counters;
  and
- a symbolized CPU profile with lost-sample count.

The deterministic gate is zero hot-path full-state loads and bounded
point-identity work. The latency receipt is reported honestly against the
sealed baseline. It does not become a G7 microsecond pass unless the measured
commit itself reaches that domain without hiding queueing, execution, or
durability cost.
