# Native scalar structure mutation evidence

Date: 2026-08-01

Status: durable scalar-operation slice; G1, G3, and G7 remain open

Source commit:
`6d2c52eec0d043949bc3874b2e31435ca8d3711b`

Source tree:
`b8032e186df9e3de1aa7c24cb8af3ac95ee7637b`

Branch: `main`

## Change

The native structure engine now owns these additional scalar operations:

- `SET` with unconditional, if-absent (`NX`), or if-present (`XX`)
  predicates;
- `DELETE` with a canonical physical `HYSTRV01` tombstone;
- independent absolute `EXPIRE`;
- missing-versus-expired `TTL` semantics; and
- checked signed `INCRBY` over canonical decimal `i64` values.

These are in-process engine operations, not RESP commands and not calls to a
Valkey/Redis process. They enter the same transaction, WAL, CSN, conflict
table, page store, root publication, and recovery path as relational and
search mutations.

## Durable representation

`HYSTRV01` flag bit 1 now identifies a tombstone. The only canonical tombstone
has:

- flags exactly `0x02`;
- inline storage;
- zero reserved and expiry bytes; and
- no payload.

Unknown combinations and nonempty tombstones fail closed. `DELETE` upserts the
tombstone through one copy-on-write B+tree path; retained roots continue to
expose the pre-delete version.

WAL opcodes 8 and 9 identify `DELETE VALUE` and `EXPIRE VALUE`. Mutation flag
bit 0 records explicit expiry presence. This removes the WAL-level
`i64::MAX` ambiguity while accepting older flag-zero records with a non-sentinel
timestamp. A runtime test commits `EXPIRE(..., i64::MAX)`, reopens the
directory, and verifies the exact remaining TTL.

## Operation semantics

`NX` and `XX` evaluate against the private transaction state at its
deterministic logical time, including earlier writes in that transaction.
False conditions add no mutation. Racing detached `NX` writers may both
prepare, but first-committer-wins allows one commit and rejects the stale
same-key writer.

`DELETE` and `EXPIRE` return false for a missing or expired key. At the exact
expiry timestamp, both `GET` and `TTL` report missing.

`INCRBY` accepts only the canonical `i64::to_string` representation. It
preserves an existing TTL, treats missing or expired keys as zero, and fails
without adding its own mutation on invalid input or checked-add overflow.
Same-key concurrent transactions retain the existing first-committer-wins
retry requirement; this slice does not claim wait-free or commutative counter
publication.

## Correctness evidence

The native runtime now has 31 tests. New cases prove:

- read-your-writes `NX`/`XX` behavior;
- a racing `NX` pair admits one writer;
- historical snapshots retain values deleted, expired, or incremented later;
- the current root contains the exact physical tombstone;
- direct reads and full-state recovery both hide tombstones;
- controlled logical-time expiry is missing at the exact boundary;
- signed minimum, noncanonical input, and overflow behavior;
- TTL preservation during increment;
- strict reopen equivalence for every new operation;
- explicit `i64::MAX` expiry through WAL and reopen; and
- every optimistic commit interruption recovers either the prior state or the
  complete relational update plus structure `DELETE` and `EXPIRE`.

The complete Debian 13/WSL2 workspace passed:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The Windows compiler and strict Clippy path passed. Windows Application
Control blocked execution of the newly linked test binary with `os error
4551`; the policy was not weakened, and this document does not claim a fresh
Windows runtime pass for this commit.

## Latency observation

The exact
[machine-readable receipt](native-microsecond-smoke-scalar-mutations-wsl2.json)
uses the unchanged v4 corpus: 2,048 structure keys, 2,049 relational rows, and
height-two B+trees. It measures live `GET`, not write, WAL, or fsync latency.

The physical 64-byte structure `GET` observed:

- p50 `0.524 us`;
- p95 `0.599 us`;
- p99 `1.169 us`;
- p99.9 `3.596 us`; and
- aggregate throughput `1,802,362 operations/s`.

The previous matched receipt observed p50 `0.421 us` and p99 `1.046 us`.
However, the unchanged relational control also moved from p50 `0.805 us` to
`0.973 us` in this run. Without affinity, interference controls, repetitions,
or hardware counters, no causal regression or improvement claim is supported.
The current result still observes a sub-microsecond batch-average p50 and does
not pass G7.

## Product boundary

This is not “Valkey complete.” Missing G3 work includes:

- expected-version predicates and version-bearing operation receipts;
- physical coalescing of repeated same-key mutations within one transaction;
- a durable expiry index and bounded timing wheel;
- unsigned and typed-register counters;
- hashes, lists, sets, sorted sets, streams, bitmaps, probabilistic structures,
  and geo structures;
- randomized model equivalence and memory/write-amplification receipts;
- eviction, blocking, pub/sub, and concurrent saturation evidence; and
- a native local-protocol command surface for these operations.

G2 SQL completeness, G4 search completeness, and every later gate remain
independent and open.
