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

Host lifecycle hooks make recall proactive: project memory is injected before
the host processes a prompt. Capture is deterministic and conservative. Only
explicit decisions, constraints, facts, and a narrow allowlist of successful
reusable commands are persisted; full prompts, full responses, reasoning, tool
output, and detected secrets are never stored automatically.

The proactive bridge also rejects detected personally identifiable
information (PII), including email addresses, phone numbers, postal-address
labels, government identifiers, payment cards, IP/MAC addresses, UUIDs, home
directory paths, and likely high-entropy credentials. Project identifiers are
stored as local keyed-purpose BLAKE3-derived names instead of repository URLs
or filesystem paths. Detection is deliberately fail-closed: a candidate that
matches any sensitive-data rule is discarded rather than partially redacted.

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
  "layer": "work",
  "agent": "claude",
  "harness": "claude-code-cli",
  "model": "anthropic/claude-sonnet",
  "text": "Use omarchy-pkg-add instead of invoking pacman directly",
  "ttl": null
}
```

- `text` — 1 to 4,096 UTF-8 bytes. The memory's identity is derived from
  the project, layer, and text content, so storing the same sentence twice
  in the same project and layer is one memory.
- `project` — 1 to 256 bytes; the isolation unit (see below).
- `scope` — `project` (default) or `global`. Global memories live in the
  reserved project `_global` and surface in every recall alongside
  project memories, clearly labeled.
- `kind` — one of `decision`, `command`, `constraint`, `fact`, `note`.
  A recall may filter by kind.
- `agent` — free-form host label (`claude`, `codex`, `opencode`, …)
  recorded for provenance, never for authority.
- `harness` — exact host integration that produced the memory, such as
  `claude-code-cli`, `codex-cli`, `opencode-cli`, or `pi-cli`.
- `model` — provider/model identity reported by the harness. Unknown model
  identity is recorded explicitly as `unknown`, never inferred.
- `layer` — `personal`, `work`, or `journal`. Work holds user/project
  decisions; journal holds a model's first-person reflection. Journal content
  is separate and is always rendered as historical model thought, never as
  user instruction or authority.
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

- **Storage** — one exclusive Native directory at
  `~/.local/share/hyphae/agent-memory/`, owned by the user, containing
  physically separate personal, work, and journal search collections. Memory
  text is indexed for lexical recall; the lifecycle record carries the
  envelope and TTL. Existing mixed collection `13` data is moved explicitly
  with the offline, retry-safe `hyphae agent migrate-domains` command.
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
the write profile adds store, journal, and forget.

## MCP surface

```text
hyphae_memory_store     (write profile)
hyphae_memory_journal   (write profile)
hyphae_memory_recall    (all profiles)
hyphae_memory_forget    (write profile)
hyphae_memory_status    (all profiles)
```

- `store(project, text, kind?, scope?, agent?, ttl?)` → the memory id and
  its expiry, if any.
- `journal(project, text, harness, model, ttl?)` → a first-person model
  journal entry in the separate `journal` layer. Text must begin in first
  person (`I ...`, `Yo ...`, `Pienso ...`, and bounded equivalents).
- `recall(project, query, limit?, kind?, prove?)` → memories ordered by
  relevance; with `prove`, the response carries the sealed proof,
  witness, and anchor for offline verification with
  `hyphae proof verify`.
- `forget(project, id)` → permanent removal.
- `status()` → redacted counts and service health; never memory content,
  never credentials.

MCP remains the explicit/manual tool surface. Host integrations also invoke
`hyphae agent hook --host <HOST>` from lifecycle hooks or plugins. Hook payloads
arrive as bounded JSON on standard input, use the owner-only native local
socket, and return bounded historical context. A hook failure never blocks a
host from continuing without memory.

When the daemon is temporarily unavailable, already-sanitized capture
candidates enter an owner-only durable spool under
`$XDG_STATE_HOME/hyphae/agent-hooks/`. Spool records are content-identified,
written with create-new and fsync semantics, deduplicated across retries, and
drained opportunistically through the local socket. Successful commits create
bounded acknowledgement records; only then is the pending record removed.

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

## External retrieval evaluation

`tools/long_term_memory_benchmarks.py` runs the pinned LoCoMo and
LongMemEval-S-cleaned retrieval protocols against a disposable Native
directory over the local protocol. The datasets remain caller-supplied
external inputs and must match their frozen upstream SHA-256 digests; Hyphae
does not redistribute them. LoCoMo is `CC-BY-NC-4.0`, while LongMemEval's
cleaned release is `MIT`.

The harness evaluates retrieval only and executes no reader model. LoCoMo uses
conversation-scoped, sample-qualified dialog-turn evidence recall; optional
LoCoMo views may add the pinned session timestamp and previous turn while each
hit still maps to exactly one evidence anchor. LongMemEval reproduces its user-only,
session-granularity lexical baseline and official non-abstention denominator.
Receipts default to ignored `artifacts-local-*` paths, remain
`local-diagnostic`, and never authorize publication.

Supplying `--progress <JSONL>` enables the versioned per-query trace used by
offline statistical selection. The trace begins with deterministic schema,
dataset, engine/source, protocol, scoring, timing, and canonical-digest
metadata. Each subsequent record contains the source ordinal, query and
sample/conversation identifiers, segment, expected and scored targets, the
full returned logical ranking up to `--candidate-limit`, unrounded metric
contributions, timed-repeat latencies, the per-query repeat-change count, and
a stable ranking SHA-256. Query text, answer text, and document text are never
written. Existing traces are locked and validated before append; incompatible
metadata, malformed records, and duplicate source ordinals fail closed, so a
resume uses a non-overlapping `--start-after` range. Chunk aggregation applies
the same validation and rejects overlap across traces; aggregation requires
`--aggregate-dataset` so query identities, targets, metrics, and excluded
source ordinals can be checked against the pinned input.

Scoring defaults to `--qrel-mode audited-v2`, which performs stable
first-occurrence deduplication of expected targets before metric calculation
and records both raw expected targets and the deduplicated scored targets for
audit. LongMemEval NDCG also derives the audited ideal ranking from those
unique targets rather than duplicate corpus occurrences. `--qrel-mode
raw-compat` retains repeated target and corpus occurrences for comparison with
earlier runs; the selected mode and semantics are bound into the receipt and
trace protocol digests.

Memory stores commit atomically when the transport serves explicit all-engine
transactions: the search document (text, doc-values, and any vector branches),
the lifecycle envelope, and its TTL stage in one transaction and commit under a
single CSN, so a crash or abort can never leave a searchable document without
its lifecycle record or vice versa. The store resolves the collection's physical
lexical and vector indexes and the default scalar keyspace through the public
catalog only. Each step is budgeted; a transport that does not answer explicit
transactions within the budget falls back to the historical ingest-then-set
sequence, and recall still gates every hit on the live lifecycle record.
Memory collections additionally carry temporal doc-values (`session`, `actor`,
`date_anchor`, `session_ts`, `turn_ord`) so retrieval can filter and deduplicate
by session through the engine's doc-value plane instead of baking temporal
hints into text.

## What Agent Memory is not

It is not "Hyphae as another database". The product is the five-verb
memory experience — store, journal, recall, forget, status — working identically
across agents from one `hyphae agent setup`, reversible without data
loss, and verifiable without trust. The engine underneath is an
implementation detail the user never has to administer.
