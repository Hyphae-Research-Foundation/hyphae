# Native local UDS transport evidence

Date: 2026-08-03

Status: first real local UDS transport receipt; G0, G1, G6, and G7 remain open

Source branch: `codex/native-local-uds`

Contract commit:
`caf4ed34f976f8917426f3c1f5bd016110e339ea`

Implementation commit:
`6e799e569a3f8b12b82e9ec2a5dda92f81892a6d`

Benchmark source commit:
`ba8a2a554e7f34f2a091a80c4e481bd1afa12579`

Benchmark source tree:
`faab7aa72005a03494b33a4d3931ccdad52bebdd`

Final verification commit:
`c042af5b028a77c448985a7dd8d49e266f809322`

Final verification tree:
`7201f0fdfb7c1f4e590709bf9e02debe2a539968`

Harness SHA-256:
`1e75b6cb8e610eb3df79bd9f553b92becca98a1c96588f9826c1e95d1c226113`

## Contract and implementation

The [frozen transport contract](../../native/local-uds-transport-v1.md)
defines the first real transport below the canonical `HYPHLCL1` frame. The
implementation adds:

- `LocalFrameIo`, with bounded reusable receive and send buffers, clean EOF
  only before the first header byte, typed truncated-header/payload failures,
  and maximum-payload rejection before payload allocation;
- filesystem-backed `UdsFrameListener` and `UdsFrameConnection` on Unix;
- fail-closed bind when any endpoint already exists;
- exact owner-only endpoint mode (`0600`);
- device/inode/socket identity tracking so cleanup cannot remove a replacement
  endpoint; and
- ordered `HELLO`/`WELCOME`, persistent `PING`, and `CLOSE` receipt traffic.

The compiler-reaching red gate was:

```text
cargo test -p hyphae-native-runtime --test local_uds_transport --no-run
```

It failed with `E0432` unresolved imports for `LocalFrameIo`,
`LocalTransportError`, `UdsFrameConnection`, and `UdsFrameListener` before
those public types existed. The complete red log SHA-256 is
`c1f565cd863033d021d7af4a0978ec627533731409000e97a9ee9a9368823213`.
An earlier missing-test-directory setup failure is excluded from this claim.

## Failure-path coverage

The integration receipt covers:

- fragmented headers and payloads, then clean EOF;
- maximum configuration above the global limit;
- partial frames;
- an oversized declared payload rejected before reading its body;
- a real round trip with exact kind, stream ID, request ID, and payload over
  three ordered `PING` frames on one persistent connection;
- exact `0600` socket permissions and normal endpoint cleanup;
- preservation of a pre-existing non-socket path; and
- preservation of a replacement endpoint plus typed `EndpointReplaced`
  cleanup failure.

Portable unit coverage additionally verifies receive/send buffer reuse without
stale frame state. Existing codec tests retain invalid magic, version, flags,
kind, length, and checksum coverage.

## Direct-Linux environment

- AWS EC2 `m6i.2xlarge` in `us-east-1`, virtualized by KVM;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- Intel Xeon Platinum 8375C @ 2.90 GHz, 8 vCPUs, 30 GiB RAM;
- `/tmp` on persistent ext4 over `/dev/nvme0n1p1` (EBS);
- Rust 1.96.0 (`ac68faa20`), `x86_64-unknown-linux-gnu`, release profile;
- client and server restricted together to logical CPUs 2 and 3 with
  `taskset`; the guest did not expose a CPU-frequency governor; and
- one client thread, one server thread, concurrency one.

This is direct Linux execution. WSL is not part of the build, test, benchmark,
or Git path.

## Latency observation

The harness ran a 256-observation connect plus `HELLO`/`WELCOME` plus `CLOSE`
route and a separate persistent-connection route with 10,000 warmups followed
by 100,000 measured 32-byte `PING` round trips. The configured maximum payload
was 64 bytes. Each row below is one independent release execution.

| Run | Persistent p50 | p95 | p99 | p99.9 | Maximum | Throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.320 us | 29.060 us | 35.340 us | 44.631 us | 1,069.968 us | 42,601.516/s |
| 2 | 23.171 us | 29.088 us | 35.237 us | 44.429 us | 132.348 us | 43,029.595/s |
| 3 | 23.261 us | 29.001 us | 35.290 us | 44.817 us | 846.969 us | 42,771.042/s |
| Median statistic | 23.261 us | 29.060 us | 35.290 us | 44.631 us | 846.969 us | 42,771.042/s |

The median connect/handshake/close statistics are p50 `35.095 us`, p95
`51.723 us`, p99 `60.715 us`, p99.9 `68.755 us`, maximum `87.702 us`, and
throughput `25,904.895/s`.

Raw JSON SHA-256:

- run 1:
  `a96e1f35662a08dd2c9aa507f565327b5341846454226b46407ca9c229c6363b`;
- run 2:
  `404f44ff3af89b08cbdc5e33b95e69868137afe23eb28749afff12f5099391ff`;
  and
- run 3:
  `d636617d0ffd28367c5e82247165a57fa7d47c046dea54b332a4fba03cc194d5`.

The checked [machine-readable receipt](native-local-uds-linux.json) preserves
all three observations. There was no earlier real UDS implementation, so this
is a first baseline and not an A/B regression comparison.

The persistent route is below the provisional 25-us p50 and 100-us p99 native
local-protocol structure-point targets, but it is only transport framing plus
echo. It does not execute a structure `GET`, so those target gates have not
passed. The approximately 23.3-us transport p50 leaves little room inside the
provisional 25-us end-to-end `GET` target; the next engine-bearing UDS
vertical must treat that as a measured design constraint.

The millisecond-scale maxima in runs 1 and 3 are reported rather than hidden.
They are compatible with guest scheduling tails, but this receipt does not
assign a cause because scheduler and hardware-counter evidence was not
captured.

## Verification

The final direct-Linux validation records:

- `cargo fmt --all -- --check`;
- `cargo test --workspace --all-targets --all-features --locked`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`;
- release compilation of `uds_ping_smoke`;
- `python3 tools/check_documentation.py`;
- `python3 -m json.tool` for the checked receipt; and
- `git diff --check`.

The workspace test run passed 637 tests with zero failures and one ignored
test. That includes 332 `hyphae-native-runtime` unit tests and the four UDS
integration tests. Documentation validation covered 233 Markdown files and 12
JSON examples.

Validation-log SHA-256:

- workspace tests:
  `1480066d39ec65d9fe0c744ecc63cd9115ab603c81f45725f3296a8e9d619eaa`;
- Clippy:
  `a415fde2923d5bf8ec1163c18f19850b4e9f3a3aa5201774fb0459b0a7e048f5`;
- Rustdoc:
  `af6664beb508e112d56c7fcfe072f2671d93d067c519fb03b111aff4cc0666dc`;
- release example check:
  `92434e198f27984c035c376ec77a7cf9e6ebdde782f8ccb1336ae3329b66ddb7`;
- documentation:
  `497e5c16ecc24493c799b372f00ec661307c015d594e3d93c69e6a300274a054`;
  and
- formatting (an empty success log):
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

The first hosted macOS run exposed a fixture-only portability defect:
GitHub's macOS `TMPDIR` plus the socket name exceeded `sockaddr_un.sun_path`.
Commit `c042af5b028a77c448985a7dd8d49e266f809322` moves only the Unix test
fixture into a unique short directory below `/tmp`; product endpoint behavior
and the measured Linux harness are unchanged.

Hosted Linux/macOS/Windows, dependency, security, fuzz, and stress lanes remain
PR evidence. Mutation testing was not executed because this repository has no
accepted mutation tool, operator set, or surviving mutant threshold for this
slice.

## Boundary

This receipt removes the explicit "no UDS transport receipt" deficit from the
minimal G1 vertical. It does not implement Windows named pipes, the complete
session/authentication/multiplexing/flow-control contracts, or database
operation payloads. It measures no engine execution, queue, fsync, cold state,
saturation, allocation/RSS, hardware counters, proof construction, or power
loss. It closes neither the rest of G1 nor G0, G6, or G7.
