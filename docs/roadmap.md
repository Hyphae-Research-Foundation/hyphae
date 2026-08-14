# Hyphae roadmap

`0.2.1` is the retained compatibility release. Its annotated `v0.2.1` tag peels to
`08028e8dac077846c638f067ce74fbcf6fb75501`, its
[GitHub release](https://github.com/celiumsai/hyphae/releases/tag/v0.2.1) is
published, and all ten publishable Rust workspace crates are available on
crates.io at version `0.2.1`. The exact candidate, tag, workflow, artifact,
and registry identities are retained in the
[`0.2.1` publication receipt](release/receipts/0.2.1.md).

The `0.2.1` maintenance target is complete and retained in its
[release gate](gates/0.2.1.md). It raises bounded local snapshot-witness
verification limits; adds separate bounded query, recovery, snapshot,
compaction, and proof-producing paths used by the packaged CLI/server;
preserves the published Rust legacy surface; and carries dependency/host-smoke
maintenance without changing API `/v1`, disk format `2`, or either proof
format.

## Next program: native local data ecosystem

The Native product program ships as `1.0.0`. G0 through G7 have retained
source-bound closure for their versioned, bounded profiles. G8 supplies
exact-release safety evidence. G7 uses an environment-bound operational-scale
authority and makes no canonical bare-metal latency or interference claim. See
the [native gate status](gates/native-gate-status.md). Hyphae will build its own
relational SQL engine, native keyspace/data-structure engine, and native
lexical/vector search engine in one process. They share a Hyphae-owned catalog, memory
manager, page/blob store, WAL, MVCC/commit sequence, scheduler, backup, and
proof substrate. They are not wrappers around PostgreSQL, Valkey, OpenSearch,
Redb, or another database engine, and they are not projections of one another.

The governing documents are:

- [ADR-0020](adr/0020-native-local-data-ecosystem.md);
- [ADR-0021](adr/0021-native-cutover-and-format-evolution.md);
- [ADR-0022](adr/0022-cloud-ready-local-primitives.md);
- [native local ecosystem architecture](architecture/native-local-ecosystem.md);
- [microsecond-first performance contract](performance/microsecond-first.md);
- [ordered phase-1 gate](gates/native-local-phase-1.md); and
- [current native gate status](gates/native-gate-status.md).

The accepted [G6 execution roadmap](roadmaps/native-g6-roadmap.md) makes the
next gate a competitive local product rather than a thin wrapper. It fixes the
embedded/local-first strategy, native HTTP `/v2`, Rust/Python/TypeScript SDKs,
optional provider adapters, integrated filtered/hybrid search, incremental ANN
lifecycle, and native offline proofs. Closing G8 will establish readiness for
that bounded local contract, not universal superiority over a distributed
vector platform; matched comparative evidence and distributed capabilities are
later programs.

Phase 1 is single-process and local. Clustering, hosted control planes, SaaS,
and model integration do not begin until G8 closes on one exact commit.

The historical `0.2.0` implementation record remains in
[`roadmap-0.2.md`](roadmap-0.2.md); its retained evidence limitations do not
describe the independently recorded `0.2.1` release.

## 0.1.0 release roadmap

The phases are ordered gates. A later phase may be prototyped early, but it
cannot be declared complete while an earlier gate is red.

Current status: Phases 0 through 8 are complete for `0.1.0`. Any source change
invalidates release closure until the complete hosted matrix passes again on
the new exact commit. See
[`gates/phase-2.md`](gates/phase-2.md),
[`gates/phase-3.md`](gates/phase-3.md),
[`gates/phase-4.md`](gates/phase-4.md),
[`gates/phase-5.md`](gates/phase-5.md), and
[`gates/phase-6.md`](gates/phase-6.md), and
[`gates/phase-7.md`](gates/phase-7.md), and
[`gates/phase-8.md`](gates/phase-8.md).

| Phase | Outcome | Exit evidence |
|---|---|---|
| 0 | Product boundary, license, ADRs, source matrix | Accepted ADRs and an auditable porting ledger |
| 1 | Clean repository, workspace, CI, RustSec, secret scanning, docs | Green baseline on Linux, macOS, and Windows |
| 2 | Durable local core | Crash recovery, atomic/idempotent writes, snapshots, migrations, checksums, compaction |
| 3 | Correct query and retrieval | KV, filters, aggregates, stable global merge, budgets, abstention, quality tests |
| 4 | Verifiable provenance | Mandatory `/v1` proofs, explicit embedded/local proof paths, offline verification, and tamper tests |
| 5 | Secure `/v1` API | OpenAPI-first compatibility, authentication, limits, loopback default |
| 6 | Equivalent clients | Rust, TypeScript, Python, CLI, and MCP pass one conformance suite |
| 7 | Optional adapters | PliegoRS, Astro, Next, and Vite adapters use only public contracts |
| 8 | Release candidate | Multiplatform packages, SBOM, signatures, backup/restore, fuzz/load gates |

## Post-0.1 candidates

These items are explicitly excluded from the `0.1.0` gate. They require new
ADRs, versioned public contracts, deterministic reference semantics, proof
coverage, and independent quality evidence before implementation:

- provider-free lexical retrieval, with optional semantic retrieval fused by
  explainable reciprocal-rank fusion;
- neutral temporal validity, explicit abstention, and configurable diversity
  policies such as maximal marginal relevance;
- an optional typed relationship graph whose edges preserve verifiable
  provenance and never become a storage prerequisite;
- optional MCP/client lifecycle hooks, a loopback-only daemon, pre-persistence
  secret redaction, and an idempotent durable spool with acknowledgements.

This backlog incorporates concepts identified during an independent review of
[MenteDB](https://github.com/nambok/mentedb) on 2026-07-16. No MenteDB code was
copied or ported. Any later source reuse remains subject to the provenance,
license, transformation, inherited-test, and human-review requirements in the
[porting ledger](porting/ledger.md).

The first end-to-end durable proof is deliberately narrow: use one binary to
write data, interrupt it during a write, restart, query the committed state,
and verify the result offline without network, external database, embedding,
or LLM.
