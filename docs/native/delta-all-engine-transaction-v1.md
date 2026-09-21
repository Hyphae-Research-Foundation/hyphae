<!-- SPDX-License-Identifier: Apache-2.0 -->

# Native delta all-engine transaction v1

Status: the base point-resolved execution is implemented and verified by
[`native-delta-all-engine-transaction-linux-2026-08-03.md`](../gates/evidence/native-delta-all-engine-transaction-linux-2026-08-03.md).
The current worktree additionally gives the point-resolved SQL, scalar,
lexical, named-vector upsert/delete/absence-fence, and exact-field V3 Hash
slices a conservative batch-wide retained-memory ledger under the 32 MiB
mutation allocation.
Hash-field state also has an 8 MiB sub-budget inside that parent bound. The
linked evidence predates this ledger and the vector slice; exact-SHA phase
qualification remains required. This is not allocation-exact or complete P6
evidence, and G7 remains open.

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
- a named-vector mutation overlay containing only each target's metadata,
  bounded exact object/path intents, and physical-or-conflict-only disposition,
  never its immutable HNSW base;
- a conservative retained-memory ledger covering the in-scope catalog,
  relational, scalar, lexical, vector-delta, mutation, identity, and container
  capacities;
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
operations. SQL, scalar, lexical, named-vector, and V3 Hash-field staging
preflight the candidate against the aggregate parent ledger. A rejected stage
restores any private hydration and leaves earlier staged operations
committable. Hash-field staging must additionally fit its identity, envelope,
mutation, and retained payload inside the 8 MiB Hash sub-budget. These are
conservative checked bounds; they must not be described as exact request-plan
or RSS accounting.

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

The typed Native v2 transaction search-mutation registry is append-only:

| Tag | Variant | Minimum negotiated protocol minor |
|---:|---|---:|
| 0 | `Index` | 0 |
| 1 | `Replace` | 0 |
| 2 | `Delete` | 0 |
| 3 | `Document` | 7 |

`Document` requires minor 7 regardless of which doc-value variants its
`ProductDocument` contains. Encode and dispatch at minor 6 or earlier reject it
as unsupported. The three historical variants retain their bytes.

### Named-vector point mutation and absence fencing

`stage_delta_upsert_vector` resolves one catalog-bound vector index, reads its
metadata, the addressed D01/D02 values, and the exact 32-node sparse-Merkle
path. It never scans the D01 prefix or restores the immutable single or
partitioned HNSW base. Planning charges conservative metadata, vector, path,
copy-on-write, and recovery-memory bounds before the intent is retained. A
second staged operation for the same object is rejected without losing the
first.

`stage_delta_vector_absence_fence` resolves the same bounded point authority.
If D02-first, D01-second, base-last lookup finds a live vector, staging records
`DeleteVector` opcode 19 and retains the authenticated path for a physical D02
tombstone. If the object is already absent, staging records
`FenceVectorAbsence` opcode 57 and retains conflict authority only. Prior
absence requires the D02 leaf or authenticated non-membership path to agree
with the overlay root, the exact D01 value to be absent or a tombstone, and the
selected base-child vector keys to be absent when neither delta layer names the
object. A delta tombstone suppresses lower layers; duplicate base ownership is
corruption. Admission and recovery repeat the proof against the admitted and
committed-prior roots respectively.

Integrated complete-image callers apply this rule to every bound named-vector
target. Insert and replacement upsert each supplied vector and invoke
delete-or-fence for each omission; complete document deletion invokes
delete-or-fence for every target, including targets omitted by the prior image.
Consequently an already-absent omission still conflicts with a same-object
concurrent upsert instead of silently leaving that upsert live.

Commit assigns sequence and CSN to physical upserts and deletes under writer
admission, freezes a first M04 D01 view without scanning or rewriting it, and
writes one D02 leaf, its 32 ancestors, the manifest, and M05 metadata. A delete
leaf is a tombstone. Exact search observes the resulting effective set
immediately, while ANN continues to search the unchanged base and merge the
effective layered delta. Conflict-only opcode 57 writes none of those physical
records and consumes no M05 sequence or delta slot. Physical point transactions
validate object and generation authority but publish only object authority, so
disjoint batches from one snapshot may rebase and same-object batches conflict.
Absence fences validate and publish object and lifecycle-fence authority.
Materialized ordinary upserts and present-object deletes after
creation use the same writer; creation and vectors in that same transaction
retain canonical M04 base publication.

Full validation retains one process-local recovery-memory authority for the
current search root: exact lexical bytes plus exact per-index ANN charges.
Point admission remeasures lexical bytes with the allocation-free borrowed
lexical range, substitutes only touched-index projected charges, and rejects a
shared total above 64 MiB before any page or WAL append. Unchanged ANN indexes
inherit their source-root charges; no root-wide ANN metadata/catalog scan is
performed. The retained exact path charge covers the transient point plan and
copy-on-write publication copies. Initial-bulk and consolidation publication
substitute their exact candidate metadata charge into this same authority and
reject an over-limit candidate before page creation.

The shared aggregate cap is conditional on M05 authority. A complete root that
contains only M01 through M04 remains accepted through its historical bounded
streaming load even when its aggregate lexical-plus-ANN estimate exceeds 64
MiB, and M04 initial-bulk publication remains on that grandfathered format. A
point publication or C02 consolidation that would make the first M04-to-M05
transition projects the complete candidate charge, activates the shared cap,
and fails before page or WAL growth when the candidate is too large; the
original M04 root remains readable and unchanged.

Upsert and physical-delete accounting includes complete retained state and
post-dequeue scratch. Let `V = 4 * dimension`; one maximum encoded sparse-path
key/value pair is `34 + 56 + 16 * 32 = 602` bytes. One upsert intent
conservatively charges:

```text
2 * V + 4 * 32 * 602 + 4,096 +
BTree::sorted_batch_structural_memory_bound(35)
```

This covers the staged and encoded vector, four path/publication copies, fixed
point state, and the largest metadata/manifest/leaf/path B+tree batch structure.

A physical delete uses the observed authenticated path. It charges the exact
capacities of captured D01 and D02 values and all 32 optional encoded node
values; its path container, option slots, point intent, allocation bookkeeping,
and fixed 256-byte structural allowance; the largest simultaneously live
decode, selected-base lookup, node/hash, and path-index proof workspace; and
`BTree::sorted_batch_structural_memory_bound(35)`.

An absence-fence intent retains no physical path or replacement and costs 256
ledger bytes for its bounded conflict/proof plan. The first physical intent for
an index additionally adds the one 2 MiB (`2,097,152`-byte) recovery reserve;
later physical intents for that index do not add it again. Per-index metadata
authority is separately `2 * encoded_metadata_length + 4,096` bytes. Every
capacity, container-growth, proof, replacement, and recovery term uses checked
arithmetic and is replayable from the retained batch.

Those ANN intent charges do not replace ordinary mutation accounting. An opcode
18, 19, or 57 mutation also contributes its retained 16-byte object key, exact
value capacity, 192-byte mutation overhead, and exact share of mutation-vector
capacity. Staging first authenticates the point, chooses upsert, physical
delete, or fence, constructs the corresponding exact structural plan, and then
replays the complete ledger before accepting the operation. Any overflow or
parent-capacity failure restores the prior authority, mutation length, and
ledger exactly.

Queue retention is not another estimate or a fixed 32 MiB reservation. Before
a delta batch enters the scheduler, the runtime replays the complete
catalog/engine/overlay/mutation ledger and requires byte equality with the
stored value. That ledger already includes each point's authentication and
publication structural scratch above. It does not include the independently
preflighted WAL encoder and cloned-payload peak.

The queue permit is exactly:

```text
{compute_threads: 0, io_slots: 0,
 memory_bytes: max(1, checked_add(replayed_retained_ledger,
                                  wal_publication_peak))}
```

Point paths, vectors, one-time recovery authority, fences, keys, mutation
capacity, and point scratch remain live while WAL record vectors and cloned
key/value payloads are encoded. The WAL peak is therefore additive, not a
replacement or `max` of retained memory. Commit repeats both preflights and
subdivides the WAL peak from the combined queue permit before any publication;
no uncharged point scratch is reacquired after dequeue. These are exact values
in the conservative ledger model, not allocator-RSS claims.

Product error conversion preserves the resource failure type. An
`AnnDeltaLimitExceeded` or direct/queued governor `ParentCapacity` rejection is
`LimitExceeded`/`Limit`/`Never`; direct or queued global/class-capacity
rejection, queue-full, and queue-timeout are
`Unavailable`/`Unavailable`/`AfterBackoff`. Raw runtime entrypoints continue to
return their native typed errors.

Point vector commits append one fixed 184-byte `HYANNA02` WAL authority body per
physically changed target index after ordinary mutations, in ascending index
order. Opcode 56 binds the duplicated index, exact physical operation count,
prior/result view and Merkle root identities, prior/result `next_sequence`, and
the M04-to-M05 flag. Its count includes opcode 18 upserts and physical opcode 19
deletes, but excludes opcode 57 fences. Thus a mixed index has one marker whose
count and sequence advance equal only its physical members; a fence-only index
has no marker. Markers are unique, trailing, ordered, exact-covering, and cannot
mix with HYANNA01. A transaction may contain at most 4,096 opcode 57 records in
total, independently of the per-index physical marker count.

Recovery replays each retained physical path transition against prior/result
committed roots before reconstructing the same object/generation conflict
authorities as live admission. Target discovery streams the bounded physical
difference across all ANN prefixes, including empty/nonempty root pairs, rather
than trusting WAL or metadata-only changes. Every point-writer-changed index
with M05 on either side requires an exact HYANNA02 marker,
physical-mutation cross-check, and per-index transition proof. Opcode 50 uses
HYANNC02 and its separate exact replacement proof. Fence targets are added from
opcode 57 even though they are absent from that physical difference. Recovery
proves their prior
absence, requires no physical transition for each fenced object/path beyond
co-committed physical point mutations, and reconstructs the object and
lifecycle-fence write keys. Legacy unmarked opcode 19 and HYANNA01 M01-M04 WAL
retain their historical semantics.

Group admission carries the projected ANN metadata version, point replacement
values, and per-index recovery charge for every accepted cohort member before
storage starts. Same-index points are planned and charged against the preceding
accepted member, not the original root. Materialized legacy charges use the
sequential candidate state. A physical point publication changes that private
layout to M05. Disjoint physical points and absence fences compose in either
cohort order. A stale same-object point conflicts, and the absence fence's
lifecycle key prevents stale initial-bulk or consolidation publication from
crossing it. Only accepted compatible members reach page or WAL staging.

M05 consolidation is a separate bounded maintenance writer. It publishes
opcode 50 with fixed 384-byte HYANNC02. Its M04/M05 capture flag,
`captured_next_sequence`, record count, and ordered effective-record digest bind
the complete captured delta, including index, object, sequence, kind, mutation
CSN, and vector. Publication reconstructs the exact physical below-bound
capture, consumes current effective records still at their captured sequences,
preserves later records and publication-time `next_sequence`, and emits
canonical overlay-only M05. A later D02 shadowing a captured D01 is valid because
the D01 remains reconstructable; overwriting a captured D02 or D01 capture is
stale. C02 is completely encoded before any page append. Historical fixed
112-byte HYANNC01 remains authoritative only for original M01-M04 recovery,
including legacy M01-M03 consolidation upgrades to M04, and is never emitted.
C02's ordered captured-record digest includes the index. Its canonical
effective-vector-set digest independently includes the same index before its
count and commutative accumulators, and the fixed body duplicates that target.
C02 accepts only M04/M05 capture and M05 result
formats. Initial-bulk rewriting of M05 remains fail closed, and background
scheduling remains outside this writer.

Consolidation publication memory is one checked additive peak covering target
hydration, any replacement base not already held by its retained build permit,
B+tree structural work, caller-owned replacement payload, every replacement
key/value clone, and the independently preflighted WAL encoder and cloned C02
payload. Because the B+tree structural plan excludes payload ownership, the ANN
contract separately charges the retained capacities of every replacement key
clone and value clone, plus any source replacement set live at the same time.
None of those owners may be replaced by a maximum-of calculation. Clones cannot
be created before admission; overflow or a replayed charge mismatch rejects
before page or WAL publication.

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
8. Encode the canonical WAL transaction, including any delta ANN authority
   markers.
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

For a physical point delete, the seven cuts expose either the prior live vector
or the complete authenticated tombstone with its commit authority. For a pure
opcode 57 transaction, cuts through `PageSynchronized` expose the prior commit;
cuts from `WalAppended` through `RootPublished` expose the durable transaction
ID, advanced CSN, and conflict authority while retaining the exact prior four
root page IDs, blob generation, and catalog version. A mixed transaction follows
the same all-engine old-or-complete rule.

## Explicit non-goals

This slice does not:

- add joins, scans, DDL, prepared DML, or transaction-private reads;
- validate inbound or outbound SQL foreign keys in a delta batch;
- expose aggregate or scanning Hash reads on a delta batch;
- rewrite M05 through initial-bulk publication;
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
- consolidation publication admission includes every caller-owned replacement
  key/value clone as well as structural memory and rejects before cloning on an
  understated or overflowing peak;
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
  any hot-path complete-state materialization; the guard instruments both
  `load_state` and the ANN `apply_tree_mutations` full `load_from_tree`
  fallback;
- delta ANN WAL marker decoding is fixed-size and bounded, recovered stale
  delta-index histories are rejected, and legacy disjoint materialized writes
  still rebase and reopen;
- M01, M02, and M03 ANN metadata remain writable through the frozen legacy path
  and atomically upgrade to M04 on the first accepted vector mutation, while
  reads and rejected writes preserve the original bytes and trailing or
  inconsistent lengths are rejected;
- one committed delta vector's creating CSN equals its commit receipt CSN and
  its persisted view identity equals the recomputed base-plus-delta identity;
- unfiltered ANN evidence executes `GraphTraversal` over a non-empty HNSW base,
  merges an exact delta hit, and remains identical after reopen;
- point deletion suppresses base, D01, and D02 values in exact and ANN search,
  including reopen, while repeated materialized upsert/delete order remains
  last-operation-wins;
- opcode 57 proves prior absence, changes no ANN page, view identity, sequence,
  or delta slot, and the 4,096-fence transaction bound rejects the next fence
  without losing earlier staged mutations;
- mixed fences and physical points count only physical members in HYANNA02,
  same-object fence/upsert races conflict in both orders, and disjoint group
  members compose in either order;
- recovery rebuilds object/generation/lifecycle conflict keys and every physical
  delete or pure-fence crash cut reopens to the specified old-or-complete
  authority;
- HYANNC02 consolidation accepts only M04/M05 capture and M05 result formats,
  reconstructs both index-bound record and vector-set digests, preserves
  captured-D01/later-D02
  shadows, rejects captured-layer overwrites, preserves later records and
  `next_sequence`, emits the canonical D02-only overlay above the replacement
  base, proves complete retained vector/graph generations, and reopens old or
  complete at every strict cut;
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
