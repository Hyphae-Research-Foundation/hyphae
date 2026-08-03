# Native local UDS transport v1

Status: implemented for Unix; transport-only and first engine-bearing
direct-Linux receipts recorded. Windows named pipe and complete session
semantics remain pending.

This contract adds the first real transport beneath Hyphae's native local
protocol. It carries canonical `HYPHLCL1` frames over one Unix domain socket
without TCP, HTTP, JSON, an external engine, or an engine-to-engine protocol
hop. Embedded engine calls remain direct Rust calls.

This is a G1 transport vertical, not the complete G6 daemon. Windows named
pipes, authorization policy, general session state, multiplexing, streaming,
flow control, cancellation, and all database operation payloads beyond the
first scalar structure `GET` remain separate contracts.

## Portable framed I/O

`LocalFrameIo` owns one reusable receive buffer and one reusable send buffer.
It is independent from the underlying byte stream and exposes:

```text
LOCAL_FRAME_IO(maximum_payload)
SEND(kind, stream_id, request_id, payload)
RECEIVE() -> EOF | DecodedFrame
```

Construction rejects a maximum above
`DEFAULT_MAX_FRAME_PAYLOAD = 16 MiB`. A zero-byte maximum remains valid for
control frames.

`RECEIVE` reads exactly the 32-byte canonical header before trusting the
declared payload length. It rejects a declared payload above the configured
maximum before resizing its buffer. Clean EOF is returned only when no header
byte was received. EOF within a header or payload is a typed truncation
failure. The complete frame then passes the existing preamble, flags, kind,
length, and CRC32C validation.

`SEND` encodes into its reusable buffer and writes the complete frame. A
successful return means every encoded byte was handed to the local stream; it
does not imply engine execution or physical durability.

## Unix domain socket ownership

`UdsFrameListener::bind(path, maximum_payload)`:

- requires the path not to exist;
- never unlinks or replaces an existing filesystem object;
- binds one filesystem-backed Unix domain socket;
- sets its mode to owner read/write only (`0600`);
- records the bound socket identity; and
- removes the endpoint on drop only when the current path is still that exact
  socket identity.

The parent directory and its permissions are supplied by the embedding host.
This slice does not silently create or chmod the parent.

`close` performs the same identity check and reports cleanup failure
explicitly. Drop uses the identical check as best-effort crash hygiene when
`close` was not called.

`accept` and `UdsFrameConnection::connect` return one blocking framed
connection with independent reusable buffers. One connection preserves byte
and frame order. OS errors retain their `ErrorKind` through the typed
transport error.

## Session used by the receipt

The latency receipt uses a minimal deterministic session over one persistent
connection:

1. the client sends an empty `HELLO`;
2. the server returns an empty `WELCOME` with the same request ID;
3. the client sends bounded `PING` frames;
4. the server returns the same `PING` payload, stream ID, and request ID; and
5. the client sends `CLOSE`, which the server echoes before shutdown.

This session exists to prove framing and UDS transport. It does not negotiate
the complete production handshake or authorize database operations.

The follow-on
[native local structure GET](local-structure-get-v1.md) session retains the
same minimal handshake and persistent `PING` control, then executes canonical
scalar `GET` requests against the current physical structure root. That
engine-bearing extension is serial and bounded; it does not implement the
complete production session.

The next
[native local structure SET and TTL](local-structure-set-ttl-v1.md) extension
uses the same serial connection for one-transaction scalar mutations and
physical TTL reads. It admits strict and memory durability, rejects group
durability until scheduler integration, and returns exact transaction/CSN
receipts. It still is not the complete production session.

## Failure behavior

The implementation must fail closed for:

- an existing endpoint path of any kind;
- a maximum above the protocol-wide limit;
- partial headers and payloads;
- oversized declared payloads before payload allocation;
- invalid magic, version, flags, kind, length, or checksum;
- write, flush, accept, connect, permission, and endpoint-cleanup failures;
  and
- a receipt session that changes kind, stream ID, request ID, or payload.

No error path may delete an endpoint it did not create.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before the public transport types exist;
- portable fragmented-read, clean-EOF, truncation, maximum, checksum, and
  send-buffer reuse tests;
- Unix round-trip tests for `HELLO`/`WELCOME`, `PING`, and `CLOSE`;
- exact `0600` endpoint permission evidence;
- existing-file and replaced-endpoint preservation tests;
- multiple ordered frames on one persistent connection;
- Linux and macOS compilation through hosted CI;
- a direct-Linux ext4 receipt bound to exact source and harness commits; and
- round-trip p50, p95, p99, p99.9, maximum, and throughput with warmup,
  payload size, observation count, and concurrency disclosed.

The first receipt is an observation, not a universal latency SLO. It measures
client encode/write, kernel UDS transport, server read/decode/echo, client
read/decode, and scheduling together. It does not infer engine execution,
queueing, fsync, cold start, saturation, allocation, RSS, hardware counters,
named-pipe behavior, or complete G7.

The exact implementation, failure-path tests, host disclosure, three raw
release observations, and latency summary are bound by the
[direct-Linux UDS evidence](../gates/evidence/native-local-uds-linux-2026-08-03.md).

## Boundaries

Passing this slice removes the explicit “no UDS transport receipt” deficit
from the G1 minimal vertical. It does not by itself close G0, the rest of G1,
G6, or G7. One scalar structure `GET` is now exposed through the socket; SQL,
structure writes and TTL commands, search, and transaction state are not.
