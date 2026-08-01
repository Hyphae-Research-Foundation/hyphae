# hyphae-native-blobs

This unpublished crate owns Hyphae's first immutable content-addressed blob
store. Blob files bind an exact logical length, stable `BlobId`, CRC32C, and
BLAKE3 content digest. Publication uses a synchronized create-new temporary
file followed by same-directory rename.

The implementation deliberately reads and verifies complete blobs. Streaming,
chunk trees, compression, encryption, retention tracing, and large-corpus
garbage collection remain later work.
