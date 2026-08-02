# Native composite inner-join evidence

Date: 2026-08-01

Status: exact composite primary and unique-secondary right lookup; G0, G1,
G2, and G7 remain open

Measured source commit:
`677d7d9c570910da2f51b8d0f695eccecf881645`

Measured source tree:
`3ba602ac9a81b5ad136df3ca46a5f25d583ed01e`

Branch at measurement: `codex/native-sql-composite-join` (pull request 29)

## Change

The native indexed nested-loop plan now accepts one or more exact
column-to-column equalities joined by `AND`:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<column>
 AND <left-table>.<column> = <right-table>.<column>
<bounded-or-exact-left-access>
```

The parser admits only this conjunction form. The binder requires the right
columns to cover exactly one complete primary key or one complete `UNIQUE`
secondary index. It then reorders the corresponding left columns into the
right key's catalog order before type validation and execution. Textual `ON`
order therefore does not define physical key order.

The lookup encodes the ordered composite key directly from the decoded left
row and right catalog types. It does not clone the joined scalar values or
materialize a parameter vector. Hyphae's own catalog, secondary projection,
B+tree, MVCC row chain, planner, and executor perform the operation without an
external SQL engine, database, cache, service, or transport.

## Snapshot and failure semantics

The snapshot, current physical, private transaction, bounded-primary-left,
and bounded-secondary-left executors all use the same bound composite access.
A null in any left key component or an absent right key produces no
inner-join row.

A private transaction sees right composite-key rewrites and new composite
keys before commit. A retained snapshot continues to resolve the old key and
row. Strict reopen resolves the committed key and row.

The binder fails closed with:

- `NoAccessPath` for a partial composite key;
- `NoAccessPath` for a complete non-unique right index;
- `DuplicateColumn` when either join side repeats a column;
- `TypeMismatch` when an aligned key component has a different logical type;
  and
- the existing typed corruption errors when stored index or row state is
  invalid.

## Red and green evidence

The first composite-primary-key test was written before implementation. It
failed with `InvalidSyntax` at the second `AND` in the `ON` clause. After the
change:

- a composite primary-key right lookup matches in retained, current physical,
  and reopened execution;
- a composite unique-secondary right lookup matches even when the `ON`
  equalities are written in reverse catalog order;
- null and missing key components return no row;
- private index rewrites and inserts are visible before commit;
- retained history and strict reopen return their respective old and new
  values;
- `EXPLAIN` identifies the unique-secondary right access; and
- partial, duplicated, type-incompatible, and non-unique candidates are
  rejected explicitly.

The complete runtime suite increased from 98 to 101 tests.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0:

```text
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py
git diff --check
```

WSL2 Ubuntu 24.04, Linux 6.18.33.1, Rust/Cargo 1.96.0:

```text
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo run --release -p hyphae-native-runtime \
  --example indexed_join_smoke -- \
  677d7d9c570910da2f51b8d0f695eccecf881645 \
  "<disclosed WSL2 environment>"
```

Hosted Linux stable and MSRV, macOS, optional integrations, client
conformance, quality, and soak lanes had passed for the measured source
commit when this evidence was written. The remaining hosted fuzzing,
dependency, Windows, packaging, and release-readiness lanes were still
pending. The final pull-request checks remain merge authority.

## Latency observation

The [machine-readable WSL2 receipt](native-composite-inner-join-wsl2.json)
uses 2,048 rows per relation, 64-byte payloads, strict commit and reopen,
10,000 warmups, and 100,000 complete calls per route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical one-column unique right | 48.491 us | 71.986 us | 135.423 us | 281.320 us | 4,944.667 us | 18,870 ops/s |
| Physical two-column unique right | 46.083 us | 70.474 us | 134.663 us | 258.717 us | 1,166.611 us | 19,557 ops/s |
| Materialized one-column unique right | 0.819 us | 1.146 us | 1.670 us | 17.344 us | 180.768 us | 1,074,359 ops/s |
| Materialized two-column unique right | 0.867 us | 1.133 us | 1.698 us | 14.443 us | 374.570 us | 1,029,115 ops/s |

The two-column physical route remained a microsecond path through p99.9 and
met the provisional 75 us p50 and 400 us p99 observations. Its maximum
crossed one millisecond. The one-column and two-column measurements are close
enough that this single run does not establish a performance improvement or
regression.

This observation is warm-state, concurrency one, one machine, exact one-row
output, and a two-column key. Cold pages, concurrent readers, saturation,
write interference, allocation and RSS, hardware counters, cancellation,
wide composite keys, and transport remain unmeasured. This is not G7.

## Remaining boundary

Hyphae still lacks non-unique and ranged right access, aliases and general
column expressions, statistics and cardinality estimation, cost-based join
ordering, hash and merge joins, outer/semi/anti joins, memory budgets and
spill, SQLLogicTest, metamorphic join equivalence, TPC-H correctness,
isolation histories, and TPC-C ACID evidence.

G0, G1, G2, and G7 remain open.
