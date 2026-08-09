# Native catalog API v1

Status: logical G6 model, canonical codec, `HYCAT006` persistence, and bounded
snapshot runtime reads implemented

This API exposes bounded product views over the native catalog. It extends the
storage and codec authority in [`catalog-v1.md`](catalog-v1.md) without making
CLI, HTTP, or SDK schemas independent catalog authorities.

## Operations

- `catalog_version()` returns the immutable version bound to the caller's
  snapshot.
- `list_objects(parent, kind, start_after, limit)` returns stable-ID ordered
  summaries with an exclusive cursor.
- `describe_object(id)` returns one complete versioned definition.
- `resolve_name(qualified_name)` returns the stable ID and complete identity.
- `list_dependencies(id, direction, start_after, limit)` returns explicit
  dependency edges.
- `capabilities()` returns admitted product, protocol, proof, and format
  versions plus hard maxima.

All operations are snapshot-bound, byte/count bounded, and deterministic.

## Product hierarchy

The public hierarchy includes database, schema, relation, secondary index,
keyspace, structure schema, search collection, analyzer, lexical index, named
vector index, cross-engine link, and admitted views. Individual runtime keys
or collection members are data, not catalog objects.

The catalog crate now exposes `CatalogObjectKind`, explicit database/schema
parents, nonzero `DefinitionVersion`, SHA-256 `DefinitionDigest`, and canonical
dependency edges. `derive_logical_dependency_edges` validates hierarchy and
referential closure; `dependency_edges_for` provides outgoing dependencies and
incoming dependents over the same canonical edge set. Bounded snapshot-backed
`list_objects`, `describe_object`, `resolve_name`, and `list_dependencies`
traverse immutable `HYCAT006` namespaces with item, visit, and byte bounds.

A keyspace definition fixes key/value types, family policy, ownership,
logical-time/TTL policy, memory class, eviction policy, and optional
relation-valued schema. Structure operations resolve a keyspace once and use
its stable ID for errors, telemetry, explain, and proofs.

A search collection owns field mappings, analyzer identities, stored/source
policy, persistent doc values, lexical options, and one or more named vector
definitions. Each named vector fixes dimension, metric, exact/ANN policy,
incremental lifecycle configuration, and optional compression version.

The implemented logical V2 model covers stored, doc-values, source-retention,
and lexical frequency/position policy; reusable tokenizer/filter analyzers;
multiple stable-ID named vectors; exact, ANN, and adaptive execution policy;
and bounded delta, consolidation, and generation-retention settings. Runtime
ANN metadata selects these settings and exposes a maintenance-due plan.
Optional compression versions remain future work.

`CatalogObject::encode_definition` and `decode_definition` remain the runtime
compatible `HYCOBJ01` APIs. Existing objects opt into the logical V2 envelope
with `encode_definition_v2(parent, definition_version)`; V2-native definitions
use `LogicalCatalogObject::encode_definition_v2` and
`decode_definition_v2`. The latter is strict and rejects noncanonical policy,
ordering, discriminants, framing, wrapped-V1 metadata mismatches, and trailing
bytes. Digests cover the complete canonical `HYCOBJ02` bytes.

Every live legacy relation, secondary index, structure, search collection, and
cross-engine link is visible through list, describe, resolve, and dependencies.
The deterministic wrapper preserves exact `HYCOBJ01` bytes, uses definition
version one, and exposes no parent because legacy definitions carry no parent
identity. Normal SQL DDL remains visible after catalog migration and reopen,
including full-width 128-bit object identities.

## Evolution

Renames retain stable IDs. Dropped names may be reused only with new IDs.
Prepared handles bind catalog version and dependencies. An incompatible change
invalidates a handle with the common stale-plan error. Analyzer, vector metric,
or physical index changes create and validate a shadow generation before an
atomic catalog/root-set switch.

## Verification

Tests prove normalized names, stable IDs, bounded enumeration, exclusive
cursors, dependency integrity, concurrent snapshot stability, prepared-plan
invalidation, and identical catalog views across every G6 surface.

Executable coverage includes V1 byte preservation, V2 golden bytes, all-kind
round trips, stable digests, corruption rejection, incoming/outgoing dependency
derivation, full-width legacy promotion, bounded `HYCAT006` reads, migration,
reopen, and normal SQL DDL visibility.
