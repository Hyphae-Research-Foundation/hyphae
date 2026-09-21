<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native embedding profile metadata v1

Status: unreleased next-major catalog incubation; embedding execution is not implemented

This contract is the first prerequisite for a future bounded
`EmbedAndIngestBatch` operation. It defines a catalogued embedding profile and
an optional named-vector binding. It does not define or ship that operation.
No current build loads model files, tokenizes input, runs inference, generates
vectors, starts a job, contacts a provider, selects a GPU, or changes search
ingestion because this metadata exists.

## Object contract

`CatalogObjectKind::EmbeddingProfile` is append-only kind tag `10`.
`CatalogObjectV2::EmbeddingProfile` is search-owned, has a schema parent, and
uses the existing generic `CreateCatalogObjectV2` WAL mutation (`51`). There is
no embedding-specific WAL opcode or product operation.

One `EmbeddingProfileDefinition` canonically contains:

| Field | Contract |
|---|---|
| header | stable nonzero object ID, search owner, valid qualified name, schema parent, nonzero definition version |
| model weights digest | nonzero SHA-256 of the complete safetensors weights bytes |
| model config digest | nonzero SHA-256 of the complete model-configuration bytes |
| tokenizer digest | nonzero SHA-256 of the complete tokenizer-configuration bytes |
| pipeline version | `SafetensorsBertMetadataV1` (`1`) |
| vector type | canonical IEEE-754 binary32 (`f32`) and a nonzero `u16` dimension |
| maximum positions | `2..=1,048,576` token positions |
| maximum input | `2..=maximum positions` token positions per input |
| maximum batch | `1..=4,096` inputs |
| pooling | `FirstToken` (`1`) or `MeanTokens` (`2`) |
| normalization | `None` (`1`) or `L2` (`2`) |
| truncation | `Reject` (`1`) or `KeepStart` (`2`) |

The digests identify content, not locations. A profile has no filesystem path,
URL, provider name, credential, device selection, or mutable artifact alias.
An all-zero digest is reserved and rejected.

`SafetensorsBertMetadataV1` fixes the stage choices and order that a future
implementation must interpret:
tokenizer processing and special-token insertion from the digest-bound
tokenizer configuration; bounded rejection or start-preserving truncation;
model evaluation from the digest-bound config and safetensors weights;
pooling; then normalization. `FirstToken` selects the first
attention-mask-selected output. `MeanTokens` includes every output selected by
the attention mask, including tokenizer-inserted special tokens, and excludes
padding. `L2` divides by the output norm and treats a zero norm as an execution
error. These rules establish profile compatibility only. They do not assert
that an executor is present or that different kernels, libraries, CPUs, or
devices produce byte-identical numeric vectors. Numeric execution requires a
separately versioned backend/execution profile and an attestation that binds
that profile to the result.
The pipeline-version tag is therefore a deterministic metadata identity, not a
cross-engine floating-point determinism claim.

Unknown pipeline, pooling, normalization, truncation, vector-element, kind, or
representation tags fail closed. Zero, inverted, and oversized bounds fail
before publication. Invalid catalog names and owners use the shared catalog
validation rules.

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

## Canonical encoding and compatibility

The profile body uses `HYCOBJ02` representation `2`. The body is the three
32-byte digests in weights/config/tokenizer order, one-byte pipeline and vector
element tags, little-endian vector dimension and three `u32` bounds, then the
one-byte pooling, normalization, and truncation tags.

Existing unbound search collections remain byte-for-byte representation `2`.
Collections with tuned BM25 but no profile remain byte-for-byte representation
`3`. A collection with at least one profile binding uses search representation
`4`: the unchanged representation-2 search body, one canonical optional object
ID per named vector in vector order, then optional BM25 parameters. A
representation-4 body with no binding is noncanonical.

The incubating logical catalog codec capability is `4`. The product API version
remains `1`, native protocol minor advances to `8`, and every request and
response operation tag remains unchanged because this contract adds no product
operation. Request tag `69` remains absent. Kind-10 filters and profile or
representation-4 catalog creation require minor 8. Responses containing kind
10, dependency kind 7, a profile definition, or a representation-4 search
definition are rejected before encoding to a minor-7-or-older peer. Rust,
Python, and TypeScript share the kind/dependency allocation and minor-gating
rules.

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
not provision a search collection or model executor.
