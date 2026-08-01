# hyphae-native-runtime

This unpublished crate is the first executable convergence slice for Hyphae's
native data ecosystem. It owns a single data directory and coordinates native
catalog, relational, structure, and lexical-search state through copy-on-write
pages, one WAL transaction, and one published CSN.

The implementation is deliberately bounded: relations are binary primary-key
maps, structures are binary scalar values with TTL, and lexical search uses a
deterministic token analyzer over small copy-on-write collections. It proves
the native transaction/recovery architecture; it is not the complete SQL,
Valkey-class structure, or OpenSearch-class search engine.
