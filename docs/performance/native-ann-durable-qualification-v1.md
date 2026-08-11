<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->

# Native ANN durable local qualification v1

Status: local qualification gate; no release or G7 closure claim

This gate separates the selected durable ANN route from the process-local bulk
bakeoff. It accepts only evidence produced from one clean exact source SHA, one
canonical corpus, and one durable `partitioned-hnsw-v1` build/view chain.

The receipt remains diagnostic and declares no closure. Running the checker in
`qualification` mode produces only a local qualification candidate. Missing
evidence produces an explicit `no closure` diagnostic and a non-zero exit.

## Required evidence

- the source commit and tree plus the corpus generator, digest, size, dimension,
  and one of the qualified metrics: squared L2, cosine, or negative dot;
- input, aggregate, expected base/view, published base/view, and routing-policy
  identities, including `metric-bound-adaptive-v1`;
- logical partitions independently from planned workers and executed worker
  batches, with the durable lifecycle-safe 111-partition ceiling enforced;
- per-query selected-route recall of at least `950000` ppm;
- every qualification query certified within the preferred partition budget,
  with zero adaptive full-fanout fallbacks in the accepted corpus and the
  maximum searched partition count equal to that preferred budget;
- byte-identical full-fanout and default-route result identities;
- initial reopen reproducing the published build and selected results;
- visible upserts and deletes that change the view without replacing the base;
- consolidation consuming that exact delta view and publishing a clean base;
  and
- final reopen reproducing the consolidated identities and visible results.

The full-fanout comparison is against the existing default approximate route.
The flat exact oracle remains a separate identity used to calculate selected
recall; the gate does not incorrectly require approximate results to equal the
exact oracle.

## Fail-closed modes

`diagnostic` mode accepts deliberately absent quality or lifecycle sections
only when `missing_gate_evidence` exactly discloses every absent gate. It never
emits a qualification candidate. Missing quality must disclose adaptive-route
certification, selected recall, and full-fanout equality independently.

`qualification` mode requires an empty missing-evidence list and the complete,
internally consistent identity chain:

```sh
PYTHONPATH=. python3 tools/check_native_ann_durable_qualification.py \
  native-ann-durable-qualification.json \
  "$(git rev-parse HEAD)" \
  --expected-corpus-identity "<canonical-64-hex-corpus-digest>" \
  --mode qualification
```

The schema is `native-ann-durable-qualification-v1.schema.json` under
`contracts/json-schema/`.
Passing this local gate does not substitute for the frozen corpus, observation
counts, interference cells, hardware counters, receipts, or independent
checkers required by G7.

## Reproducible local smoke

The focal runner creates one fresh durable database for each accepted metric,
builds the same 512-vector, eight-dimensional, 64-logical-partition corpus,
and emits three raw receipts plus one suite audit. Twelve deterministic center,
exact-boundary, and near-boundary queries must certify within the fixed
32-partition preferred budget for every metric:

```sh
PYTHONPATH=. python3 tools/run_native_ann_durable_qualification.py \
  --expected-commit "$(git rev-parse HEAD)" \
  --output-dir /tmp/hyphae-ann-durable-qualification
```

The controller requires a clean exact commit and tree, pinned corpus identities,
all three metrics exactly once, full-fanout/default equality, zero adaptive
fallbacks, and the complete reopen/delta/consolidation/reopen chain. A
consolidation that exposes `single-generation-fallback` fails qualification;
the runner still emits the raw receipt so the discrepancy remains inspectable.

The G7 readiness workflow runs this qualification on a GitHub-hosted runner.
The dedicated self-hosted benchmark job depends explicitly on both authority
validation and this exact-SHA qualification succeeding. The readiness workflow
contains no infrastructure provisioning command; provisioning remains an
external, explicitly authorized operation.

This suite is correctness/qualification evidence. Its current-root query calls
may hydrate index state and therefore provide no hot-path or microsecond latency
claim. The separate G7 runner retains its frozen `1,000,000 x 384` corpus,
warmup, observations, concurrency cells, interference, counters, and hardware
authority. Neither the 512-vector qualification nor its audit can close or
replace G7.
