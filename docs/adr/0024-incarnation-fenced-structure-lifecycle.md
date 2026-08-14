# ADR-0024: Incarnation-fenced structure lifecycle

- Status: Proposed
- Date: 2026-08-10
- Owners: Celiums Solutions LLC

## Context

`HYSTRBT2` gives hash fields, set members, list chunks, stream entries, and
sorted-set indexes independent physical keys and optimistic conflict
identities. Point and range work therefore scale without serializing a whole
collection. Whole-collection deletion is the remaining inverse problem: it
must scan, validate, and tombstone every retained child path before publishing
the metadata tombstone. Foreground latency and memory consequently grow with
cardinality.

Deleting only metadata is unsafe in the current layout. Recreating the same
user key would reuse the old child-key prefix and could expose prior fields,
members, chunks, stream entries, scores, or expiry markers. Deferring the
complete existing delete batch to maintenance would also leave ambiguous
visibility, conflict, and recovery semantics.

P6 requires bounded foreground lifecycle work without sacrificing immediate
logical deletion, key-family reuse, historical snapshots, crash recovery, or
fail-closed corruption detection.

## Decision

Introduce a new whole-root structure layout, provisionally `HYSTRBT3`. It does
not mix physical collection entries with `HYSTRBT2` inside one root.

Every collection incarnation has a canonical identity derived from the
creating transaction identity plus the zero-based absolute position of its
creating mutation in the canonical WAL mutation vector, encoded as a 32-bit
ordinal. Using the absolute position rather than a create-only counter makes
replay derive the same identity without hidden state. Hash-field,
set-member, list-chunk, stream-entry, sorted-set membership/order, and
collection-scoped expiry keys include that incarnation between the user-key
identity and child identity. Current collection metadata stores the exact live
incarnation.

An explicit whole-root v2-to-v3 migration is the one derivation exception. Its
WAL transaction contains one bounded-shape maintenance mutation rather than an
unbounded mutation per collection. The migration therefore combines that
transaction identity with each live collection metadata entry's zero-based
ordinal in the validated, exact-byte-sorted v2 metadata scan. Replay consumes
the already-published root, while deterministic retry with the same transaction
identity emits identical physical entries. This rule allocates no hidden
identity and is distinct from normal command mutation ordinals.

Reads first resolve live metadata, then access only that incarnation prefix.
Entries from every earlier incarnation are unreachable from current reads.
Historical roots retain their prior metadata and therefore continue resolving
their own incarnation.

Whole-collection deletion publishes, in one normal all-engine commit:

1. the current metadata tombstone;
2. any whole-collection expiry tombstone; and
3. one canonical retirement record containing family, user key, retired
   incarnation, declared logical cardinality, cleanup cursor, and remaining
   validation counters.

That foreground mutation has constant physical-key cardinality. Recreating the
same user key may publish a new incarnation immediately; it never waits for
retirement cleanup and cannot see the retired prefix.

Governed maintenance processes retirement records in canonical order. One
step visits at most a configured child-entry budget, validates reached
identities and payloads, tombstones the corresponding child entries, advances
the exclusive cursor and counters atomically, and reports `more_remaining`.
The cursor is restricted to one child namespace; it never scans the global,
time-ordered expiry namespace. Whole-collection expiry is tombstoned by direct
lookup in the foreground delete. Hash-field cleanup derives at most one
associated expiry identity from the validated field payload and tombstones it
by direct lookup in the same cleanup commit. The terminal step requires exact
declared cardinality and dual-index agreement, then tombstones the retirement
record. Empty work writes nothing.

Sorted-set cleanup validates membership and ordered-score entries as one
logical member. List cleanup validates contiguous chunk identities and total
element/byte summaries. Hash cleanup includes field-expiry markers. Stream and
set cleanup validate their declared cardinalities. Any mismatch fails before
the step publishes and retains the retirement record for diagnosis/retry.

Optimistic member/field conflicts remain scoped to the current incarnation and
the collection lifecycle fence. Maintenance owns the retired incarnation and
cannot conflict with a recreated current incarnation except through shared
global commit sequencing.

New directories use `HYSTRBT3` only after its migration, recovery, and crash
matrices are accepted. Existing `HYSTRBT2` roots remain readable/writable under
ADR-0021. An explicit offline or maintenance rewrite validates the complete
v2 root and emits one v3 root; open never performs an implicit conversion.
Migrated V3 roots now accept whole-collection deletion for Hash, Set, List,
Stream, and Sorted Set plus explicit retirement cleanup and compaction. They
also accept incarnation-aware creation and recreation, Hash field
set/batch/increment, Set member add/batch, List tail append, Stream append, and
Sorted Set add/rescore. Whole-collection TTL for every family, Hash-field TTL,
and ordered bounded active expiry are also incarnation-aware. Hash field
deletion, Set member removal, Sorted Set member removal, and List head/tail
push and pop now mutate only their exact physical member, ordered-score pair,
or boundary chunk while maintaining typed metadata summaries. Scalar
`SET`/conditional set, `INCRBY`, `EXPIRE`, and `DELETE` now preserve the
ordered expiry backlink and immutable blob reference directly in V3. Reusing
a due Hash, Set, List, Stream, or Sorted Set as a scalar first retires that
exact incarnation in the same commit. Current-root V3 reads now resolve one
typed metadata record and its incarnation-fenced child identity directly for
scalar `GET`/TTL, Hash `HGET`/`HGET_MANY`/`HLEN`, Set
`SISMEMBER`/`SMISMEMBER`/`SCARD`, List `LLEN`, Sorted Set `ZSCORE`/`ZCARD`,
all five collection TTL surfaces, Hash-field TTL, and bounded forward Hash
`HSCAN`, reverse Hash `HSCAN_REVERSE`, pattern Hash `HSCAN_MATCH`, and forward
Set `SSCAN`. These routes never load the complete structure state.
Cardinality comes from typed metadata except when a Hash has field expiries:
that `HLEN` route visits only the current incarnation's field prefix, applies
logical-time visibility, and verifies the physical count and expiry summary
before returning. Both directional Hash scans, pattern Hash scans, and `SSCAN`
map their exclusive binary cursor into that same incarnation prefix and
validate the complete physical summary when an unfiltered request covers the
full declared collection. Pattern pages also enforce independent output,
physical-visit, and matcher-step bounds and derive literal-prefix pruning.
List `LRANGE`, Stream `XRANGE`, binary Set union/intersection/difference, and
Sorted Set rank, reverse-rank, signed-rank ranges, and bounded score ranges now
share the same current-incarnation rule. Large range calls plan immutable
B+tree leaf segments, reserve governed workers, merge in canonical direction,
and verify exact summaries on complete coverage. Reached List chunks, Stream
entries, and live Sorted Set order entries validate their physical identity;
Sorted Set order entries additionally resolve the exact membership/score pair.
The 30 current-root logical commands (52 public Rust methods) therefore avoid
complete structure-state loading. Retained snapshots and transaction-local
structure methods remain outside this slice because they still materialize the
complete state; the V3 delta-write path is also still pending.

## Consequences

- Foreground whole-collection deletion becomes O(1) in child cardinality.
- Reclamation is incremental, governor-controlled, resumable, and observable.
- Child keys gain incarnation bytes and reduce effective user-payload key
  capacity; every existing maximum must be recomputed contract-first.
- Recovery must validate live metadata, current incarnation entries, retirement
  records, partial cursors, counters, and absence of cross-incarnation links.
- Compaction and backup must retain active retirement state until its terminal
  cleanup commit.
- The format requires a complete migration and dual-format maintenance burden
  until retirement policy removes `HYSTRBT2`.

## Alternatives considered

### Keep cardinality-linear foreground deletion

Rejected because latency, allocation, and write amplification remain
unbounded by request size and cannot satisfy P6.

### Tombstone metadata and prohibit recreation until cleanup finishes

Rejected because a large retired collection would make a familiar key
unavailable for an unbounded maintenance interval.

### Delete physical B+tree keys in place

Rejected because it violates immutable copy-on-write roots and historical
snapshot ownership.

### Store each collection as one serialized value

Rejected because it restores whole-collection write contention, amplification,
and single-worker execution that the native structure substrate removed.

### Use only the commit CSN as incarnation

Rejected because one transaction can contain more than one lifecycle mutation;
the mutation ordinal is required for a canonical unique identity.

## Verification

- Golden codecs cover v3 metadata, incarnation keys, retirement records, and
  cursor/counter states with malformed and noncanonical matrices.
- A private physical Set slice proves two foreground mutations without TTL,
  immediate incarnation-fenced recreation, bounded shared-buffer-pool cleanup,
  historical-root stability, and terminal retirement tombstoning.
- A private physical Hash slice additionally proves transactional field-expiry
  accounting and direct expiry tombstoning without a global expiry scan.
- A private physical List slice proves retained head/tail range validation,
  exact per-chunk element/byte accounting, and bounded cleanup without
  complete-list materialization.
- A private physical Stream slice proves sparse monotonic IDs, retained
  terminal-ID validation, bounded cleanup, and recreation isolation.
- A private physical Sorted Set slice proves direct membership/order pairing,
  atomic dual-index tombstoning, and a physical-entry budget that charges both
  halves of every member.
- A private whole-root validator proves exact metadata/child ownership,
  bidirectional expiry links, dual-index agreement, retirement cursor/counter
  state, and simultaneous coexistence of all five families. A private
  reachability compactor drops only safe tombstones, retains active retirement
  records and their remaining children, supports resumed cleanup, and leaves
  historical roots readable.
- The explicit migration validates the complete V2 source before appending a
  target, preserves logical state and immutable blob references across all five
  families, drops only canonical tombstones, emits opcode 42 without
  renumbering existing WAL operations, and recognizes V3 roots on reopen.
- Migration crash injection at every singleton commit boundary observes either
  the complete historical V2 root or one complete validated V3 root. Every V2
  outcome can retry and converge; no boundary exposes a mixed format.
- Native backup/restore round-trips a migrated V3 root, revalidates its logical
  state on open, preserves supported scalar mutation and current-root direct
  reads, and keeps remaining unsupported command families fail-closed. Public
  structure compaction accepts V3, routes the validated physical rebuild, and
  retains the private compactor's active-retirement invariant.
- The public transaction path deletes all five V3 collection families in one
  all-engine commit. A 128-member Set and 70-element List fixture demonstrates
  that deletion adds exactly one retirement record per collection while
  retaining historical roots and making every collection immediately absent.
- `cleanup_structure_retirements` is admitted as Maintenance work, selects the
  next retirement canonically, uses the database's shared buffer pool, rejects
  budgets outside `2..=1024`, publishes WAL opcode 52, reports identity and
  bounded progress, and performs no page/WAL/CSN work when empty. One public
  matrix reclaims all five families and then compacts every terminal tombstone.
- Crash injection at every singleton boundary for public deletion, partial
  cleanup, terminal cleanup, migration, and compaction observes the prior or
  complete next state; cleanup retries converge without changing historical
  roots. A mixed supported-delete/unsupported-write batch still fails before
  blob, page, WAL, or transaction-identity effects.
- Public V3 creation derives each incarnation from the transaction identity
  and absolute canonical mutation ordinal. One differential fixture deletes
  and recreates all five families, changes a Hash key into a Set, exercises
  bounded Hash/Set batches, Hash increment, List tail chunks, Stream appends,
  Sorted Set rescoring, and immutable blob-backed Hash/List values, then
  reopens without retired-child resurrection. Disjoint V3 collections also
  publish through one Group durability cohort and one shared page/WAL sync.
- Recreation with a blob-backed Hash field is interrupted at every singleton
  commit boundary. Recovery exposes either the complete old incarnation or
  the complete new incarnation, retains the historical migrated root, and
  retry converges.
- V3 TTL mutation preserves replacement expiry markers and collection
  summaries for all five families plus Hash fields. Due cross-family reuse
  retires the old incarnation before publication. Ordered active expiry
  validates marker ownership, processes bounded batches, creates the same
  five-family retirement records as explicit deletion, and writes nothing
  after convergence.
- Hash-field and whole-collection active-expiry sweeps are interrupted at every
  singleton commit boundary. Recovery exposes either still-due work or the
  complete physical mutation, retains the historical root, and retry
  converges without duplicate effects.
- V3 Hash/Set deletion batches exclude missing identities before WAL staging,
  update cardinality and Hash field-expiry summaries per canonical mutation,
  and preserve an empty typed collection. Sorted Set removal tombstones the
  membership and exact ordered-score entry atomically. List head push and both
  pops touch one boundary chunk, preserve blob references and collection TTL,
  and reset boundary metadata canonically when the list becomes empty. A
  migrated multi-element chunk fixture and all seven singleton crash
  boundaries prove prior-or-complete recovery, retry convergence, historical
  root immutability, and reopen equivalence.
- V3 scalar mutation validates live collection exclusion, the prior expiry
  backlink, and the exact prior value for expiry-only rewrites before appending
  any page. Missing deletes, stale expiry rewrites, malformed backlinks, and
  collection-owned keys fail without physical effects. Blob-backed
  replacement, conditional no-op, TTL-preserving increment, expiry, deletion,
  Group publication, optimistic conflict, backup/restore, all seven singleton
  crash boundaries, retry, historical-root immutability, and reopen are
  executable acceptance evidence. Due reuse covers all five collection
  families and preserves their retirement records.
- Public V3 scalar and collection point reads are exercised with both complete
  engine-state and complete structure-state materialization guards active.
  The matrix covers inline and blob-backed Hash values, duplicate batch
  positions, exact field and collection expiry boundaries, typed-kind errors,
  absent keys, metadata-only cardinality, the bounded current-incarnation Hash
  visibility walk, forward/reverse/pattern Hash plus forward Set cursor
  ranges, pattern prefix pruning, tombstone continuation, matcher exhaustion,
  V3-only identity bounds, full-range metadata-drift rejection, corruption
  rejection, and reopen equivalence.

The remaining acceptance evidence is the remaining List/Stream/Sorted Set
range, rank, set-algebra, stream-read, and broader command-equivalence matrix;
page-generation and
broader backup interruption; a million-member allocation/amplification fixture;
concurrent recreate/member/cleanup histories; and dedicated-hardware delete,
cleanup, interference, page, and WAL receipts.
