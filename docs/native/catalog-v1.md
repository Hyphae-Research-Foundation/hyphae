# Native catalog v1

Status: normative target contract; immutable object definitions and the first
runtime catalog root are implemented experimentally; complete definition
codec, DDL evolution, and dependency tracking remain pending

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

## Required definitions

### Relation

Column IDs/types/nullability/defaults, primary and unique keys, check and
foreign-key constraints, storage options, partition key, and index IDs.

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
