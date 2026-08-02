# Native checkpoint process crash matrix on Linux

Date: 2026-08-02

Status: real process-kill evidence for singleton commit and checkpoint
publication; G1 and physical power-loss gates remain open

Source commit:
`6446baab44844a7d03c5e2878bcc1e8d780000de`

Source tree:
`7d646f654f954e3a83fc769ddf8228bba1afc3b7`

Branch: `codex/native-checkpoint-process-crash`

## Scope

This slice advances the prior seven-boundary singleton
[process-crash matrix](native-process-crash-matrix-linux-2026-08-02.md)
without changing its expected results. Receipt schema v2 adds four live
checkpoint-child scenarios:

1. synchronized manifest under its temporary name;
2. immutable manifest published without WAL authority;
3. complete checkpoint record appended before explicit WAL synchronization;
   and
4. checkpoint WAL synchronized.

For each scenario, the parent first creates and cleanly closes one complete
strict all-engine CSN. A child reopens that directory, holds the lifetime
writer lock, reaches the requested checkpoint interruption, emits one bounded
readiness line, and parks. The parent sends `SIGKILL`, waits for process death,
and reopens the directory without a graceful child close.

## Environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- `/tmp` and the repository on `/dev/nvme0n1p1`, ext4 over the EBS root
  device;
- Rust `1.96.0`, target `x86_64-linux`, release profile; and
- direct execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

## Exact recovered authority

| Checkpoint boundary | Manifests | Checkpoints | Unanchored suffix | Recovered temporary manifests |
|---|---:|---:|---:|---:|
| manifest staged | 0 | 0 | 0 | 1 |
| manifest published | 1 | 0 | 1 | 0 |
| WAL appended | 1 | 1 | 0 | 0 |
| WAL synchronized | 1 | 1 | 0 | 0 |

Every child records `termination: signal-9`. Every reopened checkpoint
directory retains the complete relational 16 KiB value, scalar value and TTL,
lexical document, immutable blob, and visible CSN 1.

The staged case removes the temporary manifest during recovery. The published
case preserves a verified but non-authoritative manifest suffix. A complete
checkpoint record becomes authority at both WAL boundaries in this
process-crash lane, matching the existing deterministic recovery contract.

## Receipt and command

The checked
[schema-v2 receipt](native-process-crash-matrix-v2-linux.json) preserves the
seven singleton commit observations and adds the four checkpoint observations,
for 11 `SIGKILL`/reopen cycles in total.

The exact clean-source command was:

```text
cargo run --release --locked -p hyphae-native-runtime \
  --example process_crash_matrix -- \
  6446baab44844a7d03c5e2878bcc1e8d780000de \
  aws-m6i.2xlarge-ext4-ebs
```

The receipt passed JSON parsing and exact assertions for:

- schema and source commit;
- seven commit plus four checkpoint observations;
- signal 9 at all 11 boundaries;
- commit recovery sequence `[null, null, null, null, 1, 1, 1]`; and
- checkpoint authority tuples
  `[(0,0,0,1), (1,0,1,0), (1,1,0,0), (1,1,0,0)]`.

## Direct-Linux gates

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p hyphae-native-runtime --all-features --locked
python3 tools/check_documentation.py
```

Results:

- complete workspace Clippy: pass with warnings denied;
- native runtime tests: 196 passed;
- debug preflight matrix: 11 of 11 process kills passed;
- clean-source release matrix: 11 of 11 process kills passed;
- documentation: pass, 195 Markdown files and 12 JSON examples before this
  evidence pair was added; and
- dependency delta: none.

The hosted stress workflow already invokes the same executable. Hosted
multi-platform and stress results remain PR evidence for this follow-up commit,
not local evidence.

## Evidence boundary

This remains process-crash evidence. `SIGKILL` preserves the Linux kernel page
cache and therefore cannot establish lost-write, torn-sector, filesystem
reordering, device-cache, EC2-stop, EBS-failure, or detached-volume behavior.

Process-kill lanes also remain open for group commit, WAL retention, page
vacuum, blob collection, structure compaction, active expiry, and migration.
This slice closes neither G1 nor G8; it advances the exact checkpoint portion
of the G1 crash lane.
