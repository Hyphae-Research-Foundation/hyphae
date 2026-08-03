# Native whole-hash TTL evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, complete G3, G7,
randomized model equivalence, and physical power-loss evidence remain open

Source branch: `codex/native-hash-ttl`

Stacked base:
`codex/native-hash-lifecycle@a8174f68e866578083c7062e8640f0b0fb9aa3f8`

Contract commit:
`4a91c8b0527804d3db6d00b711973821c201775f`

Runtime implementation commit:
`f2d6649`

Format and benchmark commit:
`d2149451bc03d6733f3514b0d1ebfac6457cf270`

Matched-control harness commit:
`ad42f8592145f3257ad51915799a5a266ffcd06d`

Final verified source commit:
`f2c731210025217d9454243bb4bc5768f842e938`

Final verified source tree:
`1959e92f40c0a1f24c5defd2090c78b33d08cbc5`

## Scope

This slice adds absolute whole-family expiry to native hashes without adding a
timer service, sidecar, protocol hop, second writer, or third-party engine.
The public deterministic surfaces are `EXPIRE_HASH`, `TTL_HASH`, retained
snapshot reads, explicit-time current-root physical reads, and the existing
single-writer active-expiry scheduler.

WAL opcode `EXPIRE_HASH=31` is additive. Persistent hashes keep the exact
16-byte `HYHSHM01` metadata. Expiring hashes use exact 24-byte `HYHSHM02`
metadata with a signed expiry. The existing ordered `0x0b` namespace now uses
typed one-byte markers: tombstone `0x00`, scalar `0x01`, and whole hash
`0x02`. No timestamp is a sentinel.

Logical expiry is a complete hash-incarnation boundary. Fields never carry
independent TTL. A due hash is missing to `HGET`, `HLEN`, `HSCAN`, field
mutation, explicit hash deletion, and re-expiry before physical cleanup. The
same transaction may reuse the key as a scalar, hash, set, list, or sorted
set; logical `DELETE_HASH` cleanup retires the prior metadata, complete field
prefix, and expiry entry before the replacement becomes visible.

`EXPIRE_HASH` publishes the existing family lifecycle identity. Field
mutations retain field-granular publication while validating that family
identity. A field writer prepared before admitted expiry therefore conflicts;
a whole-hash expiry prepared before a disjoint field commit may rebase over
that field and preserve it.

## Contract-first red and green

The first compiler-reaching model gate was:

```text
cargo test -p hyphae-native-runtime \
  model::tests::whole_hash_ttl_is_a_logical_incarnation_boundary --no-run
```

It failed with the expected eight `E0599` errors for missing `expire_hash`,
`ttl_hash_micros`, and `hget_at` methods. The model gate then passed before
WAL, tree, scheduler, or public API work continued.

The completed runtime suite has 243 passing tests. New coverage includes:

- private, retained-snapshot, explicit-time physical, and reopened visibility
  before, at, and after the expiry boundary;
- empty and populated hashes, re-expiry, field insert/delete with TTL
  preservation, explicit delete, scalar reuse, and reuse by every explicit
  collection family;
- WAL opcode round-trip, exact shape rejection, `HYHSHM01`/`HYHSHM02`
  canonical bytes, and the full signed timestamp representation;
- lifecycle conflicts in both expiry-before-field and field-before-expiry
  commit order;
- mixed scalar/hash bounded cleanup, `more_due`, memory and strict durability,
  empty-sweep behavior, and real scheduler-thread observability;
- fail-closed missing, stale, orphan, malformed, and cross-kind expiry-index
  cases;
- all seven existing singleton commit interruption boundaries for hash expiry
  and mixed scalar/hash cleanup; and
- compaction of expired metadata, every field, and the typed expiry tombstone
  without resurrection.

The seven-boundary tests are deterministic injected process-boundary tests.
They are not literal process kill, block replay, EC2 stop, or physical
power-loss evidence.

## Direct-Linux latency observation

The release harness uses a 2,048-field hash, 200,000 read observations, 32
operations per timed read sample, 20,000 warmups, and concurrency one.
Mutation surfaces are measured separately from reads.

| Route | p50 | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| Persistent physical `HGET` | 1.351 us | 1.612 us | 1.676 us | 2.366 us |
| Persistent snapshot `TTL_HASH` | 0.007 us | 0.008 us | 0.008 us | 0.016 us |
| Expiring private-batch `TTL_HASH` | 0.012 us | 0.013 us | 0.016 us | 0.027 us |
| Expiring snapshot `TTL_HASH` | 0.012 us | 0.013 us | 0.014 us | 0.026 us |
| Expiring physical `TTL_HASH` | 0.631 us | 0.639 us | 0.908 us | 1.089 us |
| Expiring physical `HGET` | 1.364 us | 1.506 us | 1.656 us | 2.043 us |
| Memory `EXPIRE_HASH` commit | 1.391 ms | 1.423 ms | 1.454 ms | 1.454 ms |
| Strict `EXPIRE_HASH` commit | 8.355 ms | 10.897 ms | 10.897 ms | 10.897 ms |
| Memory cleanup, 256 fields | 0.614 ms | 0.676 ms | 0.676 ms | 0.676 ms |

Strict commit includes ext4/EBS page and WAL synchronization. Cleanup is
cardinality-sensitive because it validates and tombstones the complete live
field prefix. Neither is claimed as a microsecond read path.

## Matched persistent-read control

The exact same control harness and 2,048-field corpus ran on the parent and
current commits on the same host:

| Metric | Parent `a8174f6` | Current `ad42f85` | Change |
|---|---:|---:|---:|
| p50 | 1.350 us | 1.391 us | +3.037% |
| p95 | 1.487 us | 1.582 us | +6.389% |
| p99 | 1.639 us | 1.686 us | +2.868% |
| p99.9 | 1.854 us | 2.750 us | +48.328% |
| Throughput | 731,891/s | 708,652/s | -3.175% |

The frozen gate was no more than 10% increase at p50 and p95; it passed. The
p99.9 delta is disclosed, was not a frozen gate, and needs repeated tail
sampling before interpretation.

The final `f2c7312` smoke observation separately measured persistent physical
`HGET` at 1.351 us p50 and 1.612 us p95. It supports the same gate direction
but does not replace the exact matched-harness comparison above.

The parent ran from a detached temporary Linux worktree. The exact committed
control source from `ad42f85` was copied unchanged into that worktree; its
SHA-256 is
`24fcc0ce3c8277b58df9e59409f2dfe0338e6c108cf4920fba4cda8421219f5d`.
The worktree and its isolated Cargo target were removed after the run; all
source remains recoverable from Git.

The checked machine-readable
[receipt](native-hash-ttl-linux.json) contains the raw operation and control
statistics.

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
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_ttl_smoke
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_hget_control
python3 tools/check_documentation.py
python3 -m json.tool docs/gates/evidence/native-hash-ttl-linux.json
git diff --check
```

Every command above passed against final source commit `f2c7312`; the runtime
suite reported 243 passing tests. The machine-readable receipt SHA-256 is
`7643d77f4cbd61cf32c20b8e076d38eb4612b3369cc0467c7acb81815be881e0`.
No dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.

## Evidence boundary

This receipt proves one deterministic whole-hash TTL vertical on direct Linux:
logical absence, typed physical encoding, WAL/replay, lifecycle conflicts,
bounded scheduler cleanup, reopen, injected crash boundaries, compaction, and
separated warm latency surfaces.

It does not close complete G3 semantics, complete G7, saturation, cold cache,
allocation/RSS, hardware counters, local-protocol exposure, randomized model
equivalence, set/list/sorted-set TTL, field TTL, relative TTL, sliding expiry,
`PERSIST_HASH`, process-kill replay, block replay, or physical power loss.
