# Native inverted-search evidence

Date: 2026-08-01

Status: first native physical lexical index; G1, G4, and G7 remain open

Source commit:
`35a58cd02a1764a2052ab6cbf67a514eae38fc55`

Source tree:
`e28ecccc6a00a2e229844a3d15e23ad9650d1e5b`

Branch: `main`

## Change

Hyphae now owns a physical lexical-search index. New directories no longer
serialize the complete search state into one page and `MATCH` no longer needs
to scan every source document.

One `HYSEABT1` native B+tree root contains independent:

- `HYIDX001` collection statistics;
- `HYDOCS01` stored documents and analyzed lengths;
- `HYTERM01` document-frequency entries; and
- `HYPOST01` per-document term-frequency postings.

All entries use the common page store, buffer pool, WAL, MVCC root set, CSN,
blob namespace, conflict table, and recovery authority. There is no
OpenSearch/Lucene process, REST hop, external database, or sidecar.

## Physical query path

The current direct `MATCH` path:

1. verifies collection metadata with a pinned B+tree point lookup;
2. deduplicates query terms using the deterministic analyzer;
3. point-reads each term's document frequency;
4. uses a length-delimited term prefix and internal separators to visit only
   that term's posting range;
5. reads only candidate document lengths;
6. computes the same BM25 contribution as the materialized reference; and
7. orders by descending score and bytewise document ID.

The B+tree now exposes page-store and buffer-pool prefix scans. They derive an
exclusive binary prefix successor, handle all-`0xff` prefixes, and prune
nonintersecting internal children. They currently return owned key/value
vectors; a zero-copy streaming postings cursor remains pending.

## Write and recovery semantics

`INDEX DOCUMENT` is immutable in this slice. One transaction stores source
text, exact token count, per-term frequency, collection totals, term document
frequencies, and postings under one copy-on-write root. Text above 8,192 bytes
uses `HYDOCS01` with the common 56-byte blob reference. The WAL uses the same
envelope, so large source text is not duplicated there.

Complete-state validation reanalyzes stored source and requires exact equality
with every count and posting. Malformed namespaces, noncanonical terms,
orphan documents/postings, divergent counts/frequencies, invalid UTF-8,
broken envelopes, and missing/corrupt blobs fail closed.

Legacy page-kind-10 search directories remain readable and writable without
implicit conversion.

## Correctness evidence

The native runtime now has 41 tests. New coverage proves:

- exact score/order equality between physical postings and the reference BM25
  scorer over 512 documents and multiple query shapes;
- a multilevel search tree, retained historical snapshot, later document,
  strict commit, reopen, and direct physical read;
- separator-pruned prefix scans across a multilevel tree and unbounded
  `0xff` suffixes;
- optimistic disjoint-document rebase and all-engine crash recovery through
  the existing convergence matrices;
- one large value deduplicated across a relational row, scalar, hash field,
  and search document;
- fail-closed forged collection cardinality;
- canonical 4,096-byte composite-key bounds; and
- legacy inline-search reopen, mutation, and second reopen.

The complete Debian 13/WSL2 workspace passed:

```text
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Windows all-target compilation and strict Clippy also passed. Newly linked
Windows test executables were not run because the active Application Control
policy blocks them; the policy was not weakened.

## Latency observation

The exact
[machine-readable v6 receipt](native-microsecond-smoke-search-wsl2.json) uses
2,048 search documents alongside 2,048 scalar keys, 2,048 hash fields, and
2,049 relational rows. Every physical tree has height two. The measured query
is one rare term with document frequency one and limit one.

The complete physical `MATCH` call observed:

- p50 `20.926 us`;
- p95 `28.933 us`;
- p99 `71.596 us`;
- p99.9 `183.029 us`; and
- aggregate throughput `43,414 operations/s`.

Unlike the earlier point-read batches, search used one complete call per timer
observation for 100,000 observations. This establishes a direct
inverted-index baseline in the microsecond domain. It is not a one-million
document BM25 result and does not pass G7.

The dominant avoidable costs are currently owned query-token sets, decoded
posting vectors, document-length and score maps, and final hit allocation.
The next physical optimization is a borrowed streaming posting cursor with a
bounded top-k accumulator and request arena.

## Product boundary

This does not make the search engine OpenSearch-complete. Still missing:

- document update/delete, tombstones, retention, and segment merge;
- positions/offsets, phrase and proximity queries;
- boolean, prefix, fuzzy, wildcard, range, exists, and filter operators;
- doc values, sorting, facets, aggregations, and highlighting;
- multi-field BM25F, explanations, analyzer definitions and golden corpora;
- broad-query budgets, cancellation, spill, and randomized rebuild
  equivalence;
- exact vector, ANN, and hybrid fusion on this native substrate;
- local-protocol, SQL, CLI, and SDK exposure; and
- concurrency, saturation, cold-state, background-interference, allocation,
  hardware-counter, and dedicated-hardware evidence.

G2 SQL completeness, G3 structure completeness, and every later gate remain
independent and open.
