<!-- SPDX-License-Identifier: Apache-2.0 -->
# Exact retrieval reference semantics v1

Status: normative for the Hyphae `0.1.0` reference executor.

The executor accepts a query vector and complete candidate shards directly.
It does not load an embedding model, contact a provider, or require a network.
Optional provider adapters may produce vectors later but cannot alter these
ranking and abstention rules.

## Vector validity and scoring

- query and candidate vectors are nonempty, finite, nonzero, and have exactly
  the same dimension;
- dimension is bounded before candidate scoring;
- cosine similarity is computed by scaling each vector by its maximum absolute
  component before normalization, avoiding overflow for extreme finite values;
- scores are clamped only for final floating-point roundoff into `[-1, 1]`;
- malformed candidates fail the complete request; they are never silently
  skipped.

## Global merge

Every shard is scanned before final limiting. Candidates sort by cosine score
descending and then binary key ascending. Keys must be globally unique. No
per-shard top-k truncation is conforming to the reference semantics.

## Abstention

Abstention is a successful, typed outcome with evidence:

- no candidates produces `NoCandidates`;
- best score below the inclusive minimum produces `BelowThreshold`;
- when a runner-up exists, a best-minus-runner-up margin below the configured
  minimum produces `Ambiguous`;
- one candidate above threshold is not ambiguous because no runner-up exists.

Threshold policy runs on the global ranking before the final result limit.
Errors such as invalid dimension, non-finite values, duplicate keys, exhausted
budget, or timeout are not abstentions.

## Budgets

Request shape bounds dimension and final result count before scanning. The
candidate budget and monotonic timeout cover all shards. Exceeding either
returns no partial ranking.
