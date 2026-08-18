<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native local structure SET and TTL v1

Status: implemented over the serial UDS session; direct-Linux evidence
recorded; complete local-product and performance gates remain open.

This contract extends the first engine-bearing local session with one native
scalar mutation and one TTL query. It does not add a compatibility command
language, an internal engine hop, a general transaction protocol, or a
complete local daemon.

`SET` creates one serialized native transaction, mutates the current physical
structure root, and returns a commit receipt only after the selected durability
promise is satisfied. `TTL` is a read-only physical query. Both reuse the
canonical `HYPHLCL1` frame header and preserve the request's stream ID and
request ID.

## Operation dispatch

The first structure opcodes are:

| Opcode | Operation |
|---:|---|
| 1 | `GET` |
| 2 | `SET` |
| 3 | `TTL` |

Unknown opcodes are request-local `FAILURE(InvalidRequest)` results.

`GET` retains the exact payload and value contract in
[native local structure GET v1](local-structure-get-v1.md).

## TTL request

The `TTL` request uses the same compact point-read layout:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | structure opcode `3` (`TTL`) |
| 2 | 2 | reserved zero bytes |
| 4 | 4 | key length, little-endian `u32` |
| 8 | key length | binary key |

The key ceiling is 4,095 bytes so the scalar namespace remains within the
native B+tree's 4,096-byte canonical limit. Declared and physical lengths must
match exactly.

## SET request

`SET` has one fixed 20-byte header followed by the binary key and value:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | structure opcode `2` (`SET`) |
| 2 | 1 | requested durability |
| 3 | 1 | expiry mode |
| 4 | 4 | key length, little-endian `u32` |
| 8 | 4 | value length, little-endian `u32` |
| 12 | 8 | relative TTL in microseconds, little-endian `i64` |
| 20 | key length | binary key |
| 20 + key length | value length | binary value |

The admitted durability bytes are:

| Byte | Meaning | First-slice behavior |
|---:|---|---|
| 1 | strict | acknowledge after page and WAL synchronization |
| 2 | group | reject as `UnsupportedDurability` |
| 3 | memory | publish without a crash-durability acknowledgement |

Group durability is encoded but not simulated by a serialized singleton
commit. It remains rejected until the local product is attached to the native
group-commit scheduler.

Expiry mode `0` means persistent and requires the TTL field to be zero.
Expiry mode `1` means relative TTL and requires a strictly positive TTL. The
client never supplies absolute server time. After successful decode, the
server samples its injected absolute-microsecond clock once and computes:

```text
expires_at_micros = server_now + relative_ttl_micros
```

Overflow returns `FAILURE(ExpiryOverflow)` before a transaction begins. A
zero or negative relative TTL is noncanonical rather than an immediate-delete
shortcut. Absolute expiry, `EXPIRE`, `PERSIST`, conditional `NX`/`XX`, and
keep-existing-TTL semantics remain later operations.

The complete request must fit the negotiated frame maximum. Length arithmetic
is checked before buffer growth. Empty binary keys and values remain valid.

## Commit receipt

A successful `SET` returns a `RECEIPT` frame with this fixed 28-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | receipt tag `1` (`SET_COMMITTED`) |
| 2 | 1 | acknowledged durability |
| 3 | 1 | reserved zero byte |
| 4 | 16 | nonzero transaction ID, little-endian `u128` |
| 20 | 8 | nonzero commit CSN, little-endian `u64` |

The receipt's durability must equal the admitted request. The CSN is the same
native commit sequence shared by every root affected by that transaction.
This first receipt omits catalog version, LSN, WAL digest, timing breakdown,
and proof material; those remain available inside the native runtime and can
be added only by a versioned payload.

The session rejects a `SET` before sampling time or opening a transaction when
the negotiated frame maximum cannot hold the complete 28-byte receipt. No
success frame is sent before native commit publication. A transport failure
after publication but before the client receives the receipt is an ambiguous
acknowledgement: the client must reconnect and read state. Request replay and
idempotency tokens remain open session work.

## TTL response

A successful `TTL` query returns a `VALUE` frame with this fixed 12-byte
payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | operation payload version `1` |
| 1 | 1 | TTL tag: `0` missing, `1` persistent, `2` remaining |
| 2 | 2 | reserved zero bytes |
| 4 | 8 | remaining microseconds, little-endian `i64` |

Missing and persistent responses require a zero duration. Remaining requires
a strictly positive duration. Expiry is evaluated at the server's single
clock sample for that request; an exactly due key is missing.

## Stable failures

The failure payload remains the fixed four-byte v1 payload. This slice retains
codes 1 through 5 and adds:

| Code | Meaning |
|---:|---|
| 6 | requested durability is unsupported by this session |
| 7 | relative TTL cannot be represented as absolute server time |

Malformed version, opcode, durability, expiry mode, TTL, length, or identity
is request-local. Native begin, mutation, commit, physical TTL, or physical
GET failures map to `EngineFailure` without exposing filesystem paths,
internal text, WAL material, or stored values. A request-local failure does
not terminate the connection.

## Serial mutation session

The session remains deliberately bounded:

1. empty stream-0 `HELLO` receives matching `WELCOME`;
2. `PING`, `GET`, `SET`, `TTL`, and `CLOSE` execute serially;
3. each valid engine request samples server time once;
4. one `SET` owns one serialized native transaction and one commit CSN;
5. every response preserves stream ID and request ID; and
6. request-local failures permit the next complete frame to execute.

The session owns exclusive mutable access to one `NativeDatabase` for its
lifetime and replaces the experimental GET-specific session handle before
release. Pipelining, multiplexed execution, concurrent group admission,
explicit `BEGIN`/`COMMIT`, cancellation, deadlines, authorization, and
Windows named pipes remain separate contracts.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before the public SET/TTL codecs and general
  structure session exist;
- golden persistent SET, expiring SET, TTL request, commit receipt, and all TTL
  response encodings;
- every truncated boundary plus version, opcode, durability, expiry-mode,
  TTL, key/value length, trailing-byte, zero-identity, and reserved-byte
  rejection;
- exact-limit and one-past-limit key coverage;
- strict and memory SET receipts with exact transaction ID, CSN, durability,
  stream ID, and request ID;
- receipt-capacity rejection before clock sampling or physical mutation;
- persistent, remaining, exactly expired, and missing TTL responses under a
  controlled server clock;
- strict close, database reopen, and physical value/TTL equivalence;
- unsupported group durability, expiry overflow, kind mismatch, malformed
  request, and successful same-connection recovery;
- hosted Linux, macOS, and Windows compilation/test evidence; and
- direct-Linux release observations that separate memory SET, strict SET,
  physical TTL execution, persistent TTL round trip, and GET control traffic.

Mutation latency must separate memory publication from strict physical
durability. The first receipt is an observation, not a regression threshold
or G7 closure. It must disclose mutation cardinality, key/value size, warmup,
sample count, CPU affinity, virtualization, filesystem, maxima, and missing
cold/saturation/allocation/hardware-counter lanes.

The matched
[direct-Linux evidence](../gates/evidence/native-local-structure-set-ttl-linux-2026-08-03.md)
binds the codecs, serial execution, recovery behavior, and five separately
measured latency surfaces to exact commits and raw receipt hashes. The
observed mutation routes do not meet a microsecond mutation objective and do
not establish a regression threshold.

## Boundary

Passing this slice provides native scalar `GET`, `SET`, and TTL semantics over
the Unix local transport. It does not implement explicit expiry mutation,
conditional/batched writes, request replay, local transaction state, group
scheduling, SQL, search, Windows named pipes, the complete G6 daemon, or the
full G0/G1/G3/G7 evidence.
