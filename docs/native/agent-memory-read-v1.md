<!-- SPDX-License-Identifier: Apache-2.0 -->
# Snapshot-coherent Agent Memory read v1

`MemoryRecall` is a Native product read, shared by MCP and operator clients.
It composes the existing integrated search and scalar lifecycle reads on one
`ProductSnapshot`; it introduces no storage engine or model dependency.

The request names one to three distinct collections in ascending stable-ID
order, a complete `ProductSearchRequest`, a final result limit (1–64), and
up to 65,536 opaque provenance bytes (for the attested query embedding).
Search admits at most 1,000 candidates per collection. Explicit sorting,
offset, facets and aggregations are not part of this bounded memory profile.
Project/global/kind eligibility remains a typed search filter supplied by the
memory adapter. Every named collection must be authorized for catalog/search
reads; lifecycle access additionally requires instance data-read authority.

For each filter-eligible document the lifecycle key is `hyphae-memory/` followed by the
collection and document IDs as little-endian 128-bit integers. Missing,
expired, or empty lifecycle values are excluded before lexical/vector candidate
limits as well as the final limit. Expired history cannot crowd live memories
out of the bounded ranking or background embedding queue. The exclusion count
describes the filter-eligible documents removed by this lifecycle restriction;
it is bounded by the selected collections' total document counts. The
remaining hits sort by descending score, ascending collection ID, then
ascending document ID. The result retains the exact lifecycle bytes, complete
search receipts and common snapshot identity. Envelopes are application data:
the product does not deserialize JSON or execute a model.

The append-only wire request tag is 71 and the response tag is 45; both require
protocol minor 7. The request encodes a u32 collection count, u128 collection
IDs, u64 final limit, length-prefixed canonical `SearchCollection` request,
and length-prefixed provenance. The nested request's collection must equal the
first collection. The response encodes the common snapshot, per-collection
length-prefixed search results, ordered collection/document references with
their exact envelopes, and the excluded candidate count. All integers are
little-endian. Counts, membership, snapshot equality and canonical ordering
are checked on decoding. A memory proof uses kind 8 and execution semantics 6. Its canonical request,
ordered lifecycle-bearing result, search receipts and snapshot are all sealed.
Offline verification reexecutes this exact composition, including TTL at the
anchored logical time. Old proofs retain their existing formats and semantics;
old peers reject the new operation rather than executing a reduced read.

Embedding generation remains the optional `hyphae-embed` component. A memory
proof proves retrieval over the supplied vectors and stored provenance; model
replay is a separate attested operation. Full witnesses contain retained
directory data and remain private operator artifacts.

`MemoryEnrich` (request tag 72, minor 7) is an operator maintenance mutation.
Its body is collection u128, expected-envelope BLAKE3 (32 bytes), update
idempotency u128, then the existing canonical `ProductDocument` encoding.
It returns `SearchIngested`. It requires a live, nonempty lifecycle matching
the supplied digest and a still-present search source. Text and base doc
values must be unchanged; only `memory`, `embedding_model`,
`embedding_attestation`, and `embedding_manifest` may be supplied as changed
fields. Other vectors are preserved. It always commits strictly and never
creates, resurrects or extends a lifecycle. A late worker result after forget,
expiry, or source modification is rejected.

These additions are an unreleased source candidate based on 3.0.0. They are
not available in the published 3.0.0 crates or SDK packages. Omarchy runtime
bundles must pin the candidate source commit until an upstream release exists.
