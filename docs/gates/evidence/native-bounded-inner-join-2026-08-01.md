# Native bounded inner-join evidence

Date: 2026-08-01

Status: bounded multirow primary-key nested lookup; G0, G1, G2, and G7 remain
open

Source commit:
`c8f1c05210f3b60aa356d0e2366c51d3d2d4996b`

Source tree:
`d78c869e89b91089ca26f1d251d9d3bed0e46227`

Branch at measurement: `codex/native-sql-bounded-join` (pull request 26)

## Change

This source extends the first exact indexed join with two bounded multirow
left inputs:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<single-column-primary-key>
[WHERE <left-filter>]
[ORDER BY <complete-left-primary-key>]
LIMIT <nonnegative-integer>
```

The binder admits either a complete left primary-key range or a bounded full
primary-key scan. The right side remains one exact single-column primary-key
lookup per admitted left row. The physical executor walks the left B+tree in
canonical primary-key order, applies the left filter, performs the right
lookup against the same root set, and stops only after `LIMIT` joined output
rows exist. A null left join value or missing right row does not consume the
output limit.

`ORDER BY`, when present, must be the complete ascending left primary key.
`LIMIT` is mandatory for scan/range joins and `LIMIT 0` returns no rows after
binding relation identity and types. Exact one-row primary/unique-secondary
joins retain their no-limit form.

This is a native indexed nested-loop slice. No external parser, SQL engine,
database, cache, service, TCP, HTTP, or serialization transport participates.

## Snapshot and failure semantics

The transaction executor iterates its private relational map and can join
uncommitted left and right inserts. A retained `NativeSnapshot` iterates its
historical materialized relation state. The current physical executor captures
one `Snapshot` before traversal and uses its relational root and visible CSN
for the entire left scan and every right lookup.

The physical visitor prunes to the bound primary-key range, decodes only
reached visible rows, skips tombstones, and propagates page, row, blob, and
version-chain corruption. A malformed bound, wrong parameter type, missing
limit, or non-primary ordering fails before traversal. Read execution does not
append WAL or publish a root.

## Red and green evidence

The contract test was written first. It failed with `HYSQL001` because the
initial join grammar returned immediately after `WHERE` and admitted no range
or limit. Windows App Control then blocked the newly built test executable
with `os error 4551`; the same source was executed under WSL2 to distinguish
that host policy from a code failure.

After implementation:

- the native runtime library has 92 passing tests under WSL2;
- a range beginning at user 1 returns users 1 and 4 for `LIMIT 2`, proving
  that a null join key and a missing right row do not consume the limit;
- retained materialized, current physical, and strict-reopened execution are
  equivalent;
- a private transaction joins an uncommitted user and profile;
- a no-predicate bounded scan returns the same first two valid joins;
- `LIMIT 0`, missing limit, invalid order, and exact `EXPLAIN` have explicit
  assertions; and
- all earlier exact primary/unique-secondary join tests remain green.

The change is read-only. Existing WAL, indexed-mutation crash matrices,
recovery, and corruption tests cover the unchanged durable substrate.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0:

```text
cargo fmt --all -- --check
cargo check -p hyphae-native-runtime
cargo clippy -p hyphae-native-runtime --all-targets -- -D warnings
python tools/check_documentation.py
git diff --check
```

WSL2 Ubuntu 24.04, Linux 6.18.33.1, Rust/Cargo 1.96.0:

```text
cargo test -p hyphae-native-runtime --lib
cargo run --release -p hyphae-native-runtime \
  --example indexed_join_smoke -- \
  c8f1c05210f3b60aa356d0e2366c51d3d2d4996b \
  "<disclosed WSL2 environment>"
```

Hosted Linux, macOS, Windows, fuzzing, security, soak, conformance, and
packaging lanes remain the pull-request merge authority.

## Latency observation

The [machine-readable WSL2
receipt](native-bounded-inner-join-wsl2.json) was produced from the clean
source commit in release mode. It uses 2,048 rows per relation, a height-two
left relational tree, exact right primary keys, 64-byte payloads, strict
commit and reopen, 10,000 warmups, and 100,000 complete calls per route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical exact 1x1 | 30.901 us | 48.922 us | 104.434 us | 231.807 us | 1,099.910 us | 28,819 ops/s |
| Materialized exact 1x1 | 0.543 us | 0.766 us | 1.290 us | 8.722 us | 329.447 us | 1,581,697 ops/s |
| Physical bounded `LIMIT 10` | 45.669 us | 70.963 us | 154.697 us | 304.271 us | 2,788.082 us | 19,472 ops/s |
| Materialized bounded `LIMIT 10` | 2.766 us | 3.275 us | 4.060 us | 46.776 us | 836.985 us | 331,725 ops/s |

Both physical routes remain in microseconds at p99.9, but isolated maxima
cross one millisecond. The receipt therefore does not support an
all-observations microsecond claim. It also measures warm state, concurrency
one, one machine, exact right keys, and ten output rows. Cold pages, missing
right rows at scale, concurrent readers, saturation, write interference,
allocation/RSS, hardware counters, transport, cancellation, and wider
cardinalities remain unmeasured. This is not G7.

## Remaining boundary

The executor still lacks aliases and general column expressions, non-unique
secondary left inputs, composite/right-secondary access, join reordering,
statistics and cardinality estimation, hash and merge joins, outer/semi/anti
joins, memory budgets and spill, SQLLogicTest, metamorphic join equivalence,
TPC-H correctness, isolation histories, and TPC-C ACID evidence.

G0, G1, G2, and G7 remain open.
