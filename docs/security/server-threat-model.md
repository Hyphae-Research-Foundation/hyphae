# `/v1` server threat model

## Trust boundaries

The HTTP listener accepts untrusted transport metadata and request bytes. The
embedded engine and its owned data directory are trusted only after normal
format, log, snapshot, and index verification. A bearer token authenticates
access to one process; it does not establish multitenancy or end-to-end TLS.

## Defaults

- No listener starts during ordinary embedded or CLI operations.
- `hyphae serve` defaults to `127.0.0.1:8787`.
- A non-loopback bind is rejected before `bind(2)` unless bearer
  authentication is configured.
- Data operations never contact an upstream service.

## Enforced limits

- complete request body bytes and receive time before durable work starts;
- JSON nesting depth and total nodes;
- key, document, vector dimension/candidate bytes, lexical documents/tokens,
  batch, filter, sort, group, metric, page, scan, match, proof, snapshot
  witness, and response sizes;
- admitted concurrent data operations;
- cooperative query and verification timeouts.

Limit failures return a stable error and no partial query or proof result.
Once a durable write begins it is allowed to reach a definite receipt rather
than being cancelled behind the engine's back.

A commit that is durable but not materialized still returns its receipt, then
marks readiness unavailable. Subsequent engine operations fail closed until a
restart replays the authoritative log. Existing verified witnesses remain
downloadable during that state.

## Authentication

Configured bearer tokens are hashed before comparison. Missing and incorrect
credentials return the same `unauthorized` response. Tokens are never logged,
echoed, or included in request IDs. Health endpoints disclose no records or
configuration secrets.

Tokens contain at least 32 visible ASCII bytes. The CLI accepts them from an
environment variable or restricted file, never from an argv value. Unix token
files with group/other permissions are rejected; Windows operators must apply
an account-only ACL.

Remote deployment still requires a trusted TLS boundary. Hyphae 0.2.x does
not claim built-in certificate management, OAuth, tenant isolation, WAF,
rate-limit coordination across processes, or denial-of-service resistance
beyond its documented local limits.

Retrieval requests are untrusted computation. Q15 validation rejects invalid
elements, dimensions, and zero vectors before durable append or ranking.
Exact, lexical, and hybrid operations enforce independent work/time/result
budgets and return no partial ranking. Lexical normalization is deterministic
but is not a content-safety or secret-redaction mechanism.

The server and public clients retain a 512 MiB witness-transfer default.
The `0.2.1` source candidate's larger default local offline-verification
policy does not raise that remote transport limit.

Proof-bearing routes derive snapshot creation limits from that 512 MiB
transport policy before snapshot measurement or hashing and spend only the
originating query/retrieval request's remaining timeout. Witness download
preflights file metadata before hashing, verifies and streams the same open
regular-file handle, caps the stream at the verified byte length, and retains
its admission permit until that boundary or body drop. The cap prevents a late
append from adding unverified response bytes. It does not prevent an external
same-length overwrite of the inode after verification; operating-system
ownership and process isolation remain required. A snapshot limit/timeout
returns `result_too_large`/`timeout` without changing server readiness.

## Error disclosure

Public errors expose a stable code, bounded human message, and request ID.
Internal filesystem paths, tokens, Rust backtraces, document contents, and
dependency errors are not returned. Detailed diagnostics remain local logs.
