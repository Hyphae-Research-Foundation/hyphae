# Native hash structure evidence

Date: 2026-08-01

Status: first compound native structure family; G1, G3, and G7 remain open

Source commit:
`174d57876e48a41e72f04db676bdd9f2e99ae565`

Source tree:
`ca8983e318b4efa1880d981624b17b00a0590943`

Branch: `main`

## Change

Hyphae now owns one explicitly typed binary hash/map family:

- `CREATE_HASH`;
- `HSET`;
- `HGET`;
- `HDELETE`; and
- `HLEN`.

This is not a serialized map stored in a scalar and is not a request to
Valkey/Redis. Hash metadata and each field are independent native B+tree
entries under the same page store, buffer pool, WAL, MVCC root, CSN, conflict
table, blob namespace, and recovery authority as the relational and search
engines.

## Physical layout

The `HYSTRBT1` structure namespace now includes:

| Prefix | Identity | Value |
|---:|---|---|
| `0x02` | binary hash key | 16-byte `HYHSHM01` metadata |
| `0x03` | `u32be key_length + key + field` | persistent `HYSTRV01` |

`HYHSHM01` contains its magic and an exact `u64` live-field count. Fields use
the existing inline/blob envelope and the canonical tombstone. A field update
rewrites its field path and the small metadata path; it never decodes or
rewrites all other fields.

Recovery fails closed on malformed field identities, orphan fields,
expiry-bearing field envelopes, malformed metadata, or a live-field count
different from the reachable field set.

## Type and concurrency semantics

Hash creation is explicit. A key cannot become both scalar and hash:

- scalar mutation of an existing hash returns a kind error;
- duplicate hash creation returns an exists error;
- scalar and hash creation prepared from the same absent snapshot conflict on
  the same logical write key; and
- legacy `HYSTRT01` whole-state directories reject compound-family creation
  instead of silently losing it.

After creation, field mutations use the canonical hash-field identity as their
conflict key. Two detached transactions changing different fields both rebase
and commit, with physical cardinality advancing correctly. Same-field writers
remain first-committer-wins.

Deleting the last field leaves an empty typed hash. Whole-hash deletion and
type reuse remain unimplemented.

## Correctness evidence

The native runtime now has 37 tests. New coverage proves:

- added-versus-updated `HSET` and read-your-writes;
- field tombstones and exact `HLEN`;
- historical snapshots across field update/delete;
- strict reopen equivalence;
- a 2,048-field multilevel hash with direct reads, update, delete, retained
  snapshot, and reopen;
- disjoint-field optimistic rebase and same-field conflict;
- scalar/hash creation race admission;
- fail-closed forged metadata cardinality;
- canonical metadata and length-delimited identity codecs;
- legacy whole-state rejection;
- one large value deduplicated across a relational row, scalar key, and hash
  field; and
- all optimistic crash boundaries recovering either the prior state or the
  complete relational, scalar, expiry, and hash-field transaction.

The complete Debian 13/WSL2 workspace passed:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Windows compilation and strict Clippy also passed. The new Windows test
executable was not run because the active Application Control policy blocks
newly linked binaries; the policy was not weakened.

## Latency observation

The exact
[machine-readable v5 receipt](native-microsecond-smoke-hash-wsl2.json) adds
one 2,048-field hash to the existing 2,048-scalar and 2,049-row corpus. The
physical `HGET` performs two verified buffered B+tree lookups: metadata and
field.

The 64-byte physical `HGET` observed:

- p50 `0.917 us`;
- p95 `1.248 us`;
- p99 `2.335 us`;
- p99.9 `5.599 us`; and
- aggregate throughput `1,015,833 operations/s`.

The materialized snapshot `HGET` observed p50 `0.070 us`. There is no prior
physical hash baseline because schema v4 had no hash corpus or operation. This
is a sub-microsecond batch-average p50 observation, not individual-operation,
write, transport, concurrency, or G7 evidence.

## Product boundary

This does not make the structure engine complete. Still missing:

- whole-hash delete/recreate and hash-key TTL;
- field iteration, prefix/range scan, multi-field commands, and field
  counters;
- physical coalescing of repeated same-field mutations in one transaction;
- randomized model equivalence and write/memory amplification;
- the durable expiry index/timing wheel;
- lists, sets, sorted sets, streams, bitmaps, sketches, geo, and registers;
- eviction, blocking, pub/sub, and concurrent saturation; and
- local-protocol/CLI/SDK exposure.

G2 SQL completeness, G4 search completeness, and every later gate remain
independent and open.
