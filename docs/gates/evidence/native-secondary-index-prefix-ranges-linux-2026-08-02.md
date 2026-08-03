# Native composite secondary-index prefix-range evidence

Date: 2026-08-02

Status: first equality-prefix plus immediately-following-column range over an
ordered native secondary index; G0, G2, and G7 remain open

Source commit:
`f2a22eb97f499f367de08031b1dfa61cbc90ce60`

Source tree:
`152eb2fc4f790545b8425459114851c5c98204fe`

Source branch: `codex/sql-secondary-prefix-range`

## Change

This source adds one planner/execution slice to the native relational engine:

- a nonempty strict equality prefix of a persisted ordered `HYRIDX02`
  secondary index can be followed by lower, upper, or two-sided bounds on the
  immediately following index column;
- predicate text order is independent of catalog order, while skipped or
  duplicate equality columns do not extend the prefix;
- complete secondary equality remains exact lookup and complete secondary
  ranges retain their prior plan and explain identity;
- the binder encodes the equality prefix once, appends the range component,
  and maps inclusive, exclusive, one-sided, empty, inverted, and `NULL`
  endpoints to canonical logical bounds;
- current-root execution reuses the bounded physical `HYRIDX02` B+tree
  visitor, while transaction-private and retained snapshots use the same
  logical interval and return equivalent rows;
- residual predicates run before the output limit, and equal complete
  secondary keys retain canonical primary-key tie order;
- `HYRIDX01` never advertises this access and falls back to a legal
  primary-key scan;
- malformed ordered identities and forged row projections fail before any
  partial result is returned; and
- no dependency, sidecar, TCP, HTTP, JSON compatibility protocol, WAL opcode,
  page format, row format, catalog format, or unsafe Rust was added.

`EXPLAIN` reports:

```text
SecondaryIndexPrefixRangeScan(
  table=<id>,
  index=<id>,
  prefix_columns=<count>,
  range_column=<id>,
  lower=<kind>,
  upper=<kind>,
  limit=<n>
)
```

The actual output is one line without whitespace and appends
`,residual=true` only when unconsumed predicates remain.

## Bound semantics

For an encoded equality prefix `P` and encoded range component `R`, the
logical interval is:

| SQL endpoint | Logical endpoint |
|---|---|
| no lower bound | `Included(P)` |
| `range >= R` | `Included(P || R)` |
| `range > R` | `Included(successor(P || R))` |
| no upper bound | `Excluded(successor(P))` |
| `range < R` | `Excluded(P || R)` |
| `range <= R` | `Excluded(successor(P || R))` |

The successor endpoints include or exclude every trailing-index suffix and
primary-key tie under the bounded range component. The physical visitor adds
the index namespace and validates every decoded ordered identity and stored
row projection.

## Executable matrix

The native runtime suite covers:

- `composite_secondary_prefix_range_matches_private_snapshot_latest_and_reopen`
  for exact explain output, inclusive/exclusive and one-sided endpoints,
  inverted intervals, adjacent `a`/`aa` prefix isolation, primary-key tie
  order, residual-before-limit, private/retained/current/reopen equivalence,
  arity, type, `NULL`, duplicate-bound, invalid-order, and `LIMIT 0`
  behavior;
- `secondary_prefix_range_does_not_skip_index_columns` for a three-column
  index and duplicate equality predicates without false prefix planning;
- `secondary_prefix_range_selects_a_valid_overlapping_index` for continuing
  past an earlier range-compatible index whose order is invalid and selecting
  the later compound candidate;
- `legacy_composite_secondary_layout_rejects_prefix_range_planning` for
  `HYRIDX01` exact lookup, primary-scan fallback, and reopen;
- `physical_secondary_prefix_range_rejects_a_malformed_ordered_identity` and
  `physical_secondary_prefix_range_rejects_a_forged_row_projection` for
  fail-closed current-root traversal;
- `composite_secondary_range_requires_the_complete_ordered_key` for the
  inherited complete-range plan; and
- `legacy_secondary_layout_keeps_exact_lookup_and_rejects_range_planning` for
  the inherited single-column legacy behavior.

The exact all-target runtime execution passed 212 library tests plus three
process-boundary helper tests. The read-only operator introduces no new commit
or crash-injection boundary.

## Red, green, and discarded attempts

Contract commit `f2afbaa` preceded executable changes. The first exact test
then failed with `InvalidPrimaryKey` (`0 passed; 1 failed; 206 filtered out`),
which demonstrated that the binder had no legal composite secondary
prefix-range access. Implementation/test commit `1d03f7b` made the expanded
matrix green.

Benchmark commit `22d90ea` added schema-v16. Its first run failed before
measurement with `HYSQL014`: both `INDEX(email)` and
`INDEX(tenant,email)` matched the new physical query, and catalog-order
selection reached the simple index whose order contract did not match.
Commit `107c6be` separated the compound range column, but its completed
receipt was discarded: adding the column and index to the inherited corpus
slowed inherited routes by 10% to 58%, so it could not support a
non-regression claim.

Commit `a2b1dbb` moved the new corpus into a second isolated
`NativeDatabase`, restoring the inherited schema-v15 seed byte for byte.
That run was stopped before completion after review found that the Clippy
refactor had also changed inherited measurement order. Commit `319e3bc`
restores the schema-v15 order and measures the isolated new route last.
Implementation commit `0f3eb1f` then fixed the overlapping-index planner bug
exposed by the original failed corpus. Its first admitted clean receipt
measured 51.170 us p50, missing the provisional 50 us target by 1.170 us.
Performance commit `f2a22eb` removed a duplicate complete-row decode, resolved
secondary-index columns once per execution, validated the stored index key
without rebuilding it, and skipped predicate reevaluation only when the
planner proved that no residual remained. The same correctness matrix stayed
green. Only the optimized receipt from `f2a22eb` is admitted below.

## Mechanical validation

Executed directly on the canonical Linux host:

```text
mario@10.77.10.10
/home/mario/celiumsai/hyphae
Ubuntu 24.04.4 LTS
EC2 m6i.2xlarge, 8 vCPU, 30 GiB RAM
/dev/root ext4 on EBS
rustc 1.96.0 (ac68faa20 2026-05-25)
```

Commands and results at the implementation/benchmark lineage:

- `cargo test --workspace --all-targets`: passed; the explicit optimized
  Gate-9 benchmark test remained intentionally ignored;
- `cargo clippy --workspace --all-targets -- -D warnings`: passed;
- `cargo test -p hyphae-native-runtime --all-targets`: 212 library tests and
  three process-boundary helper tests passed;
- `cargo clippy -p hyphae-native-runtime --all-targets -- -D warnings`:
  passed without a new lint suppression;
- `cargo clippy -p hyphae-native-runtime --example microsecond_smoke --
  -D warnings`: passed;
- `cargo check --release -p hyphae-native-runtime --example
  microsecond_smoke`: passed;
- `python3 tools/check_documentation.py`: 201 Markdown files and 12 JSON
  examples passed; and
- `git diff --check`: passed.

Mutation testing was not executed. This repository has no accepted mutation
tool, operator set, or surviving-mutant threshold for this milestone. Hosted
multi-OS, dependency, security, fuzz, and stress checks remain PR evidence,
not local evidence.

## Linux latency observation

The machine-readable schema-v16 receipt
`native-microsecond-smoke-secondary-prefix-range-linux.json` was produced
from the clean source commit above. Its SHA-256 is
`28e5543eed88c1f5aeed5fcf2f089b2558d88107da049d881fb535d961055de1`.
Both new paths use an isolated height-2 relational B+tree with 2,048 rows, warm state,
memory durability, concurrency one, `LIMIT 10`, 1,000 warmups, and 10,000
single-call observations. The benchmark fails if the indexed and unindexed
paths do not return the same ten rows.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical composite secondary prefix range | 46.535 us | 92.188 us | 98.370 us | 116.189 us | 19,114 ops/s |
| Equivalent unindexed PK scan | 12,197.704 us | 12,976.263 us | 14,248.056 us | 16,622.108 us | 81 ops/s |

Within this process and corpus, the native index reduces p50 by
99.618% and p99 by 99.310%; the corresponding ratios are 262.119x and
144.841x. This differential identifies removed
algorithmic work. It is not a universal latency promise.

The inherited schema-v15 corpus and measurement order are unchanged. Against
`native-microsecond-smoke-lineage-ext4-linux.json`, no inherited route shows
a material median regression: the largest p50 increase is 0.315 us
(primary-key scan). The largest inherited p99 increase is 15.522 us
(primary-key range plus residual), whose observed p99 remains 53.713 us.
The optimized complete ordered secondary range improved from the prior
schema-v16 receipt by 9.497% at p50 and 43.344% at p99; the composite prefix
range improved by 9.058% and 7.641%.
Run-to-run variance, the second database's setup cost outside timed regions,
and shared-host noise still prevent treating one receipt as a release gate.

The indexed observation meets both halves of the provisional phase-1 bounded
indexed-SQL target: p50 at or below 50 us and p99 at or below 250 us. This is
an operator observation, not G2 or G7: it excludes cold pages, fsync, proofs,
named-pipe or UDS transport, allocation/RSS, saturation, interference, other
key widths/cardinalities, tombstone-heavy intervals, cancellation, and
long-running cursors.

## Remaining boundary

Still required for a complete relational engine:

- descending traversal, streaming/keyset cursors, offsets, and
  multi-range/bitmap access;
- cost-based selection across multiple order-compatible overlapping indexes,
  statistics, and cardinality estimation;
- general expressions, broader constraints and schema evolution, grouping,
  aggregation, sorting/spill, broader joins, CTEs, windows, and set
  operators;
- zero-copy operator cursors and bounded request arenas;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation matrix.

No G0, G2, or G7 gate closes from this milestone alone.
