# hyphae-native-pages

```toml
[dependencies]
hyphae-native-pages = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Internal, unpublished page codec, append-only page file, and partitioned
buffer pool for the native Hyphae substrate.

The normative target contract is
[`docs/native/page-row-blob-format-v1.md`](../../docs/native/page-row-blob-format-v1.md).
This crate is part of the retained closed G1 substrate evidence. G7 performance
and G8 release closure remain separate from this page-codec boundary.
