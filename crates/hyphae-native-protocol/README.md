# Hyphae Native protocol

```toml
[dependencies]
hyphae-native-protocol = "=4.0.0"
```

The workspace pins every internal crate, including this one, to this exact
version.

Portable `HYPHLCL1` frame, handshake, product request/response, `HYPERR01`,
stream completion, flow-control, cancellation, and deadline codecs.

The current codec negotiates minor 9 while retaining minors 0 through 8.
Minor 8 gates embedding-profile catalog content; minor 9 adds catalog-bound
embed-and-ingest request tag 73 and response tag 47. Its
unreleased transaction-document golden and malformed ordering vectors are
authoritative under `tests/fixtures/`; any cross-language compatibility mirror
must remain byte-identical to those crate-local copies.

This crate is a transport boundary. It delegates frame compatibility to the
current native runtime codec and carries only product-owned operations. It does
not implement data-engine behavior or engine-to-engine communication.
