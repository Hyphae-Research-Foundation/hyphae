<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native product EXPLAIN v1

Status: implemented G6 bounded contract

`EXPLAIN` reports stable product-owned variants for bounded SQL, convergence,
ANN, and hybrid operations. SQL retains versioned opaque runtime plan text plus
the visible CSN, catalog version, and `executed=false`; consumers must not parse
that text as a compatibility ABI. Convergence, ANN, and hybrid explanations are
fully typed compatibility authority.

## Plan identity

The closed family exposes the following applicable fields. SQL currently
exposes version, visible CSN, catalog version, bounded physical plan text, and
`executed=false`; typed convergence, ANN, and hybrid variants expose their
respective stable strategies and limits:

- explanation version where the variant has an independently versioned format;
- visible CSN and catalog version;
- referenced stable object IDs;
- logical operators and selected physical strategies;
- admitted limits, predicates, projections, sort, aggregation, and pushdown;
- exact, ANN, or hybrid classification;
- filter strategy and eligible-set estimate when applicable;
- ANN generation, breadth, candidate and rerank policy;
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

The canonical native protocol encodes every typed variant, so HTTP `/v2` uses
the identical bytes and local-protocol goldens cover all variants. Catalog
changes invalidate stale SQL plans. ANN explanations are copied from actual
execution receipts; hybrid explanations encode the admitted branch and fusion
policy without executing it.
