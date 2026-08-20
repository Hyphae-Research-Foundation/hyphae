# Hyphae usage manual

Status: current for the published `1.2.2` release. Every command, request
shape, and output in this manual was executed against the released `1.2.2`
binary; outputs are literal, trimmed only where marked. The normative
semantics remain in the versioned specifications under
[`docs/native/`](native/local-product-v1.md) and the contracts under
`contracts/`; this manual is the practical, end-to-end guide.

Hyphae is a local-first data engine: one binary, one exclusively owned data
directory, and four surfaces — bounded SQL, native structures, integrated
lexical/vector search, and verifiable proofs — over one shared transaction,
WAL, MVCC, recovery, and proof substrate. It runs offline and embeds no
external database, cache, search engine, cloud service, embedding provider,
or LLM.

## Contents

1. [Install](#install)
2. [The mental model](#the-mental-model)
3. [First session](#first-session)
4. [Native SQL](#native-sql)
5. [Structures](#structures)
6. [The catalog](#the-catalog)
7. [Lexical and vector search](#lexical-and-vector-search)
8. [All-engine transactions](#all-engine-transactions)
9. [Security and RBAC](#security-and-rbac)
10. [Verifiable proofs](#verifiable-proofs)
11. [Day-to-day operation](#day-to-day-operation)
12. [Backup and restore](#backup-and-restore)
13. [The local daemon and HTTP v2](#the-local-daemon-and-http-v2)
14. [SDKs](#sdks)
15. [MCP for agents](#mcp-for-agents)
16. [Use cases](#use-cases)
17. [Common errors](#common-errors)
18. [Limits and non-capabilities](#limits-and-non-capabilities)
19. [Quick reference](#quick-reference)

## Install

From crates.io:

```bash
cargo install hyphae-cli --version 1.2.2 --locked
hyphae version --json
```

```text
{
  "api_version": "v1",
  "disk_format_version": 2,
  "engine_version": "1.2.2",
  "native_directory_format": 1,
  "product": "hyphae",
  "product_api_version": 1
}
```

The [GitHub release](https://github.com/celiumsai/hyphae/releases/tag/v1.2.2)
ships signed archives for Linux x64, macOS x64/arm64, and Windows x64, each
with SHA-256 checksums, SPDX/CycloneDX SBOMs, SLSA provenance, and Sigstore
bundles; verify before installing. To embed, depend on exact versions:
`hyphae-native-product = "=1.2.2"` for new applications, or
`hyphae-engine = "=1.2.2"` for existing format-2 state. Client SDKs are
`hyphae-sdk` (Python 3.11+, standard library only) and `@hyphae_/hyphae`
(Node 20+, ESM, no runtime dependencies).

## The mental model

Five rules explain nearly all Hyphae behavior:

1. **One process owns one directory.** Opening live state takes an exclusive
   operating-system lock; a second opener fails with
   `data_directory_locked`. Multi-process access goes through the daemon
   ([§13](#the-local-daemon-and-http-v2)).
2. **One commit, one CSN.** SQL, structures, and search share one catalog,
   WAL, MVCC sequence, and commit scheduler. A committed all-engine mutation
   has exactly one visible commit sequence number; readers never combine
   roots from different generations.
3. **Everything is bounded.** Every public operation carries count, byte,
   depth, work, and deadline limits. Budget exhaustion publishes no partial
   mutation. `hyphae capabilities` discloses the effective limits.
4. **Nothing is created implicitly.** Native commands require a directory
   created by `init`; backup, restore, and proof destinations must be new
   paths. Hyphae refuses to replace existing state.
5. **Evidence is first-class.** Every commit returns a receipt with CSN,
   LSN, WAL block digest, and transaction identity. Eligible reads can emit
   proofs a third party verifies offline.

Two state generations exist: the **Native directory** (format 1; the
`init`/`sql`/`structure`/`search` command families) and the **format-2
compatibility** generation (`put`/`get`/`query`/`snapshot`, the `/v1`
surface). They never convert silently; conversion is the explicit
[`migrate`](cli/reference.md) flow. This manual covers the Native
generation unless stated otherwise.

## First session

```bash
export HYPHAE_DATA_DIR="$PWD/data"
hyphae init --data-dir "$HYPHAE_DATA_DIR"
```

```text
{ "data_path": ".../data", "native_directory_format": 1, "status": "initialized" }
```

Three commands worth knowing from day one:

```bash
hyphae capabilities --data-dir "$HYPHAE_DATA_DIR"   # effective limits
hyphae status --data-dir "$HYPHAE_DATA_DIR"         # all-engine state: CSN, WAL, pages
hyphae doctor --data-dir "$HYPHAE_DATA_DIR"         # bounded offline diagnosis
```

Successful results are formatted JSON on stdout; diagnostics go to stderr;
the exit status distinguishes failure classes.

## Native SQL

Hyphae SQL is deliberately bounded: closed, release-gated operation
families rather than a universal-SQL promise. Supported shapes are tested;
unsupported shapes fail binding with `sql_invalid_syntax` instead of
producing undefined behavior. The complete grammar is in
[`sql-semantics-v1.md`](native/sql-semantics-v1.md).

```bash
hyphae sql --data-dir "$D" execute \
  --statement 'CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT NOT NULL, stars BIGINT)'

hyphae sql --data-dir "$D" execute \
  --statement 'INSERT INTO notes (id, body, stars) VALUES (?, ?, ?)' \
  --parameter 1 --parameter '"first offline note"' --parameter 5
```

Parameters are canonical JSON scalars in positional order (note the inner
double quotes for strings). Every mutation returns a commit receipt:

```text
{
  "commit": {
    "commit_csn": 3, "commit_lsn": 328055, "durability": "strict",
    "status": "committed",
    "transaction_id": "93646194250034055130955127479421607581",
    "wal_block_digest": "c160758d66..."
  },
  "result": { "rows_affected": 1, "type": "command" }
}
```

`prepared` prepares, executes, and deallocates one query in a retained
session; the response includes the rows plus the read **snapshot**
(catalog version, `visible_csn`, root digest) for cross-engine
correlation.

Verified supported shapes:

| Shape | Verified example |
|---|---|
| Primary-key lookup | `SELECT ... WHERE id = ?` |
| Bounded scan (mandatory `LIMIT`) | `SELECT id, body FROM notes ORDER BY id LIMIT 10` |
| Primary-key range | `SELECT ... WHERE id >= ? ORDER BY id LIMIT 5` |
| Exact-key DML | `UPDATE notes SET stars = ? WHERE id = ?`; also `DELETE`, `MERGE` |
| Secondary-index equality | `SELECT` filters and exact-key DML |
| One exact indexed `INNER JOIN` shape | see [`sql-semantics-v1.md`](native/sql-semantics-v1.md) |
| Windows over the primary key | `ROW_NUMBER()` / `RANK()` with one `PARTITION BY` column |

In scans, `ORDER BY` must be the **complete primary key in catalog order**
and `LIMIT` is mandatory. `SELECT ... WHERE stars >= ? ORDER BY id` fails
with `sql_invalid_syntax` because the range is on a non-key column. Free
aggregations, ORDER BY expressions, and disk spill fail closed.

`hyphae explain --data-dir "$D" sql --statement '...'` returns bounded plan
text without executing (`PrimaryKeyLookup(table=4)`); every mutation accepts
`--durability strict|group|memory`.

## Structures

Structures cover the string/counter/hash/list/set/sorted-set/stream space
with full transactionality. Two categories with different rules:

- **Scalars** (strings, counters) auto-create on first use.
- **Containers** (hash, list, set, sorted set, stream) require an explicit
  `create` mutation before any operation.

Scalar with TTL:

```bash
hyphae structure --data-dir "$D" set --key session:active --value note-1 \
  --expires-at-micros 4102444800000000
hyphae structure --data-dir "$D" get --key session:active
hyphae structure --data-dir "$D" ttl --key session:active
```

The default keyspace (`hyphae_internal.system.default_scalar`, object `3`)
is scalar-only. Container structures need catalogued keyspaces of the
matching family:

```bash
hyphae catalog --data-dir "$D" create-keyspace --id 20 --parent 2 \
  --name hyphae_internal.system.counters --family counter
hyphae catalog --data-dir "$D" create-keyspace --id 21 --parent 2 \
  --name hyphae_internal.system.hashes --family hash
```

A `batch` applies one typed JSON mutation array as a single transaction —
all or nothing:

```bash
hyphae structure --data-dir "$D" batch --mutations-json '[
  {"operation":"create","keyspace":21,"key":"note:1","family":"hash"},
  {"operation":"hash_set","keyspace":21,"key":"note:1","field":"author","value":"mario"},
  {"operation":"hash_set","keyspace":21,"key":"note:1","field":"state","value":"published"},
  {"operation":"counter_add","keyspace":20,"key":"visits","delta":10}
]'
```

Three verified rules that save a debugging session: containers without a
prior `create` fail with `object_not_found`; `create` on scalar families is
`invalid_request` (scalars auto-create); families are snake_case in JSON
(`"sorted_set"`) but hyphenated in CLI flags (`--family sorted-set`).

Typed reads use the same envelope; `Option` fields such as `start_after`
must be present (use `null`):

```bash
hyphae structure --data-dir "$D" read --request-json \
  '{"operation":"hash_scan","keyspace":21,"key":"note:1","start_after":null,"limit":10}'
```

Available reads: `string_get`, `counter_get`, `ttl`, `hash_get/scan/length`,
`hash_field_ttl`, `list_range/length`, `set_contains/members/cardinality`,
bounded `set_algebra`, `sorted_set_score/rank/range/cardinality`, and
`stream_range`. Full semantics:
[`structures-semantics-v1.md`](native/structures-semantics-v1.md).

## The catalog

Every logical object — databases, schemas, relations, keyspaces, search
collections, analyzers, indexes — lives in the catalog under a **stable ID**
that survives reopen. Pages are bounded and stable-ID ordered.

```bash
hyphae catalog --data-dir "$D" list
hyphae catalog --data-dir "$D" describe --id 13
hyphae catalog --data-dir "$D" resolve --name main.public.notes
hyphae catalog --data-dir "$D" dependencies --id 13
```

Names follow `database.schema.object`. Provisioning search creates internal
physical objects (`__product_lexical_13`, `__product_vector_13_*`); their
IDs matter for direct lexical queries below.

## Lexical and vector search

Create and provision a collection (`--dimension` fixes the vector
dimension; `provision` creates the physical lexical index and the `exact`
and `ann` vector indexes):

```bash
hyphae catalog --data-dir "$D" create-search-collection \
  --database 10 --schema 11 --collection 13 --analyzer 12 \
  --name main.public.note_search --dimension 2

hyphae search --data-dir "$D" provision --collection 13
```

Idempotent ingest:

```bash
hyphae search --data-dir "$D" ingest --collection 13 --idempotency-id 1 \
  --documents-json '[{"id":1001,
    "text":"offline search engine with proofs",
    "doc_values":{"category":"note","price":5},
    "vectors":{"exact":[1.0,0.0],"ann":[1.0,0.0]}}]'
```

**Doc-value fields are fixed by the collection's first ingest.** A later
document introducing new field names fails with `invalid_request` — decide
the doc-value schema before the first document. Document, vector, and
idempotency IDs are stable unsigned integers; repeating an
`--idempotency-id` with identical content is a safe no-op.

Direct lexical queries target the **physical index** (find it in the
catalog as `__product_lexical_<collection>`), with `--kind term`, `phrase`,
`prefix`, or `fuzzy`:

```bash
hyphae search --data-dir "$D" query --index 23 --query offline --kind term --limit 5
```

The integrated query binds a lexical branch and a vector branch to **one
catalog snapshot**, with typed doc-value filters, sort, facets, and metric
aggregations. A verified hybrid query:

```bash
hyphae search --data-dir "$D" integrated --collection 13 \
  --lexical search \
  --vector-target exact --vector 0.7 --vector 0.7 --vector-strategy exact \
  --filter-json '{"operation":"compare","field":"category","operator":"equal","value":"article"}' \
  --facets-json '[{"field":"category","limit":5}]' --limit 10
```

```text
{
  "approximate": false,
  "hits": [ { "object_id": "1007", "score": 0.0325...,
              "doc_values": { "category": "article", "price": 4 } } ],
  "facets": [ { "field": "category", "buckets": [ {"value":"article","count":1} ] } ],
  "vector_branches": [ { "strategy": "exact_filtered", "exact_reranked": true } ],
  "snapshot": { "visible_csn": 20, "root_digest": "ac0c116c..." }
}
```

| Piece | Shapes |
|---|---|
| Filters (`--filter-json`) | `match_all` / `exists` / `compare` (equal, not_equal, less, less_or_equal, greater, greater_or_equal) / combinators `all`, `any`, `not` with `"filters":[...]` |
| Vector strategy | `exact` (the oracle) / `ann` (incremental HNSW; discloses `approximate: true` plus candidate evidence) / `adaptive` |
| ANN tuning | `--ef-search`, `--candidate-limit` |
| Facets, metrics, sort | `--facets-json '[{"field":...,"limit":...}]'`, `--metrics-json`, `--sort-json` |
| Mutations | `update` (replaces one document across every branch), `delete`; both idempotent |

Your application provides the vectors — Hyphae embeds no models. Mutations
never rebuild the whole HNSW graph, and exact search stays available as the
oracle. Semantics: [`search-semantics-v1.md`](native/search-semantics-v1.md)
and [`ann-semantics-v1.md`](native/ann-semantics-v1.md).

## All-engine transactions

An explicit transaction mixes SQL, structure, search, and vector stages,
and commits them under one CSN:

```bash
hyphae transaction --data-dir "$D" execute --steps-json '[
  {"operation":"stage_sql","statement":"INSERT INTO notes (id, body, stars) VALUES (?, ?, ?)",
   "parameters":[3,"transactional note",5]},
  {"operation":"stage_structure","mutation":{"operation":"counter_add","keyspace":20,"key":"visits","delta":1}},
  {"operation":"commit"}
]'
```

Steps: `status`, `stage_sql`, `stage_structure`, `stage_search`,
`stage_vector`, then a terminal `commit` or `rollback`. Each stage returns
its provisional result inside the transaction.

If the process dies or the commit acknowledgement is lost, durable evidence
resolves the outcome — never guess or replay the commit:

```bash
hyphae transaction --data-dir "$D" status --id <transaction_id>
```

In the SDKs a cancelled or transport-failed commit becomes terminal
`outcome_unknown`; resolve it through the transaction-status operation.

## Security and RBAC

A fresh directory requires no credential. Durable access control starts
with a one-time bootstrap that creates the owner principal and its API key:

```bash
hyphae security --data-dir "$D" bootstrap \
  --name owner --label initial-key --key-out ./owner.key
# the file is created 0600, outside the data directory; the secret never reaches stdout
```

From that moment **every online Native command requires the key** through
`--native-api-key-file`, `HYPHAE_NATIVE_API_KEY_FILE`, or
`--native-api-key-stdin` — never argv. Key files must be regular restricted
files (owner-only on Unix; protected DACL on Windows); the CLI validates
opened-handle identity against substitution.

The complete verified flow — including the two steps everyone forgets:

```bash
# 1. Create the principal (every mutation needs a unique nonzero idempotency token)
hyphae security --data-dir "$D" --native-api-key-file owner.key \
  principal create --name analytics --idempotency-token 1001

# 2. ENABLE IT — principals are created disabled
hyphae security ... principal set-enabled --principal-id <ID> --enabled true \
  --idempotency-token 1002

# 3. Assign a built-in role
hyphae security ... assignment create-built-in --principal-id <ID> \
  --role reader --scope instance --idempotency-token 1003

# 4. Issue its key with the role's permission set
hyphae security ... key issue --principal-id <ID> --label analytics-read \
  --role reader \
  --permission catalog.read --permission credential.self_manage \
  --permission data.read --permission discover \
  --permission proof.generate --permission proof.verify \
  --permission search.execute \
  --scope instance --key-out ./reader.key --idempotency-token 1004
```

Verified with that key: `SELECT` returns rows; `INSERT` returns
`authorization_denied`; `security audit list` records every action with its
actor (`bootstrap_owner`, `activate_key`, `create_principal`, ...).

Two verified traps: a **disabled** principal makes its key return
`authorization_denied` on everything, including `capabilities` — check
`principal list` before suspecting the key; and a `key issue` that fails
midway consumes its idempotency token in the reservation, so a retry with
the same token returns `catalog_conflict` — use a fresh token.

Built-in roles (`admin`, `operator`, `developer`, `writer`, `reader`,
`auditor`) carry canonical dotted permissions shown by
`security role list`. Custom roles use `role create --grant permission@scope`
with scopes `instance`, `catalog_subtree:<id>`, or `catalog_object:<id>`;
ownership authority can never sit in a custom role. `key rotate` (overlap
window 0–604800 s), `key revoke`, and `key abort` complete the lifecycle;
losing the owner key has an offline two-phase `security owner
recover`/`resume` flow that requires the exclusive lock. Every metadata
surface is redacted; no command prints secrets or verifiers. Full model:
[`access-control-v1.md`](native/access-control-v1.md).

## Verifiable proofs

An eligible read (catalog, SQL, product reads) emits a canonical **proof**
plus a **witness**; any third party verifies them offline with only a
32-byte trusted anchor — no directory access, no network:

```bash
# Generate: the query executes and is bound to the artifacts
hyphae proof generate --data-dir "$D" \
  --operation-json '{"operation":"sql","statement":"SELECT id, body FROM notes WHERE id = ?","parameters":[1]}' \
  --proof-out query.hynproof --witness-out query.hynwitness
# → { "anchor": "f68a8eae03a3ea69...", "kind": "sql", "proof_bytes": 620, ... }

# Verify (another machine, no data access): full semantic re-execution
hyphae proof verify --proof query.hynproof --witness query.hynwitness \
  --anchor f68a8eae03a3ea69...
# → { "status": "verified", "scope": "semantic_reexecution",
#     "semantic_reexecution_performed": true }
```

The verifier validates both artifacts, requires the independently supplied
anchor, re-executes the operation under bounded reference semantics, and
compares the exact result. CLI-provable operations: `catalog_list`,
`catalog_describe`, `sql`; the SDKs expose `prove`, `prove_sql`, and
`verify_proof`. The format-2 twin is `get`/`query --proof-out` plus the
offline `hyphae verify` and `verify-retrieval` commands. Formats:
[`native-proof-v1.md`](native/native-proof-v1.md) and
[`native-witness-v1.md`](native/native-witness-v1.md).

## Day-to-day operation

| Command | What it does | When |
|---|---|---|
| `status` | all-engine state: visible CSN, pages, retained WAL, replayed transactions | monitoring, scripts |
| `telemetry` | bounded, redacted process-local snapshot; enables no exporter | spot diagnosis |
| `doctor` | offline diagnosis: format, pages, WAL, manifests, blobs, indexes, recovery authority | after incidents; around restore |
| `checkpoint` | publishes one synchronized all-engine recovery boundary | before backup; after bulk loads |
| `compact` | compacts one root family (`--target structures\|search`) | scheduled maintenance |
| `vacuum` | rebuilds live roots into a smaller page generation, atomically published | reclaiming space |
| `hardware discover/calibrate` | topology fingerprint; bounded CPU/memory/storage/WAL calibration | sizing; environment evidence |

`hyphae console` opens the interactive operator TUI with nine workspaces
(Overview, SQL, Structures, Search, Catalog, Backups, Operations, Proofs,
Security). Mutations require confirmation; the Security workspace is
read-only and redacted; it holds the same exclusive directory ownership as
any embedded command and starts no listener. Details:
[CLI reference](cli/reference.md).

## Backup and restore

The verified full cycle — every step validates before promising:

```bash
hyphae checkpoint --data-dir "$D"
hyphae backup create --data-dir "$D" --out ./backup      # → created (verified at creation)
hyphae backup verify --backup ./backup                   # → verified (without opening live state)
hyphae restore --backup ./backup --data-dir ./restored   # → restored (staging + doctor + atomic activation)
hyphae doctor --data-dir ./restored                      # → healthy, snapshot_verified: true
```

The Native backup is physical and synchronized, with `NATIVE_BACKUP.json`
and an exact inventory. Restore never merges or overwrites: it rebuilds in
a sibling staging directory, runs mandatory doctor validation, and
activates atomically. Destinations must be new. There is no online or
incremental backup — a declared non-capability; media policy belongs to
your application.

## The local daemon and HTTP v2

To serve one directory to multiple local processes:

```bash
hyphae serve --data-dir "$D" \
  --endpoint ./hyphae.sock \          # UDS on Unix; named-pipe identity on Windows
  --http-bind 127.0.0.1:8791 \        # optional loopback-first HTTP /v2 edge
  --native-api-key-auth               # require durable keys on both transports
```

The binary local protocol (`HYPHLCL1`) is the primary performance surface;
HTTP `/v2` is an adapter carrying product envelopes at `POST /v2/execute`
(canonical contract: `contracts/openapi/hyphae-v2.yaml`) — there is no plain
`GET /v2/capabilities`; use the SDKs. The process owns the directory until
shutdown. The separate format-2 `/v1` server is selected with `--bind` and
consumed with `hyphae remote` or the v1 SDKs.

## SDKs

Python, synchronous, validated over both transports:

```python
from pathlib import Path
from hyphae_sdk.v2 import HyphaeClient

api_key = Path("owner.key").read_text(encoding="ascii").strip()

# Local transport (UDS / named pipe), authenticated in the HELLO trailer
with HyphaeClient.local_authenticated("./hyphae.sock", api_key) as client:
    rows = client.sql("SELECT id, body FROM notes WHERE id = ?", [2])
    # → {'kind': 'rows', 'columns': ['id','body'], 'rows': [[2, 'second note with proofs']]}
    value = client.structure_get(b"session:active")   # structure keys are bytes!
    status = client.security_status()

# Same API over HTTP v2
with HyphaeClient.http("http://127.0.0.1:8791", bearer_token=api_key) as client:
    caps = client.capabilities()
```

Structure keys and values are `bytes` (`b"key"`); passing `str` raises
`ClientError`. With a credential, `http://` is accepted only on canonical
loopback hosts; anything else requires `https://`.

The async client owns exactly one serial worker; task cancellation,
deadline expiry, and `aclose()` interrupt the active operation within one
second (proven by the hosted Windows named-pipe gate), and reconnection
afterwards is clean:

```python
async with AsyncHyphaeClient.local_authenticated(endpoint, api_key) as client:
    async with await client.begin_transaction() as tx:
        await tx.stage_sql("INSERT INTO jobs VALUES (1, 'ready')")
        await tx.stage_structure({"kind": "string_set",
            "key": {"keyspace": 3, "key": b"job:1"}, "value": b"ready"})
        await tx.commit()
```

A transaction abandoned by its context rolls back; an uncertain commit is
terminal `outcome_unknown` and resolves through the transaction-status
operation. `@hyphae_/hyphae/v2` offers the same API for Node with
deadlines, `AbortSignal`, and exact 64-bit integers (unsafe values arrive
as `bigint`). In Rust, embed `hyphae-native-product` directly —
engine-to-engine calls are typed Rust, not HTTP or JSON. See the
[Python SDK guide](../sdks/python/README.md) and the
[TypeScript SDK guide](../sdks/typescript/README.md).

## MCP for agents

The MCP adapter (revision `2025-06-18`, JSON-RPC over stdio) exposes exactly
three read-only tools over managed Native HTTP v2: capabilities, redacted
security status, and bounded redacted principal pages — built to give
Claude Code, Codex, or another MCP host context without write authority:

```bash
# 1. Issue an auditor-role key (security.read without write authority)
hyphae security ... key issue --role auditor ... --key-out ./auditor.key
# 2. Run the adapter against the HTTP v2 edge
hyphae mcp --base-url http://127.0.0.1:8791 --native-api-key-file ./auditor.key
```

Unknown tool arguments — including prompt-supplied roles, permissions, or
keys — fail closed. Every result carries the version and BLAKE3 digest of
the exact tool contract. With a durable key, `http://` is loopback-only;
remote origins require `https://`. Claude Code and Codex plugin manifests
live in `plugins/hyphae/`. See the [MCP guide](../mcp/README.md).

## Use cases

### Local-first application with durable state and a hot cache

Replace SQLite + Redis in desktop apps, agents, and edge deployments: SQL
rows for content, a hash for hot metadata, a counter, and a TTL session key
— committed together when it matters:

```bash
hyphae transaction --data-dir "$D" execute --steps-json '[
 {"operation":"stage_sql","statement":"INSERT INTO notes (id, body, stars) VALUES (?, ?, ?)","parameters":[7,"new note",0]},
 {"operation":"stage_structure","mutation":{"operation":"counter_add","keyspace":20,"key":"visits","delta":1}},
 {"operation":"commit"}]'
```

End the work session with `checkpoint` + `backup create`. The application
starts offline every time; there is no service to depend on.

### Hybrid retrieval for local RAG

Your pipeline generates the embeddings; Hyphae indexes, filters, and
combines. Ingest the corpus with text, filterable doc-values, and vectors;
query hybrid with the strategy your budget allows:

```bash
hyphae search --data-dir "$D" integrated --collection 13 \
  --lexical "$TERM" \
  --vector-target ann --vector ... --vector-strategy adaptive \
  --ef-search 32 --candidate-limit 16 \
  --filter-json '{"operation":"compare","field":"category","operator":"equal","value":"doc"}' \
  --limit 8
```

The response discloses `approximate` and per-branch candidate evidence —
the agent knows whether the result was exact or ANN — and the snapshot
anchors the answer to a concrete `visible_csn`. Document mutations are
idempotent, so re-indexing is safe.

### Results a third party can audit

Publish a result together with its proof; the auditor verifies on their
machine without seeing your data:

```bash
# You: generate and publish result + proof + witness + anchor
hyphae proof generate --data-dir "$D" \
  --operation-json '{"operation":"sql","statement":"SELECT id, body FROM notes WHERE id = ?","parameters":[1]}' \
  --proof-out report.hynproof --witness-out report.hynwitness
# The auditor: no network, no directory — full semantic re-execution
hyphae proof verify --proof report.hynproof --witness report.hynwitness --anchor <anchor>
```

The anchor travels over an independent trust channel (contract, registry,
signed mail). A tampered proof, an incomplete witness, or a foreign anchor
fails closed.

### Read-only context for AI agents

`serve --http-bind ... --native-api-key-auth`, an `auditor` key, and
`hyphae mcp` give an agent three bounded, redacted tools. The key — not the
prompt — fixes the authority; escalation attempts through unknown arguments
fail closed, and the durable audit log records every read with its actor.

### Several local processes, one data owner

The exclusive lock prevents direct multi-process access — by design. The
pattern: one `serve` owns the directory; workers speak UDS through the SDK
(`HyphaeClient.local_authenticated`). Issue one key per service with the
minimal role (`writer` for ingestors, `reader` for queries) and rotate with
an overlap window for zero-downtime credential changes.

### Migrating format-2 data to Native

```bash
hyphae migrate inspect  --source ./old                          # verifies without mutating
hyphae migrate run      --source ./old --target ./new --manifest plan.json
hyphae migrate verify   --source ./old --target ./new --manifest plan.json
hyphae migrate promote  --source ./old --target ./new --manifest plan.json
# or: rollback removes only the unpromoted pending target
```

The importer never mutates the source, verifies logical SQL/structure/
search equivalence, and nothing activates without an explicit `promote`.

## Common errors

Every error is a typed `ProductError` with a stable `code`, `category`,
`retry` policy, and transaction state. The ones you will meet first — all
reproduced and resolved while validating this manual:

| Symptom | Actual cause | Fix |
|---|---|---|
| `sql_invalid_syntax` | SQL shape outside the bounded families (range on a non-key column; scan without `LIMIT`) | use the shapes in [§4](#native-sql); `ORDER BY` = complete PK |
| `object_not_found` in `batch` | container (hash/list/set/...) used without a prior `create` | add `{"operation":"create",...,"family":...}` to the batch |
| `invalid_request` in `ingest` | doc-values with fields absent from the collection's first ingest | use exactly the founding fields; a different schema needs a different collection |
| `invalid_request` on scalar `create` | `create` does not apply to string/counter (they auto-create) | drop the `create`; operate directly |
| `authorization_denied` on everything | the key's principal is **disabled** (principals are created disabled) | `principal set-enabled --enabled true` |
| `catalog_conflict` on `key issue` | retry with an idempotency token consumed by a failed attempt | use a fresh token |
| `data_directory_locked` | another process (daemon, console, embedder) owns the directory | stop it; offline flows (`bootstrap`, `owner`) require it free |
| `upgrade_required` on open | pre-1.2 directory without the scalar keyspace binding | `hyphae upgrade --data-dir ...` (explicit, lock-held) |
| `ClientError: binary value is invalid` (SDK) | structure key/value passed as `str` | pass `bytes`: `b"key"` |
| `outcome_unknown` after commit | lost commit acknowledgement (cancellation, transport) | resolve with `transaction status --id ...`; never replay the commit |

## Limits and non-capabilities

Effective limits of the 1.2.2 build as reported by `capabilities` (yours
are versioned in the contracts — consult them, do not assume):

| Limit | Value | Limit | Value |
|---|---:|---|---:|
| `sql_rows` | 1024 | `sql_statement_bytes` | 65536 |
| `sql_parameters` | 1024 | `catalog_items` | 4096 |
| `catalog_visits` | 16384 | `catalog_bytes` | 16777216 |

Deliberate non-capabilities — declared so you do not discover them in
production: universal SQL, distributed transactions, replication,
clustering, shared-kernel multitenancy, built-in TLS, at-rest encryption,
hosted control planes, embedding models or LLMs, online/incremental backup,
and universal performance superiority. Process supervision, filesystem
permissions, remote TLS termination, backup-media policy, and vector
generation belong to your application. See
[Native capabilities and limits](product/native-capabilities.md).

## Quick reference

Environment variables:

| Variable | Equivalent |
|---|---|
| `HYPHAE_DATA_DIR` | `--data-dir` |
| `HYPHAE_NATIVE_API_KEY_FILE` | `--native-api-key-file` (Native commands and MCP) |
| `HYPHAE_BASE_URL` | `--base-url` (`remote`, `mcp`) |
| `HYPHAE_BEARER_TOKEN_FILE` / `HYPHAE_BEARER_TOKEN` | format-2 `/v1` listener credential |

Command map:

| Area | Commands |
|---|---|
| Lifecycle | `init`, `upgrade`, `capabilities`, `version` |
| Engines | `sql`, `structure`, `search`, `catalog`, `transaction`, `explain` |
| Trust | `security`, `proof`, `verify`, `verify-retrieval` |
| Operation | `status`, `telemetry`, `doctor`, `checkpoint`, `compact`, `vacuum`, `console`, `hardware` |
| Cold data | `backup`, `backup-verify`, `restore`, `snapshot`, `migrate` |
| Service | `serve`, `remote`, `mcp` |
| Format-2 | `put`, `get`, `delete`, `query` |

The complete command syntax is always `hyphae <command> --help`; semantics
and side effects are in the [CLI reference](cli/reference.md). Normative
specifications: [`docs/native/`](native/local-product-v1.md). Gate status
and exact release receipts:
[native gate status](gates/native-gate-status.md).
