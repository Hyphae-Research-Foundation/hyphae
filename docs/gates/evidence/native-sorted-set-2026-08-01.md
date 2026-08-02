# Native dual-index sorted-set evidence

Date: 2026-08-01

Status: first native sorted-set slice; G1, G3, and G7 remain open

Measured source commit:
`ae1af265b7c879a65874a47c2438860b60c820cb`

Measured source tree:
`13c1c20f701806147d58873a09228bb63c7e2fd5`

Branch at measurement: `codex/native-sorted-set-structure`

## Change

Hyphae now owns one explicitly typed binary sorted-set family:

- `CREATE_SORTED_SET`;
- `ZADD`;
- `ZSCORE`;
- `ZREM`;
- `ZCARD`; and
- inclusive signed-rank `ZRANGE`.

This is not a serialized map under a scalar key and does not call Redis or
Valkey. Every sorted set has a member-to-score index and a score/member ordered
index inside Hyphae's existing copy-on-write structure B+tree. Both indexes
share the native page store, buffer pool, WAL, MVCC root, CSN, conflict table,
checkpoint, and recovery authority used by the relational and search engines.

## Physical layout and score order

The `HYSTRBT1` namespace now includes:

| Prefix | Identity | Value |
|---:|---|---|
| `0x08` | binary sorted-set key | 16-byte `HYZSTM01` metadata |
| `0x09` | `u32be key_length + key + member` | 16-byte `HYZSCR01` score or canonical tombstone |
| `0x0a` | `u32be key_length + key + sortable_score + member` | canonical live marker or tombstone |

Metadata stores exact live cardinality. The membership index owns the
canonical binary64 score. The ordered index transforms score bits so B+tree
byte order is numeric ascending, then uses exact member bytes as the stable
tie-breaker. `NaN` fails before private mutation, and both signed zero inputs
canonicalize to positive zero. Positive and negative infinity remain valid.

`ZADD` creates or replaces both physical projections. Rescoring tombstones the
old ordered key before publishing the new key. `ZREM` tombstones both
projections and decrements metadata. An empty sorted set remains explicitly
typed.

WAL opcodes append without renumbering existing operations:

- `CREATE_SORTED_SET=25`;
- `UPSERT_SORTED_SET_MEMBER=26`; and
- `DELETE_SORTED_SET_MEMBER=27`.

## Concurrency and recovery

Creation shares the structure key's family-conflict identity. Member mutations
use the compound sorted-set/member identity: disjoint optimistic writers
rebase without losing cardinality, while same-member writers remain
first-committer-wins.

Complete-state loading reconstructs both indexes and requires one-to-one
agreement on set, member, and score. It rejects metadata count mismatch,
orphan projections, missing projections, stale live scores, malformed score
encodings, `NaN`, negative zero, noncanonical live markers, oversized
identities, and cross-family collisions.

## Correctness evidence

The native runtime suite has 115 tests. New coverage proves:

- deterministic score/member order, signed rank ranges, score replacement,
  removal, historical snapshots, direct physical reads, and strict reopen;
- `NaN` rejection, signed-zero normalization, infinity order, typed-key
  exclusion, and identity limits without partial mutation;
- WAL round-trip and malformed mutation-shape rejection;
- disjoint-member optimistic rebase and same-member conflict;
- forged metadata, membership-score, and ordered-index corruption rejection;
- a 2,048-member multilevel B+tree with update/delete, retained snapshot, and
  reopen; and
- all seven commit interruption boundaries recovering either the prior dual
  index or the complete new dual index.

The clean WSL2 workspace passed:

```text
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo fmt --all -- --check
python tools/check_documentation.py
git diff --check
```

Windows compiled the changed crate and passed clippy with warnings denied.
Execution of the newly generated Windows test binary was blocked by the
machine's Application Control policy (`os error 4551`), so this document does
not claim a fresh local Windows runtime pass. Hosted Windows CI remains the
cross-platform execution authority.

## Latency observation

The [machine-readable WSL2 receipt](native-sorted-set-wsl2.json) uses one
reopened 2,048-member set, 64-byte members, a height-two structure B+tree,
10,000 warmups, and 100,000 observations per route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical `ZCARD` | 0.502 us | 0.571 us | 1.290 us | 5.111 us | 72.248 us | 1,843,462 ops/s |
| Physical middle-member `ZSCORE` | 1.044 us | 1.195 us | 2.516 us | 7.297 us | 195.446 us | 897,450 ops/s |
| Physical `ZRANGE 0 9` | 12.057 us | 14.845 us | 29.731 us | 107.586 us | 356.815 us | 77,023 ops/s |

Point-read samples amortize timer overhead over 16 complete physical calls.
Each range sample is one allocating call that stops after ten live ordered
entries. Warm medians and p99s remained in microseconds, not milliseconds.
This is one warm, concurrency-one, single-machine observation and is not G7.

## Remaining boundary

This does not make the structure engine or a Valkey-compatible surface
complete. Missing sorted-set work includes:

- score-bound ranges, reverse ranges, rank lookup, score increment, pop, and
  conditional update modes;
- union, intersection, difference, multi-key destination operations, and
  blocking variants;
- sorted-set TTL, key deletion/reuse, durable expiry scheduling, and eviction;
- subtree rank counts or a reverse cursor: middle and tail rank ranges remain
  proportional to preceding live ordered entries;
- long randomized model equivalence, memory/write-amplification,
  compaction/density, concurrent-reader, saturation, and cold-page evidence;
  and
- local-protocol, CLI, SDK, and SQL relation-valued exposure.

G1, G3, and G7 remain open.
