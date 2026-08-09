# Native whole-list lifecycle evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, list TTL,
process-kill and block-level replay, complete G3, and G7 remain open

Source branch: `codex/native-list-lifecycle`

Stacked base:
`codex/native-set-lifecycle@ca52da51aaeba4c66d0ddbc79e4856b599701fa8`

Contract commit:
`bc56d12b17ce2ae595b0a18093d168e4bac71a20`

Runtime implementation commit:
`2c4e7779d293cf43993bc5fb92afb8e360697a8e`

Test-hardening commit:
`2b489799f95acc859789fb467da4865e04c49c15`

Benchmark and final verified Rust source commit:
`9c6008eba6dadbaf00da5bfc77a0ccb261578e55`

Final verified Rust source tree:
`e771b7dd607e90cf13f8873d28f571d49b158b8c`

## Scope

This slice adds embedded `DELETE_LIST(key) -> bool` and additive structure
opcode `DELETE_LIST=35` to Hyphae's native chunked deque. Deleting a list
retires its typed metadata and every live chunk under one global CSN. Missing
returns false without a mutation; scalar, hash, set, and sorted-set keys fail
with `StructureKindMismatch`.

Retained snapshots preserve the complete prior sequence. The same transaction
may recreate the key as every implemented structure family, including a
populated new list. Old inline or blob-backed elements never attach to the
replacement.

Physical publication decodes live metadata, scans the exact chunk prefix,
validates identities, envelopes, contiguity, and total element count, then
tombstones all live chunks plus metadata through one sorted B+tree batch.
Current-state decoding accepts a retired list only when all reached chunks are
tombstones. Blob files remain under the existing reachability collector.

No existing opcode meaning, page, catalog, metadata, or directory format
changed. No dependency, unsafe Rust, external runtime, sidecar, or internal
serialized protocol was added.

## Contract-first red and green

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime \
  list_lifecycle_equivalence::whole_list_delete_recreates_without_retired_elements_and_preserves_history \
  -- --exact --nocapture
```

It failed with one expected `E0599` because `NativeTransaction` did not expose
`delete_list`.

Nine focused tests now cover:

- missing, empty, populated, multichunk, blob-backed, and wrong-family cases;
- private read-your-writes, retained snapshots, physical current reads, and
  strict reopen;
- recreation as scalar, hash, set, populated list, and sorted set;
- deletion after earlier same-transaction head push and tail pop;
- stale writer, writer-before-delete, duplicate-delete, and recreation-fence
  conflicts under the existing whole-list identity;
- all seven singleton interruption boundaries for deletion and all seven
  again for deletion plus recreation;
- malformed/tombstoned metadata, count divergence, chunk gaps, malformed
  reached envelopes, malformed/wrong identities, and a checksum-corrupt
  reached root page; and
- compaction, pinned historical visibility, page-generation vacuum,
  checkpoint/WAL retention, blob collection, and reopen without resurrection.

The gates exposed and corrected a physical-read bug: `LLEN` and `LRANGE`
initially decoded a retired metadata tombstone as corruption instead of
returning typed absence.

The complete native-runtime suite reported 321 passing tests.

## Direct-Linux latency observation

The release harness records already-prepared private deletion, Memory
publication without explicit synchronization, and Strict publication with
page/WAL synchronization. Transaction begin and logical deletion occur before
the commit timers. Each route has 31 observations at concurrency one.

```text
HYPHAE_LIST_LIFECYCLE_OBSERVATIONS=31 \
  target/release/examples/list_lifecycle_smoke \
  > /tmp/hyphae-list-lifecycle-smoke-authoritative.json
```

| Elements | Private p50 | Private p99 | Memory commit p50 | Memory commit p99 | Strict commit p50 | Strict commit p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0.095 us | 0.247 us | 179.524 us | 196.437 us | 6.499 ms | 6.708 ms |
| 64 | 0.608 us | 0.718 us | 291.132 us | 319.283 us | 6.929 ms | 7.121 ms |
| 2,048 | 34.036 us | 42.944 us | 1.201 ms | 1.632 ms | 8.719 ms | 10.047 ms |

The result is cardinality- and chunk-sensitive. Strict time includes real
ext4/EBS synchronization. The public singleton receipt does not expose
per-component durations, so independent Memory and Strict distributions are
not subtracted.

The raw JSON remains at
`/tmp/hyphae-list-lifecycle-smoke-authoritative.json` with SHA-256
`34ec68405082b91e7e3183f83b4d19f3e8d67b6e8a5b903db9ae93968d7e99fb`.
The harness SHA-256 is
`aeaa873be656fabd9387d85f82b9723b81a1516729f67f940099872954b346d5`.
The checked
[machine-readable receipt](native-list-lifecycle-linux.json) has SHA-256
`29a6a2928b6870c24704b139d51449c5e98fceca33541c983c38d953f7607bd1`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- pinned Rust and Cargo `1.96.0`, release profile for measurements; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example list_lifecycle_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-list-lifecycle-linux.json
git diff --check
```

Results:

- 9 focused lifecycle tests: passed;
- native-runtime library: 321 passed;
- complete workspace tests and Clippy: passed;
- formatter, release example, documentation, JSON, and diff checks: passed;
- dependency delta: none.

## Open boundaries

This receipt does not claim list TTL, generic cross-family `DEL`, blocking
operations, insertion/trimming/moving, batched push/pop, streams, protocol
exposure, process-kill replay, EC2 stop, block-level power-loss replay,
saturation, cold-cache behavior, RSS, hardware counters, complete G3, or G7.
