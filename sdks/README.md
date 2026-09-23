# SDKs

Hyphae maintains three bounded clients. All expose the published `/v1` API
and the Native `/v2` product API. The table describes the `4.0.0` source
candidate; registry publication remains gated:

| Client | Location | Runtime floor | Runtime dependencies |
|---|---|---:|---|
| Rust | `crates/hyphae-client` | Rust 1.90 | Reqwest/Rustls through Cargo |
| TypeScript | [`typescript`](typescript/README.md) | Node.js 20 | None |
| Python | [`python`](python/README.md) | Python 3.11 | None |

Every client accepts one root HTTP(S) origin, optional bearer authentication,
a request/response deadline, a JSON response bound, and a snapshot witness
bound.
They expose capabilities, liveness, readiness, KV operations, vector-space and
vector mutations, exact retrieval, lexical-index definition, lexical
retrieval, hybrid retrieval, and result/retrieval witness download on `/v1`.
The TypeScript and Python clients' Native `/v2` surface additionally covers
SQL, structures, integrated search, transactions, and proofs. They reject
malformed error envelopes, require a valid `X-Request-Id`, and require
an error envelope's request ID to match its header.

The Rust, TypeScript, and Python source packages are maintained at `4.0.0`.
None has been published at that version. Live crates.io authority remains
bound to `3.0.0`; its historical receipt and distribution boundary are
unchanged. The `3.0.0` TypeScript and Python packages remain source-only and
unpublished to npm or PyPI.

TypeScript and Python preserve the signed 64-bit integer domain and reject
invalid JSON on `/v1`, but their generated success models provide static typing only.
They cast a syntactically valid successful payload without validating its
complete shape at runtime. Applications that require runtime success validation
must add it at their trust boundary. Native `/v2` additionally validates its
typed binary and product-envelope contracts at the SDK boundary.

The `4.0.0` Python and TypeScript Native codecs negotiate protocol minor 9
and retain support for minors 3 through 8. Minor 8 adds embedding profiles;
minor 9 adds catalog-bound embed-and-ingest. Native `u128` identities and
idempotency values are lossless in both SDKs: Python exposes `int`, while
TypeScript exposes `bigint` and does not narrow those fields to `number`.

TypeScript/Python models are generated from canonical JSON Schema and checked
in. Regenerate after contract changes and verify no drift:

```bash
python tools/generate_sdk_models.py
python tools/generate_sdk_models.py --check
```

All clients pass the same live black-box fixture as remote CLI and MCP. No SDK
opens storage, imports engine internals, or requires an optional provider. See
[public clients](../docs/clients/v1.md) and [API v1](../docs/api/v1.md).
