# Native sorted-set member-rank evidence on Linux

Date: 2026-08-02

Status: first bidirectional physical sorted-set member rank; complete G3, G7,
hosted CI, and mutation testing remain open

Source commit:
`63290be966a17882d7cf5fa33c73621242c8cada`

Source tree:
`02c79247d167c4210de1a33f65cf2b2e26d54ab0`

Source branch: `codex/native-sorted-set-ranks`

Base: `dev@7cf616a8f8dfad10ca4168a1724fcd12a6da2876`

## Scope

This slice adds zero-based `ZRANK` and `ZREVRANK` semantics to the native
dual-index sorted set. Ascending rank uses canonical score followed by exact
member bytes. Reverse rank reverses that complete total order, including the
member-byte tie-breaker. A missing member returns no rank; a missing sorted set
or another structure family remains a typed error.

Transaction-private and retained-snapshot reads count the frozen model without
materializing an ordered result. Current-root and reopened reads validate
metadata, resolve the member's canonical score through the membership index,
construct its exact ordered identity, and visit the `0x0a` B+tree prefix only
through that target.

`ZRANK` visits ascending from the prefix head. `ZREVRANK` uses the new generic
reverse bounded-prefix visitor and starts at the prefix tail. Both ignore
tombstones without charging rank and stop at the live target. A missing or
non-live target, a visited live duplicate under a conflicting score, malformed
metadata, identity, score, or marker, or a live rank reaching declared
cardinality fails the complete call.

The first lookup is intentionally position-sensitive: no subtree live counts
or order-statistic augmentation were added. Complexity is proportional to the
target's distance from the natural traversal edge. No dependency, unsafe Rust,
WAL opcode, page format, catalog format, or structure format changed.

## Contract-first red and green

Contract commit `7bd68c0` froze total ordering, missing/type behavior,
execution-mode equivalence, physical targeting, tombstone handling, and the
non-accelerated boundary before the API existed.

The first exact behavioral command was:

```text
cargo test -p hyphae-native-runtime \
  sorted_set_ranks_match_private_snapshot_latest_and_reopen --lib
```

It failed at compile time with 14 `E0599` errors because transaction, snapshot,
and database rank methods did not exist.

Review then identified that deriving reverse rank from metadata cardinality
would over-trust a partial physical read. Contract correction `6bf7f5e`
required a true reverse ordered traversal before implementation. Code commit
`36748fd` added that reusable B+tree primitive, model ranks, public read
surfaces, current-root execution, and the deterministic matrix. Benchmark
commit `63290be` added the independent release harness.

The final deterministic matrix proves:

- exact forward and reverse tie ranks;
- private, retained, current-root, strict-reopen, and missing-member
  equivalence;
- typed missing-set and wrong-family failures;
- live-only rank through score updates and member tombstones;
- a 2,048-member height-two tree with tombstones on both sides of targets;
- inclusive/exclusive bounded reverse B+tree visitation and early stop; and
- direct current-root rejection of forged target markers, conflicting live
  scores, visited markers, metadata cardinality, and membership scores in both
  directions.

The source commit contains 16 passing native B+tree tests and 210 passing
native-runtime library tests. The read-only operation adds no WAL mutation or
new commit/crash-injection boundary.

## Direct-Linux latency observation

The release harness seeds and strictly reopens one 2,048-member, height-two
sorted-set B+tree with 64-byte members. It pins logical CPU 2, performs 1,000
warmups of all six routes, and records 10,000 complete physical calls per
route at concurrency one. Each sample validates the exact returned rank.

The exact clean-source command was:

```text
taskset -c 2 cargo run --quiet --release --locked \
  -p hyphae-native-runtime --example sorted_set_rank_smoke \
  > /tmp/hyphae-sorted-set-rank-linux-raw.json
```

| Operation | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Forward rank of head | 15.306 us | 15.612 us | 25.301 us | 30.729 us | 45.740 us | 64,198 ops/s |
| Reverse rank of tail | 10.606 us | 10.891 us | 19.749 us | 22.434 us | 27.461 us | 92,624 ops/s |
| Forward rank of middle | 155.005 us | 165.874 us | 168.376 us | 187.309 us | 316.489 us | 6,373 ops/s |
| Reverse rank of middle | 134.728 us | 145.532 us | 148.821 us | 174.851 us | 270.969 us | 7,323 ops/s |
| Forward rank of tail | 287.051 us | 302.590 us | 316.357 us | 571.530 us | 639.976 us | 3,436 ops/s |
| Reverse rank of head | 273.731 us | 287.972 us | 296.506 us | 415.874 us | 608.279 us | 3,608 ops/s |

For the tail target, reverse traversal reduces p50 by 96.305% and p99 by
93.757% and raises throughput 26.960 times relative to forward traversal. For
the head target, forward traversal reduces p50 by 94.408% and p99 by 91.467%
and raises throughput 17.793 times relative to reverse traversal. These paired
routes validate the expected position-sensitive algorithmic work in one
process and corpus; they are not regression thresholds.

All six observed p50 and p99 values remain below one millisecond on this
bounded height-two corpus. That is evidence for the microsecond-first
direction, not a universal latency promise or G7 closure.

The raw benchmark stdout has SHA-256
`98285338107111d737be1e6ab586b208ca0abdadfa7ac850c966958038171df5`.
The checked
[metadata-enriched receipt](native-sorted-set-rank-linux.json) preserves those
exact measurements and has SHA-256
`34a2eb042f4b9fcb07dc3049cc9f2ccd8a72459a36d42e66545a669d578355dc`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust `1.96.0`, LLVM `22.1.2`, release profile; and
- direct execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on Linux with runtime code fixed at the source commit and
only the evidence delta present:

```text
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
cargo check --release --locked -p hyphae-native-runtime \
  --example sorted_set_rank_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-sorted-set-rank-linux.json
git diff --check
```

Results on the source tree plus evidence-only changes:

- complete workspace tests across all targets and features: passed;
- native B+tree tests: 16 passed;
- native-runtime tests: 210 passed;
- focused rank matrix: 4 passed;
- complete workspace Clippy with warnings denied: passed;
- formatter and release harness check: passed;
- documentation: 201 Markdown files and 12 JSON examples passed;
- JSON validation and diff check: passed;
- dependency delta: none.

Hosted checks belong to the evidence commit and PR. Mutation testing was not
executed: this repository has no accepted mutation tool, operator policy, or
surviving-mutant threshold.

## Evidence boundary

This is one warm, concurrency-one, single-core, small-corpus observation.
Latency is position-sensitive by contract and increases linearly with live and
tombstoned entries traversed before the target. The receipt does not establish
cold behavior, larger or deeper trees, tombstone-heavy distributions,
allocation/RSS, concurrency, saturation, background interference, hardware
counters, local-protocol transport, fsync latency, process crash, physical
power loss, or p99.9 stability.

Score ranges, reverse range output, subtree live counts and true
order-statistic acceleration, sorted-set algebra, TTL, protocol exposure,
randomized model equivalence, memory amplification, the complete G3
correctness matrix, and the complete G7 benchmark matrix remain open. This
milestone closes none of G0, G3, or G7.
