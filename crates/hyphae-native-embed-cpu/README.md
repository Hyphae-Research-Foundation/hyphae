# hyphae-native-embed-cpu

```toml
[dependencies]
hyphae-native-embed-cpu = "=4.0.0"
```

Optional, safe Rust, in-process CPU execution for catalog-bound
Qwen3-Embedding-0.6B profiles in `hyphae-native-product`.

The crate never downloads a model or starts a listener. Operators provide a
complete local snapshot and its manifest. Loading verifies the manifest and
every snapshot file through one set of open descriptors before the model is
made available in the process-local registry.

Code is Apache-2.0; documentation is CC-BY-SA-4.0.
