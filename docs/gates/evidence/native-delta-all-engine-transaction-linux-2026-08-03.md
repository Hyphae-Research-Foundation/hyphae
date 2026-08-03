# Native delta all-engine transaction evidence

Date: 2026-08-03

Status: bounded delta transaction gates passed; G0 through G8 remain open

Source branch: `codex/native-delta-all-engine-transaction`

Stack base: `codex/native-local-all-engine-transaction`

Contract commit: `7c7065481b89ea8977e5903a31d44af974a2c8b5`

Measured runtime commit:
`28a7af7cabb8febb7f655f9bb5a4b508cf891729`

Test-gate hardening commit:
`619da042afa567a9bcd49620187b069112d99d7d`

Scaling harness commit:
`acc137a64d12add3aaa5b37562ceaefeb8db7b9d`

UDS harness commit: `dd10289fea97ce160fbfd435bdbc819ebdcb7c50`

The checked
[machine-readable receipt](native-delta-all-engine-transaction-linux.json)
contains the three exact UDS runs, the three exact scaling runs, environment
and artifact identities, the allocation observation, and the symbolized
profile summary. Its SHA-256 before repository insertion is
`18f7caff20628e1f26706f2ac8de74f7e035176e1da20e175c39898b072feb7c`.

## Implemented mechanism

The local all-engine transaction now stages a detached physical delta instead
of materializing `MaterializedState`.

- `BEGIN` captures one immutable snapshot, logical time, durability class,
  and empty bounded overlays.
- SQL DML parses once and point-loads the named relation, its exact HYCAT004
  dependency set, the addressed primary row, and required uniqueness probes.
- scalar `SET` resolves one typed structure identity;
- immutable lexical indexing resolves one collection and one document
  identity;
- later operations resolve against the private overlay before the immutable
  roots;
- commit revalidates only the staged identities, applies copy-on-write root
  deltas, emits the existing canonical WAL transaction, and publishes one
  root set under one CSN; and
- legacy HYCAT003 roots retain a fail-closed full-catalog fallback, while the
  next catalog mutation rebuilds one immutable HYCAT004 root.

There is no sidecar, compatibility database, internal TCP/HTTP/JSON route, or
second commit coordinator.

## Red and deterministic gates

The compiler-reaching red log has SHA-256
`5c96e4953dd5c05f9082824130abae55ae429c2cbbe987020f0143151a1d14c3`.
It failed before the public delta API existed.

The final directed suites prove:

- exact HYCAT004 golden keys plus missing, nonempty, extra, and legacy
  dependency behavior;
- one transaction-private SQL insert/update sequence, structure overwrite,
  and lexical duplicate detection against prior staged work;
- semantic failure retains earlier operations and the next one-based ordinal;
- latest SQL update visits one version head;
- a retained historical root visits only its first visible version;
- hot head decode stops before an unreachable corrupt older version while
  full verification rejects the same chain;
- hot `BEGIN`, stage, and commit fail immediately if either complete engine
  state or complete catalog state is loaded;
- 256 unrelated relations cause zero full-state and zero full-catalog loads;
- one transaction ID, one CSN, prior-snapshot invisibility, reopen,
  first-committer-wins conflict, rollback, close, and peer-loss behavior; and
- all seven injected commit interruption boundaries expose prior or complete
  state, never a mixed three-engine state.

The targeted delta suite passed 5 tests, the local transaction suite passed
22 tests, and the complete native-runtime unit suite passed 339 tests.

An initial hosted run exposed a test-only race: the hot-path test compared a
process-global diagnostic counter while unrelated tests incremented it in
parallel. Linux MSRV, macOS, and Windows observed increments of four, five,
and three respectively. Commit `619da04` replaced that comparison with
thread-local fail guards for both full engine state and full catalog state.
The complete 339-test parallel unit suite then passed on the canonical host.
No product path or measured runtime changed.

## Direct-Linux environment

- canonical host `mario@10.77.10.10`;
- canonical repository `/home/mario/celiumsai/hyphae`;
- AWS EC2 `m6i.2xlarge`, Ubuntu 24.04.4 LTS;
- kernel `6.17.0-1019-aws`, KVM;
- Intel Xeon Platinum 8375C @ 2.90 GHz, 8 logical CPUs;
- 30 GiB RAM, no swap;
- ext4 on `/dev/nvme0n1p1`;
- Rust and Cargo 1.96.0, `x86_64-unknown-linux-gnu`;
- `perf` 6.17.13 and `heaptrack` 1.5.0; and
- release observations pinned to logical CPU 0 at concurrency one.

The environment receipt SHA-256 is
`9c2c3bf4ff93356fd0e05051749334c6117d82f6c4b0e400a72f86fba26715e1`.
WSL is not in the edit, build, test, benchmark, Git, or evidence path.

## UDS latency observations

The existing local all-engine harness measures one persistent UDS
connection, warm state, one client, one server, and concurrency one. PING,
each stage, memory commit, and strict commit remain independent
distributions; no percentile is subtracted from another.

Release harness source SHA-256:
`d6cacc86bacda87041b33e7118236fbdf20a7d6a7cc4b3c9162f0a9e9dd103d7`.

Release binary SHA-256:
`fabe00c46d1a0419a0fdab664cedd44ba12270e3f013c1cdf435600726fa8715`.

### P50 by run

| Surface | Run 1 | Run 2 | Run 3 | Median statistic |
|---|---:|---:|---:|---:|
| PING 32 B | 5.625 us | 5.713 us | 5.497 us | 5.625 us |
| SQL stage | 12.252 us | 12.439 us | 12.607 us | 12.439 us |
| Structure stage | 7.267 us | 6.929 us | 6.916 us | 6.929 us |
| Search stage | 7.959 us | 7.873 us | 7.870 us | 7.873 us |
| Memory commit | 1.001241 ms | 1.002084 ms | 1.001155 ms | 1.001241 ms |
| Strict commit | 9.370109 ms | 9.388397 ms | 9.390991 ms | 9.388397 ms |

Raw UDS JSON SHA-256:

- run 1:
  `49f9ced524f34d32f6bbf3fe218f1a7b97ebb6efbc7b25224b550aa79bef2809`;
- run 2:
  `f2d0cd487753f1ce2eede009fc8693dc6c5f5ec8699eebf0053c1b22b56e2ad2`;
  and
- run 3:
  `5653398ce18c117032e7684d6fd0b9dca17b77030df5fded7e4ba2b8bb6d033f`.

These current single-CPU observations must not be causally compared with the
sealed baseline whose client/server affinity differed. The current
distributions show that all three staging surfaces remain in the microsecond
domain. Memory and strict commit remain outside that domain.

## Version-depth and unrelated-population scaling

The release scaling harness source SHA-256 is
`3050c4f4437cde4778bc629dcd755f2c1d55e8f8a2fa31f072f972b515eebefe`.
The release binary SHA-256 is
`bd553d39efc308157edd99e9f670c5a51357e2aed8951e97d436efd812f83651`.

Each run is pinned to CPU 0. The depth sweep measures 32 stable-row updates
at each prior-version depth. The population sweep measures 32 transactions
that touch one stable SQL row, one stable structure key, and one new lexical
document while unrelated items per engine grow.

### Median p50 across three runs: prior versions

| Prior versions | BEGIN | SQL stage | Commit | Total | Page reads | Page appends | WAL bytes | Full state | Full catalog |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 84 ns | 44.493 us | 211.250 us | 257.708 us | 9 | 3 | 65,536 | 0 | 0 |
| 32 | 87 ns | 45.802 us | 210.607 us | 259.446 us | 9 | 3 | 65,536 | 0 | 0 |
| 256 | 81 ns | 45.359 us | 209.401 us | 257.765 us | 9 | 3 | 65,536 | 0 | 0 |
| 1,024 | 83 ns | 45.465 us | 211.255 us | 258.369 us | 9 | 3 | 65,536 | 0 | 0 |

Depth does not change median physical work: one point update remains nine
page reads, three appended pages, one 65,536-byte WAL block, and no complete
state or catalog load.

### Median p50 across three runs: unrelated population

| Unrelated items per engine | BEGIN | SQL stage | Structure stage | Search stage | Commit | Total | Page reads | Page appends | Full state | Full catalog |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 137 ns | 49.667 us | 19.111 us | 15.748 us | 820.760 us | 904.657 us | 35 | 16 | 0 | 0 |
| 256 | 154 ns | 46.367 us | 16.051 us | 31.920 us | 1.299303 ms | 1.397271 ms | 54 | 28 | 0 | 0 |
| 4,096 | 152 ns | 67.232 us | 38.517 us | 26.776 us | 1.515734 ms | 1.651544 ms | 65 | 30 | 0 | 0 |

Every population point appends one 65,536-byte WAL block. Population growth
increases B+tree height and copy-on-write pages, which the contract permits.
It does not introduce a complete engine-state or catalog scan.

Raw scaling JSON SHA-256:

- run 1:
  `2fec539bb612871e2a6f5b586eb470501933a59ee745389d8e47189da8a90a75`;
- run 2:
  `16f0bfdeec218fa8098cf74227b8b67c2558a3f872957135147535995bf1a85a`;
  and
- run 3:
  `24a82f1f75b6d938ef50fc18f513d6a60c7b20927032d9a32ada1e1ec592edc2`.

## Allocation observation

Unsafe Rust remains forbidden, so the evidence does not install an in-process
counting allocator. `heaptrack` 1.5.0 instead instruments the unchanged
stripped release binary while the complete scaling harness runs pinned to
CPU 0.

- calls to allocation functions: 53,923,701;
- temporary allocations: 2,635,915;
- peak heap: 11.52 MiB as reported by `heaptrack`;
- peak RSS including profiler overhead: 18.26 MiB; and
- leaked at process exit: 544 bytes.

This is a whole-process counter including database setup and all measured
sweeps. It is not a per-operation allocation claim, and profiler-instrumented
latencies are excluded from the latency tables.

Allocation capture SHA-256:
`066ce9f0925414aa451585d074b40a1236b4ce2327ecd190eb43d4a257703b3b`.

Analyzed summary SHA-256:
`4e4793c12c753e1ffc0efbfd23a95afbc6da5875cfb8d7fc7a759ca4fb318ebb`.

## Symbolized CPU profile

The diagnostic profile uses the same release optimization with debug
information and `strip=none` in a separate target directory. The profile
binary SHA-256 is
`2bc905a16a184043ef2182c411d1106ff6aa4e9e80b2e06b0ed650b6b291a441`.

One five-second CPU-0 capture over the depth sweep recorded 4,894 samples and
zero lost samples. Leading self costs were:

- `hyphae_native_pages::Page::decode`: 32.75%;
- BLAKE3 AVX-512 batch hashing: 16.22%;
- CRC32C parallel hashing: 10.18%;
- CRC32C append hashing: 5.62%; and
- `NativeDatabase::stage_delta_sql_dml`: 0.16%.

No complete catalog-load or catalog-scan symbol crossed the 0.10% report
threshold. This profile is diagnostic attribution, not a latency
subtraction.

`perf.data` SHA-256:
`881d3c380d9a24c1840e61283da3bb1b9b6770f854bfe6353dd510ea30b1bff2`.

Text report SHA-256:
`ca05c10e6a3222c4157ca16cbb5d035c448bf16684e13b4c7e5c5f39fc1b3cc4`.

## Final validation

The final direct-Linux funnel runs:

- `cargo fmt --all -- --check`;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace test log contains 84 result blocks: 697 passed, zero failed,
and one ignored. Documentation validation covers 245 Markdown files and 12
JSON examples.

Validation-log SHA-256:

- formatting:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
- workspace tests:
  `53c1bc8b248f66e8d10dab689b2c5c6578c248d476f6b8391e64f3edabfc4997`;
- Clippy:
  `f48a3e75c03cd93e64a911a44bc7aaea3a3a4c081c61f43d9436b1b208bebb7c`;
- Rustdoc:
  `2219fd09845721a4c57e93c116e3641e0a705b97b7b66e9a63254bdda0eef0ff`;
- documentation:
  `9c13e4b996060e3e4b444fdff10579d327b6d22e097f7beb84a51809aba246ec`;
- machine-readable receipt:
  `664c5467ce6d9f0058a16eb22b67558c3e2104fe7f39ec8268bd90bf39afaa41`;
  and
- diff check:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

## Boundary

This evidence closes the bounded delta slice, not native phase 1 and not G7.
It does not prove cold state, saturation, concurrent clients, background
interference, per-operation allocation bounds, stable dedicated hardware,
hardware counters, lexical replacement/delete, prepared transactional DML,
transaction-private query reads, savepoints, group durability, Windows named
pipes, complete SQL, backup, restore, replication, clustering, multitenancy,
TLS, encryption at rest, SaaS, billing, roles, or an LLM.

The measured path remains one Hyphae-owned binary, one data directory, one
catalog, one WAL transaction, one commit coordinator, and one published CSN
across relational, structure, and lexical state.
