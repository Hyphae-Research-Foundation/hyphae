# Native optimistic-writer evidence

Date: 2026-08-01

Status: concurrent transaction execution with serialized publication; G1 and
G7 remain open

Source commit:
`a91538fdc99d434c380b07b2c76c5eaca374220f`

Source tree:
`92bad04e95ea8ec9e352e2eb01a282753572ad83`

Branch: `main`

## Change

`NativeWriteBatch` owns an immutable snapshot, materialized private state, raw
mutations, dirty-engine flags, and durability class. It owns no page, blob, or
WAL handle and acquires no writer guard. `begin_optimistic(&self, ...)` can
therefore prepare multiple transactions concurrently.

`commit_optimistic(&mut self, batch)` then:

1. acquires serialized writer admission;
2. validates canonical write keys against the batch's original `read_csn`;
3. loads the root set current at admission;
4. reapplies the complete mutation sequence to that current all-engine state;
5. runs the existing blob, page, WAL, and root-publication pipeline; and
6. publishes one CSN or returns a first-committer-wins conflict.

Reapplication is required because structure and search roots currently encode
whole bounded states. Publishing the batch's stale private state would lose an
intervening disjoint write. The rebase covers relational rows, structure keys,
catalog creates, search collection creates, and indexed documents.

Recovery no longer requires every WAL `read_csn` to equal the immediately
previous commit. Commit CSNs remain contiguous, while an original read CSN may
be genesis or any existing earlier CSN. Recovery rebuilds the conflict table
in order and rejects a stale same-key history.

The older `begin`/`NativeTransaction` API remains compatible and delegates its
mutation surface to the same batch type. It still retains writer admission for
the full transaction lifetime.

## Concurrency and conflict evidence

The principal litmus uses two scoped OS threads. Both finish snapshot capture
against CSN 1 before a barrier releases their private mutations. One batch
updates a relational row, a structure key, and a search document; the other
inserts disjoint identities in all three engines. Serialized commits receive
CSNs 2 and 3. The second commit rebases on the first, and all six changes remain
visible after reopen.

Two later batches read CSN 3 and update the same relational row. The first
receives CSN 4; the second returns `WriteConflict` before persistence. Reopen
finds exactly four committed transactions and the winner's value.

A separate genesis litmus prepares two disjoint structure writes with
`read_csn = None`, commits them at CSNs 1 and 2, and proves recovery preserves
both. This covers the lagging-read-CSN rule at its lower boundary.

The optimistic path also executes the seven deterministic interruption points:
blob staged, blob promoted, page appended, page synchronized, WAL appended, WAL
synchronized, and root published. Reopen observes either the complete prior
CSN or the complete committed CSN.

This is real concurrent transaction preparation and private execution, but
not simultaneous commit submission. `commit_optimistic` still requires
exclusive `&mut NativeDatabase`, and publication plus durability I/O remain
inside one writer guard.

## Validation

The focused Windows run passed seven MVCC tests and 25 runtime tests. Strict
Clippy passed for both crates:

```text
cargo test -p hyphae-native-mvcc -p hyphae-native-runtime --locked
cargo clippy -p hyphae-native-mvcc -p hyphae-native-runtime \
  --all-targets --locked -- -D warnings
```

The complete Debian 13/WSL2 workspace then passed:

```text
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Formatting, whitespace, receipt JSON, and relative Markdown links were checked
separately.

## Read-path regression observation

The exact
[receipt](native-microsecond-smoke-optimistic-writers-wsl2.json) records two
immediate runs because the first run slowed several untouched operations. The
height-two physical relational read observed:

| Run | p50 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|
| first | `1.036 us` | `2.574 us` | `5.558 us` | `924,432/s` |
| immediate repeat | `0.843 us` | `2.414 us` | `5.790 us` | `1,098,722/s` |
| prior version-chain receipt | `0.841 us` | `2.422 us` | `4.942 us` | `1,088,895/s` |

The immediate repeat differs from the prior source by `+0.24%` at p50,
`-0.33%` at p99, `+17.16%` at p99.9, and `+0.90%` throughput. Because neither
affinity nor background load was controlled and the two current runs vary
materially, this does not establish a performance improvement or regression.
It does show that the refactor preserved the microsecond direct-read regime.

This smoke does not measure optimistic prepare, validation, rebase, WAL, or
commit latency. It is not a writer-concurrency benchmark and does not close
G7.

## Next limits

- Put mutable publication resources behind an owned admission component so
  multiple clients can submit commits through `&self` while publication stays
  serialized and fail-closed.
- Add a controlled writer benchmark for prepare, conflict abort, disjoint
  rebase, memory commit, strict commit, queueing, saturation, and p99.9.
- Replace eager all-engine snapshot materialization with lazy per-engine
  snapshot views before scaling relations, structures, postings, or segments.
- Add randomized snapshot-isolation histories and model-check publication
  ordering before claiming a complete concurrency gate.
