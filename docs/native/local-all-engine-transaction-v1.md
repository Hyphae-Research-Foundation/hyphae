# Native local all-engine transaction v1

Status: normative implemented contract with direct-Linux baseline evidence;
the lexical replacement/deletion extension has direct-Linux implementation
and performance receipts and awaits its hosted-stack receipts.

This contract exposes one explicit serial local transaction over the detached
`NativeDeltaWriteBatch`. One batch captures a single immutable all-engine
snapshot, stages relational SQL DML, scalar structure `SET`, and lexical
document creation/replacement/deletion in process, and publishes every
affected engine root under one native commit sequence number.

The local transport is not an engine-to-engine path. It neither serializes
internal engine calls nor introduces another transaction coordinator.

## Authority and identity

`BEGIN` creates a detached optimistic batch without acquiring the writer
guard. The local session assigns a nonzero, monotonically increasing `u64`
handle that is never reused on that connection.

The handle is not a WAL `TransactionId`. Reserving a durable transaction
identity at `BEGIN` would create holes for rollback and disconnected clients.
Hyphae allocates the nonzero `u128` WAL identity only when `COMMIT` reaches
writer admission. A successful commit receipt carries both identities.

The first slice admits one active transaction per serial session. It fixes:

- one server-authoritative logical time sampled exactly once at `BEGIN`;
- one durability class for the complete transaction;
- one immutable read CSN, encoded as zero only for a virgin database;
- at most 1,024 successfully staged client operations; and
- one current catalog snapshot inherited by the detached native batch.

Memory (`0`) and strict (`1`) durability are accepted. Group (`2`) durability
requires the later scheduling contract and is rejected without opening a
transaction.

## State machine

| Current state | Frame | Result |
|---|---|---|
| idle | `BEGIN` | open one batch and return `BEGUN` |
| idle | transaction-bound `EXECUTE`/`STRUCTURE`/`SEARCH` | `TransactionInactive` |
| idle | `COMMIT`/`ROLLBACK` | `TransactionInactive` |
| active | matching transaction-bound mutation | stage once and return `STAGED` |
| active | non-transaction operation or another `BEGIN` | `TransactionActive` |
| active | mutation/commit/rollback with another handle | `TransactionMismatch` |
| active | matching nonempty `COMMIT` with exact stage count | publish or fail once |
| active | matching `ROLLBACK` | discard the complete batch |
| active | `CLOSE`, peer loss, or transport loss | discard the complete batch |

`PING` remains available in either state and never observes or changes the
batch. The initial protocol exposes no transaction-private reads. Prepared
`SELECT`, scalar `GET`/`TTL`, and lexical `MATCH` are rejected while active
rather than returning committed state that could be mistaken for
read-your-writes.

Every request-local decode, handle, resource, or engine-semantic failure
preserves the active batch and its operation count. A commit attempt consumes
the batch through the opaque `NativeCommitBatch` commit envelope.
Success returns to idle. A write conflict also returns to idle and requires a
new `BEGIN`.

## BEGIN request

A `BEGIN` frame carries this fixed eight-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | opcode `1` (`BEGIN`) |
| 2 | 1 | durability: memory `0` or strict `1` |
| 3 | 1 | reserved zero |
| 4 | 4 | reserved zero |

The server preflights the fixed receipt, samples the clock once, creates the
detached batch, and only then advances the local handle sequence.

## BEGUN receipt

Success returns a `RECEIPT` frame with this fixed 32-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | receipt tag `1` (`BEGUN`) |
| 2 | 1 | selected durability |
| 3 | 1 | reserved zero |
| 4 | 8 | nonzero session-local handle, little-endian `u64` |
| 12 | 8 | read CSN, or zero before the first commit |
| 20 | 8 | fixed server logical time, little-endian `i64` |
| 28 | 4 | reserved zero |

The response preserves the request stream and request identities.

## Transaction-bound SQL DML

An `EXECUTE` frame with opcode `2` stages one existing Hyphae SQL `INSERT`,
`UPDATE`, or `DELETE`. DDL, `SELECT`, `EXPLAIN`, transaction-control SQL, and
multiple statements are invalid in this slice.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | SQL payload version `1` |
| 1 | 1 | execute opcode `2` (`TRANSACTION_DML`) |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 4 | SQL byte length, little-endian `u32` |
| 16 | 4 | parameter count, little-endian `u32` |
| 20 | 4 | reserved zero |
| 24 | SQL length | nonempty UTF-8 SQL |
| next | variable | canonical scalar parameter records |

Scalar records are exactly those in
[native local SQL SELECT v1](local-sql-select-v1.md). SQL remains bounded to
65,536 bytes and 1,024 parameters. A statement is atomic inside the private
batch: any SQL error leaves all earlier staged operations intact and adds no
partial mutation.

The transaction path parses DML per call. It is a convergence proof, not the
eventual prepared-DML performance surface or a G2 closure.

## Transaction-bound scalar SET

A `STRUCTURE` frame with opcode `4` stages one scalar `SET`:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | structure payload version `1` |
| 1 | 1 | opcode `4` (`TRANSACTION_SET`) |
| 2 | 1 | expiry mode: persistent `0` or relative `1` |
| 3 | 1 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 4 | key byte length, little-endian `u32` |
| 16 | 4 | value byte length, little-endian `u32` |
| 20 | 8 | positive relative TTL micros, or zero when persistent |
| 28 | 4 | reserved zero |
| 32 | key length | binary scalar key |
| next | value length | binary scalar value |

The key retains the physical scalar bound of 4,095 bytes. Empty keys and
values remain valid. Relative TTL is added to the fixed `BEGIN` logical time;
staging never samples the server clock again.

## Transaction-bound lexical documents

A `SEARCH` frame with opcode `2` creates one lexical document. Opcode `3`
replaces one exact live document and uses the same payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | search payload version `1` |
| 1 | 1 | opcode `2` (`TRANSACTION_INDEX_DOCUMENT`) or `3` (`TRANSACTION_REPLACE_DOCUMENT`) |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 16 | nonzero search collection `ObjectId` |
| 28 | 4 | document-ID byte length, little-endian `u32` |
| 32 | 4 | text byte length, little-endian `u32` |
| 36 | 4 | reserved zero |
| 40 | document-ID length | binary document identity |
| next | text length | UTF-8 document text |

Opcode `4` deletes one exact live document with a shorter body:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | search payload version `1` |
| 1 | 1 | opcode `4` (`TRANSACTION_DELETE_DOCUMENT`) |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 16 | nonzero search collection `ObjectId` |
| 28 | 4 | document-ID byte length, little-endian `u32` |
| 32 | 4 | reserved zero |
| 36 | document-ID length | binary document identity |

The local limits are 4,079 document-ID bytes and 65,536 text bytes, subject to
the stricter existing physical term/document key checks. The search collection
must already exist in the captured catalog. Empty document IDs and empty text
remain valid when physical identity checks admit them. Replacement and
deletion require a live document. Every semantic failure preserves earlier
staged operations and the next ordinal.

## STAGED receipt

Every successful transaction-bound mutation returns one fixed 32-byte
`RECEIPT`:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | receipt tag `2` (`STAGED`) |
| 2 | 1 | engine: relational `1`, structure `2`, search `3` |
| 3 | 1 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 8 | one-based staged-operation ordinal |
| 20 | 8 | SQL rows affected; `1` for structure/search |
| 28 | 4 | reserved zero |

The session preflights this receipt before mutating the batch. Ordinals advance
only after a complete successful engine operation and stop at 1,024.

## COMMIT request and receipt

A `COMMIT` frame carries this fixed 24-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | opcode `1` (`COMMIT`) |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 8 | exact expected staged-operation count |
| 20 | 4 | reserved zero |

The count must be in `1..=1,024` and equal the session count. A mismatch
preserves the active batch; zero returns `TransactionEmpty`.

Success returns a fixed 40-byte `RECEIPT`:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | receipt tag `3` (`COMMITTED`) |
| 2 | 1 | satisfied durability |
| 3 | 1 | reserved zero |
| 4 | 8 | session-local handle |
| 12 | 16 | nonzero WAL `TransactionId`, little-endian `u128` |
| 28 | 8 | single all-engine commit CSN, little-endian `u64` |
| 36 | 4 | staged-operation count, little-endian `u32` |

The response is emitted only after the selected durability promise is
satisfied. The commit uses the existing conflict validation, WAL transaction,
engine-root staging, and one atomic root-manifest publication.

## ROLLBACK request and receipt

A `ROLLBACK` frame carries this fixed 16-byte payload:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | opcode `1` (`ROLLBACK`) |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 4 | reserved zero |

Success returns a fixed 24-byte `RECEIPT`:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | transaction payload version `1` |
| 1 | 1 | receipt tag `4` (`ROLLED_BACK`) |
| 2 | 2 | reserved zero |
| 4 | 8 | session-local handle |
| 12 | 8 | discarded staged-operation count |
| 20 | 4 | reserved zero |

Rollback never allocates a WAL transaction identity, writes WAL/pages/blobs,
or advances the visible CSN.

## Stable failures

The local failure payload gains these codes:

| Code | Name | Condition |
|---:|---|---|
| 13 | `TransactionActive` | another or non-transaction operation arrived while active |
| 14 | `TransactionInactive` | a transaction-bound frame arrived while idle |
| 15 | `TransactionMismatch` | handle or expected stage count differs |
| 16 | `TransactionEmpty` | commit was requested without a staged operation |
| 17 | `TransactionResourceLimit` | local handle or 1,024-operation bound exhausted |
| 18 | `TransactionConflict` | optimistic commit lost first-committer-wins validation |

Malformed payloads remain `InvalidRequest`; SQL semantics use the existing SQL
failure classes; relative expiry overflow remains `ExpiryOverflow`; a too-small
complete receipt remains `ResponseTooLarge`; and other physical/native failures
remain `EngineFailure`. Failure payloads expose no SQL text, parameters, keys,
values, document text, filesystem path, or internal error.

## Verification gates

The implementation gate requires:

- a compiler-reaching red test before public transaction codecs/session state
  exist;
- golden BEGIN, BEGUN, each engine mutation including all three lexical
  lifecycle payloads, STAGED, COMMIT, COMMITTED,
  ROLLBACK, ROLLED_BACK, and every new failure-code byte sequence;
- every truncation boundary plus version, opcode, tag, engine, durability,
  reserved, handle, identity, count, UTF-8, scalar, length, TTL, and trailing
  byte rejection;
- exact 65,536/65,537-byte SQL/text, 1,024/1,025-parameter/operation, 4,095/
  4,096-byte scalar key, and 4,079/4,080-byte document-ID boundaries;
- idle/active state-transition, wrong-handle, expected-count, duplicate-BEGIN,
  empty-commit, automatic-close rollback, and peer-loss rollback tests;
- proof that response preflight occurs before clock sampling, handle
  allocation, staging, commit, or rollback;
- proof that the clock is sampled once at BEGIN and relative TTL uses that
  fixed logical time;
- one strict transaction that stages SQL `INSERT`, scalar `SET`, and lexical
  document indexing, then proves prior-snapshot invisibility, one receipt CSN,
  current-snapshot visibility in all three engines, exact WAL transaction
  identity, reopen equivalence, and no mixed root set;
- semantic failure followed by successful reuse of the same active batch;
- optimistic conflict with no partial engine publication;
- all seven deterministic commit interruption boundaries over a three-engine
  detached batch, with reopen showing either the complete prior root set or
  the complete new root set and never a mixed state;
- hosted Linux, macOS, and Windows compile/test evidence; and
- a direct-Linux bounded latency receipt that reports stage and commit
  separately without subtracting independent distributions.

## Boundary

Passing this slice proves one minimal all-engine transaction and advances G5.
It does not provide prepared DML, DDL in the local transaction, private reads,
savepoints, isolation-level selection, deallocation, concurrent transactions
on one connection, group durability, multiplexing, retry tokens, process-kill
crash evidence, Windows named pipes, or complete G2/G5/G6/G7 evidence.
