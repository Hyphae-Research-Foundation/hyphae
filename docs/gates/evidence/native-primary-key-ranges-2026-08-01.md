# Native primary-key range evidence

Date: 2026-08-01

Status: first bound-aware physical primary-key range and bounded prepared SQL
range; G0, G1, G2, and G7 remain open

Source commit:
`3d42f0b6ce8733654e31710fa9b8a33325d0c80e`

Source tree:
`0ee18533aa33b3f3692cca5ffcfad264c6e2938c`

Branch: `main`

## Change

The source commit adds one native ordered range path without reconstructing
the complete relation:

- `BTree::visit_prefix_range_cached` intersects a namespace prefix with
  independent full-key `Included`, `Excluded`, or `Unbounded` endpoints,
  prunes separator-disjoint child intervals, preserves canonical order, and
  propagates early stop;
- the existing exclusive cursor visitor now delegates to that range contract;
- `scan_latest_relational_range(table, lower, upper, limit)` maps canonical
  primary-key bounds into the table row namespace, validates the table marker,
  resolves HYRELBT1/HYRELBT2 visibility and blobs, skips tombstones, and stops
  after the requested live-row count;
- native SQL binds one lower and/or one upper comparison over the complete
  primary key, including composite SQL row comparison; and
- transaction-private, retained materialized, and current-root physical
  execution share `PrimaryKeyRangeScan`.

The admitted SQL forms use `>`, `>=`, `<`, or `<=`. A one-column key uses
`id >= ?`; a composite key uses `(tenant, sequence) >= (?, ?)`. Every bound
must contain the complete primary key in catalog order. `LIMIT` is mandatory.
An optional `ORDER BY` must name that complete ascending key. Equality point
predicates are not mixed with this range slice.

`EXPLAIN` reports
`PrimaryKeyRangeScan(table=<id>,lower=<kind>,upper=<kind>,limit=<n>)`, where a
kind is `inclusive`, `exclusive`, or `unbounded`.

No external database, query engine, cache, search service, TCP, HTTP, or JSON
path participates.

## Snapshot and failure semantics

The prepared current executor captures one immutable root set and uses one
relational root and visible CSN for the complete range. It rejects catalog
version drift before binding parameters. Retained snapshots continue to read
their original materialized relation; transaction execution includes private
uncommitted writes.

Range parameters are type checked through the canonical memcomparable
primary-key codec. SQL null, wrong types, wrong arity, partial or reordered
composite keys, duplicate lower/upper bounds, absent `LIMIT`, and invalid
`ORDER BY` fail before row traversal. Inverted and equal-open ranges return
zero rows. Materialized executors detect those intervals before invoking
`BTreeMap::range`, preventing its invalid-range panic; physical execution
still validates the table marker before returning empty.

A malformed marker, reached B+tree node, key order, row, version chain, or blob
fails closed. The milestone is read-only: it adds no WAL opcode and publishes
no root.

## Red and green evidence

The first B+tree acceptance test failed to compile because
`visit_prefix_range_cached` did not exist. The initial end-to-end SQL test then
failed with `HYSQL001` because row range comparisons were outside the parser.
After implementation:

- the native B+tree suite contains 11 passing tests;
- the native runtime suite contains 69 passing tests;
- a four-namespace, 2,048-key multilevel tree covers inclusive, exclusive,
  half-open, inverted and empty bounds plus early stop;
- the public physical scan covers tombstones, mixed bounds, unknown relation,
  cursor compatibility, and reopen;
- a typed 512-row composite-key relation proves transaction-private,
  materialized-snapshot, and current-root physical equivalence; and
- SQL coverage includes one-sided ranges, `EXPLAIN`, inverted/equal-open
  safety, `NULL`, parameter arity, PK order, missing `LIMIT`, and reopen.

All pre-existing deterministic commit and checkpoint interruption tests ran in
the workspace suites. No new write/crash boundary was required for this
read-only operator.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tools/check_documentation.py
git diff --check
```

Debian GNU/Linux 13 under WSL2, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Green deterministic tests are not represented
as a mutation score.

## Latency observation

The [machine-readable schema-v9
receipt](native-microsecond-smoke-primary-range-wsl2.json) was produced from
the clean source commit under Debian 13/WSL2, release profile, warm state,
memory durability, concurrency one, a height-two relational B+tree, 2,048
typed rows, primary-key interval `[1024, 1034)`, `LIMIT 10`, and 100,000
complete calls per range operation.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical PK range to 10 owned rows | 9.362 us | 13.113 us | 24.271 us | 108.562 us | 96,323 ops/s |
| Physical prepared SQL PK range and projection | 10.408 us | 14.086 us | 26.164 us | 98.951 us | 87,987 ops/s |

The nearby full-scan paths in the same receipt measured p50 8.922 us / p99
22.981 us direct and p50 9.762 us / p99 26.937 us through prepared SQL. These
figures are context, not a cross-run regression claim.

This is a narrow observation, not a G7 pass or universal latency promise. It
does not cover cold pages, other bounds, other row widths, tombstone-heavy
ranges, allocation/RSS, concurrency, saturation, background interference,
hardware counters, dedicated hardware, named-pipe/UDS transport, fsync,
proofs, spill, cancellation, or long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- scalar/boolean expressions, casts, three-valued residual filters, and
  non-key predicates;
- partial primary-key prefixes, secondary-index ranges, descending traversal,
  offset/keyset planning, and multi-range/bitmap access;
- a stateful zero-copy/operator cursor and bounded request arena;
- joins, aggregation, sorting, spill, window and set operators;
- statistics, cardinality/selectivity estimation and cost-based planning;
- schema evolution, checks, foreign keys, generated columns, views, triggers,
  and full transactional SQL semantics;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation measurement matrix.

No G0, G1, G2, or G7 gate closes from this milestone alone.
