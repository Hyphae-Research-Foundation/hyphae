# Native product error model v1

Status: accepted G6 planning contract; implementation incomplete

Every public Native surface reports one transport-independent error model.
Rust enum layout, `Display` text, HTTP status, CLI exit status, and frame kind
are adapters and are not the stable identity.

## Error envelope

One error contains:

- `code`: stable lowercase ASCII identifier;
- `category`: `invalid-request`, `not-found`, `conflict`, `limit`, `deadline`,
  `cancelled`, `authorization`, `corruption`, `unavailable`, `io`, or
  `internal`;
- `retry`: `never`, `same-request`, `new-snapshot`, `after-backoff`,
  `after-recovery`, or `unknown-commit`;
- `message`: bounded safe UTF-8 text;
- `request_id`: optional 128-bit product request identity;
- `trace_id`: optional local diagnostic identity;
- `object_id`: optional stable catalog object identity;
- `transaction_state`: `none`, `active`, `rolled-back`, `committed`, or
  `outcome-unknown`;
- `limit`: optional limit name, configured value, and observed value;
- `source_span`: optional byte offsets into caller-supplied SQL or query text;
  and
- `details`: code-specific, versioned, bounded typed fields.

Messages and details never disclose credentials, document values, binary
keys, full SQL text, internal backtraces, or host paths. SDK callers inspect
typed fields rather than parsing messages.

## Registry rules

- Codes are append-only within v1.
- A Rust error variant may map to a stable code, but renaming the variant does
  not rename the public code.
- Distinct retry or transaction outcomes cannot collapse into one broad
  `engine_failure` code.
- SQL `HYSQLxxx` identities are retained as details or subcodes where useful.
- Unknown future codes decode as an unknown typed error while preserving the
  raw stable code and safe fields.

## V1 code registry

This is the exact append-only v1 code registry. Category is stable for every
listed code. Retry is the stable default where deterministic;
`failure-dependent` means the error's typed cause selects the retry class. In
v1, `io` uses `after-backoff` for interrupted, would-block, and timed-out
operations and `after-recovery` for other I/O failures.

| Code | Category | Retry default |
| --- | --- | --- |
| `data_directory_exists` | `conflict` | `never` |
| `data_directory_locked` | `unavailable` | `after-backoff` |
| `invalid_data_directory` | `corruption` | `after-recovery` |
| `format2_directory` | `invalid-request` | `never` |
| `catalog_object_not_found` | `not-found` | `never` |
| `sql_invalid_syntax` | `invalid-request` | `never` |
| `sql_parameter_mismatch` | `invalid-request` | `never` |
| `sql_catalog_changed` | `conflict` | `new-snapshot` |
| `sql_foreign_prepared` | `conflict` | `never` |
| `sql_unknown_object` | `not-found` | `never` |
| `sql_invalid_value` | `invalid-request` | `never` |
| `sql_no_access_path` | `invalid-request` | `never` |
| `sql_unique_violation` | `conflict` | `never` |
| `sql_check_violation` | `conflict` | `never` |
| `sql_foreign_key_violation` | `conflict` | `never` |
| `write_conflict` | `conflict` | `new-snapshot` |
| `object_not_found` | `not-found` | `never` |
| `limit_exceeded` | `limit` | `never` |
| `corruption` | `corruption` | `after-recovery` |
| `io` | `io` | `failure-dependent` |
| `internal` | `internal` | `never` |

The native frame header retains its independent 64-bit stream/request
correlation ID. The product request identity is negotiated or carried in the
request/error payload and remains stable across retries or transport changes;
the frame correlation ID is connection-local and may differ.

## Surface mapping

- Embedded Rust returns a structured error with accessors.
- The local protocol encodes the canonical binary envelope.
- CLI maps categories to stable exit classes and supports machine-readable
  output.
- HTTP `/v2` maps categories to documented statuses while retaining `code` and
  retry semantics in the body.
- SDKs reconstruct the same typed error for local and HTTP transports.

## Commit uncertainty

A disconnect, cancellation, or timeout after publication may produce
`outcome-unknown`. The error carries the transaction or idempotency identity
needed to resolve status. No adapter reports rollback unless rollback is
proven, and no adapter automatically retries a possibly committed mutation
without an idempotency contract.

## Verification

Golden vectors cover every category, unknown codes, size limits, malformed
fields, redaction, SQL spans, retry classes, and uncertain commit outcomes.
Cross-surface tests provoke the same engine failures and compare stable fields.
