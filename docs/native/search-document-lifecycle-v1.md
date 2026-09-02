<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native lexical document lifecycle v1

Status: normative contract; compiler-reaching red gate, embedded/delta/local
implementation, deterministic recovery matrix, and direct Linux test gates
and performance receipts passed; hosted stack gates remain pending.

This contract adds first-class replacement and deletion to Hyphae's native
lexical document store. It does not add an OpenSearch facade, a sidecar, a
serialized engine-to-engine path, or a complete-state rewrite.

## Operations

`REPLACE DOCUMENT` requires one live document at the exact binary identity and
atomically replaces its stored source, analyzed length, term frequencies, and
posting membership. `DELETE DOCUMENT` requires one live document and atomically
retires its stored source and every derived lexical projection.

Both operations:

- require an existing search collection in the captured catalog;
- use the collection `ObjectId` plus exact document ID as their write-conflict
  identity;
- analyze source text exactly once per accepted replacement;
- preserve prior immutable roots and their BM25 results;
- publish through the existing search root, WAL transaction, and global CSN;
- reject expiry and oversized document or term identities before staging; and
- fail without adding a partial mutation or changing transaction-private state.

Missing replacement/deletion targets are errors. `INDEX DOCUMENT` remains the
creation operation. After a successful deletion, `INDEX DOCUMENT` may reuse the
same identity in the same transaction or a later transaction.

The transaction-private state machine admits these sequences:

| Sequence | Result |
|---|---|
| index, replace | one live replacement |
| index, delete | no live document |
| replace, replace | final replacement |
| replace, delete | no live document |
| delete, index | one live re-created document |
| delete, replace | missing-document error; prior delete remains staged |

## WAL authority

The additive search opcodes are:

| Opcode | Name | Key | Value |
|---:|---|---|---|
| `37` | `REPLACE DOCUMENT` | exact binary document ID | UTF-8 source text |
| `38` | `DELETE DOCUMENT` | exact binary document ID | empty |

Both require the search engine, a nonzero target collection, and no expiry.
Replacement text may be empty. The logical mutation is the replay authority;
postings and statistics are deterministic physical projections and never
produce independent WAL records. Large replacement text uses the same
`HYDOCS01` inline/blob envelope as insertion.

## Physical format

The first accepted lifecycle mutation upgrades the current search-root marker
from `HYSEABT1` to `HYSEABT2` in the same copy-on-write root publication.
Historical `HYSEABT1` roots remain readable. New live values retain their v1
encodings:

- document `HYDOCS01`;
- term metadata `HYTERM01`; and
- posting `HYPOST01` or `HYPOST02`.

`HYPOST02` is the self-describing posting: the same 16-byte layout as
`HYPOST01` (8-byte magic, u32 little-endian term frequency) with the
formerly reserved trailing 4 bytes carrying the owning document's
analyzed token count as u32 little-endian. New live postings are
written as `HYPOST02`; existing `HYPOST01` postings stay valid and
readable — a scorer reading `HYPOST01` resolves the document length
through the document header, and a scorer reading `HYPOST02` uses the
carried length without any side lookup. The carried length must equal
the header token count of the document written in the same publication;
replacing a document rewrites all of its postings, so a live posting can
never carry a stale length. A zero carried length is malformed.

`HYSEABT2` additionally admits these exact tombstone values:

| Namespace | Exact value |
|---|---|
| document `0x02` | ASCII `HYDOCT01` |
| term metadata `0x03` | ASCII `HYTERMT1` |
| posting `0x04` | ASCII `HYPOSTT1` |

No tombstone is valid under `HYSEABT1`. Tombstones carry no source text, blob
reference, count, or expiry. Any extra byte or use under another namespace
fails closed.

Replacement point-loads the current live document and computes the old and new
frequency maps. It then applies one sorted physical batch:

- overwrite the document with its new canonical source envelope;
- for a term present before and after, preserve document frequency and upsert
  the new term frequency;
- for an old-only term, tombstone its posting and decrement document frequency,
  tombstoning term metadata exactly when it reaches zero;
- for a new-only term, insert or revive its posting and increment or revive
  term metadata; and
- preserve collection document count while adjusting total analyzed terms by
  the exact old/new length delta.

Deletion applies one sorted physical batch:

- tombstone the document and every live posting derived from its source;
- decrement each affected document frequency and tombstone zero-frequency term
  metadata; and
- decrement collection document count and total analyzed terms exactly.

Insertion into `HYSEABT2` may revive canonical document, term, and posting
tombstones. It must not overwrite another live document.

## Query and validation

Current-root `MATCH`:

- treats document, term, and posting tombstones as absent;
- counts only live postings when comparing a posting range to document
  frequency;
- requires every live posting to reference one live document;
- reads the current document length from that live document; and
- preserves the existing BM25 score and bytewise document-ID tie-break.

Complete validation rebuilds the live collection statistics, terms, document
frequencies, term frequencies, and postings from live stored source text. The
rebuilt live projection must equal the physical live projection exactly.
Canonical v2 tombstones may remain as unreachable history; malformed
tombstones, live orphan postings, live zero frequencies, count divergence,
invalid UTF-8, missing/corrupt blobs, and any tombstone in v1 fail closed.

## Local transaction surface

The local search payload keeps version `1` and adds:

- opcode `3`, `TRANSACTION_REPLACE_DOCUMENT`, using the existing 40-byte
  transaction document header and UTF-8 text body; and
- opcode `4`, `TRANSACTION_DELETE_DOCUMENT`, using a 36-byte header followed by
  the exact binary document ID.

The delete header is:

| Offset | Width | Field |
|---:|---:|---|
| 0 | 1 | search payload version `1` |
| 1 | 1 | opcode `4` |
| 2 | 2 | reserved zero |
| 4 | 8 | matching local transaction handle |
| 12 | 16 | nonzero search collection `ObjectId` |
| 28 | 4 | document-ID byte length, little-endian `u32` |
| 32 | 4 | reserved zero |
| 36 | variable | exact binary document ID |

Successful replacement and deletion each return the existing search `STAGED`
receipt with `rows_affected=1`. A codec, handle, resource, missing-document, or
engine failure preserves the active batch and its next operation ordinal.

## Delta and conflict behavior

The point-resolved all-engine path hydrates at most the named collection
definition and named document. It records that identity in the private overlay
so subsequent operations resolve against transaction-private state without
re-reading or scanning the physical tree.

Concurrent insert, replace, or delete operations for the same collection and
document retain first-committer-wins. Disjoint document identities may rebase.
No lifecycle operation may call complete catalog, search-state, term, document,
or posting materialization.

### Product batch ingest

The product batch ingest (`ingest_search_batch`) is a point-resolved route
whose cost scales with the batch, not with the collection:

- The idempotency marker, the physical binding, the document manifest, and
  the posting-coverage flag resolve through durable point reads of the
  current structure root. The replay receipt and the fresh receipt bind the
  committed root identity (`visible_csn`, `catalog_version`, `root_digest`)
  without materializing engine state.
- A batch whose documents carry no named vectors stages every document
  source, doc-value posting, manifest, coverage flag, and idempotency marker
  through the physical delta batch and commits it as one all-engine
  transaction. The marker records the transaction identity the serialized
  writer will publish under; a commit that publishes under any other identity
  is a fail-closed corruption error, never a silent foreign receipt.
- A batch that carries at least one named vector keeps the materialized
  transaction until the ANN store gains a delta stage. Both paths write the
  same durable records: a reopened directory cannot tell which path ingested
  a batch.
- Duplicate document identities, the collection document bound, and
  idempotency conflicts are rejected before the first staged mutation on
  either path.

Root construction for either path treats a run of persistent scalar `SET`s
over distinct keys as one sorted copy-on-write batch, and probes the
immutable base tree through the verified buffer pool. Each key keeps the
sequential guards (no live collection under the key; a prior TTL entry
retires in the expiry index) and the logical tree contents are identical to
applying the mutations one at a time. The run splits on the first repeated
key, expiring value, or non-scalar opcode.

The manifest itself remains one bounded `HYPSMAN1` value of 16-byte identities
under the collection document bound; raising that bound requires re-measuring
the manifest rewrite alongside the ladder evidence.

## Verification gates

The slice requires:

- a compiler-reaching red test before the public embedded/delta/local APIs
  exist;
- model equivalence for index/replace/delete/reinsert sequences;
- exact BM25 result/score equivalence before replacement, after replacement,
  after deletion, across retained snapshots, and after reopen;
- `HYSEABT1` read compatibility plus atomic first-mutation upgrade to
  `HYSEABT2`;
- canonical tombstone/revival and malformed/orphan/count-corruption rejection;
- stable WAL golden bytes and rejection of target, value, expiry, engine, and
  truncation violations;
- local codec golden/truncation/boundary tests and active-session ordinal
  preservation after semantic failure;
- same-document conflict and disjoint-document optimistic rebase tests;
- all seven deterministic singleton commit interruption boundaries for
  replacement and deletion, never exposing a mixed projection;
- large-text blob replacement, deletion, reopen, vacuum, and blob-collection
  safety;
- a thread-local fail gate proving no complete state or catalog load;
- a process-counter gate proving vector-less batch ingest, its idempotent
  replay, and its receipts perform no complete state load, with equivalence of
  corpus, lexical, and doc-value results against the materialized path and
  after reopen;
- exact logical equivalence of a coalesced scalar `SET` run against sequential
  application, including expiry-index retirement, plus rejection of a run that
  reaches a live collection key or carries an expiring member without
  appending pages;
- hosted Linux, macOS, Windows, fuzz, dependency, packaging, and release gates;
  and
- direct-Linux stage, memory-commit, strict-commit, page/WAL, allocation, and
  scaling evidence reported separately.

## Boundary

This slice does not add positions, phrases, fuzzy queries, prefixes, filters,
facets, highlights, doc values, immutable segments, merge scheduling,
search-tombstone compaction, bulk APIs, partial document fields, analyzer
generations, replication, clustering, multitenancy, TLS, encryption at rest,
SaaS roles/billing, embeddings, or an LLM.
