# ADR-0022: Cloud-ready local primitives

- Status: Accepted
- Date: 2026-08-02
- Owners: Celiums Solutions LLC

## Context

The accepted business direction includes a future hosted program, working
name Hyphae Cloud, that will compete with managed data platforms. That
program lives outside this repository: the product boundary keeps hosted
SaaS concerns, billing, and cloud operations external, and phase 1 ends
with the complete single-process local ecosystem (ADR-0020). Future cloud
programs may consume only public versioned contracts.

The risk motivating this decision is narrower than cloud scope. The v1
format contracts phase 1 is freezing (WAL, retention anchors, root
manifests, snapshots, receipts) are being designed without naming the
properties a hosted program will need, and retrofitting those properties
after the formats freeze is expensive and compatibility-breaking.

The existing primitives already point in the right direction. The WAL is
digest-chained per block with absolute LSN identity. `HYWAR001` retention
anchors carry a monotonically increasing anchor epoch and bind one exact
manifest generation and digest. Root manifests are digest-chained and
carry reserved zero fields. ADR-0019 defines finite budgets and deadlines
for every bounded operation. Commit and maintenance receipts separate
admission, queue, execution, page-synchronization, and WAL-synchronization
clocks. What is missing is declaring these facts as protected contractual
properties instead of implementation accidents.

## Decision

Phase-1 local contracts adopt the following normative commitments. None of
them authorizes a network feature, a hosted API, or a cloud dependency in
this repository.

1. Lineage identity. Every native data directory has a stable lineage
   identity: a directory identifier plus a history epoch, recorded in its
   format marker and threaded through the manifest and anchor chain, so
   two divergent histories of the same origin are distinguishable offline.
   The existing reserved fields in manifests and anchors are the preferred
   vehicle; any concrete use requires updating the corresponding versioned
   contract, never a silent change.

2. The WAL is the replicable authority. The per-block digest chain, the
   absolute LSN identity, and the self-verifying retention anchors are
   protected contractual properties of WAL v1. No future optimization may
   break the property that a committed WAL suffix plus its base anchor
   reconstructs state and proves its lineage. A future change-subscription
   cursor (changefeed) is defined as a versioned local API over committed
   WAL suffixes; its remote transport is outside this repository.

3. Portable snapshots. The canonical snapshot format remains portable,
   digest-anchored, and offline-verifiable, so snapshot shipping (copying
   a snapshot to another machine and opening and verifying it there) never
   requires a new format. The existing atomic backup and restore path is
   the base.

4. Metering-ready receipts. Commit, latency, and maintenance receipts keep
   separate clocks for admission, queueing, execution, page
   synchronization, and WAL synchronization, plus scanned byte and row
   counters, as a versioned schema consumable by external programs through
   public contracts. Billing, aggregation, and multi-tenant attribution
   stay outside this repository.

5. The hosted isolation unit is the complete engine. One tenant equals one
   process plus one data directory. Phase 1 introduces no shared kernel
   multitenancy: no tenant namespaces, no per-tenant encryption in the
   kernel, and no shared quotas. This preserves the local security model
   and makes cold start and isolation an external control-plane problem.

6. Explicit boundary. Control plane, billing, replication transport,
   hosted APIs, TLS, and orchestration remain outside this repository.
   Future cloud programs consume exclusively public versioned contracts:
   on-disk formats, receipts, snapshots, the local changefeed, and `/v1`.
   This decision authorizes no new network feature in phase 1.

## Consequences

- The v1 WAL, manifest, anchor, snapshot, and `FORMAT` contracts gain
  additional identity and stability requirements before they freeze.
- Reviewing those formats now is cheap; migrating frozen formats after a
  hosted program depends on them is not.
- The repository gains a durable design criterion, "does this decision
  break a protected property?", without gaining cloud scope.
- The one-tenant-one-engine model deliberately gives up tenant density in
  exchange for simplicity and isolation; that trade is revisable only by a
  future ADR.

## Alternatives considered

### Introduce kernel multitenancy now

Rejected. Tenant namespaces, per-tenant encryption, and shared quotas add
complexity and security surface phase 1 cannot pay, and they contradict
the single-process phase-1 boundary fixed by ADR-0020.

### Build remote replication and changefeed transport now

Rejected. It inverts the ordered gate program and violates the repository
boundary that keeps cloud operations external.

### Ignore the hosted future until after G8

Rejected. Format decisions freeze during phase 1. Retrofitting lineage
identity or anchor stability into frozen formats afterward would break
compatibility.

### Document the cloud boundary only in product documentation

Rejected. Without a durable decision, any milestone can erode the
protected properties one convenient change at a time.

## Verification

- The versioned contracts in `docs/native/` must declare these properties
  explicitly: [WAL format v1](../native/wal-format-v1.md),
  [WAL retention v1](../native/wal-retention-v1.md),
  [root manifest and checkpoint v1](../native/root-manifest-checkpoint-v1.md),
  and the future `FORMAT` marker contract decided by
  [ADR-0021](0021-native-cutover-and-format-evolution.md).
- Lineage-identity round-trip tests are required when the identity fields
  are implemented: two histories diverged from one origin must be
  distinguishable offline through the recorded identity and chains.
- The documentation checker and the existing format golden tests act as
  stability guards for the declared properties.
- Any change to a protected property named by this decision requires a
  review of this ADR, not a local contract edit.
