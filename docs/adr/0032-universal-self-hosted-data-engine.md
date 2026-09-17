<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# ADR-0032: one universal self-hosted data engine

- Status: accepted
- Date: 2026-09-17
- Scope: product identity, native object domains, acceleration, and clustering

## Context

Hyphae already owns one data directory, catalog, page/blob substrate, WAL,
MVCC commit sequence, scheduler, backup, and proof authority. Its public
surfaces nevertheless expose SQL, structures, lexical search, and vector
search as visibly separate families, while the accepted target architecture
describes capabilities that the current bounded release does not yet ship.

The product target is broader than the previously closed local phase. Hyphae
must become one self-hosted engine for workloads otherwise divided among
relational, keyspace, document, retrieval, time-series, graph, geospatial, and
analytical systems. It must retain a complete one-node offline form and also
support a strongly consistent self-hosted cluster.

The target does not authorize present-tense universal or superiority claims.
Every release remains constrained by its exact capability registry and gate
evidence.

## Decision

### One product identity

Hyphae is one data engine. Public vocabulary is organized around:

- catalogued objects;
- queries;
- transactions and snapshots;
- proofs;
- administration; and
- cluster operation.

Public documentation does not lead with a bundle of engines. Technical and
format specifications may name specialized execution domains because native
physical ownership remains a correctness requirement.

### First-class object domains

The Universal Engine program admits these native domains:

1. relations and PostgreSQL-affine SQL;
2. keyspaces and typed data structures;
3. documents and canonical JSON;
4. lexical, dense-vector, sparse-vector, and multivector retrieval;
5. time series;
6. graphs;
7. geospatial data; and
8. columnar analytical datasets.

Each domain owns suitable physical layouts and fast paths. No domain is
implemented as a mandatory projection through SQL, a generic KV tree, or a
third-party engine. Every domain participates in one catalog, type and entity
identity system, query algebra, transaction authority, resource governor,
backup, and proof model.

### Universal query and entity identity

Hyphae will define one catalog-bound and snapshot-bound query IR. It admits
native sources from every object domain and common projection, filter, join,
group, aggregation, order, limit, cancellation, explain, and proof semantics.
Direct point and model-specific operations remain available and do not traverse
the general optimizer when unnecessary.

A first-class `EntityId` joins logical data across relations, documents,
vectors, keyspaces, series, graph elements, and geospatial objects.
`CatalogObjectId`, `EntityId`, `RowId`, `TransactionId`, `SnapshotId`,
`ShardId`, and `NodeId` remain distinct types.

### PostgreSQL affinity

PostgreSQL is an external semantic oracle, not a runtime dependency. The first
compatibility target is familiar SQL semantics and tooling rather than the
PostgreSQL wire protocol. A frozen profile defines matching syntax, types,
values, ordering, errors, transaction behavior, and deliberate bounded
divergences. External PostgreSQL processes may run only in standalone
conformance and migration tooling.

### Retrieval parity

Qdrant and Weaviate are pinned external comparison subjects for the local
single-node data plane. The mandatory profile includes dense, sparse, named,
and multivectors; exact and ANN execution; durable quantization; nested,
array, and geo filters; hybrid and multistage ranking; grouped results; MMR;
partial updates; TTL; streaming ingestion; lifecycle maintenance; backup; and
observability.

Distributed or managed-cloud competitor features do not redefine this local
parity profile. Hyphae clustering has its own correctness and operations gates.

### Acceleration

Accelerator-capable artifacts discover devices in a bounded deterministic
order and automatically select a validated compatible GPU. The selected
device, driver, runtime, precision, kernels, model, and fallback status form a
versioned execution profile.

GPU failure, timeout, or memory exhaustion publishes no partial result. CPU
fallback reruns the complete operation only when the contract permits it.
Hyphae never silently mixes incompatible CPU and GPU vector profiles. The
portable CPU path remains available and the default product requires no GPU,
model, provider, or network.

### Strong self-hosted cluster

The cluster data plane is part of this repository. Hosted control planes,
billing, and SaaS operations are not.

A cluster uses a Hyphae-native authenticated binary protocol. Catalog, schema,
security, membership, and shard authority are replicated through consensus.
Data lives in replicated shard groups. Strong consistency is the default;
weaker reads or writes may exist only as explicit versioned operation modes.

Cross-shard transactions require durable preparation, a recoverable
coordinator, atomic commit/abort resolution, and snapshot identity. Network
partitions, leader loss, retries, rebalancing, rolling upgrades, and backup
must fail closed under their published guarantees.

Single-node operation is a one-node cluster semantically but preserves direct
embedded paths and performs no artificial network or quorum step.

### Evidence and claims

A capability is `shipped` only when its contract, implementation, failure-path
tests, public surfaces, limits, exact release SHA, and retained evidence agree.
Other states are `experimental`, `planned`, `blocked`, or `out-of-scope`.

Semantic parity means every mandatory case in a frozen profile passes without
an unwaived mismatch. Performance comparison requires matched hardware,
transport, durability, data, recall/quality, concurrency, and configuration.
Best-in-class wording requires preregistered superiority criteria, no missing
mandatory capability, no hidden critical regression, and reproduction on a
second host.

## Consequences

- The previous local phase remains valid evidence for its bounded releases but
  no longer defines the complete forward product boundary.
- ADR-0020 remains authoritative for native ownership and direct same-node
  composition. Its three-domain phase boundary is superseded by this ADR.
- ADR-0022 remains authoritative for separating hosted control planes and SaaS
  concerns. Its exclusion of a self-hosted cluster data plane is superseded.
- Existing G0-G8 closures do not close any Universal Engine, GPU, distributed,
  or competitor-parity gate.
- New durable formats, consensus records, accelerator backends, and unsafe FFI
  require narrower ADRs and independent evidence.
- Roadmap breadth may be prototyped in parallel, but release claims remain
  ordered by gate dependencies.

## Rejected alternatives

### Market several colocated engines as one

Rejected. Shared packaging without one catalog/query/transaction authority
would preserve the user and operational boundaries Hyphae exists to remove.

### Collapse every model into SQL or a generic KV projection

Rejected. It would discard native semantics, predictable fast paths, and
specialized physical representations.

### Embed PostgreSQL, Valkey, Qdrant, Weaviate, or another engine

Rejected. It would introduce another runtime authority and violate autonomous
offline operation.

### Claim universality before profile closure

Rejected. A target architecture is not release evidence.

### Require a GPU or model

Rejected. Acceleration is automatic when present and validated, not a
prerequisite for correctness or offline operation.

## Program authority

The ordered implementation and evidence authority is the
[Universal Engine program](../roadmaps/universal-engine-program.md).
