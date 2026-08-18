# ADR-0005: Workspace layers have one-way dependencies

- Status: Accepted
- Date: 2026-07-15
- Owners: Celiums Solutions LLC

## Context

Storage internals, delivery surfaces, generated wire models, and optional
providers must evolve independently. Cycles or framework imports would make
internal implementation details part of the product contract.

## Decision

The initial workspace contains core, storage, query, retrieval, contracts,
server, client, and CLI crates. Core owns stable values only. Storage, query,
and retrieval may depend on core. Server and client may depend on contracts
and core, but never on each other. The CLI composes public libraries and is
the only executable artifact.

The future engine coordinator and embeddable facade are added only when the
durability primitives they expose are real. SDKs, MCP, providers, and
framework adapters remain outside the Rust core dependency graph.

Amendment 2026-07-15: `hyphae-engine` now implements that coordinator after
the durable phase-2 gate. It depends on storage, query, and retrieval; those
lower crates remain independent of the facade.

Amendment 2026-08-18: the Native generation adds its own bottom-up layer
stack, and the original sentence-level edges no longer describe the complete
graph. The normative rules are now:

- Native substrate crates (`hyphae-native-types`, `-pages`, `-wal`, `-mvcc`,
  `-catalog`, `-records`, `-ann`, `-btree`, `-blobs`, `-manifest`) depend
  only downward, terminating at `hyphae-native-types`.
- `hyphae-native-runtime` composes the substrate; `hyphae-native-product`
  wraps the runtime behind the curated product/dispatcher facade;
  `hyphae-native-protocol` encodes product operations;
  `hyphae-native-daemon` serves protocol clients. No substrate crate depends
  upward.
- Format-2 crates keep their original one-way order, with two recorded
  extensions: `hyphae-storage` also depends on `hyphae-query` and
  `hyphae-retrieval` for materialized query/lexical indexes, and
  `hyphae-contracts` depends on `hyphae-query` for wire-model conversion.
- `hyphae-server` hosts both the `/v1` format-2 adapter and the `/v2` Native
  adapter, so it depends on the format-2 stack and on
  `hyphae-native-product`/`-protocol`. `hyphae-client` mirrors that on the
  consumer side. Server and client still never depend on each other as
  normal dependencies; their path-only development-dependency edge exists
  solely for conformance tests and is stripped from published manifests.
- The MCP stdio adapter lives in `hyphae-cli` and consumes only the public
  HTTP `/v2` client; SDKs and framework adapters remain outside the Rust
  core dependency graph.
- The machine-enforced authority for the Native closure is
  `config/native-dependency-policy.json` with
  `tools/check_native_dependencies.py`; the publication layer order is
  `config/crates-io-release.json` with `tools/check_crate_packages.py`.

## Consequences

- Internal formats can change without leaking into clients.
- Dependency direction is reviewable from Cargo metadata.
- Some early crates intentionally expose no public behavior until their
  invariants have tests.

## Verification

CI builds every workspace target. Architecture tests will reject forbidden
dependency edges when the coordinator is introduced.
