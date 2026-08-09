# Native block-layer power-loss replay on Linux

Date: 2026-08-02

Status: stable-media block-order replay evidence for singleton commit and
checkpoint publication; G1 and literal physical-device power-loss gates remain
open

Source commit:
`0a167bc40d18d7e2cabc01b0b96da57661742613`

Source tree:
`679c549a25860024624770c1c8f82441a9c8131b`

Branch: `codex/native-block-power-loss`

## What this adds

The prior process-crash lane kills a child at deterministic native boundaries,
but Linux retains its page cache. This gate places a fresh ext4 filesystem
over `dm-log-writes`, records block writes plus flush/FUA ordering, marks the
same seven commit and four checkpoint boundaries, and kills the live writer.
It then replays only entries through the mark onto a fresh zeroed image,
mounts that image normally for ext4 journal recovery, and opens Hyphae.

Linux documents `dm-log-writes` as logging normal completed writes at the next
`PREFLUSH` and FUA writes on completion so replay can model a worst-case power
failure order. The userspace `replay-log` binary was built from exact upstream
commit `7b70d8a6863c5de30933d42a7672d35d01d2dc6c`.

This toolchain is test infrastructure only. It does not enter the Hyphae
runtime or replace any owned page, WAL, manifest, catalog, MVCC, relational,
structure, or search component.

## Isolation and environment

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- Linux device-mapper target `log-writes v1.1.0`;
- three fresh sparse files and loop devices per scenario: 128 MiB live,
  128 MiB replay, and 512 MiB write log;
- deterministic ext4 creation with discard and lazy inode/journal
  initialization disabled;
- live and replay mounts observed as
  `rw,relatime,commit=600,data=ordered`; and
- direct execution from `/home/mario/celiumsai/hyphae`; WSL was not used.

The harness accepts only `/dev/loopN` allocations whose observed backing file
equals a file below its unique `/var/tmp/hyphae-power-loss-*` root. It verifies
that identity again before formatting, replay, mount, and detach. It never
formats, mounts, replays, or writes `/dev/nvme0n1p1`. Every scenario reports
`cleanup: complete`; the final run left no mount or device-mapper resource.

## Singleton commit result

One strict transaction creates a relation and lexical index, writes a 16 KiB
relational value, sets a scalar value with TTL, and indexes one lexical
document under CSN 1.

| Interruption mark | Recovered CSN | Logical state |
|---|---:|---|
| blob staged | none | prior empty |
| blob promoted | none | prior empty |
| page appended | none | prior empty |
| page synchronized | none | prior empty |
| WAL appended | none | prior empty |
| WAL synchronized | 1 | complete all-engine state |
| root published | 1 | complete all-engine state |

All seven children terminate with signal 9. Relational, scalar/TTL, lexical,
and CSN validation rejects every partial combination.

The promoted-blob through WAL-appended images retain one immutable orphan
blob, but no row, scalar, search document, or CSN references it. This is
expected copy-on-write residue and remains subject to the separately gated
blob-collection policy.

The important difference from process kill is `WAL appended`: page-cache
reopen saw a complete transaction, while stable-block replay correctly sees
the prior state because no WAL synchronization crossed the barrier.

## Checkpoint result

Each checkpoint scenario starts from the same clean, complete CSN 1.

| Interruption mark | Manifests | Checkpoints | Unanchored | Temp recovered |
|---|---:|---:|---:|---:|
| manifest staged | 0 | 0 | 0 | 1 |
| manifest published | 1 | 0 | 1 | 0 |
| WAL appended | 1 | 0 | 1 | 0 |
| WAL synchronized | 1 | 1 | 0 | 0 |

All four replay images preserve the complete relational, scalar/TTL, lexical,
blob, and CSN state that preceded checkpointing. The staged temporary manifest
is recovered then removed. The published manifest remains non-authoritative
until its checkpoint WAL record is synchronized.

This also differs materially from process kill: the unsynchronized appended
checkpoint record survived page-cache reopen but is absent from the worst
stable replay.

## Commands and mechanical checks

The source-bound receipt command was:

```text
python3 tools/run_native_power_loss_gate.py \
  --source-commit 0a167bc40d18d7e2cabc01b0b96da57661742613 \
  --environment aws-m6i.2xlarge-ext4-ebs-dm-log-writes \
  --replay-tool-source \
    /tmp/log-writes-7b70d8a6863c5de30933d42a7672d35d01d2dc6c \
  --output /tmp/hyphae-block-power-loss-0a167bc.json
```

The harness rebuilt the native release probe and pinned replay utility before
allocating devices. Independent receipt assertions verified:

- schema, status, source commit, and source tree;
- seven commit plus four checkpoint observations;
- commit CSNs `[null, null, null, null, null, 1, 1]`;
- checkpoint authority tuples
  `[(0,0,0,1), (1,0,1,0), (1,0,1,0), (1,1,0,0)]`;
- a positive `dm-log-writes` entry count at every interruption mark, ranging
  from 119 to 176 entries in this run;
- signal 9, successful normal ext4 mount/recovery, clean read-only `e2fsck`,
  and exact cleanup for all 11 scenarios; and
- no active `dm-log-writes` mapper or Hyphae power-loss mount after the run.

The implementation red/green record first showed the new verifier mode absent
as an unexpected argument, then present and failing at the correct missing
native directory boundary. Two Rust expectation tests and seven non-privileged
Python safety tests pass.

## Evidence boundary

The raw receipt status is
`block-replay-not-physical-device-cut`. This gate is stronger than `SIGKILL`
for flush ordering and loss of unflushed writes, but it is not evidence of an
actual EC2 stop, EBS detach or failure, availability-zone loss, firmware cache,
or arbitrary tear inside a completed block write.

Group commit, WAL retention, page vacuum, blob collection, active expiry,
migration, resource exhaustion, background maintenance, cross-platform
filesystems, and literal device/host power-loss lanes remain open. This slice
advances G1; it does not close G1 or the release gate.
