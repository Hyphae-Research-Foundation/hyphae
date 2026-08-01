# Native typed SQL-row evidence

Date: 2026-08-01

Status: catalog-bound point-DML prerequisite; G0, G1, and G2 remain open

Source commit:
`7e5cd6c53e21792190eb7d60e95936845c2e7789`

Source tree:
`ef8d6d85a2b4098e7a758cd5cde3d81e0a7b6f9b`

Branch: `main`

## Change

The native SQL slice no longer treats every new relation as an opaque
two-binary-column table. It now owns:

- catalog-typed `CREATE TABLE` with stable ordered column IDs;
- primitive logical types, nullability, inline primary keys, and ordered
  table-level composite primary keys;
- named-column parameterized `INSERT`;
- canonical memcomparable primary-key encoding;
- catalog-ordered `HYTUPL01` row tuples without repeated type tags;
- named or `*` projections;
- primary-key predicates whose textual order can differ from catalog key
  order;
- catalog-version-bound prepared point `SELECT`;
- typed result materialization after strict commit and reopen; and
- stable `HYSQL001` through `HYSQL010` failure identities for the implemented
  parser, binder, value, tuple, and catalog failures.

`SqlValue` is the shared canonical `ScalarValue`, so SQL rows use the same
checked primitive value contract as future native secondary indexes.

## Tuple format

`hyphae-native-records` owns an exact self-delimiting `HYTUPL01` codec:

1. eight-byte magic;
2. checked little-endian total length;
3. checked `u16` catalog column count;
4. two reserved zero bytes;
5. canonical null bitmap;
6. `column_count + 1` absolute checked `u32` offsets; and
7. catalog-typed canonical scalar bytes.

The borrowed view validates the complete tuple before exposing a field. It
rejects wrong magic, reserved bits, length mismatch, zero columns, invalid or
nonmonotonic offsets, null fields with bytes, unused null bits, and trailing
bytes. The checked-in four-column golden has BLAKE3
`302a6901cd99efcb3a122805f6c52909864839e317247cfadaf717db76df5fef`.

## Binding and failure behavior

Insertion resolves each supplied name to one stable catalog column, rejects
duplicates and unknown columns, applies SQL null through the tuple bitmap, and
validates every non-null value against its logical type before touching
relational state. Missing or explicit null values fail for `NOT NULL`
columns.

Primary-key predicates must cover every key column exactly once. The binder
records parameter-to-column identity, then encodes components in the catalog's
primary-key order. Null, missing, duplicated, unsupported, or out-of-domain
key components fail before lookup or mutation.

Prepared plans retain catalog version, stable table ID, projection indices,
parameter-column indices, output names, and physical compatibility mode. A
catalog-version mismatch requires rebind.

## Compatibility

The historical relation shape
`(primary_key BINARY PRIMARY KEY, row BINARY)` remains explicit:

- both columns retain their non-null catalog definition;
- inserts preserve the raw primary-key and row bytes;
- prepared `SELECT row ...` retains the allocation-free borrowed lookup;
- existing binary `UPDATE` and `DELETE` keep their copy-on-write version
  behavior; and
- legacy `HYRELBT1`, `HYRELBT2`, `HYCAT001`, and `HYCAT002` recovery tests
  remain green.

Typed tuples use the current raw-row payload slot inside the existing
two-field MVCC record. This avoids an implicit disk-format conversion. A
future direct catalog-column row record requires an explicit version and
migration fixtures.

## Verification

The source commit contains:

- 9 `hyphae-native-records` tests, including three tuple codec tests;
- 46 `hyphae-native-runtime` tests;
- typed DDL/parser fixtures;
- strict type and nullability rejection before mutation;
- typed commit, prepared lookup, reopen, and result equivalence;
- composite-key lookup with reversed predicate order;
- incomplete composite-key rejection; and
- the existing cross-engine, checkpoint, crash-boundary, corruption, legacy,
  optimistic-writer, structure, and search coverage.

Windows, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
git diff --check
python tools/check_documentation.py
```

Debian GNU/Linux 13 under WSL2 passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

## Remaining boundary

This is a typed primary-key vertical, not a complete SQL engine. Still
required for G2:

- a scalable catalog B+tree and definition history;
- scans, general expressions, three-valued predicates, literals, casts, and
  functions;
- typed `UPDATE`, `DELETE`, defaults, checks, unique and foreign-key
  constraints;
- native secondary-index definitions, maintenance, range access, and
  uniqueness enforcement;
- logical and physical planning, cardinality/cost statistics, and `EXPLAIN`;
- joins, grouping, aggregates, CTEs, subqueries, set operations, sorting,
  windows, limits, and spill;
- schema evolution, transactions/savepoints at the SQL grammar surface, and
  plan cache invalidation;
- canonical JSON, array, map, and vector value codecs;
- SQLLogicTest, metamorphic, isolation, TPC-H, and TPC-C evidence; and
- direct catalog-column physical rows through an explicit versioned migration.

No G0, G1, or G2 gate closes from this evidence alone.
