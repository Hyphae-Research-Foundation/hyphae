# Native borrowed point-read evidence

Date: 2026-08-01

Status: local implementation and latency evidence; G7 remains open

Source commit:
`7b0053cdef414e411031537aa8870c05e11b1e1b`

Source tree:
`50a0991fcd781d427a77485219a94082689b363d`

Branch: `main`

## Change

The cached B+tree point route no longer constructs a `BTreeSet` or owned
key/value vectors while traversing each node. It now:

1. retains visited page IDs in a fixed 64-entry stack array;
2. parses and validates each verified immutable node in place;
3. continues validating the complete node even after the target key matches;
4. returns the leaf value as a range pinned by its `Arc<PageFrame>`; and
5. validates the row through `RowRecordView` without materializing column
   vectors before copying the selected logical value.

The public owned lookup remains available and is built over the pinned route.
The runtime's direct relational point path consumes the pinned value directly.
No `unsafe` code, mmap, external storage engine, or protocol hop was added.

The v1 node format is still sequential. Borrowed lookup performs a linear pass
inside each visited node; it does not claim a binary-searchable slotted-page
layout. The final public API still returns an owned `Vec<u8>`.

## Correctness evidence

The B+tree test set grew to seven tests. The added corruption case constructs
a page whose first key matches but whose later key violates strict ordering;
the borrowed lookup reads the complete leaf and rejects it with
`NoncanonicalKeyOrder`. The 1,000-entry recursive-split test also reads a
pinned value through an internal level.

`RowRecordView` uses the same exact length, flag, row identity, version window,
null-bitmap, offset, null-byte, and tombstone invariants as owned
`RowRecord::decode`. The owned decoder is now constructed from the validated
view, preventing semantic drift between both paths.

Before the source commit was created, these passed on Windows:

```text
cargo test -p hyphae-native-records -p hyphae-native-btree \
  -p hyphae-native-runtime --locked
cargo clippy -p hyphae-native-records -p hyphae-native-btree \
  -p hyphae-native-runtime --all-targets --locked -- -D warnings
```

The same tree then passed the complete Debian 13/WSL2 workspace:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The machine's known Windows Application Control block on complete-workspace
proc-macro loading remains unchanged; no security policy was weakened.

## Matched multilevel observation

The exact
[receipt](native-microsecond-smoke-borrowed-read-wsl2.json) uses the same
benchmark schema, dataset digest, 2,049 relational rows, tree height two,
one-million observations, 32 operations per timer sample, warmup,
buffer-pool configuration, concurrency, and WSL2 environment description as
the `5a73795` baseline receipt.

The optimized physical point path observed:

- p50 `0.468 us`;
- p95 `0.529 us`;
- p99 `1.136 us`;
- p99.9 `3.477 us`; and
- aggregate throughput `2,005,761 operations/s`.

Against the immediately preceding exact baseline, the observation shows:

- p50 down approximately `84.31%`;
- p99 down approximately `83.44%`;
- p99.9 down approximately `71.40%`; and
- throughput approximately `6.32x`.

This is a matched source/dataset comparison, but the two runs were sequential,
not interleaved or CPU-affinity controlled. It is strong local diagnostic
evidence, not a statistically controlled regression gate.

The result establishes a sub-microsecond batch-average p50 for a warm,
height-two physical B+tree/MVCC-row route on this machine. It does not establish
individual-operation sub-microsecond latency, scalable concurrency, cold
behavior, transport latency, saturation behavior, or interference resistance.
G7 therefore remains open.

## Next limits

- Add allocation and CPU/cache counters to confirm the mechanism quantitatively.
- Add a borrowed public result lifetime or pinned row handle when callers can
  accept it, avoiding the final value copy.
- Evaluate a new slotted node format only with exact-format migration and
  matched fanout/range evidence.
- Preserve this point-read route while adding physical version chains and
  genuinely concurrent writer admission.
