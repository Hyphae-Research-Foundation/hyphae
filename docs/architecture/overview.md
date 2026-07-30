# Architecture overview

Hyphae separates durable semantics from delivery surfaces.

```text
hyphae-cli (single binary)
  ├─ embedded facade
  ├─ HTTP server /v1
  └─ MCP adapter
          │
public contracts + client
          │
engine: atomic operations, query, retrieval, proofs
          │
append-only log ── snapshots ── rebuildable redb indexes
```

For the durable KV path, a write is acknowledged in two ordered stages:

1. canonical mutation frames and their commit frame are appended and synced;
2. the mutations and commit checkpoint are applied atomically to redb.

The log is authoritative. If stage 2 fails, the commit receipt remains valid,
the live handle refuses potentially stale reads, and reopen verifies the log
before replaying every missing commit. A redb checkpoint is accepted only when
its sequence and digest identify the same commit in the verified log.

Immutable generation manifests select the active segment and its optional
snapshot anchor. A new generation is committed by creating a new file rather
than replacing live state, so an interrupted migration or compaction has one
unambiguous winner. See
[`manifest-format-v1.md`](../storage/manifest-format-v1.md).

Logical snapshots stream sorted authoritative state instead of copying redb
internals. Format 1 contains KV records and receipts. Format 2 additionally
contains vector-space definitions, vectors, and lexical-index definitions;
every collection is canonically ordered and participates in the witness
digest. Each snapshot records the exact verified log checkpoint and has
independent CRC32C and BLAKE3 validation. The normative layout is documented in
[`snapshot-format-v1.md`](../storage/snapshot-format-v1.md).

Compaction commits snapshot plus next-segment selection through a new
immutable manifest, and only then retires the old segment. The interruption
matrix is defined in [`compaction-v1.md`](../storage/compaction-v1.md).

The packaged CLI/server and explicit bounded embedded startup carry one finite
`RecoveryLimits` deadline through directory validation, snapshot restore, log
scan, index replay, and lexical rebuild. The live writer retains the same
aggregate log and lexical ceilings and rejects work before durable append if
it would make future recovery under that policy exceed them. Snapshot and
compaction reuse a separate shared `MaintenanceLimits` policy; their explicit
APIs allow embedded hosts to select lower or higher finite limits. Published
embedded `open` methods retain their `0.2.0` compatibility policy.

## Layer rules

- `hyphae-core` owns stable domain values and invariants, not I/O.
- `hyphae-engine` is the embeddable facade that composes storage, structured
  documents, query, and provider-neutral retrieval.
- `hyphae-storage` owns disk format, recovery, snapshots, and indexes.
- `hyphae-query` owns a deterministic typed AST and reference semantics.
- `hyphae-retrieval` owns exact vector scoring and provider-neutral
  abstention; it has no default provider.
- `hyphae-contracts` exposes wire models tied to canonical contracts.
- `hyphae-server` and `hyphae-client` communicate only through `/v1` models.
- `hyphae-cli` is the only executable artifact and composes libraries.

`hyphae-engine` was introduced only after the phase-2 durability primitives
were proven. Provenance joins the same facade in phase 4; delivery surfaces do
not bypass its public semantics.

Result proof v1 references a canonical logical snapshot as its complete
offline witness. The verifier checks the pinned snapshot/log anchor and
reexecutes the operation from that witness, so arbitrary query completeness
does not depend on an unauthenticated materialized query index. See
[`result-proof-v1.md`](../provenance/result-proof-v1.md).

## Data directory

```text
data/
├─ FORMAT
├─ LOCK
├─ manifest/
├─ log/
├─ snapshots/
├─ indexes/
├─ blobs/
└─ tmp/
```

Restore targets a new empty directory. Temporary output is verified and
atomically promoted; an existing live directory is never overwritten in
place.
