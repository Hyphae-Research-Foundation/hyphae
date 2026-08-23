# Cross-host determinism evidence — FiQA byte-identical directories

- Date: 2026-08-22
- Harness: `tools/rag_eval.py` (sealed protocol, receipt
  `hyphae-rag-relevance-receipt-v1`)
- Dataset: BEIR FiQA-2018, 57,638 documents, 648 test queries, archive
  SHA-256 `32c7df99ed21252fdfb2cf3f5673502a8d245ee0c44c4a133570d92ce2b3ad02`
- Engine: `hyphae 1.2.2`, integrated lexical branch, candidate limit
  1,000, k=10, maintenance interval 32 batches — identical protocol
  constants on both hosts

## Protocol

The identical harness invocation ran on two deliberately different
machines: different distribution, kernel line, glibc, CPU generation, and
Python interpreter. Nothing was copied between them except the pinned
dataset archive, whose digest is verified before use.

| | Host A | Host B |
|---|---|---|
| Platform | Linux 7.0.12 (Fedora 44), glibc 2.43 | Linux 5.15.0-185 (Ubuntu 22.04), glibc 2.35 |
| CPU | Intel Xeon Platinum 8358 | Intel Xeon 6767P |
| Python | 3.13 line | 3.11.0rc1 |

## Measurements

Both receipts report, to the digit and to the byte:

| Measure | Host A | Host B |
|---|---|---|
| `ndcg@10` | 0.235959 | 0.235959 |
| `recall@10` | 0.298234 | 0.298234 |
| `mrr@10` | 0.293643 | 0.293643 |
| Directory bytes after ingest | 55,562,927,163 | 55,562,927,163 |
| Directory bytes after maintenance | 564,961,635 | 564,961,635 |

The transient directory peak (55.6 GB across 7,208 windowed ingest,
checkpoint, and vacuum cycles) and the final maintained directory
(565 MB) agree byte-for-byte across hosts. That is not a metrics
coincidence: every page, WAL generation, and index build the two runs
produced was the same size at every measured point.

## What this claims and what it does not

The engine's committed state is a pure function of the ingested content
and the protocol constants — independent of distribution, kernel, glibc,
CPU model, and interpreter version on x86-64 Linux. This is the
run-twice-and-diff property at cross-host strength: deterministic HNSW
levels, deterministic maintenance, no wall-clock or entropy leakage into
durable state.

It does not claim cross-architecture identity (aarch64 is unmeasured), and
it does not extend to the attested embedding tool's float pipelines beyond
what the [attested replay evidence](attested-embed-replay-2026-08-22.md)
separately establishes on CPU.

Full receipts: `~/hyphae-eval/fiqa-100k-receipt.json` (Host A),
`~/hyphae-eval/fiqa-baseline-receipt.json` (Host B).
