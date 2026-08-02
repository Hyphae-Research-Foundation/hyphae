# ADR-0021: Native cutover and format evolution

- Status: Accepted
- Date: 2026-08-02
- Owners: Celiums Solutions LLC

## Context

Two engine generations coexist in this repository. The packaged product,
`hyphae-cli` `0.2.x`, serves disk format 2 with the append-only log and the
rebuildable Redb index. The native runtime, `hyphae-native-runtime`, owns
the target substrate (`pages.hydb`, `wal.hywal`, `roots/`, `blobs/`) but is
unpublished, is not part of the product binary, and has no cutover contract.

Physical compatibility policy currently exists only as family-by-family
implementation facts: old catalog roots upgrade on a later write; legacy
relational, structure, and search roots remain readable and writable without
implicit conversion; and `HYRIDX01` stays exact-only while `HYRIDX02` is the
order-preserving layout. Without a durable decision, every milestone
implicitly renegotiates the fate of format-2 data and the lifetime of the
experimental native directory.

## Decision

The native runtime remains unpublished and outside the default product path
until gate G6 defines the local product surface. No `--native` flag, no
dual-authority subcommand, and no runtime mode may let two truth authorities
operate over the same data. The legacy/native feature flag rejected during
convergence planning stays rejected.

The target native data directory owns a global format marker. It contains a
`FORMAT` file whose versioned format line is distinct from disk format 2, a
`LOCK` file held under an exclusive operating-system lock, and single-writer
ownership detection. Opening fails closed when the marker is unknown,
missing, or mixed. The current experimental layout, which materializes
`pages.hydb`, `wal.hywal`, `roots/`, and `blobs/` without a marker, is
implementation evidence per the
[native architecture](../architecture/native-local-ecosystem.md) and cannot
be promoted to a contract without this marker.

One policy governs physical format evolution. Every physical family declares
versioned magics; the existing set is `HYCAT001` through `HYCAT003`,
`HYRELBT1`, `HYRELBT2`, `HYSTRBT1`, `HYSTRBT2`, `HYSEABT1`, `HYRIDX01`,
`HYRIDX02`, `HYWAL001`, `HYPAGE01`, `HYBLOB01`, `HYROOT01`, `HYROOT02`, and
`HYWAR001`. The rules are:

1. legacy roots remain readable and writable without implicit conversion;
2. new physical objects use the newest layout;
3. one physical object never mixes layouts;
4. upgrades happen only on a later explicit write, never during open; and
5. decode fails closed on an unknown magic.

Layout-dependent capabilities such as range planning are admitted only from
persisted physical metadata, never from catalog intent. `HYRIDX02` ordered
planning is the reference pattern.

Migration from disk format 2 to the native directory is offline and
fail-safe:

1. the format-2 source is held read-only;
2. a separate native target directory is created;
3. the importer consumes a verified logical snapshot;
4. legacy objects map to stable native catalog identities, and the mapping
   must preserve documents, vector spaces, lexical definitions, receipts,
   proof anchors, and caller-visible idempotency identities (UUIDs);
5. counts, digests, and semantic equivalence are verified;
6. the target is promoted only after complete validation, through an
   explicit promotion marker; and
7. the source is retained for rollback until operational policy permits
   retirement.

The source directory is never rewritten in place. Migration runs as a
subcommand of the single `hyphae` executable, consistent with ADR-0006.

The public `/v1` contracts remain served by the format-2 engine until the
cutover completes. Which `0.2` edge APIs survive after the native ecosystem
is complete is an explicitly deferred decision, scoped to after G6; this ADR
does not take it.

## Consequences

- One contract governs the lifetime of both engine generations; milestones
  stop renegotiating the fate of format-2 data.
- Migration tests require immutable format-2 fixtures; the `compatibility/`
  v1 and v2 directories already provide them.
- The importer and the promotion step need enumerated crash boundaries and
  failure injection equal to the rest of the native substrate.
- The identity mapping adds specification work before any migration code is
  written.
- Two engines without a bridge keep costing maintenance until G6 and G8
  close.

## Alternatives considered

### Rewrite the format-2 directory in place

Rejected because a failure during rewrite makes rollback and independent
verification of the source impossible.

### Run a continuous dual-write shim

Rejected because two truth paths make recovery, proof, and cutover evidence
ambiguous.

### Toggle legacy and native authority with a feature flag

Rejected for the same dual-authority reason; a flag reintroduces two truth
paths over one data directory.

### Extend disk format 2 and Redb into the final substrate

Rejected by ADR-0020: the target requires Hyphae-owned pages, WAL, MVCC,
catalog, memory governance, and specialized indexes.

### Ship compatibility gateways before the ecosystem is complete

Rejected because it inverts the product priority; gateways may be evaluated
only after the native local ecosystem is complete.

## Verification

- The immutable `compatibility/` v1 and v2 fixtures are the importer's
  input corpus.
- Crash-injection tests cover every import, verify, promote, and rollback
  boundary.
- Verification compares counts and content byte for byte or digest for
  digest between source and target.
- Round-trip tests prove that receipts, proof anchors, and idempotency
  identities survive migration.
- The [phase-1 gate](../gates/native-local-phase-1.md) closes G8 only with
  v2-to-native migration evidence on one exact commit.
