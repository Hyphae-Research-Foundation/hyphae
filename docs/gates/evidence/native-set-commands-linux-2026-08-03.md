# Native set member commands evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, unbounded
compatibility commands, destination-set writes, process-kill replay, physical
power-loss evidence, complete G3, and G7 remain open

Source branch: `codex/native-set-commands`

Stacked base:
`codex/native-set-ttl@2214e576beeae2ca63f6462347e4ce48f7688f5a`

Contract commit:
`5359f37357fb9b0aba443aee5cbef02ddf533e63`

Runtime implementation commit:
`babfae4a425bb45943d12466a228fb43536c3bd2`

Test-hardening commit:
`0c7620a0c555d9917576524d0188840cfd7d9c1b`

Benchmark and final verified Rust source commit:
`52999503c0c49b6143fcf8e4f06849234ca903fd`

Final verified Rust source tree:
`72eb6adb660d3a7ed409237b91cbb9a5473396c1`

## Scope

This slice adds bounded native `SADD_MANY`, `SREM_MANY`, `SMISMEMBER`, and
`SSCAN` operations to Hyphae's binary set family. The 4,096-position hard
bound is checked before reads or private mutation. Mutation batches reject
duplicate exact members and sort accepted inputs into canonical byte order.
Membership reads retain caller positions and duplicates. Ascending scans use
an optional exclusive exact-member cursor and never require an unbounded
`SMEMBERS` materialization.

The physical read routes capture one current root, validate set metadata and
logical time, and use exact point lookups or direct set-member B+tree bounds.
They do not reconstruct `StructureState`, route through SQL, call a sidecar,
or introduce a serialized internal protocol. Batch mutations reuse the
existing `ADD_SET_MEMBER` and `DELETE_SET_MEMBER` WAL opcodes, conflict
identities, metadata cardinality, and recovery path. No format or opcode
changed.

The same preflight now protects singleton `SADD`, `SREM`, and `SISMEMBER`.
An oversized member therefore fails before private state changes instead of
reaching physical publication.

## Contract-first red and green

The compiler-reaching red gate imported the complete intended public surface
before implementation:

```text
cargo test -p hyphae-native-runtime \
  set_member_commands_match_private_snapshot_physical_and_reopen --locked
```

It failed with nine expected `E0599` method-not-found errors covering private
batch, retained snapshot, current-root physical, and reopened calls.

The completed native runtime has 302 passing tests. Seven dedicated set
command tests cover:

- private read-your-writes, retained snapshot, current-root physical, and
  reopened equivalence;
- empty input, duplicate positional reads, duplicate mutation rejection,
  4,096-position bounds, oversized identities, zero-limit scans, and failure
  atomicity;
- canonical mutation order and exact added/deleted counts;
- whole-set TTL preservation and visibility at equality;
- disjoint-member rebase, same-member transaction-wide conflict, and
  lifecycle-fence conflict;
- all seven strict singleton commit interruption boundaries, recovering only
  the prior or complete batch state;
- a 2,048-member multilevel tree with a middle tombstone, direct cursor
  pruning, cardinality validation, malformed reached member envelopes, and
  malformed metadata; and
- 96 deterministic randomized transitions against an independent
  `BTreeSet` oracle, including paged scans and duplicated membership probes.

Existing set lifecycle, TTL, algebra, cleanup, compaction, page-vacuum, WAL
shape, and complete-state corruption tests remained green. The deterministic
interruption matrix is not literal process kill, EC2 stop, block replay, or
physical power-loss evidence.

## Direct-Linux latency observation

The release harness uses one 4,096-member set, batches of 32 members,
100,000 read observations after 10,000 warmups, 20,000 scan observations
after 2,000 warmups, and concurrency one.

| Route | p50 | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| Snapshot `SMISMEMBER(32)` | 2.378 us | 2.405 us | 2.487 us | 12.215 us |
| Private `SMISMEMBER(32)` | 2.419 us | 2.486 us | 2.636 us | 12.664 us |
| Physical `SMISMEMBER(32)` | 53.440 us | 64.001 us | 96.102 us | 140.926 us |
| 32 physical singleton `SISMEMBER` | 97.709 us | 110.747 us | 148.744 us | 207.005 us |
| Physical head `SSCAN(16)` | 15.066 us | 20.840 us | 29.553 us | 44.781 us |
| Physical middle `SSCAN(16)` | 16.960 us | 18.811 us | 30.705 us | 55.114 us |
| Physical tail `SSCAN(16)` | 17.602 us | 18.827 us | 30.566 us | 47.902 us |
| Private `SADD_MANY(32)` preparation | 7.544 us | 8.508 us | 24.237 us | 27.420 us |
| Private `SREM_MANY(32)` preparation | 9.413 us | 10.344 us | 23.762 us | 29.293 us |
| Memory `SADD_MANY(32)` commit | 11.177 ms | 12.731 ms | 12.899 ms | 12.899 ms |
| Strict `SADD_MANY(32)` commit | 22.210 ms | 23.836 ms | 23.836 ms | 23.836 ms |
| Memory `SREM_MANY(32)` commit | 11.675 ms | 12.127 ms | 12.159 ms | 12.159 ms |

One of 16 strict observations reached 5.485 seconds on the shared EBS-backed
host. It is retained in the raw receipt rather than discarded. The strict
throughput aggregate is therefore not a stable capacity claim; p50 through
p99.9 and the maximum are reported separately.

Physical `SMISMEMBER(32)` reduced p50 by 45.307% and p95 by 42.210% against
32 complete singleton calls over the same probes. Tail scan p50 was only
16.833% above head scan p50 despite starting after member 4,079, consistent
with direct B+tree cursor pruning rather than a prefix rescan. These are warm
observations, not universal latency guarantees.

The raw smoke JSON remains on the canonical Linux host at
`/tmp/hyphae-set-commands-smoke-authoritative.json`. Its SHA-256 is
`8f5a2973c60480c42251d9b291811fcc9582a89b43123598998a3b103c0dbb90`.

## Matched persistent-read control

The exact same committed control harness and 2,048-member corpus ran on the
stacked parent and current source in isolated direct-Linux worktrees:

| Metric | Parent `2214e57` | Current `5299950` | Change |
|---|---:|---:|---:|
| p50 | 1.968 us | 1.968 us | 0.000% |
| p95 | 2.242 us | 2.241 us | -0.045% |
| p99 | 2.285 us | 2.290 us | +0.219% |
| p99.9 | 3.721 us | 3.862 us | +3.789% |
| Throughput | 501,672/s | 501,556/s | -0.023% |

The frozen gate was no more than a 10% increase at p50 and p95; it passed.
The unchanged control harness SHA-256 is
`edf542fdf0ad1727bd379f0f19214d644d714ca5883b3479f09524732c7729da`.
Parent and current raw JSON SHA-256 values are respectively
`bcca8096a872abe3c680dbe7a75d84787f40a1695790f6171469d44830571a0b`
and
`2c1087c69770f28afd8e8850156e55316ea724bb44b04d025488eba5fa60450a`.

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
cargo test -p hyphae-native-runtime --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check -p hyphae-native-runtime --example set_commands_smoke --locked
python3 tools/check_documentation.py
python3 -m json.tool docs/gates/evidence/native-set-commands-linux.json
git diff --check
```

The runtime reported 302 passing tests and the workspace gate passed across
all targets and features. The benchmark harness SHA-256 is
`1afeadf71a905fd6b501a740749e610d674c3cad1764d66d357719131b400041`.
The machine-readable receipt SHA-256 is
`d8e62290cef0c75fba244c493dcf26832cc5447c803fb266563d27b0900ce8bc`.
No dependency, unsafe-Rust allowance, external runtime, network protocol,
storage format, or WAL opcode changed.

## Open boundaries

This receipt does not claim `SMEMBERS`, pattern scans, reverse scans, random
member selection, member moves, destination-set algebra, per-member TTL,
protocol compatibility, hosted CI, saturation, cold-cache behavior, RSS,
hardware counters, memory amplification, complete G3, or G7.
