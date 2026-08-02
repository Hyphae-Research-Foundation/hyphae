# Native primary-key left-prefix scan evidence

Date: 2026-08-02

Status: first bounded strict left-prefix operator for composite primary keys;
G0, G2, and G7 remain open

Source commit:
`62f7a34621a64856b97549b9dc08cbbbf6bd104c`

Source tree:
`41bc86540da982dfc979d3b168bb832b82d9f388`

Source branch: `codex/sql-primary-key-prefix-scan`

## Change

The source commit adds one native SQL access path without introducing an
external database or materializing the complete relation:

- the binder selects the longest nonempty strict left prefix of a composite
  primary key from top-level equality predicates;
- complete primary-key equality retains point-lookup semantics, while gapped,
  duplicate, nested, or non-leading predicates remain residual and cannot be
  mislabeled as prefix access;
- `PrimaryKeyPrefixScan(table=<id>,columns=<count>,limit=<n>)` makes the
  selected operator explicit in `EXPLAIN`;
- parameter values are encoded with the existing canonical memcomparable
  component codec and become the half-open interval
  `[prefix, binary-successor(prefix))`; and
- transaction-private and retained execution use the same ordered map
  interval while current-root execution uses the buffered relational B+tree
  visitor under one captured root set and visible CSN.

`LIMIT` is mandatory and applies after the complete filter evaluates to
`TRUE`. `ORDER BY`, when present, remains the complete ascending primary key.
A null prefix operand makes equality `UNKNOWN` and returns no rows. Wrong
types and parameter arity fail before storage traversal.

No WAL opcode, catalog format, page format, or write boundary changes in this
read-only milestone. Existing exact primary/secondary lookup and complete
primary-key range behavior remain intact.

## Boundary and failure semantics

The ordered component codec is self-delimiting. The test corpus deliberately
uses adjacent text keys `a` and `aa`; scanning `a` returns exactly its 128
rows and cannot cross into `aa`. The binary successor helper covers ordinary,
terminal-`0xff`, all-`0xff`, and empty inputs.

The physical visitor validates every reached HYRELBT1/HYRELBT2 row and version
chain. A forged malformed row pointer causes a typed runtime error rather than
a partial result. `LIMIT 0` still validates parameters. A duplicate leading
equality or a gapped key predicate falls back to an explicitly residual
bounded table scan; an incomplete prefix without `LIMIT` fails binding.

The executable matrix covers:

- parameters whose predicate order differs from catalog key order;
- a range-like residual evaluated before the output limit;
- private uncommitted insertion followed by rollback;
- one retained snapshot before a committed delete and insert;
- current-root results after that mutation and identical results after reopen;
- pure-prefix isolation between adjacent variable-length text components;
- null, wrong-type, wrong-arity, zero-limit, missing-limit, duplicate, gapped,
  and complete-key planning; and
- fail-closed current-root execution after physical row corruption.

All pre-existing deterministic commit, recovery, and checkpoint tests remain
green. No new crash injection point is required because this operator does
not mutate durable state.

## Red and green evidence

Contract commit `6b55c6a` preceded executable work. Test commit `db573e9`
then failed with:

```text
left:  PrimaryKeyScan(table=1,limit=3,residual=true)
right: PrimaryKeyPrefixScan(table=1,columns=1,limit=3,residual=true)
```

Implementation commit `51f0dc7` made that test green. Boundary/failure commit
`e5e2a2c` expanded the matrix. On the benchmark source commit, the native
runtime suite contains 175 passing tests and strict Clippy passes.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tools/check_documentation.py
git diff --check
```

Debian GNU/Linux 13 under WSL2, Rust/Cargo 1.96.0, passed the same format,
workspace test, strict Clippy, documentation, and diff checks.

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Green deterministic tests are not represented
as a mutation score.

## Latency observation

The [machine-readable schema-v13
receipt](native-microsecond-smoke-primary-prefix-wsl2.json) was produced from
the clean source commit under Debian 13/WSL2 with Rust 1.96.0, release profile,
warm state, memory durability, concurrency one, a height-two relational
B+tree, 2,048 composite-key rows across text tenants `a` and `aa`, `LIMIT 10`,
and 100,000 complete calls per scan operation.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical prepared SQL strict prefix | 14.557 us | 26.209 us | 78.233 us | 236.621 us | 56,496 ops/s |
| Prefix plus `id >= 512` residual | 198.603 us | 353.430 us | 589.950 us | 1,286.957 us | 4,472 ops/s |

The residual case is intentionally reported, not averaged into the pure
operator. The implemented access interval narrows to tenant `a`, but the next
key component is not yet a physical range bound, so execution examines 512
rows before producing ten matches. This is direct evidence for the next
prefix-plus-range planner/operator milestone.

The nearby prepared complete-key range measured p50 15.578 us and p99 64.837
us in the same receipt. The full prepared scan measured p50 26.297 us and p99
147.791 us. These same-run figures provide context only; the expanded corpus
and machine state prohibit cross-receipt regression claims.

This is an observation, not a G7 pass or universal latency promise. It does
not cover cold pages, other key widths or prefix cardinalities, tombstone-heavy
intervals, allocations/RSS, concurrency, saturation, background interference,
hardware counters, dedicated hardware, named-pipe/UDS transport, fsync,
proofs, spill, cancellation, or long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- equality-prefix plus a lower/upper range on the next primary-key component;
- a new order-preserving secondary-index physical identity before general
  secondary ranges over variable-width keys;
- descending traversal, offsets/keyset planning, multi-range/bitmap access,
  statistics, cardinality estimation, and cost-based planning;
- general expressions, constraints, schema evolution, joins, grouping,
  aggregation, sorting, spill, CTEs, windows, and set operators;
- zero-copy/operator cursors and bounded request arenas;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C, and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation matrix.

No G0, G2, or G7 gate closes from this milestone alone.
