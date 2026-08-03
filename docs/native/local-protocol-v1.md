# Native local protocol v1

Status: normative target contract; the allocation-free borrowed frame decoder,
encoder, version/kind validation, bounds, CRC32C, and first filesystem-backed
Unix-domain-socket framed transport are implemented experimentally. The first
canonical scalar structure `GET` also executes through that transport. Windows
named-pipe transport, writes, SQL/search payloads, the complete handshake,
transactions, and session flow control remain pending.

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

## Message families

- session: `HELLO`, `WELCOME`, `PING`, `CLOSE`;
- plan: `PREPARE`, `EXECUTE`, `DEALLOCATE`, `EXPLAIN`;
- transaction: `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`;
- structure: typed point and structure operations;
- search: typed lexical, vector, hybrid and aggregation requests;
- flow: `DATA`, `END`, `WINDOW_UPDATE`, `CANCEL`;
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

The next frozen subset is
[native local SEARCH MATCH v1](local-search-match-v1.md). It carries one
catalog object identity and bounded UTF-8 query directly to the physical
inverted index, with the all-engine visible CSN in the canonical result.
Implementation and direct-Linux evidence remain pending.

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
