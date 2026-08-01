# Native secondary-index evidence

Date: 2026-08-01

Status: first persistent exact-key SQL secondary-index vertical; G0, G1, G2,
and G7 remain open

Source commit:
`e7a06ea3122dcf20fa3ac39838db4ce7b26e107a`

Source tree:
`65beb2b8915f70405c50289af601f1a19fe0966a`

Branch: `main`

## Change

Hyphae now owns the first relational secondary-index path without embedding or
calling PostgreSQL, SQLite, Redb, RocksDB, DataFusion, or another query engine.
The source commit adds:

- canonical `SecondaryIndex` catalog objects containing stable relation and
  ordered column IDs, uniqueness, and null-distinctness;
- public catalog and runtime validation of relation and column references;
- `CREATE INDEX` and `CREATE UNIQUE INDEX`;
- exact-key `WHERE` binding to single or composite secondary indexes,
  independent of predicate text order;
- catalog-version-bound prepared secondary-index plans;
- deterministic bounded `EXPLAIN SELECT`;
- stable `HYSQL011` no-access-path and `HYSQL012` unique-violation errors;
- atomic backfill over rows already admitted in the transaction;
- maintenance of every admitted secondary projection on row insertion; and
- strict recovery validation of index metadata, entries, source rows, and
  uniqueness.

The historical binary relation shape cannot receive a typed secondary index.
Its raw point-read/update/delete compatibility path remains unchanged.

## Catalog and physical format

`HYCOBJ01` object-kind tag `4` is the relational secondary-index definition.
The checked-in 144-byte golden covers the object header, relation ID, ordered
column IDs, `unique`, and `nulls_distinct`. Decoder coverage rejects every
truncated prefix of all four implemented object kinds.

The relational B+tree adds two namespaces:

| Prefix | Key | Value |
|---:|---|---|
| `0x03` | index `ObjectId` | 32-byte `HYRIDX01` metadata |
| `0x04` | index `ObjectId` + entry identity | live/tombstone byte |

`HYRIDX01` binds the relation ID and the two canonical policy booleans.
An entry identity is a big-endian `u32` secondary-key length, canonical
memcomparable secondary-key bytes, and canonical primary-key bytes. The
complete physical key remains subject to the common 4,096-byte B+tree bound.

Recovery requires the physical metadata to equal the catalog definition,
recomputes every live entry from the catalog-typed `HYTUPL01` row, rejects
orphan or malformed entries, enforces non-null uniqueness, and proves that
every row has every expected projection. A forged tombstone that removes one
required projection fails complete-state validation.

## WAL and transaction authority

Relational WAL opcode `13` creates a secondary index and carries the complete
`HYCOBJ01` definition plus its normalized qualified-name conflict identity.
There are deliberately no independent insert/delete-index-entry WAL opcodes.

The admitted catalog definition and canonical row mutation are the only
projection authority:

- index creation backfills the final admitted row state;
- row insertion derives projections against the admitted catalog;
- optimistic rebase repeats those operations over the current root set; and
- physical page construction derives the same entries from the same
  definition and row bytes.

This prevents a separately serialized index operation stream from diverging
from its source rows.

The executable concurrency fixtures prepare index creation and row insertion
from the same pre-index snapshot and commit them in both orders. Both routes
recover the row through the index. A separate fixture prepares two disjoint
primary keys with the same unique secondary key; the first commit wins and the
second fails during admitted-root rebase with
`UniqueSecondaryIndexViolation`.

## SQL semantics

The current binder selects an access path only when predicates cover exactly
one complete primary or secondary key. Partial index keys, ranges, residual
filters, scans, and cost-based choices are not implemented.

Unique indexes use `NULLS DISTINCT`: any null component does not collide for
uniqueness. SQL equality remains three-valued. A bound `column = ?` with a
null parameter returns no rows rather than looking up the internal null index
component. Non-null parameters are still type-checked even when another
composite component is null.

During construction, the exact null-equality regression test was run against
the pre-fix dirty state and failed with two rows where an empty result was
required. The same exact test passed after the fix. The red observation was
not retained as a source commit and is not used as release evidence.

## Verification

The source commit contains:

- 9 `hyphae-native-catalog` tests;
- 54 `hyphae-native-runtime` tests;
- a stable secondary-index catalog golden;
- exhaustive object-definition truncation rejection;
- backfill before and insertion after index creation;
- composite binding in reverse textual order;
- SQL null-equality and non-null type validation;
- unique creation failure without partial catalog/index state;
- unique collision during optimistic rebase;
- both index/row optimistic commit orders;
- strict commit, reopen, and prepared-result equivalence; and
- missing physical projection rejection.

Windows, Rust/Cargo 1.96.0, passed:

```text
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tools/check_documentation.py
git diff --check
```

Debian GNU/Linux 13 under WSL2 passed:

```text
cargo test --workspace --all-features --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Mutation testing was not executed because the repository has no declared
mutation-testing tool or threshold. Passing coverage and ordinary tests are
not represented as a substitute.

No secondary-index latency receipt was produced. The existing microsecond
observations do not prove this path.

## Remaining boundary

This milestone is not a complete SQL index subsystem. Still required:

- direct pinned physical secondary-index execution and a matched benchmark;
- equality/range/prefix cursors, ordered scans, residual filters, bitmap
  combination, cancellation, budgets, and spill;
- indexed typed `UPDATE` and `DELETE`, including old/new projection
  maintenance and physical tombstone retention;
- included columns, expressions, predicates, collations/operator classes, and
  additional null policies;
- statistics, selectivity/cardinality estimation, cost-based access choice,
  logical/physical plan trees, and complete `EXPLAIN`;
- index lifecycle, drop/rebuild, online validation, schema dependencies, and
  definition history;
- scalable catalog storage beyond one 16 KiB root page;
- randomized model equivalence, property-generated ordering, mutation
  testing, isolation histories, SQLLogicTest, TPC-H, and TPC-C evidence; and
- concurrency, saturation, cold-state, background-interference, allocation,
  hardware-counter, and dedicated-hardware measurements.

No G0, G1, G2, or G7 gate closes from this evidence alone.
