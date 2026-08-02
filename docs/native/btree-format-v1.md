# Native B+tree format v1

Status: normative experimental format; copy-on-write insertion, replacement,
ordered multi-key replacement, recursive splitting, point lookup, ordered
scan, bounded prefix scan, reentrant buffered prefix visitation, complete
validation, balanced-height validation, historical roots, and allocation-free
buffer-pool point traversal are implemented or implementation-gated;
relational, structure, and lexical-search namespaces use the native tree

The native B+tree stores canonical binary keys and values directly in Hyphae
pages. It does not wrap Redb, RocksDB, SQLite, or another tree implementation.
Every mutation appends a new leaf-to-root path and returns a new immutable root.

## Common rules

- Node payload version is `1`.
- Keys are compared as unsigned binary byte strings.
- Keys within a node are strictly increasing.
- An empty tree has no root page; a published engine namespace must materialize
  at least one reserved format entry.
- Inline keys are limited to 4,096 bytes.
- One leaf entry must fit in one 16 KiB native page.
- Published nodes have a nonzero creating CSN.
- Readers fail closed on a wrong page kind, malformed length, unknown preamble,
  invalid count, duplicate or unordered key, zero child, cycle, excessive
  height, invalid separator, or future node.

The cached point route parses verified immutable node bytes in place. It uses
a fixed 64-entry stack array for path-cycle detection, validates the complete
visited node even after finding a key, and returns a value range pinned by the
leaf's buffer-pool frame. It does not allocate key/value vectors for internal
or leaf entries. The v1 sequential entry layout still requires a linear pass
within each visited node; a future slotted format may add binary-searchable
offsets only through a new format version.

Prefix scans derive the exclusive binary successor of the requested prefix
and use internal separator ranges to skip nonintersecting subtrees. A prefix
ending entirely in `0xff` has no finite upper bound. Both page-store and
buffer-pool variants preserve canonical key order; only reached nodes are
decoded during the operation. The buffered visitor accepts an exclusive
full-key resume cursor and propagates an early-stop signal without decoding or
materializing the remaining range. It validates canonical order across the
keys reached before the stop. The older materializing APIs continue to return
owned key/value vectors.

## Leaf payload

Leaf pages use native page kind `5`.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYBTLF01` |
| 8 | 2 | format version `1` |
| 10 | 2 | entry count |
| 12 | 4 | reserved zero |
| 16 | variable | ordered entries |

Each entry is:

| Width | Field |
|---:|---|
| u32 | key length |
| u32 | value length |
| variable | key bytes |
| variable | value bytes |

The payload ends exactly after the last value. Empty leaves and trailing bytes
are invalid.

## Internal payload

Internal pages use native page kind `4`.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYBTIN01` |
| 8 | 2 | format version `1` |
| 10 | 2 | separator count |
| 12 | 4 | reserved zero |
| 16 | 8 | first child page ID |
| 24 | variable | separator/right-child pairs |

Each pair is `u32 key_length`, key bytes, then one `u64` right-child page ID.
The child count is exactly the separator count plus one. Every separator equals
the minimum key in its right child. Complete-tree validation also proves that
adjacent child ranges do not overlap.

## Copy-on-write mutation

Insertion descends through immutable nodes, rewrites only the affected path,
and splits nodes when their exact encoded payload exceeds the page capacity.
A root split appends a new internal root. Insert-only mode rejects an existing
key; upsert returns the prior value. Old roots remain readable.

Ordered multi-key upsert accepts a strictly increasing, duplicate-free sequence
of canonical key/value pairs. An empty sequence is a no-op. The complete input
is validated for key order and leaf-entry fit before any page is appended.
Duplicate, descending, oversized, or individually unencodable entries fail
without changing the page count.

One batch partitions its ordered changes through the existing separator
ranges. Every reached existing node is decoded at most once for that batch;
each affected leaf merges all of its changes before it is encoded, and each
affected internal level is rebuilt once from changed and retained child
references. Unaffected subtrees retain their exact page IDs. A leaf or
internal node may split into multiple nodes, and the root grows by as many
levels as required within the canonical height bound.

The result reports the new immutable root and exact appended-page count. Batch
publication does not weaken transaction atomicity: the engine root remains
unpublished until the owning WAL transaction and global root set commit.
Callers that require sequential same-key semantics must coalesce those
operations before invoking the ordered batch.

No published page is changed in place. Pages appended by an interrupted
transaction remain unreachable until a WAL-committed root set names the new
root.

## Relational namespaces

The relational vertical uses one tree root and these private key namespaces:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYRELBT1` or `HYRELBT2` |
| `0x01` | prefix + 128-bit big-endian table `ObjectId` | empty table marker |
| `0x02` | prefix + table `ObjectId` + primary-key bytes | format-specific row value |
| `0x03` | prefix + 128-bit big-endian index `ObjectId` | 32-byte `HYRIDX01` metadata |
| `0x04` | prefix + index `ObjectId` + secondary-entry identity | live/tombstone byte |

The table marker preserves an empty table. A row record currently contains the
primary-key bytes and binary row payload as two catalog-ordered columns. The
row payload column contains the runtime's inline-or-blob envelope. The runtime
verifies the duplicated primary key, content-derived `RowId`, MVCC visibility,
blob reference/content, and row codec before returning a value.

`HYRELBT1` stores the canonical MVCC row directly as the B+tree value. It
remains supported for open, read, mutation, and recovery. `HYRELBT2` stores the
fixed `HYROWP01` pointer described in
[Native page, row, and blob format v1](page-row-blob-format-v1.md). New data
directories use `HYRELBT2`; no implicit in-place conversion rewrites an
existing V1 directory.

Under V2, UPDATE and DELETE append immutable version-chain pages and replace
only the B+tree pointer under a new copy-on-write root. Superseded copies carry
closed `end_csn` values and tombstones are ordinary open versions. Historical
snapshots retain their prior roots.

`HYRIDX01` contains its magic, the 128-bit big-endian relation `ObjectId`,
one canonical boolean for `unique`, one for `nulls_distinct`, and six reserved
zero bytes. A secondary-entry identity is:

1. big-endian `u32` secondary-key byte length;
2. canonical ordered secondary-key bytes; and
3. canonical primary-key bytes.

The B+tree key remains limited to 4,096 bytes. Entry value `1` is live and `0`
is a tombstone. Creation writes the metadata marker and backfills the final
admitted relation state. Row insertion derives every index projection from the
catalog-bound tuple. Update writes tombstones for every old projection and live
markers for every new projection; delete writes the old tombstones. These
markers share the row mutation's copy-on-write root and CSN. Optimistic rebase
repeats old/new projection maintenance against the currently admitted catalog
and rows before publication. Recovery verifies metadata against the catalog,
recomputes every live entry from its row, enforces uniqueness, rejects
orphan/malformed entries, and proves that every row has every required
projection.

Exact current-root secondary lookup now reads `HYRIDX01`, constructs the
length-delimited entry prefix, uses separator-pruned buffered prefix traversal,
validates every live/tombstone marker, and follows live primary keys to visible
rows through the same captured root. Missing metadata has a distinct
`UnknownSecondaryIndex` failure; malformed entries or dangling live rows fail
closed. The public result is deterministic primary-key order.

Current-root primary-key scan uses the `0x02 + table ObjectId` prefix and the
buffered visitor. `scan_latest_relational(table, start_after, limit)` validates
the table marker, translates the optional exclusive primary-key cursor into a
full physical key, walks rows in canonical primary-key order, resolves
HYRELBT1/HYRELBT2 visibility and blobs, skips visible tombstones, and stops
when the requested live-row count is reached. It returns only that bounded
owned page and never constructs the complete `RelationState`.

The buffered visitor also accepts independent full-key `Included`, `Excluded`,
or `Unbounded` lower and upper bounds. Bounds are intersected with the
namespace prefix. Internal traversal compares each child's separator interval
with both bounds and does not read disjoint subtrees; leaf traversal applies
exact inclusivity and retains early-stop control flow. The cursor API is a
wrapper over this range visitor with an exclusive lower bound.

`scan_latest_relational_range(table, lower, upper, limit)` maps canonical
primary-key bounds into the table's physical namespace, validates the table
marker, resolves only visible row versions, skips tombstones, and returns no
rows for inverted or equal-open intervals.

This namespace remains the first physical relational route, not the complete
SQL storage design. Secondary-key range cursors, a stateful zero-copy cursor,
prefix compression, bulk load, primary-key-changing updates, free-space
policy, scan-oriented column batches, retention, and vacuum remain pending.

## Structure namespace

New structure roots use the same native B+tree codec with a separate private
namespace:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYSTRBT2` |
| `0x01` | prefix + binary user key | canonical `HYSTRV01` TTL/storage envelope |
| `0x02` | prefix + binary hash key | `HYHSHM01` live-field count |
| `0x03` | prefix + length-delimited hash key + field | persistent `HYSTRV01` field envelope |
| `0x04` | prefix + binary set key | `HYSETM01` live-member count |
| `0x05` | prefix + length-delimited set key + member | persistent empty `HYSTRV01` marker |
| `0x06` | prefix + binary list key | `HYLSTM01` length and end-chunk IDs |
| `0x07` | prefix + length-delimited list key + ordered chunk ID | `HYLSTC01` packed element envelopes |
| `0x08` | prefix + binary sorted-set key | `HYZSTM01` live-member count |
| `0x09` | prefix + length-delimited sorted-set key + member | `HYZSCR01` canonical score |
| `0x0a` | prefix + sorted-set key + sortable score + member | persistent empty `HYSTRV01` marker |
| `0x0b` | prefix + sortable expiry + scalar key | one-byte live marker or tombstone |

The value envelope and legacy single-page compatibility are specified in
[Native structure-engine semantics v1](structures-semantics-v1.md). `SET`,
`EXPIRE`, and `DELETE` rewrite only their copy-on-write leaf-to-root paths;
`DELETE` stores the canonical scalar tombstone. Direct current reads traverse
the buffer pool. Hash field changes rewrite their own field path plus the small
hash metadata path rather than a whole serialized map. `HYSTRBT2` maintains
the scalar expiry index in the same mutation path and validates it
one-to-one on recovery. `HYSTRBT1` remains a compatibility format without
that index. The current implementation has no general range/streaming cursor,
expected-version response, whole-hash deletion, or layouts for the remaining
collection families. The list layout rewrites metadata plus one packed end
chunk per push/pop; its exact metadata, chunk, tombstone, and
corruption rules are specified in the structure contract.

## Lexical-search namespace

New lexical-search roots use marker `HYSEABT1` and four independent private
prefixes for collection statistics, stored documents, term statistics, and
postings. Query execution performs point reads for collection/term/document
metadata and a separator-pruned prefix scan for each requested term's
postings. Exact key/value envelopes are specified in
[Native search-engine semantics v1](search-semantics-v1.md).

Legacy page-kind-10 `SearchState` roots remain readable and writable without
implicit conversion. New directories use the B+tree format.

## Current-root rebuild

Structure reachability compaction validates and scans one complete current
B+tree before writing. It removes only entries already proven to be canonical
physical tombstones by the owning engine, then feeds the retained
strictly-ordered key/value pairs into the empty-tree ordered batch builder.

The resulting tree is balanced, contains the retained values byte-for-byte,
and uses pages created at the maintenance commit CSN. The old tree remains
immutable and readable. An empty tombstone set is a no-op rather than a
same-content rewrite.

This rebuild reduces the pages and tombstones reachable from the current root.
It does not reclaim superseded pages from the append-only page file; that
requires retention-aware page-file generation garbage collection.

## Verification

Current tests cover a stable leaf golden, 1,000 inserts with recursive splits,
balanced-height verification, point reads, ordered and prefix scans (including
an all-`0xff` upper range), retained historical roots, duplicate/upsert
semantics, ordered-batch equivalence and path coalescing, buffered lookup/scan,
oversized and noncanonical rejection, complete-node validation after an early
key match, and future-node rejection.
Buffered visitor coverage additionally proves exclusive resume, early stop,
multilevel traversal, order equivalence with the materialized prefix scan, and
exhaustion after the final key. Bound-aware visitor coverage proves inclusive
and exclusive endpoints, half-open intervals, inverted/empty ranges, namespace
intersection, multilevel separator pruning, and early stop.
Runtime coverage additionally proves secondary-index backfill, insert,
uniqueness, both optimistic index/row commit orders, catalog/root reopen, and
missing-projection rejection. Direct exact lookup coverage uses a multilevel
tree, compares physical and materialized prepared results, checks deterministic
non-unique order, null short-circuit, stale-plan rejection, reopen equivalence,
unknown metadata, and a forged invalid live marker. Indexed update/delete
coverage checks old/new markers, unique/null semantics, optimistic admission,
retained roots, V1/V2 reopen, and all seven commit interruption boundaries.
Bounded relational scan coverage uses a multilevel tree, tombstoned and updated
rows, exclusive pagination, zero limit, unknown relation, HYRELBT1/HYRELBT2
reopen, and equality across transaction, materialized snapshot, and current
physical SQL execution.
Bounded primary-key range coverage compares transaction-private, materialized
snapshot, and current-root physical execution for a composite key; it includes
one-sided and mixed bounds, tombstones, inverted/equal-open safety, type/null
and arity rejection, plan introspection, and reopen equivalence.
The runtime benchmark refuses to run unless its relational, structure, and
search trees are multilevel. Fuzzing, randomized model equivalence,
sector/filesystem power-loss tests, fanout/fill-factor tuning, secondary-range
and zero-copy cursors, and concurrent writer publication remain required gate
evidence.
