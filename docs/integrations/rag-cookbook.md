<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Provable RAG cookbook

One local binary, one model directory, and a receipt for every step:
chunk deterministically, embed with an attestation, retrieve hybrid with
budgeted highlights, rerank under a sealed envelope, and hand the caller
a proof that verifies offline. Every command below is the shipped
surface — no bench hooks, no private flags.

## 0. Provision

```bash
hyphae init --data-dir ./rag
hyphae catalog --data-dir ./rag create-search-collection \
  --database 10 --schema 11 --collection 13 --analyzer 12 \
  --name main.public.docs --dimension 384
hyphae search --data-dir ./rag provision --collection 13
hyphae serve --data-dir ./rag --endpoint ./rag.sock &
```

## 1. Chunk with provenance

```bash
hyphae search chunk --parent 42 --file guide.md --size 1024 --sentence
```

Every chunk carries `parent`, `byte_start`, `byte_end`, and
`chunk_ordinal` doc-values, and its identity is the first 16 bytes of
the chunk digest — the same bytes reproduce the same identity on every
host. The provenance rides the proof: a verified result names exactly
which byte range of which parent answered.

## 2. Embed with an attestation

```bash
echo '["chunk text one", "chunk text two"]' | \
  hyphae-embed embed --model-dir ./models/bge-small-en-v1.5
```

The output carries the vectors and an `AttestedLocal` `HYATTS01`
envelope binding the weights, input, and output digests. Rerunning the
same weights over the same input must reproduce the output digest — the
[replay evidence](../gates/evidence/attested-embed-replay-2026-08-22.md)
documents exactly that.

## 3. Ingest and retrieve from Python

```python
from hyphae_sdk.v2 import HyphaeClient

with HyphaeClient.local("./rag.sock") as client:
    client.search_ingest(13, {
        "idempotency_id": 1,
        "documents": [{
            "object_id": chunk_id,
            "text": chunk_text,
            "doc_values": {"parent": parent_bytes, "chunk_ordinal": 0},
            "vectors": {"exact": embedding},
        }],
    })
    result = client.search_collection(13, {
        "lexical": {"query": "deterministic retrieval", "candidate_limit": 1000, "weight": 1},
        "vectors": [{"target": "exact", "query": query_embedding, "candidate_limit": 1000, "weight": 1}],
        "limit": 10,
        "highlight": {"max_fragments": 2, "fragment_bytes": 128},
        "parent_dedupe": {"field": "parent", "first_k": 2},
    })
```

Hybrid fusion is the measured default —
[+17.9% nDCG@10 over lexical on NFCorpus](../gates/evidence/rag-hybrid-fusion-nfcorpus-2026-08-22.md)
— and each hit returns budgeted fragments cut from the canonical
normalized text.

## 4. Rerank under a sealed envelope

```bash
echo '["candidate one", "candidate two"]' | \
  hyphae-embed rerank --model-dir ./models/bge-small-en-v1.5 --query "the question"
```

Feed the scores back as the request's `rerank` stage with the
attestation envelope; the engine reorders deterministically and seals
the attestation class in the proof — it never runs the model. On
NFCorpus the attested local rerank lifts lexical retrieval by double
digits without any vector index (see the V3 evidence).

## 5. Prove and verify offline

```python
proven = client.prove("search_collection", {"collection": 13, "request": request})
open("proof.bin", "wb").write(proven.value["proof"])
open("witness.bin", "wb").write(proven.value["witness"])
anchor = proven.value["trusted_anchor"].hex()
```

```bash
hyphae proof verify --proof proof.bin --witness witness.bin --anchor "$anchor"
```

The verifier re-executes the sealed request against the witness with no
engine and no trust in the server — `"scope": "semantic_reexecution"` in
the output means the ranking itself was reproduced. Large witnesses
exceed the in-product verify work-unit cap by design; the offline
verifier is the canonical path.

## 6. Frameworks

The [LangChain and LlamaIndex adapters](optional-adapters.md) wrap steps
3–5: `prove=True` seals a proof per query and every returned document
carries the proof digest in its metadata.

```python
from hyphae_sdk.langchain import HyphaeVectorStore

store = HyphaeVectorStore(client, 13, embeddings, prove=True)
documents = store.similarity_search("deterministic retrieval", k=4)
artifacts = store.last_proof  # verify with `hyphae proof verify`
```

## Agent memory, verifiable

The same composition serves agent memory through three thin MCP tools —
no new engine features, no managed service:

- `hyphae_native_memory_store` ingests one bounded memory under its
  content-derived identity; a scalar lifecycle key carries the text and
  the optional TTL.
- `hyphae_native_memory_recall` retrieves by bounded lexical search and
  returns only memories whose lifecycle key still lives — expired or
  forgotten memories never come back. With `prove`, the recall itself is
  sealed and offline-verifiable.
- `hyphae_native_memory_forget` tombstones the lifecycle key and removes
  the document permanently.

Memory lives in your directory, costs nothing per month, expires on the
engine's deterministic clock, and every recall can carry a proof — a
managed memory service offers none of those properties to verify.

## What no alternative hands you

The receipts referenced above are the point: relevance measured under a
[sealed protocol](../retrieval/claims-protocol.md), byte-identical
committed state across hosts, attestations for every model output, and
proofs a third party checks without trusting you.
