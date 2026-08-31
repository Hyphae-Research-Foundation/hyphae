<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — Weaviate 1.39.0 lexical head-to-head rerun

- Date: 2026-08-30
- Host: one DigitalOcean c-16 droplet (16 vCPU Intel Xeon Platinum 8280,
  virtualized) running both systems sequentially on identical data
- Systems: Hyphae 2.2.0 + this change set (local UDS daemon, strict
  durability, complete index at ack) vs Weaviate OSS **1.39.0**
  (single node, anonymous, vectorizer none, default BM25)
- Dataset: BEIR NFCorpus (pinned digest), 3,633 documents, 323 queries
- Harnesses: `tools/rag_eval.py` (with the analyzer/BM25 knobs from this
  change set) and `tools/weaviate_compare.py`; identical corpus order,
  qrels, metrics, rounding
- Raw receipts:
  [`memory-2026-08-30/h2h139-lexical-*.receipt.json`](memory-2026-08-30/)

## Lexical BM25 quality

| System | nDCG@10 | Recall@10 | MRR@10 |
|---|---|---|---|
| Weaviate 1.39.0 BM25 (defaults) | 0.3073 | 0.1489 | 0.5158 |
| Hyphae BM25 (defaults) | 0.3069 | 0.1492 | 0.5151 |
| **Hyphae BM25 + stop/stem + k1=1.2,b=0.6** | **0.3241** | **0.1549** | **0.5308** |

## Honest findings

1. **Weaviate fixed its BM25.** The 2026-08-23 head-to-head measured
   1.38.0 at nDCG@10 0.1437 on this corpus; 1.39.0 measures 0.3073 —
   their BlockMax WAND rework repaired scoring. The old "2× BM25" talking
   point is dead and must not be repeated.
2. **Defaults now tie** (0.3069 vs 0.3073 — inside rounding noise).
3. **Hyphae's analyzer pipeline retakes the lead**: +1.7 nDCG points over
   Weaviate 1.39 defaults (0.3241 vs 0.3073), with recall and MRR up as
   well. The analyzer flags are a per-collection catalog option recorded
   in the receipt protocol.
4. Weaviate 1.39.0 rerun stability on this host: 0/323 changed rankings
   (their 1.38.0 measured 4/323 on the earlier dedicated host; both
   observations stand as recorded, environments differ).
5. Cold start (container restart to ready): 9.26 s for Weaviate 1.39 on
   this droplet; Hyphae cold-start comparison was not re-measured here.

## Scope

Lexical-only rerun. The hybrid/vector rerun against 1.39 (same attested
vectors on both sides) and any latency/QPS comparison remain open items;
no comparative claim beyond the table above is authorized by this
receipt. Environment class 2 (virtualized) under
[claims](../../product/claims.md): relative same-host A/B only.
