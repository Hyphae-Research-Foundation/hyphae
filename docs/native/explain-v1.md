# Native product EXPLAIN v1

Status: accepted G6 planning contract; implementation incomplete

`EXPLAIN` reports a stable typed plan for bounded SQL, structure, lexical,
vector, hybrid, and convergence operations. Human-readable text is a rendering
of the typed plan and is not the compatibility authority.

## Plan identity

Every explanation includes:

- explanation version;
- visible CSN and catalog version;
- referenced stable object IDs and definition digests;
- logical operators and selected physical strategies;
- admitted limits, predicates, projections, sort, aggregation, and pushdown;
- exact, ANN, or hybrid classification;
- filter strategy and eligible-set estimate when applicable;
- ANN metric, generation, breadth, candidate and rerank policy;
- whether execution occurred; and
- optional bounded execution counters only for `EXPLAIN ANALYZE`.

The plan never claims a predicate, limit, aggregation, or filter was pushed
down unless the physical executor applied it before the reported boundary.

## Stability

Operator and strategy identifiers are append-only within v1. Cost estimates
are advisory and versioned separately from semantic identities. Plan output is
deterministically ordered and contains no host paths, pointer values, or
unstable debug formatting.

## Verification

Cross-surface goldens prove identical typed plans for embedded, protocol, CLI,
HTTP, and SDK calls. Catalog changes invalidate stale plans. ANN explanations
are checked against execution receipts and exact-oracle metrics.
