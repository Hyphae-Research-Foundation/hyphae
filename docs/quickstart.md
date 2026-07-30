# Local quickstart

This quickstart exercises Hyphae as one executable and one owned data
directory. It does not start or contact a database, cache, cloud service,
embedding provider, or LLM.

For the complete surface-by-surface inventory, see
[`product/capabilities.md`](product/capabilities.md). For every command and
option, see [`cli/reference.md`](cli/reference.md).

## Install the binary

This source tree is the `0.2.1` candidate. Source version preparation is not
evidence that publication completed. The `cargo install` command below is
valid only after crates.io lists `hyphae-cli` version `0.2.1`:

```bash
cargo install hyphae-cli --version 0.2.1 --locked
hyphae version --json
```

Prebuilt native archives are a separate distribution path. Use them only
after a matching `v0.2.1`
[GitHub release](https://github.com/celiumsai/hyphae/releases/latest) exists;
treat that release as valid only if it carries the matching checksum, SBOM,
signature, provenance, required-checks, and release-evidence assets.

## Build from source

The repository pins its Rust toolchain in `rust-toolchain.toml`.

```bash
cargo build --release --locked -p hyphae-cli
export HYPHAE_DATA_DIR="$PWD/hyphae-data"
./target/release/hyphae version --json
```

On PowerShell, use the Windows executable and environment syntax:

```powershell
cargo build --release --locked -p hyphae-cli
$env:HYPHAE_DATA_DIR = Join-Path $PWD "hyphae-data"
.\target\release\hyphae.exe version --json
```

## Store and read structured documents

JSON numbers in the phase-3 structured value contract are signed 64-bit
integers. Every command reopens and verifies the same durable directory.

```bash
./target/release/hyphae put --key alpha --json '{"group":"x","score":10}'
./target/release/hyphae put --key beta --json '{"group":"x","score":20}'
./target/release/hyphae get --key alpha
```

`put` returns a transaction ID, commit sequence, commit digest, and
transaction digest. Supplying the same `--transaction-id` with the same
canonical operation returns `existing`; reusing it for different operations
fails. A plain local `get` returns `proof: null`; add `--proof-out` when the
result must be portable and independently verifiable.

## Query without AI

```bash
./target/release/hyphae query --field group --equals '"x"' --sort score
./target/release/hyphae query --sort score --descending --limit 1
```

The engine scans the complete logical dataset, applies the filter, performs
the global deterministic sort, and applies the final limit. Binary key order
is the mandatory final tie-breaker. The output includes scan and match counts
and a logical continuation cursor when more rows exist.

## Snapshot and compact

```bash
./target/release/hyphae snapshot
./target/release/hyphae compact
./target/release/hyphae query --sort score --descending --limit 2
```

The final query must return the same logical rows after compaction. The
black-box test in `crates/hyphae-cli/tests/single_binary.rs` executes this
complete flow, including durable idempotency and a fresh process for every
command.

## Back up, restore, and diagnose offline

Every destination must be new and outside the live data directory:

```bash
./target/release/hyphae backup \
  --data-dir "$HYPHAE_DATA_DIR" --out ./hyphae-backup
./target/release/hyphae backup-verify --backup ./hyphae-backup
./target/release/hyphae restore \
  --backup ./hyphae-backup --data-dir ./hyphae-restored
./target/release/hyphae doctor --data-dir ./hyphae-restored
```

Restore verifies the complete portable snapshot, rebuilds its embedded index,
and reopens the engine before the final directory becomes visible. See
[`operations/backup-restore.md`](operations/backup-restore.md) and
[`operations/doctor.md`](operations/doctor.md) for operating procedures.

## Prove and verify a result offline

Create a portable proof while querying:

```bash
./target/release/hyphae query \
  --sort score --descending --limit 2 \
  --proof-out result.hyproof
```

The JSON response includes `proof.snapshot_path`, `proof.anchor_digest`, and
`proof.proof_digest`. Pin the anchor digest through a channel trusted by the
verifier, transfer or retain the proof and referenced snapshot, then run:

```bash
./target/release/hyphae verify \
  --proof result.hyproof \
  --snapshot '<proof.snapshot_path>' \
  --anchor '<proof.anchor_digest>'
```

PowerShell can pass the returned fields directly:

```powershell
$proven = .\target\release\hyphae.exe query `
  --sort score --descending --limit 2 `
  --proof-out result.hyproof | ConvertFrom-Json
.\target\release\hyphae.exe verify `
  --proof result.hyproof `
  --snapshot $proven.proof.snapshot_path `
  --anchor $proven.proof.anchor_digest
```

`verify` checks both artifacts, compares the expected anchor, decodes the
complete snapshot, reexecutes the embedded operation, and requires an exact
result match. It does not open a live data directory or contact the network.

## Run proof-bearing durable retrieval

Durable vector, lexical, and hybrid operations use the same binary through
the public `/v1` surface. The maintained request files under `examples/http`
create a two-dimensional vector space, persist vectors, define a lexical
index, and query all three retrieval modes:

```bash
./target/release/hyphae serve --data-dir "$HYPHAE_DATA_DIR"
```

In a second shell:

```bash
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  define-vector-space --request examples/http/define-vector-space.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  put --request examples/http/put.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  put-vectors --request examples/http/put-vectors.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  define-lexical-index --request examples/http/define-lexical-index.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  retrieve-exact --request examples/http/retrieve-exact.json > exact.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  retrieve-lexical --request examples/http/retrieve-lexical.json > lexical.json
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  retrieve-hybrid --request examples/http/retrieve-hybrid.json > hybrid.json
```

Each retrieval response contains canonical proof bytes, a trusted-channel
anchor candidate, and the exact witness reference. Extract `proof.data` from
one response as standard padded base64 into `result.hyrproof`, save the
`proof` object itself as `proof.json`, and download its witness:

```bash
./target/release/hyphae remote --base-url http://127.0.0.1:8787 \
  witness --proof proof.json --out witness.hysnap
./target/release/hyphae verify-retrieval --kind exact \
  --proof result.hyrproof --snapshot witness.hysnap \
  --anchor '<proof.anchor_digest>'
```

The verifier opens neither the live data directory nor a network connection.
The automated package-install smoke and CLI integration test execute exact,
lexical, and hybrid proof verification end to end.

## Optional `/v1` server

The same binary can explicitly own the directory and expose the public API on
loopback:

```bash
./target/release/hyphae serve
curl --fail http://127.0.0.1:8787/v1/health/live
curl --fail http://127.0.0.1:8787/v1/capabilities
```

No listener starts unless `serve` is selected. While it runs, other Hyphae
processes cannot open the same directory. For authentication, remote-bind
rules, curl examples, limits, and proof witness download, see
[`api/v1.md`](api/v1.md).

## Optional public clients

The Rust, TypeScript, Python, remote CLI, and MCP surfaces call only `/v1`.
For installation/source examples, lossless TypeScript integer behavior,
remote request files, MCP host configuration, and the common executable suite,
see [`clients/v1.md`](clients/v1.md).

## Current boundary

The current implementation exposes durable KV documents, named vector spaces,
provider-free lexical definitions, deterministic exact/lexical/hybrid
retrieval, snapshot, compaction, backup/restore/doctor, both offline proof
formats, and the optional secure OpenAPI-first `/v1` server with equivalent
public clients. No embedding provider is enabled or required.

Continue through the [documentation index](README.md) for embedding, complete
CLI/configuration references, SDK/MCP guides, durable formats, security, and
release verification.
