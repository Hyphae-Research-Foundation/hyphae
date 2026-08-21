# RAG relevance evidence — FiQA at the 100,000-document cap

- Date: 2026-08-21
- Harness: `tools/rag_eval.py` (sealed protocol, receipt
  `hyphae-rag-relevance-receipt-v1`)
- Dataset: BEIR FiQA-2018, 57,638 documents, 648 test queries, archive
  SHA-256 `32c7df99ed21252fdfb2cf3f5673502a8d245ee0c44c4a133570d92ce2b3ad02`
- Engine: integrated lexical branch (BM25), candidate limit 1,000, k=10
- Host: DigitalOcean `so-8vcpu-64gb-intel` droplet (Fedora 44), dedicated

## Receipt

```json
{
  "metrics": {
    "mrr@10": 0.293643,
    "ndcg@10": 0.235959,
    "recall@10": 0.298234
  },
  "cost": {
    "data_directory_bytes_after_ingest": 55562927163,
    "data_directory_bytes_after_maintenance": 564961635,
    "ingest_seconds": 13788.39,
    "maintenance_seconds": 13597.12,
    "query_seconds": 589.76
  }
}
```

Full receipt: `~/hyphae-eval/fiqa-100k-receipt.json` (host declaration,
digests, protocol constants).

## What this receipt gates

This is the R5 evidence for raising
`MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` from 10,000 to 100,000:

- A 57,638-document collection ingests, maintains, and answers the full
  648-query test set through the shipped binary with no admission failure
  and no fail-open divergence.
- `ndcg@10` 0.2360 matches the published BEIR BM25 reference for FiQA
  (≈0.236) — quality holds at the new scale, not just admission.
- Queries run at 0.91 seconds per query at 57.6k documents with the
  cached-snapshot-state engine (2.5 ms/query at 3.6k documents on
  NFCorpus); per-query cost grows with posting size, not with directory
  materialization.
- The run used the harness's periodic maintenance
  (`--maintenance-interval-batches 32`): transient page/WAL generations
  are ~2 GB per 256-document batch regardless of corpus size, so an
  unmaintained 57k-document ingest would transit ~450 GB. Windowed
  maintenance bounds the peak at ~52 GB and reclaims to 565 MB live
  (~9.8 KB per document).

## Operational findings folded back into the harness

- `checkpoint` on a ~50 GB transient directory exceeds the historical
  600-second operation timeout — maintenance operations now run with a
  7,200-second bound.
- Reopening a directory with a large live index exceeds the historical
  30-second daemon-bind deadline — now 600 seconds.
- The ingest write path (~2 GB of transient generations per 256-document
  batch, independent of corpus size) remains the dominant cost at scale
  (3.8 h ingest + 3.8 h cumulative maintenance for 57.6k documents) and
  is the R-track target that gates the 100k → 1M rung.

## Next rung

100,000 → 1,000,000 stays evidence-gated on: the ingest write path fix,
measured ANN consolidation cost, and RSS at 1M×768-dim vectors on
dedicated hardware (GPU where it cuts ingest, per the remote-eval
hardware policy).
