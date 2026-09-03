<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Canonical claims and non-claims

Status: normative wording authority for external communication (papers,
talks, README, release notes). When a sentence about Hyphae's capabilities
is written anywhere, it must be reducible to this page. Gate documents bind
evidence; this page binds vocabulary.

## The system claim

Hyphae is a single-node, offline-first data engine in one Rust process that
owns one data directory and executes relational, keyspace, and search
operations over one shared transactional substrate: one catalog, one WAL,
one MVCC commit sequence, one page/blob allocator, one scheduler, one
backup/proof surface. A committed cross-engine transaction is visible at
exactly one CSN in all engines or in none.

## Engine claims (use these nouns)

| Say | Do not say | Why |
|---|---|---|
| "indexed, fail-closed relational core" or "bounded relational engine" | "SQL engine", "SQL database", "SQL compatibility" | The admitted grammar is versioned and bounded ([sql-semantics-v1](../native/sql-semantics-v1.md)): typed DDL, multi-row named-column INSERT, exact-PK UPDATE/DELETE, indexed SELECT shapes, total and primary-key-prefix grouped aggregates (COUNT/SUM/MIN/MAX/AVG), descending primary-key scans, residual LIKE/IN, one admitted indexed INNER JOIN form, one nonrecursive materialized CTE form, two window forms. There are no subqueries, no UNION, no outer join, no HAVING, no expression arithmetic. Plans that cannot bind to an index fail closed (`HYSQL011`) instead of scanning. |
| "native keyspace/data-structure engine" | "Redis-compatible" | Structures share Hyphae's transaction and TTL semantics; there is no RESP surface in the native product. |
| "integrated lexical/vector search engine" | "full-text search compatibility", "Lucene-class" | BM25/BM25F, typed doc-values with bounded filters/facets/metric aggregations, exact vectors, deterministic HNSW, same-snapshot hybrid fusion. Phrase/prefix/fuzzy execute under explicit budgets; positions are not stored. |

## Isolation claim

Snapshot isolation with first-committer-wins over logical write identities,
one global commit sequence, and read-your-writes inside a transaction.
Serializable execution is **not implemented** and must never be implied;
predicate/range conflicts are not detected
([mvcc-commit-v1](../native/mvcc-commit-v1.md)). Write skew is therefore
admissible exactly as in any snapshot-isolation system.

## Topology claim

Single-node, single-writer publication. No replication, no clustering, no
distributed transactions, no consensus, no multi-tenant kernel. "Local-first"
refers to process-and-directory ownership, not to a sync protocol.

## Durability claims

- `Strict`: acknowledgement after this transaction's WAL fsync.
- `Group`: acknowledgement after a shared cohort fsync.
- `Memory`: acknowledgement without any fsync; an acknowledged Memory commit
  can be lost by a crash (never torn — recovery drops whole commits from the
  volatile WAL suffix only).
- No universal sub-millisecond promise covers fsync, cold I/O, or unbounded
  queries; transport, execution, queueing, and physical synchronization are
  measured and reported separately
  ([microsecond-first](../performance/microsecond-first.md)).

## Performance-evidence claims

Performance statements carry their environment class. Three classes exist:

1. **development observation** — warm, concurrency-1, developer hardware;
   never quotable externally;
2. **virtualized operational scale** — the closed G7 C-60 authority; quotable
   with the explicit "virtualized, no latency certification" qualifier;
3. **dedicated hardware** — bare-metal receipts under
   [`docs/gates/evidence/`](../gates/evidence/) produced by the
   [baseline harness](../../benchmarks/baseline-harness/README.md) with
   pinned baselines (SQLite, DuckDB, Redis, Tantivy), byte-identical
   deterministic workloads, like-for-like durability, and per-phase
   latency/throughput distributions. Only this class supports comparative
   statements, and each statement names the receipt.

## Formal-model claims

The cross-engine commit protocol has a machine-checked TLA+ model
([docs/formal/HyphaeCommit.tla](../formal/HyphaeCommit.tla)) covering
atomicity across crash/recovery, strict-durability acknowledgement,
first-committer-wins, and contiguous visibility. The model checks the
protocol as specified; it is evidence about the design, not a proof of the
Rust implementation. Implementation fidelity is carried by the physical
crash matrices (`tests/all_engine_transaction_g5.rs`,
`examples/process_crash_matrix.rs`).

## Prohibited claim shapes

- "Universal SQL", "drop-in replacement", "protocol compatible".
- "Serializable" in any form.
- "Distributed", "replicated", "highly available".
- Any latency number without its environment class and receipt.
- Any comparative statement without the baseline's version, configuration,
  durability posture, and the shared workload definition.

## Third-party conduct

Named third-party systems are measurement subjects and prior art, never
adversaries. Public wording about them:

- states measurements neutrally ("measures", "records", "differs by"),
  never combatively ("beats", "crushes", "wins against", "weakness",
  "dethrones", or equivalents in any language);
- makes no general superiority or inferiority claim in either direction —
  every comparison is scoped to its dataset, configuration, host, and
  receipt, and records the other system's stronger results with the same
  prominence as Hyphae's;
- credits improvements in other systems plainly and marks any superseded
  Hyphae comparison as obsolete rather than continuing to quote it;
- frames strategy documents around Hyphae's capabilities, not around
  another project's deficiencies.
