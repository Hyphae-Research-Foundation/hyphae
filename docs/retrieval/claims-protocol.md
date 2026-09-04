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
| The collection bound is 250,000 documents: 949 docs/s ingest, 36 s reopen after maintenance, bm25 p50 39 ms, filtered+facet 46 ms, and fuzzy 61 ms at the bound, and the durable scorer reproduces the reference model's 1,000 ranked hits bit-for-bit at that rung | [250k rung receipt](../gates/evidence/collection-cap-250k-2026-09-02.md) | Synthetic short-document corpus with one unique term per document, no named vectors, one c-16 host |
| At the 250,000-document bound the chunked manifest ingests 1,099 docs/s, reopens in 25 s, and answers bm25 in 24 ms p50 (phrase 29 ms, filtered+facet 39 ms, fuzzy 46 ms); the same lexical ladder measured at 1,000,000 documents gives 1,014 docs/s, 107 s reopen, and bm25 172 ms p50 | [Chunked manifest and 1M ladder](../gates/evidence/collection-manifest-chunked-1m-ladder-2026-09-03.md) | Same synthetic corpus and host as the 250k receipt; the bound stays 250,000 because the R5 vector conditions (ANN consolidation cost, RSS at 1M×768-dim) are unmeasured; superseded on scaling shape by the `a443c52` receipt below — the 1M query ladder is linear after the B+tree split fix and the 8,192-frame buffer pool, not superlinear |
| After the B+tree split fix and the 8,192-frame buffer pool, the 1,000,000-document query ladder is linear in the document count: bm25 23.2 ms, filtered+facet 42.6 ms, phrase 24.2 ms, and fuzzy(1) 54.2 ms p50, a 3.3–4.5× latency increase for the 4× increase in documents from the unchanged 250k rung (bm25 6.2 ms, filtered+facet 10.6 ms, phrase 7.3 ms, fuzzy 12.0 ms) — against 6.6–8.3× before the fix; supersedes the earlier superlinear observation | [`hyphae-3.0-metal-a443c52-2026-09-03.md`](../gates/evidence/hyphae-3.0-metal-a443c52-2026-09-03.md) §6 | Environment class 3 (dedicated hardware, `i7i.metal-24xl`), commit `0295656158d1b22cf499077e5a0a0df56f56ed3e` (the DCO-signed rewrite of the measured tree, formerly `a443c52`/`0eadc82…`); single host, one run per phase; the shipped cap stays 250,000 — the 1M rows measure above the bound and the R5 vector conditions remain unmeasured |
| Hybrid retrieval lifts quality +17.9% nDCG@10 over lexical with attested local embeddings, and the RRF fusion default is measured, not guessed | [Hybrid fusion on NFCorpus](../gates/evidence/rag-hybrid-fusion-nfcorpus-2026-08-22.md) | NFCorpus, bge-small-en-v1.5; hybrid ingest pays embedding plus windowed ANN consolidation |
| Typed filters cost at most 1.66× an unfiltered query; exact equality is cheaper than no filter | [Filtered eligibility](../gates/evidence/rag-filtered-eligibility-2026-08-22.md) | 20k synthetic docs at the current cap; composite predicates are the worst case |
| Committed state is byte-identical across hosts: same metrics and same directory bytes on different distros, kernels, glibc, CPUs | [Cross-host determinism](../gates/evidence/rag-cross-host-determinism-2026-08-22.md) | x86-64 Linux; aarch64 unmeasured; excludes float pipelines outside the engine |
| Local embeddings and rerank scores are replayable and offline-verifiable (`HYATTS01`, pure verifier in core) | [Attested embed replay](../gates/evidence/attested-embed-replay-2026-08-22.md) | CPU execution only; cross-implementation float portability not claimed |
| Chunk provenance is sealed in the proof: parent, byte range, and ordinal ride the verified result | Offline-verified chunk test in the engine suite (`integrated_search.rs`, C1/C3) | Provenance binds doc-values, not raw parent bytes |
| A local attested rerank lifts BM25 by +11.5% nDCG@10 with no vector index, sealed in the proof; the same bi-encoder stacked on hybrid subtracts 2.9% — published per this protocol | [Attested rerank uplift](../gates/evidence/rag-attested-rerank-nfcorpus-2026-08-23.md) | Bi-encoder rerank; a cross-encoder is the path to hybrid uplift |
| Head-to-head on identical hardware with identical vectors: hybrid quality wins on every metric, rankings never change on rerun (theirs did, 4/323), cold start is faster — and ingest is slower, published | [Weaviate head-to-head](../gates/evidence/rag-weaviate-head-to-head-2026-08-23.md) | NFCorpus at 3.6k docs; RSS favors Weaviate at this scale; 1M×768 is the next rung |
| Leave Weaviate with a receipt: a live instance exported through its public cursor, re-verified through the shipped binary, every construct carrying a fidelity class | [Weaviate head-to-head](../gates/evidence/rag-weaviate-head-to-head-2026-08-23.md), [migration guide](../porting/leave-weaviate.md) | Live cursor exports can miss concurrent writes; quiesce for point-in-time |

Rows land here only after their receipts exist.

## What we do not claim

No scale-out, no replication, no multi-tenant offload, no managed cloud.
Above the measured cap rung, the honest sentence is: buy a distributed
system — and ask it for a receipt.
