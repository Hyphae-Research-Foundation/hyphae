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
