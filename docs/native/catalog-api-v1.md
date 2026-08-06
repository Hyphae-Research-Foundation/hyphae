# Native catalog API v1

Status: accepted G6 planning contract; implementation incomplete

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

A keyspace definition fixes key/value types, family policy, ownership,
logical-time/TTL policy, memory class, eviction policy, and optional
relation-valued schema. Structure operations resolve a keyspace once and use
its stable ID for errors, telemetry, explain, and proofs.

A search collection owns field mappings, analyzer identities, stored/source
policy, persistent doc values, lexical options, and one or more named vector
definitions. Each named vector fixes dimension, metric, exact/ANN policy,
incremental lifecycle configuration, and optional compression version.

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
