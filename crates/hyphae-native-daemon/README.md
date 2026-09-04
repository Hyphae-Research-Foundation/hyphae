# Hyphae Native daemon

```toml
[dependencies]
hyphae-native-daemon = "=3.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Multi-client local transport adapter over one `NativeProductService` owner.
It binds filesystem UDS endpoints with mode `0600` on Unix and safe named-pipe
listeners with a protected owner/system DACL on Windows. Every operation is
decoded to `ProductOperation` and dispatched through the product service.

The daemon contains no relational, structure, search, storage, or durability
implementation.
