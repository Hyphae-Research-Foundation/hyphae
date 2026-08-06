# Native clean-room and dependency inventory

Status: normative G0 policy; the exact native-closure gate, machine-readable
policy, unit tests, pull-request CI, and a clean WSL2 receipt are implemented;
semantic review of reported third-party unsafe use remains pending

Hyphae owns database, transaction, SQL, structure, lexical and ANN semantics.
This document distinguishes permitted primitives from forbidden target-engine
substitution.

## Forbidden target-path dependencies

The native substrate and engines must not use:

- PostgreSQL, SQLite, MySQL or another relational engine;
- Valkey, Redis or another key/structure server;
- OpenSearch, Elasticsearch, Lucene or another search engine;
- Redb, RocksDB, LMDB or another storage engine as the target page/WAL/MVCC
  implementation;
- DataFusion, DuckDB or another query optimizer/executor as Hyphae SQL;
- Tantivy or another inverted-index engine as Hyphae Search; or
- an upstream HNSW/vector database implementation as Hyphae ANN.

Existing `0.2` compatibility code may continue to depend on Redb until the
native migration gate retires that path. It cannot be used as G1 evidence.

## Permitted primitive categories

Permitted audited dependencies include:

- CRC, BLAKE3 and established cryptography/TLS;
- Unicode normalization, case folding and encoding validation;
- compression codecs;
- UUID generation and parsing;
- operating-system file, socket and synchronization wrappers;
- async runtimes, logging, metrics and command-line parsing;
- property-testing, fuzzing and benchmark harnesses; and
- memory-safe concurrency primitives that do not supply database semantics.

Adding a primitive requires license, maintenance, unsafe-code, transitive
dependency, performance and replacement-cost review.

## Current reusable dependencies

| Dependency | Target use | Decision |
|---|---|---|
| `blake3` | digests, content identity | allowed primitive |
| `crc32c` | corruption detection | allowed primitive |
| `uuid` | transaction/session ID construction | allowed primitive |
| `unicode-normalization`, `unicode-casefold` | analyzer primitives | allowed primitive |
| `tokio` | optional local server/runtime | allowed outside embedded hot path |
| `thiserror` | typed errors | allowed primitive |
| `proptest` | property tests | allowed test primitive |
| `redb` | shipped `0.2` reconstructible index | compatibility only; forbidden as native target evidence |

## Native product dependency capture

The native product target path currently consists of:

- `hyphae-native-ann`;
- `hyphae-native-types`;
- `hyphae-native-pages`;
- `hyphae-native-product`;
- `hyphae-native-wal`;
- `hyphae-native-catalog`;
- `hyphae-native-mvcc`;
- `hyphae-native-records`;
- `hyphae-native-btree`;
- `hyphae-native-blobs`;
- `hyphae-native-manifest`; and
- `hyphae-native-runtime`.

The historical `cargo tree -p hyphae-native-runtime --locked` capture on
2026-08-01 covered the engine closure before the G6 product facade existed.
The current gate is rooted at `hyphae-native-product`, which adds only that
Hyphae-owned facade to the reviewed runtime closure. The captured external
primitives include `blake3`, `crc32c`, `thiserror`, Unicode and receipt JSON
support. Their proc-macro/build dependencies include `arrayref`,
`arrayvec`, `cfg-if`, `constant_time_eq`, `cpufeatures`, `cc`,
`find-msvc-tools`, `shlex`, `rustc_version`, `semver`, `proc-macro2`, `quote`,
`syn`, and `unicode-ident`.

A case-insensitive source and manifest scan found no forbidden engine
dependency in those crates. The only product-name match was a crate-level
non-compatibility disclaimer. Direct native source contains no `unsafe`
token. This is implementation inventory evidence, not the still-pending
transitive unsafe and license audit required to close G0.

The ANN, records, B+tree, blob, and manifest crates add no new third-party
runtime dependency category. The runtime reaches the integrated crates only
through workspace path dependencies; the standalone ANN kernel currently
uses the permitted BLAKE3 primitive, the B+tree uses the Hyphae page and
buffer-pool APIs, the blob store uses the permitted CRC32C/BLAKE3 primitives,
and the checkpoint path uses the Hyphae WAL and MVCC types.

## Upstream research

Official documentation, public specifications, papers, benchmark methods and
black-box behavior may inform pain-point analysis and independent tests. They
do not become normative Hyphae behavior automatically.

Source reuse is file-by-file only after an accepted
[porting-ledger](../porting/ledger.md) entry records provenance, license,
transformation, inherited tests and human review. No PostgreSQL, Valkey,
OpenSearch, Lucene or historical Hyphae code is copied by this G0 work.

## Unsafe code

Workspace code remains `unsafe_code = "forbid"`. A primitive dependency that
contains unsafe code requires an audit record. Direct unsafe code requires a
separate accepted ADR with a narrow invariant, platform matrix, fuzzing and
review; it cannot enter through performance pressure alone.

## G0 exit audit

Before G0 closes:

1. every new runtime dependency has an inventory row;
2. `cargo tree` is captured for the exact commit;
3. licenses and notices are reviewed;
4. target crates are proven free of forbidden engines;
5. direct and transitive unsafe use is reported; and
6. the porting ledger confirms that no source was silently copied.

## Exact native-closure gate

The native dependency gate is rooted at the `hyphae-native-product` package. It
uses `cargo metadata --locked --format-version 1` and follows every non-dev
normal and build edge, including target-conditioned edges. Development-only
dependencies are outside this runtime closure and require their existing
workspace security gates instead.

The gate must fail when:

- the root package is missing or ambiguous;
- a reachable workspace package is outside the reviewed native package set;
- a reachable external package is absent from the machine-readable inventory;
- an inventoried external package is no longer reachable;
- a package version, registry source, or declared license differs from its
  reviewed inventory record;
- a forbidden database, structure, query, search, or vector engine appears
  anywhere in the closure;
- the workspace no longer sets `unsafe_code = "forbid"` or a reachable native
  crate stops inheriting workspace lints;
- `cargo-geiger` omits metrics for a reachable native package, reports direct
  unsafe usage in one, or cannot parse a file belonging to the closure; or
- a clean evidence run is requested from a dirty worktree.

The machine-readable inventory records the review category and rationale for
every external package. It is an exact allowlist, not a permissive prefix or
license-only filter. Package updates therefore require an intentional policy
change and review. The canonical allowlist is
[`config/native-dependency-policy.json`](../../config/native-dependency-policy.json).

`cargo-geiger` is evidence rather than an authority over package semantics.
Unsafe counts in reviewed third-party primitives are reported by package and
may be nonzero. Unsafe counts in Hyphae-owned native crates must be zero. Parse
diagnostics for packages outside the metadata closure are retained in the
receipt but cannot silently expand or invalidate the audited package set.
The first clean capture is the
[2026-08-02 native dependency-closure
evidence](../gates/evidence/native-dependency-closure-2026-08-02.md).

The JSON receipt must bind:

- schema version and gate implementation version;
- Git commit, source tree, and clean-worktree state;
- Rust, Cargo, `cargo-deny`, and `cargo-geiger` versions;
- the exact root and complete ordered package closure;
- source, license, dependency kind, and review rationale for each external
  package;
- direct and transitive unsafe counts plus any scan exclusions;
- forbidden-package and workspace-lint results; and
- the exact commands and exit status used for metadata, policy, license, and
  unsafe analysis.

The receipt closes only this dependency-inventory portion of G0. It does not
substitute for golden encodings, the benchmark/quality corpus, implementation
conformance, or the remaining phase gates.
