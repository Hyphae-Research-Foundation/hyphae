# RAG relevance baseline — NFCorpus, lexical branch (2026-08-20)

First execution of the pinned relevance protocol from the RAG competitive
roadmap (R1). One full BEIR NFCorpus evaluation (3,633 documents, 323
judged test queries) ran through the shipped binary over the local UDS
daemon: lexical branch only, candidate limit 1000, k = 10, deterministic
sorted-identifier ingest, dataset archive digest-pinned.

Reproduce with:

```sh
python3 tools/rag_eval.py --binary target/release/hyphae \
  --dataset nfcorpus --data-root <cache> --k 10 --download
```

## Receipt

```json
{
  "cost": {
    "data_directory_bytes_after_ingest": 29775558267,
    "ingest_seconds": 169.09,
    "query_seconds": 2792.28
  },
  "dataset": {
    "archive_sha256": "efe5be03f8c5b86a5870102d0599d227c8c6e2484328e68c6522560385671b0b",
    "documents": 3633,
    "name": "nfcorpus",
    "qrels": "qrels/test.tsv",
    "queries_evaluated": 323
  },
  "engine": {
    "api_version": "v1",
    "disk_format_version": 2,
    "engine_version": "1.2.2",
    "native_directory_format": 1,
    "product": "hyphae",
    "product_api_version": 1
  },
  "host": {
    "cpu_model": "Intel(R) Core(TM) Ultra 9 285H",
    "machine": "x86_64",
    "platform": "Linux-7.1.8-200.fc44.x86_64-x86_64-with-glibc2.43",
    "python": "3.14.7"
  },
  "metrics": {
    "mrr@10": 0.515083,
    "ndcg@10": 0.306945,
    "recall@10": 0.14915
  },
  "protocol": {
    "branches": "lexical",
    "candidate_limit": 1000,
    "ingest_batch_documents": 256,
    "ingest_order": "sorted-corpus-id",
    "k": 10,
    "transport": "local-uds-daemon"
  },
  "schema": "hyphae-rag-relevance-receipt-v1"
}
```

## Reading

- NDCG@10 0.3069 with the single canonical analyzer (no stemming, no
  stop words) sits a few points under published BM25 baselines for this
  dataset — the measured headroom the analyzer track (L3) and BM25F port
  (L2) exist to close.
- The cost block is the honest part of this receipt: 29.8 GB of data
  directory for 3,633 documents (the lexical write path rewrites the
  index per ingest batch and retains generations) and 8.6 seconds per
  query at this corpus size (the integrated search path scans documents
  per query). Both are the named R-track targets; the staged
  collection-cap raise (R5) is gated on erasing them, and every later
  run of this harness measures the fix against this baseline.


## Follow-up — maintained directory (2026-08-21, post R2)

The same evaluation rerun on the posting-index build with one
checkpoint + vacuum cycle between ingest and the query phase. Relevance
is byte-identical; the cost story changes materially:

```json
{
  "data_directory_bytes_after_ingest": 29775591035,
  "data_directory_bytes_after_maintenance": 49100675,
  "ingest_seconds": 196.36,
  "maintenance_seconds": 95.61,
  "query_seconds": 2024.87
}
```

- The 29.8 GB directory was transient page and WAL generations, not live
  index: one maintenance cycle reclaims it to 49 MB (~13 KB per
  document). The R-track disk finding resolves into an operations
  policy — maintain the directory — plus the harness now measuring both
  sides of it.
- Query time improves from 8.6 to 6.3 seconds per query once queries
  stop materializing a bloated store, but the remaining cost is the
  materialized lexical execution itself: the query path re-analyzes
  stored documents instead of reading the durable BM25 postings the
  ingest already writes. Moving the integrated lexical branch onto the
  pinned-root posting read path is the next R-track increment, measured
  against this receipt.
