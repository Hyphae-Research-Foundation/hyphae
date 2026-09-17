# Hyphae repository instructions

## Product boundary

- Hyphae is one autonomous Rust data engine. A node is one binary and one
  exclusively owned data directory; one node remains a complete offline
  product, and multiple nodes may form one self-hosted Hyphae cluster.
- The default product must work offline without another database, cache, cloud,
  embedding provider, or LLM.
- The Universal Engine program makes relations, keyspaces and data structures,
  documents, lexical and vector retrieval, time series, graphs, geospatial
  data, and columnar analytics first-class catalogued object domains.
- Public product language presents one engine with one object, query,
  transaction, administration, and proof authority. Technical specifications
  may name specialized execution domains and access methods where correctness
  requires it.
- Every domain shares Hyphae-owned types, catalog and entity identities,
  page/blob allocation, WAL, MVCC/commit sequencing, scheduling, memory policy,
  authorization, backup, and proofs. Domains are not wrappers, compatibility
  facades, or mandatory projections of one another.
- Do not introduce PostgreSQL, Valkey, OpenSearch, another database, or a
  third-party query/search engine as an internal runtime or sidecar. General
  purpose audited primitives remain allowed.
- Embedded calls and the native local protocol remain primary performance
  surfaces. Same-node domain execution never uses TCP, HTTP, JSON, RESP,
  PostgreSQL wire, or another serialized compatibility protocol.
- A self-hosted cluster uses a versioned Hyphae-native authenticated binary
  protocol. Strong consistency is the default: replicated metadata and shard
  authority must fail closed rather than silently weaken consistency.
- Single-node execution must not pay an artificial network or consensus round
  trip. It is the one-node form of the same product and retains direct fast
  paths.
- Treat "microsecond-first" as a measured hot-path objective. Report transport,
  execution, queueing, and physical durability separately; never promise a
  universal sub-millisecond bound for fsync, cold I/O, or unbounded queries.
- Accelerator-capable builds automatically select a validated compatible GPU,
  report the exact execution profile, and fall back to CPU only under the
  versioned operation contract. A GPU, model, or provider is never mandatory
  for the default offline product.
- PostgreSQL, Valkey, Qdrant, Weaviate, and other systems are external
  conformance and measurement subjects, never embedded authorities. Parity or
  superiority claims require pinned versions, configurations, hardware,
  datasets, durability, and retained receipts.
- Hosted SaaS concerns, billing, and cloud control planes remain outside this
  repository. The self-hosted cluster data plane is inside it. PliegoRS,
  Mycelium, Hyphae Network, Celiums Network, and cognitive experiments remain
  outside it.
- Integrations and semantic providers consume only public versioned contracts.
- Existing releases retain their bounded claims. A roadmap decision does not
  retroactively expand shipped behavior or gate evidence.

## Historical source

- Historical repositories are frozen read-only inputs.
- Do not copy or cherry-pick historical code without an accepted entry in
  docs/porting/ledger.md.
- Keep provenance, license, transformation, inherited tests, and human review
  explicit for every accepted port.

## Engineering rules

- Use English for code, contracts, commit messages, and repository docs.
- Keep unsafe Rust forbidden unless an accepted ADR narrows an audited use.
- Change public behavior contract-first.
- Add failure-path tests for durable behavior.
- Do not claim a roadmap phase complete without its exit evidence.
- Never add an automation attribution trailer to a commit.
- Every release updates `docs/gates/native-gate-status.md`,
  `config/native-gate-status.json`, `SECURITY.md` supported versions, and the
  crate/SDK READMEs' version pins.

## Cursor Cloud specific instructions

- This is a single Rust workspace (Cargo, no other services/databases). The
 pinned toolchain in `rust-toolchain.toml` (1.96.0, with `clippy`/`rustfmt`/
 `rust-docs`) is preinstalled; `rustup` auto-selects it inside `/workspace`.
- The startup update script runs `cargo fetch --locked` to prime the crate
 cache. It does not build; the first `cargo build`/`test` still compiles the
 full graph and can take a few minutes.
- `hyphae-cli` is the only executable (`default-members`). Build the dev binary
 with `cargo build --locked -p hyphae-cli` → `target/debug/hyphae`.
- Lint/test/doc commands live in `README.md` (Development) and
 `docs/development.md` (Required local checks); use those, e.g.
 `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`,
 `cargo test --workspace --all-features --locked`.
- The product is offline-first: no listener starts unless `hyphae serve` is
 run. To exercise the engines, `hyphae init --data-dir <new-dir>` then use the
 `sql`/`structure`/`search`/`doctor` subcommands (see
 `docs/quickstart-native.md`). The data dir must not already exist; reuse
 `--data-dir` to reopen it. Do not commit created data dirs.
- Some checks in `docs/development.md`/`CONTRIBUTING.md` are Python tools under
 `tools/` (e.g. `check_documentation.py`) and network-dependent security tools
 (`cargo deny`, `cargo audit`); these are optional for local dev and not part
 of the startup script.
