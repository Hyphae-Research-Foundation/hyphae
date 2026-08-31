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

## Findings

1. **Weaviate 1.39.0 substantially improved BM25 scoring on this
   corpus.** The 2026-08-23 comparison measured 1.38.0 at nDCG@10 0.1437;
   1.39.0 measures 0.3073, consistent with their published BM25 rework.
   Any earlier lexical comparison based on the 1.38.0 measurement is
   obsolete and must not be repeated.
2. **Defaults measure equal** (0.3069 vs 0.3073 — inside rounding noise).
3. **Enabling Hyphae's analyzer pipeline measures +1.7 nDCG points over
   both defaults** (0.3241 vs 0.3073), with recall and MRR higher as
   well. The analyzer flags are a per-collection catalog option recorded
   in the receipt protocol; an equivalent tuned-Weaviate configuration
   was not exercised in this run and remains open work.
4. Weaviate 1.39.0 rerun stability on this host: 0/323 changed rankings
   (its 1.38.0 measured 4/323 on the earlier dedicated host; both
   observations stand as recorded, environments differ).
5. Cold start (container restart to ready): 9.26 s for Weaviate 1.39 on
   this droplet; Hyphae cold-start comparison was not re-measured here.

## Scope

Lexical-only rerun. The hybrid/vector rerun against 1.39 (same attested
vectors on both sides) and any latency/QPS comparison remain open items;
no comparative claim beyond the table above is authorized by this
receipt. Environment class 2 (virtualized) under
[claims](../../product/claims.md): relative same-host A/B only.
