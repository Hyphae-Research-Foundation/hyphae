# Native sorted-set score-range evidence on Linux

Date: 2026-08-02

Status: first bounded physical sorted-set score range; complete G3, G7,
hosted CI, and mutation testing remain open

Source commit:
`5697114e3e38bec0713be0d98c283ed8482005d9`

Source tree:
`93dc4c9d4231130562e43638069dcb856fbbd05b`

Source branch: `codex/native-sorted-set-score-ranges`

Base: `dev@7cf616a8f8dfad10ca4168a1724fcd12a6da2876`

## Scope

This slice adds `ZRANGE_BY_SCORE` semantics to the native dual-index sorted
set. It accepts independently inclusive, exclusive, or unbounded binary64
score endpoints plus a nonnegative offset and limit.

Results are ascending by canonical score and exact member bytes. Offset counts
only live members inside the interval, tombstones do not consume it, and
execution stops after the requested live result count. A zero limit still
validates both endpoints, key existence, structure kind, and physical
metadata. Equal inclusive endpoints select the exact score; other equal-bound
combinations and inverted intervals are empty. `NaN` is rejected, negative
zero is canonicalized to positive zero, and infinities remain valid.

Private transactions and retained snapshots evaluate the same frozen model.
Current-root and reopened execution map the canonical score bounds directly
onto the ordered `0x0a` B+tree namespace. Inclusive upper and exclusive lower
bounds use the binary successor of the complete sortable-score prefix so all
member ties remain inside or outside the interval as specified. The visitor
prunes nonintersecting subtrees and never materializes the complete sorted set.

Malformed ordered identities, noncanonical scores, invalid live markers, and
forged key prefixes fail the complete call rather than returning partial
results. No dependency, unsafe Rust, WAL opcode, page format, catalog format,
or structure format changed.

## Contract-first red and green

Contract commit `285db48` froze bounds, ordering, offset/limit, numeric,
execution-mode, physical-pruning, and fail-closed requirements before the
runtime surface existed.

The first exact behavioral command was:

```text
cargo test -p hyphae-native-runtime \
  sorted_set_score_ranges_match_private_snapshot_latest_and_reopen --lib
```

It failed at compile time with 12 `E0599` errors because transaction,
snapshot, and database score-range methods did not exist. Implementation
commit `13795a6` added those surfaces and the physical visitor, after which the
same command passed.

Clippy then rejected the first 235-line combined test. The test was decomposed
by responsibility instead of suppressing `too_many_lines`. The final focused
matrix covers:

- private inclusive/exclusive/unbounded/inverted bounds and type behavior;
- retained snapshot, current-root, and strict reopen equivalence;
- physical tombstones and live-only offset accounting;
- infinities, negative zero, `NaN`, wrong kinds, and zero limit;
- multilevel physical-bound pruning; and
- fail-closed forged keys, markers, and scores.

The final source commit contains 212 passing native-runtime library tests and
three passing example helper tests.

## Direct-Linux latency observation

The release harness seeds and strictly reopens one 2,048-member, height-two
sorted-set B+tree with 64-byte members. It pins logical CPU 2, performs 10,000
warmups, and records 100,000 complete physical calls per route at concurrency
one.

The exact clean-source command was:

```text
taskset -c 2 cargo run --quiet --release \
  -p hyphae-native-runtime --example sorted_set_smoke \
  > /tmp/hyphae-sorted-set-score-range-linux-raw.json
```

| Operation | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical `ZCARD` | 0.883 us | 0.905 us | 1.427 us | 1.600 us | 6.839 us | 1,120,232 ops/s |
| Physical middle-member `ZSCORE` | 1.895 us | 2.219 us | 2.594 us | 3.476 us | 8.078 us | 517,568 ops/s |
| Physical head-rank `ZRANGE`, 10 members | 18.134 us | 18.524 us | 27.635 us | 31.123 us | 45.493 us | 54,211 ops/s |
| Physical middle-score `ZRANGE_BY_SCORE`, 10 members | 10.215 us | 10.460 us | 19.382 us | 22.672 us | 113.793 us | 95,812 ops/s |

Both range routes return ten members from the same deterministic corpus, but
they do not express the same predicate. Within this one run, direct
middle-score traversal has a 43.67% lower p50, a 29.86% lower p99, and 1.767
times the throughput of the head-rank route. This comparison identifies work
avoided by physical score-bound pruning; it is not a regression threshold.

The raw benchmark stdout has SHA-256
`6372edddd253a935de71c8336241e838ccea56ac20e51191fc434e2b5d9799a5`.
The checked
[metadata-enriched receipt](native-sorted-set-score-range-linux.json) preserves
those exact measurements and has SHA-256
`64e86b012636183e20730711f6f3beb4861f436f8241f996e8b32a5bd42fc6e4`.

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
cargo check --release -p hyphae-native-runtime \
  --example sorted_set_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-sorted-set-score-range-linux.json
git diff --check
```

Results on the source tree plus evidence-only changes:

- complete workspace tests across all targets and features: passed;
- native-runtime tests: 212 library and 3 example helper tests passed;
- complete workspace Clippy with warnings denied: passed;
- formatter and release harness check: passed;
- documentation: 201 Markdown files and 12 JSON examples passed;
- JSON validation and diff check: passed;
- dependency delta: none.

Hosted checks belong to the evidence commit and PR rather than the
pre-evidence source commit. Mutation testing was not executed: this repository
has no accepted mutation tool, operator policy, or surviving-mutant threshold.

## Evidence boundary

This is one warm, concurrency-one, single-core, small-corpus observation. It
does not establish cold behavior, larger/deeper trees, tombstone-heavy ranges,
other member widths or distributions, allocation/RSS, concurrency,
saturation, background interference, hardware counters, local-protocol
transport, fsync latency, process crash, power loss, or p99.9 stability.

The read-only operation adds no new commit or crash-injection boundary.
Existing strict reopen proves format compatibility but does not make this a
durability-latency result.

Reverse traversal, rank lookup and acceleration, sorted-set algebra, TTL,
protocol exposure, randomized model equivalence, memory amplification, the
complete G3 correctness matrix, and the complete G7 benchmark matrix remain
open. This milestone closes none of G0, G3, or G7.
