<!-- SPDX-License-Identifier: Apache-2.0 -->
# Hybrid retrieval reference semantics v1

Status: normative for Hyphae `0.2`.

Hybrid retrieval executes lexical semantics v1 and durable exact vector
semantics v2 at the same snapshot checkpoint.

## Request

The request contains complete lexical and vector branch requests, a positive
candidate limit for each branch, positive `lexical_weight` and
`vector_weight` in `1..=1_000_000`, and a nonzero final result limit.
The RRF constant is fixed at `60`.

## Fusion

Branch ranks are one-based. For each object key present in either branch:

```text
lexical_contribution =
  floor(lexical_weight * 1_000_000_000 / (60 + lexical_rank))
vector_contribution =
  floor(vector_weight * 1_000_000_000 / (60 + vector_rank))
fusion_score = lexical_contribution + vector_contribution
```

An absent branch contribution is zero. The final order is fusion score
descending, then binary object key ascending. The final limit is applied only
after deduplication and sorting.

## Branch absence and abstention

If one branch has no candidates or policy-abstains, the other may produce a
single-modality match outcome. The response records the absent branch reason.
If both branches abstain, hybrid returns typed abstention containing both
reasons.

Errors do not degrade to single-modality execution. An invalid request,
unknown definition, stale index, budget failure, or timeout in either branch
fails the complete hybrid request without partial results.

## Resource limits

The complete hybrid deadline is the saturating sum of the configured lexical
and exact-vector timeouts. The lexical branch is bounded by its own timeout and
the remaining total. The vector branch is then bounded by its own timeout and
the remaining total. Fusion must complete before that same total expires.

Proof-producing facade methods with explicit maintenance limits spend the
remaining hybrid total on snapshot creation as well. The legacy proof method
retains its `0.2.0` signature and creates the snapshot afterward under the
engine's stored maintenance policy.

## Explanation

Each result records lexical rank and score when present, vector rank and score
when present, both integer contributions, fusion score, and final rank.
Proof verification recomputes both branches and every explanation field.
