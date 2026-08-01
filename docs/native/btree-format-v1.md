# Native B+tree format v1

Status: normative experimental format; copy-on-write insertion, replacement,
recursive splitting, point lookup, ordered scan, complete validation,
balanced-height validation, historical roots, and allocation-free
buffer-pool node traversal are implemented; the relational namespace now
supports an explicit row-version-pointer format

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

This namespace remains the first physical relational route, not the complete
SQL storage design. Secondary indexes, range cursors, prefix compression, bulk
load, free-space policy, scan-oriented column batches, retention, and vacuum
remain pending.

## Structure namespace

New structure roots use the same native B+tree codec with a separate private
namespace:

| Prefix | Key | Value |
|---:|---|---|
| `0x00` | exact one-byte format key | ASCII `HYSTRBT1` |
| `0x01` | prefix + binary user key | canonical `HYSTRV01` TTL/storage envelope |

The value envelope and legacy single-page compatibility are specified in
[Native structure-engine semantics v1](structures-semantics-v1.md). `SET`,
`EXPIRE`, and `DELETE` rewrite only their copy-on-write leaf-to-root paths;
`DELETE` stores the canonical scalar tombstone. Direct current reads traverse
the buffer pool. The current implementation has no range/prefix cursor, expiry
index/timing wheel, expected-version response, or collection-family layout yet.

## Verification

Current tests cover a stable leaf golden, 1,000 inserts with recursive splits,
balanced-height verification, point reads, ordered scan, retained historical
roots, duplicate/upsert semantics, buffered lookup, oversized and
noncanonical rejection, complete-node validation after an early key match, and
future-node rejection. The runtime benchmark also refuses to run unless its
relational tree height is at least two. Fuzzing, randomized model equivalence,
crash power-loss tests, fanout/fill-factor tuning, and concurrent writer
publication remain required gate evidence.
