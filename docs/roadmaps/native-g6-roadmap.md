# Native G6 local-product roadmap

Status: accepted execution plan; implementation in progress

This roadmap turns the closed G2-G5 engine profiles into one competitive local
product. [ADR-0023](../adr/0023-native-local-product-and-competitive-scope.md)
fixes the positioning and public-surface decisions. The current gate state is
maintained in [native gate status](../gates/native-gate-status.md).

The complete retained G0-G5 prefix is a required predecessor. G2-G5 supply the
engine behavior reused by G6; G0-G1 remain binding constitutional and substrate
authority.

## Product outcome

One `hyphae` binary and one native data directory expose the same catalogued
state through embedded Rust, the native local protocol, CLI, HTTP `/v2`, and
Rust/Python/TypeScript SDKs. SQL, structures, lexical search, exact vector,
ANN, and hybrid search execute directly against one native facade and share
one commit sequence, scheduler, error model, telemetry model, and proof model.

G6 does not claim production-scale performance, format-2 migration, release
authorization, clustering, replication, shared-kernel multitenancy, or hosted
operation. Those boundaries belong to G7, G8, or later programs.

## Ordered work queue

| ID | Requirement | Deliverable | Depends on |
|---|---|---|---|
| G6-001 | Contract authority | ADR, product contract, 14-row profile, manifests, validators, hosted open-state lane | G0-G5 closure |
| G6-002 | Shared contracts and errors | Transport-independent values, requests, results, stable error codes and retry classes | G6-001 |
| G6-003 | Catalog and collection model | Bounded list/describe/resolve, catalogued keyspaces, search fields, links, named vectors and dependencies | G6-002 |
| G6-004 | Competitive search surface | Persistent doc values, filters, sort, facets, metrics, BM25, exact/ANN/hybrid and multi-target vectors | G6-003 |
| G6-005 | Incremental ANN lifecycle | Incremental upsert/delete, deltas, consolidation, recovery and generation reclamation | G6-003 |
| G6-006 | Embedded product facade | Curated Rust API over the existing G2-G5 runtime, limits, deadlines, cancellation and durability | G6-002 through G6-005 |
| G6-007 | Native proofs | Versioned point, SQL, search, ANN/hybrid and catalog proofs plus offline verification | G6-006 |
| G6-008 | Local daemon | Multi-client UDS and Windows named pipes, handshake, flow control, cancellation and typed errors | G6-006 |
| G6-009 | HTTP `/v2` | Loopback-first native API over the facade, bounded streaming, authentication and `/v1` compatibility decision | G6-006, G6-007 |
| G6-010 | Single-binary CLI | Native SQL, structures, search, explain, service, admin, doctor, backup, restore and proof verification | G6-006 through G6-009 |
| G6-011 | Required SDKs | Rust, Python and TypeScript high-level clients over local protocol and HTTP `/v2` | G6-008, G6-009 |
| G6-012 | Administration and explain | Typed maintenance service, authorization, progress and stable physical explanations | G6-006 |
| G6-013 | Telemetry and doctor | Bounded telemetry registry, redaction, recovery report and corruption matrix | G6-006, G6-012 |
| G6-014 | Backup/restore surface | Existing G5 native backup exposed consistently with progress, verification and doctor-after-restore | G6-006, G6-010 |
| G6-015 | Cross-surface conformance | Same IDs, CSN, values, ordering, explain, errors and proofs on every required surface/platform | G6-007 through G6-014 |
| G6-016 | Exact-SHA closure | Hosted receipts, retained predecessor audit and reviewed closure summary | G6-015 |

## Competitive search acceptance

G6 must not close with a graph rebuilt for every vector mutation or with ANN
post-filtering as the only structured-filter strategy. The functional gate
requires:

- incremental vector upsert and delete without full-graph rebuild per change;
- interruption-safe background consolidation and atomic generation switch;
- bounded reclamation of obsolete graph generations;
- persistent typed filter indexes and filter-aware ANN traversal;
- an exact filtered oracle and explicit strategy/recall receipts;
- adaptive exact versus ANN selection for restrictive candidate sets;
- one collection model for lexical fields, doc values, stored fields, and one
  or more named vector definitions;
- bounded batch and streaming ingestion with backpressure, idempotency, and no
  partial mutation success; read streams remain provisional until their
  mandatory completion trailer; and
- same-snapshot BM25, exact, ANN, hybrid, sort, facet, and metric results.

G7 supplies the production-scale latency, throughput, memory, and recall
thresholds. G6 supplies complete bounded behavior and failure-path evidence.

## Cross-surface matrix

The same corpus must execute through:

- embedded Rust;
- UDS on Linux and macOS;
- named pipes on Windows;
- local CLI;
- HTTP `/v2`;
- Rust SDK over local protocol and HTTP;
- Python SDK over local protocol and HTTP; and
- TypeScript SDK over local protocol and HTTP.

Every admitted operation must preserve catalog object IDs, catalog version,
visible CSN, canonical values, result order, error code and retry class,
explanation strategy, and proof verification result. Deadline, cancellation,
limit, malformed-input, disconnect, uncertain-commit, backpressure, and
authorization failures publish no partial mutation and cannot complete a
provisional read stream successfully.

## Proof outcome

Native proof payloads bind the canonical request, catalog version, root-set
identity, visible CSN, ordered result, execution semantics, and proof version.
Exact results are reexecuted or otherwise verified against retained authority.
Approximate results additionally bind graph generation, metric, search breadth,
candidate and rerank policy, and approximation status. The verifier operates
offline after the originating directory is unavailable.

## Product operations

The one binary provides native commands for SQL, prepared plans, catalog,
structures, search, vectors, hybrid, transactions, explain, daemon service,
status, telemetry, doctor, checkpoint, compaction, vacuum, backup, restore,
and proof verification. No `--native` mode exists. A format-2 directory is not
silently opened by the native authority; G8 owns the offline importer and
promotion evidence.

## G6 exit evidence

The machine-readable profile contains these fourteen ordered requirements:

1. `shared-contracts-and-errors`;
2. `catalog-and-collection-model`;
3. `embedded-product-facade`;
4. `competitive-search-surface`;
5. `incremental-ann-lifecycle`;
6. `native-offline-proofs`;
7. `native-local-daemon`;
8. `native-http-v2`;
9. `single-binary-cli`;
10. `rust-python-typescript-sdks`;
11. `administration-and-explain`;
12. `telemetry-and-doctor`;
13. `backup-restore-product-surface`; and
14. `cross-surface-conformance`.

All fourteen need hosted exact-SHA receipts. The G0-G5 retained closure files
are digest-bound predecessors. Candidate workflow artifacts remain supporting
evidence and cannot declare closure. A later reviewed commit retains the
aggregate and moves G6 to `closed`.

## After G8

Beating distributed vector platforms across their full scope requires later,
separately gated programs for vector quantization and disk-aware ANN,
10M/100M-vector scale, sharding, replication, consensus, repair, failover,
rolling upgrades, dense tenant lifecycle, Go/Java/C# SDKs, connectors, and
optional model adapters. Comparative claims require matched hardware, recall,
durability, concurrency, filter selectivity, quality, memory, and cost.
