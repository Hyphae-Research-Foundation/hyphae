# Native set structure evidence

Date: 2026-08-01

Status: first native set slice; G1, G3, and G7 remain open

Source commit:
`9a63526d04f94979dd5c93fc4110f1ccf68fbfb3`

Source tree:
`81803ed03ff2d1dba4c81a54c20b86e7becee471`

Branch: `codex/native-set-structure`

## Change

Hyphae now owns one explicitly typed exact binary set family:

- `CREATE_SET`;
- `SADD`;
- `SISMEMBER`;
- `SREM`; and
- `SCARD`.

This is not a serialized collection stored in a scalar and is not a request to
Valkey/Redis. Set metadata and every member are independent native B+tree
entries under the same page store, buffer pool, WAL, MVCC root, CSN, conflict
table, and recovery authority as the relational and search engines.

## Physical layout

The `HYSTRBT1` structure namespace now includes:

| Prefix | Identity | Value |
|---:|---|---|
| `0x04` | binary set key | 16-byte `HYSETM01` metadata |
| `0x05` | `u32be key_length + key + member` | empty persistent `HYSTRV01` |

`HYSETM01` contains its magic and an exact `u64` live-member count. A live
member has one exact empty persistent envelope; deletion publishes the shared
canonical tombstone. `SADD` and `SREM` rewrite only the member path and the
small metadata path.

Recovery fails closed on malformed member identities, orphan members,
non-empty or expiry-bearing member envelopes, malformed metadata, or a
live-member count different from the reachable member set.

WAL opcode assignments append without renumbering prior operations:

- `CREATE SET=14`;
- `ADD SET MEMBER=15`; and
- `DELETE SET MEMBER=16`.

## Type and concurrency semantics

Set creation is explicit. Scalar, hash, and set kinds are exclusive:

- scalar mutation of an existing set returns a kind error;
- duplicate structure creation returns an exists error;
- scalar, hash, and set creation prepared from one absent snapshot conflict
  in the shared ownership domain; and
- legacy `HYSTRT01` whole-state directories reject set creation rather than
  silently losing it.

After creation, member mutations use a separate canonical set-member conflict
domain. Two detached transactions adding different members both rebase and
commit with exact physical cardinality. Same-member writers remain
first-committer-wins. Removing the last member leaves an empty typed set.

## Correctness evidence

The native runtime now has 81 tests. New coverage proves:

- duplicate-aware `SADD`, exact `SISMEMBER`, `SREM`, and `SCARD`;
- member tombstones and read-your-writes;
- historical snapshots across member add/remove;
- strict reopen equivalence;
- a 2,048-member multilevel set with direct reads, retained snapshot, mutation,
  and reopen;
- disjoint-member optimistic rebase and same-member conflict;
- scalar/hash/set creation race admission;
- fail-closed forged metadata cardinality;
- fail-closed malformed and orphan member entries;
- canonical metadata, member envelope, and length-delimited identity codecs;
- WAL round-trip plus truncated member-identity rejection;
- legacy whole-state rejection; and
- every optimistic commit crash boundary recovering either the prior set state
  or the complete member remove/add transaction.

The complete Debian 13/WSL2 workspace passed:

```text
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Windows strict Clippy and the full library/integration test matrix passed:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --lib --tests --all-features
```

The Windows `--all-targets` matrix reached the newly linked example executable
and was blocked by Application Control with `os error 4551`; the policy was not
weakened. WSL2 executed the same example target successfully.

## Latency observation

The exact
[machine-readable v11 receipt](native-microsecond-smoke-set-wsl2.json) adds one
2,048-member set to the existing multilevel scalar, hash, search, and
relational corpus. Physical `SISMEMBER` performs two verified buffered B+tree
lookups: set metadata and member identity.

Physical `SISMEMBER` observed:

- p50 `1.635 us`;
- p95 `2.003 us`;
- p99 `3.623 us`;
- p99.9 `7.235 us`; and
- aggregate throughput `583,618 operations/s`.

Materialized snapshot `SISMEMBER` observed p50 `0.100 us` and p95 `0.104 us`.
There is no prior physical set baseline because schema v10 had no set corpus
or set operation. These are batch-average warm observations, not
individual-operation, write, transport, concurrency, or G7 evidence.

## Product boundary

This does not make the structure engine complete. Still missing:

- whole-set delete/recreate and set-key or member TTL;
- `SMEMBERS`/`SSCAN`, bounded iteration, multi-member commands, and set
  algebra;
- physical coalescing of repeated same-member mutations in one transaction;
- randomized model equivalence and write/memory amplification;
- the durable expiry index/timing wheel;
- lists, sorted sets, streams, bitmaps, sketches, geo, and registers;
- eviction, blocking, pub/sub, and concurrent saturation; and
- local-protocol/CLI/SDK exposure.

G2 SQL completeness, G4 search completeness, and every later gate remain
independent and open.
