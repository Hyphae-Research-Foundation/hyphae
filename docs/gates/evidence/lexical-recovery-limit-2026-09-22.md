<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Lexical recovery limit: synthetic source-bound reproduction

Status: open until the final 4.0.0 source, packages, and recovery matrix are
validated. This receipt uses only synthetic text; it does not identify the
sole cause of any external incident.

## Candidate before the repair

- Source: `6ed65d2b82388dd8eac26e3d1933c6c5f8d77168`.
- Platform: H100 Ubuntu lane, CPU execution for this test; Rust 1.90.0.
- Data: one vectorless product collection, lowercase analyzer, strict commit
  durability, documents of 2,957 UTF-8 bytes made from a fixed sentence and
  145 synthetic `kinase000` through `kinase144` terms repeated twice. No
  customer text, embedding service, or external database was used.
- Load: 128-document ingest batches, then progressively smaller batches to
  find the first failing document. Every accepted batch carried a strict
  commit receipt. The final accepted state had 612 live documents, CSN 13,
  commit LSN 2,635,433, 93,790 physical lexical entries, 7,610,521 encoded
  lexical key/value bytes, and a conservative retained-memory charge of
  67,010,852 bytes.
- First single-document failure: document 613 crossed the 67,108,864-byte
  recovery budget at entry 93,927, after 7,621,938 encoded bytes, with a
  retained charge of 67,108,993 bytes (129 above budget). These physical
  counts describe the traversal prefix at rejection, not every entry in the
  rejected candidate root. The product returned `corruption`, category
  `corruption`, despite an intact prior root. No commit receipt was returned;
  the snapshot identity was unchanged. Reopening recovered CSN 13 and
  `hyphae doctor` reported `snapshot_verified=true`, `verified_open=true`,
  `status=healthy`.

The retained-memory charge fired before the 131,072 physical-entry or 64 MiB
encoded-byte visitor limits. The published `release-v3.0.0-crates` tag lacks
this borrowed-visitor recovery path. The later `6655296a` checkout contains
it while still identifying its workspace as 3.0.0. Those source states must
not be reported as the same release.

## Repaired working-tree verification

The repair was exercised in an isolated H100 worktree based on `6ed65d2b`.
These measurements are **not** an exact final-source or packaged-release
receipt; the worktree must be committed and the decisive tests repeated on
that commit before closure.

- After a strict commit of synthetic document 613 into the previously
  rejected directory, recovery returned CSN 14 and the same root digest on
  two independent product opens. The process running the first open was
  killed, so the second open covered restart from a committed root.
- The automated scientific-corpus regression committed 640 documents of
  approximately 3 KB each in strict batches, reopened the directory, and
  returned 640 documents for a synthetic kinase term (16 page hits). A
  separate short-title control committed and reopened 1,024 documents and
  returned 1,024 documents for its term. Both passed with Rust 1.90.0.
- `hyphae doctor` on the 613-document source and the restored directory
  returned `snapshot_verified=true`, `verified_open=true`, `status=healthy`.
  A native backup at CSN 14 contained 14 files and 65,279,890 bytes, with
  checkpoint digest
  `41a51b737a0a879f29cd5c168961d6682286aa53db0f20ae2aec1a298309e371`.
  `backup verify` accepted it, and `restore` returned `status=restored` with
  a healthy doctor result. The separate doctor command on that restore also
  passed. Backup creation, verification, restore, and restored doctor took
  101.29, 0.06, 126.47, and 78.84 seconds respectively on the shared H100.
- A diagnostic scale run on the repaired working-tree producer reached the
  existing 250,000-document collection-count ceiling with a deterministic
  short-text corpus, one collection with no vector values, doc values, and group
  durability. The in-process query ladder completed 16 rounds each of BM25,
  filtered/faceted, phrase, and fuzzy search; each scenario returned ten
  page hits. It ended at CSN 983 with 983 unretired WAL commits. Its 1,113.5
  seconds of ingest on a shared H100 and cold first query are diagnostic
  timings, not a performance claim. This run did **not** reopen the directory;
  a separately maintained 250,000-document run is needed for that proof.
- The existing M05-focused runtime set passed 14 tests after updating the
  shared-budget assertion to the new typed limit. The first full
  runtime/product run exposed three more historical assertions expecting
  `InvalidAnnTree` for the same known budget exhaustion. After updating
  those expectations, the complete runtime library passed 756 tests.
- A fast runtime test lowers the document-state budget only for its own test
  thread, then attempts a second strict document commit. It checks the exact
  typed limit and observed excess, proves WAL length and the committed root
  did not change, reopens the directory, and confirms the first document is
  searchable while the rejected second document is absent. The production
  budget remains 67,108,864 bytes; the test-only injection exercises its
  admission path without requiring a 64 MiB input in every CI run.

The scientific and short-title runs establish the lexical resource path, not
an attribution for an external incident. The 250,000-document reopen rung,
final-source regression, and package/gate binding remain pending.

## Closure evidence still required

The repaired candidate must show that the synthetic scientific corpus
continues past the old threshold, can reopen and search after a strict commit
and a killed process, and survives `doctor` plus verified backup/restore.
A short-document control and a 250,000-document lexical rung or an explicit
source-bound capability revision are required. Source, package, and gate
receipts must bind to the final release commit before this status changes.
