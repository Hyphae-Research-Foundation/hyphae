# Native durable snapshot pins on Linux

Date: 2026-08-02

Status: direct Linux implementation, retention, and process-crash evidence;
hosted CI, physical power loss, mutation testing, and complete G1 remain open

Source commit:
`01355d0a1e4538284b0ae8b0fa82c195d4469647`

Source tree:
`8a271664ecbb6e5a2b6021eb5dfcc5c03952465c`

Branch: `codex/native-snapshot-pins`

Base: `dev@af7d12da05620275c22cc22e65cc004a66c63e77`

## Scope

This slice adds explicit, durable multi-generation MVCC retention to the
native runtime. A stable `SnapshotPinId` now names one immutable all-engine
root, captured logical time, manifest and WAL authority, page generation,
blob generation, retention floor, and directory lineage.

The implementation is native Hyphae code. Engine-to-engine paths remain
in-process and share the existing page/blob allocation, WAL, catalog, CSN,
MVCC, and recovery authorities. No database, cache, search sidecar, TCP, HTTP,
JSON, or compatibility protocol was introduced into the runtime.

The public operations are:

- `pin_current`, which checkpoints and publishes an immutable retention
  record;
- `open_pinned_snapshot`, which revalidates and materializes the exact
  historical root from its named page generation;
- `unpin`, which removes only the durable retention claim; and
- `collect_retired_page_generations`, which deletes only inactive unpinned
  page files and reports exact files and bytes.

## Red-to-green

The frozen contract landed first at `74a726f`. The first executable run failed
at compile time because the seven intended surfaces did not exist:
`SnapshotPinId`, `pin_current`, `open_pinned_snapshot`, `unpin`,
`collect_retired_page_generations`, `SnapshotPinExists`, and
`UnknownSnapshotPin`.

Implementation commit `c78686b` added the fixed 240-byte `HYPIN001` record,
create-new registry, lineage and authority verification, pinned historical
materialization, page-generation retention, and fail-closed WAL/blob
collection integration. Test commit `6cd4269` added the corruption,
multi-generation, retention, vacuum-interruption, and real process-kill
matrices. Benchmark commit `01355d0` added the matched release observation.

No warning suppression, unsafe Rust, dependency, or external runtime was
added.

## Exact behavior proven

The deterministic runtime suite proves:

- relational, structure/TTL, lexical, and exact ANN state reopen at the
  pinned CSN after divergent commits, vacuum, close, and reopen;
- captured logical time preserves TTL results;
- three distinct pinned page generations survive beside a fourth active
  generation and never alias equal page IDs;
- unpinning the middle generation removes only its file; repeated collection
  removes zero files;
- removing the final historical pins permits the remaining retired files,
  WAL/manifests, and blobs to be collected while current state reopens;
- every existing vacuum interruption boundary preserves the older pin;
- older pins reject incompatible WAL retention and blob collection before
  destructive mutation;
- canonical temporary stages never become authority;
- duplicate, zero, unknown, malformed, corrupt, renamed, foreign-lineage,
  missing-manifest, and missing/corrupt-page cases fail closed; and
- old native directories without `pins/` gain a synchronized empty namespace
  during open without changing their committed state.

## Live process crash matrix

The checked
[schema-v3 receipt](native-snapshot-pins-process-crash-linux.json) preserves
the seven singleton commit and four checkpoint scenarios and adds two
snapshot-pin children. Each child reaches one exact boundary, reports
readiness while its database handle and writer lock remain live, and is then
terminated by the parent with `SIGKILL`.

| Snapshot-pin boundary | Expected pin | Recovered pins | Files in `pins/` | Retained page generations |
|---|---|---:|---:|---:|
| record synchronized under `.tmp` | absent | 0 | 0 | 1 |
| immutable record published | complete | 1 | 1 | 1 |

All 13 children record `termination: signal-9`. The staged case proves that a
complete synchronized temporary record is not authority and is removed during
recovery. The published case reopens and reads the exact relational,
structure/TTL, and lexical state through the stable pin.

The exact clean-source command was:

```text
cargo run --release --locked -p hyphae-native-runtime \
  --example process_crash_matrix -- \
  01355d0a1e4538284b0ae8b0fa82c195d4469647 \
  aws-m6i.2xlarge-ext4-ebs
```

## Multi-generation release observation

The checked
[snapshot-pin receipt](native-snapshot-pins-linux.json) creates and pins page
generations one through three, publishes generation four as active, closes
the database, reopens all four authorities, and verifies the exact
relational, structure, lexical, and vector state.

| Observation | Result |
|---|---:|
| Reopen with three pins | 2.241694 ms |
| Pin 1 publish / materialize | 15.147653 ms / 0.465657 ms |
| Pin 2 publish / materialize | 14.120765 ms / 0.467309 ms |
| Pin 3 publish / materialize | 13.784026 ms / 0.459946 ms |
| Vacuum generations 2 / 3 / 4 | 10.600091 / 10.283201 / 10.457868 ms |
| Middle unpin collection | 1 file, 327,680 bytes removed |
| State retained after middle collection | 3 files, 999,424 bytes |
| Repeated collection | 0 files, 0 bytes removed |
| Final historical collection | 2 files, 917,504 bytes removed |
| Final active state retained | 1 file, 81,920 bytes |
| Final reopen with zero pins | 1.316766 ms |

These are single release observations, not latency distributions or
microsecond performance gates. Pin publication includes a strict checkpoint
and filesystem synchronization; vacuum and reopen are maintenance paths. The
historical materialization observations are below one millisecond on this
small warm corpus, but no p99.9 or universal latency claim follows.

The exact clean-source command was:

```text
cargo run --release --locked -p hyphae-native-runtime \
  --example snapshot_pin_benchmark -- \
  01355d0a1e4538284b0ae8b0fa82c195d4469647 \
  8a271664ecbb6e5a2b6021eb5dfcc5c03952465c \
  rustc-1.96.0-(ac68faa20-2026-05-25) \
  aws-m6i.2xlarge-ext4-ebs
```

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- repository and `/tmp` on `/dev/nvme0n1p1`, ext4 over the EBS root device;
- Rust `1.96.0`, target `x86_64-unknown-linux-gnu`; and
- direct execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Direct-Linux gates

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo run --release --locked -p hyphae-native-runtime \
  --example process_crash_matrix -- <commit> <environment>
cargo run --release --locked -p hyphae-native-runtime \
  --example snapshot_pin_benchmark -- <commit> <tree> <rustc> <filesystem>
python3 tools/check_documentation.py
```

Results before adding this evidence pair:

- formatter: pass;
- complete workspace Clippy: pass with warnings denied;
- complete workspace tests: pass;
- native runtime: 206 passed, 0 failed;
- release process matrix: 13 of 13 `SIGKILL`/reopen scenarios passed;
- release multi-generation observation: pass with exact byte accounting;
- dependency delta: none; and
- mutation testing: not configured and not run.

The documentation result and hosted CI belong to the evidence commit and PR,
respectively; they are not inherited from the pre-evidence source commit.

## Evidence boundary

`SIGKILL` is process-crash evidence, not physical power loss. It preserves the
Linux kernel page cache and cannot establish lost-write, torn-sector,
filesystem-reordering, device-cache, EC2-stop, EBS-failure, detached-volume,
disk-full, or injected-I/O-error behavior.

The benchmark is one warm, concurrency-one, small-corpus observation. It does
not establish cold behavior, saturation, background scheduling, quotas,
leases, replicas, backup pins, remote archives, or p99.9 latency.

This milestone removes multi-generation durable snapshot pins from the open
G1 substrate list. It does not close G1: offline promotion/epoch transition,
broader maintenance and resource-exhaustion matrices, literal infrastructure
failure, and the complete phase exit evidence remain open.
