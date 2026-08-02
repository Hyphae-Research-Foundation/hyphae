# Native chunked-list evidence

Date: 2026-08-01

Status: first native deque/list slice; G1, G3, and G7 remain open

Measured source commit:
`9f0e1158f058b1f4f9f21488f8b42af01d6cfde4`

Measured source tree:
`467821bc735ea2ea1b48a4ddebf0448008c8f5a2`

Branch at measurement: `codex/native-list-structure` (pull request 30)

## Change

Hyphae now owns one explicitly typed binary deque/list family:

- `CREATE_LIST`;
- `LPUSH` and `RPUSH`;
- `LPOP` and `RPOP`;
- `LLEN`; and
- inclusive signed-index `LRANGE`.

This is not a serialized vector stored under a scalar key and is not a request
to Valkey or Redis. Logical snapshots use a deque. Durable state uses
independent metadata and packed end chunks inside Hyphae's structure B+tree,
under the same page store, buffer pool, WAL, blob store, MVCC root, CSN,
conflict table, and recovery authority as the relational and search engines.

## Physical layout

The `HYSTRBT1` structure namespace now includes:

| Prefix | Identity | Value |
|---:|---|---|
| `0x06` | binary list key | 32-byte `HYLSTM01` metadata |
| `0x07` | `u32be key_length + key + ordered i64 chunk_id` | `HYLSTC01` packed chunk or canonical tombstone |

Metadata records the exact `u64` element count and signed head/tail chunk
identities. Empty lists have count, head, and tail equal to zero. Each live
chunk carries one to 64 persistent `HYSTRV01` element envelopes and is capped
at 10,000 encoded bytes. Large elements therefore use the existing immutable
blob path without enlarging the B+tree page value.

A push rewrites only the current end chunk when it still fits, otherwise it
creates the adjacent signed chunk. A pop rewrites the end chunk or tombstones
it and advances the boundary. Popping the final element leaves canonical
typed-empty metadata. A bounded physical `LRANGE` starts at the closer end and
decodes only chunks intersecting the requested range.

WAL opcodes append without renumbering existing operations:

- `CREATE_LIST=20`;
- `PUSH_LIST_HEAD=21`;
- `PUSH_LIST_TAIL=22`;
- `POP_LIST_HEAD=23`; and
- `POP_LIST_TAIL=24`.

Pop records retain the exact removed logical bytes so physical replay can
verify that the selected end element agrees with the admitted mutation.

## Semantics, concurrency, and failure behavior

Scalar, hash, set, and list kinds are exclusive. Explicit creation persists an
empty typed list. Empty pops return absence without adding a mutation.
`LRANGE` clamps indexes to the live domain, interprets negative indexes from
the tail, uses an inclusive stop, and returns an empty result for an inverted
or nonintersecting range.

All mutations of one list intentionally share a whole-list conflict identity
in this version. Disjoint-end optimistic writers therefore remain
first-committer-wins instead of publishing an unproved merge. Creation races
with scalar, hash, and set creation in the same ownership domain.

Complete-state loading fails closed on malformed metadata or chunks, expiry
or tombstone element envelopes, missing or additional live chunks, count
mismatch, noncontiguous chunk identities, orphan chunks, invalid blobs, and
cross-family key collisions. List identities that cannot fit the native
B+tree key limit fail before private mutation.

## Correctness evidence

The native runtime suite now has 108 tests. New coverage proves:

- both-end pushes and pops, read-your-writes, empty pops, signed/clamped
  ranges, and typed-empty persistence;
- retained MVCC snapshots and strict reopen;
- a packed multi-chunk list, end-chunk tombstone, and a large blob-backed
  element;
- whole-list first-committer-wins and four-family creation races;
- exact WAL round-trip and malformed target, expiry, and creation shapes;
- fail-closed forged count and chunk-gap roots;
- legacy whole-state rejection; and
- all seven commit interruption boundaries recovering either the prior list
  or the complete blob-backed list transaction.

The complete WSL2 workspace passed:

```text
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo fmt --all -- --check
python tools/check_documentation.py
git diff --check
```

Hosted cross-platform checks for the pull request were still running when
this evidence was written. They remain merge authority.

## Latency observation

The [machine-readable WSL2 receipt](native-list-wsl2.json) uses one reopened
2,048-element list, 64-byte values, a multilevel structure B+tree, 10,000
warmups, and 100,000 observations per route.

| Route | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| Physical `LLEN` | 0.226 us | 0.238 us | 0.275 us | 3.191 us | 11.554 us | 4,186,678 ops/s |
| Physical `LRANGE 0 9` | 2.178 us | 2.404 us | 3.002 us | 30.801 us | 295.415 us | 429,283 ops/s |
| Physical `LRANGE -10 -1` | 2.213 us | 2.439 us | 3.499 us | 19.544 us | 1,057.864 us | 425,378 ops/s |

The `LLEN` observation amortizes timer overhead over 16 physical calls per
sample. Each `LRANGE` sample is one complete allocating call. Warm p50 and p99
for both end ranges remained in single-digit microseconds. One tail-range
maximum crossed one millisecond; this run does not establish a worst-case
bound.

This is one warm, concurrency-one, single-machine observation. It does not
measure cold pages, middle ranges, concurrent readers, write interference,
saturation, transport, allocation or RSS, wide/large elements, compaction, or
blocking consumers. It is not G7.

## Remaining boundary

This does not make the structure engine or Valkey-compatible surface
complete. Missing list work includes:

- indexed insert/update, trim, move, rotate, and multi-key operations;
- blocking pops, wakeup ordering, cancellation, and timeout semantics;
- list-key TTL, durable expiry scheduling, eviction, and key deletion/reuse;
- chunk split/merge compaction, density controls, and write-amplification
  evidence;
- randomized model equivalence over long mutation histories;
- concurrent reader/writer and saturation benchmarks; and
- local-protocol, CLI, and SDK exposure.

Middle `LRANGE` remains proportional to the number of chunks between the
closer end and the requested range because v1 metadata has no per-chunk rank
index. G1, G3, and G7 remain open.
