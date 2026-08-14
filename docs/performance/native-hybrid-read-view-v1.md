<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->

# Native hybrid read view v1

Status: implemented qualification candidate with a receipt-v4 contract; no P4
or G7 closure claim

`NativeHybridReadView` is one process-local read authority for lexical and ANN
search at one immutable committed root. It exists to remove repeated snapshot,
catalog, page, and ANN-restore work from the hybrid hot loop without turning a
cached final result into the measured operation. Its lexical branch is an
explicit prepared physical query: open binds and retains encoded physical
inputs, while every observation still performs decode, BM25 scoring, merge,
ranking, explanations, and result construction.

This contract does not alter the lexical, ANN, MVCC, governor, durability, or
proof authorities. It composes them under one captured root and makes their
shared authority and timed work mechanically auditable.

## Authority and public surfaces

Opening a hybrid view captures one coordinator snapshot and its visible CSN,
catalog root, search root, and complete root-set identity. From that single
capture the database opens:

- a `NativeLexicalReadView` bound to one lexical index definition and its
  immutable root/index metadata; and
- a `NativeAnnReadView` bound to one ANN index, base/view identity, routing
  policy, and retained governor admission.

The two branches must not capture separate current snapshots and then merely
compare their CSNs. They are children of the same root authority. The hybrid
open receipt records the root identity and CSN once, plus the lexical-index
identity and ANN-view identity. A mismatch fails the open.

`NativeLexicalReadView` is query-bound. It may retain the immutable root,
catalog/index identity, invariant corpus counts, canonical analyzed terms, raw
encoded posting records, raw encoded document headers, the root-derived
physical plan, governed handles, and its retained-memory permit. The receipt
declares this scope exactly as
`lexical_plan_scope=query-bound-encoded-postings-v1`.

The view must not retain decoded term frequency, decoded document length,
precomputed IDF or average length, term contributions, BM25 scores, merged
rankings, explanations, hits, or final results. Every observation re-decodes
the retained physical records, derives the BM25 inputs and scores, merges and
ranks candidates, and constructs fresh explanations and results. The query
interval declares that measured work exactly as
`lexical_execution=decode-bm25-rank-per-observation-v1`. This is comparable to
a prepared SQL physical query, not final-result caching.

The hybrid view owns the two immutable branch handles and one parent lifecycle.
Cloning it clones handles, not index contents, snapshots, or permits. Dropping
the last handle releases both retained admissions exactly once. A root advance
does not mutate an existing view; a newly opened view sees the new authority.

## Open and hot-loop boundary

The open phase occurs outside warmup and measurement. It may perform the exact,
governed physical work required to validate and bind the selected lexical
index and hydrate the ANN view. Its receipt reports planned and observed
physical entries and bytes. Those counters describe the query-bound encoded
lexical inputs retained by open. Observed work must not exceed the admitted
physical plan.

The physical plan is engine-derived from root-bound metadata before retained
admission and the bounded content scan. A caller-provided maximum is only a
rejection ceiling and must not be copied into `planned_physical_entries` or
`planned_physical_bytes`. `observed_physical_entries` and
`observed_physical_bytes` are the exact encoded records and bytes visited by
that bounded scan; logical retained capacity or allocator estimates are not
physical observations. `retained_memory_bytes` is measured owned memory and
must not exceed `admitted_retained_memory_bytes`. The runner must preserve
these distinct sources rather than alias one counter into another.

After open, one hybrid query first acquires one atomic foreground peak
admission. Its compute limit is the exact governed ANN worker limit, its I/O
limit is zero, and its memory limit is the result-retention requirement plus
the larger of lexical and ANN scratch. This one queued permit prevents partial
multi-resource acquisition and convoy. The query derives bounded subdivisions
from that parent without another governor queue:

1. a memory-only result-retention subdivision with zero compute threads and
   zero I/O slots is created before either branch and remains live through
   result and receipt construction;
2. the lexical branch uses a one-thread, zero-I/O scratch subdivision to
   re-decode, score, merge, and rank the prepared encoded lexical inputs;
3. the ANN branch then executes under worker-bounded sequential-phase evidence
   covered by the peak parent; the shared canonical execution pool subdivides
   workers from that parent without creating a second scratch permit; and
4. fusion uses a compute-only subdivision with one compute thread, no I/O slot,
   and zero additional memory to combine the retained fresh branch rankings
   with deterministic reciprocal-rank fusion.

Only after the canonical result and both execution receipts exist may the
result-retention subdivision and peak admission be released and the result
published. This lifetime prevents branch outputs from escaping their admitted
memory boundary while keeping fusion compute separate from result ownership.

The two branches are sequential in v1; the peak limit is a capacity bound, not
a claim of concurrent branch execution. No subdivision may exceed the parent,
create another worker pool, acquire unreported capacity, or use TCP, HTTP,
JSON, or another serialized compatibility path.

During the timed interval, every query must report zero hydration, physical
page reads, index-scoped restores, full-state loads, and full-catalog loads.
Opening a snapshot, restoring ANN, scanning durable pages, or materializing a
complete engine state inside the interval fails the qualification. Cancellation
is cooperative before peak admission, before the lexical branch, between the
two branches, between ANN routing waves, before fusion, after fusion, and before
result publication. Error or cancellation releases the parent and all live
subdivisions without mutating or poisoning the reusable view.

## Prepared lexical and filtered BM25 surfaces

The standalone G7 BM25 surface executes the lexical child of the same
`NativeHybridReadView`; it does not call a latest-snapshot API per observation.
Its open evidence therefore has the same root identity, snapshot CSN, lexical
index identity, physical plan, observed physical work, and retained allocation
as the hybrid lexical open. Every observation increments the lexical execution
sequence exactly once, evaluates the one retained `rare` posting, and reports
zero receipt-level and process-level page reads. The complete one-million-
observation interval must form one gapless execution-sequence span and report no
full-state or full-catalog load.

`NativeFilteredLexicalReadView` composes a prepared lexical query with one exact
structure scalar predicate by deriving from the already-open hybrid lexical
child. It shares that child's root, snapshot, physical lexical plan, and retained
admission; it must not hydrate or retain a second lexical plan. Filter planning
is governed before the structure scan. Filter hydration acquires its retained-
memory and I/O admission before it copies any encoded record. The open receipt
binds the root identity, snapshot CSN, catalog version, complete lexical-open
identity, structure root and predicate identity, planned and observed filter
physical entries and bytes, retained record count, retained allocation, and
both filter planning and hydration admissions. Its
`open_filter_physical_page_reads` counts only filter-open work and must never be
populated by copying the earlier lexical-open counter. Receipt v4 therefore
does not claim or duplicate a second lexical open receipt: it binds the derived
view to the hybrid lexical child through exact root, CSN, lexical-index identity,
and plan-scope equality, while runtime tests prove the shared handle and single
lexical hydration.

The frozen filter value scope is `inline-scalar-only-v1`. A blob-backed
candidate is outside this public boundary and makes view open fail closed. The
view may retain only unique candidate document IDs and their raw encoded inline,
tombstone, expiry, or missing-record state. It must not retain predicate matches,
decoded values, BM25 scores, rankings, hits, or results.

Every filtered observation declares
`decode-expiry-inline-value-filter-before-rank-v1`, re-decodes the retained
predicate records, applies expiry and scalar equality before final ranking,
decodes and scores only admitted postings, and constructs fresh hits. The G7
corpus has a filter density of exactly `0.5`; the single `rare` candidate is a
`keep` record, so measured candidate selectivity is exactly `1.0`. These are
different quantities and the receipt must not alias one into the other. Every
execution increments its filtered-view sequence exactly once; the one-million-
observation interval is one gapless first/last sequence span.

## Selected-certified ANN routing

Receipt v4 applies the same evidence contract to standalone ANN and the ANN
branch of hybrid search. The preferred budget is exactly 32 logical
partitions. A query may certify after any non-empty adaptive prefix within
that budget; it is not required to execute all 32 partitions.

For each one-million-observation interval, the receipt reports:

- selected-certified observations equal to actual observations;
- no requested full fanout, budget fallback, or single-generation fallback;
- a next-partition lower bound for every observation;
- maximum selected partitions in `1..=32`;
- maximum execution workers within the governed per-query limit;
- maximum executed worker batches in `1..=32`; and
- maximum execution waves in `1..=6`, the complete `1, 2, 4, 8, 16, 32`
  widening envelope.

The minimum omitted-partition lower bound and maximum kth result distance must
both be finite floating-point values, and the minimum lower bound must be
strictly greater than the maximum kth distance. Missing, equal, unordered,
infinite, or NaN bounds fail closed. This aggregate condition is intentionally
stronger than a selected-outcome label: it proves that every reported omission
remained certified throughout the interval.

## Deterministic RRF and correctness oracle

Fusion uses these receipt-v4 constants:

- reciprocal-rank constant: `60`;
- contribution scale: `1_000_000_000`;
- lexical and vector weights: `1` and `1`;
- result limit: `10`; and
- tie break: fusion score descending, then object ID ascending.

For a branch rank `r`, its integer contribution is
`floor(1_000_000_000 / (60 + r))`. A missing branch contributes zero. The
fusion score is the exact sum of both contributions. Object IDs are canonical
non-zero lowercase 128-bit hexadecimal strings. Explanations contain the two
optional branch ranks, both contributions, the fusion score, and the final
rank.

The G7 oracle executes outside the timed interval so correctness work cannot
improve or pollute latency. It independently evaluates both branches at the
same root identity and CSN as the measured hybrid view, then recomputes fusion
without consuming the measured result as its expected value. The runner emits
the branch rankings, fused explanations, and two identical digests:
`result_digest` and `oracle_digest`.

The digest is SHA-256 over UTF-8 canonical JSON of the fused-result array with
ASCII escaping, lexicographically sorted object keys, and no insignificant
whitespace. The checker independently recomputes all ranks, contributions,
ordering, and the digest. An oracle from another root/CSN, a mutated
explanation, a non-canonical ID, duplicate input, ranking drift, or digest
mismatch fails closed.

## G7 receipt v4

Receipt v4 is a deliberate schema change. Receipt v3 remains immutable and
cannot carry hybrid read-view closure evidence.

The ANN open object adds its positive `snapshot_csn`. The hybrid cell records:

- `lexical_read_view_open` and `lexical_read_view_query_interval` on the
  standalone BM25 cell, including the shared root/CSN/index identity, exact
  bounded open plan, a gapless execution-sequence span, exact posting count,
  and zero receipt/process page reads or complete-state loads;
- `filtered_lexical_read_view_open` and
  `filtered_lexical_read_view_query_interval` on filtered BM25, including the
  same lexical root authority, independent structure-filter identity and
  inline-only value scope, planning/hydration admissions, exact pre-rank
  evaluated/matched/scored counts, a gapless execution-sequence span, and zero
  hot storage/materialization work;
- `per_query_worker_limit`, the exact positive bounded
  `query_queue_wait_millis` shared with standalone ANN, and
  `preferred_partition_budget`;
- `hybrid_read_view_open`, including root identity, snapshot CSN, lexical-index
  identity, ANN-view identity, exact query-bound plan scope, independent
  planned/observed physical work, admitted retained bytes, and measured
  retained bytes;
- `hybrid_read_view_query_interval`, with exact observations and all forbidden
  storage/materialization counters at zero plus the exact per-observation
  decode/BM25/rank execution declaration, one atomic peak admission, one
  memory-only result-retention subdivision spanning each observation, and one
  compute-only fusion subdivision per observation;
- `hybrid_ann_routing_interval`, with the selected-certified evidence above;
  and
- `hybrid_oracle`, with root-bound branch rankings, canonical RRF
  explanations, and matching result/oracle digests.

The public checker helper `validate_hybrid_read_view_cell` is the single Python
authority for the hybrid cell. Matrix and controller code must call or reuse
that helper rather than duplicate weaker rules.

For the complete measured interval, peak, result-retention, and fusion
execution counts each equal observations. Peak admission is
`foreground-bounded`, has the exact per-query worker limit, zero I/O slots, and
positive ordered memory bounds. Its minimum memory is at least the maximum
result-retention memory. Result retention is `foreground-bounded`, has zero
compute threads and I/O slots, and positive ordered memory bounds. Fusion is
`foreground-bounded`, has exactly one compute thread, zero I/O slots, and zero
memory. Missing, extra, boolean-as-integer, contradictory, or underprovisioned
aggregate evidence fails closed.

## RED/GREEN acceptance

Implementation is eligible for dedicated measurement only after deterministic
tests prove all of the following:

1. One captured root opens both branch views, while deliberate root, CSN,
   lexical-index, and ANN-view substitutions fail.
2. Repeated lexical execution may reuse only canonical analyzed terms and raw
   encoded postings/document headers. It independently re-decodes frequency
   and length and recomputes IDF, BM25 scores, merge, ranking, explanations,
   hits, and final results.
3. Filtered lexical open plans and admits before copying encoded structure
   records; repeated execution re-evaluates the predicate before final ranking;
   root, CSN, lexical identity, and structure-filter identity drift fail closed.
4. One open followed by repeated queries performs no hydration, page read,
   restore, full-state load, or full-catalog load in the query interval.
5. Adaptive ANN prefixes return the same canonical top-k as the complete
   oracle, never search a child twice, and expose a strict finite omission
   bound for every selected-certified observation.
6. RRF order and explanations reproduce across concurrency, reopen, and
   process restart at an unchanged durable root. Any branch-ranking mutation
   changes the independently recomputed digest.
7. Root advance leaves the old view stable; a new view observes the new root;
   vacuum after open cannot invalidate the owned ANN state or lexical root
   authority.
8. Success, cancellation, branch error, fusion error, panic, and last-handle
   drop return the atomic peak permit and all sequential subdivisions without
   leaking or double-releasing capacity.

These tests are functional authority, not performance evidence. Receipt v4
does not change the frozen 1,000,000-document and 1,000,000-vector corpus,
384-dimensional vectors, 100,000 warmup operations, 1,000,000 measured
observations, latency thresholds, recall floor, matrix, or runtime budgets.
Only accepted exact-source bare-metal receipts may close G7.
