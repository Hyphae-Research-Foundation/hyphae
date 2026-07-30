# Phase 7 optional-integration gate

Status: complete. Optional adapters pass public-boundary and live conformance
checks, and Astro, Next, and Vite build without Hyphae installed.

## Boundaries covered

- `hyphae-pliegors` depends only on the public Rust HTTP client and no PliegoRS
  internal code;
- Astro middleware attaches an explicit public client without owning host
  state;
- Next construction is server-only and reads no public browser-prefixed
  secret;
- Vite proxies only `/v1`, never accepts a bearer token, and offers a
  same-origin browser helper;
- all framework packages are optional peers outside the core workspace;
- separate host fixtures install and build with no Hyphae package present.

## Local evidence

```bash
python tools/check_integration_boundaries.py
cargo test -p hyphae-pliegors --locked
(cd integrations/javascript && npm ci --ignore-scripts && npm audit --audit-level=moderate && npm test)
(cd integrations/host-smoke && npm ci --ignore-scripts && npm audit --audit-level=moderate && npm test)
cargo build -p hyphae-cli --locked
python tools/run_integration_conformance.py
```

The host-only suite produces static production builds with Astro 7.1.3, Next
16.2.12, and Vite 8.1.4. Its isolated lockfile contains no Hyphae SDK or
adapter. The live suite then validates all three JavaScript adapters against a
fresh loopback `/v1` server and data directory.

## Explicit limits

The PliegoRS boundary intentionally provides no internal framework wiring;
applications register it through their own public state mechanism. Vite's
proxy covers development and preview only. Package publication and production
archive signing belong to phase 8.
