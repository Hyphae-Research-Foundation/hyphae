# Native local SQL SELECT v1

Status: implemented for the experimental serial UDS session; the
compiler-reaching red gate and [direct-Linux evidence][sql-linux-evidence]
were recorded on 2026-08-03. DDL, DML, explicit transactions, streaming, and
the complete G2/G6/G7 evidence remain pending.

This contract exposes the existing catalog-bound, physical current-root SQL
`SELECT` executor through the serial `HYPHLCL1` local session. A client
prepares UTF-8 SQL once and executes the resulting session-local plan with
canonical typed parameters. Results carry a complete logical schema, typed
rows, and the single all-engine CSN observed by execution.

The protocol does not send JSON, return delimited text, open an internal
network hop, delegate to another SQL engine, or materialize unrelated native
engines.

## Resource bounds

The first SQL surface is intentionally bounded:

- one SQL statement is at most 65,536 UTF-8 bytes;
- one session retains at most 64 prepared statements;
- one execution carries at most 1,024 parameters;
- one result has at most 1,024 columns and 1,024 rows;
- one logical-type descriptor is at most 256 bytes; and
- every complete request or response must fit the negotiated frame maximum.

`PREPARE` rejects a plan when the binder cannot prove a result-row upper bound
or when that bound exceeds 1,024. A primary-key or unique-index lookup has an
upper bound of one. Scan, range, non-unique-index, and join plans require the
existing bounded access path. This prevents the local session from
materializing an unbounded result while pagination and row streaming remain
unimplemented.

Plan IDs are nonzero, increase monotonically within one session, and are never
reused. `CLOSE` releases every plan. The initial slice has no deallocation
operation; reaching the 64-plan bound returns a stable resource failure and
does not evict an earlier plan.

## PREPARE request

A `PREPARE` frame with opcode `1` binds one `SELECT`:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | SQL payload version `1` |
| 1 | 1 | prepare opcode `1` (`SELECT`) |
| 2 | 2 | reserved zero bytes |
| 4 | 4 | statement byte length, little-endian `u32` |
| 8 | statement length | UTF-8 SQL bytes |

The declared and physical lengths must match exactly. Empty SQL, invalid
UTF-8, trailing bytes, an unknown opcode, or a statement above 65,536 bytes is
noncanonical. The server calls `prepare_sql_latest`; DDL, DML, unsupported
syntax, absent objects, and access paths without an implemented physical plan
return a stable SQL-invalid failure.

## PREPARE receipt

Success returns one `RECEIPT` frame with this fixed 32-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | SQL payload version `1` |
| 1 | 1 | receipt tag `2` (`SQL_PREPARED`) |
| 2 | 2 | reserved zero bytes |
| 4 | 8 | nonzero session-local plan ID, little-endian `u64` |
| 12 | 8 | nonzero bound catalog version, little-endian `u64` |
| 20 | 4 | parameter count, little-endian `u32` |
| 24 | 4 | result column count, little-endian `u32` |
| 28 | 4 | proven maximum result rows, little-endian `u32` |

The response preserves the request's stream ID and request ID. The server
preflights room for the fixed receipt before binding or retaining a plan.

## EXECUTE request

An `EXECUTE` frame with opcode `1` invokes one retained plan:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | SQL payload version `1` |
| 1 | 1 | execute opcode `1` (`PREPARED_SELECT`) |
| 2 | 2 | reserved zero bytes |
| 4 | 8 | nonzero session-local plan ID, little-endian `u64` |
| 12 | 4 | parameter count, little-endian `u32` |
| 16 | remaining | exactly `parameter count` scalar records |

Each parameter scalar is self-describing:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | scalar tag |
| 1 | 3 | reserved zero bytes |
| 4 | 4 | scalar byte length, little-endian `u32` |
| 8 | value length | canonical scalar bytes |

The tags are `0` null, `1` boolean, `2` signed `i64`, `3` unsigned `u64`,
`4` decimal coefficient `i128`, `5` canonical `f32`, `6` canonical `f64`,
`7` UTF-8 text, `8` binary, `9` date `i32`, `10` time `u64`, `11` timestamp
`i64`, `12` interval, and `13` UUID. Fixed-width payloads use little-endian
bytes; interval is months `i32`, days `i32`, then nanoseconds `i64`.

Null requires zero bytes. Boolean accepts only one byte `0` or `1`. Float
payloads reject noncanonical NaN payloads and negative zero; time validates
the native nanoseconds-from-midnight domain; and text validates UTF-8.
`ScalarValue` deliberately carries integer and decimal values independently
of their bound column width/scale, so the prepared executor remains the
authority for column-domain validation. JSON, array, map, and vector scalar
parameters remain unsupported in this version.

The parameter count must equal the bound plan exactly. Unknown plan IDs,
noncanonical scalars, type mismatch, null violation, and invalid key binding
are request-local failures.

## Row result

Success returns one `VALUE` frame. Its 20-byte header precedes the schema and
row-major cells:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | SQL payload version `1` |
| 1 | 1 | value tag `2` (`SQL_ROWS`) |
| 2 | 2 | reserved zero bytes |
| 4 | 8 | nonzero visible CSN, little-endian `u64` |
| 12 | 4 | column count, little-endian `u32` |
| 16 | 4 | row count, little-endian `u32` |

Each column descriptor is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 4 | UTF-8 column-name length, little-endian `u32` |
| 4 | 4 | logical-type descriptor length, little-endian `u32` |
| 8 | name length | column-name bytes |
| 8 + name length | type length | canonical `LogicalType` descriptor |

Each row contains exactly `column count` cells. A cell is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | value tag: `0` null, `1` present |
| 1 | 3 | reserved zero bytes |
| 4 | 4 | scalar byte length, little-endian `u32` |
| 8 | value length | canonical storage bytes for the column logical type |

Null requires zero scalar bytes. Present cells reuse
`ScalarValue::encode_storage` and `decode_storage` against the declared
column type. The complete schema and all rows are length-checked before the
encoder grows its reusable response buffer. A response that does not fit the
frame bound returns `FAILURE(ResponseTooLarge)` with no partial schema or row
batch.

The visible CSN is captured with the same immutable physical root set used by
the complete execution. It is not a prepare-time catalog version or a
per-row version.

## Stable failures

The existing four-byte local failure payload gains these SQL codes:

| Code | Name | Condition |
|---:|---|---|
| 8 | `SqlInvalid` | non-SELECT SQL, syntax/bind failure, or unsupported access path |
| 9 | `SqlParameters` | parameter count, type, null, or key-binding mismatch |
| 10 | `SqlCatalogChanged` | the prepared catalog version is no longer current |
| 11 | `SqlResourceLimit` | plan-table or statically proven row/column bound exceeded |
| 12 | `UnknownPrepared` | plan ID is not retained by this session |

Malformed payloads remain `InvalidRequest`; native page, catalog, root, or I/O
failure remains `EngineFailure`; an oversized complete result remains
`ResponseTooLarge`; and SQL frames in an invalid session state remain
`UnexpectedFrame`. No failure exposes SQL text, parameter values, stored
values, filesystem paths, or internal error strings.

Every request-local failure preserves the connection and all earlier prepared
plans.

## Execution

The serial local session:

1. preflights the fixed response header or receipt;
2. decodes and validates the complete request;
3. binds `PREPARE` against only the current catalog;
4. retains the plan, result schema, catalog version, and proven row bound;
5. captures one immutable current root set for `EXECUTE`;
6. rejects a stale catalog binding before row traversal;
7. executes the prepared physical primary/secondary B+tree access path;
8. verifies executor columns and row bounds against retained metadata; and
9. emits one complete canonical response or one stable failure.

SQL requests do not sample the structure TTL clock. No engine-to-engine path
uses the local transport.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before public SQL codecs/session behavior
  exist;
- golden PREPARE, receipt, zero-parameter EXECUTE, every primitive parameter,
  empty result, null cell, and multi-row result bytes;
- every truncated boundary plus version, opcode, tag, reserved, identity,
  UTF-8, length, count, type-descriptor, scalar, CSN, schema, row-shape, and
  trailing-byte rejection;
- exact 65,536/65,537-byte statement, 1,024/1,025-parameter,
  1,024/1,025-column, 1,024/1,025-row, and 64/65-plan boundaries;
- prepare rejection for unbounded or over-limit physical plans;
- primary-key, unique/non-unique secondary, bounded scan/range, null, empty,
  and join equivalence against embedded physical execution;
- unknown-plan, malformed, parameter, stale-catalog, engine, resource, and
  response-too-large failures followed by successful reuse of the same
  connection and retained plan;
- exact stream ID, request ID, plan ID, catalog version, visible CSN, schema,
  scalar bytes, row order, and reopen behavior;
- hosted Linux, macOS, and Windows compile/test evidence; and
- direct-Linux release observations separating embedded prepared execution,
  persistent `PING`, and persistent UDS `EXECUTE`.

The first latency receipt is an observation, not a regression threshold or G7
closure. It must disclose data shape, access path, tree height, request/result
bytes, warmup, sample count, CPU affinity, virtualization, filesystem,
maxima, and missing cold/saturation/allocation/hardware-counter lanes.
Independent percentile distributions are not subtracted.

## Boundary

Passing this slice provides native prepared `SELECT` over the Unix local
transport. It does not expose DDL or DML over the protocol, SQL transactions,
deallocation, pagination, streaming row batches, cancellation, Windows named
pipes, authorization, or the complete G2/G6/G7 evidence. Those remain
separate contract-first slices.

[sql-linux-evidence]: ../gates/evidence/native-local-sql-select-linux-2026-08-03.md
