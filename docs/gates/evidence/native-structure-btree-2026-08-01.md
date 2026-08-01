# Native structure B+tree evidence

Date: 2026-08-01

Status: scalable scalar-storage slice; G1, G3, and G7 remain open

Source commit:
`ac81a3846ea64a5568ef2a753570c35f94128a5f`

Source tree:
`d0df6a735ec897b0af8ae091ba4843674760ad9f`

Branch: `main`

## Change

New data directories no longer serialize the entire structure keyspace into
one `StructureNode` page. Their structure root is a native copy-on-write
B+tree with:

- exact format marker `HYSTRBT1`;
- binary user keys under prefix `0x01`;
- exact `HYSTRV01` value envelopes;
- explicit expiry flag and signed absolute microsecond timestamp;
- inline values through 8,192 bytes; and
- immutable native blob references above that threshold.

Direct `get_latest_structure` and `ttl_latest_structure` traverse verified
pinned B+tree pages and decode only the selected envelope. Snapshot reads still
use the materialized compatibility state.

The expiry flag eliminates the legacy codec's `i64::MAX` sentinel ambiguity:
explicit timestamps of zero and `i64::MAX` round-trip distinctly from a
persistent value. Unknown flags, nonzero reserved bytes, noncanonical expiry
bytes, invalid storage kinds, oversized inline values, and undersized blob
references fail closed.

Earlier directories whose structure root has page kind `StructureNode` remain
readable and writable in their existing format. No implicit migration rewrites
their state.

## Correctness evidence

The 2,048-key physical test forces a height-two structure B+tree, reads a
64-byte target and its TTL directly, retains the original value through an old
snapshot after update, reopens, and verifies the new value and persistent TTL.

Additional tests prove:

- one large byte string used by both a relational row and a structure key
  resolves to one verified immutable blob;
- legacy single-page structure directories reopen, update, and reopen again;
- explicit `i64::MAX` expiry survives the new envelope;
- reserved-byte and noncanonical-persistent corruption fail closed;
- disjoint optimistic structure writes rebase without loss;
- same-key conflict admission still has one winner; and
- the existing seven commit and four checkpoint interruption matrices remain
  green with a B+tree structure root.

The complete Debian 13/WSL2 workspace passed:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The focused Windows run passed 28 runtime tests and strict Clippy. Formatting,
whitespace, receipt JSON, and relative Markdown links were checked separately.

## Multilevel latency observation

The exact
[receipt](native-microsecond-smoke-structure-btree-wsl2.json) introduces
benchmark schema v4 because the corpus now contains 2,048 structure keys and a
new physical operation. It retains 2,049 relational rows and requires both
trees to have height at least two.

The physical 64-byte structure `GET` observed:

- p50 `0.421 us`;
- p95 `0.556 us`;
- p99 `1.046 us`;
- p99.9 `3.247 us`; and
- aggregate throughput `2,193,898 operations/s`.

There is no valid prior physical-structure baseline: v3 measured a
materialized one-key `BTreeMap`, used a different corpus, and did not traverse a
structure root. The result is therefore an absolute local observation, not an
improvement claim.

This establishes a sub-microsecond batch-average p50 for one warm embedded,
height-two native structure read on this machine. It does not establish
individual-operation latency, cold performance, transport latency, concurrent
saturation, write latency, or a G7 pass.

## Product boundary

This is the owned physical scalar foundation, not “Valkey complete.” The
implemented command surface remains unconditional `SET`, `GET`, and `TTL`.
There is no `DELETE`, independent `EXPIRE`, conditional set, counters, hashes,
lists, sets, sorted sets, streams, pub/sub, blocking operations, expiry wheel,
or eviction policy yet.

## Next limits

- Add scalar tombstones, `DELETE`, `EXPIRE`, and conditional/versioned writes.
- Add signed/unsigned atomic counters and overflow semantics.
- Introduce family-tagged layouts for hashes, lists, sets, and sorted sets
  without changing scalar envelope identity.
- Replace eager whole-keyspace snapshot materialization with lazy root-backed
  views before large-scale randomized model testing.
- Measure write amplification, memory amplification, TTL scheduling, and
  concurrent saturation on an exact commit.
