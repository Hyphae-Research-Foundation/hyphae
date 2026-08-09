# hyphae-native-mvcc

Internal, unpublished snapshot/version semantics and atomic cross-engine root
publication for the native Hyphae substrate.

The normative contract is
[`docs/native/mvcc-commit-v1.md`](../../docs/native/mvcc-commit-v1.md). The
current implementation serializes root publication and uses a short-lived
standard-library `RwLock` for snapshot acquisition. Conflict validation, WAL
coordination, retained snapshots, and atomic all-engine publication are part
of the closed G1 and G5 evidence. Lock-free publication is not a 1.0 contract.
