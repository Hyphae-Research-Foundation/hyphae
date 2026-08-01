# Native catalog v1

Status: normative target contract; immutable relation/secondary-index/
structure/search object definitions, their canonical `HYCOBJ01` codec, and
full-definition `HYCAT002` runtime persistence are implemented experimentally.
Scalable catalog pages, DDL evolution, constraints, and dependency tracking
remain pending.

The catalog is the shared namespace and type authority. It does not force the
three engines to share one physical data model.

## Object hierarchy

```text
database
└─ schema
   ├─ relation
   │  ├─ columns
   │  ├─ constraints
   │  └─ relational indexes
   ├─ keyspace
   │  └─ structure objects
   ├─ search collection
   │  ├─ fields/analyzers
   │  ├─ lexical indexes
   │  └─ vector indexes
   ├─ cross-engine link
   └─ view/materialized view
```

Every object has a nonzero stable `ObjectId`, owner engine, display name,
normalized lookup name, creation catalog version, optional drop version, and
versioned definition digest.

## Names

- Unquoted SQL identifiers fold ASCII `A-Z` to lowercase.
- Quoted identifiers preserve exact UTF-8 bytes.
- Names are unique within their parent under normalized lookup rules.
- Renaming retains the stable ID.
- Dropped names may be reused only with a new stable ID.

## Immutable snapshots

`CatalogVersion` starts at one. DDL creates a new immutable catalog snapshot;
readers retain the snapshot bound to their transaction. Catalog publication
uses the same WAL and root-set commit boundary as data.

Catalog reads use stable IDs after binding. Hot prepared execution does not
repeat name lookup.

## Implemented object-definition codec

`HYCOBJ01` encodes exactly one complete object:

1. object-kind tag;
2. nonzero `ObjectId`, owner, and fully qualified name;
3. relation columns and ordered primary-key IDs, structure kind/types/policy,
   secondary-index relation/ordered columns/uniqueness policy, or search
   fields/analyzers/doc-values/vector declaration; and
4. no trailing bytes.

Each name component stores display and normalized lookup UTF-8 separately.
Unquoted lookup bytes must equal ASCII-folded display bytes; quoted lookup
bytes must equal display bytes. A component is limited to 1,024 bytes.

Logical types use the canonical recursive descriptor from
[`types-v1.md`](types-v1.md). Relation columns and search fields are strictly
ordered by stable ID. Duplicate IDs or normalized names, nullable primary-key
columns, wrong owners, invalid types, excessive lists, malformed booleans,
truncation, and trailing bytes fail closed. One definition is limited to
16 MiB and one definition list to 100,000 items.

The codec currently covers `Relation`, relational `SecondaryIndex`,
`Structure`, and `Search` objects. Constraint, link, view, analyzer, and
dependency-edge object variants remain to be defined.

## Implemented runtime persistence

New catalog-root payloads use `HYCAT002`:

| Field | Encoding |
|---|---|
| magic | ASCII `HYCAT002` |
| live object count | little-endian `u32` |
| each object | little-endian `u32` byte length followed by one `HYCOBJ01` definition |

New `CREATE TABLE`, `CREATE [UNIQUE] INDEX`, and search-collection WAL mutations
carry the complete definition plus a length-framed normalized qualified-name
conflict identity. Recovery revalidates the object kind, target ID, owner,
definition, and name identity before applying it. Secondary-index creation
also verifies that its relation and every stable column ID exist in the
admitted catalog snapshot.

Legacy `HYCAT001` roots and name-only create mutations remain readable. Their
known fixed binary relation or single-text-field search shape is reconstructed
explicitly; the next catalog write emits `HYCAT002`. Unknown legacy owner
shapes fail closed instead of inventing a definition.

`HYCAT002` is still stored in one 16 KiB catalog-root page. This is sufficient
to establish definition authority and recovery compatibility, but it is not a
scalable final catalog. A copy-on-write catalog B+tree, separate name
namespace, definition blobs, and per-object history remain required.

## Required definitions

### Relation

Column IDs/types/nullability/defaults, primary and unique keys, check and
foreign-key constraints, storage options, partition key, and index IDs.

### Relational secondary index

The implemented definition stores a nonzero owning relation ID, one or more
ordered stable column IDs, uniqueness, and null-distinctness. Catalog and
runtime admission verify the relation and every column reference. Included
columns, expressions, predicates, access method options, collation/operator
classes, definition history, dependency edges, and schema-evolution behavior
remain target work.

### Keyspace and structure

Key and value types, structure kind, canonical/cache ownership, memory class,
TTL policy, eviction policy, partition key, and optional relational access
schema.

### Search collection

Source ownership, field mappings, analyzer IDs, stored/source fields, doc
values, lexical index options, vector definitions, and visibility policy.

### Link

Source and target object IDs, stable-ID mapping, maintenance mode, delete
behavior, and whether updates participate synchronously in the originating
transaction.

## Dependencies

Views, indexes, constraints, analyzers, links and prepared-plan dependencies
are explicit edges. DDL rejects a destructive change while live dependents
exist unless the statement explicitly and atomically replaces or drops them.

## Schema evolution

- Adding a nullable column without a volatile default is metadata-only.
- Adding constraints requires validation before enforcement is marked valid.
- Type changes require a lossless binary-compatible rule or a new physical
  version and background rewrite.
- Search analyzer or vector metric changes create a shadow generation and
  atomically switch after validation.
- No DDL rewrites a live format in place without an interruption-safe protocol.

## Verification

Tests cover normalized names, stable-ID retention, non-reuse, immutable reader
snapshots, DDL/data atomicity, dependency enforcement, crash recovery,
concurrent prepared-plan invalidation, and schema-evolution interruption.

Current executable coverage proves definition golden bytes and canonical
round trips for all four implemented object kinds; every truncated prefix;
owner, name, ID/order, PK-nullability, secondary-index relation/column/
uniqueness policy, type, length, and trailing-byte failures; `HYCAT001`
reconstruction and `HYCAT002` rewrite; full-definition WAL/root persistence
through reopen; and the existing all-engine crash and recovery matrices.
Non-reuse, drops/evolution, dependency enforcement, and prepared-plan
invalidation beyond catalog-version mismatch remain target requirements.
