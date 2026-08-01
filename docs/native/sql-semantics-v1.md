# Hyphae SQL semantics v1

Status: normative target contract; an exact binary first slice for `CREATE
TABLE`, primary-key `INSERT`, `UPDATE`, `DELETE`, parameterized `SELECT`,
prepared binding, `BEGIN`/`COMMIT`, and rollback is implemented
experimentally; G2 remains open

Hyphae SQL is a native SQL implementation. Its familiar syntax does not imply
an embedded PostgreSQL engine or PostgreSQL-specific semantics.

## Language pipeline

Hyphae owns lexer, parser, binder, rewriter, logical planner, cost optimizer,
physical planner and executor. Parsed input becomes a catalog-bound plan; no
public wire AST or third-party engine is the execution authority.

Prepared plans are keyed by normalized statement fingerprint, parameter types,
catalog version and relevant configuration. Execution uses stable object and
column IDs.

## V1 grammar families

- transaction control: `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`;
- DDL: `CREATE`, `ALTER`, `DROP` for schemas, tables, views and indexes;
- DML: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`;
- query composition: joins, subqueries, common table expressions, set
  operations, grouping, `HAVING`, ordering, limits and window functions;
- expressions: literals, parameters, casts, `CASE`, boolean logic, comparison,
  arithmetic, scalar and aggregate functions; and
- introspection: `EXPLAIN`, catalog views and execution statistics.

The first implementation slice is one fixed binary table shape, primary-key
`INSERT`, `UPDATE ... SET row = ? WHERE primary_key = ?`, `DELETE ... WHERE
primary_key = ?`, prepared parameterized point `SELECT`, `BEGIN`, `COMMIT`,
and `ROLLBACK`. Updates publish a new copy-on-write root; deletes publish a
canonical tombstone. Retained snapshots continue to resolve their historical
roots. New directories retain an explicit per-key physical version chain under
the current root and close each superseded copy's `end_csn`; the earlier
inline-row directory format remains supported. Retention and vacuum remain
pending. This slice does not close relational gate G2.

## Null and boolean semantics

SQL uses three-valued logic: `TRUE`, `FALSE`, `UNKNOWN`. Ordinary comparison
with SQL `NULL` returns `UNKNOWN`; `IS NULL` and `IS NOT NULL` test nullness.
Filtering retains only `TRUE`. Constraint and join semantics are specified in
terms of the same truth table.

JSON null is a non-null JSON value.

## Names, types, and casts

Name resolution follows the catalog specification. Type behavior follows
[canonical types v1](types-v1.md). Implicit casts are lossless only. All other
casts are explicit and checked.

V1 text ordering is binary UTF-8. A query that depends on a future collation
binds its versioned collation ID in the plan.

## Relational operators

The logical algebra includes scan, point lookup, filter, project, inner/left/
right/full/cross/semi/anti join, aggregate, window, distinct, union/intersect/
except, sort, limit, insert, update, delete and merge.

Physical access includes heap/page scan, hash lookup, B-tree lookup/range,
bitmap combination, nested-loop join, hash join, merge join, bounded top-k,
external sort and spill. Operators expose cardinality, cost and memory budgets.

## Relational access to other engines

Hyphae SQL binds structure and search sources through native table-valued
operators:

```text
STRUCTURE(namespace => <expr>, name => <expr>)
SEARCH(index => <expr>, query => <expr>, mode => <expr>)
VECTOR_SEARCH(index => <expr>, vector => <expr>, k => <expr>)
```

Their returned schemas are catalog definitions, not generic JSON. Search
sources include stable object ID, score, rank and engine-specific explanation.
The optimizer may push filters, limits and stable-ID joins into the owning
engine.

These operators provide relational access without copying engine-owned data
into SQL rows.

## Transactions and errors

SQL statements run inside the common MVCC transaction. Statement failure rolls
back that statement's private changes. An explicit transaction remains failed
until rollback when an error can invalidate later semantics.

Errors have stable Hyphae codes, human text, statement byte span, optional
object/column ID, and retry classification. Compatibility SQLSTATE mapping is
an edge concern, not the native identity.

## Determinism

Without `ORDER BY`, row order is unspecified. With a non-unique order, stable
row/object ID is the final deterministic tie-breaker for proofs and pagination.
Volatile functions are marked and cannot participate in immutable indexes or
deterministic proofs without captured inputs.

## Verification

Required evidence includes parser/binder negative fixtures, SQLLogicTest for
the supported common subset, metamorphic equivalence tests, three-valued logic
goldens, constraint and isolation litmus tests, TPC-H result correctness,
TPC-C ACID tests, plan-cache invalidation, spill/resource limits, and
cross-engine pushdown correctness.
