<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Hyphae Agent Memory on Omarchy

Local, shared, and verifiable memory for the coding agents Omarchy
ships: Claude Code stores a decision, Codex recalls it in another
session, OpenCode recalls it next week — all data local, every recall
optionally provable offline. This is an external integration; it
changes nothing in Omarchy itself.

The product contract is
[Hyphae Agent Memory](../product/agent-memory.md); this page is the
operating recipe for an Omarchy machine.

## Install

```bash
omarchy pkg aur add hyphae-bin
hyphae agent setup
```

Setup explains every resource before creating it, asks before enabling
the user service, and prints exact backup and removal instructions.
Then register the memory server with your agents:

```bash
hyphae agent configure claude --apply
hyphae agent configure codex --apply
hyphae agent configure opencode --apply
hyphae agent configure pi --apply
```

Hosts receive recall and status access by default. Grant store, journal, and
forget to one host only when needed with `--access write`. Every host uses the same
local socket and profile; configuration carries only a credential file path,
never a secret.

`--apply` also installs proactive lifecycle integration. Claude Code and Codex
receive command hooks, OpenCode receives a local plugin, and Pi receives a
local extension. Before each prompt they recall relevant project memory and
inject it as delimited, untrusted historical context. After a turn, only
explicit decision/constraint/fact lines and safe successful build/test
commands are considered for capture; prompts and responses are not stored in
full.

Each memory records two additional provenance columns: the harness that
produced it and the model identity reported by that harness. A separate
first-person `journal` layer lets one model record its own reflection for later
models to inspect without mixing that reflection with user/project work memory.
Journal entries are labeled as model-authored historical context and never
override current user instructions.

Automatic capture and injection both pass through fail-closed secret and PII
screening. Email, phone, address, government-ID, payment-card, network-address,
UUID, home-path, and likely credential patterns are discarded. Repository
names and local paths are represented by a private local project digest.
Sanitized capture survives a temporary service outage in the private XDG state
spool and is committed exactly once on the next successful hook invocation.

## Operate

```bash
hyphae agent status               # redacted JSON: paths, service, credentials
hyphae agent doctor               # engine health over the memory directory
hyphae agent backup               # one verified backup under ~/.local/share/hyphae/backups
systemctl --user status hyphae-agent-memory
journalctl --user -u hyphae-agent-memory
```

## Upgrade

```bash
omarchy update                    # updates system and installed AUR packages
hyphae agent upgrade              # stop, backup, doctor, start
```

## Restore

```bash
systemctl --user stop hyphae-agent-memory
hyphae agent restore --backup ~/.local/share/hyphae/backups/agent-memory-<stamp>
systemctl --user start hyphae-agent-memory
```

The previous directory is preserved aside automatically.

## Remove

```bash
hyphae agent remove               # service and credentials; data and backups stay
hyphae agent purge-data           # only this deletes data, after confirmation
```

## Verify a recall

Ask an agent to recall with `prove: true`; the response names the proof
and witness files. Then, without trusting anything:

```bash
hyphae proof verify --proof <file> --witness <file> --anchor <hex>
```

A verified answer reports `semantic_reexecution`.

## Conformance

Every supported host passes the same ten-step corpus against the same
published release — discovery, store, recall, project isolation,
forget, permanence, escalation refusal, bounds, read-only gating, and a
credential-canary transcript scan:

```bash
python3 tools/agent_memory_conformance.py \
  --binary hyphae \
  --writer-key ~/.config/hyphae/credentials/memory-writer.key \
  --reader-key ~/.config/hyphae/credentials/memory-reader.key
```

## Troubleshooting

| Symptom | Check |
|---|---|
| Tools missing in the agent | `hyphae agent status` → `service_active`; the host config points at the writer key for write access |
| `authorization_denied` | The reader profile cannot store or forget; use the writer credential |
| Service will not start | `journalctl --user -u hyphae-agent-memory`, then `hyphae agent doctor` |
| Recall returns nothing | Projects isolate exactly; pass the same project string your agents use |

## Scope

No home-directory indexing, no cloud, no replication, no LLM inside the
engine, and no Omarchy core changes. Uninstalling never deletes your
memories.
