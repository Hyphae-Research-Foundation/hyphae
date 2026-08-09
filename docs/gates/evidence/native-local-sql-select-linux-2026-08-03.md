# Native local SQL SELECT evidence

Date: 2026-08-03

Status: first relational-engine local UDS receipt; G0, G1, G2, G5, G6, and G7
remain open

Source branch: `codex/native-local-sql-select`

Contract commit: `83de5e8edd665094175a8a3f938c369cc1a68d3f`

Implementation commit: `642dbdbdec611725c1be3523961bee2115c10df2`

Benchmark source commit: `2bf6d341c31c9b2acaef5c3087515d3d7fe42220`

Source tree: `6f2324a860f0e117c14b4d6f2b2018c761bee66c`

Harness SHA-256:
`8df8ce5c4ec1ffd6fd7efb5169bd8a7ea6453b94253543622dd79cf90bfc855a`

## Contract and implementation

The [frozen operation contract](../../native/local-sql-select-v1.md) adds:

- canonical `PREPARE SELECT` and prepared `EXECUTE` payloads;
- bounded session-local plans, typed parameters, logical schemas, and rows;
- direct current-root primary, secondary, bounded-scan/range, and indexed-join
  execution;
- the visible all-engine CSN from the immutable root set used by execution;
- stable SQL-invalid, parameter, stale-catalog, resource, and unknown-plan
  failures;
- checked complete-response sizing before reusable-buffer growth; and
- request-local recovery without discarding earlier prepared plans.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime --test local_sql_select --no-run --locked
```

It failed with `E0432` unresolved imports for the frozen SQL codecs and
session behavior before they existed. The red log SHA-256 is
`32aed72a281259228b5ede63944527bebac90b478d0004469b6eaafe08fa9454`.

## Correctness and failure paths

The nine SQL integration tests and the stable-failure unit test cover:

- golden PREPARE, receipt, EXECUTE, result, and failure bytes;
- exact 65,536/65,537-byte statement, 1,024/1,025-parameter,
  1,024/1,025-column, 1,024/1,025-row, and 64/65-plan boundaries;
- every primitive null, boolean, signed, unsigned, decimal, floating-point,
  text, binary, date, time, timestamp, interval, and UUID parameter/result;
- every header and body truncation boundary plus version, opcode, tag,
  reserved, identity, UTF-8, count, length, type, scalar, time, float, and
  trailing-byte rejection;
- physical primary-key, unique and non-unique secondary, bounded scan/range,
  null, empty, and indexed-join equivalence;
- malformed, unknown-plan, parameter-count/type, stale-catalog, resource, and
  response-too-large failures followed by successful connection/plan reuse;
- exact stream, request, plan, catalog, visible CSN, schema, cell, and row
  identity;
- proof that SQL execution does not sample the structure TTL clock; and
- close/reopen behavior.

The existing five GET, six SET/TTL, and six MATCH integration tests also
remain green.

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
Non-interactive SSH commands source `/home/mario/.cargo/env` to select the
host's Rust toolchain.

## Workload

Each run creates a fresh native database and commits 2,048 deterministic
relational rows under memory durability. The relational B+tree has height two.
The prepared 53-byte query selects two columns by primary key with one signed
parameter and returns row 1,024.

The canonical EXECUTE request is 32 bytes and the one-row result is 104 bytes.
The frame payload ceiling is 256 bytes. The deterministic dataset BLAKE3 is
`fb1f1ab9e40e05d136a6e38d6a3392611c395454b1d75b4b1bead9d449920f88`.

The harness measures three surfaces independently after 10,000 warmups each:

1. embedded physical prepared primary-key SELECT;
2. a persistent 32-byte `PING` round trip; and
3. persistent UDS `EXECUTE` with exact CSN/schema/row validation.

Each surface has 100,000 single-call observations.

## Latency observations

### Embedded physical prepared primary-key SELECT

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1.878 us | 1.965 us | 2.022 us | 10.836 us | 30.045 us | 517,446.586/s |
| 2 | 1.882 us | 1.964 us | 2.027 us | 10.825 us | 16.043 us | 516,660.797/s |
| 3 | 1.874 us | 1.955 us | 2.019 us | 10.826 us | 24.263 us | 516,047.458/s |
| Median statistic | 1.878 us | 1.964 us | 2.022 us | 10.826 us | 24.263 us | 516,660.797/s |

### Persistent PING round trip

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.365 us | 23.784 us | 34.019 us | 36.745 us | 139.366 us | 43,587.854/s |
| 2 | 23.380 us | 23.781 us | 33.780 us | 36.653 us | 187.533 us | 43,038.547/s |
| 3 | 23.275 us | 23.683 us | 33.155 us | 36.582 us | 120.309 us | 43,865.813/s |
| Median statistic | 23.365 us | 23.781 us | 33.780 us | 36.653 us | 139.366 us | 43,587.854/s |

### Persistent engine-bearing SQL EXECUTE

| Run | p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 21.924 us | 22.330 us | 32.960 us | 38.142 us | 262.086 us | 45,525.144/s |
| 2 | 21.932 us | 22.367 us | 32.976 us | 38.060 us | 133.040 us | 45,415.039/s |
| 3 | 21.902 us | 22.295 us | 33.011 us | 37.850 us | 129.184 us | 45,422.552/s |
| Median statistic | 21.924 us | 22.330 us | 32.976 us | 38.060 us | 133.040 us | 45,422.552/s |

Raw JSON SHA-256:

- run 1:
  `678dd1757d792fd8a5af9165873c5c76b30798fb06f8e895b43011e194dd0204`;
- run 2:
  `cdb000e254ec420d01ed930ccad7685158ba7d66c43e638e9e804f4a39d93386`;
  and
- run 3:
  `8e9a364ed8fc64ff04ec0e01157e73d985fde627ace777aeb419a62e1140fdfc`.

The checked
[machine-readable receipt](native-local-sql-select-linux.json) preserves all
three observations.

All bounded surfaces remain in microseconds. The UDS SQL p50 is lower than the
PING p50 in this sample. They are independent distributions, so this is not
negative overhead and their percentiles are not subtracted. The
262.086-microsecond UDS maximum from run 1 remains visible; no scheduler or
hardware counters assign its cause.

This is not a G7 pass or regression threshold. The virtualized, warm,
concurrency-one receipt lacks a million-observation histogram, cold state,
concurrency 8/32, saturation, background interference, allocation/RSS,
hardware counters, stable dedicated hardware, and Windows named-pipe
execution.

## Verification

The final direct-Linux validation records:

- `cargo fmt --all -- --check`;
- runtime unit tests plus the GET, SET/TTL, MATCH, and SQL integration targets;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of all five UDS smoke examples;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace run passed 664 tests with zero failures and one ignored test.
That includes 333 `hyphae-native-runtime` unit tests, five GET integration
tests, six SET/TTL integration tests, six MATCH integration tests, and nine
SQL integration tests. Documentation validation covered 241 Markdown files
and 12 JSON examples.

Validation-log SHA-256:

- workspace tests:
  `4151e2cae5776364d41fb1a915bb78f2d86fdbebd8ac671efa682e17bd69184b`;
- targeted tests:
  `512a2207ae0a4c21006ff69d0babd1c8835f731f2a183ad0b0fd01846a5be2d3`;
- Clippy:
  `011c759e3e7bbec6a51fea60f484ecf1eb80ffabb103fdc9c2e0179ef26c7644`;
- Rustdoc:
  `429332ac918ecc0637964f3fd178252fd6a7877889a8f5f6727020adb9a6a649`;
- release example checks:
  `92434e198f27984c035c376ec77a7cf9e6ebdde782f8ccb1336ae3329b66ddb7`;
- documentation:
  `d09b189d52b4e168485982c9d03c385dc33792d91ef852a6567751e6b9fb2ed0`;
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

This receipt proves bounded prepared relational reads through the local Unix
transport. It does not expose DDL or DML over the protocol, explicit SQL or
all-engine transactions, deallocation, pagination/streaming, cancellation,
Windows named pipes, authorization, cold or saturated performance, a
regression threshold, or complete G2/G5/G6/G7 evidence. It closes neither G0,
G1, G2, G5, G6, nor G7.
