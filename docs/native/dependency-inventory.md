# Native clean-room and dependency inventory

Status: normative G0 policy; implementation inventory in progress

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

## Native-slice dependency capture

The experimental target path currently consists of:

- `hyphae-native-ann`;
- `hyphae-native-types`;
- `hyphae-native-pages`;
- `hyphae-native-wal`;
- `hyphae-native-catalog`;
- `hyphae-native-mvcc`;
- `hyphae-native-records`;
- `hyphae-native-btree`;
- `hyphae-native-blobs`;
- `hyphae-native-manifest`; and
- `hyphae-native-runtime`.

`cargo tree -p hyphae-native-runtime --locked` on 2026-08-01 showed only the
workspace crates above plus `blake3 1.8.5`, `crc32c 0.6.8`, and `thiserror
2.0.19` at runtime. Their proc-macro/build dependencies were `arrayref`,
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
