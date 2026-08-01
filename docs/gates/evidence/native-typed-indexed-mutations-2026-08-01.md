# Native typed indexed-mutation evidence

Date: 2026-08-01

Status: exact-primary-key typed `UPDATE`/`DELETE` with atomic secondary-index
maintenance; G0, G1, and G2 remain open

Source commit:
`b64079007e161a5bb25c2b800f10032df91a8f8f`

Source tree:
`ffdab6f9397f45b1b2f2c008717423e8520b2459`

Branch: `main`

## Change

Hyphae now accepts typed:

```text
UPDATE table
SET non_primary_column = ? [, ...]
WHERE complete_primary_key = ?

DELETE FROM table
WHERE complete_primary_key = ?
```

Composite primary-key predicates may appear in a different text order than
their catalog order. Update parameters are assignments first, then predicates.
The binder rejects duplicate assignments, incomplete/duplicate predicates,
type mismatches, nullability violations, and primary-key assignment
(`HYSQL013`) before mutation. A missing key returns zero affected rows.

The historical binary `(primary_key, row)` spelling uses the same parser and
retains its raw row representation.

## One projection authority

No index-entry WAL opcode was added. Relational `UPDATE ROW=6` still carries
the new canonical row; `DELETE ROW=7` still carries an empty value. The
catalog definition and canonical row mutation remain the only authority.

For each statement, the private materialized state is cloned. Hyphae:

1. derives every old index projection;
2. removes those entries in the clone;
3. changes or deletes the row;
4. derives and validates every new projection for update; and
5. admits the clone only if the complete operation succeeds.

A unique collision therefore cannot leave a partially changed row or index.
During optimistic commit, the same mutation is reapplied to the currently
admitted state. Two disjoint primary keys can prepare the same new unique key,
but only the first admitted update publishes.

## Physical MVCC publication

Page construction reads the prior visible row from the captured relational
root. It supports both inline `HYRELBT1` rows and `HYRELBT2` version chains,
including blob-backed values.

The new copy-on-write root contains:

- one new row version or row tombstone;
- a tombstone marker for every old secondary projection; and
- for update, a live marker for every new projection.

All share one commit CSN and root publication. A retained historical root
continues to expose its old row and live entries. Same-transaction row
rewrites retain the existing one-version coalescing behavior.

## Verification

The source commit contains 63 `hyphae-native-runtime` tests. New coverage
proves:

- multi-column typed update over one unique and one non-unique index;
- old exact keys disappear and new keys reach the updated row;
- the unchanged non-unique fanout remains correct;
- retained materialized snapshots preserve old indexed results;
- unique collision, PK assignment, nullability, duplicate-column, and type
  failures leave the statement state unchanged;
- missing update returns zero affected rows;
- delete removes the row and all secondary projections;
- composite primary-key predicate text order for update and delete;
- two nullable unique keys update to SQL null under `NULLS DISTINCT`;
- optimistic disjoint-row updates recheck uniqueness at admission;
- strict reopen validates updated/tombstoned projections;
- legacy inline `HYRELBT1` indexed update and reopen; and
- all seven deterministic commit interruption boundaries.

The indexed update/delete crash matrix demonstrates:

| Interruption | Recovered state |
|---|---|
| `BlobStaged` | complete prior row/index state |
| `BlobPromoted` | complete prior row/index state |
| `PageAppended` | complete prior row/index state |
| `PageSynchronized` | complete prior row/index state |
| `WalAppended` | complete new row/index state |
| `WalSynchronized` | complete new row/index state |
| `RootPublished` | complete new row/index state |

No boundary recovers a new row with old live projections, an old row with new
projections, or a partial delete.

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

Mutation testing was not executed because the repository declares no mutation
tool or acceptance threshold. Ordinary tests are not represented as a
substitute.

## Remaining boundary

This is not complete SQL DML. Still required:

- primary-key-changing updates and dependency/cascade semantics;
- secondary/range access for multi-row mutation;
- expressions, literals, casts, defaults, generated columns and `RETURNING`;
- checks, foreign keys, exclusion constraints, triggers and referential
  actions;
- statement/savepoint failure-state policy beyond this bounded slice;
- row-count estimates, general plans, cancellation and resource budgets;
- randomized row/index model equivalence, SQLLogicTest, isolation histories,
  TPC-H/TPC-C and mutation testing; and
- write-amplification, WAL/page/blob, fsync, concurrency and saturation
  measurements.

No G0, G1, or G2 gate closes from this milestone alone.
