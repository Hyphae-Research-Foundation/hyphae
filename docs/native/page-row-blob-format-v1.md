# Native page, row, and blob format v1

Status: normative target contract; page codec, append-only page file, tail
repair, and partitioned buffer pool are implemented experimentally; row and
blob formats remain pending

The native substrate uses fixed-size copy-on-write pages and separate
content-addressed blobs. No target-path page is a Redb page.

## Page geometry

- Page size: 16,384 bytes.
- Header size: 96 bytes.
- Maximum inline payload: 16,288 bytes.
- `PageId` zero is invalid.
- Physical offset is `(page_id - 1) * 16,384` in the active page file.
- Pages are immutable after publication. A logical update allocates new pages
  and publishes a new root set at commit.

## Page header

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYPAGE01` |
| 8 | 2 | format version, little-endian, value `1` |
| 10 | 1 | page kind |
| 11 | 1 | flags |
| 12 | 2 | header length, value `96` |
| 14 | 2 | reserved zero |
| 16 | 8 | page ID |
| 24 | 8 | creating CSN; zero only before commit publication |
| 32 | 8 | kind-specific next/child/overflow page ID or zero |
| 40 | 4 | payload length |
| 44 | 4 | CRC32C with checksum and digest fields zeroed |
| 48 | 32 | BLAKE3 digest of the complete page with this field zeroed |
| 80 | 16 | reserved zero |
| 96 | variable | payload followed by zero padding |

Unknown kinds, nonzero reserved bytes, invalid lengths, ID mismatch, checksum
failure, digest failure, or nonzero padding fail closed.

## Page kinds

V1 reserves:

1. catalog root;
2. heap leaf;
3. version-chain page;
4. B-tree internal;
5. B-tree leaf;
6. hash directory;
7. hash bucket;
8. structure node;
9. bitmap/doc-values page;
10. search delta page;
11. vector metadata page; and
12. overflow reference page.

Immutable search and ANN segments use their own versioned segment formats but
are referenced from pages and the root set.

## Row record

Rows are catalog-typed and do not repeat type tags per column.

| Field | Encoding |
|---|---|
| total record length | u32 |
| flags | u16 |
| column count | u16 |
| row ID | 128 bits |
| begin CSN | u64 |
| end CSN | u64; `u64::MAX` means open-ended |
| null bitmap | ceil(column count / 8) bytes |
| value offsets | column count + 1 checked u32 offsets |
| value area | canonical type bytes or blob references |

Columns are ordered by stable `ColumnId`, not display name. A dropped column
keeps its physical ID reserved. A row tombstone has the tombstone flag, row
ID, begin/end CSNs, and no values.

The record length includes every field and padding. Offsets are monotonic,
inside the record, and end exactly at the record boundary.

## Variable-length and blob values

Values up to a catalog-configured inline threshold remain in the row or
structure page. Larger values use:

| Blob reference field | Width |
|---|---:|
| blob ID | 128 bits |
| logical length | u64 |
| BLAKE3 content digest | 256 bits |

Blob bytes are immutable and stored under the digest-derived path. Creation is
staged in `tmp/`, synchronized according to the transaction durability class,
verified, and promoted before a root referencing it becomes visible. Garbage
collection may remove a blob only after no retained root or snapshot can
reference it.

## Copy-on-write root publication

A root set contains the catalog root and one root per engine partition plus
the visible CSN and WAL checkpoint LSN. Page writes and blob promotion happen
before the root set is installed. The root manifest is immutable; publishing
a new generation creates a new file and synchronizes its parent directory.

## Buffer-pool contract

- Pages are keyed by `(file_generation, PageId)`.
- A pinned page cannot be evicted.
- Reads verify bytes before admission.
- Dirty unpublished pages are private to one transaction or background build.
- Published pages are immutable and may be shared across readers.
- Capacity is partitioned; no single engine can exceed its reservation plus
  explicitly available shared capacity.
- Admission and eviction never hold a global engine lock.

## Verification

Required evidence includes golden pages, random round trips, truncation and
bit-flip rejection at every byte range, invalid offset/length fixtures,
concurrent pin/eviction tests, copy-on-write root atomicity, blob promotion
interruption tests, and reference-count/retention tests across snapshots.
