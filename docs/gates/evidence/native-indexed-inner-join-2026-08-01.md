# Native indexed inner-join evidence

Date: 2026-08-01

Status: first exact indexed `INNER JOIN`; G0, G1, G2, and G7 remain open

Source commit:
`cbb58aa787b7e89eafd85e45bed74ae313ccc297`

Source tree:
`01b100ad0e689e1d5182b745144e21e89f723adb`

Branch at measurement: `codex/native-sql-inner-join` (pull request 25)

## Change

The source commit adds Hyphae's first native SQL join without delegating
parsing, planning, storage, or execution to another database:

- the lexer and parser admit one explicit qualified `INNER JOIN` form;
- the binder requires an exact left primary-key lookup or exact unique
  secondary-index lookup;
- the right join column must be the complete single-column primary key;
- join-column logical types must match exactly;
- output columns are explicit, qualified, and catalog-bound;
- `EXPLAIN` identifies both physical access paths; and
- transaction-private, retained-snapshot, current physical, and reopened
  execution use the same prepared plan contract.

The admitted form is:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<single-column-primary-key>
WHERE <exact-left-key-filter>
```

The left filter may include residual predicates, but it must expose one
complete primary key or one complete unique secondary-index key. The exact
left access therefore reaches at most one row. The executor decodes that row,
applies SQL three-valued filtering, binds its non-null join value to the right
primary-key codec, and performs the second lookup against the same captured
root set. A null join key or absent right row returns no row.

Aliases, `SELECT *`, non-unique left access, right non-primary access,
composite right keys, multi-row nested loops, hash/merge joins, outer joins,
ordering, limits, subqueries, and cross-engine sources remain outside this
slice.

## Snapshot and failure semantics

The current physical executor captures one immutable root set and visible CSN
before either lookup. It resolves the unique secondary entry and both relation
rows from that root. A retained `NativeSnapshot` stays on its historical
materialized state, while a private SQL batch sees its own uncommitted right
row update. Reopen reconstructs the same catalog, index, and row state before
the prepared plan executes.

Binding fails before traversal for an unqualified projection or join column,
an incompatible join type, a right column that is not the complete primary
key, a non-unique left index, or incorrect parameter arity. A unique index
that physically resolves to more than one primary key fails closed as a
malformed stored row. The join is read-only and introduces no WAL opcode or
durable format.

## Red and green evidence

The initial integration test compiled the grammar and binder changes, then
failed with `HYSQL001` because no join executor existed. After implementation:

- the native runtime library has 90 passing tests;
- parser fixtures prove the exact admitted grammar and reject `SELECT *`,
  missing `WHERE`, and non-inner syntax;
- binder/executor fixtures prove primary and unique-secondary left access;
- `EXPLAIN`, parameter arity, non-unique access, ambiguous projection,
  non-primary right access, and type mismatch have explicit assertions;
- null left keys, missing left rows, and missing right rows return no row;
- one private right-row update is visible before commit;
- a retained snapshot continues to return the historical right row; and
- strict reopen returns the newly committed joined row.

The existing relational recovery, secondary-index corruption, version-chain,
and seven-boundary indexed-mutation matrices remain green. No write or
publication path changed in this milestone.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tools/check_documentation.py
git diff --check
```

WSL2 Ubuntu 24.04, Linux 6.18.33.1, Rust/Cargo 1.96.0:

```text
cargo run --release -p hyphae-native-runtime \
  --example indexed_join_smoke -- \
  cbb58aa787b7e89eafd85e45bed74ae313ccc297 \
  "<disclosed WSL2 environment>"
```

The full Windows workspace, documentation, JSON parse, and diff checks
completed successfully on the working tree containing the exact source plus
these evidence files. Hosted Linux, macOS, Windows, security, soak,
conformance, and packaging lanes remain the pull-request merge authority.

## Latency observation

The [machine-readable WSL2
receipt](native-indexed-inner-join-wsl2.json) came from the clean source
commit in release mode. The fixture has 2,048 users, 2,048 profiles, a unique
left email index, 64-byte payloads, strict commit and reopen, 10,000 warmups,
and 100,000 complete prepared executions per route.

| Route | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical current-root join | 31.177 us | 45.363 us | 84.364 us | 180.409 us | 29,465 ops/s |
| Materialized retained-snapshot join | 0.575 us | 0.852 us | 1.328 us | 2.951 us | 1,526,764 ops/s |

Both routes are below the provisional 75-us p50 and 400-us p99 targets for
this bounded shape. This is an observation, not G7: it measures concurrency
one, warm state, one row per side, one machine, and one row width. It does not
cover cold pages, many-to-many cardinality, concurrent readers, saturation,
background work, allocation/RSS, hardware counters, transport, spill,
cancellation, or tail behavior under writes.

## Remaining boundary

This milestone does not constitute a complete SQL join engine. G2 still
requires general multi-row join inputs, composite and secondary right access,
statistics and cardinality estimation, cost-based join ordering, nested-loop,
hash and merge implementations, outer/semi/anti joins, spill and resource
budgets, aliases and expression binding, plus SQLLogicTest, metamorphic
equivalence, TPC-H correctness, isolation histories, and TPC-C ACID evidence.

G0, G1, G2, and G7 remain open.
