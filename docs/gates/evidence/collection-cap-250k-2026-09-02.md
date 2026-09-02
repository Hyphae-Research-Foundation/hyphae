# Collection document cap — 250,000-document rung

- Date: 2026-09-02
- Harnesses: `collection_scale_evidence` (ingest, maintenance, reopen, query
  ladder) and `scale_stage_diagnostic` (durable-scorer stages and model
  equivalence), both in `crates/hyphae-native-product/examples/`
- Corpus: deterministic synthetic collection (rotating 16-word vocabulary,
  `category` string doc value, `price` integer doc value, no named vectors),
  256-document batches, `Group` durability
- Engine: shipped integrated search path, one process, no governor or
  execution pool installed (`workers=1`)
- Host: DigitalOcean `c-16` droplet (16 vCPU Xeon Platinum 8168 @ 2.7 GHz,
  32 GB, 200 GB local disk, Ubuntu 24.04), dedicated, release profile
- Source: the commits between `eec0784` and the commit that raises the cap;
  the 250k rung was measured with the collection bound lifted on the
  measurement host only

## What this receipt gates

Raising `MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` from 100,000 to 250,000.
The bound moves only with a receipt (R5 in the RAG roadmap); this is that
receipt. It records the four costs the bound protects — ingest, open,
query, and manifest size — and the scorer-equivalence oracle at the new
rung. It does not claim the 1,000,000 rung.

## Ingest

Per-batch ingest was superlinear before this work: `BEGIN`, commit, and the
receipt each materialized the complete all-engine state, root construction
re-read the base tree per scalar key, and staging took 6 ms of a ~1.9 s
batch at 10k documents. After point-resolved ingest (durable point reads,
delta staging for vector-less batches, coalesced scalar root construction,
buffer-pool probes in root construction):

| rung | docs/s | ingest wall | vacuum | directory after maintenance |
|------|--------|-------------|--------|-----------------------------|
| 10k  | ~1,500 (append) | — | — | — |
| 100k | 1,261  | 79.3 s      | 16.7 s | 385 MB |
| 250k | 949    | 263 s       | 48.1 s | 2.1 GB |

Baseline before the work: 309 docs/s at 10k, 48 docs/s at 100k.

## Open

Open validated every retained committed root with a complete-state decode;
a 100k directory holding ~400 unretired batch commits reopened in ~17 min.
Open now decodes complete state once (the root that becomes current) and
verifies every retained root structurally.

| rung | reopen, unmaintained | reopen after vacuum + checkpoint + retain |
|------|----------------------|-------------------------------------------|
| 100k | 2 m 02 s (was ~17 m) | 12.8 s |
| 250k | not measured         | 35.6 s |

## Query ladder (p50, 16 samples, `limit=10`, candidate limit 1,000)

| rung | bm25 | filtered (`price < 500`) + facet | phrase | fuzzy (distance 1) |
|------|------|----------------------------------|--------|--------------------|
| 100k | 19 ms | 20 ms | 22 ms | 80 ms  |
| 250k | 41 ms | 46 ms | 42 ms | 211 ms |
| ratio (2.5× documents) | 2.2× | 2.4× | 1.9× | 2.6× |

Measured with the shipped bound (`c783e2c`, reopened corpora). The
intermediate receipt before eligibility stopped copying keys — bm25 26 /
63 ms, filtered+facet 43 / 137 ms (3.2×), phrase 28 / 68 ms, fuzzy 78 /
227 ms — is kept in the `collection_scale_evidence` header for the trend.

Baseline at 100k before the work: bm25 73 ms, filtered 108 ms, phrase
97 ms, fuzzy 194 ms. The first bm25 sample after a fresh load is a
cold-cache outlier (p95 in the seconds); it is reported in the harness
output and not excluded.

Durable-scorer stage at 250k (`scale_stage_diagnostic`, 2 terms, 393
segments, 93,702 physical posting entries): ~13–17 ms warm, from ~112 ms
before the borrowed-leaf scorer. At 100k: 7.9 ms, from 47 ms.

## Scorer equivalence

`scale_stage_diagnostic` compares the retained-model scorer's ranked hits
against the durable scorer's hits — document ids, order, and score bits —
and fails the run on any divergence.

- 100k: 1,000 hits, `bit_identical=true`; model scorer 335 s, durable
  scorer 12.6 ms.
- 250k: 1,000 hits, `bit_identical=true`; see the appended run log in this
  file's history.

## Manifest

The collection manifest remains one `HYPSMAN1` value of 16-byte identities:
4 MB at 250,000 documents, rewritten once per ingest batch on either ingest
path. At 1,000,000 documents it would be 16 MB per batch and must be
re-measured or restructured before that rung.

## Boundaries

- Synthetic corpus with short documents; FiQA-scale text per document was
  measured separately at the 100k rung and is not re-measured here.
- Single host class; no aarch64, no hosted CI.
- No named vectors in the ladder corpus: a batch that carries vectors keeps
  the materialized ingest transaction and its cost is not covered by the
  ingest table.
- Fuzzy expansion (distance 1) is the least linear stage remaining (2.6×
  for 2.5× documents; 211 ms p50 at 250k) and is the next profiling
  target; the bound is raised on the aggregate receipt, not on a claim that
  every stage is linear.
