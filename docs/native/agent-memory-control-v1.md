<!-- SPDX-License-Identifier: Apache-2.0 -->
# Agent Memory operator control v1

`hyphae agent ui` consumes one JSON request from stdin and emits one JSON
response on stdout. Both are bounded to 1 MiB. This local operator surface is
used by the Omarchy panel; it is not an agent tool or a network service.

Machine-readable contracts use JSON Schema 2020-12:

- [Operator requests and responses](../../contracts/json-schema/agent-memory-control-v1.schema.json)
  (`$defs/AgentMemoryControlRequest` and `$defs/AgentMemoryControlResponse`).
- [Attested model manifest](../../contracts/json-schema/hyphae-attested-model-v1.schema.json).
- [Embedding worker requests and responses](../../contracts/json-schema/hyphae-embed-worker-v1.schema.json)
  (`$defs/HyphaeEmbedWorkerRequest` and `$defs/HyphaeEmbedWorkerResponse`).

Schema validation checks shape. UTF-8 byte limits, normalized project identity,
unsigned native object identifiers, path ownership, matching vector dimensions,
and current lifecycle eligibility are additional semantic checks. The
`x-hyphae-maxUtf8Bytes` annotation names a byte limit; JSON Schema `maxLength`
counts Unicode characters. Neither schema validity nor a successful request
grants access beyond the local credential's authority.

```json
{"schema":"hyphae-omarchy-control-v1","id":1,"operation":"status","arguments":{}}
```

Success is `{ "schema": "hyphae-omarchy-control-v1", "id": 1, "ok": true,
"result": {} }`. Failure replaces `result` with `error: {code, message}` and
sets `ok: false`. `code` is stable; diagnostic messages do not contain raw
memory contents or credentials. Optional unsigned `id` is echoed without
interpretation. Unknown envelope fields, operations and typed argument
fields are rejected. The process exits successfully after a typed response;
transport/process failure is distinct from `ok: false`.

| Operation | Arguments | Behavior |
| --- | --- | --- |
| `status`, `projects`, `agents`, `backups` | `{}` | Bounded local state; no credentials or memory text in status |
| `recall`, `list` | Memory recall input: optional `project`, `query`, `limit`, `kind`, `layer`, `mode`, `prove` | Common native snapshot; `list` admits an empty query; final limit 1–64 |
| `store`, `forget` | Existing memory-profile store/forget inputs | Explicit operator-selected mutation |
| `pause` | `paused: bool`, optional `project` | Pause/resume global or selected project automatic capture |
| `configure` | `host: claude/codex/opencode/pi`, `access: read/write` | Install managed MCP and lifecycle integration, preserving unrelated handlers |
| `disconnect` | `host`, optional `access` | Remove only unchanged managed entries; preserve/reject edited entries |
| `setup` | `enable_service: bool` (default false) | Initialize local data/credentials and optionally the user service |
| `service_start` | `{}` | Backup, activate runtime, reconnect managed hosts and refresh the embedding worker |
| `semantic` | `enabled: bool`, optional `model_dir` | Disable semantics or migrate to the verified local model profile |
| `doctor`, `backup` | `{}` | Health report or a verified local backup |
| `restore` | `backup: path`, `confirm: true` | Verify a backup below the managed backup root, preserve the current directory and restore |
| `verify` | `proof: path`, `witness: path`, `anchor: hex` | Offline semantic verification; only memory proofs are accepted by this control action |
| `remove` | `confirm: true` | Remove managed integration and credentials; preserve memory, runtime/model files and backups |

Setup with service activation restores a previously configured semantic worker
after removal. Runtime activation captures human-readable host CLI progress;
stdout remains exactly one operator JSON response even when it reconnects all
managed hosts.

`status.initialized` requires the native directory and its three managed
credentials. Preserved data after removal therefore leads the panel back to
setup, where credentials and services can be restored. A failed backup restore
restarts both previously managed services using the unchanged current profile;
the operation still reports the original restore failure.

Setup, activation and restore wait for the native endpoint after starting the
managed service. Readiness probes use IPC and never open the data directory
offline, so policy reconciliation cannot take its lock away from the starting
daemon. An unavailable policy endpoint remains an availability error instead
of being reported as invalid memory input.
Explicit operator starts clear a prior systemd start-limit failure before
starting the managed unit. Automatic failure restarts retain systemd's normal
rate limits. Enabling semantics starts the worker once per operation.

Restoring a backup made before credentials were rotated or recovered checks
the current managed keys against the restored authority. If they no longer
authenticate, the operator preserves all three files in a private
`credentials-before-restore-*` directory and runs the existing native owner
recovery/setup flow. Agent configuration retains credential file paths;
existing agent sessions must restart to load the recovered credentials.

The native response envelope is limited to 16 MiB, including proof and witness
bytes. An encoding limit produces a terminal `limit_exceeded` response; it
does not leave the client waiting for a response that cannot be transmitted.
Complete directory witnesses can exceed this bound on larger retained
histories even when the query returns few records. Unproved recall remains
available in that case.

`recall` requires a nonempty `query`; `list` also requires `query` but accepts
the empty string. Omitted project names use the shared current-directory
resolver. Identifiers are decimal strings, timestamps ending in `_micros` are
Unix microseconds, and proof digests/anchors are lowercase hexadecimal. An
omitted or null request `id` produces a null response `id`. An envelope that
cannot be parsed has `invalid_request` and may omit `id` entirely. The bridge
adds its own correlation identifier when it forwards that response.

Runtime/model installation is owned by the Omarchy plugin, not the engine.
The plugin bridge adds `install` and `install_model` using its reviewed
artifact inventories. It invokes the operator binary with a fixed argument
vector and forwards data through stdin, never through shell interpolation.

The native key `hyphae-agent-policy/v1` is authoritative for physical
collection aliases and semantic model identity. The private JSON policy file
is the local capture preference/cache. Backups reconcile these before storing
the profile; a stale file cannot roll back a committed native alias change.
Restore reloads the backed-up native profile. An independently started daemon
must release the directory lock before offline migration or restore.

`hyphae agent maintain` drains sanitized durable captures and conditionally
enriches up to 16 live records per pass. The managed user timer runs every five
seconds. `hyphae agent recover` resolves a committed semantic cutover before
service start. A missing model or failed worker leaves lexical recall usable.

Claude Code and Codex registrations use their official CLI. Codex hooks require
review in `/hooks`; installing or updating this integration does not bypass
host trust. OpenCode contributes MCP through its local plugin config hook.
Pi loads the official extension API and keeps MCP stdin open until the tool
response arrives; EOF is cancellation, not a request to drain pending work.

The reference embedding component exposes `hyphae-embed model-info` and
`hyphae-embed serve --model-dir PATH --endpoint PRIVATE_SOCKET`. It loads one
CPU model, validates bounded NDJSON requests, and returns the attested file
manifest/fingerprint with vectors. No alternate embedding engine, network
provider or cloud account is required.

The Linux worker accepts one newline-terminated JSON request per connection.
The private parent directory is owned by the current user with mode 0700 and
the socket uses mode 0600. Input is bounded to 2 MiB including its terminating
newline; encoded responses are bounded to 16 MiB before the newline. Reads
time out after one second and writes after two seconds. One loaded model
executes one inference operation at a time.

Worker requests use `hyphae-embed-request-v1`, a nonzero unsigned 64-bit `id`,
and `status`, `embed`, or `rerank`. `status` admits no text or query. Inference
accepts 1–256 texts, each at most 65,536 UTF-8 bytes; `rerank` also requires a
query within that byte bound. `expected_model`, when present and non-null,
must match the model fingerprint. The worker echoes the identifier in a
`hyphae-embed-response-v1` envelope; invalid framing uses identifier zero.
Errors contain a stable code, without input text. An embedding result has one
vector per input, each of the manifest's dimensionality; reranking has one
score per input. Both include the manifest and `HYATTS01` attestation as hex.

The model fingerprint is BLAKE3 over the exact byte sequence
`hyphae-embed-model-v1\0bert-mean-pool-l2-cpu-v1\0` followed by the 32-byte
weights, config and tokenizer BLAKE3 digests, in that order. It excludes the
directory's display name. The manifest records all three file digests,
dimensions, positional capacity, CPU pipeline and runtime version. The
standalone `model-info` command emits this manifest without a worker envelope.
File identity and retrieval provenance do not independently prove that a
particular model execution occurred.

## Explicit semantic retrieval

`recall` and `list` also accept `mode: "semantic"`. When the local worker is
available, this selects the native vector branch without a lexical branch,
so shared words in one language cannot displace a closer cross-language
embedding match through fusion. `hybrid` retains lexical/vector RRF and
`lexical` retains text search. An unavailable worker still falls back to
lexical recall. Empty-query browsing remains lexical.

The `semantic` operator action optionally accepts `mode: "hybrid" | "semantic"`
to choose the default for agent recalls that omit a mode. Existing profiles
default to hybrid. Status reports `semantic_mode`. Hybrid mode is omitted
from serialized policy for compatibility; a profile explicitly selecting
semantic mode requires a supporting runtime. Select hybrid again before
rolling back to an older runtime. Native wire and durable formats are unchanged.
