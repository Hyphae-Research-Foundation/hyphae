# Native local ecosystem architecture

Status: accepted target architecture; not shipped behavior

Hyphae's phase-1 target is one Rust process, one data directory, and three
Hyphae-owned first-class engines. This document describes the target and must
not be read as a capability claim for `0.2.1`.

## Product thesis

Applications commonly combine a relational database, a memory structure
server, and a search service. That split introduces change-data-capture
pipelines, dual writes, cache invalidation, asynchronous search refresh,
duplicated documents, three schemas, and separate backup, authorization, and
memory policies.

Hyphae removes those boundaries inside one process:

```text
embedded API                 native local protocol
       │                              │
       └──────────── local fabric ────┘
                         │
              typed requests / plan handles
                         │
        ┌────────────────┼────────────────┐
        ▼                ▼                ▼
 relational engine  structure engine  search engine
 SQL + relations    keys + native      documents + lexical
 joins + indexes    structures + TTL  + vector indexes
        └────────────────┼────────────────┘
                         ▼
 catalog / types / IDs / page+blob allocator
 WAL / CSN / MVCC / scheduler / memory budget
 checkpoint / recovery / backup / restore / proofs
```

These are three native engines, not three views over a generic KV store. Each
engine owns its data and physical layouts. They compose through explicit
cross-engine transactions, links, materialized views, and plan operators.

## Relational access without relationalizing storage

The complete ecosystem is relationally queryable. Hyphae SQL can bind a
catalogued keyspace, structure, document set, lexical result, or vector result
as a relation-valued source and can filter, join, group, aggregate, and project
it with ordinary relational operators.

That logical reach does not turn every object into a row-store copy. A sorted
set remains a native sorted-set structure; a posting list remains a native
search structure; an ANN graph remains a native vector structure. Their
engines expose typed iterators, cardinality and cost information, stable IDs,
and snapshot-aware access to the cross-engine planner.

## Shared substrate

### Types and catalog

The canonical type system must cover null, booleans, signed and unsigned
integers, decimal, floating point, text, binary, date/time, timestamp,
interval, UUID, JSON, arrays, maps, and vectors. The catalog assigns stable
IDs to databases, schemas, relations, columns, constraints, keyspaces,
structures, documents, analyzers, and indexes.

Catalog snapshots are immutable and addressed by `catalog_version`. Prepared
plans bind to stable IDs and are invalidated deliberately when a relevant
schema version changes.

### Page and blob store

The target owns its page and blob formats. The initial design uses
copy-on-write pages, checksums, immutable published roots, a partitioned buffer
pool, and explicit large-value storage. Safe file I/O and owned buffers are
the default; mmap or an unsafe acceleration requires a separate accepted ADR
and audit.

Redb can remain in the `0.2` compatibility path while the native store is
built, but it is not the target authority or materialization engine.

### WAL, commits, and MVCC

The WAL records compact typed operations in blocks rather than routing every
engine through a serialized public protocol. A commit coordinator:

1. reserves a commit sequence number (CSN);
2. validates catalog and write-set assumptions;
3. appends one cross-engine transaction to the WAL;
4. satisfies the selected durability policy;
5. installs the relational, structure, and search roots; and
6. advances the globally visible CSN.

Readers acquire `(visible_csn, root_set)` without a global engine mutex. A
transaction that touches all engines becomes visible atomically.

Snapshot isolation is the first transaction target. Serializable behavior
requires an explicit conflict model and separate exit evidence.

### Scheduler and memory

Partitions have fixed ownership and publish immutable roots. Cross-partition
work uses bounded queues and deterministic lock or ownership ordering.
Compaction, search merges, expiry, statistics, checkpointing, and backup run
under independent budgets and cannot monopolize the foreground executor.

One memory governor accounts for row pages, key structures, postings, vectors,
plan caches, and request arenas. Specialized indexes necessarily consume
metadata, but Hyphae avoids three copies of serialized source documents and
three unrelated cache policies.

## Native engines

### Relational engine

The relational engine owns:

- a familiar SQL language implemented by a Hyphae lexer, parser, binder,
  rewriter, cost model, optimizer, and executor;
- DDL, DML, constraints, transactions, savepoints, and isolation;
- row-oriented OLTP storage plus column batches for scans and aggregation;
- hash and B-tree access, statistics, nested/hash/merge joins, sorting, spill,
  common table expressions, window functions, and `EXPLAIN`; and
- prepared plan handles keyed by normalized input, parameter types, and
  catalog version.

SQL familiarity is a user interface choice. PostgreSQL is not embedded and its
wire protocol does not define Hyphae semantics.

### Structure engine

The structure engine owns a direct key directory and specialized physical
representations for strings, counters, hashes, lists, sets, sorted sets,
streams, bitmaps, probabilistic structures, geo values, and TTL.

Small values may be inline while large values use pages or blobs. Expiry uses
both lazy checks and a bounded timing wheel. Canonical structures cannot be
silently evicted; explicitly declared cache objects may be evictable or
reconstructible. Atomic batches and observation/conflict primitives use the
same CSN and MVCC boundary as relational work.

Valkey can be used as a pain-point and behavior reference. It is not a process,
library, persistence format, or command dispatcher inside Hyphae.

### Search engine

The search engine owns document storage, analyzer pipelines, term dictionaries,
compressed postings with positions, doc values, lexical scoring, WAND-style
top-k execution, phrase/prefix/fuzzy matching, highlighting, facets,
aggregations, vector storage, ANN, exact reranking, and hybrid fusion.

A mutable transactional delta is visible at commit. Background work folds
that delta into immutable segments without a global refresh boundary. Search
results identify the CSN and index generation used.

Small vector sets use exact execution. Large sets use a Hyphae-owned ANN index
with explicit recall, memory, rebuild, and tail-latency evidence. Approximate
results must remain labelled approximate.

OpenSearch and Weaviate can be comparative oracles. Neither defines Hyphae's
storage, APIs, refresh model, or query language.

## Cross-engine execution

A cross-engine planner can combine native operators without converting data
through JSON or a network protocol. Examples include:

- joining a relational filter with lexical or ANN candidates by stable ID;
- joining and aggregating directly over keyspace or structure data from
  Hyphae SQL;
- maintaining a cache object atomically with a relational mutation;
- deriving an explicit search index or cache view from a relation;
- applying relational constraints to a transaction that also updates a
  sorted set or stream; and
- using one snapshot for SQL, key, lexical, and vector reads.

Every link is catalogued and explicit. No engine requires an equivalent object
in another engine, while every catalogued object remains available to
relational queries through its native access operator.

## Local fabric

The primary path is the embedded Rust API. A local daemon adds a compact typed
binary protocol over Unix domain sockets on Unix and named pipes on Windows;
a shared-memory ring is an optimization candidate after its safety and crash
semantics are specified. TCP loopback and compatibility protocols are edge
surfaces.

Internal engine calls never use sockets, HTTP, JSON, RESP, PostgreSQL wire, or
OpenSearch REST. Prepared handles and binary values avoid repeated parsing and
binding. Point operations have dedicated fast paths instead of traversing the
general query optimizer.

## Durability classes

Hyphae exposes durability rather than hiding hardware physics:

- `strict`: acknowledge only after the relevant group is durably synchronized;
- `group`: batch multiple commits behind one synchronization while preserving
  acknowledgement-after-sync semantics; and
- `memory`: acknowledge publication without crash durability, for explicitly
  volatile data.

Latency receipts must name the class. Fsync time is device-dependent and is
never folded into an unqualified microsecond execution claim.

## Target data directory

```text
data/
├─ FORMAT
├─ LOCK
├─ manifest/
├─ wal/
├─ pages/
├─ blobs/
├─ segments/
│  ├─ search/
│  └─ vector/
├─ snapshots/
└─ tmp/
```

Migration from disk format 2 creates a separate target directory, imports a
logical snapshot, verifies counts and digests, and promotes only after full
validation. It never rewrites the source directory in place.

## Pain points Hyphae must beat

- OpenSearch ingestion commonly requires a separate pipeline or CDC and bulk
  requests can partially succeed. Hyphae requires one all-or-nothing
  cross-engine commit.
- OpenSearch search visibility is near-real-time unless refresh cost is paid.
  Hyphae's committed delta is visible at the same CSN.
- Valkey transactions do not roll back already executed commands after
  `EXEC`. Hyphae cross-engine transactions require real rollback before
  publication.
- PostgreSQL vacuum, Valkey long-command/fork pauses, and OpenSearch refresh,
  merge, heap, and GC behavior all create independent maintenance tails.
  Hyphae requires bounded incremental background work and measured p99.9
  under interference.
- Search products do not provide a full relational optimizer. Hyphae must plan
  joins, B-tree/hash/bitmap access, postings, and ANN candidates together.

The evidence program uses official upstream behavior as research input:
[PostgreSQL MVCC](https://www.postgresql.org/docs/current/mvcc-intro.html),
[PostgreSQL vacuum](https://www.postgresql.org/docs/current/routine-vacuuming.html),
[Valkey latency](https://valkey.io/topics/latency/),
[Valkey transactions](https://valkey.io/topics/transactions/),
[OpenSearch Data Prepper](https://docs.opensearch.org/latest/data-prepper/),
and [OpenSearch indexing and refresh](https://docs.opensearch.org/latest/api-reference/document-apis/index/).

## Implementation boundary

Core storage, SQL planning/execution, native structures, lexical indexing, and
ANN are implemented in this repository. General-purpose audited primitives
remain dependencies. Historical or upstream source reuse requires the porting
ledger, provenance, retained licensing, inherited tests, and human review.

Phase 1 follows the [native local gate](../gates/native-local-phase-1.md).
Clustering, SaaS, and LLM work cannot redefine or bypass this architecture.
