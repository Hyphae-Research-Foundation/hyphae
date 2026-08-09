# Native whole-list TTL evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, complete G3, G7,
randomized list-TTL model equivalence, process-kill replay, and physical
power-loss evidence remain open

Source branch: `codex/native-list-ttl`

Stacked base:
`codex/native-list-lifecycle@6d56bb1ba2544754731cd84eba3b634cc2cb4bd3`

Contract commits:
`5a7a84593ea8098aab52a8e406de9d261bc1fb7f` and
`c85d2e9a9490be3525a15bbac7906380c9a7f160`

Runtime implementation commit:
`f491f859fd5b494d955b694018ab753d92108f1c`

Test-hardening commit:
`2310f812671d3578e1b349f1cd3171b015cad404`

Benchmark commit:
`3e888652e9811c18594c7120dc17c4119d67cde8`

Matched-control harness commit:
`601f1311afaa9985ccfa47193f8e33af66365cdc`

Final verified source commit:
`947a423019ae480ef7c5a9874a7cb55b67f0c862`

Final verified source tree:
`c7fe917e4d0e505271b55cd08f84d217dcf8a17f`

## Scope

This slice adds deterministic absolute expiry to a complete native binary
list. It uses the existing Hyphae-owned WAL, MVCC publication, B+tree, buffer
pool, chunked deque, immutable blobs, conflict table, scheduler, compaction,
and page-generation vacuum. It adds no sidecar, compatibility projection,
internal protocol hop, external database, or wall-clock read.

The embedded surfaces are `EXPIRE_LIST`, `TTL_LIST`, and explicit-time
physical `LLEN` and `LRANGE`. A list is visible only while its expiry is
absent or strictly greater than logical time. Equality is due.

Persistent lists retain the exact 32-byte `HYLSTM01` metadata. Expiring lists
use 40-byte `HYLSTM02` metadata with a signed absolute timestamp. The shared
top-level `0x0b` expiry namespace reserves marker `4` for a live list expiry;
markers `0`, `1`, `2`, and `3` retain tombstone, scalar, whole-hash, and
whole-set meanings. WAL opcode `EXPIRE_LIST=36` is additive and existing
`DELETE_LIST=35` remains the single complete-list retirement authority.

Head and tail pushes and pops preserve complete-list expiry. A due list is
missing to list commands. A checked scalar, hash, set, list, or sorted-set
creation may retire the due incarnation and reuse the user key in the same
transaction without resurrecting old chunks or elements.

## Contract-first red and green

The compiler-reaching red gate imported the intended public methods before
they existed:

```text
cargo test -p hyphae-native-runtime \
  --test list_ttl_equivalence --no-run
```

It failed with eight expected `E0599` method-not-found errors for
`ttl_list`, `expire_list`, `llen_latest_list_at`,
`lrange_latest_list_at`, and `ttl_latest_list`. Its SHA-256 is
`ac23787e32feed7958ab5a1d030686ed4c482a1436da68af4ad30d1f7064069b8`.
Implementation proceeded only after that interface failure was captured.

The completed native runtime has 331 passing tests. Ten dedicated list-TTL
tests cover:

- exact private, retained-snapshot, current-root physical, reopened, and
  equality-boundary visibility;
- every existing list command on persistent, expiring, due, empty, and
  multichunk lists;
- expiry preservation through both-end mutation and expiry replacement;
- scalar, hash, set, list, and sorted-set reuse without chunk or element
  resurrection, including scalar `INCR`;
- lifecycle/list-writer first-committer-wins conflicts in both directions;
- Group durability commit and reopen;
- scalar/hash/set/list/hash-field global cleanup ordering under one bound;
- all seven singleton strict-durability cleanup interruption boundaries;
- malformed metadata, chunk identities and gaps, counts, element envelopes,
  blob references, expiry markers, and fail-closed complete-state validation;
  and
- cleanup, compaction, page-generation vacuum, checkpoint/WAL retention, blob
  collection, and reopen without resurrection.

Fifteen WAL codec tests retain existing opcode bytes and add round-trip plus
invalid-shape coverage for opcode 36. The seven-boundary tests are
deterministic native commit interruptions. They are not literal process kill,
EC2 stop, block replay, or physical power-loss evidence.

## Direct-Linux latency observation

The release harness uses a 2,048-element list, 200,000 read observations, 32
operations per timed read sample, 20,000 warmups, and concurrency one.
Mutation and durability surfaces are timed separately.

| Route | p50 | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| Persistent physical `LLEN` | 0.216 us | 0.301 us | 0.398 us | 0.608 us |
| Persistent snapshot `TTL_LIST` | 0.007 us | 0.008 us | 0.008 us | 0.010 us |
| Expiring private-batch `TTL_LIST` | 0.013 us | 0.014 us | 0.015 us | 0.027 us |
| Expiring snapshot `TTL_LIST` | 0.012 us | 0.013 us | 0.024 us | 0.028 us |
| Expiring physical `TTL_LIST` | 0.212 us | 0.338 us | 0.405 us | 0.636 us |
| Expiring physical `LLEN` | 0.216 us | 0.321 us | 0.408 us | 0.640 us |
| Memory `EXPIRE_LIST` commit | 0.681 ms | 0.698 ms | 0.712 ms | 0.712 ms |
| Strict `EXPIRE_LIST` commit | 7.633 ms | 10.216 ms | 10.216 ms | 10.216 ms |
| Memory cleanup, 1 element | 0.337 ms | 0.356 ms | 0.356 ms | 0.356 ms |
| Memory cleanup, 256 elements | 0.401 ms | 0.460 ms | 0.460 ms | 0.460 ms |

Strict commit includes ext4/EBS page and WAL synchronization. Cleanup must
validate and tombstone the live metadata plus complete chunk prefix, so its
cost is cardinality- and payload-sensitive. Neither surface is represented
as a universal microsecond path.

The raw smoke JSON is retained on the canonical Linux host at
`/tmp/hyphae-list-ttl-smoke-authoritative.json`. Its SHA-256 is
`c392ac47835919eee50d88d33181cca4332c7f2ddd8b754ee8c8d07c8365e479`;
the committed harness SHA-256 is
`42298fde0a23a512dc649b33bdbc08becc54038e076b25c578efb3ba514a7d7a`.

## Matched persistent-read control

One unpinned parent/current pair produced nearly equal p50 but an unstable
p95. That single result was rejected as insufficient. The exact committed
harness then ran as five alternating parent/current pairs, pinned to vCPU 2,
over the same 2,048-element corpus and Linux host. The receipt preserves every
sample and raw SHA-256.

| Median metric | Parent `6d56bb1` | Current `601f131` | Change |
|---|---:|---:|---:|
| p50 | 0.217 us | 0.216 us | -0.461% |
| p95 | 0.371 us | 0.372 us | +0.270% |
| p99 | 0.524 us | 0.527 us | +0.573% |
| p99.9 | 0.789 us | 0.782 us | -0.887% |
| Throughput | 4,231,997/s | 4,238,712/s | +0.159% |

The frozen gate was no more than 10% increase at median p50 and p95; it
passed. The control harness SHA-256 is
`63088625fa4930117bfca0684ec7db4a6bbfb43dcc8b8c4329401bd51e684ac6`.

The parent ran from a detached temporary Linux worktree. The exact committed
current harness was copied unchanged into that worktree, and the parent
binary was retained only long enough to execute the alternating pairs.
Temporary worktrees were removed after the checked runs; all source remains
recoverable from Git.

## Matched lifecycle control

The existing list-lifecycle harness also ran unchanged on the stacked parent
and current source. At 2,048 elements:

| Route | p50 change | p95 change |
|---|---:|---:|
| Private `DELETE_LIST` | -1.878% | -22.416% |
| Memory commit | -0.080% | +6.923% |
| Strict commit | +3.774% | -0.295% |

Every 2,048-element p50 and p95 remained inside the 10% increase gate.
The harness SHA-256 is
`aeaa873be656fabd9387d85f82b9723b81a1516729f67f940099872954b346d5`;
parent and current raw JSON SHA-256 values are respectively
`0fd98deb2d2074723e66cbf75e61254c66c4419cfcd9959b96ff01265730ca5f`
and
`f9e59f605e4933a6a5fbf66f44488d79105541c95f0d60d7726c36c3130f842b`.
Lower-cardinality nanosecond-tail variation is retained as observation, not
used to overrule the cardinality-sensitive route.

The machine-readable
[receipt](native-list-ttl-linux.json) contains all operation statistics,
paired controls, raw hashes, and explicit remaining gates.

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
RUSTDOCFLAGS="-D warnings" \
  cargo doc --workspace --all-features --no-deps --locked
cargo check --release --locked -p hyphae-native-runtime \
  --example list_ttl_smoke
cargo check --release --locked -p hyphae-native-runtime \
  --example list_llen_control
python3 tools/check_documentation.py
python3 -m json.tool docs/gates/evidence/native-list-ttl-linux.json
git diff --check
```

The final funnel result is recorded in the machine-readable receipt. No
dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.
The machine-readable receipt SHA-256 is
`f3b07a78d5e4257d3fee91d289ea7174c016dc17a2f5a7cd3c7c6f6a6bf55c8a`.

## Evidence boundary

This receipt proves one deterministic whole-list TTL vertical on direct
Linux: logical absence, typed physical encoding, WAL/replay, Group durability,
lifecycle conflicts, bounded shared cleanup, reopen, injected crash
boundaries, corruption rejection, compaction, page vacuum, blob collection,
and separated warm latency surfaces.

It does not close complete G3, complete G7, local-protocol exposure,
saturation, cold cache, allocation/RSS, hardware counters, randomized
list-TTL model equivalence, relative or conditional expiry, persist,
per-element TTL, blocking operations, indexed insertion, trimming, moving,
element mutation, batched push/pop, streams, process-kill replay, block
replay, or physical power loss.
