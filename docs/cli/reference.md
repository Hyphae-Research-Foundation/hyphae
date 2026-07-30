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
- `compact`
- `backup`
- `backup-verify`
- `restore`
- `doctor`
- `verify`
- `verify-retrieval`
- `serve`
- `remote`
- `mcp`
<!-- cli-commands:end -->

All commands also accept `--help`; the executable accepts `--version`.

## Common data-directory behavior

`put`, `get`, `delete`, `query`, `snapshot`, `compact`, `backup`, `restore`,
`doctor`, and `serve` require `--data-dir <PATH>` or `HYPHAE_DATA_DIR`.
Opening the path initializes it if absent, verifies durable state, rebuilds a
stale disposable index, and takes an exclusive operating-system lock.

Every path supplied to `--out` or restore destination must be new. Hyphae
refuses to replace an existing proof, witness, backup, or data directory.

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

## `compact`

```text
hyphae compact --data-dir <PATH>
```

Creates/reuses a verified snapshot, selects a new log generation through an
immutable manifest, and only then retires the old segment. Output is either a
compaction report or an already-compacted outcome. Logical records and commit
receipts remain unchanged.

## `backup`

```text
hyphae backup --data-dir <PATH> --out <NEW_DIRECTORY>
```

Creates one portable backup at a locked logical checkpoint. The destination
contains exactly `BACKUP.json` and `snapshot.hysnap`; it must not exist and
must be outside the live data directory.

## `backup-verify`

```text
hyphae backup-verify --backup <DIRECTORY>
```

Verifies backup layout, manifest metadata, snapshot framing/checksums/digest,
and checkpoint identity without creating or opening a live data directory.

## `restore`

```text
hyphae restore --backup <DIRECTORY> --data-dir <NEW_DIRECTORY>
```

Verifies the source, reconstructs and reopens storage in a sibling staging
directory, then atomically activates the new destination. It never merges or
overwrites data and does not modify the backup.

## `doctor`

```text
hyphae doctor --data-dir <PATH>
```

Opens and verifies the format, manifest, snapshot, log chain, recovery state,
and materialized index, then creates/reuses a logical snapshot. Success prints
`status: healthy` plus recovery and checkpoint evidence. It is not an
in-place corruption repair tool.

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
```

Starts API v1 and owns the data directory until Ctrl+C. The default bind is
`127.0.0.1:8787`. A non-loopback bind is rejected before socket creation
unless a valid bearer token file or `HYPHAE_BEARER_TOKEN` is present. The
binary uses the audited default resource policy; inspect it with
`/v1/capabilities`.

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
hyphae mcp --base-url <ROOT_ORIGIN> [--bearer-token-file <PATH>]
```

Runs MCP revision `2025-11-25` as newline-delimited JSON-RPC 2.0 over stdio.
It opens no listener or data directory. The adapter enforces a 4 MiB message
bound and exposes twelve tools through canonical schemas. See the
[MCP guide](../../mcp/README.md).

## Environment summary

| Variable | Equivalent option or fallback |
|---|---|
| `HYPHAE_DATA_DIR` | `--data-dir` |
| `HYPHAE_BASE_URL` | `--base-url` |
| `HYPHAE_BEARER_TOKEN_FILE` | `--bearer-token-file` |
| `HYPHAE_BEARER_TOKEN` | Token fallback when no file is selected |

See the [configuration reference](../configuration.md) for precedence,
security requirements, and programmatic server/client limits.
