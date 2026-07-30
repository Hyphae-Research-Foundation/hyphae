# Development guide

Hyphae uses Rust 2024 with MSRV 1.89.0 and a pinned validation toolchain. Code,
contracts, commit messages, and repository documentation are English. Unsafe
Rust is forbidden workspace-wide.

## Workspace map

The dependency direction is intentionally one-way:

```text
core / query / retrieval / storage
              ↓
            engine
              ↓
contracts → server ← client
              ↓
             CLI
```

`hyphae-cli` is the only executable artifact. Optional integrations consume
public clients and contracts; they cannot import storage or engine internals.
See [architecture](architecture/overview.md) for the durable flow.

## Required local checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
python tools/generate_sdk_models.py --check
python tools/check_documentation.py --binary target/debug/hyphae
python tools/run_documentation_examples.py --binary target/debug/hyphae
python packaging/test_package.py
```

Also run the common client and integration suites when those surfaces change:

```bash
python tools/run_conformance.py
python tools/check_integration_boundaries.py
python tools/run_integration_conformance.py
```

Security/release workflows add RustSec, cargo-deny, dependency integrity,
secret history scanning, fuzzing, load/kill recovery, native package smoke,
SBOMs, checksums, signatures, and attestations.

## Contract-first changes

For a public `/v1` behavior change:

1. update typed models in `hyphae-contracts`;
2. update OpenAPI 3.1 and the affected JSON Schema 2020-12 files;
3. regenerate TypeScript/Python models and require `--check` to be clean;
4. update server/client/CLI/MCP behavior and common conformance fixtures;
5. update the human API, client, capability, and example documentation;
6. use a new API path version for a breaking wire change.

Generated SDK model files are checked in so consumers can audit exact public
types. Do not hand-edit them.

## Durable changes

Any change to log, mutation, document, snapshot, manifest, proof, backup, or
data-directory behavior requires:

- an accepted ADR when the invariant or compatibility policy changes;
- a versioned normative format update;
- crash/fault/corruption tests for every commit boundary;
- immutable compatibility fixtures for every supported disk format;
- explicit migration and rollback documentation.

The log remains authority and indexes remain rebuildable. Never introduce a
mandatory network, database, cache, model, or cloud dependency.

## Documentation changes

Every shipped capability needs a discoverable page or section, an accurate
example, limitations, and a link from [the documentation index](README.md).
The documentation checker verifies local links, index coverage, JSON example
syntax, and CLI command inventory against the built binary. The documentation
example runner starts a private loopback server and executes the maintained
KV/query/aggregation, vector mutation, and exact/lexical/hybrid retrieval
request files end to end.

Do not describe roadmap candidates as current behavior. If code and docs
disagree, correct both or state the limitation explicitly; do not weaken a
gate claim through vague wording.

## Historical source and dependencies

Frozen historical repositories are read-only inputs. No source or test enters
this repository without a reviewed entry in the [porting ledger](porting/ledger.md)
covering provenance, license, transformation, inherited tests, and human
review. New third-party dependencies require license/source policy review and
locked integrity metadata.

## Release discipline

Any source or documentation commit invalidates release-candidate closure until
the complete hosted matrix passes on that exact SHA. Do not tag or publish a
new version until the release gate is green and publication is explicitly
authorized. Automation attribution trailers are forbidden.
