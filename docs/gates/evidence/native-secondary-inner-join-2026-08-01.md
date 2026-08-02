# Native secondary-index inner-join evidence

Date: 2026-08-01

Status: bounded non-unique secondary-index left input; G0, G1, G2, and G7
remain open

Source commit:
`0e53b3453b4b911b8e64c6d142f4859ad86520bd`

Source tree:
`033300cec85ac767f71c953897e729a3467b8c3b`

Branch at measurement: `codex/native-sql-secondary-join` (pull request 27)

## Change

This source extends native indexed nested-loop execution with a bounded
secondary-index left input:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<single-column-primary-key>
WHERE <secondary-index-equality> [AND <left-filter> ...]
[ORDER BY <complete-left-primary-key>]
LIMIT <nonnegative-integer>
```

An equality bound to a declared non-unique secondary index now supplies left
rows in canonical primary-key order. `LIMIT` is mandatory for a non-unique
secondary input and counts joined output rows, not raw index entries. A left
row rejected by the residual filter, a null join value, or a missing right row
does not consume the limit. Exact primary-key and unique-secondary joins keep
their existing no-limit forms; an explicitly limited unique-secondary input
uses the same bounded path.

The right side remains one exact lookup through a complete single-column
primary key. This change adds no external parser, SQL engine, database, cache,
service, TCP, HTTP, or serialization transport.

## Physical and snapshot semantics

The current-root executor captures one physical snapshot, seeks the secondary
B+tree prefix, validates each live projection, resolves its row-version chain
from that same root and CSN, performs the right lookup against the same
snapshot, and stops as soon as the requested output cardinality exists. It
does not first materialize every matching index entry.

Retained snapshots and private transactions use their canonical ordered
secondary projection sets. The transaction route sees uncommitted index, left
row, and right row changes. Strict reopen reconstructs the same catalog,
secondary projection, rows, and result ordering.

Corrupt index metadata, markers, row versions, stored rows, or catalog
identities propagate typed failures. A non-unique join without `LIMIT`, an
order other than the complete ascending left primary key, a parameter type
mismatch, or an unsupported access shape fails before returning rows.
`LIMIT 0` and a null equality operand return an empty bound result.

## Red and green evidence

The contract test was written first and failed with `HYSQL001` because the
binder rejected `ORDER BY` and `LIMIT` on secondary-index equality.
After implementation:

- two early index entries with a null join key and a missing right row do not
  consume `LIMIT 2`; users 1 and 4 are returned;
- transaction, retained snapshot, physical current-root, and strict-reopened
  routes return equivalent ordered results;
- a private transaction joins its uncommitted user and profile through its
  uncommitted secondary projection;
- bounded plain `SELECT` through the same secondary access is proven across
  all executors;
- exact `EXPLAIN`, missing limit, invalid order, `LIMIT 0`, and null equality
  operands have explicit assertions; and
- every existing exact and primary-scan join test remains green.

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
  0e53b3453b4b911b8e64c6d142f4859ad86520bd \
  "<disclosed WSL2 environment>"
```

Hosted Linux, macOS, Windows, fuzzing, security, soak, conformance, and
packaging lanes remain the pull-request merge authority.

## Latency observation

The [machine-readable WSL2
receipt](native-secondary-inner-join-wsl2.json) was produced from the clean
source commit in release mode. It uses 2,048 rows per relation, 16 equal-size
secondary cohorts, exact right primary keys, 64-byte payloads, strict commit
and reopen, 10,000 warmups, and 100,000 complete calls per route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical exact 1x1 | 31.847 us | 47.935 us | 89.717 us | 189.851 us | 1,041.367 us | 28,682 ops/s |
| Materialized exact 1x1 | 0.594 us | 0.788 us | 1.075 us | 5.387 us | 431.386 us | 1,501,550 ops/s |
| Physical primary scan `LIMIT 10` | 45.802 us | 66.688 us | 128.662 us | 263.325 us | 1,325.901 us | 19,874 ops/s |
| Materialized primary scan `LIMIT 10` | 3.016 us | 3.560 us | 4.951 us | 51.624 us | 409.495 us | 305,679 ops/s |
| Physical secondary `LIMIT 10` | 33.410 us | 46.682 us | 92.060 us | 199.661 us | 1,191.322 us | 27,659 ops/s |
| Materialized secondary `LIMIT 10` | 3.576 us | 4.307 us | 6.210 us | 51.298 us | 155.446 us | 259,126 ops/s |

The physical secondary route is a microsecond path through p99.9 in this
observation and is materially faster than the bounded full/range traversal at
the measured cardinality. Isolated physical maxima still cross one
millisecond, so the receipt does not support an all-observations microsecond
claim. It is also warm-state, concurrency-one evidence from one machine.
Cold pages, concurrent readers, saturation, write interference, allocation
and RSS, hardware counters, cancellation, wider secondary cardinalities, and
transport remain unmeasured. This is not G7.

## Remaining boundary

This is not a complete SQL join engine. Hyphae still lacks aliases and general
column expressions, composite/right-secondary join access, secondary ranges,
statistics and cardinality estimation, cost-based join ordering, hash and
merge joins, outer/semi/anti joins, memory budgets and spill, SQLLogicTest,
metamorphic join equivalence, TPC-H correctness, isolation histories, and
TPC-C ACID evidence.

G0, G1, G2, and G7 remain open.
