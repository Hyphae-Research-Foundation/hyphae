<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Hyphae Agent Memory — product contract

Local, shared, and verifiable memory for coding agents.

## The problem and the primary workflow

Coding agents forget everything between sessions, and every agent forgets
separately: a decision recorded in one Claude Code session is invisible to
Codex an hour later and to OpenCode next week. The result is repeated
questions, contradicted decisions, and per-agent silos of context.

Agent Memory is one local engine all agents share:

1. Claude Code stores a technical decision.
2. Codex recalls it in another session.
3. OpenCode recalls it later in the same project.
4. The user inspects, expires, forgets, backs up, and restores memories.
5. All data stays local, and a recall can carry an offline-verifiable
   proof.

## Supported hosts and non-goals

Hosts, in priority order: Claude Code, Codex, OpenCode, Pi. Other hosts
follow demonstrated demand. All hosts speak to the same MCP binary with
the same profile; no host receives host-specific memory semantics.

Non-goals for this program:

- No automatic indexing of the user's home directory.
- No replacement of Obsidian, dotfiles, or host configuration.
- No cloud memory, hosted accounts, or distributed replication.
- No LLM inside Hyphae.
- No graphical panel before the MCP experience is validated.

## The memory envelope

Every memory is one bounded record:

```json
{
  "project": "basecamp/omarchy",
  "scope": "project",
  "kind": "decision",
  "agent": "claude",
  "text": "Use omarchy-pkg-add instead of invoking pacman directly",
  "ttl": null
}
```

- `text` — 1 to 4,096 UTF-8 bytes. The memory's identity is derived from
  the project and the text content, so storing the same sentence twice in
  the same project is one memory.
- `project` — 1 to 256 bytes; the isolation unit (see below).
- `scope` — `project` (default) or `global`. Global memories live in the
  reserved project `_global` and surface in every recall alongside
  project memories, clearly labeled.
- `kind` — one of `decision`, `command`, `constraint`, `fact`, `note`.
  A recall may filter by kind.
- `agent` — free-form host label (`claude`, `codex`, `opencode`, …)
  recorded for provenance, never for authority.
- `ttl` — optional lifetime in seconds, 1 second to 10 years. Expiry is
  evaluated on the engine's clock; an expired memory never appears in a
  recall again, and its storage is reclaimed by maintenance.

## Project isolation semantics

A recall sees exactly: the memories of the named project, plus `_global`
memories. Nothing else. Two projects never observe each other's memories
through any tool surface. The project string is normalized (NFKC,
case-preserved) and compared exactly; `basecamp/omarchy` and
`Basecamp/Omarchy` are different projects by design — hosts should pass a
stable identifier such as the repository slug or the workspace path.

## Storage, expiry, deletion, backup, restore

- **Storage** — one Native directory at
  `~/.local/share/hyphae/agent-memory/`, owned by the user, containing a
  dedicated search collection. Memory text is indexed for lexical recall;
  the lifecycle record carries the envelope and TTL.
- **Expiry** — TTL is enforced at recall time (an expired memory cannot be
  returned) and reclaimed by the engine's maintenance. Expiry is
  deterministic on the engine clock, never a background guess.
- **Deletion** — `forget` is permanent: the lifecycle record and the
  document leave together and no recall can surface the memory again.
  Forgetting is idempotent.
- **Backup** — `hyphae agent backup` produces one verified backup archive
  under `~/.local/share/hyphae/backups/`; the command prints the archive
  path and its digest.
- **Restore** — `hyphae agent restore <archive>` verifies the archive
  before replacing the data directory and refuses while the service owns
  the directory.
- **Removal** — `hyphae agent remove` never deletes data. Only
  `hyphae agent purge-data` deletes, requires interactive confirmation,
  and refuses while the service owns the directory.

## Permission profiles

| Profile | Authority |
|---|---|
| `memory-reader` | Recall and status for the Agent Memory collection |
| `memory-writer` | Store, recall, forget, and status for the Agent Memory collection |
| `auditor` | Redacted security inspection without reading memory content |
| `operator` | Administration and backup; never granted automatically to agents |

An agent credential must not create roles or keys, administer backups,
read other collections, execute arbitrary SQL, or reach unrelated
security state. The MCP surface never advertises a tool the presented
credential cannot execute: the read profile lists recall and status only;
the write profile adds store and forget.

## MCP surface

```text
hyphae_memory_store     (write profile)
hyphae_memory_recall    (all profiles)
hyphae_memory_forget    (write profile)
hyphae_memory_status    (all profiles)
```

- `store(project, text, kind?, scope?, agent?, ttl?)` → the memory id and
  its expiry, if any.
- `recall(project, query, limit?, kind?, prove?)` → memories ordered by
  relevance; with `prove`, the response carries the sealed proof,
  witness, and anchor for offline verification with
  `hyphae proof verify`.
- `forget(project, id)` → permanent removal.
- `status()` → redacted counts and service health; never memory content,
  never credentials.

## Security and privacy boundaries

- Everything is local: loopback-only HTTP and an owner-only local
  endpoint; nothing listens beyond the machine.
- Credentials live in `~/.config/hyphae/credentials/*.key` at mode 0600
  and reach processes only through `HYPHAE_NATIVE_API_KEY_FILE` — a path,
  never a secret value. Secrets never appear in process arguments, logs,
  status output, or host configuration files.
- Logs never contain memory content or secrets by default.
- Recall proofs are the engine's standard sealed proofs: a third party
  can verify offline what was recalled without trusting the machine.

## What Agent Memory is not

It is not "Hyphae as another database". The product is the four-verb
memory experience — store, recall, forget, status — working identically
across agents from one `hyphae agent setup`, reversible without data
loss, and verifiable without trust. The engine underneath is an
implementation detail the user never has to administer.
