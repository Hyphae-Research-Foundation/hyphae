# hyphae-native-wal

```toml
[dependencies]
hyphae-native-wal = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Internal, unpublished block-framed WAL codec and append/recovery file for the
native Hyphae transaction substrate.

The normative target contract is
[`docs/native/wal-format-v1.md`](../../docs/native/wal-format-v1.md). This
crate supplies framing, integrity, tail repair, record ordering, and the WAL
side of the cross-engine commit protocol retained by the closed G1 and G5
gates.
