<!-- SPDX-License-Identifier: Apache-2.0 -->
# Hyphae 0.2 retrieval benchmark methodology

Status: normative local evidence procedure

This document defines how Hyphae records retrieval evidence for the `0.2`
release line. The report is evidence about one executable and one environment;
it is not a comparative marketing claim.

## Harness

Run:

```text
python tools/run_retrieval_benchmark.py \
  --output docs/gates/evidence/0.2-retrieval-benchmark-<platform>.json \
  --iterations 7 \
  --scenario 10000:128:10 \
  --scenario 10000:384:10 \
  --scenario 10000:768:10 \
  --scenario 100000:128:10
```

Each scenario uses a deterministic synthetic mixed document/vector corpus. The
harness records:

- record and vector ingest throughput with durable commits and fsync enabled;
- exact, lexical, and hybrid p50, p95, and p99 latency;
- process-tree peak RSS;
- logical payload and on-disk bytes;
- caught-up reopen and full derived-index rebuild;
- snapshot, compaction, backup, restore, and restored-open latency;
- record/vector update and delete latency;
- exact, lexical, and hybrid proof generation, bytes, and offline verification
  latency.

The 10K scenarios cover dimensions 128, 384, and 768. The 100K scenario is the
minimum scale case. The small default matrix is retained for CI smoke. The
checked compatibility corpus and unit tests separately pin ties, near-ties,
empty spaces, sparse spaces, malformed inputs, and abstention.

## Quality oracles

The harness rejects the report unless:

- durable exact retrieval has `recall@k = 1.0` against the exhaustive canonical
  scorer;
- the materialized inverted lexical index exactly equals the BM25F reference
  executor;
- durable hybrid output exactly equals the deterministic RRF reference
  executor;
- updates and deletes are immediately visible;
- deleting the derived Redb file and replaying durable authority reproduces the
  same complete rankings.

The report expresses exact recall in millionths; `1000000` means `1.0`.

## Interpretation

The timing clock is `std::time::Instant`. Percentiles use the nearest-rank
sample after one untimed warm-up. With seven samples, p95 and p99 are both the
maximum sample; this is intentionally conservative but not a substitute for a
long statistical study. Environment metadata and peak RSS are captured by the
Python wrapper.

No benchmark hides fsync, proof creation, witness creation, verification,
compaction, or recovery costs. Proof verification reexecutes against the
canonical snapshot witness and does not use the materialized indexes.

## Regression policy

- Keep each checked evidence report as the baseline for its exact source
  candidate and environment.
- Fail CI only on correctness divergence or on performance thresholds measured
  on controlled, stable hardware.
- On shared or variable hosts, publish observations and use only broad
  operational ceilings for load/soak safety gates.
- Do not compare different machines, profiles, corpus definitions, or compiler
  versions as if they were a regression.
- Do not claim that `0.2` is faster than an ANN database. Hyphae `0.2`
  prioritizes exactness, deterministic proofs, and reconstructible indexes;
  ANN remains outside this release.
