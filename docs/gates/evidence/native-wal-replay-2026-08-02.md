# Native bounded-WAL-replay evidence

Date: 2026-08-02

Status: current-root retention and bounded replay implementation evidence; G1
and G7 remain open

## Source identity

- measured implementation commit:
  `a74e35523977f319d29cfc5c9688b14793ebbbcb`;
- measured implementation tree:
  `271e65606b6369ee2e8d6ed530c02fd1632c34c8`;
- contract commit: `5d56e6f`;
- retention-anchor codec commit: `d3e63e5`;
- retention-store commit: `d346498`;
- explicit transition-state commit: `60374e1`;
- bounded-replay implementation commit: `5e57c28`;
- recovery-instrumentation commit: `c1a9ffa`; and
- Rust: `rustc 1.96.0 (ac68faa20 2026-05-25)`.

The evidence documents and raw observations are intentionally committed after
the measured benchmark commit.

## Red and green evidence

Acceptance work preceded implementation in four slices. The initial WAL tests
failed because `WalRetentionAnchor`, its fields, and `WalFile::open_after`
did not exist. The persistence slice then failed on unresolved
`WalRetentionStore` and `WalFile::reset_after`; the transition slice failed on
the missing pending/stable publication API; and the runtime acceptance failed
on the missing truncation method and recovery-report fields.

The focused lanes are now green:

```text
hyphae-native-wal: 10 passed; 0 failed
hyphae-native-runtime: 157 passed; 0 failed
```

No new `allow`, `unsafe`, `unwrap`, `expect`, `panic`, or `unreachable`
occurrence was admitted in the changed implementation or benchmark.

## Implemented behavior

- `HYWAR001` is an exact 256-byte, CRC32C- and BLAKE3-bound retention anchor.
- The anchor binds the retired checkpoint, root-manifest identity, prior block
  digest, next transaction ID, cumulative checkpoint count, and cumulative
  committed-transaction count.
- Retained blocks preserve their original absolute sequence, record LSNs,
  previous digest, and bytes. The implementation does not renumber history.
- Anchor publication uses explicit create-new `.tmp`, destructive
  `.hywa.pending`, and immutable `.hywa` states.
- Truncation is eligible only at the latest synchronized current-root
  checkpoint whose visible CSN equals the physical retention floor.
- Open reconstructs the fixed base root from the bound immutable manifest,
  verifies the retained suffix from the anchor's absolute sequence and digest,
  and semantically replays only suffix transactions.
- Point-write conflict reconstruction starts at the retained base. Cumulative
  committed/checkpoint counts, transaction IDs, CSNs, and checkpoint
  continuity remain absolute.
- Retrying after recovered publication is idempotent.
- A stable anchor never masks complete anchor or suffix corruption. Complete
  sequence, digest, framing, or content divergence fails closed.

## Deterministic interruption and failure evidence

The runtime test matrix interrupts at all six publication boundaries:

1. `AnchorStaged`;
2. `AnchorPending`;
3. `WalReset`;
4. `WalSynchronized`;
5. `AnchorStabilized`; and
6. `PriorAnchorRemoved`.

Every interrupted directory reopens to the same complete relational,
structure, lexical-search, and ANN base, then accepts an idempotent retention
retry. Separate tests reject a checkpoint newer than the retention floor and
prove that neither a corrupt stable anchor nor a corrupt retained suffix can
fall back to an older prefix.

The primary vertical also appends a retained suffix checkpoint and verifies
absolute transaction-ID and checkpoint continuity after reopen.

## Benchmark method

The executable observation is:

```text
cargo run -p hyphae-native-runtime \
  --example wal_replay_benchmark \
  --locked --release -- \
  <source-commit> <source-tree> "<rustc-version>"
```

Two independent directories reach the same final logical state:

- one retains the complete WAL;
- one vacuums, checkpoints, retires the prefix, and retains only the suffix;
- both contain 402 commits before the retention floor and four suffix commits,
  so the retired-prefix-to-suffix ratio is 100.5 to one;
- every suffix commit mutates the relational row, scalar key, and lexical
  index;
- both are reopened 25 times; and
- every reopen verifies the exact latest relational value, structure value,
  and lexical document identity.

The first reopen is reported separately. Percentiles use the remaining 24 warm
reopens.

## Windows observation

Environment:

- Microsoft Windows NT `10.0.26200.0`;
- x86-64; and
- repository and temporary benchmark directories on NTFS.

Raw observation:
[native-wal-replay-windows.json](native-wal-replay-windows.json).

| Observation | Full history | Retained suffix |
|---|---:|---:|
| WAL bytes | 26,804,224 | 327,680 |
| verified WAL blocks | 409 | 5 |
| replayed transactions | 406 | 4 |
| first external reopen | 42.641 ms | 2.637 ms |
| warm external p50 | 27.714 ms | 2.085 ms |
| warm external p95 | 31.501 ms | 3.081 ms |
| warm external p99 | 36.829 ms | 3.122 ms |
| warm internal p50 | 27.468 ms | 2.083 ms |
| physical WAL verification p50 | 24.687 ms | 0.310 ms |
| semantic replay p50 | 0.326 ms | 0.004 ms |
| root validation p50 | 1.582 ms | 1.277 ms |

The anchor retired 404 blocks and 26,476,544 bytes. The resulting WAL byte
reduction was 98.7775%, and warm external reopen p50 improved by 13.293409
times.

| Retention phase | Time |
|---|---:|
| anchor publication | 1.488 ms |
| WAL reset synchronization | 3.829 ms |
| anchor stabilization | 0.350 ms |
| total | 5.785 ms |

## WSL2 observation

Environment:

- Debian userspace under WSL2 kernel
  `6.18.33.1-microsoft-standard-WSL2`;
- repository resolved through `/mnt/e/...`, reported as `v9fs`; and
- benchmark data under `/tmp`, reported as `tmpfs`.

Raw observation:
[native-wal-replay-wsl2.json](native-wal-replay-wsl2.json).

| Observation | Full history | Retained suffix |
|---|---:|---:|
| WAL bytes | 26,804,224 | 327,680 |
| verified WAL blocks | 409 | 5 |
| replayed transactions | 406 | 4 |
| first external reopen | 24.685 ms | 1.584 ms |
| warm external p50 | 22.800 ms | 1.475 ms |
| warm external p95 | 23.821 ms | 1.796 ms |
| warm external p99 | 24.094 ms | 1.910 ms |
| warm internal p50 | 22.730 ms | 1.473 ms |
| physical WAL verification p50 | 20.883 ms | 0.224 ms |
| semantic replay p50 | 0.159 ms | 0.003 ms |
| root validation p50 | 1.422 ms | 1.152 ms |

The WAL byte reduction remained 98.7775%, and warm external reopen p50
improved by 15.461882 times.

| Retention phase | Time |
|---|---:|
| anchor publication | 0.019 ms |
| WAL reset synchronization | 1.103 ms |
| anchor stabilization | 0.007 ms |
| total | 1.134 ms |

The WSL2 benchmark data directory was `tmpfs`. Its synchronization timings are
not treated as native Linux filesystem or physical-durability evidence.

## Mechanical validation

The final evidence source is exercised on Windows and WSL2 with:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked --quiet
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py
```

The workspace-wide format, Clippy, test, rustdoc, and documentation lanes exit
zero on both environments. Hosted CI is recorded separately on the pull
request before merge.

Mutation testing is not configured for the repository and was not replaced by
ordinary tests.

## Remaining boundary

This vertical proves that WAL verification and semantic replay can be bounded
by a retained suffix under the current-root policy. It does not close G1 or
G7. Still missing:

- pruning and bounding the immutable manifest chain;
- multi-generation historical retention;
- replica, backup, archive, and snapshot pin registration;
- old manifest and immutable-blob collection;
- native Linux/ext4, macOS, cold-cache, saturation, p99.9, allocation, and
  hardware-counter observations;
- sector/power-loss fault injection beyond deterministic process boundaries;
- full interaction with mixed strict/group/memory scheduling and background
  maintenance; and
- mutation-testing evidence.

The measured retained-suffix reopen remains millisecond work. No universal
microsecond restart or power-loss durability claim follows from this evidence.
