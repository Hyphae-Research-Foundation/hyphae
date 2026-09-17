<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Universal Engine program

Status: active forward program, accepted 2026-09-17. Governing decision:
[ADR-0032](../adr/0032-universal-self-hosted-data-engine.md).

This document is the single forward roadmap authority. Older roadmaps remain
historical design and release evidence; they cannot independently promote a
current capability or claim.

## Product outcome

Hyphae becomes one self-hosted engine with one catalog, entity model, query
algebra, transaction/snapshot authority, storage substrate, resource governor,
backup, proof, and cluster operating model across relational, keyspace,
document, retrieval, time-series, graph, geospatial, and columnar objects.

The complete program supports a strongly consistent cluster while retaining a
complete offline one-node product. PostgreSQL, Valkey, Qdrant, and Weaviate are
external conformance, migration, and measurement subjects.

## Capability states

Every capability registry row uses exactly one state:

- `shipped`: exact release contract and retained evidence exist;
- `experimental`: executable source exists without release authority;
- `planned`: accepted contract/program work has not landed;
- `blocked`: a named prerequisite or external constraint prevents progress;
- `out-of-scope`: the governing ADR explicitly excludes it.

No roadmap prose changes a shipped state.

## Ordered gates

### UE-0: cloud development authority

Outcome: development, testing, acceleration, and evidence run from a
reproducible AWS GPU workstation rather than a developer laptop.

Exit evidence:

- SSM-only access with no ingress and IMDSv2 required;
- encrypted persistent workspace storage and separate ephemeral scratch;
- pinned Rust, Node, Python, CUDA, driver, AMI, and host identity;
- a real CUDA canary and NVIDIA container runtime;
- complete workspace, SDK, documentation, and packaging checks;
- reproducible environment receipt retained outside the source tree; and
- an H100 qualification lane when reserved capacity is active.

### UE-1: exact current baseline

Outcome: the current source has one truthful capability inventory and no open
critical authority defect.

Required work:

- isolate and merge the protocol allocation and structure no-op correction;
- fix keyspace physical isolation with an explicit complete-root migration;
- introduce durable-prefix certification for WAL/page/blob authority;
- fix transaction identity reuse and complete-document vector replacement;
- close unsafe recovery path handling and uncertain-publication poisoning;
- reconcile release tag/schema/workflow authority; and
- repeat all affected crash, corruption, backup, proof, and release checks.

### UE-2: universal object and identity model

Outcome: every domain is a catalogued object with engine-allocated identity,
qualified name, schema, dependencies, authorization, and physical binding.

Required types include `CatalogObjectId`, `EntityId`, `RowId`, `TransactionId`,
`SnapshotId`, `ShardId`, and `NodeId`. Public calls use names or logical IDs;
physical index identities are administrative evidence.

### UE-3: universal query and transaction kernel

Outcome: one catalog-bound, snapshot-bound query IR composes every object
domain through typed direct calls and one resource envelope.

Exit scenario:

1. create one object from every admitted domain;
2. mutate them in one transaction;
3. read private changes through a transaction-bound query;
4. commit once;
5. query across domains by `EntityId`;
6. return one snapshot and explanation;
7. generate and verify an offline proof; and
8. recover, back up, restore, and reproduce the same result.

### UE-4A: PostgreSQL semantic and tooling profile

Order:

1. remove semantic traps: `INT8`, quoted identifiers, `$n`, comments, inserts
   without column lists, conventional `LIMIT`, null ordering, source spans,
   and SQLSTATE mapping;
2. add typed expressions, casts, common functions, `CASE`, `COALESCE`,
   defaults, `RETURNING`, and `ON CONFLICT`;
3. add general DML, schema evolution, joins, subqueries, set operations, CTEs,
   windows, SQL transaction commands, and savepoints;
4. add statistics, cost planning, governed sort/spill, and server cursors; and
5. ship a SQL shell, DB-API 2.0, SQLAlchemy dialect, typed TypeScript query API,
   schema inspection, and migration tooling.

The external PostgreSQL oracle is pinned and never enters the runtime graph.

### UE-4B: universal native domains

Required domain profiles:

- canonical nested/array document and JSON operations;
- time-series retention, compression, downsampling, and late-arrival policy;
- graph vertices, edges, adjacency indexes, and bounded traversals;
- geospatial values, indexes, distance predicates, and joins; and
- immutable column batches, vectorized analytics, compression, statistics,
  and governed spill.

Each profile includes transactions, query IR, backup, cluster placement,
proofs, SDKs, limits, and failure tests.

### UE-4C: Qdrant/Weaviate local data-plane profile

Mandatory capabilities include dense, sparse, named, and multivectors;
MaxSim/late interaction; exact and ANN execution; durable quantization and
rescoring; nested, array, geo, and text filters; hybrid and multistage query;
formula scoring; MMR; grouping; facets and metrics; partial updates; document
TTL; streaming ingestion; automatic index maintenance; profiling; and
Prometheus/OpenTelemetry export.

The comparison matrix uses pinned single-node Qdrant and Weaviate releases on
identical hardware, data, durability, filters, vectors, quality floor, and
concurrency. Distributed/cloud features are evaluated by Hyphae cluster gates,
not smuggled into the local parity claim.

### UE-4D: automatic heterogeneous acceleration

Order:

1. accelerator capability and execution-profile contracts;
2. CUDA embedding and cross-encoder batching;
3. VRAM, pinned-memory, stream, and cancellation governance;
4. ANN build and candidate generation;
5. exact distance and quantization kernels;
6. device-loss and complete CPU fallback semantics;
7. L40S and H100 qualification;
8. AMD ROCm and Apple Metal qualification; and
9. Vulkan/WGPU portability.

No accelerated result becomes authority without the declared equivalence or
verification step. Backend/profile changes never silently mix durable vectors.

### UE-4E: strong self-hosted cluster

Required layers:

- node identity, membership, mTLS, and native cluster protocol;
- consensus-replicated catalog, schema, security, and topology;
- deterministic sharding and replicated shard groups;
- leader election, quorum writes, and explicit read consistency;
- distributed snapshots and transaction identities;
- durable prepare and recoverable cross-shard commit;
- network-partition, clock-skew, retry, and coordinator-loss behavior;
- rebalancing, shard movement, repair, and rolling upgrades; and
- cluster-consistent backup, restore, and PITR.

A one-node deployment executes the same logical contracts without an
artificial network or quorum path.

### UE-5: unified product experience

Primary CLI nouns become `object`, `query`, `keyspace`, `collection`,
`timeseries`, `graph`, `geo`, `analytics`, `transaction`, `proof`, `cluster`,
and `admin`. Format-2 compatibility moves under an explicit `legacy` namespace
with a versioned deprecation policy.

Rust, Python, TypeScript, and later SDKs expose object handles and the same
query/transaction model. Users do not supply physical object, analyzer, or
index IDs for ordinary creation and use.

### UE-6: migration and operations

Versioned importers cover PostgreSQL, Valkey, Qdrant, Weaviate, document,
time-series, and graph sources. Every construct is classified as `exact`,
`equivalent`, `transformed`, `degraded`, or `rejected`, with a sealed receipt.

Operational closure includes online and incremental backup, PITR, schema
migration, capacity planning, slow-query and GPU profiling, cluster telemetry,
and an administrative interface.

### UE-7: converged-stack evidence

The decisive benchmark compares one Hyphae deployment against an application
stack using PostgreSQL plus Valkey plus Qdrant or Weaviate. The workload
mutates all domains, generates embeddings, queries a consistent joined view,
injects commit and node failures, and performs backup/restore.

It reports consistency violations, visibility delay, latency distributions,
throughput, total RSS/VRAM/disk, write amplification, processes, ports,
operator steps, RPO, RTO, and cost. Isolated component results remain visible
and no composite score hides a failed guardrail.

### UE-8: release and claim authorization

A Universal Engine release requires exact-SHA contracts, all applicable
platform and accelerator receipts, cluster fault evidence, migration evidence,
SBOMs, signatures, provenance, and a repeated G8 release closure. Historical
3.0 evidence is never inherited by changed source.

Best-in-class wording is authorized only after every mandatory profile passes,
preregistered superiority criteria hold without a critical guardrail loss,
and a second host reproduces the result.

## Active execution order

1. close UE-0 on the AWS GPU development authority;
2. merge the isolated corrective PR;
3. land this charter and create the capability registry;
4. close keyspace isolation and durable-prefix authority;
5. freeze universal identity and query contracts;
6. start PostgreSQL, native-domain, retrieval, acceleration, and cluster tracks
   in parallel after their shared dependencies are stable; and
7. publish no expanded claim until its exact gate closes.
