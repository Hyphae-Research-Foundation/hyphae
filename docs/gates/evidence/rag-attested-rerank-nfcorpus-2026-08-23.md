# V3 evidence — attested local rerank uplift on NFCorpus

- Date: 2026-08-23
- Harness: `tools/rag_eval.py` two-pass attested rerank mode (receipt
  `hyphae-rag-relevance-receipt-v1`)
- Dataset: BEIR NFCorpus, 3,633 documents, 323 test queries, archive
  SHA-256 `efe5be03f8c5b86a5870102d0599d227c8c6e2484328e68c6522560385671b0b`
- Rerank stage: 50 first-pass candidates scored by `bge-small-en-v1.5`
  through `hyphae-embed`; every stage carries its `AttestedLocal`
  `HYATTS01` envelope and the engine applies the reorder inside the
  search pipeline (298 of 323 queries produced candidates to rerank)
- Host: DigitalOcean dedicated droplet (Fedora 44, 8 vCPU), identical to
  the F2 runs

## Measurements

| Run | nDCG@10 | Recall@10 | MRR@10 | vs its baseline |
|---|---|---|---|---|
| lexical (baseline) | 0.306945 | 0.149150 | 0.515083 | — |
| **lexical + attested rerank** | **0.342121** | **0.158707** | **0.556117** | **+11.5% nDCG** |
| hybrid-RRF (baseline) | 0.361920 | 0.174458 | 0.577092 | — |
| hybrid-RRF + attested rerank | 0.351306 | 0.169675 | 0.542417 | −2.9% nDCG |

## What this decides

- **The counter to the cloud query agent**: a local, attested, sealed
  bi-encoder rerank lifts plain BM25 by +11.5% nDCG@10 — recovering most
  of the gap to full hybrid retrieval (0.3421 of 0.3619) with **no
  vector index at all**, and unlike any cloud reranker, the stage rides
  the proof: the attestation class and scores are sealed and the reorder
  re-executes offline.
- **The published loss**: stacking the same bi-encoder on top of hybrid
  retrieval *subtracts* 2.9% — the rerank replaces the fused
  lexical+vector order with a single-signal order the vector branch
  already contributed. Reranking over hybrid needs a different model
  class (a cross-encoder) to add information; with the same model, keep
  the fusion. Per the claim protocol, this number is published with the
  same prominence as the win.
- Cost: the rerank pays one attested model invocation per query
  (~17 s/query on this 8-vCPU host for 51 CPU-BERT encodes, dominated by
  per-invocation model load); batching amortization is an optimization,
  not a correctness gate.

Full receipts: `~/hyphae-eval/v3-lexrr.json`, `~/hyphae-eval/v3-hybrr.json`.
