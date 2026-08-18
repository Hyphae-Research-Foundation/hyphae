<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native product error model v1

Status: implemented shared G6 product contract in `hyphae-native-product`

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

The v1 implementation admits at most 64 ASCII bytes for code and limit
identifiers, 256 UTF-8 bytes for the fixed safe message, 16 unknown detail
fields, 256 bytes per unknown detail, and 8 KiB for the complete canonical
binary envelope. Identifiers are nonempty lowercase ASCII beginning with a
letter and continuing with letters, digits, or underscore. `source_span` is a
half-open `[start, end)` pair of unsigned 32-bit byte offsets with `start <=
end`. Request IDs, trace IDs, object IDs, and transaction IDs are 128-bit;
object and transaction IDs are nonzero.

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
| `invalid_request` | `invalid-request` | `never` |
| `catalog_conflict` | `conflict` | `new-snapshot` |
| `deadline_exceeded` | `deadline` | `same-request` |
| `cancelled` | `cancelled` | `same-request` |
| `authorization_denied` | `authorization` | `never` |
| `unavailable` | `unavailable` | `after-backoff` |
| `unknown_commit` | `unavailable` | `unknown-commit` |
| `backup_invalid` | `corruption` | `after-recovery` |
| `idempotency_conflict` | `conflict` | `never` |
| `secret_delivery_consumed` | `conflict` | `never` |
| `confirmation_digest_mismatch` | `authorization` | `never` |
| `upgrade_required` | `conflict` | `after-recovery` |

The first 21 strings above are the originally accepted registry and retain
their exact order and meaning. New v1 strings are appended only. A decoder that
does not recognize an otherwise canonical code represents it as unknown while
retaining its exact raw string, category, retry class, message, and fields.

## Typed fields

`limit` consists of a typed limit identity, a configured unsigned 64-bit
value, and an observed unsigned 64-bit value. The initial identities are
`sql_statement_bytes`, `sql_parameters`, `sql_result_rows`, `request_bytes`,
`response_bytes`, `hash_field_batch_items`, `set_member_batch_items`,
`expiry_sweep_keys`, and `group_commit_transactions`. Unknown canonical limit
identities are preserved.

`details` is an ascending-tag typed field sequence. The initial registry is:

| Tag | Field | Canonical payload |
|---:|---|---|
| 1 | SQL subcode | exactly one of ASCII `HYSQL001` through `HYSQL017` |
| 2 | transaction ID | nonzero little-endian `u128` |
| 3-65535 | unknown future detail | opaque bytes, retained within v1 bounds |

No detail tag may repeat. Unknown detail order and bytes survive decode and
re-encode. SQL source mappings retain the exact `HYSQLxxx` identity rather than
requiring callers to infer it from a broad product code.

## Canonical binary envelope

The local protocol and other binary adapters use `HYPERR01`. All integer
fields are little-endian. The fixed header is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYPERR01` |
| 8 | 4 | complete envelope byte length |
| 12 | 1 | category discriminant |
| 13 | 1 | retry discriminant |
| 14 | 1 | transaction-state discriminant |
| 15 | 1 | optional-field flags |
| 16 | 1 | code byte length |
| 17 | 2 | message byte length |
| 19 | 1 | detail field count |

The header is followed by code bytes, message bytes, and flagged fields in
this exact order: request ID, trace ID, object ID, limit, and source span. A
limit is one-byte name length, name bytes, configured `u64`, and observed
`u64`; a source span is start `u32` followed by end `u32`. Details then use
`u16 tag`, `u16 payload length`, and payload bytes in strictly increasing tag
order.

Category discriminants follow the category list in this document starting at
zero. Retry discriminants follow the retry list starting at zero.
Transaction-state discriminants are `none=0`, `active=1`, `rolled-back=2`,
`committed=3`, and `outcome-unknown=4`. Optional flag bits 0 through 4 mean
request ID, trace ID, object ID, limit, and source span respectively; all other
bits are zero.

Canonical decoding requires an exact declared length, no trailing bytes,
known discriminants, zero reserved flags, canonical bounded identifiers,
valid UTF-8, nonzero typed identities, increasing unique detail tags, exact
known-detail widths, and fields consistent with the registry. Known codes use
their fixed redaction-safe message and fixed category/default retry. Malformed,
truncated, oversized, or noncanonical envelopes fail closed. Encoders emit the
same unique bytes after a successful decode.

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

Direct mutating `ExecuteSql` does not run a cancellation/deadline checkpoint
after its commit returns. A cancellation observed at or after that boundary
therefore preserves the committed response and transaction receipt. If the
commit call itself cannot prove its result, the response is `outcome-unknown`
with the transaction identity; it is never rewritten as ordinary `cancelled`.
Read-only SQL retains its final publication checkpoint.

`ProductFailureBoundary` represents `none`, `active`, proven rollback, proven
commit, and publication-unknown boundaries. Applying publication-unknown to
any source failure deterministically produces code `unknown_commit`, retry
`unknown-commit`, transaction state `outcome-unknown`, and the nonzero
transaction ID. It never inherits a broad deadline, cancellation, I/O, or
unavailable retry classification.

## Verification

Golden vectors cover every category, unknown codes, size limits, malformed
fields, redaction, SQL spans, retry classes, and uncertain commit outcomes.
Cross-surface tests provoke the same engine failures and compare stable fields.
