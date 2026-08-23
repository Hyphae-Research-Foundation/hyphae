<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native HTTP API v2

Status: implemented bounded Native HTTP v2 adapter and versioned contract; G6
cross-platform and hosted receipts are closed for the bounded product profile

HTTP `/v2` is an optional edge adapter over the native product facade. The
embedded API and native local protocol remain the primary performance
surfaces. No engine-to-engine call uses HTTP or JSON.

## Boundary

The single `hyphae` binary starts no listener unless `serve` is selected. The
default bind is loopback. Remote exposure requires explicit bind,
authentication, request/response limits, and operator-owned TLS termination or
a separately accepted in-process TLS decision.

`NativeHttpV2Server::new` preserves the unmanaged adapter only while the
directory is not bootstrapped. A bootstrapped service automatically selects
managed authentication. During 1.2 only, durable `dual_window` plus explicit
process configuration may additionally authenticate the migrated fixed bearer
as synthetic `legacy-owner`; every other state/configuration combination fails
startup closed.
`NativeHttpV2Server::new_managed` can force the same managed policy before
bootstrap without changing the public configuration shape. Managed mode
permits a loopback bind only because this adapter does not terminate TLS and
opens product sessions only through
`NativeProductHandle::open_authenticated_session`. Remote managed exposure
must terminate TLS in a proxy that forwards to the loopback listener; a durable
API key never travels to a non-loopback plaintext bind.

`POST /v2/security/keys` owns minor-3 API-key lifecycle requests. Start
responses are one-time binary secret deliveries with mandatory `no-store,
private, max-age=0`, `Pragma: no-cache`, and no content encoding. The endpoint
accepts every self/admin Issue Start, Activate, Abort, Rotate Start, Activate,
Abort, and Revoke variant, requires strict durability and catalog-managed
authority, and does not accept filesystem paths. Start must not be automatically
retried. The generic `/v2/execute` family rejects every lifecycle variant.
The Rust and Python Native v2 HTTP clients enforce the matching egress rule:
plaintext credentials are accepted only for canonical IPv4/IPv6 loopback or
the exact `localhost` hostname, while every other managed origin requires
`https://`. The MCP adapter inherits the Rust check before starting its stdio
request loop. The separate format-2 `/v1` bearer client is unchanged.

## Resource families

The versioned contract includes resources for capabilities, catalog, SQL,
structures, search, transactions, explain, proofs/witnesses, telemetry,
doctor, backup, restore, and bounded administration. Concrete route schemas are
added contract-first with canonical JSON Schema and OpenAPI 3.1 assets.

Large admitted batches and result streams use bounded streaming with
backpressure. Multi-item mutations publish all items or none and never return
partial item success. Read streams carry provisional chunks followed by one
mandatory completion trailer; a client discards all provisional chunks if the
trailer is absent or reports an error. Limits and cancellation before the
trailer therefore produce no successful logical result without requiring the
server to stage an unbounded response.

## Identity and errors

Requests carry or receive a stable request ID. Responses expose visible CSN,
catalog version, and relevant stable object IDs. Errors use
[`product-error-v1.md`](product-error-v1.md); HTTP statuses are transport
mapping, not the stable error identity.

Managed mode requires exactly one `Authorization` header containing the
canonical `Bearer hyp1_<key-id>_<secret>` form on every request. Missing,
duplicated, malformed, unknown, and incorrect credentials return the same
typed `authorization_denied` error with HTTP `401` and
`WWW-Authenticate: Bearer realm="hyphae-native-v2"`. The candidate is copied
only into the product's redacted, zero-on-drop credential carrier; the HTTP
adapter never stores or logs the secret and does not hold its session mutex
while the sole product owner authenticates it.

A valid credential creates a catalog-managed product session with the key's
current principal, permissions, scope, epoch, expiry, and directory lineage.
Retained HTTP sessions are bound to a non-reversible credential fingerprint;
their product session revalidates durable authority before execution. A
permission denial or authority loss after successful request authentication
remains a typed `authorization_denied` response with HTTP `403` and no
authentication challenge. Authentication is repeated before retained-session
lookup, so a revoked or expired canonical credential instead receives the
uniform challenged `401` response even when it supplies its former session ID.

Request admission is acquired before every blocking authentication task,
including `GET /v2/capabilities`. At most 256 HTTP requests and 256 verifier
tasks can be active; overflow fails immediately with typed `unavailable` rather
than entering an unbounded blocking queue. Execution retains the request permit
acquired by authentication and never acquires a second permit.

Normal authentication never opens a terminal session. If a canonical key is
not live, the product returns only opaque pending state with no credential
bytes or public revoked/unknown distinction. Every method and path except
`POST /v2/security/keys` rejects that state with the same challenged `401`
before handler or product-client dispatch. The dedicated route may read and
decode its already bounded body under admission. Only an exact self-revoke or
zero-overlap self-rotation activation whose actor key, operation, nonzero
idempotency token, complete request digest, and durable marker all match may
consume that state and open terminal replay authority. Every other body returns
the same challenged HTTP `401` as an unknown credential, never `403` or an
idempotency/digest oracle.

The migrated bearer follows the same retained-session distinction: terminal
revocation makes the next operation on a retained synthetic session HTTP 403,
while a new legacy request receives uniform HTTP 401. A candidate beginning
with canonical `hyp1_` syntax never falls back to the legacy verifier. The
legacy bearer plaintext is process-local and supplied only from
`--native-legacy-bearer-file`; `HYACAT05` stores only its verifier keyed by the
persisted product-local cursor authority. No HTTP body or CLI argument carries
the secret, and no bare digest is an offline authentication credential.
It is never installed in the native local daemon, UDS, or named-pipe handshake.

## `/v1` compatibility

The published format-2 `/v1` contract is not silently redefined. After native
cutover, an explicitly retained `/v1` route calls the native facade only when
its complete semantics and proof behavior map exactly. Otherwise it returns a
documented incompatibility error. `/v1` never opens a format-2 directory in the
Native 1.0 process.

The implementation makes that boundary structural: `NativeHttpV2Server::new`
accepts only `NativeProductHandle`. It has no data-directory path and no API
that can construct `HyphaeEngine`, `NativeProduct`, or format-2 authority. The
existing `HyphaeServer` retains its current `/v1` behavior as a separate
format-2 service. On the Native server, `/v1` compatibility is exact mappings
only; the current mapping set is empty because the published v1 proof and data
semantics are not exact Native equivalents, so every v1 request fails
explicitly with `invalid_request` and HTTP 409.

## Implemented framing

- Canonical `HYPREQ01` and `HYPRSP01` product envelopes use media type
  `application/vnd.hyphae.product-v1` at the HTTP edge. Handlers decode directly
  to `ProductOperation`, submit through `NativeProductHandle`, and encode the
  resulting `ProductResponse`.
- JSON Product errors retain code, category, retry, message, request, trace,
  object, transaction, limit, source-span, and typed-detail fields. Clients can
  request exact `HYPERR01` bytes with `Accept:
  application/vnd.hyphae.error-v1`.
- `X-Hyphae-Request-Id` is a nonzero canonical decimal `u128`; the server
  accepts one caller value or generates one and uses it as the Product request
  ID.
- `X-Hyphae-Deadline-Micros`, when present, must exactly match the deadline in
  the canonical request envelope.
- `/v2/read-stream` emits provisional base64 chunks as NDJSON and one mandatory
  terminal `completion` record. A connection ending before that record has no
  successful logical result.

## Verification

OpenAPI/JSON Schema synchronization, authentication, body/response bounds,
timeouts, cancellation, streaming, request-ID correlation, error parity,
cross-surface result equality, and listener opt-in are mandatory G6 evidence.
Managed-auth evidence additionally covers exact bearer grammar, uniform
`401` challenges, permission `403` mapping, credential-bound session reuse,
and service-level revocation revalidation.

## Managed security administration

The generic `/v2/execute` envelope carries the six bounded security reads from
Native protocol minor 1 and the six secret-free security mutations from minor
2. The write operations create or enable a principal, create an immutable
custom role, create a non-Owner built-in or custom-role assignment, or revoke
a non-Owner assignment. Each mutation requires instance-scoped
`security.manage`, strict durability, and a nonzero idempotency token in the
canonical request context. Local and HTTP transports return the same durable
receipt for an exact replay and the same `idempotency_conflict` for token reuse
with a different request.

Every Native v2 request must offer its supported protocol minors in
`X-Hyphae-Protocol-Minor` as bounded canonical decimal tokens: at most eight
across every header instance, comma separation tolerated for header-coalescing
intermediaries, no duplicates, no leading zeros, no other bytes. The server
selects the highest offered minor it serves (this build serves minors 3, 4,
and 5) and rejects a missing, malformed, duplicated, or entirely unsupported offer
before authentication and product dispatch. The selection gates request decode
and response encode, and the server emits the selected minor (or its highest
served minor on admission failure) in `X-Hyphae-Protocol-Minor` on every
success and error. Clients validate that the echoed selection is one of their
offered minors before accepting session state, reading a body, or applying
one-time-secret handling, and decode the response at the selected minor.
The explicitly contracted `/v1` incompatibility response is not converted into
a v2 request by this admission rule.

API-key issuance, rotation, activation, abort, and revocation use the dedicated
minor-3 security route. Legacy-bearer migration remains offline and file-only;
HTTP carries only the canonical Owner-authorized terminal revocation operation.
Ownership transfer, owner recovery, and arbitrary server-local paths are not
representable in this HTTP slice.
