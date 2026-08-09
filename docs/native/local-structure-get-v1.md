# Native local structure GET v1

Status: implemented experimentally; direct-Linux correctness and latency
evidence recorded. The complete local product and G7 matrix remain pending.

This contract carries the first real native-engine operation over Hyphae's
filesystem-backed Unix-domain transport. It reads one scalar structure value
from the current physical structure root. It is not a compatibility protocol,
an internal engine hop, a materialized-state projection, or a complete local
daemon.

The operation reuses the canonical `HYPHLCL1` frame header and the minimal
`HELLO`/`WELCOME` receipt session from
[native local UDS transport v1](local-uds-transport-v1.md). The client sends a
`STRUCTURE` frame and receives exactly one `VALUE` or `FAILURE` frame with the
same stream ID and request ID.

## Request payload

The `STRUCTURE GET` payload is canonical:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | structure opcode `1` (`GET`) |
| 2 | 2 | reserved zero bytes |
| 4 | 4 | key length, little-endian `u32` |
| 8 | key length | binary key |

The physical scalar namespace adds one byte to the supplied key. The maximum
request key is therefore 4,095 bytes so the resulting key remains within the
native B+tree's 4,096-byte canonical limit. Empty and arbitrary binary keys
are valid. Declared and physical payload lengths must match exactly.

The client does not supply logical time. The server samples its injected
absolute-microsecond clock once per request and applies the normal scalar TTL
semantics at that time.

## Value payload

The `VALUE` payload is canonical:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | value tag: `0` missing, `1` present |
| 2 | 2 | reserved zero bytes |
| 4 | 4 | value length, little-endian `u32` |
| 8 | value length | binary value |

`missing` requires zero length and no trailing bytes. `present` permits a
zero-length value, so an empty value remains distinct from an absent or
expired key. Declared and physical lengths must match exactly.

The complete payload must fit the connection's negotiated frame maximum. If
the stored value cannot fit, the server returns `FAILURE(ResponseTooLarge)`
without a partial value.

## Failure payload

The first operation slice uses one fixed four-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | stable failure code |
| 2 | 2 | reserved zero bytes |

The stable codes are:

| Code | Meaning |
|---:|---|
| 1 | malformed or unsupported operation payload |
| 2 | key exceeds the canonical structure-key limit |
| 3 | native physical read failed |
| 4 | complete value cannot fit the connection frame limit |
| 5 | frame kind is invalid for this session state |

Failure frames preserve the request's stream ID and request ID and contain no
filesystem path, internal error text, secret, or stored value. Malformed
operation payloads and engine failures are request-local: the next complete
frame may still execute. A truncated outer frame remains a transport failure.

## Minimal session

The first engine-bearing session is deliberately serial and bounded:

1. the first frame is empty `HELLO` on stream `0`;
2. the server returns empty `WELCOME` with the same request ID;
3. the connection accepts `PING`, `STRUCTURE GET`, and `CLOSE`;
4. `PING` remains an exact echo control;
5. every `STRUCTURE GET` completes before the next frame executes; and
6. `CLOSE` is echoed and ends the session.

Multiplexed execution, pipelining, authorization, transaction state, flow
control, cancellation, deadlines, writes, SQL, search, and Windows named
pipes remain separate contracts.

## Execution boundary

The server executes `NativeDatabase::get_latest_structure` against the
current physical root. It does not materialize the complete structure engine.
This first vertical may allocate the owned returned value; allocation-free
inline point responses remain an explicit performance follow-up rather than
an inferred property.

The server clock implements the existing `NativeSchedulerClock` authority so
tests and receipts can supply deterministic time while a product host can
supply real absolute time.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before the public operation/session types
  exist;
- golden request, missing, empty, present, and failure encodings;
- exact-limit and one-past-limit key tests;
- truncated, unsupported-version, reserved-byte, unknown-opcode/tag/code,
  length-divergence, and trailing-byte rejection;
- a real UDS session reading live, missing, empty, and expired scalar values
  from one native physical root;
- request-local malformed, oversized-key, and response-too-large failures
  followed by a successful request on the same connection;
- exact stream-ID and request-ID preservation;
- hosted Linux, macOS, and Windows compilation/test evidence; and
- a direct-Linux release receipt that reports embedded physical execution,
  persistent `PING`, and persistent engine-bearing `GET` separately.

The first receipt uses warm state and concurrency one. It is an observation,
not a regression threshold or G7 closure. It must disclose warmup, sample
count, value size, tree height, CPU affinity, virtualization, maxima, and all
missing cold/saturation/allocation/hardware-counter lanes.

The exact implementation, failure-path tests, host disclosure, three raw
release observations, and latency summary are bound by the
[direct-Linux structure GET evidence](../gates/evidence/native-local-structure-get-linux-2026-08-03.md).

## Boundary

Passing this slice proves one native physical engine read through the local
transport. It does not close G0, G1, G6, or G7; provide `SET`/`TTL` over the
transport; implement a general daemon; or prove the complete three-engine
protocol, transaction, crash, and performance matrices.
