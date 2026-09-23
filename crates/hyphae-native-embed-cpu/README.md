# hyphae-native-embed-cpu

```toml
[dependencies]
hyphae-native-embed-cpu = "=4.0.0"
```

Optional, safe Rust, in-process CPU execution for catalog-bound
Qwen3-Embedding-0.6B profiles in `hyphae-native-product`. A default-off CUDA
feature adds validated NVIDIA H100 BF16 or FP16 execution:

```toml
[dependencies]
hyphae-native-embed-cpu = { version = "=4.0.0", features = ["cuda"] }
```

The crate never downloads a model or starts a listener. Operators provide a
complete local snapshot and its manifest. Loading verifies the manifest and
every snapshot file through one set of open descriptors before the model is
made available in the process-local registry.

`Qwen3AcceleratorExecutor` automatically selects an H100 80GB with compute
capability 9.0 and reports its CUDA UUID and PCI identity. A CUDA-selected
registry also preloads the same verified model on CPU. An unavailable CUDA
result is discarded and every input is rerun as one complete CPU batch;
cancellation, deadline, validation, and limit failures are not retried. The
execution profile declares this fallback, and no returned batch mixes CPU and
GPU vectors. If no validated H100 can be selected, the registry fixes itself
to the complete CPU path before model loading.

Code is Apache-2.0; documentation is CC-BY-SA-4.0.
