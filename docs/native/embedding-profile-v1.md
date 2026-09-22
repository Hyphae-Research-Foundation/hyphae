<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native embedding profile metadata v1

Status: unreleased next-major embedded product contract; no concrete executor or wire surface

This contract defines a catalogued embedding profile, an optional named-vector
binding, and the embedded-only bounded `EmbedAndIngestBatch` product operation.
The product crate supplies only the executor trait and atomic orchestration. No
current build supplies a concrete model executor, loads model files, contacts a
provider, selects a device, or exposes the operation on a wire protocol.

## Object contract

`CatalogObjectKind::EmbeddingProfile` is append-only kind tag `10`.
`CatalogObjectV2::EmbeddingProfile` is search-owned, has a schema parent, and
uses the existing generic `CreateCatalogObjectV2` WAL mutation (`51`). There is
no embedding-specific WAL opcode.

One `EmbeddingProfileDefinition` canonically contains:

| Field | Contract |
|---|---|
| header | stable nonzero object ID, search owner, valid qualified name, schema parent, nonzero definition version |
| artifact manifest digest | nonzero SHA-256 of the complete artifact-manifest bytes |
| artifact manifest byte length | exact `1..=16,777,216` complete-manifest length |
| pipeline version | `Qwen3EmbeddingV1` (`1`) |
| vector type | canonical IEEE-754 binary32 (`f32`) with dimension `384`, `768`, or `1024` |
| maximum input | `1..=32,768` token positions per formatted input |
| query instruction | exact UTF-8 bytes `Given a web search query, retrieve relevant passages that answer the query` |

The digest and length identify content, not a location. A profile has no
filesystem path, URL, provider name, credential, device selection, mutable
artifact alias, or independently mutable model/config/tokenizer identity. An
all-zero digest and a zero or oversized length are reserved and rejected.

Before execution, the complete manifest bytes must match both bound values and
decode as `hyphae-embedding-model-manifest-v1` with status `verified`. The
manifest, rather than duplicated profile strings, must identify repository
`Qwen/Qwen3-Embedding-0.6B`, one immutable 40-lowercase-hex revision, model type
`qwen3`, native dimension `1024`, supported output dimensions exactly
`[384, 768, 1024]`, canonical output dtype `float32`, safetensors-only weights,
and the path, SHA-256, and byte length of every snapshot file. Missing, extra,
duplicate, or mismatched files fail closed. The checked-in golden binds
revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`; another revision is a
different manifest and therefore a different profile identity.

## Closed pipeline

`Qwen3EmbeddingV1` fixes these stages and their order; none is a profile
option:

1. A query is formatted as `Instruct: {instruction}\nQuery: {text}`. The
   instruction is the exact bound string above, `\n` is one LF byte, and there
   is exactly one ASCII space after each colon. A passage is exactly `{text}`,
   with no prefix, instruction, suffix, or whitespace rewriting.
2. The manifest-bound tokenizer processes one formatted input with
   `add_special_tokens=true`, `truncation=true`, `truncation_side=right`, and
   `max_length=max_input_tokens`. Right truncation discards the suffix. It
   produces exactly one chunk: no overflow chunk, stride, or overlap is
   permitted.
3. Inputs in a batch are left padded to the longest admitted sequence. Padding
   is excluded by the attention mask; no right padding is permitted.
4. The manifest-bound Qwen model evaluates the token IDs and attention mask.
   Pooling selects the hidden state at the greatest token index whose attention
   mask is one for each input (last-token pooling). The native hidden-state
   width is exactly `1024`.
5. The selected vector is converted to canonical `f32`, then projected by
   retaining its leading `D` coordinates, where `D` is the profile dimension
   `384`, `768`, or `1024`. Projection occurs before normalization.
6. L2 normalization is computed in `f32` as
   `y_i = x_i / max(sqrt(sum_j(x_j * x_j)), 1e-12)` over those `D`
   coordinates. A smaller norm uses the `1e-12` denominator floor.

These rules establish profile compatibility only. They do not assert that an
executor is present or that different kernels, libraries, CPUs, or devices
produce byte-identical numeric vectors. Numeric execution requires a
separately versioned backend/execution profile and an attestation that binds
that profile to the result. The pipeline-version tag is therefore a
deterministic metadata identity, not a cross-engine floating-point determinism
claim.

Unknown pipeline, vector-element, kind, or representation tags fail closed.
Instruction drift, unsupported dimensions, malformed manifest identities, and
zero or oversized bounds fail before publication. Invalid catalog names and
owners use the shared catalog validation rules.

## Named-vector binding

`NamedVectorDefinition.embedding_profile` is an optional stable `ObjectId`.
When present, the collection derives one `EmbeddingProfile` dependency edge
(append-only dependency tag `7`) from the collection to the profile. The
referenced object must exist, must have kind `EmbeddingProfile`, and its output
vector element and dimension must equal the named vector exactly. The profile
pipeline must itself be valid. Any missing, wrong-kind, invalid-pipeline, type,
or dimension mismatch rejects the private catalog transaction before root or
WAL publication.

The profile has no dependency on a collection or named vector. Its only edge is
the ordinary schema-parent edge, so this binding cannot create a
profile/collection dependency cycle. Incoming dependency lookup exposes bound
collections. The private catalog-state removal invariant rejects a profile
while any such dependent is live, ready for a future public logical DROP
operation. No public generic logical DROP operation is shipped by this
contract.

## Embedded operation

`ProductOperation::EmbedAndIngestBatch` selects one collection and one
normalized named-vector target. Its `ProductSearchIngestBatch` must be nonempty,
contain at most 256 documents and 16 MiB of logical input, carry a nonzero
idempotency identity, and have empty input vector maps. The operation resolves
the target's catalogued profile and invokes the installed
`ProductEmbeddingExecutor` for passage text only. One finite vector of the
profile dimension and one nonzero input-token count must return for every input
in original order.

The executor receives the immutable profile plus explicit ceilings for input
count and bytes, per-input and aggregate token positions, output dimension, and
output bytes. Product-side request and result walks accumulate checked bounds
without constructing an unbounded aggregate. Cancellation and deadline
checkpoints run before execution and incrementally while validating output.

Authorization binds `CatalogRead + DataWrite` to the collection and separately
binds `CatalogRead` to the embedding profile. Managed authority is reloaded and
both exact object requirements are checked at admission, immediately before
executor invocation, and again after native staging immediately before commit.
Loss of either scope publishes no completion or search state.

The operation writes an internal completion marker in the same native
transaction as the document, lexical, doc-value, generated vector, manifest,
posting, and ordinary search-ingest idempotency mutations. The marker binds the
collection, target, caller idempotency identity, complete profile identity, and
complete vector-free input digest to the original transaction. A matching
durable replay skips executor invocation and publication and returns the
original commit receipt. `ProductEmbedAndIngestReceipt.commit` is non-optional,
so every successful first execution and replay carries definite commit
evidence. A mismatched reuse fails with `idempotency_conflict`.

`NativeProduct::set_embedding_executor` is process-local configuration. Reopen
preserves completion records but requires the caller to reinstall any executor;
a matching replay does not require one. This contract ships no concrete CPU,
accelerator, provider, artifact loader, or job executor.

## Canonical encoding and compatibility

The profile body uses `HYCOBJ02` representation `2` under logical catalog codec
capability `4`. The body is the 32-byte manifest digest, little-endian `u64`
manifest byte length, one-byte pipeline and vector-element tags, little-endian
`u16` vector dimension, little-endian `u32` maximum input tokens, and one
length-prefixed exact query-instruction UTF-8 string.

Existing unbound search collections remain byte-for-byte representation `2`.
Collections with tuned BM25 but no profile remain byte-for-byte representation
`3`. A collection with at least one profile binding uses search representation
`4`: the unchanged representation-2 search body, one canonical optional object
ID per named vector in vector order, then optional BM25 parameters. A
representation-4 body with no binding is noncanonical.

The product API version remains `1` and native protocol minor remains `8`. The
Rust embedded operation and response have no protocol encoding; every request
and response operation tag remains unchanged and request tag `69` remains
absent. Kind-10 filters and profile or representation-4 catalog creation
require minor 8.
Responses containing kind 10, dependency kind 7, a profile definition, or a
representation-4 search definition are rejected before encoding to a
minor-7-or-older peer. Rust, Python, and TypeScript share the kind/dependency
allocation and minor-gating rules.

The current dependency-list request selects only object and direction; it has
no dependency-kind filter. Therefore no new request field is allocated for
dependency kind 7. If a future request adds that filter, selecting kind 7 must
require minor 8. Today the dependency page itself is content-gated at minor 8.

Adding the profile variants and named-vector field changes Rust types that were
public and exhaustive in 3.0.0. This source may ship only in the next major
release. Its presence on the unreleased main branch is not a 3.x capability or
compatibility claim.

Catalog definitions and dependency entries use the existing catalog tree,
backup, restore, describe, resolve, and reopen machinery. Their presence does
not provision a search collection or install a model executor.

The real artifact-manifest golden is
`compatibility/qwen3-embedding-0.6b-artifact-manifest-v1.json`: 8,323 bytes with
SHA-256 `befecdb46c391a8ea205c50a00b54765a46a448c6db09396a4927a51d2b8d357`.
The query-instruction golden records the exact 74 instruction bytes as hex in
`compatibility/qwen3-embedding-query-instruction-v1.hex`. The profile fixture
binds both. The earlier BERT-only fixture never shipped or merged and has no
compatibility standing; this Qwen profile replaces it rather than allocating a
legacy variant. Search representations 2 and 3 and the representation-4
binding layout remain unchanged.
