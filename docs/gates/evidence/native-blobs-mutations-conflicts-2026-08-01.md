# Native blobs, relational mutations, and conflict substrate evidence

Date: 2026-08-01

Status: local implementation evidence; advances G0/G1 and the first relational
slice, but closes no phase-1 gate

Source commit:
`5a7379527a7207142de81d84a52699d2f0de92c1`

Source tree:
`0c64a2a3416be24a1906c6e850e5e20e9a4fb4a9`

Branch: `main`

## Implemented scope

The exact source commit adds the Hyphae-owned `hyphae-native-blobs` crate and
connects it to the convergence runtime. Values above 8,192 bytes are written
as immutable content-addressed files, while row and WAL values retain the
fixed 56-byte reference. The file binds `BlobId`, logical length, CRC32C, and
BLAKE3 content identity. Strict publication stages a create-new file, syncs
it, renames it into `blobs/`, and syncs parent directories on Unix.

The root set, immutable root manifest, and 124-byte WAL commit body now bind
the verified blob generation. Reopen verifies every complete blob, removes
canonical interrupted stages, rejects a committed generation beyond physical
state, and resolves every referenced row value through full identity
verification.

The exact binary SQL slice now supports:

```sql
UPDATE accounts SET row = ? WHERE primary_key = ?
DELETE FROM accounts WHERE primary_key = ?
```

UPDATE publishes a replacement `RowRecord` under a new copy-on-write root.
DELETE publishes a canonical tombstone. Snapshots retaining the prior roots
continue to return the prior values after later update and delete commits.
Reopen resolves the final tombstone.

Committed WAL mutation bodies are now decoded rather than merely length
checked. A `ConflictTable` maps canonical point-write keys to their latest
committed CSN, rejects a same-key writer whose snapshot is stale, admits
disjoint keys, and is reconstructed from digest-verified committed WAL.
Catalog creates also claim object-ID and engine-qualified name identities.

## Boundaries that this evidence does not cross

The database still accepts write transactions through exclusive `&mut`
access, and the MVCC coordinator holds one writer guard for the transaction.
Consequently the conflict table is first-committer-wins substrate, not
evidence of two concurrently executing writers.

Physical update/delete history is retained through immutable historical roots.
The current root does not yet contain an explicit per-key version chain and
the superseded row record keeps its open-ended `end_csn`. Vacuum, root
retention, version-chain traversal, and snapshot-safe blob garbage collection
remain pending.

A blob promoted before its transaction reaches WAL can remain as an immutable
unreferenced orphan. Its bytes cannot become a visible row, but reclamation is
not implemented. Streaming/chunk trees, compression, encryption, and large
corpus GC are also outside this slice. Windows safe Rust still cannot claim a
strict parent-directory flush.

## Exact format and failure evidence

The complete blob-file golden has BLAKE3:

```text
9a0f3fe1cb0a72d63a0e4aa9bf00dd22c99e8439f5bd18f18414f09a405e42da
```

The focused tests cover:

- complete blob round trip, content deduplication, and reopen;
- canonical temporary-stage recovery;
- fail-closed complete blob corruption;
- exact blob-file encoding;
- large relational value commit, direct read, snapshot read, checkpoint, and
  reopen;
- interruption after blob staging and after blob promotion;
- UPDATE and DELETE through SQL, historical snapshots, current-root tombstone,
  and reopen;
- conflict rejection for a stale same key, admission for a disjoint key,
  monotonic/idempotent conflict publication, and reconstruction from WAL; and
- the expanded seven-boundary commit matrix: blob staged, blob promoted, page
  appended, page synchronized, WAL appended, WAL synchronized, and root
  published.

These are deterministic in-process interruption tests. They are not
sector-level power-loss, filesystem-reordering, or independent restore
evidence.

## Validation

On the source commit's tree before the evidence files were added:

```text
cargo test -p hyphae-native-btree -p hyphae-native-mvcc \
  -p hyphae-native-blobs -p hyphae-native-runtime --locked
```

passed 6 B+tree, 7 MVCC, 4 blob, and 19 runtime tests plus doc tests.

```text
cargo clippy -p hyphae-native-btree -p hyphae-native-mvcc \
  -p hyphae-native-blobs -p hyphae-native-runtime \
  --all-targets --locked -- -D warnings
```

passed on Windows.

The complete workspace then passed under Debian 13 on WSL2:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The Windows complete-workspace test remained blocked before test execution by
the machine's Application Control policy while loading `serde_derive`, with
`os error 4551`. The policy was not weakened. Focused native binaries did
execute on Windows.

The native runtime dependency tree contains only Hyphae workspace crates plus
`blake3`, `crc32c`, and `thiserror` as runtime dependency families. A
case-insensitive target-crate scan found no forbidden engine dependency or
direct `unsafe` implementation; its two product-name hits are explicit
non-compatibility statements in the runtime README and crate documentation.
This is not the still-required transitive unsafe and license audit.

The relative-link check passed for all 133 tracked Markdown files after adding
the blob crate README.

## Multilevel latency observation

The exact
[WSL2 receipt](native-microsecond-smoke-multilevel-wsl2.json) uses 2,049
relational rows and refuses to measure unless complete validation reports a
B+tree height of at least two. The direct route traverses an internal node and
a leaf through the partitioned buffer pool, decodes the MVCC row, and returns
an owned value.

The physical multilevel point lookup observed:

- p50 `2.983 us`;
- p95 `4.108 us`;
- p99 `6.858 us`;
- p99.9 `12.156 us`; and
- aggregate throughput `317,504 operations/s`.

This remains in the requested microsecond domain and does not introduce a
millisecond internal hop. It is also materially slower than the historical
one-row/one-leaf observation. That comparison is diagnostic, not a controlled
regression, because both the corpus and benchmark source changed.

Source inspection identifies three immediate hot-path costs: a fresh
cycle-detection `BTreeSet`, complete owned decoding of each node, and owned row
and value copies on each direct read. Allocation counters and hardware
counters were not collected, so these are grounded hypotheses rather than
measured attribution. The next physical optimization should introduce
validated zero-copy node views, allocation-free bounded traversal, and pinned
row/value views before expanding the SQL executor onto this path.

The receipt remains `observation-not-gate`: it batch-averages 32 operations,
uses concurrency one and WSL2, covers only height two, and lacks transport,
saturation, interference, affinity, allocation, and hardware-counter controls.

## Remaining critical path

- explicit current-root MVCC version chains, closed intervals, retention, and
  vacuum;
- genuinely concurrent writers and isolation/constraint race litmus tests;
- range cursors, secondary indexes, typed rows, constraints, joins, spill, and
  the complete SQL planner/executor;
- scalable physical structure and search engines in place of bounded
  single-page state;
- blob streaming, reference tracing, orphan collection, encryption, and
  backup/restore;
- bounded checkpoint replay, WAL retention/truncation, group commit, and
  physical power-loss testing; and
- the controlled performance matrix required by G7.
