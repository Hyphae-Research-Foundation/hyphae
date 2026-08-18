<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native SQL vector execution v1

Status: experimental P5 foundation; no P5 or G7 closure claim

This contract records the first batch-oriented native SQL execution slice. It
does not replace the scalar executor and it does not claim SIMD, aggregation,
sorting, adaptive joins, spill, or a completed SQL performance gate.

## Selection policy

Prepared current-root primary-key, prefix, prefix-range, and primary-key range
scans retain the direct visitor when their declared result limit is at most
256 rows. Above that threshold, the executor requests bounded
`ForegroundBounded` CPU, I/O, and memory resources once, then consumes physical
rows in batches of at most 1,024 candidates.

The threshold is a conservative implementation boundary, not a frozen
hardware-independent optimum. Calibration and qualification evidence must
precede any claim that it is the preferred crossover on a particular host.

## Batch operator

For each candidate batch the executor:

1. decodes every visible physical tuple exactly once against the prepared
   catalog relation;
2. evaluates the complete SQL three-valued residual expression into a compact
   64-byte-aligned native selection bitmap;
3. projects only selected rows;
4. preserves canonical primary-key order; and
5. advances with the last physical primary key as an exclusive cursor.

The bitmap is process-local and contains no transport or compatibility
representation. It is intentionally shaped so later lexical and vector
operators can consume the same native candidate-mask concept, but that fusion
is not implemented by this slice.

## Parallel source

When a validated resource governor and its persistent execution pool are
installed, the outer SQL request reserves a bounded worker count derived from
the declared result limit and governor policy. Immutable B+tree leaf segments
subdivide that parent permit; no executor creates a private thread pool and no
nested top-level admission occurs. Segment results merge in plan order before
SQL filtering, so worker scheduling cannot alter row or tie order.

Without a governor or with one reserved worker, the identical batch operator
uses the serial segment source. Small queries never pay batch or worker setup.

## Receipt

`NativeSqlExecutionReceipt` binds the result to its CSN, catalog version, and
root digest and reports:

- direct, exact index-intersection, bounded indexed nested-loop join,
  vectorized-serial, or vectorized-parallel execution;
- visible candidates decoded;
- total rows read from index streams and the first intersection-stream size;
- right-side probes attempted by indexed joins, including null-key probes;
- vector batches evaluated;
- workers reserved and worker batches executed; and
- the outer admission/queue/execution observation when a governor exists.

The existing unprofiled API delegates to this executor and returns only the
exact `SqlResult`.

## Physical work ceilings

Every bounded native SQL scan has a separate hard ceiling of 1,048,576
physical candidates. The ceiling applies consistently to transaction-local,
materialized-snapshot, and current-root execution, including primary ranges,
secondary-index lookups and ranges, exact index intersections, and vectorized
batch sources. Index intersections charge every row read from every input
stream rather than charging only the surviving intersection.

Exhaustion returns `HYSQL019` and discards the operation-local result buffer;
no partial result is observable. The product boundary retains the subcode and
emits a typed `sql_scan_candidates` limit with the configured and observed
counts. SQL `LIMIT` continues to constrain matching rows returned to the
caller, while this independent engine ceiling constrains the physical work
needed to discover those matches when residual predicates are sparse or
adversarial.

Bounded indexed joins use the analogous 1,048,576 visible-left-row ceiling and
return `HYSQL018` with typed `sql_join_candidates` evidence. Join right-side
probes remain separately observable, so a future cost model can distinguish a
large left input from repeated or missing right-side keys.

## Current evidence

The runtime differential test builds a multi-leaf typed relation with updates
and tombstones, compares the vectorized current-root result to the materialized
scalar snapshot oracle, and proves equality between serial and governed
parallel paths. It also verifies the direct-path threshold and release of CPU,
I/O, and memory tokens.

The same prepared executor can intersect two or more disjoint exact secondary
index bindings as ordered physical primary-key streams. It applies the complete
residual expression after intersection and returns canonical primary-key order
on private-transaction, materialized-snapshot, current-root, and reopened
surfaces. The binder prefers a complete composite equality index over an
intersection when one exists, avoiding redundant index work.

`prepare_sql_latest` also retains up to 256 successful prepared statements per
open database handle. Every entry includes the already-bound filter expression
and belongs to exactly one catalog version. The first prepare after a catalog
transition atomically retires all older entries before lookup, while the
existing execution-time catalog check continues to reject previously returned
stale plans. Cache counters expose entries, hits, misses, catalog invalidations,
and deterministic insertion-order capacity evictions. Failed prepares are not
cached.

For intersections of three or more indexes, the current-root executor estimates
each exact prefix from immutable B+tree leaf cardinality summaries bound to the
same root. It orders streams by that conservative estimate, with index identity
as the deterministic tie break, before decoding index rows. Two-index queries
retain their direct order so the common case does not pay a second planning
walk. The receipt records actual total input rows and the chosen first-stream
size. A skewed differential test proves that a 384-row broad predicate is not
chosen ahead of the selective 48-row-or-smaller prefixes.

Bounded joins now identify their physical strategy as indexed nested-loop and
record every visible left candidate plus every attempted right-side key probe.
Point joins remain on the direct path. This makes missing-right rows, null join
keys, and skew visible instead of reporting only returned throughput. It is
observability and cost-model input, not a claim that hash or merge joins exist.
Every bounded join also has a hard ceiling of 1,048,576 visible left
candidates. Exhaustion returns `HYSQL018` without a partial result. SQL `LIMIT`
continues to constrain returned matches, while the separate engine ceiling
prevents a sparse or adversarial right side from turning a bounded plan into an
unbounded foreground walk. The product boundary preserves `HYSQL018` and emits
typed `sql_join_candidates` limit evidence instead of collapsing the failure
to an internal error.

Primary and secondary scans apply the same fail-closed principle through the
independent `HYSQL019` physical-candidate ceiling described above. Direct-path
receipts now report candidates as well as vectorized paths; a zero candidate
count therefore means that no physical candidate was consumed, not merely that
batch execution was not selected.

## Still required for P5

- column-selective and SIMD decode kernels with portable differential tests;
- measured allocation-free prepared point execution;
- persisted fine-grained min/max, null-count, membership, and selectivity
  summaries with stale-statistics controls;
- vectorized aggregation and sorting;
- adaptive nested-loop, hash, and merge joins;
- calibrated NUMA placement and deterministic parallel joins;
- bounded spill under explicit I/O and memory tokens;
- cancellation and stale-statistics matrices; and
- adversarial residual-filter and multi-index budget matrices across every
  execution surface;
- dedicated-hardware latency, scaling, allocation, cache, and interference
  evidence.

Until those items and Gate P5 are proven, this document is implementation
evidence only.
