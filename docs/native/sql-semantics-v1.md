# Hyphae SQL semantics v1

Status: normative target contract; catalog-typed `CREATE TABLE`, primary-key
`INSERT`, projection and parameterized primary-key `SELECT`, prepared binding,
exact-key secondary indexes, `CREATE [UNIQUE] INDEX`, bounded `EXPLAIN`,
`BEGIN`/`COMMIT`, and rollback are implemented experimentally. The legacy
binary shape also supports `UPDATE` and `DELETE`; G2 remains open

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

The current implementation slice accepts catalog-typed primitive columns,
inline or table-level ordered primary keys, explicit nullability, named-column
`INSERT`, `SELECT *` or named projection, conjunctions covering exactly the
primary key or one complete secondary-index key, and catalog-bound prepared
point lookup. Predicate text order may differ from catalog key order.
Primary-key and secondary-index components use the canonical memcomparable
codec; row values use the catalog-ordered `HYTUPL01` tuple. Type, domain,
nullability, duplicate-column, missing-column, and incomplete-key failures
occur before the row mutation.

`CREATE INDEX name ON table (columns)` and `CREATE UNIQUE INDEX` create a
stable catalog object and a physical relational B+tree namespace. Creation
backfills admitted rows atomically. Later inserts derive index entries from
the catalog-bound tuple in the same transaction and WAL/CSN publication.
Unique indexes reject duplicate non-null composite keys before statement or
commit publication. The current SQL spelling always uses `NULLS DISTINCT`:
any null component exempts the composite key from uniqueness, while ordinary
`column = NULL` remains `UNKNOWN` and returns no rows.

`EXPLAIN SELECT` currently reports only the admitted exact access path:
`PrimaryKeyLookup(table=<id>)` or
`SecondaryIndexLookup(table=<id>,index=<id>)`. It is deterministic
introspection, not yet a logical tree, cost estimate, row estimate, or runtime
statistics report.

The grammar currently recognizes boolean, signed and unsigned fixed-width
integers, `DECIMAL(p,s)`, binary32/binary64, text, binary, date, time,
timestamp, interval, UUID, and JSON declarations. Primitive value codecs are
executable; JSON is declaration-only until its canonical scalar validator
exists. General expressions, literals, casts, heap/index scans, range access,
constraints beyond primary key/nullability and the first unique index, and
typed `UPDATE`/`DELETE` remain pending. The current access binder requires the
predicate column set to equal a complete primary or secondary key. It does not
perform prefix, residual-filter, bitmap, or cost-based access selection.

The historical table shape `(primary_key BINARY PRIMARY KEY, row BINARY)`
retains its byte-for-byte raw row route, allocation-free prepared binary point
lookup, and fixed `UPDATE ... SET row = ? WHERE primary_key = ?` and `DELETE`
behavior. Updates publish a new copy-on-write root; deletes publish a canonical
tombstone. Retained snapshots continue to resolve their historical roots. New
directories retain an explicit per-key physical version chain under the
current root and close each superseded copy's `end_csn`; the earlier inline-row
directory format remains supported. Retention and vacuum remain pending. This
slice does not close relational gate G2. Typed indexed `UPDATE` and `DELETE`
are rejected until their old/new physical projection and WAL semantics are
implemented; the runtime never permits them to leave a stale index.

Secondary-index definitions and entries are durable physical state, but the
current general `NativeSnapshot` SQL path materializes the bounded relational
snapshot before executing the lookup. Direct pinned physical secondary-index
lookup, range cursors, and their microbenchmarks remain separate work.

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

Errors have stable Hyphae codes. The current slice implements `HYSQL001`
through `HYSQL012` for syntax, parameters, stale plans, columns, types,
nullability, primary-key binding, stored tuples, catalog-kind mismatch, absent
implemented access paths, and unique-index violations. Statement byte spans,
optional object/column IDs, retry classification, and compatibility SQLSTATE
mapping remain target requirements.

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
