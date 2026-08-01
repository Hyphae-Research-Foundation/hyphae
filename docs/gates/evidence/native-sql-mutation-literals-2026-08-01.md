# Native SQL mutation-literal evidence

Date: 2026-08-01

Status: first catalog-bound scalar literals in exact-key DML; G0, G1 and G2
remain open

Source commit:
`61dc550a9ebcda3fd9e3df9f763c463fdc273898`

Source tree:
`088ff0fdce35673f8ca9eb828369e8ba645c265e`

Branch: `codex/sql-mutation-literals`

## Change

The source commit extends the native mutation grammar to admit the first scalar
operand family in:

```text
INSERT INTO <table> (<column> [, ...])
VALUES (<scalar> [, ...])

UPDATE <table>
SET <non-primary-key-column> = <scalar> [, ...]
WHERE <complete-primary-key-column> = <scalar> [AND ...]

DELETE FROM <table>
WHERE <complete-primary-key-column> = <scalar> [AND ...]
```

`<scalar>` is `?`, SQL `NULL`, `TRUE`, `FALSE`, a positive or negative
base-10 integer, or single-quoted text with doubled-quote escaping. Literals
and parameters can be mixed. Parameter positions follow statement text order,
including update assignments before primary-key predicates.

The parser emits one column/operand binding. Before reading or mutating a row,
the binder resolves the catalog column, converts a literal into its declared
logical family, validates parameters through the storage codec, and enforces
parameter arity. The existing insert/update/delete binders then enforce
duplicate columns, nullability, complete primary-key shape, primary-key
immutability, and ordered-key encoding.

Decimal, floating, binary, temporal, UUID and JSON literal spellings remain
pending. Legacy binary DML remains parameterized. Defaults, generated values,
casts, arithmetic, functions, subqueries, `RETURNING`, multi-row values and
multi-row/range mutation remain outside this slice.

## Atomic mutation and recovery

Literal DML uses the existing native mutation path. An update still derives
old and new secondary-index projections and publishes the tuple, index
tombstone and new index entry under one transaction and CSN. A delete still
publishes row and secondary-index tombstones atomically. No external SQL
engine, database, cache, search service, network transport or JSON execution
path participates.

The end-to-end fixture:

- inserts one all-literal row and one mixed parameter/literal row;
- builds a unique secondary index over both rows;
- rejects a multi-assignment update whose second value has the wrong type and
  proves the first assignment did not persist;
- rejects wrong parameter arity, nullability violation and signed overflow;
- updates indexed text, nullable text and boolean columns with literals;
- deletes the second row through a literal primary key;
- verifies the old index projection is absent and the new projection is live;
  and
- verifies the indexed literal query and index projection after reopen.

The change adds no WAL opcode or commit boundary. Existing indexed-mutation
crash tests, optimistic uniqueness tests, interruption matrices and complete
workspace recovery suite ran unchanged.

## Red and green evidence

The new end-to-end test first failed with `HYSQL001` because the mutation
parser accepted only `?`. After implementation:

- the runtime crate has 75 passing tests;
- parser coverage fixes assignment/predicate parameter order and mixed literal
  AST shape;
- every pre-existing parameter-only insert, update and delete test remains
  green;
- incompatible logical families and signed overflow return `HYSQL006`;
- missing parameters return `HYSQL002`;
- a null non-null column returns `HYSQL005`; and
- statement failure leaves private row and index state unchanged.

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
and executable examples are validated after this receipt is added.

## Remaining boundary

Still required for a complete relational engine:

- remaining literal families, casts, arithmetic, scalar and aggregate
  functions, `CASE`, defaults, generated values and `RETURNING`;
- multi-row values, range/multi-row update and delete, `MERGE`, and
  secondary-access mutation;
- joins, aggregation, sorting, spill, window and set operators;
- schema evolution, checks, foreign keys, views and triggers;
- partial primary-key prefixes, secondary ranges, descending traversal,
  offset/keyset planning, bitmap/multi-range access, and cost-based selection;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, fuzzing and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation measurement matrix.

No G0, G1 or G2 gate closes from this milestone alone.
