# Native right secondary-index inner-join evidence

Date: 2026-08-01

Status: exact unique-secondary right lookup; G0, G1, G2, and G7 remain open

Source commit:
`7e390e67a63752b8c36f02062eef6fb2e1abad3c`

Source tree:
`81ee09783bb7d1a9bed8db6646f367dc01c29b94`

Branch at measurement: `codex/native-sql-right-secondary-join` (pull request
28)

## Change

The native indexed nested-loop plan now represents both sides as explicit
access paths. The right side accepts either its previous complete
single-column primary key or an exact single-column `UNIQUE` secondary index:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<primary-or-unique-column>
<bounded-or-exact-left-access>
```

The binder resolves relation identity, column identity, logical type, and
right access before execution. A non-unique right index, a composite unique
index that is only partially named by the equality, or a column without an
exact access path remains `NoAccessPath`.

This is Hyphae's own catalog, secondary projection, B+tree, MVCC row chain,
planner, and executor. No external parser, SQL engine, database, cache,
service, TCP, HTTP, or serialization transport participates.

## Snapshot and failure semantics

Each executor binds the non-null left join value into the selected right
access. A primary-key plan performs one row lookup. A unique-secondary plan
performs one exact secondary-prefix lookup, rejects more than one projected
primary key as invalid stored state, then decodes the selected row with its
real primary key.

The private executor observes uncommitted right-index inserts and key
rewrites. A retained snapshot keeps the earlier index projection and row. The
physical executor resolves the index and row from the same captured root set
and visible CSN. Strict reopen reconstructs the new unique key and joined row.
A null left join value or absent secondary key produces no inner-join row.

Index/table identity mismatches, malformed live markers, corrupt row versions,
and invalid stored rows propagate typed failures through the existing
physical lookup. Read execution appends no WAL and publishes no root.

## Red and green evidence

The contract test was written first. It failed with `NoAccessPath` because the
right side admitted only the complete primary key. After implementation:

- exact primary-key left access joins through a right unique secondary index
  in private, retained, current physical, and reopened execution;
- null, missing, and absent left/right identities return no inner-join row;
- a private transaction updates the right unique key and corresponding left
  value, inserts another right key and left row, and reads both before commit;
- the retained snapshot still returns the old key and value after commit;
- strict reopen returns the committed updated value;
- `EXPLAIN` reports
  `right_access=unique-secondary(index=<id>)`;
- a declared non-unique right index remains rejected; and
- all previous primary-right, exact, bounded-scan, and bounded-secondary-left
  join tests remain green.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0:

```text
cargo fmt --all -- --check
cargo check -p hyphae-native-runtime
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py
git diff --check
```

WSL2 Ubuntu 24.04, Linux 6.18.33.1, Rust/Cargo 1.96.0:

```text
cargo test --workspace --all-targets --all-features --locked
cargo run --release -p hyphae-native-runtime \
  --example indexed_join_smoke -- \
  7e390e67a63752b8c36f02062eef6fb2e1abad3c \
  "<disclosed WSL2 environment>"
```

Hosted Linux, macOS, Windows, fuzzing, security, soak, conformance, and
packaging lanes remain the pull-request merge authority.

## Latency observation

The [machine-readable WSL2
receipt](native-right-secondary-inner-join-wsl2.json) was produced from the
clean source commit in release mode. It uses 2,048 rows per relation, unique
left email and right code indexes, exact one-row output, 64-byte payloads,
strict commit and reopen, 10,000 warmups, and 100,000 complete calls per
route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical primary right | 39.879 us | 58.394 us | 111.006 us | 215.955 us | 1,008.903 us | 23,074 ops/s |
| Physical unique-secondary right | 43.508 us | 66.576 us | 121.144 us | 222.570 us | 1,595.788 us | 20,865 ops/s |
| Materialized primary right | 0.760 us | 0.956 us | 1.239 us | 7.791 us | 193.890 us | 1,189,670 ops/s |
| Materialized unique-secondary right | 0.743 us | 0.919 us | 1.187 us | 3.215 us | 156.800 us | 1,252,599 ops/s |

In the same corpus and process, the physical unique-secondary right lookup
adds 3.629 us at p50 and 10.138 us at p99 relative to the primary-right route.
Both remain microsecond paths through p99.9. The unique-secondary maximum
crosses one millisecond, so this does not support an all-observations
microsecond claim.

The observation is warm-state, concurrency one, one machine, one-row output,
and a unique one-column key. Cold pages, concurrent readers, saturation,
write interference, allocation and RSS, hardware counters, cancellation,
composite keys, and transport remain unmeasured. This is not G7.

## Remaining boundary

Hyphae still lacks composite join equalities and composite right keys,
non-unique/ranged right access, aliases and general column expressions,
statistics and cardinality estimation, cost-based join ordering, hash and
merge joins, outer/semi/anti joins, memory budgets and spill, SQLLogicTest,
metamorphic join equivalence, TPC-H correctness, isolation histories, and
TPC-C ACID evidence.

G0, G1, G2, and G7 remain open.
