# Native block-layer power-loss replay gate v1

Status: normative test contract; harness and direct-Linux/ext4 receipt
implemented; literal physical-device cut remains open

This gate distinguishes stable-media recovery from process death. `SIGKILL`
leaves the Linux page cache alive and can therefore expose writes that never
crossed a storage barrier. The gate instead records block writes beneath an
ext4 filesystem and replays only the worst-case order that completed through
`PREFLUSH` or FUA before an interruption mark.

The application contract remains Hyphae-owned. Linux device-mapper,
`dm-log-writes`, ext4, and the pinned replay utility are external test
infrastructure only; they are not product runtime dependencies.

## Threat model

The gate covers:

- loss of writes that did not cross a completed flush or FUA barrier;
- block-write completion order at each commit and checkpoint interruption;
- ext4 journal recovery from the reconstructed stable image;
- native WAL, manifest, page, blob, catalog, MVCC, relational, structure, and
  lexical recovery after that filesystem recovery; and
- all-engine atomicity under one CSN.

It does not establish:

- a literal EC2 host power cut, EBS detach, availability-zone failure, or
  device-firmware cache behavior;
- arbitrary sector tearing inside a completed atomic block write;
- Windows/NTFS or macOS/APFS durability;
- group commit, WAL retention, vacuum, blob collection, expiry, migration, or
  background-maintenance power-loss behavior; or
- a latency or throughput result.

The receipt status must therefore say `block-replay-not-physical-device-cut`.

## Isolated topology

Every boundary receives a fresh topology:

1. three newly created sparse files for the live, replay, and write-log
   devices;
2. three loop devices allocated only from those files;
3. one uniquely named `dm-log-writes` mapping over the live loop device;
4. a deterministic ext4 filesystem created through that mapping with lazy
   initialization disabled;
5. one mount directory below the gate's unique temporary root; and
6. one native data directory below that mount.

The canonical repository, `/dev/nvme0n1p1`, every pre-existing block device,
and every caller-supplied mount remain out of scope. The harness must prove
that each loop device names one of its own regular files before formatting,
mounting, replaying, or cleanup. Cleanup addresses only resources carrying the
unique run identifier.

The harness requires non-interactive `sudo` because loop, device-mapper, and
mount operations are privileged. Missing tools, unavailable kernel targets,
an occupied mapper name, an unexpected device backing file, or incomplete
cleanup fails the gate.

## Recording and replay

The harness uses Linux `dm-log-writes`, whose kernel contract logs normal
writes after a completed `PREFLUSH` and logs FUA writes on completion. This
ordering deliberately models the worst stable state at power loss. The
userspace `replay-log` utility is built from exact upstream commit
`7b70d8a6863c5de30933d42a7672d35d01d2dc6c`.

For every boundary the harness:

1. creates ext4 through the logging target and inserts an `ext4-ready` mark;
2. starts a native child that retains the database handle and writer lock;
3. waits for the child's exact bounded boundary signal;
4. inserts a unique interruption mark and sends that child `SIGKILL`;
5. unmounts and removes only the live topology;
6. replays the write log from entry zero through the interruption mark onto
   the initially zeroed replay loop device;
7. mounts the replay image normally so ext4 performs journal recovery;
8. opens Hyphae and validates the recovered logical and physical authority;
9. unmounts, removes the remaining loop devices, and deletes the unique
   temporary root; and
10. emits one source-bound JSON receipt.

Writes caused by live unmount occur after the interruption mark and are not
part of the reconstructed image.

## Required recovery states

The strict transaction creates one table and one lexical index, then writes a
16 KiB relational value, one scalar value with TTL, and one lexical document.
No partial combination may become visible.

| Commit interruption | Required recovered state |
|---|---|
| blob staged | prior empty state |
| blob promoted | prior empty state |
| page appended | prior empty state |
| page synchronized | prior empty state |
| WAL appended | prior empty state |
| WAL synchronized | complete CSN 1 |
| root published | complete CSN 1 |

The `WAL appended` result intentionally differs from the process-kill lane:
without a completed WAL flush, the worst stable-media replay has no commit
authority.

Checkpoint scenarios start from one clean strict all-engine CSN:

| Checkpoint interruption | Manifests | Checkpoints | Unanchored suffix |
|---|---:|---:|---:|
| manifest staged | 0 | 0 | 0 |
| manifest published | 1 | 0 | 1 |
| WAL appended | 1 | 0 | 1 |
| WAL synchronized | 1 | 1 | 0 |

A staged temporary manifest may be absent or recovered and removed because
its parent-directory entry is not yet the authoritative publication. Both
states must leave zero manifests, zero checkpoints, zero unanchored suffix,
and the complete prior CSN. Every other count is exact.

## Receipt contract

The JSON receipt must bind:

- schema `hyphae.native.block-power-loss-replay.v1`;
- the exact clean source commit and source tree;
- kernel, architecture, filesystem, mount options, and environment label;
- the `dm-log-writes` target version;
- the exact replay utility commit;
- one termination, replay, mount, and native-reopen observation per boundary;
- the expected and recovered CSN or checkpoint authority counts; and
- cleanup completion for every isolated resource.

A passing receipt requires all seven commit and four checkpoint observations.
Compilation, deterministic interruption tests, process-kill evidence, or a
successful filesystem mount cannot substitute for this receipt.

The first source-bound execution is recorded in the
[Linux evidence narrative](../gates/evidence/native-block-power-loss-replay-linux-2026-08-02.md)
and its adjacent JSON receipt.
