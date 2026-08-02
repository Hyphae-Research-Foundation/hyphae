# Native ext4 Linux baseline evidence

Date: 2026-08-02

Status: first native-ext4 Linux observation lane; G0 and G7 remain open

Source commit:
`fdc3293861eca83b1104a966011848ae2b3a16d0`

Source tree:
`2b5d6f8379ff74901e97e5f9e05791d1641c3dd0`

Source branch: `codex/ext4-evidence`

## Environment

Every prior receipt in the microsecond-smoke family ran under
Debian/WSL2 with the benchmark data directory on `tmpfs`, or on
Windows/NTFS. This run is the first on native Linux with the data
directory on a persistent ext4 filesystem:

- AWS EC2 devbox, Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- Intel Xeon Platinum 8375C @ 2.90GHz, 8 vCPUs, 30 GiB RAM;
- benchmark data directory under `std::env::temp_dir()` (`/tmp`) on
  ext4 over `/dev/root` (EBS), not `tmpfs`; and
- Rust 1.96.0 (`ac68faa20`), target `x86_64-linux`, release profile,
  clean worktree at the source commit.

The environment still bounds what this run proves: EC2 is virtualized,
state is warm, durability is memory, and concurrency is one. The timed
paths do not fsync, so the ext4-versus-tmpfs distinction carries less
weight under this durability class than it would under strict
durability. The run exercises no named-pipe/UDS transport and no
proofs, and it observes no cold state, saturation, interference,
allocation/RSS, or hardware counters.

## What this run establishes

- the first native-Linux, non-tmpfs execution lane for the
  microsecond-smoke receipt family;
- the latency baseline for future receipts produced on this devbox; and
- the first receipt in this family without the standing WSL2 caveat
  that the benchmark data directory was `tmpfs` rather than a
  persistent filesystem.

It does not establish fsync latency on ext4, power-loss behavior,
physical durability, or any comparison against prior receipts.

## Mechanical validation

The executed local validation for this evidence is:

- the release benchmark ran to completion from a clean worktree at the
  exact source commit on the environment above, and its JSON receipt
  validated with `python3 -m json.tool`;
- `python3 tools/check_documentation.py` passes with this evidence
  closure; and
- `git diff --check` is clean.

Hosted multi-OS, dependency, security, fuzz, and stress checks remain
PR evidence rather than local evidence. Mutation testing was not
executed: the repository has no accepted mutation tool, operator set,
or surviving-mutant threshold for this milestone.

## Latency observation

The [machine-readable schema-v15
receipt](native-microsecond-smoke-ext4-linux.json) was produced with
warm state, memory durability, concurrency one, a height-two relational
B+tree, 6,145 relational rows (2,049 primary, 2,048 secondary-index,
and 2,048 prefix-tenant rows), 2,048 structure keys, hash fields, set
members, and search documents, `LIMIT 10` scans, a `[1024, 1034)`
primary-key range, and an `[a,b)` ordered secondary range over
variable-width text keys. Batched paths use 1,000,000 timer samples of
32 operations each after 100,000 warmups; scan, secondary-exact, and
search paths use 100,000 single-call observations; the two
secondary-range paths use 1,000 warmups plus 10,000 single-call
observations. The local-frame path measures codec plus embedded
dispatch; no named-pipe transport is involved.

| Operation | p50 | p95 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|---:|
| Embedded structure get (64 B) | 0.058 us | 0.138 us | 0.147 us | 0.378 us | 14,941,656 ops/s |
| Buffered structure B+tree get (64 B, multilevel) | 1.028 us | 2.150 us | 2.536 us | 2.998 us | 882,943 ops/s |
| Embedded hash `HGET` (64 B, materialized snapshot) | 0.124 us | 0.236 us | 0.310 us | 0.518 us | 7,384,037 ops/s |
| Buffered hash `HGET` (64 B, multilevel) | 1.806 us | 4.451 us | 4.837 us | 5.118 us | 463,105 ops/s |
| Embedded set `SISMEMBER` (materialized snapshot) | 0.124 us | 0.235 us | 0.302 us | 0.578 us | 7,304,922 ops/s |
| Buffered set `SISMEMBER` (multilevel) | 2.502 us | 4.901 us | 6.255 us | 6.740 us | 364,051 ops/s |
| Buffered inverted B+tree BM25 `MATCH` top-1 (rare term) | 29.272 us | 50.997 us | 56.508 us | 72.431 us | 31,681 ops/s |
| Embedded prepared SQL primary key (materialized snapshot) | 0.057 us | 0.123 us | 0.141 us | 0.367 us | 15,489,818 ops/s |
| Buffered relational B+tree primary key (multilevel) | 1.749 us | 3.265 us | 4.192 us | 4.676 us | 524,098 ops/s |
| Buffered relational B+tree primary-key scan `LIMIT 10` | 18.833 us | 33.680 us | 36.116 us | 51.611 us | 48,983 ops/s |
| Physical prepared SQL primary-key scan `LIMIT 10` | 21.630 us | 40.502 us | 42.007 us | 58.224 us | 42,237 ops/s |
| Buffered relational B+tree primary-key range `LIMIT 10` | 18.728 us | 36.085 us | 37.746 us | 53.245 us | 48,519 ops/s |
| Physical prepared SQL primary-key range `LIMIT 10` | 24.009 us | 44.251 us | 47.395 us | 63.343 us | 38,103 ops/s |
| Physical prepared SQL range plus boolean residual `LIMIT 10` | 29.011 us | 39.811 us | 53.700 us | 66.572 us | 32,918 ops/s |
| Physical prepared SQL strict prefix `LIMIT 10` | 21.646 us | 32.353 us | 41.324 us | 55.131 us | 43,840 ops/s |
| Physical prepared SQL prefix plus range `LIMIT 10` | 22.188 us | 38.908 us | 42.684 us | 58.920 us | 41,013 ops/s |
| Buffered relational B+tree secondary exact unique | 18.816 us | 31.258 us | 36.623 us | 51.442 us | 49,799 ops/s |
| Physical prepared SQL secondary exact unique | 19.519 us | 31.587 us | 37.979 us | 51.915 us | 48,216 ops/s |
| Unindexed text-range PK-scan baseline `LIMIT 10` | 12,833.304 us | 14,882.676 us | 16,465.621 us | 19,855.517 us | 77 ops/s |
| Physical prepared SQL ordered secondary range `LIMIT 10` | 42.698 us | 81.151 us | 90.215 us | 107.329 us | 21,404 ops/s |
| Local frame decode plus structure dispatch (64 B) | 0.101 us | 0.178 us | 0.253 us | 0.493 us | 9,065,736 ops/s |

The physical prepared ordered secondary range is within the provisional
phase-1 target for indexed SQL returning at most 100 rows (p50 50 us,
p99 250 us) on this hardware as well, for this single scenario under
the disclosed warm/bounded conditions. Against the same-run unindexed
baseline, which returns the same ten validated rows, the indexed path
reduces p50 by 99.667% and p99 by 99.452%; the differential identifies
the removed algorithmic work and is not a regression threshold.

## Relationship to prior receipts

The schema-v15 WSL2 receipt was produced on different physical
hardware. Nothing in this run controls for CPU model, clocks, memory,
virtualization layer, and filesystem at the same time, so no number
here is run-to-run comparable with any WSL2 or Windows receipt. Higher
or lower values against those receipts are neither regressions nor
improvements; this receipt is the first point of a new native-ext4
baseline series.

## Remaining boundary

Still required for G7:

- stable, non-virtualized hardware with controlled scheduling;
- cold-state, saturation, interference, and concurrency lanes;
- allocation/RSS accounting and hardware counters;
- named-pipe/UDS transport and proof-bearing measurement;
- strict-durability timings with real fsync on ext4, plus the separate
  native-ext4 power-loss and physical-durability evidence that the
  retention milestones still require; and
- the complete G7 warm/cold/concurrency/saturation matrix.

No G0 or G7 gate closes from this observation alone.
