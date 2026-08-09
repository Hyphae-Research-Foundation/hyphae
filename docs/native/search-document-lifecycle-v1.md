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
- posting `HYPOST01`.

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
