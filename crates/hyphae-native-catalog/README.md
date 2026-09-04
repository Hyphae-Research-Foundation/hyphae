# hyphae-native-catalog

```toml
[dependencies]
hyphae-native-catalog = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Internal, unpublished immutable catalog model shared by Hyphae's native
relational, structure, and search engines. It owns the bounded canonical
`HYCOBJ01` definition codec used by legacy WAL mutations, and the additive
logical `HYCOBJ02` codec used by the G6 catalog model and `HYCAT006` views.

The normative target contract is
[`docs/native/catalog-v1.md`](../../docs/native/catalog-v1.md). The current
runtime catalog tree is `HYCAT006`. Existing canonical `HYCOBJ01` definitions
remain byte-preserved and have deterministic compatible logical V2 views.
`CatalogObject::encode_definition` and
`CatalogObject::decode_definition` therefore retain their existing behavior and
bytes.

The additive V2 model supplies database/schema hierarchy, first-class
keyspaces, reusable analyzers, complete stored/doc-values/source/lexical field
policy, multiple named vectors, exact/ANN/adaptive selection, incremental
lifecycle policy, stable definition versions and SHA-256 digests, object kinds,
and bidirectional dependency derivation. Existing objects can be wrapped
losslessly with `encode_definition_v2`; logical definitions use strict
`HYCOBJ02` decode and canonical re-encoding checks. Lifecycle policy enforces
the durable delta ceiling, threshold ordering, and retained-generation bound.
