# Native group-commit evidence

Date: 2026-08-02

Status: bounded scheduler and shared-flush implementation evidence; G1 and G7
remain open

## Source identity

- implementation commit:
  `d48f3c2c69be0b19130c00bebbec3a29e76a479d`;
- implementation tree:
  `8af4f3a5f6304c72d6bc7c43d1f4cc0c02946e9e`;
- contract commit: `8317dca`;
- native cohort implementation commit: `9336405`;
- scheduler implementation commit: `64fde82`;
- post-measurement admission-context cleanup commit: `92ac358`; and
- Rust: `rustc 1.96.0 (ac68faa20 2026-05-25)`.

The evidence documents and raw receipts are intentionally committed after the
measured implementation commit. The cleanup commit removes a new Clippy
suppression without changing the measured commit or benchmark receipt.

## Red and green evidence

The first acceptance test was added before implementation:

```text
error[E0432]: unresolved import `super::GroupCommitOutcome`
error[E0599]: no method named `commit_group` found for struct `NativeDatabase`
```

The scheduler acceptance was then added before its implementation:

```text
error[E0432]: unresolved imports `super::GroupCommitConfig`,
`super::GroupCommitSubmitError`, `super::NativeCommitScheduler`
```

The same focused lane is now green:

```text
running 5 tests
.....
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured
```

## Implemented behavior

- `RootGroupTransaction` retains a private WAL-anchored root chain while holding
  one writer admission; dropping it publishes nothing and consumes no CSN.
- `NativeDatabase::commit_group` admits at most 256 detached `group` batches.
- Accepted requests receive independent transaction IDs, contiguous CSNs,
  WAL transactions, root sets and receipts.
- First-committer-wins validation includes earlier accepted writes in the same
  cohort. A conflicting request is rejected without rejecting later disjoint
  requests.
- Catalog mutations advance catalog versions independently inside one cohort.
- The cohort appends all pages and WAL transactions, synchronizes the page file
  once, synchronizes the WAL once, then publishes the final root and conflict
  state.
- `NativeCommitScheduler` owns the database on a named thread, collects through
  a bounded multi-producer queue, drains commands preceding shutdown and closes
  the database handle.
- `ScheduledCommitReceipt` separates queue wait, database-side cohort
  execution, page sync, WAL sync and caller-observed end-to-end time.
- Large immutable blobs retain their per-file/directory durability work; the
  one-sync claim applies only to the shared page file and WAL.

The group is not an atomic super-transaction. Before acknowledgement, recovery
may retain a valid committed prefix. After WAL synchronization, every accepted
transaction in the cohort recovers.

## Correctness and crash evidence

Executable coverage includes:

- a two-commit cohort that mutates relational, structure and lexical-search
  state while one same-snapshot conflict is rejected;
- independent CSNs 2 and 3, cohort positions 0 and 1, one reported page sync
  and one reported WAL sync;
- strict reopen with both all-engine mutations present;
- catalog versions 2 and 3 in one genesis cohort;
- rejection of a `strict` batch from the group path with zero syncs;
- invalid batch/wait/queue scheduler bounds;
- two concurrent scheduler producers joining one cohort;
- clean shutdown and post-shutdown `Unavailable`; and
- the five-boundary crash matrix:
  `AdmittedWalPrefixAppended`, `CohortAppended`, `PageSynchronized`,
  `WalSynchronized`, and `RootPublished`.

Before WAL synchronization, reopen accepts only the prior state or a valid
queue-order prefix. At and after `WalSynchronized`, reopen requires the complete
cohort. No mixed mutation within one committed transaction is admitted.

## Benchmark command

```text
cargo run -p hyphae-native-runtime \
  --example group_commit_benchmark \
  --locked --release -- \
  <source-commit> <source-tree> "<rustc-version>"
```

Both receipts use 256 independent one-key commits, eight synchronized producer
threads, 32 cohorts and cohort size exactly eight. Strict and group directories
are reopened and every key is reverified.

## Windows observation

Environment:

- Microsoft Windows NT `10.0.26200.0`;
- x86-64, Intel Family 6 Model 197; and
- NTFS.

Raw receipt:
[native-group-commit-windows.json](native-group-commit-windows.json).

| Observation | Strict | Group |
|---|---:|---:|
| wall time | 381.925 ms | 109.031 ms |
| commits/second | 670.289 | 2,347.963 |
| per-request p50 | 1.294 ms | 2.735 ms |
| per-request p95 | 1.757 ms | 3.685 ms |
| per-request p99 | 1.991 ms | 3.839 ms |

Group throughput was 3.502910 times strict throughput. Group queue wait was
14.600 microseconds p50, 54.400 microseconds p95 and 60.700 microseconds p99.
The shared cohort execution was 2.696 milliseconds p50; page sync was 0.464
milliseconds p50 and WAL sync was 0.540 milliseconds p50.

The result proves amortized throughput for this exact warm corpus. It does not
prove lower single-request latency: group p50 was 2.114 times the strict p50.

## WSL2 observation

Environment:

- Debian userspace under WSL2 kernel
  `6.18.33.1-microsoft-standard-WSL2`;
- repository resolved through `/mnt/e/...`, reported as `v9fs`; and
- benchmark data under `/tmp`, reported as `tmpfs`.

Raw receipt:
[native-group-commit-wsl2.json](native-group-commit-wsl2.json).

| Observation | Strict | Group |
|---|---:|---:|
| wall time | 107.348 ms | 64.876 ms |
| commits/second | 2,384.775 | 3,945.977 |
| per-request p50 | 0.269 ms | 1.594 ms |
| per-request p95 | 0.346 ms | 1.900 ms |
| per-request p99 | 0.550 ms | 2.893 ms |

Group throughput was 1.654654 times strict throughput. Queue wait was 60.536
microseconds p50. The reported page/WAL sync p50 values were 0.485 and 0.150
microseconds.

Those `tmpfs` sync timings are not treated as physical-durability evidence. They
are materially different from the Windows NTFS receipt and must not be
generalized to ext4, bare metal, or power-loss behavior.

## Mechanical validation

The implementation was exercised with:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked --quiet
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
python tools/check_documentation.py
```

The workspace-wide format, Clippy, test and rustdoc lanes exited zero on
Windows and WSL2. The hosted CI result is recorded separately on the pull
request before merge.

Mutation testing is not configured for the repository and was not replaced by
ordinary tests.

## Remaining boundary

This vertical does not close G1 or G7. Still missing:

- simultaneous strict/group/memory scheduling under one resource policy;
- cancellation and deadline semantics for queued commits;
- background expiry and maintenance scheduling;
- bounded checkpoint replay, WAL retention and truncation;
- cold, saturation, long-soak, p99.9, allocation and hardware-counter receipts;
- native ext4 and macOS durability measurements;
- sector/power-loss fault injection beyond deterministic process boundaries;
- scheduler fairness and backpressure under abandoned clients;
- large-blob cohort amplification evidence; and
- mutation-testing evidence.

The measured queue can operate in microseconds. Shared durable execution remains
millisecond work on the primary Windows receipt. No universal microsecond commit
claim follows from this milestone.
