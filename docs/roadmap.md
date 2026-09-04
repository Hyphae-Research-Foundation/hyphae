# Hyphae roadmap

## 3.0.0 shipped (2026-09-04)

Hyphae `3.0.0` published from source commit
`24bce1accdff8d14127797afe6f237a57c1cd4f3` (tag `release-v3.0.0-crates`); see
the [3.0.0 changelog entry](../CHANGELOG.md) for the full accounting and the
[publication receipt](release/receipts/3.0.0.md) for the release identity,
hosted evidence, and crates.io checksums. It shipped SQL slice 2 (`HAVING`,
grouped `ORDER BY`, `SELECT DISTINCT`, `OFFSET`, `BETWEEN`, `AS` aliases),
Valkey-shaped keyspace conditional and range commands (`SETNX`, `APPEND`,
`SETRANGE`, `HSETNX`, `ZINCRBY`, `ZPOP`, seeded `SPOP`/`SRANDMEMBER`), search
minor 6/semantics v5 (relative-score fusion, autocut, range facets, offset
pagination, lexical `AND`/`OR` with minimum-match, prefix expansion, BM25F
field boosts, fuzzy expansion, phrase matching, and highlighting over
expanded terms), and HNSW diversity neighbour selection with the SQ8
scalar-quantization primitive. A B+tree batch-rewrite fix removed a
structural degeneration in the durable scorer and raised
`MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` from 100,000 to 250,000 on the rung
receipt; the 1,000,000-document rung is measured but not shipped until the
ANN consolidation and RSS conditions of roadmap item R5 are met.

`0.2.1` is the retained compatibility release. Its annotated `v0.2.1` tag peels to
`08028e8dac077846c638f067ce74fbcf6fb75501`, its
[GitHub release](https://github.com/Hyphae-Research-Foundation/hyphae/releases/tag/v0.2.1) is
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

## Native local data ecosystem (shipped)

The Native product program shipped through `1.0.0` and the published `1.1.0`
release. G0 through G8 have retained source-bound closure for their
versioned, bounded profiles; G8 closed on the exact `1.1.0` release commit.
G7 uses an environment-bound operational-scale authority and makes no
canonical bare-metal latency or interference claim. See the
[native gate status](gates/native-gate-status.md). Hyphae owns its
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

The accepted [G6 execution roadmap](roadmaps/native-g6-roadmap.md) made that
gate a competitive local product rather than a thin wrapper. It fixed the
embedded/local-first strategy, native HTTP `/v2`, Rust/Python/TypeScript SDKs,
optional provider adapters, integrated filtered/hybrid search, incremental ANN
lifecycle, and native offline proofs. The closed G8 established readiness for
that bounded local contract, not universal superiority over a distributed
vector platform; matched comparative evidence and distributed capabilities are
later programs.

Phase 1 is single-process and local. Clustering, hosted control planes, SaaS,
and model integration remain separate later programs with their own accepted
contracts and gates.

## Post-1.2 program

Historical: this program predates the `3.0.0` release; see
[3.0.0 shipped](#300-shipped-2026-09-04) above and `../CHANGELOG.md` for what
actually landed.

With the `1.2.2` registry publications complete, the active forward plan is
the [Native acceleration and verification-asymmetry
roadmap](roadmaps/native-acceleration-roadmap.md): embedded-path contention
first, then single-stage filtering completion, SIMD, optional heterogeneous
acceleration behind the G9 gate, attested embeddings with Proof of
Retrieval, and distribution.

## 1.2.0 programs

Historical: this program predates the `3.0.0` release; see
[3.0.0 shipped](#300-shipped-2026-09-04) above and `../CHANGELOG.md` for what
actually landed.

Hyphae `1.2.0` has two coupled execution programs. The
[operator and agent experience plan](roadmaps/1.2.0-operator-and-agent-experience.md)
adds durable access control, the operator console, shared agent plugins, and a
published Python client. The accepted-target
[relicensing plan](roadmaps/1.2.0-relicensing.md) moved the current integration
tree and new software artifacts back to `Apache-2.0` after a fail-closed legal,
dependency, ownership, and governance preflight.
[ADR-0029](adr/0029-apache-2.0-software-and-normative-specifications.md) is the
effective classification authority.

The programs must close on the same exact release candidate. Published
`v1.1.0` artifacts remain `AGPL-3.0-only`; the roadmap does not retroactively
change their terms. No `v1.2.0` tag or registry publication is allowed while
either program has an open gate.

Historical tags `v0.1.0` through `v1.0.1` retain their Apache-2.0 root license
and declarations. Because they contain no separate documentation license, the
documentation classification for those tags is `not-separately-specified`,
not an inferred Creative Commons license.

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
