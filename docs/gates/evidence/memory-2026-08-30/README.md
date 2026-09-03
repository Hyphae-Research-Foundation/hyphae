<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Raw receipts — Agent Memory sweep and Weaviate 1.39 reruns (2026-08-30/31)

Verbatim harness output backing three narrative evidence documents. Nothing
in this directory is hand-edited; every file self-declares
`evidence_class: local-diagnostic`, carries its protocol digests, and
authorizes no publication claim on its own. Read the narratives first:

- [Agent Memory sweep](../agent-memory-sweep-2026-08-30.md)
- [Weaviate 1.39 lexical rerun](../weaviate-139-lexical-rerun-2026-08-30.md)
- [Weaviate 1.39 hybrid rerun](../weaviate-139-hybrid-rerun-2026-08-31.md)

## File inventory

| Files | Producer | What they are |
|---|---|---|
| `locomo-*.receipt.json` | `tools/long_term_memory_benchmarks.py` | LoCoMo retrieval runs (1,986 questions each): the frozen `baseline`, the selected `centered-w122`, and the near-tied `w123`/`w133`/`w022-tp-c` candidates |
| `slice-a-manifest.json`, `slice-a-eval.json` | `tools/locomo_slice_a_statistics.py` | Nested LOCO-CV candidate selection over the frozen traces: outer folds, 1-SE rule, cluster bootstrap, sign-flip; status `passed` selecting `centered-w122` |
| `longmemeval-analyzed.receipt.json` | `tools/long_term_memory_benchmarks.py` | LongMemEval-S official 419-question denominator, aggregated from 10 audited parallel chunks (stop/stem analyzers) |
| `rag-nfcorpus-lex-*.receipt.json` | `tools/rag_eval.py` | NFCorpus lexical BM25 ablation on the memory host: defaults vs stop/stem vs stop/stem+k1/b |
| `h2h139-lexical-*.receipt.json` | `tools/rag_eval.py` + `tools/weaviate_compare.py` | Same-host lexical head-to-head vs Weaviate 1.39.0 |
| `h2h139-hybrid-*.receipt.json` | `tools/rag_eval.py` + `tools/weaviate_compare.py` | Same-host hybrid head-to-head vs Weaviate 1.39.0 with identical attested `bge-small-en-v1.5` vectors on both sides (alphas 0.5 and 0.75) |

## Hosts

LoCoMo/LongMemEval/rag-nfcorpus receipts: AWS `i7i.metal-48xl` (bare metal,
192 CPUs). `h2h139-*` receipts: one DigitalOcean droplet per rerun (lexical
c-16, hybrid c-32), both systems sequentially on the same host. Environment
class is declared inside each receipt; comparisons are same-host relative
A/B under [claims](../../../product/claims.md).

## Datasets

Caller-supplied and digest-verified before every run (LoCoMo
`79fa87e9…98ff4`, LongMemEval-S-cleaned `d6f21ea9…3a442`, BEIR NFCorpus
pinned in the harness). Hyphae does not redistribute them; the receipts
record the digests, not the data.
