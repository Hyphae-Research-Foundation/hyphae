# hyphae-native-runtime

This unpublished crate is the first executable convergence slice for Hyphae's
native data ecosystem. It owns a single data directory and coordinates native
catalog, relational, structure, and lexical-search state through copy-on-write
pages, one WAL transaction, and one published CSN.

The first relational physical route stores table markers and canonical MVCC
rows in the native copy-on-write B+tree and performs current-root point reads
through the native buffer pool. Immutable root manifests are anchored by
standalone WAL checkpoint records and cross-validated during recovery.

The implementation remains deliberately bounded: transaction snapshots still
materialize relation state, structures are binary scalar values with TTL, and
lexical search uses a deterministic analyzer over small copy-on-write
collections. It proves the native transaction/recovery architecture; it is not
the complete SQL, Valkey-class structure, or OpenSearch-class search engine.
