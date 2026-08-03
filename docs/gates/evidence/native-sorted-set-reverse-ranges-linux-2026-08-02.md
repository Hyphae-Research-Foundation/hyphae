# Native sorted-set reverse-range evidence on Linux

Date: 2026-08-02

Status: first physical descending rank and score ranges; complete G3, G7,
hosted CI, and mutation testing remain open

Source commit:
`3a1fcd293d39bb7969b36572ed88caf8b0de9633`

Source tree:
`631d4117905b766d5bd932a29261f0def22162cf`

Source branch: `codex/native-sorted-set-reverse-ranges`

Stacked base:
`codex/integrate-pr59-pr60-pr61@6dd5ed914f1d45ba513f1ff4cf1a10694ac49e8a`

## Scope

This slice adds `ZREVRANGE` and `ZREVRANGE_BY_SCORE` to the native dual-index
sorted set. It is stacked on the clean #59-#61 integration branch because
protected `dev` remains unchanged pending owner-authorized promotion.

`ZREVRANGE` applies the existing signed, inclusive rank interval to the
descending total order. Negative indexes count from the tail of that
descending sequence. Equal-score member bytes reverse together with scores;
the operation never preserves ascending tie order accidentally.

`ZREVRANGE_BY_SCORE` keeps lower and upper endpoint meaning independent of
output direction. Inclusive, exclusive, unbounded, empty, inverted, infinity,
negative-zero, and `NaN` behavior therefore remains identical to
`ZRANGE_BY_SCORE`. Offset counts live matching members in descending order and
execution returns at most `limit` entries.

Private write batches and retained snapshots use one explicit
`SortedSetDirection`. Current-root and reopened rank ranges start from the
ordered B+tree prefix tail. Reverse score ranges apply the same canonical
physical bounds as ascending score ranges and visit only that interval in
reverse. Both skip tombstones without charging rank or offset and stop after
the requested live result boundary. They do not materialize the complete
physical sorted set.

Malformed metadata, live markers, identities, or scores fail the complete
physical call. No dependency, unsafe Rust, WAL opcode, page format, catalog
format, or structure format changed.

## Contract-first red and green

Contract commit `5f1b07a` froze:

- complete score/member tie reversal;
- signed rank and negative-index behavior in descending order;
- direction-independent score-bound meaning;
- live-only descending offset accounting;
- private, retained, current-root, and reopened equivalence;
- reverse physical pruning and early stop; and
- fail-closed malformed physical state.

The first exact behavioral command was:

```text
cargo test --locked -p hyphae-native-runtime --lib \
  sorted_set_reverse_ranges_match_private_snapshot_latest_and_reopen
```

It failed at compile time with eight `E0599` errors because the private,
snapshot, and database reverse-range methods did not exist. Implementation
commit `5f84086` added all surfaces, one direction type, model execution, and
physical forward/reverse visitors. The same command then passed.

The focused matrix proves:

- complete equal-score tie reversal;
- signed and negative descending rank intervals;
- inclusive, exclusive, unbounded, inverted, infinity, `NaN`, and zero-limit
  score behavior;
- private, retained, current-root, strict-reopen, missing-key, and wrong-kind
  behavior;
- live-only rank and offset accounting after member tombstones;
- bounded reverse traversal in a 2,048-member height-two B+tree; and
- fail-closed invalid markers and forged cardinality metadata.

During implementation, an overly generic patch context first placed two
physical helper extractions inside `ZSCORE` and `ZCARD`. The compiler rejected
the resulting undefined variables and return-type mismatches. Those methods
were restored exactly before any commit. Clippy later rejected an eight-argument
helper; canonical score bounds were grouped as one semantic pair instead of
adding a lint suppression.

The first benchmark draft also exceeded the repository's function-size rule
and used redundant field suffixes. Dataset preparation, route validation,
timing, and reporting were separated, the internal field names were simplified,
and report state is borrowed. No lint suppression was added.

The final source commit contains 226 passing native-runtime library tests. The
read-only API adds no mutation or crash-injection boundary.

## Direct-Linux latency observation

Benchmark commit `3a1fcd2` adds an independent release harness. It strictly
seeds and reopens one 2,048-member, height-two sorted-set B+tree with 64-byte
members, pins logical CPU 2, performs 1,000 warmups of all four routes, and
records 10,000 complete calls per route at concurrency one.

The exact clean-source command was:

```text
taskset -c 2 cargo run --quiet --release --locked \
  -p hyphae-native-runtime --example sorted_set_reverse_range_smoke \
  > /tmp/hyphae-sorted-set-reverse-range-linux-raw.json
```

| Operation | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical head-ten `ZREVRANGE` | 11.769 us | 12.022 us | 21.465 us | 24.618 us | 43.008 us | 83,571 ops/s |
| Physical tail-ten ascending `ZRANGE` control | 266.197 us | 279.235 us | 286.552 us | 379.923 us | 485.598 us | 3,709 ops/s |
| Physical middle-ten `ZREVRANGE_BY_SCORE` | 10.237 us | 10.500 us | 19.506 us | 23.211 us | 31.087 us | 95,782 ops/s |
| Physical middle-ten ascending score control | 10.530 us | 10.752 us | 19.603 us | 22.781 us | 29.379 us | 93,463 ops/s |

The descending head route returns the same highest ten members as the
ascending tail control in reverse order. Within this process and corpus,
starting from the natural reverse edge reduces p50 by 95.579%, p99 by 92.509%,
and raises throughput 22.531 times. This demonstrates avoided full-prefix
traversal; it is not a universal regression threshold.

The bounded reverse and ascending score routes use the same exact interval
with opposite output order. Their p50 differs by 2.783%, p99 by 0.495%, and
throughput by 2.481%, providing a same-run symmetry check rather than a
latency guarantee.

The raw benchmark stdout has SHA-256
`8ac17cf6579c879090952031e91111f069b0847a0afd8c890159ab61eee5caa7`.
The checked
[metadata-enriched receipt](native-sorted-set-reverse-range-linux.json)
preserves those exact measurements and has SHA-256
`1245edfd46d8bb62d0a278c344975e67d812638aed2fbe518e4ef01bd0ef295f`.

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
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example sorted_set_reverse_range_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-sorted-set-reverse-range-linux.json
git diff --check
```

Results on the source tree plus evidence-only changes:

- complete workspace tests across all targets and features: passed;
- native-runtime tests: 226 library tests passed;
- complete workspace Clippy with warnings denied: passed;
- formatter and release harness check: passed;
- documentation and JSON validation: passed;
- diff check: passed; and
- dependency delta: none.

Hosted checks belong to the evidence commit and draft PR rather than the
pre-evidence source commit. Mutation testing was not executed: the repository
has no accepted mutation tool, operator policy, or surviving-mutant threshold.

## Evidence boundary

This is one warm, concurrency-one, single-core, small-corpus observation. It
does not establish cold behavior, larger/deeper trees, tombstone-heavy
benchmarks, other member widths or distributions, allocation/RSS, concurrency,
saturation, background interference, hardware counters, local-protocol
transport, fsync latency, process crash, physical power loss, or p99.9
stability.

This milestone closes reverse range output only. Subtree-count order-statistic
acceleration, whole-hash lifecycle/iteration, set and sorted-set algebra/TTL,
streams, adaptive expiry backoff, randomized model equivalence, memory
amplification, protocol exposure, the complete G3 correctness suite, and the
complete G7 benchmark matrix remain open.
