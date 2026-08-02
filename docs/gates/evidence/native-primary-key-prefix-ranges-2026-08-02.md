# Native primary-key prefix-plus-range evidence

Date: 2026-08-02

Status: first bounded physical range over the primary-key component
immediately following an equality prefix; G0, G2, and G7 remain open

Source commit:
`7cc7cfcf6ede0574f10cff5c5aeb2c316fad614f`

Source tree:
`d742824994496d70c1de6e3be79e9c1d6f8527dc`

Source branch: `codex/sql-primary-prefix-range`

## Change

The source commit replaces one measured residual path with a native physical
operator without adding an external engine or materializing the complete
relation:

- the binder combines the longest nonempty strict primary-key equality prefix
  with at most one lower and one upper comparison on the immediately following
  primary-key column;
- `PrimaryKeyPrefixRangeScan` records the prefix width, stable range-column ID,
  endpoint kinds, output limit, and remaining-residual status;
- parameter values use the existing canonical memcomparable component codec;
- transaction-private and retained execution use the same ordered-map
  interval, while current-root execution visits the buffered relational
  B+tree under one captured root set and visible CSN; and
- `LIMIT` counts only rows whose complete filter evaluates to `TRUE`.

No WAL opcode, page format, row format, catalog format, or mutation boundary
changes in this read-only milestone.

## Bound semantics

For encoded equality prefix `P` and encoded next component `C`, the operator
uses:

| SQL endpoint | Physical endpoint |
|---|---|
| no lower bound | `Included(P)` |
| `column >= value` | `Included(P || C)` |
| `column > value` | `Included(successor(P || C))` |
| no upper bound | `Excluded(successor(P))` when one exists |
| `column < value` | `Excluded(P || C)` |
| `column <= value` | `Excluded(successor(P || C))` |

The successor rules deliberately include or exclude every remaining
primary-key suffix under the ranged component. Empty or inverted intersections
return no rows. `NULL` in the prefix or either endpoint makes the comparison
`UNKNOWN` and returns no rows after complete parameter/type validation.

Duplicate lower or upper endpoints fail binding. A range on a skipped or
later primary-key column stays residual and cannot be reported as this access
path.

## Executable matrix

The native runtime suite covers:

- lower-only, upper-only, closed/open, inverted, and equal-open ranges;
- parameter order independent from catalog primary-key order;
- a three-column primary key proving that exclusive lower and inclusive upper
  bounds handle every remaining suffix correctly;
- residual predicates evaluated before the output limit;
- private insertion followed by rollback;
- retained state before delete/insert, current-root state afterwards, and
  identical results after reopen;
- null, wrong-type, wrong-arity, duplicate-bound, and skipped-column cases;
  and
- fail-closed current-root execution after physical row corruption.

All pre-existing transaction, commit, recovery, checkpoint, retention,
structure, lexical, and ANN tests remain in the native runtime suite. The
read-only operator adds no crash injection boundary.

## Red and green evidence

Contract commit `065dbe7` preceded executable work. Test commit `433843c`
then failed under Debian/WSL2 with:

```text
left:  PrimaryKeyPrefixScan(table=1,columns=1,limit=3,residual=true)
right: PrimaryKeyPrefixRangeScan(table=1,prefix_columns=1,range_column=2,lower=inclusive,upper=exclusive,limit=3)
```

Implementation commit `dd8c822` made the same test green. Boundary commit
`6653393` expanded the matrix. The runtime suite at the benchmark source
contains 179 passing tests.

The first Windows execution attempt was not a semantic red or green run:
Windows Application Control blocked the newly linked test executable with OS
error 4551. Compilation and checks can run on Windows, while executable test
evidence for this source uses WSL2 and hosted CI.

## Mechanical validation

The benchmark source and this evidence closure passed:

- Windows `cargo fmt --all -- --check`;
- Windows `cargo check -p hyphae-native-runtime --locked`;
- WSL2 `cargo test --workspace --all-features --locked`, including all 179
  native runtime tests;
- WSL2 `cargo clippy --workspace --all-targets --all-features --locked --
  -D warnings`;
- WSL2 `cargo fmt --all -- --check`;
- WSL2 `python3 tools/check_documentation.py`, covering 185 Markdown files and
  12 JSON examples; and
- WSL2 `git diff --check`.

The Windows Python selected by the session was blocked by Application Control,
so the successful WSL2 documentation run is the executable evidence for that
check. Mutation testing was not executed: the repository has no accepted
mutation tool, operator set, or surviving-mutant threshold for this milestone.
Hosted multi-OS, dependency, security, fuzz, and stress checks remain PR
evidence rather than local evidence.

## Latency observation

The [machine-readable schema-v14
receipt](native-microsecond-smoke-primary-prefix-range-wsl2.json) was produced
from the clean source commit under Debian 13/WSL2, Rust 1.96.0, release
profile, warm state, memory durability, concurrency one, a height-two
relational B+tree, 2,048 composite-key rows split between adjacent text
tenants `a` and `aa`, `LIMIT 10`, and 100,000 complete calls per scan
operation.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical prepared SQL strict prefix | 16.425 us | 21.556 us | 40.672 us | 150.373 us | 55,944 ops/s |
| Physical prefix plus `id >= 512` | 15.119 us | 20.963 us | 40.127 us | 147.976 us | 60,804 ops/s |

The prior schema-v13 source executed the second query as a residual after
examining 512 rows. It observed p50/p99 `198.603/589.950 us` and throughput
`4,472 ops/s`. Across those exact source-bound observations, the physical
operator reduces p50 by 92.387% and p99 by 93.198%, while throughput is
13.597 times higher. This comparison identifies the removed algorithmic work;
it is not a same-process regression threshold.

The new operator is within the provisional bounded indexed-SQL target for this
single scenario. It is not G7: the run is virtualized, warm, concurrency one,
and lacks cold pages, other key widths/cardinalities, tombstone-heavy ranges,
allocations/RSS, saturation, interference, hardware counters, named-pipe/UDS
transport, fsync, proofs, cancellation, and long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- an order-preserving secondary-index physical identity and general secondary
  ranges;
- descending traversal, offsets/keyset planning, multi-range/bitmap access,
  statistics, cardinality estimation, and cost-based planning;
- general expressions, constraints, schema evolution, broader joins,
  grouping, aggregation, sorting, spill, CTEs, windows, and set operators;
- zero-copy operator cursors and bounded request arenas;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation matrix.

No G0, G2, or G7 gate closes from this milestone alone.
