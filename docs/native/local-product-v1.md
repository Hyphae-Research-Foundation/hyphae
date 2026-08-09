# Native local product contract v1

Status: implemented; G6 closed for this bounded contract

This contract defines the bounded Native 1.0 product exposed by G6. It does not
describe shipped `0.2.1` behavior. G6 closure is recorded by the native gate
status and its retained exact-SHA evidence.

## One authority

One product instance owns one `NativeDatabase`, one native data directory, one
catalog snapshot sequence, one MVCC root-set sequence, and one scheduler. The
embedded API, local daemon, CLI, HTTP `/v2`, and SDKs are adapters over that
instance. No adapter maintains a second logical database or indexing path.

Engine-to-engine execution uses direct typed Rust calls. TCP, HTTP, JSON,
GraphQL, gRPC, RESP, PostgreSQL wire, OpenSearch REST, and the native local
protocol are forbidden internal engine paths.

## Required surfaces

### Embedded Rust

The curated facade owns configuration, limits, logical time, durability,
deadlines, cancellation, telemetry, and administration policy. Runtime page,
WAL, root, crash-injection, and private physical types are not stable public
API merely because they are currently exported by an experimental crate.

### Native local protocol

The native protocol runs over UDS on Unix and named pipes on Windows. G6
requires a multi-client daemon, version/capability negotiation, database and
schema selection, catalog version, prepared handles, bounded row streaming,
flow control, deadlines, cancellation, typed errors, peer identity, graceful
shutdown, and explicit transaction outcome after disconnect. Read streams are
provisional until one mandatory completion frame; clients discard a stream
that terminates without successful completion. Mutations remain atomically
all-or-none and never stream partial success.

### CLI

The single `hyphae` executable exposes local operations and starts no listener
unless explicitly asked to serve. There is no `--native` mode and no mixed
format-2/native authority.

### HTTP `/v2`

HTTP `/v2` is loopback-first and calls the facade directly. It has bounded
bodies and responses, authentication, request IDs, stable errors, deadlines,
streaming for large bounded operations, atomic mutations, and provisional read
streams that require successful completion.

HTTP `/v1` may be retained only for operations with an exact native mapping.
An unmappable request fails explicitly. `/v1` cannot open or mutate format-2
state after native cutover.

### SDKs

Rust, Python, and TypeScript provide equivalent high-level operations over the
native local protocol and HTTP `/v2`. They expose typed errors, deadlines,
cancellation, transaction state, proofs, catalog identities, and explain
results without requiring callers to construct frames or transport JSON.

## Admitted capability families

The G6 profile exposes the closed bounded G2-G5 behavior through every
applicable surface:

- catalog list, describe, resolve, capabilities, and version;
- bounded SQL DDL, DML, queries, prepared plans, transactions, and `EXPLAIN`;
- native strings, counters, hashes, lists, sets, sorted sets, streams, TTL,
  scans, algebra, and atomic batches;
- catalogued search collections with fields, analyzers, stored fields, doc
  values, filters, sorting, facets, metric aggregations, exact vectors,
  incremental ANN, named vectors, and same-snapshot hybrid fusion;
- all-engine transactions and stable-ID relation-valued sources;
- administration, telemetry, doctor, backup, restore, and proof verification.

Every request has explicit count, byte, depth, work, memory, and deadline
limits appropriate to its shape. Budget exhaustion publishes no partial
mutation and cannot complete a provisional read stream successfully.
The labelled partial aggregation mode admitted by the lower-level search
contract is not part of the G6 public product profile.

## Competitive search lifecycle

One vector mutation must not rebuild the complete graph. The admitted ANN
lifecycle uses incremental deltas or equivalent bounded mutations, durable
tombstones, interruption-safe consolidation, atomic generation publication,
snapshot-safe reclamation, and exact-oracle checks.

Structured filtering is integrated before or during ANN candidate selection.
Post-filter-only behavior may remain an explicitly requested compatibility
strategy, but it cannot be the default competitive path. The planner may
select exact search for a sufficiently small eligible set. Receipts report the
eligible cardinality, selected strategy, visited candidates, rerank count, and
approximation status.

## Product invariants

- A committed all-engine write has one visible CSN on every surface.
- Readers never combine roots from different generations.
- Catalog names resolve to the same stable IDs on every surface.
- Result ordering and tie-breaking are canonical and transport-independent.
- One public error registry maps every externally reachable failure.
- Approximate execution is always labelled and never presented as exact.
- Ordinary operation does not require a database, cache, cloud account,
  embedding provider, reranker, generative model, or LLM.
- Optional model adapters consume public contracts and never become storage
  authority.

## Non-claims

G6 does not claim production-scale performance, online or incremental backup,
format-2 migration, release authorization, universal SQL, distributed
transactions, clustering, replication, shared-kernel multitenancy, hosted
control planes, or universal superiority over another product.

## Verification

The G6 suite manifest binds each requirement to executable tests. Cross-surface
conformance compares IDs, CSN, canonical results, errors, explain output, and
proof verification. Linux, macOS, and Windows functional lanes are required;
G7 separately owns stable-hardware thresholds.
