# Optional framework adapters

Hyphae integrations are consumers, never core dependencies. Omit them and the
host application behaves exactly as before. The Rust adapter is published as
`hyphae-pliegors`; the JavaScript adapters remain source packages in this
repository until they receive an independent registry release.

## LangChain

```python
from hyphae_sdk.v2 import HyphaeClient
from hyphae_sdk.langchain import HyphaeVectorStore

with HyphaeClient.local("/run/hyphae.sock") as client:
    store = HyphaeVectorStore(client, 13, embeddings, prove=True)
    store.add_texts(["deterministic retrieval"], metadatas=[{"kind": "engine"}])
    documents = store.similarity_search("retrieval", k=4)
```

The store needs the `langchain` extra (`pip install 'hyphae-sdk[langchain]'`).
Documents ingest under content-derived BLAKE3 identities, queries run as
hybrid retrieval by default, and with `prove=True` every returned document
carries the sealed proof's digest in its metadata while `store.last_proof`
holds the artifacts for offline verification with `hyphae proof verify`.
Metadata keys listed in `metadata_fields` are persisted as typed doc-values
(the collection schema must declare them); everything else stays
adapter-side.

## PliegoRS boundary

Add `hyphae-pliegors` only in an application that wants remote Hyphae access.
The crate has no PliegoRS dependency and wraps only `hyphae-client`.

```rust,no_run
use hyphae_pliegors::PliegoHyphaeConfig;

# fn configure() -> Result<(), Box<dyn std::error::Error>> {
if let Some(config) = PliegoHyphaeConfig::from_env()? {
    let optional_state = config.build()?;
    // Register `optional_state` through the application's public state API.
}
# Ok(())
# }
```

Both variables absent means disabled. `HYPHAE_BASE_URL` enables the adapter;
`HYPHAE_BEARER_TOKEN` is optional and never selects an implicit endpoint.

## Astro

```typescript
import { createHyphaeAstroMiddleware } from "@hyphae_/hyphae-integrations/astro";

export const onRequest = createHyphaeAstroMiddleware({
  baseUrl: "http://127.0.0.1:8787",
});
```

The middleware attaches one public client to `Astro.locals.hyphae` and refuses
to overwrite existing host state.

## Next

```typescript
import { createHyphaeNextClientFromEnv } from "@hyphae_/hyphae-integrations/next";

const client = createHyphaeNextClientFromEnv();
```

Use this only in server components, route handlers, or other server-only code.
Keep `HYPHAE_BASE_URL` and `HYPHAE_BEARER_TOKEN` private; never use a
`NEXT_PUBLIC_` prefix.

## Vite

```typescript
import { defineConfig } from "vite";
import { hyphaeVite } from "@hyphae_/hyphae-integrations/vite";

export default defineConfig({
  plugins: [hyphaeVite({ target: "http://127.0.0.1:8787" })],
});
```

Browser code uses `@hyphae_/hyphae-integrations/vite/client`. It reaches `/v1`
through the same origin and cannot accept a bearer token. Production proxying
must be configured by the deployment host.

## Verification

```bash
python tools/check_integration_boundaries.py
(cd integrations/javascript && npm ci --ignore-scripts && npm test)
(cd integrations/host-smoke && npm ci --ignore-scripts && npm test)
cargo build -p hyphae-cli --locked
python tools/run_integration_conformance.py
```
