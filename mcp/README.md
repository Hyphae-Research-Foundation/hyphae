# Native MCP adapter

`hyphae mcp` is a bounded, read-only stdio adapter for an already running
managed Native HTTP v2 service. It never opens a data directory, starts a
listener, accepts a format-2 bearer token, or exposes Native write operations.

## Start and configure

```bash
hyphae serve --data-dir ./hyphae-data \
  --endpoint ./hyphae.sock \
  --http-bind 127.0.0.1:8787 \
  --native-api-key-auth

hyphae mcp --base-url http://127.0.0.1:8787 \
  --native-api-key-file ./auditor.key
```

Plaintext `http://` is accepted only for a canonical loopback host
(`127.0.0.0/8`, `[::1]`, or exact `localhost`). Any other MCP base URL must use
`https://`; the adapter rejects a remote plaintext origin before sending the
durable API key.

Use a built-in Auditor assignment at Instance scope so both security tools
receive `security.read` without receiving mutation authority. The key file must
be a restricted regular file and must contain exactly one durable `hyp1_...`
credential. `HYPHAE_NATIVE_API_KEY_FILE` may replace the file option. The
credential is never accepted as an argv value or ordinary environment value.
`--native-api-key-stdin` instead consumes the first bounded stdin line as the
credential; subsequent lines are MCP messages.

The API key establishes the only authority used by all tools in that process.
Tool arguments and model-generated prompt input cannot supply another key,
principal, role, or permission. Unknown input fields fail closed.

## Protocol and contract

- MCP revision: `2025-11-25`.
- Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout.
- Maximum complete input message: 4 MiB.
- Lifecycle: `initialize`, `notifications/initialized`, `ping`, `tools/list`,
  and `tools/call`.
- `tools/list` returns a fixed maximum of two definitions per page. Only an
  opaque cursor emitted at that boundary is accepted.
- MCP tasks are forbidden.

The exact tool definitions live in
[`contracts/native-mcp-v2.json`](../contracts/native-mcp-v2.json).
`initialize`, every tool page, and every tool result report
`hyphaeToolSchemaVersion` plus the BLAKE3 digest of those exact contract bytes.

| Tool | Native v2 operation | Authority |
|---|---|---|
| `hyphae_native_capabilities` | `Capabilities` | `discover` |
| `hyphae_native_security_status` | `SecurityStatus` | `security.read` at Instance |
| `hyphae_native_security_principals` | `SecurityPrincipalList` | `security.read` at Instance |

All three tools are read-only, non-destructive, idempotent, closed-world, and
task-forbidden. Principal pages are bounded by `limit` (`1..=1000`) and use the
opaque, authorization-epoch-bound Native security cursor.

## Errors and redaction

Malformed JSON-RPC or tool envelopes receive normal JSON-RPC errors. A valid
tool call rejected by Native returns `isError: true` and structured content:

```json
{
  "schema": "hyphae-native-mcp-tool-error-v1",
  "error": {
    "code": "authorization_denied",
    "category": "authorization",
    "message": "operation is not authorized",
    "retry": "never",
    "transaction_state": "none",
    "request_id": null,
    "trace_id": null,
    "object_id": null,
    "transaction_id": null
  }
}
```

Every advertised `outputSchema` is an explicit `oneOf`: the tool's exact
success DTO or this typed error envelope. The text content is the compact JSON
encoding of the same `structuredContent`; it is not a second error contract.
The error fields come from the Native `ProductError` registry. The adapter does
not return, log, or interpolate the API key. Security results contain only the
redacted Native DTOs.

## Verification

The CLI tests start one bootstrapped Native product, use an Auditor credential
through managed HTTP v2, prove bounded tool pagination, preserve typed Native
authorization denial, reject prompt-supplied escalation fields, and scan both
stdout and stderr for the credential canary.

```bash
cargo test -p hyphae-cli native_mcp --locked
cargo clippy -p hyphae-cli --all-targets --locked -- -D warnings
```

See the [CLI reference](../docs/cli/reference.md) and
[configuration reference](../docs/configuration.md).
