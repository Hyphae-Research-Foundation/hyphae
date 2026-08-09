# Native hash field TTL evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, relative and
conditional field expiry, field persist/batches, reverse-pattern scans,
collection-family TTL, randomized model equivalence, complete G3, and G7
remain open

Source branch: `codex/native-hash-field-ttl`

Stacked base:
`codex/native-hash-pattern-scan@6fe6467ea3096eb37776e99dcd90129145dbf61a`

Contract commit:
`7b1cf552aa9e8847b1ce538e1cea104b8f39dd6d`

Runtime commit:
`1d6a2dc99c7576789efc74ffc526cd10ca3a682a`

Failure-boundary commit:
`97603a02c66c624d847f613004c0364b7a7917d7`

Equivalence commit:
`d0ca02f80daeec59a8c0c33cae54d188f183c3a6`

Benchmark and control-harness commit:
`fbbd6a999d20e1cd2fe5a9ff2a287c14dfe6504a`

Persistent-layout correction:
`5c02dc7c29be3dc86c3dd99f92a746f2d1cd94ac`

Final benchmark commit:
`ee18ec821775f4f27fd07fac0e0123495a952a45`

Final benchmark tree:
`202b7d8f8581faa7a3e87098d3e8872de1356d92`

## Scope

This slice adds absolute expiry to one exact native hash field without a
timer service, sidecar, compatibility protocol, external database, or second
writer. The embedded public surfaces are `EXPIRE_HASH_FIELD` and
`TTL_HASH_FIELD`; private batches, retained snapshots, current-root calls, WAL
replay, active cleanup, compaction, and reopen share the same semantics.

WAL opcode `EXPIRE_HASH_FIELD=32` is additive. An expiring field reuses the
canonical `HYSTRV01` envelope expiry flag. Its derived ordered identity is
`0x0c + sortable_signed_expiry + compound_hash_field_identity`, with exact
one-byte tombstone and live markers. Recovery requires a one-to-one match
between every expiring live field envelope and its index entry. Existing
persistent fields and trees without `0x0c` remain valid.

Whole-hash expiry dominates field expiry. Due fields are absent from `HGET`,
`HGET_MANY`, `HLEN`, ascending scans, descending scans, and pattern scans even
before physical cleanup. Physical scans still charge a due field as visited,
and pattern scans do not charge it as a matcher step. `HSET`, `HSET_MANY`, and
`HINCRBY` clear an exact field expiry; deleting a live expiring field
tombstones both physical identities.

The single-writer scheduler merges top-level and field-expiry namespaces in
`(expiry, namespace order, identity)` order under one combined `max_keys`
bound. Field operations publish one field conflict identity and validate the
whole-hash lifecycle identity, preserving disjoint-field rebasing while
rejecting stale same-field or retired-incarnation publication.

## Contract-first red and green

The compiler-reaching model gate was:

```text
cargo test -p hyphae-native-runtime \
  model::tests::hash_field_ttl_is_logical_and_replacement_clears_it --no-run
```

It failed on the deliberately missing field-expiry model methods before the
model, WAL, physical tree, scheduler, public APIs, or recovery changes were
implemented. The completed native-runtime suite has 272 passing tests.

New executable coverage includes:

- private, retained-snapshot, current-root, and reopened TTL/read equivalence;
- logical visibility immediately before, at, and after both field and
  whole-hash expiry;
- `HGET`, `HGET_MANY`, `HLEN`, ascending, descending, and pattern surfaces,
  including due-field visit and matcher-step accounting;
- `HSET`, `HSET_MANY`, `HINCRBY`, singular/batch delete, re-expiry, and
  immediate-due behavior;
- same-field conflicts, disjoint-field rebase, and whole-hash lifecycle
  fencing;
- WAL opcode shape, round-trip, replay, and the complete signed timestamp
  domain;
- missing, orphan, stale, wrong-timestamp, invalid-marker, truncated-identity,
  envelope/index, metadata, and blob corruption rejection;
- combined scalar, whole-hash, and field active-expiry order and bounds;
- all seven singleton commit interruption boundaries for field expiry and
  field cleanup; and
- compaction and reopen without resurrection.

The seven boundaries are deterministic injected commit interruptions. They
are not physical power-loss or EC2-stop evidence.

## Direct-Linux latency observation

The exact committed release harness uses one 2,048-field hash. Point reads use
200,000 observations, 20,000 warmups, and 32 operations per timed sample.
`HLEN` uses 10,000 observations and 1,000 warmups because it is a complete
field-prefix visibility operation. Mutation surfaces are measured separately.

| Route | p50 | p95 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| Persistent physical `HGET` | 1.500 us | 1.767 us | 1.795 us | 2.871 us |
| Persistent physical `HLEN`, no due fields | 228.047 us | 238.659 us | 241.940 us | 331.551 us |
| Persistent snapshot `TTL_HASH_FIELD` | 0.110 us | 0.113 us | 0.115 us | 0.393 us |
| Expiring private-batch `TTL_HASH_FIELD` | 0.118 us | 0.122 us | 0.124 us | 0.402 us |
| Expiring snapshot `TTL_HASH_FIELD` | 0.119 us | 0.122 us | 0.125 us | 0.404 us |
| Expiring physical `TTL_HASH_FIELD` | 1.484 us | 1.653 us | 1.780 us | 2.512 us |
| Expiring physical `HGET` | 1.501 us | 1.766 us | 1.802 us | 2.568 us |
| Physical `HLEN`, one due field | 226.646 us | 237.628 us | 242.202 us | 312.472 us |
| Memory `EXPIRE_HASH_FIELD` commit | 1.368 ms | 1.396 ms | 1.408 ms | 1.408 ms |
| Strict `EXPIRE_HASH_FIELD` commit | 8.275 ms | 10.974 ms | 10.974 ms | 10.974 ms |
| Memory cleanup, 64 fields | 5.372 ms | 5.614 ms | 5.614 ms | 5.614 ms |

Warm point TTL and HGET remain microsecond paths. `HLEN` is explicitly
cardinality-sensitive because independent due fields require visibility
filtering over the complete field prefix. Memory publication and cleanup are
millisecond paths. Strict commit includes ext4/EBS page and WAL
synchronization. None is presented as a universal latency bound.

The benchmark harness SHA-256 is
`a42534940d47c7454be748103c1d3592edc939d5a963d49156aaac2256353220`.
The raw output SHA-256 is
`2cbae1131d6510d23d08e906a4b9e48e4d6ad7dc4e1a1aa15fedc328519b5dd6`.

## Matched persistent-read control

The unchanged HGET harness SHA-256 is
`8d03592813a5607be4ff1f63c7fced8c059a59a37ebd524c4098f1b9ed05b0eb`.
The parent and current source trees used isolated Cargo targets.

| Metric | Parent median `6fe6467` | Current median `ee18ec8` | Change |
|---|---:|---:|---:|
| p50 | 1.3455 us | 1.379 us | +2.490% |
| p95 | 1.492 us | 1.526 us | +2.279% |
| p99 | 1.6435 us | 1.673 us | +1.795% |
| p99.9 | 2.621 us | 2.6495 us | +1.087% |
| Throughput | 732,646/s | 716,049/s | -2.265% |

The frozen gate was no more than a 10% increase at p50 and p95; it passed.
The machine-readable receipt retains both raw rounds and their hashes.

An early comparison that reused one Cargo target between worktrees is
explicitly invalid because Cargo reused the parent binary. Another pair used
the Rust version in the control's harness-commit field. Both were discarded
and repeated. A valid pre-fix comparison then exposed an approximately 40%
HGET regression: batching ordinary persistent HSET field and metadata writes
changed the seeded B+tree layout. Commit `5c02dc7` restored the original
sequential persistent fast path while retaining atomic batch publication only
when an existing field TTL must be cleared. The final isolated comparison
above proves the correction.

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
  --example hash_field_ttl_smoke
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_hget_control
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-field-ttl-linux.json
git diff --check
```

The runtime suite reported 272 passing tests. No dependency, unsafe-Rust
allowance, external runtime, or network protocol changed. Hosted checks are
deliberately not claimed by this local receipt; they are evaluated on the
stacked pull request. The checked machine-readable receipt SHA-256 is
`276285a73bd18e30646f2c33be314784ab2b0ecb2eccb2241662682aa30cf152`.

## Evidence boundary

This receipt proves one durable absolute hash-field TTL vertical on direct
Linux: logical visibility, compatible field envelopes, collision-free
ordered indexing, WAL/replay, field and lifecycle conflicts, bounded combined
cleanup, reopen, injected crash boundaries, compaction, and separated warm
latency surfaces.

It does not close relative or conditional expiry, field `PERSIST`, batch
field expiry, reverse-pattern scans, floating-point counters, TTL for other
collection families, streams, local-protocol exposure, randomized model
equivalence, saturation, cold cache, allocation/RSS, hardware counters,
physical power loss, complete G3, or G7.
