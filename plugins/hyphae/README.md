# Hyphae agent plugin

This one plugin directory supports Codex and Claude Code. Both hosts start the
same `hyphae mcp` stdio server from [`.mcp.json`](.mcp.json); there is no
host-specific tool implementation.

## Prerequisites

1. Install the exact `hyphae` binary and ensure it is on `PATH`.
2. Start a bootstrapped Native HTTP v2 service on `http://127.0.0.1:8787`.
3. Assign the built-in Auditor role at Instance scope and set
   `HYPHAE_NATIVE_API_KEY_FILE` to its restricted API-key file before starting
   the agent host. The inherited variable contains a path, never the credential
   value.

The checked-in plugin uses plaintext only at the canonical loopback origin.
If `--base-url` is overridden for a remote service, it must be an `https://`
origin; the MCP adapter rejects remote plaintext before sending the key.

The plugin targets the versioned Native HTTP v2/RBAC MCP contract. It exposes
five bounded read-only tools: capabilities, redacted security status, a
paginated redacted principal list, one bounded lexical query, and one
integrated collection search with typed filters, facets, and per-branch
recall evidence. The API key's durable roles remain the sole authority;
prompt text cannot select a role or widen its permissions. The Auditor key
covers the capability and security tools; the two search tools require the
`search.execute` authority, which the built-in Reader role carries — issue
the key whose authority matches the tools the agent needs. Legacy bearers
and MCP writes are not accepted.

The server admits one active tool call and one pending response, limits complete
input and output messages to 4 MiB, and handles idempotent cancellation while
Native HTTP is in flight. It rejects saturation rather than growing a queue.

## Codex

Validate the bundle with the repository checker, then add the repository
marketplace and install `hyphae@personal`. Codex reads
`.codex-plugin/plugin.json` and the shared `.mcp.json`.

## Claude Code

For local development:

```bash
claude --plugin-dir ./plugins/hyphae
```

For marketplace installation, add this repository and install
`hyphae@hyphae`:

```text
/plugin marketplace add celiumsai/hyphae
/plugin install hyphae@hyphae
```

Claude Code reads `.claude-plugin/plugin.json`, the shared `.mcp.json`, and
the namespaced `use-hyphae` skill.

The shared host corpus and receipt runner live under `conformance/mcp`. A host
receipt is valid only when the installed host exposes deterministic
machine-readable MCP evidence; unsupported or missing host evidence fails
closed and is never replaced by a direct server simulation.
