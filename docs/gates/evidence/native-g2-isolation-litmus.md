# G2 SQL isolation litmus

Status: bounded implementation evidence; not promoted into the G2 closure at
`a839037`, which claims no universal SQL or official benchmark result.

`crates/hyphae-native-runtime/tests/sql_isolation_g2.rs` exercises detached
optimistic SQL batches against the native MVCC and conflict table. It proves:

- repeatable reads from an immutable transaction snapshot;
- private writes remain visible within the transaction;
- overlapping row writes use first-committer-wins;
- the rejected transaction cannot replace the committed value;
- disjoint stale write sets rebase and both commit; and
- committed state survives reopen.

This is not yet the complete G2 isolation evidence. The matrix must additionally
cover dirty reads, non-repeatable reads, phantoms, lost update, write skew,
read-only anomalies, catalog conflicts, unique-index conflicts, rollback and
crash boundaries, and document the admitted isolation level and anomaly policy.
Hosted exact-SHA execution and a semantic receipt remain required.
