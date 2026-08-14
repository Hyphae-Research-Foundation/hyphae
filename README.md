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
  <a href="LICENSE-POLICY.md"><img alt="License" src="https://img.shields.io/badge/code-AGPL--3.0--only-C86F4A"></a>
  <img alt="MSRV 1.89" src="https://img.shields.io/badge/MSRV-1.89-43585A?logo=rust">
</p>

Hyphae is a local-first data engine written in Rust. One process owns one data
directory and exposes relational SQL, native structures, lexical search, and
vector search over a shared transaction, WAL, MVCC, recovery, and proof
substrate. The engine runs offline and does not embed PostgreSQL, Valkey,
OpenSearch, a cloud service, an embedding provider, or an LLM.

**Stable Native release:** Hyphae Native is the active architecture. G0
through G7 are closed for their versioned, bounded profiles. G7 uses an
environment-bound operational-scale authority; the exact-commit G8 release evidence is produced
by the release workflow. The
[native gate status](docs/gates/native-gate-status.md) is the current status
authority; temporary workflow artifacts alone do not close a gate.

**Release candidate:** `v1.1.0` will distribute the complete 24-crate Native
Rust ecosystem through crates.io and signed native archives through GitHub
Releases after its exact-SHA G8 gate closes.
The prior `0.2.1` compatibility publication receipt is retained at
[`docs/release/receipts/0.2.1.md`](docs/release/receipts/0.2.1.md).

G8 release evidence binds publication to the exact release commit. The G7
closure certifies the C-60 operational-scale control matrix but makes no
canonical dedicated-hardware latency, interference, or bare-metal claim.

## What Hyphae does

- Executes bounded SQL DDL, DML, prepared queries, secondary-index reads, and
  transactions over a Hyphae-owned relational engine.
- Provides native strings, counters, hashes, lists, sets, sorted sets, streams,
  TTL, scans, algebra, and atomic structure batches.
- Owns lexical, exact-vector, incremental ANN, filtered, faceted, metric, and
  same-snapshot hybrid search without an external search engine.
- Commits SQL, structure, and search mutations under one catalog, WAL, MVCC
  root set, commit sequence, scheduler, and stable object-ID namespace.
- Exposes one embedded Rust facade, local UDS/named-pipe protocol, CLI, optional
  loopback HTTP `/v2`, and typed Python and TypeScript SDKs.
- Creates Native checkpoints, proofs, witnesses, backups, restores, vacuum
  generations, and complete `doctor` reports with bounded failure behavior.
- Imports format-2 state offline into a separate pending Native directory,
  verifies equivalence, and requires explicit promotion.

See the [Native capability matrix](docs/product/native-capabilities.md),
including surface differences, boundaries, and deliberate non-capabilities.
The [published 0.2.1 matrix](docs/product/capabilities.md) remains separate.

## Install

Download the archive for your platform from the
`v1.1.0` GitHub release after its exact-SHA G8 gate closes,
then verify its checksum and Sigstore bundle before installing the `hyphae`
binary. To install from crates.io, run:

```bash
cargo install hyphae-cli --version 1.1.0 --locked
```

Build and embed the exact release from source with:

```bash
git checkout v1.1.0
cargo build --release --locked -p hyphae-cli
./target/release/hyphae version --json
```

The release contains Linux x64, macOS x64/arm64, and Windows x64 archives plus
checksums, SPDX/CycloneDX SBOMs, provenance, signatures, and attestations.

## Published 0.2.1 compatibility flow

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

These commands describe the published format-2 compatibility release, not the
Native `dev` command surface. The query response names the snapshot and anchor
needed by `hyphae verify`.
The [quickstart](docs/quickstart.md) covers Windows syntax, compaction,
restore, offline proof verification, the optional server, and clients.

## Architecture

```text
application
  ├─ embedded Rust facade ────────────────────────────────┐
  ├─ local CLI                                            │
  ├─ UDS / named-pipe clients (Rust / Python / TypeScript)│
  └─ optional loopback HTTP /v2 ──────────────────────────┤
                                                          ▼
                     Native product and transaction authority
                         │              │              │
                        SQL         structures       search
                         └──────────────┬──────────────┘
                                        ▼
                      catalog / WAL / MVCC / pages / blobs
                              checkpoints / backup / proofs
```

One operating-system lock gives one product instance exclusive ownership of a
Native data directory. See the
[Native architecture](docs/architecture/native-local-ecosystem.md) and
[versioned Native specifications](docs/README.md#understand-correctness).

## Public surfaces

| Surface | Purpose |
|---|---|
| `hyphae` binary | Native local operations, administration, migration, server, and verifier |
| `hyphae-native-product` | Curated embedded Native product facade |
| Native local protocol | Primary UDS/named-pipe multi-client transport |
| HTTP `/v2` | Optional loopback-first Native edge |
| `@celiums/hyphae` | TypeScript Native local-protocol and HTTP client |
| `hyphae-sdk` | Python Native local-protocol and HTTP client |
| `/v1` compatibility | Separately retained published format-2 HTTP product |

OpenAPI 3.1 and JSON Schema 2020-12 under `contracts/` are the canonical wire
contracts. Native uses `hyphae-v2.yaml` and `native-v2.schema.json`; the
published format-2 product retains `hyphae-v1.yaml`.

## Rust crates

The Native line is organized around `hyphae-native-product` (embedded facade),
`hyphae-native-runtime` (SQL, structures, search, transactions, and
scheduling), `hyphae-native-protocol`/`hyphae-native-daemon` (local transport),
and the owned `hyphae-native-{types,catalog,pages,blobs,wal,mvcc,btree,records,manifest,ann}`
storage and execution primitives. `hyphae-cli` builds the single product
binary.

The following crates are the published format-2 compatibility libraries at
`0.2.1`; their crates.io pages do not describe the unpublished Native facade:

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

- [Native capabilities and limits](docs/product/native-capabilities.md)
- [Native 1.0 quickstart](docs/quickstart-native.md)
- [Published 0.2.1 compatibility guide](docs/quickstart.md)
- [CLI reference](docs/cli/reference.md)
- [Configuration](docs/configuration.md)
- [Native product contract](docs/native/local-product-v1.md)
- [Native HTTP API v2](docs/native/http-v2.md)
- [Native local protocol](docs/native/local-protocol-v1.md)
- [Operations and troubleshooting](docs/operations/troubleshooting.md)
- [Security model](docs/security/threat-model.md)
- [Native local ecosystem target](docs/architecture/native-local-ecosystem.md)
- [Microsecond-first target](docs/performance/microsecond-first.md)
- [Release verification](docs/release/verification.md)
- [0.2.1 publication receipt](docs/release/receipts/0.2.1.md)
- [crates.io release procedure](docs/release/crates-io.md)

## Product boundary

Hyphae Native is a local, single-node data ecosystem with Hyphae-owned SQL,
structures, lexical search, and ANN under one durable authority. G0 through G6
are closed for their bounded contracts. G8 protects the exact release commit;
G7 remains a post-release performance certification and is not a release claim.

Hyphae is not Mycelium, Hyphae Network, Celiums Network, an AI cognition
runtime, a hosted SaaS, or a framework-specific data layer. The retained
`0.2.1` compatibility line does not include the Native SQL/structures/search
architecture. Replication, clustering, built-in TLS, at-rest encryption,
multitenancy, billing, a control plane, an embedding model, and an LLM are also
outside Native 1.0.

Hosted, distributed, and model-driven programs remain later phases. Applications
still own process supervision, remote TLS termination, filesystem permissions,
backup media policy, and optional embedding providers. Semantic providers can
supply vectors to the Rust APIs but never become a core dependency or source of
authority.

## Repository map

- `crates/`: Native product/runtime/storage/protocol crates, retained format-2
  libraries, public contracts, clients, servers, and the single CLI.
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

Software is licensed under [AGPL-3.0-only](LICENSE); documentation is licensed
under [CC-BY-SA-4.0](LICENSE-DOCUMENTATION). See the
[licensing policy](LICENSE-POLICY.md), [NOTICE](NOTICE), and
[third-party notices](THIRD_PARTY_NOTICES.md). Releases through `v1.0.1`
retain their original Apache-2.0 terms. The Hyphae name and visual identity are
covered separately by [TRADEMARKS.md](TRADEMARKS.md).
