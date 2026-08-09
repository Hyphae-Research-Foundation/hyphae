# Native bounded hash-scan evidence on Linux

Date: 2026-08-02

Status: first bounded physical hash-field iteration; complete G3, G7,
hosted CI, and mutation testing remain open

Source commit:
`0134fcbfb93875d9978c70555cbe17d7dcbc54e8`

Source tree:
`862c82568ef4bc9287914fcaa59ca25a4f452a2e`

Source branch: `codex/native-hash-field-scan`

Stacked base:
`codex/native-sorted-set-reverse-ranges@0434eb03294f58b8b470e54767d389d75fd417ce`

## Scope

This slice adds bounded `HSCAN` to the native hash family. It returns owned
binary field/value pairs in ascending exact field-byte order. Its optional
`start_after` field is an exclusive resume cursor; `None` starts at the first
field, while an empty byte string remains a real field cursor. A deleted or
otherwise absent cursor still defines the next bytewise position.

Private write batches, retained snapshots, current-root physical reads, and
strict reopen expose the same contract. A zero limit validates the key kind
and hash existence before returning no entries. Missing hashes and other
structure kinds remain distinct typed errors.

The current-root route reads hash metadata, maps the cursor directly into the
`0x03` hash-field B+tree namespace, skips canonical tombstones without charging
the limit, and stops after the requested live result count. A reached malformed
identity, envelope, expiry, blob, or cardinality contradiction fails the
complete call. Complete scans from the first field validate the declared live
cardinality when the requested limit reaches or exceeds it.

No WAL opcode, page format, structure format, catalog format, unsafe Rust, or
dependency changed. Pattern matching, reverse iteration, whole-hash
materialization, multi-field mutation commands, whole-hash deletion/recreate,
and hash TTL remain outside this slice.

## Contract-first red and green

Contract commit `068b77e` froze:

- ascending exact binary field order;
- an exclusive field cursor independent of field liveness;
- zero-limit type and existence validation;
- private, retained, current-root, and reopened equivalence;
- physical prefix pruning, live-only result accounting, and early stop; and
- fail-closed reached physical state.

The first compiler-reaching behavioral command was:

```text
cargo test --locked -p hyphae-native-runtime --lib \
  hash_scan_matches_private_snapshot_latest_and_reopen
```

It failed with one expected `E0432` unresolved result type and fifteen
expected `E0599` missing-method errors. The missing surfaces were the result
entry, private batch, retained snapshot, current-root/reopen API, and the
test-only physical helper.

Implementation commit `ed21f50` added `HashFieldEntry`, one materialized
ordered model route, the three public execution surfaces, and one bounded
physical visitor. The same focused command then passed. Two additional tests
cover zero limit, an empty field cursor, absent cursors, typed errors,
tombstones, a 2,048-field height-two tree, strict reopen, malformed values,
and forged cardinality.

An earlier SSH command used a non-login shell and therefore stopped before
compilation with `cargo: command not found`. Subsequent commands use the
instance's login environment. This transport setup failure is not counted as
the contractual red result.

Clippy rejected one test-only `expect` used for a provably bounded byte
conversion. The conversion now propagates its typed error; no lint
suppression was added.

The final source commit contains 229 passing native-runtime library tests. The
read-only API adds no mutation or crash-injection boundary.

## Direct-Linux latency observation

Benchmark commit `0134fcb` adds an independent release harness. It strictly
seeds and reopens one 2,048-field, height-two hash B+tree with 64-byte fields
and 64-byte values, pins logical CPU 2, performs 1,000 warmups of all three
routes, and records 10,000 complete calls per route at concurrency one.

The exact clean-source command was:

```text
taskset -c 2 cargo run --quiet --release --locked \
  -p hyphae-native-runtime --example hash_scan_smoke \
  > /tmp/hyphae-hash-scan-linux-raw.json
```

| Operation | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical head-ten `HSCAN` | 7.166 us | 7.403 us | 14.194 us | 18.147 us | 50.440 us | 136,173 ops/s |
| Physical middle-ten `HSCAN` | 7.531 us | 7.862 us | 16.288 us | 20.797 us | 32.239 us | 127,073 ops/s |
| Physical tail-ten `HSCAN` | 9.899 us | 10.240 us | 19.045 us | 22.150 us | 46.653 us | 99,074 ops/s |

Within this process and corpus, the middle route differs from the head by
5.093% at p50 and 14.753% at p99. The tail differs by 38.138% at p50 and
34.176% at p99, while remaining below 10 us p50 and 20 us p99. The result is a
position-sensitive observation of bounded physical traversal, not a universal
latency or position-invariance threshold.

The raw benchmark stdout has SHA-256
`ab6b4b8c9e3a4b45cb4242a2b58e8c4b2dba68bce4534cc50b199bb997ac0959`.
The checked [metadata-enriched receipt](native-hash-scan-linux.json) preserves
those exact measurements and has SHA-256
`93f9b269c72b729de123318b736df3b9fee7b3264f89a41e9dc19633d826fe97`.

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
  --example hash_scan_smoke
python3 tools/check_documentation.py
python3 -m json.tool docs/gates/evidence/native-hash-scan-linux.json
git diff --check
```

Results on the source tree plus evidence-only changes:

- complete workspace tests across all targets and features: passed;
- native-runtime tests: 229 library tests passed;
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
benchmarks, other field/value widths or distributions, allocation/RSS,
concurrency, saturation, background interference, hardware counters,
local-protocol transport, fsync latency, process crash, physical power loss,
or p99.9 stability.

This milestone closes bounded hash-field iteration only. Whole-hash
delete/recreate, hash TTL, pattern/reverse scans, multi-field commands, field
counters, set and sorted-set algebra/TTL, subtree-count order statistics,
streams, adaptive expiry backoff, randomized model equivalence, memory
amplification, protocol exposure, the complete G3 correctness suite, and the
complete G7 benchmark matrix remain open.
