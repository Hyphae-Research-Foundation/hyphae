# Native local structure SET and TTL evidence

Date: 2026-08-03

Status: first native local mutation and TTL receipt; G0, G1, G3, G6, and G7
remain open

Source branch: `codex/native-local-structure-set-ttl`

Contract commit: `9666b5a946dccc8da2939e0846aea99ce983c431`

Receipt-width correction commit:
`b01d6e17d798a12f55727d6ba9e363819d1e3791`

Implementation commit: `253e11e5dc89c41f2369af4c5a366287c21aca53`

Benchmark source commit: `8ce45eb91d5ee87c296204685f1a82d5140515ef`

Source tree: `5a58f60010508daf306e949716f37ec904866bbc`

Harness SHA-256:
`edfc651f42e4f2407d3c559b15d3555099899f6ed6d0ff5ad1e0b2186f62d5ca`

## Contract and implementation

The [frozen operation contract](../../native/local-structure-set-ttl-v1.md)
extends the serial `HYPHLCL1` session with:

- canonical binary `SET`, `TTL`, commit-receipt, TTL-value, and stable failure
  payloads;
- strict and memory durability admission, with group durability rejected until
  native scheduler integration;
- server-authoritative relative TTL with checked absolute-time conversion;
- receipt-capacity preflight before clock sampling or transaction creation;
- one native transaction and one CSN per accepted `SET`;
- exact `u128` transaction ID, `u64` CSN, and acknowledged durability in the
  commit receipt;
- request-local failure recovery without internal error disclosure; and
- strict close/reopen equivalence for both value and expiry.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime --test local_structure_set_ttl --no-run
```

It failed with `E0432` unresolved imports for the frozen SET/TTL codecs and
general structure session before they existed. The red log SHA-256 is
`49deec64b2f6587a9a2e92964e9063d75620f90c461d286192ebac52e923f5c9`.

That gate also exposed that native `TransactionId` is `u128`, not `u64`. The
contract was corrected from a 20-byte to a 28-byte receipt before
implementation. No narrowing shim or alternate identity format was retained.

## Correctness and failure paths

The six new integration tests cover:

- golden SET, TTL, receipt, failure, and all TTL response bytes;
- every relevant truncated boundary plus unsupported version, opcode,
  durability, expiry mode, TTL, key/value length, trailing byte, zero identity,
  reserved byte, and noncanonical response rejection;
- exact-limit and one-past-limit keys and frame payloads;
- receipt-capacity rejection before clock sampling, transaction creation, or
  commit;
- malformed, unsupported-group, expiry-overflow, and kind-mismatch failures
  followed by successful requests on the same connection;
- memory and strict receipts with exact stream ID, request ID, transaction ID,
  CSN, and durability;
- persistent, remaining, exactly expired, and missing TTL under a controlled
  clock; and
- strict close, database reopen, and physical value/TTL equivalence.

The existing five GET integration tests were migrated to the general
`LocalStructureSession` and retained. The session stays serial and bounded;
the implementation does not simulate group commit, replay ambiguous
acknowledgements, or expose explicit transaction state.

## Direct-Linux environment

- AWS EC2 `m6i.2xlarge` in `us-east-1`, virtualized by KVM;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- Intel Xeon Platinum 8375C @ 2.90 GHz, 8 vCPUs/4 visible cores, 30 GiB RAM;
- benchmark data on ext4 over `/dev/nvme0n1p1` (EBS);
- Rust 1.96.0 (`ac68faa20`), `x86_64-unknown-linux-gnu`, release profile;
- client and server restricted together to logical CPUs 2 and 3 with
  `taskset`; the guest exposed no CPU-frequency governor; and
- one client thread, one server thread, one persistent connection, and
  concurrency one.

This is direct Linux execution at `/home/mario/celiumsai/hyphae` on
`10.77.10.10`. WSL is not part of the build, test, benchmark, or Git path.

## Workload

Each independent run creates three fresh databases:

1. a read database with 2,048 scalar keys, 64-byte values, a physical B+tree
   height of two, and a target key with a 60-second relative TTL;
2. a memory-durability SET database; and
3. a strict-durability SET database.

The deterministic read dataset BLAKE3 is
`b1fd6494926976e3604c7f42c02d47d38feca9b4423d70721915d6d79ccf2f4b`.
The harness uses a 128-byte frame payload ceiling and measures:

- embedded physical TTL: 10,000 warmups and 100,000 observations;
- persistent UDS GET with a 64-byte value: 10,000 warmups and 100,000
  observations;
- persistent UDS TTL: 10,000 warmups and 100,000 observations;
- persistent UDS memory SET with a 64-byte value and relative TTL: 1,000
  warmups and 10,000 observations; and
- persistent UDS strict SET with the same payload: 16 warmups and 256
  observations.

Every SET reuses one key but supplies a distinct deterministic value and checks
the exact transaction ID, CSN, and durability receipt. Memory and strict SET
use separate fresh databases so the first strict synchronization does not
inherit the memory-only warmup's unsynchronized state.

## Latency observations

### Embedded physical TTL

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.832 us | 0.867 us | 0.886 us | 4.505 us | 22.355 us | 1,150,779.384/s |
| 2 | 0.829 us | 0.862 us | 0.883 us | 4.549 us | 24.017 us | 1,150,483.865/s |
| 3 | 0.837 us | 0.869 us | 0.890 us | 4.701 us | 26.982 us | 1,143,617.825/s |
| Median statistic | 0.832 us | 0.867 us | 0.886 us | 4.549 us | 24.017 us | 1,150,483.865/s |

### Persistent STRUCTURE GET control

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.546 us | 23.971 us | 33.797 us | 37.755 us | 121.342 us | 42,827.937/s |
| 2 | 23.494 us | 23.926 us | 34.286 us | 37.684 us | 105.212 us | 43,571.268/s |
| 3 | 23.618 us | 24.047 us | 34.418 us | 37.863 us | 119.277 us | 42,571.393/s |
| Median statistic | 23.546 us | 23.971 us | 34.286 us | 37.755 us | 119.277 us | 42,827.937/s |

### Persistent STRUCTURE TTL

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.487 us | 23.905 us | 34.489 us | 37.651 us | 122.774 us | 42,717.137/s |
| 2 | 23.479 us | 23.886 us | 34.600 us | 37.765 us | 119.892 us | 43,106.106/s |
| 3 | 23.540 us | 23.963 us | 34.342 us | 37.784 us | 633.714 us | 42,677.201/s |
| Median statistic | 23.487 us | 23.905 us | 34.489 us | 37.765 us | 122.774 us | 42,717.137/s |

### Persistent STRUCTURE SET with memory durability

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 377.986 us | 396.671 us | 401.059 us | 479.580 us | 601.276 us | 2,611.466/s |
| 2 | 377.113 us | 396.345 us | 409.379 us | 602.590 us | 668.837 us | 2,612.986/s |
| 3 | 376.722 us | 395.668 us | 400.902 us | 497.677 us | 647.865 us | 2,618.620/s |
| Median statistic | 377.113 us | 396.345 us | 401.059 us | 497.677 us | 647.865 us | 2,612.986/s |

### Persistent STRUCTURE SET with strict durability

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 6,731.272 us | 6,886.954 us | 6,940.559 us | 6,960.779 us | 6,961.613 us | 148.537/s |
| 2 | 6,744.689 us | 6,870.519 us | 6,903.161 us | 6,939.291 us | 6,959.673 us | 148.359/s |
| 3 | 6,738.743 us | 6,878.384 us | 6,928.074 us | 6,947.070 us | 7,078.042 us | 148.531/s |
| Median statistic | 6,738.743 us | 6,878.384 us | 6,928.074 us | 6,947.070 us | 6,961.613 us | 148.531/s |

Raw JSON SHA-256:

- run 1:
  `2b65f72bcc45a5bb795e911e96b2787bab2c22c54916b6246090cb10eb3fd883`;
- run 2:
  `735fa0bdfdc4290fd7e06aae4274bbf2518f9562d428c9c52398b664d68a91b9`;
  and
- run 3:
  `59887b6bbb26d7122fd162d3790b29c51a4facf9746c754d478f809864b5edd6`.

The checked
[machine-readable receipt](native-local-structure-set-ttl-linux.json)
preserves all three observations.

The bounded read observations remain in the microsecond domain. Memory SET is
hundreds of microseconds and strict SET is milliseconds. Strict includes page
and WAL synchronization; memory excludes that physical durability promise but
still does not satisfy a microsecond mutation objective. This is a measured
deficit, not a gate pass.

No independent percentile is subtracted from another to infer execution,
transport, or synchronization cost. The 633.714-microsecond TTL maximum in run
3 and the strict maxima remain visible. The receipt has no scheduler or
hardware counters with which to assign their causes.

## Verification

The final direct-Linux validation records:

- `cargo fmt --all -- --check`;
- the GET and SET/TTL integration targets;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of both UDS structure smoke examples;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace run passed 648 tests with zero failures and one ignored test.
That includes 332 `hyphae-native-runtime` unit tests, five GET integration
tests, and six SET/TTL integration tests. Documentation validation covered 237
Markdown files and 12 JSON examples.

Validation-log SHA-256:

- workspace tests:
  `1c096c7b85320043faae7f95fcae20d387b9193e3a5e89a22d33d8458e27c1f7`;
- targeted integration tests:
  `517da8da7dc14fd15a1cf378a4f5557d1067465e09fdfc94b5b22dea7e71475c`;
- Clippy:
  `bfc0236e7664df6e9da3ee14f45d7beda0de304f3e2c8a1d6f505794b29e78c7`;
- Rustdoc:
  `af6664beb508e112d56c7fcfe072f2671d93d067c519fb03b111aff4cc0666dc`;
- release example checks:
  `e84a41aa560f77c20cd003db76ada1536570cc444b576f391dce05cbcebb684c`;
- documentation:
  `6ee0b7b3c51d77da4a17c3d88e817f3716dbc732503cf8533d279d0789940196`;
- formatting:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`;
  and
- diff check:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

Hosted Linux, macOS, Windows, dependency, security, fuzz, and stress lanes
remain PR evidence. Mutation testing was not executed because the repository
has no accepted mutation tool, operator set, or surviving-mutant threshold
for this slice.

## Boundary

This receipt proves native scalar `GET`, `SET`, and TTL semantics over the
filesystem-backed Unix local transport, including strict reopen proof. It does
not implement explicit `EXPIRE`/`PERSIST`, conditional or batched mutation,
group scheduling, replay/idempotency, explicit local transactions, SQL,
search, complete handshake/authorization/multiplexing/cancellation, Windows
named pipes, cold or saturated performance, timing decomposition, a
regression threshold, or a literal device power cut. It closes neither G0,
G1, G3, G6, nor G7.
