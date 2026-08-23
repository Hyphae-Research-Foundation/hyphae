# Attested local embedding replay evidence — bge-small-en-v1.5

- Date: 2026-08-22
- Tool: `hyphae-embed` (excluded `embed/` workspace, candle 0.9, CPU
  execution)
- Target: `bge-small-en-v1.5` (BAAI, BERT family, 384 dimensions),
  operator-provided `config.json` + `tokenizer.json` + `model.safetensors`
- Host: Linux x86-64 (Fedora 44)

## Protocol

Three mixed-language texts were embedded twice through identical
invocations, and reranked twice against the query `provable retrieval`.
Every invocation emits an `AttestedLocal` `HYATTS01` envelope binding the
BLAKE3 digests of the exact model weights, the canonical length-framed
input, and the canonical little-endian output.

## Measurements

- `embed` run 1 and run 2: **attestation envelopes byte-identical**, and
  the full 3×384 vector payloads compare equal element-for-element.
- `rerank` run 1 and run 2: **attestation envelopes byte-identical**;
  scores `[0.4669, 0.8046, 0.5966]` rank the deterministic-retrieval text
  first for the provable-retrieval query.
- Both envelopes share the same weights digest (same `model.safetensors`
  bytes) and differ in input and output digests exactly as the canonical
  framing requires.

Embed attestation (hex):

```
48594154545330310111006267652d736d616c6c2d656e2d76312e356588b38fa23ad1
3648a2678bc8cd8733bf4be79ba12ac6dfa1368d33d80e8fc7ea43d114748f472c6cce
055b84c6883f3d1cef1d165cc16ab2b6116541f9849de387575ee894016bfb2125428f
eb5c6f1bccbfe9faa09bbde1ac5ac9ab0afebb
```

## What this claims and what it does not

The `AttestedLocal` class claims replayability: anyone holding the same
weights bytes and the same input reproduces the same output digest with
this tool on CPU. It does not claim cross-implementation portability of
float pipelines (a different inference engine may round differently), and
GPU execution is excluded from the attested path precisely because
parallel reduction orders are not host-independent. Re-verification is
`hyphae-embed` replay plus the engine's pure `verify_attestation` over the
envelope and payload bytes.
