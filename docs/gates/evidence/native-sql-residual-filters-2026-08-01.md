# Native SQL residual-filter evidence

Date: 2026-08-01

Status: first typed parameterized residual filters and executable SQL
three-valued logic; G0, G1, G2, and G7 remain open

Source commit:
`28adf987bc785b18d87aab9fc2d04681e39e1310`

Source tree:
`8dc81fb84ac06b5892eab5ff0d4aab246bea9566`

Branch: `codex/sql-residual-filters`

## Change

The source commit adds one bounded native expression/filter slice:

- scalar parameter comparisons support `=`, `<>`, `!=`, `<`, `<=`, `>`, and
  `>=`;
- `IS NULL`, `IS NOT NULL`, `NOT`, `AND`, `OR`, and parentheses use the
  specified SQL precedence;
- `TRUE`, `FALSE`, and `UNKNOWN` have explicit truth tables, comparisons with
  SQL null produce `UNKNOWN`, and `WHERE` retains only `TRUE`;
- the binder validates every referenced column, parameter position, arity, and
  non-null logical type before storage traversal;
- top-level conjunctions can extract a complete primary-key equality, complete
  secondary-index equality, or complete-primary-key lower/upper range while
  retaining the remaining expression as a residual filter; and
- a query without one of those access paths uses a bounded primary-key scan.

`LIMIT` is mandatory for scan and range plans and counts matching rows rather
than examined rows. Exact primary/secondary lookup retains its bounded key
shape without `LIMIT`. Complete composite-primary-key row comparisons remain
range-only; literals, casts, arithmetic, functions, and column-to-column
comparisons are not part of this slice.

`EXPLAIN` preserves the existing access-path spelling and adds
`,residual=true` when a filter term remains outside the extracted key/range.

No external SQL engine, database, cache, search service, network transport, or
JSON execution path participates.

## Physical execution and snapshots

The current-root executor uses a fallible relational B+tree visitor. It decodes
visible row versions under one captured root/CSN, evaluates the bound filter,
materializes only projected matches, and propagates early stop after the
post-filter limit. It does not build a complete relation or
`MaterializedState`.

Transaction-private execution includes uncommitted rows. Retained
`NativeSnapshot` execution reads its historical materialized state. The same
prepared predicate and result were verified through transaction-private,
retained materialized, current-root physical, and reopened execution.

The milestone is read-only. It adds no WAL opcode, root publication, or new
crash boundary. Existing deterministic commit/checkpoint interruption and
indexed-mutation crash tests ran in the complete workspace suite.

## Failure semantics

Wrong parameter arity returns `HYSQL002`; missing columns return `HYSQL004`;
non-null type mismatch returns `HYSQL006`. SQL null is admitted as a comparison
parameter and evaluates to `UNKNOWN`. A null exact primary-key comparison
returns zero rows rather than trying to encode a physical key.

Duplicate exact-key equality terms, invalid/incomplete primary-key shapes,
duplicate range endpoints, missing scan/range `LIMIT`, invalid scan order, and
unsupported composite-row expression shapes fail during parse/bind. Malformed
catalog objects, tuples, physical keys, pages, row-version chains, and blobs
remain fail-closed through the existing typed errors.

## Red and green evidence

The initial end-to-end acceptance test failed with `HYSQL001` because the
parser did not admit `IS NULL`, `OR`, or non-key predicates. After
implementation:

- the runtime crate has 72 passing tests;
- parser coverage fixes parameter positions and proves `NOT > AND > OR`
  precedence;
- exhaustive boolean tests cover all nine `AND` and `OR` pairs plus `NOT`;
- the end-to-end fixture proves `LIMIT` is applied after filtering by placing a
  non-match before the second requested match;
- exact primary-key plus residual, exact secondary-index plus residual,
  primary-key range plus residual, and full residual scan all execute;
- SQL null equality, `IS NULL`, `IS NOT NULL`, wrong type, wrong arity, missing
  column, transaction-private rollback, retained snapshot, physical execution,
  and reopen are covered; and
- the complete Debian/WSL workspace suite passed.

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Green deterministic tests are not represented as
a mutation score.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo check -p hyphae-native-runtime --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
```

The Windows runtime executable was blocked by the machine's Application
Control policy with `os error 4551`; the policy was not weakened. Executable
validation used Debian GNU/Linux 13 under WSL2:

```text
cargo test -p hyphae-native-runtime --all-features --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 tools/check_integration_boundaries.py
python3 tools/check_crate_packages.py
python3 tools/check_documentation.py --binary target/debug/hyphae
```

## Latency observation

Benchmark commit:
`82880276d0c1e0470ae54200c3776cf31c6ee5d2`

Benchmark tree:
`ab70dc940ae3fff6491a4c4e2eb95c698e31fae7`

The [machine-readable schema-v10
receipt](native-microsecond-smoke-residual-filter-wsl2.json) is a release-mode,
warm-state, memory-durability, concurrency-one WSL2 observation. It uses a
height-two relational B+tree with 2,048 typed rows. The residual query starts
at primary key 1,024, accepts alternating boolean rows, evaluates about 19 rows
to return `LIMIT 10`, and records 100,000 complete calls.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Prepared PK range, 10 rows | 12.657 us | 16.864 us | 32.505 us | 118.805 us | 72,821 ops/s |
| Prepared PK range + residual boolean, 10 matches | 15.464 us | 21.237 us | 40.370 us | 137.250 us | 59,109 ops/s |

In this one matched run, residual evaluation and roughly nine additional
visited rows added 2.807 us at p50. This is an observation, not a regression
claim, G7 pass, or universal latency promise. It excludes cold pages,
alternative selectivity and row width, null-heavy predicates, complex boolean
trees, concurrency, saturation, allocation/RSS, hardware counters, dedicated
hardware, transport, fsync, and long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- literals, casts, arithmetic, scalar/aggregate functions, `CASE`, and
  column-to-column expressions;
- partial primary-key prefixes, secondary ranges, descending traversal,
  offset/keyset planning, bitmap/multi-range access, and cost-based selection;
- joins, aggregation, sorting, spill, window and set operators;
- schema evolution, checks, foreign keys, generated columns, views, triggers,
  and multi-row/range mutation;
- a stateful zero-copy operator cursor and bounded request arena;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, fuzzing, and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation measurement matrix.

No G0, G1, G2, or G7 gate closes from this milestone alone.
