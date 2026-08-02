# Hyphae SQL semantics v1

Status: normative target contract; catalog-typed `CREATE TABLE`, primary-key
`INSERT`, projection and parameterized primary-key `SELECT`, prepared binding,
exact-key secondary indexes, `CREATE [UNIQUE] INDEX`, bounded `EXPLAIN`,
direct current-root prepared primary/secondary lookup, exact-primary-key typed
`UPDATE`/`DELETE`, bounded primary-key table/range scan, `BEGIN`/`COMMIT`,
rollback, and the parameterized residual-filter slice below are implemented
experimentally. The first catalog-bound scalar literal slice is also
implemented for `SELECT` filters and exact-primary-key DML. One exact indexed
`INNER JOIN` shape is implemented as described below. G2 remains open

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
point lookup. It also accepts bounded no-predicate scans:

```text
SELECT <projection>
FROM <table>
[ORDER BY <complete-primary-key-in-catalog-order>]
LIMIT <nonnegative-integer>
```

`LIMIT` is mandatory for this first scan shape. `ORDER BY`, when present, must
name the complete primary key in catalog order; descending order and
expressions remain unsupported. Without `ORDER BY`, SQL ordering is
unspecified even though the current physical operator walks canonical primary
keys. Predicate text order may differ from catalog key order.
Primary-key and secondary-index components use the canonical memcomparable
codec; row values use the catalog-ordered `HYTUPL01` tuple. Type, domain,
nullability, duplicate-column, missing-column, incomplete-key, and invalid
scan-order failures occur before execution or row mutation.

The first primary-key range shape is:

```text
SELECT <projection>
FROM <table>
WHERE <complete-primary-key> { > | >= | < | <= } <parameters>
[AND <complete-primary-key> { > | >= | < | <= } <parameters>]
[ORDER BY <complete-primary-key-in-catalog-order>]
LIMIT <nonnegative-integer>
```

A single-column key uses `id >= ?`. A composite key uses the SQL row form
`(tenant, sequence) >= (?, ?)`. Every comparison must name the complete
primary key in catalog order. At most one lower and one upper bound are
accepted. `LIMIT` is mandatory; `ORDER BY`, when present, must be the complete
ascending primary key. Bound parameters occur in predicate text order.
Inclusive, exclusive, one-sided, inverted, and equal-open ranges have explicit
semantics: inverted and empty ranges return no rows rather than raising or
panicking. Equality predicates retain their exact point/index access contract
when they cover a complete primary or secondary key.

The residual-filter slice admits parameterized and literal scalar predicates:

```text
<filter> ::= <filter> OR <term> | <term>
<term> ::= <term> AND <factor> | <factor>
<factor> ::= NOT <factor> | ( <filter> ) | <predicate>
<predicate> ::= <column> { = | <> | != | < | <= | > | >= } <scalar>
              | <column> IS [NOT] NULL
<scalar> ::= ? | NULL | TRUE | FALSE | <signed-integer> | <text-literal>
```

`NOT` binds tighter than `AND`, and `AND` binds tighter than `OR`. Parentheses
override that precedence. Complete-primary-key row comparisons remain admitted
only as top-level `AND` range terms; they are not general row expressions.
Text literals use single quotes and double an embedded quote (`'Mario''s'`).
Boolean, text, and integer literals bind only to the same catalog logical
family. Integer range/domain checks use the declared signed/unsigned width.
SQL `NULL` is admissible for every comparison and evaluates to `UNKNOWN`.
Decimal/floating/binary/temporal/UUID/JSON literals, casts, arithmetic,
functions, and column-to-column comparisons remain outside this milestone.
The same scalar operands are admitted by the exact-primary-key mutation slice
defined below.

The binder may extract complete primary-key equality, one complete
secondary-index equality, or complete-primary-key lower/upper bounds from
top-level `AND` terms. Every remaining term is a residual filter. When no
admitted exact or range access exists, execution uses a bounded primary-key
scan. `LIMIT` is mandatory for every scan or range plan and is applied to rows
that evaluate to `TRUE`, not to rows merely examined. Exact primary/secondary
access retains its existing no-`LIMIT` form. `ORDER BY`, when admitted for a
scan or range, remains the complete ascending primary key.

All parameter arity and catalog logical-type checks occur before storage
traversal. A SQL `NULL` parameter is type-admissible in a comparison and makes
that comparison `UNKNOWN`; it is not an invalid scalar type. Physical current-
root filtering traverses the bound B+tree visitor and stops after the requested
number of matching rows without constructing a complete relation or
`MaterializedState`.

Typed mutation accepts:

```text
INSERT INTO <table> (<column> [, ...])
VALUES (<scalar> [, ...])

UPDATE <table>
SET <non-primary-key-column> = <scalar> [, ...]
WHERE <primary-key-column> = <scalar> [AND ...]

DELETE FROM <table>
WHERE <primary-key-column> = <scalar> [AND ...]
```

`<scalar>` has the same first-slice definition used by filters: `?`, `NULL`,
`TRUE`, `FALSE`, signed base-10 integer, or single-quoted text. Parameters and
literals may be mixed; parameter positions follow statement text order.
The `WHERE` column set must equal the complete primary key; predicate text
order may differ from catalog key order. Update parameters occur in assignment
order followed by predicate order. Assignment columns are unique, values are
type/null checked, and primary-key assignment fails with `HYSQL013` before
mutation. A missing key returns `rows_affected = 0`. An admitted update or
delete returns one and publishes the row version plus every old/new secondary
projection atomically under the same root and CSN. Failure, including a new
unique-key collision, leaves the statement's private row and indexes
unchanged.

`CREATE INDEX name ON table (columns)` and `CREATE UNIQUE INDEX` create a
stable catalog object and a physical relational B+tree namespace. Creation
backfills admitted rows atomically. Later inserts, updates, and deletes derive
their live/tombstone index changes from catalog-bound tuples in the same
transaction and WAL/CSN publication.
Unique indexes reject duplicate non-null composite keys before statement or
commit publication. The current SQL spelling always uses `NULLS DISTINCT`:
any null component exempts the composite key from uniqueness, while ordinary
`column = NULL` remains `UNKNOWN` and returns no rows.

The first exact native join shape is:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<single-column-primary-key>
WHERE <exact-left-key-filter>
```

The projection must contain explicit qualified columns; `SELECT *`, aliases,
ordering, and limits are not admitted for this shape. The left filter must
expose a complete primary-key equality or a complete unique-secondary-index
equality, so at most one left row is reached. Residual predicates may further
filter that row. The qualified right join column must be the relation's
complete single-column primary key, and both join columns must have the same
catalog logical type.

Execution performs an indexed nested lookup inside one captured root set and
visible CSN. SQL `NULL` on the left join column produces no match. An absent
right primary key also produces no match. Transaction execution sees private
row changes, a retained snapshot remains historical, and
`execute_prepared_latest` performs both physical lookups without constructing
`MaterializedState`. Non-unique left access, composite/right-secondary access,
aliases, general column expressions, join reordering, hash/merge algorithms,
outer/semi/anti joins, and cross-engine table-valued sources remain pending.

The bounded multirow extension is:

```text
SELECT <qualified-column> [, ...]
FROM <left-table>
INNER JOIN <right-table>
  ON <left-table>.<column> = <right-table>.<single-column-primary-key>
[WHERE <left-filter>]
[ORDER BY <complete-left-primary-key-in-catalog-order>]
LIMIT <nonnegative-integer>
```

The left input is a full primary-key scan or a complete primary-key range.
`LIMIT` is mandatory and applies to joined output rows, not to examined left
rows. A left row filtered to `FALSE`/`UNKNOWN`, a null join key, or an absent
right primary key does not consume the limit. `ORDER BY`, when present, must
be the complete ascending left primary key. `LIMIT 0` binds the complete plan
and returns no rows.

Private execution can join uncommitted left and right rows. Retained execution
uses historical materialized relation maps. Current execution uses the
bound-aware physical B+tree visitor and every right lookup shares the one
captured root set and CSN. The visitor stops after the requested output
cardinality; it does not materialize the complete left relation.

`EXPLAIN SELECT` currently reports only the admitted access path:
`PrimaryKeyLookup(table=<id>)` or
`SecondaryIndexLookup(table=<id>,index=<id>)`, or
`PrimaryKeyScan(table=<id>,limit=<n>)`, or
`PrimaryKeyRangeScan(table=<id>,lower=<kind>,upper=<kind>,limit=<n>)`, or
`IndexedInnerJoin(left_table=<id>,left_access=<access>,right_table=<id>,right_access=primary-key)`.
Join left access is `primary-key`, `unique-secondary(index=<id>)`,
`primary-key-scan(limit=<n>)`, or
`primary-key-range(lower=<kind>,upper=<kind>,limit=<n>)`.
Range kinds are `inclusive`, `exclusive`, or `unbounded`. This is deterministic
introspection, not yet a logical tree, cost estimate, row estimate, or runtime
statistics report. A plan appends `,residual=true` inside the closing
parenthesis when an access path has non-access predicates; a filtered full scan
is residual by definition.

The grammar currently recognizes boolean, signed and unsigned fixed-width
integers, `DECIMAL(p,s)`, binary32/binary64, text, binary, date, time,
timestamp, interval, UUID, and JSON declarations. Primitive value codecs are
executable; JSON is declaration-only until its canonical scalar validator
exists. General expressions beyond the admitted residual-filter slice,
remaining literal families, casts, partial primary-key prefix ranges,
secondary ranges,
descending scans, offsets, and constraints beyond primary key/nullability and
the first unique index remain pending. Typed mutation does
not yet change primary keys, use a secondary access path, evaluate general
expressions, or update multiple rows. Prefix, bitmap, and cost-based access
selection remain pending.

The historical table shape `(primary_key BINARY PRIMARY KEY, row BINARY)`
retains its byte-for-byte raw row route, allocation-free prepared binary point
lookup, and fixed `UPDATE ... SET row = ? WHERE primary_key = ?` and `DELETE`
behavior. Updates publish a new copy-on-write root; deletes publish a canonical
tombstone. Retained snapshots continue to resolve their historical roots. New
directories retain an explicit per-key physical version chain under the
current root and close each superseded copy's `end_csn`; the earlier inline-row
directory format remains supported. Retention and vacuum remain pending. This
slice does not close relational gate G2.

`NativeSnapshot` remains the complete materialized all-engine snapshot used
for retained historical reads and cross-engine semantics. The separate
`prepare_sql_latest` path decodes only the current catalog and embeds the
relation/index definitions in the catalog-version-bound plan.
`execute_prepared_latest` captures one immutable root set, rejects a stale
catalog version, traverses the buffered relational B+tree directly, and
materializes only rows reached by the exact primary/secondary key or bounded
primary scan/range. The secondary path scans only the length-delimited exact
index-key prefix, follows each live entry to its primary-key row in the same
root, and returns rows in canonical primary-key order. The scan path uses an
inclusive/exclusive bound-aware physical visitor, prunes separator-disjoint
subtrees, skips row tombstones, and stops after `LIMIT` matching rows. It does
not construct `MaterializedState`.

The public relational scan returns one owned bounded page and exposes its last
primary key as the caller's next exclusive cursor.
`scan_latest_relational_range` additionally accepts independent canonical
primary-key `Included`, `Excluded`, or `Unbounded` bounds. A stateful zero-copy
cursor, secondary-key ranges, offset handling, request arenas, and allocation
evidence remain separate work.

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
through `HYSQL013` for syntax, parameters, stale plans, columns, types,
nullability, primary-key binding/mutation, stored tuples, catalog-kind
mismatch, absent implemented access paths, and unique-index violations.
Statement byte spans, optional object/column IDs, retry classification, and
compatibility SQLSTATE mapping remain target requirements.

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
