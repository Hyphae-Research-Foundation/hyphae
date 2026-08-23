# F2 evidence — hybrid fusion methods measured on NFCorpus

- Date: 2026-08-22
- Harness: `tools/rag_eval.py` (sealed protocol, receipt
  `hyphae-rag-relevance-receipt-v1`)
- Dataset: BEIR NFCorpus, 3,633 documents, 323 test queries, archive
  SHA-256 `efe5be03f8c5b86a5870102d0599d227c8c6e2484328e68c6522560385671b0b`
- Engine: `hyphae 1.2.2`, k=10, candidate limit 1,000 per branch
- Vectors: `bge-small-en-v1.5` (384-dim) through `hyphae-embed` — every
  corpus batch and every query embedding carries an `AttestedLocal`
  `HYATTS01` envelope (15 corpus attestations recorded in each hybrid
  receipt)
- Host: DigitalOcean dedicated droplet (Fedora 44, 8 vCPU), identical for
  the three runs

## Measurements

| Run | Branches | Fusion | nDCG@10 | Recall@10 | MRR@10 |
|---|---|---|---|---|---|
| lexical | BM25 only | — | 0.306945 | 0.149150 | 0.515083 |
| hybrid-rrf | BM25 + exact vector | weighted reciprocal-rank (default) | **0.361920** | **0.174458** | **0.577092** |
| hybrid-weighted | BM25 + exact vector | `weighted_score` | 0.339423 | 0.170570 | 0.547206 |

Cost, same protocol: the lexical run ingests in 203 s with 95 s
maintenance and answers 323 queries in 3.0 s; each hybrid run pays 434 s
ingest (embedding included), 616 s maintenance (windowed ANN
consolidation, checkpoint, vacuum), and 67 s for the query phase
(per-query embedding included).

## What this decides

- **Hybrid retrieval pays for itself on quality**: +17.9% nDCG@10 and
  +17.0% recall@10 over the lexical baseline with the same engine and
  the same pinned protocol.
- **The deterministic weighted reciprocal-rank default is the measured
  choice, not a guess**: it beats the `weighted_score` blend on every
  metric (+6.6% nDCG@10). `weighted_score` stays available as the
  request-selectable alternative; the default stays RRF.
- The hybrid path exercises the full sustained-vector-ingest surface:
  bounded delta consolidation through `hyphae search consolidate` inside
  the maintenance windows, 64-document vector batches inside the frame
  bound, and attested embeddings end to end.

Full receipts: `~/hyphae-eval/f2-lex.json`, `~/hyphae-eval/f2-rrf.json`,
`~/hyphae-eval/f2-weighted.json`.
