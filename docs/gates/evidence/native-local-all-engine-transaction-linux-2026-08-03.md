# Native local all-engine transaction evidence

Date: 2026-08-03

Status: minimal explicit local transaction slice passed; G0 through G8 remain
open

Source branch: `codex/native-local-all-engine-transaction`

Stack base: `codex/native-local-sql-select`

Contract commit: `bbcb69a48617e8bca4301b1ef6f1f7bcec68fabf`

Implementation commit: `30d53da2e6e7ebf1ce790b9923c15d47dbf6603`

Benchmark commit: `dd10289fea97ce160fbfd435bdbc819ebdcb7c50`

Gate-hardening commit: `2f8fcb946233d710fdeb7c42e680120ead154f9d`

Verified source tree before this evidence:
`d236d47875060125dc690c8bd6069c682eb8c8f8`

Harness SHA-256:
`d6cacc86bacda87041b33e7118236fbdf20a7d6a7cc4b3c9162f0a9e9dd103d7`

Release harness binary SHA-256:
`3d3c7f3a62601c2193dac511b3219b1cebd5165f8f9153dcd2591093749062eb`

## Contract and implementation

The [frozen contract](../../native/local-all-engine-transaction-v1.md)
exposes one explicit serial transaction over the existing detached
`NativeWriteBatch`. It does not add another coordinator or retain a borrowed
`NativeTransaction`.

One session can now:

- `BEGIN` one memory or strict transaction and receive a connection-local
  nonzero handle, immutable read CSN, and one server-clock sample;
- stage bounded SQL `INSERT`/`UPDATE`/`DELETE`, scalar `SET`, and lexical
  document indexing operations;
- receive one-based stage ordinals and SQL affected-row counts;
- `COMMIT` the exact expected operation count through the existing optimistic
  validation, WAL transaction, root-set publication, and one CSN;
- `ROLLBACK` without allocating a durable transaction identity; and
- discard the complete private batch on canonical `CLOSE` or peer loss.

Normal SQL preparation/execution, structure reads, and search reads fail with
`TransactionActive` while the transaction is open. There is no invented
read-your-writes behavior. Handles are session-local `u64` values and remain
separate from durable WAL `TransactionId` values.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime \
  --test local_all_engine_transaction --no-run --locked
```

Before the public codecs and session state existed it reached `rustc`, exited
`101`, and failed with `E0432` unresolved imports. The red log SHA-256 is
`141da2c951768c02a8d1497a40d38def0ea596c6c5c52aa319ee3212cc1fa1de`.

## Correctness and failure paths

The 21 transaction integration tests cover:

- exact golden bytes for BEGIN, BEGUN, the three engine mutations, STAGED,
  COMMIT, COMMITTED, ROLLBACK, ROLLED_BACK, and failure codes 13 through 18;
- every fixed-frame truncation and trailing-byte boundary plus transaction-
  specific version, opcode, tag, engine, durability, reserved, handle,
  transaction-ID, CSN, count, UTF-8, scalar, length, TTL, and frame-bound
  rejection;
- exact 65,536/65,537-byte statement/document, 1,024/1,025-parameter/
  operation, 4,095/4,096-byte scalar-key, and 4,079/4,080-byte document-ID
  boundaries;
- idle and active state transitions, unsupported group durability,
  wrong-handle, duplicate-BEGIN, empty-COMMIT, and expected-count failures;
- a semantic SQL failure followed by successful reuse of the same batch with
  ordinal one;
- response-capacity preflight before clock/batch creation and before commit,
  with the active batch still available for rollback;
- exactly one clock sample at BEGIN and relative TTL from that fixed time;
- explicit rollback, automatic close rollback, and peer-loss rollback;
- the exact 1,024-operation admission bound without a partially staged 1,025th
  operation;
- one strict SQL + structure + search commit whose prior snapshot sees none
  of the writes, whose receipt carries one durable transaction ID and CSN,
  and whose reopened current snapshot sees all three writes;
- optimistic conflict with the complete loser absent; and
- all seven deterministic commit interruption boundaries.

For each interruption at blob stage, blob promotion, page append, page
synchronization, WAL append, WAL synchronization, or root publication, reopen
returns either the complete prior root set or the complete new root set.
No relational/structure/search mixture was observed.

This is in-process deterministic crash injection. Existing independent
process-kill and block-layer replay evidence covers the underlying singleton
coordinator, but this slice does not claim a fresh process-kill matrix driven
through the local session.

## Direct-Linux environment

- canonical host `mario@10.77.10.10`;
- canonical repository `/home/mario/celiumsai/hyphae`;
- AWS EC2 `m6i.2xlarge` in `us-east-1`;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, KVM;
- Intel Xeon Platinum 8375C @ 2.90 GHz, 8 vCPUs/4 visible cores;
- 30 GiB RAM and no swap;
- ext4 on `/dev/nvme0n1p1` over EBS, mounted with
  `rw,relatime,discard,errors=remount-ro,commit=30`;
- Rust 1.96.0 (`ac68faa20`), `x86_64-unknown-linux-gnu`, release profile; and
- client and server restricted together to logical CPUs 2 and 3.

WSL is not in the edit, build, test, benchmark, Git, or evidence path.
Non-interactive SSH commands source `/home/mario/.cargo/env`.

## Benchmark method

The release harness uses one persistent UDS connection, one client thread,
one server thread, concurrency one, a 512-byte payload ceiling, and warm
state.

It measures independent distributions:

1. a persistent 32-byte PING round trip;
2. SQL DML staging, with BEGIN and rollback outside the timed interval;
3. scalar SET staging in the same private batch;
4. lexical document staging in the same private batch;
5. only the memory COMMIT round trip after all three operations are staged;
   and
6. only the strict COMMIT round trip after all three operations are staged.

PING uses 10,000 warmups and 100,000 observations. Each stage uses 1,000
warmups and 10,000 observations. Each durability class uses 16 warmups and
256 commits.

The commit workload updates one stable relational primary key, overwrites one
stable structure key with a changing 32-byte value, and appends one lexical
document. Lexical identities grow because the current native search API
treats document IDs as immutable and rejects replacement as a duplicate.
This limitation remains visible rather than being bypassed.

## Latency observations

### P50 by run

| Surface | Run 1 | Run 2 | Run 3 | Median statistic |
|---|---:|---:|---:|---:|
| PING 32 B | 23.897 us | 23.817 us | 23.853 us | 23.853 us |
| SQL stage | 24.508 us | 24.415 us | 24.322 us | 24.415 us |
| Structure stage | 22.496 us | 22.364 us | 22.305 us | 22.364 us |
| Search stage | 24.371 us | 24.271 us | 24.227 us | 24.271 us |
| Memory commit | 6.476 ms | 6.489 ms | 6.455 ms | 6.476 ms |
| Strict commit | 15.064 ms | 15.097 ms | 15.140 ms | 15.097 ms |

### Median run statistics

| Surface | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---|---:|---:|---:|---:|---:|---:|
| PING 32 B | 23.853 us | 24.904 us | 36.983 us | 40.367 us | 144.417 us | 42,182.341/s |
| SQL stage | 24.415 us | 26.099 us | 39.321 us | 46.231 us | 49.234 us | 42,138.760/s |
| Structure stage | 22.364 us | 23.549 us | 36.449 us | 40.034 us | 56.501 us | 46,685.786/s |
| Search stage | 24.271 us | 25.330 us | 38.524 us | 41.940 us | 50.997 us | 41,487.502/s |
| Memory commit | 6.476 ms | 11.242 ms | 11.656 ms | 11.827 ms | 11.957 ms | 151.680/s |
| Strict commit | 15.097 ms | 19.790 ms | 20.275 ms | 20.728 ms | 21.180 ms | 66.261/s |

Raw JSON SHA-256:

- run 1:
  `f9bba235d310ff9323af6e7e8f3d5d98379cf41f94eeff6155063c7fd588852e`;
- run 2:
  `9d8e6387e711978ae86b9effc49d7345cd05d325b1b80b3d27603c372261d578`;
  and
- run 3:
  `93c9fb5c687037fe5201424c8b0ac8fbf4586ae0ff5ca1d6288688f1270762ad`.

The checked
[machine-readable receipt](native-local-all-engine-transaction-linux.json)
preserves all observations.

The three staging routes remain in the microsecond domain and sit near the
independently measured PING distribution. No percentile is subtracted from
another and no negative overhead is claimed.

Neither commit route meets the microsecond objective. Memory durability
excludes `fsync` but still pays validation, physical copy-on-write page/WAL
append, and root publication. Strict durability adds page and WAL
synchronization over ext4/EBS. The result is a measured deficit and the next
performance target, not a G7 pass.

## Verification

The final direct-Linux validation runs:

- `cargo fmt --all -- --check`;
- the 21 transaction integration tests;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of all six UDS smoke examples;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The funnel passed 685 tests with zero failures and one ignored test across 82
result blocks. The targeted transaction suite passed 21 tests. Documentation
validation covered 243 Markdown files and 12 JSON examples.

Validation-log SHA-256:

- workspace tests:
  `72b32b03c97f1c6f68d44e6069fb8c2f4eb30b5f99df41bd694edb5b4f0c175c`;
- targeted transaction tests:
  `731adf3778d5b1f2f272f64247573a895b26dcd0e8162c7b3ea9b1d8a7cff94f`;
- Clippy:
  `c7eddbc8e451ff77a1882d929e7cbfdc5a7221c366ae1b7f69073560b7960ebb`;
- Rustdoc:
  `d6d1461a68927a50eeb3067ba696850ee710e95a63b6d9af3b668751cbe0b72a`;
- six release examples:
  `5aa7a40260e3ee8d766193100f4c34e6039d5e9cb4b282739300dfd1dd1295b5`;
- documentation:
  `c83e176d1b9992e91a76791d778eebc67a27c3f60aa40e1da5448624643e0e76`;
- formatting:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
  and
- diff check:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

Hosted Linux, macOS, Windows, dependency, security, fuzz, and stress lanes
remain PR evidence. Mutation testing is not claimed because the repository
has no accepted mutation tool, operator set, or surviving-mutant threshold
for this slice.

## Boundary

This evidence proves the minimal explicit local all-engine transaction and
advances the G1/G5/G6 vertical. It does not close G0, G1, G2, G3, G4, G5, G6,
G7, or G8.

Still absent are prepared DML, DDL in the local transaction, private reads,
savepoints, isolation-level selection, deallocation, concurrent transactions
on one connection, group durability, multiplexing, retry tokens, lexical
document replacement/delete, a local-session process-kill matrix, Windows
named pipes, cold/saturated/concurrent performance, allocation/RSS, hardware
counters, and complete cross-engine SQL joins, backup, and restore.
