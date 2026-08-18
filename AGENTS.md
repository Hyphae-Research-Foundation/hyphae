# Hyphae repository instructions

## Product boundary

- Hyphae is an autonomous Rust data engine: one binary and one data directory.
- The default product must work offline without a database, cache, cloud,
  embedding provider, or LLM.
- The phase-1 target is a fully Hyphae-owned local data ecosystem with three
  first-class native engines: relational/SQL, keyspace/data structures, and
  lexical/vector search.
- Those engines share Hyphae-owned types, catalog, page/blob allocation, WAL,
  MVCC/commit sequencing, scheduling, memory policy, backup, and proofs. They
  are not wrappers, compatibility facades, or projections of one another.
- Do not introduce PostgreSQL, Valkey, OpenSearch, another database, or a
  third-party query/search engine as an internal runtime or sidecar. General
  purpose audited primitives remain allowed.
- Embedded calls and the native local protocol are the primary performance
  surfaces. No internal engine-to-engine path may use TCP, HTTP, JSON, or
  another serialized compatibility protocol.
- Treat "microsecond-first" as a measured hot-path objective. Report transport,
  execution, queueing, and physical durability separately; never promise a
  universal sub-millisecond bound for fsync, cold I/O, or unbounded queries.
- PliegoRS, Mycelium, Hyphae Network, Celiums Network, cognitive experiments,
  hosted SaaS concerns, billing, and cloud operations are outside this repo.
- Integrations and semantic providers consume only public versioned contracts.

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
