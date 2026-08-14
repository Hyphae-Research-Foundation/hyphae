# Native structure execution v1

Status: experimental P6 foundation; no P6 or G7 closure claim

This contract records the current Hyphae-owned execution substrate for scalar,
hash, set, list, sorted-set, and stream operations. Familiar command semantics
do not introduce Valkey, RESP, a compatibility sidecar, or a second durability
authority. Every operation remains bound to native pages, WAL, MVCC, commit
sequencing, governor admission, backup, and recovery.

## Direct and segmented paths

Scalar and collection point operations use direct current-root B+tree lookups
under `ForegroundPoint` admission. Bounded collection ranges retain direct
visitors for small work and switch to immutable leaf-segment plans when the
declared range or result size crosses 256 entries. Hash, set, stream, list, and
sorted-set receipts expose the captured CSN, declared logical cardinality,
planned leaves and physical entries, admitted workers, and executed worker
batches.

The `HYSTRBT3` current-root surface now covers all 30 logical structure
commands exposed as 52 public Rust methods. `LRANGE`, `XRANGE`, Set algebra,
and Sorted Set rank/rank-range/score-range execution resolve only current
incarnation metadata and children. Full-range paths verify declared counts,
logical List bytes, Stream terminal ID, and Sorted Set membership/order pairs;
partial paths reject every reached malformed identity or pair without claiming
global validation. Retained snapshots and transaction-local reads are not yet
part of this result because those surfaces still materialize complete state.

Segment workers use frozen page/blob readers and only subdivide one parent
governor permit. Results merge in canonical collection order, independently of
worker completion order. No scan creates a private thread pool or acquires a
global reader mutex.

## Multi-key and mutation bounds

Hash-field and set-member mutation batches are capped at 4,096 positions and
fail before publication when the complete request cannot be admitted. Set
algebra has independent key, visit, and output ceilings. Optimistic conflict
keys distinguish unrelated hash fields and set/sorted-set members while whole
list operations intentionally serialize on list identity.

All accepted mutations still publish through the shared all-engine commit.
Commit receipts separate complete execution, WAL append, page synchronization,
and WAL synchronization time. Group durability reports cohort size and exact
flush sharing rather than presenting append time as durable latency.

## Expiry and maintenance

The ordered expiry namespace covers scalar, hash, set, list, stream,
sorted-set, and hash-field identities. One sweep retires at most 4,096 due
identities, reports `more_due`, writes nothing for an empty sweep, and executes
as governed maintenance. The active expiry scheduler bounds foreground work,
guarantees maintenance progress under load, and retains terminal failures for
operator inspection.

Reachability compaction drops current tombstones into a replacement immutable
root and reports scanned, retained, dropped, page, and commit counts. Page
generation vacuum remains a separate explicit durability operation so
historical snapshot pins cannot be bypassed.

## Current evidence

Equivalence matrices cover point and batched commands, TTL boundaries,
missing/wrong-family behavior, optimistic rebase and conflicts, private and
current snapshots, reopen, compaction, vacuum, and every commit interruption.
Segmented range tests compare direct, serial segmented, and governed persistent
worker results for all collection families while proving token release and
canonical order.

## HYSTRBT3 codec foundation

The `structure_v3` module freezes the first executable bytes for ADR-0024 and
the runtime now recognizes a validated V3 root on open. New directories still
select V2. Migrated V3 roots accept public whole-collection deletion for Hash,
Set, List, Stream, and Sorted Set. They also accept incarnation-aware create
and recreate, scalar set/conditional/increment/expiry/delete, Hash
set/batch/increment/delete, Set add/batch/remove, List head/tail push and pop,
Stream append, Sorted Set add/rescore/remove, whole-collection TTL for all five
families, Hash-field TTL, and ordered bounded active expiry. Current-root V3
direct reads cover scalar GET/TTL, Hash HGET/HGET_MANY/HLEN/HSCAN, Set
SISMEMBER/SMISMEMBER/SCARD/SSCAN, List LLEN, Sorted Set ZSCORE/ZCARD, and the
collection/field TTL surfaces without complete structure-state
materialization. One collection
incarnation is the canonical big-endian pair of a
nonzero 128-bit runtime transaction identity and a 32-bit lifecycle-mutation
ordinal. The ordinal is the zero-based absolute position of the creating
mutation in the canonical WAL mutation vector, so normal execution and replay
derive identical bytes without a second identity allocator. Hash-field,
set-member, list-chunk, stream-entry, sorted-set member and
order keys place that incarnation after the self-delimiting collection key and
before the child identity. Collection and hash-field expiry identities use the
same fence.

Explicit complete-root migration uses a separate deterministic derivation:
one `MigrateStructureV3` WAL maintenance mutation owns the transaction identity,
and each live collection receives the zero-based ordinal of its metadata entry
in the validated exact-byte-sorted V2 scan. This avoids an unbounded WAL vector
while keeping retry deterministic and allocation authority explicit.

Versioned collection metadata distinguishes live and tombstoned incarnations.
A family-tagged typed payload now freezes the live summaries for all five
collection families: hash field and field-expiry counts, set member count, list
element/byte and head/tail summaries, sorted-set member count, and stream entry
count plus last stream ID. Cross-family payloads and noncanonical shapes fail
closed. The hash expiry count is necessary state: it lets foreground delete
publish an exact retirement record without discovering fields first.
A retirement key binds collection identity and retired incarnation; its value
records family, declared and remaining logical items, primary/secondary/expiry
entry counts, logical bytes, and an exclusive physical cursor. The pure
retirement transition admits at most 1,024 entries per step, requires strictly
increasing cursors, rejects cross-family/key/incarnation progress, and uses
checked subtraction so counter mismatch fails before any caller can publish a
partial step. Existing child tombstones advance the physical cursor without
consuming live counters. Counter exhaustion is deliberately insufficient for
completion: the caller must also prove that the incarnation range is physically
exhausted, and a range exhausted with nonzero counters fails closed. Golden,
malformed, limit, cursor-fence, underflow, tombstone, and terminal-progress
tests exercise these codecs.

Private Set and Hash vertical slices additionally exercise the physical
B+tree. A whole-collection delete publishes exactly two physical mutations
when no collection TTL exists, or three when it does, independently of child
count. Immediate same-key recreation selects a new incarnation while
historical roots retain the prior children. Incremental cleanup visits at most
its declared budget through the shared buffer pool and tombstones only the
retired incarnation. Hash writes transactionally maintain field and
field-expiry counts; hash cleanup validates each field payload and directly
tombstones its optional expiry identity in the same bounded commit. The List
slice stores retired head/tail chunk bounds in the retirement record. Each live
cleanup candidate must be the exact next chunk; decoded element and
logical-byte summaries decrement independent checked counters. Missing,
duplicated, out-of-order, or malformed chunks therefore fail before
publication without materializing the complete list.
The Stream slice freezes both entry count and terminal ID in retirement state.
Sparse monotonically increasing IDs remain valid, while terminal cleanup fails
unless the final live physical entry matches the retired `last_id` exactly.
The Sorted Set slice treats each membership entry and its score-ordered entry
as one validated pair. Cleanup derives and verifies the ordered key by direct
lookup, then tombstones both entries atomically. Its candidate allowance is
half the physical-entry budget so dual-index work cannot hide a twofold read or
mutation cost behind one logical member.

Retirement reclamation does not scan the globally time-ordered expiry
namespace. Its hash-field cleanup step reads the field's encoded expiry
timestamp and directly tombstones that one associated identity while
processing the field child; other families declare zero incremental expiry
entries. The separate active-expiry scheduler walks the ordered namespace at a
caller-supplied logical time and bounded key limit. It validates every V3
marker against current incarnation metadata, tombstones expired Hash fields in
place, and converts whole-collection expiry into the same constant-cardinality
retirement used by explicit deletion. Empty sweeps write no page, WAL, CSN, or
transaction identity.

The private whole-tree validator now inventories the complete `HYSTRBT3` root
and fails closed on an invalid marker, malformed or cross-family metadata,
scalar/collection key collisions, orphan live children, mismatched current
summaries, sorted-set order entries without their exact membership/score pair,
one-way expiry references, and retirement cursor or remaining-counter drift.
Expiry validation is bidirectional: every live marker must resolve its owner,
and every scalar, collection, or hash field that declares expiry must retain
its exact live marker. Active retirement validation replays every physical
candidate after the exclusive cursor and requires the retained counters to
reach exact zero; a live child at or before the cursor is corruption.

A private reachability compactor validates the source root, rebuilds it without
safe metadata, child, expiry, or terminal-retirement tombstones, validates the
replacement root, and requires the active-retirement count to remain exact.
Tests exercise partial-retirement compaction and resumed cleanup, all five
families in one root, preserved historical roots, corrupt counters/cursors,
missing hash-expiry backlinks, orphan children, and unpaired sorted-set order
entries. This is executable private format evidence, not recovery or public
command-path evidence.

`NativeDatabase::migrate_structure_to_v3` now performs one explicit maintenance
commit from a fully validated `HYSTRBT2` root. It preserves scalar and all-five-
family logical state plus immutable blob references, rewrites collection and
expiry identities, drops only canonical V2 tombstones, validates the complete
target before publication, and returns physical source/target/page counters in
`StructureV3MigrationReceipt`. WAL opcode 42 identifies the append-only
migration without renumbering any existing operation. Recovery recognizes V3,
and interruption at every singleton commit boundary observes either the prior
complete V2 root or the complete V3 root; a V2 outcome can retry deterministically.
The historical V2 root remains readable. Remaining unsupported V3 command
families fail before blob, page, WAL, or transaction-identity effects.

The native backup/restore path now round-trips the migrated V3 marker, logical
state, expiry state, supported direct/write behavior, and the fence around
remaining unsupported commands. `compact_structure` accepts both V2 and V3.
Its V3 path plans without appending pages, returns a no-op when no safe
tombstone exists, and otherwise routes the complete validated rebuild; the
underlying compactor rejects any change in active-retirement count.

Public deletion now publishes only metadata, optional collection-expiry, and
one retirement record, independently of child cardinality. Public governed
cleanup selects the next active retirement in canonical order, holds one
Maintenance permit through publication, uses the shared buffer pool, and
accepts a `2..=1024` entry budget. WAL opcode 52 binds the exact retirement key
and budget. `StructureRetirementCleanupReceipt` exposes the retired collection
and incarnation identity, processed primary entries, physical mutations,
remaining work, pages appended, and commit. Empty maintenance writes nothing
and advances neither CSN nor transaction identity. Singleton interruption
matrices cover public deletion plus partial and terminal cleanup; every
recovered state is the prior or complete next cursor and retry converges.

The public create/write slice derives each new collection incarnation from the
commit transaction identity and the creating mutation's absolute canonical
ordinal, including mutations from other engines. Before creation, the physical
path validates scalar and all five typed metadata namespaces, permitting only
canonical tombstones. This makes immediate same-family recreation and
cross-family reuse independent of retirement cleanup. One all-family fixture
exercises Hash field set/batch/increment/delete, Set member add/batch/remove,
List head/tail push and pop, Stream append, Sorted Set add/rescore/remove,
immutable blob-backed Hash/List values, historical-root stability, and reopen.
A Group durability fixture
publishes disjoint collection incarnations with one shared page and WAL sync.
Singleton interruption at every commit boundary for a blob-backed Hash
recreation exposes only the old or complete new incarnation; retry converges.

V3 TTL mutation preserves old/new ordered markers, metadata summaries, and
incarnation fences for every collection family and Hash fields. Due
cross-family reuse retires the expired incarnation before creating the new
family. The active sweep proves deterministic field-before-collection order,
bounded multi-commit progress, five-family retirement creation, no-op
completion, cleanup convergence, historical-root stability, and reopen.
Interruption at every singleton boundary for Hash-field and whole-collection
sweeps recovers either still-due work or the complete physical mutation; retry
converges in both cases.

Forward, reverse, and pattern V3 `HSCAN` plus forward `SSCAN` capture one root,
resolve typed metadata and the current incarnation, map an exact exclusive
cursor into that child prefix, and stop at the admitted result/visit bounds.
Reverse Hash execution uses the B+tree reverse visitor rather than
materializing and reversing forward output. Pattern Hash execution enforces
separate output, physical-visit, and matcher-step budgets, derives
literal-prefix bounds, and advances through tombstones. Hash traversal applies
field TTL and preserves blob/value validation; complete unfiltered scans
additionally verify physical field and expiry summaries. Set traversal
validates member payloads and full declared cardinality when complete.
Zero-limit, missing, wrong-kind, expiry, cursor, V3 identity precedence,
metadata-drift, matcher exhaustion, reopen, and anti-materialization cases are
executable. The V3 profiled forward receipt reports its route as the direct
one-worker path; segmented V3 range execution remains separate pending work.

This is durable bounded lifecycle and direct-read evidence, not full V3
activation. New directories still select V2. Remaining range/rank/set-algebra
commands, page-generation crash coverage, broader backup interruption, and
complete differential command matrices remain required before ADR-0024 can be
accepted.

## Required P6 work

- prove allocation-free hot point paths with caller-owned output buffers;
- add cache/SIMD-aware batch comparison and membership kernels;
- integrate remaining V3 List/Stream/Sorted Set ranges, rank, and set algebra
  without weakening incarnation or
  optimistic-conflict fences;
- extend V3 backup, compaction, and page-generation interruption matrices;
- add million-member allocation/amplification and concurrent
  recreate/member/cleanup evidence without resurrection;
- add hot-key versus unrelated-key scaling receipts, including skew and
  serialization visibility; and
- qualify point, batch, expiry, and lifecycle latency on dedicated hardware.

The existing physical `HYSTRBT2` delete paths still validate and tombstone
every member/chunk before publication. Migrated `HYSTRBT3` roots remove that
foreground walk for all five collection families and reclaim it incrementally.
P6 remains open until the rest of the V3 command surface, allocation and
concurrency proofs, and dedicated-hardware latency/interference evidence land.
