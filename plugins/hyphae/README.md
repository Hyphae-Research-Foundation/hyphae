# Hyphae agent plugin

This one plugin directory supports Codex and Claude Code. Both hosts start the
same `hyphae mcp` stdio server from [`.mcp.json`](.mcp.json); there is no
host-specific tool implementation.

## Prerequisites

1. Install the exact `hyphae` binary and ensure it is on `PATH`.
2. Start a local format-2 HTTP service on `http://127.0.0.1:8787`.
3. If authentication is enabled, set `HYPHAE_BEARER_TOKEN_FILE` to a
   restricted credential file before starting the agent host.

The first plugin version intentionally targets the shipped `/v1` MCP contract.
The Native v2/RBAC MCP migration remains an explicit 1.2.0 gate; this package
does not pretend the legacy bearer is a Native role-scoped API key.

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
