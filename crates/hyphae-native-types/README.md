# hyphae-native-types

```toml
[dependencies]
hyphae-native-types = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Internal, unpublished canonical identities, logical-type descriptors,
primitive scalar storage codecs, and memcomparable ordered-index components for
the native Hyphae substrate.

The normative target contract is
[`docs/native/types-v1.md`](../../docs/native/types-v1.md). JSON, nested
collection, and vector value codecs remain explicit future work. This crate is
G1 implementation evidence and is not part of the published `0.2.1` API.
