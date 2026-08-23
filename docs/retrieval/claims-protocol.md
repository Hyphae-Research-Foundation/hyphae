<!-- SPDX-License-Identifier: Apache-2.0 -->
# Retrieval claim protocol

Every externally stated retrieval claim — quality, cost, determinism, or
capability — maps to a receipt an operator can regenerate. This page is
the map: the rule a claim must satisfy, the harness that produces its
receipt, and the honest boundary past which the claim does not extend.
It extends the G7 gate style (pinned inputs, shipped binaries, sealed
receipts, published losses) from engine performance to retrieval.

## The protocol

A claim is admissible only when all five hold:

1. **Pinned inputs.** Datasets are immutable public artifacts verified
   against frozen SHA-256 digests before use; corpus order, protocol
   constants, and query sets are part of the receipt.
2. **Shipped binary.** Runs go through the released `hyphae` binary and
   its public surfaces — never a bench harness with private hooks.
3. **Sealed receipt.** Every number lands in a schema-versioned JSON
   receipt carrying the engine version, the host declaration, and the
   full protocol constants, quoted by an evidence page in
   `docs/gates/evidence/`.
4. **Rerunnable.** The receipt names the exact harness invocation; a
   third party holding the dataset reproduces the numbers.
5. **Losses published.** A protocol that produces an unfavorable number
   publishes it with the same prominence (the G7 rule).

Model-derived numbers carry one more obligation: the attestation class.
`AttestedLocal` numbers are replayable (same weights, same input, same
output digest); `DeclaredProvider` numbers record what was sent and
received, never that the provider was deterministic. A claim must never
promote a declared number to a replayable one.

## Claim ledger

| Claim | Evidence | Boundary |
|---|---|---|
| BM25 relevance holds at the 100,000-document cap: nDCG@10 0.2360 on FiQA matches the published BEIR BM25 reference | [FiQA at the 100k cap](../gates/evidence/rag-relevance-fiqa-2026-08-21.md) | Lexical branch; per-query cost grows with posting size (0.91 s/query at 57.6k docs on that host) |
| Hybrid retrieval lifts quality +17.9% nDCG@10 over lexical with attested local embeddings, and the RRF fusion default is measured, not guessed | [Hybrid fusion on NFCorpus](../gates/evidence/rag-hybrid-fusion-nfcorpus-2026-08-22.md) | NFCorpus, bge-small-en-v1.5; hybrid ingest pays embedding plus windowed ANN consolidation |
| Typed filters cost at most 1.66× an unfiltered query; exact equality is cheaper than no filter | [Filtered eligibility](../gates/evidence/rag-filtered-eligibility-2026-08-22.md) | 20k synthetic docs at the current cap; composite predicates are the worst case |
| Committed state is byte-identical across hosts: same metrics and same directory bytes on different distros, kernels, glibc, CPUs | [Cross-host determinism](../gates/evidence/rag-cross-host-determinism-2026-08-22.md) | x86-64 Linux; aarch64 unmeasured; excludes float pipelines outside the engine |
| Local embeddings and rerank scores are replayable and offline-verifiable (`HYATTS01`, pure verifier in core) | [Attested embed replay](../gates/evidence/attested-embed-replay-2026-08-22.md) | CPU execution only; cross-implementation float portability not claimed |
| Chunk provenance is sealed in the proof: parent, byte range, and ordinal ride the verified result | Offline-verified chunk test in the engine suite (`integrated_search.rs`, C1/C3) | Provenance binds doc-values, not raw parent bytes |

Rows land here only after their receipts exist; the head-to-head (V2)
and attested-rerank-uplift (V3) rows join with their runs.

## What we do not claim

No scale-out, no replication, no multi-tenant offload, no managed cloud.
Above the measured cap rung, the honest sentence is: buy a distributed
system — and ask it for a receipt.
