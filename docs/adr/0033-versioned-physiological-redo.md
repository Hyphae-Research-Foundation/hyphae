# ADR-0033: Version physiological redo separately from WAL v1

- Status: Proposed
- Date: 2026-09-17
- Owners: Celiums Solutions LLC

## Context

Native WAL v1 (`HYWAL001`) has six released record roles. Adding a seventh
role to its public `RecordKind` would let a new writer produce bytes that an
older v1 reader rejects even though both claim the same format version.

The current `HYROOT03` manifest and `HYCHK001` checkpoint bind roots, page
generation, lineage, CSN, and WAL history. They do not bind the page-file
high-water mark or a digest of the exact physical checkpoint prefix. A caller
cannot safely fill in those missing identities: doing so would make the caller,
rather than the verified manifest/checkpoint/WAL chain, recovery authority.
Rehashing the complete database before every redo batch is also not an
acceptable recovery path.

A page-image sequence can span several fixed WAL blocks. Block integrity alone
does not prove that the final block, full page inventory, or transaction result
exists. Recovery must not accept the complete block prefix of an interrupted
multi-block sequence as a complete batch.

## Decision

This ADR is a proposal, not permission to emit redo. Until a format-transition
ADR is accepted and implemented end to end, production code must not allocate
a page-redo `RecordKind`, emit page redo, apply it during open, or advertise the
capability. WAL v1 keeps values above `CATALOG=6` unallocated and fails closed.

The transition must introduce both a data-directory feature boundary and a WAL
framing version distinct from `HYWAL001`. A format marker must select that
version before any writer emits it. Opening without the feature, opening with
an older binary, or seeing the records in WAL v1 must fail before mutation. The
exact future magic, kind allocation, and upgrade procedure remain unallocated
until the transition is accepted.

The future checkpoint authority must persist and authenticate a canonical base
descriptor containing at least:

- directory lineage;
- root-manifest generation and visible CSN;
- page-file generation and complete page high-water mark; and
- a domain-separated digest of every canonical page through that mark.

Both the verified root manifest and checkpoint record must bind the descriptor
digest. Physical WAL verification must bind the checkpoint record LSN and
block digest to the same lineage. Standard database open, not an application
caller, constructs an opaque verified checkpoint token after cross-validating
those authorities and scanning the checkpoint prefix once. The constant-size
token remains attached to that open page-store generation and is reused for
all redo batches. Reopen reconstructs it; no batch repeats the database-prefix
scan.

Each transaction's future terminal authority must bind the checkpoint token,
transaction identity, first page identity, total page count, ordered inventory
digest, final page high-water mark, resulting root inventory, blob generation,
page generation, and commit identity. The inventory digest includes each
zero-based position, page ID, and complete page-image digest. A batch is
eligible for staging only after semantic recovery sees exactly that many pages
in order followed by the one matching terminal authority. Missing, duplicated,
misordered, substituted, or post-terminal records fail closed. In particular,
a proper prefix ending at any complete WAL block is incomplete and cannot
stage pages.

Recovery ordering is:

1. validate the data-directory format and redo-feature authority without
   changing any WAL or page file;
2. verify and repair only an incomplete final WAL block;
3. verify the complete manifest/checkpoint/WAL lineage;
4. validate the physical checkpoint page count, every canonical checkpoint
   page, and the complete prefix digest, then construct the checkpoint token,
   without changing the page file;
5. repair only an incomplete page tail after the checkpoint prefix verifies;
6. verify the complete redo inventory and terminal authority without changing
   the page file;
7. verify any complete crash-left page overlap byte for byte;
8. append the missing unpublished page tail and synchronize it;
9. after uncertain append or synchronization, reopen and repeat steps 1 through
   5 before continuing; and
10. validate all page and blob dependencies before atomically publishing the
   terminal roots.

An append, synchronization, or repair-truncation error poisons that writer
attempt. It returns no publication authority and requires reopen. Complete
divergent pages are corruption and are never truncated. Blob reconstruction,
page-generation transitions, retention, replication, and root publication
must each have equivalent authority and crash evidence before redo can become
emittable.

Redo implementation types remain internal. The foundation must not add public
exhaustive capability enums or fixed public inventory arrays; diagnostics use
versioned data contracts only after the executable feature exists.

## Consequences

- Existing WAL-v1 readers and writers remain byte-compatible.
- The next executable slice requires coordinated marker, WAL, manifest,
  checkpoint, commit, open-recovery, page, blob, and migration work.
- Prefix verification costs one checkpoint-prefix scan per reopen, not per
  batch; bounded batch verification still costs its own page inventory.
- A terminal record consumes space but prevents a complete multi-block prefix
  from being mistaken for an authoritative transaction.
- No current product or native gate may claim physiological redo from this
  proposal or its test model.

## Alternatives considered

### Add `PageRedo=7` to WAL v1

Rejected because it changes the accepted v1 byte language without changing
the framing version.

### Accept caller-supplied checkpoint and page-prefix digests

Rejected because unverified caller data would become durable recovery
authority and could select the wrong database prefix.

### Rehash the complete prefix for every batch

Rejected because recovery cost becomes proportional to database size for each
bounded batch.

### Apply every complete physical WAL block immediately

Rejected because a multi-block crash can leave valid prefix blocks without the
declared inventory or terminal commit authority.

## Verification

`hyphae-native-wal` test
`valid_integrity_wal_v1_block_rejects_unallocated_kind_seven` recomputes the
record checksum, block checksum, and block digest after setting kind `7`, then
proves full WAL-v1 decode still rejects it.

The `hyphae-native-wal` integration test `page_redo_contract` is not linked
into the library and cannot emit WAL. It uses a private minimal page model and
preverified authority fixtures. It exercises only format admission, bounded
token caching, record inventory, writer poisoning, and page-tail mechanics:

- `checkpoint_token_is_fixture_bound_and_cached_across_batches`;
- `future_format_authority_is_validated_before_tail_repair`;
- `invalid_checkpoint_prefix_never_repairs_incomplete_tail`;
- `terminal_fixture_binds_complete_count_order_and_inventory`;
- `every_complete_frame_group_prefix_without_terminal_is_rejected_before_staging`;
- `injected_partial_append_is_repaired_on_reopen_before_complete_replay`;
- `injected_sync_failure_reopens_repairs_and_verifies_complete_overlap`; and
- `injected_reopen_truncation_failure_fails_closed_and_retry_repairs`.

The complete-record groups in that model are not encoded WAL blocks. These are
not tests of a future WAL checksum/digest chain, physical checkpoint or commit
authority, production redo, open-time application, blob reconstruction, or
root publication. Those remain required before this proposal can advance.
