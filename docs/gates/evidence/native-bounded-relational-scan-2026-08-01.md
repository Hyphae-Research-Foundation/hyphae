# Native bounded relational scan evidence

Date: 2026-08-01

Status: first resumable current-root primary-key scan and bounded prepared SQL
scan; G0, G1, G2, and G7 remain open

Source commit:
`53576eb2fa6341016e2666ee10ab17160dc39c78`

Source tree:
`7d72fdcf187a4d111265bf315fd89abf6dc58475`

Branch: `main`

## Change

The source commit adds a physical relational scan without reconstructing the
complete engine state:

- `BTree::visit_prefix_cached` walks a separator-pruned prefix in canonical
  key order, accepts an exclusive full-key resume cursor, and propagates early
  stop before the remaining range is read or materialized;
- `scan_latest_relational(table, start_after, limit)` validates the table
  marker, scans the table's `0x02` row namespace, resolves visible
  HYRELBT1/HYRELBT2 rows and blobs, skips tombstones, and returns at most
  `limit` owned rows;
- the last returned canonical primary key is a valid exclusive cursor for the
  next call;
- `prepare_sql_latest` and both existing materialized executors bind and run
  the same new `PrimaryKeyScan` plan; and
- `EXPLAIN` identifies the path as
  `PrimaryKeyScan(table=<id>,limit=<n>)`.

The admitted SQL shape is:

```text
SELECT <projection>
FROM <table>
[ORDER BY <complete-primary-key-in-catalog-order>]
LIMIT <nonnegative-integer>
```

`LIMIT` is mandatory. An explicit order must equal the complete primary key in
catalog order. This slice does not implement filters, descending order,
offsets, secondary ranges, joins, aggregation, or an unbounded scan.

No external database, query engine, cache, search service, TCP, HTTP, or JSON
path participates.

## Snapshot and failure semantics

Every current execution captures one immutable root set and uses its
relational root and visible CSN throughout the scan. A prepared current plan
rejects a catalog-version change before reading rows. A retained
`NativeSnapshot` executes its materialized plan against the original root and
catalog.

The physical path distinguishes an unknown relation from an empty relation. A
malformed table marker, reached B+tree node, key order, row, version chain, or
blob fails closed. `LIMIT 0` validates the relation identity but intentionally
does not read row pages. Read execution does not append WAL or publish a root.

## Red and green evidence

The initial B+tree acceptance test failed to compile because
`visit_prefix_cached` did not exist. The initial SQL parser test failed because
`Statement::Select` had no `order_by` or `limit` fields. After implementation:

- the B+tree suite contains 10 passing tests;
- the native runtime suite contains 66 passing tests;
- a 2,048-entry multilevel prefix fixture proves early stop, exclusive resume,
  exhaustion, and equality with the materializing prefix scan;
- a 512-row physical relation proves update visibility, tombstone skipping,
  pagination, zero limit, unknown relation, and strict reopen;
- HYRELBT1 reopen coverage proves the same scan API over the legacy inline
  format; and
- typed composite-PK SQL proves equal results in a private transaction, a
  retained materialized snapshot, and the current physical executor.

The SQL fixture also covers `EXPLAIN`, optional omitted `ORDER BY`, invalid
reversed PK order, parameter mismatch, `LIMIT 0`, catalog invalidation, update,
delete, and reopen.

All existing deterministic commit and checkpoint interruption tests ran in the
workspace suites. No new write or WAL opcode was introduced by this read-only
milestone.

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
tool or acceptance threshold. Test count, coverage, and green workspace lanes
are not represented as a substitute for mutation score.

## Latency observation

The [machine-readable schema-v8
receipt](native-microsecond-smoke-relational-scan-wsl2.json) was produced from
the clean source commit under Debian 13/WSL2, release profile, warm state,
memory durability, concurrency one, a height-two relational B+tree, 2,048
typed scan rows, `LIMIT 10`, and 100,000 complete calls per scan operation.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Physical relational scan to 10 owned rows | 10.963 us | 14.302 us | 26.608 us | 124.137 us | 83,678 ops/s |
| Physical prepared SQL scan and two-column projection | 10.741 us | 13.818 us | 26.115 us | 116.218 us | 85,540 ops/s |

The nearby exact routes in the same receipt measured:

- physical primary-key lookup p50 0.984 us and p99 2.819 us;
- physical exact unique secondary lookup p50 6.954 us and p99 20.700 us; and
- physical prepared exact secondary SQL p50 7.133 us and p99 20.686 us.

This is a narrow observation, not a G7 pass or a universal latency promise. It
does not cover cold pages, different limits or row widths, tombstone-heavy
datasets, allocation/RSS, concurrency, saturation, background interference,
hardware counters, dedicated hardware, named-pipe/UDS transport, fsync,
proofs, spill, cancellation, or long-running cursor retention.

## Remaining boundary

Still required for a complete relational engine:

- row predicates, expressions, casts and three-valued filter execution;
- primary and secondary half-open ranges, descending traversal and offset/keyset
  planning;
- a stateful zero-copy/operator cursor and bounded request arena;
- joins, aggregation, sorting, spill, window and set operators;
- statistics, cardinality/selectivity estimation and cost-based planning;
- schema evolution, checks, foreign keys, generated columns, views, triggers
  and full transactional SQL semantics;
- randomized model equivalence, SQLLogicTest, isolation histories, TPC-H,
  TPC-C and mutation testing; and
- the complete G7 warm/cold/concurrency/saturation measurement matrix.

No G0, G1, G2, or G7 gate closes from this milestone alone.
