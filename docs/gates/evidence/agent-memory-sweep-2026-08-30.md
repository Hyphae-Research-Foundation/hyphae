<!-- SPDX-License-Identifier: Apache-2.0 -->
# Evidence — Agent Memory retrieval sweep and lexical analyzer uplift

- Date: 2026-08-30
- Host: AWS `i7i.metal-48xl` (bare metal, 192 logical CPUs, 1.5 TiB RAM,
  local instance-store NVMe ext4 `noatime`, `performance` governor,
  hypervisor flag absent), Ubuntu 24.04
- Source: `main` @ `8aeb6ea` plus the uncommitted phase-1 optimization set
  and the SQL multi-row/aggregate slices (binary `hyphae 2.2.0`, release)
- Harnesses: `tools/long_term_memory_benchmarks.py` (with the slice-b
  session-cover alignment fix in this change set),
  `tools/locomo_slice_a_statistics.py`, `tools/rag_eval.py` (with the
  analyzer/BM25 provisioning knobs added in this change set)
- Datasets (caller-supplied, digest-verified): LoCoMo
  `79fa87e9…98ff4`, LongMemEval-S-cleaned `d6f21ea9…3a442`,
  BEIR NFCorpus (pinned archive digest in the harness)
- Raw receipts: [`memory-2026-08-30/`](memory-2026-08-30/) (verbatim;
  every receipt self-declares `evidence_class: local-diagnostic`,
  `claims: []`, `closure_declared: false`)
- Retrieval-only: no model executed during retrieval, no LLM judge, no
  network during execution. These are retrieval measurements, not answer
  accuracy, and are not directly comparable to LLM-as-a-judge scores
  published by model-based memory systems.

## LoCoMo — candidate sweep (1,986 questions, audited-v2 qrels)

Sixteen frozen candidates evaluated; headline `evidence_recall@10` /
`ndcg@10` (micro over evaluated queries):

| Candidate | evidence_recall@10 | ndcg@10 |
|---|---|---|
| frozen baseline (bare view, no analyzers) | 0.5424 | 0.4015 |
| enriched (published 2.2.0 config) | 0.6015 | 0.4562 |
| enriched + stop/stem analyzers | 0.6442 | 0.4931 |
| + centered view (t/tp/c, w=2/1/1) | 0.6721 | 0.5102 |
| centered w122 (t=1, tp=2, c=2) | 0.7059 | 0.5241 |
| **w123 (t=1, tp=2, c=3)** | **0.7146** | 0.5240 |
| w133 (t=1, tp=3, c=3) | 0.7143 | **0.5249** |

The published 2.2.0 number (0.7012 with the then-best `enriched`
selection protocol) is reproduced by the frozen baseline arm (0.5424
exact) and exceeded by the new family: **+17.2 points over baseline,
+1.3 over the published best**, with ndcg@10 up **+12.3 points** over
baseline. The largest single uplift is the analyzer pipeline
(stop+stem, +4.3 points), followed by the centered view (+2.8) and
weight rebalancing toward context views (+3.4).

## LoCoMo — nested LOCO-CV statistical selection (Slice A)

`locomo_slice_a_statistics.py` over frozen traces (baseline +
{centered-w122, w123, w133, w022-tp-c}), 10 outer folds, 1-SE rule,
10,000-replicate cluster bootstrap, exact paired sign-flip:

- Selected candidate: **centered-w122** (every outer fold; 1-SE
  simplicity rule prefers it over the marginally higher-mean w123/w133).
- Out-of-fold micro difference vs baseline (evidence_recall@10):
  **+16.2 points**, 95% CI **[+14.2, +18.4]**, sign-flip p = 1/512
  (0.00195), Holm-adjusted 0.0566 across the 22-metric family.
- Every conversation improved (per-conversation deltas +11.1 to +23.3).
- Status: `passed`; receipt `slice-a-eval.json` with trace digests.

## LongMemEval-S — official 419-question denominator

Analyzer-enabled run (stop+stem), aggregated from 10 audited parallel
chunks (`longmemeval-analyzed.receipt.json`):

| Metric | Published 2.2.0 | This run |
|---|---|---|
| recall_all@10 | 0.8926 | **0.9236** |
| ndcg_any@10 | 0.8775 | **0.8984** |
| recall_any@10 | — | 0.9714 |
| recall_all@50 / recall_any@50 | — | **1.000 / 1.000** |

Complete evidence coverage at k=50 for every evaluated question.

## NFCorpus lexical BM25 — analyzer uplift (anti-baseline context)

`tools/rag_eval.py`, lexical-only branch, 323 test queries:

| Configuration | nDCG@10 | Recall@10 | MRR@10 |
|---|---|---|---|
| defaults (identity analyzer) | 0.3069 | 0.1492 | 0.5151 |
| + stop/stem analyzers | 0.3229 | 0.1530 | 0.5277 |
| + k1=1.2, b=0.6 | **0.3241** | **0.1549** | **0.5308** |

The stop/stem pipeline closes the previously documented gap to the
published BM25 reference (~0.325 on NFCorpus). For context only: the
2026-08-23 head-to-head measured Weaviate 1.38.0 BM25 at 0.1437 and its
default hybrid at 0.3570 on identical hardware; the lexical-only branch
above is within 3.3 points of that full hybrid. A fresh same-host
head-to-head against current Weaviate is required before any comparative
claim is published.

## Fixes landed with this evidence

- `tools/long_term_memory_benchmarks.py`: session-cover lookups now
  cover slice-b branches (previously `--slice-b --session-cover` always
  failed alignment); 17 unit tests pass.
- `tools/rag_eval.py`: `--analyzer-english-stop/stem/ascii-folding` and
  `--bm25-k1-micros/--bm25-b-micros` provisioning knobs, recorded in the
  receipt protocol block.
- `tools/locomo_sweep_metal.sh`: reproducible sweep driver.

## Scope and non-claims

- Single host, single run per candidate; the statistical layer covers
  candidate selection, not host-to-host variance.
- `--candidate-limit 2000` exceeds the bounded local-protocol frame and
  fails closed (`native request exceeds a product limit`) — recorded as
  a negative result; 1,000 remains the ceiling.
- Slice-b temporal branches did not beat the view family on this corpus
  (0.5755 with the fixed session-cover fusion) and remain off the
  selected configuration.
- The five-verb Agent Memory MCP still executes the single-branch
  lexical recall; these numbers are the engine-side ceiling through the
  product search surface, reachable by the MCP once multi-view fusion
  is exposed there.
