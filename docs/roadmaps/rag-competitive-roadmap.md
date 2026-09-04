# RAG competitive roadmap — matching and beating Weaviate at local scale

Status: adopted. This document extends
[`native-acceleration-roadmap.md`](native-acceleration-roadmap.md) (adopted
2026-08-20). It renumbers nothing: Phase 0 (embedded contention) remains
blocking, Phases 1–6 proceed as adopted, and every track below names the
adopted phase it extends. All standing rules apply: fail-closed, bounded,
deterministic, every claim measured under a G7-style protocol before it is
published, zero new dependencies in the proof-bearing core.

## Thesis

Weaviate leads on breadth and scale-out; those remain its strengths and
this roadmap does not contest them. Hyphae differentiates on
verifiability, determinism, local cost, and a fully local feature set —
structural properties of Hyphae's architecture, not judgments about
anyone else's. The strategy: close the six disqualifying RAG gaps
(collection cap, lexical maturity, fusion options, filter operators,
embeddings, framework integrations), invest in four differentiators no
local engine ships today (proof-carrying RAG, deterministic chunking with
provenance, MCP proof tools, migration receipts), and explicitly defer
the rest. Compete directly at up to one million documents; above that,
concede the category argument on purpose.

The adopted positioning ("Weaviate, Milvus, Qdrant: a category argument,
not a benchmark one") is sharpened, not abandoned: it still holds for
scale-out. At local RAG scale, Hyphae competes directly on proof,
determinism, memory, and price, with every comparative number bound to a
same-host receipt under the claims policy.

## Gap matrix

Impact classes: DISQUALIFYING = a RAG evaluation ends without it.
IMPORTANT = loses evaluations. NICE = tie-breaker. Responses: MATCH,
COUNTER (different mechanism, same job), NOT-COMPETE (outside the product
boundary — single node, local-first, one process, one directory).

| # | Capability | Weaviate | Hyphae today | Impact | Response |
|---|---|---|---|---|---|
| 1 | Filtered-search scale | ACORN in-graph over millions of objects | 250,000-document collection cap, raised on receipt (was 10,000); full linear scan removed from the product search path, `MatchAll` eligibility probes manifest chunks and filters resolve through the doc-value posting index. 1,000,000 documents measured linear (3.3–4.5× latency for 4× the documents) but not shipped — waits on the ANN delta stage and vector-batch ingest ([`collection-cap-250k-2026-09-02.md`](../gates/evidence/collection-cap-250k-2026-09-02.md), [`hyphae-3.0-metal-a443c52-2026-09-03.md`](../gates/evidence/hyphae-3.0-metal-a443c52-2026-09-03.md), CHANGELOG §3.0.0) | DISQUALIFYING | MATCH — R track, cap stays 250k until ANN delta stage |
| 2 | Embedding generation | Provider vectorizers + local models + managed service | none (bring-your-own f32) | DISQUALIFYING | MATCH — E track, out-of-graph |
| 3 | Lexical maturity | BM25F, many tokenizers, stop-word presets, property boosting | BM25F with per-field weighted boosts (ported into the native runtime, cross-engine equivalence harness), per-field analyzer selection, in-tree stemmer and stop-word presets, lexical `AND`/`OR` with minimum-match, bounded prefix expansion, fuzzy expansion, phrase matching (CHANGELOG §3.0.0) | DISQUALIFYING | MATCH — L track delivered in 3.0.0 (L2/L3); one analyzer pipeline, not a tokenizer marketplace |
| 4 | Hybrid fusion options | alpha weighting + relative-score + ranked fusion | RRF, weighted-score (alpha), and relative-score fusion, plus autocut knee truncation, range facets, and offset pagination over the final ranking (CHANGELOG §3.0.0, F1) | DISQUALIFYING | MATCH — F track delivered in 3.0.0 (F1); F2 relevance evaluation still open |
| 5 | Filter operators and types | ContainsAny/All, Like, IsNull, date/number/array/nested, geo | Eq/Ne/Lt/Le/Gt/Ge, In, IsNull, bounded Like (substring), boolean All/Any/Not composition, over bool/int/string/bytes/float doc values, plus Date (canonical epoch integer) and Array doc values (CHANGELOG §3.0.0, R6) | DISQUALIFYING | MATCH core set delivered in 3.0.0; ContainsAny/ContainsAll, geo, and nested still NOT-COMPETE |
| 6 | LangChain/LlamaIndex presence | deep, everywhere | none | DISQUALIFYING (a discovery channel, not a feature) | MATCH — I track |
| 7 | Rerankers | provider and local reranker modules | exact-distance rerank only | IMPORTANT | MATCH — rides the E track |
| 8 | Generative search (retrieve + LLM in one call) | 15+ providers, per-query selection | none | IMPORTANT | COUNTER — an LLM call never enters the engine; compose above it and bind the retrieved context with a proof |
| 9 | MCP server | GA, four tools (query, upsert, config, tenants) | three admin-only tools | IMPORTANT | MATCH AND BEAT — M track adds a proof tool nobody has |
| 10 | Quantization (PQ/BQ/SQ/RQ, multi-vector) | mature | none | IMPORTANT (memory optics) | DEFERRED — adopted stance stands (opt-in, never default, receipted); counter now with recall-risk honesty and measured RSS |
| 11 | Query Agent (cloud test-time compute) | GA, cloud-only | none | IMPORTANT | COUNTER — local attested rerank plus published harness numbers; position on the fully local alternative |
| 12 | Multi-vector / late interaction (ColBERT, MUVERA) | GA | 16 named-vector branches, no late interaction | NICE at local scale | DEFER (revisit after F and E tracks) |
| 13 | Native chunking | absent (delegated to frameworks) | absent | IMPORTANT | LEAPFROG — C track: deterministic chunking with provenance in the proof |
| 14 | Highlighting | yes | budgeted snippets (fragment count and byte budget) over re-derived positions, covering expanded terms (prefix, fuzzy, phrase) (CHANGELOG §3.0.0, L4) | NICE | MATCH delivered in 3.0.0 (L4) |
| 15 | Disk-based billion-vector indexes | GA | no | n/a at local scale | NOT-COMPETE |
| 16 | Sharding / replication / consensus | yes | single node | n/a | NOT-COMPETE (post-G8 non-goal stands) |
| 17 | Native multi-tenancy with tiered offload | yes | explicit non-goal | n/a | NOT-COMPETE — tenancy by directory |
| 18 | Managed cloud (embeddings, agents, memory service) | yes | no | n/a | NOT-COMPETE — Hyphae positions on the fully local alternative |
| 19 | GraphQL API | yes | typed SDKs, SQL, local protocol | none | NOT-COMPETE |

## Differentiators

Each pairs a capability the local-RAG market does not ship today with the
Hyphae capability that provides it and the claim it unlocks. A claim
ships only with its measurement, names no competitor deficiency, and
follows the [claims policy](../product/claims.md).

1. Offline verifiability → offline-verifiable native proofs for lexical,
   exact-vector, ANN, and hybrid operations, extended by the Phase 4
   Proof of Retrieval. Claim: your retrieval ran exactly this, provable
   offline, without trusting the operator.
2. Determinism → deterministic HNSW (definition-pinned seed,
   digest-derived levels, byte-identical rebuilds). Claim: same corpus,
   same query, same result — run it twice and diff.
3. Operational simplicity → one static binary, bounded budgets,
   fail-closed limits, no tuning surface. Any memory comparison ships
   only with a measured side-by-side RSS protocol.
4. Fully local feature set → everything Hyphae ships is Apache-2.0 and
   runs locally, including agents-facing memory and attested embeddings.
   Claim: no Hyphae feature is behind a subscription.
5. Local cost → zero-cost local operation at the 10^5–10^6-chunk scale
   where most private RAG lives.
6. Native chunking → deterministic chunker with provenance digests bound
   into proofs.
7. Evidence-bearing MCP → an MCP surface with search, ingest, and proof
   tools.
8. Exit accounting → importers with sealed G10 fidelity receipts, so
   migration in either direction is verifiable. Claim: migrate with a
   receipt.

## Model integration: two routes, both shipped, one flagship

**Route A — provider layer (SDK-resident, out-of-graph).** Optional
extras in the Python and TypeScript SDKs (OpenAI, Cohere, Voyage, Mistral,
Google, plus Ollama for a local bridge), including provider rerank
endpoints. Zero new Rust dependencies — HTTP lives in the SDK languages.
Weeks to provider-checklist parity. Its structural limit: attestation is
weak — a provider embedding can bind an identifier, never weights, and
remote models change silently.

**Route B — local models via `hyphae-embed` (the adopted Phase 4 design).**
An out-of-graph binary in its own Cargo workspace with its own dependency
policy, built on **candle** (pure Rust, no C toolchain, covers the
BERT-family embedders and cross-encoder rerankers; ONNX Runtime rejected as
a supply-chain surface, llama.cpp rejected as the wrong tool). It signs
model, weights, tokenizer, input, output, and target triple. The
proof-bearing core gains exactly one thing: a pure verifier for the
attestation envelope. Float determinism is claimed per attested target
triple, never bit-identical across ISAs — the recall-risk discipline
applied to embeddings.

**Both routes are kept honest inside proofs by attestation classes**,
borrowing the G10 fidelity-class pattern: `AttestedLocal` (weights digest,
replayable) versus `DeclaredProvider` (identifier only). Route A can never
dilute the proof claim because the proof states which class it got. Route A
ships first (fast, neutralizes the checklist objection); Route B is the
flagship — the only fully attested embedding and rerank pipeline on the
market.

## Tracks and waves (PR-level, each independently green)

Tracks: **R** retrieval scale (extends Phase 1) · **L** lexical · **F**
fusion · **C** chunking · **E** embeddings/rerank (extends Phase 4) · **M**
MCP · **V** evaluation · **I** integrations · **A** agent memory · **G**
migration (extends Phase 6). SIMD (Phase 2) and GPU (Phase 3, gate G9)
proceed as adopted and are orthogonal.

### Wave 1 — cheap, high-impact (independent of Phase 0)

- **M1** MCP `search-hybrid`, `search-lexical`, `get-document` (read-only,
  RBAC-scoped). Exceeds Weaviate's query tool immediately.
- **M2** MCP `ingest-documents` (write-scoped principal required; existing
  batch limits enforced, fail-closed).
- **M3** MCP `prove-search` + `verify-proof`; routing search-proof
  generation through the service path also closes the "CLI cannot generate
  search proofs" gap.
- **M4** `capabilities` exposes search and vector limits (today a caller
  cannot discover the collection cap or hit cap).
- **L1** Tunable BM25 `k1`/`b` per lexical index, bound into the proof.

### Wave 2 — raise the 10,000-document cap; lexical and fusion parity — SHIPPED in 3.0.0

- **R1** Relevance harness: NDCG@10, recall@k, MRR over local-scale public
  datasets with pinned digests, seeds, and hardware declaration — the G7
  protocol applied to relevance. This is the adopted Phase 1 "measurement
  gap" deliverable, extended. Everything later measures against it.
- **R2** Doc-value posting index: per-(field, value) postings persisted
  through the existing tree keyspace, WAL-covered, deterministic layout;
  equivalence-tested against the linear scan.
- **R3** Deterministic in-tree bitmap masks — CLOSED BY EVIDENCE at the
  100k rung (2026-08-22, dedicated 28-vCPU host, 20,000 documents,
  1,000-candidate lexical queries): match-all eligibility costs 5.04 ms per
  query, exact-equality filtering 4.93 ms (the posting index reduces the
  candidate set below the unfiltered cost), bounded membership 5.96 ms,
  missing-field 5.20 ms, and the worst composite (range plus negation)
  8.37 ms — 1.66x match-all, under the 2x threshold this item set for
  taking on bitmap complexity. The ordered posting index answers filtered
  eligibility in single-digit milliseconds; re-evaluate when the 1M rung's
  write-path work lands. **Status: shipped, closed by evidence.**
- **R4** Masks wired into ANN strategy selection; the full-collection scan
  in the product search path is deleted. Evidence: filtered recall/latency
  at 100k documents across a selectivity sweep, all recall-risk labels
  exercised; the shipped 3.0.0 search path answers `MatchAll` eligibility by
  probing manifest chunks instead of a linear scan (CHANGELOG §3.0.0).
  **Status: shipped.**
- **R5** Staged collection-cap raise: 10k → 100k on R4 evidence; → 250k on
  the [2026-09-02 rung receipt](../gates/evidence/collection-cap-250k-2026-09-02.md)
  (point-resolved batch ingest 48 → 949–1,261 docs/s, open decoding
  complete state once, borrowed-leaf durable scorer 73 → 26 ms bm25 p50 at
  100k and 63 ms at 250k, bit-identical model/durable scorer hits at both
  rungs); → 1M on measured consolidation cost and RSS at 1M×768-dim plus a
  manifest that no longer rewrites 16 bytes per document per batch. The
  manifest condition is met by the chunked `HYPSMAN2` manifest
  ([2026-09-03 receipt](../gates/evidence/collection-manifest-chunked-1m-ladder-2026-09-03.md):
  1,014 docs/s flat from 250k to 1M, bm25 p50 24 ms at 250k and 172 ms at
  1M, reopen 25 s / 107 s); the two vector conditions wait on an ANN delta
  stage, since a vector-carrying batch still takes the materialized ingest
  transaction. **Status: shipped at 250k; 1M measured, not shipped.**
  `MAX_PRODUCT_SEARCH_COLLECTION_DOCUMENTS` moved to 250,000 in 3.0.0 on the
  above receipt. After the B+tree split fix and the 8,192-frame buffer pool,
  the dedicated-hardware re-measurement
  ([`hyphae-3.0-metal-a443c52-2026-09-03.md`](../gates/evidence/hyphae-3.0-metal-a443c52-2026-09-03.md))
  found the 1M query ladder linear in the document count — bm25 23.2 ms,
  filtered+facet 42.6 ms, phrase 24.2 ms, fuzzy 54.2 ms, a 3.3–4.5× latency
  increase for the 4× document increase (was 6.6–8.3× before the fix); the
  250k rung is unchanged (bm25 6.2 ms, filtered+facet 10.6 ms, phrase
  7.3 ms, fuzzy 12.0 ms). The cap stays 250k until the ANN delta stage: the
  1M rows measure above the shipped bound and the R5 vector conditions
  (consolidation cost, RSS at 1M×768-dim) remain unmeasured. The cap moves
  only when the receipt exists.
- **R6** Filter operators In / ContainsAny / ContainsAll / bounded Like /
  IsNull and doc-value types Float, Date (canonical epoch integer), Array —
  product model, SQL surface, proof binding, SDKs. Decided ordering story:
  Float ingest rejects non-finite values fail-closed, comparison uses IEEE-754
  `total_cmp` over the admitted finite domain, and the memcomparable posting
  encoding is the sign-flipped big-endian bit pattern; Array ordering is
  lexicographic over already-ordered elements. **Status: partially shipped.**
  In, IsNull, bounded Like, and the Float/Date/Array doc-value types shipped
  in 3.0.0 (CHANGELOG §3.0.0); ContainsAny and ContainsAll remain open.
- **L2** BM25F port from the legacy engine (weighted fields with the
  existing caps) into the native runtime, gated by an output-equivalence
  harness against the legacy implementation. Ported: the runtime hosts the
  weighted-field scorer (canonical-analyzer tokenization, vendored msun
  logarithm, half-up nano quantization) with a cross-engine harness proving
  exact ranking and nano-score equality. Adoption by the integrated
  collection surface requires the multi-field ingest model (per-field texts
  in the document codec) and rides the chunking wave.
- **L3** Analyzer pipeline: make the catalog's analyzer types real —
  stop-word presets and an in-tree stemmer, per-field analyzer selection;
  the analyzer configuration digest enters index identity and proofs.
- **F1** Weighted-score (alpha) fusion with relative-score normalization,
  deterministic tie-break, alongside RRF; the proof model already reserves
  the variant. **Status: shipped in 3.0.0** (CHANGELOG §3.0.0: relative-score
  fusion beside RRF and weighted-score, autocut knee truncation).

### Wave 3 — leapfrog: chunking with provenance

- **F2** Fusion evaluation on the R1 harness, published. The fusion default
  becomes a measured choice.
- **C1** Deterministic chunker: byte-offset based, fixed-size/overlap and
  sentence-boundary modes, bounded; chunk identity is a digest over the
  document digest, chunker configuration digest, and byte range. Pure, no
  dependencies; chunks carry parent identity and offsets as doc-values.
- **C2** Parent-aware retrieval: bounded first-k-per-parent deduplication
  at the product layer.
- **C3** Chunk provenance in native-proof result bindings. Claim: every
  retrieved chunk provably traceable to exact source bytes — unavailable
  from Weaviate at any price, because it does not chunk.

### Wave 4 — models and head-to-head evidence

- **E1/E2** Provider extras in the Python and TypeScript SDKs
  (`DeclaredProvider` attestation records; includes provider rerank).
- **E3** Attestation envelope format and pure verifier in the core proof
  subsystem; attestation classes recorded in proofs. Zero new core
  dependencies.
- **E4** `hyphae-embed` (candle) with `embed` and `rerank` subcommands and
  a replay-determinism evidence document per attested target.
- **E5** Attested rerank stage in search options; proofs record the
  reranker class.
- **L4** Highlighting: budgeted snippets from the existing re-derived
  positions machinery.
- **V1** Claim protocol for relevance, latency, and memory (extends the G7
  protocol style).
- **V2** Head-to-head against Weaviate OSS on identical hardware: hybrid
  quality, RSS at 1M×768-dim, cold start, and the run-twice-diff
  determinism demonstration. Published with every measured delta, including
  losses.
- **V3** Counter to the cloud Query Agent: measured uplift of local
  attested rerank over base hybrid on the R1 datasets, honestly framed.

### Wave 5 — distribution and the memory story

- **I1** `langchain-hyphae` (vector store + retriever; proof digest in
  result metadata). **I2** `llamaindex-hyphae`. **I3** Cookbooks.
- **A1** Verifiable agent memory as composition, not new engine features:
  structures with TTL + doc-values + hybrid search + proofs, exposed as
  thin MCP memory tools. **A2** Positioning against the managed memory
  service: local, free, every recall provable.

### Wave 6 — the exit ramp (extends Phase 6 / G10)

- **G1** Weaviate importer, out-of-graph, exporting through the public
  cursor API (its storage format is not a stable contract; the API export
  is the honest consistency point). Objects map to documents, named vectors
  to branches, tenants to separate target directories; quantized vectors
  are DeclaredDegraded; index configuration is Equivalent (graph rebuilt
  deterministically, recall re-measured on the R1 harness).
- **G2** Sealed G10 migration receipt per collection.
- **G3** Migration guide with a post-migration relevance rerun. The receipt
  proves what transferred and what degraded — no other system produces one
  in either direction.

## Claim ladder

Each claim ships only with its evidence: after R5, per-query exactness
labels at 1M documents; after L3+F1, hybrid parity, deterministic; after
C3, chunk provenance in the proof; after M3, the only MCP server with a
proof tool; after E4, attested embeddings; after V2, every published number
carries a protocol; after G2, leave Weaviate with a receipt.

## What this roadmap refuses

Billion-vector disk indexes, sharding/replication/consensus, native
multi-tenancy with tiered offload, managed cloud services, GraphQL, and any
LLM call inside the engine (it would poison determinism and proofs;
generation composes above the engine, bound to a Proof of Retrieval). Geo
filters, nested-object filtering, and late-interaction multi-vector are
deferred, not refused.

## Risks and reversals

In-tree bitmap underperforms → fall back to the `roaring` crate in the
acceleration crate only, dependency-reviewed. candle model coverage gaps →
Route A plus Ollama covers the interim; the attestation format, not the
model list, is the stable contract. BM25F port drift → the legacy
equivalence harness gates it. 1M-document consolidation surprises → the
staged cap raise prevents overclaiming by construction.
