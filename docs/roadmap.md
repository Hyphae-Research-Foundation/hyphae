# Hyphae roadmap

`0.2.0` is the latest published release. Its annotated `v0.2.0` tag peels to
`170380453a2ca6322a4c8bc50417318daee1c011`, its
[GitHub release](https://github.com/celiumsai/hyphae/releases/tag/v0.2.0) is
published, and all ten publishable Rust workspace crates are available on
crates.io at version `0.2.0`. The implementation record in
[`roadmap-0.2.md`](roadmap-0.2.md) remains historical and retains two unchecked
hosted-evidence items; it does not authorize a new release.

The active maintenance target is `0.2.1`, tracked by the
[`0.2.1` release gate](gates/0.2.1.md). It raises bounded local
snapshot-witness verification limits; adds separate bounded query, recovery,
snapshot, compaction, and proof-producing paths used by the packaged
CLI/server; preserves the published Rust legacy surface; and carries
dependency/host-smoke maintenance without changing API `/v1`, disk format `2`,
or either proof format. Final local evidence, the hosted
Linux/macOS/Windows matrix, exact candidate binding, tag, and publication
remain pending.

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
