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

The plugin targets the versioned Native HTTP v2/RBAC MCP contract. By default
it exposes eight read-only tools: `hyphae_native_capabilities`, redacted
security status (`hyphae_native_security_status`), a paginated redacted
principal list (`hyphae_native_security_principals`), one bounded lexical
query (`hyphae_native_search_lexical`), one integrated collection search with
typed filters, facets, and per-branch recall evidence
(`hyphae_native_search_collection`), a sealed offline-verifiable search proof
(`hyphae_native_prove_search`), trustless local proof verification
(`hyphae_native_verify_proof`), and Agent Memory recall
(`hyphae_native_memory_recall`). Three write-scoped tools —
`hyphae_native_search_ingest`, `hyphae_native_memory_store`, and
`hyphae_native_memory_forget` — are listed only when the adapter is started
with `--allow-ingest`; the checked-in plugin never passes that flag.

The API key's durable roles remain the sole authority; prompt text cannot
select a role or widen its permissions. The Auditor key covers the capability
and security tools. The search tools (`hyphae_native_search_lexical`,
`hyphae_native_search_collection`) require the `search.execute` authority,
which the built-in Reader role carries. `hyphae_native_prove_search` also
requires `proof.generate`, which the Reader role carries alongside
`search.execute`. The write-scoped ingest and memory-store/forget tools
require a key with `data.write` authority (the built-in Writer role) in
addition to `search.execute`; a Reader or Auditor key receives a typed
denial. Issue the key whose authority matches the tools the agent needs.
Legacy bearers are not accepted, and no write tool is reachable unless the
operator both starts the adapter with `--allow-ingest` and presents a
Writer-authorized key.

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
/plugin marketplace add Hyphae-Research-Foundation/hyphae
/plugin install hyphae@hyphae
```

Claude Code reads `.claude-plugin/plugin.json`, the shared `.mcp.json`, and
the namespaced `use-hyphae` skill.

The shared host corpus and receipt runner live under `conformance/mcp`. A host
receipt is valid only when the installed host exposes deterministic
machine-readable MCP evidence; unsupported or missing host evidence fails
closed and is never replaced by a direct server simulation.

## Agent Memory

This bundle's `.mcp.json` always starts the full profile above; it does not
configure the separate Agent Memory five-verb surface. To wire an agent host
to Agent Memory (store, recall, journal, forget, and status over physically
separated personal/work/journal collections), run
`hyphae agent configure <host>`, or start `hyphae mcp --profile memory`
directly with `--allow-write` and the three
`--personal-memory-collection`/`--work-memory-collection`/
`--journal-memory-collection` flags. See
[`docs/product/agent-memory.md`](../../docs/product/agent-memory.md).
