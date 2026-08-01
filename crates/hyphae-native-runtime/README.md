# hyphae-native-runtime

This unpublished crate is the first executable convergence slice for Hyphae's
native data ecosystem. It owns a single data directory and coordinates native
catalog, relational, structure, and lexical-search state through copy-on-write
pages, one WAL transaction, and one published CSN.

The first relational physical route stores table markers and canonical MVCC
rows in the native copy-on-write B+tree and performs current-root point reads
through the native buffer pool. That route traverses node bytes without
materializing node entries, pins the matching leaf value, and validates a
borrowed row view before copying the selected value. Immutable root manifests
are anchored by standalone WAL checkpoint records and cross-validated during
recovery.
Binary SQL UPDATE and DELETE publish new roots while retained snapshots keep
their historical roots. Values above 8,192 bytes use the Hyphae-owned
content-addressed blob store, and committed WAL mutations rebuild the
point-write conflict table during recovery. New directories store a fixed
row-version pointer in the B+tree and retain an immutable, fail-closed chain
whose superseded records have explicit half-open `end_csn` boundaries.
Directories written with the earlier inline-row marker remain readable and
writable without an implicit format conversion.
New structure roots also use the native B+tree: binary keys map to a canonical
TTL/storage envelope, direct `GET`/`TTL` traverse pinned pages, and large
structure strings share the immutable blob namespace with relational values.
Legacy single-page structure roots remain readable and writable.

The implementation remains deliberately bounded: transaction snapshots still
materialize relation state, version retention and vacuum are not implemented,
structures currently expose only unconditional binary scalar `SET`/`GET`/TTL,
and lexical search uses a deterministic analyzer over small copy-on-write
collections. Detached transactions can prepare concurrently without holding
writer admission; commit validates their original read CSN and rebases
disjoint mutations over the admitted current root. Publication and its
durability I/O are still serialized and require exclusive access to the
database handle. This proves the native transaction/recovery architecture; it
is not the complete SQL, Valkey-class structure, or OpenSearch-class search
engine.
