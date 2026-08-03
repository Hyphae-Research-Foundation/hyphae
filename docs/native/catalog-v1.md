# Native catalog v1

Status: normative target contract; immutable relation/secondary-index/
structure/search definitions, catalogued vector metric/HNSW configuration,
their canonical `HYCOBJ01` codec, and full-definition `HYCAT002` runtime
persistence are implemented experimentally. Scalable `HYCAT003` B+tree
persistence, immutable definition blobs and buffered ID/name lookup are also
implemented experimentally. `HYCAT004` bounded relation-to-secondary-index
lookup is specified below and remains an implementation gate. DDL evolution,
constraints, and general dependency tracking remain pending.

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

Search definitions use a compatibility-preserving vector discriminant:

| Tag | Meaning | Tail |
|---:|---|---|
| `0` | no vector declaration | none |
| `1` | legacy exact `f32` vector | element tag and little-endian `u16` dimension |
| `2` | native ANN `f32` vector | element, dimension, metric, `M`, construction/default/maximum search breadth, and seed |

ANN metrics are cosine `1`, negative dot product `2`, and squared L2 `3`.
`M` must be 2 through 64, construction breadth must be at least `M`, and the
nonzero default search breadth cannot exceed its configured maximum. An ANN
tail without a vector declaration is invalid. The explicit tag means a
truncated ANN tail cannot be accepted as a legacy exact-vector definition.

## Implemented runtime persistence

New catalog-root payloads use `HYCAT002`:

| Field | Encoding |
|---|---|
| magic | ASCII `HYCAT002` |
| live object count | little-endian `u32` |
| each object | little-endian `u32` byte length followed by one `HYCOBJ01` definition |

New `CREATE TABLE`, `CREATE [UNIQUE] INDEX`, lexical-search collection, and
vector-index WAL mutations carry the complete definition plus a length-framed
normalized qualified-name conflict identity. Recovery revalidates the object
kind, target ID, owner, definition, and name identity before applying it.
Secondary-index creation also verifies that its relation and every stable
column ID exist in the admitted catalog snapshot.

Legacy `HYCAT001` roots and name-only create mutations remain readable. Their
known fixed binary relation or single-text-field search shape is reconstructed
explicitly; the next catalog write emits `HYCAT002`. Unknown legacy owner
shapes fail closed instead of inventing a definition.

`HYCAT002` is still stored in one 16 KiB catalog-root page. This is sufficient
to establish definition authority and recovery compatibility, but it is not a
scalable final catalog.

## Scalable B+tree persistence

New catalog writes use one native copy-on-write B+tree rooted directly in the
catalog root slot. Generic `HYBTLF01`/`HYBTIN01` pages provide traversal,
checksums, visibility and split behavior. The catalog defines these ordered
entries:

| Key | Value |
|---|---|
| `00` | ASCII `HYCAT003` |
| `01 \|\| object_id_be` | one `HYCVAL01` definition envelope |
| `02 \|\| qualified_lookup` | the same 16-byte big-endian `ObjectId` |

`object_id_be` is the complete nonzero 128-bit stable ID. `qualified_lookup`
contains database, schema and object lookup bytes in that order. Each
component is framed by a little-endian `u32` byte length and retains the name
limits from `HYCOBJ01`. Including the namespace byte, its maximum canonical
key size is 3,085 bytes and therefore remains below the native B+tree's
4,096-byte key limit.

`HYCVAL01` is:

| Field | Encoding |
|---|---|
| magic | ASCII `HYCVAL01` |
| storage | `0` inline, `1` immutable blob |
| reserved | seven zero bytes |
| payload | complete `HYCOBJ01` bytes or one canonical blob reference |

An inline definition is at most 8,192 bytes. A blob reference is canonical
only when its logical length is greater than 8,192 bytes. The referenced blob
must exist, match its encoded digest and length, and decode as exactly one
canonical `HYCOBJ01` definition.

The format marker is mandatory and sorts first. Every live object has exactly
one ID entry and one normalized-name entry. The ID inside the decoded
definition must equal the ID key. Its normalized qualified name must reproduce
the name key, and the name entry must point back to the same ID. Duplicate,
missing, extra, unknown-prefix, wrong-length, noncanonical-envelope, dangling
blob and cross-linked entries fail closed. Secondary-index relation and column
references are revalidated after reconstruction.

One catalog mutation publishes its object and name entries under the same
catalog root, WAL commit and global CSN. Definition blobs are staged and
synchronized before page publication under strict or group durability. A
failed or interrupted mutation cannot expose only one namespace entry.

Legacy `HYCAT001` and `HYCAT002` catalog-root pages remain readable. The next
catalog mutation materializes their validated live definitions into
`HYCAT003`; the legacy root remains immutable for retained snapshots. Once a
root is `HYCAT003`, later create operations copy only affected B+tree paths.
Page-generation vacuum rebuilds the complete reachable catalog tree without
rewriting immutable definition blobs.

`HYCAT003` removes the single-page object-count limit. It does not add drop
history, dependency edges, background blob reclamation or schema evolution.

## Bounded relational-index lookup

`HYCAT004` preserves the `HYCAT003` object, name, and value encodings and adds
one derived dependency namespace:

| Key | Value |
|---|---|
| `00` | ASCII `HYCAT004` |
| `01 \|\| object_id_be` | one `HYCVAL01` definition envelope |
| `02 \|\| qualified_lookup` | the same 16-byte big-endian `ObjectId` |
| `03 \|\| relation_id_be \|\| secondary_index_id_be` | empty |

Every live `SecondaryIndex` definition has exactly one dependency entry. Its
relation and index IDs must match the decoded definition, the relation must
resolve to a live `Relation`, and no other object kind may have a dependency
entry. Missing, duplicate, extra, nonempty, wrong-length, dangling, or
cross-linked dependency entries fail closed.

The relation prefix `03 || relation_id_be` permits one bounded B+tree range
read for only that relation's secondary-index IDs. A delta SQL transaction
resolves its relation through the name namespace, loads that relation through
the object namespace, ranges only its dependency prefix, and point-loads the
referenced index definitions. Beginning a delta transaction does not scan or
decode unrelated catalog definitions.

`HYCAT001`, `HYCAT002`, and `HYCAT003` roots remain readable. A legacy root may
fall back to validated full reconstruction when relation dependencies are
required. The next catalog mutation writes a new immutable `HYCAT004` root;
retained snapshots keep their original root and semantics. New directories
write `HYCAT004` from their first catalog publication. The migration never
rewrites an existing root in place.

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
round trips for all four implemented object kinds; legacy exact-vector byte
compatibility; ANN metric/HNSW bounds; every truncated prefix; owner, name,
ID/order, PK-nullability, secondary-index relation/column/uniqueness policy,
type, length, and trailing-byte failures; `HYCAT001` reconstruction and
`HYCAT002` rewrite; full-definition WAL/root persistence through reopen; and
the existing all-engine crash and recovery matrices. Non-reuse,
drops/evolution, dependency enforcement, and prepared-plan invalidation beyond
catalog-version mismatch remain target requirements.

Current `HYCAT003` coverage additionally proves golden marker, key and envelope
bytes; an object count that exceeds one page; multilevel buffered ID and name
lookup; large-definition blob recovery; V1/V2 migration; retained prior-root
snapshots; cross-linked and noncanonical entry rejection; bounded
copy-on-write page amplification; every normal commit interruption boundary;
page-generation vacuum; reopen equivalence; and source-bound Windows/WSL2
latency receipts. Missing-entry permutations, mutation testing, cold-cache,
concurrency, saturation and p99.9 evidence remain open.

`HYCAT004` exit evidence must add canonical dependency-key bytes, full
dependency-set reconstruction, malformed/missing/extra/cross-linked rejection,
V3 fallback plus V4 migration, retained V3 snapshot readability, buffered
relation-prefix lookup, and a delta hot-path counter proving that unrelated
catalog definitions are not decoded.
