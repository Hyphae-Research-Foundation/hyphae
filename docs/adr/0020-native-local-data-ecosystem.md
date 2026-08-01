# ADR-0020: Hyphae owns a native local data ecosystem

- Status: Accepted
- Date: 2026-08-01
- Owners: Celiums Solutions LLC
- Supersedes: ADR-0001 product-scope decision

## Context

Hyphae `0.2.1` proves a narrow autonomous engine: one binary, one data
directory, durable KV, structured queries, retrieval, recovery, and proofs.
That release boundary is not the long-term product boundary.

The target product must replace the common local stack of a relational
database, a Valkey-class structure server, and an OpenSearch-class lexical and
vector search service. Building protocol translators around those products,
embedding their engines, or running them as sidecars would preserve their
separate consistency models, memory managers, maintenance paths, and latency
boundaries. It would not create Hyphae.

## Decision

Hyphae will own three first-class native engines in one Rust process:

1. a relational engine with Hyphae's own SQL parser, binder, optimizer,
   executor, row/column layouts, constraints, indexes, and transaction
   semantics;
2. a structure engine with Hyphae's own key directory, strings, counters,
   maps, lists, sets, sorted sets, streams, TTL, and atomic operations; and
3. a search engine with Hyphae's own document storage, analyzers, inverted
   index, doc values, lexical ranking, aggregations, vector indexes, and hybrid
   execution.

The engines are not projections or compatibility facades. Each can own data
that has no equivalent object in either of the other engines. Explicit links,
views, indexes, and cross-engine transactions compose them when requested.
Hyphae SQL provides relational access to catalogued structure and search data
through native operators, so users can join and aggregate across the complete
ecosystem without forcing those engines to store their data as SQL rows.

They share one Hyphae-owned substrate:

- canonical types, stable object identifiers, and a versioned catalog;
- page and blob allocation plus one coordinated memory budget;
- WAL, commit sequence, MVCC snapshots, recovery, and checkpointing;
- scheduling, admission, background-work budgets, backup, restore, and proofs.

A transaction may modify objects in all three engines. One commit sequence
becomes visible only after the complete write set is installed, so a reader
sees the state before or after that transaction and never a cross-engine mix.

The primary surfaces are an embedded Rust API and a compact native local
protocol. Internal calls use typed values and direct execution; they do not
cross TCP, HTTP, JSON, RESP, PostgreSQL wire, or OpenSearch REST boundaries.
Compatibility gateways may be evaluated after the native local ecosystem is
complete, but they do not define storage, semantics, or execution.

The hot path is microsecond-first. Performance claims must separate dispatch,
execution, transport, queueing, and physical durability. Prepared hot point
operations receive explicit microsecond targets. Fsync, cold I/O, large joins,
full scans, and broad lexical or ANN work remain hardware- and workload-bound
and cannot receive an unconditional sub-millisecond promise.

PostgreSQL, Valkey, OpenSearch, Redb, or another database/query/search engine
will not be the target engine beneath these capabilities. Hyphae may use
audited general-purpose primitives such as checksums, cryptography, Unicode,
compression, and async/runtime utilities. It will not reimplement standard
cryptography or TLS.

Phase 1 ends with the complete single-process local ecosystem. Clustering,
replication, multitenancy, hosted control planes, billing, and LLM integration
are later programs. The default product remains offline and model-free.

## Consequences

- Hyphae becomes substantially more ambitious than the `0.2` engine.
- The current log, recovery, proof discipline, and deterministic reference
  implementations remain evidence inputs, not an immutable target design.
- Disk format 2 and the Redb materialization path cannot serve as the final
  page, transaction, or indexing architecture.
- SQL, structures, and search require independent semantics and tests while
  sharing one failure-atomic commit boundary.
- Relational access is universal even though physical storage remains
  specialized and engine-owned.
- A native implementation requires a long ordered program; thin demos cannot
  be labelled complete engines.
- Compatibility with an upstream product is optional evidence or edge
  behavior, never the product identity.

## Alternatives considered

### Bundle PostgreSQL, Valkey, and OpenSearch

Rejected because it creates one installer around three authorities, memory
budgets, maintenance systems, and latency boundaries.

### Implement three compatibility facades over the existing KV engine

Rejected because a parser or protocol surface does not supply relational
storage, MVCC, specialized structures, inverted indexes, ANN, or their
operational semantics.

### Make structures and search projections of SQL

Rejected because it demotes two required engines and prevents them from owning
native data and physical layouts. Cross-engine views remain explicit.

### Use upstream database engines as Rust libraries

Rejected for the target architecture because ownership of core semantics,
layout, scheduling, and hot paths would remain outside Hyphae.

## Verification

The [native architecture](../architecture/native-local-ecosystem.md),
[microsecond discipline](../performance/microsecond-first.md), and
[phase-1 gate](../gates/native-local-phase-1.md) define the required evidence.
No phase may close without crash injection, cross-engine snapshot tests, a
reproducible latency receipt, and proof that the tested path contains no
external database or search runtime.
