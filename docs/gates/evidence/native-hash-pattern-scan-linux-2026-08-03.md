# Native hash pattern scan evidence on Linux

Date: 2026-08-03

Status at capture: direct-Linux gates complete; hosted CI, reverse-pattern
scans, field TTL, local-protocol exposure, randomized model equivalence,
complete G3, and G7 remain open

Source branch: `codex/native-hash-pattern-scan`

Stacked base:
`codex/native-hash-reverse-scan@232ba96d848cec5ac37bf0bdf011d01c9aafacd7`

Contract commit:
`8c2e652b71403cb9ae22a6379f20e364b0e78af3`

Runtime commit:
`8c2eb1aa6db688ad74b2747ad9f465098cfb8787`

Benchmark commit:
`f5f76aa42ab2d4b57a894a5c3afa6f9e4ef8daf1`

Benchmark tree:
`14b1185eb73a8db872e037cb3296a9ac58876fc3`

## Scope

This slice adds bounded `HSCAN_MATCH` pages to the embedded native structure
engine. One request compiles a binary glob once and carries an exclusive exact
field cursor, a returned-match limit, a physical-visit limit, and a
matcher-step limit. The grammar supports literal bytes, `?`, `*`, bracket
classes and ranges, negated classes, and byte escapes. It does not depend on
UTF-8, regex, an external query engine, or a sidecar.

Private batches and retained snapshots filter their materialized ordered hash
map. Current-root calls execute against the native structure B+tree. An exact
literal uses one point lookup. A leading literal prefix becomes physical lower
and upper bounds. Only a leading-wildcard pattern scans the complete per-hash
prefix, and that route remains bounded by physical visits and matcher steps.

The continuation is the last physical field visited, including a tombstone or
nonmatch. This permits an empty non-exhausted page to advance without rescanning
the same candidates. Output limit wins when output and visit limits are
satisfied by the same field. Exceeding the matcher budget fails the complete
call rather than returning a partial page.

Inside a canonical per-hash field prefix, every remaining byte suffix is a
valid binary field identity. Cross-hash and truncated identities cannot enter
the selected bounds. Oversized caller cursors and derived-prefix identities
are rejected before traversal; reached malformed metadata, values, blobs,
page order, or cycles fail closed.

This is a read-only surface. It adds no WAL opcode, mutation, physical format,
dependency, sidecar, or internal network protocol.

## Contract-first red and green

The compiler-reaching model gate was:

```text
cargo test -p hyphae-native-runtime \
  model::tests::hash_pattern_scan_pages_advance_across_nonmatches --no-run
```

It failed with the expected two `E0599` errors for the missing
`hscan_match_at` model surface. The model, matcher, embedded, and physical
routes were then implemented against the frozen contract.

The completed native-runtime suite has 261 passing tests. New coverage
includes:

- binary glob grammar, escapes, ranges, negation, NUL bytes, adjacent-star
  normalization, malformed patterns, and fixed input limits;
- exact-point and leading-literal-prefix route derivation;
- private-batch, retained-snapshot, current-root, explicit-time, and reopened
  result equivalence;
- cursors below, inside, and above the derived prefix;
- whole-hash TTL visibility immediately before, at, and after expiry;
- sparse leading-wildcard pages whose empty continuation advances across
  nonmatches;
- tombstones consuming physical visits without consuming output;
- explicit segmentation differences between materialized and physical pages
  with eventual result equivalence;
- matcher-step exhaustion with no partial result;
- oversized cursor rejection on private, snapshot, and physical surfaces;
- a height-two structure tree with literal-prefix pruning and bounded early
  stop; and
- unreached corruption exclusion plus fail-closed reached metadata, value,
  blob, page-order, and cycle errors.

The corruption tests use forged in-memory roots and do not publish invalid
state.

## Direct-Linux latency observation

The exact committed release harness uses one reopened 2,048-field,
height-two hash with 64-byte fields and values. Half the fields begin with
`tenant-a:` and half with `tenant-b:`. Thirty-two evenly distributed fields
end with `needle`. Every route uses 10,000 observations, 1,000 warmups,
32 returned fields, concurrency one, and CPU 2.

| Route | Visits | p50 | p95 | p99 | Throughput |
|---|---:|---:|---:|---:|---:|
| Native `tenant-b:*` | 32 | 16.337 us | 16.646 us | 25.456 us | 60,302/s |
| Full HSCAN + prefix filter | 2,048 | 401.302 us | 419.621 us | 433.324 us | 2,462/s |
| Native `*needle` | 1,985 | 651.244 us | 663.124 us | 670.262 us | 1,533/s |
| Full HSCAN + suffix filter | 2,048 | 401.791 us | 418.600 us | 424.938 us | 2,465/s |

Leading-literal physical pruning reduced p50 by 95.929%, p95 by 96.033%, and
p99 by 94.125% against full materialization and application filtering. Call
throughput increased by 2,349.178%.

The leading-wildcard result is deliberately retained even though it is
negative: native p50 was 62.085% slower and throughput was 37.797% lower than
full HSCAN plus the specialized application suffix predicate. The native
route provides bounded progress, one canonical binary-glob grammar, and
matcher-budget enforcement, but this corpus exposes matcher and decode
overhead when no prefix can be pruned. It is a measured optimization target,
not evidence to claim a universal pattern-scan speedup.

The benchmark harness SHA-256 is
`a532864deaf0903660ff2a3bb06ea6dc69e7f72ab398bda40b6a2f3d5ab8d317`.
The raw benchmark output SHA-256 is
`2af1ec0f92d6399af853abe2d36f8d418a68032dcc94a4e381679b39659e8618`.

## Matched persistent-read control

The unchanged HGET control harness had SHA-256
`8d03592813a5607be4ff1f63c7fced8c059a59a37ebd524c4098f1b9ed05b0eb`
on both the stacked parent and benchmark commits. Two alternating
parent/current rounds were pinned to CPU 2:

| Round | Order | Commit | p50 | p95 | p99 | p99.9 | Throughput |
|---|---|---|---:|---:|---:|---:|---:|
| 1 | first | Current | 1.354 us | 1.597 us | 1.655 us | 2.723 us | 726,965/s |
| 1 | second | Parent | 1.365 us | 1.596 us | 1.655 us | 2.291 us | 722,879/s |
| 2 | first | Parent | 1.389 us | 1.648 us | 1.684 us | 2.648 us | 709,866/s |
| 2 | second | Current | 1.343 us | 1.572 us | 1.641 us | 2.625 us | 733,634/s |

The two-round medians changed by -2.070% at p50, -2.312% at p95,
-1.288% at p99, +8.281% at p99.9, and +1.944% in throughput. The frozen
gate was no more than a 10% increase at p50 and p95; it passed.

The parent ran from a detached temporary Linux worktree with an isolated Cargo
target. Its exact path and commit were verified before removal. Source remains
recoverable from Git.

The checked machine-readable
[receipt](native-hash-pattern-scan-linux.json) contains the benchmark and both
pinned control rounds. Its SHA-256 is
`7b5e37c22572828dbcfec50e49b9c83313d66f5d216d40ba152d0a47d0e96e46`.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`, x86_64;
- repository and temporary data on `/dev/nvme0n1p1`, ext4 over EBS;
- Rust `1.96.0`, release profile for measurements; and
- direct SSH execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Verification

Executed directly on the canonical Linux host:

```text
cargo fmt --all -- --check
cargo test -p hyphae-native-runtime --lib --locked
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --release --locked -p hyphae-native-runtime \
  --example hash_pattern_scan_smoke
python3 tools/check_documentation.py
python3 -m json.tool \
  docs/gates/evidence/native-hash-pattern-scan-linux.json
git diff --check
```

The runtime suite reported 261 passing tests and every command above passed
against benchmark commit `f5f76aa` plus the evidence-only changes. No
dependency, unsafe-Rust allowance, external runtime, or network protocol
changed. Hosted checks are deliberately not claimed by this local receipt;
they are evaluated on the stacked pull request.

## Evidence boundary

This receipt proves one bounded native binary-glob implementation across the
logical model, private batches, retained snapshots, physical B+tree execution,
TTL visibility, reopen, tombstones, empty-page progress, prefix pruning,
matcher budgeting, early stop, and reached corruption.

It does not close reverse-pattern scans, field TTL, local-protocol exposure,
randomized model equivalence, saturation, cold cache, allocation/RSS,
hardware counters, memory amplification, physical power loss, complete G3, or
G7. The leading-wildcard negative result remains an explicit performance
backlog item.
