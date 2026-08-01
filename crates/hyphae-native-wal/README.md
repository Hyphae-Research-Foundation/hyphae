# hyphae-native-wal

Internal, unpublished block-framed WAL codec and append/recovery file for the
native Hyphae transaction substrate.

The normative target contract is
[`docs/native/wal-format-v1.md`](../../docs/native/wal-format-v1.md). This
crate proves framing, integrity, tail repair, and record ordering only; it does
not yet close cross-engine commit or G1.
