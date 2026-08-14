# Native 1.0 capabilities and limits

Status: implemented in the current source tree for the closed bounded G0-G6
profiles. G7 performance and G8 release authorization remain open, so this is
not a published `1.1.0` claim.

Hyphae Native is one process, one executable, and one exclusively owned data
directory. It works offline and embeds no external database, cache, search
engine, cloud service, embedding provider, or LLM.

## Capability matrix

| Capability | Embedded Rust | Local CLI | Local protocol | HTTP `/v2` | Python / TypeScript SDKs |
|---|---:|---:|---:|---:|---:|
| Catalog and stable object IDs | Yes | Yes | Yes | Yes | Yes |
| Bounded SQL DDL, DML, queries, prepared plans | Yes | Yes | Yes | Yes | Yes |
| Strings, counters, hashes, lists, sets, sorted sets, streams | Yes | Yes | Yes | Yes | Yes |
| TTL, scans, algebra, atomic structure batches | Yes | Yes | Yes | Yes | Yes |
| Lexical, exact-vector, ANN, and hybrid search | Yes | Yes | Yes | Yes | Yes |
| Filters, sorting, facets, and metric aggregations | Yes | Yes | Yes | Yes | Yes |
| Explicit SQL and all-engine transactions | Yes | Script | Yes | Yes | Yes |
| Transaction outcome after disconnect | Yes | Yes | Yes | Yes | Yes |
| Explain, status, telemetry, and capabilities | Yes | Yes | Yes | Yes | Yes |
| Native proof generation and offline verification | Yes | Yes | Yes | Yes | Yes |
| Checkpoint, vacuum, backup, restore, doctor | Yes | Yes | Administration | Yes | Administration |

“Yes” means the applicable bounded G6 operation family, not universal SQL or
an unbounded compatibility promise. SDKs expose typed high-level requests and
transaction state; callers do not need to construct protocol frames.

## Shared authority

- SQL, structures, lexical search, and vector search share one catalog, WAL,
  MVCC/root-set sequence, commit scheduler, page/blob allocator, backup, and
  proof substrate.
- A committed all-engine mutation has one visible CSN. Readers never combine
  roots from different generations.
- Engine-to-engine execution uses direct typed Rust calls, not HTTP, TCP,
  JSON, or a compatibility protocol.
- Native directory format `1` is separate from published format-2 state.
  Conversion is an offline, verified, explicitly promoted migration.

## Bounded behavior

Every public operation carries shape-appropriate count, byte, depth, work,
memory, deadline, and cancellation limits. Budget exhaustion publishes no
partial mutation. Streamed reads remain provisional until a mandatory success
completion; clients discard incomplete streams.

The effective limits are disclosed by `hyphae capabilities`, the local
protocol negotiation response, and HTTP `/v2/capabilities`. Exact defaults and
wire fields are versioned in the Native contracts rather than inferred from
host resources.

## Search behavior

The search engine owns its analyzer, postings, doc values, exact-vector
storage, and incremental HNSW lifecycle. Mutations do not rebuild the complete
graph. ANN results disclose approximation and candidate evidence; exact search
remains the oracle and planner fallback for sufficiently small eligible sets.
Integrated search binds lexical and vector branches to one catalog snapshot.

## Delivery boundaries

- The embedded facade and local UDS/named-pipe protocol are the primary
  performance surfaces.
- HTTP `/v2` is an optional loopback-first adapter. It is not an internal
  engine path.
- The CLI starts no listener unless `serve` is selected.
- The published `/v1`/format-2 implementation remains a separate compatibility
  product until Native replaces it; it is never silently opened as Native.

## Deliberate non-capabilities

Native 1.0 does not claim universal SQL, distributed transactions,
replication, clustering, shared-kernel multitenancy, built-in TLS, at-rest
encryption, hosted control planes, billing, an embedding model, an LLM,
online/incremental backup, or universal performance superiority. Applications
own process supervision, filesystem permissions, remote TLS termination,
backup-media policy, and optional vector generation.

See the [Native product contract](../native/local-product-v1.md),
[Native quickstart](../quickstart-native.md), and
[current gate status](../gates/native-gate-status.md).
