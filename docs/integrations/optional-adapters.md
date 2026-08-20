# Optional framework adapters

Hyphae integrations are consumers, never core dependencies. Omit them and the
host application behaves exactly as before. The Rust adapter is published as
`hyphae-pliegors`; the JavaScript adapters remain source packages in this
repository until they receive an independent registry release.

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
