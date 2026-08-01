# Native catalog-definition evidence

Date: 2026-08-01

Status: persistent typed-definition prerequisite; G0, G1, and G2 remain open

Source commit:
`d611eb8f92157ab77a51496498a66e31fe3f0b4e`

Source tree:
`ca0e94cfc97167f3c295d7418613b9b213a2382d`

Branch: `main`

## Change

Hyphae no longer validates relation/search definitions and then discards them.
The native catalog now owns:

- bounded canonical `HYCOBJ01` definitions for relation, structure, and search
  objects;
- strict qualified display/lookup names;
- stable ordered column and field IDs;
- recursive canonical logical-type descriptors;
- relation nullability and ordered primary-key IDs;
- structure kind, key/value types, ownership, and TTL policy;
- search fields, analyzer references, doc-values flags, and optional vector
  declaration; and
- runtime `HYCAT002` roots containing the complete live definitions.

Snapshots expose the immutable definition pinned by their catalog version.

## WAL and recovery

New relation/search create mutations carry:

1. the complete `HYCOBJ01` definition as the WAL value; and
2. a length-framed normalized qualified-name identity as the WAL key.

Optimistic conflict reconstruction uses that stable name identity. Mutation
replay verifies kind, owner, target `ObjectId`, definition bytes, and name
identity before changing materialized state or physical roots.

Legacy `HYCAT001` roots and name-only create mutations remain readable for the
two shapes that existed:

- a two-column binary relation (`primary_key`, `row`); and
- a one-field text search collection.

Those shapes are reconstructed explicitly. A later catalog write emits
`HYCAT002`. Unknown legacy owners fail closed.

## Canonical failure behavior

The object codec rejects:

- empty or over-1,024-byte name components;
- arbitrary display/lookup pairs;
- zero or duplicate IDs;
- wrong object owners;
- duplicate normalized column/field names;
- unsorted stable column/field IDs;
- missing, repeated, or nullable primary-key columns;
- malformed recursive type descriptors;
- invalid booleans, enums, analyzer identities, and vector dimensions;
- more than 100,000 definition items;
- definitions above 16 MiB;
- every truncated prefix; and
- trailing bytes.

A relation definition has one exact checked-in golden byte fixture.

## Verification

The source commit raises:

- `hyphae-native-catalog` from 4 to 7 tests; and
- `hyphae-native-runtime` from 41 to 42 tests.

The runtime coverage now proves `HYCAT001` reconstruction, `HYCAT002` rewrite,
complete-definition WAL/root publication, snapshot introspection, strict
commit, reopen, and recovered column/type/PK identity. Existing all-engine
transaction, optimistic rebase, checkpoint, blob, corruption, and crash
matrices remain green.

Windows passed:

```text
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Debian 13 under WSL2 passed:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Remaining boundary

The catalog root is still one 16 KiB page. This milestone does not claim a
scalable or SQL-complete catalog. Still required:

- copy-on-write catalog B+tree object and name namespaces;
- definition blobs and per-object definition history;
- index, constraint, dependency, link, analyzer, and view object kinds;
- create/drop/alter versions and definition digests;
- dependency enforcement and schema-evolution protocols;
- typed row validation/decoding and typed SQL DDL/DML; and
- physical secondary-index maintenance.

No G0, G1, or G2 gate closes from this evidence alone.
