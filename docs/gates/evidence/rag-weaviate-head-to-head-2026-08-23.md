# V2 evidence — Weaviate OSS head-to-head on identical hardware

> Superseded in part: the lexical BM25 rows below measured Weaviate
> `1.38.0` and no longer describe current Weaviate — its 1.39.0 BM25
> rework substantially improved those scores. See the
> [1.39 lexical rerun](weaviate-139-lexical-rerun-2026-08-30.md) and
> [1.39 hybrid rerun](weaviate-139-hybrid-rerun-2026-08-31.md) for the
> current measurements. Do not quote the 1.38 lexical numbers.

- Date: 2026-08-23
- Harnesses: `tools/rag_eval.py` and `tools/weaviate_compare.py` — same
  dataset digests, corpus order, qrels, metrics, and rounding discipline;
  identical corpus and query vectors from the attested local embedder
  (`bge-small-en-v1.5`) on both systems
- Dataset: BEIR NFCorpus, 3,633 documents, 323 test queries
- Systems: `hyphae 1.2.2` (local daemon, strict durability) and Weaviate
  OSS `1.38.0` (single node, anonymous, vectorizer none, default HNSW)
- Host: one DigitalOcean dedicated droplet (Fedora 44, 8 vCPU) for every
  run

## Quality, same vectors on both sides

| Run | nDCG@10 | Recall@10 | MRR@10 |
|---|---|---|---|
| **Hyphae hybrid (RRF default)** | **0.361920** | **0.174458** | **0.577092** |
| Weaviate hybrid (α=0.5 default) | 0.356986 | 0.173503 | 0.559105 |
| Hyphae BM25 | 0.306945 | 0.149150 | 0.515083 |
| Weaviate BM25 | 0.143738 | 0.065610 | 0.236257 |

Hyphae's hybrid measured higher on each metric in this run (+1.4%
nDCG@10). The lexical rows reflect Weaviate 1.38.0 only and are
superseded by the 1.39 rerun noted above.

## Determinism, measured not claimed

Every query was immediately rerun and the rankings compared:

| System | Changed rankings on rerun |
|---|---|
| Hyphae (full mix, twice measured) | **0 / 323, both runs** |
| Weaviate hybrid | **4 / 323** |
| Weaviate BM25 | 0 / 323 |

Weaviate's hybrid path returned different rankings for four queries on
an idle single node with no concurrent writes. Hyphae's determinism
extends further than rankings: the same protocol produced
[byte-identical committed directories across hosts](rag-cross-host-determinism-2026-08-22.md).

## Operational cost, published with the losses

| Measure | Hyphae | Weaviate |
|---|---|---|
| Ingest (3,633 docs + vectors) | 931 s (strict durability, windowed consolidate/checkpoint/vacuum) | 3 s (asynchronous batch) |
| Query phase (embedding included) | 68 s | 66 s |
| RSS after the query mix | 264 MB | 80 MB |
| Cold start to ready | **4.1 s** | 6.2–6.8 s |
| Data at rest | 74 MB single directory | container volume |

The ingest gap is real and published: Hyphae pays for durable,
deterministic, maintenance-complete state on every window; Weaviate
acknowledges batches asynchronously. The RSS numbers hold at this scale
(3.6k × 384-dim); RSS at 1M × 768-dim remains the documented next rung.

## The exit ramp, demonstrated live

The Weaviate instance populated by this head-to-head was then imported
back out through `tools/weaviate_import.py` on the same host: all 3,633
objects exported through the public cursor, ingested under UUID-derived
identities with their vectors, and re-verified through the shipped
binary — `objects: exact`, `vectors: equivalent`, count matched, receipt
sealed (`hyphae-weaviate-import-receipt-v1`). The
[migration guide](../../porting/leave-weaviate.md) documents the path.

Full receipts: `~/hyphae-eval/v2-weaviate-bm25.json`,
`~/hyphae-eval/v2-weaviate-hybrid.json`, `~/hyphae-eval/v2-hyphae-probe.json`,
`~/hyphae-eval/g1-import-receipt.json`.
