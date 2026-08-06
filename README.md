<p align="center">
  <a href="https://hyphae.dev" aria-label="Hyphae website">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup-reversed.svg">
      <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg">
      <img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="420">
    </picture>
  </a>
</p>

<p align="center"><strong>Data that can prove itself.</strong></p>

<p align="center">
  <a href="https://github.com/celiumsai/hyphae/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/celiumsai/hyphae/ci.yml?branch=main&amp;label=CI&amp;logo=github"></a>
  <a href="https://crates.io/crates/hyphae-engine"><img alt="crates.io" src="https://img.shields.io/crates/v/hyphae-engine?logo=rust"></a>
  <a href="https://docs.rs/hyphae-engine"><img alt="docs.rs" src="https://img.shields.io/docsrs/hyphae-engine?logo=docs.rs"></a>
  <a href="https://github.com/celiumsai/hyphae/releases/latest"><img alt="GitHub release" src="https://img.shields.io/github/v/release/celiumsai/hyphae?logo=github"></a>
  <a href="https://hyphae.dev"><img alt="Website" src="https://img.shields.io/badge/website-hyphae.dev-8FCBC6"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-C86F4A"></a>
  <img alt="MSRV 1.89" src="https://img.shields.io/badge/MSRV-1.89-43585A?logo=rust">
</p>

Hyphae is a local-first data engine written in Rust. One process owns one data
directory and exposes relational SQL, native structures, lexical search, and
vector search over a shared transaction, WAL, MVCC, recovery, and proof
substrate. The engine runs offline and does not embed PostgreSQL, Valkey,
OpenSearch, a cloud service, an embedding provider, or an LLM.

**Development line:** Hyphae Native is the active architecture on `dev`. G0
through G5 are closed for their versioned, bounded profiles; G6 through G8
remain open. The
[native gate status](docs/gates/native-gate-status.md) is the current status
authority; temporary workflow artifacts alone do not close a gate.

**Published stable release:** [`v0.2.1`](https://github.com/celiumsai/hyphae/releases/tag/v0.2.1)
is still the version available from crates.io and GitHub Releases. It is the
compatibility baseline, not a description of the current `dev` architecture.
Its publication receipt remains available at
[`docs/release/receipts/0.2.1.md`](docs/release/receipts/0.2.1.md).

The target release for the native line is `1.0.0`. The remaining G6-G8 product,
performance, and release gates must close before `main` receives the native
line and replaces `0.2.1`.

## What Hyphae does

- Atomically stores and deletes structured records under binary keys.
- Recovers an append-only checksummed/digest-chained log and rebuilds embedded
  Redb indexes.
- Makes mutation retries durable and idempotent through caller-visible UUIDs.
- Executes deterministic filters, global sorting, logical cursors, and
  grouped/global aggregations with hard budgets and no partial results.
- Persists named vector spaces and executes deterministic exact cosine
  retrieval with explicit abstention.
- Builds a reconstructible provider-free lexical index and fuses vector and
  lexical results with deterministic reciprocal-rank fusion.
- Creates canonical snapshots, commits anchored compaction generations, and
  rejects unsupported or corrupt formats.
- Produces portable result and retrieval proofs and verifies them offline
  against their canonical request, result, semantics, and snapshot witnesses.
- Creates, verifies, and atomically restores portable logical backups; `doctor`
  reports complete local recovery evidence.
- Optionally exposes a secure, loopback-first OpenAPI `/v1` server.
- Supplies equivalent Rust, TypeScript, Python, remote CLI, and MCP clients.
- Keeps PliegoRS, Astro, Next, and Vite adapters optional and outside the core.

The API server always returns proofs for successful get/query operations. The
embedded facade exposes ordinary and proof-bearing methods; the local CLI
creates proof files explicitly with `--proof-out`.

See the complete [capability matrix](docs/product/capabilities.md), including
surface differences, default limits, and deliberate non-capabilities.

## Install

Install the latest published native binary:

```bash
cargo install hyphae-cli --version 0.2.1 --locked
hyphae version --json
```

Embed the latest published engine with an exact product-version requirement:

```bash
cargo add hyphae-engine@=0.2.1
```

Native archives, checksums, SBOMs, provenance, signatures, and attestations
for `0.2.1` are attached to its
[GitHub release](https://github.com/celiumsai/hyphae/releases/tag/v0.2.1).

## Five-minute local flow

```bash
cargo build --release --locked -p hyphae-cli
export HYPHAE_DATA_DIR="$PWD/hyphae-data"

./target/release/hyphae version --json
./target/release/hyphae put \
  --key alpha --json '{"group":"x","score":10}'
./target/release/hyphae query \
  --field group --equals '"x"' --sort score \
  --proof-out result.hyproof
./target/release/hyphae backup --out ./hyphae-backup
./target/release/hyphae backup-verify --backup ./hyphae-backup
./target/release/hyphae doctor
```

The query response names the snapshot and anchor needed by `hyphae verify`.
The [quickstart](docs/quickstart.md) covers Windows syntax, compaction,
restore, offline proof verification, the optional server, and clients.

## Architecture

```text
application
  ├─ embedded Rust facade ───────────────────────────────┐
  ├─ local CLI                                           │
  └─ /v1 clients (Rust / TypeScript / Python / CLI / MCP)│
                             │                           │
                       secure HTTP server                │
                             └──────────────┬────────────┘
                                            ▼
                         engine: documents / query / proof
                              │                  │
                 durable exact/lexical/hybrid   │
                              │                  ▼
                              └──── append-only durable log
                                      │        │
                                  snapshots  rebuildable index
```

The append-only log is durable authority. Embedded indexes are replaceable.
One operating-system lock gives one engine/server exclusive ownership of a
data directory. See the [architecture overview](docs/architecture/overview.md)
and versioned [storage specifications](docs/README.md#durable-formats).

## Public surfaces

| Surface | Purpose |
|---|---|
| `hyphae` binary | Local engine, operations, server, remote client, verifier, MCP |
| `hyphae-engine` | Recommended embeddable Rust facade |
| `/v1` | Stable proof-bearing HTTP contract |
| `hyphae-client` | Bounded async Rust HTTP client |
| `@celiums/hyphae` | Dependency-free TypeScript client |
| `hyphae-sdk` | Dependency-free Python client |
| MCP stdio | Twelve schema-bound tools over `/v1` |
| Optional adapters | PliegoRS, Astro, Next, and Vite consumers |

OpenAPI 3.1 and JSON Schema 2020-12 under `contracts/` are the canonical wire
contracts. Integrations consume public clients only; hosts continue to build
and run with Hyphae absent.

## Rust crates

| Crate | Purpose | Documentation |
|---|---|---|
| [`hyphae-engine`](https://crates.io/crates/hyphae-engine) | Recommended embeddable facade | [docs.rs](https://docs.rs/hyphae-engine) |
| [`hyphae-storage`](https://crates.io/crates/hyphae-storage) | Durable log, recovery, snapshots, and backups | [docs.rs](https://docs.rs/hyphae-storage) |
| [`hyphae-query`](https://crates.io/crates/hyphae-query) | Deterministic structured query | [docs.rs](https://docs.rs/hyphae-query) |
| [`hyphae-retrieval`](https://crates.io/crates/hyphae-retrieval) | Deterministic exact, lexical, and hybrid retrieval | [docs.rs](https://docs.rs/hyphae-retrieval) |
| [`hyphae-contracts`](https://crates.io/crates/hyphae-contracts) | Versioned `/v1` models and embedded schemas | [docs.rs](https://docs.rs/hyphae-contracts) |
| [`hyphae-client`](https://crates.io/crates/hyphae-client) | Bounded async Rust HTTP client | [docs.rs](https://docs.rs/hyphae-client) |
| [`hyphae-server`](https://crates.io/crates/hyphae-server) | Loopback-first `/v1` server | [docs.rs](https://docs.rs/hyphae-server) |
| [`hyphae-core`](https://crates.io/crates/hyphae-core) | Product and compatibility constants | [docs.rs](https://docs.rs/hyphae-core) |
| [`hyphae-cli`](https://crates.io/crates/hyphae-cli) | Single `hyphae` binary, verifier, and MCP adapter | [docs.rs](https://docs.rs/hyphae-cli) |
| [`hyphae-pliegors`](https://crates.io/crates/hyphae-pliegors) | Optional PliegoRS public-contract adapter | [docs.rs](https://docs.rs/hyphae-pliegors) |

## Documentation

Start at the [documentation index](docs/README.md). Key guides:

- [Capabilities and limits](docs/product/capabilities.md)
- [Quickstart](docs/quickstart.md)
- [CLI reference](docs/cli/reference.md)
- [Configuration](docs/configuration.md)
- [Data model](docs/concepts/data-model.md)
- [Embed in Rust](docs/embedding/rust.md)
- [HTTP API v1](docs/api/v1.md)
- [Public clients](docs/clients/v1.md)
- [Operations and troubleshooting](docs/operations/troubleshooting.md)
- [Security model](docs/security/threat-model.md)
- [Native local ecosystem target](docs/architecture/native-local-ecosystem.md)
- [Microsecond-first target](docs/performance/microsecond-first.md)
- [Release verification](docs/release/verification.md)
- [0.2.1 publication receipt](docs/release/receipts/0.2.1.md)
- [crates.io release procedure](docs/release/crates-io.md)

## Product boundary

Hyphae Native is a local, single-node data ecosystem with Hyphae-owned SQL,
structures, lexical search, and ANN under one durable authority. G0 freezes and
verifies that foundation; it does not claim that the remaining `1.0.0` gates
are complete.

Hyphae is not Mycelium, Hyphae Network, Celiums Network, an AI cognition
runtime, a hosted SaaS, or a framework-specific data layer. The published
`0.2.1` release does not include the native SQL/structures/search architecture
now present on `dev`. Replication, clustering, built-in TLS, at-rest encryption,
multitenancy, billing, a control plane, an embedding model, and an LLM are also
outside `0.2.1`.

Hosted, distributed, and model-driven programs remain later phases. Applications
still own process supervision, remote TLS termination, filesystem permissions,
backup media policy, and optional embedding providers. Semantic providers can
supply vectors to the Rust APIs but never become a core dependency or source of
authority.

## Repository map

- `crates/`: Rust storage, engine, query, retrieval, contracts, server, client,
  and single CLI.
- `contracts/`: canonical OpenAPI and JSON Schemas.
- `sdks/`: TypeScript and Python clients/models.
- `mcp/`: MCP adapter guide; implementation is in the single binary.
- `integrations/`: optional PliegoRS, Astro, Next, and Vite adapters.
- `examples/`: maintained embedded, HTTP, and MCP examples.
- `docs/`: product, architecture, operations, security, normative formats,
  ADRs, and release gates.
- `packaging/`: deterministic multiplatform archives and release verification.
- `compatibility/`: immutable historical on-disk fixtures.

## Development

The repository pins its toolchain and enforces format, Clippy, tests,
rustdoc, contracts, documentation, dependency policy, secret scanning,
cross-platform packages, fuzzing, and recovery stress.

```console
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py --binary target/debug/hyphae
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and the
[development guide](docs/development.md).

## Historical source

Historical repositories are frozen inputs, not this repository's history. No
historical source may enter this tree without an audited entry in the
[porting ledger](docs/porting/ledger.md). Hyphae Network is not modified by
this project.

## License

Apache License 2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). The Hyphae name and visual
identity are covered separately by [TRADEMARKS.md](TRADEMARKS.md).
