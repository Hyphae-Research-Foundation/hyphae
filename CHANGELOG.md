# Changelog

All notable changes are documented here. Hyphae follows Semantic Versioning
for public APIs after `0.1.0`; on-disk format versions are tracked separately.

## [Unreleased]

### Changed

- License the current software tree and future releases under
  `AGPL-3.0-only`, while repository documentation and diagrams use
  `CC-BY-SA-4.0`. Releases through `v1.0.1` retain their original
  Apache-2.0 grants.

## [1.0.1] - 2026-08-09

Hyphae 1.0.1 is a distribution follow-up to 1.0.0. It publishes the complete
24-crate Native Rust dependency closure to crates.io; it does not change the
Native format or make a new performance claim. G7 performance certification
remains open.

### Changed

- Enabled crates.io publication for every product crate, including the Native
  storage/runtime substrate, protocol, daemon, client, server, CLI, and
  PliegoRS adapter.
- Added a machine-checked release graph that binds every crate to version
  1.0.1, exact internal requirements, dependency order, and packaged assets.

## [1.0.0] - 2026-08-09

Hyphae 1.0.0 introduces the Native local data ecosystem. G0 through G6 are
closed, and publication is bound to the exact-commit G8 release evidence. G7
performance certification remains open, so this release makes no certified
latency, saturation, or production-scale performance claim.

### Added

- Added the Hyphae-owned Native SQL, structure, lexical, exact-vector, ANN, and
  hybrid engines over shared catalog, WAL, MVCC, recovery, backup, and proofs.
- Added the Native embedded facade, local protocol/daemon, HTTP `/v2`, CLI, and
  Rust, Python, and TypeScript SDK surfaces.
- Added format-2-to-Native offline migration, Native backup/restore, and G7/G8
  fail-closed evidence authorities.

## [0.2.1] - 2026-07-30

Status: published. The annotated `v0.2.1` tag, signed GitHub release, and all
ten Rust crates are recorded in the
[publication receipt](docs/release/receipts/0.2.1.md).

### Added

- Added a post-commit release-evidence contract and validation path that can
  bind an exact source commit/tree, hosted workflow identity, checks report,
  and primary release-artifact digests. No hosted report or manifest is
  claimed until the `0.2.1` gate records it.
- Added explicit finite `StorageLimits`, `RecoveryLimits`, and
  `MaintenanceLimits` entry points for embedded engine/server startup,
  snapshot creation, and compaction. The packaged CLI and
  `HyphaeServer::open` select `StorageLimits::default()`; the existing
  embedded `open` entry points retain their `0.2.0` compatibility policy.
- Added exact aggregate scanned-byte execution entry points for structured
  query. The standalone server uses a fixed 256 MiB ceiling covering every
  inspected binary key plus its canonical document envelope and payload.
  Existing `execute`, `execute_with_clock`, and durable `query` entry points
  retain their `0.2.0` count-bounded behavior without a new byte ceiling.
- Added separate `BoundedQueryError` and `BoundedEngineQueryError` surfaces for
  scanned-byte failures, plus typed `StorageLimitError` causes retained through
  the existing exhaustive storage-error source chains.
- Added bounded proof-producing facade methods that carry the query/retrieval
  operation's remaining timeout through snapshot creation. Existing proof
  methods retain their `0.2.0` signatures and stored maintenance policy.
- Defined one hybrid execution deadline as the saturating sum of the lexical
  and exact-vector branch timeouts. Each branch keeps its own ceiling while
  also consuming that shared total, and fusion must complete before it expires.
  Only `retrieve_hybrid_with_proof_with_limits` carries the remaining hybrid
  total through snapshot creation; the legacy proof method snapshots afterward
  under its retained maintenance policy.

### Fixed

- Raised the default offline snapshot-witness bounds to 2 GiB encoded and
  1 GiB of aggregate decoded logical payload, and aligned exact-vector replay
  candidate key/vector bytes at 1 GiB, so large but bounded result and
  retrieval witnesses are not rejected below those default policy limits.
- Added regression coverage for the shared snapshot and retrieval-verification
  defaults.
- Preflight snapshot-witness file length and logical counts on the same file
  handle before canonical verification, enforce decoded-byte policy during
  that scan, and carry the verifier's remaining deadline through snapshot
  loops. Oversized or over-count witnesses now fail without an unbounded
  preliminary scan.
- Require result and retrieval proof inputs to be regular files, preflight
  their lengths, read at most the caller/hard byte limit in bounded chunks, and
  reject files whose observed length changes during the read. Complete proof
  verification applies its shared deadline during those reads; the standalone
  proof decoders remain byte-bounded APIs without a timeout parameter.
- Preflight exact-retrieval replay candidate count, aggregate key/vector bytes,
  and deadline before allocating and cloning the candidate collection,
  including the exact branch of hybrid replay.
- Bound remote CLI request JSON by the server's default 4 MiB request-body
  policy and proof JSON by its default 32 MiB response policy. File inputs are
  rejected from metadata when already oversized, read through `limit + 1`, and
  rejected if their observed length changes. Bearer-token file/environment
  input is capped at 4,098 raw bytes so one terminal CRLF still permits the
  canonical 4,096-byte token maximum. The 4/32 MiB CLI ceilings are fixed and
  are not negotiated from a custom server's capabilities.
- Bound packaged CLI/server open and additive embedded recovery to one shared
  60-second deadline and finite directory, log, replay, snapshot, and
  lexical-rebuild budgets. A writer opened under that policy rejects an append
  before it would make the active segment impossible to reopen under the same
  policy.
- Bound snapshot creation and compaction when using retained finite
  `StorageLimits` or explicit `snapshot_with_limits`/`compact_with_limits`,
  clean abandoned temporary snapshot/index files, and keep the manifest as
  the compaction commit point.
- Make backup layout validation fail on the first noncanonical entry without
  accumulating directory names. Snapshot copy now captures one opened regular
  file's initial length, copies no more than that length, and rejects a
  same-handle length change before accepting the copy. Complete backup
  verification and restore still have no shared end-to-end deadline.
- Enforce one absolute TypeScript deadline from before request serialization
  across `fetch` and every success/error/witness read, normalize aborts, cancel
  unused bodies, and reject redirects. An injected transport is raced even if
  it ignores cancellation.
- Enforce one absolute Python deadline with a cancelable per-request watchdog
  that closes the active CPython socket across headers and all body kinds.
  Operating-system DNS resolution remains non-preemptible until a socket exists
  and fails closed before the request can continue after an expired lookup.
- Verify HTTP witnesses through the exact file handle later streamed, reject
  oversized files from metadata before hashing, and hold admission through
  stream completion. Direct witness admission now returns HTTP `413`
  `result_too_large` for witness-policy exhaustion, and the OpenAPI contract
  advertises `413` on the route.
- Hardened dependency review to compare explicit base/head Git objects and
  validate the candidate's archived lockfiles instead of mutable worktree
  state.
- Made the Vite host-smoke fixture resolve its physical project root so its
  Windows build remains valid when the checkout is reached through a junction.
- Made lexical-budget durability tests portable to native Windows by asserting
  the append-only log and logical index state instead of reading `redb`'s
  exclusively locked, internally mutable database bytes while it is open.
- Made metadata-only JavaScript manifest changes validate the archived npm
  lock with `npm ci --dry-run`, avoiding both stale dependencies and
  meaningless lockfile edits.
- Isolated native package artifacts from the assembled release-candidate
  artifact so a full workflow rerun cannot merge stale candidate payloads into
  a new assembly.
- Bound every required check to the canonical workflow path and successful
  workflow-run metadata for the same source commit. The release preflight
  fetches all 17 Jobs API records and requires every job `run_attempt` to match
  its workflow run's current attempt, preventing homonymous or mixed-attempt
  jobs from satisfying release evidence.
- Reject unregistered dependency manifests and locks by common file families,
  not only a short basename list.

### Changed

- Refreshed locked Rust dependencies, pinned GitHub setup actions, and the
  framework host-smoke dependency set carried after `v0.2.0`.
- Clarified that TypeScript/Python generated success models are static types,
  not runtime shape validators. The clients reject invalid JSON and validate
  error-envelope/request-ID correlation, but full successful-payload validation
  remains outside this source-only patch.

## [0.2.0] - 2026-07-21

### Added

- Added durable named vector spaces and atomic vector mutations to disk format
  `2`, including snapshot, compaction, backup, restore, migration, and
  derived-index rebuild coverage.
- Added deterministic exact vector retrieval with canonical signed Q15 cosine
  scores, bounded execution, explicit abstention, and stable binary-key ties.
- Added provider-free lexical retrieval with pinned Unicode normalization and
  BM25F-compatible integer scoring.
- Added deterministic hybrid retrieval using reciprocal-rank fusion and
  per-modality explanations.
- Added `retrieval-proof-v1`, including canonical encoding, offline
  verification, and request/result/witness/semantics tamper detection.
- Added additive `/v1` schemas, OpenAPI paths, server routes, generated models,
  Rust/TypeScript/Python clients, remote CLI commands, MCP tools, and shared
  conformance cases for vector, lexical, and hybrid retrieval.
- Added immutable disk-format-2 and retrieval golden fixtures, generators, and
  synchronization checks.
- Added retrieval benchmarks, load and restart/restore soak gates, retrieval
  proof fuzzing, in-flight write interruption recovery, and local release
  evidence.

### Fixed

- Made the `hyphae-contracts` tarball self-contained by shipping byte-identical
  OpenAPI and JSON Schema assets inside the crate.
- Reused the packaged contract constants from the CLI MCP adapter and included
  the engine compatibility fixture in its crate tarball.
- Added a release-readiness audit that rejects compile-time assets outside a
  crate or missing from its generated `cargo package` file list.

## [0.1.0] - 2026-07-16

### Added

- Autonomous product boundary and release gates.
- Clean Rust workspace and public contract layout.
- Audited source-porting policy.
- Append-only durable storage with recovery, idempotency, snapshots,
  migrations, and anchored compaction.
- Deterministic structured query, exact provider-neutral retrieval,
  abstention, budgets, and generative correctness tests.
- Embeddable engine facade and autonomous KV/query/snapshot/compaction CLI.
- Canonical snapshot-witness result proofs with caller-pinned anchors and
  complete offline reexecution.
- Secure OpenAPI-first `/v1` server with bounded requests and loopback default.
- Equivalent Rust, TypeScript, Python, CLI, and MCP public clients with one
  black-box conformance suite.
- Optional PliegoRS, Astro, Next, and Vite adapters with public-only dependency
  enforcement and host-without-Hyphae production-build tests.
- Portable logical backups, atomic verified restores, and complete local
  `doctor` diagnostics in the single binary.
- Immutable on-disk compatibility fixtures and deterministic multiplatform
  release archives.
- SPDX/CycloneDX SBOMs, SHA-256 manifests, SLSA v1 build provenance, keyless
  Sigstore signature/attestation bundles, bounded fuzzing, and
  load/kill-restart soak gates.
- Canonical documentation hub, complete capability/CLI/configuration/data
  model/embedding/operations references, package-specific SDK and MCP guides,
  maintained embedded/HTTP/MCP examples, and automated documentation drift
  validation.
- Public Rust crates for every supported library, the `hyphae` binary, and the
  optional PliegoRS adapter, with package-specific README and docs.rs metadata.
- Official project identity, release badges, website links, and public
  crates.io installation guidance.
