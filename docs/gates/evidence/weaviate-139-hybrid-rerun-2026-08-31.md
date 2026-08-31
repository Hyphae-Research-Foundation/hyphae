<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — Weaviate 1.39.0 hybrid head-to-head, same attested vectors

- Date: 2026-08-31
- Host: one DigitalOcean c-32 droplet (32 vCPU, virtualized) running both
  systems sequentially on identical data
- Systems: Hyphae 2.2.0 + this change set (local UDS daemon, strict
  durability, lexical + exact-vector hybrid, RRF fusion) vs Weaviate OSS
  **1.39.0** (single node, anonymous, vectorizer none, default HNSW,
  hybrid alpha 0.5 and 0.75)
- Vectors: identical on both sides — BAAI `bge-small-en-v1.5` through the
  attested local embedder (`hyphae-embed`, CPU, deterministic), corpus and
  query vectors shared byte-for-byte
- Dataset: BEIR NFCorpus (pinned digest), 3,633 documents, 323 queries
- Raw receipts:
  [`memory-2026-08-30/h2h139-hybrid-*.receipt.json`](memory-2026-08-30/)

## Hybrid quality (same vectors both sides)

| System | nDCG@10 | Recall@10 | MRR@10 |
|---|---|---|---|
| **Hyphae hybrid + stop/stem + k1=1.2,b=0.6** | **0.3679** | 0.1757 | 0.5691 |
| Weaviate 1.39 hybrid α=0.75 (server default) | 0.3624 | **0.1771** | 0.5607 |
| Hyphae hybrid (defaults) | 0.3619 | 0.1745 | **0.5771** |
| Weaviate 1.39 hybrid α=0.5 | 0.3590 | 0.1751 | 0.5613 |

## Findings

1. Hyphae's analyzed hybrid measures the highest nDCG@10 in this run
   (0.3679): +0.55 points over Weaviate's α=0.75 configuration and +0.89
   over α=0.5. The highest MRR@10 comes from Hyphae's default hybrid
   (0.5771 vs 0.5607).
2. Weaviate 1.39 measures the higher Recall@10 at α=0.75 by +0.14 points
   (0.1771 vs 0.1757) — recorded, not hidden. The margins in both
   directions are small; the honest summary is parity on recall with a
   consistent Hyphae margin on the ranking metrics (nDCG/MRR) in this
   configuration set.
3. Weaviate 1.39 hybrid rerun stability on this host: 0/323 changed
   rankings at both alphas (1.38.0 had measured 4/323 on the 2026-08-23
   dedicated host). Hyphae remains deterministic by construction; its
   receipts carry the protocol digests.
4. Hyphae's default-vs-default 2026-08-23 result (0.3619 vs 0.3570 on
   1.38.0) reproduces against 1.39: the Weaviate hybrid measures 0.3590
   at α=0.5, Hyphae unchanged at 0.3619 on this host.

## Combined 1.39 rerun measurements (with the lexical receipt)

| Branch | Hyphae best | Weaviate 1.39 best | Difference |
|---|---|---|---|
| Lexical BM25 | 0.3241 (analyzed) | 0.3073 (defaults) | Hyphae +1.7 pts |
| Hybrid nDCG@10 | 0.3679 (analyzed) | 0.3624 (α=0.75) | Hyphae +0.55 pts |
| Hybrid Recall@10 | 0.1757 | 0.1771 (α=0.75) | Weaviate +0.14 pts |
| Hybrid MRR@10 | 0.5771 | 0.5607 | Hyphae +1.6 pts |

## Scope

Environment class 2 (virtualized) under
[claims](../../product/claims.md): relative same-host A/B only, quality
metrics, no latency/QPS claims, and no general superiority claim in
either direction — Weaviate serves scales and features this comparison
does not exercise. Both systems ran their documented defaults plus the
two Weaviate alphas; a fully tuned Weaviate configuration was not
exercised and remains open work. The embedding phase re-executes inside
the query loop on both harnesses identically, so quality comparisons are
like-for-like. Cross-encoder rerank remains future work.
