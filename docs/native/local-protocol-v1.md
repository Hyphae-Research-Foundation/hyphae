# Native local protocol v1

Status: normative target contract; the portable frame and product codecs,
complete capability handshake, canonical errors, provisional completion,
flow-control, cancellation/deadline controls, multi-client UDS daemon, and safe
Windows named-pipe implementation compile path are present. The broader G6
product operation matrix and cross-platform functional receipts remain
incomplete.

The local protocol exposes native typed operations without defining internal
engine communication. Embedded calls remain direct Rust calls.

## Transports

- Unix: Unix domain socket with filesystem permissions.
- Windows: named pipe with an explicit security descriptor.
- Optional TCP loopback: disabled by default and requires authentication.

Shared-memory rings require a later accepted safety/crash ADR. No transport is
used between native engines inside the process.

## Connection handshake

The client sends `HELLO` with:

- protocol major/minor range;
- client identity and optional process metadata;
- maximum frame and in-flight limits;
- supported compression;
- authentication material when required; and
- requested database/schema.

The server returns `WELCOME` with selected version, session ID, engine
version, data-format version, limits, capabilities and catalog version.
Incompatible major versions fail before accepting operations.

## Frame header

Every frame has a 32-byte header:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYPHLCL1` |
| 8 | 2 | major version `1` |
| 10 | 1 | frame kind |
| 11 | 1 | flags |
| 12 | 4 | stream ID |
| 16 | 8 | request ID |
| 24 | 4 | payload length |
| 28 | 4 | CRC32C of header with checksum zeroed plus payload |

Payloads use canonical type encodings and little-endian fixed fields. The
default maximum is 16 MiB. Larger inputs/results use bounded `DATA` frames on
one stream with explicit total length and final digest.

The v1 kind registry is append-only:

| Code | Kind | Family |
|---:|---|---|
| 1 | `HELLO` | session |
| 2 | `WELCOME` | session |
| 3 | `PING` | session |
| 4 | `PREPARE` | plan |
| 5 | `EXECUTE` | plan |
| 6 | `BEGIN` | transaction |
| 7 | `COMMIT` | transaction |
| 8 | `ROLLBACK` | transaction |
| 9 | `STRUCTURE` | structure |
| 10 | `SEARCH` | search |
| 11 | `VALUE` | result |
| 12 | `RECEIPT` | result |
| 13 | `ERROR` | failure |
| 14 | `CANCEL` | flow |
| 15 | `CLOSE` | session |
| 16 | `DEALLOCATE` | plan |
| 17 | `EXPLAIN` | plan |
| 18 | `SAVEPOINT` | transaction |
| 19 | `DATA` | flow |
| 20 | `END` | flow |
| 21 | `WINDOW_UPDATE` | flow |
| 22 | `ROW_SCHEMA` | result |
| 23 | `ROW_BATCH` | result |
| 24 | `DEADLINE` | flow |

Codes 25 through 255 are unassigned in v1 and fail as unknown kinds. Adding a
kind uses a previously unassigned code; existing assignments never change.
Registry recognition does not imply that every corresponding product operation
is implemented.

## Message families

- session: `HELLO`, `WELCOME`, `PING`, `CLOSE`;
- plan: `PREPARE`, `EXECUTE`, `DEALLOCATE`, `EXPLAIN`;
- transaction: `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`;
- structure: typed point and structure operations;
- search: typed lexical, vector, hybrid and aggregation requests;
- flow: `DATA`, `END`, `WINDOW_UPDATE`, `CANCEL`, `DEADLINE`;
- result: `ROW_SCHEMA`, `ROW_BATCH`, `VALUE`, `RECEIPT`;
- failure: `ERROR`.

The transaction ID returned by `BEGIN` can carry SQL, structure and search
operations on the same connection/session.

The implemented engine-bearing subset now includes
[native local structure GET v1](local-structure-get-v1.md) and
[native local structure SET and TTL v1](local-structure-set-ttl-v1.md). They
use the canonical frame header and a deliberately minimal serial session.
Each `SET` owns one implicit native transaction; the complete message families,
explicit transaction state, group scheduling, and multiplexing are not
implemented.

The implemented search subset is
[native local SEARCH MATCH v1](local-search-match-v1.md). It carries one
catalog object identity and bounded UTF-8 query directly to the physical
inverted index, with the all-engine visible CSN in the canonical result.

The implemented relational subset is
[native local SQL SELECT v1](local-sql-select-v1.md). It prepares bounded
session-local plans and executes canonical typed parameters directly against
the physical current-root relational paths. Results preserve logical schema,
typed rows, and the visible all-engine CSN.

The prior serial handle remains available for G1 evidence. The G6
`hyphae-native-daemon` adapter dispatches admitted operations through the sole
`NativeProductService` owner; no engine behavior is implemented in the daemon.

## Multiplexing and flow control

Request IDs are unique per connection while active. Stream IDs allow
interleaved results. Each stream has a byte window; producers stop when the
window is exhausted. Per-connection and global in-flight limits reject excess
work before unbounded buffering.

## Cancellation and deadlines

Every executable request carries an absolute monotonic deadline or explicit
no-deadline policy allowed only for trusted embedded use. `CANCEL` is
idempotent and keyed by request ID. Cancellation cannot report rollback until
the transaction coordinator confirms that no root was published.

## Errors

`ERROR` includes stable Hyphae code, retry class, message, optional source span,
object ID, transaction state, request ID and server trace ID. Unknown required
payload fields or message kinds fail the request or connection according to
the negotiated minor-version rule.

## Authentication boundary

UDS/named-pipe deployments rely on OS peer identity and endpoint ACL by
default. Loopback TCP requires a configured token or later TLS identity.
Authentication, authorization and tenant policy are not engine-to-engine
protocols.

## Performance

Prepared point operations avoid SQL text, JSON, schema lookup and per-request
heap allocation after decode. Transport, queueing, execution and durability
times are reported separately.

## Verification

Required evidence includes golden frames, cross-platform client/server
conformance, malformed length/checksum/kind rejection, bounded streaming,
flow-control deadlock tests, cancellation races, transaction-state tests,
peer-identity tests and the first local-protocol latency receipt.
