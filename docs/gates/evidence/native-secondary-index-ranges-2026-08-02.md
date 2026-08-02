# Native secondary-index ordered-range evidence

Date: 2026-08-02

Status: first ordered physical range scan over a secondary index; G0, G2,
and G7 remain open

Source commit:
`b73e99e906888e64cc923923d6da4454129bfd43`

Source tree:
`c168d9793845916d6823fbb26926c626babda02b`

Source branch: `codex/sql-secondary-range`

## Change

The source commit establishes the first ordered physical range scan over
a secondary index without adding an external engine or materializing the
complete relation:

- new secondary indexes use `HYRIDX02` metadata and the order-preserving
  entry identity `index_key || primary_key || u32be(primary_key_length)`;
  the index key orders first, and the canonical primary-key bytes are a
  deterministic tie-breaker;
- `HYRIDX01` and its historical
  `u32be(index_key_length) || index_key || primary_key` identity remain
  readable, writable, recoverable, and valid for exact lookup, and are
  never advertised as range-capable because variable-width values order
  by length before value;
- the planner admits a physical secondary range only when the persisted
  physical metadata confirms the ordered layout; catalog intent alone is
  insufficient, and a legacy layout falls back to a bounded primary-key
  scan when that fallback is legal, without being labeled a physical
  secondary range;
- current-root execution walks the physical B+tree directly, while
  transaction-private and retained snapshots preserve equivalent
  semantics;
- inclusive, exclusive, one-sided, empty, and inverted bounds and SQL
  `NULL` have explicit behavior; single-column and complete composite
  secondary keys are supported, and a partial composite key is not;
- residual predicates evaluate before `LIMIT`;
- malformed ordered identities and forged row projections fail closed; and
- one physical index cannot mix layouts.

No WAL opcode, page format, row format, or catalog format changes: the
milestone is read-only with respect to substrate formats and adds the
`HYRIDX02` metadata magic. It does not implement equality-prefix plus
next-column ranges for composite secondary indexes, descending scans,
streaming cursors, offsets, multi-range/bitmap access, statistics,
cardinality estimation, or cost-based planning.

## Bound semantics

For ordered namespace prefix `N` and complete encoded secondary key `K`,
the operator uses:

| SQL endpoint | Physical endpoint |
|---|---|
| no lower bound | `Included(N)` |
| `key >= K` | `Included(N || K)` |
| `key > K` | `Included(successor(N || K))` |
| no upper bound | `Excluded(successor(N))` |
| `key < K` | `Excluded(N || K)` |
| `key <= K` | `Excluded(successor(N || K))` |

The successor endpoints deliberately include or exclude every primary-key
tie under `K`. An empty or inverted intersection returns no rows. `NULL`
in either complete endpoint makes that comparison `UNKNOWN` and returns
no rows after complete arity and type validation.

A second lower or upper endpoint for the same selected index fails
binding with the new `HYSQL014` error code, "native SQL secondary-index
range binding is invalid". `LIMIT` is mandatory, and `ORDER BY`, when
requested, is the complete ascending secondary key.

## Executable matrix

The native runtime suite in `crates/hyphae-native-runtime/src/lib.rs`
covers:

- `complete_secondary_index_range_plans_ordered_physical_bounds` for the
  planner identity and the physical bounds;
- `ordered_secondary_identity_preserves_variable_key_then_primary_order`
  for physical order under variable key widths;
- `ordered_secondary_range_matches_private_snapshot_latest_and_reopen`
  for private, retained, latest, and reopen equivalence;
- `ordered_secondary_range_enforces_boundaries_and_binding_failures` for
  inclusive, exclusive, one-sided, empty, and `NULL` bounds plus
  wrong-arity, wrong-type, duplicate-bound, and skipped-key rejections;
- `composite_secondary_range_requires_the_complete_ordered_key` for the
  partial composite-key rejection;
- `legacy_secondary_layout_keeps_exact_lookup_and_rejects_range_planning`
  for `HYRIDX01` exact lookup and reopen without false range planning;
- `physical_secondary_range_rejects_a_malformed_ordered_identity` and
  `physical_secondary_range_rejects_a_forged_row_projection` for
  fail-closed behavior; and
- `latest_sql_uses_exact_physical_secondary_index_without_materialized_state`.

All pre-existing transaction, commit, recovery, checkpoint, retention,
structure, lexical, and ANN tests remain in the native runtime suite. The
read-only operator adds no new crash-injection boundary.

## Red and green evidence

Contract commit `274c0dd` (Specify ordered secondary index ranges)
preceded executable work. Test commit `55d0051` added the red planner
test and the range-planning matrix. Implementation commit `af55a97` made
it green.

Benchmark commit `60f1a5d` added the schema-v15 paths. Commit `a007874`
replaced the corpus with variable-width ordered keys: ten keys `a`, `aa`,
and so on up to ten repetitions of `a`, with the remainder under `z-...`,
so `[a,b)` selects exactly ten rows. Commit `c2bd2c1` gave the two
secondary-range paths an independent 1,000-call warmup.

The native runtime suite at the receipt commit contains 187 passing tests
(WSL2, `cargo test -p hyphae-native-runtime --locked`).

## Mechanical validation

The executed local validation for this milestone is:

- WSL2 `cargo test -p hyphae-native-runtime --locked`, with 187 passing
  tests at the exact receipt source commit; and
- the release benchmark, which ran to completion from a clean worktree
  and whose JSON receipt validated with `python3 -m json.tool`.

Hosted multi-OS, dependency, security, fuzz, and stress checks remain PR
evidence at the implementation anchor `c2bd2c1` rather than local
evidence. Mutation testing was not executed: the repository has no
accepted mutation tool, operator set, or surviving-mutant threshold for
this milestone.

The environment bounds what this run proves: WSL2 is virtualized, the
benchmark data directory lives in the temp directory (tmpfs), and the run
uses warm state, memory durability, and concurrency one. It exercises no
named-pipe/UDS transport, no proofs, and no fsync, and it observes no
cold state, saturation, interference, allocation/RSS, or hardware
counters.

## Latency observation

The [machine-readable schema-v15
receipt](native-microsecond-smoke-secondary-range-wsl2.json) was produced
from the clean source commit under Debian 13/WSL2, Rust 1.96.0, release
profile, warm state, memory durability, concurrency one, a height-two
relational B+tree, 2,048 secondary-index rows, `LIMIT 10`, an `[a,b)`
range over variable-width text keys, and 1,000 warmups plus 10,000
observations per new path, one operation per observation.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical prepared SQL ordered secondary range | 24.328 us | 33.291 us | 58.527 us | 227.101 us | 37,844 ops/s |
| Unindexed text-range PK-scan baseline | 7,971.720 us | 9,231.635 us | 10,082.542 us | 11,879.722 us | 123 ops/s |

Both paths return the same ten rows, validated in-process in the same
run; the benchmark fails if the results differ. Within that one process
and corpus, the indexed path reduces p50 by 99.695% and p99 by 99.420%,
the p50 and p99 ratios are 327.677x and 172.272x, and throughput is
307.675 times higher. The unindexed baseline is deliberately expensive:
it examines the complete 2,048-row relation to return 10 rows. The
differential identifies the removed algorithmic work; it is not a
regression threshold.

Each inherited v14 path was reviewed against the schema-v14 receipt, and
none shows a material change. The largest absolute p50 increase is
0.004 us (materialized `HGET`), the scan-family p50s improved, and some
sub-microsecond and scan p99s moved within ordinary WSL2 run-to-run
variance (largest: primary-key prefix p99 40.672 us to 44.137 us), while
the primary-key range plus residual p99 fell from 69.169 us to 48.787 us.

The indexed path is within the provisional phase-1 target for indexed SQL
returning at most 100 rows (p50 50 us, p99 250 us) for this single
scenario under the disclosed warm/bounded conditions. It is not G7: the
run is virtualized, warm, and concurrency one, and it lacks cold pages,
other key widths/cardinalities, tombstone-heavy ranges, allocations/RSS,
saturation, interference, hardware counters, named-pipe/UDS transport,
fsync, proofs, cancellation, and long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- equality-prefix plus next-column ranges for composite secondary
  indexes;
- descending traversal, streaming/keyset cursors and offsets, and
  multi-range/bitmap access;
- statistics, cardinality estimation, and cost-based planning;
- general expressions, constraints, schema evolution, broader joins,
  grouping, aggregation, sorting, spill, CTEs, windows, and set
  operators;
- zero-copy operator cursors and bounded request arenas;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation matrix.

No G0, G2, or G7 gate closes from this milestone alone.
