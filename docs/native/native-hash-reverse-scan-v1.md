# Native reverse hash scan v1

Status: accepted implementation target.

This contract extends
[Native structure-engine semantics v1](structures-semantics-v1.md) and
[Native whole-hash TTL v1](native-hash-ttl-v1.md). It adds descending bounded
field iteration through Hyphae's native reverse B+tree visitor without
materializing or reversing an ascending result.

## Surface

The embedded native surface adds:

```text
HSCAN_REVERSE(key, start_before?, limit) -> [(field, value)]
```

The Rust methods are named `hscan_reverse` on private batches and retained
snapshots, and `hscan_reverse_latest_hash` /
`hscan_reverse_latest_hash_at` on the current-root physical surface.

The operation requires one existing visible native hash. Another live
structure family fails with `StructureKindMismatch`. A missing or logically
expired hash fails with `UnknownStructureHash`.

## Ordering and cursor

Results are ordered by descending exact field bytes. Values do not influence
order.

`start_before` is an optional exclusive exact-field cursor:

- `None` starts at the greatest live field;
- `Some(field)` visits only fields whose exact bytes are less than `field`;
- `Some(empty)` returns no entries after validating the hash; and
- cursor bytes do not need to identify a current live field.

The caller resumes by passing the last returned field as `start_before`.
Fields inserted above that cursor after a retained snapshot was captured are
not visible to the retained snapshot and cannot appear in its continuation.

`limit` is an output bound. Zero validates the hash kind, metadata, visibility,
and cursor identity before returning an empty vector. The call never returns
more than `limit` live entries.

## Execution

Private-batch and retained-snapshot execution iterate the materialized
`BTreeMap` in reverse exact-byte order over the exclusive upper bound.

The current-root route captures one root set, validates live hash metadata and
logical time once, maps `start_before` to an exclusive physical upper bound,
and calls the native reverse cached prefix-range visitor. It skips canonical
field tombstones without charging `limit` and stops when the requested number
of live entries has been emitted.

The physical route must not:

- run an ascending scan and reverse its output;
- materialize the complete hash;
- visit fields greater than or equal to `start_before`; or
- continue into lower pages after satisfying `limit`.

When `start_before` is absent and `limit` covers declared cardinality, the
route verifies that the complete reverse traversal observes exactly the
metadata live count. Reached malformed metadata, identity, field envelope,
expiry, blob, page order, or cycle fails the complete call rather than
returning a partial vector.

## Durability and concurrency

This is a read-only operation. It adds no mutation, WAL opcode, conflict key,
page format, or catalog object. Retained snapshots use their pinned root and
current-root calls use one captured root, so every result belongs to one
committed CSN.

Whole-hash expiry is evaluated before traversal. Physical cleanup and
compaction may remove due or tombstoned fields from later roots without
changing retained-snapshot results.

## Required evidence

Implementation evidence must include:

- a compiler-reaching red gate before model and public methods exist;
- exact descending order over empty, binary, prefix-related, and tombstoned
  fields;
- absent, live, and non-live cursors, including `Some(empty)`;
- zero, one, exact-cardinality, and over-cardinality limits;
- private, retained-snapshot, current-root, explicit-time, and reopened
  equivalence;
- whole-hash TTL visibility before, at, and after expiry;
- height-two physical pruning and early stop;
- fail-closed reached metadata, identity, value, and blob corruption;
- a direct-Linux release comparison against a full ascending materialize-and-
  reverse fallback; and
- formatting, workspace tests, warnings-denied Clippy, documentation, and
  hosted checks.

## Boundaries

This contract does not add glob matching, opaque cursors, field TTL, relative
or sliding expiry, protocol exposure, randomized model equivalence, or a
complete G3/G7 claim.
