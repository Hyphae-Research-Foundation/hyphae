<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Raw receipts — phase-1 optimization before/after (2026-08-30)

Verbatim `benchmarks/baseline-harness` output backing the
[phase-1 optimization evidence](../phase1-optimization-2026-08-30.md).
Nothing here is hand-edited; every receipt embeds its host fingerprint,
source commit label, and per-phase latency distributions.

Naming: `B` = baseline tree (`main` @ `8aeb6ea`), `A` = the same tree plus
the phase-1 optimization set. Both binaries were verified distinct before
running.

| Files | What they are |
|---|---|
| `sql-B1/A1/B2/A2.json`, `key-B1/A1/B2/A2.json` | Interleaved B-A-B-A series on one droplet (one discarded warm-up preceded it): SQL point workload and keyspace point workload, 1M rows/keys each. This series is what exposed the cold-first-run artifact and established the warm-path null result |
| `lexical-before-small.json`, `lexical-after-small.json` | 20k-document lexical suite, both arms (the scale both trees complete) |
| `lexical-after.json` | 100k-document lexical suite, optimized arm only — the baseline arm filled a 161 GB disk (ENOSPC) before completing, which is itself the write-amplification result |
| `ablation-before.json`, `ablation-after.json` | Hyphae-only ablations: durability classes, materialized-vs-delta transaction shape, per-engine commit composition |

Host: DigitalOcean c-16 droplets (virtualized) — relative same-host A/B
only, environment class 2 under [claims](../../../product/claims.md).
Absolute comparative numbers live in the dedicated-hardware receipt
([baseline-i7i-metal-2026-08-30](../baseline-i7i-metal-2026-08-30.md)).
