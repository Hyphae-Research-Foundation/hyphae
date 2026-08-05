# G2 metamorphic SQL corpus

Status: bounded implementation evidence; G2 remains open.

The versioned corpus at
`crates/hyphae-native-runtime/tests/corpus/g2-metamorphic.json` freezes a
deterministic dataset and semantically equivalent query pairs. The native test
runner currently checks:

- commutativity of `AND` and `OR`;
- double negation;
- De Morgan rewriting;
- primary-key range-bound ordering; and
- CTE identity for a bounded materialized projection.

Every pair must return the same output schema and byte-equivalent logical rows.
A second seeded generator (`seed = 20260804`) executes 256 deterministic cases
with three rewrites per case (768 comparisons) over a 64-row typed dataset,
covering boolean commutation, range-bound permutation, and double negation.
This is still not the complete randomized
metamorphic evidence required for G2. Closure still requires generated typed
datasets, seeded reproducibility, broad expression and join rewrites, failure
shrinking, exact corpus/generator digests, and hosted exact-SHA receipts.
