# ADR-0011: Optional, host-owned framework integrations

- Status: Accepted
- Date: 2026-07-15
- Owners: Celiums Solutions LLC

## Context

PliegoRS, Astro, Next, and Vite must remain complete products when Hyphae is
absent. An adapter that imports storage internals, owns application state, or
injects secrets into browser code would turn an optional integration into a
hidden runtime dependency.

## Decision

All framework adapters live under `integrations/`, outside the engine crates.
They consume only `hyphae-client` or the public `@hyphae_/hyphae` SDK. A static
boundary check rejects private crate dependencies, deep JavaScript imports,
and references back into core source directories.

`hyphae-pliegors` is a small configuration and application-state wrapper. It
does not depend on PliegoRS or inspect its internals. Applications explicitly
add the wrapper to their own state mechanism. Missing configuration is a
normal disabled state, and a token without an explicit origin is rejected.

The JavaScript adapter package publishes independent subpaths:

- Astro middleware places a public client in a caller-selected `locals` key;
- Next creates a server-only client from explicit inputs or private runtime
  environment values;
- Vite configures only a `/v1` development/preview proxy and never accepts a
  bearer token;
- the Vite browser helper uses a same-origin public client with no bearer
  option.

Astro, Next, and Vite are optional peer dependencies. Host-only fixtures have
a separate lockfile containing no Hyphae SDK or adapter package, and must all
produce successful production builds. A separate live suite exercises every
JavaScript adapter against the public `/v1` server.

## Consequences

- Installing or running any host framework never installs or starts Hyphae.
- Enabling an adapter is an explicit application decision.
- Production reverse-proxy and secret management remain host responsibilities.
- Browser integrations cannot carry Hyphae bearer credentials.
- Framework-specific convenience may evolve without changing core storage or
  query contracts.

## Verification

`tools/check_integration_boundaries.py` enforces dependency direction and the
host-only fixture. `integrations/host-smoke` builds current pinned Astro, Next,
and Vite applications without Hyphae. `tools/run_integration_conformance.py`
starts an isolated server and verifies Astro, Next, and Vite through `/v1`.
The Rust workspace tests verify disabled configuration and secret redaction for
the PliegoRS boundary.
