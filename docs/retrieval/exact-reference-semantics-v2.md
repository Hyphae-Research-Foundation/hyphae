<!-- SPDX-License-Identifier: Apache-2.0 -->
# Durable exact retrieval reference semantics v2

Status: normative for Hyphae `0.2` durable retrieval.

This semantics version operates only on durable Q15 vectors from one named
space. It does not contact a provider and does not alter
[`reference-semantics-v1.md`](reference-semantics-v1.md).

## Request

The canonical request contains, in order:

1. canonical vector-space identifier;
2. dimension and exactly that many signed Q15 query elements;
3. nonzero result limit;
4. signed `minimum_score_nanos`;
5. unsigned `minimum_margin_nanos`.

The query must be nonzero and cannot contain `-32768`.

## Scoring and ordering

Every vector in the space is scored using the integer cosine-nanos algorithm
in ADR-0015. The executor scans the complete space before final limiting.
Results sort by score descending, then binary object key ascending.

The best result must have `score_nanos >= minimum_score_nanos`. When a runner-up
exists, `best.score_nanos - runner_up.score_nanos` must be greater than or
equal to `minimum_margin_nanos`.

## Outcomes

`matches` contains ordered object keys, integer scores, and the global scanned
candidate count.

`abstained` contains one stable reason, optional best and runner-up scores, and
the scanned count:

- `no_candidates`;
- `below_threshold`;
- `ambiguous`.

Unknown space, wrong dimension, invalid vector, stale materialization,
duplicate logical identity, exhausted budget, or timeout is an error.

## Resource limits

Dimension and result count are checked before scan. Candidate count, aggregate
candidate key/vector bytes, and one monotonic deadline cover scan, decode,
scoring, ordering, abstention, and result creation. Offline proof replay
preflights the same count/byte policy and deadline before allocating and cloning
the candidate collection. A limit failure returns no ranking.

## Golden case

For query `[32767, 0]`, candidate `a = [32767, 0]` scores
`1_000_000_000`; candidate `b = [0, 32767]` scores `0`; candidate
`c = [-32767, 0]` scores `-1_000_000_000`. The order is `a`, `b`, `c`.
