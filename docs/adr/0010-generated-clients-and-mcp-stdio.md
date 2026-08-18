# ADR-0010: Generated clients, common conformance, and MCP stdio

- Status: Accepted
- Date: 2026-07-15
- Owners: Celiums Solutions LLC

## Context

The Rust client, TypeScript SDK, Python SDK, command-line client, and MCP
adapter must behave as equivalent optional consumers of `/v1`. Handwritten
wire models would drift independently, language-native JSON can lose integer
precision, and an MCP implementation coupled to storage internals would create
a second unversioned API.

## Decision

JSON Schema 2020-12 under `contracts/json-schema/` remains canonical. The
standard-library generator `tools/generate_sdk_models.py` reads every version
1 schema, rejects conflicting model names, records a deterministic aggregate
contract digest, and emits checked-in TypeScript and Python models. CI runs the
generator in `--check` mode.

The Rust HTTP client depends only on `hyphae-contracts`; TypeScript and Python
have no runtime package dependencies. All clients enforce root HTTP(S)
origins, deadlines, bounded response streams, strict JSON media types, request
correlation, stable error envelopes, canonical witness references, and witness
digest headers.

HTTPS uses rustls with the bundled Mozilla/CCADB trust-anchor dataset from
`webpki-roots`. Its permissive data license, `CDLA-Permissive-2.0`, is explicit
in `deny.toml` and `NOTICE`; it does not change Hyphae's Apache-2.0 source
license. This avoids a platform-specific OpenSSL runtime dependency while
keeping one cross-platform client behavior.

Hyphae JSON numbers are signed integers, not IEEE-754 measurements. Python and
Rust preserve them natively. TypeScript uses `JsonInteger = number | bigint`
and a strict codec: safe integers remain `number`, larger integer tokens become
`bigint`, and unsafe JavaScript numbers fail instead of rounding.

The CLI exposes `hyphae remote` over the Rust public client. Secrets are read
from the existing environment/file mechanism and never accepted as a command
argument.

MCP is an optional `hyphae mcp` stdio mode in the same executable. It opens no
data directory and calls only the public HTTP client. It implements the stable
MCP revision `2025-06-18` as newline-delimited UTF-8 JSON-RPC 2.0, advertises
only tools, caps each input message at 4 MiB, embeds the canonical input/output
schemas, returns both `structuredContent` and a JSON text fallback, and marks
mutation tools as destructive. Streamable HTTP and experimental MCP tasks are
not implemented.

One fixed black-box fixture exercises every consumer against a fresh server
and data directory. The suite covers capabilities, atomic put, idempotent
retry/conflict, proven presence/absence, deterministic pagination, grouped
aggregation with missing/null identity, delete, and witness download where the
surface supports binary transfer.

## Consequences

- A schema change that does not regenerate both language surfaces fails CI.
- SDKs can be installed independently without the server or engine packages.
- TypeScript callers must use `bigint` for document integers outside the safe
  JavaScript range; JSON transport remains interoperable integer text.
- MCP hosts spawn one local adapter process and separately configure the
  Hyphae HTTP origin; MCP is never an alternate storage authority.
- Every conformance client starts with isolated durable state, so fixed UUIDs
  and exact expected results remain deterministic.

## Verification

`tools/run_conformance.py` starts a fresh loopback server for the Rust,
TypeScript, Python, and CLI `/v1` clients, then requires the same versioned
fixture to pass. MCP has since moved to a managed Native v2 contract and is
verified by its authority, plugin, and live CLI gates. SDK unit tests cover
origin/secret validation, error correlation, stream bounds, and lossless
signed-64-bit TypeScript JSON. Rust stable/MSRV tests and Clippy cover the
single-binary CLI/MCP implementation.
