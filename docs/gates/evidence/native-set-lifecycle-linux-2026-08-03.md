# Native whole-set lifecycle evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, process-kill and
block-level replay, complete G3, and G7 remain open

Source branch: `codex/native-set-lifecycle`

Stacked base:
`codex/native-set-commands@76854a04d42ae51219c67f41b786a883bfca57ec`

Contract commit:
`a557f9d4aee5ac421aa5ca211520a04c8479a588`

Runtime implementation commit:
`765cdd7980ea0e16a88a96a26e21590ee3d438ed`

Test-hardening commit:
`36b9d1b63fa19234e3a2b2568f10fa623962e2d9`

Benchmark and final verified Rust source commit:
`13dfe30dfd87a22cc68c089a2e72107543961714`

Final verified Rust source tree:
`7014d35004f114003cb51be281d8fddf9fb4677f`

## Scope

This slice promotes Hyphae's existing internal `DELETE_SET=34` lifecycle
mutation to the embedded native write surface. Deleting a visible set retires
its metadata, every live exact member, and the exact current whole-set expiry
marker under one global CSN. A missing or logically due set returns false
without a mutation. A scalar, hash, list, or sorted set fails with
`StructureKindMismatch`.

Retained snapshots continue to observe the complete prior incarnation. The
same transaction may recreate the key as a scalar, hash, populated set, list,
or sorted set. Retired members and expiry never attach to the replacement.
Member writes prepared earlier in the deleting transaction also disappear
from the final visible state.

The implementation adds no opcode, page, catalog, metadata, or directory
format. It adds no dependency, unsafe Rust, external runtime, network
protocol, or serialized internal hop.

## Contract-first red and green

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime \
  set_lifecycle_equivalence::whole_set_delete_recreates_without_retired_members_and_preserves_history \
  -- --exact --nocapture
```

It failed with one expected `E0599` because `NativeTransaction` did not expose
`delete_set`.

The existing WAL codec, materialized replay, lifecycle conflict identity, and
physical `delete_set_in_tree` path already supported internal expiry cleanup.
The implementation commit exposed the checked API without changing those
formats. The hardening commit added ten focused tests covering:

- missing, due, empty, populated, expiring, and wrong-family behavior;
- private read-your-writes, retained snapshots, current-root physical state,
  strict reopen, and exact expiry-marker retirement;
- recreation as every currently implemented structure family;
- deletion after same-transaction member additions and removals;
- stale-member rejection, admitted-member rebase, duplicate deletion, and
  prior-incarnation rejection after recreation;
- all seven singleton interruption boundaries for deletion and all seven
  again for deletion plus recreation;
- malformed/tombstoned metadata, cardinality divergence, malformed reached
  member envelopes, invalid expiry markers, malformed/wrong member
  identities, and a checksum-corrupt reached root page; and
- current-root structure compaction, pinned historical visibility,
  page-generation vacuum, and reopen without resurrection.

The complete native-runtime suite reported 312 passing tests.

## Direct-Linux latency observation

The release harness records three distinct surfaces for empty, 64-member, and
2,048-member sets:

- `private_delete_set`: only the already-prepared materialized deletion call;
- `memory_commit`: physical B+tree and WAL publication without explicit page
  or WAL synchronization; and
- `strict_commit`: the same publication with page and WAL synchronization.

Transaction begin and logical `DELETE_SET` preparation occur before the commit
timers. Each route has 31 observations at concurrency one. Commit samples
delete distinct sets from one seeded database; every database is reopened and
its commit count checked after the run.

```text
HYPHAE_SET_LIFECYCLE_OBSERVATIONS=31 \
  target/release/examples/set_lifecycle_smoke \
  > /tmp/hyphae-set-lifecycle-smoke-authoritative.json
```

| Members | Private p50 | Private p99 | Memory commit p50 | Memory commit p99 | Strict commit p50 | Strict commit p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0.101 us | 0.256 us | 174.554 us | 204.966 us | 6.483 ms | 6.757 ms |
| 64 | 0.868 us | 1.015 us | 359.388 us | 453.963 us | 7.040 ms | 7.258 ms |
| 2,048 | 27.598 us | 34.417 us | 2.470 ms | 3.235 ms | 10.851 ms | 12.106 ms |

The result is cardinality-sensitive by contract. Private materialization
retires the in-memory member set; physical commit validates the declared
cardinality, visits the complete member prefix, and publishes sorted
tombstones. Strict time includes real ext4/EBS synchronization.

The public singleton receipt does not expose execution, page-sync, WAL-sync,
and root-publication durations separately. This receipt therefore keeps
Memory and Strict as separate measured routes and does not subtract one
independent distribution from another.

The raw JSON remains on the canonical Linux host at
`/tmp/hyphae-set-lifecycle-smoke-authoritative.json`. Its SHA-256 is
`f4f6e44e802041000275db58bb6cf3ee35dd87e17a0afa5a826f02ecf0bcf5a5`.
The harness SHA-256 is
`1bf8ae228ce82cd611efaf97921fe6de19ef06b0a80a26101ca663bb96626d9a`.
The checked
[machine-readable receipt](native-set-lifecycle-linux.json) has SHA-256
`2a6e3989ac583da4cd0b56ca0d35979c84950d33197732e38d28ab8973793889`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- pinned Rust and Cargo `1.96.0`, release profile for measurements; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host with Rust fixed at the
benchmark commit and only documentation/evidence changes added afterward:

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example set_lifecycle_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-set-lifecycle-linux.json
git diff --check
```

Results:

- 10 focused lifecycle tests: passed;
- native-runtime library: 312 passed;
- complete workspace tests across all targets and features: passed;
- complete workspace Clippy with warnings denied: passed;
- formatter and release example check: passed;
- documentation and machine-readable receipt validation: passed; and
- dependency delta: none.

## Open boundaries

This receipt does not claim generic cross-family `DEL`, protocol exposure,
relative or conditional expiry, `PERSIST_SET`, member TTL, destination-set
algebra, process-kill replay, EC2 stop, block-level power-loss replay,
saturation, cold-cache behavior, RSS, hardware counters, p99.9 stability,
complete G3, or G7.
