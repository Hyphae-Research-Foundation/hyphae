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
