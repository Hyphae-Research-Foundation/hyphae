# Native segmented substrate v1

Status: shared relational, hash, set, stream, sorted-set score/rank, list,
lexical, and exact-vector execution slices implemented; P3 remains open

Hyphae's native B+tree leaf pages are immutable copy-on-write objects already
shared by relational, structure, catalog, and lexical state. P3 makes that
physical boundary explicit instead of introducing a second segment format.

## Leaf segment contract

`BTree::plan_range_segments` walks internal separator ranges and selects only
leaf pages that intersect an inclusive/exclusive canonical key range. Each
`BTreeSegment` carries:

- the originating immutable root;
- its immutable leaf page identity;
- the physical minimum and maximum key;
- physical entry cardinality; and
- the exact owned query bounds.

Plans are emitted in canonical key order. Empty or reversed ranges produce no
segments. `scan_planned_segment` re-reads and validates the leaf summary,
rejects plans from another root, reapplies the original bounds, and returns
owned entries. A plan therefore cannot be reused accidentally after a new
copy-on-write root is published.

The planner validates every reached page and cycle boundary. It does not claim
that an online bounded query has verified pruned, unreachable subtrees; full
root validation remains an explicit verification/recovery responsibility.

## Relational, structure, and lexical routes

Current primary-key range scans retain the direct cached visitor for limits up
to 256 rows. Larger bounded scans map their relation prefix and primary-key
bounds into the shared physical keyspace and plan immutable leaf segments.
When a matching governor/pool is installed, they reserve one compute and one
I/O token per selected worker, execute leaves on persistent workers, merge in
segment/key order, apply MVCC/tombstone decoding, and truncate to the requested
visible-row limit. The portable or ungoverned fallback executes the identical
plan serially.

Workers receive immutable `PageStoreReader` and `BlobStoreReader` handles.
These capture the complete page boundary and verified blob-reference namespace
at planning time, expose no mutable publication methods, and reject references
published after capture. `NativeDatabase` remains the sole append/publication
authority; no global reader mutex is introduced. The shared buffer pool is
partitioned and synchronized independently.

`scan_latest_relational_range_profiled` returns the common snapshot CSN,
visible rows, selected segment count, covered physical entry count, planning
and execution governor receipts, planned workers, and query-local worker batch
count. The compatibility API delegates to the same operation and returns only
rows.

Large physical hash scans use the same substrate after validating current hash
metadata and logical-time visibility. Limits up to 256 fields retain the direct
cached prefix visitor. Larger scans plan the hash-field prefix and exclusive
cursor, decode leaf segments serially or on governed persistent workers, merge
in exact field-byte order, preserve TTL/tombstone filtering, and validate the
physical live count against declared cardinality on complete scans.
`hscan_latest_hash_at_profiled` exposes the shared snapshot, declared field
count, segment coverage, admission scopes, planned workers, and actual batches;
the ordinary `HSCAN`-style API delegates and returns only entries.

Large set-member scans follow the same structure path. They plan the encoded
set-member prefix plus exclusive member cursor, decode each immutable leaf on
governed workers, merge in exact member-byte order, skip physical tombstones,
and compare live members with declared set cardinality for complete scans.
`sscan_latest_set_at_profiled` exposes the snapshot, declared cardinality,
segment coverage, admissions, planned workers, and actual batches. Limits up
to 256 members retain the direct visitor.

Stream ranges no longer iterate every integer ID across a large sparse range.
Ranges wider than 256 IDs or requesting more than 256 entries map their
inclusive ID bounds directly into the stream-entry keyspace, prune unrelated
leaves, decode selected segments in parallel, and merge by stable ID. Narrow
ranges retain direct point probes. `xrange_latest_stream_at_profiled` reports
the declared last ID, physical coverage, admissions, workers, and batches.
Every reached key and payload ID must agree; tombstoned entries remain hidden.

Wide sorted-set score ranges map canonical floating-point bounds directly into
the ordered score/member keyspace. Ascending plans retain leaf and in-leaf
order; descending plans reverse both before execution. Ordered worker results
then apply the logical live-entry offset and limit, so equal-score member ties
match the direct oracle independently of completion order. The two profiled
score-range APIs expose physical coverage and worker admissions. Small
`offset + limit` windows retain the direct visitor.

Wide sorted-set rank ranges use the same ordered-index segment plan when the
requested output covers at least half of the live set. Workers decode live
entries independently, the coordinator validates the complete live count
against metadata, and only then applies the normalized signed rank window.
Small or selective rank windows retain the direct visitor: without persisted
per-segment live-count summaries, scheduling the entire set for a narrow deep
rank would be false parallelism. Ascending and descending profiled APIs expose
this choice and preserve exact score/member tie order.

Native-list metadata now has a backward-compatible sized form carrying total
logical value bytes. Push and pop update this summary in the same copy-on-write
publication as length and chunk boundaries; complete materialization validates
both summaries. Large ranges covering at least half of a sized list map the
current inclusive head/tail chunk identities into immutable B+tree segments.
Workers use frozen page and blob readers, decode chunks independently, and the
coordinator requires contiguous chunk identities, exact element count, and
exact logical bytes before slicing the requested range. The memory governor
reserves the declared output bytes plus plan overhead. Legacy metadata and
narrow ranges remain on the direct head/tail walk.

Large BM25 queries now plan one immutable posting range per live canonical
query term. Planning records the term's declared document frequency, IDF,
selected leaves, and physical entries. Selected `(term, leaf)` work is emitted
in term-byte order and then leaf-key order. Each persistent worker receives a
snapshot-frozen page handle, resolves document lengths through the shared
buffer pool, skips admitted posting tombstones, and returns an ordered score
contribution batch. The coordinator reduces those batches in original plan
order; it never performs a completion-order floating-point reduction. BM25
scores and ties therefore remain bit-for-bit identical to the serial oracle
for every worker count.

The segmented lexical path validates summed live postings against every term's
declared document frequency before ranking. Missing documents, live postings
for tombstoned documents, reordered batches, cardinality overflow, and count
mismatches fail closed as search-tree corruption. Corpora with at most 256
documents retain the direct cached visitor. `match_latest_text_profiled`
reports the snapshot, live planned terms, selected segments and entries,
planning/execution admissions, reserved workers, and query-local batches; the
compatibility APIs delegate and return the same hits and CSN.

The exact vector oracle now partitions the effective stable-ID vector order
into at most one contiguous batch per governed worker. Each worker computes
and canonically sorts only its local top-k; the coordinator merges those
bounded lists and applies the same distance/object-ID total order. This reduces
intermediate results from one hit per vector to at most `workers * k` without
changing exactness. The receipt reports exact admitted vector and batch counts
in addition to compute and memory reservations. This is the portable exact
slice; durable geometry summaries and approximate partition selection remain
P4 work.

Tests prove relational, hash, set, stream, sorted-set score/rank, list,
lexical, and exact-vector ordered serial
and persistent-worker execution, exact equality with unsegmented oracles,
reversed-range behavior, root binding, immutable read boundaries, relational
MVCC/tombstone filtering, lexical term/cardinality validation, canonical
first/last rows, list byte/cardinality accounting, and complete CPU/I/O token
return. A multi-engine, multilevel fixture mutates far-apart SQL rows, set
members, and lexical documents and crashes at every commit boundary; reopen
observes either the complete prior CSN or the complete new CSN, never a partial
combination of segment roots.

The mixed six-family amplification regression now executes actual structure
compaction followed by page-generation vacuum. It accounts current logical
bytes after churn, reports append, compaction, and maintained-retention ratios
separately, and freezes maintained retention at 6.0x and compaction output at
4.0x for its deterministic corpus. Exercising that path also closed a physical
namespace gap: structure compaction now validates and retains live stream
metadata/entries while dropping their canonical tombstones. The much larger
append-only COW ratio remains explicit debt and is not presented as a target.

## Remaining P3 work

- add engine summaries beyond key range/cardinality: SQL min/max/null and
  membership data, structure family/expiry data, lexical block maxima and
  filter bitmaps, and vector geometry/generation data;
- produce dedicated-hardware pruning and foreground-interference receipts;
  deterministic compaction/retention bounds are now covered locally, while
  append amplification remains optimization debt.
