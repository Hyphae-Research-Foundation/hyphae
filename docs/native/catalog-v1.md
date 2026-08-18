<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native catalog v1

Status: normative contract. The runtime implements `HYCAT007`, including
the scalable B+tree catalog, immutable definition blobs, buffered ID/name
lookup, bounded relation-to-secondary-index edges, and durable monotonic object
ID authority, logical V2 persistence, deterministic legacy wrappers, and
bounded object/dependency reads. Legacy definitions remain canonical
`HYCOBJ01`; V2-native definitions use strict `HYCOBJ02`. DDL evolution beyond
the current create/drop/rename behavior remains pending.

`HYCAT007` is additive over `HYCAT006`. Prefix `0x06 || ancestor_id_be ||
descendant_id_be` has an empty value and contains the reflexive transitive
closure of the logical parent relation. Every live object has its self edge;
every V2 descendant has one edge for each ancestor. Complete-load validation
re-derives the set and rejects missing, extra, duplicate, dangling, cyclic,
nonempty, or malformed entries. A strict explicit migration writes a new
immutable root; open never mutates a directory. New catalog writes emit V7.
The index supports subtree visibility without traversing unrelated global
objects and remains ordered by ancestor then descendant `ObjectId`.

The catalog is the shared namespace and type authority. It does not force the
three engines to share one physical data model.

The product provisions its wire-compatible default scalar namespace as one
internal Database, one internal Schema, and one V2 Keyspace in the same strict
native transaction as the internal `HYPDKB01` binding. IDs come from the
catalog's durable `next_object_id` authority and are neither constants nor
derived from names. The keyspace is exactly String/Binary/Binary,
Canonical/PerValue/Durable/None with no relation schema. Open validates the
binding lineage plus all three IDs, parents, owners, kinds, names, definition
versions, types, ownership, TTL, memory, and eviction policy. It never adopts
objects by name. Binding and definitions therefore become visible together or
not at all without changing `HYCAT006`, WAL, or directory format versions.

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

`HYCOBJ01` encodes exactly one complete runtime object:

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

The codec covers `Relation`, relational `SecondaryIndex`, `Structure`, `Search`,
and `CrossEngineLink` objects. Relation CHECK/foreign-key tails and link
definitions are compatibility-preserving additions. Database, schema,
first-class keyspace, analyzer, and richer search definitions are represented
by logical V2 rather than changing these bytes.

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

## Implemented logical V2 codec

`HYCOBJ02` is additive. Existing `CatalogObject` constructors, variants, and
`encode_definition`/`decode_definition` APIs retain exact `HYCOBJ01` behavior
for `HYCAT005` runtime compatibility. Existing objects may be losslessly wrapped
with a parent and nonzero logical definition version. V2-native objects encode
database, schema, keyspace, analyzer, and richer search-collection definitions.

One canonical `HYCOBJ02` definition contains:

1. ASCII magic `HYCOBJ02`;
2. one stable `CatalogObjectKind` and representation tag;
3. stable ID, owner, qualified display/lookup name, optional stable parent, and
   nonzero little-endian `u64` definition version; and
4. either length-framed exact canonical `HYCOBJ01` bytes or the kind-specific
   V2 definition.

The strict decoder rejects unknown tags, zero identities/versions, invalid
owners or parents, malformed names/types/booleans/options, noncanonical field
or vector ordering, duplicate IDs/names/policies, contradictory keyspace/field/
vector policy, mismatched wrapped `HYCOBJ01` metadata, truncation, excessive
lengths, and trailing bytes. It re-encodes and compares the complete definition
before admission. The limits remain 1,024 bytes per name, 100,000 list items,
and 16 MiB per definition.

Logical V2 includes:

- explicit database and schema objects with stable hierarchy parents;
- first-class keyspaces with structure family, key/value types, ownership,
  disabled/per-value/default TTL, memory class, eviction, and optional relation
  schema dependency;
- reusable analyzers with tokenizer and ordered, nonduplicated filter pipeline;
- fields with independent stored, doc-values, source-retention, and lexical
  frequency/position policy; and
- multiple stable-ID named vectors with fixed type/metric, exact, ANN, or
  adaptive threshold policy, and incremental delta/consolidation/generation
  retention settings.

Each complete canonical `HYCOBJ02` definition has a stable SHA-256 digest.
Dependency derivation emits canonical directed edges from dependent to
prerequisite for hierarchy parents, secondary indexes, foreign keys, analyzers,
link endpoints, and relation-valued keyspaces. Helpers provide both outgoing
dependencies and incoming dependents. Logical-set derivation checks target
existence and database/schema parent kinds. `HYCAT006` persists the general
edge set in both directions.

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

## Scalable B+tree persistence (`HYCAT003`)

`HYCAT003` introduced one native copy-on-write B+tree rooted directly in the
catalog root slot. Generic `HYBTLF01`/`HYBTIN01` pages provide traversal,
checksums, visibility and split behavior. It defines these ordered entries:

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

## Bounded relational-index lookup (`HYCAT004`)

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
required. `HYCAT004` migration writes a new immutable root and never rewrites an
existing root in place. Current new writes advance to `HYCAT005` as described
below; retained snapshots keep their original root and semantics.

## Current object-ID authority

`HYCAT005` preserves all `HYCAT004` keys and values and adds one mandatory
authority entry:

| Key | Value |
|---|---|
| `00 01` | next never-issued object ID as a 16-byte big-endian integer; zero means exhausted |

New runtime catalog trees write `HYCAT005`. `HYCAT003` and `HYCAT004` trees
remain readable and rebuild to `HYCAT005` on the next catalog mutation. During
reconstruction, the authority must be no lower than every live ID plus one;
missing, malformed, regressed, or inconsistently exhausted authority fails
closed. Dropping an object does not lower this value, so a retired object ID is
not silently reused. `HYCAT005` otherwise stores `HYCOBJ01` inside `HYCVAL01`;
the current `HYCAT006` format below preserves those bytes while adding logical
V2 definitions and dependency namespaces.

## Current logical catalog (`HYCAT006`)

`HYCAT006` preserves every `HYCAT005` key and adds outgoing `0x04` and incoming
`0x05` dependency namespaces. Every legacy `HYCOBJ01` relation, secondary
index, structure, search collection, and cross-engine link has a deterministic
lossless logical V2 view for bounded list, describe, resolve, and dependency
reads. The view uses definition version one and no parent; it does not allocate
synthetic namespace IDs or narrow full-width object identities.

The next catalog mutation rebuilds `HYCAT003`, `HYCAT004`, or `HYCAT005` into
`HYCAT006`. SQL-created relations and indexes remain in the object and name
namespaces and therefore remain visible after migration and reopen. Derived
edges are persisted for legacy and V2 definitions and validated against the
decoded definitions on complete load.

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
round trips for all five `HYCOBJ01` object kinds; legacy exact-vector byte
compatibility; ANN metric/HNSW bounds; every truncated prefix; owner, name,
ID/order, PK-nullability, secondary-index relation/column/uniqueness policy,
type, length, and trailing-byte failures; `HYCAT001` reconstruction and
`HYCAT002` rewrite; full-definition WAL/root persistence through reopen; and
the existing all-engine crash and recovery matrices. Logical V2 coverage adds
database and compatible-object golden bytes, V2-native round trips, SHA-256
digest stability, every truncated prefix, corrupt/trailing/wrapped-V1 mismatch,
duplicate analyzer/vector policy, contradictory field/keyspace/vector policy,
and incoming/outgoing dependency derivation. Drops/evolution, general runtime
dependency enforcement, and prepared-plan invalidation beyond catalog-version
mismatch remain target requirements.

Current `HYCAT003` coverage additionally proves golden marker, key and envelope
bytes; an object count that exceeds one page; multilevel buffered ID and name
lookup; large-definition blob recovery; V1/V2 migration; retained prior-root
snapshots; cross-linked and noncanonical entry rejection; bounded
copy-on-write page amplification; every normal commit interruption boundary;
page-generation vacuum; reopen equivalence; and source-bound Windows/WSL2
latency receipts. Missing-entry permutations, mutation testing, cold-cache,
concurrency, saturation and p99.9 evidence remain open.

Current `HYCAT004`/`HYCAT005` runtime coverage includes canonical relation-index
dependency keys, malformed/duplicate/extra/cross-linked rejection, legacy
fallback and migration, retained snapshot readability, buffered relation-prefix
lookup, a delta hot-path guard against unrelated definition decoding, and
monotonic object-ID authority through reopen. `HYCAT006` coverage additionally
proves deterministic legacy relation/index/structure/search/link views, the
general persisted dependency graph, bounded list/describe/resolve/dependency
reads, full-width IDs, SQL DDL visibility, migration, interruption, and reopen.
