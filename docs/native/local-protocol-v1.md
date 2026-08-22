<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native local protocol v1

Protocol minor 4 adds no request or response tags: it admits three new
integrated-search filter nodes inside the existing `SearchCollection` body —
node tag `6` (`in` bounded same-type membership, at most 256 members), node
tag `7` (`is_null` missing-field membership), and node tag `8` (`like`
anchored pattern over `_` and `%`, at most 256 pattern bytes) — plus
content-derived tagged sections after the request limit, in ascending tag
order with duplicates rejected: tag `1` is the fusion selector (one byte,
`weighted_score`), tag `2` is first-k-per-parent deduplication (field text
plus a bounded count). An absent section is the default, so every request
expressible at minor 3 keeps its exact historical bytes. Operation
minor requirements are content-inspecting: a client or server that
negotiated minor 3 or lower rejects a request containing the new nodes
before sending or before dispatch, and every filter expressible at minor 3
keeps its exact historical bytes. Sealed search proofs whose request
contains the new nodes carry semantics version 3; every other proof keeps
version 2 and its exact historical bytes, and verifiers accept both.

Protocol minor 3 adds request tag `54` (`CatalogVisibleList`) and response tag
`42` (`CatalogVisiblePage`). Minor 0-2 retain the exact historical
`CatalogList` tag `15` and `CatalogPage` tag `13` layout; minor 2 rejects the
new variants before dispatch. The new cursor field is length-framed opaque
bytes only. Python exposes `bytes`, TypeScript exposes `Uint8Array`, and Rust
exposes `CatalogVisibleCursor` without snapshot or position accessors.

Minor 3 also reserves request tags `55` through `68` for the separated
`SecurityApiKey{Issue,Rotate}{Self,}{Start,Activate,Abort}` and
`SecurityApiKeyRevoke{Self,}` variants. Response tag `43` is the one-time
`SecurityApiKeyStarted` delivery; tag `44` is the redacted activation receipt.
Every lifecycle request is path-free, strict, managed-authority, and requires a
nonzero idempotency token. A repeated Start never re-encodes its secret and
fails with `secret_delivery_consumed`; a token reused with another payload
fails with `idempotency_conflict`. Start commits an inactive verifier, Activate
requires the exact confirmation digest derived from the delivered secret, and
disconnecting before Activate leaves the key inactive and unauthenticatable.
These variants are never accepted inside `Prove` and are absent from MCP.

Status: implemented normative contract; G6 cross-surface and cross-platform
receipts are closed for the bounded product profile

The local protocol exposes native typed operations without defining internal
engine communication. Embedded calls remain direct Rust calls.

## Transports

- Unix: Unix domain socket with filesystem permissions.
- Windows: named pipe with an explicit security descriptor.
- TCP loopback: not implemented by the native daemon. Remote access uses the
  optional loopback HTTP `/v2` adapter; a native TCP transport would require
  its own accepted contract before implementation.

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

Minor negotiation selects the highest version in the intersection of the
client range and the server range. A 1.0 client therefore remains on minor
`0` when it connects to a 1.2 server; neither peer may send or accept a payload
introduced after the selected minor.

### HELLO authentication extension

The canonical legacy `HELLO` remains byte-for-byte unchanged. Its fixed
58-byte header uses bytes 49 through 51 as zero-valued reserved bytes, and its
payload ends after the client identity, database and schema UTF-8 fields.

Managed API-key authentication uses a parallel `HELLO` codec with this exact
extension:

| Offset | Width | Field |
|---:|---:|---|
| 49 | 1 | authentication kind: `1` for Native API key |
| 50 | 2 | authentication trailer length, little-endian |

The authenticated payload order is the fixed header, client identity,
database, schema, then authentication trailer. The trailer is one canonical
Native API-key candidate: exactly 102 UTF-8 bytes. The combined identity,
database and schema bound remains 4 KiB and excludes the fixed-size secret.
No padding or trailing bytes are admitted.

Authenticated `HELLO` requires capability bit 7, `API_KEY_AUTH`, in both the
supported and required capability masks. Requiring the bit prevents a peer
from silently downgrading a managed connection to OS-peer authentication.
The legacy decoder rejects the extension, while the authenticated decoder
rejects a legacy payload, an unrequired capability, unknown authentication
kinds, non-canonical lengths, invalid UTF-8, truncation and trailing bytes.
Credential syntax and verifier failures are intentionally left to the sole
product authority so they remain indistinguishable as authorization denial.

A managed daemon sends `WELCOME` immediately for current authority. When
normal authentication fails, it retains only opaque pending terminal state and
waits for one bounded request frame before replying. It sends `WELCOME` only if
that frame is an exact durable self-revoke or zero-overlap self-rotation
activation replay, then executes that already supplied frame. Unknown keys,
nonterminal operations, malformed frames, and any token, target, digest, or
marker mismatch receive the same handshake `authorization_denied`; no terminal
session or revoked-key oracle is exposed.

The raw credential is ephemeral transport material. It is never part of the
public `Hello` value, client identity, diagnostics or `Debug` output. Decoded
credentials are held only in a redacted, erase-on-drop value until transferred
to the product authority.

The daemon selects this authenticated `HELLO` automatically whenever its sole
product service reports a bootstrapped access-control catalog. The legacy
OS-peer `HELLO` remains byte-identical for unbootstrapped directories, but is
rejected with the uniform authorization failure after bootstrap. The public
daemon constructors cannot force OS-peer authority for a bootstrapped service.

The closed capability registry is:

| Bit | Name |
|---:|---|
| 0 | `STREAM_COMPLETION` |
| 1 | `FLOW_CONTROL` |
| 2 | `CANCELLATION` |
| 3 | `DEADLINES` |
| 4 | `PREPARED` |
| 5 | `PEER_IDENTITY` |
| 6 | `PRODUCT_ERRORS` |
| 7 | `API_KEY_AUTH` |

Bits 8 through 63 are unknown and fail closed.

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

### Product payload minor registry

Minor `1` adds the managed, redacted security read plane. Its append-only
product request tags are `42..47`, in this exact order:

1. `SecurityStatus`;
2. `SecurityPrincipalList`;
3. `SecurityRoleList`;
4. `SecurityAssignmentList`;
5. `SecurityKeyList`; and
6. `SecurityAuditRead`.

The corresponding response tags are `32..37`. Cursors are typed, bounded,
exclusive continuations bound to the current authorization epoch. Key pages
contain public IDs and policy metadata only; API-key secrets and verifier
digests are not representable in the wire schema. Every request requires a
managed API-key session and an instance-scoped `security.read` or `audit.read`
grant as appropriate.

These tags require negotiated minor `1` in both directions. A client that
negotiated minor `0` rejects them before sending, and a server that negotiated
minor `0` rejects them before dispatch. Existing minor-0 payload tags and
golden bytes remain unchanged.

Minor `2` adds the first secret-free managed security write plane. Its
append-only request tags are `48..53`, in this exact order:

1. `SecurityPrincipalCreate`;
2. `SecurityPrincipalSetEnabled`;
3. `SecurityCustomRoleCreate`;
4. `SecurityBuiltInAssignmentCreate`;
5. `SecurityCustomAssignmentCreate`; and
6. `SecurityAssignmentRevoke`.

The corresponding response tags are `38..41`: principal, custom-role,
assignment, and generic security mutation receipts. Every write request
requires negotiated minor `2`, a managed session with instance-scoped
`security.manage`, strict durability, and a nonzero idempotency token in the
canonical request context. The token is not duplicated inside the operation
payload. Exact retries return the retained durable receipt; a token reused for
a different canonical request returns `idempotency_conflict` without dispatching
a second mutation. Minor `0` and `1` peers reject these request and response
tags before dispatch or delivery. Existing minor-0 and minor-1 golden bytes
remain unchanged.

The write payloads contain public IDs and bounded policy metadata only. They
cannot carry an API-key secret, verifier, output path, caller-selected actor,
or Owner assignment. They are not eligible for proof wrapping.

Backup verification is not part of this transport extension. It remains a
local API until a later contract defines a configured backup root and
handle-relative, no-follow path resolution; arbitrary client-selected server
filesystem paths are never accepted by this read plane.

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
default. Managed deployments use the authenticated `HELLO` extension and
revalidate its resulting authority at the product boundary. Any TCP transport
that carries an API key requires TLS; loopback placement alone does not protect
the credential. Authentication, authorization and tenant policy are not
engine-to-engine protocols.

## Performance

Prepared point operations avoid SQL text, JSON, schema lookup and per-request
heap allocation after decode. Transport, queueing, execution and durability
times are reported separately.

## Verification

Required evidence includes golden frames, cross-platform client/server
conformance, malformed length/checksum/kind rejection, bounded streaming,
flow-control deadlock tests, cancellation races, transaction-state tests,
peer-identity tests and the first local-protocol latency receipt.
