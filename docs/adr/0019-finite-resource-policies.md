# ADR-0019: Finite resource policies for bounded durable operations

- Status: Accepted
- Date: 2026-07-29
- Owners: Celiums Solutions LLC

## Context

Hyphae accepts attacker-controlled documents, queries, vectors, lexical text,
logs, snapshots, and proof witnesses while remaining an offline-capable
single-process engine. Per-page or per-record checks do not bound the complete
operation: a valid input can cross many pages, and recovery, retrieval,
compaction, or proof creation can spend work in several successive phases.

The `0.2.0` Rust API also published exhaustive error enums. Adding new variants
to those enums in a `0.2.1` patch would break downstream exhaustive matches.
Conversely, silently mapping every new resource failure onto an unrelated old
variant would erase the policy evidence callers need.

## Decision

One monotonic deadline covers each complete bounded operation. Work completed
in validation, lookup, tokenization, scan, decoding, scoring, ordering, proof
construction, snapshot verification, replay, rebuild, or commit preparation
reduces the time available to every later phase. A timeout or exhausted
aggregate budget returns no partial query or retrieval result.

Structured-query record-count limits remain unchanged. New additive query
entry points also account every inspected binary key plus its canonical
document envelope and payload across all shards and storage pages. The
standalone server uses a fixed 256 MiB aggregate scanned-byte ceiling. The
legacy in-memory `execute` and `execute_with_clock` entry points retain their
`0.2.0` signatures and error surface; callers select byte accounting through
the additive bounded entry points and their separate error type.

Exact and lexical retrieval each spend one end-to-end configured deadline from
before definition lookup and request validation through candidate
materialization, scoring, stable ordering, and result construction. Hybrid
defines its total deadline as the saturating sum of the lexical and
exact-vector timeouts. The lexical branch is capped by its own timeout and the
remaining total; the vector branch is then capped by its own timeout and the
remaining total; fusion must also finish within that total. The additive
proof-producing `*_with_limits` facade methods include snapshot creation in the
operation's remaining deadline. Legacy proof methods retain their `0.2.0`
signatures and snapshot afterward under the maintenance policy stored by the
opened engine.

Standalone result/retrieval proof readers require a regular file, preflight its
metadata length, read in bounded chunks through the smaller of the caller and
canonical proof limit, and reject a changed observed length. Those reader APIs
have no timeout parameter. Complete verification paths use the same readers
with the verifier's remaining cooperative deadline. Exact-vector proof replay
first scans matching candidates to enforce count, aggregate key/vector bytes,
and deadline, and only then allocates and clones the candidate collection.

Remote CLI request JSON uses the server's default request-body ceiling; proof
JSON uses its default response ceiling. File inputs are preflighted from
metadata, read through `maximum + 1`, and rejected if their length changes.
Stdin receives the same byte ceiling without file metadata. Bearer-token
file/environment input allows at most 4,098 raw bytes so stripping one terminal
CRLF still leaves no more than the canonical 4,096 token bytes.

Backup layout verification keeps only the two canonical-name flags and fails
on the first unexpected entry. Snapshot copy opens a regular source once,
captures its length, copies through that exact byte boundary, and rejects a
different final length from the same handle. These byte/layout bounds do not
add a new shared deadline to complete backup verification or restore: those
published `0.2.0` operations retain their compatibility behavior and require
an external command timeout for an operator-level elapsed-time ceiling.
Restore still composes the legacy verifier, reopen, and snapshot paths; neither
that composition nor an individual filesystem call or `sync_all` is
preemptible by the new policies.

The packaged CLI and `HyphaeServer::open` select finite
`StorageLimits::default()`. Embedded callers opt into the same policy with
`StorageEngine::open_with_limits` or `HyphaeEngine::open_with_limits`; the
published `open` methods retain their `0.2.0` compatibility policy and do not
silently acquire new finite ceilings in a patch release. A writer opened under
finite limits preflights later appends so a successful write cannot make the
active segment ineligible for a subsequent reopen under the same policy.
Snapshot and compaction reuse the policy retained at open, while their
additive `*_with_limits` methods accept an explicit finite maintenance policy
intersected with the retained recovery snapshot policy. Manifest activation
remains the compaction commit point; an ambiguous activation poisons the
current handle until reopen. Temporary artifacts are best-effort cleanup after
a definite outcome.

The exhaustive error enums published in `0.2.0` do not gain variants. New
bounded entry points use additive error types. Storage-limit failures carried
through an existing error variant remain typed in its source chain and are
available through explicit accessors. This preserves downstream source
compatibility without hiding the finite-policy cause.

The fixed query byte ceiling is not added to `ApiLimitsV1`: that wire object
rejects unknown fields, so adding one in a patch would break strict `0.2.0`
clients. The ceiling is documented as a fixed server policy and may become an
advertised capability only in a compatibly versioned contract.

## Consequences

- Work and decoded data are bounded across each bounded operation instead of
  independently per helper or page.
- Existing exhaustive matches over published `0.2.0` errors continue to
  compile against `0.2.1`.
- Legacy embedded entry points remain behaviorally compatible and do not
  silently gain aggregate query-byte or finite storage ceilings. New
  security-sensitive embeddings should select explicit finite limits, normally
  `StorageLimits::default()`.
- A data directory accepted by an earlier binary can be rejected by the
  packaged CLI/server or an explicitly bounded embedded open if it exceeds the
  selected finite ceilings. The legacy compatibility open remains available;
  neither path changes the disk format.
- Cooperative deadlines can stop between bounded chunks but cannot preempt an
  operating-system call already in progress.
- Standalone proof parsing is byte bounded but not time bounded; callers that
  require one deadline through proof read, snapshot loading, and replay use the
  complete verification APIs.
- Backup copy is fixed to one captured source length, but complete
  backup-verify/restore remains an operational timeout boundary rather than a
  newly deadline-bounded `0.2.1` API.
- Wire publication of query-byte policy is deferred rather than weakening the
  strict version-1 contract.

## Alternatives considered

- Adding variants to the existing public errors was rejected because it would
  make the patch source-incompatible.
- Reusing record-count errors for byte exhaustion was rejected because it
  would make diagnostics and policy evidence false.
- Applying fresh timeouts independently to lookup, scan, scoring, and proof
  construction was rejected because total elapsed time would remain unbounded.
- Advertising a new field in `ApiLimitsV1` was rejected because strict clients
  intentionally reject unknown fields.
- Making the published embedded `open` methods finite was rejected for this
  patch because it would silently tighten `0.2.0` behavior. Additive bounded
  entry points and finite CLI/server defaults provide the hardened path without
  removing the compatibility path.

## Verification

- `cargo-semver-checks 0.48.0` compares every workspace library to the exact
  `v0.2.0` commit as a patch release in the required release-readiness job.
- Structured-query tests cover exact and one-byte-under aggregate budgets
  across the 4,096-entry storage page boundary.
- Retrieval tests inject clocks during lookup, tokenization, corpus scan,
  scoring, ordering, final result construction, hybrid fusion, and bounded
  proof snapshot creation.
- Storage tests cover exact and N+1 log, replay, lexical rebuild, snapshot,
  directory-entry, compaction, cleanup, and reopen limits.
- CLI tests cover exact/over-limit JSON and bearer inputs plus file-length
  changes; backup tests cover fail-fast layout validation and source-length
  changes during bounded copy.
- Proof and HTTP tests cover regular-file and changed-length proof reads,
  candidate preflight before materialization, same-handle witness verification,
  accepted large witnesses, direct-witness HTTP `413`, oversized rejection,
  timeout, and no-partial-result behavior.
