# Native whole-hash lifecycle evidence on Linux

Date: 2026-08-02

Status: whole-hash delete/recreate slice complete; complete G3, G7, hosted CI,
and mutation testing remain open

Source commit:
`2549c37fe403ed68caabeefe03badd9ce1a5824a`

Source tree:
`915fa55b6ece222718a1adff903e1cc5572d5b0d`

Source branch: `codex/native-hash-lifecycle`

Stacked base:
`codex/native-hash-field-scan@169ac4545145f18fc9dd3fc9323ac989a42ce5b4`

## Scope

This slice adds typed `DELETE_HASH(key)` to the native structure engine.
Deleting a live hash retires its metadata and every live field under one
global CSN. A missing key returns false without a mutation; another live
structure kind fails with the existing kind-mismatch error. Retained snapshots
preserve the prior incarnation.

The same transaction may recreate the key as an empty hash or another
structure kind. A recreated hash cannot expose fields from the retired
incarnation. WAL opcode 30 is additive and carries only the exact binary hash
key, with no target, value, or expiry.

Field mutations keep their field-granular write identities. They additionally
validate the scalar/collection ownership identity as a hash-lifecycle read
dependency without publishing it. Only hash create/delete publishes that
fence. This rejects a field writer prepared before deletion/recreation while
preserving independent admission for disjoint fields in one live incarnation.
If the field commits first, a later deletion rebases over and retires the
admitted field.

Physical publication scans only the admitted hash-field prefix, validates the
declared live count, and submits live field tombstones plus the metadata
tombstone through one sorted copy-on-write B+tree batch. Recovery accepts
tombstoned fields only under retired metadata and rejects live orphan fields.
Current-root compaction can remove the retired metadata and field tombstones
without resurrecting content or deleting a replacement scalar.

No structure marker, page format, catalog format, unsafe Rust, dependency, or
external runtime changed.

## Contract-first red and green

Contract commit `c9d962e` froze:

- missing, kind-mismatch, and successful-delete outcomes;
- retained-snapshot visibility and same-transaction typed recreation;
- the non-publishing field lifecycle dependency;
- bounded logical WAL identity with deterministic physical prefix deletion;
- metadata/field tombstones and fail-closed recovery; and
- explicit linear work for complete physical hash retirement.

The first compiler-reaching behavioral command was:

```text
cargo test -p hyphae-native-runtime \
  whole_hash_delete_recreates_without_retired_fields_and_preserves_history
```

It failed with six expected `E0599` errors because neither
`NativeWriteBatch` nor `NativeTransaction` exposed `delete_hash`.

Implementation commit `7420875` added the private model operation, public
batch surface, WAL codec/replay, lifecycle validation dependency, logical
rebase, physical tombstoning, retired-metadata decoding, typed recreation, and
compaction recognition. Test commit `a1cd872` added lifecycle/reopen,
concurrency ordering, all seven singleton commit crash boundaries, strict WAL
shape rejection, and post-delete compaction coverage. Benchmark commit
`2549c37` added the independent release harness.

The native-runtime suite now contains 234 passing library tests. Focused gates
cover:

- delete, recreate, retained snapshot, strict reopen, and cross-kind reuse;
- deletion-before-field rejection and field-before-deletion rebase;
- prior-or-complete recovery at blob stage/promotion, page append/sync, WAL
  append/sync, and root publication;
- exact empty WAL body requirements; and
- compaction of metadata plus field tombstones without resurrection.

## Direct-Linux latency observation

The release harness records three intentionally separate surfaces for hashes
with 0, 64, and 2,048 fields:

- `private_delete_hash`: only the already-prepared materialized batch method;
- `memory_commit`: physical B+tree pages and WAL publication without explicit
  durability synchronization; and
- `strict_commit`: the same physical publication with page and WAL
  synchronization.

Each route has 31 observations at concurrency one. Private samples roll back
the same prepared deletion. Commit samples delete 31 distinct hashes from one
seeded database; transaction begin and logical deletion occur before the
commit timer. Values are 64 bytes and field identities are four bytes.

The exact clean-source command was:

```text
HYPHAE_LIFECYCLE_OBSERVATIONS=31 \
  target/release/examples/hash_lifecycle_smoke \
  > /tmp/native-hash-lifecycle-linux.json
```

| Fields | Private p50 | Private p99 | Memory commit p50 | Memory commit p99 | Strict commit p50 | Strict commit p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0.103 us | 0.162 us | 177.367 us | 189.969 us | 6.500 ms | 6.787 ms |
| 64 | 1.321 us | 1.558 us | 397.373 us | 427.872 us | 7.057 ms | 7.234 ms |
| 2,048 | 54.106 us | 66.973 us | 3.561 ms | 5.179 ms | 12.299 ms | 13.223 ms |

The result is cardinality-sensitive by design. Retiring the private
materialized map deallocates its fields, and physical commit must enumerate
and tombstone every live field. The 2,048-field route is therefore not a
single-digit-microsecond operation. Strict timings include ext4/EBS
synchronization and are not a localhost execution-latency promise.

The raw stdout has SHA-256
`7a479603e397c951f71c24aabbb4ff9a938916ec6bb5575898102cb1e4a34fbe`.
The checked
[metadata-enriched receipt](native-hash-lifecycle-linux.json) preserves the
exact measurements, binds them to the source commit, and has SHA-256
`894dfdab3613adf2665705bdfa552a7b81ca43fff9bf3243c7c36773643401f2`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust and Cargo `1.96.0`, release profile; and
- direct execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on Linux with runtime code fixed at the source commit and
only evidence/documentation changes present:

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_lifecycle_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-lifecycle-linux.json
git diff --check
```

Results:

- complete workspace tests across all targets and features: passed;
- native-runtime library tests: 234 passed;
- complete workspace Clippy with warnings denied: passed;
- formatter and release harness check: passed;
- documentation and JSON validation: passed;
- diff check: passed; and
- dependency delta: none.

Hosted checks belong to the evidence commit and draft PR. Mutation testing was
not executed because the repository has no accepted mutation tool, operator
policy, or surviving-mutant threshold.

## Evidence boundary

The latency run is a concurrency-one, release, ext4/EBS observation. It does
not establish concurrent deletion, saturation, cold-cache behavior, other
field/value distributions, allocation/RSS, page amplification, hardware
counters, local-protocol transport, process power loss, or p99.9 stability.
The crash matrix is deterministic single-process interruption, not block-level
power-loss evidence.

This milestone closes whole-hash delete/recreate only. Hash TTL,
pattern/reverse scans, multi-field commands, field counters, set and
sorted-set whole-family lifecycle/algebra/TTL, streams, adaptive expiry
backoff, randomized model equivalence, complete memory-amplification evidence,
protocol exposure, complete G3 correctness, and the complete G7 matrix remain
open.
