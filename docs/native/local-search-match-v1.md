# Native local SEARCH MATCH v1

Status: implemented over the serial UDS session; direct-Linux evidence
recorded; complete search and performance gates remain open.

This contract adds the first search-engine operation to the serial
filesystem-backed `HYPHLCL1` session. It carries a catalog object identity,
UTF-8 query, and bounded hit limit directly to Hyphae's physical inverted
index. It does not use JSON, HTTP, SQL projection, an OpenSearch facade, or an
internal compatibility protocol.

The operation is read-only. One request captures one committed all-engine root
set, executes BM25 matching against that physical search root, and returns the
visible CSN with canonical ordered hits.

## Request

A `SEARCH` frame with opcode `1` means lexical `MATCH`. Its 28-byte fixed
header is followed by the UTF-8 query:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | search opcode `1` (`MATCH`) |
| 2 | 2 | reserved zero bytes |
| 4 | 16 | nonzero search object ID, little-endian `u128` |
| 20 | 4 | query byte length, little-endian `u32` |
| 24 | 4 | maximum hit count, little-endian `u32` |
| 28 | query length | UTF-8 query bytes |

The query may be empty and is capped at 4,096 encoded bytes. An empty query or
one that analyzes to no terms returns an empty result rather than a failure.
The hit limit must be in `1..=1,024`. The declared and physical payload lengths
must match exactly. Zero object identity, invalid UTF-8, a larger query or
limit, a nonzero reserved byte, unknown opcode, or trailing data is
noncanonical and returns `FAILURE(InvalidRequest)`.

The 4,096-byte request bound is a protocol CPU/memory guard, not a guarantee
that every analyzed term fits a physical search key. An oversized analyzed
term fails through the engine boundary.

## Result

A successful request returns one `VALUE` frame. Its 16-byte result header is
followed by zero or more canonical hit records:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | value tag `1` (`MATCH_RESULTS`) |
| 2 | 2 | reserved zero bytes |
| 4 | 4 | hit count, little-endian `u32` |
| 8 | 8 | nonzero visible CSN, little-endian `u64` |

Each hit begins with:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 4 | document-ID length, little-endian `u32` |
| 4 | 8 | positive finite BM25 score as little-endian IEEE-754 `f64` bits |
| 12 | document-ID length | binary document ID |

Empty binary document IDs remain valid because the native search engine admits
them. Scores must be finite and strictly positive. Hits are strictly ordered
by descending `f64::total_cmp` score and then ascending binary document ID.
Duplicate or out-of-order records are noncanonical. Hit count cannot exceed
1,024.

The visible CSN is the single root-set snapshot used for the complete search.
It is not a per-hit version. The response preserves the request's stream ID
and request ID.

The encoder computes the complete checked length before growing its reusable
buffer. If the response cannot fit the negotiated frame maximum, the server
returns `FAILURE(ResponseTooLarge)` and sends no partial hit batch. Streaming,
pagination cursors, field materialization, highlighting, facets, score
explanations, and proof payloads remain later versioned operations.

## Execution

The general local data session:

1. completes the existing minimal `HELLO`/`WELCOME`;
2. accepts `PING`, scalar `GET`/`SET`/`TTL`, lexical `MATCH`, and `CLOSE`
   serially;
3. preflights room for the fixed result header before search execution;
4. captures one current committed root set;
5. executes `NativeDatabase` physical inverted-index matching without
   materializing unrelated engines;
6. encodes the exact visible CSN and ordered hits; and
7. permits the next request after every request-local failure.

The search request does not sample the TTL clock. Renaming the experimental
structure-only session handle to `LocalDataSession` is part of this slice;
there is no released compatibility alias to preserve.

## Stable failures

The existing four-byte failure payload is reused:

| Condition | Stable failure |
|---|---|
| malformed request, invalid identity/UTF-8/limit/order | `InvalidRequest` |
| unknown collection, oversized term, corrupt search state/root/page/blob | `EngineFailure` |
| result header or complete hit batch exceeds frame maximum | `ResponseTooLarge` |
| `SEARCH` in an invalid session state | `UnexpectedFrame` |

No failure exposes stored text, document contents, filesystem paths, query
terms, page identity, or internal error text.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before public request/result codecs and the
  general local data session exist;
- golden empty-query, one-hit, tied-score, and binary-document-ID bytes;
- every truncated boundary plus version, opcode, reserved byte, zero object,
  invalid UTF-8, query length, hit limit, result tag, zero CSN, hit count,
  score, ordering, duplicate, record length, and trailing-byte rejection;
- exact 4,096/4,097-byte query and 1,024/1,025-hit boundaries;
- response-capacity rejection before physical search execution;
- current physical search equivalence for empty, missing-term, rare-term,
  common-term, and stable tie-order queries;
- malformed, unknown-index, and response-too-large failures followed by a
  successful request on the same connection;
- exact stream ID, request ID, visible CSN, document IDs, and score bits;
- close, reopen, and physical-result equivalence;
- hosted Linux, macOS, and Windows compilation/test evidence; and
- direct-Linux release observations separating embedded physical `MATCH` from
  persistent UDS `MATCH`.

The first latency receipt is an observation, not a regression threshold or G7
closure. It must disclose corpus size, term distribution, result count,
response bytes, warmup, sample count, CPU affinity, virtualization,
filesystem, maxima, and missing cold/saturation/allocation/hardware-counter
lanes.

The matched
[direct-Linux evidence](../gates/evidence/native-local-search-match-linux-2026-08-03.md)
binds canonical codecs, physical execution, request-local recovery, reopen
equivalence, and separately measured embedded/PING/UDS distributions to exact
commits and raw receipt hashes.

## Boundary

Passing this slice provides one native lexical `MATCH` over the Unix local
transport. It does not add document mutation, fielded search, boolean/phrase/
prefix syntax, ANN or hybrid search, aggregation, pagination, explicit
transactions, SQL, Windows named pipes, the complete G6 daemon, or the full
G0/G1/G4/G7 evidence.
