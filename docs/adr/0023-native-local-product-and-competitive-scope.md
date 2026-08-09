# ADR-0023: Native local product and competitive scope

- Status: Accepted
- Date: 2026-08-06
- Owners: Celiums Solutions LLC

## Context

G0 through G5 establish bounded native relational, structure, search, and
cross-engine convergence behavior. The native runtime is not yet the default
product, and the one-line G6 outcome does not decide the public facade, HTTP
version, required SDKs, proof surface, or how Hyphae competes with established
vector-search products.

Attempting immediate feature parity with a distributed cloud database would
mix local product closure with clustering, replication, multitenancy, hosted
operations, and model-provider concerns. Conversely, a minimal wrapper around
the existing runtime would not provide a credible local search product.

## Decision

Hyphae Native 1.0 is local-first. It competes first on one-process embedded and
same-host workloads through:

- one binary and one exclusively owned native data directory;
- one catalog, WAL, MVCC root set, CSN, scheduler, error model, and proof model;
- atomic SQL, structure, lexical, exact-vector, ANN, and hybrid operations;
- immediate same-CSN visibility without an independent search refresh cycle;
- deterministic bounded execution and offline-verifiable native proofs; and
- direct embedded execution plus a compact native same-host protocol.

G6 is a competitive local-product gate, not a thin delivery-surface gate. Its
admitted search surface includes catalogued collections, BM25, exact vector,
incremental ANN lifecycle, same-snapshot hybrid fusion, persistent typed
filters, sort, facets, metric aggregations, and named vectors. Unsupported
shapes fail closed.

The public surfaces are:

1. a curated embedded Rust facade, which is the primary execution surface;
2. the native binary protocol over Unix domain sockets and Windows named
   pipes;
3. the single `hyphae` CLI and local daemon;
4. an additive native HTTP `/v2` edge adapter; and
5. Rust, Python, and TypeScript SDKs with equivalent local-protocol and HTTP
   transports.

HTTP `/v2` calls the same in-process facade and is never an engine-to-engine
path. HTTP `/v1` may survive temporarily only as a compatibility adapter over
the native facade. It cannot open format-2 state, create a second authority, or
silently change a request whose semantics cannot be mapped exactly.

This decision supersedes ADR-0021 only where that ADR deferred the `/v1`
survival decision until after G6. G6 now defines and tests the compatibility
policy; ADR-0021 continues to govern format-2 authority and G8 migration.

Native point, bounded SQL, lexical, exact-vector, ANN, hybrid, filtered, and
catalog results have versioned proof forms. Approximate proofs identify the
index generation, parameters, candidate process, and approximation status;
they do not misrepresent ANN output as an exact nearest-neighbor proof.

Embedding, reranking, and generative-model providers remain optional adapters
that consume public versioned contracts. The default product remains offline
and provider-free. Go, Java, and C# SDKs, shared-kernel multitenancy,
replication, sharding, clustering, hosted control planes, and SaaS operations
are post-G8 programs.

Closing G8 will establish readiness for this bounded local contract. It will
not by itself establish universal superiority over a distributed vector
database. Comparative claims require matched-hardware, matched-recall,
matched-durability, public reproducible benchmarks.

## Consequences

- G6 includes substantial product and search-lifecycle implementation, not
  only adapters around G2 through G5.
- A stable contract layer must be independent of Rust internals and transport.
- All surfaces must report the same catalog identities, CSN, result ordering,
  errors, explanations, and proof semantics.
- ANN mutation and filtered search must become operationally credible before
  G6 closes; production-scale thresholds remain G7 evidence.
- The existing format-2 product remains the published compatibility baseline
  until the G8 offline migration and release evidence close.
- Adding HTTP `/v2` increases conformance and security work but does not alter
  the primary embedded/local performance boundary.

## Alternatives considered

### Immediate distributed parity

Rejected for Native 1.0 because replication, consensus, sharding, and dense
multitenancy would obscure the local authority and delay measurable product
closure. They remain required for a later distributed competitive program.

### Minimal G6 wrappers

Rejected because exposing rebuild-per-mutation ANN, post-filter-only search,
and disconnected search components would not produce a competitive local
product.

### Extend HTTP `/v1` in place

Rejected because `/v1` is bound to the format-2 product and lacks native SQL,
structure, transaction, catalog, and approximation semantics. `/v2` provides a
clean contract while an exact compatibility adapter can be evaluated
separately.

### Make model providers part of the core

Rejected because mandatory models violate offline operation and make external
provider behavior part of storage correctness.

## Verification

G6 closes only with the exact-SHA profile and evidence defined by
[`native-g6-roadmap.md`](../roadmaps/native-g6-roadmap.md). The hosted lane must
prove cross-surface equivalence, native proofs, UDS and named-pipe operation,
HTTP `/v2`, all three required SDKs, common errors, administration, telemetry,
doctor, backup/restore, and the absence of serialized internal engine paths.

G7 separately proves stable-hardware performance. G8 separately proves
migration, failure, packaging, signing, and release readiness.
