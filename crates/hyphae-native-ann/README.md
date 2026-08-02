# hyphae-native-ann

`hyphae-native-ann` is Hyphae's owned deterministic HNSW kernel. It provides:

- canonical finite `f32` vectors;
- cosine, negative dot-product, and squared-L2 distance;
- deterministic level selection and rebuild order;
- bounded HNSW construction and search;
- an exact full-scan quality oracle;
- updates and deletes through canonical graph rebuilds;
- explicit approximation, breadth, candidate, rerank, and build-identity
  receipts; and
- persistence-facing vector and graph records with fail-closed canonical
  restore.

The crate is intentionally unpublished while the native search engine binds
these records to its page store, WAL, MVCC snapshots, and corruption recovery.
It contains no external ANN implementation.

Run its focused verification with:

```text
cargo test -p hyphae-native-ann
cargo clippy -p hyphae-native-ann --all-targets -- -D warnings
```

The deterministic quality smoke is an observation rather than a release gate:

```text
cargo run --release -p hyphae-native-ann --example quality_smoke -- <commit> <environment>
```
