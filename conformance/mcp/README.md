# MCP host conformance

`corpus.json` is the single behavioral corpus for Codex and Claude Code. The
in-repository adapters under `adapters/` drive the hosts' own control planes;
they never start or call `hyphae mcp` directly.

The host installation is immutable evidence:

- `hosts/package.json` pins `@openai/codex` `0.147.0` and
  `@anthropic-ai/claude-code` `2.1.233` exactly.
- `hosts/package-lock.json` binds all npm and platform package integrities.
- `hosts/install-lock.json` binds package/version, exact `--version` output,
  expected executable basename, and native executable SHA-256 per CI platform.
- Install with `npm ci --ignore-scripts`. The adapters execute the locked
  platform-native package binary, so Claude Code's postinstall copy is neither
  necessary nor trusted.

Codex is installed through its real local marketplace and plugin interface in a
temporary `CODEX_HOME`. The adapter then uses app-server JSONL `initialize`,
`plugin/list`, `plugin/read`, an ephemeral `thread/start` with no turn,
`mcpServerStatus/list` at `toolsAndAuthOnly`, and `mcpServer/tool/call`.

Claude Code runs with `-p --input-format stream-json --output-format
stream-json --plugin-dir plugins/hyphae`. The adapter sends only control-plane
`initialize`, `mcp_status`, and namespaced `mcp_call` requests. Any assistant,
user, result, or Codex turn frame is rejected as evidence that a model path was
entered. Neither host receives an OpenAI or Anthropic API key.

Run the direct MCP suite first, then both real hosts:

```bash
cargo test -p hyphae-cli native_mcp --locked
evidence="${TMPDIR:-/tmp}/hyphae-mcp-evidence"
mkdir -p "$evidence"
python tools/run_mcp_host_conformance.py \
  --host claude-code \
  --output "$evidence/claude-code.receipt.json" \
  --transcript "$evidence/claude-code.transcript.json"
python tools/run_mcp_host_conformance.py \
  --host codex \
  --output "$evidence/codex.receipt.json" \
  --transcript "$evidence/codex.transcript.json"
python tools/check_mcp_host_receipts.py --evidence "$evidence"
```

`HYPHAE_NATIVE_API_KEY_FILE` must point to the restricted Auditor key file and
is inherited by the plugin's `hyphae mcp` process. The config never contains or
hardcodes a credential. The runner and both adapters scan host output,
transcript, and receipt bytes for the exact credential and for any `hyp1_`
credential-shaped value.

Clean worktrees produce `source_mode: clean`. Pull-request jobs can pass
`--allow-integration-tree`; this computes an isolated temporary Git index over
the exact current tracked and untracked bytes and binds its tree object plus the
adapter digest. This supports an explicitly dirty integration checkout without
claiming that `HEAD^{tree}` was tested. The checker must receive the same flag.
