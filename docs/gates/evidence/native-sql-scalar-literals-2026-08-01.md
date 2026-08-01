# Native SQL scalar-literal evidence

Date: 2026-08-01

Status: first catalog-bound scalar literals in `SELECT` filters; G0, G1, G2,
and G7 remain open

Source commit:
`7e6e5e399601679fd6c754e907f694cdbffad3ec`

Source tree:
`16829d312259bdd2032f33e39578412e5a97b110`

Branch: `codex/sql-scalar-literals`

## Change

The source commit extends the native residual-filter grammar with:

- SQL `NULL`;
- `TRUE` and `FALSE`;
- positive and negative base-10 integer literals; and
- single-quoted text with doubled-quote escaping.

The parser retains parameter positions when literals and `?` operands are
mixed. The binder resolves every literal against the referenced catalog
column. Boolean and text require the same logical family. Integers parse into
the column's signed or unsigned family and pass the existing ordered-component
codec, including declared-width and domain validation. Incompatible,
out-of-range, and negative-to-unsigned bindings return `HYSQL006` before
storage traversal.

SQL `NULL` is admissible for every comparison and evaluates to `UNKNOWN`.
`WHERE` therefore retains no row for `column = NULL`. Exact null key
comparisons return an empty result without attempting to encode or traverse an
invalid physical key.

This slice is limited to `SELECT` filters. Mutation value lists remain
parameter-only. Decimal, floating, binary, temporal, UUID and JSON literals,
casts, arithmetic, functions and column-to-column expressions remain pending.

## Planning and physical execution

Catalog-bound literals use the same `BoundScalarOperand` abstraction as
parameters. Top-level conjunction extraction therefore retains:

- complete primary-key equality;
- complete secondary-index equality; and
- complete-primary-key lower and upper range bounds.

`EXPLAIN` tests prove a literal primary-key lower bound produces
`PrimaryKeyRangeScan(...,residual=true)` and a text index literal plus boolean
residual produces `SecondaryIndexLookup(...,residual=true)`. No external SQL
engine, database, cache, search service, network transport or JSON execution
path participates.

The same literal range result is verified through retained materialized
snapshot and current-root physical execution. Reopen coverage verifies that a
negative integer range bound and boolean literal execute against recovered
catalog and row state. Escaped text, exact secondary lookup and null exact
lookup have direct result assertions.

The change is read-only. It adds no WAL opcode, root publication, mutation or
new crash boundary. Existing deterministic interruption and recovery tests ran
in the complete workspace suite.

## Red and green evidence

The first end-to-end literal test failed with `HYSQL001` before the grammar and
binder change. After implementation:

- the runtime crate has 74 passing tests;
- parser coverage fixes literal AST shape, escaped quotes, negative integers,
  mixed parameter positions, unterminated text and invalid negative tokens;
- runtime coverage proves range, secondary-index, escaped text and SQL-null
  results;
- incompatible boolean/integer and text/integer bindings fail with
  `HYSQL006`;
- an integer above signed 64-bit range fails during preparation; and
- materialized snapshot, direct physical execution and reopen return the
  expected rows.

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Deterministic test success is not represented
as a mutation score.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
```

Debian GNU/Linux under WSL2, Rust/Cargo 1.96.0, passed:

```text
cargo test -p hyphae-native-runtime --all-features --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 tools/check_integration_boundaries.py
python3 tools/check_crate_packages.py
python3 tools/check_documentation.py --binary target/debug/hyphae
python3 tools/run_documentation_examples.py --binary target/debug/hyphae
```

The integration-boundary audit returned `integration-boundaries-ok`. The crate
package audit covered 10 packages and 23 compile-time assets. Documentation
validation covered 150 pre-existing Markdown files and 12 JSON examples; the
new evidence document was added after that run and is validated before its
evidence commit.

## Remaining boundary

Still required for a complete relational engine:

- remaining literal families, casts, arithmetic, scalar and aggregate
  functions, `CASE`, and column-to-column expressions;
- partial primary-key prefixes, secondary ranges, descending traversal,
  offset/keyset planning, bitmap/multi-range access, and cost-based selection;
- joins, aggregation, sorting, spill, window and set operators;
- schema evolution, checks, foreign keys, generated columns, views, triggers,
  multi-row/range mutation, and literal mutation values;
- a stateful zero-copy operator cursor and bounded request arena;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, fuzzing and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation measurement matrix.

No G0, G1, G2 or G7 gate closes from this milestone alone.
