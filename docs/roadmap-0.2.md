# Hyphae 0.2 execution roadmap

Status: historical `0.2.0` implementation record

Target: proof-bearing durable retrieval

This roadmap records the implementation work for Hyphae `0.2.0`. Gates 0
through 8 were completed locally with contracts, specifications, tests, and
evidence. The unchecked Gate 9 items remain unchecked because the repository
does not contain hosted closure evidence for the exact release commit. This
document is not current release authorization. The annotated `v0.2.0` tag
peels to `170380453a2ca6322a4c8bc50417318daee1c011`, and the matching
[GitHub release](https://github.com/Hyphae-Research-Foundation/hyphae/releases/tag/v0.2.0) is
published; those observable outcomes do not replace the missing retained
hosted receipts.

Maintenance release `0.2.1` is tracked separately in
[`gates/0.2.1.md`](gates/0.2.1.md).

The work starts from commit
`51ab2bc73ac6e93664bf455c5ebeb5993482baa9`. The repository audit and baseline
are recorded in [`gates/0.2-gate-0.md`](gates/0.2-gate-0.md).

## Objective

Hyphae `0.2` will add durable, deterministic, proof-bearing retrieval while
preserving the product boundary:

- one Rust binary and one data directory;
- offline operation by default;
- no required database, cache, cloud service, embedding provider, or LLM;
- exact retrieval before approximate retrieval;
- public, additive, versioned contracts;
- durable state derived from authoritative logical records;
- explicit abstention and no partial-success responses.

## Invariants

Every gate must preserve these rules:

1. The mutation log and logical snapshot are durable authority.
2. Redb tables and all retrieval indexes are reconstructible accelerators.
3. A successful write is visible immediately and survives restart, compaction,
   backup, restore, and index rebuild.
4. Existing disk format `1`, API `/v1`, and `result-proof-v1` inputs retain
   their current meaning.
5. New durable encodings and proof formats are versioned explicitly.
6. Ranking, tie-breaking, pagination, budgets, and abstention are
   deterministic and specified before implementation.
7. Batch mutations are atomic: all items commit or none commit.
8. No gate is complete without failure-path tests and recorded exit evidence.

## Non-goals

The following are excluded from `0.2` unless a later accepted ADR changes the
scope:

- HNSW or another approximate-nearest-neighbor index;
- disk ANN, sharding, replication, or multi-tenant collections;
- an embedding provider or model runtime;
- LLM integration;
- hosted SaaS, billing, or cloud operations;
- GPU acceleration;
- SQL or joins;
- copying code from a historical repository without an accepted porting-ledger
  entry.

## Gate order

| Gate | Outcome | State |
|---|---|---|
| 0 | Repository audit, baseline, dependency map, and risk register | Complete locally |
| 1 | Accepted ADRs and normative specifications | Complete locally |
| 2 | Durable named vector records and recovery | Complete locally |
| 3 | Exact durable vector retrieval | Complete locally |
| 4 | Offline-verifiable retrieval proofs | Complete locally |
| 5 | Additive HTTP contracts and server surface | Complete locally |
| 6 | Provider-free lexical retrieval | Complete locally |
| 7 | Deterministic hybrid retrieval | Complete locally |
| 8 | Equivalent SDK, CLI, and MCP surfaces | Complete locally |
| 9 | Benchmarks, compatibility evidence, and release closure | Complete locally; `v0.2.0` published; hosted closure not demonstrated by retained evidence |

Gates are sequential. Work may be explored in parallel, but no later gate may
be declared complete while an earlier exit criterion is red.

## Fixed work queue

These identifiers are stable planning anchors. Each task should land as one
reviewable change unless its acceptance evidence proves that a smaller split is
necessary.

| ID | Gate | Task | Depends on | Required artifact |
|---|---:|---|---|---|
| HYP-0201 | 1 | Decide durable vector ownership, disk-format evolution, migration, and old-binary behavior | Gate 0 | ADR-0013 plus format/migration spec |
| HYP-0202 | 1 | Benchmark canonical score representations and decide cross-platform numeric semantics | Gate 0 | Benchmark evidence, golden vectors, ADR-0015 |
| HYP-0203 | 1 | Specify exact durable retrieval, ordering, budgets, and abstention | HYP-0202 | ADR-0014 plus reference semantics |
| HYP-0204 | 1 | Specify retrieval proof payload and offline verification | HYP-0202, HYP-0203 | ADR-0016 plus proof-format spec |
| HYP-0205 | 1 | Specify provider-free lexical retrieval and pinned text semantics | Gate 0 | ADR-0017 plus lexical reference semantics |
| HYP-0206 | 1 | Specify deterministic hybrid fusion and explanations | HYP-0203, HYP-0205 | ADR-0018 plus hybrid reference semantics |
| HYP-0207 | 1 | Add contract sketches and cross-language golden vectors | HYP-0201 through HYP-0206 | Draft schemas, OpenAPI shapes, fixtures |
| HYP-0208 | 2 | Implement vector domain types, codecs, and atomic logical mutations | Gate 1 | Rust types, codecs, negative tests |
| HYP-0209 | 2 | Implement migration, snapshots, recovery, compaction, backup, restore, and rebuild | HYP-0208 | Durable implementation and compatibility fixture |
| HYP-0210 | 3 | Execute exact retrieval from consistent durable snapshots | HYP-0209 | Engine path and deterministic conformance |
| HYP-0211 | 4 | Implement retrieval proof generation and offline verification | HYP-0210 | Proof codec, verifier, tamper suite |
| HYP-0212 | 5 | Publish additive contracts and HTTP vector/retrieval routes | HYP-0210, HYP-0211 | Schemas, OpenAPI, server, error tests |
| HYP-0213 | 6 | Implement durable provider-free lexical retrieval | HYP-0205, HYP-0209 | Lexical index, rebuild and recovery tests |
| HYP-0214 | 7 | Implement deterministic hybrid fusion and proof coverage | HYP-0211, HYP-0213 | Hybrid engine path, fixtures, explanations |
| HYP-0215 | 8 | Extend Rust, TypeScript, Python, CLI, MCP, and adapters | HYP-0212, HYP-0214 | Generated clients and shared conformance |
| HYP-0216 | 9 | Close performance, compatibility, security, documentation, and release evidence | HYP-0215 | Exact-commit release gate |

HYP-0201 through HYP-0216 were implemented and gated locally. Publication was
outside the implementation queue and required separate authorization. The
later `v0.2.0` publication is recorded above; it does not close the missing
hosted evidence.

## Gate 0 — Audit and baseline

Exit evidence:

- [x] Read all repository instructions and the 0.2 handoff.
- [x] Inventory workspace crates, contracts, storage formats, clients,
  integrations, documentation, release automation, and compatibility
  fixtures.
- [x] Trace the current mutation, recovery, snapshot, backup, proof, and
  retrieval paths.
- [x] Run the local Rust, documentation, SDK, conformance, integration,
  packaging, compatibility, load, and soak gates.
- [x] Confirm the exact remote HEAD and its hosted CI, Stress, Fuzz, and
  Security results.
- [x] Record unresolved design decisions and implementation hazards.

See [`gates/0.2-gate-0.md`](gates/0.2-gate-0.md).

## Gate 1 — Decisions and specifications

No production implementation begins before this gate is accepted.

### ADR work

- [x] ADR-0013: durable named vector records and ownership boundaries.
- [x] ADR-0014: exact durable retrieval semantics.
- [x] ADR-0015: canonical numeric scoring and cross-platform ordering.
- [x] ADR-0016: retrieval proof format and offline verifier.
- [x] ADR-0017: provider-free lexical retrieval and text normalization.
- [x] ADR-0018: deterministic hybrid fusion and explainability.

### Normative specifications

- [x] Define vector-space identity, dimension, element representation, and
  metadata.
- [x] Define atomic put/delete vector batches and their idempotency behavior.
- [x] Decide disk-format evolution and the supported migration path from
  format `1`.
- [x] Define the logical snapshot representation required for backup and
  restore equivalence.
- [x] Define exact cosine scoring, invalid-input behavior, total ordering,
  budget accounting, and abstention.
- [x] Compare canonical score encodings with a portable microbenchmark before
  selecting one.
- [x] Define `retrieval-proof-v1` independently from `result-proof-v1`, unless
  ADR-0016 proves that another additive design is safer.
- [x] Define lexical tokenization, Unicode normalization/versioning, field
  weights, document frequency, length normalization, and ties.
- [x] Define reciprocal-rank fusion, candidate limits, duplicate handling,
  modality absence, and explanation fields.
- [x] Define additive JSON Schema and OpenAPI shapes without implementing
  server routes.

### Exit criteria

- [x] All six ADRs are accepted.
- [x] Every planned durable byte representation has a version and compatibility rule.
- [x] Golden vectors cover numeric ranking, ties, malformed values, and proof
  encoding.
- [x] The crate ownership plan introduces no dependency cycle.
- [x] Old binaries reject new data directories before replay rather than
  partially interpreting them.
- [x] The full existing gate remains green.

## Gate 2 — Durable vector records

### Storage

- [x] Add typed logical mutations for vector put/delete batches.
- [x] Add explicit codecs with corruption, truncation, size, and overflow
  checks.
- [x] Persist named vector spaces with fixed dimensions.
- [x] Add vector state to logical snapshots.
- [x] Rebuild vector indexes exclusively from authoritative log/snapshot
  state.
- [x] Preserve vector state across compaction, backup, restore, and reopen.
- [x] Extend idempotency receipts without weakening request-hash checks.

### Failure-path tests

- [x] Wrong dimension and non-finite element rejection.
- [x] Mixed-validity batch rollback.
- [x] Interrupted append and truncated-frame recovery.
- [x] Corrupt vector payload detection.
- [x] Compaction before/after crash.
- [x] Backup/restore equivalence.
- [x] Rebuild after deleting the derived index.
- [x] Migration from the immutable format-1 fixture.

### Exit criteria

- [x] Durable semantics pass restart and recovery tests.
- [x] No vector is authoritative only in Redb.
- [x] A new immutable compatibility fixture is checked in.
- [x] Storage limits are documented and enforced before allocation.

## Gate 3 — Exact durable retrieval

- [x] Read candidates from a consistent engine snapshot.
- [x] Execute the accepted exact-scoring reference algorithm.
- [x] Apply deterministic score and binary-key tie-breaking.
- [x] Enforce candidate, byte, dimension, and time budgets.
- [x] Return typed abstention instead of partial results.
- [x] Test immediate read-after-write and delete visibility.
- [x] Test results before/after restart, compaction, restore, and rebuild.
- [x] Keep the existing in-memory reference API compatible or deprecate it
  explicitly.

Exit requires conformance against golden vectors on supported platforms.

## Gate 4 — Retrieval proofs

- [x] Implement the accepted `retrieval-proof-v1` payload and codec.
- [x] Bind the proof to request, snapshot witness, ordered result set, score
  representation, and retrieval semantics version.
- [x] Add an offline verifier with no data-directory dependency.
- [x] Add tamper tests for query, score, rank, object key, vector space,
  witness, and semantics version.
- [x] Document what the proof establishes and what it does not establish.
- [x] Preserve `result-proof-v1` byte-for-byte and semantically.

Exit requires successful offline verification after the originating data
directory is unavailable.

## Gate 5 — Public HTTP surface

- [x] Add canonical JSON Schemas under `contracts/json-schema/`.
- [x] Mirror canonical contract assets into `hyphae-contracts` through a
  checked synchronization step.
- [x] Add OpenAPI paths for vector mutation and exact retrieval.
- [x] Generate Rust contract models and regenerate TypeScript/Python models.
- [x] Implement server routes only after schema checks pass.
- [x] Enforce authentication, body limits, batch limits, and stable errors.
- [x] Add negative conformance for atomicity, malformed scores, wrong
  dimensions, unknown spaces, budgets, and abstention.

All additions remain under `/v1`; breaking changes require a new major API
version.

## Gate 6 — Lexical retrieval

- [x] Persist only logical document/token statistics required by ADR-0017.
- [x] Build a reconstructible lexical index.
- [x] Implement the accepted BM25F-compatible reference semantics.
- [x] Specify and test Unicode behavior across supported platforms.
- [x] Prove deterministic ties and explicit abstention.
- [x] Cover restart, compaction, backup, restore, and index rebuild.

No semantic provider becomes a dependency of lexical retrieval.

## Gate 7 — Hybrid retrieval

- [x] Fuse exact-vector and lexical ranked lists using the accepted RRF
  semantics.
- [x] Keep modality candidate limits explicit.
- [x] Deduplicate by canonical object key.
- [x] Return per-modality ranks and fusion explanation.
- [x] Define behavior when one modality is absent or abstains.
- [x] Add deterministic golden vectors and proof coverage.

Hybrid execution must not silently downgrade to a different algorithm.

## Gate 8 — Client surfaces

- [x] Add Rust client methods from canonical contracts.
- [x] Regenerate TypeScript and Python models and add client methods.
- [x] Add CLI commands with machine-readable output.
- [x] Add MCP tools using the same public schemas.
- [x] Extend common positive and negative conformance suites.
- [x] Re-run Astro, Next, Vite, and optional PliegoRS boundary checks.

No client or adapter may depend on an internal engine or storage crate.

## Gate 9 — Evidence and release closure

- [x] Record exact-retrieval latency and memory across corpus sizes,
  dimensions, and `top_k` values.
- [x] Record write amplification, reopen, replay, compaction, rebuild,
  backup, and restore costs.
- [x] Record proof generation and verification overhead.
- [x] Add quality fixtures for vector, lexical, and hybrid retrieval.
- [ ] Run the complete Linux, macOS, and Windows hosted matrix on the exact
  release commit.
- [x] Run fuzz, security, dependency review, load, soak, packaging, SDK,
  conformance, documentation, and compatibility gates.
- [x] Update capabilities, limits, migration, backup, API, client, security,
  and release documentation.
- [x] Publish the annotated `v0.2.0` tag, GitHub release, and ten Rust crates.
- [ ] Retain commit-bound evidence proving that every required hosted result
  and published artifact points to the exact release commit.

The published `v0.2.0` outcome is recorded above. The unchecked Gate 9 items
remain an evidence gap in this historical record, not an assertion that no
release exists.

## Planned change slices

Changes should remain reviewable and reversible:

1. ADRs, specifications, golden vectors, and contract sketches.
2. Durable vector domain types and codecs.
3. Format migration, snapshot, recovery, compaction, and fixtures.
4. Exact engine retrieval and deterministic conformance.
5. Retrieval proof codec and offline verifier.
6. OpenAPI, schemas, server routes, and stable errors.
7. Lexical retrieval.
8. Hybrid fusion.
9. SDK, CLI, MCP, and integration conformance.
10. Performance evidence, documentation, and release closure.

Each slice must update its tests and documentation in the same change. A slice
must not combine an unresolved durable-format decision with production code.

## Start task

The first implementation task is Gate 1, change slice 1. It produces ADRs and
normative specifications only. The first decision to resolve is disk-format
evolution because mutation tags, log frames, snapshots, backup/restore, and old
binary behavior all depend on it.
