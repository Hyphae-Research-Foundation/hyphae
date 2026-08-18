# Architecture overview

Hyphae separates durable semantics from delivery surfaces. One process owns
one data directory. Since `1.1.0` the active architecture is the Native
local ecosystem: three Hyphae-owned engines over one shared substrate. The
earlier format-2 generation remains packaged strictly as a compatibility
product and is described at the end of this document.

## Native generation (active)

```text
hyphae-cli (single binary)
  ├─ embedded product facade (hyphae-native-product)
  ├─ local daemon: UDS / named pipe (hyphae-native-daemon + protocol)
  ├─ optional loopback HTTP /v2 (hyphae-server)
  ├─ Ratatui console, CLI commands, MCP adapter
  └─ Python / TypeScript SDKs (local protocol or HTTP /v2)
          │
typed ProductOperation dispatch + durable RBAC decision
          │
native runtime: SQL, structures, lexical/vector search
          │
catalog ── copy-on-write pages ── blobs ── WAL ── MVCC roots ── scheduler
```

Every surface consumes the same product operations and the same durable
authorization decision. Engine-to-engine execution uses direct typed Rust
calls; no internal path uses TCP, HTTP, JSON, or another serialized
compatibility protocol.

### Commit ordering

A native commit acknowledges only after ordered, injectable stages:

1. large values are staged and promoted in the blob store;
2. new copy-on-write pages are appended and synchronized;
3. one complete cross-engine WAL transaction is appended;
4. the WAL is synchronized for the selected durability class; and
5. the root set and commit sequence number (CSN) become visible.

Visibility never precedes durability. Each boundary is an explicit
interruption point exercised by the crash matrices, and recovery replays or
discards exactly one unambiguous winner. A failed synchronization poisons the
writer, which refuses further work until reopen verifies the log.

### Shared substrate

SQL, structures, lexical search, and vector search share one catalog with
stable object IDs, one page/blob allocator, one WAL, one MVCC root set and
commit sequence, one scheduler and memory policy, and one backup, restore,
vacuum, checkpoint, doctor, and proof substrate. A committed all-engine
mutation has one visible CSN; readers never combine roots from different
generations.

### Security and data-directory ownership

Durable principals, roles, scoped grants, API keys, rotation, revocation,
owner recovery, and audit events commit through the same native WAL and CSN
sequence. A new native data directory is created owner-only (`0o700`) on
Unix inside the `mkdir` call itself; it holds the raw WAL, pages, blobs,
security catalog, and the default local endpoint. The daemon requires
durable API keys once the access catalog is bootstrapped. See the
[native access-control threat model](../security/native-access-control-threat-model.md).

### Native data directory

```text
data/
├─ FORMAT            hyphae-native-format marker
├─ LOCK
├─ pages*.hydb
├─ wal / wal-retention
├─ manifests / snapshot pins
├─ blobs
└─ hyphae.sock       default Unix local endpoint
```

Native and format-2 directories are distinguished by their FORMAT markers;
mixed-authority startup options fail closed. Restore targets a new empty
directory; an existing live directory is never overwritten in place.

The normative engine, protocol, and proof documents live under
[`docs/native/`](../native/), with the accepted decision record in
[ADR-0020](../adr/0020-native-local-data-ecosystem.md),
[ADR-0023](../adr/0023-native-local-product-and-competitive-scope.md), and the
[native local ecosystem architecture](native-local-ecosystem.md).

## Format-2 compatibility generation

The format-2 product remains packaged for existing `0.2` data directories and
the published `/v1` API. It is a compatibility surface, not the recommended
facade for new applications.

For the durable KV path, a write is acknowledged in two ordered stages:

1. canonical mutation frames and their commit frame are appended and synced;
2. the mutations and commit checkpoint are applied atomically to redb.

The log is authoritative. If stage 2 fails, the commit receipt remains valid,
the live handle refuses potentially stale reads, and reopen verifies the log
before replaying every missing commit. A redb checkpoint is accepted only when
its sequence and digest identify the same commit in the verified log. redb is
confined to this compatibility generation; it is not on any native execution
path.

Immutable generation manifests select the active segment and its optional
snapshot anchor ([`manifest-format-v1.md`](../storage/manifest-format-v1.md)).
Logical snapshots stream sorted authoritative state with independent CRC32C
and BLAKE3 validation ([`snapshot-format-v1.md`](../storage/snapshot-format-v1.md)).
Compaction commits snapshot plus next-segment selection through a new
immutable manifest ([`compaction-v1.md`](../storage/compaction-v1.md)).
Recovery, snapshot, and compaction run under finite `RecoveryLimits` and
`MaintenanceLimits` policies. Result proof v1 reexecutes the operation from a
pinned snapshot witness ([`result-proof-v1.md`](../provenance/result-proof-v1.md)).

### Format-2 layer rules

- `hyphae-core` owns stable domain values and invariants, not I/O.
- `hyphae-engine` is the format-2 embeddable compatibility facade.
- `hyphae-storage` owns the format-2 disk format, recovery, and indexes.
- `hyphae-query` owns the deterministic typed AST and reference semantics.
- `hyphae-retrieval` owns exact vector scoring and provider-neutral
  abstention; it has no default provider.
- `hyphae-contracts` exposes wire models tied to canonical contracts.
- `hyphae-server` and `hyphae-client` communicate only through versioned
  public models.
- `hyphae-cli` is the only executable artifact and composes libraries.

### Format-2 data directory

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

Format-2 state can be imported offline into a separate pending native
directory, verified for equivalence, and promoted explicitly; see
[ADR-0021](../adr/0021-native-cutover-and-format-evolution.md).
