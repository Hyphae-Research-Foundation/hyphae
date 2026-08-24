<p align="center">
  <a href="https://hyphae.dev" aria-label="Hyphae website">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Hyphae-Research-Foundation/hyphae/main/.github/assets/hyphae-lockup-reversed.svg">
      <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/Hyphae-Research-Foundation/hyphae/main/.github/assets/hyphae-lockup.svg">
      <img alt="Hyphae" src="https://raw.githubusercontent.com/Hyphae-Research-Foundation/hyphae/main/.github/assets/hyphae-lockup.svg" width="420">
    </picture>
  </a>
</p>

<p align="center"><strong>Data that can prove itself.</strong></p>

<p align="center">
  <a href="https://github.com/Hyphae-Research-Foundation/hyphae/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/Hyphae-Research-Foundation/hyphae/ci.yml?branch=main&amp;label=CI&amp;logo=github"></a>
  <a href="https://crates.io/crates/hyphae-engine"><img alt="crates.io" src="https://img.shields.io/crates/v/hyphae-engine?logo=rust"></a>
  <a href="https://docs.rs/hyphae-engine"><img alt="docs.rs" src="https://img.shields.io/docsrs/hyphae-engine?logo=docs.rs"></a>
  <a href="https://github.com/Hyphae-Research-Foundation/hyphae/releases/latest"><img alt="GitHub release" src="https://img.shields.io/github/v/release/Hyphae-Research-Foundation/hyphae?logo=github"></a>
  <a href="https://hyphae.dev"><img alt="Website" src="https://img.shields.io/badge/website-hyphae.dev-8FCBC6"></a>
  <a href="LICENSE-POLICY.md"><img alt="License" src="https://img.shields.io/badge/code-Apache--2.0-C86F4A"></a>
  <img alt="MSRV 1.89" src="https://img.shields.io/badge/MSRV-1.89-43585A?logo=rust">
</p>

Hyphae is a local-first data engine written in Rust. One process owns one data
directory and exposes relational SQL, native structures, lexical search, and
vector search over a shared transaction, WAL, MVCC, recovery, and proof
substrate. The engine runs offline and does not embed PostgreSQL, Valkey,
OpenSearch, a cloud service, an embedding provider, or an LLM.

**Stable Native release:** [`v2.1.0`](https://github.com/Hyphae-Research-Foundation/hyphae/releases/tag/v2.1.0)
is the latest release of the active Native architecture. G0 through
G8 are closed for their versioned, bounded profiles. G7 uses an
environment-bound operational-scale authority; G8 binds the release archives,
SBOMs, signatures, provenance, and fault matrices to the exact release
commit. The [native gate status](docs/gates/native-gate-status.md) is the
current status authority; temporary workflow artifacts alone do not close a
gate.

**Published release:** the 1.2 program ships durable native access control,
the operator console, agent plugins, the published Python client, and the
Apache-2.0 software transition. [`v1.2.0`](https://github.com/Hyphae-Research-Foundation/hyphae/releases/tag/v1.2.0)
closed the exact-SHA release gates with signed GitHub Release archives;
`v2.1.0` re-issues that program from the current integration tree as the
Apache-2.0 registry publication for crates.io, PyPI, and npm. Until a given
registry publication completes, the latest already-published registry version
is `1.1.0` under that release's original terms; its exact checksums, source
identities, workflow runs, and consumer verification are in the
[1.1.0 publication receipt](docs/release/receipts/1.1.0.md).

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

## Operational-scale measurements

The G7 authority exercised all 11 surfaces at C1, C8, and C32 on a DigitalOcean
`c-60-intel` VM with 60 vCPUs and 120 GiB RAM. Each cell used 1,000,000
measured observations plus 100,000 warmup observations per surface against
1,000,000 documents and 1,000,000 384-dimensional vectors. Queueing time is
included. Values below are observed p50 latency in microseconds.

| Surface | C1 p50 | C8 p50 | C32 p50 |
|---|---:|---:|---:|
| Embedded structure point get | 2.634 µs | 352.043 µs | 7,026.185 µs |
| Embedded prepared SQL primary key | 3.710 µs | 10.867 µs | 7,224.293 µs |
| Local structure point get | 123.323 µs | 153.425 µs | 303.213 µs |
| Local prepared SQL primary key | 128.761 µs | 126.416 µs | 287.156 µs |
| Indexed SQL bounded read | 45.443 µs | 399.870 µs | 8,340.482 µs |
| Two-index join bounded read | 38.325 µs | 325.779 µs | 8,306.193 µs |
| BM25 top 10 | 1.595 µs | 3.168 µs | 64.335 µs |
| Filtered BM25 top 10 | 1.660 µs | 3.250 µs | 65.369 µs |
| ANN top 10 | 115.995 µs | 139.118 µs | 8,397.640 µs |
| Hybrid top 10 | 118.190 µs | 141.885 µs | 8,409.802 µs |
| Strict group commit | 138.587 ms | 141.622 ms | 130.425 ms |

The matrix completed 33/33 surface-concurrency cells: 33,000,000 measured
observations and 3,300,000 warmups. ANN recall@10 was `1.0` in every cell.
Strict group commit completed 3,000,000 logical commits with 3,000,000 distinct
CSNs; recovery reported zero missing and zero mismatched commits.

These VM measurements establish functional concurrency, accounting,
correctness, recall, and durable recovery at operational scale. They do **not**
certify portable latency, dedicated hardware, bare metal, or background
interference. The [machine-readable measurements](docs/gates/evidence/native-g7-provisional-do-c60-2026-08-13.json),
[method and limitations](docs/gates/evidence/native-g7-provisional-do-c60-2026-08-13.md),
and [G7 closure authority](docs/gates/evidence/closures/native-g7-ff188af.json)
retain the exact source, environment, contract, and artifact hashes. The
machine-readable measurement file has SHA-256
`95a78beb031de35a3d2532c7536d76491d6273f7b8ce74c6b3976038bc8c3234`.

## Install

Download the archive for your platform from the
[`v2.1.0` GitHub release](https://github.com/Hyphae-Research-Foundation/hyphae/releases/tag/v2.1.0),
then verify its checksum and Sigstore bundle before installing the `hyphae`
binary. To install from crates.io, run:

```bash
cargo install hyphae-cli --version 2.1.0 --locked
```

That coordinate is valid once the `v2.1.0` publication completes; the latest
already-published version is `1.1.0`. Build and embed the exact release from
source with:

```bash
git checkout v2.1.0
cargo build --release --locked -p hyphae-cli
./target/release/hyphae version --json
```

The release contains Linux x64, macOS x64/arm64, and Windows x64 archives plus
checksums, SPDX/CycloneDX SBOMs, provenance, signatures, and attestations.

## Legacy 0.2.1 compatibility flow

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
| `@hyphae_/hyphae` | TypeScript Native local-protocol and HTTP client |
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

Version `2.1.0` publishes the complete 24-crate graph:

- contracts and shared APIs: `hyphae-core`, `hyphae-contracts`,
  `hyphae-query`, and `hyphae-retrieval`;
- owned storage and indexing: `hyphae-native-types`, `hyphae-native-ann`,
  `hyphae-native-catalog`, `hyphae-native-mvcc`, `hyphae-native-pages`,
  `hyphae-native-records`, `hyphae-native-wal`, `hyphae-native-blobs`,
  `hyphae-native-btree`, and `hyphae-native-manifest`;
- runtime and public access: `hyphae-native-runtime`,
  `hyphae-native-product`, `hyphae-native-protocol`,
  `hyphae-native-daemon`, `hyphae-engine`, `hyphae-storage`,
  `hyphae-client`, and `hyphae-server`; and
- distribution and adapters: `hyphae-cli` and `hyphae-pliegors`.

The compatibility crates remain available as part of that graph:

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
- [Native quickstart](docs/quickstart-native.md)
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
- [1.1.0 publication receipt](docs/release/receipts/1.1.0.md)
- [G7 operational-scale measurements](docs/gates/evidence/native-g7-provisional-do-c60-2026-08-13.md)
- [0.2.1 publication receipt](docs/release/receipts/0.2.1.md)
- [crates.io release procedure](docs/release/crates-io.md)

## Product boundary

Hyphae Native is a local, single-node data ecosystem with Hyphae-owned SQL,
structures, lexical search, and ANN under one durable authority. G0 through G8
are closed for their bounded contracts. G7 is an operational-scale C-60
authority with the explicit hardware and latency non-claims above; G8 protects
the exact release commit.

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

Software and normative specifications are licensed under
[Apache-2.0](LICENSE); narrative documentation is licensed under
[CC-BY-SA-4.0](LICENSE-DOCUMENTATION). See the
[licensing policy](LICENSE-POLICY.md), [NOTICE](NOTICE), and
[third-party notices](THIRD_PARTY_NOTICES.md). Published releases retain their
original terms, including `AGPL-3.0-only` for `v1.1.0`. The Hyphae name and
visual identity are covered separately by [TRADEMARKS.md](TRADEMARKS.md).
