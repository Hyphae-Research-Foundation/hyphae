<!-- SPDX-License-Identifier: Apache-2.0 -->
# Stable `/v1` error codes

Every error body has exactly `code`, `message`, and `request_id`. Messages are
bounded diagnostics, not a compatibility surface; clients branch on `code`
and HTTP status. Internal paths, dependency errors, tokens, document contents,
and backtraces are never returned.

| Status | Code | Meaning |
|---:|---|---|
| 400 | `invalid_request` | JSON or a typed value violates `/v1` semantics |
| 401 | `unauthorized` | Configured bearer credential is missing or wrong |
| 404 | `not_found` | Route or exact snapshot witness does not exist |
| 405 | `method_not_allowed` | Route exists but the method is not defined |
| 408 | `timeout` | Body or cooperative query deadline elapsed |
| 409 | `idempotency_conflict` | UUID was committed with different contents |
| 413 | `payload_too_large` | Complete request/response byte bound failed |
| 413 | `result_too_large` | Proof-bearing result or witness policy failed |
| 415 | `unsupported_media_type` | A JSON route did not receive a JSON media type |
| 422 | `limit_exceeded` | Batch, shape, work, result, proof, or witness limit failed |
| 429 | `busy` | Concurrent-operation admission is saturated |
| 500 | `internal_error` | Local diagnostics are required; details are withheld |
| 503 | `unavailable` | Opened server cannot currently serve data operations |

The exact idempotency UUID is part of a write's durable identity. Retrying the
same UUID with the same canonical batch returns an `existing` receipt;
retrying it with different contents returns `idempotency_conflict`.

Unknown fields are rejected. New optional response fields or endpoints may be
added compatibly inside `/v1`; removing a field or changing deterministic
query/proof behavior requires a new API version.
