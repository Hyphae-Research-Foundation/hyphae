# CLI reference

`hyphae` is the only executable. It prints successful machine-readable
operation results as formatted JSON on stdout, diagnostics on stderr, and
returns a nonzero exit status on failure. Commands never start a listener
unless `serve` is selected.

Run `hyphae <command> --help` for the syntax shipped by the current binary.
This page explains semantics and side effects that do not fit in short help.

## Command inventory

<!-- cli-commands:start -->
- `version`
- `put`
- `get`
- `delete`
- `query`
- `snapshot`
- `migrate`
- `init`
- `capabilities`
- `catalog`
- `sql`
- `structure`
- `search`
- `transaction`
- `explain`
- `hardware`
- `status`
- `telemetry`
- `console`
- `security`
- `doctor`
- `checkpoint`
- `compact`
- `vacuum`
- `backup`
- `backup-verify`
- `restore`
- `proof`
- `verify`
- `verify-retrieval`
- `serve`
- `remote`
- `mcp`
<!-- cli-commands:end -->

All commands also accept `--help`; the executable accepts `--version`.

## Common data-directory behavior

Commands that operate on live state accept `--data-dir <PATH>` or
`HYPHAE_DATA_DIR`. Format-2 compatibility commands initialize an absent path;
Native commands require an existing directory created by `init`. Opening live
state verifies recovery authority and takes an exclusive operating-system lock.

Every path supplied to `--out` or restore destination must be new. Hyphae
refuses to replace an existing proof, witness, backup, or data directory.

Commands in the Native `dev` surface never initialize a missing directory on
open: use `init` first. The format-2 compatibility commands retain their
published `0.2.1` initialization behavior.

After `security bootstrap`, every online Native CLI command and `console`
automatically requires the durable API key. Supply it through
`--native-api-key-file <RESTRICTED_FILE>` (or
`HYPHAE_NATIVE_API_KEY_FILE`) or pipe it to `--native-api-key-stdin`. The raw
credential is never accepted in argv. Unix key files must be regular,
non-symlink files without group or other permission bits. Windows ACL
restriction and named-pipe parity remain explicit 1.2 release gates; no
cross-platform file-hygiene claim is made yet.
On Unix the CLI also compares device/inode identity for the path before and
after opening and for the opened handle, rejecting substitution rather than
reading a different credential file.

The offline `security bootstrap` path does not consume a credential. `doctor`
uses the centrally authorized `maintain` operation whenever the directory can
open; only a directory that cannot produce a product owner uses the bounded
offline corruption/busy diagnostic. Native maintenance helpers that do not yet
have a central product operation fail closed after bootstrap.

## `console`

```text
hyphae console --data-dir <NATIVE_DIRECTORY>
```

Opens the interactive native operator console while holding the same exclusive
directory ownership as other embedded commands. It starts no listener and
connects to no external service. The console currently provides:

- responsive overview and authenticated-session dashboards;
- an interactive SQL editor and result pane backed by `ProductOperation`;
- a read-only Security workspace for status, principals, roles, assignments,
  key metadata, and retained audit events;
- navigation targets for structures, search, and catalog workflows; and
- bounded input/output rendering that never displays API-key secrets.

Use Tab or the arrow keys to change views. In the SQL view, Enter executes the
current statement and Backspace edits it. In other views, `r` refreshes
capabilities through the current authenticated session. Within Security,
Up/Down selects Status, Principals, Roles, Assignments, Keys, or Audit; `n`
requests the next page and `r` returns to the first page. Every page is capped
at 12 rows and retains no cursor history, so terminal memory remains bounded.
Security views use only the central `ProductOperation` read plane on the
console's existing managed session. A principal without `security.read` or
`audit.read` sees the typed denial in the panel rather than a raw-catalog
fallback. Key rows structurally contain only public identifiers and redacted
metadata; credentials and verifiers are never rendered. Escape or Ctrl-C
exits. Terminal state is restored on normal exit and errors. Structure,
search, and catalog mutation actions remain read-only placeholders until their
typed workflows are implemented; the UI does not claim those actions are
available.

## `security`

```text
hyphae security --data-dir <NATIVE_DIRECTORY> status
hyphae security --data-dir <NATIVE_DIRECTORY> principal list \
  [--cursor <OPAQUE_CURSOR>] [--limit <ROWS>]
hyphae security --data-dir <NATIVE_DIRECTORY> role list \
  [--cursor <OPAQUE_CURSOR>] [--limit <ROWS>]
hyphae security --data-dir <NATIVE_DIRECTORY> assignment list \
  [--cursor <OPAQUE_CURSOR>] [--limit <ROWS>]
hyphae security --data-dir <NATIVE_DIRECTORY> key list \
  [--cursor <OPAQUE_CURSOR>] [--limit <ROWS>]
hyphae security --data-dir <NATIVE_DIRECTORY> audit list \
  [--cursor <EVENT_ID>] [--limit <ROWS>]
hyphae security --data-dir <NATIVE_DIRECTORY> bootstrap \
  --name <PRINCIPAL_NAME> --label <KEY_LABEL> --key-out <NEW_FILE>
```

`status`, `principal list`, `role list`, `assignment list`, and `key list`
execute through the current managed session and the central
`security.read` product operations. `audit list` requires `audit.read`.
They fail closed without a valid key and expose only redacted metadata. List
limits are validated by the native product. Metadata cursors are opaque,
authorization-generation-bound values and must be supplied only to the same
list command that emitted them; stale and cross-family cursors are rejected.
Audit cursors are canonical retained event IDs.

The offline `bootstrap` command is the sole unauthenticated exception. It
creates the first owner principal and key through the strict native WAL. The
output credential file must not exist, is created with owner-only permissions
on Unix, and is activated only after its contents have been synchronized.
Hyphae never prints a credential secret or verifier to stdout or logs.

### Security write-plane status

The CLI exposes exactly six secret-free managed mutations:

```text
hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  principal create --name <DISPLAY_NAME> --idempotency-token <NONZERO_U128>

hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  principal set-enabled --principal-id <SECURITY_ID> --enabled <true|false> \
  --idempotency-token <NONZERO_U128>

hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  role create --name <DISPLAY_NAME> --grant <PERMISSION@SCOPE> \
  [--grant <PERMISSION@SCOPE> ...] --idempotency-token <NONZERO_U128>

hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  assignment create-built-in --principal-id <SECURITY_ID> \
  --role <admin|operator|developer|writer|reader|auditor> --scope <SCOPE> \
  --idempotency-token <NONZERO_U128>

hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  assignment create-custom --principal-id <SECURITY_ID> --role-id <SECURITY_ID> \
  --idempotency-token <NONZERO_U128>

hyphae security --data-dir <NATIVE_DIRECTORY> \
  --native-api-key-file <RESTRICTED_FILE> \
  assignment revoke --assignment-id <SECURITY_ID> \
  --idempotency-token <NONZERO_U128>
```

`SECURITY_ID` is canonical lowercase 32-hex. `SCOPE` is exactly `instance`,
`catalog_subtree:<NONZERO_DECIMAL_OBJECT_ID>`, or
`catalog_object:<NONZERO_DECIMAL_OBJECT_ID>`. Permission names are the
canonical dotted identifiers shown by `security role list`. Grants are
bounded, canonicalized, and must be unique; ownership authority cannot be
placed in a custom role.

Every mutation requires a managed key with instance-scoped `security.manage`,
a nonzero idempotency token, and strict durability. An exact retry returns the
same receipt; reuse of the token with another payload fails with
`idempotency_conflict`. The versioned JSON receipt contains only the operation,
public result identity, authorization epoch, and native commit evidence. It
never contains the credential, verifier, or an actor identity supplied by CLI
flags. Generic Owner assignment and Owner-assignment revocation remain
forbidden, and these mutations are not eligible for `Prove`.

The TUI remains read-only for security administration. Principal rename;
custom-role rename, drop, or replacement; API-key secret delivery or
revocation; ownership transfer; legacy-bearer migration; and owner recovery
are outside this slice. The CLI never uses the raw access-control catalog as a
temporary administration path.

## `version`

```text
hyphae version [--json]
```

Prints product/engine, API, and disk-format versions. `--json` is intended for
automation; the default is one human-readable line. It opens no data.

## `put`

```text
hyphae put --data-dir <PATH> --key <UTF8> --json <JSON>
           [--transaction-id <UUID>]
```

Atomically stores one document. The JSON number domain is signed 64-bit
integer only. The key's UTF-8 bytes become the durable key. Without a
transaction ID Hyphae creates a UUIDv7.

Output is a commit receipt with `committed` or `existing`, transaction ID,
commit sequence/digest, and transaction digest. An exact retry is idempotent;
reusing an ID for a different operation fails.

## `get`

```text
hyphae get --data-dir <PATH> --key <UTF8> [--proof-out <NEW_FILE>]
```

Returns `found`, the record or null, and `proof`. Without `--proof-out`, the
local read uses the ordinary embedded method and `proof` is null. With it,
Hyphae creates a canonical `.hyproof`, returns its snapshot path and digests,
and refuses to replace an existing proof file. Missing keys can be proven.

## `delete`

```text
hyphae delete --data-dir <PATH> --key <UTF8>
              [--transaction-id <UUID>]
```

Atomically records one deletion and returns a commit receipt. Deleting a
missing key is successful and idempotency follows the same rules as `put`.

## `query`

```text
hyphae query --data-dir <PATH>
             [--field <DOT.PATH> --equals <JSON>]
             [--sort <DOT.PATH>] [--descending] [--nulls-first]
             [--limit <ROWS>] [--proof-out <NEW_FILE>]
```

Executes the convenient local subset of structured query:

- no `--field`/`--equals` means match-all;
- `--field` and `--equals` must appear together and perform exact typed
  equality;
- at most one sort field is accepted;
- missing/null sort last unless `--nulls-first` is present;
- non-null values sort ascending unless `--descending` is present;
- the default final limit is 100;
- binary key ascending is always the final tie-breaker.

Output includes rows, optional next cursor, scan/match counts, and `proof`.
The local command does not accept an input cursor or aggregation plan; use
`remote query`, an SDK, or embedded Rust for the full v1 AST. Without
`--proof-out`, `proof` is null. With it, the complete query/result is bound to
the written proof and referenced snapshot.

## `snapshot`

```text
hyphae snapshot --data-dir <PATH>
```

Creates or reuses the canonical logical snapshot for the current checkpoint.
Output includes path, checkpoint identity, snapshot digest, entry/receipt
counts, and file length.

## `migrate`

```text
hyphae migrate inspect --source <FORMAT2_DIRECTORY>
hyphae migrate run --source <FORMAT2_DIRECTORY> --target <NEW_NATIVE_DIRECTORY> --manifest <NEW_FILE>
hyphae migrate verify --source <FORMAT2_DIRECTORY> --target <NATIVE_DIRECTORY> --manifest <FILE>
hyphae migrate promote --source <FORMAT2_DIRECTORY> --target <PENDING_NATIVE_DIRECTORY> --manifest <FILE>
hyphae migrate rollback --target <PENDING_NATIVE_DIRECTORY> [--manifest <FILE>]
```

The importer reads the format-2 source without mutating it, creates a separate
pending Native directory, verifies identity and logical SQL/structure/search
equivalence, and requires explicit promotion. Rollback only removes an
unpromoted target owned by the migration manifest.

## `init` and `capabilities`

```text
hyphae init --data-dir <NEW_NATIVE_DIRECTORY>
hyphae capabilities --data-dir <NATIVE_DIRECTORY>
```

`init` fails if the destination exists. `capabilities` reports the Native
product API, directory format, operations, limits, and durability classes
without starting a listener.

## `hardware`

```text
hyphae hardware discover [--data-dir <PATH>]
hyphae hardware calibrate [--data-dir <PATH>] [--mode <quick|thorough>]
                           [--cache-dir <PATH> | --no-cache]
hyphae hardware governor-policy [--data-dir <PATH> | --profile <FILE>]
                                --calibration <RECEIPT.json>
                                [--mode <latency|bulk|mixed>]
hyphae hardware execution-topology [--data-dir <PATH> | --profile <FILE>]
                                   --calibration <RECEIPT.json>
                                   [--mode <latency|bulk|mixed>]
```

Discovers the process-visible CPU topology and features, memory and page
configuration, operating system, virtualization status, and the filesystem and
device containing the selected path. It performs no host or database mutation.
The JSON fingerprint excludes available memory and the literal data path while
binding scheduling-relevant topology, affinity, quota, mount, device, and
kernel properties. Missing platform data remains explicit rather than being
reported as zero.

Policy and topology derivation may consume the exact discovery receipt through
`--profile`; this is the qualification path because volatile available-memory
observations must not be silently rediscovered between evidence steps.

`calibrate` binds the static profile to the exact executable and compiler, then
measures the implemented CPU, memory, engine, storage, WAL, thread-scaling,
I/O-depth, and supported NUMA-local/remote matrix. Linux scaling workers use
the discovered per-core affinity order when topology is complete. Storage work
is confined to a bounded temporary directory on the
selected filesystem and is removed before return. Quick mode targets
5–15 seconds; thorough mode targets 3–10 minutes. The receipt reports variance,
differential correctness, accepted kernel selections, and all unsupported P1
surfaces. An unstable or out-of-window receipt is diagnostic and publishes no
kernel selection. Accepted receipts use an immutable per-user cache keyed by
hardware, kernel, filesystem, compiler, build, executable bytes, mode, and
policy. `--no-cache` performs a diagnostic run without persistence.

`governor-policy` re-discovers the selected path and fails unless its hardware
fingerprint matches the receipt. It independently derives the stable scaling
recommendation, preserves 15 percent total-memory headroom, and emits the
versioned global and per-class CPU/I/O/memory policy plus the canonical
admission-queue capacity and foreground burst bound for inspection. The default
is `mixed`; this command admits no work and modifies no state.

`execution-topology` derives that policy and emits the versioned persistent
worker placement without starting threads. Complete processor discovery is
physical-core-first, grouped by NUMA node, and includes logical processor,
socket, core, and SMT rank; incomplete platforms emit one explicit portable
unbound pool. The semantic checker rejects missing workers, duplicate CPUs,
cross-node placement, partial topology, and noncanonical SMT ranks.

## `catalog`

```text
hyphae catalog --data-dir <NATIVE_DIRECTORY> <list|describe|resolve|dependencies|create-keyspace|create-search-collection> ...
```

Catalog pages are bounded and stable-ID ordered. Creation commands atomically
publish catalog-owned keyspaces or an integrated search collection; inspect
each subcommand's help for typed IDs, families, limits, and durability.

## `sql`

```text
hyphae sql --data-dir <NATIVE_DIRECTORY> execute --statement <SQL> [--parameter <JSON>]... [--durability <CLASS>]
hyphae sql --data-dir <NATIVE_DIRECTORY> prepared --statement <SQL> [--parameter <JSON>]...
```

Parameters are canonical JSON scalars in statement order. `prepared` prepares,
executes, and deallocates in one retained session; both commands return typed
commit or bounded row results.

## `structure`

```text
hyphae structure --data-dir <NATIVE_DIRECTORY> <get|set|ttl|batch|read> ...
```

`get`, `set`, and `ttl` address the scalar Native keyspace. `batch` applies a
typed JSON mutation array atomically across catalogued strings, counters,
hashes, lists, sets, sorted sets, and streams. `read` accepts the corresponding
bounded typed read request.

## `search`

```text
hyphae search --data-dir <NATIVE_DIRECTORY> <provision|query|integrated|ingest|update|delete> ...
```

`provision` creates catalog-owned physical indexes. Ingest/update/delete are
idempotent all-branch mutations. Query surfaces cover lexical term/phrase/
prefix/fuzzy matching and integrated lexical, exact-vector, ANN, hybrid,
filter, sort, facet, and metric execution.

## `transaction`

```text
hyphae transaction --data-dir <NATIVE_DIRECTORY> execute --steps-json <ARRAY> [--durability <CLASS>]
hyphae transaction --data-dir <NATIVE_DIRECTORY> status --id <U128>
```

The script retains one explicit transaction session for SQL, structure,
search, and vector stages followed by commit or rollback. `status` resolves
durable outcome evidence after disconnect or an uncertain commit response.

## `explain`, `status`, and `telemetry`

```text
hyphae explain --data-dir <NATIVE_DIRECTORY> sql --statement <SQL>
hyphae status --data-dir <NATIVE_DIRECTORY>
hyphae telemetry --data-dir <NATIVE_DIRECTORY>
```

`explain` returns bounded physical plan text without execution. `status`
reports the all-engine catalog/root/CSN state. `telemetry` returns a bounded,
redacted process-local snapshot and does not enable a background exporter.

## `checkpoint` and `vacuum`

```text
hyphae checkpoint --data-dir <NATIVE_DIRECTORY>
hyphae vacuum --data-dir <NATIVE_DIRECTORY>
```

`checkpoint` synchronizes and publishes one all-engine recovery boundary.
`vacuum` rebuilds live roots into a smaller page generation and publishes it
atomically; neither command is an online compaction latency promise.

## `proof`

```text
hyphae proof generate --data-dir <NATIVE_DIRECTORY> --operation-json <JSON> --proof-out <NEW_FILE> --witness-out <NEW_FILE>
hyphae proof verify --proof <FILE> --witness <FILE> --anchor <64_HEX_CHARS>
```

Native proof generation covers admitted catalog, SQL, and product reads. The
offline verifier checks the proof, complete witness, canonical request/result,
and independently supplied anchor without opening live state.

## `compact`

```text
hyphae compact --data-dir <PATH>
```

For Native directories this compacts the selected root family (`structures` by
default) and atomically publishes the result. For format-2 compatibility state
it creates/reuses a verified snapshot, selects a new log generation through an
immutable manifest, and only then retires the old segment.

## `backup`

```text
hyphae backup --data-dir <PATH> --out <NEW_DIRECTORY>
hyphae backup create --data-dir <NATIVE_DIRECTORY> --out <NEW_DIRECTORY>
hyphae backup verify --backup <NATIVE_BACKUP_DIRECTORY>
```

The first form creates a published format-2 portable backup. The Native form
creates a synchronized physical backup containing `NATIVE_BACKUP.json` and an
exact data inventory, then verifies it before promotion. Destinations must be
new and outside the source.

## `backup-verify`

```text
hyphae backup-verify --backup <DIRECTORY>
```

Verifies the published format-2 backup layout, manifest metadata, snapshot
framing/checksums/digest, and checkpoint identity without opening live state.
Use `backup verify` for a Native backup.

## `restore`

```text
hyphae restore --backup <DIRECTORY> --data-dir <NEW_DIRECTORY>
```

Verifies a Native backup, reconstructs and reopens storage in a sibling staging
directory, runs mandatory doctor validation, then atomically activates the new
destination. It never merges or overwrites data and does not modify the backup.

## `doctor`

```text
hyphae doctor --data-dir <PATH>
```

Auto-detects Native versus format-2 state and runs the matching bounded offline
diagnosis. Native diagnosis verifies format, pages, WAL, manifests, blobs,
indexes, catalog/root authority, and recovery state. It is not an in-place
corruption repair tool.

## `verify`

```text
hyphae verify --proof <FILE> --snapshot <FILE> --anchor <64_HEX_CHARS>
```

Verifies a canonical result proof completely offline. The anchor is a trusted
32-byte digest encoded as hexadecimal. The verifier validates both artifacts,
matches the caller's anchor, reexecutes get/query, and requires the exact
result. It opens no live data directory and performs no network request.
Version `0.2.1` defaults to a 2 GiB snapshot file and 1 GiB of aggregate
decoded logical payload retained by the verifier, including KV, vector-space,
vector, and lexical-index payloads. Embedded callers may configure a different
policy; larger values expand their resource-exposure envelope.

## `verify-retrieval`

```text
hyphae verify-retrieval --kind <exact|lexical|hybrid>
  --proof <FILE> --snapshot <FILE> --anchor <64_HEX_CHARS>
```

Verifies a canonical `.hyrproof` completely offline. The caller selects the
operation because exact, lexical, and hybrid proof payloads are closed,
independently decoded formats. The verifier validates both files, checks the
trusted retrieval anchor, reconstructs the relevant durable snapshot state,
and reexecutes the complete operation under bounded reference semantics. It
opens no live data directory and performs no network request. Version `0.2.1`
shares the snapshot defaults above and adds a default 1 GiB limit for
exact-vector candidate key/vector bytes, including the exact branch of hybrid
replay. Lexical replay remains separately bounded by its document, token, and
candidate-count policy.

## `serve`

```text
hyphae serve --data-dir <PATH> [--bind <IP:PORT>]
             [--bearer-token-file <PATH>]
hyphae serve --data-dir <NATIVE_DIRECTORY> [--endpoint <LOCAL_ENDPOINT>]
             [--http-bind <IP:PORT>]
```

Native state starts the local UDS/named-pipe daemon and, only when requested,
the HTTP `/v2` edge. `--bind` selects the separate format-2 `/v1` compatibility
server. The process owns the directory until shutdown and refuses mixed Native
and format-2 listener options.

## `remote`

```text
hyphae remote --base-url <ROOT_ORIGIN> [--bearer-token-file <PATH>] <COMMAND>
```

The remote mode never opens a data directory. It uses only the public v1 Rust
client and accepts `HYPHAE_BASE_URL`, `HYPHAE_BEARER_TOKEN_FILE`, and
`HYPHAE_BEARER_TOKEN`.

Request JSON from a file or stdin is capped at the server default
`request_body_bytes` policy, currently 4 MiB. The witness command's proof JSON
is capped at the default `response_bytes` policy, currently 32 MiB. Named files
are rejected from metadata when already oversized, read only through the limit
plus one detection byte, and rejected when a regular file's observed length
changes during the read. Stdin has the same byte ceiling but no file metadata
to recheck. These CLI ceilings are fixed defaults, not negotiated from
`/v1/capabilities`; a custom server configured above 4 MiB request or 32 MiB
response JSON needs a different client with a matching local policy.

<!-- remote-commands:start -->
| Command | Input | Result |
|---|---|---|
| `capabilities` | None | Features and effective limits |
| `liveness` | None | Process liveness |
| `readiness` | None | Engine readiness |
| `put --request <FILE_OR_->` | `PutRequestV1` JSON | Commit receipt |
| `get --request <FILE_OR_->` | `GetRequestV1` JSON | Proven get response |
| `delete --request <FILE_OR_->` | `DeleteRequestV1` JSON | Commit receipt |
| `query --request <FILE_OR_->` | `QueryRequestV1` JSON | Proven query response |
| `define-vector-space --request <FILE_OR_->` | `DefineVectorSpaceRequestV1` JSON | Commit receipt |
| `put-vectors --request <FILE_OR_->` | `PutVectorsRequestV1` JSON | Commit receipt |
| `delete-vectors --request <FILE_OR_->` | `DeleteVectorsRequestV1` JSON | Commit receipt |
| `retrieve-exact --request <FILE_OR_->` | `ExactRetrievalRequestV1` JSON | Proven exact outcome |
| `define-lexical-index --request <FILE_OR_->` | `DefineLexicalIndexRequestV1` JSON | Commit receipt |
| `retrieve-lexical --request <FILE_OR_->` | `LexicalRetrievalRequestV1` JSON | Proven lexical outcome |
| `retrieve-hybrid --request <FILE_OR_->` | `HybridRetrievalRequestV1` JSON | Proven hybrid outcome |
| `witness --proof <FILE> --out <NEW_FILE>` | `ProofV1` or `RetrievalProofV1` JSON | Verified witness bytes |
<!-- remote-commands:end -->

`-` means read the complete request from stdin. The witness command checks
the canonical proof path, response digest header, and exact file length before
writing a new file. Example requests live in [`examples/http`](../../examples/http/README.md).

## `mcp`

```text
hyphae mcp --base-url <ROOT_ORIGIN> --native-api-key-file <RESTRICTED_PATH>
hyphae mcp --base-url <ROOT_ORIGIN> --native-api-key-stdin
```

Runs MCP revision `2025-11-25` as newline-delimited JSON-RPC 2.0 over stdio.
It opens no listener or data directory. The adapter enforces a 4 MiB message
bound and exposes only three Native v2 read tools: capabilities, redacted
security status, and bounded redacted principal pages. `tools/list` returns at
most two definitions and uses its own opaque cursor.

Use a restricted key for the built-in Auditor role at Instance scope. It has
the `security.read` permission required by both security tools without write
authority.

The key establishes the exact authority for every call. It is accepted only
from a restricted file or the first stdin line, never an argv value or plain
environment value. In stdin-key mode every following line is an MCP message.
Unknown tool arguments, including prompt-supplied roles, permissions, or API
keys, fail closed. Results expose a version and BLAKE3 digest of the exact tool
contract; Native failures retain structured `ProductError` fields. See the
[MCP guide](../../mcp/README.md).

With a durable Native API key, `http://` is limited to canonical loopback
hosts (`127.0.0.0/8`, `[::1]`, or exact `localhost`). Remote MCP endpoints must
use `https://`; plaintext remote origins fail before the stdio request loop.

## Environment summary

| Variable | Equivalent option or fallback |
|---|---|
| `HYPHAE_DATA_DIR` | `--data-dir` |
| `HYPHAE_BASE_URL` | `--base-url` |
| `HYPHAE_BEARER_TOKEN_FILE` | `--bearer-token-file` |
| `HYPHAE_BEARER_TOKEN` | Token fallback when no file is selected |
| `HYPHAE_NATIVE_API_KEY_FILE` | `--native-api-key-file` for Native local commands and MCP |

See the [configuration reference](../configuration.md) for precedence,
security requirements, and programmatic server/client limits.
