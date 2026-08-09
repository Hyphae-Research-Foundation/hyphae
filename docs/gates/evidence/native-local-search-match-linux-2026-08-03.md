# Native local SEARCH MATCH evidence

Date: 2026-08-03

Status: first search-engine local UDS receipt; G0, G1, G4, G6, and G7 remain
open

Source branch: `codex/native-local-search-match`

Contract commit: `d467de7188e49d0ce2395067c1d73c047798fb6c`

Implementation commit: `9b69318cf0928680dd7e71c1257c28eaeebdba55`

Benchmark source commit: `2c40f8ccdd9430e06502a9699c93255dcf34b96a`

Source tree: `dbe98da459727a3cd666580f6dce1e07c80dbe36`

Harness SHA-256:
`3d721a7e405794a327d42970f01d579e8305832ca985d855859a6282f6027f6f`

## Contract and implementation

The [frozen operation contract](../../native/local-search-match-v1.md) adds:

- one canonical `SEARCH MATCH` request with nonzero `u128` catalog identity,
  bounded UTF-8 query, and hit limit;
- one canonical result carrying the visible all-engine CSN, positive finite
  BM25 scores, binary document IDs, and strict score/document ordering;
- checked complete-response sizing before reusable-buffer growth;
- direct current-root physical inverted-index execution;
- request-local malformed, unknown-index, and response-too-large failures;
- exact stream/request identity preservation; and
- a general `LocalDataSession` replacing the experimental structure-only
  handle before release.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime --test local_search_match --no-run --locked
```

It failed with `E0432` unresolved imports for the frozen request/result codecs
and `LocalDataSession` before they existed. The red log SHA-256 is
`f68fd480a3987b4a6ae18a5fc3a7143e7d8d4269701db4f5620dc0c228a0dfe5`.

## Correctness and failure paths

The six MATCH integration tests cover:

- golden request, one-hit, tied-score, and binary-document-ID bytes;
- every request-header and query-body truncation boundary;
- every result-header and hit-record truncation boundary;
- unsupported version/opcode/tag, reserved bytes, zero object/CSN, invalid
  UTF-8, query length, hit limit/count, score, ordering, duplicate, record
  length, trailing bytes, and response frame bounds;
- exact 4,096/4,097-byte query and 1,024/1,025-hit boundaries;
- query-empty, missing-term, rare-term, common-term, and stable-tie physical
  equivalence;
- malformed, unknown-index, and response-too-large failures followed by
  successful requests on the same connection;
- exact visible CSN, document IDs, score bits, stream ID, and request ID;
- proof that MATCH does not sample the scalar TTL clock; and
- close, reopen, and physical-result equivalence.

The existing five GET and six SET/TTL integration tests were migrated to
`LocalDataSession` and retained.

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

Each run creates a fresh native database and commits one search index with
2,048 deterministic documents under memory durability. Every document
contains `common`; even/odd documents otherwise contain `rust`/`sql`; document
1,024 contains the unique term `needle`. The physical inverted B+tree has
height two.

The query is the six-byte rare term `needle`, limit 10. It returns one
four-byte binary document ID in a 32-byte canonical result. The deterministic
dataset BLAKE3 is
`d30066a3472d4c76c4b0aae03c6b36a4c459d566f59a10427a6883d5e7f097cd`.
The frame payload ceiling is 128 bytes.

The harness measures three surfaces independently after 10,000 warmups each:

1. embedded physical `match_latest_text`;
2. a persistent 32-byte `PING` round trip; and
3. persistent `SEARCH MATCH` with exact CSN/document/score validation.

Each surface has 100,000 single-call observations.

## Latency observations

### Embedded physical SEARCH MATCH

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.407 us | 24.038 us | 32.704 us | 39.979 us | 131.202 us | 42,139.094/s |
| 2 | 23.339 us | 23.981 us | 32.674 us | 41.687 us | 132.958 us | 42,234.559/s |
| 3 | 23.346 us | 24.080 us | 34.878 us | 57.523 us | 134.718 us | 41,939.573/s |
| Median statistic | 23.346 us | 24.038 us | 32.704 us | 41.687 us | 132.958 us | 42,139.094/s |

### Persistent PING round trip

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.358 us | 23.765 us | 33.728 us | 36.700 us | 146.892 us | 43,319.441/s |
| 2 | 23.422 us | 23.857 us | 34.321 us | 36.800 us | 122.486 us | 43,096.730/s |
| 3 | 23.246 us | 23.663 us | 32.944 us | 36.534 us | 117.784 us | 43,854.607/s |
| Median statistic | 23.358 us | 23.765 us | 33.728 us | 36.700 us | 122.486 us | 43,319.441/s |

### Persistent engine-bearing SEARCH MATCH

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 56.150 us | 64.973 us | 68.327 us | 81.589 us | 158.616 us | 17,618.058/s |
| 2 | 56.168 us | 64.943 us | 68.348 us | 74.709 us | 224.120 us | 17,642.846/s |
| 3 | 56.032 us | 64.637 us | 68.186 us | 80.429 us | 153.903 us | 17,659.204/s |
| Median statistic | 56.150 us | 64.943 us | 68.327 us | 80.429 us | 158.616 us | 17,642.846/s |

Raw JSON SHA-256:

- run 1:
  `254c09e1e7e6e999dab74b1b6ace12356113cbd465b2ef850843fc06302bedc8`;
- run 2:
  `ee79510f0d8b8d2d63708ff4c17901a491ea367b28c185d53d40fb9f77c8d950`;
  and
- run 3:
  `b9074a217803df209169afb3774cdd15e547d27a0e39dbbd5e461427529bcde5`.

The checked
[machine-readable receipt](native-local-search-match-linux.json) preserves all
three observations.

All bounded surfaces remain in microseconds. This is not a G7 pass or
regression threshold: the virtualized, warm, concurrency-one receipt lacks a
million-observation histogram, cold state, concurrency 8/32, saturation,
background interference, allocation/RSS, hardware counters, stable dedicated
hardware, and Windows named-pipe execution.

PING, physical MATCH, and complete UDS MATCH distributions are independent.
Their percentiles are not subtracted. The 224.120-microsecond UDS maximum in
run 2 remains visible; no scheduler or hardware counters assign its cause.

## Verification

The final direct-Linux validation records:

- `cargo fmt --all -- --check`;
- the GET, SET/TTL, and MATCH integration targets;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of all three UDS engine smoke examples;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace run passed 654 tests with zero failures and one ignored test.
That includes 332 `hyphae-native-runtime` unit tests, five GET integration
tests, six SET/TTL integration tests, and six MATCH integration tests.
Documentation validation covered 239 Markdown files and 12 JSON examples.

Validation-log SHA-256:

- workspace tests:
  `25cd290ffcd158903b6fcc6b3954e81687c26811a5a8ffc050050eb8dcde4710`;
- targeted integration tests:
  `40cfe16265ada3661294b156facd2eef99c1bfd87639b4fa464f3ab42f23868c`;
- Clippy:
  `db4d9b529406219198b4d5a351356849ea8392b959bb4dacaf83471a0290bc52`;
- Rustdoc:
  `1419bd75c89fdccd5995ce0e636ed262bd7f0a817b6f03103b3ac5fb66f6ffc3`;
- release example checks:
  `92434e198f27984c035c376ec77a7cf9e6ebdde782f8ccb1336ae3329b66ddb7`;
- documentation:
  `33a86b2a8cca22894ea8698500beaf6866446f4d55ce5823eef9b0cf710db4d7`;
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

This receipt proves one physical lexical engine query through the local
transport. It does not implement document mutation, fielded/boolean/phrase/
prefix search, ANN/hybrid search, aggregation, pagination/streaming, SQL,
explicit transactions, complete handshake/authorization/multiplexing/
cancellation, Windows named pipes, cold or saturated performance, a regression
threshold, or a complete G6 daemon. It closes neither G0, G1, G4, G6, nor G7.
