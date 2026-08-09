# Native reverse hash scan evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, pattern hash scans,
field TTL, local-protocol exposure, randomized model equivalence, complete G3,
and G7 remain open

Source branch: `codex/native-hash-reverse-scan`

Stacked base:
`codex/native-hash-field-commands@dccf9336cacb30af33cbfbe20fbc13c80f346f19`

Contract commit:
`8d25ed673cb22c9c80e589ef6cd9db49edf29f42`

Runtime commit:
`fc708223ec4f6435ad1fde6644bdd35dd522e9b5`

Benchmark commit:
`de54334b524ba6d7aeafddc0e8e4fda302849f17`

Benchmark tree:
`09cf3210dc621a6f6f11b4ab243378ec7cdeb7a2`

## Scope

This slice adds `HSCAN_REVERSE(key, start_before?, limit)` to the embedded
native structure engine. Private batches and retained snapshots iterate their
materialized `BTreeMap` in descending exact-byte order. Current-root physical
calls map the optional exclusive field cursor into an upper B+tree bound and
invoke Hyphae's native cached reverse prefix-range visitor.

The physical route captures one committed root set, validates typed hash
metadata and whole-family visibility once, skips canonical tombstones without
charging the output limit, and stops immediately after the requested live
count. It does not run an ascending scan, reverse a complete materialization,
or continue into lower pages after satisfying the limit.

`None` starts at the greatest live field. A live or dead cursor is exclusive;
`Some(empty)` returns no fields after validation. Zero validates metadata,
kind, visibility, and cursor identity. A complete unbounded-from-above scan
whose limit covers declared cardinality checks the observed live count against
metadata.

This is a read-only surface. It adds no WAL opcode, mutation, physical format,
dependency, sidecar, or internal protocol.

## Contract-first red and green

The compiler-reaching model gate was:

```text
cargo test -p hyphae-native-runtime \
  model::tests::reverse_hash_scan_is_descending_and_cursor_exclusive --no-run
```

It failed with three expected `E0599` errors for missing `hscan_reverse` and
`hscan_reverse_at` methods. The model test then passed before the public and
physical routes were implemented.

The completed native-runtime suite has 253 passing tests. New coverage
includes:

- descending exact-byte order across empty, ASCII, prefix-related, and binary
  fields;
- absent, live, deleted, and non-live cursors, including `Some(empty)`;
- zero, one, exact-cardinality, and over-cardinality limits;
- private-batch, retained-snapshot, current-root, explicit-time, and reopened
  equivalence;
- whole-hash TTL visibility immediately before, at, and after expiry;
- a height-two structure tree with a top-cohort tombstone and bounded early
  stop;
- proof that corruption below the satisfied limit is not visited, while a
  complete traversal reaches it and fails closed;
- reached malformed metadata and field envelopes plus a corrupted blob
  reference; and
- oversized cursor identity rejection on private, snapshot, and physical
  surfaces.

The corruption tests use forged in-memory roots and do not publish invalid
state.

## Direct-Linux latency observation

The exact committed release harness uses one reopened 2,048-field,
height-two hash with 64-byte fields and values. Every route uses 10,000
observations, 1,000 warmups, 32 returned fields, and concurrency one.

| Route | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Native reverse, greatest 32 | 11.514 us | 11.778 us | 21.259 us | 27.789 us | 84,792/s |
| Native reverse, middle 32 | 12.488 us | 12.742 us | 21.677 us | 23.772 us | 78,761/s |
| Ascending 2,048 + reverse + truncate 32 | 390.204 us | 408.188 us | 423.701 us | 621.503 us | 2,533/s |

Against the explicit full-materialization fallback, the native greatest-page
route reduced p50 by 97.049%, p95 by 97.115%, and p99 by 94.983%.
Call throughput increased by 3,247.796%. This comparison demonstrates bounded
physical pruning; it is not a universal latency SLO or a cold-cache result.

The benchmark harness SHA-256 is
`08d26e166a9e9190187e43ec3803fc108470dcc2a8bd2a0ecf2d143db6f75aa1`.
The raw benchmark output SHA-256 is
`d2b16efa7936e46290fdc87b79fbac6f90870997c3b0ebf632ea6445df787851`.

## Matched persistent-read control

The unchanged HGET control harness had SHA-256
`8d03592813a5607be4ff1f63c7fced8c059a59a37ebd524c4098f1b9ed05b0eb`
on both the stacked parent and benchmark commits.

An initial unpinned parent-then-current pass showed an apparent 6.501% p50 and
7.601% p95 increase. That observation triggered controlled repetition rather
than being hidden. Two alternating candidate/parent rounds were then pinned
to CPU 2:

| Round | Commit | p50 | p95 | p99 | p99.9 | Throughput |
|---|---|---:|---:|---:|---:|---:|
| 2 | Current | 1.372 us | 1.603 us | 1.692 us | 2.615 us | 718,557/s |
| 2 | Parent | 1.369 us | 1.602 us | 1.690 us | 2.496 us | 719,998/s |
| 3 | Current | 1.371 us | 1.591 us | 1.684 us | 2.186 us | 719,528/s |
| 3 | Parent | 1.373 us | 1.600 us | 1.687 us | 2.613 us | 718,086/s |

The two-round medians changed by +0.036% at p50, -0.250% at p95, -0.030% at
p99, -6.029% at p99.9, and effectively 0.000% in throughput. The frozen gate
was no more than 10% increase at p50 and p95; it passed under the repeated
pinned comparison.

The parent ran from a detached temporary Linux worktree with an isolated Cargo
target. Its exact path and commit were verified before removal. Source remains
recoverable from Git.

The checked machine-readable
[receipt](native-hash-reverse-scan-linux.json) contains the benchmark,
unpinned diagnostic, and both pinned rounds. Its SHA-256 is
`df6de9cace3420dd2dbfcfbe8f7ace711698cdf7c232bdb31f3d4545073ea521`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust `1.96.0`, release profile for measurements; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host:

```text
cargo fmt --all -- --check
cargo test -p hyphae-native-runtime --lib --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_reverse_scan_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-reverse-scan-linux.json
git diff --check
```

The runtime suite reported 253 passing tests and every command above passed
against benchmark commit `de54334` plus the evidence-only changes. No
dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.

## Evidence boundary

This receipt proves bounded native descending hash-field iteration across the
logical model, private batches, retained snapshots, physical B+tree execution,
TTL visibility, reopen, tombstone skipping, early stop, and reached
corruption, with warm direct-Linux performance separated from a deliberately
unbounded fallback.

It does not close pattern/glob scans, field TTL, local-protocol exposure,
randomized model equivalence, saturation, cold cache, allocation/RSS,
hardware counters, memory amplification, physical power loss, complete G3, or
G7.
