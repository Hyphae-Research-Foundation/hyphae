# Native lineage ext4 latency receipt

Date: 2026-08-02

Status: same-host lineage-bearing observation; G1 and G7 remain open

Executed source commit:
`02945dfd84867ba62dbb0493c23f8cb76b121449`

Executed source tree:
`3c762ae361e072c4baff3b24eecd2068eacddddd`

Merged `dev` commit:
`905ffd2be74d813e78515de33699d879508217a2`

The executed source and merged commit have the same Git tree. The merge adds
no source difference to the measured binary.

## Environment and command

- AWS EC2 `m6i.2xlarge`, 8 vCPUs and 30 GiB RAM;
- Ubuntu 24.04.4 LTS, kernel `6.17.0-1019-aws`;
- Intel Xeon Platinum 8375C at 2.90 GHz;
- `/tmp` on ext4 over the persistent EBS root device;
- Rust `1.96.0`, target `x86_64-linux`, release profile; and
- direct Linux execution in `/home/mario/celiumsai/hyphae`; WSL was not used.

The source worktree was clean before execution. The raw receipt was preserved
before this narrative was written:

```text
cargo run --release -p hyphae-native-runtime \
  --example microsecond_smoke -- \
  02945dfd84867ba62dbb0493c23f8cb76b121449 \
  'rustc-1.96.0-(ac68faa20-2026-05-25)'
```

The run completed in approximately eight minutes. It used warm state, memory
durability, concurrency one, and the exact schema-v15 corpus and observation
counts documented by the receipt. The local-frame route measures codec plus
embedded dispatch. It does not include UDS, named-pipe, TCP, JSON, fsync,
strict commit, proof generation, cold state, saturation, or power loss.

## Receipt validation

The checked
[schema-v15 receipt](native-microsecond-smoke-lineage-ext4-linux.json)
passed `python3 -m json.tool` and records:

- schema `hyphae.native.microsecond-smoke.v15`;
- the exact executed source commit;
- 21 operation routes;
- the expected BLAKE3 dataset digest
  `3e6fc3cd51fcea472cd977311bd4561109f6bb7e2d4d0b9df8c6e6f0e9a5773f`;
- 6,145 relational rows, 2,048 structure keys, 2,048 hash fields, 2,048
  set members, and 2,048 search documents; and
- height-two relational, structure, and search trees.

Git independently reported the same tree for the executed source and merged
`dev` commits, and `git diff --exit-code` between them was empty.

## Observations

| Operation | p50 | p99 | p99.9 | Throughput |
|---|---:|---:|---:|---:|
| Embedded structure get, 64 B | 0.059 us | 0.147 us | 0.415 us | 14,529,809 ops/s |
| Buffered structure B+tree get | 1.032 us | 2.542 us | 3.008 us | 837,086 ops/s |
| Embedded hash `HGET`, 64 B | 0.123 us | 0.307 us | 0.502 us | 7,532,611 ops/s |
| Buffered hash `HGET` | 1.813 us | 3.520 us | 4.457 us | 536,917 ops/s |
| Embedded set `SISMEMBER` | 0.123 us | 0.270 us | 0.425 us | 7,739,000 ops/s |
| Buffered set `SISMEMBER` | 2.525 us | 5.496 us | 6.489 us | 379,958 ops/s |
| Buffered BM25 `MATCH` top 1 | 29.034 us | 53.672 us | 70.453 us | 32,390 ops/s |
| Embedded prepared SQL primary key | 0.056 us | 0.077 us | 0.329 us | 17,043,849 ops/s |
| Buffered relational B+tree primary key | 1.986 us | 3.808 us | 4.357 us | 485,710 ops/s |
| Buffered primary-key scan `LIMIT 10` | 18.630 us | 30.663 us | 39.471 us | 52,418 ops/s |
| Prepared SQL primary-key scan `LIMIT 10` | 21.377 us | 31.028 us | 38.647 us | 46,045 ops/s |
| Buffered primary-key range `LIMIT 10` | 18.722 us | 28.207 us | 34.218 us | 52,534 ops/s |
| Prepared SQL primary-key range `LIMIT 10` | 23.688 us | 33.242 us | 40.256 us | 41,619 ops/s |
| Prepared SQL range plus residual `LIMIT 10` | 28.600 us | 38.191 us | 42.908 us | 34,495 ops/s |
| Prepared SQL strict prefix `LIMIT 10` | 21.434 us | 31.069 us | 38.727 us | 45,920 ops/s |
| Prepared SQL prefix plus range `LIMIT 10` | 21.600 us | 32.950 us | 39.416 us | 45,392 ops/s |
| Buffered secondary exact unique | 18.758 us | 33.820 us | 40.949 us | 51,480 ops/s |
| Prepared SQL secondary exact unique | 19.360 us | 32.997 us | 39.444 us | 50,127 ops/s |
| Unindexed text-range differential | 11,417.190 us | 16,195.958 us | 21,024.144 us | 85 ops/s |
| Prepared SQL ordered secondary range | 42.989 us | 83.120 us | 101.821 us | 22,402 ops/s |
| Local frame decode plus structure dispatch | 0.104 us | 0.121 us | 0.388 us | 9,302,026 ops/s |

All 20 hot or indexed routes remained below one millisecond through p99.9.
The deliberately unindexed differential remained millisecond work. The
ordered secondary range remained inside the provisional phase-1 bounded
indexed-SQL target of p50 at most 50 us and p99 at most 250 us for this one
scenario.

## Same-host baseline comparison

The prior [native ext4 baseline](native-ext4-linux-baseline-2026-08-02.md)
used the same host, compiler, corpus, and schema. It is therefore the only
earlier receipt suitable for an observational comparison. The comparison is
not a regression gate: CPU placement, frequency, host interference, and
repeat variance were not controlled.

- 19 of 21 p50 observations moved by no more than 3%.
- Buffered relational primary-key p50 moved from `1.749 us` to `1.986 us`
  (`+13.551%`), while its p99 moved from `4.192 us` to `3.808 us`
  (`-9.160%`). Without a repeat distribution, this is recorded but is not
  classified as either a regression or an improvement.
- The unindexed differential p50 moved from `12,833.304 us` to
  `11,417.190 us` (`-11.035%`), while p99.9 moved from `19,855.517 us` to
  `21,024.144 us` (`+5.886%`). It remains a deliberately slow comparison
  route, not a target path.
- No operation changed latency regime and no indexed route crossed the
  provisional phase-1 bound.

This receipt satisfies the first direct-Linux embedded and local-frame
latency observation for the lineage-bearing source tree. It does not time
the one-CSN commit itself and does not close G1 or G7.

## Gates still open

- process-level kill and physical power-loss validation on ext4/EBS;
- strict-durability commit and recovery timings with real synchronization;
- UDS or named-pipe transport and proof-bearing measurements;
- repeated controlled runs with an accepted regression threshold;
- cold, concurrency, saturation, interference, allocation/RSS, and hardware
  counter lanes; and
- the remaining G1 substrate, migration, pinning, and ownership work.
