<!-- SPDX-License-Identifier: Apache-2.0 -->
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
| `getrandom` | operating-system entropy for durable resolution identities | allowed operating-system primitive |
| `uuid` | transaction/session ID construction | allowed primitive |
| `unicode-normalization`, `unicode-casefold` | analyzer primitives | allowed primitive |
| `tokio` | native daemon scheduling and asynchronous local IPC | allowed outside embedded hot path |
| `futures-channel` | one-shot completion handoff from the sole product owner to asynchronous transport adapters | allowed bounded async primitive |
| `subtle` | constant-time comparison of durable API-key verifier material | allowed cryptographic primitive |
| `windows-permissions` | safe process-SID and handle-based ACL operations for Windows credential files | allowed operating-system access-control wrapper |
| `interprocess` | safe UDS and Windows named-pipe local transport, peer credentials and endpoint ACLs | allowed operating-system IPC wrapper |
| `widestring` | safe UTF-16 security-descriptor construction for the Windows named-pipe ACL | allowed Windows encoding primitive |
| `recvmsg` | target-conditioned Windows named-pipe message primitive used by `interprocess` | allowed operating-system IPC wrapper |
| `doctest-file` | transitive documentation macro used by `interprocess` | allowed build primitive |
| `bytes`, `futures-core`, `mio`, `socket2`, `signal-hook-registry`, `errno`, `pin-project-lite` | transitive asynchronous and operating-system support for Tokio and IPC | allowed runtime primitives |
| `log` | Mio diagnostic facade activated by Cargo workspace feature unification through the TUI closure | allowed diagnostic primitive; not an isolated daemon dependency |
| `windows-sys`, `windows-link`, `wasi` | target-conditioned operating-system bindings for IPC and async I/O | allowed platform bindings |
| `r-efi`, `wasip2`, `wit-bindgen` | target-conditioned backends in the `getrandom` entropy closure | allowed platform bindings |
| `thiserror` | typed errors | allowed primitive |
| `proptest` | property tests | allowed test primitive |
| `redb` | shipped `0.2` reconstructible index | compatibility only; forbidden as native target evidence |

## Native product dependency capture

The native product daemon target path currently consists of:

- `hyphae-native-ann`;
- `hyphae-native-types`;
- `hyphae-native-pages`;
- `hyphae-native-product`;
- `hyphae-native-protocol`;
- `hyphae-native-daemon`;
- `hyphae-native-wal`;
- `hyphae-native-catalog`;
- `hyphae-native-mvcc`;
- `hyphae-native-records`;
- `hyphae-native-btree`;
- `hyphae-native-blobs`;
- `hyphae-native-manifest`; and
- `hyphae-native-runtime`.

The exact locked normal/build metadata closure is rooted at
`hyphae-native-daemon`, the local native product entry point. This root includes
the protocol, product facade, runtime, and every native engine package: 14
Hyphae-owned packages and 62 external package identities (60 package names) as
of 2026-08-14. The two identities for `syn` are 2.0.119 on the Tokio macro
path and 3.0.2 on the serde/thiserror derive paths. Target-conditioned
dependencies remain in scope even when they are not compiled on the audit
host.

Cargo resolves features across the workspace before the gate walks outward
from the daemon node. Consequently, this conservative metadata closure also
contains `log` 0.4.33 because the TUI's `crossterm` dependency activates Mio's
optional diagnostic feature. An isolated `cargo tree -p hyphae-native-daemon`
does not contain `log`; the allowlist records the wider feature-unified graph
honestly instead of presenting it as an isolated daemon dependency.

`hyphae-native-daemon` uses `interprocess` 2.4.3 as the reviewed safe OS IPC
wrapper. The Rust standard library has no Windows named-pipe server API;
`interprocess` supplies named-pipe server instances, peer process credentials,
and security-descriptor attachment without unsafe code in Hyphae-owned crates.
The reviewed wrapper does contain OS-boundary unsafe implementation blocks, so
it remains subject to the existing transitive unsafe audit rather than being
treated as safe-Rust-only. Its closure includes `doctest-file`, `futures-core`,
`recvmsg`, `widestring`, `windows-sys`, and `windows-link`, plus Tokio's
`bytes`, `mio`, `pin-project-lite`, `signal-hook-registry`, `errno`, `socket2`,
`tokio-macros`, and `syn` 2.x support. These are documentation, async runtime,
OS encoding, message, and API primitives rather than data-engine semantics.
The daemon applies a protected owner/system Windows DACL and a 0600 filesystem
mode on Unix.

`hyphae-native-product` and the CLI use `windows-permissions` 0.2.4 as the
reviewed safe adapter for process-token SIDs and file-handle security
descriptors. Restricted credential outputs retain an exclusive handle while a
protected current-account/LocalSystem DACL is applied and verified before any
secret bytes are written. Credential readers open the final component as a
reparse point with no sharing, reject reparse metadata, and validate that same
handle's owner and exact DACL before reading. Its target-conditioned closure
adds `bitflags` 1.3.2, `winapi` 0.3.9, and the GNU target import libraries.

The runtime directly uses `getrandom` 0.3.4 for unpredictable durable
transaction resolution identities. Its complete target-conditioned closure
includes `r-efi` 5.3.0 for UEFI and `wasip2` 1.0.4+wasi-0.2.12 with
`wit-bindgen` 0.57.1 for WASI preview2. `mio` also retains the target-conditioned
`wasi` 0.11.1+wasi-snapshot-preview1 binding. None of these packages supplies
database, transaction, protocol, or query semantics.

The historical `cargo tree -p hyphae-native-runtime --locked` capture on
2026-08-01 covered the engine closure before the G6 product facade, protocol,
and daemon existed. The current gate instead starts at `hyphae-native-daemon`
and therefore audits that complete native service closure. The remaining
external primitives include `blake3`, `crc32c`, `thiserror`, Unicode and
receipt JSON support. Their proc-macro/build dependencies include `arrayref`,
`arrayvec`, `cfg-if`, `constant_time_eq`, `cpufeatures`, `cc`,
`find-msvc-tools`, `shlex`, `rustc_version`, `semver`, `proc-macro2`, `quote`,
`syn` 3.x, and `unicode-ident`.

A case-insensitive source and manifest scan found no forbidden engine
dependency in those crates. The only product-name match was a crate-level
non-compatibility disclaimer. Direct native source contains no `unsafe` token.
The daemon-rooted transitive unsafe scan is required to report reviewed
third-party findings while keeping all 14 repository-owned packages at zero;
that report remains audit evidence, not an approval of third-party unsafe
implementations.

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
contains unsafe code requires an audit record. The unsafe scan follows the
same daemon-rooted metadata closure, so transitive Tokio and interprocess unsafe
implementation blocks are reported alongside the engine primitives. Every
reachable repository-owned crate must retain zero unsafe findings. Direct
unsafe code requires a separate accepted ADR with a narrow invariant, platform
matrix, fuzzing and review; it cannot enter through performance pressure alone.

## G0 exit audit

Before G0 closes:

1. every new runtime dependency has an inventory row;
2. `cargo tree` is captured for the exact commit;
3. licenses and notices are reviewed;
4. target crates are proven free of forbidden engines;
5. direct and transitive unsafe use is reported; and
6. the porting ledger confirms that no source was silently copied.

## Exact native-closure gate

The native dependency gate is rooted at the `hyphae-native-daemon` package. It
uses `cargo metadata --locked --format-version 1` and follows every non-dev
normal and build edge, including target-conditioned edges. Development-only
dependencies are outside this runtime closure and require their existing
workspace security gates instead.

The gate must fail when:

- the root package is missing or ambiguous;
- a reachable workspace package is outside the reviewed native package set;
- a reachable external package is absent from the machine-readable inventory;
- an inventoried external package is no longer reachable;
- two reachable package identities share one name and therefore expose a gate
  implementation limitation rather than being audited incorrectly;
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
diagnostics for reviewed external packages are retained in the receipt; parser
failure in repository-owned closure code is fatal. Diagnostics outside the
metadata closure cannot silently expand or invalidate the audited package set.
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
