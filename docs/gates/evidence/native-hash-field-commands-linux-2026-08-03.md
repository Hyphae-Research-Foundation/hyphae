# Native hash field commands evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, pattern/reverse hash
scans, complete G3, G7, randomized model equivalence, and physical power-loss
evidence remain open

Source branch: `codex/native-hash-field-commands`

Stacked base:
`codex/native-hash-ttl@6df7c8020a460bcd99848a654043197c8c57f1c7`

Contract commit:
`616515df36fba0a2dd9b0b8c51babe73f28b2fd8`

Runtime commit:
`714b5781ba3949b01b46636e321cff8afe2f6c6f`

Benchmark commit:
`c406d15bc1b4fbda031d4a41b999863644f1a182`

Benchmark tree:
`4cdfe21518b632d477fbb14572c50e76ef377dc0`

## Scope

This slice adds four bounded embedded hash operations:

- `hget_many` returns owned optional values in caller order and preserves
  duplicate positions;
- `hset_many` rejects duplicate exact fields, prepares accepted fields in
  exact-byte order, and returns the added-field count;
- `hdelete_many` applies the same validation/order rules and returns the
  deleted-field count; and
- `hincrement_i64` starts a missing field at zero, accepts only canonical
  signed-decimal `i64` bytes, uses checked arithmetic, and returns the new
  integer.

Every multi-field call is bounded to 4,096 positions. Empty input validates
the hash and returns an empty result or zero without adding a mutation. Batch
size, complete identities, and duplicates are preflighted before private
state changes. The same identity preflight also closes the singular
`HSET`/`HDELETE` error-after-private-mutation gap exposed during Clippy review.

All operations require an existing visible typed hash and preserve its exact
whole-family expiry. They use the existing `SET_HASH_FIELD` and
`DELETE_HASH_FIELD` WAL mutations, field write identities, lifecycle
validation identity, B+tree namespaces, blob path, cardinality metadata,
replay, and compaction. No opcode, physical format, dependency, sidecar, or
internal protocol was added.

## Contract-first red and green

The compiler-reaching model gate was:

```text
cargo test -p hyphae-native-runtime \
  model::tests::hash_field_commands_form_one_atomic_model_transition --no-run
```

It failed with five expected `E0599` errors for missing `hset_many`,
`hincrement_i64`, `hget_many_at`, and `hdelete_many` methods. The logical
model transition then passed before public API and physical execution work.

The completed runtime suite has 249 passing tests. New coverage includes:

- private, retained-snapshot, current-root physical, and reopened
  `HGET_MANY` equivalence with missing and duplicate positions;
- exact whole-hash TTL preservation and logical absence at expiry;
- empty input, 4,096 plus one, duplicate mutation fields, oversized compound
  identities, another kind, and a missing hash;
- canonical/noncanonical integer input, checked overflow, and proof that
  rejected calls add no private mutation;
- exact-byte canonical WAL mutation order independent of caller order;
- disjoint-field optimistic rebasing, same-field conflict, whole-transaction
  conflict atomicity, and the whole-hash lifecycle fence;
- all seven singleton commit interruption boundaries with inline values, one
  staged blob, multi-field set/delete, a counter, cardinality, TTL, and reopen;
  and
- fail-closed reached hash metadata, field envelope, and blob corruption.

The seven-boundary matrix is deterministic injected process-boundary evidence.
It is not literal process kill, block replay, EC2 stop, or physical power loss.

## Direct-Linux latency observation

The release harness uses one 2,048-field hash. Read routes use 100,000
observations, 10,000 warmups, 32 exact existing fields per call, and
concurrency one. Commit surfaces use 32 memory observations or 16 strict
observations.

| Route | p50 total | p50/field | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|---:|
| Snapshot `HGET_MANY(32)` | 1.982 us | 0.062 us | 2.029 us | 2.063 us | 11.685 us |
| Private `HGET_MANY(32)` | 1.975 us | 0.062 us | 2.024 us | 2.055 us | 11.308 us |
| Physical `HGET_MANY(32)` | 27.300 us | 0.853 us | 27.933 us | 37.193 us | 50.936 us |
| Physical 32 singular `HGET` calls | 47.334 us | 1.479 us | 55.974 us | 57.596 us | 86.072 us |
| Memory `HSET_MANY(32)` commit | 4.638 ms | 0.145 ms | 4.933 ms | 5.224 ms | 5.224 ms |
| Strict `HSET_MANY(32)` commit | 14.558 ms | 0.455 ms | 14.722 ms | 14.722 ms | 14.722 ms |
| Memory `HDELETE_MANY(32)` commit | 6.829 ms | 0.213 ms | 7.023 ms | 7.023 ms | 7.023 ms |
| Memory `HINCRBY` commit | 1.476 ms | 1.476 ms | 1.497 ms | 1.498 ms | 1.498 ms |
| Strict `HINCRBY` commit | 7.817 ms | 7.817 ms | 8.080 ms | 8.080 ms | 8.080 ms |

One physical batch captures and validates the hash metadata once. Against 32
singular physical calls on the same corpus, it reduced total p50 by 42.325%,
p95 by 50.096%, p99 by 35.424%, and p99.9 by 40.822%; call throughput
increased by 73.337%.

Commit measurements include private preparation, B+tree copy-on-write, WAL,
root publication, and selected durability. Strict maximums reached 292.663 ms
for multi-set and 224.391 ms for increment; these EBS/fsync tails are disclosed
and are not microsecond-path claims.

## Matched persistent-read control

The unchanged control harness ran on the stacked parent and benchmark commits
on the same host. Both source files had SHA-256
`8d03592813a5607be4ff1f63c7fced8c059a59a37ebd524c4098f1b9ed05b0eb`.

| Metric | Parent `6df7c80` | Current `c406d15` | Change |
|---|---:|---:|---:|
| p50 | 1.393 us | 1.371 us | -1.579% |
| p95 | 1.640 us | 1.623 us | -1.037% |
| p99 | 1.710 us | 1.693 us | -0.994% |
| p99.9 | 2.435 us | 2.917 us | +19.795% |
| Throughput | 707,814/s | 718,104/s | +1.454% |

The frozen gate was no more than 10% increase at p50 and p95; it passed. The
p99.9 change is disclosed, was not a frozen gate, and remains open for
repeated tail sampling.

The parent ran from a detached temporary Linux worktree with an isolated Cargo
target. Both paths were resolved and verified before removal. Source remains
recoverable from Git.

The checked machine-readable
[receipt](native-hash-field-commands-linux.json) contains all raw statistics.
Its SHA-256 is
`801c411fd8b50929ead8d41a23bed7382d011ed6771e2ffa661c801e89f27a0e`.

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
cargo test -p hyphae-native-runtime --lib --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_field_commands_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-field-commands-linux.json
git diff --check
```

The runtime suite reported 249 passing tests and every command above passed
against benchmark commit `c406d15` plus the evidence-only changes. No
dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.

## Evidence boundary

This receipt proves bounded native multi-field read/set/delete and a signed
field counter across model, private state, physical B+tree, WAL/replay,
optimistic conflicts, lifecycle fences, crash boundaries, blobs, retained
snapshots, reopen, and separated warm latency surfaces.

It does not close glob matching, reverse hash scans, field TTL, relative or
sliding expiry, floating-point counters, local-protocol exposure, randomized
model equivalence, saturation, cold cache, allocation/RSS, hardware counters,
memory amplification, process-kill replay, physical power loss, complete G3,
or G7.
