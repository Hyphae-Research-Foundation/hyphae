# Native ANN kernel evidence

Date: 2026-08-01

Status: deterministic HNSW kernel slice; G1, G4, and G7 remain open

Source commit:
`3e353c089344e47e650caa85408421423a5e5608`

Source tree:
`66e80cee66ec8029f6a4b82497b9cc84d5ca6fe3`

Branch: `codex/native-ann-kernel`

## Change

Hyphae now owns an executable ANN kernel in `hyphae-native-ann`. It does not
call or wrap an external ANN implementation. The kernel provides:

- canonical finite `f32` vectors with signed-zero normalization;
- fixed dimensions from 1 through 65,535;
- cosine, negative dot-product, and squared-L2 distance;
- deterministic HNSW levels, construction order and neighbor tie-breaks;
- an exact full-scan quality oracle;
- bounded `k`, `ef_search`, candidate and exact-rerank receipts;
- updates and deletes through a canonical full-graph rebuild;
- a content-bound physical-build identity; and
- persistence-facing vector/graph snapshots with fail-closed canonical
  restore.

`M` is bounded to 2 through 64. V1 requires
`ef_construction >= M`, derives levels from BLAKE3 over the definition digest
and object ID, and orders canonical rebuild input by creating CSN followed by
object ID.

Mutation methods build a replacement generation privately and swap it into
the handle only after validation. A rejected dimension or zero cosine vector
therefore leaves the prior build identity unchanged.

## Red and green evidence

The first targeted run failed because none of the required ANN types existed:

```text
cargo test -p hyphae-native-ann
error[E0432]: unresolved imports HnswConfig, HnswIndex, Metric,
SearchOptions, Vector, VectorIndexDefinition
```

After implementation, nine focused tests pass. They cover:

- exact metric goldens and stable object-ID tie-breaks;
- explicit approximation and search breadth;
- deterministic graph/build identity across arrival orders;
- update/delete rebuild without stale neighbors;
- rollback observability after invalid updates;
- NaN, infinity, dimension and zero-cosine admission failures;
- duplicate object-ID rejection;
- corrupted build identity and neighbor records;
- a deterministic 512-vector quality corpus with recall@10 at least 0.95; and
- the executable benchmark target.

## Validation

Debian 13 under WSL2 executed the complete workspace:

```text
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Windows completed the non-execution constraints:

```text
cargo fmt --all -- --check
cargo clippy -p hyphae-native-ann --all-targets -- -D warnings
cargo check --workspace --all-targets --all-features
python tools/check_documentation.py --binary target/debug/hyphae.exe
python tools/check_crate_packages.py
```

The documentation check covered 154 Markdown files and 12 JSON examples. The
crate package audit covered the ten publishable packages and 23 compile-time
assets; the ANN crate remains deliberately unpublished.

After the ANN test binary changed, Windows Application Control blocked its
execution with `os error 4551`. The policy was not weakened. WSL2 executed the
same target. `cargo-mutants` is not installed, so mutation testing was not
executed and ordinary tests are not represented as a substitute.

## Quality and latency observation

The checked
[machine-readable receipt](native-ann-kernel-wsl2.json) was produced by:

```text
cargo run --release -p hyphae-native-ann --example quality_smoke -- \
  3e353c089344e47e650caa85408421423a5e5608 \
  "Debian 13 WSL2; Linux 6.18.33.1; x86_64; Intel Core Ultra 9 285H; \
16 CPUs; 16115352 kB RAM; rustc 1.96.0"
```

The deterministic corpus contains 10,000 vectors, 32 dimensions and 100
independent queries. With `M=16`, `ef_construction=128`, `ef_search=128` and
top 10:

- recall@10 was `0.970`;
- the minimum per-query overlap was 7 of 10 and the median was 10 of 10;
- build time was `7,026.173 ms`;
- HNSW p50/p95/p99 were `716.048/823.839/1,270.667 us`; and
- exact p50/p95/p99 were `639.333/689.556/846.339 us`.

The quality floor passed, but HNSW was slower than exact search on this small
corpus. This is an observation, not the one-million-vector G4/G7 gate. It has
only 100 concurrency-one queries under virtualization and lacks 384
dimensions, p99.9, warm/cold separation, saturation, interference,
allocation/RSS, hardware counters, update/delete cost and a persisted graph.

## Product boundary

This slice is not durable native ANN. The native search runtime still lacks:

- catalogued metric and HNSW definitions;
- vector, metadata and graph B+tree namespaces;
- WAL opcodes and all-engine MVCC visibility;
- transactional delta merge, versioned tombstones and background generation
  publication;
- direct buffered graph traversal;
- filters, doc values, hybrid fusion and stable-ID SQL joins;
- interrupted-build and page-level corruption recovery; and
- the one-million-vector, 384-dimensional recall and latency gate.

G1, G4, G7 and G8 remain explicitly open.
