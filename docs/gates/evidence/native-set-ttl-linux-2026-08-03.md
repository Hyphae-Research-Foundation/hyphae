# Native whole-set TTL evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, complete G3, G7,
randomized set-TTL model equivalence, process-kill replay, and physical
power-loss evidence remain open

Source branch: `codex/native-set-ttl`

Stacked base:
`codex/native-set-algebra@ba3f67e3898ce1a7b79c3ed0caf8fd8397fa656d`

Contract commit:
`f0cf410663a89c5df1f0356b70347429cd969bfc`

Runtime implementation commit:
`fc6fe8e18a19f82e432f5d95ab0a1261a1040f4f`

Benchmark commit:
`5f72511edbf3cd60e9f54db9e579420c48d2ca2e`

Matched-control harness and final verified source commit:
`d4fbc5ade73f7c80b2d770f0a3ce759e66818bb2`

Final verified source tree:
`41f74cb2d15f6f622ec41b017c620a09849abc8e`

## Scope

This slice adds deterministic absolute expiry to a complete native binary set.
It uses the existing Hyphae-owned WAL, MVCC publication, B+tree, buffer pool,
conflict table, scheduler, compaction, and page-generation vacuum. It adds no
sidecar, compatibility projection, internal protocol hop, external database,
or wall-clock read.

The embedded surfaces are `EXPIRE_SET`, `TTL_SET`, explicit-time physical
`SISMEMBER` and `SCARD`, and the existing bounded union, intersection, and
ordered difference operations. A set is visible only while its expiry is
absent or strictly greater than logical time. Equality is due.

Persistent sets retain the exact 16-byte `HYSETM01` metadata. Expiring sets
use 24-byte `HYSETM02` metadata with a signed absolute timestamp. The shared
top-level `0x0b` expiry namespace reserves marker `3` for a live set expiry;
markers `0`, `1`, and `2` retain tombstone, scalar, and whole-hash meanings.
WAL opcodes `EXPIRE_SET=33` and internal `DELETE_SET=34` are additive.

Member additions and removals preserve complete-set expiry. A due set is
missing to set commands and mathematical empty input to read-only set algebra.
A checked scalar, hash, set, list, or sorted-set creation may retire the due
incarnation and reuse the user key in the same transaction without
resurrecting old members.

## Contract-first red and green

The compiler-reaching red gate imported the intended public methods before
they existed:

```text
cargo test -p hyphae-native-runtime set_ttl_equivalence --no-run
```

It failed with eight expected `E0599` method-not-found errors for
`ttl_set`, `expire_set`, `sismember_latest_set_at`,
`scard_latest_set_at`, and `ttl_latest_set`. Implementation proceeded only
after that interface failure was captured.

The completed native runtime has 295 passing tests. Eleven dedicated set-TTL
tests cover:

- exact private, retained-snapshot, current-root physical, and reopened
  visibility;
- membership, cardinality, union, intersection, and ordered difference at the
  expiry boundary;
- member mutation with expiry preservation and expiry replacement;
- scalar, hash, set, list, and sorted-set reuse without resurrection,
  including scalar `INCR`;
- lifecycle/member first-committer-wins and admitted-member rebase;
- group-durability commit and reopen;
- scalar/hash/set/hash-field global cleanup ordering under one bound;
- all seven singleton strict-durability cleanup interruption boundaries;
- malformed metadata, count, member envelopes, orphan/stale/wrong-kind expiry
  markers, and fail-closed complete-state validation; and
- cleanup, compaction, page-generation vacuum, and reopen without
  resurrection.

The seven-boundary tests are deterministic native commit interruptions. They
are not literal process kill, EC2 stop, block replay, or physical power-loss
evidence.

## Direct-Linux latency observation

The release harness uses a 2,048-member set, 200,000 read observations, 32
operations per timed read sample, 20,000 warmups, and concurrency one.
Mutation and durability surfaces are timed separately.

| Route | p50 | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| Persistent physical `SISMEMBER` | 1.904 us | 2.182 us | 2.213 us | 2.757 us |
| Persistent snapshot `TTL_SET` | 0.007 us | 0.008 us | 0.008 us | 0.012 us |
| Expiring private-batch `TTL_SET` | 0.012 us | 0.013 us | 0.014 us | 0.042 us |
| Expiring snapshot `TTL_SET` | 0.012 us | 0.013 us | 0.013 us | 0.014 us |
| Expiring physical `TTL_SET` | 0.873 us | 0.880 us | 1.155 us | 1.296 us |
| Expiring physical `SISMEMBER` | 1.902 us | 2.176 us | 2.203 us | 2.568 us |
| Memory `EXPIRE_SET` commit | 1.047 ms | 1.070 ms | 1.087 ms | 1.087 ms |
| Strict `EXPIRE_SET` commit | 8.272 ms | 10.317 ms | 10.317 ms | 10.317 ms |
| Memory cleanup, 1 member | 0.403 ms | 0.444 ms | 0.444 ms | 0.444 ms |
| Memory cleanup, 256 members | 0.602 ms | 0.666 ms | 0.666 ms | 0.666 ms |

Strict commit includes ext4/EBS page and WAL synchronization. Cleanup must
validate and tombstone the complete live member prefix, so its cost is
cardinality-sensitive. Neither surface is represented as a universal
microsecond path.

The raw smoke JSON is retained on the canonical Linux host at
`/tmp/hyphae-set-ttl-smoke-authoritative.json`. Its SHA-256 is
`9d64f17e7d80ac9a068669186645abc4908a9a13accf1a7e267dfbadf8447fcc`.

## Matched persistent-read control

The exact same committed harness and 2,048-member corpus ran on the stacked
parent and current source on the same Linux host:

| Metric | Parent `ba3f67e` | Current `d4fbc5a` | Change |
|---|---:|---:|---:|
| p50 | 1.889 us | 1.882 us | -0.371% |
| p95 | 2.159 us | 2.157 us | -0.093% |
| p99 | 2.197 us | 2.182 us | -0.683% |
| p99.9 | 3.376 us | 2.770 us | -17.950% |
| Throughput | 522,190/s | 524,594/s | +0.460% |

The frozen gate was no more than 10% increase at p50 and p95; it passed. The
control harness SHA-256 is
`edf542fdf0ad1727bd379f0f19214d644d714ca5883b3479f09524732c7729da`.
Parent and current raw JSON SHA-256 values are respectively
`2138ccb2c44aa7bb33b5847b9453c86292cec1a28c7f82720b3c78e6144766a5`
and
`1cf136fc8be7f1dcfbbca9de89a5f54d1f4aafc1b8bde15b7f41cbfe4ce33221`.

The parent ran from a detached temporary Linux worktree with an isolated Cargo
target. The exact committed current harness was copied unchanged into that
worktree. Both temporary directories were removed after the checked run; all
source remains recoverable from Git.

The machine-readable
[receipt](native-set-ttl-linux.json) contains the raw operation and matched
control statistics.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- pinned repository toolchain Rust `1.96.0`, release profile for
  measurements; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host:

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example set_ttl_smoke
cargo check --release --locked -p hyphae-native-runtime \
  --example set_sismember_control
python3 tools/check_documentation.py
python3 -m json.tool docs/gates/evidence/native-set-ttl-linux.json
git diff --check
```

Every command passed against source commit `d4fbc5a`; the native runtime
reported 295 passing tests. The machine-readable receipt SHA-256 is
`d7c074e5d710f809269211d833d1f6cdde547904156c2c0e2a3f634006e8eff7`.
No dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.

## Evidence boundary

This receipt proves one deterministic whole-set TTL vertical on direct Linux:
logical absence, typed physical encoding, WAL/replay, group durability,
lifecycle conflicts, bounded shared cleanup, reopen, injected crash
boundaries, corruption rejection, compaction, page vacuum, and separated warm
latency surfaces.

It does not close complete G3, complete G7, local-protocol exposure,
saturation, cold cache, allocation/RSS, hardware counters, randomized set-TTL
model equivalence, relative or conditional expiry, persist, per-member TTL,
destination-set algebra, sorted-set TTL/algebra, streams, process-kill replay,
block replay, or physical power loss.
