# Native direct secondary-index execution evidence

Date: 2026-08-01

Status: first exact physical secondary-index and prepared-SQL read vertical;
G0, G1, G2, and G7 remain open

Source commit:
`62267d5196a954e4e582e85cbb6b1f48ab70248c`

Source tree:
`737d3348282c8f6013e665d5e1dedbd6c3210cea`

Branch: `main`

## Change

The earlier secondary-index source commit persisted and validated exact-key
projections but its general SQL read path first materialized the complete
snapshot. This source commit adds a separate current-root path:

- `prepare_sql_latest` captures one root-set snapshot, decodes only the
  catalog root, and binds the statement to that catalog version;
- prepared primary/secondary plans own the immutable relation and index
  definitions needed for parameter binding and tuple decoding;
- `execute_prepared_latest` captures one current root set, rejects a changed
  catalog version, and executes against physical relational pages;
- `select_latest_secondary_index` exposes exact native index-to-row lookup
  without constructing `MaterializedState`; and
- the historical `NativeSnapshot` path remains unchanged for complete
  all-engine and retained-root semantics.

No PostgreSQL, SQLite, DataFusion, Valkey, OpenSearch, or other external data
engine participates in this path.

## Physical read

One exact lookup:

1. reads the 32-byte `HYRIDX01` metadata by index `ObjectId`;
2. constructs prefix `0x04 || index_id || u32be(key_length) || index_key`;
3. performs a separator-pruned buffered prefix traversal;
4. decodes and validates every reached entry identity and live/tombstone byte;
5. follows each live primary key through the same captured relational root;
6. resolves the visible canonical row/version/blob; and
7. returns owned rows in canonical primary-key byte order.

Missing metadata returns `UnknownSecondaryIndex`. A malformed identity,
marker, dangling live projection, invalid version chain, or corrupt row fails
closed. SQL equality with a null parameter returns an empty result before a
physical index call.

The prefix scan and result currently allocate owned vectors. This milestone is
direct and buffered, not an allocation-free streaming cursor.

## Catalog and snapshot semantics

A prepared latest plan embeds the exact relation/index definitions used by the
binder and retains its catalog version. Data-only commits do not invalidate
the plan; a DDL commit does. Every execution captures one immutable root set,
checks the plan against that root's catalog version, and uses the same
relational root and visible CSN for entries and rows.

The materialized `NativeSnapshot` executor still supports its original
historical semantics. The equivalence fixture proves the physical and
materialized executors return the same rows, then changes the catalog:
current-root execution rejects the stale plan while the retained historical
snapshot continues to execute it against its original catalog and data.

## Verification

The source commit contains 56 `hyphae-native-runtime` tests. New coverage
proves:

- exact physical and materialized prepared result equivalence;
- 512 typed rows and a multilevel relational B+tree;
- a non-unique exact key returning three rows in deterministic primary-key
  order;
- every returned physical row equals a direct primary-key read;
- SQL null equality returns no rows;
- unknown index metadata has a typed failure;
- DDL invalidates current-root plans but not retained historical snapshots;
- strict reopen preserves both physical entries and prepared results; and
- a forged noncanonical live marker fails the direct path.

The first targeted command used an incomplete `--exact` name and selected zero
tests; it is excluded from evidence. The corrected canonical test and the
broader filter selected and passed their intended tests.

Windows, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tools/check_documentation.py
git diff --check
```

Debian GNU/Linux 13 under WSL2 passed:

```text
cargo test --workspace --all-features --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Ordinary tests are not represented as a
substitute.

## Matched latency observation

The [machine-readable schema-v7
receipt](native-microsecond-smoke-secondary-sql-wsl2.json) was produced from
the clean source commit under Debian 13/WSL2, release profile, warm state,
memory durability, concurrency one, a 2,048-row unique text index, one-row
selectivity, and a height-two relational B+tree.

Each secondary sample times one complete call rather than a batch average:

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical exact secondary index to owned row | 8.514 us | 11.183 us | 22.144 us | 97.363 us | 107,245 ops/s |
| Physical prepared SQL, bind through two projected values | 9.497 us | 13.338 us | 27.479 us | 113.768 us | 94,779 ops/s |

This single observation is inside the provisional 50-us p50 and 250-us p99
target for a bounded indexed SQL query. It does not pass G7. It has no
concurrency sweep, saturation, background interference, cold state,
allocation/RSS, hardware counters, UDS/named-pipe transport, dedicated
hardware, or mutation/durability timing. The corpus, query, selectivity, and
result size are deliberately narrow.

## Remaining boundary

Still required:

- allocation-free streaming equality/range cursors and bounded request arenas;
- non-unique fanout, composite/range/prefix, residual-filter, bitmap, scan,
  cancellation, spill, and resource-budget execution;
- indexed typed `UPDATE` and `DELETE` with old/new projection maintenance and
  retained tombstone semantics;
- included/expression/partial indexes, collations, operator classes, lifecycle
  and online rebuild/validation;
- statistics, cardinality/selectivity estimation, cost-based choices, general
  logical/physical plans, and complete `EXPLAIN`;
- randomized model equivalence, property-generated ordering, SQLLogicTest,
  isolation histories, TPC-H, TPC-C, and mutation-testing evidence; and
- the complete G7 measurement matrix.

No G0, G1, G2, or G7 gate closes from this milestone alone.
