# Changelog

All notable changes are documented here. Hyphae follows Semantic Versioning
for public APIs after `0.1.0`; on-disk format versions are tracked separately.

## [2.0.1] - 2026-08-23

Hyphae 2.0.1 is the registry publication of the 2.0 program. The first
live 2.0.0 gate runs exposed two defects in tag-bound control code — the
distribution policy rejected the new optional extras' guarded
requirements, and an operator recovery deleted the immutable 2.0.0
GitHub release, permanently retiring that tag name for release
creation — so the publication ships as a new exact release version, the
same discipline as 1.2.2. It contains no engine changes.

### Fixed

- The Python distribution policy admits the frozen optional extras
  (`providers`, `langchain`, `llamaindex`): every declared requirement
  must be one of the frozen extra-guarded strings, the base install
  still carries zero runtime dependencies, and unguarded or unknown
  requirements still fail closed.
- The DCO certificate walk exempts the forge's merge commits, matching
  reference DCO practice.

## [2.0.0] - 2026-08-23

Hyphae 2.0.0 completes the RAG competitive program: hybrid retrieval,
attested model stages, budgeted highlighting, verifiable agent memory,
framework adapters, and the Weaviate exit ramp — every claim seated in
the retrieval claim ledger with a receipt.

### Added

- Hybrid retrieval measured under the sealed protocol: +17.9% nDCG@10
  over lexical on NFCorpus with attested local embeddings; the
  deterministic weighted reciprocal-rank fusion default is a measured
  choice over `weighted_score`.
- Attested model stages end to end: the `HYATTS01` envelope and pure
  verifier in core, `hyphae-embed` (candle, CPU-replayable) for
  embeddings and reranking, declared-provider records in both SDKs, and
  the sealed rerank stage the engine applies without running any model
  (+11.5% nDCG@10 over BM25 with no vector index; the same bi-encoder
  stacked on hybrid subtracts 2.9% — published).
- Budgeted highlighting at protocol minor 5: deterministic
  normalized-text fragments per hit, proof-bound budget at semantics
  version 4, cross-language goldens in all three codecs.
- Deterministic chunking with provenance sealed in proofs, parent
  deduplication, and the raised 100,000-document collection cap with
  filtered-eligibility evidence.
- `search consolidate`: bounded ANN delta consolidation through a new
  admin maintenance-status surface.
- Verifiable agent memory as three MCP tools (store, recall, forget):
  lifecycle-keyed TTL, recall that filters dead memories and can seal
  itself, tool schema v4.
- LangChain and LlamaIndex vector stores with provable retrieval and the
  provable RAG cookbook.
- The Weaviate exit ramp: a cursor-API importer with sealed fidelity
  receipts and the "Leave Weaviate with a receipt" guide, demonstrated
  live against the head-to-head instance (3,633/3,633 objects, exact).
- The retrieval claim protocol: every published number maps to a
  regenerable receipt, losses published with the wins — including the
  head-to-head where hybrid quality, rerun stability, and cold start win
  while strict-durable ingest and small-scale RSS lose.

### Fixed

- The local daemon no longer closes a connection when a flow-control
  credit races the stream's own completion — streamed responses beyond
  the crediting threshold (sealed proofs with megabyte witnesses) now
  leave the connection healthy.
- The Python SDK decodes integrated search responses again (a property
  called as a method broke every integrated decode); Rust-encoded
  response goldens now lock the decode path in both SDK suites.
- The CLI dispatch runs on a worker with an explicit stack and a
  heap-pinned future, ending Windows main-thread stack overflows.
- Windows-hosted CI stabilized across the protocol minor-5 authority
  sweep.

### Compatibility

- Native local protocol minor 5 (minor-4 exchanges keep exact
  historical bytes); HTTP v2 serves minors 3, 4, and 5.
- Proof semantics version 4 for requests carrying a highlight budget;
  verifiers accept versions 2 through 4.
- On-disk directory format unchanged from 1.2.

## [1.2.2] - 2026-08-19

Hyphae 1.2.2 is the Apache-2.0 registry publication of the 1.2 program. The
first live crates.io gate run for `v1.2.1` exposed a defect in the trusted
publication checker, and that checker is tag-bound control code, so the fix
ships as a new exact release version. It contains no engine changes.

### Fixed

- The registry publication gate now accepts the Cargo VCS metadata of a clean
  packaging tree, which omits the dirty marker entirely, and continues to
  fail closed on an explicit dirty marker.

## [1.2.1] - 2026-08-19

Hyphae 1.2.1 is the Apache-2.0 registry publication of the 1.2 program,
re-issued from the current integration tree after `v1.2.0` closed its
exact-SHA release gates. It contains no engine changes beyond `1.2.0`.

### Added

- Documented the Cursor Cloud development environment in `AGENTS.md`.

### Changed

- Moved every workspace, SDK, packaging, and registry-publication pin to the
  exact `1.2.1` release version.

## [1.2.0] - 2026-08-19

Hyphae 1.2.0 completes the coupled operator/agent-experience and relicensing
programs: durable native access control, the operator console, agent plugins,
a published Python client, and the Apache-2.0 software transition. It does
not change the Native format.

### Added

- Added durable principals, custom roles, scoped grants, API keys, rotation,
  revocation, owner bootstrap/recovery, legacy-bearer migration, and bounded
  security audit committed through the native WAL and CSN sequence.
- Added the `hyphae console` Ratatui operator console with bounded SQL,
  keyspace, search, catalog, telemetry, maintenance, backup, and redacted
  security views.
- Added the versioned read-only Native v2 MCP stdio server with Claude Code
  and Codex plugin manifests around one adapter.
- Added the typed `hyphae` Python client with embedded/local and remote
  transports and sync/async APIs.
- New native data directories are now created owner-only (`0o700`) on Unix
  inside the `mkdir` call itself, covering the raw WAL, pages, blobs,
  security catalog, and the default local endpoint.

### Changed

- License the software tree and normative specifications under `Apache-2.0`;
  narrative documentation remains `CC-BY-SA-4.0`. Published `v1.1.0`
  artifacts retain their `AGPL-3.0-only` terms.
- Deprecated the pre-daemon `LocalDataSession`, `UdsFrameListener`, and
  `UdsFrameConnection` runtime APIs; local clients use `hyphae-native-daemon`
  with `hyphae-native-protocol`.
- Impossible internal residues in the local session, aggregate convergence,
  and product vector-search paths now return bounded typed failures instead
  of panicking; the remaining format-2 invariant panics are documented at
  each site.
- Refreshed the architecture overview, workspace-boundary ADR, gate-status
  documents, and crate READMEs to describe the shipped Native generation and
  label the format-2 product as compatibility-only.

### Non-claims

- No universal latency claim; transport, execution, queueing, and durability
  remain separately reported.
- Filesystem hardening covers newly created data directories; existing
  directories retain their modes, and a custom Unix endpoint outside the
  data directory requires an owner-only parent directory.

## [1.1.0] - 2026-08-14

Hyphae 1.1.0 completes the Native local-engine readiness program through G7
and prepares one exact source candidate for the independent G8 release gate.
It does not change the Native format.

### Added

- Added calibrated execution topology, bounded admission, retained ANN read
  views, deterministic routing evidence, and fail-closed hardware/bootstrap
  validation for the controlled performance program.
- Added a source-bound G7 operational-scale closure over the validated
  DigitalOcean C-60 control matrix: eleven surfaces at C1, C8, and C32, with
  1,000,000 observations and 100,000 warmups per surface.

### Changed

- License the current software tree and future releases under
  `AGPL-3.0-only`, while repository documentation and diagrams use
  `CC-BY-SA-4.0`. Releases through `v1.0.1` retain their original
  Apache-2.0 grants.
- Hardened concurrent scheduling, wake observation, queueing, recovery,
  calibration resampling, ANN publication, and release evidence validation.

### Non-claims

- The G7 closure is environment-bound. It does not certify canonical
  dedicated-hardware latency, background interference, or bare-metal
  performance. The final bare-metal attempt failed during toolchain setup
  before product measurement.
- G8 and publication remain open until the exact 1.1.0 candidate passes the
  full release-safety matrix.

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
