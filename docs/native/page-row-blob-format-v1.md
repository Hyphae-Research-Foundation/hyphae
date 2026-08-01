# Native page, row, and blob format v1

Status: normative target contract; page codec, append-only page file, tail
repair, partitioned buffer pool, canonical MVCC row codec, fixed blob
reference, B+tree-backed row persistence, and the first immutable
content-addressed blob byte store are implemented experimentally

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

The record length includes every field and has no trailing padding. Offsets
are absolute from the start of the row, monotonic, inside the record, and end
exactly at the record boundary. Null columns consume no value bytes; unused
null-bitmap bits must be zero.

The implemented codec rejects empty regular rows, unknown flags, zero IDs or
CSNs, empty/inverted version windows, noncanonical null bits, null values with
physical bytes, malformed offsets, and tombstones with column payloads.
Tombstones are exactly the 40-byte header.

The codec exposes both an owned `RowRecord` and a validated borrowed
`RowRecordView`. The borrowed view checks the exact same length, identity,
window, null-bit, offset, and tombstone invariants without allocating column
vectors. A pinned B+tree leaf can therefore be decoded and filtered before the
selected logical value is copied.

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

The first relational route uses an 8,192-byte threshold and stores a one-byte
value envelope inside the row column: `0` followed by inline bytes, or `1`
followed by the fixed 56-byte blob reference. Blob files are named
`blobs/<lowercase BLAKE3 digest>.hyblob`; canonical stages are
`tmp/blobs/<digest>.tmp`.

The exact blob file is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYBLOB01` |
| 8 | 2 | format version `1` |
| 10 | 2 | header length `80` |
| 12 | 4 | flags/reserved zero |
| 16 | 16 | content-derived `BlobId`, little-endian |
| 32 | 8 | logical content length |
| 40 | 4 | CRC32C with this field zeroed |
| 44 | 4 | reserved zero |
| 48 | 32 | BLAKE3 digest of content bytes |
| 80 | variable | exact content bytes; no trailing padding |

The first implementation bounds one blob at 1 GiB, verifies every complete
blob during open, removes canonical interrupted stages, deduplicates identical
content, and derives `blob_generation` from the verified immutable-file count.
The root set and WAL commit bind that generation. A blob promoted before a
failed transaction can remain as an unreferenced immutable orphan until
retention tracing and garbage collection exist; it never makes a logical row
visible. Safe parent-directory synchronization is implemented on Unix. The
Windows implementation reports that strict parent-directory synchronization
is unsupported rather than claiming that guarantee.

## Copy-on-write root publication

A root set contains the catalog root and one root per engine partition plus
the visible CSN and committed WAL LSN/digest anchor. Page writes and blob
promotion happen before the root set is installed. The root manifest is
immutable; publishing a new generation creates a new file and synchronizes
its parent directory where the platform implementation supports it.
The implemented binary formats are specified in
[Native B+tree format v1](btree-format-v1.md) and
[Native root manifest and checkpoint format v1](root-manifest-checkpoint-v1.md).

## Buffer-pool contract

- Pages are keyed by `(file_generation, PageId)`.
- A pinned page cannot be evicted.
- Reads verify bytes before admission.
- Dirty unpublished pages are private to one transaction or background build.
- Published pages are immutable and may be shared across readers.
- Capacity is partitioned; no single engine can exceed its reservation plus
  explicitly available shared capacity.
- Admission and eviction never hold a global engine lock.

The experimental pool currently serves one page-file generation, hashes
`PageId` across bounded mutex partitions, admits only verified immutable pages,
and evicts the least-recently-used unpinned frame within a partition. Explicit
file-generation keys, engine reservations, and a shared-capacity policy remain
pending; they are target requirements, not current claims.

## Verification

Current evidence includes stable page, row, blob-reference, complete blob-file,
B+tree-leaf, and root-manifest encodings; exact row and blob round trips;
malformed null/offset/window rejection; complete-blob corruption rejection;
page tail repair; buffer-pool pin/eviction tests; copy-on-write historical
roots; deduplication; and blob-promotion and checkpoint interruption tests.

Still required are broad random/property and fuzz corpora, bit flips across
every byte range, streaming/chunked values, compression, encryption,
reference-count/retention tests across snapshots, orphan reclamation, and
large-corpus garbage collection.
