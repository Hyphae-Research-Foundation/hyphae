# Native manifest-retention evidence

Date: 2026-08-02

Status: current-root manifest-prefix retirement and retained-chain recovery
evidence; G1 and G7 remain open

## Source identity

- measured implementation commit:
  `fc11ae290fa7c5b1808142d86aa304787f118b44`;
- measured implementation tree:
  `e583c9d55ded4179e21ae2bf33363ec353c579f3`;
- contract commit: `ab3de1e`;
- retained-chain store commit: `eb0b253`;
- runtime integration commit: `f3f82be`; and
- Rust: `rustc 1.96.0 (ac68faa20 2026-05-25)`.

The evidence documents and raw observations are intentionally committed after
the measured benchmark commit.

## Red and green evidence

The first store acceptance failed before implementation:

```text
error[E0599]: no associated function or constant named `open_after`
found for struct `RootManifestStore`
```

The runtime acceptance then failed on the missing manifest receipt/report
fields and the missing `WalRetentionBoundary::ManifestPrefixRemoved` variant.

The focused lanes are now green:

```text
hyphae-native-manifest: 8 passed; 0 failed
hyphae-native-runtime: 159 passed; 0 failed
```

The changed implementation and benchmark add no `allow`, `unsafe`, `unwrap`,
`expect`, `panic`, or `unreachable` occurrence.

## Implemented behavior

- No new durable file format is introduced. The selected `HYWAR001` anchor
  authenticates the exact retained manifest generation and digest.
- `RootManifestStore::open_after` requires that exact immutable base and
  verifies every later generation and predecessor-digest link.
- The retained base keeps its original absolute generation, nonzero
  predecessor digest, bytes, filename, WAL anchor, page generation, and
  retention floor.
- Without a retention anchor, open still requires the historical
  generation-one/zero-predecessor chain.
- Canonical files below the verified base are removable candidates. Their
  contents are no longer recovery authority, so interrupted deletion may leave
  any lower-generation subset.
- Base or retained-suffix absence, corruption, generation gaps, or digest
  divergence fail closed.
- Runtime selects a stable or recoverable pending WAL anchor before opening
  manifests, then validates retained WAL, checkpoints, roots, pages, blobs,
  catalog, relational, structure, lexical, and ANN state before deleting any
  prefix file.
- Manifest deletion precedes prior WAL-anchor cleanup and is idempotent on
  reopen.
- Recovery and maintenance receipts separate retained/retired manifest
  counts/bytes, chain-verification time, pruning time, and directory-sync
  support.

## Interruption and corruption evidence

The WAL-retention matrix now covers seven boundaries:

1. `AnchorStaged`;
2. `AnchorPending`;
3. `WalReset`;
4. `WalSynchronized`;
5. `AnchorStabilized`;
6. `ManifestPrefixRemoved`; and
7. `PriorAnchorRemoved`.

Every boundary reopens to one complete current-root state and accepts an
idempotent retry.

A separate test interrupts after anchor stabilization, manually removes one
lower manifest, corrupts another retired lower manifest, and then reopens from
the exact generation-three anchor. Recovery ignores those nonauthoritative
contents, removes the remaining retired file, and verifies the relational,
structure, and lexical state. Other tests corrupt the retained base and a
later retained checkpoint manifest; both fail closed with a manifest checksum
error.

The store tests separately prove exact base-digest selection, retained-chain
continuity, idempotent pruning, arbitrary retired-prefix gaps, strict
generation-one behavior without an anchor, and base/suffix corruption
rejection.

## Benchmark method

The executable observation is:

```text
cargo run -p hyphae-native-runtime \
  --example manifest_retention_benchmark \
  --locked --release -- \
  <source-commit> <source-tree> "<rustc-version>"
```

Two independent directories reach the same final state:

- each performs one seed commit and eight historical updates;
- each publishes 128 pre-base checkpoint manifests;
- each vacuums the page generation and publishes manifest generation 129 as
  the current-root base;
- only the retained corpus publishes `HYWAR001` and removes generations
  1–128;
- each appends one suffix commit that mutates relational, scalar-key,
  lexical-search, and ANN state;
- each publishes two suffix checkpoints, ending at absolute generation 131;
- each is reopened 25 times; and
- every reopen verifies the exact latest row, scalar value, lexical document,
  and ANN object identity.

The first reopen is reported separately. Percentiles use the remaining 24 warm
reopens. External reopen includes both WAL and manifest retention effects;
`warm_manifest_verification` isolates manifest-chain work inside open.

## Windows observation

Environment:

- Microsoft Windows NT `10.0.26200.0`;
- x86-64; and
- benchmark data directories under the Windows temporary directory on NTFS.

Raw observation:
[native-manifest-retention-windows.json](native-manifest-retention-windows.json).

| Observation | Complete chain | Retained chain |
|---|---:|---:|
| manifest files | 131 | 3 |
| manifest bytes | 29,392 | 720 |
| manifest base generation | 1 | 129 |
| cumulative checkpoints | 131 | 131 |
| replayed transactions | 11 | 1 |
| first external reopen | 20.761 ms | 1.548 ms |
| warm external p50 | 15.794 ms | 1.202 ms |
| warm external p95 | 17.969 ms | 1.506 ms |
| warm external p99 | 18.929 ms | 1.515 ms |
| manifest verification p50 | 4.984 ms | 0.130 ms |
| physical WAL verification p50 | 9.493 ms | 0.169 ms |
| semantic replay p50 | 0.046 ms | 0.003 ms |
| root validation p50 | 0.590 ms | 0.511 ms |

Manifest bytes fell by 97.5504%. Manifest verification p50 improved by
38.218558 times, while complete external reopen p50 improved by 13.144985
times.

The strict maintenance receipt removed 128 files and 28,672 bytes in 13.295
milliseconds. Total anchor/WAL/manifest retention took 17.093 milliseconds.
Windows does not expose a strict roots-directory flush in this implementation,
so this is process-boundary and performance evidence, not a power-loss
durability claim.

## WSL2 observation

Environment:

- Debian userspace under WSL2 kernel
  `6.18.33.1-microsoft-standard-WSL2`;
- source checkout through `/mnt/e/...`, reported as `v9fs`; and
- benchmark data under `/tmp`, reported as `tmpfs`.

Raw observation:
[native-manifest-retention-wsl2.json](native-manifest-retention-wsl2.json).

| Observation | Complete chain | Retained chain |
|---|---:|---:|
| manifest files | 131 | 3 |
| manifest bytes | 29,392 | 720 |
| manifest base generation | 1 | 129 |
| cumulative checkpoints | 131 | 131 |
| replayed transactions | 11 | 1 |
| first external reopen | 8.187 ms | 0.730 ms |
| warm external p50 | 8.072 ms | 0.648 ms |
| warm external p95 | 8.791 ms | 0.758 ms |
| warm external p99 | 8.896 ms | 0.787 ms |
| manifest verification p50 | 0.380 ms | 0.008 ms |
| physical WAL verification p50 | 7.017 ms | 0.132 ms |
| semantic replay p50 | 0.018 ms | 0.001 ms |
| root validation p50 | 0.488 ms | 0.422 ms |

Manifest verification p50 improved by 48.024295 times, and external reopen p50
improved by 12.463069 times. Pruning measured 0.301 milliseconds and total
retention 0.696 milliseconds.

The data directory was on `tmpfs`. These synchronization and latency values
are not native ext4, persistent-device, or power-loss evidence.

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

This vertical closes the immediate unbounded generation-one manifest scan
under the current-root policy. It does not close G1 or G7. Still missing:

- multi-generation historical retention;
- replica, snapshot, backup, archive, and incremental-backup pins;
- immutable-blob collection coordinated with retained roots;
- native Linux/ext4, macOS, cold-cache, saturation, p99.9, allocation, and
  hardware-counter observations;
- Windows roots-directory durability support;
- sector/power-loss fault injection beyond deterministic process boundaries;
- interaction with mixed strict/group/memory scheduling and background
  maintenance; and
- mutation-testing evidence.

The retained Windows reopen remains millisecond work. The WSL2 sub-millisecond
result was observed on `tmpfs`. No universal microsecond persistent restart or
power-loss durability claim follows from this milestone.
