# SDKs

Hyphae ships three bounded clients for the public `/v1` API:

| Client | Location | Runtime floor | Runtime dependencies |
|---|---|---:|---|
| Rust | `crates/hyphae-client` | Rust 1.89 | Reqwest/Rustls through Cargo |
| TypeScript | [`typescript`](typescript/README.md) | Node.js 20 | None |
| Python | [`python`](python/README.md) | Python 3.11 | None |

Every client accepts one root HTTP(S) origin, optional bearer authentication,
a request/response deadline, a JSON response bound, and a snapshot witness
bound.
They expose capabilities, liveness, readiness, KV operations, vector-space and
vector mutations, exact retrieval, lexical-index definition, lexical
retrieval, hybrid retrieval, and result/retrieval witness download. They
reject malformed error envelopes, require a valid `X-Request-Id`, and require
an error envelope's request ID to match its header.

The Rust client follows the crates.io release procedure. The TypeScript and
Python packages are maintained as source packages in this repository; this
documentation does not claim an npm or PyPI publication without a separate
registry release and receipt.

TypeScript and Python preserve the signed 64-bit integer domain and reject
invalid JSON, but their generated success models provide static typing only.
They cast a syntactically valid successful payload without validating its
complete shape at runtime. Applications that require runtime success validation
must add it at their trust boundary; full validators are outside this `0.2.1`
source-only patch.

TypeScript/Python models are generated from canonical JSON Schema and checked
in. Regenerate after contract changes and verify no drift:

```bash
python tools/generate_sdk_models.py
python tools/generate_sdk_models.py --check
```

All clients pass the same live black-box fixture as remote CLI and MCP. No SDK
opens storage, imports engine internals, or requires an optional provider. See
[public clients](../docs/clients/v1.md) and [API v1](../docs/api/v1.md).
