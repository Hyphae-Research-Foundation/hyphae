# G2 SQLLogicTest corpus

Status: bounded implementation evidence; not promoted into the G2 closure at
`a839037`, which claims no universal SQL or official benchmark result.

`crates/hyphae-native-runtime/tests/corpus/g2-smoke.slt` is a checked-in,
SQLLogicTest-compatible corpus executed directly against the Hyphae Native SQL
engine. It currently covers:

- typed table creation;
- inserts, updates and deletes;
- deterministic ordered reads;
- exact-key reads;
- bounded non-recursive CTEs; and
- bounded `ROW_NUMBER` windows.

The runner validates statement/query shape and expected rows and rejects unknown
record headers. This bounded corpus proves the harness and the admitted SQL
slice; it is not yet the broad SQLLogicTest conformance evidence required to
close G2. The corpus must grow alongside the grammar, constraints, joins,
transactions and expression semantics, then produce an exact-SHA hosted
receipt with statement/query counts and a content digest.
