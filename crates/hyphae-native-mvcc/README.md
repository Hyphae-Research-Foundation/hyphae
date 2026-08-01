# hyphae-native-mvcc

Internal, unpublished snapshot/version semantics and atomic cross-engine root
publication for the native Hyphae substrate.

The normative contract is
[`docs/native/mvcc-commit-v1.md`](../../docs/native/mvcc-commit-v1.md). The
current implementation serializes root publication and uses a short-lived
standard-library `RwLock` for snapshot acquisition; conflict tables, WAL
coordination, and lock-free publication remain required before G1 closes.
