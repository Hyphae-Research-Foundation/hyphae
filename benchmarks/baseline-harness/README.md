# SPDX-License-Identifier: Apache-2.0
# Baseline harness

Standalone benchmark workspace comparing the Hyphae native engines against
widely deployed single-purpose baselines on dedicated hardware. It is
deliberately **not** a member of the root workspace: SQLite, DuckDB, Redis,
and Tantivy exist here only as measurement subjects and never enter the
product dependency graph.

## Suites

| Suite | Hyphae surface | Baseline | Variable isolated |
|---|---|---|---|
| `sql` | native SQL (prepared PK reads, strict/batched writes) | SQLite (WAL, `synchronous=FULL`), DuckDB (default WAL) | point-indexed OLTP shape |
| `keyspace` | native structures (embedded) | Redis over UDS (`appendfsync always` and `everysec`) | transport + fsync policy |
| `lexical` | native BM25 (`match_latest_text`) | Tantivy (default BM25) | inverted-index ingest + top-10 query |
| `ablation` | Hyphae only | — | fsync policy, batch materialization vs delta, per-engine commit composition |

Workloads are deterministic (seeded xorshift64*), byte-identical across
engines, and every receipt embeds the host fingerprint, source commit, and
per-phase p50/p95/p99/p99.9 exclusive latencies plus throughput.

## Fairness rules

- Identical row/document/key contents and identical operation sequences.
- Durability compared like-for-like: fsync-per-commit phases against
  fsync-per-write baselines; no-fsync-ack phases against `everysec`.
- Prepared statements everywhere; each engine uses its fastest documented
  local read path.
- Baselines run their defaults where a default is the documented production
  posture; deviations are stated in the receipt.
- DuckDB is included as a familiar reference point, not as an OLTP victim:
  it is a columnar OLAP engine and the receipt says so.

## Running on dedicated hardware

```bash
# on the metal host (Ubuntu 24.04, root), with the repo at /root/hyphae:
bash benchmarks/baseline-harness/scripts/run-metal.sh
```

The script formats the spare local NVMe instance-store disk, pins the
performance governor, starts two UDS-only Redis servers (one per fsync
policy), builds the harness in release mode, and writes one JSON receipt per
suite to `/root/bench-results/`.

A quick local smoke (small scale, no Redis):

```bash
cargo run --release --manifest-path benchmarks/baseline-harness/Cargo.toml -- \
  all /tmp/hyphae-bench-scratch /tmp/hyphae-bench.json --scale small
```

Receipts feed the evidence documents under `docs/gates/evidence/`; raw JSON
outputs are attached there verbatim, never post-processed by hand.
