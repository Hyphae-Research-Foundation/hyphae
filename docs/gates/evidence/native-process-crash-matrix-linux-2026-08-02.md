# Native process crash matrix on Linux

Date: 2026-08-02

Status: real process-kill evidence for the singleton all-engine commit; G1 and
physical power-loss gates remain open

Source commit:
`91af0b731785ec827177edf84e0fe5eaad732360`

Source tree:
`a613893da019d0840c9310887a8e342b0453a457`

Branch: `codex/native-process-crash-matrix`

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- `/tmp` and the repository on `/dev/nvme0n1p1`, ext4 over the EBS root
  device;
- Rust `1.96.0`, target `x86_64-linux`, release profile; and
- direct execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Crash mechanism

`process_crash_matrix` starts one child process for each native singleton
commit boundary. Each child:

1. creates its own native directory and keeps the lifetime writer lock open;
2. begins one strict-durability transaction;
3. creates a relation and inserts a 16 KiB value that crosses the current
   immutable-blob threshold;
4. writes one scalar key with an absolute TTL;
5. creates a lexical index and indexes one document;
6. reaches one deterministic commit interruption; and
7. signals the parent while the `NativeDatabase` handle remains live.

The parent then calls `Child::kill`. On this Linux host every child terminated
with signal 9. The parent waits for process death, reopens the directory, and
validates relational, structure/TTL, lexical, blob, and visible-CSN state.
There is no graceful database close between the selected boundary and reopen.

The readiness channel has a ten-second bound. Unexpected child output,
successful child exit, a non-`SIGKILL` Unix termination, or any mixed recovered
state fails the run.

## Expected atomicity

| Boundary | Expected reopen state |
|---|---|
| blob staged | prior empty state |
| blob promoted | prior empty state |
| page appended | prior empty state |
| page synchronized | prior empty state |
| WAL appended | complete CSN 1 |
| WAL synchronized | complete CSN 1 |
| root published | complete CSN 1 |

For the prior state, no relation row or structure value is visible and lexical
lookup rejects the absent index. For the complete state, one snapshot at CSN 1
returns the full 16 KiB relational value, the scalar value with the expected
50 microseconds of remaining TTL, and the exact lexical document. A partial
combination is a hard failure.

## Exact Linux result

The checked
[machine-readable receipt](native-process-crash-matrix-linux.json) records
this recovered sequence:

```text
[null, null, null, null, 1, 1, 1]
```

Every boundary records `termination: signal-9`. The staged-only boundary
reopens with zero promoted blobs. Later pre-WAL boundaries can retain one
unreferenced promoted blob, but no logical state becomes visible without WAL
authority. Every boundary at or after the complete WAL append reopens the
single blob and the complete all-engine CSN.

The exact clean-source command was:

```text
cargo run --release --locked -p hyphae-native-runtime \
  --example process_crash_matrix -- \
  91af0b731785ec827177edf84e0fe5eaad732360 \
  aws-m6i.2xlarge-ext4-ebs
```

The receipt passed JSON parsing plus exact schema, source commit, boundary
count, termination, and recovered-CSN assertions.

## Red-to-green and local gates

The first Clippy run rejected the helper API:

```text
error: this argument is passed by value, but not consumed in the function body
```

The helper was changed to compare borrowed values. The complete direct-Linux
gate then passed:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p hyphae-native-runtime --all-features --locked
python3 tools/check_documentation.py
```

Results:

- complete workspace Clippy: pass with warnings denied;
- native runtime tests: 196 passed;
- documentation: pass, 194 Markdown files and 12 JSON examples before this
  evidence pair was added;
- release process-crash matrix: all seven boundaries passed; and
- dependency delta: none.

The hosted stress workflow now runs this native process-kill matrix before the
existing public-binary kill/restart and backup/restore soak.

## Evidence boundary

`SIGKILL` is a real process crash, not physical power loss. Linux closes file
descriptors and releases the writer lock, but the kernel page cache survives.
This result therefore does not establish behavior under:

- lost device-cache writes, torn sectors, or filesystem reordering;
- EC2 stop, host failure, EBS failure, or detached-volume recovery;
- process kill during checkpoint, retention, vacuum, blob collection, group
  commit, or migration;
- resource exhaustion, read-only remount, I/O error injection, or disk full;
  or
- multi-process readers, UDS transport, backup/restore, or independent restore.

This closes neither G1 nor G8. It advances the singleton commit portion of the
G1 crash lane and replaces an in-process-only claim with exact process-level
evidence on the current Linux development host.
