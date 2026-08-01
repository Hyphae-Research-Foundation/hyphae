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
point-write conflict table during recovery.

The implementation remains deliberately bounded: transaction snapshots still
materialize relation state, the writer is serialized, physical row histories
do not yet form current-root version chains, structures are binary scalar
values with TTL, and lexical search uses a deterministic analyzer over small
copy-on-write collections. It proves the native transaction/recovery
architecture; it is not the complete SQL, Valkey-class structure, or
OpenSearch-class search engine.
