# Chunked collection manifest — 250,000 re-measurement and the 1,000,000-document ladder

- Date: 2026-09-03
- Harnesses: `collection_scale_evidence` (ingest, windowed and final
  maintenance, query ladder) and `scale_stage_diagnostic` (manifest stage,
  durable-scorer stages, model equivalence), both in
  `crates/hyphae-native-product/examples/`
- Corpus: deterministic synthetic collection (rotating 16-word vocabulary,
  `category` string doc value, `price` integer doc value, no named vectors),
  256-document batches, `Group` durability
- Engine: shipped integrated search path, one process, no governor or
  execution pool installed (`workers=1`)
- Host: DigitalOcean `c-16` droplet (16 vCPU Xeon Platinum 8168 @ 2.7 GHz,
  32 GB, 200 GB local disk, Ubuntu 24.04), dedicated, release profile
- Source: `92f3c7a` (chunked manifest) and `51d8fb0` (lazy eligibility) on
  `feat/sql-slice-2-and-evidence`; the 1M rung was measured with the
  collection bound lifted to 1,000,000 on the measurement host only

## What this receipt records

The [250k rung receipt](collection-cap-250k-2026-09-02.md) named the
collection manifest as the declared blocker for the next rung: one
`HYPSMAN1` value of 16-byte identities, 4 MB at 250,000 documents, rewritten
on every ingest batch and cloned on every `MatchAll` query. This receipt
records what the chunked `HYPSMAN2` manifest and lazy eligibility changed at
250k, and measures the complete lexical/doc-value ladder at 1,000,000
documents for the first time.

It does **not** raise `MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS`, which stays
at 250,000. R5 in the RAG roadmap gates the 1M rung on three things: a
manifest that no longer rewrites 16 bytes per document per batch (met
here), measured ANN consolidation cost, and RSS at 1M × 768-dim vectors.
The two vector conditions are not measured: the ladder corpus carries no
vectors, and a vector-carrying batch still takes the materialized ingest
transaction because the ANN store has no delta stage (handoff open item 2).
The bound moves only when that receipt exists.

## Manifest

| rung | format | chunks | header bytes | largest chunk | header decode | full materialization | one membership probe |
|------|--------|--------|--------------|---------------|---------------|----------------------|----------------------|
| 100k (legacy directory) | `HYPSMAN1` | — | 1,600,012 | — | 0.54 ms | 1.4 ms | 0 µs (in-memory vector) |
| 100k after first accepted batch | `HYPSMAN2` | 98 | 1,976 | 16,412 | 0.27–0.30 ms | 1.8–1.9 ms | 6–7 µs |
| 250k (legacy directory) | `HYPSMAN1` | — | 4,000,012 | — | 1.7 ms | 3.8 ms | 0 µs |
| 250k fresh | `HYPSMAN2` | 487 | 9,756 | 14,636 | 0.8–1.0 ms | 4.8–5.3 ms | 10–16 µs |
| 1M fresh | `HYPSMAN2` | 1,952 | 39,056 | 13,356 | 2.6 ms | 21 ms | 17–19 µs |

The per-batch manifest write is now the header plus the chunks the batch
touches: with sequential identities one chunk of at most 16 KB plus a 10 KB
(250k) or 39 KB (1M) header, against 4 MB and 16 MB before. Sequential
inserts fill chunks to between half and full capacity (midpoint splits), so
the chunk count is roughly `2 × documents / 1,024`; the derived bound
`MAX_PRODUCT_SEARCH_MANIFEST_CHUNKS` (978 at the 250k bound, 3,908 at 1M)
holds with margin. Full materialization is paid only by vector allowlists
and `Not` / `IsNull` filters; every other query probes chunks.

Upgrade of a legacy directory: reads on the 100k `HYPSMAN1` corpus served
the value unchanged (same `stage=integrated` 18.5 ms as before); the first
accepted 256-document batch decoded the 1.6 MB blob, packed it into 98
chunks, and committed in 0.8 s.

## Ingest

| rung | docs/s | ingest wall | windowed maintenance | final vacuum | directory after maintenance |
|------|--------|-------------|----------------------|--------------|-----------------------------|
| 250k, legacy manifest (previous receipt) | 949 | 263 s | — | 48.1 s | 2.1 GB |
| 250k, chunked manifest | 1,099 | 227 s | — | 50.6 s | 232 MB |
| 250k, plus the B+tree split fix `b53348e` | 1,346 | 186 s | — | 41.7 s | 232 MB |
| 1M, chunked manifest | 1,014 | 986 s | 9 rounds every 400 batches, 868 s | 185 s | 995 MB |

`ingest wall` excludes windowed maintenance. The 1M load ran with
`HYPHAE_SCALE_MAINTENANCE_EVERY=400` because the unmaintained directory grew
at ~1.7 GB/min (3.5 GB after 120,000 documents); an unmaintained 1M load
would transit roughly 28 GB, which this host's disk could not hold. With
the windows the transient directory stayed between 3 and 6 GB. Ingest
throughput is flat from 250k to 1M (1,099 → 1,014 docs/s); the previous
receipt's 100k → 250k slope (1,261 → 949) included the manifest rewrite.

The directory after maintenance shrank from 2.1 GB to 232 MB at 250k: the
retired page generations of the 4 MB-per-batch manifest rewrites were the
bulk of what vacuum could not reclaim before the format change.

## Open

| rung | reopen after maintenance |
|------|--------------------------|
| 250k, legacy manifest (previous receipt) | 35.6 s |
| 250k, chunked manifest | 24.8 s |
| 250k, plus the B+tree split fix `b53348e` | 22.5 s |
| 1M, chunked manifest | 106.6 s |

Open still scales with retained pages plus one complete-state decode; 1M
is 4.3× the 250k time for 4× the documents.

## Query ladder (p50, 16 samples, `limit=10`, candidate limit 1,000, reopened corpora)

| rung | bm25 | filtered (`price < 500`) + facet | phrase | fuzzy (distance 1) |
|------|------|----------------------------------|--------|--------------------|
| 250k, legacy manifest (previous receipt) | 39 ms | 46 ms | 42 ms | 61 ms |
| 250k, chunked manifest, before lazy eligibility | 39.3 ms | 44.0 ms | 44.2 ms | 62.6 ms |
| 250k, chunked manifest + lazy eligibility | 24.2 ms | 38.8 ms | 28.8 ms | 46.2 ms |
| 250k, plus the B+tree split fix `b53348e` | 22.3 ms | 29.3 ms | 23.5 ms | 39.5 ms |
| 1M, chunked manifest + lazy eligibility | 171.9 ms | 233.4 ms | 174.5 ms | 308.2 ms |
| ratio 250k → 1M (4× documents) | 7.1× | 6.0× | 6.1× | 6.7× |

The chunked format alone left query latency unchanged (it moved the ingest
cost); lazy eligibility removed the per-query manifest clone: bm25
39 → 24 ms, phrase 44 → 29 ms at 250k. The first bm25 sample after a fresh
load remains a cold-cache outlier (p95 49 s at 1M, 12 s at 250k) and is
reported, not excluded.

Diagnostic stages (`scale_stage_diagnostic`, 3 warm rounds):

| stage | 250k | 1M |
|-------|------|-----|
| durable scorer, "database engine", 1,000 hits | 22–24 ms (393 segments, 93,702 entries) | 155–168 ms (1,562 segments, 372,418 entries) |
| integrated `MatchAll` | 19–25 ms | 153–174 ms |
| integrated sparse (2 hits) | 0.4–0.7 ms | 0.4–1.0 ms |
| integrated `category = …` (Eq filter) | 28–31 ms | 184–198 ms |
| integrated `price < 500` + facet | 35–41 ms | 226–230 ms (500,000 eligible) |
| integrated fuzzy (distance 1) | 49–69 ms | 288–308 ms |

The 1M ladder is superlinear, and the manifest is no longer the reason: the
sparse query costs under a millisecond at both rungs, and the durable
scorer alone accounts for the `MatchAll` time. The scorer spends ~430 ns per
physical posting entry at 1M against ~235 ns at 250k for the same two
terms; the candidate causes are buffer-pool residency of the 1,562 posting
segments and the parallel scorer path that never activates in the product
surface (`workers=1`, handoff open item 3). Both are the next diagnostic
targets, not this receipt's claim.

### After the B+tree split fix (`b53348e`)

The bare-metal receipt of the same day traced a materialized-path regression
to `93dc3d3`: `upsert_sorted_batch` split an overflowing rewritten leaf
full-plus-remainder, so every scalar `SET` — manifest chunks and doc-value
postings included — degenerated the tree toward one leaf per key. With the
even split, a fresh 250k load on this host ingests at 1,346 docs/s (from
1,099), vacuums in 41.7 s (from 50.6), reopens in 22.5 s (from 24.8), and
the durable scorer stage drops from 22–24 ms to 13.6 ms for the same 393
segments and 93,702 entries because the posting leaves are dense again;
the reopened ladder is in the table above. The 1M rows predate the fix and
are pessimistic.

## Scorer equivalence

`scale_stage_diagnostic` compares the retained-model scorer's ranked hits
against the durable scorer's — document ids, order, and score bits — and
fails the run on any divergence. The chunked manifest does not touch the
scorer, and the 250k comparison recorded in the previous receipt stands.
The 1M comparison (`HYPHAE_DIAG_MODEL_ROUNDS=1`) was started on the
measurement host after the ladder; its outcome is appended to this file
when the run completes.

## Correctness gate

fmt clean, clippy `-D warnings` clean, 1,772 workspace tests passed on the
devbox at `92f3c7a` and again at `51d8fb0` (19 new tests: codec round trips
and fail-closed decodes, deterministic splits and merges, the adjacent-pair
invariant under a 20,000-operation random sequence, the bound at
`MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS`, byte-identical records from the
delta, materialized, and operation-batch paths across a split before and
after reopen, legacy read compatibility with a first-accepted-mutation
upgrade on a reopened directory, pagination continuity across a deleted
chunk boundary, and a durable chunk-key delete).

## Boundaries

- Synthetic corpus with short documents and one unique term per document
  (`document {ordinal}`), so the fuzzy dictionary grows linearly with the
  corpus; a natural-language corpus has a sublinear dictionary.
- No named vectors: vector-carrying batches keep the materialized ingest
  transaction and are not measured; RSS and ANN consolidation cost at 1M
  are not measured. These are the R5 conditions still open for the 1M
  rung.
- Sequential identities: a batch of 256 random identities would touch up to
  256 chunks (≤ 4 MB of chunk writes) — constant in the collection size,
  but not measured here.
- Single host class; no aarch64, no hosted CI; one run per configuration.
- The 1M load interleaved maintenance every 400 batches; the ingest wall
  time excludes those windows and their cost is reported separately.
