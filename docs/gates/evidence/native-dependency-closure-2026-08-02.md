# Native dependency-closure evidence — 2026-08-02

## Scope

This evidence binds the first machine-enforced G0 dependency closure for the
Hyphae-owned native runtime. It proves the exact locked normal/build graph
reachable from `hyphae-native-runtime`, excludes dev-only edges, validates the
reviewed package allowlist, rejects named upstream engines, verifies workspace
unsafe-lint inheritance, and reports host-observed transitive unsafe syntax.

It does not prove that every third-party unsafe block is semantically sound,
that a WSL2 host activates every target-conditioned implementation, or that G0
is complete.

## Source identity

- source commit:
  `fc8dbc9f1f849e8a10302d4595b383168331281b`;
- source tree:
  `2ef9efcbe8c1e6df1458c941bb8e9e25df27e121`;
- branch at capture: `codex/native-g0-dependency-gate`;
- worktree before receipt generation: clean; and
- raw receipt:
  [`native-dependency-closure-wsl2.json`](native-dependency-closure-wsl2.json).

The receipt contains no user-home or checkout absolute paths. Machine-specific
prefixes are normalized to `<repo>`, `<cargo-home>`, and `<home>`.

## Gate contract

The canonical policy is
[`config/native-dependency-policy.json`](../../../config/native-dependency-policy.json).
The implementation is
[`tools/check_native_dependencies.py`](../../../tools/check_native_dependencies.py),
with nine focused negative/positive tests in
[`tools/test_check_native_dependencies.py`](../../../tools/test_check_native_dependencies.py).

The closure follows every `cargo metadata --locked --format-version 1` edge
whose kind is normal or build. A package must match one exact reviewed
name/version/source/license record. Reachable workspace packages must match
the exact native crate set and inherit the workspace
`unsafe_code = "forbid"` lint. Any reachable forbidden engine, stale
inventory entry, unreviewed package, missing native `cargo-geiger` metric,
native unsafe finding, or in-closure parser gap fails closed.

The security workflow runs the unit tests and complete gate on every pull
request, uploads the JSON receipt, and continues to run the existing
RustSec/cargo-deny and secret-history checks.

## Commands

The clean evidence lane ran under WSL2 with `/usr/bin/git` selected ahead of a
local incompatible Git wrapper:

```text
PATH="/usr/bin:$HOME/.cargo/bin:$PATH" \
python3 tools/check_native_dependencies.py \
  --require-clean \
  --output docs/gates/evidence/native-dependency-closure-wsl2.json
```

The gate internally recorded successful executions of:

```text
cargo metadata --locked --format-version 1
cargo deny check
cargo geiger --manifest-path <repo>/crates/hyphae-native-runtime/Cargo.toml \
  --all-features --build-dependencies --locked \
  --output-format Json --color never --quiet
```

Focused gate tests:

```text
python -m unittest tools/test_check_native_dependencies.py -v
```

Result: 9 tests passed. The pre-implementation run failed because the gate
module did not exist. A later regression run reproduced path/target
cross-contamination; per-host target isolation and receipt path sanitization
were then added before this clean capture.

## Environment

- Linux `6.18.33.1-microsoft-standard-WSL2`, x86_64;
- Python `3.13.5`;
- Git `2.47.3`, executable `/usr/bin/git`;
- Rust `1.96.0 (ac68faa20 2026-05-25)`;
- Cargo `1.96.0 (30a34c682 2026-05-25)`;
- cargo-deny `0.20.2`; and
- cargo-geiger `0.13.0`.

The Windows host could execute development scans, but an Application Control
policy later blocked a newly emitted `proc-macro2` build-script executable
with OS error 4551. No Windows clean receipt is claimed. The hosted gate uses
Linux, and this checked-in receipt uses WSL2.

## Results

| Observation | Result |
|---|---:|
| Reachable packages | 30 |
| Hyphae-owned native workspace packages | 11 |
| Reviewed external packages | 19 |
| Reachable forbidden engines | 0 |
| Native unsafe findings | 0 |
| External used unsafe syntax findings on this host | 873 |
| External used plus unused unsafe syntax findings | 6,488 |
| In-closure cargo-geiger parse failures | 0 |
| Out-of-closure parse diagnostics retained | 2 |
| Used-but-unscanned paths retained | 6 |
| Failed gate commands | 0 |

The external unsafe figures are cargo-geiger syntax counters across functions,
expressions, impls, traits, and methods. They are not unique unsafe blocks,
vulnerabilities, exploitability findings, or an approval. The largest counts
come from the reviewed primitive closure around BLAKE3, CRC32C, platform
feature detection, build tooling, and proc-macro parsing.

The two retained parser diagnostics name `signal-hook-registry 1.4.8` and
`unicode-casefold 0.2.0`. Neither is in the metadata closure rooted at
`hyphae-native-runtime`; the gate does not silently add them. The six
used-but-unscanned paths are one external C source, one external README, three
generated CRC tables, and one generated thiserror file. No Hyphae-owned native
source file was excluded.

The 19 external records are only audited primitives and their build/proc-macro
support. The closure contains no PostgreSQL, SQLite, MySQL, Redb, RocksDB,
Valkey/Redis client, OpenSearch/Elasticsearch, DataFusion, DuckDB, Tantivy,
upstream HNSW, or vector-database package.

## Remaining boundaries

- Semantically review the reported unsafe use and target-specific
  implementations rather than treating counts as approval.
- Add another host/target lane if its conditional closure differs from WSL2.
- Keep cargo-geiger parser limitations explicit; an in-closure parser gap is
  already fatal.
- License expressions pass cargo-deny and exact metadata comparison, but
  release notice assembly remains a separate release gate.
- Porting provenance remains governed by the human-reviewed allowlist in
  `docs/porting/ledger.md`; dependency metadata cannot prove source authorship.
- G0 still requires the complete cross-crate golden corpus, benchmark/quality
  corpus, and remaining implementation-facing conformance tests.
- G1 through G8 remain open.

This receipt advances the dependency-inventory portion of G0. It closes no
phase or engine gate.
