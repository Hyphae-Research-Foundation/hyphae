# Native ANN durability evidence

Date: 2026-08-01

Status: first native durable ANN vertical; G1, G4 and G7 remain open

Source commit:
`7117daf6bed24ef064262a8c01b729bbeb271540`

Source tree:
`e6685e12c3ea0981afc165a07f6182c51ab5ffd0`

Branch: `codex/native-ann-durability`

## Change

The source commit connects the Hyphae-owned deterministic HNSW kernel to the
native runtime without adding a database, cache, search server or ANN
dependency. One search B+tree now owns:

- current-generation `HYANNM01` metadata;
- canonical `HYANNV01` finite-`f32` vectors with creating CSNs; and
- per-layer `HYANNG01` stable-ID neighbor lists.

The catalog pins cosine, negative-dot or squared-L2 distance plus `M`,
construction breadth, default/maximum search breadth and seed. Its explicit
ANN vector tag preserves the exact legacy vector bytes and prevents a
truncated ANN definition from decoding as legacy.

`CREATE ANN INDEX=17`, `UPSERT VECTOR=18` and `DELETE VECTOR=19` are canonical
search-engine WAL mutations. They publish through the same root set and CSN as
catalog, relational, structure and lexical state. Vector-level optimistic
conflict identities allow disjoint vectors to rebase and reject a stale writer
to the same vector.

Open and snapshot materialization validate all physical ANN keys and values,
reconstruct the selected `IndexSnapshot`, and require canonical
`HnswIndex::restore`. Queries expose exact ranking or an explicitly
approximate receipt containing snapshot CSN, build identity, metric,
`ef_search`, candidate count, reranking status and visited-node count.

## Batch rebuild correction

The first benchmark worktree rebuilt the complete graph once for every vector
inside the private transaction and repeated that behavior during commit. For
512 vectors this observed approximately `20.984 s` private seeding and
`21.036 s` strict commit.

The source commit adds atomic duplicate-free `upsert_vectors` and groups the
ordered committed mutations by index. Each side now builds one canonical
replacement. A trial on the same Windows corpus observed `118 ms` private
seeding and `209 ms` strict commit. The checked WSL2 receipt observed
`88.642 ms` and `127.021 ms`, respectively. This is a bounded correction, not
a complete ingestion-throughput gate.

## Transaction, recovery and failure evidence

The end-to-end fixture creates a relation row, structure value, lexical
document and vector index/vectors in one strict transaction and verifies one
CSN across all four roots. A later vector update/delete/insert retains the
historical snapshot and recovers the current generation byte-for-byte.

Additional tests prove:

- invalid dimension and duplicate-ID batches leave the private generation
  unchanged;
- disjoint optimistic vector writers rebase while same-vector writers use
  first-committer-wins;
- all seven implemented commit interruption boundaries recover either no
  transaction or the complete relational/structure/lexical/ANN CSN;
- malformed current vectors fail the complete root;
- an ANN generation record without metadata fails the complete root;
- WAL vector identities must be exactly 16 bytes and values must be nonempty
  `f32`-aligned bytes; and
- catalog definitions reject every truncated prefix, invalid HNSW bound and
  ANN-without-vector shape.

## Red and green evidence

The contract-first targeted run initially failed on unresolved catalog ANN
types, the missing `ann` search-definition field, missing vector runtime
methods and missing ANN error variants. After implementation, the runtime has
87 passing tests and the catalog has 11 passing tests. Five runtime tests are
ANN-specific; the existing recovery and cross-engine suites remain green.

Mutation testing was not executed. Deterministic unit, integration, crash and
corruption tests are not represented as a mutation score.

## Mechanical validation

Windows x86-64, Rust/Cargo 1.96.0, passed on the source:

```text
cargo fmt --all -- --check
cargo clippy -p hyphae-native-runtime --all-targets -- -D warnings
cargo test -p hyphae-native-runtime --all-targets
```

Debian 13 under WSL2, Rust/Cargo 1.96.0, passed:

```text
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 tools/check_integration_boundaries.py
python3 tools/check_crate_packages.py
```

The integration audit returned `integration-boundaries-ok`. The package audit
covered 10 packages and 23 compile-time assets. After this evidence was added,
documentation validation covered 156 Markdown files and 12 JSON examples;
the executable KV/query, vector-mutation and exact/lexical/hybrid examples
also passed, as did rustdoc with warnings denied.

## Durable quality and latency observation

The checked
[machine-readable receipt](native-ann-durability-wsl2.json) was produced by:

```text
cargo run --release -p hyphae-native-runtime \
  --example ann_durable_smoke -- \
  7117daf6bed24ef064262a8c01b729bbeb271540 \
  "Debian 13 WSL2; Linux 6.18.33.1; x86_64; \
Intel Core Ultra 9 285H; 16 CPUs; 16115344 kB RAM; rustc 1.96.0; \
uncontrolled desktop"
```

The executable creates a strict native directory, batches 512 deterministic
32-dimensional vectors, commits CSN 1, records the pre-reopen result, reopens,
materializes a pinned snapshot, requires an identical generation/result, and
then measures 64 deterministic queries. Each exact and HNSW route receives
1,000 warmups and 10,000 concurrency-one observations.

With `M=16`, construction/search breadth 128, exact reranking 128 and top 10:

- recall@10 was `1.000`;
- minimum and median per-query overlap were both 10 of 10;
- HNSW p50/p95/p99/p99.9 were
  `227.227/305.403/483.309/826.426 us`;
- exact p50/p95/p99/p99.9 were `23.032/25.592/42.020/169.375 us`;
- strict commit was `127.021 ms`;
- reopen was `89.845 ms`;
- snapshot materialization was `86.492 ms`; and
- the first query after snapshot creation was `230.911 us`.

The query clocks exclude commit, reopen and snapshot materialization. The
materialized HNSW route is inside the provisional 250-us p50 and 900-us p99
targets for this scenario, but exact ranking remains roughly ten times faster
on the small corpus.

## Remaining boundary

This milestone is durable ANN, but it is not the complete G4/G7 engine:

- query execution traverses a validated in-memory materialization rather than
  buffer-pool pages;
- foreground updates/deletes rebuild a complete generation;
- old generations lack reclamation;
- transactional delta/graph merge and versioned tombstones are absent;
- snapshot filters, doc values, hybrid fusion and stable-ID SQL joins are
  absent;
- background generation build/publication and its interruption matrix are
  absent;
- the corpus is 512 vectors at 32 dimensions, not 1,000,000 at 384;
- WSL2 is virtualized and the desktop was uncontrolled; and
- concurrency 8/32, saturation, interference, cold query, allocations, RSS,
  hardware counters, page faults and storage counters were not measured.

G1, G4 and G7 therefore remain explicitly open.
