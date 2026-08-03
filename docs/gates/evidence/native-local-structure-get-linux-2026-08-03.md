# Native local structure GET evidence

Date: 2026-08-03

Status: first engine-bearing local UDS receipt; G0, G1, G6, and G7 remain open

Source branch: `codex/native-local-structure-get`

Contract commit: `dd69ef2a29d1e095669878eb5e3d003c1f9b1f2b`

Implementation commit: `e3ecdf13b19f5bde667078fc5625d382ce822dc2`

Benchmark and verification source commit:
`f6ca46b00cdadf472022d3efabe693bd3cd642dc`

Source tree: `f33b6ad2cdd5e722160df7d83a30a52ac3508702`

Harness SHA-256:
`02a4f0dbfcccdbc23d3393323fdc17b8a2533f5fe9bc3e65869265b2de52d793`

## Contract and implementation

The [frozen operation contract](../../native/local-structure-get-v1.md)
defines the first native-engine operation carried by the filesystem-backed
`HYPHLCL1` transport. The implementation adds:

- canonical binary `STRUCTURE GET`, `VALUE`, and stable `FAILURE` payloads;
- a 4,095-byte request-key ceiling that preserves the physical scalar
  namespace inside the native B+tree's 4,096-byte key limit;
- distinct missing and present-empty value encodings;
- a serial bounded session with `HELLO`/`WELCOME`, `PING`, `STRUCTURE GET`,
  and `CLOSE`;
- one server-authoritative logical-time sample per valid request;
- direct execution through `NativeDatabase::get_latest_structure`; and
- request-local failures that preserve stream and request identity without
  exposing internal error text.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime --test local_structure_get --no-run
```

It failed with `E0432` unresolved imports for the frozen local operation and
session types before they existed. The complete red log SHA-256 is
`7c84357c8c473d653b2deab7f3ff0a1df901a6c4a3ea803e2cceadafbc721fe2`.
An unrelated private-import setup error was corrected before this gate and is
excluded from the claim.

## Correctness and failure paths

The five integration tests cover:

- golden request, missing, present-empty, present, and failure bytes;
- every truncated boundary plus unsupported version, reserved bytes, unknown
  opcode/tag/failure code, length divergence, and noncanonical missing value;
- exact-limit and one-past-limit binary keys;
- a session payload limit too small for operation headers;
- an invalid first frame returning `FAILURE(InvalidHandshake)` without
  sampling the engine clock;
- live, missing, present-empty, and exactly expired physical scalar values;
- malformed, oversized-key, response-too-large, and unexpected-frame
  failures; and
- a successful request after each request-local failure on the same
  connection with exact stream and request IDs.

The session uses bounded reusable frame and response buffers. The current
physical point read returns an owned value and may allocate it. Allocation-free
inline response construction remains an explicit follow-up.

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

This is direct Linux execution. WSL is not part of the build, test, benchmark,
or Git path.

## Workload

Each independent run creates a fresh native database, commits 2,048 scalar
keys with 64-byte values under memory durability, verifies a physical B+tree
height of two, and reads key 1,024. The deterministic dataset BLAKE3 is
`03c89426c3fed14727a064336a560a3789da1b47a2bbee8edfe97d9bced67350`.

The harness measures three surfaces separately after 10,000 warmups each:

1. embedded physical `get_latest_structure`;
2. a persistent 32-byte `PING` round trip; and
3. a persistent `STRUCTURE GET` returning 64 bytes.

Each surface has 100,000 single-call observations. The frame payload ceiling
is 128 bytes.

## Latency observations

### Embedded physical structure GET

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.830 us | 1.167 us | 1.588 us | 4.876 us | 23.300 us | 1,107,481.721/s |
| 2 | 0.814 us | 0.865 us | 0.900 us | 4.521 us | 14.671 us | 1,170,610.745/s |
| 3 | 0.816 us | 1.355 us | 1.672 us | 10.540 us | 64.643 us | 1,064,555.406/s |
| Median statistic | 0.816 us | 1.167 us | 1.588 us | 4.876 us | 23.300 us | 1,107,481.721/s |

### Persistent PING round trip

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.338 us | 29.272 us | 35.457 us | 44.376 us | 137.531 us | 43,334.563/s |
| 2 | 23.342 us | 29.400 us | 35.716 us | 45.270 us | 813.011 us | 42,804.140/s |
| 3 | 23.318 us | 28.972 us | 35.291 us | 44.222 us | 170.191 us | 43,543.864/s |
| Median statistic | 23.338 us | 29.272 us | 35.457 us | 44.376 us | 170.191 us | 43,334.563/s |

### Persistent engine-bearing STRUCTURE GET

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.503 us | 29.102 us | 35.663 us | 45.158 us | 966.953 us | 42,763.566/s |
| 2 | 23.437 us | 29.047 us | 36.190 us | 45.807 us | 1,108.009 us | 43,285.993/s |
| 3 | 23.466 us | 29.007 us | 35.939 us | 45.557 us | 197.555 us | 42,902.518/s |
| Median statistic | 23.466 us | 29.047 us | 35.939 us | 45.557 us | 966.953 us | 42,902.518/s |

Raw JSON SHA-256:

- run 1:
  `b82161fe1bd9fedfa087d90072f134cc985de7c6aa9a699131fba2c956c13f38`;
- run 2:
  `62b84df7dab796bfb768c1b019830e3303c22719c5ade56e8eb3c4f7905e7edd`;
  and
- run 3:
  `c5ef638b5482c93405f387628d4564a4580442f4df831d86bc17617ffa4fc2fb`.

The checked
[machine-readable receipt](native-local-structure-get-linux.json) preserves
all three observations.

The bounded observation is below both provisional targets: embedded p50/p99
`2/10 us` and native local-protocol p50/p99 `25/100 us`. This does not pass G7:
the virtualized, warm, concurrency-one receipt lacks the required
million-observation histogram, cold state, concurrency 8/32, saturation,
background interference, allocation/RSS, hardware counters, stable dedicated
hardware, and Windows lane.

The median `PING` and engine-bearing distributions are reported independently.
Their p50 difference is not asserted as execution cost because independent
percentiles and sequential measurement order are not subtractive. The
millisecond-scale GET maximum in run 2 is visible rather than discarded; the
receipt captured no scheduler or hardware counters with which to assign a
cause.

## Verification

The direct-Linux validation records:

- `cargo fmt --all -- --check`;
- the five-test `local_structure_get` integration target;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of `uds_structure_get_smoke`;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace run passed 642 tests with zero failures and one ignored test.
That includes 332 `hyphae-native-runtime` unit tests and the five new
integration tests. Documentation validation covered 235 Markdown files and
12 JSON examples.

Validation-log SHA-256:

- workspace tests:
  `46fd2a3bb541b694098c168a005aa398b1f160c75255a5814a915079a63dae37`;
- targeted tests:
  `1a41c2e7567447bc097e69346ea21d2da86d8a593722e0edad149042c04e81de`;
- Clippy:
  `ed64f077998dfde6486a0fa19ac6ee33e941161ac1ad62e42cd99f646b5364ae`;
- Rustdoc:
  `998c246adeda48d6067910eb610a6568bc666e825e054371c4c153eaf7a91888`;
- release example check:
  `92434e198f27984c035c376ec77a7cf9e6ebdde782f8ccb1336ae3329b66ddb7`;
- documentation:
  `71295b75526afcbe5932079f02b0955f3db2e9bb974e00b8a5bf5b8b96d5f284`;
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

This receipt proves one native physical engine read through the local
transport and removes the previous engine-operation deficit from the first
UDS vertical. It does not implement `SET`, transport-level TTL operations,
SQL, search, transactions, the complete handshake, authorization,
multiplexing, flow control, cancellation, Windows named pipes, cold or
saturated performance, physical durability, or a regression threshold. It
closes neither G0, G1, G6, nor G7.
