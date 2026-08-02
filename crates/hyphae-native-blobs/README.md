# hyphae-native-blobs

This unpublished crate owns Hyphae's first immutable content-addressed blob
store. Blob files bind an exact logical length, stable `BlobId`, CRC32C, and
BLAKE3 content digest. Publication uses a synchronized create-new temporary
file followed by same-directory rename.

The implementation deliberately reads and verifies complete blobs. The
single-retained-root tracing, committed generation floor, deterministic
partial pruning, and namespace synchronization behavior is specified in
`docs/native/blob-collection-v1.md` and implemented. Streaming, chunk trees,
compression, encryption, automatic scheduling, and multi-root retention
remain later work.
