# Product capabilities and limits

This page describes the published format-2 `0.2.1` compatibility product. The
current Native `dev` capability matrix is maintained separately in
[`native-capabilities.md`](native-capabilities.md).

Hyphae `0.2.1` is a published local-first Rust data engine. The base
deployment is one native `hyphae` executable and one exclusively owned data
directory. It starts no listener unless `serve` is selected and requires no
database, cache, cloud account, model, embedding provider, or LLM.

## Capability matrix

| Capability | Embedded Rust | Local CLI | HTTP `/v1` | Remote CLI / SDKs | MCP |
|---|---:|---:|---:|---:|---:|
| Atomic KV put/delete | Batch | Single record | Batch | Batch | Batch |
| Exact KV get | Yes | Yes | Yes | Yes | Yes |
| Deterministic structured query | Full AST | Simplified | Full AST | Full AST | Full AST |
| Grouped/global aggregation | Yes | No | Yes | Yes | Yes |
| Logical cursor pagination | Yes | Output only | Yes | Yes | Yes |
| Result proof creation | Explicit method | `--proof-out` | Always | Always | Always |
| Result proof verification | Yes | `verify` | Download artifacts | Download artifacts | Via returned proof |
| Durable vector define/put/delete | Yes | No | Yes | Yes | Yes |
| Exact durable retrieval | Yes | No | Yes | Yes | Yes |
| Provider-free lexical retrieval | Yes | No | Yes | Yes | Yes |
| Deterministic hybrid retrieval | Yes | No | Yes | Yes | Yes |
| Retrieval proof creation | Explicit method | No | Always | Always | Always |
| Retrieval proof verification | Yes | `verify-retrieval` | Download artifacts | Download artifacts | Via returned proof |
| Snapshot and compaction | Yes | Yes | No | No | No |
| Backup, verify, restore | Yes | Yes | No | No | No |
| Complete doctor report | Recovery primitives | Yes | No | No | No |
| Start the HTTP server | Library | `serve` | N/A | N/A | N/A |

“Full AST” includes recursive filters, deterministic multi-field sorting,
cursors, grouping, and `count`/`sum`/`min`/`max`. Local query and retrieval
administration stay deliberately narrow; use the embedded API or `/v1` for
the complete public surface.

## Durable storage

- The append-only framed log and canonical logical snapshots are authority;
  Redb tables are reconstructible accelerators.
- Atomic UUID-addressed mutation batches cover KV records, immutable vector
  spaces, vector upsert/delete, and immutable lexical definitions.
- An exact transaction retry returns the original receipt. Reusing the UUID
  with different canonical operations is rejected.
- Disk format `2` snapshots preserve KV, vector spaces, vectors, lexical
  definitions, and receipts across recovery, compaction, backup, restore, and
  derived-index rebuild.
- Format-1 directories migrate atomically while exclusively locked. Older
  binaries reject format 2 before replay.
- Recovery truncates only an incomplete final frame. Complete checksum,
  digest-chain, manifest, snapshot, and format corruption fails closed.
- The packaged CLI and server use one finite default recovery policy across
  directory enumeration, snapshot restore, log scan, replay, and lexical
  rebuild. Embedded callers select it through `open_with_limits`; accepted
  appends then preserve the active segment's count/byte ceilings so a clean
  close cannot manufacture a policy-unreopenable log. Published embedded
  `open` methods retain their `0.2.0` compatibility policy.

The exact layouts are documented under
[durable formats](../README.md#durable-formats).

## Structured data and query

Records have nonempty binary keys and deterministic values: null, boolean,
signed 64-bit integer, UTF-8 string, bytes, array, or ordered object. Floating
point is deliberately absent. Query evaluates the complete logical dataset,
merges globally, then applies sort, cursor, and final limit. Binary key
ascending is always the final tie-breaker.

Filters support existence, same-type ordered comparison, prefix, contains,
recursive all/any, and negation. Aggregation runs over the complete filtered
set before pagination. Every budget or timeout failure returns no partial
logical result. See [data model](../concepts/data-model.md) and
[query semantics](../query/reference-semantics-v1.md).

## Retrieval without a provider dependency

Durable vector spaces use caller-supplied nonzero signed-Q15 vectors with a
fixed dimension. Exact retrieval computes canonical integer cosine scores in
nanos, orders by score descending then binary key ascending, and can abstain
for no candidates, insufficient score, or an ambiguous top margin.

Lexical retrieval uses pinned provider-free Unicode/tokenization and
deterministic BM25F-compatible scoring over configured document fields.
Hybrid retrieval fuses complete lexical and vector branches with deterministic
integer reciprocal-rank fusion and returns per-modality explanations. Branch
errors never silently become single-modality success.

Hyphae does not generate embeddings or call a model. See the
[exact](../retrieval/exact-reference-semantics-v2.md),
[lexical](../retrieval/lexical-reference-semantics-v1.md), and
[hybrid](../retrieval/hybrid-reference-semantics-v1.md) specifications.

## Verifiable results

Result get/query and exact/lexical/hybrid retrieval each have separate
canonical proof formats. Public server responses include proof bytes, proof
and caller-pinnable anchor digests, and the exact snapshot-witness reference.

`hyphae verify` reexecutes result proofs offline.
`hyphae verify-retrieval --kind exact|lexical|hybrid` validates a format-2
witness and reexecutes the complete retrieval semantics offline. A
self-consistent proof is not an external trust anchor; the expected anchor
must come from a trusted channel. See
[result proof v1](../provenance/result-proof-v1.md) and
[retrieval proof v1](../provenance/retrieval-proof-v1.md).

## Delivery surfaces

- `hyphae`: local engine operations, recovery tools, secure server, remote
  client, both offline verifiers, and MCP adapter in one executable.
- `hyphae-engine`: embeddable durable facade.
- `hyphae-storage`, `hyphae-query`, and `hyphae-retrieval`: lower-level public
  libraries containing durable and reference components.
- `hyphae-contracts`: typed v1 wire models and embedded OpenAPI/JSON Schemas.
- `hyphae-server` and `hyphae-client`: optional HTTP server and bounded Rust
  client.
- TypeScript and Python SDKs: bounded clients generated from public contracts.
- Astro, Next, Vite, and PliegoRS adapters: opt-in consumers outside the core.

## Default hard and service limits

The server publishes its v1-configurable values at `GET /v1/capabilities`.
The fixed 256 MiB server scanned-byte query ceiling is listed below but is not
a field in `ApiLimitsV1`: adding a required field to that strict v1 response
in a patch would break existing decoders. Embedded Rust callers select a byte
ceiling explicitly with `query_with_byte_limit`; legacy query methods have no
new byte default. Remote callers cannot configure or discover a different
server value in `0.2.1`.

| Limit | Default |
|---|---:|
| Binary key | 1 MiB |
| Canonical document | 16 MiB |
| Vector-space name / dimension | 128 bytes / 4,096 |
| JSON request body | 4 MiB |
| JSON depth / nodes | 64 / 100,000 |
| Atomic batch items | 1,000 |
| Query scanned / matched / returned | 1,000,000 / 100,000 / 1,000 |
| Server query aggregate scanned key + canonical document bytes (fixed, not v1-published) | 256 MiB |
| Aggregation groups | 10,000 |
| Filter nodes / depth | 256 / 64 |
| Sort / group / metric fields | 16 / 8 / 32 |
| Exact candidates / candidate bytes / returned | 100,000 / 256 MiB / 1,000 |
| Lexical documents / tokens / candidates / returned | 1,000,000 / 10,000,000 / 100,000 / 1,000 |
| Query / exact / lexical timeout | 30 seconds each |
| Hybrid total timeout | lexical timeout + exact-vector timeout (60 seconds by default) |
| Concurrent data operations | 16 |
| JSON response | 32 MiB |
| Encoded proof before base64 | 16 MiB |
| Downloadable witness | 512 MiB |

Both proof codecs accept at most 64 MiB; the default server transport policy
is stricter. Programmatic server users may reduce positive limits but cannot
raise canonical hard bounds. Hybrid uses the configured lexical final-result
bound and explicit request weights/limits. Its total deadline is the saturating
sum of the lexical and exact-vector timeouts: each branch also retains its own
ceiling, and fusion consumes the same total.

Offline verification has separate bounded defaults:

| Offline verifier bound | Default |
|---|---:|
| Snapshot witness file | 2 GiB |
| Aggregate decoded logical payload retained by the verifier | 1 GiB |
| Exact-vector replay candidate key/vector bytes | 1 GiB |

These offline bounds apply to local result and retrieval verification. They do
not raise the 512 MiB server/client witness-download default. They are the
default CLI and library policy, not immutable hard ceilings: embedded callers
may configure `SnapshotReadLimits` and verification limits. A larger value
expands the caller's resource-exposure envelope; applications handling
untrusted artifacts should prefer limits appropriate to their environment.
Decoded logical payload accounting includes KV keys/values, vector-space
names, vector space/key/Q15 payloads, and lexical-index definitions.
Proof readers require regular files, preflight their opened-file lengths, and
use bounded chunked reads. Standalone decoders are byte bounded but have no
timeout parameter; complete verification carries one deadline through proof
read, snapshot verification/loading, and replay. Exact replay preflights
candidate count and aggregate key/vector bytes before materialization.

`StorageLimits::default()`, used by the packaged CLI/server and available to
bounded embedded opens, defines:

| Recovery / maintenance bound | Default |
|---|---:|
| Shared timeout per bounded open, snapshot, or compaction | 60 seconds |
| Owned metadata directory entries | 1,000,000 |
| Active log bytes / complete frames | 2 GiB / 1,000,000 |
| Unique transactions / operation frames | 1,000,000 / 1,000,000 |
| Decoded operation payload bytes | 1 GiB |
| Snapshot file / retained decoded payload | 2 GiB / 1 GiB |
| Lexical rebuild documents / tokens | 1,000,000 / 10,000,000 |

`StorageLimits` configures embedded startup and the policy retained by the
writer; `MaintenanceLimits` configures one snapshot or compaction. The
packaged CLI/server choose the finite defaults, while published embedded
`open` methods retain their compatibility policy. Snapshot and compaction
reuse the stored policy unless an explicit `*_with_limits` policy is supplied.
HTTP proof creation further reduces snapshot file/decoded limits to the
configured downloadable witness budget and spends only the originating
request's remaining timeout. Direct witness admission and proof-bearing
response exhaustion return HTTP `413` with `result_too_large`; the OpenAPI
contract lists `413` on those routes.

The query byte ceiling is exact scanned-input accounting, not a same-sized
heap guarantee. Group keys, metric result names, sorting, and result
materialization remain governed by the documented count and shape limits.

TypeScript/Python clients preserve exact integers, reject invalid JSON, and
validate error-envelope/request-ID correlation. Their generated success models
are static types only; complete runtime success-shape validation is not a
capability of the `0.2.1` source packages.

## Deliberate non-capabilities in 0.2.1

Hyphae does not provide SQL, joins, floating-point documents, approximate
nearest-neighbor indexing, a distributed protocol, replication, clustering,
built-in TLS, encryption at rest, access-control roles, multitenancy, billing,
a hosted control plane, background daemon installation, an embedding model,
or an LLM. It is not Mycelium, Hyphae Network, Celiums Network, or a cognitive
runtime.

Application hosts own process supervision, TLS termination for remote access,
filesystem permissions, backup media encryption/retention, and any optional
embedding provider. Backup layout/manifest and snapshot-copy bytes are bounded,
but complete backup verification and restore do not expose one shared
end-to-end deadline. Restore still uses legacy verification/reopen/snapshot
paths, and filesystem calls or `sync_all` are not preemptible; operators must
provide an external timeout when required.

This section is a release-specific statement, not the forward product
constitution. The accepted
[native local ecosystem](../architecture/native-local-ecosystem.md) targets
Hyphae-owned relational, structure, and search engines in a later release
program while retaining offline operation and no mandatory model or external
database.
